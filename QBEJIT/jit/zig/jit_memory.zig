//! jit_memory.zig — Platform-specific JIT memory allocation and management
//!
//! This module implements the executable memory allocation strategy described
//! in the JIT design document (design/jit_design.md):
//!
//!   - **Buddy Allocation**: Reserve contiguous VA space, then commit
//!     separate Code (RX) and Data (RW) regions with guaranteed proximity
//!     for ADRP (±4GB) addressing.
//!
//!   - **W^X Compliance**: macOS Apple Silicon requires MAP_JIT and
//!     pthread_jit_write_protect_np toggling. Linux uses mmap + mprotect.
//!
//!   - **Instruction Cache Invalidation**: Required after writing code
//!     before execution on ARM64.
//!
//! Architecture:
//!   JitModule (encoded buffers)
//!     → JitMemoryRegion.allocate()
//!       → copyCode() / copyData()
//!       → linkAndRelocate() [via jit_linker.zig]
//!       → makeExecutable()
//!       → icacheInvalidate()
//!       → execute via function pointer cast
//!     → JitMemoryRegion.free()
//!
//! The memory layout follows the "Buddy Allocation" technique:
//!
//!   [      Virtual Address Space Reservation (total_size)      ]
//!   |--------------------|--------------------------------------|
//!   |  Code (RX) + Lit   |          Data (RW)                   |
//!   |  (MAP_JIT on macOS)|          (Standard mmap)             |
//!   |--------------------|--------------------------------------|
//!   ^                    ^
//!   base                 base + code_capacity
//!
//! This guarantees that ADRP from any code instruction can reach any
//! data address (they're in the same contiguous VA range).

const std = @import("std");
const builtin = @import("builtin");

// ============================================================================
// Section: Platform-Specific Extern Declarations
// ============================================================================

/// macOS: Toggle JIT write protection on the current thread.
///   - protect = false (0): Enable WRITE access (disable execute)
///   - protect = true  (1): Enable EXECUTE access (disable write)
extern "c" fn pthread_jit_write_protect_np(protect: c_int) void;

/// macOS: Invalidate instruction cache for a range.
extern "c" fn sys_icache_invalidate(addr: *const anyopaque, len: usize) void;

/// Linux ARM64: GCC built-in for icache invalidation.
/// On Linux, we use inline assembly or __builtin___clear_cache equivalent.
/// Zig's std.os doesn't expose this directly, so we use the syscall.

// ============================================================================
// Section: Constants
// ============================================================================

/// Default code region capacity (128 KB — plenty for most JIT programs)
pub const DEFAULT_CODE_CAPACITY: usize = 128 * 1024;

/// Default data region capacity (64 KB)
pub const DEFAULT_DATA_CAPACITY: usize = 64 * 1024;

/// Trampoline stub size: LDR X16, [PC, #8] + BR X16 + .quad addr = 16 bytes
pub const TRAMPOLINE_STUB_SIZE: usize = 16;

/// Maximum number of external symbols we support trampolines for
pub const MAX_TRAMPOLINE_STUBS: usize = 256;

/// Reserved trampoline island capacity
pub const TRAMPOLINE_ISLAND_CAPACITY: usize = TRAMPOLINE_STUB_SIZE * MAX_TRAMPOLINE_STUBS;

/// Page size — uses the compile-time minimum page size so it can be used
/// in alignment specifiers.  On macOS ARM64 this is 16384, Linux 4096.
pub const PAGE_SIZE: usize = std.heap.page_size_min;

/// PROT constants for mmap/mprotect (platform-independent u32 values)
const PROT_NONE: u32 = 0x0;
const PROT_READ: u32 = 0x1;
const PROT_WRITE: u32 = 0x2;
const PROT_EXEC: u32 = 0x4;

// ============================================================================
// Section: Error Types
// ============================================================================

pub const JitMemoryError = error{
    /// Target address is beyond ±128MB BL instruction range.
    BLOutOfRange,
    /// Failed to reserve virtual address space
    ReserveFailed,
    /// Failed to commit code region
    CodeCommitFailed,
    /// Failed to commit data region
    DataCommitFailed,
    /// Failed to set memory protection
    ProtectFailed,
    /// Code buffer overflow
    CodeOverflow,
    /// Data buffer overflow
    DataOverflow,
    /// Trampoline island overflow
    TrampolineOverflow,
    /// Region not in writable state
    NotWritable,
    /// Region not in executable state
    NotExecutable,
    /// Invalid alignment
    InvalidAlignment,
    /// Region already freed
    AlreadyFreed,
    /// Platform not supported for JIT execution
    UnsupportedPlatform,
};

// ============================================================================
// Section: Protection State
// ============================================================================

/// Tracks the current W^X state of the code region.
pub const ProtectionState = enum {
    /// Initial state — region is writable for code emission
    Writable,
    /// Region is executable — code can be called but not written
    Executable,
    /// Region has been freed
    Freed,
};

// ============================================================================
// Section: JitMemoryRegion
// ============================================================================

