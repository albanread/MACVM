//! jit_capstone.zig — Capstone AArch64 disassembler integration for JIT pipeline
//!
//! This module provides Zig bindings around the Capstone disassembly engine,
//! specifically configured for AArch64 (ARM64) instruction decoding. It is
//! used to produce human-readable disassembly listings of the JIT code buffer
//! as part of the verbose JIT pipeline report.
//!
//! Architecture:
//!   JitModule.code[]  (raw ARM64 machine code bytes)
//!     → Capstone cs_disasm()
//!       → cs_insn[] with mnemonic + operand strings
//!         → formatted listing with labels, symbols, annotations
//!
//! The disassembler can operate on:
//!   1. Raw byte buffers (for standalone use)
//!   2. JitModule code buffers (with label/symbol/ext-call annotations)
//!   3. JitMemoryRegion live code (with runtime addresses)
//!
//! This module is optional — the JIT pipeline works without it, falling back
//! to raw hex dumps. When available, it enriches the verbose report with
//! proper instruction mnemonics.
//!
//! References:
//!   - Capstone Engine: https://www.capstone-engine.org
//!   - design/jit_design.md §Diagnostics & Verbose Report

const std = @import("std");
const jit = @import("jit_encode.zig");
const linker = @import("jit_linker.zig");

const JitModule = jit.JitModule;
const SourceMapEntry = jit.SourceMapEntry;
const LinkResult = linker.LinkResult;
const TrampolineStub = linker.TrampolineStub;

// ============================================================================
// Section: Capstone C API Bindings (via @cImport)
// ============================================================================

const cs = @cImport({
    @cInclude("capstone/capstone.h");
});

const CS_ARCH_ARM64: c_int = cs.CS_ARCH_AARCH64;
const CS_MODE_LITTLE_ENDIAN: c_uint = cs.CS_MODE_LITTLE_ENDIAN;
const CS_OPT_DETAIL: c_int = cs.CS_OPT_DETAIL;
const CS_OPT_ON: usize = cs.CS_OPT_ON;
const CS_OPT_OFF: usize = cs.CS_OPT_OFF;
const CS_ERR_OK: c_int = cs.CS_ERR_OK;
const CS_MNEMONIC_SIZE = cs.CS_MNEMONIC_SIZE;

const csh = cs.csh;
const cs_insn = cs.cs_insn;

// ============================================================================
// Section: Public Types
// ============================================================================

/// Error type for Capstone operations.
pub const CapstoneError = error{
    /// Failed to initialize Capstone engine
    InitFailed,
    /// Disassembly failed (invalid input or engine error)
    DisasmFailed,
    /// Engine not initialized
    NotInitialized,
    /// Out of memory
    OutOfMemory,
};

/// A single disassembled instruction with its metadata.
pub const DisasmInst = struct {
    /// Byte offset from the start of the code buffer
    offset: u32,
    /// Runtime address (if known, otherwise same as offset)
    address: u64,
    /// Instruction size in bytes (always 4 for AArch64)
    size: u16,
    /// Raw instruction word (little-endian)
    raw_word: u32,
    /// Instruction mnemonic (e.g., "mov", "ldr", "bl")
    mnemonic: [CS_MNEMONIC_SIZE]u8,
    /// Operands string (e.g., "x0, x1", "[sp, #16]")
    op_str: [160]u8,

    /// Get the mnemonic as a Zig slice.
    pub fn getMnemonic(self: *const DisasmInst) []const u8 {
        const slice: []const u8 = &self.mnemonic;
        return std.mem.sliceTo(@as([*:0]const u8, @ptrCast(slice.ptr)), 0);
    }

    /// Get the operands as a Zig slice.
    pub fn getOperands(self: *const DisasmInst) []const u8 {
        const slice: []const u8 = &self.op_str;
        return std.mem.sliceTo(@as([*:0]const u8, @ptrCast(slice.ptr)), 0);
    }

    /// Format as a single disassembly line (no newline).
    pub fn format(self: *const DisasmInst, writer: anytype) !void {
        const mnem = self.getMnemonic();
        const ops = self.getOperands();
        if (ops.len > 0) {
            try std.fmt.format(writer, "0x{x:0>4}:  {x:0>8}    {s:<8} {s}", .{
                self.offset, self.raw_word, mnem, ops,
            });
        } else {
            try std.fmt.format(writer, "0x{x:0>4}:  {x:0>8}    {s}", .{
                self.offset, self.raw_word, mnem,
            });
        }
    }
};

// ============================================================================
// Section: Disassembler Handle
// ============================================================================

/// Capstone AArch64 disassembler wrapper.
///
/// Manages a Capstone engine handle and provides methods for disassembling
/// ARM64 machine code buffers. Designed to be reused across multiple
/// disassembly operations within a JIT session.
pub const Disassembler = struct {
    handle: csh,
    initialized: bool,

    /// Initialize the Capstone AArch64 disassembler.
    pub fn init() CapstoneError!Disassembler {
        var handle: csh = 0;
        const err = cs.cs_open(CS_ARCH_ARM64, CS_MODE_LITTLE_ENDIAN, &handle);
        if (err != CS_ERR_OK) {
            return CapstoneError.InitFailed;
        }

        return Disassembler{
            .handle = handle,
            .initialized = true,
        };
    }

    /// Close the Capstone engine and release resources.
    pub fn deinit(self: *Disassembler) void {
        if (self.initialized) {
            _ = cs.cs_close(&self.handle);
            self.initialized = false;
        }
    }

    /// Disassemble a raw byte buffer into an array of DisasmInst.
    ///
    /// Parameters:
    ///   code      - pointer to ARM64 machine code bytes
    ///   code_len  - length of the code buffer in bytes
    ///   base_addr - virtual address of the first instruction (for display)
    ///   allocator - allocator for the returned slice
    ///
    /// Returns: owned slice of DisasmInst (caller must free with allocator)
    pub fn disassemble(
        self: *const Disassembler,
        code: [*]const u8,
        code_len: usize,
        base_addr: u64,
        allocator: std.mem.Allocator,
    ) CapstoneError![]DisasmInst {
        if (!self.initialized) return CapstoneError.NotInitialized;
        if (code_len == 0) return allocator.alloc(DisasmInst, 0) catch return CapstoneError.OutOfMemory;

        var cs_insns: [*c]cs_insn = null;
        const count = cs.cs_disasm(self.handle, code, code_len, base_addr, 0, &cs_insns);

        if (count == 0 or cs_insns == null) {
            // No instructions could be disassembled — return empty
            return allocator.alloc(DisasmInst, 0) catch return CapstoneError.OutOfMemory;
        }
        defer cs.cs_free(cs_insns, count);

        const result = allocator.alloc(DisasmInst, count) catch return CapstoneError.OutOfMemory;

        for (0..count) |i| {
            const ci = cs_insns[i];
            const ci_addr: u64 = ci.address;
            const offset: u32 = if (ci_addr >= base_addr)
                @intCast(ci_addr - base_addr)
            else
                0;

            var raw_word: u32 = 0;
            if (ci.size >= 4) {
                raw_word = @as(u32, ci.bytes[0]) |
                    (@as(u32, ci.bytes[1]) << 8) |
                    (@as(u32, ci.bytes[2]) << 16) |
                    (@as(u32, ci.bytes[3]) << 24);
            }

            result[i] = DisasmInst{
                .offset = offset,
                .address = ci_addr,
                .size = ci.size,
                .raw_word = raw_word,
                .mnemonic = ci.mnemonic,
                .op_str = ci.op_str,
            };
        }

        return result;
    }

    /// Disassemble a single instruction at a given offset.
    ///
    /// Returns null if the instruction could not be disassembled.
    pub fn disassembleOne(
        self: *const Disassembler,
        code: [*]const u8,
        code_len: usize,
        base_addr: u64,
    ) ?DisasmInst {
        if (!self.initialized) return null;
        if (code_len < 4) return null;

        var cs_insns: [*c]cs_insn = null;
        const count = cs.cs_disasm(self.handle, code, @min(code_len, 4), base_addr, 1, &cs_insns);
        if (count == 0 or cs_insns == null) return null;
        defer cs.cs_free(cs_insns, count);

        const ci = cs_insns[0];
        const ci_addr: u64 = ci.address;
        const offset: u32 = if (ci_addr >= base_addr)
            @intCast(ci_addr - base_addr)
        else
            0;

        var raw_word: u32 = 0;
        if (ci.size >= 4) {
            raw_word = @as(u32, ci.bytes[0]) |
                (@as(u32, ci.bytes[1]) << 8) |
                (@as(u32, ci.bytes[2]) << 16) |
                (@as(u32, ci.bytes[3]) << 24);
        }

        return DisasmInst{
            .offset = offset,
            .address = ci_addr,
            .size = ci.size,
            .raw_word = raw_word,
            .mnemonic = ci.mnemonic,
            .op_str = ci.op_str,
        };
    }
};

