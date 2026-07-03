//! Platform-specific JIT memory allocation and management.
//!
//! Rust port of FasterBASIC's `jit_memory.zig`
//! (`zig_compiler/src/jit_memory.zig`). Implements the executable memory
//! allocation strategy from MACVM's own `docs/arm64.md`, which already
//! specifies `MAP_JIT` + `pthread_jit_write_protect_np` on Apple Silicon
//! for exactly this purpose — this module matches that plan rather than
//! inventing a new one:
//!
//!   - **Buddy allocation**: reserve contiguous VA space, then commit
//!     separate Code (RX) and Data (RW) regions with guaranteed proximity
//!     for ADRP (±4GB) addressing.
//!
//!   - **W^X compliance**: macOS Apple Silicon requires `MAP_JIT` and
//!     `pthread_jit_write_protect_np` toggling. Linux uses mmap + mprotect.
//!
//!   - **Instruction cache invalidation**: required after writing code,
//!     before execution, on ARM64.
//!
//! Architecture:
//! ```text
//!   JitModule (encoded buffers)
//!     -> JitMemoryRegion::allocate()
//!       -> copy_code() / copy_data()
//!       -> link/relocate (via the linker module)
//!       -> make_executable()
//!       -> icache_invalidate()
//!       -> execute via function pointer cast
//!     -> JitMemoryRegion dropped (frees mappings)
//! ```
//!
//! The memory layout follows the "buddy allocation" technique:
//!
//! ```text
//!   [      Virtual Address Space Reservation (total_size)      ]
//!   |--------------------|--------------------------------------|
//!   |  Code (RX) + Lit   |          Data (RW)                   |
//!   |  (MAP_JIT on macOS)|          (Standard mmap)             |
//!   |--------------------|--------------------------------------|
//!   ^                    ^
//!   base                 base + code_capacity
//! ```
//!
//! This guarantees that ADRP from any code instruction can reach any data
//! address (they're in the same contiguous VA range).
//!
//! The macOS path is exercised and tested on this machine (Darwin arm64).
//! The Linux path is ported faithfully from the Zig source but is
//! unverified here — there is no Linux CI/dev machine in this repo at
//! present.

#![allow(dead_code)]

use libc::{c_void, size_t};
use std::ptr::NonNull;

// ============================================================================
// Section: Platform-Specific Extern Declarations
// ============================================================================

// `pthread_jit_write_protect_np` is exposed by the `libc` crate itself
// (`src/unix/bsd/apple/mod.rs`) for `target_os = "macos"`, so it is used
// directly as `libc::pthread_jit_write_protect_np` rather than redeclared
// here — redeclaring an already-correct extern would just be duplicate
// surface to keep in sync.
//
// `sys_icache_invalidate` is a real libSystem entry point but is *not*
// exposed by the `libc` crate, so it is declared by hand below. No extra
// linking is required: libSystem is linked into every macOS binary by
// default.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// macOS: invalidate the instruction cache for `[addr, addr+len)`.
    fn sys_icache_invalidate(addr: *const c_void, len: size_t);
}

// ============================================================================
// Section: Constants
// ============================================================================

/// Default code region capacity (128 KiB — plenty for most JIT programs).
pub const DEFAULT_CODE_CAPACITY: usize = 128 * 1024;

/// Default data region capacity (64 KiB).
pub const DEFAULT_DATA_CAPACITY: usize = 64 * 1024;

/// Trampoline stub size: `LDR X16, [PC, #8]` + `BR X16` + `.quad addr` = 16 bytes.
pub const TRAMPOLINE_STUB_SIZE: usize = 16;

/// Maximum number of external symbols we support trampolines for.
pub const MAX_TRAMPOLINE_STUBS: usize = 256;

/// Reserved trampoline island capacity.
pub const TRAMPOLINE_ISLAND_CAPACITY: usize = TRAMPOLINE_STUB_SIZE * MAX_TRAMPOLINE_STUBS;

/// Page size used for alignment. Mirrors the Zig source's use of
/// `std.heap.page_size_min`: macOS ARM64 pages are 16 KiB, Linux (and
/// everything else this module might plausibly run on) uses the
/// conventional 4 KiB. This is a compile-time constant, matching the Zig
/// original, rather than a `sysconf` runtime query.
#[cfg(target_os = "macos")]
pub const PAGE_SIZE: usize = 16384;
#[cfg(not(target_os = "macos"))]
pub const PAGE_SIZE: usize = 4096;

// PROT/MAP constants come from `libc` (`PROT_NONE`, `PROT_READ`,
// `PROT_WRITE`, `PROT_EXEC`, `MAP_PRIVATE`, `MAP_ANON`). `libc::MAP_JIT`
// (`target_os = "macos"`) is present as of libc 0.2.186 (`= 0x0800`),
// which is what this crate's Cargo.lock resolves to, so it is used
// directly rather than hand-declared.

// ============================================================================
// Section: Error Type
// ============================================================================

/// Mirrors the Zig source's `JitMemoryError` error set 1:1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryError {
    /// Target address is beyond the ±128 MB `BL` instruction range.
    BLOutOfRange,
    /// Failed to reserve virtual address space.
    ReserveFailed,
    /// Failed to commit the code region.
    CodeCommitFailed,
    /// Failed to commit the data region.
    DataCommitFailed,
    /// Failed to set memory protection.
    ProtectFailed,
    /// Code buffer overflow.
    CodeOverflow,
    /// Data buffer overflow.
    DataOverflow,
    /// Trampoline island overflow.
    TrampolineOverflow,
    /// Region not in writable state.
    NotWritable,
    /// Region not in executable state.
    NotExecutable,
    /// Invalid alignment.
    InvalidAlignment,
    /// Region already freed.
    AlreadyFreed,
    /// Platform not supported for JIT execution.
    UnsupportedPlatform,
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            Self::BLOutOfRange => "target address is beyond +/-128MB BL instruction range",
            Self::ReserveFailed => "failed to reserve virtual address space",
            Self::CodeCommitFailed => "failed to commit code region",
            Self::DataCommitFailed => "failed to commit data region",
            Self::ProtectFailed => "failed to set memory protection",
            Self::CodeOverflow => "code buffer overflow",
            Self::DataOverflow => "data buffer overflow",
            Self::TrampolineOverflow => "trampoline island overflow",
            Self::NotWritable => "region not in writable state",
            Self::NotExecutable => "region not in executable state",
            Self::InvalidAlignment => "invalid alignment",
            Self::AlreadyFreed => "region already freed",
            Self::UnsupportedPlatform => "platform not supported for JIT execution",
        };
        f.write_str(msg)
    }
}
impl std::error::Error for MemoryError {}

// ============================================================================
// Section: Protection State
// ============================================================================

/// Tracks the current W^X state of the code region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectionState {
    /// Initial state — region is writable for code emission.
    Writable,
    /// Region is executable — code can be called but not written.
    Executable,
    /// Region has been freed.
    Freed,
}

// ============================================================================
// Section: JitMemoryRegion
// ============================================================================

/// A JIT memory region that manages code and data segments with proper
/// W^X compliance and ADRP-reachable proximity guarantees.
///
/// Usage:
///   1. [`JitMemoryRegion::allocate`] — reserve and commit memory.
///   2. [`JitMemoryRegion::copy_code`] / [`JitMemoryRegion::copy_data`] —
///      write encoded bytes (region is `Writable`).
///   3. [`JitMemoryRegion::make_executable`] — switch to RX mode + flush
///      icache.
///   4. [`JitMemoryRegion::get_function_ptr`] — get a callable function
///      pointer.
///   5. Drop — release all memory (mirrors the Zig source's explicit
///      `free()`, but wired through `Drop` so it happens automatically).
pub struct JitMemoryRegion {
    /// Base of the entire reserved VA range (code + trampolines; on macOS
    /// this is the `MAP_JIT` region only — data lives in a separate mmap).
    base: Option<NonNull<u8>>,

    /// Total reserved VA size of the base mapping (code + trampolines).
    /// On Linux this also includes data; on macOS data is separate.
    total_size: usize,

    // ── Code segment ──────────────────────────────────────────────
    /// Start of the code region (same as `base`).
    code_base: Option<NonNull<u8>>,

    /// Capacity of the code region in bytes.
    code_capacity: usize,

    /// Current write offset in the code region.
    code_len: usize,

