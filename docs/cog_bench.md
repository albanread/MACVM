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

## Scoreboard (2026-07-22, M-series, best of 3 rounds)

Two same-day measurements: BEFORE and AFTER porting WINVM 9cb272e's
special-selector lowering to arm64 (`Ir::RefCmpVal` for identity `==`/`~~`,
`Ir::BoolNot` for boolean `not` — `cmp`+`csel` and a guarded literal-compare
flip in emit.rs, the A64 sequences WINVM's own port left unwritten).

| bench     | MACVM before | MACVM after | Cog ms | verdict (after)    |
|-----------|-------------:|------------:|-------:|--------------------|
| arith     |         35.1 |        35.3 |   59.6 | **MACVM 1.69x**    |
| fib       |        155.9 |       158.8 |  189.4 | **MACVM 1.19x**    |
| sieve     |          2.3 |         2.4 |    3.6 | **MACVM 1.52x**    |
| dict      |          8.3 |         8.5 |   15.9 | **MACVM 1.86x**    |
| alloc     |         13.0 |        12.9 |   14.7 | **MACVM 1.14x**    |
| richards  |     **63.3** |    **20.1** |   22.4 | **MACVM 1.11x**    |
| deltablue |      **4.1** |     **2.8** |    3.5 | **MACVM 1.23x**    |

(warm = median of 6 x10-rep batches, microsecond clock; MACVM `threshold=20`.)

## What this says

- **MACVM now wins all seven benchmarks.** The morning's honest measurement
  showed richards **2.85x behind** (63.3 vs 22.2) and deltablue 1.15x behind
  — real, threshold-independent, and not a deopt storm (44 warmup deopts
  total). The cause was exactly WINVM's independent x64 diagnosis: richards
  sends `==` ~130k times and activates `not` ~90k times per run — selectors
  Cog's bytecode compiler never emits as sends at all.
- The special-selector port (same day) closed it: richards 63.3 → 20.1 ms
  (3.1x), deltablue 4.1 → 2.8 ms. Both now BEAT Cog — unlike WINVM/x64,
  which still trails its Cog ~1.15x on richards (its younger x64 backend
  spills more per activation; and this port also fuses inside spliced BLOCK
  bodies, a third splice arm WINVM doesn't have).
- The port found two upstream-worthy WINVM bugs: its `successors()` misses
  `BoolNot`'s trap edge (reverse-postorder would drop the trap block), and
  its canonical-`^false`/`^true` decoder requires the method to END at the
  ReturnTos — but this frontend appends a dead implicit `ReturnSelf`, so the
  check never passed and `not` silently stayed generic (the fix accepts dead
  trailing code after the unconditional return).
- Remaining Cog gaps: none on this suite. The nearest-to-parity rows
  (richards 1.11x, alloc 1.14x) are the ones to watch when codegen changes;
  WINVM's F3c (register-resident oops across safepoints) is the next
  structural lever if richards needs more headroom later.