// ============================================================================
// Section: JIT Module Disassembly Report
// ============================================================================

/// Disassemble a JitModule's code buffer and write a formatted listing.
///
/// This is the primary integration point for the JIT verbose report. It
/// produces a listing like:
///
///   === Capstone Disassembly (ARM64) ===
///
///   $main:
///   .L1:
///     0x0000:  d10083ff    sub      sp, sp, #0x20
///     0x0004:  a9017bfd    stp      x29, x30, [sp, #0x10]
///     ...
///     0x001c:  94000000    bl       _samm_init           ; EXT
///     ...
///
/// Labels, function symbols, external call annotations, and source line
/// mappings are interleaved with the disassembly.
pub fn dumpModuleDisassembly(
    mod: *const JitModule,
    writer: anytype,
) !void {
    if (mod.code_len == 0) {
        try writer.writeAll("\n=== Capstone Disassembly (ARM64) ===\n");
        try writer.writeAll("  (no code)\n\n");
        return;
    }

    var disasm = Disassembler.init() catch {
        try writer.writeAll("\n=== Capstone Disassembly (ARM64) ===\n");
        try writer.writeAll("  (capstone init failed — falling back to hex dump)\n");
        try dumpCodeHexFallback(mod, writer);
        return;
    };
    defer disasm.deinit();

    try writer.writeAll("\n=== Capstone Disassembly (ARM64) ===\n\n");

    // Build reverse label map: code_offset → label_id
    var label_at = std.AutoHashMap(u32, i32).init(mod.allocator);
    defer label_at.deinit();
    {
        var li = mod.labels.iterator();
        while (li.next()) |entry| {
            label_at.put(entry.value_ptr.*, entry.key_ptr.*) catch {};
        }
    }

    // Build reverse symbol map: code_offset → symbol_name
    var func_at = std.AutoHashMap(u32, []const u8).init(mod.allocator);
    defer func_at.deinit();
    {
        var si = mod.symbols.iterator();
        while (si.next()) |entry| {
            if (entry.value_ptr.is_code) {
                func_at.put(entry.value_ptr.offset, entry.key_ptr.*) catch {};
            }
        }
    }

    // Build reverse ext-call map: code_offset → symbol_name
    var ext_at = std.AutoHashMap(u32, []const u8).init(mod.allocator);
    defer ext_at.deinit();
    for (mod.ext_calls.items) |*ext| {
        ext_at.put(ext.code_offset, ext.getName()) catch {};
    }

    // Comment map cursor — comments are sorted by code_offset (emission order)
    var comment_idx: usize = 0;

    // Disassemble instruction by instruction so we can interleave annotations
    var offset: u32 = 0;
    while (offset < mod.code_len) {
        // Emit any codegen comments at or before this offset
        while (comment_idx < mod.comment_map.items.len) {
            const entry = &mod.comment_map.items[comment_idx];
            if (entry.code_offset <= offset) {
                try std.fmt.format(writer, "                          ; {s}\n", .{entry.getText()});
                comment_idx += 1;
            } else {
                break;
            }
        }

        // Emit function symbol if present at this offset
        if (func_at.get(offset)) |sym_name| {
            try writer.writeAll("\n");
            try std.fmt.format(writer, "  {s}:\n", .{sym_name});
        }

        // Emit label if present at this offset
        if (label_at.get(offset)) |block_id| {
            try std.fmt.format(writer, "  .L{d}:\n", .{block_id});
        }

        // Disassemble this instruction
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

            // Main disassembly line
            try std.fmt.format(writer, "    0x{x:0>4}:  {x:0>8}    {s:<8}", .{
                offset, inst.raw_word, mnem,
            });

            if (ops.len > 0) {
                try std.fmt.format(writer, " {s}", .{ops});
            }

            // Annotations — external call target
            if (ext_at.get(offset)) |ext_sym| {
                try std.fmt.format(writer, "    ; → {s}", .{ext_sym});
            }

            // Source line annotation
            const src_line = sourceLineForOffset(mod, offset);
            if (src_line > 0) {
                try std.fmt.format(writer, "    ; line {d}", .{src_line});
            }

            try writer.writeAll("\n");

            offset += @as(u32, inst.size);
        } else {
            // Failed to disassemble — show raw hex
            const word = readWordFromSlice(mod.code[offset..]);
            try std.fmt.format(writer, "    0x{x:0>4}:  {x:0>8}    .word 0x{x:0>8}  ; <invalid>\n", .{
                offset, word, word,
            });
            offset += 4;
        }
    }

    // Emit any trailing comments past the last instruction
    while (comment_idx < mod.comment_map.items.len) {
        const entry = &mod.comment_map.items[comment_idx];
        try std.fmt.format(writer, "                          ; {s}\n", .{entry.getText()});
        comment_idx += 1;
    }

    try writer.writeAll("\n");
}

