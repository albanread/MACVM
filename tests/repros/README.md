Tracked JIT-bug repros (S15 step 6 — flushed out by the Richards/DeltaBlue
ports; the benchmarks' own 4-mode gates stay red until these are fixed):

1. ic317_reosr_klass_skew.mst — BUG A (Richards blocker, VM ABORT):
   MACVM_JIT=threshold=1000 ./target/release/macvm run <file> --world world
   -> panic at interpreter/ic.rs:317 "klass k0 must still resolve selector",
   from rt_uncommon_trap (abort). Interpreted: prints "9999 10001", exit 0.
   Shape: OSR-compiled scheduler loop over 2 sibling subclasses sharing
   inherited methods, whose overrides call back into the scheduler and flip
   the loop-guard state; repeated trap-deopt -> re-OSR from nested
   interpretation (fp deepens each round) until a mono IC holds a
   (klass, compiled-id) pair whose klass no longer resolves the site's
   selector. Suspect: IC site/selector skew written somewhere in the
   re-OSR/trap-resume cycle (only writer is send.rs set_mono_compiled).

2. deltablue_projection_t1000_differential.mst — BUG C (silent wrong
   result): MACVM_JIT=threshold=1000 -> "Projection test 4 failed";
   interpreted answer 224775. Passes at threshold=100/10000/100000 —
   timing-sensitive (which loop OSRs when). chainTest alone passes at all
   thresholds; the miscompile is in the projection shape (ScaleConstraint
   execute/recalculate or the plan do: iteration).

Both also reproduce via the full benchmarks:
  MACVM_JIT=threshold=1 ./target/release/macvm run world/bench/richards.mst  --world world
  MACVM_JIT=threshold=1000 ./target/release/macvm run world/bench/deltablue.mst --world world
