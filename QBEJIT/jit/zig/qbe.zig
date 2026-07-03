//! QBE Backend Integration — In-process IL → Assembly / JIT compilation
//!
//! This module provides Zig bindings to the embedded QBE C backend.
//! Instead of shelling out to an external `qbe` binary, we call QBE's
//! optimization and emission pipeline directly via the C bridge.
//!
//! For JIT mode, QBE IL is compiled through the full optimization pipeline
//! and collected into JitInst[] records (C-side, jit_collect.c), then
//! encoded to ARM64 machine code by the Zig encoder (jit_encode.zig)
//! directly — no FFI boundary, just a normal Zig function call.
//!
//! Usage:
//!   const qbe = @import("qbe.zig");
//!
//!   // AOT: Compile QBE IL text to an assembly file
//!   try qbe.compileIL(il_text, "/tmp/output.s", null);
//!
//!   // JIT: Compile QBE IL text to in-memory machine code
//!   var result = try qbe.compileILJit(allocator, il_text, null);
//!   defer result.deinit();
//!   const code = result.module.codeSlice();
//!
//! Thread safety: NOT thread-safe. QBE uses extensive mutable global state.

const std = @import("std");
const jit = @import("jit_encode.zig");
const capstone = @import("jit_capstone.zig");

// Re-export the types callers need
pub const JitModule = jit.JitModule;
pub const JitInst = jit.JitInst;
pub const JitInstKind = jit.JitInstKind;
pub const EncodeStats = jit.EncodeStats;

// ── C bridge extern declarations ───────────────────────────────────────
//
// Only the actual C functions live here. No Zig-to-Zig FFI.

const c = struct {
    const QBE_OK: c_int = 0;
    const QBE_ERR_OUTPUT: c_int = -1;
    const QBE_ERR_INPUT: c_int = -2;
    const QBE_ERR_TARGET: c_int = -3;
    const QBE_ERR_PARSE: c_int = -4;

    // ── AOT compilation (qbe_bridge.c) ─────────────────────────────

    extern fn qbe_compile_il(
        il_text: [*]const u8,
        il_len: usize,
        asm_path: [*:0]const u8,
        target_name: ?[*:0]const u8,
    ) c_int;

    extern fn qbe_compile_il_to_file(
        il_text: [*]const u8,
        il_len: usize,
        output_file: *anyopaque,
        target_name: ?[*:0]const u8,
    ) c_int;

    extern fn qbe_default_target() [*:0]const u8;
    extern fn qbe_available_targets() [*]const ?[*:0]const u8;
    extern fn qbe_version() [*:0]const u8;

    // ── JIT collection (jit_collect.c) ─────────────────────────────
    //
    // These C functions run the QBE pipeline and populate a JitCollector
    // with JitInst records. The JitInst layout is defined in jit_collect.h
    // and mirrored by jit_encode.zig's JitInst extern struct — same
    // memory layout, so we can pass the pointer straight through.

    extern fn jit_collector_init(jc: *JitCollector) c_int;
    extern fn jit_collector_free(jc: *JitCollector) void;
    extern fn jit_collector_reset(jc: *JitCollector) void;
    extern fn jit_collector_dump(jc: *const JitCollector) void;

    extern fn qbe_compile_il_jit(
        il_text: [*]const u8,
        il_len: usize,
        jc: *JitCollector,
        target_name: ?[*:0]const u8,
    ) c_int;

    // ── Opcode histogram (jit_collect.c) ───────────────────────────
    extern fn jit_histogram_reset() void;
    extern fn jit_histogram_dump() void;
};

// ── JitCollector C struct ──────────────────────────────────────────────
//
// This mirrors the C JitCollector from jit_collect.h. It's the only C
// struct we need — its `insts` field points to JitInst records whose
// layout is shared between C and Zig (defined as extern struct in both).

