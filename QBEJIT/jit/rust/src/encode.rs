//! `JitInst[]` → ARM64 machine code. Port of `jit_encode.zig`'s
//! `encodeInstruction`/`resolveFixups`/`jitEncode` against this crate's own
//! [`crate::encoder`] and [`crate::module::JitModule`].
//!
//! Dropped versus the Zig original (see `module.rs` and `../README.md`):
//! source maps, comment maps, per-category encode statistics, and the
//! `dump*`/disassembly-report functions built on top of them — debugging
//! conveniences, not part of the codegen contract. What's kept is exactly
//! what [`crate::linker::link`] needs afterward: code/data bytes, labels,
//! and every fixup/relocation kind.
//!
//! Errors here are non-fatal and accumulate in [`JitModule::errors`],
//! matching the Zig source's "collect diagnostics, keep encoding" approach
//! (§ this file's module doc's "Errors" note) — a caller that wants strict
//! behavior should check `errors.is_empty()` after [`encode`] returns.

use crate::encoder::{self as enc, BranchClass, Condition, NeonRegister, NeonSize, Register, RegisterParam, ShiftType};
use crate::ffi::{cls, inst_kind as k, reg_sentinel, JitInst};
use crate::module::{BranchFixup, DataSymRef, ExtCallEntry, JitModule, JitSymType, LoadAddrReloc, SymbolEntry};

#[derive(Debug, Clone, Copy)]
pub enum EncodeError {
    InvalidRegister,
}

// ============================================================================
// Section: Register/Condition/Shift/NeonSize mapping (JitInst → encoder)
// ============================================================================

/// Mirrors `mapGPRegister` (`jit_encode.zig`). `Register`'s discriminants
/// (`R0..R28 = 0..28`, `Fp = 29`, `Lr = 30`, `Sp = 31`) line up with QBE's
/// physical register numbering exactly for `0..=30` — this match is
/// mechanical, not a reinterpretation.
fn map_gp_register(reg_id: i32) -> Result<Register, EncodeError> {
    match reg_id {
        0 => Ok(Register::R0),
        1 => Ok(Register::R1),
        2 => Ok(Register::R2),
        3 => Ok(Register::R3),
        4 => Ok(Register::R4),
        5 => Ok(Register::R5),
        6 => Ok(Register::R6),
        7 => Ok(Register::R7),
        8 => Ok(Register::R8),
        9 => Ok(Register::R9),
        10 => Ok(Register::R10),
        11 => Ok(Register::R11),
        12 => Ok(Register::R12),
        13 => Ok(Register::R13),
        14 => Ok(Register::R14),
        15 => Ok(Register::R15),
        16 => Ok(Register::R16),
        17 => Ok(Register::R17),
        18 => Ok(Register::R18),
        19 => Ok(Register::R19),
        20 => Ok(Register::R20),
        21 => Ok(Register::R21),
        22 => Ok(Register::R22),
        23 => Ok(Register::R23),
        24 => Ok(Register::R24),
        25 => Ok(Register::R25),
        26 => Ok(Register::R26),
        27 => Ok(Register::R27),
        28 => Ok(Register::R28),
        29 => Ok(Register::Fp),
        30 => Ok(Register::Lr),
        reg_sentinel::SP => Ok(Register::Sp),
        reg_sentinel::FP => Ok(Register::Fp),
        reg_sentinel::LR => Ok(Register::Lr),
        reg_sentinel::IP0 => Ok(Register::R16),
        reg_sentinel::IP1 => Ok(Register::R17),
        _ => Err(EncodeError::InvalidRegister),
    }
}

/// Mirrors `mapNeonRegister`. NEON registers are stored as
/// `JIT_VREG_BASE - index` (V0 = -100, V1 = -101, ...).
fn map_neon_register(reg_id: i32) -> Result<NeonRegister, EncodeError> {
    if reg_id > reg_sentinel::VREG_BASE {
        return Err(EncodeError::InvalidRegister);
    }
    let index = reg_sentinel::VREG_BASE - reg_id;
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
    if (0..32).contains(&index) {
        Ok(TABLE[index as usize])
    } else {
        Err(EncodeError::InvalidRegister)
    }
}

/// Mirrors `mapCondition`. QBE never emits condition value 0xF (NV) — ARM64
/// treats it as a legacy alias for AL, and neither the Zig source nor
/// [`crate::encoder::Condition`] defines an `Nv` variant — so 0xF (and any
/// other out-of-range nibble, which cannot occur from a real `u8 & 0xF`)
/// falls back to `Al` rather than panicking.
fn map_condition(cond: u8) -> Condition {
    match cond & 0xF {
        0x0 => Condition::Eq,
        0x1 => Condition::Ne,
        0x2 => Condition::Cs,
        0x3 => Condition::Cc,
        0x4 => Condition::Mi,
        0x5 => Condition::Pl,
        0x6 => Condition::Vs,
        0x7 => Condition::Vc,
        0x8 => Condition::Hi,
        0x9 => Condition::Ls,
        0xA => Condition::Ge,
        0xB => Condition::Lt,
        0xC => Condition::Gt,
        0xD => Condition::Le,
        _ => Condition::Al, // 0xE (Al) and the unrepresentable 0xF (Nv)
    }
}

/// Mirrors `mapShiftType`.
fn map_shift_type(shift: u8) -> ShiftType {
    match shift & 0x3 {
        0 => ShiftType::Lsl,
        1 => ShiftType::Lsr,
        2 => ShiftType::Asr,
        _ => ShiftType::Ror,
    }
}

/// Mirrors `mapNeonArr` (`JitNeonArr` → `NeonSize`).
fn map_neon_arr(arr_val: u8, _is_float: bool) -> NeonSize {
    match arr_val {
        0 => NeonSize::Size4S,  // NEON_4S
        1 => NeonSize::Size2D,  // NEON_2D
        2 => NeonSize::Size4S,  // NEON_4SF
        3 => NeonSize::Size2D,  // NEON_2DF
        4 => NeonSize::Size8H,  // NEON_8H
        _ => NeonSize::Size16B, // NEON_16B
    }
}

/// Mirrors `scalarSizeFromCls`.
fn scalar_size_from_cls(cls_val: u8) -> NeonSize {
    if cls_val == cls::D {
        NeonSize::Size1D
    } else {
        NeonSize::Size1S
    }
}

fn is64(inst: &JitInst) -> bool {
    inst.cls == cls::L
}
fn is_float_operand(inst: &JitInst) -> bool {
    inst.cls >= cls::S
}
fn is_double(inst: &JitInst) -> bool {
    inst.cls == cls::D
}

fn reg_error(mod_: &mut JitModule, inst_index: u32, name: &str, reg_id: i32) {
    mod_.errors.push(format!(
        "inst[{inst_index}]: invalid register {name}={reg_id}"
    ));
}

// ============================================================================
// Section: Transient encode-time state (not part of JitModule's output)
// ============================================================================

/// A data-section symbol recorded at `JIT_DATA_START` but not committed to
/// `JitModule::symbols` until the first byte is actually emitted (mirrors
/// the Zig source's `pending_data_sym_name`/`has_pending_data_sym` — see
/// that field's comment in `jit_encode.zig` for why: the recorded offset
/// must reflect the position *after* any `JIT_DATA_ALIGN`, not before).
struct EncodeState {
    in_data_section: bool,
    pending_data_sym: Option<String>,
}

impl EncodeState {
    fn commit_pending_data_sym(&mut self, mod_: &mut JitModule) {
        if let Some(name) = self.pending_data_sym.take() {
            let offset = mod_.data_offset();
            if crate::module::is_oop_symbol(&name) {
                mod_.oop_slots.insert(crate::module::canonical_symbol_key(&name), offset);
            }
            mod_.symbols.insert(
                name,
                SymbolEntry {
                    offset,
                    is_code: false,
                    sym_type: JitSymType::Data,
                },
            );
        }
    }
}

// ============================================================================
// Section: Data-section emission helpers (operate directly on mod_.data)
// ============================================================================

fn emit_data_bytes(mod_: &mut JitModule, bytes: &[u8]) {
    mod_.data.extend_from_slice(bytes);
}
fn align_data(mod_: &mut JitModule, alignment: u32) {
    if alignment <= 1 {
        return;
    }
    let alignment = alignment as usize;
    let pad = (alignment - (mod_.data.len() % alignment)) % alignment;
    mod_.data.resize(mod_.data.len() + pad, 0);
}

