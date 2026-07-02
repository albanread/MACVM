//! Closure creation, Context allocation, and block activation (SPEC §2.3,
//! §5.4, S4). The only module that builds `ClosureOop`/`ContextOop` values
//! or pushes a block frame (`sprint_s04_detail.md` §Layer boundaries).

use crate::oops::home_ref::{pack_home_ref, HomeRef};
use crate::oops::smi::SmallInt;
use crate::oops::wrappers::{ClosureOop, MethodOop};
use crate::runtime::primitives::PrimResult;
use crate::runtime::vm_state::VmState;

use super::stack::Frame;

/// SPEC §5.4 step 1: if `method.has_ctx()`, allocates a fresh
/// `Context(nctx)` and stores it into the (already-pushed, already-`nil`)
/// `FRAME_CONTEXT` slot at `fp`. Called only *after* the frame is fully
/// formed — SPEC's "frame fully formed before the allocation" rule means a
/// stress-GC during this allocation must always see a valid frame. Shared
/// by plain method activation (`interpreter::send::activate_method`) and
/// block activation (`activate_block`, below); `enclosing` is the
/// `home_hint` to link — `nil` for a method's own Context (methods have no
/// enclosing block-context chain), the inherited context for a block's.
pub fn maybe_alloc_context(
    vm: &mut VmState,
    method: MethodOop,
    fp: usize,
    enclosing: crate::oops::Oop,
) {
    if !method.has_ctx() {
        return;
    }
    let nctx = method.nctx();
    let ctx = crate::memory::alloc::alloc_context(vm, nctx);
    ctx.set_home_hint(enclosing);
    Frame { fp }.set_context(&mut vm.stack, ctx.oop());
}

/// SPEC §5.4 Algorithm 3 — used by the `value`/`value:`/… primitives
/// (`runtime::primitives`, ids 50–54). `argc` is the number of block
/// arguments the caller supplied (already sitting on the operand stack,
/// exactly where a normal send leaves them — unified arg indexing reads
/// them from there, same as any other activation). Returns `Fail` on an
/// argc mismatch (the installed `value:`-family method's own bytecode
/// fallback handles that as a guest-visible error); otherwise pushes a
/// real block frame and returns `Activated` — the caller (the primitive
/// call site in `interpreter::send::activate_method`) must push no result.
pub fn activate_block(vm: &mut VmState, closure: ClosureOop, argc: usize) -> PrimResult {
    let blk = closure.method();
    if blk.argc() != argc {
        return PrimResult::Fail;
    }
    super::send::bump_invocation(blk);

    // Push the fixed frame slots exactly like a plain method activation
    // (`interpreter::push_frame`), except FRAME_RECEIVER is the closure's
    // captured home receiver, never "whatever sits at fp-argc-1" (that
    // slot holds the closure itself, not `self`).
    let fp_new = vm.stack.sp;
    vm.stack.push(blk.oop());
    let saved_fp = vm.stack.fp as i64;
    let saved_bci = vm.regs.bci;
    vm.stack.push(SmallInt::new(saved_fp).oop());
    vm.stack.push(SmallInt::new(saved_bci as i64).oop());
    vm.stack.push(vm.universe.nil_obj); // context: patched below
    vm.stack.push(closure.copied(0)); // receiver: the captured home self
    let serial = vm.alloc_frame_serial();
    vm.stack.push(SmallInt::new(serial as i64).oop());
    vm.stack.push(vm.universe.nil_obj); // marker: none
    let ntemps = blk.ntemps();
    let nil = vm.universe.nil_obj;
    for _ in 0..ntemps {
        vm.stack.push(nil);
    }
    vm.stack.activate_frame(fp_new);

    // Context: a `has_ctx` block gets a fresh Context linked to the
    // enclosing chain; a ctx-less block's frame *aliases* the enclosing
    // Context directly at FP+3 (this is what makes ctx_temp depth-counting
    // uniform — see `ctx_temp_walk`'s doc).
    let captures_ctx = blk.captures_ctx();
    let enclosing = if captures_ctx {
        closure.copied(1)
    } else {
        vm.universe.nil_obj
    };
    if blk.has_ctx() {
        maybe_alloc_context(vm, blk, fp_new, enclosing);
    } else {
        Frame { fp: fp_new }.set_context(&mut vm.stack, enclosing);
    }

    // Value captures land in the first LOCAL temp slots — `ntemps` counts
    // value-captures-then-block-locals, and unified indexing already puts
    // local temp `t` (`t >= argc`) at `fp + FRAME_TEMPS_BASE + (t - argc)`,
    // so writing at unified index `argc + i` lands exactly there.
    let value_base = 1 + captures_ctx as usize;
    let n_value_captures = closure.ncopied() - value_base;
    let frame = Frame { fp: fp_new };
    for i in 0..n_value_captures {
        let v = closure.copied(value_base + i);
        frame.set_temp(&mut vm.stack, argc + i, v);
    }

    vm.regs.method = Some(blk);
    vm.regs.bci = 0;
    PrimResult::Activated
}

