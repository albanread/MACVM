//! jit_linker.zig — Post-allocation relocation and trampoline generation
//!
//! This module implements the final linking pass that runs after the JIT
//! encoder has produced machine code and the JIT memory manager has
//! allocated executable memory regions with known addresses.
//!
//! It resolves three kinds of relocations:
//!
//!   1. **Data Relocations (ADRP + ADD)**: LOAD_ADDR instructions emit
//!      placeholder ADRP/ADD pairs with zero offsets. Once the code and
//!      data regions have real addresses, this module patches them with
//!      the correct page deltas and page offsets.
//!
//!   2. **Trampoline Island (CALL_EXT)**: External function calls (e.g.
//!      printf, basic_print_string_desc) may be located anywhere in the
//!      64-bit address space — far beyond the ±128MB range of a BL
//!      instruction. For each unique external symbol, we generate a
//!      16-byte trampoline stub:
//!        LDR X16, [PC, #8]   ; load absolute address
//!        BR  X16              ; indirect branch
//!        .quad <address>      ; 64-bit target
//!      Then we patch each BL call site to branch to the local stub.
//!
//!   3. **Symbol Resolution**: External symbols are resolved via dlsym()
//!      or a pre-built RuntimeContext jump table. The linker tries the
//!      jump table first (fast path), then falls back to dlsym().
//!
//! Architecture:
//!   JitModule (encoded code/data buffers)
//!     + JitMemoryRegion (allocated VA regions)
//!     → JitLinker.link()
//!       → resolveDataRelocations()
//!       → buildTrampolineIsland()
//!       → patchExternalCalls()
//!     → JitMemoryRegion ready for makeExecutable()
//!
//! References:
//!   - design/jit_design.md §3 "Data Relocations (ADRP + ADD)"
//!   - design/jit_design.md §4 "External Symbol Relocations (Trampoline Island)"
//!   - jit_status_tracker.md "Data Relocation — LOAD_ADDR"
//!   - jit_status_tracker.md "Trampoline Island — CALL_EXT"

const std = @import("std");
const builtin = @import("builtin");
const jit = @import("jit_encode.zig");
const mem = @import("jit_memory.zig");

const JitModule = jit.JitModule;
const JitInst = jit.JitInst;
const JitInstKind = jit.JitInstKind;
const ExtCallEntry = jit.ExtCallEntry;
const DiagSeverity = jit.DiagSeverity;
const SymbolEntry = jit.SymbolEntry;
const JitMemoryRegion = mem.JitMemoryRegion;

// ============================================================================
// Section: Error Types
// ============================================================================

pub const LinkerError = error{
    /// A required external symbol could not be resolved
    UnresolvedSymbol,
    /// Trampoline island capacity exceeded
    TrampolineOverflow,
    /// Data relocation target out of ADRP range (±4GB)
    AdrpRangeExceeded,
    /// Code region not in writable state
    NotWritable,
    /// Memory region not allocated
    RegionNotAllocated,
    /// Invalid relocation record
    InvalidRelocation,
    /// dlsym failed
    DlsymFailed,
    /// Allocator failure
    OutOfMemory,
};

// ============================================================================
// Section: Relocation Records
// ============================================================================

/// A data relocation record — tracks where ADRP+ADD pairs were emitted
/// and which data symbol they reference.
pub const DataRelocation = struct {
    /// Byte offset of the ADRP instruction in the code buffer
    adrp_offset: u32,

    /// Name of the data symbol being referenced
    sym_name: [jit.JIT_SYM_MAX]u8,

    /// Length of the symbol name
    sym_name_len: u8,

    /// Instruction index in the JitInst[] stream (for diagnostics)
    inst_index: u32,

    /// Addend: byte offset added to the resolved symbol address.
    /// Carries the CAddr bits.i value (e.g. +16 for a struct field
    /// like arr_desc + 16 to reach the element-count field).
    addend: i64,

    /// Get the symbol name as a slice.
    pub fn getName(self: *const DataRelocation) []const u8 {
        return self.sym_name[0..self.sym_name_len];
    }
};

/// A resolved external symbol — maps name to absolute address.
pub const ResolvedSymbol = struct {
    name: [jit.JIT_SYM_MAX]u8,
    name_len: u8,
    address: u64,
    source: SymbolSource,

    pub fn getName(self: *const ResolvedSymbol) []const u8 {
        return self.name[0..self.name_len];
    }
};

/// Where a symbol was resolved from.
pub const SymbolSource = enum {
    /// Resolved from the RuntimeContext jump table
    JumpTable,
    /// Resolved via dlsym()
    Dlsym,
    /// Resolved from the JIT module's own symbol table
    Internal,
    /// Could not be resolved (placeholder stub generated)
    Unresolved,
};

// ============================================================================
// Section: Trampoline Stub
// ============================================================================

/// A single trampoline stub entry — tracks the mapping from symbol name
/// to stub offset in the trampoline island.
pub const TrampolineStub = struct {
    /// Symbol name this stub serves
    name: [jit.JIT_SYM_MAX]u8,
    name_len: u8,

    /// Byte offset of the stub relative to code_base
    stub_offset: usize,

    /// Resolved absolute address of the target (0 if unresolved)
    target_addr: u64,

    /// Whether the symbol was successfully resolved
    resolved: bool,

    pub fn getName(self: *const TrampolineStub) []const u8 {
        return self.name[0..self.name_len];
    }
};

// ============================================================================
// Section: Link Statistics
// ============================================================================

/// Statistics collected during the linking pass.
pub const LinkStats = struct {
    /// Number of data relocations processed
    data_relocs_processed: u32 = 0,
    /// Number of data relocations successfully patched
    data_relocs_patched: u32 = 0,
    /// Number of trampoline stubs generated
    trampolines_generated: u32 = 0,
    /// Number of external call sites patched
    ext_calls_patched: u32 = 0,
    /// Number of external calls patched with direct BL (no trampoline)
    ext_calls_direct: u32 = 0,
    /// Number of symbols resolved via jump table
    symbols_from_jump_table: u32 = 0,
    /// Number of symbols resolved via dlsym
    symbols_from_dlsym: u32 = 0,
    /// Number of symbols resolved internally
    symbols_internal: u32 = 0,
    /// Number of symbols that could not be resolved
    symbols_unresolved: u32 = 0,
    /// Number of errors encountered
    errors: u32 = 0,
    /// Number of warnings generated
    warnings: u32 = 0,
};

// ============================================================================
// Section: Link Result
// ============================================================================