    // ── Trampoline island ─────────────────────────────────────────
    /// Start of the trampoline island (at end of code region capacity).
    trampoline_base: Option<NonNull<u8>>,

    /// Capacity reserved for trampolines.
    trampoline_capacity: usize,

    /// Current write offset in the trampoline island.
    trampoline_len: usize,

    // ── Data segment ──────────────────────────────────────────────
    /// Start of the data region.
    data_base: Option<NonNull<u8>>,

    /// Capacity of the data region in bytes.
    data_capacity: usize,

    /// Current write offset in the data region.
    data_len: usize,

    // ── Separate data mmap (macOS) ────────────────────────────────
    /// On macOS the data section lives in a separate non-`MAP_JIT` mmap so
    /// it stays RW while the code region is toggled to RX for execution.
    /// On Linux (or when data shares the base mapping) these are
    /// `None` / `0`.
    data_mmap_base: Option<NonNull<u8>>,
    data_mmap_size: usize,

    // ── State tracking ────────────────────────────────────────────
    /// Current W^X protection state.
    state: ProtectionState,

    /// Whether this is running on macOS (uses `MAP_JIT` +
    /// `pthread_jit_write_protect_np`).
    is_macos: bool,
}

impl JitMemoryRegion {
    /// Allocate a JIT memory region with the specified capacities.
    ///
    /// On macOS: uses a single `MAP_JIT` mmap for the entire region (code +
    /// trampolines) plus a separate regular mmap for data. This guarantees
    /// contiguous VA space with ADRP reachability for code+trampolines,
    /// and W^X compliance via `pthread_jit_write_protect_np`.
    ///
    /// On Linux: uses the "buddy allocation" technique — reserve
    /// contiguous VA with `PROT_NONE`, then commit code (RW) and data (RW)
    /// sub-regions via `mprotect`. The code region is switched to RX
    /// before execution.
    ///
    /// The trampoline island is allocated at the end of the code region's
    /// capacity, ensuring all `BL` targets are within ±128 MB.
    pub fn allocate(code_cap: usize, data_cap: usize) -> Result<JitMemoryRegion, MemoryError> {
        Self::allocate_with_trampoline(code_cap, data_cap, TRAMPOLINE_ISLAND_CAPACITY)
    }

    /// Like [`allocate`](Self::allocate) but with explicit trampoline
    /// capacity.
    pub fn allocate_with_trampoline(
        code_cap: usize,
        data_cap: usize,
        trampoline_cap: usize,
    ) -> Result<JitMemoryRegion, MemoryError> {
        let is_macos = cfg!(target_os = "macos");

        // Round capacities up to page boundaries.
        let code_pages = align_to_page(code_cap + trampoline_cap);
        let data_pages = align_to_page(data_cap);
        let total = code_pages + data_pages;

        if is_macos {
            Self::allocate_macos(code_cap, data_cap, trampoline_cap, code_pages)
        } else {
            Self::allocate_linux(code_cap, data_cap, trampoline_cap, code_pages, data_pages, total)
        }
    }

