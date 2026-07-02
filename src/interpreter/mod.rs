//! The tier-0 interpreter (SPEC §5): activation/return discipline and the
//! dispatch loop. No `unsafe`; nothing outside `stack.rs` manipulates
//! `sp`/`fp` directly. S3 adds real sends (`send.rs`, `ic.rs`); closures and
//! contexts remain S4 — their arms are `unimplemented!` (reaching them
//! before S4 is a VM bug, since only `BytecodeBuilder` produces bytecode
//! and nothing before S4 emits those opcodes).

pub mod ic;
pub mod send;
pub mod stack;

use crate::bytecode::opcode::*;
use crate::oops::layout::ENTRY_FRAME_SENTINEL;
use crate::oops::smi::SmallInt;
use crate::oops::wrappers::MethodOop;
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;

use stack::Frame;

#[inline]
fn push(vm: &mut VmState, v: Oop) {
    vm.stack.push(v);
}

#[inline]
fn pop(vm: &mut VmState) -> Oop {
    vm.stack.pop()
}

fn current_frame(vm: &VmState) -> Frame {
    Frame { fp: vm.stack.fp }
}

fn current_method(vm: &VmState) -> MethodOop {
    current_frame(vm).method(&vm.stack)
}

fn frame_receiver(vm: &VmState) -> Oop {
    current_frame(vm).receiver(&vm.stack)
}

/// The bci the caller should resume at, after a non-entry
/// `return_from_frame`. See the `resume_bci` field doc on `VmState`.
fn resume_bci(vm: &VmState) -> usize {
    vm.resume_bci
}

/// Pushes a new frame for `method`. Receiver + `argc` args must already be
/// on the stack (SPEC §5.1's activation algorithm, step-numbered there).
/// `saved_fp`/`saved_bci` are the caller's resume link; S3's sends will
/// pass the current frame's fp/bci, `activate_entry` passes the sentinel.
pub fn push_frame(
    vm: &mut VmState,
    method: MethodOop,
    argc: usize,
    saved_fp: i64,
    saved_bci: usize,
) {
    let fp_new = vm.stack.sp;
    push(vm, method.oop());
    push(vm, SmallInt::new(saved_fp).oop());
    push(vm, SmallInt::new(saved_bci as i64).oop());
    push(vm, vm.universe.nil_obj); // context: nil in S2, Contexts are S4
    let receiver = vm.stack.get(fp_new - argc - 1);
    push(vm, receiver);
    let ntemps = method.ntemps();
    let nil = vm.universe.nil_obj;
    for _ in 0..ntemps {
        push(vm, nil);
    }
    vm.stack.activate_frame(fp_new);
}

/// The entry-frame variant `run_method` uses: no caller exists yet, so the
/// saved link is the sentinel.
pub fn activate_entry(vm: &mut VmState, method: MethodOop, argc: usize) {
    push_frame(vm, method, argc, ENTRY_FRAME_SENTINEL, 0);
}

/// Pops the current frame, writing `result` into the canonical receiver
/// slot (`fp - argc - 1`) and truncating `sp` to just past it (SPEC §5.1).
/// `Some(result)` when the entry frame returns (the interpreter loop should
/// stop and hand `result` back to `run_method`'s caller); `None` when a
/// caller frame was restored (the loop should reload `method`/`bci` — via
/// `current_method`/`resume_bci` — and continue).
pub fn return_from_frame(vm: &mut VmState, result: Oop) -> Option<Oop> {
    let frame = current_frame(vm);
    let argc = frame.method(&vm.stack).argc();
    let base = vm.stack.fp - argc - 1;
    let saved_fp = frame.saved_fp(&vm.stack);
    let saved_bci = frame.saved_bci(&vm.stack);

    vm.stack.set(base, result);
    vm.stack.sp = base + 1;

    if saved_fp == ENTRY_FRAME_SENTINEL {
        vm.stack.sp = base;
        vm.stack.deactivate();
        return Some(result);
    }
    vm.stack.activate_frame(saved_fp as usize);
    vm.resume_bci = saved_bci;
    None
}

