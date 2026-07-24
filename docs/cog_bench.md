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

## 2026-07-22 re-measure — MACVM wins ALL SEVEN (macros included)

Fresh Pharo 13 (VM v10.3.9, arm64) reinstalled into `.cog/`; `ROUNDS=3`,
load-gated (1.88), frameless emission DEFAULT-OFF (d9587cc gated):

| bench | MACVM ms | Cog ms | verdict |
|---|---|---|---|
| arith | 33.8 | 51.7 | MACVM 1.53x |
| fib | 153.3 | 184.5 | MACVM 1.20x |
| sieve | 2.3 | 3.6 | MACVM 1.56x |
| dict | 8.5 | 12.8 | MACVM 1.51x |
| alloc | 12.9 | 14.4 | MACVM 1.12x |
| **richards** | **19.6** | **22.1** | **MACVM 1.13x** |
| **deltablue** | **2.8** | **3.4** | **MACVM 1.21x** |

CORRECTED TIMELINE (git-audited): BOTH scoreboards are from 2026-07-22,
~100 minutes apart — the "07-15" date in earlier notes was wrong. The 2.85x
snapshot is 7c495f1 (18:17); ONE commit landed in between that touches the
delta: 20b37b0 (19:09, special selectors — RefCmpVal/BoolNot inlining
richards' ~130k `==` + ~90k `not` sends), committed from a PARALLEL session
mid-way through this one. The attribution is clean because everything else
held still: Cog's numbers are stable across the two runs (richards 22.2 ->
22.1) and so are MACVM's five micros (±4%); ONLY the two macros moved
(richards 63.3 -> 19.6, deltablue 4.1 -> 2.8) — exactly the two workloads
20b37b0's own commit message targets. `cog_send_portability.md`'s step-0
("re-measure before building") was right in substance: the per-activation
send-overhead diagnosis was already fixed by 20b37b0 before any new
machinery was needed. The standing "at least as fast as Cog" target is MET
on every benchmark in the suite. The harness now stamps `commit=<sha>` into
every scoreboard header so cross-run deltas are attributable by
construction.

## 2026-07-22 F3 confirmation — frameless default-on (commit=145c881)

Same protocol, ~30 min after the previous table; Cog stable (richards 22.1
both runs), MACVM micros ±3%: richards 19.6 -> **18.8** (the frameless F2
prediction of ~2-4%, visible cross-VM), deltablue flat at 2.8. Final margins:
richards MACVM 1.17x, deltablue 1.28x, micros 1.17-1.54x — all seven ahead
with frameless as the shipped default.

## 2026-07-22 F7 confirmation — fib joins the movers (commit=3072c77)

Fresh quiet-load run (load 2.0, ROUNDS=3) after F7 (prologue nil-fill
shrink, the WINVM 9cb272e port with the tightened whitelist rule):

| bench | MACVM ms | Cog ms | verdict |
|---|---|---|---|
| arith | 34.0 | 51.1 | MACVM 1.50x |
| **fib** | **135.4** | 181.0 | **MACVM 1.34x** |
| sieve | 2.3 | 3.5 | MACVM 1.48x |
| dict | 7.7 | 12.0 | MACVM 1.55x |
| alloc | 12.5 | 14.2 | MACVM 1.13x |
| richards | 18.8 | 21.9 | MACVM 1.17x |
| deltablue | 2.7 | 3.5 | MACVM 1.27x |

Attribution is clean by the same-session rule: Cog held still vs the prior
sessions (richards 21.9 vs 22.1, arith 51.1 vs 51.7, fib 181.0 vs 184.5)
and MACVM moved only on F7's predicted prime beneficiary — fib 153.3 ->
135.4 ms (~12%; its three removed fill stores execute 4.4M times per
timing). richards matches the frameless-confirmation 18.8 exactly; the
other five rows are within +-5%. All seven remain ahead with frameless
(F3) and the fill shrink (F7) both shipped as defaults.