    /// macOS allocation: two separate mmaps.
    ///
    /// 1. Code + Trampolines -> `MAP_JIT` mmap (W^X via
    ///    `pthread_jit_write_protect_np`).
    /// 2. Data -> regular mmap (always `PROT_READ | PROT_WRITE`).
    ///
    /// `pthread_jit_write_protect_np` toggles write permission for ALL
    /// `MAP_JIT` pages on the current thread. If the data section lived in
    /// the same `MAP_JIT` region, calling `make_executable()` would make
    /// data non-writable and any store into a global variable would fault.
    /// Separating them ensures data stays RW while code is RX.
    #[cfg(target_os = "macos")]
    fn allocate_macos(
        code_cap: usize,
        data_cap: usize,
        trampoline_cap: usize,
        code_pages: usize,
    ) -> Result<JitMemoryRegion, MemoryError> {
        // 1. Code + Trampolines: MAP_JIT mmap.
        //
        // SAFETY: `null` addr hint + `MAP_PRIVATE | MAP_ANON | MAP_JIT` with
        // fd -1 and offset 0 is the documented anonymous-mapping form of
        // mmap(2); it has no aliasing preconditions to uphold since it
        // creates brand-new memory rather than referencing existing Rust
        // objects. The result is checked against `MAP_FAILED` below before
        // any pointer derived from it is used.
        let code_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                code_pages as size_t,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
                -1,
                0,
            )
        };
        if code_ptr == libc::MAP_FAILED {
            return Err(MemoryError::ReserveFailed);
        }
        // SAFETY: just checked `code_ptr != MAP_FAILED`; mmap never returns
        // null on success, so this is non-null.
        let code_base = unsafe { NonNull::new_unchecked(code_ptr as *mut u8) };

        // 2. Data: regular (non-MAP_JIT) mmap — always RW.
        let data_pages = align_to_page(data_cap);
        // SAFETY: same anonymous-mapping contract as the code mapping
        // above; independent of it (no aliasing with `code_ptr`).
        let data_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                data_pages as size_t,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if data_ptr == libc::MAP_FAILED {
            // Clean up the code mapping on failure.
            // SAFETY: `code_base`/`code_pages` describe exactly the mapping
            // just created above and not yet unmapped or referenced
            // elsewhere.
            unsafe { release_va(code_base, code_pages) };
            return Err(MemoryError::DataCommitFailed);
        }
        // SAFETY: just checked `data_ptr != MAP_FAILED`.
        let data_base = unsafe { NonNull::new_unchecked(data_ptr as *mut u8) };

        // Enable write access for initial code emission.
        enable_write();

        // SAFETY: `code_base` is a valid mmap'd allocation of at least
        // `code_cap` bytes; `code_cap <= code_pages`, so `code_base +
        // code_cap` stays within (or exactly at the end of) that mapping,
        // making it a valid (one-past-the-end-permitted) pointer to derive.
        let trampoline_base = unsafe { NonNull::new_unchecked(code_base.as_ptr().add(code_cap)) };

        Ok(JitMemoryRegion {
            base: Some(code_base),
            total_size: code_pages,
            code_base: Some(code_base),
            code_capacity: code_cap,
            code_len: 0,
            trampoline_base: Some(trampoline_base),
            trampoline_capacity: trampoline_cap,
            trampoline_len: 0,
            data_base: Some(data_base),
            data_capacity: data_cap,
            data_len: 0,
            data_mmap_base: Some(data_base),
            data_mmap_size: data_pages,
            state: ProtectionState::Writable,
            is_macos: true,
        })
    }

    #[cfg(not(target_os = "macos"))]
    fn allocate_macos(
        _code_cap: usize,
        _data_cap: usize,
        _trampoline_cap: usize,
        _code_pages: usize,
    ) -> Result<JitMemoryRegion, MemoryError> {
        Err(MemoryError::UnsupportedPlatform)
    }

    /// Linux allocation: reserve contiguous VA, then commit sub-regions.
    ///
    /// Ported from the Zig source's Linux path. Unverified on real
    /// hardware in this repo (no Linux dev/CI machine here yet) — the
    /// macOS path above is what is actually exercised by the test suite.
    #[cfg(target_os = "linux")]
    fn allocate_linux(
        code_cap: usize,
        data_cap: usize,
        trampoline_cap: usize,
        code_pages: usize,
        data_pages: usize,
        total: usize,
    ) -> Result<JitMemoryRegion, MemoryError> {
        // Step 1: Reserve contiguous VA space with PROT_NONE.
        // SAFETY: anonymous PROT_NONE reservation; see `reserve_va`.
        let base = match unsafe { reserve_va(total) } {
            Some(b) => b,
            None => return Err(MemoryError::ReserveFailed),
        };

        // Step 2: Commit code region (RW initially, switched to RX before
        // exec).
        // SAFETY: `base`/`code_pages` describe the leading sub-range of the
        // reservation just made above, still PROT_NONE and not yet
        // committed or referenced elsewhere.
        if unsafe { commit_region_rw(base, code_pages) }.is_err() {
            // SAFETY: `base`/`total` describe exactly the reservation made
            // above, not yet unmapped.
            unsafe { release_va(base, total) };
            return Err(MemoryError::CodeCommitFailed);
        }

        // Step 3: Commit data region (RW, stays RW).
        // SAFETY: `base + code_pages` lands exactly at the start of the
        // reservation's second sub-range (`total == code_pages +
        // data_pages`), still within the single `total`-byte mapping.
        let data_ptr = unsafe { NonNull::new_unchecked(base.as_ptr().add(code_pages)) };
        // SAFETY: `data_ptr`/`data_pages` describe the trailing sub-range
        // of the reservation, still PROT_NONE and not yet committed.
        if unsafe { commit_region_rw(data_ptr, data_pages) }.is_err() {
            // SAFETY: `base`/`total` describe exactly the reservation made
            // above, not yet unmapped.
            unsafe { release_va(base, total) };
            return Err(MemoryError::DataCommitFailed);
        }

        // SAFETY: `base + code_cap` is within the code sub-range
        // (`code_cap <= code_pages`), a mapping that now exists and is
        // committed RW.
        let trampoline_base = unsafe { NonNull::new_unchecked(base.as_ptr().add(code_cap)) };

        Ok(JitMemoryRegion {
            base: Some(base),
            total_size: total,
            code_base: Some(base),
            code_capacity: code_cap,
            code_len: 0,
            trampoline_base: Some(trampoline_base),
            trampoline_capacity: trampoline_cap,
            trampoline_len: 0,
            data_base: Some(data_ptr),
            data_capacity: data_cap,
            data_len: 0,
            data_mmap_base: None,
            data_mmap_size: 0,
            state: ProtectionState::Writable,
            is_macos: false,
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn allocate_linux(
        _code_cap: usize,
        _data_cap: usize,
        _trampoline_cap: usize,
        _code_pages: usize,
        _data_pages: usize,
        _total: usize,
    ) -> Result<JitMemoryRegion, MemoryError> {
        Err(MemoryError::UnsupportedPlatform)
    }

    /// Copy encoded machine code into the code region.
    /// Region must be in `Writable` state.
    pub fn copy_code(&mut self, code: &[u8]) -> Result<(), MemoryError> {
        if self.state != ProtectionState::Writable {
            return Err(MemoryError::NotWritable);
        }
        if self.code_len + code.len() > self.code_capacity {
            return Err(MemoryError::CodeOverflow);
        }
        let dst = self.code_base.ok_or(MemoryError::AlreadyFreed)?;
        // SAFETY: `dst` is the live code mapping; `code_len + code.len() <=
        // code_capacity` was just checked, so `[code_len, code_len +
        // code.len())` is within the mapped, writable region (state ==
        // Writable, checked above). `code` is a valid `&[u8]` of its own
        // length. The two ranges don't overlap since `dst` is our own mmap
        // allocation, not derived from `code`.
        unsafe {
            std::ptr::copy_nonoverlapping(
                code.as_ptr(),
                dst.as_ptr().add(self.code_len),
                code.len(),
            );
        }
        self.code_len += code.len();
        Ok(())
    }

    /// Copy data section bytes into the data region.
    pub fn copy_data(&mut self, data: &[u8]) -> Result<(), MemoryError> {
        if self.data_len + data.len() > self.data_capacity {
            return Err(MemoryError::DataOverflow);
        }
        let dst = self.data_base.ok_or(MemoryError::AlreadyFreed)?;
        // SAFETY: `data_len + data.len() <= data_capacity` was just
        // checked, and the data region is always RW (macOS: separate
        // regular mmap never toggled; Linux: committed RW and only the
        // code sub-range is ever mprotect'd to RX), so this write is
        // always valid regardless of `state`.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                dst.as_ptr().add(self.data_len),
                data.len(),
            );
        }
        self.data_len += data.len();
        Ok(())
    }

    /// Overwrite an already-`copy_data`'d 8-byte slot in place. No W^X
    /// toggle needed — see `copy_data`'s SAFETY note: the data region is
    /// always RW, unlike the code+trampoline region `make_writable`/
    /// `make_executable` gate.
    ///
    /// This is the primitive a moving GC uses to update an embedded oop
    /// after relocating the object it points to: route the oop through a
    /// data-section slot (see `crate::patch`'s module doc) instead of
    /// materializing it as an immediate in the instruction stream, and a
    /// GC move becomes this one bounds-checked memory write — no
    /// instruction re-encoding, no icache invalidation (data isn't
    /// fetched through the instruction cache).
    pub fn write_data_u64(&mut self, offset: usize, value: u64) -> Result<(), MemoryError> {
        if offset + 8 > self.data_len {
            return Err(MemoryError::DataOverflow);
        }
        let dst = self.data_base.ok_or(MemoryError::AlreadyFreed)?;
        // SAFETY: `offset + 8 <= data_len <= data_capacity` just checked;
        // the data region is always RW regardless of `state` (see
        // `copy_data`'s SAFETY note above).
        unsafe { write_u64(dst.as_ptr().add(offset), value) };
        Ok(())
    }

    /// Read an 8-byte slot from the data region. Counterpart to
    /// [`Self::write_data_u64`], mainly for tests/diagnostics — GC code
    /// reading back what it just wrote has no need for this if it already
    /// knows the value.
    pub fn read_data_u64(&self, offset: usize) -> Option<u64> {
        if offset + 8 > self.data_len {
            return None;
        }
        let data = self.data_base?;
        // SAFETY: `offset + 8 <= data_len`, just checked; `data_len` bytes
        // starting at `data_base` are initialized (see `data_slice`'s
        // SAFETY note).
        Some(unsafe { read_u64(data.as_ptr().add(offset)) })
    }

    /// Write a single trampoline stub into the trampoline island.
    /// Returns the byte offset of the stub relative to `code_base`.
    ///
    /// Stub layout (16 bytes):
    /// ```text
    ///   +0: LDR X16, [PC, #8]   -> 0x58000050
    ///   +4: BR  X16              -> 0xd61f0200
    ///   +8: .quad <target_addr>  -> 64-bit absolute address
    /// ```
    pub fn write_trampoline(&mut self, target_addr: u64) -> Result<usize, MemoryError> {
        if self.state != ProtectionState::Writable {
            return Err(MemoryError::NotWritable);
        }
        if self.trampoline_len + TRAMPOLINE_STUB_SIZE > self.trampoline_capacity {
            return Err(MemoryError::TrampolineOverflow);
        }
        let tramp = self.trampoline_base.ok_or(MemoryError::AlreadyFreed)?;
        let off = self.trampoline_len;

        // SAFETY: `tramp` is the live trampoline sub-range of the code
        // mapping (state == Writable, checked above); `off + 16 <=
        // trampoline_capacity` was just checked, and the trampoline island
        // itself sits within `code_base`'s mapping (allocated as
        // `code_capacity + trampoline_capacity` pages), so bytes
        // `[off, off+16)` are in bounds and writable.
        unsafe {
            let p = tramp.as_ptr().add(off);
            // LDR X16, [PC, #8] — load address from 8 bytes ahead.
            write_u32(p, 0x58000050);
            // BR X16 — indirect branch to loaded address.
            write_u32(p.add(4), 0xd61f0200);
            // .quad target_addr — 64-bit absolute address.
            write_u64(p.add(8), target_addr);
        }

        self.trampoline_len += TRAMPOLINE_STUB_SIZE;

        // Return absolute offset from code_base.
        Ok(self.code_capacity + off)
    }

    /// Write a trap stub into the trampoline island for an unresolved
    /// symbol. Returns the byte offset of the stub relative to
    /// `code_base`.
    ///
    /// Trap stub layout (16 bytes — same size as a trampoline):
    /// ```text
    ///   +0: BRK #0xF001   -> 0xD43E0020  (signals "unresolved JIT symbol")
    ///   +4: BRK #0xF001   -> 0xD43E0020  (redundant, for alignment)
    ///   +8: .quad 0xDEAD  -> sentinel value (never executed)
    /// ```
    ///
    /// When JIT code `BL`s to this stub, the CPU hits `BRK` immediately,
    /// producing a clean `SIGTRAP` instead of a null-pointer `SIGSEGV`.
    pub fn write_trap_stub(&mut self) -> Result<usize, MemoryError> {
        if self.state != ProtectionState::Writable {
            return Err(MemoryError::NotWritable);
        }
        if self.trampoline_len + TRAMPOLINE_STUB_SIZE > self.trampoline_capacity {
            return Err(MemoryError::TrampolineOverflow);
        }
        let tramp = self.trampoline_base.ok_or(MemoryError::AlreadyFreed)?;
        let off = self.trampoline_len;

        // BRK #0xF001. ARM64's "exception generation" class needs opc=001
        // in bits[23:21] to mean BRK specifically (000 there is SVC's
        // slot, and with LL=00 as used here, unallocated — executing it
        // raises an Undefined Instruction exception, i.e. SIGILL, not the
        // intended SIGTRAP). That opc bit lives at bit 21, i.e. 0x0020_0000
        // — base must be 0xD420_0000, not 0xD400_0000. This was wrong in
        // the original jit_memory.zig too (its own doc comment states the
        // correct 0xD43E0020 result right above code that computed
        // 0xD41E0020) — found by actually executing the trap path while
        // investigating the arm64 register-allocation bug in
        // ../aot/known_issues/BUG_REPORT.md (unrelated: that one did not
        // reproduce here, see jit/rust/tests/register_bug_investigation.rs).
        let brk_word: u32 = 0xD420_0000 | (0xF001 << 5);

        // SAFETY: same bounds/writability argument as write_trampoline
        // above — `off + 16 <= trampoline_capacity`, checked just above.
        unsafe {
            let p = tramp.as_ptr().add(off);
            write_u32(p, brk_word);
            write_u32(p.add(4), brk_word);
            // .quad 0xDEAD — sentinel, never reached.
            write_u64(p.add(8), 0xDEAD);
        }

        self.trampoline_len += TRAMPOLINE_STUB_SIZE;

        // Return absolute offset from code_base.
        Ok(self.code_capacity + off)
    }

    /// Patch a `BL` instruction at the given code offset to branch
    /// directly to an absolute target address. Returns
    /// [`MemoryError::BLOutOfRange`] if the target is beyond ±128 MB from
    /// the `BL` instruction.
    pub fn patch_bl_direct(&mut self, bl_offset: usize, target_addr: u64) -> Result<(), MemoryError> {
        if self.state != ProtectionState::Writable {
            return Err(MemoryError::NotWritable);
        }
        let code = self.code_base.ok_or(MemoryError::AlreadyFreed)?;

        // Absolute address of the BL instruction.
        let bl_addr = code.as_ptr() as u64 + bl_offset as u64;

        // Signed byte delta from BL to target.
        let delta_bytes = target_addr as i64 - bl_addr as i64;

        // BL range: imm26 is signed, scaled by 4 -> +/-128 MB.
        const BL_MAX: i64 = 128 * 1024 * 1024;
        if delta_bytes >= BL_MAX || delta_bytes < -BL_MAX {
            return Err(MemoryError::BLOutOfRange);
        }

        // Must be 4-byte aligned.
        if delta_bytes % 4 != 0 {
            return Err(MemoryError::InvalidAlignment);
        }

        let delta_words = (delta_bytes / 4) as i32;
        let delta_u32 = delta_words as u32;

        // BL encoding: opcode[31:26] = 100101, imm26[25:0].
        let bl_word: u32 = 0x94000000 | (delta_u32 & 0x03ff_ffff);

        // SAFETY: `state == Writable` was checked above, and `bl_offset`
        // is documented (matching the Zig original) as a caller-supplied
        // offset into already-emitted code — i.e. `bl_offset + 4 <=
        // code_len <= code_capacity`, within `code`'s mapping. This
        // mirrors the Zig source's own lack of an explicit bounds check
        // here (`code[bl_offset..][0..4]` would panic in Zig too if out of
        // range); the contract is caller-supplied valid offsets.
        unsafe {
            write_u32(code.as_ptr().add(bl_offset), bl_word);
        }
        Ok(())
    }

    /// Patch a `BL` instruction at the given code offset to branch to a
    /// trampoline stub at the given stub offset (both relative to
    /// `code_base`).
    pub fn patch_bl_to_trampoline(
        &mut self,
        bl_offset: usize,
        stub_offset: usize,
    ) -> Result<(), MemoryError> {
        if self.state != ProtectionState::Writable {
            return Err(MemoryError::NotWritable);
        }
        let code = self.code_base.ok_or(MemoryError::AlreadyFreed)?;

        // Compute delta in instruction words.
        let delta_bytes = stub_offset as i64 - bl_offset as i64;
        let delta_words = (delta_bytes / 4) as i32;
        let delta_u32 = delta_words as u32;

        // BL encoding: opcode[31:26] = 100101, imm26[25:0].
        let bl_word: u32 = 0x94000000 | (delta_u32 & 0x03ff_ffff);

        // SAFETY: see patch_bl_direct — same caller contract (valid,
        // already-emitted `bl_offset` within the writable code mapping).
        unsafe {
            write_u32(code.as_ptr().add(bl_offset), bl_word);
        }
        Ok(())
    }

    /// Patch an `ADRP` instruction at the given code offset with the
    /// actual page delta, and patch the following `ADD` with the page
    /// offset.
    ///
    /// This resolves `LOAD_ADDR` relocations once real addresses are
    /// known.
    pub fn patch_adrp_add(&mut self, adrp_offset: usize, target_addr: u64) -> Result<(), MemoryError> {
        if self.state != ProtectionState::Writable {
            return Err(MemoryError::NotWritable);
        }
        let code = self.code_base.ok_or(MemoryError::AlreadyFreed)?;

        // Compute ADRP page delta.
        let pc_addr = code.as_ptr() as usize + adrp_offset;
        let pc_page = (pc_addr as u64) & !0xFFFu64;
        let target_page = target_addr & !0xFFFu64;

        let page_delta_bytes = target_page as i64 - pc_page as i64;
        let page_delta = (page_delta_bytes / 4096) as i32;

        let page_offset: u32 = (target_addr & 0xFFF) as u32;

        // SAFETY: `state == Writable` was checked above; `adrp_offset` and
        // `adrp_offset + 4` (the following ADD) are caller-supplied
        // offsets into already-emitted code, matching the Zig source's own
        // implicit contract (no bounds check there either).
        unsafe {
            let p = code.as_ptr().add(adrp_offset);

            // Read existing ADRP to preserve Rd.
            let existing_adrp = read_u32(p);
            let rd_bits = existing_adrp & 0x1F;

            // Encode ADRP: 1 immlo[1:0] 10000 immhi[18:0] Rd[4:0].
            let delta_u32 = page_delta as u32;
            let immlo = delta_u32 & 0x3;
            let immhi = (delta_u32 >> 2) & 0x7FFFF;
            let adrp_word: u32 = 0x90000000 | (immlo << 29) | (immhi << 5) | rd_bits;
            write_u32(p, adrp_word);

            // Patch ADD immediately after ADRP.
            let add_p = p.add(4);
            let existing_add = read_u32(add_p);
            // Preserve Rd and Rn, patch imm12.
            let add_base = existing_add & 0xFFC0_03FF; // clear imm12 field [21:10]
            let add_word = add_base | (page_offset << 10);
            write_u32(add_p, add_word);
        }
        Ok(())
    }

    /// Switch the code region from `Writable` to `Executable`.
    ///
    /// This:
    ///   1. On macOS: calls `pthread_jit_write_protect_np(1)` to enable X.
    ///   2. On Linux: calls `mprotect(PROT_READ | PROT_EXEC)`.
    ///   3. Invalidates the instruction cache.
    pub fn make_executable(&mut self) -> Result<(), MemoryError> {
        if self.state == ProtectionState::Freed {
            return Err(MemoryError::AlreadyFreed);
        }
        let base = self.base.ok_or(MemoryError::AlreadyFreed)?;
        let code_pages = align_to_page(self.code_capacity + self.trampoline_capacity);

        if self.is_macos {
            enable_execute();
        } else {
            // SAFETY: `base`/`code_pages` describe exactly the code
            // sub-mapping committed at allocation time (Linux path); no
            // other reference is mutated concurrently (single-threaded
            // access to `self` is required by `&mut self`).
            let rc = unsafe {
                libc::mprotect(
                    base.as_ptr() as *mut c_void,
                    code_pages as size_t,
                    libc::PROT_READ | libc::PROT_EXEC,
                )
            };
            if rc != 0 {
                return Err(MemoryError::ProtectFailed);
            }
        }

        // Invalidate instruction cache for the code + trampoline region.
        let total_code_len =
            self.code_len + self.trampoline_len + (self.code_capacity - self.code_len); // include gap to trampolines

        // SAFETY: `base` is valid for `total_code_len` bytes — that value
        // is bounded by `code_capacity + trampoline_capacity`, which is
        // exactly the range the code mapping was sized for at allocation.
        unsafe {
            icache_invalidate(base.as_ptr(), total_code_len);
        }

        self.state = ProtectionState::Executable;
        Ok(())
    }

    /// Switch the code region back to `Writable` (for hot-patching).
    ///
    /// Used for breakpoint insertion, trampoline updates, etc.
    pub fn make_writable(&mut self) -> Result<(), MemoryError> {
        if self.state == ProtectionState::Freed {
            return Err(MemoryError::AlreadyFreed);
        }
        let base = self.base.ok_or(MemoryError::AlreadyFreed)?;
        let code_pages = align_to_page(self.code_capacity + self.trampoline_capacity);

        if self.is_macos {
            enable_write();
        } else {
            // SAFETY: same as the mprotect call in make_executable — same
            // sub-mapping, single-threaded access via `&mut self`.
            let rc = unsafe {
                libc::mprotect(
                    base.as_ptr() as *mut c_void,
                    code_pages as size_t,
                    libc::PROT_READ | libc::PROT_WRITE,
                )
            };
            if rc != 0 {
                return Err(MemoryError::ProtectFailed);
            }
        }

        self.state = ProtectionState::Writable;
        Ok(())
    }

    /// Get a typed function pointer to the code at a given offset.
    ///
    /// The caller must ensure the region is in `Executable` state and that
    /// the offset points to a valid function entry. This mirrors the Zig
    /// original's `comptime T: type` API as a generic raw-pointer return;
    /// callers transmute to the concrete `fn` type they need.
    pub fn get_function_ptr(&self, offset: usize) -> Result<*const (), MemoryError> {
        if self.state != ProtectionState::Executable {
            return Err(MemoryError::NotExecutable);
        }
        let code = self.code_base.ok_or(MemoryError::AlreadyFreed)?;
        // SAFETY: pointer arithmetic only — no dereference here. Turning
        // this into a callable function pointer and invoking it is the
        // caller's unsafety to uphold (offset must address a valid entry
        // point; that can't be checked generically at this layer, exactly
        // as in the Zig original).
        let addr = unsafe { code.as_ptr().add(offset) };
        Ok(addr as *const ())
    }

    /// Get the absolute address of a code offset (for relocation
    /// calculations).
    pub fn code_address(&self, offset: usize) -> Option<u64> {
        let code = self.code_base?;
        Some(code.as_ptr() as u64 + offset as u64)
    }

    /// Get the absolute address of a data offset (for relocation
    /// calculations).
    pub fn data_address(&self, offset: usize) -> Option<u64> {
        let data = self.data_base?;
        Some(data.as_ptr() as u64 + offset as u64)
    }

    /// Get a pointer to the data region for direct access.
    pub fn data_slice(&self) -> Option<&[u8]> {
        let data = self.data_base?;
        // SAFETY: `data` is the live data mapping (macOS: separate mmap
        // never freed while `self` lives; Linux: sub-range of `base`'s
        // mapping); `[0, data_len)` was written by `copy_data`, which only
        // ever advances `data_len` after a successful in-bounds write, so
        // the whole range is initialized and in bounds. The returned
        // borrow's lifetime is tied to `&self`, preventing use-after-free
        // via the Drop impl.
        Some(unsafe { std::slice::from_raw_parts(data.as_ptr(), self.data_len) })
    }

    /// Get a read-only view of the code region.
    pub fn code_slice(&self) -> Option<&[u8]> {
        let code = self.code_base?;
        // SAFETY: same argument as data_slice, for the code mapping and
        // `code_len` (only advanced by copy_code after a successful
        // in-bounds write).
        Some(unsafe { std::slice::from_raw_parts(code.as_ptr(), self.code_len) })
    }

    /// Get a read-only view of the trampoline island.
    pub fn trampoline_slice(&self) -> Option<&[u8]> {
        let tramp = self.trampoline_base?;
        // SAFETY: same argument as data_slice, for the trampoline
        // sub-range and `trampoline_len` (only advanced by
        // write_trampoline/write_trap_stub after a successful in-bounds
        // write).
        Some(unsafe { std::slice::from_raw_parts(tramp.as_ptr(), self.trampoline_len) })
    }

    /// Read a 32-bit word from the code region at the given byte offset.
    pub fn read_code_word(&self, offset: usize) -> Option<u32> {
        let code = self.code_base?;
        if offset + 4 > self.code_len {
            return None;
        }
        // SAFETY: `offset + 4 <= code_len <= code_capacity`, just checked,
        // so this 4-byte read is within the initialized, mapped code
        // region.
        Some(unsafe { read_u32(code.as_ptr().add(offset)) })
    }

    /// Patch a 32-bit word in the code region (must be `Writable`).
    pub fn patch_code_word(&mut self, offset: usize, word: u32) -> Result<(), MemoryError> {
        if self.state != ProtectionState::Writable {
            return Err(MemoryError::NotWritable);
        }
        let code = self.code_base.ok_or(MemoryError::AlreadyFreed)?;
        // SAFETY: `state == Writable` was checked above; `offset` is a
        // caller-supplied offset into already-emitted code, matching the
        // Zig original's own lack of an explicit bounds check here.
        unsafe {
            write_u32(code.as_ptr().add(offset), word);
        }
        Ok(())
    }

    /// Free all memory regions.
    ///
    /// After this call, all pointers into the region are invalid. Called
    /// automatically by `Drop`; also safe to call (or no-op re-call)
    /// explicitly, matching the Zig original's explicit `free()` — repeat
    /// calls are harmless because `state` becomes `Freed` and `base`
    /// becomes `None` after the first call.
    pub fn free(&mut self) {
        // Free the separate data mmap first (macOS only).
        if let Some(data_base) = self.data_mmap_base.take() {
            // SAFETY: `data_base`/`data_mmap_size` describe exactly the
            // regular (non-MAP_JIT) mmap created in `allocate_macos` and
            // not yet unmapped; no other live reference to it exists once
            // this function is called (that's the point of `free`/`Drop`).
            unsafe { release_va(data_base, self.data_mmap_size) };
            self.data_mmap_size = 0;
        }

        if let Some(base) = self.base.take() {
            // On macOS, ensure we're in write mode before unmapping (not
            // strictly necessary but clean).
            if self.is_macos && self.state == ProtectionState::Executable {
                enable_write();
            }

            // SAFETY: `base`/`total_size` describe exactly the primary
            // mapping created in `allocate_macos`/`allocate_linux` and not
            // yet unmapped; no other live reference to it exists once this
            // function is called.
            unsafe { release_va(base, self.total_size) };

            self.code_base = None;
            self.data_base = None;
            self.trampoline_base = None;
            self.state = ProtectionState::Freed;
        }
    }

    /// Check if the region has been allocated and not freed.
    pub fn is_alive(&self) -> bool {
        self.base.is_some() && self.state != ProtectionState::Freed
    }

    /// Return diagnostic information about the memory layout.
    pub fn layout_info(&self) -> LayoutInfo {
        LayoutInfo {
            base_addr: self.base.map(|b| b.as_ptr() as usize).unwrap_or(0),
            code_addr: self.code_base.map(|b| b.as_ptr() as usize).unwrap_or(0),
            data_addr: self.data_base.map(|b| b.as_ptr() as usize).unwrap_or(0),
            trampoline_addr: self
                .trampoline_base
                .map(|b| b.as_ptr() as usize)
                .unwrap_or(0),
            total_size: self.total_size,
            code_capacity: self.code_capacity,
            code_used: self.code_len,
            data_capacity: self.data_capacity,
            data_used: self.data_len,
            trampoline_capacity: self.trampoline_capacity,
            trampoline_used: self.trampoline_len,
            state: self.state,
        }
    }

    /// Create an uninitialized (null) region — useful for deferred
    /// allocation.
    pub fn empty() -> JitMemoryRegion {
        JitMemoryRegion {
            base: None,
            total_size: 0,
            code_base: None,
            code_capacity: 0,
            code_len: 0,
            trampoline_base: None,
            trampoline_capacity: 0,
            trampoline_len: 0,
            data_base: None,
            data_capacity: 0,
            data_len: 0,
            data_mmap_base: None,
            data_mmap_size: 0,
            state: ProtectionState::Freed,
            is_macos: cfg!(target_os = "macos"),
        }
    }
}