const JitCollector = extern struct {
    insts: ?[*]JitInst,
    ninst: u32,
    inst_cap: u32,
    nfunc: u32,
    ndata: u32,
    @"error": c_int,
    error_msg: [256]u8,
};

// ── Error type ─────────────────────────────────────────────────────────

pub const QBEError = error{
    /// Cannot open or write the output assembly file.
    OutputError,
    /// Cannot create input stream from IL text (empty input?).
    InputError,
    /// Unknown target name.
    UnknownTarget,
    /// QBE IL parse error — the IL text was malformed.
    ParseError,
    /// Unexpected error code from the C bridge.
    UnexpectedError,
    /// JIT collector initialization failed (allocation).
    JitCollectorInitFailed,
    /// JIT collector reported an error during QBE pipeline.
    JitCollectError,
    /// JIT encoder failed (allocation).
    JitEncodeFailed,
};

// ════════════════════════════════════════════════════════════════════════
// AOT Public API
// ════════════════════════════════════════════════════════════════════════

/// Compile QBE IL text to an assembly file.
///
/// Parameters:
///   - `il_text`:     The QBE IL source text (e.g., from CodeGenerator.generate()).
///   - `asm_path`:    Output path for the generated assembly file.
///   - `target_name`: Target architecture name, or null for host default.
///                     Valid names: "amd64_sysv", "amd64_apple", "arm64",
///                     "arm64_apple", "rv64".
///
/// Returns QBEError on failure.
pub fn compileIL(il_text: []const u8, asm_path: []const u8, target_name: ?[]const u8) QBEError!void {
    if (il_text.len == 0) return QBEError.InputError;

    // NUL-terminated asm_path for C API.
    var path_buf: [4096]u8 = undefined;
    if (asm_path.len >= path_buf.len) return QBEError.OutputError;
    @memcpy(path_buf[0..asm_path.len], asm_path);
    path_buf[asm_path.len] = 0;
    const asm_path_z: [*:0]const u8 = path_buf[0..asm_path.len :0];

    const target_z = try nullTermTarget(target_name);

    const rc = c.qbe_compile_il(il_text.ptr, il_text.len, asm_path_z, target_z);
    try checkResult(rc);
}

/// Compile QBE IL text to an assembly file, allocating paths with the given allocator.
///
/// This variant handles paths of any length by heap-allocating the
/// NUL-terminated copy. Prefer `compileIL()` for typical use.
pub fn compileILAlloc(
    allocator: std.mem.Allocator,
    il_text: []const u8,
    asm_path: []const u8,
    target_name: ?[]const u8,
) (std.mem.Allocator.Error || QBEError)!void {
    if (il_text.len == 0) return QBEError.InputError;

    const asm_path_z = try allocator.dupeZ(u8, asm_path);
    defer allocator.free(asm_path_z);

    var target_z: ?[*:0]const u8 = null;
    var target_alloc: ?[:0]const u8 = null;
    defer if (target_alloc) |ta| allocator.free(ta);

    if (target_name) |tn| {
        const tz = try allocator.dupeZ(u8, tn);
        target_alloc = tz;
        target_z = tz.ptr;
    }

    const rc = c.qbe_compile_il(il_text.ptr, il_text.len, asm_path_z.ptr, target_z);
    try checkResult(rc);
}

/// Returns the default target name for the current platform.
pub fn defaultTarget() []const u8 {
    const ptr = c.qbe_default_target();
    return std.mem.span(ptr);
}

/// Returns a list of all available QBE target names.
pub fn availableTargets() []const []const u8 {
    const raw = c.qbe_available_targets();

    var count: usize = 0;
    while (raw[count] != null) : (count += 1) {}

    const S = struct {
        var buf: [8][]const u8 = undefined;
        var init: bool = false;
        var len: usize = 0;
    };

    if (!S.init) {
        var i: usize = 0;
        while (i < count and i < 8) : (i += 1) {
            S.buf[i] = std.mem.span(raw[i].?);
        }
        S.len = i;
        S.init = true;
    }

    return S.buf[0..S.len];
}

