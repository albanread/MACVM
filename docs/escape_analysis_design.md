# Escape analysis / scalar replacement — design + the blocking finding

**Status:** investigated 2026-07-16, not built. Read the finding first — it
changes the scope.

## Goal

Generalize the unboxed-float fast path (unbox → compute in registers → sink the
box) to arbitrary short-lived, non-escaping objects: don't heap-allocate an
object that never leaves its creating method; keep its fields as SSA values;
materialize a real object only if it escapes or the frame deoptimizes. Our own
profiling is the argument — Mandelbrot's 746→166 ms came from *deleting
allocation*, and Smalltalk allocates relentlessly (every `Point`, every
intermediate, every small collection).

## The blocking finding (why this needs a prerequisite)

Grounding in the real IR before building surfaced the crux: **there are
essentially no scalar-replaceable allocations in current MACVM, because every
allocation and every field access is hidden behind a `CallSend`.**

- `Ir::Alloc { dst, klass, size_words }` — the only visible allocation — fires
  **only** for a literal `X basicNew` where `X` is a compile-time class constant
  and the target is the `basicNew` primitive (`ir.rs`, the `Instr::Send` arm's
  `alloc_site_klass` gate).
- Real construction never looks like that. `3 @ 4` is a `CallSend` of `@` to an
  integer; the `Point` is allocated two calls deep (`@` → `Point x:y:` →
  `basicNew`). `Point new` is a `CallSend` to `new` (verified: it does **not**
  become an `Alloc`; the class-constant isn't propagated through `new`'s
  `self basicNew`).
- Field access is call-hidden too: `p x` is a `LoadField` **on a Point that
  arrived from a call**, and `p setX:` is a `CallSend`. So even the
  store-into-the-new-object and the read-back-from-it aren't visible as
  `StoreField`/`LoadField` on the allocation.

Consequence: an escape-analysis pass over today's IR would find nothing to do.
You cannot even write a Smalltalk test that produces a local
`Alloc`+`StoreField`+`LoadField` triple without the accessors already inlined.
The float fast path only works because the JIT **synthesizes** the box (`FBox`)
inside the method — the allocation is JIT-created, not call-hidden. Arbitrary
objects are call-hidden.

**So the real work is not the escape analysis — it is the inlining that exposes
allocation.** The analysis + scalar replacement is the payoff on top, and worth
maybe 30% of the effort; the allocation-exposing inlining is the other 70% and
is the actual lever.

## The prerequisite: allocation-exposing inlining

To make allocations (and their field traffic) visible in the creating method's
IR, the JIT must inline the small-method construction chain with **class-
constant propagation**:

1. Inline `X class >> new` and propagate that its `self` is the class constant
   `X`, so the inner `self basicNew` re-recognizes as an `Alloc` site (it does
   not today — this is the missing piece the `Point new` probe exposed).
2. Inline simple constructors (`x:y:`, `@`) and simple ivar setters/getters so
   the field stores/reads become `StoreField`/`LoadField` on the fresh object.

MACVM already has `try_inline_nonleaf` (S14) and the `Alloc` op; what's missing
is the const-class-through-`self`-send propagation and inlining depth to reach
`basicNew` + the setters. This inlining has value **on its own** (fewer calls,
smaller methods) even before any escape analysis.

## Then: escape analysis + scalar replacement (the payoff, once allocations show)

- **Escape analysis** over the IR, using `Ir::uses`: an `Alloc` dst `p` escapes
  if it appears as a `CallSend`/`CallRuntime` arg, a `Ret`/`RetSelf`/`NlrReturn`
  value, or the `val` of a `StoreField` into another object; it does NOT escape
  when it is the `obj` of its own `LoadField`/`StoreField`. Track aliases through
  `Move`. First cut: all-or-nothing per object, conservative (any non-local use
  = escape). Later: partial escape (materialize lazily only on the escaping
  path) — the Graal refinement.
- **Scalar replacement**: drop the `Alloc`, forward each `LoadField` to the
  last-stored SSA value for that offset, drop the `StoreField`s.
- **Deopt materialization** — the correctness core, and it GENERALIZES the float
  `DoubleSlot`: a scalar-replaced object must be reconstructable at every
  safepoint it is live across. Add a virtual-object deopt descriptor (klass +
  each field's `ValueLoc`); the materializer (`runtime::deopt`) allocates a real
  instance and fills it, exactly as `DeoptSlot::Double` reboxes an unboxed float.
  This is the piece that must be GC-stress-verified.

## Reuse map

| need | exists |
|---|---|
| visible allocation | `Ir::Alloc` (but only for literal `X basicNew` — the gap) |
| field ops | `Ir::LoadField` / `Ir::StoreField` |
| operand enumeration for EA | `Ir::uses(f)` |
| non-leaf inlining | `try_inline_nonleaf` (S14) — needs const-class propagation added |
| deopt-time reallocation | the materializer + `DeoptSlot::Double` pattern to generalize |
| shape metadata (size, offsets) | `klass.non_indexable_size()`, field byte offsets |

## Staged plan

- **P0 (prerequisite) — allocation-exposing inlining.** Const-class propagation
  through inlined `new`/self-sends so `basicNew` becomes an `Alloc`; inline the
  simple setters/getters. Gate: a `3 @ 4; p x + p y` method shows `Alloc` +
  `StoreField` + `LoadField` in its IR (today it shows none).
- **S1 — escape analysis pass** (pure IR analysis, unit-tested in isolation).
- **S2 — scalar replacement** for objects that don't escape and don't live
  across a safepoint (rare, but proves the LoadField-forwarding).
- **S3 — deopt materialization** (virtual-object DeoptSlot), which is what makes
  it useful for objects live across safepoints (i.e. almost all). GC-stress the
  reallocate-on-deopt path with an instance held from every root kind.
- **S4 — partial escape** (materialize only on the escaping path).

## Recommendation

This is a real arc, and its foundation (P0 inlining) is the bulk of it. Two
honest options:
- **Commit to the arc** if the allocation win is the priority — P0 is valuable by
  itself, and the payoff (S1–S4) is measured-to-be-real.
- **Defer** in favor of a cheaper roadmap item (the scoped `catch:`, which is
  unblocked and small) and take escape analysis as a later, larger project once
  P0 inlining exists.

Either way, the lesson stands: the win is deleting allocation, but *exposing* the
allocation to the optimizer is the prerequisite nobody sees until they read the
IR.
