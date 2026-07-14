//! The Cocoa bridge, C0 (docs/cocoa_bridge_design.md): `ObjcRef` wrapping,
//! retain-on-wrap / release-with-poison ownership, the per-thread bottom
//! autorelease pool, and Foundation sends through the `@try` exception shim
//! (`objc_shim.m`, compiled by `build.rs`).
//!
//! The memory-model contract (design §2, condensed): the raw `id` lives as
//! **8 raw bytes in the wrapper's indexable byte tail** — the one storage
//! the collector genuinely never walks. Deliberately NOT `Alien`'s
//! named-slot smi idiom: arm64 tagged-pointer objects (small `NSNumber`s,
//! `NSTaggedPointerString`…) set bit 63 — out of smi range — and a raw
//! word in a *named* slot would be oop-scanned, where an id whose low bits
//! resemble `MEM_TAG` gets "relocated" and corrupts the heap. Byte tail,
//! full 64 bits, no exceptions (the adversarial-review finding that
//! reshaped this module before it was written).
//!
//! Ownership (design §3): every `id` crossing in is retained before boxing
//! (C0 retains unconditionally; the +1-family classifier is C1), `release`
//! poisons the wrapper (zeroed tail — subsequent sends fail cleanly, the
//! terminated-worker discipline), and the bias is ALWAYS leak-side: a leak
//! is diagnosable (`wrap`/`release` counters, `MACVM_TRACE=cocoa`); an
//! over-release corrupts a runtime we don't control.
//!
//! The bottom pool (design §3.1): `autorelease` on a thread with no pool
//! doesn't defer — it leaks outright. So the first Cocoa call on a thread
//! pushes a bottom `NSAutoreleasePool` (via the runtime's C entry points),
//! drained + renewed at doit boundaries (`frontend::classdef` calls
//! [`drain_pool_at_doit_boundary`] after every top-level doit — a natural
//! quiescent point: no Cocoa call is in flight between doits). Same
//! arrangement NewBCPL's Cocoa support landed on (`../MacBCPL`,
//! `docs/memory_model.md`).
//!
//! Threading: C0 is VM-thread-only, Foundation-only. The pool is
//! per-thread (`thread_local!`), so worker VMs each get their own on first
//! use. GC safety needs nothing special: the VM is single-threaded and the
//! collector only runs at safepoints on the VM thread — a thread inside
//! `objc_msgSend` is by definition not at a safepoint.

#![allow(unsafe_code)]

use crate::memory::alloc;
use crate::oops::wrappers::{ByteArrayOop, MemOop};
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;
use std::cell::Cell;
use std::ffi::CString;
use std::os::raw::{c_char, c_void};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

// The shim (objc_shim.m): CALLS the send inside @try; a caught NSException
// comes back as status 1 + description bytes. Never unwinds into Rust.
extern "C" {
    fn macvm_try_msgsend2(
        target: *mut c_void,
        sel: *mut c_void,
        a: *mut c_void,
        b: *mut c_void,
        out: *mut *mut c_void,
        excbuf: *mut c_char,
        cap: u64,
    ) -> i64;
}

/// The libobjc entry points the bridge needs, resolved once. Foundation is
/// dlopen'd alongside so `objc_getClass("NSProcessInfo")` etc. resolve.
struct Syms {
    objc_get_class: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    sel_register_name: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    objc_retain: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    objc_release: unsafe extern "C" fn(*mut c_void),
    pool_push: unsafe extern "C" fn() -> *mut c_void,
    pool_pop: unsafe extern "C" fn(*mut c_void),
}

static SYMS: OnceLock<Option<Syms>> = OnceLock::new();

/// Lifetime wrap/release balance — the design's permanent leak tripwire.
/// Process-wide (all VMs), monotonic; tests assert deltas.
static WRAPS: AtomicU64 = AtomicU64::new(0);
static RELEASES: AtomicU64 = AtomicU64::new(0);

pub fn counters() -> (u64, u64) {
    (
        WRAPS.load(Ordering::Relaxed),
        RELEASES.load(Ordering::Relaxed),
    )
}

thread_local! {
    /// This thread's bottom autorelease pool token (null = none yet).
    static POOL: Cell<*mut c_void> = const { Cell::new(std::ptr::null_mut()) };
}

fn resolve(name: &str) -> Option<u64> {
    crate::vendor::wfasm::native_macos::dlsym_resolve(None, name)
}

