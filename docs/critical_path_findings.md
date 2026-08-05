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