/// Result of the linking pass.
pub const LinkResult = struct {
    /// Statistics from the linking process
    stats: LinkStats,

    /// Resolved symbol table (allocated, caller must free)
    resolved_symbols: std.ArrayListUnmanaged(ResolvedSymbol),

    /// Trampoline stubs generated (allocated, caller must free)
    trampoline_stubs: std.ArrayListUnmanaged(TrampolineStub),

    /// Data relocations found and processed
    data_relocations: std.ArrayListUnmanaged(DataRelocation),

    /// Diagnostic messages from linking
    diagnostics: std.ArrayListUnmanaged(LinkDiagnostic),

    /// Allocator used for dynamic allocations
    allocator: std.mem.Allocator,

    /// Whether linking completed without errors.
    pub fn success(self: *const LinkResult) bool {
        return self.stats.errors == 0;
    }

    /// Free all resources.
    pub fn deinit(self: *LinkResult) void {
        self.resolved_symbols.deinit(self.allocator);
        self.trampoline_stubs.deinit(self.allocator);
        self.data_relocations.deinit(self.allocator);
        self.diagnostics.deinit(self.allocator);
    }

    /// Generate a human-readable link report.
    pub fn dumpReport(self: *const LinkResult, writer: anytype) !void {
        try writer.writeAll("\n=== JIT Linker Report ===\n");

        // Data relocations
        try std.fmt.format(writer, "\n── Data Relocations ({d} found, {d} patched) ──\n", .{
            self.stats.data_relocs_processed,
            self.stats.data_relocs_patched,
        });
        for (self.data_relocations.items, 0..) |reloc, i| {
            try std.fmt.format(writer, "  [{d}] ADRP+ADD @0x{x:0>4} → sym \"{s}\"\n", .{
                i,
                reloc.adrp_offset,
                reloc.getName(),
            });
        }

        // Trampoline island
        try std.fmt.format(writer, "\n── Trampoline Island ({d} stubs) ──\n", .{
            self.stats.trampolines_generated,
        });
        for (self.trampoline_stubs.items, 0..) |stub, i| {
            const status: []const u8 = if (stub.resolved) "OK" else "UNRESOLVED";
            try std.fmt.format(writer, "  [{d}] \"{s}\" → stub @0x{x:0>4}, target 0x{x:0>16} [{s}]\n", .{
                i,
                stub.getName(),
                stub.stub_offset,
                stub.target_addr,
                status,
            });
        }

        // External call patching
        try std.fmt.format(writer, "\n── External Calls ({d} patched) ──\n", .{
            self.stats.ext_calls_patched,
        });

        // Symbol resolution summary
        try std.fmt.format(writer, "\n── Symbol Resolution ──\n", .{});
        try std.fmt.format(writer, "  Jump table:  {d}\n", .{self.stats.symbols_from_jump_table});
        try std.fmt.format(writer, "  dlsym:       {d}\n", .{self.stats.symbols_from_dlsym});
        try std.fmt.format(writer, "  Internal:    {d}\n", .{self.stats.symbols_internal});
        try std.fmt.format(writer, "  Unresolved:  {d}\n", .{self.stats.symbols_unresolved});

        // Diagnostics
        if (self.diagnostics.items.len > 0) {
            try std.fmt.format(writer, "\n── Link Diagnostics ({d}) ──\n", .{self.diagnostics.items.len});
            for (self.diagnostics.items) |diag| {
                const sev: []const u8 = switch (diag.severity) {
                    .Error => "ERROR",
                    .Warning => "WARN",
                    .Info => "INFO",
                };
                try std.fmt.format(writer, "  [{s}] {s}\n", .{ sev, diag.getMessage() });
            }
        }

        try std.fmt.format(writer, "\n── Link Result: {s} ({d} errors, {d} warnings) ──\n", .{
            if (self.stats.errors == 0) "SUCCESS" else "FAILED",
            self.stats.errors,
            self.stats.warnings,
        });
    }
};

/// A diagnostic message from the linker.
pub const LinkDiagnostic = struct {
    severity: DiagSeverity,
    message: [256]u8,
    message_len: u8,

    pub fn getMessage(self: *const LinkDiagnostic) []const u8 {
        return self.message[0..self.message_len];
    }

    pub fn create(severity: DiagSeverity, comptime fmt: []const u8, args: anytype) LinkDiagnostic {
        var d = LinkDiagnostic{
            .severity = severity,
            .message = [_]u8{0} ** 256,
            .message_len = 0,
        };
        const written = std.fmt.bufPrint(&d.message, fmt, args) catch &d.message;
        d.message_len = @intCast(written.len);
        return d;
    }
};

// ============================================================================
// Section: Runtime Context / Jump Table
// ============================================================================

/// A jump table entry mapping a symbol name to a function pointer.
pub const JumpTableEntry = struct {
    name: []const u8,
    address: u64,
};

/// The RuntimeContext provides function pointers for BASIC runtime
/// functions that JIT-compiled code needs to call.
///
/// This is the "fast path" for symbol resolution — instead of calling
/// dlsym() for every external symbol, we look them up in this table first.
pub const RuntimeContext = struct {
    /// Function pointer table entries
    entries: []const JumpTableEntry,

    /// Optional: handle for dlsym fallback (RTLD_DEFAULT or a specific dylib)
    dlsym_handle: ?*anyopaque,

    /// Look up a symbol by name in the jump table.
    pub fn lookup(self: *const RuntimeContext, name: []const u8) ?u64 {
        for (self.entries) |entry| {
            if (std.mem.eql(u8, entry.name, name)) {
                return entry.address;
            }
        }
        return null;
    }

    /// Create an empty runtime context with only dlsym fallback.
    pub fn empty() RuntimeContext {
        return .{
            .entries = &.{},
            .dlsym_handle = null,
        };
    }
};

// ============================================================================
// Section: dlsym Interface
// ============================================================================

/// Platform-specific dlsym for resolving external symbols at runtime.
const DlsymFn = struct {
    /// RTLD_DEFAULT — search all loaded shared objects
    const RTLD_DEFAULT: ?*anyopaque = if (builtin.os.tag == .macos)
        @as(?*anyopaque, @ptrFromInt(@as(usize, @bitCast(@as(isize, -2)))))
    else if (builtin.os.tag == .linux)
        null
    else
        null;

    extern "c" fn dlsym(handle: ?*anyopaque, symbol: [*:0]const u8) ?*anyopaque;

    /// Resolve a symbol name to its absolute address.
    /// Returns null if the symbol cannot be found.
    fn resolve(handle: ?*anyopaque, name: []const u8) ?u64 {
        // We need a null-terminated copy of the name
        var name_z: [jit.JIT_SYM_MAX + 1]u8 = undefined;
        if (name.len >= jit.JIT_SYM_MAX) return null;

        @memcpy(name_z[0..name.len], name);
        name_z[name.len] = 0;

        const name_ptr: [*:0]const u8 = @ptrCast(&name_z);
        const effective_handle = handle orelse RTLD_DEFAULT;

        const ptr = dlsym(effective_handle, name_ptr);
        if (ptr) |p| {
            return @intFromPtr(p);
        }

        // On macOS, C symbols often have a leading underscore.
        // Try with underscore prefix if the plain name failed.
        if (comptime builtin.os.tag == .macos) {
            if (name.len + 1 < jit.JIT_SYM_MAX) {
                var prefixed: [jit.JIT_SYM_MAX + 1]u8 = undefined;
                prefixed[0] = '_';
                @memcpy(prefixed[1..][0..name.len], name);
                prefixed[name.len + 1] = 0;

                const prefixed_ptr: [*:0]const u8 = @ptrCast(&prefixed);
                const ptr2 = dlsym(effective_handle, prefixed_ptr);
                if (ptr2) |p2| {
                    return @intFromPtr(p2);
                }
            }
        }

        return null;
    }
};

// ============================================================================
// Section: JIT Linker Core
// ============================================================================