/// Disassemble a JitModule's trampoline island and write a formatted listing.
///
/// Shows the trampoline stubs used for external function calls:
///   LDR X16, [PC, #4]    ; load target address
///   BR  X16               ; indirect branch
///   .quad <target_addr>   ; 8-byte absolute address
pub fn dumpTrampolineDisassembly(
    trampoline_base: [*]const u8,
    trampoline_len: usize,
    writer: anytype,
) !void {
    if (trampoline_len == 0) return;

    var disasm = Disassembler.init() catch {
        try writer.writeAll("\n=== Trampoline Island ===\n");
        try writer.writeAll("  (capstone init failed)\n\n");
        return;
    };
    defer disasm.deinit();

    try writer.writeAll("\n=== Trampoline Island ===\n\n");

    const stub_size: u32 = 16; // TRAMPOLINE_STUB_SIZE from jit_memory.zig
    var stub_idx: u32 = 0;
    var offset: u32 = 0;

    while (offset + stub_size <= trampoline_len) {
        try std.fmt.format(writer, "  trampoline[{d}]:\n", .{stub_idx});

        // Disassemble the two instructions (LDR X16 + BR X16)
        var ioff: u32 = 0;
        while (ioff < 8) { // two 4-byte instructions
            const code_ptr = trampoline_base + offset + ioff;
            const inst_opt = disasm.disassembleOne(
                code_ptr,
                4,
                @as(u64, offset + ioff),
            );
            if (inst_opt) |inst| {
                const mnem = inst.getMnemonic();
                const ops = inst.getOperands();
                if (ops.len > 0) {
                    try std.fmt.format(writer, "    0x{x:0>4}:  {x:0>8}    {s:<8} {s}\n", .{
                        offset + ioff, inst.raw_word, mnem, ops,
                    });
                } else {
                    try std.fmt.format(writer, "    0x{x:0>4}:  {x:0>8}    {s}\n", .{
                        offset + ioff, inst.raw_word, mnem,
                    });
                }
                ioff += @as(u32, inst.size);
            } else {
                const raw = readWordFromPtr(trampoline_base + offset + ioff);
                try std.fmt.format(writer, "    0x{x:0>4}:  {x:0>8}    .word 0x{x:0>8}\n", .{
                    offset + ioff, raw, raw,
                });
                ioff += 4;
            }
        }

        // Show the 8-byte target address
        const addr_ptr = trampoline_base + offset + 8;
        const target_lo = readWordFromPtr(addr_ptr);
        const target_hi = readWordFromPtr(addr_ptr + 4);
        const target: u64 = @as(u64, target_lo) | (@as(u64, target_hi) << 32);
        try std.fmt.format(writer, "    0x{x:0>4}:  .quad 0x{x:0>16}", .{
            offset + 8, target,
        });
        if (target == 0) {
            try writer.writeAll("  ; <unresolved>");
        }
        try writer.writeAll("\n");

        offset += stub_size;
        stub_idx += 1;
    }

    try writer.writeAll("\n");
}

// ============================================================================
// Section: Full Disassembly Report
// ============================================================================

/// Produce a comprehensive disassembly report section for the JIT verbose report.
///
/// This is the top-level function called from JitSession.dumpFullReport().
/// It includes:
///   1. Code disassembly with annotations
///   2. Trampoline island disassembly (if trampoline info provided)
///   3. Instruction frequency analysis
pub fn dumpFullDisassemblyReport(
    mod: *const JitModule,
    trampoline_base: ?[*]const u8,
    trampoline_len: usize,
    writer: anytype,
) !void {
    try writer.writeAll("\n");
    try writer.writeAll("╔══════════════════════════════════════════════════════════════╗\n");
    try writer.writeAll("║           Capstone ARM64 Disassembly Analysis               ║\n");
    try writer.writeAll("╚══════════════════════════════════════════════════════════════╝\n");

    // Report Capstone version
    var major: c_int = 0;
    var minor: c_int = 0;
    _ = cs.cs_version(&major, &minor);
    try std.fmt.format(writer, "\n  Capstone version: {d}.{d}\n", .{ major, minor });
    try std.fmt.format(writer, "  Architecture:     AArch64 (ARM64)\n", .{});
    try std.fmt.format(writer, "  Code size:        {d} bytes ({d} instructions max)\n", .{
        mod.code_len, mod.code_len / 4,
    });

    // Code disassembly
    try dumpModuleDisassembly(mod, writer);

    // Trampoline disassembly
    if (trampoline_base) |base| {
        if (trampoline_len > 0) {
            try dumpTrampolineDisassembly(base, trampoline_len, writer);
        }
    }

    // Instruction frequency analysis
    if (mod.code_len > 0) {
        try dumpInstructionAnalysis(mod, writer);
    }
}

/// Disassemble the **linked** code buffer at its real mapped address.
///
/// Unlike `dumpModuleDisassembly` (which reads from `mod.code`, the pre-link
/// staging buffer with unpatched BL/ADRP placeholders), this reads directly
/// from the live JIT memory region after all linking is complete.  Every BL
/// target, ADRP page delta, and ADD page-offset will reflect the patched
/// values the CPU will actually execute.
///
/// Annotations include:
///   • function entry labels from the module symbol table
///   • block labels from the label map
///   • external call target names resolved via the link result
///   • codegen comments from the comment map (JIT_COMMENT pseudo-instructions)
///   • BASIC source-line numbers from the source map
pub fn dumpLinkedDisassembly(
    code_ptr: [*]const u8,
    code_len: usize,
    base_addr: u64,
    mod: *const JitModule,
    link_result: *const LinkResult,
    writer: anytype,
) !void {
    if (code_len == 0) {
        try writer.writeAll("\n=== Linked ARM64 Disassembly ===\n");
        try writer.writeAll("  (no code)\n\n");
        return;
    }

    var disasm = Disassembler.init() catch {
        try writer.writeAll("\n=== Linked ARM64 Disassembly ===\n");
        try writer.writeAll("  (capstone init failed — falling back to hex dump)\n");
        try dumpLinkedHexFallback(code_ptr, code_len, base_addr, writer);
        return;
    };
    defer disasm.deinit();

    try writer.writeAll("\n=== Linked ARM64 Disassembly (live code buffer) ===\n\n");

    // Build reverse label map: code_offset → label_id
    var label_at = std.AutoHashMap(u32, i32).init(mod.allocator);
    defer label_at.deinit();
    {
        var li = mod.labels.iterator();
        while (li.next()) |entry| {
            label_at.put(entry.value_ptr.*, entry.key_ptr.*) catch {};
        }
    }

    // Build reverse symbol map: code_offset → symbol_name
    var func_at = std.AutoHashMap(u32, []const u8).init(mod.allocator);
    defer func_at.deinit();
    {
        var si = mod.symbols.iterator();
        while (si.next()) |entry| {
            if (entry.value_ptr.is_code) {
                func_at.put(entry.value_ptr.offset, entry.key_ptr.*) catch {};
            }
        }
    }

    // Build reverse ext-call map: code_offset → symbol_name
    var ext_at = std.AutoHashMap(u32, []const u8).init(mod.allocator);
    defer ext_at.deinit();
    for (mod.ext_calls.items) |*ext| {
        ext_at.put(ext.code_offset, ext.getName()) catch {};
    }

    // Build trampoline stub offset → symbol name map for BL target annotation
    var tramp_at = std.AutoHashMap(u64, []const u8).init(mod.allocator);
    defer tramp_at.deinit();
    for (link_result.trampoline_stubs.items) |*stub| {
        const abs_addr = base_addr + stub.stub_offset;
        tramp_at.put(abs_addr, stub.getName()) catch {};
    }

    // Comment map cursor — comments are sorted by code_offset (emission order)
    var comment_idx: usize = 0;

    // Disassemble instruction by instruction with annotations
    var offset: u32 = 0;
    while (offset < code_len) {
        // Emit any codegen comments at or before this offset
        while (comment_idx < mod.comment_map.items.len) {
            const centry = &mod.comment_map.items[comment_idx];
            if (centry.code_offset <= offset) {
                try std.fmt.format(writer, "                                    ; {s}\n", .{centry.getText()});
                comment_idx += 1;
            } else {
                break;
            }
        }

        // Emit function symbol if present at this offset
        if (func_at.get(offset)) |sym_name| {
            try writer.writeAll("\n");
            try std.fmt.format(writer, "  {s}:\n", .{sym_name});
        }

        // Emit label if present at this offset
        if (label_at.get(offset)) |block_id| {
            try std.fmt.format(writer, "  .L{d}:\n", .{block_id});
        }

        // Disassemble this instruction from the live buffer
        const remaining = code_len - offset;
        const inst_opt = disasm.disassembleOne(
            code_ptr + offset,
            remaining,
            base_addr + offset,
        );

        if (inst_opt) |inst| {
            const mnem = inst.getMnemonic();
            const ops = inst.getOperands();

            // Main disassembly line with real address
            try std.fmt.format(writer, "    0x{x:0>12}:  {x:0>8}    {s:<8}", .{
                base_addr + offset, inst.raw_word, mnem,
            });

            if (ops.len > 0) {
                try std.fmt.format(writer, " {s}", .{ops});
            }

            // Annotation: external call target name (from pre-link ext_calls)
            if (ext_at.get(offset)) |ext_sym| {
                // Also try to resolve the BL target address to a stub name
                // for extra clarity (confirms the BL was patched correctly)
                try std.fmt.format(writer, "    ; → {s}", .{ext_sym});
            }

            // Annotation: if this is a BL, try to resolve target to a
            // trampoline stub name or internal function
            if (sw(mnem, "bl") and !ext_at.contains(offset)) {
                // Parse the BL target from operands (capstone gives absolute addr)
                // Format is typically "#0x..." — try to find it in tramp_at
                // Actually, we can compute it from the instruction word:
                const bl_word = inst.raw_word;
                const imm26_raw: u32 = bl_word & 0x03FFFFFF;
                // Sign-extend 26 bits to i32
                const imm26_signed: i32 = if (imm26_raw & 0x02000000 != 0)
                    @bitCast(imm26_raw | 0xFC000000)
                else
                    @intCast(imm26_raw);
                const target: u64 = @intCast(@as(i64, @intCast(base_addr + offset)) + @as(i64, imm26_signed) * 4);
                if (tramp_at.get(target)) |stub_name| {
                    try std.fmt.format(writer, "    ; → {s}", .{stub_name});
                }
            }

            // Source line annotation
            const src_line = sourceLineForOffset(mod, offset);
            if (src_line > 0) {
                try std.fmt.format(writer, "    ; line {d}", .{src_line});
            }

            try writer.writeAll("\n");
            offset += @as(u32, inst.size);
        } else {
            // Failed to disassemble — show raw hex
            const word = readWordFromPtr(code_ptr + offset);
            try std.fmt.format(writer, "    0x{x:0>12}:  {x:0>8}    .word 0x{x:0>8}  ; <invalid>\n", .{
                base_addr + offset, word, word,
            });
            offset += 4;
        }
    }

    // Emit any trailing comments past the last instruction
    while (comment_idx < mod.comment_map.items.len) {
        const centry = &mod.comment_map.items[comment_idx];
        try std.fmt.format(writer, "                                    ; {s}\n", .{centry.getText()});
        comment_idx += 1;
    }

    try writer.writeAll("\n");
}

