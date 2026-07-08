# Next architecture: closing the interpreter↔compiler boundary (always-compile)

Started 2026-07-08, out of a conversation about why MACVM's tier-0
(Rust interpreter) ↔ tier-1 (hand-assembled ARM64 JIT) boundary is a real
departure from Strongtalk's own design (Strongtalk's interpreter was
*itself* low-level — fixed hardware registers for TOS/bcp/frame, per
`arm64.md`'s own citation) and closer to Self's, which went further and
mostly eliminated the boundary: always compile, even cheaply, re-optimize
what gets hot.

**This is not a big-bang rewrite.** The framing that makes it tractable:
getting to always-compile is *literally* the project's own already-underway
arc — keep widening the tier-1 eligibility gate, sprint over sprint, same
as S11 (StoreField), S13 (deopt), S14 (inlining/closures), S15 (OSR) already
did — pushed all the way to its limit, plus one config change. Nothing here
supersedes `weekend_work.md`, which stays scoped to the concrete, near-term
Richards fixes; this is the "someday" direction those fixes sit inside of.

## Two independent axes — both required, neither sufficient alone

**Axis 1 — WHEN compilation is attempted.** Already fully built:
`JitMode::Threshold(1)` compiles on the first call. Zero new engineering;
already exercised by `tests_s10.md`'s own gate item 1 ("every eligible
method compiles on first send").

