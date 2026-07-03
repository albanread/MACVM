//! Raw FFI surface: mirrors `jit/c/jit_collect.h` and `jit/c/qbe_bridge.h`
//! exactly, plus `rust_qbe_protected_call` from `csrc/rust_bridge.c`.
//!
//! Nothing in this module is safe to call directly except through
//! [`crate::compile::compile_il_jit`], which takes the global compile lock
//! and routes every call through the setjmp/longjmp guard — see
//! `csrc/rust_bridge.c` for why that's mandatory, not a convenience.

use libc::{c_char, c_int, c_void};

/// `JIT_SYM_MAX` from `jit_collect.h`.
pub const JIT_SYM_MAX: usize = 80;

/// Mirrors `enum JitInstKind` (`jit_collect.h`). Only the discriminants are
/// reproduced here (as `u16` constants, not a Rust `enum`) because QBE's C
/// side is the source of truth for the numeric values and Rust `#[repr(C)]`
/// enums must be exhaustive — a `u16` field plus these constants is the
/// correct FFI shape for a C enum you don't own.
pub mod inst_kind {
    pub const JIT_LABEL: u16 = 0;
    pub const JIT_FUNC_BEGIN: u16 = 1;
    pub const JIT_FUNC_END: u16 = 2;
    pub const JIT_DBGLOC: u16 = 3;
    pub const JIT_NOP: u16 = 4;
    pub const JIT_COMMENT: u16 = 5;

    pub const JIT_ADD_RRR: u16 = 16;
    pub const JIT_SUB_RRR: u16 = 17;
    pub const JIT_MUL_RRR: u16 = 18;
    pub const JIT_SDIV_RRR: u16 = 19;
    pub const JIT_UDIV_RRR: u16 = 20;
    pub const JIT_AND_RRR: u16 = 21;
    pub const JIT_ORR_RRR: u16 = 22;
    pub const JIT_EOR_RRR: u16 = 23;
    pub const JIT_LSL_RRR: u16 = 24;
    pub const JIT_LSR_RRR: u16 = 25;
    pub const JIT_ASR_RRR: u16 = 26;
    pub const JIT_NEG_RR: u16 = 27;

    pub const JIT_MSUB_RRRR: u16 = 32;
    pub const JIT_MADD_RRRR: u16 = 33;

    pub const JIT_ADD_RRI: u16 = 48;
    pub const JIT_SUB_RRI: u16 = 49;

    pub const JIT_MOV_RR: u16 = 64;
    pub const JIT_MOVZ: u16 = 65;
    pub const JIT_MOVK: u16 = 66;
    pub const JIT_MOVN: u16 = 67;
    pub const JIT_MOV_WIDE_IMM: u16 = 68;

    pub const JIT_FADD_RRR: u16 = 80;
    pub const JIT_FSUB_RRR: u16 = 81;
    pub const JIT_FMUL_RRR: u16 = 82;
    pub const JIT_FDIV_RRR: u16 = 83;
    pub const JIT_FNEG_RR: u16 = 84;
    pub const JIT_FMOV_RR: u16 = 85;

    pub const JIT_FCVT_SD: u16 = 96;
    pub const JIT_FCVT_DS: u16 = 97;
    pub const JIT_FCVTZS: u16 = 98;
    pub const JIT_FCVTZU: u16 = 99;
    pub const JIT_SCVTF: u16 = 100;
    pub const JIT_UCVTF: u16 = 101;
    pub const JIT_FMOV_GF: u16 = 102;
    pub const JIT_FMOV_FG: u16 = 103;

    pub const JIT_SXTB: u16 = 112;
    pub const JIT_UXTB: u16 = 113;
    pub const JIT_SXTH: u16 = 114;
    pub const JIT_UXTH: u16 = 115;
    pub const JIT_SXTW: u16 = 116;
    pub const JIT_UXTW: u16 = 117;

    pub const JIT_CMP_RR: u16 = 128;
    pub const JIT_CMP_RI: u16 = 129;
    pub const JIT_CMN_RR: u16 = 130;
    pub const JIT_FCMP_RR: u16 = 131;
    pub const JIT_TST_RR: u16 = 132;

