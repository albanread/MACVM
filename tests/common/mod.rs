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
