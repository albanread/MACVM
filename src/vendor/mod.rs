//! Vendored third-party source, kept as close to upstream as the crate
//! boundary allows (CONVENTIONS §1: this tree and `src/codecache/` are the
//! `unsafe`-permitted modules besides `oops`/`memory`). See each vendored
//! module's own `VENDOR.md`/provenance headers for exact commit and diff.
//!
//! The module-scoped opt-back-in from the crate's `#![deny(unsafe_code)]`
//! (`oops`/`memory` do the same) — `wfasm::native_macos` mmaps and toggles
//! `MAP_JIT` write-protection directly. `rust_tcl` itself has no unsafe
//! code (its own `VENDOR.md` notes this); it sits under the same allow
//! only because it's vendored source, not because it needs the opt-out.
#![allow(unsafe_code)]

pub mod rust_tcl;
pub mod wfasm;
