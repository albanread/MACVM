# MACVM vs Cog — the honest head-to-head harness

`scripts/cog-bench.sh` runs the micro + macro benchmark suite under Pharo/Cog
and MACVM back-to-back on the same machine, same workloads, same protocol,
**microsecond clock on both sides**. The standing target is: *at least as
fast as Cog* (the production Smalltalk JIT — a far more meaningful yardstick
for this VM than C).

This harness mirrors WINVM's (its `scripts/cog-bench.sh`); the two repos
share byte-identical bench workloads (`world/41a_bench_workloads.mst`,
`world/42_benchdash.mst`), so the Pharo-side artifacts (`cog-bench.st`,
`mst2st.py`) are shared, and the MACVM driver (`cog-bench.mst`) runs the
world's own `BenchmarkDashboard`.

## Why it exists — the measurement bugs it removes

The earlier Cog comparison (the one recorded in memory as "Cog 6.5x faster on
sieve, MACVM 1.9x faster on deltablue") was **wrong in both directions**, for
two independent reasons:

1. **Clock truncation.** Both Pharo's millisecond clock and MACVM's old
   `millisecondClock` (`.as_millis()`) truncate to whole milliseconds. On the
   sub-5 ms benches (sieve ~2–4 ms, deltablue ~4 ms) that is a 25–50% error
   that both *manufactured* phantom losses (sieve read as "6.5x behind" when
   it is 1.5x **ahead**) and *hid* real ones. Fix: both sides now read a
   microsecond clock — Pharo `Time microsecondClockValue`, MACVM `Smalltalk
   microsecondClock` (primitive 252, monotonic, added for this; see
   `src/runtime/primitives.rs`).

2. **A slow Cog translation.** The earlier macro numbers (richards/deltablue)
   were taken against an ad-hoc Pharo translation of `world/41a` that made
   Cog look *slower* than it is, producing a false "MACVM is faster on the
   macro benches" verdict. `mst2st.py` now emits a faithful fileIn from the
   same `.mst` source, checksum-asserted identical to the MACVM run.

## Protocol

- Each bench is timed as **10 inner reps**; **cold** = the first 10-rep
  batch (includes compilation), **warm** = the **median of 6** further 10-rep
  batches. (Identical to WINVM's `run:block:check:`.)
- Every bench is **checksum-verified** on both VMs (e.g. richards
  `2324609297`, deltablue `224874`, sieve `1899`) — if any body diverges the
  run aborts. This is what guarantees the two VMs do byte-for-byte the same
  work.
- **Interleaved rounds:** each round runs Cog then MACVM back-to-back (a
  same-thermal-state pair), for `ROUNDS` rounds (default 3); the report takes
  best-of across rounds.
- **No hard core pinning — and the harness says so.** Unlike the
  WINVM/Windows harness (which pins both VMs to one logical CPU), macOS/arm64
  exposes no per-core affinity and thread-affinity tags are advisory (ignored
  on Apple Silicon). Foreground default-QoS work already stays on P-cores, so
  the residual is thermal drift, not the P/E lottery. The script refuses to
  start above a 1-min load of 4.0 (override `FORCE=1`), and only same-round
  pairs are meaningful.

## Running it

```sh
cargo build --release
# one-time: install Pharo 13 headless into $COG_DIR (default ./.cog) so that
#   $COG_DIR/pharo and $COG_DIR/Pharo.image exist
COG_DIR=/path/to/cog ROUNDS=3 ./scripts/cog-bench.sh
```

## Scoreboard (2026-07-22, M-series, load 1.6, best of 3 rounds)

| bench     | MACVM ms | Cog ms | verdict            |
|-----------|---------:|-------:|--------------------|
| arith     |     35.1 |   58.2 | **MACVM 1.66x**    |
| fib       |    155.9 |  187.7 | **MACVM 1.20x**    |
| sieve     |      2.3 |    3.5 | **MACVM 1.50x**    |
| dict      |      8.3 |   15.4 | **MACVM 1.86x**    |
| alloc     |     13.0 |   14.7 | **MACVM 1.13x**    |
| richards  |     63.3 |   22.2 | **Cog 2.85x**      |
| deltablue |      4.1 |    3.5 | **Cog 1.15x**      |

(warm = median of 6 x10-rep batches, microsecond clock; MACVM `threshold=20`,
richards/deltablue verified threshold-independent — 63 ms at both 10 and 20.)

## What this says

- **MACVM wins all five micro-benchmarks** (arith, fib, sieve, dict, alloc),
  1.13–1.86x. The eden bump to 32 MiB (`default_eden_for`, this session)
  turned alloc from last session's "tie" into a 1.13x win.
- **MACVM loses both macro-benchmarks** — richards **2.85x**, deltablue
  1.15x. This is real, threshold-independent, and **not** a deopt storm (44
  deopts total across the whole 7-bench run, i.e. warmup only).
- The loss is exactly the shape WINVM found independently on x64 (richards
  its largest gap): the residual is **per-activation send overhead** —
  ~130k `==` + ~90k `not` sends per richards run that Cog's bytecode compiler
  never emits (WINVM commit 9cb272e, the special-selector lowering — present
  as arch-neutral IR recognition, but the aarch64 machine-code sequences are
  not yet written), plus per-activation spill-all across safepoints (WINVM's
  F3c: register-resident oops with oop-maps covering registers). Closing the
  richards gap is now the top JIT-perf priority — a real 2.85x loss, not the
  "lead" the old numbers claimed.
