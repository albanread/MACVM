//! The tier-0 interpreter (SPEC §5): activation/return discipline and the
//! dispatch loop. No `unsafe`; nothing outside `stack.rs` manipulates
//! `sp`/`fp` directly. S3 adds real sends (`send.rs`, `ic.rs`); closures and
//! contexts remain S4 — their arms are `unimplemented!` (reaching them
//! before S4 is a VM bug, since only `BytecodeBuilder` produces bytecode
//! and nothing before S4 emits those opcodes).

pub mod blocks;
pub mod compiled_call;
pub mod ic;
pub mod send;
pub mod stack;
pub mod unwind;

use crate::bytecode::opcode::*;
use crate::oops::layout::ENTRY_FRAME_SENTINEL;
use crate::oops::smi::SmallInt;
use crate::oops::wrappers::{ContextOop, MethodOop};
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

/// SPEC §5.4 step 4: walks `depth` `home_hint` hops from the current
/// frame's own Context — depth counts only `has_ctx` scopes, never lexical
/// block nesting (a ctx-less block's frame *aliases* its enclosing Context
/// directly, so no hop is spent on it). A hop that doesn't land on a real
/// Context is a compiler bug (wrong depth), never a guest error.
fn ctx_temp_walk(vm: &VmState, depth: u8) -> ContextOop {
    let mut c = current_frame(vm).context(&vm.stack);
    for _ in 0..depth {
        let ctx = ContextOop::try_from(c)
            .expect("ctx_temp_walk: hop landed on a non-Context (compiler bug: wrong depth)");
        c = ctx.home_hint();
    }
    ContextOop::try_from(c)
        .expect("ctx_temp_walk: final hop is not a Context (compiler bug: wrong depth)")
}

/// The method dispatch should resume in after a `None`-returning
/// `unwind::do_return`/`unwind::continue_unwind` call — always
/// `vm.regs.method`, the single source of truth every such path stamps
/// (see `unwind::pop_and_deliver`'s doc for why this can't be re-derived
/// from `vm.stack.fp` alone: a nested resume sentinel may have activated a
/// fresh handler frame rather than merely restoring a caller's).
fn regs_method(vm: &VmState) -> MethodOop {
    vm.regs
        .method
        .expect("regs_method: no active method after a return")
}

/// Pushes a new frame for `method`. Receiver + `argc` args must already be
/// on the stack (SPEC §5.1's activation algorithm, step-numbered there).
/// `saved_fp`/`saved_bci` are the caller's resume link; sends pass the
/// current frame's fp/bci, `activate_entry` passes the sentinel. `FRAME_
/// CONTEXT` is left `nil` here — a `has_ctx` method's real Context is
/// allocated by a separate step right after this returns (SPEC §5.4's
/// "frame fully formed before the allocation" rule), never inline in this
/// function. Stamps a fresh, never-reused frame serial (SPEC §5.4 dead-home
/// detection) and an empty marker.
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
    push(vm, vm.universe.nil_obj); // context: patched by the caller for has_ctx methods
    let receiver = vm.stack.get(fp_new - argc - 1);
    push(vm, receiver);
    let serial = vm.alloc_frame_serial();
    push(vm, SmallInt::new(serial as i64).oop());
    push(vm, vm.universe.nil_obj); // marker: none
    let ntemps = method.ntemps();
    let nil = vm.universe.nil_obj;
    for _ in 0..ntemps {
        push(vm, nil);
    }
    vm.stack.activate_frame(fp_new);
}

