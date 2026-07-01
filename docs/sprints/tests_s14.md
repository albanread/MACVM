# Sprint S14 — Test Plan

## Acceptance gate

Restated from SPRINTS.md S14, made checkable:

1. Entire in-language suite green under **all three stress modes combined**:
   `MACVM_JIT=threshold=1` + `MACVM_GC_STRESS=1` + `MACVM_DEOPT_STRESS=1`.
2. fib(30) and sieve ≥ **10×** interpreter (SPEC §13, tier 1 + inlining row);
   recorded in `docs/PERF.md`, gated here (one-off exception to standing
   rule 3, per SPRINTS S14 gate text).
3. `do:`/`inject:into:` steady-state loops allocate **zero Contexts**
   (asserted via GC stats — test T2 below).
4. Recompilation-level ladder visible in `-v` logs and capped: no (klass,
   selector) key ever exceeds level 4 or version 3; ineffective recompiles
   declined.
5. `just gate-s14` runs 1–4.

## Unit tests

| Test name | Module | Assertion | Rationale |
|---|---|---|---|
| `feedback_read_empty_mono_poly_mega` | compiler/feedback | hand-built interpreter IC arrays in each lattice state decode to the right `SiteFeedback` | the reader is the compiler's eyes |
| `feedback_mono_nmethod_id_target` | compiler/feedback | mono IC whose target is an nmethod-id smi resolves to the underlying MethodOop via CodeTable | SPEC §4.3 compiled_send state |
| `feedback_pic_counts` | compiler/feedback | compiled PIC with counts {A:90, B:10} → `Poly.cases[0]` = A with `count=Some(90)` | dominant-case ordering |
| `profile_hash_stability` | compiler/feedback | same klass sets, different counts → equal hash; extra klass at one site → different hash | A5 depends on exactly this equivalence |
| `inline_cost_model` | compiler/inline | accessor=2, prim wrapper=4, plain method=bc_len; table-driven over 8 methods | cost rules pinned |
| `budget_ladder` | compiler/inline | `budget_for_level(1..=4)` monotone in all three fields | level scaling sanity |
| `recursion_cap` | compiler/inline | self-recursive mono callee inlined at most once per chain | no unrolling in v1 |
| `block_bonus` | compiler/inline | site with literal-block arg accepts a callee of cost 55 at level 1 (30×2 budget); same callee rejected without block arg | the bonus is behavior, not decoration |
| `trap_veto` | compiler/inline | seed `trap_counts` over the limit for a site → `decide` returns Call/slow-path, never Trap/Inline-with-guard | S13→S14 feedback loop |
| `escape_uses` | compiler/escape | table-driven closure-use corpus: value-send-inlined→elidable; stored/arg-passed/returned/re-captured/non-value-send→escaping | Step 2's use list, case by case |
| `promotion_all_blocks_rule` | compiler/escape | temp captured by two blocks, one escaping → not promoted; both elidable → promoted | Step 3 |
| `all_or_nothing_ctx` | compiler/escape | scope with one promoted + one unpromoted temp → `CtxLoc::Materialized`, zero promotions applied | Step 4 pinned resolution |
| `pick_recompilee_caller_pref` | runtime/recompile | synthetic frame stack: hot accessor with hot caller → caller chosen; mega boundary stops the walk; cap at 10 frames | A4 walk, edge cases |
| `version_cap` | runtime/recompile | 4th recompile request for a key at version 3 → declined, counters disabled | thrash cap |
| `effectiveness_decline` | runtime/recompile | prev nmethod whose live profile hash == stored hash → declined + `counter_limit == u32::MAX`; trap-overflow override recompiles anyway | A5 both branches |

## Integration/golden tests

### T1. Inlining correctness differentials

Every test runs twice — `MACVM_JIT=off` vs `threshold=1` — with byte-equal
transcripts (SPEC §12.5):

- `inline_mono.mst` — 3-deep accessor chains, prim wrappers, a cost-capped
  large callee (stays a call; verified by `-v` log golden, see T4).
- `inline_poly_dominant.mst` — site trained 9:1 across two klasses inside a
  warmup loop, then both receivers exercised after compilation: dominant path
  inlined, minority path via slow call, results identical.
- `inline_guard_deopt.mst` — train mono on klass A, compile, then send with
  klass B: guard trap fires, deopt (S13), correct DNU-free result; next
  recompile (level 2) sees POLY feedback.
- `customization.mst` — method defined once in a superclass, invoked on 3
  concrete subklasses: assert (Rust-side hook `__vmStats`) 3 distinct
  nmethods keyed per klass; instvar access correct in each.

### T2. Context-elision zero-alloc assertion (the flagship)

**Design.** GC stats give the observable: `VmState.stats.contexts_allocated`
(bumped in the ONE allocation choke point, SPEC §7.2, when the format is
`Context`), exposed to Smalltalk via the `__vmStats` dev primitive (returns an
Array of named counters).

