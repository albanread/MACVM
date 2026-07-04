# Sprint S11 — Compiled sends, PICs, patching

Objective: compiled code calls compiled code. Send sites in nmethods are
patchable `bl` inline caches with the full mono → PIC → megamorphic lattice;
klass-guarded entry points make customization sound; adapter frames connect
both tiers in both directions; the shared runtime-stub table and the inline
allocation fast path land. Implements SPEC §8.2 (entry/verified_entry), §8.5
(compiled sends, PICs), §9 (stubs), `arm64.md` §4.2/§4.3 (IC patching,
icache), and relaxes S10's eligibility to "most methods".

## Prerequisites

- S10 complete: `Ir` (with dormant `CallSend/CallRuntime/Alloc/GuardKlass/
  StoreField`), regalloc with spill-all-at-safepoint policy, `Nmethod`/
  `CodeTable` (+`oops_do`), call stub, `enter_compiled`, `TierLink`s,
  `VmRegBlock`, listing goldens.
- S9: `bl_patchable`, `call_far`, `CodeCache::{patch_branch26, alloc, free}`,
  `JitWriteGuard`, veneer fallback.
- S3: interpreter send protocol + `send_generic(vm, rcvr, sel, args)` and
  lookup (§6.1) callable from Rust.

## Deliverables

- Entry/verified-entry prologue emission; nmethod `entry_off ≠
  verified_entry_off` from this sprint on.
- `src/codecache/stubs.rs` — full `RuntimeStubs` table (D5).
- `src/codecache/icpatch.rs` — compiled-IC state machine, miss protocol,
  PIC stub generation, per-selector megamorphic stubs, patch protocol.
- `src/compiler/*` — `CallSend` lowering + `IcSite` records; `Alloc` inline
  fast path; `StoreField` with card barrier; eligibility relaxation.
- Adapter path: `c2i` stubs + `rt_interpret_call` (C→I), NLR unwind through
  compiled frames.
- Temporary GC bridge for the pre-S12 window (D8).

## Design

### D1. Eligibility (relaxed) and the death of bailout-by-restart

S10's restart trick is unsound once effects precede a potential slow path.
From S11, slow paths are REAL calls, and eligibility becomes:

- Allowed additionally: `store_instvar_pop`, `store_global_pop`, arbitrary
  `send`/`send_w` in ANY IC state, `send_super` (D4.6), smi-inlined ops keep
  their fused fast path but `fail:` edges now target a block containing
  `CallSend` to the same IC site (generic send does the LargeInteger/Double
  fallback via the interpreter callee).
- Still excluded (→ interpret): `push_closure`, `push_ctx_temp`,
  `store_ctx_temp_pop`, `block_return_tos`, `nlr_tos`, `has_ctx == 1`,
  `is_block == 1`, argc > 5. (Blocks/contexts compile in S14 with inlining;
  a non-inlined closure method is rare and stays tier-0.)
- `Ir::Bailout` is DELETED from live codegen (kept for `BoolBr.not_bool` →
  now a `CallRuntime{stub: MustBeBoolean}`; simpler: not_bool block calls
  `stub_dnu`-style runtime `rt_must_be_boolean` which raises the Smalltalk
  send and never returns normally).

### D2. Entry points and the klass-guard prologue

Layout of every nmethod from S11 (offsets recorded in the header):

```text
entry:            ; UNVERIFIED — callers that cannot guarantee the receiver klass
    tst  x0, #3                   ; smi receiver?
    b.eq 1f
    ldur x17, [x0, #7]            ; klass word (offset 8, biased −1)
    b    2f
1:  ldr  x17, <pool: smi_klass oop>          ; RelocKind::Oop
2:  ldr  x16, <pool: key_klass oop>          ; RelocKind::KeyKlassOop
    cmp  x17, x16
    b.ne <stub_ic_miss trampoline slot>      ; wrong klass → runtime (D4.2)
verified_entry:   ; callers with the klass already guaranteed land here
    stp  x29, x30, [sp, #-16]!
    …S10 prologue…
```

