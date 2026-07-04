# Sprint S12 — Moving GC under compiled frames

Objective: the two hard subsystems coexist. The scavenger and the full
collector find, trace, and relocate every oop held by compiled activations
(spill slots via oop maps), by code (literal pools via reloc records), and by
the IC machinery (PIC/adapter/mega pools) — while compiled frames are live on
the native stack. Deletes S11's temporary GC bridge. Implements SPEC §8.5
(GC interaction), §7.3/§7.5 (root sets extended), the nmethod key-klass
weakness + flushing, and the flagship combined gate (`threshold=1` +
`MACVM_GC_STRESS=1`).

## Prerequisites

- S11 complete: send sites with PcDescs at every call/alloc-slow return
  address, RootSpill-disciplined stubs + adapters (pinned offsets in
  `layout.rs`), anchor protocol (`last_compiled_fp/pc`), `TierLink`s,
  `PicTable`/`AdapterTable`/mega map, D8 bridge in `memory/allocator.rs`
  (marked `// S12: remove`).
- S10: regalloc `slot_is_oop` + spill-all-at-safepoint policy + its debug
  assert; `OopMap`/`PcDesc` emission plumbing; `CodeTable::{oops_do,
  find_by_pc, rehash}`.
- S7/S8: scavenger + full GC with pluggable root iteration
  (`memory::roots::each_root(vm, f)` — if S7 hard-coded the root list,
  refactor to a visitor FIRST, as step 0).

## Deliverables

- `src/compiler/oopmap.rs` — `OopMap` format, builder, decoder, verifier.
- `src/codecache/nmethod.rs` — finalized `PcDesc`, per-nmethod map tables,
  `Nmethod::oops_do_frame` helpers.
- `src/runtime/frames.rs` — `FrameView` + `walk_frames` unifying process
  stack, native compiled frames, adapters, stubs.
- GC integration: compiled-frame root enumeration in scavenge + full GC;
  literal-pool rewriting via relocs (already live from S10 — extended +
  verified here); key-klass weakness; nmethod flush + IC/PIC invalidation
  on free; bridge deletion.
- The flagship gate wiring: `just gate-s12`.

## Design

### D1. Safepoints — the v1 definition (explicit and enforced)

**A compiled-code safepoint is the return address of a call emitted for
`CallSend`, `CallRuntime`, or the slow edge of `Alloc`. Nothing else.**
Loop polls are NOT GC safepoints in v1: `stub_poll` neither allocates nor
GCs (S10 D5.6), so a frame stopped in a poll has its GC-relevant state
identical to the poll's enclosing call-free region — and since the VM is
single-threaded and GC only ever starts inside an allocating runtime call,
every compiled frame on the stack at GC time is ALWAYS suspended at a
safepoint return address. This holds **by construction**; the walker asserts
it (D4, exact PcDesc match required).

**The spill-all invariant (restated from S10 D3.5, now load-bearing):**
at every safepoint, no oop lives in a register — every oop live across the
call is in a spill slot for its entire interval. Therefore **oop maps cover
stack slots only**; there are no register maps in v1. Enforcement is
mechanical, in two places:

1. `regalloc`: any interval with `crosses_safepoint` is `Assignment::Spill`
   (policy), followed by `verify_spill_all(&intervals, &safepoints)` — a
   release-mode-cheap pass that walks every safepoint position and panics if
   any `Assignment::Reg` interval spans it. Runs always, not just debug
   (it is O(intervals·safepoints), trivial).
2. `oopmap::verify(nm)` (debug + stress): decodes every map and checks
   `bit ⇒ slot < frame_slots` and `bit ⇒ slot_is_oop[slot]`.

Resist register maps, callee-saved carrying, or safepoints-at-polls until
v2 — spill-all is exactly what makes this sprint one sprint.

### D2. OopMap + PcDesc — formats

```rust
// src/compiler/oopmap.rs
/// Bitmap over spill slots: bit i set ⇔ frame slot i (at [fp − 8·(i+1)])
/// holds a live oop at the associated safepoint. Slots only — no registers
/// (D1). frame_slots ≤ 60 (S10 eligibility) ⇒ one u64 usually suffices.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OopMap {
    pub bits: SmallVec<[u64; 1]>,       // ceil(frame_slots / 64) words
}
impl OopMap {
    pub fn set(&mut self, slot: u16);
    pub fn is_oop(&self, slot: u16) -> bool;
    pub fn iter_slots(&self) -> impl Iterator<Item = u16> + '_;
}

// src/codecache/nmethod.rs
#[derive(Clone, Copy, Debug)]
pub struct PcDesc {
    pub pc_off: u32,      // RETURN-ADDRESS offset from code base (call + 4)
    pub bci: u16,         // bytecode index of the send/alloc (traces, S13 seed)
    pub oopmap: u16,      // index into Nmethod.oopmaps; u16::MAX = block-start
                          // entry (trace-only, no map — S10 legacy entries)
}
// Nmethod.pcdescs: Vec<PcDesc> sorted by pc_off (binary search);
// Nmethod.oopmaps: Vec<OopMap> — identical maps deduplicated by the builder.

impl Nmethod {
    /// Exact-match lookup — GC path. Panics (VM bug) if `ret_pc` is not a
    /// recorded safepoint: a compiled frame suspended anywhere else means
    /// the D1 invariant broke.
    pub fn oopmap_at(&self, ret_pc: u64) -> &OopMap;
    /// Nearest-below lookup — trace path (any pc inside the method).
    pub fn bci_at(&self, pc: u64) -> u16;
}
```