impl Drop for JitMemoryRegion {
    fn drop(&mut self) {
        self.free();
    }
}

// SAFETY: `JitMemoryRegion` owns its mmap'd allocations exclusively (no
// other Rust or C code holds a pointer into them while a `JitMemoryRegion`
// is alive), and the raw pointers it holds are plain heap-like memory
// addresses, not thread-affine resources — the only thread-affine part of
// this design is `pthread_jit_write_protect_np`, which affects the calling
// thread's view of MAP_JIT pages, not the region's ownership. Moving a
// `JitMemoryRegion` to another thread and calling `make_executable`/
// `make_writable` there is safe (that thread's own W^X state is toggled);
// it is the caller's responsibility not to call these concurrently from
// two threads on the same region (this type is not `Sync`).
unsafe impl Send for JitMemoryRegion {}

/// Diagnostic layout information for reporting.
#[derive(Debug, Clone, Copy)]
pub struct LayoutInfo {
    pub base_addr: usize,
    pub code_addr: usize,
    pub data_addr: usize,
    pub trampoline_addr: usize,
    pub total_size: usize,
    pub code_capacity: usize,
    pub code_used: usize,
    pub data_capacity: usize,
    pub data_used: usize,
    pub trampoline_capacity: usize,
    pub trampoline_used: usize,
    pub state: ProtectionState,
}

