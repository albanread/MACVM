//! `eligible` (D1) + `compile_method` (D4) — the S10 compile driver: the
//! only place that decides whether a method compiles, and that drives the
//! decode -> convert -> regalloc -> emit -> publish -> install pipeline.
//! Neither function pushes a frame or touches the process stack — both are
//! callable directly, independent of the interpreter trigger (S10 step 8).

use crate::bytecode::opcode::{decode_at, Instr};
use crate::codecache::nmethod::{IcSite, NmState, Nmethod, NmethodId, OopMap, PcDesc};
use crate::codecache::stubs::resolve_super_target_entry;
use crate::compiler::jasm_assembler::JasmAssembler;
use crate::compiler::{decode, emit, ir, oopmap, regalloc};
use crate::interpreter::ic::InterpreterIc;
use crate::interpreter::ic::{ic_state, IcState};
use crate::oops::wrappers::{KlassOop, MethodOop, SymbolOop};
use crate::runtime::lookup::lookup;
use crate::runtime::vm_state::VmState;
use crate::runtime::JitMode;

/// `codecache::nmethod::IcState` — spelled out at every use site rather
/// than imported under its own name: `interpreter::ic::IcState` (the
/// UNRELATED interpreter-IC lattice, already imported above under that
/// exact name) would collide with it.
type CompiledIcState = crate::codecache::nmethod::IcState;

/// D1 point 2: `primitives.rs`'s own pinned ids for
/// `{ +, -, *, bitAnd:, bitOr:, bitXor:, <, <=, >, >=, =, ~= }` — division
/// (`//`, `\\`, `bitShift:`: ids 4, 5, 9) excluded in v1. `pub(crate)`: also
/// read directly by `ir::Translator::is_smi_inlinable` (S11 step 7) — both
/// readers must agree on exactly the same set, since `eligibility_detail`'s
/// own `mono_smi_inline_send` below is what decides a method compiles AT
/// ALL, while `is_smi_inlinable` (ir.rs) decides, per send site within an
/// already-eligible method, fused fast path vs. a real `CallSend`.
pub(crate) const SMI_INLINE: [i64; 12] = [1, 2, 3, 6, 7, 8, 10, 11, 12, 13, 14, 15];

/// D1 point 3, "tunable".
const FRAME_BUDGET_SLOTS: i32 = 60;
/// D1 point 4, "tunable".
const MAX_BYTECODE_LEN: usize = 2048;

/// D1's own linear scan, at a finer grain than the `bool` its text
/// describes: an ELIGIBLE method compiles now; a `NoPermanent` one never
/// will (a structural property of its bytecode/flags, or a send site
/// that's already resolved to something D1 doesn't cover) and gets
/// `compile_disabled`; a `NoRetryLater` one has EVERY check passing
/// except that some send site's IC is still `Empty` — the site simply
/// hasn't been reached yet, not "reached and rejected". This distinction
/// exists because `activate_method`'s trigger fires from OUTSIDE the
/// method, before its own body has ever run: on `MACVM_JIT=threshold=1`
/// specifically, the very first call is ALSO the trigger, so any method
/// with an inner send is *guaranteed* cold on that first attempt — an
/// early draft of this scan treated that as permanent and disabled every
/// such method forever after one failed attempt, which silently defeated
/// `threshold=1`'s own stated purpose ("every eligible method compiles on
/// first send", tests_s10.md gate item 1) for anything but the rare
/// method with zero internal sends. `NoRetryLater` leaves the counter
/// alone instead of disabling, so the method's *next* call — after this
/// one has actually run its body interpreted and warmed its own sites —
/// gets a fresh attempt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Eligibility {
    Yes,
    NoPermanent,
    NoRetryLater,
}

impl Eligibility {
    fn worse(self, other: Eligibility) -> Eligibility {
        use Eligibility::*;
        match (self, other) {
            (NoPermanent, _) | (_, NoPermanent) => NoPermanent,
            (NoRetryLater, _) | (_, NoRetryLater) => NoRetryLater,
            (Yes, Yes) => Yes,
        }
    }
}

/// D1: a single linear scan of `method`'s bytecode — ALL of opcode
/// allowlist, per-send IC shape, method flags, and frame-budget bound must
/// hold for `compile_method` to attempt compiling `method`. Thin `bool`
/// wrapper over [`eligibility_detail`] matching D1's own documented
/// signature (and what this module's existing tests call) — `compile_method`
/// itself calls `eligibility_detail` directly, since it needs the
/// permanent-vs-retry distinction `bool` collapses away.
pub fn eligible(vm: &VmState, method: MethodOop) -> bool {
    eligibility_detail(vm, method) == Eligibility::Yes
}

/// D1 point 2 (mono-smi-inline gate): a `Send` site only clears eligibility
/// when its own IC is already `Mono`, guarded on `SmallInteger`, targeting a
/// method whose primitive is in [`SMI_INLINE`] — this is deliberately
/// NARROWER than what a compiled site can actually reach at runtime (a
/// generic, non-smi, or polymorphic send): S11 step 7 experimented with
/// widening this to "arbitrary send in ANY IC state" (D1's own text) and
/// found the C2I/reentrant-call machinery underneath (S11 step 4/5) isn't
/// robust enough yet for the much larger surface that unlocks —
/// specifically, deep block-activation nesting reached through a compiled
/// caller's own generic `CallSend` (see the fixes alongside this one:
/// `emit::Emitter::emit_call_send`'s spill/register hazard,
/// `interpreter::send::try_primitive`, `run_method_reentrant`'s `vm.regs`
/// snapshot, and `run_method`'s entry-sentinel spoof around an `Activated`
/// primitive). Those four fixes are real, verified bugs in their own right
/// and stay; this gate stays at its ORIGINAL, conservative shape until a
/// later step gives C2I reentrancy the depth of testing D1's fuller text
/// deserves — `docs/sprints/sprint_s11_detail.md` has a SPEC-QUESTION on
/// this gap.
fn eligibility_detail(vm: &VmState, method: MethodOop) -> Eligibility {
    // S14 step 7-II-b: `has_ctx` (M owns a heap Context because a nested block
    // captures its temps) is NO LONGER an outright reject — the escape pre-pass
    // below gates it: a has_ctx method always creates a capturing closure, so it
    // compiles ONLY if that closure is elidable (all_elidable), which promotes
    // ALL of M's ctx-temps to vregs and elides M's Context. A `has_ctx` method
    // whose block escapes still returns NoPermanent (via the pre-pass).
    if method.is_block()
        || method.argc() > 5
        || method.primitive() != 0
        || method.bytecode_len() > MAX_BYTECODE_LEN
    {
        if vm.options.trace.is_enabled("jit") {
            eprintln!(
                "[jit] NoPermanent reason: block={} argc={} prim={} bc_len={}",
                method.is_block(),
                method.argc(),
                method.primitive(),
                method.bytecode_len()
            );
        }
        return Eligibility::NoPermanent;
    }

    // S14 step 7-I: a method that CREATES a literal closure (`push_closure`) is
    // no longer rejected outright. Run the escape pre-pass ONCE (only when a
    // closure is present — the common closure-free method pays nothing) and let
    // it decide: every closure site must be provably elidable (immediately
    // invoked via a matching `value`-send, non-escaping, spliceable) or the
    // whole method stays interpreted (`NoPermanent`). An escaping closure a
    // compiled frame cannot represent is the exact soundness boundary (SPEC
    // §8.4) — "inline-or-gated". A `value`-send on a proven site bypasses the
    // IC-mono check below (its receiver is a statically-known block; it need not
    // be IC-mono to be splicable).
    let has_closure = {
        let mut b = 0usize;
        let mut found = false;
        while b < method.bytecode_len() {
            let (instr, next) = decode_at(method, b);
            if matches!(instr, Instr::PushClosure { .. }) {
                found = true;
                break;
            }
            b = next;
        }
        found
    };
    let escape = if has_closure {
        let e = crate::compiler::escape::analyze(method);
        if !e.all_elidable {
            // Some closure escapes (or is unspliceable) — cannot compile M.
            return Eligibility::NoPermanent;
        }
        Some(e)
    } else {
        None
    };
    // S14 step 7-II-b: a `has_ctx` method's Context elides ONLY because every
    // capturing block is inlined. A has_ctx method with NO (elidable) closure has
    // nothing justifying the elision — real frontend output never produces one
    // (has_ctx ⟺ a capturing block exists), so reject the degenerate case rather
    // than silently compile a method whose Context has no promotable owner.
    if method.has_ctx() && escape.is_none() {
        return Eligibility::NoPermanent;
    }

    let mut verdict = Eligibility::Yes;
    let mut bci = 0usize;
    while bci < method.bytecode_len() {
        let (instr, next) = decode_at(method, bci);
        match instr {
            Instr::PushSelf
            | Instr::PushNil
            | Instr::PushTrue
            | Instr::PushFalse
            | Instr::PushSmi(_)
            | Instr::PushLiteral(_)
            | Instr::PushTemp(_)
            | Instr::StoreTemp(_)
            | Instr::StoreTempPop(_)
            | Instr::PushInstvar(_)
            | Instr::PushGlobal(_)
            | Instr::Pop
            | Instr::Dup
            | Instr::JumpFwd(_)
            | Instr::JumpBack(_)
            | Instr::BrTrueFwd(_)
            | Instr::BrFalseFwd(_)
            | Instr::ReturnTos
            | Instr::ReturnSelf => {}
            Instr::Send { ic, super_ } => {
                // S14 step 7-I: a `value`-send on a PROVEN elidable closure site
                // (the escape pre-pass mapped this bci to a splice) is eligible
                // regardless of its IC — its receiver is a statically-known block
                // that `ir::convert` splices inline, so the IC-mono check below
                // (meant for ordinary dynamic dispatch) does not apply.
                if escape.as_ref().is_some_and(|e| {
                    e.value_send_target(bci).is_some() || e.blockarg_send_target(bci).is_some()
                }) {
                    // Yes — never worse than the current verdict.
                    bci = next;
                    continue;
                }
                // D4.6: a super send's own target is resolved statically
                // (holder.superclass(), fixed at compile time) rather than
                // through the interpreter's own IC lattice, so
                // `mono_smi_inline_send`'s "is this site's own IC already
                // mono-smi" check is meaningless for it — skip straight to
                // `Yes` (never worse than the current verdict: `Yes` is
                // this enum's own best case).
                if !super_ {
                    verdict = verdict.worse(mono_smi_inline_send(vm, method, ic));
                    if verdict == Eligibility::NoPermanent {
                        return Eligibility::NoPermanent; // short-circuit: nothing later can undo this
                    }
                }
            }
            // S11 step 7 (D1): instvar/global stores are now allowed --
            // `ir.rs`'s own `StoreField{barrier:true}` conversion handles
            // both (mirrors `PushInstvar`/`PushGlobal`'s existing
            // read-side handling exactly).
            Instr::StoreInstvarPop(_) | Instr::StoreGlobalPop(_) => {}
            // S14 step 7-I: a `push_closure` in M's own top-level bytecode is
            // allowed ONLY when the escape pre-pass above proved EVERY closure
            // site elidable (we returned `NoPermanent` otherwise). `ir::convert`
            // emits no IR for it (it splices the block body at the value-send).
            Instr::PushClosure { .. } => {
                debug_assert!(
                    escape.is_some(),
                    "a push_closure reached the scan but has_closure was false"
                );
            }
            // S14 step 7-II-b: M's own captured temp access. Allowed at DEPTH 0
            // (M's own Context) — `ir::convert` promotes it to a vreg and elides
            // M's Context. A has_ctx M always creates a capturing closure, so the
            // escape pre-pass (`escape.is_some()` + all_elidable) already proved
            // the Context elidable. A `depth != 0` (nested-context access) is a
            // later slice → NoPermanent.
            Instr::PushCtxTemp { depth, .. } | Instr::StoreCtxTempPop { depth, .. } => {
                if depth != 0 || escape.is_none() {
                    return Eligibility::NoPermanent;
                }
            }
            // Block returns / NLR only legitimately appear INSIDE a block body
            // (reached via the splice, not this scan). At M's top level they are
            // malformed / an explicit NLR (7-III) → still excluded.
            Instr::BlockReturnTos | Instr::NlrTos => return Eligibility::NoPermanent,
        }
        bci = next;
    }
    if verdict != Eligibility::Yes {
        // S14 step 3: `mono_smi_inline_send` no longer returns `NoRetryLater`
        // (an `Empty` IC is now `Yes` — compilable as a trap), so in practice
        // `verdict` is always `Yes` here (a `NoPermanent` site short-circuits
        // above). This guard is kept as a defensive catch-all: any future
        // `NoRetryLater`/`NoPermanent` producer added to the loop still returns
        // its verdict rather than falling through to the frame-budget check.
        return verdict;
    }

    // Frame budget (D1 point 3): reuses ir.rs's own CFG-aware stack-depth
    // worklist (`compute_entry_depths`) rather than a second, separately
    // maintained scan — `decode`/`compute_entry_depths` are both safe to
    // call on any structurally-valid method regardless of D1 eligibility
    // (neither cares whether a `Send` site is mono-smi-guarded, only that
    // its IC has a recorded argc).
    let cfg = decode::decode(method);
    let (_, max_stack) = ir::compute_entry_depths(method, &cfg);
    if method.ntemps() as i32 + max_stack > FRAME_BUDGET_SLOTS {
        if vm.options.trace.is_enabled("jit") {
            eprintln!(
                "[jit] NoPermanent reason: frame budget (ntemps {} + max_stack {} > {})",
                method.ntemps(),
                max_stack,
                FRAME_BUDGET_SLOTS
            );
        }
        return Eligibility::NoPermanent;
    }

    Eligibility::Yes
}

