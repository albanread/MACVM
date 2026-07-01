# Sprint S13 — Deoptimization

Build the safety net that makes all later speculation legal: compiler-emitted
scope descriptors and PcDescs, the `brk`-based uncommon-trap mechanism with
its macOS SIGTRAP handler, materialization of interpreter frames from
compiled frames, `not_entrant` invalidation with lazy return-address
redirection, the dependency index, and `MACVM_DEOPT_STRESS`. Implements SPEC
§8.6 and §8.7 (deopt direction only; OSR is S15) plus SPEC §6.2's
invalidation hooks. Evidence: `reference-vm-analysis.md` §2.5, §3.4;
machine glue: `arm64.md` §6–§7.

## Prerequisites

From prior sprints (all assumed green under their own gates):

- **S2/S5** — interpreter frame layout (SPEC §5.1) on `ProcessStack`
  (`src/interpreter/`), the `Frame` view type, and the debug frame verifier
  (`Frame::verify`: slot tags, `saved_fp`/`saved_bci` smi encoding,
  operand-stack bounds).
- **S4** — Contexts (`ContextOop`), `has_ctx` method entry protocol.
- **S10/S11** — `Nmethod`, `NmethodId`, `CodeTable` (`src/codecache/`);
  compiled frames on the **native** stack (FP-chained, `arm64.md` §6); tier
  adapters (interpreter enters compiled code as a Rust→JIT call returning the
  result oop in `x0`; compiled code calls interpreted methods via a runtime
  stub that runs the interpreter); runtime stubs segment; patching +
  `sys_icache_invalidate` protocol.
- **S12** — safepoint sites (calls, allocation slow paths, loop polls) with
  oop maps; compiled-frame stack walking (FP chain + per-nmethod PC range
  lookup); the v1 rule that **all live oops are spilled to canonical frame
  slots at every safepoint** (no live-register maps). S13 leans on this hard.
- Compiled-frame discipline from S10 (restated because S13 depends on it):
  v1 compiled methods save/restore only `FP`/`LR` and never touch
  callee-saved `x19`–`x28` (VM-role registers, `arm64.md` §3), so a compiled
  frame is discarded with `mov sp, fp; ldp fp, lr, [sp], #16; ret`.

## Deliverables

- `src/compiler/scopes.rs` — `ValueLoc`, `CtxLoc`, `ScopeDescRecorder`,
  LEB128 pack/unpack, `ScopeDesc`/`PcDesc` decode side.
- Emit stage extended: scope + safepoint recording at every safepoint and
  trap site; two organic trap clients (smi-overflow, `mustBeBoolean`).
- `src/codecache/deopt_trap.rs` *(unsafe OK)* — SIGTRAP handler, ucontext PC
  rewrite, generated `deopt_uncommon_trampoline` / `deopt_return_trampoline`.
- `src/runtime/deopt.rs` *(safe)* — `FrameView`, `deoptimize_frame`,
  materialization, nested-interpreter resume, pending-deopt table.
- `src/runtime/deps.rs` — `DependencyIndex` multimap + invalidation walk;
  hooks in method installation (SPEC §6.2).
- `Nmethod` state machine `alive → not_entrant → zombie → freed`; entry
  patching; zombie sweep at full GC (SPEC §8.6).
- Loop-poll deopt check (poll sites test a `pending_deopt` flag).
- `MACVM_DEOPT_STRESS=1`; `MACVM_TRACE=deopt` (see SPEC-QUESTION at end);
  per-trap-site counters + `UncommonTrapLimit` bookkeeping (consumed by S14).

## Design

### Data structures

#### ValueLoc — where an interpreter-visible value lives in a compiled frame

```rust
// src/compiler/scopes.rs
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ValueLoc {
    /// Oop in the nmethod's literal/oop pool (GC keeps pool words current).
    ConstPool(u32),
    /// A small integer constant, stored untagged; re-tag on materialization.
    ConstSmi(i64),
    /// Frame slot at byte offset `off` from the compiled frame's FP
    /// (negative = below FP, exactly the S12 spill-slot convention).
    FrameSlot(i32),
    /// Shorthand for the nil_obj well-known oop (very common).
    Nil,
}
```

**There is no `Register` variant in v1.** S12 pinned the rule that every
value named by deopt metadata is in its canonical frame slot at every
safepoint (emit flushes dirty vregs to spill homes before recording). A
`Register` variant — and Self's live-register `mask()` machinery, analysis
§2.1 — is deliberately deferred to a later regalloc upgrade. Say so in code
comments too.

