//! Object memory: address-space reservation, the eden bump allocator,
//! `Universe`/genesis, symbol interning, and the heap verifier (SPEC §3,
//! §7). Survivor spaces, the old generation, and any GC arrive in S7/S8;
//! S1 commits only eden at reservation offset 0.
//!
//! `unsafe` is confined to this module tree (CONVENTIONS §1) — the
//! module-scoped opt-back-in from the crate's `#![deny(unsafe_code)]`, used
//! by `reservation.rs`'s `mmap`/`mprotect`/`munmap` FFI and by `alloc.rs`'s
//! fresh-object construction (via `oops::heap`'s safe-looking, internally
//! `unsafe`, accessors).

#![allow(unsafe_code)]

pub mod alloc;
pub mod print;
pub mod reservation;
pub mod space;
pub mod symbols;
pub mod universe;
pub mod verify;

pub use print::print_oop;
pub use symbols::SymbolTable;
pub use universe::Universe;
