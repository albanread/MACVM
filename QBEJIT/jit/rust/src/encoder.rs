//! ARM64 instruction encoder: pure functions from register/immediate
//! operands to a raw `u32` machine-code word. Rust port of the whitelisted
//! subset of FasterBASIC's `arm64_encoder.zig` (a standalone, dependency-free
//! Zig module derived from `ARM64Encoder.h`, `ARM64NeonEncoder.h`,
//! `EncoderMD.h`/`.cpp`, `ARM64LogicalImmediates.h`) needed by this crate's
//! encode-dispatch layer.
//!
//! This is a bit-for-bit port, not a redesign: the Zig source's test
//! blocks assert each `emit*` function's returned `u32` against values
//! independently verified against clang/llvm-mc, so those constants are
//! the correctness oracle here too — see the `tests` module at the bottom,
//! which reproduces a representative subset of them with the identical
//! expected values. Bit manipulation is translated mechanically rather
//! than "improved"; a single wrong bit produces a machine instruction
//! that silently does the wrong thing at runtime.
//!
//! Deliberately not ported (out of scope for this crate's dispatch layer,
//! not used transitively by anything below): the majority of the Zig
//! file's ~340 `emit*`/helper functions — only the whitelisted subset
//! actually called by the encode-dispatch layer, plus the support types
//! and internal helpers they need, are here. See each section for what
//! was skipped and why.
//!
//! `std`-only, pure-function module: every `emit_*` function takes
//! operands and returns `u32` (or `Option<u32>` where the Zig original
//! returns `?u32` — offset-out-of-encodable-range failure). No internal
//! buffer, no side effects, no `unsafe`. The caller manages the code
//! buffer (see `crate::module::JitModule::push_code_word`).

// ============================================================================
// Section: Core enums
// ============================================================================

/// A general-purpose ARM64 register (`Register`, `arm64_encoder.zig`).
/// `Sp` and `Zr` share hardware encoding 31 — see [`Register::ZR`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Register {
    R0 = 0,
    R1 = 1,
    R2 = 2,
    R3 = 3,
    R4 = 4,
    R5 = 5,
    R6 = 6,
    R7 = 7,
    R8 = 8,
    R9 = 9,
    R10 = 10,
    R11 = 11,
    R12 = 12,
    R13 = 13,
    R14 = 14,
    R15 = 15,
    R16 = 16,
    R17 = 17,
    R18 = 18,
    R19 = 19,
    R20 = 20,
    R21 = 21,
    R22 = 22,
    R23 = 23,
    R24 = 24,
    R25 = 25,
    R26 = 26,
    R27 = 27,
    R28 = 28,
    Fp = 29,
    Lr = 30,
    Sp = 31,
}

impl Register {
    /// The zero register. Shares hardware encoding 31 with `Sp` — ARM64
    /// disambiguates by instruction class (load/store base vs. most other
    /// operands), exactly as Zig's `Register.ZR: Register = .SP` const alias
    /// does.
    pub const ZR: Register = Register::Sp;

    pub fn raw(self) -> u8 {
        self as u8
    }
}

/// A NEON/FP register (`NeonRegister`, `arm64_encoder.zig`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NeonRegister {
    V0 = 0,
    V1 = 1,
    V2 = 2,
    V3 = 3,
    V4 = 4,
    V5 = 5,
    V6 = 6,
    V7 = 7,
    V8 = 8,
    V9 = 9,
    V10 = 10,
    V11 = 11,
    V12 = 12,
    V13 = 13,
    V14 = 14,
    V15 = 15,
    V16 = 16,
    V17 = 17,
    V18 = 18,
    V19 = 19,
    V20 = 20,
    V21 = 21,
    V22 = 22,
    V23 = 23,
    V24 = 24,
    V25 = 25,
    V26 = 26,
    V27 = 27,
    V28 = 28,
    V29 = 29,
    V30 = 30,
    V31 = 31,
}

impl NeonRegister {
    pub fn raw(self) -> u8 {
        self as u8
    }

    /// Reinterpret a general-purpose register's raw encoding as a
    /// `NeonRegister` of the same numeric index. Mirrors Zig's
    /// `@enumFromInt(src.raw())` calls in `emitNeonDup`, `emitNeonIns`,
    /// `emitNeonSmov`/`emitNeonSmov64`, `emitNeonUmov`/`emitNeonUmov64` —
    /// those instructions encode a GPR operand in a NEON register field.
    fn from_gpr(reg: Register) -> NeonRegister {
        // SAFETY-free: table lookup, not a transmute. Register and
        // NeonRegister both range 0..=31, so this always succeeds.
        const TABLE: [NeonRegister; 32] = [
            NeonRegister::V0,
            NeonRegister::V1,
            NeonRegister::V2,
            NeonRegister::V3,
            NeonRegister::V4,
            NeonRegister::V5,
            NeonRegister::V6,
            NeonRegister::V7,
            NeonRegister::V8,
            NeonRegister::V9,
            NeonRegister::V10,
            NeonRegister::V11,
            NeonRegister::V12,
            NeonRegister::V13,
            NeonRegister::V14,
            NeonRegister::V15,
            NeonRegister::V16,
            NeonRegister::V17,
            NeonRegister::V18,
            NeonRegister::V19,
            NeonRegister::V20,
            NeonRegister::V21,
            NeonRegister::V22,
            NeonRegister::V23,
            NeonRegister::V24,
            NeonRegister::V25,
            NeonRegister::V26,
            NeonRegister::V27,
            NeonRegister::V28,
            NeonRegister::V29,
            NeonRegister::V30,
            NeonRegister::V31,
        ];
        TABLE[reg.raw() as usize]
    }
}

/// Shift kind for a shifted-register operand (`ShiftType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ShiftType {
    Lsl = 0,
    Lsr = 1,
    Asr = 2,
    Ror = 3,
}

/// Extend kind for an extended-register operand (`ExtendType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExtendType {
    Uxtb = 0,
    Uxth = 1,
    Uxtw = 2,
    Uxtx = 3,
    Sxtb = 4,
    Sxth = 5,
    Sxtw = 6,
    Sxtx = 7,
}

/// ARM64 4-bit condition code (`Condition`). Numeric values agree with
/// `enum JitCond` in `jit/c/jit_collect.h` (`JIT_COND_EQ = 0x0` through
/// `JIT_COND_AL = 0xE`) — a hardware invariant, not a coincidence; there is
/// no `NV` (0xF) variant here because the Zig source doesn't define one
/// either (ARM64's `1111` condition-code encoding is a legacy alias for
/// `AL` in most contexts, not a distinct condition).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Condition {
    Eq = 0,
    Ne = 1,
    Cs = 2,
    Cc = 3,
    Mi = 4,
    Pl = 5,
    Vs = 6,
    Vc = 7,
    Hi = 8,
    Ls = 9,
    Ge = 10,
    Lt = 11,
    Gt = 12,
    Le = 13,
    Al = 14,
}

impl Condition {
    /// Flips the low bit — every ARM64 condition pairs with its logical
    /// negation this way (EQ/NE, CS/CC, ..., GT/LE), per `Condition.invert`.
    pub fn invert(self) -> Condition {
        match self {
            Condition::Eq => Condition::Ne,
            Condition::Ne => Condition::Eq,
            Condition::Cs => Condition::Cc,
            Condition::Cc => Condition::Cs,
            Condition::Mi => Condition::Pl,
            Condition::Pl => Condition::Mi,
            Condition::Vs => Condition::Vc,
            Condition::Vc => Condition::Vs,
            Condition::Hi => Condition::Ls,
            Condition::Ls => Condition::Hi,
            Condition::Ge => Condition::Lt,
            Condition::Lt => Condition::Ge,
            Condition::Gt => Condition::Le,
            Condition::Le => Condition::Gt,
            // AL (14) ^ 1 = 15, which has no Condition variant in the Zig
            // source (no NV case) — Zig's `@enumFromInt` on that value
            // would itself be a runtime-checked panic in safe builds, so
            // this arm is unreachable under the same contract: callers
            // never invert AL, matching every emit* alias in this file
            // (CSET/CINC/etc.) which only ever invert real branch
            // conditions, never AL.
            Condition::Al => unreachable!("Condition::Al has no inverse in this encoding"),
        }
    }
}

/// NEON element/vector size (`NeonSize`). Scalar sizes are encoded as
/// `0:Q:size`, vector sizes as `1:Q:size` (Q = bit 3 of the discriminant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NeonSize {
    Size1B = 0,
    Size1H = 1,
    Size1S = 2,
    Size1D = 3,
    Size1Q = 4,
    Size8B = 8,
    Size4H = 9,
    Size2S = 10,
    Size16B = 12,
    Size8H = 13,
    Size4S = 14,
    Size2D = 15,
}

impl NeonSize {
    // Note: the Zig source's size-adjustment idiom (`@intFromEnum(sz) & ~4`
    // to force scalar, `| 4` to force vector, then `@enumFromInt` back) is
    // used by narrowing/widening NEON ops (SHLL, XTN, SQXTN, FCVTL, ...)
    // that aren't in this port's whitelist — no whitelisted `emit*`
    // function needs to reconstruct a `NeonSize` from an adjusted raw
    // discriminant, so that conversion isn't ported here.

    pub fn is_scalar(self) -> bool {
        (self as u8) <= 4
    }

    pub fn is_vector(self) -> bool {
        (self as u8) >= 8
    }

    pub fn element_size(self) -> u8 {
        let v = self as u8;
        if self.is_scalar() {
            v
        } else {
            v & 3
        }
    }

    pub fn q_bit(self) -> u8 {
        ((self as u8) >> 2) & 1
    }
}

/// Branch-instruction immediate-field class, used by [`ArmBranchLinker`]
/// to know which bits of an already-emitted instruction word to patch
/// (`BranchClass`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BranchClass {
    #[default]
    Invalid,
    /// B, BL
    Imm26,
    /// B.cond, CBZ, CBNZ
    Imm19,
    /// TBZ, TBNZ
    Imm14,
}

// ============================================================================
// Section: Constants
// ============================================================================

pub const ARM64_OPCODE_NOP: u32 = 0xd503201f;
pub const ARM64_OPCODE_DMB_ISH: u32 = 0xd5033bbf;
pub const ARM64_OPCODE_DMB_ISHST: u32 = 0xd5033abf;
pub const ARM64_OPCODE_DMB_ISHLD: u32 = 0xd50339bf;

/// System-register encoding helper (`ARM64_SYSREG`): `op0:op1:CRn:CRm:op2`
/// packed into the 15-bit MRS/MSR system-register field. Kept (along with
/// `ARM64_FPCR`/`ARM64_NZCV`) only because the ported test suite below
/// exercises it directly (`ARM64_SYSREG encoding` in the Zig source); no
/// whitelisted `emit*` function needs MRS/MSR, so nothing else here calls
/// it. `ARM64_TPIDR_EL0` and the BRK-code/atomic-opcode constants from the
/// Zig source are dead for this port's whitelist and were dropped.
pub const fn arm64_sysreg(op0: u8, op1: u8, crn: u8, crm: u8, op2: u8) -> u16 {
    ((op0 as u16) << 14) | ((op1 as u16) << 11) | ((crn as u16) << 7) | ((crm as u16) << 3) | (op2 as u16)
}

pub const ARM64_FPCR: u16 = arm64_sysreg(1, 3, 4, 4, 0);
pub const ARM64_NZCV: u16 = arm64_sysreg(1, 3, 4, 2, 0);

// ============================================================================
// Section: NeonSize validity masks
// ============================================================================
//
// Only VALID_4S, VALID_2D, VALID_816B, and VALID_24S are used transitively
// by anything in `neonSizeIsValid` calls reachable from the whitelist — but
// `neonSizeIsValid`/these masks are not actually called by ANY whitelisted
// `emit*` function or its transitive helpers (they exist in the Zig source
// purely as a caller-side validation convenience, invoked by higher-level
// wrapper code that isn't part of this port's scope). Skipped entirely;
// noted here so a future port that needs them knows why they're absent.

// ============================================================================
// Section: RegisterParam
// ============================================================================

/// A shifted- or extended-register operand descriptor (`RegisterParam`).
/// `reg_only`/`shifted`/`extended` mirror the Zig constructors; `encode`/
/// `encode_extended` mirror the two field-layout encoders used by
/// register-register ALU instructions.
#[derive(Debug, Clone, Copy)]
pub struct RegisterParam {
    pub reg: Register,
    pub shift_type: Option<ShiftType>,
    pub extend_type: Option<ExtendType>,
    pub amount: u8,
}

impl RegisterParam {
    pub fn reg_only(reg: Register) -> RegisterParam {
        RegisterParam {
            reg,
            shift_type: None,
            extend_type: None,
            amount: 0,
        }
    }

    pub fn shifted(reg: Register, shift: ShiftType, amount: u8) -> RegisterParam {
        RegisterParam {
            reg,
            shift_type: Some(shift),
            extend_type: None,
            amount,
        }
    }

    pub fn extended(reg: Register, extend: ExtendType, amount: u8) -> RegisterParam {
        RegisterParam {
            reg,
            shift_type: None,
            extend_type: Some(extend),
            amount,
        }
    }

    pub fn is_extended(&self) -> bool {
        self.extend_type.is_some()
    }

    pub fn is_reg_only(&self) -> bool {
        self.shift_type.is_none() && self.extend_type.is_none() && self.amount == 0
    }

    /// Encode for shifted-register instructions: bits `[23:22] shift_type |
    /// [20:16] Rm | [15:10] amount`.
    pub fn encode(&self) -> u32 {
        let shift: u32 = self.shift_type.map_or(0, |s| s as u32);
        let rv = self.reg.raw() as u32;
        let amt = self.amount as u32;
        (shift << 22) | (rv << 16) | (amt << 10)
    }

    /// Encode for extended-register instructions: bits `[20:16] Rm |
    /// [15:13] extend_option | [12:10] amount`.
    pub fn encode_extended(&self) -> u32 {
        let ext: u32 = self.extend_type.map_or(0, |e| e as u32);
        let rv = self.reg.raw() as u32;
        let amt = self.amount as u32;
        (rv << 16) | (ext << 13) | (amt << 10)
    }
}

// ============================================================================
// Section: Logical immediate encoding
// ============================================================================

/// Algorithmic encoder for ARM64 bitmask immediates. Returns the 13-bit
/// `N:immr:imms` field, or `None` if the value cannot be encoded.
/// Direct port of `encodeLogicalImmediate`/`encodeLogicalImmediate64`
/// (collapsed into one function here since the Zig 32-bit wrapper's only
/// job — replicate the low 32 bits into the high 32 and recurse at
/// `reg_size=64` — is easiest to express as a single function with a
/// `reg_size` parameter, matching the *behavior* exactly).
pub fn encode_logical_immediate(value: u64, reg_size: u32) -> Option<u16> {
    if value == 0 || value == 0xFFFF_FFFF_FFFF_FFFF {
        return None;
    }
    if reg_size == 32 {
        let v32 = value & 0xFFFF_FFFF;
        if v32 != ((value >> 32) & 0xFFFF_FFFF) && (value >> 32) != 0 {
            return None;
        }
        return encode_logical_immediate_64(v32 | (v32 << 32), 32);
    }
    encode_logical_immediate_64(value, 64)
}

fn encode_logical_immediate_64(value: u64, reg_size: u32) -> Option<u16> {
    if value == 0 || value == 0xFFFF_FFFF_FFFF_FFFF {
        return None;
    }

    // Determine the smallest element size that tiles the value.
    let mut size: u32 = 64;
    let mut test_size: u32 = 2;
    while test_size <= 32 {
        let mask = (1u64 << test_size) - 1;
        let base = value & mask;
        let mut ok = true;
        let mut shift = test_size;
        while shift < 64 {
            if ((value >> shift) & mask) != base {
                ok = false;
                break;
            }
            shift += test_size;
        }
        if ok {
            size = test_size;
            break;
        }
        test_size *= 2;
    }

    let mask: u64 = if size >= 64 { !0u64 } else { (1u64 << size) - 1 };
    let pattern = value & mask;

    if pattern == 0 || pattern == mask {
        return None;
    }

    let doubled: u64 = if size >= 64 { pattern } else { pattern | (pattern << size) };

    // Find a 0 bit first (there must be one since pattern != mask).
    let mut tmp = pattern;
    let mut first_zero: u32 = 0;
    while first_zero < size {
        if (tmp & 1) == 0 {
            break;
        }
        tmp >>= 1;
        first_zero += 1;
    }

    // Find the first 1 at or after first_zero.
    let mut start_of_ones: u32 = first_zero;
    let limit: u32 = size * 2;
    while start_of_ones < limit {
        if start_of_ones < 64 && ((doubled >> start_of_ones) & 1) == 1 {
            break;
        }
        start_of_ones += 1;
    }

    let rotation: u32 = start_of_ones % size;
    let rotated: u64 = if rotation > 0 {
        ((pattern >> rotation) | (pattern << (size - rotation))) & mask
    } else {
        pattern
    };

    // Count trailing ones.
    let mut ones: u32 = 0;
    let mut rv = rotated;
    while ones < size {
        if (rv & 1) == 0 {
            break;
        }
        rv >>= 1;
        ones += 1;
    }

    // Verify the remaining bits are all zero.
    if rv != 0 {
        return None;
    }
    if ones == 0 || ones == size {
        return None;
    }

    let (n_bit, imms_prefix_ok): (u16, bool) = match size {
        64 => (1, true),
        32 | 16 | 8 | 4 | 2 => (0, true),
        _ => return None,
    };
    if !imms_prefix_ok {
        return None;
    }

    let imms: u16 = match size {
        64 => ((ones - 1) & 0x3f) as u16,
        32 => ((ones - 1) & 0x1f) as u16,
        16 => (0b100000 | ((ones - 1) & 0xf)) as u16,
        8 => (0b110000 | ((ones - 1) & 0x7)) as u16,
        4 => (0b111000 | ((ones - 1) & 0x3)) as u16,
        2 => (0b111100 | ((ones - 1) & 0x1)) as u16,
        _ => return None,
    };

    // If reg_size is 32, N must be 0.
    if reg_size == 32 && n_bit == 1 {
        return None;
    }

    let immr: u16 = (rotation & 0x3f) as u16;

    Some((n_bit << 12) | (immr << 6) | imms)
}

