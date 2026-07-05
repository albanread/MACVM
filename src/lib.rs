//! MACVM — a research virtual machine in the Self → Strongtalk lineage,
//! implemented in Rust and targeting macOS on Apple Silicon (arm64).
//!
//! See `docs/SPEC.md` for the full engineering specification and
//! `docs/DESIGN.md` for the high-level architecture. The native code
//! generator is deliberately abstract (see [`compiler::assembler`]) so the
//! backend choice — JASM AArch64 encoder, LLVM, or interpreter-first — can be
//! made later without disturbing the rest of the VM.
//!
//! `unsafe` is confined to a small set of modules (object memory, codegen);
//! everywhere else it is denied at the crate root (CONVENTIONS §1).

#![deny(unsafe_code)]

pub mod bytecode; // opcode set, CompiledMethod, builder, disassembler
pub mod codecache; // nmethod code cache: CodeCache, CodeHandle, JitWriteGuard
pub mod compiler; // adaptive optimizing compiler + abstract codegen
pub mod frontend; // lexer, parser, AST, capture analysis, codegen, class loader
pub mod interpreter; // baseline threaded-code interpreter
pub mod memory; // object memory, allocation, garbage collection
pub mod oops; // object references, tagging, 2-word headers, classes
pub mod runtime; // stacks, activation frames, method lookup, inline caches, primitives
pub mod rusttcl; // live VM-introspection shell (disasm/methods/nmethods/ic/stats/trace), built on vendored rust-tcl
pub mod utils; // shared utilities
pub mod vendor; // vendored third-party source (S9: JASM's wfasm AArch64 encoder; rust-tcl)
