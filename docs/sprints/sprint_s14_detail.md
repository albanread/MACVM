# Sprint S14 — Type feedback, inlining, customization

The point of the lineage: the tier-1 compiler reads receiver-type feedback out
of interpreter IC side tables and compiled PIC stubs, inlines within
level-scaled budgets, customizes nmethods per receiver klass, inlines blocks
and elides their Contexts, guards speculations with klass tests backed by
S13 uncommon traps, and drives the whole loop with Strongtalk's recompilation
policy (caller preference, levels 1–4, version cap, effectiveness check).
Implements SPEC §8.1 (policy), §8.4 (feedback + inlining), the customization
half of §8.2, and populates §8.6 dependencies. Evidence:
`reference-vm-analysis.md` §2.2–2.4 (Self feedback/recompilation), §3.4
(Strongtalk compiler).

## Prerequisites

- **S3/S5** — interpreter IC side tables with the full state lattice
  (SPEC §4.3): heap `Array`, stride 4:
  `[sel][argc][guard: klassOop|nil|POLY|MEGA][target]`; POLY target =
  `[k1,m1,k2,m2,…]` (≤4 pairs).
- **S11** — compiled ICs + PIC stubs in the code cache; `CodeTable` keyed
  `(KlassOop, SymbolOop)`; verified vs. checked entry points (SPEC §8.2);
  klass-guard prologue.
- **S12** — safepoints with oop maps; spill-at-safepoint rule.
- **S13** — `ScopeDescRecorder` with `SenderLink` + `CtxLoc::Elided`
  (format complete, unused until now); `brk #0xDE00` uncommon traps;
  `DependencyIndex::record`; `trap_counts` + `UNCOMMON_TRAP_LIMIT`;
  frame materialization incl. Elided-context allocation (M6).
- Compiler pipeline S10 shape (SPEC §8.3): decode → feedback → inline → opt →
  lower → regalloc → emit. S14 fills the `feedback` and `inline` stages.

## Deliverables

- `src/compiler/feedback.rs` — `SiteFeedback`, IC/PIC readers, profile
  snapshot + hash.
- `src/compiler/inline.rs` — budgets, cost model, the inliner (methods,
  blocks, polymorphic dominant case), recursion/depth caps, scope-tree
  bookkeeping feeding the `ScopeDescRecorder`.
- `src/compiler/escape.rs` — closure escape analysis + Context elision.
- Guard insertion + dominator-local redundant-klass-test elimination hook-up
  (the opt stage already exists; this feeds it guards).
- `src/runtime/recompile.rs` — trigger plumbing, caller-preference stack walk,
  levels, version cap, effectiveness check; nmethod invocation counters.
- PIC stubs gain a per-entry hit counter word (compiled sites only).
- `MACVM_TRACE=ic,jit` extended: per-compilation inlining log (`-v` ladder
  visibility, SPRINTS S14 gate); stats: `contexts_allocated`,
  `compilations_by_level[4]`, `recompile_declined_ineffective`.

## Design

### Data structures

#### SiteFeedback — one send site's observed receivers

```rust
// src/compiler/feedback.rs
#[derive(Clone, Debug)]
pub enum SiteFeedback {
    /// IC still empty: the site never executed. Compile it as an uncommon
    /// trap (Self's lazy cold path, SPEC §8.4).
    Untaken,
    Mono { klass: Handle<KlassOop>, method: Handle<MethodOop> },
    /// Pairs ordered by count when counts exist (compiled PIC), else by
    /// first-seen order (interpreter POLY array — count = None).
    Poly { cases: Vec<FeedbackCase> },
    Mega,
}

#[derive(Clone, Debug)]
pub struct FeedbackCase {
    pub klass: Handle<KlassOop>,
    pub method: Handle<MethodOop>,
    pub count: Option<u32>,
}

/// Read the feedback for send site `ic_index` of `method`. Source priority:
/// 1. if `prev` (the nmethod being replaced) has a compiled IC/PIC for the
///    site, read it (richer: has counts);
/// 2. else the interpreter IC side table entry.
pub fn read_send_site(
    vm: &VmState, method: MethodOop, ic_index: u16, prev: Option<NmethodId>,
) -> SiteFeedback;
```