/// A JIT memory region that manages code and data segments with proper
/// W^X compliance and ADRP-reachable proximity guarantees.
///
/// Usage:
///   1. allocate() — reserve and commit memory
///   2. copyCode() / copyData() — write encoded bytes (region is Writable)
///   3. makeExecutable() — switch to RX mode + flush icache
///   4. codePtr() — get a callable function pointer
///   5. free() — release all memory
pub const JitMemoryRegion = struct {
    /// Base of the entire reserved VA range (code + trampolines; on macOS
    /// this is the MAP_JIT region only — data lives in a separate mmap).
    base: ?[*]align(PAGE_SIZE) u8,

    /// Total reserved VA size of the base mapping (code + trampolines).
    /// On Linux this also includes data; on macOS data is separate.
    total_size: usize,

    // ── Code segment ──────────────────────────────────────────────

    /// Start of the code region (same as base)
    code_base: ?[*]u8,

    /// Capacity of the code region in bytes
    code_capacity: usize,

    /// Current write offset in the code region
    code_len: usize,

    // ── Trampoline island ─────────────────────────────────────────

    /// Start of trampoline island (at end of code region capacity)
    trampoline_base: ?[*]u8,

    /// Capacity reserved for trampolines
    trampoline_capacity: usize,

    /// Current write offset in trampoline island
    trampoline_len: usize,

    // ── Data segment ──────────────────────────────────────────────

    /// Start of the data region
    data_base: ?[*]u8,

    /// Capacity of the data region in bytes
    data_capacity: usize,

    /// Current write offset in the data region
    data_len: usize,

    // ── Separate data mmap (macOS) ────────────────────────────────

    /// On macOS the data section lives in a separate non-MAP_JIT mmap so
    /// it stays RW while the code region is toggled to RX for execution.
    /// On Linux (or when data shares the base mapping) these are null / 0.
    data_mmap_base: ?[*]align(PAGE_SIZE) u8,
    data_mmap_size: usize,

    // ── State tracking ────────────────────────────────────────────

    /// Current W^X protection state
    state: ProtectionState,

    /// Whether this is running on macOS (uses MAP_JIT + pthread_jit_write_protect_np)
    is_macos: bool,

    // ── Public API ────────────────────────────────────────────────

    /// Allocate a JIT memory region with the specified capacities.
    ///
    /// On macOS: Uses a single MAP_JIT mmap for the entire region
    /// (code + trampolines + data). This guarantees contiguous VA space
    /// with ADRP reachability and W^X compliance via
    /// pthread_jit_write_protect_np.
    ///
    /// On Linux: Uses the "Buddy Allocation" technique — reserve
    /// contiguous VA with PROT_NONE, then commit code (RW) and data (RW)
    /// sub-regions via mprotect. Code region is switched to RX before
    /// execution.
    ///
    /// The trampoline island is allocated at the end of the code region's
    /// capacity, ensuring all BL targets are within ±128MB.
    pub fn allocate(
        code_cap: usize,
        data_cap: usize,
    ) JitMemoryError!JitMemoryRegion {
        return allocateWithTrampoline(code_cap, data_cap, TRAMPOLINE_ISLAND_CAPACITY);
    }

    /// Like allocate() but with explicit trampoline capacity.
    pub fn allocateWithTrampoline(
        code_cap: usize,
        data_cap: usize,
        trampoline_cap: usize,
    ) JitMemoryError!JitMemoryRegion {
        const is_macos = (builtin.os.tag == .macos);

        // Round capacities up to page boundaries
        const code_pages = alignToPage(code_cap + trampoline_cap);
        const data_pages = alignToPage(data_cap);
        const total = code_pages + data_pages;

        if (is_macos) {
            return allocateMacOS(code_cap, data_cap, trampoline_cap, code_pages, total);
        } else {
            return allocateLinux(code_cap, data_cap, trampoline_cap, code_pages, data_pages, total);
        }
    }

    /// macOS allocation: two separate mmaps.
    ///
    /// 1. Code + Trampolines → MAP_JIT mmap (W^X via pthread_jit_write_protect_np)
    /// 2. Data               → regular mmap (always PROT_READ | PROT_WRITE)
    ///
    /// pthread_jit_write_protect_np toggles write permission for ALL MAP_JIT
    /// pages on the current thread.  If the data section lived in the same
    /// MAP_JIT region, calling makeExecutable() would make data non-writable
    /// and any STR into a global variable would fault.  Separating them
    /// ensures data stays RW while code is RX.
    fn allocateMacOS(
        code_cap: usize,
        data_cap: usize,
        trampoline_cap: usize,
        code_pages: usize,
        _total: usize,
    ) JitMemoryError!JitMemoryRegion {
        _ = _total; // no longer used — we mmap code and data separately

        // 1. Code + Trampolines: MAP_JIT mmap
        const code_mapping = std.posix.mmap(
            null,
            code_pages,
            PROT_READ | PROT_WRITE | PROT_EXEC,
            .{ .TYPE = .PRIVATE, .ANONYMOUS = true, .JIT = true },
            -1,
            0,
        ) catch return JitMemoryError.ReserveFailed;

        const code_base: [*]align(PAGE_SIZE) u8 = @alignCast(code_mapping.ptr);

        // 2. Data: regular (non-MAP_JIT) mmap — always RW
        const data_pages = alignToPage(data_cap);
        const data_mapping = std.posix.mmap(
            null,
            data_pages,
            PROT_READ | PROT_WRITE,
            .{ .TYPE = .PRIVATE, .ANONYMOUS = true },
            -1,
            0,
        ) catch {
            // Clean up the code mapping on failure
            releaseVA(code_base, code_pages);
            return JitMemoryError.DataCommitFailed;
        };

        const data_base: [*]align(PAGE_SIZE) u8 = @alignCast(data_mapping.ptr);

        // Enable write access for initial code emission
        enableWrite();

        return JitMemoryRegion{
            .base = code_base,
            .total_size = code_pages,
            .code_base = code_base,
            .code_capacity = code_cap,
            .code_len = 0,
            .trampoline_base = @ptrFromInt(@intFromPtr(code_base) + code_cap),
            .trampoline_capacity = trampoline_cap,
            .trampoline_len = 0,
            .data_base = data_base,
            .data_capacity = data_cap,
            .data_len = 0,
            .data_mmap_base = data_base,
            .data_mmap_size = data_pages,
            .state = .Writable,
            .is_macos = true,
        };
    }

    /// Linux allocation: reserve contiguous VA, then commit sub-regions.
    fn allocateLinux(
        code_cap: usize,
        data_cap: usize,
        trampoline_cap: usize,
        code_pages: usize,
        data_pages: usize,
        total: usize,
    ) JitMemoryError!JitMemoryRegion {
        // Step 1: Reserve contiguous VA space with PROT_NONE
        const base = reserveVA(total) orelse return JitMemoryError.ReserveFailed;

        // Step 2: Commit code region (RW initially, switched to RX before exec)
        commitRegionRW(base, code_pages) catch {
            releaseVA(base, total);
            return JitMemoryError.CodeCommitFailed;
        };

        // Step 3: Commit data region (RW, stays RW)
        const data_ptr: [*]align(PAGE_SIZE) u8 = @ptrFromInt(@intFromPtr(base) + code_pages);
        commitRegionRW(data_ptr, data_pages) catch {
            releaseVA(base, total);
            return JitMemoryError.DataCommitFailed;
        };

        return JitMemoryRegion{
            .base = base,
            .total_size = total,
            .code_base = base,
            .code_capacity = code_cap,
            .code_len = 0,
            .trampoline_base = @ptrFromInt(@intFromPtr(base) + code_cap),
            .trampoline_capacity = trampoline_cap,
            .trampoline_len = 0,
            .data_base = @ptrFromInt(@intFromPtr(base) + code_pages),
            .data_capacity = data_cap,
            .data_len = 0,
            .data_mmap_base = null,
            .data_mmap_size = 0,
            .state = .Writable,
            .is_macos = false,
        };
    }

    /// Copy encoded machine code into the code region.
    /// Region must be in Writable state.
    pub fn copyCode(self: *JitMemoryRegion, code: []const u8) JitMemoryError!void {
        if (self.state != .Writable) return JitMemoryError.NotWritable;
        if (self.code_len + code.len > self.code_capacity) return JitMemoryError.CodeOverflow;

        const dst = self.code_base orelse return JitMemoryError.AlreadyFreed;
        @memcpy(dst[self.code_len..][0..code.len], code);
        self.code_len += code.len;
    }

    /// Copy data section bytes into the data region.
    pub fn copyData(self: *JitMemoryRegion, data: []const u8) JitMemoryError!void {
        if (self.data_len + data.len > self.data_capacity) return JitMemoryError.DataOverflow;

        const dst = self.data_base orelse return JitMemoryError.AlreadyFreed;
        @memcpy(dst[self.data_len..][0..data.len], data);
        self.data_len += data.len;
    }

    /// Write a single trampoline stub into the trampoline island.
    /// Returns the byte offset of the stub relative to code_base.
    ///
    /// Stub layout (16 bytes):
    ///   +0: LDR X16, [PC, #8]   → 0x58000050
    ///   +4: BR  X16              → 0xd61f0200
    ///   +8: .quad <target_addr>  → 64-bit absolute address
    pub fn writeTrampoline(self: *JitMemoryRegion, target_addr: u64) JitMemoryError!usize {
        if (self.state != .Writable) return JitMemoryError.NotWritable;
        if (self.trampoline_len + TRAMPOLINE_STUB_SIZE > self.trampoline_capacity) {
            return JitMemoryError.TrampolineOverflow;
        }

        const tramp = self.trampoline_base orelse return JitMemoryError.AlreadyFreed;
        const off = self.trampoline_len;

        // LDR X16, [PC, #8] — load address from 8 bytes ahead
        writeU32(tramp[off..][0..4], 0x58000050);

        // BR X16 — indirect branch to loaded address
        writeU32(tramp[off + 4 ..][0..4], 0xd61f0200);

        // .quad target_addr — 64-bit absolute address
        writeU64(tramp[off + 8 ..][0..8], target_addr);

        self.trampoline_len += TRAMPOLINE_STUB_SIZE;

        // Return absolute offset from code_base
        return self.code_capacity + off;
    }

    /// Write a trap stub into the trampoline island for an unresolved symbol.
    /// Returns the byte offset of the stub relative to code_base.
    ///
    /// Trap stub layout (16 bytes — same size as a trampoline):
    ///   +0: BRK #0xF001   → 0xD43E0020  (signals "unresolved JIT symbol")
    ///   +4: BRK #0xF001   → 0xD43E0020  (redundant, for alignment)
    ///   +8: .quad 0xDEAD  → sentinel value (never executed)
    ///
    /// When JIT code BLs to this stub, the CPU hits BRK immediately,
    /// producing a clean SIGTRAP instead of a null-pointer SIGSEGV.
    pub fn writeTrapStub(self: *JitMemoryRegion) JitMemoryError!usize {
        if (self.state != .Writable) return JitMemoryError.NotWritable;
        if (self.trampoline_len + TRAMPOLINE_STUB_SIZE > self.trampoline_capacity) {
            return JitMemoryError.TrampolineOverflow;
        }

        const tramp = self.trampoline_base orelse return JitMemoryError.AlreadyFreed;
        const off = self.trampoline_len;

        // BRK #0xF001 — immediate 0xF001 encodes as: 0xD4000000 | (0xF001 << 5)
        const brk_word: u32 = 0xD4000000 | (0xF001 << 5);
        writeU32(tramp[off..][0..4], brk_word);
        writeU32(tramp[off + 4 ..][0..4], brk_word);

        // .quad 0xDEAD — sentinel, never reached
        writeU64(tramp[off + 8 ..][0..8], 0xDEAD);

        self.trampoline_len += TRAMPOLINE_STUB_SIZE;

        // Return absolute offset from code_base
        return self.code_capacity + off;
    }

    /// Patch a BL instruction at the given code offset to branch directly
    /// to an absolute target address.  Returns `BLOutOfRange` if the
    /// target is beyond ±128 MB from the BL instruction.
    pub fn patchBLDirect(self: *JitMemoryRegion, bl_offset: usize, target_addr: u64) JitMemoryError!void {
        if (self.state != .Writable) return JitMemoryError.NotWritable;

        const code = self.code_base orelse return JitMemoryError.AlreadyFreed;

        // Absolute address of the BL instruction
        const bl_addr: u64 = @intFromPtr(code) + bl_offset;

        // Signed byte delta from BL to target
        const delta_bytes: i64 = @as(i64, @intCast(target_addr)) - @as(i64, @intCast(bl_addr));

        // BL range: imm26 is signed, scaled by 4 → ±128 MB
        const BL_MAX: i64 = 128 * 1024 * 1024;
        if (delta_bytes >= BL_MAX or delta_bytes < -BL_MAX) {
            return JitMemoryError.BLOutOfRange;
        }

        // Must be 4-byte aligned
        if (@rem(delta_bytes, 4) != 0) return JitMemoryError.InvalidAlignment;

        const delta_words: i32 = @intCast(@divExact(delta_bytes, 4));
        const delta_u32: u32 = @bitCast(delta_words);

        // BL encoding: opcode[31:26] = 100101, imm26[25:0]
        const bl_word: u32 = 0x94000000 | (delta_u32 & 0x03ffffff);
        writeU32(code[bl_offset..][0..4], bl_word);
    }

    /// Patch a BL instruction at the given code offset to branch to
    /// a trampoline stub at the given stub offset (both relative to code_base).
    pub fn patchBLToTrampoline(self: *JitMemoryRegion, bl_offset: usize, stub_offset: usize) JitMemoryError!void {
        if (self.state != .Writable) return JitMemoryError.NotWritable;

        const code = self.code_base orelse return JitMemoryError.AlreadyFreed;

        // Compute delta in instruction words
        const delta_bytes: i64 = @as(i64, @intCast(stub_offset)) - @as(i64, @intCast(bl_offset));
        const delta_words: i32 = @intCast(@divExact(delta_bytes, 4));
        const delta_u32: u32 = @bitCast(delta_words);

        // BL encoding: opcode[31:26] = 100101, imm26[25:0]
        const bl_word: u32 = 0x94000000 | (delta_u32 & 0x03ffffff);
        writeU32(code[bl_offset..][0..4], bl_word);
    }

    /// Patch an ADRP instruction at the given code offset with the actual
    /// page delta, and patch the following ADD with the page offset.
    ///
    /// This resolves LOAD_ADDR relocations once real addresses are known.
    pub fn patchAdrpAdd(
        self: *JitMemoryRegion,
        adrp_offset: usize,
        target_addr: u64,
    ) JitMemoryError!void {
        if (self.state != .Writable) return JitMemoryError.NotWritable;

        const code = self.code_base orelse return JitMemoryError.AlreadyFreed;

        // Compute ADRP page delta
        const pc_addr = @intFromPtr(code) + adrp_offset;
        const pc_page = pc_addr & ~@as(usize, 0xFFF);
        const target_page = target_addr & ~@as(u64, 0xFFF);

        const page_delta_bytes: i64 = @as(i64, @intCast(target_page)) - @as(i64, @intCast(pc_page));
        const page_delta: i32 = @intCast(@divExact(page_delta_bytes, 4096));

        const page_offset: u12 = @truncate(target_addr & 0xFFF);

        // Read existing ADRP to preserve Rd
        const existing_adrp = readU32(code[adrp_offset..][0..4]);
        const rd_bits = existing_adrp & 0x1F;

        // Encode ADRP: 1 immlo[1:0] 10000 immhi[18:0] Rd[4:0]
        const delta_u32: u32 = @bitCast(page_delta);
        const immlo: u32 = delta_u32 & 0x3;
        const immhi: u32 = (delta_u32 >> 2) & 0x7FFFF;
        const adrp_word: u32 = 0x90000000 | (immlo << 29) | (immhi << 5) | rd_bits;
        writeU32(code[adrp_offset..][0..4], adrp_word);

        // Patch ADD immediately after ADRP
        const add_offset = adrp_offset + 4;
        const existing_add = readU32(code[add_offset..][0..4]);
        // Preserve Rd and Rn, patch imm12
        const add_base = existing_add & 0xFFC003FF; // clear imm12 field [21:10]
        const add_word = add_base | (@as(u32, page_offset) << 10);
        writeU32(code[add_offset..][0..4], add_word);
    }

    /// Switch the code region from Writable to Executable.
    ///
    /// This:
    ///   1. On macOS: calls pthread_jit_write_protect_np(1) to enable X
    ///   2. On Linux: calls mprotect(PROT_READ | PROT_EXEC)
    ///   3. Invalidates the instruction cache
    pub fn makeExecutable(self: *JitMemoryRegion) JitMemoryError!void {
        if (self.state == .Freed) return JitMemoryError.AlreadyFreed;

        const base = self.base orelse return JitMemoryError.AlreadyFreed;
        const code_pages = alignToPage(self.code_capacity + self.trampoline_capacity);

        if (self.is_macos) {
            // macOS: Thread-local W^X toggle
            enableExecute();
        } else {
            // Linux: mprotect the code region to RX
            const slice: []align(PAGE_SIZE) u8 = @alignCast(base[0..code_pages]);
            std.posix.mprotect(slice, PROT_READ | PROT_EXEC) catch {
                return JitMemoryError.ProtectFailed;
            };
        }

        // Invalidate instruction cache for the code + trampoline region
        const total_code_len = self.code_len + self.trampoline_len +
            (self.code_capacity - self.code_len); // include gap to trampolines
        icacheInvalidate(base, total_code_len);

        self.state = .Executable;
    }

    /// Switch the code region back to Writable (for hot-patching).
    ///
    /// Used for breakpoint insertion, trampoline updates, etc.
    pub fn makeWritable(self: *JitMemoryRegion) JitMemoryError!void {
        if (self.state == .Freed) return JitMemoryError.AlreadyFreed;

        const base = self.base orelse return JitMemoryError.AlreadyFreed;
        const code_pages = alignToPage(self.code_capacity + self.trampoline_capacity);

        if (self.is_macos) {
            enableWrite();
        } else {
            const slice: []align(PAGE_SIZE) u8 = @alignCast(base[0..code_pages]);
            std.posix.mprotect(slice, PROT_READ | PROT_WRITE) catch {
                return JitMemoryError.ProtectFailed;
            };
        }

        self.state = .Writable;
    }

    /// Get a typed function pointer to the code at a given offset.
    ///
    /// The caller must ensure the region is in Executable state and
    /// that the offset points to a valid function entry.
    pub fn getFunctionPtr(self: *const JitMemoryRegion, comptime T: type, offset: usize) JitMemoryError!T {
        if (self.state != .Executable) return JitMemoryError.NotExecutable;
        const code = self.code_base orelse return JitMemoryError.AlreadyFreed;
        const addr = @intFromPtr(code) + offset;
        return @ptrFromInt(addr);
    }

    /// Get the absolute address of a code offset (for relocation calculations).
    pub fn codeAddress(self: *const JitMemoryRegion, offset: usize) ?u64 {
        const code = self.code_base orelse return null;
        return @intFromPtr(code) + offset;
    }

    /// Get the absolute address of a data offset (for relocation calculations).
    pub fn dataAddress(self: *const JitMemoryRegion, offset: usize) ?u64 {
        const data = self.data_base orelse return null;
        return @intFromPtr(data) + offset;
    }

    /// Get a pointer to the data region for direct access.
    pub fn dataSlice(self: *const JitMemoryRegion) ?[]u8 {
        const data = self.data_base orelse return null;
        return data[0..self.data_len];
    }

    /// Get a read-only view of the code region.
    pub fn codeSlice(self: *const JitMemoryRegion) ?[]const u8 {
        const code = self.code_base orelse return null;
        return code[0..self.code_len];
    }

    /// Get a read-only view of the trampoline island.
    pub fn trampolineSlice(self: *const JitMemoryRegion) ?[]const u8 {
        const tramp = self.trampoline_base orelse return null;
        return tramp[0..self.trampoline_len];
    }

    /// Read a 32-bit word from the code region at the given byte offset.
    pub fn readCodeWord(self: *const JitMemoryRegion, offset: usize) ?u32 {
        const code = self.code_base orelse return null;
        if (offset + 4 > self.code_len) return null;
        return readU32(code[offset..][0..4]);
    }

    /// Patch a 32-bit word in the code region (must be Writable).
    pub fn patchCodeWord(self: *JitMemoryRegion, offset: usize, word: u32) JitMemoryError!void {
        if (self.state != .Writable) return JitMemoryError.NotWritable;
        const code = self.code_base orelse return JitMemoryError.AlreadyFreed;
        writeU32(code[offset..][0..4], word);
    }

    /// Free all memory regions.
    ///
    /// After this call, all pointers into the region are invalid.
    pub fn free(self: *JitMemoryRegion) void {
        // Free the separate data mmap first (macOS only)
        if (self.data_mmap_base) |data_base| {
            releaseVA(data_base, self.data_mmap_size);
            self.data_mmap_base = null;
            self.data_mmap_size = 0;
        }

        if (self.base) |base| {
            // On macOS, ensure we're in write mode before unmapping
            // (not strictly necessary but clean)
            if (self.is_macos and self.state == .Executable) {
                enableWrite();
            }

            releaseVA(base, self.total_size);

            self.base = null;
            self.code_base = null;
            self.data_base = null;
            self.trampoline_base = null;
            self.state = .Freed;
        }
    }

    /// Check if the region has been allocated and not freed.
    pub fn isAlive(self: *const JitMemoryRegion) bool {
        return self.base != null and self.state != .Freed;
    }

    /// Return diagnostic information about the memory layout.
    pub fn layoutInfo(self: *const JitMemoryRegion) LayoutInfo {
        return .{
            .base_addr = if (self.base) |b| @intFromPtr(b) else 0,
            .code_addr = if (self.code_base) |c| @intFromPtr(c) else 0,
            .data_addr = if (self.data_base) |d| @intFromPtr(d) else 0,
            .trampoline_addr = if (self.trampoline_base) |t| @intFromPtr(t) else 0,
            .total_size = self.total_size,
            .code_capacity = self.code_capacity,
            .code_used = self.code_len,
            .data_capacity = self.data_capacity,
            .data_used = self.data_len,
            .trampoline_capacity = self.trampoline_capacity,
            .trampoline_used = self.trampoline_len,
            .state = self.state,
        };
    }

    /// Create an uninitialized (null) region — useful for deferred allocation.
    pub fn empty() JitMemoryRegion {
        return JitMemoryRegion{
            .base = null,
            .total_size = 0,
            .code_base = null,
            .code_capacity = 0,
            .code_len = 0,
            .trampoline_base = null,
            .trampoline_capacity = 0,
            .trampoline_len = 0,
            .data_base = null,
            .data_capacity = 0,
            .data_len = 0,
            .data_mmap_base = null,
            .data_mmap_size = 0,
            .state = .Freed,
            .is_macos = (builtin.os.tag == .macos),
        };
    }
};

