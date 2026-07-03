//! `JitModule`: the output of [`crate::encode::encode`] and the input
//! [`crate::linker::link`] patches in place. Rust port of the essential,
//! non-debug-only fields of `JitModule` in FasterBASIC's `jit_encode.zig`
//! (`pub const JitModule = struct { ... }`).
//!
//! Deliberately dropped versus the Zig original: `source_map`,
//! `comment_map`, and `diagnostics` (BASIC-line and codegen-comment
//! annotations for human-readable disassembly/crash reports — a debugging
//! convenience, not part of the codegen/linking contract) and
//! `EncodeStats`'s per-category counters beyond a plain instruction count.
//! None of that is needed to go from `JitInst[]` to executable code; add it
//! back if MACVM's own diagnostics need it.

use crate::encoder::BranchClass;
use std::collections::HashMap;

/// Naming convention a MACVM-side QBE-IL codegen would use to mark a
/// `data` definition as a GC-managed oop slot — e.g. `data $__macvm_oop$42
/// = { l 0 }`, then load it in function bodies the normal QBE way
/// (`%v =l loadl $__macvm_oop$42`). No change to `jit/c` (the vendored QBE
/// fork) is needed for this: QBE already carries whatever `data $name`
/// identifier the frontend chose all the way through to the symbol table
/// [`encode`](crate::encode) builds — this prefix is purely a convention
/// recognized on the Rust side. See `crate::patch`'s module doc for the
/// full design rationale (route oops through a data slot, not an embedded
/// code immediate, so a GC move is a plain memory write).
pub const OOP_SYMBOL_PREFIX: &str = "__macvm_oop$";

/// Naming convention for an inline-cache-patchable call site — e.g. `call
/// $__macvm_ic$42(...)`. Same mechanism as `OOP_SYMBOL_PREFIX`: QBE just
/// carries the symbol name, this crate recognizes the prefix.
pub const IC_SITE_PREFIX: &str = "__macvm_ic$";

/// QBE's arm64 backend ABI-prefixes *some* global symbol references with a
/// leading `_` — confirmed empirically: a QBE IL `call $foo(...)`
/// (`JIT_CALL_EXT`) shows up in `JitInst.sym_name` as `"_foo"`, but a
/// `data $foo = {...}` definition (`JIT_DATA_START`) shows up as `"foo"`,
/// unprefixed. So this can't unconditionally strip one leading underscore
/// — this crate's own chosen prefixes (`OOP_SYMBOL_PREFIX`,
/// `IC_SITE_PREFIX`) themselves start with underscores, so a name that
/// already matches on its own (the `DATA_START` case) would have its
/// leading underscore wrongly eaten by an unconditional strip, breaking
/// the match. Instead: try the raw name first, and only fall back to a
/// one-underscore-stripped form if the raw name doesn't already match a
/// known convention (the `CALL_EXT` case).
fn canonicalize_symbol_name(name: &str) -> &str {
    let stripped = name.strip_prefix('_').unwrap_or(name);
    for candidate in [name, stripped] {
        if candidate.starts_with(OOP_SYMBOL_PREFIX) || candidate.starts_with(IC_SITE_PREFIX) {
            return candidate;
        }
    }
    name
}

pub fn is_oop_symbol(name: &str) -> bool {
    canonicalize_symbol_name(name).starts_with(OOP_SYMBOL_PREFIX)
}
pub fn is_ic_site_symbol(name: &str) -> bool {
    canonicalize_symbol_name(name).starts_with(IC_SITE_PREFIX)
}

/// The logical name to key `oop_slots`/`ic_sites` by — see
/// [`canonicalize_symbol_name`].
pub fn canonical_symbol_key(name: &str) -> String {
    canonicalize_symbol_name(name).to_string()
}

/// A forward (or as-yet-unresolved) branch: the instruction word at
/// `code_offset` was emitted with a zero/placeholder offset field and
/// needs patching once `target_id`'s label offset is known.
/// Mirrors `BranchFixup` (`jit_encode.zig`).
#[derive(Debug, Clone, Copy)]
pub struct BranchFixup {
    pub code_offset: u32,
    pub target_id: i32,
    pub branch_class: BranchClass,
    pub inst_index: u32,
    /// The instruction word already written, with the offset field still
    /// zero — resolution ORs the computed offset into this and rewrites
    /// the word at `code_offset`.
    pub base_opcode: u32,
}

/// A `BL`/external-call site that needs a trampoline stub.
/// Mirrors `ExtCallEntry`.
#[derive(Debug, Clone)]
pub struct ExtCallEntry {
    pub code_offset: u32,
    pub sym_name: String,
    pub inst_index: u32,
}