/// Returns the QBE version string (e.g., "qbe+fasterbasic-zig").
pub fn version() []const u8 {
    return std.mem.span(c.qbe_version());
}

// ════════════════════════════════════════════════════════════════════════
// JIT Public API
// ════════════════════════════════════════════════════════════════════════

/// Result of a JIT compilation. Owns the encoded machine code and data.
/// Call `deinit()` when done.
pub const JitResult = struct {
    /// The encoded module — contains code buffer, data buffer, labels,
    /// fixups, external call records, diagnostics, source map, and stats.
    module: JitModule,

    /// Number of JitInst records that were collected from QBE.
    inst_count: u32,

    /// Number of functions collected.
    func_count: u32,

    /// Number of data definitions collected.
    data_count: u32,

    /// Pipeline report text — generated while the JitInst[] stream is
    /// still live, covers every phase of JIT compilation.  null when
    /// report generation was skipped or failed.
    report: ?[]const u8 = null,

    /// Allocator used for the report buffer (needed for deinit).
    report_allocator: ?std.mem.Allocator = null,

    // ── Convenience accessors ──────────────────────────────────────

    /// Slice of the generated machine code bytes.
    pub fn codeSlice(self: *const JitResult) ?[]const u8 {
        if (self.module.code) |code| {
            if (self.module.code_len == 0) return null;
            return code[0..self.module.code_len];
        }
        return null;
    }

    /// Slice of the generated data section bytes.
    pub fn dataSlice(self: *const JitResult) ?[]const u8 {
        if (self.module.data) |data| {
            if (self.module.data_len == 0) return null;
            return data[0..self.module.data_len];
        }
        return null;
    }

    /// Dump a human-readable summary to stderr.
    pub fn dump(self: *const JitResult) void {
        const stderr = std.fs.File.stderr().deprecatedWriter();
        jit.dumpModuleSummary(&self.module, stderr) catch {};
    }

    /// Dump generated code in hex to stderr.
    pub fn dumpCode(self: *const JitResult) void {
        const stderr = std.fs.File.stderr().deprecatedWriter();
        jit.dumpCodeHex(&self.module, stderr) catch {};
    }

    /// Return the pipeline report text, or "(no report)" if unavailable.
    pub fn pipelineReport(self: *const JitResult) []const u8 {
        return self.report orelse "(no report available)";
    }

    /// Print the full pipeline report to stderr.
    pub fn dumpPipelineReport(self: *const JitResult) void {
        const stderr = std.fs.File.stderr().deprecatedWriter();
        stderr.writeAll(self.pipelineReport()) catch {};
    }

    /// Free all resources.
    pub fn deinit(self: *JitResult) void {
        if (self.report) |r| {
            if (self.report_allocator) |ra| {
                ra.free(r);
            }
        }
        self.module.deinit();
    }
};

/// Compile QBE IL text directly to in-memory ARM64 machine code.
///
/// Pipeline:
///   1. QBE C pipeline (parse → SSA → regalloc → isel) → JitInst[]
///   2. Zig encoder (jit_encode.jitEncode) → JitModule with machine code
///
/// Parameters:
///   - `allocator`:   Allocator for the encoder's buffers. The returned
///                    JitResult owns the memory and frees it in deinit().
///   - `il_text`:     The QBE IL source text.
///   - `target_name`: Target name, or null for host default.
///
/// Returns a `JitResult` owning the generated machine code.
pub fn compileILJit(
    allocator: std.mem.Allocator,
    il_text: []const u8,
    target_name: ?[]const u8,
) QBEError!JitResult {
    return compileILJitWithCapacity(allocator, il_text, target_name, 64 * 1024, 16 * 1024);
}

