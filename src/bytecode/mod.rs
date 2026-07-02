//! The MACVM bytecode: opcode set, `CompiledMethod` accessors, the
//! `BytecodeBuilder` test/codegen assembler, and the disassembler (SPEC
//! §4). No `unsafe` — everything here manipulates the heap only through
//! `oops` wrappers and `memory::alloc`.
//!
//! Layer rule: `bytecode/` may use `oops`, `memory::alloc`, `Universe`
//! (for `intern`/`print_oop`); it must not know about frames or
//! `ProcessStack` (that's `interpreter/`'s job).

pub mod builder;
pub mod disasm;
pub mod method;
pub mod opcode;

pub use builder::{build_standalone_block, BytecodeBuilder};
pub use disasm::disassemble;
pub use opcode::{decode_at, Instr};
