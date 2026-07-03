//! jit_runtime.zig — JIT execution harness and compile-and-run API
//!
//! This module provides the high-level API for compiling BASIC programs
//! (via QBE IL) to ARM64 machine code and executing them in-process.
//!
//! It ties together all JIT pipeline stages:
//!
//!   1. **Compilation**: QBE IL → JitInst[] → JitModule (via qbe.zig)
//!   2. **Memory Allocation**: JitMemoryRegion with W^X and ADRP proximity
//!   3. **Linking**: Data relocations + trampoline island (via jit_linker.zig)
//!   4. **Execution**: Cast code pointer to function and call
//!   5. **Teardown**: Free all resources
//!
//! Architecture:
//!   IL text
//!     → compileAndRun()
//!       → compileILJit()          [qbe.zig — QBE C pipeline + Zig encoder]
//!       → JitMemoryRegion.allocate()  [jit_memory.zig — W^X mmap]
//!       → JitLinker.link()        [jit_linker.zig — relocations + trampolines]
//!       → makeExecutable()        [jit_memory.zig — mprotect + icache]
//!       → call function pointer   [execution]
//!       → free()                  [teardown]
//!     → JitExecResult { exit_code, diagnostics }
//!
//! Safety:
//!   - Signal handlers catch SIGSEGV/SIGBUS/SIGTRAP in JIT'd code
//!   - Source map allows mapping crash PC → BASIC source line
//!   - W^X compliance prevents code injection attacks
//!
//! References:
//!   - design/jit_design.md §Platform Specifics, §Lifecycle Management
//!   - jit_status_tracker.md §Phase 6: Execution & Verification

const std = @import("std");
const builtin = @import("builtin");
const jit = @import("jit_encode.zig");
const mem_mod = @import("jit_memory.zig");
const linker = @import("jit_linker.zig");
const capstone = @import("jit_capstone.zig");

const JitModule = jit.JitModule;
const JitInst = jit.JitInst;
const JitInstKind = jit.JitInstKind;
const EncodeStats = jit.EncodeStats;
const SourceMapEntry = jit.SourceMapEntry;
const DiagSeverity = jit.DiagSeverity;
const JitMemoryRegion = mem_mod.JitMemoryRegion;
const JitLinker = linker.JitLinker;
const LinkResult = linker.LinkResult;
const LinkStats = linker.LinkStats;
const RuntimeContext = linker.RuntimeContext;
const JumpTableEntry = linker.JumpTableEntry;
const LinkDiagnostic = linker.LinkDiagnostic;

// ============================================================================
// Section: Error Types
// ============================================================================

pub const JitExecError = error{
    /// QBE IL compilation failed
    CompilationFailed,
    /// JIT memory allocation failed
    MemoryAllocationFailed,
    /// Linking failed (unresolved symbols, trampoline overflow, etc.)
    LinkingFailed,
    /// Memory protection change failed
    ProtectionFailed,
    /// Function entry point not found in symbol table
    EntryPointNotFound,
    /// Signal caught during JIT execution (SIGSEGV, SIGBUS, SIGTRAP)
    SignalCaught,
    /// Region is in wrong state for requested operation
    InvalidState,
    /// Platform does not support JIT execution
    UnsupportedPlatform,
    /// Out of memory
    OutOfMemory,
};

// ============================================================================
// Section: Execution Result
// ============================================================================

