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

## Disposition — GREENLIT, next is the rework

Unlike F3c S1, the rework is worth building. The change:
`compute_intervals`' `record()` loops (regalloc.rs) stop recording all
`1..=n_slots` and instead record receiver + operand stack + only the
bytecode-live-in slots at each site's bci. Fewer forced
`crosses_safepoint` pins ⇒ fewer spills, smaller oopmaps, fewer task-#94
nil-fills — and the loop-carried vregs F3c wanted in registers stop being
pinned by dead-slot records.

Risk is real (the earlier-safepoint task-#94 coverage, the BUG-D
path-sensitivity scars, the ctx-capture and inlined-frame interactions),
so the rework lands behind its own flag with the full 4-mode +
DEOPT_STRESS differential as the gate, and the F3c census re-run
(`MACVM_F3C_COUNT`) as the confirming second signal — it should move off
`freed=0` once the dead-slot pins are gone. The census stays permanently
as the ceiling instrument.
