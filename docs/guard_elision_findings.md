# Guard-elision census findings — and why they all point to the inliner (2026-07-26)

Two attempts at dropping the per-call `GuardKlass` from mono inlines
(dart124 item 4, CHA), both census-first. Both dead-ended, and the dead
ends converge on a single conclusion.

## CHA (single-implementor guard drop): 58% eligible — but UNSOUND on untyped MACVM

`MACVM_CHA_COUNT=1` (`cha_implementor_count`): of 327 KlassTest-guarded mono
accessor inlines in the bench suite, **190 (58%)** are over single-implementor
selectors — `walkStrength`, `mark`, `determinedBy:`, `stronger:`, `link:`,
`stay`, `waitingWithPacket` … the per-class accessors of richards/deltablue,
exactly where their gap to Dart lives.

But single-implementor does NOT make the guard droppable. The mono guard
`receiver.klass == K` does TWO jobs: pick method M, AND prove the receiver
UNDERSTANDS the selector (its fail edge traps → reexecutes → clean DNU for a
non-understanding receiver). Drop it and a non-understanding receiver runs M
anyway — reading fields that may not exist: **DNU becomes UB**, violating
MACVM's bedrock invariant ("DNU defined, never UB"). Single-implementor
proves *which* method for receivers that understand S, never that an
arbitrary receiver understands it. Only a SOUND receiver type proves that —
which is why Dart *2* went sound-static, and the boundary the AOT/typing
discussion named. MACVM's optional types are erased + unsound, so they can't
carry it. The sound sub-cases are already covered: sibling-widening is
`SameTargetPoly`'s membership guard; self-sends are guard-free via
self-devirt. Pure CHA adds only the unsound part. NOT implemented.

## Allocation-provenance guard drop (the sound cousin): 0 eligible

`MACVM_ALLOCGUARD_COUNT=1` (`alloc_guard_census`): a `GuardKlass(obj, K)`
whose `obj` traces to an `Ir::Alloc` of klass K is soundly droppable (the
alloc result IS exactly klass K, no DNU risk). Result: **eligible=0 of 252**
guards, suite-wide. The pattern `x := X new. x foo` — alloc and guarded send
in the SAME compiled method — does not occur in the hot code. Constructors
create-and-return; the guarded sends live in the CALLERS, a different method,
where the klass is unknown. NOT implemented (nothing to do).

## The convergence: the inliner is the keystone

Alloc-provenance would fire if the alloc and the send were colocated — which
INLINING the constructor into its caller achieves. Same shape as: dict's cost
is the `Dictionary>>at:` → `scanFor:` chain that only collapses when inlined;
load-forwarding's cross-method redundancy; CHA-via-monomorphism. Every "flat"
or "zero" census this session — F3c S1 `freed=0`, deopt-liveness subsumed,
alloc-provenance 0 — is flat BECAUSE the relevant operations sit in SEPARATE
methods that MACVM's mono/small splicer doesn't merge.

The support passes are built and idle: range analysis, load forwarding,
guard-elision machinery (L1), alloc provenance, the intrinsic recognizers.
They are the TEETH. **The aggressive budgeted inliner (dart124 item 2 —
`flow_graph_inliner.cc`'s 10-knob worklist: depth 6, recursion depth 1,
count-ranked, `inlining_size_threshold` 25/`callee_size_threshold` 80) is the
JAW.** It's what brings allocs+sends, dictionary+scanFor, accessor chains into
one unit where the teeth finally bite.

And its feared cost is disproven: the "inlining explodes deopt metadata"
objection (F3c) measured FLAT (the deopt-liveness rework reduced records with
zero wall-clock effect). So the inliner's real costs are register pressure and
icache — not a correctness-adjacent metadata blowup. It is more directly
viable than the earlier "trap until per-site liveness" framing claimed.

**Recommendation:** the budgeted worklist inliner is the next lever — the one
that makes everything already built pay. The censuses here
(`MACVM_CHA_COUNT`, `MACVM_ALLOCGUARD_COUNT`) stay as the re-evaluation probes:
re-run them after the inliner lands to watch alloc-provenance move off 0.
