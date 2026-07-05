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
   #deviceInAdd: DNU hunt's own minimized repro. `MACVM_JIT=threshold=1
   ./target/release/macvm run <file> --world world` -> release build:
   SIGSEGV (exit 139); debug build: panics EARLIER, at
   `oops/mod.rs:53` "reserved tag: word 0x2 has the unused RESERVED_TAG",
   from `memory::roots::each_code_root` during a scavenge nested inside
   compiled code (backtrace: `alloc_indexable_oops` <-
   `prim_basic_new_colon` <- `try_primitive` <- `run_method_reentrant` <-
   `rt_interpret_call`) — i.e. the GC's own root-scan finds a compiled
   frame's oop-map claiming a slot is a live oop when the raw word there
   isn't validly tagged at all. Interpreted: completes cleanly, 577783.

   **The corrupted frame belongs to `R3 class>>go` (the outer driver with
   the two `1 to: 50000 do:` loops) — NOT `process:`.** Its own two
   selector calls (line 46/47) are separate call sites, one per loop, each
   hardcoded to a different `kind` constant — so the SECOND loop's own
   `#process:` call site is itself entirely untaken until the first loop's
   50000 iterations finish, the same "recompile while a sibling site is
   cold" shape one level up. `go` recompiles (nmethod 4 -> 52) very early
   (within the first loop), so by the time the second loop's own call site
   finally fires for the first time ever, it does so inside a nmethod
   compiled without ever having seen it.

   Evidence chain (ad hoc `MACVM_DEBUG_REGALLOC`/`MACVM_DEBUG_MATERIALIZE`
   env-gated instrumentation, added then reverted each pass — not shipped;
   re-add at the cited call sites to reproduce this trail):
     - Confirmed via a same-shaped interleaved variant: the bug needs a
       recompile-while-a-sibling-branch-is-cold transition — running both
       arms from the very first call never reproduces it.
     - Traced to `nmethod 52` (`go`'s own recompiled version) specifically,
       via a `[code-root] nm=... slot=...` dump added at `memory::roots::
       each_code_root`'s own `Oop::from_raw` call: the bad word is at
       `go`'s SpillSlot(12), not anywhere in `process:`'s frame.
     - `go`'s own bci=102 (the second loop's `#process:`/`#size` call
       site) IS correctly classified `Untaken` and traps correctly, in
       BOTH nmethod 4 and 52 (confirmed by instrumenting `ir::convert`'s
       feedback-read and step-3 trap-emission sites directly) — ruling out
       a missing-trap bug at that level.
     - `copy_propagate` correctly resolves this trap's own recorded
       `DeoptRaw.stack` back through to the SAME long-lived vreg (12) both
       before and after propagation.
     - **Vreg 12's own computed live interval, `[50, 71]`, DOES cover
       position 58 — the bci=102 trap's own linearized position** (added a
       `block_start_pos`/`block_end_pos` dump to confirm this directly).
       This DISPROVES the "trap position isn't covered by the interval"
       hypothesis an earlier pass in this same investigation had reached
       for `process:`'s own shape — that theory does not hold once the
       actual culprit method (`go`, not `process:`) is identified. Do not
       re-assume it without re-verifying against `go`'s own data.
     - So as of this pass: the interval computation, trap emission, and
       copy-propagation are ALL independently confirmed correct for the
       one site directly implicated by the recorded deopt metadata. The
       corruption must be introduced somewhere ELSE — most likely a LATER
       call-site inside the loop (the `#size`/`#queuePacket:`-equivalent
       calls after `#process:` returns) whose own oop-map, built from the
       SAME interval data, ends up marking SpillSlot(12) live when the
       actual compiled code no longer maintains a valid value there, or a
       genuine emit.rs codegen bug in the spill write/read sequence itself
       (not yet directly inspected — the next concrete step is dumping
       `nmethod 52`'s actual machine code, e.g. via `CodeBlob.listing` in a
       debug build, and reading the instructions around every call site
       after the second loop's own `#process:` send).
   Next step: get `go`'s (nmethod 52) real disassembled listing and read
   the actual spill instructions around each of its post-loop-entry call
   sites, rather than continuing to infer from IR-level dumps alone — the
   IR/regalloc layer has now been checked as thoroughly as static+dynamic
   dumps allow without reading emitted machine code directly.

All three also reproduce via the full benchmarks:
  MACVM_JIT=threshold=1 ./target/release/macvm run world/bench/richards.mst  --world world
  MACVM_JIT=threshold=1000 ./target/release/macvm run world/bench/deltablue.mst --world world
