//! Sprint S2 execution kernels (tests_s02.md §Integration/golden tests,
//! items 1–9) plus the subprocess-observed stress tests that need a real
//! process exit code (`env!("CARGO_BIN_EXE_macvm")` is only available to
//! integration test binaries).

mod common;

use macvm::bytecode::BytecodeBuilder;
use macvm::interpreter::run_method;
use macvm::memory::alloc;
use macvm::oops::smi::SmallInt;
use macvm::oops::Oop;
use macvm::runtime::{VmOptions, VmState};

fn finish_after_return_check(vm: &mut VmState, result: Oop) {
    assert_eq!(vm.stack.sp, 0, "process stack must be exactly restored");
    let _ = result;
}

#[test]
fn k_return_self() {
    let mut vm = common::test_vm();
    let mut b = BytecodeBuilder::new();
    b.ret_self();
    let sel = vm.universe.intern(b"m");
    let m = b.finish(&mut vm, sel, 0, 0);
    let recv = SmallInt::new(7).oop();
    let result = run_method(&mut vm, m, recv, &[]);
    assert_eq!(result, recv);
    finish_after_return_check(&mut vm, result);
}

#[test]
fn k_push_const_matrix() {
    let mut vm = common::test_vm();

    let mut b = BytecodeBuilder::new();
    b.push_nil();
    b.ret_tos();
    let sel = vm.universe.intern(b"push_nil");
    let m = b.finish(&mut vm, sel, 0, 0);
    let recv = vm.universe.nil_obj;
    let r = run_method(&mut vm, m, recv, &[]);
    assert_eq!(r, vm.universe.nil_obj);

    let mut b = BytecodeBuilder::new();
    b.push_true();
    b.ret_tos();
    let sel = vm.universe.intern(b"push_true");
    let m = b.finish(&mut vm, sel, 0, 0);
    let r = run_method(&mut vm, m, recv, &[]);
    assert_eq!(r, vm.universe.true_obj);

    let mut b = BytecodeBuilder::new();
    b.push_false();
    b.ret_tos();
    let sel = vm.universe.intern(b"push_false");
    let m = b.finish(&mut vm, sel, 0, 0);
    let r = run_method(&mut vm, m, recv, &[]);
    assert_eq!(r, vm.universe.false_obj);

    for v in [-128i8, 127i8] {
        let mut b = BytecodeBuilder::new();
        b.push_smi_i8(v);
        b.ret_tos();
        let sel = vm.universe.intern(b"push_smi");
        let m = b.finish(&mut vm, sel, 0, 0);
        let r = run_method(&mut vm, m, recv, &[]);
        assert_eq!(r, SmallInt::new(v as i64).oop());
    }

    let sym = vm.universe.intern(b"probe");
    let mut b = BytecodeBuilder::new();
    b.push_literal(&mut vm, sym.oop());
    b.ret_tos();
    let sel = vm.universe.intern(b"push_lit");
    let m = b.finish(&mut vm, sel, 0, 0);
    let r = run_method(&mut vm, m, recv, &[]);
    assert_eq!(r, sym.oop());

    assert_eq!(vm.stack.sp, 0);
}

#[test]
fn k_temps() {
    let mut vm = common::test_vm();
    for argc in 0..=2usize {
        for ntemps in 0..=2usize {
            // t := arg0 (if any args); ^t   (t is the last unified index,
            // argc+ntemps-1, so this exercises both arg-area and
            // fixed-slot addressing depending on the matrix cell).
            if argc + ntemps == 0 {
                continue; // no temp slot to exercise
            }
            let last_t = (argc + ntemps - 1) as u8;
            let mut b = BytecodeBuilder::new();
            if argc > 0 {
                b.push_temp(0); // arg0
                b.store_temp_pop(last_t);
            } else {
                b.push_nil();
                b.store_temp_pop(last_t);
            }
            b.push_temp(last_t);
            b.ret_tos();
            let sel = vm.universe.intern(b"k_temps");
            let m = b.finish(&mut vm, sel, argc, ntemps);

            let recv = vm.universe.nil_obj;
            let args: Vec<Oop> = (0..argc)
                .map(|i| SmallInt::new(100 + i as i64).oop())
                .collect();
            let result = run_method(&mut vm, m, recv, &args);
            let expected = if argc > 0 {
                SmallInt::new(100).oop()
            } else {
                vm.universe.nil_obj
            };
            assert_eq!(result, expected, "argc={argc} ntemps={ntemps}");
            assert_eq!(vm.stack.sp, 0, "argc={argc} ntemps={ntemps}");
        }
    }
}

#[test]
fn k_instvar() {
    let mut vm = common::test_vm();
    let assoc_klass = vm.universe.association_klass;
    let recv = alloc::alloc_slots(&mut vm, assoc_klass).oop();
    let value_lit = SmallInt::new(99).oop();

    let mut b = BytecodeBuilder::new();
    b.push_literal(&mut vm, value_lit);
    b.store_instvar_pop(1); // Association's #value is body index 1
    b.push_instvar(1);
    b.ret_tos();
    let sel = vm.universe.intern(b"k_instvar");
    let m = b.finish(&mut vm, sel, 0, 0);

    let result = run_method(&mut vm, m, recv, &[]);
    assert_eq!(result, value_lit);

    let mem = macvm::oops::wrappers::MemOop::try_from(recv).unwrap();
    assert_eq!(
        mem.body_oop(1),
        value_lit,
        "the write must land in the object"
    );
    assert_eq!(vm.stack.sp, 0);
}