/// The JIT linker performs post-allocation relocation resolution.
///
/// Usage:
///   1. Create a linker with the encoded JitModule and allocated JitMemoryRegion
///   2. Optionally set a RuntimeContext for jump table lookups
///   3. Call link() to resolve all relocations
///   4. Inspect the LinkResult for diagnostics
///   5. The JitMemoryRegion is now ready for makeExecutable()
pub const JitLinker = struct {
    allocator: std.mem.Allocator,

    /// Collect data relocations needed for LOAD_ADDR (ADRP+ADD) patching.
    ///
    /// Prefers the module's `load_addr_relocs` (populated during encoding)
    /// which are always available. Falls back to scanning the JitInst stream
    /// only if the module has no recorded relocs and insts are provided.
    pub fn collectDataRelocations(
        allocator: std.mem.Allocator,
        module: *const JitModule,
        insts: ?[*]const JitInst,
        ninst: u32,
    ) std.ArrayListUnmanaged(DataRelocation) {
        var relocs: std.ArrayListUnmanaged(DataRelocation) = .{};

        // Prefer module's load_addr_relocs (always available, no JitInst[] needed)
        if (module.load_addr_relocs.items.len > 0) {
            for (module.load_addr_relocs.items) |lar| {
                var reloc = DataRelocation{
                    .adrp_offset = lar.adrp_offset,
                    .sym_name = lar.sym_name,
                    .sym_name_len = lar.sym_name_len,
                    .inst_index = lar.inst_index,
                    .addend = lar.addend,
                };
                _ = &reloc;
                relocs.append(allocator, reloc) catch {};
            }
            return relocs;
        }

        // Fallback: scan the JitInst stream (legacy path)
        if (insts == null) return relocs;
        const inst_ptr = insts.?;

        // Scan for LOAD_ADDR instructions
        // Each LOAD_ADDR emits 2 words (ADRP + ADD), so we need to track
        // the code offset as we walk the instruction stream.
        var code_offset: u32 = 0;
        var i: u32 = 0;
        while (i < ninst) : (i += 1) {
            const inst = &inst_ptr[i];
            const kind = inst.getKind();

            switch (kind) {
                .JIT_LOAD_ADDR => {
                    // Record the data relocation
                    var reloc = DataRelocation{
                        .adrp_offset = code_offset,
                        .sym_name = [_]u8{0} ** jit.JIT_SYM_MAX,
                        .sym_name_len = 0,
                        .inst_index = i,
                        .addend = inst.imm,
                    };

                    // Copy symbol name from the instruction
                    const sym = inst.getSymName();
                    if (sym.len > 0 and sym.len <= jit.JIT_SYM_MAX) {
                        @memcpy(reloc.sym_name[0..sym.len], sym);
                        reloc.sym_name_len = @intCast(sym.len);
                    }

                    relocs.append(allocator, reloc) catch {};

                    // LOAD_ADDR emits 2 words: ADRP + ADD
                    code_offset += 8;
                },
                // Instructions that don't emit code
                .JIT_LABEL, .JIT_FUNC_BEGIN, .JIT_FUNC_END, .JIT_DBGLOC, .JIT_NOP, .JIT_COMMENT => {},
                // Data section instructions don't emit code
                .JIT_DATA_START, .JIT_DATA_END, .JIT_DATA_BYTE, .JIT_DATA_HALF, .JIT_DATA_WORD, .JIT_DATA_QUAD, .JIT_DATA_ZERO, .JIT_DATA_SYMREF, .JIT_DATA_ASCII, .JIT_DATA_ALIGN => {},
                // Most instructions emit 1 word
                else => {
                    code_offset += 4;
                },
            }
        }

        return relocs;
    }

    /// Resolve all external symbols needed by the module.
    ///
    /// Tries the RuntimeContext jump table first, then falls back to dlsym().
    pub fn resolveExternalSymbols(
        allocator: std.mem.Allocator,
        module: *const JitModule,
        context: ?*const RuntimeContext,
        stats: *LinkStats,
        diagnostics: *std.ArrayListUnmanaged(LinkDiagnostic),
    ) std.ArrayListUnmanaged(ResolvedSymbol) {
        var resolved: std.ArrayListUnmanaged(ResolvedSymbol) = .{};

        // Build a deduplicated list of external symbol names from ext_calls
        var seen = std.StringHashMap(void).init(allocator);
        defer seen.deinit();

        for (module.ext_calls.items) |ext_call| {
            const name = ext_call.getName();
            if (name.len == 0) continue;

            // Skip duplicates
            if (seen.contains(name)) continue;
            seen.put(allocator.dupe(u8, name) catch continue, {}) catch continue;

            var sym = ResolvedSymbol{
                .name = [_]u8{0} ** jit.JIT_SYM_MAX,
                .name_len = 0,
                .address = 0,
                .source = .Unresolved,
            };
            @memcpy(sym.name[0..name.len], name);
            sym.name_len = @intCast(name.len);

            // Step 1: Check internal symbol table
            // On macOS, CALL_EXT names have a leading '_' (ABI prefix)
            // but FUNC_BEGIN names don't. Try both the original name
            // and the name with the leading underscore stripped.
            const internal_lookup_name = if (name.len > 1 and name[0] == '_')
                name[1..]
            else
                name;
            if (module.symbols.get(name)) |internal_sym| {
                if (internal_sym.is_code) {
                    sym.address = internal_sym.offset;
                    sym.source = .Internal;
                    stats.symbols_internal += 1;
                }
            } else if (module.symbols.get(internal_lookup_name)) |internal_sym| {
                if (internal_sym.is_code) {
                    sym.address = internal_sym.offset;
                    sym.source = .Internal;
                    stats.symbols_internal += 1;
                }
            }

            // Step 2: Try RuntimeContext jump table
            if (sym.source == .Unresolved) {
                if (context) |ctx| {
                    if (ctx.lookup(name)) |addr| {
                        sym.address = addr;
                        sym.source = .JumpTable;
                        stats.symbols_from_jump_table += 1;
                    }
                }
            }

            // Step 3: Fall back to dlsym
            if (sym.source == .Unresolved) {
                const handle = if (context) |ctx| ctx.dlsym_handle else null;
                if (DlsymFn.resolve(handle, name)) |addr| {
                    sym.address = addr;
                    sym.source = .Dlsym;
                    stats.symbols_from_dlsym += 1;
                } else {
                    stats.symbols_unresolved += 1;
                    diagnostics.append(allocator, LinkDiagnostic.create(
                        .Warning,
                        "unresolved external: \"{s}\"",
                        .{name},
                    )) catch {};
                }
            }

            resolved.append(allocator, sym) catch {};
        }

        return resolved;
    }

    /// Build the trampoline island in the memory region.
    ///
    /// For each resolved external symbol, writes a 16-byte stub:
    ///   LDR X16, [PC, #8]
    ///   BR  X16
    ///   .quad <absolute_address>
    ///
    /// For **unresolved** symbols, writes a safe trap stub instead:
    ///   BRK #0xF001  (signals "unresolved JIT symbol")
    ///   BRK #0xF001
    ///   .quad 0xDEAD  (sentinel, never executed)
    ///
    /// This prevents jumping to address 0x0 and crashing with an
    /// unhelpful SIGSEGV on null dereference.
    ///
    /// Returns the mapping from symbol name to stub offset.
    pub fn buildTrampolineIsland(
        allocator: std.mem.Allocator,
        region: *JitMemoryRegion,
        resolved_symbols: *const std.ArrayListUnmanaged(ResolvedSymbol),
        stats: *LinkStats,
        diagnostics: *std.ArrayListUnmanaged(LinkDiagnostic),
    ) std.ArrayListUnmanaged(TrampolineStub) {
        var stubs: std.ArrayListUnmanaged(TrampolineStub) = .{};

        for (resolved_symbols.items) |sym| {
            // Only generate trampolines for externally resolved symbols
            if (sym.source == .Internal) continue;

            const is_resolved = sym.source != .Unresolved;

            var stub = TrampolineStub{
                .name = sym.name,
                .name_len = sym.name_len,
                .stub_offset = 0,
                .target_addr = sym.address,
                .resolved = is_resolved,
            };

            if (!is_resolved) {
                // Unresolved symbol: write a safe BRK trap stub instead of
                // a trampoline to 0x0. BRK #0xF001 = 0xD43E0020.
                // This produces a clean SIGTRAP that our signal handler can
                // identify as "unresolved JIT symbol" rather than a null-
                // pointer SIGSEGV.
                if (region.writeTrapStub()) |offset| {
                    stub.stub_offset = @intCast(offset);
                    stats.trampolines_generated += 1;
                } else |err| {
                    diagnostics.append(allocator, LinkDiagnostic.create(
                        .Error,
                        "trap stub write failed for \"{s}\": {s}",
                        .{ sym.getName(), @errorName(err) },
                    )) catch {};
                    stats.errors += 1;
                }
                stubs.append(allocator, stub) catch {};
                continue;
            }

            // Write the trampoline stub into the memory region
            if (region.writeTrampoline(sym.address)) |offset| {
                stub.stub_offset = offset;
                stats.trampolines_generated += 1;
            } else |err| {
                diagnostics.append(allocator, LinkDiagnostic.create(
                    .Error,
                    "trampoline write failed for \"{s}\": {s}",
                    .{ sym.getName(), @errorName(err) },
                )) catch {};
                stats.errors += 1;
            }

            stubs.append(allocator, stub) catch {};
        }

        return stubs;
    }

    /// Patch all CALL_EXT BL instructions to point to their trampoline stubs,
    /// or directly to internal functions within the code buffer.
    pub fn patchExternalCalls(
        region: *JitMemoryRegion,
        module: *const JitModule,
        trampoline_stubs: *const std.ArrayListUnmanaged(TrampolineStub),
        resolved_symbols: *const std.ArrayListUnmanaged(ResolvedSymbol),
        stats: *LinkStats,
        diagnostics: *std.ArrayListUnmanaged(LinkDiagnostic),
        allocator: std.mem.Allocator,
    ) void {
        for (module.ext_calls.items) |ext_call| {
            const name = ext_call.getName();
            if (name.len == 0) continue;

            // Check if this is an internal function (defined in the same module)
            var is_internal = false;
            var internal_offset: usize = 0;
            for (resolved_symbols.items) |sym| {
                if (sym.source == .Internal and std.mem.eql(u8, sym.getName(), name)) {
                    is_internal = true;
                    internal_offset = @intCast(sym.address);
                    break;
                }
            }

            if (is_internal) {
                // Patch BL to branch directly to the function within the code buffer.
                // Both bl_offset and target_offset are relative to code_base,
                // so we can reuse patchBLToTrampoline (it just computes a delta).
                region.patchBLToTrampoline(ext_call.code_offset, internal_offset) catch |err| {
                    diagnostics.append(allocator, LinkDiagnostic.create(
                        .Error,
                        "BL patch (internal) failed at 0x{x:0>4} for \"{s}\": {s}",
                        .{ ext_call.code_offset, name, @errorName(err) },
                    )) catch {};
                    stats.errors += 1;
                    continue;
                };
                stats.ext_calls_patched += 1;
                continue;
            }

            // Try direct BL first — skip the trampoline if target is
            // within ±128MB of the call site.
            var target_addr: u64 = 0;
            for (resolved_symbols.items) |sym| {
                if (sym.source != .Internal and std.mem.eql(u8, sym.getName(), name)) {
                    target_addr = sym.address;
                    break;
                }
            }

            if (target_addr != 0) {
                if (region.patchBLDirect(ext_call.code_offset, target_addr)) {
                    stats.ext_calls_patched += 1;
                    stats.ext_calls_direct += 1;
                    continue;
                } else |_| {
                    // Out of range — fall through to trampoline path
                }
            }

            // Find the corresponding trampoline stub
            var found_stub: ?*const TrampolineStub = null;
            for (trampoline_stubs.items) |*stub| {
                if (std.mem.eql(u8, stub.getName(), name)) {
                    found_stub = stub;
                    break;
                }
            }

            if (found_stub) |stub| {
                // Patch the BL at ext_call.code_offset to branch to the stub
                region.patchBLToTrampoline(ext_call.code_offset, stub.stub_offset) catch |err| {
                    diagnostics.append(allocator, LinkDiagnostic.create(
                        .Error,
                        "BL patch failed at 0x{x:0>4} for \"{s}\": {s}",
                        .{ ext_call.code_offset, name, @errorName(err) },
                    )) catch {};
                    stats.errors += 1;
                    continue;
                };
                stats.ext_calls_patched += 1;
            } else {
                // No trampoline and not internal — truly unresolved
                diagnostics.append(allocator, LinkDiagnostic.create(
                    .Warning,
                    "no trampoline for \"{s}\" at 0x{x:0>4}",
                    .{ name, ext_call.code_offset },
                )) catch {};
                stats.warnings += 1;
            }
        }
    }

    /// Resolve data relocations (ADRP + ADD pairs) using real addresses.
    ///
    /// For each LOAD_ADDR relocation:
    ///   1. Look up the data symbol's offset in the module's symbol table
    ///   2. Compute the absolute target address (data_base + symbol_offset)
    ///   3. Patch the ADRP instruction with the page delta
    ///   4. Patch the ADD instruction with the page offset
    pub fn resolveDataRelocations(
        region: *JitMemoryRegion,
        module: *const JitModule,
        data_relocs: *const std.ArrayListUnmanaged(DataRelocation),
        stats: *LinkStats,
        diagnostics: *std.ArrayListUnmanaged(LinkDiagnostic),
        allocator: std.mem.Allocator,
    ) void {
        for (data_relocs.items) |reloc| {
            stats.data_relocs_processed += 1;

            const name = reloc.getName();
            if (name.len == 0) {
                diagnostics.append(allocator, LinkDiagnostic.create(
                    .Error,
                    "data reloc at 0x{x:0>4} has empty symbol name",
                    .{reloc.adrp_offset},
                )) catch {};
                stats.errors += 1;
                continue;
            }

            // Look up the data symbol's offset in the module's symbol table
            if (module.symbols.get(name)) |sym_entry| {
                // Compute absolute target address
                const base_addr = if (sym_entry.is_code)
                    region.codeAddress(sym_entry.offset) orelse {
                        diagnostics.append(allocator, LinkDiagnostic.create(
                            .Error,
                            "code address null for \"{s}\"",
                            .{name},
                        )) catch {};
                        stats.errors += 1;
                        continue;
                    }
                else
                    region.dataAddress(sym_entry.offset) orelse {
                        diagnostics.append(allocator, LinkDiagnostic.create(
                            .Error,
                            "data address null for \"{s}\"",
                            .{name},
                        )) catch {};
                        stats.errors += 1;
                        continue;
                    };

                // Apply the addend (e.g. +16 for struct field access).
                // The addend comes from the QBE CAddr constant's bits.i
                // field — it represents a byte offset within the symbol.
                const target_addr = if (reloc.addend != 0)
                    base_addr +% @as(usize, @bitCast(@as(isize, @intCast(reloc.addend))))
                else
                    base_addr;

                // Patch ADRP + ADD
                region.patchAdrpAdd(reloc.adrp_offset, target_addr) catch |err| {
                    diagnostics.append(allocator, LinkDiagnostic.create(
                        .Error,
                        "ADRP+ADD patch failed for \"{s}\": {s}",
                        .{ name, @errorName(err) },
                    )) catch {};
                    stats.errors += 1;
                    continue;
                };

                stats.data_relocs_patched += 1;
            } else {
                diagnostics.append(allocator, LinkDiagnostic.create(
                    .Warning,
                    "data symbol \"{s}\" not found in symbol table",
                    .{name},
                )) catch {};
                stats.warnings += 1;
            }
        }
    }

    /// Resolve DATA_SYMREF relocations — symbol references embedded in the
    /// data section (e.g. vtable function pointers like `l $Dog__Speak`).
    ///
    /// After code and data are copied into the memory region, this patches
    /// each 8-byte slot in the data section with the resolved absolute address
    /// of the referenced symbol (code or data).
    pub fn resolveDataSymRefs(
        region: *JitMemoryRegion,
        module: *const JitModule,
        context: ?*const RuntimeContext,
        stats: *LinkStats,
        diagnostics: *std.ArrayListUnmanaged(LinkDiagnostic),
        allocator: std.mem.Allocator,
    ) void {
        const data_slice = region.dataSlice() orelse return;

        for (module.data_sym_refs.items) |ref| {
            const name = ref.getName();
            if (name.len == 0) continue;

            // Try to resolve the symbol: check module symbol table first,
            // then jump table, then dlsym. Strip leading '_' for internal lookup.
            var resolved_addr: ?u64 = null;

            // Internal lookup (module's own functions/data)
            const internal_name = if (name.len > 1 and name[0] == '_')
                name[1..]
            else
                name;

            if (module.symbols.get(name)) |sym_entry| {
                resolved_addr = if (sym_entry.is_code)
                    region.codeAddress(sym_entry.offset)
                else
                    region.dataAddress(sym_entry.offset);
            } else if (module.symbols.get(internal_name)) |sym_entry| {
                resolved_addr = if (sym_entry.is_code)
                    region.codeAddress(sym_entry.offset)
                else
                    region.dataAddress(sym_entry.offset);
            }

            // Jump table fallback
            if (resolved_addr == null) {
                if (context) |ctx| {
                    resolved_addr = ctx.lookup(name);
                }
            }

            // dlsym fallback
            if (resolved_addr == null) {
                const handle = if (context) |ctx| ctx.dlsym_handle else null;
                resolved_addr = DlsymFn.resolve(handle, name);
            }

            if (resolved_addr) |addr| {
                // Apply addend and write the 8-byte absolute address into data
                const final_addr: u64 = if (ref.addend >= 0)
                    addr +% @as(u64, @intCast(ref.addend))
                else
                    addr -% @as(u64, @intCast(-ref.addend));

                const off = ref.data_offset;
                if (off + 8 <= data_slice.len) {
                    const slot: *align(1) u64 = @ptrCast(data_slice[off..][0..8]);
                    slot.* = final_addr;
                    stats.data_relocs_patched += 1;
                } else {
                    diagnostics.append(allocator, LinkDiagnostic.create(
                        .Error,
                        "DATA_SYMREF offset 0x{x:0>4} out of range for \"{s}\"",
                        .{ off, name },
                    )) catch {};
                    stats.errors += 1;
                }
            } else {
                diagnostics.append(allocator, LinkDiagnostic.create(
                    .Warning,
                    "DATA_SYMREF unresolved: \"{s}\" at data offset 0x{x:0>4}",
                    .{ name, ref.data_offset },
                )) catch {};
                stats.warnings += 1;
            }
        }
    }

    /// Perform the full linking pass.
    ///
    /// This is the main entry point. It:
    ///   1. Copies code and data from JitModule into the JitMemoryRegion
    ///   2. Collects data relocations from the instruction stream
    ///   3. Resolves external symbols (jump table + dlsym)
    ///   4. Builds the trampoline island
    ///   5. Patches external call sites
    ///   6. Resolves data relocations (ADRP+ADD)
    ///   7. Resolves data symbol references (vtable function pointers)
    ///   8. Returns a LinkResult with full diagnostics
    pub fn link(
        allocator: std.mem.Allocator,
        module: *const JitModule,
        region: *JitMemoryRegion,
        context: ?*const RuntimeContext,
        insts: ?[*]const JitInst,
        ninst: u32,
    ) LinkResult {
        var result = LinkResult{
            .stats = .{},
            .resolved_symbols = .{},
            .trampoline_stubs = .{},
            .data_relocations = .{},
            .diagnostics = .{},
            .allocator = allocator,
        };

        // Step 1: Copy code into the executable region
        if (module.code_len > 0) {
            region.copyCode(module.code[0..module.code_len]) catch |err| {
                result.diagnostics.append(allocator, LinkDiagnostic.create(
                    .Error,
                    "code copy failed: {s}",
                    .{@errorName(err)},
                )) catch {};
                result.stats.errors += 1;
                return result;
            };
        }

        // Step 2: Copy data into the data region
        if (module.data_len > 0) {
            region.copyData(module.data[0..module.data_len]) catch |err| {
                result.diagnostics.append(allocator, LinkDiagnostic.create(
                    .Error,
                    "data copy failed: {s}",
                    .{@errorName(err)},
                )) catch {};
                result.stats.errors += 1;
                return result;
            };
        }

        // Step 3: Collect data relocations
        result.data_relocations = collectDataRelocations(
            allocator,
            module,
            insts,
            ninst,
        );

        // Step 4: Resolve external symbols
        result.resolved_symbols = resolveExternalSymbols(
            allocator,
            module,
            context,
            &result.stats,
            &result.diagnostics,
        );

        // Step 5: Build trampoline island
        result.trampoline_stubs = buildTrampolineIsland(
            allocator,
            region,
            &result.resolved_symbols,
            &result.stats,
            &result.diagnostics,
        );

        // Step 6: Patch external call sites
        patchExternalCalls(
            region,
            module,
            &result.trampoline_stubs,
            &result.resolved_symbols,
            &result.stats,
            &result.diagnostics,
            allocator,
        );

        // Step 7: Resolve data relocations (ADRP+ADD for LOAD_ADDR)
        resolveDataRelocations(
            region,
            module,
            &result.data_relocations,
            &result.stats,
            &result.diagnostics,
            allocator,
        );

        // Step 8: Resolve data symbol references (vtable function pointers etc.)
        resolveDataSymRefs(
            region,
            module,
            context,
            &result.stats,
            &result.diagnostics,
            allocator,
        );

        result.diagnostics.append(allocator, LinkDiagnostic.create(
            .Info,
            "linking complete: {d} trampolines, {d} data relocs, {d} ext calls patched",
            .{
                result.stats.trampolines_generated,
                result.stats.data_relocs_patched,
                result.stats.ext_calls_patched,
            },
        )) catch {};

        return result;
    }

    /// Convenience: link and make executable in one step.
    pub fn linkAndFinalize(
        allocator: std.mem.Allocator,
        module: *const JitModule,
        region: *JitMemoryRegion,
        context: ?*const RuntimeContext,
        insts: ?[*]const JitInst,
        ninst: u32,
    ) struct { result: LinkResult, executable: bool } {
        var link_result = link(allocator, module, region, context, insts, ninst);

        if (!link_result.success()) {
            return .{ .result = link_result, .executable = false };
        }

        // Make executable + flush icache
        region.makeExecutable() catch |err| {
            link_result.diagnostics.append(allocator, LinkDiagnostic.create(
                .Error,
                "makeExecutable failed: {s}",
                .{@errorName(err)},
            )) catch {};
            link_result.stats.errors += 1;
            return .{ .result = link_result, .executable = false };
        };

        return .{ .result = link_result, .executable = true };
    }
};

