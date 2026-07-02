//! Sprint S4 golden/integration tests (tests_s04.md §Integration/golden
//! tests, block-focused subset): closures, Contexts, and block re-entry.
//! BytecodeBuilder-assembled (source form arrives S5). Transcripts via a
//! `vm.out` buffer.

mod common;

use macvm::bytecode::BytecodeBuilder;
use macvm::interpreter::run_method;
use macvm::oops::smi::SmallInt;
use macvm::oops::wrappers::ClosureOop;
use macvm::runtime::lookup::install_method;
use macvm::runtime::vm_state::VmState;

/// Installs the smi `+` primitive (id 1) on `smi_klass` and `value` (50)
/// on `closure_klass` — everything these goldens need. Every klass/
/// selector is (re-)read right before use, never cached across a
/// `finish()`: under MACVM_GC_STRESS each `finish` scavenges, and a stale
/// cached oop handed to `install_method` gets faithfully handle-protected
/// as a stale pointer — which the next scavenge then chases into recycled
/// survivor space, corrupting the heap.
fn install_block_prims(vm: &mut VmState) {
    let value_sel = vm.universe.intern(b"value");
    let mut value_b = BytecodeBuilder::new();
    value_b.push_self();
    value_b.ret_self();
    let value_method = value_b.finish(vm, value_sel, 0, 0);
    value_method.set_primitive(50);
    let closure_klass = vm.universe.closure_klass;
    let value_sel = vm.universe.intern(b"value");
    install_method(vm, closure_klass, value_sel, value_method);

    let plus_sel = vm.universe.intern(b"+");
    let mut plus_b = BytecodeBuilder::new();
    plus_b.push_self();
    plus_b.ret_self();
    let plus_method = plus_b.finish(vm, plus_sel, 1, 0);
    plus_method.set_primitive(1);
    let smi_klass = vm.universe.smi_klass;
    let plus_sel = vm.universe.intern(b"+");
    install_method(vm, smi_klass, plus_sel, plus_method);
}

/// Gate 1: a counter closure sharing mutable state through a heap Context,
/// `value`d 3 times *after* its home method has already returned. Expected:
/// `1 2 3`.
#[test]
fn counter_closure() {
    let mut vm = common::test_vm();
    install_block_prims(&mut vm);
    let _ = vm.universe.intern(b"value"); // pre-intern; re-interned fresh at each use below
    let plus_sel = vm.universe.intern(b"+");

    let mut home = BytecodeBuilder::new();
    home.push_smi_i8(0);
    home.store_ctx_temp_pop(0, 0); // n := 0
                                   // [n := n + 1. n] — captures the enclosing (home's) Context.
    let mut blk_b = BytecodeBuilder::new();
    blk_b.push_ctx_temp(0, 0);
    blk_b.push_smi_i8(1);
    blk_b.send(&mut vm, plus_sel, 1);
    blk_b.store_ctx_temp_pop(0, 0);
    blk_b.push_ctx_temp(0, 0);
    blk_b.ret_tos();
    let blk_sel = vm.universe.intern(b"aBlock");
    let blk = blk_b.finish(&mut vm, blk_sel, 0, 0);
    blk.set_flags(0, 0, false, true, false, true, 0);
    let lit = home.intern_block_literal(&mut vm, blk);
    home.push_closure(lit, 0);
    home.ret_tos();
    let home_sel = vm.universe.intern(b"home");
    let home_method = home.finish(&mut vm, home_sel, 0, 1);
    home_method.set_flags(0, 1, true, false, false, false, 1); // has_ctx, nctx=1

    let recv = vm.universe.nil_obj;
    let closure_oop = run_method(&mut vm, home_method, recv, &[]);
    assert!(
        common::stack_clean(&vm),
        "home must have fully returned before the closure is ever value'd"
    );
    let closure = ClosureOop::try_from(closure_oop).expect("home must return the closure");
    // Under MACVM_GC_STRESS every builder/run step below scavenges: the
    // closure must ride in a handle across iterations, and the selector/
    // nil locals cached above are re-derived fresh per use (re-interning
    // is dedup — it returns the same Symbol at its current address).
    let scope = macvm::memory::handles::HandleScope::enter(&mut vm);
    let closure_h = scope.handle(&mut vm, closure);

    // `value` the closure 3 times, each via a fresh top-level activation —
    // proving the shared Context outlives `home`'s own (long-dead) frame.
    let mut results = Vec::new();
    for _ in 0..3 {
        let mut caller = BytecodeBuilder::new();
        caller.push_temp(0);
        let value_sel = vm.universe.intern(b"value");
        caller.send(&mut vm, value_sel, 0);
        caller.ret_tos();
        let caller_sel = vm.universe.intern(b"caller");
        let caller_method = caller.finish(&mut vm, caller_sel, 1, 0);
        let recv = vm.universe.nil_obj;
        let arg = closure_h.get(&vm).oop();
        let r = run_method(&mut vm, caller_method, recv, &[arg]);
        results.push(
            SmallInt::try_from(r)
                .expect("counter must return a smi")
                .value(),
        );
    }
    assert_eq!(results, vec![1, 2, 3]);
}