Reading is trivial by construction (SPEC §4.3): interpreter ICs are heap
arrays; decode `guard` (nil → Untaken; klassOop → Mono; POLY smi → walk the
pairs array; MEGA smi → Mega). Mono targets may be nmethod ids (smi handles) —
resolve to the underlying MethodOop via `CodeTable`. Compiled PIC stubs are
our own S11 format; S14 appends one u32 counter word per entry, bumped by the
stub itself (`ldr/add/str` on a data word co-located with the stub — no
tearing concerns, single-threaded VM). Interpreter POLY arrays stay count-free
(stride pinned by SPEC §4.3) — see SPEC-QUESTION at end.

**Histogram semantics**: `Poly.cases` with counts → dominant = highest count;
without counts → dominant = first-seen (`cases[0]`), a deliberate v1
approximation noted in the inliner.

#### Profile snapshot (for the effectiveness check)

```rust
/// Canonical digest of "what the feedback said" at compile time: for each
/// send site, the sorted set of receiver klass identity hashes (values, not
/// addresses — stable across GC) plus the site's lattice state.
pub struct ProfileSnapshot(pub u64 /* FNV-1a over the canonical encoding */);

pub fn snapshot_profile(vm: &VmState, method: MethodOop, prev: Option<NmethodId>)
    -> ProfileSnapshot;
```

Stored in the nmethod header (one u64, `profile_hash`). Counts are **excluded**
from the digest — only klass *sets* matter; two profiles that differ only in
counts would produce the same inlining decisions at the same level.

#### Budgets and cost

```rust
// src/compiler/inline.rs  — all values are tunables (SPEC §8.1/§8.4 style)
pub struct InlineBudget {
    pub per_call_cost: u32,   // max cost of a single inlinee
    pub total_bytes: u32,     // cumulative inlined bytecode per compilation
    pub max_depth: u32,       // inline-chain depth
}

pub fn budget_for_level(level: u8) -> InlineBudget {
    match level {                        // level 1..=4 (SPEC §8.1)
        1 => InlineBudget { per_call_cost: 30,  total_bytes: 300,  max_depth: 4 },
        2 => InlineBudget { per_call_cost: 50,  total_bytes: 600,  max_depth: 6 },
        3 => InlineBudget { per_call_cost: 80,  total_bytes: 1200, max_depth: 8 },
        _ => InlineBudget { per_call_cost: 120, total_bytes: 2400, max_depth: 8 },
    }
}

/// Cost model: callee bytecode length in bytes, with discounts.
pub fn inline_cost(callee: MethodOop) -> u32;
```

Cost rules (evaluate top to bottom, first match wins):

| Callee shape | Cost |
|---|---|
| `primitive ≠ 0` and bytecode is only the failure fallback (smi `+`, `at:`, …) | 4 |
| Accessor (`push_instvar; return_tos`) or quick return (`push_*; return_tos`, `return_self`) | 2 |
| Otherwise | `bytecode_len_bytes` |

Modifiers applied to the *budget*, not the cost:

- **Block-argument bonus**: if the call site passes ≥1 literal block
  (`push_closure` feeding this send's args in the caller's bytecode),
  `per_call_cost × 2` for this site — inlining the callee is what unlocks
  inlining its block arguments and thus Context elision, the single biggest
  win in Smalltalk code (SPEC §8.4).
- **Recursion cap**: a MethodOop already on the current inline chain is not
  inlined again (`MaxRecursionInline = 1` appearance — i.e. no unrolling).
- **Trap veto**: if `vm.trap_counts` shows this (nmethod-being-replaced, site)
  crossed `UNCOMMON_TRAP_LIMIT`, the failed assumption is not re-speculated:
  Untaken becomes a real send, a dominant-case guard becomes the poly slow
  path — this is how S13's counters close the loop.

#### The inliner