fn syms() -> Option<&'static Syms> {
    SYMS.get_or_init(|| {
        // Foundation must be loaded for its classes to be visible to
        // objc_getClass; dlopen-by-path with RTLD_GLOBAL does that. libobjc
        // itself is already linked (build.rs: rustc-link-lib=objc), so its
        // symbols resolve from the process image.
        let _ = crate::vendor::wfasm::native_macos::dlsym_resolve(
            Some("/System/Library/Frameworks/Foundation.framework/Foundation"),
            "NSStringFromClass",
        );
        // objc_autoreleasePoolPush/Pop are the runtime's C entry points for
        // pool management — no NSAutoreleasePool object needed.
        // Each address is transmuted to its exact fn-pointer type; the
        // signatures match libobjc's C ABI (objc/message.h, objc/runtime.h).
        Some(Syms {
            objc_get_class: unsafe {
                std::mem::transmute::<u64, unsafe extern "C" fn(*const c_char) -> *mut c_void>(
                    resolve("objc_getClass")?,
                )
            },
            sel_register_name: unsafe {
                std::mem::transmute::<u64, unsafe extern "C" fn(*const c_char) -> *mut c_void>(
                    resolve("sel_registerName")?,
                )
            },
            objc_retain: unsafe {
                std::mem::transmute::<u64, unsafe extern "C" fn(*mut c_void) -> *mut c_void>(
                    resolve("objc_retain")?,
                )
            },
            objc_release: unsafe {
                std::mem::transmute::<u64, unsafe extern "C" fn(*mut c_void)>(resolve(
                    "objc_release",
                )?)
            },
            pool_push: unsafe {
                std::mem::transmute::<u64, unsafe extern "C" fn() -> *mut c_void>(resolve(
                    "objc_autoreleasePoolPush",
                )?)
            },
            pool_pop: unsafe {
                std::mem::transmute::<u64, unsafe extern "C" fn(*mut c_void)>(resolve(
                    "objc_autoreleasePoolPop",
                )?)
            },
        })
    })
    .as_ref()
}

/// Make sure THIS thread has its bottom pool (design §3.1) — called on the
/// way into every bridged operation.
fn ensure_pool(s: &Syms) {
    POOL.with(|p| {
        if p.get().is_null() {
            p.set(unsafe { (s.pool_push)() });
        }
    });
}

/// Doit-boundary pool hygiene: drain the bottom pool and push a fresh one,
/// releasing every +0 autoreleased object Cocoa handed back during the doit
/// (our wrapped refs survive — retain-on-wrap owns them independently).
/// A cheap no-op on threads that never touched Cocoa.
pub fn drain_pool_at_doit_boundary() {
    POOL.with(|p| {
        let tok = p.get();
        if !tok.is_null() {
            if let Some(s) = syms() {
                unsafe { (s.pool_pop)(tok) };
                p.set(unsafe { (s.pool_push)() });
            }
        }
    });
}

// ── ObjcRef wrapping ────────────────────────────────────────────────────────

/// Read the wrapped `id` out of an `ObjcRef`'s byte tail. `None` if the
/// oop isn't an ObjcRef or the wrapper is poisoned (zero id).
pub fn read_id(vm: &VmState, o: Oop) -> Option<*mut c_void> {
    let m = MemOop::try_from(o)?;
    if m.klass().oop().raw() != vm.universe.objcref_klass.oop().raw() {
        return None;
    }
    let b = ByteArrayOop::try_from(o)?;
    if b.len() != 8 {
        return None;
    }
    let mut raw = [0u8; 8];
    for (i, byte) in raw.iter_mut().enumerate() {
        *byte = b.byte_at(i);
    }
    let id = u64::from_le_bytes(raw);
    if id == 0 {
        None // poisoned (or wrapped nil — never minted, see wrap())
    } else {
        Some(id as *mut c_void)
    }
}

/// Wrap an `id` as a fresh `ObjcRef`, retaining it first (design §3.1 —
/// C0 retains unconditionally; the +1-family classifier is C1). A NULL id
/// answers Smalltalk `nil` — no wrapper is minted for nothing.
/// The raw pointer is a bridge-produced id (`read_id`-validated, a class
/// object, or a runtime return); this module owns the ObjC boundary the way
/// `codecache` owns raw JIT-code pointers.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn wrap(vm: &mut VmState, id: *mut c_void) -> Oop {
    if id.is_null() {
        return vm.universe.nil_obj;
    }
    if let Some(s) = syms() {
        unsafe { (s.objc_retain)(id) };
    }
    WRAPS.fetch_add(1, Ordering::Relaxed);
    let k = vm.universe.objcref_klass;
    let b = alloc::alloc_indexable_bytes(vm, k, 8);
    for (i, byte) in (id as u64).to_le_bytes().iter().enumerate() {
        b.byte_at_put(i, *byte);
    }
    if vm.options.trace.is_enabled("cocoa") {
        let _ = writeln!(vm.out, "[cocoa] wrap {:p}", id);
    }
    b.oop()
}
use std::io::Write;

