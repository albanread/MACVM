//! Shared test scaffolding (tests_s02.md §Unit tests scaffolding note).
//! `test_vm()` never uses `std::env::set_var` — the test runner is
//! multi-threaded, and env mutation across parallel tests races. READING
//! the environment is race-free, though, which is what lets the S7 gate's
//! `MACVM_GC_STRESS=1 cargo test` reach every in-process test VM here.

use macvm::runtime::{VmOptions, VmState};

#[allow(dead_code)] // not every integration test file uses every helper here
pub fn test_vm() -> VmState {
    // Starts from VmOptions::from_env() rather than hand-parsing
    // MACVM_GC_STRESS here: that's what lets `MACVM_GC_STRESS=1` AND
    // `MACVM_GC_STRESS=full[:N]` both reach every in-process test VM (S8
    // step 8), and keeps this helper automatically correct for any future
    // VmOptions field without needing a matching hand-rolled parse here.
    // heap_mib/eden_kb are still fixed at test-friendly values regardless
    // of MACVM_HEAP/MACVM_EDEN in the ambient environment.
    VmState::with_options(VmOptions {
        heap_mib: 64,
        eden_kb: None,
        ..VmOptions::from_env()
    })
}

/// Post-run walker (S4, tests_s04.md's golden epilogue helper): a fully
/// unwound `VmState` — no residual frames, markers, or tokens — has its
/// operand stack truncated all the way back to empty.
#[allow(dead_code)]
pub fn stack_clean(vm: &VmState) -> bool {
    vm.stack.sp == 0
}
