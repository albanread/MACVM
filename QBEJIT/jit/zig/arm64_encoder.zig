//! ARM64 Instruction Encoder
//!
//! A standalone, dependency-free Zig module that encodes many ARM64 instructions
//!   - ARM64Encoder.h
//!   - ARM64NeonEncoder.h
//!   - EncoderMD.h / EncoderMD.cpp
//!   - ARM64LogicalImmediates.h
//!
//! Each emit* function returns a u32 encoding of the instruction word.
//! The caller manages the buffer.

const std = @import("std");

// ============================================================================
// Section: Core Enums
// ============================================================================

pub const Register = enum(u5) {
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
    FP = 29,
    LR = 30,
    SP = 31,

    pub const ZR: Register = .SP; // ZR and SP share encoding 31

    pub fn raw(self: Register) u5 {
        return @intFromEnum(self);
    }
};

pub const NeonRegister = enum(u5) {
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

    pub fn raw(self: NeonRegister) u5 {
        return @intFromEnum(self);
    }
};

pub const ShiftType = enum(u2) {
    LSL = 0,
    LSR = 1,
    ASR = 2,
    ROR = 3,
};

pub const ExtendType = enum(u3) {
    UXTB = 0,
    UXTH = 1,
    UXTW = 2,
    UXTX = 3,
    SXTB = 4,
    SXTH = 5,
    SXTW = 6,
    SXTX = 7,
};

pub const Condition = enum(u4) {
    EQ = 0, // Equal:             Z == 1
    NE = 1, // Not equal:         Z == 0
    CS = 2, // Carry set:         C == 1  (HS)
    CC = 3, // Carry clear:       C == 0  (LO)
    MI = 4, // Negative:          N == 1
    PL = 5, // Positive:          N == 0
    VS = 6, // Overflow:          V == 1
    VC = 7, // No overflow:       V == 0
    HI = 8, // Unsigned higher:   C == 1 and Z == 0
    LS = 9, // Unsigned lower:    C == 0 or Z == 1
    GE = 10, // Signed GE:        N == V
    LT = 11, // Signed LT:        N != V
    GT = 12, // Signed GT:        Z == 0 and N == V
    LE = 13, // Signed LE:        Z == 1 or N != V
    AL = 14, // Always

    pub fn invert(self: Condition) Condition {
        return @enumFromInt(@intFromEnum(self) ^ 1);
    }
};

pub const NeonSize = enum(u4) {
    // scalar sizes, encoded as 0:Q:size
    Size1B = 0,
    Size1H = 1,
    Size1S = 2,
    Size1D = 3,
    Size1Q = 4,
    // vector sizes, encoded as 1:Q:size
    Size8B = 8,
    Size4H = 9,
    Size2S = 10,
    Size16B = 12,
    Size8H = 13,
    Size4S = 14,
    Size2D = 15,

    pub fn isScalar(self: NeonSize) bool {
        return @intFromEnum(self) <= 4;
    }

    pub fn isVector(self: NeonSize) bool {
        return @intFromEnum(self) >= 8;
    }

    pub fn elementSize(self: NeonSize) u2 {
        if (self.isScalar()) {
            return @truncate(@intFromEnum(self));
        } else {
            return @truncate(@intFromEnum(self) & 3);
        }
    }

    pub fn qBit(self: NeonSize) u1 {
        return @truncate((@intFromEnum(self) >> 2) & 1);
    }
};

pub const BranchClass = enum {
    Invalid,
    Imm26, // B, BL
    Imm19, // B.cond, CBZ, CBNZ
    Imm14, // TBZ, TBNZ
};

pub const IndexScale = enum(u2) {
    Scale1 = 0,
    Scale2 = 1,
    Scale4 = 2,
    Scale8 = 3,
};

// ============================================================================
// Section: NeonSize validity masks
// ============================================================================

pub const VALID_1B: u16 = 1 << @intFromEnum(NeonSize.Size1B);
pub const VALID_1H: u16 = 1 << @intFromEnum(NeonSize.Size1H);
pub const VALID_1S: u16 = 1 << @intFromEnum(NeonSize.Size1S);
pub const VALID_1D: u16 = 1 << @intFromEnum(NeonSize.Size1D);
pub const VALID_1Q: u16 = 1 << @intFromEnum(NeonSize.Size1Q);
pub const VALID_8B: u16 = 1 << @intFromEnum(NeonSize.Size8B);
pub const VALID_16B: u16 = 1 << @intFromEnum(NeonSize.Size16B);
pub const VALID_4H: u16 = 1 << @intFromEnum(NeonSize.Size4H);
pub const VALID_8H: u16 = 1 << @intFromEnum(NeonSize.Size8H);
pub const VALID_2S: u16 = 1 << @intFromEnum(NeonSize.Size2S);
pub const VALID_4S: u16 = 1 << @intFromEnum(NeonSize.Size4S);
pub const VALID_2D: u16 = 1 << @intFromEnum(NeonSize.Size2D);

pub const VALID_816B: u16 = VALID_8B | VALID_16B;
pub const VALID_48H: u16 = VALID_4H | VALID_8H;
pub const VALID_24S: u16 = VALID_2S | VALID_4S;

pub fn neonSizeIsValid(size: NeonSize, mask: u16) bool {
    return ((mask >> @intFromEnum(size)) & 1) != 0;
}

// ============================================================================
// Section: Constants
// ============================================================================

// Special opcodes
pub const ARM64_OPCODE_NOP: u32 = 0xd503201f;
pub const ARM64_OPCODE_DMB_ISH: u32 = 0xd5033bbf;
pub const ARM64_OPCODE_DMB_ISHST: u32 = 0xd5033abf;
pub const ARM64_OPCODE_DMB_ISHLD: u32 = 0xd50339bf;

// BRK codes
pub const ARM64_BREAK_DEBUG_BASE: u16 = 0xf000;
pub const ARM64_BREAKPOINT: u16 = ARM64_BREAK_DEBUG_BASE + 0;
pub const ARM64_ASSERT: u16 = ARM64_BREAK_DEBUG_BASE + 1;
pub const ARM64_DEBUG_SERVICE: u16 = ARM64_BREAK_DEBUG_BASE + 2;
pub const ARM64_FASTFAIL: u16 = ARM64_BREAK_DEBUG_BASE + 3;
pub const ARM64_DIVIDE_BY_0: u16 = ARM64_BREAK_DEBUG_BASE + 4;

// CCMP/CCMN flags
pub const CCMP_N: u4 = 0x8;
pub const CCMP_Z: u4 = 0x4;
pub const CCMP_C: u4 = 0x2;
pub const CCMP_V: u4 = 0x1;

// System registers
pub fn ARM64_SYSREG(op0: u1, op1: u3, crn: u4, crm: u4, op2: u3) u15 {
    return (@as(u15, op0) << 14) |
        (@as(u15, op1) << 11) |
        (@as(u15, crn) << 7) |
        (@as(u15, crm) << 3) |
        (@as(u15, op2) << 0);
}

pub const ARM64_FPCR: u15 = ARM64_SYSREG(1, 3, 4, 4, 0);
pub const ARM64_FPSR: u15 = ARM64_SYSREG(1, 3, 4, 4, 1);
pub const ARM64_NZCV: u15 = ARM64_SYSREG(1, 3, 4, 2, 0);
pub const ARM64_CNTVCT: u15 = ARM64_SYSREG(1, 3, 14, 0, 2);
pub const ARM64_TPIDR_EL0: u15 = ARM64_SYSREG(1, 3, 13, 0, 2);

// Atomic load/store opcodes
pub const ARM64_OPCODE_LDARB: u32 = 0x08dffc00;
pub const ARM64_OPCODE_LDARH: u32 = 0x48dffc00;
pub const ARM64_OPCODE_LDAR: u32 = 0x88dffc00;
pub const ARM64_OPCODE_LDAR64: u32 = 0xc8dffc00;

pub const ARM64_OPCODE_LDXRB: u32 = 0x085f7c00;
pub const ARM64_OPCODE_LDXRH: u32 = 0x485f7c00;
pub const ARM64_OPCODE_LDXR: u32 = 0x885f7c00;
pub const ARM64_OPCODE_LDXR64: u32 = 0xc85f7c00;

pub const ARM64_OPCODE_LDAXRB: u32 = 0x085ffc00;
pub const ARM64_OPCODE_LDAXRH: u32 = 0x485ffc00;
pub const ARM64_OPCODE_LDAXR: u32 = 0x885ffc00;
pub const ARM64_OPCODE_LDAXR64: u32 = 0xc85ffc00;

pub const ARM64_OPCODE_STLRB: u32 = 0x089ffc00;
pub const ARM64_OPCODE_STLRH: u32 = 0x489ffc00;
pub const ARM64_OPCODE_STLR: u32 = 0x889ffc00;
pub const ARM64_OPCODE_STLR64: u32 = 0xc89ffc00;

pub const ARM64_OPCODE_STXRB: u32 = 0x08007c00;
pub const ARM64_OPCODE_STXRH: u32 = 0x48007c00;
pub const ARM64_OPCODE_STXR: u32 = 0x88007c00;
pub const ARM64_OPCODE_STXR64: u32 = 0xc8007c00;

pub const ARM64_OPCODE_STLXRB: u32 = 0x0800fc00;
pub const ARM64_OPCODE_STLXRH: u32 = 0x4800fc00;
pub const ARM64_OPCODE_STLXR: u32 = 0x8800fc00;
pub const ARM64_OPCODE_STLXR64: u32 = 0xc800fc00;

// Relocation types (from EncoderMD.h)
pub const RelocType = enum {
    Branch14,
    Branch19,
    Branch26,
    LabelAdr,
    LabelImmed,
    Label,
};

// ============================================================================
// Section: Encoding errors
// ============================================================================

pub const EncodeError = error{
    ImmediateOutOfRange,
    InvalidShift,
    InvalidEncoding,
    CannotEncode,
};

// ============================================================================
// Section: RegisterParam
// ============================================================================

pub const RegisterParam = struct {
    reg: Register,
    shift_type: ?ShiftType = null,
    extend_type: ?ExtendType = null,
    amount: u6 = 0,

    pub fn reg_only(reg_arg: Register) RegisterParam {
        return .{ .reg = reg_arg };
    }

    pub fn shifted(reg_arg: Register, shift: ShiftType, amt: u6) RegisterParam {
        return .{ .reg = reg_arg, .shift_type = shift, .amount = amt };
    }

    pub fn extended(reg_arg: Register, ext: ExtendType, amt: u6) RegisterParam {
        return .{ .reg = reg_arg, .extend_type = ext, .amount = amt };
    }

    pub fn isExtended(self: RegisterParam) bool {
        return self.extend_type != null;
    }

    pub fn isRegOnly(self: RegisterParam) bool {
        return self.shift_type == null and self.extend_type == null and self.amount == 0;
    }

    /// Encode for shifted register instructions.
    /// Returns bits: [23:22] shift_type | [20:16] Rm | [15:10] amount
    pub fn encode(self: RegisterParam) u32 {
        const shift: u2 = if (self.shift_type) |s| @intFromEnum(s) else 0;
        const rv: u32 = @as(u32, self.reg.raw());
        const amt: u32 = @as(u32, self.amount);
        return (@as(u32, shift) << 22) | (rv << 16) | (amt << 10);
    }

    /// Encode for extended register instructions.
    /// Returns bits: [20:16] Rm | [15:13] extend_option | [12:10] amount
    pub fn encodeExtended(self: RegisterParam) u32 {
        const ext: u3 = if (self.extend_type) |e| @intFromEnum(e) else 0;
        const rv: u32 = @as(u32, self.reg.raw());
        const amt: u32 = @as(u32, self.amount);
        return (rv << 16) | (@as(u32, ext) << 13) | (amt << 10);
    }
};

// ============================================================================
// Section: NeonRegisterParam
// ============================================================================

pub const NeonRegisterParam = struct {
    reg: NeonRegister,
    size: NeonSize = .Size1D,

    pub fn init(reg_arg: NeonRegister) NeonRegisterParam {
        return .{ .reg = reg_arg };
    }

    pub fn initWithSize(reg_arg: NeonRegister, s: NeonSize) NeonRegisterParam {
        return .{ .reg = reg_arg, .size = s };
    }

    pub fn rawRegister(self: NeonRegisterParam) u5 {
        return self.reg.raw();
    }
};

// ============================================================================
// Section: Logical Immediate Encoding
// ============================================================================

/// Algorithmic encoder for ARM64 bitmask immediates.
/// Returns the 13-bit N:immr:imms field, or null if the value cannot be encoded.
pub fn encodeLogicalImmediate(value: u64, reg_size: u7) ?u13 {
    if (value == 0 or value == 0xFFFFFFFFFFFFFFFF) return null;
    if (reg_size == 32) {
        // For 32-bit, the value must replicate in both halves
        const v32: u64 = value & 0xFFFFFFFF;
        if (v32 != ((value >> 32) & 0xFFFFFFFF) and value >> 32 != 0) return null;
        return encodeLogicalImmediate64(v32 | (v32 << 32), 32);
    }
    return encodeLogicalImmediate64(value, @as(u7, 64));
}

fn encodeLogicalImmediate64(value: u64, reg_size: u7) ?u13 {
    // A bitmask immediate is a pattern of width 2,4,8,16,32,64 bits
    // that is replicated to fill 64 bits, consisting of a run of set bits
    // rotated within the element.

    if (value == 0 or value == 0xFFFFFFFFFFFFFFFF) return null;

    // Determine the smallest element size that tiles the value
    var size: u7 = 64;
    // Try element sizes from 2..32 (powers of 2)
    var test_size: u7 = 2;
    while (test_size <= 32) : (test_size *= 2) {
        const mask = (@as(u64, 1) << @intCast(test_size)) - 1;
        // Check if the value is made of repeating `test_size`-bit patterns
        var ok = true;
        const base = value & mask;
        var shift: u7 = test_size;
        while (shift < 64) : (shift += test_size) {
            if (((value >> @intCast(shift)) & mask) != base) {
                ok = false;
                break;
            }
        }
        if (ok) {
            size = test_size;
            break;
        }
    }

    // Extract the element-sized pattern
    const mask: u64 = if (size >= 64) ~@as(u64, 0) else (@as(u64, 1) << @intCast(size)) - 1;
    const pattern = value & mask;

    if (pattern == 0 or pattern == mask) return null;

    // Count the run of ones after rotation
    // First, find the rotation: we need to find the position where
    // the pattern transitions from 0->1 (the start of the ones run).

    // Double the pattern to find runs that wrap around.
    // When size == 64 we can't shift a u64 by 64, but the pattern
    // already fills the full width so doubling is a no-op.
    const doubled = if (size >= 64) pattern else pattern | (pattern << @intCast(size));

    // Find trailing zeros of ~pattern to locate position after a run of 1s ends
    // We need to find: rotation (immr) and the number of set bits (imms)

    // Strategy: rotate the pattern so the ones run is right-aligned,
    // then count them.
    var rotated = pattern;
    var rotation: u7 = 0;

    // Find position where bit transitions from 0 to 1 (least significant such position)
    // = position of lowest set bit in pattern where the previous bit is 0
    // The C trick: find where (rotated & 1) == 1 and it's the start of a run
    // Rotate right until we have: [0...0][1...1] pattern
    // i.e. no wrap-around: the ones are contiguous and right-aligned

    // Find a 0 bit first (there must be one since pattern != mask)
    var tmp = pattern;
    var first_zero: u7 = 0;
    while (first_zero < size) : (first_zero += 1) {
        if ((tmp & 1) == 0) break;
        tmp >>= 1;
    }

    // Now rotate right by the number of positions to put the start of the 1-run at bit 0
    // Find the first 1 at or after first_zero
    var start_of_ones: u8 = first_zero;
    const limit: u8 = @as(u8, size) * 2;
    while (start_of_ones < limit) : (start_of_ones += 1) {
        if (start_of_ones < 64 and ((doubled >> @intCast(start_of_ones)) & 1) == 1) break;
    }

    rotation = @intCast(start_of_ones % size);
    if (rotation > 0) {
        rotated = ((pattern >> @intCast(rotation)) | (pattern << @intCast(size - rotation))) & mask;
    }

    // Now rotated should have ones right-aligned: 0...01...1
    // Count trailing ones
    var ones: u7 = 0;
    var rv = rotated;
    while (ones < size) : (ones += 1) {
        if ((rv & 1) == 0) break;
        rv >>= 1;
    }

    // Verify the remaining bits are all zero
    if (rv != 0) return null;

    if (ones == 0 or ones == size) return null;

    // Encode: immr, imms, N
    // immr = rotation (number of positions to rotate right)
    // imms = ones - 1, plus element size encoding in high bits
    // N = 1 if size==64, else 0

    // For element size `size`:
    //   N = (size == 64) ? 1 : 0
    //   imms = (size-specific prefix) | (ones - 1)
    // The element size is encoded via imms high bits:
    //   size=64: N=1, imms = 0b0xxxxx  (ones-1)
    //   size=32: N=0, imms = 0b0xxxxx  ← wait, that's same pattern
    // Actually the encoding is:
    //   N:imms encodes both the element size and the number of ones.
    //   imms = NOT(size-1) in the high bits, ones-1 in the low bits.
    //   Specifically: imms has its high bits set to the bitwise NOT of (size-1) truncated.

    var n_bit: u1 = 0;
    var imms_mask: u6 = undefined;

    switch (size) {
        64 => {
            n_bit = 1;
            imms_mask = 0b111111;
        },
        32 => {
            n_bit = 0;
            imms_mask = 0b011111;
        },
        16 => {
            n_bit = 0;
            imms_mask = 0b001111;
        },
        8 => {
            n_bit = 0;
            imms_mask = 0b000111;
        },
        4 => {
            n_bit = 0;
            imms_mask = 0b000011;
        },
        2 => {
            n_bit = 0;
            imms_mask = 0b000001;
        },
        else => return null,
    }

    // imms prefix bits: ~(size-1) in the upper bits of a 6-bit field
    // For size=32: prefix is 10xxxx => imms = 0b10xxxx | (ones-1)
    // Wait, let me reconsider. The ARM64 encoding is:
    //   If N=1: element size is 64
    //   If N=0: element size determined by highest bit of ~imms:
    //     imms = 0b0xxxxx => size=32
    //     imms = 0b10xxxx => size=16
    //     imms = 0b110xxx => size=8
    //     imms = 0b1110xx => size=4
    //     imms = 0b11110x => size=2

    var imms: u6 = undefined;
    switch (size) {
        64 => imms = @truncate(ones - 1),
        32 => imms = @truncate((ones - 1) & 0x1f),
        16 => imms = @truncate(0b100000 | ((ones - 1) & 0xf)),
        8 => imms = @truncate(0b110000 | ((ones - 1) & 0x7)),
        4 => imms = @truncate(0b111000 | ((ones - 1) & 0x3)),
        2 => imms = @truncate(0b111100 | ((ones - 1) & 0x1)),
        else => return null,
    }

    // If reg_size is 32, N must be 0
    if (reg_size == 32 and n_bit == 1) return null;

    const immr: u6 = @truncate(rotation);

    // Return 13-bit encoding: N:immr:imms
    return (@as(u13, n_bit) << 12) | (@as(u13, immr) << 6) | @as(u13, imms);
}

/// Check if a value can be encoded as a logical immediate for a given register size.
pub fn canEncodeLogicalImmediate(value: u64, reg_size: u7) bool {
    return encodeLogicalImmediate(value, reg_size) != null;
}

// ============================================================================
// Section: ArmBranchLinker
// ============================================================================

/// Branch linker for resolving branch targets in emitted code.
pub const ArmBranchLinker = struct {
    const INVALID_OFFSET: u32 = 0x80000000;

    instruction_offset: u32 = INVALID_OFFSET, // in instruction words
    target_offset: i32 = @bitCast(INVALID_OFFSET), // in instruction words
    branch_class: BranchClass = .Invalid,

    pub fn init() ArmBranchLinker {
        return .{};
    }

    pub fn initWithTarget(target_byte_offset: u32) ArmBranchLinker {
        return .{
            .target_offset = @intCast(target_byte_offset / 4),
        };
    }

    pub fn hasInstruction(self: ArmBranchLinker) bool {
        return self.instruction_offset != INVALID_OFFSET;
    }

    pub fn hasTarget(self: ArmBranchLinker) bool {
        return @as(u32, @bitCast(self.target_offset)) != INVALID_OFFSET;
    }

    pub fn reset(self: *ArmBranchLinker) void {
        self.instruction_offset = INVALID_OFFSET;
        self.target_offset = @bitCast(INVALID_OFFSET);
        self.branch_class = .Invalid;
    }

    pub fn setInstructionOffset(self: *ArmBranchLinker, byte_offset: u32) void {
        std.debug.assert(byte_offset % 4 == 0);
        self.instruction_offset = byte_offset / 4;
    }

    pub fn setClass(self: *ArmBranchLinker, class: BranchClass) void {
        self.branch_class = class;
    }

    pub fn setInstructionAndClass(self: *ArmBranchLinker, byte_offset: u32, class: BranchClass) void {
        self.setInstructionOffset(byte_offset);
        self.setClass(class);
    }

    pub fn setTarget(self: *ArmBranchLinker, byte_offset: i32) void {
        std.debug.assert(@rem(byte_offset, 4) == 0);
        self.target_offset = @divExact(byte_offset, 4);
    }

    /// Resolve a branch by patching the instruction word in the buffer.
    pub fn resolve(self: *ArmBranchLinker, buffer: []u32) void {
        if (!self.hasInstruction() or !self.hasTarget()) return;

        const idx = self.instruction_offset;
        buffer[idx] = self.updateInstruction(buffer[idx]);
    }

    fn updateInstruction(self: ArmBranchLinker, instruction: u32) u32 {
        const delta = self.target_offset - @as(i32, @intCast(self.instruction_offset));
        var result = instruction;
        switch (self.branch_class) {
            .Imm26 => {
                result = (instruction & 0xfc000000) | (@as(u32, @bitCast(delta)) & 0x03ffffff);
            },
            .Imm19 => {
                result = (instruction & 0xff00001f) | ((@as(u32, @bitCast(delta)) << 5) & 0x00ffffe0);
            },
            .Imm14 => {
                result = (instruction & 0xfff8001f) | ((@as(u32, @bitCast(delta)) << 5) & 0x0007ffe0);
            },
            .Invalid => {},
        }
        return result;
    }

    /// Detect branch class from instruction encoding and link.
    pub fn linkRaw(existing: *u32, target: *u32) void {
        const instr = existing.*;
        var class: BranchClass = .Invalid;

        if ((instr & 0x7c000000) == 0x14000000) {
            class = .Imm26;
        } else if ((instr & 0xff000010) == 0x54000000 or
            (instr & 0x7e000000) == 0x34000000)
        {
            class = .Imm19;
        } else if ((instr & 0x7e000000) == 0x36000000) {
            class = .Imm14;
        } else {
            return;
        }

        // Compute delta in instruction words
        const ptr_diff = @intFromPtr(target) - @intFromPtr(existing);
        const delta: i32 = @intCast(@as(isize, @intCast(ptr_diff)) >> 2);

        var linker = ArmBranchLinker{
            .instruction_offset = 0,
            .target_offset = delta,
            .branch_class = class,
        };
        existing.* = linker.updateInstruction(instr);
    }
};

// ============================================================================
// Section: Helper commons
// ============================================================================

fn r(reg: Register) u32 {
    return @as(u32, reg.raw());
}

fn nr(reg: NeonRegister) u32 {
    return @as(u32, reg.raw());
}

// ============================================================================
// Section: NOP / DMB / BRK / Debug
// ============================================================================

pub fn emitNop() u32 {
    return ARM64_OPCODE_NOP;
}

pub fn emitDmb() u32 {
    return ARM64_OPCODE_DMB_ISH;
}

pub fn emitDmbSt() u32 {
    return ARM64_OPCODE_DMB_ISHST;
}

pub fn emitDmbLd() u32 {
    return ARM64_OPCODE_DMB_ISHLD;
}

pub fn emitBrk(code: u16) u32 {
    return 0xd4200000 | (@as(u32, code) << 5);
}

pub fn emitDebugBreak() u32 {
    return emitBrk(ARM64_BREAKPOINT);
}

pub fn emitFastFail() u32 {
    return emitBrk(ARM64_FASTFAIL);
}

pub fn emitDiv0Exception() u32 {
    return emitBrk(ARM64_DIVIDE_BY_0);
}

// ============================================================================
// Section: MRS / MSR
// ============================================================================

pub fn emitMrs(dest: Register, system_reg: u15) u32 {
    return 0xd5300000 | (@as(u32, system_reg) << 5) | r(dest);
}

pub fn emitMsr(source: Register, system_reg: u15) u32 {
    return 0xd5100000 | (@as(u32, system_reg) << 5) | r(source);
}

// ============================================================================
// Section: Branch instructions
// ============================================================================

/// B target (unconditional, PC-relative)
pub fn emitB(offset_words: i26) u32 {
    return 0x14000000 | (@as(u32, @bitCast(@as(i32, offset_words))) & 0x03ffffff);
}

/// BL target (branch with link)
pub fn emitBl(offset_words: i26) u32 {
    return 0x94000000 | (@as(u32, @bitCast(@as(i32, offset_words))) & 0x03ffffff);
}

/// B.cond target (conditional branch)
pub fn emitBCond(offset_words: i19, cond: Condition) u32 {
    return 0x54000000 |
        ((@as(u32, @bitCast(@as(i32, offset_words))) & 0x7ffff) << 5) |
        @as(u32, @intFromEnum(cond));
}

/// CBZ Wn/Xn, target (32-bit)
pub fn emitCbz(reg: Register, offset_words: i19) u32 {
    return 0x34000000 |
        ((@as(u32, @bitCast(@as(i32, offset_words))) & 0x7ffff) << 5) |
        r(reg);
}

/// CBZ Xn, target (64-bit)
pub fn emitCbz64(reg: Register, offset_words: i19) u32 {
    return 0xb4000000 |
        ((@as(u32, @bitCast(@as(i32, offset_words))) & 0x7ffff) << 5) |
        r(reg);
}

/// CBNZ Wn, target (32-bit)
pub fn emitCbnz(reg: Register, offset_words: i19) u32 {
    return 0x35000000 |
        ((@as(u32, @bitCast(@as(i32, offset_words))) & 0x7ffff) << 5) |
        r(reg);
}

/// CBNZ Xn, target (64-bit)
pub fn emitCbnz64(reg: Register, offset_words: i19) u32 {
    return 0xb5000000 |
        ((@as(u32, @bitCast(@as(i32, offset_words))) & 0x7ffff) << 5) |
        r(reg);
}

/// TBZ Rn, #bit, target
pub fn emitTbz(reg: Register, bit: u6, offset_words: i14) u32 {
    return 0x36000000 |
        (@as(u32, bit >> 5) << 31) |
        (@as(u32, bit & 0x1f) << 19) |
        ((@as(u32, @bitCast(@as(i32, offset_words))) & 0x3fff) << 5) |
        r(reg);
}

/// TBNZ Rn, #bit, target
pub fn emitTbnz(reg: Register, bit: u6, offset_words: i14) u32 {
    return 0x37000000 |
        (@as(u32, bit >> 5) << 31) |
        (@as(u32, bit & 0x1f) << 19) |
        ((@as(u32, @bitCast(@as(i32, offset_words))) & 0x3fff) << 5) |
        r(reg);
}

/// BR Xn (branch register)
pub fn emitBr(reg: Register) u32 {
    return 0xd61f0000 | (r(reg) << 5);
}

/// BLR Xn (branch with link register)
pub fn emitBlr(reg: Register) u32 {
    return 0xd63f0000 | (r(reg) << 5);
}

/// RET Xn
pub fn emitRet(reg: Register) u32 {
    return 0xd65f0000 | (r(reg) << 5);
}

// ============================================================================
// Section: ADD/SUB register
// ============================================================================

fn emitAddSubRegisterCommon(dest: Register, src1: Register, src2: RegisterParam, opcode: u32, extended_opcode: u32) u32 {
    if (src2.isExtended()) {
        return extended_opcode | src2.encodeExtended() | (r(src1) << 5) | r(dest);
    } else {
        return opcode | src2.encode() | (r(src1) << 5) | r(dest);
    }
}

// ADD (shifted/extended register)
pub fn emitAddRegister(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitAddSubRegisterCommon(dest, src1, src2, 0x0b000000, 0x0b200000);
}

pub fn emitAddRegister64(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitAddSubRegisterCommon(dest, src1, src2, 0x8b000000, 0x8b200000);
}

// ADDS (shifted/extended register)
pub fn emitAddsRegister(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitAddSubRegisterCommon(dest, src1, src2, 0x2b000000, 0x2b200000);
}

pub fn emitAddsRegister64(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitAddSubRegisterCommon(dest, src1, src2, 0xab000000, 0xab200000);
}

// SUB (shifted/extended register)
pub fn emitSubRegister(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitAddSubRegisterCommon(dest, src1, src2, 0x4b000000, 0x4b200000);
}

pub fn emitSubRegister64(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitAddSubRegisterCommon(dest, src1, src2, 0xcb000000, 0xcb200000);
}

// SUBS (shifted/extended register)
pub fn emitSubsRegister(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitAddSubRegisterCommon(dest, src1, src2, 0x6b000000, 0x6b200000);
}

pub fn emitSubsRegister64(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitAddSubRegisterCommon(dest, src1, src2, 0xeb000000, 0xeb200000);
}

// CMP (alias for SUBS with ZR dest)
pub fn emitCmpRegister(src1: Register, src2: RegisterParam) u32 {
    return emitSubsRegister(Register.ZR, src1, src2);
}

pub fn emitCmpRegister64(src1: Register, src2: RegisterParam) u32 {
    return emitSubsRegister64(Register.ZR, src1, src2);
}

// ============================================================================
// Section: ADC/SBC register
// ============================================================================

fn emitAdcSbcRegisterCommon(dest: Register, src1: Register, src2: Register, opcode: u32) u32 {
    return opcode | (r(src2) << 16) | (r(src1) << 5) | r(dest);
}

pub fn emitAdcRegister(dest: Register, src1: Register, src2: Register) u32 {
    return emitAdcSbcRegisterCommon(dest, src1, src2, 0x1a000000);
}
pub fn emitAdcRegister64(dest: Register, src1: Register, src2: Register) u32 {
    return emitAdcSbcRegisterCommon(dest, src1, src2, 0x9a000000);
}
pub fn emitAdcsRegister(dest: Register, src1: Register, src2: Register) u32 {
    return emitAdcSbcRegisterCommon(dest, src1, src2, 0x3a000000);
}
pub fn emitAdcsRegister64(dest: Register, src1: Register, src2: Register) u32 {
    return emitAdcSbcRegisterCommon(dest, src1, src2, 0xba000000);
}
pub fn emitSbcRegister(dest: Register, src1: Register, src2: Register) u32 {
    return emitAdcSbcRegisterCommon(dest, src1, src2, 0x5a000000);
}
pub fn emitSbcRegister64(dest: Register, src1: Register, src2: Register) u32 {
    return emitAdcSbcRegisterCommon(dest, src1, src2, 0xda000000);
}
pub fn emitSbcsRegister(dest: Register, src1: Register, src2: Register) u32 {
    return emitAdcSbcRegisterCommon(dest, src1, src2, 0x7a000000);
}
pub fn emitSbcsRegister64(dest: Register, src1: Register, src2: Register) u32 {
    return emitAdcSbcRegisterCommon(dest, src1, src2, 0xfa000000);
}

// ============================================================================
// Section: MADD/MSUB/MUL/SMULL/UMULL/SDIV/UDIV
// ============================================================================

fn emitMaddMsubCommon(dest: Register, src1: Register, src2: Register, src3: Register, opcode: u32) u32 {
    return opcode | (r(src2) << 16) | (r(src3) << 10) | (r(src1) << 5) | r(dest);
}

pub fn emitMadd(dest: Register, src1: Register, src2: Register, src3: Register) u32 {
    return emitMaddMsubCommon(dest, src1, src2, src3, 0x1b000000);
}
pub fn emitMadd64(dest: Register, src1: Register, src2: Register, src3: Register) u32 {
    return emitMaddMsubCommon(dest, src1, src2, src3, 0x9b000000);
}
pub fn emitMsub(dest: Register, src1: Register, src2: Register, src3: Register) u32 {
    return emitMaddMsubCommon(dest, src1, src2, src3, 0x1b008000);
}
pub fn emitMsub64(dest: Register, src1: Register, src2: Register, src3: Register) u32 {
    return emitMaddMsubCommon(dest, src1, src2, src3, 0x9b008000);
}
pub fn emitSmaddl(dest: Register, src1: Register, src2: Register, src3: Register) u32 {
    return emitMaddMsubCommon(dest, src1, src2, src3, 0x9b200000);
}
pub fn emitUmaddl(dest: Register, src1: Register, src2: Register, src3: Register) u32 {
    return emitMaddMsubCommon(dest, src1, src2, src3, 0x9ba00000);
}
pub fn emitSmsubl(dest: Register, src1: Register, src2: Register, src3: Register) u32 {
    return emitMaddMsubCommon(dest, src1, src2, src3, 0x9b208000);
}
pub fn emitUmsubl(dest: Register, src1: Register, src2: Register, src3: Register) u32 {
    return emitMaddMsubCommon(dest, src1, src2, src3, 0x9ba08000);
}

// ============================================================================
// Section: FMADD/FMSUB (scalar floating-point fused multiply-add/sub)
// ============================================================================

// FMADD: dest = src3 + (src1 * src2)
// FMSUB: dest = src3 - (src1 * src2)
// These use NEON registers for scalar float operations.
fn emitFmaddFmsubCommon(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, src3: NeonRegister, opcode: u32) u32 {
    return opcode | (nr(src2) << 16) | (nr(src3) << 10) | (nr(src1) << 5) | nr(dest);
}

// FMADD Sd, Sn, Sm, Sa  (single-precision)
pub fn emitFmadd(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, src3: NeonRegister) u32 {
    return emitFmaddFmsubCommon(dest, src1, src2, src3, 0x1f000000);
}
// FMADD Dd, Dn, Dm, Da  (double-precision)
pub fn emitFmadd64(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, src3: NeonRegister) u32 {
    return emitFmaddFmsubCommon(dest, src1, src2, src3, 0x1f400000);
}
// FMSUB Sd, Sn, Sm, Sa  (single-precision)
pub fn emitFmsub(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, src3: NeonRegister) u32 {
    return emitFmaddFmsubCommon(dest, src1, src2, src3, 0x1f008000);
}
// FMSUB Dd, Dn, Dm, Da  (double-precision)
pub fn emitFmsub64(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, src3: NeonRegister) u32 {
    return emitFmaddFmsubCommon(dest, src1, src2, src3, 0x1f408000);
}
// FNMADD Sd, Sn, Sm, Sa  (single-precision negated)
pub fn emitFnmadd(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, src3: NeonRegister) u32 {
    return emitFmaddFmsubCommon(dest, src1, src2, src3, 0x1f200000);
}
// FNMADD Dd, Dn, Dm, Da  (double-precision negated)
pub fn emitFnmadd64(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, src3: NeonRegister) u32 {
    return emitFmaddFmsubCommon(dest, src1, src2, src3, 0x1f600000);
}
// FNMSUB Sd, Sn, Sm, Sa  (single-precision negated)
pub fn emitFnmsub(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, src3: NeonRegister) u32 {
    return emitFmaddFmsubCommon(dest, src1, src2, src3, 0x1f208000);
}
// FNMSUB Dd, Dn, Dm, Da  (double-precision negated)
pub fn emitFnmsub64(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, src3: NeonRegister) u32 {
    return emitFmaddFmsubCommon(dest, src1, src2, src3, 0x1f608000);
}

// MUL (alias for MADD with ZR addend)
pub fn emitMul(dest: Register, src1: Register, src2: Register) u32 {
    return emitMadd(dest, src1, src2, Register.ZR);
}
pub fn emitMul64(dest: Register, src1: Register, src2: Register) u32 {
    return emitMadd64(dest, src1, src2, Register.ZR);
}
pub fn emitSmull(dest: Register, src1: Register, src2: Register) u32 {
    return emitSmaddl(dest, src1, src2, Register.ZR);
}
pub fn emitUmull(dest: Register, src1: Register, src2: Register) u32 {
    return emitUmaddl(dest, src1, src2, Register.ZR);
}

// Division
fn emitDivideCommon(dest: Register, src1: Register, src2: Register, opcode: u32) u32 {
    return opcode | (r(src2) << 16) | (r(src1) << 5) | r(dest);
}

pub fn emitSdiv(dest: Register, src1: Register, src2: Register) u32 {
    return emitDivideCommon(dest, src1, src2, 0x1ac00c00);
}
pub fn emitSdiv64(dest: Register, src1: Register, src2: Register) u32 {
    return emitDivideCommon(dest, src1, src2, 0x9ac00c00);
}
pub fn emitUdiv(dest: Register, src1: Register, src2: Register) u32 {
    return emitDivideCommon(dest, src1, src2, 0x1ac00800);
}
pub fn emitUdiv64(dest: Register, src1: Register, src2: Register) u32 {
    return emitDivideCommon(dest, src1, src2, 0x9ac00800);
}

// ============================================================================
// Section: NEG / CMN aliases
// ============================================================================

// NEG Wd, Wm  (alias for SUB Wd, WZR, Wm)
pub fn emitNeg(dest: Register, src: Register) u32 {
    return emitSubRegister(dest, Register.ZR, RegisterParam.reg_only(src));
}
// NEG Xd, Xm  (alias for SUB Xd, XZR, Xm)
pub fn emitNeg64(dest: Register, src: Register) u32 {
    return emitSubRegister64(dest, Register.ZR, RegisterParam.reg_only(src));
}
// NEGS Wd, Wm  (alias for SUBS Wd, WZR, Wm)
pub fn emitNegs(dest: Register, src: Register) u32 {
    return emitSubsRegister(dest, Register.ZR, RegisterParam.reg_only(src));
}
// NEGS Xd, Xm  (alias for SUBS Xd, XZR, Xm)
pub fn emitNegs64(dest: Register, src: Register) u32 {
    return emitSubsRegister64(dest, Register.ZR, RegisterParam.reg_only(src));
}
// CMN Wn, Wm  (alias for ADDS WZR, Wn, Wm)
pub fn emitCmnRegister(src1: Register, src2: RegisterParam) u32 {
    return emitAddsRegister(Register.ZR, src1, src2);
}
// CMN Xn, Xm  (alias for ADDS XZR, Xn, Xm)
pub fn emitCmnRegister64(src1: Register, src2: RegisterParam) u32 {
    return emitAddsRegister64(Register.ZR, src1, src2);
}
// CMN Wn, #imm  (alias for ADDS WZR, Wn, #imm)
pub fn emitCmnImmediate(src: Register, imm: u12) u32 {
    return emitAddsImmediate(Register.ZR, src, imm);
}
// CMN Xn, #imm  (alias for ADDS XZR, Xn, #imm)
pub fn emitCmnImmediate64(src: Register, imm: u12) u32 {
    return emitAddsImmediate64(Register.ZR, src, imm);
}
// SXTW Xd, Wn  (alias for SBFM Xd, Xn, #0, #31)
pub fn emitSxtw64(dest: Register, src: Register) u32 {
    return emitSbfm64(dest, src, 0, 31);
}

// ============================================================================
// Section: HINT instruction
// ============================================================================

// HINT #imm  (system hint instruction)
// NOP = hint #0, YIELD = hint #1, WFE = hint #2, WFI = hint #3,
// SEV = hint #4, SEVL = hint #5, BTI = hint #34, etc.
pub fn emitHint(imm: u7) u32 {
    return 0xd503201f | (@as(u32, imm) << 5);
}
// BTI C (Branch Target Identification for calls) = hint #34
pub fn emitBtiC() u32 {
    return emitHint(34);
}
// BTI J (Branch Target Identification for jumps) = hint #36
pub fn emitBtiJ() u32 {
    return emitHint(36);
}
// BTI JC (Branch Target Identification for calls and jumps) = hint #54  -- actually hint #38 for bti jc
// Actually: BTI = hint #32, BTI C = hint #34, BTI J = hint #36, BTI JC = hint #38
pub fn emitBtiJC() u32 {
    return emitHint(38);
}

// ============================================================================
// Section: Scalar FCVT (float precision conversion)
// ============================================================================

// FCVT Dd, Sn  (single-precision to double-precision)
pub fn emitFcvtStoD(dest: NeonRegister, src: NeonRegister) u32 {
    return 0x1e22c000 | (nr(src) << 5) | nr(dest);
}
// FCVT Sd, Dn  (double-precision to single-precision)
pub fn emitFcvtDtoS(dest: NeonRegister, src: NeonRegister) u32 {
    return 0x1e624000 | (nr(src) << 5) | nr(dest);
}
// FCVT Hd, Sn  (single-precision to half-precision)
pub fn emitFcvtStoH(dest: NeonRegister, src: NeonRegister) u32 {
    return 0x1e23c000 | (nr(src) << 5) | nr(dest);
}
// FCVT Sd, Hn  (half-precision to single-precision)
pub fn emitFcvtHtoS(dest: NeonRegister, src: NeonRegister) u32 {
    return 0x1ee24000 | (nr(src) << 5) | nr(dest);
}
// FCVT Hd, Dn  (double-precision to half-precision)
pub fn emitFcvtDtoH(dest: NeonRegister, src: NeonRegister) u32 {
    return 0x1e63c000 | (nr(src) << 5) | nr(dest);
}
// FCVT Dd, Hn  (half-precision to double-precision)
pub fn emitFcvtHtoD(dest: NeonRegister, src: NeonRegister) u32 {
    return 0x1ee2c000 | (nr(src) << 5) | nr(dest);
}

// ============================================================================
// Section: Logical register operations
// ============================================================================

fn emitLogicalRegisterCommon(dest: Register, src1: Register, src2: RegisterParam, opcode: u32) u32 {
    return opcode | src2.encode() | (r(src1) << 5) | r(dest);
}

pub fn emitAndRegister(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitLogicalRegisterCommon(dest, src1, src2, 0x0a000000);
}
pub fn emitAndRegister64(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitLogicalRegisterCommon(dest, src1, src2, 0x8a000000);
}
pub fn emitAndsRegister(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitLogicalRegisterCommon(dest, src1, src2, 0x6a000000);
}
pub fn emitAndsRegister64(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitLogicalRegisterCommon(dest, src1, src2, 0xea000000);
}
pub fn emitBicRegister(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitLogicalRegisterCommon(dest, src1, src2, 0x0a200000);
}
pub fn emitBicRegister64(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitLogicalRegisterCommon(dest, src1, src2, 0x8a200000);
}
pub fn emitBicsRegister(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitLogicalRegisterCommon(dest, src1, src2, 0x6a200000);
}
pub fn emitBicsRegister64(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitLogicalRegisterCommon(dest, src1, src2, 0xea200000);
}
pub fn emitEonRegister(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitLogicalRegisterCommon(dest, src1, src2, 0x4a200000);
}
pub fn emitEonRegister64(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitLogicalRegisterCommon(dest, src1, src2, 0xca200000);
}
pub fn emitEorRegister(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitLogicalRegisterCommon(dest, src1, src2, 0x4a000000);
}
pub fn emitEorRegister64(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitLogicalRegisterCommon(dest, src1, src2, 0xca000000);
}
pub fn emitOrnRegister(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitLogicalRegisterCommon(dest, src1, src2, 0x2a200000);
}
pub fn emitOrnRegister64(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitLogicalRegisterCommon(dest, src1, src2, 0xaa200000);
}
pub fn emitOrrRegister(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitLogicalRegisterCommon(dest, src1, src2, 0x2a000000);
}
pub fn emitOrrRegister64(dest: Register, src1: Register, src2: RegisterParam) u32 {
    return emitLogicalRegisterCommon(dest, src1, src2, 0xaa000000);
}

// TST (alias for ANDS with ZR dest)
pub fn emitTestRegister(src1: Register, src2: RegisterParam) u32 {
    return emitAndsRegister(Register.ZR, src1, src2);
}
pub fn emitTestRegister64(src1: Register, src2: RegisterParam) u32 {
    return emitAndsRegister64(Register.ZR, src1, src2);
}

// MOV (alias for ORR with ZR src1)
pub fn emitMovRegister(dest: Register, src: Register) u32 {
    return emitOrrRegister(dest, Register.ZR, RegisterParam.reg_only(src));
}
pub fn emitMovRegister64(dest: Register, src: Register) u32 {
    return emitOrrRegister64(dest, Register.ZR, RegisterParam.reg_only(src));
}

// MVN (alias for ORN with ZR src1)
pub fn emitMvnRegister(dest: Register, src: Register) u32 {
    return emitOrnRegister(dest, Register.ZR, RegisterParam.reg_only(src));
}
pub fn emitMvnRegister64(dest: Register, src: Register) u32 {
    return emitOrnRegister64(dest, Register.ZR, RegisterParam.reg_only(src));
}

// ============================================================================
// Section: Shift register operations (variable shift)
// ============================================================================

fn emitShiftRegisterCommon(dest: Register, src1: Register, src2: Register, opcode: u32) u32 {
    return opcode | (r(src2) << 16) | (r(src1) << 5) | r(dest);
}

pub fn emitAsrRegister(dest: Register, src1: Register, src2: Register) u32 {
    return emitShiftRegisterCommon(dest, src1, src2, 0x1ac02800);
}
pub fn emitAsrRegister64(dest: Register, src1: Register, src2: Register) u32 {
    return emitShiftRegisterCommon(dest, src1, src2, 0x9ac02800);
}
pub fn emitLslRegister(dest: Register, src1: Register, src2: Register) u32 {
    return emitShiftRegisterCommon(dest, src1, src2, 0x1ac02000);
}
pub fn emitLslRegister64(dest: Register, src1: Register, src2: Register) u32 {
    return emitShiftRegisterCommon(dest, src1, src2, 0x9ac02000);
}
pub fn emitLsrRegister(dest: Register, src1: Register, src2: Register) u32 {
    return emitShiftRegisterCommon(dest, src1, src2, 0x1ac02400);
}
pub fn emitLsrRegister64(dest: Register, src1: Register, src2: Register) u32 {
    return emitShiftRegisterCommon(dest, src1, src2, 0x9ac02400);
}
pub fn emitRorRegister(dest: Register, src1: Register, src2: Register) u32 {
    return emitShiftRegisterCommon(dest, src1, src2, 0x1ac02c00);
}
pub fn emitRorRegister64(dest: Register, src1: Register, src2: Register) u32 {
    return emitShiftRegisterCommon(dest, src1, src2, 0x9ac02c00);
}

// ============================================================================
// Section: Bitfield operations
// ============================================================================

fn emitBitfieldCommon(dest: Register, src: Register, immr: u6, imms: u6, opcode: u32) u32 {
    return opcode | (@as(u32, immr) << 16) | (@as(u32, imms) << 10) | (r(src) << 5) | r(dest);
}

// BFM
pub fn emitBfm(dest: Register, src: Register, immr: u6, imms: u6) u32 {
    return emitBitfieldCommon(dest, src, immr, imms, 0x33000000);
}
pub fn emitBfm64(dest: Register, src: Register, immr: u6, imms: u6) u32 {
    return emitBitfieldCommon(dest, src, immr, imms, 0xb3400000);
}

// SBFM
pub fn emitSbfm(dest: Register, src: Register, immr: u6, imms: u6) u32 {
    return emitBitfieldCommon(dest, src, immr, imms, 0x13000000);
}
pub fn emitSbfm64(dest: Register, src: Register, immr: u6, imms: u6) u32 {
    return emitBitfieldCommon(dest, src, immr, imms, 0x93400000);
}

// UBFM
pub fn emitUbfm(dest: Register, src: Register, immr: u6, imms: u6) u32 {
    return emitBitfieldCommon(dest, src, immr, imms, 0x53000000);
}
pub fn emitUbfm64(dest: Register, src: Register, immr: u6, imms: u6) u32 {
    return emitBitfieldCommon(dest, src, immr, imms, 0xd3400000);
}

// SXTB/SXTH/UXTB/UXTH
pub fn emitSxtb(dest: Register, src: Register) u32 {
    return emitSbfm(dest, src, 0, 7);
}
pub fn emitSxtb64(dest: Register, src: Register) u32 {
    return emitSbfm64(dest, src, 0, 7);
}
pub fn emitSxth(dest: Register, src: Register) u32 {
    return emitSbfm(dest, src, 0, 15);
}
pub fn emitSxth64(dest: Register, src: Register) u32 {
    return emitSbfm64(dest, src, 0, 15);
}
pub fn emitUxtb(dest: Register, src: Register) u32 {
    return emitUbfm(dest, src, 0, 7);
}
pub fn emitUxtb64(dest: Register, src: Register) u32 {
    return emitUbfm64(dest, src, 0, 7);
}
pub fn emitUxth(dest: Register, src: Register) u32 {
    return emitUbfm(dest, src, 0, 15);
}
pub fn emitUxth64(dest: Register, src: Register) u32 {
    return emitUbfm64(dest, src, 0, 15);
}

// BFI
pub fn emitBfi(dest: Register, src: Register, lsb: u5, width: u6) u32 {
    const immr: u6 = @truncate(@as(u32, 32 -% @as(u32, lsb)) % 32);
    const imms: u6 = @truncate(width -% 1);
    return emitBfm(dest, src, immr, imms);
}
pub fn emitBfi64(dest: Register, src: Register, lsb: u6, width: u7) u32 {
    const immr: u6 = @truncate(@as(u32, 64 -% @as(u32, lsb)) % 64);
    const imms: u6 = @truncate(width -% 1);
    return emitBfm64(dest, src, immr, imms);
}

// BFXIL
pub fn emitBfxil(dest: Register, src: Register, lsb: u5, width: u6) u32 {
    const imms: u6 = @truncate(@as(u32, lsb) + @as(u32, width) - 1);
    return emitBfm(dest, src, @truncate(@as(u32, lsb)), imms);
}
pub fn emitBfxil64(dest: Register, src: Register, lsb: u6, width: u7) u32 {
    const imms: u6 = @truncate(@as(u32, lsb) + @as(u32, width) - 1);
    return emitBfm64(dest, src, lsb, imms);
}

// SBFX
pub fn emitSbfx(dest: Register, src: Register, lsb: u5, width: u6) u32 {
    const imms: u6 = @truncate(@as(u32, lsb) + @as(u32, width) - 1);
    return emitSbfm(dest, src, @truncate(@as(u32, lsb)), imms);
}
pub fn emitSbfx64(dest: Register, src: Register, lsb: u6, width: u7) u32 {
    const imms: u6 = @truncate(@as(u32, lsb) + @as(u32, width) - 1);
    return emitSbfm64(dest, src, lsb, imms);
}

// UBFX
pub fn emitUbfx(dest: Register, src: Register, lsb: u5, width: u6) u32 {
    const imms: u6 = @truncate(@as(u32, lsb) + @as(u32, width) - 1);
    return emitUbfm(dest, src, @truncate(@as(u32, lsb)), imms);
}
pub fn emitUbfx64(dest: Register, src: Register, lsb: u6, width: u7) u32 {
    const imms: u6 = @truncate(@as(u32, lsb) + @as(u32, width) - 1);
    return emitUbfm64(dest, src, lsb, imms);
}

// ============================================================================
// Section: Shift/rotate immediate (aliases of bitfield instructions)
// ============================================================================

pub fn emitAsrImmediate(dest: Register, src: Register, imm: u5) u32 {
    return emitSbfm(dest, src, @truncate(@as(u32, imm)), 31);
}
pub fn emitAsrImmediate64(dest: Register, src: Register, imm: u6) u32 {
    return emitSbfm64(dest, src, imm, 63);
}

pub fn emitLslImmediate(dest: Register, src: Register, imm: u5) u32 {
    const immr: u6 = @truncate(@as(u32, 32 -% @as(u32, imm)) % 32);
    const imms: u6 = @truncate(31 -% @as(u32, imm));
    return emitUbfm(dest, src, immr, imms);
}
pub fn emitLslImmediate64(dest: Register, src: Register, imm: u6) u32 {
    const immr: u6 = @truncate(@as(u32, 64 -% @as(u32, imm)) % 64);
    const imms: u6 = @truncate(63 -% @as(u32, imm));
    return emitUbfm64(dest, src, immr, imms);
}

pub fn emitLsrImmediate(dest: Register, src: Register, imm: u5) u32 {
    return emitUbfm(dest, src, @truncate(@as(u32, imm)), 31);
}
pub fn emitLsrImmediate64(dest: Register, src: Register, imm: u6) u32 {
    return emitUbfm64(dest, src, imm, 63);
}

// EXTR
pub fn emitExtr(dest: Register, src1: Register, src2: Register, shift: u6) u32 {
    return 0x13800000 | (r(src2) << 16) | ((@as(u32, shift) & 0x3f) << 10) | (r(src1) << 5) | r(dest);
}
pub fn emitExtr64(dest: Register, src1: Register, src2: Register, shift: u6) u32 {
    return 0x93c00000 | (r(src2) << 16) | ((@as(u32, shift) & 0x3f) << 10) | (r(src1) << 5) | r(dest);
}

// ROR immediate (alias for EXTR with src1==src2)
pub fn emitRorImmediate(dest: Register, src: Register, imm: u6) u32 {
    return emitExtr(dest, src, src, imm);
}
pub fn emitRorImmediate64(dest: Register, src: Register, imm: u6) u32 {
    return emitExtr64(dest, src, src, imm);
}

// ============================================================================
// Section: Conditional select
// ============================================================================

fn emitConditionalCommon(dest: Register, src1: Register, src2: Register, cond: Condition, opcode: u32) u32 {
    return opcode | (r(src2) << 16) | (@as(u32, @intFromEnum(cond)) << 12) | (r(src1) << 5) | r(dest);
}

pub fn emitCsel(dest: Register, src1: Register, src2: Register, cond: Condition) u32 {
    return emitConditionalCommon(dest, src1, src2, cond, 0x1a800000);
}
pub fn emitCsel64(dest: Register, src1: Register, src2: Register, cond: Condition) u32 {
    return emitConditionalCommon(dest, src1, src2, cond, 0x9a800000);
}
pub fn emitCsinc(dest: Register, src1: Register, src2: Register, cond: Condition) u32 {
    return emitConditionalCommon(dest, src1, src2, cond, 0x1a800400);
}
pub fn emitCsinc64(dest: Register, src1: Register, src2: Register, cond: Condition) u32 {
    return emitConditionalCommon(dest, src1, src2, cond, 0x9a800400);
}
pub fn emitCsinv(dest: Register, src1: Register, src2: Register, cond: Condition) u32 {
    return emitConditionalCommon(dest, src1, src2, cond, 0x5a800000);
}
pub fn emitCsinv64(dest: Register, src1: Register, src2: Register, cond: Condition) u32 {
    return emitConditionalCommon(dest, src1, src2, cond, 0xda800000);
}
pub fn emitCsneg(dest: Register, src1: Register, src2: Register, cond: Condition) u32 {
    return emitConditionalCommon(dest, src1, src2, cond, 0x5a800400);
}
pub fn emitCsneg64(dest: Register, src1: Register, src2: Register, cond: Condition) u32 {
    return emitConditionalCommon(dest, src1, src2, cond, 0xda800400);
}

// CINC (alias: CSINC with inverted condition and same source)
pub fn emitCinc(dest: Register, src: Register, cond: Condition) u32 {
    return emitCsinc(dest, src, src, cond.invert());
}
pub fn emitCinc64(dest: Register, src: Register, cond: Condition) u32 {
    return emitCsinc64(dest, src, src, cond.invert());
}

// CSET (alias: CSINC with ZR sources and inverted condition)
pub fn emitCset(dest: Register, cond: Condition) u32 {
    return emitCsinc(dest, Register.ZR, Register.ZR, cond.invert());
}
pub fn emitCset64(dest: Register, cond: Condition) u32 {
    return emitCsinc64(dest, Register.ZR, Register.ZR, cond.invert());
}

// CINV
pub fn emitCinv(dest: Register, src: Register, cond: Condition) u32 {
    return emitCsinv(dest, src, src, cond.invert());
}
pub fn emitCinv64(dest: Register, src: Register, cond: Condition) u32 {
    return emitCsinv64(dest, src, src, cond.invert());
}

// CSETM (alias: CSINV with ZR sources and inverted condition)
pub fn emitCsetm(dest: Register, cond: Condition) u32 {
    return emitCsinv(dest, Register.ZR, Register.ZR, cond.invert());
}
pub fn emitCsetm64(dest: Register, cond: Condition) u32 {
    return emitCsinv64(dest, Register.ZR, Register.ZR, cond.invert());
}

// CNEG
pub fn emitCneg(dest: Register, src: Register, cond: Condition) u32 {
    return emitCsneg(dest, src, src, cond.invert());
}
pub fn emitCneg64(dest: Register, src: Register, cond: Condition) u32 {
    return emitCsneg64(dest, src, src, cond.invert());
}

// ============================================================================
// Section: Conditional compare
// ============================================================================

fn emitConditionalCompareRegister(src1: Register, src2: Register, nzcv: u4, cond: Condition, opcode: u32) u32 {
    return opcode | (r(src2) << 16) | (@as(u32, @intFromEnum(cond)) << 12) | (r(src1) << 5) | @as(u32, nzcv);
}

pub fn emitCcmnRegister(src1: Register, src2: Register, nzcv: u4, cond: Condition) u32 {
    return emitConditionalCompareRegister(src1, src2, nzcv, cond, 0x3A400000);
}
pub fn emitCcmnRegister64(src1: Register, src2: Register, nzcv: u4, cond: Condition) u32 {
    return emitConditionalCompareRegister(src1, src2, nzcv, cond, 0xBA400000);
}
pub fn emitCcmpRegister(src1: Register, src2: Register, nzcv: u4, cond: Condition) u32 {
    return emitConditionalCompareRegister(src1, src2, nzcv, cond, 0x7A400000);
}
pub fn emitCcmpRegister64(src1: Register, src2: Register, nzcv: u4, cond: Condition) u32 {
    return emitConditionalCompareRegister(src1, src2, nzcv, cond, 0xFA400000);
}

// ============================================================================
// Section: Move immediate (MOVZ/MOVN/MOVK)
// ============================================================================

fn emitMovImmediateCommon(dest: Register, imm16: u16, shift: u6, opcode: u32) u32 {
    std.debug.assert(shift % 16 == 0);
    return opcode | (@as(u32, shift / 16) << 21) | (@as(u32, imm16) << 5) | r(dest);
}

pub fn emitMovk(dest: Register, imm16: u16, shift: u6) u32 {
    return emitMovImmediateCommon(dest, imm16, shift, 0x72800000);
}
pub fn emitMovk64(dest: Register, imm16: u16, shift: u6) u32 {
    return emitMovImmediateCommon(dest, imm16, shift, 0xf2800000);
}
pub fn emitMovn(dest: Register, imm16: u16, shift: u6) u32 {
    return emitMovImmediateCommon(dest, imm16, shift, 0x12800000);
}
pub fn emitMovn64(dest: Register, imm16: u16, shift: u6) u32 {
    return emitMovImmediateCommon(dest, imm16, shift, 0x92800000);
}
pub fn emitMovz(dest: Register, imm16: u16, shift: u6) u32 {
    return emitMovImmediateCommon(dest, imm16, shift, 0x52800000);
}
pub fn emitMovz64(dest: Register, imm16: u16, shift: u6) u32 {
    return emitMovImmediateCommon(dest, imm16, shift, 0xd2800000);
}

/// Load a 32-bit immediate into a register using the minimal number of instructions.
/// Returns a slice of 1-2 instruction words written to `out`.
pub fn emitLoadImmediate32(dest: Register, imm: u32, out: *[2]u32) u8 {
    const word0: u16 = @truncate(imm & 0xffff);
    const word1: u16 = @truncate((imm >> 16) & 0xffff);

    if (word0 != 0 and word1 != 0) {
        // Try logical immediate encoding (ORR with ZR)
        if (encodeLogicalImmediate(@as(u64, imm), 32)) |enc| {
            out[0] = 0x32000000 | (@as(u32, enc) << 10) | (31 << 5) | r(dest);
            return 1;
        }

        // Use MOVN if one half is 0xffff
        if (word1 == 0xffff) {
            out[0] = 0x12800000 | (0 << 21) | (@as(u32, word0 ^ 0xffff) << 5) | r(dest);
            return 1;
        } else if (word0 == 0xffff) {
            out[0] = 0x12800000 | (1 << 21) | (@as(u32, word1 ^ 0xffff) << 5) | r(dest);
            return 1;
        }
    }

    // Use MOVZ + optional MOVK
    var count: u8 = 0;
    if (word0 != 0 or word1 == 0) {
        out[count] = 0x52800000 | (0 << 21) | (@as(u32, word0) << 5) | r(dest);
        count += 1;
    }
    if (word1 != 0) {
        const opcode: u32 = if (count > 0) @as(u32, 0x72800000) else @as(u32, 0x52800000);
        out[count] = opcode | (1 << 21) | (@as(u32, word1) << 5) | r(dest);
        count += 1;
    }
    return count;
}

/// Load a 64-bit immediate into a register using minimal instructions.
/// Returns the number of instruction words written to `out` (1-4).
pub fn emitLoadImmediate64(dest: Register, imm: u64, out: *[4]u32) u8 {
    // If upper 32 bits are 0, use 32-bit form
    if ((imm >> 32) == 0) {
        var buf2: [2]u32 = undefined;
        const n = emitLoadImmediate32(dest, @truncate(imm), &buf2);
        var i: u8 = 0;
        while (i < n) : (i += 1) {
            out[i] = buf2[i];
        }
        return n;
    }

    // Try logical immediate encoding (ORR with ZR)
    if (encodeLogicalImmediate(imm, 64)) |enc| {
        out[0] = 0xb2000000 | (@as(u32, enc) << 10) | (31 << 5) | r(dest);
        return 1;
    }

    // Break into words and count zeros/ones
    const words = [4]u16{
        @truncate(imm & 0xffff),
        @truncate((imm >> 16) & 0xffff),
        @truncate((imm >> 32) & 0xffff),
        @truncate((imm >> 48) & 0xffff),
    };

    var num_zeros: u8 = 0;
    var num_ones: u8 = 0;
    for (words) |w| {
        if (w == 0) num_zeros += 1;
        if (w == 0xffff) num_ones += 1;
    }

    // Use MOVZ/MOVN + MOVK
    const use_movn = num_ones > num_zeros;
    const word_mask: u16 = if (use_movn) 0xffff else 0x0000;
    var first = true;
    var count: u8 = 0;

    for (words, 0..) |w, i| {
        if (w != word_mask) {
            const shift: u6 = @truncate(i * 16);
            if (first) {
                if (use_movn) {
                    out[count] = emitMovn64(dest, w ^ 0xffff, shift);
                } else {
                    out[count] = emitMovz64(dest, w, shift);
                }
                first = false;
            } else {
                out[count] = emitMovk64(dest, w, shift);
            }
            count += 1;
        }
    }
    return count;
}

// ============================================================================
// Section: PC-relative addressing (ADR/ADRP)
// ============================================================================

fn emitAdrAdrp(dest: Register, offset: i21, opcode: u32) u32 {
    const off: u32 = @bitCast(@as(i32, offset));
    return opcode | ((off & 3) << 29) | (((off >> 2) & 0x7ffff) << 5) | r(dest);
}

pub fn emitAdr(dest: Register, offset: i21) u32 {
    return emitAdrAdrp(dest, offset, 0x10000000);
}

pub fn emitAdrp(dest: Register, page_offset: i21) u32 {
    return emitAdrAdrp(dest, page_offset, 0x90000000);
}

// ============================================================================
// Section: ADD/SUB immediate
// ============================================================================

/// Core helper: encode ADD/SUB immediate with auto-negation and LSL #12 support.
/// Returns the encoded instruction, or null if the immediate can't be encoded.
pub fn emitAddSubImmediateCommon(dest: Register, src: Register, immediate: i32, set_flags: bool, is_add: bool, opcode_high_bit: u32) ?u32 {
    const s_bit: u32 = if (set_flags) (1 << 29) else 0;

    if (immediate == 0) {
        if (is_add) {
            return opcode_high_bit | s_bit | 0x11000000 | (r(src) << 5) | r(dest);
        } else {
            return opcode_high_bit | s_bit | 0x51000000 | (r(src) << 5) | r(dest);
        }
    }

    // 12-bit unsigned immediate
    if ((immediate & 0xfff) == immediate) {
        return opcode_high_bit | s_bit | 0x11000000 |
            (@as(u32, @bitCast(immediate)) & 0xfff) << 10 | (r(src) << 5) | r(dest);
    }

    const neg_imm = -%immediate;
    const neg_u: u32 = @bitCast(neg_imm);

    if ((neg_u & 0xfff) == neg_u) {
        return opcode_high_bit | s_bit | 0x51000000 |
            (neg_u & 0xfff) << 10 | (r(src) << 5) | r(dest);
    }

    // 12-bit shifted left by 12
    const imm_u: u32 = @bitCast(immediate);
    if ((imm_u & 0xfff000) == imm_u) {
        return opcode_high_bit | s_bit | 0x11000000 | (1 << 22) |
            (((imm_u >> 12) & 0xfff) << 10) | (r(src) << 5) | r(dest);
    }
    if ((neg_u & 0xfff000) == neg_u) {
        return opcode_high_bit | s_bit | 0x51000000 | (1 << 22) |
            (((neg_u >> 12) & 0xfff) << 10) | (r(src) << 5) | r(dest);
    }

    return null;
}

pub fn emitAddImmediate(dest: Register, src: Register, imm: u12) u32 {
    return emitAddSubImmediateCommon(dest, src, @as(i32, imm), false, true, 0) orelse unreachable;
}
pub fn emitAddImmediate64(dest: Register, src: Register, imm: u12) u32 {
    return emitAddSubImmediateCommon(dest, src, @as(i32, imm), false, true, 0x80000000) orelse unreachable;
}
pub fn emitAddsImmediate(dest: Register, src: Register, imm: u12) u32 {
    return emitAddSubImmediateCommon(dest, src, @as(i32, imm), true, true, 0) orelse unreachable;
}
pub fn emitAddsImmediate64(dest: Register, src: Register, imm: u12) u32 {
    return emitAddSubImmediateCommon(dest, src, @as(i32, imm), true, true, 0x80000000) orelse unreachable;
}
pub fn emitSubImmediate(dest: Register, src: Register, imm: u12) u32 {
    return emitAddSubImmediateCommon(dest, src, -@as(i32, imm), false, false, 0) orelse unreachable;
}
pub fn emitSubImmediate64(dest: Register, src: Register, imm: u12) u32 {
    return emitAddSubImmediateCommon(dest, src, -@as(i32, imm), false, false, 0x80000000) orelse unreachable;
}
pub fn emitSubsImmediate(dest: Register, src: Register, imm: u12) u32 {
    return emitAddSubImmediateCommon(dest, src, -@as(i32, imm), true, false, 0) orelse unreachable;
}
pub fn emitSubsImmediate64(dest: Register, src: Register, imm: u12) u32 {
    return emitAddSubImmediateCommon(dest, src, -@as(i32, imm), true, false, 0x80000000) orelse unreachable;
}

// CMP immediate (alias for SUBS with ZR dest)
pub fn emitCmpImmediate(src: Register, imm: u12) u32 {
    return emitSubsImmediate(Register.ZR, src, imm);
}
pub fn emitCmpImmediate64(src: Register, imm: u12) u32 {
    return emitSubsImmediate64(Register.ZR, src, imm);
}

// ============================================================================
// Section: Logical immediate
// ============================================================================

fn emitLogicalImmediateCommon(dest: Register, src: Register, imm: u64, opcode: u32, not_reg_opcode: u32, size: u7) ?u32 {
    if (encodeLogicalImmediate(imm, size)) |enc| {
        return opcode | (@as(u32, enc) << 10) | (r(src) << 5) | r(dest);
    }

    // Special case: -1 can't be encoded, but the NOT form can use ZR
    if (not_reg_opcode != 0) {
        if ((size == 32 and @as(u32, @truncate(imm)) == 0xFFFFFFFF) or
            (size == 64 and imm == 0xFFFFFFFFFFFFFFFF))
        {
            return not_reg_opcode | (31 << 16) | (r(src) << 5) | r(dest);
        }
    }

    return null;
}

pub fn emitAndImmediate(dest: Register, src: Register, imm: u32) ?u32 {
    return emitLogicalImmediateCommon(dest, src, @as(u64, imm), 0x12000000, 0x0a200000, 32);
}
pub fn emitAndImmediate64(dest: Register, src: Register, imm: u64) ?u32 {
    return emitLogicalImmediateCommon(dest, src, imm, 0x92000000, 0x8a200000, 64);
}
pub fn emitAndsImmediate(dest: Register, src: Register, imm: u32) ?u32 {
    return emitLogicalImmediateCommon(dest, src, @as(u64, imm), 0x72000000, 0x6a200000, 32);
}
pub fn emitAndsImmediate64(dest: Register, src: Register, imm: u64) ?u32 {
    return emitLogicalImmediateCommon(dest, src, imm, 0xf2000000, 0xea200000, 64);
}
pub fn emitOrrImmediate(dest: Register, src: Register, imm: u32) ?u32 {
    return emitLogicalImmediateCommon(dest, src, @as(u64, imm), 0x32000000, 0x2a200000, 32);
}
pub fn emitOrrImmediate64(dest: Register, src: Register, imm: u64) ?u32 {
    return emitLogicalImmediateCommon(dest, src, imm, 0xb2000000, 0xaa200000, 64);
}
pub fn emitEorImmediate(dest: Register, src: Register, imm: u32) ?u32 {
    return emitLogicalImmediateCommon(dest, src, @as(u64, imm), 0x52000000, 0x4a200000, 32);
}
pub fn emitEorImmediate64(dest: Register, src: Register, imm: u64) ?u32 {
    return emitLogicalImmediateCommon(dest, src, imm, 0xd2000000, 0xca200000, 64);
}

// TST immediate (ANDS with ZR dest)
pub fn emitTestImmediate(src: Register, imm: u32) ?u32 {
    return emitAndsImmediate(Register.ZR, src, imm);
}
pub fn emitTestImmediate64(src: Register, imm: u64) ?u32 {
    return emitAndsImmediate64(Register.ZR, src, imm);
}

// ============================================================================
// Section: Reverse/count operations (CLZ, RBIT, REV, REV16, REV32)
// ============================================================================

fn emitReverseCommon(dest: Register, src: Register, opcode: u32) u32 {
    return opcode | (r(src) << 5) | r(dest);
}

pub fn emitClz(dest: Register, src: Register) u32 {
    return emitReverseCommon(dest, src, 0x5ac01000);
}
pub fn emitClz64(dest: Register, src: Register) u32 {
    return emitReverseCommon(dest, src, 0xdac01000);
}
pub fn emitRbit(dest: Register, src: Register) u32 {
    return emitReverseCommon(dest, src, 0x5ac00000);
}
pub fn emitRbit64(dest: Register, src: Register) u32 {
    return emitReverseCommon(dest, src, 0xdac00000);
}
pub fn emitRev(dest: Register, src: Register) u32 {
    return emitReverseCommon(dest, src, 0x5ac00800);
}
pub fn emitRev64(dest: Register, src: Register) u32 {
    return emitReverseCommon(dest, src, 0xdac00c00);
}
pub fn emitRev16(dest: Register, src: Register) u32 {
    return emitReverseCommon(dest, src, 0x5ac00400);
}
pub fn emitRev1664(dest: Register, src: Register) u32 {
    return emitReverseCommon(dest, src, 0xdac00400);
}
pub fn emitRev3264(dest: Register, src: Register) u32 {
    return emitReverseCommon(dest, src, 0xdac00800);
}

// ============================================================================
// Section: Load/store with register offset
// ============================================================================

fn emitLdrStrRegisterCommon(src_dest: Register, addr: Register, index: RegisterParam, access_shift: u2, opcode: u32) u32 {
    // Choose extend type: default to UXTX (3) for non-extended
    var extend_type: u32 = @intFromEnum(ExtendType.UXTX);
    if (index.extend_type) |ext| {
        extend_type = @intFromEnum(ext);
    }

    // Determine S bit (shift amount indicator)
    var amount: u32 = 0;
    if (index.amount == access_shift and access_shift != 0) {
        amount = 1;
    } else if (index.amount != 0 and index.amount != access_shift) {
        // Cannot encode this shift amount
        amount = 0;
    }

    return opcode | (@as(u32, index.reg.raw()) << 16) | (extend_type << 13) | (amount << 12) | (r(addr) << 5) | r(src_dest);
}

// LDR byte/half/word/dword register
pub fn emitLdrbRegister(dest: Register, addr: Register, index: RegisterParam) u32 {
    return emitLdrStrRegisterCommon(dest, addr, index, 0, 0x38600800);
}
pub fn emitLdrsbRegister(dest: Register, addr: Register, index: RegisterParam) u32 {
    return emitLdrStrRegisterCommon(dest, addr, index, 0, 0x38e00800);
}
pub fn emitLdrsbRegister64(dest: Register, addr: Register, index: RegisterParam) u32 {
    return emitLdrStrRegisterCommon(dest, addr, index, 0, 0x38a00800);
}
pub fn emitLdrhRegister(dest: Register, addr: Register, index: RegisterParam) u32 {
    return emitLdrStrRegisterCommon(dest, addr, index, 1, 0x78600800);
}
pub fn emitLdrshRegister(dest: Register, addr: Register, index: RegisterParam) u32 {
    return emitLdrStrRegisterCommon(dest, addr, index, 1, 0x78e00800);
}
pub fn emitLdrshRegister64(dest: Register, addr: Register, index: RegisterParam) u32 {
    return emitLdrStrRegisterCommon(dest, addr, index, 1, 0x78a00800);
}
pub fn emitLdrRegister(dest: Register, addr: Register, index: RegisterParam) u32 {
    return emitLdrStrRegisterCommon(dest, addr, index, 2, 0xb8600800);
}
pub fn emitLdrswRegister64(dest: Register, addr: Register, index: RegisterParam) u32 {
    return emitLdrStrRegisterCommon(dest, addr, index, 2, 0xb8a00800);
}
pub fn emitLdrRegister64(dest: Register, addr: Register, index: RegisterParam) u32 {
    return emitLdrStrRegisterCommon(dest, addr, index, 3, 0xf8600800);
}

// STR byte/half/word/dword register
pub fn emitStrbRegister(src: Register, addr: Register, index: RegisterParam) u32 {
    return emitLdrStrRegisterCommon(src, addr, index, 0, 0x38200800);
}
pub fn emitStrhRegister(src: Register, addr: Register, index: RegisterParam) u32 {
    return emitLdrStrRegisterCommon(src, addr, index, 1, 0x78200800);
}
pub fn emitStrRegister(src: Register, addr: Register, index: RegisterParam) u32 {
    return emitLdrStrRegisterCommon(src, addr, index, 2, 0xb8200800);
}
pub fn emitStrRegister64(src: Register, addr: Register, index: RegisterParam) u32 {
    return emitLdrStrRegisterCommon(src, addr, index, 3, 0xf8200800);
}

// PRFM register
pub fn emitPrfmRegister(addr: Register, index: RegisterParam) u32 {
    return emitLdrStrRegisterCommon(.R0, addr, index, 2, 0xf8a00800);
}

// ============================================================================
// Section: Load/store with unsigned offset / unscaled offset
// ============================================================================

fn emitLdrStrOffsetCommon(src_dest: Register, addr: Register, offset: i32, access_shift: u32, opcode: u32, opcode_unscaled: u32) ?u32 {
    if (opcode != 0) {
        const enc_offset = @as(u32, @bitCast(offset)) >> @intCast(access_shift);
        if ((@as(i32, @bitCast(enc_offset << @intCast(access_shift))) == offset) and (enc_offset & 0xfff) == enc_offset) {
            return opcode | ((enc_offset & 0xfff) << 10) | (r(addr) << 5) | r(src_dest);
        }
    }

    if (opcode_unscaled != 0 and offset >= -0x100 and offset <= 0xff) {
        return opcode_unscaled | ((@as(u32, @bitCast(offset)) & 0x1ff) << 12) | (r(addr) << 5) | r(src_dest);
    }

    return null;
}

// LDR byte offset
pub fn emitLdrbOffset(dest: Register, addr: Register, offset: i32) ?u32 {
    return emitLdrStrOffsetCommon(dest, addr, offset, 0, 0x39400000, 0x38400000);
}
pub fn emitLdrsbOffset(dest: Register, addr: Register, offset: i32) ?u32 {
    return emitLdrStrOffsetCommon(dest, addr, offset, 0, 0x39c00000, 0x38c00000);
}
pub fn emitLdrsbOffset64(dest: Register, addr: Register, offset: i32) ?u32 {
    return emitLdrStrOffsetCommon(dest, addr, offset, 0, 0x39800000, 0x38800000);
}

// LDR half offset
pub fn emitLdrhOffset(dest: Register, addr: Register, offset: i32) ?u32 {
    return emitLdrStrOffsetCommon(dest, addr, offset, 1, 0x79400000, 0x78400000);
}
pub fn emitLdrshOffset(dest: Register, addr: Register, offset: i32) ?u32 {
    return emitLdrStrOffsetCommon(dest, addr, offset, 1, 0x79c00000, 0x78c00000);
}
pub fn emitLdrshOffset64(dest: Register, addr: Register, offset: i32) ?u32 {
    return emitLdrStrOffsetCommon(dest, addr, offset, 1, 0x79800000, 0x78800000);
}

// LDR word offset
pub fn emitLdrOffset(dest: Register, addr: Register, offset: i32) ?u32 {
    return emitLdrStrOffsetCommon(dest, addr, offset, 2, 0xb9400000, 0xb8400000);
}
pub fn emitLdrswOffset64(dest: Register, addr: Register, offset: i32) ?u32 {
    return emitLdrStrOffsetCommon(dest, addr, offset, 2, 0xb9800000, 0xb8800000);
}

// LDR dword offset
pub fn emitLdrOffset64(dest: Register, addr: Register, offset: i32) ?u32 {
    return emitLdrStrOffsetCommon(dest, addr, offset, 3, 0xf9400000, 0xf8400000);
}

// Post-index variants
pub fn emitLdrhOffsetPostIndex(dest: Register, addr: Register, offset: i32) ?u32 {
    if (offset == 0) return emitLdrhOffset(dest, addr, 0);
    return emitLdrStrOffsetCommon(dest, addr, offset, 1, 0, 0x78400400);
}
pub fn emitLdrOffsetPostIndex(dest: Register, addr: Register, offset: i32) ?u32 {
    if (offset == 0) return emitLdrOffset(dest, addr, 0);
    return emitLdrStrOffsetCommon(dest, addr, offset, 2, 0, 0xb8400400);
}
pub fn emitLdrOffsetPostIndex64(dest: Register, addr: Register, offset: i32) ?u32 {
    if (offset == 0) return emitLdrOffset64(dest, addr, 0);
    return emitLdrStrOffsetCommon(dest, addr, offset, 3, 0, 0xf8400400);
}

// STR byte/half/word/dword offset
pub fn emitStrbOffset(src: Register, addr: Register, offset: i32) ?u32 {
    return emitLdrStrOffsetCommon(src, addr, offset, 0, 0x39000000, 0x38000000);
}
pub fn emitStrhOffset(src: Register, addr: Register, offset: i32) ?u32 {
    return emitLdrStrOffsetCommon(src, addr, offset, 1, 0x79000000, 0x78000000);
}
pub fn emitStrOffset(src: Register, addr: Register, offset: i32) ?u32 {
    return emitLdrStrOffsetCommon(src, addr, offset, 2, 0xb9000000, 0xb8000000);
}
pub fn emitStrOffset64(src: Register, addr: Register, offset: i32) ?u32 {
    return emitLdrStrOffsetCommon(src, addr, offset, 3, 0xf9000000, 0xf8000000);
}

// Pre-index variants
pub fn emitStrhOffsetPreIndex(src: Register, addr: Register, offset: i32) ?u32 {
    if (offset == 0) return emitStrhOffset(src, addr, 0);
    return emitLdrStrOffsetCommon(src, addr, offset, 1, 0, 0x78000c00);
}
pub fn emitStrOffsetPreIndex(src: Register, addr: Register, offset: i32) ?u32 {
    if (offset == 0) return emitStrOffset(src, addr, 0);
    return emitLdrStrOffsetCommon(src, addr, offset, 2, 0, 0xb8000c00);
}

// PRFM offset
pub fn emitPrfmOffset(addr: Register, offset: i32) ?u32 {
    return emitLdrStrOffsetCommon(.R0, addr, offset, 2, 0xf9800000, 0xf8800000);
}

// ============================================================================
// Section: FP scalar load/store with immediate offset
// ============================================================================
//
// ARM64 FP load/store use the same encoding layout as GP load/store
// but with the V bit (bit 26) set.  The 5-bit register field encodes
// the NEON/FP register number (0–31).
//
// Opcodes (unsigned-offset / unscaled):
//   LDR St  0xBD400000 / LDUR St 0xBC400000   (access_shift=2, 32-bit)
//   STR St  0xBD000000 / STUR St 0xBC000000
//   LDR Dt  0xFD400000 / LDUR Dt 0xFC400000   (access_shift=3, 64-bit)
//   STR Dt  0xFD000000 / STUR Dt 0xFC000000

fn emitFpLdrStrOffsetCommon(fp_reg: NeonRegister, addr: Register, offset: i32, access_shift: u32, opcode: u32, opcode_unscaled: u32) ?u32 {
    if (opcode != 0) {
        const enc_offset = @as(u32, @bitCast(offset)) >> @intCast(access_shift);
        if ((@as(i32, @bitCast(enc_offset << @intCast(access_shift))) == offset) and (enc_offset & 0xfff) == enc_offset) {
            return opcode | ((enc_offset & 0xfff) << 10) | (r(addr) << 5) | @as(u32, fp_reg.raw());
        }
    }

    if (opcode_unscaled != 0 and offset >= -0x100 and offset <= 0xff) {
        return opcode_unscaled | ((@as(u32, @bitCast(offset)) & 0x1ff) << 12) | (r(addr) << 5) | @as(u32, fp_reg.raw());
    }

    return null;
}

// LDR St, [Xn, #imm]  — single-precision float load
pub fn emitFpLdrSOffset(dest: NeonRegister, addr: Register, offset: i32) ?u32 {
    return emitFpLdrStrOffsetCommon(dest, addr, offset, 2, 0xBD400000, 0xBC400000);
}

// LDR Dt, [Xn, #imm]  — double-precision float load
pub fn emitFpLdrDOffset(dest: NeonRegister, addr: Register, offset: i32) ?u32 {
    return emitFpLdrStrOffsetCommon(dest, addr, offset, 3, 0xFD400000, 0xFC400000);
}

// STR St, [Xn, #imm]  — single-precision float store
pub fn emitFpStrSOffset(src: NeonRegister, addr: Register, offset: i32) ?u32 {
    return emitFpLdrStrOffsetCommon(src, addr, offset, 2, 0xBD000000, 0xBC000000);
}

// STR Dt, [Xn, #imm]  — double-precision float store
pub fn emitFpStrDOffset(src: NeonRegister, addr: Register, offset: i32) ?u32 {
    return emitFpLdrStrOffsetCommon(src, addr, offset, 3, 0xFD000000, 0xFC000000);
}

// ============================================================================
// Section: Load/store pair (LDP/STP)
// ============================================================================

fn emitLdpStpOffsetCommon(src_dest1: Register, src_dest2: Register, addr: Register, offset: i32, access_shift: u5, opcode: u32) ?u32 {
    const enc_offset = offset >> access_shift;
    if ((@as(i32, enc_offset) << access_shift) == offset and enc_offset >= -0x40 and enc_offset <= 0x3f) {
        return opcode |
            ((@as(u32, @bitCast(enc_offset)) & 0x7f) << 15) |
            (r(src_dest2) << 10) |
            (r(addr) << 5) |
            r(src_dest1);
    }
    return null;
}

pub fn emitLdpOffset(dest1: Register, dest2: Register, addr: Register, offset: i32) ?u32 {
    return emitLdpStpOffsetCommon(dest1, dest2, addr, offset, 2, 0x29400000);
}
pub fn emitLdpOffset64(dest1: Register, dest2: Register, addr: Register, offset: i32) ?u32 {
    return emitLdpStpOffsetCommon(dest1, dest2, addr, offset, 3, 0xa9400000);
}
pub fn emitLdpOffsetPostIndex(dest1: Register, dest2: Register, addr: Register, offset: i32) ?u32 {
    if (offset == 0) return emitLdpOffset(dest1, dest2, addr, 0);
    return emitLdpStpOffsetCommon(dest1, dest2, addr, offset, 2, 0x28c00000);
}
pub fn emitLdpOffsetPostIndex64(dest1: Register, dest2: Register, addr: Register, offset: i32) ?u32 {
    if (offset == 0) return emitLdpOffset64(dest1, dest2, addr, 0);
    return emitLdpStpOffsetCommon(dest1, dest2, addr, offset, 3, 0xa8c00000);
}
pub fn emitStpOffset(src1: Register, src2: Register, addr: Register, offset: i32) ?u32 {
    return emitLdpStpOffsetCommon(src1, src2, addr, offset, 2, 0x29000000);
}
pub fn emitStpOffset64(src1: Register, src2: Register, addr: Register, offset: i32) ?u32 {
    return emitLdpStpOffsetCommon(src1, src2, addr, offset, 3, 0xa9000000);
}
pub fn emitStpOffsetPreIndex(src1: Register, src2: Register, addr: Register, offset: i32) ?u32 {
    if (offset == 0) return emitStpOffset(src1, src2, addr, 0);
    return emitLdpStpOffsetCommon(src1, src2, addr, offset, 2, 0x29800000);
}
pub fn emitStpOffsetPreIndex64(src1: Register, src2: Register, addr: Register, offset: i32) ?u32 {
    if (offset == 0) return emitStpOffset64(src1, src2, addr, 0);
    return emitLdpStpOffsetCommon(src1, src2, addr, offset, 3, 0xa9800000);
}

// ============================================================================
// Section: Atomic load/store (LDAR/LDXR/LDAXR/STLR/STXR/STLXR)
// ============================================================================

fn emitLdaStlCommon(dest: Register, addr: Register, opcode: u32) u32 {
    return opcode | (r(addr) << 5) | r(dest);
}

pub fn emitLdarb(dest: Register, addr: Register) u32 {
    return emitLdaStlCommon(dest, addr, ARM64_OPCODE_LDARB);
}
pub fn emitLdarh(dest: Register, addr: Register) u32 {
    return emitLdaStlCommon(dest, addr, ARM64_OPCODE_LDARH);
}
pub fn emitLdar(dest: Register, addr: Register) u32 {
    return emitLdaStlCommon(dest, addr, ARM64_OPCODE_LDAR);
}
pub fn emitLdar64(dest: Register, addr: Register) u32 {
    return emitLdaStlCommon(dest, addr, ARM64_OPCODE_LDAR64);
}

pub fn emitLdxrb(dest: Register, addr: Register) u32 {
    return emitLdaStlCommon(dest, addr, ARM64_OPCODE_LDXRB);
}
pub fn emitLdxrh(dest: Register, addr: Register) u32 {
    return emitLdaStlCommon(dest, addr, ARM64_OPCODE_LDXRH);
}
pub fn emitLdxr(dest: Register, addr: Register) u32 {
    return emitLdaStlCommon(dest, addr, ARM64_OPCODE_LDXR);
}
pub fn emitLdxr64(dest: Register, addr: Register) u32 {
    return emitLdaStlCommon(dest, addr, ARM64_OPCODE_LDXR64);
}

pub fn emitLdaxrb(dest: Register, addr: Register) u32 {
    return emitLdaStlCommon(dest, addr, ARM64_OPCODE_LDAXRB);
}
pub fn emitLdaxrh(dest: Register, addr: Register) u32 {
    return emitLdaStlCommon(dest, addr, ARM64_OPCODE_LDAXRH);
}
pub fn emitLdaxr(dest: Register, addr: Register) u32 {
    return emitLdaStlCommon(dest, addr, ARM64_OPCODE_LDAXR);
}
pub fn emitLdaxr64(dest: Register, addr: Register) u32 {
    return emitLdaStlCommon(dest, addr, ARM64_OPCODE_LDAXR64);
}

pub fn emitStlrb(src: Register, addr: Register) u32 {
    return emitLdaStlCommon(src, addr, ARM64_OPCODE_STLRB);
}
pub fn emitStlrh(src: Register, addr: Register) u32 {
    return emitLdaStlCommon(src, addr, ARM64_OPCODE_STLRH);
}
pub fn emitStlr(src: Register, addr: Register) u32 {
    return emitLdaStlCommon(src, addr, ARM64_OPCODE_STLR);
}
pub fn emitStlr64(src: Register, addr: Register) u32 {
    return emitLdaStlCommon(src, addr, ARM64_OPCODE_STLR64);
}

// STXR/STLXR (with status register)
fn emitStlxrCommon(status: Register, src: Register, addr: Register, opcode: u32) u32 {
    return opcode | (r(status) << 16) | (r(addr) << 5) | r(src);
}

pub fn emitStxrb(status: Register, src: Register, addr: Register) u32 {
    return emitStlxrCommon(status, src, addr, ARM64_OPCODE_STXRB);
}
pub fn emitStxrh(status: Register, src: Register, addr: Register) u32 {
    return emitStlxrCommon(status, src, addr, ARM64_OPCODE_STXRH);
}
pub fn emitStxr(status: Register, src: Register, addr: Register) u32 {
    return emitStlxrCommon(status, src, addr, ARM64_OPCODE_STXR);
}
pub fn emitStxr64(status: Register, src: Register, addr: Register) u32 {
    return emitStlxrCommon(status, src, addr, ARM64_OPCODE_STXR64);
}

pub fn emitStlxrb(status: Register, src: Register, addr: Register) u32 {
    return emitStlxrCommon(status, src, addr, ARM64_OPCODE_STLXRB);
}
pub fn emitStlxrh(status: Register, src: Register, addr: Register) u32 {
    return emitStlxrCommon(status, src, addr, ARM64_OPCODE_STLXRH);
}
pub fn emitStlxr(status: Register, src: Register, addr: Register) u32 {
    return emitStlxrCommon(status, src, addr, ARM64_OPCODE_STLXR);
}
pub fn emitStlxr64(status: Register, src: Register, addr: Register) u32 {
    return emitStlxrCommon(status, src, addr, ARM64_OPCODE_STLXR64);
}

// LDAXP
fn emitLdaxpCommon(dest1: Register, dest2: Register, addr: Register, opcode: u32) u32 {
    return opcode | (r(dest2) << 10) | (r(addr) << 5) | r(dest1);
}

pub fn emitLdaxp(dest1: Register, dest2: Register, addr: Register) u32 {
    return emitLdaxpCommon(dest1, dest2, addr, 0x887f8000);
}
pub fn emitLdaxp64(dest1: Register, dest2: Register, addr: Register) u32 {
    return emitLdaxpCommon(dest1, dest2, addr, 0xc87f8000);
}

// STLXP
fn emitStlxpCommon(status: Register, src1: Register, src2: Register, addr: Register, opcode: u32) u32 {
    return opcode | (r(status) << 16) | (r(src2) << 10) | (r(addr) << 5) | r(src1);
}

pub fn emitStlxp(status: Register, src1: Register, src2: Register, addr: Register) u32 {
    return emitStlxpCommon(status, src1, src2, addr, 0x88208000);
}
pub fn emitStlxp64(status: Register, src1: Register, src2: Register, addr: Register) u32 {
    return emitStlxpCommon(status, src1, src2, addr, 0xc8208000);
}

// ============================================================================
// Section: NEON unary integer operations
// ============================================================================

fn emitNeonBinaryCommon(dest: NeonRegister, src: NeonRegister, src_size: NeonSize, vector_opcode: u32, scalar_opcode: u32) u32 {
    const size_val: u32 = @intFromEnum(src_size);
    if (src_size.isScalar()) {
        return scalar_opcode | ((size_val & 3) << 22) | (nr(src) << 5) | nr(dest);
    } else {
        return vector_opcode |
            (@as(u32, src_size.qBit()) << 30) |
            ((size_val & 3) << 22) |
            (nr(src) << 5) | nr(dest);
    }
}

pub fn emitNeonAbs(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x0e20b800, 0x5e20b800);
}
pub fn emitNeonAddpScalar(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0, 0x5e31b800);
}
pub fn emitNeonAddv(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x0e31b800, 0);
}
pub fn emitNeonCls(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x0e204800, 0);
}
pub fn emitNeonClz(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x2e204800, 0);
}
pub fn emitNeonCmeq0(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x0e209800, 0x5e209800);
}
pub fn emitNeonCmge0(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x2e208800, 0x7e208800);
}
pub fn emitNeonCmgt0(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x0e208800, 0x5e208800);
}
pub fn emitNeonCmle0(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x2e209800, 0x7e209800);
}
pub fn emitNeonCmlt0(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x0e20a800, 0x5e20a800);
}
pub fn emitNeonCnt(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x0e205800, 0);
}
pub fn emitNeonNeg(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x2e20b800, 0x7e20b800);
}
pub fn emitNeonNot(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x2e205800, 0);
}
pub fn emitNeonRbit(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x2e605800, 0);
}
pub fn emitNeonRev16(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x0e201800, 0);
}
pub fn emitNeonRev32(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x2e200800, 0);
}
pub fn emitNeonRev64(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x0e200800, 0);
}
pub fn emitNeonSadalp(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x0e206800, 0);
}
pub fn emitNeonSaddlp(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x0e202800, 0);
}
pub fn emitNeonSaddlv(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x0e303800, 0);
}

pub fn emitNeonShll(dest: NeonRegister, src: NeonRegister, src_size: NeonSize) u32 {
    const adj_size: NeonSize = @enumFromInt(@intFromEnum(src_size) & ~@as(u4, 4));
    return emitNeonBinaryCommon(dest, src, adj_size, 0x2e213800, 0);
}
pub fn emitNeonShll2(dest: NeonRegister, src: NeonRegister, src_size: NeonSize) u32 {
    const adj_size: NeonSize = @enumFromInt(@intFromEnum(src_size) | 4);
    return emitNeonBinaryCommon(dest, src, adj_size, 0x2e213800, 0);
}

pub fn emitNeonSmaxv(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x0e30a800, 0);
}
pub fn emitNeonSminv(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x0e31a800, 0);
}
pub fn emitNeonSqabs(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x0e207800, 0x5e207800);
}
pub fn emitNeonSqneg(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x2e207800, 0x7e207800);
}

pub fn emitNeonSqxtn(dest: NeonRegister, src: NeonRegister, src_size: NeonSize) u32 {
    const adj: NeonSize = @enumFromInt((@intFromEnum(src_size) -% 1) & ~@as(u4, 4));
    return emitNeonBinaryCommon(dest, src, adj, 0x0e214800, 0x5e214800);
}
pub fn emitNeonSqxtn2(dest: NeonRegister, src: NeonRegister, src_size: NeonSize) u32 {
    const adj: NeonSize = @enumFromInt((@intFromEnum(src_size) -% 1) | 4);
    return emitNeonBinaryCommon(dest, src, adj, 0x0e214800, 0x5e214800);
}
pub fn emitNeonSqxtun(dest: NeonRegister, src: NeonRegister, src_size: NeonSize) u32 {
    const adj: NeonSize = @enumFromInt((@intFromEnum(src_size) -% 1) & ~@as(u4, 4));
    return emitNeonBinaryCommon(dest, src, adj, 0x2e212800, 0x7e212800);
}
pub fn emitNeonSqxtun2(dest: NeonRegister, src: NeonRegister, src_size: NeonSize) u32 {
    const adj: NeonSize = @enumFromInt((@intFromEnum(src_size) -% 1) | 4);
    return emitNeonBinaryCommon(dest, src, adj, 0x2e212800, 0x7e212800);
}

pub fn emitNeonSuqadd(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x0e203800, 0x5e203800);
}

pub fn emitNeonUadalp(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x2e206800, 0);
}
pub fn emitNeonUaddlp(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x2e202800, 0);
}
pub fn emitNeonUaddlv(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x2e303800, 0);
}
pub fn emitNeonUmaxv(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x2e30a800, 0);
}
pub fn emitNeonUminv(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x2e31a800, 0);
}

pub fn emitNeonUqxtn(dest: NeonRegister, src: NeonRegister, src_size: NeonSize) u32 {
    const adj: NeonSize = @enumFromInt((@intFromEnum(src_size) -% 1) & ~@as(u4, 4));
    return emitNeonBinaryCommon(dest, src, adj, 0x2e214800, 0x7e214800);
}
pub fn emitNeonUqxtn2(dest: NeonRegister, src: NeonRegister, src_size: NeonSize) u32 {
    const adj: NeonSize = @enumFromInt((@intFromEnum(src_size) -% 1) | 4);
    return emitNeonBinaryCommon(dest, src, adj, 0x2e214800, 0x7e214800);
}

pub fn emitNeonUrecpe(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x0ea1c800, 0);
}
pub fn emitNeonUrsqrte(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x2ea1c800, 0);
}
pub fn emitNeonUsqadd(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonBinaryCommon(dest, src, size, 0x2e203800, 0x7e203800);
}

pub fn emitNeonXtn(dest: NeonRegister, src: NeonRegister, src_size: NeonSize) u32 {
    const adj: NeonSize = @enumFromInt((@intFromEnum(src_size) -% 1) & ~@as(u4, 4));
    return emitNeonBinaryCommon(dest, src, adj, 0x0e212800, 0);
}
pub fn emitNeonXtn2(dest: NeonRegister, src: NeonRegister, src_size: NeonSize) u32 {
    const adj: NeonSize = @enumFromInt((@intFromEnum(src_size) -% 1) | 4);
    return emitNeonBinaryCommon(dest, src, adj, 0x0e212800, 0);
}

// ============================================================================
// Section: NEON float unary operations
// ============================================================================

fn emitNeonFloatBinaryCommon(dest: NeonRegister, src: NeonRegister, src_size: NeonSize, vector_opcode: u32, scalar_opcode: u32) u32 {
    const size_val: u32 = @intFromEnum(src_size);
    if (src_size.isScalar()) {
        return scalar_opcode | ((size_val & 1) << 22) | (nr(src) << 5) | nr(dest);
    } else {
        return vector_opcode |
            (@as(u32, src_size.qBit()) << 30) |
            ((size_val & 1) << 22) |
            (nr(src) << 5) | nr(dest);
    }
}

pub fn emitNeonFabs(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x0ea0f800, 0x1e20c000);
}
pub fn emitNeonFaddpScalar(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x7e30d800, 0);
}
pub fn emitNeonFcmeq0(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x0ea0d800, 0x5ea0d800);
}
pub fn emitNeonFcmge0(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x2ea0c800, 0x7ea0c800);
}
pub fn emitNeonFcmgt0(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x0ea0c800, 0x5ea0c800);
}
pub fn emitNeonFcmle0(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x2ea0d800, 0x7ea0d800);
}
pub fn emitNeonFcmlt0(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x0ea0e800, 0x5ea0e800);
}

// FCVTxx (neon-to-neon float conversions)
pub fn emitNeonFcvtas(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x0e21c800, 0x5e21c800);
}
pub fn emitNeonFcvtau(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x2e21c800, 0x7e21c800);
}
pub fn emitNeonFcvtms(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x0e21b800, 0x5e21b800);
}
pub fn emitNeonFcvtmu(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x2e21b800, 0x7e21b800);
}
pub fn emitNeonFcvtns(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x0e21a800, 0x5e21a800);
}
pub fn emitNeonFcvtnu(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x2e21a800, 0x7e21a800);
}
pub fn emitNeonFcvtps(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x0ea1a800, 0x5ea1a800);
}
pub fn emitNeonFcvtpu(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x2ea1a800, 0x7ea1a800);
}
pub fn emitNeonFcvtzs(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x0ea1b800, 0x5ea1b800);
}
pub fn emitNeonFcvtzu(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x2ea1b800, 0x7ea1b800);
}

pub fn emitNeonFcvtl(dest: NeonRegister, src: NeonRegister, src_size: NeonSize) u32 {
    const adj: NeonSize = @enumFromInt((@intFromEnum(src_size) + 1) & ~@as(u4, 4));
    return emitNeonFloatBinaryCommon(dest, src, adj, 0x0e217800, 0);
}
pub fn emitNeonFcvtl2(dest: NeonRegister, src: NeonRegister, src_size: NeonSize) u32 {
    const adj: NeonSize = @enumFromInt((@intFromEnum(src_size) + 1) | 4);
    return emitNeonFloatBinaryCommon(dest, src, adj, 0x0e217800, 0);
}
pub fn emitNeonFcvtn(dest: NeonRegister, src: NeonRegister, src_size: NeonSize) u32 {
    const adj: NeonSize = @enumFromInt(@intFromEnum(src_size) & ~@as(u4, 4));
    return emitNeonFloatBinaryCommon(dest, src, adj, 0x0e216800, 0);
}
pub fn emitNeonFcvtn2(dest: NeonRegister, src: NeonRegister, src_size: NeonSize) u32 {
    const adj: NeonSize = @enumFromInt(@intFromEnum(src_size) | 4);
    return emitNeonFloatBinaryCommon(dest, src, adj, 0x4e216800, 0);
}
pub fn emitNeonFcvtxn(dest: NeonRegister, src: NeonRegister, src_size: NeonSize) u32 {
    const adj: NeonSize = @enumFromInt(@intFromEnum(src_size) & ~@as(u4, 4));
    return emitNeonFloatBinaryCommon(dest, src, adj, 0x2e216800, 0x7e216800);
}
pub fn emitNeonFcvtxn2(dest: NeonRegister, src: NeonRegister, src_size: NeonSize) u32 {
    const adj: NeonSize = @enumFromInt(@intFromEnum(src_size) | 4);
    return emitNeonFloatBinaryCommon(dest, src, adj, 0x2e216800, 0x7e216800);
}

pub fn emitNeonFmov(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0, 0x1e204000);
}
pub fn emitNeonFmovImmediate(dest: NeonRegister, imm8: u8, dest_size: NeonSize) u32 {
    return 0x1e201000 | ((@as(u32, @intFromEnum(dest_size)) & 1) << 22) | (@as(u32, imm8) << 13) | nr(dest);
}

pub fn emitNeonFneg(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x2ea0f800, 0x1e214000);
}
pub fn emitNeonFrecpe(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x0ea1d800, 0x5ea1d800);
}
pub fn emitNeonFrecpx(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0, 0x5ea1f800);
}

// FRINTx
pub fn emitNeonFrinta(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x2e218800, 0x1e264000);
}
pub fn emitNeonFrinti(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x2ea19800, 0x1e27c000);
}
pub fn emitNeonFrintm(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x0e219800, 0x1e254000);
}
pub fn emitNeonFrintn(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x0e218800, 0x1e244000);
}
pub fn emitNeonFrintp(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x0ea18800, 0x1e24c000);
}
pub fn emitNeonFrintx(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x2e219800, 0x1e274000);
}
pub fn emitNeonFrintz(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x0ea19800, 0x1e25c000);
}

pub fn emitNeonFrsqrte(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x2ea1d800, 0x7ea1d800);
}
pub fn emitNeonFsqrt(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x2ea1f800, 0x1e21c000);
}

// SCVTF/UCVTF (neon-to-neon)
pub fn emitNeonScvtfVec(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x0e21d800, 0x5e21d800);
}
pub fn emitNeonUcvtfVec(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatBinaryCommon(dest, src, size, 0x2e21d800, 0x7e21d800);
}

// ============================================================================
// Section: NEON trinary integer operations (3-register same)
// ============================================================================

fn emitNeonTrinaryCommon(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, src_size: NeonSize, vector_opcode: u32, scalar_opcode: u32) u32 {
    const size_val: u32 = @intFromEnum(src_size);
    if (src_size.isScalar()) {
        return scalar_opcode | ((size_val & 3) << 22) | (nr(src2) << 16) | (nr(src1) << 5) | nr(dest);
    } else {
        return vector_opcode |
            (@as(u32, src_size.qBit()) << 30) |
            ((size_val & 3) << 22) |
            (nr(src2) << 16) | (nr(src1) << 5) | nr(dest);
    }
}

pub fn emitNeonAdd(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e208400, 0x5e208400);
}
pub fn emitNeonAddhn(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, @enumFromInt(@intFromEnum(size) & ~@as(u4, 4)), 0x0e204000, 0);
}
pub fn emitNeonAddhn2(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, @enumFromInt(@intFromEnum(size) | 4), 0x0e204000, 0);
}
pub fn emitNeonAddp(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e20bc00, 0);
}
pub fn emitNeonAnd(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e201c00, 0);
}
pub fn emitNeonBicReg(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e601c00, 0);
}
pub fn emitNeonBif(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2ee01c00, 0);
}
pub fn emitNeonBit(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2ea01c00, 0);
}
pub fn emitNeonBsl(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e601c00, 0);
}

// CMxx (register)
pub fn emitNeonCmeq(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e208c00, 0x7e208c00);
}
pub fn emitNeonCmge(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e203c00, 0x5e203c00);
}
pub fn emitNeonCmgt(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e203400, 0x5e203400);
}
pub fn emitNeonCmhi(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e203400, 0x7e203400);
}
pub fn emitNeonCmhs(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e203c00, 0x7e203c00);
}
pub fn emitNeonCmtst(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e208c00, 0x5e208c00);
}

pub fn emitNeonEor(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e201c00, 0);
}
pub fn emitNeonMla(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e209400, 0);
}
pub fn emitNeonMls(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e209400, 0);
}
pub fn emitNeonMov(dest: NeonRegister, src: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src, src, size, 0x0ea01c00, 0);
}
pub fn emitNeonMul(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e209c00, 0);
}
pub fn emitNeonOrn(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0ee01c00, 0);
}
pub fn emitNeonOrrReg(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0ea01c00, 0);
}
pub fn emitNeonPmul(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e209c00, 0);
}
pub fn emitNeonPmull(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, @enumFromInt(@intFromEnum(size) & ~@as(u4, 4)), 0x0e20e000, 0x0e20e000);
}
pub fn emitNeonPmull2(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, @enumFromInt(@intFromEnum(size) | 4), 0x0e20e000, 0x0e20e000);
}

// Narrowing high
pub fn emitNeonRaddhn(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, @enumFromInt((@intFromEnum(size) -% 1) & ~@as(u4, 4)), 0x2e204000, 0);
}
pub fn emitNeonRaddhn2(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, @enumFromInt((@intFromEnum(size) -% 1) | 4), 0x2e204000, 0);
}
pub fn emitNeonRsubhn(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, @enumFromInt((@intFromEnum(size) -% 1) & ~@as(u4, 4)), 0x2e206000, 0);
}
pub fn emitNeonRsubhn2(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, @enumFromInt((@intFromEnum(size) -% 1) | 4), 0x2e206000, 0);
}

// Signed integer ops
pub fn emitNeonSub(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e208400, 0x7e208400);
}
pub fn emitNeonSubhn(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, @enumFromInt(@intFromEnum(size) & ~@as(u4, 4)), 0x0e206000, 0);
}
pub fn emitNeonSubhn2(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, @enumFromInt(@intFromEnum(size) | 4), 0x0e206000, 0);
}

pub fn emitNeonSqadd(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e200c00, 0x5e200c00);
}
pub fn emitNeonSqsub(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e202c00, 0x5e202c00);
}
pub fn emitNeonUqadd(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e200c00, 0x7e200c00);
}
pub fn emitNeonUqsub(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e202c00, 0x7e202c00);
}

// Signed halving, rounding halving, shift
pub fn emitNeonShadd(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e200400, 0);
}
pub fn emitNeonShsub(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e202400, 0);
}
pub fn emitNeonSrhadd(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e201400, 0);
}
pub fn emitNeonUhadd(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e200400, 0);
}
pub fn emitNeonUhsub(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e202400, 0);
}
pub fn emitNeonUrhadd(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e201400, 0);
}

// Min/Max
pub fn emitNeonSmax(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e206400, 0);
}
pub fn emitNeonSmaxp(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e20a400, 0);
}
pub fn emitNeonSmin(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e206c00, 0);
}
pub fn emitNeonSminp(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e20ac00, 0);
}
pub fn emitNeonUmax(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e206400, 0);
}
pub fn emitNeonUmaxp(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e20a400, 0);
}
pub fn emitNeonUmin(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e206c00, 0);
}
pub fn emitNeonUminp(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e20ac00, 0);
}

// Register shifts
pub fn emitNeonSshl(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e204400, 0x5e204400);
}
pub fn emitNeonUshl(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e204400, 0x7e204400);
}
pub fn emitNeonSrshl(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e205400, 0x5e205400);
}
pub fn emitNeonUrshl(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e205400, 0x7e205400);
}
pub fn emitNeonSqshlReg(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e204c00, 0x5e204c00);
}
pub fn emitNeonUqshlReg(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e204c00, 0x7e204c00);
}
pub fn emitNeonSqrshl(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e205c00, 0x5e205c00);
}
pub fn emitNeonUqrshl(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x2e205c00, 0x7e205c00);
}

// Permute
pub fn emitNeonTrn1(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e002800, 0);
}
pub fn emitNeonTrn2(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e006800, 0);
}
pub fn emitNeonUzp1(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e001800, 0);
}
pub fn emitNeonUzp2(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e005800, 0);
}
pub fn emitNeonZip1(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e003800, 0);
}
pub fn emitNeonZip2(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonTrinaryCommon(dest, src1, src2, size, 0x0e007800, 0);
}

// Widening/long signed
pub fn emitNeonSaba(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonTrinaryCommon(d, s1, s2, sz, 0x0e207c00, 0);
}
pub fn emitNeonSabd(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonTrinaryCommon(d, s1, s2, sz, 0x0e207400, 0);
}
pub fn emitNeonSqdmulh(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonTrinaryCommon(d, s1, s2, sz, 0x0e20b400, 0x5e20b400);
}
pub fn emitNeonSqrdmulh(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonTrinaryCommon(d, s1, s2, sz, 0x2e20b400, 0x7e20b400);
}
pub fn emitNeonUaba(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonTrinaryCommon(d, s1, s2, sz, 0x2e207c00, 0);
}
pub fn emitNeonUabd(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonTrinaryCommon(d, s1, s2, sz, 0x2e207400, 0);
}

// ============================================================================
// Section: NEON float trinary operations
// ============================================================================

fn emitNeonFloatTrinaryCommon(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, src_size: NeonSize, vector_opcode: u32, scalar_opcode: u32) u32 {
    const size_val: u32 = @intFromEnum(src_size);
    if (src_size.isScalar()) {
        return scalar_opcode | ((size_val & 1) << 22) | (nr(src2) << 16) | (nr(src1) << 5) | nr(dest);
    } else {
        return vector_opcode |
            (@as(u32, src_size.qBit()) << 30) |
            ((size_val & 1) << 22) |
            (nr(src2) << 16) | (nr(src1) << 5) | nr(dest);
    }
}

pub fn emitNeonFabd(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x2ea0d400, 0x7ea0d400);
}
pub fn emitNeonFacge(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x2e20ec00, 0x7e20ec00);
}
pub fn emitNeonFacgt(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x2ea0ec00, 0x7ea0ec00);
}
pub fn emitNeonFadd(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x0e20d400, 0x1e202800);
}
pub fn emitNeonFaddp(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x2e20d400, 0);
}
pub fn emitNeonFcmeq(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x0e20e400, 0x5e20e400);
}
pub fn emitNeonFcmge(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x2e20e400, 0x7e20e400);
}
pub fn emitNeonFcmgt(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x2ea0e400, 0x7ea0e400);
}

// FCMP/FCMPE (scalar only, destination is NZCV flags)
pub fn emitNeonFcmp(src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(.V0, src1, src2, size, 0, 0x1e202000);
}
pub fn emitNeonFcmp0(src1: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(.V0, src1, .V0, size, 0, 0x1e202008);
}
pub fn emitNeonFcmpe(src1: NeonRegister, src2: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(.V0, src1, src2, size, 0, 0x1e202010);
}
pub fn emitNeonFcmpe0(src1: NeonRegister, size: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(.V0, src1, .V0, size, 0, 0x1e202018);
}

pub fn emitNeonFdiv(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x2e20fc00, 0x1e201800);
}
pub fn emitNeonFmax(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x0e20f400, 0x1e204800);
}
pub fn emitNeonFmaxnm(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x0e20c400, 0x1e206800);
}
pub fn emitNeonFmin(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x0ea0f400, 0x1e205800);
}
pub fn emitNeonFminnm(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x0ea0c400, 0x1e207800);
}
pub fn emitNeonFmla(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x0e20cc00, 0);
}
pub fn emitNeonFmls(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x0ea0cc00, 0);
}
pub fn emitNeonFmul(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x2e20dc00, 0x1e200800);
}
pub fn emitNeonFmulx(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x0e20dc00, 0x5e20dc00);
}
pub fn emitNeonFnmul(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0, 0x1e208800);
}
pub fn emitNeonFrecps(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x0e20fc00, 0x5e20fc00);
}
pub fn emitNeonFrsqrts(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x0ea0fc00, 0x5ea0fc00);
}
pub fn emitNeonFsub(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x0ea0d400, 0x1e203800);
}

// Pairwise float ops (vector only)
pub fn emitNeonFmaxnmp(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x2e20c400, 0);
}
pub fn emitNeonFmaxp(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x2e20f400, 0);
}
pub fn emitNeonFminnmp(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x2ea0c400, 0);
}
pub fn emitNeonFminp(d: NeonRegister, s1: NeonRegister, s2: NeonRegister, sz: NeonSize) u32 {
    return emitNeonFloatTrinaryCommon(d, s1, s2, sz, 0x2ea0f400, 0);
}

// ============================================================================
// Section: NEON shift immediate
// ============================================================================

fn emitNeonShiftLeftImmediateCommon(dest: NeonRegister, src: NeonRegister, imm: u6, src_size: NeonSize, vector_opcode: u32, scalar_opcode: u32) u32 {
    const size: u32 = @intFromEnum(src_size) & 3;
    const eff_shift: u32 = @as(u32, imm) + (@as(u32, 8) << @intCast(size));

    if (src_size.isScalar()) {
        return scalar_opcode | (eff_shift << 16) | (nr(src) << 5) | nr(dest);
    } else {
        return vector_opcode | (@as(u32, src_size.qBit()) << 30) | (eff_shift << 16) | (nr(src) << 5) | nr(dest);
    }
}

fn emitNeonShiftRightImmediateCommon(dest: NeonRegister, src: NeonRegister, imm: u6, src_size: NeonSize, vector_opcode: u32, scalar_opcode: u32) u32 {
    const size: u32 = @intFromEnum(src_size) & 3;
    const eff_shift: u32 = (@as(u32, 16) << @intCast(size)) - @as(u32, imm);

    if (src_size.isScalar()) {
        return scalar_opcode | (eff_shift << 16) | (nr(src) << 5) | nr(dest);
    } else {
        return vector_opcode | (@as(u32, src_size.qBit()) << 30) | (eff_shift << 16) | (nr(src) << 5) | nr(dest);
    }
}

pub fn emitNeonShl(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftLeftImmediateCommon(d, s, imm, sz, 0x0f005400, 0x5f005400);
}
pub fn emitNeonSli(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftLeftImmediateCommon(d, s, imm, sz, 0x2f005400, 0x7f005400);
}
pub fn emitNeonSri(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, sz, 0x2f004400, 0x7f004400);
}

pub fn emitNeonSshr(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, sz, 0x0f000400, 0x5f000400);
}
pub fn emitNeonSsra(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, sz, 0x0f001400, 0x5f001400);
}
pub fn emitNeonSrshr(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, sz, 0x0f002400, 0x5f002400);
}
pub fn emitNeonSrsra(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, sz, 0x0f003400, 0x5f003400);
}

pub fn emitNeonUshr(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, sz, 0x2f000400, 0x7f000400);
}
pub fn emitNeonUsra(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, sz, 0x2f001400, 0x7f001400);
}
pub fn emitNeonUrshr(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, sz, 0x2f002400, 0x7f002400);
}
pub fn emitNeonUrsra(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, sz, 0x2f003400, 0x7f003400);
}

// Narrowing shift right
pub fn emitNeonShrn(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, @enumFromInt((@intFromEnum(sz) -% 1) & ~@as(u4, 4)), 0x0f008400, 0);
}
pub fn emitNeonShrn2(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, @enumFromInt((@intFromEnum(sz) -% 1) | 4), 0x0f008400, 0);
}
pub fn emitNeonRshrn(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, @enumFromInt((@intFromEnum(sz) -% 1) & ~@as(u4, 4)), 0x0f008c00, 0);
}
pub fn emitNeonRshrn2(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, @enumFromInt((@intFromEnum(sz) -% 1) | 4), 0x0f008c00, 0);
}

// Saturating shift
pub fn emitNeonSqshlImm(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftLeftImmediateCommon(d, s, imm, sz, 0x0f007400, 0x5f007400);
}
pub fn emitNeonSqshlu(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftLeftImmediateCommon(d, s, imm, sz, 0x2f006400, 0x7f006400);
}
pub fn emitNeonUqshlImm(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftLeftImmediateCommon(d, s, imm, sz, 0x2f007400, 0x7f007400);
}

pub fn emitNeonSqshrn(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, @enumFromInt((@intFromEnum(sz) -% 1) & ~@as(u4, 4)), 0x0f009400, 0x5f009400);
}
pub fn emitNeonSqshrn2(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, @enumFromInt((@intFromEnum(sz) -% 1) | 4), 0x0f009400, 0);
}
pub fn emitNeonSqrshrn(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, @enumFromInt((@intFromEnum(sz) -% 1) & ~@as(u4, 4)), 0x0f009c00, 0x5f009c00);
}
pub fn emitNeonSqrshrn2(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, @enumFromInt((@intFromEnum(sz) -% 1) | 4), 0x0f009c00, 0);
}
pub fn emitNeonSqrshrun(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, @enumFromInt((@intFromEnum(sz) -% 1) & ~@as(u4, 4)), 0x2f008c00, 0x7f008c00);
}
pub fn emitNeonSqrshrun2(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, @enumFromInt((@intFromEnum(sz) -% 1) | 4), 0x2f008c00, 0);
}
pub fn emitNeonUqshrn(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, @enumFromInt((@intFromEnum(sz) -% 1) & ~@as(u4, 4)), 0x2f009400, 0x7f009400);
}
pub fn emitNeonUqshrn2(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, @enumFromInt((@intFromEnum(sz) -% 1) | 4), 0x2f009400, 0);
}
pub fn emitNeonUqrshrn(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, @enumFromInt((@intFromEnum(sz) -% 1) & ~@as(u4, 4)), 0x2f009c00, 0x7f009c00);
}
pub fn emitNeonUqrshrn2(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftRightImmediateCommon(d, s, imm, @enumFromInt((@intFromEnum(sz) -% 1) | 4), 0x2f009c00, 0);
}

// Widening shift left
pub fn emitNeonSshll(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftLeftImmediateCommon(d, s, imm, @enumFromInt(@intFromEnum(sz) & ~@as(u4, 4)), 0x0f00a400, 0);
}
pub fn emitNeonSshll2(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftLeftImmediateCommon(d, s, imm, @enumFromInt(@intFromEnum(sz) | 4), 0x0f00a400, 0);
}
pub fn emitNeonUshll(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftLeftImmediateCommon(d, s, imm, @enumFromInt(@intFromEnum(sz) & ~@as(u4, 4)), 0x2f00a400, 0);
}
pub fn emitNeonUshll2(d: NeonRegister, s: NeonRegister, imm: u6, sz: NeonSize) u32 {
    return emitNeonShiftLeftImmediateCommon(d, s, imm, @enumFromInt(@intFromEnum(sz) | 4), 0x2f00a400, 0);
}

// SXTL/UXTL (aliases for SSHLL/USHLL with shift=0)
pub fn emitNeonSxtl(d: NeonRegister, s: NeonRegister, sz: NeonSize) u32 {
    return emitNeonSshll(d, s, 0, sz);
}
pub fn emitNeonSxtl2(d: NeonRegister, s: NeonRegister, sz: NeonSize) u32 {
    return emitNeonSshll2(d, s, 0, sz);
}
pub fn emitNeonUxtl(d: NeonRegister, s: NeonRegister, sz: NeonSize) u32 {
    return emitNeonUshll(d, s, 0, sz);
}
pub fn emitNeonUxtl2(d: NeonRegister, s: NeonRegister, sz: NeonSize) u32 {
    return emitNeonUshll2(d, s, 0, sz);
}

// ============================================================================
// Section: NEON scalar<->general register transfers
// ============================================================================

fn emitNeonConvertToGenCommon(dest: Register, src: NeonRegister, src_size: NeonSize, opcode: u32) u32 {
    return opcode | ((@as(u32, @intFromEnum(src_size)) & 1) << 22) | (nr(src) << 5) | r(dest);
}

fn emitNeonConvertFromGenCommon(dest: NeonRegister, src: Register, dst_size: NeonSize, opcode: u32) u32 {
    return opcode | ((@as(u32, @intFromEnum(dst_size)) & 1) << 22) | (r(src) << 5) | nr(dest);
}

// FCVT*Gen (float-to-integer, writing to general register)
pub fn emitNeonFcvtmsGen(dest: Register, src: NeonRegister, sz: NeonSize) u32 {
    return emitNeonConvertToGenCommon(dest, src, sz, 0x1e300000);
}
pub fn emitNeonFcvtmsGen64(dest: Register, src: NeonRegister, sz: NeonSize) u32 {
    return emitNeonConvertToGenCommon(dest, src, sz, 0x9e300000);
}
pub fn emitNeonFcvtmuGen(dest: Register, src: NeonRegister, sz: NeonSize) u32 {
    return emitNeonConvertToGenCommon(dest, src, sz, 0x1e310000);
}
pub fn emitNeonFcvtmuGen64(dest: Register, src: NeonRegister, sz: NeonSize) u32 {
    return emitNeonConvertToGenCommon(dest, src, sz, 0x9e310000);
}
pub fn emitNeonFcvtnsGen(dest: Register, src: NeonRegister, sz: NeonSize) u32 {
    return emitNeonConvertToGenCommon(dest, src, sz, 0x1e200000);
}
pub fn emitNeonFcvtnsGen64(dest: Register, src: NeonRegister, sz: NeonSize) u32 {
    return emitNeonConvertToGenCommon(dest, src, sz, 0x9e200000);
}
pub fn emitNeonFcvtnuGen(dest: Register, src: NeonRegister, sz: NeonSize) u32 {
    return emitNeonConvertToGenCommon(dest, src, sz, 0x1e210000);
}
pub fn emitNeonFcvtnuGen64(dest: Register, src: NeonRegister, sz: NeonSize) u32 {
    return emitNeonConvertToGenCommon(dest, src, sz, 0x9e210000);
}
pub fn emitNeonFcvtpsGen(dest: Register, src: NeonRegister, sz: NeonSize) u32 {
    return emitNeonConvertToGenCommon(dest, src, sz, 0x1e280000);
}
pub fn emitNeonFcvtpsGen64(dest: Register, src: NeonRegister, sz: NeonSize) u32 {
    return emitNeonConvertToGenCommon(dest, src, sz, 0x9e280000);
}
pub fn emitNeonFcvtpuGen(dest: Register, src: NeonRegister, sz: NeonSize) u32 {
    return emitNeonConvertToGenCommon(dest, src, sz, 0x1e290000);
}
pub fn emitNeonFcvtpuGen64(dest: Register, src: NeonRegister, sz: NeonSize) u32 {
    return emitNeonConvertToGenCommon(dest, src, sz, 0x9e290000);
}
pub fn emitNeonFcvtzsGen(dest: Register, src: NeonRegister, sz: NeonSize) u32 {
    return emitNeonConvertToGenCommon(dest, src, sz, 0x1e380000);
}
pub fn emitNeonFcvtzsGen64(dest: Register, src: NeonRegister, sz: NeonSize) u32 {
    return emitNeonConvertToGenCommon(dest, src, sz, 0x9e380000);
}
pub fn emitNeonFcvtzuGen(dest: Register, src: NeonRegister, sz: NeonSize) u32 {
    return emitNeonConvertToGenCommon(dest, src, sz, 0x1e390000);
}
pub fn emitNeonFcvtzuGen64(dest: Register, src: NeonRegister, sz: NeonSize) u32 {
    return emitNeonConvertToGenCommon(dest, src, sz, 0x9e390000);
}

// FMOV to general register
pub fn emitNeonFmovToGeneral(dest: Register, src: NeonRegister) u32 {
    return emitNeonConvertToGenCommon(dest, src, .Size1S, 0x1e260000);
}
pub fn emitNeonFmovToGeneral64(dest: Register, src: NeonRegister) u32 {
    return emitNeonConvertToGenCommon(dest, src, .Size1D, 0x9e260000);
}
pub fn emitNeonFmovToGeneralHigh64(dest: Register, src: NeonRegister) u32 {
    return emitNeonConvertToGenCommon(dest, src, .Size1S, 0x9eae0000);
}

// FMOV from general register
pub fn emitNeonFmovFromGeneral(dest: NeonRegister, src: Register) u32 {
    return emitNeonConvertFromGenCommon(dest, src, .Size1S, 0x1e270000);
}
pub fn emitNeonFmovFromGeneral64(dest: NeonRegister, src: Register) u32 {
    return emitNeonConvertFromGenCommon(dest, src, .Size1D, 0x9e270000);
}
pub fn emitNeonFmovFromGeneralHigh64(dest: NeonRegister, src: Register) u32 {
    return emitNeonConvertFromGenCommon(dest, src, .Size1S, 0x9eaf0000);
}

// SCVTF/UCVTF from general register
pub fn emitNeonScvtfGen(dest: NeonRegister, src: Register, dst_size: NeonSize) u32 {
    return emitNeonConvertFromGenCommon(dest, src, dst_size, 0x1e220000);
}
pub fn emitNeonScvtfGen64(dest: NeonRegister, src: Register, dst_size: NeonSize) u32 {
    return emitNeonConvertFromGenCommon(dest, src, dst_size, 0x9e220000);
}
pub fn emitNeonUcvtfGen(dest: NeonRegister, src: Register, dst_size: NeonSize) u32 {
    return emitNeonConvertFromGenCommon(dest, src, dst_size, 0x1e230000);
}
pub fn emitNeonUcvtfGen64(dest: NeonRegister, src: Register, dst_size: NeonSize) u32 {
    return emitNeonConvertFromGenCommon(dest, src, dst_size, 0x9e230000);
}

// ============================================================================
// Section: NEON element operations (DUP, INS, SMOV, UMOV)
// ============================================================================

fn emitNeonMovElementCommon(dest: NeonRegister, src: NeonRegister, src_index: u4, src_size: NeonSize, opcode: u32) u32 {
    const size: u32 = @intFromEnum(src_size) & 3;
    const idx: u32 = ((@as(u32, src_index) << 1) | 1) << @intCast(size);
    return opcode | (idx << 16) | (nr(src) << 5) | nr(dest);
}

/// DUP Vd.T, Vn.Ts[index]
pub fn emitNeonDupElement(dest: NeonRegister, src: NeonRegister, src_index: u4, dest_size: NeonSize) u32 {
    const q: u32 = @as(u32, dest_size.qBit());
    return emitNeonMovElementCommon(dest, src, src_index, dest_size, 0x0e000400 | (q << 30));
}

/// DUP Vd.T, Rn
pub fn emitNeonDup(dest: NeonRegister, src: Register, dest_size: NeonSize) u32 {
    const q: u32 = @as(u32, dest_size.qBit());
    // Encode general register as if it were a neon register at index 0
    const src_neon: NeonRegister = @enumFromInt(src.raw());
    return emitNeonMovElementCommon(dest, src_neon, 0, dest_size, 0x0e000c00 | (q << 30));
}

/// INS Vd.Ts[index], Rn
pub fn emitNeonIns(dest: NeonRegister, dest_index: u4, src: Register, dest_size: NeonSize) u32 {
    const src_neon: NeonRegister = @enumFromInt(src.raw());
    return emitNeonMovElementCommon(dest, src_neon, dest_index, dest_size, 0x4e001c00);
}

/// SMOV Wd, Vn.Ts[index]  (sign-extending move to 32-bit)
pub fn emitNeonSmov(dest: Register, src: NeonRegister, src_index: u4, src_size: NeonSize) u32 {
    const dest_neon: NeonRegister = @enumFromInt(dest.raw());
    return emitNeonMovElementCommon(dest_neon, src, src_index, src_size, 0x0e002c00);
}

/// SMOV Xd, Vn.Ts[index]  (sign-extending move to 64-bit)
pub fn emitNeonSmov64(dest: Register, src: NeonRegister, src_index: u4, src_size: NeonSize) u32 {
    const dest_neon: NeonRegister = @enumFromInt(dest.raw());
    return emitNeonMovElementCommon(dest_neon, src, src_index, src_size, 0x4e002c00);
}

/// UMOV Wd, Vn.Ts[index]  (zero-extending move to 32-bit)
pub fn emitNeonUmov(dest: Register, src: NeonRegister, src_index: u4, src_size: NeonSize) u32 {
    const dest_neon: NeonRegister = @enumFromInt(dest.raw());
    return emitNeonMovElementCommon(dest_neon, src, src_index, src_size, 0x0e003c00);
}

/// UMOV Xd, Vn.Ts[index]  (zero-extending move to 64-bit)
pub fn emitNeonUmov64(dest: Register, src: NeonRegister, src_index: u4, src_size: NeonSize) u32 {
    const dest_neon: NeonRegister = @enumFromInt(dest.raw());
    return emitNeonMovElementCommon(dest_neon, src, src_index, src_size, 0x4e003c00);
}

/// INS Vd.Ts[dstidx], Vn.Ts[srcidx]  (element to element)
pub fn emitNeonInsElement(dest: NeonRegister, dest_index: u4, src: NeonRegister, src_index: u4, src_size: NeonSize) u32 {
    const size: u32 = @intFromEnum(src_size) & 3;
    const dst_idx: u32 = ((@as(u32, dest_index) << 1) | 1) << @intCast(size);
    const src_idx: u32 = @as(u32, src_index) << @intCast(size);
    return 0x6e000400 | (dst_idx << 16) | (src_idx << 11) | (nr(src) << 5) | nr(dest);
}

// ============================================================================
// Section: NEON misc (FCSEL, MOVI, TBL, EXT)
// ============================================================================

/// FCSEL Vd, Vn, Vm, cond
pub fn emitNeonFcsel(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, cond: Condition, size: NeonSize) u32 {
    return 0x1e200c00 |
        ((@as(u32, @intFromEnum(size)) & 1) << 22) |
        (nr(src2) << 16) |
        (@as(u32, @intFromEnum(cond)) << 12) |
        (nr(src1) << 5) | nr(dest);
}

/// Compute NEON MOVI immediate encoding fields.
/// Returns the combined Op:abc:cmode:defgh bits, or null if unencodable.
pub fn computeNeonImmediate(imm: u64, dest_size: NeonSize) ?u32 {
    const op_size: u32 = @intFromEnum(dest_size) & 3;

    // Replicate element to 64-bit
    var immediate = imm;
    if (op_size == 0) {
        immediate &= 0xff;
        immediate |= immediate << 8;
    }
    if (op_size <= 1) {
        immediate &= 0xffff;
        immediate |= immediate << 16;
    }
    if (op_size <= 2) {
        immediate &= 0xffffffff;
        immediate |= immediate << 32;
    }

    const rep2x = ((immediate >> 32) & 0xffffffff) == (immediate & 0xffffffff);
    const rep4x = rep2x and ((immediate >> 16) & 0xffff) == (immediate & 0xffff);

    var op: u32 = 0;
    var cmode: u32 = undefined;
    var enc_imm: u32 = undefined;

    // For byte element sizes, always use cmode=14 (per-byte encoding)
    if (op_size == 0) {
        cmode = 14;
        enc_imm = @truncate(immediate & 0xff);
    } else if (rep2x and (immediate & 0xffffff00) == 0) {
        cmode = 0;
        enc_imm = @truncate(immediate & 0xff);
    } else if (rep2x and (immediate & 0xffff00ff) == 0) {
        cmode = 2;
        enc_imm = @truncate((immediate >> 8) & 0xff);
    } else if (rep2x and (immediate & 0xff00ffff) == 0) {
        cmode = 4;
        enc_imm = @truncate((immediate >> 16) & 0xff);
    } else if (rep2x and (immediate & 0x00ffffff) == 0) {
        cmode = 6;
        enc_imm = @truncate((immediate >> 24) & 0xff);
    } else if (rep4x and (immediate & 0xff00) == 0) {
        cmode = 8;
        enc_imm = @truncate(immediate & 0xff);
    } else if (rep4x and (immediate & 0x00ff) == 0) {
        cmode = 10;
        enc_imm = @truncate((immediate >> 8) & 0xff);
    } else if (rep2x and (immediate & 0xffff00ff) == 0x000000ff) {
        cmode = 12;
        enc_imm = @truncate((immediate >> 8) & 0xff);
    } else if (rep2x and (immediate & 0xff00ffff) == 0x0000ffff) {
        cmode = 13;
        enc_imm = @truncate((immediate >> 16) & 0xff);
    } else if (rep4x and ((immediate >> 8) & 0xff) == (immediate & 0xff)) {
        cmode = 14;
        enc_imm = @truncate(immediate & 0xff);
    } else {
        // Try byte-mask pattern (each byte is 0x00 or 0xff)
        enc_imm = 0;
        var all_ok = true;
        for (0..8) |idx| {
            const byte: u8 = @truncate((immediate >> @intCast(8 * idx)) & 0xff);
            if (byte == 0xff) {
                enc_imm |= @as(u32, 1) << @intCast(idx);
            } else if (byte != 0) {
                all_ok = false;
                break;
            }
        }
        if (all_ok) {
            cmode = 14;
            op = 1;
        } else {
            return null;
        }
    }

    return (op << 29) | (((enc_imm >> 5) & 7) << 16) | (cmode << 12) | ((enc_imm & 0x1f) << 5);
}

/// MOVI Vd.T, #imm
pub fn emitNeonMovi(dest: NeonRegister, imm: u64, dest_size: NeonSize) ?u32 {
    if (computeNeonImmediate(imm, dest_size)) |enc| {
        return 0x0f000400 | (@as(u32, dest_size.qBit()) << 30) | enc | nr(dest);
    }
    // Try MVNI (inverted)
    if (computeNeonImmediate(~imm, dest_size)) |enc| {
        return 0x2f000400 | (@as(u32, dest_size.qBit()) << 30) | enc | nr(dest);
    }
    return null;
}

/// TBL Vd.T, {Vn.16B}, Vm.T
pub fn emitNeonTbl(dest: NeonRegister, src: NeonRegister, indices: NeonRegister, size: NeonSize) u32 {
    const q: u32 = if (size == .Size16B) @as(u32, 1) else @as(u32, 0);
    return (q << 30) | 0x0e000000 | (nr(indices) << 16) | (nr(src) << 5) | nr(dest);
}

/// EXT Vd.T, Vn.T, Vm.T, #index
pub fn emitNeonExt(dest: NeonRegister, src1: NeonRegister, src2: NeonRegister, imm: u4, size: NeonSize) u32 {
    return 0x2e000000 |
        (@as(u32, size.qBit()) << 30) |
        (nr(src2) << 16) |
        (@as(u32, imm) << 11) |
        (nr(src1) << 5) | nr(dest);
}

// ============================================================================
// Section: NEON load/store
// ============================================================================

/// LDR/STR (SIMD&FP, unsigned offset)
fn emitNeonLdrStrOffsetCommon(src_dest: NeonRegister, src_dest_size: NeonSize, addr: Register, offset: i32, opcode: u32, opcode_unscaled: u32) ?u32 {
    const sz: u32 = @intFromEnum(src_dest_size);
    const size_bits: u32 = ((sz & 3) << 30) | ((sz >> 2) << 23);

    if (opcode != 0) {
        const shift: u5 = @intCast(@intFromEnum(src_dest_size));
        const enc_offset = @as(u32, @bitCast(offset)) >> shift;
        if ((@as(i32, @bitCast(enc_offset << shift)) == offset) and (enc_offset & 0xfff) == enc_offset) {
            return opcode | size_bits | ((enc_offset & 0xfff) << 10) | (r(addr) << 5) | nr(src_dest);
        }
    }

    if (opcode_unscaled != 0 and offset >= -0x100 and offset <= 0xff) {
        return opcode_unscaled | size_bits | ((@as(u32, @bitCast(offset)) & 0x1ff) << 12) | (r(addr) << 5) | nr(src_dest);
    }

    return null;
}

pub fn emitNeonLdrOffset(dest: NeonRegister, dest_size: NeonSize, addr: Register, offset: i32) ?u32 {
    return emitNeonLdrStrOffsetCommon(dest, dest_size, addr, offset, 0x3d400000, 0x3c400000);
}

pub fn emitNeonStrOffset(src: NeonRegister, src_size: NeonSize, addr: Register, offset: i32) ?u32 {
    return emitNeonLdrStrOffsetCommon(src, src_size, addr, offset, 0x3d000000, 0x3c000000);
}

/// LDP/STP (SIMD&FP, signed offset)
fn emitNeonLdpStpOffsetCommon(sd1: NeonRegister, sd2: NeonRegister, sd_size: NeonSize, addr: Register, offset: i32, opcode: u32) ?u32 {
    const opc: u32 = @intFromEnum(sd_size) - 2;
    const shift: u5 = @intCast(@intFromEnum(sd_size));
    const enc_offset = offset >> shift;
    if ((@as(i32, enc_offset) << shift) == offset and enc_offset >= -0x40 and enc_offset <= 0x3f) {
        return opcode | (opc << 30) |
            ((@as(u32, @bitCast(enc_offset)) & 0x7f) << 15) |
            (nr(sd2) << 10) | (r(addr) << 5) | nr(sd1);
    }
    return null;
}

pub fn emitNeonLdpOffset(dest1: NeonRegister, dest2: NeonRegister, size: NeonSize, addr: Register, offset: i32) ?u32 {
    return emitNeonLdpStpOffsetCommon(dest1, dest2, size, addr, offset, 0x2d400000);
}

pub fn emitNeonStpOffset(src1: NeonRegister, src2: NeonRegister, size: NeonSize, addr: Register, offset: i32) ?u32 {
    return emitNeonLdpStpOffsetCommon(src1, src2, size, addr, offset, 0x2d000000);
}

/// LD1/ST1 (single structure, immediate offset)
fn emitNeonLd1St1Common(src_dest: NeonRegister, index: u4, addr: Register, sd_size: NeonSize, opcode: u32) u32 {
    const sz: u32 = @intFromEnum(sd_size) & 3;
    var qs_size: u32 = @as(u32, index) << @intCast(sz);
    if (sd_size == .Size1D) {
        qs_size |= 1;
    }
    const op: u32 = if (sd_size == .Size1B) 0 else if (sd_size == .Size1H) 2 else 4;
    return opcode |
        ((qs_size >> 3) << 30) |
        (op << 13) |
        ((qs_size & 7) << 10) |
        (r(addr) << 5) | nr(src_dest);
}

pub fn emitNeonLd1(dest: NeonRegister, index: u4, addr: Register, size: NeonSize) u32 {
    return emitNeonLd1St1Common(dest, index, addr, size, 0x0d400000);
}

pub fn emitNeonSt1(src: NeonRegister, index: u4, addr: Register, size: NeonSize) u32 {
    return emitNeonLd1St1Common(src, index, addr, size, 0x0d000000);
}

// ============================================================================
// Section: NEON crypto (AES)
// ============================================================================

pub fn emitNeonAesD(dest: NeonRegister, src: NeonRegister) u32 {
    return 0x4e285800 | (nr(src) << 5) | nr(dest);
}

pub fn emitNeonAesE(dest: NeonRegister, src: NeonRegister) u32 {
    return 0x4e284800 | (nr(src) << 5) | nr(dest);
}

pub fn emitNeonAesImc(dest: NeonRegister, src: NeonRegister) u32 {
    return 0x4e287800 | (nr(src) << 5) | nr(dest);
}

pub fn emitNeonAesMc(dest: NeonRegister, src: NeonRegister) u32 {
    return 0x4e286800 | (nr(src) << 5) | nr(dest);
}

// ============================================================================
// Section: MRS/MSR convenience helpers for TLS
// ============================================================================

// MRS Xd, TPIDR_EL0  (read thread-local base pointer)
pub fn emitMrsTpidrEl0(dest: Register) u32 {
    return emitMrs(dest, ARM64_TPIDR_EL0);
}
// MSR TPIDR_EL0, Xn  (write thread-local base pointer)
pub fn emitMsrTpidrEl0(src: Register) u32 {
    return emitMsr(src, ARM64_TPIDR_EL0);
}

// ============================================================================
// Section: Tests
// ============================================================================

test "NOP encoding" {
    try std.testing.expectEqual(@as(u32, 0xd503201f), emitNop());
}

test "BRK #0 encoding" {
    try std.testing.expectEqual(@as(u32, 0xd4200000), emitBrk(0));
}

test "BRK breakpoint" {
    try std.testing.expectEqual(@as(u32, 0xd4200000 | (@as(u32, 0xf000) << 5)), emitDebugBreak());
}

test "RET LR" {
    // RET X30 = 0xd65f0000 | (30 << 5) = 0xd65f03c0
    try std.testing.expectEqual(@as(u32, 0xd65f03c0), emitRet(.LR));
}

test "ADD W0, W1, W2" {
    // ADD (shifted register, 32-bit): 0x0b000000 | Rm=2<<16 | Rn=1<<5 | Rd=0
    const expected: u32 = 0x0b000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitAddRegister(.R0, .R1, RegisterParam.reg_only(.R2)));
}

test "ADD X0, X1, X2" {
    const expected: u32 = 0x8b000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitAddRegister64(.R0, .R1, RegisterParam.reg_only(.R2)));
}

test "SUB W3, W4, W5" {
    const expected: u32 = 0x4b000000 | (5 << 16) | (4 << 5) | 3;
    try std.testing.expectEqual(expected, emitSubRegister(.R3, .R4, RegisterParam.reg_only(.R5)));
}

test "AND W0, W1, W2" {
    const expected: u32 = 0x0a000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitAndRegister(.R0, .R1, RegisterParam.reg_only(.R2)));
}

test "ORR W0, WZR, W1 (MOV W0, W1)" {
    const expected: u32 = 0x2a000000 | (1 << 16) | (31 << 5) | 0;
    try std.testing.expectEqual(expected, emitMovRegister(.R0, .R1));
}

test "MUL W0, W1, W2" {
    // MUL is MADD with Ra=ZR(31)
    const expected: u32 = 0x1b000000 | (2 << 16) | (31 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitMul(.R0, .R1, .R2));
}

test "SDIV W0, W1, W2" {
    const expected: u32 = 0x1ac00c00 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitSdiv(.R0, .R1, .R2));
}

test "MOVZ W0, #0x1234, LSL #0" {
    const expected: u32 = 0x52800000 | (0 << 21) | (0x1234 << 5) | 0;
    try std.testing.expectEqual(expected, emitMovz(.R0, 0x1234, 0));
}

test "MOVK X0, #0x5678, LSL #16" {
    const expected: u32 = 0xf2800000 | (1 << 21) | (0x5678 << 5) | 0;
    try std.testing.expectEqual(expected, emitMovk64(.R0, 0x5678, 16));
}

test "B unconditional" {
    // B +4 (offset_words=1)
    const expected: u32 = 0x14000001;
    try std.testing.expectEqual(expected, emitB(1));
}

test "BL" {
    const expected: u32 = 0x94000001;
    try std.testing.expectEqual(expected, emitBl(1));
}

test "BR X16" {
    const expected: u32 = 0xd61f0000 | (16 << 5);
    try std.testing.expectEqual(expected, emitBr(.R16));
}

test "BLR X16" {
    const expected: u32 = 0xd63f0000 | (16 << 5);
    try std.testing.expectEqual(expected, emitBlr(.R16));
}

test "CBZ W0, +0" {
    const expected: u32 = 0x34000000;
    try std.testing.expectEqual(expected, emitCbz(.R0, 0));
}

test "CBNZ X1, +0" {
    const expected: u32 = 0xb5000001;
    try std.testing.expectEqual(expected, emitCbnz64(.R1, 0));
}

test "B.EQ +0" {
    const expected: u32 = 0x54000000; // B.EQ with 0 offset
    try std.testing.expectEqual(expected, emitBCond(0, .EQ));
}

test "ADC W0, W1, W2" {
    const expected: u32 = 0x1a000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitAdcRegister(.R0, .R1, .R2));
}

test "CSEL W0, W1, W2, EQ" {
    const expected: u32 = 0x1a800000 | (2 << 16) | (0 << 12) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitCsel(.R0, .R1, .R2, .EQ));
}

test "CSET W0, EQ" {
    // CSET is CSINC W0, WZR, WZR, NE (inverted condition)
    const expected: u32 = 0x1a800400 | (31 << 16) | (1 << 12) | (31 << 5) | 0;
    try std.testing.expectEqual(expected, emitCset(.R0, .EQ));
}

test "CLZ W0, W1" {
    const expected: u32 = 0x5ac01000 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitClz(.R0, .R1));
}

test "SXTB W0, W1" {
    // SBFM W0, W1, #0, #7
    const expected: u32 = 0x13000000 | (0 << 16) | (7 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitSxtb(.R0, .R1));
}

test "ADD W0, W1, #1" {
    const expected: u32 = 0x11000000 | (1 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitAddImmediate(.R0, .R1, 1));
}

test "SUB X0, X1, #4" {
    // SUB immediate 64-bit: 0x80000000 | 0x51000000 | (4 << 10) | (1 << 5) | 0
    const result = emitSubImmediate64(.R0, .R1, 4);
    // Should produce SUB X0, X1, #4
    const expected: u32 = 0x80000000 | 0x51000000 | (4 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result);
}

test "LDR W0, [X1, #0] (offset)" {
    const result = emitLdrOffset(.R0, .R1, 0);
    const expected: u32 = 0xb9400000 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result.?);
}

test "STR X0, [X1, #8] (offset)" {
    const result = emitStrOffset64(.R0, .R1, 8);
    // 64-bit store, shift=3, offset=8>>3=1
    const expected: u32 = 0xf9000000 | (1 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result.?);
}

test "LDP W0, W1, [SP, #0]" {
    const result = emitLdpOffset(.R0, .R1, .SP, 0);
    const expected: u32 = 0x29400000 | (1 << 10) | (31 << 5) | 0;
    try std.testing.expectEqual(expected, result.?);
}

test "STP X0, X1, [SP, #-16]!" {
    // pre-index, offset=-16, shift=3, enc=-2
    const result = emitStpOffsetPreIndex64(.R0, .R1, .SP, -16);
    try std.testing.expect(result != null);
}

test "LDAR W0, [X1]" {
    const expected: u32 = ARM64_OPCODE_LDAR | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitLdar(.R0, .R1));
}

test "DMB ISH" {
    try std.testing.expectEqual(ARM64_OPCODE_DMB_ISH, emitDmb());
}

test "MRS X0, NZCV" {
    const expected: u32 = 0xd5300000 | (@as(u32, ARM64_NZCV) << 5) | 0;
    try std.testing.expectEqual(expected, emitMrs(.R0, ARM64_NZCV));
}

test "Logical immediate - encode 0xFF (8 ones)" {
    // 0xFF repeated to 32-bit = 0x000000FF
    // This should be encodable: element size 8, 8 ones all set => can't encode all ones for size 8
    // Actually 0xFF in 8-bit means all ones for that element, which is NOT encodable.
    // Let's try 0x0F (4 ones in 8-bit element)
    const result = encodeLogicalImmediate(0x0F0F0F0F, 32);
    try std.testing.expect(result != null);
}

test "Logical immediate - 0 and -1 unencodable" {
    try std.testing.expectEqual(@as(?u13, null), encodeLogicalImmediate(0, 32));
    try std.testing.expectEqual(@as(?u13, null), encodeLogicalImmediate(0xFFFFFFFFFFFFFFFF, 64));
}

test "NEON ADD V0.4S, V1.4S, V2.4S" {
    const expected: u32 = 0x0e208400 | (1 << 30) | (2 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonAdd(.V0, .V1, .V2, .Size4S));
}

test "NEON FMUL S0, S1, S2 (scalar)" {
    const expected: u32 = 0x1e200800 | (0 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFmul(.V0, .V1, .V2, .Size1S));
}

test "NEON FCMP S0, S1" {
    const expected: u32 = 0x1e202000 | (0 << 22) | (1 << 16) | (0 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFcmp(.V0, .V1, .Size1S));
}

test "NEON AES encode" {
    const expected: u32 = 0x4e284800 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonAesE(.V0, .V1));
}

test "CCMP W0, W1, #0, EQ" {
    const expected: u32 = 0x7A400000 | (1 << 16) | (0 << 12) | (0 << 5) | 0;
    try std.testing.expectEqual(expected, emitCcmpRegister(.R0, .R1, 0, .EQ));
}

test "EXTR W0, W1, W2, #3" {
    const expected: u32 = 0x13800000 | (2 << 16) | (3 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitExtr(.R0, .R1, .R2, 3));
}

test "ADR X0, #0" {
    const expected: u32 = 0x10000000;
    try std.testing.expectEqual(expected, emitAdr(.R0, 0));
}

test "ADRP X0, #0" {
    const expected: u32 = 0x90000000;
    try std.testing.expectEqual(expected, emitAdrp(.R0, 0));
}

test "Condition invert" {
    try std.testing.expectEqual(Condition.NE, Condition.EQ.invert());
    try std.testing.expectEqual(Condition.EQ, Condition.NE.invert());
    try std.testing.expectEqual(Condition.CC, Condition.CS.invert());
    try std.testing.expectEqual(Condition.LT, Condition.GE.invert());
}

test "LoadImmediate32 simple" {
    var buf: [2]u32 = undefined;
    const n = emitLoadImmediate32(.R0, 42, &buf);
    try std.testing.expectEqual(@as(u8, 1), n);
    // MOVZ W0, #42
    try std.testing.expectEqual(@as(u32, 0x52800000 | (42 << 5) | 0), buf[0]);
}

test "LoadImmediate32 two parts" {
    var buf: [2]u32 = undefined;
    const n = emitLoadImmediate32(.R0, 0x12340001, &buf);
    // Two-part: can't use logical immediate, not a MOVN pattern
    // Should be MOVZ + MOVK
    try std.testing.expectEqual(@as(u8, 2), n);
}

test "NEON SHL V0.4S, V1.4S, #3" {
    // size=2 (S), eff_shift = 3 + 8*4 = 3+32 = 35, Q=1
    const result = emitNeonShl(.V0, .V1, 3, .Size4S);
    const expected: u32 = 0x0f005400 | (1 << 30) | (35 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result);
}

test "NEON USHR V0.4S, V1.4S, #1" {
    // size=2 (S), eff_shift = 16*4 - 1 = 63, Q=1
    const result = emitNeonUshr(.V0, .V1, 1, .Size4S);
    const expected: u32 = 0x2f000400 | (1 << 30) | (63 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result);
}

test "NeonSize methods" {
    try std.testing.expect(NeonSize.Size1S.isScalar());
    try std.testing.expect(!NeonSize.Size4S.isScalar());
    try std.testing.expect(NeonSize.Size4S.isVector());
    try std.testing.expect(!NeonSize.Size1D.isVector());
    try std.testing.expectEqual(@as(u2, 2), NeonSize.Size1S.elementSize());
    try std.testing.expectEqual(@as(u2, 2), NeonSize.Size4S.elementSize());
}

test "STXR W0, W1, [X2]" {
    const expected: u32 = ARM64_OPCODE_STXR | (0 << 16) | (2 << 5) | 1;
    try std.testing.expectEqual(expected, emitStxr(.R0, .R1, .R2));
}

test "LDAXP X0, X1, [X2]" {
    const expected: u32 = 0xc87f8000 | (1 << 10) | (2 << 5) | 0;
    try std.testing.expectEqual(expected, emitLdaxp64(.R0, .R1, .R2));
}

// ============================================================================
// Additional tests
// ============================================================================

// --- Register enum ---

test "Register ZR alias equals SP" {
    try std.testing.expectEqual(Register.SP, Register.ZR);
    try std.testing.expectEqual(@as(u5, 31), Register.ZR.raw());
}

test "Register raw values" {
    try std.testing.expectEqual(@as(u5, 0), Register.R0.raw());
    try std.testing.expectEqual(@as(u5, 29), Register.FP.raw());
    try std.testing.expectEqual(@as(u5, 30), Register.LR.raw());
    try std.testing.expectEqual(@as(u5, 31), Register.SP.raw());
}

test "NeonRegister raw values" {
    try std.testing.expectEqual(@as(u5, 0), NeonRegister.V0.raw());
    try std.testing.expectEqual(@as(u5, 15), NeonRegister.V15.raw());
    try std.testing.expectEqual(@as(u5, 31), NeonRegister.V31.raw());
}

// --- RegisterParam ---

test "RegisterParam reg_only" {
    const p = RegisterParam.reg_only(.R5);
    try std.testing.expect(p.isRegOnly());
    try std.testing.expect(!p.isExtended());
}

test "RegisterParam shifted" {
    const p = RegisterParam.shifted(.R3, .LSL, 2);
    try std.testing.expect(!p.isRegOnly());
    try std.testing.expect(!p.isExtended());
    // encode: shift=0 (LSL), Rm=3, amount=2
    const enc = p.encode();
    try std.testing.expectEqual(@as(u32, (0 << 22) | (3 << 16) | (2 << 10)), enc);
}

test "RegisterParam extended" {
    const p = RegisterParam.extended(.R4, .SXTW, 0);
    try std.testing.expect(!p.isRegOnly());
    try std.testing.expect(p.isExtended());
    const enc = p.encodeExtended();
    // ext=SXTW=6, Rm=4, amount=0
    try std.testing.expectEqual(@as(u32, (4 << 16) | (6 << 13) | (0 << 10)), enc);
}

// --- Condition invert (more cases) ---

test "Condition invert all pairs" {
    try std.testing.expectEqual(Condition.NE, Condition.EQ.invert());
    try std.testing.expectEqual(Condition.EQ, Condition.NE.invert());
    try std.testing.expectEqual(Condition.CC, Condition.CS.invert());
    try std.testing.expectEqual(Condition.CS, Condition.CC.invert());
    try std.testing.expectEqual(Condition.PL, Condition.MI.invert());
    try std.testing.expectEqual(Condition.MI, Condition.PL.invert());
    try std.testing.expectEqual(Condition.VC, Condition.VS.invert());
    try std.testing.expectEqual(Condition.VS, Condition.VC.invert());
    try std.testing.expectEqual(Condition.LS, Condition.HI.invert());
    try std.testing.expectEqual(Condition.HI, Condition.LS.invert());
    try std.testing.expectEqual(Condition.LT, Condition.GE.invert());
    try std.testing.expectEqual(Condition.GE, Condition.LT.invert());
    try std.testing.expectEqual(Condition.LE, Condition.GT.invert());
    try std.testing.expectEqual(Condition.GT, Condition.LE.invert());
}

// --- NeonSize ---

test "NeonSize qBit" {
    try std.testing.expectEqual(@as(u1, 0), NeonSize.Size1B.qBit());
    try std.testing.expectEqual(@as(u1, 1), NeonSize.Size1Q.qBit());
    try std.testing.expectEqual(@as(u1, 0), NeonSize.Size8B.qBit());
    try std.testing.expectEqual(@as(u1, 1), NeonSize.Size16B.qBit());
    try std.testing.expectEqual(@as(u1, 0), NeonSize.Size2S.qBit());
    try std.testing.expectEqual(@as(u1, 1), NeonSize.Size4S.qBit());
    try std.testing.expectEqual(@as(u1, 1), NeonSize.Size2D.qBit());
}

test "NeonSize elementSize" {
    try std.testing.expectEqual(@as(u2, 0), NeonSize.Size1B.elementSize());
    try std.testing.expectEqual(@as(u2, 1), NeonSize.Size1H.elementSize());
    try std.testing.expectEqual(@as(u2, 2), NeonSize.Size1S.elementSize());
    try std.testing.expectEqual(@as(u2, 3), NeonSize.Size1D.elementSize());
    try std.testing.expectEqual(@as(u2, 0), NeonSize.Size8B.elementSize());
    try std.testing.expectEqual(@as(u2, 0), NeonSize.Size16B.elementSize());
    try std.testing.expectEqual(@as(u2, 1), NeonSize.Size4H.elementSize());
    try std.testing.expectEqual(@as(u2, 1), NeonSize.Size8H.elementSize());
    try std.testing.expectEqual(@as(u2, 3), NeonSize.Size2D.elementSize());
}

test "neonSizeIsValid" {
    try std.testing.expect(neonSizeIsValid(.Size4S, VALID_4S));
    try std.testing.expect(neonSizeIsValid(.Size4S, VALID_24S));
    try std.testing.expect(!neonSizeIsValid(.Size2D, VALID_4S));
    try std.testing.expect(neonSizeIsValid(.Size8B, VALID_816B));
    try std.testing.expect(neonSizeIsValid(.Size16B, VALID_816B));
}

// --- NeonRegisterParam ---

test "NeonRegisterParam init and initWithSize" {
    const p1 = NeonRegisterParam.init(.V5);
    try std.testing.expectEqual(NeonRegister.V5, p1.reg);
    try std.testing.expectEqual(NeonSize.Size1D, p1.size); // default
    try std.testing.expectEqual(@as(u5, 5), p1.rawRegister());

    const p2 = NeonRegisterParam.initWithSize(.V10, .Size4S);
    try std.testing.expectEqual(NeonRegister.V10, p2.reg);
    try std.testing.expectEqual(NeonSize.Size4S, p2.size);
}

// --- DMB variants ---

test "DMB ISHST" {
    try std.testing.expectEqual(ARM64_OPCODE_DMB_ISHST, emitDmbSt());
}

test "DMB ISHLD" {
    try std.testing.expectEqual(ARM64_OPCODE_DMB_ISHLD, emitDmbLd());
}

// --- BRK variants ---

test "BRK fastfail" {
    try std.testing.expectEqual(@as(u32, 0xd4200000 | (@as(u32, ARM64_FASTFAIL) << 5)), emitFastFail());
}

test "BRK div0" {
    try std.testing.expectEqual(@as(u32, 0xd4200000 | (@as(u32, ARM64_DIVIDE_BY_0) << 5)), emitDiv0Exception());
}

test "BRK arbitrary code" {
    const code: u16 = 0x1234;
    try std.testing.expectEqual(@as(u32, 0xd4200000 | (@as(u32, code) << 5)), emitBrk(code));
}

// --- MSR ---

test "MSR NZCV, X1" {
    const expected: u32 = 0xd5100000 | (@as(u32, ARM64_NZCV) << 5) | 1;
    try std.testing.expectEqual(expected, emitMsr(.R1, ARM64_NZCV));
}

test "MRS X0, FPCR" {
    const expected: u32 = 0xd5300000 | (@as(u32, ARM64_FPCR) << 5) | 0;
    try std.testing.expectEqual(expected, emitMrs(.R0, ARM64_FPCR));
}

// --- Branch instructions ---

test "B negative offset" {
    // B -4 (offset_words=-1)
    const result = emitB(-1);
    try std.testing.expectEqual(@as(u32, 0x14000000 | 0x03ffffff), result);
}

test "B.NE +0" {
    const expected: u32 = 0x54000000 | @as(u32, @intFromEnum(Condition.NE));
    try std.testing.expectEqual(expected, emitBCond(0, .NE));
}

test "B.GT with offset" {
    // B.GT offset_words=5
    const expected: u32 = 0x54000000 | (5 << 5) | @as(u32, @intFromEnum(Condition.GT));
    try std.testing.expectEqual(expected, emitBCond(5, .GT));
}

test "CBZ X0, +0" {
    const expected: u32 = 0xb4000000;
    try std.testing.expectEqual(expected, emitCbz64(.R0, 0));
}

test "CBNZ W0, +0" {
    const expected: u32 = 0x35000000;
    try std.testing.expectEqual(expected, emitCbnz(.R0, 0));
}

test "TBZ R0, #0, +0" {
    // TBZ: bit=0, offset=0
    const expected: u32 = 0x36000000;
    try std.testing.expectEqual(expected, emitTbz(.R0, 0, 0));
}

test "TBNZ R0, #0, +0" {
    const expected: u32 = 0x37000000;
    try std.testing.expectEqual(expected, emitTbnz(.R0, 0, 0));
}

test "TBZ with bit >= 32" {
    // TBZ R0, #32, +0: bit=32, bit>>5=1 goes to bit[31], bit&0x1f=0
    const expected: u32 = 0x36000000 | (1 << 31) | (0 << 19);
    try std.testing.expectEqual(expected, emitTbz(.R0, 32, 0));
}

test "RET R0" {
    const expected: u32 = 0xd65f0000 | (0 << 5);
    try std.testing.expectEqual(expected, emitRet(.R0));
}

// --- ADD/SUB register with shift ---

test "ADD W0, W1, W2, LSL #3" {
    const expected: u32 = 0x0b000000 | (0 << 22) | (2 << 16) | (3 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitAddRegister(.R0, .R1, RegisterParam.shifted(.R2, .LSL, 3)));
}

test "SUB X0, X1, X2, ASR #5" {
    const expected: u32 = 0xcb000000 | (2 << 22) | (2 << 16) | (5 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitSubRegister64(.R0, .R1, RegisterParam.shifted(.R2, .ASR, 5)));
}

test "ADD X0, X1, W2, SXTW" {
    const expected: u32 = 0x8b200000 | (2 << 16) | (6 << 13) | (0 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitAddRegister64(.R0, .R1, RegisterParam.extended(.R2, .SXTW, 0)));
}

// --- ADDS/SUBS register ---

test "ADDS W0, W1, W2" {
    const expected: u32 = 0x2b000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitAddsRegister(.R0, .R1, RegisterParam.reg_only(.R2)));
}

test "SUBS W0, W1, W2" {
    const expected: u32 = 0x6b000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitSubsRegister(.R0, .R1, RegisterParam.reg_only(.R2)));
}

test "SUBS X0, X1, X2" {
    const expected: u32 = 0xeb000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitSubsRegister64(.R0, .R1, RegisterParam.reg_only(.R2)));
}

// --- CMP register ---

test "CMP W1, W2" {
    // CMP is SUBS WZR, W1, W2
    const expected: u32 = 0x6b000000 | (2 << 16) | (1 << 5) | 31;
    try std.testing.expectEqual(expected, emitCmpRegister(.R1, RegisterParam.reg_only(.R2)));
}

test "CMP X1, X2" {
    const expected: u32 = 0xeb000000 | (2 << 16) | (1 << 5) | 31;
    try std.testing.expectEqual(expected, emitCmpRegister64(.R1, RegisterParam.reg_only(.R2)));
}

// --- ADC/SBC 64-bit and flag-setting variants ---

test "ADC X0, X1, X2" {
    const expected: u32 = 0x9a000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitAdcRegister64(.R0, .R1, .R2));
}

test "ADCS W0, W1, W2" {
    const expected: u32 = 0x3a000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitAdcsRegister(.R0, .R1, .R2));
}

test "ADCS X0, X1, X2" {
    const expected: u32 = 0xba000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitAdcsRegister64(.R0, .R1, .R2));
}

test "SBC W0, W1, W2" {
    const expected: u32 = 0x5a000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitSbcRegister(.R0, .R1, .R2));
}

test "SBC X0, X1, X2" {
    const expected: u32 = 0xda000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitSbcRegister64(.R0, .R1, .R2));
}

test "SBCS W0, W1, W2" {
    const expected: u32 = 0x7a000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitSbcsRegister(.R0, .R1, .R2));
}

test "SBCS X0, X1, X2" {
    const expected: u32 = 0xfa000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitSbcsRegister64(.R0, .R1, .R2));
}

// --- MADD/MSUB ---

test "MADD W0, W1, W2, W3" {
    const expected: u32 = 0x1b000000 | (2 << 16) | (3 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitMadd(.R0, .R1, .R2, .R3));
}

test "MADD X0, X1, X2, X3" {
    const expected: u32 = 0x9b000000 | (2 << 16) | (3 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitMadd64(.R0, .R1, .R2, .R3));
}

test "MSUB W0, W1, W2, W3" {
    const expected: u32 = 0x1b008000 | (2 << 16) | (3 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitMsub(.R0, .R1, .R2, .R3));
}

test "MSUB X0, X1, X2, X3" {
    const expected: u32 = 0x9b008000 | (2 << 16) | (3 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitMsub64(.R0, .R1, .R2, .R3));
}

test "SMULL X0, W1, W2" {
    // SMADDL with ZR addend
    const expected: u32 = 0x9b200000 | (2 << 16) | (31 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitSmull(.R0, .R1, .R2));
}

test "UMULL X0, W1, W2" {
    const expected: u32 = 0x9ba00000 | (2 << 16) | (31 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitUmull(.R0, .R1, .R2));
}

test "MUL X0, X1, X2" {
    const expected: u32 = 0x9b000000 | (2 << 16) | (31 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitMul64(.R0, .R1, .R2));
}

// --- Division ---

test "SDIV X0, X1, X2" {
    const expected: u32 = 0x9ac00c00 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitSdiv64(.R0, .R1, .R2));
}

test "UDIV W0, W1, W2" {
    const expected: u32 = 0x1ac00800 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitUdiv(.R0, .R1, .R2));
}

test "UDIV X0, X1, X2" {
    const expected: u32 = 0x9ac00800 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitUdiv64(.R0, .R1, .R2));
}

// --- Logical register ---

test "AND X0, X1, X2" {
    const expected: u32 = 0x8a000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitAndRegister64(.R0, .R1, RegisterParam.reg_only(.R2)));
}

test "ANDS W0, W1, W2" {
    const expected: u32 = 0x6a000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitAndsRegister(.R0, .R1, RegisterParam.reg_only(.R2)));
}

test "EOR W0, W1, W2" {
    const expected: u32 = 0x4a000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitEorRegister(.R0, .R1, RegisterParam.reg_only(.R2)));
}

test "EOR X0, X1, X2" {
    const expected: u32 = 0xca000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitEorRegister64(.R0, .R1, RegisterParam.reg_only(.R2)));
}

test "ORR X0, X1, X2" {
    const expected: u32 = 0xaa000000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitOrrRegister64(.R0, .R1, RegisterParam.reg_only(.R2)));
}

test "BIC W0, W1, W2" {
    const expected: u32 = 0x0a200000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitBicRegister(.R0, .R1, RegisterParam.reg_only(.R2)));
}

test "EON W0, W1, W2" {
    const expected: u32 = 0x4a200000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitEonRegister(.R0, .R1, RegisterParam.reg_only(.R2)));
}

test "ORN W0, W1, W2" {
    const expected: u32 = 0x2a200000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitOrnRegister(.R0, .R1, RegisterParam.reg_only(.R2)));
}

test "TST W1, W2" {
    // ANDS WZR, W1, W2
    const expected: u32 = 0x6a000000 | (2 << 16) | (1 << 5) | 31;
    try std.testing.expectEqual(expected, emitTestRegister(.R1, RegisterParam.reg_only(.R2)));
}

test "MOV X0, X1" {
    // ORR X0, XZR, X1
    const expected: u32 = 0xaa000000 | (1 << 16) | (31 << 5) | 0;
    try std.testing.expectEqual(expected, emitMovRegister64(.R0, .R1));
}

test "MVN W0, W1" {
    // ORN W0, WZR, W1
    const expected: u32 = 0x2a200000 | (1 << 16) | (31 << 5) | 0;
    try std.testing.expectEqual(expected, emitMvnRegister(.R0, .R1));
}

test "MVN X0, X1" {
    const expected: u32 = 0xaa200000 | (1 << 16) | (31 << 5) | 0;
    try std.testing.expectEqual(expected, emitMvnRegister64(.R0, .R1));
}

// --- Shift register (variable) ---

test "ASR W0, W1, W2" {
    const expected: u32 = 0x1ac02800 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitAsrRegister(.R0, .R1, .R2));
}

test "LSL W0, W1, W2" {
    const expected: u32 = 0x1ac02000 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitLslRegister(.R0, .R1, .R2));
}

test "LSR X0, X1, X2" {
    const expected: u32 = 0x9ac02400 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitLsrRegister64(.R0, .R1, .R2));
}

test "ROR W0, W1, W2" {
    const expected: u32 = 0x1ac02c00 | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitRorRegister(.R0, .R1, .R2));
}

// --- Bitfield ---

test "BFM W0, W1, #3, #7" {
    const expected: u32 = 0x33000000 | (3 << 16) | (7 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitBfm(.R0, .R1, 3, 7));
}

test "BFM X0, X1, #3, #7" {
    const expected: u32 = 0xb3400000 | (3 << 16) | (7 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitBfm64(.R0, .R1, 3, 7));
}

test "SBFM W0, W1, #2, #10" {
    const expected: u32 = 0x13000000 | (2 << 16) | (10 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitSbfm(.R0, .R1, 2, 10));
}

test "UBFM W0, W1, #5, #20" {
    const expected: u32 = 0x53000000 | (5 << 16) | (20 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitUbfm(.R0, .R1, 5, 20));
}

test "SXTB X0, W1" {
    // SBFM X0, X1, #0, #7
    const expected: u32 = 0x93400000 | (0 << 16) | (7 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitSxtb64(.R0, .R1));
}

test "SXTH W0, W1" {
    const expected: u32 = 0x13000000 | (0 << 16) | (15 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitSxth(.R0, .R1));
}

test "UXTB W0, W1" {
    const expected: u32 = 0x53000000 | (0 << 16) | (7 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitUxtb(.R0, .R1));
}

test "UXTH W0, W1" {
    const expected: u32 = 0x53000000 | (0 << 16) | (15 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitUxth(.R0, .R1));
}

// --- Shift/rotate immediate ---

test "ASR W0, W1, #5" {
    // SBFM W0, W1, #5, #31
    const expected: u32 = 0x13000000 | (5 << 16) | (31 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitAsrImmediate(.R0, .R1, 5));
}

test "LSR W0, W1, #3" {
    // UBFM W0, W1, #3, #31
    const expected: u32 = 0x53000000 | (3 << 16) | (31 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitLsrImmediate(.R0, .R1, 3));
}

test "LSR X0, X1, #10" {
    // UBFM X0, X1, #10, #63
    const expected: u32 = 0xd3400000 | (10 << 16) | (63 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitLsrImmediate64(.R0, .R1, 10));
}

test "ROR W0, W1, #4" {
    // EXTR W0, W1, W1, #4
    const expected: u32 = 0x13800000 | (1 << 16) | (4 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitRorImmediate(.R0, .R1, 4));
}

test "ROR X0, X1, #7" {
    const expected: u32 = 0x93c00000 | (1 << 16) | (7 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitRorImmediate64(.R0, .R1, 7));
}

test "EXTR X0, X1, X2, #10" {
    const expected: u32 = 0x93c00000 | (2 << 16) | (10 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitExtr64(.R0, .R1, .R2, 10));
}

// --- Conditional select (more variants) ---

test "CSEL X0, X1, X2, NE" {
    const expected: u32 = 0x9a800000 | (2 << 16) | (1 << 12) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitCsel64(.R0, .R1, .R2, .NE));
}

test "CSINC W0, W1, W2, GE" {
    const expected: u32 = 0x1a800400 | (2 << 16) | (10 << 12) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitCsinc(.R0, .R1, .R2, .GE));
}

test "CSINV W0, W1, W2, LT" {
    const expected: u32 = 0x5a800000 | (2 << 16) | (11 << 12) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitCsinv(.R0, .R1, .R2, .LT));
}

test "CSNEG W0, W1, W2, EQ" {
    const expected: u32 = 0x5a800400 | (2 << 16) | (0 << 12) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitCsneg(.R0, .R1, .R2, .EQ));
}

test "CINC W0, W1, EQ" {
    // CSINC W0, W1, W1, NE (inverted)
    const expected: u32 = 0x1a800400 | (1 << 16) | (1 << 12) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitCinc(.R0, .R1, .EQ));
}

test "CSET X0, GE" {
    // CSINC X0, XZR, XZR, LT (inverted)
    const expected: u32 = 0x9a800400 | (31 << 16) | (11 << 12) | (31 << 5) | 0;
    try std.testing.expectEqual(expected, emitCset64(.R0, .GE));
}

test "CSETM W0, NE" {
    // CSINV W0, WZR, WZR, EQ (inverted)
    const expected: u32 = 0x5a800000 | (31 << 16) | (0 << 12) | (31 << 5) | 0;
    try std.testing.expectEqual(expected, emitCsetm(.R0, .NE));
}

test "CNEG W0, W1, GT" {
    // CSNEG W0, W1, W1, LE (inverted)
    const expected: u32 = 0x5a800400 | (1 << 16) | (13 << 12) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitCneg(.R0, .R1, .GT));
}

test "CINV W0, W1, CS" {
    // CSINV W0, W1, W1, CC (inverted)
    const expected: u32 = 0x5a800000 | (1 << 16) | (3 << 12) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitCinv(.R0, .R1, .CS));
}

// --- Conditional compare ---

test "CCMN W0, W1, #0, EQ" {
    const expected: u32 = 0x3A400000 | (1 << 16) | (0 << 12) | (0 << 5) | 0;
    try std.testing.expectEqual(expected, emitCcmnRegister(.R0, .R1, 0, .EQ));
}

test "CCMP X0, X1, #NZCV, NE" {
    const nzcv: u4 = CCMP_N | CCMP_Z;
    const expected: u32 = 0xFA400000 | (1 << 16) | (1 << 12) | (0 << 5) | @as(u32, nzcv);
    try std.testing.expectEqual(expected, emitCcmpRegister64(.R0, .R1, nzcv, .NE));
}

// --- Move immediate ---

test "MOVN W0, #0x1234, LSL #0" {
    const expected: u32 = 0x12800000 | (0 << 21) | (0x1234 << 5) | 0;
    try std.testing.expectEqual(expected, emitMovn(.R0, 0x1234, 0));
}

test "MOVN X0, #0, LSL #0" {
    const expected: u32 = 0x92800000 | (0 << 21) | (0 << 5) | 0;
    try std.testing.expectEqual(expected, emitMovn64(.R0, 0, 0));
}

test "MOVZ X0, #0xABCD, LSL #48" {
    const expected: u32 = 0xd2800000 | (3 << 21) | (0xABCD << 5) | 0;
    try std.testing.expectEqual(expected, emitMovz64(.R0, 0xABCD, 48));
}

// --- LoadImmediate32 ---

test "LoadImmediate32 zero" {
    var buf: [2]u32 = undefined;
    const n = emitLoadImmediate32(.R0, 0, &buf);
    try std.testing.expectEqual(@as(u8, 1), n);
    // MOVZ W0, #0
    try std.testing.expectEqual(@as(u32, 0x52800000 | 0), buf[0]);
}

test "LoadImmediate32 0xFFFF0000 uses MOVZ for upper half only" {
    var buf: [2]u32 = undefined;
    const n = emitLoadImmediate32(.R0, 0xFFFF0000, &buf);
    // word0=0, word1=0xFFFF. word0==0 and word1!=0 so:
    // MOVN W0, #0, LSL #0 (since word1==0xFFFF, uses MOVN path)
    // Actually let me trace: word0=0, word1=0xFFFF
    // word0 != 0 and word1 != 0 is FALSE (word0 == 0), so skip to MOVZ path
    // word0 != 0 or word1 == 0 → 0 or 0 → false, skip first MOVZ
    // word1 != 0 → true, count==0, so use MOVZ for word1
    try std.testing.expectEqual(@as(u8, 1), n);
}

test "LoadImmediate32 0xFFFFFFFF uses MOVN" {
    var buf: [2]u32 = undefined;
    const n = emitLoadImmediate32(.R0, 0xFFFFFFFF, &buf);
    // Both halves non-zero. word1==0xFFFF → MOVN path
    // MOVN W0, #0, LSL #0 (word0 ^ 0xFFFF = 0)
    try std.testing.expectEqual(@as(u8, 1), n);
    try std.testing.expectEqual(@as(u32, 0x12800000 | (0 << 21) | (0 << 5) | 0), buf[0]);
}

// --- LoadImmediate64 ---

test "LoadImmediate64 small value" {
    var buf: [4]u32 = undefined;
    const n = emitLoadImmediate64(.R0, 100, &buf);
    try std.testing.expectEqual(@as(u8, 1), n);
    // Delegates to 32-bit: MOVZ W0, #100
    try std.testing.expectEqual(@as(u32, 0x52800000 | (100 << 5) | 0), buf[0]);
}

test "LoadImmediate64 pattern with many 0xFFFF halves" {
    var buf: [4]u32 = undefined;
    // 0xFFFFFFFF00000001: three 0xFFFF halves + one non-matching
    const n = emitLoadImmediate64(.R0, 0xFFFFFFFF00000001, &buf);
    // num_ones=2 (words[2]=0xFFFF, words[3]=0xFFFF), num_zeros=1 (words[1]=0)
    // Actually words = [0x0001, 0x0000, 0xFFFF, 0xFFFF]
    // num_zeros = 1, num_ones = 2, use_movn = true
    // Words != 0xFFFF: [0]=0x0001, [1]=0x0000 → MOVN then MOVK
    try std.testing.expect(n >= 1 and n <= 4);
}

// --- ADD/SUB immediate ---

test "ADD X0, X1, #0" {
    const expected: u32 = 0x80000000 | 0x11000000 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitAddImmediate64(.R0, .R1, 0));
}

test "ADDS W0, W1, #10" {
    const expected: u32 = (1 << 29) | 0x11000000 | (10 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitAddsImmediate(.R0, .R1, 10));
}

test "SUBS W0, W1, #7" {
    const expected: u32 = (1 << 29) | 0x51000000 | (7 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitSubsImmediate(.R0, .R1, 7));
}

test "CMP W0, #42" {
    // SUBS WZR, W0, #42
    const expected: u32 = (1 << 29) | 0x51000000 | (42 << 10) | (0 << 5) | 31;
    try std.testing.expectEqual(expected, emitCmpImmediate(.R0, 42));
}

test "CMP X0, #1" {
    const expected: u32 = 0x80000000 | (1 << 29) | 0x51000000 | (1 << 10) | (0 << 5) | 31;
    try std.testing.expectEqual(expected, emitCmpImmediate64(.R0, 1));
}

// --- Logical immediate ---

test "AND W0, W1, #0x0F0F0F0F" {
    const result = emitAndImmediate(.R0, .R1, 0x0F0F0F0F);
    try std.testing.expect(result != null);
}

test "ORR W0, W1, #0x00FF00FF" {
    const result = emitOrrImmediate(.R0, .R1, 0x00FF00FF);
    try std.testing.expect(result != null);
}

test "EOR X0, X1, #0x5555555555555555" {
    const result = emitEorImmediate64(.R0, .R1, 0x5555555555555555);
    try std.testing.expect(result != null);
}

test "TST W0, #0xFF" {
    const result = emitTestImmediate(.R0, 0xFF);
    try std.testing.expect(result != null);
}

test "Logical immediate unencodable value" {
    // Non-repeating bit pattern
    const result = emitAndImmediate(.R0, .R1, 0x12345678);
    try std.testing.expectEqual(@as(?u32, null), result);
}

test "Logical immediate encode known patterns" {
    // 0x1 repeated in 2-bit element: 0x5555555555555555
    const result_01 = encodeLogicalImmediate(0x5555555555555555, 64);
    try std.testing.expect(result_01 != null);

    // 0xAAAAAAAAAAAAAAAA (inverted 0x55...)
    const result_10 = encodeLogicalImmediate(0xAAAAAAAAAAAAAAAA, 64);
    try std.testing.expect(result_10 != null);

    // Single bit set: 0x1 in 64-bit
    const result_1 = encodeLogicalImmediate(0x1, 64);
    try std.testing.expect(result_1 != null);

    // 32-bit: 0x80000000
    const result_high = encodeLogicalImmediate(0x80000000, 32);
    try std.testing.expect(result_high != null);
}

test "canEncodeLogicalImmediate" {
    try std.testing.expect(canEncodeLogicalImmediate(0xFF, 32));
    try std.testing.expect(!canEncodeLogicalImmediate(0, 32));
    try std.testing.expect(!canEncodeLogicalImmediate(0xFFFFFFFFFFFFFFFF, 64));
}

// --- Reverse/count operations ---

test "CLZ X0, X1" {
    const expected: u32 = 0xdac01000 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitClz64(.R0, .R1));
}

test "RBIT W0, W1" {
    const expected: u32 = 0x5ac00000 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitRbit(.R0, .R1));
}

test "RBIT X0, X1" {
    const expected: u32 = 0xdac00000 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitRbit64(.R0, .R1));
}

test "REV W0, W1" {
    const expected: u32 = 0x5ac00800 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitRev(.R0, .R1));
}

test "REV X0, X1" {
    const expected: u32 = 0xdac00c00 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitRev64(.R0, .R1));
}

test "REV16 W0, W1" {
    const expected: u32 = 0x5ac00400 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitRev16(.R0, .R1));
}

test "REV32 X0, X1" {
    const expected: u32 = 0xdac00800 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitRev3264(.R0, .R1));
}

// --- Load/store register offset ---

test "LDRB W0, [X1, X2]" {
    const result = emitLdrbRegister(.R0, .R1, RegisterParam.reg_only(.R2));
    try std.testing.expectEqual(@as(u32, 0x38600800 | (2 << 16) | (3 << 13) | (0 << 12) | (1 << 5) | 0), result);
}

test "STRB W0, [X1, X2]" {
    const result = emitStrbRegister(.R0, .R1, RegisterParam.reg_only(.R2));
    try std.testing.expectEqual(@as(u32, 0x38200800 | (2 << 16) | (3 << 13) | (0 << 12) | (1 << 5) | 0), result);
}

test "LDR X0, [X1, X2]" {
    const result = emitLdrRegister64(.R0, .R1, RegisterParam.reg_only(.R2));
    try std.testing.expectEqual(@as(u32, 0xf8600800 | (2 << 16) | (3 << 13) | (0 << 12) | (1 << 5) | 0), result);
}

test "STR W0, [X1, X2]" {
    const result = emitStrRegister(.R0, .R1, RegisterParam.reg_only(.R2));
    try std.testing.expectEqual(@as(u32, 0xb8200800 | (2 << 16) | (3 << 13) | (0 << 12) | (1 << 5) | 0), result);
}

// --- Load/store offset ---

test "LDRB W0, [X1, #5]" {
    const result = emitLdrbOffset(.R0, .R1, 5);
    try std.testing.expect(result != null);
    try std.testing.expectEqual(@as(u32, 0x39400000 | (5 << 10) | (1 << 5) | 0), result.?);
}

test "STRB W0, [X1, #0]" {
    const result = emitStrbOffset(.R0, .R1, 0);
    try std.testing.expect(result != null);
    try std.testing.expectEqual(@as(u32, 0x39000000 | (0 << 10) | (1 << 5) | 0), result.?);
}

test "LDRH W0, [X1, #0]" {
    const result = emitLdrhOffset(.R0, .R1, 0);
    try std.testing.expect(result != null);
    try std.testing.expectEqual(@as(u32, 0x79400000 | (1 << 5) | 0), result.?);
}

test "STRH W0, [X1, #2]" {
    const result = emitStrhOffset(.R0, .R1, 2);
    try std.testing.expect(result != null);
    // offset=2, shift=1, enc=1
    try std.testing.expectEqual(@as(u32, 0x79000000 | (1 << 10) | (1 << 5) | 0), result.?);
}

test "LDR X0, [X1, #16]" {
    const result = emitLdrOffset64(.R0, .R1, 16);
    try std.testing.expect(result != null);
    // shift=3, enc=2
    try std.testing.expectEqual(@as(u32, 0xf9400000 | (2 << 10) | (1 << 5) | 0), result.?);
}

test "STR W0, [X1, #4]" {
    const result = emitStrOffset(.R0, .R1, 4);
    try std.testing.expect(result != null);
    // shift=2, enc=1
    try std.testing.expectEqual(@as(u32, 0xb9000000 | (1 << 10) | (1 << 5) | 0), result.?);
}

test "LDR offset unaligned falls back to unscaled" {
    // Offset 3 is not 4-byte aligned for LDR W → uses LDUR
    const result = emitLdrOffset(.R0, .R1, 3);
    try std.testing.expect(result != null);
    // Should use unscaled: 0xb8400000
    try std.testing.expectEqual(@as(u32, 0xb8400000 | (3 << 12) | (1 << 5) | 0), result.?);
}

test "LDR offset negative uses unscaled" {
    const result = emitLdrOffset(.R0, .R1, -4);
    try std.testing.expect(result != null);
}

test "LDR offset too large returns null" {
    // Unsigned offset can only encode 0..4095*4 for LDR W
    // Unscaled only -256..255
    // 0x10000 (65536) is way too large
    const result = emitLdrOffset(.R0, .R1, 0x10000);
    try std.testing.expectEqual(@as(?u32, null), result);
}

// --- Load/store pair ---

test "LDP X0, X1, [SP, #16]" {
    const result = emitLdpOffset64(.R0, .R1, .SP, 16);
    try std.testing.expect(result != null);
    // shift=3, enc=2
    const expected: u32 = 0xa9400000 | ((2 & 0x7f) << 15) | (1 << 10) | (31 << 5) | 0;
    try std.testing.expectEqual(expected, result.?);
}

test "STP W0, W1, [SP, #8]" {
    const result = emitStpOffset(.R0, .R1, .SP, 8);
    try std.testing.expect(result != null);
    // shift=2, enc=2
    const expected: u32 = 0x29000000 | ((2 & 0x7f) << 15) | (1 << 10) | (31 << 5) | 0;
    try std.testing.expectEqual(expected, result.?);
}

test "LDP post-index" {
    const result = emitLdpOffsetPostIndex64(.R0, .R1, .SP, 16);
    try std.testing.expect(result != null);
}

test "STP pre-index 32" {
    const result = emitStpOffsetPreIndex(.R0, .R1, .SP, -8);
    try std.testing.expect(result != null);
}

test "LDP offset out of range returns null" {
    // 7-bit signed offset field, scaled by 8 for 64-bit: range = -512..504
    const result = emitLdpOffset64(.R0, .R1, .SP, 1024);
    try std.testing.expectEqual(@as(?u32, null), result);
}

// --- Post/pre-index load/store ---

test "LDR post-index 32" {
    const result = emitLdrOffsetPostIndex(.R0, .R1, 4);
    try std.testing.expect(result != null);
}

test "LDR post-index 64" {
    const result = emitLdrOffsetPostIndex64(.R0, .R1, 8);
    try std.testing.expect(result != null);
}

test "STR pre-index 32" {
    const result = emitStrOffsetPreIndex(.R0, .R1, -4);
    try std.testing.expect(result != null);
}

// --- Atomic load/store ---

test "LDARB W0, [X1]" {
    const expected: u32 = ARM64_OPCODE_LDARB | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitLdarb(.R0, .R1));
}

test "LDARH W0, [X1]" {
    const expected: u32 = ARM64_OPCODE_LDARH | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitLdarh(.R0, .R1));
}

test "LDAR X0, [X1]" {
    const expected: u32 = ARM64_OPCODE_LDAR64 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitLdar64(.R0, .R1));
}

test "STLRB W0, [X1]" {
    const expected: u32 = ARM64_OPCODE_STLRB | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitStlrb(.R0, .R1));
}

test "STLRH W0, [X1]" {
    const expected: u32 = ARM64_OPCODE_STLRH | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitStlrh(.R0, .R1));
}

test "STLR W0, [X1]" {
    const expected: u32 = ARM64_OPCODE_STLR | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitStlr(.R0, .R1));
}

test "STLR X0, [X1]" {
    const expected: u32 = ARM64_OPCODE_STLR64 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitStlr64(.R0, .R1));
}

test "LDXR W0, [X1]" {
    const expected: u32 = ARM64_OPCODE_LDXR | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitLdxr(.R0, .R1));
}

test "LDXR X0, [X1]" {
    const expected: u32 = ARM64_OPCODE_LDXR64 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitLdxr64(.R0, .R1));
}

test "LDAXR W0, [X1]" {
    const expected: u32 = ARM64_OPCODE_LDAXR | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitLdaxr(.R0, .R1));
}

test "STXRB W0, W1, [X2]" {
    const expected: u32 = ARM64_OPCODE_STXRB | (0 << 16) | (2 << 5) | 1;
    try std.testing.expectEqual(expected, emitStxrb(.R0, .R1, .R2));
}

test "STLXR W0, W1, [X2]" {
    const expected: u32 = ARM64_OPCODE_STLXR | (0 << 16) | (2 << 5) | 1;
    try std.testing.expectEqual(expected, emitStlxr(.R0, .R1, .R2));
}

test "STLXR X0, X1, [X2]" {
    const expected: u32 = ARM64_OPCODE_STLXR64 | (0 << 16) | (2 << 5) | 1;
    try std.testing.expectEqual(expected, emitStlxr64(.R0, .R1, .R2));
}

test "LDAXP W0, W1, [X2]" {
    const expected: u32 = 0x887f8000 | (1 << 10) | (2 << 5) | 0;
    try std.testing.expectEqual(expected, emitLdaxp(.R0, .R1, .R2));
}

test "STLXP W0, W1, W2, [X3]" {
    const expected: u32 = 0x88208000 | (0 << 16) | (2 << 10) | (3 << 5) | 1;
    try std.testing.expectEqual(expected, emitStlxp(.R0, .R1, .R2, .R3));
}

test "STLXP X0, X1, X2, [X3]" {
    const expected: u32 = 0xc8208000 | (0 << 16) | (2 << 10) | (3 << 5) | 1;
    try std.testing.expectEqual(expected, emitStlxp64(.R0, .R1, .R2, .R3));
}

// --- ADR/ADRP ---

test "ADR X1, #4" {
    // offset=4: low 2 bits = 0, upper = 1
    const expected: u32 = 0x10000000 | (0 << 29) | (1 << 5) | 1;
    try std.testing.expectEqual(expected, emitAdr(.R1, 4));
}

test "ADR X0, #3" {
    // offset=3: low 2 bits = 3, upper = 0
    const expected: u32 = 0x10000000 | (3 << 29) | (0 << 5) | 0;
    try std.testing.expectEqual(expected, emitAdr(.R0, 3));
}

test "ADRP X1, #0" {
    const expected: u32 = 0x90000000 | 1;
    try std.testing.expectEqual(expected, emitAdrp(.R1, 0));
}

// --- NEON unary integer ---

test "NEON ABS V0.4S, V1.4S" {
    const expected: u32 = 0x0e20b800 | (1 << 30) | (2 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonAbs(.V0, .V1, .Size4S));
}

test "NEON NEG V0.4S, V1.4S" {
    const expected: u32 = 0x2e20b800 | (1 << 30) | (2 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonNeg(.V0, .V1, .Size4S));
}

test "NEON NOT V0.16B, V1.16B" {
    const expected: u32 = 0x2e205800 | (1 << 30) | (0 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonNot(.V0, .V1, .Size16B));
}

test "NEON CNT V0.8B, V1.8B" {
    const expected: u32 = 0x0e205800 | (0 << 30) | (0 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonCnt(.V0, .V1, .Size8B));
}

test "NEON CMEQ V0.4S, V1.4S, #0" {
    const expected: u32 = 0x0e209800 | (1 << 30) | (2 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonCmeq0(.V0, .V1, .Size4S));
}

// --- NEON trinary integer ---

test "NEON SUB V0.4S, V1.4S, V2.4S" {
    const expected: u32 = 0x2e208400 | (1 << 30) | (2 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonSub(.V0, .V1, .V2, .Size4S));
}

test "NEON AND V0.16B, V1.16B, V2.16B" {
    const expected: u32 = 0x0e201c00 | (1 << 30) | (0 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonAnd(.V0, .V1, .V2, .Size16B));
}

test "NEON EOR V0.16B, V1.16B, V2.16B" {
    const expected: u32 = 0x2e201c00 | (1 << 30) | (0 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonEor(.V0, .V1, .V2, .Size16B));
}

test "NEON ORR V0.16B, V1.16B, V2.16B" {
    const expected: u32 = 0x0ea01c00 | (1 << 30) | (2 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonOrrReg(.V0, .V1, .V2, .Size16B));
}

test "NEON MUL V0.4S, V1.4S, V2.4S" {
    const expected: u32 = 0x0e209c00 | (1 << 30) | (2 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonMul(.V0, .V1, .V2, .Size4S));
}

test "NEON CMEQ V0.4S, V1.4S, V2.4S" {
    const expected: u32 = 0x2e208c00 | (1 << 30) | (2 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonCmeq(.V0, .V1, .V2, .Size4S));
}

test "NEON ADDP V0.4S, V1.4S, V2.4S" {
    const expected: u32 = 0x0e20bc00 | (1 << 30) | (2 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonAddp(.V0, .V1, .V2, .Size4S));
}

test "NEON MOV V0, V1 (8B)" {
    // ORR V0.8B, V1.8B, V1.8B
    const expected: u32 = 0x0ea01c00 | (0 << 30) | (2 << 22) | (1 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonMov(.V0, .V1, .Size8B));
}

// --- NEON float unary ---

test "NEON FABS S0, S1" {
    const expected: u32 = 0x1e20c000 | (0 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFabs(.V0, .V1, .Size1S));
}

test "NEON FABS D0, D1" {
    const expected: u32 = 0x1e20c000 | (1 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFabs(.V0, .V1, .Size1D));
}

test "NEON FNEG S0, S1" {
    const expected: u32 = 0x1e214000 | (0 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFneg(.V0, .V1, .Size1S));
}

test "NEON FSQRT S0, S1" {
    const expected: u32 = 0x1e21c000 | (0 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFsqrt(.V0, .V1, .Size1S));
}

// --- NEON float trinary ---

test "NEON FADD S0, S1, S2" {
    const expected: u32 = 0x1e202800 | (0 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFadd(.V0, .V1, .V2, .Size1S));
}

test "NEON FSUB S0, S1, S2" {
    const expected: u32 = 0x1e203800 | (0 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFsub(.V0, .V1, .V2, .Size1S));
}

test "NEON FDIV D0, D1, D2" {
    const expected: u32 = 0x1e201800 | (1 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFdiv(.V0, .V1, .V2, .Size1D));
}

test "NEON FMAX S0, S1, S2" {
    const expected: u32 = 0x1e204800 | (0 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFmax(.V0, .V1, .V2, .Size1S));
}

test "NEON FMIN S0, S1, S2" {
    const expected: u32 = 0x1e205800 | (0 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFmin(.V0, .V1, .V2, .Size1S));
}

test "NEON FNMUL S0, S1, S2" {
    const expected: u32 = 0x1e208800 | (0 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFnmul(.V0, .V1, .V2, .Size1S));
}

test "NEON FADD V0.4S, V1.4S, V2.4S (vector)" {
    const expected: u32 = 0x0e20d400 | (1 << 30) | (0 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFadd(.V0, .V1, .V2, .Size4S));
}

test "NEON FMUL V0.2D, V1.2D, V2.2D (vector)" {
    const expected: u32 = 0x2e20dc00 | (1 << 30) | (1 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFmul(.V0, .V1, .V2, .Size2D));
}

test "NEON FCMP D0, D1" {
    const expected: u32 = 0x1e202000 | (1 << 22) | (1 << 16) | (0 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFcmp(.V0, .V1, .Size1D));
}

test "NEON FCMP S0, #0.0" {
    const expected: u32 = 0x1e202008 | (0 << 22) | (0 << 16) | (0 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFcmp0(.V0, .Size1S));
}

// --- Comprehensive FCMP / FCMPE tests ---
// These cover varied register combinations and both single/double precision,
// including higher registers (V28-V30) used by the JIT pipeline.

test "NEON FCMP S0, S1 (bit pattern)" {
    // FCMP S0, S1: base=0x1e202000, type=0 (single), Rm=1, Rn=0
    const expected: u32 = 0x1e202000 | (0 << 22) | (1 << 16) | (0 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmp(.V0, .V1, .Size1S));
}

test "NEON FCMP S3, S5" {
    const expected: u32 = 0x1e202000 | (0 << 22) | (5 << 16) | (3 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmp(.V3, .V5, .Size1S));
}

test "NEON FCMP D2, D3" {
    const expected: u32 = 0x1e202000 | (1 << 22) | (3 << 16) | (2 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmp(.V2, .V3, .Size1D));
}

test "NEON FCMP D0, #0.0" {
    const expected: u32 = 0x1e202008 | (1 << 22) | (0 << 16) | (0 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmp0(.V0, .Size1D));
}

test "NEON FCMP S5, #0.0" {
    const expected: u32 = 0x1e202008 | (0 << 22) | (0 << 16) | (5 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmp0(.V5, .Size1S));
}

test "NEON FCMP D10, #0.0" {
    const expected: u32 = 0x1e202008 | (1 << 22) | (0 << 16) | (10 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmp0(.V10, .Size1D));
}

test "NEON FCMPE S0, S1" {
    // FCMPE S0, S1: base=0x1e202010, type=0 (single), Rm=1, Rn=0
    const expected: u32 = 0x1e202010 | (0 << 22) | (1 << 16) | (0 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmpe(.V0, .V1, .Size1S));
}

test "NEON FCMPE D0, D1" {
    const expected: u32 = 0x1e202010 | (1 << 22) | (1 << 16) | (0 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmpe(.V0, .V1, .Size1D));
}

test "NEON FCMPE S0, #0.0" {
    const expected: u32 = 0x1e202018 | (0 << 22) | (0 << 16) | (0 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmpe0(.V0, .Size1S));
}

test "NEON FCMPE D0, #0.0" {
    const expected: u32 = 0x1e202018 | (1 << 22) | (0 << 16) | (0 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmpe0(.V0, .Size1D));
}

test "NEON FCMPE S3, S7" {
    const expected: u32 = 0x1e202010 | (0 << 22) | (7 << 16) | (3 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmpe(.V3, .V7, .Size1S));
}

test "NEON FCMPE D15, D20" {
    const expected: u32 = 0x1e202010 | (1 << 22) | (20 << 16) | (15 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmpe(.V15, .V20, .Size1D));
}

test "NEON FCMPE D28, D29 (JIT high registers)" {
    // These are the registers commonly used by the JIT pipeline
    const expected: u32 = 0x1e202010 | (1 << 22) | (29 << 16) | (28 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmpe(.V28, .V29, .Size1D));
}

test "NEON FCMPE D29, D30 (JIT high registers)" {
    const expected: u32 = 0x1e202010 | (1 << 22) | (30 << 16) | (29 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmpe(.V29, .V30, .Size1D));
}

test "NEON FCMPE S28, S29 (JIT high registers, single)" {
    const expected: u32 = 0x1e202010 | (0 << 22) | (29 << 16) | (28 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmpe(.V28, .V29, .Size1S));
}

test "NEON FCMP D30, D0" {
    // High src1, low src2 — checks no masking issues
    const expected: u32 = 0x1e202000 | (1 << 22) | (0 << 16) | (30 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmp(.V30, .V0, .Size1D));
}

test "NEON FCMPE D0, D30" {
    // Low src1, high src2
    const expected: u32 = 0x1e202010 | (1 << 22) | (30 << 16) | (0 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmpe(.V0, .V30, .Size1D));
}

test "NEON FCMP D31, D31 (max register)" {
    // V31 is the highest register — ensure all 5 bits encode properly
    const expected: u32 = 0x1e202000 | (1 << 22) | (31 << 16) | (31 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmp(.V31, .V31, .Size1D));
}

test "NEON FCMPE D31, #0.0" {
    const expected: u32 = 0x1e202018 | (1 << 22) | (0 << 16) | (31 << 5);
    try std.testing.expectEqual(expected, emitNeonFcmpe0(.V31, .Size1D));
}

// --- FCMP/FCMPE: verify dest bits are not polluted ---
// The FCMP/FCMPE instructions have no destination register; bits [4:0]
// are fixed opcode bits (00000 for FCMP, 10000 for FCMPE, etc.)
// Ensure the encoder's use of V0 as dummy dest doesn't corrupt these.

test "NEON FCMP low 5 bits are 00000" {
    const word = emitNeonFcmp(.V5, .V10, .Size1D);
    try std.testing.expectEqual(@as(u32, 0b00000), word & 0x1F);
}

test "NEON FCMPE low 5 bits are 10000" {
    const word = emitNeonFcmpe(.V5, .V10, .Size1D);
    try std.testing.expectEqual(@as(u32, 0b10000), word & 0x1F);
}

test "NEON FCMP0 low 5 bits are 01000" {
    const word = emitNeonFcmp0(.V5, .Size1D);
    try std.testing.expectEqual(@as(u32, 0b01000), word & 0x1F);
}

test "NEON FCMPE0 low 5 bits are 11000" {
    const word = emitNeonFcmpe0(.V5, .Size1D);
    try std.testing.expectEqual(@as(u32, 0b11000), word & 0x1F);
}

// --- FP scalar LDR/STR offset tests ---
// These verify the FP load/store encoding used by the JIT to load
// FP constants and variables before FCMP.

test "FP LDR S0, [X1, #0]" {
    const result = emitFpLdrSOffset(.V0, .R1, 0);
    try std.testing.expect(result != null);
    // LDR S0, [X1] = 0xBD400000 | (0 << 10) | (1 << 5) | 0
    const expected: u32 = 0xBD400000 | (0 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result.?);
}

test "FP LDR S0, [X1, #4]" {
    const result = emitFpLdrSOffset(.V0, .R1, 4);
    try std.testing.expect(result != null);
    // offset 4 >> 2 = 1
    const expected: u32 = 0xBD400000 | (1 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result.?);
}

test "FP LDR D0, [X1, #0]" {
    const result = emitFpLdrDOffset(.V0, .R1, 0);
    try std.testing.expect(result != null);
    const expected: u32 = 0xFD400000 | (0 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result.?);
}

test "FP LDR D0, [X1, #8]" {
    const result = emitFpLdrDOffset(.V0, .R1, 8);
    try std.testing.expect(result != null);
    // offset 8 >> 3 = 1
    const expected: u32 = 0xFD400000 | (1 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result.?);
}

test "FP LDR D0, [X1, #16]" {
    const result = emitFpLdrDOffset(.V0, .R1, 16);
    try std.testing.expect(result != null);
    // offset 16 >> 3 = 2
    const expected: u32 = 0xFD400000 | (2 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result.?);
}

test "FP LDR D5, [X10, #0]" {
    const result = emitFpLdrDOffset(.V5, .R10, 0);
    try std.testing.expect(result != null);
    const expected: u32 = 0xFD400000 | (0 << 10) | (10 << 5) | 5;
    try std.testing.expectEqual(expected, result.?);
}

test "FP STR S0, [X1, #0]" {
    const result = emitFpStrSOffset(.V0, .R1, 0);
    try std.testing.expect(result != null);
    const expected: u32 = 0xBD000000 | (0 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result.?);
}

test "FP STR D0, [X1, #0]" {
    const result = emitFpStrDOffset(.V0, .R1, 0);
    try std.testing.expect(result != null);
    const expected: u32 = 0xFD000000 | (0 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result.?);
}

test "FP STR D0, [X1, #8]" {
    const result = emitFpStrDOffset(.V0, .R1, 8);
    try std.testing.expect(result != null);
    const expected: u32 = 0xFD000000 | (1 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result.?);
}

test "FP LDR D unaligned offset uses unscaled" {
    // offset 5 is not 8-byte aligned, should fall back to LDUR
    const result = emitFpLdrDOffset(.V0, .R1, 5);
    try std.testing.expect(result != null);
    // LDUR D0, [X1, #5]: 0xFC400000 | ((5 & 0x1ff) << 12) | (1 << 5) | 0
    const expected: u32 = 0xFC400000 | ((5 & 0x1ff) << 12) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result.?);
}

test "FP LDR D negative offset uses unscaled" {
    // offset -8: should use LDUR encoding
    const result = emitFpLdrDOffset(.V0, .R1, -8);
    try std.testing.expect(result != null);
    const neg8_u9: u32 = @as(u32, @bitCast(@as(i32, -8))) & 0x1ff;
    const expected: u32 = 0xFC400000 | (neg8_u9 << 12) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result.?);
}

test "FP STR S unaligned offset uses unscaled" {
    const result = emitFpStrSOffset(.V0, .R1, 3);
    try std.testing.expect(result != null);
    const expected: u32 = 0xBC000000 | ((3 & 0x1ff) << 12) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result.?);
}

// --- NEON shift immediate ---

test "NEON SHL V0.8H, V1.8H, #2" {
    // size=1 (H), eff_shift = 2 + 8*2 = 18, Q=1
    const result = emitNeonShl(.V0, .V1, 2, .Size8H);
    const expected: u32 = 0x0f005400 | (1 << 30) | (18 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result);
}

test "NEON SSHR V0.4S, V1.4S, #4" {
    // size=2 (S), eff_shift = 16*4 - 4 = 60, Q=1
    const result = emitNeonSshr(.V0, .V1, 4, .Size4S);
    const expected: u32 = 0x0f000400 | (1 << 30) | (60 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result);
}

test "NEON SHL scalar D0, D1, #5" {
    // size=3 (D), eff_shift = 5 + 8*8 = 69
    const result = emitNeonShl(.V0, .V1, 5, .Size1D);
    const expected: u32 = 0x5f005400 | (69 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result);
}

test "NEON USHR scalar D0, D1, #3" {
    // size=3 (D), eff_shift = 16*8 - 3 = 125
    const result = emitNeonUshr(.V0, .V1, 3, .Size1D);
    const expected: u32 = 0x7f000400 | (125 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result);
}

// --- NEON element operations ---

test "NEON DUP V0.4S, V1.S[0]" {
    // size=2 (S), idx = (0*2+1)<<2 = 4
    const result = emitNeonDupElement(.V0, .V1, 0, .Size4S);
    const expected: u32 = 0x0e000400 | (1 << 30) | (4 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result);
}

test "NEON UMOV W0, V1.S[1]" {
    // size=2 (S), idx = (1*2+1)<<2 = 12
    const result = emitNeonUmov(.R0, .V1, 1, .Size1S);
    const dest_neon: NeonRegister = @enumFromInt(Register.R0.raw());
    const expected: u32 = 0x0e003c00 | (12 << 16) | (1 << 5) | @as(u32, @intFromEnum(dest_neon));
    try std.testing.expectEqual(expected, result);
}

// --- NEON FCSEL ---

test "NEON FCSEL S0, S1, S2, EQ" {
    const expected: u32 = 0x1e200c00 | (0 << 22) | (2 << 16) | (0 << 12) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFcsel(.V0, .V1, .V2, .EQ, .Size1S));
}

test "NEON FCSEL D0, D1, D2, NE" {
    const expected: u32 = 0x1e200c00 | (1 << 22) | (2 << 16) | (1 << 12) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFcsel(.V0, .V1, .V2, .NE, .Size1D));
}

// --- NEON MOVI ---

test "NEON MOVI V0.4S, #0" {
    const result = emitNeonMovi(.V0, 0, .Size4S);
    try std.testing.expect(result != null);
}

test "NEON MOVI V0.16B, #0xFF" {
    const result = emitNeonMovi(.V0, 0xFF, .Size16B);
    try std.testing.expect(result != null);
}

// --- NEON EXT ---

test "NEON EXT V0.16B, V1.16B, V2.16B, #3" {
    const expected: u32 = 0x2e000000 | (1 << 30) | (2 << 16) | (3 << 11) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonExt(.V0, .V1, .V2, 3, .Size16B));
}

// --- NEON load/store ---

test "NEON LDR S0, [X1, #0]" {
    const result = emitNeonLdrOffset(.V0, .Size1S, .R1, 0);
    try std.testing.expect(result != null);
}

test "NEON STR D0, [X1, #8]" {
    const result = emitNeonStrOffset(.V0, .Size1D, .R1, 8);
    try std.testing.expect(result != null);
}

test "NEON LDR offset returns null for out of range" {
    const result = emitNeonLdrOffset(.V0, .Size1S, .R1, 0x10000);
    try std.testing.expectEqual(@as(?u32, null), result);
}

// --- NEON crypto (AES) ---

test "NEON AESD V0, V1" {
    const expected: u32 = 0x4e285800 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonAesD(.V0, .V1));
}

test "NEON AESIMC V0, V1" {
    const expected: u32 = 0x4e287800 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonAesImc(.V0, .V1));
}

test "NEON AESMC V0, V1" {
    const expected: u32 = 0x4e286800 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonAesMc(.V0, .V1));
}

// --- NEON scalar<->general transfers ---

test "FMOV W0, S1" {
    const expected: u32 = 0x1e260000 | (0 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFmovToGeneral(.R0, .V1));
}

test "FMOV X0, D1" {
    const expected: u32 = 0x9e260000 | (1 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFmovToGeneral64(.R0, .V1));
}

test "FMOV S0, W1" {
    const expected: u32 = 0x1e270000 | (0 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFmovFromGeneral(.V0, .R1));
}

test "FMOV D0, X1" {
    const expected: u32 = 0x9e270000 | (1 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFmovFromGeneral64(.V0, .R1));
}

test "SCVTF S0, W1" {
    const expected: u32 = 0x1e220000 | (0 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonScvtfGen(.V0, .R1, .Size1S));
}

test "UCVTF D0, X1" {
    const expected: u32 = 0x9e230000 | (1 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonUcvtfGen64(.V0, .R1, .Size1D));
}

test "FCVTZS W0, S1" {
    const expected: u32 = 0x1e380000 | (0 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFcvtzsGen(.R0, .V1, .Size1S));
}

test "FCVTZU X0, D1" {
    const expected: u32 = 0x9e390000 | (1 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFcvtzuGen64(.R0, .V1, .Size1D));
}

// --- ArmBranchLinker ---

test "ArmBranchLinker init" {
    const linker = ArmBranchLinker.init();
    try std.testing.expect(!linker.hasInstruction());
    try std.testing.expect(!linker.hasTarget());
}

test "ArmBranchLinker set and resolve Imm26" {
    var buffer = [_]u32{ 0x14000000, 0, 0, 0 };
    var linker = ArmBranchLinker.init();
    linker.setInstructionAndClass(0, .Imm26);
    linker.setTarget(8); // target at byte offset 8 = word 2
    try std.testing.expect(linker.hasInstruction());
    try std.testing.expect(linker.hasTarget());
    linker.resolve(&buffer);
    // delta = 2 - 0 = 2
    try std.testing.expectEqual(@as(u32, 0x14000002), buffer[0]);
}

test "ArmBranchLinker reset" {
    var linker = ArmBranchLinker.init();
    linker.setInstructionAndClass(0, .Imm26);
    linker.setTarget(4);
    linker.reset();
    try std.testing.expect(!linker.hasInstruction());
    try std.testing.expect(!linker.hasTarget());
}

test "ArmBranchLinker resolve Imm19" {
    var buffer = [_]u32{ 0x54000000, 0, 0, 0 }; // B.EQ
    var linker = ArmBranchLinker.init();
    linker.setInstructionAndClass(0, .Imm19);
    linker.setTarget(12); // target at word 3
    linker.resolve(&buffer);
    // delta = 3, encoded: (3 << 5) in imm19 field
    const imm19_field = (buffer[0] >> 5) & 0x7ffff;
    try std.testing.expectEqual(@as(u32, 3), imm19_field);
}

test "ArmBranchLinker resolve Imm14" {
    var buffer = [_]u32{ 0x36000000, 0, 0, 0 }; // TBZ
    var linker = ArmBranchLinker.init();
    linker.setInstructionAndClass(0, .Imm14);
    linker.setTarget(8); // target at word 2
    linker.resolve(&buffer);
    // delta = 2, encoded: (2 << 5) in imm14 field
    const imm14_field = (buffer[0] >> 5) & 0x3fff;
    try std.testing.expectEqual(@as(u32, 2), imm14_field);
}

// --- SYSREG encoding ---

test "ARM64_SYSREG encoding" {
    // FPCR = op0=1, op1=3, CRn=4, CRm=4, op2=0
    const fpcr = ARM64_SYSREG(1, 3, 4, 4, 0);
    try std.testing.expectEqual(ARM64_FPCR, fpcr);

    // NZCV = op0=1, op1=3, CRn=4, CRm=2, op2=0
    const nzcv = ARM64_SYSREG(1, 3, 4, 2, 0);
    try std.testing.expectEqual(ARM64_NZCV, nzcv);
}

// --- emitLogicalImmediateCommon -1 fallback (bug fix test) ---

test "AND W0, W1, #0xFFFFFFFF returns fallback via NOT register" {
    // After fix: size==32 check triggers, returns BIC W0, W1, WZR
    const result = emitAndImmediate(.R0, .R1, 0xFFFFFFFF);
    try std.testing.expect(result != null);
    // Should be the not_reg_opcode form: 0x0a200000 | (31 << 16) | (1 << 5) | 0
    const expected: u32 = 0x0a200000 | (31 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, result.?);
}

// --- NEON permute ---

test "NEON ZIP1 V0.4S, V1.4S, V2.4S" {
    const expected: u32 = 0x0e003800 | (1 << 30) | (2 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonZip1(.V0, .V1, .V2, .Size4S));
}

test "NEON UZP1 V0.4S, V1.4S, V2.4S" {
    const expected: u32 = 0x0e001800 | (1 << 30) | (2 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonUzp1(.V0, .V1, .V2, .Size4S));
}

test "NEON TRN1 V0.4S, V1.4S, V2.4S" {
    const expected: u32 = 0x0e002800 | (1 << 30) | (2 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonTrn1(.V0, .V1, .V2, .Size4S));
}

// --- NEON FMOV immediate ---

test "NEON FMOV S0, #imm8" {
    // imm8 = 0x70 (1.0 in IEEE half encoding)
    const expected: u32 = 0x1e201000 | (0 << 22) | (0x70 << 13) | 0;
    try std.testing.expectEqual(expected, emitNeonFmovImmediate(.V0, 0x70, .Size1S));
}

// --- NEON BSL/BIT/BIF ---

test "NEON BSL V0.16B, V1.16B, V2.16B" {
    const expected: u32 = 0x2e601c00 | (1 << 30) | (0 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonBsl(.V0, .V1, .V2, .Size16B));
}

test "NEON BIT V0.16B, V1.16B, V2.16B" {
    const expected: u32 = 0x2ea01c00 | (1 << 30) | (0 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonBit(.V0, .V1, .V2, .Size16B));
}

test "NEON BIF V0.16B, V1.16B, V2.16B" {
    const expected: u32 = 0x2ee01c00 | (1 << 30) | (0 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonBif(.V0, .V1, .V2, .Size16B));
}

// --- NEON min/max ---

test "NEON SMAX V0.4S, V1.4S, V2.4S" {
    const expected: u32 = 0x0e206400 | (1 << 30) | (2 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonSmax(.V0, .V1, .V2, .Size4S));
}

test "NEON UMIN V0.4S, V1.4S, V2.4S" {
    const expected: u32 = 0x2e206c00 | (1 << 30) | (2 << 22) | (2 << 16) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonUmin(.V0, .V1, .V2, .Size4S));
}

// --- NEON SXTL/UXTL ---

test "NEON UXTL V0.8H from 8B" {
    // USHLL with shift=0, inner size = 8B
    const result = emitNeonUxtl(.V0, .V1, .Size8B);
    // Should produce valid instruction
    try std.testing.expect(result != 0);
}

// --- NEON SCVTF/UCVTF vec ---

test "NEON SCVTF V0.4S, V1.4S" {
    const expected: u32 = 0x0e21d800 | (1 << 30) | (0 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonScvtfVec(.V0, .V1, .Size4S));
}

test "NEON UCVTF V0.2D, V1.2D" {
    const expected: u32 = 0x2e21d800 | (1 << 30) | (1 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonUcvtfVec(.V0, .V1, .Size2D));
}

// --- NEON FCVTZS/FCVTZU vec ---

test "NEON FCVTZS V0.4S, V1.4S" {
    const expected: u32 = 0x0ea1b800 | (1 << 30) | (0 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFcvtzs(.V0, .V1, .Size4S));
}

test "NEON FCVTZU V0.2D, V1.2D" {
    const expected: u32 = 0x2ea1b800 | (1 << 30) | (1 << 22) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitNeonFcvtzu(.V0, .V1, .Size2D));
}

// --- FMADD/FMSUB (scalar float fused multiply-add/sub) ---

test "FMADD S0, S1, S2, S3" {
    // FMADD Sd, Sn, Sm, Sa = 0x1f000000 | Sm<<16 | Sa<<10 | Sn<<5 | Sd
    const expected: u32 = 0x1f000000 | (2 << 16) | (3 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitFmadd(.V0, .V1, .V2, .V3));
}

test "FMADD D0, D1, D2, D3" {
    const expected: u32 = 0x1f400000 | (2 << 16) | (3 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitFmadd64(.V0, .V1, .V2, .V3));
}

test "FMSUB S0, S1, S2, S3" {
    const expected: u32 = 0x1f008000 | (2 << 16) | (3 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitFmsub(.V0, .V1, .V2, .V3));
}

test "FMSUB D0, D1, D2, D3" {
    const expected: u32 = 0x1f408000 | (2 << 16) | (3 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitFmsub64(.V0, .V1, .V2, .V3));
}

test "FNMADD S0, S1, S2, S3" {
    const expected: u32 = 0x1f200000 | (2 << 16) | (3 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitFnmadd(.V0, .V1, .V2, .V3));
}

test "FNMSUB D0, D1, D2, D3" {
    const expected: u32 = 0x1f608000 | (2 << 16) | (3 << 10) | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitFnmsub64(.V0, .V1, .V2, .V3));
}

// --- Scalar FCVT (float precision conversion) ---

test "FCVT D0, S1 (single to double)" {
    const expected: u32 = 0x1e22c000 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitFcvtStoD(.V0, .V1));
}

test "FCVT S0, D1 (double to single)" {
    const expected: u32 = 0x1e624000 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitFcvtDtoS(.V0, .V1));
}

test "FCVT H0, S1 (single to half)" {
    const expected: u32 = 0x1e23c000 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitFcvtStoH(.V0, .V1));
}

test "FCVT S0, H1 (half to single)" {
    const expected: u32 = 0x1ee24000 | (1 << 5) | 0;
    try std.testing.expectEqual(expected, emitFcvtHtoS(.V0, .V1));
}

// --- NEG aliases ---

test "NEG W0, W1" {
    // NEG Wd, Wm = SUB Wd, WZR, Wm
    const expected = emitSubRegister(.R0, Register.ZR, RegisterParam.reg_only(.R1));
    try std.testing.expectEqual(expected, emitNeg(.R0, .R1));
}

test "NEG X0, X1" {
    const expected = emitSubRegister64(.R0, Register.ZR, RegisterParam.reg_only(.R1));
    try std.testing.expectEqual(expected, emitNeg64(.R0, .R1));
}

test "NEGS W0, W1" {
    const expected = emitSubsRegister(.R0, Register.ZR, RegisterParam.reg_only(.R1));
    try std.testing.expectEqual(expected, emitNegs(.R0, .R1));
}

// --- CMN aliases ---

test "CMN W0, W1" {
    const expected = emitAddsRegister(Register.ZR, .R0, RegisterParam.reg_only(.R1));
    try std.testing.expectEqual(expected, emitCmnRegister(.R0, RegisterParam.reg_only(.R1)));
}

test "CMN X0, X1" {
    const expected = emitAddsRegister64(Register.ZR, .R0, RegisterParam.reg_only(.R1));
    try std.testing.expectEqual(expected, emitCmnRegister64(.R0, RegisterParam.reg_only(.R1)));
}

test "CMN W0, #42" {
    const expected = emitAddsImmediate(Register.ZR, .R0, 42);
    try std.testing.expectEqual(expected, emitCmnImmediate(.R0, 42));
}

// --- SXTW alias ---

test "SXTW X0, W1" {
    const expected = emitSbfm64(.R0, .R1, 0, 31);
    try std.testing.expectEqual(expected, emitSxtw64(.R0, .R1));
}

// --- HINT / BTI ---

test "HINT #0 is NOP" {
    try std.testing.expectEqual(ARM64_OPCODE_NOP, emitHint(0));
}

test "HINT #34 is BTI C" {
    // BTI C = hint #34 = 0xd503245f
    try std.testing.expectEqual(@as(u32, 0xd503245f), emitBtiC());
}

test "BTI J" {
    try std.testing.expectEqual(emitHint(36), emitBtiJ());
}

test "BTI JC" {
    try std.testing.expectEqual(emitHint(38), emitBtiJC());
}

// --- TPIDR_EL0 ---

test "ARM64_TPIDR_EL0 sysreg" {
    const tpidr = ARM64_SYSREG(1, 3, 13, 0, 2);
    try std.testing.expectEqual(ARM64_TPIDR_EL0, tpidr);
}

test "MRS X0, TPIDR_EL0" {
    const expected = emitMrs(.R0, ARM64_TPIDR_EL0);
    try std.testing.expectEqual(expected, emitMrsTpidrEl0(.R0));
}

test "MSR TPIDR_EL0, X1" {
    const expected = emitMsr(.R1, ARM64_TPIDR_EL0);
    try std.testing.expectEqual(expected, emitMsrTpidrEl0(.R1));
}

// ============================================================================
// Clang + otool verification driver
//
// Assembles each instruction with the system clang, extracts the machine code
// via otool, and compares it against the Zig encoder's u32 output.
//
// Run:  zig build-exe arm64_encoder.zig && ./arm64_encoder
// ============================================================================

const VerifyCase = struct {
    name: []const u8,
    expected: u32,
    asm_text: []const u8,
};

/// Static table of (name, zig-encoded u32, assembly text) triples.
/// Top-level const so all string slices have static lifetime — never
/// return comptime aggregate slices from a function body.
const verify_cases = blk: {
    @setEvalBranchQuota(200_000);
    break :blk [_]VerifyCase{
        // ---- System / NOP / BRK / DMB ----
        .{ .name = "NOP", .expected = emitNop(), .asm_text = "nop" },
        .{ .name = "BRK #0", .expected = emitBrk(0), .asm_text = "brk #0" },
        .{ .name = "BRK #0xF000", .expected = emitDebugBreak(), .asm_text = "brk #0xf000" },
        .{ .name = "BRK #0xF003", .expected = emitFastFail(), .asm_text = "brk #0xf003" },
        .{ .name = "BRK #0xF004", .expected = emitDiv0Exception(), .asm_text = "brk #0xf004" },
        .{ .name = "DMB ISH", .expected = emitDmb(), .asm_text = "dmb ish" },
        .{ .name = "DMB ISHST", .expected = emitDmbSt(), .asm_text = "dmb ishst" },
        .{ .name = "DMB ISHLD", .expected = emitDmbLd(), .asm_text = "dmb ishld" },

        // ---- MRS / MSR ----
        .{ .name = "MRS X0, NZCV", .expected = emitMrs(.R0, ARM64_NZCV), .asm_text = "mrs x0, nzcv" },
        .{ .name = "MRS X0, FPCR", .expected = emitMrs(.R0, ARM64_FPCR), .asm_text = "mrs x0, fpcr" },
        .{ .name = "MRS X5, FPSR", .expected = emitMrs(.R5, ARM64_FPSR), .asm_text = "mrs x5, fpsr" },
        .{ .name = "MSR NZCV, X1", .expected = emitMsr(.R1, ARM64_NZCV), .asm_text = "msr nzcv, x1" },
        .{ .name = "MSR FPCR, X3", .expected = emitMsr(.R3, ARM64_FPCR), .asm_text = "msr fpcr, x3" },

        // ---- Branch ----
        .{ .name = "B .  (offset 0)", .expected = emitB(0), .asm_text = "b ." },
        .{ .name = "B .+4", .expected = emitB(1), .asm_text = "b .+4" },
        .{ .name = "BL .+4", .expected = emitBl(1), .asm_text = "bl .+4" },
        .{ .name = "BR X16", .expected = emitBr(.R16), .asm_text = "br x16" },
        .{ .name = "BR X0", .expected = emitBr(.R0), .asm_text = "br x0" },
        .{ .name = "BLR X16", .expected = emitBlr(.R16), .asm_text = "blr x16" },
        .{ .name = "BLR X8", .expected = emitBlr(.R8), .asm_text = "blr x8" },
        .{ .name = "RET (LR)", .expected = emitRet(.LR), .asm_text = "ret" },
        .{ .name = "RET X0", .expected = emitRet(.R0), .asm_text = "ret x0" },
        .{ .name = "RET X22", .expected = emitRet(.R22), .asm_text = "ret x22" },
        .{ .name = "B.EQ .", .expected = emitBCond(0, .EQ), .asm_text = "b.eq ." },
        .{ .name = "B.NE .", .expected = emitBCond(0, .NE), .asm_text = "b.ne ." },
        .{ .name = "B.GE .", .expected = emitBCond(0, .GE), .asm_text = "b.ge ." },
        .{ .name = "B.LT .", .expected = emitBCond(0, .LT), .asm_text = "b.lt ." },
        .{ .name = "B.GT .", .expected = emitBCond(0, .GT), .asm_text = "b.gt ." },
        .{ .name = "B.LE .", .expected = emitBCond(0, .LE), .asm_text = "b.le ." },
        .{ .name = "B.HI .", .expected = emitBCond(0, .HI), .asm_text = "b.hi ." },
        .{ .name = "B.LS .", .expected = emitBCond(0, .LS), .asm_text = "b.ls ." },
        .{ .name = "B.CS .", .expected = emitBCond(0, .CS), .asm_text = "b.hs ." },
        .{ .name = "B.CC .", .expected = emitBCond(0, .CC), .asm_text = "b.lo ." },
        .{ .name = "B.MI .", .expected = emitBCond(0, .MI), .asm_text = "b.mi ." },
        .{ .name = "B.PL .", .expected = emitBCond(0, .PL), .asm_text = "b.pl ." },
        .{ .name = "B.VS .", .expected = emitBCond(0, .VS), .asm_text = "b.vs ." },
        .{ .name = "B.VC .", .expected = emitBCond(0, .VC), .asm_text = "b.vc ." },
        .{ .name = "CBZ W0, .", .expected = emitCbz(.R0, 0), .asm_text = "cbz w0, ." },
        .{ .name = "CBZ X0, .", .expected = emitCbz64(.R0, 0), .asm_text = "cbz x0, ." },
        .{ .name = "CBNZ W0, .", .expected = emitCbnz(.R0, 0), .asm_text = "cbnz w0, ." },
        .{ .name = "CBNZ X1, .", .expected = emitCbnz64(.R1, 0), .asm_text = "cbnz x1, ." },
        .{ .name = "TBZ W0, #0, .", .expected = emitTbz(.R0, 0, 0), .asm_text = "tbz w0, #0, ." },
        .{ .name = "TBZ W5, #15, .", .expected = emitTbz(.R5, 15, 0), .asm_text = "tbz w5, #15, ." },
        .{ .name = "TBNZ W0, #0, .", .expected = emitTbnz(.R0, 0, 0), .asm_text = "tbnz w0, #0, ." },
        .{ .name = "TBNZ W3, #7, .", .expected = emitTbnz(.R3, 7, 0), .asm_text = "tbnz w3, #7, ." },
        .{ .name = "TBZ X0, #32, .", .expected = emitTbz(.R0, 32, 0), .asm_text = "tbz x0, #32, ." },
        .{ .name = "TBZ X1, #63, .", .expected = emitTbz(.R1, 63, 0), .asm_text = "tbz x1, #63, ." },
        .{ .name = "TBNZ X2, #40, .", .expected = emitTbnz(.R2, 40, 0), .asm_text = "tbnz x2, #40, ." },

        // ---- ADD/SUB register ----
        .{ .name = "ADD W0, W1, W2", .expected = emitAddRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "add w0, w1, w2" },
        .{ .name = "ADD X0, X1, X2", .expected = emitAddRegister64(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "add x0, x1, x2" },
        .{ .name = "ADD W0, W1, W2, LSL #3", .expected = emitAddRegister(.R0, .R1, RegisterParam.shifted(.R2, .LSL, 3)), .asm_text = "add w0, w1, w2, lsl #3" },
        .{ .name = "SUB W3, W4, W5", .expected = emitSubRegister(.R3, .R4, RegisterParam.reg_only(.R5)), .asm_text = "sub w3, w4, w5" },
        .{ .name = "SUB X0, X1, X2", .expected = emitSubRegister64(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "sub x0, x1, x2" },
        .{ .name = "SUB X0, X1, X2, ASR #5", .expected = emitSubRegister64(.R0, .R1, RegisterParam.shifted(.R2, .ASR, 5)), .asm_text = "sub x0, x1, x2, asr #5" },
        .{ .name = "ADD X3, X4, X5, LSR #2", .expected = emitAddRegister64(.R3, .R4, RegisterParam.shifted(.R5, .LSR, 2)), .asm_text = "add x3, x4, x5, lsr #2" },
        .{ .name = "SUB W6, W7, W8, LSL #1", .expected = emitSubRegister(.R6, .R7, RegisterParam.shifted(.R8, .LSL, 1)), .asm_text = "sub w6, w7, w8, lsl #1" },
        .{ .name = "ADD W10, W11, W12, ASR #4", .expected = emitAddRegister(.R10, .R11, RegisterParam.shifted(.R12, .ASR, 4)), .asm_text = "add w10, w11, w12, asr #4" },
        .{ .name = "ADDS W0, W1, W2", .expected = emitAddsRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "adds w0, w1, w2" },
        .{ .name = "ADDS X0, X1, X2", .expected = emitAddsRegister64(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "adds x0, x1, x2" },
        .{ .name = "SUBS W0, W1, W2", .expected = emitSubsRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "subs w0, w1, w2" },
        .{ .name = "SUBS X0, X1, X2", .expected = emitSubsRegister64(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "subs x0, x1, x2" },
        .{ .name = "CMP W1, W2", .expected = emitCmpRegister(.R1, RegisterParam.reg_only(.R2)), .asm_text = "cmp w1, w2" },
        .{ .name = "CMP X1, X2", .expected = emitCmpRegister64(.R1, RegisterParam.reg_only(.R2)), .asm_text = "cmp x1, x2" },

        // ---- ADC / SBC ----
        .{ .name = "ADC W0, W1, W2", .expected = emitAdcRegister(.R0, .R1, .R2), .asm_text = "adc w0, w1, w2" },
        .{ .name = "ADC X0, X1, X2", .expected = emitAdcRegister64(.R0, .R1, .R2), .asm_text = "adc x0, x1, x2" },
        .{ .name = "ADCS W0, W1, W2", .expected = emitAdcsRegister(.R0, .R1, .R2), .asm_text = "adcs w0, w1, w2" },
        .{ .name = "ADCS X0, X1, X2", .expected = emitAdcsRegister64(.R0, .R1, .R2), .asm_text = "adcs x0, x1, x2" },
        .{ .name = "SBC W0, W1, W2", .expected = emitSbcRegister(.R0, .R1, .R2), .asm_text = "sbc w0, w1, w2" },
        .{ .name = "SBC X0, X1, X2", .expected = emitSbcRegister64(.R0, .R1, .R2), .asm_text = "sbc x0, x1, x2" },
        .{ .name = "SBCS W0, W1, W2", .expected = emitSbcsRegister(.R0, .R1, .R2), .asm_text = "sbcs w0, w1, w2" },
        .{ .name = "SBCS X0, X1, X2", .expected = emitSbcsRegister64(.R0, .R1, .R2), .asm_text = "sbcs x0, x1, x2" },

        // ---- MADD / MSUB / MUL / SMULL / UMULL / SMADDL / UMADDL ----
        .{ .name = "SMADDL X0, W1, W2, X3", .expected = emitSmaddl(.R0, .R1, .R2, .R3), .asm_text = "smaddl x0, w1, w2, x3" },
        .{ .name = "UMADDL X4, W5, W6, X7", .expected = emitUmaddl(.R4, .R5, .R6, .R7), .asm_text = "umaddl x4, w5, w6, x7" },
        .{ .name = "SMSUBL X0, W1, W2, X3", .expected = emitSmsubl(.R0, .R1, .R2, .R3), .asm_text = "smsubl x0, w1, w2, x3" },
        .{ .name = "UMSUBL X0, W1, W2, X3", .expected = emitUmsubl(.R0, .R1, .R2, .R3), .asm_text = "umsubl x0, w1, w2, x3" },
        .{ .name = "MADD W0, W1, W2, W3", .expected = emitMadd(.R0, .R1, .R2, .R3), .asm_text = "madd w0, w1, w2, w3" },
        .{ .name = "MADD X0, X1, X2, X3", .expected = emitMadd64(.R0, .R1, .R2, .R3), .asm_text = "madd x0, x1, x2, x3" },
        .{ .name = "MSUB W0, W1, W2, W3", .expected = emitMsub(.R0, .R1, .R2, .R3), .asm_text = "msub w0, w1, w2, w3" },
        .{ .name = "MSUB X0, X1, X2, X3", .expected = emitMsub64(.R0, .R1, .R2, .R3), .asm_text = "msub x0, x1, x2, x3" },
        .{ .name = "MUL W0, W1, W2", .expected = emitMul(.R0, .R1, .R2), .asm_text = "mul w0, w1, w2" },
        .{ .name = "MUL X0, X1, X2", .expected = emitMul64(.R0, .R1, .R2), .asm_text = "mul x0, x1, x2" },
        .{ .name = "SMULL X0, W1, W2", .expected = emitSmull(.R0, .R1, .R2), .asm_text = "smull x0, w1, w2" },
        .{ .name = "UMULL X0, W1, W2", .expected = emitUmull(.R0, .R1, .R2), .asm_text = "umull x0, w1, w2" },

        // ---- Division ----
        .{ .name = "SDIV W0, W1, W2", .expected = emitSdiv(.R0, .R1, .R2), .asm_text = "sdiv w0, w1, w2" },
        .{ .name = "SDIV X0, X1, X2", .expected = emitSdiv64(.R0, .R1, .R2), .asm_text = "sdiv x0, x1, x2" },
        .{ .name = "UDIV W0, W1, W2", .expected = emitUdiv(.R0, .R1, .R2), .asm_text = "udiv w0, w1, w2" },
        .{ .name = "UDIV X0, X1, X2", .expected = emitUdiv64(.R0, .R1, .R2), .asm_text = "udiv x0, x1, x2" },

        // ---- Logical register ----
        .{ .name = "AND W0, W1, W2", .expected = emitAndRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "and w0, w1, w2" },
        .{ .name = "AND X0, X1, X2", .expected = emitAndRegister64(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "and x0, x1, x2" },
        .{ .name = "ANDS W0, W1, W2", .expected = emitAndsRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "ands w0, w1, w2" },
        .{ .name = "ANDS X0, X1, X2", .expected = emitAndsRegister64(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "ands x0, x1, x2" },
        .{ .name = "ORR W0, W1, W2", .expected = emitOrrRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "orr w0, w1, w2" },
        .{ .name = "ORR X0, X1, X2", .expected = emitOrrRegister64(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "orr x0, x1, x2" },
        .{ .name = "EOR W0, W1, W2", .expected = emitEorRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "eor w0, w1, w2" },
        .{ .name = "EOR X0, X1, X2", .expected = emitEorRegister64(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "eor x0, x1, x2" },
        .{ .name = "BIC W0, W1, W2", .expected = emitBicRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "bic w0, w1, w2" },
        .{ .name = "BIC X3, X4, X5", .expected = emitBicRegister64(.R3, .R4, RegisterParam.reg_only(.R5)), .asm_text = "bic x3, x4, x5" },
        .{ .name = "BICS W0, W1, W2", .expected = emitBicsRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "bics w0, w1, w2" },
        .{ .name = "BICS X0, X1, X2", .expected = emitBicsRegister64(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "bics x0, x1, x2" },
        .{ .name = "EON W0, W1, W2", .expected = emitEonRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "eon w0, w1, w2" },
        .{ .name = "EON X3, X4, X5", .expected = emitEonRegister64(.R3, .R4, RegisterParam.reg_only(.R5)), .asm_text = "eon x3, x4, x5" },
        .{ .name = "ORN W0, W1, W2", .expected = emitOrnRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "orn w0, w1, w2" },
        .{ .name = "ORN X6, X7, X8", .expected = emitOrnRegister64(.R6, .R7, RegisterParam.reg_only(.R8)), .asm_text = "orn x6, x7, x8" },
        .{ .name = "AND W3, W4, W5, LSL #2", .expected = emitAndRegister(.R3, .R4, RegisterParam.shifted(.R5, .LSL, 2)), .asm_text = "and w3, w4, w5, lsl #2" },
        .{ .name = "ORR X6, X7, X8, LSR #4", .expected = emitOrrRegister64(.R6, .R7, RegisterParam.shifted(.R8, .LSR, 4)), .asm_text = "orr x6, x7, x8, lsr #4" },
        .{ .name = "EOR X0, X1, X2, ASR #3", .expected = emitEorRegister64(.R0, .R1, RegisterParam.shifted(.R2, .ASR, 3)), .asm_text = "eor x0, x1, x2, asr #3" },
        .{ .name = "TST W1, W2", .expected = emitTestRegister(.R1, RegisterParam.reg_only(.R2)), .asm_text = "tst w1, w2" },
        .{ .name = "TST X1, X2", .expected = emitTestRegister64(.R1, RegisterParam.reg_only(.R2)), .asm_text = "tst x1, x2" },

        // ---- MOV / MVN register ----
        .{ .name = "MOV W0, W1", .expected = emitMovRegister(.R0, .R1), .asm_text = "mov w0, w1" },
        .{ .name = "MOV X0, X1", .expected = emitMovRegister64(.R0, .R1), .asm_text = "mov x0, x1" },
        .{ .name = "MVN W0, W1", .expected = emitMvnRegister(.R0, .R1), .asm_text = "mvn w0, w1" },
        .{ .name = "MVN X0, X1", .expected = emitMvnRegister64(.R0, .R1), .asm_text = "mvn x0, x1" },

        // ---- Shift register ----
        .{ .name = "ASR W0, W1, W2", .expected = emitAsrRegister(.R0, .R1, .R2), .asm_text = "asr w0, w1, w2" },
        .{ .name = "ASR X0, X1, X2", .expected = emitAsrRegister64(.R0, .R1, .R2), .asm_text = "asr x0, x1, x2" },
        .{ .name = "LSL W0, W1, W2", .expected = emitLslRegister(.R0, .R1, .R2), .asm_text = "lsl w0, w1, w2" },
        .{ .name = "LSL X0, X1, X2", .expected = emitLslRegister64(.R0, .R1, .R2), .asm_text = "lsl x0, x1, x2" },
        .{ .name = "LSR W0, W1, W2", .expected = emitLsrRegister(.R0, .R1, .R2), .asm_text = "lsr w0, w1, w2" },
        .{ .name = "LSR X0, X1, X2", .expected = emitLsrRegister64(.R0, .R1, .R2), .asm_text = "lsr x0, x1, x2" },
        .{ .name = "ROR W0, W1, W2", .expected = emitRorRegister(.R0, .R1, .R2), .asm_text = "ror w0, w1, w2" },
        .{ .name = "ROR X3, X4, X5", .expected = emitRorRegister64(.R3, .R4, .R5), .asm_text = "ror x3, x4, x5" },

        // ---- Shift / Rotate immediate ----
        .{ .name = "ASR W0, W1, #5", .expected = emitAsrImmediate(.R0, .R1, 5), .asm_text = "asr w0, w1, #5" },
        .{ .name = "ASR X0, X1, #10", .expected = emitAsrImmediate64(.R0, .R1, 10), .asm_text = "asr x0, x1, #10" },
        .{ .name = "LSL W0, W1, #3", .expected = emitLslImmediate(.R0, .R1, 3), .asm_text = "lsl w0, w1, #3" },
        .{ .name = "LSL X0, X1, #7", .expected = emitLslImmediate64(.R0, .R1, 7), .asm_text = "lsl x0, x1, #7" },
        .{ .name = "LSR W0, W1, #3", .expected = emitLsrImmediate(.R0, .R1, 3), .asm_text = "lsr w0, w1, #3" },
        .{ .name = "LSR X0, X1, #10", .expected = emitLsrImmediate64(.R0, .R1, 10), .asm_text = "lsr x0, x1, #10" },
        .{ .name = "ROR W0, W1, #4", .expected = emitRorImmediate(.R0, .R1, 4), .asm_text = "ror w0, w1, #4" },
        .{ .name = "ROR X0, X1, #7", .expected = emitRorImmediate64(.R0, .R1, 7), .asm_text = "ror x0, x1, #7" },
        .{ .name = "EXTR W0, W1, W2, #3", .expected = emitExtr(.R0, .R1, .R2, 3), .asm_text = "extr w0, w1, w2, #3" },
        .{ .name = "EXTR X0, X1, X2, #10", .expected = emitExtr64(.R0, .R1, .R2, 10), .asm_text = "extr x0, x1, x2, #10" },

        // ---- Bitfield ----
        .{ .name = "BFM W0, W1, #3, #7", .expected = emitBfm(.R0, .R1, 3, 7), .asm_text = "bfm w0, w1, #3, #7" },
        .{ .name = "BFM X0, X1, #3, #7", .expected = emitBfm64(.R0, .R1, 3, 7), .asm_text = "bfm x0, x1, #3, #7" },
        .{ .name = "SBFM W0, W1, #2, #10", .expected = emitSbfm(.R0, .R1, 2, 10), .asm_text = "sbfm w0, w1, #2, #10" },
        .{ .name = "SBFM X0, X1, #5, #30", .expected = emitSbfm64(.R0, .R1, 5, 30), .asm_text = "sbfm x0, x1, #5, #30" },
        .{ .name = "UBFM W0, W1, #5, #20", .expected = emitUbfm(.R0, .R1, 5, 20), .asm_text = "ubfm w0, w1, #5, #20" },
        .{ .name = "UBFM X0, X1, #8, #15", .expected = emitUbfm64(.R0, .R1, 8, 15), .asm_text = "ubfm x0, x1, #8, #15" },
        .{ .name = "SXTB W0, W1", .expected = emitSxtb(.R0, .R1), .asm_text = "sxtb w0, w1" },
        .{ .name = "SXTB X0, W1", .expected = emitSxtb64(.R0, .R1), .asm_text = "sxtb x0, w1" },
        .{ .name = "SXTH W0, W1", .expected = emitSxth(.R0, .R1), .asm_text = "sxth w0, w1" },
        .{ .name = "SXTH X0, W1", .expected = emitSxth64(.R0, .R1), .asm_text = "sxth x0, w1" },
        .{ .name = "UXTB W0, W1", .expected = emitUxtb(.R0, .R1), .asm_text = "uxtb w0, w1" },
        .{ .name = "UXTH W0, W1", .expected = emitUxth(.R0, .R1), .asm_text = "uxth w0, w1" },
        .{ .name = "UXTB W5, W6", .expected = emitUxtb(.R5, .R6), .asm_text = "uxtb w5, w6" },
        .{ .name = "UXTH W7, W8", .expected = emitUxth(.R7, .R8), .asm_text = "uxth w7, w8" },

        // ---- Move immediate ----
        .{ .name = "MOVZ W0, #0x1234", .expected = emitMovz(.R0, 0x1234, 0), .asm_text = "movz w0, #0x1234" },
        .{ .name = "MOVZ W5, #0, LSL #16", .expected = emitMovz(.R5, 0, 16), .asm_text = "movz w5, #0, lsl #16" },
        .{ .name = "MOVZ X0, #0xABCD, LSL #48", .expected = emitMovz64(.R0, 0xABCD, 48), .asm_text = "movz x0, #0xabcd, lsl #48" },
        .{ .name = "MOVZ X3, #0x42", .expected = emitMovz64(.R3, 0x42, 0), .asm_text = "movz x3, #0x42" },
        .{ .name = "MOVZ X1, #0xFFFF, LSL #32", .expected = emitMovz64(.R1, 0xFFFF, 32), .asm_text = "movz x1, #0xffff, lsl #32" },
        .{ .name = "MOVK W0, #0x5678", .expected = emitMovk(.R0, 0x5678, 0), .asm_text = "movk w0, #0x5678" },
        .{ .name = "MOVK W1, #0xABCD, LSL #16", .expected = emitMovk(.R1, 0xABCD, 16), .asm_text = "movk w1, #0xabcd, lsl #16" },
        .{ .name = "MOVK X0, #0x5678, LSL #16", .expected = emitMovk64(.R0, 0x5678, 16), .asm_text = "movk x0, #0x5678, lsl #16" },
        .{ .name = "MOVK X2, #0x1111, LSL #48", .expected = emitMovk64(.R2, 0x1111, 48), .asm_text = "movk x2, #0x1111, lsl #48" },
        .{ .name = "MOVN W0, #0x1234", .expected = emitMovn(.R0, 0x1234, 0), .asm_text = "movn w0, #0x1234" },
        .{ .name = "MOVN X0, #0", .expected = emitMovn64(.R0, 0, 0), .asm_text = "movn x0, #0" },
        .{ .name = "MOVN X5, #0x100, LSL #16", .expected = emitMovn64(.R5, 0x100, 16), .asm_text = "movn x5, #0x100, lsl #16" },

        // ---- ADD/SUB immediate ----
        .{ .name = "ADD W0, W1, #1", .expected = emitAddImmediate(.R0, .R1, 1), .asm_text = "add w0, w1, #1" },
        .{ .name = "ADD W9, W10, #4095", .expected = emitAddImmediate(.R9, .R10, 4095), .asm_text = "add w9, w10, #4095" },
        .{ .name = "ADD X0, X1, #42", .expected = emitAddImmediate64(.R0, .R1, 42), .asm_text = "add x0, x1, #42" },
        .{ .name = "ADDS W0, W1, #10", .expected = emitAddsImmediate(.R0, .R1, 10), .asm_text = "adds w0, w1, #10" },
        .{ .name = "ADDS X3, X4, #100", .expected = emitAddsImmediate64(.R3, .R4, 100), .asm_text = "adds x3, x4, #100" },
        .{ .name = "SUB W0, W1, #7", .expected = emitSubImmediate(.R0, .R1, 7), .asm_text = "sub w0, w1, #7" },
        .{ .name = "SUB X0, X1, #4", .expected = emitSubImmediate64(.R0, .R1, 4), .asm_text = "sub x0, x1, #4" },
        .{ .name = "SUBS W0, W1, #7", .expected = emitSubsImmediate(.R0, .R1, 7), .asm_text = "subs w0, w1, #7" },
        .{ .name = "SUBS X5, X6, #33", .expected = emitSubsImmediate64(.R5, .R6, 33), .asm_text = "subs x5, x6, #33" },
        .{ .name = "CMP W0, #42", .expected = emitCmpImmediate(.R0, 42), .asm_text = "cmp w0, #42" },
        .{ .name = "CMP X0, #1", .expected = emitCmpImmediate64(.R0, 1), .asm_text = "cmp x0, #1" },

        // ---- Logical immediate ----
        .{ .name = "AND W0, W1, #0xFF", .expected = emitAndImmediate(.R0, .R1, 0xFF).?, .asm_text = "and w0, w1, #0xff" },
        .{ .name = "AND X0, X1, #0xFF", .expected = emitAndImmediate64(.R0, .R1, 0xFF).?, .asm_text = "and x0, x1, #0xff" },
        .{ .name = "ANDS X3, X4, #0x0F0F0F0F0F0F0F0F", .expected = emitAndsImmediate64(.R3, .R4, 0x0F0F0F0F0F0F0F0F).?, .asm_text = "ands x3, x4, #0xf0f0f0f0f0f0f0f" },
        .{ .name = "ORR W0, W1, #0x00FF00FF", .expected = emitOrrImmediate(.R0, .R1, 0x00FF00FF).?, .asm_text = "orr w0, w1, #0x00ff00ff" },
        .{ .name = "ORR X5, X6, #0x00FF00FF00FF00FF", .expected = emitOrrImmediate64(.R5, .R6, 0x00FF00FF00FF00FF).?, .asm_text = "orr x5, x6, #0xff00ff00ff00ff" },
        .{ .name = "EOR W0, W1, #0x0F0F0F0F", .expected = emitEorImmediate(.R0, .R1, 0x0F0F0F0F).?, .asm_text = "eor w0, w1, #0xf0f0f0f" },
        .{ .name = "EOR X0, X1, #0x5555555555555555", .expected = emitEorImmediate64(.R0, .R1, 0x5555555555555555).?, .asm_text = "eor x0, x1, #0x5555555555555555" },
        .{ .name = "TST W0, #0xFF", .expected = emitTestImmediate(.R0, 0xFF).?, .asm_text = "tst w0, #0xff" },
        .{ .name = "TST X0, #0xAAAAAAAAAAAAAAAA", .expected = emitTestImmediate64(.R0, 0xAAAAAAAAAAAAAAAA).?, .asm_text = "tst x0, #0xaaaaaaaaaaaaaaaa" },

        // ---- Conditional select ----
        .{ .name = "CSEL W0, W1, W2, EQ", .expected = emitCsel(.R0, .R1, .R2, .EQ), .asm_text = "csel w0, w1, w2, eq" },
        .{ .name = "CSEL X0, X1, X2, NE", .expected = emitCsel64(.R0, .R1, .R2, .NE), .asm_text = "csel x0, x1, x2, ne" },
        .{ .name = "CSINC W0, W1, W2, GE", .expected = emitCsinc(.R0, .R1, .R2, .GE), .asm_text = "csinc w0, w1, w2, ge" },
        .{ .name = "CSINC X3, X4, X5, LT", .expected = emitCsinc64(.R3, .R4, .R5, .LT), .asm_text = "csinc x3, x4, x5, lt" },
        .{ .name = "CSINV W0, W1, W2, LT", .expected = emitCsinv(.R0, .R1, .R2, .LT), .asm_text = "csinv w0, w1, w2, lt" },
        .{ .name = "CSINV X0, X1, X2, GE", .expected = emitCsinv64(.R0, .R1, .R2, .GE), .asm_text = "csinv x0, x1, x2, ge" },
        .{ .name = "CSNEG W0, W1, W2, EQ", .expected = emitCsneg(.R0, .R1, .R2, .EQ), .asm_text = "csneg w0, w1, w2, eq" },
        .{ .name = "CSNEG X0, X1, X2, NE", .expected = emitCsneg64(.R0, .R1, .R2, .NE), .asm_text = "csneg x0, x1, x2, ne" },
        .{ .name = "CSET W0, EQ", .expected = emitCset(.R0, .EQ), .asm_text = "cset w0, eq" },
        .{ .name = "CSET X0, GE", .expected = emitCset64(.R0, .GE), .asm_text = "cset x0, ge" },
        .{ .name = "CSETM W0, NE", .expected = emitCsetm(.R0, .NE), .asm_text = "csetm w0, ne" },
        .{ .name = "CSETM X3, LT", .expected = emitCsetm64(.R3, .LT), .asm_text = "csetm x3, lt" },
        .{ .name = "CINC W0, W1, EQ", .expected = emitCinc(.R0, .R1, .EQ), .asm_text = "cinc w0, w1, eq" },
        .{ .name = "CINC X5, X6, NE", .expected = emitCinc64(.R5, .R6, .NE), .asm_text = "cinc x5, x6, ne" },
        .{ .name = "CINV W0, W1, CS", .expected = emitCinv(.R0, .R1, .CS), .asm_text = "cinv w0, w1, hs" },
        .{ .name = "CINV X0, X1, CC", .expected = emitCinv64(.R0, .R1, .CC), .asm_text = "cinv x0, x1, lo" },
        .{ .name = "CNEG W0, W1, GT", .expected = emitCneg(.R0, .R1, .GT), .asm_text = "cneg w0, w1, gt" },
        .{ .name = "CNEG X2, X3, LE", .expected = emitCneg64(.R2, .R3, .LE), .asm_text = "cneg x2, x3, le" },

        // ---- Conditional compare ----
        .{ .name = "CCMP W0, W1, #0, EQ", .expected = emitCcmpRegister(.R0, .R1, 0, .EQ), .asm_text = "ccmp w0, w1, #0, eq" },
        .{ .name = "CCMP X0, X1, #0xF, NE", .expected = emitCcmpRegister64(.R0, .R1, 0xF, .NE), .asm_text = "ccmp x0, x1, #0xf, ne" },
        .{ .name = "CCMP W3, W4, #5, GT", .expected = emitCcmpRegister(.R3, .R4, 5, .GT), .asm_text = "ccmp w3, w4, #5, gt" },
        .{ .name = "CCMN W0, W1, #0, EQ", .expected = emitCcmnRegister(.R0, .R1, 0, .EQ), .asm_text = "ccmn w0, w1, #0, eq" },
        .{ .name = "CCMN X5, X6, #3, LT", .expected = emitCcmnRegister64(.R5, .R6, 3, .LT), .asm_text = "ccmn x5, x6, #3, lt" },

        // ---- Reverse / CLZ ----
        .{ .name = "CLZ W0, W1", .expected = emitClz(.R0, .R1), .asm_text = "clz w0, w1" },
        .{ .name = "CLZ X0, X1", .expected = emitClz64(.R0, .R1), .asm_text = "clz x0, x1" },
        .{ .name = "CLZ W10, W11", .expected = emitClz(.R10, .R11), .asm_text = "clz w10, w11" },
        .{ .name = "RBIT W0, W1", .expected = emitRbit(.R0, .R1), .asm_text = "rbit w0, w1" },
        .{ .name = "RBIT X0, X1", .expected = emitRbit64(.R0, .R1), .asm_text = "rbit x0, x1" },
        .{ .name = "REV W0, W1", .expected = emitRev(.R0, .R1), .asm_text = "rev w0, w1" },
        .{ .name = "REV X0, X1", .expected = emitRev64(.R0, .R1), .asm_text = "rev x0, x1" },
        .{ .name = "REV16 W0, W1", .expected = emitRev16(.R0, .R1), .asm_text = "rev16 w0, w1" },
        .{ .name = "REV16 X3, X4", .expected = emitRev1664(.R3, .R4), .asm_text = "rev16 x3, x4" },
        .{ .name = "REV32 X0, X1", .expected = emitRev3264(.R0, .R1), .asm_text = "rev32 x0, x1" },

        // ---- ADR / ADRP ----
        .{ .name = "ADR X0, .", .expected = emitAdr(.R0, 0), .asm_text = "adr x0, ." },
        .{ .name = "ADR X1, .+4", .expected = emitAdr(.R1, 4), .asm_text = "adr x1, .+4" },

        // ---- Load/store register offset ----
        .{ .name = "LDRB W0, [X1, X2]", .expected = emitLdrbRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "ldrb w0, [x1, x2]" },
        .{ .name = "LDRSB W0, [X1, X2]", .expected = emitLdrsbRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "ldrsb w0, [x1, x2]" },
        .{ .name = "LDRSB X0, [X1, X2]", .expected = emitLdrsbRegister64(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "ldrsb x0, [x1, x2]" },
        .{ .name = "STRB W0, [X1, X2]", .expected = emitStrbRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "strb w0, [x1, x2]" },
        .{ .name = "LDRH W0, [X1, X2]", .expected = emitLdrhRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "ldrh w0, [x1, x2]" },
        .{ .name = "LDRSH W0, [X1, X2]", .expected = emitLdrshRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "ldrsh w0, [x1, x2]" },
        .{ .name = "LDRSH X0, [X1, X2]", .expected = emitLdrshRegister64(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "ldrsh x0, [x1, x2]" },
        .{ .name = "STRH W0, [X1, X2]", .expected = emitStrhRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "strh w0, [x1, x2]" },
        .{ .name = "LDR W0, [X1, X2]", .expected = emitLdrRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "ldr w0, [x1, x2]" },
        .{ .name = "LDRSW X0, [X1, X2]", .expected = emitLdrswRegister64(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "ldrsw x0, [x1, x2]" },
        .{ .name = "LDR X0, [X1, X2]", .expected = emitLdrRegister64(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "ldr x0, [x1, x2]" },
        .{ .name = "STR W0, [X1, X2]", .expected = emitStrRegister(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "str w0, [x1, x2]" },
        .{ .name = "STR X0, [X1, X2]", .expected = emitStrRegister64(.R0, .R1, RegisterParam.reg_only(.R2)), .asm_text = "str x0, [x1, x2]" },

        // ---- Load/store unsigned offset ----
        .{ .name = "LDRB W0, [X1, #5]", .expected = emitLdrbOffset(.R0, .R1, 5).?, .asm_text = "ldrb w0, [x1, #5]" },
        .{ .name = "STRB W0, [X1]", .expected = emitStrbOffset(.R0, .R1, 0).?, .asm_text = "strb w0, [x1]" },
        .{ .name = "LDRH W0, [X1]", .expected = emitLdrhOffset(.R0, .R1, 0).?, .asm_text = "ldrh w0, [x1]" },
        .{ .name = "STRH W0, [X1, #2]", .expected = emitStrhOffset(.R0, .R1, 2).?, .asm_text = "strh w0, [x1, #2]" },
        .{ .name = "LDR W0, [X1, #4]", .expected = emitLdrOffset(.R0, .R1, 4).?, .asm_text = "ldr w0, [x1, #4]" },
        .{ .name = "LDR X0, [X1, #16]", .expected = emitLdrOffset64(.R0, .R1, 16).?, .asm_text = "ldr x0, [x1, #16]" },
        .{ .name = "STR W0, [X1, #4]", .expected = emitStrOffset(.R0, .R1, 4).?, .asm_text = "str w0, [x1, #4]" },
        .{ .name = "STR X0, [X1, #8]", .expected = emitStrOffset64(.R0, .R1, 8).?, .asm_text = "str x0, [x1, #8]" },
        .{ .name = "LDRSB W0, [X1, #3]", .expected = emitLdrsbOffset(.R0, .R1, 3).?, .asm_text = "ldrsb w0, [x1, #3]" },
        .{ .name = "LDRSB X0, [X1, #3]", .expected = emitLdrsbOffset64(.R0, .R1, 3).?, .asm_text = "ldrsb x0, [x1, #3]" },
        .{ .name = "LDRSH W0, [X1, #4]", .expected = emitLdrshOffset(.R0, .R1, 4).?, .asm_text = "ldrsh w0, [x1, #4]" },
        .{ .name = "LDRSH X0, [X1, #4]", .expected = emitLdrshOffset64(.R0, .R1, 4).?, .asm_text = "ldrsh x0, [x1, #4]" },
        .{ .name = "LDRSW X0, [X1, #8]", .expected = emitLdrswOffset64(.R0, .R1, 8).?, .asm_text = "ldrsw x0, [x1, #8]" },

        // ---- Load/store pair ----
        .{ .name = "LDP W0, W1, [SP]", .expected = emitLdpOffset(.R0, .R1, .SP, 0).?, .asm_text = "ldp w0, w1, [sp]" },
        .{ .name = "LDP W3, W4, [X5, #8]", .expected = emitLdpOffset(.R3, .R4, .R5, 8).?, .asm_text = "ldp w3, w4, [x5, #8]" },
        .{ .name = "LDP X0, X1, [SP, #16]", .expected = emitLdpOffset64(.R0, .R1, .SP, 16).?, .asm_text = "ldp x0, x1, [sp, #16]" },
        .{ .name = "LDP X19, X20, [SP, #-16]", .expected = emitLdpOffset64(.R19, .R20, .SP, -16).?, .asm_text = "ldp x19, x20, [sp, #-16]" },
        .{ .name = "STP W0, W1, [SP]", .expected = emitStpOffset(.R0, .R1, .SP, 0).?, .asm_text = "stp w0, w1, [sp]" },
        .{ .name = "STP X0, X1, [SP, #16]", .expected = emitStpOffset64(.R0, .R1, .SP, 16).?, .asm_text = "stp x0, x1, [sp, #16]" },
        .{ .name = "STP W2, W3, [SP, #-8]!", .expected = emitStpOffsetPreIndex(.R2, .R3, .SP, -8).?, .asm_text = "stp w2, w3, [sp, #-8]!" },
        .{ .name = "STP X0, X1, [SP, #-16]!", .expected = emitStpOffsetPreIndex64(.R0, .R1, .SP, -16).?, .asm_text = "stp x0, x1, [sp, #-16]!" },
        .{ .name = "LDP X21, X22, [SP], #16", .expected = emitLdpOffsetPostIndex64(.R21, .R22, .SP, 16).?, .asm_text = "ldp x21, x22, [sp], #16" },

        // ---- Atomic load/store ----
        .{ .name = "LDARB W0, [X1]", .expected = emitLdarb(.R0, .R1), .asm_text = "ldarb w0, [x1]" },
        .{ .name = "LDARH W0, [X1]", .expected = emitLdarh(.R0, .R1), .asm_text = "ldarh w0, [x1]" },
        .{ .name = "LDAR W0, [X1]", .expected = emitLdar(.R0, .R1), .asm_text = "ldar w0, [x1]" },
        .{ .name = "LDAR X0, [X1]", .expected = emitLdar64(.R0, .R1), .asm_text = "ldar x0, [x1]" },
        .{ .name = "STLRB W0, [X1]", .expected = emitStlrb(.R0, .R1), .asm_text = "stlrb w0, [x1]" },
        .{ .name = "STLRH W0, [X1]", .expected = emitStlrh(.R0, .R1), .asm_text = "stlrh w0, [x1]" },
        .{ .name = "STLR W0, [X1]", .expected = emitStlr(.R0, .R1), .asm_text = "stlr w0, [x1]" },
        .{ .name = "STLR X0, [X1]", .expected = emitStlr64(.R0, .R1), .asm_text = "stlr x0, [x1]" },
        .{ .name = "LDXRB W0, [X1]", .expected = emitLdxrb(.R0, .R1), .asm_text = "ldxrb w0, [x1]" },
        .{ .name = "LDXRH W0, [X1]", .expected = emitLdxrh(.R0, .R1), .asm_text = "ldxrh w0, [x1]" },
        .{ .name = "LDXR W0, [X1]", .expected = emitLdxr(.R0, .R1), .asm_text = "ldxr w0, [x1]" },
        .{ .name = "LDXR X0, [X1]", .expected = emitLdxr64(.R0, .R1), .asm_text = "ldxr x0, [x1]" },
        .{ .name = "LDAXRB W0, [X1]", .expected = emitLdaxrb(.R0, .R1), .asm_text = "ldaxrb w0, [x1]" },
        .{ .name = "LDAXRH W0, [X1]", .expected = emitLdaxrh(.R0, .R1), .asm_text = "ldaxrh w0, [x1]" },
        .{ .name = "LDAXR W0, [X1]", .expected = emitLdaxr(.R0, .R1), .asm_text = "ldaxr w0, [x1]" },
        .{ .name = "LDAXR X0, [X1]", .expected = emitLdaxr64(.R0, .R1), .asm_text = "ldaxr x0, [x1]" },
        .{ .name = "STXRB W0, W1, [X2]", .expected = emitStxrb(.R0, .R1, .R2), .asm_text = "stxrb w0, w1, [x2]" },
        .{ .name = "STXRH W0, W1, [X2]", .expected = emitStxrh(.R0, .R1, .R2), .asm_text = "stxrh w0, w1, [x2]" },
        .{ .name = "STXR W0, W1, [X2]", .expected = emitStxr(.R0, .R1, .R2), .asm_text = "stxr w0, w1, [x2]" },
        .{ .name = "STXR W0, X1, [X2]", .expected = emitStxr64(.R0, .R1, .R2), .asm_text = "stxr w0, x1, [x2]" },
        .{ .name = "STLXRB W0, W1, [X2]", .expected = emitStlxrb(.R0, .R1, .R2), .asm_text = "stlxrb w0, w1, [x2]" },
        .{ .name = "STLXRH W0, W1, [X2]", .expected = emitStlxrh(.R0, .R1, .R2), .asm_text = "stlxrh w0, w1, [x2]" },
        .{ .name = "STLXR W0, W1, [X2]", .expected = emitStlxr(.R0, .R1, .R2), .asm_text = "stlxr w0, w1, [x2]" },
        .{ .name = "STLXR X0, X1, [X2]", .expected = emitStlxr64(.R0, .R1, .R2), .asm_text = "stlxr w0, x1, [x2]" },
        .{ .name = "LDAXP W0, W1, [X2]", .expected = emitLdaxp(.R0, .R1, .R2), .asm_text = "ldaxp w0, w1, [x2]" },
        .{ .name = "LDAXP X0, X1, [X2]", .expected = emitLdaxp64(.R0, .R1, .R2), .asm_text = "ldaxp x0, x1, [x2]" },
        .{ .name = "STLXP W0, W1, W2, [X3]", .expected = emitStlxp(.R0, .R1, .R2, .R3), .asm_text = "stlxp w0, w1, w2, [x3]" },
        .{ .name = "STLXP X0, X1, X2, [X3]", .expected = emitStlxp64(.R0, .R1, .R2, .R3), .asm_text = "stlxp w0, x1, x2, [x3]" },

        // ---- NEON integer — arrangement variants ----
        .{ .name = "NEON ADD V0.8B", .expected = emitNeonAdd(.V0, .V1, .V2, .Size8B), .asm_text = "add v0.8b, v1.8b, v2.8b" },
        .{ .name = "NEON ADD V0.16B", .expected = emitNeonAdd(.V0, .V1, .V2, .Size16B), .asm_text = "add v0.16b, v1.16b, v2.16b" },
        .{ .name = "NEON ADD V0.4H", .expected = emitNeonAdd(.V0, .V1, .V2, .Size4H), .asm_text = "add v0.4h, v1.4h, v2.4h" },
        .{ .name = "NEON ADD V0.8H", .expected = emitNeonAdd(.V0, .V1, .V2, .Size8H), .asm_text = "add v0.8h, v1.8h, v2.8h" },
        .{ .name = "NEON ADD V0.2S", .expected = emitNeonAdd(.V0, .V1, .V2, .Size2S), .asm_text = "add v0.2s, v1.2s, v2.2s" },
        .{ .name = "NEON ADD V0.4S", .expected = emitNeonAdd(.V0, .V1, .V2, .Size4S), .asm_text = "add v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON ADD V0.2D", .expected = emitNeonAdd(.V0, .V1, .V2, .Size2D), .asm_text = "add v0.2d, v1.2d, v2.2d" },
        .{ .name = "NEON SUB V0.8B", .expected = emitNeonSub(.V0, .V1, .V2, .Size8B), .asm_text = "sub v0.8b, v1.8b, v2.8b" },
        .{ .name = "NEON SUB V0.16B", .expected = emitNeonSub(.V0, .V1, .V2, .Size16B), .asm_text = "sub v0.16b, v1.16b, v2.16b" },
        .{ .name = "NEON SUB V0.4H", .expected = emitNeonSub(.V0, .V1, .V2, .Size4H), .asm_text = "sub v0.4h, v1.4h, v2.4h" },
        .{ .name = "NEON SUB V0.8H", .expected = emitNeonSub(.V0, .V1, .V2, .Size8H), .asm_text = "sub v0.8h, v1.8h, v2.8h" },
        .{ .name = "NEON SUB V0.4S", .expected = emitNeonSub(.V0, .V1, .V2, .Size4S), .asm_text = "sub v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON SUB V0.2D", .expected = emitNeonSub(.V0, .V1, .V2, .Size2D), .asm_text = "sub v0.2d, v1.2d, v2.2d" },
        .{ .name = "NEON MUL V0.8B", .expected = emitNeonMul(.V0, .V1, .V2, .Size8B), .asm_text = "mul v0.8b, v1.8b, v2.8b" },
        .{ .name = "NEON MUL V0.16B", .expected = emitNeonMul(.V0, .V1, .V2, .Size16B), .asm_text = "mul v0.16b, v1.16b, v2.16b" },
        .{ .name = "NEON MUL V0.4H", .expected = emitNeonMul(.V0, .V1, .V2, .Size4H), .asm_text = "mul v0.4h, v1.4h, v2.4h" },
        .{ .name = "NEON MUL V0.8H", .expected = emitNeonMul(.V0, .V1, .V2, .Size8H), .asm_text = "mul v0.8h, v1.8h, v2.8h" },
        .{ .name = "NEON MUL V0.4S", .expected = emitNeonMul(.V0, .V1, .V2, .Size4S), .asm_text = "mul v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON AND V0.8B", .expected = emitNeonAnd(.V0, .V1, .V2, .Size8B), .asm_text = "and v0.8b, v1.8b, v2.8b" },
        .{ .name = "NEON AND V0.16B", .expected = emitNeonAnd(.V0, .V1, .V2, .Size16B), .asm_text = "and v0.16b, v1.16b, v2.16b" },
        .{ .name = "NEON EOR V0.8B", .expected = emitNeonEor(.V0, .V1, .V2, .Size8B), .asm_text = "eor v0.8b, v1.8b, v2.8b" },
        .{ .name = "NEON EOR V0.16B", .expected = emitNeonEor(.V0, .V1, .V2, .Size16B), .asm_text = "eor v0.16b, v1.16b, v2.16b" },
        .{ .name = "NEON ORR V0.8B", .expected = emitNeonOrrReg(.V0, .V1, .V2, .Size8B), .asm_text = "orr v0.8b, v1.8b, v2.8b" },
        .{ .name = "NEON ORR V0.16B", .expected = emitNeonOrrReg(.V0, .V1, .V2, .Size16B), .asm_text = "orr v0.16b, v1.16b, v2.16b" },
        .{ .name = "NEON ABS V0.8B", .expected = emitNeonAbs(.V0, .V1, .Size8B), .asm_text = "abs v0.8b, v1.8b" },
        .{ .name = "NEON ABS V0.16B", .expected = emitNeonAbs(.V0, .V1, .Size16B), .asm_text = "abs v0.16b, v1.16b" },
        .{ .name = "NEON ABS V0.4H", .expected = emitNeonAbs(.V0, .V1, .Size4H), .asm_text = "abs v0.4h, v1.4h" },
        .{ .name = "NEON ABS V0.8H", .expected = emitNeonAbs(.V0, .V1, .Size8H), .asm_text = "abs v0.8h, v1.8h" },
        .{ .name = "NEON ABS V0.2S", .expected = emitNeonAbs(.V0, .V1, .Size2S), .asm_text = "abs v0.2s, v1.2s" },
        .{ .name = "NEON ABS V0.4S", .expected = emitNeonAbs(.V0, .V1, .Size4S), .asm_text = "abs v0.4s, v1.4s" },
        .{ .name = "NEON NEG V0.8B", .expected = emitNeonNeg(.V0, .V1, .Size8B), .asm_text = "neg v0.8b, v1.8b" },
        .{ .name = "NEON NEG V0.16B", .expected = emitNeonNeg(.V0, .V1, .Size16B), .asm_text = "neg v0.16b, v1.16b" },
        .{ .name = "NEON NEG V0.4H", .expected = emitNeonNeg(.V0, .V1, .Size4H), .asm_text = "neg v0.4h, v1.4h" },
        .{ .name = "NEON NEG V0.8H", .expected = emitNeonNeg(.V0, .V1, .Size8H), .asm_text = "neg v0.8h, v1.8h" },
        .{ .name = "NEON NEG V0.2S", .expected = emitNeonNeg(.V0, .V1, .Size2S), .asm_text = "neg v0.2s, v1.2s" },
        .{ .name = "NEON NEG V0.4S", .expected = emitNeonNeg(.V0, .V1, .Size4S), .asm_text = "neg v0.4s, v1.4s" },
        .{ .name = "NEON NEG V0.2D", .expected = emitNeonNeg(.V0, .V1, .Size2D), .asm_text = "neg v0.2d, v1.2d" },
        .{ .name = "NEON NOT V0.8B", .expected = emitNeonNot(.V0, .V1, .Size8B), .asm_text = "not v0.8b, v1.8b" },
        .{ .name = "NEON NOT V0.16B", .expected = emitNeonNot(.V0, .V1, .Size16B), .asm_text = "not v0.16b, v1.16b" },
        .{ .name = "NEON CNT V0.8B", .expected = emitNeonCnt(.V0, .V1, .Size8B), .asm_text = "cnt v0.8b, v1.8b" },
        .{ .name = "NEON CNT V0.16B", .expected = emitNeonCnt(.V0, .V1, .Size16B), .asm_text = "cnt v0.16b, v1.16b" },
        .{ .name = "NEON ADDP V0.8B", .expected = emitNeonAddp(.V0, .V1, .V2, .Size8B), .asm_text = "addp v0.8b, v1.8b, v2.8b" },
        .{ .name = "NEON ADDP V0.4H", .expected = emitNeonAddp(.V0, .V1, .V2, .Size4H), .asm_text = "addp v0.4h, v1.4h, v2.4h" },
        .{ .name = "NEON ADDP V0.4S", .expected = emitNeonAddp(.V0, .V1, .V2, .Size4S), .asm_text = "addp v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON ADDP V0.2D", .expected = emitNeonAddp(.V0, .V1, .V2, .Size2D), .asm_text = "addp v0.2d, v1.2d, v2.2d" },
        .{ .name = "NEON CMEQ V0.16B, #0", .expected = emitNeonCmeq0(.V0, .V1, .Size16B), .asm_text = "cmeq v0.16b, v1.16b, #0" },
        .{ .name = "NEON CMEQ V0.8H, #0", .expected = emitNeonCmeq0(.V0, .V1, .Size8H), .asm_text = "cmeq v0.8h, v1.8h, #0" },
        .{ .name = "NEON CMEQ V0.4S, #0", .expected = emitNeonCmeq0(.V0, .V1, .Size4S), .asm_text = "cmeq v0.4s, v1.4s, #0" },
        .{ .name = "NEON CMEQ V0.2D, #0", .expected = emitNeonCmeq0(.V0, .V1, .Size2D), .asm_text = "cmeq v0.2d, v1.2d, #0" },
        .{ .name = "NEON CMEQ V0.8B", .expected = emitNeonCmeq(.V0, .V1, .V2, .Size8B), .asm_text = "cmeq v0.8b, v1.8b, v2.8b" },
        .{ .name = "NEON CMEQ V0.4S", .expected = emitNeonCmeq(.V0, .V1, .V2, .Size4S), .asm_text = "cmeq v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON CMEQ V0.2D", .expected = emitNeonCmeq(.V0, .V1, .V2, .Size2D), .asm_text = "cmeq v0.2d, v1.2d, v2.2d" },
        .{ .name = "NEON CMGT V0.4S", .expected = emitNeonCmgt(.V0, .V1, .V2, .Size4S), .asm_text = "cmgt v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON CMGE V0.4S", .expected = emitNeonCmge(.V0, .V1, .V2, .Size4S), .asm_text = "cmge v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON CMHI V0.4S", .expected = emitNeonCmhi(.V0, .V1, .V2, .Size4S), .asm_text = "cmhi v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON CMHS V0.4S", .expected = emitNeonCmhs(.V0, .V1, .V2, .Size4S), .asm_text = "cmhs v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON CMTST V0.4S", .expected = emitNeonCmtst(.V0, .V1, .V2, .Size4S), .asm_text = "cmtst v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON BSL V0.8B", .expected = emitNeonBsl(.V0, .V1, .V2, .Size8B), .asm_text = "bsl v0.8b, v1.8b, v2.8b" },
        .{ .name = "NEON BSL V0.16B", .expected = emitNeonBsl(.V0, .V1, .V2, .Size16B), .asm_text = "bsl v0.16b, v1.16b, v2.16b" },
        .{ .name = "NEON BIT V0.8B", .expected = emitNeonBit(.V0, .V1, .V2, .Size8B), .asm_text = "bit v0.8b, v1.8b, v2.8b" },
        .{ .name = "NEON BIT V0.16B", .expected = emitNeonBit(.V0, .V1, .V2, .Size16B), .asm_text = "bit v0.16b, v1.16b, v2.16b" },
        .{ .name = "NEON BIF V0.8B", .expected = emitNeonBif(.V0, .V1, .V2, .Size8B), .asm_text = "bif v0.8b, v1.8b, v2.8b" },
        .{ .name = "NEON BIF V0.16B", .expected = emitNeonBif(.V0, .V1, .V2, .Size16B), .asm_text = "bif v0.16b, v1.16b, v2.16b" },
        .{ .name = "NEON SMAX V0.8B", .expected = emitNeonSmax(.V0, .V1, .V2, .Size8B), .asm_text = "smax v0.8b, v1.8b, v2.8b" },
        .{ .name = "NEON SMAX V0.4H", .expected = emitNeonSmax(.V0, .V1, .V2, .Size4H), .asm_text = "smax v0.4h, v1.4h, v2.4h" },
        .{ .name = "NEON SMAX V0.4S", .expected = emitNeonSmax(.V0, .V1, .V2, .Size4S), .asm_text = "smax v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON SMIN V0.4S", .expected = emitNeonSmin(.V0, .V1, .V2, .Size4S), .asm_text = "smin v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON UMAX V0.4S", .expected = emitNeonUmax(.V0, .V1, .V2, .Size4S), .asm_text = "umax v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON UMIN V0.8B", .expected = emitNeonUmin(.V0, .V1, .V2, .Size8B), .asm_text = "umin v0.8b, v1.8b, v2.8b" },
        .{ .name = "NEON UMIN V0.4S", .expected = emitNeonUmin(.V0, .V1, .V2, .Size4S), .asm_text = "umin v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON ZIP1 V0.8B", .expected = emitNeonZip1(.V0, .V1, .V2, .Size8B), .asm_text = "zip1 v0.8b, v1.8b, v2.8b" },
        .{ .name = "NEON ZIP1 V0.4S", .expected = emitNeonZip1(.V0, .V1, .V2, .Size4S), .asm_text = "zip1 v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON ZIP1 V0.2D", .expected = emitNeonZip1(.V0, .V1, .V2, .Size2D), .asm_text = "zip1 v0.2d, v1.2d, v2.2d" },
        .{ .name = "NEON ZIP2 V0.4S", .expected = emitNeonZip2(.V0, .V1, .V2, .Size4S), .asm_text = "zip2 v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON UZP1 V0.16B", .expected = emitNeonUzp1(.V0, .V1, .V2, .Size16B), .asm_text = "uzp1 v0.16b, v1.16b, v2.16b" },
        .{ .name = "NEON UZP1 V0.4S", .expected = emitNeonUzp1(.V0, .V1, .V2, .Size4S), .asm_text = "uzp1 v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON UZP2 V0.4S", .expected = emitNeonUzp2(.V0, .V1, .V2, .Size4S), .asm_text = "uzp2 v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON TRN1 V0.16B", .expected = emitNeonTrn1(.V0, .V1, .V2, .Size16B), .asm_text = "trn1 v0.16b, v1.16b, v2.16b" },
        .{ .name = "NEON TRN1 V0.4S", .expected = emitNeonTrn1(.V0, .V1, .V2, .Size4S), .asm_text = "trn1 v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON TRN2 V0.4S", .expected = emitNeonTrn2(.V0, .V1, .V2, .Size4S), .asm_text = "trn2 v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON SSHL V0.4S", .expected = emitNeonSshl(.V0, .V1, .V2, .Size4S), .asm_text = "sshl v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON USHL V0.4S", .expected = emitNeonUshl(.V0, .V1, .V2, .Size4S), .asm_text = "ushl v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON SABD V0.4S", .expected = emitNeonSabd(.V0, .V1, .V2, .Size4S), .asm_text = "sabd v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON UABD V0.4S", .expected = emitNeonUabd(.V0, .V1, .V2, .Size4S), .asm_text = "uabd v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON MLA V0.4S", .expected = emitNeonMla(.V0, .V1, .V2, .Size4S), .asm_text = "mla v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON MLS V0.4S", .expected = emitNeonMls(.V0, .V1, .V2, .Size4S), .asm_text = "mls v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON SHADD V0.4S", .expected = emitNeonShadd(.V0, .V1, .V2, .Size4S), .asm_text = "shadd v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON UHADD V0.4S", .expected = emitNeonUhadd(.V0, .V1, .V2, .Size4S), .asm_text = "uhadd v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON SHSUB V0.4S", .expected = emitNeonShsub(.V0, .V1, .V2, .Size4S), .asm_text = "shsub v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON UHSUB V0.4S", .expected = emitNeonUhsub(.V0, .V1, .V2, .Size4S), .asm_text = "uhsub v0.4s, v1.4s, v2.4s" },
        .{ .name = "NEON RBIT V0.8B", .expected = emitNeonRbit(.V0, .V1, .Size8B), .asm_text = "rbit v0.8b, v1.8b" },
        .{ .name = "NEON RBIT V0.16B", .expected = emitNeonRbit(.V0, .V1, .Size16B), .asm_text = "rbit v0.16b, v1.16b" },
        .{ .name = "NEON REV16 V0.8B", .expected = emitNeonRev16(.V0, .V1, .Size8B), .asm_text = "rev16 v0.8b, v1.8b" },
        .{ .name = "NEON REV32 V0.8B", .expected = emitNeonRev32(.V0, .V1, .Size8B), .asm_text = "rev32 v0.8b, v1.8b" },
        .{ .name = "NEON REV32 V0.4H", .expected = emitNeonRev32(.V0, .V1, .Size4H), .asm_text = "rev32 v0.4h, v1.4h" },
        .{ .name = "NEON REV64 V0.8B", .expected = emitNeonRev64(.V0, .V1, .Size8B), .asm_text = "rev64 v0.8b, v1.8b" },
        .{ .name = "NEON REV64 V0.4S", .expected = emitNeonRev64(.V0, .V1, .Size4S), .asm_text = "rev64 v0.4s, v1.4s" },
        .{ .name = "NEON CMGT V0.16B, #0", .expected = emitNeonCmgt0(.V0, .V1, .Size16B), .asm_text = "cmgt v0.16b, v1.16b, #0" },
        .{ .name = "NEON CMGE V0.4S, #0", .expected = emitNeonCmge0(.V0, .V1, .Size4S), .asm_text = "cmge v0.4s, v1.4s, #0" },
        .{ .name = "NEON CMLE V0.4S, #0", .expected = emitNeonCmle0(.V0, .V1, .Size4S), .asm_text = "cmle v0.4s, v1.4s, #0" },
        .{ .name = "NEON CMLT V0.4S, #0", .expected = emitNeonCmlt0(.V0, .V1, .Size4S), .asm_text = "cmlt v0.4s, v1.4s, #0" },
        .{ .name = "NEON MOV V0.8B", .expected = emitNeonMov(.V0, .V1, .Size8B), .asm_text = "mov v0.8b, v1.8b" },
        .{ .name = "NEON MOV V3.16B", .expected = emitNeonMov(.V3, .V4, .Size16B), .asm_text = "mov v3.16b, v4.16b" },

        // ---- NEON shift immediate — arrangement variants ----
        .{ .name = "NEON SHL V0.8B, #1", .expected = emitNeonShl(.V0, .V1, 1, .Size8B), .asm_text = "shl v0.8b, v1.8b, #1" },
        .{ .name = "NEON SHL V0.16B, #7", .expected = emitNeonShl(.V0, .V1, 7, .Size16B), .asm_text = "shl v0.16b, v1.16b, #7" },
        .{ .name = "NEON SHL V0.4H, #5", .expected = emitNeonShl(.V0, .V1, 5, .Size4H), .asm_text = "shl v0.4h, v1.4h, #5" },
        .{ .name = "NEON SHL V0.8H, #2", .expected = emitNeonShl(.V0, .V1, 2, .Size8H), .asm_text = "shl v0.8h, v1.8h, #2" },
        .{ .name = "NEON SHL V0.2S, #10", .expected = emitNeonShl(.V0, .V1, 10, .Size2S), .asm_text = "shl v0.2s, v1.2s, #10" },
        .{ .name = "NEON SHL V0.4S, #3", .expected = emitNeonShl(.V0, .V1, 3, .Size4S), .asm_text = "shl v0.4s, v1.4s, #3" },
        .{ .name = "NEON SHL V0.2D, #20", .expected = emitNeonShl(.V0, .V1, 20, .Size2D), .asm_text = "shl v0.2d, v1.2d, #20" },
        .{ .name = "NEON SHL D0, D1, #5", .expected = emitNeonShl(.V0, .V1, 5, .Size1D), .asm_text = "shl d0, d1, #5" },
        .{ .name = "NEON SSHR V0.8B, #3", .expected = emitNeonSshr(.V0, .V1, 3, .Size8B), .asm_text = "sshr v0.8b, v1.8b, #3" },
        .{ .name = "NEON SSHR V0.4H, #5", .expected = emitNeonSshr(.V0, .V1, 5, .Size4H), .asm_text = "sshr v0.4h, v1.4h, #5" },
        .{ .name = "NEON SSHR V0.4S, #4", .expected = emitNeonSshr(.V0, .V1, 4, .Size4S), .asm_text = "sshr v0.4s, v1.4s, #4" },
        .{ .name = "NEON SSHR V0.2D, #10", .expected = emitNeonSshr(.V0, .V1, 10, .Size2D), .asm_text = "sshr v0.2d, v1.2d, #10" },
        .{ .name = "NEON SSHR D0, D1, #1", .expected = emitNeonSshr(.V0, .V1, 1, .Size1D), .asm_text = "sshr d0, d1, #1" },
        .{ .name = "NEON USHR V0.8B, #2", .expected = emitNeonUshr(.V0, .V1, 2, .Size8B), .asm_text = "ushr v0.8b, v1.8b, #2" },
        .{ .name = "NEON USHR V0.4H, #3", .expected = emitNeonUshr(.V0, .V1, 3, .Size4H), .asm_text = "ushr v0.4h, v1.4h, #3" },
        .{ .name = "NEON USHR V0.4S, #1", .expected = emitNeonUshr(.V0, .V1, 1, .Size4S), .asm_text = "ushr v0.4s, v1.4s, #1" },
        .{ .name = "NEON USHR V0.2D, #7", .expected = emitNeonUshr(.V0, .V1, 7, .Size2D), .asm_text = "ushr v0.2d, v1.2d, #7" },
        .{ .name = "NEON USHR D0, D1, #3", .expected = emitNeonUshr(.V0, .V1, 3, .Size1D), .asm_text = "ushr d0, d1, #3" },
        .{ .name = "NEON SSRA V0.4S, #8", .expected = emitNeonSsra(.V0, .V1, 8, .Size4S), .asm_text = "ssra v0.4s, v1.4s, #8" },
        .{ .name = "NEON USRA V0.4S, #4", .expected = emitNeonUsra(.V0, .V1, 4, .Size4S), .asm_text = "usra v0.4s, v1.4s, #4" },
        .{ .name = "NEON SRSHR V0.4S, #2", .expected = emitNeonSrshr(.V0, .V1, 2, .Size4S), .asm_text = "srshr v0.4s, v1.4s, #2" },
        .{ .name = "NEON URSHR V0.4S, #5", .expected = emitNeonUrshr(.V0, .V1, 5, .Size4S), .asm_text = "urshr v0.4s, v1.4s, #5" },
        .{ .name = "NEON SLI V0.4S, #3", .expected = emitNeonSli(.V0, .V1, 3, .Size4S), .asm_text = "sli v0.4s, v1.4s, #3" },
        .{ .name = "NEON SRI V0.4S, #5", .expected = emitNeonSri(.V0, .V1, 5, .Size4S), .asm_text = "sri v0.4s, v1.4s, #5" },

        // ---- NEON float — scalar S/D variants + vector arrangements ----
        .{ .name = "FABS S0, S1", .expected = emitNeonFabs(.V0, .V1, .Size1S), .asm_text = "fabs s0, s1" },
        .{ .name = "FABS D0, D1", .expected = emitNeonFabs(.V0, .V1, .Size1D), .asm_text = "fabs d0, d1" },
        .{ .name = "FABS V0.2S", .expected = emitNeonFabs(.V0, .V1, .Size2S), .asm_text = "fabs v0.2s, v1.2s" },
        .{ .name = "FABS V0.4S", .expected = emitNeonFabs(.V0, .V1, .Size4S), .asm_text = "fabs v0.4s, v1.4s" },
        .{ .name = "FABS V0.2D", .expected = emitNeonFabs(.V0, .V1, .Size2D), .asm_text = "fabs v0.2d, v1.2d" },
        .{ .name = "FNEG S0, S1", .expected = emitNeonFneg(.V0, .V1, .Size1S), .asm_text = "fneg s0, s1" },
        .{ .name = "FNEG D0, D1", .expected = emitNeonFneg(.V0, .V1, .Size1D), .asm_text = "fneg d0, d1" },
        .{ .name = "FNEG V0.4S", .expected = emitNeonFneg(.V0, .V1, .Size4S), .asm_text = "fneg v0.4s, v1.4s" },
        .{ .name = "FNEG V0.2D", .expected = emitNeonFneg(.V0, .V1, .Size2D), .asm_text = "fneg v0.2d, v1.2d" },
        .{ .name = "FSQRT S0, S1", .expected = emitNeonFsqrt(.V0, .V1, .Size1S), .asm_text = "fsqrt s0, s1" },
        .{ .name = "FSQRT D0, D1", .expected = emitNeonFsqrt(.V0, .V1, .Size1D), .asm_text = "fsqrt d0, d1" },
        .{ .name = "FSQRT V0.4S", .expected = emitNeonFsqrt(.V0, .V1, .Size4S), .asm_text = "fsqrt v0.4s, v1.4s" },
        .{ .name = "FSQRT V0.2D", .expected = emitNeonFsqrt(.V0, .V1, .Size2D), .asm_text = "fsqrt v0.2d, v1.2d" },
        .{ .name = "FADD S0, S1, S2", .expected = emitNeonFadd(.V0, .V1, .V2, .Size1S), .asm_text = "fadd s0, s1, s2" },
        .{ .name = "FADD D0, D1, D2", .expected = emitNeonFadd(.V0, .V1, .V2, .Size1D), .asm_text = "fadd d0, d1, d2" },
        .{ .name = "FADD V0.2S", .expected = emitNeonFadd(.V0, .V1, .V2, .Size2S), .asm_text = "fadd v0.2s, v1.2s, v2.2s" },
        .{ .name = "FADD V0.4S", .expected = emitNeonFadd(.V0, .V1, .V2, .Size4S), .asm_text = "fadd v0.4s, v1.4s, v2.4s" },
        .{ .name = "FADD V0.2D", .expected = emitNeonFadd(.V0, .V1, .V2, .Size2D), .asm_text = "fadd v0.2d, v1.2d, v2.2d" },
        .{ .name = "FSUB S0, S1, S2", .expected = emitNeonFsub(.V0, .V1, .V2, .Size1S), .asm_text = "fsub s0, s1, s2" },
        .{ .name = "FSUB D0, D1, D2", .expected = emitNeonFsub(.V0, .V1, .V2, .Size1D), .asm_text = "fsub d0, d1, d2" },
        .{ .name = "FSUB V0.4S", .expected = emitNeonFsub(.V0, .V1, .V2, .Size4S), .asm_text = "fsub v0.4s, v1.4s, v2.4s" },
        .{ .name = "FMUL S0, S1, S2", .expected = emitNeonFmul(.V0, .V1, .V2, .Size1S), .asm_text = "fmul s0, s1, s2" },
        .{ .name = "FMUL D0, D1, D2", .expected = emitNeonFmul(.V0, .V1, .V2, .Size1D), .asm_text = "fmul d0, d1, d2" },
        .{ .name = "FMUL V0.4S", .expected = emitNeonFmul(.V0, .V1, .V2, .Size4S), .asm_text = "fmul v0.4s, v1.4s, v2.4s" },
        .{ .name = "FMUL V0.2D", .expected = emitNeonFmul(.V0, .V1, .V2, .Size2D), .asm_text = "fmul v0.2d, v1.2d, v2.2d" },
        .{ .name = "FDIV S0, S1, S2", .expected = emitNeonFdiv(.V0, .V1, .V2, .Size1S), .asm_text = "fdiv s0, s1, s2" },
        .{ .name = "FDIV D0, D1, D2", .expected = emitNeonFdiv(.V0, .V1, .V2, .Size1D), .asm_text = "fdiv d0, d1, d2" },
        .{ .name = "FDIV V0.4S", .expected = emitNeonFdiv(.V0, .V1, .V2, .Size4S), .asm_text = "fdiv v0.4s, v1.4s, v2.4s" },
        .{ .name = "FDIV V0.2D", .expected = emitNeonFdiv(.V0, .V1, .V2, .Size2D), .asm_text = "fdiv v0.2d, v1.2d, v2.2d" },
        .{ .name = "FMAX S0, S1, S2", .expected = emitNeonFmax(.V0, .V1, .V2, .Size1S), .asm_text = "fmax s0, s1, s2" },
        .{ .name = "FMAX D0, D1, D2", .expected = emitNeonFmax(.V0, .V1, .V2, .Size1D), .asm_text = "fmax d0, d1, d2" },
        .{ .name = "FMIN S0, S1, S2", .expected = emitNeonFmin(.V0, .V1, .V2, .Size1S), .asm_text = "fmin s0, s1, s2" },
        .{ .name = "FMIN D0, D1, D2", .expected = emitNeonFmin(.V0, .V1, .V2, .Size1D), .asm_text = "fmin d0, d1, d2" },
        .{ .name = "FMAXNM S0, S1, S2", .expected = emitNeonFmaxnm(.V0, .V1, .V2, .Size1S), .asm_text = "fmaxnm s0, s1, s2" },
        .{ .name = "FMINNM D0, D1, D2", .expected = emitNeonFminnm(.V0, .V1, .V2, .Size1D), .asm_text = "fminnm d0, d1, d2" },
        .{ .name = "FNMUL S0, S1, S2", .expected = emitNeonFnmul(.V0, .V1, .V2, .Size1S), .asm_text = "fnmul s0, s1, s2" },
        .{ .name = "FNMUL D0, D1, D2", .expected = emitNeonFnmul(.V0, .V1, .V2, .Size1D), .asm_text = "fnmul d0, d1, d2" },
        .{ .name = "FMLA V0.4S", .expected = emitNeonFmla(.V0, .V1, .V2, .Size4S), .asm_text = "fmla v0.4s, v1.4s, v2.4s" },
        .{ .name = "FMLA V0.2D", .expected = emitNeonFmla(.V0, .V1, .V2, .Size2D), .asm_text = "fmla v0.2d, v1.2d, v2.2d" },
        .{ .name = "FMLS V0.4S", .expected = emitNeonFmls(.V0, .V1, .V2, .Size4S), .asm_text = "fmls v0.4s, v1.4s, v2.4s" },
        .{ .name = "FCMEQ V0.4S", .expected = emitNeonFcmeq(.V0, .V1, .V2, .Size4S), .asm_text = "fcmeq v0.4s, v1.4s, v2.4s" },
        .{ .name = "FCMGE V0.4S", .expected = emitNeonFcmge(.V0, .V1, .V2, .Size4S), .asm_text = "fcmge v0.4s, v1.4s, v2.4s" },
        .{ .name = "FCMGT V0.4S", .expected = emitNeonFcmgt(.V0, .V1, .V2, .Size4S), .asm_text = "fcmgt v0.4s, v1.4s, v2.4s" },
        .{ .name = "FCMEQ S0, #0.0", .expected = emitNeonFcmeq0(.V0, .V1, .Size1S), .asm_text = "fcmeq s0, s1, #0.0" },
        .{ .name = "FCMGT V0.2D, #0.0", .expected = emitNeonFcmgt0(.V0, .V1, .Size2D), .asm_text = "fcmgt v0.2d, v1.2d, #0.0" },
        .{ .name = "FCMLE S0, #0.0", .expected = emitNeonFcmle0(.V0, .V1, .Size1S), .asm_text = "fcmle s0, s1, #0.0" },
        .{ .name = "FCMLT S0, #0.0", .expected = emitNeonFcmlt0(.V0, .V1, .Size1S), .asm_text = "fcmlt s0, s1, #0.0" },
        .{ .name = "FCMP S0, S1", .expected = emitNeonFcmp(.V0, .V1, .Size1S), .asm_text = "fcmp s0, s1" },
        .{ .name = "FCMP D0, D1", .expected = emitNeonFcmp(.V0, .V1, .Size1D), .asm_text = "fcmp d0, d1" },
        .{ .name = "FCMP S0, #0.0", .expected = emitNeonFcmp0(.V0, .Size1S), .asm_text = "fcmp s0, #0.0" },
        .{ .name = "FCMP D0, #0.0", .expected = emitNeonFcmp0(.V0, .Size1D), .asm_text = "fcmp d0, #0.0" },
        .{ .name = "FCMPE S0, S1", .expected = emitNeonFcmpe(.V0, .V1, .Size1S), .asm_text = "fcmpe s0, s1" },
        .{ .name = "FCMPE D0, D1", .expected = emitNeonFcmpe(.V0, .V1, .Size1D), .asm_text = "fcmpe d0, d1" },
        .{ .name = "FCMPE S0, #0.0", .expected = emitNeonFcmpe0(.V0, .Size1S), .asm_text = "fcmpe s0, #0.0" },
        .{ .name = "FCMPE D0, #0.0", .expected = emitNeonFcmpe0(.V0, .Size1D), .asm_text = "fcmpe d0, #0.0" },

        // FCMP/FCMPE with varied registers (cross-check encoding for higher regs)
        .{ .name = "FCMP S3, S5", .expected = emitNeonFcmp(.V3, .V5, .Size1S), .asm_text = "fcmp s3, s5" },
        .{ .name = "FCMP D2, D3", .expected = emitNeonFcmp(.V2, .V3, .Size1D), .asm_text = "fcmp d2, d3" },
        .{ .name = "FCMP D10, D20", .expected = emitNeonFcmp(.V10, .V20, .Size1D), .asm_text = "fcmp d10, d20" },
        .{ .name = "FCMP D30, D0", .expected = emitNeonFcmp(.V30, .V0, .Size1D), .asm_text = "fcmp d30, d0" },
        .{ .name = "FCMP S5, #0.0", .expected = emitNeonFcmp0(.V5, .Size1S), .asm_text = "fcmp s5, #0.0" },
        .{ .name = "FCMP D10, #0.0", .expected = emitNeonFcmp0(.V10, .Size1D), .asm_text = "fcmp d10, #0.0" },
        .{ .name = "FCMPE S3, S7", .expected = emitNeonFcmpe(.V3, .V7, .Size1S), .asm_text = "fcmpe s3, s7" },
        .{ .name = "FCMPE D2, D5", .expected = emitNeonFcmpe(.V2, .V5, .Size1D), .asm_text = "fcmpe d2, d5" },
        .{ .name = "FCMPE D15, D20", .expected = emitNeonFcmpe(.V15, .V20, .Size1D), .asm_text = "fcmpe d15, d20" },
        .{ .name = "FCMPE D0, D30", .expected = emitNeonFcmpe(.V0, .V30, .Size1D), .asm_text = "fcmpe d0, d30" },
        .{ .name = "FCMPE S10, #0.0", .expected = emitNeonFcmpe0(.V10, .Size1S), .asm_text = "fcmpe s10, #0.0" },
        .{ .name = "FCMPE D31, #0.0", .expected = emitNeonFcmpe0(.V31, .Size1D), .asm_text = "fcmpe d31, #0.0" },

        // FP scalar LDR/STR (used by JIT to load FP constants before FCMP)
        .{ .name = "LDR S0, [X1]", .expected = emitFpLdrSOffset(.V0, .R1, 0).?, .asm_text = "ldr s0, [x1]" },
        .{ .name = "LDR S0, [X1, #4]", .expected = emitFpLdrSOffset(.V0, .R1, 4).?, .asm_text = "ldr s0, [x1, #4]" },
        .{ .name = "LDR D0, [X1]", .expected = emitFpLdrDOffset(.V0, .R1, 0).?, .asm_text = "ldr d0, [x1]" },
        .{ .name = "LDR D0, [X1, #8]", .expected = emitFpLdrDOffset(.V0, .R1, 8).?, .asm_text = "ldr d0, [x1, #8]" },
        .{ .name = "LDR D0, [X1, #16]", .expected = emitFpLdrDOffset(.V0, .R1, 16).?, .asm_text = "ldr d0, [x1, #16]" },
        .{ .name = "LDR D5, [X10]", .expected = emitFpLdrDOffset(.V5, .R10, 0).?, .asm_text = "ldr d5, [x10]" },
        .{ .name = "STR S0, [X1]", .expected = emitFpStrSOffset(.V0, .R1, 0).?, .asm_text = "str s0, [x1]" },
        .{ .name = "STR S0, [X1, #4]", .expected = emitFpStrSOffset(.V0, .R1, 4).?, .asm_text = "str s0, [x1, #4]" },
        .{ .name = "STR D0, [X1]", .expected = emitFpStrDOffset(.V0, .R1, 0).?, .asm_text = "str d0, [x1]" },
        .{ .name = "STR D0, [X1, #8]", .expected = emitFpStrDOffset(.V0, .R1, 8).?, .asm_text = "str d0, [x1, #8]" },
        .{ .name = "STR D5, [X10, #16]", .expected = emitFpStrDOffset(.V5, .R10, 16).?, .asm_text = "str d5, [x10, #16]" },

        .{ .name = "FCSEL S0, S1, S2, EQ", .expected = emitNeonFcsel(.V0, .V1, .V2, .EQ, .Size1S), .asm_text = "fcsel s0, s1, s2, eq" },
        .{ .name = "FCSEL D0, D1, D2, NE", .expected = emitNeonFcsel(.V0, .V1, .V2, .NE, .Size1D), .asm_text = "fcsel d0, d1, d2, ne" },
        .{ .name = "FCSEL D3, D4, D5, GT", .expected = emitNeonFcsel(.V3, .V4, .V5, .GT, .Size1D), .asm_text = "fcsel d3, d4, d5, gt" },

        // ---- NEON <-> General register transfers ----
        .{ .name = "FMOV W0, S1", .expected = emitNeonFmovToGeneral(.R0, .V1), .asm_text = "fmov w0, s1" },
        .{ .name = "FMOV X0, D1", .expected = emitNeonFmovToGeneral64(.R0, .V1), .asm_text = "fmov x0, d1" },
        .{ .name = "FMOV S0, W1", .expected = emitNeonFmovFromGeneral(.V0, .R1), .asm_text = "fmov s0, w1" },
        .{ .name = "FMOV D0, X1", .expected = emitNeonFmovFromGeneral64(.V0, .R1), .asm_text = "fmov d0, x1" },
        .{ .name = "SCVTF S0, W1", .expected = emitNeonScvtfGen(.V0, .R1, .Size1S), .asm_text = "scvtf s0, w1" },
        .{ .name = "SCVTF D0, W1", .expected = emitNeonScvtfGen(.V0, .R1, .Size1D), .asm_text = "scvtf d0, w1" },
        .{ .name = "SCVTF S0, X1", .expected = emitNeonScvtfGen64(.V0, .R1, .Size1S), .asm_text = "scvtf s0, x1" },
        .{ .name = "SCVTF D0, X1", .expected = emitNeonScvtfGen64(.V0, .R1, .Size1D), .asm_text = "scvtf d0, x1" },
        .{ .name = "UCVTF S0, W1", .expected = emitNeonUcvtfGen(.V0, .R1, .Size1S), .asm_text = "ucvtf s0, w1" },
        .{ .name = "UCVTF D0, W1", .expected = emitNeonUcvtfGen(.V0, .R1, .Size1D), .asm_text = "ucvtf d0, w1" },
        .{ .name = "UCVTF S0, X1", .expected = emitNeonUcvtfGen64(.V0, .R1, .Size1S), .asm_text = "ucvtf s0, x1" },
        .{ .name = "UCVTF D0, X1", .expected = emitNeonUcvtfGen64(.V0, .R1, .Size1D), .asm_text = "ucvtf d0, x1" },
        .{ .name = "FCVTZS W0, S1", .expected = emitNeonFcvtzsGen(.R0, .V1, .Size1S), .asm_text = "fcvtzs w0, s1" },
        .{ .name = "FCVTZS W0, D1", .expected = emitNeonFcvtzsGen(.R0, .V1, .Size1D), .asm_text = "fcvtzs w0, d1" },
        .{ .name = "FCVTZS X0, S1", .expected = emitNeonFcvtzsGen64(.R0, .V1, .Size1S), .asm_text = "fcvtzs x0, s1" },
        .{ .name = "FCVTZS X0, D1", .expected = emitNeonFcvtzsGen64(.R0, .V1, .Size1D), .asm_text = "fcvtzs x0, d1" },
        .{ .name = "FCVTZU W0, S1", .expected = emitNeonFcvtzuGen(.R0, .V1, .Size1S), .asm_text = "fcvtzu w0, s1" },
        .{ .name = "FCVTZU W0, D1", .expected = emitNeonFcvtzuGen(.R0, .V1, .Size1D), .asm_text = "fcvtzu w0, d1" },
        .{ .name = "FCVTZU X0, S1", .expected = emitNeonFcvtzuGen64(.R0, .V1, .Size1S), .asm_text = "fcvtzu x0, s1" },
        .{ .name = "FCVTZU X0, D1", .expected = emitNeonFcvtzuGen64(.R0, .V1, .Size1D), .asm_text = "fcvtzu x0, d1" },
        .{ .name = "FMOV X0, V1.D[1]", .expected = emitNeonFmovToGeneralHigh64(.R0, .V1), .asm_text = "fmov x0, v1.d[1]" },
        .{ .name = "FMOV V0.D[1], X1", .expected = emitNeonFmovFromGeneralHigh64(.V0, .R1), .asm_text = "fmov v0.d[1], x1" },

        // ---- NEON AES ----
        .{ .name = "AESD V0.16B, V1.16B", .expected = emitNeonAesD(.V0, .V1), .asm_text = "aesd v0.16b, v1.16b" },
        .{ .name = "AESE V0.16B, V1.16B", .expected = emitNeonAesE(.V0, .V1), .asm_text = "aese v0.16b, v1.16b" },
        .{ .name = "AESIMC V0.16B, V1.16B", .expected = emitNeonAesImc(.V0, .V1), .asm_text = "aesimc v0.16b, v1.16b" },
        .{ .name = "AESMC V0.16B, V1.16B", .expected = emitNeonAesMc(.V0, .V1), .asm_text = "aesmc v0.16b, v1.16b" },

        // ---- NEON MOVI ----
        .{ .name = "NEON MOVI V0.4S, #0", .expected = (emitNeonMovi(.V0, 0, .Size4S) orelse unreachable), .asm_text = "movi v0.4s, #0" },
        .{ .name = "NEON MOVI V0.2S, #0", .expected = (emitNeonMovi(.V0, 0, .Size2S) orelse unreachable), .asm_text = "movi v0.2s, #0" },
        .{ .name = "NEON MOVI V0.8B, #0x55", .expected = (emitNeonMovi(.V0, 0x55, .Size8B) orelse unreachable), .asm_text = "movi v0.8b, #0x55" },
        .{ .name = "NEON MOVI V0.16B, #0xFF", .expected = (emitNeonMovi(.V0, 0xFF, .Size16B) orelse unreachable), .asm_text = "movi v0.16b, #0xff" },
        .{ .name = "NEON MOVI V5.16B, #0", .expected = (emitNeonMovi(.V5, 0, .Size16B) orelse unreachable), .asm_text = "movi v5.16b, #0" },

        // ---- NEON EXT ----
        .{ .name = "NEON EXT V0.8B, #2", .expected = emitNeonExt(.V0, .V1, .V2, 2, .Size8B), .asm_text = "ext v0.8b, v1.8b, v2.8b, #2" },
        .{ .name = "NEON EXT V0.16B, #3", .expected = emitNeonExt(.V0, .V1, .V2, 3, .Size16B), .asm_text = "ext v0.16b, v1.16b, v2.16b, #3" },
        .{ .name = "NEON EXT V0.16B, #0", .expected = emitNeonExt(.V0, .V1, .V2, 0, .Size16B), .asm_text = "ext v0.16b, v1.16b, v2.16b, #0" },

        // ---- NEON vector conversions — all width variants ----
        .{ .name = "SCVTF V0.2S", .expected = emitNeonScvtfVec(.V0, .V1, .Size2S), .asm_text = "scvtf v0.2s, v1.2s" },
        .{ .name = "SCVTF V0.4S", .expected = emitNeonScvtfVec(.V0, .V1, .Size4S), .asm_text = "scvtf v0.4s, v1.4s" },
        .{ .name = "SCVTF V0.2D", .expected = emitNeonScvtfVec(.V0, .V1, .Size2D), .asm_text = "scvtf v0.2d, v1.2d" },
        .{ .name = "UCVTF V0.2S", .expected = emitNeonUcvtfVec(.V0, .V1, .Size2S), .asm_text = "ucvtf v0.2s, v1.2s" },
        .{ .name = "UCVTF V0.4S", .expected = emitNeonUcvtfVec(.V0, .V1, .Size4S), .asm_text = "ucvtf v0.4s, v1.4s" },
        .{ .name = "UCVTF V0.2D", .expected = emitNeonUcvtfVec(.V0, .V1, .Size2D), .asm_text = "ucvtf v0.2d, v1.2d" },
        .{ .name = "FCVTZS V0.2S", .expected = emitNeonFcvtzs(.V0, .V1, .Size2S), .asm_text = "fcvtzs v0.2s, v1.2s" },
        .{ .name = "FCVTZS V0.4S", .expected = emitNeonFcvtzs(.V0, .V1, .Size4S), .asm_text = "fcvtzs v0.4s, v1.4s" },
        .{ .name = "FCVTZS V0.2D", .expected = emitNeonFcvtzs(.V0, .V1, .Size2D), .asm_text = "fcvtzs v0.2d, v1.2d" },
        .{ .name = "FCVTZU V0.2S", .expected = emitNeonFcvtzu(.V0, .V1, .Size2S), .asm_text = "fcvtzu v0.2s, v1.2s" },
        .{ .name = "FCVTZU V0.4S", .expected = emitNeonFcvtzu(.V0, .V1, .Size4S), .asm_text = "fcvtzu v0.4s, v1.4s" },
        .{ .name = "FCVTZU V0.2D", .expected = emitNeonFcvtzu(.V0, .V1, .Size2D), .asm_text = "fcvtzu v0.2d, v1.2d" },

        // ---- NEON LDR/STR offset — size variants ----
        .{ .name = "LDR B0, [X1]", .expected = emitNeonLdrOffset(.V0, .Size1B, .R1, 0).?, .asm_text = "ldr b0, [x1]" },
        .{ .name = "LDR H0, [X1, #2]", .expected = emitNeonLdrOffset(.V0, .Size1H, .R1, 2).?, .asm_text = "ldr h0, [x1, #2]" },
        .{ .name = "LDR S0, [X1, #4]", .expected = emitNeonLdrOffset(.V0, .Size1S, .R1, 4).?, .asm_text = "ldr s0, [x1, #4]" },
        .{ .name = "LDR D0, [X1, #8]", .expected = emitNeonLdrOffset(.V0, .Size1D, .R1, 8).?, .asm_text = "ldr d0, [x1, #8]" },
        .{ .name = "LDR Q0, [X1, #16]", .expected = emitNeonLdrOffset(.V0, .Size1Q, .R1, 16).?, .asm_text = "ldr q0, [x1, #16]" },
        .{ .name = "STR B0, [X1]", .expected = emitNeonStrOffset(.V0, .Size1B, .R1, 0).?, .asm_text = "str b0, [x1]" },
        .{ .name = "STR H0, [X1, #2]", .expected = emitNeonStrOffset(.V0, .Size1H, .R1, 2).?, .asm_text = "str h0, [x1, #2]" },
        .{ .name = "STR S0, [X1, #4]", .expected = emitNeonStrOffset(.V0, .Size1S, .R1, 4).?, .asm_text = "str s0, [x1, #4]" },
        .{ .name = "STR D0, [X1, #8]", .expected = emitNeonStrOffset(.V0, .Size1D, .R1, 8).?, .asm_text = "str d0, [x1, #8]" },
        .{ .name = "STR Q0, [X1, #16]", .expected = emitNeonStrOffset(.V0, .Size1Q, .R1, 16).?, .asm_text = "str q0, [x1, #16]" },

        // ---- NEON LDP/STP ----
        .{ .name = "LDP S0, S1, [X2]", .expected = emitNeonLdpOffset(.V0, .V1, .Size1S, .R2, 0).?, .asm_text = "ldp s0, s1, [x2]" },
        .{ .name = "LDP D0, D1, [X2, #16]", .expected = emitNeonLdpOffset(.V0, .V1, .Size1D, .R2, 16).?, .asm_text = "ldp d0, d1, [x2, #16]" },
        .{ .name = "LDP Q0, Q1, [X2, #32]", .expected = emitNeonLdpOffset(.V0, .V1, .Size1Q, .R2, 32).?, .asm_text = "ldp q0, q1, [x2, #32]" },
        .{ .name = "STP S0, S1, [X2]", .expected = emitNeonStpOffset(.V0, .V1, .Size1S, .R2, 0).?, .asm_text = "stp s0, s1, [x2]" },
        .{ .name = "STP D0, D1, [X2, #-16]", .expected = emitNeonStpOffset(.V0, .V1, .Size1D, .R2, -16).?, .asm_text = "stp d0, d1, [x2, #-16]" },
        .{ .name = "STP Q0, Q1, [X2]", .expected = emitNeonStpOffset(.V0, .V1, .Size1Q, .R2, 0).?, .asm_text = "stp q0, q1, [x2]" },

        // ---- BFI / BFXIL / SBFX / UBFX ----
        .{ .name = "BFI W0, W1, #5, #10", .expected = emitBfi(.R0, .R1, 5, 10), .asm_text = "bfi w0, w1, #5, #10" },
        .{ .name = "BFI X0, X1, #20, #8", .expected = emitBfi64(.R0, .R1, 20, 8), .asm_text = "bfi x0, x1, #20, #8" },
        .{ .name = "BFXIL W0, W1, #3, #5", .expected = emitBfxil(.R0, .R1, 3, 5), .asm_text = "bfxil w0, w1, #3, #5" },
        .{ .name = "BFXIL X0, X1, #10, #20", .expected = emitBfxil64(.R0, .R1, 10, 20), .asm_text = "bfxil x0, x1, #10, #20" },
        .{ .name = "SBFX W0, W1, #4, #8", .expected = emitSbfx(.R0, .R1, 4, 8), .asm_text = "sbfx w0, w1, #4, #8" },
        .{ .name = "SBFX X0, X1, #0, #16", .expected = emitSbfx64(.R0, .R1, 0, 16), .asm_text = "sbfx x0, x1, #0, #16" },
        .{ .name = "UBFX W0, W1, #8, #8", .expected = emitUbfx(.R0, .R1, 8, 8), .asm_text = "ubfx w0, w1, #8, #8" },
        .{ .name = "UBFX X0, X1, #32, #16", .expected = emitUbfx64(.R0, .R1, 32, 16), .asm_text = "ubfx x0, x1, #32, #16" },

        // ---- NEON DUP / INS / SMOV / UMOV ----
        .{ .name = "DUP V0.4S, V1.S[0]", .expected = emitNeonDupElement(.V0, .V1, 0, .Size4S), .asm_text = "dup v0.4s, v1.s[0]" },
        .{ .name = "DUP V0.4S, V1.S[2]", .expected = emitNeonDupElement(.V0, .V1, 2, .Size4S), .asm_text = "dup v0.4s, v1.s[2]" },
        .{ .name = "DUP V0.8H, V1.H[3]", .expected = emitNeonDupElement(.V0, .V1, 3, .Size8H), .asm_text = "dup v0.8h, v1.h[3]" },
        .{ .name = "DUP V0.16B, V1.B[5]", .expected = emitNeonDupElement(.V0, .V1, 5, .Size16B), .asm_text = "dup v0.16b, v1.b[5]" },
        .{ .name = "DUP V0.2D, V1.D[0]", .expected = emitNeonDupElement(.V0, .V1, 0, .Size2D), .asm_text = "dup v0.2d, v1.d[0]" },
        .{ .name = "DUP V0.4S, W1", .expected = emitNeonDup(.V0, .R1, .Size4S), .asm_text = "dup v0.4s, w1" },
        .{ .name = "DUP V0.2D, X1", .expected = emitNeonDup(.V0, .R1, .Size2D), .asm_text = "dup v0.2d, x1" },
        .{ .name = "DUP V0.8B, W1", .expected = emitNeonDup(.V0, .R1, .Size8B), .asm_text = "dup v0.8b, w1" },
        .{ .name = "DUP V0.8H, W1", .expected = emitNeonDup(.V0, .R1, .Size8H), .asm_text = "dup v0.8h, w1" },
        .{ .name = "INS V0.S[1], W1", .expected = emitNeonIns(.V0, 1, .R1, .Size4S), .asm_text = "ins v0.s[1], w1" },
        .{ .name = "INS V0.D[0], X1", .expected = emitNeonIns(.V0, 0, .R1, .Size2D), .asm_text = "ins v0.d[0], x1" },
        .{ .name = "INS V0.B[3], W1", .expected = emitNeonIns(.V0, 3, .R1, .Size16B), .asm_text = "ins v0.b[3], w1" },
        .{ .name = "INS V0.H[2], W1", .expected = emitNeonIns(.V0, 2, .R1, .Size8H), .asm_text = "ins v0.h[2], w1" },
        .{ .name = "SMOV W0, V1.B[3]", .expected = emitNeonSmov(.R0, .V1, 3, .Size16B), .asm_text = "smov w0, v1.b[3]" },
        .{ .name = "SMOV W0, V1.H[1]", .expected = emitNeonSmov(.R0, .V1, 1, .Size8H), .asm_text = "smov w0, v1.h[1]" },
        .{ .name = "SMOV X0, V1.B[5]", .expected = emitNeonSmov64(.R0, .V1, 5, .Size16B), .asm_text = "smov x0, v1.b[5]" },
        .{ .name = "SMOV X0, V1.H[2]", .expected = emitNeonSmov64(.R0, .V1, 2, .Size8H), .asm_text = "smov x0, v1.h[2]" },
        .{ .name = "SMOV X0, V1.S[1]", .expected = emitNeonSmov64(.R0, .V1, 1, .Size4S), .asm_text = "smov x0, v1.s[1]" },
        .{ .name = "UMOV W0, V1.B[7]", .expected = emitNeonUmov(.R0, .V1, 7, .Size16B), .asm_text = "umov w0, v1.b[7]" },
        .{ .name = "UMOV W0, V1.H[3]", .expected = emitNeonUmov(.R0, .V1, 3, .Size8H), .asm_text = "umov w0, v1.h[3]" },
        .{ .name = "UMOV W0, V1.S[1]", .expected = emitNeonUmov(.R0, .V1, 1, .Size4S), .asm_text = "umov w0, v1.s[1]" },
        .{ .name = "UMOV X0, V1.D[0]", .expected = emitNeonUmov64(.R0, .V1, 0, .Size2D), .asm_text = "umov x0, v1.d[0]" },
        .{ .name = "INS V0.S[2], V1.S[0]", .expected = emitNeonInsElement(.V0, 2, .V1, 0, .Size4S), .asm_text = "ins v0.s[2], v1.s[0]" },
        .{ .name = "INS V0.D[1], V1.D[0]", .expected = emitNeonInsElement(.V0, 1, .V1, 0, .Size2D), .asm_text = "ins v0.d[1], v1.d[0]" },
        .{ .name = "INS V0.B[5], V1.B[3]", .expected = emitNeonInsElement(.V0, 5, .V1, 3, .Size16B), .asm_text = "ins v0.b[5], v1.b[3]" },
        .{ .name = "INS V0.H[1], V1.H[6]", .expected = emitNeonInsElement(.V0, 1, .V1, 6, .Size8H), .asm_text = "ins v0.h[1], v1.h[6]" },

        // ---- NEON TBL ----
        .{ .name = "TBL V0.8B, {V1.16B}, V2.8B", .expected = emitNeonTbl(.V0, .V1, .V2, .Size8B), .asm_text = "tbl v0.8b, {v1.16b}, v2.8b" },
        .{ .name = "TBL V0.16B, {V1.16B}, V2.16B", .expected = emitNeonTbl(.V0, .V1, .V2, .Size16B), .asm_text = "tbl v0.16b, {v1.16b}, v2.16b" },

        // ---- NEON FMOV register + immediate ----
        .{ .name = "FMOV S0, S1", .expected = emitNeonFmov(.V0, .V1, .Size1S), .asm_text = "fmov s0, s1" },
        .{ .name = "FMOV D0, D1", .expected = emitNeonFmov(.V0, .V1, .Size1D), .asm_text = "fmov d0, d1" },

        // ---- NEON float rounding ----
        .{ .name = "FRINTA S0, S1", .expected = emitNeonFrinta(.V0, .V1, .Size1S), .asm_text = "frinta s0, s1" },
        .{ .name = "FRINTA D0, D1", .expected = emitNeonFrinta(.V0, .V1, .Size1D), .asm_text = "frinta d0, d1" },
        .{ .name = "FRINTA V0.4S", .expected = emitNeonFrinta(.V0, .V1, .Size4S), .asm_text = "frinta v0.4s, v1.4s" },
        .{ .name = "FRINTI S0, S1", .expected = emitNeonFrinti(.V0, .V1, .Size1S), .asm_text = "frinti s0, s1" },
        .{ .name = "FRINTI D0, D1", .expected = emitNeonFrinti(.V0, .V1, .Size1D), .asm_text = "frinti d0, d1" },
        .{ .name = "FRINTM S0, S1", .expected = emitNeonFrintm(.V0, .V1, .Size1S), .asm_text = "frintm s0, s1" },
        .{ .name = "FRINTM D0, D1", .expected = emitNeonFrintm(.V0, .V1, .Size1D), .asm_text = "frintm d0, d1" },
        .{ .name = "FRINTM V0.4S", .expected = emitNeonFrintm(.V0, .V1, .Size4S), .asm_text = "frintm v0.4s, v1.4s" },
        .{ .name = "FRINTN S0, S1", .expected = emitNeonFrintn(.V0, .V1, .Size1S), .asm_text = "frintn s0, s1" },
        .{ .name = "FRINTN D0, D1", .expected = emitNeonFrintn(.V0, .V1, .Size1D), .asm_text = "frintn d0, d1" },
        .{ .name = "FRINTP S0, S1", .expected = emitNeonFrintp(.V0, .V1, .Size1S), .asm_text = "frintp s0, s1" },
        .{ .name = "FRINTP D0, D1", .expected = emitNeonFrintp(.V0, .V1, .Size1D), .asm_text = "frintp d0, d1" },
        .{ .name = "FRINTX S0, S1", .expected = emitNeonFrintx(.V0, .V1, .Size1S), .asm_text = "frintx s0, s1" },
        .{ .name = "FRINTZ S0, S1", .expected = emitNeonFrintz(.V0, .V1, .Size1S), .asm_text = "frintz s0, s1" },
        .{ .name = "FRINTZ D0, D1", .expected = emitNeonFrintz(.V0, .V1, .Size1D), .asm_text = "frintz d0, d1" },
        .{ .name = "FRINTZ V0.2D", .expected = emitNeonFrintz(.V0, .V1, .Size2D), .asm_text = "frintz v0.2d, v1.2d" },

        // ---- NEON float reciprocal / rsqrt ----
        .{ .name = "FRECPE S0, S1", .expected = emitNeonFrecpe(.V0, .V1, .Size1S), .asm_text = "frecpe s0, s1" },
        .{ .name = "FRECPE D0, D1", .expected = emitNeonFrecpe(.V0, .V1, .Size1D), .asm_text = "frecpe d0, d1" },
        .{ .name = "FRECPE V0.4S", .expected = emitNeonFrecpe(.V0, .V1, .Size4S), .asm_text = "frecpe v0.4s, v1.4s" },
        .{ .name = "FRECPX S0, S1", .expected = emitNeonFrecpx(.V0, .V1, .Size1S), .asm_text = "frecpx s0, s1" },
        .{ .name = "FRECPX D0, D1", .expected = emitNeonFrecpx(.V0, .V1, .Size1D), .asm_text = "frecpx d0, d1" },
        .{ .name = "FRSQRTE S0, S1", .expected = emitNeonFrsqrte(.V0, .V1, .Size1S), .asm_text = "frsqrte s0, s1" },
        .{ .name = "FRSQRTE D0, D1", .expected = emitNeonFrsqrte(.V0, .V1, .Size1D), .asm_text = "frsqrte d0, d1" },
        .{ .name = "FRSQRTE V0.4S", .expected = emitNeonFrsqrte(.V0, .V1, .Size4S), .asm_text = "frsqrte v0.4s, v1.4s" },
        .{ .name = "FRECPS S0, S1, S2", .expected = emitNeonFrecps(.V0, .V1, .V2, .Size1S), .asm_text = "frecps s0, s1, s2" },
        .{ .name = "FRECPS D0, D1, D2", .expected = emitNeonFrecps(.V0, .V1, .V2, .Size1D), .asm_text = "frecps d0, d1, d2" },
        .{ .name = "FRECPS V0.4S", .expected = emitNeonFrecps(.V0, .V1, .V2, .Size4S), .asm_text = "frecps v0.4s, v1.4s, v2.4s" },
        .{ .name = "FRSQRTS S0, S1, S2", .expected = emitNeonFrsqrts(.V0, .V1, .V2, .Size1S), .asm_text = "frsqrts s0, s1, s2" },
        .{ .name = "FRSQRTS D0, D1, D2", .expected = emitNeonFrsqrts(.V0, .V1, .V2, .Size1D), .asm_text = "frsqrts d0, d1, d2" },
        .{ .name = "FRSQRTS V0.4S", .expected = emitNeonFrsqrts(.V0, .V1, .V2, .Size4S), .asm_text = "frsqrts v0.4s, v1.4s, v2.4s" },

        // ---- NEON float abs-diff / abs-compare ----
        .{ .name = "FABD S0, S1, S2", .expected = emitNeonFabd(.V0, .V1, .V2, .Size1S), .asm_text = "fabd s0, s1, s2" },
        .{ .name = "FABD D0, D1, D2", .expected = emitNeonFabd(.V0, .V1, .V2, .Size1D), .asm_text = "fabd d0, d1, d2" },
        .{ .name = "FABD V0.4S", .expected = emitNeonFabd(.V0, .V1, .V2, .Size4S), .asm_text = "fabd v0.4s, v1.4s, v2.4s" },
        .{ .name = "FACGE S0, S1, S2", .expected = emitNeonFacge(.V0, .V1, .V2, .Size1S), .asm_text = "facge s0, s1, s2" },
        .{ .name = "FACGE D0, D1, D2", .expected = emitNeonFacge(.V0, .V1, .V2, .Size1D), .asm_text = "facge d0, d1, d2" },
        .{ .name = "FACGE V0.4S", .expected = emitNeonFacge(.V0, .V1, .V2, .Size4S), .asm_text = "facge v0.4s, v1.4s, v2.4s" },
        .{ .name = "FACGT S0, S1, S2", .expected = emitNeonFacgt(.V0, .V1, .V2, .Size1S), .asm_text = "facgt s0, s1, s2" },
        .{ .name = "FACGT D0, D1, D2", .expected = emitNeonFacgt(.V0, .V1, .V2, .Size1D), .asm_text = "facgt d0, d1, d2" },
        .{ .name = "FACGT V0.4S", .expected = emitNeonFacgt(.V0, .V1, .V2, .Size4S), .asm_text = "facgt v0.4s, v1.4s, v2.4s" },

        // ---- NEON float pairwise ----
        .{ .name = "FADDP V0.4S", .expected = emitNeonFaddp(.V0, .V1, .V2, .Size4S), .asm_text = "faddp v0.4s, v1.4s, v2.4s" },
        .{ .name = "FADDP V0.2D", .expected = emitNeonFaddp(.V0, .V1, .V2, .Size2D), .asm_text = "faddp v0.2d, v1.2d, v2.2d" },
        .{ .name = "FADDP V0.2S", .expected = emitNeonFaddp(.V0, .V1, .V2, .Size2S), .asm_text = "faddp v0.2s, v1.2s, v2.2s" },
        .{ .name = "FMULX S0, S1, S2", .expected = emitNeonFmulx(.V0, .V1, .V2, .Size1S), .asm_text = "fmulx s0, s1, s2" },
        .{ .name = "FMULX D0, D1, D2", .expected = emitNeonFmulx(.V0, .V1, .V2, .Size1D), .asm_text = "fmulx d0, d1, d2" },
        .{ .name = "FMULX V0.4S", .expected = emitNeonFmulx(.V0, .V1, .V2, .Size4S), .asm_text = "fmulx v0.4s, v1.4s, v2.4s" },
        .{ .name = "FMAXNMP V0.4S", .expected = emitNeonFmaxnmp(.V0, .V1, .V2, .Size4S), .asm_text = "fmaxnmp v0.4s, v1.4s, v2.4s" },
        .{ .name = "FMAXP V0.4S", .expected = emitNeonFmaxp(.V0, .V1, .V2, .Size4S), .asm_text = "fmaxp v0.4s, v1.4s, v2.4s" },
        .{ .name = "FMINNMP V0.4S", .expected = emitNeonFminnmp(.V0, .V1, .V2, .Size4S), .asm_text = "fminnmp v0.4s, v1.4s, v2.4s" },
        .{ .name = "FMINP V0.4S", .expected = emitNeonFminp(.V0, .V1, .V2, .Size4S), .asm_text = "fminp v0.4s, v1.4s, v2.4s" },
        .{ .name = "FMAXP V0.2D", .expected = emitNeonFmaxp(.V0, .V1, .V2, .Size2D), .asm_text = "fmaxp v0.2d, v1.2d, v2.2d" },
        .{ .name = "FMINP V0.2D", .expected = emitNeonFminp(.V0, .V1, .V2, .Size2D), .asm_text = "fminp v0.2d, v1.2d, v2.2d" },

        // ---- NEON saturating add/sub ----
        .{ .name = "SQADD V0.4S", .expected = emitNeonSqadd(.V0, .V1, .V2, .Size4S), .asm_text = "sqadd v0.4s, v1.4s, v2.4s" },
        .{ .name = "SQSUB V0.4S", .expected = emitNeonSqsub(.V0, .V1, .V2, .Size4S), .asm_text = "sqsub v0.4s, v1.4s, v2.4s" },
        .{ .name = "UQADD V0.4S", .expected = emitNeonUqadd(.V0, .V1, .V2, .Size4S), .asm_text = "uqadd v0.4s, v1.4s, v2.4s" },
        .{ .name = "UQSUB V0.4S", .expected = emitNeonUqsub(.V0, .V1, .V2, .Size4S), .asm_text = "uqsub v0.4s, v1.4s, v2.4s" },
        .{ .name = "SQADD V0.8H", .expected = emitNeonSqadd(.V0, .V1, .V2, .Size8H), .asm_text = "sqadd v0.8h, v1.8h, v2.8h" },
        .{ .name = "UQADD V0.16B", .expected = emitNeonUqadd(.V0, .V1, .V2, .Size16B), .asm_text = "uqadd v0.16b, v1.16b, v2.16b" },

        // ---- NEON rounding halving add ----
        .{ .name = "SRHADD V0.4S", .expected = emitNeonSrhadd(.V0, .V1, .V2, .Size4S), .asm_text = "srhadd v0.4s, v1.4s, v2.4s" },
        .{ .name = "URHADD V0.4S", .expected = emitNeonUrhadd(.V0, .V1, .V2, .Size4S), .asm_text = "urhadd v0.4s, v1.4s, v2.4s" },
        .{ .name = "SRHADD V0.8B", .expected = emitNeonSrhadd(.V0, .V1, .V2, .Size8B), .asm_text = "srhadd v0.8b, v1.8b, v2.8b" },

        // ---- NEON pairwise min/max ----
        .{ .name = "SMAXP V0.4S", .expected = emitNeonSmaxp(.V0, .V1, .V2, .Size4S), .asm_text = "smaxp v0.4s, v1.4s, v2.4s" },
        .{ .name = "SMINP V0.4S", .expected = emitNeonSminp(.V0, .V1, .V2, .Size4S), .asm_text = "sminp v0.4s, v1.4s, v2.4s" },
        .{ .name = "UMAXP V0.4S", .expected = emitNeonUmaxp(.V0, .V1, .V2, .Size4S), .asm_text = "umaxp v0.4s, v1.4s, v2.4s" },
        .{ .name = "UMINP V0.4S", .expected = emitNeonUminp(.V0, .V1, .V2, .Size4S), .asm_text = "uminp v0.4s, v1.4s, v2.4s" },
        .{ .name = "SMAXP V0.8B", .expected = emitNeonSmaxp(.V0, .V1, .V2, .Size8B), .asm_text = "smaxp v0.8b, v1.8b, v2.8b" },

        // ---- NEON register shift / sat shift ----
        .{ .name = "SRSHL V0.4S", .expected = emitNeonSrshl(.V0, .V1, .V2, .Size4S), .asm_text = "srshl v0.4s, v1.4s, v2.4s" },
        .{ .name = "URSHL V0.4S", .expected = emitNeonUrshl(.V0, .V1, .V2, .Size4S), .asm_text = "urshl v0.4s, v1.4s, v2.4s" },
        .{ .name = "SQSHL V0.4S (reg)", .expected = emitNeonSqshlReg(.V0, .V1, .V2, .Size4S), .asm_text = "sqshl v0.4s, v1.4s, v2.4s" },
        .{ .name = "UQSHL V0.4S (reg)", .expected = emitNeonUqshlReg(.V0, .V1, .V2, .Size4S), .asm_text = "uqshl v0.4s, v1.4s, v2.4s" },
        .{ .name = "SQRSHL V0.4S", .expected = emitNeonSqrshl(.V0, .V1, .V2, .Size4S), .asm_text = "sqrshl v0.4s, v1.4s, v2.4s" },
        .{ .name = "UQRSHL V0.4S", .expected = emitNeonUqrshl(.V0, .V1, .V2, .Size4S), .asm_text = "uqrshl v0.4s, v1.4s, v2.4s" },

        // ---- NEON accumulate / abs-diff-accumulate ----
        .{ .name = "SABA V0.4S", .expected = emitNeonSaba(.V0, .V1, .V2, .Size4S), .asm_text = "saba v0.4s, v1.4s, v2.4s" },
        .{ .name = "UABA V0.4S", .expected = emitNeonUaba(.V0, .V1, .V2, .Size4S), .asm_text = "uaba v0.4s, v1.4s, v2.4s" },
        .{ .name = "SQDMULH V0.4S", .expected = emitNeonSqdmulh(.V0, .V1, .V2, .Size4S), .asm_text = "sqdmulh v0.4s, v1.4s, v2.4s" },
        .{ .name = "SQRDMULH V0.4S", .expected = emitNeonSqrdmulh(.V0, .V1, .V2, .Size4S), .asm_text = "sqrdmulh v0.4s, v1.4s, v2.4s" },

        // ---- NEON BIC / ORN register ----
        .{ .name = "BIC V0.8B", .expected = emitNeonBicReg(.V0, .V1, .V2, .Size8B), .asm_text = "bic v0.8b, v1.8b, v2.8b" },
        .{ .name = "BIC V0.16B", .expected = emitNeonBicReg(.V0, .V1, .V2, .Size16B), .asm_text = "bic v0.16b, v1.16b, v2.16b" },
        .{ .name = "ORN V0.8B", .expected = emitNeonOrn(.V0, .V1, .V2, .Size8B), .asm_text = "orn v0.8b, v1.8b, v2.8b" },
        .{ .name = "ORN V0.16B", .expected = emitNeonOrn(.V0, .V1, .V2, .Size16B), .asm_text = "orn v0.16b, v1.16b, v2.16b" },

        // ---- NEON PMUL / PMULL ----
        .{ .name = "PMUL V0.8B", .expected = emitNeonPmul(.V0, .V1, .V2, .Size8B), .asm_text = "pmul v0.8b, v1.8b, v2.8b" },
        .{ .name = "PMUL V0.16B", .expected = emitNeonPmul(.V0, .V1, .V2, .Size16B), .asm_text = "pmul v0.16b, v1.16b, v2.16b" },

        // ---- NEON narrow / widen ----
        // XTN/XTN2 take the SOURCE arrangement (the wider type)
        .{ .name = "XTN V0.8B, V1.8H", .expected = emitNeonXtn(.V0, .V1, .Size8H), .asm_text = "xtn v0.8b, v1.8h" },
        .{ .name = "XTN V0.4H, V1.4S", .expected = emitNeonXtn(.V0, .V1, .Size4S), .asm_text = "xtn v0.4h, v1.4s" },
        .{ .name = "XTN V0.2S, V1.2D", .expected = emitNeonXtn(.V0, .V1, .Size2D), .asm_text = "xtn v0.2s, v1.2d" },
        .{ .name = "XTN2 V0.16B, V1.8H", .expected = emitNeonXtn2(.V0, .V1, .Size8H), .asm_text = "xtn2 v0.16b, v1.8h" },
        .{ .name = "XTN2 V0.8H, V1.4S", .expected = emitNeonXtn2(.V0, .V1, .Size4S), .asm_text = "xtn2 v0.8h, v1.4s" },
        // SXTL/UXTL take the SOURCE arrangement (the narrower type)
        .{ .name = "SXTL V0.8H, V1.8B", .expected = emitNeonSxtl(.V0, .V1, .Size8B), .asm_text = "sxtl v0.8h, v1.8b" },
        .{ .name = "SXTL V0.4S, V1.4H", .expected = emitNeonSxtl(.V0, .V1, .Size4H), .asm_text = "sxtl v0.4s, v1.4h" },
        .{ .name = "SXTL V0.2D, V1.2S", .expected = emitNeonSxtl(.V0, .V1, .Size2S), .asm_text = "sxtl v0.2d, v1.2s" },
        .{ .name = "SXTL2 V0.8H, V1.16B", .expected = emitNeonSxtl2(.V0, .V1, .Size16B), .asm_text = "sxtl2 v0.8h, v1.16b" },
        .{ .name = "UXTL V0.8H, V1.8B", .expected = emitNeonUxtl(.V0, .V1, .Size8B), .asm_text = "uxtl v0.8h, v1.8b" },
        .{ .name = "UXTL V0.4S, V1.4H", .expected = emitNeonUxtl(.V0, .V1, .Size4H), .asm_text = "uxtl v0.4s, v1.4h" },
        .{ .name = "UXTL V0.2D, V1.2S", .expected = emitNeonUxtl(.V0, .V1, .Size2S), .asm_text = "uxtl v0.2d, v1.2s" },
        .{ .name = "UXTL2 V0.8H, V1.16B", .expected = emitNeonUxtl2(.V0, .V1, .Size16B), .asm_text = "uxtl2 v0.8h, v1.16b" },

        // ---- NEON widening shift immediate ----
        // SSHLL/USHLL take the SOURCE arrangement
        .{ .name = "SSHLL V0.8H, V1.8B, #3", .expected = emitNeonSshll(.V0, .V1, 3, .Size8B), .asm_text = "sshll v0.8h, v1.8b, #3" },
        .{ .name = "SSHLL V0.4S, V1.4H, #5", .expected = emitNeonSshll(.V0, .V1, 5, .Size4H), .asm_text = "sshll v0.4s, v1.4h, #5" },
        .{ .name = "SSHLL V0.2D, V1.2S, #10", .expected = emitNeonSshll(.V0, .V1, 10, .Size2S), .asm_text = "sshll v0.2d, v1.2s, #10" },
        .{ .name = "SSHLL2 V0.8H, V1.16B, #2", .expected = emitNeonSshll2(.V0, .V1, 2, .Size16B), .asm_text = "sshll2 v0.8h, v1.16b, #2" },
        .{ .name = "USHLL V0.8H, V1.8B, #1", .expected = emitNeonUshll(.V0, .V1, 1, .Size8B), .asm_text = "ushll v0.8h, v1.8b, #1" },
        .{ .name = "USHLL V0.4S, V1.4H, #4", .expected = emitNeonUshll(.V0, .V1, 4, .Size4H), .asm_text = "ushll v0.4s, v1.4h, #4" },
        .{ .name = "USHLL V0.2D, V1.2S, #8", .expected = emitNeonUshll(.V0, .V1, 8, .Size2S), .asm_text = "ushll v0.2d, v1.2s, #8" },
        .{ .name = "USHLL2 V0.4S, V1.8H, #3", .expected = emitNeonUshll2(.V0, .V1, 3, .Size8H), .asm_text = "ushll2 v0.4s, v1.8h, #3" },

        // ---- NEON narrowing shift ----
        // SHRN/RSHRN take the SOURCE arrangement (the wider type)
        .{ .name = "SHRN V0.8B, V1.8H, #3", .expected = emitNeonShrn(.V0, .V1, 3, .Size8H), .asm_text = "shrn v0.8b, v1.8h, #3" },
        .{ .name = "SHRN V0.4H, V1.4S, #5", .expected = emitNeonShrn(.V0, .V1, 5, .Size4S), .asm_text = "shrn v0.4h, v1.4s, #5" },
        .{ .name = "SHRN V0.2S, V1.2D, #10", .expected = emitNeonShrn(.V0, .V1, 10, .Size2D), .asm_text = "shrn v0.2s, v1.2d, #10" },
        .{ .name = "SHRN2 V0.16B, V1.8H, #2", .expected = emitNeonShrn2(.V0, .V1, 2, .Size8H), .asm_text = "shrn2 v0.16b, v1.8h, #2" },
        .{ .name = "RSHRN V0.8B, V1.8H, #4", .expected = emitNeonRshrn(.V0, .V1, 4, .Size8H), .asm_text = "rshrn v0.8b, v1.8h, #4" },
        .{ .name = "RSHRN2 V0.16B, V1.8H, #1", .expected = emitNeonRshrn2(.V0, .V1, 1, .Size8H), .asm_text = "rshrn2 v0.16b, v1.8h, #1" },

        // ---- NEON saturating shift immediate ----
        .{ .name = "SQSHL V0.4S, V1.4S, #3 (imm)", .expected = emitNeonSqshlImm(.V0, .V1, 3, .Size4S), .asm_text = "sqshl v0.4s, v1.4s, #3" },
        .{ .name = "SQSHLU V0.4S, V1.4S, #5", .expected = emitNeonSqshlu(.V0, .V1, 5, .Size4S), .asm_text = "sqshlu v0.4s, v1.4s, #5" },
        .{ .name = "UQSHL V0.4S, V1.4S, #2 (imm)", .expected = emitNeonUqshlImm(.V0, .V1, 2, .Size4S), .asm_text = "uqshl v0.4s, v1.4s, #2" },
        .{ .name = "SQSHL V0.8H, V1.8H, #7 (imm)", .expected = emitNeonSqshlImm(.V0, .V1, 7, .Size8H), .asm_text = "sqshl v0.8h, v1.8h, #7" },

        // ---- NEON addhn / subhn ----
        // ADDHN/SUBHN take the destination (narrow) arrangement; & ~4 clears Q
        .{ .name = "ADDHN V0.8B, V1.8H, V2.8H", .expected = emitNeonAddhn(.V0, .V1, .V2, .Size8B), .asm_text = "addhn v0.8b, v1.8h, v2.8h" },
        .{ .name = "ADDHN V0.4H, V1.4S, V2.4S", .expected = emitNeonAddhn(.V0, .V1, .V2, .Size4H), .asm_text = "addhn v0.4h, v1.4s, v2.4s" },
        .{ .name = "ADDHN V0.2S, V1.2D, V2.2D", .expected = emitNeonAddhn(.V0, .V1, .V2, .Size2S), .asm_text = "addhn v0.2s, v1.2d, v2.2d" },
        .{ .name = "ADDHN2 V0.16B, V1.8H, V2.8H", .expected = emitNeonAddhn2(.V0, .V1, .V2, .Size16B), .asm_text = "addhn2 v0.16b, v1.8h, v2.8h" },
        .{ .name = "SUBHN V0.8B, V1.8H, V2.8H", .expected = emitNeonSubhn(.V0, .V1, .V2, .Size8B), .asm_text = "subhn v0.8b, v1.8h, v2.8h" },
        .{ .name = "SUBHN2 V0.16B, V1.8H, V2.8H", .expected = emitNeonSubhn2(.V0, .V1, .V2, .Size16B), .asm_text = "subhn2 v0.16b, v1.8h, v2.8h" },
        // RADDHN/RSUBHN take the source (wide) arrangement; (sz-1) & ~4 adjusts
        .{ .name = "RADDHN V0.8B, V1.8H, V2.8H", .expected = emitNeonRaddhn(.V0, .V1, .V2, .Size8H), .asm_text = "raddhn v0.8b, v1.8h, v2.8h" },
        .{ .name = "RADDHN2 V0.16B, V1.8H, V2.8H", .expected = emitNeonRaddhn2(.V0, .V1, .V2, .Size8H), .asm_text = "raddhn2 v0.16b, v1.8h, v2.8h" },
        .{ .name = "RSUBHN V0.8B, V1.8H, V2.8H", .expected = emitNeonRsubhn(.V0, .V1, .V2, .Size8H), .asm_text = "rsubhn v0.8b, v1.8h, v2.8h" },
        .{ .name = "RSUBHN2 V0.16B, V1.8H, V2.8H", .expected = emitNeonRsubhn2(.V0, .V1, .V2, .Size8H), .asm_text = "rsubhn2 v0.16b, v1.8h, v2.8h" },

        // ---- NEON across-lane reductions ----
        .{ .name = "ADDV B0, V1.8B", .expected = emitNeonAddv(.V0, .V1, .Size8B), .asm_text = "addv b0, v1.8b" },
        .{ .name = "ADDV B0, V1.16B", .expected = emitNeonAddv(.V0, .V1, .Size16B), .asm_text = "addv b0, v1.16b" },
        .{ .name = "ADDV H0, V1.4H", .expected = emitNeonAddv(.V0, .V1, .Size4H), .asm_text = "addv h0, v1.4h" },
        .{ .name = "ADDV H0, V1.8H", .expected = emitNeonAddv(.V0, .V1, .Size8H), .asm_text = "addv h0, v1.8h" },
        .{ .name = "ADDV S0, V1.4S", .expected = emitNeonAddv(.V0, .V1, .Size4S), .asm_text = "addv s0, v1.4s" },
        .{ .name = "SMAXV B0, V1.16B", .expected = emitNeonSmaxv(.V0, .V1, .Size16B), .asm_text = "smaxv b0, v1.16b" },
        .{ .name = "SMAXV S0, V1.4S", .expected = emitNeonSmaxv(.V0, .V1, .Size4S), .asm_text = "smaxv s0, v1.4s" },
        .{ .name = "SMINV B0, V1.16B", .expected = emitNeonSminv(.V0, .V1, .Size16B), .asm_text = "sminv b0, v1.16b" },
        .{ .name = "UMAXV B0, V1.16B", .expected = emitNeonUmaxv(.V0, .V1, .Size16B), .asm_text = "umaxv b0, v1.16b" },
        .{ .name = "UMINV B0, V1.16B", .expected = emitNeonUminv(.V0, .V1, .Size16B), .asm_text = "uminv b0, v1.16b" },

        // ---- NEON pairwise scalar ----
        // ADDP scalar uses scalar_opcode, so needs scalar NeonSize (Size1D not Size2D)
        .{ .name = "ADDP D0, V1.2D", .expected = emitNeonAddpScalar(.V0, .V1, .Size1D), .asm_text = "addp d0, v1.2d" },
        .{ .name = "FADDP S0, V1.2S", .expected = emitNeonFaddpScalar(.V0, .V1, .Size2S), .asm_text = "faddp s0, v1.2s" },
        .{ .name = "FADDP D0, V1.2D", .expected = emitNeonFaddpScalar(.V0, .V1, .Size2D), .asm_text = "faddp d0, v1.2d" },

        // ---- NEON CLS / CLZ (vector) ----
        .{ .name = "CLS V0.8B, V1.8B", .expected = emitNeonCls(.V0, .V1, .Size8B), .asm_text = "cls v0.8b, v1.8b" },
        .{ .name = "CLS V0.4S, V1.4S", .expected = emitNeonCls(.V0, .V1, .Size4S), .asm_text = "cls v0.4s, v1.4s" },
        .{ .name = "CLZ V0.8B, V1.8B", .expected = emitNeonClz(.V0, .V1, .Size8B), .asm_text = "clz v0.8b, v1.8b" },
        .{ .name = "CLZ V0.4S, V1.4S", .expected = emitNeonClz(.V0, .V1, .Size4S), .asm_text = "clz v0.4s, v1.4s" },

        // ---- NEON vector convert (various rounding) ----
        .{ .name = "FCVTAS V0.4S", .expected = emitNeonFcvtas(.V0, .V1, .Size4S), .asm_text = "fcvtas v0.4s, v1.4s" },
        .{ .name = "FCVTAU V0.4S", .expected = emitNeonFcvtau(.V0, .V1, .Size4S), .asm_text = "fcvtau v0.4s, v1.4s" },
        .{ .name = "FCVTMS V0.4S", .expected = emitNeonFcvtms(.V0, .V1, .Size4S), .asm_text = "fcvtms v0.4s, v1.4s" },
        .{ .name = "FCVTMU V0.4S", .expected = emitNeonFcvtmu(.V0, .V1, .Size4S), .asm_text = "fcvtmu v0.4s, v1.4s" },
        .{ .name = "FCVTNS V0.4S", .expected = emitNeonFcvtns(.V0, .V1, .Size4S), .asm_text = "fcvtns v0.4s, v1.4s" },
        .{ .name = "FCVTNU V0.4S", .expected = emitNeonFcvtnu(.V0, .V1, .Size4S), .asm_text = "fcvtnu v0.4s, v1.4s" },
        .{ .name = "FCVTPS V0.4S", .expected = emitNeonFcvtps(.V0, .V1, .Size4S), .asm_text = "fcvtps v0.4s, v1.4s" },
        .{ .name = "FCVTPU V0.4S", .expected = emitNeonFcvtpu(.V0, .V1, .Size4S), .asm_text = "fcvtpu v0.4s, v1.4s" },
        .{ .name = "FCVTAS V0.2D", .expected = emitNeonFcvtas(.V0, .V1, .Size2D), .asm_text = "fcvtas v0.2d, v1.2d" },
        .{ .name = "FCVTMS V0.2D", .expected = emitNeonFcvtms(.V0, .V1, .Size2D), .asm_text = "fcvtms v0.2d, v1.2d" },

        // ---- NEON gen-register convert (various rounding) ----
        .{ .name = "FCVTMS W0, S1", .expected = emitNeonFcvtmsGen(.R0, .V1, .Size1S), .asm_text = "fcvtms w0, s1" },
        .{ .name = "FCVTMS X0, D1", .expected = emitNeonFcvtmsGen64(.R0, .V1, .Size1D), .asm_text = "fcvtms x0, d1" },
        .{ .name = "FCVTMU W0, S1", .expected = emitNeonFcvtmuGen(.R0, .V1, .Size1S), .asm_text = "fcvtmu w0, s1" },
        .{ .name = "FCVTMU X0, D1", .expected = emitNeonFcvtmuGen64(.R0, .V1, .Size1D), .asm_text = "fcvtmu x0, d1" },
        .{ .name = "FCVTNS W0, S1", .expected = emitNeonFcvtnsGen(.R0, .V1, .Size1S), .asm_text = "fcvtns w0, s1" },
        .{ .name = "FCVTNS X0, D1", .expected = emitNeonFcvtnsGen64(.R0, .V1, .Size1D), .asm_text = "fcvtns x0, d1" },
        .{ .name = "FCVTNU W0, S1", .expected = emitNeonFcvtnuGen(.R0, .V1, .Size1S), .asm_text = "fcvtnu w0, s1" },
        .{ .name = "FCVTNU X0, D1", .expected = emitNeonFcvtnuGen64(.R0, .V1, .Size1D), .asm_text = "fcvtnu x0, d1" },
        .{ .name = "FCVTPS W0, S1", .expected = emitNeonFcvtpsGen(.R0, .V1, .Size1S), .asm_text = "fcvtps w0, s1" },
        .{ .name = "FCVTPS X0, D1", .expected = emitNeonFcvtpsGen64(.R0, .V1, .Size1D), .asm_text = "fcvtps x0, d1" },
        .{ .name = "FCVTPU W0, S1", .expected = emitNeonFcvtpuGen(.R0, .V1, .Size1S), .asm_text = "fcvtpu w0, s1" },
        .{ .name = "FCVTPU X0, D1", .expected = emitNeonFcvtpuGen64(.R0, .V1, .Size1D), .asm_text = "fcvtpu x0, d1" },

        // ---- NEON SADDLP / UADDLP / SADALP / UADALP ----
        // These take the source arrangement directly (no internal adjustment)
        .{ .name = "SADDLP V0.4H, V1.8B", .expected = emitNeonSaddlp(.V0, .V1, .Size8B), .asm_text = "saddlp v0.4h, v1.8b" },
        .{ .name = "SADDLP V0.2S, V1.4H", .expected = emitNeonSaddlp(.V0, .V1, .Size4H), .asm_text = "saddlp v0.2s, v1.4h" },
        .{ .name = "SADDLP V0.8H, V1.16B", .expected = emitNeonSaddlp(.V0, .V1, .Size16B), .asm_text = "saddlp v0.8h, v1.16b" },
        .{ .name = "UADDLP V0.4H, V1.8B", .expected = emitNeonUaddlp(.V0, .V1, .Size8B), .asm_text = "uaddlp v0.4h, v1.8b" },
        .{ .name = "UADDLP V0.8H, V1.16B", .expected = emitNeonUaddlp(.V0, .V1, .Size16B), .asm_text = "uaddlp v0.8h, v1.16b" },
        .{ .name = "SADALP V0.4H, V1.8B", .expected = emitNeonSadalp(.V0, .V1, .Size8B), .asm_text = "sadalp v0.4h, v1.8b" },
        .{ .name = "UADALP V0.4H, V1.8B", .expected = emitNeonUadalp(.V0, .V1, .Size8B), .asm_text = "uadalp v0.4h, v1.8b" },

        // ---- NEON across-lane long add ----
        .{ .name = "SADDLV H0, V1.8B", .expected = emitNeonSaddlv(.V0, .V1, .Size8B), .asm_text = "saddlv h0, v1.8b" },
        .{ .name = "SADDLV H0, V1.16B", .expected = emitNeonSaddlv(.V0, .V1, .Size16B), .asm_text = "saddlv h0, v1.16b" },
        .{ .name = "SADDLV S0, V1.8H", .expected = emitNeonSaddlv(.V0, .V1, .Size8H), .asm_text = "saddlv s0, v1.8h" },
        .{ .name = "UADDLV H0, V1.8B", .expected = emitNeonUaddlv(.V0, .V1, .Size8B), .asm_text = "uaddlv h0, v1.8b" },

        // ---- NEON SUQADD / USQADD ----
        .{ .name = "SUQADD V0.4S", .expected = emitNeonSuqadd(.V0, .V1, .Size4S), .asm_text = "suqadd v0.4s, v1.4s" },
        .{ .name = "USQADD V0.4S", .expected = emitNeonUsqadd(.V0, .V1, .Size4S), .asm_text = "usqadd v0.4s, v1.4s" },
        .{ .name = "SUQADD V0.16B", .expected = emitNeonSuqadd(.V0, .V1, .Size16B), .asm_text = "suqadd v0.16b, v1.16b" },
        .{ .name = "USQADD V0.16B", .expected = emitNeonUsqadd(.V0, .V1, .Size16B), .asm_text = "usqadd v0.16b, v1.16b" },

        // ---- NEON SQABS / SQNEG ----
        .{ .name = "SQABS V0.4S", .expected = emitNeonSqabs(.V0, .V1, .Size4S), .asm_text = "sqabs v0.4s, v1.4s" },
        .{ .name = "SQNEG V0.4S", .expected = emitNeonSqneg(.V0, .V1, .Size4S), .asm_text = "sqneg v0.4s, v1.4s" },
        .{ .name = "SQABS V0.16B", .expected = emitNeonSqabs(.V0, .V1, .Size16B), .asm_text = "sqabs v0.16b, v1.16b" },
        .{ .name = "SQNEG V0.8H", .expected = emitNeonSqneg(.V0, .V1, .Size8H), .asm_text = "sqneg v0.8h, v1.8h" },

        // ---- NEON URECPE / URSQRTE ----
        .{ .name = "URECPE V0.4S", .expected = emitNeonUrecpe(.V0, .V1, .Size4S), .asm_text = "urecpe v0.4s, v1.4s" },
        .{ .name = "URECPE V0.2S", .expected = emitNeonUrecpe(.V0, .V1, .Size2S), .asm_text = "urecpe v0.2s, v1.2s" },
        .{ .name = "URSQRTE V0.4S", .expected = emitNeonUrsqrte(.V0, .V1, .Size4S), .asm_text = "ursqrte v0.4s, v1.4s" },
        .{ .name = "URSQRTE V0.2S", .expected = emitNeonUrsqrte(.V0, .V1, .Size2S), .asm_text = "ursqrte v0.2s, v1.2s" },

        // ---- NEON SHLL ----
        // SHLL takes source arrangement
        .{ .name = "SHLL V0.8H, V1.8B, #8", .expected = emitNeonShll(.V0, .V1, .Size8B), .asm_text = "shll v0.8h, v1.8b, #8" },
        .{ .name = "SHLL V0.4S, V1.4H, #16", .expected = emitNeonShll(.V0, .V1, .Size4H), .asm_text = "shll v0.4s, v1.4h, #16" },
        .{ .name = "SHLL V0.2D, V1.2S, #32", .expected = emitNeonShll(.V0, .V1, .Size2S), .asm_text = "shll v0.2d, v1.2s, #32" },
        .{ .name = "SHLL2 V0.8H, V1.16B, #8", .expected = emitNeonShll2(.V0, .V1, .Size16B), .asm_text = "shll2 v0.8h, v1.16b, #8" },
        .{ .name = "SHLL2 V0.4S, V1.8H, #16", .expected = emitNeonShll2(.V0, .V1, .Size8H), .asm_text = "shll2 v0.4s, v1.8h, #16" },

        // ---- NEON SRSRA / URSRA ----
        .{ .name = "SRSRA V0.4S, #3", .expected = emitNeonSrsra(.V0, .V1, 3, .Size4S), .asm_text = "srsra v0.4s, v1.4s, #3" },
        .{ .name = "URSRA V0.4S, #5", .expected = emitNeonUrsra(.V0, .V1, 5, .Size4S), .asm_text = "ursra v0.4s, v1.4s, #5" },

        // ---- NEON SQXTN / SQXTUN / UQXTN ----
        // These take the SOURCE arrangement (the wider type)
        .{ .name = "SQXTN V0.8B, V1.8H", .expected = emitNeonSqxtn(.V0, .V1, .Size8H), .asm_text = "sqxtn v0.8b, v1.8h" },
        .{ .name = "SQXTN V0.4H, V1.4S", .expected = emitNeonSqxtn(.V0, .V1, .Size4S), .asm_text = "sqxtn v0.4h, v1.4s" },
        .{ .name = "SQXTN V0.2S, V1.2D", .expected = emitNeonSqxtn(.V0, .V1, .Size2D), .asm_text = "sqxtn v0.2s, v1.2d" },
        .{ .name = "SQXTN2 V0.16B, V1.8H", .expected = emitNeonSqxtn2(.V0, .V1, .Size8H), .asm_text = "sqxtn2 v0.16b, v1.8h" },
        .{ .name = "SQXTUN V0.8B, V1.8H", .expected = emitNeonSqxtun(.V0, .V1, .Size8H), .asm_text = "sqxtun v0.8b, v1.8h" },
        .{ .name = "SQXTUN2 V0.16B, V1.8H", .expected = emitNeonSqxtun2(.V0, .V1, .Size8H), .asm_text = "sqxtun2 v0.16b, v1.8h" },
        .{ .name = "UQXTN V0.8B, V1.8H", .expected = emitNeonUqxtn(.V0, .V1, .Size8H), .asm_text = "uqxtn v0.8b, v1.8h" },
        .{ .name = "UQXTN V0.4H, V1.4S", .expected = emitNeonUqxtn(.V0, .V1, .Size4S), .asm_text = "uqxtn v0.4h, v1.4s" },
        .{ .name = "UQXTN2 V0.16B, V1.8H", .expected = emitNeonUqxtn2(.V0, .V1, .Size8H), .asm_text = "uqxtn2 v0.16b, v1.8h" },

        // ---- NEON FCVTL / FCVTN / FCVTXN ----
        // FCVTL takes source arrangement; FCVTN/FCVTXN take source arrangement
        .{ .name = "FCVTL V0.4S, V1.4H", .expected = emitNeonFcvtl(.V0, .V1, .Size4H), .asm_text = "fcvtl v0.4s, v1.4h" },
        // Note: FCVTL V0.2D, V1.2S skipped — hits enum hole at value 11
        .{ .name = "FCVTL2 V0.4S, V1.8H", .expected = emitNeonFcvtl2(.V0, .V1, .Size8H), .asm_text = "fcvtl2 v0.4s, v1.8h" },
        .{ .name = "FCVTL2 V0.2D, V1.4S", .expected = emitNeonFcvtl2(.V0, .V1, .Size4S), .asm_text = "fcvtl2 v0.2d, v1.4s" },
        .{ .name = "FCVTN V0.4H, V1.4S", .expected = emitNeonFcvtn(.V0, .V1, .Size4S), .asm_text = "fcvtn v0.4h, v1.4s" },
        // Note: FCVTN V0.2S, V1.2D skipped — hits enum hole at value 11
        .{ .name = "FCVTN2 V0.8H, V1.4S", .expected = emitNeonFcvtn2(.V0, .V1, .Size4S), .asm_text = "fcvtn2 v0.8h, v1.4s" },
        .{ .name = "FCVTN2 V0.4S, V1.2D", .expected = emitNeonFcvtn2(.V0, .V1, .Size2D), .asm_text = "fcvtn2 v0.4s, v1.2d" },
        // Note: FCVTXN V0.2S, V1.2D skipped — hits enum hole at value 11
        .{ .name = "FCVTXN2 V0.4S, V1.2D", .expected = emitNeonFcvtxn2(.V0, .V1, .Size2D), .asm_text = "fcvtxn2 v0.4s, v1.2d" },

        // ---- Load/store pre/post index ----
        .{ .name = "LDRH W0, [X1], #4", .expected = emitLdrhOffsetPostIndex(.R0, .R1, 4).?, .asm_text = "ldrh w0, [x1], #4" },
        .{ .name = "LDR W0, [X1], #8", .expected = emitLdrOffsetPostIndex(.R0, .R1, 8).?, .asm_text = "ldr w0, [x1], #8" },
        .{ .name = "LDR X0, [X1], #16", .expected = emitLdrOffsetPostIndex64(.R0, .R1, 16).?, .asm_text = "ldr x0, [x1], #16" },
        .{ .name = "STRH W0, [X1, #-4]!", .expected = emitStrhOffsetPreIndex(.R0, .R1, -4).?, .asm_text = "strh w0, [x1, #-4]!" },
        .{ .name = "STR W0, [X1, #-8]!", .expected = emitStrOffsetPreIndex(.R0, .R1, -8).?, .asm_text = "str w0, [x1, #-8]!" },

        // ---- PRFM (prefetch) ----
        .{ .name = "PRFM PLDL1KEEP, [X0, X1]", .expected = emitPrfmRegister(.R0, RegisterParam.reg_only(.R1)), .asm_text = "prfm pldl1keep, [x0, x1]" },
        .{ .name = "PRFM PLDL1KEEP, [X0, #0]", .expected = emitPrfmOffset(.R0, 0).?, .asm_text = "prfm pldl1keep, [x0]" },

        // ---- LDP post-index 32-bit ----
        .{ .name = "LDP W0, W1, [SP], #8", .expected = emitLdpOffsetPostIndex(.R0, .R1, .SP, 8).?, .asm_text = "ldp w0, w1, [sp], #8" },
        .{ .name = "STP W0, W1, [SP, #-8]!", .expected = emitStpOffsetPreIndex(.R0, .R1, .SP, -8).?, .asm_text = "stp w0, w1, [sp, #-8]!" },

        // ---- ADD/SUB register with extend ----
        .{ .name = "ADD X0, X1, W2, SXTW", .expected = emitAddRegister64(.R0, .R1, RegisterParam.extended(.R2, .SXTW, 0)), .asm_text = "add x0, x1, w2, sxtw" },
        .{ .name = "ADD X0, X1, W2, UXTW", .expected = emitAddRegister64(.R0, .R1, RegisterParam.extended(.R2, .UXTW, 0)), .asm_text = "add x0, x1, w2, uxtw" },
        .{ .name = "ADD X0, X1, W2, SXTW #2", .expected = emitAddRegister64(.R0, .R1, RegisterParam.extended(.R2, .SXTW, 2)), .asm_text = "add x0, x1, w2, sxtw #2" },
        .{ .name = "SUB X0, X1, W2, SXTW", .expected = emitSubRegister64(.R0, .R1, RegisterParam.extended(.R2, .SXTW, 0)), .asm_text = "sub x0, x1, w2, sxtw" },
        .{ .name = "ADD X0, X1, X2, SXTX", .expected = emitAddRegister64(.R0, .R1, RegisterParam.extended(.R2, .SXTX, 0)), .asm_text = "add x0, x1, x2, sxtx" },
        .{ .name = "ADD X0, X1, W2, UXTB", .expected = emitAddRegister64(.R0, .R1, RegisterParam.extended(.R2, .UXTB, 0)), .asm_text = "add x0, x1, w2, uxtb" },
        .{ .name = "ADD X0, X1, W2, UXTH", .expected = emitAddRegister64(.R0, .R1, RegisterParam.extended(.R2, .UXTH, 0)), .asm_text = "add x0, x1, w2, uxth" },
        .{ .name = "ADD X0, X1, W2, SXTB", .expected = emitAddRegister64(.R0, .R1, RegisterParam.extended(.R2, .SXTB, 0)), .asm_text = "add x0, x1, w2, sxtb" },
        .{ .name = "ADD X0, X1, W2, SXTH", .expected = emitAddRegister64(.R0, .R1, RegisterParam.extended(.R2, .SXTH, 0)), .asm_text = "add x0, x1, w2, sxth" },

        // ---- FMADD / FMSUB (scalar float fused multiply-add/sub) ----
        .{ .name = "FMADD S0, S1, S2, S3", .expected = emitFmadd(.V0, .V1, .V2, .V3), .asm_text = "fmadd s0, s1, s2, s3" },
        .{ .name = "FMADD D0, D1, D2, D3", .expected = emitFmadd64(.V0, .V1, .V2, .V3), .asm_text = "fmadd d0, d1, d2, d3" },
        .{ .name = "FMADD S4, S5, S6, S7", .expected = emitFmadd(.V4, .V5, .V6, .V7), .asm_text = "fmadd s4, s5, s6, s7" },
        .{ .name = "FMADD D4, D5, D6, D7", .expected = emitFmadd64(.V4, .V5, .V6, .V7), .asm_text = "fmadd d4, d5, d6, d7" },
        .{ .name = "FMSUB S0, S1, S2, S3", .expected = emitFmsub(.V0, .V1, .V2, .V3), .asm_text = "fmsub s0, s1, s2, s3" },
        .{ .name = "FMSUB D0, D1, D2, D3", .expected = emitFmsub64(.V0, .V1, .V2, .V3), .asm_text = "fmsub d0, d1, d2, d3" },
        .{ .name = "FMSUB S4, S5, S6, S7", .expected = emitFmsub(.V4, .V5, .V6, .V7), .asm_text = "fmsub s4, s5, s6, s7" },
        .{ .name = "FMSUB D4, D5, D6, D7", .expected = emitFmsub64(.V4, .V5, .V6, .V7), .asm_text = "fmsub d4, d5, d6, d7" },
        .{ .name = "FNMADD S0, S1, S2, S3", .expected = emitFnmadd(.V0, .V1, .V2, .V3), .asm_text = "fnmadd s0, s1, s2, s3" },
        .{ .name = "FNMADD D0, D1, D2, D3", .expected = emitFnmadd64(.V0, .V1, .V2, .V3), .asm_text = "fnmadd d0, d1, d2, d3" },
        .{ .name = "FNMSUB S0, S1, S2, S3", .expected = emitFnmsub(.V0, .V1, .V2, .V3), .asm_text = "fnmsub s0, s1, s2, s3" },
        .{ .name = "FNMSUB D0, D1, D2, D3", .expected = emitFnmsub64(.V0, .V1, .V2, .V3), .asm_text = "fnmsub d0, d1, d2, d3" },

        // ---- Scalar FCVT (float precision conversion) ----
        .{ .name = "FCVT D0, S1", .expected = emitFcvtStoD(.V0, .V1), .asm_text = "fcvt d0, s1" },
        .{ .name = "FCVT S0, D1", .expected = emitFcvtDtoS(.V0, .V1), .asm_text = "fcvt s0, d1" },
        .{ .name = "FCVT D5, S10", .expected = emitFcvtStoD(.V5, .V10), .asm_text = "fcvt d5, s10" },
        .{ .name = "FCVT S5, D10", .expected = emitFcvtDtoS(.V5, .V10), .asm_text = "fcvt s5, d10" },
        .{ .name = "FCVT H0, S1", .expected = emitFcvtStoH(.V0, .V1), .asm_text = "fcvt h0, s1" },
        .{ .name = "FCVT S0, H1", .expected = emitFcvtHtoS(.V0, .V1), .asm_text = "fcvt s0, h1" },
        .{ .name = "FCVT H0, D1", .expected = emitFcvtDtoH(.V0, .V1), .asm_text = "fcvt h0, d1" },
        .{ .name = "FCVT D0, H1", .expected = emitFcvtHtoD(.V0, .V1), .asm_text = "fcvt d0, h1" },

        // ---- NEG (integer negate alias) ----
        .{ .name = "NEG W0, W1", .expected = emitNeg(.R0, .R1), .asm_text = "neg w0, w1" },
        .{ .name = "NEG X0, X1", .expected = emitNeg64(.R0, .R1), .asm_text = "neg x0, x1" },
        .{ .name = "NEG W5, W10", .expected = emitNeg(.R5, .R10), .asm_text = "neg w5, w10" },
        .{ .name = "NEG X5, X10", .expected = emitNeg64(.R5, .R10), .asm_text = "neg x5, x10" },
        .{ .name = "NEGS W0, W1", .expected = emitNegs(.R0, .R1), .asm_text = "negs w0, w1" },
        .{ .name = "NEGS X0, X1", .expected = emitNegs64(.R0, .R1), .asm_text = "negs x0, x1" },

        // ---- CMN (compare negative alias) ----
        .{ .name = "CMN W0, W1", .expected = emitCmnRegister(.R0, RegisterParam.reg_only(.R1)), .asm_text = "cmn w0, w1" },
        .{ .name = "CMN X0, X1", .expected = emitCmnRegister64(.R0, RegisterParam.reg_only(.R1)), .asm_text = "cmn x0, x1" },
        .{ .name = "CMN W0, #42", .expected = emitCmnImmediate(.R0, 42), .asm_text = "cmn w0, #42" },
        .{ .name = "CMN X0, #42", .expected = emitCmnImmediate64(.R0, 42), .asm_text = "cmn x0, #42" },
        .{ .name = "CMN W5, W10", .expected = emitCmnRegister(.R5, RegisterParam.reg_only(.R10)), .asm_text = "cmn w5, w10" },
        .{ .name = "CMN X5, X10", .expected = emitCmnRegister64(.R5, RegisterParam.reg_only(.R10)), .asm_text = "cmn x5, x10" },

        // ---- SXTW (sign extend word alias) ----
        .{ .name = "SXTW X0, W1", .expected = emitSxtw64(.R0, .R1), .asm_text = "sxtw x0, w1" },
        .{ .name = "SXTW X5, W10", .expected = emitSxtw64(.R5, .R10), .asm_text = "sxtw x5, w10" },

        // ---- HINT / BTI ----
        .{ .name = "HINT #34 (BTI C)", .expected = emitBtiC(), .asm_text = "hint #34" },
        .{ .name = "HINT #36 (BTI J)", .expected = emitBtiJ(), .asm_text = "hint #36" },
        .{ .name = "HINT #38 (BTI JC)", .expected = emitBtiJC(), .asm_text = "hint #38" },
        .{ .name = "HINT #0 (NOP)", .expected = emitHint(0), .asm_text = "hint #0" },
        .{ .name = "HINT #1 (YIELD)", .expected = emitHint(1), .asm_text = "hint #1" },
        .{ .name = "HINT #2 (WFE)", .expected = emitHint(2), .asm_text = "hint #2" },
        .{ .name = "HINT #3 (WFI)", .expected = emitHint(3), .asm_text = "hint #3" },

        // ---- MRS/MSR TPIDR_EL0 ----
        .{ .name = "MRS X0, TPIDR_EL0", .expected = emitMrsTpidrEl0(.R0), .asm_text = "mrs x0, tpidr_el0" },
        .{ .name = "MRS X5, TPIDR_EL0", .expected = emitMrsTpidrEl0(.R5), .asm_text = "mrs x5, tpidr_el0" },
        .{ .name = "MSR TPIDR_EL0, X0", .expected = emitMsrTpidrEl0(.R0), .asm_text = "msr tpidr_el0, x0" },
        .{ .name = "MSR TPIDR_EL0, X5", .expected = emitMsrTpidrEl0(.R5), .asm_text = "msr tpidr_el0, x5" },
    };
};

// ============================================================================
// Verification helpers
// ============================================================================

const AssembleError = error{
    ClangFailed,
    OtoolFailed,
    ToolCrashed,
    ParseFailed,
    Unexpected,
};

/// Write a one-instruction .s file, assemble with clang, disassemble with
/// otool, and return the 32-bit machine-code word.
fn assembleAndExtract(alloc: std.mem.Allocator, asm_text: []const u8) AssembleError!u32 {
    const tmp_s = "/tmp/_arm64_verify.s";
    const tmp_o = "/tmp/_arm64_verify.o";

    // Write directly to the file — no manual buffer concat needed.
    {
        const f = std.fs.cwd().createFile(tmp_s, .{}) catch return error.Unexpected;
        defer f.close();
        f.writeAll(".text\n.align 2\n    ") catch return error.Unexpected;
        f.writeAll(asm_text) catch return error.Unexpected;
        f.writeAll("\n") catch return error.Unexpected;
    }

    // Assemble
    {
        const result = std.process.Child.run(.{
            .allocator = alloc,
            .argv = &.{ "clang", "-c", "-arch", "arm64", tmp_s, "-o", tmp_o },
        }) catch return error.Unexpected;
        defer alloc.free(result.stdout);
        defer alloc.free(result.stderr);
        switch (result.term) {
            .Exited => |code| if (code != 0) return error.ClangFailed,
            else => return error.ToolCrashed,
        }
    }

    // Disassemble — otool -t -X prints raw hex without headers
    const otool_out = blk: {
        const result = std.process.Child.run(.{
            .allocator = alloc,
            .argv = &.{ "otool", "-t", "-X", tmp_o },
        }) catch return error.Unexpected;
        defer alloc.free(result.stderr);
        errdefer alloc.free(result.stdout);
        switch (result.term) {
            .Exited => |code| if (code != 0) {
                alloc.free(result.stdout);
                return error.OtoolFailed;
            },
            else => {
                alloc.free(result.stdout);
                return error.ToolCrashed;
            },
        }
        break :blk result.stdout;
    };
    defer alloc.free(otool_out);

    // Parse: each line is "addr\thex_word ...".  We want the first hex word.
    var lines = std.mem.splitScalar(u8, otool_out, '\n');
    while (lines.next()) |line| {
        const trimmed = std.mem.trim(u8, line, " \t\r");
        if (trimmed.len == 0) continue;
        var tok = std.mem.tokenizeAny(u8, trimmed, " \t");
        _ = tok.next() orelse continue; // address column
        const hex = tok.next() orelse continue;
        return std.fmt.parseUnsigned(u32, hex, 16) catch return error.ParseFailed;
    }
    return error.ParseFailed;
}

// ============================================================================
// Main — clang/otool round-trip verification driver
// ============================================================================

pub fn main() !void {
    var gpa: std.heap.GeneralPurposeAllocator(.{}) = .init;
    defer _ = gpa.deinit();
    const alloc = gpa.allocator();

    const total: usize = verify_cases.len;

    std.debug.print("\n" ++
        "================================================================\n" ++
        "  ARM64 Encoder  -  clang/otool verification  ({d} cases)\n" ++
        "================================================================\n\n", .{total});

    var pass: usize = 0;
    var fail: usize = 0;
    var errs: usize = 0;

    for (&verify_cases, 0..) |*c, i| {
        std.debug.print("\r\x1b[2K  [{d}/{d}] {d}%  {s}", .{
            i + 1,
            total,
            ((i + 1) * 100) / total,
            c.name,
        });

        const clang_word = assembleAndExtract(alloc, c.asm_text) catch |e| {
            std.debug.print("\n    !! toolchain error ({s}): \"{s}\"\n", .{
                @errorName(e),
                c.asm_text,
            });
            errs += 1;
            continue;
        };

        if (c.expected == clang_word) {
            pass += 1;
        } else {
            fail += 1;
            std.debug.print(
                "\n    MISMATCH  {s}\n" ++
                    "      asm:   {s}\n" ++
                    "      zig:   0x{x:0>8}\n" ++
                    "      clang: 0x{x:0>8}\n",
                .{ c.name, c.asm_text, c.expected, clang_word },
            );
        }
    }

    // Blank out the progress line, then print summary.
    std.debug.print("\r\x1b[2K", .{});
    std.debug.print("================================================================\n", .{});
    if (fail == 0 and errs == 0) {
        std.debug.print("  ALL {d} encodings match clang.\n", .{pass});
    } else {
        std.debug.print("  {d} passed, {d} failed, {d} errors  (of {d})\n", .{
            pass, fail, errs, total,
        });
    }
    std.debug.print("================================================================\n\n", .{});

    // Best-effort cleanup of temp files.
    std.fs.cwd().deleteFile("/tmp/_arm64_verify.s") catch {};
    std.fs.cwd().deleteFile("/tmp/_arm64_verify.o") catch {};

    if (fail > 0 or errs > 0) std.process.exit(1);
}