// ============================================================================
// Section: Core Instruction Encoding Dispatch
// ============================================================================

/// Encode a single `JitInst` into `mod_`'s code (or data) buffer. Mirrors
/// `encodeInstruction`.
fn encode_instruction(mod_: &mut JitModule, state: &mut EncodeState, inst: &JitInst, inst_index: u32) {
    match inst.kind() {
        // ── Pseudo-instructions (no machine code) ──
        k::JIT_LABEL => {
            mod_.labels.insert(inst.target_id, mod_.code_offset());
        }
        k::JIT_FUNC_BEGIN => {
            let name = inst.sym_name_str().to_string();
            mod_.symbols.insert(
                name,
                SymbolEntry {
                    offset: mod_.code_offset(),
                    is_code: true,
                    sym_type: JitSymType::Func,
                },
            );
        }
        k::JIT_FUNC_END | k::JIT_DBGLOC | k::JIT_COMMENT => {
            // Boundary marker / source-line / comment — no machine code,
            // and (per this port's scope) no side-table recorded either.
        }
        k::JIT_NOP => { mod_.push_code_word(enc::emit_nop()); }

        // ── Integer arithmetic (register-register) ──
        k::JIT_ADD_RRR => encode_alu_rrr(mod_, inst, inst_index, AluOp::Add),
        k::JIT_SUB_RRR => encode_alu_rrr(mod_, inst, inst_index, AluOp::Sub),
        k::JIT_MUL_RRR => encode_alu_rrr(mod_, inst, inst_index, AluOp::Mul),
        k::JIT_SDIV_RRR => encode_alu_rrr(mod_, inst, inst_index, AluOp::Sdiv),
        k::JIT_UDIV_RRR => encode_alu_rrr(mod_, inst, inst_index, AluOp::Udiv),
        k::JIT_AND_RRR => encode_alu_rrr(mod_, inst, inst_index, AluOp::And),
        k::JIT_ORR_RRR => encode_alu_rrr(mod_, inst, inst_index, AluOp::Orr),
        k::JIT_EOR_RRR => encode_alu_rrr(mod_, inst, inst_index, AluOp::Eor),
        k::JIT_LSL_RRR => encode_alu_rrr(mod_, inst, inst_index, AluOp::Lsl),
        k::JIT_LSR_RRR => encode_alu_rrr(mod_, inst, inst_index, AluOp::Lsr),
        k::JIT_ASR_RRR => encode_alu_rrr(mod_, inst, inst_index, AluOp::Asr),

        k::JIT_NEG_RR => {
            let (rd, rm) = match (map_gp_register(inst.rd), map_gp_register(inst.rn)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(_), _) => return reg_error(mod_, inst_index, "rd", inst.rd),
                (_, Err(_)) => return reg_error(mod_, inst_index, "rn", inst.rn),
            };
            let word = if is64(inst) { enc::emit_neg_64(rd, rm) } else { enc::emit_neg(rd, rm) };
            mod_.push_code_word(word);
        }

        // ── Fused multiply-add/sub (4-operand) ──
        k::JIT_MADD_RRRR => encode_madd_msub(mod_, inst, inst_index, false),
        k::JIT_MSUB_RRRR => encode_madd_msub(mod_, inst, inst_index, true),

        // ── Register-immediate ALU ──
        k::JIT_ADD_RRI => encode_alu_rri(mod_, inst, inst_index, false),
        k::JIT_SUB_RRI => encode_alu_rri(mod_, inst, inst_index, true),

        // ── Shifted-operand ALU ──
        k::JIT_ADD_SHIFT => encode_shifted_alu(mod_, inst, inst_index, ShiftedAluOp::Add),
        k::JIT_SUB_SHIFT => encode_shifted_alu(mod_, inst, inst_index, ShiftedAluOp::Sub),
        k::JIT_AND_SHIFT => encode_shifted_alu(mod_, inst, inst_index, ShiftedAluOp::And),
        k::JIT_ORR_SHIFT => encode_shifted_alu(mod_, inst, inst_index, ShiftedAluOp::Orr),
        k::JIT_EOR_SHIFT => encode_shifted_alu(mod_, inst, inst_index, ShiftedAluOp::Eor),

        // ── Move / constant loading ──
        k::JIT_MOV_RR => {
            let (rd, rn) = match (map_gp_register(inst.rd), map_gp_register(inst.rn)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(_), _) => return reg_error(mod_, inst_index, "rd", inst.rd),
                (_, Err(_)) => return reg_error(mod_, inst_index, "rn", inst.rn),
            };
            let word = if is64(inst) { enc::emit_mov_register_64(rd, rn) } else { enc::emit_mov_register(rd, rn) };
            mod_.push_code_word(word);
        }
        k::JIT_MOVZ => encode_move_wide(mod_, inst, inst_index, MoveWideOp::Movz),
        k::JIT_MOVK => encode_move_wide(mod_, inst, inst_index, MoveWideOp::Movk),
        k::JIT_MOVN => encode_move_wide(mod_, inst, inst_index, MoveWideOp::Movn),

        k::JIT_MOV_WIDE_IMM => {
            let rd = match map_gp_register(inst.rd) {
                Ok(r) => r,
                Err(_) => return reg_error(mod_, inst_index, "rd", inst.rd),
            };
            if is64(inst) {
                let mut buf = [0u32; 4];
                let count = enc::emit_load_immediate_64(rd, inst.imm as u64, &mut buf);
                for w in &buf[..count as usize] {
                    mod_.push_code_word(*w);
                }
            } else {
                let mut buf = [0u32; 2];
                let count = enc::emit_load_immediate_32(rd, inst.imm as u32, &mut buf);
                for w in &buf[..count as usize] {
                    mod_.push_code_word(*w);
                }
            }
        }

        // ── Floating point arithmetic ──
        k::JIT_FADD_RRR => encode_fp_rrr(mod_, inst, inst_index, FpOp::Fadd),
        k::JIT_FSUB_RRR => encode_fp_rrr(mod_, inst, inst_index, FpOp::Fsub),
        k::JIT_FMUL_RRR => encode_fp_rrr(mod_, inst, inst_index, FpOp::Fmul),
        k::JIT_FDIV_RRR => encode_fp_rrr(mod_, inst, inst_index, FpOp::Fdiv),

        k::JIT_FNEG_RR => {
            let (rd, rn) = match (map_neon_register(inst.rd), map_neon_register(inst.rn)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(_), _) => return reg_error(mod_, inst_index, "rd(neon)", inst.rd),
                (_, Err(_)) => return reg_error(mod_, inst_index, "rn(neon)", inst.rn),
            };
            mod_.push_code_word(enc::emit_neon_fneg(rd, rn, scalar_size_from_cls(inst.cls)));
        }
        k::JIT_FMOV_RR => {
            let (rd, rn) = match (map_neon_register(inst.rd), map_neon_register(inst.rn)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(_), _) => return reg_error(mod_, inst_index, "rd(neon)", inst.rd),
                (_, Err(_)) => return reg_error(mod_, inst_index, "rn(neon)", inst.rn),
            };
            mod_.push_code_word(enc::emit_neon_fmov(rd, rn, scalar_size_from_cls(inst.cls)));
        }

        // ── Float <-> Int conversions ──
        k::JIT_FCVT_SD => {
            let (rd, rn) = match (map_neon_register(inst.rd), map_neon_register(inst.rn)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(_), _) => return reg_error(mod_, inst_index, "rd(neon)", inst.rd),
                (_, Err(_)) => return reg_error(mod_, inst_index, "rn(neon)", inst.rn),
            };
            mod_.push_code_word(enc::emit_fcvt_s_to_d(rd, rn));
        }
        k::JIT_FCVT_DS => {
            let (rd, rn) = match (map_neon_register(inst.rd), map_neon_register(inst.rn)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(_), _) => return reg_error(mod_, inst_index, "rd(neon)", inst.rd),
                (_, Err(_)) => return reg_error(mod_, inst_index, "rn(neon)", inst.rn),
            };
            mod_.push_code_word(enc::emit_fcvt_d_to_s(rd, rn));
        }
        k::JIT_FCVTZS => {
            let rd = match map_gp_register(inst.rd) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rd", inst.rd) };
            let rn = match map_neon_register(inst.rn) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rn(neon)", inst.rn) };
            let size = scalar_size_from_cls(inst.cls);
            let word = if inst.is_float != 0 { enc::emit_neon_fcvtzs_gen_64(rd, rn, size) } else { enc::emit_neon_fcvtzs_gen(rd, rn, size) };
            mod_.push_code_word(word);
        }
        k::JIT_FCVTZU => {
            let rd = match map_gp_register(inst.rd) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rd", inst.rd) };
            let rn = match map_neon_register(inst.rn) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rn(neon)", inst.rn) };
            let size = scalar_size_from_cls(inst.cls);
            let word = if inst.is_float != 0 { enc::emit_neon_fcvtzu_gen_64(rd, rn, size) } else { enc::emit_neon_fcvtzu_gen(rd, rn, size) };
            mod_.push_code_word(word);
        }
        k::JIT_SCVTF => {
            let rd = match map_neon_register(inst.rd) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rd(neon)", inst.rd) };
            let rn = match map_gp_register(inst.rn) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rn", inst.rn) };
            let size = scalar_size_from_cls(inst.cls);
            let word = if inst.is_float != 0 { enc::emit_neon_scvtf_gen_64(rd, rn, size) } else { enc::emit_neon_scvtf_gen(rd, rn, size) };
            mod_.push_code_word(word);
        }
        k::JIT_UCVTF => {
            let rd = match map_neon_register(inst.rd) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rd(neon)", inst.rd) };
            let rn = match map_gp_register(inst.rn) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rn", inst.rn) };
            let size = scalar_size_from_cls(inst.cls);
            let word = if inst.is_float != 0 { enc::emit_neon_ucvtf_gen_64(rd, rn, size) } else { enc::emit_neon_ucvtf_gen(rd, rn, size) };
            mod_.push_code_word(word);
        }
        k::JIT_FMOV_GF => {
            // cls is the *destination* (integer) class — use is64(), not
            // is_double(), matching the Zig source's fix for this exact bug.
            let rd = match map_gp_register(inst.rd) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rd", inst.rd) };
            let rn = match map_neon_register(inst.rn) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rn(neon)", inst.rn) };
            let word = if is64(inst) { enc::emit_neon_fmov_to_general_64(rd, rn) } else { enc::emit_neon_fmov_to_general(rd, rn) };
            mod_.push_code_word(word);
        }
        k::JIT_FMOV_FG => {
            let rd = match map_neon_register(inst.rd) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rd(neon)", inst.rd) };
            let rn = match map_gp_register(inst.rn) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rn", inst.rn) };
            let word = if is_double(inst) { enc::emit_neon_fmov_from_general_64(rd, rn) } else { enc::emit_neon_fmov_from_general(rd, rn) };
            mod_.push_code_word(word);
        }

        // ── Extensions ──
        k::JIT_SXTB => encode_extension(mod_, inst, inst_index, ExtOp::Sxtb),
        k::JIT_UXTB => encode_extension(mod_, inst, inst_index, ExtOp::Uxtb),
        k::JIT_SXTH => encode_extension(mod_, inst, inst_index, ExtOp::Sxth),
        k::JIT_UXTH => encode_extension(mod_, inst, inst_index, ExtOp::Uxth),
        k::JIT_SXTW => {
            let (rd, rn) = match (map_gp_register(inst.rd), map_gp_register(inst.rn)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(_), _) => return reg_error(mod_, inst_index, "rd", inst.rd),
                (_, Err(_)) => return reg_error(mod_, inst_index, "rn", inst.rn),
            };
            mod_.push_code_word(enc::emit_sxtw_64(rd, rn));
        }
        k::JIT_UXTW => {
            // Aliased as MOV Wd, Wn (32-bit mov clears the upper 32 bits).
            let (rd, rn) = match (map_gp_register(inst.rd), map_gp_register(inst.rn)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(_), _) => return reg_error(mod_, inst_index, "rd", inst.rd),
                (_, Err(_)) => return reg_error(mod_, inst_index, "rn", inst.rn),
            };
            mod_.push_code_word(enc::emit_mov_register(rd, rn));
        }

        // ── Compare ──
        k::JIT_CMP_RR => {
            let (rn, rm) = match (map_gp_register(inst.rn), map_gp_register(inst.rm)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(_), _) => return reg_error(mod_, inst_index, "rn", inst.rn),
                (_, Err(_)) => return reg_error(mod_, inst_index, "rm", inst.rm),
            };
            let word = if is64(inst) {
                enc::emit_cmp_register_64(rn, RegisterParam::reg_only(rm))
            } else {
                enc::emit_cmp_register(rn, RegisterParam::reg_only(rm))
            };
            mod_.push_code_word(word);
        }
        k::JIT_CMP_RI => {
            let rn = match map_gp_register(inst.rn) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rn", inst.rn) };
            let imm12 = (inst.imm as u64 & 0xFFF) as u16;
            let word = if is64(inst) { enc::emit_cmp_immediate_64(rn, imm12) } else { enc::emit_cmp_immediate(rn, imm12) };
            mod_.push_code_word(word);
        }
        k::JIT_CMN_RR => {
            let (rn, rm) = match (map_gp_register(inst.rn), map_gp_register(inst.rm)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(_), _) => return reg_error(mod_, inst_index, "rn", inst.rn),
                (_, Err(_)) => return reg_error(mod_, inst_index, "rm", inst.rm),
            };
            let word = if is64(inst) {
                enc::emit_cmn_register_64(rn, RegisterParam::reg_only(rm))
            } else {
                enc::emit_cmn_register(rn, RegisterParam::reg_only(rm))
            };
            mod_.push_code_word(word);
        }
        k::JIT_FCMP_RR => {
            let (rn, rm) = match (map_neon_register(inst.rn), map_neon_register(inst.rm)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(_), _) => return reg_error(mod_, inst_index, "rn(neon)", inst.rn),
                (_, Err(_)) => return reg_error(mod_, inst_index, "rm(neon)", inst.rm),
            };
            mod_.push_code_word(enc::emit_neon_fcmpe(rn, rm, scalar_size_from_cls(inst.cls)));
        }
        k::JIT_TST_RR => {
            let (rn, rm) = match (map_gp_register(inst.rn), map_gp_register(inst.rm)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(_), _) => return reg_error(mod_, inst_index, "rn", inst.rn),
                (_, Err(_)) => return reg_error(mod_, inst_index, "rm", inst.rm),
            };
            let word = if is64(inst) {
                enc::emit_test_register_64(rn, RegisterParam::reg_only(rm))
            } else {
                enc::emit_test_register(rn, RegisterParam::reg_only(rm))
            };
            mod_.push_code_word(word);
        }

        // ── Conditional select/set ──
        k::JIT_CSET => {
            let rd = match map_gp_register(inst.rd) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rd", inst.rd) };
            let cond = map_condition(inst.cond);
            let word = if is64(inst) { enc::emit_cset_64(rd, cond) } else { enc::emit_cset(rd, cond) };
            mod_.push_code_word(word);
        }
        k::JIT_CSEL => {
            let (rd, rn, rm) = match (map_gp_register(inst.rd), map_gp_register(inst.rn), map_gp_register(inst.rm)) {
                (Ok(a), Ok(b), Ok(c)) => (a, b, c),
                (Err(_), _, _) => return reg_error(mod_, inst_index, "rd", inst.rd),
                (_, Err(_), _) => return reg_error(mod_, inst_index, "rn", inst.rn),
                (_, _, Err(_)) => return reg_error(mod_, inst_index, "rm", inst.rm),
            };
            let cond = map_condition(inst.cond);
            let word = if is64(inst) { enc::emit_csel_64(rd, rn, rm, cond) } else { enc::emit_csel(rd, rn, rm, cond) };
            mod_.push_code_word(word);
        }

        // ── Memory: load/store with immediate offset ──
        k::JIT_LDR_RI => encode_ldr_str_offset(mod_, inst, inst_index, LdrStrOp::Ldr),
        k::JIT_LDRB_RI => encode_ldr_str_offset(mod_, inst, inst_index, LdrStrOp::Ldrb),
        k::JIT_LDRH_RI => encode_ldr_str_offset(mod_, inst, inst_index, LdrStrOp::Ldrh),
        k::JIT_LDRSB_RI => encode_ldr_str_offset(mod_, inst, inst_index, LdrStrOp::Ldrsb),
        k::JIT_LDRSH_RI => encode_ldr_str_offset(mod_, inst, inst_index, LdrStrOp::Ldrsh),
        k::JIT_LDRSW_RI => encode_ldr_str_offset(mod_, inst, inst_index, LdrStrOp::Ldrsw),
        k::JIT_STR_RI => encode_ldr_str_offset(mod_, inst, inst_index, LdrStrOp::Str),
        k::JIT_STRB_RI => encode_ldr_str_offset(mod_, inst, inst_index, LdrStrOp::Strb),
        k::JIT_STRH_RI => encode_ldr_str_offset(mod_, inst, inst_index, LdrStrOp::Strh),

        // ── Memory: load/store with register offset ──
        k::JIT_LDR_RR => encode_ldr_str_register(mod_, inst, inst_index, LdrStrOp::Ldr),
        k::JIT_STR_RR => encode_ldr_str_register(mod_, inst, inst_index, LdrStrOp::Str),
        k::JIT_LDRB_RR => encode_ldr_str_register(mod_, inst, inst_index, LdrStrOp::Ldrb),
        k::JIT_LDRH_RR => encode_ldr_str_register(mod_, inst, inst_index, LdrStrOp::Ldrh),
        k::JIT_LDRSB_RR => encode_ldr_str_register(mod_, inst, inst_index, LdrStrOp::Ldrsb),
        k::JIT_LDRSH_RR => encode_ldr_str_register(mod_, inst, inst_index, LdrStrOp::Ldrsh),
        k::JIT_LDRSW_RR => encode_ldr_str_register(mod_, inst, inst_index, LdrStrOp::Ldrsw),
        k::JIT_STRB_RR => encode_ldr_str_register(mod_, inst, inst_index, LdrStrOp::Strb),
        k::JIT_STRH_RR => encode_ldr_str_register(mod_, inst, inst_index, LdrStrOp::Strh),

        // ── Memory: load/store pair ──
        k::JIT_LDP => encode_ldp_stp(mod_, inst, inst_index, LdpStpOp::Ldp),
        k::JIT_STP => encode_ldp_stp(mod_, inst, inst_index, LdpStpOp::Stp),
        k::JIT_LDP_POST => encode_ldp_stp(mod_, inst, inst_index, LdpStpOp::LdpPost),
        k::JIT_STP_PRE => encode_ldp_stp(mod_, inst, inst_index, LdpStpOp::StpPre),

        // ── Branches ──
        k::JIT_B => encode_branch(mod_, inst, inst_index, BranchOp::B),
        k::JIT_BL => encode_branch(mod_, inst, inst_index, BranchOp::Bl),
        k::JIT_B_COND => encode_branch(mod_, inst, inst_index, BranchOp::BCond),
        k::JIT_CBZ => encode_branch(mod_, inst, inst_index, BranchOp::Cbz),
        k::JIT_CBNZ => encode_branch(mod_, inst, inst_index, BranchOp::Cbnz),

        k::JIT_BR => {
            let rn = match map_gp_register(inst.rn) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rn", inst.rn) };
            mod_.push_code_word(enc::emit_br(rn));
        }
        k::JIT_BLR => {
            let rn = match map_gp_register(inst.rn) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rn", inst.rn) };
            mod_.push_code_word(enc::emit_blr(rn));
        }
        k::JIT_RET => {
            mod_.push_code_word(enc::emit_ret(Register::Lr));
        }

        // ── Call external ──
        k::JIT_CALL_EXT => {
            let bl_offset = mod_.code_offset();
            let name = inst.sym_name_str().to_string();
            if crate::module::is_ic_site_symbol(&name) {
                mod_.ic_sites.insert(crate::module::canonical_symbol_key(&name), bl_offset);
            }
            mod_.ext_calls.push(ExtCallEntry { code_offset: bl_offset, sym_name: name, inst_index });
            // Placeholder BL (offset 0) — patched during trampoline linking.
            mod_.push_code_word(enc::emit_bl(0));
        }

        // ── PC-relative ──
        k::JIT_ADRP => {
            let rd = match map_gp_register(inst.rd) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rd", inst.rd) };
            mod_.push_code_word(enc::emit_adrp(rd, 0));
        }
        k::JIT_ADR => {
            let rd = match map_gp_register(inst.rd) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rd", inst.rd) };
            mod_.push_code_word(enc::emit_adr(rd, 0));
        }
        k::JIT_LOAD_ADDR => {
            let rd = match map_gp_register(inst.rd) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rd", inst.rd) };
            let adrp_offset = mod_.code_offset();
            mod_.push_code_word(enc::emit_adrp(rd, 0));
            mod_.push_code_word(enc::emit_add_immediate_64(rd, rd, 0));
            mod_.load_addr_relocs.push(LoadAddrReloc {
                adrp_offset,
                sym_name: inst.sym_name_str().to_string(),
                inst_index,
                addend: inst.imm,
            });
        }

        // ── Stack manipulation ──
        k::JIT_SUB_SP => encode_sp_adjust(mod_, inst, inst_index, true),
        k::JIT_ADD_SP => encode_sp_adjust(mod_, inst, inst_index, false),
        k::JIT_MOV_SP => {
            if inst.rd != reg_sentinel::NONE && inst.rd != reg_sentinel::SP {
                let rd = match map_gp_register(inst.rd) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rd", inst.rd) };
                mod_.push_code_word(enc::emit_add_immediate_64(rd, Register::Sp, 0));
            } else {
                let rn = match map_gp_register(inst.rn) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rn", inst.rn) };
                mod_.push_code_word(enc::emit_add_immediate_64(Register::Sp, rn, 0));
            }
        }

        // ── Special ──
        k::JIT_HINT => {
            mod_.push_code_word(enc::emit_hint((inst.imm as u64 & 0x7F) as u32));
        }
        k::JIT_BRK => {
            mod_.push_code_word(enc::emit_brk((inst.imm as u64 & 0xFFFF) as u16));
        }

        // ── NEON vector ops ──
        k::JIT_NEON_LDR_Q => encode_neon_ldr_str(mod_, inst, inst_index, true),
        k::JIT_NEON_STR_Q => encode_neon_ldr_str(mod_, inst, inst_index, false),
        k::JIT_NEON_ADD => encode_neon_bin_op(mod_, inst, NeonBinOp::Add),
        k::JIT_NEON_SUB => encode_neon_bin_op(mod_, inst, NeonBinOp::Sub),
        k::JIT_NEON_MUL => encode_neon_bin_op(mod_, inst, NeonBinOp::Mul),
        k::JIT_NEON_DIV => encode_neon_bin_op(mod_, inst, NeonBinOp::Div),
        k::JIT_NEON_NEG => encode_neon_unary_op(mod_, inst, NeonUnaryOp::Neg),
        k::JIT_NEON_ABS => encode_neon_unary_op(mod_, inst, NeonUnaryOp::Abs),
        k::JIT_NEON_FMA => encode_neon_fma(mod_, inst),
        k::JIT_NEON_MIN => encode_neon_bin_op(mod_, inst, NeonBinOp::Min),
        k::JIT_NEON_MAX => encode_neon_bin_op(mod_, inst, NeonBinOp::Max),
        k::JIT_NEON_DUP => encode_neon_dup(mod_, inst, inst_index),
        k::JIT_NEON_ADDV => encode_neon_addv(mod_, inst, inst_index),

        // ── Data directives ──
        k::JIT_DATA_START => {
            state.in_data_section = true;
            let name = inst.sym_name_str();
            state.pending_data_sym = if !name.is_empty() { Some(name.to_string()) } else { None };
        }
        k::JIT_DATA_END => {
            state.commit_pending_data_sym(mod_);
            state.in_data_section = false;
        }
        k::JIT_DATA_BYTE => {
            state.commit_pending_data_sym(mod_);
            emit_data_bytes(mod_, &[(inst.imm as u64 & 0xFF) as u8]);
        }
        k::JIT_DATA_HALF => {
            state.commit_pending_data_sym(mod_);
            emit_data_bytes(mod_, &((inst.imm as u64 & 0xFFFF) as u16).to_le_bytes());
        }
        k::JIT_DATA_WORD => {
            state.commit_pending_data_sym(mod_);
            emit_data_bytes(mod_, &((inst.imm as u64 & 0xFFFF_FFFF) as u32).to_le_bytes());
        }
        k::JIT_DATA_QUAD => {
            state.commit_pending_data_sym(mod_);
            emit_data_bytes(mod_, &(inst.imm as u64).to_le_bytes());
        }
        k::JIT_DATA_ZERO => {
            state.commit_pending_data_sym(mod_);
            let count = (inst.imm as u64 & 0xFFFF_FFFF) as usize;
            mod_.data.resize(mod_.data.len() + count, 0);
        }
        k::JIT_DATA_ASCII => {
            state.commit_pending_data_sym(mod_);
            let decoded_len = if inst.imm > 0 { inst.imm as u64 as usize } else { inst.sym_name_str().len() };
            let bytes = inst.sym_name_bytes(decoded_len).to_vec();
            emit_data_bytes(mod_, &bytes);
        }
        k::JIT_DATA_ALIGN => {
            align_data(mod_, (inst.imm as u64 & 0xFFFF_FFFF) as u32);
        }
        k::JIT_DATA_SYMREF => {
            state.commit_pending_data_sym(mod_);
            let data_off = mod_.data_offset();
            emit_data_bytes(mod_, &0u64.to_le_bytes());
            let sym_name = inst.sym_name_str();
            if !sym_name.is_empty() {
                mod_.data_sym_refs.push(DataSymRef {
                    data_offset: data_off,
                    sym_name: sym_name.to_string(),
                    addend: inst.imm,
                    inst_index,
                });
            }
        }

        other => {
            mod_.errors.push(format!("inst[{inst_index}]: unknown JitInstKind {other}"));
        }
    }
}