/// `push_closure <u8 lit> <u8 n_value_captures>` (SPEC §4.2, §5.4 Algorithm
/// 2). `blk` is the `CompiledBlock` literal; `n_value_captures` is how many
/// read-only value captures the enclosing bytecode pushed immediately
/// before this instruction (deepest-first, i.e. the first-pushed value
/// lands at the lowest `copied[]` index among the value captures).
///
/// **Resolution of an underspecified point** (`sprint_s04_detail.md`'s own
/// pseudocode says the compiler "pushes: receiver, [context], value-
/// captures" before `push_closure`, but no pinned opcode exists to push
/// "my own context oop" as a value, and `push_self` for the receiver would
/// be redundant with what the VM already knows). This implementation has
/// the VM supply `copied[0]` (the current frame's receiver) and, when
/// `blk.captures_ctx()`, `copied[1]` (the current frame's Context) directly
/// — the operand stack carries ONLY the value captures. This still
/// satisfies the pinned `copied[]` layout convention (`oops::closure`'s
/// doc) byte-for-byte; it just means the *bytecode operand* `ncopied`
/// counts value captures alone, not the full `copied[]` length.
pub fn make_closure(vm: &mut VmState, blk: MethodOop, n_value_captures: usize) -> ClosureOop {
    debug_assert!(
        blk.is_block(),
        "make_closure: push_closure literal is not a CompiledBlock"
    );
    let captures_ctx = blk.captures_ctx();
    let value_base = 1 + captures_ctx as usize;
    let total = value_base + n_value_captures;

    // Allocate BEFORE popping — the value captures stay live as GC roots on
    // the operand stack during the allocation (S7 choke-point pattern).
    let closure = crate::memory::alloc::alloc_closure(vm, total);
    let sp = vm.stack.sp;
    let base = sp - n_value_captures;
    for i in 0..n_value_captures {
        closure.set_copied(value_base + i, vm.stack.get(base + i));
    }
    vm.stack.sp = base;

    closure.set_method(blk);
    let current = Frame { fp: vm.stack.fp };
    closure.set_copied(0, current.receiver(&vm.stack));
    if captures_ctx {
        closure.set_copied(1, current.context(&vm.stack));
    }

    let home = home_ref_for_new_closure(vm);
    closure.set_home(home);
    closure
}