The emit stage (S10 D7 plumbing, live since S11 emitted calls) fills these:
at each call's return offset, snapshot `slot_is_oop ∧ slot_live_at(pos)` —
liveness at the safepoint, not the whole method, so dead slots don't drag
garbage through a collection (compute from the same interval data:
slot live ⇔ its interval contains the safepoint position).

### D3. FrameView + walk_frames — the unified walker

```rust
// src/runtime/frames.rs
pub enum FrameView<'a> {
    Interpreted { frame_index: usize, method: MethodOop, bci: usize },
    Compiled    { fp: u64, ret_pc: u64, nm: NmethodId },
    Adapter     { fp: u64, kind: AdapterKind },   // c2i / resolve / mega / dnu /
                                                  // alloc-slow / poll stubs
    CallStub    { fp: u64 },                      // the I→C trampoline frame
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AdapterKind { C2i, Resolve, Mega, Dnu, AllocSlow, Poll }

/// Walk all activations of the (single) process, innermost first,
/// interleaving native compiled/stub frames and process-stack interpreter
/// frames via VmState.tier_links + the anchor.
pub fn walk_frames(vm: &VmState, f: impl FnMut(FrameView<'_>));
```

Algorithm (single-threaded; `tier_links` is a faithful journal of every tier
crossing):

1. **Start segment.** If `vm.last_compiled_fp != 0`, the VM is in Rust
   called from compiled code: start at `(last_compiled_fp,
   last_compiled_pc)`. Otherwise the innermost activation is interpreted:
   start with the interpreter frame at `vm.current_frame`, and skip to
   step 3 with the topmost un-consumed TierLink.
2. **Native segment.** From `(fp, pc)`: classify `pc` — in an nmethod range
   (`code_table.find_by_pc`) → `Compiled`; in a stub/adapter range
   (`RuntimeStubs`/`AdapterTable` ranges) → `Adapter`; equal to the call
   stub's interior → `CallStub`, END of segment. Step: `pc = [fp+8]`,
   `fp = [fp]`. Every `Compiled` frame's `ret_pc` (the pc by which the
   walker LEFT it, i.e. its resume address) must exact-match a PcDesc when
   the walk is on behalf of GC (D1 assert).
3. **Interpreter segment.** Consume the matching
   `TierLink::IntoCompiled { interp_frame, .. }` and walk process-stack
   frames from `interp_frame` downward (SPEC §5.1 layout) until a frame that
   is the base of the next `TierLink::IntoInterpreter { compiled_fp, .. }`
   → resume step 2 at that `compiled_fp`. Repeat until both the tier-link
   stack and the process stack are exhausted.

Consumers: GC root iteration (below), `print_stack_trace` (replaces S10's
version), S13's deopt scan, the recompilation policy's caller walk
(SPEC §8.1, S14).

### D4. GC integration

**D4.1 Root enumeration additions** (both collectors — one visitor):

```rust
pub fn each_code_root(vm: &mut VmState, f: &mut dyn FnMut(&mut u64)) {
    // 1. compiled frame spill slots (via oop maps):
    walk_frames-mut variant over raw slots:
      Compiled { fp, ret_pc, nm } →
        for slot in nm.oopmap_at(ret_pc).iter_slots() {
            f(&mut *((fp − 8·(slot as u64 + 1)) as *mut u64))
        }
      Adapter { fp, kind } →  f over that kind's FIXED RootSpill slots
        (offsets from layout.rs — 6 arg words + method word for C2i, 6 arg
        words for Resolve/Mega/Dnu, klass word for AllocSlow, none for Poll)
      CallStub / Interpreted → nothing here (interpreter frames are already
        roots via the process-stack scan, SPEC §7.3 item “process stacks”)
    // 2. code-embedded oops:
    code_table.oops_do(cache, f)      // nmethod pools + IcSite selectors (S10/S11)
    pic_table.oops_do(cache, f)       // PIC klass pool words
    adapter_table.oops_do(cache, f)   // c2i method pool words
    mega_map keys                     // selector oops (Rust-side u64 bits — visit + rehash)
}
```