## 2026-07-25 dart124 poly-inlining arc — richards 19.0 -> 17.8 (commit=a3405f4)

The WINVM dart124 items-2+3 port, landed as three gated slices mirroring
WINVM e545380/b45d2d6/8d9325f (plus the dead-tail unlock, 943d896, ported
earlier the same day): per-arm PIC counts (interpreter row-7 is the
profiler; upgrade/append seed the triggering arm at 1), count-proven
dominance at any arity (16-sample/34%-share floors retire the len==2 pin),
and SameTargetPoly — the all-arms-one-method splice behind the new
`Ir::GuardKlassIn` membership guard, leaf leg + CFG-graft leg for
multi-block predicates.

MACVM-only runs (no Cog interleave this session; the .cog install was
present but the arc's gate is the checksummed differential, not the
head-to-head):

| bench | before (session baseline) | after slice 3 | confirm |
|---|---|---|---|
| richards | 19.0 (18.9-19.2 band, 4 runs) | **17.8** | 17.9 |
| deltablue | 2.8 | 2.8 | 2.9 |
| others | — | flat +-3% | — |

Slices 1 and 2 were bench-flat (WINVM's own instructive negative
reproduced: richards' hot sites are flat-by-klass AND organic warm-up
starves an unseeded 16-sample floor — 12 < 16); slice 3's count seeding
is what unlocked the sites, worth ~6%. Honest framing vs WINVM's -25%:
their gain came off a 27 ms baseline without our mono-splice machinery;
the same mechanism buys a smaller real slice on top of ours.

Gates: full lib 835/835; the two new it_tier1 e2es (synthetic-seeded
membership guard; ORGANIC round-robin warm-up through upgrade/append
seeding into leg=cfg) plus the count-seeded dominant e2e; release
4-mode world differential (plain, GC_STRESS=1, GC_STRESS=full:64,
DEOPT_STRESS=64) byte-identical JIT-off vs threshold=20 with correct
checksums — covering the dead-tail fix and all three slices together.

## 2026-07-25 head-to-head confirmation — richards margin 1.17x -> 1.27x (commit=79be668)

Interleaved 3-round run (load 1.58, quiet gate passed) after the dart124
poly-inlining arc:

| bench | MACVM ms | Cog ms | verdict |
|---|---|---|---|
| arith | 34.9 | 50.8 | MACVM 1.45x |
| fib | 136.7 | 186.0 | MACVM 1.36x |
| sieve | 2.3 | 3.6 | MACVM 1.56x |
| dict | 7.9 | 12.5 | MACVM 1.59x |
| alloc | 11.8 | 14.5 | MACVM 1.23x |
| **richards** | **17.5** | 22.3 | **MACVM 1.27x** |
| deltablue | 2.8 | 3.5 | MACVM 1.27x |

Attribution is clean by the same-session rule: Cog held still (richards
22.3 vs the prior 21.9-22.1 band, fib 186.0 vs 181-184.5) and MACVM moved
on the arc's predicted bench — richards 18.8 -> 17.5 (the MACVM-only runs
bracketed it at 17.8/17.9; 17.5 is best-of-3 under the interleave). All
seven remain ahead; the richards watch row improves from 1.17x to 1.27x.

## 2026-07-25 dict collapse — PolyCmpFuse (poly-identity `=`), 6.26 -> 3.35 ms

The first purely benchmark-driven find after retiring census-first. A c2i
adapter census (`MACVM_C2I_CENSUS=1`, stubs.rs) showed benchDict spending
4M interpreter dispatches per 200 reps on ONE selector: `SmallInteger>>=`.
Chain, fully evidenced (MACVM_DBG_IR + MACVM_TRACE=deopt): `Dictionary>>
scanFor:`'s `probe = key` IC is shared by every Dictionary, and the boot
world's dictionaries key on Symbols — so v0 compiled the site as
GuardKlass(Symbol)+RefCmpVal. benchDict's smi keys then failed that guard
on every probe -> bci-47 trap storm -> the storm recompile (v1) saw a poly
{Symbol, smi} IC no existing decision could serve (targets differ, so no
SameTargetPoly; the smi arm's method is a primitive, so no
DominantWithSlowPath splice) and gave up to a plain CallSend — every key
compare left compiled code for the interpreter, forever.