Guard uses only x16/x17 (arg regs untouched). The two pool loads are
LDR-literal into the nmethod's own pool: GC-updatable, no adrp/add splitting
hazard (S9 P5). `b.ne` target: Branch19 range is ±1 MiB — the miss stub may
be farther in a 64 MiB cache, so emit `b.eq verified_entry; b stub_ic_miss`
(Branch26) instead when the assembler can't prove range; v1 rule: ALWAYS use
the `b.eq`+`b` form (two words, zero range anxiety).

Who uses which entry (pinned):

| caller | entry used |
|---|---|
| compiled mono send site (`bl`) | **entry** (guard runs in callee — the send site itself carries no klass check) |
| PIC stub case that matched klass K == key_klass | **verified_entry** |
| megamorphic stub (lookup returned this nmethod for the actual klass) | **verified_entry** |
| interpreter IC dispatch (guard already compared, S10) | **verified_entry** (via call stub) |

> **SPEC-QUESTION:** SPEC §8.5 says a monomorphic compiled send is
> "`bl target_verified_entry`", but nothing at such a site checks the
> receiver klass, so a receiver of a different klass would execute wrong
> customized code. §8.2's own definition ("entry: prologue loads receiver
> klass, compares…") implies mono sites must call `entry`. S11 implements
> **`bl entry`** for mono sites (Strongtalk's model — callee-side guard);
> `verified_entry` is used exactly where a caller-side check exists (PIC,
> mega, interpreter IC). SPEC §8.5 should be amended.

### D3. Compiled send sites

**Lowering `send <ic>`** (non-smi-inlined): pop argc+1 vregs →
`CallSend { dst, site, args }`. Emit:

1. Marshal: receiver → x0, args → x1..x5 (regalloc reserves x0–x5 around the
   site by treating CallSend as def/clobber of all allocatable regs — the
   spill-all policy already keeps everything live in slots; marshaling is
   plain `ldr` from slots into arg regs).
2. `let off = asm.bl_patchable(RelocKind::InlineCache);` — records the site.
3. Result: x0 → dst's slot.
4. `IcSite { off, selector: SymbolOop, argc: u8, state: Cell<IcState> }`
   appended to the nmethod's `ic_sites`; selector oop ALSO gets a pool entry
   (RelocKind::Oop) so GC keeps/updates it — `IcSite.selector` is a Rust-side
   oop field updated by `oops_do` alongside (add `ic_sites` selectors to the
   root visit).
5. `PcDesc { pc_off: off + 4, bci, oopmap }` — the return address is the
   safepoint (S12 consumes; emission was plumbed in S10 D7).

Initial target of every `bl` site at publish: **`stub_resolve`** (D5). No
per-site pre-resolution — first execution resolves, exactly like an empty
interpreter IC.

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IcState { Unresolved, Mono, Pic { stub: CodeHandle, n: u8 }, Mega { stub: CodeHandle } }
pub struct IcSite { pub off: u32, pub selector: SymbolOop, pub argc: u8, pub state: IcState }
```

### D4. Miss/resolve protocol (the compiled IC state machine)

**D4.1 `stub_resolve` / `stub_ic_miss`** — one shared stub, two doors, same
tail. Register protocol at stub entry: x0..x5 = receiver+args (untouched),
x30 = return address = **site_off + 4** (for `stub_resolve`, reached by `bl`)
or the ORIGINAL send's return address (for `stub_ic_miss`, reached by `b`
from a guard — x30 still holds the send site's return address because the
guard `b` doesn't touch x30). Both cases: x30 − 4 = the `bl` site to patch.

Stub body: anchor (`str x29,[x28,#LAST_CFP]; str x30,[x28,#LAST_CPC]`), build
a small AAPCS frame, spill x0..x5 to it at FIXED offsets (the **RootSpill
area** — pre-S12 bridge D8 and S12 both treat these six slots as GC roots via
the adapter-frame rule), then `call_far rt_resolve_send`:

```rust
extern "C" fn rt_resolve_send(vm: &mut VmState, ret_addr: u64, argv: *mut u64) -> u64
// returns the code address to TAIL-JUMP to (never returns an oop)
```

`rt_resolve_send` steps:
1. `caller_nm = code_table.find_by_pc(ret_addr)`; `site = caller_nm.site_at
   (ret_addr − 4 − code.base)` → selector, argc, state.
2. `k = klass_of(argv[0])`; full `lookup(k, selector)`; DNU → return
   `stubs.dnu` address (D5.4).
3. Target resolution: `code_table.lookup(k, sel)` → nmethod → its
   **entry** address (mono patch) / **verified_entry** (PIC entry). No
   nmethod → the target CompiledMethod gets a **c2i adapter** (D6.1):
   `adapters.get_or_make(method)` → adapter address (adapters count as
   "verified" — the interpreter re-checks nothing but semantics don't need
   the guard).
4. State transition + patch (all under one `JitWriteGuard`):

| current | action | new state |
|---|---|---|
| Unresolved | `patch_branch26(site, mono_target_entry)` | Mono (record target klass in site) |
| Mono, same klass (guard raced a redefinition) | repatch to fresh target | Mono |
| Mono, different klass | build PIC stub with {old pair, new pair}; patch site → PIC | Pic{n:2} |
| Pic, n < 4 | REBUILD PIC stub with n+1 pairs (PICs are immutable once published — no in-place growth); free old stub; patch site | Pic{n+1} |
| Pic, n == 4 | free PIC; patch site → per-selector mega stub | Mega |
| Mega | unreachable (mega never calls resolve) | — |

5. Return the address to jump to for THIS dispatch (the freshly resolved
   target — do not re-execute the patched `bl`; tail-jump keeps the original
   x30 so the callee returns straight to the send's continuation).

Stub tail: reload x0..x5 from RootSpill, pop frame, clear the anchor
(`str xzr,[x28,#LAST_CFP]`), `br x16` (address returned in x0 moved to x16
before reloading arg regs — pinned: rt_resolve_send's result goes to x16).

**D4.2 Branch26 range + veneer.** `patch_branch26` (S9) handles range: all
intra-cache targets fit ±128 MB by construction (64 MiB region); the veneer
fallback exists for defense and is unit-tested by forcing a fake far target
(S9 test 5). c2i adapters and PIC stubs live in the cache → always in range.

**D4.3 PIC stub layout** (allocated via `cache.alloc`, generated with the
normal `Assembler`, published like any blob):

```text
pic_stub(selector, [(k1,t1)…(kn,tn)]):      ; tX = verified_entry or c2i addr
    tst  x0, #3                    ; receiver klass into x17 (same 5-word
    b.eq 1f                        ;  sequence as the nmethod entry guard)
    ldur x17, [x0, #7]
    b    2f
1:  ldr  x17, <pool: smi_klass>
2:  ldr  x16, <pool: k1>           ; RelocKind::Oop — GC updates PIC pools too
    cmp  x17, x16
    b.ne 3f
    b    t1                        ; Branch26 tail-call, x30 untouched
3:  ldr  x16, <pool: k2>
    …
N:  b    <stub_resolve_from_pic>   ; all miss → resolve (which rebuilds/promotes)
```

Per entry: 4 words + 1 pool word. n ≤ 4 (SPEC §4.3 tunable shared with
interpreter PICs). PIC targets that are nmethod entries use
**verified_entry** (the pair's klass was just compared). PIC stubs are
registered in a Rust-side `PicTable: Vec<PicDesc { handle, site: (NmethodId,
u32), relocs }>` so (a) GC's `oops_do` visits PIC pool words, (b) S12's flush
protocol can find PICs referencing dead nmethods.

**D4.4 Megamorphic stub — one per selector**, cached in
`HashMap<u64 /*sym bits*/, CodeHandle>` (rehashed after full GC like
CodeTable):

```text
mega_<sel>:
    ldr  x16, <pool: selector oop>
    b    stub_mega_shared
stub_mega_shared:                  ; x0..x5 args, x16 selector, x30 ret
    <anchor + frame + RootSpill as D4.1>
    call_far rt_mega_lookup        ; probes lookup cache (SPEC §6.1), full
                                   ; lookup on miss, NEVER patches the site
    <reload, unfrear, br x16>
```

**D4.5 Interpreter-IC coupling.** Interpreter ICs keep their own lattice
(SPEC §5.3) — unchanged. The nmethod-id-validation dispatch from S10 remains
the only interpreter↔CodeTable contact; when S12 frees an nmethod, compiled
sites are repatched (S12 D6) and interpreter ICs self-heal (S10 D4).

**D4.6 `send_super` from compiled code:** the compile-time static superclass
lookup (holder.superclass is fixed at compile time — method holders don't
move between klasses in v1): resolve the target METHOD at compile time,
emit a direct `bl` to its nmethod entry or c2i adapter with
`RelocKind::InlineCache` and `IcSite { state: Mono }` (so invalidation can
repatch it). Record a dependency note in `Nmethod` (`deps` seed for S13).

> **SPEC-QUESTION:** this section's own "bl to its nmethod entry" reads as
> ordinary `entry`, but implementation plus a failing integration test
> (`send_super_resolves_at_compile_time_and_dispatches`, a 3-klass override
> chain) proved that's wrong, the same way D2's own SPEC-QUESTION above
> caught it for mono sites: `entry`'s klass-guard checks the ACTUAL
> receiver (whatever subclass `self` really is — e.g. `Leaf`) against the
> TARGET's own `key_klass` (the super lookup's static holder — e.g.
> `Mid`). Those essentially never match by construction (that mismatch is
> the entire point of `super`), so the guard always misses and silently
> re-resolves from the receiver's REAL klass via `stub_resolve` — quietly
> collapsing back into an ordinary send and losing `super` semantics
> entirely, with no crash to flag it. S11 implements **`verified_entry`**
> for `send_super` targets instead: the send's own static compile-time
> resolution already IS the verification a guard would otherwise redo
> (same reasoning as PIC/mega targets in D4.3/D4.4). S13's `Nmethod.deps`
> work should assume `verified_entry`, not `entry`, for every super-send
> dependency it seeds from this step.

### D5. Runtime stubs table (generated at startup, in cache, in this order)

```rust
pub struct RuntimeStubs {
    pub call_stub: *const u8,        // S10 (I→C)
    pub poll: *const u8,             // S10
    pub resolve: *const u8,          // D4.1 (bl-initial + guard-miss door)
    pub mega_shared: *const u8,      // D4.4
    pub alloc_slow: *const u8,       // D7
    pub dnu: *const u8,              // D5.4
    pub must_be_boolean: *const u8,
    pub nlr_unwind: *const u8,       // D6.3
    pub c2i_shared: *const u8,       // D6.1
}
```

Every stub that can reach Rust follows the same skeleton: anchor → AAPCS
frame → RootSpill(x0..x5) → `call_far` → restore → clear anchor → tail.
Rust-side entry points (all `extern "C"`, first arg `*mut VmState` loaded
from x28 by the stub):

```rust
extern "C" fn rt_resolve_send(vm, ret_addr: u64, argv: *mut u64) -> u64;
extern "C" fn rt_mega_lookup(vm, selector_bits: u64, argv: *mut u64) -> u64;
extern "C" fn rt_alloc_slow(vm, klass_bits: u64, size_bytes: u64) -> u64;   // returns oop
extern "C" fn rt_dnu(vm, argv: *mut u64, selector_bits: u64, argc: u64) -> u64; // builds Message, send_generic
extern "C" fn rt_must_be_boolean(vm, val: u64) -> u64;
extern "C" fn rt_interpret_call(vm, method_bits: u64, argv: *const u64, argc: u64) -> u64;
extern "C" fn rt_poll(vm) ;
```

**D5.4 DNU from compiled code:** lookup failure in `rt_resolve_send` /
`rt_mega_lookup` returns `stubs.dnu`; the dnu stub re-spills args and calls
`rt_dnu`, which builds the `Message` (allocates — D8 bridge applies) and
performs `doesNotUnderstand:` via `send_generic`, returning its result oop —
the dnu stub RETURNS it in x0 (this stub returns like a callee rather than
tail-jumping; x30 at stub entry is the send continuation, so plain `ret`
semantics hold).

### D6. Adapter frames — C↔I in both directions

**D6.1 C→I (compiled caller, interpreted callee).** Per-target adapter
(8 words + pool) + shared tail:

```text
c2i_<method>:
    ldr  x17, <pool: CompiledMethod oop>     ; RelocKind::Oop
    b    c2i_shared
c2i_shared:
    stp  x29, x30, [sp, #-16]!               ; REAL frame → FP chain uniform
    mov  x29, sp
    sub  sp, sp, #64                          ; RootSpill: 6 args + method + pad
    stp  x0, x1, [x29, #-16] … str x17,[x29,#-56]
    str  x29, [x28, #LAST_CFP]                ; anchor
    str  x30, [x28, #LAST_CPC]
    <call_far rt_interpret_call(vm, method, argv=&[x29-16], argc)>
    ; x0 = result (or NLR sentinel, D6.3)
    mov  sp, x29 ; ldp x29, x30, [sp], #16 ; ret
```

Adapter frame shape (pinned; the stack walker and pre-S12 bridge depend on
it): return-pc ∈ `c2i_shared` range identifies it; `[x29−8·i]` slots 2..8
hold rcvr/args/method oops → `walk_frames` reports it as
`FrameView::Adapter` and S12's root scan enumerates exactly those 7 slots
(fixed map, no PcDesc needed).

`rt_interpret_call` pushes `TierLink::IntoInterpreter { compiled_fp:
vm.last_compiled_fp, compiled_ret_pc }`, copies argv onto the process stack,
runs `activate_method` + the interpreter loop until that activation returns,
pops the TierLink, returns the result. Re-entrant interpretation — the
interpreter loop must already be re-entrant from S3 primitives calling
`send_generic`; assert depth is tracked.

Adapters are cached per CompiledMethod (`HashMap<u64, CodeHandle>`, rehash on
full GC; adapter pool word updated by `oops_do` via an `AdapterTable`
mirroring `PicTable`). If the method is later compiled, `rt_resolve_send`
naturally repatches sites to the nmethod on their next miss — sites bound to
an adapter are ALSO enumerated and eagerly repatched at nmethod install
(`CodeTable::install` sweeps `ic_sites` whose target == that adapter;
cheap: adapters record their dependent sites).

**D6.2 I→C** — S10's call stub + interpreter IC dispatch, unchanged.

**D6.3 NLR through compiled frames.** A block (interpreted — blocks never
compile in S11) can NLR to a home whose frame is separated from it by
compiled frames + adapters:

- Interpreter unwinding (S4) walks the process stack. When the unwind must
  pop an interpreter activation that was entered VIA `rt_interpret_call`
  (its frame is the base of a `TierLink::IntoInterpreter`), control must
  also discard the native frames above the matching
  `TierLink::IntoCompiled`'s call stub — those compiled frames cannot hold
  `ensure:` markers (no blocks compile) so discarding is semantically safe.
- Mechanism: `rt_interpret_call` detects the NLR crossing its activation
  boundary and returns the **NLR sentinel** (`0b0110` — second reserved-tag
  value; NLR target+value are parked in `vm.nlr_state: Option<NlrState>`).
  The c2i adapter returns it to the compiled caller — but compiled code must
  not treat it as an oop: EVERY compiled send site is followed by a 2-word
  check `cmp x0, #6; b.eq <stub_nlr_unwind>` *(only emitted when the method
  could be on an NLR path — v1: always; 2 words per site, revisit in S14)*.
- `stub_nlr_unwind`: walks `vm.tier_links` top-down to the innermost
  `IntoCompiled`, restores `sp/fp` from its `entry_sp` snapshot (extend
  TierLink::IntoCompiled with `entry_fp`), and returns the NLR sentinel from
  the call stub itself; `enter_compiled` sees the sentinel and resumes the
  interpreter's unwind (which continues below the interpreter frame that did
  the I→C call). `ensure:` frames BETWEEN interpreter activations are run by
  the interpreter's own unwinder on each side; compiled frames contribute
  none.

### D7. Allocation fast path in compiled code

Emitted when a send site is monomorphic to a metaclass whose cached target
has `primitive == PRIM_BASIC_NEW`, receiver is a `push_global` class constant,
and the klass `format == Slots` with fixed `non_indexable_size` (read at
compile time; guarded by S13 deps — until then a klass-format redefinition
flushes via S12's full sweep; record the assumption in `Nmethod.deps` now):

```text
    ldr  x16, [x28, #EDEN_TOP]
    add  x17, x16, #size_bytes            ; size = non_indexable_size·8
    ldr  xD,  [x28, #EDEN_END]            ; xD = dst reg as scratch
    cmp  x17, xD
    b.hi Lslow
    str  x17, [x28, #EDEN_TOP]
    ldr  x17, <pool: mark_init constant>  ; pristine mark word (tag 11|sentinel)
    str  x17, [x16]
    ldr  x17, <pool: klass oop>           ; RelocKind::Oop
    str  x17, [x16, #8]
    ; nil-init body: <pool: nil> stored to slots 2..n (unrolled, n is small)
    add  xD, x16, #1                      ; mem-tag the result
    b    Ldone
Lslow:
    <marshal klass+size; bl stub_alloc_slow; result → xD>
Ldone:
```

`Alloc`'s slow edge is a safepoint (CallRuntime); the fast path is not (no
GC possible on it). Nil-initialization is MANDATORY before the object is
reachable (GC at the next safepoint scans a garbage body otherwise).

### D8. Temporary GC bridge (pre-S12 window — DELETED by S12)

S11 compiled frames hold oops in spill slots that no collector can yet find.
Until S12:

```rust
// memory/allocator.rs — the one choke point:
if vm.compiled_depth > 0 && would_need_gc {
    // moving GC is FORBIDDEN under compiled frames until S12:
    allocate_directly_in_old_gen(…)   // non-moving; card/offset tables grow
} else { scavenge_then_retry(…) }
// full GC requests with compiled_depth > 0 → defer: grow old gen, set
// vm.gc_pending; the interpreter loop runs the full GC at the next point
// where compiled_depth == 0.
```

Plus: `oops_do` (S10 D8) covers nmethod/PIC/adapter pools at every collection
(those run only at compiled_depth == 0 now). Log a `MACVM_TRACE=gc` warning
whenever the bridge diverts an allocation, and expose
`vm.stats.bridge_old_allocs` so tests can see it. This rule is ugly,
memory-hungry under `threshold=1`, and exists precisely so S11 can gate
without S12's machinery; S12's first commit deletes it.

## Implementation order

1. Stub skeleton infra (anchor/RootSpill emitters as reusable `Assembler`
   helper fns) + `RuntimeStubs` startup generation; unit-test `poll` +
   `resolve` reachability with a fake site.
2. Entry/verified-entry prologue + `IcSite` emission; nmethods now publish
   with `Unresolved` sites aimed at `stub_resolve`.
3. `rt_resolve_send` mono path + patching; C→C mono calls work (test: two
   compiled methods).
4. c2i adapters + `rt_interpret_call` + TierLink; C→I works.
5. PIC generation + rebuild-on-grow + mega stubs; full lattice.
6. DNU, must_be_boolean, `send_super`.
7. `StoreField` + card barrier sequence; eligibility relaxation; smi fail
   edges → CallSend.
8. Inline `Alloc` + `rt_alloc_slow`.
9. NLR sentinel + `stub_nlr_unwind` + send-site check.
10. GC bridge (D8) + trace/stat hooks. Gates.

## Pitfalls

- **P1 — patch = ONE aligned u32 store + icache flush** (`arm64.md` §4.3).
  Mono→PIC→mega transitions each rewrite exactly the `bl` word; PIC growth
  builds a NEW stub and repatches the site rather than editing a live stub
  (a multi-word in-place edit is not atomic w.r.t. the thread's own
  execution if the site is on the current call chain — rebuild-and-swing is
  the only sane protocol even single-threaded).
- **P2 — free the OLD PIC stub only after the site is repatched**, and never
  while an activation could be executing inside it. Single-threaded v1: the
  patching happens inside `rt_resolve_send`, i.e. the thread is IN the
  runtime, not in the old stub — but the old stub's address may be x30-1
  frames up? No: PIC stubs tail-jump (`b`), they never own a frame → no
  return address into a PIC can exist. Assert: PICs contain no `bl`.
- **P3 — the guard's two-branch form.** `b.ne <far>` has ±1 MiB range;
  always emit `b.eq verified; b miss` (D2). A Branch19 range panic in the
  assembler would only surface once the cache grows past 1 MiB — build it
  right the first time.
- **P4 — x30 discipline in stubs.** Tail-jumps (`br`/`b`) preserve x30 = the
  send continuation; any stub that creates a frame must restore x30 before
  its tail `br x16`. Off-by-one here returns into the middle of a stub —
  crashes far from the cause. The stub skeleton helper owns this; hand-rolled
  stub epilogues are forbidden.
- **P5 — `rt_resolve_send` may compile? NO.** v1: resolution never triggers
  compilation (only counters do). Otherwise resolve → compile → cache alloc
  → potential flush interacts with the site being patched. Keep resolution
  pure: lookup + patch + jump.
- **P6 — interpreter ICs holding nmethod ids** are NOT touched by compiled-IC
  transitions; their validity check (S10 D4) is the invalidation mechanism.
  When S12 frees nmethods, compiled sites get repatched by sweep; do not
  build a second mechanism here.
- **P7 — the write barrier is on the SLOT address**, not the object header:
  card index = `(obj_addr + off − 1 − old_start) >> 9`. Barrier fires only
  when `obj ≥ old_start && val is heap oop && val < old_start` (SPEC §7.4
  value-conditional form). Sequence (5–7 insns) goes behind the store;
  get the biased card base from `VmRegBlock.card_base_biased`.
- **P8 — RootSpill offsets are ABI.** The D6.1 adapter layout and the D4.1
  stub layout are load-bearing for S12's root enumeration; change them only
  with the matching `FrameView` decoder change + golden update. Pin them as
  consts in `layout.rs`.
- **P9 — anchor hygiene.** Every stub that calls Rust sets
  `last_compiled_fp/pc` and CLEARS the fp to 0 on exit; a stale anchor makes
  the S12 walker walk freed frames. Debug assert in `enter_compiled`:
  anchor is clear on entry.
- **P10 — the NLR-sentinel check costs 2 words per send site.** Do not
  "optimize" it away for leaf callees — ANY callee can reach an interpreted
  block that NLRs (through further sends). S14's inliner can elide it with
  actual analysis.
- **P11 — mega stubs and adapters are keyed by raw oop bits** — rehash both
  maps after full GC (same bug class as S10 P7).
- **P12 — inline-alloc must nil-init before the next safepoint** (D7) and
  the mark-word init constant must have hash=0, age=0, sentinel=1 — read it
  from `layout.rs`, don't inline a magic number.

## Interfaces for later sprints

- S12: `PicTable`/`AdapterTable`/mega-map enumeration (flush protocol +
  `oops_do` extension); `IcSite.state` + repatch-to-resolve
  (`icpatch::reset_site`); adapter `FrameView::Adapter` fixed map; RootSpill
  consts; D8 bridge DELETION point (`memory/allocator.rs` marked `// S12:
  remove`); safepoint PcDescs now populated at every call site.
- S13: `Nmethod.deps` seeds (super-send target, alloc klass-format
  assumption); `stub_resolve`'s repatch path is what not_entrant patching
  redirects.
- S14: send-site fusion budget notes (NLR check elision, marshaling
  coalescing).

## Out of scope

- GC scanning compiled frames / oop-map consumption / nmethod flushing —
  S12 (D8 bridges the window).
- Deopt, not_entrant, dependency-driven invalidation — S13 (redefining a
  method under a compiled caller in S11 self-heals only via IC misses; a
  STALE MONO SITE whose klass didn't change keeps calling the old target —
  document as a known S11 semantic hole, closed by S13's dependency index;
  the in-language suite must not redefine methods mid-run before S13).
- Blocks/contexts in compiled code, inlining, customization beyond the key,
  OSR — S14/S15. Indexable allocation fast path (`basicNew:`) — S14.