/// Full linked disassembly report: code + trampolines + instruction analysis,
/// all from the live memory region with real addresses.
pub fn dumpLinkedReport(
    code_ptr: [*]const u8,
    code_len: usize,
    base_addr: u64,
    mod: *const JitModule,
    link_result: *const LinkResult,
    trampoline_base: ?[*]const u8,
    trampoline_len: usize,
    trampoline_real_addr: u64,
    writer: anytype,
) !void {
    try writer.writeAll("\n");
    try writer.writeAll("╔══════════════════════════════════════════════════════════════╗\n");
    try writer.writeAll("║       Linked ARM64 Disassembly (real addresses)             ║\n");
    try writer.writeAll("╚══════════════════════════════════════════════════════════════╝\n");

    // Report Capstone version
    var major: c_int = 0;
    var minor: c_int = 0;
    _ = cs.cs_version(&major, &minor);
    try std.fmt.format(writer, "\n  Capstone version: {d}.{d}\n", .{ major, minor });
    try std.fmt.format(writer, "  Architecture:     AArch64 (ARM64)\n", .{});
    try std.fmt.format(writer, "  Code base:        0x{x:0>12}\n", .{base_addr});
    try std.fmt.format(writer, "  Code size:        {d} bytes ({d} instructions)\n", .{
        code_len, code_len / 4,
    });

    // Linked code disassembly
    try dumpLinkedDisassembly(code_ptr, code_len, base_addr, mod, link_result, writer);

    // Trampoline disassembly (already at real addresses)
    if (trampoline_base) |base| {
        if (trampoline_len > 0) {
            try writer.writeAll("\n=== Trampoline Island (real addresses) ===\n\n");

            var disasm = Disassembler.init() catch {
                try writer.writeAll("  (capstone init failed)\n\n");
                return;
            };
            defer disasm.deinit();

            const stub_size: u32 = 16;
            var stub_idx: u32 = 0;
            var offset: u32 = 0;

            while (offset + stub_size <= trampoline_len) {
                // Match stub name by index — stubs are sequential
                var stub_name: []const u8 = "<unknown>";
                if (stub_idx < link_result.trampoline_stubs.items.len) {
                    stub_name = link_result.trampoline_stubs.items[stub_idx].getName();
                }

                try std.fmt.format(writer, "  trampoline[{d}] \"{s}\":\n", .{ stub_idx, stub_name });

                // Disassemble the two instructions (LDR X16 + BR X16)
                var ioff: u32 = 0;
                while (ioff < 8) {
                    const ip = base + offset + ioff;
                    const real_addr = trampoline_real_addr + offset + ioff;
                    const inst_opt = disasm.disassembleOne(ip, 4, real_addr);
                    if (inst_opt) |inst| {
                        const mnem = inst.getMnemonic();
                        const ops = inst.getOperands();
                        if (ops.len > 0) {
                            try std.fmt.format(writer, "    0x{x:0>12}:  {x:0>8}    {s:<8} {s}\n", .{
                                real_addr, inst.raw_word, mnem, ops,
                            });
                        } else {
                            try std.fmt.format(writer, "    0x{x:0>12}:  {x:0>8}    {s}\n", .{
                                real_addr, inst.raw_word, mnem,
                            });
                        }
                        ioff += @as(u32, inst.size);
                    } else {
                        const raw = readWordFromPtr(ip);
                        try std.fmt.format(writer, "    0x{x:0>12}:  {x:0>8}    .word 0x{x:0>8}\n", .{
                            real_addr, raw, raw,
                        });
                        ioff += 4;
                    }
                }

                // Show the 8-byte target address
                const addr_ptr = base + offset + 8;
                const target_lo = readWordFromPtr(addr_ptr);
                const target_hi = readWordFromPtr(addr_ptr + 4);
                const target: u64 = @as(u64, target_lo) | (@as(u64, target_hi) << 32);
                try std.fmt.format(writer, "    0x{x:0>12}:  .quad 0x{x:0>16}\n", .{
                    trampoline_real_addr + offset + 8, target,
                });

                offset += stub_size;
                stub_idx += 1;
            }

            try writer.writeAll("\n");
        }
    }

    // Instruction frequency analysis (from the live buffer)
    if (code_len > 0) {
        try dumpInstructionAnalysisFromBuffer(code_ptr, code_len, base_addr, writer);
    }
}