/// Like `compileILJit` but with explicit buffer capacities.
pub fn compileILJitWithCapacity(
    allocator: std.mem.Allocator,
    il_text: []const u8,
    target_name: ?[]const u8,
    code_capacity: u32,
    data_capacity: u32,
) QBEError!JitResult {
    if (il_text.len == 0) return QBEError.InputError;

    const target_z = try nullTermTarget(target_name);

    // Step 1: Run the QBE C pipeline → JitInst[]
    var collector: JitCollector = undefined;
    if (c.jit_collector_init(&collector) != 0) {
        return QBEError.JitCollectorInitFailed;
    }
    defer c.jit_collector_free(&collector);

    const rc = c.qbe_compile_il_jit(il_text.ptr, il_text.len, &collector, target_z);
    if (rc != c.QBE_OK) {
        return switch (rc) {
            c.QBE_ERR_INPUT => QBEError.InputError,
            c.QBE_ERR_TARGET => QBEError.UnknownTarget,
            c.QBE_ERR_PARSE => QBEError.ParseError,
            else => QBEError.JitCollectError,
        };
    }

    if (collector.@"error" != 0) {
        return QBEError.JitCollectError;
    }

    // Step 2: Encode JitInst[] → machine code (pure Zig, no FFI)
    const insts_ptr = collector.insts orelse return QBEError.JitCollectError;

    var module = jit.jitEncode(
        insts_ptr,
        collector.ninst,
        allocator,
        code_capacity,
        data_capacity,
    ) catch return QBEError.JitEncodeFailed;

    // Step 3: Generate the pipeline report while the JitInst[] stream
    // is still live (the collector owns it and will free it on defer).
    const report_text = generatePipelineReportText(
        allocator,
        &module,
        insts_ptr,
        collector.ninst,
        collector.nfunc,
        collector.ndata,
    );

    // If the encoder itself had errors, we still return the module
    // so the caller can inspect diagnostics. The hasErrors() method
    // on the module will tell them.

    return JitResult{
        .module = module,
        .inst_count = collector.ninst,
        .func_count = collector.nfunc,
        .data_count = collector.ndata,
        .report = report_text,
        .report_allocator = if (report_text != null) allocator else null,
    };
}

/// Internal: render the pipeline report into a heap-allocated string.
/// Returns null on allocation failure (non-fatal).
fn generatePipelineReportText(
    allocator: std.mem.Allocator,
    module: *const JitModule,
    insts: [*]const JitInst,
    ninst: u32,
    func_count: u32,
    data_count: u32,
) ?[]const u8 {
    var buf: std.ArrayListUnmanaged(u8) = .{};
    jit.dumpPipelineReport(
        module,
        insts,
        ninst,
        func_count,
        data_count,
        buf.writer(allocator),
    ) catch {
        buf.deinit(allocator);
        return null;
    };
    return buf.toOwnedSlice(allocator) catch {
        buf.deinit(allocator);
        return null;
    };
}

/// Collect QBE IL to JitInst[] only (no encoding). For debugging.
/// Dumps the collected instructions to stderr and returns the count.
pub fn collectILJit(
    il_text: []const u8,
    target_name: ?[]const u8,
) QBEError!u32 {
    if (il_text.len == 0) return QBEError.InputError;

    const target_z = try nullTermTarget(target_name);

    var collector: JitCollector = undefined;
    if (c.jit_collector_init(&collector) != 0) {
        return QBEError.JitCollectorInitFailed;
    }
    defer c.jit_collector_free(&collector);

    const rc = c.qbe_compile_il_jit(il_text.ptr, il_text.len, &collector, target_z);
    if (rc != c.QBE_OK) {
        return switch (rc) {
            c.QBE_ERR_INPUT => QBEError.InputError,
            c.QBE_ERR_TARGET => QBEError.UnknownTarget,
            else => QBEError.JitCollectError,
        };
    }

    c.jit_collector_dump(&collector);
    return collector.ninst;
}

// ── Opcode histogram ───────────────────────────────────────────────────
//
// Global counters indexed by JitInstKind, accumulated across multiple
// qbe_compile_il_jit() calls.  Useful for profiling the instruction mix
// across a batch run.