Fix: `InlineDecision::PolyCmpFuse` — at a poly `=` site where EVERY arm is
individually fusible to one raw-bits compare (smi prim 14 with a both-smi
guard, whose key-miss routes coercion (`3 = 3.0`) to the send; or an
identity-`=` body `^self == other`, Symbol's shape, sound for any arg),
lower a receiver-dispatch chain of guarded RefCmpVals, hottest first,
every miss edge ONE shared rejoining send. No traps anywhere -> can't
storm; an unseen klass is merely slow, never wrong. No count floor — the
post-storm site's counts are frozen at ~{0,1} (compiled sends never bump
interpreter ICs), so any floor would lock in the give-up Call.

Interleaved 3-round A/B (HEAD vs fix, same tree otherwise, load ~2.1):

| bench | before warm us (med) | after warm us (med) | delta |
|---|---|---|---|
| **dict** | 6258 | 3354 | **-46% (1.87x)** |
| deltablue | 6783 | 6353 | -7% (consistent sign, all rounds) |
| others | — | — | flat within noise |

All 7 checksums held on all 6 runs. Gates: lib 835+3, it_tier1 104 (new
e2e `poly_cmp_fuse_dispatches_all_legs_and_slow_path` covers both fast
legs hit+miss, the smi-key coercion route, and the unseen-klass slow
path, interpreter==compiled), and the 5-mode release differential
(JIT-off / t=1 / GC_STRESS=1 / GC_STRESS=full:64 / DEOPT_STRESS=64) all
exit 0 with zero WRONG lines. No Cog interleave this round — the Cog-side
table above is unchanged; re-stamp dict vs Cog on the next head-to-head.

Also this session, falsified before building: the S11-step-10
"repatch c2i adapters on tier-up" hypothesis — the census measured
compiled_avail=0% (the interpreted callees have no compiled version for
the receiver klass at all; dict's cost was the storm above, not stale
adapter links). A naive per-method adapter->entry patch also HANGS: the
adapter is per-method but customization is per-(klass,method), so a
wrong-klass caller loops guard-miss -> resolve -> same adapter forever.

## 2026-07-25 head-to-head re-stamp after PolyCmpFuse (commit=776f3e2)

Interleaved 3-round run (load 2.12, gate passed), threshold=20, best-of:

| bench | MACVM ms | Cog ms | verdict |
|---|---|---|---|
| arith | 35.6 | 52.9 | MACVM 1.49x |
| fib | 138.1 | 189.1 | MACVM 1.37x |
| sieve | 2.1 | 3.6 | MACVM 1.68x |
| **dict** | **3.2** | 12.9 | **MACVM 4.01x** |
| alloc | 11.9 | 14.5 | MACVM 1.22x |
| richards | 17.4 | 22.6 | MACVM 1.30x |
| **deltablue** | **1.9** | 3.6 | **MACVM 1.84x** |

Attribution clean: one commit (776f3e2, PolyCmpFuse) separates this from
the 79be668 stamp above, and Cog held its band on every row (52.9 vs
50.8, 189.1 vs 186.0, 12.9 vs 12.5, 22.6 vs 22.3…). MACVM moved exactly
on the fuse's benches: dict 7.9 -> 3.2 (1.59x -> 4.01x over Cog) and
deltablue 2.8 -> 1.9 (1.27x -> 1.84x) — deltablue's poly `=` sites gain
more at the harness's threshold=20 warmup than the t=1000 A/B showed.
Former weakest rows are now alloc 1.22x and richards 1.30x.