/// Check if a value can be encoded as a logical immediate for a given
/// register size (`canEncodeLogicalImmediate`).
pub fn can_encode_logical_immediate(value: u64, reg_size: u32) -> bool {
    encode_logical_immediate(value, reg_size).is_some()
}

// ============================================================================
// Section: ArmBranchLinker
// ============================================================================

/// Branch linker for resolving branch targets in emitted code
/// (`ArmBranchLinker`). Offsets are tracked in instruction words (4-byte
/// units); `INVALID_OFFSET` is a sentinel meaning "not yet set".
///
/// `linkRaw` (the Zig original's raw-pointer-diff variant used to patch a
/// branch given only two `*u32` instruction addresses) is not ported: it
/// is not called by anything in this crate's encode-dispatch layer — all
/// resolution here goes through [`ArmBranchLinker::resolve`], which takes
/// an explicit `&mut [u32]` buffer and index instead of raw pointers, and
/// needs no `unsafe`. If a future caller needs `linkRaw`'s exact contract
/// (resolving a branch from two bare instruction addresses rather than a
/// buffer + index), it can be added then.
#[derive(Debug, Clone, Copy)]
pub struct ArmBranchLinker {
    instruction_offset: u32,
    target_offset: i32,
    branch_class: BranchClass,
}

impl ArmBranchLinker {
    const INVALID_OFFSET: u32 = 0x8000_0000;

    pub fn init() -> ArmBranchLinker {
        ArmBranchLinker {
            instruction_offset: Self::INVALID_OFFSET,
            target_offset: Self::INVALID_OFFSET as i32,
            branch_class: BranchClass::Invalid,
        }
    }

    pub fn has_instruction(&self) -> bool {
        self.instruction_offset != Self::INVALID_OFFSET
    }

    pub fn has_target(&self) -> bool {
        (self.target_offset as u32) != Self::INVALID_OFFSET
    }

    pub fn reset(&mut self) {
        self.instruction_offset = Self::INVALID_OFFSET;
        self.target_offset = Self::INVALID_OFFSET as i32;
        self.branch_class = BranchClass::Invalid;
    }

    pub fn set_instruction_offset(&mut self, byte_offset: u32) {
        debug_assert_eq!(byte_offset % 4, 0);
        self.instruction_offset = byte_offset / 4;
    }

    pub fn set_class(&mut self, class: BranchClass) {
        self.branch_class = class;
    }

    pub fn set_instruction_and_class(&mut self, byte_offset: u32, class: BranchClass) {
        self.set_instruction_offset(byte_offset);
        self.set_class(class);
    }

    pub fn set_target(&mut self, byte_offset: i32) {
        debug_assert_eq!(byte_offset.rem_euclid(4), 0);
        self.target_offset = byte_offset / 4;
    }

    /// Resolve a branch by patching the instruction word in `buffer` at
    /// `instruction_offset`, if both the instruction and target positions
    /// have been set.
    pub fn resolve(&mut self, buffer: &mut [u32]) {
        if !self.has_instruction() || !self.has_target() {
            return;
        }
        let idx = self.instruction_offset as usize;
        buffer[idx] = self.update_instruction(buffer[idx]);
    }

    fn update_instruction(&self, instruction: u32) -> u32 {
        let delta = self.target_offset.wrapping_sub(self.instruction_offset as i32);
        match self.branch_class {
            BranchClass::Imm26 => (instruction & 0xfc00_0000) | ((delta as u32) & 0x03ff_ffff),
            BranchClass::Imm19 => (instruction & 0xff00_001f) | (((delta as u32) << 5) & 0x00ff_ffe0),
            BranchClass::Imm14 => (instruction & 0xfff8_001f) | (((delta as u32) << 5) & 0x0007_ffe0),
            BranchClass::Invalid => instruction,
        }
    }
}

impl Default for ArmBranchLinker {
    fn default() -> Self {
        Self::init()
    }
}

// ============================================================================
// Section: Helper commons
// ============================================================================

fn r(reg: Register) -> u32 {
    reg.raw() as u32
}

fn nr(reg: NeonRegister) -> u32 {
    reg.raw() as u32
}

// ============================================================================
// Section: NOP / BRK / Debug / HINT
// ============================================================================

pub fn emit_nop() -> u32 {
    ARM64_OPCODE_NOP
}

pub fn emit_brk(code: u16) -> u32 {
    0xd420_0000 | ((code as u32) << 5)
}

// HINT #imm (system hint instruction). NOP = hint #0, BTI C = hint #34,
// BTI J = hint #36, BTI JC = hint #38 (per the Zig source's own comment
// correcting an earlier mistaken value).
pub fn emit_hint(imm: u32) -> u32 {
    0xd503_201f | ((imm & 0x7f) << 5)
}

// ============================================================================
// Section: Branch instructions
// ============================================================================

/// B target (unconditional, PC-relative). Zig `i26` offset parameter
/// widened to `i32`; masked to 26 bits exactly as the Zig body does.
pub fn emit_b(offset_words: i32) -> u32 {
    0x1400_0000 | ((offset_words as u32) & 0x03ff_ffff)
}

/// BL target (branch with link).
pub fn emit_bl(offset_words: i32) -> u32 {
    0x9400_0000 | ((offset_words as u32) & 0x03ff_ffff)
}

/// B.cond target (conditional branch). Zig `i19` offset widened to `i32`;
/// masked to 19 bits.
pub fn emit_b_cond(offset_words: i32, cond: Condition) -> u32 {
    0x5400_0000 | (((offset_words as u32) & 0x7ffff) << 5) | (cond as u32)
}

/// CBZ Wn, target (32-bit).
pub fn emit_cbz(reg: Register, offset_words: i32) -> u32 {
    0x3400_0000 | (((offset_words as u32) & 0x7ffff) << 5) | r(reg)
}

/// CBZ Xn, target (64-bit).
pub fn emit_cbz_64(reg: Register, offset_words: i32) -> u32 {
    0xb400_0000 | (((offset_words as u32) & 0x7ffff) << 5) | r(reg)
}

/// CBNZ Wn, target (32-bit).
pub fn emit_cbnz(reg: Register, offset_words: i32) -> u32 {
    0x3500_0000 | (((offset_words as u32) & 0x7ffff) << 5) | r(reg)
}

/// CBNZ Xn, target (64-bit).
pub fn emit_cbnz_64(reg: Register, offset_words: i32) -> u32 {
    0xb500_0000 | (((offset_words as u32) & 0x7ffff) << 5) | r(reg)
}

/// BR Xn (branch register).
pub fn emit_br(reg: Register) -> u32 {
    0xd61f_0000 | (r(reg) << 5)
}

/// BLR Xn (branch with link register).
pub fn emit_blr(reg: Register) -> u32 {
    0xd63f_0000 | (r(reg) << 5)
}

/// RET Xn.
pub fn emit_ret(reg: Register) -> u32 {
    0xd65f_0000 | (r(reg) << 5)
}

// ============================================================================
// Section: ADD/SUB register
// ============================================================================

fn emit_add_sub_register_common(
    dest: Register,
    src1: Register,
    src2: RegisterParam,
    opcode: u32,
    extended_opcode: u32,
) -> u32 {
    if src2.is_extended() {
        extended_opcode | src2.encode_extended() | (r(src1) << 5) | r(dest)
    } else {
        opcode | src2.encode() | (r(src1) << 5) | r(dest)
    }
}

pub fn emit_add_register(dest: Register, src1: Register, src2: RegisterParam) -> u32 {
    emit_add_sub_register_common(dest, src1, src2, 0x0b00_0000, 0x0b20_0000)
}

pub fn emit_add_register_64(dest: Register, src1: Register, src2: RegisterParam) -> u32 {
    emit_add_sub_register_common(dest, src1, src2, 0x8b00_0000, 0x8b20_0000)
}

fn emit_adds_register(dest: Register, src1: Register, src2: RegisterParam) -> u32 {
    emit_add_sub_register_common(dest, src1, src2, 0x2b00_0000, 0x2b20_0000)
}

fn emit_adds_register_64(dest: Register, src1: Register, src2: RegisterParam) -> u32 {
    emit_add_sub_register_common(dest, src1, src2, 0xab00_0000, 0xab20_0000)
}

pub fn emit_sub_register(dest: Register, src1: Register, src2: RegisterParam) -> u32 {
    emit_add_sub_register_common(dest, src1, src2, 0x4b00_0000, 0x4b20_0000)
}

pub fn emit_sub_register_64(dest: Register, src1: Register, src2: RegisterParam) -> u32 {
    emit_add_sub_register_common(dest, src1, src2, 0xcb00_0000, 0xcb20_0000)
}

fn emit_subs_register(dest: Register, src1: Register, src2: RegisterParam) -> u32 {
    emit_add_sub_register_common(dest, src1, src2, 0x6b00_0000, 0x6b20_0000)
}

fn emit_subs_register_64(dest: Register, src1: Register, src2: RegisterParam) -> u32 {
    emit_add_sub_register_common(dest, src1, src2, 0xeb00_0000, 0xeb20_0000)
}

/// CMP (alias for SUBS with ZR dest).
pub fn emit_cmp_register(src1: Register, src2: RegisterParam) -> u32 {
    emit_subs_register(Register::ZR, src1, src2)
}

pub fn emit_cmp_register_64(src1: Register, src2: RegisterParam) -> u32 {
    emit_subs_register_64(Register::ZR, src1, src2)
}

/// CMN Wn, Wm (alias for ADDS WZR, Wn, Wm).
pub fn emit_cmn_register(src1: Register, src2: RegisterParam) -> u32 {
    emit_adds_register(Register::ZR, src1, src2)
}

/// CMN Xn, Xm (alias for ADDS XZR, Xn, Xm).
pub fn emit_cmn_register_64(src1: Register, src2: RegisterParam) -> u32 {
    emit_adds_register_64(Register::ZR, src1, src2)
}

// ============================================================================
// Section: MADD/MSUB/MUL/SDIV/UDIV
// ============================================================================

fn emit_madd_msub_common(dest: Register, src1: Register, src2: Register, src3: Register, opcode: u32) -> u32 {
    opcode | (r(src2) << 16) | (r(src3) << 10) | (r(src1) << 5) | r(dest)
}

pub fn emit_madd(dest: Register, src1: Register, src2: Register, src3: Register) -> u32 {
    emit_madd_msub_common(dest, src1, src2, src3, 0x1b00_0000)
}

pub fn emit_madd_64(dest: Register, src1: Register, src2: Register, src3: Register) -> u32 {
    emit_madd_msub_common(dest, src1, src2, src3, 0x9b00_0000)
}

pub fn emit_msub(dest: Register, src1: Register, src2: Register, src3: Register) -> u32 {
    emit_madd_msub_common(dest, src1, src2, src3, 0x1b00_8000)
}

pub fn emit_msub_64(dest: Register, src1: Register, src2: Register, src3: Register) -> u32 {
    emit_madd_msub_common(dest, src1, src2, src3, 0x9b00_8000)
}

/// MUL (alias for MADD with ZR addend).
pub fn emit_mul(dest: Register, src1: Register, src2: Register) -> u32 {
    emit_madd(dest, src1, src2, Register::ZR)
}

pub fn emit_mul_64(dest: Register, src1: Register, src2: Register) -> u32 {
    emit_madd_64(dest, src1, src2, Register::ZR)
}

fn emit_divide_common(dest: Register, src1: Register, src2: Register, opcode: u32) -> u32 {
    opcode | (r(src2) << 16) | (r(src1) << 5) | r(dest)
}

pub fn emit_sdiv(dest: Register, src1: Register, src2: Register) -> u32 {
    emit_divide_common(dest, src1, src2, 0x1ac0_0c00)
}

pub fn emit_sdiv_64(dest: Register, src1: Register, src2: Register) -> u32 {
    emit_divide_common(dest, src1, src2, 0x9ac0_0c00)
}

pub fn emit_udiv(dest: Register, src1: Register, src2: Register) -> u32 {
    emit_divide_common(dest, src1, src2, 0x1ac0_0800)
}

pub fn emit_udiv_64(dest: Register, src1: Register, src2: Register) -> u32 {
    emit_divide_common(dest, src1, src2, 0x9ac0_0800)
}

// ============================================================================
// Section: NEG alias
// ============================================================================

/// NEG Xd, Xm (alias for SUB Xd, XZR, Xm) — used indirectly via
/// `emit_sub_register`/`emit_sub_register_64` directly at call sites in
/// this crate's dispatch layer in most cases, but ported explicitly since
/// `emitNeg`/`emitNeg64` are whitelisted.
pub fn emit_neg(dest: Register, src: Register) -> u32 {
    emit_sub_register(dest, Register::ZR, RegisterParam::reg_only(src))
}

pub fn emit_neg_64(dest: Register, src: Register) -> u32 {
    emit_sub_register_64(dest, Register::ZR, RegisterParam::reg_only(src))
}

/// SXTW Xd, Wn (alias for SBFM Xd, Xn, #0, #31).
pub fn emit_sxtw_64(dest: Register, src: Register) -> u32 {
    emit_sbfm_64(dest, src, 0, 31)
}

// ============================================================================
// Section: Scalar FCVT (float precision conversion)
// ============================================================================

/// FCVT Dd, Sn (single-precision to double-precision).
pub fn emit_fcvt_s_to_d(dest: NeonRegister, src: NeonRegister) -> u32 {
    0x1e22_c000 | (nr(src) << 5) | nr(dest)
}

/// FCVT Sd, Dn (double-precision to single-precision).
pub fn emit_fcvt_d_to_s(dest: NeonRegister, src: NeonRegister) -> u32 {
    0x1e62_4000 | (nr(src) << 5) | nr(dest)
}

// ============================================================================
// Section: Logical register operations
// ============================================================================

fn emit_logical_register_common(dest: Register, src1: Register, src2: RegisterParam, opcode: u32) -> u32 {
    opcode | src2.encode() | (r(src1) << 5) | r(dest)
}

pub fn emit_and_register(dest: Register, src1: Register, src2: RegisterParam) -> u32 {
    emit_logical_register_common(dest, src1, src2, 0x0a00_0000)
}

pub fn emit_and_register_64(dest: Register, src1: Register, src2: RegisterParam) -> u32 {
    emit_logical_register_common(dest, src1, src2, 0x8a00_0000)
}

fn emit_ands_register(dest: Register, src1: Register, src2: RegisterParam) -> u32 {
    emit_logical_register_common(dest, src1, src2, 0x6a00_0000)
}

fn emit_ands_register_64(dest: Register, src1: Register, src2: RegisterParam) -> u32 {
    emit_logical_register_common(dest, src1, src2, 0xea00_0000)
}

pub fn emit_eor_register(dest: Register, src1: Register, src2: RegisterParam) -> u32 {
    emit_logical_register_common(dest, src1, src2, 0x4a00_0000)
}

pub fn emit_eor_register_64(dest: Register, src1: Register, src2: RegisterParam) -> u32 {
    emit_logical_register_common(dest, src1, src2, 0xca00_0000)
}

pub fn emit_orr_register(dest: Register, src1: Register, src2: RegisterParam) -> u32 {
    emit_logical_register_common(dest, src1, src2, 0x2a00_0000)
}

pub fn emit_orr_register_64(dest: Register, src1: Register, src2: RegisterParam) -> u32 {
    emit_logical_register_common(dest, src1, src2, 0xaa00_0000)
}

/// TST (alias for ANDS with ZR dest).
pub fn emit_test_register(src1: Register, src2: RegisterParam) -> u32 {
    emit_ands_register(Register::ZR, src1, src2)
}