/// Gate 2: `M(ctx: a=10) ⊃ B1 ⊃ B2(ctx: b=20) ⊃ B3 = [a + b + c]` with `c` a
/// value-capture (30). `value` chains to compute 60; `B3` also stores
/// through to `a` (depth 1), and `M` observes 99 afterward. Exercises
/// depths 0/1, value captures, and store-through ctx_temp addressing.
#[test]
fn nested_blocks_3deep() {
    let mut vm = common::test_vm();
    install_block_prims(&mut vm);
    let _ = vm.universe.intern(b"value"); // pre-intern; re-interned fresh at each use below
    let plus_sel = vm.universe.intern(b"+");

    // B3 = [a + b + c. a := 99. a] — has no ctx of its own; captures the
    // enclosing (B2's) Context, plus a value-capture `c`=30.
    let mut b3_b = BytecodeBuilder::new();
    b3_b.push_ctx_temp(1, 0); // a (one has_ctx hop up: B2's ctx -> M's ctx)
    b3_b.push_ctx_temp(0, 0); // b (B2's own ctx, depth 0)
    b3_b.send(&mut vm, plus_sel, 1);
    b3_b.push_temp(0); // c: the value-capture, unified temp index 0 (argc=0)
    b3_b.send(&mut vm, plus_sel, 1);
    b3_b.push_smi_i8(99);
    b3_b.store_ctx_temp_pop(1, 0); // a := 99 (store-through)
    b3_b.ret_tos();
    let b3_sel = vm.universe.intern(b"aBlock");
    let b3_blk = b3_b.finish(&mut vm, b3_sel, 0, 1);
    b3_blk.set_flags(0, 1, false, true, false, true, 0);

    // B2(ctx: b=20) ⊃ [B3] — captures the enclosing (B1-aliased-M's) ctx,
    // has its own ctx (nctx=1) for `b`. `value` is RE-interned before each
    // builder use below: under MACVM_GC_STRESS the `value_sel` local cached
    // at the top goes stale across every intervening `finish` (re-interning
    // is dedup — same Symbol, current address).
    let mut b2_builder = BytecodeBuilder::new();
    b2_builder.push_smi_i8(20);
    b2_builder.store_ctx_temp_pop(0, 0); // b := 20
    let b3_lit = b2_builder.intern_block_literal(&mut vm, b3_blk);
    b2_builder.push_smi_i8(30); // value-capture c
    b2_builder.push_closure(b3_lit, 1);
    let value_sel = vm.universe.intern(b"value");
    b2_builder.send(&mut vm, value_sel, 0);
    b2_builder.ret_tos();
    let b2_sel = vm.universe.intern(b"aBlock");
    let b2_method = b2_builder.finish(&mut vm, b2_sel, 0, 1);
    b2_method.set_flags(0, 1, true, true, false, true, 1); // is_block, has_ctx, nctx=1, captures_ctx

    // B1 ⊃ [B2] — no ctx of its own, aliases the enclosing (M's) ctx.
    let mut b1_builder = BytecodeBuilder::new();
    let b2_lit = b1_builder.intern_block_literal(&mut vm, b2_method);
    b1_builder.push_closure(b2_lit, 0);
    let value_sel = vm.universe.intern(b"value");
    b1_builder.send(&mut vm, value_sel, 0);
    b1_builder.ret_tos();
    let b1_sel = vm.universe.intern(b"aBlock");
    let b1_method = b1_builder.finish(&mut vm, b1_sel, 0, 0);
    b1_method.set_flags(0, 0, false, true, false, true, 0); // is_block, captures_ctx, no own ctx

    // M(ctx: a=10) — builds B1 and immediately values it.
    let mut m = BytecodeBuilder::new();
    m.push_smi_i8(10);
    m.store_ctx_temp_pop(0, 0); // a := 10
    let b1_lit = m.intern_block_literal(&mut vm, b1_method);
    m.push_closure(b1_lit, 0);
    let value_sel = vm.universe.intern(b"value");
    m.send(&mut vm, value_sel, 0);
    m.pop();
    m.push_ctx_temp(0, 0); // read `a` back after the store-through
    m.ret_tos();
    let m_sel = vm.universe.intern(b"m");
    let m_method = m.finish(&mut vm, m_sel, 0, 1);
    m_method.set_flags(0, 1, true, false, false, false, 1); // has_ctx, nctx=1

    let recv = vm.universe.nil_obj;
    let result = run_method(&mut vm, m_method, recv, &[]);
    assert_eq!(
        result,
        SmallInt::new(99).oop(),
        "M must observe the store-through to `a`"
    );
}