    pub const JIT_CSET: u16 = 144;
    pub const JIT_CSEL: u16 = 145;

    pub const JIT_LDR_RI: u16 = 160;
    pub const JIT_LDRB_RI: u16 = 161;
    pub const JIT_LDRH_RI: u16 = 162;
    pub const JIT_LDRSB_RI: u16 = 163;
    pub const JIT_LDRSH_RI: u16 = 164;
    pub const JIT_LDRSW_RI: u16 = 165;

    pub const JIT_STR_RI: u16 = 176;
    pub const JIT_STRB_RI: u16 = 177;
    pub const JIT_STRH_RI: u16 = 178;

    pub const JIT_LDR_RR: u16 = 192;
    pub const JIT_STR_RR: u16 = 193;
    pub const JIT_LDRB_RR: u16 = 194;
    pub const JIT_LDRH_RR: u16 = 195;
    pub const JIT_LDRSB_RR: u16 = 196;
    pub const JIT_LDRSH_RR: u16 = 197;
    pub const JIT_LDRSW_RR: u16 = 198;
    pub const JIT_STRB_RR: u16 = 199;
    pub const JIT_STRH_RR: u16 = 200;

    pub const JIT_LDP: u16 = 208;
    pub const JIT_STP: u16 = 209;
    pub const JIT_LDP_POST: u16 = 210;
    pub const JIT_STP_PRE: u16 = 211;

    pub const JIT_B: u16 = 224;
    pub const JIT_BL: u16 = 225;

    pub const JIT_B_COND: u16 = 226;

    pub const JIT_CBZ: u16 = 227;
    pub const JIT_CBNZ: u16 = 228;

    pub const JIT_BR: u16 = 232;
    pub const JIT_BLR: u16 = 233;
    pub const JIT_RET: u16 = 234;

    pub const JIT_CALL_EXT: u16 = 240;

    pub const JIT_ADRP: u16 = 248;
    pub const JIT_ADR: u16 = 249;

    pub const JIT_LOAD_ADDR: u16 = 252;

    pub const JIT_SUB_SP: u16 = 256;
    pub const JIT_ADD_SP: u16 = 257;
    pub const JIT_MOV_SP: u16 = 258;

    pub const JIT_HINT: u16 = 264;
    pub const JIT_BRK: u16 = 265;

    pub const JIT_NEON_LDR_Q: u16 = 272;
    pub const JIT_NEON_STR_Q: u16 = 273;
    pub const JIT_NEON_ADD: u16 = 274;
    pub const JIT_NEON_SUB: u16 = 275;
    pub const JIT_NEON_MUL: u16 = 276;
    pub const JIT_NEON_DIV: u16 = 277;
    pub const JIT_NEON_NEG: u16 = 278;
    pub const JIT_NEON_ABS: u16 = 279;
    pub const JIT_NEON_FMA: u16 = 280;
    pub const JIT_NEON_MIN: u16 = 281;
    pub const JIT_NEON_MAX: u16 = 282;
    pub const JIT_NEON_DUP: u16 = 283;
    pub const JIT_NEON_ADDV: u16 = 284;

    pub const JIT_ADD_SHIFT: u16 = 296;
    pub const JIT_SUB_SHIFT: u16 = 297;
    pub const JIT_AND_SHIFT: u16 = 298;
    pub const JIT_ORR_SHIFT: u16 = 299;
    pub const JIT_EOR_SHIFT: u16 = 300;

    pub const JIT_DATA_START: u16 = 320;
    pub const JIT_DATA_END: u16 = 321;
    pub const JIT_DATA_BYTE: u16 = 322;
    pub const JIT_DATA_HALF: u16 = 323;
    pub const JIT_DATA_WORD: u16 = 324;
    pub const JIT_DATA_QUAD: u16 = 325;
    pub const JIT_DATA_ZERO: u16 = 326;
    pub const JIT_DATA_SYMREF: u16 = 327;
    pub const JIT_DATA_ASCII: u16 = 328;
    pub const JIT_DATA_ALIGN: u16 = 329;
}

