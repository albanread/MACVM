//! Adaptive optimizing compiler.
//!
//! Recompiles hot methods using type feedback gathered by the inline caches in
//! [`crate::interpreter::ic`], and emits native code through the backend-neutral
//! [`assembler::Assembler`] trait. The concrete backend is an OPEN DECISION
//! (see `docs/DESIGN.md` §4).

pub mod assembler;
