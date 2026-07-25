# Budgeted inliner — the jaw (2026-07-25, slice I1 landed)

Every flat census this week (`guard_elision_findings.md`) converged on one
conclusion: the support passes — known-smi, range analysis, load
forwarding, S2 residency, alloc provenance — are TEETH that only bite
when the relevant operations share a compilation unit, and MACVM's
splicer was **depth-1**: a spliced body's own sends always lowered to
plain `CallSend`s. The budget knobs (`InlineBudget::total_bytes`,
`max_depth`, per-recompile-level scaling 300B/4 → 2400B/8) existed,
were level-scaled, tested monotone — and never consumed by any
recursion. Dart 1.24.3's `flow_graph_inliner.cc` (dart124 item 2) is the
reference shape: a count-ranked worklist under depth/size budgets.

## I1 (landed): nested LEAF inlining inside the splice walks

The minimal recursion with maximal soundness leverage: at a send site
INSIDE an already-spliced body (both the non-leaf and CFG walks), after
the fuse ladder declines, a **mono LEAF callee** now splices in place of
the compiled send. Why leaves first: a leaf has no sends → no in-body
safepoints → **no new deopt scope is ever created**. Its only record is
the entry-guard trap, which re-executes the inner send in the ENCLOSING
inlined frame — carried by a tightly-scoped `Translator::
nested_inline_scope` that `try_inline_leaf`'s cold block reads (None for
every root-level splice, so root behavior is byte-identical).

Gates on the decision (`try_nested_leaf`): mono feedback only; never a
primitive; `is_leaf`; `inline_cost ≤ per_call_cost`; the CUMULATIVE
`budget_would_exceed` check (this makes `total_bytes` real at depth ≥ 2);
depth from the enclosing proto's parent chain `+ 1 ≤ max_depth` (makes
`max_depth` real); in the CFG walk, decline when any phantom block-arg is
on the stack (a phantom must never enter a recorded reexecute stack).
Recursion is structurally impossible for leaves (no sends), so no
inline-stack is needed until deeper slices. `budget_commit` charges every
accepted nested splice, same as root splices.

## Measured (4-round interleaved A/B, best-of, vs the S2c tree)

| bench | delta | why |
|---|---|---|
| **alloc** | **−7.8%** | `Association key:value:` constructor's ivar-setter leaves colocate with the alloc — the exact shape `alloc_guard_census` measured as "0 eligible" until inlining colocated it |
| **richards** | **−3.3%** | per-class accessor leaves (`count`, `destination`, …) splice into the spliced task bodies |
| dict | −2.5% | scanFor-adjacent leaves |
| sieve | −2.0% | — |
| deltablue | −1.2% | strength accessors |
| arith / fib | flat | no nested-leaf shapes — correctly untouched |