/// Mirrors `enum JitCond` (ARM64 4-bit condition field encoding).
pub mod cond {
    pub const EQ: u8 = 0x0;
    pub const NE: u8 = 0x1;
    pub const CS: u8 = 0x2;
    pub const CC: u8 = 0x3;
    pub const MI: u8 = 0x4;
    pub const PL: u8 = 0x5;
    pub const VS: u8 = 0x6;
    pub const VC: u8 = 0x7;
    pub const HI: u8 = 0x8;
    pub const LS: u8 = 0x9;
    pub const GE: u8 = 0xA;
    pub const LT: u8 = 0xB;
    pub const GT: u8 = 0xC;
    pub const LE: u8 = 0xD;
    pub const AL: u8 = 0xE;
    pub const NV: u8 = 0xF;
}

/// Mirrors `enum JitCls`.
pub mod cls {
    pub const W: u8 = 0;
    pub const L: u8 = 1;
    pub const S: u8 = 2;
    pub const D: u8 = 3;
}

/// Mirrors `enum JitShift`.
pub mod shift {
    pub const LSL: u8 = 0;
    pub const LSR: u8 = 1;
    pub const ASR: u8 = 2;
    pub const ROR: u8 = 3;
}

/// Mirrors `enum JitSymType`.
pub mod sym_type {
    pub const NONE: u8 = 0;
    pub const GLOBAL: u8 = 1;
    pub const THREAD_LOCAL: u8 = 2;
    pub const DATA: u8 = 3;
    pub const FUNC: u8 = 4;
}

/// `JIT_REG_*` sentinels (`jit_collect.h`) — negative register-id values
/// with special meaning instead of a physical GPR index.
pub mod reg_sentinel {
    pub const NONE: i32 = -1;
    pub const SP: i32 = -2;
    pub const FP: i32 = -3;
    pub const LR: i32 = -4;
    pub const IP0: i32 = -5;
    pub const IP1: i32 = -6;
    /// NEON v-regs are encoded as `VREG_BASE - qbe_vreg_id`; a register id
    /// `r < VREG_BASE` decodes to NEON register `VREG_BASE - r`.
    pub const VREG_BASE: i32 = -100;
}

/// Mirrors `struct JitInst` (`jit_collect.h`) byte-for-byte. QBE's own
/// comment targets 128 bytes; we don't assert that here (alignment can
/// legitimately vary by field order changes upstream) — `bindgen`-style
/// exactness on the fields is what actually matters, not the total size.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JitInst {
    pub kind: u16,
    pub cls: u8,
    pub cond: u8,
    pub shift_type: u8,
    pub sym_type: u8,
    pub is_float: u8,
    _pad1: u8,

    pub rd: i32,
    pub rn: i32,
    pub rm: i32,
    pub ra: i32,

    pub imm: i64,
    pub imm2: i64,

    pub target_id: i32,
    _pad2: i32,

    pub sym_name: [c_char; JIT_SYM_MAX],
}

impl JitInst {
    pub fn kind(&self) -> u16 {
        self.kind
    }

    /// `true` for `JIT_CLS_L` (64-bit integer) — mirrors `JitInst.is64()`
    /// in `jit_encode.zig`.
    pub fn is64(&self) -> bool {
        self.cls == cls::L
    }

    /// `true` for `JIT_CLS_S`/`JIT_CLS_D` (either float width) — mirrors
    /// `JitInst.isFloat()`. Note this is a `cls`-based float check, not
    /// the same thing as the `is_float` field (which flags a *variant* of
    /// certain int/float conversion opcodes) — the Zig source keeps both
    /// named similarly; don't conflate them when porting call sites.
    pub fn is_float_cls(&self) -> bool {
        self.cls >= cls::S
    }

    /// `true` for `JIT_CLS_D` — mirrors `JitInst.isDouble()`.
    pub fn is_double(&self) -> bool {
        self.cls == cls::D
    }