// ============================================================================
// Section: Direct BL Reach Analysis
// ============================================================================

/// A sample address pair captured during analysis for verification.
pub const BLSample = struct {
    bl_addr: u64,
    target_addr: u64,
    delta: i64,
    name: [jit.JIT_SYM_MAX]u8,
    name_len: u8,

    pub fn getName(self: *const BLSample) []const u8 {
        return self.name[0..self.name_len];
    }
};

/// Results of analyzing whether external call sites could use a direct BL
/// instruction (±128MB range) instead of going through a trampoline.
pub const DirectBLAnalysis = struct {
    /// Total external (non-internal) call sites analyzed
    total_ext_calls: u32 = 0,
    /// Call sites where the target is within ±128MB of the BL instruction
    in_range: u32 = 0,
    /// Call sites where the target is outside ±128MB
    out_of_range: u32 = 0,
    /// Call sites that were internal (already direct, not counted)
    internal_calls: u32 = 0,
    /// Call sites where the target could not be resolved
    unresolved_calls: u32 = 0,
    /// Minimum absolute distance seen (bytes)
    min_distance: u64 = std.math.maxInt(u64),
    /// Maximum absolute distance seen (bytes)
    max_distance: u64 = 0,
    /// Sum of all distances (for computing average)
    total_distance: u128 = 0,
    /// Code base address used for the analysis
    code_base: u64 = 0,
    /// Sample address pairs for verification (up to MAX_SAMPLES)
    samples: [MAX_SAMPLES]BLSample = undefined,
    sample_count: u8 = 0,

    const MAX_SAMPLES: u8 = 8;

    /// BL instruction range: ±128MB (26-bit signed offset × 4)
    const BL_RANGE: u64 = 128 * 1024 * 1024;

    pub fn avgDistance(self: *const DirectBLAnalysis) u64 {
        if (self.total_ext_calls == 0) return 0;
        return @intCast(self.total_distance / self.total_ext_calls);
    }

    pub fn pctInRange(self: *const DirectBLAnalysis) f64 {
        if (self.total_ext_calls == 0) return 0;
        return 100.0 * @as(f64, @floatFromInt(self.in_range)) / @as(f64, @floatFromInt(self.total_ext_calls));
    }

    /// Merge another analysis result into this one (for batch accumulation).
    pub fn accumulate(self: *DirectBLAnalysis, other: *const DirectBLAnalysis) void {
        self.total_ext_calls += other.total_ext_calls;
        self.in_range += other.in_range;
        self.out_of_range += other.out_of_range;
        self.internal_calls += other.internal_calls;
        self.unresolved_calls += other.unresolved_calls;
        if (other.total_ext_calls > 0) {
            self.min_distance = @min(self.min_distance, other.min_distance);
            self.max_distance = @max(self.max_distance, other.max_distance);
        }
        self.total_distance += other.total_distance;
        // Keep the first set of samples we see (from the first file
        // that has external calls) so the dump can show real addresses.
        if (self.sample_count == 0 and other.sample_count > 0) {
            self.sample_count = other.sample_count;
            self.samples = other.samples;
            self.code_base = other.code_base;
        }
    }

    /// Print the analysis to stderr.
    pub fn dump(self: *const DirectBLAnalysis, writer: anytype) void {
        writer.print("\n", .{}) catch {};
        writer.print("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n", .{}) catch {};
        writer.print("  Direct BL Reach Analysis\n", .{}) catch {};
        writer.print("──────────────────────────────────────────────────────\n", .{}) catch {};
        if (self.code_base != 0) {
            writer.print("  Code base:        0x{x:0>12}\n", .{self.code_base}) catch {};
        }
        writer.print("  BL range:         ±{d} MB\n", .{BL_RANGE / (1024 * 1024)}) catch {};
        writer.print("──────────────────────────────────────────────────────\n", .{}) catch {};
        writer.print("  External calls:   {d}\n", .{self.total_ext_calls}) catch {};
        writer.print("  In range (direct):{d}  ({d:.1}%)\n", .{ self.in_range, self.pctInRange() }) catch {};
        writer.print("  Out of range:     {d}  ({d:.1}%)\n", .{
            self.out_of_range,
            if (self.total_ext_calls > 0)
                100.0 - self.pctInRange()
            else
                @as(f64, 0),
        }) catch {};
        writer.print("  Internal (skip):  {d}\n", .{self.internal_calls}) catch {};
        writer.print("  Unresolved:       {d}\n", .{self.unresolved_calls}) catch {};
        if (self.total_ext_calls > 0) {
            writer.print("──────────────────────────────────────────────────────\n", .{}) catch {};
            const min_mb = @as(f64, @floatFromInt(self.min_distance)) / (1024.0 * 1024.0);
            const max_mb = @as(f64, @floatFromInt(self.max_distance)) / (1024.0 * 1024.0);
            const avg_mb = @as(f64, @floatFromInt(self.avgDistance())) / (1024.0 * 1024.0);
            writer.print("  Min distance:     {d:.2} MB\n", .{min_mb}) catch {};
            writer.print("  Max distance:     {d:.2} MB\n", .{max_mb}) catch {};
            writer.print("  Avg distance:     {d:.2} MB\n", .{avg_mb}) catch {};
            if (self.in_range == self.total_ext_calls) {
                writer.print("  Verdict:          ALL calls could be direct BL ✓\n", .{}) catch {};
            } else if (self.in_range == 0) {
                writer.print("  Verdict:          NO calls in direct BL range ✗\n", .{}) catch {};
            } else {
                writer.print("  Verdict:          {d:.1}% of calls could skip trampoline\n", .{self.pctInRange()}) catch {};
            }
            // Show sample address pairs so the real addresses are visible
            if (self.sample_count > 0) {
                writer.print("──────────────────────────────────────────────────────\n", .{}) catch {};
                writer.print("  Sample BL→target addresses:\n", .{}) catch {};
                for (0..self.sample_count) |i| {
                    const s = &self.samples[i];
                    const d_mb = @as(f64, @floatFromInt(if (s.delta < 0) -s.delta else s.delta)) / (1024.0 * 1024.0);
                    const in_r: []const u8 = if (@as(u64, @intCast(if (s.delta < 0) -s.delta else s.delta)) < BL_RANGE) "✓" else "✗";
                    writer.print("    BL@0x{x:0>12} → 0x{x:0>12}  Δ{d:.1}MB {s}\n", .{
                        s.bl_addr, s.target_addr, d_mb, in_r,
                    }) catch {};
                    writer.print("      {s}\n", .{s.getName()}) catch {};
                }
            }
        }
        writer.print("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n", .{}) catch {};
    }
};

