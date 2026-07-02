//! Sprint S4 golden/integration tests (tests_s04.md §Integration/golden
//! tests, NLR-focused subset): non-local return, `ensure:`/`ifCurtailed:`,
//! and `cannotReturn:`. BytecodeBuilder-assembled (source form arrives S5).
//! Transcripts via a `vm.out` buffer.

mod common;

use macvm::bytecode::BytecodeBuilder;
use macvm::interpreter::run_method;
use macvm::oops::smi::SmallInt;
use macvm::oops::Oop;
use macvm::runtime::lookup::install_method;
use macvm::runtime::vm_state::{OutputBuffer, VmState};

fn string_literal(vm: &mut VmState, s: &str) -> Oop {
    let bytearray_klass = vm.universe.bytearray_klass;
    let b = macvm::memory::alloc::alloc_indexable_bytes(vm, bytearray_klass, s.len());
    for (i, byte) in s.bytes().enumerate() {
        b.byte_at_put(i, byte);
    }
    b.oop()
}

/// Installs `value` (50), `ensure:` (60), `ifCurtailed:` (61) on
/// `closure_klass`, and `print:` (91, `printOnStdout:`)/`quit` (90) on
/// `object_klass` — everything these goldens need.
fn install_prims(vm: &mut VmState) {
    let closure_klass = vm.universe.closure_klass;
    let object_klass = vm.universe.object_klass;

    let mk = |vm: &mut VmState, name: &[u8], argc: usize, prim: i64| {
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.ret_self();
        let sel = vm.universe.intern(name);
        let m = b.finish(vm, sel, argc, 0);
        m.set_primitive(prim);
        (sel, m)
    };

    let (sel, m) = mk(vm, b"value", 0, 50);
    install_method(vm, closure_klass, sel, m);
    let (sel, m) = mk(vm, b"ensure:", 1, 60);
    install_method(vm, closure_klass, sel, m);
    let (sel, m) = mk(vm, b"ifCurtailed:", 1, 61);
    install_method(vm, closure_klass, sel, m);
    let (sel, m) = mk(vm, b"print:", 1, 91);
    install_method(vm, object_klass, sel, m);
    let (sel, m) = mk(vm, b"quit", 0, 90);
    install_method(vm, object_klass, sel, m);
}

/// `[T print: 'body'. 7] ensure: [T print: 'cleanup']` — the handler runs
/// *after* the body, *before* the result is delivered; the delivered value
/// is the body's own (7), the handler's own result discarded.
#[test]
fn ensure_normal_completion() {
    let mut vm = common::test_vm();
    install_prims(&mut vm);
    let print_sel = vm.universe.intern(b"print:");
    let ensure_sel = vm.universe.intern(b"ensure:");
    let body_str = string_literal(&mut vm, "body");
    let cleanup_str = string_literal(&mut vm, "cleanup");

    let mut home = BytecodeBuilder::new();
    let protected_lit = home.build_block(&mut vm, 0, 0, false, 0, false, |b, vm| {
        b.push_self();
        b.push_literal(vm, body_str);
        b.send(vm, print_sel, 1);
        b.pop();
        b.push_smi_i8(7);
        b.ret_tos();
    });
    let handler_lit = home.build_block(&mut vm, 0, 0, false, 0, false, |b, vm| {
        b.push_self();
        b.push_literal(vm, cleanup_str);
        b.send(vm, print_sel, 1);
        b.ret_self();
    });
    home.push_closure(protected_lit, 0);
    home.push_closure(handler_lit, 0);
    home.send(&mut vm, ensure_sel, 1);
    home.ret_tos();
    let sel = vm.universe.intern(b"run");
    let method = home.finish(&mut vm, sel, 0, 0);

    let buf = OutputBuffer::new();
    vm.out = Box::new(buf.clone());
    let recv = vm.universe.nil_obj;
    let result = run_method(&mut vm, method, recv, &[]);
    assert_eq!(result, SmallInt::new(7).oop());
    assert_eq!(buf.as_string(), "bodycleanup");
    assert!(common::stack_clean(&vm));
}

