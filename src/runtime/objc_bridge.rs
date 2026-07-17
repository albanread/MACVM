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
use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_void};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

// The shim (objc_shim.m): CALLS the send inside @try; a caught NSException
// comes back as status 1 + description bytes. Never unwinds into Rust.
// One general entry (C1): the full AAPCS64 outgoing model — 6 GPR words
// (x2..x7), 8 FPR doubles (d0..d7), 4 spilled stack words — plus a
// return-kind token selecting which result registers the shim reads.
extern "C" {
    #[allow(clippy::too_many_arguments)]
    fn macvm_try_msgsend(
        target: *mut c_void,
        sel: *mut c_void,
        gpr: *const u64,
        fpr: *const f64,
        stack: *const u64,
        ret_kind: i64,
        out_gpr: *mut u64,
        out_fpr: *mut f64,
        excbuf: *mut c_char,
        cap: u64,
    ) -> i64;
}

/// Which result registers a send reads back — mirrors `objc_shim.m`'s
/// `MACVM_RET_*` tokens (keep in sync). The C1 vocabulary is exactly the
/// register-classifiable subset of `cocoa_data`'s ABI tokens: `g`, `f`,
/// `h2`, `h4`, `i2`, `v` (docs/FFI.md §1); larger/sret shapes are deferred.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RetKind {
    /// id / NSInteger / BOOL / pointer — `x0`.
    Gpr = 0,
    /// double / CGFloat — `d0`.
    Fpr = 1,
    /// CGPoint / CGSize (2-double HFA) — `d0..d1`.
    Hfa2 = 2,
    /// CGRect (4-double HFA) — `d0..d3`.
    Hfa4 = 3,
    /// NSRange (16-byte integer composite) — `x0..x1`.
    IntPair = 4,
    /// void — nothing read.
    Void = 5,
    /// float — `s0`, widened to double by the shim.
    F32 = 6,
}

/// A general send's raw result registers: `gpr` holds `x0`/`x1`, `fpr`
/// holds `d0..d3`. Which of them mean anything is the caller's `RetKind`.
#[derive(Clone, Copy, Default)]
pub struct SendOut {
    pub gpr: [u64; 2],
    pub fpr: [f64; 4],
}

/// The AAPCS64 argument-slot capacities the general send offers (mirroring
/// the shim's fixed shape): 6 GPR words (x2..x7 — x0/x1 are self/_cmd),
/// 8 FPR doubles, 4 shared spill/stack words.
pub const SEND_GPR_SLOTS: usize = 6;
pub const SEND_FPR_SLOTS: usize = 8;
pub const SEND_STACK_SLOTS: usize = 4;

/// The libobjc entry points the bridge needs, resolved once. Foundation is
/// dlopen'd alongside so `objc_getClass("NSProcessInfo")` etc. resolve.
struct Syms {
    objc_get_class: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    sel_register_name: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    objc_retain: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    objc_release: unsafe extern "C" fn(*mut c_void),
    pool_push: unsafe extern "C" fn() -> *mut c_void,
    pool_pop: unsafe extern "C" fn(*mut c_void),
    // C2 shape resolution: ask the LIVE runtime for a method's @encode
    // signature. `object_getClass` answers the metaclass for a class
    // object, whose *instance* methods ARE the class methods — so
    // `class_getInstanceMethod(object_getClass(target), sel)` resolves
    // uniformly for instance and class sends.
    object_get_class: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    class_get_instance_method: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void,
    method_get_type_encoding: unsafe extern "C" fn(*mut c_void) -> *const c_char,
}

static SYMS: OnceLock<Option<Syms>> = OnceLock::new();

/// Lifetime wrap/release balance — the design's permanent leak tripwire.
/// Process-wide (all VMs), monotonic; tests assert deltas. `CONSUMED`
/// counts init-family ownership transfers (design §3.2): the receiver
/// wrapper gave up its +1 TO the init call rather than to `objc_release`,
/// so quiescent balance is `WRAPS == RELEASES + CONSUMED`.
static WRAPS: AtomicU64 = AtomicU64::new(0);
static RELEASES: AtomicU64 = AtomicU64::new(0);
static CONSUMED: AtomicU64 = AtomicU64::new(0);

pub fn counters() -> (u64, u64, u64) {
    (
        WRAPS.load(Ordering::Relaxed),
        RELEASES.load(Ordering::Relaxed),
        CONSUMED.load(Ordering::Relaxed),
    )
}

thread_local! {
    /// This thread's bottom autorelease pool token (null = none yet).
    /// MAIN-thread caveat (C3): a hop's `ensure_pool` on main nests our
    /// token inside whatever pool scope AppKit/GCD had open; their pops
    /// drain it, leaving this token stale. Harmless as long as
    /// `drain_pool_at_doit_boundary` is only ever called from VM-thread
    /// sites (it is: the doit boundary and prim 238) — never call it on
    /// main.
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
            object_get_class: unsafe {
                std::mem::transmute::<u64, unsafe extern "C" fn(*mut c_void) -> *mut c_void>(
                    resolve("object_getClass")?,
                )
            },
            class_get_instance_method: unsafe {
                std::mem::transmute::<
                    u64,
                    unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void,
                >(resolve("class_getInstanceMethod")?)
            },
            method_get_type_encoding: unsafe {
                std::mem::transmute::<u64, unsafe extern "C" fn(*mut c_void) -> *const c_char>(
                    resolve("method_getTypeEncoding")?,
                )
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
    let o = b.oop();
    mint_note(vm, o);
    o
}
use std::io::Write;

// ── the +1-family classifier (design §3.2, C1) ─────────────────────────────

/// What a selector's name promises about its result's ownership — ARC's
/// exact method-family analysis, mechanized.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    /// Ordinary selector: the result is +0 (borrowed/autoreleased) — the
    /// bridge retains on wrap.
    Plus0,
    /// `alloc` / `new` / `copy` / `mutableCopy` family: the result is
    /// already +1 (caller-owned) — wrapping must NOT retain again.
    Plus1,
    /// `init` family: +1 result AND the receiver's own +1 was consumed by
    /// the call (class clusters may return a different object) — the
    /// receiver's wrapper must be poisoned-without-release.
    Init,
}

/// ARC's family rule, verbatim (clang's `objc-arc` docs): ignoring leading
/// underscores, the selector's first component begins with the family name
/// followed by a character that is NOT a lowercase letter. So `copy`,
/// `copyWithZone:`, `_copyDeep` are in the copy family; `copyright` and
/// `newButtonTitle` and `initialize` are NOT in any family.
pub fn selector_family(selector: &str) -> Family {
    let s = selector.trim_start_matches('_');
    let in_family = |prefix: &str| -> bool {
        s.strip_prefix(prefix)
            .is_some_and(|rest| !rest.chars().next().is_some_and(|c| c.is_ascii_lowercase()))
    };
    if in_family("init") {
        Family::Init
    } else if in_family("alloc")
        || in_family("new")
        || in_family("copy")
        || in_family("mutableCopy")
    {
        Family::Plus1
    } else {
        Family::Plus0
    }
}