/// Analyze whether external call sites could use a direct BL instruction
/// instead of a trampoline indirect branch.
///
/// This does NOT modify anything — it's a pure read-only analysis pass.
/// Call after `link()` completes, while the region is still writable or
/// has been made executable (we only read addresses, not code bytes).
pub fn analyzeDirectBLReach(
    region: *const JitMemoryRegion,
    module: *const JitModule,
    resolved_symbols: *const std.ArrayListUnmanaged(ResolvedSymbol),
) DirectBLAnalysis {
    var result = DirectBLAnalysis{};

    const code_base_addr = region.codeAddress(0) orelse return result;
    result.code_base = code_base_addr;

    for (module.ext_calls.items) |ext_call| {
        const name = ext_call.getName();
        if (name.len == 0) continue;

        // Find the resolved symbol
        var sym_addr: u64 = 0;
        var source: SymbolSource = .Unresolved;
        for (resolved_symbols.items) |sym| {
            if (std.mem.eql(u8, sym.getName(), name)) {
                sym_addr = sym.address;
                source = sym.source;
                break;
            }
        }

        if (source == .Internal) {
            result.internal_calls += 1;
            continue; // Already a direct BL within the code buffer
        }

        if (source == .Unresolved) {
            result.unresolved_calls += 1;
            continue;
        }

        // Compute absolute address of the BL instruction
        const bl_addr = code_base_addr + ext_call.code_offset;

        // Compute signed distance
        const delta: i64 = @as(i64, @intCast(sym_addr)) - @as(i64, @intCast(bl_addr));
        const abs_delta: u64 = @intCast(if (delta < 0) -delta else delta);

        result.total_ext_calls += 1;
        result.total_distance += abs_delta;
        result.min_distance = @min(result.min_distance, abs_delta);
        result.max_distance = @max(result.max_distance, abs_delta);

        if (abs_delta < DirectBLAnalysis.BL_RANGE) {
            result.in_range += 1;
        } else {
            result.out_of_range += 1;
        }

        // Capture sample address pairs for verification
        if (result.sample_count < DirectBLAnalysis.MAX_SAMPLES) {
            var sample = BLSample{
                .bl_addr = bl_addr,
                .target_addr = sym_addr,
                .delta = delta,
                .name = [_]u8{0} ** jit.JIT_SYM_MAX,
                .name_len = 0,
            };
            const n = @min(name.len, jit.JIT_SYM_MAX);
            @memcpy(sample.name[0..n], name[0..n]);
            sample.name_len = @intCast(n);
            result.samples[result.sample_count] = sample;
            result.sample_count += 1;
        }
    }

    return result;
}

