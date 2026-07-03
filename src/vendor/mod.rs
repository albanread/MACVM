//! Vendored third-party source, kept as close to upstream as the crate
//! boundary allows (CONVENTIONS §1: this tree and `src/codecache/` are the
//! `unsafe`-permitted modules besides `oops`/`memory`). See each vendored
//! module's own `VENDOR.md`/provenance headers for exact commit and diff.
//!
//! The module-scoped opt-back-in from the crate's `#![deny(unsafe_code)]`
//! (`oops`/`memory` do the same) — `wfasm::native_macos` mmaps and toggles
//! `MAP_JIT` write-protection directly.
#![allow(unsafe_code)]

pub mod wfasm;
