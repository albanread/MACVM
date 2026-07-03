//! jit_encode.zig — JitInst[] → ARM64 machine code encoder
//!
//! This module bridges QBE's JitCollector output (a flat array of JitInst
//! records) and the ARM64 instruction encoder (arm64_encoder.zig).
//!
//! Architecture:
//!   JitCollector.insts[0..ninst]
//!     → jitEncode()
//!       → for each JitInst: mapRegisters() + dispatch to emit*()
//!       → write u32 into code buffer
//!       → record labels, branch fixups, external calls
//!     → resolveFixups()
//!       → patch forward branches via ArmBranchLinker
//!     → JitModule { .code, .data, .symbols, .source_map }
//!
//! Design:
//!   - The encoder (arm64_encoder.zig) is stateless — pure u32-returning functions.
//!   - This module manages the mutable state: buffers, label tables, fixup lists.
//!   - All errors are collected (not fatal) so we can report multiple issues at once.
//!   - Introspection/dump utilities are built in from the start.

const std = @import("std");
const enc = @import("arm64_encoder.zig");
const capstone = @import("jit_capstone.zig");

const Register = enc.Register;
const NeonRegister = enc.NeonRegister;
const Condition = enc.Condition;
const ShiftType = enc.ShiftType;
const NeonSize = enc.NeonSize;
const RegisterParam = enc.RegisterParam;
const NeonRegisterParam = enc.NeonRegisterParam;
const ArmBranchLinker = enc.ArmBranchLinker;
const BranchClass = enc.BranchClass;

// ============================================================================
// Section: JitInst C ABI Mirror Types
//
// These mirror the C definitions in jit_collect.h so we can read the
// JitCollector output without @cImport. The layout is extern struct
// compatible with the C side.
// ============================================================================

pub const JIT_SYM_MAX = 80;

/// Mirrors enum JitInstKind from jit_collect.h.
/// We use a raw u16 in the struct and cast on use.
pub const JitInstKind = enum(u16) {
    // Pseudo-instructions
    JIT_LABEL = 0,
    JIT_FUNC_BEGIN = 1,
    JIT_FUNC_END = 2,
    JIT_DBGLOC = 3,
    JIT_NOP = 4,
    JIT_COMMENT = 5,

    // Register-register ALU (3-operand)
    JIT_ADD_RRR = 16,
    JIT_SUB_RRR = 17,
    JIT_MUL_RRR = 18,
    JIT_SDIV_RRR = 19,
    JIT_UDIV_RRR = 20,
    JIT_AND_RRR = 21,
    JIT_ORR_RRR = 22,
    JIT_EOR_RRR = 23,
    JIT_LSL_RRR = 24,
    JIT_LSR_RRR = 25,
    JIT_ASR_RRR = 26,
    JIT_NEG_RR = 27,

    // Fused multiply
    JIT_MSUB_RRRR = 32,
    JIT_MADD_RRRR = 33,

    // Register-immediate ALU
    JIT_ADD_RRI = 48,
    JIT_SUB_RRI = 49,

    // Move / constant loading
    JIT_MOV_RR = 64,
    JIT_MOVZ = 65,
    JIT_MOVK = 66,
    JIT_MOVN = 67,
    JIT_MOV_WIDE_IMM = 68,

    // Floating point register-register
    JIT_FADD_RRR = 80,
    JIT_FSUB_RRR = 81,
    JIT_FMUL_RRR = 82,
    JIT_FDIV_RRR = 83,
    JIT_FNEG_RR = 84,
    JIT_FMOV_RR = 85,

    // Float <-> Int conversions
    JIT_FCVT_SD = 96,
    JIT_FCVT_DS = 97,
    JIT_FCVTZS = 98,
    JIT_FCVTZU = 99,
    JIT_SCVTF = 100,
    JIT_UCVTF = 101,
    JIT_FMOV_GF = 102,
    JIT_FMOV_FG = 103,

    // Extensions
    JIT_SXTB = 112,
    JIT_UXTB = 113,
    JIT_SXTH = 114,
    JIT_UXTH = 115,
    JIT_SXTW = 116,
    JIT_UXTW = 117,

    // Compare
    JIT_CMP_RR = 128,
    JIT_CMP_RI = 129,
    JIT_CMN_RR = 130,
    JIT_FCMP_RR = 131,
    JIT_TST_RR = 132,

    // Conditional
    JIT_CSET = 144,
    JIT_CSEL = 145,

    // Load with register + immediate offset
    JIT_LDR_RI = 160,
    JIT_LDRB_RI = 161,
    JIT_LDRH_RI = 162,
    JIT_LDRSB_RI = 163,
    JIT_LDRSH_RI = 164,
    JIT_LDRSW_RI = 165,

    // Store with register + immediate offset
    JIT_STR_RI = 176,
    JIT_STRB_RI = 177,
    JIT_STRH_RI = 178,

    // Load/store with register + register offset
    JIT_LDR_RR = 192,
    JIT_STR_RR = 193,
    JIT_LDRB_RR = 194,
    JIT_LDRH_RR = 195,
    JIT_LDRSB_RR = 196,
    JIT_LDRSH_RR = 197,
    JIT_LDRSW_RR = 198,
    JIT_STRB_RR = 199,
    JIT_STRH_RR = 200,

    // Load/store pair
    JIT_LDP = 208,
    JIT_STP = 209,
    JIT_LDP_POST = 210,
    JIT_STP_PRE = 211,

    // Branch unconditional
    JIT_B = 224,
    JIT_BL = 225,

    // Branch conditional
    JIT_B_COND = 226,

    // Compare and branch
    JIT_CBZ = 227,
    JIT_CBNZ = 228,

    // Branch register
    JIT_BR = 232,
    JIT_BLR = 233,
    JIT_RET = 234,

    // Call external
    JIT_CALL_EXT = 240,

    // PC-relative
    JIT_ADRP = 248,
    JIT_ADR = 249,

    // Address of symbol
    JIT_LOAD_ADDR = 252,

    // Stack manipulation
    JIT_SUB_SP = 256,
    JIT_ADD_SP = 257,
    JIT_MOV_SP = 258,

    // Special
    JIT_HINT = 264,
    JIT_BRK = 265,

    // NEON vector
    JIT_NEON_LDR_Q = 272,
    JIT_NEON_STR_Q = 273,
    JIT_NEON_ADD = 274,
    JIT_NEON_SUB = 275,
    JIT_NEON_MUL = 276,
    JIT_NEON_DIV = 277,
    JIT_NEON_NEG = 278,
    JIT_NEON_ABS = 279,
    JIT_NEON_FMA = 280,
    JIT_NEON_MIN = 281,
    JIT_NEON_MAX = 282,
    JIT_NEON_DUP = 283,
    JIT_NEON_ADDV = 284,

    // Fused shifted-operand ALU
    JIT_ADD_SHIFT = 296,
    JIT_SUB_SHIFT = 297,
    JIT_AND_SHIFT = 298,
    JIT_ORR_SHIFT = 299,
    JIT_EOR_SHIFT = 300,

    // Data directives
    JIT_DATA_START = 320,
    JIT_DATA_END = 321,
    JIT_DATA_BYTE = 322,
    JIT_DATA_HALF = 323,
    JIT_DATA_WORD = 324,
    JIT_DATA_QUAD = 325,
    JIT_DATA_ZERO = 326,
    JIT_DATA_SYMREF = 327,
    JIT_DATA_ASCII = 328,
    JIT_DATA_ALIGN = 329,

    _,
};

pub const JitCls = enum(u8) {
    W = 0, // 32-bit integer
    L = 1, // 64-bit integer
    S = 2, // 32-bit float (single)
    D = 3, // 64-bit float (double)
};

pub const JitCond = enum(u8) {
    EQ = 0x0,
    NE = 0x1,
    CS = 0x2,
    CC = 0x3,
    MI = 0x4,
    PL = 0x5,
    VS = 0x6,
    VC = 0x7,
    HI = 0x8,
    LS = 0x9,
    GE = 0xA,
    LT = 0xB,
    GT = 0xC,
    LE = 0xD,
    AL = 0xE,
    NV = 0xF,
};

pub const JitShift = enum(u8) {
    LSL = 0,
    LSR = 1,
    ASR = 2,
    ROR = 3,
};

pub const JitNeonArr = enum(u8) {
    NEON_4S = 0,
    NEON_2D = 1,
    NEON_4SF = 2,
    NEON_2DF = 3,
    NEON_8H = 4,
    NEON_16B = 5,
};

pub const JitSymType = enum(u8) {
    NONE = 0,
    GLOBAL = 1,
    THREAD_LOCAL = 2,
    DATA = 3,
    FUNC = 4,
};

// Register sentinel values (must match jit_collect.h)
pub const JIT_REG_NONE: i32 = -1;
pub const JIT_REG_SP: i32 = -2;
pub const JIT_REG_FP: i32 = -3;
pub const JIT_REG_LR: i32 = -4;
pub const JIT_REG_IP0: i32 = -5;
pub const JIT_REG_IP1: i32 = -6;
pub const JIT_VREG_BASE: i32 = -100;

/// The flat instruction record — mirrors C struct JitInst exactly.
/// Layout must be extern-compatible with the C side.
pub const JitInst = extern struct {
    kind: u16,
    cls: u8,
    cond: u8,
    shift_type: u8,
    sym_type: u8,
    is_float: u8,
    _pad1: u8,

    rd: i32,
    rn: i32,
    rm: i32,
    ra: i32,

    imm: i64,
    imm2: i64,

    target_id: i32,
    _pad2: i32,

    sym_name: [JIT_SYM_MAX]u8,

    pub fn getKind(self: *const JitInst) JitInstKind {
        return @enumFromInt(self.kind);
    }

    pub fn getCls(self: *const JitInst) JitCls {
        return @enumFromInt(self.cls);
    }

    pub fn getCond(self: *const JitInst) JitCond {
        return @enumFromInt(self.cond);
    }

    pub fn getShift(self: *const JitInst) JitShift {
        return @enumFromInt(self.shift_type);
    }

    pub fn getSymName(self: *const JitInst) []const u8 {
        const name = &self.sym_name;
        var len: usize = 0;
        while (len < JIT_SYM_MAX and name[len] != 0) : (len += 1) {}
        return name[0..len];
    }

    /// Returns true if this is a 64-bit integer class.
    pub fn is64(self: *const JitInst) bool {
        return self.cls == @intFromEnum(JitCls.L);
    }

    /// Returns true if this is a float class (single or double).
    pub fn isFloat(self: *const JitInst) bool {
        return self.cls >= @intFromEnum(JitCls.S);
    }

    /// Returns true if this is a double-precision float.
    pub fn isDouble(self: *const JitInst) bool {
        return self.cls == @intFromEnum(JitCls.D);
    }
};

// ============================================================================
// Section: Encoding Errors & Diagnostics
// ============================================================================

pub const EncodeError = error{
    InvalidRegister,
    InvalidInstruction,
    UnencodableImmediate,
    BranchOutOfRange,
    BufferOverflow,
    UnresolvedLabel,
    UnresolvedSymbol,
    UnsupportedInstruction,
    InternalError,
    OutOfMemory,
};

pub const DiagSeverity = enum {
    Info,
    Warning,
    Error,
};

pub const Diagnostic = struct {
    severity: DiagSeverity,
    inst_index: u32,
    code_offset: u32,
    message: [256]u8,
    message_len: u8,

    pub fn getMessage(self: *const Diagnostic) []const u8 {
        return self.message[0..self.message_len];
    }

    fn create(severity: DiagSeverity, inst_index: u32, code_offset: u32, comptime fmt: []const u8, args: anytype) Diagnostic {
        var d: Diagnostic = undefined;
        d.severity = severity;
        d.inst_index = inst_index;
        d.code_offset = code_offset;
        d.message_len = 0;
        var buf: [256]u8 = undefined;
        const s = std.fmt.bufPrint(&buf, fmt, args) catch {
            // Truncate on overflow
            @memcpy(d.message[0..255], buf[0..255]);
            d.message_len = 255;
            return d;
        };
        const len: u8 = @intCast(@min(s.len, 255));
        @memcpy(d.message[0..len], s[0..len]);
        d.message_len = len;
        return d;
    }
};

// ============================================================================
// Section: Branch Fixup Record
// ============================================================================

const BranchFixup = struct {
    /// Index into the code buffer (byte offset of the branch instruction)
    code_offset: u32,
    /// Target label (block ID)
    target_id: i32,
    /// What kind of branch this is (determines encoding width)
    branch_class: BranchClass,
    /// Index into the JitInst array (for diagnostics)
    inst_index: u32,
    /// The base opcode already written (we'll OR in the offset)
    base_opcode: u32,
};

// ============================================================================
// Section: External Call Entry (Trampoline)
// ============================================================================

pub const ExtCallEntry = struct {
    /// Offset in code buffer where the BL instruction lives
    code_offset: u32,
    /// Symbol name to resolve
    sym_name: [JIT_SYM_MAX]u8,
    /// Instruction index for diagnostics
    inst_index: u32,

    pub fn getName(self: *const ExtCallEntry) []const u8 {
        var len: usize = 0;
        while (len < JIT_SYM_MAX and self.sym_name[len] != 0) : (len += 1) {}
        return self.sym_name[0..len];
    }
};

// ============================================================================
// Section: Source Map Entry
// ============================================================================

pub const SourceMapEntry = struct {
    /// Offset in code buffer
    code_offset: u32,
    /// BASIC source line number
    source_line: u32,
    /// Source column (0 if unknown)
    source_col: u32,
};

// ============================================================================
// Section: Comment Map Entry
// ============================================================================

/// Maps a code offset to a codegen comment string.
/// Comments are pseudo-instructions emitted by jit_collect (JIT_COMMENT)
/// that annotate the generated code with human-readable context (e.g.
/// "TRY block", "DIM X(...)", "unhandled op 42").  The encoder records
/// the current code_len when it encounters a JIT_COMMENT so that
/// Capstone disassembly can interleave these annotations at the
/// corresponding machine-code offsets.
pub const CommentEntry = struct {
    /// Byte offset in code buffer where this comment applies
    /// (the next instruction emitted after this comment)
    code_offset: u32,
    /// Comment text (copied from JitInst.sym_name)
    text: [JIT_SYM_MAX]u8,
    /// Length of the comment text
    text_len: u8,

    pub fn getText(self: *const CommentEntry) []const u8 {
        return self.text[0..self.text_len];
    }
};

// ============================================================================
// Section: Load Address Relocation Entry
// ============================================================================

/// Records a LOAD_ADDR relocation emitted during encoding.
/// The encoder emits ADRP+ADD with placeholder immediates (0,0) and records
/// the code offset + symbol name here so the linker can patch them after
/// real addresses are known — without needing the raw JitInst[] stream.
/// Records a DATA_SYMREF relocation — a symbol reference embedded in the
/// data section (e.g. vtable function pointers like `l $Dog__Speak`).
/// The encoder emits an 8-byte zero placeholder and records the data offset
/// + symbol name here so the linker can patch it with the real address.
pub const DataSymRef = struct {
    /// Byte offset within the data buffer where the 8-byte slot lives
    data_offset: u32,
    /// Symbol name (copied from JitInst.sym_name)
    sym_name: [JIT_SYM_MAX]u8,
    /// Length of the symbol name
    sym_name_len: u8,
    /// Addend (offset added to resolved address, from JitInst.imm)
    addend: i64,
    /// Index of the originating JitInst (for diagnostics)
    inst_index: u32,

    pub fn getName(self: *const DataSymRef) []const u8 {
        return self.sym_name[0..self.sym_name_len];
    }
};

pub const LoadAddrReloc = struct {
    /// Byte offset of the ADRP instruction in the code buffer
    adrp_offset: u32,
    /// Symbol name (copied from JitInst.sym_name)
    sym_name: [JIT_SYM_MAX]u8,
    /// Length of the symbol name
    sym_name_len: u8,
    /// Index of the originating JitInst (for diagnostics)
    inst_index: u32,
    /// Addend: byte offset added to the symbol address (e.g. +16 for
    /// accessing a field inside a struct/descriptor).  Set from
    /// JitInst.imm when the QBE CAddr constant carries a non-zero
    /// bits.i offset.
    addend: i64,

    pub fn getName(self: *const LoadAddrReloc) []const u8 {
        return self.sym_name[0..self.sym_name_len];
    }
};

// ============================================================================
// Section: Symbol Entry
// ============================================================================

pub const SymbolEntry = struct {
    /// Offset within the appropriate buffer (code or data)
    offset: u32,
    /// Whether this is a code or data symbol
    is_code: bool,
    /// Symbol type
    sym_type: JitSymType,
};

// ============================================================================
// Section: JitModule — the output of encoding
// ============================================================================

