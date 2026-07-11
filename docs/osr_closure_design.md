# S24 B-phase L2 — OSR for closure-bearing methods + sub-threshold entry

**Status:** LANDED (S24 B-phase L2). Commits d609251 (steps 1-2), 51401dc (step 3
phase A), c0c51cd (step 4 phase B Context ADOPTION), d01a67a (steps 5+7).
deltablue 30.6x, richards 34x, ctxloop 134x. Product of a 3-reader + 3-design-panel
+ judge workflow (wf_2fef6e9f-5a7, 2026-07-10; full transcripts in the session
workflow dir). Base design = "symmetry-first" (score 35/40), with grafts from
"risk-first" (32) — baseline pin, version-churn fix, `ctxloop.mst` microbench,
extra tripwires. The judge fresh-read every disputed fact against source.

## 0. The premise correction (read this first)

L2 was scoped as "extend the OSR envelope so deltablue's driver methods can
tier up." **The read phase falsified that premise:** three of the four tail
methods (`projectionTest:`, `chainTest:`, `makePlan:`) contain **zero
closures**, already pass today's envelope, and **already OSR-compile**
(nmethods exist for all three in a plain t=200 run; `osr_entries=6`). Their
interpreted bytecodes come from **sub-threshold call entry**: each is called
~103× < 200, and `activate_method` only routes a call into a compiled nmethod
once the invocation counter crosses the threshold — so every *call* interprets
end-to-end even though a perfectly good nmethod (produced by OSR!) sits in
`by_key`. The fourth method (`inputsKnown:`, 9.2%) is loop-free — no OSR
mechanism can ever touch it; it belongs to B1/B3 (block-arg inlining).

So the plan has two payoff-separated halves:
- **Step 2 (sub-threshold entry)** owns the deltablue win (~56% of the
  remaining tail): let a call enter an existing Alive nmethod below threshold.
- **Steps 3–4 (the envelope proper)** are the compile-coverage-arc
  investment — cheap post-A3b, near-zero deltablue delta (recorded honestly),
  measured on a purpose-built microbench instead.

## 1. A live bug found during the design pass (fix first)

`COUNTERS_COMPILE_DISABLED_BIT = 1 << 16` (layout.rs:311) sits **inside the
S15 loop-counter field** (`COUNTERS_LOOP_MASK = 0xFFFF << 16`,
layout.rs:301-302). `bump_loop_counter`/`reset_loop_counter` (osr.rs:43-54) do
masked RMWs over bits 16-31 — **one backedge through a loopy
`compile_disabled` method clobbers the disable bit**, so `NoPermanent` methods
silently re-attempt compiles forever. The layout.rs:309 comment predates S15.
Step 1 relocates the bit (33; bit 32 is `HAS_BP`). Note: this is a semantic
fix riding a bit move — loopy NoPermanent methods will now *actually stay*
disabled (the documented S10-D1 intent).

## 2. The identity rule (non-negotiable)

The Context is the identity hub of a has_ctx activation: every
interpretively-created closure holds it at `copied[1]`; every compiled
`AllocClosure` stores `method_ctx_vreg` there; both tiers' ctx traffic goes
through its body slots. **OSR must ADOPT the interpreter frame's existing
Context — never copy, never re-allocate** (a fresh alloc at OSR entry is the
9de470b per-iteration-snapshot failure class in one step). Every transfer rule
is an existing deopt arm run in reverse; the standing oracle is the round trip
*interpreted → OSR → mid-loop trap → interpreted* ≡ pure interpretation,
bit-identical.

## 3. Steps (each committable + gated)

**Step 0 — Premise record + baseline pin.** Correct PERF.md's "drivers are
OSR-ineligible" claim; new test pinning that a projectionTest:-shaped ctx-free
method OSR-compiles today (`osr_entries >= 1`, by_key nmethod exists).

**Step 1 — Counters bit fix.** `COUNTERS_COMPILE_DISABLED_BIT = 1 << 33`;
stale-comment fixes; tripwire test (disable survives `bump_loop_counter`×3 +
`reset_loop_counter`; counter unaliased). Gates: suites + world differentials.