```rust
pub struct Inliner<'c> { /* scope tree, budget ledger, chain, recorder */ }

pub enum InlineDecision {
    Inline { callee: Handle<MethodOop>, guard: GuardKind },
    DominantWithSlowPath { case: FeedbackCase, rest_go_to_call: bool },
    Call,          // real send (compiled IC, S11)
    Trap,          // untaken site → brk #0xDE00 (reexecute=true)
}

pub enum GuardKind {
    None,          // receiver klass statically known (self after entry check,
                   // literal, result of an inlined allocation, prior guard)
    KlassTest,     // cmp klass, pool-literal; b.ne → uncommon trap
    SmiTest,       // tst low bits (smi receivers)
}

impl<'c> Inliner<'c> {
    pub fn decide(&mut self, site: &SiteFeedback, callee_hint: Option<MethodOop>,
                  has_block_args: bool) -> InlineDecision;
    /// Inline `callee` at the current position: registers a new scope with
    /// the ScopeDescRecorder (SenderLink { sender, sender_bci, pending_stack }),
    /// splices the callee's decoded CFG, maps its slots to fresh vregs, and
    /// records a dependency (receiver_klass, selector).
    pub fn inline_method(&mut self, ...) -> ScopeId;
}
```

#### Recompilation

```rust
// src/runtime/recompile.rs
pub const MAX_RECOMPILATION_LEVELS: u8 = 4;    // SPEC §8.1 (Strongtalk)
pub const MAX_VERSIONS: u8 = 3;                // SPEC §8.1
pub const NMETHOD_COUNTER_LIMIT: u32 = 25_000; // tunable; interpreter limit
                                               // stays 10_000 (SPEC §5.3)
pub const POLICY_WALK_FRAMES: usize = 10;      // SPEC §8.1 (tunable)

/// From the interpreter (MethodOop.counters overflow) — SPEC §5.3.
pub fn trigger(vm: &mut VmState, receiver: Oop, method: MethodOop);
/// From a compiled prologue counter overflow.
pub fn trigger_compiled(vm: &mut VmState, nm: NmethodId);

struct Recompilee { method: Handle<MethodOop>, receiver_klass: Handle<KlassOop>,
                    level: u8, prev: Option<NmethodId> }
fn pick_recompilee(vm: &VmState, hot: &Recompilee) -> Recompilee; // caller walk
fn recompilation_effective(vm: &VmState, prev: NmethodId) -> bool;
```

nmethod header additions: `counter: u32`, `counter_limit: u32`
(prologue: load counter, add 1, store, compare against `counter_limit`,
`b.lo` continue, else call `trigger_compiled` stub; **disabling counters =
store `u32::MAX` into `counter_limit`** — no code patch needed), and
`profile_hash: u64` (above). See SPEC-QUESTION at end (§8.2 header lists no
counter field).

### Algorithms

#### A1. Feedback annotation (pipeline `feedback` stage)

For every send node in the decoded CFG: `read_send_site` → attach
`SiteFeedback` to the node. Additionally propagate **static receiver klasses**
forward (SPEC §8.3 "redundant klass-test elimination"): `self` has the
customized klass after entry (see A3); literals have known klasses; the result
of inlined `basicNew` is the instantiated klass; a value that passed a
`KlassTest` guard keeps that klass along the guarded path (dominator-local).
Static knowledge beats feedback: `GuardKind::None`.

#### A2. Inlining walk

Depth-first over send nodes of the root method, then recursively into inlined
bodies (work-list, budget ledger shared):