/// D1 point 2: `ic_idx`'s own site must already be `Mono`, guarded on
/// `SmallInteger`, targeting a method whose primitive is in [`SMI_INLINE`]
/// — OR (S11 D7) a mono `basicNew` site, which `ir.rs` compiles to an
/// inline `Ir::Alloc`. Anything else (`Empty`, `Poly`, `Mega`, a non-smi
/// non-basicNew guard, or a mono-smi target whose primitive isn't fusable)
/// keeps this method interpreted. See `eligibility_detail`'s own doc for why
/// this stays narrower than D1's full "arbitrary send in any IC state" text.
fn mono_smi_inline_send(_vm: &VmState, method: MethodOop, ic_idx: u16) -> Eligibility {
    match ic_state(method, ic_idx) {
        // S14 step 3: an `Empty` IC (the site never executed while interpreted)
        // no longer BLOCKS compilation. It is now COMPILABLE as an uncommon
        // TRAP (`SiteFeedback::Untaken` -> `inline::decide` -> `Ir::UncommonTrap`
        // at the generic send site in `ir.rs`): Self's lazy cold path. When the
        // trap fires it re-executes the send interpreted, warming the IC for the
        // NEXT compilation. So `Empty` is `Yes` here, not `NoRetryLater` — a
        // method whose ONLY sends are `Empty` still compiles (a method full of
        // traps, valid, just cold). This is what lets a first-call-is-the-
        // trigger method (`MACVM_JIT=threshold=1`, where every inner send is
        // guaranteed cold on the first attempt) compile immediately instead of
        // deferring to a warmer next call.
        //
        // INTERIM DEOPT-STORM (documented, NOT solved in step 3): an executed
        // trap warms the IC but the nmethod stays Alive with the trap still
        // compiled, so it re-traps every call until recompiled with the warm
        // feedback. That recompile-on-trap loop is S14 step 8 (recompile.rs) and
        // needs `trap_counts`/`UNCOMMON_TRAP_LIMIT`, which S13 never built. The
        // storm is CORRECTNESS-PRESERVING (each trap re-executes the send
        // exactly, identical output) — just slow, bounded per run by the call
        // count. No trap counting / recompilation is added here.
        IcState::Empty => return Eligibility::Yes,
        IcState::Mono => {}
        // S14: a POLY or MEGA site no longer blocks compilation — it compiles as
        // a generic compiled-IC `CallSend` (`inline::decide` → `Call`, there is
        // no single target to speculate on). The S11 IC machinery handles the
        // polymorphism at runtime: the site's `bl` starts at `stub_resolve` and
        // transitions Mono → PIC → Mega exactly as an interpreter IC does
        // (S11 step 5, PIC/mega already built + tested). Inlining the dominant
        // poly case behind a klass guard (DominantWithSlowPath) is a later
        // optimization; this just STOPS BLOCKING the method. The `it_world`
        // differential gate covers the runtime correctness.
        IcState::Poly(_) | IcState::Mega => return Eligibility::Yes,
    }
    let site = InterpreterIc::at(method, ic_idx);
    let Some(target) = MethodOop::try_from(site.target()) else {
        // S14 step 8: a mono site whose target is a compiled NMETHOD ID (a smi
        // handle — `set_mono_compiled` rewrites the IC once the callee
        // compiles). The method-shape checks below need a MethodOop, but a
        // generic mono send compiles fine WITHOUT one (`Ir::CallSend` through
        // the S11 machinery, exactly the 962be22 widening) — and
        // `feedback::read_send_site` resolves compiled ids on its own for the
        // inline decision. The old `NoPermanent` here PERMANENTLY DISABLED any
        // method recompiled after its callee had compiled — the exact shape
        // the recompile-on-trap loop produces (found by its storm test).
        return Eligibility::Yes;
    };
    // S11 D7: a mono `basicNew` site clears eligibility. `ir.rs` turns it
    // into an inline `Ir::Alloc` when the receiver is a compile-time Slots
    // class constant, else an ordinary generic send to the basicNew
    // PRIMITIVE — which allocates and returns WITHOUT re-entering the
    // interpreter's bytecode (a shallow c2i-to-primitive hop, not the deep
    // block-activation reentrancy the broader D1 relaxation reverted in
    // step 7 was tripping on), so admitting it here is safe. `argc == 0`:
    // `basicNew:` (prim 24, indexable) is a different, non-inlined thing.
    if target.primitive() == crate::compiler::ir::PRIM_BASIC_NEW && site.argc() == 0 {
        return Eligibility::Yes;
    }
    // S14 step 4b/4c: a mono send whose callee is a cheap NON-PRIMITIVE body is
    // inlinable regardless of the receiver klass — `ir::convert` splices it
    // behind a receiver-klass guard (cold path = an uncommon trap). This admits
    // a mono-non-smi accessor send (`^self val`, `val` a leaf — 4b) OR a cheap
    // single-block NON-leaf callee (`run [ ^self bar ]` — 4c), either of which
    // would otherwise be rejected as a "mono but non-smi guard" below. A
    // PRIMITIVE target is excluded (its bytecode is only the failure fallback,
    // not its real behaviour). Kept in EXACT lockstep with
    // `inline::decide_with_budget`'s own gate (leaf OR inline-eligible non-leaf,
    // non-primitive, within budget) so the eligibility check and the actual
    // inline decision never disagree. Budget-checked at level 1 (tier-1 level).
    if target.primitive() == 0
        && crate::compiler::inline::inline_cost(target)
            <= crate::compiler::inline::budget_for_level(1).per_call_cost
        && (crate::compiler::inline::is_leaf(target)
            || crate::compiler::inline::is_inline_eligible_nonleaf(target))
    {
        return Eligibility::Yes;
    }
    // S14 (the S11-deferred D1 widening): any remaining Mono send is compilable
    // as a plain compiled-IC `Call` (S11). The site dispatches to its single
    // known target — a direct compiled call if that target is compiled, else a
    // shallow c2i hop that interprets it and returns. This is neither
    // smi-inlinable (handled as a fused fast path in `ir::is_smi_inlinable`) nor
    // inline-eligible (handled above), but S11's compiled-IC send path handles a
    // generic mono send, and S14 step 4c validated exactly this shape — an
    // inlined non-leaf callee's OWN sends become generic `CallSend`s and run
    // correctly, incl. c2i re-entry. So a mono send NO LONGER blocks compilation
    // regardless of its guard klass or its target's primitive; `ir::convert`
    // lowers it to `Ir::CallSend` (`inline::decide` -> `Call`). The `it_world`
    // differential gate (interpreter vs `MACVM_JIT=threshold=1`) is the safety
    // net for the broader c2i-reentrancy surface this opens.
    Eligibility::Yes
}