**Axis 2 — WHAT can succeed.** The real work. Setting threshold=1 today
just means a `NoPermanent` method fails its compilation attempt
*immediately* instead of after 2000 calls (the GUI's current default) — it
still runs interpreted forever either way. **Threshold=1 buys nothing on
its own; it only pays off once `NoPermanent` is rare.** Do axis 2 first,
flip axis 1 once axis 2 makes it worth doing — flipping it early is
harmless but inert.

## The current gate, precisely (as of 2026-07-08, `driver.rs`'s `eligibility_detail`)

Read in full for this doc, not recalled from memory — some of what earlier
sprint docs list as excluded (super sends, NLR) was already closed by S11
step 6 / S13 step 10d and is NOT part of the current remaining gap. The
**actual current exclusion list**:

1. `method.is_block()` — a block, considered as its own standalone compile
   target, is still `NoPermanent`. (Narrower than it sounds: a block
   *created and immediately invoked* inside an otherwise-eligible method
   already compiles fine via S14's inlining/splicing — this only bites a
   block considered in isolation.)
2. `method.argc() > 5` — Gap 2 in `weekend_work.md`, already scoped there
   with two concrete design options.
3. `method.primitive() != 0` — **every** primitive-bearing method,
   unconditionally. 67 primitives total (`grep -c "PrimDesc {"
   src/runtime/primitives.rs`). The single largest remaining exclusion by
   real-world method count — see below.
4. `method.bytecode_len() > 2048` ("tunable" per its own comment).
5. Any send site with `argc > 7` anywhere in the method's body — the other
   half of Gap 2.
6. An escaping (non-elidable) closure — S14's escape pre-pass already
   handles the common case (create-and-immediately-invoke) by promoting
   temps to vregs and eliding the Context; a closure that genuinely
   outlives its creating activation (stored in an ivar, returned, handed to
   something async) is still `NoPermanent`.

**What's NOT actually a remaining obstacle, despite first appearances:**
ordinary/generic/polymorphic sends *inside* an eligible method do not block
compilation — confirmed by the test suite itself
(`eligible_accepts_generic_mono_non_smi_send`,
`eligible_accepts_generic_mono_primitive_target`,
`eligible_accepts_poly_ic`, all in `driver.rs`). A method full of ordinary
sends to arbitrary targets compiles fine; those sends just don't get
SMI_INLINE's special fast-path fusion, they compile as ordinary
`Ir::CallSend`. The exclusion is about what a method *is* (a primitive, a
block, high-argc), not what it *calls*.

## The reframing that makes "gradual" tractable: a compiled entry isn't a rewrite

The instinct "primitives are the big obstacle" is right in scale (67 of
them, and most real Smalltalk methods that aren't pure computation touch at
least one) but wrong in shape if read as "hand-assemble 67 native
implementations." **A compiled entry point for a primitive-bearing method
doesn't need its own codegen for the primitive's behavior — it needs to
call the existing Rust primitive function directly**, the same way
compiled code already calls into Rust for GC, alloc-slow-path, and every
non-curated primitive reached via an ordinary send (`AdapterTable`'s c2i
path, already proven, already tested). The missing piece isn't "teach the
compiler what `at:put:` means in assembly" — most primitives already have
that answer, in Rust, today. The missing piece is giving the
*primitive-bearing method itself* a compiled entry that reaches it, instead
of that method being permanently excluded from having any compiled entry
at all.

Concretely, per remaining item, roughly ordered by how much of it is
"mechanical shim work" vs. "genuinely new compiler-construction problem":

- **Primitives**: real numbers, not a flat 67-item estimate (pulled
  directly from `PrimDesc` in `src/runtime/primitives.rs`, 2026-07-08):
  **18 of 67 can allocate** (need correct GC-safepoint description at the
  call site — exactly the discipline behind this project's worst bugs so
  far), **55 of 67 can fail** (need the fall-through shape described
  below), and only **7 of 67 are neither** — the genuinely trivial,
  pure-shim tier. "Mostly mechanical" underates this: the trivial tier is
  7, not 67.

  A compiled entry for the trivial tier really is just: marshal
  receiver+args per the existing `x0`-`x5` convention (D5.1, already
  pinned), call the Rust primitive function directly, return. The
  genuinely new shape needed for the other 60: **on primitive `Fail`, the
  real semantics are "fall through to this method's own Smalltalk bytecode
  body"** (e.g. `SmallInteger>><`'s primitive falls back to a real
  Smalltalk implementation on a non-SmallInteger operand — this session's
  own earlier walk-through of `prim_lt`/`PrimitiveOutcome::Fallthrough`).
  Today that fallback is "the whole method runs interpreted." Under
  always-compile it needs to become an ordinary intra-method branch in
  compiled code — ergonomically similar to the untaken-arm-as-trap
  machinery from `weekend_work.md` Gap 1, but a distinct case (a primitive
  Fail, not a speculative guard mismatch).

  Worth being honest that the 12 already-fused SMI_INLINE primitives don't
  fully prove this pattern out: their current answer to "what happens on
  failure" is a **deopt to the interpreter** — fine when failure is rare
  (that's the entire Gap 1 conversation), not a general answer for a
  primitive that fails routinely, which some of the remaining 55 plausibly
  do. The efficient, non-deopting fail-fallthrough this section describes
  is closer to unbuilt even for the primitives that already compile today.

  **Correction/refinement (2026-07-08): today's guard-failure deopts to
  the interpreter not by design choice but because there is no other
  target** — `<`'s own method is `NoPermanent` today, so the interpreter
  is the *only* place its real semantics exist in any form for a fused
  caller's guard to fall back to. Once primitive-bearing methods have real
  compiled entries, the right answer for a guard failure isn't "deopt to
  that compiled method" either — it's **an ordinary call to it**. The
  fused fast path already has receiver+args in registers at the guard;
  falling through to a plain compiled-to-compiled call (the same
  `Ir::CallSend` shape every ordinary send already uses) needs no new
  machinery at all — no scope descriptors, no frame materialization, none
  of S13's deopt apparatus. "Deopt" specifically means *reconstructing* a
  different representation of the same state; a fused fast path falling
  through to an ordinary call isn't a reconstruction, it's just a call. So
  compiling primitives has a second payoff beyond the primitive's own fast
  path running natively: every *existing* SMI_INLINE fast path gets to
  downgrade its own failure handling from a full deopt to a plain call,
  once its target is compiled too — strictly cheaper than either "deopt to
  interpreter" or a hypothetical "deopt to compiled method," not a tie
  between two comparable options. (Sibling idea to Gap 1, not the same
  fix: Richards' `processWork:` trap isn't a primitive-guard failure at
  all — both branch arms are ordinary Smalltalk, no primitive involved —
  so poly-arm branch compilation and this "call instead of deopt" idea are
  different mechanisms that happen to share the same motive: don't pay
  deopt cost for something that isn't actually rare.)

  A smaller number of primitives are not shim-shaped at all, regardless of
  fail/allocate handling — `valueWithArguments:` (dynamic-arity block
  activation, runtime-determined argc), `ensure:`/`ifCurtailed:`
  (marker-based NLR protocol), FFI dispatch (S20's own multi-step
  machinery) — each closer in scope to its own mini-project than to a
  shim, and the same *kind* of hard as escaping closures below, not a
  smaller version of it.
- **argc caps**: already scoped, two real options on record
  (`weekend_work.md` Gap 2).
- **Escaping closures**: the one item that is NOT a reframe-as-a-shim
  problem. A closure that outlives its creating activation needs a real,
  general compiled representation — a heap object, invokable later,
  from compiled code, with its own captured environment. This is closer to
  genuine open compiler-construction work than anything else on this list,
  and worth treating as the long pole, not the primitives.
- **Bytecode length / frame budget**: probably mechanical to widen, but
  check interactions with fixed-size assumptions elsewhere (RootSpill,
  oopmap slot tables, deopt scope descriptors) before assuming it's free —
  those exact fixed-size assumptions are what the RootSpill 6→8 history
  (`VMregisters.md` §3) already burned this project on once.

## What doesn't go away even at zero fallback

- **The GC/safepoint correctness burden doesn't shrink — it gets exercised
  by more code, not less.** Oop maps and safepoints currently only have to
  be right for the curated subset of methods that already compile. Every
  widening step puts MORE of the system's total execution through exactly
  the machinery that's already produced this project's hardest bugs (the
  S12 oop-map safepoint SIGSEGV, RootSpill's silent-corruption history,
  AdapterTable's stale-key aliasing). Not a reason to stop — a reason each
  widening step needs the same stress-test/adversarial-review rigor that
  closed those, not a lighter one just because the change "sounds
  mechanical."
- **Keep the interpreter as a differential-testing oracle regardless of
  how rarely production code still reaches it.**
  `compiled_result_equals_interpreted` (S10, task #32) exists specifically
  to catch drift between the interpreter's primitive implementations and
  the compiler's independently-written codegen for the same semantics —
  that value doesn't depend on the interpreter being on the hot path, only
  on it continuing to exist as ground truth. Don't delete it once it's
  rarely executed; that's exactly when an independent oracle is most
  valuable, not least.
- **Debugging/introspection is a simplification at the limit, not a new
  problem, but confirm that before assuming it.** DBG0-3 already handles
  mixed interpreted/compiled backtraces; at zero fallback there's only one
  frame kind to support, which should be strictly simpler. Worth an
  explicit check that scope-descriptor-based introspection of a compiled
  frame is fully equivalent to what direct interpreter-frame inspection
  offers today for an interactive debugging session, rather than assuming
  parity.

## Recommended path

1. Keep doing exactly what's already underway: one gate-widening step at a
   time, each independently verified (differential test against the
   interpreter, stress-tested, adversarially reviewed) — same discipline
   that closed every prior gap, not a faster/looser pass because this
   phase "is just shims."
2. **Primitive-entry shims are probably the single highest-leverage next
   widening** after `weekend_work.md`'s Gap 1/2/3 land — 67 primitives is
   the largest remaining exclusion by real method count in ordinary
   Smalltalk code, and the reframing above means it's mostly mechanical
   rather than requiring new compiler theory (unlike escaping closures).
3. Escaping closures are the long pole — budget real design time, not a
   sprint-step-sized slot, when it's picked up.
4. Flip `threshold=1` last, once `NoPermanent` is genuinely rare —
   flipping it earlier is harmless but buys nothing (axis 1 without axis 2
   is just failing fast instead of failing slow).
5. Never delete the interpreter. Its job changes from "tier-0 execution
   path" to "correctness oracle," but that job doesn't go away.