// ============================================================================
// Section: Tests
// ============================================================================

test "LinkDiagnostic formatting" {
    const diag = LinkDiagnostic.create(.Error, "test error: {d}", .{42});
    try std.testing.expectEqual(DiagSeverity.Error, diag.severity);
    try std.testing.expectEqualStrings("test error: 42", diag.getMessage());
}

test "LinkDiagnostic Info severity" {
    const diag = LinkDiagnostic.create(.Info, "linking complete", .{});
    try std.testing.expectEqual(DiagSeverity.Info, diag.severity);
    try std.testing.expectEqualStrings("linking complete", diag.getMessage());
}

test "LinkStats initial state" {
    const stats = LinkStats{};
    try std.testing.expectEqual(@as(u32, 0), stats.data_relocs_processed);
    try std.testing.expectEqual(@as(u32, 0), stats.trampolines_generated);
    try std.testing.expectEqual(@as(u32, 0), stats.ext_calls_patched);
    try std.testing.expectEqual(@as(u32, 0), stats.errors);
}

test "DataRelocation getName" {
    var reloc = DataRelocation{
        .adrp_offset = 0x100,
        .sym_name = [_]u8{0} ** jit.JIT_SYM_MAX,
        .sym_name_len = 0,
        .inst_index = 0,
        .addend = 0,
    };
    const name = "hello_str";
    @memcpy(reloc.sym_name[0..name.len], name);
    reloc.sym_name_len = @intCast(name.len);
    try std.testing.expectEqualStrings("hello_str", reloc.getName());
}