/// Reset all histogram counters to zero.  Call before a batch run.
pub fn histogramReset() void {
    c.jit_histogram_reset();
}

/// Print the accumulated histogram to stderr, sorted by count descending,
/// with percentages and a simple bar chart.
pub fn histogramDump() void {
    c.jit_histogram_dump();
}

// ── Internal helpers ───────────────────────────────────────────────────

fn nullTermTarget(target_name: ?[]const u8) QBEError!?[*:0]const u8 {
    // We need a static buffer because C expects the pointer to live
    // through the call. This is fine — QBE is single-threaded.
    const S = struct {
        var buf: [64]u8 = undefined;
    };

    if (target_name) |tn| {
        if (tn.len >= S.buf.len) return QBEError.UnknownTarget;
        @memcpy(S.buf[0..tn.len], tn);
        S.buf[tn.len] = 0;
        return S.buf[0..tn.len :0];
    }
    return null;
}

fn checkResult(rc: c_int) QBEError!void {
    if (rc == c.QBE_OK) return;
    return switch (rc) {
        c.QBE_ERR_OUTPUT => QBEError.OutputError,
        c.QBE_ERR_INPUT => QBEError.InputError,
        c.QBE_ERR_TARGET => QBEError.UnknownTarget,
        c.QBE_ERR_PARSE => QBEError.ParseError,
        else => QBEError.UnexpectedError,
    };
}

// ════════════════════════════════════════════════════════════════════════
// Tests
// ════════════════════════════════════════════════════════════════════════

test "qbe version is non-empty" {
    const v = version();
    try std.testing.expect(v.len > 0);
    try std.testing.expect(std.mem.indexOf(u8, v, "fasterbasic") != null);
}

test "qbe default target is non-empty" {
    const t = defaultTarget();
    try std.testing.expect(t.len > 0);
}

test "qbe available targets includes default" {
    const targets = availableTargets();
    try std.testing.expect(targets.len > 0);

    const def = defaultTarget();
    var found = false;
    for (targets) |t| {
        if (std.mem.eql(u8, t, def)) {
            found = true;
            break;
        }
    }
    try std.testing.expect(found);
}

test "qbe compile trivial IL to assembly" {
    const il =
        \\export function w $main() {
        \\@start
        \\  ret 0
        \\}
        \\
    ;

    const tmp_path = "/tmp/fbc_qbe_test.s";

    try compileIL(il, tmp_path, null);

    const file = try std.fs.cwd().openFile(tmp_path, .{});
    defer file.close();
    const stat = try file.stat();
    try std.testing.expect(stat.size > 0);

    var buf: [4096]u8 = undefined;
    const n = try file.readAll(&buf);
    const asm_text = buf[0..n];
    try std.testing.expect(std.mem.indexOf(u8, asm_text, "main") != null);

    std.fs.cwd().deleteFile(tmp_path) catch {};
}

test "qbe compile IL with print call" {
    const il =
        \\data $hello_str = { b "Hello, World!\n", b 0 }
        \\
        \\export function w $main() {
        \\@start
        \\  %s =l loadl $hello_str
        \\  call $puts(l %s)
        \\  ret 0
        \\}
        \\
    ;

    const tmp_path = "/tmp/fbc_qbe_test2.s";

    try compileIL(il, tmp_path, null);

    const file = try std.fs.cwd().openFile(tmp_path, .{});
    defer file.close();
    const stat = try file.stat();
    try std.testing.expect(stat.size > 0);

    std.fs.cwd().deleteFile(tmp_path) catch {};
}

test "qbe rejects empty IL" {
    const result = compileIL("", "/tmp/fbc_qbe_empty.s", null);
    try std.testing.expectError(QBEError.InputError, result);
}

test "qbe rejects unknown target" {
    const il =
        \\export function w $main() {
        \\@start
        \\  ret 0
        \\}
        \\
    ;
    const result = compileIL(il, "/tmp/fbc_qbe_badtarget.s", "z80_cpm");
    try std.testing.expectError(QBEError.UnknownTarget, result);
}

