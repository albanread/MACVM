# The complete performance history of MACVM

*Written 2026-07-26 at commit `fc8232e`. The first commit (`3eb0b3e`,
an empty Rust scaffold) is dated 2026-07-01 — everything below happened
in twenty-four days.*

MACVM is a from-scratch Smalltalk VM in Rust for Apple Silicon: tagged
oops, a generational moving GC, a bytecode interpreter, and a tier-1
template JIT with inline caches, speculative inlining, OSR, and full
deoptimization. This document is the record of how it went from "the
interpreter runs fib" to **beating Eliot Miranda's production Cog VM on
all seven benchmark workloads** and standing within 1.3–3.4× of the
2017 Dart V1 optimizing JIT — one of the fastest dynamic-language VMs
ever shipped — with an outright win on deltablue.

The final scoreboards are at the bottom. The story of how each number
was won is the body.

---

## Part I — Building the machine (July 1–5)

The first five days built the substrate the whole campaign stands on.
None of these sprints chased benchmark numbers; all of them decided how
fast the VM could ever become.

- **S0–S6** (`c93c721`…`f16aebe`): tagged oops with smi arithmetic,
  heap + genesis, the bytecode set and interpreter, sends with inline
  caches, blocks/closures/NLR/`ensure:`, the source compiler, and the
  core library. The S6 interpreter baseline was recorded honestly in
  PERF.md and never gated anything — *tracking, not gating* became a
  standing rule.
- **S7/S7.5** (`17764ed`…): young-gen scavenging with cards and
  generation-validated handles. The first stress discipline appeared
  here: `MACVM_GC_STRESS` found root-scan gaps within a day of the
  collector existing.
- **S8** (`4f223ed`, July 3): full mark-slide-compact GC.
- **S9–S10** (`b0c36ad`, July 3): JASM assembler, the W^X code cache,
  and the tier-1 pipeline — `decode.rs` (bytecode→CFG), `ir.rs`
  (SSA-lite), `regalloc.rs` (linear scan), `emit.rs` (arm64). Two
  fundamental regalloc bugs were found by the first `.mst` gate file,
  setting the pattern: every tier gets a differential gate.
- **S11** (`7ac7b53`, July 4): compiled sends — klass-guard prologues,
  PICs, c2i adapters, DNU, NLR through compiled frames via an
  epilogue-propagated sentinel.
- **S12**: real per-safepoint oop maps and a unified stack walker;
  moving GC under compiled frames. The D8 interpreter/JIT eden bridge
  was deleted for a single-source-of-truth eden.
- **S13** (`e04805b`, July 5): full deoptimization — scope descriptors,
  SIGTRAP uncommon traps, NotEntrant patching, lazy return-address
  redirection, `MACVM_DEOPT_STRESS`. (A detail that pays off twenty
  days later: the deopt materializer shipped with a `ValueLoc::ConstSmi`
  arm, tested but never produced. See F3.)
- **S14** (`564585a`, July 5): type feedback, the inlining cost model,
  leaf and CFG splicing, block inlining through captured temps and NLR,
  customization (per-klass compiles), recompile-on-trap.
- **S15** (`0ed9d65`, July 5): **OSR is live** — hot loops tier up
  mid-run. Richards and DeltaBlue were ported; they immediately flushed
  out real JIT bugs (BUG A/C/D), each closed with a written dossier.

Everything after this point is *making the compiler smarter*, on a
substrate whose GC/deopt/stress discipline never had to be revisited.

## Part II — The first performance wars (July 9–21)

**The IC-stomp fix — richards 16× in one line** (`a2bfd8b`, July 9).
The debugger built in DBG0–3 earned its keep immediately: richards was
mysteriously interpreter-speed, and the trace showed `activate_method`
stomping polymorphic ICs back to mono-compiled on every activation —
endless recompilation. One guard: richards 208→13 ms. The lesson that
diagnosis beats speculation was learned here and never unlearned.

