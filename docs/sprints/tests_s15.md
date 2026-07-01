# Sprint S15 — Test Plan

## Acceptance gate

Restated from SPRINTS.md S15, made checkable:

1. **No-method-reentry OSR proof** (T1): a single-invocation long-running
   loop reaches compiled speed; `osr_entries == 1` and zero additional
   invocations of the method occur.
2. Richards ≥ **5×** interpreter (SPEC §13); Richards and DeltaBlue validate
   their canonical check values in every run.
3. No regression in any stress suite: full matrix
   (`threshold=1 × GC_STRESS ∈ {off,1} × DEOPT_STRESS ∈ {off,1}`) green,
   transcripts equal to `MACVM_JIT=off`.
4. `docs/PERF.md` exists with the SPEC §13 table measured per procedure A7.
5. `just gate-s15` runs 1–4.

## Unit tests

| Test name | Module | Assertion | Rationale |
|---|---|---|---|
| `osrmap_roundtrip` | compiler/scopes | `OsrMap` with all four `OsrSource` variants + negative `dst_frame_off` packs/unpacks identically | format contract |
| `osr_bci_is_backedge_target` | compiler | pipeline with `osr_bci` not a `jump_back` target → compile-time error (Rust `Err`, not panic) | A1's legality is enforced, not assumed |
| `osr_root_scope_only` | compiler | `osr_bci` resolving into an inlined scope's bci range → error | A2 step 2 |
| `osr_ctx_elision_disabled` | compiler | has_ctx method compiled with `osr_bci`: root scope's `CtxLoc` is `Materialized`, never `Elided`; same method without `osr_bci` elides | A2 step 3, both directions |
| `osr_map_covers_liveins` | compiler | for a corpus of 5 loop methods: every live-in entity of the header block (per the abstract stack model + liveness) appears exactly once in `slots`; dead temps absent | conversion completeness |
| `osr_table_lookup_install` | codecache | install/lookup keyed (klass, sel, bci); rebuild-after-GC (klass moved by full GC) still resolves | oop-keyed side-table discipline |
| `counter_reset_on_declined` | runtime/osr | Declined outcome resets the method's loop counter | the quadratic-slowdown pitfall |
| `stats_counters_monotone` | runtime | each new stats counter increments where designed (table-driven: provoke, read, assert delta) | observability is a deliverable |
| `pack_window_no_alloc` | runtime/osr | debug no-alloc-scope assert fires if a test hook allocates inside the pack→enter window | the GC hole guard itself is tested |

## Integration/golden tests

### T1. No-method-reentry OSR proof (the flagship)

**Design.** Prove the hot loop entered compiled code *within its single
activation*, not via re-invocation.

`tests/golden/osr_reentry.mst`:

1. `Spin>>runOnce` contains `| s | s := 0. 1 to: 5000000 do: [:i |
   s := s + (i bitAnd: 7)]. ^s` — send-light so only OSR (not S14 caller
   recompilation of the harness) can compile the loop's activation.
2. Top level calls `Spin new runOnce` **exactly once** under default
   thresholds (`MACVM_JIT` unset) and prints the sum plus a stats probe.
3. Assertions via `__vmStats` printed into the golden transcript:
   - `osr_entries = 1`, `osr_declined = 0`;
   - invocation-counter compilations of `runOnce` = 0 (its `counter` never hit
     10,000 — it was invoked once): checked as
     `compilations_by_level` total for the key equals the 1 OSR compile
     (Rust-side integration assert via a test hook keyed on the nmethod);
   - the loop result is exact (correctness).
