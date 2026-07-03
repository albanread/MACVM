// Vendored from JASM (wfasm), https://github.com/albanread/JASM  commit 5ac1b1b53d1cfa859e124ff962cf6180ffb72c55
// Original path: rust/src/native_macos.rs.  License: MIT (see LICENSE-JASM in
// this directory; Copyright (c) 2026 alban read).
// Local modifications are marked with `// MACVM:` comments — keep the diff
// against upstream minimal so re-vendoring stays mechanical.
// MACVM: `crate::backend`/`crate::relocpatch`/`crate::a64` -> their
// `crate::vendor::wfasm::` equivalents (modules moved under MACVM's own
// tree). MACVM: added `MacJit::region_raw` + the standalone
// `jit_write_protect`/`icache_invalidate` fns at the bottom of this file
// (sprint_s09_detail.md D1.2) so `CodeCache` can manage the region and W^X
// state directly instead of going through `MacJit`'s own build/load/finalize
// protocol, which assumes one whole-module encode-and-place call rather than
// many independently-published code-cache segments.

//! macOS / Apple Silicon native loader (`MacJit`) — the AArch64 sibling of the
//! Windows [`NativeJit`](crate::native). It places an
//! [`EncodedModule`](crate::vendor::wfasm::backend::EncodedModule) from the native
//! [`A64Encoder`](crate::vendor::wfasm::a64::A64Encoder) into executable memory, resolves
//! relocations, and hands back a callable function pointer. No LLVM.
//!
//! Memory model (see docs/design/aarch64-apple-silicon.md §3.6):
//!
//! * One region is `mmap`'d `MAP_JIT` (RWX once; the page protection never
//!   changes after that).
//! * Per-cycle write/exec is the **per-thread** `pthread_jit_write_protect_np`
//!   toggle — NOT `mprotect`. We flip to writable at allocation, build + relocate
//!   the whole module, then flip to executable in [`MacJit::finalize`].
//! * `sys_icache_invalidate` runs after the writes are in place and before
//!   execution (ARM has split I/D caches — mandatory).
//! * Far branch targets (host externs resolved via `dlsym`, routinely > ±128 MB
//!   from the region) are reached through a per-target absolute veneer
//!   (`movz/movk x16…; br x16`), the analogue of the x86 `movabs rax; jmp rax`.
//!
//! The executing thread must be the one that flipped to exec mode (the toggle is
//! per-thread); building + finalizing + calling on one thread satisfies that.

#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr;

use anyhow::{bail, Context, Result};

use crate::vendor::wfasm::backend::{EncodedModule, Reloc};
use crate::vendor::wfasm::relocpatch::{self, ResolvedReloc, VENEER_LEN};

// ── libSystem FFI (linked by default on macOS) ──────────────────────────────

extern "C" {
    fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
    /// `<libkern/OSCacheControl.h>` — data-cache clean + instruction-cache
    /// invalidate over `[start, start+len)`. Public since macOS 10.5.
    fn sys_icache_invalidate(start: *mut c_void, len: usize);
    /// `<pthread.h>` — per-thread W^X toggle for `MAP_JIT` pages. `1` =
    /// write-protected (executable), `0` = writable (non-executable).
    fn pthread_jit_write_protect_np(enabled: i32);
}

const PROT_READ: i32 = 0x1;
const PROT_WRITE: i32 = 0x2;
const PROT_EXEC: i32 = 0x4;
const MAP_PRIVATE: i32 = 0x0002;
const MAP_ANON: i32 = 0x1000;
const MAP_JIT: i32 = 0x0800;
const MAP_FAILED: *mut c_void = usize::MAX as *mut c_void;
const PAGE: usize = 0x4000; // 16 KiB on Apple Silicon

fn round_up(n: usize, to: usize) -> usize {
    (n + to - 1) & !(to - 1)
}

struct Placed {
    base: u64,
    relocs: Vec<Reloc>,
}

pub struct MacJit {
    region: *mut u8,
    cap: usize,
    used: usize,
    /// Defined symbol → absolute runtime address.
    symbols: HashMap<String, u64>,
    /// Host extern name → absolute address.
    externs: HashMap<String, u64>,
    placed: Vec<Placed>,
    finalized: bool,
    writable: bool,
    /// Accumulated text for the [`Loader`](crate::vendor::wfasm::backend::Loader) builder path.
    pending_text: String,
}

impl MacJit {
    /// Reserve `cap` bytes (rounded to a page) of `MAP_JIT` RWX code space, left
    /// in the *writable* state for building.
    pub fn with_capacity(cap: usize) -> Result<Self> {
        let cap = round_up(cap.max(PAGE), PAGE);
        let region = unsafe {
            mmap(
                ptr::null_mut(),
                cap,
                PROT_READ | PROT_WRITE | PROT_EXEC,
                MAP_PRIVATE | MAP_ANON | MAP_JIT,
                -1,
                0,
            )
        };
        if region == MAP_FAILED || region.is_null() {
            bail!("mmap(MAP_JIT) failed — JIT memory unavailable");
        }
        // Enter write mode for this thread so we can populate the region.
        unsafe { pthread_jit_write_protect_np(0) };
        Ok(MacJit {
            region: region as *mut u8,
            cap,
            used: 0,
            symbols: HashMap::new(),
            externs: HashMap::new(),
            placed: Vec::new(),
            finalized: false,
            writable: true,
            pending_text: String::new(),
        })
    }