**Step 2 — Trigger unification (THE deltablue payoff; USER-DECIDED 2026-07-10,
replacing the hint-bit draft — see R15).** Policy, in the user's words: "the
loop counters have detected in a different way that the method containing the
loop is hot; the method is now hot." Mechanism: when `compile_method_full`
successfully installs a `by_key` nmethod, **saturate the method's invocation
counter up to the threshold** (`saturate_invocation(m, n)`: masked RMW on bits
0-15, `max(cur, min(n, COUNTERS_INVOCATION_MAX))` — never lowers). For
threshold-triggered compiles this is a no-op (counter already ≥ n); for
OSR-triggered and versioned installs it fast-forwards, so `activate_method`'s
EXISTING gate (`bumped >= n && !compile_disabled`) routes every future call
into the nmethod with zero new dispatch machinery. No new counters-word bit;
no new branch in `activate_method`. Consequences accepted with the policy:
after a later invalidation the method recompiles on the next call like any
hot method (it IS hot); a new receiver klass's customized compile may be
minted on first call post-saturation (consistent with over-threshold
behavior today; profile-justified at method granularity, which is the
granularity the counters have). The principle guardrail holds: saturation
happens only on a profile-triggered install — no compilation ever happens
that no counter justified. Stats: `trigger_unifications` (count of installs
where saturation actually raised the counter). Payoff gate: interpreted-bc
histogram — projectionTest:/chainTest: tails (~1.55M bc) collapse to warmup
prefixes.