pub fn emit_test_register_64(src1: Register, src2: RegisterParam) -> u32 {
    emit_ands_register_64(Register::ZR, src1, src2)
}

/// MOV (alias for ORR with ZR src1).
pub fn emit_mov_register(dest: Register, src: Register) -> u32 {
    emit_orr_register(dest, Register::ZR, RegisterParam::reg_only(src))
}

pub fn emit_mov_register_64(dest: Register, src: Register) -> u32 {
    emit_orr_register_64(dest, Register::ZR, RegisterParam::reg_only(src))
}

// ============================================================================
// Section: Shift register operations (variable shift)
// ============================================================================

fn emit_shift_register_common(dest: Register, src1: Register, src2: Register, opcode: u32) -> u32 {
    opcode | (r(src2) << 16) | (r(src1) << 5) | r(dest)
}

pub fn emit_asr_register(dest: Register, src1: Register, src2: Register) -> u32 {
    emit_shift_register_common(dest, src1, src2, 0x1ac0_2800)
}

pub fn emit_asr_register_64(dest: Register, src1: Register, src2: Register) -> u32 {
    emit_shift_register_common(dest, src1, src2, 0x9ac0_2800)
}

pub fn emit_lsl_register(dest: Register, src1: Register, src2: Register) -> u32 {
    emit_shift_register_common(dest, src1, src2, 0x1ac0_2000)
}

pub fn emit_lsl_register_64(dest: Register, src1: Register, src2: Register) -> u32 {
    emit_shift_register_common(dest, src1, src2, 0x9ac0_2000)
}

pub fn emit_lsr_register(dest: Register, src1: Register, src2: Register) -> u32 {
    emit_shift_register_common(dest, src1, src2, 0x1ac0_2400)
}

pub fn emit_lsr_register_64(dest: Register, src1: Register, src2: Register) -> u32 {
    emit_shift_register_common(dest, src1, src2, 0x9ac0_2400)
}

// ============================================================================
// Section: Bitfield operations
// ============================================================================

fn emit_bitfield_common(dest: Register, src: Register, immr: u32, imms: u32, opcode: u32) -> u32 {
    opcode | ((immr & 0x3f) << 16) | ((imms & 0x3f) << 10) | (r(src) << 5) | r(dest)
}

pub fn emit_sbfm(dest: Register, src: Register, immr: u32, imms: u32) -> u32 {
    emit_bitfield_common(dest, src, immr, imms, 0x1300_0000)
}

pub fn emit_sbfm_64(dest: Register, src: Register, immr: u32, imms: u32) -> u32 {
    emit_bitfield_common(dest, src, immr, imms, 0x9340_0000)
}

pub fn emit_ubfm(dest: Register, src: Register, immr: u32, imms: u32) -> u32 {
    emit_bitfield_common(dest, src, immr, imms, 0x5300_0000)
}

pub fn emit_ubfm_64(dest: Register, src: Register, immr: u32, imms: u32) -> u32 {
    emit_bitfield_common(dest, src, immr, imms, 0xd340_0000)
}

// SXTB/SXTH/UXTB/UXTH (aliases of SBFM/UBFM with immr=0).
pub fn emit_sxtb(dest: Register, src: Register) -> u32 {
    emit_sbfm(dest, src, 0, 7)
}

pub fn emit_sxtb_64(dest: Register, src: Register) -> u32 {
    emit_sbfm_64(dest, src, 0, 7)
}

pub fn emit_sxth(dest: Register, src: Register) -> u32 {
    emit_sbfm(dest, src, 0, 15)
}

pub fn emit_sxth_64(dest: Register, src: Register) -> u32 {
    emit_sbfm_64(dest, src, 0, 15)
}

pub fn emit_uxtb(dest: Register, src: Register) -> u32 {
    emit_ubfm(dest, src, 0, 7)
}

pub fn emit_uxtb_64(dest: Register, src: Register) -> u32 {
    emit_ubfm_64(dest, src, 0, 7)
}

pub fn emit_uxth(dest: Register, src: Register) -> u32 {
    emit_ubfm(dest, src, 0, 15)
}

pub fn emit_uxth_64(dest: Register, src: Register) -> u32 {
    emit_ubfm_64(dest, src, 0, 15)
}

// ============================================================================
// Section: Shift/rotate immediate (aliases of bitfield instructions)
// ============================================================================

pub fn emit_asr_immediate(dest: Register, src: Register, imm: u32) -> u32 {
    emit_sbfm(dest, src, imm & 0x1f, 31)
}

pub fn emit_asr_immediate_64(dest: Register, src: Register, imm: u32) -> u32 {
    emit_sbfm_64(dest, src, imm & 0x3f, 63)
}

pub fn emit_lsl_immediate(dest: Register, src: Register, imm: u32) -> u32 {
    let imm = imm & 0x1f;
    let immr = (32u32.wrapping_sub(imm)) % 32;
    let imms = 31u32.wrapping_sub(imm) & 0x1f;
    emit_ubfm(dest, src, immr & 0x1f, imms)
}

pub fn emit_lsl_immediate_64(dest: Register, src: Register, imm: u32) -> u32 {
    let imm = imm & 0x3f;
    let immr = (64u32.wrapping_sub(imm)) % 64;
    let imms = 63u32.wrapping_sub(imm) & 0x3f;
    emit_ubfm_64(dest, src, immr & 0x3f, imms)
}

pub fn emit_lsr_immediate(dest: Register, src: Register, imm: u32) -> u32 {
    emit_ubfm(dest, src, imm & 0x1f, 31)
}

pub fn emit_lsr_immediate_64(dest: Register, src: Register, imm: u32) -> u32 {
    emit_ubfm_64(dest, src, imm & 0x3f, 63)
}

// ============================================================================
// Section: Conditional select
// ============================================================================

fn emit_conditional_common(dest: Register, src1: Register, src2: Register, cond: Condition, opcode: u32) -> u32 {
    opcode | (r(src2) << 16) | ((cond as u32) << 12) | (r(src1) << 5) | r(dest)
}

pub fn emit_csel(dest: Register, src1: Register, src2: Register, cond: Condition) -> u32 {
    emit_conditional_common(dest, src1, src2, cond, 0x1a80_0000)
}

pub fn emit_csel_64(dest: Register, src1: Register, src2: Register, cond: Condition) -> u32 {
    emit_conditional_common(dest, src1, src2, cond, 0x9a80_0000)
}

fn emit_csinc(dest: Register, src1: Register, src2: Register, cond: Condition) -> u32 {
    emit_conditional_common(dest, src1, src2, cond, 0x1a80_0400)
}

fn emit_csinc_64(dest: Register, src1: Register, src2: Register, cond: Condition) -> u32 {
    emit_conditional_common(dest, src1, src2, cond, 0x9a80_0400)
}

/// CSET (alias: CSINC with ZR sources and inverted condition).
pub fn emit_cset(dest: Register, cond: Condition) -> u32 {
    emit_csinc(dest, Register::ZR, Register::ZR, cond.invert())
}

pub fn emit_cset_64(dest: Register, cond: Condition) -> u32 {
    emit_csinc_64(dest, Register::ZR, Register::ZR, cond.invert())
}

// ============================================================================
// Section: Move immediate (MOVZ/MOVN/MOVK)
// ============================================================================

fn emit_mov_immediate_common(dest: Register, imm16: u16, shift: u32, opcode: u32) -> u32 {
    debug_assert_eq!(shift % 16, 0);
    opcode | (((shift / 16) & 0x3) << 21) | ((imm16 as u32) << 5) | r(dest)
}

pub fn emit_movk(dest: Register, imm16: u16, shift: u32) -> u32 {
    emit_mov_immediate_common(dest, imm16, shift, 0x7280_0000)
}

pub fn emit_movk_64(dest: Register, imm16: u16, shift: u32) -> u32 {
    emit_mov_immediate_common(dest, imm16, shift, 0xf280_0000)
}

pub fn emit_movn(dest: Register, imm16: u16, shift: u32) -> u32 {
    emit_mov_immediate_common(dest, imm16, shift, 0x1280_0000)
}

pub fn emit_movn_64(dest: Register, imm16: u16, shift: u32) -> u32 {
    emit_mov_immediate_common(dest, imm16, shift, 0x9280_0000)
}

pub fn emit_movz(dest: Register, imm16: u16, shift: u32) -> u32 {
    emit_mov_immediate_common(dest, imm16, shift, 0x5280_0000)
}

pub fn emit_movz_64(dest: Register, imm16: u16, shift: u32) -> u32 {
    emit_mov_immediate_common(dest, imm16, shift, 0xd280_0000)
}

/// Load a 32-bit immediate into a register using the minimal number of
/// instructions. Returns the words written to `out[..n]`, where `n` is the
/// returned count (1 or 2). Direct port of `emitLoadImmediate32`.
pub fn emit_load_immediate_32(dest: Register, imm: u32, out: &mut [u32; 2]) -> u8 {
    let word0 = (imm & 0xffff) as u16;
    let word1 = ((imm >> 16) & 0xffff) as u16;

    if word0 != 0 && word1 != 0 {
        // Try logical immediate encoding (ORR with ZR).
        if let Some(enc) = encode_logical_immediate(imm as u64, 32) {
            out[0] = 0x3200_0000 | ((enc as u32) << 10) | (31 << 5) | r(dest);
            return 1;
        }

        // Use MOVN if one half is 0xffff.
        if word1 == 0xffff {
            out[0] = 0x1280_0000 | (((word0 ^ 0xffff) as u32) << 5) | r(dest);
            return 1;
        } else if word0 == 0xffff {
            out[0] = 0x1280_0000 | (1 << 21) | (((word1 ^ 0xffff) as u32) << 5) | r(dest);
            return 1;
        }
    }

    // Use MOVZ + optional MOVK.
    let mut count: u8 = 0;
    if word0 != 0 || word1 == 0 {
        out[count as usize] = 0x5280_0000 | ((word0 as u32) << 5) | r(dest);
        count += 1;
    }
    if word1 != 0 {
        let opcode: u32 = if count > 0 { 0x7280_0000 } else { 0x5280_0000 };
        out[count as usize] = opcode | (1 << 21) | ((word1 as u32) << 5) | r(dest);
        count += 1;
    }
    count
}

/// Load a 64-bit immediate into a register using minimal instructions.
/// Returns the number of instruction words written to `out[..n]` (1-4).
/// Direct port of `emitLoadImmediate64`.
pub fn emit_load_immediate_64(dest: Register, imm: u64, out: &mut [u32; 4]) -> u8 {
    // If upper 32 bits are 0, use the 32-bit form.
    if (imm >> 32) == 0 {
        let mut buf2: [u32; 2] = [0; 2];
        let n = emit_load_immediate_32(dest, imm as u32, &mut buf2);
        for i in 0..n as usize {
            out[i] = buf2[i];
        }
        return n;
    }

    // Try logical immediate encoding (ORR with ZR).
    if let Some(enc) = encode_logical_immediate(imm, 64) {
        out[0] = 0xb200_0000 | ((enc as u32) << 10) | (31 << 5) | r(dest);
        return 1;
    }

    // Break into words and count zeros/ones.
    let words: [u16; 4] = [
        (imm & 0xffff) as u16,
        ((imm >> 16) & 0xffff) as u16,
        ((imm >> 32) & 0xffff) as u16,
        ((imm >> 48) & 0xffff) as u16,
    ];

    let mut num_zeros: u8 = 0;
    let mut num_ones: u8 = 0;
    for &w in &words {
        if w == 0 {
            num_zeros += 1;
        }
        if w == 0xffff {
            num_ones += 1;
        }
    }

    let use_movn = num_ones > num_zeros;
    let word_mask: u16 = if use_movn { 0xffff } else { 0x0000 };
    let mut first = true;
    let mut count: u8 = 0;

    for (i, &w) in words.iter().enumerate() {
        if w != word_mask {
            let shift = (i as u32) * 16;
            if first {
                if use_movn {
                    out[count as usize] = emit_movn_64(dest, w ^ 0xffff, shift);
                } else {
                    out[count as usize] = emit_movz_64(dest, w, shift);
                }
                first = false;
            } else {
                out[count as usize] = emit_movk_64(dest, w, shift);
            }
            count += 1;
        }
    }
    count
}

// ============================================================================
// Section: PC-relative addressing (ADR/ADRP)
// ============================================================================

fn emit_adr_adrp(dest: Register, offset: i32, opcode: u32) -> u32 {
    let off = offset as u32;
    opcode | ((off & 3) << 29) | (((off >> 2) & 0x7ffff) << 5) | r(dest)
}

/// ADR Xd, #offset. Zig `i21` offset widened to `i32`.
pub fn emit_adr(dest: Register, offset: i32) -> u32 {
    emit_adr_adrp(dest, offset, 0x1000_0000)
}

/// ADRP Xd, #page_offset.
pub fn emit_adrp(dest: Register, page_offset: i32) -> u32 {
    emit_adr_adrp(dest, page_offset, 0x9000_0000)
}

// ============================================================================
// Section: ADD/SUB immediate
// ============================================================================

/// Core helper: encode ADD/SUB immediate with auto-negation and LSL #12
/// support. Returns `None` if the immediate can't be encoded. Direct port
/// of `emitAddSubImmediateCommon` — note the Zig original is itself `pub`
/// (unlike most `*Common` helpers) but is not itself in the whitelist; the
/// eight `emit*Immediate`/`emit*Immediate64` wrappers that call it are.
fn emit_add_sub_immediate_common(
    dest: Register,
    src: Register,
    immediate: i32,
    set_flags: bool,
    is_add: bool,
    opcode_high_bit: u32,
) -> Option<u32> {
    let s_bit: u32 = if set_flags { 1 << 29 } else { 0 };

    if immediate == 0 {
        return Some(if is_add {
            opcode_high_bit | s_bit | 0x1100_0000 | (r(src) << 5) | r(dest)
        } else {
            opcode_high_bit | s_bit | 0x5100_0000 | (r(src) << 5) | r(dest)
        });
    }

    // 12-bit unsigned immediate.
    if (immediate & 0xfff) == immediate {
        return Some(
            opcode_high_bit | s_bit | 0x1100_0000 | (((immediate as u32) & 0xfff) << 10) | (r(src) << 5) | r(dest),
        );
    }

    let neg_imm = immediate.wrapping_neg();
    let neg_u = neg_imm as u32;

    if (neg_u & 0xfff) == neg_u {
        return Some(opcode_high_bit | s_bit | 0x5100_0000 | ((neg_u & 0xfff) << 10) | (r(src) << 5) | r(dest));
    }

    // 12-bit shifted left by 12.
    let imm_u = immediate as u32;
    if (imm_u & 0xfff000) == imm_u {
        return Some(
            opcode_high_bit
                | s_bit
                | 0x1100_0000
                | (1 << 22)
                | (((imm_u >> 12) & 0xfff) << 10)
                | (r(src) << 5)
                | r(dest),
        );
    }
    if (neg_u & 0xfff000) == neg_u {
        return Some(
            opcode_high_bit
                | s_bit
                | 0x5100_0000
                | (1 << 22)
                | (((neg_u >> 12) & 0xfff) << 10)
                | (r(src) << 5)
                | r(dest),
        );
    }

    None
}

pub fn emit_add_immediate(dest: Register, src: Register, imm: u16) -> u32 {
    emit_add_sub_immediate_common(dest, src, imm as i32, false, true, 0)
        .expect("12-bit unsigned immediate always encodable")
}

pub fn emit_add_immediate_64(dest: Register, src: Register, imm: u16) -> u32 {
    emit_add_sub_immediate_common(dest, src, imm as i32, false, true, 0x8000_0000)
        .expect("12-bit unsigned immediate always encodable")
}

/// Not on the whitelist (no `JitInst` kind needs ADDS-immediate directly —
/// `JIT_CMP_RI` goes through `emit_cmp_immediate[_64]` instead) — kept
/// `#[cfg(test)]`-only because one ported test exercises it directly.
#[cfg(test)]
fn emit_adds_immediate(dest: Register, src: Register, imm: u16) -> u32 {
    emit_add_sub_immediate_common(dest, src, imm as i32, true, true, 0)
        .expect("12-bit unsigned immediate always encodable")
}

pub fn emit_sub_immediate(dest: Register, src: Register, imm: u16) -> u32 {
    emit_add_sub_immediate_common(dest, src, -(imm as i32), false, false, 0)
        .expect("12-bit unsigned immediate always encodable")
}

pub fn emit_sub_immediate_64(dest: Register, src: Register, imm: u16) -> u32 {
    emit_add_sub_immediate_common(dest, src, -(imm as i32), false, false, 0x8000_0000)
        .expect("12-bit unsigned immediate always encodable")
}

fn emit_subs_immediate(dest: Register, src: Register, imm: u16) -> u32 {
    emit_add_sub_immediate_common(dest, src, -(imm as i32), true, false, 0)
        .expect("12-bit unsigned immediate always encodable")
}