    /// Raw bytes of `sym_name` up to `len` (caller-supplied, e.g. from
    /// `imm` on `JIT_DATA_ASCII` — that directive's payload can contain
    /// embedded NULs, so it is read by explicit length, not NUL-scanned).
    /// `len` is clamped to `JIT_SYM_MAX`.
    pub fn sym_name_bytes(&self, len: usize) -> &[u8] {
        let len = len.min(JIT_SYM_MAX);
        // SAFETY: sym_name is a JIT_SYM_MAX-byte buffer inline in this
        // struct; any length <= JIT_SYM_MAX is in-bounds. c_char is i8 on
        // aarch64 — the cast to u8 is a bit-preserving reinterpret,
        // matching how C treats `char*` as bytes.
        unsafe { std::slice::from_raw_parts(self.sym_name.as_ptr() as *const u8, len) }
    }

    /// Reads `sym_name` as a NUL-terminated `&str` (the common case: an
    /// external symbol or data-label identifier, always ASCII in
    /// practice). For `JIT_DATA_ASCII`'s raw/NUL-containing payload, use
    /// [`Self::sym_name_bytes`] instead.
    pub fn sym_name_str(&self) -> &str {
        let bytes = self.sym_name_bytes(JIT_SYM_MAX);
        let len = bytes.iter().position(|&b| b == 0).unwrap_or(JIT_SYM_MAX);
        std::str::from_utf8(&bytes[..len]).unwrap_or("")
    }
}

/// Mirrors `struct JitCollector` (`jit_collect.h`).
#[repr(C)]
pub struct JitCollector {
    pub insts: *mut JitInst,
    pub ninst: u32,
    pub inst_cap: u32,
    pub nfunc: u32,
    pub ndata: u32,
    pub error: c_int,
    pub error_msg: [c_char; 256],
}

impl JitCollector {
    /// A zeroed collector, valid to pass to `jit_collector_init`. Not valid
    /// to use for anything else until `jit_collector_init` has run — its
    /// `insts` pointer is null.
    pub fn zeroed() -> Self {
        // SAFETY: all-zero is a valid bit pattern for every field (null
        // pointer, zero counters, zero error code, zero-filled char array).
        unsafe { std::mem::zeroed() }
    }
}

/// Sentinel `rust_qbe_protected_call` returns when the wrapped call
/// aborted via `basic_exit()` (fatal QBE parse error) instead of
/// returning normally. Matches `RUST_QBE_LONGJMP_SENTINEL` in
/// `csrc/rust_bridge.c`.
pub const RUST_QBE_LONGJMP_SENTINEL: c_int = -12345;

/// `QBE_OK` / `QBE_ERR_*` from `qbe_bridge.h`.
pub mod qbe_status {
    pub const OK: i32 = 0;
    pub const ERR_OUTPUT: i32 = -1;
    pub const ERR_INPUT: i32 = -2;
    pub const ERR_TARGET: i32 = -3;
    pub const ERR_PARSE: i32 = -4;
}

extern "C" {
    // ── jit_collect.h ────────────────────────────────────────────────
    pub fn jit_collector_init(jc: *mut JitCollector) -> c_int;
    pub fn jit_collector_free(jc: *mut JitCollector);
    pub fn jit_collector_reset(jc: *mut JitCollector);
    pub fn jit_inst_kind_name(kind: u16) -> *const c_char;

    // ── qbe_bridge.h / jit_collect.h JIT entry point ───────────────────
    // NEVER call this directly — always through rust_qbe_protected_call.
    // See csrc/rust_bridge.c's module doc for why.
    pub fn qbe_compile_il_jit(
        il_text: *const c_char,
        il_len: usize,
        jc: *mut JitCollector,
        target_name: *const c_char,
    ) -> c_int;
    pub fn qbe_jit_cleanup();
    pub fn qbe_default_target() -> *const c_char;

    // ── csrc/rust_bridge.c ───────────────────────────────────────────
    pub fn rust_qbe_protected_call(
        compile_fn: extern "C" fn(*mut c_void) -> c_int,
        ctx: *mut c_void,
    ) -> c_int;
}