/// Instruction frequency analysis from a raw code buffer (for linked report).
fn dumpInstructionAnalysisFromBuffer(
    code_ptr: [*]const u8,
    code_len: usize,
    base_addr: u64,
    writer: anytype,
) !void {
    var disasm = Disassembler.init() catch return;
    defer disasm.deinit();

    try writer.writeAll("=== Instruction Analysis ===\n\n");

    var count_arith: u32 = 0;
    var count_memory: u32 = 0;
    var count_branch: u32 = 0;
    var count_move: u32 = 0;
    var count_system: u32 = 0;
    var count_neon: u32 = 0;
    var count_compare: u32 = 0;
    var count_fp: u32 = 0;
    var count_other: u32 = 0;
    var count_total: u32 = 0;

    var offset: u32 = 0;
    while (offset < code_len) {
        const inst_opt = disasm.disassembleOne(
            code_ptr + offset,
            code_len - offset,
            base_addr + offset,
        );

        if (inst_opt) |inst| {
            const mnem = inst.getMnemonic();
            count_total += 1;

            if (isArithmetic(mnem)) {
                count_arith += 1;
            } else if (isMemory(mnem)) {
                count_memory += 1;
            } else if (isBranch(mnem)) {
                count_branch += 1;
            } else if (isMove(mnem)) {
                count_move += 1;
            } else if (isCompare(mnem)) {
                count_compare += 1;
            } else if (isSystem(mnem)) {
                count_system += 1;
            } else if (isFloatingPoint(mnem)) {
                count_fp += 1;
            } else if (isNeon(mnem)) {
                count_neon += 1;
            } else {
                count_other += 1;
            }

            offset += @as(u32, inst.size);
        } else {
            count_other += 1;
            count_total += 1;
            offset += 4;
        }
    }

    try std.fmt.format(writer, "  Total instructions: {d}\n\n", .{count_total});
    try writer.writeAll("  Category breakdown:\n");

    if (count_arith > 0)
        try std.fmt.format(writer, "    Arithmetic:    {d:>5}  ({d:>2}%)\n", .{ count_arith, pct(count_arith, count_total) });
    if (count_memory > 0)
        try std.fmt.format(writer, "    Memory:        {d:>5}  ({d:>2}%)\n", .{ count_memory, pct(count_memory, count_total) });
    if (count_branch > 0)
        try std.fmt.format(writer, "    Branch:        {d:>5}  ({d:>2}%)\n", .{ count_branch, pct(count_branch, count_total) });
    if (count_move > 0)
        try std.fmt.format(writer, "    Move/Imm:      {d:>5}  ({d:>2}%)\n", .{ count_move, pct(count_move, count_total) });
    if (count_compare > 0)
        try std.fmt.format(writer, "    Compare:       {d:>5}  ({d:>2}%)\n", .{ count_compare, pct(count_compare, count_total) });
    if (count_fp > 0)
        try std.fmt.format(writer, "    Floating-Pt:   {d:>5}  ({d:>2}%)\n", .{ count_fp, pct(count_fp, count_total) });
    if (count_neon > 0)
        try std.fmt.format(writer, "    NEON/SIMD:     {d:>5}  ({d:>2}%)\n", .{ count_neon, pct(count_neon, count_total) });
    if (count_system > 0)
        try std.fmt.format(writer, "    System:        {d:>5}  ({d:>2}%)\n", .{ count_system, pct(count_system, count_total) });
    if (count_other > 0)
        try std.fmt.format(writer, "    Other:         {d:>5}  ({d:>2}%)\n", .{ count_other, pct(count_other, count_total) });

    try writer.writeAll("\n");
}

// ============================================================================
// Section: Instruction Frequency Analysis
// ============================================================================

/// Analyze the disassembled code for instruction frequency and patterns.
///
/// Provides a breakdown of instruction categories:
///   - Arithmetic (add, sub, mul, ...)
///   - Memory (ldr, str, ldp, stp, ...)
///   - Branch (b, bl, b.cc, cbz, ret, ...)
///   - Move/immediate (mov, movz, movk, ...)
///   - System (nop, brk, hint, ...)
///   - NEON/SIMD
fn dumpInstructionAnalysis(
    mod: *const JitModule,
    writer: anytype,
) !void {
    var disasm = Disassembler.init() catch return;
    defer disasm.deinit();

    try writer.writeAll("=== Instruction Analysis ===\n\n");

    var count_arith: u32 = 0;
    var count_memory: u32 = 0;
    var count_branch: u32 = 0;
    var count_move: u32 = 0;
    var count_system: u32 = 0;
    var count_neon: u32 = 0;
    var count_compare: u32 = 0;
    var count_fp: u32 = 0;
    var count_other: u32 = 0;
    var count_total: u32 = 0;

    var offset: u32 = 0;
    while (offset < mod.code_len) {
        const code_ptr = mod.code[offset..];
        const remaining = mod.code_len - offset;

        const inst_opt = disasm.disassembleOne(
            code_ptr.ptr,
            remaining,
            @as(u64, offset),
        );

        if (inst_opt) |inst| {
            const mnem = inst.getMnemonic();
            count_total += 1;

            // Classify instruction by mnemonic prefix
            if (isArithmetic(mnem)) {
                count_arith += 1;
            } else if (isMemory(mnem)) {
                count_memory += 1;
            } else if (isBranch(mnem)) {
                count_branch += 1;
            } else if (isMove(mnem)) {
                count_move += 1;
            } else if (isCompare(mnem)) {
                count_compare += 1;
            } else if (isSystem(mnem)) {
                count_system += 1;
            } else if (isFloatingPoint(mnem)) {
                count_fp += 1;
            } else if (isNeon(mnem)) {
                count_neon += 1;
            } else {
                count_other += 1;
            }

            offset += @as(u32, inst.size);
        } else {
            count_other += 1;
            count_total += 1;
            offset += 4;
        }
    }

    try std.fmt.format(writer, "  Total instructions: {d}\n\n", .{count_total});
    try writer.writeAll("  Category breakdown:\n");

    if (count_arith > 0)
        try std.fmt.format(writer, "    Arithmetic:    {d:>5}  ({d:>2}%)\n", .{ count_arith, pct(count_arith, count_total) });
    if (count_memory > 0)
        try std.fmt.format(writer, "    Memory:        {d:>5}  ({d:>2}%)\n", .{ count_memory, pct(count_memory, count_total) });
    if (count_branch > 0)
        try std.fmt.format(writer, "    Branch:        {d:>5}  ({d:>2}%)\n", .{ count_branch, pct(count_branch, count_total) });
    if (count_move > 0)
        try std.fmt.format(writer, "    Move/Imm:      {d:>5}  ({d:>2}%)\n", .{ count_move, pct(count_move, count_total) });
    if (count_compare > 0)
        try std.fmt.format(writer, "    Compare:       {d:>5}  ({d:>2}%)\n", .{ count_compare, pct(count_compare, count_total) });
    if (count_fp > 0)
        try std.fmt.format(writer, "    Floating-Pt:   {d:>5}  ({d:>2}%)\n", .{ count_fp, pct(count_fp, count_total) });
    if (count_neon > 0)
        try std.fmt.format(writer, "    NEON/SIMD:     {d:>5}  ({d:>2}%)\n", .{ count_neon, pct(count_neon, count_total) });
    if (count_system > 0)
        try std.fmt.format(writer, "    System:        {d:>5}  ({d:>2}%)\n", .{ count_system, pct(count_system, count_total) });
    if (count_other > 0)
        try std.fmt.format(writer, "    Other:         {d:>5}  ({d:>2}%)\n", .{ count_other, pct(count_other, count_total) });

    try writer.writeAll("\n");
}

