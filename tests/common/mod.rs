//! Shared test scaffolding (tests_s02.md §Unit tests scaffolding note).
//! `test_vm()` never uses `std::env::set_var` — the test runner is
//! multi-threaded, and env mutation across parallel tests races.

use macvm::runtime::{VmOptions, VmState};

#[allow(dead_code)] // not every integration test file uses every helper here
pub fn test_vm() -> VmState {
    VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: Default::default(),
    })
}

/// Post-run walker (S4, tests_s04.md's golden epilogue helper): a fully
/// unwound `VmState` — no residual frames, markers, or tokens — has its
/// operand stack truncated all the way back to empty.
#[allow(dead_code)]
pub fn stack_clean(vm: &VmState) -> bool {
    vm.stack.sp == 0
}