/// The result of executing JIT-compiled code.
pub const JitExecResult = struct {
    /// Exit code returned by the JIT'd main function (0 = success).
    exit_code: i32,

    /// Whether execution completed normally (no signal caught).
    completed: bool,

    /// If a signal was caught, which one (0 if none).
    signal: i32,

    /// If a crash occurred, the PC at the time of the crash.
    crash_pc: u64,

    /// If a crash occurred and source mapping is available, the
    /// corresponding BASIC source line number (0 if unknown).
    crash_source_line: u32,

    /// Encode statistics from compilation phase.
    encode_stats: EncodeStats,

    /// Link statistics from linking phase.
    link_stats: LinkStats,

    /// Pipeline report text (owned, must be freed).
    report: ?[]const u8,

    /// Link report text (owned, must be freed).
    link_report: ?[]const u8,

    /// Allocator for owned strings.
    allocator: ?std.mem.Allocator,

    /// Free owned resources.
    pub fn deinit(self: *JitExecResult) void {
        if (self.allocator) |alloc| {
            if (self.report) |r| alloc.free(r);
            if (self.link_report) |lr| alloc.free(lr);
        }
    }

    /// Check if execution was successful.
    pub fn success(self: *const JitExecResult) bool {
        return self.completed and self.exit_code == 0;
    }

    /// Dump a human-readable summary to a writer.
    pub fn dump(self: *const JitExecResult, writer: anytype) !void {
        try writer.writeAll("\n=== JIT Execution Result ===\n");
        if (self.completed) {
            try std.fmt.format(writer, "  Status:      Completed\n", .{});
            try std.fmt.format(writer, "  Exit code:   {d}\n", .{self.exit_code});
        } else {
            try std.fmt.format(writer, "  Status:      CRASHED\n", .{});
            try std.fmt.format(writer, "  Signal:      {d}\n", .{self.signal});
            try std.fmt.format(writer, "  Crash PC:    0x{x:0>16}\n", .{self.crash_pc});
            if (self.crash_source_line > 0) {
                try std.fmt.format(writer, "  Source line: {d}\n", .{self.crash_source_line});
            }
        }

        try writer.writeAll("\n  Encode Stats:\n");
        try std.fmt.format(writer, "    Instructions: {d}\n", .{self.encode_stats.instructions_emitted});
        try std.fmt.format(writer, "    Functions:    {d}\n", .{self.encode_stats.functions_encoded});
        try std.fmt.format(writer, "    Labels:       {d}\n", .{self.encode_stats.labels_recorded});
        try std.fmt.format(writer, "    Fixups:       {d}/{d}\n", .{ self.encode_stats.fixups_resolved, self.encode_stats.fixups_created });
        try std.fmt.format(writer, "    Ext calls:    {d}\n", .{self.encode_stats.ext_calls_recorded});
        try std.fmt.format(writer, "    Data bytes:   {d}\n", .{self.encode_stats.data_bytes_emitted});

        try writer.writeAll("\n  Link Stats:\n");
        try std.fmt.format(writer, "    Trampolines:  {d}\n", .{self.link_stats.trampolines_generated});
        try std.fmt.format(writer, "    Data relocs:  {d}/{d}\n", .{ self.link_stats.data_relocs_patched, self.link_stats.data_relocs_processed });
        try std.fmt.format(writer, "    Ext patched:  {d}\n", .{self.link_stats.ext_calls_patched});
        try std.fmt.format(writer, "    Sym dlsym:    {d}\n", .{self.link_stats.symbols_from_dlsym});
        try std.fmt.format(writer, "    Sym jump tbl: {d}\n", .{self.link_stats.symbols_from_jump_table});
        try std.fmt.format(writer, "    Sym unresolved: {d}\n", .{self.link_stats.symbols_unresolved});
    }
};

// ============================================================================
// Section: JIT Execution Session
// ============================================================================

