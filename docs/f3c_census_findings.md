# F3c S1 census — the design's premise falsified on MACVM (2026-07-25)

WINVM's `docs/f3c_design.md` (Dart 1.24.3 slow-path-save model) scopes its
slice 1 to: intervals whose safepoint crossings are POLLS ONLY keep their
registers, saved/restored by the poll's own slow path; traps and calls
still pin to slots. Expected movement: arith's loop-carried
accumulator/induction pair.

Before building the machinery (frame save areas, per-poll records, oopmap
union, deopt-resolution changes — S12-class risk), we ran a census
(`regalloc::f3c_census`, `MACVM_F3C_COUNT=1`, behavior-free): classify
every spilled safepoint-crossing interval by whether S1 would free it.
The classification is exactly S1's would-be eligibility rule.

## Result: S1 frees ZERO intervals across the entire benchmark suite

    freed=0  of 2047 crossing intervals   (every compiled unit, all 7 workloads)

Per-kernel:

| unit | freed | freed-with-trap-ext | crossing |
|---|---|---|---|
| benchArith (OSR) | 0 | 0 | 12 |
| benchSieve | 0 | 0 | 7 |
| RichardsBenchmark>>schedule (OSR) | 0 | 0 | 14 |
| DeltaBlue projectionTest: | 0 | 1 | 77 |
| suite total | **0** | **72 (3.5%)** | 2047 |

## Why (the architectural cause, not an accident)

Deopt-metadata recording is MEMBERSHIP-based
(`compute_intervals`' `record()`: every `UncommonTrap` and inlined-body
deopt site records the receiver + ALL arg/temp slot vregs + its operand
stack, and membership forces `crosses_safepoint` → spill). The hot
kernels' loops are dense with smi-overflow trap fail edges, so the
loop-carried accumulator/induction vregs — S1's entire target — are
TRAP-pinned, not merely poll-pinned. "Traps still pin" in the design's
own scoping therefore leaves them pinned. The hottest kernels are also
OSR compiles, which S1 defers outright.

The second counter ("trap-ext": what if trap fail edges ALSO saved
registers before their `brk`, Dart's SlowPathCode applied to guard
fails — an extension in NO WINVM slice) frees only 72/2047, scattered
one-per-method in small units, and still ~nothing in the hot kernels —
because after the S14/S24/dart124 splicing arcs, hot methods are dense
with INLINED-body deopt sites, whose multi-frame records span real calls
and pin everything again. **The better the inliner gets, the more the
deopt metadata pins** — that is the real law this census surfaced.

## Disposition

- S1 is NOT implemented. The census stays (permanent, behavior-free,
  `MACVM_F3C_COUNT=1`) as the falsification record and the re-evaluation
  tool for any future deopt-metadata redesign.
- The residual that would actually pay is S3-shaped (callee-saved
  registers described across REAL calls + a rethink of membership-based
  recording toward per-site liveness) — a genuinely structural project.
  Any future attempt must START by extending this census to price it.
- Finding sent upstream to WINVM in spirit: their S1, built on the same
  recording architecture, would very likely also free ~0 — the design
  doc's arith prediction assumes poll-pinning is the binding constraint,
  and on this lineage it is not.

Method note: this is the F0-census pattern (classify + count before
build) and the falsify-before-building rule doing exactly their job —
the census cost ~an hour; the machinery it invalidated would have cost
days at S12-class risk for zero measured return.
