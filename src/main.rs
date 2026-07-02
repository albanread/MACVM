//! MACVM entry point (placeholder).
//!
//! The VM is at the scaffold stage; this just proves the crate builds and
//! links. Hidden test hooks observed via a real subprocess (integration
//! tests can't otherwise see this process's own exit code or exhaustive
//! stderr output): `--selftest-alloc-loop` allocates until eden is
//! exhausted (`tests/it_memory.rs::eden_exhaustion_aborts`);
//! `--selftest-stack-overflow` pushes until the process stack is exhausted
//! (`tests/it_interp.rs::process_stack_overflow_exits_cleanly`);
//! `--selftest-trace-diamond` runs the k_diamond kernel under
//! `MACVM_TRACE=bytecode` so the caller can count emitted trace lines
//! (`tests/it_interp.rs::trace_mode_line_count`).

use macvm::bytecode::BytecodeBuilder;
use macvm::memory::alloc;
use macvm::oops::smi::SmallInt;
use macvm::runtime::{VmOptions, VmState};

fn main() {
    if std::env::args().any(|a| a == "--selftest-alloc-loop") {
        selftest_alloc_loop();
    }
    if std::env::args().any(|a| a == "--selftest-stack-overflow") {
        selftest_stack_overflow();
    }
    if std::env::args().any(|a| a == "--selftest-trace-diamond") {
        selftest_trace_diamond();
    }
    println!("MACVM — Self/Strongtalk-lineage research VM (arm64). Scaffold only.");
}

fn selftest_alloc_loop() -> ! {
    let mut vm = VmState::new();
    let klass = vm.universe.array_klass;
    loop {
        let _ = alloc::alloc_indexable_oops(&mut vm, klass, 1000);
    }
}

fn selftest_stack_overflow() -> ! {
    let mut vm = VmState::new();
    let v = SmallInt::new(0).oop();
    loop {
        vm.stack.push(v);
    }
}

fn selftest_trace_diamond() -> ! {
    let mut vm = VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: macvm::runtime::TraceFlags::parse("bytecode"),
    });
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
    let m = b.finish(&mut vm, sel, 0, 0);
    let true_obj = vm.universe.true_obj;
    let _ = macvm::interpreter::run_method(&mut vm, m, true_obj, &[]);
    std::process::exit(0)
}
