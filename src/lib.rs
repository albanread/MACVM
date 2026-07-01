//! MACVM — a research virtual machine in the Self → Strongtalk lineage,
//! implemented in Rust and targeting macOS on Apple Silicon (arm64).
//!
//! See `docs/DESIGN.md` for the architecture and open decisions. The native
//! code generator is deliberately abstract (see [`compiler::assembler`]) so the
//! backend choice — JASM AArch64 encoder, LLVM, or interpreter-first — can be
//! made later without disturbing the rest of the VM.

pub mod oops; // object references, tagging, 2-word headers, classes
pub mod memory; // object memory, allocation, garbage collection
pub mod lookup; // method lookup, inline caches, PICs, type feedback
pub mod interpreter; // baseline threaded-code interpreter
pub mod compiler; // adaptive optimizing compiler + abstract codegen
pub mod runtime; // stacks, activation frames, deoptimization
pub mod utils; // shared utilities
