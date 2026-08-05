# Per-arm polymorphic inlining — the P arc (campaign opener)

*Written 2026-08-05, on the post-Z-arc profile. Extends
`docs/budgeted_inliner_design.md` (whose F-arc closed with "richards'
residue wants a fresh profile"); this doc is that profile plus the plan.*

## 0. Decision of record — P-D1: gated off by default, A/B flips it

Additional inlining lands **behind an opt-in gate, off by default**
(`MACVM_INLINE_POLY=1`, following `MACVM_INLINE_LEVEL`'s pattern in
`inline.rs`). Inlining has measurably slowed this VM before — the level-4
probe turned Mandelbrot 12× slower through fusion decay in spliced bodies
(`ir.rs:1290`'s lesson), and `regalloc_findings.md` records the A/B gate
rejecting plausible changes. So: every P slice is measured gate-on vs
gate-off on the full seven-bench harness; **only a reproduced win flips
the default**; a flat or negative result is recorded in §5 and the gate
stays off — never mind, no harm done. The differential gate proves
correctness either way; only A/B proves profit.

## 1. Motivation — the fresh richards profile (2026-08-05, post-Z)

`MACVM_TRACE=nmmap` + `sample(1)` join, 20k `runOne`, 9,586 top-of-stack
samples: **93% in stable compiled Smalltalk bodies, 0.4% Rust runtime**
(the Z arc erased the call-out residue), 6.3% PIC/dispatch stubs.

| share | body |
|---|---|
| 35.9% | `RichardsBenchmark>>schedule` |
| ~27% | `processWork:` ×4 (Handler 11.7, Device 6.1, Idle 5.7, Worker 3.4) |
| ~12.5% | `runTask` ×4 + `queuePacket:` |
| ~7% | small helpers: `release:`, `append:head:` ×2, `addInput:checkPriority:`, `taskWaiting:` |

`schedule`'s hot sends (`runTask`, `processWork:` targets via the task
chain) are clean **4-way task-subtype PICs**. The existing poly shapes
(`DominantWithSlowPath` ≥34% share, `SameTargetPoly`, `PolyCmpFuse`)
cannot express "inline all four arms"; richards' round-robin scheduler
gives no dominant arm on the sites that matter. The July anatomy
(`richards_profile.md`) still holds: hot bodies are 21–30% frame-slot
traffic, so per-arm inlining also feeds the regalloc levers — bigger
spliced regions give residency more to work with.

## 2. Mechanism sketch (to be validated against `inline.rs` before P1)

A new `PolyShape::PerArm { arms: Vec<(KlassOop, MethodOop)> }` decided in
`inline.rs`'s decision layer when: site is poly with 2–4 receiver klasses,
**every** arm's target passes the same eligibility the mono splicers use
(leaf / nonleaf / CFG, budgeted), cumulative cost within the level budget,
and each arm's klass is scavenge-stable (the same key-klass discipline
nmethods already keep). Lowering: a `GuardKlassIn`-ordered dispatch chain
(hottest first, per IC counts) where each arm falls into its own spliced
body — the splicers already exist; the new work is the multi-arm driver,
the merged continuation (each arm's result vreg φ-joins the same
continuation stack slot), and the final else-edge (reexecute trap, or a
plain `CallSend` — decide by measurement; the trap risks megamorphic-drift
storms, the call keeps a send).

Known risks, named now:
- **Fusion decay inside arms** — every `_on` twin must fire inside each
  spliced arm exactly as in mono splices (the Mandelbrot lesson; the Z
  twins made this uniform, which is what makes P feasible at all).
- **Register pressure / code growth** — 4 arms × 300-insn bodies is
  1200+ insns in one frame; budgets must scale per-arm and the A/B may
  simply say no. That is an acceptable outcome (P-D1).
- **Deopt shape** — each arm's traps carry that arm's inline proto; the
  dispatch chain's else-edge re-executes the ORIGINAL send.

## 3. Slices

- **P0 — gate + baseline.** `MACVM_INLINE_POLY` plumbed through
  `InlineBudget`; no behavior change at either setting; the 7-round
  harness run recorded as the baseline for every later A/B.
- **P1 — two-arm PerArm**, leaf/nonleaf-eligible arms only. Gates +
  A/B on dict (its `SmallInteger`/`Symbol` `=` sites are the simplest
  real poly shape) and richards.
- **P2 — up to four arms + CFG-eligible bodies** (the richards shape:
  `processWork:`/`runTask` are branchy). A/B richards specifically;
  expect this to be the slice that moves it or proves it can't.
- **P3 — the verdict.** Reproduce the winning configuration twice
  (P-D1), flip the default or record the rejection; either way, re-run
  the nmmap profile and hand the residue to the regalloc arc.