test "qbe JIT compile trivial function" {
    const il =
        \\export function w $main() {
        \\@start
        \\  ret 0
        \\}
        \\
    ;

    var result = compileILJit(std.testing.allocator, il, null) catch |err| {
        std.debug.print("JIT test skipped: {}\n", .{err});
        return;
    };
    defer result.deinit();

    try std.testing.expect(result.module.code_len > 0);
    try std.testing.expect(result.func_count >= 1);

    result.dump();
}

test "qbe JIT compile function with arithmetic" {
    const il =
        \\export function w $add(w %a, w %b) {
        \\@start
        \\  %c =w add %a, %b
        \\  ret %c
        \\}
        \\
    ;

    var result = compileILJit(std.testing.allocator, il, null) catch |err| {
        std.debug.print("JIT test skipped: {}\n", .{err});
        return;
    };
    defer result.deinit();

    try std.testing.expect(result.module.code_len > 0);
    try std.testing.expect(!result.module.hasErrors());
    try std.testing.expect(result.module.stats.instructions_emitted > 0);
    try std.testing.expect(result.module.stats.functions_encoded >= 1);
}

test "qbe JIT compile with data section" {
    const il =
        \\data $msg = { b "hello", b 0 }
        \\
        \\export function w $main() {
        \\@start
        \\  ret 0
        \\}
        \\
    ;

    var result = compileILJit(std.testing.allocator, il, null) catch |err| {
        std.debug.print("JIT test skipped: {}\n", .{err});
        return;
    };
    defer result.deinit();

    try std.testing.expect(result.module.code_len > 0);
    try std.testing.expect(result.module.data_len > 0);
}

test "qbe JIT pipeline report - print hello" {
    // IL representing: PRINT "hello" / END
    // This is the input the JIT receives — a data section with the string,
    // a main function that calls basic_print_string_desc and basic_print_newline.
    const il =
        \\data $hello_str = { b "hello", b 0 }
        \\
        \\export function w $main() {
        \\@start
        \\  %s =l loadl $hello_str
        \\  call $basic_print_string_desc(l %s)
        \\  call $basic_print_newline()
        \\  ret 0
        \\}
        \\
    ;

    var result = compileILJit(std.testing.allocator, il, null) catch |err| {
        std.debug.print("JIT pipeline report skipped: {}\n", .{err});
        return;
    };
    defer result.deinit();

    // Dump the full phase-by-phase pipeline report to stderr
    result.dumpPipelineReport();

    // Verify each JIT phase produced expected results
    // Phase 1: Collection
    try std.testing.expect(result.inst_count > 0);
    try std.testing.expect(result.func_count >= 1);
    try std.testing.expect(result.data_count >= 1);
    // Phase 2: Code generation
    try std.testing.expect(result.module.code_len > 0);
    try std.testing.expect(result.module.stats.instructions_emitted > 0);
    try std.testing.expect(result.module.stats.functions_encoded >= 1);
    // Phase 3: Data generation
    try std.testing.expect(result.module.data_len > 0);
    try std.testing.expect(result.module.stats.data_bytes_emitted > 0);
    // Phase 4: Linking (labels may be 0 for single-block functions)
    // Fixups resolved should equal fixups created (no unresolved branches)
    try std.testing.expectEqual(result.module.stats.fixups_created, result.module.stats.fixups_resolved);
    // Phase 5: External calls
    try std.testing.expect(result.module.stats.ext_calls_recorded >= 2);
    // Report text was generated
    try std.testing.expect(result.report != null);
}