**The closure campaign, S24 A/L/B** (July 9–11). Blocks were the last
interpreter island. A1–A3 compiled closures organically (NLR
origination, `by_block` registry, Context materialization); L1/L2
unified triggers and put OSR under closure-bearing methods (ctxloop
134×); B1–B5 built multi-basic-block block splicing with a
deopt-materializer deferred-fixup phase, then devirtualized
self-receiver block-arg sends. **DeltaBlue: 214 ms → 4 ms (53.5×)**;
richards 34× over the interpreter. `de7e20e` closed the arc by killing
the OSR cold-send deopt storm (sieve 90→9 ms).

**The float fast path** (July 11). An unboxing reducer for method-local
Double math: FP IR ops, mono-Double fuses, the box/unbox cancellation
reducer, deopt-sunk boxing, float-temp promotion with
`DeoptSlot::Double`, and d8–d15 register residency.
**Mandelbrot: 746 ms → 25 ms** with zero allocation in the loop.
Float-kernel inlining was proposed three times and rejected three
times; the discipline of writing down *rejected* designs starts here.

**SIMD, FFI, Accelerate** (July 12–20, interleaved with GUI work):
Float64x2/Float32x4/Int32x4 NEON fuses, FloatArray explicit-NEON
kernels (the rule: `core::arch::aarch64` intrinsics, never hope for
autovectorization), the dlopen/dlsym FFI tier with shape-keyed
trampolines, and vDSP/vForce/dgemm — four orders of magnitude over pure
Smalltalk on matrix multiply.

**One free 2.6×** (`94ac9f8`, July 21): the interpreter was paying for
debug-assert bounds checks in release builds. Found by finally
profiling the release binary instead of reasoning about the source.

## Part III — The yardsticks (July 22–23)

`7c495f1` built the honest harness: microsecond clock, warm = median of
6 ×10-rep batches, best-of-rounds, interleaved A/B against **Cog**
(Squeak 6.0 / OpenSmalltalk) on the same seven checksummed workloads —
arith, fib, sieve, dict, alloc, richards, deltablue.