/// Wrap a send RESULT, honoring the producing selector's family: a
/// +1-family result is already caller-owned, so the bridge retain is
/// skipped (retaining again would leak); everything else wraps normally.
/// The `WRAPS` counter still ticks either way — it counts "wrapper minted
/// owning one strong reference," which is true in both cases.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn wrap_result(vm: &mut VmState, id: *mut c_void, selector: &str) -> Oop {
    if id.is_null() {
        return vm.universe.nil_obj;
    }
    match selector_family(selector) {
        Family::Plus0 => wrap(vm, id),
        Family::Plus1 | Family::Init => wrap_owned(vm, id),
    }
}

/// Mint a wrapper for an id whose +1 is ALREADY in hand (a +1-family
/// result, or a hopped result whose retain happened ON the main thread
/// inside the hop — the C3 review's cross-thread-autorelease fix): no
/// bridge retain, ownership recorded as-is. NULL answers `nil`.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn wrap_owned(vm: &mut VmState, id: *mut c_void) -> Oop {
    if id.is_null() {
        return vm.universe.nil_obj;
    }
    WRAPS.fetch_add(1, Ordering::Relaxed);
    let k = vm.universe.objcref_klass;
    let b = alloc::alloc_indexable_bytes(vm, k, 8);
    for (i, byte) in (id as u64).to_le_bytes().iter().enumerate() {
        b.byte_at_put(i, *byte);
    }
    if vm.options.trace.is_enabled("cocoa") {
        let _ = writeln!(vm.out, "[cocoa] wrap {:p} (already owned, no retain)", id);
    }
    let o = b.oop();
    mint_note(vm, o);
    o
}

/// The init-family receiver transfer (design §3.2): after a successful
/// init-family send the receiver's +1 belongs to the callee — poison the
/// wrapper WITHOUT `objc_release` (its reference was consumed, not
/// dropped), mechanizing "always use the init result, never the alloc
/// result." Balance accounting goes to `CONSUMED`. No-op (false) if the
/// oop isn't a live ObjcRef. Never allocates — safe to call while other
/// raw ids are in hand.
pub fn consume_receiver(vm: &mut VmState, o: Oop) -> bool {
    let Some(id) = read_id(vm, o) else {
        return false;
    };
    let b = ByteArrayOop::try_from(o).expect("read_id proved bytes-ness");
    for i in 0..8 {
        b.byte_at_put(i, 0);
    }
    CONSUMED.fetch_add(1, Ordering::Relaxed);
    if vm.options.trace.is_enabled("cocoa") {
        let _ = writeln!(vm.out, "[cocoa] consume {:p} (init family transfer)", id);
    }
    true
}

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

// ── C2: shape resolution from the live runtime ─────────────────────────────
//
// The bridge never guesses a method's ABI: it asks libobjc for the method's
// `@encode` signature (`method_getTypeEncoding`) and parses it into a
// [`Shape`] — the same information `cocoa_data`'s offline tables mirror,
// taken from the authoritative source at runtime. Parsed shapes are cached
// per (class, selector) with hit/miss counters (the design's "PIC-cached
// resolution" — one process-wide map rather than per-send-site PICs in C2;
// per-site caching can ride the real PIC entries when DNU sends compile).

/// One argument's marshalling class, from its `@encode` token. Integer
/// tokens carry their DECLARED width: the marshaller range-checks against
/// it, and any argument narrower than 8 bytes REFUSES to spill to the
/// stack (Darwin packs non-variadic stack args to natural size — an
/// 8-byte spill word for a declared `int` would shift every later stack
/// offset; the C2 review's mis-marshal finding).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjcArg {
    /// `@` object / `#` Class — an ObjcRef, nil, or a String (temp NSString).
    Id,
    /// `B` — true/false (or 0/1). 8 bits: never spills.
    Bool,
    /// `c s i l q` / `C S I L Q` — a SmallInteger, range-checked against
    /// the declared width (`l`/`L` are 32-bit BY DEFINITION in @encode).
    /// Only the 64-bit widths may spill.
    Int { signed: bool, bits: u8 },
    /// `d` — a Double or a SmallInteger (coerced; the register class is
    /// decided by the CALLEE's signature now, not the argument's tag).
    F64,
    /// `f` — as F64, then narrowed; the f32 bits ride the d-register's low
    /// half, exactly where the callee's `s` register view reads. Never
    /// spills (a spilled float packs to 4 bytes).
    F32,
    /// `:` — a selector name as a String/Symbol.
    Sel,
    /// `{CGPoint=dd}` / `{CGSize=dd}` — an Array of 2 numbers (2 FPR slots).
    Point,
    /// `{CGRect=...}` — an Array of 4 numbers (4 FPR slots).
    Rect,
    /// `{_NSRange=QQ}` — an Array of 2 non-negative SmallIntegers (2 GPRs).
    Range,
}

/// A method's return class, from its `@encode` token. Integer returns
/// carry their width: the result is truncated to the declared width and
/// sign-/zero-extended from there (the #i32 lesson generalized — a `c`
/// return is a signed char answered as a SmallInteger, NOT a Boolean:
/// on arm64 BOOL encodes as `B`, so a live `c` is a genuine char).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjcRet {
    Id,
    /// `B` only.
    Bool,
    Int {
        signed: bool,
        bits: u8,
    },
    F64,
    F32,
    Void,
    Point,
    Rect,
    Range,
    /// `*` — a C string, copied out.
    CharStar,
}

/// A resolved method shape: what to marshal in, what to read back.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Shape {
    pub ret: ObjcRet,
    pub args: Vec<ObjcArg>,
}

/// Skip an `@encode` token's method-signature qualifiers (`r n N o O R V`)
/// and return the rest.
fn skip_qualifiers(s: &str) -> &str {
    s.trim_start_matches(['r', 'n', 'N', 'o', 'O', 'R', 'V'])
}

/// Consume ONE type token from the front of `s`; answer (token, rest).
/// Struct/array/union tokens are consumed with brace matching.
fn split_type(s: &str) -> Option<(&str, &str)> {
    let s = skip_qualifiers(s);
    let mut chars = s.char_indices();
    let (_, first) = chars.next()?;
    let (open, close) = match first {
        '{' => ('{', '}'),
        '[' => ('[', ']'),
        '(' => ('(', ')'),
        '^' => {
            // A pointer: `^` then one pointee type.
            let (_, rest) = split_type(&s[1..])?;
            let taken = s.len() - rest.len();
            return Some((&s[..taken], rest));
        }
        _ => return Some(s.split_at(1)),
    };
    let mut depth = 1usize;
    for (i, c) in chars {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Some(s.split_at(i + 1));
            }
        }
    }
    None // unbalanced — malformed encoding
}

/// An integer token's (signedness, width). `l`/`L` are 32-bit BY
/// DEFINITION in @encode, even on LP64 (Apple's type-encoding doc).
fn int_width(byte: u8) -> Option<(bool, u8)> {
    match byte {
        b'c' => Some((true, 8)),
        b's' => Some((true, 16)),
        b'i' | b'l' => Some((true, 32)),
        b'q' => Some((true, 64)),
        b'C' => Some((false, 8)),
        b'S' => Some((false, 16)),
        b'I' | b'L' => Some((false, 32)),
        b'Q' => Some((false, 64)),
        _ => None,
    }
}