/// Gate 5: a closure `[x + x]` (value-capture x=21, standing in for `[x *
/// 2]` — `*` isn't wired in this focused test) whose home has already
/// returned, `value`d 3 times. No NLR, no error — legality assertion.
#[test]
fn block_reentry_after_home_return() {
    let mut vm = common::test_vm();
    install_block_prims(&mut vm);
    let _ = vm.universe.intern(b"value"); // pre-intern; re-interned fresh at each use below
    let plus_sel = vm.universe.intern(b"+");

    let mut home = BytecodeBuilder::new();
    home.push_smi_i8(21); // value-capture x
    let mut blk_b = BytecodeBuilder::new();
    blk_b.push_temp(0); // x
    blk_b.push_temp(0); // x
    blk_b.send(&mut vm, plus_sel, 1);
    blk_b.ret_tos();
    let blk_sel = vm.universe.intern(b"aBlock");
    let blk = blk_b.finish(&mut vm, blk_sel, 0, 1);
    blk.set_flags(0, 1, false, true, false, false, 0);
    let lit = home.intern_block_literal(&mut vm, blk);
    home.push_closure(lit, 1);
    home.ret_tos();
    let home_sel = vm.universe.intern(b"home");
    let home_method = home.finish(&mut vm, home_sel, 0, 0);

    let recv = vm.universe.nil_obj;
    let closure_oop = run_method(&mut vm, home_method, recv, &[]);
    assert!(common::stack_clean(&vm), "home must have fully returned");
    let closure = ClosureOop::try_from(closure_oop).unwrap();
    // Handle + fresh re-reads per iteration, same as `counter_closure` —
    // every step below scavenges under MACVM_GC_STRESS.
    let scope = macvm::memory::handles::HandleScope::enter(&mut vm);
    let closure_h = scope.handle(&mut vm, closure);

    let mut results = Vec::new();
    for _ in 0..3 {
        let mut caller = BytecodeBuilder::new();
        caller.push_temp(0);
        let value_sel = vm.universe.intern(b"value");
        caller.send(&mut vm, value_sel, 0);
        caller.ret_tos();
        let caller_sel = vm.universe.intern(b"caller");
        let caller_method = caller.finish(&mut vm, caller_sel, 1, 0);
        let recv = vm.universe.nil_obj;
        let arg = closure_h.get(&vm).oop();
        let r = run_method(&mut vm, caller_method, recv, &[arg]);
        results.push(SmallInt::try_from(r).expect("must return a smi").value());
        assert!(common::stack_clean(&vm));
    }
    assert_eq!(results, vec![42, 42, 42]);
}
