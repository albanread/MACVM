//! MACVM entry point (placeholder).
//!
//! The VM is at the scaffold stage; this just proves the crate builds and
//! links. `--selftest-alloc-loop` is a hidden test hook (used by
//! `tests/it_memory.rs::eden_exhaustion_aborts`): it boots a `VmState` from
//! the environment (so the test controls heap size via `MACVM_HEAP`) and
//! allocates in a loop until `memory::alloc`'s exhaustion branch exits the
//! process with code 70 — pinning the S1 slow-path contract that S7's
//! scavenger replaces.

use macvm::memory::alloc;
use macvm::runtime::VmState;

fn main() {
    if std::env::args().any(|a| a == "--selftest-alloc-loop") {
        selftest_alloc_loop();
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