/// Classify one consumed type token as an argument class. `None` = a shape
/// C2 doesn't marshal (raw pointers, unknown structs, unions, arrays…).
fn classify_arg(tok: &str) -> Option<ObjcArg> {
    let first = *tok.as_bytes().first()?;
    if let Some((signed, bits)) = int_width(first) {
        return Some(ObjcArg::Int { signed, bits });
    }
    match first {
        b'@' | b'#' => Some(ObjcArg::Id),
        b'B' => Some(ObjcArg::Bool),
        b'd' => Some(ObjcArg::F64),
        b'f' => Some(ObjcArg::F32),
        b':' => Some(ObjcArg::Sel),
        b'{' => match struct_name(tok)? {
            "CGPoint" | "NSPoint" | "CGSize" | "NSSize" => Some(ObjcArg::Point),
            "CGRect" | "NSRect" => Some(ObjcArg::Rect),
            "_NSRange" | "NSRange" => Some(ObjcArg::Range),
            _ => None,
        },
        _ => None,
    }
}

fn classify_ret(tok: &str) -> Option<ObjcRet> {
    let first = *tok.as_bytes().first()?;
    if let Some((signed, bits)) = int_width(first) {
        return Some(ObjcRet::Int { signed, bits });
    }
    match first {
        b'@' | b'#' => Some(ObjcRet::Id),
        b'B' => Some(ObjcRet::Bool),
        b'd' => Some(ObjcRet::F64),
        b'f' => Some(ObjcRet::F32),
        b'v' => Some(ObjcRet::Void),
        b'*' => Some(ObjcRet::CharStar),
        b'{' => match struct_name(tok)? {
            "CGPoint" | "NSPoint" | "CGSize" | "NSSize" => Some(ObjcRet::Point),
            "CGRect" | "NSRect" => Some(ObjcRet::Rect),
            "_NSRange" | "NSRange" => Some(ObjcRet::Range),
            _ => None,
        },
        _ => None,
    }
}

/// `{CGRect={CGPoint=dd}{CGSize=dd}}` → `CGRect`.
fn struct_name(tok: &str) -> Option<&str> {
    let inner = tok.strip_prefix('{')?;
    let end = inner.find(['=', '}'])?;
    Some(&inner[..end])
}

/// Parse a full method `@encode` string (`ret self _cmd args…`, each type
/// followed by ignorable stack-offset digits) into a [`Shape`]. `None` for
/// anything C2 can't marshal — the caller fails cleanly.
pub fn parse_type_encoding(enc: &str) -> Option<Shape> {
    fn strip_digits(s: &str) -> &str {
        s.trim_start_matches(|c: char| c.is_ascii_digit())
    }
    let (ret_tok, rest) = split_type(enc)?;
    let ret = classify_ret(ret_tok)?;
    // self (`@`) and _cmd (`:`), mandatory in every method signature.
    let rest = strip_digits(rest);
    let (self_tok, rest) = split_type(rest)?;
    if !self_tok.starts_with('@') {
        return None;
    }
    let rest = strip_digits(rest);
    let (cmd_tok, mut rest) = split_type(rest)?;
    if !cmd_tok.starts_with(':') {
        return None;
    }
    let mut args = Vec::new();
    loop {
        rest = strip_digits(rest);
        if rest.is_empty() {
            return Some(Shape { ret, args });
        }
        let (tok, r) = split_type(rest)?;
        args.push(classify_arg(tok)?);
        rest = r;
    }
}

/// The process-wide shape cache + the design's visible hit-rate counters.
/// Keyed by (Class pointer, selector). ObjC classes are never unloaded on
/// macOS, so a Class pointer is a stable key for the process's lifetime.
/// A cached `None` ("no such method") is sticky by design — categories
/// loaded AFTER first resolution won't be seen for that exact class×
/// selector until restart, an accepted C2 simplification.
type ShapeCache = std::sync::RwLock<HashMap<(u64, String), Option<Shape>>>;
static SHAPES: OnceLock<ShapeCache> = OnceLock::new();
static SHAPE_HITS: AtomicU64 = AtomicU64::new(0);
static SHAPE_MISSES: AtomicU64 = AtomicU64::new(0);

pub fn shape_stats() -> (u64, u64) {
    (
        SHAPE_HITS.load(Ordering::Relaxed),
        SHAPE_MISSES.load(Ordering::Relaxed),
    )
}

/// Resolve `selector`'s ABI shape on `target`'s class, from the cache or
/// the live runtime. `None`: no such method, or a shape C2 can't marshal.
/// Bridge-produced pointers only (see `wrap`'s note).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn resolve_shape(target: *mut c_void, selector: &str) -> Option<Shape> {
    let s = syms()?;
    let cls = unsafe { (s.object_get_class)(target) };
    if cls.is_null() {
        return None;
    }
    let key = (cls as u64, selector.to_string());
    let cache = SHAPES.get_or_init(|| std::sync::RwLock::new(HashMap::new()));
    if let Some(hit) = cache.read().ok()?.get(&key) {
        SHAPE_HITS.fetch_add(1, Ordering::Relaxed);
        return hit.clone();
    }
    SHAPE_MISSES.fetch_add(1, Ordering::Relaxed);
    let csel = CString::new(selector).ok()?;
    let sel = unsafe { (s.sel_register_name)(csel.as_ptr()) };
    let m = unsafe { (s.class_get_instance_method)(cls, sel) };
    if m.is_null() {
        // The class doesn't implement it TODAY — deliberately NOT cached:
        // a framework/category loaded later (S20's dlopen) can add methods
        // to existing classes, and a sticky negative entry would keep the
        // selector dead forever (C2 review). Missing methods are the rare
        // path; re-probing them is cheap.
        return None;
    }
    let enc = unsafe { (s.method_get_type_encoding)(m) };
    let shape = if enc.is_null() {
        None
    } else {
        let text = unsafe { std::ffi::CStr::from_ptr(enc) }
            .to_string_lossy()
            .into_owned();
        // An unparseable encoding IS cached (as None): it's a stable
        // property of an existing method, not a loadable gap.
        parse_type_encoding(&text)
    };
    cache.write().ok()?.insert(key, shape.clone());
    shape
}

/// Selectors the bridge refuses to send RAW: manual reference-counting and
/// deallocation belong to the bridge's own ownership machinery (`wrap`/
/// `release`/the pool), never to guest code — `dealloc` through DNU is a
/// use-after-free, a raw `retain`/`autorelease` unbalances counts the
/// counters can't see (C2 review). Exact matches only: `retainCount` is
/// blocked too (meaningless under ARC-era runtimes and an attractive
/// nuisance); family selectors like `retainedValue` are NOT blocked.
pub fn is_manual_memory_selector(selector: &str) -> bool {
    matches!(
        selector,
        "retain" | "release" | "autorelease" | "dealloc" | "retainCount"
    )
}

