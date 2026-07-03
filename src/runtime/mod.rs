//! Runtime support: the `VmState` god-struct, environment options, method
//! lookup, primitives, and (later) activation stacks and deoptimization
//! (SPEC §3, §6, §10). No `unsafe` here — everything above `oops`/`memory`
//! stays safe Rust (CONVENTIONS §1).

pub mod error;
pub mod globals;
pub mod lookup;
pub mod primitives;
pub mod vm_state;

pub use vm_state::{InterpRegs, JitMode, TraceFlags, VmOptions, VmRegBlock, VmState};

/// A guest-execution failure (SPEC §3.2 bootstrap, `sprint_s05_detail.md`
/// §Interfaces for later sprints). No Rust-level failure path exists yet
/// for a running method (DNU/errors print-and-exit, SPEC §6.3) — this type
/// exists so the frontend/world loader's call shape is stable once real
/// exception handling lands.
#[derive(Debug)]
pub struct VmError {
    pub msg: String,
}

/// `frontend/`'s single entry point into `interpreter/` (layer boundary,
/// `sprint_s05_detail.md` §Layer boundaries) — runs a compiled `#doIt`
/// (nil receiver, no args) to completion.
pub fn execute_doit(
    vm: &mut VmState,
    m: crate::oops::wrappers::MethodOop,
) -> Result<crate::oops::Oop, VmError> {
    let nil = vm.universe.nil_obj;
    Ok(crate::interpreter::run_method(vm, m, nil, &[]))
}

/// Sends `selector` (unary) to `receiver` iff it is understood; a miss is a
/// silent no-op (used only for the world-load bootstrap's `Smalltalk
/// startUp` send, SPEC §3.2 step 3 — a real send whose absence in an
/// early/incomplete world must never abort the load).
pub fn send_unary_if_understood(
    vm: &mut VmState,
    receiver: crate::oops::Oop,
    selector: crate::oops::wrappers::SymbolOop,
) -> Option<crate::oops::Oop> {
    let klass = lookup::klass_of(vm, receiver);
    let m = lookup::lookup(vm, klass, selector)?;
    Some(crate::interpreter::run_method(vm, m, receiver, &[]))
}