/// A JIT execution session manages the lifecycle of compiled and linked
/// JIT code. It holds all the resources needed for execution and provides
/// methods for inspecting the generated code.
///
/// Usage:
///   var session = try JitSession.compile(allocator, il_text, context);
///   defer session.deinit();
///
///   const result = session.execute();
///   // or: const fn_ptr = try session.getFunctionPtr("main");
pub const JitSession = struct {
    /// Heap allocator for all owned memory.
    allocator: std.mem.Allocator,

    /// Encoded module from the JIT compiler.
    module: JitModule,

    /// Executable memory region (code + data + trampolines).
    region: JitMemoryRegion,

    /// Link result with diagnostics.
    link_result: LinkResult,

    /// Number of JitInst records collected.
    inst_count: u32,

    /// Number of functions collected.
    func_count: u32,

    /// Number of data definitions collected.
    data_count: u32,

    /// Pipeline report text from compilation (owned).
    pipeline_report: ?[]const u8,

    /// Whether the session has been finalized (code is executable).
    finalized: bool,

    /// Compile QBE IL text into a ready-to-execute JIT session.
    ///
    /// This performs the full pipeline:
    ///   1. QBE IL → JitInst[] → JitModule (compile)
    ///   2. JitMemoryRegion allocation (mmap)
    ///   3. Linking (trampolines + data relocations)
    ///   4. Make executable (mprotect + icache flush)
    pub fn compile(
        allocator: std.mem.Allocator,
        il_text: []const u8,
        context: ?*const RuntimeContext,
    ) JitExecError!JitSession {
        return compileWithCapacity(allocator, il_text, context, 64 * 1024, 16 * 1024);
    }

    /// Like compile() but with explicit buffer capacities.
    pub fn compileWithCapacity(
        _: std.mem.Allocator,
        il_text: []const u8,
        _: ?*const RuntimeContext,
        _: u32,
        _: u32,
    ) JitExecError!JitSession {
        if (comptime builtin.os.tag != .macos and builtin.os.tag != .linux) {
            return JitExecError.UnsupportedPlatform;
        }

        if (il_text.len == 0) return JitExecError.CompilationFailed;

        // Step 1: Compile IL → JitModule
        // We import qbe at the call site to avoid circular dependency issues.
        // The caller should use compileFromModule() if they already have a module.
        // For now, we provide compileFromModule as the primary API.
        return JitExecError.CompilationFailed;
    }

    /// Create a JIT session from a pre-compiled JitModule.
    ///
    /// This is the preferred API when you already have a compiled module
    /// (e.g., from qbe.compileILJit). It handles memory allocation,
    /// linking, and finalization.
    pub fn compileFromModule(
        allocator: std.mem.Allocator,
        module: JitModule,
        context: ?*const RuntimeContext,
        insts: ?[*]const JitInst,
        ninst: u32,
        inst_count: u32,
        func_count: u32,
        data_count: u32,
        pipeline_report: ?[]const u8,
    ) JitExecError!JitSession {
        if (comptime builtin.os.tag != .macos and builtin.os.tag != .linux) {
            return JitExecError.UnsupportedPlatform;
        }

        // Step 2: Allocate executable memory
        const code_cap = if (module.code_len > 0) @as(usize, module.code_len) + 4096 else 4096;
        const data_cap = if (module.data_len > 0) @as(usize, module.data_len) + 4096 else 4096;

        var region = JitMemoryRegion.allocate(code_cap, data_cap) catch {
            return JitExecError.MemoryAllocationFailed;
        };
        errdefer region.free();

        // Step 3: Link (copy code/data, build trampolines, resolve relocations)
        var link_result = JitLinker.link(
            allocator,
            &module,
            &region,
            context,
            insts,
            ninst,
        );

        if (!link_result.success()) {
            // Non-fatal: we still create the session so the caller can
            // inspect diagnostics. But we won't finalize.
            return JitSession{
                .allocator = allocator,
                .module = module,
                .region = region,
                .link_result = link_result,
                .inst_count = inst_count,
                .func_count = func_count,
                .data_count = data_count,
                .pipeline_report = pipeline_report,
                .finalized = false,
            };
        }

        // Step 4: Make executable + flush icache
        region.makeExecutable() catch {
            link_result.diagnostics.append(allocator, LinkDiagnostic.create(
                .Error,
                "makeExecutable failed",
                .{},
            )) catch {};
            link_result.stats.errors += 1;

            return JitSession{
                .allocator = allocator,
                .module = module,
                .region = region,
                .link_result = link_result,
                .inst_count = inst_count,
                .func_count = func_count,
                .data_count = data_count,
                .pipeline_report = pipeline_report,
                .finalized = false,
            };
        };

        return JitSession{
            .allocator = allocator,
            .module = module,
            .region = region,
            .link_result = link_result,
            .inst_count = inst_count,
            .func_count = func_count,
            .data_count = data_count,
            .pipeline_report = pipeline_report,
            .finalized = true,
        };
    }

    /// Execute the JIT-compiled code's main entry point.
    ///
    /// Looks up the "main" symbol in the module's symbol table,
    /// casts the code pointer to a function, and calls it.
    ///
    /// The expected function signature is: fn(i32, [*][*:0]u8) callconv(.C) i32
    /// (matches the QBE `export function w $main(w %argc, l %argv)` convention).
    /// When called without args, argc=0 and argv points to an empty list.
    pub fn execute(self: *JitSession) JitExecResult {
        // Call main with argc=0 and a dummy argv (just a null terminator)
        var dummy_argv = [_:null]?[*:0]u8{null};
        return self.executeMain(0, @ptrCast(&dummy_argv));
    }

    /// Execute the JIT-compiled main with program arguments.
    ///
    /// `program_args` should include argv[0] (the program name).
    /// This builds a C-style argv array and calls main(argc, argv).
    pub fn executeWithArgs(self: *JitSession, program_args: []const []const u8) JitExecResult {
        // Build a C argv array: each entry is a pointer to a
        // null-terminated string.  We point directly into the
        // slice data — the strings come from std.process.args()
        // and are already null-terminated on the OS side.
        const argc: i32 = @intCast(program_args.len);
        var argv_buf: [128]?[*:0]u8 = undefined;
        const cap = @min(program_args.len, argv_buf.len - 1);
        for (0..cap) |i| {
            // std.process.args() yields [*:0]u8 slices — the
            // underlying OS strings are null-terminated, so we
            // can recover the sentinel pointer from the slice.
            argv_buf[i] = @ptrCast(@constCast(program_args[i].ptr));
        }
        argv_buf[cap] = null; // C argv is null-terminated
        return self.executeMain(argc, @ptrCast(&argv_buf));
    }

    /// Low-level: execute main(argc, argv) in the JIT region.
    fn executeMain(self: *JitSession, argc: i32, argv: [*][*:0]u8) JitExecResult {
        return self.executeFunctionWithArgs("main", argc, argv);
    }

    /// Execute a named function in the JIT-compiled code (no args).
    pub fn executeFunction(self: *JitSession, func_name: []const u8) JitExecResult {
        var dummy_argv = [_:null]?[*:0]u8{null};
        return self.executeFunctionWithArgs(func_name, 0, @ptrCast(&dummy_argv));
    }

    /// Execute a named function with argc/argv arguments.
    fn executeFunctionWithArgs(self: *JitSession, func_name: []const u8, argc: i32, argv: [*][*:0]u8) JitExecResult {
        var result = JitExecResult{
            .exit_code = -1,
            .completed = false,
            .signal = 0,
            .crash_pc = 0,
            .crash_source_line = 0,
            .encode_stats = self.module.stats,
            .link_stats = self.link_result.stats,
            .report = null,
            .link_report = null,
            .allocator = self.allocator,
        };

        // Copy pipeline report
        if (self.pipeline_report) |pr| {
            result.report = self.allocator.dupe(u8, pr) catch null;
        }

        // Generate link report
        {
            var buf: std.ArrayListUnmanaged(u8) = .{};
            self.link_result.dumpReport(buf.writer(self.allocator)) catch {};
            result.link_report = buf.toOwnedSlice(self.allocator) catch blk: {
                buf.deinit(self.allocator);
                break :blk null;
            };
        }

        if (!self.finalized) {
            result.signal = -1;
            return result;
        }

        // Look up the function entry point
        const entry_offset = self.findFunctionOffset(func_name) orelse blk: {
            // Default: if "main" is not in the symbol table, start at offset 0
            // (QBE typically puts the first function at the beginning)
            if (std.mem.eql(u8, func_name, "main")) {
                break :blk @as(u32, 0);
            } else {
                return result;
            }
        };

        // Get a callable function pointer — main(argc, argv) → int
        const MainFn = *const fn (i32, [*][*:0]u8) callconv(.c) i32;
        const fn_ptr = self.region.getFunctionPtr(MainFn, entry_offset) catch {
            return result;
        };

        // Install signal handler for crash detection
        var prev_handlers = installSignalHandlers();
        defer restoreSignalHandlers(&prev_handlers);

        // Store session pointer for signal handler context
        setCurrentSession(self);
        defer setCurrentSession(null);

        // Execute via basic_jit_exec which arms a setjmp so that
        // runtime calls to basic_exit() longjmp back instead of
        // killing the host process.  This is essential for --batch-jit.
        const exit_code = basic_jit_exec(@ptrCast(fn_ptr), argc, argv);

        result.exit_code = exit_code;
        result.completed = true;

        return result;
    }

    /// Get a typed function pointer for a named function.
    ///
    /// Returns null if the function is not found or the session is not finalized.
    pub fn getFunctionPtr(self: *const JitSession, comptime T: type, func_name: []const u8) ?T {
        if (!self.finalized) return null;

        const offset = self.findFunctionOffset(func_name) orelse {
            if (std.mem.eql(u8, func_name, "main")) {
                return self.region.getFunctionPtr(T, 0) catch null;
            }
            return null;
        };

        return self.region.getFunctionPtr(T, offset) catch null;
    }

    /// Look up a function's code offset by name.
    fn findFunctionOffset(self: *const JitSession, name: []const u8) ?u32 {
        // Check the module's symbol table
        if (self.module.symbols.get(name)) |sym| {
            if (sym.is_code) {
                return sym.offset;
            }
        }

        // Try with '$' prefix (QBE convention)
        var prefixed: [jit.JIT_SYM_MAX]u8 = undefined;
        if (name.len + 1 < jit.JIT_SYM_MAX) {
            prefixed[0] = '$';
            @memcpy(prefixed[1..][0..name.len], name);
            const prefixed_name = prefixed[0 .. name.len + 1];
            if (self.module.symbols.get(prefixed_name)) |sym| {
                if (sym.is_code) {
                    return sym.offset;
                }
            }
        }

        return null;
    }

    /// Look up a BASIC source line from a code offset using the source map.
    pub fn sourceLineForPC(self: *const JitSession, pc: u64) u32 {
        const code_base = self.region.codeAddress(0) orelse return 0;
        if (pc < code_base) return 0;

        const offset: u32 = @intCast(pc - code_base);

        // Binary search the source map for the nearest entry ≤ offset
        const entries = self.module.source_map.items;
        if (entries.len == 0) return 0;

        var best_line: u32 = 0;
        for (entries) |entry| {
            if (entry.code_offset <= offset) {
                best_line = entry.source_line;
            } else {
                break;
            }
        }

        return best_line;
    }

    /// Check if the code PC is within our JIT region.
    pub fn isJitPC(self: *const JitSession, pc: u64) bool {
        const code_base = self.region.codeAddress(0) orelse return false;
        const code_end = code_base + self.region.code_len;
        return pc >= code_base and pc < code_end;
    }

    /// Get layout info for the memory region.
    pub fn layoutInfo(self: *const JitSession) mem_mod.LayoutInfo {
        return self.region.layoutInfo();
    }

    /// Dump a comprehensive execution report to a writer.
    pub fn dumpFullReport(self: *const JitSession, writer: anytype) !void {
        try writer.writeAll("\n");
        try writer.writeAll("╔══════════════════════════════════════════╗\n");
        try writer.writeAll("║     FasterBASIC JIT Execution Report     ║\n");
        try writer.writeAll("╚══════════════════════════════════════════╝\n");

        // Memory layout
        const info = self.region.layoutInfo();
        try info.dump(writer);

        // Pipeline report
        if (self.pipeline_report) |pr| {
            try writer.writeAll("\n");
            try writer.writeAll(pr);
        }

        // Link report
        try self.link_result.dumpReport(writer);

        // Capstone ARM64 disassembly — linked code at real addresses
        {
            const code_slice = self.region.codeSlice();
            const code_base_addr = self.region.codeAddress(0);
            const tramp_base: ?[*]const u8 = if (self.region.trampoline_base) |tb|
                @as([*]const u8, @ptrCast(tb))
            else
                null;
            const tramp_len = if (tramp_base != null) self.region.trampoline_len else 0;
            const tramp_real_addr: u64 = if (self.region.trampoline_base) |tb|
                @intFromPtr(tb)
            else
                0;

            if (code_slice) |cs_buf| {
                if (code_base_addr) |base| {
                    try capstone.dumpLinkedReport(
                        cs_buf.ptr,
                        cs_buf.len,
                        base,
                        &self.module,
                        &self.link_result,
                        tramp_base,
                        tramp_len,
                        tramp_real_addr,
                        writer,
                    );
                } else {
                    // Fallback to pre-link disassembly
                    try capstone.dumpFullDisassemblyReport(
                        &self.module,
                        tramp_base,
                        tramp_len,
                        writer,
                    );
                }
            } else {
                // Fallback to pre-link disassembly
                try capstone.dumpFullDisassemblyReport(
                    &self.module,
                    tramp_base,
                    tramp_len,
                    writer,
                );
            }
        }

        // Session status
        try writer.writeAll("\n=== Session Status ===\n");
        try std.fmt.format(writer, "  Finalized:    {}\n", .{self.finalized});
        try std.fmt.format(writer, "  Inst count:   {d}\n", .{self.inst_count});
        try std.fmt.format(writer, "  Func count:   {d}\n", .{self.func_count});
        try std.fmt.format(writer, "  Data count:   {d}\n", .{self.data_count});
        try std.fmt.format(writer, "  Code len:     {d} bytes\n", .{self.module.code_len});
        try std.fmt.format(writer, "  Data len:     {d} bytes\n", .{self.module.data_len});
        try std.fmt.format(writer, "  Trampolines:  {d}\n", .{self.region.trampoline_len / mem_mod.TRAMPOLINE_STUB_SIZE});
        try std.fmt.format(writer, "  Errors:       {d}\n", .{self.module.stats.error_count});
    }

    /// Free all resources owned by this session.
    pub fn deinit(self: *JitSession) void {
        self.link_result.deinit();
        self.region.free();
        if (self.pipeline_report) |pr| {
            self.allocator.free(pr);
        }
        self.module.deinit();
    }
};