    /// A builder-mode loader: accumulate text + externs, assemble + place on the
    /// first `lookup_addr`.
    pub fn new() -> Self {
        MacJit {
            region: ptr::null_mut(),
            cap: 0,
            used: 0,
            symbols: HashMap::new(),
            externs: HashMap::new(),
            placed: Vec::new(),
            finalized: false,
            writable: false,
            pending_text: String::new(),
        }
    }

    /// Bind a host extern (a Rust `extern "C"` function) by name.
    pub fn define_extern(&mut self, name: &str, addr: u64) {
        self.externs.insert(name.to_string(), addr);
    }

    /// Copy `m`'s code into the region (16-byte aligned), record its symbols at
    /// their final addresses, and queue its relocations. Returns the base.
    pub fn load_module(&mut self, m: &EncodedModule) -> Result<u64> {
        if self.finalized {
            bail!("MacJit already finalized");
        }
        debug_assert!(self.writable, "region must be writable to load");
        let start = round_up(self.used, 16);
        let end = start + m.code.len();
        if end > self.cap {
            bail!("code region exhausted: need {end} bytes, cap {}", self.cap);
        }
        let base = self.region as u64 + start as u64;
        unsafe { ptr::copy_nonoverlapping(m.code.as_ptr(), self.region.add(start), m.code.len()) };
        for (name, off) in &m.symbols {
            self.symbols.insert(name.clone(), base + *off as u64);
        }
        self.placed.push(Placed {
            base,
            relocs: m.relocs.clone(),
        });
        self.used = end;
        Ok(base)
    }

    fn resolve(&self, name: &str) -> Option<u64> {
        self.symbols
            .get(name)
            .copied()
            .or_else(|| self.externs.get(name).copied())
    }

    /// Apply all relocations (building far-call veneers as needed), flip the
    /// region to executable, and invalidate the icache. Idempotent.
    ///
    /// The actual bit-patching lives in `relocpatch::patch_relocs`, shared
    /// with the AOT Mach-O writer — this just resolves targets to absolute
    /// addresses (a JIT-only concern; the file-output path resolves against
    /// its own chosen load address instead) and hands off a region-relative
    /// view. `field_offset` is `p.base`'s offset from `self.region` (i.e.
    /// where `load_module` placed that module) plus the reloc's own `r.at`
    /// — the same `p.base + r.at` absolute address as before, just expressed
    /// relative to the buffer `patch_relocs` receives instead of computed as
    /// a raw pointer directly.
    pub fn finalize(&mut self) -> Result<()> {
        if self.finalized {
            return Ok(());
        }
        let region_base = self.region as u64;
        let mut resolved: Vec<ResolvedReloc> = Vec::new();
        for p in &self.placed {
            let module_offset = (p.base - region_base) as usize;
            for r in &p.relocs {
                let target = self
                    .resolve(&r.target)
                    .with_context(|| format!("unresolved reloc target `{}`", r.target))?;
                resolved.push(ResolvedReloc {
                    field_offset: module_offset + r.at,
                    kind: r.kind,
                    target: (target as i64 + r.addend) as u64,
                });
            }
        }

        let code = unsafe { std::slice::from_raw_parts_mut(self.region, self.cap) };
        self.used = relocpatch::patch_relocs(code, region_base, self.used, &resolved)?;

        // Flip this thread to exec mode, then invalidate the icache.
        unsafe {
            pthread_jit_write_protect_np(1);
            sys_icache_invalidate(self.region as *mut c_void, self.cap);
        }
        self.writable = false;
        self.finalized = true;
        Ok(())
    }

    /// Builder path: assemble accumulated text with the native AArch64 encoder,
    /// reserve a region, place + relocate + protect. Idempotent.
    fn build_if_needed(&mut self) -> Result<()> {
        if self.finalized {
            return Ok(());
        }
        if self.region.is_null() {
            let module = crate::vendor::wfasm::a64::assemble(&self.pending_text)
                .context("A64Encoder: assemble kernel text")?;
            // code + one veneer per extern + slack.
            let cap = module.code.len() + module.externs.len() * VENEER_LEN + PAGE;
            let externs = std::mem::take(&mut self.externs);
            *self = MacJit::with_capacity(cap)?;
            self.externs = externs;
            self.load_module(&module)?;
        }
        self.finalize()
    }

    /// Runtime address of a defined symbol (after [`finalize`]).
    pub fn lookup(&self, name: &str) -> Option<u64> {
        self.symbols.get(name).copied()
    }