/// Release-with-poison (design §3.3): `objc_release` the target, then zero
/// the tail so every later send through this wrapper fails cleanly. False
/// if the wrapper was already poisoned (double release refused — the bias
/// is leak-side, never over-release).
pub fn release(vm: &mut VmState, o: Oop) -> bool {
    let Some(id) = read_id(vm, o) else {
        return false;
    };
    if let Some(s) = syms() {
        unsafe { (s.objc_release)(id) };
    }
    RELEASES.fetch_add(1, Ordering::Relaxed);
    let b = ByteArrayOop::try_from(o).expect("read_id proved bytes-ness");
    for i in 0..8 {
        b.byte_at_put(i, 0);
    }
    if vm.options.trace.is_enabled("cocoa") {
        let _ = writeln!(vm.out, "[cocoa] release {:p}", id);
    }
    true
}

// ── sends ───────────────────────────────────────────────────────────────────

/// Look up an Objective-C class by name. NULL if unknown.
pub fn class_named(name: &str) -> Option<*mut c_void> {
    let s = syms()?;
    ensure_pool(s);
    let c = CString::new(name).ok()?;
    let cls = unsafe { (s.objc_get_class)(c.as_ptr()) };
    if cls.is_null() {
        None
    } else {
        Some(cls)
    }
}

/// One bridged send: up to two GPR arguments, GPR result, exception-caught.
/// `Err` carries the caught NSException's description.
/// Bridge-produced pointers only (see `wrap`'s note).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn try_send(
    target: *mut c_void,
    selector: &str,
    a: *mut c_void,
    b: *mut c_void,
) -> Result<*mut c_void, String> {
    let s = syms().ok_or_else(|| "Objective-C runtime unavailable".to_string())?;
    ensure_pool(s);
    let csel = CString::new(selector).map_err(|_| "selector contains NUL".to_string())?;
    let sel = unsafe { (s.sel_register_name)(csel.as_ptr()) };
    let mut out: *mut c_void = std::ptr::null_mut();
    let mut excbuf = [0u8; 512];
    let rc = unsafe {
        macvm_try_msgsend2(
            target,
            sel,
            a,
            b,
            &mut out,
            excbuf.as_mut_ptr() as *mut c_char,
            excbuf.len() as u64,
        )
    };
    if rc == 0 {
        Ok(out)
    } else {
        let end = excbuf.iter().position(|&c| c == 0).unwrap_or(excbuf.len());
        Err(String::from_utf8_lossy(&excbuf[..end]).into_owned())
    }
}

/// A send whose result is an `NSString`: copies its UTF-8 bytes out
/// immediately (the intermediate NSString stays +0 under the bottom pool —
/// never wrapped, never retained; the bytes are owned Rust before any
/// allocation can move anything).
pub fn try_send_string(target: *mut c_void, selector: &str) -> Result<Vec<u8>, String> {
    let ns = try_send(target, selector, std::ptr::null_mut(), std::ptr::null_mut())?;
    if ns.is_null() {
        return Err(format!("{selector} answered nil, not a string"));
    }
    let utf8 = try_send(ns, "UTF8String", std::ptr::null_mut(), std::ptr::null_mut())?;
    if utf8.is_null() {
        return Err("UTF8String answered NULL".to_string());
    }
    let cstr = unsafe { std::ffi::CStr::from_ptr(utf8 as *const c_char) };
    Ok(cstr.to_bytes().to_vec())
}

/// Build an `NSString` from Rust bytes (`stringWithUTF8String:` — a +0
/// return; the caller wraps it, which retains).
pub fn nsstring_from(bytes: &[u8]) -> Result<*mut c_void, String> {
    let cls = class_named("NSString").ok_or_else(|| "NSString class missing".to_string())?;
    let c = CString::new(bytes).map_err(|_| "string contains NUL".to_string())?;
    let ns = try_send(
        cls,
        "stringWithUTF8String:",
        c.as_ptr() as *mut c_void,
        std::ptr::null_mut(),
    )?;
    if ns.is_null() {
        Err("stringWithUTF8String: answered nil".to_string())
    } else {
        Ok(ns)
    }
}