// ============================================================================
// Section: ALU Encoding Helpers
// ============================================================================

enum AluOp { Add, Sub, Mul, Sdiv, Udiv, And, Orr, Eor, Lsl, Lsr, Asr }

fn encode_alu_rrr(mod_: &mut JitModule, inst: &JitInst, inst_index: u32, op: AluOp) {
    let (rd, rn, rm) = match (map_gp_register(inst.rd), map_gp_register(inst.rn), map_gp_register(inst.rm)) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        (Err(_), _, _) => return reg_error(mod_, inst_index, "rd", inst.rd),
        (_, Err(_), _) => return reg_error(mod_, inst_index, "rn", inst.rn),
        (_, _, Err(_)) => return reg_error(mod_, inst_index, "rm", inst.rm),
    };
    let w64 = is64(inst);

    // ARM64 quirk: in shifted-register ADD/SUB, register 31 means XZR, NOT
    // SP. When either operand is SP we must use the extended-register form
    // (UXTX #0), where register 31 *does* mean SP — matters for dynamic
    // stack allocation (`SUB SP, SP, Xn`).
    let uses_sp = rd == Register::Sp || rn == Register::Sp;
    let rmp = if uses_sp && matches!(op, AluOp::Add | AluOp::Sub) {
        RegisterParam::extended(rm, crate::encoder::ExtendType::Uxtx, 0)
    } else {
        RegisterParam::reg_only(rm)
    };

    let word = match op {
        AluOp::Add => if w64 { enc::emit_add_register_64(rd, rn, rmp) } else { enc::emit_add_register(rd, rn, rmp) },
        AluOp::Sub => if w64 { enc::emit_sub_register_64(rd, rn, rmp) } else { enc::emit_sub_register(rd, rn, rmp) },
        AluOp::Mul => if w64 { enc::emit_mul_64(rd, rn, rm) } else { enc::emit_mul(rd, rn, rm) },
        AluOp::Sdiv => if w64 { enc::emit_sdiv_64(rd, rn, rm) } else { enc::emit_sdiv(rd, rn, rm) },
        AluOp::Udiv => if w64 { enc::emit_udiv_64(rd, rn, rm) } else { enc::emit_udiv(rd, rn, rm) },
        AluOp::And => if w64 { enc::emit_and_register_64(rd, rn, rmp) } else { enc::emit_and_register(rd, rn, rmp) },
        AluOp::Orr => if w64 { enc::emit_orr_register_64(rd, rn, rmp) } else { enc::emit_orr_register(rd, rn, rmp) },
        AluOp::Eor => if w64 { enc::emit_eor_register_64(rd, rn, rmp) } else { enc::emit_eor_register(rd, rn, rmp) },
        AluOp::Lsl => if w64 { enc::emit_lsl_register_64(rd, rn, rm) } else { enc::emit_lsl_register(rd, rn, rm) },
        AluOp::Lsr => if w64 { enc::emit_lsr_register_64(rd, rn, rm) } else { enc::emit_lsr_register(rd, rn, rm) },
        AluOp::Asr => if w64 { enc::emit_asr_register_64(rd, rn, rm) } else { enc::emit_asr_register(rd, rn, rm) },
    };
    mod_.push_code_word(word);
}