impl std::fmt::Display for LayoutInfo {
    /// Format layout info for human-readable display.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "=== JIT Memory Layout ===")?;
        writeln!(f, "  Base:        0x{:016x}", self.base_addr)?;
        writeln!(
            f,
            "  Code:        0x{:016x} ({}/{} bytes used)",
            self.code_addr, self.code_used, self.code_capacity
        )?;
        writeln!(
            f,
            "  Trampolines: 0x{:016x} ({}/{} bytes used)",
            self.trampoline_addr, self.trampoline_used, self.trampoline_capacity
        )?;
        writeln!(
            f,
            "  Data:        0x{:016x} ({}/{} bytes used)",
            self.data_addr, self.data_used, self.data_capacity
        )?;
        writeln!(f, "  Total VA:    {} bytes", self.total_size)?;
        writeln!(f, "  State:       {:?}", self.state)?;

        if self.code_addr != 0 && self.data_addr != 0 {
            let distance = if self.data_addr > self.code_addr {
                self.data_addr - self.code_addr
            } else {
                self.code_addr - self.data_addr
            };
            writeln!(f, "  Code<->Data: {} bytes ({} KB)", distance, distance / 1024)?;
        }
        Ok(())
    }
}

// ============================================================================
// Section: Platform-Specific Implementation
// ============================================================================