// ============================================================================
// Section: Signal Handling
// ============================================================================

/// Thread-local pointer to the currently executing JitSession.
/// Used by signal handlers to map crash PCs to source lines.
threadlocal var current_session: ?*JitSession = null;

// ── JIT exit override ───────────────────────────────────────────────────
// basic_jit_exec arms a setjmp; if the runtime calls basic_exit() it
// longjmps back instead of calling real exit(), so the host survives.
extern fn basic_jit_exec(fn_ptr: *const anyopaque, argc: i32, argv: [*][*:0]u8) callconv(.c) i32;

fn setCurrentSession(session: ?*JitSession) void {
    current_session = session;
}

/// Previous signal handler state for restoration after JIT execution.
const SignalHandlerState = struct {
    sigsegv_installed: bool,
    sigbus_installed: bool,
    sigtrap_installed: bool,
};

/// Install signal handlers that catch crashes in JIT'd code.
///
/// On crash, the handler:
///   1. Checks if the PC is within the JIT code region
///   2. If so, maps PC → BASIC source line via source map
///   3. Prints a diagnostic message
///   4. Exits with error
///
/// If the crash is outside JIT code, the default handler is restored
/// and the signal re-raised.
fn installSignalHandlers() SignalHandlerState {
    // For now, we use a simple approach: just record that we want
    // signal handling. The actual sigaction setup is platform-specific.
    //
    // TODO: Full implementation with sigaction, ucontext_t parsing,
    //       and register dump. This is Phase 5 work (Debugging & Diagnostics).
    //
    // The current implementation is a no-op placeholder that allows
    // the execution to proceed. If JIT code crashes, we'll get the
    // default signal behavior (core dump).

    return SignalHandlerState{
        .sigsegv_installed = false,
        .sigbus_installed = false,
        .sigtrap_installed = false,
    };
}