fn encode_madd_msub(mod_: &mut JitModule, inst: &JitInst, inst_index: u32, is_msub: bool) {
    let (rd, rn, rm, ra) = match (map_gp_register(inst.rd), map_gp_register(inst.rn), map_gp_register(inst.rm), map_gp_register(inst.ra)) {
        (Ok(a), Ok(b), Ok(c), Ok(d)) => (a, b, c, d),
        (Err(_), _, _, _) => return reg_error(mod_, inst_index, "rd", inst.rd),
        (_, Err(_), _, _) => return reg_error(mod_, inst_index, "rn", inst.rn),
        (_, _, Err(_), _) => return reg_error(mod_, inst_index, "rm", inst.rm),
        (_, _, _, Err(_)) => return reg_error(mod_, inst_index, "ra", inst.ra),
    };
    let w64 = is64(inst);
    let word = if is_msub {
        if w64 { enc::emit_msub_64(rd, rn, rm, ra) } else { enc::emit_msub(rd, rn, rm, ra) }
    } else if w64 { enc::emit_madd_64(rd, rn, rm, ra) } else { enc::emit_madd(rd, rn, rm, ra) };
    mod_.push_code_word(word);
}

fn encode_alu_rri(mod_: &mut JitModule, inst: &JitInst, inst_index: u32, is_sub: bool) {
    let (rd, rn) = match (map_gp_register(inst.rd), map_gp_register(inst.rn)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(_), _) => return reg_error(mod_, inst_index, "rd", inst.rd),
        (_, Err(_)) => return reg_error(mod_, inst_index, "rn", inst.rn),
    };
    let imm12 = (inst.imm as u64 & 0xFFF) as u16;
    let w64 = is64(inst);
    let word = if is_sub {
        if w64 { enc::emit_sub_immediate_64(rd, rn, imm12) } else { enc::emit_sub_immediate(rd, rn, imm12) }
    } else if w64 { enc::emit_add_immediate_64(rd, rn, imm12) } else { enc::emit_add_immediate(rd, rn, imm12) };
    mod_.push_code_word(word);
}

