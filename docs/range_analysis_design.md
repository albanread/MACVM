# Range analysis — induction-bounded check elimination (dart124 item 6)

*Dart 1.24.3's `flow_graph_range_analysis.cc` runs symbolic range inference
and deletes proven `CheckArrayBound`s and smi-overflow deopts. Its sieve
runs 0.69 ms where ours runs ~2.3 on identical semantics. This design is
the MINIMAL version WINVM's dart124 doc calls for: the structural
`to:do:` pattern only, no symbolic general case — the benchmarks' loops
are all this shape, verified against the real `sieveOnce` IR (below).*

## Why this is the right next lever (and the F3c connection)

The F3c census (`docs/f3c_census_findings.md`) proved register residency
across safepoints is blocked by TRAP-site metadata: every smi-overflow
fail edge records all slot vregs and pins them to frame slots. Range
analysis attacks the same loops from the other side — it DELETES those
trap sites. Payoff is therefore double:
1. direct: the `b.vs` overflow branch + trap block disappear from the
   hot loop body;
2. indirect: fewer trap deopt-sites → fewer membership pins → the F3c
   census (`MACVM_F3C_COUNT=1`) should flip from `freed=0` toward
   nonzero, re-opening the residency door. The census is the instrument;
   it is a bonus metric here, NOT a build gate.

## The verified pattern (real `sieveOnce` v0 IR)

```
block 0:  v8  := ConstSmi 1            ; induction init
          v9  := v3 (:= PoolLit smi 8190)  ; loop bound, const via Move chain
block 1:  SmiCmpBr { Le, a: v8, b: v9, if_true: 2, if_false: 3, fail: trap }
block 2:  ArrayAtPut { arr: v1, idx: v8, val: true, fail: trap16 }   ; R2
          SmiArith  { Add, dst: v35, a: v8, b: ConstSmi 1, fail: trap18 } ; R1
block 17: v8 := v35 ; Poll ; Jump 1
```

## Rungs (each gated + committed)

**R1 — overflow-check elimination on bounded adds.** A pure IR reducer
pass (sibling of `copy_propagate`, runs before regalloc):
- For each block B that is the `if_true` target of
  `SmiCmpBr { Le|Lt, a, b, .. }`: derive `upper(a) = const(b)` when `b`
  resolves to a smi constant through a single-def `Move`/`ConstSmi`/
  `ConstPool`(smi-raw) chain, and `a` is not redefined before use.