Scavenge calls this with the copy/forward closure; full GC calls it in mark
(as strong roots, EXCEPT key-klass words — D5) and again in the update phase
with the pointer-rewrite closure. All pool writes happen inside one
`JitWriteGuard` per phase (pool words are data — the flush is belt-and-
braces, S9 P5/D3.3 rationale).

**D4.2 Code does not move — the invariant, stated once:** the code cache
never relocates a published blob (bump + free list, no compaction — SPEC §9).
Therefore return addresses into code held in native frames, `TierLink`s, the
anchor, and interpreter-IC smi ids **never need rewriting by GC**. The only
GC-mutable things in the cache are 8-byte pool words. Any future compacting
code GC must revisit every one of those holders — that is why it is a
stretch goal and why `CodeCache::free` requires the no-live-frames proof
(D6.2).

**D4.3 Bridge deletion.** Remove S11 D8 from `memory/allocator.rs`
(`compiled_depth` stays — traces + asserts use it). Scavenge and full GC now
run with compiled frames on stack as a matter of course. Keep
`vm.stats.gc_under_compiled` counting (now expected > 0 under the flagship
gate — the S11 test asserting it was zero is inverted, see tests).

### D5. nmethod key-klass weakness + flushing (full GC only)

Ordering within full GC (extends SPEC §7.5):

1. **Mark** from all roots. Code roots participate EXCEPT pool words whose
   reloc kind is `KeyKlassOop` (the customization key klass — SPEC §8.5
   "weak treatment of nmethod key.klass"): these are skipped by
   `oops_do`'s mark pass (an `oops_do_filtered(kinds)` variant). The
   Rust-side `Nmethod.key_klass` field is likewise NOT marked through.
   Everything else in pools (method literals, selectors, guard klasses in
   PICs, adapter method oops) stays STRONG in v1 — deliberately
   conservative; only the key is weak, so only true klass death flushes.
2. **Weak sweep**: for each alive nmethod, if `key_klass` is unmarked →
   add to `flush_set`. Invariant check: a dead key klass implies no live
   activation of that nmethod exists (any receiver would have marked the
   klass via its header) — `debug_assert` by walking frames for members of
   `flush_set`.
3. **Flush** (D6) the set — BEFORE the update phase, so no updater touches
   freed memory.
4. **Update phase** rewrites all surviving references, including surviving
   `KeyKlassOop` pool words and `Nmethod.key_klass/key_selector` fields
   (weak ≠ unmaintained), interpreter stacks, compiled frames (D4.1 again
   with the rewrite closure), then `code_table.rehash()`, PIC/adapter/mega
   map rehashes (S11 P11), lookup-cache flush (SPEC §7.5).

Scavenge never flushes: key klasses are old-gen-reachable or the nmethod is
newborn; treat all code-root kinds strongly at scavenge (a young key klass
is kept alive by its nmethod — acceptable float, gone at the next full GC).

### D6. Flushing an nmethod (also the S13 zombie path's substrate)

`CodeTable::flush(vm, id)`:

1. `state = Zombie`; remove from `by_key` (id slot retained, `find_by_pc`
   entry retained until step 4 so a concurrent walk this cycle still
   classifies frames — flush is only called when step-2's no-activations
   invariant holds, but ordering discipline costs nothing).
2. **Compiled-site invalidation sweep**: for every alive nmethod, for every
   `IcSite`, read the current `bl` target word; if it resolves into the
   flushed nmethod's `code` range (directly or via a recorded veneer) →
   `icpatch::reset_site` (repatch to `stub_resolve`, state Unresolved).
   For every `PicDesc`: if any pair's target is in range → free the PIC and
   reset its owning site. One `JitWriteGuard` around the sweep. O(all
   sites) per flush — flushes are rare (klass death, redefinition);
   the reverse-dependency index that makes this O(callers) is S13's
   dependency work, don't build it twice.
3. **Interpreter ICs**: NO sweep — the id-validation dispatch (S10 D4)
   makes stale smi ids self-heal on next use. This is the pinned mechanism;
   S13's dependency index supersedes it for eager invalidation but the
   validation check REMAINS as the safety net.
4. Remove `by_addr` entry, `cache.free(code)`, drop the `Nmethod`
   (slot → None; id reusable — safe because of the validation check).

### D7. Return-address safety recap (what could go wrong, pinned)

| holder of a code address | why it stays valid |
|---|---|
| native frame return addrs (bl continuations) | code never moves (D4.2); callee flush requires no-activations (D5.2/D6) |
| anchor `last_compiled_pc` | same |
| interpreter IC smi ids | validated on every dispatch (S10 D4) |
| compiled IC `bl` targets | reset by the D6.2 sweep when target dies |
| PIC pair targets | PIC freed by the same sweep |
| `TierLink` snapshots (sp/fp, not pcs) | native stack, untouched by GC |