enum ShiftedAluOp { Add, Sub, And, Orr, Eor }

fn encode_shifted_alu(mod_: &mut JitModule, inst: &JitInst, inst_index: u32, op: ShiftedAluOp) {
    let (rd, rn, rm) = match (map_gp_register(inst.rd), map_gp_register(inst.rn), map_gp_register(inst.rm)) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        (Err(_), _, _) => return reg_error(mod_, inst_index, "rd", inst.rd),
        (_, Err(_), _) => return reg_error(mod_, inst_index, "rn", inst.rn),
        (_, _, Err(_)) => return reg_error(mod_, inst_index, "rm", inst.rm),
    };
    let shift = map_shift_type(inst.shift_type);
    let amount = (inst.imm2 as u64 & 0x3F) as u8;
    let rmp = RegisterParam::shifted(rm, shift, amount);
    let w64 = is64(inst);
    let word = match op {
        ShiftedAluOp::Add => if w64 { enc::emit_add_register_64(rd, rn, rmp) } else { enc::emit_add_register(rd, rn, rmp) },
        ShiftedAluOp::Sub => if w64 { enc::emit_sub_register_64(rd, rn, rmp) } else { enc::emit_sub_register(rd, rn, rmp) },
        ShiftedAluOp::And => if w64 { enc::emit_and_register_64(rd, rn, rmp) } else { enc::emit_and_register(rd, rn, rmp) },
        ShiftedAluOp::Orr => if w64 { enc::emit_orr_register_64(rd, rn, rmp) } else { enc::emit_orr_register(rd, rn, rmp) },
        ShiftedAluOp::Eor => if w64 { enc::emit_eor_register_64(rd, rn, rmp) } else { enc::emit_eor_register(rd, rn, rmp) },
    };
    mod_.push_code_word(word);
}