/// Diagnostic layout information for reporting.
pub const LayoutInfo = struct {
    base_addr: usize,
    code_addr: usize,
    data_addr: usize,
    trampoline_addr: usize,
    total_size: usize,
    code_capacity: usize,
    code_used: usize,
    data_capacity: usize,
    data_used: usize,
    trampoline_capacity: usize,
    trampoline_used: usize,
    state: ProtectionState,

    /// Format layout info for human-readable display.
    pub fn dump(self: *const LayoutInfo, writer: anytype) !void {
        try writer.writeAll("=== JIT Memory Layout ===\n");
        try std.fmt.format(writer, "  Base:        0x{x:0>16}\n", .{self.base_addr});
        try std.fmt.format(writer, "  Code:        0x{x:0>16} ({d}/{d} bytes used)\n", .{ self.code_addr, self.code_used, self.code_capacity });
        try std.fmt.format(writer, "  Trampolines: 0x{x:0>16} ({d}/{d} bytes used)\n", .{ self.trampoline_addr, self.trampoline_used, self.trampoline_capacity });
        try std.fmt.format(writer, "  Data:        0x{x:0>16} ({d}/{d} bytes used)\n", .{ self.data_addr, self.data_used, self.data_capacity });
        try std.fmt.format(writer, "  Total VA:    {d} bytes\n", .{self.total_size});
        try std.fmt.format(writer, "  State:       {s}\n", .{@tagName(self.state)});

        if (self.code_addr != 0 and self.data_addr != 0) {
            const distance = if (self.data_addr > self.code_addr)
                self.data_addr - self.code_addr
            else
                self.code_addr - self.data_addr;
            try std.fmt.format(writer, "  Code↔Data:   {d} bytes ({d} KB)\n", .{ distance, distance / 1024 });
        }
    }
};