// ============================================================================
// Section: Standalone Buffer Disassembly
// ============================================================================

/// Disassemble a raw byte buffer and write a simple listing.
///
/// This is useful for ad-hoc disassembly outside of the JIT module context,
/// such as inspecting trampoline stubs or patched code regions.
pub fn disassembleBuffer(
    code: [*]const u8,
    code_len: usize,
    base_addr: u64,
    writer: anytype,
) !void {
    var disasm = Disassembler.init() catch {
        try writer.writeAll("  (capstone init failed)\n");
        return;
    };
    defer disasm.deinit();

    var offset: usize = 0;
    while (offset < code_len) {
        const inst_opt = disasm.disassembleOne(
            code + offset,
            code_len - offset,
            base_addr + offset,
        );

        if (inst_opt) |inst| {
            try inst.format(writer);
            try writer.writeAll("\n");
            offset += inst.size;
        } else {
            const raw = readWordFromPtr(code + offset);
            try std.fmt.format(writer, "0x{x:0>4}:  {x:0>8}    .word 0x{x:0>8}\n", .{
                @as(u32, @intCast(offset)), raw, raw,
            });
            offset += 4;
        }
    }
}

/// Disassemble a raw byte buffer and return the result as an owned string.
pub fn disassembleToString(
    code: [*]const u8,
    code_len: usize,
    base_addr: u64,
    allocator: std.mem.Allocator,
) ![]u8 {
    var buf: std.ArrayListUnmanaged(u8) = .{};
    try disassembleBuffer(code, code_len, base_addr, buf.writer(allocator));
    return buf.toOwnedSlice(allocator);
}

// ============================================================================
// Section: Capstone Availability Check
// ============================================================================

/// Check whether the Capstone disassembler is available and functional.
///
/// Returns true if Capstone can be initialized for AArch64.
/// This is useful for graceful fallback in the JIT report when Capstone
/// is not linked or not working.
pub fn isAvailable() bool {
    var disasm = Disassembler.init() catch return false;
    disasm.deinit();
    return true;
}

/// Get the Capstone library version as a string.
pub fn versionString(buf: []u8) []u8 {
    var major: c_int = 0;
    var minor: c_int = 0;
    _ = cs.cs_version(&major, &minor);
    const result = std.fmt.bufPrint(buf, "{d}.{d}", .{ major, minor }) catch return buf[0..0];
    return result;
}

// ============================================================================
// Section: Internal Helpers
// ============================================================================

/// Look up the source line for a code offset using the module's source map.
fn sourceLineForOffset(mod: *const JitModule, offset: u32) u32 {
    if (mod.source_map.items.len == 0) return 0;

    var best_line: u32 = 0;
    for (mod.source_map.items) |entry| {
        if (entry.code_offset <= offset) {
            best_line = entry.source_line;
        } else {
            break;
        }
    }
    return best_line;
}

/// Read a little-endian u32 from a byte slice.
fn readWordFromSlice(slice: []const u8) u32 {
    if (slice.len < 4) return 0;
    return @as(u32, slice[0]) |
        (@as(u32, slice[1]) << 8) |
        (@as(u32, slice[2]) << 16) |
        (@as(u32, slice[3]) << 24);
}

/// Read a little-endian u32 from a byte pointer.
fn readWordFromPtr(ptr: [*]const u8) u32 {
    return @as(u32, ptr[0]) |
        (@as(u32, ptr[1]) << 8) |
        (@as(u32, ptr[2]) << 16) |
        (@as(u32, ptr[3]) << 24);
}

/// Check if a string starts with a given prefix.
fn sw(s: []const u8, prefix: []const u8) bool {
    return std.mem.startsWith(u8, s, prefix);
}

/// Check if mnemonic exactly equals a given string.
fn eq(s: []const u8, target: []const u8) bool {
    return std.mem.eql(u8, s, target);
}

/// Calculate percentage (integer, rounded).
fn pct(part: u32, total: u32) u32 {
    if (total == 0) return 0;
    return (part * 100 + total / 2) / total;
}

// ── Instruction classification helpers ─────────────────────────────────

fn isArithmetic(m: []const u8) bool {
    return sw(m, "add") or sw(m, "sub") or sw(m, "mul") or
        sw(m, "sdiv") or sw(m, "udiv") or sw(m, "neg") or
        sw(m, "and") or sw(m, "orr") or sw(m, "eor") or
        sw(m, "lsl") or sw(m, "lsr") or sw(m, "asr") or
        sw(m, "madd") or sw(m, "msub") or sw(m, "adc") or
        sw(m, "sbc") or sw(m, "ror") or sw(m, "bic") or
        sw(m, "eon") or sw(m, "orn") or sw(m, "cls") or
        sw(m, "clz") or sw(m, "rbit") or sw(m, "rev") or
        sw(m, "mneg") or sw(m, "smull") or sw(m, "umull") or
        sw(m, "smulh") or sw(m, "umulh") or sw(m, "smaddl") or
        sw(m, "umaddl") or sw(m, "smsubl") or sw(m, "umsubl");
}

fn isMemory(m: []const u8) bool {
    return sw(m, "ldr") or sw(m, "str") or sw(m, "ldp") or
        sw(m, "stp") or sw(m, "ldur") or sw(m, "stur") or
        sw(m, "ldar") or sw(m, "stlr") or sw(m, "ldxr") or
        sw(m, "stxr") or sw(m, "ldax") or sw(m, "stlx") or
        sw(m, "prfm") or sw(m, "ldtr") or sw(m, "sttr") or
        sw(m, "ldnp") or sw(m, "stnp");
}

fn isBranch(m: []const u8) bool {
    return sw(m, "b.") or eq(m, "b") or sw(m, "bl") or
        sw(m, "br") or sw(m, "ret") or sw(m, "cbz") or
        sw(m, "cbnz") or sw(m, "tbz") or sw(m, "tbnz");
}

fn isMove(m: []const u8) bool {
    return sw(m, "mov") or sw(m, "mvn") or sw(m, "adr") or
        sw(m, "csel") or sw(m, "cset") or sw(m, "csinc") or
        sw(m, "csinv") or sw(m, "csneg") or sw(m, "sxt") or
        sw(m, "uxt") or sw(m, "bfm") or sw(m, "ubfm") or
        sw(m, "sbfm") or sw(m, "bfi") or sw(m, "bfxil") or
        sw(m, "ubfiz") or sw(m, "sbfiz") or sw(m, "extr");
}