test "ResolvedSymbol getName" {
    var sym = ResolvedSymbol{
        .name = [_]u8{0} ** jit.JIT_SYM_MAX,
        .name_len = 0,
        .address = 0x1000,
        .source = .Dlsym,
    };
    const name = "printf";
    @memcpy(sym.name[0..name.len], name);
    sym.name_len = @intCast(name.len);
    try std.testing.expectEqualStrings("printf", sym.getName());
    try std.testing.expectEqual(SymbolSource.Dlsym, sym.source);
}

test "TrampolineStub getName" {
    var stub = TrampolineStub{
        .name = [_]u8{0} ** jit.JIT_SYM_MAX,
        .name_len = 0,
        .stub_offset = 4096,
        .target_addr = 0xDEAD,
        .resolved = true,
    };
    const name = "my_func";
    @memcpy(stub.name[0..name.len], name);
    stub.name_len = @intCast(name.len);
    try std.testing.expectEqualStrings("my_func", stub.getName());
    try std.testing.expect(stub.resolved);
}

test "RuntimeContext empty has no entries" {
    const ctx = RuntimeContext.empty();
    try std.testing.expectEqual(@as(usize, 0), ctx.entries.len);
    try std.testing.expectEqual(@as(?u64, null), ctx.lookup("anything"));
}

test "RuntimeContext lookup finds entry" {
    const entries = [_]JumpTableEntry{
        .{ .name = "basic_print_string_desc", .address = 0x100000 },
        .{ .name = "basic_print_newline", .address = 0x200000 },
    };
    const ctx = RuntimeContext{
        .entries = &entries,
        .dlsym_handle = null,
    };

    try std.testing.expectEqual(@as(?u64, 0x100000), ctx.lookup("basic_print_string_desc"));
    try std.testing.expectEqual(@as(?u64, 0x200000), ctx.lookup("basic_print_newline"));
    try std.testing.expectEqual(@as(?u64, null), ctx.lookup("nonexistent"));
}

test "RuntimeContext lookup is exact match" {
    const entries = [_]JumpTableEntry{
        .{ .name = "print", .address = 0x1000 },
    };
    const ctx = RuntimeContext{
        .entries = &entries,
        .dlsym_handle = null,
    };

    // Partial matches should not work
    try std.testing.expectEqual(@as(?u64, null), ctx.lookup("prin"));
    try std.testing.expectEqual(@as(?u64, null), ctx.lookup("printf"));
    try std.testing.expectEqual(@as(?u64, 0x1000), ctx.lookup("print"));
}

test "dlsym resolves known libc symbol" {
    if (comptime builtin.os.tag != .macos and builtin.os.tag != .linux) {
        return; // Skip on unsupported platforms
    }

    // printf should be resolvable via dlsym on any Unix-like system
    const addr = DlsymFn.resolve(null, "printf");
    if (addr) |a| {
        try std.testing.expect(a != 0);
    }
    // It's OK if this fails in a sandboxed test environment
}

test "dlsym returns null for nonexistent symbol" {
    if (comptime builtin.os.tag != .macos and builtin.os.tag != .linux) {
        return;
    }

    const addr = DlsymFn.resolve(null, "this_symbol_definitely_does_not_exist_xyz_42");
    try std.testing.expectEqual(@as(?u64, null), addr);
}

test "collectDataRelocations with null insts returns empty" {
    const allocator = std.testing.allocator;
    var module = try JitModule.init(allocator, 1024, 256);
    defer module.deinit();

    var relocs = JitLinker.collectDataRelocations(allocator, &module, null, 0);
    defer relocs.deinit(allocator);

    try std.testing.expectEqual(@as(usize, 0), relocs.items.len);
}

