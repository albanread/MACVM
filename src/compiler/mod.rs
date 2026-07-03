//! Adaptive optimizing compiler.
//!
//! Recompiles hot methods using type feedback gathered by the inline caches in
//! [`crate::interpreter::ic`], and emits native code through the backend-neutral
//! [`assembler::Assembler`] trait, over JASM's vendored AArch64 encoder
//! (`docs/DESIGN.md` §5 D6; `docs/sprints/sprint_s09_detail.md`).
//! [`jasm_assembler::JasmAssembler`] is the only implementor.

pub mod assembler;
pub mod jasm_assembler;