/// The **home-ref propagation rule** (SPEC §5.4 Algorithm 2 step 4 — the
/// classic NLR bug if gotten wrong, `sprint_s04_detail.md` §Pitfalls): if
/// the currently executing method is itself a block, the new closure's
/// home is copied verbatim from the *current block frame's own closure*
/// (found at `receiver_slot()` — the closure that was `value`d to activate
/// this frame), never computed fresh from the current (block) frame.
/// Otherwise the current frame genuinely IS the home.
fn home_ref_for_new_closure(vm: &VmState) -> crate::oops::smi::SmallInt {
    // Read off the ACTIVE FRAME, not `vm.regs.method` — `vm.regs` only
    // tracks state around send boundaries (SPEC §5.3), so it can be stale
    // or `None` here; the frame itself is always current.
    let frame = Frame { fp: vm.stack.fp };
    let current_method = frame.method(&vm.stack);
    if current_method.is_block() {
        let enclosing_closure_oop = vm.stack.get(frame.receiver_slot(&vm.stack));
        let enclosing_closure = ClosureOop::try_from(enclosing_closure_oop)
            .expect("home_ref_for_new_closure: block frame's receiver slot is not a Closure");
        enclosing_closure.home()
    } else {
        pack_home_ref(HomeRef {
            proc: 0,
            serial: frame.serial(&vm.stack),
            fp: vm.stack.fp,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::BytecodeBuilder;
    use crate::oops::home_ref::unpack_home_ref;
    use crate::oops::smi::SmallInt;
    use crate::oops::wrappers::ContextOop;
    use crate::runtime::vm_state::{VmOptions, VmState};

    fn test_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            eden_kb: None,
        })
    }

    #[test]
    fn context_alloc_method() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        b.ret_self();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 0, 0);
        method.set_flags(0, 0, true, false, false, false, 3); // has_ctx, nctx=3

        let recv = vm.universe.nil_obj;
        super::super::push(&mut vm, recv);
        let fp = vm.stack.sp;
        super::super::push_frame(
            &mut vm,
            method,
            0,
            crate::oops::layout::ENTRY_FRAME_SENTINEL,
            0,
        );
        let nil = vm.universe.nil_obj;
        maybe_alloc_context(&mut vm, method, fp, nil);

        let frame = Frame { fp };
        let ctx = ContextOop::try_from(frame.context(&vm.stack)).expect("FP+3 must be a Context");
        assert_eq!(ctx.size(), 3);
        assert_eq!(ctx.home_hint(), vm.universe.nil_obj);
        for i in 0..3 {
            assert_eq!(ctx.slot(i), vm.universe.nil_obj);
        }
    }

    #[test]
    fn context_no_alloc_without_has_ctx() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        b.ret_self();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 0, 0);

        let recv = vm.universe.nil_obj;
        super::super::push(&mut vm, recv);
        let fp = vm.stack.sp;
        super::super::push_frame(
            &mut vm,
            method,
            0,
            crate::oops::layout::ENTRY_FRAME_SENTINEL,
            0,
        );
        let eden_top_before = vm.universe.eden.top;
        let nil = vm.universe.nil_obj;
        maybe_alloc_context(&mut vm, method, fp, nil);
        assert_eq!(
            vm.universe.eden.top, eden_top_before,
            "no Context allocated"
        );

        let frame = Frame { fp };
        assert_eq!(frame.context(&vm.stack), vm.universe.nil_obj);
    }

    #[test]
    fn push_closure_copied_layout() {
        let mut vm = test_vm();
        let mut home = BytecodeBuilder::new();
        let lit = home.build_block(&mut vm, 0, 0, false, 0, false, |b, _vm| {
            b.ret_self();
        });
        home.push_smi_i8(10);
        home.push_smi_i8(20);
        home.push_closure(lit, 2);
        home.ret_tos();
        let sel = vm.universe.intern(b"m");
        let method = home.finish(&mut vm, sel, 0, 0);

        let recv = SmallInt::new(999).oop();
        let result = crate::interpreter::run_method(&mut vm, method, recv, &[]);
        let closure = ClosureOop::try_from(result).expect("result must be a Closure");
        assert_eq!(closure.ncopied(), 3); // receiver + 2 value captures, no ctx
        assert_eq!(closure.copied(0), recv);
        assert_eq!(closure.copied(1), SmallInt::new(10).oop());
        assert_eq!(closure.copied(2), SmallInt::new(20).oop());
    }

    #[test]
    fn push_closure_home_method() {
        let mut vm = test_vm();
        let mut home = BytecodeBuilder::new();
        let lit = home.build_block(&mut vm, 0, 0, false, 0, false, |b, _vm| {
            b.ret_self();
        });
        home.push_closure(lit, 0);
        home.ret_tos();
        let sel = vm.universe.intern(b"m");
        let method = home.finish(&mut vm, sel, 0, 0);

        // Predict the entry frame's fp/serial: `run_method` pushes `recv`
        // (sp -> sp+1) then activates the entry frame at that new sp;
        // `alloc_frame_serial` hands out the *next* serial value.
        let expected_fp = vm.stack.sp + 1;
        let expected_serial = vm.peek_next_frame_serial();

        let recv = vm.universe.nil_obj;
        let result = crate::interpreter::run_method(&mut vm, method, recv, &[]);
        let closure = ClosureOop::try_from(result).unwrap();
        let home_ref = unpack_home_ref(closure.home());
        assert_eq!(home_ref.proc, 0);
        assert_eq!(home_ref.fp, expected_fp);
        assert_eq!(home_ref.serial, expected_serial);
    }

    #[test]
    fn push_closure_home_propagates() {
        // A closure made *inside* a block frame must copy that block's own
        // closure's home verbatim — the classic NLR bug is computing a
        // fresh home from the (block) frame instead. Since `activate_block`
        // doesn't exist yet, this hand-builds the "currently executing a
        // block" precondition directly: push a block-flagged method's
        // frame with a real Closure sitting in its receiver-arg slot (the
        // shape `activate_block` will produce in S4-6), matching exactly
        // what `home_ref_for_new_closure` reads.
        let mut vm = test_vm();

        // The "outer" closure whose home we expect to see propagated.
        let mut home = BytecodeBuilder::new();
        let outer_lit = home.build_block(&mut vm, 0, 0, false, 0, false, |b, _vm| {
            b.ret_self();
        });
        home.push_closure(outer_lit, 0);
        home.ret_tos();
        let outer_sel = vm.universe.intern(b"outer");
        let outer_method = home.finish(&mut vm, outer_sel, 0, 0);
        let recv = vm.universe.nil_obj;
        let outer_closure_oop = crate::interpreter::run_method(&mut vm, outer_method, recv, &[]);
        let outer_home = ClosureOop::try_from(outer_closure_oop).unwrap().home();

        // A second, INNER block (built standalone — its own literal pool
        // is irrelevant here) that itself does `push_closure` when run.
        let mut inner_home_builder = BytecodeBuilder::new();
        let inner_lit = inner_home_builder.build_block(&mut vm, 0, 0, false, 0, false, |b, _vm| {
            b.ret_self();
        });
        inner_home_builder.ret_self(); // never run — only `discard_method`'s literal pool matters
        let discard_sel = vm.universe.intern(b"discard");
        let discard_method = inner_home_builder.finish(&mut vm, discard_sel, 0, 0);
        let inner_blk =
            crate::oops::wrappers::MethodOop::try_from(discard_method.literals().at(inner_lit))
                .unwrap();

        // Hand-build a block-activation frame for `inner_blk` whose
        // receiver-arg slot holds `outer_closure` (i.e. simulate having
        // been `value`d from the outer closure).
        vm.stack.push(outer_closure_oop); // the "closure" argument slot
        crate::interpreter::push_frame(
            &mut vm,
            inner_blk,
            0,
            crate::oops::layout::ENTRY_FRAME_SENTINEL,
            0,
        );

        let new_home = home_ref_for_new_closure(&vm);
        assert_eq!(
            unpack_home_ref(new_home),
            unpack_home_ref(outer_home),
            "a closure made inside a block must inherit the block's own closure's home"
        );
    }

    #[test]
    fn activate_block_frame_shape() {
        let mut vm = test_vm();
        let mut home = BytecodeBuilder::new();
        // argc=1, ntemps=1 (1 value capture, 0 block locals), no ctx.
        let lit = home.build_block(&mut vm, 1, 1, false, 0, false, |b, _vm| {
            b.ret_self();
        });
        home.push_smi_i8(7); // 1 value capture
        home.push_closure(lit, 1);
        home.ret_tos();
        let sel = vm.universe.intern(b"m");
        let method = home.finish(&mut vm, sel, 0, 0);
        let recv = SmallInt::new(555).oop();
        let closure_oop = crate::interpreter::run_method(&mut vm, method, recv, &[]);
        let closure = ClosureOop::try_from(closure_oop).unwrap();

        // Simulate the `value:` send: push closure + 1 arg, then activate
        // directly (the real prim call site does exactly this).
        vm.stack.push(closure.oop());
        let arg = SmallInt::new(42).oop();
        vm.stack.push(arg);
        let result = activate_block(&mut vm, closure, 1);
        assert_eq!(result, PrimResult::Activated);

        let fp = vm.stack.fp;
        let frame = Frame { fp };
        assert_eq!(frame.method(&vm.stack), closure.method());
        assert_eq!(
            frame.receiver(&vm.stack),
            recv,
            "receiver must be the closure's captured home receiver, not the closure itself"
        );
        assert_eq!(vm.stack.get(frame.receiver_slot(&vm.stack)), closure.oop());
        assert_eq!(frame.temp(&vm.stack, 0), arg, "arg from the caller's stack");
        assert_eq!(
            frame.temp(&vm.stack, 1),
            SmallInt::new(7).oop(),
            "value capture in the first local temp slot"
        );
        assert_eq!(
            vm.stack.sp,
            fp + crate::oops::layout::FRAME_TEMPS_BASE + 1,
            "sp must sit exactly past the 1 pushed temp"
        );
    }

    #[test]
    fn activate_block_argc_mismatch() {
        let mut vm = test_vm();
        let mut home = BytecodeBuilder::new();
        let lit = home.build_block(&mut vm, 2, 2, false, 0, false, |b, _vm| {
            b.ret_self();
        });
        home.push_closure(lit, 0);
        home.ret_tos();
        let sel = vm.universe.intern(b"m");
        let method = home.finish(&mut vm, sel, 0, 0);
        let recv = vm.universe.nil_obj;
        let closure_oop = crate::interpreter::run_method(&mut vm, method, recv, &[]);
        let closure = ClosureOop::try_from(closure_oop).unwrap();

        vm.stack.push(closure.oop());
        vm.stack.push(vm.universe.nil_obj); // only 1 arg; the block wants 2
        let sp_before = vm.stack.sp;
        let result = activate_block(&mut vm, closure, 1);
        assert_eq!(result, PrimResult::Fail);
        assert_eq!(vm.stack.sp, sp_before, "Fail must not touch the stack");
    }

    #[test]
    fn context_chain_block() {
        let mut vm = test_vm();
        // Home method: has_ctx, 1 slot (a = 10).
        let mut home = BytecodeBuilder::new();
        home.push_smi_i8(10);
        home.store_ctx_temp_pop(0, 0);
        // Block: has_ctx too, captures the enclosing context.
        let lit = home.build_block(&mut vm, 0, 1, true, 1, true, |b, _vm| {
            b.ret_self();
        });
        home.push_closure(lit, 0);
        home.ret_tos();
        let sel = vm.universe.intern(b"m");
        let method = home.finish(&mut vm, sel, 0, 1);
        method.set_flags(0, 1, true, false, false, false, 1); // home's own has_ctx/nctx

        let recv = vm.universe.nil_obj;
        let closure_oop = crate::interpreter::run_method(&mut vm, method, recv, &[]);
        let closure = ClosureOop::try_from(closure_oop).unwrap();

        vm.stack.push(closure.oop());
        let result = activate_block(&mut vm, closure, 0);
        assert_eq!(result, PrimResult::Activated);

        let frame = Frame { fp: vm.stack.fp };
        let block_ctx = ContextOop::try_from(frame.context(&vm.stack))
            .expect("has_ctx block must get its own fresh Context");
        let enclosing =
            ContextOop::try_from(closure.copied(1)).expect("copied[1] is the enclosing Context");
        assert_eq!(block_ctx.home_hint(), enclosing.oop());
    }

    #[test]
    fn ctxless_block_aliases() {
        let mut vm = test_vm();
        let mut home = BytecodeBuilder::new();
        home.push_smi_i8(10);
        home.store_ctx_temp_pop(0, 0);
        // Block has NO own context, but does capture the enclosing one.
        let lit = home.build_block(&mut vm, 0, 0, false, 0, true, |b, _vm| {
            b.ret_self();
        });
        home.push_closure(lit, 0);
        home.ret_tos();
        let sel = vm.universe.intern(b"m");
        let method = home.finish(&mut vm, sel, 0, 1);
        method.set_flags(0, 1, true, false, false, false, 1);

        let recv = vm.universe.nil_obj;
        let closure_oop = crate::interpreter::run_method(&mut vm, method, recv, &[]);
        let closure = ClosureOop::try_from(closure_oop).unwrap();

        vm.stack.push(closure.oop());
        let result = activate_block(&mut vm, closure, 0);
        assert_eq!(result, PrimResult::Activated);

        let frame = Frame { fp: vm.stack.fp };
        // Identity, not structural equality: the ctx-less block's FP+3
        // must be the SAME object as the captured enclosing context.
        assert_eq!(frame.context(&vm.stack), closure.copied(1));
    }

    #[test]
    fn block_own_ics_counters() {
        let mut vm = test_vm();
        let object_klass = vm.universe.object_klass;
        let foo_sel = vm.universe.intern(b"foo");
        let foo_method = {
            let mut b = BytecodeBuilder::new();
            b.push_smi_i8(1);
            b.ret_tos();
            let s = vm.universe.intern(b"fooImpl");
            b.finish(&mut vm, s, 0, 0)
        };
        crate::runtime::lookup::install_method(&mut vm, object_klass, foo_sel, foo_method);

        let mut home = BytecodeBuilder::new();
        let lit = home.build_block(&mut vm, 0, 0, false, 0, false, |b, vm| {
            b.push_self();
            b.send(vm, foo_sel, 0);
            b.ret_tos();
        });
        home.push_self();
        home.push_closure(lit, 0);
        home.ret_tos();
        let sel = vm.universe.intern(b"m");
        let method = home.finish(&mut vm, sel, 0, 0);
        crate::runtime::lookup::install_method(&mut vm, object_klass, sel, method);

        let recv = crate::memory::alloc::alloc_slots(&mut vm, object_klass).oop();
        let closure_oop = crate::interpreter::run_method(&mut vm, method, recv, &[]);
        let closure = ClosureOop::try_from(closure_oop).unwrap();
        let blk = closure.method();
        assert_eq!(
            blk.counters() & crate::oops::layout::COUNTERS_INVOCATION_MASK,
            0
        );
        let home_counter_before = method.counters() & crate::oops::layout::COUNTERS_INVOCATION_MASK;

        vm.stack.push(closure.oop());
        let result = activate_block(&mut vm, closure, 0);
        assert_eq!(result, PrimResult::Activated);
        assert_eq!(
            blk.counters() & crate::oops::layout::COUNTERS_INVOCATION_MASK,
            1,
            "the block's own counter must bump"
        );
        assert_eq!(
            method.counters() & crate::oops::layout::COUNTERS_INVOCATION_MASK,
            home_counter_before,
            "the home method's counter is unaffected by the block's activation"
        );

        // The `send foo` inside the block's body indexes the BLOCK's own
        // IC table, not the home method's.
        let ic = crate::interpreter::ic::InterpreterIc::at(blk, 0);
        assert_eq!(ic.selector(), foo_sel);
        assert_eq!(
            method.ics().len(),
            0,
            "the home method emits no sends of its own"
        );
    }
}