fn emit_subs_immediate_64(dest: Register, src: Register, imm: u16) -> u32 {
    emit_add_sub_immediate_common(dest, src, -(imm as i32), true, false, 0x8000_0000)
        .expect("12-bit unsigned immediate always encodable")
}

/// CMP immediate (alias for SUBS with ZR dest).
pub fn emit_cmp_immediate(src: Register, imm: u16) -> u32 {
    emit_subs_immediate(Register::ZR, src, imm)
}

pub fn emit_cmp_immediate_64(src: Register, imm: u16) -> u32 {
    emit_subs_immediate_64(Register::ZR, src, imm)
}

/// CMN Wn, #imm (alias for ADDS WZR, Wn, #imm). Only referenced via the
/// `emit_cmn_immediate` test below (`emitCmnImmediate` is not itself
/// whitelisted, but its test exercises `emitAddsImmediate` — a whitelist
/// prerequisite — so the helper stays private here; the public wrapper is
/// intentionally omitted since nothing in the whitelist calls it).
#[cfg(test)]
fn emit_cmn_immediate(src: Register, imm: u16) -> u32 {
    emit_adds_immediate(Register::ZR, src, imm)
}

// ============================================================================
// Section: Logical immediate
// ============================================================================

fn emit_logical_immediate_common(
    dest: Register,
    src: Register,
    imm: u64,
    opcode: u32,
    not_reg_opcode: u32,
    size: u32,
) -> Option<u32> {
    if let Some(enc) = encode_logical_immediate(imm, size) {
        return Some(opcode | ((enc as u32) << 10) | (r(src) << 5) | r(dest));
    }

    // Special case: -1 can't be encoded, but the NOT form can use ZR.
    if not_reg_opcode != 0 {
        let is_all_ones = (size == 32 && (imm as u32) == 0xFFFF_FFFF) || (size == 64 && imm == 0xFFFF_FFFF_FFFF_FFFF);
        if is_all_ones {
            return Some(not_reg_opcode | (31 << 16) | (r(src) << 5) | r(dest));
        }
    }

    None
}

pub fn emit_and_immediate(dest: Register, src: Register, imm: u32) -> Option<u32> {
    emit_logical_immediate_common(dest, src, imm as u64, 0x1200_0000, 0x0a20_0000, 32)
}

pub fn emit_and_immediate_64(dest: Register, src: Register, imm: u64) -> Option<u32> {
    emit_logical_immediate_common(dest, src, imm, 0x9200_0000, 0x8a20_0000, 64)
}

fn emit_ands_immediate(dest: Register, src: Register, imm: u32) -> Option<u32> {
    emit_logical_immediate_common(dest, src, imm as u64, 0x7200_0000, 0x6a20_0000, 32)
}

fn emit_ands_immediate_64(dest: Register, src: Register, imm: u64) -> Option<u32> {
    emit_logical_immediate_common(dest, src, imm, 0xf200_0000, 0xea20_0000, 64)
}

pub fn emit_orr_immediate(dest: Register, src: Register, imm: u32) -> Option<u32> {
    emit_logical_immediate_common(dest, src, imm as u64, 0x3200_0000, 0x2a20_0000, 32)
}

pub fn emit_orr_immediate_64(dest: Register, src: Register, imm: u64) -> Option<u32> {
    emit_logical_immediate_common(dest, src, imm, 0xb200_0000, 0xaa20_0000, 64)
}

pub fn emit_eor_immediate(dest: Register, src: Register, imm: u32) -> Option<u32> {
    emit_logical_immediate_common(dest, src, imm as u64, 0x5200_0000, 0x4a20_0000, 32)
}

pub fn emit_eor_immediate_64(dest: Register, src: Register, imm: u64) -> Option<u32> {
    emit_logical_immediate_common(dest, src, imm, 0xd200_0000, 0xca20_0000, 64)
}

/// TST immediate (ANDS with ZR dest).
pub fn emit_test_immediate(src: Register, imm: u32) -> Option<u32> {
    emit_ands_immediate(Register::ZR, src, imm)
}

pub fn emit_test_immediate_64(src: Register, imm: u64) -> Option<u32> {
    emit_ands_immediate_64(Register::ZR, src, imm)
}

// ============================================================================
// Section: Load/store with unsigned offset / unscaled offset
// ============================================================================

fn emit_ldr_str_offset_common(
    src_dest: Register,
    addr: Register,
    offset: i32,
    access_shift: u32,
    opcode: u32,
    opcode_unscaled: u32,
) -> Option<u32> {
    if opcode != 0 {
        let enc_offset = (offset as u32) >> access_shift;
        if (((enc_offset << access_shift) as i32) == offset) && (enc_offset & 0xfff) == enc_offset {
            return Some(opcode | ((enc_offset & 0xfff) << 10) | (r(addr) << 5) | r(src_dest));
        }
    }

    if opcode_unscaled != 0 && offset >= -0x100 && offset <= 0xff {
        return Some(opcode_unscaled | (((offset as u32) & 0x1ff) << 12) | (r(addr) << 5) | r(src_dest));
    }

    None
}

// LDR byte offset.
pub fn emit_ldrb_offset(dest: Register, addr: Register, offset: i32) -> Option<u32> {
    emit_ldr_str_offset_common(dest, addr, offset, 0, 0x3940_0000, 0x3840_0000)
}

pub fn emit_ldrsb_offset(dest: Register, addr: Register, offset: i32) -> Option<u32> {
    emit_ldr_str_offset_common(dest, addr, offset, 0, 0x39c0_0000, 0x38c0_0000)
}

pub fn emit_ldrsb_offset_64(dest: Register, addr: Register, offset: i32) -> Option<u32> {
    emit_ldr_str_offset_common(dest, addr, offset, 0, 0x3980_0000, 0x3880_0000)
}

// LDR half offset.
pub fn emit_ldrh_offset(dest: Register, addr: Register, offset: i32) -> Option<u32> {
    emit_ldr_str_offset_common(dest, addr, offset, 1, 0x7940_0000, 0x7840_0000)
}

pub fn emit_ldrsh_offset(dest: Register, addr: Register, offset: i32) -> Option<u32> {
    emit_ldr_str_offset_common(dest, addr, offset, 1, 0x79c0_0000, 0x78c0_0000)
}

pub fn emit_ldrsh_offset_64(dest: Register, addr: Register, offset: i32) -> Option<u32> {
    emit_ldr_str_offset_common(dest, addr, offset, 1, 0x7980_0000, 0x7880_0000)
}

// LDR word offset.
pub fn emit_ldr_offset(dest: Register, addr: Register, offset: i32) -> Option<u32> {
    emit_ldr_str_offset_common(dest, addr, offset, 2, 0xb940_0000, 0xb840_0000)
}

pub fn emit_ldrsw_offset_64(dest: Register, addr: Register, offset: i32) -> Option<u32> {
    emit_ldr_str_offset_common(dest, addr, offset, 2, 0xb980_0000, 0xb880_0000)
}

// LDR dword offset.
pub fn emit_ldr_offset_64(dest: Register, addr: Register, offset: i32) -> Option<u32> {
    emit_ldr_str_offset_common(dest, addr, offset, 3, 0xf940_0000, 0xf840_0000)
}

// STR byte/half/word/dword offset.
pub fn emit_strb_offset(src: Register, addr: Register, offset: i32) -> Option<u32> {
    emit_ldr_str_offset_common(src, addr, offset, 0, 0x3900_0000, 0x3800_0000)
}

pub fn emit_strh_offset(src: Register, addr: Register, offset: i32) -> Option<u32> {
    emit_ldr_str_offset_common(src, addr, offset, 1, 0x7900_0000, 0x7800_0000)
}

pub fn emit_str_offset(src: Register, addr: Register, offset: i32) -> Option<u32> {
    emit_ldr_str_offset_common(src, addr, offset, 2, 0xb900_0000, 0xb800_0000)
}

pub fn emit_str_offset_64(src: Register, addr: Register, offset: i32) -> Option<u32> {
    emit_ldr_str_offset_common(src, addr, offset, 3, 0xf900_0000, 0xf800_0000)
}

// ============================================================================
// Section: Load/store with register offset
// ============================================================================

fn emit_ldr_str_register_common(
    src_dest: Register,
    addr: Register,
    index: RegisterParam,
    access_shift: u8,
    opcode: u32,
) -> u32 {
    let extend_type: u32 = index.extend_type.map_or(ExtendType::Uxtx as u32, |e| e as u32);

    let mut amount: u32 = 0;
    if index.amount == access_shift && access_shift != 0 {
        amount = 1;
    } else if index.amount != 0 && index.amount != access_shift {
        amount = 0;
    }

    opcode | ((index.reg.raw() as u32) << 16) | (extend_type << 13) | (amount << 12) | (r(addr) << 5) | r(src_dest)
}

pub fn emit_ldrb_register(dest: Register, addr: Register, index: RegisterParam) -> u32 {
    emit_ldr_str_register_common(dest, addr, index, 0, 0x3860_0800)
}

pub fn emit_ldrsb_register(dest: Register, addr: Register, index: RegisterParam) -> u32 {
    emit_ldr_str_register_common(dest, addr, index, 0, 0x38e0_0800)
}

pub fn emit_ldrsb_register_64(dest: Register, addr: Register, index: RegisterParam) -> u32 {
    emit_ldr_str_register_common(dest, addr, index, 0, 0x38a0_0800)
}

pub fn emit_ldrh_register(dest: Register, addr: Register, index: RegisterParam) -> u32 {
    emit_ldr_str_register_common(dest, addr, index, 1, 0x7860_0800)
}

pub fn emit_ldrsh_register(dest: Register, addr: Register, index: RegisterParam) -> u32 {
    emit_ldr_str_register_common(dest, addr, index, 1, 0x78e0_0800)
}

pub fn emit_ldrsh_register_64(dest: Register, addr: Register, index: RegisterParam) -> u32 {
    emit_ldr_str_register_common(dest, addr, index, 1, 0x78a0_0800)
}

pub fn emit_ldr_register(dest: Register, addr: Register, index: RegisterParam) -> u32 {
    emit_ldr_str_register_common(dest, addr, index, 2, 0xb860_0800)
}

pub fn emit_ldr_register_64(dest: Register, addr: Register, index: RegisterParam) -> u32 {
    emit_ldr_str_register_common(dest, addr, index, 3, 0xf860_0800)
}

pub fn emit_ldrsw_register_64(dest: Register, addr: Register, index: RegisterParam) -> u32 {
    emit_ldr_str_register_common(dest, addr, index, 2, 0xb8a0_0800)
}

pub fn emit_strb_register(src: Register, addr: Register, index: RegisterParam) -> u32 {
    emit_ldr_str_register_common(src, addr, index, 0, 0x3820_0800)
}

pub fn emit_strh_register(src: Register, addr: Register, index: RegisterParam) -> u32 {
    emit_ldr_str_register_common(src, addr, index, 1, 0x7820_0800)
}

pub fn emit_str_register(src: Register, addr: Register, index: RegisterParam) -> u32 {
    emit_ldr_str_register_common(src, addr, index, 2, 0xb820_0800)
}

pub fn emit_str_register_64(src: Register, addr: Register, index: RegisterParam) -> u32 {
    emit_ldr_str_register_common(src, addr, index, 3, 0xf820_0800)
}

// ============================================================================
// Section: FP scalar load/store with immediate offset
// ============================================================================
//
// ARM64 FP load/store use the same encoding layout as GP load/store but
// with the V bit (bit 26) set; the 5-bit register field encodes the
// NEON/FP register number (0-31).

fn emit_fp_ldr_str_offset_common(
    fp_reg: NeonRegister,
    addr: Register,
    offset: i32,
    access_shift: u32,
    opcode: u32,
    opcode_unscaled: u32,
) -> Option<u32> {
    if opcode != 0 {
        let enc_offset = (offset as u32) >> access_shift;
        if (((enc_offset << access_shift) as i32) == offset) && (enc_offset & 0xfff) == enc_offset {
            return Some(opcode | ((enc_offset & 0xfff) << 10) | (r(addr) << 5) | (fp_reg.raw() as u32));
        }
    }

    if opcode_unscaled != 0 && offset >= -0x100 && offset <= 0xff {
        return Some(opcode_unscaled | (((offset as u32) & 0x1ff) << 12) | (r(addr) << 5) | (fp_reg.raw() as u32));
    }

    None
}

/// LDR St, [Xn, #imm] — single-precision float load.
pub fn emit_fp_ldr_s_offset(dest: NeonRegister, addr: Register, offset: i32) -> Option<u32> {
    emit_fp_ldr_str_offset_common(dest, addr, offset, 2, 0xBD40_0000, 0xBC40_0000)
}

/// LDR Dt, [Xn, #imm] — double-precision float load.
pub fn emit_fp_ldr_d_offset(dest: NeonRegister, addr: Register, offset: i32) -> Option<u32> {
    emit_fp_ldr_str_offset_common(dest, addr, offset, 3, 0xFD40_0000, 0xFC40_0000)
}

/// STR St, [Xn, #imm] — single-precision float store.
pub fn emit_fp_str_s_offset(src: NeonRegister, addr: Register, offset: i32) -> Option<u32> {
    emit_fp_ldr_str_offset_common(src, addr, offset, 2, 0xBD00_0000, 0xBC00_0000)
}

/// STR Dt, [Xn, #imm] — double-precision float store.
pub fn emit_fp_str_d_offset(src: NeonRegister, addr: Register, offset: i32) -> Option<u32> {
    emit_fp_ldr_str_offset_common(src, addr, offset, 3, 0xFD00_0000, 0xFC00_0000)
}

// ============================================================================
// Section: Load/store pair (LDP/STP)
// ============================================================================

fn emit_ldp_stp_offset_common(
    src_dest1: Register,
    src_dest2: Register,
    addr: Register,
    offset: i32,
    access_shift: u32,
    opcode: u32,
) -> Option<u32> {
    let enc_offset = offset >> access_shift;
    if ((enc_offset << access_shift) == offset) && enc_offset >= -0x40 && enc_offset <= 0x3f {
        return Some(
            opcode
                | (((enc_offset as u32) & 0x7f) << 15)
                | (r(src_dest2) << 10)
                | (r(addr) << 5)
                | r(src_dest1),
        );
    }
    None
}

pub fn emit_ldp_offset(dest1: Register, dest2: Register, addr: Register, offset: i32) -> Option<u32> {
    emit_ldp_stp_offset_common(dest1, dest2, addr, offset, 2, 0x2940_0000)
}

pub fn emit_ldp_offset_64(dest1: Register, dest2: Register, addr: Register, offset: i32) -> Option<u32> {
    emit_ldp_stp_offset_common(dest1, dest2, addr, offset, 3, 0xa940_0000)
}

pub fn emit_ldp_offset_post_index(dest1: Register, dest2: Register, addr: Register, offset: i32) -> Option<u32> {
    if offset == 0 {
        return emit_ldp_offset(dest1, dest2, addr, 0);
    }
    emit_ldp_stp_offset_common(dest1, dest2, addr, offset, 2, 0x28c0_0000)
}

pub fn emit_ldp_offset_post_index_64(dest1: Register, dest2: Register, addr: Register, offset: i32) -> Option<u32> {
    if offset == 0 {
        return emit_ldp_offset_64(dest1, dest2, addr, 0);
    }
    emit_ldp_stp_offset_common(dest1, dest2, addr, offset, 3, 0xa8c0_0000)
}

pub fn emit_stp_offset(src1: Register, src2: Register, addr: Register, offset: i32) -> Option<u32> {
    emit_ldp_stp_offset_common(src1, src2, addr, offset, 2, 0x2900_0000)
}

pub fn emit_stp_offset_64(src1: Register, src2: Register, addr: Register, offset: i32) -> Option<u32> {
    emit_ldp_stp_offset_common(src1, src2, addr, offset, 3, 0xa900_0000)
}

pub fn emit_stp_offset_pre_index(src1: Register, src2: Register, addr: Register, offset: i32) -> Option<u32> {
    if offset == 0 {
        return emit_stp_offset(src1, src2, addr, 0);
    }
    emit_ldp_stp_offset_common(src1, src2, addr, offset, 2, 0x2980_0000)
}

pub fn emit_stp_offset_pre_index_64(src1: Register, src2: Register, addr: Register, offset: i32) -> Option<u32> {
    if offset == 0 {
        return emit_stp_offset_64(src1, src2, addr, 0);
    }
    emit_ldp_stp_offset_common(src1, src2, addr, offset, 3, 0xa980_0000)
}

// ============================================================================
// Section: NEON binary (unary-operand) integer/float operations
// ============================================================================

fn emit_neon_binary_common(
    dest: NeonRegister,
    src: NeonRegister,
    src_size: NeonSize,
    vector_opcode: u32,
    scalar_opcode: u32,
) -> u32 {
    let size_val = src_size as u32;
    if src_size.is_scalar() {
        scalar_opcode | ((size_val & 3) << 22) | (nr(src) << 5) | nr(dest)
    } else {
        vector_opcode | ((src_size.q_bit() as u32) << 30) | ((size_val & 3) << 22) | (nr(src) << 5) | nr(dest)
    }
}