// ── C3: the sync main-thread hop (design §4 path 2) ─────────────────────────
//
// AppKit is main-thread-only; the VM lives on its own thread (S21). The
// hop is `dispatch_sync_f` onto the main queue — the C-function form of
// the design's performSelectorOnMainThread mechanism, no Blocks runtime
// needed. Sync-waiting on another thread is safe HERE by architectural
// invariant: the main thread never synchronously waits on the VM
// (vm_host is async by construction — submit + drain-on-wake, S21/M3),
// so no wait cycle can close. The degenerate case — already ON main
// (a Cocoa callback context) — runs inline: that IS the correct sync
// semantics and the non-deadlocking one.
//
// The hop only completes if something drains the main queue (the GUI's
// NSApplication run loop). A headless host has no drain, so the hop is
// GATED: [`enable_main_hop`] is called by a GUI host once its run loop is
// (about to be) live; un-enabled, the hop fails cleanly instead of
// hanging forever.

// libdispatch/pthread entry points — plain libSystem symbols, linked, not
// dlsym'd. `_dispatch_main_q` is the main queue object itself (what the
// dispatch_get_main_queue() macro takes the address of).
#[repr(C)]
struct DispatchQueueS {
    _opaque: [u8; 0],
}
extern "C" {
    static _dispatch_main_q: DispatchQueueS;
    fn dispatch_sync_f(queue: *mut c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn pthread_main_np() -> std::os::raw::c_int;
}

static MAIN_HOP_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Hop traffic counters: (dispatched-to-main, ran-inline-already-on-main).
static HOPS_DISPATCHED: AtomicU64 = AtomicU64::new(0);
static HOPS_INLINE: AtomicU64 = AtomicU64::new(0);

/// A GUI host calls this once at startup, when the main run loop is (about
/// to be) live and will drain the main queue. Never call it from a
/// headless host: an un-drained main queue turns every hop into a hang,
/// which is exactly what the un-enabled clean failure exists to prevent.
pub fn enable_main_hop() {
    MAIN_HOP_ENABLED.store(true, Ordering::Release);
}

pub fn main_hop_enabled() -> bool {
    MAIN_HOP_ENABLED.load(Ordering::Acquire)
}

/// CG2 (design §8): the AppKit main-thread guard is armed ONLY in the native
/// Cocoa GUI (`macvm-cocoa`), where a UI worker owns the main thread and the
/// primary/compute workers must never touch AppKit directly. It stays OFF for
/// every other host — crucially the shipping WKWebView GUI, where the single VM
/// runs on a *worker* thread and legitimately resolves an AppKit class off-main
/// as the first half of the C3 resolve-then-`onMain` pattern (CocoaPad, C5). A
/// guard that fired there would break that shipping demo, so it is opt-in.
static COCOA_UI_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `macvm-cocoa` calls this once at startup (design §3): from here on the main
/// thread is AppKit's, and an off-main AppKit *send* from a background VM is a
/// bug the guard refuses loudly. Idempotent; never unset.
pub fn enable_cocoa_ui_mode() {
    COCOA_UI_MODE.store(true, Ordering::Release);
}

/// Is the native Cocoa GUI's main-thread guard armed? See [`enable_cocoa_ui_mode`].
pub fn cocoa_ui_mode() -> bool {
    COCOA_UI_MODE.load(Ordering::Acquire)
}

pub fn hop_stats() -> (u64, u64) {
    (
        HOPS_DISPATCHED.load(Ordering::Relaxed),
        HOPS_INLINE.load(Ordering::Relaxed),
    )
}

/// [`try_send_full`], executed on the MAIN thread, synchronously, with
/// result OWNERSHIP taken on main too (the C3 review's cross-thread
/// autorelease finding): a `+0` object result lives in the MAIN thread's
/// autorelease pool, and main keeps popping pools concurrently once the
/// hop returns — so the retain (`retain_result`) and any C-string copy
/// (`copy_cstr`) must happen INSIDE the hop, before main moves on. By the
/// time this returns, an object result is +1 in hand (wrap it with
/// [`wrap_owned`], never a retaining wrap) and string bytes are owned
/// Rust memory.
///
/// Every value that crosses is a plain native word or owned Rust — no oop
/// is ever visible to the main thread, and the VM thread is blocked (not
/// at a safepoint) for the duration, so no GC can run under the hop. The
/// @try exception shim runs ON main; a caught NSException crosses back as
/// the ordinary `Err` description. Honest residuals (C3 review): a
/// non-ObjC C++ exception inside a hopped call unwinds on MAIN and kills
/// the app (the same @catch(id) hole C0 accepted, relocated); a SIGSEGV
/// in hopped framework code faults on main, which has no PROBE jmp slot —
/// the process dies honestly rather than siglongjmp-ing across Apple's
/// frames. And the standing misuse warning: driving AppKit from a NON-main
/// thread via the plain send path (bypassing this hop) can interleave with
/// AppKit's own internal main dispatches — use the hop for AppKit, always.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::type_complexity)]
pub fn try_send_full_on_main_owned(
    target: *mut c_void,
    selector: &str,
    gpr: &[u64; SEND_GPR_SLOTS],
    fpr: &[f64; SEND_FPR_SLOTS],
    stack: &[u64; SEND_STACK_SLOTS],
    ret: RetKind,
    retain_result: bool,
    copy_cstr: bool,
) -> Result<(SendOut, Option<Vec<u8>>), String> {
    // The post-send ownership step, shared by the inline and dispatched
    // legs — it must run on whichever thread ran the send, BEFORE that
    // thread's pool can pop.
    fn own_result(
        out: &SendOut,
        retain_result: bool,
        copy_cstr: bool,
    ) -> Result<Option<Vec<u8>>, String> {
        if retain_result {
            let p = out.gpr[0] as *mut c_void;
            if !p.is_null() {
                if let Some(s) = syms() {
                    unsafe { (s.objc_retain)(p) };
                }
            }
        }
        if copy_cstr {
            return match cstr_bytes(out.gpr[0] as *const c_char) {
                Some(b) => Ok(Some(b)),
                None => Err("char* result was NULL".to_string()),
            };
        }
        Ok(None)
    }

    if unsafe { pthread_main_np() } == 1 {
        // Already on main (a callback context): inline IS the sync hop —
        // the degenerate, non-deadlocking case the design's "assert not
        // main" clause became.
        HOPS_INLINE.fetch_add(1, Ordering::Relaxed);
        let out = try_send_full(target, selector, gpr, fpr, stack, ret)?;
        let bytes = own_result(&out, retain_result, copy_cstr)?;
        return Ok((out, bytes));
    }
    if !main_hop_enabled() {
        return Err(
            "main-thread hop unavailable: no main run loop is draining the queue \
             (a GUI host enables it; headless VMs run Foundation on the VM thread)"
                .to_string(),
        );
    }

    struct HopCtx<'a> {
        target: *mut c_void,
        selector: &'a str,
        gpr: &'a [u64; SEND_GPR_SLOTS],
        fpr: &'a [f64; SEND_FPR_SLOTS],
        stack: &'a [u64; SEND_STACK_SLOTS],
        ret: RetKind,
        retain_result: bool,
        copy_cstr: bool,
        out: Option<Result<(SendOut, Option<Vec<u8>>), String>>,
    }
    extern "C" fn hop_exec(ctx: *mut c_void) {
        // Runs ON the main thread, inside dispatch_sync_f — the caller's
        // stack frame (and everything HopCtx borrows) is pinned until
        // this returns; the write to `out` happens-before the caller
        // resumes (dispatch_sync_f is a full synchronization point).
        // Nothing here can realistically panic (CString/syms are fallible,
        // excbuf decoding is lossy) — a panic in an extern "C" fn would
        // abort, which is the honest outcome for a broken invariant on
        // the UI thread.
        let c = unsafe { &mut *(ctx as *mut HopCtx) };
        c.out = Some(
            try_send_full(c.target, c.selector, c.gpr, c.fpr, c.stack, c.ret).and_then(|out| {
                let bytes = own_result(&out, c.retain_result, c.copy_cstr)?;
                Ok((out, bytes))
            }),
        );
    }
    let mut ctx = HopCtx {
        target,
        selector,
        gpr,
        fpr,
        stack,
        ret,
        retain_result,
        copy_cstr,
        out: None,
    };
    unsafe {
        dispatch_sync_f(
            std::ptr::addr_of!(_dispatch_main_q) as *mut c_void,
            &mut ctx as *mut HopCtx as *mut c_void,
            hop_exec,
        );
    }
    HOPS_DISPATCHED.fetch_add(1, Ordering::Relaxed);
    ctx.out
        .unwrap_or_else(|| Err("main-thread hop completed without a result".to_string()))
}