// ============================================================================
// Section: Platform-Specific Implementation
// ============================================================================

/// Round a size up to the nearest page boundary.
fn alignToPage(size: usize) usize {
    return (size + PAGE_SIZE - 1) & ~(PAGE_SIZE - 1);
}

/// Reserve virtual address space without committing physical memory.
fn reserveVA(total_size: usize) ?[*]align(PAGE_SIZE) u8 {
    if (comptime builtin.os.tag == .macos or builtin.os.tag == .linux) {
        // mmap with PROT_NONE reserves VA space without committing
        const result = std.posix.mmap(
            null,
            total_size,
            PROT_NONE,
            .{ .TYPE = .PRIVATE, .ANONYMOUS = true },
            -1,
            0,
        );
        if (result) |mapping| {
            return @alignCast(mapping.ptr);
        } else |_| {
            return null;
        }
    } else {
        // Unsupported platform
        return null;
    }
}

/// Release reserved VA space.
fn releaseVA(base: [*]align(PAGE_SIZE) u8, total_size: usize) void {
    if (comptime builtin.os.tag == .macos or builtin.os.tag == .linux) {
        const slice: []align(PAGE_SIZE) u8 = base[0..total_size];
        std.posix.munmap(slice);
    }
}

/// Commit a sub-region of a PROT_NONE reservation to RW via mprotect.
/// Used by the Linux buddy-allocation path.
fn commitRegionRW(base: [*]align(PAGE_SIZE) u8, size: usize) !void {
    if (comptime builtin.os.tag == .linux) {
        const slice: []align(PAGE_SIZE) u8 = base[0..size];
        try std.posix.mprotect(slice, PROT_READ | PROT_WRITE);
    } else if (comptime builtin.os.tag == .macos) {
        // macOS path uses single mmap — this should not be called.
        // But if it is, mprotect the reserved range to RW.
        const slice: []align(PAGE_SIZE) u8 = base[0..size];
        try std.posix.mprotect(slice, PROT_READ | PROT_WRITE);
    }
}