/// Reads and executes bytecode from a fresh entry frame until it returns.
pub fn run_method(vm: &mut VmState, method: MethodOop, receiver: Oop, args: &[Oop]) -> Oop {
    push(vm, receiver);
    for &a in args {
        push(vm, a);
    }
    activate_entry(vm, method, args.len());
    dispatch(vm)
}

/// `S2: panic; S3: send` (SPEC §4.2 requires a real `#mustBeBoolean` send,
/// impossible before lookup exists — sprint_s02_detail.md's pinned
/// resolution). Diverges, so it type-checks as any branch's result.
fn must_be_boolean(vm: &VmState, t: Oop) -> ! {
    let s = crate::memory::print_oop(&vm.universe, t);
    panic!("mustBeBoolean: non-boolean receiver {s} in a branch (S3 replaces this with a send)");
}

fn dispatch(vm: &mut VmState) -> Oop {
    let mut method: MethodOop = current_method(vm);
    let mut bci: usize = 0;
    loop {
        debug_assert!(
            bci < method.bytecode_len(),
            "fell off method end at bci {bci}"
        );
        let op = method.bytecode_byte(bci);
        if vm.options.trace.is_enabled("bytecode") {
            trace_bc(vm, method, bci, op);
        }
        match op {
            OP_PUSH_SELF => {
                let r = frame_receiver(vm);
                push(vm, r);
                bci += 1;
            }
            OP_PUSH_NIL => {
                push(vm, vm.universe.nil_obj);
                bci += 1;
            }
            OP_PUSH_TRUE => {
                push(vm, vm.universe.true_obj);
                bci += 1;
            }
            OP_PUSH_FALSE => {
                push(vm, vm.universe.false_obj);
                bci += 1;
            }
            OP_PUSH_SMI_I8 => {
                let v = method.bytecode_byte(bci + 1) as i8;
                push(vm, SmallInt::new(v as i64).oop());
                bci += 2;
            }
            OP_PUSH_LITERAL => {
                let idx = method.bytecode_byte(bci + 1) as usize;
                let lit = method.literals().at(idx);
                push(vm, lit);
                bci += 2;
            }
            OP_PUSH_LITERAL_W => {
                let idx = read_u16(method, bci + 1) as usize;
                let lit = method.literals().at(idx);
                push(vm, lit);
                bci += 3;
            }
            OP_PUSH_TEMP => {
                let t = method.bytecode_byte(bci + 1) as usize;
                let v = current_frame(vm).temp(&vm.stack, t);
                push(vm, v);
                bci += 2;
            }
            OP_STORE_TEMP => {
                let t = method.bytecode_byte(bci + 1) as usize;
                let v = vm.stack.top();
                current_frame(vm).set_temp(&mut vm.stack, t, v);
                bci += 2;
            }
            OP_STORE_TEMP_POP => {
                let t = method.bytecode_byte(bci + 1) as usize;
                let v = pop(vm);
                current_frame(vm).set_temp(&mut vm.stack, t, v);
                bci += 2;
            }
            OP_PUSH_INSTVAR => {
                let i = method.bytecode_byte(bci + 1) as usize;
                let recv = frame_receiver(vm);
                let m = crate::oops::wrappers::MemOop::try_from(recv)
                    .expect("push_instvar: receiver is not a mem oop");
                debug_assert!(
                    i < m.klass().non_indexable_size() - crate::oops::layout::HEADER_WORDS,
                    "push_instvar: index {i} out of the named part"
                );
                push(vm, m.body_oop(i));
                bci += 2;
            }
            OP_STORE_INSTVAR_POP => {
                let i = method.bytecode_byte(bci + 1) as usize;
                let recv = frame_receiver(vm);
                let m = crate::oops::wrappers::MemOop::try_from(recv)
                    .expect("store_instvar_pop: receiver is not a mem oop");
                debug_assert!(
                    i < m.klass().non_indexable_size() - crate::oops::layout::HEADER_WORDS,
                    "store_instvar_pop: index {i} out of the named part"
                );
                let v = pop(vm);
                m.set_body_oop(i, v);
                bci += 2;
            }
            OP_PUSH_GLOBAL => {
                let idx = method.bytecode_byte(bci + 1) as usize;
                let assoc_oop = method.literals().at(idx);
                let assoc = crate::oops::wrappers::MemOop::try_from(assoc_oop)
                    .expect("push_global: literal is not an Association");
                push(vm, assoc.body_oop(1));
                bci += 2;
            }
            OP_STORE_GLOBAL_POP => {
                let idx = method.bytecode_byte(bci + 1) as usize;
                let assoc_oop = method.literals().at(idx);
                let assoc = crate::oops::wrappers::MemOop::try_from(assoc_oop)
                    .expect("store_global_pop: literal is not an Association");
                let v = pop(vm);
                assoc.set_body_oop(1, v);
                bci += 2;
            }
            OP_POP => {
                let _ = pop(vm);
                bci += 1;
            }
            OP_DUP => {
                let v = vm.stack.top();
                push(vm, v);
                bci += 1;
            }
            OP_JUMP_FWD => {
                let d = read_u16(method, bci + 1);
                let next = bci + 3;
                bci = next + d as usize;
            }
            OP_JUMP_BACK => {
                let d = read_u16(method, bci + 1);
                let next = bci + 3;
                if vm.pending {
                    poll(vm);
                }
                bci = next - d as usize;
            }
            OP_BR_TRUE_FWD => {
                let d = read_u16(method, bci + 1);
                let next = bci + 3;
                let t = pop(vm);
                bci = if t.raw() == vm.universe.true_obj.raw() {
                    next + d as usize
                } else if t.raw() == vm.universe.false_obj.raw() {
                    next
                } else {
                    must_be_boolean(vm, t)
                };
            }
            OP_BR_FALSE_FWD => {
                let d = read_u16(method, bci + 1);
                let next = bci + 3;
                let t = pop(vm);
                bci = if t.raw() == vm.universe.false_obj.raw() {
                    next + d as usize
                } else if t.raw() == vm.universe.true_obj.raw() {
                    next
                } else {
                    must_be_boolean(vm, t)
                };
            }
            OP_RETURN_TOS => {
                let r = pop(vm);
                match return_from_frame(vm, r) {
                    Some(result) => return result,
                    None => {
                        method = current_method(vm);
                        bci = resume_bci(vm);
                    }
                }
            }
            OP_RETURN_SELF => {
                let r = frame_receiver(vm);
                match return_from_frame(vm, r) {
                    Some(result) => return result,
                    None => {
                        method = current_method(vm);
                        bci = resume_bci(vm);
                    }
                }
            }
            OP_SEND | OP_SEND_SUPER | OP_SEND_W | OP_SEND_SUPER_W => {
                let is_super = op == OP_SEND_SUPER || op == OP_SEND_SUPER_W;
                let (ic_idx, next) = if op == OP_SEND_W || op == OP_SEND_SUPER_W {
                    (read_u16(method, bci + 1), bci + 3)
                } else {
                    (method.bytecode_byte(bci + 1) as u16, bci + 2)
                };
                let argc = ic::InterpreterIc::at(method, ic_idx).argc();

                let fp_before = vm.stack.fp;
                vm.regs.method = Some(method);
                vm.regs.bci = next;
                send::send_generic(vm, argc, ic_idx, is_super);

                if vm.exit_requested {
                    // `quit` (SPEC §10 system group): stop dispatch right
                    // here rather than unwinding every frame normally.
                    return if vm.stack.sp > 0 {
                        vm.stack.top()
                    } else {
                        vm.universe.nil_obj
                    };
                }
                if vm.stack.fp != fp_before {
                    // A real frame was pushed for the callee's bytecode
                    // body (no primitive short-circuit) — resume there.
                    method = current_method(vm);
                    bci = 0;
                } else {
                    // Primitive fast path: no new frame, resume right after
                    // the send in the SAME method.
                    bci = next;
                }
            }
            OP_PUSH_CTX_TEMP
            | OP_STORE_CTX_TEMP_POP
            | OP_PUSH_CLOSURE
            | OP_BLOCK_RETURN_TOS
            | OP_NLR_TOS => {
                unimplemented!("closures/contexts land in S4")
            }
            bad => panic!("undefined opcode {bad:#04x} at bci {bci}"),
        }
    }
}