pub const JitModule = struct {
    /// Executable code buffer
    code: []u8,
    /// Current write offset in code buffer
    code_len: u32,

    /// Data section buffer
    data: []u8,
    /// Current write offset in data buffer
    data_len: u32,

    /// Label table: block_id → code byte offset
    labels: std.AutoHashMap(i32, u32),

    /// Forward branch fixups (resolved after all labels are known)
    fixups: std.ArrayListUnmanaged(BranchFixup),

    /// External call entries (need trampoline stubs)
    ext_calls: std.ArrayListUnmanaged(ExtCallEntry),

    /// Source map (code offset → BASIC line)
    source_map: std.ArrayListUnmanaged(SourceMapEntry),

    /// Comment map (code offset → codegen comment text)
    comment_map: std.ArrayListUnmanaged(CommentEntry),

    /// Symbol table (name → offset)
    symbols: std.StringHashMap(SymbolEntry),

    /// LOAD_ADDR relocations recorded during encoding
    load_addr_relocs: std.ArrayListUnmanaged(LoadAddrReloc),

    /// DATA_SYMREF relocations (symbol references in data, e.g. vtable function pointers)
    data_sym_refs: std.ArrayListUnmanaged(DataSymRef),

    /// Diagnostics collected during encoding
    diagnostics: std.ArrayListUnmanaged(Diagnostic),

    /// Statistics
    stats: EncodeStats,

    /// Allocator used for all dynamic allocations
    allocator: std.mem.Allocator,

    /// Whether we're currently in a data section (between DATA_START/DATA_END)
    in_data_section: bool,

    /// Pending data symbol: a slice into the *stable* JitInst sym_name
    /// array saved at DATA_START.  The offset is not committed to the
    /// symbol table until after DATA_ALIGN (or the first data-emission
    /// instruction), so that the recorded offset is the true
    /// post-alignment start of the constant — not the pre-alignment
    /// position.  We store a slice (not a copy) so the StringHashMap
    /// key keeps pointing at the same stable memory the old code used.
    pending_data_sym_name: []const u8,
    has_pending_data_sym: bool,

    /// Current function name (set by JIT_FUNC_BEGIN)
    current_func: [JIT_SYM_MAX]u8,
    current_func_len: u8,

    pub fn init(allocator: std.mem.Allocator, code_capacity: u32, data_capacity: u32) !JitModule {
        const code = try allocator.alloc(u8, code_capacity);
        const data = try allocator.alloc(u8, data_capacity);
        return JitModule{
            .code = code,
            .code_len = 0,
            .data = data,
            .data_len = 0,
            .labels = std.AutoHashMap(i32, u32).init(allocator),
            .fixups = .{},
            .ext_calls = .{},
            .source_map = .{},
            .comment_map = .{},
            .symbols = std.StringHashMap(SymbolEntry).init(allocator),
            .load_addr_relocs = .{},
            .data_sym_refs = .{},
            .diagnostics = .{},
            .stats = EncodeStats{},
            .allocator = allocator,
            .in_data_section = false,
            .pending_data_sym_name = &.{},
            .has_pending_data_sym = false,
            .current_func = [_]u8{0} ** JIT_SYM_MAX,
            .current_func_len = 0,
        };
    }

    pub fn deinit(self: *JitModule) void {
        self.allocator.free(self.code);
        self.allocator.free(self.data);
        self.labels.deinit();
        self.fixups.deinit(self.allocator);
        self.ext_calls.deinit(self.allocator);
        self.source_map.deinit(self.allocator);
        self.comment_map.deinit(self.allocator);
        self.symbols.deinit();
        self.load_addr_relocs.deinit(self.allocator);
        self.data_sym_refs.deinit(self.allocator);
        self.diagnostics.deinit(self.allocator);
    }

    /// Write a 32-bit instruction word to the code buffer.
    pub fn emitWord(self: *JitModule, word: u32) EncodeError!void {
        if (self.code_len + 4 > self.code.len) {
            return EncodeError.BufferOverflow;
        }
        const offset = self.code_len;
        self.code[offset + 0] = @truncate(word);
        self.code[offset + 1] = @truncate(word >> 8);
        self.code[offset + 2] = @truncate(word >> 16);
        self.code[offset + 3] = @truncate(word >> 24);
        self.code_len += 4;
        self.stats.instructions_emitted += 1;
    }

    /// Write raw bytes to the data buffer.
    pub fn emitData(self: *JitModule, bytes: []const u8) EncodeError!void {
        if (self.data_len + bytes.len > self.data.len) {
            return EncodeError.BufferOverflow;
        }
        @memcpy(self.data[self.data_len..][0..bytes.len], bytes);
        self.data_len += @intCast(bytes.len);
    }

    /// Write a data byte.
    pub fn emitDataByte(self: *JitModule, b: u8) EncodeError!void {
        if (self.data_len + 1 > self.data.len) {
            return EncodeError.BufferOverflow;
        }
        self.data[self.data_len] = b;
        self.data_len += 1;
    }

    /// Write a data half-word (16-bit LE).
    pub fn emitDataHalf(self: *JitModule, val: u16) EncodeError!void {
        if (self.data_len + 2 > self.data.len) {
            return EncodeError.BufferOverflow;
        }
        self.data[self.data_len + 0] = @truncate(val);
        self.data[self.data_len + 1] = @truncate(val >> 8);
        self.data_len += 2;
    }

    /// Write a data word (32-bit LE).
    pub fn emitDataWord(self: *JitModule, val: u32) EncodeError!void {
        if (self.data_len + 4 > self.data.len) {
            return EncodeError.BufferOverflow;
        }
        self.data[self.data_len + 0] = @truncate(val);
        self.data[self.data_len + 1] = @truncate(val >> 8);
        self.data[self.data_len + 2] = @truncate(val >> 16);
        self.data[self.data_len + 3] = @truncate(val >> 24);
        self.data_len += 4;
    }

    /// Write a data quad (64-bit LE).
    pub fn emitDataQuad(self: *JitModule, val: u64) EncodeError!void {
        if (self.data_len + 8 > self.data.len) {
            return EncodeError.BufferOverflow;
        }
        var i: u32 = 0;
        while (i < 8) : (i += 1) {
            self.data[self.data_len + i] = @truncate(val >> @intCast(i * 8));
        }
        self.data_len += 8;
    }

    /// Write zero-fill to data buffer.
    pub fn emitDataZero(self: *JitModule, count: u32) EncodeError!void {
        if (self.data_len + count > self.data.len) {
            return EncodeError.BufferOverflow;
        }
        @memset(self.data[self.data_len..][0..count], 0);
        self.data_len += count;
    }

    /// Align data buffer to a given boundary.
    pub fn alignData(self: *JitModule, alignment: u32) EncodeError!void {
        if (alignment == 0) return;
        const mask = alignment - 1;
        const padding = (alignment - (self.data_len & mask)) & mask;
        if (padding > 0) {
            try self.emitDataZero(padding);
        }
        // After aligning, commit any pending data symbol so its offset
        // points to the true aligned start of the constant.
        self.commitPendingDataSym();
    }

    /// If a DATA_START recorded a pending symbol name, commit it to the
    /// symbol table now — using the *current* data_len as the offset.
    /// This must be called after alignment and/or before the first byte
    /// of the constant is emitted, so the symbol points at the right place.
    pub fn commitPendingDataSym(self: *JitModule) void {
        if (!self.has_pending_data_sym) return;
        if (self.pending_data_sym_name.len > 0) {
            self.symbols.put(self.pending_data_sym_name, SymbolEntry{
                .offset = self.data_len,
                .is_code = false,
                .sym_type = .DATA,
            }) catch {};
        }
        self.has_pending_data_sym = false;
    }

    /// Patch a previously emitted instruction at the given byte offset.
    pub fn patchWord(self: *JitModule, offset: u32, word: u32) void {
        self.code[offset + 0] = @truncate(word);
        self.code[offset + 1] = @truncate(word >> 8);
        self.code[offset + 2] = @truncate(word >> 16);
        self.code[offset + 3] = @truncate(word >> 24);
    }

    /// Read back a previously emitted instruction word.
    pub fn readWord(self: *const JitModule, offset: u32) u32 {
        return @as(u32, self.code[offset + 0]) |
            (@as(u32, self.code[offset + 1]) << 8) |
            (@as(u32, self.code[offset + 2]) << 16) |
            (@as(u32, self.code[offset + 3]) << 24);
    }

    /// Record a diagnostic.
    pub fn addDiag(self: *JitModule, severity: DiagSeverity, inst_index: u32, comptime fmt: []const u8, args: anytype) void {
        const d = Diagnostic.create(severity, inst_index, self.code_len, fmt, args);
        self.diagnostics.append(self.allocator, d) catch {};
        if (severity == .Error) {
            self.stats.error_count += 1;
        }
    }

    /// Check if encoding completed without errors.
    pub fn hasErrors(self: *const JitModule) bool {
        return self.stats.error_count > 0;
    }

    /// Get error count.
    pub fn errorCount(self: *const JitModule) u32 {
        return self.stats.error_count;
    }
};

// ============================================================================
// Section: Encoding Statistics
// ============================================================================

pub const EncodeStats = struct {
    instructions_emitted: u32 = 0,
    labels_recorded: u32 = 0,
    fixups_created: u32 = 0,
    fixups_resolved: u32 = 0,
    ext_calls_recorded: u32 = 0,
    source_map_entries: u32 = 0,
    data_bytes_emitted: u32 = 0,
    error_count: u32 = 0,
    skipped_pseudo: u32 = 0,
    functions_encoded: u32 = 0,
    neon_ops_encoded: u32 = 0,
    comments_recorded: u32 = 0,
};

// ============================================================================
// Section: Register Mapping
//
// Maps JitInst register IDs (i32) to the encoder's Register/NeonRegister enums.
// ============================================================================

/// Map a JitInst GP register ID to the encoder's Register enum.
pub fn mapGPRegister(reg_id: i32) EncodeError!Register {
    if (reg_id >= 0 and reg_id <= 30) {
        return @enumFromInt(@as(u5, @intCast(reg_id)));
    }
    return switch (reg_id) {
        JIT_REG_SP => Register.SP,
        JIT_REG_FP => Register.FP,
        JIT_REG_LR => Register.LR,
        JIT_REG_IP0 => Register.R16,
        JIT_REG_IP1 => Register.R17,
        else => EncodeError.InvalidRegister,
    };
}

/// Map a JitInst NEON register ID to the encoder's NeonRegister enum.
/// NEON registers are stored as JIT_VREG_BASE - index (so V0 = -100, V1 = -101, etc.)
pub fn mapNeonRegister(reg_id: i32) EncodeError!NeonRegister {
    if (reg_id > JIT_VREG_BASE) {
        return EncodeError.InvalidRegister;
    }
    const index = JIT_VREG_BASE - reg_id;
    if (index < 0 or index > 31) {
        return EncodeError.InvalidRegister;
    }
    return @enumFromInt(@as(u5, @intCast(index)));
}

/// Determine if a register ID refers to a NEON register.
pub fn isNeonReg(reg_id: i32) bool {
    return reg_id <= JIT_VREG_BASE;
}

/// Map JitCond to encoder Condition. Values are identical (ARM64 encoding).
pub fn mapCondition(cond: u8) Condition {
    return @enumFromInt(@as(u4, @intCast(cond & 0xF)));
}

/// Map JitShift to encoder ShiftType. Values are identical.
pub fn mapShiftType(shift: u8) ShiftType {
    return @enumFromInt(@as(u2, @intCast(shift & 0x3)));
}

/// Map JitNeonArr to NeonSize for the encoder.
/// QBE uses a small set of arrangements; map them to the encoder's NeonSize.
pub fn mapNeonArr(arr_val: u8, is_float: bool) NeonSize {
    const arr: JitNeonArr = @enumFromInt(arr_val);
    return switch (arr) {
        .NEON_4S => if (is_float) NeonSize.Size4S else NeonSize.Size4S,
        .NEON_2D => if (is_float) NeonSize.Size2D else NeonSize.Size2D,
        .NEON_4SF => NeonSize.Size4S,
        .NEON_2DF => NeonSize.Size2D,
        .NEON_8H => NeonSize.Size8H,
        .NEON_16B => NeonSize.Size16B,
    };
}

/// Get the scalar NeonSize for a JitCls (S → Size1S, D → Size1D).
pub fn scalarSizeFromCls(cls: u8) NeonSize {
    return if (cls == @intFromEnum(JitCls.D)) NeonSize.Size1D else NeonSize.Size1S;
}

// ============================================================================
// Section: Core Instruction Encoding Dispatch
// ============================================================================