#### CtxLoc — the heap Context of a scope

```rust
#[derive(Clone, Debug)]
pub enum CtxLoc {
    /// has_ctx = 0.
    None,
    /// A real Context exists: compiled code allocated it at entry and writes
    /// captured temps through it. Deopt reuses it as-is.
    Materialized(ValueLoc),
    /// S14 Context elision: no Context exists; each captured temp's value
    /// has its own location. Deopt allocates a fresh Context and populates
    /// it from these. Format designed NOW; S13 never emits it.
    Elided { temps: Vec<ValueLoc> },
}
```

Context temps' values at materialization (M6): `Materialized` → the Context
already holds them; `Elided` → from the recorded per-temp `ValueLoc`s, into a
freshly allocated Context.

#### ScopeDesc — one virtual frame

Recorder-side (unpacked) form:

```rust
pub type ScopeId = u32;

#[derive(Clone, Debug)]
pub struct ScopeDescData {
    pub method_pool_ix: u32,        // MethodOop, via nmethod oop pool
    pub is_block: bool,             // CompiledBlock scope (S14)
    /// Inlined-scope link. S13 always None (depth-1 chains); the packed
    /// format supports arbitrary depth from day one.
    pub sender: Option<SenderLink>,
    pub receiver: ValueLoc,
    /// Unified arg/temp slots 0..argc+ntemps (SPEC §4.1 push_temp numbering).
    pub slots: Vec<ValueLoc>,
    pub ctx: CtxLoc,
}

#[derive(Clone, Debug)]
pub struct SenderLink {
    pub sender: ScopeId,
    pub sender_bci: u16,            // bci of the inlined send in the sender
    /// Sender's operand stack BELOW the inlined send's receiver+args, frozen
    /// for the whole inlined extent. Receiver+args are NOT here — they are
    /// reconstructed from this scope's receiver/slots (frame overlap, §5.1).
    pub pending_stack: Vec<ValueLoc>,
}
```

Per-scope locations are **pc-independent**: each interpreter entity has one
canonical frame slot for the whole compiled method. Only the operand stack
varies per safepoint, so it lives in the safepoint record, not the scope.

#### Safepoint record + PcDesc

```rust
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SafepointKind { Call, LoopPoll, Alloc, UncommonTrap }

#[derive(Clone, Debug)]
pub struct SafepointState {
    pub scope: ScopeId,             // innermost scope at this pc
    pub bci: u16,                   // bci in that scope
    pub kind: SafepointKind,
    /// true  → resume by RE-EXECUTING the bytecode at `bci`; recorded stack
    ///          = state BEFORE that bytecode, all its inputs on it.
    /// false → call-return site; resume at the bytecode AFTER the send at
    ///          `bci`; recorded stack EXCLUDES the send's popped
    ///          receiver+args; the materializer pushes the incoming result.
    pub reexecute: bool,
    /// Innermost scope's operand stack at this pc (outer scopes' stacks come
    /// from their SenderLink.pending_stack chains).
    pub stack: Vec<ValueLoc>,
}

/// Fixed-width, sorted by code_off, binary-searchable. Stored in nmethod
/// at header.pcdesc_off as a plain array.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct PcDesc {
    pub code_off: u32,   // KEY. Calls: the RETURN address offset (pc after bl).
                         // Traps/polls: the brk / poll instruction offset itself.
    pub site_off: u32,   // offset of the packed SafepointState in the scopes blob
}
```

`PcDesc::find(nm: &Nmethod, code_off: u32) -> Option<&PcDesc>` — binary
search, **exact match required**. A miss is a VM bug (every deopt-relevant pc
was recorded at compile time): `panic!` with nmethod id and offset.

#### Packed (LEB128) byte format — the scopes blob

One blob per nmethod at `header.scopes_off`. Integers are standard
7-bits-per-byte high-bit-continuation LEB128: **ULEB128** for unsigned
(indices, counts, bcis), **SLEB128** for signed (frame-slot offsets, smi
constants); `read_uleb/write_uleb/read_sleb/write_sleb` in `scopes.rs`,
round-trip unit-tested (SPEC §12.1 names this test explicitly).