## 4. Gates (every slice)

The Z-arc battery (byte-identical differential, both GC-stress modes,
release suite) **plus, specifically for inlining**: the Mandelbrot canary
(`MandelZoom` frame times must not regress — the historical failure mode),
`MACVM_DEOPT_STRESS` both thresholds (per-arm deopt protos are the novel
metadata), and both A/B directions per P-D1.

## 5. Measurement log

### P0 + P1 landed 2026-08-05 — gate stays OFF (P-D1 applied as written)

P0: `poly_inline_enabled()` (`MACVM_INLINE_POLY=1`, read-once) — gate-off
world-test output is byte-identical to pre-P1 (a true no-op). P1:
`InlineDecision::PerArmPoly` (2 arms, different targets, every arm a
send-free leaf within budget, no smi case, evidence floors) + the
lowering: the DominantWithSlowPath shape with one minted middle block —
current block guards arm 0 (fail → arm 1's block), arm 1 guards its
klass (fail → the shared rejoining slow send), all three routes Move
into one shared dst. `MACVM_TRACE=perarm` prints each fired site so a
flat A/B is distinguishable from a dead gate.

Gate-on gates: differential byte-identical (6200/0), both GC-stress
modes green.

**Coverage census (the actual finding):** the leaf-only 2-arm shape
fires on exactly 2 sites (`#isNil`) on the classic bench, 2 on the
library composite, and **zero on richards** — whose poly targets
(`processWork:`/`runTask` ×4) are 4-arm *branchy* bodies, not leaves.
A/B: classic-bench warm total 12293 vs 12237 µs (0.5%, inside noise).
Verdict per P-D1: **default stays off**; the machinery is correct and
waiting. P2 widens coverage next.

### P2 landed 2026-08-05 — gate stays OFF, and the premise is falsified

P2 generalizes the lowering to 2–4 arms, each arm a send-free leaf OR a
CFG-eligible body (each CFG arm = the SameTargetPoly cfg-leg recipe:
graft, stub continuation Moving into the shared dst, side blocks ending
in an explicit Jump to their graft entry; built last-arm-first so every
guard's fail target exists). The count floors were removed for
PolyCmpFuse's exact documented reason — real poly sites freeze at counts
~{0,1}, so any floor locks them into Call forever.

Gates: gate-off still byte-identical to pre-P1 (a true no-op); gate-on
differential byte-identical (6200/0), both GC-stress modes green,
`MACVM_DEOPT_STRESS=1` green.

**The census finding that closes the arc's question:** richards'
heavyweights were never poly-dispatch problems. `runTask` is defined
ONCE on TaskControlBlock — `SameTargetPoly` already serves the schedule
loop — and `processWork:` is a SELF-send inside `runTask`, which
per-klass customization already devirtualizes statically, guard-free
(the profile's "runTask ×4 / processWork: ×4" nmethods are customized
copies, not dispatch arms). What per-arm inlining actually finds on
richards is the helper bucket: `#priority` (3 sites, 3-arm leaf) and
`#taskWaiting:`. A/B, 3 interleaved rounds: classic-bench totals
12424/12483/12567 (off) vs 12455/12516/12519 (on); richards 1043–1122
vs 1059–1083 — flat, bands overlap.

**Verdict per P-D1: default stays off. The residual richards lever is
NOT dispatch shape — it is the inline BUDGET for statically-known
self-send targets** (`processWork:` bodies are ~300–460 compiled insns,
far over `per_call_cost`, so the devirtualized call stays a call) plus
the slot-traffic regalloc arc. P3-as-planned (reproduce-and-flip) is
moot; the P arc closes here and hands off to a budget/regalloc campaign
with its premise honestly falsified — which is exactly what P-D1's
"never mind" branch is for.

### Coda — the budget hypothesis, tested the same evening (zero code)

`MACVM_INLINE_LEVEL=4` (per_call 120 / total 2400 / depth 8) vs default,
3 interleaved rounds on the classic bench: richards 1080–1104 →
**3318–3369 µs (3× worse)**, fib 8880–8945 → 10370–10454 (+17%), suite
total +38% — perfectly reproduced. So brute-force budget raising is
falsified too, and NOT via the old fusion-decay mode (the Z twins fixed
that): the giant spliced bodies lose on **slot traffic and register
pressure** — the same 21–30%-of-instructions cost `richards_profile.md`
measured in the hot bodies, made worse. The ordering conclusion for the
next campaign: **the regalloc/slot-traffic arc comes FIRST; selective
depth (the `processWork:`-into-`runTask` self-send chain specifically)
only pays after big bodies are cheap.** Both cheap richards hypotheses
died with data in one evening — that is the P arc's real deliverable.