fn isCompare(m: []const u8) bool {
    return sw(m, "cmp") or sw(m, "cmn") or sw(m, "tst") or
        sw(m, "ccmp") or sw(m, "ccmn") or sw(m, "fcmp") or
        sw(m, "fccmp");
}

fn isSystem(m: []const u8) bool {
    return sw(m, "nop") or sw(m, "brk") or sw(m, "hint") or
        sw(m, "svc") or sw(m, "hvc") or sw(m, "smc") or
        sw(m, "isb") or sw(m, "dsb") or sw(m, "dmb") or
        sw(m, "bti") or sw(m, "pac") or sw(m, "aut") or
        sw(m, "mrs") or sw(m, "msr") or sw(m, "sys") or
        sw(m, "dc") or sw(m, "ic") or sw(m, "at") or
        sw(m, "tlbi") or sw(m, "wfe") or sw(m, "wfi") or
        sw(m, "sev") or sw(m, "yield") or sw(m, "clrex") or
        sw(m, "eret");
}

fn isFloatingPoint(m: []const u8) bool {
    return sw(m, "fadd") or sw(m, "fsub") or sw(m, "fmul") or
        sw(m, "fdiv") or sw(m, "fneg") or sw(m, "fabs") or
        sw(m, "fmov") or sw(m, "fcvt") or sw(m, "scvtf") or
        sw(m, "ucvtf") or sw(m, "fmadd") or sw(m, "fmsub") or
        sw(m, "fsqrt") or sw(m, "fmin") or sw(m, "fmax") or
        sw(m, "frint") or sw(m, "fcsel") or sw(m, "fnmadd") or
        sw(m, "fnmsub") or sw(m, "fnmul");
}

fn isNeon(m: []const u8) bool {
    return sw(m, "dup") or sw(m, "ins") or sw(m, "umov") or
        sw(m, "smov") or sw(m, "fmla") or sw(m, "fmls") or
        sw(m, "addv") or sw(m, "ext") or sw(m, "zip") or
        sw(m, "uzp") or sw(m, "trn") or sw(m, "tbl") or
        sw(m, "tbx") or sw(m, "cnt") or sw(m, "ld1") or
        sw(m, "st1") or sw(m, "ld2") or sw(m, "st2") or
        sw(m, "ld3") or sw(m, "st3") or sw(m, "ld4") or
        sw(m, "st4") or sw(m, "saddl") or sw(m, "uaddl") or
        sw(m, "ssubl") or sw(m, "usubl") or sw(m, "smull") or
        sw(m, "sqdmulh") or sw(m, "sqrdmulh") or sw(m, "mla") or
        sw(m, "mls") or sw(m, "abs") or sw(m, "sqabs") or
        sw(m, "smax") or sw(m, "smin") or sw(m, "umax") or
        sw(m, "umin") or sw(m, "addp") or sw(m, "uminv") or
        sw(m, "umaxv") or sw(m, "sminv") or sw(m, "smaxv");
}

/// Fallback hex dump when Capstone is not available.
fn dumpCodeHexFallback(mod: *const JitModule, writer: anytype) !void {
    try writer.writeAll("\n  Code (hex):\n");
    var offset: u32 = 0;
    while (offset < mod.code_len) : (offset += 4) {
        if (offset % 32 == 0) {
            if (offset > 0) try writer.writeAll("\n");
            try std.fmt.format(writer, "    {x:0>4}:", .{offset});
        }
        const word = readWordFromSlice(mod.code[offset..]);
        try std.fmt.format(writer, " {x:0>8}", .{word});
    }
    try writer.writeAll("\n\n");
}

fn dumpLinkedHexFallback(code_ptr: [*]const u8, code_len: usize, base_addr: u64, writer: anytype) !void {
    try writer.writeAll("\n  Linked code (hex):\n");
    var offset: u32 = 0;
    while (offset < code_len) : (offset += 4) {
        if (offset % 32 == 0) {
            if (offset > 0) try writer.writeAll("\n");
            try std.fmt.format(writer, "    0x{x:0>12}:", .{base_addr + offset});
        }
        const word = readWordFromPtr(code_ptr + offset);
        try std.fmt.format(writer, " {x:0>8}", .{word});
    }
    try writer.writeAll("\n\n");
}

// ============================================================================
// Section: Tests
// ============================================================================

test "Capstone availability check" {
    // This test verifies that the Capstone engine can be initialized.
    // If Capstone is not linked, this will fail at link time, which is expected.
    const available = isAvailable();
    try std.testing.expect(available);
}

test "Capstone version is valid" {
    var buf: [32]u8 = undefined;
    const ver = versionString(&buf);
    // Version should be non-empty and contain a dot
    try std.testing.expect(ver.len >= 3);
    try std.testing.expect(std.mem.indexOf(u8, ver, ".") != null);
}

test "Disassembler init and deinit" {
    var disasm = try Disassembler.init();
    defer disasm.deinit();
    try std.testing.expect(disasm.initialized);
}

test "Disassemble NOP instruction" {
    var disasm = try Disassembler.init();
    defer disasm.deinit();

    // ARM64 NOP = 0xd503201f
    const nop = [_]u8{ 0x1f, 0x20, 0x03, 0xd5 };
    const inst = disasm.disassembleOne(&nop, nop.len, 0);

    try std.testing.expect(inst != null);
    if (inst) |i| {
        const mnem = i.getMnemonic();
        try std.testing.expectEqualStrings("nop", mnem);
        try std.testing.expectEqual(@as(u16, 4), i.size);
        try std.testing.expectEqual(@as(u32, 0), i.offset);
    }
}

test "Disassemble RET instruction" {
    var disasm = try Disassembler.init();
    defer disasm.deinit();

    // ARM64 RET = 0xd65f03c0
    const ret_bytes = [_]u8{ 0xc0, 0x03, 0x5f, 0xd6 };
    const inst = disasm.disassembleOne(&ret_bytes, ret_bytes.len, 0);

    try std.testing.expect(inst != null);
    if (inst) |i| {
        const mnem = i.getMnemonic();
        try std.testing.expectEqualStrings("ret", mnem);
    }
}

test "Disassemble ADD X0, X1, X2" {
    var disasm = try Disassembler.init();
    defer disasm.deinit();

    // ADD X0, X1, X2 = 0x8b020020
    const add_bytes = [_]u8{ 0x20, 0x00, 0x02, 0x8b };
    const inst = disasm.disassembleOne(&add_bytes, add_bytes.len, 0);

    try std.testing.expect(inst != null);
    if (inst) |i| {
        const mnem = i.getMnemonic();
        try std.testing.expectEqualStrings("add", mnem);
        try std.testing.expectEqual(@as(u32, 0x8b020020), i.raw_word);
    }
}

test "Disassemble SUB SP, SP, #0x20" {
    var disasm = try Disassembler.init();
    defer disasm.deinit();

    // SUB SP, SP, #0x20 = 0xd10083ff
    const sub_bytes = [_]u8{ 0xff, 0x83, 0x00, 0xd1 };
    const inst = disasm.disassembleOne(&sub_bytes, sub_bytes.len, 0);

    try std.testing.expect(inst != null);
    if (inst) |i| {
        const mnem = i.getMnemonic();
        try std.testing.expectEqualStrings("sub", mnem);
        const ops = i.getOperands();
        // Should reference sp
        try std.testing.expect(std.mem.indexOf(u8, ops, "sp") != null);
    }
}

