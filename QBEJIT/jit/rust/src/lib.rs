//! `qbejit` — Rust port of FasterBASIC's in-process QBE-based ARM64 JIT.
//!
//! See `QBEJIT/jit/rust/README.md` for scope and provenance. Short version:
//! QBE IL text goes in (via [`compile::compile_il_jit`]), through the
//! vendored C QBE-fork optimizer/regalloc (`jit/c/`, driven over FFI — see
//! [`ffi`]), out as a flat `JitInst[]` ([`compile::JitCollectorHandle`]).
//! [`encode::encode`] turns that into a [`module::JitModule`] (code/data
//! buffers + unresolved fixups) using the ARM64 encoder in [`encoder`].
//! [`linker::link`] resolves those fixups against a [`memory::JitMemoryRegion`]
//! (the `mmap`/`MAP_JIT`/W^X-managed executable memory from [`memory`]),
//! after which the region is executable. [`patch`] adds two things the
//! FasterBASIC original doesn't have: moving-GC-safe oop updates and
//! inline-cache call-site re-patching, both for MACVM's adaptive tier.
//!
//! [`ir`] is a second, text-free entry point, alongside
//! [`compile::compile_il_jit`]: [`ir::build_function_jit`] constructs a
//! function directly (`jit/c/ir_builder.c`, driven over FFI via
//! [`ir_ffi`]) through the identical optimizer pipeline and `JitInst[]`
//! collection, no QBE IL text or `parse()` involved. From `encode::encode`
//! onward both paths converge — same `JitCollectorHandle` output type.
//!
//! Not wired into MACVM's own build (see the crate's `Cargo.toml`
//! description) — this is an evaluation crate for `docs/DESIGN.md` §5 (D6).

pub mod compile;
pub mod encode;
pub mod encoder;
pub mod ffi;
pub mod ir;
pub mod ir_ffi;
pub mod linker;
pub mod memory;
pub mod module;
pub mod patch;

pub use compile::{compile_il_jit, default_target, JitCollectorHandle, JitCompileError};
pub use ir::build_function_jit;