1. `Untaken` → `Trap` (record site `reexecute=true` at the send bci; the trap,
   when hit, re-executes the send interpreted, populating the IC for the
   *next* compilation — Self's lazy cold path).
2. `Mono { klass, method }`:
   - if `inline_cost(method) ≤ per_call_cost`(× block bonus) and ledger/depth/
     recursion allow → `Inline` with `GuardKind` per static knowledge
     (`None` if receiver klass statically = klass; `KlassTest`/`SmiTest`
     otherwise, cold side = uncommon trap).
   - else → `Call` (S11 compiled IC; still customizes the callee via
     CodeTable on first miss).
3. `Poly { cases }` with a **dominant** case (count ≥ 80% of summed counts
   *(tunable)*; countless interpreter POLY: `cases[0]` treated as dominant
   only if `cases.len() == 2` — first-seen dominance is too weak beyond that):
   → `DominantWithSlowPath`: guard klass test, `b.ne slow`; inlined dominant
   body; `slow:` a real compiled send (NOT a trap — the other receivers are
   known-taken, trapping would deopt-storm; SPEC §8.4 "poly with a dominant
   class → inline dominant case behind a klass guard + slow-path send").
4. `Poly` flat, `Mega` → `Call`.
5. Every `Inline`/`DominantWithSlowPath` records
   `DependencyIndex::record(receiver_klass, selector, new_nm)` — the guard
   assumed `lookup(receiver_klass, selector)`’s result (SPEC §8.6). Emitted
   into the nmethod `deps[]` as pool-index pairs (S13 layout).

Inlined scope splicing: callee args map to the vregs holding the send's
operands (no stores); callee temps get fresh vregs with canonical spill homes
(S12 rule — every interpreter-visible entity of every inlined scope has a
frame slot in the ONE physical frame, so S13 `ValueLoc::FrameSlot` covers
depth-N chains with zero format work). `return_tos` in the callee becomes an
assignment to the send's result vreg + branch to the merge block.

#### A3. Customization (SPEC §8.2)

Compilation is always **for a receiver klass K**: `self` sends dispatch
against K statically (lookup at compile time, no guard — the entry-point klass
check already proved it), instvar offsets come from K, and the nmethod is
keyed `(K, selector)` in `CodeTable`.

Reuse vs recompile decision at dispatch/trigger time:

| Situation | Action |
|---|---|
| IC miss on (K, sel); `CodeTable[(K, sel)]` has an Alive nmethod | **reuse** — patch/extend the IC to it (S11 miss protocol); no compilation |
| Same, but only nmethods for other klasses K' exist | **compile fresh for K** (customization = one nmethod per concrete receiver klass, even for a method inherited from a shared superclass — the same MethodOop compiles many times) |
| Trigger fires on an Alive nmethod at level L < 4 | recompile at L+1 (same key); old nmethod → NotEntrant after the new one is installed in CodeTable (S13 D1 machinery, minus the dependency walk); `version += 1` carried into the new header |
| Trigger fires, `version == MAX_VERSIONS` | no recompile; disable counters (`counter_limit = u32::MAX`) — the thrash cap |
| Trigger fires, `recompilation_effective == false` | no recompile; disable counters (A5) |

#### A4. Recompilation policy — caller preference (SPEC §8.1)

`trigger(receiver, hot_method)` (and `trigger_compiled`, which resolves its
nmethod's key first):

1. Build the candidate: `hot = Recompilee { method, klass_of(receiver),
   level: 1 or prev.level+1, prev }`.
2. **Caller walk**: walk up to `POLICY_WALK_FRAMES` frames (mixed
   ProcessStack + native frames via the S12 walker). At each caller frame,
   if the *callee we came from* is trivially small
   (`inline_cost ≤ budget_for_level(1).per_call_cost`) then the caller, not
   the callee, is the better compilee (compiling it inlines the hot little
   method — Self's `Recompilation` walk, analysis §2.3). Keep ascending while
   that holds; stop at a frame that is already compiled at max level, at a
   megamorphic boundary (the IC that dispatched to us is MEGA — inlining
   through it is impossible), or at the walk cap. Pick the highest frame
   reached.
3. Version/effectiveness gates (table above), then compile **synchronously**
   (SPEC §8.1) for the recompilee's receiver klass; install in CodeTable;
   patch the triggering IC to the new nmethod.
4. If the recompilee was an existing nmethod: new nmethod *inherits* the old
   one's send-site knowledge implicitly by reading the old compiled ICs/PICs
   as the feedback source (`prev` in `read_send_site` — Self's
   `forwardLinkedSends` intent without pointer surgery).

**Counter interplay** (SPEC §5.3 + this sprint): interpreter bumps
`MethodOop.counters` per activation → first compile at level 1. Once an IC
dispatches to an nmethod, that path stops bumping the method counter (the
interpreter never activates the method for that receiver klass again); the
nmethod's own prologue counter (limit 25,000) drives escalation to levels
2–4. Level-4 nmethods are emitted with `counter_limit = u32::MAX` from birth
(no trigger stub call — the check still exists but never fires; keeping the
check uniform costs 3 instructions and avoids a second prologue shape).
The interpreter's *loop* counter path is S15 (OSR); until then loop-counter
overflow in an interpreted method triggers a plain method recompile of the
enclosing method (existing S10 behavior, unchanged).

#### A5. Effectiveness check (stop the thrash — Self's `checkEffectiveness`)

When `trigger_compiled` fires on nmethod `prev` (it wants level+1):

1. `current = snapshot_profile(vm, prev.key.method, Some(prev))` — reads
   prev's *live* compiled ICs/PICs.
2. If `current == prev.profile_hash` (the profile it was compiled from):
   recompiling would see the same klass sets and make the same decisions —
   **concretely: the new nmethod's send-site profile would equal the old's**,
   so nothing can improve. Do not compile. `prev.counter_limit = u32::MAX`
   (counters disabled), bump `stats.recompile_declined_ineffective`.
3. One exception overrides the decline: if any trap site of `prev` crossed
   `UNCOMMON_TRAP_LIMIT`, recompile anyway (the *assumption set* changes even
   though the profile hash may not — trap vetoes alter decisions).

This, plus `MAX_VERSIONS`, is the two-layer defense against the
recompile-forever loop.

#### A6. Guard insertion & the trap side

`GuardKind::KlassTest` lowers to: load receiver's klass word
(`ldr xk, [xrcvr, #klass_off-1]` after a smi-tag check if the receiver could
be smi), `cmp` against the expected klassOop materialized from the literal
pool, `b.ne cold`. `cold:` is `brk #0xDE00` with a recorded SafepointState
(`reexecute=true`, bci of the guarded send, full pre-send operand stack,
`kind=UncommonTrap`). `SmiTest` = `tbnz xrcvr, #0, cold` (tag 00 ⇒ test bit 0
and bit 1 — use `tst xrcvr, #3; b.ne cold` per SPEC §2.1's 2-bit scheme).
Guard *dedup* is the existing S10 opt stage: a guard dominated by an equal
guard on the same vreg is deleted (dominator-local, SPEC §8.3).

#### A7. Block inlining & Context elision (SPEC §8.4 — the payoff)

Definitions: home scope H (a method or enclosing block being compiled),
literal block B created by `push_closure` at a site s in H.

**Step 1 — inline the block bodies.** When an inlined callee sends
`value/value:…` to a parameter that is bound (via the dataflow of the
compilation unit) to a specific `push_closure` site s, splice B's CFG at that
send (a new scope: `is_block=true`, `SenderLink` to the scope containing the
`value` send). B's arg/temps get vregs like any inlinee. `nlr_tos` inside an
inlined B: if H (its home) is a scope of this same compilation unit, lower to
"assign result vreg of H + branch to H's epilogue"; **decline inlining B if
any scope on the path from B's inlined position to H has ensure-marker
potential (contains an `ensure:`/`ifCurtailed:` send)** — unwinding
inside one physical frame can't run ensure handlers, so those cases stay
un-inlined (real closure, real NLR machinery). If H is *not* in the unit,
B cannot be inlined at all (its NLR needs a real home_frame_ref).

**Step 2 — escape condition (precise).** A closure-creation site s is
**elidable** iff every use of its value in the unit's dataflow is one of:

  (a) receiver of a `value…` send that was inlined in Step 1, or
  (b) dead (no use).

Any other use makes s escaping: stored anywhere (`store_instvar_pop`,
`store_global_pop`, `store_ctx_temp_pop`, `at:put:`-style sends), passed as a
*non-receiver* argument to any send (inlined or not — the callee might store
it), returned (`return_tos`/`nlr_tos`/`block_return_tos` of s's value),
captured by another `push_closure`, or receiver of any non-`value` send.
Escaping sites emit the normal `push_closure` allocation.

**Step 3 — temp promotion.** A captured temp t of H (context-allocated by the
source compiler, SPEC §4.5) becomes a plain vreg iff **every** block that
captures t has **all** of its creation sites elidable (i.e. *all activations
of those blocks are inlined into the home scope's compilation*). Then reads/
writes of t's ctx slot in H and in the inlined block bodies become vreg
moves.

**Step 4 — Context elision.** H's Context is elided iff ALL of H's ctx temps
were promoted in Step 3. Then: no allocation at entry
(`stats.contexts_allocated` unaffected — the S14 gate's observable), and H's
scope records `CtxLoc::Elided { temps: [promoted temps' ValueLocs] }` so S13
deopt can rebuild a real Context (materializer M6). If *some* temps promote
but others don't, the Context is still allocated with its full layout
(slot indices are compile-time-fixed by the source compiler) and only the
escaping temps are written through it; promoted temps still list their vreg
homes in… **no** — partial promotion is unsound if any non-elided block reads
a promoted temp's slot. Rule: promotion is per-temp but a temp is promotable
only under Step 3's *all-capturing-blocks-elided* condition, which exactly
guarantees no surviving reader/writer of its slot. Partially-elided contexts
are therefore sound; deopt records `CtxLoc::Materialized` (the real Context)
and the promoted temps appear ONLY as ordinary… they were ctx temps, the
interpreter will look for them in the Context. **Resolution (pinned): if any
temp of H fails promotion, elide nothing for H** — all ctx temps stay in the
materialized Context and blocks write through it. Per-temp partial elision is
a later optimization; v1 keeps deopt simple: a scope's ctx is entirely
Elided or entirely Materialized.

**Step 5 — deopt interplay.** A trap anywhere inside an inlined block
materializes the full chain (S13 M3): the home method's frame is rebuilt
*first* (with a freshly allocated Context if elided), then each inlined scope
down to the block's frame — whose materialized closure/context links are real,
so a subsequent interpreted `nlr_tos` unwinds through the ordinary NLR path
into the home frame that deopt just built. This is why uncommon-trap sites
inside inlined blocks resume correctly in the HOME method's frame chain: the
chain *is* materialized, in order, from one physical frame.

### Layer boundaries

| Module | May touch | Must not touch |
|---|---|---|
| `compiler/feedback.rs` | heap IC arrays (read-only), PIC stubs via a `codecache` read API, CodeTable (read) | patching, allocation |
| `compiler/inline.rs`, `escape.rs` | decoded CFGs, ScopeDescRecorder, DependencyIndex (record), budget ledger | ProcessStack, signals |
| `runtime/recompile.rs` | stack walker (read), CodeTable, compiler entry point, nmethod headers (counters/state via codecache API) | direct code writes (delegates to `codecache::patch`) |

## Implementation order

1. `feedback.rs`: interpreter-IC reader + `SiteFeedback` + unit tests against
   hand-built IC arrays. PIC counter word + compiled reader.
2. Feedback stage wired into the pipeline (annotation only, no behavior
   change); `MACVM_TRACE=jit` prints per-site feedback.
3. Guards + trap lowering for `Untaken` sites (first speculative codegen;
   deopt-stress exercises it immediately).
4. Method inlining: cost model, budgets, `Mono` case, scope registration,
   dependencies. Gate: fib/sieve improvement visible.
5. Customization: compile-per-klass on IC miss for inherited methods; static
   `self`-send devirtualization.
6. `DominantWithSlowPath` poly inlining.
7. Block inlining (Step 1) then escape analysis + Context elision (Steps 2–4)
   with `CtxLoc::Elided` emission; `contexts_allocated` stat.
8. `recompile.rs`: nmethod counters, levels, caller walk, version cap,
   effectiveness check; `-v` ladder logging.
9. Full stress matrix hardening (SPRINTS S14 gate: all three stress modes
   combined).

## Pitfalls

- **The effectiveness check is not optional.** Without A5, a megamorphic or
  budget-capped hot method recompiles at the same level forever (Self shipped
  `checkEffectiveness` for exactly this). The version cap alone is too blunt:
  it burns all 3 versions on identical code first.
- **Poly slow path must be a call, not a trap** — the non-dominant receivers
  are *known to occur*; a trap there deopt-storms until `UNCOMMON_TRAP_LIMIT`
  bails the whole nmethod out.
- **Trap veto (budget table)**: re-speculating an assumption whose trap
  counter already overflowed recreates the deopt storm one level up. Feedback
  must consult `trap_counts` keyed by the *previous* nmethod's sites.
- **Elision soundness lives in Step 2's use-list** — the subtle escapes are
  "closure passed as non-receiver argument to an *inlined* callee" (the
  inlined body might store it; the analysis must follow the value into the
  spliced CFG, where it becomes a normal vreg with visible uses — follow it,
  don't special-case it) and "closure captured by another closure".
- **NLR × ensure:**: the decline rule in Step 1 is load-bearing. An inlined
  NLR that jumps over a scope which would have pushed an ensure marker
  silently skips the handler. When in doubt, don't inline the block.
- **All-or-nothing per-scope elision (Step 4 resolution)** — partial elision
  with a live Context reader is a heap-corruption-grade bug; the pinned rule
  makes the analysis result binary per scope.
- **Deopt metadata for inlined scopes**: every inlined callee's MethodOop and
  every guard's klassOop must be in the nmethod oop pool (S13 rule: no raw
  oops in blobs). The inliner adds pool entries as it splices.
- **Customized nmethod ≠ method truth**: `CodeTable` entries key on
  (K, selector); redefining the *inherited* method in a superclass must
  invalidate all K-customized nmethods — that is exactly S13 D2's super-chain
  rule; the inliner must record the dependency against the **receiver klass
  K** (lookup start), not the holder, or D2 can't see it.
- **Interpreter POLY dominance is a guess** (no counts): keep the
  `cases.len() == 2` restriction until compiled-PIC counts take over on
  recompile; measure before loosening.
- **Counter prologues and level 4**: don't emit a second prologue shape;
  `counter_limit = u32::MAX` keeps one code path and lets the effectiveness
  check "disable" counters without touching code (no icache flush).

## Interfaces for later sprints

- `budget_for_level`, `Inliner` — S15 OSR compilation calls the same pipeline
  with one extra parameter (`osr_bci: Option<u16>`); no inliner changes.
- `trigger`/`trigger_compiled` — S15 adds `trigger_osr` beside them, sharing
  `pick_recompilee`'s gates (version cap, effectiveness).
- `snapshot_profile`/`ProfileSnapshot` — S15's PERF work reads
  `stats.recompile_declined_ineffective` and the ladder logs.
- `contexts_allocated` stat — S14's own gate and S15's PERF.md both consume.

## Out of scope

- Self-style **splitting** (analysis §2.4) — S19 stretch (SPEC §8.4 Δ).
- Inlining database persistence across runs (Strongtalk `inliningdb`) — not
  planned for v1.
- OSR entries in inlined loops — S15.
- Per-temp partial Context elision (Step 4 resolution) — later optimization.
- Interpreter POLY IC counters — see SPEC-QUESTION.

> **SPEC-QUESTION:** SPEC §4.3 pins interpreter POLY ICs at stride-2 pairs
> `[k,m]` with no hit counts, which starves the inliner's dominant-case
> choice until a compiled PIC exists. Propose either (a) accept first-seen
> dominance for 2-case sites only (this sprint's behavior), or (b) amend §4.3
> to triples `[k,m,count]` (a Δ to the pinned stride). S14 implements (a).

> **SPEC-QUESTION:** SPEC §8.2's nmethod header has no invocation-counter or
> profile-hash fields, but §8.1's level escalation needs a compiled-side
> trigger. S14 adds `counter/counter_limit/profile_hash` words to the header;
> SPEC §8.2 should be amended to list them.

> **SPEC-QUESTION:** SPEC §8.1 does not mention an effectiveness check; the
> version cap alone permits 3 wasted identical compilations. S14 adds A5
> (Self's `checkEffectiveness`) — propose amending §8.1 to include it.