/// `ifCurtailed:`'s handler runs on unwind (NLR) but is skipped on normal
/// completion.
#[test]
fn if_curtailed_both_paths() {
    fn run_variant(do_nlr: bool) -> (Oop, String) {
        let mut vm = common::test_vm();
        install_prims(&mut vm);
        let if_curtailed_sel = vm.universe.intern(b"ifCurtailed:");
        let print_sel = vm.universe.intern(b"print:");
        let marker_str = string_literal(&mut vm, "curtailed");

        let handler_lit =
            macvm::bytecode::build_standalone_block(&mut vm, 0, 0, false, 0, false, |h, vm| {
                h.push_self();
                h.push_literal(vm, marker_str);
                h.send(vm, print_sel, 1);
                h.ret_self();
            });

        let mut run_b = BytecodeBuilder::new();
        let protected_lit = run_b.build_block(&mut vm, 0, 0, false, 0, false, |b, _vm| {
            if do_nlr {
                b.push_smi_i8(1);
                b.nlr_tos();
            } else {
                b.push_smi_i8(7);
                b.ret_tos();
            }
        });
        run_b.push_closure(protected_lit, 0);
        let hl = run_b.intern_block_literal(&mut vm, handler_lit);
        run_b.push_closure(hl, 0);
        run_b.send(&mut vm, if_curtailed_sel, 1);
        run_b.ret_tos();
        let run_sel = vm.universe.intern(b"run");
        let method = run_b.finish(&mut vm, run_sel, 0, 0);

        let buf = OutputBuffer::new();
        vm.out = Box::new(buf.clone());
        let recv = vm.universe.nil_obj;
        let result = run_method(&mut vm, method, recv, &[]);
        assert!(common::stack_clean(&vm));
        (result, buf.as_string())
    }

    let (result, transcript) = run_variant(false);
    assert_eq!(result, SmallInt::new(7).oop());
    assert_eq!(transcript, "", "normal completion must NOT run the handler");

    let (result, transcript) = run_variant(true);
    assert_eq!(result, SmallInt::new(1).oop());
    assert_eq!(
        transcript, "curtailed",
        "an NLR (curtailment) must run the handler"
    );
}

/// Gate 3: `run = [[^42] ensure: [T print: 'inner']] ensure: [T print:
/// 'outer']`, NLR from the innermost block whose home is `run`. Two marked
/// frames sit between the NLR site and home; handlers must run
/// innermost-first, and `run`'s caller must see the delivered value.
#[test]
fn nlr_two_frames_ensure_order() {
    let mut vm = common::test_vm();
    install_prims(&mut vm);
    let print_sel = vm.universe.intern(b"print:");
    let ensure_sel = vm.universe.intern(b"ensure:");
    let run_sel = vm.universe.intern(b"run");
    let inner_str = string_literal(&mut vm, "inner");
    let outer_str = string_literal(&mut vm, "outer");
    let done_str = string_literal(&mut vm, "done");

    // Innermost blocks first, standalone — nested `build_block` calls can't
    // share one `&mut VmState` borrow.
    let handler_inner_blk =
        macvm::bytecode::build_standalone_block(&mut vm, 0, 0, false, 0, false, |h, vm| {
            h.push_self();
            h.push_literal(vm, inner_str);
            h.send(vm, print_sel, 1);
            h.ret_self();
        });
    let nlr_blk =
        macvm::bytecode::build_standalone_block(&mut vm, 0, 0, false, 0, false, |b, _vm| {
            b.push_smi_i8(42);
            b.nlr_tos();
        });

    let mut run_b = BytecodeBuilder::new();
    let outer_lit = run_b.build_block(&mut vm, 0, 0, false, 0, false, |lo, vm| {
        let handler_inner_lit = lo.intern_block_literal(vm, handler_inner_blk);
        let nlr_lit = lo.intern_block_literal(vm, nlr_blk);
        lo.push_closure(nlr_lit, 0);
        lo.push_closure(handler_inner_lit, 0);
        lo.send(vm, ensure_sel, 1);
        lo.ret_tos(); // unreachable: the NLR above always fires first
    });
    let handler_outer_lit = run_b.build_block(&mut vm, 0, 0, false, 0, false, |h, vm| {
        h.push_self();
        h.push_literal(vm, outer_str);
        h.send(vm, print_sel, 1);
        h.ret_self();
    });
    run_b.push_closure(outer_lit, 0);
    run_b.push_closure(handler_outer_lit, 0);
    run_b.send(&mut vm, ensure_sel, 1);
    run_b.ret_tos(); // unreachable: home receives the NLR directly
    let run_method_oop = run_b.finish(&mut vm, run_sel, 0, 0);
    let object_klass = vm.universe.object_klass;
    install_method(&mut vm, object_klass, run_sel, run_method_oop);

    let mut driver = BytecodeBuilder::new();
    driver.push_self();
    driver.send(&mut vm, run_sel, 0);
    driver.store_temp_pop(0);
    driver.push_self();
    driver.push_literal(&mut vm, done_str);
    driver.send(&mut vm, print_sel, 1);
    driver.pop();
    driver.push_temp(0);
    driver.ret_tos();
    let driver_sel = vm.universe.intern(b"driver");
    let driver_method = driver.finish(&mut vm, driver_sel, 0, 1);

    let buf = OutputBuffer::new();
    vm.out = Box::new(buf.clone());
    let recv = macvm::memory::alloc::alloc_slots(&mut vm, object_klass).oop();
    let result = run_method(&mut vm, driver_method, recv, &[]);
    assert_eq!(result, SmallInt::new(42).oop());
    assert_eq!(
        buf.as_string(),
        "innerouterdone",
        "handlers must run innermost-first, before the NLR value is delivered"
    );
    assert!(common::stack_clean(&vm));
}