No regressions. One behavior pin updated: the tier1 nonleaf test
asserted the depth-1 boundary ("inner `self bar` = exactly one compiled
IC site") — it now asserts ZERO sites + two inline deps, and installs
`bar` BEFORE compiling (the nested dep means a post-compile install
correctly invalidates; a Mono IC only ever points at an installed
method in reality).

Gates: debug lib 839/0, release lib 827 + tier1 104/0, stress modes
(GC_STRESS, DEOPT_STRESS ×2 thresholds, JIT-off), GUI GC_VERIFY boot.

## Next slices

- **I2 — nested NON-LEAF/CFG**: a spliced body's mono send on an
  inline-eligible body grafts with a `parent`-chained `InlineSite`
  (the machinery `GraftMode::Block { parent }` already exercises at
  depth 3; extend `GraftMode::Method` with a parent). This is what
  reaches `Dictionary>>at:` → `scanFor:` whole-chain fusion and the
  LoadField-defined loop values the S2 census flagged.
- **I3 — count-ranked ordering**: with M1's poly per-arm counts and the
  invocation counters, rank candidate sites by hotness before spending
  `total_bytes` (today: bytecode order — first-come wins the budget).
- **I4 — recursion cap** (dart's 1) once I2 makes recursion possible.

## I2 landed: nested CFG grafts (parent-chained method grafts)

`GraftMode::Method` now carries `parent: Option<Box<InlineSite>>` (root
splices pass None — byte-identical), and the CFG walk's generic-send
path attempts a nested METHOD graft after I1's leaf attempt declines:
mono + non-primitive + `is_inline_eligible_cfg` + per-call and
cumulative budgets + proto-chain depth + `inline_stack` recursion guard
(the root method and every in-flight nested callee's raw; a candidate
already on it is direct recursion and declines). The graft's own deopt
scopes chain through the parent'd proto; its guard trap re-executes the
inner send in the ENCLOSING frame via `nested_inline_scope` (the same
convention the leaf and CFG cold blocks now share). Segment protocol
copied verbatim from the proven block-graft branch. try_inline_leaf/cfg
self-commit to the budget (I1's double-charge removed).

**Honest measurement: FLAT on the 7-bench suite** (all rows within
noise; deltablue −0.3%). The fire census (`MACVM_S2_COUNT=1`) shows WHY:
7 nested grafts total — deltablue's `stronger:`/`weaker:` and one
`add:` — because this suite's deep chain (`at:` → `scanFor:`) already
fuses at ROOT level (`at:` compiles as its own root and splices
`scanFor:` there). I2 is correct, budget-bounded, live infrastructure
whose fire-rate is the lever I3 pulls: count-ranked ordering + budget
levels decide WHICH sites deserve the graft, and the world corpus
(GUI boot: streams, collections) offers far more chain shapes than the
bench micros. Nonleaf-walk nesting stays leaf-only (no segment protocol
there); its bodies are single-block by construction.

Gates: debug lib 839/0, release lib 827 + tier1 104/0, GC_STRESS +
DEOPT_STRESS ×2, GUI GC_VERIFY boot, both-threshold checksums.

## I3 landed: count-ranked budget pre-allocation

The one-pass translator decides sites in bytecode order, so the ranking
happens BEFORE translation (`rank_site_allowances`, using the Cfg
convert already receives): enumerate the root's non-super sends, keep
in-loop mono non-primitive candidates (loop position is the dominant
tier-1 hotness signal; invocation-count ranking can join later), rank by
(loop-depth DESC, callee cost ASC), and simulate spending `total_bytes`
down the ranking. An approved site's allowance covers its callee's
actual cost (≤ total_bytes/2) and RAISES that site's `per_call_cost`
only — absent/unranked sites are byte-identical to before. The active
site's allowance is published as `allowance_ceiling`, which the nested
leaf/CFG checks max against, so an approved chain's inner grafts can
afford the ride (at: bringing scanFor: and friends along).

Fire census: nested grafts tripled (7 → 20+: `isEmpty`,
`between:and:`, `size`, `privateAt:put:value:`, `checkGrow`, `of:` —
dictionary/collection internals fusing into approved in-loop chains).
Clocks, honestly, across two 4-5-round A/Bs: flat-to-slightly-positive
(deltablue −0.9% consistent; dict −0.7%/−4.5% across runs; an alloc
+4.0% in run 1 did NOT reproduce — noise). The micro suite's hot loops
were already well-served by root-level fusion; the tripled fire-rate's
main beneficiaries are the world corpus's deeper chains, which these
micros don't time. Gates: debug lib 839/0, release lib 827 + tier1
104/0, stress modes, GUI GC_VERIFY boot, both-threshold checksums.

Remaining staged: I4 recursion depth 1; invocation-count ranking joining
loop depth; per-site allowances for NESTED rankings (today the ceiling
is chain-wide).

## Post-I3 profile: where richards/fib actually spend (2026-07-25)

**fib (2.38x behind Dart): the compiled body is 139 instructions, 52 of
them memory ops, for a method Dart compiles to ~15.** Itemized from the
listing (DBG_IR=fib:): (1) the ENTRY GUARD — 8 instructions — runs on
every recursive call although `self fib:` can never fail it (the
receiver IS the already-verified self); x2 calls per activation. (2) the
Param `n` is outside known_smi (all-defs is flow-INsensitive — an arg
may be non-smi) so it reloads + re-guards at each use even after the
first guard passed. (3) nil-init + param-spill + ConstSmi write-throughs
(the S3-crumbs item). Ranked levers: **F1-self-call — a mono site whose
receiver is `self` and whose target is a compiled method calls the
VERIFIED entry directly** (skips the 8-insn guard; applies to all
self-sends incl. the non-spliced recursive ones — the self-devirt proof
already exists, only the call-target choice is missing); **F2 —
flow-sensitive param smi-ness** (after a Param's first passing guard,
later guards on the same vreg are redundant on that path);
**F3 — ConstSmi slot elision** via `ValueLoc::ConstSmi` at recording.

**richards (3.86x): flat unsymbolized-JIT profile + rt_call_primitive at
~5%** — one dominant nmethod cluster (the spliced scheduler chain),
death-by-activation across poly dispatch. F1-self-call shaves its
self-send chains too; the poly processWork dispatch itself is
SameTargetPoly/Dominant territory already built — the residue after F1
should be re-profiled before further guessing.

## F1 landed: proven-self sends call the verified entry

`CallSiteInfo.self_klass: Option<KlassOop>` marks the two root-level
CallSend fallbacks reached with `self_send_target.is_some()` — sends
whose receiver is provably the root method's own `self`, whose klass the
entry guard already verified. Deliberately separate from `static_klass`
(no runtime super-dispatch entanglement: an unpatched F1 site resolves
like any dynamic send). The driver resolves such sites AFTER publish:
the self-recursive case (`lookup(K, sel)` == the method being compiled —
fib) patches the `bl` to THIS blob's own verified entry; an
already-compiled callee patches to its nmethod's verified entry; a
not-yet-compiled non-self callee stays lazy. Patched sites start
`CompiledIcState::Mono` on the direct target (the super-site
convention) and pin a (K, selector) inline dep, so redefinition
invalidates the caller; a recompiled/invalidated CALLEE is safe because
make_not_entrant patches BOTH of its entries to the not-entrant stub.

Measured (two A/Bs, 4+5 rounds): **fib −7.6%/−7.0%** (right at the
16-of-139-insns prediction), **dict −4.0%/−3.8%** (its self-send
chains), richards/alloc/deltablue flat within bands, sieve's one-run
+4.4% did not reproduce. Gates: tier1 104/0, release lib 827, stress
modes, GUI GC_VERIFY boot, both-threshold checksums, debug lib 839/0.

F2 (flow-sensitive param smi-ness) and F3 (ConstSmi slot elision)
remain from the profile; richards' residue wants a fresh profile after
F1's self-chains land.

## F2 landed: flow-sensitive proven-smi guard elision

`proven_smi_positions` — a forward MUST-dataflow (meet = intersection)
over the linearized CFG in the exact per-op position numbering emit and
regalloc share: a vreg is proven at an op when, on EVERY path there, an
earlier tag guard on it passed or a smi-producing def reached it with no
other def in between. `emit_tag_check` skips a side when (pos, vreg) is
proven — the second skip source alongside S1's flow-insensitive
`known_smi`. Guards prove operands on the fall-through only; fail edges
leak the fact harmlessly into trap/slow blocks, which consume none.
The fib validation: v1 drops to exactly the sound minimum of 4 tst —
entry guard, `n`'s FIRST guard, and the two send-result guards (call
results are rightly unprovable). One bug found en route: the fixpoint's
`slot.take()` emptied the slot before the changed-comparison — an
infinite loop the 10-minute timeout caught; compare-then-assign.

Measured (two suite A/Bs + isolated run): **sieve −5.0%/−9.6%**, **dict
−0.6%/−5.6%**, deltablue −0.4/−0.8% consistent; fib +1.3% ISOLATED —
with strictly fewer instructions in its body, that is branch-alignment
luck on razor-thin recursion (8 removed bytes shift every downstream
target), not semantics; arith/alloc drift inside bands. Net
suite-positive. Gates: tier1 104/0, focused debug suites (codecache 63,
deopt 24, send 38, driver 25 — 0-fail), stress modes, fresh GUI
GC_VERIFY boot, both-threshold checksums. Follow-up lever noted:
branch/function alignment padding as a generic stabilizer.