The first full measurement (July 22, `3a8121d`): **MACVM ahead on all
seven** — the richards loss inverted by the special-selector port
(`==`/`~~`/`not` lowered inline, from the WINVM sister repo) and the
eden-geometry fix (4→32 MiB after `gc_alloc_gap.md` root-caused the
alloc gap: 65% allocation send path, 30% nursery geometry). Frameless
leaf methods (F0–F3, Cog's `needsFrame` idea, arm64-shaped) went
default-ON for another 2–4% on richards; F7 skipped provably-dead
prologue nil-fills (fib 153→135 ms).

The scoreboard section in the README was framed then and still holds:
*a yardstick, not a competition* — Cog runs a full Squeak image; MACVM
runs a small world. The seven workloads are the honest overlap.

## Part IV — The Dart campaign (July 24–25)

The sister repo WINVM had studied the **Dart 1.24.3 (V1)** VM — the
2017 optimizing JIT, Smalltalk-lineage (Lars Bak), and a much harder
yardstick than Cog. Its census of ten portable mechanisms became the
campaign map. Two process rules were set by the user and governed
everything:

1. **"The census has been misleading — just benchmark from now on."**
   Instruction counts had bought −21% instructions for −0.35% time
   (~60:1). Every slice after this ships with an interleaved,
   load-gated, best-of A/B, and any surprising delta is re-run before
   it is believed.
2. **Test in release, focused.** A 25-minute debug suite mid-loop is
   process failure; the gate became `cargo test --release` plus ~30 s
   of focused debug filters, with full-debug reserved for GC-arc
   rewrites.

The slices, in landing order:

**Dead-tail unlock** (`943d896`): the inline-splicer's entry
precondition rejected methods with dead trailing bytecodes — one
relaxed check unblocked accessor splicing everywhere.

**Poly-inlining M1–M3** (`492d329`…`a3405f4`): per-arm counts in the
polymorphic ICs (interpreter-side bumps, reverify carries counts,
count-insensitive recompile fingerprint), count-proven dominant
inlining at any arity, then `GuardKlassIn` + `SameTargetPoly` — one
membership guard in front of a spliced body shared by N receiver
klasses, leaf and CFG legs, with count seeding at IC transitions so
organic warm-up isn't starved. Richards 19.0→17.8 ms.

**Range analysis R1+R2** (`10bc2eb`): induction-bounded overflow-check
and bounds-check elimination (`SmiArithNoOv`). Sieve 2.4→2.1 ms.

**Load forwarding L1** (`624bf56`) + **`Array>>size` intrinsic**
(`fe2ae02`): intra-block redundant load/guard elimination; dict
8.3→6.3 ms.

**The falsification week paid out.** Four censuses in two days came
back negative — F3c dead-slot freeing (premise falsified: freed
0/2047), deopt-liveness rework (58% dead slots, measured flat), CHA
guard elision (unsound here), alloc-provenance (0 eligible sites) — and
`e57e7a7` recorded that they all *converge on the inliner*: "the teeth
need the jaw." Building any of them first would have wasted the week.

**PolyCmpFuse** (`776f3e2`): dict's residual cost was the polymorphic
`=` dispatch chain (Symbol vs smi receivers). Fusing a poly-identity
compare chain took **dict 6.26→3.35 ms (−46%)** and pulled deltablue to
1.9 ms. The naive alternative — repatching c2i adapters in place — was
tried first, hung the VM, and was falsified in an afternoon; the
diagnosis toolkit (c2i census, `MACVM_TRACE=deopt`) became permanent.

**Acquiring the real yardstick** (`2fb60b9`): Dart 1.24.3 arm64
running natively in a Lima VM, all seven workloads ported class-for-class
with identical checksums, interleaved three-way harness. Two corners of
the 2017 arm64 JIT SIGILL on modern Apple Silicon; the workaround flags
can only slow Dart, so **its column is a floor**. First measurement:
gap 1.6–4.8× (folklore said 4–12×), and **MACVM already ahead on
deltablue** — the closure campaign's machinery, vindicated against the
harder yardstick.

**The smi fast path S1–S3** (`1f8edd9`…`1346613`) — arith's version of
the float story, for tagged integers:

- **S1**: `known_smi_vregs` — a poison-by-default all-defs analysis;
  provably-true tag guards vanish (arith's inner loop: 16 `tst` → 1).
  Arith −45%.
- **S3**: loop-bounded `Mul` overflow elision with bound flow down
  single-pred chains — `smulh` disappears from bounded loops. −13%.
- **S2**: the hard one — stop write-through-spilling known-smi loop
  temps at safepoints (registers become authoritative between polls).
  The first attempt corrupted the GUI boot (a 2.2 TB allocation
  request). Four hypotheses were falsified; then a **poison canary**
  (stores write `0xC0DE|slot` instead of the real value; anything that
  *reads* the slot crashes loudly) identified the one true reader —
  call-argument marshalling reading slots instead of residents — in a
  single run. Fixed, verified on a cool machine, defaulted ON
  (`MACVM_S2=0` opt-out), then widened to OSR bodies (S2b) and
  dominance-widened in-loop stack temps (S2c). The S2 family: arith
  −11.3% more.

Composite: **arith 35.6 → ~14.1 ms (−60%)**, from 4.81× behind Dart to
under 2×.

**The budgeted inliner I1–I3** (`c6bbb62`…`850ea32`) — the jaw. The
discovery that started it: `InlineBudget::total_bytes`/`max_depth` were
*vestigial* — declared, level-scaled, tested monotone, never consumed;
the splicer was depth-1. I1 inlined nested **leaves** inside both
splice walks (free deopt story: a leaf has no in-body safepoints, so
its guard trap re-executes in the enclosing frame) — **alloc −7.8%**
(constructor+setter colocation), richards −3.3%. I2 added nested
**CFG** grafts with parent-chained inline scopes and a recursion guard.
I3 added `rank_site_allowances` — a pre-pass that spends the budget on
loop-ranked sites before translation begins — tripling nested-graft
fires. The micro suite was already root-fused (honestly recorded as
flat); the world corpus is the beneficiary.

**The profile that named the endgame** (`5f19fc0`): fib's compiled body
was 139 instructions / 52 memory ops against Dart's ~15. The anatomy
listed three levers, built in order:

- **F1** (`19b56a8`): proven-self sends call the callee's **verified
  entry**, skipping the 8-instruction klass-guard prologue that can
  never fail (the receiver *is* the proven self); self-recursion calls
  its own blob's verified entry. Patched post-publish with proper
  invalidation (make_not_entrant patches both entries). Fib −7%, dict
  −4%.
- **F2** (`038de0f`): `proven_smi_positions` — a flow-sensitive
  must-dataflow (intersection meet) over the shared emit/regalloc
  position numbering; a value that passed a tag guard on the
  fall-through edge needs no second guard downstream. Sieve −9.6%,
  dict −5.6%.
- **F3** (`fc8232e`): vregs whose every def is the same `ConstSmi` go
  **slot-free** — `resolve_frame_loc` finally produces the
  `ValueLoc::ConstSmi` the S13 materializer had waited twenty days for;
  the oop map skips the never-written slot; reloads rematerialize a
  `movz`; call marshalling gained `Src::Imm` (the F1 lesson applied
  preemptively — an immediate can never join a shuffle cycle).
  **Fib −10.6%**, reproduced.

Fib on the day: 138 → 111 ms.

## The final scoreboards (2026-07-26, commit `fc8232e`)

Interleaved, load-gated, best-of-rounds; warm = median of 6 ×10-rep
batches, microsecond clock. Dart = one fresh process per bench with the
2017-arm64 workaround flags (a floor).

> **⚠️ These milliseconds are per 10-rep BATCH.** Every absolute figure in
> this file above this line is. `f3bafb8` later changed the timed region
> to a SINGLE rep, so the next scoreboard's numbers are ~10× smaller for
> the same speed. **Compare ratios across that boundary, never absolute
> ms** — fib 110.0 here and 8.9 there is a unit change, not a 12× win.

| bench | MACVM ms | Cog ms | vs Cog | Dart V1 ms | vs Dart |
|---|---|---|---|---|---|
| arith | 14.2 | 55.2 | **MACVM 3.90×** | 8.1 | Dart 1.81× |
| fib | 110.0 | 191.1 | **MACVM 1.74×** | 62.4 | Dart 1.77× |
| sieve | 1.9 | 3.7 | **MACVM 1.95×** | 0.7 | Dart 2.48× |
| dict | 2.9 | 12.3 | **MACVM 4.29×** | 2.2 | Dart 1.28× |
| alloc | 9.6 | 14.9 | **MACVM 1.55×** | 4.7 | Dart 2.22× |
| richards | 16.5 | 22.0 | **MACVM 1.33×** | 4.9 | Dart 3.36× |
| **deltablue** | **1.8** | 3.6 | **MACVM 1.96×** | 2.5 | **MACVM 1.37×** |

- **vs Cog: all seven, margins 1.33–4.29×** — every row's best margin
  ever recorded.
- **vs Dart V1: the gap is 1.28–3.36×** (July 24 it was 1.6–4.8×;
  folklore before measuring said 4–12×), with deltablue an outright
  1.37× MACVM win.

Selected single-bench journeys, first honest measurement → today:
richards 208→16.5 ms, deltablue 214→1.8 ms, Mandelbrot 746→25 ms,
arith 35.6→14.2 ms, dict 8.3→2.9 ms, fib 153→110 ms, sieve 9→1.9 ms.

## The 2026-08-05 re-measure (commit `435bb8b`)

Ten days on, with the Z-arc intrinsics, the per-arm poly-inlining probes
and the eden-proportional survivors landed, the suite was re-run — and
the harness itself turned out to need a fix first (below).

**Protocol (current):** cold, then the median of 41 SINGLE-rep samples
after 30 untimed warm-up reps; microsecond clock; interleaved A/B rounds,
best-of-3, load-gated. Dart still runs one fresh process per bench with
the 2017-arm64 workaround flags, so its column remains a floor.

| bench | MACVM ms | Cog ms | vs Cog | Dart V1 ms | vs Dart |
|---|---|---|---|---|---|
| arith | 1.394 | 5.123 | **MACVM 3.68×** | 0.789 | Dart 1.77× |
| fib | 8.907 | 18.507 | **MACVM 2.08×** | 6.195 | Dart 1.45× |
| sieve | 0.174 | 0.357 | **MACVM 2.05×** | 0.070 | Dart 2.66× |
| dict | 0.254 | 1.011 | **MACVM 3.98×** | 0.225 | Dart 1.27× |
| alloc | 0.577 | 0.713 | **MACVM 1.24×** | 0.351 | Dart 1.66× |
| richards | 1.067 | 2.209 | **MACVM 2.07×** | 0.488 | Dart 2.18× |
| **deltablue** | **0.136** | 0.277 | **MACVM 2.04×** | 0.233 | **MACVM 1.70×** |

*(ms per rep. The MACVM column is the Cog leg's; the Dart leg measured
MACVM independently and agreed to <1% on five rows — arith 1.400,
richards 1.063, deltablue 0.137 — see the dict caveat below. Each ratio
is a same-round pair, which is the only meaningful comparison.)*

- **vs Cog: still all seven, 1.24–3.98×.** Against the July 26 stamp the
  margins are broadly up: richards 1.33→2.07×, fib 1.74→2.08×, deltablue
  1.96→2.04×; dict and arith eased slightly (4.29→3.98, 3.90→3.68) and
  alloc is the thin row at 1.24×.
- **vs Dart V1: behind on six (1.27–2.66×), deltablue an outright
  1.70× win.** Five of seven rows improved: richards 3.36→2.18×, alloc
  2.22→1.66×, fib 1.77→1.45×, dict 1.28→1.27×, deltablue 1.37→1.70×.
  arith is flat (1.81→1.77×) and sieve regressed (2.48→2.66×). Dart's own
  numbers were unchanged from July, so those deltas are real.

**Reproducibility.** Run twice at different machine loads (2.66 and
1.36): every vs-Cog ratio moved ≤2.5%, five of seven vs-Dart ratios ≤5%,
and the external controls repeated almost exactly (Cog arith 5.035→5.123,
Dart fib 6.203→6.195).

**Caveat — `dict` is the noisy row.** 1.09→1.27× vs Dart between runs, and
its two independent MACVM measurements in one run differ 12% (0.254 vs
0.285). At ~0.25 ms it has the least headroom over timer and scheduler
noise; treat a dict move under ~15% as noise. Both runs were taken on a
machine in light interactive use, not an idle one.

### The harness bug this run exposed

`f3bafb8` moved the two Smalltalk sides (`cog-bench.mst`, `cog-bench.st`)
from a 10-rep timed batch to a single timed rep, but left
`scripts/dart-bench.dart` on the old batch. Because the reduce scripts
treat every VM's `warm_us=` identically, MACVM was reported per REP and
Dart per TEN: **every Dart ratio was inflated 10× for ten days**, and the
first re-run printed a triumphant, entirely false "MACVM 5.6–18× faster
than Dart".

The tell was **treating Cog and Dart as external controls**: Cog's arith
had apparently improved 55.2→5.0 ms, impossible for a VM this project
never touches, while Dart's fib held at 62.4→62.3 — the fixed reference
that localised the fault to the Smalltalk sides. Dividing the broken
Dart column by 10 predicted arith 1.84×; the corrected run measured
1.84×, accounting for the whole discrepancy.

`435bb8b` restores parity (`kWarm=30`/`kSamples=41`, one rep timed),
prints ms to 3 decimals — at one decimal the sub-ms rows rendered as
0.1/0.2, a single significant figure — and rewrites both footers, since
the stale "median of 6 ×10-rep batches" line is precisely what made the
mismatch invisible. **Lesson, now standing: sanity-check any large
movement against the unchanged rival's column before believing it.**

## Part V — How the numbers were won (method, not luck)

1. **Benchmark, don't estimate.** Interleaved A/B, load gate < 3,
   best-of, and *re-run any surprise before believing it* — the harness
   caught at least four phantom regressions (one-run +4%s that vanished
   on confirmation) and one real oddity (F2's fib +1.3% with *fewer*
   instructions — branch-alignment luck, documented, later reclaimed by
   F3). Instruction counts are not time: −21% instructions once bought
   −0.35% (~60:1).
2. **Falsify before building.** Four censuses (F3c, deopt-liveness,
   CHA, alloc-provenance) were run *as censuses* before any
   implementation; all four came back negative and pointed at the
   inliner. The one time the rule was skipped (adapter repatching) the
   VM hung and the afternoon was spent falsifying it anyway.
3. **Make bugs identify themselves.** The poison canary turned S2's
   "somewhere, something reads a stale slot" into a crash at the exact
   reader in one run. The stall dossier prints the Smalltalk stack at
   heap exhaustion. `MACVM_TRACE=deopt` names deopt storms by
   `Klass>>selector`.
4. **Stress differentially, in release.** Every slice gates on the
   7-checksum suite at two thresholds, both GC_STRESS flavors,
   DEOPT_STRESS, and the tier1 differential battery — in minutes, not
   half-hours. Full-debug suites are reserved for GC-arc rewrites.
5. **Write down the rejections.** Scoped catch, float-kernel inlining
   (×3), call-path q-save/restore, adapter repatching, the VMapp relay
   — all recorded with reasons, none relitigated.
6. **Steal honestly.** Cog contributed the yardstick, `needsFrame`, and
   send-machinery portability studies; WINVM (the x64 sister) traded
   special selectors, nursery geometry, and the dart124 census both
   ways; Dart 1.24.3 contributed the map of what a great V1-era JIT
   spends its instructions on. Every port was re-measured here before
   it was believed here.

## What remains (ranked, from the standing docs)

- **sieve 2.66× vs Dart** — now the widest row (it regressed slightly
  while the others closed): array inner loops. LoadField loops are
  inliner+residency territory.
- **richards 2.18× vs Dart** — was the widest at 3.36×; the Z-arc and
  F1–F3 waves took a third off it. Activation cost smeared over
  polymorphic sends. I4 (recursion depth 1) and invocation-count ranking
  joined with loop-depth in the inliner budget.
- **alloc 1.66× vs Dart, 1.24× vs Cog** — the allocation path; the
  eden-proportional survivors bought most of the July gap back, and it is
  now the thinnest margin over Cog.
- **Branch/function alignment padding** — F2's fib anomaly proved
  layout luck is worth ±1%; make it deterministic.
- **Recording-level trap liveness** — the last S2c stack-temp pins are
  loop-head trap extras, a recording-granularity fix.
- The interpreter half of every story: threading, superinstructions —
  unexplored because the JIT kept paying better.

*Twenty-four days. One machine, one fan-less MacBook Air, every number
in this file reproducible from `scripts/cog-bench.sh` and
`scripts/dart-bench.sh` at the stamped commits.*