/// D4: `eligible`? -> decode -> convert -> regalloc -> emit -> publish ->
/// install. `None` on ineligibility (having set `compile_disabled` so the
/// counter-overflow trigger, S10 step 8, doesn't re-attempt this method
/// every 10k sends) or on code-cache exhaustion (having disabled the JIT
/// trigger for the rest of this process — every future attempt would fail
/// the identical way, so there is nothing to gain by trying again).
///
/// Allocates NOTHING on the Smalltalk heap (D4): `decode`/`convert`/
/// `regalloc`/`emit` are pure Rust computations over already-existing
/// bytecode/IC/pool data, so no `HandleScope` is needed here and no GC can
/// strike mid-compile.
pub fn compile_method(
    vm: &mut VmState,
    rcvr_klass: KlassOop,
    method: MethodOop,
) -> Option<NmethodId> {
    compile_method_versioned(vm, rcvr_klass, method, 0)
}

/// S14 step 8: [`compile_method`] with an explicit `version` — the
/// recompile-on-trap loop passes `old.version + 1` so the thrash cap
/// (`recompile::MAX_VERSIONS`) can count replacement generations.
pub fn compile_method_versioned(
    vm: &mut VmState,
    rcvr_klass: KlassOop,
    method: MethodOop,
    version: u8,
) -> Option<NmethodId> {
    compile_method_full(vm, rcvr_klass, method, version, None)
}

/// S15 A2: compile WITH an OSR entry at `osr_bci` (a root-scope loop-header
/// bci — the backward jump target that overflowed the loop counter). The
/// result is a NORMAL nmethod for the key (it also serves future calls; it
/// replaces the CodeTable entry) that additionally carries an `OsrMap` +
/// entry block. Declines (None) when the method/header shape is outside the
/// v1 OSR envelope — the caller resets the loop counter and keeps
/// interpreting.
/// S15: does `method`'s bytecode create any literal closure? OSR v1
/// declines such methods (see `compile_method_full`'s envelope comment).
fn method_has_closure(method: MethodOop) -> bool {
    let len = method.bytecode_len();
    let mut bci = 0;
    while bci < len {
        let (instr, next) = decode_at(method, bci);
        if matches!(instr, Instr::PushClosure { .. }) {
            return true;
        }
        bci = next;
    }
    false
}

pub fn compile_method_osr(
    vm: &mut VmState,
    rcvr_klass: KlassOop,
    method: MethodOop,
    osr_bci: u16,
) -> Option<NmethodId> {
    compile_method_full(vm, rcvr_klass, method, 0, Some(osr_bci))
}

