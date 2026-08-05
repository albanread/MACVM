# Cost is position in the dependency graph, not instruction count

*(2026-08-05, measured at `aa6c368` on an Apple **M4** P-core, 4 P-cores.)*

Two ceiling probes on the same benchmark (`sieveOnce`), same session, same
protocol — 8 interleaved rounds, alternating order, load-gated — settle a
question this repo keeps re-asking: what actually costs time in generated
code on Apple Silicon?

| arm | mean | vs base |
|---|---|---|
| base | 48.4 µs | — |
| remove the card barrier (**−12 instructions per store**) | 49.4 µs | **−2.1%, i.e. nothing** |
| remove the bounds check (**−4 instructions per store**) | 40.9 µs | **+15.5%** |
| remove both | 41.8 µs | +13.7% (no better than bounds alone) |

**Removing three times as many instructions bought nothing; removing a
quarter as many bought 15.5%.** Instruction count did not merely fail to
predict the outcome — it predicted the wrong winner.

## Why, from the disassembly

The marking loop `[k <= size] whileTrue: [flags at: k put: false.
k := k + prime]` compiles to ~28 instructions per iteration, of which
**one** is the store (`disasm-native "BenchmarkDashboard class"
sieveOnce`, nmethod v2, marking loop at +0x0350..+0x03c0):

```
cmp x19,x20 / b.ne          klass guard          2
tst x8,#3   / b.ne          smi tag check        2
ldur x19,[x22,#15]        ← LOAD the size word  \
sub / cmp / b.hs            bounds check         4   ON the critical path
add x19,x22,x8 / add        address (two adds)   2
stur x13,[x19,#15]        ← THE STORE            1   the only useful one
ldr x20,[x28,#16] ...     ← CARD BARRIER        12   OFF the critical path
adds / b.vs                 k+prime + overflow   2
stur x8,[x29,#-112]       ← SPILL k every iter   1
ldr w16,[x28,#32] / cbz     safepoint poll       2
```

- The **bounds check is a load → compare → conditional branch that gates
  the store**. The store cannot retire until that branch resolves, and the
  branch waits on a load. Four instructions, but they sit directly on the
  loop's critical path.
- The **card barrier runs after the store and feeds nothing**. Its two
  loads hit the same card-table words every iteration (L1-hot), its branches
  are perfectly predicted, and nothing downstream waits for it. A wide
  out-of-order core with spare issue slots absorbs all twelve for free.

So the M4 has *demonstrable* spare execution capacity — twelve instructions
per iteration cost zero. That is the answer to "should we feed it more?":
**yes, extra instructions are close to free, provided they are off the
critical path.** The lever is never "emit fewer instructions"; it is
"shorten the dependency chain, and get loads and branches out from in front
of the work".

## Consequences for the roadmap

- **Card-barrier elision is FALSIFIED as a perf lever — do not build it.**
  It was ranked #2 (medium confidence) on the reasoning that Dart elides
  barriers for constant/bool/smi stores while we emit ten to twelve
  instructions. The reasoning was sound and the conclusion was wrong: the
  measured value is 0%. (Elision may still be worth it for *code size*;
  it is worth nothing for speed here.)
- **Bounds-check elision is CONFIRMED at 15.5%** — see
  `range_analysis_design.md` R4, whose ceiling this supersedes (the earlier
  11–14% came from a 6-round sample).
- **Rank future levers by critical-path position, not instruction share.**
  The 21–30% "frame-slot traffic" figure for richards is an instruction
  share; this file is the reason that number cannot be converted into an
  expected speedup without a probe. The spill of `k` at +0x03b8 and the
  safepoint-poll load are the interesting ones in this loop *because they
  are loop-carried*, not because they are numerous.
- **Unrolling is now the most interesting untried lever** and has never
  been attempted. It does not remove instructions — it adds them — but it
  breaks the loop-carried `k` dependency and puts several independent
  stores in flight at once. That is precisely the shape this core rewards.

## Unrolling: the prediction, tested (10%, and SOUND)

The law above predicts unrolling should pay: it *adds* instructions but
removes loop-carried work. Tested at source level on the same marking loop
(hand-unrolled in Smalltalk, `pJ == J*prime` hoisted as loop invariants,
tail loop for the remainder, `count=1899` preserved on every arm):

| unroll | run 1 | run 2 | vs 1x |
|---|---|---|---|
| 1x (baseline) | 41 µs | 42 µs | — |
| **2x** | **37 µs** | **38 µs** | **~10% faster** |
| 4x | 38 µs | 37 µs | ~10% faster |
| 8x | 40 µs | 39 µs | ~6% — regressing |

**~10%, and it plateaus at 2x.** That the whole benefit arrives at 2x says
the win is amortising the *loop-carried* overhead — the `k` update, the
compare/branch, the safepoint poll — not deep instruction-level
parallelism across many stores. 8x gives it back to code bloat and
register pressure.

Unlike the two probes above this is a **sound** transformation, and the
source-level result is a lower bound on what a compiler unroller would get
(it could also hoist the invariants the guard reloads — see below).

