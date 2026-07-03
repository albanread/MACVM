//! `eligible` (D1) + `compile_method` (D4) — the S10 compile driver: the
//! only place that decides whether a method compiles, and that drives the
//! decode -> convert -> regalloc -> emit -> publish -> install pipeline.
//! Neither function pushes a frame or touches the process stack — both are
//! callable directly, independent of the interpreter trigger (S10 step 8).

use crate::bytecode::opcode::{decode_at, Instr};
use crate::codecache::nmethod::{NmState, Nmethod, NmethodId, OopMap, PcDesc};
use crate::compiler::jasm_assembler::JasmAssembler;
use crate::compiler::{decode, emit, ir, regalloc};
use crate::interpreter::ic::{ic_state, IcState, InterpreterIc};
use crate::oops::wrappers::{KlassOop, MethodOop, SymbolOop};
use crate::runtime::vm_state::VmState;
use crate::runtime::JitMode;

/// D1 point 2: `primitives.rs`'s own pinned ids for
/// `{ +, -, *, bitAnd:, bitOr:, bitXor:, <, <=, >, >=, =, ~= }` — division
/// (`//`, `\\`, `bitShift:`: ids 4, 5, 9) excluded in v1.
const SMI_INLINE: [i64; 12] = [1, 2, 3, 6, 7, 8, 10, 11, 12, 13, 14, 15];

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

fn eligibility_detail(vm: &VmState, method: MethodOop) -> Eligibility {
    if method.is_block()
        || method.has_ctx()
        || method.argc() > 5
        || method.primitive() != 0
        || method.bytecode_len() > MAX_BYTECODE_LEN
    {
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
                if super_ {
                    return Eligibility::NoPermanent; // D1 point 1: super sends excluded
                }
                verdict = verdict.worse(mono_smi_inline_send(vm, method, ic));
                if verdict == Eligibility::NoPermanent {
                    return Eligibility::NoPermanent; // short-circuit: nothing later can undo this
                }
            }
            // Excluded (D1 point 1): instvar/global/ctx stores, closures,
            // ctx temps, super sends (handled above), block returns, NLR.
            Instr::StoreInstvarPop(_)
            | Instr::StoreGlobalPop(_)
            | Instr::PushCtxTemp { .. }
            | Instr::StoreCtxTempPop { .. }
            | Instr::PushClosure { .. }
            | Instr::BlockReturnTos
            | Instr::NlrTos => return Eligibility::NoPermanent,
        }
        bci = next;
    }
    if verdict != Eligibility::Yes {
        return verdict; // NoRetryLater: some site was Empty, nothing else disqualified
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
        return Eligibility::NoPermanent;
    }

    Eligibility::Yes
}