/// Round a size up to the nearest page boundary.
fn align_to_page(size: usize) -> usize {
    (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

/// Reserve virtual address space without committing physical memory.
/// Linux-only helper (macOS uses a single MAP_JIT mmap instead — see
/// `allocate_macos`).
#[cfg(target_os = "linux")]
unsafe fn reserve_va(total_size: usize) -> Option<NonNull<u8>> {
    // SAFETY (caller obligation, forwarded): `total_size` is a plain
    // length; a PROT_NONE anonymous mapping has no aliasing preconditions.
    let result = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            total_size as size_t,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if result == libc::MAP_FAILED {
        None
    } else {
        // SAFETY: just checked `result != MAP_FAILED`; mmap never returns
        // null on success.
        Some(unsafe { NonNull::new_unchecked(result as *mut u8) })
    }
}

/// Release reserved VA space.
///
/// # Safety
/// `base` must point to a live mapping of exactly `total_size` bytes
/// created by `mmap` (directly or via `reserve_va`/`allocate_macos`), not
/// already unmapped, and with no other live references to it.
unsafe fn release_va(base: NonNull<u8>, total_size: usize) {
    // SAFETY: forwarded from this function's own safety contract — `base`
    // is a live mmap mapping of `total_size` bytes.
    unsafe {
        libc::munmap(base.as_ptr() as *mut c_void, total_size as size_t);
    }
}

/// Commit a sub-region of a `PROT_NONE` reservation to RW via `mprotect`.
/// Used by the Linux buddy-allocation path.
///
/// # Safety
/// `base` must point to a live mapping of at least `size` bytes (a
/// sub-range of a `reserve_va` reservation) that is not concurrently
/// accessed.
#[cfg(target_os = "linux")]
unsafe fn commit_region_rw(base: NonNull<u8>, size: usize) -> Result<(), ()> {
    // SAFETY: forwarded from this function's own safety contract.
    let rc = unsafe {
        libc::mprotect(
            base.as_ptr() as *mut c_void,
            size as size_t,
            libc::PROT_READ | libc::PROT_WRITE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(())
    }
}

/// macOS: enable write access on the current thread's JIT pages.
fn enable_write() {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `pthread_jit_write_protect_np` takes a plain `c_int`
        // flag and affects only the calling thread's view of its own
        // MAP_JIT pages — no pointer/lifetime precondition to uphold.
        unsafe { libc::pthread_jit_write_protect_np(0) }; // 0 = writable
    }
}

/// macOS: enable execute access on the current thread's JIT pages.
fn enable_execute() {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: see enable_write — same no-precondition contract.
        unsafe { libc::pthread_jit_write_protect_np(1) }; // 1 = executable
    }
}

/// Invalidate the instruction cache for the given range.
///
/// Required on ARM64 because the instruction and data caches are not
/// coherent — writes to the data cache (code emission) are not
/// automatically visible to the instruction cache.
///
/// # Safety
/// `ptr` must be valid for reads of `len` bytes (the range whose stale
/// icache lines are being invalidated).
pub unsafe fn icache_invalidate(ptr: *const u8, len: usize) {
    if len == 0 {
        return;
    }

    #[cfg(target_os = "macos")]
    {
        // SAFETY: forwarded from this function's own safety contract —
        // `ptr` is valid for `len` bytes. `sys_icache_invalidate` only
        // reads/flushes cache lines in that range; it does not retain the
        // pointer.
        unsafe { sys_icache_invalidate(ptr as *const c_void, len as size_t) };
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: forwarded from this function's own safety contract.
        unsafe { linux_clear_cache(ptr, len) };
    }
    // Other platforms: no-op (assume coherent caches or non-ARM64).
}

/// Linux ARM64: manual icache invalidation using `DC CVAU` + `IC IVAU` +
/// `DSB` + `ISB`.
///
/// Ported from the Zig source's inline-asm sequence. Unverified on real
/// hardware in this repo (no Linux dev/CI machine here yet).
///
/// # Safety
/// `ptr` must be valid for reads of `len` bytes.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
unsafe fn linux_clear_cache(ptr: *const u8, len: usize) {
    use std::arch::asm;

    // Cache line size (typically 64 bytes on ARM64).
    const CACHE_LINE: usize = 64;
    let start = (ptr as usize) & !(CACHE_LINE - 1);
    let end = ptr as usize + len;

    // Step 1: Clean data cache to point of unification.
    let mut addr = start;
    while addr < end {
        // SAFETY: `dc cvau` on `addr` only requires `addr` be a valid VA
        // (it operates on cache lines, not exact byte ranges); forwarded
        // from this function's caller contract that `[ptr, ptr+len)` is
        // valid, and `addr` walks cache-line-aligned steps through
        // (a superset-aligned covering of) that range.
        unsafe {
            asm!("dc cvau, {0}", in(reg) addr, options(nostack, preserves_flags));
        }
        addr += CACHE_LINE;
    }

    // Step 2: Data synchronization barrier.
    // SAFETY: DSB is a pure synchronization instruction, no memory access.
    unsafe {
        asm!("dsb ish", options(nostack, preserves_flags));
    }

    // Step 3: Invalidate instruction cache to point of unification.
    let mut addr = start;
    while addr < end {
        // SAFETY: same argument as the `dc cvau` loop above.
        unsafe {
            asm!("ic ivau, {0}", in(reg) addr, options(nostack, preserves_flags));
        }
        addr += CACHE_LINE;
    }

    // Step 4: Data synchronization barrier + instruction synchronization
    // barrier.
    // SAFETY: DSB/ISB are pure synchronization instructions, no memory
    // access.
    unsafe {
        asm!("dsb ish", options(nostack, preserves_flags));
        asm!("isb", options(nostack, preserves_flags));
    }
}

#[cfg(all(target_os = "linux", not(target_arch = "aarch64")))]
unsafe fn linux_clear_cache(_ptr: *const u8, _len: usize) {
    // Non-ARM64 Linux: no-op, matching the Zig source's `comptime`-gated
    // no-op for this combination.
}

// ============================================================================
// Section: Byte-Level Helpers
// ============================================================================

/// Write a little-endian `u32` to the 4 bytes at `dst`.
///
/// # Safety
/// `dst` must be valid for writes of 4 bytes.
unsafe fn write_u32(dst: *mut u8, val: u32) {
    // SAFETY: forwarded from this function's own safety contract; a 4-byte
    // little-endian store one byte at a time, no alignment requirement.
    unsafe {
        dst.write(val as u8);
        dst.add(1).write((val >> 8) as u8);
        dst.add(2).write((val >> 16) as u8);
        dst.add(3).write((val >> 24) as u8);
    }
}

/// Write a little-endian `u64` to the 8 bytes at `dst`.
///
/// # Safety
/// `dst` must be valid for writes of 8 bytes.
unsafe fn write_u64(dst: *mut u8, val: u64) {
    // SAFETY: forwarded from this function's own safety contract; an
    // 8-byte little-endian store one byte at a time, no alignment
    // requirement.
    unsafe {
        for i in 0..8u32 {
            dst.add(i as usize).write((val >> (i * 8)) as u8);
        }
    }
}

/// Read a little-endian `u32` from the 4 bytes at `src`.
///
/// # Safety
/// `src` must be valid for reads of 4 bytes.
unsafe fn read_u32(src: *const u8) -> u32 {
    // SAFETY: forwarded from this function's own safety contract; a
    // 4-byte little-endian load one byte at a time, no alignment
    // requirement.
    unsafe {
        (src.read() as u32)
            | ((src.add(1).read() as u32) << 8)
            | ((src.add(2).read() as u32) << 16)
            | ((src.add(3).read() as u32) << 24)
    }
}

/// Read a little-endian `u64` from the 8 bytes at `src`.
///
/// # Safety
/// `src` must be valid for reads of 8 bytes.
unsafe fn read_u64(src: *const u8) -> u64 {
    // SAFETY: forwarded from this function's own safety contract; an
    // 8-byte little-endian load one byte at a time, no alignment
    // requirement.
    unsafe {
        let mut v = 0u64;
        for i in 0..8u32 {
            v |= (src.add(i as usize).read() as u64) << (i * 8);
        }
        v
    }
}

// ============================================================================
// Section: Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn jit_supported() -> bool {
        cfg!(target_os = "macos") || cfg!(target_os = "linux")
    }

    #[test]
    fn align_to_page_rounds_up_correctly() {
        // macOS: PAGE_SIZE = 16384, Linux: PAGE_SIZE = 4096.
        let ps = PAGE_SIZE;
        assert_eq!(ps, align_to_page(1));
        assert_eq!(ps, align_to_page(ps));
        assert_eq!(ps * 2, align_to_page(ps + 1));
        assert_eq!(ps * 2, align_to_page(ps * 2));
        assert_eq!(0, align_to_page(0));
    }

    #[test]
    fn empty_region_is_freed_state() {
        let region = JitMemoryRegion::empty();
        assert!(!region.is_alive());
        assert_eq!(ProtectionState::Freed, region.state);
        assert!(region.code_base.is_none());
        assert!(region.data_base.is_none());
    }

    #[test]
    fn trampoline_stub_constants() {
        assert_eq!(16usize, TRAMPOLINE_STUB_SIZE);
        assert_eq!(4096usize, TRAMPOLINE_ISLAND_CAPACITY);
    }

    #[test]
    fn write_u32_and_read_u32_round_trip() {
        let mut buf = [0u8; 4];
        // SAFETY: `buf` is a local 4-byte array, valid for the write/read.
        unsafe {
            write_u32(buf.as_mut_ptr(), 0xDEADBEEF);
            assert_eq!(0xDEADBEEFu32, read_u32(buf.as_ptr()));

            write_u32(buf.as_mut_ptr(), 0x94000050);
            assert_eq!(0x94000050u32, read_u32(buf.as_ptr()));
        }
    }

    #[test]
    fn write_u64_stores_little_endian() {
        let mut buf = [0u8; 8];
        // SAFETY: `buf` is a local 8-byte array, valid for the write.
        unsafe {
            write_u64(buf.as_mut_ptr(), 0x0123456789ABCDEF);
        }
        assert_eq!(0xEFu8, buf[0]);
        assert_eq!(0xCDu8, buf[1]);
        assert_eq!(0xABu8, buf[2]);
        assert_eq!(0x89u8, buf[3]);
        assert_eq!(0x67u8, buf[4]);
        assert_eq!(0x45u8, buf[5]);
        assert_eq!(0x23u8, buf[6]);
        assert_eq!(0x01u8, buf[7]);
    }

    #[test]
    fn trampoline_ldr_x16_encoding_is_correct() {
        // LDR X16, [PC, #8] should encode to 0x58000050.
        // Encoding: opc=01 011 000 imm19 Rt.
        // imm19 = 2 (offset 8 bytes = 2 words), Rt = 16.
        // 0x58000000 | (2 << 5) | 16 = 0x58000050.
        let expected: u32 = 0x58000050;
        let opc_base: u32 = 0x58000000;
        let imm19: u32 = 2; // 8 bytes / 4
        let rt: u32 = 16; // X16
        let encoded = opc_base | (imm19 << 5) | rt;
        assert_eq!(expected, encoded);
    }

    #[test]
    fn trampoline_br_x16_encoding_is_correct() {
        // BR X16 should encode to 0xd61f0200.
        let expected: u32 = 0xd61f0200;
        let br_base: u32 = 0xd61f0000;
        let rn: u32 = 16; // X16
        let encoded = br_base | (rn << 5);
        assert_eq!(expected, encoded);
    }

    #[test]
    fn allocate_and_free_jit_memory_region() {
        if !jit_supported() {
            return; // Skip on unsupported platforms.
        }

        let region = match JitMemoryRegion::allocate(4096, 4096) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("JIT memory allocation not available: {e}");
                return;
            }
        };

        assert!(region.is_alive());
        assert_eq!(ProtectionState::Writable, region.state);
        assert!(region.code_base.is_some());
        assert!(region.data_base.is_some());
        assert!(region.trampoline_base.is_some());
        // region dropped here -> free() runs.
    }

    #[test]
    fn copy_code_into_region() {
        if !jit_supported() {
            return;
        }
        let mut region = match JitMemoryRegion::allocate(4096, 4096) {
            Ok(r) => r,
            Err(_) => return,
        };

        // Write a NOP (0xd503201f) and RET (0xd65f03c0).
        let nop = [0x1f, 0x20, 0x03, 0xd5];
        let ret = [0xc0, 0x03, 0x5f, 0xd6];

        region.copy_code(&nop).unwrap();
        region.copy_code(&ret).unwrap();

        assert_eq!(8usize, region.code_len);

        // Verify code was written correctly.
        let word0 = region.read_code_word(0);
        assert!(word0.is_some());
        assert_eq!(0xd503201fu32, word0.unwrap());

        let word1 = region.read_code_word(4);
        assert!(word1.is_some());
        assert_eq!(0xd65f03c0u32, word1.unwrap());
    }

    #[test]
    fn copy_data_into_region() {
        if !jit_supported() {
            return;
        }
        let mut region = match JitMemoryRegion::allocate(4096, 4096) {
            Ok(r) => r,
            Err(_) => return,
        };

        let hello = b"hello\x00";
        region.copy_data(hello).unwrap();

        assert_eq!(6usize, region.data_len);

        let data = region.data_slice();
        assert!(data.is_some());
        assert_eq!(hello, data.unwrap());
    }

    #[test]
    fn write_trampoline_stub() {
        if !jit_supported() {
            return;
        }
        let mut region = match JitMemoryRegion::allocate(4096, 4096) {
            Ok(r) => r,
            Err(_) => return,
        };

        // Write a trampoline for a fake address.
        let target: u64 = 0x0000000100003F00;
        let stub_offset = region.write_trampoline(target).unwrap();

        // Stub should be at code_capacity offset.
        assert_eq!(4096usize, stub_offset);
        assert_eq!(TRAMPOLINE_STUB_SIZE, region.trampoline_len);

        // Verify stub contents.
        let tramp = region.trampoline_slice();
        assert!(tramp.is_some());
        let t = tramp.unwrap();
        assert_eq!(16usize, t.len());

        // Check LDR X16, [PC, #8].
        // SAFETY: `t` is a 16-byte slice just asserted above; reading 4
        // bytes at offset 0 is in bounds.
        let ldr = unsafe { read_u32(t.as_ptr()) };
        assert_eq!(0x58000050u32, ldr);

        // Check BR X16.
        // SAFETY: reading 4 bytes at offset 4 of a 16-byte slice is in
        // bounds.
        let br = unsafe { read_u32(t.as_ptr().add(4)) };
        assert_eq!(0xd61f0200u32, br);
    }

    #[test]
    fn write_trap_stub_encodes_a_real_brk_not_svc() {
        // Regression test for a bug found while investigating an unrelated
        // arm64 regalloc bug report (see
        // jit/rust/tests/register_bug_investigation.rs): the trap stub's
        // opcode base was missing bit 21 (0xD400_0000 instead of
        // 0xD420_0000), so it decoded as an unallocated "exception
        // generation" encoding (opc=000, LL=00 — not SVC's LL=01, not
        // BRK's opc=001) rather than `BRK #0xF001`. Executing it raised
        // SIGILL (Undefined Instruction) instead of the intended SIGTRAP —
        // silently defeating the whole point of a trap stub (a clean,
        // identifiable fault for calling an unresolved JIT symbol, instead
        // of following a null pointer). Present in the original
        // jit_memory.zig too — its own doc comment states the correct
        // 0xD43E0020 result immediately above code that computed
        // 0xD41E0020.
        if !jit_supported() {
            return;
        }
        let mut region = match JitMemoryRegion::allocate(4096, 4096) {
            Ok(r) => r,
            Err(_) => return,
        };

        let stub_offset = region.write_trap_stub().unwrap();
        assert_eq!(4096usize, stub_offset);

        let tramp = region.trampoline_slice().unwrap();
        assert_eq!(16usize, tramp.len());

        // SAFETY: `tramp` is a 16-byte slice just asserted above.
        let brk0 = unsafe { read_u32(tramp.as_ptr()) };
        let brk1 = unsafe { read_u32(tramp.as_ptr().add(4)) };
        let sentinel = unsafe { read_u64(tramp.as_ptr().add(8)) };

        // Ground truth: crate::encoder::emit_brk(0xF001), independently
        // verified against clang/llvm-mc-derived tests.
        let expected_brk = crate::encoder::emit_brk(0xF001);
        assert_eq!(0xD43E_0020u32, expected_brk, "sanity: emit_brk's own known-good encoding");
        assert_eq!(expected_brk, brk0);
        assert_eq!(expected_brk, brk1);
        assert_eq!(0xDEADu64, sentinel);
    }

    #[test]
    fn layout_info_reporting() {
        let region = JitMemoryRegion::empty();
        let info = region.layout_info();
        assert_eq!(0usize, info.base_addr);
        assert_eq!(0usize, info.code_used);
        assert_eq!(ProtectionState::Freed, info.state);
    }

    #[test]
    fn make_executable_and_back_to_writable() {
        if !jit_supported() {
            return;
        }
        let mut region = match JitMemoryRegion::allocate(4096, 4096) {
            Ok(r) => r,
            Err(_) => return,
        };

        // Write minimal code: NOP + RET.
        let nop_ret = [
            0x1f, 0x20, 0x03, 0xd5, // NOP
            0xc0, 0x03, 0x5f, 0xd6, // RET
        ];
        region.copy_code(&nop_ret).unwrap();

        // Make executable.
        region.make_executable().unwrap();
        assert_eq!(ProtectionState::Executable, region.state);

        // Make writable again (for hot-patching).
        region.make_writable().unwrap();
        assert_eq!(ProtectionState::Writable, region.state);
    }

    #[test]
    fn code_overflow_detection() {
        if !jit_supported() {
            return;
        }
        // Allocate tiny region.
        let mut region = match JitMemoryRegion::allocate_with_trampoline(16, 16, 0) {
            Ok(r) => r,
            Err(_) => return,
        };

        // Fill it up.
        let data = [0u8; 16];
        region.copy_code(&data).unwrap();

        // Should fail on overflow.
        let result = region.copy_code(&[0u8]);
        assert_eq!(Err(MemoryError::CodeOverflow), result);
    }

    #[test]
    fn data_overflow_detection() {
        if !jit_supported() {
            return;
        }
        let mut region = match JitMemoryRegion::allocate_with_trampoline(16, 8, 0) {
            Ok(r) => r,
            Err(_) => return,
        };

        let data = [0u8; 8];
        region.copy_data(&data).unwrap();

        let result = region.copy_data(&[0u8]);
        assert_eq!(Err(MemoryError::DataOverflow), result);
    }

    #[test]
    fn write_protection_state_enforcement() {
        if !jit_supported() {
            return;
        }
        let mut region = match JitMemoryRegion::allocate(4096, 4096) {
            Ok(r) => r,
            Err(_) => return,
        };

        // Write some code.
        let code = [0xc0, 0x03, 0x5f, 0xd6]; // RET
        region.copy_code(&code).unwrap();

        // Make executable.
        region.make_executable().unwrap();

        // Attempts to write should fail.
        let result = region.copy_code(&code);
        assert_eq!(Err(MemoryError::NotWritable), result);

        let tramp_result = region.write_trampoline(0x1000);
        assert_eq!(Err(MemoryError::NotWritable), tramp_result);

        // But get_function_ptr should work.
        let ptr = region.get_function_ptr(0);
        assert!(ptr.is_ok());
    }

    #[test]
    fn patch_bl_to_trampoline_computes_correct_offset() {
        if !jit_supported() {
            return;
        }
        let mut region = match JitMemoryRegion::allocate(4096, 4096) {
            Ok(r) => r,
            Err(_) => return,
        };

        // Write placeholder BL at offset 0.
        let placeholder_bl = [0x00, 0x00, 0x00, 0x94]; // BL #0
        region.copy_code(&placeholder_bl).unwrap();

        // Write a trampoline.
        let stub_offset = region.write_trampoline(0xDEAD).unwrap();

        // Patch the BL to point to the trampoline.
        region.patch_bl_to_trampoline(0, stub_offset).unwrap();

        // Read back and verify.
        let patched = region.read_code_word(0);
        assert!(patched.is_some());

        // The BL should have opcode 0x94 in the top byte and a non-zero
        // offset.
        assert_eq!(0x94u32, (patched.unwrap() >> 24) & 0xFC);
    }

    #[test]
    fn multiple_trampolines_get_sequential_offsets() {
        if !jit_supported() {
            return;
        }
        let mut region = match JitMemoryRegion::allocate(4096, 4096) {
            Ok(r) => r,
            Err(_) => return,
        };

        let off1 = region.write_trampoline(0x1000).unwrap();
        let off2 = region.write_trampoline(0x2000).unwrap();
        let off3 = region.write_trampoline(0x3000).unwrap();

        // Each stub is TRAMPOLINE_STUB_SIZE apart.
        assert_eq!(4096usize, off1);
        assert_eq!(4096 + 16, off2);
        assert_eq!(4096 + 32, off3);

        assert_eq!(48usize, region.trampoline_len);
    }

    #[test]
    fn address_computation_for_code_and_data() {
        if !jit_supported() {
            return;
        }
        let region = match JitMemoryRegion::allocate(4096, 4096) {
            Ok(r) => r,
            Err(_) => return,
        };

        let code_addr_0 = region.code_address(0);
        let code_addr_4 = region.code_address(4);
        let data_addr_0 = region.data_address(0);

        assert!(code_addr_0.is_some());
        assert!(code_addr_4.is_some());
        assert!(data_addr_0.is_some());

        // Code addresses should be sequential.
        assert_eq!(code_addr_0.unwrap() + 4, code_addr_4.unwrap());

        // Data should be at a higher address than code (buddy allocation)
        // on macOS in practice, though not guaranteed by mmap in general —
        // this mirrors the Zig test's own assumption about typical mmap
        // placement on this machine.
        assert!(data_addr_0.unwrap() > code_addr_0.unwrap());
    }
}
