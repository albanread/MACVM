//! Runtime support: the `VmState` god-struct, environment options, method
//! lookup, primitives, and (later) activation stacks and deoptimization
//! (SPEC §3, §6, §10). No `unsafe` here — everything above `oops`/`memory`
//! stays safe Rust (CONVENTIONS §1).

pub mod error;
pub mod lookup;
pub mod primitives;
pub mod vm_state;

pub use vm_state::{InterpRegs, TraceFlags, VmOptions, VmState};