`world/tests/ctx_elision.mst`:

1. Warmup: run `coll inject: 0 into: [:a :e | a + e]` and
   `coll do: [:e | sum := sum + e]` loops enough times to reach a compiled,
   block-inlined level (≥ 2 × InvocationCounterLimit iterations of the caller;
   under `threshold=1` a handful suffices — the test reads thresholds via
   `__vmStats` rather than hard-coding).
2. `before := __vmStats at: #contextsAllocated`.
3. Run 10,000 more iterations of the same loops (steady state).
4. `self assert: (__vmStats at: #contextsAllocated) - before equals: 0`.
5. Control leg (proves the counter works): run an *escaping*-block loop
   (closure stored into a collection) and assert the delta is **> 0**.

Run under normal mode AND `MACVM_GC_STRESS=1` (elision must not depend on GC
quiescence). Under `MACVM_JIT=off` the test asserts delta > 0 instead
(interpreter always allocates) — the mode is read from `__vmStats`.

### T3. Elided-context deopt round trip

`ctx_elision_deopt.mst`: an inlined `do:` block whose body contains a
trained-mono send; after compilation, introduce a new receiver klass → guard
trap **inside the inlined block** → S13 materializes home + block frames,
allocating the elided Context (M6/`CtxLoc::Elided` — first real exercise).
Assert: captured temp's value visible and *mutable* after deopt (the
materialized Context is live), loop completes, transcript equals
interpreter run. Include an NLR variant: the block does `^result` after the
deopt point — must return from the materialized home frame.

### T4. Ladder & policy observability

Golden `-v` log test (filtered to stable lines): a hot method under default
thresholds shows `compile L1 → compile L2 → … capped`, caller-preference
picks visible (`recompilee=caller`), one `declined: ineffective` line from a
megamorphic-site method. Log format pinned by this golden.

### T5. Performance gate

`tests/it_perf_s14.rs` (marked `#[ignore]` except in `just gate-s14`):
fib(30) and sieve(8192, 100 passes) wall-clock, median of 5 in-process
repetitions after 2 warmups, vs `MACVM_JIT=off`. Assert ratio ≥ 10.0.
Numbers appended to `docs/PERF.md`.

## In-language tests

`world/tests/inlining.mst` — semantics-only (must pass in every mode):

- `testInlinedNlr` — `detect:ifNone:`-style NLR out of an inlined block.
- `testEnsureBlocksNotInlined` — `ensure:` inside a block chain still runs
  handlers in order (the decline rule's observable).
- `testPolyBothPaths` — dominant + minority receivers both correct post-warmup.
- `testRedefineInlinedCallee` — redefine a method that was *inlined* into a
  hot caller; assert the caller's behavior changes on its next call
  (dependency → invalidation → deopt; S13 D1 driven by S14-recorded deps).
- `testSuperSendCustomized` — super sends inside customized nmethods bind per
  holder, not per receiver klass.
- `testBlockValueOutsideHome` — closure stored then evaluated later (escaping;
  never elided): correct capture semantics.

## Stress/negative tests

- **Gate matrix**: full suite under `threshold=1 × GC_STRESS=1 ×
  DEOPT_STRESS=1` combined (the SPRINTS S14 flagship), transcripts diffed
  against `MACVM_JIT=off`.
- `deopt_storm_bounded`: a site alternating two klasses each call under a
  mono-speculating level-1 compile — assert total traps for the site ≤
  `UNCOMMON_TRAP_LIMIT + slack`, then the recompiled version stops trapping
  (poly path), and total compilations for the key ≤ 4 levels × 3 versions.
- `recompile_forever_regression`: megamorphic hot method (10 receiver
  klasses); assert compilations for its key stop (effectiveness check) while
  the program keeps running; without A5 this test times out — keep it fast
  (~seconds) so the failure mode is a clean assert, not a hang: assert
  `compilations(key) ≤ 5` after 10 × NMETHOD_COUNTER_LIMIT calls.
- `budget_ledger_exhaustion`: a caller with 50 inlinable sites — compilation
  succeeds, total inlined bytes ≤ level budget (Rust-side assert via
  compilation stats), correctness differential green.
- Negative: `escape.rs` fed a hand-built CFG where a closure flows through a
  phi/merge into a `value` send — analysis must classify **escaping** (merge
  of two creation sites is beyond v1's must-alias tracking) rather than
  mis-elide; asserted by unit test `escape_merge_conservative`.

## Non-goals

- OSR-related behavior (loop-counter compile-with-entry) — `tests_s15.md`.
- Splitting, inlining-database persistence — S19+/unplanned.
- Peak-performance tuning beyond the two gated ratios (Richards lands S15).
- Interpreter POLY IC count accuracy (SPEC-QUESTION in the detail file) —
  only the 2-case dominance rule is tested here.
- Scope-desc *format* tests — done in S13; S14 tests exercise depth-N
  behavior through T3, not re-test encoding.