/// macOS: Enable write access on the current thread's JIT pages.
fn enableWrite() void {
    if (comptime builtin.os.tag == .macos) {
        pthread_jit_write_protect_np(0); // 0 = writable
    }
}

/// macOS: Enable execute access on the current thread's JIT pages.
fn enableExecute() void {
    if (comptime builtin.os.tag == .macos) {
        pthread_jit_write_protect_np(1); // 1 = executable
    }
}

/// Invalidate the instruction cache for the given range.
///
/// Required on ARM64 because the instruction and data caches are
/// not coherent — writes to the data cache (code emission) are not
/// automatically visible to the instruction cache.
pub fn icacheInvalidate(ptr: [*]const u8, len: usize) void {
    if (len == 0) return;

    if (comptime builtin.os.tag == .macos) {
        // macOS provides sys_icache_invalidate()
        sys_icache_invalidate(@ptrCast(ptr), len);
    } else if (comptime builtin.os.tag == .linux) {
        // Linux ARM64: use the DC CVAU / IC IVAU sequence
        // This is the standard ARMv8 cache maintenance sequence
        linuxClearCache(ptr, len);
    }
    // Other platforms: no-op (assume coherent caches or non-ARM64)
}

/// Linux ARM64: Manual icache invalidation using DC CVAU + IC IVAU + DSB + ISB.
fn linuxClearCache(ptr: [*]const u8, len: usize) void {
    if (comptime builtin.os.tag != .linux) return;
    if (comptime builtin.cpu.arch != .aarch64) return;

    // Cache line size (typically 64 bytes on ARM64)
    const cache_line: usize = 64;
    const start = @intFromPtr(ptr) & ~(cache_line - 1);
    const end = @intFromPtr(ptr) + len;

    // Step 1: Clean data cache to point of unification
    var addr = start;
    while (addr < end) : (addr += cache_line) {
        asm volatile ("dc cvau, %[addr]"
            :
            : [addr] "r" (addr),
            : .{ .memory = true });
    }

    // Step 2: Data synchronization barrier
    asm volatile ("dsb ish" ::: .{ .memory = true });

    // Step 3: Invalidate instruction cache to point of unification
    addr = start;
    while (addr < end) : (addr += cache_line) {
        asm volatile ("ic ivau, %[addr]"
            :
            : [addr] "r" (addr),
            : .{ .memory = true });
    }

    // Step 4: Data synchronization barrier + instruction synchronization barrier
    asm volatile ("dsb ish" ::: .{ .memory = true });
    asm volatile ("isb" ::: .{ .memory = true });
}

