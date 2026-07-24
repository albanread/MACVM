# Deopt-slot liveness — census GREENLIGHTS the per-site recording rework (2026-07-26)

The F3c census (`f3c_census_findings.md`) indicted membership-based deopt
recording: every `UncommonTrap` / inlined site records the receiver + ALL
`argc+ntemps` root slots, and membership forces each to spill-all — so
register residency read `freed=0`. The law it surfaced: *the better the
inliner, the more the deopt metadata pins.*

The obvious fix is to record only the slots a re-executing interpreter can
actually observe — **per-site bytecode liveness** instead of membership.
Before touching the delicate recording path (S12/BUG-D territory), a
behavior-free census (`ir::deopt_slot_census`, `MACVM_DEOPTLIVE_COUNT=1`;
the F3c-S1 discipline) measures the ceiling.

## Result: 58% of recorded root-trap slots are bytecode-DEAD

    recorded=2428  live=1013  dead=1415 (58%)  over 494 root UncommonTrap sites

Per hot kernel (dead / recorded):

| method | dead | recorded | % |
|---|---|---|---|
| projectionTest: | 350 | 512 | 68% |
| sieveOnce | 124 / 106 | 195 / 169 | ~64% |
| processWork: (richards) | 68, 26 | 110, 36 | 62–72% |
| benchDict | 38 | 56 | 68% |
| recalculate | 12 | 24 | 50% |
| scanFor: | 15, 16 | 40, 44 | ~37% |
| satisfy: | 8, 14 | 27, 30 | 30–47% |

This is the OPPOSITE of F3c S1 (`freed=0`): here most pins are provably
unnecessary. A re-executing interpreter resuming at the trap's bci reads
only the slots live-in there; the other 58% hold values it never observes
before overwriting.

## Method (what "dead" means, and the caveats)

- Point-accurate backward bytecode liveness over `decode::decode`'s CFG
  (`PushTemp` = use, `StoreTemp`/`StoreTempPop` = def), walked within the
  trap's block to its exact reexecute bci.
- CAPTURED slots (`irm.ctx_vregs` — temps an escaping block may read
  post-deopt) count as ALWAYS-LIVE. Args and temps are otherwise equal (a
  dead arg's frame slot is never observed → droppable).
- ROOT `UncommonTrap` sites only. Inlined-body sites (indirect root bci)
  and Call/Alloc sites are NOT counted — their reduction is separate and
  additive, so 58% is a floor on the total opportunity.
- Sound-to-measure because MACVM deopt is RE-EXECUTION (continue
  interpreting), not a debugger snapshot: only bytecode-reachable reads
  matter. (DBG4 pins a breakpointed method to tier-0, so compiled frames
  are never debugged — the frameless design already relies on this.)

## The rework was BUILT (slice 1, root traps) — and measured: flat, with a corrected hypothesis

`compute_intervals` now records receiver + operand stack + only the
bytecode-live slots at each ROOT `UncommonTrap` (via `root_trap_live_slots`,
the census's shared oracle), behind `MACVM_DEOPTLIVE=1`. Flag OFF =
membership, byte-identical.

**Correctness (sound):** 4-mode release world differential byte-identical
off-vs-`MACVM_DEOPTLIVE=1`, INCLUDING `DEOPT_STRESS=64` — the sharp gate,
which forces every trap to deopt+re-execute, so a wrongly-dropped live
slot would read nil and mismatch. It doesn't.

**But the measured effect is essentially ZERO, and two hypotheses were
WRONG:**

1. *"It will unblock F3c residency."* WRONG. The 58% dead slots are dead
   TEMPS clustered in large cold-ish methods (projectionTest: 350 of 512).
   The loop-carried accumulator/induction vregs F3c wants in registers are
   LIVE at their traps (read every iteration, by definition) — so liveness
   recording correctly keeps them pinned. Dead slots and F3c-target slots
   are DISJOINT. F3c census re-run with the flag: STILL `freed=0`.

2. *"Fewer records ⇒ fewer spills/nil-fills."* WRONG at slice 1. Frame
   slots across the whole suite: 1716 → **1713** (−3, 0.2%). Nil-fills:
   996 → **996** (zero). Per hot kernel: zero change, every one. Wall-clock
   (interleaved best-of-3): flat, ±4% both directions = noise.

**Why slice 1 is a structural near-no-op — the subsumption insight:** the
~1415 removed root-trap records are REDUNDANT. The same dead-temp vregs
are still pinned by the LoopPoll and inlined-body sites (left at
membership) and by their own cross-safepoint liveness, so dropping the
root-trap record doesn't unpin the slot. A slot only truly unpins when
dropped at ALL its sites at once.

## Disposition — KEPT behind the off-by-default flag as a FOUNDATION

Slice 1 is sound and complete but pays nothing alone. It is retained
(default OFF ⇒ zero impact) as the first plank + the `root_trap_live_slots`
machinery for a future ALL-SITE reduction (root + LoopPoll + inlined
consistently), which is the only version that could move frames — at much
higher risk (LoopPoll loop-carried liveness, inlined multi-frame
liveness). The scenario that would justify that: an aggressive inliner
minting far more deopt sites and dead slots (dart124 item 2). Until then,
the census (`MACVM_DEOPTLIVE_COUNT`) and `MACVM_FRAMESTAT` stay as the
permanent ceiling + effect instruments.