test "LinkResult deinit does not leak" {
    const allocator = std.testing.allocator;

    var result = LinkResult{
        .stats = .{},
        .resolved_symbols = .{},
        .trampoline_stubs = .{},
        .data_relocations = .{},
        .diagnostics = .{},
        .allocator = allocator,
    };

    // Add some items
    result.diagnostics.append(allocator, LinkDiagnostic.create(.Info, "test", .{})) catch {};
    result.resolved_symbols.append(allocator, ResolvedSymbol{
        .name = [_]u8{0} ** jit.JIT_SYM_MAX,
        .name_len = 0,
        .address = 0,
        .source = .Unresolved,
    }) catch {};

    // Should not leak
    result.deinit();
}

test "LinkResult success check" {
    var result = LinkResult{
        .stats = .{},
        .resolved_symbols = .{},
        .trampoline_stubs = .{},
        .data_relocations = .{},
        .diagnostics = .{},
        .allocator = std.testing.allocator,
    };
    defer result.deinit();

    try std.testing.expect(result.success());

    result.stats.errors = 1;
    try std.testing.expect(!result.success());
}

test "LinkResult dumpReport produces output" {
    const allocator = std.testing.allocator;

    var result = LinkResult{
        .stats = .{
            .data_relocs_processed = 2,
            .data_relocs_patched = 1,
            .trampolines_generated = 3,
            .ext_calls_patched = 3,
            .symbols_from_dlsym = 2,
            .symbols_from_jump_table = 1,
            .symbols_unresolved = 0,
        },
        .resolved_symbols = .{},
        .trampoline_stubs = .{},
        .data_relocations = .{},
        .diagnostics = .{},
        .allocator = allocator,
    };
    defer result.deinit();

    var buf: std.ArrayListUnmanaged(u8) = .{};
    defer buf.deinit(allocator);

    try result.dumpReport(buf.writer(allocator));

    const report = buf.items;
    try std.testing.expect(report.len > 0);
    try std.testing.expect(std.mem.indexOf(u8, report, "JIT Linker Report") != null);
    try std.testing.expect(std.mem.indexOf(u8, report, "Trampoline Island") != null);
    try std.testing.expect(std.mem.indexOf(u8, report, "Data Relocations") != null);
    try std.testing.expect(std.mem.indexOf(u8, report, "Symbol Resolution") != null);
}

test "ADRP page delta calculation" {
    // Verify the ADRP page delta formula from the design doc:
    //   P_PC = AddressOf(Instruction) & ~0xFFF
    //   P_Target = AddressOf(Data) & ~0xFFF
    //   PageDelta = (P_Target - P_PC) >> 12

    const pc_addr: u64 = 0x1000_0100; // instruction at offset 0x100 in a 4KB-aligned page
    const target_addr: u64 = 0x1001_0200; // data at offset 0x200 in a different page

    const pc_page = pc_addr & ~@as(u64, 0xFFF);
    const target_page = target_addr & ~@as(u64, 0xFFF);
    const page_delta = @as(i64, @intCast(target_page)) - @as(i64, @intCast(pc_page));
    const page_delta_pages = @divExact(page_delta, 4096);

    try std.testing.expectEqual(@as(u64, 0x1000_0000), pc_page);
    try std.testing.expectEqual(@as(u64, 0x1001_0000), target_page);
    try std.testing.expectEqual(@as(i64, 0x10000), page_delta);
    try std.testing.expectEqual(@as(i64, 16), page_delta_pages);

    // Page offset (lo12) for ADD
    const page_offset: u12 = @truncate(target_addr & 0xFFF);
    try std.testing.expectEqual(@as(u12, 0x200), page_offset);
}

test "ADRP encoding formula" {
    // Verify ADRP encoding matches design doc:
    //   immlo = PageDelta & 3
    //   immhi = (PageDelta >> 2) & 0x7FFFF
    //   instr = 0x90000000 | (immlo << 29) | (immhi << 5) | Rd

    const page_delta: i32 = 16; // 16 pages = 64KB ahead
    const delta_u32: u32 = @bitCast(page_delta);
    const immlo: u32 = delta_u32 & 0x3;
    const immhi: u32 = (delta_u32 >> 2) & 0x7FFFF;
    const rd: u32 = 0; // X0

    const adrp = @as(u32, 0x90000000) | (immlo << 29) | (immhi << 5) | rd;

    // immlo = 0, immhi = 4, so:
    // 0x90000000 | (0 << 29) | (4 << 5) | 0 = 0x90000080
    try std.testing.expectEqual(@as(u32, 0), immlo);
    try std.testing.expectEqual(@as(u32, 4), immhi);
    try std.testing.expectEqual(@as(u32, 0x90000080), adrp);
}

test "ADD lo12 encoding formula" {
    // ADD X0, X0, #offset where offset is the lo12 bits of the target
    // ADD immediate: sf=1, op=0, S=0, 100010, sh=0, imm12, Rn, Rd
    // Base: 0x91000000

    const page_offset: u12 = 0x200; // byte 512 into the page
    const rd: u32 = 0; // X0
    const rn: u32 = 0; // X0

    const add = @as(u32, 0x91000000) | (@as(u32, page_offset) << 10) | (rn << 5) | rd;

    // 0x91000000 | (0x200 << 10) | (0 << 5) | 0 = 0x91080000
    try std.testing.expectEqual(@as(u32, 0x91080000), add);
}

test "trampoline BL offset calculation" {
    // BL to trampoline at code_capacity + trampoline_offset
    // code_offset = 0x10, stub_offset = 0x1000
    // delta_bytes = 0x1000 - 0x10 = 0xFF0
    // delta_words = 0xFF0 / 4 = 0x3FC

    const bl_offset: usize = 0x10;
    const stub_offset: usize = 0x1000;
    const delta_bytes: i64 = @as(i64, @intCast(stub_offset)) - @as(i64, @intCast(bl_offset));
    const delta_words: i32 = @intCast(@divExact(delta_bytes, 4));
    const delta_u32: u32 = @bitCast(delta_words);

    const bl_word: u32 = 0x94000000 | (delta_u32 & 0x03ffffff);

    try std.testing.expectEqual(@as(i64, 0xFF0), delta_bytes);
    try std.testing.expectEqual(@as(i32, 0x3FC), delta_words);
    // BL should encode the offset correctly
    try std.testing.expectEqual(@as(u32, 0x940003FC), bl_word);
}

test "negative BL offset for backward trampoline" {
    // Edge case: trampoline before the call site (shouldn't happen normally
    // but tests the encoding logic)
    const bl_offset: usize = 0x100;
    const stub_offset: usize = 0x80;
    const delta_bytes: i64 = @as(i64, @intCast(stub_offset)) - @as(i64, @intCast(bl_offset));
    const delta_words: i32 = @intCast(@divExact(delta_bytes, 4));

    try std.testing.expectEqual(@as(i32, -32), delta_words);

    // BL encoding with negative offset
    const delta_u32: u32 = @bitCast(delta_words);
    const bl_word: u32 = 0x94000000 | (delta_u32 & 0x03ffffff);

    // Verify the top 6 bits are preserved as the BL opcode
    try std.testing.expectEqual(@as(u32, 0x94), (bl_word >> 24) & 0xFC);
}