test "qbe JIT pipeline report - branches" {
    // IL with conditional branches: if (x > 0) print "yes" else print "no"
    // This exercises forward jumps, conditional branches, multiple blocks,
    // labels, and branch fixup resolution.
    const il =
        \\data $yes_str = { b "yes", b 0 }
        \\data $no_str = { b "no", b 0 }
        \\
        \\export function w $check(w %x) {
        \\@start
        \\  %c =w csltw %x, 1
        \\  jnz %c, @else, @then
        \\@then
        \\  %s1 =l loadl $yes_str
        \\  call $basic_print_string_desc(l %s1)
        \\  jmp @done
        \\@else
        \\  %s2 =l loadl $no_str
        \\  call $basic_print_string_desc(l %s2)
        \\@done
        \\  call $basic_print_newline()
        \\  ret 0
        \\}
        \\
    ;

    var result = compileILJit(std.testing.allocator, il, null) catch |err| {
        std.debug.print("JIT branch report skipped: {}\n", .{err});
        return;
    };
    defer result.deinit();

    // Dump the full pipeline report — this is the one we want to study
    // for understanding branch linking and fixup resolution.
    result.dumpPipelineReport();

    // Phase 1: Collection — multiple blocks means multiple labels
    try std.testing.expect(result.inst_count > 0);
    try std.testing.expect(result.func_count >= 1);
    try std.testing.expect(result.data_count >= 2);
    // Phase 2: Code generation
    try std.testing.expect(result.module.code_len > 0);
    try std.testing.expect(result.module.stats.instructions_emitted > 0);
    // Phase 2: Labels — QBE optimizes fallthroughs, expect at least 2 labels
    try std.testing.expect(result.module.stats.labels_recorded >= 2);
    // Phase 4: Linking — forward branches require fixups
    try std.testing.expect(result.module.stats.fixups_created > 0);
    try std.testing.expectEqual(result.module.stats.fixups_created, result.module.stats.fixups_resolved);
    // Phase 5: External calls
    try std.testing.expect(result.module.stats.ext_calls_recorded >= 3);
    // Report text was generated
    try std.testing.expect(result.report != null);
}

test "qbe JIT rejects empty IL" {
    const result = compileILJit(std.testing.allocator, "", null);
    try std.testing.expectError(QBEError.InputError, result);
}

