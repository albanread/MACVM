Tracked JIT-bug repros (S15 step 6 — flushed out by the Richards/DeltaBlue
ports; the benchmarks' own 4-mode gates stay red until these are fixed):

1. ic317_reosr_klass_skew.mst — BUG A (Richards blocker, VM ABORT).
   **FIXED** (commit 6145c63): `ic_transition`'s mono→poly arm read
   `selector` AFTER `alloc_poly_pairs`'s own allocation without protecting
   it first — a scavenge mid-transition left it a stale pre-move address.
   Handle-protected across the allocation, then re-derived. This repro now
   passes (prints "9999 10001", exit 0) at every threshold tried.

2. deltablue_projection_t1000_differential.mst — BUG C (silent wrong
   result, STILL OPEN): MACVM_JIT=threshold=1000 -> "Projection test 4
   failed"; interpreted answer 224775. Passes at threshold=100/10000/100000
   — timing-sensitive (which loop OSRs when). chainTest alone passes at all
   thresholds; the miscompile is in the projection shape (ScaleConstraint
   execute/recalculate or the plan do: iteration). Possibly the SAME root
   cause as BUG D below (both are "a value read again well after an
   intervening branch" shapes) — not yet confirmed either way.

3. cold_branch_recompile_spill_corruption.mst — BUG D (Richards blocker,
   STILL OPEN, JIT correctness / memory corruption class): the current
   #deviceInAdd: DNU hunt's own minimized repro. `MACVM_JIT=threshold=1`
   ./target/release/macvm run <file> --world world` -> release build:
   SIGSEGV (exit 139); debug build: panics EARLIER, at
   `oops/mod.rs:53` "reserved tag: word 0x2 has the unused RESERVED_TAG",
   from `memory::roots::each_code_root` during a scavenge nested inside
   compiled `process:` (backtrace: `alloc_indexable_oops` <-
   `prim_basic_new_colon` <- `try_primitive` <- `run_method_reentrant` <-
   `rt_interpret_call`) — i.e. the GC's own root-scan finds a compiled
   frame's oop-map claiming a slot is a live oop when the raw word there
   isn't validly tagged at all. Interpreted: completes cleanly, 577783.

   Evidence chain (see the investigation this session, `MACVM_DEBUG_REGALLOC`-
   style ad hoc instrumentation, since reverted — not shipped):
     - Reproduces ONLY across a recompile-after-a-cold-sibling-branch
       transition: running the same two arms interleaved from the very
       first call (no Untaken->Mono transition ever needed) does NOT
       reproduce it — confirmed by a same-shaped variant.
     - `add2:`'s send site (the cold sibling, `ifFalse: [ data add2: 7 ]`)
       IS correctly classified `SiteFeedback::Untaken` and DOES lower to a
       proper step-3 `Ir::UncommonTrap` in BOTH the v0 AND the recompiled
       v1 nmethod (confirmed directly — not a missing-trap bug).
     - `copy_propagate` correctly resolves the trap's own recorded
       `DeoptRaw.stack` vregs back to `data`'s canonical, long-lived vreg
       (confirmed: raw `[18, 19]` -> post-copy-prop `[2, 19]`, matching
       `data`'s own wide `[2, 68]` live interval and dedicated spill slot —
       no slot collision found in the allocation itself either).
     - So the corruption is NOT in: trap emission, copy-propagation, or
       the live-interval/slot-assignment computation, as far as verified.
       Remaining suspects, in rough likelihood order: `emit.rs`'s actual
       arm64 codegen for a spill read/write specific to this multi-branch
       shape; `scopes.rs`'s LEB128 `ValueLoc`/`ScopeDesc` pack/unpack
       round-trip; or something entry/prologue-related that skips
       re-establishing `data` on a specific compiled entry path (unverified
       — not yet actually checked).
     - The GC-root panic (found via a debug build, which the earlier hunt
       hadn't tried) is likely the SAME underlying corruption caught one
       step earlier by the GC's own oop-map validation, rather than a
       second independent bug — not yet proven, but the shape (a spill
       slot's oop-map bit says "live oop", the actual bit pattern is an
       invalid raw tag) matches "wrong value ended up in this slot"
       exactly.
   Next step: `emit.rs`'s spill-slot store/load codegen for this exact
   shape, and/or a `MACVM_DEBUG_REGALLOC`-equivalent env-gated trace
   channel built properly (this session's version was ad hoc and reverted)
   so the next pass doesn't have to re-derive the vreg numbering by hand.

All three also reproduce via the full benchmarks:
  MACVM_JIT=threshold=1 ./target/release/macvm run world/bench/richards.mst  --world world
  MACVM_JIT=threshold=1000 ./target/release/macvm run world/bench/deltablue.mst --world world