/// SPEC §5.5's poll point. A no-op hook in S2 (`vm.pending` is never set) so
/// the loop's shape never changes once S7 wires a real GC poll behind it.
fn poll(_vm: &mut VmState) {}

fn trace_bc(vm: &VmState, method: MethodOop, bci: usize, op: u8) {
    eprintln!(
        "[bc] {} @{bci}: {op:#04x}",
        crate::memory::print_oop(&vm.universe, method.selector())
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::BytecodeBuilder;
    use crate::runtime::vm_state::VmOptions;
    use stack::ProcessStack;

    fn test_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
        })
    }

    fn method_returning_self(vm: &mut VmState, argc: usize, ntemps: usize) -> MethodOop {
        let mut b = BytecodeBuilder::new();
        b.ret_self();
        let sel = vm.universe.intern(b"m");
        b.finish(vm, sel, argc, ntemps)
    }

    #[test]
    fn activate_then_return() {
        let mut vm = test_vm();
        let method = method_returning_self(&mut vm, 2, 0);
        let recv = SmallInt::new(1).oop();
        let a0 = SmallInt::new(2).oop();
        let a1 = SmallInt::new(3).oop();
        let pre_sp = vm.stack.sp;
        push(&mut vm, recv);
        push(&mut vm, a0);
        push(&mut vm, a1);
        push_frame(&mut vm, method, 2, ENTRY_FRAME_SENTINEL, 0);

        let result = SmallInt::new(9).oop();
        let ret = return_from_frame(&mut vm, result);
        assert_eq!(ret, Some(result));
        assert_eq!(vm.stack.sp, pre_sp, "no residue above the pre-push sp");
    }

    #[test]
    fn activate_inits_slots() {
        let mut vm = test_vm();
        let method = method_returning_self(&mut vm, 0, 2);
        let recv = SmallInt::new(7).oop();
        push(&mut vm, recv);
        push_frame(&mut vm, method, 0, ENTRY_FRAME_SENTINEL, 0);

        let fp = vm.stack.fp;
        assert_eq!(
            SmallInt::try_from(vm.stack.get(fp + crate::oops::layout::FRAME_SAVED_FP))
                .unwrap()
                .value(),
            ENTRY_FRAME_SENTINEL
        );
        assert_eq!(
            SmallInt::try_from(vm.stack.get(fp + crate::oops::layout::FRAME_SAVED_BCI))
                .unwrap()
                .value(),
            0
        );
        assert_eq!(
            vm.stack.get(fp + crate::oops::layout::FRAME_CONTEXT),
            vm.universe.nil_obj
        );
        assert_eq!(vm.stack.get(fp + crate::oops::layout::FRAME_RECEIVER), recv);
        assert_eq!(
            vm.stack.get(fp + crate::oops::layout::FRAME_TEMPS_BASE),
            vm.universe.nil_obj
        );
        assert_eq!(
            vm.stack.get(fp + crate::oops::layout::FRAME_TEMPS_BASE + 1),
            vm.universe.nil_obj
        );
    }

    #[test]
    fn receiver_copies_agree() {
        let mut vm = test_vm();
        let method = method_returning_self(&mut vm, 2, 0);
        let recv = SmallInt::new(42).oop();
        push(&mut vm, recv);
        push(&mut vm, SmallInt::new(1).oop());
        push(&mut vm, SmallInt::new(2).oop());
        push_frame(&mut vm, method, 2, ENTRY_FRAME_SENTINEL, 0);

        let fp = vm.stack.fp;
        let via_fast_copy = vm.stack.get(fp + crate::oops::layout::FRAME_RECEIVER);
        let via_caller_slot = current_frame(&vm).receiver_slot(&vm.stack);
        assert_eq!(via_fast_copy, vm.stack.get(via_caller_slot));
        assert_eq!(via_fast_copy, recv);
    }

    #[test]
    fn dispatch_no_alloc() {
        let mut vm = test_vm();
        // The largest S2 kernel that doesn't send/close over anything:
        // a boolean flip loop (mirrors k_bool_loop from tests_s02.md).
        let mut b = BytecodeBuilder::new();
        let loop_head = b.new_label();
        let exit = b.new_label();
        b.push_true();
        b.store_temp_pop(0);
        b.bind(loop_head);
        b.push_temp(0);
        b.br_false_fwd(exit);
        b.push_false();
        b.store_temp_pop(0);
        b.jump_back(loop_head);
        b.bind(exit);
        b.push_smi_i8(42);
        b.ret_tos();
        let sel = vm.universe.intern(b"loop");
        let method = b.finish(&mut vm, sel, 0, 1);

        let eden_top_before = vm.universe.eden.top;
        let recv = vm.universe.nil_obj;

        let result = run_method(&mut vm, method, recv, &[]);
        assert_eq!(result, SmallInt::new(42).oop());
        assert_eq!(
            vm.universe.eden.top, eden_top_before,
            "S2 loop must not allocate"
        );
    }

    #[test]
    fn k_stack_restored_after_result() {
        let mut vm = test_vm();
        let method = method_returning_self(&mut vm, 0, 0);
        let recv = SmallInt::new(1).oop();
        let r1 = run_method(&mut vm, method, recv, &[]);
        assert_eq!(vm.stack.sp, 0);
        let r2 = run_method(&mut vm, method, recv, &[]);
        assert_eq!(vm.stack.sp, 0);
        assert_eq!(r1, r2);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "undefined opcode")]
    fn undefined_opcode_panics() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        b.ret_self(); // valid terminal instruction so finish() accepts it
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 0, 0);
        // Patch bci 0 to an undefined opcode, bypassing the builder.
        method.set_bytecode_byte(0, 0x7F);
        let recv = vm.universe.nil_obj;

        let _ = run_method(&mut vm, method, recv, &[]);
    }

    #[test]
    #[should_panic(expected = "closures/contexts land in S4")]
    fn closure_opcode_unimplemented() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        b.ret_self();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 0, 0);
        method.set_bytecode_byte(0, crate::bytecode::opcode::OP_PUSH_CLOSURE);
        let recv = vm.universe.nil_obj;

        let _ = run_method(&mut vm, method, recv, &[]);
    }

    #[test]
    #[should_panic(expected = "mustBeBoolean")]
    fn non_boolean_branch_panics() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        let l = b.new_label();
        b.push_smi_i8(0);
        b.br_true_fwd(l);
        b.bind(l);
        b.ret_self();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 0, 0);
        let recv = vm.universe.nil_obj;

        let _ = run_method(&mut vm, method, recv, &[]);
    }

    // `process_stack_overflow_exits_cleanly` lives in tests/it_interp.rs:
    // `env!("CARGO_BIN_EXE_macvm")` is only available to integration test
    // binaries, not `src/`-internal unit tests.

    #[test]
    fn trace_mode_smoke() {
        let mut vm = VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: crate::runtime::vm_state::TraceFlags::parse("bytecode"),
        });
        let mut b = BytecodeBuilder::new();
        b.push_true();
        b.pop();
        b.ret_self();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 0, 0);
        // 3 bytecodes executed: push_true, pop, return_self.
        let recv = vm.universe.nil_obj;

        let _ = run_method(&mut vm, method, recv, &[]);
        // No direct stderr capture here (that needs a subprocess); this
        // test asserts trace mode doesn't crash or alter results — the
        // line-count assertion lives in tests/it_interp.rs via subprocess.
    }

    #[test]
    fn frame_activation_flag_gates_pop_floor() {
        let mut st = ProcessStack::with_capacity(8);
        let a = SmallInt::new(1).oop();
        st.push(a);
        // With no frame active, pop's floor is 0 — this must not panic.
        assert_eq!(st.pop(), a);
    }
}