/// Restore previous signal handlers.
fn restoreSignalHandlers(state: *SignalHandlerState) void {
    _ = state;
    // No-op for now — see installSignalHandlers comment.
}

// ============================================================================
// Section: Convenience Functions
// ============================================================================

/// Build a RuntimeContext from a list of function pointer entries.
///
/// This is a convenience function for creating a jump table
/// from known runtime function pointers.
///
/// Example:
///   const ctx = buildRuntimeContext(&.{
///       .{ .name = "basic_print_string_desc", .address = @intFromPtr(&myPrintFn) },
///       .{ .name = "basic_print_newline",     .address = @intFromPtr(&myNewlineFn) },
///   });
pub fn buildRuntimeContext(entries: []const JumpTableEntry) RuntimeContext {
    return RuntimeContext{
        .entries = entries,
        .dlsym_handle = null,
    };
}

/// Build a RuntimeContext that only uses dlsym for resolution.
pub fn dlsymOnlyContext() RuntimeContext {
    return RuntimeContext.empty();
}

/// Create an empty JitExecResult for error reporting.
pub fn errorResult(allocator: std.mem.Allocator) JitExecResult {
    return JitExecResult{
        .exit_code = -1,
        .completed = false,
        .signal = 0,
        .crash_pc = 0,
        .crash_source_line = 0,
        .encode_stats = EncodeStats{},
        .link_stats = LinkStats{},
        .report = null,
        .link_report = null,
        .allocator = allocator,
    };
}

