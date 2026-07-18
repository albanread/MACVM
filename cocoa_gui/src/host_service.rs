//! `MacvmHostService` — the native side of the CG7 browser's SOURCE pane.
//!
//! The primary VM keeps no method source (methods compile and drop their text;
//! the SQLite image is the source of truth), so source display is served by the
//! HOST, exactly as the WKWebView GUI does it (`vm_host`'s browser requests →
//! `image_store::method_source`) — dual placement: the VM snapshot supplies the
//! ROWS, the image supplies the TEXT, in both GUIs.
//!
//! Mechanism: one `NSObject` subclass registered at boot with two instance
//! methods. The UI worker reaches it through an ORDINARY C3 bridge send —
//! `(Cocoa classNamed: 'MacvmHostService') … sourceForClass:side:selector:` —
//! no new primitive, no protocol verb, no VM re-entry: the IMP runs pure Rust
//! (SQLite read) and answers an autoreleased `NSString` the caller copies
//! immediately. Every call happens on the main thread inside a UI-worker
//! callback, where the run-loop autorelease pool is live.
//!
//! A miss (class/method not in the image — e.g. defined live this session)
//! answers the EMPTY string; the Smalltalk side owns the user-facing wording.

use std::ffi::{c_void, CStr, CString};
use std::os::raw::c_char;
use std::path::PathBuf;

use image_store::{Image, Side};

use crate::objc::{self, Id, Sel};

/// The image the source pane reads: `MACVM_IMAGE_PATH` or the same
/// `world/image.sqlite3` default the WKWebView GUI browses. Opened per call —
/// a click-driven read, and never holding a connection across the run loop.
fn image_path() -> PathBuf {
    std::env::var_os("MACVM_IMAGE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("world/image.sqlite3"))
}

/// Read an `NSString` argument's UTF-8 contents (empty for nil / non-string).
fn ns_to_string(ns: Id) -> String {
    if ns.is_null() {
        return String::new();
    }
    let p = objc::send0(ns, objc::sel("UTF8String")) as *const c_char;
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// `sourceForClass:side:selector:` — the latest non-deleted method source from
/// the image, or `""`. `side` is `'instance'` / `'class'`.
extern "C" fn imp_source_for(
    _this: *mut c_void,
    _cmd: *mut c_void,
    class_name: Id,
    side: Id,
    selector: Id,
) -> Id {
    let class_name = ns_to_string(class_name);
    let selector = ns_to_string(selector);
    let side = match ns_to_string(side).as_str() {
        "class" => Side::Class,
        _ => Side::Instance,
    };
    let text = Image::open_read_only(&image_path())
        .ok()
        .and_then(|img| img.method_source(&class_name, side, &selector).ok())
        .flatten()
        .unwrap_or_default();
    objc::nsstring(&text)
}

/// `classSourceFor:` — the class's whole-definition source (the editor's
/// round-trip text), or `""`.
extern "C" fn imp_class_source_for(
    _this: *mut c_void,
    _cmd: *mut c_void,
    class_name: Id,
) -> Id {
    let class_name = ns_to_string(class_name);
    let text = Image::open_read_only(&image_path())
        .ok()
        .and_then(|img| img.class_source(&class_name).ok())
        .flatten()
        .unwrap_or_default();
    objc::nsstring(&text)
}

type AllocPair = unsafe extern "C" fn(Id, *const c_char, usize) -> Id;
type RegisterPair = unsafe extern "C" fn(Id);
type AddMethod = unsafe extern "C" fn(Id, Sel, *const c_void, *const c_char) -> u8;

/// Register the `MacvmHostService` class once, at boot (before `CocoaUI
/// startup` can resolve it). Idempotent by construction: a second alloc of the
/// same name returns NULL and we leave the first registration in place.
pub fn register() {
    let alloc: AllocPair = unsafe { std::mem::transmute(objc::sym_addr("objc_allocateClassPair")) };
    let reg: RegisterPair = unsafe { std::mem::transmute(objc::sym_addr("objc_registerClassPair")) };
    let add: AddMethod = unsafe { std::mem::transmute(objc::sym_addr("class_addMethod")) };
    let superclass = objc::get_class("NSObject");
    let name = CString::new("MacvmHostService").expect("class name");
    let cls = unsafe { alloc(superclass, name.as_ptr(), 0) };
    if cls.is_null() {
        return; // already registered (a restart re-runs boot paths)
    }
    let methods: [(&str, *const c_void, &str); 2] = [
        (
            "sourceForClass:side:selector:",
            imp_source_for
                as extern "C" fn(*mut c_void, *mut c_void, Id, Id, Id) -> Id
                as *const c_void,
            "@@:@@@",
        ),
        (
            "classSourceFor:",
            imp_class_source_for as extern "C" fn(*mut c_void, *mut c_void, Id) -> Id
                as *const c_void,
            "@@:@",
        ),
    ];
    for (sel_name, imp, types) in methods {
        let types = CString::new(types).expect("types");
        unsafe { add(cls, objc::sel(sel_name), imp, types.as_ptr()) };
    }
    unsafe { reg(cls) };
}
