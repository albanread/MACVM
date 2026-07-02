//! Object memory: address-space reservation, the `[eden][from][to][old…]`
//! heap layout, `Universe`/genesis, symbol interning, and the heap verifier
//! (SPEC §3, §7). The scavenging collector itself (cards, offsets, the
//! Cheney copier, handles) lands across the rest of S7; a full GC (old-gen
//! compaction) is S8.
//!
//! `unsafe` is confined to this module tree (CONVENTIONS §1) — the
//! module-scoped opt-back-in from the crate's `#![deny(unsafe_code)]`, used
//! by `reservation.rs`'s `mmap`/`mprotect`/`munmap` FFI and by `alloc.rs`'s
//! fresh-object construction (via `oops::heap`'s safe-looking, internally
//! `unsafe`, accessors).

#![allow(unsafe_code)]

pub mod alloc;
pub mod cards;
pub mod fullgc;
pub mod handles;
pub mod layout;
pub mod offsets;
pub mod print;
pub mod reservation;
pub mod roots;
pub mod scavenge;
pub mod spaces;
pub mod stall;
pub mod stats;
pub mod store;
pub mod symbols;
pub mod universe;
pub mod verify;

pub use print::print_oop;
pub use symbols::SymbolTable;
pub use universe::Universe;