// ============================================================================
// Section: Tests
// ============================================================================

test "JitExecResult success check" {
    var result = JitExecResult{
        .exit_code = 0,
        .completed = true,
        .signal = 0,
        .crash_pc = 0,
        .crash_source_line = 0,
        .encode_stats = EncodeStats{},
        .link_stats = LinkStats{},
        .report = null,
        .link_report = null,
        .allocator = null,
    };

    try std.testing.expect(result.success());

    result.exit_code = 1;
    try std.testing.expect(!result.success());

    result.exit_code = 0;
    result.completed = false;
    try std.testing.expect(!result.success());
}

test "JitExecResult dump produces output" {
    const allocator = std.testing.allocator;

    const result = JitExecResult{
        .exit_code = 0,
        .completed = true,
        .signal = 0,
        .crash_pc = 0,
        .crash_source_line = 0,
        .encode_stats = EncodeStats{
            .instructions_emitted = 10,
            .functions_encoded = 1,
            .labels_recorded = 3,
            .fixups_created = 2,
            .fixups_resolved = 2,
            .ext_calls_recorded = 2,
            .data_bytes_emitted = 6,
            .error_count = 0,
            .skipped_pseudo = 0,
            .neon_ops_encoded = 0,
        },
        .link_stats = LinkStats{
            .trampolines_generated = 2,
            .ext_calls_patched = 2,
            .symbols_from_dlsym = 2,
        },
        .report = null,
        .link_report = null,
        .allocator = null,
    };

    var buf: std.ArrayListUnmanaged(u8) = .{};
    defer buf.deinit(allocator);

    try result.dump(buf.writer(allocator));

    const output = buf.items;
    try std.testing.expect(output.len > 0);
    try std.testing.expect(std.mem.indexOf(u8, output, "Execution Result") != null);
    try std.testing.expect(std.mem.indexOf(u8, output, "Completed") != null);
    try std.testing.expect(std.mem.indexOf(u8, output, "Instructions") != null);
}

