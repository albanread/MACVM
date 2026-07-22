# Cog's send/activation machinery — what MACVM can still take

*Source study of the REAL Cog logic (readable VMMaker Smalltalk, not the
generated C): `~/claudeprojects/pharo-vm-src/smalltalksrc/VMMaker/` — the
Pharo fork's Tonel tree, the same lineage as the Pharo 13 VM `cog-bench.sh`
measures against. Focus: why Cog wins richards 2.85× (per-activation send
overhead, `cog_bench.md`) while losing every micro to us. Method names cited
are the actual sources read; MACVM-side claims checked against `emit.rs`,
`regalloc`, `driver.rs`, `ir.rs`.*

## 1. Cog's eight mechanisms — and where MACVM stands

| # | Cog mechanism (where it lives) | MACVM status |
|---|---|---|
| 1 | **Register calling convention** — receiver in `ReceiverResultReg`, ≤2 args in `Arg0Reg`/`Arg1Reg` (`StackToRegisterMappingCogit>>marshallSendArguments:`); low-arity sends never touch memory | **Already have** — `emit_call_send` parallel-moves receiver+args into x0..x5 |
| 2 | **Deferred stack-to-register mapping** — a simulated stack (`CogSimStackEntry`) defers materialization until a consumer; pushes feeding a send never hit memory (class comment) | **Already have** — the IR + linear-scan keep operands in vregs; same idea, different formalism |
| 3 | **Monomorphic IC = one patchable constant-load + direct call** (`genLoadInlineCacheWithSelector:`, `linkSendAt:in:to:offset:receiver:`, `rewriteInlineCacheAt:tag:target:`) — warm send is load-imm class tag → `bl` | **Already have** — `bl_patchable(RelocKind::InlineCache)`, mono+poly |
| 4 | **Three-instruction checked entry** (`Cogit>>compileEntry`: load tag, `CmpR`, `JumpNonZero`) with the checked/unchecked **entry-offset alignment trick** — super sends bind to `noCheckEntry` and skip the guard | **Already have** (guard shape: `emit_entry_guard`); the super-send unchecked-entry shortcut is a possible micro-refinement, unmeasured |
| 5 | **Frameless methods** — `scanMethod` computes `needsFrame` per bytecode (`needsFrameNever` for most pushes/returns); `compileFrameBuild` emits NOTHING for frameless methods: no fp push, no stack-limit check, args stay in registers, bare `RetN` | **MISSING** — `emit()` unconditionally emits `stp x29,x30,[sp,#-16]!; mov x29,sp; sub sp,#frame` + deopt nil-fills for every activation, leaf accessors included |
| 6 | **Special-selector inlining with branch fusion** — `genSpecialSelectorComparison` fuses compare + following branch, no boolean materialized; `==`/`~~`/`class` inline (`genSpecialSelectorEqualsEquals`) | **Already have** — `SmiArith`/`SmiCmpVal`/`FCmpBr` fusion; `RefCmpVal` (unguarded `==`/`~~`) + `BoolNot` landed in the S24 special-selectors wave |
| 7 | **One shared out-of-line abort** for SIC-miss + stack overflow (`compileAbort`, disambiguated by `ReceiverResultReg=0`); overflow = single `Cmp SPReg` in frame build | **N/A / partial** — different but equivalent structure (`stub_resolve` + `Poll`); no action indicated |
| 8 | **PIC escalation in machine code** — closed PIC to 6 cases (`MaxPICCases`), then open/megamorphic (`cePICMiss:receiver:`), never an interpreter round-trip | **Already have** — mono → poly per the IC design |

**The two taxes MACVM pays that Cog does not:**

- **Per-send NLR sentinel check** — `emit_nlr_check` puts `sub x17,x0,#NLR_SENTINEL; cbz x17,<epilogue>` after *every* `bl`. Cog's non-local return is callee-side: a block `^` unwinds through `ceNonLocalReturnTrampoline`; the common no-NLR return costs zero instructions.
- **Spill-all across send safepoints** — `regalloc`'s `crosses_safepoint` forces every oop live across a `CallSend` to a stack slot. Cog keeps receiver/args register-resident through sends.

## 2. Ranked plan for the richards gap

**Step 0 — re-measure before building anything** *(the falsify-before-building
law).* The recorded 2.85× (`cog_bench.md`) was snapshotted **before** the
`RefCmpVal`/`BoolNot` special-selector landings; richards does ~130k `==` +
~90k `not` sends, all now inlined. The gap to explain may already be smaller.
Requires reinstalling the Pharo 13 headless into `.cog/` (the install is
currently absent).

**1 — Frameless compilation for trap-free small methods** *(the strongest
candidate; the exact shape of richards' cost).* Mirror `scanMethod`: a method
compiles frameless when it has no `Poll`, no speculative guard needing a frame
past entry, no context/block capture, and no non-tail send with an oop live
across it. For those, skip prologue (`stp/mov/sub`), deopt nil-fills, and
epilogue (`mov sp/ldp`) — receiver/args stay in x0..xN, bare `ret`. Richards'
swarm of tiny accessors/predicates on the four `Task` subclasses — precisely
the ones the inliner's IC/budget gate declines — currently pay ~6 instructions
plus frame memory traffic each; under Cog they pay nothing.

**2 — Retire the per-send NLR sentinel** *(second; two instructions off every
send in every hot chain).* Move to Cog's callee-side model: block `^` unwinds
via a runtime trampoline walking the native frames the deopt metadata already
describes; the sentinel check after every `bl` disappears. The rare real NLR
pays; the millions of ordinary returns stop paying.

**3 — Register-resident oops across sends** *(biggest engineering lift,
attacks the residual).* Generalize the loop-var `resident` mechanism
(`emit_resident_reloads`) to oops live across `CallSend`; teach `oopmap.rs` to
describe oops in callee-saved registers so `crosses_safepoint` can relax.
This is Cog's "args stay in registers" made general.

Each step is independently measurable with the interleaved-A/B discipline;
none may be costed by instruction counts (`instructions-are-not-time`, the
60:1 lesson).

## 3. What NOT to port

- **Slang/simulation machinery** — Cog's ability to run the JIT in-image is
  its development environment, not a runtime speed mechanism.
- **Spur object representation** (class-index tags, forwarding objects) —
  presumes `become:`/lazy-forwarding; MACVM's fixed-shape direct-pointer
  design deliberately rejects that lineage.
- **The interpreter's threaded-code tricks** — MACVM's interpreter was just
  measured 2.6× faster (94ac9f8) and is not the richards bottleneck; the gap
  is compiled-send shape, not interpretation.