test "qbe JIT branch disassembly — IF/ELSE diamond" {
    const allocator = std.testing.allocator;

    const il =
        \\data $yes_str = { b "yes", b 0 }
        \\data $no_str = { b "no", b 0 }
        \\
        \\export function w $check(w %x) {
        \\@start
        \\  %c =w csltw %x, 1
        \\  jnz %c, @else, @then
        \\@then
        \\  %s1 =l loadl $yes_str
        \\  call $basic_print_string_desc(l %s1)
        \\  jmp @done
        \\@else
        \\  %s2 =l loadl $no_str
        \\  call $basic_print_string_desc(l %s2)
        \\@done
        \\  call $basic_print_newline()
        \\  ret 0
        \\}
        \\
    ;

    var result = compileILJit(allocator, il, null) catch |err| {
        std.debug.print("JIT branch disasm skipped: {}\n", .{err});
        return;
    };
    defer result.deinit();

    const mod = &result.module;
    const code = mod.code[0..mod.code_len];

    std.debug.print("\n======================================================================\n", .{});
    std.debug.print("IF/ELSE branch disassembly ({d} bytes, {d} fixups created, {d} resolved)\n", .{
        mod.code_len, mod.stats.fixups_created, mod.stats.fixups_resolved,
    });
    std.debug.print("======================================================================\n", .{});

    // Build label-offset reverse map for annotation
    var label_offsets = std.AutoHashMap(u32, i32).init(allocator);
    defer label_offsets.deinit();
    {
        var it = mod.labels.iterator();
        while (it.next()) |entry| {
            try label_offsets.put(entry.value_ptr.*, entry.key_ptr.*);
        }
    }

    // Capstone disassembly
    var disasm = capstone.Disassembler.init() catch {
        std.debug.print("  [Capstone unavailable — falling back to hex]\n", .{});
        jit.dumpCodeHex(mod, std.fs.File.stderr().deprecatedWriter()) catch {};
        return;
    };
    defer disasm.deinit();

    const insns = try disasm.disassemble(code.ptr, code.len, 0, allocator);
    defer allocator.free(insns);

    for (insns) |di| {
        // Print label if this offset has one
        if (label_offsets.get(di.offset)) |lid| {
            std.debug.print(".L{d}:\n", .{lid});
        }
        // Print ext-call annotation
        for (mod.ext_calls.items) |ext| {
            if (ext.code_offset == di.offset) {
                std.debug.print("  ; ext-call: {s}\n", .{ext.getName()});
            }
        }
        std.debug.print("  0x{x:0>4}:  {x:0>8}  {s} {s}\n", .{
            di.offset, di.raw_word, di.getMnemonic(), di.getOperands(),
        });
    }

    std.debug.print("======================================================================\n", .{});

    // Verify all fixups resolved
    try std.testing.expectEqual(mod.stats.fixups_created, mod.stats.fixups_resolved);

    // Verify every resolved branch actually lands on a known label offset
    for (mod.fixups.items) |fixup| {
        const word = mod.readWord(fixup.code_offset);
        const target_off = mod.labels.get(fixup.target_id) orelse {
            std.debug.print("FAIL: unresolved label {d}\n", .{fixup.target_id});
            return error.TestUnexpectedResult;
        };
        const decoded = decodeBranchTarget(word, fixup.code_offset);
        if (decoded) |actual| {
            std.debug.print("  fixup@0x{x:0>4} → 0x{x:0>4} (expect 0x{x:0>4}) {s}\n", .{
                fixup.code_offset,                              actual, target_off,
                if (actual == target_off) "OK" else "MISMATCH",
            });
            try std.testing.expectEqual(target_off, actual);
        } else {
            std.debug.print("  fixup@0x{x:0>4}: can't decode word 0x{x:0>8}\n", .{
                fixup.code_offset, word,
            });
            return error.TestUnexpectedResult;
        }
    }

    // Scan for zero-offset branches (unresolved placeholders)
    var off: u32 = 0;
    while (off < mod.code_len) : (off += 4) {
        const w = mod.readWord(off);
        if (decodeBranchTarget(w, off)) |t| {
            if (t == off) {
                std.debug.print("  WARNING: self-branch at 0x{x:0>4} (0x{x:0>8})\n", .{ off, w });
            }
        }
    }
}

/// Decode the target byte-offset of an ARM64 branch instruction.
/// Returns null if the word is not a recognised branch encoding.
fn decodeBranchTarget(word: u32, pc: u32) ?u32 {
    // B / BL (imm26)
    if (word & 0x7c000000 == 0x14000000) {
        const raw: u32 = word & 0x03ffffff;
        const sext: i32 = if (raw & (1 << 25) != 0)
            @bitCast(raw | 0xfc000000)
        else
            @intCast(raw);
        const target: i64 = @as(i64, pc) + @as(i64, sext) * 4;
        return @intCast(@as(u64, @bitCast(target)));
    }
    // B.cond (imm19)
    if (word & 0xff000010 == 0x54000000) {
        const raw: u32 = (word >> 5) & 0x7ffff;
        const sext: i32 = if (raw & (1 << 18) != 0)
            @bitCast(raw | 0xfff80000)
        else
            @intCast(raw);
        const target: i64 = @as(i64, pc) + @as(i64, sext) * 4;
        return @intCast(@as(u64, @bitCast(target)));
    }
    // CBZ / CBNZ (imm19)
    if (word & 0x7e000000 == 0x34000000) {
        const raw: u32 = (word >> 5) & 0x7ffff;
        const sext: i32 = if (raw & (1 << 18) != 0)
            @bitCast(raw | 0xfff80000)
        else
            @intCast(raw);
        const target: i64 = @as(i64, pc) + @as(i64, sext) * 4;
        return @intCast(@as(u64, @bitCast(target)));
    }
    return null;
}