4. **Speed leg** (Rust integration test, `#[ignore]` outside the gate): time
   `runOnce` under `MACVM_JIT=off` vs default; assert default ≥ 5× — compiled
   speed was actually *reached mid-activation* (with only ~10k of 5M
   iterations interpreted, anything less than a large multiple means OSR
   didn't transfer).
5. Debug-build leg: `bytecodes_interpreted` delta during the second half of
   the run is ~0 (sampled counter; assert < 1% of first-half delta).

### T2. OSR correctness differentials

Each runs `MACVM_JIT=off` vs default vs `threshold=1`, byte-equal transcripts:

- `osr_basic.mst` — accumulating loop with temps of mixed kinds (smi, Array,
  String) live across the backedge, plus two values live on the operand
  stack at the header (`to:do:` desugaring keeps limit/index live) — exercises
  `Slot`, `StackSlot`, `Receiver` sources.
- `osr_ctx.mst` — has_ctx method: loop body's block captures and *mutates* a
  temp (`coll do: [:e | s := s + e]` shape where `s` is context-allocated
  because an escaping block also captures it); after OSR the closure and the
  compiled code share the one materialized Context — final value correct.
- `osr_nested_sends.mst` — loop body full of mono sends (S14 inlining active
  inside the OSR nmethod): result equality + `contexts_allocated` steady-state
  delta 0 for the inlined-block variant (S14's T2 assertion holds under OSR).

### T3. OSR frame deopting back (interaction test)

`osr_then_deopt.mst`: loop trained mono; at iteration `k` past the OSR point
the receiver klass changes → guard trap **inside the OSR frame** → S13
materializes mid-loop → the rest runs interpreted → result exact and
transcript equals `MACVM_JIT=off`. Assert `osr_entries = 1` and
`deopt_count ≥ 1`. Variant: redefine the inlined callee mid-loop instead
(invalidation path through the loop poll — S13 D1d) — same equality.
Both variants also run under `GC_STRESS=1` (materialization allocates while
the OSR frame is the walk root).

### T4. Benchmarks as correctness tests

- `richards.mst` golden: one run, transcript asserts
  `queuePacketCount = 23246` and `holdCount = 9297` — run in all three JIT
  modes (the check values are mode-independent).
- `deltablue.mst` golden: `chainTest`/`projectionTest` internal asserts pass;
  transcript prints `deltablue ok`.
- Both under the full stress matrix **with reduced inner counts** (a
  `Bench stressScale` knob) so the gate stays fast.

### T5. Performance recording

`just perf` produces/updates `docs/PERF.md`; `tests/it_perf_s15.rs`
(gate-only) asserts: Richards ratio ≥ 5.0; fib/sieve still ≥ 10.0 (S14
non-regression); scavenge p95 < 1 ms and full-GC < 50 ms at the standard
churn workload (SPEC §13 rows), reading the pause stats counters.

## In-language tests

`world/tests/osr.mst` (semantics in every mode):

- `testLoopResultAcrossOsr` — checksum loop long enough to trigger OSR at
  default thresholds; exact result.
- `testNlrFromOsrLoop` — `detect:`-style early `^` out of the hot loop after
  OSR: NLR from a compiled OSR frame unwinds correctly.
- `testEnsureAroundOsrLoop` — `[hot loop] ensure: [marker]`: handler runs
  exactly once whether the loop OSRs, deopts, or both.
- `testTwoHotLoopsSequential` — two loops in one method; only the first is
  the OSR entry (v1 single-entry); second loop still correct (runs compiled
  via the same nmethod's normal flow).
- `testOsrDeclinedStillCorrect` — version-cap a method via repeated
  redefinition, then run its hot loop: OSR declines, loop completes
  interpreted, result exact.

## Stress/negative tests

- **Full matrix** (gate item 3) including the S13/S14 suites — OSR must not
  regress any earlier stress result.
- `osr_under_gc_stress`: T2's `osr_basic` with `GC_STRESS=1` — a scavenge on
  every allocation brackets the pack→enter window; the no-alloc assert plus
  transcript equality catch window violations deterministically.
- `osr_deopt_osr_cycle`: loop that OSRs, trap-deopts, continues interpreted,
  re-triggers the loop counter, OSRs again (same nmethod, still Alive since
  the trap count is low): assert `osr_entries = 2`, result exact, nested
  `interpret_active` depth bounded.
- `declined_no_thrash`: version-capped hot loop of 10M iterations completes
  in bounded time with `osr_declined` growing only ~(iterations /
  LoopCounterLimit) — the counter-reset pitfall's regression test.
- Negative (Rust): corrupt an `OsrMap` blob (truncate) — decoder errors with
  nmethod id; `rt_osr_request` on a method with no OSR nmethod and a failing
  compile (inject compiler error via test hook) → `Declined`, interpreter
  unharmed.

## Non-goals

- Optimized→optimized on-stack replacement (Self `replaceOnStack`) — S19
  stretch; no tests here.
- Multi-entry OSR / nested-loop entry selection — out of scope in v1 (the
  `testTwoHotLoopsSequential` case documents the accepted behavior instead).
- Absolute performance beyond the SPEC §13 rows (DeltaBlue has no gated
  ratio — tracking only, recorded in PERF.md).
- Profiling tooling itself (Instruments/samply are guidance, not tested).
- Green-process interaction with OSR — S17's tests.