fn compile_method_full(
    vm: &mut VmState,
    rcvr_klass: KlassOop,
    method: MethodOop,
    version: u8,
    osr_bci: Option<u16>,
) -> Option<NmethodId> {
    match eligibility_detail(vm, method) {
        Eligibility::Yes => {}
        Eligibility::NoPermanent => {
            method.set_compile_disabled();
            if vm.options.trace.is_enabled("jit") {
                eprintln!(
                    "[jit] ineligible, compile_disabled: {}",
                    selector_string(method)
                );
            }
            return None;
        }
        Eligibility::NoRetryLater => {
            // Counter left as-is (D1's own dont-compile-bit rationale is
            // about a permanently ineligible method re-firing every 10k
            // sends — a cold IC isn't that; it's warm by the time this
            // same method is next called, see Eligibility's own doc).
            if vm.options.trace.is_enabled("jit") {
                eprintln!(
                    "[jit] not yet eligible (cold inner IC), retry next call: {}",
                    selector_string(method)
                );
            }
            return None;
        }
    }

    // S15 A2 v1 OSR envelope: no `has_ctx` (the interpreter frame already
    // owns a materialized Context; compiling root ctx-access through the
    // incoming Context oop is a later slice — the flagship loop shapes,
    // frontend-inlined to:do:/whileTrue:, are ctx-free) and no closures
    // (escape-mode operand stacks can hold phantoms the transfer buffer
    // cannot represent).
    if osr_bci.is_some() && (method.has_ctx() || method_has_closure(method)) {
        return None;
    }

    let cfg = decode::decode(method);
    let ir_method = ir::convert(vm, rcvr_klass, method, &cfg);
    let regalloc_result = regalloc::regalloc(&ir_method);

    let mut asm = JasmAssembler::new();
    let stub_poll_addr = vm.stubs.stub_poll_addr();
    let must_be_boolean_addr = vm.stubs.must_be_boolean_addr();
    let alloc_slow_addr = vm.stubs.alloc_slow_addr();
    let guard = emit::EntryGuard {
        smi_klass_bits: vm.universe.smi_klass.oop().raw(),
        key_klass_bits: rcvr_klass.oop().raw(),
        resolve_addr: vm.stubs.resolve_addr(),
    };
    // S15 A2 steps 4-5: locate the header block (decode block index == IR
    // block id for original blocks), resolve its live-in entities against
    // its own linear start position, and hand emit the copy plan. Entities
    // whose interval is DEAD at the header resolve to Nil and are OMITTED
    // (scope descs materialize dead slots as nil on a later deopt — the
    // doc's honest-caveat rule). A header that cannot be found (bci not a
    // block start) or whose entry stack is unavailable declines the whole
    // OSR compile.
    let mut osr_plan: Option<(emit::EmitOsr, Vec<crate::compiler::scopes::OsrSlot>)> = None;
    if let Some(bci) = osr_bci {
        let header_ix = cfg
            .blocks
            .iter()
            .position(|b| b.bci_start == bci as usize)?;
        let header = crate::compiler::ir::BlockId(header_ix as u32);
        let header_pos = *regalloc_result
            .block_start_pos
            .get(&header_ix.try_into().ok()?)?;
        let entry_stack = &ir_method.blocks[header_ix].entry_stack;

        use crate::compiler::scopes::{OsrSlot, OsrSource, ValueLoc};
        let n_slots = method.argc() + method.ntemps();
        let mut sources: Vec<(OsrSource, crate::compiler::ir::VReg)> =
            vec![(OsrSource::Receiver, crate::compiler::ir::VReg(0))];
        for i in 0..n_slots {
            sources.push((
                OsrSource::Slot(i as u16),
                crate::compiler::ir::VReg((1 + i) as u32),
            ));
        }
        for (j, &v) in entry_stack.iter().enumerate() {
            sources.push((OsrSource::StackSlot(j as u16), v));
        }
        let mut copies = Vec::new();
        let mut slots = Vec::new();
        for (src, v) in sources {
            match crate::compiler::scopes::resolve_frame_loc(
                v,
                header_pos,
                &regalloc_result.intervals,
            ) {
                ValueLoc::FrameSlot(off) => {
                    debug_assert!(off < 0 && off % 8 == 0);
                    let slot = crate::compiler::regalloc::SpillSlot((-off / 8 - 1) as u16);
                    copies.push(slot);
                    slots.push(OsrSlot {
                        src,
                        dst_frame_off: off,
                    });
                }
                ValueLoc::Nil => {} // dead at the header — omitted
                other => {
                    debug_assert!(
                        false,
                        "OSR live-in resolved to non-slot {other:?} (compiler bug)"
                    );
                    return None;
                }
            }
        }
        osr_plan = Some((
            emit::EmitOsr {
                header,
                copies,
                reload_pos: header_pos,
            },
            slots,
        ));
    }
    let osr_req: Option<emit::EmitOsr> = osr_plan.as_ref().map(|(req, _)| emit::EmitOsr {
        header: req.header,
        copies: req.copies.clone(),
        reload_pos: req.reload_pos,
    });
    let frame_slots_for_osr = regalloc_result.frame_slots;
    let (blob, block_pcs, verified_entry_off, emitted_ic_sites, safepoint_pcs, osr_off) =
        emit::emit(
            &mut asm,
            &ir_method,
            &regalloc_result,
            stub_poll_addr,
            must_be_boolean_addr,
            alloc_slow_addr,
            Some(guard),
            osr_req.as_ref(),
        );

    // D4.6: pre-resolve every `send_super` site's own target BEFORE
    // publishing anything -- resolving here (not at runtime, via
    // `stub_resolve`, like every other site) is the whole point of a
    // super send's own static klass. A lookup failure (the superclass
    // genuinely doesn't implement this selector) fails the WHOLE method's
    // own compile, same as any other compile failure here -- falls back
    // to the interpreter, which already handles a super-send DNU
    // correctly via its own existing mechanism; no new runtime-DNU-from-
    // compile-time path is needed. Allocates no Smalltalk heap memory
    // (`lookup`/`resolve_target_entry`'s own c2i-adapter fallback only
    // ever touches the CODE cache), so this doesn't disturb this
    // function's own "no HandleScope needed, no GC can strike mid-
    // compile" invariant (this function's own doc, above).
    //
    // `use_verified=true` here is load-bearing, not a micro-optimization:
    // a super site's own receiver is whatever `self` actually is at the
    // call site (`Leaf`, or any further subclass) -- essentially NEVER
    // the resolved target's own `key_klass` (`super_klass`, the STATIC
    // holder's superclass), since that's the whole point of `super`. If
    // the target were reached via its own `entry` (guarded, `use_verified
    // =false` -- Mono's own convention, since a Mono site's `bl` really
    // could later see a different RECEIVER klass through the same site),
    // that guard would almost always MISMATCH and misroute back through
    // `stub_resolve`, which re-resolves from `klass_of(receiver)` -- the
    // receiver's own actual klass, NOT `super_klass` -- silently
    // collapsing back into an ordinary dynamic send and reaching an
    // override `super` was specifically supposed to skip. Caught by this
    // step's own integration test (a 3-klass override chain where an
    // ordinary send and a super send provably reach different methods);
    // `entry` was the first thing tried and failed exactly this way.
    let mut super_resolutions: Vec<Option<(KlassOop, u64)>> =
        Vec::with_capacity(emitted_ic_sites.len());
    for s in &emitted_ic_sites {
        let resolved = match ir_method.call_sites[s.site as usize].static_klass {
            Some(super_klass) => {
                let target_method = lookup(vm, super_klass, s.selector)?;
                let target = resolve_super_target_entry(vm, super_klass, s.selector, target_method);
                Some((super_klass, target))
            }
            None => None,
        };
        super_resolutions.push(resolved);
    }

    let Some(h) = vm.code_cache.alloc(blob.code.len()) else {
        vm.options.jit = JitMode::Off;
        if vm.options.trace.is_enabled("jit") {
            eprintln!(
                "[jit] code cache exhausted ({} bytes wanted), disabling JIT for the rest of \
                 this run",
                blob.code.len()
            );
        }
        return None;
    };
    vm.code_cache.publish(h, &blob);

    // S12 D2: `oopmaps[0]` is always the reserved, always-empty map —
    // block-start descs (trace path only, never a real safepoint's own
    // return address, `OopMap::empty`'s own doc) point at it; every REAL
    // safepoint below gets its own liveness-intersected map, deduplicated
    // by content (`oopmap_dedup`: two safepoints with identical live sets
    // share one entry).
    let mut oopmaps: Vec<OopMap> = vec![OopMap::empty()];
    let mut safepoint_pcdescs: Vec<PcDesc> = Vec::with_capacity(safepoint_pcs.len());
    for sp in &safepoint_pcs {
        let map = oopmap::build_for_position(
            &regalloc_result.intervals,
            regalloc_result.frame_slots,
            sp.position,
        );
        let idx = oopmap::intern(&mut oopmaps, map);
        safepoint_pcdescs.push(PcDesc {
            pc_off: sp.pc_off,
            bci: sp.bci,
            oopmap: idx,
        });
    }
    let mut pcdescs: Vec<PcDesc> = block_pcs
        .iter()
        .map(|bp| PcDesc {
            pc_off: bp.pc_off,
            bci: bp.bci,
            oopmap: 0,
        })
        .chain(safepoint_pcdescs)
        .collect();
    // `bci_at`'s nearest-below lookup binary-searches this, ascending.
    pcdescs.sort_by_key(|d| d.pc_off);
    let key_selector = SymbolOop::try_from(method.selector())
        .expect("compile_method: method selector is not a Symbol");

    // Block-bci granularity (matching pcdescs' own precision): the block
    // containing this method's Ir::Poll, if it has one. A mixed-tier
    // stack-trace walker has no exact native pc to work from at a poll
    // callback (S11's last_compiled_pc anchor isn't wired up yet), so this
    // is the IR-derived approximation `Nmethod::poll_bci` documents.
    let poll_bci = ir_method
        .blocks
        .iter()
        .find(|b| b.code.iter().any(|instr| matches!(instr, ir::Ir::Poll)))
        .map(|b| b.bci);

    // S11 D3: every fresh site starts Unresolved -- its `bl` still
    // self-targets (`bl_patchable`'s own placeholder) until the patch pass
    // just below aims it at `stub_resolve` (D3: "no per-site
    // pre-resolution", exactly like an empty interpreter IC) -- EXCEPT a
    // `send_super` site (D4.6), already resolved above: it starts
    // `Mono{klass, target}` directly, its `bl` already pointing at the
    // real target, never touching `stub_resolve` at all on a normal run.
    let ic_sites: Vec<IcSite> = emitted_ic_sites
        .iter()
        .zip(&super_resolutions)
        .map(|(s, resolved)| IcSite {
            off: s.off,
            selector: s.selector,
            argc: s.argc,
            state: match resolved {
                Some((klass, target)) => CompiledIcState::Mono {
                    klass: *klass,
                    target: *target,
                },
                None => CompiledIcState::Unresolved,
            },
            // S13 step 10d: `super_resolutions` is `Some` iff this is a
            // `send_super` site (D4.6) — its `klass` IS the static
            // holder-superclass. Record it so a later `not_entrant_stub`
            // re-dispatch stays super-aware even if the state is reset.
            super_klass: (*resolved).map(|(klass, _)| klass),
        })
        .collect();

    // S13 step 3b: build this method's deopt scope blob + PcDescs from the
    // safepoints the converter flagged (`ir::IrBlock::deopt_sites`). Every
    // value read after a resume bci is spilled (S12 spill-all), so
    // `scopes::resolve_frame_loc` alone resolves receiver/slots/stack; a
    // value not live at the safepoint is dead -> Nil.
    let (deopt_scopes, deopt_pcdescs) =
        build_deopt_metadata(&ir_method, &regalloc_result, &safepoint_pcs);

    let nm = Nmethod {
        id: NmethodId(0), // overwritten by CodeTable::install
        key_klass: rcvr_klass,
        key_selector,
        code: h,
        entry_off: 0,
        verified_entry_off,
        state: NmState::Alive,
        level: 1,
        version,
        trap_count: 0,
        // S14 step 8 (A5): the feedback profile this compile SAW — the
        // effectiveness check re-snapshots at trap time and declines the
        // recompile when nothing changed.
        profile_hash: crate::compiler::feedback::snapshot_profile(vm, method),
        literal_off: blob.literal_off,
        relocs: blob.relocs,
        frame_slots: regalloc_result.frame_slots,
        slot_is_oop: regalloc_result.slot_is_oop.clone(),
        pcdescs,
        oopmaps,
        ic_sites,
        poll_bci,
        deopt_scopes,
        deopt_pcdescs,
        // S14 step 4b: the inline dependencies the converter recorded (one
        // `(receiver_klass, selector)` per spliced leaf). `deps::
        // affected_by_install` reads these to invalidate this nmethod when an
        // inlined callee is redefined.
        inline_deps: ir_method.inline_deps.clone(),
        self_devirt: ir_method.self_devirt,
        osr_map: osr_plan.as_ref().map(|(_, slots)| {
            let frame_bytes = ((8 * frame_slots_for_osr as i64) + 15) & !15;
            crate::compiler::scopes::OsrMap {
                osr_bci: osr_bci.expect("osr_plan implies osr_bci"),
                entry_off: osr_off.expect("emit built the entry block when asked"),
                frame_words: (frame_bytes / 8) as u32,
                slots: slots.clone(),
            }
        }),
    };
    // S12 D1 enforcement point 2: "debug + stress" — reuses the existing
    // heap-verifier's own gate (`MACVM_GC_VERIFY=1` opts a release build
    // in too) rather than a second, parallel env var for the same concept.
    if crate::memory::verify::verify_enabled() {
        oopmap::verify(&nm);
    }
    let resolve_addr = vm.stubs.resolve_addr();
    for (site, resolved) in emitted_ic_sites.iter().zip(&super_resolutions) {
        let patch_target = resolved.map_or(resolve_addr, |(_, target)| target);
        vm.code_cache.patch_branch26_at(h, site.off, patch_target);
    }
    vm.stats.compilations += 1; // S15 A8 tier-balance counter
    let has_osr = nm.osr_map.is_some();
    let id = vm.code_table.install(nm);
    if has_osr {
        let sel = crate::oops::wrappers::SymbolOop::try_from(method.selector())
            .expect("a method's selector is always a Symbol");
        vm.code_table.install_osr(
            rcvr_klass,
            sel,
            osr_bci.expect("has_osr implies osr_bci"),
            id,
        );
    }
    if vm.options.trace.is_enabled("jit") {
        eprintln!(
            "[jit] compiled {} -> nmethod {} ({} bytes, {} frame slots)",
            selector_string(method),
            id.0,
            h.len,
            regalloc_result.frame_slots
        );
    }
    Some(id)
}

fn selector_string(method: MethodOop) -> String {
    SymbolOop::try_from(method.selector())
        .map(|s| s.as_string())
        .unwrap_or_else(|| "<?>".to_string())
}

