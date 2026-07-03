//! Adaptive optimizing compiler.
//!
//! Recompiles hot methods using type feedback gathered by the inline caches in
//! [`crate::interpreter::ic`], and emits native code through the backend-neutral
//! [`assembler::Assembler`] trait, over JASM's vendored AArch64 encoder
//! (`docs/DESIGN.md` §5 D6; `docs/sprints/sprint_s09_detail.md`).
//! [`jasm_assembler::JasmAssembler`] is the only implementor.
//!
//! Tier 1 pipeline (S10, `docs/sprints/sprint_s10_detail.md`): bytecode ->
//! [`decode::Cfg`] -> SSA-lite `Ir` -> linear-scan regalloc -> emit.

pub mod assembler;
pub mod decode;
pub mod ir;
pub mod jasm_assembler;
pub mod regalloc;