/// The plain-registers hop (no ownership post-step) — for results that are
/// VALUES (BOOL/int/float/struct registers), where nothing pool-owned
/// crosses back. Object/char* results must use
/// [`try_send_full_on_main_owned`] instead.
pub fn try_send_full_on_main(
    target: *mut c_void,
    selector: &str,
    gpr: &[u64; SEND_GPR_SLOTS],
    fpr: &[f64; SEND_FPR_SLOTS],
    stack: &[u64; SEND_STACK_SLOTS],
    ret: RetKind,
) -> Result<SendOut, String> {
    try_send_full_on_main_owned(target, selector, gpr, fpr, stack, ret, false, false)
        .map(|(out, _)| out)
}

// ── sends ───────────────────────────────────────────────────────────────────

/// Copy a runtime-owned C string to owned bytes (a `*` return — valid
/// under the bottom pool; copy before anything can drain it).
/// Bridge-produced pointers only (see `wrap`'s note).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn cstr_bytes(p: *const c_char) -> Option<Vec<u8>> {
    if p.is_null() {
        return None;
    }
    Some(unsafe { std::ffi::CStr::from_ptr(p) }.to_bytes().to_vec())
}

/// Intern an Objective-C selector by name (for `:`-class arguments).
pub fn register_selector(name: &str) -> Option<*mut c_void> {
    let s = syms()?;
    let c = CString::new(name).ok()?;
    Some(unsafe { (s.sel_register_name)(c.as_ptr()) })
}

// ── AppKit main-thread guard (cocoa_gui_design.md §8) ─────────────────────────
//
// Only the UI worker — pinned to the process's main thread by AppKit — may
// touch AppKit; the primary and compute-worker VMs live on background threads
// and have no business building windows or views. The moment two VMs coexist
// (the Cocoa GUI), an AppKit-class send from a background VM is a bug that must
// fail LOUDLY rather than silently corrupt AppKit's main-thread-only state.
// This is the in-bridge guard `cocoa_bridge_design.md` §7 explicitly deferred.
//
// The list is a CURATED set of exact AppKit UI class names, NOT an `NS*` prefix:
// Foundation is also `NS*` (NSString/NSNumber/NSProcessInfo/NSCalendar/…), is
// safe on any thread, and the headless bridge gates depend on it — so a prefix
// match would wrongly strangle Foundation. Start small (design §8): exactly the
// UI classes the IDE builds. Grow it as views are added (CG5+).
const APPKIT_GUARDED_CLASSES: &[&str] = &[
    "NSApplication",
    "NSWindow",
    "NSView",
    "NSMenu",
    "NSMenuItem",
    "NSButton",
    "NSTextView",
    "NSTextField",
    "NSTableView",
    "NSOutlineView",
    "NSScrollView",
];

/// Is `name` an AppKit UI class the main-thread guard protects? An exact-name
/// membership test against [`APPKIT_GUARDED_CLASSES`] — deliberately NOT a
/// prefix test (Foundation shares the `NS` prefix; see the list's note above).
pub fn is_appkit_ui_class(name: &str) -> bool {
    APPKIT_GUARDED_CLASSES.contains(&name)
}

/// Are we on the process's main thread — the only thread AppKit may be driven
/// from? A thin wrap of `pthread_main_np()` (nonzero on main).
pub fn on_main_thread() -> bool {
    unsafe { pthread_main_np() == 1 }
}

/// The pure guard decision (design §8), split out so ALL three inputs are
/// explicit and BOTH outcomes are deterministically unit-testable off any
/// thread. A class is refused ONLY when all of: the native Cocoa GUI is running
/// (`cocoa_mode`), the class is an AppKit UI class, and the caller is off the
/// main thread. Everything else — any class outside Cocoa mode, any Foundation
/// class, any class on the main thread — is allowed. Gating on `cocoa_mode` is
/// what keeps the shipping WKWebView GUI working: there the single VM runs on a
/// worker thread and legitimately resolves an AppKit class off-main before the
/// C3 `onMain` hop (CocoaPad, C5), which a thread-only guard would wrongly break.
fn appkit_guard(name: &str, on_main: bool, cocoa_mode: bool) -> Result<(), String> {
    if cocoa_mode && is_appkit_ui_class(name) && !on_main {
        return Err(format!(
            "AppKit class '{name}' may only be used from the main-thread UI worker \
             (cocoa_gui_design.md §8); this send came from a background VM — refused"
        ));
    }
    Ok(())
}

/// Enforce the AppKit main-thread guard for `name` on the CURRENT thread
/// (design §8). Armed only in the native Cocoa GUI ([`cocoa_ui_mode`]); a no-op
/// for every other host (the WKWebView GUI, the CLI, the test suite). When
/// armed: `Ok` for every Foundation class regardless of thread, and for any
/// class on the main thread; `Err` (loud, describing the refusal) only for an
/// AppKit UI class off the main thread. Checked at the Smalltalk-facing
/// `primClassNamed:` primitive for a loud transcript line; class *resolution*
/// itself is deliberately NOT blocked (it is thread-safe and the legitimate
/// first half of the C3 resolve-then-`onMain` pattern).
pub fn check_appkit_main_thread(name: &str) -> Result<(), String> {
    // Foundation (and any non-AppKit class) short-circuits before any atomic or
    // pthread call — the common path pays only a short list scan.
    if !is_appkit_ui_class(name) {
        return Ok(());
    }
    appkit_guard(name, on_main_thread(), cocoa_ui_mode())
}

