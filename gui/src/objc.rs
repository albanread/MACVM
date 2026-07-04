//! Minimal Cocoa/AppKit/WebKit bridge for the MACVM GUI shell.
//!
//! Deliberately dependency-free: `dlopen`/`dlsym` resolve the Objective-C
//! runtime and framework entry points at process startup, and every message
//! send is a raw `objc_msgSend` call transmuted to the ABI shape that
//! particular call site needs (self/`_cmd` plus whatever arguments, per
//! AAPCS64 — integer/pointer args in `x0..`, floating-point args in `d0..`,
//! counted independently, which is what makes e.g. passing `NSRect`'s four
//! `f64` fields as four separate arguments work).
//!
//! This is the same approach already proven in the sibling MacModula2
//! project's `src/newm2-runtime/src/objc.rs` (shipping in `MacM2 IDE.app`) —
//! adopted here instead of the `objc2`/`objc2-app-kit`/`objc2-web-kit` crate
//! ecosystem `../PLAN.md`'s original D-G2 considered, for the same "Rust
//! owns the shell, no Swift/Xcode target" outcome with zero external crate
//! dependencies and no risk from an evolving crate's macro API. See that
//! file for the pattern this one mirrors; this one is scoped to exactly what
//! the GUI shell needs (`NSApplication`/`NSWindow`/`NSMenu` + `WKWebView` +
//! a `WKScriptMessageHandler`), not MacModula2's broader IDE surface.

#![allow(dead_code)]

use std::ffi::{c_void, CStr, CString};
use std::sync::OnceLock;

pub type Id = *mut c_void;
pub type Sel = *mut c_void;
pub type Class = *mut c_void;

const RTLD_DEFAULT: *mut c_void = (-2isize) as *mut c_void;
const RTLD_NOW: i32 = 0x2;

unsafe extern "C" {
    fn dlopen(path: *const i8, mode: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const i8) -> *mut c_void;
}

/// Map the frameworks the shell needs into the process so their classes and
/// C entry points resolve via `dlsym(RTLD_DEFAULT, …)`. Idempotent.
pub fn bootstrap() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        for path in [
            "/System/Library/Frameworks/Cocoa.framework/Cocoa",
            "/System/Library/Frameworks/AppKit.framework/AppKit",
            "/System/Library/Frameworks/Foundation.framework/Foundation",
            "/System/Library/Frameworks/WebKit.framework/WebKit",
            "/usr/lib/libobjc.A.dylib",
        ] {
            if let Ok(c) = CString::new(path) {
                unsafe { dlopen(c.as_ptr(), RTLD_NOW) };
            }
        }
    });
}

fn sym(name: &str) -> *mut c_void {
    bootstrap();
    let c = CString::new(name).expect("symbol name has no interior NUL");
    let p = unsafe { dlsym(RTLD_DEFAULT, c.as_ptr()) };
    if p.is_null() {
        panic!("objc bridge: symbol \"{name}\" not found — is the framework list in bootstrap() complete?");
    }
    p
}

fn msg_send_ptr() -> *mut c_void {
    static PTR: OnceLock<usize> = OnceLock::new();
    *PTR.get_or_init(|| sym("objc_msgSend") as usize) as *mut c_void
}

/// `objc_getClass(name)`.
pub fn get_class(name: &str) -> Class {
    let c = CString::new(name).unwrap();
    let f: extern "C" fn(*const i8) -> Class = unsafe { std::mem::transmute(sym("objc_getClass")) };
    f(c.as_ptr())
}

/// `sel_registerName(name)`.
pub fn sel(name: &str) -> Sel {
    let c = CString::new(name).unwrap();
    let f: extern "C" fn(*const i8) -> Sel = unsafe { std::mem::transmute(sym("sel_registerName")) };
    f(c.as_ptr())
}

/// `+[NSString stringWithUTF8String:]` — an autoreleased `NSString*` from a
/// Rust `&str`.
pub fn nsstring(s: &str) -> Id {
    let c = CString::new(s).unwrap_or_default();
    let cls = get_class("NSString");
    let f: extern "C" fn(Id, Sel, *const i8) -> Id = unsafe { std::mem::transmute(msg_send_ptr()) };
    f(cls, sel("stringWithUTF8String:"), c.as_ptr())
}