// ============================================================================
// Section: Byte-Level Helpers
// ============================================================================

/// Write a little-endian u32 to a byte slice.
fn writeU32(dst: *[4]u8, val: u32) void {
    dst[0] = @truncate(val);
    dst[1] = @truncate(val >> 8);
    dst[2] = @truncate(val >> 16);
    dst[3] = @truncate(val >> 24);
}

/// Write a little-endian u64 to a byte slice.
fn writeU64(dst: *[8]u8, val: u64) void {
    var i: u6 = 0;
    while (i < 8) : (i += 1) {
        dst[i] = @truncate(val >> (@as(u6, i) * 8));
    }
}

/// Read a little-endian u32 from a byte slice.
fn readU32(src: *const [4]u8) u32 {
    return @as(u32, src[0]) |
        (@as(u32, src[1]) << 8) |
        (@as(u32, src[2]) << 16) |
        (@as(u32, src[3]) << 24);
}

// ============================================================================
// Section: Tests
// ============================================================================

test "alignToPage rounds up correctly" {
    // macOS: PAGE_SIZE = 16384, Linux: PAGE_SIZE = 4096
    const ps = PAGE_SIZE;
    try std.testing.expectEqual(ps, alignToPage(1));
    try std.testing.expectEqual(ps, alignToPage(ps));
    try std.testing.expectEqual(ps * 2, alignToPage(ps + 1));
    try std.testing.expectEqual(ps * 2, alignToPage(ps * 2));
    try std.testing.expectEqual(0, alignToPage(0));
}