/// Look up an Objective-C class by name. NULL if unknown.
pub fn class_named(name: &str) -> Option<*mut c_void> {
    // Design §8, ARMED ONLY IN THE NATIVE COCOA GUI ([`cocoa_ui_mode`]): a
    // background VM may not reach AppKit UI classes. Foundation is never
    // blocked; the main-thread UI worker is always allowed; every non-Cocoa
    // host (the WKWebView GUI, the CLI, tests) is unaffected — there this guard
    // is a no-op and off-main AppKit resolution (the C3 resolve-then-`onMain`
    // pattern) proceeds as always.
    if check_appkit_main_thread(name).is_err() {
        return None;
    }
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

/// The general bridged send (C1): the full marshaled register model in,
/// the raw result registers out, exception-caught. `Err` carries the
/// caught NSException's description. Bridge-produced pointers only (see
/// `wrap`'s note).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn try_send_full(
    target: *mut c_void,
    selector: &str,
    gpr: &[u64; SEND_GPR_SLOTS],
    fpr: &[f64; SEND_FPR_SLOTS],
    stack: &[u64; SEND_STACK_SLOTS],
    ret: RetKind,
) -> Result<SendOut, String> {
    let s = syms().ok_or_else(|| "Objective-C runtime unavailable".to_string())?;
    ensure_pool(s);
    let csel = CString::new(selector).map_err(|_| "selector contains NUL".to_string())?;
    let sel = unsafe { (s.sel_register_name)(csel.as_ptr()) };
    let mut out = SendOut::default();
    let mut excbuf = [0u8; 512];
    let rc = unsafe {
        macvm_try_msgsend(
            target,
            sel,
            gpr.as_ptr(),
            fpr.as_ptr(),
            stack.as_ptr(),
            ret as i64,
            out.gpr.as_mut_ptr(),
            out.fpr.as_mut_ptr(),
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

/// One bridged send: up to two GPR arguments, GPR result, exception-caught
/// — the C0 shape, now a thin view over [`try_send_full`].
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn try_send(
    target: *mut c_void,
    selector: &str,
    a: *mut c_void,
    b: *mut c_void,
) -> Result<*mut c_void, String> {
    let mut gpr = [0u64; SEND_GPR_SLOTS];
    gpr[0] = a as u64;
    gpr[1] = b as u64;
    let out = try_send_full(
        target,
        selector,
        &gpr,
        &[0.0; SEND_FPR_SLOTS],
        &[0u64; SEND_STACK_SLOTS],
        RetKind::Gpr,
    )?;
    Ok(out.gpr[0] as *mut c_void)
}

/// Copy an `NSString` id's UTF-8 bytes out into owned Rust memory (the
/// `UTF8String` buffer is autoreleased/interior — the copy must happen
/// before the pool drains or the string dies).
/// Bridge-produced pointers only (see `wrap`'s note).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn nsstring_utf8_bytes(ns: *mut c_void) -> Result<Vec<u8>, String> {
    let utf8 = try_send(ns, "UTF8String", std::ptr::null_mut(), std::ptr::null_mut())?;
    if utf8.is_null() {
        return Err("UTF8String answered NULL".to_string());
    }
    let cstr = unsafe { std::ffi::CStr::from_ptr(utf8 as *const c_char) };
    Ok(cstr.to_bytes().to_vec())
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
    nsstring_utf8_bytes(ns)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// CG2 — the AppKit main-thread guard's pure decision (design §8), all
    /// three inputs explicit so every outcome is deterministic off any thread.
    /// The Cocoa-mode gate is the load-bearing dimension: OFF (the WKWebView /
    /// CLI / test default) NOTHING is ever refused, which is what keeps the
    /// shipping CocoaPad resolve-then-`onMain` pattern working; ON, an AppKit UI
    /// class off-main is refused while on-main and all Foundation stay allowed.
    #[test]
    fn appkit_guard_blocks_ui_classes_off_main_only() {
        let ui_classes = [
            "NSWindow",
            "NSView",
            "NSApplication",
            "NSMenu",
            "NSButton",
            "NSTextView",
            "NSTableView",
            "NSOutlineView",
        ];
        // COCOA MODE ON: an AppKit UI class off-main → refused; on-main → OK.
        for ui in ui_classes {
            assert!(
                appkit_guard(ui, false, true).is_err(),
                "{ui} off-main in Cocoa mode must be refused"
            );
            assert!(
                appkit_guard(ui, true, true).is_ok(),
                "{ui} on-main in Cocoa mode must be allowed (the UI worker IS main)"
            );
        }
        // COCOA MODE OFF (the shipping WKWebView default): the SAME AppKit class
        // off-main is ALLOWED — the anti-regression for CocoaPad, which resolves
        // NSWindow on its worker thread before hopping via `onMain`.
        for ui in ui_classes {
            assert!(
                appkit_guard(ui, false, false).is_ok(),
                "{ui} off-main outside Cocoa mode must NOT be refused (CocoaPad path)"
            );
        }
        // Foundation is NEVER blocked, on any thread, in any mode — the headless
        // bridge gates (NSString/NSProcessInfo round-trips) run off-main and
        // depend on this.
        for f in [
            "NSString",
            "NSMutableString",
            "NSNumber",
            "NSValue",
            "NSProcessInfo",
            "NSCalendar",
            "NSThread",
            "NSDate",
        ] {
            for cocoa in [false, true] {
                assert!(
                    appkit_guard(f, false, cocoa).is_ok(),
                    "Foundation {f} must never be blocked off-main (cocoa_mode={cocoa})"
                );
                assert!(appkit_guard(f, true, cocoa).is_ok());
            }
        }
    }

    /// CG2 — the LIVE guard through `class_named` on this libtest worker thread
    /// (never main), with Cocoa mode OFF (the process default — nothing calls
    /// [`enable_cocoa_ui_mode`] in the test binary). This is the CocoaPad
    /// anti-regression: outside the native Cocoa GUI the guard is inert, so
    /// AppKit class *resolution* off-main is NOT refused (it is the legitimate
    /// first half of the C3 resolve-then-`onMain` pattern the WKWebView GUI's
    /// CocoaPad demo ships), and Foundation resolves as always.
    #[test]
    fn guard_inert_off_main_outside_cocoa_mode() {
        assert!(
            !on_main_thread(),
            "libtest #[test]s run off the process's main thread"
        );
        assert!(
            !cocoa_ui_mode(),
            "the test binary never enables Cocoa mode — the guard must be inert"
        );
        // The guard does NOT reject AppKit UI classes off-main here — the exact
        // check `class_named` performs. (Whether NSWindow ultimately *resolves*
        // depends on AppKit being loaded, which it is not headlessly; what
        // matters is the guard is not the thing standing in the way.)
        assert!(
            check_appkit_main_thread("NSWindow").is_ok(),
            "outside Cocoa mode the guard must not block AppKit resolution off-main"
        );
        assert!(check_appkit_main_thread("NSView").is_ok());
        // Foundation resolves fine off-main (the runtime is linked in the test
        // binary — the C0 gates prove it).
        assert!(
            class_named("NSString").is_some(),
            "Foundation NSString must resolve off-main"
        );
        assert!(class_named("NSProcessInfo").is_some());
    }

    /// ARC's method-family rule, pinned against its own documented edge
    /// cases — the difference between a prefix TEST and a prefix RULE is
    /// exactly `copyright` vs `copy`.
    #[test]
    fn selector_family_matches_arcs_exact_rule() {
        // In-family: the prefix followed by a non-lowercase character.
        assert_eq!(selector_family("alloc"), Family::Plus1);
        assert_eq!(selector_family("new"), Family::Plus1);
        assert_eq!(selector_family("copy"), Family::Plus1);
        assert_eq!(selector_family("copyWithZone:"), Family::Plus1);
        assert_eq!(selector_family("mutableCopy"), Family::Plus1);
        assert_eq!(selector_family("mutableCopyWithZone:"), Family::Plus1);
        assert_eq!(selector_family("newTaskWithURL:"), Family::Plus1);
        assert_eq!(selector_family("init"), Family::Init);
        assert_eq!(selector_family("initWithString:"), Family::Init);
        // Leading underscores are ignored, per the rule.
        assert_eq!(selector_family("_init"), Family::Init);
        assert_eq!(selector_family("__copy"), Family::Plus1);
        // NOT in a family: the prefix runs into a lowercase letter.
        assert_eq!(selector_family("copyright"), Family::Plus0);
        assert_eq!(selector_family("initialize"), Family::Plus0);
        // `newButtonTitle` IS in the new family — "new" followed by a
        // non-lowercase 'B'. That's ARC's real (and famously surprising)
        // behavior, the reason `objc_method_family(none)` exists; matching
        // clang exactly is the point, and the odd corner's escape hatch is
        // an explicit override on the wrapper (design §3.2), not a
        // different rule.
        assert_eq!(selector_family("newButtonTitle"), Family::Plus1);
        assert_eq!(selector_family("allocate"), Family::Plus0);
        assert_eq!(selector_family("newton"), Family::Plus0);
        // `mutableCopy` is its own family, not a lowercase-collision of
        // `copy` — and an ordinary selector is Plus0.
        assert_eq!(selector_family("length"), Family::Plus0);
        assert_eq!(selector_family("description"), Family::Plus0);
    }

    /// The `@encode` parser, pinned against real signatures the macOS
    /// runtime hands back (offsets vary by arch/method — the parser must
    /// skip any digit run).
    #[test]
    fn type_encoding_parser_handles_real_signatures() {
        // -[NSString length]: NSUInteger, no args.
        let s = parse_type_encoding("Q16@0:8").unwrap();
        assert_eq!(
            s.ret,
            ObjcRet::Int {
                signed: false,
                bits: 64
            }
        );
        assert!(s.args.is_empty());
        // -[NSMutableString appendString:]: void, one id.
        let s = parse_type_encoding("v24@0:8@16").unwrap();
        assert_eq!(s.ret, ObjcRet::Void);
        assert_eq!(s.args, vec![ObjcArg::Id]);
        // -[NSString rangeOfString:]: NSRange return.
        let s = parse_type_encoding("{_NSRange=QQ}32@0:8@16").unwrap();
        assert_eq!(s.ret, ObjcRet::Range);
        assert_eq!(s.args, vec![ObjcArg::Id]);
        // +[NSValue valueWithRect:]: id ret, one CGRect (nested struct).
        let s = parse_type_encoding("@48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16").unwrap();
        assert_eq!(s.ret, ObjcRet::Id);
        assert_eq!(s.args, vec![ObjcArg::Rect]);
        // -[NSValue pointValue]: CGPoint return.
        let s = parse_type_encoding("{CGPoint=dd}16@0:8").unwrap();
        assert_eq!(s.ret, ObjcRet::Point);
        // -[NSNumber intValue]: int (32-bit signed), BOOL, char, long, float.
        assert_eq!(
            parse_type_encoding("i16@0:8").unwrap().ret,
            ObjcRet::Int {
                signed: true,
                bits: 32
            }
        );
        assert_eq!(parse_type_encoding("B16@0:8").unwrap().ret, ObjcRet::Bool);
        // `c` is a GENUINE signed char on arm64 (BOOL is `B`) — a char
        // return answers a SmallInteger, never a Boolean (C2 review:
        // charValue 65 must be 65, not true).
        assert_eq!(
            parse_type_encoding("c16@0:8").unwrap().ret,
            ObjcRet::Int {
                signed: true,
                bits: 8
            }
        );
        // `l` is 32-bit BY DEFINITION in @encode, even on LP64.
        assert_eq!(
            parse_type_encoding("l16@0:8").unwrap().ret,
            ObjcRet::Int {
                signed: true,
                bits: 32
            }
        );
        assert_eq!(parse_type_encoding("f16@0:8").unwrap().ret, ObjcRet::F32);
        // -[NSString compare:options:]: NSInteger ret, id + NSUInteger.
        let s = parse_type_encoding("q32@0:8@16Q24").unwrap();
        assert_eq!(
            s.ret,
            ObjcRet::Int {
                signed: true,
                bits: 64
            }
        );
        assert_eq!(
            s.args,
            vec![
                ObjcArg::Id,
                ObjcArg::Int {
                    signed: false,
                    bits: 64
                }
            ]
        );
        // Const-qualified char* return (-[NSString UTF8String]: r*).
        let s = parse_type_encoding("r*16@0:8").unwrap();
        assert_eq!(s.ret, ObjcRet::CharStar);
        // SEL argument (-[NSObject respondsToSelector:]: B ret, : arg).
        let s = parse_type_encoding("B24@0:8:16").unwrap();
        assert_eq!(s.ret, ObjcRet::Bool);
        assert_eq!(s.args, vec![ObjcArg::Sel]);
        // Unmarshalable shapes answer None, never panic: a raw pointer
        // arg, an unknown struct, a union, a variadic-ish garbage tail.
        assert!(parse_type_encoding("v24@0:8^v16").is_none());
        assert!(parse_type_encoding("{NSDecimal=...}16@0:8").is_none());
        assert!(parse_type_encoding("(aUnion=id)16@0:8").is_none());
        assert!(parse_type_encoding("").is_none());
        assert!(parse_type_encoding("{unbalanced").is_none());
        // Double arg coercion class + f32 arg class parse correctly
        // (+[NSNumber numberWithDouble:] / numberWithFloat:).
        assert_eq!(
            parse_type_encoding("@24@0:8d16").unwrap().args,
            vec![ObjcArg::F64]
        );
        assert_eq!(
            parse_type_encoding("@24@0:8f16").unwrap().args,
            vec![ObjcArg::F32]
        );
    }
}

// ── C4: callbacks — Cocoa calls Smalltalk (design §6) ───────────────────────
//
// The reverse direction never touches an oop: ONE runtime-registered ObjC
// trampoline class, `MacvmAction`, whose fire IMP looks up a Rust registry
// entry and posts a PRE-PICKLED `{#cocoaEvent. ticket}` envelope through
// the primary's `InboxSender` — the worker transport, delivery, and
// coalesced wake, unmodified. The envelope bytes are pickled at action-
// CREATION time (on the VM thread, where a VmState exists), so the IMP —
// which AppKit invokes on the MAIN thread — is completely VM-free:
// registry lookup, channel send, return. Dispatch happens between doits
// on the VM thread (`Worker dispatchInbox` → the world's Actions
// registry), preserving the strictly-serial invariant.

/// Envelope `from` for Cocoa-originated events — never a real worker id
/// (worker ids are small and monotonic from 1).
pub const COCOA_PEER_ID: u32 = u32::MAX;

struct ActionEntry {
    sender: crate::runtime::workers::InboxSender,
    bytes: Vec<u8>,
}

static ACTIONS: OnceLock<std::sync::RwLock<HashMap<usize, ActionEntry>>> = OnceLock::new();
static MACVM_ACTION_CLASS: OnceLock<Option<usize>> = OnceLock::new();

/// The fire IMP — invoked BY Cocoa (target/action) on whatever thread the
/// control lives on (main, for AppKit). No oop, no VmState, no lock held
/// across anything blocking: registry read, envelope send (coalesced wake
/// inside), done. An unknown instance (never registered — impossible
/// through the bridge) or a closed inbox (the primary is exiting) drops
/// the event silently: a late click during teardown is not an error.
extern "C" fn macvm_action_fire(this: *mut c_void, _cmd: *mut c_void, _sender: *mut c_void) {
    if let Some(reg) = ACTIONS.get() {
        if let Ok(map) = reg.read() {
            if let Some(e) = map.get(&(this as usize)) {
                let _ = e.sender.send(crate::runtime::workers::Envelope {
                    from: COCOA_PEER_ID,
                    corr: 0,
                    bytes: e.bytes.clone(),
                });
            }
        }
    }
}

/// Register the `MacvmAction` class with the ObjC runtime, once.
fn macvm_action_class() -> Option<*mut c_void> {
    MACVM_ACTION_CLASS
        .get_or_init(|| {
            let alloc_pair = resolve("objc_allocateClassPair")?;
            let register_pair = resolve("objc_registerClassPair")?;
            let add_method = resolve("class_addMethod")?;
            let s = syms()?;
            let superclass = {
                let c = CString::new("NSObject").ok()?;
                let sc = unsafe { (s.objc_get_class)(c.as_ptr()) };
                if sc.is_null() {
                    return None;
                }
                sc
            };
            type AllocPair = unsafe extern "C" fn(*mut c_void, *const c_char, usize) -> *mut c_void;
            type RegisterPair = unsafe extern "C" fn(*mut c_void);
            type AddMethod = unsafe extern "C" fn(
                *mut c_void,
                *mut c_void,
                extern "C" fn(*mut c_void, *mut c_void, *mut c_void),
                *const c_char,
            ) -> u8;
            let name = CString::new("MacvmAction").ok()?;
            let cls = unsafe {
                std::mem::transmute::<u64, AllocPair>(alloc_pair)(superclass, name.as_ptr(), 0)
            };
            if cls.is_null() {
                return None; // name collision — a previous registration owns it
            }
            let sel = register_selector("macvmFire:")?;
            let types = CString::new("v@:@").ok()?;
            unsafe {
                std::mem::transmute::<u64, AddMethod>(add_method)(
                    cls,
                    sel,
                    macvm_action_fire,
                    types.as_ptr(),
                );
                std::mem::transmute::<u64, RegisterPair>(register_pair)(cls);
            }
            Some(cls as usize)
        })
        .map(|p| p as *mut c_void)
}

/// Mint one `MacvmAction` instance bound to a pre-pickled fire payload.
/// Answers a +1 id (alloc/init — the caller wraps with [`wrap_owned`]).
/// Registry entries live for the process (design §6: tickets are never
/// reused; a dead ticket's late fire is dropped WORLD-side, and the few
/// bytes here are not worth a reclamation protocol before finalization).
pub fn new_action(
    sender: crate::runtime::workers::InboxSender,
    bytes: Vec<u8>,
) -> Result<*mut c_void, String> {
    let cls =
        macvm_action_class().ok_or_else(|| "MacvmAction class registration failed".to_string())?;
    let inst = try_send(cls, "alloc", std::ptr::null_mut(), std::ptr::null_mut())?;
    let inst = try_send(inst, "init", std::ptr::null_mut(), std::ptr::null_mut())?;
    if inst.is_null() {
        return Err("MacvmAction alloc/init answered nil".to_string());
    }
    ACTIONS
        .get_or_init(|| std::sync::RwLock::new(HashMap::new()))
        .write()
        .map_err(|_| "action registry poisoned".to_string())?
        .insert(inst as usize, ActionEntry { sender, bytes });
    Ok(inst)
}

/// C4 `poolDo:` mint-note: append a freshly minted wrapper to the OPEN
/// mint-list (top of `vm.cocoa_mint_stack`), growing the in-heap Array
/// when full. The list layout is `[count(smi), w1, w2, …]`. Everything
/// here is barrier-correct (`store_tail_oop`) and handle-rooted across
/// the one allocation (growth) — the design's point: the list the GC must
/// know about IS a heap object, so the GC already knows about it.
fn mint_note(vm: &mut VmState, wrapper: Oop) {
    use crate::oops::smi::SmallInt;
    use crate::oops::wrappers::ArrayOop;
    let Some(&top) = vm.cocoa_mint_stack.last() else {
        return; // no open pool scope — the common case, zero cost
    };
    let Some(arr) = ArrayOop::try_from(top) else {
        return;
    };
    let count = SmallInt::try_from(arr.at(0))
        .map(|s| s.value())
        .unwrap_or(0)
        .max(0) as usize;
    let cap = arr.len() - 1;
    if count < cap {
        // No allocation on this path — `top`/`wrapper` stay valid raw.
        let m = MemOop::try_from(top).expect("mint list is a mem oop");
        crate::memory::store::store_tail_oop(vm, m, 1 + count, wrapper);
        arr.at_put(0, SmallInt::new((count + 1) as i64).oop()); // smi: barrier-exempt
    } else {
        // Grow (doubling): the ONE allocating path — wrapper + old list
        // ride handles across it.
        let scope = crate::memory::handles::HandleScope::enter(vm);
        let w_h = scope.handle(vm, wrapper);
        let old_h = scope.handle(vm, top);
        let bigger = alloc::alloc_indexable_oops(vm, vm.universe.array_klass, 2 + cap * 2);
        let old = ArrayOop::try_from(old_h.get(vm)).expect("handle-rooted list survived");
        let big_m = MemOop::try_from(bigger.oop()).expect("fresh array is a mem oop");
        for i in 1..=count {
            crate::memory::store::store_tail_oop(vm, big_m, i, old.at(i));
        }
        crate::memory::store::store_tail_oop(vm, big_m, 1 + count, w_h.get(vm));
        bigger.at_put(0, SmallInt::new((count + 1) as i64).oop());
        *vm.cocoa_mint_stack.last_mut().expect("stack non-empty") = bigger.oop();
    }
}