enum MoveWideOp { Movz, Movk, Movn }

fn encode_move_wide(mod_: &mut JitModule, inst: &JitInst, inst_index: u32, op: MoveWideOp) {
    let rd = match map_gp_register(inst.rd) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rd", inst.rd) };
    let imm16 = (inst.imm as u64 & 0xFFFF) as u16;
    let hw = (inst.imm2 as u64 & 0x3F) as u32;
    let w64 = is64(inst);
    let word = match op {
        MoveWideOp::Movz => if w64 { enc::emit_movz_64(rd, imm16, hw) } else { enc::emit_movz(rd, imm16, hw) },
        MoveWideOp::Movk => if w64 { enc::emit_movk_64(rd, imm16, hw) } else { enc::emit_movk(rd, imm16, hw) },
        MoveWideOp::Movn => if w64 { enc::emit_movn_64(rd, imm16, hw) } else { enc::emit_movn(rd, imm16, hw) },
    };
    mod_.push_code_word(word);
}

enum FpOp { Fadd, Fsub, Fmul, Fdiv }

fn encode_fp_rrr(mod_: &mut JitModule, inst: &JitInst, inst_index: u32, op: FpOp) {
    let (rd, rn, rm) = match (map_neon_register(inst.rd), map_neon_register(inst.rn), map_neon_register(inst.rm)) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        (Err(_), _, _) => return reg_error(mod_, inst_index, "rd(neon)", inst.rd),
        (_, Err(_), _) => return reg_error(mod_, inst_index, "rn(neon)", inst.rn),
        (_, _, Err(_)) => return reg_error(mod_, inst_index, "rm(neon)", inst.rm),
    };
    let size = scalar_size_from_cls(inst.cls);
    let word = match op {
        FpOp::Fadd => enc::emit_neon_fadd(rd, rn, rm, size),
        FpOp::Fsub => enc::emit_neon_fsub(rd, rn, rm, size),
        FpOp::Fmul => enc::emit_neon_fmul(rd, rn, rm, size),
        FpOp::Fdiv => enc::emit_neon_fdiv(rd, rn, rm, size),
    };
    mod_.push_code_word(word);
}

enum ExtOp { Sxtb, Uxtb, Sxth, Uxth }

fn encode_extension(mod_: &mut JitModule, inst: &JitInst, inst_index: u32, op: ExtOp) {
    let (rd, rn) = match (map_gp_register(inst.rd), map_gp_register(inst.rn)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(_), _) => return reg_error(mod_, inst_index, "rd", inst.rd),
        (_, Err(_)) => return reg_error(mod_, inst_index, "rn", inst.rn),
    };
    let w64 = is64(inst);
    let word = match op {
        ExtOp::Sxtb => if w64 { enc::emit_sxtb_64(rd, rn) } else { enc::emit_sxtb(rd, rn) },
        ExtOp::Uxtb => if w64 { enc::emit_uxtb_64(rd, rn) } else { enc::emit_uxtb(rd, rn) },
        ExtOp::Sxth => if w64 { enc::emit_sxth_64(rd, rn) } else { enc::emit_sxth(rd, rn) },
        ExtOp::Uxth => if w64 { enc::emit_uxth_64(rd, rn) } else { enc::emit_uxth(rd, rn) },
    };
    mod_.push_code_word(word);
}

// ============================================================================
// Section: Load/Store Encoding Helpers
// ============================================================================

enum LdrStrOp { Ldr, Ldrb, Ldrh, Ldrsb, Ldrsh, Ldrsw, Str, Strb, Strh }

fn encode_ldr_str_offset(mod_: &mut JitModule, inst: &JitInst, inst_index: u32, op: LdrStrOp) {
    let offset = inst.imm as i32;

    if is_float_operand(inst) && matches!(op, LdrStrOp::Ldr | LdrStrOp::Str) {
        let fp_rt = match map_neon_register(inst.rd) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rd(neon)", inst.rd) };
        let rn = match map_gp_register(inst.rn) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rn", inst.rn) };
        let word = match op {
            LdrStrOp::Ldr => if is_double(inst) { enc::emit_fp_ldr_d_offset(fp_rt, rn, offset) } else { enc::emit_fp_ldr_s_offset(fp_rt, rn, offset) },
            LdrStrOp::Str => if is_double(inst) { enc::emit_fp_str_d_offset(fp_rt, rn, offset) } else { enc::emit_fp_str_s_offset(fp_rt, rn, offset) },
            _ => unreachable!(),
        };
        match word {
            Some(w) => { mod_.push_code_word(w); }
            None => mod_.errors.push(format!("inst[{inst_index}]: FP LDR/STR offset out of range: {offset}")),
        }
        return;
    }

    let rt = match map_gp_register(inst.rd) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rd", inst.rd) };
    let rn = match map_gp_register(inst.rn) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rn", inst.rn) };
    let w64 = is64(inst);
    let word = match op {
        LdrStrOp::Ldr => if w64 { enc::emit_ldr_offset_64(rt, rn, offset) } else { enc::emit_ldr_offset(rt, rn, offset) },
        LdrStrOp::Ldrb => enc::emit_ldrb_offset(rt, rn, offset),
        LdrStrOp::Ldrh => enc::emit_ldrh_offset(rt, rn, offset),
        LdrStrOp::Ldrsb => if w64 { enc::emit_ldrsb_offset_64(rt, rn, offset) } else { enc::emit_ldrsb_offset(rt, rn, offset) },
        LdrStrOp::Ldrsh => if w64 { enc::emit_ldrsh_offset_64(rt, rn, offset) } else { enc::emit_ldrsh_offset(rt, rn, offset) },
        LdrStrOp::Ldrsw => enc::emit_ldrsw_offset_64(rt, rn, offset),
        LdrStrOp::Str => if w64 { enc::emit_str_offset_64(rt, rn, offset) } else { enc::emit_str_offset(rt, rn, offset) },
        LdrStrOp::Strb => enc::emit_strb_offset(rt, rn, offset),
        LdrStrOp::Strh => enc::emit_strh_offset(rt, rn, offset),
    };
    match word {
        Some(w) => { mod_.push_code_word(w); }
        None => mod_.errors.push(format!("inst[{inst_index}]: LDR/STR offset out of range: {offset}")),
    }
}

fn encode_ldr_str_register(mod_: &mut JitModule, inst: &JitInst, inst_index: u32, op: LdrStrOp) {
    let (rt, rn, rm) = match (map_gp_register(inst.rd), map_gp_register(inst.rn), map_gp_register(inst.rm)) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        (Err(_), _, _) => return reg_error(mod_, inst_index, "rd", inst.rd),
        (_, Err(_), _) => return reg_error(mod_, inst_index, "rn", inst.rn),
        (_, _, Err(_)) => return reg_error(mod_, inst_index, "rm", inst.rm),
    };
    let w64 = is64(inst);
    let rmp = RegisterParam::reg_only(rm);
    let word = match op {
        LdrStrOp::Ldr => if w64 { enc::emit_ldr_register_64(rt, rn, rmp) } else { enc::emit_ldr_register(rt, rn, rmp) },
        LdrStrOp::Str => if w64 { enc::emit_str_register_64(rt, rn, rmp) } else { enc::emit_str_register(rt, rn, rmp) },
        LdrStrOp::Ldrb => enc::emit_ldrb_register(rt, rn, rmp),
        LdrStrOp::Ldrh => enc::emit_ldrh_register(rt, rn, rmp),
        LdrStrOp::Ldrsb => if w64 { enc::emit_ldrsb_register_64(rt, rn, rmp) } else { enc::emit_ldrsb_register(rt, rn, rmp) },
        LdrStrOp::Ldrsh => if w64 { enc::emit_ldrsh_register_64(rt, rn, rmp) } else { enc::emit_ldrsh_register(rt, rn, rmp) },
        LdrStrOp::Ldrsw => enc::emit_ldrsw_register_64(rt, rn, rmp),
        LdrStrOp::Strb => enc::emit_strb_register(rt, rn, rmp),
        LdrStrOp::Strh => enc::emit_strh_register(rt, rn, rmp),
    };
    mod_.push_code_word(word);
}

