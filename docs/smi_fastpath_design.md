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

## S2 attempt record (2026-07-25): reverted — the missing-reader problem

The first S2 implementation (commit-skip for known-smi resident vregs +
`emit_s2_spill_stores` before all nine safepoint-creating sites) produced
a perfect arith loop (zero stores/loads in the fast path, ~18 insns/iter)
and passed tier1/gc_jit — then **corrupted the Cocoa GUI boot** (a 2.2TB
allocation request = a garbage value read as a size; deterministic,
`Array class>>with:with:with:with:` per the new stall dossier).

Falsified along the way (each a real constraint on the next attempt):
1. Store filter must mirror BOTH `resolve_frame_loc` disjuncts — interval
   AND `extra_oop_live` exact facts (fixing this cured the
   depth3_deopt tier1 crash, not the GUI).
2. Stores before a vreg's first def plant CALLER callee-saved leftovers
   into nil-filled oopmap slots — the def-before-first-safepoint horizon
   (+ entry-straightline dominance + no-OSR) closes the reasoning hole
   but did NOT cure the GUI.
3. The prim-shim's safepoint is an EMITTER-side push invisible to
   `regalloc.safepoint_positions` — the def-horizon can't see it. Shim
   stores removed. Still not the cure.
4. **The decisive experiment**: stores-everywhere + write-through
   RESTORED = clean boot. So the added stores are harmless; the SKIP is
   the bug — some slot reader survives outside the nine covered sites,
   and MACVM_GC_VERIFY never tripping suggests a direct stale-VALUE read
   (not GC pointer corruption).

**Next attempt needs the reader identified first, not more site
patching**: build a debug-mode consistency checker that, at every
safepoint (and ideally on every slot LOAD), compares an S2 vreg's slot
against its resident register and names the divergent reader's pc. Keep:
the stall-path stack dossier (landed), the nine-site store machinery
design, and the falsification list above. S1+S3 stand alone: arith
35.6 -> 16.8 ms without S2.

## S2 retry (2026-07-25, later): the canary found the reader — landed env-gated

The prescribed checker worked on the FIRST shot. `MACVM_S2_POISON=1`
replaces the skipped write-through with a per-slot canary (a tagged smi
`0xC0DE… | slot<<4`); the GUI-boot failure immediately became decodable:
`requested_bytes = 8 × (canary(slot 6) + tagged 3)` — slot 6 read as an
ARRAY LENGTH (+3-word header) by `Array class>>new:`. **The reader:
`emit_call_send`'s argument marshalling**, whose parallel-move classified
sources by raw `Assignment` (`Spill → Src::Mem`), bypassing the resident
cache and loading spilled args from their slots. Fixed by marshalling
resident vregs from their registers (also simply faster, and x21-x27 can
never alias the x0..x5 destinations, so the shuffle only gets easier).
A full audit found no sibling: every other slot load is resident-first.

Landed env-gated: `MACVM_S2=1` activates the commit-skip; poison mode
stays as the permanent checker; both off = byte-identical emission.
Validation at MACVM_S2=1: release lib 827 + tier1 104 + gc_jit + 7-mode
battery + 75 s GUI/GC_VERIFY soak + poison-clean boot. Interleaved
env-toggled A/B (two runs): **fib −5.3/−8.2%, sieve −2.6/−7.6%,
richards −2.7/0%, dict −5.2/+1.5%**, deltablue flat, alloc +1.5/+2.5%
(the pre-safepoint stores at its hot Alloc slow paths — the one real
cost), **arith FLAT — blocked, precisely**: benchArith's hot code is the
OSR-heal recompile, and (a) OSR bodies are S2-excluded by design, (b)
the HEALED whole-body version's loop vregs fail `known_smi` (census:
half the residents smi=false there — the heal-version IR has def shapes
the all-defs rule poisons). **S2b follow-up: teach known_smi the healed
version's def shapes (run `MACVM_S2_COUNT=1` on a 3-call arith script
and read the v1 IR), and consider S2 for OSR bodies' post-entry defs.**
The def-horizon is entry-straightline only (temps' nil-init defs
dominate; the first_safepoint clamp was retired as needless).

## S2b (same day): the arith blocker was the OSR exclusion — dropped

The heal-pipeline fact that unblocked it: a fully-warm OSR nmethod
(`osr_cold_sends == 0`) serves warm calls FOREVER — heal never replaces
it — so benchArith's hot loop IS its OSR body, and the `!is_osr` guard
excluded exactly that code. The exclusion was needless: BOTH entries
initialize S2 residents (normal entry via the entry-block defs; OSR
entry via the materializer's slot writes + `emit_resident_reloads_at`
(header)). With it dropped, the OSR loop skips the write-throughs of
`s` and `i` (bare `mov` where `mov+str` was) while stack temps keep
theirs (in-loop defs — outside the entry-straightline horizon; widening
that via block-chain dominance, the S3 bound-flow machinery, is S2c).

A/B (env-toggled, 3 rounds): **arith −9.0%** (16.3→14.9 ms — under 2x
of Dart V1), fib −7.8%, sieve −6.2%, alloc/deltablue flat; dict and
richards flip sign across runs (wide thermal bands — re-verify cool
before quoting). Gates at MACVM_S2=1: release lib 827 + tier1 104 +
GC_STRESS/DEOPT_STRESS modes + GUI GC_VERIFY boot. The `!smi` census
also names S2's next coverage frontier beyond S2c: `LoadField`-defined
loop values (stream/ivar loops) are never known-smi — that is inliner/
type-feedback territory, not this analysis.

## S2 default ON (cool-machine verified)

4-round env-toggled A/B at load 1.66: **arith −9.2%, fib −7.2%, sieve
−6.0%; richards −0.1%, dict +1.3%, deltablue +1.2%, alloc +0.4%** — the
wins reproduce cool and the earlier hot-run dict/richards wobbles were
thermal artifacts. Flipped: S2 is on by default; `MACVM_S2=0` opts out
(the A/B lever, byte-identical S1+S3-only emission); `MACVM_S2_POISON=1`
remains the permanent stale-reader checker. Gates at default-ON: release
lib 827 + tier1 104 (goldens unchanged — their resident vregs are
Param-fed, correctly outside S2) + 7-mode battery + GUI GC_VERIFY boot +
debug lib 839.