/// S13 step 3b: build a freshly compiled method's deopt scope blob + sorted
/// `scopes::PcDesc`s from the converter-flagged safepoints
/// (`ir::IrBlock::deopt_sites`).
///
/// Re-walks `block_order` in the SAME linear-position numbering `regalloc`
/// (`compute_intervals`) and `emit` share, so each deopt site's op position
/// is re-derived here and matched — via that position — to the emitted
/// [`emit::SafepointPc`] carrying its return-address `pc_off` (the deopt
/// `PcDesc.code_off` key, same value the S12 oopmap `PcDesc` uses for a
/// call). A deopt site is always a safepoint-emitting op, so its position
/// is always present in the map; a divergence between this walk and emit's
/// would surface as a hard panic (not a silent wrong-frame deopt), and the
/// golden decodes the blob to catch a subtler slip.
///
/// Every `VReg` resolves via [`scopes::resolve_frame_loc`]: S12's spill-all
/// invariant puts every value live across a safepoint in its canonical
/// frame slot, and a value NOT live there is dead → `Nil` (safe — never
/// read before being overwritten). One scope record PER SITE (undeduped):
/// linear-scan assigns a spill slot per interval, so a multi-def temp has
/// no single canonical slot; resolving at each site's own position is
/// unambiguous. `method_pool_ix` is a `0` placeholder until materialization
/// (step 6) interns the compile-time method oop — leaving the pool
/// untouched keeps existing listing goldens byte-stable.
fn build_deopt_metadata(
    ir_method: &ir::IrMethod,
    regalloc_result: &regalloc::RegallocResult,
    safepoint_pcs: &[emit::SafepointPc],
) -> (Vec<u8>, Vec<crate::compiler::scopes::PcDesc>) {
    use crate::compiler::scopes::{
        resolve_frame_loc, CtxLoc, SafepointState, ScopeDescData, ScopeDescRecorder, SenderLink,
    };

    let sp_by_pos: std::collections::HashMap<u32, &emit::SafepointPc> =
        safepoint_pcs.iter().map(|sp| (sp.position, sp)).collect();

    let intervals = &regalloc_result.intervals;
    let n_slots = ir_method.argc as usize + ir_method.ntemps as usize;
    let mut rec = ScopeDescRecorder::new();
    let mut pos = 0u32;
    for &bid in &regalloc_result.block_order {
        let block = &ir_method.blocks[bid.0 as usize];
        for (idx, _ir) in block.code.iter().enumerate() {
            if let Some((_, raw)) = block.deopt_sites.iter().find(|(ci, _)| *ci == idx as u32) {
                let sp = sp_by_pos.get(&pos).unwrap_or_else(|| {
                    panic!(
                        "build_deopt_metadata: deopt site at position {pos} has no emitted \
                         safepoint -- emit/regalloc position numbering diverged"
                    )
                });
                let position = sp.position; // == pos (map is keyed by it)
                                            // The ROOT (caller) scope — the depth-1 shape S13 always built:
                                            // this method's own receiver (VReg 0) + unified slots
                                            // (VReg 1..=n_slots), `sender: None`, root method_pool_ix.
                let root_receiver = resolve_frame_loc(ir::VReg(0), position, intervals);
                let root_slots = (0..n_slots)
                    .map(|i| resolve_frame_loc(ir::VReg(i as u32 + 1), position, intervals))
                    .collect();
                let root_method_ix = ir_method
                    .method_pool_ix
                    .expect("a method with a deopt site interned its own method oop");

                // S14 step 7-II-b: if M owns a heap Context that was ELIDED (its
                // captured temps promoted to `ctx_vregs`), the root scope records
                // `CtxLoc::Elided` over those vregs' frame slots so a deopt allocs
                // a fresh Context and fills it (materializer M6). No ctx-vregs →
                // M never owned a Context → `None` (unchanged S13 behaviour).
                let root_ctx = if ir_method.ctx_vregs.is_empty() {
                    CtxLoc::None
                } else {
                    CtxLoc::Elided {
                        temps: ir_method
                            .ctx_vregs
                            .iter()
                            .map(|&v| resolve_frame_loc(v, position, intervals))
                            .collect(),
                    }
                };

                // S14 step 4c: a safepoint INSIDE an inlined body records a
                // NESTED scope — the inlined callee's own receiver/slots/method
                // + a `SenderLink` to the (freshly begun) caller scope. A
                // root-method safepoint (`raw.inline == None`) records the
                // depth-1 caller scope directly, exactly as S13 did (no
                // regression — `sender: None`).
                let scope = match &raw.inline {
                    None => rec.begin_scope(ScopeDescData {
                        method_pool_ix: root_method_ix,
                        is_block: false,
                        sender: None,
                        receiver: root_receiver,
                        slots: root_slots,
                        ctx: root_ctx,
                    }),
                    Some(site) => {
                        // S14 step 7-IV-b: the inline levels form a CHAIN
                        // (`site.parent` — a block spliced inside an inlined
                        // callee is depth 3: block ← callee ← root). Begin the
                        // ROOT scope first (`begin_scope`'s invariant: a
                        // SenderLink's target must exist before its child), then
                        // each level OUTERMOST-first; every pre-7-IV site has
                        // `parent: None` (a 2-scope chain, byte-identical to the
                        // old shape).
                        let mut chain: Vec<&ir::InlineSite> = vec![site];
                        while let Some(p) = &chain.last().unwrap().parent {
                            chain.push(p);
                        }
                        let mut prev_scope = rec.begin_scope(ScopeDescData {
                            method_pool_ix: root_method_ix,
                            is_block: false,
                            sender: None,
                            receiver: root_receiver,
                            slots: root_slots,
                            ctx: root_ctx,
                        });
                        for level in chain.iter().rev() {
                            // The SenderLink: where this level's CALLER resumes
                            // (the inlined send's bci, advanced past it by the
                            // materializer) and the caller's frozen operand
                            // stack below the send's operands.
                            let pending_stack = level
                                .caller_pending_stack
                                .iter()
                                .map(|&v| resolve_frame_loc(v, position, intervals))
                                .collect();
                            let inl_receiver =
                                resolve_frame_loc(level.receiver, position, intervals);
                            let mut inl_slots: Vec<_> = level
                                .slots
                                .iter()
                                .map(|&v| resolve_frame_loc(v, position, intervals))
                                .collect();
                            // S14 step 7-IV-c: a slot holding an ELIDED-CLOSURE
                            // phantom overrides its (filler) vreg location — the
                            // materializer allocates the real closure.
                            for &(slot_ix, pool_ix) in &level.slot_closures {
                                inl_slots[slot_ix as usize] =
                                    crate::compiler::scopes::ValueLoc::ElidedClosure(pool_ix);
                            }
                            prev_scope = rec.begin_scope(ScopeDescData {
                                method_pool_ix: level.method_pool_ix,
                                // S14 step 7-II: an inlined spliced BLOCK records
                                // an `is_block` scope (the materializer rebuilds
                                // a block activation frame).
                                is_block: level.is_block,
                                sender: Some(SenderLink {
                                    sender: prev_scope,
                                    sender_bci: level.sender_bci,
                                    pending_stack,
                                }),
                                receiver: inl_receiver,
                                slots: inl_slots,
                                ctx: CtxLoc::None,
                            });
                        }
                        prev_scope
                    }
                };
                let mut stack: Vec<_> = raw
                    .stack
                    .iter()
                    .map(|&v| resolve_frame_loc(v, position, intervals))
                    .collect();
                // S14 step 7-IV-c: phantom stack entries override their filler
                // vregs (a block-arg send's guard-cold reexecute stack; in-callee
                // sites with the phantom below a send's operands).
                for &(ix, pool_ix) in &raw.stack_closures {
                    stack[ix as usize] = crate::compiler::scopes::ValueLoc::ElidedClosure(pool_ix);
                }
                rec.record_site(
                    sp.pc_off,
                    SafepointState {
                        scope,
                        bci: raw.bci as u16,
                        kind: raw.kind,
                        reexecute: raw.reexecute,
                        stack,
                    },
                );
            }
            pos += 1;
        }
    }
    rec.pack()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::builder::BytecodeBuilder;
    use crate::runtime::vm_state::VmOptions;

    fn test_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            gc_stress_full_period: None,
            eden_kb: None,
            jit: JitMode::Off,
        })
    }

    /// A throwaway method standing in for a real SmallInteger primitive —
    /// `eligible` only ever reads its `primitive()` field, never executes
    /// its bytecode (same rationale as `ir::tests::primitive_stub`, not
    /// reused across the module boundary since it's a two-line helper).
    fn primitive_stub(
        vm: &mut VmState,
        sel: crate::oops::wrappers::SymbolOop,
        prim_id: i64,
    ) -> MethodOop {
        let mut b = BytecodeBuilder::new();
        b.ret_self();
        let m = b.finish(vm, sel, 1, 0);
        m.set_primitive(prim_id);
        m
    }

    /// `self + arg`, argc=1: `push_self; push_temp(0); send(#+); ret_tos`.
    /// The IC is left at whatever state the caller sets up afterward —
    /// `ic_idx` 0 is always this send site's own IC (the method's only
    /// send).
    fn plus_method(vm: &mut VmState) -> MethodOop {
        let plus_sel = vm.universe.intern(b"+");
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.push_temp(0);
        b.send(vm, plus_sel, 1);
        b.ret_tos();
        let m_sel = vm.universe.intern(b"plusArg:");
        b.finish(vm, m_sel, 1, 0)
    }

    fn set_mono_smi_plus_ic(vm: &mut VmState, method: MethodOop) {
        let plus_sel = vm.universe.intern(b"+");
        let plus_target = primitive_stub(vm, plus_sel, 1);
        let smi_klass = vm.universe.smi_klass;
        let epoch = vm.ic_epoch;
        InterpreterIc::at(method, 0).set_mono(vm, smi_klass, plus_target, epoch);
    }

    #[test]
    fn eligible_accepts_mono_smi_plus() {
        let mut vm = test_vm();
        let method = plus_method(&mut vm);
        set_mono_smi_plus_ic(&mut vm, method);
        assert!(eligible(&vm, method));
    }

    /// S14 step 3: an `Empty` IC (the site never executed while interpreted)
    /// is now ELIGIBLE — the generic send lowers to an uncommon trap
    /// (`SiteFeedback::Untaken` -> `inline::decide` -> `Ir::UncommonTrap`), so a
    /// cold site no longer blocks compilation. (Before this step it returned
    /// `NoRetryLater` -> `eligible == false`.)
    #[test]
    fn eligible_accepts_empty_ic_as_trap() {
        let mut vm = test_vm();
        let method = plus_method(&mut vm);
        // No `set_mono` call: the IC is left at its fresh, empty state — which
        // is now compilable (as a trap), not a compile blocker.
        assert!(eligible(&vm, method));
    }

    /// A NON-LEAF target used by the eligibility-rejection tests below: its
    /// bytecode contains a send, so `inline::is_leaf` is false and the S14
    /// step-4b leaf-inline eligibility relaxation doesn't admit it — the older
    /// mono-SMI/non-inline-primitive gates apply unchanged. (`primitive_stub`'s
    /// `^self` body IS a leaf and would now inline.)
    fn non_leaf_stub(vm: &mut VmState, sel: SymbolOop, prim_id: i64) -> MethodOop {
        let inner = vm.universe.intern(b"innerStubSel");
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.send(vm, inner, 0); // a send → non-leaf
        b.ret_tos();
        let m = b.finish(vm, sel, 1, 0);
        m.set_primitive(prim_id);
        m
    }

    /// S14 (S11-deferred D1 widening): a Mono IC guarded on a NON-smi klass and
    /// targeting a non-inlinable (too-big) non-leaf body is now ELIGIBLE — it
    /// compiles as a plain compiled-IC `Call` (was `NoPermanent`). A leaf callee
    /// would inline behind a klass guard (4b); this one is too big to inline, so
    /// it stays a generic dynamic send, which S11's IC machinery handles.
    #[test]
    fn eligible_accepts_generic_mono_non_smi_send() {
        let mut vm = test_vm();
        let method = plus_method(&mut vm);
        let plus_sel = vm.universe.intern(b"+");
        let plus_target = non_leaf_stub(&mut vm, plus_sel, 1);
        let object_klass = vm.universe.object_klass;
        let epoch = vm.ic_epoch;
        InterpreterIc::at(method, 0).set_mono(&mut vm, object_klass, plus_target, epoch);
        assert!(
            eligible(&vm, method),
            "a generic mono send compiles as a Call"
        );
    }

    /// Same widening for a Mono smi-guarded send to a non-inlinable-primitive
    /// (non-leaf) target: now compiles as a generic `Call` (was `NoPermanent`).
    #[test]
    fn eligible_accepts_generic_mono_primitive_target() {
        let mut vm = test_vm();
        let method = plus_method(&mut vm);
        let plus_sel = vm.universe.intern(b"+");
        let not_inline = non_leaf_stub(&mut vm, plus_sel, 23);
        let smi_klass = vm.universe.smi_klass;
        let epoch = vm.ic_epoch;
        InterpreterIc::at(method, 0).set_mono(&mut vm, smi_klass, not_inline, epoch);
        assert!(
            eligible(&vm, method),
            "a generic mono send compiles as a Call"
        );
    }

    /// D4.6 (S11 step 6): a `super` send's own target is resolved
    /// statically at compile time, not through the interpreter's own IC
    /// lattice — so, unlike an ordinary send, its own site's IC state is
    /// irrelevant to eligibility. `set_mono_smi_plus_ic` is deliberately
    /// NOT called here (a super send would ignore it anyway): this method
    /// must be eligible on the strength of the super send alone.
    #[test]
    fn eligible_allows_super_send() {
        let mut vm = test_vm();
        let plus_sel = vm.universe.intern(b"+");
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.push_temp(0);
        b.send_super(&mut vm, plus_sel, 1);
        b.ret_tos();
        let m_sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, m_sel, 1, 0);
        assert!(eligible(&vm, method));
    }

    /// S13 step 10d: compiling a `send_super` site stamps its STATIC
    /// holder-superclass onto the runtime `IcSite` (`super_klass`) — the marker
    /// that keeps a later `not_entrant_stub` re-dispatch super-aware instead of
    /// collapsing into a receiver-klass dynamic send. A method on SmallInteger
    /// doing `super +` resolves against Integer (smi's superclass), so its
    /// compiled site must carry `super_klass == Some(Integer)`.
    #[test]
    fn compiled_super_send_records_static_super_klass() {
        let mut vm = test_vm();
        let plus_sel = vm.universe.intern(b"+");
        let smi_klass = vm.universe.smi_klass;
        let integer_klass = vm.universe.integer_klass; // smi_klass.superclass()

        // The super target must resolve: install `+` on Integer (smi's super).
        let int_plus = primitive_stub(&mut vm, plus_sel, 1);
        crate::runtime::lookup::install_method(&mut vm, integer_klass, plus_sel, int_plus);

        // `superPlus: arg [ ^super + arg ]`, holder = SmallInteger.
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.push_temp(0);
        b.send_super(&mut vm, plus_sel, 1);
        b.ret_tos();
        let m_sel = vm.universe.intern(b"superPlus:");
        let method = b.finish(&mut vm, m_sel, 1, 0);
        crate::runtime::lookup::install_method(&mut vm, smi_klass, m_sel, method);

        let id = compile_method(&mut vm, smi_klass, method).expect("super-send method compiles");
        let nm = vm.code_table.get(id).unwrap();
        assert_eq!(
            nm.ic_sites.len(),
            1,
            "the one super send is the method's only IC site"
        );
        assert_eq!(
            nm.ic_sites[0].super_klass.map(|k| k.oop().raw()),
            Some(integer_klass.oop().raw()),
            "the super site records Integer (smi's superclass) as its static super_klass"
        );
    }

    #[test]
    fn eligible_rejects_argc_over_five() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        b.ret_self();
        let sel = vm.universe.intern(b"sixArgs:x:x:x:x:x:");
        let method = b.finish(&mut vm, sel, 6, 0);
        assert!(!eligible(&vm, method));
    }

    /// S14 step 7-I (was `eligible_rejects_closure_op`): a `PushClosure` no
    /// longer excludes a method outright. The escape pre-pass classifies each
    /// closure site; here `[self]` is immediately `pop`ped — a DEAD closure
    /// (SPEC A7 step 2(b) "no use") is elidable, so the method is eligible and
    /// `ir::convert` elides the closure entirely (compiles to `^self`). An
    /// ESCAPING closure (stored/returned/passed) still keeps the method
    /// interpreted — covered by `eligible_rejects_escaping_closure` below and
    /// the `escape.rs` unit tests / `it_tier1` differential tests.
    #[test]
    fn eligible_allows_dead_closure() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        let lit = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, _vm| {
            blk.push_self();
            blk.ret_tos();
        });
        b.push_closure(lit, 0);
        b.pop();
        b.ret_self();
        let sel = vm.universe.intern(b"withBlock");
        let method = b.finish(&mut vm, sel, 0, 0);
        assert!(
            eligible(&vm, method),
            "a dead (immediately popped) closure is elidable → eligible (7-I)"
        );
    }

    /// S14 step 7-I: an ESCAPING closure (stored into an instvar → a compiled
    /// frame cannot be its `home_frame_ref`) keeps the whole method interpreted
    /// — the "inline-or-gated" soundness boundary.
    #[test]
    fn eligible_rejects_escaping_closure() {
        let mut vm = test_vm();
        let holder = vm.universe.new_klass(
            vm.universe.object_klass,
            "S14EscapeUnit",
            crate::oops::Format::Slots,
            false,
            crate::oops::layout::HEADER_WORDS + 1,
        );
        let mut b = BytecodeBuilder::new();
        let lit = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, _vm| {
            blk.push_self();
            blk.ret_tos();
        });
        b.push_closure(lit, 0);
        b.store_instvar_pop(0);
        b.ret_self();
        let sel = vm.universe.intern(b"stash");
        let method = b.finish(&mut vm, sel, 0, 0);
        crate::runtime::lookup::install_method(&mut vm, holder, sel, method);
        assert!(
            !eligible(&vm, method),
            "a stored (escaping) closure keeps the method interpreted (7-I gate)"
        );
    }

    /// S14 step 7-II-b: `has_ctx` is no longer an outright reject (a real
    /// has_ctx method's Context ELIDES when its capturing blocks inline). But
    /// this DEGENERATE method sets `has_ctx` with NO closure at all — nothing
    /// justifies the elision, so it stays interpreted (the `has_ctx && no-closure`
    /// guard). A real has_ctx-with-elidable-block method IS eligible — covered by
    /// the `it_tier1` captured-temp differential tests.
    #[test]
    fn eligible_rejects_has_ctx() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        b.ret_self();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 0, 0);
        method.set_flags(0, 0, true, false, false, false, 1); // has_ctx = true
        assert!(!eligible(&vm, method));
    }

    /// S11 step 7 (D1): `StoreInstvarPop` is now allowed — the write
    /// barrier (`ir::Ir::StoreField{barrier:true}`) and real slow-path
    /// sends this needed are exactly this step's own deliverables.
    #[test]
    fn eligible_allows_instvar_store() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.store_instvar_pop(0);
        b.ret_self();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 0, 0);
        assert!(eligible(&vm, method));
    }

    /// S14: a polymorphic IC (more than one klass seen at a send site) is now
    /// ELIGIBLE — it compiles as a generic compiled-IC `CallSend` and the S11
    /// PIC machinery handles the polymorphism at runtime (was `NoPermanent`).
    #[test]
    fn eligible_accepts_poly_ic() {
        let mut vm = test_vm();
        let plus_sel = vm.universe.intern(b"+");
        let plus_target = primitive_stub(&mut vm, plus_sel, 1);

        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.push_temp(0);
        b.send(&mut vm, plus_sel, 1);
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 1, 0);

        let smi_klass = vm.universe.smi_klass;
        let other_klass = vm.universe.object_klass;
        let array_klass = vm.universe.array_klass;
        let pairs = crate::memory::alloc::alloc_indexable_oops(
            &mut vm,
            array_klass,
            crate::oops::layout::IC_POLY_ARRAY_LEN,
        );
        pairs.at_put(0, smi_klass.oop());
        pairs.at_put(1, plus_target.oop());
        pairs.at_put(2, other_klass.oop());
        pairs.at_put(3, plus_target.oop());
        let epoch = vm.ic_epoch;
        InterpreterIc::at(method, 0).set_poly(&mut vm, pairs, epoch);

        assert!(
            eligible(&vm, method),
            "a poly send compiles as a generic Call (PIC handles it at runtime)"
        );
    }

    /// D1 point 3: a method with a primitive attached stays interpreted —
    /// its own Rust fast path is already fast, and S10's bailout-by-
    /// restart soundness argument doesn't extend to primitives (which
    /// really do call into Rust and can allocate).
    #[test]
    fn eligible_rejects_primitive_method() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.push_temp(0);
        b.ret_tos();
        let sel = vm.universe.intern(b"+");
        let method = b.finish(&mut vm, sel, 1, 0);
        method.set_primitive(1);
        assert!(!eligible(&vm, method));
    }

    /// D1's `NoPermanent` path: a STRUCTURALLY ineligible method (here,
    /// argc > 5 — never becomes eligible no matter how many times it's
    /// called) gets `compile_disabled` set (and its invocation count reset
    /// to 0 as a side effect of sharing the smi) rather than being
    /// silently skipped, so the interpreter's counter-overflow trigger
    /// (S10 step 8) doesn't re-attempt it forever.
    #[test]
    fn compile_method_disables_permanently_ineligible_method() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        b.ret_self();
        let sel = vm.universe.intern(b"sixArgs:x:x:x:x:x:");
        let method = b.finish(&mut vm, sel, 6, 0);
        assert!(!method.compile_disabled());
        let smi_klass = vm.universe.smi_klass;
        let id = compile_method(&mut vm, smi_klass, method);
        assert!(id.is_none());
        assert!(method.compile_disabled());
        assert_eq!(
            method.counters() & crate::oops::layout::COUNTERS_INVOCATION_MASK,
            0
        );
    }

    /// S14 step 3: a method whose only inner send is a still-cold (`Empty`) IC
    /// now COMPILES — the send lowers to an uncommon trap
    /// (`SiteFeedback::Untaken` -> `Ir::UncommonTrap`), Self's lazy cold path.
    /// Before this step the cold IC returned `NoRetryLater` and `compile_method`
    /// declined with `None` (leaving the counter alone for a warmer retry); now
    /// there is nothing to defer — the trap re-executes the send interpreted
    /// when hit, warming the IC for a *later* recompile (S14 step 8). The method
    /// is neither compile-disabled nor left uncompiled.
    #[test]
    fn compile_method_compiles_cold_ic_method_as_trap() {
        let mut vm = test_vm();
        let method = plus_method(&mut vm);
        // Left with an empty inner IC: now compilable as a trap.
        assert!(!method.compile_disabled());
        method.set_counters(41);
        let smi_klass = vm.universe.smi_klass;
        let id = compile_method(&mut vm, smi_klass, method);
        assert!(
            id.is_some(),
            "a cold inner IC now compiles as an uncommon trap, not a compile blocker"
        );
        assert!(
            !method.compile_disabled(),
            "a successful compilation never sets the don't-compile bit"
        );
        // The nmethod carries a deopt PcDesc — the trap site itself — proving
        // the send lowered to a trap, not a real `CallSend`.
        assert!(
            !vm.code_table
                .get(id.unwrap())
                .expect("installed")
                .deopt_pcdescs
                .is_empty(),
            "the trapped send must carry at least one deopt PcDesc (the trap site)"
        );
    }

    /// `compile_method` on an eligible method installs a real, gettable
    /// `Nmethod` keyed on the receiver klass passed in. The full
    /// pipeline's actual machine code is exercised end to end (through a
    /// real `call_stub` invocation, not just "did this return `Some`") by
    /// `compiled_plus_arg_executes_correctly` in `tests/it_tier1.rs` — that
    /// needs raw FFI `unsafe`, which this crate's `#![deny(unsafe_code)]`
    /// forbids outside a handful of codegen modules, so the executing half
    /// of this same scenario lives in the separate `tests/` crate instead.
    #[test]
    fn compile_method_installs_eligible_method() {
        let mut vm = test_vm();
        let method = plus_method(&mut vm);
        set_mono_smi_plus_ic(&mut vm, method);

        let smi_klass = vm.universe.smi_klass;
        let id = compile_method(&mut vm, smi_klass, method).expect("eligible method must compile");

        let nm = vm
            .code_table
            .get(id)
            .expect("installed nmethod must be gettable");
        assert_eq!(nm.key_klass.oop().raw(), smi_klass.oop().raw());
        assert_eq!(
            vm.code_table
                .lookup(smi_klass, SymbolOop::try_from(method.selector()).unwrap()),
            Some(id)
        );
        assert!(!method.compile_disabled());
    }

    /// S13 step 3b golden: the full deopt-metadata pipeline — capture
    /// (`ir.rs::translate_instr`) → correlate + resolve + pack
    /// (`build_deopt_metadata`) → decode — on a method with a NON-EMPTY
    /// operand stack live across a safepoint, so all three `ValueLoc`
    /// dimensions (receiver, arg/temp slots, operand stack) are exercised
    /// against REAL regalloc output, not hand-faked intervals.
    ///
    /// `foo: a [ ^self box: (self bar) with: a ]`: at the inner `self bar`
    /// send, the OUTER `self` (the box:with: receiver) is still on the
    /// operand stack, and both `self` (used again as the box:with: receiver)
    /// and `a` (its second arg) are live across it — S12 spill-all forces
    /// all three to canonical frame slots, and the decoded scope must name
    /// exactly those. Both sends are generic `CallSend`s (Call,
    /// reexecute=false). Driven through decode/convert/regalloc/emit
    /// directly (not `compile_method`, whose eligibility gate rejects
    /// generic non-smi sends) so the recorder runs on this exact shape.
    #[test]
    fn deopt_scope_blob_records_real_valuelocs() {
        use crate::compiler::scopes::{decode_scope, decode_site, SafepointKind, ValueLoc};
        use ir::VReg;

        let mut vm = test_vm();
        let bar_sel = vm.universe.intern(b"bar");
        let box_sel = vm.universe.intern(b"box:with:");
        let foo_sel = vm.universe.intern(b"foo:");

        let mut b = BytecodeBuilder::new();
        b.push_self(); // box:with: receiver
        b.push_self(); // bar receiver
        b.send(&mut vm, bar_sel, 0); // (self bar)                    <- SITE 0
        b.push_temp(0); // the arg `a`
        b.send(&mut vm, box_sel, 2); // self box: (self bar) with: a  <- SITE 1
        b.ret_tos();
        let method = b.finish(&mut vm, foo_sel, 1, 0);

        // S14 step 3: warm both send sites to Mono on a NON-smi klass so they
        // stay generic `CallSend`s (this test's premise). An Empty IC would now
        // lower to an uncommon TRAP (`SiteFeedback::Untaken`), which is a
        // different scope shape exercised by the it_tier1 trap test — here we
        // want the two-CallSend safepoint layout. S14 step 4c: the targets are
        // deliberately NON-INLINABLE (each contains a SUPER send, which the
        // inliner gates out), so the inliner leaves both sites as real
        // `CallSend`s rather than splicing a body inline (a leaf accessor OR a
        // plain non-leaf now inlines — either would collapse a safepoint and
        // break the two-CallSend premise).
        let obj_klass = vm.universe.object_klass;
        let inner_sel = vm.universe.intern(b"inner");
        let bar_target = {
            let mut tb = BytecodeBuilder::new();
            tb.push_self();
            tb.send_super(&mut vm, inner_sel, 0); // a SUPER send → not inlinable
            tb.ret_tos();
            tb.finish(&mut vm, bar_sel, 0, 0)
        };
        let box_target = {
            let mut tb = BytecodeBuilder::new();
            tb.push_self();
            tb.send_super(&mut vm, inner_sel, 0); // a SUPER send → not inlinable
            tb.ret_tos();
            tb.finish(&mut vm, box_sel, 2, 0)
        };
        let epoch = vm.ic_epoch;
        InterpreterIc::at(method, 0).set_mono(&mut vm, obj_klass, bar_target, epoch);
        InterpreterIc::at(method, 1).set_mono(&mut vm, obj_klass, box_target, epoch);

        let cfg = decode::decode(method);
        let ir_method = ir::convert(&vm, vm.universe.smi_klass, method, &cfg);
        let ra = regalloc::regalloc(&ir_method);

        let mut asm = JasmAssembler::new();
        let (_blob, _pcs, _ve, _ic, safepoint_pcs, _osr_off) =
            emit::emit(&mut asm, &ir_method, &ra, 0, 0, 0, None, None);
        assert_eq!(
            safepoint_pcs.len(),
            2,
            "two generic sends -> two safepoints"
        );

        let (blob, pcdescs) = build_deopt_metadata(&ir_method, &ra, &safepoint_pcs);
        assert_eq!(
            pcdescs.len(),
            2,
            "two deopt sites recorded (both main CallSends)"
        );

        // Expected FrameSlot for a vreg at a position, straight from
        // regalloc's own assignment — proves `resolve_frame_loc` read the
        // real slot, not a coincidence (same technique as the resolver
        // golden in it_tier1.rs).
        let expect_slot = |vreg: VReg, pos: u32| -> ValueLoc {
            let iv = ra
                .intervals
                .iter()
                .find(|iv| iv.vreg == vreg && iv.start <= pos && iv.end > pos)
                .unwrap_or_else(|| panic!("{vreg:?} must be live across position {pos}"));
            match iv.assignment {
                Some(regalloc::Assignment::Spill(slot)) => {
                    ValueLoc::FrameSlot(-8 * (slot.0 as i32 + 1))
                }
                other => panic!(
                    "S12 spill-all: {vreg:?} must be SPILLED across a safepoint, got {other:?}"
                ),
            }
        };

        // The `self bar` safepoint is emitted first -> lower return-address
        // offset -> pcdescs[0] (pack sorts by code_off); its position is
        // `safepoint_positions[0]` in the same numbering.
        let p0 = ra.safepoint_positions[0];
        let site0 = decode_site(&blob, pcdescs[0].site_off);
        assert_eq!(site0.kind, SafepointKind::Call);
        assert!(!site0.reexecute, "a call-return site is never reexecute");
        assert_eq!(
            site0.stack,
            vec![expect_slot(VReg(0), p0)],
            "the box:with: receiver `self` is live on the operand stack across `self bar`"
        );

        let scope0 = decode_scope(&blob, site0.scope_off);
        assert_eq!(
            scope0.receiver,
            expect_slot(VReg(0), p0),
            "self live-across"
        );
        assert_eq!(
            scope0.slots,
            vec![expect_slot(VReg(1), p0)],
            "the arg `a` (VReg 1), used again at the box:with: send, is a frame slot"
        );
        // S13: a method with deopt sites interns its own compiled MethodOop
        // into the pool (`ir::convert`), and every scope record names that
        // real pool index — no longer the 3b `0` placeholder.
        let expected_ix = ir_method
            .method_pool_ix
            .expect("a method with deopt sites interned its method oop");
        assert_eq!(
            scope0.method_pool_ix, expected_ix,
            "scope names the real interned method-oop pool index"
        );
        assert_eq!(
            ir_method.pool[expected_ix as usize].value,
            method.oop().raw(),
            "that pool word holds this method's own oop"
        );
        assert_eq!(scope0.sender, None, "S13 emits depth-1 chains only");

        // The outer `self box:with:` safepoint: receiver + both args popped,
        // so nothing is left on the operand stack below it.
        let site1 = decode_site(&blob, pcdescs[1].site_off);
        assert_eq!(site1.kind, SafepointKind::Call);
        assert!(!site1.reexecute);
        assert!(
            site1.stack.is_empty(),
            "box:with: pops receiver + 2 args -> empty stack below"
        );
    }

    /// S13 step 10b: a method with a backward-jump loop produces an nmethod
    /// whose deopt metadata carries a `LoopPoll` site (`reexecute == true`) at
    /// the poll's return offset, and its recorded scope resolves the
    /// loop-carried operand-stack vregs to real FrameSlot `ValueLoc`s (via
    /// spill-all plus `deopt_live` widening), never `Nil`. Driven through the
    /// decode/convert/regalloc/emit pipeline directly, exactly like
    /// `deopt_scope_blob_records_real_valuelocs`, on a call-free loop shape.
    ///
    /// `countTo: n [ | i | i := 0. [i < n] whileTrue: [i := i + 1]. ^i ]`,
    /// hand-built as a header-test loop: a `LOOP:` block tests `i < n`
    /// (`br_false_fwd END`), the body increments `i`, and a `jump_back LOOP`
    /// closes it — that back-edge is the `Ir::Poll` this test targets.
    #[test]
    fn loop_poll_records_loop_poll_deopt_scope() {
        use crate::compiler::scopes::{decode_scope, decode_site, SafepointKind, ValueLoc};

        let mut vm = test_vm();
        let lt_sel = vm.universe.intern(b"<");
        let plus_sel = vm.universe.intern(b"+");
        let m_sel = vm.universe.intern(b"countTo:");

        // temps: t0 = n (the arg), t1 = i (the counter).
        let mut b = BytecodeBuilder::new();
        b.push_smi_i8(0);
        b.store_temp_pop(1); // i := 0
        let loop_hdr = b.new_label();
        b.bind(loop_hdr);
        b.push_temp(1); // i
        b.push_temp(0); // n
        b.send(&mut vm, lt_sel, 1); // i < n
        let end = b.new_label();
        b.br_false_fwd(end); // exit when !(i < n)
        b.push_temp(1); // i
        b.push_smi_i8(1);
        b.send(&mut vm, plus_sel, 1); // i + 1
        b.store_temp_pop(1); // i := i + 1
        b.jump_back(loop_hdr); // <- BACKWARD JUMP -> Ir::Poll
        b.bind(end);
        b.push_temp(1); // ^i
        b.ret_tos();
        let method = b.finish(&mut vm, m_sel, 1, 1);

        // S14 step 3: warm both in-loop send sites to Mono on a NON-smi klass so
        // they stay generic `CallSend`s inside the loop (this test targets the
        // back-edge `Poll` and its loop-carried scope, which needs the loop body
        // intact). An Empty IC would now lower each send to an uncommon TRAP,
        // dismantling the loop before the poll is even reached.
        let obj_klass = vm.universe.object_klass;
        let lt_target = {
            let mut tb = BytecodeBuilder::new();
            tb.ret_self();
            tb.finish(&mut vm, lt_sel, 1, 0)
        };
        let plus_target = {
            let mut tb = BytecodeBuilder::new();
            tb.ret_self();
            tb.finish(&mut vm, plus_sel, 1, 0)
        };
        let epoch = vm.ic_epoch;
        InterpreterIc::at(method, 0).set_mono(&mut vm, obj_klass, lt_target, epoch);
        InterpreterIc::at(method, 1).set_mono(&mut vm, obj_klass, plus_target, epoch);

        let cfg = decode::decode(method);
        let ir_method = ir::convert(&vm, vm.universe.smi_klass, method, &cfg);
        let ra = regalloc::regalloc(&ir_method);

        let mut asm = JasmAssembler::new();
        let (_blob, _pcs, _ve, _ic, safepoint_pcs, _osr_off) =
            emit::emit(&mut asm, &ir_method, &ra, 0, 0, 0, None, None);

        let (blob, pcdescs) = build_deopt_metadata(&ir_method, &ra, &safepoint_pcs);

        // Exactly one LoopPoll site among the decoded deopt sites.
        let poll_sites: Vec<_> = pcdescs
            .iter()
            .map(|d| decode_site(&blob, d.site_off))
            .filter(|s| matches!(s.kind, SafepointKind::LoopPoll))
            .collect();
        assert_eq!(
            poll_sites.len(),
            1,
            "the single loop back-edge produces exactly one LoopPoll deopt site"
        );
        let poll = &poll_sites[0];
        assert!(
            poll.reexecute,
            "a loop-poll deopt re-executes the loop condition at the header bci"
        );
        // The loop-header bci is the resume point (re-execute the condition).
        let loop_hdr_bci = cfg
            .blocks
            .iter()
            .find(|blk| blk.is_loop_header)
            .expect("the loop has a header block")
            .bci_start;
        assert_eq!(
            poll.bci as usize, loop_hdr_bci,
            "the LoopPoll resumes at the loop-header bci (re-executes the condition)"
        );

        // Every recorded stack ValueLoc (the loop-carried operand stack, empty
        // here — the back-edge block leaves nothing on the stack — but the
        // SCOPE's own slots must resolve) must be a concrete FrameSlot, never
        // Nil: spill-all + deopt_live pin the live loop-carried vregs.
        let scope = decode_scope(&blob, poll.scope_off);
        for (i, loc) in scope.slots.iter().enumerate() {
            assert!(
                matches!(loc, ValueLoc::FrameSlot(_)),
                "loop-carried slot {i} must resolve to a FrameSlot across the poll, got {loc:?}"
            );
        }
        assert!(
            matches!(scope.receiver, ValueLoc::FrameSlot(_)),
            "the receiver must resolve to a FrameSlot across the poll, got {:?}",
            scope.receiver
        );
        for loc in &poll.stack {
            assert!(
                matches!(loc, ValueLoc::FrameSlot(_)),
                "every loop-carried operand-stack value must be a FrameSlot, got {loc:?}"
            );
        }
    }
}