enum LdpStpOp { Ldp, Stp, LdpPost, StpPre }

fn encode_ldp_stp(mod_: &mut JitModule, inst: &JitInst, inst_index: u32, op: LdpStpOp) {
    let (rt1, rt2, rn) = match (map_gp_register(inst.rd), map_gp_register(inst.rm), map_gp_register(inst.rn)) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        (Err(_), _, _) => return reg_error(mod_, inst_index, "rd", inst.rd),
        (_, Err(_), _) => return reg_error(mod_, inst_index, "rm", inst.rm),
        (_, _, Err(_)) => return reg_error(mod_, inst_index, "rn", inst.rn),
    };
    let offset = inst.imm as i32;
    let w64 = is64(inst);
    let word = match op {
        LdpStpOp::Ldp => if w64 { enc::emit_ldp_offset_64(rt1, rt2, rn, offset) } else { enc::emit_ldp_offset(rt1, rt2, rn, offset) },
        LdpStpOp::Stp => if w64 { enc::emit_stp_offset_64(rt1, rt2, rn, offset) } else { enc::emit_stp_offset(rt1, rt2, rn, offset) },
        LdpStpOp::LdpPost => if w64 { enc::emit_ldp_offset_post_index_64(rt1, rt2, rn, offset) } else { enc::emit_ldp_offset_post_index(rt1, rt2, rn, offset) },
        LdpStpOp::StpPre => if w64 { enc::emit_stp_offset_pre_index_64(rt1, rt2, rn, offset) } else { enc::emit_stp_offset_pre_index(rt1, rt2, rn, offset) },
    };
    match word {
        Some(w) => { mod_.push_code_word(w); }
        None => mod_.errors.push(format!("inst[{inst_index}]: LDP/STP offset out of range: {offset}")),
    }
}

// ============================================================================
// Section: Branch Encoding
// ============================================================================

enum BranchOp { B, Bl, BCond, Cbz, Cbnz }

fn encode_branch(mod_: &mut JitModule, inst: &JitInst, inst_index: u32, op: BranchOp) {
    let target_id = inst.target_id;

    if let Some(&target_offset) = mod_.labels.get(&target_id) {
        // Backward branch — offset is already known.
        let current_offset = mod_.code_offset();
        let delta_bytes = target_offset as i64 - current_offset as i64;
        let delta_words = (delta_bytes / 4) as i32;

        let word = match op {
            BranchOp::B => Some(enc::emit_b(delta_words)),
            BranchOp::Bl => Some(enc::emit_bl(delta_words)),
            BranchOp::BCond => Some(enc::emit_b_cond(delta_words, map_condition(inst.cond))),
            BranchOp::Cbz => match map_gp_register(inst.rd) {
                Ok(rt) => Some(if is64(inst) { enc::emit_cbz_64(rt, delta_words) } else { enc::emit_cbz(rt, delta_words) }),
                Err(_) => { reg_error(mod_, inst_index, "rd", inst.rd); None }
            },
            BranchOp::Cbnz => match map_gp_register(inst.rd) {
                Ok(rt) => Some(if is64(inst) { enc::emit_cbnz_64(rt, delta_words) } else { enc::emit_cbnz(rt, delta_words) }),
                Err(_) => { reg_error(mod_, inst_index, "rd", inst.rd); None }
            },
        };
        if let Some(w) = word {
            mod_.push_code_word(w);
        }
    } else {
        // Forward branch — emit a placeholder and record a fixup.
        let branch_class = match op {
            BranchOp::B | BranchOp::Bl => BranchClass::Imm26,
            BranchOp::BCond | BranchOp::Cbz | BranchOp::Cbnz => BranchClass::Imm19,
        };

        let base = match op {
            BranchOp::B => enc::emit_b(0),
            BranchOp::Bl => enc::emit_bl(0),
            BranchOp::BCond => enc::emit_b_cond(0, map_condition(inst.cond)),
            BranchOp::Cbz => match map_gp_register(inst.rd) {
                Ok(rt) => if is64(inst) { enc::emit_cbz_64(rt, 0) } else { enc::emit_cbz(rt, 0) },
                Err(_) => { reg_error(mod_, inst_index, "rd", inst.rd); enc::emit_nop() }
            },
            BranchOp::Cbnz => match map_gp_register(inst.rd) {
                Ok(rt) => if is64(inst) { enc::emit_cbnz_64(rt, 0) } else { enc::emit_cbnz(rt, 0) },
                Err(_) => { reg_error(mod_, inst_index, "rd", inst.rd); enc::emit_nop() }
            },
        };

        let fixup_offset = mod_.code_offset();
        mod_.push_code_word(base);
        mod_.fixups.push(BranchFixup {
            code_offset: fixup_offset,
            target_id,
            branch_class,
            inst_index,
            base_opcode: base,
        });
    }
}

// ============================================================================
// Section: NEON Encoding Helpers
// ============================================================================
//
// QBE's arm64 backend uses fixed scratch registers v28-v30 for NEON ops
// (see design/QBE_extensions.md's "Register Reservation" note — v28-v30
// are excluded from NFPS specifically so the register allocator never
// hands them out), so these helpers hardcode V28/V29/V30 rather than
// mapping a JitInst register field, matching jit_encode.zig exactly.

enum NeonBinOp { Add, Sub, Mul, Div, Min, Max }

fn encode_neon_bin_op(mod_: &mut JitModule, inst: &JitInst, op: NeonBinOp) {
    let arr_val = (inst.imm as u64 & 0xFF) as u8;
    let is_float = inst.is_float != 0;
    let size = map_neon_arr(arr_val, is_float);
    let (vd, vn, vm) = (NeonRegister::V28, NeonRegister::V28, NeonRegister::V29);
    let word = match op {
        NeonBinOp::Add => if is_float { enc::emit_neon_fadd(vd, vn, vm, size) } else { enc::emit_neon_add(vd, vn, vm, size) },
        NeonBinOp::Sub => if is_float { enc::emit_neon_fsub(vd, vn, vm, size) } else { enc::emit_neon_sub(vd, vn, vm, size) },
        NeonBinOp::Mul => if is_float { enc::emit_neon_fmul(vd, vn, vm, size) } else { enc::emit_neon_mul(vd, vn, vm, size) },
        // Integer vector divide has no NEON instruction; QBE only ever
        // requests this for float, matching the Zig source's comment.
        NeonBinOp::Div => enc::emit_neon_fdiv(vd, vn, vm, size),
        NeonBinOp::Min => if is_float { enc::emit_neon_fmin(vd, vn, vm, size) } else { enc::emit_neon_smin(vd, vn, vm, size) },
        NeonBinOp::Max => if is_float { enc::emit_neon_fmax(vd, vn, vm, size) } else { enc::emit_neon_smax(vd, vn, vm, size) },
    };
    mod_.push_code_word(word);
}

enum NeonUnaryOp { Neg, Abs }

fn encode_neon_unary_op(mod_: &mut JitModule, inst: &JitInst, op: NeonUnaryOp) {
    let arr_val = (inst.imm as u64 & 0xFF) as u8;
    let is_float = inst.is_float != 0;
    let size = map_neon_arr(arr_val, is_float);
    let (vd, vn) = (NeonRegister::V28, NeonRegister::V28);
    let word = match op {
        NeonUnaryOp::Neg => if is_float { enc::emit_neon_fneg(vd, vn, size) } else { enc::emit_neon_neg(vd, vn, size) },
        NeonUnaryOp::Abs => if is_float { enc::emit_neon_fabs(vd, vn, size) } else { enc::emit_neon_abs(vd, vn, size) },
    };
    mod_.push_code_word(word);
}

fn encode_neon_fma(mod_: &mut JitModule, inst: &JitInst) {
    let arr_val = (inst.imm as u64 & 0xFF) as u8;
    let is_float = inst.is_float != 0;
    let size = map_neon_arr(arr_val, is_float);
    // v28 += v29 * v30
    let word = if is_float {
        enc::emit_neon_fmla(NeonRegister::V28, NeonRegister::V29, NeonRegister::V30, size)
    } else {
        enc::emit_neon_mla(NeonRegister::V28, NeonRegister::V29, NeonRegister::V30, size)
    };
    mod_.push_code_word(word);
}