pub fn emit_neon_abs(dest: NeonRegister, src: NeonRegister, size: NeonSize) -> u32 {
    emit_neon_binary_common(dest, src, size, 0x0e20_b800, 0x5e20_b800)
}

pub fn emit_neon_addp_scalar(dest: NeonRegister, src: NeonRegister, size: NeonSize) -> u32 {
    emit_neon_binary_common(dest, src, size, 0, 0x5e31_b800)
}

pub fn emit_neon_addv(dest: NeonRegister, src: NeonRegister, size: NeonSize) -> u32 {
    emit_neon_binary_common(dest, src, size, 0x0e31_b800, 0)
}

pub fn emit_neon_add(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) -> u32 {
    emit_neon_trinary_common(dest, src1, src2, size, 0x0e20_8400, 0x5e20_8400)
}

pub fn emit_neon_neg(dest: NeonRegister, src: NeonRegister, size: NeonSize) -> u32 {
    emit_neon_binary_common(dest, src, size, 0x2e20_b800, 0x7e20_b800)
}

// ============================================================================
// Section: NEON float binary (unary-operand) operations
// ============================================================================

fn emit_neon_float_binary_common(
    dest: NeonRegister,
    src: NeonRegister,
    src_size: NeonSize,
    vector_opcode: u32,
    scalar_opcode: u32,
) -> u32 {
    let size_val = src_size as u32;
    if src_size.is_scalar() {
        scalar_opcode | ((size_val & 1) << 22) | (nr(src) << 5) | nr(dest)
    } else {
        vector_opcode | ((src_size.q_bit() as u32) << 30) | ((size_val & 1) << 22) | (nr(src) << 5) | nr(dest)
    }
}

pub fn emit_neon_fabs(dest: NeonRegister, src: NeonRegister, size: NeonSize) -> u32 {
    emit_neon_float_binary_common(dest, src, size, 0x0ea0_f800, 0x1e20_c000)
}

pub fn emit_neon_faddp_scalar(dest: NeonRegister, src: NeonRegister, size: NeonSize) -> u32 {
    emit_neon_float_binary_common(dest, src, size, 0x7e30_d800, 0)
}

pub fn emit_neon_fcvtzs_gen(dest: Register, src: NeonRegister, sz: NeonSize) -> u32 {
    emit_neon_convert_to_gen_common(dest, src, sz, 0x1e38_0000)
}

pub fn emit_neon_fneg(dest: NeonRegister, src: NeonRegister, size: NeonSize) -> u32 {
    emit_neon_float_binary_common(dest, src, size, 0x2ea0_f800, 0x1e21_4000)
}

/// FMOV Sd, Sn / FMOV Dd, Dn (register-to-register float move, no
/// vector-opcode form — Zig passes `0` for `vector_opcode`, meaning this
/// is scalar-only, exactly like `emitNeonFcmpe`/`emitNeonFrecpx` etc.).
pub fn emit_neon_fmov(dest: NeonRegister, src: NeonRegister, size: NeonSize) -> u32 {
    emit_neon_float_binary_common(dest, src, size, 0, 0x1e20_4000)
}

// ============================================================================
// Section: NEON trinary integer operations (3-register same)
// ============================================================================

fn emit_neon_trinary_common(
    dest: NeonRegister,
    src1: NeonRegister,
    src2: NeonRegister,
    src_size: NeonSize,
    vector_opcode: u32,
    scalar_opcode: u32,
) -> u32 {
    let size_val = src_size as u32;
    if src_size.is_scalar() {
        scalar_opcode | ((size_val & 3) << 22) | (nr(src2) << 16) | (nr(src1) << 5) | nr(dest)
    } else {
        vector_opcode
            | ((src_size.q_bit() as u32) << 30)
            | ((size_val & 3) << 22)
            | (nr(src2) << 16)
            | (nr(src1) << 5)
            | nr(dest)
    }
}

pub fn emit_neon_mla(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) -> u32 {
    emit_neon_trinary_common(dest, src1, src2, size, 0x0e20_9400, 0)
}

pub fn emit_neon_mul(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) -> u32 {
    emit_neon_trinary_common(dest, src1, src2, size, 0x0e20_9c00, 0)
}

pub fn emit_neon_sub(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) -> u32 {
    emit_neon_trinary_common(dest, src1, src2, size, 0x2e20_8400, 0x7e20_8400)
}

pub fn emit_neon_smax(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) -> u32 {
    emit_neon_trinary_common(dest, src1, src2, size, 0x0e20_6400, 0)
}

pub fn emit_neon_smin(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) -> u32 {
    emit_neon_trinary_common(dest, src1, src2, size, 0x0e20_6c00, 0)
}

// ============================================================================
// Section: NEON float trinary operations
// ============================================================================

fn emit_neon_float_trinary_common(
    dest: NeonRegister,
    src1: NeonRegister,
    src2: NeonRegister,
    src_size: NeonSize,
    vector_opcode: u32,
    scalar_opcode: u32,
) -> u32 {
    let size_val = src_size as u32;
    if src_size.is_scalar() {
        scalar_opcode | ((size_val & 1) << 22) | (nr(src2) << 16) | (nr(src1) << 5) | nr(dest)
    } else {
        vector_opcode
            | ((src_size.q_bit() as u32) << 30)
            | ((size_val & 1) << 22)
            | (nr(src2) << 16)
            | (nr(src1) << 5)
            | nr(dest)
    }
}

pub fn emit_neon_fadd(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) -> u32 {
    emit_neon_float_trinary_common(d, s1, s2, sz, 0x0e20_d400, 0x1e20_2800)
}

pub fn emit_neon_faddp(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) -> u32 {
    emit_neon_float_trinary_common(d, s1, s2, sz, 0x2e20_d400, 0)
}

/// FCMPE (scalar only, destination is NZCV flags). `src2`'s NEON register
/// field participates in the encoding; the dest slot is fixed opcode bits
/// and V0 is used only as the Zig source's placeholder `dest` operand for
/// `emitNeonFloatTrinaryCommon` (its value never appears in the output —
/// see the low-5-bits tests).
pub fn emit_neon_fcmpe(src1: NeonRegister, src2: NeonRegister, size: NeonSize) -> u32 {
    emit_neon_float_trinary_common(NeonRegister::V0, src1, src2, size, 0, 0x1e20_2010)
}

pub fn emit_neon_fdiv(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) -> u32 {
    emit_neon_float_trinary_common(d, s1, s2, sz, 0x2e20_fc00, 0x1e20_1800)
}

pub fn emit_neon_fmax(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) -> u32 {
    emit_neon_float_trinary_common(d, s1, s2, sz, 0x0e20_f400, 0x1e20_4800)
}

pub fn emit_neon_fmin(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) -> u32 {
    emit_neon_float_trinary_common(d, s1, s2, sz, 0x0ea0_f400, 0x1e20_5800)
}

pub fn emit_neon_fmla(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) -> u32 {
    emit_neon_float_trinary_common(d, s1, s2, sz, 0x0e20_cc00, 0)
}

pub fn emit_neon_fmul(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) -> u32 {
    emit_neon_float_trinary_common(d, s1, s2, sz, 0x2e20_dc00, 0x1e20_0800)
}

pub fn emit_neon_fsub(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) -> u32 {
    emit_neon_float_trinary_common(d, s1, s2, sz, 0x0ea0_d400, 0x1e20_3800)
}

// ============================================================================
// Section: NEON scalar<->general register transfers
// ============================================================================

fn emit_neon_convert_to_gen_common(dest: Register, src: NeonRegister, src_size: NeonSize, opcode: u32) -> u32 {
    opcode | (((src_size as u32) & 1) << 22) | (nr(src) << 5) | r(dest)
}

fn emit_neon_convert_from_gen_common(dest: NeonRegister, src: Register, dst_size: NeonSize, opcode: u32) -> u32 {
    opcode | (((dst_size as u32) & 1) << 22) | (r(src) << 5) | nr(dest)
}

pub fn emit_neon_fcvtzs_gen_64(dest: Register, src: NeonRegister, sz: NeonSize) -> u32 {
    emit_neon_convert_to_gen_common(dest, src, sz, 0x9e38_0000)
}

pub fn emit_neon_fcvtzu_gen(dest: Register, src: NeonRegister, sz: NeonSize) -> u32 {
    emit_neon_convert_to_gen_common(dest, src, sz, 0x1e39_0000)
}

pub fn emit_neon_fcvtzu_gen_64(dest: Register, src: NeonRegister, sz: NeonSize) -> u32 {
    emit_neon_convert_to_gen_common(dest, src, sz, 0x9e39_0000)
}

/// FMOV to general register.
pub fn emit_neon_fmov_to_general(dest: Register, src: NeonRegister) -> u32 {
    emit_neon_convert_to_gen_common(dest, src, NeonSize::Size1S, 0x1e26_0000)
}

pub fn emit_neon_fmov_to_general_64(dest: Register, src: NeonRegister) -> u32 {
    emit_neon_convert_to_gen_common(dest, src, NeonSize::Size1D, 0x9e26_0000)
}

/// FMOV from general register.
pub fn emit_neon_fmov_from_general(dest: NeonRegister, src: Register) -> u32 {
    emit_neon_convert_from_gen_common(dest, src, NeonSize::Size1S, 0x1e27_0000)
}

pub fn emit_neon_fmov_from_general_64(dest: NeonRegister, src: Register) -> u32 {
    emit_neon_convert_from_gen_common(dest, src, NeonSize::Size1D, 0x9e27_0000)
}

/// SCVTF/UCVTF from general register.
pub fn emit_neon_scvtf_gen(dest: NeonRegister, src: Register, dst_size: NeonSize) -> u32 {
    emit_neon_convert_from_gen_common(dest, src, dst_size, 0x1e22_0000)
}

pub fn emit_neon_scvtf_gen_64(dest: NeonRegister, src: Register, dst_size: NeonSize) -> u32 {
    emit_neon_convert_from_gen_common(dest, src, dst_size, 0x9e22_0000)
}

pub fn emit_neon_ucvtf_gen(dest: NeonRegister, src: Register, dst_size: NeonSize) -> u32 {
    emit_neon_convert_from_gen_common(dest, src, dst_size, 0x1e23_0000)
}

pub fn emit_neon_ucvtf_gen_64(dest: NeonRegister, src: Register, dst_size: NeonSize) -> u32 {
    emit_neon_convert_from_gen_common(dest, src, dst_size, 0x9e23_0000)
}

// ============================================================================
// Section: NEON DUP / SMOV
// ============================================================================

fn emit_neon_mov_element_common(
    dest: NeonRegister,
    src: NeonRegister,
    src_index: u32,
    src_size: NeonSize,
    opcode: u32,
) -> u32 {
    let size = (src_size as u32) & 3;
    let idx = ((src_index << 1) | 1) << size;
    opcode | (idx << 16) | (nr(src) << 5) | nr(dest)
}

/// DUP Vd.T, Rn.
pub fn emit_neon_dup(dest: NeonRegister, src: Register, dest_size: NeonSize) -> u32 {
    let q = dest_size.q_bit() as u32;
    let src_neon = NeonRegister::from_gpr(src);
    emit_neon_mov_element_common(dest, src_neon, 0, dest_size, 0x0e00_0c00 | (q << 30))
}

/// SMOV Wd, Vn.Ts[index] (sign-extending move to 32-bit).
pub fn emit_neon_smov(dest: Register, src: NeonRegister, src_index: u32, src_size: NeonSize) -> u32 {
    let dest_neon = NeonRegister::from_gpr(dest);
    emit_neon_mov_element_common(dest_neon, src, src_index, src_size, 0x0e00_2c00)
}

// ============================================================================
// Section: NEON load/store
// ============================================================================

/// LDR/STR (SIMD&FP, unsigned offset).
fn emit_neon_ldr_str_offset_common(
    src_dest: NeonRegister,
    src_dest_size: NeonSize,
    addr: Register,
    offset: i32,
    opcode: u32,
    opcode_unscaled: u32,
) -> Option<u32> {
    let sz = src_dest_size as u32;
    let size_bits = ((sz & 3) << 30) | ((sz >> 2) << 23);

    if opcode != 0 {
        let shift = src_dest_size as u32;
        let enc_offset = (offset as u32) >> shift;
        if (((enc_offset << shift) as i32) == offset) && (enc_offset & 0xfff) == enc_offset {
            return Some(opcode | size_bits | ((enc_offset & 0xfff) << 10) | (r(addr) << 5) | nr(src_dest));
        }
    }

    if opcode_unscaled != 0 && offset >= -0x100 && offset <= 0xff {
        return Some(opcode_unscaled | size_bits | (((offset as u32) & 0x1ff) << 12) | (r(addr) << 5) | nr(src_dest));
    }

    None
}

pub fn emit_neon_ldr_offset(dest: NeonRegister, dest_size: NeonSize, addr: Register, offset: i32) -> Option<u32> {
    emit_neon_ldr_str_offset_common(dest, dest_size, addr, offset, 0x3d40_0000, 0x3c40_0000)
}

pub fn emit_neon_str_offset(src: NeonRegister, src_size: NeonSize, addr: Register, offset: i32) -> Option<u32> {
    emit_neon_ldr_str_offset_common(src, src_size, addr, offset, 0x3d00_0000, 0x3c00_0000)
}

// ============================================================================
// Section: FMADD/FMSUB (scalar floating-point fused multiply-add/sub)
// ============================================================================
//
// FMADD: dest = src3 + (src1 * src2). FMSUB: dest = src3 - (src1 * src2).
// These are not in the whitelist by name (`emitFmadd` etc. are not listed)
// but are required transitively by nothing in the whitelist either —
// double-checked against the whitelist: none of emitNeonFmla/emitNeonFadd/
// etc. call these. Skipped; not ported.