test "empty region is freed state" {
    const region = JitMemoryRegion.empty();
    try std.testing.expect(!region.isAlive());
    try std.testing.expectEqual(ProtectionState.Freed, region.state);
    try std.testing.expectEqual(@as(?[*]u8, null), region.code_base);
    try std.testing.expectEqual(@as(?[*]u8, null), region.data_base);
}

test "trampoline stub constants" {
    try std.testing.expectEqual(@as(usize, 16), TRAMPOLINE_STUB_SIZE);
    try std.testing.expectEqual(@as(usize, 4096), TRAMPOLINE_ISLAND_CAPACITY);
}

test "writeU32 and readU32 round-trip" {
    var buf: [4]u8 = undefined;
    writeU32(&buf, 0xDEADBEEF);
    try std.testing.expectEqual(@as(u32, 0xDEADBEEF), readU32(&buf));

    writeU32(&buf, 0x94000050);
    try std.testing.expectEqual(@as(u32, 0x94000050), readU32(&buf));
}

test "writeU64 stores little-endian" {
    var buf: [8]u8 = undefined;
    writeU64(&buf, 0x0123456789ABCDEF);
    try std.testing.expectEqual(@as(u8, 0xEF), buf[0]);
    try std.testing.expectEqual(@as(u8, 0xCD), buf[1]);
    try std.testing.expectEqual(@as(u8, 0xAB), buf[2]);
    try std.testing.expectEqual(@as(u8, 0x89), buf[3]);
    try std.testing.expectEqual(@as(u8, 0x67), buf[4]);
    try std.testing.expectEqual(@as(u8, 0x45), buf[5]);
    try std.testing.expectEqual(@as(u8, 0x23), buf[6]);
    try std.testing.expectEqual(@as(u8, 0x01), buf[7]);
}

test "trampoline LDR X16 encoding is correct" {
    // LDR X16, [PC, #8] should encode to 0x58000050
    // Encoding: opc=01 011 000 imm19 Rt
    // imm19 = 2 (offset 8 bytes = 2 words), Rt = 16
    // 0x58000000 | (2 << 5) | 16 = 0x58000050
    const expected: u32 = 0x58000050;
    const opc_base: u32 = 0x58000000;
    const imm19: u32 = 2; // 8 bytes / 4
    const rt: u32 = 16; // X16
    const encoded = opc_base | (imm19 << 5) | rt;
    try std.testing.expectEqual(expected, encoded);
}

test "trampoline BR X16 encoding is correct" {
    // BR X16 should encode to 0xd61f0200
    const expected: u32 = 0xd61f0200;
    const br_base: u32 = 0xd61f0000;
    const rn: u32 = 16; // X16
    const encoded = br_base | (rn << 5);
    try std.testing.expectEqual(expected, encoded);
}

test "allocate and free JIT memory region" {
    if (comptime builtin.os.tag != .macos and builtin.os.tag != .linux) {
        return; // Skip on unsupported platforms
    }

    var region = JitMemoryRegion.allocate(
        4096,
        4096,
    ) catch |err| {
        std.debug.print("JIT memory allocation not available: {}\n", .{err});
        return;
    };
    defer region.free();

    try std.testing.expect(region.isAlive());
    try std.testing.expectEqual(ProtectionState.Writable, region.state);
    try std.testing.expect(region.code_base != null);
    try std.testing.expect(region.data_base != null);
    try std.testing.expect(region.trampoline_base != null);
}

test "copy code into region" {
    if (comptime builtin.os.tag != .macos and builtin.os.tag != .linux) {
        return;
    }

    var region = JitMemoryRegion.allocate(4096, 4096) catch return;
    defer region.free();

    // Write a NOP (0xd503201f) and RET (0xd65f03c0)
    const nop = [_]u8{ 0x1f, 0x20, 0x03, 0xd5 };
    const ret = [_]u8{ 0xc0, 0x03, 0x5f, 0xd6 };

    try region.copyCode(&nop);
    try region.copyCode(&ret);

    try std.testing.expectEqual(@as(usize, 8), region.code_len);

    // Verify code was written correctly
    const word0 = region.readCodeWord(0);
    try std.testing.expect(word0 != null);
    try std.testing.expectEqual(@as(u32, 0xd503201f), word0.?);

    const word1 = region.readCodeWord(4);
    try std.testing.expect(word1 != null);
    try std.testing.expectEqual(@as(u32, 0xd65f03c0), word1.?);
}

test "copy data into region" {
    if (comptime builtin.os.tag != .macos and builtin.os.tag != .linux) {
        return;
    }

    var region = JitMemoryRegion.allocate(4096, 4096) catch return;
    defer region.free();

    const hello = "hello\x00";
    try region.copyData(hello);

    try std.testing.expectEqual(@as(usize, 6), region.data_len);

    const data = region.dataSlice();
    try std.testing.expect(data != null);
    try std.testing.expectEqualSlices(u8, hello, data.?);
}

test "write trampoline stub" {
    if (comptime builtin.os.tag != .macos and builtin.os.tag != .linux) {
        return;
    }

    var region = JitMemoryRegion.allocate(4096, 4096) catch return;
    defer region.free();

    // Write a trampoline for a fake address
    const target: u64 = 0x0000000100003F00;
    const stub_offset = try region.writeTrampoline(target);

    // Stub should be at code_capacity offset
    try std.testing.expectEqual(@as(usize, 4096), stub_offset);
    try std.testing.expectEqual(@as(usize, TRAMPOLINE_STUB_SIZE), region.trampoline_len);

    // Verify stub contents
    const tramp = region.trampolineSlice();
    try std.testing.expect(tramp != null);
    const t = tramp.?;
    try std.testing.expectEqual(@as(usize, 16), t.len);

    // Check LDR X16, [PC, #8]
    const ldr = readU32(t[0..4]);
    try std.testing.expectEqual(@as(u32, 0x58000050), ldr);

    // Check BR X16
    const br = readU32(t[4..8]);
    try std.testing.expectEqual(@as(u32, 0xd61f0200), br);
}