/// D1 point 2: `ic_idx`'s site is monomorphic, guarded on `smi_klass`, and
/// its cached target's primitive is in [`SMI_INLINE`]. `Empty` is the one
/// `NoRetryLater` case — see [`Eligibility`]'s own doc; every other
/// non-eligible shape (poly, mega, mono-but-non-smi, mono-smi-but-not-an-
/// inlinable-primitive) is treated as permanent: none of those states
/// un-happen on their own the way a cold, not-yet-reached site does.
fn mono_smi_inline_send(vm: &VmState, method: MethodOop, ic_idx: u16) -> Eligibility {
    match ic_state(method, ic_idx) {
        IcState::Empty => return Eligibility::NoRetryLater,
        IcState::Mono => {}
        IcState::Poly(_) | IcState::Mega => return Eligibility::NoPermanent,
    }
    let site = InterpreterIc::at(method, ic_idx);
    if site.guard().raw() != vm.universe.smi_klass.oop().raw() {
        return Eligibility::NoPermanent; // mono but non-smi guard
    }
    let Some(target) = MethodOop::try_from(site.target()) else {
        return Eligibility::NoPermanent;
    };
    if SMI_INLINE.contains(&target.primitive()) {
        Eligibility::Yes
    } else {
        Eligibility::NoPermanent
    }
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

    let cfg = decode::decode(method);
    let ir_method = ir::convert(vm, method, &cfg);
    let regalloc_result = regalloc::regalloc(&ir_method);

    let mut asm = JasmAssembler::new();
    let stub_poll_addr = vm.stubs.stub_poll_addr();
    let (blob, block_pcs) = emit::emit(&mut asm, &ir_method, &regalloc_result, stub_poll_addr);

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

    // Block-start-only descs (S10 emits no real safepoints, D1) all point
    // at the one oopmap this nmethod's own spill slots produce (D7) — never
    // actually consulted until S11 gives some `PcDesc` a genuine call site.
    let pcdescs: Vec<PcDesc> = block_pcs
        .iter()
        .map(|bp| PcDesc {
            pc_off: bp.pc_off,
            bci: bp.bci,
            oopmap: 0,
        })
        .collect();
    let oopmaps = vec![OopMap {
        bits: regalloc_result.slot_is_oop.clone(),
    }];
    let key_selector = SymbolOop::try_from(method.selector())
        .expect("compile_method: method selector is not a Symbol");

    let nm = Nmethod {
        id: NmethodId(0), // overwritten by CodeTable::install
        key_klass: rcvr_klass,
        key_selector,
        code: h,
        entry_off: 0,
        verified_entry_off: 0,
        state: NmState::Alive,
        level: 1,
        version: 0,
        literal_off: blob.literal_off,
        relocs: blob.relocs,
        frame_slots: regalloc_result.frame_slots,
        pcdescs,
        oopmaps,
        ic_sites: Vec::new(),
    };
    let id = vm.code_table.install(nm);
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

    #[test]
    fn eligible_rejects_empty_ic() {
        let mut vm = test_vm();
        let method = plus_method(&mut vm);
        // No `set_mono` call: the IC is left at its fresh, empty state.
        assert!(!eligible(&vm, method));
    }

    #[test]
    fn eligible_rejects_non_smi_guard() {
        let mut vm = test_vm();
        let method = plus_method(&mut vm);
        let plus_sel = vm.universe.intern(b"+");
        let plus_target = primitive_stub(&mut vm, plus_sel, 1);
        // Mono, but guarded on Object's klass instead of SmallInteger's.
        let object_klass = vm.universe.object_klass;
        let epoch = vm.ic_epoch;
        InterpreterIc::at(method, 0).set_mono(&mut vm, object_klass, plus_target, epoch);
        assert!(!eligible(&vm, method));
    }

    #[test]
    fn eligible_rejects_non_inline_primitive() {
        let mut vm = test_vm();
        let method = plus_method(&mut vm);
        // Mono, smi-guarded, but the target's primitive (23 = basicNew)
        // isn't in SMI_INLINE.
        let plus_sel = vm.universe.intern(b"+");
        let not_inline = primitive_stub(&mut vm, plus_sel, 23);
        let smi_klass = vm.universe.smi_klass;
        let epoch = vm.ic_epoch;
        InterpreterIc::at(method, 0).set_mono(&mut vm, smi_klass, not_inline, epoch);
        assert!(!eligible(&vm, method));
    }

    #[test]
    fn eligible_rejects_super_send() {
        let mut vm = test_vm();
        let plus_sel = vm.universe.intern(b"+");
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.push_temp(0);
        b.send_super(&mut vm, plus_sel, 1);
        b.ret_tos();
        let m_sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, m_sel, 1, 0);
        set_mono_smi_plus_ic(&mut vm, method);
        assert!(!eligible(&vm, method));
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

    /// D1's `NoRetryLater` path (the fix in this same commit): a method
    /// whose only problem is a still-cold inner send site must NOT be
    /// permanently disabled — the counter is left exactly as the caller
    /// set it, so a later call (after this method's own body has actually
    /// run and warmed that site) gets a fresh attempt. See `Eligibility`'s
    /// own doc for why this distinction exists at all.
    #[test]
    fn compile_method_leaves_cold_ic_method_retryable() {
        let mut vm = test_vm();
        let method = plus_method(&mut vm);
        // Left with an empty inner IC -> not yet eligible, but not
        // permanently so either.
        assert!(!method.compile_disabled());
        method.set_counters(41); // as if bump_invocation had already run once
        let smi_klass = vm.universe.smi_klass;
        let id = compile_method(&mut vm, smi_klass, method);
        assert!(id.is_none());
        assert!(
            !method.compile_disabled(),
            "a cold inner IC must not permanently disable the method"
        );
        assert_eq!(
            method.counters() & crate::oops::layout::COUNTERS_INVOCATION_MASK,
            41,
            "the counter must be left untouched, not reset, so a later call still crosses \
             threshold and retries"
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
}