```
value_loc :=
    tag: u8      0 = ConstPool   payload: uleb pool_ix
                 1 = ConstSmi    payload: sleb value (untagged)
                 2 = FrameSlot   payload: sleb byte offset from FP
                 3 = Nil         (no payload)

ctx_loc :=      (2 bits of the scope flags byte select the arm)
    None       → nothing
    Materialized → value_loc
    Elided     → uleb count, count × value_loc

scope_record :=                       (referenced by byte offset in the blob)
    flags: u8         bit0 has_sender, bit1 is_block, bits3:2 ctx_kind
                      (0 None, 1 Materialized, 2 Elided), bits 7:4 reserved=0
    method_pool_ix: uleb
    if has_sender:
        sender_scope_off: uleb        (byte offset of sender's scope_record)
        sender_bci:       uleb
        n_pending:        uleb,  n_pending × value_loc
    receiver: value_loc
    n_slots: uleb,  n_slots × value_loc      (unified slots, argc first)
    ctx per ctx_kind

safepoint_record :=                   (referenced by PcDesc.site_off)
    scope_off: uleb                   (byte offset of innermost scope_record)
    bci: uleb
    flags: u8      bit0 reexecute, bits3:1 SafepointKind, rest 0
    n_stack: uleb,  n_stack × value_loc
```

Scope records are deduplicated: one record per compiler scope, shared by all
its safepoints. Heap oops **never** appear raw in the blob (the GC does not
scan it) — only pool indices: the nmethod oop pool (S11 literal pool,
`arm64.md` §4.1) is the single GC-visible home for every oop the metadata
references, including each scope's `MethodOop`. Every method a scope names
MUST be added to the pool at recording time.

#### ScopeDescRecorder — compiler-side emission API

```rust
pub struct ScopeDescRecorder { /* scopes: Vec<ScopeDescData>, sites: Vec<(u32, SafepointState)> */ }

impl ScopeDescRecorder {
    pub fn new() -> Self;
    /// S13: exactly one call per compilation, sender=None.
    /// S14: once per inlined scope, with a SenderLink.
    pub fn begin_scope(&mut self, data: ScopeDescData) -> ScopeId;
    /// Emit stage, at each safepoint/trap (PcDesc.code_off conventions above).
    pub fn record_site(&mut self, code_off: u32, state: SafepointState);
    /// Dedup + pack; returns (scopes blob, sorted PcDesc array).
    pub fn pack(self) -> (Vec<u8>, Vec<PcDesc>);
}
```

Where in the pipeline (SPEC §8.3): the **emit** stage owns the recorder. The
decode stage already models the interpreter operand stack per bci (that is
how stack→SSA-lite conversion works); each IR node carries `(scope, bci)` and
its abstract-stack shape. At a safepoint node, emit (a) flushes dirty vregs
to spill homes (S12 rule), (b) maps each abstract stack element and
interpreter entity through the regalloc's `vreg → spill slot / constant`
assignment to a `ValueLoc`, (c) calls `record_site`. Trap sites record
`kind = UncommonTrap, reexecute = true` at the guarded operation's bci.

#### Uncommon trap sites — the `brk #imm` convention

AArch64 `brk #imm16`. MACVM claims the `0xDExx` namespace:

| imm16    | meaning |
|----------|---------|
| `0xDE00` | uncommon trap (guard failure / cold path) — deopt + count |
| `0xDE01` | forced trap from `MACVM_DEOPT_STRESS` instrumentation — deopt, counted separately, does NOT count toward `UncommonTrapLimit` |
| `0xDE02` | compiled-code assertion ("should not reach") — VM bug, panic with pc |
| anything else | **not ours** — restore `SIG_DFL` and return (the instruction re-executes and the process dies with the default disposition). Rust's `abort()` lowers to `brk #1` on arm64 — this rule keeps Rust aborts fatal. |

The trap **site**, not the imm, identifies the deopt state (faulting pc →
nmethod → `PcDesc::find`); the imm only namespaces against foreign `brk`s.

S13's two organic trap clients (real traffic before S14):

1. **Compiled smi arithmetic overflow / non-smi operand** cold paths
   (runtime-call fallbacks in S10) become `brk #0xDE00`, `reexecute=true` at
   the send's bci → the interpreter re-executes the send, the primitive
   fails, the LargeInteger fallback runs (SPEC §1.3, §10).
2. **`mustBeBoolean`** cold side of compiled `br_true/br_false`
   (`reexecute=true` at the branch bci).

#### Trap counters, nmethod state, pending deopts