/// The entry-frame variant `run_method` uses: no caller exists yet, so the
/// saved link is the sentinel. An entry method's own Context (if any) has
/// no enclosing chain, same as any other plain method activation.
pub fn activate_entry(vm: &mut VmState, method: MethodOop, argc: usize) {
    push_frame(vm, method, argc, ENTRY_FRAME_SENTINEL, 0);
    let nil = vm.universe.nil_obj;
    blocks::maybe_alloc_context(vm, method, vm.stack.fp, nil);
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

/// S11 D6.1's C→I entry point: like [`run_method`], but safe to call from
/// WITHIN an already-active interpreter activation that's currently paused
/// (a compiled frame running above it — an I→C→I round trip). `run_method`
/// itself is only ever a genuine top-level entry elsewhere in this
/// codebase (`main.rs`, bootstrap code, or a fresh-`vm` test) — its own
/// completion unconditionally clears `vm.stack`'s "is a frame active"
/// bookkeeping (`ProcessStack::deactivate`, via
/// `unwind::pop_and_deliver`'s `ENTRY_FRAME_SENTINEL` case), which would
/// silently drop an OUTER activation's own `fp` if one exists. Saving and
/// restoring that snapshot around the call is what makes nesting safe
/// without touching `run_method`/`dispatch`/`do_return` themselves.
pub fn run_method_reentrant(
    vm: &mut VmState,
    method: MethodOop,
    receiver: Oop,
    args: &[Oop],
) -> Oop {
    let saved = vm.stack.save_activation();
    let result = run_method(vm, method, receiver, args);
    vm.stack.restore_activation(saved);
    result
}

/// SPEC §5.4 Algorithm 11: a branch popped a non-`true`/`false` value.
/// Pushes it back, rolls `vm.regs.bci` back to the *branch opcode itself*
/// (not its operand) so a normal-returning handler makes the branch
/// re-examine its (hopefully now boolean) result, and sends `#mustBeBoolean`
/// (unary) via a full lookup — no dedicated IC site (SPEC: "not worth a
/// site"). No handler installed → the S3 DNU fallback prints and
/// terminates (never returns). A handler that keeps returning non-booleans
/// livelocks by construction — same as real Smalltalk, not a VM bug.
fn must_be_boolean_send(vm: &mut VmState, method: MethodOop, branch_bci: usize, t: Oop) {
    push(vm, t);
    vm.regs.method = Some(method);
    vm.regs.bci = branch_bci;
    let klass = send::klass_of(vm, t);
    let sel = vm.universe.sel_must_be_boolean;
    match crate::runtime::lookup::lookup(vm, klass, sel) {
        Some(m) => send::activate_method(vm, m, 0, None),
        None => crate::runtime::error::dnu_fallback(vm, sel, klass), // never returns
    }
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
        if vm.options.trace.is_enabled("count") {
            vm.bytecode_count += 1;
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
                crate::memory::store::store(vm, m, i, v);
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
                crate::memory::store::store(vm, assoc, 1, v);
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
                if vm.reg_block.poll_flag != 0 {
                    poll(vm);
                }
                bci = next - d as usize;
            }
            OP_BR_TRUE_FWD => {
                let d = read_u16(method, bci + 1);
                let next = bci + 3;
                let t = pop(vm);
                if t.raw() == vm.universe.true_obj.raw() {
                    bci = next + d as usize;
                } else if t.raw() == vm.universe.false_obj.raw() {
                    bci = next;
                } else {
                    let fp_before = vm.stack.fp;
                    must_be_boolean_send(vm, method, bci, t);
                    if vm.stack.fp != fp_before {
                        method = current_method(vm);
                        bci = 0;
                    } else {
                        // A primitive-backed #mustBeBoolean returned without
                        // pushing a frame — re-examine the same branch.
                        // Re-read: the send may have allocated (S7).
                        method = current_method(vm);
                        bci = vm.regs.bci;
                    }
                }
            }
            OP_BR_FALSE_FWD => {
                let d = read_u16(method, bci + 1);
                let next = bci + 3;
                let t = pop(vm);
                if t.raw() == vm.universe.false_obj.raw() {
                    bci = next + d as usize;
                } else if t.raw() == vm.universe.true_obj.raw() {
                    bci = next;
                } else {
                    let fp_before = vm.stack.fp;
                    must_be_boolean_send(vm, method, bci, t);
                    if vm.stack.fp != fp_before {
                        method = current_method(vm);
                        bci = 0;
                    } else {
                        // Re-read: the send may have allocated (S7).
                        method = current_method(vm);
                        bci = vm.regs.bci;
                    }
                }
            }
            OP_RETURN_TOS => {
                let r = pop(vm);
                match unwind::do_return(vm, r) {
                    Some(result) => return result,
                    None => {
                        method = regs_method(vm);
                        bci = vm.regs.bci;
                    }
                }
            }
            OP_RETURN_SELF => {
                let r = frame_receiver(vm);
                match unwind::do_return(vm, r) {
                    Some(result) => return result,
                    None => {
                        method = regs_method(vm);
                        bci = vm.regs.bci;
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
                    // the send in the SAME method — but through a re-read:
                    // a `can_allocate` primitive may have scavenged, moving
                    // the method this local still points at (the frame's
                    // method slot is a scanned root; this local is not).
                    method = current_method(vm);
                    bci = next;
                }
            }
            OP_PUSH_CTX_TEMP => {
                let depth = method.bytecode_byte(bci + 1);
                let idx = method.bytecode_byte(bci + 2) as usize;
                let ctx = ctx_temp_walk(vm, depth);
                push(vm, ctx.slot(idx));
                bci += 3;
            }
            OP_STORE_CTX_TEMP_POP => {
                let depth = method.bytecode_byte(bci + 1);
                let idx = method.bytecode_byte(bci + 2) as usize;
                let ctx = ctx_temp_walk(vm, depth);
                let v = pop(vm);
                ctx.set_slot(idx, v);
                // A guest mutator store the S7-5 barrier sweep missed: a
                // long-lived closure's Context is routinely old while `v`
                // is young (S7-10).
                crate::memory::store::post_write_barrier(vm, ctx.as_mem());
                bci += 3;
            }
            OP_PUSH_CLOSURE => {
                let lit = method.bytecode_byte(bci + 1) as usize;
                let n_value_captures = method.bytecode_byte(bci + 2) as usize;
                let blk = crate::oops::wrappers::MethodOop::try_from(method.literals().at(lit))
                    .expect("push_closure: literal is not a CompiledBlock");
                let closure = blocks::make_closure(vm, blk, n_value_captures);
                push(vm, closure.oop());
                // make_closure allocates — re-read the (possibly moved)
                // current method before fetching the next opcode.
                method = current_method(vm);
                bci += 3;
            }
            OP_BLOCK_RETURN_TOS => {
                // A block's implicit fall-off-the-end return: identical
                // pop/deliver mechanics to `return_tos` — this is a LOCAL
                // return from just the block's own activation (delivered
                // to whoever sent it `value`/`value:`/…), not a non-local
                // return to the block's lexical home (that's `nlr_tos`).
                let r = pop(vm);
                match unwind::do_return(vm, r) {
                    Some(result) => return result,
                    None => {
                        method = regs_method(vm);
                        bci = vm.regs.bci;
                    }
                }
            }
            OP_NLR_TOS => {
                let value = pop(vm);
                debug_assert!(
                    method.is_block(),
                    "nlr_tos: only a block's own bytecode ever emits this (`^` in a method compiles to return_tos)"
                );
                vm.regs.method = Some(method);
                vm.regs.bci = bci;
                let frame = current_frame(vm);
                let closure_oop = vm.stack.get(frame.receiver_slot(&vm.stack));
                let closure = crate::oops::wrappers::ClosureOop::try_from(closure_oop)
                    .expect("nlr_tos: block frame's receiver slot is not a Closure");
                let home = crate::oops::home_ref::unpack_home_ref(closure.home());
                match unwind::continue_unwind(vm, home, value) {
                    unwind::UnwindStep::ReturnedFromHome(Some(result)) => return result,
                    unwind::UnwindStep::ReturnedFromHome(None)
                    | unwind::UnwindStep::RanHandler
                    | unwind::UnwindStep::CannotReturn => {
                        // In every case, whichever activation is now current
                        // (a restored caller, a fresh handler, or a fresh
                        // #cannotReturn: activation) already stamped
                        // `vm.regs` as the single source of truth — see
                        // `unwind::pop_and_deliver`'s doc.
                        method = regs_method(vm);
                        bci = vm.regs.bci;
                    }
                }
            }
            bad => panic!("undefined opcode {bad:#04x} at bci {bci}"),
        }
    }
}

