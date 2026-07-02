//! `send`/`send_super` end-to-end (SPEC §5.3): the IC fast path, the
//! transition-table slow path (`interpreter::ic`), `activate_method`
//! (invocation counter + primitive step + frame push), and the
//! `doesNotUnderstand:` path. The only module that mutates `InterpRegs` or
//! pushes frames on a send (`sprint_s03_detail.md` §Layer boundaries).

use crate::oops::layout::{COUNTERS_INVOCATION_MASK, COUNTERS_INVOCATION_MAX};
use crate::oops::wrappers::{KlassOop, MethodOop, SymbolOop};
use crate::runtime::primitives::PrimResult;
use crate::runtime::vm_state::VmState;

use super::ic::{ic_transition, InterpreterIc};
use super::{pop, push};

/// `klass_of` (SPEC §5.3 step 3), re-exported here for the dispatch loop's
/// convenience; the canonical implementation lives in `runtime::lookup`
/// (also used by `runtime::primitives`).
pub use crate::runtime::lookup::klass_of;

/// `pub(crate)`: also bumped by `interpreter::blocks::activate_block` —
/// blocks have counters too (SPEC §5.4, S14 feedback).
pub(crate) fn bump_invocation(m: MethodOop) {
    let c = m.counters();
    let inv = c & COUNTERS_INVOCATION_MASK;
    let bumped = (inv + 1).min(COUNTERS_INVOCATION_MAX);
    m.set_counters((c & !COUNTERS_INVOCATION_MASK) | bumped);
}

/// SPEC §5.3 step 5. Bumps the invocation counter (every arrival, including
/// primitive short-circuits), tries the primitive if one is attached, and
/// otherwise pushes a real frame for the bytecode body. `vm.regs` is
/// updated on the frame-push path only — a primitive success leaves it
/// exactly as the dispatch loop set it before the send (the resume point in
/// the *same*, unpushed, calling method).
pub fn activate_method(vm: &mut VmState, m: MethodOop, argc: u8) {
    bump_invocation(m);

    let prim_id = m.primitive();
    if prim_id != 0 {
        let desc = crate::runtime::primitives::prim_by_id(prim_id as u16)
            .unwrap_or_else(|| panic!("activate_method: unknown primitive id {prim_id}"));
        let argc_usize = argc as usize;
        let sp = vm.stack.sp;
        let base = sp - argc_usize - 1;
        let mut buf = [vm.universe.nil_obj; 6];
        for (i, slot) in buf.iter_mut().enumerate().take(argc_usize + 1) {
            *slot = vm.stack.get(base + i);
        }
        vm.prim_arg_base = base;
        match (desc.f)(vm, &buf[..=argc_usize]) {
            PrimResult::Ok(v) => {
                vm.stack.sp = base;
                push(vm, v);
                return;
            }
            PrimResult::Fail => {
                debug_assert!(
                    m.prim_fails(),
                    "activate_method: primitive {} Failed but the method's prim_fails flag is unset",
                    desc.name
                );
                // Falls through: the method's bytecode body is the fallback.
            }
            PrimResult::Activated => {
                // The primitive already pushed a real frame and set
                // `vm.regs` itself (the `value` family, `ensure:`,
                // `ifCurtailed:`) — push NO result, just resume dispatch.
                return;
            }
        }
    }

    let saved_fp = vm.stack.fp as i64;
    let saved_bci = vm.regs.bci;
    super::push_frame(vm, m, argc as usize, saved_fp, saved_bci);
    // A plain method's own Context has no enclosing chain (SPEC §5.4);
    // block activation (`activate_block`) passes its inherited context
    // instead.
    let nil = vm.universe.nil_obj;
    super::blocks::maybe_alloc_context(vm, m, vm.stack.fp, nil);
    vm.regs.method = Some(m);
    vm.regs.bci = 0;
}

/// Steps 1–4 of SPEC §5.3's `send` algorithm plus the transition-table
/// dispatch (step 5–7). `argc` must equal the site's own IC-recorded argc
/// (the dispatch loop reads both from the same place, so this is always
/// true — the assert documents the invariant rather than guarding against a
/// real divergence).
pub fn send_generic(vm: &mut VmState, argc: u8, ic_idx: u16, is_super: bool) {
    let caller = vm.regs.method.expect("send_generic: no active method");
    let ic = InterpreterIc::at(caller, ic_idx);
    debug_assert_eq!(argc, ic.argc(), "send_generic: argc/IC mismatch");

    let rcvr = vm.stack.get(vm.stack.sp - argc as usize - 1);
    let k = klass_of(vm, rcvr);

    // Fast path: mono guard, current epoch — read-only, no IC write.
    if ic.guard().raw() == k.oop().raw() && ic.epoch() == vm.ic_epoch {
        let m = MethodOop::try_from(ic.target())
            .expect("send_generic: mono target is not a CompiledMethod");
        activate_method(vm, m, argc);
        return;
    }

    match ic_transition(vm, caller, ic_idx, k, is_super) {
        Some(m) => activate_method(vm, m, argc),
        None => dnu(vm, k, ic.selector(), argc),
    }
}

