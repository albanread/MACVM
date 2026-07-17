//! Minimal raw-AppKit bridge for the `macvm-cocoa` boot glue (CG2).
//!
//! **Structural decision (own vs. factor — see `Cargo.toml`).** This is a
//! deliberately small, self-contained copy of the `gui/src/objc.rs` pattern,
//! scoped to *exactly* what the Cocoa GUI boot needs: bring NSApplication up,
//! set the activation policy, install a Quit menu (⌘Q → `terminate:`), enter
//! `[NSApp run]`, and poke the main run loop from a background thread. It does
//! **not** reach into the core `src/runtime/objc_bridge.rs` (that bridge is
//! Smalltalk-oriented — `ObjcRef`/marshalling — not raw-Rust AppKit), and it
//! does **not** factor the `gui` crate's helper (which the shipping WKWebView
//! shell depends on) — a fresh minimal module is the lowest-risk route (CG2
//! task guidance). The window itself is built from *Smalltalk* through the
//! Cocoa bridge (`world/64_cocoaui.mst`), not here.
//!
//! Dependency-free: `dlopen`/`dlsym` resolve the Objective-C runtime and the
//! frameworks at startup; every send is `objc_msgSend` transmuted to the ABI
//! shape the call site needs. This runs only on the main thread (except the
//! run-loop wake), so no main-thread-only-API hazard applies.

#![allow(dead_code)]

use std::ffi::{c_void, CString};
use std::sync::OnceLock;

pub type Id = *mut c_void;
pub type Sel = *mut c_void;
pub type Class = *mut c_void;

pub const NIL: Id = std::ptr::null_mut();

/// `NSApplicationActivationPolicyRegular` — a normal foreground app (Dock icon,
/// menu bar, key-window activation). Without it a bare executable (no `.app`
/// bundle) behaves like a background agent and no window ever becomes visible
/// (the exact bug `gui/src/main.rs` documents).
const NS_APPLICATION_ACTIVATION_POLICY_REGULAR: i64 = 0;

const RTLD_DEFAULT: *mut c_void = (-2isize) as *mut c_void;
const RTLD_NOW: i32 = 0x2;

unsafe extern "C" {
    fn dlopen(path: *const i8, mode: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const i8) -> *mut c_void;
}

// CoreFoundation, linked (not dlsym'd): the background-thread run-loop wake.
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRunLoopGetMain() -> *mut c_void;
    fn CFRunLoopWakeUp(rl: *mut c_void);
}

/// Map the frameworks the glue needs into the process so classes and C entry
/// points resolve via `dlsym(RTLD_DEFAULT, …)`. Idempotent. MUST run on main
/// before any AppKit use (`cocoa_gui_design.md` §3 step 1).
pub fn bootstrap() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        for path in [
            "/System/Library/Frameworks/Cocoa.framework/Cocoa",
            "/System/Library/Frameworks/AppKit.framework/AppKit",
            "/System/Library/Frameworks/Foundation.framework/Foundation",
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
        panic!("cocoa_gui objc: symbol \"{name}\" not found — is bootstrap()'s framework list complete?");
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
    let f: extern "C" fn(*const i8) -> Sel =
        unsafe { std::mem::transmute(sym("sel_registerName")) };
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

pub fn send0(recv: Id, s: Sel) -> Id {
    let f: extern "C" fn(Id, Sel) -> Id = unsafe { std::mem::transmute(msg_send_ptr()) };
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

pub fn send3_id(recv: Id, s: Sel, a: Id, b: Id, c: Id) -> Id {
    let f: extern "C" fn(Id, Sel, Id, Id, Id) -> Id =
        unsafe { std::mem::transmute(msg_send_ptr()) };
    f(recv, s, a, b, c)
}

/// `[[cls alloc] init]`.
pub fn alloc_init(class_name: &str) -> Id {
    let cls = get_class(class_name);
    send0(send0(cls, sel("alloc")), sel("init"))
}

// ── App lifecycle ─────────────────────────────────────────────────────────────

/// `[NSApplication sharedApplication]` — the process-singleton app object.
pub fn app_shared() -> Id {
    send0(get_class("NSApplication"), sel("sharedApplication"))
}

/// Make this a normal foreground app (see the policy constant's note).
pub fn set_activation_policy_regular(app: Id) {
    send1_i64(
        app,
        sel("setActivationPolicy:"),
        NS_APPLICATION_ACTIVATION_POLICY_REGULAR,
    );
}

/// A minimal main menu with an application submenu carrying one Quit item
/// (⌘Q → `terminate:`, target `nil` so it routes to `NSApp`), so the app quits
/// cleanly (CG2). The full Smalltalk-built menu bar arrives in CG4 (design
/// §7/R5); this is the lifecycle minimum.
pub fn install_quit_menu(app: Id) {
    let quit_item = send0(get_class("NSMenuItem"), sel("alloc"));
    let quit_item = send3_id(
        quit_item,
        sel("initWithTitle:action:keyEquivalent:"),
        nsstring("Quit MACVM"),
        sel("terminate:"),
        nsstring("q"),
    );
    let app_menu = alloc_init("NSMenu");
    send1_id(app_menu, sel("addItem:"), quit_item);

    // The app-menu goes in a titleless container item on the main menu.
    let app_menu_item = send0(get_class("NSMenuItem"), sel("alloc"));
    let app_menu_item = send3_id(
        app_menu_item,
        sel("initWithTitle:action:keyEquivalent:"),
        nsstring(""),
        NIL,
        nsstring(""),
    );
    send1_id(app_menu_item, sel("setSubmenu:"), app_menu);

    let main_menu = alloc_init("NSMenu");
    send1_id(main_menu, sel("addItem:"), app_menu_item);
    send1_id(app, sel("setMainMenu:"), main_menu);
}

/// `[NSApp activateIgnoringOtherApps: YES]`.
pub fn activate(app: Id) {
    send1_bool(app, sel("activateIgnoringOtherApps:"), true);
}

/// `[NSApp run]` — enters the AppKit run loop and blocks until termination
/// (⌘Q → `terminate:` → process exit). The VM is at rest when this is called
/// (`cocoa_gui_design.md` §3 step 5): every future callback is a top-level VM
/// entry (CG3).
pub fn run(app: Id) {
    send0(app, sel("run"));
}

/// Wake the process's main run loop from a background thread — the run-loop
/// poke the externally-hosted UI worker's inbox wake fires when the primary
/// `send`s it (CG1's `InboxWakeFn`, promoted to the AppKit poke here, design
/// §8). `CFRunLoopWakeUp` is documented thread-safe. In CG2 there is nothing to
/// drain yet, so waking is a benign nudge; CG4 adds the default-mode
/// `CFRunLoopSource` drain this feeds.
pub fn wake_main_runloop() {
    unsafe {
        let rl = CFRunLoopGetMain();
        if !rl.is_null() {
            CFRunLoopWakeUp(rl);
        }
    }
}