/// `[nsstr UTF8String]` read back into an owned `String`. Empty string for
/// a nil/non-UTF8 input rather than panicking — this is used on data that
/// crossed the JS bridge and shouldn't be able to bring the shell down.
pub fn string_from_nsstring(nsstr: Id) -> String {
    if nsstr.is_null() {
        return String::new();
    }
    let f: extern "C" fn(Id, Sel) -> *const i8 = unsafe { std::mem::transmute(msg_send_ptr()) };
    let ptr = f(nsstr, sel("UTF8String"));
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
}

// ── Typed message sends, one per ABI shape actually used by main.rs ────────
// Named by (return, args) shape rather than by any particular selector —
// each is reused across many call sites, exactly like MacModula2's
// Send0/SendI/SendP/... but declared inline (Rust doesn't need M2's
// CAST-from-one-raw-pointer indirection: each function below resolves and
// transmutes objc_msgSend for its own specific signature).

pub fn send0(recv: Id, s: Sel) -> Id {
    let f: extern "C" fn(Id, Sel) -> Id = unsafe { std::mem::transmute(msg_send_ptr()) };
    f(recv, s)
}

pub fn send0_bool(recv: Id, s: Sel) -> bool {
    let f: extern "C" fn(Id, Sel) -> bool = unsafe { std::mem::transmute(msg_send_ptr()) };
    f(recv, s)
}

pub fn send0_i64(recv: Id, s: Sel) -> i64 {
    let f: extern "C" fn(Id, Sel) -> i64 = unsafe { std::mem::transmute(msg_send_ptr()) };
    f(recv, s)
}

pub fn send1_id(recv: Id, s: Sel, a: Id) -> Id {
    let f: extern "C" fn(Id, Sel, Id) -> Id = unsafe { std::mem::transmute(msg_send_ptr()) };
    f(recv, s, a)
}

pub fn send1_i64(recv: Id, s: Sel, a: i64) -> Id {
    let f: extern "C" fn(Id, Sel, i64) -> Id = unsafe { std::mem::transmute(msg_send_ptr()) };
    f(recv, s, a)
}

pub fn send1_bool(recv: Id, s: Sel, a: bool) -> Id {
    let f: extern "C" fn(Id, Sel, bool) -> Id = unsafe { std::mem::transmute(msg_send_ptr()) };
    f(recv, s, a)
}

pub fn send1_f64(recv: Id, s: Sel, a: f64) -> Id {
    let f: extern "C" fn(Id, Sel, f64) -> Id = unsafe { std::mem::transmute(msg_send_ptr()) };
    f(recv, s, a)
}

pub fn send2_id(recv: Id, s: Sel, a: Id, b: Id) -> Id {
    let f: extern "C" fn(Id, Sel, Id, Id) -> Id = unsafe { std::mem::transmute(msg_send_ptr()) };
    f(recv, s, a, b)
}

pub fn send3_id(recv: Id, s: Sel, a: Id, b: Id, c: Id) -> Id {
    let f: extern "C" fn(Id, Sel, Id, Id, Id) -> Id = unsafe { std::mem::transmute(msg_send_ptr()) };
    f(recv, s, a, b, c)
}