/// A `JIT_LOAD_ADDR`/`JIT_ADRP`+`ADD` pair that needs the real ADRP
/// page-delta/lo12 patched in once code+data addresses are known.
/// Mirrors `LoadAddrReloc`.
#[derive(Debug, Clone)]
pub struct LoadAddrReloc {
    pub adrp_offset: u32,
    pub sym_name: String,
    pub inst_index: u32,
    pub addend: i64,
}

/// A `.quad symbol+addend` slot inside the data section that needs the
/// real resolved address written in. Mirrors `DataSymRef`.
#[derive(Debug, Clone)]
pub struct DataSymRef {
    pub data_offset: u32,
    pub sym_name: String,
    pub addend: i64,
    pub inst_index: u32,
}

/// Mirrors `JitSymType` (`jit_collect.h`), reproduced here (rather than
/// reused from `crate::ffi::sym_type`, which is untyped `u8` constants)
/// so `SymbolEntry` gets a real enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitSymType {
    None,
    Global,
    ThreadLocal,
    Data,
    Func,
}

impl JitSymType {
    pub fn from_raw(v: u8) -> Self {
        match v {
            crate::ffi::sym_type::GLOBAL => Self::Global,
            crate::ffi::sym_type::THREAD_LOCAL => Self::ThreadLocal,
            crate::ffi::sym_type::DATA => Self::Data,
            crate::ffi::sym_type::FUNC => Self::Func,
            _ => Self::None,
        }
    }
}

/// Mirrors `SymbolEntry`.
#[derive(Debug, Clone, Copy)]
pub struct SymbolEntry {
    pub offset: u32,
    pub is_code: bool,
    pub sym_type: JitSymType,
}

/// The output of encoding a `JitInst[]` stream: raw code/data buffers plus
/// every fixup that still needs resolving before the buffers are
/// executable. Mirrors the non-debug fields of `JitModule`.
#[derive(Default)]
pub struct JitModule {
    pub code: Vec<u8>,
    pub data: Vec<u8>,

    /// Block id → code byte offset, populated as `JIT_LABEL` instructions
    /// are encountered.
    pub labels: HashMap<i32, u32>,

    pub fixups: Vec<BranchFixup>,
    pub ext_calls: Vec<ExtCallEntry>,
    pub load_addr_relocs: Vec<LoadAddrReloc>,
    pub data_sym_refs: Vec<DataSymRef>,
    pub symbols: HashMap<String, SymbolEntry>,

    /// `data` symbols matching [`OOP_SYMBOL_PREFIX`] → their data-section
    /// byte offset. Populated as [`crate::encode::encode`] commits each
    /// `JIT_DATA_START` symbol (see `EncodeState::commit_pending_data_sym`
    /// in `encode.rs`). Used by [`crate::patch::update_oop_slot`].
    pub oop_slots: HashMap<String, u32>,

    /// `CALL_EXT` symbols matching [`IC_SITE_PREFIX`] → the code-section
    /// byte offset of their (post-link) `BL` instruction. Populated
    /// alongside `ext_calls` as `JIT_CALL_EXT` instructions are encoded.
    /// Used by [`crate::patch::repatch_ic_site`].
    pub ic_sites: HashMap<String, u32>,

    pub instructions_emitted: u32,

    /// Problems encountered while encoding (bad register ids, out-of-range
    /// offsets, unresolved branch labels, ...). Mirrors the *content* of
    /// Zig's severity-tagged `diagnostics` list, minus the severity
    /// tagging and dump-report formatting (dropped per this crate's scope
    /// — see README.md). Non-empty means the code buffer should not be
    /// trusted; [`crate::encode::encode`] does not fail loudly on its own
    /// the way the Zig source doesn't either — callers must check this.
    pub errors: Vec<String>,
}

impl JitModule {
    pub fn with_capacity(code_capacity: usize, data_capacity: usize) -> Self {
        Self {
            code: Vec::with_capacity(code_capacity),
            data: Vec::with_capacity(data_capacity),
            ..Default::default()
        }
    }

    /// Appends one 32-bit instruction word (little-endian, matching ARM64
    /// and this crate's target byte order) and returns its byte offset.
    pub fn push_code_word(&mut self, word: u32) -> u32 {
        let offset = self.code.len() as u32;
        self.code.extend_from_slice(&word.to_le_bytes());
        offset
    }

    pub fn code_offset(&self) -> u32 {
        self.code.len() as u32
    }

    pub fn data_offset(&self) -> u32 {
        self.data.len() as u32
    }
}
