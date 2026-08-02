# Peephole findings — two negative results, measured, and what they teach

2026-08-02. The MACDART front-end performance arc (its `docs/dart_engine_laws.md`)
closed DeltaBlue from a 4.6× loss to a tie with Cog using dispatch caches,
compile-time interning, and helper fast-paths. This document records the attempt
to transfer the *peephole-shaped* laws to MACVM's own compiler — and why both
attempted cleanups were **rejected by the A/B gate**, which is itself the most
useful outcome: two anti-optimizations are now proven and documented instead of
landed.

The review that motivated this (profiling richards/arith/fib, reading emitted
nmethods via `disasm-native`) stands: MACVM's dispatch layer is exemplary — a
hot-bench `sample` profile shows **100% of time in JIT code, zero named Rust
frames**, `ic_misses=0`. The losses to MACDART on compute benches live in
**register-allocation policy** (`regalloc.rs`: "spill-all-at-safepoints +
classic linear-scan", hole-free conservative intervals) and its consequences:
arguments homed to frame slots and re-loaded per use (`fib:` reloads `n` twice
within ten instructions of storing it), whole-frame nil-init at entry, and
values bracketing every send with stores/reloads. That is the real lever
(hereafter "the regalloc rework": live-across-only spilling, parameter register
promotion, ultimately deopt environments). These peepholes were attempted as
the cheap first step. They are not cheap wins here. The reasons generalize.

## Negative result 1 — constant LVN is an anti-optimization under spill-all

**The attempt.** Per-block local value numbering for `ConstSmi`/`ConstPool`:
the first def of a constant is canonical, later duplicates become
`Move {dup, canonical}` for the existing `copy_propagate` to clean. Motivation:
benchArith's prologue loaded the *same nil pool literal four times* into four
registers for the temp nil-inits.

**Failure A — it silently killed range analysis.** The nil temp-inits classify
in `range_reduce` as `NonSmiConst` — the **ignorable** arm its bound resolver
skips ("on any path where the vreg held nil, the compare's tag check already
took the fail edge"). Rewritten to `Move`, the def classifies as `MoveFrom`,
whose recursive resolution of a pure-`NonSmiConst` source returns `None`
(*unprovable*) instead of a skip — and range analysis died method-wide: the
proven muls grew their `smulh` checks back (plus a spill/reload *inside* the
check sequence) and the loop increment regrew its overflow branch. Caught by
an IR-level trace plus a disasm diff, before the bench even ran. Guarding the
LVN to smi-valued constants restored range analysis — and removed the only
valuable dedup (nil), pointing at the real fix: teach `resolve_bound`'s
`MoveFrom` arm to treat a chain ending in `NonSmiConst` as ignorable. That is
the prerequisite for ever landing a constant-dedup here.

**Failure B — what remained was a pessimization by design.** A `ConstSmi`
rematerializes as one `movz`: no memory traffic, no dependency. Its
"deduplicated" replacement `Move` costs a register copy at best and an
`ldur`+`stur` frame round-trip under spill-all at worst. **Deduplicating
rematerializable constants is backwards on this backend.** A/B (cooled,
alternated, 3 rounds, best-of): alloc +6–7% consistent, arith +1–4%,
dict/richards +2%; only sieve marginally better.

> The corresponding MACDART law ("hot helpers must be tiny; collapse duplicate
> work") did not transfer because its substrate assumption — that a reused
> value stays in a register — is exactly what spill-all denies. **Peephole
> laws are register-allocation-policy-relative.**

## Negative result 2 — even strictly-work-removing folds lose to regalloc perturbation

**The attempt.** `SmiArithNoOvImm`: fold a block-local `ConstSmi` operand of a
range-proven `SmiArithNoOv{Add|Sub}` into an add/sub-immediate. benchArith's
loop re-materialized `movz #4` every iteration for `i := i + 1`; post-fold the
increment is a single `add x, x, #4`. Nothing else changes; one instruction is
strictly removed (the const def is kept for deopt metadata, off the chain).

**The gate said no.** Fold-only A/B (const-LVN fully removed): every bench flat
within noise **except alloc: +9%, consistent across all three rounds**
(570/572/606 → 630/621/632 µs). Mechanism: removing the constant's *use*
shrinks its conservative live interval, linear scan hands the freed register to
a different vreg, and the spill set around benchAlloc's per-iteration send
(`Association key:value:` — a safepoint, so spill-all triggers) reshuffles for
the worse. **Under spill-all with hole-free intervals, the spill-set lottery
around sends dominates single-instruction savings** — an honest instruction
removed can cost 9% via second-order allocation effects.

## Disposition

- `SmiArithNoOvImm` + `fold_noov_imm` are **landed but default-off**, behind
  `MACVM_PEEP_IMM=1` (this codebase's env-gate convention). With the flag off
  the emitted code is **opcode-identical to baseline** (disasm-diff proven).
  The pass is correct (release suite: 827 passed + the 4 known pre-existing
  failures — 3 release-mode `should_panic`/`debug_assert` artifacts + the stale
  `disasm_fallback` sdiv expectation; debug suite: 839 passed + that same stale
  test), and it is the natural companion of the regalloc rework — re-gate it
  then.
- The constant LVN is **not landed**. Its prerequisite is the
  `resolve_bound`/`MoveFrom`-of-`NonSmiConst` fix in `range_reduce`; its
  justification only exists after the regalloc rework makes a `Move` cheaper
  than a reload.
- `MACVM_PEEP_TRACE=1` prints per-compile pass counters (blocks seen, NoOv
  present) — kept, in the family of the existing behavior-free census traces.

## The transferable law (now in MACDART's `dart_engine_laws.md` terms)

MACDART's arc taught *profile first, because the cost is below the flow graph*.
This arc adds the mirror lesson for a VM you own: **gate first, because the
cost is below the peephole** — on a spill-all backend, instruction-level
reasoning about wins is unreliable in both directions until the register
allocator stops moving the ground. Sequence the work accordingly: the regalloc
rework is not just the biggest lever (fib's ~8 cycles/call activation tax,
richards' 2.3×) — it is the *precondition* for every smaller lever measuring
true.