#[test]
fn k_global() {
    let mut vm = common::test_vm();
    let assoc_klass = vm.universe.association_klass;
    let assoc = alloc::alloc_slots(&mut vm, assoc_klass);
    assoc.set_body_oop(1, SmallInt::new(5).oop());

    let mut b = BytecodeBuilder::new();
    b.push_global(&mut vm, assoc.oop());
    b.ret_tos();
    let sel = vm.universe.intern(b"k_global_read");
    let m = b.finish(&mut vm, sel, 0, 0);
    let recv = vm.universe.nil_obj;
    let r = run_method(&mut vm, m, recv, &[]);
    assert_eq!(r, SmallInt::new(5).oop());

    let mut b = BytecodeBuilder::new();
    b.push_smi_i8(9);
    b.store_global_pop(&mut vm, assoc.oop());
    b.push_global(&mut vm, assoc.oop());
    b.ret_tos();
    let sel = vm.universe.intern(b"k_global_write");
    let m = b.finish(&mut vm, sel, 0, 0);
    let r = run_method(&mut vm, m, recv, &[]);
    assert_eq!(r, SmallInt::new(9).oop());

    assert_eq!(vm.stack.sp, 0);
}

fn build_diamond(vm: &mut VmState) -> macvm::oops::wrappers::MethodOop {
    let mut b = BytecodeBuilder::new();
    let l1 = b.new_label();
    let l2 = b.new_label();
    b.push_self();
    b.br_false_fwd(l1);
    b.push_smi_i8(1);
    b.jump_fwd(l2);
    b.bind(l1);
    b.push_smi_i8(2);
    b.bind(l2);
    b.ret_tos();
    let sel = vm.universe.intern(b"diamond");
    b.finish(vm, sel, 0, 0)
}

#[test]
fn k_diamond() {
    let mut vm = common::test_vm();
    let m = build_diamond(&mut vm);
    let true_obj = vm.universe.true_obj;
    let r = run_method(&mut vm, m, true_obj, &[]);
    assert_eq!(r, SmallInt::new(1).oop());

    let m2 = build_diamond(&mut vm);
    let false_obj = vm.universe.false_obj;
    let r2 = run_method(&mut vm, m2, false_obj, &[]);
    assert_eq!(r2, SmallInt::new(2).oop());

    assert_eq!(vm.stack.sp, 0);
}

/// The pinned kernel from tests_s02.md item 7: one not-taken branch, one
/// taken back jump, one taken branch, then terminates.
#[test]
fn k_bool_loop() {
    let mut vm = common::test_vm();
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
    let sel = vm.universe.intern(b"k_bool_loop");
    let m = b.finish(&mut vm, sel, 0, 1);

    let recv = vm.universe.nil_obj;
    let result = run_method(&mut vm, m, recv, &[]);
    assert_eq!(result, SmallInt::new(42).oop());
    assert_eq!(vm.stack.sp, 0);
}

#[test]
fn k_deep_operand_stack() {
    let mut vm = common::test_vm();
    let mut b = BytecodeBuilder::new();
    for i in 0..40i64 {
        // i8 range only covers -128..127; use distinct small values.
        b.push_smi_i8((i % 100) as i8);
    }
    for _ in 0..39 {
        b.pop();
    }
    b.ret_tos();
    let sel = vm.universe.intern(b"k_deep");
    let m = b.finish(&mut vm, sel, 0, 0);
    let recv = vm.universe.nil_obj;
    let result = run_method(&mut vm, m, recv, &[]);
    // 39 pops remove the top 39 (LIFO: values 39,38,...,1), leaving the
    // FIRST-pushed item (value 0) as the sole remaining/returned value.
    assert_eq!(result, SmallInt::new(0).oop());
    assert_eq!(vm.stack.sp, 0);
}

#[test]
fn k_stack_restored_after_result() {
    let mut vm = common::test_vm();
    let mut b = BytecodeBuilder::new();
    b.ret_self();
    let sel = vm.universe.intern(b"m");
    let m = b.finish(&mut vm, sel, 0, 0);
    let recv = SmallInt::new(1).oop();
    let r1 = run_method(&mut vm, m, recv, &[]);
    assert_eq!(vm.stack.sp, 0);
    let r2 = run_method(&mut vm, m, recv, &[]);
    assert_eq!(vm.stack.sp, 0);
    assert_eq!(r1, r2);
}

// --- subprocess-observed stress tests --------------------------------------

#[test]
fn process_stack_overflow_exits_cleanly() {
    let exe = env!("CARGO_BIN_EXE_macvm");
    let output = std::process::Command::new(exe)
        .arg("--selftest-stack-overflow")
        .output()
        .expect("failed to spawn macvm binary");
    assert_eq!(output.status.code(), Some(70));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("process stack overflow"),
        "stderr: {stderr}"
    );
}

#[test]
fn trace_mode_smoke() {
    // In-process half: trace mode must not crash or alter results.
    let mut vm = VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: macvm::runtime::TraceFlags::parse("bytecode"),
        gc_stress: false,
        eden_kb: None,
    });
    let m = build_diamond(&mut vm);
    let true_obj = vm.universe.true_obj;
    let r = run_method(&mut vm, m, true_obj, &[]);
    assert_eq!(r, SmallInt::new(1).oop());
}

#[test]
fn trace_mode_line_count() {
    // Subprocess half: `--selftest-trace-diamond` runs the same k_diamond
    // true-branch path (push_self, br_false_fwd not-taken, push_smi,
    // jump_fwd taken, return_tos = 5 bytecodes) under MACVM_TRACE=bytecode
    // and prints one line per executed bytecode to stderr.
    let exe = env!("CARGO_BIN_EXE_macvm");
    let output = std::process::Command::new(exe)
        .arg("--selftest-trace-diamond")
        .output()
        .expect("failed to spawn macvm binary");
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line_count = stderr.lines().filter(|l| l.starts_with("[bc]")).count();
    assert_eq!(line_count, 5, "stderr was:\n{stderr}");
}