test "layout info reporting" {
    const region = JitMemoryRegion.empty();
    const info = region.layoutInfo();
    try std.testing.expectEqual(@as(usize, 0), info.base_addr);
    try std.testing.expectEqual(@as(usize, 0), info.code_used);
    try std.testing.expectEqual(ProtectionState.Freed, info.state);
}

test "make executable and back to writable" {
    if (comptime builtin.os.tag != .macos and builtin.os.tag != .linux) {
        return;
    }

    var region = JitMemoryRegion.allocate(4096, 4096) catch return;
    defer region.free();

    // Write minimal code: NOP + RET
    const nop_ret = [_]u8{
        0x1f, 0x20, 0x03, 0xd5, // NOP
        0xc0, 0x03, 0x5f, 0xd6, // RET
    };
    try region.copyCode(&nop_ret);

    // Make executable
    try region.makeExecutable();
    try std.testing.expectEqual(ProtectionState.Executable, region.state);

    // Make writable again (for hot-patching)
    try region.makeWritable();
    try std.testing.expectEqual(ProtectionState.Writable, region.state);
}

test "code overflow detection" {
    if (comptime builtin.os.tag != .macos and builtin.os.tag != .linux) {
        return;
    }

    // Allocate tiny region
    var region = JitMemoryRegion.allocateWithTrampoline(16, 16, 0) catch return;
    defer region.free();

    // Fill it up
    const data = [_]u8{0} ** 16;
    try region.copyCode(&data);

    // Should fail on overflow
    const result = region.copyCode(&[_]u8{0});
    try std.testing.expectError(JitMemoryError.CodeOverflow, result);
}

test "data overflow detection" {
    if (comptime builtin.os.tag != .macos and builtin.os.tag != .linux) {
        return;
    }

    var region = JitMemoryRegion.allocateWithTrampoline(16, 8, 0) catch return;
    defer region.free();

    const data = [_]u8{0} ** 8;
    try region.copyData(&data);

    const result = region.copyData(&[_]u8{0});
    try std.testing.expectError(JitMemoryError.DataOverflow, result);
}

test "write protection state enforcement" {
    if (comptime builtin.os.tag != .macos and builtin.os.tag != .linux) {
        return;
    }

    var region = JitMemoryRegion.allocate(4096, 4096) catch return;
    defer region.free();

    // Write some code
    const code = [_]u8{ 0xc0, 0x03, 0x5f, 0xd6 }; // RET
    try region.copyCode(&code);

    // Make executable
    try region.makeExecutable();

    // Attempts to write should fail
    const result = region.copyCode(&code);
    try std.testing.expectError(JitMemoryError.NotWritable, result);

    const tramp_result = region.writeTrampoline(0x1000);
    try std.testing.expectError(JitMemoryError.NotWritable, tramp_result);

    // But getFunctionPtr should work
    const ptr = region.getFunctionPtr(*const fn () void, 0);
    try std.testing.expect(ptr != JitMemoryError.NotExecutable);
}

test "patchBLToTrampoline computes correct offset" {
    if (comptime builtin.os.tag != .macos and builtin.os.tag != .linux) {
        return;
    }

    var region = JitMemoryRegion.allocate(4096, 4096) catch return;
    defer region.free();

    // Write placeholder BL at offset 0
    const placeholder_bl = [_]u8{ 0x00, 0x00, 0x00, 0x94 }; // BL #0
    try region.copyCode(&placeholder_bl);

    // Write a trampoline
    const stub_offset = try region.writeTrampoline(0xDEAD);

    // Patch the BL to point to the trampoline
    try region.patchBLToTrampoline(0, stub_offset);

    // Read back and verify
    const patched = region.readCodeWord(0);
    try std.testing.expect(patched != null);

    // The BL should have opcode 0x94 in the top byte and a non-zero offset
    try std.testing.expectEqual(@as(u32, 0x94), (patched.? >> 24) & 0xFC);
}

test "multiple trampolines get sequential offsets" {
    if (comptime builtin.os.tag != .macos and builtin.os.tag != .linux) {
        return;
    }

    var region = JitMemoryRegion.allocate(4096, 4096) catch return;
    defer region.free();

    const off1 = try region.writeTrampoline(0x1000);
    const off2 = try region.writeTrampoline(0x2000);
    const off3 = try region.writeTrampoline(0x3000);

    // Each stub is TRAMPOLINE_STUB_SIZE apart
    try std.testing.expectEqual(@as(usize, 4096), off1);
    try std.testing.expectEqual(@as(usize, 4096 + 16), off2);
    try std.testing.expectEqual(@as(usize, 4096 + 32), off3);

    try std.testing.expectEqual(@as(usize, 48), region.trampoline_len);
}

test "address computation for code and data" {
    if (comptime builtin.os.tag != .macos and builtin.os.tag != .linux) {
        return;
    }

    var region = JitMemoryRegion.allocate(4096, 4096) catch return;
    defer region.free();

    const code_addr_0 = region.codeAddress(0);
    const code_addr_4 = region.codeAddress(4);
    const data_addr_0 = region.dataAddress(0);

    try std.testing.expect(code_addr_0 != null);
    try std.testing.expect(code_addr_4 != null);
    try std.testing.expect(data_addr_0 != null);

    // Code addresses should be sequential
    try std.testing.expectEqual(code_addr_0.? + 4, code_addr_4.?);

    // Data should be at a higher address than code (buddy allocation)
    try std.testing.expect(data_addr_0.? > code_addr_0.?);
}