/// `-[NSObject performSelectorOnMainThread:withObject:waitUntilDone:]` — the
/// mechanism a background thread uses to safely run a method on the main
/// thread's run loop. Deliberately not the same thing as calling
/// `objc_msgSend` directly from a background thread: AppKit/WebKit objects
/// (windows, web views) are only safe to message from the main thread, but
/// `performSelectorOnMainThread:...` is itself one of the few NSObject
/// entry points documented as safe to call from *any* thread — it just
/// enqueues the real call onto the main thread's run loop rather than
/// executing inline. This is how a VM worker thread (`vm_host.rs`) wakes
/// the main thread to drain a response queue, without this bridge needing
/// to implement the GCD/block-literal ABI (see this module's doc comment:
/// that's deliberately out of scope).
///
/// Getting the selector name wrong here is not a quiet failure: sending a
/// selector NSObject doesn't implement triggers `-doesNotRecognizeSelector:`,
/// which raises an Objective-C exception — and since that happens on a bare
/// `std::thread`, not inside any Objective-C exception handler Rust's
/// unwinder understands, the whole process aborts
/// ("Rust cannot catch foreign exceptions"). Confirmed the hard way: an
/// earlier version of this function sent the nonexistent selector
/// `performSelector:withObject:waitUntilDone:` (missing `OnMainThread`) and
/// crashed the app on the very first VM response.
pub fn perform_selector_on_main_thread(recv: Id, selector: Sel, arg: Id, wait_until_done: bool) {
    let f: extern "C" fn(Id, Sel, Sel, Id, bool) =
        unsafe { std::mem::transmute(msg_send_ptr()) };
    f(recv, sel("performSelectorOnMainThread:withObject:waitUntilDone:"), selector, arg, wait_until_done)
}

/// `-initWithFrame:` — `CGRect`'s four `f64` fields passed as four separate
/// arguments (see this module's doc comment for why that's ABI-correct on
/// arm64).
pub fn send_frame_init(recv: Id, s: Sel, x: f64, y: f64, w: f64, h: f64) -> Id {
    let f: extern "C" fn(Id, Sel, f64, f64, f64, f64) -> Id =
        unsafe { std::mem::transmute(msg_send_ptr()) };
    f(recv, s, x, y, w, h)
}

/// `-initWithContentRect:styleMask:backing:defer:` on `NSWindow`.
pub fn send_window_init(
    recv: Id,
    s: Sel,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    style_mask: u64,
    backing: u64,
    defer: bool,
) -> Id {
    let f: extern "C" fn(Id, Sel, f64, f64, f64, f64, u64, u64, bool) -> Id =
        unsafe { std::mem::transmute(msg_send_ptr()) };
    f(recv, s, x, y, w, h, style_mask, backing, defer)
}

/// `-initWithFrame:configuration:` on `WKWebView`.
pub fn send_frame_config_init(recv: Id, s: Sel, x: f64, y: f64, w: f64, h: f64, config: Id) -> Id {
    let f: extern "C" fn(Id, Sel, f64, f64, f64, f64, Id) -> Id =
        unsafe { std::mem::transmute(msg_send_ptr()) };
    f(recv, s, x, y, w, h, config)
}

// ── Runtime class registration (for delegate / message-handler objects) ───

/// `objc_allocateClassPair(superclass, name, extraBytes:0)` — begin defining
/// a new Objective-C class. Add methods, then [`register_class`].
pub fn allocate_class(superclass: Class, name: &str) -> Class {
    let c = CString::new(name).unwrap();
    let f: extern "C" fn(Class, *const i8, usize) -> Class =
        unsafe { std::mem::transmute(sym("objc_allocateClassPair")) };
    f(superclass, c.as_ptr(), 0)
}

/// `class_addMethod(cls, sel, imp, types)`. `types` is the Objective-C type
/// encoding string, e.g. `"v@:@@"` for `-(void)foo:(id)a bar:(id)b`, `"B@:@"`
/// for a `BOOL`-returning one-object-arg method.
pub fn add_method(cls: Class, s: Sel, imp: *const c_void, types: &str) -> bool {
    let t = CString::new(types).unwrap();
    let f: extern "C" fn(Class, Sel, *const c_void, *const i8) -> bool =
        unsafe { std::mem::transmute(sym("class_addMethod")) };
    f(cls, s, imp, t.as_ptr())
}

/// `objc_registerClassPair(cls)` — finalize a class begun with
/// [`allocate_class`].
pub fn register_class(cls: Class) {
    let f: extern "C" fn(Class) = unsafe { std::mem::transmute(sym("objc_registerClassPair")) };
    f(cls)
}

/// `[[cls alloc] init]`.
pub fn alloc_init(class_name: &str) -> Id {
    let cls = get_class(class_name);
    send0(send0(cls, sel("alloc")), sel("init"))
}

pub const NIL: Id = std::ptr::null_mut();