```rust
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum NmState { Alive, NotEntrant, Zombie }

pub struct PendingDeopt { pub orig_ret_pc: usize, pub nm: NmethodId }

// in VmState (runtime side)
pub trap_counts: HashMap<(NmethodId, u32 /*code_off*/), u32>,
pub const UNCOMMON_TRAP_LIMIT: u32 = 1_000; // tunable, SPEC §8.7 (Self's value)
/// keyed by the FP of the frame whose saved-LR slot was rewritten
pub pending_deopts: HashMap<usize, PendingDeopt>,
pub pending_deopt_flag: bool,   // checked by compiled loop polls
```

`rt_uncommon_trap` bumps the site count; crossing the limit marks the
nmethod `not_entrant`, so the next trigger recompiles it — S14's compiler
then sees the enriched IC state and compiles without the failed assumption
(in S13 the recompile is merely identical, which is harmless).

#### Dependency index

Authoritative dependency data lives **in each nmethod**: `deps[]` at
`header.deps_off` is a list of `(klass_pool_ix: u32, selector_pool_ix: u32)`
pairs — pool indices, so a moving GC keeps them valid for free. The Rust-side
multimap is a **rebuildable cache** of that data:

```rust
// src/runtime/deps.rs
pub struct DependencyIndex {
    /// (klass, selector) → nmethods that assumed lookup(klass, selector).
    /// Keys are raw oops: valid only until a moving GC; `dirty` forces rebuild.
    map: HashMap<(Oop /*KlassOop*/, Oop /*SymbolOop*/), SmallVec<[NmethodId; 4]>>,
    dirty: bool,
}

impl DependencyIndex {
    pub fn record(&mut self, klass: KlassOop, selector: SymbolOop, nm: NmethodId);
    pub fn remove_nmethod(&mut self, nm: NmethodId);
    /// Every nmethod whose recorded (K', sel) is affected by installing
    /// `selector` in `holder` — the match rule is algorithm D2.
    pub fn affected_by_install(
        &mut self, universe: &Universe, code: &CodeTable,
        holder: KlassOop, selector: SymbolOop,
    ) -> Vec<NmethodId>;
    /// Called by any GC that moved objects: sets `dirty`; the next query
    /// rebuilds from CodeTable + nmethod deps[].
    pub fn invalidate_cache(&mut self);
}
```

Rationale: symbols and klasses move (young symbols under scavenge, everything
under full GC), so oop-keyed hashes cannot survive a collection; rebuild cost
scales with the (small) count of alive nmethods, from GC-maintained pool
indices.

**Hooks** (SPEC §6.2): after installing `(holder, selector)` in the
MethodDictionary and flushing the lookup cache, call
`invalidate_dependents(vm, holder, selector)` (algorithm D1).

### Algorithms

#### D1. Invalidation (method redefinition or `MACVM_DEOPT_STRESS` periodic)

Runs on the VM thread, **allocates nothing and runs no Smalltalk code** —
legal to call anywhere outside GC, and never called *during* GC
(installation is a primitive; GC does not install methods):