    pub fn has_symbol(&self, name: &str) -> bool {
        self.symbols.contains_key(name)
    }

    // MACVM: expose the raw region so CodeCache can manage it directly —
    // `CodeCache` owns segment allocation/publish/patch itself (sprint_s09_
    // detail.md D2.3), using this `MacJit` purely for the mmap/W^X/icache
    // primitives, not its own build/load/finalize protocol.
    pub fn region_raw(&self) -> (*mut u8, usize) {
        (self.region, self.cap)
    }
}

// MACVM: re-exported W^X primitives for CodeCache / JitWriteGuard — the FFI
// decls above are private to this file, and CodeCache needs the toggle/flush
// operations directly (per-segment publish/patch), not through MacJit's own
// whole-region build/finalize protocol.
pub fn jit_write_protect(exec: bool) {
    unsafe { pthread_jit_write_protect_np(exec as i32) }
}
pub fn icache_invalidate(start: *const u8, len: usize) {
    unsafe { sys_icache_invalidate(start as *mut c_void, len) }
}

impl Default for MacJit {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::vendor::wfasm::backend::Loader for MacJit {
    fn add_asm(&mut self, asm_text: &str) -> Result<()> {
        self.pending_text.push_str(asm_text);
        self.pending_text.push('\n');
        Ok(())
    }
    fn declare_fn(&mut self, _name: &str, _arg_count: usize) -> Result<()> {
        Ok(())
    }
    fn define_extern_fn(&mut self, name: &str, _arg_count: usize, addr: *mut c_void) -> Result<()> {
        self.externs.insert(name.to_string(), addr as u64);
        Ok(())
    }
    fn lookup_addr(&mut self, name: &str) -> Result<u64> {
        self.build_if_needed()?;
        self.symbols
            .get(name)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("macos loader: symbol `{name}` not found"))
    }
}

impl Drop for MacJit {
    fn drop(&mut self) {
        if !self.region.is_null() {
            // Return to write mode so a subsequent builder on this thread starts
            // clean, then release the mapping.
            unsafe {
                if !self.writable {
                    pthread_jit_write_protect_np(0);
                }
                munmap(self.region as *mut c_void, self.cap);
            }
            self.region = ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vendor::wfasm::a64::A64Encoder;
    use crate::vendor::wfasm::backend::Encoder;

    fn place(asm: &str) -> MacJit {
        let m = A64Encoder.encode(asm).expect("encode");
        let mut jit = MacJit::with_capacity(m.code.len() + PAGE).expect("mmap");
        jit.load_module(&m).expect("load");
        jit.finalize().expect("finalize");
        jit
    }

    /// End-to-end: encode `(x+1)*2`, JIT it, and run it. Proves the whole
    /// pipe — A64Encoder → MAP_JIT → W^X toggle → icache → execute.
    #[test]
    fn leaf_executes() {
        let jit = place(".globl entry\nentry:\nadd x0, x0, #1\nadd x0, x0, x0\nret\n");
        let f: extern "C" fn(u64) -> u64 =
            unsafe { std::mem::transmute(jit.lookup("entry").unwrap()) };
        assert_eq!(f(10), 22);
        assert_eq!(f(0), 2);
        assert_eq!(f(100), 202);
    }

    /// Internal `bl` (in-range, patched, no veneer): entry calls a leaf helper.
    #[test]
    fn internal_call_executes() {
        let src = "\
.globl entry
entry:
stp x29, x30, [sp, #-16]!
bl dbl
ldp x29, x30, [sp], #16
ret
dbl:
add x0, x0, x0
ret
";
        let jit = place(src);
        let f: extern "C" fn(u64) -> u64 =
            unsafe { std::mem::transmute(jit.lookup("entry").unwrap()) };
        assert_eq!(f(21), 42);
    }

    extern "C" fn host_inc(x: u64) -> u64 {
        x + 1
    }

    /// Host callback: JIT'd code `bl`s a Rust `extern "C"` function. The target
    /// is in the test binary, typically far from the JIT region, so this
    /// exercises the absolute veneer path (and is correct either way).
    #[test]
    fn host_callback_executes() {
        let src = "\
.globl entry
entry:
stp x29, x30, [sp, #-16]!
bl host_inc
add x0, x0, x0
ldp x29, x30, [sp], #16
ret
";
        let m = A64Encoder.encode(src).expect("encode");
        let mut jit = MacJit::with_capacity(m.code.len() + PAGE).expect("mmap");
        jit.define_extern("host_inc", host_inc as *const () as u64);
        jit.load_module(&m).expect("load");
        jit.finalize().expect("finalize");
        let f: extern "C" fn(u64) -> u64 =
            unsafe { std::mem::transmute(jit.lookup("entry").unwrap()) };
        assert_eq!(f(10), 22, "(10+1)*2 via host callback");
        assert_eq!(f(0), 2);
    }

    // abs_veneer's own encoding is covered by relocpatch::tests::abs_veneer_encoding
    // now that the implementation lives there.
}