// ============================================================================
// Tests
// ============================================================================
//
// Representative subset of the Zig `test "..."` blocks: every test that
// exercises a whitelisted `emit*` function (or a required support type),
// reproducing the identical expected `u32` from the Zig source verbatim.
// Tests for non-whitelisted functions (even when built on whitelisted
// helpers, e.g. `emitNeg`/`emitCmnRegister` variants not in the list) are
// skipped, per the port's scope.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nop_encoding() {
        assert_eq!(0xd503201f, emit_nop());
    }

    #[test]
    fn brk_0_encoding() {
        assert_eq!(0xd4200000, emit_brk(0));
    }

    #[test]
    fn ret_lr() {
        assert_eq!(0xd65f03c0, emit_ret(Register::Lr));
    }

    #[test]
    fn add_w0_w1_w2() {
        let expected: u32 = 0x0b000000 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_add_register(Register::R0, Register::R1, RegisterParam::reg_only(Register::R2)));
    }

    #[test]
    fn add_x0_x1_x2() {
        let expected: u32 = 0x8b000000 | (2 << 16) | (1 << 5);
        assert_eq!(
            expected,
            emit_add_register_64(Register::R0, Register::R1, RegisterParam::reg_only(Register::R2))
        );
    }

    #[test]
    fn sub_w3_w4_w5() {
        let expected: u32 = 0x4b000000 | (5 << 16) | (4 << 5) | 3;
        assert_eq!(expected, emit_sub_register(Register::R3, Register::R4, RegisterParam::reg_only(Register::R5)));
    }

    #[test]
    fn and_w0_w1_w2() {
        let expected: u32 = 0x0a000000 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_and_register(Register::R0, Register::R1, RegisterParam::reg_only(Register::R2)));
    }

    #[test]
    fn orr_wzr_w1_is_mov() {
        let expected: u32 = 0x2a000000 | (1 << 16) | (31 << 5);
        assert_eq!(expected, emit_mov_register(Register::R0, Register::R1));
    }

    #[test]
    fn mul_w0_w1_w2() {
        let expected: u32 = 0x1b000000 | (2 << 16) | (31 << 10) | (1 << 5);
        assert_eq!(expected, emit_mul(Register::R0, Register::R1, Register::R2));
    }

    #[test]
    fn sdiv_w0_w1_w2() {
        let expected: u32 = 0x1ac00c00 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_sdiv(Register::R0, Register::R1, Register::R2));
    }

    #[test]
    fn movz_w0_0x1234_lsl0() {
        let expected: u32 = 0x52800000 | (0x1234 << 5);
        assert_eq!(expected, emit_movz(Register::R0, 0x1234, 0));
    }

    #[test]
    fn movk_x0_0x5678_lsl16() {
        let expected: u32 = 0xf2800000 | (1 << 21) | (0x5678 << 5);
        assert_eq!(expected, emit_movk_64(Register::R0, 0x5678, 16));
    }

    #[test]
    fn b_unconditional() {
        assert_eq!(0x14000001, emit_b(1));
    }

    #[test]
    fn bl_offset1() {
        assert_eq!(0x94000001, emit_bl(1));
    }

    #[test]
    fn br_x16() {
        let expected: u32 = 0xd61f0000 | (16 << 5);
        assert_eq!(expected, emit_br(Register::R16));
    }

    #[test]
    fn blr_x16() {
        let expected: u32 = 0xd63f0000 | (16 << 5);
        assert_eq!(expected, emit_blr(Register::R16));
    }

    #[test]
    fn cbz_w0_plus0() {
        assert_eq!(0x34000000, emit_cbz(Register::R0, 0));
    }

    #[test]
    fn cbnz_x1_plus0() {
        assert_eq!(0xb5000001, emit_cbnz_64(Register::R1, 0));
    }

    #[test]
    fn b_eq_plus0() {
        assert_eq!(0x54000000, emit_b_cond(0, Condition::Eq));
    }

    #[test]
    fn csel_w0_w1_w2_eq() {
        let expected: u32 = 0x1a800000 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_csel(Register::R0, Register::R1, Register::R2, Condition::Eq));
    }

    #[test]
    fn cset_w0_eq() {
        // CSET is CSINC W0, WZR, WZR, NE (inverted condition).
        let expected: u32 = 0x1a800400 | (31 << 16) | (1 << 12) | (31 << 5);
        assert_eq!(expected, emit_cset(Register::R0, Condition::Eq));
    }

    #[test]
    fn sxtb_w0_w1() {
        let expected: u32 = 0x13000000 | (7 << 10) | (1 << 5);
        assert_eq!(expected, emit_sxtb(Register::R0, Register::R1));
    }

    #[test]
    fn add_w0_w1_1() {
        let expected: u32 = 0x11000000 | (1 << 10) | (1 << 5);
        assert_eq!(expected, emit_add_immediate(Register::R0, Register::R1, 1));
    }

    #[test]
    fn sub_x0_x1_4() {
        let expected: u32 = 0x80000000 | 0x51000000 | (4 << 10) | (1 << 5);
        assert_eq!(expected, emit_sub_immediate_64(Register::R0, Register::R1, 4));
    }

    #[test]
    fn ldr_w0_x1_0_offset() {
        let expected: u32 = 0xb9400000 | (1 << 5);
        assert_eq!(expected, emit_ldr_offset(Register::R0, Register::R1, 0).unwrap());
    }

    #[test]
    fn str_x0_x1_8_offset() {
        let expected: u32 = 0xf9000000 | (1 << 10) | (1 << 5);
        assert_eq!(expected, emit_str_offset_64(Register::R0, Register::R1, 8).unwrap());
    }

    #[test]
    fn ldp_w0_w1_sp_0() {
        let expected: u32 = 0x29400000 | (1 << 10) | (31 << 5);
        assert_eq!(expected, emit_ldp_offset(Register::R0, Register::R1, Register::Sp, 0).unwrap());
    }

    #[test]
    fn stp_x0_x1_sp_neg16_pre() {
        let result = emit_stp_offset_pre_index_64(Register::R0, Register::R1, Register::Sp, -16);
        assert!(result.is_some());
    }

    #[test]
    fn logical_immediate_encode_0f0f0f0f() {
        let result = encode_logical_immediate(0x0F0F0F0F, 32);
        assert!(result.is_some());
    }

    #[test]
    fn logical_immediate_zero_and_neg1_unencodable() {
        assert_eq!(None, encode_logical_immediate(0, 32));
        assert_eq!(None, encode_logical_immediate(0xFFFFFFFFFFFFFFFF, 64));
    }

    #[test]
    fn neon_add_v0_4s_v1_4s_v2_4s() {
        // NEON ADD isn't in the whitelist directly, but emitNeonMla/emitNeonMul
        // etc. share emitNeonTrinaryCommon; use emitNeonSub (whitelisted)
        // instead to validate the shared helper with the Zig test's shape.
        let expected: u32 = 0x2e208400 | (1 << 30) | (2 << 22) | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_neon_sub(NeonRegister::V0, NeonRegister::V1, NeonRegister::V2, NeonSize::Size4S));
    }

    #[test]
    fn neon_fmul_s0_s1_s2_scalar() {
        let expected: u32 = 0x1e200800 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_neon_fmul(NeonRegister::V0, NeonRegister::V1, NeonRegister::V2, NeonSize::Size1S));
    }

    #[test]
    fn extr_w0_w1_w2_3_via_ror_immediate_shape() {
        // EXTR itself isn't whitelisted; ROR-immediate isn't either. Instead
        // validate emit_sbfm (whitelisted transitively via emit_sxtb etc.)
        // directly using the Zig test's SBFM shape.
        let expected: u32 = 0x13000000 | (2 << 16) | (10 << 10) | (1 << 5);
        assert_eq!(expected, emit_sbfm(Register::R0, Register::R1, 2, 10));
    }

    #[test]
    fn adr_x0_0() {
        assert_eq!(0x10000000, emit_adr(Register::R0, 0));
    }

    #[test]
    fn adrp_x0_0() {
        assert_eq!(0x90000000, emit_adrp(Register::R0, 0));
    }

    #[test]
    fn condition_invert() {
        assert_eq!(Condition::Ne, Condition::Eq.invert());
        assert_eq!(Condition::Eq, Condition::Ne.invert());
        assert_eq!(Condition::Cc, Condition::Cs.invert());
        assert_eq!(Condition::Lt, Condition::Ge.invert());
    }

    #[test]
    fn load_immediate32_simple() {
        let mut buf = [0u32; 2];
        let n = emit_load_immediate_32(Register::R0, 42, &mut buf);
        assert_eq!(1, n);
        assert_eq!(0x52800000 | (42 << 5), buf[0]);
    }

    #[test]
    fn load_immediate32_two_parts() {
        let mut buf = [0u32; 2];
        let n = emit_load_immediate_32(Register::R0, 0x12340001, &mut buf);
        assert_eq!(2, n);
    }

    #[test]
    fn neon_shl_v0_4s_v1_4s_3_via_dup_shape() {
        // NEON SHL isn't whitelisted; validate NeonSize/qBit machinery
        // instead via a whitelisted vector op (emit_neon_smax) matching
        // the Zig test's V0/V1/V2 4S shape.
        let expected: u32 = 0x0e206400 | (1 << 30) | (2 << 22) | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_neon_smax(NeonRegister::V0, NeonRegister::V1, NeonRegister::V2, NeonSize::Size4S));
    }

    #[test]
    fn neon_size_methods() {
        assert!(NeonSize::Size1S.is_scalar());
        assert!(!NeonSize::Size4S.is_scalar());
        assert!(NeonSize::Size4S.is_vector());
        assert!(!NeonSize::Size1D.is_vector());
        assert_eq!(2, NeonSize::Size1S.element_size());
        assert_eq!(2, NeonSize::Size4S.element_size());
    }

    // --- Register enum ---

    #[test]
    fn register_zr_alias_equals_sp() {
        assert_eq!(Register::Sp, Register::ZR);
        assert_eq!(31, Register::ZR.raw());
    }

    #[test]
    fn register_raw_values() {
        assert_eq!(0, Register::R0.raw());
        assert_eq!(29, Register::Fp.raw());
        assert_eq!(30, Register::Lr.raw());
        assert_eq!(31, Register::Sp.raw());
    }

    #[test]
    fn neon_register_raw_values() {
        assert_eq!(0, NeonRegister::V0.raw());
        assert_eq!(15, NeonRegister::V15.raw());
        assert_eq!(31, NeonRegister::V31.raw());
    }

    // --- RegisterParam ---

    #[test]
    fn register_param_reg_only() {
        let p = RegisterParam::reg_only(Register::R5);
        assert!(p.is_reg_only());
        assert!(!p.is_extended());
    }

    #[test]
    fn register_param_shifted() {
        let p = RegisterParam::shifted(Register::R3, ShiftType::Lsl, 2);
        assert!(!p.is_reg_only());
        assert!(!p.is_extended());
        let enc = p.encode();
        assert_eq!((0u32 << 22) | (3 << 16) | (2 << 10), enc);
    }

    #[test]
    fn register_param_extended() {
        let p = RegisterParam::extended(Register::R4, ExtendType::Sxtw, 0);
        assert!(!p.is_reg_only());
        assert!(p.is_extended());
        let enc = p.encode_extended();
        assert_eq!((4u32 << 16) | (6 << 13), enc);
    }

    // --- Condition invert (all pairs) ---

    #[test]
    fn condition_invert_all_pairs() {
        assert_eq!(Condition::Ne, Condition::Eq.invert());
        assert_eq!(Condition::Eq, Condition::Ne.invert());
        assert_eq!(Condition::Cc, Condition::Cs.invert());
        assert_eq!(Condition::Cs, Condition::Cc.invert());
        assert_eq!(Condition::Pl, Condition::Mi.invert());
        assert_eq!(Condition::Mi, Condition::Pl.invert());
        assert_eq!(Condition::Vc, Condition::Vs.invert());
        assert_eq!(Condition::Vs, Condition::Vc.invert());
        assert_eq!(Condition::Ls, Condition::Hi.invert());
        assert_eq!(Condition::Hi, Condition::Ls.invert());
        assert_eq!(Condition::Lt, Condition::Ge.invert());
        assert_eq!(Condition::Ge, Condition::Lt.invert());
        assert_eq!(Condition::Le, Condition::Gt.invert());
        assert_eq!(Condition::Gt, Condition::Le.invert());
    }

    // --- NeonSize ---

    #[test]
    fn neon_size_q_bit() {
        assert_eq!(0, NeonSize::Size1B.q_bit());
        assert_eq!(1, NeonSize::Size1Q.q_bit());
        assert_eq!(0, NeonSize::Size8B.q_bit());
        assert_eq!(1, NeonSize::Size16B.q_bit());
        assert_eq!(0, NeonSize::Size2S.q_bit());
        assert_eq!(1, NeonSize::Size4S.q_bit());
        assert_eq!(1, NeonSize::Size2D.q_bit());
    }

    #[test]
    fn neon_size_element_size() {
        assert_eq!(0, NeonSize::Size1B.element_size());
        assert_eq!(1, NeonSize::Size1H.element_size());
        assert_eq!(2, NeonSize::Size1S.element_size());
        assert_eq!(3, NeonSize::Size1D.element_size());
        assert_eq!(0, NeonSize::Size8B.element_size());
        assert_eq!(0, NeonSize::Size16B.element_size());
        assert_eq!(1, NeonSize::Size4H.element_size());
        assert_eq!(1, NeonSize::Size8H.element_size());
        assert_eq!(3, NeonSize::Size2D.element_size());
    }

    // --- Branch instructions ---

    #[test]
    fn b_negative_offset() {
        let result = emit_b(-1);
        assert_eq!(0x14000000 | 0x03ffffff, result);
    }

    #[test]
    fn b_ne_plus0() {
        let expected: u32 = 0x54000000 | (Condition::Ne as u32);
        assert_eq!(expected, emit_b_cond(0, Condition::Ne));
    }

    #[test]
    fn b_gt_with_offset() {
        let expected: u32 = 0x54000000 | (5 << 5) | (Condition::Gt as u32);
        assert_eq!(expected, emit_b_cond(5, Condition::Gt));
    }

    #[test]
    fn cbz_x0_plus0() {
        assert_eq!(0xb4000000, emit_cbz_64(Register::R0, 0));
    }

    #[test]
    fn cbnz_w0_plus0() {
        assert_eq!(0x35000000, emit_cbnz(Register::R0, 0));
    }

    #[test]
    fn ret_r0() {
        let expected: u32 = 0xd65f0000;
        assert_eq!(expected, emit_ret(Register::R0));
    }

    // --- ADD/SUB register with shift ---

    #[test]
    fn add_w0_w1_w2_lsl3() {
        let expected: u32 = 0x0b000000 | (2 << 16) | (3 << 10) | (1 << 5);
        assert_eq!(
            expected,
            emit_add_register(Register::R0, Register::R1, RegisterParam::shifted(Register::R2, ShiftType::Lsl, 3))
        );
    }

    #[test]
    fn sub_x0_x1_x2_asr5() {
        let expected: u32 = 0xcb000000 | (2 << 22) | (2 << 16) | (5 << 10) | (1 << 5);
        assert_eq!(
            expected,
            emit_sub_register_64(Register::R0, Register::R1, RegisterParam::shifted(Register::R2, ShiftType::Asr, 5))
        );
    }

    #[test]
    fn add_x0_x1_w2_sxtw() {
        let expected: u32 = 0x8b200000 | (2 << 16) | (6 << 13) | (1 << 5);
        assert_eq!(
            expected,
            emit_add_register_64(
                Register::R0,
                Register::R1,
                RegisterParam::extended(Register::R2, ExtendType::Sxtw, 0)
            )
        );
    }

    // --- CMP register ---

    #[test]
    fn cmp_w1_w2() {
        let expected: u32 = 0x6b000000 | (2 << 16) | (1 << 5) | 31;
        assert_eq!(expected, emit_cmp_register(Register::R1, RegisterParam::reg_only(Register::R2)));
    }

    #[test]
    fn cmp_x1_x2() {
        let expected: u32 = 0xeb000000 | (2 << 16) | (1 << 5) | 31;
        assert_eq!(expected, emit_cmp_register_64(Register::R1, RegisterParam::reg_only(Register::R2)));
    }

    // --- MADD/MSUB ---

    #[test]
    fn madd_w0_w1_w2_w3() {
        let expected: u32 = 0x1b000000 | (2 << 16) | (3 << 10) | (1 << 5);
        assert_eq!(expected, emit_madd(Register::R0, Register::R1, Register::R2, Register::R3));
    }

    #[test]
    fn madd_x0_x1_x2_x3() {
        let expected: u32 = 0x9b000000 | (2 << 16) | (3 << 10) | (1 << 5);
        assert_eq!(expected, emit_madd_64(Register::R0, Register::R1, Register::R2, Register::R3));
    }

    #[test]
    fn msub_w0_w1_w2_w3() {
        let expected: u32 = 0x1b008000 | (2 << 16) | (3 << 10) | (1 << 5);
        assert_eq!(expected, emit_msub(Register::R0, Register::R1, Register::R2, Register::R3));
    }

    #[test]
    fn msub_x0_x1_x2_x3() {
        let expected: u32 = 0x9b008000 | (2 << 16) | (3 << 10) | (1 << 5);
        assert_eq!(expected, emit_msub_64(Register::R0, Register::R1, Register::R2, Register::R3));
    }

    #[test]
    fn mul_x0_x1_x2() {
        let expected: u32 = 0x9b000000 | (2 << 16) | (31 << 10) | (1 << 5);
        assert_eq!(expected, emit_mul_64(Register::R0, Register::R1, Register::R2));
    }

    // --- Division ---

    #[test]
    fn sdiv_x0_x1_x2() {
        let expected: u32 = 0x9ac00c00 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_sdiv_64(Register::R0, Register::R1, Register::R2));
    }

    #[test]
    fn udiv_w0_w1_w2() {
        let expected: u32 = 0x1ac00800 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_udiv(Register::R0, Register::R1, Register::R2));
    }

    #[test]
    fn udiv_x0_x1_x2() {
        let expected: u32 = 0x9ac00800 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_udiv_64(Register::R0, Register::R1, Register::R2));
    }

    // --- Logical register ---

    #[test]
    fn and_x0_x1_x2() {
        let expected: u32 = 0x8a000000 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_and_register_64(Register::R0, Register::R1, RegisterParam::reg_only(Register::R2)));
    }

    #[test]
    fn eor_w0_w1_w2() {
        let expected: u32 = 0x4a000000 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_eor_register(Register::R0, Register::R1, RegisterParam::reg_only(Register::R2)));
    }

    #[test]
    fn eor_x0_x1_x2() {
        let expected: u32 = 0xca000000 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_eor_register_64(Register::R0, Register::R1, RegisterParam::reg_only(Register::R2)));
    }

    #[test]
    fn orr_x0_x1_x2() {
        let expected: u32 = 0xaa000000 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_orr_register_64(Register::R0, Register::R1, RegisterParam::reg_only(Register::R2)));
    }

    #[test]
    fn tst_w1_w2() {
        let expected: u32 = 0x6a000000 | (2 << 16) | (1 << 5) | 31;
        assert_eq!(expected, emit_test_register(Register::R1, RegisterParam::reg_only(Register::R2)));
    }

    #[test]
    fn mov_x0_x1() {
        let expected: u32 = 0xaa000000 | (1 << 16) | (31 << 5);
        assert_eq!(expected, emit_mov_register_64(Register::R0, Register::R1));
    }

    // --- Shift register (variable) ---

    #[test]
    fn asr_w0_w1_w2() {
        let expected: u32 = 0x1ac02800 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_asr_register(Register::R0, Register::R1, Register::R2));
    }

    #[test]
    fn lsl_w0_w1_w2() {
        let expected: u32 = 0x1ac02000 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_lsl_register(Register::R0, Register::R1, Register::R2));
    }

    #[test]
    fn lsr_x0_x1_x2() {
        let expected: u32 = 0x9ac02400 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_lsr_register_64(Register::R0, Register::R1, Register::R2));
    }

    // --- Bitfield ---

    #[test]
    fn sbfm_w0_w1_2_10() {
        let expected: u32 = 0x13000000 | (2 << 16) | (10 << 10) | (1 << 5);
        assert_eq!(expected, emit_sbfm(Register::R0, Register::R1, 2, 10));
    }

    #[test]
    fn ubfm_w0_w1_5_20() {
        let expected: u32 = 0x53000000 | (5 << 16) | (20 << 10) | (1 << 5);
        assert_eq!(expected, emit_ubfm(Register::R0, Register::R1, 5, 20));
    }

    #[test]
    fn sxtb_x0_w1() {
        let expected: u32 = 0x93400000 | (7 << 10) | (1 << 5);
        assert_eq!(expected, emit_sxtb_64(Register::R0, Register::R1));
    }

    #[test]
    fn sxth_w0_w1() {
        let expected: u32 = 0x13000000 | (15 << 10) | (1 << 5);
        assert_eq!(expected, emit_sxth(Register::R0, Register::R1));
    }

    #[test]
    fn uxtb_w0_w1() {
        let expected: u32 = 0x53000000 | (7 << 10) | (1 << 5);
        assert_eq!(expected, emit_uxtb(Register::R0, Register::R1));
    }

    #[test]
    fn uxth_w0_w1() {
        let expected: u32 = 0x53000000 | (15 << 10) | (1 << 5);
        assert_eq!(expected, emit_uxth(Register::R0, Register::R1));
    }

    // --- Shift/rotate immediate ---

    #[test]
    fn asr_w0_w1_5() {
        let expected: u32 = 0x13000000 | (5 << 16) | (31 << 10) | (1 << 5);
        assert_eq!(expected, emit_asr_immediate(Register::R0, Register::R1, 5));
    }

    #[test]
    fn lsr_w0_w1_3() {
        let expected: u32 = 0x53000000 | (3 << 16) | (31 << 10) | (1 << 5);
        assert_eq!(expected, emit_lsr_immediate(Register::R0, Register::R1, 3));
    }

    #[test]
    fn lsr_x0_x1_10() {
        let expected: u32 = 0xd3400000 | (10 << 16) | (63 << 10) | (1 << 5);
        assert_eq!(expected, emit_lsr_immediate_64(Register::R0, Register::R1, 10));
    }

    // --- Conditional select (more variants) ---

    #[test]
    fn csel_x0_x1_x2_ne() {
        let expected: u32 = 0x9a800000 | (2 << 16) | (1 << 12) | (1 << 5);
        assert_eq!(expected, emit_csel_64(Register::R0, Register::R1, Register::R2, Condition::Ne));
    }

    #[test]
    fn cset_x0_ge() {
        // CSINC X0, XZR, XZR, LT (inverted).
        let expected: u32 = 0x9a800400 | (31 << 16) | (11 << 12) | (31 << 5);
        assert_eq!(expected, emit_cset_64(Register::R0, Condition::Ge));
    }

    // --- Move immediate ---

    #[test]
    fn movn_w0_0x1234_lsl0() {
        let expected: u32 = 0x12800000 | (0x1234 << 5);
        assert_eq!(expected, emit_movn(Register::R0, 0x1234, 0));
    }

    #[test]
    fn movn_x0_0_lsl0() {
        let expected: u32 = 0x92800000;
        assert_eq!(expected, emit_movn_64(Register::R0, 0, 0));
    }

    #[test]
    fn movz_x0_0xabcd_lsl48() {
        let expected: u32 = 0xd2800000 | (3 << 21) | (0xABCD << 5);
        assert_eq!(expected, emit_movz_64(Register::R0, 0xABCD, 48));
    }

    // --- LoadImmediate32 ---

    #[test]
    fn load_immediate32_zero() {
        let mut buf = [0u32; 2];
        let n = emit_load_immediate_32(Register::R0, 0, &mut buf);
        assert_eq!(1, n);
        assert_eq!(0x52800000, buf[0]);
    }

    #[test]
    fn load_immediate32_0xffff0000_movz_upper_half_only() {
        let mut buf = [0u32; 2];
        let n = emit_load_immediate_32(Register::R0, 0xFFFF0000, &mut buf);
        assert_eq!(1, n);
    }

    #[test]
    fn load_immediate32_0xffffffff_uses_movn() {
        let mut buf = [0u32; 2];
        let n = emit_load_immediate_32(Register::R0, 0xFFFFFFFF, &mut buf);
        assert_eq!(1, n);
        assert_eq!(0x12800000, buf[0]);
    }

    // --- LoadImmediate64 ---

    #[test]
    fn load_immediate64_small_value() {
        let mut buf = [0u32; 4];
        let n = emit_load_immediate_64(Register::R0, 100, &mut buf);
        assert_eq!(1, n);
        assert_eq!(0x52800000 | (100 << 5), buf[0]);
    }

    #[test]
    fn load_immediate64_many_0xffff_halves() {
        let mut buf = [0u32; 4];
        let n = emit_load_immediate_64(Register::R0, 0xFFFFFFFF00000001, &mut buf);
        assert!((1..=4).contains(&n));
    }

    // --- ADD/SUB immediate ---

    #[test]
    fn add_x0_x1_0() {
        let expected: u32 = 0x80000000 | 0x11000000 | (1 << 5);
        assert_eq!(expected, emit_add_immediate_64(Register::R0, Register::R1, 0));
    }

    #[test]
    fn cmp_w0_42() {
        let expected: u32 = (1 << 29) | 0x51000000 | (42 << 10) | 31;
        assert_eq!(expected, emit_cmp_immediate(Register::R0, 42));
    }

    #[test]
    fn cmp_x0_1() {
        let expected: u32 = 0x80000000 | (1 << 29) | 0x51000000 | (1 << 10) | 31;
        assert_eq!(expected, emit_cmp_immediate_64(Register::R0, 1));
    }

    // --- Logical immediate ---

    #[test]
    fn and_w0_w1_0x0f0f0f0f() {
        let result = emit_and_immediate(Register::R0, Register::R1, 0x0F0F0F0F);
        assert!(result.is_some());
    }

    #[test]
    fn orr_w0_w1_0x00ff00ff() {
        let result = emit_orr_immediate(Register::R0, Register::R1, 0x00FF00FF);
        assert!(result.is_some());
    }

    #[test]
    fn eor_x0_x1_0x5555_pattern() {
        let result = emit_eor_immediate_64(Register::R0, Register::R1, 0x5555555555555555);
        assert!(result.is_some());
    }

    #[test]
    fn tst_w0_0xff() {
        let result = emit_test_immediate(Register::R0, 0xFF);
        assert!(result.is_some());
    }

    #[test]
    fn logical_immediate_unencodable_value() {
        let result = emit_and_immediate(Register::R0, Register::R1, 0x12345678);
        assert_eq!(None, result);
    }

    #[test]
    fn logical_immediate_encode_known_patterns() {
        assert!(encode_logical_immediate(0x5555555555555555, 64).is_some());
        assert!(encode_logical_immediate(0xAAAAAAAAAAAAAAAA, 64).is_some());
        assert!(encode_logical_immediate(0x1, 64).is_some());
        assert!(encode_logical_immediate(0x80000000, 32).is_some());
    }

    #[test]
    fn can_encode_logical_immediate_cases() {
        assert!(can_encode_logical_immediate(0xFF, 32));
        assert!(!can_encode_logical_immediate(0, 32));
        assert!(!can_encode_logical_immediate(0xFFFFFFFFFFFFFFFF, 64));
    }

    // --- Load/store register offset ---

    #[test]
    fn ldrb_w0_x1_x2() {
        let result = emit_ldrb_register(Register::R0, Register::R1, RegisterParam::reg_only(Register::R2));
        assert_eq!(0x38600800 | (2 << 16) | (3 << 13) | (1 << 5), result);
    }

    #[test]
    fn strb_w0_x1_x2() {
        let result = emit_strb_register(Register::R0, Register::R1, RegisterParam::reg_only(Register::R2));
        assert_eq!(0x38200800 | (2 << 16) | (3 << 13) | (1 << 5), result);
    }

    #[test]
    fn ldr_x0_x1_x2() {
        let result = emit_ldr_register_64(Register::R0, Register::R1, RegisterParam::reg_only(Register::R2));
        assert_eq!(0xf8600800 | (2 << 16) | (3 << 13) | (1 << 5), result);
    }

    #[test]
    fn str_w0_x1_x2() {
        let result = emit_str_register(Register::R0, Register::R1, RegisterParam::reg_only(Register::R2));
        assert_eq!(0xb8200800 | (2 << 16) | (3 << 13) | (1 << 5), result);
    }

    // --- Load/store offset ---

    #[test]
    fn ldrb_w0_x1_5() {
        let result = emit_ldrb_offset(Register::R0, Register::R1, 5);
        assert!(result.is_some());
        assert_eq!(0x39400000 | (5 << 10) | (1 << 5), result.unwrap());
    }

    #[test]
    fn strb_w0_x1_0() {
        let result = emit_strb_offset(Register::R0, Register::R1, 0);
        assert!(result.is_some());
        assert_eq!(0x39000000 | (1 << 5), result.unwrap());
    }

    #[test]
    fn ldrh_w0_x1_0() {
        let result = emit_ldrh_offset(Register::R0, Register::R1, 0);
        assert!(result.is_some());
        assert_eq!(0x79400000 | (1 << 5), result.unwrap());
    }

    #[test]
    fn strh_w0_x1_2() {
        let result = emit_strh_offset(Register::R0, Register::R1, 2);
        assert!(result.is_some());
        assert_eq!(0x79000000 | (1 << 10) | (1 << 5), result.unwrap());
    }

    #[test]
    fn ldr_x0_x1_16() {
        let result = emit_ldr_offset_64(Register::R0, Register::R1, 16);
        assert!(result.is_some());
        assert_eq!(0xf9400000 | (2 << 10) | (1 << 5), result.unwrap());
    }

    #[test]
    fn str_w0_x1_4() {
        let result = emit_str_offset(Register::R0, Register::R1, 4);
        assert!(result.is_some());
        assert_eq!(0xb9000000 | (1 << 10) | (1 << 5), result.unwrap());
    }

    #[test]
    fn ldr_offset_unaligned_falls_back_to_unscaled() {
        let result = emit_ldr_offset(Register::R0, Register::R1, 3);
        assert!(result.is_some());
        assert_eq!(0xb8400000 | (3 << 12) | (1 << 5), result.unwrap());
    }

    #[test]
    fn ldr_offset_negative_uses_unscaled() {
        let result = emit_ldr_offset(Register::R0, Register::R1, -4);
        assert!(result.is_some());
    }

    #[test]
    fn ldr_offset_too_large_returns_none() {
        let result = emit_ldr_offset(Register::R0, Register::R1, 0x10000);
        assert_eq!(None, result);
    }

    // --- Load/store pair ---

    #[test]
    fn ldp_x0_x1_sp_16() {
        let result = emit_ldp_offset_64(Register::R0, Register::R1, Register::Sp, 16);
        assert!(result.is_some());
        let expected: u32 = 0xa9400000 | ((2 & 0x7f) << 15) | (1 << 10) | (31 << 5);
        assert_eq!(expected, result.unwrap());
    }

    #[test]
    fn stp_w0_w1_sp_8() {
        let result = emit_stp_offset(Register::R0, Register::R1, Register::Sp, 8);
        assert!(result.is_some());
        let expected: u32 = 0x29000000 | ((2 & 0x7f) << 15) | (1 << 10) | (31 << 5);
        assert_eq!(expected, result.unwrap());
    }

    #[test]
    fn ldp_post_index() {
        let result = emit_ldp_offset_post_index_64(Register::R0, Register::R1, Register::Sp, 16);
        assert!(result.is_some());
    }

    #[test]
    fn stp_pre_index_32() {
        let result = emit_stp_offset_pre_index(Register::R0, Register::R1, Register::Sp, -8);
        assert!(result.is_some());
    }

    #[test]
    fn ldp_offset_out_of_range_returns_none() {
        let result = emit_ldp_offset_64(Register::R0, Register::R1, Register::Sp, 1024);
        assert_eq!(None, result);
    }

    // --- Post/pre-index load/store ---

    #[test]
    fn ldr_post_index_32() {
        let result = emit_ldp_offset_post_index(Register::R0, Register::R1, Register::Sp, 4);
        assert!(result.is_some());
    }

    // --- ADR/ADRP ---

    #[test]
    fn adr_x1_4() {
        let expected: u32 = 0x10000000 | (1 << 5) | 1;
        assert_eq!(expected, emit_adr(Register::R1, 4));
    }

    #[test]
    fn adr_x0_3() {
        let expected: u32 = 0x10000000 | (3 << 29);
        assert_eq!(expected, emit_adr(Register::R0, 3));
    }

    #[test]
    fn adrp_x1_0() {
        let expected: u32 = 0x90000000 | 1;
        assert_eq!(expected, emit_adrp(Register::R1, 0));
    }

    // --- NEON unary integer ---

    #[test]
    fn neon_abs_v0_4s_v1_4s() {
        let expected: u32 = 0x0e20b800 | (1 << 30) | (2 << 22) | (1 << 5);
        assert_eq!(expected, emit_neon_abs(NeonRegister::V0, NeonRegister::V1, NeonSize::Size4S));
    }

    #[test]
    fn neon_neg_v0_4s_v1_4s() {
        let expected: u32 = 0x2e20b800 | (1 << 30) | (2 << 22) | (1 << 5);
        assert_eq!(expected, emit_neon_neg(NeonRegister::V0, NeonRegister::V1, NeonSize::Size4S));
    }

    // --- NEON trinary integer ---

    #[test]
    fn neon_sub_v0_4s_v1_4s_v2_4s() {
        let expected: u32 = 0x2e208400 | (1 << 30) | (2 << 22) | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_neon_sub(NeonRegister::V0, NeonRegister::V1, NeonRegister::V2, NeonSize::Size4S));
    }

    #[test]
    fn neon_mul_v0_4s_v1_4s_v2_4s() {
        let expected: u32 = 0x0e209c00 | (1 << 30) | (2 << 22) | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_neon_mul(NeonRegister::V0, NeonRegister::V1, NeonRegister::V2, NeonSize::Size4S));
    }

    // --- NEON float unary ---

    #[test]
    fn neon_fabs_s0_s1() {
        let expected: u32 = 0x1e20c000 | (1 << 5);
        assert_eq!(expected, emit_neon_fabs(NeonRegister::V0, NeonRegister::V1, NeonSize::Size1S));
    }

    #[test]
    fn neon_fabs_d0_d1() {
        let expected: u32 = 0x1e20c000 | (1 << 22) | (1 << 5);
        assert_eq!(expected, emit_neon_fabs(NeonRegister::V0, NeonRegister::V1, NeonSize::Size1D));
    }

    #[test]
    fn neon_fneg_s0_s1() {
        let expected: u32 = 0x1e214000 | (1 << 5);
        assert_eq!(expected, emit_neon_fneg(NeonRegister::V0, NeonRegister::V1, NeonSize::Size1S));
    }

    // --- NEON float trinary ---

    #[test]
    fn neon_fadd_s0_s1_s2() {
        let expected: u32 = 0x1e202800 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_neon_fadd(NeonRegister::V0, NeonRegister::V1, NeonRegister::V2, NeonSize::Size1S));
    }

    #[test]
    fn neon_fsub_s0_s1_s2() {
        let expected: u32 = 0x1e203800 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_neon_fsub(NeonRegister::V0, NeonRegister::V1, NeonRegister::V2, NeonSize::Size1S));
    }

    #[test]
    fn neon_fdiv_d0_d1_d2() {
        let expected: u32 = 0x1e201800 | (1 << 22) | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_neon_fdiv(NeonRegister::V0, NeonRegister::V1, NeonRegister::V2, NeonSize::Size1D));
    }

    #[test]
    fn neon_fmax_s0_s1_s2() {
        let expected: u32 = 0x1e204800 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_neon_fmax(NeonRegister::V0, NeonRegister::V1, NeonRegister::V2, NeonSize::Size1S));
    }

    #[test]
    fn neon_fmin_s0_s1_s2() {
        let expected: u32 = 0x1e205800 | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_neon_fmin(NeonRegister::V0, NeonRegister::V1, NeonRegister::V2, NeonSize::Size1S));
    }

    #[test]
    fn neon_fadd_v0_4s_v1_4s_v2_4s_vector() {
        let expected: u32 = 0x0e20d400 | (1 << 30) | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_neon_fadd(NeonRegister::V0, NeonRegister::V1, NeonRegister::V2, NeonSize::Size4S));
    }

    #[test]
    fn neon_fmul_v0_2d_v1_2d_v2_2d_vector() {
        let expected: u32 = 0x2e20dc00 | (1 << 30) | (1 << 22) | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_neon_fmul(NeonRegister::V0, NeonRegister::V1, NeonRegister::V2, NeonSize::Size2D));
    }

    // --- Comprehensive FCMPE tests ---

    #[test]
    fn neon_fcmpe_s0_s1() {
        let expected: u32 = 0x1e202010 | (1 << 16);
        assert_eq!(expected, emit_neon_fcmpe(NeonRegister::V0, NeonRegister::V1, NeonSize::Size1S));
    }

    #[test]
    fn neon_fcmpe_d0_d1() {
        let expected: u32 = 0x1e202010 | (1 << 22) | (1 << 16);
        assert_eq!(expected, emit_neon_fcmpe(NeonRegister::V0, NeonRegister::V1, NeonSize::Size1D));
    }

    #[test]
    fn neon_fcmpe_s3_s7() {
        let expected: u32 = 0x1e202010 | (7 << 16) | (3 << 5);
        assert_eq!(expected, emit_neon_fcmpe(NeonRegister::V3, NeonRegister::V7, NeonSize::Size1S));
    }

    #[test]
    fn neon_fcmpe_d15_d20() {
        let expected: u32 = 0x1e202010 | (1 << 22) | (20 << 16) | (15 << 5);
        assert_eq!(expected, emit_neon_fcmpe(NeonRegister::V15, NeonRegister::V20, NeonSize::Size1D));
    }

    #[test]
    fn neon_fcmpe_d28_d29_jit_high_registers() {
        let expected: u32 = 0x1e202010 | (1 << 22) | (29 << 16) | (28 << 5);
        assert_eq!(expected, emit_neon_fcmpe(NeonRegister::V28, NeonRegister::V29, NeonSize::Size1D));
    }

    #[test]
    fn neon_fcmpe_d29_d30_jit_high_registers() {
        let expected: u32 = 0x1e202010 | (1 << 22) | (30 << 16) | (29 << 5);
        assert_eq!(expected, emit_neon_fcmpe(NeonRegister::V29, NeonRegister::V30, NeonSize::Size1D));
    }

    #[test]
    fn neon_fcmpe_s28_s29_jit_high_registers_single() {
        let expected: u32 = 0x1e202010 | (29 << 16) | (28 << 5);
        assert_eq!(expected, emit_neon_fcmpe(NeonRegister::V28, NeonRegister::V29, NeonSize::Size1S));
    }

    #[test]
    fn neon_fcmpe_d0_d30() {
        let expected: u32 = 0x1e202010 | (1 << 22) | (30 << 16);
        assert_eq!(expected, emit_neon_fcmpe(NeonRegister::V0, NeonRegister::V30, NeonSize::Size1D));
    }

    #[test]
    fn neon_fcmpe_d31_low5_bits() {
        let word = emit_neon_fcmpe(NeonRegister::V5, NeonRegister::V10, NeonSize::Size1D);
        assert_eq!(0b10000, word & 0x1F);
    }

    // --- FP scalar LDR/STR offset tests ---

    #[test]
    fn fp_ldr_s0_x1_0() {
        let result = emit_fp_ldr_s_offset(NeonRegister::V0, Register::R1, 0);
        assert!(result.is_some());
        let expected: u32 = 0xBD400000 | (1 << 5);
        assert_eq!(expected, result.unwrap());
    }

    #[test]
    fn fp_ldr_s0_x1_4() {
        let result = emit_fp_ldr_s_offset(NeonRegister::V0, Register::R1, 4);
        assert!(result.is_some());
        let expected: u32 = 0xBD400000 | (1 << 10) | (1 << 5);
        assert_eq!(expected, result.unwrap());
    }

    #[test]
    fn fp_ldr_d0_x1_0() {
        let result = emit_fp_ldr_d_offset(NeonRegister::V0, Register::R1, 0);
        assert!(result.is_some());
        let expected: u32 = 0xFD400000 | (1 << 5);
        assert_eq!(expected, result.unwrap());
    }

    #[test]
    fn fp_ldr_d0_x1_8() {
        let result = emit_fp_ldr_d_offset(NeonRegister::V0, Register::R1, 8);
        assert!(result.is_some());
        let expected: u32 = 0xFD400000 | (1 << 10) | (1 << 5);
        assert_eq!(expected, result.unwrap());
    }

    #[test]
    fn fp_ldr_d0_x1_16() {
        let result = emit_fp_ldr_d_offset(NeonRegister::V0, Register::R1, 16);
        assert!(result.is_some());
        let expected: u32 = 0xFD400000 | (2 << 10) | (1 << 5);
        assert_eq!(expected, result.unwrap());
    }

    #[test]
    fn fp_ldr_d5_x10_0() {
        let result = emit_fp_ldr_d_offset(NeonRegister::V5, Register::R10, 0);
        assert!(result.is_some());
        let expected: u32 = 0xFD400000 | (10 << 5) | 5;
        assert_eq!(expected, result.unwrap());
    }

    #[test]
    fn fp_str_s0_x1_0() {
        let result = emit_fp_str_s_offset(NeonRegister::V0, Register::R1, 0);
        assert!(result.is_some());
        let expected: u32 = 0xBD000000 | (1 << 5);
        assert_eq!(expected, result.unwrap());
    }

    #[test]
    fn fp_str_d0_x1_0() {
        let result = emit_fp_str_d_offset(NeonRegister::V0, Register::R1, 0);
        assert!(result.is_some());
        let expected: u32 = 0xFD000000 | (1 << 5);
        assert_eq!(expected, result.unwrap());
    }

    #[test]
    fn fp_str_d0_x1_8() {
        let result = emit_fp_str_d_offset(NeonRegister::V0, Register::R1, 8);
        assert!(result.is_some());
        let expected: u32 = 0xFD000000 | (1 << 10) | (1 << 5);
        assert_eq!(expected, result.unwrap());
    }

    #[test]
    fn fp_ldr_d_unaligned_offset_uses_unscaled() {
        let result = emit_fp_ldr_d_offset(NeonRegister::V0, Register::R1, 5);
        assert!(result.is_some());
        let expected: u32 = 0xFC400000 | ((5 & 0x1ff) << 12) | (1 << 5);
        assert_eq!(expected, result.unwrap());
    }

    #[test]
    fn fp_ldr_d_negative_offset_uses_unscaled() {
        let result = emit_fp_ldr_d_offset(NeonRegister::V0, Register::R1, -8);
        assert!(result.is_some());
        let neg8_u9: u32 = (-8i32 as u32) & 0x1ff;
        let expected: u32 = 0xFC400000 | (neg8_u9 << 12) | (1 << 5);
        assert_eq!(expected, result.unwrap());
    }

    #[test]
    fn fp_str_s_unaligned_offset_uses_unscaled() {
        let result = emit_fp_str_s_offset(NeonRegister::V0, Register::R1, 3);
        assert!(result.is_some());
        let expected: u32 = 0xBC000000 | ((3 & 0x1ff) << 12) | (1 << 5);
        assert_eq!(expected, result.unwrap());
    }

    // --- NEON element operations ---

    #[test]
    fn neon_dup_v0_4s_from_r1() {
        // Zig's test exercises emitNeonDupElement (not whitelisted); this
        // instead checks the whitelisted emit_neon_dup (DUP Vd.T, Rn) shape.
        // dest_size=Size4S: q_bit=1, size=(14&3)=2, idx=((0<<1)|1)<<2=4.
        let result = emit_neon_dup(NeonRegister::V0, Register::R1, NeonSize::Size4S);
        let expected: u32 = 0x0e000c00 | (1 << 30) | (4 << 16) | (1 << 5);
        assert_eq!(expected, result);
    }

    #[test]
    fn neon_smov_w0_v1_s1() {
        // SMOV Wd, Vn.Ts[index]: size=2 (S), idx = (1*2+1)<<2 = 12.
        let result = emit_neon_smov(Register::R0, NeonRegister::V1, 1, NeonSize::Size1S);
        let expected: u32 = 0x0e002c00 | (12 << 16) | (1 << 5);
        assert_eq!(expected, result);
    }

    // --- NEON load/store ---

    #[test]
    fn neon_ldr_s0_x1_0() {
        let result = emit_neon_ldr_offset(NeonRegister::V0, NeonSize::Size1S, Register::R1, 0);
        assert!(result.is_some());
    }

    #[test]
    fn neon_str_d0_x1_8() {
        let result = emit_neon_str_offset(NeonRegister::V0, NeonSize::Size1D, Register::R1, 8);
        assert!(result.is_some());
    }

    #[test]
    fn neon_ldr_offset_returns_none_for_out_of_range() {
        let result = emit_neon_ldr_offset(NeonRegister::V0, NeonSize::Size1S, Register::R1, 0x10000);
        assert_eq!(None, result);
    }

    // --- NEON scalar<->general transfers ---

    #[test]
    fn fmov_w0_s1() {
        let expected: u32 = 0x1e260000 | (1 << 5);
        assert_eq!(expected, emit_neon_fmov_to_general(Register::R0, NeonRegister::V1));
    }

    #[test]
    fn fmov_x0_d1() {
        let expected: u32 = 0x9e260000 | (1 << 22) | (1 << 5);
        assert_eq!(expected, emit_neon_fmov_to_general_64(Register::R0, NeonRegister::V1));
    }

    #[test]
    fn fmov_s0_w1() {
        let expected: u32 = 0x1e270000 | (1 << 5);
        assert_eq!(expected, emit_neon_fmov_from_general(NeonRegister::V0, Register::R1));
    }

    #[test]
    fn fmov_d0_x1() {
        let expected: u32 = 0x9e270000 | (1 << 22) | (1 << 5);
        assert_eq!(expected, emit_neon_fmov_from_general_64(NeonRegister::V0, Register::R1));
    }

    #[test]
    fn scvtf_s0_w1() {
        let expected: u32 = 0x1e220000 | (1 << 5);
        assert_eq!(expected, emit_neon_scvtf_gen(NeonRegister::V0, Register::R1, NeonSize::Size1S));
    }

    #[test]
    fn ucvtf_d0_x1() {
        let expected: u32 = 0x9e230000 | (1 << 22) | (1 << 5);
        assert_eq!(expected, emit_neon_ucvtf_gen_64(NeonRegister::V0, Register::R1, NeonSize::Size1D));
    }

    #[test]
    fn fcvtzs_w0_s1() {
        let expected: u32 = 0x1e380000 | (1 << 5);
        assert_eq!(expected, emit_neon_fcvtzs_gen(Register::R0, NeonRegister::V1, NeonSize::Size1S));
    }

    #[test]
    fn fcvtzu_x0_d1() {
        let expected: u32 = 0x9e390000 | (1 << 22) | (1 << 5);
        assert_eq!(expected, emit_neon_fcvtzu_gen_64(Register::R0, NeonRegister::V1, NeonSize::Size1D));
    }

    // --- ArmBranchLinker ---

    #[test]
    fn arm_branch_linker_init() {
        let linker = ArmBranchLinker::init();
        assert!(!linker.has_instruction());
        assert!(!linker.has_target());
    }

    #[test]
    fn arm_branch_linker_set_and_resolve_imm26() {
        let mut buffer = [0x14000000u32, 0, 0, 0];
        let mut linker = ArmBranchLinker::init();
        linker.set_instruction_and_class(0, BranchClass::Imm26);
        linker.set_target(8); // target at byte offset 8 = word 2
        assert!(linker.has_instruction());
        assert!(linker.has_target());
        linker.resolve(&mut buffer);
        // delta = 2 - 0 = 2
        assert_eq!(0x14000002, buffer[0]);
    }

    #[test]
    fn arm_branch_linker_reset() {
        let mut linker = ArmBranchLinker::init();
        linker.set_instruction_and_class(0, BranchClass::Imm26);
        linker.set_target(4);
        linker.reset();
        assert!(!linker.has_instruction());
        assert!(!linker.has_target());
    }

    #[test]
    fn arm_branch_linker_resolve_imm19() {
        let mut buffer = [0x54000000u32, 0, 0, 0]; // B.EQ
        let mut linker = ArmBranchLinker::init();
        linker.set_instruction_and_class(0, BranchClass::Imm19);
        linker.set_target(12); // target at word 3
        linker.resolve(&mut buffer);
        // delta = 3, encoded: (3 << 5) in imm19 field
        let imm19_field = (buffer[0] >> 5) & 0x7ffff;
        assert_eq!(3, imm19_field);
    }

    #[test]
    fn arm_branch_linker_resolve_imm14() {
        let mut buffer = [0x36000000u32, 0, 0, 0]; // TBZ
        let mut linker = ArmBranchLinker::init();
        linker.set_instruction_and_class(0, BranchClass::Imm14);
        linker.set_target(8); // target at word 2
        linker.resolve(&mut buffer);
        // delta = 2, encoded: (2 << 5) in imm14 field
        let imm14_field = (buffer[0] >> 5) & 0x3fff;
        assert_eq!(2, imm14_field);
    }

    // --- ARM64_SYSREG ---

    #[test]
    fn arm64_sysreg_encoding() {
        let fpcr = arm64_sysreg(1, 3, 4, 4, 0);
        assert_eq!(ARM64_FPCR, fpcr);
        let nzcv = arm64_sysreg(1, 3, 4, 2, 0);
        assert_eq!(ARM64_NZCV, nzcv);
    }

    // --- emitLogicalImmediateCommon -1 fallback (bug fix test) ---

    #[test]
    fn and_w0_w1_0xffffffff_returns_fallback_via_not_register() {
        let result = emit_and_immediate(Register::R0, Register::R1, 0xFFFFFFFF);
        assert!(result.is_some());
        let expected: u32 = 0x0a200000 | (31 << 16) | (1 << 5);
        assert_eq!(expected, result.unwrap());
    }

    // --- NEON min/max ---

    #[test]
    fn neon_smax_v0_4s_v1_4s_v2_4s() {
        let expected: u32 = 0x0e206400 | (1 << 30) | (2 << 22) | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_neon_smax(NeonRegister::V0, NeonRegister::V1, NeonRegister::V2, NeonSize::Size4S));
    }

    #[test]
    fn neon_smin_v0_4s_v1_4s_v2_4s() {
        // Zig test exercises emitNeonUmin (not whitelisted); check
        // emit_neon_smin (whitelisted) instead with the analogous shape.
        let expected: u32 = 0x0e206c00 | (1 << 30) | (2 << 22) | (2 << 16) | (1 << 5);
        assert_eq!(expected, emit_neon_smin(NeonRegister::V0, NeonRegister::V1, NeonRegister::V2, NeonSize::Size4S));
    }

    // --- NEG aliases ---

    #[test]
    fn neg_w0_w1() {
        let expected = emit_sub_register(Register::R0, Register::ZR, RegisterParam::reg_only(Register::R1));
        assert_eq!(expected, emit_neg(Register::R0, Register::R1));
    }

    #[test]
    fn neg_x0_x1() {
        let expected = emit_sub_register_64(Register::R0, Register::ZR, RegisterParam::reg_only(Register::R1));
        assert_eq!(expected, emit_neg_64(Register::R0, Register::R1));
    }

    // --- CMN aliases ---

    #[test]
    fn cmn_w0_w1() {
        let expected = emit_adds_register(Register::ZR, Register::R0, RegisterParam::reg_only(Register::R1));
        assert_eq!(expected, emit_cmn_register(Register::R0, RegisterParam::reg_only(Register::R1)));
    }

    #[test]
    fn cmn_x0_x1() {
        let expected = emit_adds_register_64(Register::ZR, Register::R0, RegisterParam::reg_only(Register::R1));
        assert_eq!(expected, emit_cmn_register_64(Register::R0, RegisterParam::reg_only(Register::R1)));
    }

    #[test]
    fn cmn_w0_42() {
        let expected = emit_adds_immediate(Register::ZR, Register::R0, 42);
        assert_eq!(expected, emit_cmn_immediate(Register::R0, 42));
    }

    // --- SXTW alias ---

    #[test]
    fn sxtw_x0_w1() {
        let expected = emit_sbfm_64(Register::R0, Register::R1, 0, 31);
        assert_eq!(expected, emit_sxtw_64(Register::R0, Register::R1));
    }

    // --- HINT / BTI ---

    #[test]
    fn hint_0_is_nop() {
        assert_eq!(ARM64_OPCODE_NOP, emit_hint(0));
    }

    #[test]
    fn hint_34_is_bti_c() {
        assert_eq!(0xd503245f, emit_hint(34));
    }

    // --- Scalar FCVT ---

    #[test]
    fn fcvt_d0_s1_single_to_double() {
        let expected: u32 = 0x1e22c000 | (1 << 5);
        assert_eq!(expected, emit_fcvt_s_to_d(NeonRegister::V0, NeonRegister::V1));
    }

    #[test]
    fn fcvt_s0_d1_double_to_single() {
        let expected: u32 = 0x1e624000 | (1 << 5);
        assert_eq!(expected, emit_fcvt_d_to_s(NeonRegister::V0, NeonRegister::V1));
    }
}