- Rewrite `SmiArith { Add, a, b: const c > 0 }` (with `a` bounded,
  `upper + c <= SMI_MAX`) — and the self-add form `a + a`
  (`2*upper <= SMI_MAX`, sieve's block 6) — to the new
  `Ir::SmiArithNoOv { op, dst, a, b }`: NO fail edge, NOT a safepoint,
  emitted as a bare tagged `add`/`sub` (smi tag 00 — tagged addition is
  exact). Its trap block, if now unreferenced, dies naturally
  (unreachable blocks are already tolerated by layout).
- Scope: `Add`/`Sub` only (`Mul` needs untag-shift care — later),
  non-OSR and OSR alike (the proof is per-op, not per-entry).
- Expected: arith/sieve movement; census `freed`/`trapext` rises.

**R2 — bounds-check elimination where size is PROVEN.** Two provable
sources, both structural:
- the bound vreg is `LoadField` of the SAME array's size word
  (`1 to: arr size do:` — richards' shape); or
- the array is this method's own `Alloc`/customized-`new:` with the SAME
  vreg as its size argument (sieve's `flags := Array new: n` — the
  guarded metaclass receiver makes `new:`'s postcondition compiler
  knowledge, same license as the customized-basicNew fuse).
Rewrite `ArrayAt`/`ArrayAtPut` to unchecked forms when
`1 <= lower(idx)` and `upper(idx) <= size`. R2 lands ONLY after R1's
differential has soaked — it is the sharper knife (a wrong bounds proof
is heap corruption, not a wrong answer).

**R3 — measure + the F3c census re-read.** Interleaved bench pair
(arith + sieve named movers in advance); `MACVM_F3C_COUNT=1` re-run
recorded in `f3c_census_findings.md` as the residency-door re-check.

**R4 — variable-stride lower bound. DEFERRED, but the ceiling is now
MEASURED (2026-08-05): 15.5% of sieve.** Worth doing later; not worth
doing first. (A first 6-round sample read 11–14%; a later 8-round
decomposition at `aa6c368` refined it to 15.5% — see
`critical_path_findings.md`, which also explains WHY this lever pays
while the larger card-barrier one does not.)

*The gap.* `lower_bound`'s `DefK::AddOf` arm proves `k` stays positive
only when the addend is a compile-time positive constant. Sieve's
marking loop —

```smalltalk
k := i + prime.
[ k <= size ] whileTrue: [ flags at: k put: false. k := k + prime ]
```

— strides by `prime`, a temp, so the proof fails and the store keeps its
check. The other two loops in the same method (`1 to: size do:` init and
scan) are both proven, which is exactly what the counter reports:

```
range-reduce: 1 overflow + 2 bounds checks deleted   # 3 array loops, 2 proven
```

The proposed relaxation is small: accept an addend that is *provably
positive* (const OR itself lower-bounded `>= 1`) rather than only a
constant.

*The measured ceiling — and how it was NOT measured.* The first estimate
came from a synthetic 15000-store microloop and claimed 37–43%. That is
wrong, and the way it was wrong is the point: at sieve's scale a clean
guarded-vs-proven pair could not be reproduced above noise (deltas of
±9 µs on a ~20 µs loop, and one variant showed the *variable*-stride arm
faster). Estimating instead from instruction counts — "the guard is 10
of 13 instructions, so it must be ~77% of the store cost" — is the trap
this repo already has a receipt for: **−21% instructions once bought
−0.35%, ~60:1**. A predicted compare-and-branch on a wide out-of-order
M-series core retires in spare issue slots; added stores are memory
traffic. They are not interchangeable, and neither substitutes for a
clock.

What was actually done: a throwaway, deliberately UNSOUND probe
(`MACVM_UNSAFE_NOBOUNDS=1`) that force-rewrote every `ArrayAt`/
`ArrayAtPut` to its existing `…NC` form regardless of proof, verified it
still produced `count=1899`, and was measured on the clock and then
deleted. Since sieve's other two checks are already proven away, the
probe's delta is precisely this one loop's check.

Interleaved A/B, alternating order, 8 rounds each, **no overlap between
the distributions**. Two independent samples agree:

| sample | guards on | guards removed | delta |
|---|---|---|---|
| first (6 rounds) | 47.4 µs | 40.6 µs | 14.2% |
| **refined (8 rounds, 4-arm decomposition)** | **48.4 µs** | **40.9 µs** | **15.5%** |

*Why deferred.* ~15% takes sieve from 2.66× behind Dart V1 to roughly
2.3× — it narrows the widest row without closing it, on the
smallest-absolute-time bench in the suite. The sound version also cannot
capture the whole unsound ceiling (the proof will not always succeed, and
it carries recompile-dependency machinery). It is real, it is bounded,
and it is independent of the inliner arc — so it waits.

*Acceptance criterion, fixed in advance.* Repeat exactly the A/B above
against a sound implementation and require **≥8%** end-to-end on sieve
(the unsound 15.5% is the ceiling; a sound pass capturing less than half
of it is not worth the dependency machinery).

*Why this one pays.* Not because it deletes instructions — it deletes
only four. It pays because those four are a load → compare → conditional
branch **gating the store**: the store cannot retire until the branch
resolves and the branch waits on the array's size word. Removing twelve
card-barrier instructions from the same loop is worth 0%, because they
run after the store and nothing waits on them. Rank by critical-path
position, not instruction count — `critical_path_findings.md`. Plus the R1
tripwire below, whose negative case is the correctness case.

*Negative findings, recorded so they are not re-run.*
- `k to: size by: prime do: [...]` marks the identical index set but is
  **4× slower** (218 µs vs 50 µs) and deletes **zero** checks — a
  variable `by:` is not lowered to a counted loop at all. Rewriting
  Smalltalk source into this shape is a pessimisation, not a fix.
- With the array arriving as an *argument*, no loop form proves anything
  (its length cannot be related to the bound). Provability needs the
  array's own `Alloc` in the method, as R2 says.
- A constant-stride `whileTrue:` over a local array *does* prove after
  recompile, so the loop form is not the blocker here — the stride is.

## Gates (every rung)

Full lib suite; it_tier1 loop/OSR/deopt suites; 4-mode release world
differential (plain + GC_STRESS=1 + full:64 + DEOPT_STRESS=64) —
byte-identical off-vs-threshold with correct checksums; release bench
pair. R1's tripwire: a loop whose bound is NOT provably const (runtime
bound near SMI_MAX) must keep its check and still trap correctly —
the negative case is the correctness case.