/// Gate 4: home method returns a `[^1]` closure; a `BlockClosure>>
/// cannotReturn:` handler prints `cannotReturn` and quits. Expected: clean
/// exit, the handler's transcript line, `1` as the failed NLR's value.
#[test]
fn cannot_return_escaped() {
    let mut vm = common::test_vm();
    install_prims(&mut vm);
    let value_sel = vm.universe.intern(b"value");
    let print_sel = vm.universe.intern(b"print:");
    let quit_sel = vm.universe.intern(b"quit");
    let cannot_return_sel = vm.universe.sel_cannot_return;
    let object_klass = vm.universe.object_klass;
    let msg_str = string_literal(&mut vm, "cannotReturn");

    let mut cr = BytecodeBuilder::new();
    cr.push_self();
    cr.push_literal(&mut vm, msg_str);
    cr.send(&mut vm, print_sel, 1);
    cr.send(&mut vm, quit_sel, 0);
    cr.ret_tos();
    let cr_method = cr.finish(&mut vm, cannot_return_sel, 1, 0);
    install_method(&mut vm, object_klass, cannot_return_sel, cr_method);

    let mut home = BytecodeBuilder::new();
    let lit = home.build_block(&mut vm, 0, 0, false, 0, false, |b, _vm| {
        b.push_smi_i8(1);
        b.nlr_tos();
    });
    home.push_closure(lit, 0);
    home.ret_tos();
    let home_sel = vm.universe.intern(b"home");
    let home_method = home.finish(&mut vm, home_sel, 0, 0);

    let recv = macvm::memory::alloc::alloc_slots(&mut vm, object_klass).oop();
    let closure_oop = run_method(&mut vm, home_method, recv, &[]);
    assert!(
        common::stack_clean(&vm),
        "home must have fully returned (and its frame died) before the escaped closure is value'd"
    );

    let mut driver = BytecodeBuilder::new();
    driver.push_temp(0);
    driver.send(&mut vm, value_sel, 0);
    driver.ret_tos();
    let driver_sel = vm.universe.intern(b"driver");
    let driver_method = driver.finish(&mut vm, driver_sel, 1, 0);

    let buf = OutputBuffer::new();
    vm.out = Box::new(buf.clone());
    let _ = run_method(&mut vm, driver_method, recv, &[closure_oop]);
    assert_eq!(buf.as_string(), "cannotReturn");
    assert!(
        vm.exit_requested,
        "the handler's `quit` must have been observed"
    );
}