## Implementation order

0. If needed: refactor S7/S8 root iteration to the visitor form.
1. `oopmap.rs` (format, builder from interval data, decoder, verifier) —
   pure, unit-tested against hand intervals.
2. PcDesc finalization + `oopmap_at` exact-match + emit-side liveness
   snapshot. Listing/pcdesc golden for one two-send method.
3. `frames.rs` walker + `print_stack_trace` re-pointed onto it; mixed-trace
   goldens from S10/S11 re-verified.
4. `each_code_root` — scavenge integration first (strong everything);
   mid-loop-forced-scavenge unit test (tests file) green.
5. Full-GC integration: mark with weak-key filtering, weak sweep, update
   phase ordering, rehashes.
6. `CodeTable::flush` + invalidation sweep + `cache.free` wiring; key-klass
   death test green.
7. Delete the S11 bridge; flip the S11 bridge-assert test.
8. Flagship gate runs + soak.

> **STEP-3 NOTES (as-built).** (a) **The anchor needs a FOURTH field,
> `last_compiled_kind`, this doc never anticipated.** D3's own text implies
> a native frame's kind is derivable from its pc; it isn't, for the
> anchor's own first frame specifically. Standard AArch64 convention means
> x30 at a callee's own prologue is always the CALLER's resume address —
> so `last_compiled_pc` (x30 captured by `emit_stub_prologue`) describes
> the anchor-setting stub's own CALLER (a real compiled method), never the
> stub's own code, and none of the six anchor-setting stubs
> (`resolve`/`c2i_shared`/`mega_shared`/`dnu`/`must_be_boolean`/
> `alloc_slow`) is ever reached via a `bl`/`blr` from another stub or
> adapter (per-instance c2i/mega trampolines and PICs are confirmed
> tail-jump-only, never touching x30 — S11's own P2 invariant), so there
> is no pc anywhere "inside" the stub's own code a lookup could classify.
> Fixed by a new `VMREG_LAST_COMPILED_KIND_OFFSET` (`layout.rs`) + each of
> the six stubs' own preamble tagging itself (`codecache::stubs::KIND_*`),
> found via an adversarial-review pass on this exact design BEFORE
> implementing — the same review also caught (b) below before it could
> ship. (b) **The kind-tag write must NOT go inside the shared
> `emit_stub_prologue`** — an earlier draft tried exactly that, using
> x16/x17 as scratch; `build_c2i_shared`/`build_mega_shared` read x17/x16
> (the method/selector oop carried through from the per-instance
> trampoline's own tail-jump) as their OWN first instruction after the
> prologue returns, so writing through either register there would have
> clobbered dispatch itself on every c2i/mega call in the system. Fixed:
> each of the six stubs tags itself, in its OWN preamble, using x9 (free
> in all six, confirmed by direct inspection, not just the review's own
> claim). (c) **A second, independently-found bug, NOT from the review:**
> `interpreter::run_method_reentrant` (S11 D6.1's C2I entry point) didn't
> save/clear/restore the anchor the way it already does for
> `vm.stack`/`vm.regs`. Left alone, the anchor would stay pointed at the
> OUTER `c2i_shared` frame for the whole duration of a reentrant
> interpreted call — even while THAT call's own dispatch loop is genuinely
> the innermost activation — so `walk_frames` (which decides where to
> start purely by checking whether the anchor is nonzero) would have
> started from the wrong, stale, outer frame for any GC triggered by an
> allocation inside such a call. Moot today (the S11 D8 bridge this sprint
> deletes in step 7 forbids exactly this), but would have been a live,
> `GC_STRESS`-shaped corruption path the moment step 7 landed. Fixed by
> giving the anchor the identical save/clear/restore treatment
> `run_method_reentrant` already gives its other two pieces of "which
> activation is currently innermost" state. (d) **`print_stack_trace` is
> NOT re-pointed onto `walk_frames`**, despite this file's own text above —
> see `runtime::frames`'s own module doc for the full reasoning (`stub_poll`
> never tags the anchor, since GC root-scanning never needs it there, but a
> `trace_on_poll`-triggered trace does; teaching `stub_poll`'s own
> hand-rolled, wider-register-set prologue/epilogue to also tag itself is a
> real, separate, riskier change to an already-working mechanism, deferred
> rather than folded in here). `AdapterKind::Poll` stays in the enum,
> documented as unreachable via the CURRENT walker (true for its actual,
> GC-facing consumer), pending that follow-up. (e) Verified via a REAL
> executed I→C→(native safepoint) chain (`AllocTarget basicNew`'s slow
> edge, `walker_classifies_all_kinds`), not hand-faked native memory —
> faking `last_compiled_fp` to an arbitrary value would have `walk_frames`
> dereference bogus stack memory, so there is no honest way to test this
> module without a real in-flight native call; the hook doing this
> (`VmState::test_walk_capture`) catches the walker's own panics with
> `catch_unwind` INSIDE the stub's `extern "C"` function, never letting one
> unwind across that boundary into hand-assembled native frames (UB).

> **STEP-4 NOTES (as-built).** (a) **`each_code_root`'s own signature
> deviates from this doc's D4.1 pseudocode** (`fn each_code_root(vm: &mut
> VmState, f: &mut dyn FnMut(&mut u64))`). That literal shape cannot be
> called with a transform that itself needs `&mut VmState` (`scavenge_oop`
> — exactly what scavenge integration requires): a closure built to capture
> `vm` mutably, then passed alongside `vm` itself as this function's own
> first argument, is two live mutable borrows of the same value for the
> whole call — rejected unconditionally, regardless of what either
> function body does internally. Fixed by copying `for_each_root`'s own
> convention instead: `F: FnMut(&mut VmState, Oop) -> Oop`, threading `vm`
> as an explicit PER-CALL argument to `f` rather than letting `f` capture
> it. The four embedded-pool tables' own `oops_do`/`update_keys` methods
> keep their existing `&mut dyn FnMut(&mut u64)` signature unchanged
> (untouched, already correct since S10/S11) — `each_code_root` bridges to
> them internally via the same `std::mem::take`-then-restore dance
> `scavenge.rs` already used at each of those four call sites before this
> function existed, now consolidated into one place. (b) **Scope is
> scavenge-only, matching the Implementation order's own "strong
> everything" phrase** — `fullgc.rs`'s mark/phase-C sections are
> UNTOUCHED this step (still their own direct table calls), deliberately:
> full-GC integration (weak key-klass filtering) is step 5's job, and the
> D8 bridge still forbids full GC under a live compiled frame regardless,
> so there is nothing for `each_code_root` to do there yet. (c) **The
> flagship `mid_loop_forced_scavenge` test (tests_s12.md gate item 2), AS
> LITERALLY DESIGNED, cannot go green until step 7.** Tracing the actual
> D8 bridge code (not just this doc's own prose) shows it is a genuine
> THREE-part interlock: `alloc::alloc_words`'s diversion arm, BOTH
> `prim_gc_scavenge`/`prim_gc_full`'s own `gc_pending` defer (`alloc.rs`'s
> own doc comment already treats all three, plus both collectors'
> `debug_assert`s, as one interlock, deleted together in step 7) — and the
> test's own design sends the REAL `Gc scavenge` primitive from inside a
> compiled loop 100 times, expecting it to run for real each time. That
> needs `prim_gc_scavenge`'s OWN defer-guard lifted, which is step 7's
> work, not step 4's. Rather than silently skip proving the mechanism,
> added a narrow, default-off, dormant-not-`#[cfg(test)]` field
> (`VmState::test_allow_moving_gc_under_compiled`, mirroring
> `test_walk_capture`'s own established shape) that bypasses ONLY
> `scavenge`'s own internal `debug_assert` — none of the three PRODUCTION
> doors are touched, so ordinary Smalltalk code still cannot reach a
> moving collector under a live compiled frame until step 7 opens all
> three together. `tests/it_gc_jit.rs`'s
> `compiled_frame_spill_slot_survives_forced_scavenge` proves the SAME
> underlying mechanism today — a real scavenge, a real live compiled
> frame, a real relocation — via a direct `scavenge()` call from a new
> `rt_alloc_slow` test hook (`test_force_scavenge_in_alloc_slow` +
> `test_scavenge_probe`, same `catch_unwind`-inside-the-`extern "C"`-
> boundary shape as `test_walk_capture`) instead of a Smalltalk send. The
> full loop-based golden (100 iterations, real `Gc scavenge` sends,
> `gc_under_compiled >= 100`) is deferred to step 7/8. (d) **A systemic
> hazard found while designing that test, NOT yet fixed anywhere:** every
> one of the six anchor-setting stubs' Rust handlers copies an oop out of
> its own RootSpill slot into a plain Rust local BEFORE doing its own
> "business logic" call (`rt_alloc_slow`'s `klass_bits`/`klass`, `stub_
> alloc_slow`'s own `mov x1,x0` running AFTER `emit_stub_prologue`'s `stp`
> already parked the ORIGINAL x0 to RootSpill, so a native memory copy and
> a separate Rust copy coexist). `each_code_root` correctly updates the
> NATIVE copy if a scavenge relocates it, but nothing re-derives any of
> the six handlers' OWN Rust locals from it afterward — moot today (no
> caller reaches a real GC from inside any of the six before step 7 opens
> the other two bridge doors) but a real latent hazard the moment it does.
> Tracked here rather than fixed now: fixing it needs a systematic pass
> across all six handlers, not a one-off patch to `rt_alloc_slow` alone,
> and nothing can exercise it for real before step 7 regardless. The new
> integration test sidesteps it locally by tenuring `AllocTarget`'s own
> klass before compiling/running anything (old gen is never touched by a
> scavenge), which also surfaced the identical shape one level up, in the
> TEST's own setup code (a `KlassOop` local captured before a tenuring
> scavenge that then promoted/relocated it) — fixed there by deriving the
> Rust-local `KlassOop` only AFTER the tenuring scavenge runs. (e) **A
> second, unrelated setup bug found empirically, not by inspection:** forcing
> the Alloc slow edge by lying about `eden.top` (`vm.universe.eden.top =
> vm.universe.eden.end`, `it_tier1.rs`'s own established
> `allocation_fast_and_slow` idiom) is safe only as long as nothing walks
> the heap afterward — that test never scavenges, so the gap of
> never-initialized memory between the real frontier and the lied-about
> `top` is never read. This step's own test DOES scavenge (that's the
> point), and `scavenge`'s entry-verify (`verify::verify_enabled`, always
> on in debug builds) walks eden up to `eden.top` — reading straight into
> that gap and panicking on a malformed mark word. Fixed by topping up
> eden HONESTLY instead: a loop allocating real, fully-valid throwaway
> instances until less than one more fits, so every byte up to the true
> `eden.top` is a genuine walkable object at every point, including inside
> the forced scavenge.

> **STEP-5 NOTES (as-built).** (a) **`CodeTable::oops_do`/`update_keys`
> both gained an `include_key_klass: bool` parameter**, threaded through
> `each_code_root`'s own new second parameter (same name) — the ONLY thing
> D5's weak-key rule touches. Every other call site (scavenge, always;
> full GC's own update/rewrite phase) passes `true`; only full GC's OWN
> mark phase passes `false`. The other three tables (`PicTable`/
> `AdapterTable`/`MegaTable`) are completely unaffected — no key-klass
> concept exists there (PIC guard klasses and adapter method oops stay
> strong unconditionally, D5's own text), so their signatures didn't
> change at all. (b) **`CodeTable::weak_sweep`** (D5 point 2) reads each
> alive nmethod's `key_klass`'s plain mark bit — MUST run strictly between
> phase A (mark) and phase B (`forwarding_compute`), the only window a
> plain (not-yet-forwarded) mark bit is meaningful; `full_gc` now has an
> explicit "phase A.5" for exactly this. (c) **D6's own flush mechanism
> doesn't exist yet (that's step 6) — so `flush_set` is asserted empty
> with a real (non-debug) `assert!`, not silently ignored.** Reasoned
> through why: leaving a dead-key-klass nmethod un-flushed and letting
> phase C's `forward_chase` reach its (never-forwarded, correctly-so)
> `key_klass` would panic anyway via `forward_chase`'s OWN existing
> `debug_assert!(is_forwarded())` — three phases later and one step
> removed from the actual cause. The new `assert!` fails at the source
> instead, with a message naming exactly what's missing. A companion
> debug-only `debug_assert_weak_sweep_invariant` (D5 point 2's own
> "dead key klass implies no live activation" check, walking
> `runtime::frames::walk_frames`) is real, working code, not a stub —
> unlike the flush itself, this check needs nothing from step 6 to be
> meaningful, and costs nothing to build now. Both are moot today (the
> S11 D8 bridge means `flush_set` is always empty and `walk_frames` never
> finds a live compiled frame during a full GC at all) and become load-
> bearing together once step 7 removes the bridge AND step 6 gives the
> `assert!` something real to replace. (d) **New unit tests, not full
> integration goldens** (`key_klass_death_flushes`, tests_s12.md gate item
> 4, needs step 6's real flush to mean anything): `oops_do_skips_key_
> klass_reloc_when_excluded`, `update_keys_skips_key_klass_when_excluded`,
> `weak_sweep_finds_unmarked_key_klass` (`codecache::nmethod`) — each
> proving ONE piece of the filter/sweep machinery directly and cheaply,
> the same "verify the mechanism in isolation now, defer the full
> end-to-end scenario to the step that completes it" approach step 4 used
> for its own flagship test.

> **STEP-6 NOTES (as-built).** (a) **`flush_nmethod` is a free function**
> (`codecache::flush.rs`, new file), not a `CodeTable` method — same
> each_code_root-style reasoning (D4.1's own STEP-4 NOTES item (a)): it
> needs THREE of `VmState`'s own tables (`code_table`, `pic_table`,
> `code_cache`) at once, and a method receiver borrowing `self` while also
> wanting `vm` itself is the same double-mutable-borrow this sprint has
> hit before. (b) **The sweep is single-pass over every alive nmethod's
> own `ic_sites`**, NOT two passes (nmethods, then separately PICs) —
> `IcState::Pic{stub}` is resolved via `pic_table.pairs_of(stub)` right
> from the site's own iteration, so the `(nmethod, offset)` pair needed to
> reset a hit is already in hand without needing PicTable's own
> `site: (NmethodId, u32)` field the S11 draft of `PicDesc` carried "for a
> future S12 flush pass." Confirmed that pass (this one) genuinely doesn't
> need it and REMOVED the field entirely (plus `PicTable::build`'s own
> now-unused parameter, 4 call sites) rather than leaving it as permanent
> speculative dead code — matching this project's own "if you're certain
> it's unused, delete it" rule now that the future the field was reserved
> for has actually arrived. (c) **`IcState::Mega` never needs resetting on
> a flush at all** — confirmed by reading `mega.rs`'s own
> `build_mega_trampoline` directly: a mega trampoline carries the
> SELECTOR through to `rt_mega_lookup`, which re-derives its own target
> fresh on every single call (D4.4's own doc, already noted in step 3);
> it never embeds or caches a specific nmethod's entry anywhere a flush
> could leave stale. (d) **P8's "flush ordering vs. update phase" trace-
> scrape suggestion is deliberately NOT built.** The code's own control
> flow already makes the wrong order structurally impossible (flush runs
> in `full_gc`'s own sequential "phase A.5", a straight-line step before
> `forwarding_compute` is ever called — not a runtime race a trace could
> catch that the code itself doesn't already prevent), and if it somehow
> WERE wrong, `forward_chase`'s own existing `debug_assert!(is_forwarded
> ())` would panic loudly the next phase anyway (P1's own "fail at the
> source, not silently" philosophy applied one level up) — building a
> dedicated log-scrape mechanism for an ordering fact the type system and
> an existing assert already guarantee felt like manufactured busywork,
> not verification. (e) **A major, empirically-confirmed finding: `tests_
> s12.md`'s own literal `key_klass_death_flushes` scenario (a surviving
> compiled Mono caller AND an interpreter IC, alongside the dying
> customization) cannot actually observe a flush at all.** Tracing
> `CodeTable::update_keys`'s own existing, correct S11 code (`D4.1`:
> "everything else in pools stays STRONG") shows it visits a Mono site's
> own guard klass UNCONDITIONALLY, never gated by the weak-key flag —
> and `interpreter::ic::InterpreterIc::set_mono`'s own guard klass is
> just an ordinary strong heap field, marked by ordinary graph traversal
> like any other. Either one surviving means the "dying" klass gets
> marked anyway, via a path entirely independent of the customizing
> nmethod's own (correctly weak) `key_klass` field — a class can only
> actually die once EVERYTHING that ever compared against it is ALSO
> unreachable, not just the one nmethod under test. Verified by BUILDING
> the surviving-caller scenario for real
> (`compiled_mono_caller_guard_keeps_key_klass_alive`, `it_gc_jit.rs`) and
> confirming the klass (and both nmethods) demonstrably survive a real
> full GC — not just reasoned about. `key_klass_death_flushes` itself was
> narrowed to the achievable core (customization-only reference, nothing
> else alive) and passes. (f) **Building the confirming test surfaced two
> more, smaller findings**: `driver::eligible` rejects an ordinary
> (non-smi-receiver) `Send` outright — D1 point 2's own gate requires a
> site to be interpreter-IC Mono, SMALLINTEGER-GUARDED, targeting an
> `SMI_INLINE` primitive, so a ordinary dynamic dispatch like `self hot`
> against a plain object can never compile via `driver::compile_method`
> at all today; worked around (matching `it_tier1.rs`'s own established
> precedent, `mono_resolve_patches_call_site_and_dispatches`) by
> hand-building the caller as raw `IrMethod`/`Ir::CallSend`, bypassing
> `driver::eligible` entirely. Separately, the raw `CallStubFn` ABI's own
> `argc` parameter counts `argv.len()` (receiver INCLUDED) — DIFFERENT
> from `enter_compiled`'s own `argc` parameter (which excludes the
> receiver, `compiled_call.rs`'s own `0..=argc_usize` range) — the two
> compiled-entry paths are genuinely separate conventions, not one
> mistakenly documented two ways; got this wrong on the first attempt
> (passed `0` for a receiver-only call), diagnosed via a `rt_dnu`
> "subtract with overflow" abort, fixed by matching `mono_resolve_
> patches_call_site_and_dispatches`'s own already-correct `argc: 1`
> exactly rather than re-deriving it from `enter_compiled`'s different
> convention.

## Pitfalls

- **P1 — exact PcDesc match or die.** The GC-path `oopmap_at` must panic on
  a missing entry, never fall back to "nearest". A near-miss means a call
  site got emitted without a map (or the walker mis-stepped) — silent
  nearest-match converts that into heap corruption three tests later.
- **P2 — the map must reflect liveness AT the safepoint**, not slot
  oop-ness globally. A dead slot may hold a stale oop from a previous
  interval sharing the slot; tracing it retains garbage (annoying) — but a
  NON-oop interval reusing an oop slot number that the map still claims is
  fatal (random bits traced as a pointer). The builder intersects
  `slot_is_oop` with interval-live-at-position; the verifier cross-checks.
- **P3 — spill-slot addresses are fp-relative NEGATIVE** (`fp − 8·(i+1)`,
  S10 D5.2). An off-by-one reads the saved-fp/lr pair as an "oop". The
  layout consts + `rootspill_offsets_pinned`-style test guard it.
- **P4 — literal pools are data; instructions are not.** GC rewrites ONLY
  reloc-recorded 8-byte pool words. If any future emit path materializes an
  oop via movz/movk or adrp/add, GC would have to re-encode instructions +
  icache-flush mid-collection — this is forbidden (S9 P5); the oopmap
  verifier should also grep blob relocs: every `Oop`/`KeyKlassOop` reloc
  offset must be ≥ `literal_off`.
- **P5 — write-toggle scope during GC.** One `JitWriteGuard` per GC phase
  that writes pools, not one per word (the toggle is a per-thread mode
  switch — cheap, but thousands per scavenge shows up), and NEVER hold it
  across the parts of GC that execute cache code (there are none — assert
  by construction: the guard lives strictly inside `each_code_root`
  passes). Remember it is per-thread (S9 P3) — irrelevant single-threaded,
  but the flagship gate would hide a latent multi-thread bug; leave the
  depth assert on.
- **P6 — anchor discipline is now load-bearing for correctness**, not just
  traces: if a runtime path that can GC is reachable WITHOUT the anchor set
  (a Rust function called directly from another Rust function that was
  called from compiled code — fine, anchor still set from the original
  entry; the bug is a stub that clears the anchor too early), the walker
  misses every compiled frame silently. Stress mode: poison-check — on
  every allocation with `compiled_depth > 0`, assert `last_compiled_fp !=
  0`.
- **P7 — mega-map/adapter/CodeTable rehash ordering**: rehash AFTER the
  update phase rewrote the key oops (S10 P7 family). The lookup cache is
  flushed, not rehashed (SPEC §6.1).
- **P8 — flush ordering vs. update phase** (D5 step 3 before 4): updating a
  pool word inside an nmethod that is about to be freed is wasted but
  harmless; the reverse — freeing then updating — writes freed cache
  memory. Keep the order test-pinned (`flush_before_update` assertion via
  trace scrape in the key-klass test).
- **P9 — don't let the scavenger tenure the world through code roots.**
  Code roots are scanned every scavenge (v1 scans ALL alive nmethods'
  pools — no scavengeable-nmethod list yet). With `GC_STRESS=1` this is
  every allocation: O(nmethods) per alloc. Acceptable for the suite; if the
  flagship gate is intolerably slow, add the Strongtalk-style
  "nmethods-with-young-pool-refs" side list — as an OPTIMIZATION ONLY,
  behind the same visitor.
- **P10 — S11's `gc_under_compiled == 0` test must be inverted, not
  deleted** — it becomes the proof the bridge is gone (must be > 0 under
  the flagship gate).

## Interfaces for later sprints

- S13: `PcDesc.bci` + `oopmap` per safepoint (scope descs attach here);
  `walk_frames`/`FrameView` (deopt scan + frame materialization);
  `NmState::NotEntrant` + `CodeTable::flush` (zombie path);
  `oops_do_filtered` (deopt metadata oops).
- S14: `walk_frames` for the recompilation policy's caller preference;
  flush sweep reused by dependency invalidation (until the reverse index
  lands).
- S15: OSR frames must emit the same OopMap/PcDesc artifacts — the formats
  here are the contract.

## Out of scope

- Deoptimization, scope descs, not_entrant patching, dependency index,
  `MACVM_DEOPT_STRESS` — S13 (S12 flushes only on klass DEATH; method
  redefinition freshness remains the documented S11 hole until S13).
- Register oop maps, safepoints at polls, derived/interior pointers
  (`arm64.md` §6 budgets them; v1 has none: no compiled code holds an
  interior pointer across a safepoint — spill-all + no-raw-pointer IR ops
  guarantee it; assert in the oopmap verifier).
- Compacting/moving code cache; scavengeable-nmethod side list (P9, only
  if measured); weak PIC guard klasses (v1 strong — flushed only via key
  death or S13 deps).
- Green-process multi-stack walking (§11 stretch) — `walk_frames` takes the
  single process; keep its signature process-agnostic internally.