## Loop-invariant memory traffic — the other half

Both hot loops reload compile-time-known values from the constant pool on
every single iteration:

- marking loop, +0x034c: `ldr x20, pool[0x303000dd1]` — the klass constant
  for the guard, feeding the `cmp`/`b.ne` that gates the entire body;
- init loop, +0x0480..+0x0484: `ldr x10, pool[true]` **and then a spill of
  it** — two memory ops per iteration for the constant `true`.

And the method as a whole executes **158 frame spill/reload operations out
of 352 instructions** (109 stores, 49 loads).

Two levers fall out, and they are ordered:
1. **LICM** beats rematerialisation for these, because the values are
   loop-invariant — hoisting removes the operation entirely rather than
   trading it for cheaper ALU work. No LICM, dominator tree or pre-header
   exists anywhere in `src/compiler/` today.
2. **Rematerialisation** (recompute a pool constant with `movz`/`movk`
   instead of reloading or, worse, spilling it) is the right tool where a
   value is *not* loop-invariant or where the allocator would otherwise
   spill. `0x303000dd1` is three `movz`/`movk` — serially dependent, so
   roughly latency-neutral against an L1 hit, but it frees a load slot and
   costs no frame slot. Given the 158 spill ops above, remat-instead-of-
   spill is the high-value form of "more instructions, less memory".

## The gated-feature matrix — nothing earns its gate (2026-08-05)

Five optimizations sat gated OFF, each landed on the reasoning that it *ought*
to pay: `MACVM_R4` (variable-stride bounds), `MACVM_UNROLL` (2x unroller),
`MACVM_INLINE_POLY` (per-arm poly inlining), `MACVM_LFCSE` (local load
forwarding), `MACVM_PEEP_IMM` (immediate peepholes). The open question was
whether they COMPOUND — several sub-noise levers together clearing the bar.

Measured across all seven benchmarks, 4 rounds, config order rotated each
round, every value normalised against a baseline measured in the SAME round
(negative = faster):

| config | arith | fib | sieve | dict | alloc | richards | deltablue | MEAN |
|---|---|---|---|---|---|---|---|---|
| R4 | +1.7 | +0.5 | −0.4 | −3.8 | +0.2 | +0.5 | +0.0 | **−0.2** |
| UNROLL | +1.6 | +0.2 | +1.5 | −2.1 | +1.3 | +1.0 | +3.3 | +1.0 |
| INLINE_POLY | +1.7 | +0.5 | −0.0 | −0.4 | +1.2 | −2.4 | −0.7 | **−0.0** |
| LFCSE | +2.1 | +0.8 | +2.3 | −0.4 | +1.3 | −0.0 | −0.0 | +0.9 |
| PEEP_IMM | +2.5 | +0.8 | +4.4 | −0.7 | +5.1 | −0.8 | +0.9 | +1.8 |
| R4+LFCSE | +2.5 | +1.0 | −0.1 | +2.2 | +1.3 | +0.1 | +2.2 | +1.3 |
| **ALL FIVE** | +2.8 | +1.0 | −1.2 | +2.9 | +5.3 | −0.4 | −0.0 | **+1.5** |

**Nothing compounds.** All five together is +1.5% — worse than baseline. The
specific compounding hypothesis (R4 deletes the array size-word load, giving
LFCSE something to forward) is falsified: R4+LFCSE is worse than either alone.

**Read the uncertainty honestly.** Every config shows +1.5..+2.8% on `arith`,
including ones that cannot touch it — so ~±2% of systematic bias survived even
the rotation, and every row sits inside that band. The conclusion is NOT "these
features are mildly harmful"; it is **"none is measurable, alone or in any
combination tested."**

Also: R4's isolated +3.1% on sieve did NOT reproduce here (−0.4%). The isolated
A/B drove `sieveOnce` directly for 400 iterations; `benchSieve` runs it 4x per
sample with different compile/OSR dynamics. Since R4 only wins in one compile of
four, the +3.1% was driver-specific, not a general win.

**A methodology note that cost a table.** The first run of this matrix measured
each config's reps consecutively, baseline first, never re-measured. Every
config came out 5–11% "worse" on arith — five independent features degrading one
benchmark identically, which is drift, not signal. It looked entirely plausible.
Uniform, unsurprising numbers deserve the same suspicion as spectacular ones;
interleave and rotate, or do not report.

## Method note

Both numbers come from deliberately **unsound** throwaway probes
(`MACVM_UNSAFE_NOBOUNDS`, `MACVM_UNSAFE_NOBARRIER`) that skipped the work
entirely, were checked to still produce `count=1899`, were measured on the
clock, and were then deleted. Neither is in the tree. This is the only
honest way to get a ceiling: estimating from instruction counts is exactly
the error this file exists to document, and the repo already had the
receipt for it — **−21% instructions once bought −0.35%, about 60:1**.

One trap worth naming: an intermediate run reported the bounds arm at
"0.0%" because the probe had already been reverted from that binary — a
dead env flag reads exactly like a null result. **Verify a probe actually
changes the measurement before trusting the arm that uses it.**
