# MACVM interpreter throughput — S6 baseline

Recorded per `sprint_s06_detail.md` §Benchmarks' procedure (SPRINTS
standing rule 3: **tracking, not gating** — these numbers are not part of
any test's pass/fail criteria).

## Environment

- Host: Apple M4, 10 cores, macOS (Darwin 25.5.0, arm64)
- Build: `cargo build --release` (rustc 1.96.0)
- Date: 2026-07-02

## Procedure

`MACVM_TRACE=count` (prints total bytecodes dispatched at exit) plus
`/usr/bin/time -p` (wall clock) around each of 5 runs per benchmark;
`world/bench/fib.mst` (fib 25), a 30-variant of the same script, and
`world/bench/sieve.mst` (10 iterations, size 8190, expected count 1899).
Bytecode counts were byte-for-byte identical across all 5 runs of every
benchmark (determinism confirmed, per the procedure's requirement).

## Results

| Benchmark | Result | Bytecodes (median = all 5) | Median wall (traced) | bc/s (traced) |
|---|---|---|---|---|
| fib(25) | 75025 | 2,677,644 | 0.08 s | ~33.5M bc/s |
| fib(30) | 832040 | 29,625,095 | 0.90 s | ~32.9M bc/s |
| sieve ×10 | 1899 | 5,672,753 | 0.17 s | ~33.4M bc/s |

fib(30) wall time (0.90 s traced) is well under SPEC §13's `< 2 s` gate.

## `MACVM_TRACE=count` overhead

The three benchmarks above cluster tightly around **~33M bc/s with the
counter enabled** — noticeably below SPEC §13 row 1's 50M bc/s target.
Re-running fib(30) *without* `MACVM_TRACE=count` gives a median wall time
of **0.55 s** for the same 29,625,095 bytecodes: **~53.9M bc/s**, above
target. The counter itself (`sprint_s06_detail.md`'s own estimate: "cost ≈
1 add/dispatch, acceptable") measurably costs more than that in practice
on this build — a ~40% slowdown, not the "1 add" the doc's estimate
assumed. This is worth another look in a later throughput-focused sprint
(S10/S14/S15 per the SPRINTS doc), but is out of scope here: S6 is a
library sprint, not an interpreter-optimization one.

## Pass/fail against SPEC §13 row 1 (tracking only)

- fib(30) < 2 s: **PASS** (0.55 s untraced, 0.90 s traced — both well
  under).
- ≥ 50M bc/s: **PASS untraced** (~53.9M bc/s), **FAIL traced** (~33M
  bc/s) — the gap is attributable to the counter overhead noted above,
  not to per-bytecode dispatch cost. Since the procedure as written
  measures wall time *with* the counter active, the honest reading of
  this baseline is "fails as measured, passes with the counter removed
  from the hot path" — recorded for whoever picks up interpreter
  throughput work later.

# S10 tier-1 JIT — perf marker

Recorded per `tests_s10.md`'s "Perf marker" procedure (SPRINTS standing
rule 3: **tracking, not gating**). `world/bench/arith.mst`'s
`sumTo: 5_000_000` — a send-free, once-compiled smi arithmetic kernel
(`SmiArith Add`, the inlined `to:do:`'s `SmiCmpBr`, `Poll` at the loop
back-edge) — timed via `millisecondClock` after two small warm-up calls
through the same call site (so the compile itself never lands inside the
timed window), under `MACVM_JIT=off` vs `MACVM_JIT=threshold=1`, via
`just bench-s10` (`--release`). The gate WARNS below 5x and FAILS only
below 2x (an architectural-mistake tripwire, not a perf gate — gate item
3 of tests_s10.md's acceptance gate).

| Date | Commit | interp_ms | jit_ms | ratio |
|---|---|---|---|---|
| 2026-07-03 | 177abf1 | 1221 | 9 | 135.66x |
| 2026-07-03 | 353db27 | 1233 | 10 | 123.30x |

# S11 D8 bridge — allocation cost of the pre-S12 GC bridge

Recorded per `tests_s11.md`'s "Bridge accounting" stress/negative test
(SPRINTS standing rule 3: **tracking, not gating** for `bridge_old_allocs`
itself — `gc_under_compiled` IS gating: `just bridge-stats-s11` fails the
run if it's ever nonzero). The full world test suite, under
`MACVM_GC_STRESS=full:64` combined with `MACVM_JIT=threshold=1` (the same
combination `gate-s11` stress-tests), traced with `MACVM_TRACE=gc`.
`bridge_old_allocs` is every allocation the D8 bridge diverted old-direct
because a compiled frame was live (`compiled_depth > 0`) — non-moving,
so it costs old-gen space no scavenge can ever reclaim until S12 deletes
the whole bridge. `gc_under_compiled` is the number of times a
scavenge/full-GC actually ran while `compiled_depth > 0` — i.e. the
bridge failing to hold; must always read 0.

| Date | Commit | bridge_old_allocs | gc_under_compiled |
|---|---|---|---|
| 2026-07-04 | 7ac7b53 | 110 | 0 |

# S11 dispatch — perf marker (adapted, see world/bench/dispatch.mst's own doc)

Recorded per `tests_s11.md`'s gate item 4 ("Dispatch micro-benchmark"),
ADAPTED: the literal 3-class polymorphic design that file sketches cannot
compile at all under S11's as-built eligibility gate (`mono_smi_inline_send`
rejects any non-super send whose IC guard isn't `SmallInteger`, monomorphic
or not — see `world/bench/dispatch.mst`'s own header and
`sprint_s11_detail.md`'s STEP-10 NOTES for the full reasoning). This instead
times `world/bench/dispatch.mst`'s `runLoop: 5_000_000` — arith.mst's own
`sumTo:` shape with its inlined `+` replaced by a REAL super-send dispatch
per iteration (D4.6: the one non-arithmetic, non-`basicNew` send a compiled
method may contain) — under `MACVM_JIT=off` vs `threshold=1`, via
`just bench-s11` (`--release`). Same warn<5x/fail<2x tripwire as
`bench-s10` (tracking, not gating).

A smaller ratio than `bench-s10`'s ~130x is the EXPECTED, honest result: a
real send still costs a real dispatch even compiled (unlike inlined
arithmetic, which erases the cost entirely) — this benchmark measures that
cost, it doesn't erase it.

| Date | Commit | interp_ms | jit_ms | ratio |
|---|---|---|---|---|
| 2026-07-04 | 7ac7b53 | 1834 | 472 | 3.88x |
| 2026-07-04 | abe4f2e | 110 | 0 |
| 2026-07-04 | cdfab6a | 110 | 0 |
| 2026-07-04 | a1e57ac | 110 | 0 |
| 2026-07-04 | 04e774b | (bridge deleted) | 110 |

# S15 A6/A7 — Richards/DeltaBlue perf recording

Recorded per `tests_s15.md` T5's procedure: `world/bench/bench.list`
(`richards.mst`, `deltablue.mst`) run through the shared `Bench.mst`
harness (3 discarded warmups + median-of-outer, timed via
`millisecondClock`, excludes genesis/world load) under `MACVM_JIT=off` vs
`threshold=1` vs `threshold=1000`, via `scripts/perf.sh --release`.

## 2026-07-06 (commit f62a1e4) — Richards t=1 CORRECT for the first time

BUG D root cause 4 turned out to be two distinct bugs (OSR uninit frame
slots + c2i >5-arg marshaling overflow — see tests/repros/README.md and
f62a1e4's own message), both fixed. Richards now completes CORRECTLY
under `threshold=1` (23246/9297 golden values) — previously it could not
complete under any JIT threshold at all.

| Benchmark | interp_ms | jit t=1 | jit t=1000 | best/interp |
|---|---|---|---|---|
| richards | 204 | 193 | **blocked** (mid-threshold wrong-answer) | 1.1x (t=1) |
| deltablue | 208 | 119 | **blocked** (BUG C) | 1.7x (t=1) |

- **richards t=1 = 1.1x is a PERF gap now, not a correctness gap**: the
  run is correct but trap-heavy (the `work kind` two-way branch keeps one
  arm trapping — 60k+ uncommon traps observed per run), so most of the
  win is eaten by deopt/reexecute churn plus interpreted fallback for the
  7-arg-send creator methods the new eligibility cap declines. T5's
  "Richards ratio ≥ 5.0" gate therefore remains unmet — but the remaining
  work is optimization (trap-site healing / poly-arm compilation), not
  bug-fixing. `it_perf_s15.rs` still should not be written yet: it would
  fail on day one for perf reasons.
- **The mid-threshold silent wrong-answer (Richards t=100..20000,
  DeltaBlue t=1000 — very likely BUG C's own band)**: results are correct
  interpreted, at t≤10, and at t≥100000 (OSR-only compilation), but wrong
  whenever invocation-triggered compilation lands MID-run — the
  early-exit fraction tracks the threshold precisely (at t=20000 the
  scheduler dies ~92% through, exactly where `queuePacket:` crosses
  20000 invocations). Documented as the next investigation; blocks only
  the t=1000 columns above.
- Also known, pre-existing (stash-bisected, not from these fixes): the
  BUG D repro fails under `MACVM_GC_STRESS=1` + `threshold=1`
  (`doesNotUnderstand: size`), while passing under `MACVM_DEOPT_STRESS`.

## 2026-07-06 regression A/B: the f62a1e4 fixes are perf-neutral

Question asked and answered with a true A/B (release builds of HEAD vs
d65d1dd — the commit immediately before the fixes — same machine, runs
interleaved): did the root-cause-4 fixes cost performance?

| Benchmark | Mode | pre-fix (d65d1dd) | post-fix (HEAD) |
|---|---|---|---|
| arith | off / t=1 / t=1000 | 1253 / 6 / 9 ms | 1262 / 6 / 9 ms |
| dispatch | off / t=1 / t=1000 | 1844 / 26 / 11 ms | 1838 / 26 / 11 ms |
| sieve | off | 85 ms | 85 ms |
| sieve | t=1 / t=1000 | **SIGSEGV (exit 139)** | 84 / 85 ms, correct |
| richards | off / t=1 | 204 / DNU abort | 205 / 194 ms correct |
| deltablue | off / t=1 | 209 / 119 ms | 209 / 120 ms |

Identical to the millisecond on every benchmark that ran before — which
also directly measures the only hot-path cost the fixes added (the two
extra register-pair spills per runtime-stub call: no observable effect).
And the A/B surfaced something the record didn't yet know: the PRE-fix
build SIGSEGVs on sieve under BOTH JIT thresholds in release — the OSR
uninitialized-slot bug again, cured by the same fix. Net: nothing slower,
two benchmarks (sieve JIT, richards t=1) went from crashing to correct.

# Dual-arm branch storm — sieve is the canonical repro (2026-07-08)

Representative-benchmark sweep after the primitive-shim work (release,
`--world world`; richards/deltablue self-timed, fib/factorial self-timed
via `millisecondClock`, arith/dispatch process-timed incl. ~10 ms boot):

| Benchmark | interp (off) | jit t=1 | speedup | shape |
|---|---|---|---|---|
| arith (`sumTo: 5M`) | 1280 ms | ~10 ms | >100x | tight fused smi loop |
| fib(32) | 1637 ms | 15 ms | ~109x | recursion + dispatch + fused arith |
| factorial 20! x200k | 6989 ms | 890 ms | ~7.9x | smi `*` + overflow→LargeInteger |
| factorial 500! x300 | 72325 ms | 4386 ms | ~16.5x | bignum multiply + allocation |
| deltablue (10x10) | 213 ms | 113 ms | ~1.9x | constraint solver |
| dispatch | 1870 ms | ~20 ms | ~90x | send/IC dispatch |
| **sieve (8190 x10)** | **87 ms** | **88 ms** | **~1.0x** | **dual-arm branch storm** |

**CORRECTION (2026-07-08, via `MACVM_TRACE=deopt`/`MACVM_DBG_REEXEC`/
`MACVM_DBG_IR` — an earlier draft of this entry crowned sieve the "dual-arm"
repro; the debugger overturned that. Recorded honestly.):**

- **Sieve is NOT a balanced-branch storm.** Its `threshold=1` flatness (65
  deopts) is a *compile-cold* artifact: at `threshold=1` the method compiles
  before its loop body has run, so its send ICs are all `Empty`, and the
  compiler lowers `Empty`-IC sends to `Untaken → UncommonTrap` (dead-code
  speculation) — then the loop runs and they all trap. Across the threshold
  sweep sieve is flat at EVERY setting (94-96 ms) and at `threshold=2000`
  has only **30 deopts** — no storm. Its high-threshold flatness is
  compile-timing on a short (~95 ms) workload, not speculation. Not the
  repro we thought.

- **The real speculation storm is Richards**, and it is NOT in
  `processWork:` (weekend_work.md Gap 1's guess, also eyeballed) and NOT a
  balanced boolean branch. The debugger pins it to **`addInput:checkPriority:`
  bci=21** — a `GuardKlass { obj, expect } fail → UncommonTrap` (block1
  @bci8 → block10 @bci21 in the warm IR). It is a **receiver-klass guard on
  a mono-inlined send** (S14 step 4b/5): the compiler inlined a
  `priority`/`packetPending:` accessor betting one Task subclass, but
  Richards runs four (Idle/Worker/Handler/Device) through this method, so the
  guard fails ~half the calls. **160,555 of 160,674 deopts** are this one
  site, and it is **threshold-independent** (826k at t=1, 160k at t=2000 —
  the steady-state storm survives full warmup because the site is genuinely
  polymorphic, not cold).

- **Richards is ~2.4×, not 1.1×.** The "1.1×" on record was measured at
  `threshold=1` (the cold-compile worst case, 826k deopts). Warmed up
  realistically (`threshold=2000`): **off 207 ms vs 85 ms = 2.4×**, deopts
  160k. The benchmark harness's `threshold=1` convention systematically
  understates the JIT. (Threshold sweep: sieve 94/95/96/95 ms at
  off/1/100/2000; richards 207/191/91/85 ms.)

- **The fix** is still the "detect an over-deopting speculation site, then
  de-speculate" shape, but "de-speculate" here = **stop mono-inlining that
  send; dispatch it polymorphically** (a real send, or S14 step 6's existing
  `DominantWithSlowPath`), NOT "compile both branch arms." Gate on Richards
  `addInput:checkPriority:` (deopts at bci=21 → ~0). Open puzzle: it already
  recompiles 18× (nm 38/41/42) and still storms — the recompiler isn't
  switching this site to poly; that is the thing to fix.

## RESOLVED (2026-07-09, commit a2bfd8b): the IC stomp in activate_method

The "open puzzle" above cracked the case: the recompiler wasn't switching
the site to poly because the IC never STAYED poly. `interpreter::send::
activate_method`'s over-threshold path unconditionally rewrote the caller's
IC to Mono-compiled(current receiver klass) on every dispatch —
`ic_transition` would upgrade Mono(A)→Poly[A,B] and the very next
over-threshold dispatch stomped it back to Mono(B). The IC ping-ponged
between Mono states forever: `snapshot_profile`'s tag-only hash never
changed (8,501 "profile unchanged" declines, 0 recompiles in one warm run),
and each customized compile baked whichever klass was last stomped in as a
mono-inline KlassGuard whose fail-edge trap then fired on ~every other
call. Proven with the in-tree debugger: at every reexecution the receiver's
klass EQUALED the live IC guard (the interpreter never missed!) while the
baked pool word held a different klass — a Mono→Mono re-key that
`ic_transition` cannot produce; the only writer capable was the stomp.

Fix (one gated seed in `activate_method`): seed the IC only from Empty or
same-klass Mono; never downgrade Poly/Mega or re-key a different-klass
Mono. The preserved Poly tag lets the EXISTING recompile machinery re-lower
the send (DominantWithSlowPath / plain Call) — no new mechanism needed.

| Benchmark | interp (off) | jit t=1 | jit t=2000 | best/interp |
|---|---|---|---|---|
| richards (before) | 208 | 191 (826k deopts) | 85 (160,674 deopts) | 2.4x |
| **richards (after)** | 208 | **18** (30k deopts, 58 recompiles) | **13** (**2 deopts**, 1 recompile) | **16x** |
| deltablue (before) | 208 | 113 | — | 1.9x |
| **deltablue (after)** | 208 | — | **62** | **3.4x** |
| sieve | 88 | — | 93 (count 1899 correct) | ~1.0x (separate: threshold=1 cold-compile artifact + short workload) |

Correctness: Bench's own checkResult (error: on mismatch) passed on every
run; full test suite green (19 binaries); stress matrix over 4,609 world
tests × {GC_STRESS=1, GC_STRESS=full:64, DEOPT_STRESS=64} × threshold=1 —
0 failures. The S15 T5 gate ("richards ≥ 5.0") is now PASSED at 16x.

## 2026-07-09 (S24 A1, commits 979daf0..db378fa) — compiled closures land

First slice of closure compilation: standalone block bodies compile
(`by_block` registry, closure calling convention, compiled-block NLR
origination, root-is_block deopt materializer). Numbers via
`scripts/perf.sh --release` (t=1/t=1000 columns) on the A1 code:

| benchmark | interp (ms) | jit t=1 | jit t=1000 | best/interp |
|---|---|---|---|---|
| richards | 218 | 19 | 13 | 16.8x |
| deltablue | 229 | 43 | 55 | **5.3x** |

- **DeltaBlue gate (>=5.0x) PASSED already at A1** — the design expected
  this to need A2/A3. Block-iteration backbone (do:/detect:-style bodies)
  no longer interprets. Was 3.4x at the IC-stomp fix (a2bfd8b).
- richards 16.8x: the >=16x no-regression gate holds (blocks there are
  the S14-ELIDED kind; A1 must not and did not perturb the splices).
- arith 1447 -> 11ms (131x), fib/sieve unchanged — gate 3 noise band.
- Interpreted tail (MACVM_TRACE=count, deltablue warm t=2000 vs off):
  14,316,872 / 102,901,203 = **0.139** (pre-A1 doc baseline 0.163;
  gate 2 target <0.10 is A3's job). The NEW per-method attribution
  (`bytecodes-by-method:` lines) shows the residue is entirely the
  A3-target creator methods (constraintsConsuming:do:, makePlan:, ...)
  — ZERO `[block]` entries in the top 40: A1 did exactly its share.
- Correctness: world suite byte-identical vs interpreter at t=1 AND
  t=200, plain and under {GC_STRESS=1, GC_STRESS=full:64,
  DEOPT_STRESS=64}; deltablue+richards under the same matrix at t=200
  all green. Two real bugs found by the benchmark half of the matrix:
  the PIC duplicate-klass GC corruption (FIXED, db378fa — also the true
  cause of the "cache exhaustion" abort) and the t=1-only stale-slot
  (task #125, pre-existing, full dossier in tests/repros/README.md
  entry 9).
- Measurement policy note: threshold=1 stays the DIFFERENTIAL oracle
  (compile everything, compare bytes); it is NOT a perf configuration —
  cold compiles get no feedback-driven code. Stress and perf runs now
  also gate on threshold=200 (metric-driven compiles), release builds,
  all modes in parallel.

## 2026-07-09 (S24 A2, commit 0401df7) — direct value-family dispatch

Compiled `value`-family sends now tail-jump straight to the block nmethod
via a shared per-argc dispatch stub (`by_block` probe), replacing the c2i
adapter + nested interpreter activation. `scripts/perf.sh --release`:

| benchmark | interp (ms) | jit t=1 | jit t=1000 | best/interp |
|---|---|---|---|---|
| richards | 204 | 19 | 12 | 17.0x |
| deltablue | 213 | 34 | 41 | **6.3x** |

- **DeltaBlue 5.3x -> 6.3x** — the warm run dropped 43->34ms purely from
  the value-dispatch fast path. `MACVM_TRACE=stats` on deltablue t=200:
  `value_dispatch_hits=1741710 value_dispatch_fallbacks=1516` — 99.9% of
  `value:` sends in compiled methods tail-jump; the 1516 fallbacks are cold
  warmup before each block compiles. (My pre-implementation caution that A2
  might barely fire before A3 was wrong: deltablue's compiled constraint
  methods send `value:` heavily.)
- richards ~17x (noise vs A1's 16.8x — its blocks are the S14-elided kind,
  few standalone `value:` sites); arith/fib/sieve unchanged.
- Interpreted tail UNCHANGED from A1's 0.139: A2 changes how compiled
  `value:` sites dispatch, not which methods compile. A3 (compiling the
  closure-creating orchestrators) is what closes the tail toward <10%.
- Correctness: world byte-identical vs interpreter at t=1 AND t=200, plain
  and under {GC_STRESS=1, DEOPT_STRESS=64}; benches x 3 stress modes x
  t=200 all green; cargo test 833/0.

## S24 A3 — compiled closure creation (A3a escaping non-ctx, A3b materialize Context)

2026-07-10, commits 96faa0a (A3a), fb01b7a (A3b), 70b5513 + 9de470b (review
remediation). Release, MacBook arm64, `world/bench/*` via `Bench run:`.

| benchmark | interp (ms) | jit t=200 | jit t=1000 | best/interp |
|---|---|---|---|---|
| richards | 204 | 20 | 13 | 15.7x (held) |
| deltablue | 214 | 33 | 33 | **6.5x** |

- **deltablue 6.3x -> 6.5x** and, far more important, the A3b tail methods
  (constraintsConsuming:do:, addConstraintsConsuming:to:, printOn:) now
  COMPILE — closure-creating methods with captured temps get a real
  materialized Context. T5 >=5.0 gate: PASSED.
- **The benchmark run was the detector for two release-observable bugs the
  world differential missed** (both fixed, see tests/repros/):
  1. `rt_alloc_slow` still enforced D7's Slots-only contract and allocated
     `nis` words ignoring the site size — a Closure/Context Alloc overflowing
     eden returned a too-short object and the continuation corrupted the
     neighbor (deltablue DNU #value: under real allocation pressure; every
     small repro stayed on the inline fast path). Latent since A3a.
  2. a has_ctx method whose FIRST bytecode is a loop header re-ran the
     block-0 prologue every iteration (per-iteration Context snapshots vs the
     interpreter's ONE shared Context — silent wrong answers). Now declined.
- Remaining deltablue tail at t=200: 8.9M interpreted bytecodes, led by
  ScaleConstraint>>execute (39.8%) + recalculate (13.0%) — the next
  eligibility targets (B-phase).
- Correctness: world byte-identical off vs t=200 (release), plain and under
  {GC_STRESS=1, GC_STRESS=full:64, DEOPT_STRESS=64}; loop-header +
  tiny-eden repros green under DEOPT_STRESS/GC_STRESS; 633 lib + full
  integration suites green.
- Process note: fb01b7a's post-commit code review (8 finder angles) filed 10
  findings; the two live ones were fixed same-day and the CtxLoc::None
  bci-fingerprint (three finders converged) was re-keyed to ctx-vreg
  liveness before it could bite under organic NotEntrant deopts.

## S24 B-phase L1 — stale PIC-c2i heal (e3a3f00)

2026-07-10. The B-phase understand pass (3-reader + adversarial-verify
workflow) found the deltablue tail was a DISPATCH FREEZE, not eligibility: PIC
pairs baked (klass -> c2i) before the callee compiled never upgrade. One lazy
re-key arm in rt_interpret_call's upgrade hook:

| benchmark | interp (ms) | jit t=200 | jit t=1000 | best/interp |
|---|---|---|---|---|
| richards | 204 | 12 | 12 | **17.0x** |
| deltablue | 214 | 11 | 11 | **19.5x** |

- deltablue **6.5x -> 19.5x** (33 -> 11ms); richards 20 -> 12ms at t=200 (its
  t=1000 record now holds at ALL realistic thresholds). c2i_pic_rekeys=19.
- Remaining deltablue tail (2.8M interpreted bytecodes): the sub-threshold
  DRIVER methods (projectionTest: 32%, chainTest: 24%, makePlan: 15% — all
  called ~103x < 200 and OSR-ineligible because they contain closures,
  driver.rs:750). L2 target: extend the OSR envelope to closure-bearing
  methods. B1-B4 (block-arg inlining wideners) mapped from the design doc
  thereafter.
- Correctness: world byte-identical off vs t=200 plain + all three stress
  modes; deltablue correct under DEOPT_STRESS (re-key x invalidation churn).

## S24 L2 steps 1-2 — counters bit fix + trigger unification (d609251)

2026-07-10. The L2 design pass (3-reader + 3-design panel + judge workflow)
CORRECTED the premise: 3 of deltablue's 4 tail drivers have zero closures and
already OSR-compile — the tail was sub-threshold CALL entry (calls never
consult by_key until the invocation counter crosses). Fix, user-decided
policy: "the loop counters have detected in a different way that the method
containing the loop is hot; the method is now hot" — a by_key install
saturates the invocation counter to the threshold, unifying the two profile
triggers with zero new dispatch state.

| benchmark | interp (ms) | jit t=200 | best/interp |
|---|---|---|---|
| richards | 204 | **6** | **34.0x** |
| deltablue | 214 | **7** | **30.6x** |

- deltablue **19.5x -> 30.6x**, richards **17x -> 34x** (its loop methods
  were also OSR-earned + call-starved). trigger_unifications=3 per bench.
- Also fixed en route (found by the design pass's read phase): the
  COUNTERS_COMPILE_DISABLED_BIT sat inside the S15 loop-counter field —
  loopy NoPermanent methods re-attempted compilation every 10k backedges
  forever (unguided re-compilation). Now bit 33; tripwire test pins it.
- Remaining deltablue tail (1.74M bc): pre-first-OSR warmup of the drivers
  (~55%) + AbstractConstraint>>inputsKnown: (14.5%, loop-free — the B1/B3
  block-arg-inlining flagship). Envelope steps 3-6 (OSR for closure-bearing
  methods via Context adoption) next per the design; measured by the new
  ctxloop.mst when built.
- Correctness: world byte-identical off vs t=200 plain + all three stress
  modes; benches correct under stress; new send-based integration test
  proves a sub-threshold call enters an OSR-earned nmethod (<50 dispatched
  bytecodes vs ~800 interpreted).

S24 arc summary to date: interp -> 6-7ms on both flagship benches
(A1 5.3x -> A2 6.3x -> A3 6.5x -> L1 19.5x -> L2 30.6x deltablue;
richards 16x -> 17x -> 34x).
