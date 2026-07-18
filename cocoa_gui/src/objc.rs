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
use std::sync::atomic::{AtomicPtr, Ordering};
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

// CoreFoundation, linked (not dlsym'd): the background-thread run-loop wake +
// the default-mode drain source (CG4 §8).
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRunLoopGetMain() -> *mut c_void;
    fn CFRunLoopWakeUp(rl: *mut c_void);
    fn CFRunLoopAddSource(rl: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopSourceCreate(
        allocator: *const c_void,
        order: isize,
        context: *mut CFRunLoopSourceContext,
    ) -> *mut c_void;
    fn CFRunLoopSourceSignal(source: *mut c_void);
    static kCFRunLoopDefaultMode: *const c_void;
}

/// A CoreFoundation version-0 run-loop source context (the only fields we set:
/// `info` + `perform`; the rest are null callbacks). `perform` runs on the run
/// loop's thread (main) when the source is signalled and the loop is in a mode
/// the source belongs to.
#[repr(C)]
struct CFRunLoopSourceContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
    hash: *const c_void,
    schedule: *const c_void,
    cancel: *const c_void,
    perform: Option<extern "C" fn(*mut c_void)>,
}

/// The default-mode drain source, once created — read by [`wake_main_runloop`]
/// (fired from background/primary threads) to signal a drain. `AtomicPtr` so the
/// cross-thread read of a pointer main published is well-defined.
static DRAIN_SOURCE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

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