1. `victims = deps.affected_by_install(holder, selector)`.
2. For each victim nmethod `nm`:
   a. Set `nm.state = NotEntrant`; remove from `CodeTable`. Interpreter ICs
      holding its id self-heal: dispatch-through-id checks `state` first.
   b. **Entry patching**: with the JIT write toggle open
      (`pthread_jit_write_protect_np(0)`), overwrite the first instruction at
      BOTH `entry` and `verified_entry` with `b not_entrant_stub` — a shared
      startup stub that re-dispatches via normal lookup for (receiver klass,
      selector), exactly like an IC miss. Close toggle,
      `sys_icache_invalidate` both words. (S11 guarantees a patchable
      instruction at offset 0 of each entry.)
   c. **Lazy return-address redirection** (Strongtalk's approach, SPEC §8.7):
      walk the native stack (FP chain, continuing through each
      interpreter↔compiled boundary the ProcessStack records). For every
      frame record whose `saved_lr` lies inside `nm`'s code range:
      `pending_deopts.insert(fp_of_the_nm_activation,
      PendingDeopt { orig_ret_pc: saved_lr, nm })` and overwrite the saved-LR
      **stack slot** (plain data — no icache flush, no JIT toggle) with
      `deopt_return_trampoline`'s address. The nm activation's own frame is
      NOT touched — it runs old code until control next crosses one of its
      boundaries (return into it, trap inside it, or loop poll).
   d. Set `vm.pending_deopt_flag = true` so **loop polls** in fully-inlined
      loops (no calls → no return to redirect) also drain: the poll slow path
      `rt_poll(vm, fp, pc)` deopts the polling frame if its nmethod is
      `NotEntrant` (poll sites have scope descs; `reexecute=true` at the
      loop-header bci). The flag clears when a full stack walk finds no live
      NotEntrant frames.
3. Zombie/freeing: each **full GC** sweeps all stacks; a `NotEntrant` nmethod
   referenced by no frame pc/redirected-LR becomes `Zombie`: deps leave the
   index, its code-cache block returns to the free list (SPEC §8.6, §9).
   Semantics: existing activations complete under the **old** method (scope
   descs hold the old MethodOop); new sends see the new method immediately
   via the lookup-cache flush.

#### D2. `affected_by_install` subclass rule

A dependency (K', sel) means "compilation assumed `lookup(K', sel)` resolves
to the method it saw". Installing `sel` in `holder` changes that result —
v1 conservatively — iff `K' == holder` or `holder ∈ superclasses(K')`:
implemented by walking K's super chain (cheap; no subclass links exist in
the klass layout, SPEC §2.4, and none are needed). Superclass *changes* are
unsupported in v1 (world files only add methods, SPEC §1.2), so
hierarchy-change invalidation is out of scope.

#### D3. Trap delivery — the SIGTRAP handler (macOS arm64, honest version)

`brk` raises a Mach `EXC_BREAKPOINT`; with no Mach exception port claiming
it, the kernel converts it to **SIGTRAP** on the faulting thread. v1 takes
the POSIX route: `sigaction(SIGTRAP, SA_SIGINFO)`, installed once at startup
by `codecache::deopt_trap::install()`. Constraints and the design:

- **Debugger caveat (document loudly)**: lldb claims `EXC_BREAKPOINT` *before*
  SIGTRAP conversion, so under lldb every trap stops the debugger. Mach
  exception ports are the robust v2 path; v1 accepts the caveat (README:
  `MACVM_JIT=off` for lldb sessions).
- **Signal-safety**: the handler is async-signal-safe by doing almost nothing
  — no allocation, locks, string formatting, or runtime calls. Its whole job
  (≈20 lines of `unsafe` in `deopt_trap.rs`):
  1. Read the fault pc from `(*(*ctx).uc_mcontext).__ss.__pc`
     (`libc::ucontext_t` → `__darwin_mcontext64`, arm64 `__ss` fields).
  2. Check pc is inside the code-cache range (two u64 compares against
     bounds cached in write-once statics at startup — the ONE permitted
     global) and that `*(pc as *const u32)` decodes as
     `brk #(0xDE00..=0xDE02)` (encoding `0xD4200000 | imm16 << 5`; the code
     cache is always readable, so the load is safe).
  3. Not ours → `sigaction(SIGTRAP, SIG_DFL)` and return (re-execution kills
     the process with the default disposition).
  4. `imm == 0xDE02` → rewrite pc to a tiny stub calling
     `rt_compiled_assert_failed` (which panics) — still no work in-handler.
  5. Otherwise **redirect**: set `__ss.__x[16] = pc` (stash the trap pc in
     IP0 — scratch, never live at a safepoint) and
     `__ss.__pc = DEOPT_UNCOMMON_TRAMPOLINE`; return.
  All real work happens *after* sigreturn, in ordinary Rust reached via the
  trampoline. The handler never triggers GC, takes the JIT write toggle, or
  touches `VmState`.
- PAC is off for VM-internal control flow (`arm64.md` §5 baseline), so
  rewriting `__pc` and later frame surgery need no `pacia/autia` — state this
  next to the code; it is the reason the whole design is legal as written.

#### D4. `deopt_uncommon_trampoline` (generated startup stub, SPEC §9)

Runs right after sigreturn, trapped compiled frame intact (FP = its frame),
trap pc in `x16`:

```
mov lr, x16                  // lr is dead; make saved-lr = trap_pc so the
stp fp, lr, [sp, #-16]!      // stack walker sees the trapped frame at
mov fp, sp                   // pc = trap_pc through a normal frame record
mov x0, x28                  // &mut VmState (VM-state register, arm64.md §3)
mov x1, x16                  // trap pc
mov x2, fp-of-trapped-frame  // (= incoming fp, reloaded from [fp])
bl  rt_uncommon_trap         // Rust; returns result oop in x0
mov sp, <trapped frame fp>   // tear down trampoline + trapped frame
ldp fp, lr, [sp], #16
ret                          // return the deopt result to the native caller
```

