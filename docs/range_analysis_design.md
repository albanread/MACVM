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

## Gates (every rung)

Full lib suite; it_tier1 loop/OSR/deopt suites; 4-mode release world
differential (plain + GC_STRESS=1 + full:64 + DEOPT_STRESS=64) —
byte-identical off-vs-threshold with correct checksums; release bench
pair. R1's tripwire: a loop whose bound is NOT provably const (runtime
bound near SMI_MAX) must keep its check and still trap correctly —
the negative case is the correctness case.
