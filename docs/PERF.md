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