/// SPEC §5.5's poll point, checked at `jump_back` — same flag
/// (`reg_block.poll_flag`) the compiled `Poll` Ir op reads directly via
/// `[x28, #VMREG_POLL_FLAG_OFFSET]` (S10 D5.3/D6), so an interrupt/trace
/// request is visible to both tiers with no separate synchronization. A
/// no-op hook still (nothing sets `poll_flag` nonzero yet — S2's original
/// status, just relocated) so the loop's shape never changes once a real
/// poll producer exists behind it.
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
            gc_stress: false,
            gc_stress_full_period: None,
            eden_kb: None,
            jit: crate::runtime::JitMode::Off,
        })
    }

    fn method_returning_self(vm: &mut VmState, argc: usize, ntemps: usize) -> MethodOop {
        let mut b = BytecodeBuilder::new();
        b.ret_self();
        let sel = vm.universe.intern(b"m");
        b.finish(vm, sel, argc, ntemps)
    }

    /// `tests_s06.md`'s `trace_count_flag`: `MACVM_TRACE=count` (enabled
    /// here by constructing `TraceFlags` directly, not via env var — tests
    /// run multi-threaded) counts a stable, deterministic bytecode total
    /// for a fixed doIt-equivalent method (S6 PERF procedure determinism).
    #[test]
    fn trace_count_flag() {
        let mut vm = VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: crate::runtime::vm_state::TraceFlags::parse("count"),
            gc_stress: false,
            gc_stress_full_period: None,
            eden_kb: None,
            jit: crate::runtime::JitMode::Off,
        });
        let mut b = BytecodeBuilder::new();
        b.push_smi_i8(42);
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 0, 0);
        let recv = vm.universe.nil_obj;
        let before = vm.bytecode_count;
        let result = run_method(&mut vm, method, recv, &[]);
        assert_eq!(SmallInt::try_from(result).unwrap().value(), 42);
        assert_eq!(
            vm.bytecode_count - before,
            2,
            "push_smi_i8 + ret_tos = 2 bytecodes"
        );
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
        let ret = unwind::do_return(&mut vm, result);
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

    /// SPEC §5.4 Algorithm 11: a non-boolean branch operand sends
    /// `#mustBeBoolean`, and the branch re-executes with the handler's
    /// result once it returns.
    #[test]
    fn must_be_boolean_handler_reexecutes_branch() {
        let mut vm = test_vm();
        let smi_klass = vm.universe.smi_klass;
        let mb_sel = vm.universe.sel_must_be_boolean;
        let mut h = BytecodeBuilder::new();
        h.push_true();
        h.ret_tos();
        let handler = h.finish(&mut vm, mb_sel, 0, 0);
        crate::runtime::lookup::install_method(&mut vm, smi_klass, mb_sel, handler);

        let mut b = BytecodeBuilder::new();
        let true_branch = b.new_label();
        let end = b.new_label();
        b.push_smi_i8(5); // non-boolean
        b.br_true_fwd(true_branch);
        b.push_smi_i8(0); // false-branch value
        b.jump_fwd(end);
        b.bind(true_branch);
        b.push_smi_i8(1); // true-branch value
        b.bind(end);
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 0, 0);
        let recv = vm.universe.nil_obj;

        let result = run_method(&mut vm, method, recv, &[]);
        assert_eq!(
            result,
            SmallInt::new(1).oop(),
            "the handler returned true: the true-branch must run"
        );
    }

    // `process_stack_overflow_exits_cleanly` lives in tests/it_interp.rs:
    // `env!("CARGO_BIN_EXE_macvm")` is only available to integration test
    // binaries, not `src/`-internal unit tests.

    #[test]
    fn trace_mode_smoke() {
        let mut vm = VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: crate::runtime::vm_state::TraceFlags::parse("bytecode"),
            gc_stress: false,
            gc_stress_full_period: None,
            eden_kb: None,
            jit: crate::runtime::JitMode::Off,
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

    #[test]
    fn ctx_temp_rw_depth0() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        b.push_smi_i8(9);
        b.store_ctx_temp_pop(0, 0);
        b.push_ctx_temp(0, 0);
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 0, 0);
        method.set_flags(0, 0, true, false, false, false, 1); // has_ctx, nctx=1

        let recv = vm.universe.nil_obj;
        let result = run_method(&mut vm, method, recv, &[]);
        assert_eq!(result, SmallInt::new(9).oop());
    }

    #[test]
    fn ctx_temp_depth_walk() {
        let mut vm = test_vm();
        // M's own context (home_hint nil): slot 0 = a (10).
        let ctx_m = crate::memory::alloc::alloc_context(&mut vm, 1);
        ctx_m.set_slot(0, SmallInt::new(10).oop());
        // B2's own context (home_hint = M's ctx, one has_ctx hop up): slot 0 = b (20).
        let ctx_b2 = crate::memory::alloc::alloc_context(&mut vm, 1);
        ctx_b2.set_home_hint(ctx_m.oop());
        ctx_b2.set_slot(0, SmallInt::new(20).oop());

        // B3 has no Context of its own — its frame aliases ctx_b2 directly
        // at FP+3 (B1, likewise ctx-less, is elided here since depth-walk
        // only cares about `has_ctx` hops, never lexical nesting).
        let method = method_returning_self(&mut vm, 0, 0);
        let recv = vm.universe.nil_obj;
        push(&mut vm, recv);
        let fp = vm.stack.sp;
        push_frame(&mut vm, method, 0, ENTRY_FRAME_SENTINEL, 0);
        Frame { fp }.set_context(&mut vm.stack, ctx_b2.oop());

        assert_eq!(ctx_temp_walk(&vm, 0).slot(0), SmallInt::new(20).oop());
        assert_eq!(ctx_temp_walk(&vm, 1).slot(0), SmallInt::new(10).oop());
    }

    #[test]
    fn frame_serial_monotonic() {
        let mut vm = test_vm();
        let method = method_returning_self(&mut vm, 0, 0);
        let recv = vm.universe.nil_obj;

        // 100 nested activations (never popped): serials strictly increase.
        let mut serials = Vec::new();
        for _ in 0..100 {
            push(&mut vm, recv);
            push_frame(&mut vm, method, 0, ENTRY_FRAME_SENTINEL, 0);
            serials.push(current_frame(&vm).serial(&vm.stack));
        }
        for w in serials.windows(2) {
            assert!(w[1] > w[0], "serials must strictly increase: {w:?}");
        }

        // Pop back to before the last frame's receiver, then push a fresh
        // frame at the exact same fp: the index is reused, the serial is
        // not.
        let last_fp = current_frame(&vm).fp;
        let last_serial = current_frame(&vm).serial(&vm.stack);
        vm.stack.sp = last_fp - 1; // drop the frame and its receiver
        push(&mut vm, recv);
        let reused_fp = vm.stack.sp;
        assert_eq!(reused_fp, last_fp, "must reuse the same stack index");
        push_frame(&mut vm, method, 0, ENTRY_FRAME_SENTINEL, 0);
        let new_serial = current_frame(&vm).serial(&vm.stack);
        assert_ne!(new_serial, last_serial);
        assert!(new_serial > last_serial);
    }
}