test "Disassemble BRK #0" {
    var disasm = try Disassembler.init();
    defer disasm.deinit();

    // BRK #0 = 0xd4200000
    const brk_bytes = [_]u8{ 0x00, 0x00, 0x20, 0xd4 };
    const inst = disasm.disassembleOne(&brk_bytes, brk_bytes.len, 0);

    try std.testing.expect(inst != null);
    if (inst) |i| {
        const mnem = i.getMnemonic();
        try std.testing.expectEqualStrings("brk", mnem);
    }
}

test "Disassemble STP X29, X30 pre-index" {
    var disasm = try Disassembler.init();
    defer disasm.deinit();

    // STP X29, X30, [SP, #-16]! = 0xa9bf7bfd
    const stp_bytes = [_]u8{ 0xfd, 0x7b, 0xbf, 0xa9 };
    const inst = disasm.disassembleOne(&stp_bytes, stp_bytes.len, 0);

    try std.testing.expect(inst != null);
    if (inst) |i| {
        const mnem = i.getMnemonic();
        try std.testing.expectEqualStrings("stp", mnem);
    }
}

test "Disassemble multiple instructions (batch)" {
    var disasm = try Disassembler.init();
    defer disasm.deinit();

    // SUB SP, SP, #0x20  |  STP X29, X30, [SP, #0x10]  |  RET
    const code = [_]u8{
        0xff, 0x83, 0x00, 0xd1, // sub sp, sp, #0x20
        0xfd, 0x7b, 0x01, 0xa9, // stp x29, x30, [sp, #0x10]
        0xc0, 0x03, 0x5f, 0xd6, // ret
    };

    const insts = try disasm.disassemble(&code, code.len, 0, std.testing.allocator);
    defer std.testing.allocator.free(insts);

    try std.testing.expectEqual(@as(usize, 3), insts.len);

    try std.testing.expectEqualStrings("sub", insts[0].getMnemonic());
    try std.testing.expectEqual(@as(u32, 0), insts[0].offset);

    try std.testing.expectEqualStrings("stp", insts[1].getMnemonic());
    try std.testing.expectEqual(@as(u32, 4), insts[1].offset);

    try std.testing.expectEqualStrings("ret", insts[2].getMnemonic());
    try std.testing.expectEqual(@as(u32, 8), insts[2].offset);
}

test "DisasmInst format produces expected output" {
    var disasm = try Disassembler.init();
    defer disasm.deinit();

    const nop = [_]u8{ 0x1f, 0x20, 0x03, 0xd5 };
    const inst = disasm.disassembleOne(&nop, nop.len, 0) orelse
        return error.TestUnexpectedResult;

    var buf: [256]u8 = undefined;
    var fbs = std.io.fixedBufferStream(&buf);
    try inst.format(fbs.writer());
    const output = fbs.getWritten();

    // Should contain the offset, hex word, and mnemonic
    try std.testing.expect(std.mem.indexOf(u8, output, "0x0000") != null);
    try std.testing.expect(std.mem.indexOf(u8, output, "nop") != null);
}

test "disassembleBuffer writes listing" {
    const code = [_]u8{
        0x1f, 0x20, 0x03, 0xd5, // nop
        0xc0, 0x03, 0x5f, 0xd6, // ret
    };

    var buf: [512]u8 = undefined;
    var fbs = std.io.fixedBufferStream(&buf);
    try disassembleBuffer(&code, code.len, 0, fbs.writer());
    const output = fbs.getWritten();

    try std.testing.expect(output.len > 0);
    try std.testing.expect(std.mem.indexOf(u8, output, "nop") != null);
    try std.testing.expect(std.mem.indexOf(u8, output, "ret") != null);
}

test "readWordFromSlice little-endian" {
    const bytes = [_]u8{ 0x78, 0x56, 0x34, 0x12 };
    const word = readWordFromSlice(&bytes);
    try std.testing.expectEqual(@as(u32, 0x12345678), word);
}

test "readWordFromSlice short input" {
    const bytes = [_]u8{ 0x01, 0x02 };
    const word = readWordFromSlice(&bytes);
    try std.testing.expectEqual(@as(u32, 0), word);
}

test "pct calculation" {
    try std.testing.expectEqual(@as(u32, 50), pct(1, 2));
    try std.testing.expectEqual(@as(u32, 100), pct(10, 10));
    try std.testing.expectEqual(@as(u32, 0), pct(0, 10));
    try std.testing.expectEqual(@as(u32, 0), pct(0, 0));
    try std.testing.expectEqual(@as(u32, 33), pct(1, 3));
}

test "instruction classification helpers" {
    try std.testing.expect(isArithmetic("add"));
    try std.testing.expect(isArithmetic("sub"));
    try std.testing.expect(isArithmetic("adds"));
    try std.testing.expect(isArithmetic("madd"));
    try std.testing.expect(!isArithmetic("ldr"));

    try std.testing.expect(isMemory("ldr"));
    try std.testing.expect(isMemory("str"));
    try std.testing.expect(isMemory("ldp"));
    try std.testing.expect(isMemory("stp"));
    try std.testing.expect(!isMemory("add"));

    try std.testing.expect(isBranch("b"));
    try std.testing.expect(isBranch("bl"));
    try std.testing.expect(isBranch("b.eq"));
    try std.testing.expect(isBranch("ret"));
    try std.testing.expect(isBranch("cbz"));
    try std.testing.expect(!isBranch("mov"));

    try std.testing.expect(isMove("mov"));
    try std.testing.expect(isMove("movz"));
    try std.testing.expect(isMove("adrp"));
    try std.testing.expect(isMove("csel"));
    try std.testing.expect(isMove("sxtw"));
    try std.testing.expect(!isMove("str"));

    try std.testing.expect(isCompare("cmp"));
    try std.testing.expect(isCompare("tst"));
    try std.testing.expect(isCompare("fcmp"));
    try std.testing.expect(!isCompare("add"));

    try std.testing.expect(isSystem("nop"));
    try std.testing.expect(isSystem("brk"));
    try std.testing.expect(isSystem("svc"));
    try std.testing.expect(isSystem("dsb"));
    try std.testing.expect(!isSystem("mov"));

    try std.testing.expect(isFloatingPoint("fadd"));
    try std.testing.expect(isFloatingPoint("fmov"));
    try std.testing.expect(isFloatingPoint("scvtf"));
    try std.testing.expect(!isFloatingPoint("add"));

    try std.testing.expect(isNeon("dup"));
    try std.testing.expect(isNeon("ld1"));
    try std.testing.expect(isNeon("addv"));
    try std.testing.expect(!isNeon("ldr"));
}

test "Disassembler not initialized returns error" {
    var disasm = Disassembler{
        .handle = 0,
        .initialized = false,
    };

    const result = disasm.disassemble(undefined, 0, 0, std.testing.allocator);
    try std.testing.expectError(CapstoneError.NotInitialized, result);

    const one = disasm.disassembleOne(undefined, 0, 0);
    try std.testing.expect(one == null);
}

test "disassembleOne with short input returns null" {
    var disasm = try Disassembler.init();
    defer disasm.deinit();

    const short = [_]u8{ 0x00, 0x01 };
    const inst = disasm.disassembleOne(&short, short.len, 0);
    try std.testing.expect(inst == null);
}