pub fn send_super_generic(vm: &mut VmState, argc: u8, ic_idx: u16) {
    send_generic(vm, argc, ic_idx, true);
}

/// SPEC §5.3 step 6: `#doesNotUnderstand:` resolution. Builds the `Message`
/// (array parked on the operand stack across the allocation — the S7
/// choke-point pattern), rewrites the send site's args down to the single
/// `Message` argument, and re-sends through a full lookup (never through
/// the original site's IC — that IC's state describes the *original*
/// selector, not `#doesNotUnderstand:`).
fn dnu(vm: &mut VmState, rcvr_klass: KlassOop, selector: SymbolOop, argc: u8) {
    let argc_usize = argc as usize;
    let sp = vm.stack.sp;
    let args_start = sp - argc_usize;

    let array_klass = vm.universe.array_klass;
    let args_array = crate::memory::alloc::alloc_indexable_oops(vm, array_klass, argc_usize);
    for i in 0..argc_usize {
        args_array.at_put(i, vm.stack.get(args_start + i));
    }

    // Drop the raw args (receiver stays), park the array as a GC root
    // before allocating the Message.
    vm.stack.sp = args_start;
    push(vm, args_array.oop());

    let message_klass = vm.universe.message_klass;
    let msg = crate::memory::alloc::alloc_slots(vm, message_klass);
    let parked_array = pop(vm);
    msg.set_body_oop(0, selector.oop());
    msg.set_body_oop(1, parked_array);
    push(vm, msg.oop()); // stack: ..., receiver, message

    let sel_dnu = vm.universe.sel_does_not_understand;
    match crate::runtime::lookup::lookup(vm, rcvr_klass, sel_dnu) {
        Some(dnu_method) => activate_method(vm, dnu_method, 1),
        None => crate::runtime::error::dnu_fallback(vm, selector, rcvr_klass),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::BytecodeBuilder;
    use crate::oops::layout::{COUNTERS_INVOCATION_MASK, COUNTERS_INVOCATION_MAX, HEADER_WORDS};
    use crate::oops::smi::SmallInt;
    use crate::oops::Oop;
    use crate::runtime::lookup::install_method;
    use crate::runtime::vm_state::{VmOptions, VmState};

    fn test_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            eden_kb: None,
        })
    }

    fn new_klass(vm: &mut VmState, name: &str) -> KlassOop {
        let object_klass = vm.universe.object_klass;
        vm.universe.new_klass(
            object_klass,
            name,
            crate::oops::Format::Slots,
            false,
            HEADER_WORDS,
        )
    }

    /// A caller with one send site: `push_temp(0..=argc)`, `send(sel,
    /// argc)`, `ret_tos` — see `interpreter::ic`'s tests for the same shape.
    fn build_caller(vm: &mut VmState, sel: SymbolOop, argc: u8) -> MethodOop {
        let mut b = BytecodeBuilder::new();
        for t in 0..=argc {
            b.push_temp(t);
        }
        b.send(vm, sel, argc);
        b.ret_tos();
        let name = vm.universe.intern(b"caller");
        b.finish(vm, name, (argc + 1) as usize, 0)
    }

    fn send_once(vm: &mut VmState, caller: MethodOop, args: &[Oop]) -> Oop {
        let dummy_self = vm.universe.nil_obj;
        crate::interpreter::run_method(vm, caller, dummy_self, args)
    }

    #[test]
    fn activate_counter_bump() {
        let mut vm = test_vm();
        let sel = vm.universe.intern(b"foo");
        let a = new_klass(&mut vm, "A");
        let mut b = BytecodeBuilder::new();
        b.ret_self();
        let name = vm.universe.intern(b"foo");
        let m = b.finish(&mut vm, name, 0, 0);
        install_method(&mut vm, a, sel, m);
        let caller = build_caller(&mut vm, sel, 0);
        let recv = crate::memory::alloc::alloc_slots(&mut vm, a).oop();

        for i in 1..=5i64 {
            let _ = send_once(&mut vm, caller, &[recv]);
            assert_eq!(m.counters() & COUNTERS_INVOCATION_MASK, i);
        }
    }

    #[test]
    fn counter_saturates() {
        let mut vm = test_vm();
        let sel = vm.universe.intern(b"foo");
        let a = new_klass(&mut vm, "A");
        let mut b = BytecodeBuilder::new();
        b.ret_self();
        let name = vm.universe.intern(b"foo");
        let m = b.finish(&mut vm, name, 0, 0);
        m.set_counters(COUNTERS_INVOCATION_MAX);
        install_method(&mut vm, a, sel, m);
        let caller = build_caller(&mut vm, sel, 0);
        let recv = crate::memory::alloc::alloc_slots(&mut vm, a).oop();

        let _ = send_once(&mut vm, caller, &[recv]);
        assert_eq!(
            m.counters() & COUNTERS_INVOCATION_MASK,
            COUNTERS_INVOCATION_MAX
        );
    }

    #[test]
    fn counter_bumped_on_prim_success() {
        let mut vm = test_vm();
        let sel = vm.universe.intern(b"identityHash");
        let a = new_klass(&mut vm, "A");
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.ret_tos();
        let name = vm.universe.intern(b"identityHash");
        let m = b.finish(&mut vm, name, 0, 0);
        m.set_primitive(20); // oops-group `identityHash`, never Fails
        install_method(&mut vm, a, sel, m);
        let caller = build_caller(&mut vm, sel, 0);
        let recv = crate::memory::alloc::alloc_slots(&mut vm, a).oop();

        let _ = send_once(&mut vm, caller, &[recv]);
        assert_eq!(
            m.counters() & COUNTERS_INVOCATION_MASK,
            1,
            "counter must bump even on a primitive short-circuit"
        );
    }

    #[test]
    fn prim_fallback_runs_bytecode_body() {
        let mut vm = test_vm();
        let sel = vm.universe.intern(b"foo");
        let a = new_klass(&mut vm, "A");
        let mut b = BytecodeBuilder::new();
        b.push_smi_i8(7); // the fallback body's own return value
        b.ret_tos();
        let name = vm.universe.intern(b"foo");
        let m = b.finish(&mut vm, name, 1, 0); // argc=1: smi `+`'s shape
        m.set_primitive(1); // smi group `+`
        m.set_flags(1, 0, false, false, true, false, 0); // prim_fails = true
        install_method(&mut vm, a, sel, m);
        let caller = build_caller(&mut vm, sel, 1);
        // The receiver is not a smi, so primitive 1 (`+`) always Fails.
        let recv = crate::memory::alloc::alloc_slots(&mut vm, a).oop();
        let arg = SmallInt::new(1).oop();

        let result = send_once(&mut vm, caller, &[recv, arg]);
        assert_eq!(
            result,
            SmallInt::new(7).oop(),
            "a Failed primitive must fall through to the bytecode body"
        );
    }

    #[test]
    fn stack_receiver_index() {
        let mut vm = test_vm();
        let recv = SmallInt::new(1).oop();
        let arg0 = SmallInt::new(2).oop();
        let arg1 = SmallInt::new(3).oop();
        vm.stack.push(recv);
        vm.stack.push(arg0);
        vm.stack.push(arg1);

        // The exact arithmetic `send_generic` uses to locate the receiver
        // (SPEC §5.3's pinned `sp - argc - 1` convention).
        let argc = 2usize;
        let receiver_index = vm.stack.sp - argc - 1;
        assert_eq!(vm.stack.get(receiver_index), recv);
    }

    /// The primitive call site's 3rd arm (`PrimResult::Activated`, S4) must
    /// push NO result — an off-by-one there would corrupt the new (block)
    /// frame's first temp slot with a stray value instead of leaving it
    /// `nil`.
    #[test]
    fn activated_no_result_push() {
        let mut vm = test_vm();
        let closure_klass = vm.universe.closure_klass;
        let value_sel = vm.universe.intern(b"value");
        let mut value_body = BytecodeBuilder::new();
        value_body.push_self();
        value_body.ret_self(); // unreachable: the primitive never Fails here
        let value_method = value_body.finish(&mut vm, value_sel, 0, 0);
        value_method.set_primitive(50);
        install_method(&mut vm, closure_klass, value_sel, value_method);

        let mut home = BytecodeBuilder::new();
        // The block itself has 1 local temp, never explicitly stored to —
        // if `Activated` handling pushed a stray result, it would land
        // exactly here.
        let lit = home.build_block(&mut vm, 0, 1, false, 0, false, |b, _vm| {
            b.push_temp(0);
            b.ret_tos();
        });
        home.push_closure(lit, 0);
        home.send(&mut vm, value_sel, 0);
        home.ret_tos();
        let sel = vm.universe.intern(b"m");
        let method = home.finish(&mut vm, sel, 0, 0);

        let recv = vm.universe.nil_obj;
        let result = crate::interpreter::run_method(&mut vm, method, recv, &[]);
        assert_eq!(
            result, recv,
            "the block's untouched temp[0] must be nil, not a stray pushed result"
        );
    }
}