**Step 3 — Envelope phase A: non-ctx closure-bearing methods.** driver.rs:750
narrows to `has_ctx` only; `method_has_closure` deleted. Mostly *proving* the
existing OSR plan machinery already handles it: live phantom temps pack the
interpreter's real closure oop (valid, GC-safe, never read — uses are
CFG-spliced); dead-but-homed use the existing widening; header operand stacks
are phantom-free by the ir.rs invariant — pinned by a NEW plan-builder
tripwire (debug_assert + **release-mode decline** if any `stack_closures` fact
covers the header's entry stack) plus a defensive `ValueLoc::ElidedClosure`
plan arm. Differential + deopt + GC_STRESS tests per shape.

**Step 4 — Envelope phase B: has_ctx materialize-form OSR via adoption.**
- Gate (pre-decode): has_ctx + `escape::analyze` → **decline only
  `all_elidable`** (the elided form is the one unsound case: the interpreted
  prefix may have leaked a real closure over the frame Context through a
  dynamically-dispatched block-arg send the static 7-IV-c proof never saw;
  elided vreg writes would split from it). OSR-only decline — normal-call
  compiles stay elided. Stat `osr_declined_elided_ctx` is the evidence
  collector for ever revisiting (R1 write-through).
- Plan delta: one line — push `(OsrSource::Context, method_ctx_vreg)`;
  resolution is guaranteed FrameSlot (the regalloc pin). The `OsrSource::
  Context` packer arm has existed since S15, never emitted until now.
- **Codegen delta: none.** The materialize prologue's Alloc stays on the
  normal-call path (block 0); the synthetic OSR entry already nil-fills,
  copies buffer→spill homes, reloads residents, and branches to the header —
  which is never block 0 for a compilable has_ctx method (the 9de470b
  loop-header decline is load-bearing here; tripwire T3 pins it). Adoption is
  purely one more copy pair.
- Deopt round trip needs **zero deopt changes**: mid-loop safepoints record
  `CtxLoc::Materialized(FrameSlot)` via the 9de470b liveness keying; deopt
  hands the same adopted Context back by identity; re-OSR re-adopts it
  (idempotent).
- Tests: shared-identity flagship (pre-OSR and post-OSR closures see one C);
  mid-loop trap identity round-trip; double-OSR; elided-decline observed;
  GC_STRESS (old-space adopted C exercises the store barrier).

**Step 5 — Version-churn coupling (H7).** `compile_method_osr` inherits the
current Alive by_key nmethod's version instead of hardwired 0 — today a
trap-retired OSR nmethod's recompile (osr_bci=None) leaves the still-hot loop
re-arming fresh v0s forever, bypassing `MAX_VERSIONS`. Storm test under
DEOPT_STRESS: ≤ MAX_VERSIONS+1 nmethods per key over 100k backedges.

**Step 6 — Hardening.** Debug transfer-buffer verifier (every packed word
nil/smi/valid-heap-oop); 4 new stats surfaced; `gate-s24-l2` justfile recipe
(full stress matrix, RELEASE, PARALLEL).

**Step 7 — Measurement.** deltablue (Step 2 owns it); **new
`world/bench/ctxloop.mst`** — has_ctx method + escaping accumulator closure +
100k-iteration loop called ONCE: only OSR can tier it; this is the envelope's
own payoff gate. richards regression guard. PERF.md records, including the
honest "envelope ≈ 0 on deltablue" note and the `inputsKnown:` → B1/B3 flagship
pointer (per-klass `inputsDo:` inlining; splice-gate relaxations; the
deopt-inside-inlined-NLR-block hazard is that design's real soundness
question).

## 4. Tripwire inventory

T1 Context source resolves FrameSlot (debug assert + release decline) · T2
ctx-vreg pin intact · T3 `method_ctx_vreg.is_some() ⇒ header != block 0` · T4
packed Context non-nil + klass check before any frame mutation (release
decline) · T5 gc-epoch snapshot across pack→invoke · T6 deopt
Materialized-arm klass check · T7 explicit `ValueLoc::ElidedClosure` plan arm
· T8 gate/form agreement post-convert · T9 header entry-stack phantom-free
cross-check (debug assert + release decline) · T10 sub-threshold entered
nmethod key==(k,sel) && Alive · T11 stats surfaced · T12 no safepoint pc ≥
osr_entry_off · T13 storm ≤ MAX_VERSIONS+1 · T14 compile_disabled survives
loop-counter RMWs.

## 5. Rejected alternatives (why, on record)

R1 write-through (forcing materialize on elided has_ctx OSR compiles):
pessimizes every normal call to serve a rare OSR — revisit only on
`osr_declined_elided_ctx` evidence. R2 elided-form OSR via a CtxSlot tag:
unsound per the 7-IV-c dynamic-prefix leak; tag space stays reserved. R3
un-materializing interpreter closures into phantoms: inverse of
`ValueLoc::ElidedClosure` is inexpressible; compiled correctness depends on
phantoms staying phantom. R4 fresh Context at OSR entry: the 9de470b failure
class. R5 OSR-twin nmethods: breaks the one-nmethod-per-key lifecycle. R7
lowering the invocation threshold / per-send code_table lookups: global churn
vs one self-healing hint branch. R8 letting OSR carry deltablue: falsified by
arithmetic (osr_entries=6). R10 clear-hint-on-miss: strips polymorphic
methods. R11 hint bit at 17 / whole-word flag writes: inside the counter
field / clobbers counters. R12 permanent decline bit for elided-ctx OSR: state
for a once-per-10k-backedge cost. R14 attacking `inputsKnown:` in L2:
loop-free — B1/B3 territory. R15 the hint-bit draft of Step 2
(`COUNTERS_HAS_NMETHOD_BIT` + an `over || hint` dispatch branch): superseded
by trigger unification (user call, 2026-07-10) — it was a parallel mechanism
where saturating the existing counter unifies the two profile triggers with
zero new dispatch state; its lapse-to-interpretation-after-invalidation
behavior also contradicted the "the method is now hot" policy.

## 6. Open questions — RESOLVED (2026-07-10)

1. **Priority after Step 2:** envelope first (Steps 3-6) per recommendation;
   re-rank against B1/B3 after Step 2's measurement lands.
2. **Step 1's semantic ride-along:** proceed. The user's premature-optimization
   principle IS the ride-along's effect — loopy NoPermanent methods staying
   disabled is the documented S10-D1 intent; today's re-attempt-forever
   behavior is the accidental unguided compilation.
3. **Step 2 mechanism:** trigger unification (counter saturation), user-decided
   — "the loop counters have detected in a different way that the method
   containing the loop is hot; the method is now hot."