/// A resolved runtime symbol address for sibling modules registering their own
/// ObjC classes (`host_service`) — the same panic-on-missing contract as `sym`.
pub fn sym_addr(name: &str) -> *mut c_void {
    sym(name)
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

/// `NSUInteger`-returning unary send (`length`).
pub fn send0_int(recv: Id, s: Sel) -> i64 {
    let f: extern "C" fn(Id, Sel) -> i64 = unsafe { std::mem::transmute(msg_send_ptr()) };
    f(recv, s)
}

/// One `CGFloat` argument (`userFixedPitchFontOfSize:`).
pub fn send1_f64(recv: Id, s: Sel, a: f64) -> Id {
    let f: extern "C" fn(Id, Sel, f64) -> Id = unsafe { std::mem::transmute(msg_send_ptr()) };
    f(recv, s, a)
}

/// Two `CGFloat` arguments (`monospacedSystemFontOfSize:weight:`).
pub fn send2_f64(recv: Id, s: Sel, a: f64, b: f64) -> Id {
    let f: extern "C" fn(Id, Sel, f64, f64) -> Id = unsafe { std::mem::transmute(msg_send_ptr()) };
    f(recv, s, a, b)
}

/// `addAttribute:value:range:` — the `NSRange` passes by value as two GPR
/// words on arm64 (location, length), after the two id arguments.
pub fn send_attr(storage: Id, name: Id, value: Id, loc: u64, len: u64) {
    let sel = self::sel("addAttribute:value:range:");
    let f: extern "C" fn(Id, Sel, Id, Id, u64, u64) =
        unsafe { std::mem::transmute(msg_send_ptr()) };
    f(storage, sel, name, value, loc, len)
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

/// Register an `NSObject` subclass `name` carrying `methods` (each `(selector,
/// IMP, @encode types)`), once. Idempotent: a name collision leaves the prior
/// registration in place. The generalized form of `install_app_delegate`'s
/// inline class-pair dance — used by the CG10 game-timer target.
pub fn register_class(name: &str, methods: &[(&str, *const c_void, &str)]) {
    let superclass = get_class("NSObject");
    let cname = match CString::new(name) {
        Ok(c) => c,
        Err(_) => return,
    };
    let alloc_pair: extern "C" fn(Class, *const i8, usize) -> Class =
        unsafe { std::mem::transmute(sym("objc_allocateClassPair")) };
    let cls = alloc_pair(superclass, cname.as_ptr(), 0);
    if cls.is_null() {
        return; // already registered
    }
    let add_method: extern "C" fn(Class, Sel, *const c_void, *const i8) -> u8 =
        unsafe { std::mem::transmute(sym("class_addMethod")) };
    for (selector, imp, types) in methods {
        if let Ok(ctypes) = CString::new(*types) {
            add_method(cls, sel(selector), *imp, ctypes.as_ptr());
        }
    }
    let register_pair: extern "C" fn(Class) =
        unsafe { std::mem::transmute(sym("objc_registerClassPair")) };
    register_pair(cls);
}

/// `+[NSTimer scheduledTimerWithTimeInterval:target:selector:userInfo:repeats:]`
/// — added to the current run loop in `NSDefaultRunLoopMode` (so it does NOT
/// fire inside AppKit's nested tracking/modal loops — the same tracking-safety
/// the drain source has, §8). Returns the timer; send `invalidate` to stop it.
pub fn scheduled_timer(interval: f64, target: Id, selector: Sel, repeats: bool) -> Id {
    let f: extern "C" fn(Class, Sel, f64, Id, Sel, Id, bool) -> Id =
        unsafe { std::mem::transmute(msg_send_ptr()) };
    f(
        get_class("NSTimer"),
        sel("scheduledTimerWithTimeInterval:target:selector:userInfo:repeats:"),
        interval,
        target,
        selector,
        NIL,
        repeats,
    )
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

/// Register a minimal `NSApplication` delegate whose
/// `applicationShouldTerminateAfterLastWindowClosed:` returns YES, and set it,
/// so the red close button quitting the *window* also quits the *app* — without
/// it, closing hides the window and leaves a headless process running (the
/// on-screen "close closed the window but left the app running" report). The
/// delegate is a process-lifetime singleton (deliberately leaked).
pub fn install_app_delegate(app: Id) {
    // BOOL-returning IMP: `-(BOOL)applicationShouldTerminateAfterLastWindowClosed:`
    // → always YES (1). ObjC BOOL on arm64 is a 1-byte value; `u8` matches.
    extern "C" fn should_terminate(_self: Id, _cmd: Sel, _app: Id) -> u8 {
        1
    }
    let superclass = get_class("NSObject");
    let cname = CString::new("MacvmCocoaAppDelegate").unwrap();
    let alloc_pair: extern "C" fn(Class, *const i8, usize) -> Class =
        unsafe { std::mem::transmute(sym("objc_allocateClassPair")) };
    let cls = alloc_pair(superclass, cname.as_ptr(), 0);
    if !cls.is_null() {
        let add_method: extern "C" fn(Class, Sel, *const c_void, *const i8) -> u8 =
            unsafe { std::mem::transmute(sym("class_addMethod")) };
        let types = CString::new("B@:@").unwrap();
        add_method(
            cls,
            sel("applicationShouldTerminateAfterLastWindowClosed:"),
            should_terminate as extern "C" fn(Id, Sel, Id) -> u8 as *const c_void,
            types.as_ptr(),
        );
        let register_pair: extern "C" fn(Class) =
            unsafe { std::mem::transmute(sym("objc_registerClassPair")) };
        register_pair(cls);
    }
    let del = alloc_init("MacvmCocoaAppDelegate");
    send1_id(app, sel("setDelegate:"), del);
}

/// `objc_autoreleasePoolPush()` — push an autorelease pool, returning its token
/// for [`autorelease_pool_pop`]. The Cocoa GUI wraps its **pre-`[NSApp run]`**
/// startup (the window/menu build) in one explicit pool: before the run loop is
/// live there is no CF/AppKit pool on main, so autoreleased objects would "leak
/// with no pool in place". Once `[NSApp run]` starts, CF's own per-event and
/// per-callout pools take over (and the bridge's bottom pool is disabled on main
/// so it can't corrupt them).
pub fn autorelease_pool_push() -> Id {
    let f: extern "C" fn() -> Id = unsafe { std::mem::transmute(sym("objc_autoreleasePoolPush")) };
    f()
}

/// `objc_autoreleasePoolPop(token)` — drain everything autoreleased since the
/// matching [`autorelease_pool_push`].
pub fn autorelease_pool_pop(token: Id) {
    let f: extern "C" fn(Id) = unsafe { std::mem::transmute(sym("objc_autoreleasePoolPop")) };
    f(token);
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

/// Install the UI worker's inbox drain as a **default-mode** `CFRunLoopSource`
/// (CG4 §8) and remember it for [`wake_main_runloop`] to signal. MUST run on
/// main. `perform` is the drain callback — CoreFoundation calls it on the main
/// thread with `info` when the source is signalled AND the run loop is in
/// `NSDefaultRunLoopMode`; `info` is a raw pointer to main-owned drain state.
///
/// Default mode ONLY, deliberately (§8): a source in the *common* modes would
/// also fire inside AppKit's NESTED run loops (menu tracking, live resize, modal
/// sessions) — swapping a snapshot mid-tracking throws AppKit consistency
/// exceptions. The UI is intentionally stale during a tracking session and
/// re-syncs when it ends (the loop returns to default mode). The source is
/// `perform`-only (a signalled software source), never a port/timer.
pub fn install_default_mode_drain(info: *mut c_void, perform: extern "C" fn(*mut c_void)) {
    let mut ctx = CFRunLoopSourceContext {
        version: 0,
        info,
        retain: std::ptr::null(),
        release: std::ptr::null(),
        copy_description: std::ptr::null(),
        equal: std::ptr::null(),
        hash: std::ptr::null(),
        schedule: std::ptr::null(),
        cancel: std::ptr::null(),
        perform: Some(perform),
    };
    unsafe {
        let source = CFRunLoopSourceCreate(std::ptr::null(), 0, &mut ctx);
        if source.is_null() {
            return;
        }
        let rl = CFRunLoopGetMain();
        // Default mode ONLY — NOT kCFRunLoopCommonModes (§8).
        CFRunLoopAddSource(rl, source, kCFRunLoopDefaultMode);
        DRAIN_SOURCE.store(source, Ordering::Release);
    }
}

/// Wake the process's main run loop from a background thread — the run-loop
/// poke the externally-hosted UI worker's inbox wake fires when the primary
/// `send`s it (CG1's `InboxWakeFn`, the AppKit poke, design §8). Signals the
/// default-mode drain source (so its `perform` runs the drain once the run loop
/// is back in default mode) and wakes the loop. Both `CFRunLoopSourceSignal` and
/// `CFRunLoopWakeUp` are documented thread-safe. Before the source exists (early
/// boot) this is a bare nudge.
pub fn wake_main_runloop() {
    unsafe {
        let source = DRAIN_SOURCE.load(Ordering::Acquire);
        if !source.is_null() {
            CFRunLoopSourceSignal(source);
        }
        let rl = CFRunLoopGetMain();
        if !rl.is_null() {
            CFRunLoopWakeUp(rl);
        }
    }
}

// ── In-app client-area screenshots (dev aid) ────────────────────────────────
//
// Capture the front window's ON-SCREEN CONTENT to a PNG from INSIDE the app —
// so on-screen work is inspectable without a display. Driven by `snapshot.rs`
// off `MACVM_COCOA_SNAP`.
//
// Deliberately does NOT render via NSView + a struct-by-value `objc_msgSend`
// call (`cacheDisplayInRect:toBitmap:` etc., each needing an NSRect argument
// marshalled through a runtime-`transmute`d function pointer): that is exactly
// the ARM64 pitfall where a `#[repr(C)]` struct's true AAPCS64 HFA classification
// (passed in v0–v3) can silently disagree with what a transmuted, non-variadic-
// looking fn pointer causes LLVM to generate at the CALL SITE — it crashed here
// (`cache_display`, confirmed via a per-step trace: execution reached `rep=...`
// then aborted inside the very next struct-by-value call, "panic in a function
// that cannot unwind"). Instead this uses ONLY real, compiler-typed `extern
// "C"` declarations (`CGWindowListCreateImage`, ImageIO) — LLVM generates the
// correct calling convention for every struct argument at COMPILE time, same
// as the `CFRunLoop*` calls above, never via `transmute`. The one AppKit send
// needed (`windowNumber`) returns a plain `NSInteger` scalar — no struct-passing
// risk at all.

const K_CG_WINDOW_LIST_OPTION_INCLUDING_WINDOW: u32 = 1 << 3;
const K_CG_WINDOW_IMAGE_DEFAULT: u32 = 0;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

/// `CGRect` = `{{x,y},{w,h}}`. Only ever passed to a REAL typed `extern "C"`
/// function below (never transmuted) — see the module note above.
#[repr(C)]
#[derive(Clone, Copy)]
struct CgRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    /// Rasterize a window's on-screen content. `screen_bounds` is ignored when
    /// `list_option` is `kCGWindowListOptionIncludingWindow` (captures that
    /// window's own extent regardless), so `CgRect` here is passed correctly by
    /// the compiler even though its VALUE doesn't matter for this call shape.
    fn CGWindowListCreateImage(
        screen_bounds: CgRect,
        list_option: u32,
        window_id: u32,
        image_option: u32,
    ) -> *mut c_void;
}

#[link(name = "ImageIO", kind = "framework")]
extern "C" {
    fn CGImageDestinationCreateWithURL(
        url: *mut c_void,
        image_type: *mut c_void,
        count: usize,
        options: *mut c_void,
    ) -> *mut c_void;
    fn CGImageDestinationAddImage(dest: *mut c_void, image: *mut c_void, properties: *mut c_void);
    fn CGImageDestinationFinalize(dest: *mut c_void) -> u8;
}

extern "C" {
    fn CFRelease(cf: *mut c_void);
    fn CFStringCreateWithCString(
        alloc: *const c_void,
        c_str: *const i8,
        encoding: u32,
    ) -> *mut c_void;
    fn CFURLCreateFromFileSystemRepresentation(
        alloc: *const c_void,
        buffer: *const u8,
        buf_len: isize,
        is_directory: u8,
    ) -> *mut c_void;
}

/// `[win windowNumber]` — a plain `NSInteger` return, no struct in sight.
fn window_number(win: Id) -> i64 {
    let f: extern "C" fn(Id, Sel) -> i64 = unsafe { std::mem::transmute(msg_send_ptr()) };
    f(win, sel("windowNumber"))
}

/// Write a `CGImageRef` to `path` as a PNG via ImageIO. Consumes nothing owned
/// by the caller; `image` is still the caller's to release.
unsafe fn write_cgimage_png(image: *mut c_void, path: &str) -> bool {
    let Ok(c_path) = CString::new(path) else {
        return false;
    };
    let url = CFURLCreateFromFileSystemRepresentation(
        std::ptr::null(),
        c_path.as_ptr() as *const u8,
        c_path.as_bytes().len() as isize,
        0,
    );
    if url.is_null() {
        return false;
    }
    let Ok(png_type_str) = CString::new("public.png") else {
        CFRelease(url);
        return false;
    };
    let png_type = CFStringCreateWithCString(
        std::ptr::null(),
        png_type_str.as_ptr(),
        K_CF_STRING_ENCODING_UTF8,
    );
    if png_type.is_null() {
        CFRelease(url);
        return false;
    }
    let dest = CGImageDestinationCreateWithURL(url, png_type, 1, std::ptr::null_mut());
    let ok = if !dest.is_null() {
        CGImageDestinationAddImage(dest, image, std::ptr::null_mut());
        let finalized = CGImageDestinationFinalize(dest) != 0;
        CFRelease(dest);
        finalized
    } else {
        false
    };
    CFRelease(png_type);
    CFRelease(url);
    ok
}

/// Capture the front window's on-screen content to `path`. Runs on whatever
/// thread calls it — `CGWindowListCreateImage`/ImageIO are ordinary Quartz
/// calls, not AppKit view rendering, so no main-thread hop is needed; the one
/// AppKit send (`keyWindow`/`windowNumber`) is safe to call cross-thread for a
/// read like this (matches the design's existing off-main Foundation-read
/// posture). Best-effort: returns false if there is no window yet, or capture
/// fails (e.g. Screen Recording permission not granted).
pub fn snapshot_client_area(path: &str) -> bool {
    let app = app_shared();
    let mut win = send0(app, sel("keyWindow"));
    if win.is_null() {
        let windows = send0(app, sel("windows"));
        if !windows.is_null() {
            win = send0(windows, sel("firstObject"));
        }
    }
    if win.is_null() {
        return false;
    }
    let wid = window_number(win);
    if wid <= 0 {
        return false;
    }
    let image = unsafe {
        CGWindowListCreateImage(
            CgRect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
            },
            K_CG_WINDOW_LIST_OPTION_INCLUDING_WINDOW,
            wid as u32,
            K_CG_WINDOW_IMAGE_DEFAULT,
        )
    };
    if image.is_null() {
        return false;
    }
    let ok = unsafe { write_cgimage_png(image, path) };
    unsafe { CFRelease(image) };
    ok
}