fn encode_neon_dup(mod_: &mut JitModule, inst: &JitInst, inst_index: u32) {
    let arr_val = (inst.imm as u64 & 0xFF) as u8;
    let is_float = inst.is_float != 0;
    let size = map_neon_arr(arr_val, is_float);
    // The GP source register maps to inst.rm in the collector.
    let src_gp = match map_gp_register(inst.rm) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rm(gp for dup)", inst.rm) };
    mod_.push_code_word(enc::emit_neon_dup(NeonRegister::V28, src_gp, size));
}

fn encode_neon_addv(mod_: &mut JitModule, inst: &JitInst, inst_index: u32) {
    // Horizontal reduction — a short multi-instruction sequence per
    // arrangement, matching QBE's emit.c exactly (see jit_encode.zig's
    // encodeNeonAddv doc comment for the per-arrangement instruction list).
    let arr_val = (inst.imm as u64 & 0xFF) as u8;
    let dest_gp = match map_gp_register(inst.rd) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rd", inst.rd) };
    let v28 = NeonRegister::V28;
    match arr_val {
        0 => {
            // NEON_4S (int): addv s28, v28.4s ; fmov wDest, s28
            mod_.push_code_word(enc::emit_neon_addv(v28, v28, NeonSize::Size4S));
            mod_.push_code_word(enc::emit_neon_fmov_to_general(dest_gp, v28));
        }
        1 => {
            // NEON_2D: addp d28, v28.2d ; fmov xDest, d28
            mod_.push_code_word(enc::emit_neon_addp_scalar(v28, v28, NeonSize::Size2D));
            mod_.push_code_word(enc::emit_neon_fmov_to_general_64(dest_gp, v28));
        }
        2 => {
            // NEON_4SF (float): faddp v28.4s,v28.4s,v28.4s ; faddp s28,v28.2s ; fmov wDest,s28
            mod_.push_code_word(enc::emit_neon_faddp(v28, v28, v28, NeonSize::Size4S));
            mod_.push_code_word(enc::emit_neon_faddp_scalar(v28, v28, NeonSize::Size2S));
            mod_.push_code_word(enc::emit_neon_fmov_to_general(dest_gp, v28));
        }
        3 => {
            // NEON_2DF: faddp d28, v28.2d ; fmov xDest, d28
            mod_.push_code_word(enc::emit_neon_faddp_scalar(v28, v28, NeonSize::Size2D));
            mod_.push_code_word(enc::emit_neon_fmov_to_general_64(dest_gp, v28));
        }
        4 => {
            // NEON_8H: addv h28, v28.8h ; smov wDest, v28.h[0]
            mod_.push_code_word(enc::emit_neon_addv(v28, v28, NeonSize::Size8H));
            mod_.push_code_word(enc::emit_neon_smov(dest_gp, v28, 0, NeonSize::Size1H));
        }
        _ => {
            // NEON_16B: addv b28, v28.16b ; smov wDest, v28.b[0]
            mod_.push_code_word(enc::emit_neon_addv(v28, v28, NeonSize::Size16B));
            mod_.push_code_word(enc::emit_neon_smov(dest_gp, v28, 0, NeonSize::Size1B));
        }
    }
}

fn encode_neon_ldr_str(mod_: &mut JitModule, inst: &JitInst, inst_index: u32, is_load: bool) {
    // 128-bit load/store; imm2 selects the vector register (28/29/30).
    let rn = match map_gp_register(inst.rn) { Ok(r) => r, Err(_) => return reg_error(mod_, inst_index, "rn", inst.rn) };
    let vreg = match inst.imm2 {
        29 => NeonRegister::V29,
        30 => NeonRegister::V30,
        _ => NeonRegister::V28,
    };
    let word = if is_load {
        enc::emit_neon_ldr_offset(vreg, NeonSize::Size1Q, rn, 0)
    } else {
        enc::emit_neon_str_offset(vreg, NeonSize::Size1Q, rn, 0)
    };
    match word {
        Some(w) => { mod_.push_code_word(w); }
        None => mod_.errors.push(format!("inst[{inst_index}]: NEON LDR/STR Q encoding failed")),
    }
}

// ============================================================================
// Section: Stack Pointer Adjustment
// ============================================================================

fn encode_sp_adjust(mod_: &mut JitModule, inst: &JitInst, inst_index: u32, is_sub: bool) {
    let imm_val = inst.imm as u64;
    if imm_val <= 4095 {
        let imm12 = imm_val as u16;
        let word = if is_sub {
            enc::emit_sub_immediate_64(Register::Sp, Register::Sp, imm12)
        } else {
            enc::emit_add_immediate_64(Register::Sp, Register::Sp, imm12)
        };
        mod_.push_code_word(word);
    } else {
        // Large frame: materialize into a scratch register (x16/IP0) first.
        let mut buf = [0u32; 4];
        let count = enc::emit_load_immediate_64(Register::R16, inst.imm as u64, &mut buf);
        for w in &buf[..count as usize] {
            mod_.push_code_word(*w);
        }
        let word = if is_sub {
            enc::emit_sub_register_64(Register::Sp, Register::Sp, RegisterParam::reg_only(Register::R16))
        } else {
            enc::emit_add_register_64(Register::Sp, Register::Sp, RegisterParam::reg_only(Register::R16))
        };
        mod_.push_code_word(word);
        let _ = inst_index; // kept for signature symmetry with other encode_* helpers
    }
}

// ============================================================================
// Section: Fixup Resolution (Pass 2)
// ============================================================================

/// Resolve all forward-branch fixups. Mirrors `resolveFixups`. Called after
/// every instruction has been encoded and every label is known.
pub fn resolve_fixups(mod_: &mut JitModule) {
    let fixups = std::mem::take(&mut mod_.fixups);
    for fixup in &fixups {
        let Some(&target_offset) = mod_.labels.get(&fixup.target_id) else {
            mod_.errors.push(format!(
                "inst[{}]: unresolved label: block {}",
                fixup.inst_index, fixup.target_id
            ));
            continue;
        };

        let delta_bytes = target_offset as i64 - fixup.code_offset as i64;
        let delta_words = (delta_bytes / 4) as i32;
        let delta_u32 = delta_words as u32;

        let base_word = fixup.base_opcode;
        let patched = match fixup.branch_class {
            // B / BL: bits [25:0] = offset in instruction words.
            BranchClass::Imm26 => (base_word & 0xfc000000) | (delta_u32 & 0x03ffffff),
            // B.cond / CBZ / CBNZ: bits [23:5] = offset in instruction words.
            BranchClass::Imm19 => (base_word & 0xff00001f) | ((delta_u32 & 0x7ffff) << 5),
            // TBZ / TBNZ: bits [18:5] = offset in instruction words (unused
            // by QBE's arm64 backend today, ported for completeness).
            BranchClass::Imm14 => (base_word & 0xfff8001f) | ((delta_u32 & 0x3fff) << 5),
            BranchClass::Invalid => {
                mod_.errors.push(format!("inst[{}]: invalid branch class for fixup", fixup.inst_index));
                continue;
            }
        };

        let start = fixup.code_offset as usize;
        mod_.code[start..start + 4].copy_from_slice(&patched.to_le_bytes());
    }
    mod_.fixups = fixups;
}

// ============================================================================
// Section: Top-Level Encode Function
// ============================================================================

/// Encode a `JitInst[]` stream into a [`JitModule`]. Mirrors `jitEncode`:
/// Pass 1 emits instructions (recording labels and forward-branch fixups
/// along the way), Pass 2 resolves those fixups. Non-fatal problems
/// accumulate in the returned module's `errors` — check `errors.is_empty()`
/// before trusting the code buffer.
pub fn encode(insts: &[JitInst], code_capacity: usize, data_capacity: usize) -> JitModule {
    let mut mod_ = JitModule::with_capacity(code_capacity, data_capacity);
    let mut state = EncodeState { in_data_section: false, pending_data_sym: None };

    for (i, inst) in insts.iter().enumerate() {
        encode_instruction(&mut mod_, &mut state, inst, i as u32);
    }
    // Safety net matching the Zig source's own commitPendingDataSym() call
    // in the DATA_END arm: a well-formed instruction stream always closes
    // its DATA_START before the stream ends, but don't leave a symbol
    // silently unrecorded if it doesn't.
    state.commit_pending_data_sym(&mut mod_);

    resolve_fixups(&mut mod_);
    mod_
}