(Emit via the S9 `Assembler`; normative in effect, not exact instruction
choice.) The frame-record trick is load-bearing: while `rt_uncommon_trap`
runs (and may GC), the stack walker must see the trapped nmethod frame at
pc = trap pc so S12's oop maps cover it. GC then scans both the logically
dead compiled slots and the materialized interpreter frames — double-scanning
copies of the same oops is harmless (each copy updates consistently).

#### D5. `rt_uncommon_trap` and frame materialization

```rust
// src/runtime/deopt.rs
pub struct FrameView {
    pub fp: usize,          // trapped/returning compiled frame's FP
    pub pc: usize,          // trap pc, or orig_ret_pc for return-path deopt
    pub nm: NmethodId,
    /// Some(result) via deopt_return_trampoline (reexecute=false sites):
    /// the completed call's value, to push on the materialized stack.
    pub incoming_result: Option<Oop>,
}

/// Materialize interpreter frames for `frame` onto the ProcessStack. Does
/// NOT run them. Allocates (Contexts) — callers hold no raw oops across it.
pub fn deoptimize_frame(vm: &mut VmState, frame: FrameView) -> ();

/// extern "C" entry the trampolines call: deoptimize_frame + nested
/// interpreter run of the materialized frames; returns the result oop.
pub extern "C" fn rt_uncommon_trap(vm: &mut VmState, trap_pc: usize, fp: usize) -> u64;
```

Materialization (compiled physical frame → N interpreter frames; N = 1 in
S13, ≥1 in S14):

- **M0.** `PcDesc::find(nm, pc - nm.code_base)`; decode the safepoint record,
  then the scope chain innermost→outermost via `sender_scope_off`; collect
  and reverse (process outermost first).
- **M1.** Bump `vm.stats.deopt_count`; trace under `MACVM_TRACE=deopt`.
- **M2.** Record the ProcessStack watermark `base_sp` (the nested run ends
  when the stack pops back below it).