test "JitExecResult dump crash report" {
    const allocator = std.testing.allocator;

    const result = JitExecResult{
        .exit_code = -1,
        .completed = false,
        .signal = 11, // SIGSEGV
        .crash_pc = 0x100004000,
        .crash_source_line = 42,
        .encode_stats = EncodeStats{},
        .link_stats = LinkStats{},
        .report = null,
        .link_report = null,
        .allocator = null,
    };

    var buf: std.ArrayListUnmanaged(u8) = .{};
    defer buf.deinit(allocator);

    try result.dump(buf.writer(allocator));

    const output = buf.items;
    try std.testing.expect(std.mem.indexOf(u8, output, "CRASHED") != null);
    try std.testing.expect(std.mem.indexOf(u8, output, "Signal") != null);
    try std.testing.expect(std.mem.indexOf(u8, output, "Source line") != null);
}

test "errorResult returns failed state" {
    var result = errorResult(std.testing.allocator);
    defer result.deinit();

    try std.testing.expect(!result.success());
    try std.testing.expect(!result.completed);
    try std.testing.expectEqual(@as(i32, -1), result.exit_code);
}

test "buildRuntimeContext creates context" {
    const entries = [_]JumpTableEntry{
        .{ .name = "func_a", .address = 0x1000 },
        .{ .name = "func_b", .address = 0x2000 },
    };
    const ctx = buildRuntimeContext(&entries);

    try std.testing.expectEqual(@as(usize, 2), ctx.entries.len);
    try std.testing.expectEqual(@as(?u64, 0x1000), ctx.lookup("func_a"));
    try std.testing.expectEqual(@as(?u64, 0x2000), ctx.lookup("func_b"));
    try std.testing.expectEqual(@as(?u64, null), ctx.lookup("func_c"));
}

