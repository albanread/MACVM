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