- **M3.** For each virtual frame, outermost → innermost:
  1. Read receiver and unified-slot values via `ValueLoc` against the
     physical frame: `FrameSlot(off)` → `*(fp + off)`; `ConstPool(i)` →
     nmethod pool word i (GC-current); `ConstSmi(v)` → tag as smi; `Nil` →
     `nil_obj`. (S14 inlined scopes: outer frames' pending stacks come from
     their child's `SenderLink.pending_stack`.)
  2. Push onto the ProcessStack, per SPEC §5.1 layout: pending operand stack
     (outer scopes only), then receiver, then args (unified slots `0..argc`
     — the callee's negative-offset arg area, reproducing the interpreter's
     caller-pushes-args overlap); then the frame header: `method` (scope's
     pool ix — the **old** MethodOop), `saved_fp` (previous materialized
     frame's FP, or the Rust-entry sentinel), `saved_bci` (outer frames:
     `sender_bci + len(send bytecode)`, the post-send resume point;
     innermost: set in M5), `context` (M6), `receiver` copy at FP+4, temps
     (unified slots `argc..`), then the operand stack values.
  3. **Innermost frame only**: the recorded `stack` values; if
     `reexecute == false`, additionally push `incoming_result`
     (must be `Some`; assert). If `reexecute == true`, `incoming_result`
     must be `None`; the recorded stack already holds the operation's inputs.
- **M4.** Operand-stack height check (THE classic deopt bug — see Pitfalls):
  `debug_assert_eq!(materialized_height, interpreter_model_height(method, resume_bci))`,
  using the compiler decode stage's bytecode abstract interpreter (debug builds).
- **M5.** Innermost resume bci: `bci` if `reexecute` else
  `bci + bytecode_len_at(method, bci)`.
- **M6.** Contexts: `CtxLoc::Materialized(loc)` → store the read oop in the
  frame's context slot. `CtxLoc::Elided { temps }` → read all temp values
  FIRST into a `HandleScope`, then allocate a Context (allocation may GC —
  the compiled frame is still walkable per D4, values rooted in handles),
  fill it, store it. `None` → nil.
- **M7.** Run `Frame::verify` on every materialized frame — it must be
  indistinguishable from one the interpreter built (SPRINTS S13 gate).
- **M8.** Nested resume: `interpret_active(vm, base_sp, resume_bci)` — the
  interpreter loop parameterized to start at an arbitrary bci of the top
  frame and return when the frame at `base_sp` returns. Its result oop is
  `rt_uncommon_trap`'s return value, which the trampoline hands to the native
  caller as if the compiled method returned normally. (Nests the Rust
  interpreter function — bounded by deopt frequency, the same nesting the
  S11 compiled→interpreted adapter already performs.)

#### D6. `deopt_return_trampoline` (return-path deopt)

Hit when a callee returns into a NotEntrant frame whose saved-LR slot was
redirected (D1c). Result is in `x0`; control was about to land at
`orig_ret_pc` with FP = victim's FP. The stub saves x0, builds the same
walker-visible frame record as D4, and calls `rt_deopt_on_return(vm, fp,
result)`, which consumes `pending_deopts[fp]` and runs
`deoptimize_frame`/nested-resume with `FrameView { pc: orig_ret_pc,
incoming_result: Some(result), .. }`; the stub then tears down the victim
frame and `ret`s the final result to *its* caller. `orig_ret_pc` sites are
call returns → `reexecute == false` by construction.

#### D7. `MACVM_DEOPT_STRESS=1`

Two coordinated behaviors (SPEC §12.6):

1. **Every trap-eligible site fires once.** Compile-time instrumentation: the
   compiler allocates a per-nmethod *stress bitmap* (one byte per trap site,
   in the nmethod data area) and prefixes each trap-eligible guard with: load
   site byte; if 0 → store 1, branch to a dedicated `brk #0xDE01` recorded
   with the same SafepointState as the real guard's trap. The first execution
   of every guard thus deopts regardless of outcome; the nmethod stays Alive
   (`0xDE01` doesn't count toward the limit), the activation finishes
   interpreted, and the next call re-enters compiled code. Program output
   must equal unstressed runs (differential transcripts, SPEC §12.5–12.6).
2. **Periodic invalidation**: a countdown in `VmState`
   (`stress_invalidate_every = 1_000` compiled calls *(tunable)*), decremented
   in the compiled-entry dispatch path; at zero, pick the next Alive nmethod
   round-robin and run D1 on it as if its key method were redefined.

### Layer boundaries

| Module | May touch | Must not touch |
|---|---|---|
| `compiler/scopes.rs` (safe) | compiler IR, regalloc results, nmethod blob layout | signals, ucontext, ProcessStack |
| `codecache/deopt_trap.rs` (unsafe OK) | sigaction/ucontext, code-cache bounds, stub generation, JIT toggle | heap allocation, Universe, interpreter |
| `runtime/deopt.rs` (safe) | ProcessStack, Handles, interpreter entry, PcDesc/ScopeDesc decode, VmState stats | raw pointers into native stack **except** through `FrameView` reads via a small checked helper `read_frame_slot(fp, off) -> Oop` re-exported from `codecache` |
| `runtime/deps.rs` (safe) | CodeTable, Universe klass walks | code patching (delegates to `codecache::patch`) |

Signal handler ⇒ `codecache` per standing rule 4 (unsafe modules).

## Implementation order

1. LEB128 read/write + unit tests (`scopes.rs`).
2. `ValueLoc/CtxLoc/ScopeDescData/SafepointState/PcDesc` + pack/unpack
   round-trip tests (no compiler integration yet).
3. `ScopeDescRecorder` wired into emit; every S12 safepoint records a site;
   nmethod gains `scopes_off/pcdesc_off` payloads (fields reserved,
   SPEC §8.2). Golden-disasm updated.
4. `PcDesc::find` + decoder (`ScopeDesc::at(nm, code_off)` chain iterator).
5. `deopt_trap.rs`: handler install, namespace check, trampoline stubs
   (generated at startup with the other SPEC §9 stubs); unit test `brk`s from
   a hand-emitted stub and asserts the redirect fires.
6. `runtime/deopt.rs`: `FrameView`, materializer M0–M7 (no nested run yet);
   tested against hand-built compiled frames.
7. `interpret_active` + M8; `rt_uncommon_trap` end-to-end via the two organic
   trap clients (smi overflow, mustBeBoolean).
8. Nmethod state machine + entry patching + `not_entrant_stub`.
9. Return-address redirection walk + `deopt_return_trampoline` +
   `pending_deopts`.
10. `DependencyIndex` + installation hook + loop-poll deopt check + zombie
    sweep at full GC.
11. `MACVM_DEOPT_STRESS` (both behaviors), `MACVM_TRACE=deopt`, stats
    (`deopt_count`, `deopt_by_reason[trap|return|poll]`, `traps_per_site`).

Each step compiles and keeps all prior gates green.

## Pitfalls

- **Exact operand-stack height at the bci** is the classic deopt bug: an
  off-by-one (stack recorded post-arg-pop but resumed with reexecute=true, or
  vice versa) produces frames that *mostly* work. `SafepointState.reexecute`
  is the single source of truth; M4's height check catches violations in
  debug builds. Never "fix" a height mismatch in the materializer.
- **Materialized frames must pass `Frame::verify`** — on every frame (M7),
  not just in tests; when verifier and materializer disagree, the
  materializer is wrong.
- **Foreign `brk`s**: Rust `abort()`/`unreachable` emit `brk` too; the imm
  namespace check + SIG_DFL re-raise (D3 step 3) keeps Rust aborts fatal.
- **No work in the signal handler.** Everything observable (allocation, GC,
  locks, `VmState` mutation, printing) happens after sigreturn in
  `rt_uncommon_trap`. The handler only inspects and redirects.
- **Invalidation must not run Smalltalk code**: D1 only flips state, patches
  instructions, and rewrites stack *data* slots. Materialization — which
  allocates and can GC — happens strictly later, at trap/return/poll time.
  Never materialize from inside a GC-triggered path.
- **Icache discipline**: every instruction patch (D1b) needs the JIT write
  toggle + `sys_icache_invalidate` (`arm64.md` §4.3). Saved-LR slots are stack
  *data* — no toggle, no flush; do not cargo-cult one in.
- **Handle discipline in M6**: `Elided` context temps must be in handles
  before the Context allocation; a raw `Vec<Oop>` across it is exactly the
  bug `MACVM_GC_STRESS=1` exists to catch (SPEC §7.6).
- **PcDesc key convention**: calls key on the *return address* offset; traps/
  polls on the instruction itself. Mixing them makes `find` miss by 4 bytes —
  assert kind-vs-convention in `record_site`.
- **Inlined-block traps (forward-compat, S14)**: a trap inside an inlined
  block materializes the *home method's* frame chain; NLR out of the
  materialized block then uses the ordinary `home_frame_ref` machinery —
  which is why M6 rebuilds real Contexts rather than faking them.
- **Old-method semantics**: materialized frames reference the compile-time
  MethodOop (pool), not the current dictionary entry — redefinition
  mid-activation completes under old code (D1). Don't "helpfully" re-look-up.
- **`x16` at trap sites**: stashing the trap pc in IP0 is sound only because
  compiled code has no live value in IP0/IP1 across a safepoint
  (branch-veneer scratch, `arm64.md` §3); keep the regalloc off x16/x17.

## Interfaces for later sprints

- `ScopeDescRecorder::begin_scope` with `SenderLink` — S14 registers one
  scope per inlined method/block; the packed format needs **no changes** for
  depth-N chains or `CtxLoc::Elided`.
- `trap_counts` + `UNCOMMON_TRAP_LIMIT` → S14's policy reads these to
  recompile-without-assumption.
- `deoptimize_frame(&mut VmState, FrameView)` + `interpret_active` → S15 OSR
  reuses both unchanged for the "OSR frame deopts back" path.
- `DependencyIndex::record` — S14's inliner records one dependency per
  inlined (receiver klass, selector).
- `pending_deopt_flag` loop-poll check → S15's OSR poll shares the slow path.

## Out of scope

- Inlined scopes of depth > 1, `CtxLoc::Elided` emission, poly-guard traps —
  S14 (format shipped now, dark). OSR in either direction — S15.
- Mach exception ports (trap-delivery v2), background compilation,
  live-register `ValueLoc::Register`, PAC-signed return addresses — post-v1.
- Superclass-change invalidation (no such operation exists in v1, §D2);
  code-cache compaction of zombies (free list only, SPEC §9).

> **SPEC-QUESTION:** CONVENTIONS §3 pins `MACVM_TRACE=bytecode|gc|jit|ic`;
> S13 needs a `deopt` channel (S15 will want `stats`). Proposing the list
> become open-ended: unknown channel names are ignored with a warning.

> **SPEC-QUESTION:** SPEC §8.2's nmethod header lists `deps_off` but no
> per-site trap-counter storage; S13 keeps counters in a Rust-side map
> (`VmState.trap_counts`, NmethodId-keyed so GC-immune). If profiling shows
> map overhead, counters could move into the nmethod data area — flagging
> the layout question rather than deciding it.