/// Encode a single JitInst into the JitModule's code buffer.
/// This is the main dispatch function — it switches on inst.kind and calls
/// the appropriate encoder function(s).
pub fn encodeInstruction(mod: *JitModule, inst: *const JitInst, inst_index: u32) void {
    const kind = inst.getKind();

    switch (kind) {
        // ── Pseudo-instructions (no machine code) ──
        .JIT_LABEL => {
            mod.labels.put(inst.target_id, mod.code_len) catch {
                mod.addDiag(.Error, inst_index, "label table full (block {d})", .{inst.target_id});
            };
            mod.stats.labels_recorded += 1;
        },

        .JIT_FUNC_BEGIN => {
            const name = inst.getSymName();
            const len: u8 = @intCast(@min(name.len, JIT_SYM_MAX));
            @memcpy(mod.current_func[0..len], name[0..len]);
            if (len < JIT_SYM_MAX) mod.current_func[len] = 0;
            mod.current_func_len = len;

            // Record symbol for this function
            mod.symbols.put(name, SymbolEntry{
                .offset = mod.code_len,
                .is_code = true,
                .sym_type = .FUNC,
            }) catch {};

            mod.stats.functions_encoded += 1;
        },

        .JIT_FUNC_END => {
            // No machine code — boundary marker only
            mod.stats.skipped_pseudo += 1;
        },

        .JIT_DBGLOC => {
            // Record source mapping
            mod.source_map.append(mod.allocator, SourceMapEntry{
                .code_offset = mod.code_len,
                .source_line = @intCast(@as(u32, @bitCast(@as(i32, @truncate(inst.imm))))),
                .source_col = @intCast(@as(u32, @bitCast(@as(i32, @truncate(inst.imm2))))),
            }) catch {};
            mod.stats.source_map_entries += 1;
            mod.stats.skipped_pseudo += 1;
        },

        .JIT_NOP => {
            mod.emitWord(enc.emitNop()) catch |e| {
                mod.addDiag(.Error, inst_index, "NOP emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_COMMENT => {
            // Record the comment at the current code offset so Capstone
            // disassembly can display it alongside the machine code.
            const name = inst.getSymName();
            const len: u8 = @intCast(@min(name.len, JIT_SYM_MAX));
            var entry = CommentEntry{
                .code_offset = mod.code_len,
                .text = [_]u8{0} ** JIT_SYM_MAX,
                .text_len = len,
            };
            @memcpy(entry.text[0..len], name[0..len]);
            mod.comment_map.append(mod.allocator, entry) catch {};
            mod.stats.comments_recorded += 1;
            mod.stats.skipped_pseudo += 1;
        },

        // ── Integer arithmetic (register-register) ──
        .JIT_ADD_RRR => encodeAluRRR(mod, inst, inst_index, .Add),
        .JIT_SUB_RRR => encodeAluRRR(mod, inst, inst_index, .Sub),
        .JIT_MUL_RRR => encodeAluRRR(mod, inst, inst_index, .Mul),
        .JIT_SDIV_RRR => encodeAluRRR(mod, inst, inst_index, .Sdiv),
        .JIT_UDIV_RRR => encodeAluRRR(mod, inst, inst_index, .Udiv),
        .JIT_AND_RRR => encodeAluRRR(mod, inst, inst_index, .And),
        .JIT_ORR_RRR => encodeAluRRR(mod, inst, inst_index, .Orr),
        .JIT_EOR_RRR => encodeAluRRR(mod, inst, inst_index, .Eor),
        .JIT_LSL_RRR => encodeAluRRR(mod, inst, inst_index, .Lsl),
        .JIT_LSR_RRR => encodeAluRRR(mod, inst, inst_index, .Lsr),
        .JIT_ASR_RRR => encodeAluRRR(mod, inst, inst_index, .Asr),

        .JIT_NEG_RR => {
            const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
            const rm = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
            const word = if (inst.is64()) enc.emitNeg64(rd, rm) else enc.emitNeg(rd, rm);
            mod.emitWord(word) catch |e| {
                mod.addDiag(.Error, inst_index, "NEG emit failed: {s}", .{@errorName(e)});
            };
        },

        // ── Fused multiply-add/sub (4-operand) ──
        .JIT_MADD_RRRR => encodeMaddMsub(mod, inst, inst_index, false),
        .JIT_MSUB_RRRR => encodeMaddMsub(mod, inst, inst_index, true),

        // ── Register-immediate ALU ──
        .JIT_ADD_RRI => encodeAluRRI(mod, inst, inst_index, false),
        .JIT_SUB_RRI => encodeAluRRI(mod, inst, inst_index, true),

        // ── Shifted-operand ALU ──
        .JIT_ADD_SHIFT => encodeShiftedAlu(mod, inst, inst_index, .Add),
        .JIT_SUB_SHIFT => encodeShiftedAlu(mod, inst, inst_index, .Sub),
        .JIT_AND_SHIFT => encodeShiftedAlu(mod, inst, inst_index, .And),
        .JIT_ORR_SHIFT => encodeShiftedAlu(mod, inst, inst_index, .Orr),
        .JIT_EOR_SHIFT => encodeShiftedAlu(mod, inst, inst_index, .Eor),

        // ── Move / constant loading ──
        .JIT_MOV_RR => {
            const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
            const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
            const word = if (inst.is64()) enc.emitMovRegister64(rd, rn) else enc.emitMovRegister(rd, rn);
            mod.emitWord(word) catch |e| {
                mod.addDiag(.Error, inst_index, "MOV emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_MOVZ => encodeMoveWide(mod, inst, inst_index, .Movz),
        .JIT_MOVK => encodeMoveWide(mod, inst, inst_index, .Movk),
        .JIT_MOVN => encodeMoveWide(mod, inst, inst_index, .Movn),

        .JIT_MOV_WIDE_IMM => {
            // Multi-instruction immediate load
            const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
            if (inst.is64()) {
                const val: u64 = @bitCast(inst.imm);
                var buf: [4]u32 = undefined;
                const count = enc.emitLoadImmediate64(rd, val, &buf);
                var j: u8 = 0;
                while (j < count) : (j += 1) {
                    mod.emitWord(buf[j]) catch |e| {
                        mod.addDiag(.Error, inst_index, "MOV_WIDE_IMM emit failed: {s}", .{@errorName(e)});
                        return;
                    };
                }
            } else {
                const val: u32 = @truncate(@as(u64, @bitCast(inst.imm)));
                var buf: [2]u32 = undefined;
                const count = enc.emitLoadImmediate32(rd, val, &buf);
                var j: u8 = 0;
                while (j < count) : (j += 1) {
                    mod.emitWord(buf[j]) catch |e| {
                        mod.addDiag(.Error, inst_index, "MOV_WIDE_IMM emit failed: {s}", .{@errorName(e)});
                        return;
                    };
                }
            }
        },

        // ── Floating point arithmetic ──
        .JIT_FADD_RRR => encodeFpRRR(mod, inst, inst_index, .Fadd),
        .JIT_FSUB_RRR => encodeFpRRR(mod, inst, inst_index, .Fsub),
        .JIT_FMUL_RRR => encodeFpRRR(mod, inst, inst_index, .Fmul),
        .JIT_FDIV_RRR => encodeFpRRR(mod, inst, inst_index, .Fdiv),

        .JIT_FNEG_RR => {
            const rd = mapNeonRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd(neon)", inst.rd, e);
            const rn = mapNeonRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn(neon)", inst.rn, e);
            const size = scalarSizeFromCls(inst.cls);
            const word = enc.emitNeonFneg(rd, rn, size);
            mod.emitWord(word) catch |e| {
                mod.addDiag(.Error, inst_index, "FNEG emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_FMOV_RR => {
            const rd = mapNeonRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd(neon)", inst.rd, e);
            const rn = mapNeonRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn(neon)", inst.rn, e);
            const size = scalarSizeFromCls(inst.cls);
            const word = enc.emitNeonFmov(rd, rn, size);
            mod.emitWord(word) catch |e| {
                mod.addDiag(.Error, inst_index, "FMOV emit failed: {s}", .{@errorName(e)});
            };
        },

        // ── Float <-> Int conversions ──
        .JIT_FCVT_SD => {
            // Single → Double
            const rd = mapNeonRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd(neon)", inst.rd, e);
            const rn = mapNeonRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn(neon)", inst.rn, e);
            mod.emitWord(enc.emitFcvtStoD(rd, rn)) catch |e| {
                mod.addDiag(.Error, inst_index, "FCVT S→D emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_FCVT_DS => {
            // Double → Single
            const rd = mapNeonRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd(neon)", inst.rd, e);
            const rn = mapNeonRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn(neon)", inst.rn, e);
            mod.emitWord(enc.emitFcvtDtoS(rd, rn)) catch |e| {
                mod.addDiag(.Error, inst_index, "FCVT D→S emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_FCVTZS => {
            // Float → signed int (truncate)
            // cls = source FP type (S or D), is_float = dest is 64-bit int
            const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
            const rn = mapNeonRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn(neon)", inst.rn, e);
            const size = scalarSizeFromCls(inst.cls);
            const dest_is_64 = inst.is_float != 0;
            const word = if (dest_is_64) enc.emitNeonFcvtzsGen64(rd, rn, size) else enc.emitNeonFcvtzsGen(rd, rn, size);
            mod.emitWord(word) catch |e| {
                mod.addDiag(.Error, inst_index, "FCVTZS emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_FCVTZU => {
            // cls = source FP type (S or D), is_float = dest is 64-bit int
            const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
            const rn = mapNeonRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn(neon)", inst.rn, e);
            const size = scalarSizeFromCls(inst.cls);
            const dest_is_64 = inst.is_float != 0;
            const word = if (dest_is_64) enc.emitNeonFcvtzuGen64(rd, rn, size) else enc.emitNeonFcvtzuGen(rd, rn, size);
            mod.emitWord(word) catch |e| {
                mod.addDiag(.Error, inst_index, "FCVTZU emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_SCVTF => {
            // cls = dest FP type (S or D), is_float = source is 64-bit int
            const rd = mapNeonRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd(neon)", inst.rd, e);
            const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
            const size = scalarSizeFromCls(inst.cls);
            const src_is_64 = inst.is_float != 0;
            const word = if (src_is_64) enc.emitNeonScvtfGen64(rd, rn, size) else enc.emitNeonScvtfGen(rd, rn, size);
            mod.emitWord(word) catch |e| {
                mod.addDiag(.Error, inst_index, "SCVTF emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_UCVTF => {
            // cls = dest FP type (S or D), is_float = source is 64-bit int
            const rd = mapNeonRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd(neon)", inst.rd, e);
            const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
            const size = scalarSizeFromCls(inst.cls);
            const src_is_64 = inst.is_float != 0;
            const word = if (src_is_64) enc.emitNeonUcvtfGen64(rd, rn, size) else enc.emitNeonUcvtfGen(rd, rn, size);
            mod.emitWord(word) catch |e| {
                mod.addDiag(.Error, inst_index, "UCVTF emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_FMOV_GF => {
            // FPR → GPR (bitcast)
            // The cls field is the *destination* (integer) class, not the
            // source FP class.  cls=L means 64-bit dest → fmov Xd, Dn;
            // cls=W means 32-bit dest → fmov Wd, Sn.  We must use is64()
            // (not isDouble()) to select the correct encoding.
            const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
            const rn = mapNeonRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn(neon)", inst.rn, e);
            const word = if (inst.is64()) enc.emitNeonFmovToGeneral64(rd, rn) else enc.emitNeonFmovToGeneral(rd, rn);
            mod.emitWord(word) catch |e| {
                mod.addDiag(.Error, inst_index, "FMOV GF emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_FMOV_FG => {
            // GPR → FPR (bitcast)
            const rd = mapNeonRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd(neon)", inst.rd, e);
            const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
            const word = if (inst.isDouble()) enc.emitNeonFmovFromGeneral64(rd, rn) else enc.emitNeonFmovFromGeneral(rd, rn);
            mod.emitWord(word) catch |e| {
                mod.addDiag(.Error, inst_index, "FMOV FG emit failed: {s}", .{@errorName(e)});
            };
        },

        // ── Extensions ──
        .JIT_SXTB => encodeExtension(mod, inst, inst_index, .Sxtb),
        .JIT_UXTB => encodeExtension(mod, inst, inst_index, .Uxtb),
        .JIT_SXTH => encodeExtension(mod, inst, inst_index, .Sxth),
        .JIT_UXTH => encodeExtension(mod, inst, inst_index, .Uxth),
        .JIT_SXTW => {
            const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
            const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
            mod.emitWord(enc.emitSxtw64(rd, rn)) catch |e| {
                mod.addDiag(.Error, inst_index, "SXTW emit failed: {s}", .{@errorName(e)});
            };
        },
        .JIT_UXTW => {
            // UXTW is aliased as MOV Wd, Wn (32-bit mov clears upper 32 bits)
            const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
            const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
            mod.emitWord(enc.emitMovRegister(rd, rn)) catch |e| {
                mod.addDiag(.Error, inst_index, "UXTW/MOV emit failed: {s}", .{@errorName(e)});
            };
        },

        // ── Compare ──
        .JIT_CMP_RR => {
            const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
            const rm = mapGPRegister(inst.rm) catch |e| return regError(mod, inst_index, "rm", inst.rm, e);
            const word = if (inst.is64())
                enc.emitCmpRegister64(rn, RegisterParam.reg_only(rm))
            else
                enc.emitCmpRegister(rn, RegisterParam.reg_only(rm));
            mod.emitWord(word) catch |e| {
                mod.addDiag(.Error, inst_index, "CMP RR emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_CMP_RI => {
            const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
            const imm12: u12 = @truncate(@as(u64, @bitCast(inst.imm)));
            const word = if (inst.is64()) enc.emitCmpImmediate64(rn, imm12) else enc.emitCmpImmediate(rn, imm12);
            mod.emitWord(word) catch |e| {
                mod.addDiag(.Error, inst_index, "CMP RI emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_CMN_RR => {
            const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
            const rm = mapGPRegister(inst.rm) catch |e| return regError(mod, inst_index, "rm", inst.rm, e);
            const word = if (inst.is64())
                enc.emitCmnRegister64(rn, RegisterParam.reg_only(rm))
            else
                enc.emitCmnRegister(rn, RegisterParam.reg_only(rm));
            mod.emitWord(word) catch |e| {
                mod.addDiag(.Error, inst_index, "CMN emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_FCMP_RR => {
            const rn = mapNeonRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn(neon)", inst.rn, e);
            const rm = mapNeonRegister(inst.rm) catch |e| return regError(mod, inst_index, "rm(neon)", inst.rm, e);
            const size = scalarSizeFromCls(inst.cls);
            // QBE uses FCMPE (signalling) for all FP compares
            const word = enc.emitNeonFcmpe(rn, rm, size);
            mod.emitWord(word) catch |e| {
                mod.addDiag(.Error, inst_index, "FCMPE emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_TST_RR => {
            const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
            const rm = mapGPRegister(inst.rm) catch |e| return regError(mod, inst_index, "rm", inst.rm, e);
            const word = if (inst.is64())
                enc.emitTestRegister64(rn, RegisterParam.reg_only(rm))
            else
                enc.emitTestRegister(rn, RegisterParam.reg_only(rm));
            mod.emitWord(word) catch |e| {
                mod.addDiag(.Error, inst_index, "TST emit failed: {s}", .{@errorName(e)});
            };
        },

        // ── Conditional select/set ──
        .JIT_CSET => {
            const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
            const cond = mapCondition(inst.cond);
            const word = if (inst.is64()) enc.emitCset64(rd, cond) else enc.emitCset(rd, cond);
            mod.emitWord(word) catch |e| {
                mod.addDiag(.Error, inst_index, "CSET emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_CSEL => {
            const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
            const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
            const rm = mapGPRegister(inst.rm) catch |e| return regError(mod, inst_index, "rm", inst.rm, e);
            const cond = mapCondition(inst.cond);
            const word = if (inst.is64()) enc.emitCsel64(rd, rn, rm, cond) else enc.emitCsel(rd, rn, rm, cond);
            mod.emitWord(word) catch |e| {
                mod.addDiag(.Error, inst_index, "CSEL emit failed: {s}", .{@errorName(e)});
            };
        },

        // ── Memory: load with immediate offset ──
        .JIT_LDR_RI => encodeLdrStrOffset(mod, inst, inst_index, .Ldr),
        .JIT_LDRB_RI => encodeLdrStrOffset(mod, inst, inst_index, .Ldrb),
        .JIT_LDRH_RI => encodeLdrStrOffset(mod, inst, inst_index, .Ldrh),
        .JIT_LDRSB_RI => encodeLdrStrOffset(mod, inst, inst_index, .Ldrsb),
        .JIT_LDRSH_RI => encodeLdrStrOffset(mod, inst, inst_index, .Ldrsh),
        .JIT_LDRSW_RI => encodeLdrStrOffset(mod, inst, inst_index, .Ldrsw),

        // ── Memory: store with immediate offset ──
        .JIT_STR_RI => encodeLdrStrOffset(mod, inst, inst_index, .Str),
        .JIT_STRB_RI => encodeLdrStrOffset(mod, inst, inst_index, .Strb),
        .JIT_STRH_RI => encodeLdrStrOffset(mod, inst, inst_index, .Strh),

        // ── Memory: load/store with register offset ──
        .JIT_LDR_RR => encodeLdrStrRegister(mod, inst, inst_index, .Ldr),
        .JIT_STR_RR => encodeLdrStrRegister(mod, inst, inst_index, .Str),
        .JIT_LDRB_RR => encodeLdrStrRegister(mod, inst, inst_index, .Ldrb),
        .JIT_LDRH_RR => encodeLdrStrRegister(mod, inst, inst_index, .Ldrh),
        .JIT_LDRSB_RR => encodeLdrStrRegister(mod, inst, inst_index, .Ldrsb),
        .JIT_LDRSH_RR => encodeLdrStrRegister(mod, inst, inst_index, .Ldrsh),
        .JIT_LDRSW_RR => encodeLdrStrRegister(mod, inst, inst_index, .Ldrsw),
        .JIT_STRB_RR => encodeLdrStrRegister(mod, inst, inst_index, .Strb),
        .JIT_STRH_RR => encodeLdrStrRegister(mod, inst, inst_index, .Strh),

        // ── Memory: load/store pair ──
        .JIT_LDP => encodeLdpStp(mod, inst, inst_index, .Ldp, false),
        .JIT_STP => encodeLdpStp(mod, inst, inst_index, .Stp, false),
        .JIT_LDP_POST => encodeLdpStp(mod, inst, inst_index, .LdpPost, true),
        .JIT_STP_PRE => encodeLdpStp(mod, inst, inst_index, .StpPre, true),

        // ── Branches ──
        .JIT_B => encodeBranch(mod, inst, inst_index, .B),
        .JIT_BL => encodeBranch(mod, inst, inst_index, .BL),
        .JIT_B_COND => encodeBranch(mod, inst, inst_index, .BCond),
        .JIT_CBZ => encodeBranch(mod, inst, inst_index, .CBZ),
        .JIT_CBNZ => encodeBranch(mod, inst, inst_index, .CBNZ),

        .JIT_BR => {
            const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
            mod.emitWord(enc.emitBr(rn)) catch |e| {
                mod.addDiag(.Error, inst_index, "BR emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_BLR => {
            const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
            mod.emitWord(enc.emitBlr(rn)) catch |e| {
                mod.addDiag(.Error, inst_index, "BLR emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_RET => {
            // RET defaults to LR (x30)
            mod.emitWord(enc.emitRet(Register.LR)) catch |e| {
                mod.addDiag(.Error, inst_index, "RET emit failed: {s}", .{@errorName(e)});
            };
        },

        // ── Call external ──
        .JIT_CALL_EXT => {
            // Record the call site for trampoline resolution later
            var entry: ExtCallEntry = undefined;
            @memcpy(&entry.sym_name, &inst.sym_name);
            entry.code_offset = mod.code_len;
            entry.inst_index = inst_index;
            mod.ext_calls.append(mod.allocator, entry) catch {
                mod.addDiag(.Error, inst_index, "ext call table full", .{});
                return;
            };
            mod.stats.ext_calls_recorded += 1;

            // Emit a placeholder BL (offset 0) — will be patched during trampoline linking
            mod.emitWord(enc.emitBl(0)) catch |e| {
                mod.addDiag(.Error, inst_index, "CALL_EXT BL emit failed: {s}", .{@errorName(e)});
            };
        },

        // ── PC-relative ──
        .JIT_ADRP => {
            const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
            // ADRP with symbol — will need relocation; for now emit with imm=0 placeholder
            mod.emitWord(enc.emitAdrp(rd, 0)) catch |e| {
                mod.addDiag(.Error, inst_index, "ADRP emit failed: {s}", .{@errorName(e)});
            };
            // TODO: record relocation for symbol resolution
            if (inst.sym_name[0] != 0) {
                mod.addDiag(.Info, inst_index, "ADRP symbol reloc needed: {s}", .{inst.getSymName()});
            }
        },

        .JIT_ADR => {
            const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
            mod.emitWord(enc.emitAdr(rd, 0)) catch |e| {
                mod.addDiag(.Error, inst_index, "ADR emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_LOAD_ADDR => {
            // Load address of symbol (ADRP + ADD sequence)
            // Emit placeholders with imm=0; the linker will patch these
            // once real addresses are known.  The inst.imm field carries
            // a CAddr addend (e.g. +16 for struct field access) which
            // the linker must add to the resolved symbol address.
            const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
            const adrp_offset = mod.code_len;
            mod.emitWord(enc.emitAdrp(rd, 0)) catch |e| {
                mod.addDiag(.Error, inst_index, "LOAD_ADDR ADRP emit failed: {s}", .{@errorName(e)});
                return;
            };
            mod.emitWord(enc.emitAddImmediate64(rd, rd, 0)) catch |e| {
                mod.addDiag(.Error, inst_index, "LOAD_ADDR ADD emit failed: {s}", .{@errorName(e)});
            };

            // Record relocation in the module so the linker can patch
            // without needing the raw JitInst[] stream.
            var reloc = LoadAddrReloc{
                .adrp_offset = adrp_offset,
                .sym_name = [_]u8{0} ** JIT_SYM_MAX,
                .sym_name_len = 0,
                .inst_index = inst_index,
                .addend = inst.imm,
            };
            const sym = inst.getSymName();
            if (sym.len > 0 and sym.len <= JIT_SYM_MAX) {
                @memcpy(reloc.sym_name[0..sym.len], sym);
                reloc.sym_name_len = @intCast(sym.len);
            }
            mod.load_addr_relocs.append(mod.allocator, reloc) catch {};
            if (inst.imm != 0) {
                mod.addDiag(.Info, inst_index, "LOAD_ADDR reloc recorded: {s}+{d}", .{ sym, inst.imm });
            } else {
                mod.addDiag(.Info, inst_index, "LOAD_ADDR reloc recorded: {s}", .{sym});
            }
        },

        // ── Stack manipulation ──
        .JIT_SUB_SP => {
            const imm_val: u64 = @bitCast(inst.imm);
            if (imm_val <= 4095) {
                const imm12: u12 = @truncate(imm_val);
                const w = enc.emitSubImmediate64(Register.SP, Register.SP, imm12);
                mod.emitWord(w) catch |e| {
                    mod.addDiag(.Error, inst_index, "SUB SP emit failed: {s}", .{@errorName(e)});
                };
            } else {
                // Large frame: use a scratch register
                var buf: [4]u32 = undefined;
                const count = enc.emitLoadImmediate64(Register.R16, @bitCast(inst.imm), &buf);
                var j: u8 = 0;
                while (j < count) : (j += 1) {
                    mod.emitWord(buf[j]) catch return;
                }
                mod.emitWord(enc.emitSubRegister64(Register.SP, Register.SP, RegisterParam.reg_only(Register.R16))) catch |e| {
                    mod.addDiag(.Error, inst_index, "SUB SP (large) emit failed: {s}", .{@errorName(e)});
                };
            }
        },

        .JIT_ADD_SP => {
            const imm_val2: u64 = @bitCast(inst.imm);
            if (imm_val2 <= 4095) {
                const imm12: u12 = @truncate(imm_val2);
                const w = enc.emitAddImmediate64(Register.SP, Register.SP, imm12);
                mod.emitWord(w) catch |e| {
                    mod.addDiag(.Error, inst_index, "ADD SP emit failed: {s}", .{@errorName(e)});
                };
            } else {
                var buf2: [4]u32 = undefined;
                const count2 = enc.emitLoadImmediate64(Register.R16, @bitCast(inst.imm), &buf2);
                var j2: u8 = 0;
                while (j2 < count2) : (j2 += 1) {
                    mod.emitWord(buf2[j2]) catch return;
                }
                mod.emitWord(enc.emitAddRegister64(Register.SP, Register.SP, RegisterParam.reg_only(Register.R16))) catch |e| {
                    mod.addDiag(.Error, inst_index, "ADD SP (large) emit failed: {s}", .{@errorName(e)});
                };
            }
        },

        .JIT_MOV_SP => {
            // MOV rd, SP  or  MOV SP, rn
            if (inst.rd != JIT_REG_NONE and inst.rd != JIT_REG_SP) {
                // Reading SP into rd
                const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
                mod.emitWord(enc.emitAddImmediate64(rd, Register.SP, 0)) catch |e| {
                    mod.addDiag(.Error, inst_index, "MOV from SP emit failed: {s}", .{@errorName(e)});
                };
            } else {
                // Writing rn into SP
                const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
                mod.emitWord(enc.emitAddImmediate64(Register.SP, rn, 0)) catch |e| {
                    mod.addDiag(.Error, inst_index, "MOV to SP emit failed: {s}", .{@errorName(e)});
                };
            }
        },

        // ── Special ──
        .JIT_HINT => {
            const imm7: u7 = @truncate(@as(u64, @bitCast(inst.imm)));
            mod.emitWord(enc.emitHint(imm7)) catch |e| {
                mod.addDiag(.Error, inst_index, "HINT emit failed: {s}", .{@errorName(e)});
            };
        },

        .JIT_BRK => {
            const imm16: u16 = @truncate(@as(u64, @bitCast(inst.imm)));
            mod.emitWord(enc.emitBrk(imm16)) catch |e| {
                mod.addDiag(.Error, inst_index, "BRK emit failed: {s}", .{@errorName(e)});
            };
        },

        // ── NEON vector ops ──
        .JIT_NEON_LDR_Q => encodeNeonLdrStr(mod, inst, inst_index, true),
        .JIT_NEON_STR_Q => encodeNeonLdrStr(mod, inst, inst_index, false),
        .JIT_NEON_ADD => encodeNeonBinOp(mod, inst, inst_index, .Add),
        .JIT_NEON_SUB => encodeNeonBinOp(mod, inst, inst_index, .Sub),
        .JIT_NEON_MUL => encodeNeonBinOp(mod, inst, inst_index, .Mul),
        .JIT_NEON_DIV => encodeNeonBinOp(mod, inst, inst_index, .Div),
        .JIT_NEON_NEG => encodeNeonUnaryOp(mod, inst, inst_index, .Neg),
        .JIT_NEON_ABS => encodeNeonUnaryOp(mod, inst, inst_index, .Abs),
        .JIT_NEON_FMA => encodeNeonFma(mod, inst, inst_index),
        .JIT_NEON_MIN => encodeNeonBinOp(mod, inst, inst_index, .Min),
        .JIT_NEON_MAX => encodeNeonBinOp(mod, inst, inst_index, .Max),
        .JIT_NEON_DUP => encodeNeonDup(mod, inst, inst_index),
        .JIT_NEON_ADDV => encodeNeonAddv(mod, inst, inst_index),

        // ── Data directives ──
        .JIT_DATA_START => {
            mod.in_data_section = true;
            // Save a slice pointing into the stable JitInst sym_name
            // but do NOT record the offset yet.  The offset must be
            // recorded *after* any subsequent DATA_ALIGN so it reflects
            // the true post-alignment start.
            const name = inst.getSymName();
            if (name.len > 0) {
                mod.pending_data_sym_name = name;
                mod.has_pending_data_sym = true;
            } else {
                mod.has_pending_data_sym = false;
            }
        },
        .JIT_DATA_END => {
            // Safety: commit any pending symbol that was never consumed
            // (e.g. an empty data block with no ALIGN or data emission).
            mod.commitPendingDataSym();
            mod.in_data_section = false;
        },
        .JIT_DATA_BYTE => {
            mod.commitPendingDataSym();
            mod.emitDataByte(@truncate(@as(u64, @bitCast(inst.imm)))) catch |e| {
                mod.addDiag(.Error, inst_index, "DATA_BYTE emit failed: {s}", .{@errorName(e)});
            };
            mod.stats.data_bytes_emitted += 1;
        },
        .JIT_DATA_HALF => {
            mod.commitPendingDataSym();
            mod.emitDataHalf(@truncate(@as(u64, @bitCast(inst.imm)))) catch |e| {
                mod.addDiag(.Error, inst_index, "DATA_HALF emit failed: {s}", .{@errorName(e)});
            };
            mod.stats.data_bytes_emitted += 2;
        },
        .JIT_DATA_WORD => {
            mod.commitPendingDataSym();
            mod.emitDataWord(@truncate(@as(u64, @bitCast(inst.imm)))) catch |e| {
                mod.addDiag(.Error, inst_index, "DATA_WORD emit failed: {s}", .{@errorName(e)});
            };
            mod.stats.data_bytes_emitted += 4;
        },
        .JIT_DATA_QUAD => {
            mod.commitPendingDataSym();
            mod.emitDataQuad(@bitCast(inst.imm)) catch |e| {
                mod.addDiag(.Error, inst_index, "DATA_QUAD emit failed: {s}", .{@errorName(e)});
            };
            mod.stats.data_bytes_emitted += 8;
        },
        .JIT_DATA_ZERO => {
            mod.commitPendingDataSym();
            const count: u32 = @truncate(@as(u64, @bitCast(inst.imm)));
            mod.emitDataZero(count) catch |e| {
                mod.addDiag(.Error, inst_index, "DATA_ZERO emit failed: {s}", .{@errorName(e)});
            };
            mod.stats.data_bytes_emitted += count;
        },
        .JIT_DATA_ASCII => {
            mod.commitPendingDataSym();
            // Use imm field for the decoded byte count (set by jit_collect)
            // so that embedded NUL characters are emitted correctly.
            const decoded_len: u32 = if (inst.imm > 0) @intCast(@as(u64, @bitCast(inst.imm))) else @intCast(inst.getSymName().len);
            const text = inst.sym_name[0..decoded_len];
            mod.emitData(text) catch |e| {
                mod.addDiag(.Error, inst_index, "DATA_ASCII emit failed: {s}", .{@errorName(e)});
            };
            mod.stats.data_bytes_emitted += decoded_len;
        },
        .JIT_DATA_ALIGN => {
            const alignment: u32 = @truncate(@as(u64, @bitCast(inst.imm)));
            mod.alignData(alignment) catch |e| {
                mod.addDiag(.Error, inst_index, "DATA_ALIGN emit failed: {s}", .{@errorName(e)});
            };
        },
        .JIT_DATA_SYMREF => {
            mod.commitPendingDataSym();
            // Symbol reference in data — emit 8-byte placeholder, record relocation
            const data_off = mod.data_len;
            mod.emitDataQuad(0) catch |e| {
                mod.addDiag(.Error, inst_index, "DATA_SYMREF emit failed: {s}", .{@errorName(e)});
            };
            // Record the relocation so the linker can patch it with the real address
            const sym_name = inst.getSymName();
            if (sym_name.len > 0) {
                var reloc: DataSymRef = undefined;
                @memcpy(reloc.sym_name[0..sym_name.len], sym_name);
                if (sym_name.len < JIT_SYM_MAX) reloc.sym_name[sym_name.len] = 0;
                reloc.sym_name_len = @intCast(sym_name.len);
                reloc.data_offset = data_off;
                reloc.addend = inst.imm;
                reloc.inst_index = inst_index;
                mod.data_sym_refs.append(mod.allocator, reloc) catch {
                    mod.addDiag(.Error, inst_index, "DATA_SYMREF reloc table full", .{});
                };
            }
            mod.stats.data_bytes_emitted += 8;
        },

        _ => {
            mod.addDiag(.Error, inst_index, "unknown JitInstKind {d}", .{inst.kind});
        },
    }
}

// ============================================================================
// Section: ALU Encoding Helpers
// ============================================================================

const AluOp = enum { Add, Sub, Mul, Sdiv, Udiv, And, Orr, Eor, Lsl, Lsr, Asr };

fn encodeAluRRR(mod: *JitModule, inst: *const JitInst, inst_index: u32, op: AluOp) void {
    const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
    const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
    const rm = mapGPRegister(inst.rm) catch |e| return regError(mod, inst_index, "rm", inst.rm, e);
    const w64 = inst.is64();

    // ARM64 quirk: in shifted-register ADD/SUB, register 31 means XZR
    // (the zero register), NOT SP.  When any operand is SP we must use
    // the extended-register form (UXTX #0) where register 31 *does*
    // mean SP.  This matters for dynamic stack allocation (alloc8)
    // which emits SUB SP, SP, Xn.
    const uses_sp = (rd == Register.SP or rn == Register.SP);
    const rmp = if (uses_sp and (op == .Add or op == .Sub))
        RegisterParam.extended(rm, .UXTX, 0)
    else
        RegisterParam.reg_only(rm);

    const word: u32 = switch (op) {
        .Add => if (w64) enc.emitAddRegister64(rd, rn, rmp) else enc.emitAddRegister(rd, rn, rmp),
        .Sub => if (w64) enc.emitSubRegister64(rd, rn, rmp) else enc.emitSubRegister(rd, rn, rmp),
        .Mul => if (w64) enc.emitMul64(rd, rn, rm) else enc.emitMul(rd, rn, rm),
        .Sdiv => if (w64) enc.emitSdiv64(rd, rn, rm) else enc.emitSdiv(rd, rn, rm),
        .Udiv => if (w64) enc.emitUdiv64(rd, rn, rm) else enc.emitUdiv(rd, rn, rm),
        .And => if (w64) enc.emitAndRegister64(rd, rn, rmp) else enc.emitAndRegister(rd, rn, rmp),
        .Orr => if (w64) enc.emitOrrRegister64(rd, rn, rmp) else enc.emitOrrRegister(rd, rn, rmp),
        .Eor => if (w64) enc.emitEorRegister64(rd, rn, rmp) else enc.emitEorRegister(rd, rn, rmp),
        .Lsl => if (w64) enc.emitLslRegister64(rd, rn, rm) else enc.emitLslRegister(rd, rn, rm),
        .Lsr => if (w64) enc.emitLsrRegister64(rd, rn, rm) else enc.emitLsrRegister(rd, rn, rm),
        .Asr => if (w64) enc.emitAsrRegister64(rd, rn, rm) else enc.emitAsrRegister(rd, rn, rm),
    };

    mod.emitWord(word) catch |e| {
        mod.addDiag(.Error, inst_index, "ALU RRR emit failed: {s}", .{@errorName(e)});
    };
}

fn encodeMaddMsub(mod: *JitModule, inst: *const JitInst, inst_index: u32, is_msub: bool) void {
    const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
    const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
    const rm = mapGPRegister(inst.rm) catch |e| return regError(mod, inst_index, "rm", inst.rm, e);
    const ra = mapGPRegister(inst.ra) catch |e| return regError(mod, inst_index, "ra", inst.ra, e);
    const w64 = inst.is64();

    const word: u32 = if (is_msub)
        (if (w64) enc.emitMsub64(rd, rn, rm, ra) else enc.emitMsub(rd, rn, rm, ra))
    else
        (if (w64) enc.emitMadd64(rd, rn, rm, ra) else enc.emitMadd(rd, rn, rm, ra));

    mod.emitWord(word) catch |e| {
        mod.addDiag(.Error, inst_index, "MADD/MSUB emit failed: {s}", .{@errorName(e)});
    };
}

fn encodeAluRRI(mod: *JitModule, inst: *const JitInst, inst_index: u32, is_sub: bool) void {
    const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
    const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
    const imm12: u12 = @truncate(@as(u64, @bitCast(inst.imm)));
    const w64 = inst.is64();

    const word: u32 = if (is_sub)
        (if (w64) enc.emitSubImmediate64(rd, rn, imm12) else enc.emitSubImmediate(rd, rn, imm12))
    else
        (if (w64) enc.emitAddImmediate64(rd, rn, imm12) else enc.emitAddImmediate(rd, rn, imm12));

    mod.emitWord(word) catch |e| {
        mod.addDiag(.Error, inst_index, "ALU RRI emit failed: {s}", .{@errorName(e)});
    };
}

const ShiftedAluOp = enum { Add, Sub, And, Orr, Eor };

fn encodeShiftedAlu(mod: *JitModule, inst: *const JitInst, inst_index: u32, op: ShiftedAluOp) void {
    const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
    const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
    const rm = mapGPRegister(inst.rm) catch |e| return regError(mod, inst_index, "rm", inst.rm, e);
    const shift = mapShiftType(inst.shift_type);
    const amount: u6 = @truncate(@as(u64, @bitCast(inst.imm2)));
    const rmp = RegisterParam.shifted(rm, shift, amount);
    const w64 = inst.is64();

    const word: u32 = switch (op) {
        .Add => if (w64) enc.emitAddRegister64(rd, rn, rmp) else enc.emitAddRegister(rd, rn, rmp),
        .Sub => if (w64) enc.emitSubRegister64(rd, rn, rmp) else enc.emitSubRegister(rd, rn, rmp),
        .And => if (w64) enc.emitAndRegister64(rd, rn, rmp) else enc.emitAndRegister(rd, rn, rmp),
        .Orr => if (w64) enc.emitOrrRegister64(rd, rn, rmp) else enc.emitOrrRegister(rd, rn, rmp),
        .Eor => if (w64) enc.emitEorRegister64(rd, rn, rmp) else enc.emitEorRegister(rd, rn, rmp),
    };

    mod.emitWord(word) catch |e| {
        mod.addDiag(.Error, inst_index, "Shifted ALU emit failed: {s}", .{@errorName(e)});
    };
}

// ============================================================================
// Section: Move Wide Encoding
// ============================================================================

const MoveWideOp = enum { Movz, Movk, Movn };

fn encodeMoveWide(mod: *JitModule, inst: *const JitInst, inst_index: u32, op: MoveWideOp) void {
    const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
    const imm16: u16 = @truncate(@as(u64, @bitCast(inst.imm)));
    const hw: u6 = @truncate(@as(u64, @bitCast(inst.imm2)));
    const w64 = inst.is64();

    const word: u32 = switch (op) {
        .Movz => if (w64) enc.emitMovz64(rd, imm16, hw) else enc.emitMovz(rd, imm16, hw),
        .Movk => if (w64) enc.emitMovk64(rd, imm16, hw) else enc.emitMovk(rd, imm16, hw),
        .Movn => if (w64) enc.emitMovn64(rd, imm16, hw) else enc.emitMovn(rd, imm16, hw),
    };

    mod.emitWord(word) catch |e| {
        mod.addDiag(.Error, inst_index, "MOV wide emit failed: {s}", .{@errorName(e)});
    };
}

// ============================================================================
// Section: FP Encoding Helpers
// ============================================================================

const FpOp = enum { Fadd, Fsub, Fmul, Fdiv };

fn encodeFpRRR(mod: *JitModule, inst: *const JitInst, inst_index: u32, op: FpOp) void {
    const rd = mapNeonRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd(neon)", inst.rd, e);
    const rn = mapNeonRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn(neon)", inst.rn, e);
    const rm = mapNeonRegister(inst.rm) catch |e| return regError(mod, inst_index, "rm(neon)", inst.rm, e);
    const size = scalarSizeFromCls(inst.cls);

    const word: u32 = switch (op) {
        .Fadd => enc.emitNeonFadd(rd, rn, rm, size),
        .Fsub => enc.emitNeonFsub(rd, rn, rm, size),
        .Fmul => enc.emitNeonFmul(rd, rn, rm, size),
        .Fdiv => enc.emitNeonFdiv(rd, rn, rm, size),
    };

    mod.emitWord(word) catch |e| {
        mod.addDiag(.Error, inst_index, "FP RRR emit failed: {s}", .{@errorName(e)});
    };
}

// ============================================================================
// Section: Extension Encoding Helpers
// ============================================================================

const ExtOp = enum { Sxtb, Uxtb, Sxth, Uxth };

fn encodeExtension(mod: *JitModule, inst: *const JitInst, inst_index: u32, op: ExtOp) void {
    const rd = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
    const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
    const w64 = inst.is64();

    const word: u32 = switch (op) {
        .Sxtb => if (w64) enc.emitSxtb64(rd, rn) else enc.emitSxtb(rd, rn),
        .Uxtb => if (w64) enc.emitUxtb64(rd, rn) else enc.emitUxtb(rd, rn),
        .Sxth => if (w64) enc.emitSxth64(rd, rn) else enc.emitSxth(rd, rn),
        .Uxth => if (w64) enc.emitUxth64(rd, rn) else enc.emitUxth(rd, rn),
    };

    mod.emitWord(word) catch |e| {
        mod.addDiag(.Error, inst_index, "Extension emit failed: {s}", .{@errorName(e)});
    };
}

// ============================================================================
// Section: Load/Store Encoding Helpers
// ============================================================================

const LdrStrOp = enum { Ldr, Ldrb, Ldrh, Ldrsb, Ldrsh, Ldrsw, Str, Strb, Strh };

fn encodeLdrStrOffset(mod: *JitModule, inst: *const JitInst, inst_index: u32, op: LdrStrOp) void {
    const offset: i32 = @intCast(@as(i64, inst.imm));

    // ── FP path: cls is S (single) or D (double) ──
    // ARM64 FP loads/stores use different opcodes (V bit set) and the
    // data register is a NEON/FP register while the address register
    // is still a GP register.
    if (inst.isFloat() and (op == .Ldr or op == .Str)) {
        const fp_rt = mapNeonRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd(neon)", inst.rd, e);
        const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);

        const word_opt: ?u32 = switch (op) {
            .Ldr => if (inst.isDouble()) enc.emitFpLdrDOffset(fp_rt, rn, offset) else enc.emitFpLdrSOffset(fp_rt, rn, offset),
            .Str => if (inst.isDouble()) enc.emitFpStrDOffset(fp_rt, rn, offset) else enc.emitFpStrSOffset(fp_rt, rn, offset),
            else => unreachable,
        };

        if (word_opt) |w| {
            mod.emitWord(w) catch |e| {
                mod.addDiag(.Error, inst_index, "FP LDR/STR offset emit failed: {s}", .{@errorName(e)});
            };
        } else {
            mod.addDiag(.Error, inst_index, "FP LDR/STR offset out of range: {d}", .{offset});
        }
        return;
    }

    // ── GP path ──
    const rt = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
    const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
    const w64 = inst.is64();

    // Try to encode as scaled unsigned offset first, fall back to unscaled
    const word_opt: ?u32 = switch (op) {
        .Ldr => if (w64) enc.emitLdrOffset64(rt, rn, offset) else enc.emitLdrOffset(rt, rn, offset),
        .Ldrb => enc.emitLdrbOffset(rt, rn, offset),
        .Ldrh => enc.emitLdrhOffset(rt, rn, offset),
        .Ldrsb => if (w64) enc.emitLdrsbOffset64(rt, rn, offset) else enc.emitLdrsbOffset(rt, rn, offset),
        .Ldrsh => if (w64) enc.emitLdrshOffset64(rt, rn, offset) else enc.emitLdrshOffset(rt, rn, offset),
        .Ldrsw => enc.emitLdrswOffset64(rt, rn, offset),
        .Str => if (w64) enc.emitStrOffset64(rt, rn, offset) else enc.emitStrOffset(rt, rn, offset),
        .Strb => enc.emitStrbOffset(rt, rn, offset),
        .Strh => enc.emitStrhOffset(rt, rn, offset),
    };

    if (word_opt) |w| {
        mod.emitWord(w) catch |e| {
            mod.addDiag(.Error, inst_index, "LDR/STR offset emit failed: {s}", .{@errorName(e)});
        };
    } else {
        mod.addDiag(.Error, inst_index, "LDR/STR offset out of range: {d}", .{offset});
    }
}

fn encodeLdrStrRegister(mod: *JitModule, inst: *const JitInst, inst_index: u32, op: LdrStrOp) void {
    const rt = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
    const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
    const rm = mapGPRegister(inst.rm) catch |e| return regError(mod, inst_index, "rm", inst.rm, e);
    const w64 = inst.is64();

    const rmp = RegisterParam.reg_only(rm);
    const word: u32 = switch (op) {
        .Ldr => if (w64) enc.emitLdrRegister64(rt, rn, rmp) else enc.emitLdrRegister(rt, rn, rmp),
        .Str => if (w64) enc.emitStrRegister64(rt, rn, rmp) else enc.emitStrRegister(rt, rn, rmp),
        .Ldrb => enc.emitLdrbRegister(rt, rn, rmp),
        .Ldrh => enc.emitLdrhRegister(rt, rn, rmp),
        .Ldrsb => if (w64) enc.emitLdrsbRegister64(rt, rn, rmp) else enc.emitLdrsbRegister(rt, rn, rmp),
        .Ldrsh => if (w64) enc.emitLdrshRegister64(rt, rn, rmp) else enc.emitLdrshRegister(rt, rn, rmp),
        .Ldrsw => enc.emitLdrswRegister64(rt, rn, rmp),
        .Strb => enc.emitStrbRegister(rt, rn, rmp),
        .Strh => enc.emitStrhRegister(rt, rn, rmp),
    };

    mod.emitWord(word) catch |e| {
        mod.addDiag(.Error, inst_index, "LDR/STR register emit failed: {s}", .{@errorName(e)});
    };
}

// ============================================================================
// Section: Load/Store Pair Encoding
// ============================================================================

const LdpStpOp = enum { Ldp, Stp, LdpPost, StpPre };

fn encodeLdpStp(mod: *JitModule, inst: *const JitInst, inst_index: u32, op: LdpStpOp, comptime _: bool) void {
    const rt1 = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);
    const rt2 = mapGPRegister(inst.rm) catch |e| return regError(mod, inst_index, "rm", inst.rm, e);
    const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);
    const offset: i32 = @intCast(@as(i64, inst.imm));
    const w64 = inst.is64();

    const word_opt: ?u32 = switch (op) {
        .Ldp => if (w64) enc.emitLdpOffset64(rt1, rt2, rn, offset) else enc.emitLdpOffset(rt1, rt2, rn, offset),
        .Stp => if (w64) enc.emitStpOffset64(rt1, rt2, rn, offset) else enc.emitStpOffset(rt1, rt2, rn, offset),
        .LdpPost => if (w64) enc.emitLdpOffsetPostIndex64(rt1, rt2, rn, offset) else enc.emitLdpOffsetPostIndex(rt1, rt2, rn, offset),
        .StpPre => if (w64) enc.emitStpOffsetPreIndex64(rt1, rt2, rn, offset) else enc.emitStpOffsetPreIndex(rt1, rt2, rn, offset),
    };

    if (word_opt) |w| {
        mod.emitWord(w) catch |e| {
            mod.addDiag(.Error, inst_index, "LDP/STP emit failed: {s}", .{@errorName(e)});
        };
    } else {
        mod.addDiag(.Error, inst_index, "LDP/STP offset out of range: {d}", .{offset});
    }
}

// ============================================================================
// Section: Branch Encoding
// ============================================================================

const BranchOp = enum { B, BL, BCond, CBZ, CBNZ };

fn encodeBranch(mod: *JitModule, inst: *const JitInst, inst_index: u32, op: BranchOp) void {
    const target_id = inst.target_id;

    // Check if label is already known (backward branch)
    if (mod.labels.get(target_id)) |target_offset| {
        // Backward branch — we can compute the offset now
        const current_offset = mod.code_len;
        const delta: i64 = @as(i64, @intCast(target_offset)) - @as(i64, @intCast(current_offset));
        const delta_i26: i26 = @intCast(@divExact(delta, 4));

        const word: ?u32 = switch (op) {
            .B => @as(?u32, enc.emitB(delta_i26)),
            .BL => @as(?u32, enc.emitBl(delta_i26)),
            .BCond => blk: {
                const cond = mapCondition(inst.cond);
                const delta_i19: i19 = @intCast(@divExact(delta, 4));
                break :blk @as(?u32, enc.emitBCond(delta_i19, cond));
            },
            .CBZ => blk: {
                const rt = mapGPRegister(inst.rd) catch |e| {
                    regError(mod, inst_index, "rd", inst.rd, e);
                    break :blk null;
                };
                const delta_i19: i19 = @intCast(@divExact(delta, 4));
                break :blk if (inst.is64()) enc.emitCbz64(rt, delta_i19) else enc.emitCbz(rt, delta_i19);
            },
            .CBNZ => blk: {
                const rt = mapGPRegister(inst.rd) catch |e| {
                    regError(mod, inst_index, "rd", inst.rd, e);
                    break :blk null;
                };
                const delta_i19: i19 = @intCast(@divExact(delta, 4));
                break :blk if (inst.is64()) enc.emitCbnz64(rt, delta_i19) else enc.emitCbnz(rt, delta_i19);
            },
        };

        if (word) |w| {
            mod.emitWord(w) catch |e| {
                mod.addDiag(.Error, inst_index, "branch emit failed: {s}", .{@errorName(e)});
            };
        }
    } else {
        // Forward branch — emit placeholder and record fixup
        const branch_class: BranchClass = switch (op) {
            .B, .BL => .Imm26,
            .BCond, .CBZ, .CBNZ => .Imm19,
        };

        // Build the base opcode (with offset=0)
        const zero_i26: i26 = 0;
        const zero_i19: i19 = 0;
        var base: u32 = switch (op) {
            .B => enc.emitB(zero_i26),
            .BL => enc.emitBl(zero_i26),
            .BCond => enc.emitBCond(zero_i19, mapCondition(inst.cond)),
            .CBZ => blk: {
                const rt = mapGPRegister(inst.rd) catch |e| {
                    regError(mod, inst_index, "rd", inst.rd, e);
                    break :blk enc.emitNop();
                };
                break :blk if (inst.is64()) enc.emitCbz64(rt, zero_i19) else enc.emitCbz(rt, zero_i19);
            },
            .CBNZ => blk: {
                const rt = mapGPRegister(inst.rd) catch |e| {
                    regError(mod, inst_index, "rd", inst.rd, e);
                    break :blk enc.emitNop();
                };
                break :blk if (inst.is64()) enc.emitCbnz64(rt, zero_i19) else enc.emitCbnz(rt, zero_i19);
            },
        };
        _ = &base;

        const fixup_offset = mod.code_len;
        mod.emitWord(base) catch |e| {
            mod.addDiag(.Error, inst_index, "branch placeholder emit failed: {s}", .{@errorName(e)});
            return;
        };

        mod.fixups.append(mod.allocator, BranchFixup{
            .code_offset = fixup_offset,
            .target_id = target_id,
            .branch_class = branch_class,
            .inst_index = inst_index,
            .base_opcode = base,
        }) catch {
            mod.addDiag(.Error, inst_index, "fixup table full", .{});
        };
        mod.stats.fixups_created += 1;
    }
}

// ============================================================================
// Section: NEON Encoding Helpers
// ============================================================================

const NeonBinOp = enum { Add, Sub, Mul, Div, Min, Max };

fn encodeNeonBinOp(mod: *JitModule, inst: *const JitInst, inst_index: u32, op: NeonBinOp) void {
    // QBE's NEON ops use fixed registers (v28, v29, v30) with arrangement from imm
    // The arrangement is stored in inst.imm
    const arr_val: u8 = @truncate(@as(u64, @bitCast(inst.imm)));
    const is_float = inst.is_float != 0;
    const size = mapNeonArr(arr_val, is_float);

    // QBE uses v28 as destination/first source, v29 as second source
    const vd = NeonRegister.V28;
    const vn = NeonRegister.V28;
    const vm = NeonRegister.V29;

    const word: u32 = switch (op) {
        .Add => if (is_float) enc.emitNeonFadd(vd, vn, vm, size) else enc.emitNeonAdd(vd, vn, vm, size),
        .Sub => if (is_float) enc.emitNeonFsub(vd, vn, vm, size) else enc.emitNeonSub(vd, vn, vm, size),
        .Mul => if (is_float) enc.emitNeonFmul(vd, vn, vm, size) else enc.emitNeonMul(vd, vn, vm, size),
        .Div => enc.emitNeonFdiv(vd, vn, vm, size), // integer div not supported on NEON
        .Min => if (is_float) enc.emitNeonFmin(vd, vn, vm, size) else enc.emitNeonSmin(vd, vn, vm, size),
        .Max => if (is_float) enc.emitNeonFmax(vd, vn, vm, size) else enc.emitNeonSmax(vd, vn, vm, size),
    };

    mod.emitWord(word) catch |e| {
        mod.addDiag(.Error, inst_index, "NEON binop emit failed: {s}", .{@errorName(e)});
    };
    mod.stats.neon_ops_encoded += 1;
}

const NeonUnaryOp = enum { Neg, Abs };

fn encodeNeonUnaryOp(mod: *JitModule, inst: *const JitInst, inst_index: u32, op: NeonUnaryOp) void {
    const arr_val: u8 = @truncate(@as(u64, @bitCast(inst.imm)));
    const is_float = inst.is_float != 0;
    const size = mapNeonArr(arr_val, is_float);

    const vd = NeonRegister.V28;
    const vn = NeonRegister.V28;

    const word: u32 = switch (op) {
        .Neg => if (is_float) enc.emitNeonFneg(vd, vn, size) else enc.emitNeonNeg(vd, vn, size),
        .Abs => if (is_float) enc.emitNeonFabs(vd, vn, size) else enc.emitNeonAbs(vd, vn, size),
    };

    mod.emitWord(word) catch |e| {
        mod.addDiag(.Error, inst_index, "NEON unary emit failed: {s}", .{@errorName(e)});
    };
    mod.stats.neon_ops_encoded += 1;
}

fn encodeNeonFma(mod: *JitModule, inst: *const JitInst, inst_index: u32) void {
    const arr_val: u8 = @truncate(@as(u64, @bitCast(inst.imm)));
    const is_float = inst.is_float != 0;
    const size = mapNeonArr(arr_val, is_float);

    // v28 += v29 * v30
    const vd = NeonRegister.V28;
    const vn = NeonRegister.V29;
    const vm = NeonRegister.V30;

    const word: u32 = if (is_float) enc.emitNeonFmla(vd, vn, vm, size) else enc.emitNeonMla(vd, vn, vm, size);

    mod.emitWord(word) catch |e| {
        mod.addDiag(.Error, inst_index, "NEON FMA emit failed: {s}", .{@errorName(e)});
    };
    mod.stats.neon_ops_encoded += 1;
}

fn encodeNeonDup(mod: *JitModule, inst: *const JitInst, inst_index: u32) void {
    // DUP v28.arr, Wn/Xn — broadcast scalar GP register to all lanes
    const arr_val: u8 = @truncate(@as(u64, @bitCast(inst.imm)));
    const is_float = inst.is_float != 0;
    const size = mapNeonArr(arr_val, is_float);

    // The GP source register is in arg[1] which maps to inst.rm in the collector
    const src_gp = mapGPRegister(inst.rm) catch |e| return regError(mod, inst_index, "rm(gp for dup)", inst.rm, e);

    const word = enc.emitNeonDup(NeonRegister.V28, src_gp, size);

    mod.emitWord(word) catch |e| {
        mod.addDiag(.Error, inst_index, "NEON DUP emit failed: {s}", .{@errorName(e)});
    };
    mod.stats.neon_ops_encoded += 1;
}

fn encodeNeonAddv(mod: *JitModule, inst: *const JitInst, inst_index: u32) void {
    // Horizontal reduction — multiple instructions depending on arrangement.
    // This is a multi-instruction sequence matching QBE's emit.c:
    //   .4s int:  addv s28, v28.4s ; fmov wDest, s28
    //   .2d:      addp d28, v28.2d ; fmov xDest, d28
    //   .8h:      addv h28, v28.8h ; smov wDest, v28.h[0]
    //   .16b:     addv b28, v28.16b; smov wDest, v28.b[0]
    //   .4s flt:  faddp v28.4s, v28.4s, v28.4s ; faddp s28, v28.2s ; fmov wDest, s28
    const arr_val: u8 = @truncate(@as(u64, @bitCast(inst.imm)));
    const dest_gp = mapGPRegister(inst.rd) catch |e| return regError(mod, inst_index, "rd", inst.rd, e);

    const arr: JitNeonArr = @enumFromInt(arr_val);

    switch (arr) {
        .NEON_4S => {
            // Integer: addv s28, v28.4s
            mod.emitWord(enc.emitNeonAddv(NeonRegister.V28, NeonRegister.V28, NeonSize.Size4S)) catch return;
            // fmov wDest, s28
            mod.emitWord(enc.emitNeonFmovToGeneral(dest_gp, NeonRegister.V28)) catch return;
        },
        .NEON_2D => {
            // addp d28, v28.2d
            mod.emitWord(enc.emitNeonAddpScalar(NeonRegister.V28, NeonRegister.V28, NeonSize.Size2D)) catch return;
            // fmov xDest, d28
            mod.emitWord(enc.emitNeonFmovToGeneral64(dest_gp, NeonRegister.V28)) catch return;
        },
        .NEON_4SF => {
            // Float: faddp v28.4s, v28.4s, v28.4s
            mod.emitWord(enc.emitNeonFaddp(NeonRegister.V28, NeonRegister.V28, NeonRegister.V28, NeonSize.Size4S)) catch return;
            // faddp s28, v28.2s
            mod.emitWord(enc.emitNeonFaddpScalar(NeonRegister.V28, NeonRegister.V28, NeonSize.Size2S)) catch return;
            // fmov wDest, s28
            mod.emitWord(enc.emitNeonFmovToGeneral(dest_gp, NeonRegister.V28)) catch return;
        },
        .NEON_2DF => {
            // Float 2D: faddp d28, v28.2d
            mod.emitWord(enc.emitNeonFaddpScalar(NeonRegister.V28, NeonRegister.V28, NeonSize.Size2D)) catch return;
            // fmov xDest, d28
            mod.emitWord(enc.emitNeonFmovToGeneral64(dest_gp, NeonRegister.V28)) catch return;
        },
        .NEON_8H => {
            // addv h28, v28.8h
            mod.emitWord(enc.emitNeonAddv(NeonRegister.V28, NeonRegister.V28, NeonSize.Size8H)) catch return;
            // smov wDest, v28.h[0]
            mod.emitWord(enc.emitNeonSmov(dest_gp, NeonRegister.V28, 0, NeonSize.Size1H)) catch return;
        },
        .NEON_16B => {
            // addv b28, v28.16b
            mod.emitWord(enc.emitNeonAddv(NeonRegister.V28, NeonRegister.V28, NeonSize.Size16B)) catch return;
            // smov wDest, v28.b[0]
            mod.emitWord(enc.emitNeonSmov(dest_gp, NeonRegister.V28, 0, NeonSize.Size1B)) catch return;
        },
    }
    mod.stats.neon_ops_encoded += 1;
}

fn encodeNeonLdrStr(mod: *JitModule, inst: *const JitInst, inst_index: u32, is_load: bool) void {
    // NEON LDR/STR Qn, [Xn] — 128-bit load/store
    // imm2 selects the vector register: 0 or 28 → V28, 29 → V29, 30 → V30
    const rn = mapGPRegister(inst.rn) catch |e| return regError(mod, inst_index, "rn", inst.rn, e);

    const vreg: NeonRegister = switch (@as(i32, @truncate(inst.imm2))) {
        29 => NeonRegister.V29,
        30 => NeonRegister.V30,
        else => NeonRegister.V28,
    };

    const word_opt: ?u32 = if (is_load)
        enc.emitNeonLdrOffset(vreg, NeonSize.Size1Q, rn, 0)
    else
        enc.emitNeonStrOffset(vreg, NeonSize.Size1Q, rn, 0);

    if (word_opt) |w| {
        mod.emitWord(w) catch |e| {
            mod.addDiag(.Error, inst_index, "NEON LDR/STR Q emit failed: {s}", .{@errorName(e)});
        };
    } else {
        mod.addDiag(.Error, inst_index, "NEON LDR/STR Q encoding failed", .{});
    }
    mod.stats.neon_ops_encoded += 1;
}

// ============================================================================
// Section: Error Helpers
// ============================================================================

fn regError(mod: *JitModule, inst_index: u32, name: []const u8, reg_id: i32, _: EncodeError) void {
    mod.addDiag(.Error, inst_index, "invalid register {s}={d}", .{ name, reg_id });
}

// ============================================================================
// Section: Fixup Resolution (Pass 2)
// ============================================================================

/// Resolve all forward branch fixups.
/// Called after all instructions have been encoded and all labels are known.
pub fn resolveFixups(mod: *JitModule) void {
    for (mod.fixups.items) |fixup| {
        if (mod.labels.get(fixup.target_id)) |target_offset| {
            // Compute delta in bytes, then convert to instruction words (÷4)
            const delta_bytes: i64 = @as(i64, @intCast(target_offset)) - @as(i64, @intCast(fixup.code_offset));
            const delta_words: i32 = @intCast(@divExact(delta_bytes, 4));
            const delta_u32: u32 = @bitCast(delta_words);

            const base_word = mod.readWord(fixup.code_offset);
            var patched: u32 = base_word;

            switch (fixup.branch_class) {
                .Imm26 => {
                    // B / BL: bits [25:0] = offset in instruction words
                    patched = (base_word & 0xfc000000) | (delta_u32 & 0x03ffffff);
                },
                .Imm19 => {
                    // B.cond / CBZ / CBNZ: bits [23:5] = offset in instruction words
                    patched = (base_word & 0xff00001f) | ((delta_u32 & 0x7ffff) << 5);
                },
                .Imm14 => {
                    // TBZ / TBNZ: bits [18:5] = offset in instruction words
                    patched = (base_word & 0xfff8001f) | ((delta_u32 & 0x3fff) << 5);
                },
                .Invalid => {
                    mod.addDiag(.Error, fixup.inst_index, "invalid branch class for fixup", .{});
                    continue;
                },
            }

            mod.patchWord(fixup.code_offset, patched);
            mod.stats.fixups_resolved += 1;
        } else {
            mod.addDiag(.Error, fixup.inst_index, "unresolved label: block {d}", .{fixup.target_id});
        }
    }
}

// ============================================================================
// Section: Top-Level Encode Function
// ============================================================================

/// Encode a JitInst array into machine code.
///
/// This is the main entry point. It:
/// 1. Iterates over all instructions (Pass 1: emit + record fixups)
/// 2. Resolves forward branch fixups (Pass 2)
/// 3. Returns a JitModule containing code, data, symbols, and diagnostics
///
/// The caller owns the returned JitModule and must call deinit() when done.
pub fn jitEncode(
    insts: [*]const JitInst,
    ninst: u32,
    allocator: std.mem.Allocator,
    code_capacity: u32,
    data_capacity: u32,
) !JitModule {
    var mod = try JitModule.init(allocator, code_capacity, data_capacity);

    // Pass 1: Emit instructions
    var i: u32 = 0;
    while (i < ninst) : (i += 1) {
        encodeInstruction(&mod, &insts[i], i);
    }

    // Pass 2: Resolve forward branches
    resolveFixups(&mod);

    return mod;
}

// ============================================================================
// Section: Introspection & Dump Utilities
// ============================================================================

/// Print a human-readable dump of the JitInst array.
/// This is the Zig-side equivalent of jit_collector_dump().
pub fn dumpInstructions(insts: [*]const JitInst, ninst: u32, writer: anytype) !void {
    try writer.print("\n=== JitInst Dump ({d} instructions) ===\n\n", .{ninst});

    var i: u32 = 0;
    while (i < ninst) : (i += 1) {
        const inst = &insts[i];
        try dumpSingleInstruction(inst, i, writer);
    }

    try writer.print("\n=== End Dump ===\n", .{});
}

/// Print a single JitInst in human-readable form.
pub fn dumpSingleInstruction(inst: *const JitInst, index: u32, writer: anytype) !void {
    const kind = inst.getKind();

    try writer.print("  [{d:4}] ", .{index});

    switch (kind) {
        .JIT_LABEL => try writer.print("LABEL       .L{d}\n", .{inst.target_id}),
        .JIT_FUNC_BEGIN => try writer.print("FUNC_BEGIN  {s}  frame={d}\n", .{ inst.getSymName(), inst.imm }),
        .JIT_FUNC_END => try writer.print("FUNC_END\n", .{}),
        .JIT_DBGLOC => try writer.print("DBGLOC      line={d} col={d}\n", .{ inst.imm, inst.imm2 }),
        .JIT_NOP => try writer.print("NOP\n", .{}),
        .JIT_COMMENT => try writer.print("// {s}\n", .{inst.getSymName()}),

        .JIT_ADD_RRR => try dumpRRR(writer, "ADD", inst),
        .JIT_SUB_RRR => try dumpRRR(writer, "SUB", inst),
        .JIT_MUL_RRR => try dumpRRR(writer, "MUL", inst),
        .JIT_SDIV_RRR => try dumpRRR(writer, "SDIV", inst),
        .JIT_UDIV_RRR => try dumpRRR(writer, "UDIV", inst),
        .JIT_AND_RRR => try dumpRRR(writer, "AND", inst),
        .JIT_ORR_RRR => try dumpRRR(writer, "ORR", inst),
        .JIT_EOR_RRR => try dumpRRR(writer, "EOR", inst),
        .JIT_LSL_RRR => try dumpRRR(writer, "LSL", inst),
        .JIT_LSR_RRR => try dumpRRR(writer, "LSR", inst),
        .JIT_ASR_RRR => try dumpRRR(writer, "ASR", inst),
        .JIT_NEG_RR => try dumpRR(writer, "NEG", inst),

        .JIT_MADD_RRRR => try dumpRRRR(writer, "MADD", inst),
        .JIT_MSUB_RRRR => try dumpRRRR(writer, "MSUB", inst),

        .JIT_ADD_RRI => try dumpRRI(writer, "ADD", inst),
        .JIT_SUB_RRI => try dumpRRI(writer, "SUB", inst),

        .JIT_MOV_RR => try dumpRR(writer, "MOV", inst),
        .JIT_MOVZ => try dumpMoveWide(writer, "MOVZ", inst),
        .JIT_MOVK => try dumpMoveWide(writer, "MOVK", inst),
        .JIT_MOVN => try dumpMoveWide(writer, "MOVN", inst),
        .JIT_MOV_WIDE_IMM => try writer.print("MOV_WIDE    {s}, #0x{x}\n", .{ regName(inst.rd), @as(u64, @bitCast(inst.imm)) }),

        .JIT_FADD_RRR => try dumpFpRRR(writer, "FADD", inst),
        .JIT_FSUB_RRR => try dumpFpRRR(writer, "FSUB", inst),
        .JIT_FMUL_RRR => try dumpFpRRR(writer, "FMUL", inst),
        .JIT_FDIV_RRR => try dumpFpRRR(writer, "FDIV", inst),
        .JIT_FNEG_RR => try dumpFpRR(writer, "FNEG", inst),
        .JIT_FMOV_RR => try dumpFpRR(writer, "FMOV", inst),

        .JIT_FCVT_SD => try dumpFpRR(writer, "FCVT(S→D)", inst),
        .JIT_FCVT_DS => try dumpFpRR(writer, "FCVT(D→S)", inst),
        .JIT_FCVTZS => try writer.print("FCVTZS      {s}, {s}\n", .{ regName(inst.rd), fpRegName(inst.rn, inst.cls) }),
        .JIT_FCVTZU => try writer.print("FCVTZU      {s}, {s}\n", .{ regName(inst.rd), fpRegName(inst.rn, inst.cls) }),
        .JIT_SCVTF => try writer.print("SCVTF       {s}, {s}\n", .{ fpRegName(inst.rd, inst.cls), regName(inst.rn) }),
        .JIT_UCVTF => try writer.print("UCVTF       {s}, {s}\n", .{ fpRegName(inst.rd, inst.cls), regName(inst.rn) }),
        .JIT_FMOV_GF => try writer.print("FMOV        {s}, {s}  (FP→GP)\n", .{ regName(inst.rd), fpRegName(inst.rn, inst.cls) }),
        .JIT_FMOV_FG => try writer.print("FMOV        {s}, {s}  (GP→FP)\n", .{ fpRegName(inst.rd, inst.cls), regName(inst.rn) }),

        .JIT_CMP_RR => try writer.print("CMP         {s}, {s}\n", .{ regName(inst.rn), regName(inst.rm) }),
        .JIT_CMP_RI => try writer.print("CMP         {s}, #{d}\n", .{ regName(inst.rn), inst.imm }),
        .JIT_CMN_RR => try writer.print("CMN         {s}, {s}\n", .{ regName(inst.rn), regName(inst.rm) }),
        .JIT_FCMP_RR => try writer.print("FCMPE       {s}, {s}\n", .{ fpRegName(inst.rn, inst.cls), fpRegName(inst.rm, inst.cls) }),
        .JIT_TST_RR => try writer.print("TST         {s}, {s}\n", .{ regName(inst.rn), regName(inst.rm) }),

        .JIT_CSET => try writer.print("CSET        {s}, {s}\n", .{ regName(inst.rd), condName(inst.cond) }),
        .JIT_CSEL => try writer.print("CSEL        {s}, {s}, {s}, {s}\n", .{ regName(inst.rd), regName(inst.rn), regName(inst.rm), condName(inst.cond) }),

        .JIT_B => try writer.print("B           .L{d}\n", .{inst.target_id}),
        .JIT_BL => try writer.print("BL          .L{d}\n", .{inst.target_id}),
        .JIT_B_COND => try writer.print("B.{s}       .L{d}\n", .{ condName(inst.cond), inst.target_id }),
        .JIT_CBZ => try writer.print("CBZ         {s}, .L{d}\n", .{ regName(inst.rd), inst.target_id }),
        .JIT_CBNZ => try writer.print("CBNZ        {s}, .L{d}\n", .{ regName(inst.rd), inst.target_id }),
        .JIT_BR => try writer.print("BR          {s}\n", .{regName(inst.rn)}),
        .JIT_BLR => try writer.print("BLR         {s}\n", .{regName(inst.rn)}),
        .JIT_RET => try writer.print("RET\n", .{}),
        .JIT_CALL_EXT => try writer.print("CALL_EXT    {s}\n", .{inst.getSymName()}),

        .JIT_LDR_RI => try dumpMemRI(writer, "LDR", inst),
        .JIT_STR_RI => try dumpMemRI(writer, "STR", inst),
        .JIT_LDRB_RI => try dumpMemRI(writer, "LDRB", inst),
        .JIT_LDRH_RI => try dumpMemRI(writer, "LDRH", inst),
        .JIT_LDRSB_RI => try dumpMemRI(writer, "LDRSB", inst),
        .JIT_LDRSH_RI => try dumpMemRI(writer, "LDRSH", inst),
        .JIT_LDRSW_RI => try dumpMemRI(writer, "LDRSW", inst),
        .JIT_STRB_RI => try dumpMemRI(writer, "STRB", inst),
        .JIT_STRH_RI => try dumpMemRI(writer, "STRH", inst),

        .JIT_LDR_RR => try writer.print("LDR         {s}, [{s}, {s}]\n", .{ regName(inst.rd), regName(inst.rn), regName(inst.rm) }),
        .JIT_STR_RR => try writer.print("STR         {s}, [{s}, {s}]\n", .{ regName(inst.rd), regName(inst.rn), regName(inst.rm) }),
        .JIT_LDRB_RR => try writer.print("LDRB        {s}, [{s}, {s}]\n", .{ regName(inst.rd), regName(inst.rn), regName(inst.rm) }),
        .JIT_LDRH_RR => try writer.print("LDRH        {s}, [{s}, {s}]\n", .{ regName(inst.rd), regName(inst.rn), regName(inst.rm) }),
        .JIT_LDRSB_RR => try writer.print("LDRSB       {s}, [{s}, {s}]\n", .{ regName(inst.rd), regName(inst.rn), regName(inst.rm) }),
        .JIT_LDRSH_RR => try writer.print("LDRSH       {s}, [{s}, {s}]\n", .{ regName(inst.rd), regName(inst.rn), regName(inst.rm) }),
        .JIT_LDRSW_RR => try writer.print("LDRSW       {s}, [{s}, {s}]\n", .{ regName(inst.rd), regName(inst.rn), regName(inst.rm) }),
        .JIT_STRB_RR => try writer.print("STRB        {s}, [{s}, {s}]\n", .{ regName(inst.rd), regName(inst.rn), regName(inst.rm) }),
        .JIT_STRH_RR => try writer.print("STRH        {s}, [{s}, {s}]\n", .{ regName(inst.rd), regName(inst.rn), regName(inst.rm) }),

        .JIT_LDP => try writer.print("LDP         {s}, {s}, [{s}, #{d}]\n", .{ regName(inst.rd), regName(inst.rm), regName(inst.rn), inst.imm }),
        .JIT_STP => try writer.print("STP         {s}, {s}, [{s}, #{d}]\n", .{ regName(inst.rd), regName(inst.rm), regName(inst.rn), inst.imm }),
        .JIT_LDP_POST => try writer.print("LDP         {s}, {s}, [{s}], #{d}\n", .{ regName(inst.rd), regName(inst.rm), regName(inst.rn), inst.imm }),
        .JIT_STP_PRE => try writer.print("STP         {s}, {s}, [{s}, #{d}]!\n", .{ regName(inst.rd), regName(inst.rm), regName(inst.rn), inst.imm }),

        .JIT_ADRP => try writer.print("ADRP        {s}, {s}\n", .{ regName(inst.rd), inst.getSymName() }),
        .JIT_ADR => try writer.print("ADR         {s}, {s}\n", .{ regName(inst.rd), inst.getSymName() }),
        .JIT_LOAD_ADDR => {
            if (inst.imm != 0) {
                try writer.print("LOAD_ADDR   {s}, {s}+{d}\n", .{ regName(inst.rd), inst.getSymName(), inst.imm });
            } else {
                try writer.print("LOAD_ADDR   {s}, {s}\n", .{ regName(inst.rd), inst.getSymName() });
            }
        },

        .JIT_HINT => try writer.print("HINT        #{d}\n", .{inst.imm}),
        .JIT_BRK => try writer.print("BRK         #{d}\n", .{inst.imm}),

        .JIT_SUB_SP => try writer.print("SUB         SP, SP, #{d}\n", .{inst.imm}),
        .JIT_ADD_SP => try writer.print("ADD         SP, SP, #{d}\n", .{inst.imm}),
        .JIT_MOV_SP => try writer.print("MOV         SP ↔ {s}\n", .{regName(if (inst.rd != JIT_REG_NONE and inst.rd != JIT_REG_SP) inst.rd else inst.rn)}),

        .JIT_NEON_ADD => try writer.print("NEON ADD    v28, v28, v29  arr={d}\n", .{inst.imm}),
        .JIT_NEON_SUB => try writer.print("NEON SUB    v28, v28, v29  arr={d}\n", .{inst.imm}),
        .JIT_NEON_MUL => try writer.print("NEON MUL    v28, v28, v29  arr={d}\n", .{inst.imm}),
        .JIT_NEON_DUP => try writer.print("NEON DUP    v28, {s}  arr={d}\n", .{ regName(inst.rm), inst.imm }),
        .JIT_NEON_LDR_Q => try writer.print("NEON LDR    q28, [{s}]\n", .{regName(inst.rn)}),
        .JIT_NEON_STR_Q => try writer.print("NEON STR    q28, [{s}]\n", .{regName(inst.rn)}),

        .JIT_ADD_SHIFT => try writer.print("ADD         {s}, {s}, {s}, shift #{d}\n", .{ regName(inst.rd), regName(inst.rn), regName(inst.rm), inst.imm }),
        .JIT_SUB_SHIFT => try writer.print("SUB         {s}, {s}, {s}, shift #{d}\n", .{ regName(inst.rd), regName(inst.rn), regName(inst.rm), inst.imm }),
        .JIT_AND_SHIFT => try writer.print("AND         {s}, {s}, {s}, shift #{d}\n", .{ regName(inst.rd), regName(inst.rn), regName(inst.rm), inst.imm }),
        .JIT_ORR_SHIFT => try writer.print("ORR         {s}, {s}, {s}, shift #{d}\n", .{ regName(inst.rd), regName(inst.rn), regName(inst.rm), inst.imm }),
        .JIT_EOR_SHIFT => try writer.print("EOR         {s}, {s}, {s}, shift #{d}\n", .{ regName(inst.rd), regName(inst.rn), regName(inst.rm), inst.imm }),

        .JIT_DATA_START => try writer.print("DATA_START  {s}\n", .{inst.getSymName()}),
        .JIT_DATA_END => try writer.print("DATA_END\n", .{}),
        .JIT_DATA_BYTE => try writer.print("DATA_BYTE   0x{x}\n", .{@as(u8, @truncate(@as(u64, @bitCast(inst.imm))))}),
        .JIT_DATA_HALF => try writer.print("DATA_HALF   0x{x}\n", .{@as(u16, @truncate(@as(u64, @bitCast(inst.imm))))}),
        .JIT_DATA_WORD => try writer.print("DATA_WORD   0x{x}\n", .{@as(u32, @truncate(@as(u64, @bitCast(inst.imm))))}),
        .JIT_DATA_QUAD => try writer.print("DATA_QUAD   0x{x}\n", .{@as(u64, @bitCast(inst.imm))}),
        .JIT_DATA_ZERO => try writer.print("DATA_ZERO   {d} bytes\n", .{inst.imm}),
        .JIT_DATA_SYMREF => try writer.print("DATA_SYMREF {s}\n", .{inst.getSymName()}),
        .JIT_DATA_ASCII => try writer.print("DATA_ASCII  \"{s}\"\n", .{inst.getSymName()}),
        .JIT_DATA_ALIGN => try writer.print("DATA_ALIGN  {d}\n", .{inst.imm}),

        else => try writer.print("??? kind={d}\n", .{inst.kind}),
    }
}

// ── Dump format helpers ──

fn regName(reg_id: i32) []const u8 {
    return switch (reg_id) {
        JIT_REG_SP => "sp",
        JIT_REG_FP => "x29",
        JIT_REG_LR => "x30",
        JIT_REG_IP0 => "x16",
        JIT_REG_IP1 => "x17",
        JIT_REG_NONE => "---",
        else => if (reg_id >= 0 and reg_id <= 30) blk: {
            const names = [_][]const u8{
                "r0",  "r1",  "r2",  "r3",  "r4",  "r5",  "r6",  "r7",
                "r8",  "r9",  "r10", "r11", "r12", "r13", "r14", "r15",
                "r16", "r17", "r18", "r19", "r20", "r21", "r22", "r23",
                "r24", "r25", "r26", "r27", "r28", "r29", "r30",
            };
            break :blk names[@intCast(reg_id)];
        } else if (reg_id <= JIT_VREG_BASE) blk: {
            const idx = JIT_VREG_BASE - reg_id;
            if (idx >= 0 and idx <= 31) {
                const vnames = [_][]const u8{
                    "v0",  "v1",  "v2",  "v3",  "v4",  "v5",  "v6",  "v7",
                    "v8",  "v9",  "v10", "v11", "v12", "v13", "v14", "v15",
                    "v16", "v17", "v18", "v19", "v20", "v21", "v22", "v23",
                    "v24", "v25", "v26", "v27", "v28", "v29", "v30", "v31",
                };
                break :blk vnames[@intCast(idx)];
            }
            break :blk "v??";
        } else "???",
    };
}

fn fpRegName(reg_id: i32, cls: u8) []const u8 {
    _ = cls;
    // For FP registers stored as NEON IDs
    if (reg_id <= JIT_VREG_BASE) {
        return regName(reg_id);
    }
    return regName(reg_id);
}

fn condName(cond: u8) []const u8 {
    const names = [_][]const u8{
        "eq", "ne", "cs", "cc", "mi", "pl", "vs", "vc",
        "hi", "ls", "ge", "lt", "gt", "le", "al", "nv",
    };
    if (cond < 16) return names[cond];
    return "??";
}

fn dumpRRR(writer: anytype, mnemonic: []const u8, inst: *const JitInst) !void {
    const cls_prefix: u8 = if (inst.is64()) 'X' else 'W';
    try writer.print("{s:<12}{c} {s}, {s}, {s}\n", .{ mnemonic, cls_prefix, regName(inst.rd), regName(inst.rn), regName(inst.rm) });
}

fn dumpRRRR(writer: anytype, mnemonic: []const u8, inst: *const JitInst) !void {
    try writer.print("{s:<12}{s}, {s}, {s}, {s}\n", .{ mnemonic, regName(inst.rd), regName(inst.rn), regName(inst.rm), regName(inst.ra) });
}

fn dumpRR(writer: anytype, mnemonic: []const u8, inst: *const JitInst) !void {
    try writer.print("{s:<12}{s}, {s}\n", .{ mnemonic, regName(inst.rd), regName(inst.rn) });
}

fn dumpRRI(writer: anytype, mnemonic: []const u8, inst: *const JitInst) !void {
    try writer.print("{s:<12}{s}, {s}, #{d}\n", .{ mnemonic, regName(inst.rd), regName(inst.rn), inst.imm });
}

fn dumpMoveWide(writer: anytype, mnemonic: []const u8, inst: *const JitInst) !void {
    try writer.print("{s:<12}{s}, #0x{x}, LSL #{d}\n", .{ mnemonic, regName(inst.rd), @as(u64, @bitCast(inst.imm)), inst.imm2 });
}

fn dumpFpRRR(writer: anytype, mnemonic: []const u8, inst: *const JitInst) !void {
    const prefix: u8 = if (inst.isDouble()) 'D' else 'S';
    try writer.print("{s:<12}{c} {s}, {s}, {s}\n", .{ mnemonic, prefix, regName(inst.rd), regName(inst.rn), regName(inst.rm) });
}

fn dumpFpRR(writer: anytype, mnemonic: []const u8, inst: *const JitInst) !void {
    const prefix: u8 = if (inst.isDouble()) 'D' else 'S';
    try writer.print("{s:<12}{c} {s}, {s}\n", .{ mnemonic, prefix, regName(inst.rd), regName(inst.rn) });
}

fn dumpMemRI(writer: anytype, mnemonic: []const u8, inst: *const JitInst) !void {
    try writer.print("{s:<12}{s}, [{s}, #{d}]\n", .{ mnemonic, regName(inst.rd), regName(inst.rn), inst.imm });
}

// ============================================================================
// Section: Pipeline Report — Phase-by-Phase JIT Report
//
// Produces a human/LLM-readable report of the entire JIT compilation
// pipeline, organized by phase:
//   Phase 1: Collection — JitInst[] records from QBE
//   Phase 2: Code Generation — ARM64 instructions emitted
//   Phase 3: Data Generation — static data sections
//   Phase 4: Linking — branch fixups and label resolution
//   Phase 5: External Calls — unresolved symbols needing trampolines
//   Phase 6: Diagnostics & Summary
// ============================================================================

/// Generate a full pipeline report from the JitModule and the original
/// JitInst[] stream.  This is the primary reporting interface for the
/// production JIT compiler.
///
/// Parameters:
///   - mod:     The encoded JitModule (after jitEncode + resolveFixups).
///   - insts:   The original JitInst[] array from the collector.
///   - ninst:   Number of JitInst records.
///   - inst_count / func_count / data_count: collector-level stats.
///   - writer:  Any std.io writer (stderr, file, buffer, …).
pub fn dumpPipelineReport(
    mod: *const JitModule,
    insts: ?[*]const JitInst,
    ninst: u32,
    func_count: u32,
    data_count: u32,
    writer: anytype,
) !void {
    // ── Header ─────────────────────────────────────────────────────
    try writer.print(
        "\n" ++
            "╔══════════════════════════════════════════════════════════════╗\n" ++
            "║              JIT Pipeline Report                           ║\n" ++
            "╚══════════════════════════════════════════════════════════════╝\n\n",
        .{},
    );

    // ── Phase 1: Collection ────────────────────────────────────────
    try writer.print(
        "── Phase 1: Collection ─────────────────────────────────────────\n" ++
            "   QBE pipeline (parse → SSA → regalloc → isel) → JitInst[]\n\n" ++
            "   Total records collected:  {d}\n" ++
            "   Functions:                {d}\n" ++
            "   Data definitions:         {d}\n\n",
        .{ ninst, func_count, data_count },
    );

    // Dump the instruction stream if we have it
    if (insts) |inst_ptr| {
        try writer.print("   Instruction stream:\n", .{});
        var i: u32 = 0;
        while (i < ninst) : (i += 1) {
            const inst = &inst_ptr[i];
            try writer.print("   ", .{});
            try dumpSingleInstruction(inst, i, writer);
        }
        try writer.print("\n", .{});
    } else {
        try writer.print("   (instruction stream not available — already freed)\n\n", .{});
    }

    // ── Phase 2: Code Generation ───────────────────────────────────
    try writer.print(
        "── Phase 2: Code Generation ────────────────────────────────────\n" ++
            "   ARM64 encoder: JitInst[] → machine code buffer\n\n" ++
            "   Code size:         {d} bytes  ({d} ARM64 instructions)\n" ++
            "   Functions encoded: {d}\n" ++
            "   NEON ops encoded:  {d}\n" ++
            "   Labels recorded:   {d}\n" ++
            "   Pseudo skipped:    {d}\n\n",
        .{
            mod.code_len,
            mod.stats.instructions_emitted,
            mod.stats.functions_encoded,
            mod.stats.neon_ops_encoded,
            mod.stats.labels_recorded,
            mod.stats.skipped_pseudo,
        },
    );

    // Function symbols with offsets
    {
        var sym_iter = mod.symbols.iterator();
        var any_func = false;
        while (sym_iter.next()) |entry| {
            if (entry.value_ptr.is_code) {
                if (!any_func) {
                    try writer.print("   Function entries:\n", .{});
                    any_func = true;
                }
                try writer.print("     0x{x:0>4}  {s}\n", .{ entry.value_ptr.offset, entry.key_ptr.* });
            }
        }
        if (any_func) try writer.print("\n", .{});
    }

    // Label table
    if (mod.stats.labels_recorded > 0) {
        try writer.print("   Labels:\n", .{});
        var label_iter = mod.labels.iterator();
        while (label_iter.next()) |entry| {
            try writer.print("     .L{d}  →  0x{x:0>4}\n", .{ entry.key_ptr.*, entry.value_ptr.* });
        }
        try writer.print("\n", .{});
    }

    // Code disassembly with labels
    if (mod.code_len > 0) {
        try writer.print("   Code:\n", .{});

        // Build reverse label map
        var label_at = std.AutoHashMap(u32, i32).init(mod.allocator);
        defer label_at.deinit();
        {
            var li = mod.labels.iterator();
            while (li.next()) |entry| {
                label_at.put(entry.value_ptr.*, entry.key_ptr.*) catch {};
            }
        }

        // Build reverse ext_call map
        var ext_at = std.AutoHashMap(u32, []const u8).init(mod.allocator);
        defer ext_at.deinit();
        for (mod.ext_calls.items) |ext| {
            ext_at.put(ext.code_offset, ext.getName()) catch {};
        }

        // Try Capstone disassembly first, fall back to raw hex
        var disasm_opt = capstone.Disassembler.init() catch null;
        defer if (disasm_opt) |*d| d.deinit();

        var offset: u32 = 0;
        while (offset < mod.code_len) {
            if (label_at.get(offset)) |block_id| {
                try writer.print("   .L{d}:\n", .{block_id});
            }
            const word = mod.readWord(offset);

            if (disasm_opt) |*disasm| {
                // Capstone-powered disassembly with mnemonics
                const code_ptr = mod.code[offset..];
                const remaining = mod.code_len - offset;
                const inst_opt = disasm.disassembleOne(
                    code_ptr.ptr,
                    remaining,
                    @as(u64, offset),
                );
                if (inst_opt) |inst| {
                    const mnem = inst.getMnemonic();
                    const ops = inst.getOperands();
                    try writer.print("     0x{x:0>4}:  {x:0>8}    {s:<8}", .{
                        offset, word, mnem,
                    });
                    if (ops.len > 0) {
                        try writer.print(" {s}", .{ops});
                    }
                } else {
                    try writer.print("     0x{x:0>4}:  {x:0>8}    .word    0x{x:0>8}", .{
                        offset, word, word,
                    });
                }
            } else {
                // Fallback: raw hex only
                try writer.print("     0x{x:0>4}:  {x:0>8}", .{ offset, word });
            }

            // Annotate external calls
            if (ext_at.get(offset)) |sym| {
                try writer.print("    ; CALL_EXT {s}", .{sym});
            }
            try writer.print("\n", .{});
            offset += 4;
        }
        try writer.print("\n", .{});
    }

    // ── Phase 3: Data Generation ───────────────────────────────────
    try writer.print(
        "── Phase 3: Data Generation ────────────────────────────────────\n" ++
            "   Static data (string literals, constants, …)\n\n" ++
            "   Data size:  {d} bytes\n\n",
        .{mod.data_len},
    );

    if (mod.data_len > 0) {
        // Hex + ASCII dump
        try writer.print("   Data:\n", .{});
        var doff: u32 = 0;
        while (doff < mod.data_len) {
            try writer.print("     0x{x:0>4}:", .{doff});
            // Hex bytes (up to 16 per line)
            const line_end = @min(doff + 16, mod.data_len);
            var j: u32 = doff;
            while (j < line_end) : (j += 1) {
                try writer.print(" {x:0>2}", .{mod.data[j]});
            }
            // Pad if short line
            var pad: u32 = j;
            while (pad < doff + 16) : (pad += 1) {
                try writer.print("   ", .{});
            }
            // ASCII column
            try writer.print("  |", .{});
            j = doff;
            while (j < line_end) : (j += 1) {
                const b = mod.data[j];
                if (b >= 0x20 and b < 0x7f) {
                    try writer.print("{c}", .{b});
                } else {
                    try writer.print(".", .{});
                }
            }
            try writer.print("|\n", .{});
            doff = line_end;
        }
        try writer.print("\n", .{});

        // Data symbols
        {
            var sym_iter = mod.symbols.iterator();
            var any_data = false;
            while (sym_iter.next()) |entry| {
                if (!entry.value_ptr.is_code) {
                    if (!any_data) {
                        try writer.print("   Data symbols:\n", .{});
                        any_data = true;
                    }
                    try writer.print("     0x{x:0>4}  {s}\n", .{ entry.value_ptr.offset, entry.key_ptr.* });
                }
            }
            if (any_data) try writer.print("\n", .{});
        }

        // Typed data dump — decode FP constants and other known data at each symbol
        {
            var sym_iter2 = mod.symbols.iterator();
            var any_typed = false;
            while (sym_iter2.next()) |entry| {
                if (entry.value_ptr.is_code) continue;
                const name = entry.key_ptr.*;
                const off = entry.value_ptr.offset;

                if (!any_typed) {
                    try writer.print("   Typed data dump:\n", .{});
                    any_typed = true;
                }

                // Detect Lfp (FP literal pool) symbols — names contain "Lfp"
                const is_fp = std.mem.indexOf(u8, name, "Lfp") != null;

                if (is_fp) {
                    // Try to decode as double (8 bytes) first, then single (4 bytes)
                    if (off + 8 <= mod.data_len) {
                        // Read 8 bytes LE as u64
                        var qval: u64 = 0;
                        var qi: u32 = 0;
                        while (qi < 8) : (qi += 1) {
                            qval |= @as(u64, mod.data[off + qi]) << @intCast(qi * 8);
                        }
                        const dbl: f64 = @bitCast(qval);

                        // Also read first 4 bytes as single
                        var wval: u32 = 0;
                        var wi: u32 = 0;
                        while (wi < 4) : (wi += 1) {
                            wval |= @as(u32, mod.data[off + wi]) << @intCast(wi * 8);
                        }
                        const sng: f32 = @bitCast(wval);

                        // Check if upper 4 bytes are zero → likely a 4-byte single
                        const upper4_zero = (qval >> 32) == 0;

                        try writer.print("     0x{x:0>4}  {s}\n", .{ off, name });
                        try writer.print("             raw bytes:", .{});
                        var bi: u32 = 0;
                        const blen: u32 = if (upper4_zero) 4 else 8;
                        while (bi < blen) : (bi += 1) {
                            try writer.print(" {x:0>2}", .{mod.data[off + bi]});
                        }
                        try writer.print("\n", .{});

                        if (upper4_zero) {
                            try writer.print("             as u32:   0x{x:0>8}\n", .{wval});
                            try writer.print("             as f32:   {d}\n", .{sng});
                            // Also show what this would be as f64 if someone mistakenly loaded 8 bytes
                            try writer.print("             as f64 (if 8B read): {d}  [0x{x:0>16}]\n", .{ dbl, qval });
                        } else {
                            try writer.print("             as u64:   0x{x:0>16}\n", .{qval});
                            try writer.print("             as f64:   {d}\n", .{dbl});
                            // Also show the lower 4 bytes as f32 in case of size mismatch
                            try writer.print("             as f32 (lower 4B): {d}  [0x{x:0>8}]\n", .{ sng, wval });
                        }
                    } else if (off + 4 <= mod.data_len) {
                        // Only 4 bytes available — must be single
                        var wval: u32 = 0;
                        var wi: u32 = 0;
                        while (wi < 4) : (wi += 1) {
                            wval |= @as(u32, mod.data[off + wi]) << @intCast(wi * 8);
                        }
                        const sng: f32 = @bitCast(wval);
                        try writer.print("     0x{x:0>4}  {s}\n", .{ off, name });
                        try writer.print("             raw bytes: {x:0>2} {x:0>2} {x:0>2} {x:0>2}\n", .{
                            mod.data[off], mod.data[off + 1], mod.data[off + 2], mod.data[off + 3],
                        });
                        try writer.print("             as u32:   0x{x:0>8}\n", .{wval});
                        try writer.print("             as f32:   {d}\n", .{sng});
                    } else {
                        try writer.print("     0x{x:0>4}  {s}  (truncated, only {d} bytes remain)\n", .{ off, name, mod.data_len - off });
                    }
                } else {
                    // Non-FP symbol: show first few bytes as hex + ASCII
                    try writer.print("     0x{x:0>4}  {s}", .{ off, name });
                    const remain = mod.data_len - off;
                    if (remain > 0) {
                        const show = @min(remain, 32);
                        try writer.print("  [", .{});
                        var si: u32 = 0;
                        while (si < show) : (si += 1) {
                            const b = mod.data[off + si];
                            if (b >= 0x20 and b < 0x7f) {
                                try writer.print("{c}", .{b});
                            } else if (b == 0) {
                                break; // stop at NUL
                            } else {
                                try writer.print("\\x{x:0>2}", .{b});
                            }
                        }
                        try writer.print("]\n", .{});
                    } else {
                        try writer.print("\n", .{});
                    }
                }
            }
            if (any_typed) try writer.print("\n", .{});
        }
    }

    // ── Phase 4: Linking ───────────────────────────────────────────
    try writer.print(
        "── Phase 4: Linking ────────────────────────────────────────────\n" ++
            "   Branch fixups: resolve forward jumps to label offsets\n\n" ++
            "   Fixups created:   {d}\n" ++
            "   Fixups resolved:  {d}\n" ++
            "   Unresolved:       {d}\n\n",
        .{
            mod.stats.fixups_created,
            mod.stats.fixups_resolved,
            mod.stats.fixups_created -| mod.stats.fixups_resolved,
        },
    );

    if (mod.fixups.items.len > 0) {
        try writer.print("   Fixup details:\n", .{});
        for (mod.fixups.items, 0..) |fixup, i| {
            const resolved = mod.labels.get(fixup.target_id) != null;
            const status: []const u8 = if (resolved) "OK" else "UNRESOLVED";
            const class_name: []const u8 = switch (fixup.branch_class) {
                .Imm26 => "B/BL(imm26)",
                .Imm19 => "B.cc/CBZ(imm19)",
                .Imm14 => "TBZ(imm14)",
                .Invalid => "INVALID",
            };
            try writer.print("     [{d}] @0x{x:0>4} → .L{d}  {s}  [{s}]\n", .{
                i,
                fixup.code_offset,
                fixup.target_id,
                class_name,
                status,
            });
        }
        try writer.print("\n", .{});
    }

    // ── Phase 5: External Calls ────────────────────────────────────
    try writer.print(
        "── Phase 5: External Calls ─────────────────────────────────────\n" ++
            "   Symbols requiring runtime trampolines / dynamic linking\n\n" ++
            "   External call sites:  {d}\n\n",
        .{mod.stats.ext_calls_recorded},
    );

    if (mod.ext_calls.items.len > 0) {
        try writer.print("   Call sites:\n", .{});
        for (mod.ext_calls.items, 0..) |ext, i| {
            try writer.print("     [{d}] @0x{x:0>4}  → {s}\n", .{
                i,
                ext.code_offset,
                ext.getName(),
            });
        }
        try writer.print("\n", .{});

        // Deduplicated symbol list
        try writer.print("   Unique external symbols:\n", .{});
        // Simple O(n²) dedup — ext_calls is small
        var seen_count: u32 = 0;
        for (mod.ext_calls.items, 0..) |ext, i| {
            const name = ext.getName();
            var dup = false;
            for (mod.ext_calls.items[0..i]) |prev| {
                if (std.mem.eql(u8, prev.getName(), name)) {
                    dup = true;
                    break;
                }
            }
            if (!dup) {
                // Count call sites for this symbol
                var count: u32 = 0;
                for (mod.ext_calls.items) |e| {
                    if (std.mem.eql(u8, e.getName(), name)) count += 1;
                }
                try writer.print("     {s}  ({d} call site{s})\n", .{
                    name,
                    count,
                    if (count != 1) "s" else "",
                });
                seen_count += 1;
            }
        }
        try writer.print("   Total unique symbols: {d}\n\n", .{seen_count});
    }

    // ── Phase 6: Diagnostics & Summary ─────────────────────────────
    try writer.print(
        "── Phase 6: Diagnostics & Summary ──────────────────────────────\n\n",
        .{},
    );

    if (mod.diagnostics.items.len > 0) {
        var n_err: u32 = 0;
        var n_warn: u32 = 0;
        var n_info: u32 = 0;
        for (mod.diagnostics.items) |d| {
            switch (d.severity) {
                .Error => n_err += 1,
                .Warning => n_warn += 1,
                .Info => n_info += 1,
            }
        }
        try writer.print("   Diagnostics: {d} error(s), {d} warning(s), {d} info(s)\n\n", .{ n_err, n_warn, n_info });
        for (mod.diagnostics.items, 0..) |d, i| {
            const sev: []const u8 = switch (d.severity) {
                .Info => "INFO",
                .Warning => "WARN",
                .Error => "ERR ",
            };
            try writer.print("     [{d}] {s}  inst[{d}] @0x{x:0>4}: {s}\n", .{
                i, sev, d.inst_index, d.code_offset, d.getMessage(),
            });
        }
        try writer.print("\n", .{});
    } else {
        try writer.print("   Diagnostics: none\n\n", .{});
    }

    // Source map
    if (mod.source_map.items.len > 0) {
        try writer.print("   Source map ({d} entries):\n", .{mod.source_map.items.len});
        for (mod.source_map.items) |sm| {
            try writer.print("     0x{x:0>4} → line {d}", .{ sm.code_offset, sm.source_line });
            if (sm.source_col > 0) {
                try writer.print(" col {d}", .{sm.source_col});
            }
            try writer.print("\n", .{});
        }
        try writer.print("\n", .{});
    }

    // Final summary
    try writer.print(
        "   ┌─────────────────────────────────────────┐\n" ++
            "   │ Code:  {d:>6} bytes  ({d:>4} instructions) │\n" ++
            "   │ Data:  {d:>6} bytes                      │\n" ++
            "   │ Funcs: {d:>6}                             │\n" ++
            "   │ Ext:   {d:>6} call sites                  │\n" ++
            "   │ Errs:  {d:>6}                             │\n" ++
            "   └─────────────────────────────────────────┘\n\n",
        .{
            mod.code_len,
            mod.stats.instructions_emitted,
            mod.data_len,
            mod.stats.functions_encoded,
            mod.stats.ext_calls_recorded,
            mod.stats.error_count,
        },
    );
}

// ============================================================================
// Section: Module Dump — Print Encoding Results
// ============================================================================

/// Print a comprehensive summary of encoding results.
pub fn dumpModuleSummary(mod: *const JitModule, writer: anytype) !void {
    try writer.print(
        "\n" ++
            "================================================================\n" ++
            "  JIT Encode Summary\n" ++
            "================================================================\n" ++
            "\n" ++
            "  Code:            {d} bytes ({d} instructions)\n" ++
            "  Data:            {d} bytes\n" ++
            "  Labels:          {d}\n" ++
            "  Fixups:          {d} created, {d} resolved\n" ++
            "  External calls:  {d}\n" ++
            "  Source map:       {d} entries\n" ++
            "  Comments:        {d}\n" ++
            "  Functions:       {d}\n" ++
            "  NEON ops:        {d}\n" ++
            "  Pseudo skipped:  {d}\n" ++
            "  Errors:          {d}\n" ++
            "\n",
        .{
            mod.code_len,
            mod.stats.instructions_emitted,
            mod.stats.data_bytes_emitted,
            mod.stats.labels_recorded,
            mod.stats.fixups_created,
            mod.stats.fixups_resolved,
            mod.stats.ext_calls_recorded,
            mod.stats.source_map_entries,
            mod.stats.comments_recorded,
            mod.stats.functions_encoded,
            mod.stats.neon_ops_encoded,
            mod.stats.skipped_pseudo,
            mod.stats.error_count,
        },
    );

    // Print diagnostics
    if (mod.diagnostics.items.len > 0) {
        try writer.print("  Diagnostics:\n", .{});
        for (mod.diagnostics.items) |d| {
            const sev: []const u8 = switch (d.severity) {
                .Info => "INFO",
                .Warning => "WARN",
                .Error => "ERR ",
            };
            try writer.print("    [{s}] inst[{d}] @0x{x:0>4}: {s}\n", .{
                sev, d.inst_index, d.code_offset, d.getMessage(),
            });
        }
    }

    try writer.print(
        "================================================================\n\n",
        .{},
    );
}

/// Hex dump of the generated code buffer.
pub fn dumpCodeHex(mod: *const JitModule, writer: anytype) !void {
    try writer.print("\n=== Code Hex Dump ({d} bytes) ===\n", .{mod.code_len});

    var offset: u32 = 0;
    while (offset < mod.code_len) : (offset += 4) {
        if (offset % 32 == 0) {
            if (offset > 0) try writer.print("\n", .{});
            try writer.print("  {x:0>4}:", .{offset});
        }
        const word = mod.readWord(offset);
        try writer.print(" {x:0>8}", .{word});
    }
    try writer.print("\n\n", .{});
}

/// Disassembly-style dump: for each instruction word, show offset and opcode.
pub fn dumpCodeDisasm(mod: *const JitModule, writer: anytype) !void {
    try writer.print("\n=== Code Disassembly ({d} instructions) ===\n\n", .{mod.stats.instructions_emitted});

    // Build reverse label map for annotation
    var label_at = std.AutoHashMap(u32, i32).init(mod.allocator);
    defer label_at.deinit();

    var label_iter = mod.labels.iterator();
    while (label_iter.next()) |entry| {
        label_at.put(entry.value_ptr.*, entry.key_ptr.*) catch {};
    }

    // Build reverse ext_call map for annotation
    var ext_at = std.AutoHashMap(u32, []const u8).init(mod.allocator);
    defer ext_at.deinit();
    for (mod.ext_calls.items) |ext| {
        ext_at.put(ext.code_offset, ext.getName()) catch {};
    }

    // Try Capstone disassembly, fall back to raw hex
    var disasm_opt = capstone.Disassembler.init() catch null;
    defer if (disasm_opt) |*d| d.deinit();

    var offset: u32 = 0;
    while (offset < mod.code_len) {
        // Check if there's a label at this offset
        if (label_at.get(offset)) |block_id| {
            try writer.print(".L{d}:\n", .{block_id});
        }
        const word = mod.readWord(offset);

        if (disasm_opt) |*disasm| {
            const code_ptr = mod.code[offset..];
            const remaining = mod.code_len - offset;
            const inst_opt = disasm.disassembleOne(
                code_ptr.ptr,
                remaining,
                @as(u64, offset),
            );
            if (inst_opt) |inst| {
                const mnem = inst.getMnemonic();
                const ops = inst.getOperands();
                try writer.print("  {x:0>4}:  {x:0>8}    {s:<8}", .{
                    offset, word, mnem,
                });
                if (ops.len > 0) {
                    try writer.print(" {s}", .{ops});
                }
            } else {
                try writer.print("  {x:0>4}:  {x:0>8}    .word    0x{x:0>8}", .{
                    offset, word, word,
                });
            }
        } else {
            try writer.print("  {x:0>4}:  {x:0>8}", .{ offset, word });
        }

        if (ext_at.get(offset)) |sym| {
            try writer.print("    ; → {s}", .{sym});
        }
        try writer.print("\n", .{});
        offset += 4;
    }
    try writer.print("\n", .{});
}

// ============================================================================
// Section: Tests
// ============================================================================

test "JitInst layout matches C struct" {
    // Verify key field offsets for C ABI compatibility
    try std.testing.expectEqual(@as(usize, 0), @offsetOf(JitInst, "kind"));
    try std.testing.expectEqual(@as(usize, 2), @offsetOf(JitInst, "cls"));
    try std.testing.expectEqual(@as(usize, 3), @offsetOf(JitInst, "cond"));
    try std.testing.expectEqual(@as(usize, 4), @offsetOf(JitInst, "shift_type"));
    try std.testing.expectEqual(@as(usize, 5), @offsetOf(JitInst, "sym_type"));
    try std.testing.expectEqual(@as(usize, 6), @offsetOf(JitInst, "is_float"));
    try std.testing.expectEqual(@as(usize, 8), @offsetOf(JitInst, "rd"));
    try std.testing.expectEqual(@as(usize, 12), @offsetOf(JitInst, "rn"));
    try std.testing.expectEqual(@as(usize, 16), @offsetOf(JitInst, "rm"));
    try std.testing.expectEqual(@as(usize, 20), @offsetOf(JitInst, "ra"));
    try std.testing.expectEqual(@as(usize, 24), @offsetOf(JitInst, "imm"));
    try std.testing.expectEqual(@as(usize, 32), @offsetOf(JitInst, "imm2"));
    try std.testing.expectEqual(@as(usize, 40), @offsetOf(JitInst, "target_id"));
}

test "register mapping: GP registers" {
    try std.testing.expectEqual(Register.R0, try mapGPRegister(0));
    try std.testing.expectEqual(Register.R1, try mapGPRegister(1));
    try std.testing.expectEqual(Register.R15, try mapGPRegister(15));
    try std.testing.expectEqual(Register.R28, try mapGPRegister(28));
    try std.testing.expectEqual(Register.FP, try mapGPRegister(29));
    try std.testing.expectEqual(Register.LR, try mapGPRegister(30));
}

test "register mapping: special registers" {
    try std.testing.expectEqual(Register.SP, try mapGPRegister(JIT_REG_SP));
    try std.testing.expectEqual(Register.FP, try mapGPRegister(JIT_REG_FP));
    try std.testing.expectEqual(Register.LR, try mapGPRegister(JIT_REG_LR));
    try std.testing.expectEqual(Register.R16, try mapGPRegister(JIT_REG_IP0));
    try std.testing.expectEqual(Register.R17, try mapGPRegister(JIT_REG_IP1));
}

test "register mapping: NEON registers" {
    try std.testing.expectEqual(NeonRegister.V0, try mapNeonRegister(-100));
    try std.testing.expectEqual(NeonRegister.V1, try mapNeonRegister(-101));
    try std.testing.expectEqual(NeonRegister.V28, try mapNeonRegister(-128));
    try std.testing.expectEqual(NeonRegister.V31, try mapNeonRegister(-131));
}

test "register mapping: invalid register" {
    try std.testing.expectError(EncodeError.InvalidRegister, mapGPRegister(31));
    try std.testing.expectError(EncodeError.InvalidRegister, mapGPRegister(-10));
    try std.testing.expectError(EncodeError.InvalidRegister, mapNeonRegister(0));
    try std.testing.expectError(EncodeError.InvalidRegister, mapNeonRegister(-50));
}

test "condition mapping" {
    try std.testing.expectEqual(Condition.EQ, mapCondition(0x0));
    try std.testing.expectEqual(Condition.NE, mapCondition(0x1));
    try std.testing.expectEqual(Condition.GT, mapCondition(0xC));
    try std.testing.expectEqual(Condition.LE, mapCondition(0xD));
}

test "isNeonReg" {
    try std.testing.expect(isNeonReg(-100));
    try std.testing.expect(isNeonReg(-131));
    try std.testing.expect(!isNeonReg(0));
    try std.testing.expect(!isNeonReg(-1));
    try std.testing.expect(!isNeonReg(-99));
}

test "JitModule emit and read word" {
    var mod = try JitModule.init(std.testing.allocator, 1024, 256);
    defer mod.deinit();

    try mod.emitWord(0xDEADBEEF);
    try std.testing.expectEqual(@as(u32, 4), mod.code_len);
    try std.testing.expectEqual(@as(u32, 0xDEADBEEF), mod.readWord(0));

    try mod.emitWord(0x12345678);
    try std.testing.expectEqual(@as(u32, 8), mod.code_len);
    try std.testing.expectEqual(@as(u32, 0x12345678), mod.readWord(4));
}

test "JitModule patch word" {
    var mod = try JitModule.init(std.testing.allocator, 1024, 256);
    defer mod.deinit();

    try mod.emitWord(0x00000000);
    mod.patchWord(0, 0xCAFEBABE);
    try std.testing.expectEqual(@as(u32, 0xCAFEBABE), mod.readWord(0));
}

test "JitModule data emit" {
    var mod = try JitModule.init(std.testing.allocator, 256, 256);
    defer mod.deinit();

    try mod.emitDataByte(0x42);
    try std.testing.expectEqual(@as(u32, 1), mod.data_len);
    try std.testing.expectEqual(@as(u8, 0x42), mod.data[0]);

    try mod.emitDataWord(0xDEADBEEF);
    try std.testing.expectEqual(@as(u32, 5), mod.data_len);
}

test "encode NOP" {
    var mod = try JitModule.init(std.testing.allocator, 1024, 256);
    defer mod.deinit();

    var inst = std.mem.zeroes(JitInst);
    inst.kind = @intFromEnum(JitInstKind.JIT_NOP);

    encodeInstruction(&mod, &inst, 0);

    try std.testing.expectEqual(@as(u32, 4), mod.code_len);
    try std.testing.expectEqual(enc.emitNop(), mod.readWord(0));
}

test "encode ADD W0, W1, W2" {
    var mod = try JitModule.init(std.testing.allocator, 1024, 256);
    defer mod.deinit();

    var inst = std.mem.zeroes(JitInst);
    inst.kind = @intFromEnum(JitInstKind.JIT_ADD_RRR);
    inst.cls = @intFromEnum(JitCls.W);
    inst.rd = 0; // R0
    inst.rn = 1; // R1
    inst.rm = 2; // R2

    encodeInstruction(&mod, &inst, 0);

    try std.testing.expectEqual(@as(u32, 4), mod.code_len);
    // Verify against the known encoding
    const expected = enc.emitAddRegister(Register.R0, Register.R1, RegisterParam.reg_only(Register.R2));
    try std.testing.expectEqual(expected, mod.readWord(0));
}

test "encode ADD X0, X1, X2 (64-bit)" {
    var mod = try JitModule.init(std.testing.allocator, 1024, 256);
    defer mod.deinit();

    var inst = std.mem.zeroes(JitInst);
    inst.kind = @intFromEnum(JitInstKind.JIT_ADD_RRR);
    inst.cls = @intFromEnum(JitCls.L);
    inst.rd = 0;
    inst.rn = 1;
    inst.rm = 2;

    encodeInstruction(&mod, &inst, 0);

    const expected = enc.emitAddRegister64(Register.R0, Register.R1, RegisterParam.reg_only(Register.R2));
    try std.testing.expectEqual(expected, mod.readWord(0));
}

test "encode label and backward branch" {
    var mod = try JitModule.init(std.testing.allocator, 1024, 256);
    defer mod.deinit();

    // Emit a label at offset 0
    var label_inst = std.mem.zeroes(JitInst);
    label_inst.kind = @intFromEnum(JitInstKind.JIT_LABEL);
    label_inst.target_id = 42;
    encodeInstruction(&mod, &label_inst, 0);

    // Emit a NOP (4 bytes)
    var nop_inst = std.mem.zeroes(JitInst);
    nop_inst.kind = @intFromEnum(JitInstKind.JIT_NOP);
    encodeInstruction(&mod, &nop_inst, 1);

    // Emit B back to label 42 (at offset 4, targeting offset 0)
    var b_inst = std.mem.zeroes(JitInst);
    b_inst.kind = @intFromEnum(JitInstKind.JIT_B);
    b_inst.target_id = 42;
    encodeInstruction(&mod, &b_inst, 2);

    // The B instruction at offset 4 should branch backward by -4 bytes = -1 instruction word
    try std.testing.expectEqual(@as(u32, 8), mod.code_len);
    const b_word = mod.readWord(4);
    // B encoding: delta = (0 - 4) / 4 = -1 instruction words
    const expected = enc.emitB(-1);
    try std.testing.expectEqual(expected, b_word);
}

test "encode forward branch with fixup" {
    var mod = try JitModule.init(std.testing.allocator, 1024, 256);
    defer mod.deinit();

    // Emit B to label 99 (not yet defined — forward branch)
    var b_inst = std.mem.zeroes(JitInst);
    b_inst.kind = @intFromEnum(JitInstKind.JIT_B);
    b_inst.target_id = 99;
    encodeInstruction(&mod, &b_inst, 0);

    // Should have created a fixup
    try std.testing.expectEqual(@as(usize, 1), mod.fixups.items.len);
    try std.testing.expectEqual(@as(i32, 99), mod.fixups.items[0].target_id);

    // Emit a NOP
    var nop_inst = std.mem.zeroes(JitInst);
    nop_inst.kind = @intFromEnum(JitInstKind.JIT_NOP);
    encodeInstruction(&mod, &nop_inst, 1);

    // Now define label 99 at offset 8
    var label_inst = std.mem.zeroes(JitInst);
    label_inst.kind = @intFromEnum(JitInstKind.JIT_LABEL);
    label_inst.target_id = 99;
    encodeInstruction(&mod, &label_inst, 2);

    // Resolve fixups
    resolveFixups(&mod);

    try std.testing.expectEqual(@as(u32, 1), mod.stats.fixups_resolved);
    // The B instruction at offset 0 should now branch forward by +8 bytes = +2 instruction words
    const b_word = mod.readWord(0);
    const expected = enc.emitB(2);
    try std.testing.expectEqual(expected, b_word);
}

test "encode BRK" {
    var mod = try JitModule.init(std.testing.allocator, 1024, 256);
    defer mod.deinit();

    var inst = std.mem.zeroes(JitInst);
    inst.kind = @intFromEnum(JitInstKind.JIT_BRK);
    inst.imm = 1000;
    encodeInstruction(&mod, &inst, 0);

    try std.testing.expectEqual(enc.emitBrk(1000), mod.readWord(0));
}

test "encode MOVZ" {
    var mod = try JitModule.init(std.testing.allocator, 1024, 256);
    defer mod.deinit();

    var inst = std.mem.zeroes(JitInst);
    inst.kind = @intFromEnum(JitInstKind.JIT_MOVZ);
    inst.cls = @intFromEnum(JitCls.W);
    inst.rd = 0;
    inst.imm = 0x1234;
    inst.imm2 = 0; // shift = 0
    encodeInstruction(&mod, &inst, 0);

    try std.testing.expectEqual(enc.emitMovz(Register.R0, 0x1234, 0), mod.readWord(0));
}

test "encode RET" {
    var mod = try JitModule.init(std.testing.allocator, 1024, 256);
    defer mod.deinit();

    var inst = std.mem.zeroes(JitInst);
    inst.kind = @intFromEnum(JitInstKind.JIT_RET);
    encodeInstruction(&mod, &inst, 0);

    try std.testing.expectEqual(enc.emitRet(Register.LR), mod.readWord(0));
}

test "encode HINT #34 (BTI C)" {
    var mod = try JitModule.init(std.testing.allocator, 1024, 256);
    defer mod.deinit();

    var inst = std.mem.zeroes(JitInst);
    inst.kind = @intFromEnum(JitInstKind.JIT_HINT);
    inst.imm = 34;
    encodeInstruction(&mod, &inst, 0);

    try std.testing.expectEqual(enc.emitHint(34), mod.readWord(0));
}

test "encode LDR X0, [X1, #16]" {
    var mod = try JitModule.init(std.testing.allocator, 1024, 256);
    defer mod.deinit();

    var inst = std.mem.zeroes(JitInst);
    inst.kind = @intFromEnum(JitInstKind.JIT_LDR_RI);
    inst.cls = @intFromEnum(JitCls.L);
    inst.rd = 0;
    inst.rn = 1;
    inst.imm = 16;
    encodeInstruction(&mod, &inst, 0);

    const expected = enc.emitLdrOffset64(Register.R0, Register.R1, 16);
    try std.testing.expect(expected != null);
    try std.testing.expectEqual(expected.?, mod.readWord(0));
}

test "encode STP X29, X30, [SP, #-16]! (pre-index)" {
    var mod = try JitModule.init(std.testing.allocator, 1024, 256);
    defer mod.deinit();

    var inst = std.mem.zeroes(JitInst);
    inst.kind = @intFromEnum(JitInstKind.JIT_STP_PRE);
    inst.cls = @intFromEnum(JitCls.L);
    inst.rd = 29; // FP
    inst.rm = 30; // LR
    inst.rn = JIT_REG_SP;
    inst.imm = -16;
    encodeInstruction(&mod, &inst, 0);

    const expected = enc.emitStpOffsetPreIndex64(Register.FP, Register.LR, Register.SP, -16);
    try std.testing.expect(expected != null);
    try std.testing.expectEqual(expected.?, mod.readWord(0));
}

test "encode CALL_EXT records entry" {
    var mod = try JitModule.init(std.testing.allocator, 1024, 256);
    defer mod.deinit();

    var inst = std.mem.zeroes(JitInst);
    inst.kind = @intFromEnum(JitInstKind.JIT_CALL_EXT);
    @memcpy(inst.sym_name[0..6], "printf");
    encodeInstruction(&mod, &inst, 0);

    try std.testing.expectEqual(@as(usize, 1), mod.ext_calls.items.len);
    try std.testing.expectEqualStrings("printf", mod.ext_calls.items[0].getName());
    try std.testing.expectEqual(@as(u32, 4), mod.code_len); // BL placeholder emitted
}

test "data section emit" {
    var mod = try JitModule.init(std.testing.allocator, 256, 1024);
    defer mod.deinit();

    var start = std.mem.zeroes(JitInst);
    start.kind = @intFromEnum(JitInstKind.JIT_DATA_START);
    encodeInstruction(&mod, &start, 0);

    var byte_inst = std.mem.zeroes(JitInst);
    byte_inst.kind = @intFromEnum(JitInstKind.JIT_DATA_BYTE);
    byte_inst.imm = 0x42;
    encodeInstruction(&mod, &byte_inst, 1);

    var word_inst = std.mem.zeroes(JitInst);
    word_inst.kind = @intFromEnum(JitInstKind.JIT_DATA_WORD);
    word_inst.imm = 0xDEADBEEF;
    encodeInstruction(&mod, &word_inst, 2);

    var end = std.mem.zeroes(JitInst);
    end.kind = @intFromEnum(JitInstKind.JIT_DATA_END);
    encodeInstruction(&mod, &end, 3);

    try std.testing.expectEqual(@as(u32, 5), mod.data_len);
    try std.testing.expectEqual(@as(u8, 0x42), mod.data[0]);
}

test "diagnostics collection" {
    var mod = try JitModule.init(std.testing.allocator, 1024, 256);
    defer mod.deinit();

    // Try to encode with an invalid register
    var inst = std.mem.zeroes(JitInst);
    inst.kind = @intFromEnum(JitInstKind.JIT_ADD_RRR);
    inst.cls = @intFromEnum(JitCls.W);
    inst.rd = 31; // Invalid (no R31 in GP — it's SP/ZR)
    inst.rn = 0;
    inst.rm = 1;
    encodeInstruction(&mod, &inst, 0);

    try std.testing.expect(mod.hasErrors());
    try std.testing.expect(mod.diagnostics.items.len > 0);
}

test "neonArr mapping" {
    try std.testing.expectEqual(NeonSize.Size4S, mapNeonArr(0, false));
    try std.testing.expectEqual(NeonSize.Size2D, mapNeonArr(1, false));
    try std.testing.expectEqual(NeonSize.Size4S, mapNeonArr(2, true)); // 4SF
    try std.testing.expectEqual(NeonSize.Size2D, mapNeonArr(3, true)); // 2DF
    try std.testing.expectEqual(NeonSize.Size8H, mapNeonArr(4, false));
    try std.testing.expectEqual(NeonSize.Size16B, mapNeonArr(5, false));
}

test "scalar size from cls" {
    try std.testing.expectEqual(NeonSize.Size1S, scalarSizeFromCls(@intFromEnum(JitCls.S)));
    try std.testing.expectEqual(NeonSize.Size1D, scalarSizeFromCls(@intFromEnum(JitCls.D)));
    try std.testing.expectEqual(NeonSize.Size1S, scalarSizeFromCls(@intFromEnum(JitCls.W)));
}

test "encode multiple instructions sequence" {
    var mod = try JitModule.init(std.testing.allocator, 4096, 256);
    defer mod.deinit();

    // Simulate a small function: HINT #34; STP x29,x30,[sp,#-16]!; MOV x29,sp; ... ; RET
    const sequence = [_]struct { kind: JitInstKind, cls: JitCls, rd: i32, rn: i32, rm: i32, imm: i64, imm2: i64, target_id: i32 }{
        .{ .kind = .JIT_HINT, .cls = .W, .rd = 0, .rn = 0, .rm = 0, .imm = 34, .imm2 = 0, .target_id = 0 },
        .{ .kind = .JIT_STP_PRE, .cls = .L, .rd = 29, .rn = JIT_REG_SP, .rm = 30, .imm = -16, .imm2 = 0, .target_id = 0 },
        .{ .kind = .JIT_MOV_SP, .cls = .L, .rd = 29, .rn = JIT_REG_SP, .rm = 0, .imm = 0, .imm2 = 0, .target_id = 0 },
        .{ .kind = .JIT_RET, .cls = .W, .rd = 0, .rn = 0, .rm = 0, .imm = 0, .imm2 = 0, .target_id = 0 },
    };

    for (sequence, 0..) |s, i| {
        var inst = std.mem.zeroes(JitInst);
        inst.kind = @intFromEnum(s.kind);
        inst.cls = @intFromEnum(s.cls);
        inst.rd = s.rd;
        inst.rn = s.rn;
        inst.rm = s.rm;
        inst.imm = s.imm;
        inst.imm2 = s.imm2;
        inst.target_id = s.target_id;
        encodeInstruction(&mod, &inst, @intCast(i));
    }

    // Should have emitted 4 instructions (16 bytes)
    // HINT emits 1, STP_PRE emits 1, MOV_SP emits 1, RET emits 1
    try std.testing.expect(mod.code_len >= 16);
    try std.testing.expect(!mod.hasErrors());
}