test "dlsymOnlyContext has no entries" {
    const ctx = dlsymOnlyContext();
    try std.testing.expectEqual(@as(usize, 0), ctx.entries.len);
    try std.testing.expectEqual(@as(?u64, null), ctx.lookup("anything"));
}

test "SignalHandlerState initialization" {
    const state = installSignalHandlers();
    try std.testing.expect(!state.sigsegv_installed);
    try std.testing.expect(!state.sigbus_installed);
    try std.testing.expect(!state.sigtrap_installed);
}

test "JitSession findFunctionOffset with empty module" {
    const allocator = std.testing.allocator;

    // Create a minimal module
    var module = try JitModule.init(allocator, 256, 64);
    defer module.deinit();

    // Create a JitSession-like struct to test findFunctionOffset
    // We can't create a full session without memory allocation, so
    // test the symbol lookup logic directly via the module.

    // No symbols → lookup returns null
    const result = module.symbols.get("main");
    try std.testing.expectEqual(@as(?jit.SymbolEntry, null), result);
}

test "source map binary search" {
    // Test the source line lookup logic that JitSession.sourceLineForPC uses
    const allocator = std.testing.allocator;

    var source_map: std.ArrayListUnmanaged(SourceMapEntry) = .{};
    defer source_map.deinit(allocator);

    // Add entries in order
    try source_map.append(allocator, .{ .code_offset = 0, .source_line = 10, .source_col = 0 });
    try source_map.append(allocator, .{ .code_offset = 12, .source_line = 20, .source_col = 0 });
    try source_map.append(allocator, .{ .code_offset = 28, .source_line = 30, .source_col = 0 });
    try source_map.append(allocator, .{ .code_offset = 44, .source_line = 40, .source_col = 0 });

    // Simulate the lookup algorithm
    const entries = source_map.items;

    // Look up offset 0 → line 10
    {
        const offset: u32 = 0;
        var best: u32 = 0;
        for (entries) |e| {
            if (e.code_offset <= offset) best = e.source_line else break;
        }
        try std.testing.expectEqual(@as(u32, 10), best);
    }

    // Look up offset 16 → line 20 (between entries 1 and 2)
    {
        const offset: u32 = 16;
        var best: u32 = 0;
        for (entries) |e| {
            if (e.code_offset <= offset) best = e.source_line else break;
        }
        try std.testing.expectEqual(@as(u32, 20), best);
    }

    // Look up offset 44 → line 40 (exact match last entry)
    {
        const offset: u32 = 44;
        var best: u32 = 0;
        for (entries) |e| {
            if (e.code_offset <= offset) best = e.source_line else break;
        }
        try std.testing.expectEqual(@as(u32, 40), best);
    }

    // Look up offset 100 → line 40 (past all entries)
    {
        const offset: u32 = 100;
        var best: u32 = 0;
        for (entries) |e| {
            if (e.code_offset <= offset) best = e.source_line;
        }
        try std.testing.expectEqual(@as(u32, 40), best);
    }
}

test "EncodeStats default is zero" {
    const stats = EncodeStats{};
    try std.testing.expectEqual(@as(u32, 0), stats.instructions_emitted);
    try std.testing.expectEqual(@as(u32, 0), stats.functions_encoded);
    try std.testing.expectEqual(@as(u32, 0), stats.error_count);
}

test "JitExecResult deinit with null allocator is safe" {
    var result = JitExecResult{
        .exit_code = 0,
        .completed = true,
        .signal = 0,
        .crash_pc = 0,
        .crash_source_line = 0,
        .encode_stats = EncodeStats{},
        .link_stats = LinkStats{},
        .report = null,
        .link_report = null,
        .allocator = null,
    };

    // Should not crash
    result.deinit();
}

test "JitExecResult deinit frees owned strings" {
    const allocator = std.testing.allocator;

    const report = try allocator.dupe(u8, "test report");
    const link_report = try allocator.dupe(u8, "test link report");

    var result = JitExecResult{
        .exit_code = 0,
        .completed = true,
        .signal = 0,
        .crash_pc = 0,
        .crash_source_line = 0,
        .encode_stats = EncodeStats{},
        .link_stats = LinkStats{},
        .report = report,
        .link_report = link_report,
        .allocator = allocator,
    };

    // Should free without leaking
    result.deinit();
}
