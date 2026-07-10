# S24 B4 — spliced NLR blocks with sends (design + as-built record)

**Status:** LANDED. Product of a 2-reader + risk-first-designer + adversarial-
verifier workflow (wf_42e89d59-f74, 2026-07-10; verdict REFUTED:false with one
required amendment, folded in). Prerequisite: B1 (1598654).

## The hazard (H1, verified real)

A spliced block's `^expr` (`Instr::NlrTos`) lowers to plain `Ir::Ret` of the
home compilation — the block's home is by construction the compilation root,
so compiled code needs no closure. But if the block ALSO contains sends, a
deopt can land at one INSIDE the spliced extent: the materializer rebuilds a
genuine BLOCK activation (the is_block chained scope, block-bci space) whose
receiver-arg slot held the scope's recorded receiver — M's SELF, not a
ClosureOop — and the interpreter's OP_NLR_TOS does
`ClosureOop::try_from(receiver_slot).expect(..)` → release panic (or, if self
happened to be a closure, a silent wrong-frame NLR). The old 7-III gate
(`has_nlr && has_send → not spliceable`) existed precisely to keep deopt out
of NLR-bearing spliced extents; its own comment named the fix as "a later
slice". B4 is that slice.

## The fix (small, verified line-by-line)

1. **Materializer synthesis** (deopt.rs M6, the spliced-is_block arm): after
   the FP+3 home-Context alias, synthesize a REAL closure via the existing
   `materialize_closure` (home_ref = the same-materialization root's
   fp+serial; copied[0] = root receiver; copied[1] = root Context iff
   captures_ctx) using the scope's `method_pool_ix` (splice_block has always
   interned the CompiledBlock there), and store it in the rebuilt frame's
   receiver-ARG slot. FP+4 keeps the home self push_frame copied — which
   equals closure.copied(0) by construction: the activate_block_interp shape.
   GC-safe for the same reason as the adjacent phantom-slot materialization
   (frame + root pushed and rooted).
2. **Frame::verify tightened** (stack.rs): the old "spliced shape (b)"
   (FP+4 == arg, both M's self) is dead — ONE legal block-frame shape
   (activate-shaped), making M7 a hard tripwire that the synthesis ran on
   every spliced-block deopt.
3. **Gate deleted** (escape.rs), with the soundness argument in place —
   including the verifier's amendment A1: the marker-exclusion argument is
   stated on three real invariants (markers only on the protected block's own
   fresh interpreter activation; ensure:/ifCurtailed: are primitive methods
   excluded from inlining AND splicing; a protected block lexically containing
   an NLR block fails block_transitively_nlr_free) — NOT the stale "an M with
   ensure: fails the escape gate" claim (false since A3a).
4. **Observable:** `blocks_spliced_nlr` (per-compile, counted on the
   Translator, transferred via IrMethod, bumped into stats by the driver).

## Soundness argument (survived adversarial attack)

A spliced `^` has exactly two executors and both return from the same frame
with the same value. COMPILED: `Ir::Ret` — a branch to M's own epilogue;
correct because the block's home is the compilation root (v1 splices only
into the home method, even in nested inline extents). INTERPRETED: the only
way interpretation reaches the block's `nlr_tos` is through the materializer
(every interpreter-visible entry into a spliced extent is a deopt safepoint
carrying an is_block scope — in-body cold trap, call-return, fused-op fail
paths), and every such rebuilt frame now carries a synthesized closure whose
home_ref packs the just-rebuilt root's fp+serial — `home_is_live` holds by
construction, and unwind proceeds exactly as for an interpreter-created
block. Verifier confirmations: sends inside spliced bodies are never further
inlined (block frame always innermost, no pending-stack interaction); the
value-send-bci reexecute path already materializes a real closure via the
ElidedClosure machinery; all interpreter block activations funnel through
activate_block_interp, so the tightened verify's premise is total.

## Tests (all differential vs MACVM_JIT=off, subprocess)

b4_first (unconditional `^x + k` in `do:` — the detect: shape — incl. the
empty-collection fall-through) + the stat needle; b4_deopt — THE H1 kill-shot:
warm with smi elements then a LargeInteger element (overflow-promoted) flips
the in-block receiver klass → guard deopt INSIDE the spliced block →
synthesis → interpreted nlr_tos, plain + DEOPT_STRESS=64 + GC_STRESS=full:64;
b4_conditional — the multi-BB conditional-NLR negative twin still declines and
stays correct via the A-series path.

## Scope honesty

`blocks_spliced_nlr = 0` on the world suite and deltablue today: real-world
NLR blocks (inputsKnown:'s `ifFalse: [^false]`, detect:-style guards) are
CONDITIONAL → multi-BB block bodies → the single-BB splice limit declines
them. B4 is the SOUNDNESS layer; the payoff arrives with **B5 (multi-BB block
splicing)**, which can now land without touching deopt at all. deltablue
delta ≈ 0 as designed (6ms held).
