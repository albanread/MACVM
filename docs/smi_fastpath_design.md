# Smi fast path — the integer twin of the float fast path (2026-07-25)

Dart V1 (1.24.3, native arm64, measured honestly in `cog_bench.md`) runs
benchArith 4.81× faster than MACVM — the purest gap on the board: a
smi-only loop, no calls, no allocation. The float fast path solved this
exact disease for doubles (Mandelbrot 746→25 ms: box/unbox cancellation +
raw write-through temps). The smi twin was never built. This doc grounds
it in the measured loop body and cuts it into benchmark-gated slices.

## The evidence: benchArith's compiled loop today

`MACVM_DBG_IR=benchArith` on the loop `s := s + (i*i) - (i*3)`:
IR is tight (4 SmiArith + SmiCmpBr + Poll per iteration). The LISTING is
not — ~55 instructions/iteration where ~10-12 are essential:

| bucket | /iter | why it exists | why it's removable |
|---|---|---|---|
| smi tag guards (`tst;b.ne`) | ~14 | every fuse re-guards both operands | `i`,`s`,limit,consts are smi BY CONSTRUCTION (ConstSmi / SmiArith dst / smi pool lit); `i` is guarded twice in ONE mul fuse |
| write-through stores | ~8 | every def spilled for deopt visibility | deopt only READS slots at safepoints; between polls registers can be authoritative |
| reloads | ~6 | incl. `str x17;ldr x17` same-slot inside mul fuse; post-Poll refresh of x21-x25 | artifacts of spill-all across the per-iteration Poll |
| mul overflow (`smulh`+cmp) | 2×4 | generic mul ovf | loop bound proves `i*i ≤ 2.25e12` « smi max — range analysis (R2 machinery) can elide |
| add/sub ovf (`b.vs`) | 2 | genuine (s reaches 1.12e18) | keep — 1 instruction each |
| essential | ~10-12 | arith + cmp + poll check | — |

Dart's loop: untagged register-resident ints, branch-on-overflow, poll
amortized. Same shape MACVM's float path already achieves for doubles.

## Slices

### S1 — known-smi propagation: delete provably-true tag guards

A per-method fact set `known_smi: HashSet<vreg>`: a vreg is known-smi iff
EVERY def of it is smi-producing — `ConstSmi`, any `SmiArith*` dst
(result of a smi op is smi), a `ConstPool` whose pool word is a tagged
smi, or a `Move` from a known-smi vreg (fixpoint over Move edges; the IR
is not SSA, so the rule is all-defs, which is flow-insensitively sound).
Emission consults the set and SKIPS the operand tag guard when proven.
In benchArith every guarded value qualifies → ~14 guards/iter → 0.
Behavior-identical by construction (guards are only dropped when they
cannot fail), so the gate is byte-diff on non-smi-heavy goldens + the
full differential battery.

### S2 — poll-path spill relocation: registers authoritative between polls

Today values live in FRAME slots across the per-iteration `Poll`
(spill-all): every def writes through, every iteration reloads. The fix
mirrors how a rare-path should pay: move the spill-all INTO the
poll-taken slow path (before the `blr` to stub_poll) — the fast
fall-through keeps loop-carried vregs resident in x21-x27 and writes
NOTHING. GC only scans at the safepoint, which now executes after the
relocated spills; the poll's deopt reads the same just-spilled slots.
Proven-smi vregs (S1's set) need no GC slot at all even when spilled.
Removes ~13 memory ops/iter from the fast path. This is the LIVE-value
register residency F3c actually wanted (the dead-slot census variant was
falsified; this is the version with the evidence behind it).

### S3 — loop-bounded mul overflow elision (+ crumbs)

Extend R2's bound provenance to `SmiArith Mul` where both operands are
loop-bounded (`i ≤ pool-lit limit` from `SmiCmpBr`): `i*i` and `i*3`
lose the `smulh` sequence (→ `SmiArithNoOv`). Also: stop write-through
of `ConstSmi` temps (storing the constant 3 to the frame each iteration
is pure waste; rematerialize on deopt via the existing `ValueLoc::
ConstSmi`).

## Order and gates

S1 first (pure redundancy, no metadata changes), benchmark; S2 second
(the big structural one — touches regalloc/emit safepoint discipline),
benchmark; S3 last. Each slice: lib + it_tier1 green, 4-mode release
differential (JIT-off vs threshold, GC_STRESS both flavors, DEOPT_STRESS),
interleaved A/B on arith + the six others (watch alloc/richards for
regression), commit. Expected composite: arith's ~55/iter → ~20 —
roughly 33.6 → 13-15 ms batch, closing most of Dart's 4.81× to ~2×.
fib/richards should ride S1+S2 too (every compiled method has guards;
every loop has a poll).
