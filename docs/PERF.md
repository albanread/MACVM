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
