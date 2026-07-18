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

use image_store::{flows, Image, Side};

use crate::objc::{self, Id, Sel};

/// Reply conventions for the WRITE methods: `OK <payload>` / `ERR <message>`
/// (the Smalltalk side checks the prefix). Field separator for multi-part
/// payloads is the unit separator (0x1F) — it can't occur in identifiers.
const SEP: char = '\u{1f}';

fn ok(payload: &str) -> Id {
    objc::nsstring(&format!("OK {payload}"))
}
fn err(msg: &str) -> Id {
    objc::nsstring(&format!("ERR {msg}"))
}

fn side_from(s: &str) -> Side {
    match s {
        "class" => Side::Class,
        _ => Side::Instance,
    }
}

/// The write-capable image open (schema-ensuring — the same `Image::open`
/// the web GUI's own write path uses).
fn writer() -> Result<Image, String> {
    Image::open(&image_path()).map_err(|e| e.to_string())
}

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

/// `classShellFor:` — "superclass␟instanceVars␟classVars" from the image
/// (empty string when the class isn't stored). The browser's variables pane
/// reads THIS (the image), not the live snapshot — the web's own split: a
/// just-added variable shows immediately even though live instance shape
/// waits for the next boot.
extern "C" fn imp_class_shell_for(_this: *mut c_void, _cmd: *mut c_void, class_name: Id) -> Id {
    let class_name = ns_to_string(class_name);
    let shell = Image::open_read_only(&image_path())
        .ok()
        .and_then(|img| img.class_named(&class_name).ok())
        .flatten()
        .map(|c| {
            format!(
                "{}{SEP}{}{SEP}{}",
                c.superclass.unwrap_or_else(|| "nil".to_string()),
                c.instance_vars,
                c.class_vars
            )
        })
        .unwrap_or_default();
    objc::nsstring(&shell)
}

/// `newClassFrom:` — `flows::new_class_from_source` (the web's NewClass
/// accept sequence). `OK <name>` / `ERR <why>`.
extern "C" fn imp_new_class_from(_this: *mut c_void, _cmd: *mut c_void, text: Id) -> Id {
    let text = ns_to_string(text);
    match writer().and_then(|img| flows::new_class_from_source(&img, &text)) {
        Ok(name) => ok(&name),
        Err(e) => err(&e),
    }
}

/// `saveMethodFor:side:source:` — `flows::save_method` (the web's NewMethod/
/// Method accept sequence, versioned create-or-update). `OK <selector>`.
extern "C" fn imp_save_method(
    _this: *mut c_void,
    _cmd: *mut c_void,
    class_name: Id,
    side: Id,
    source: Id,
) -> Id {
    let class_name = ns_to_string(class_name);
    let side = side_from(&ns_to_string(side));
    let source = ns_to_string(source);
    match writer().and_then(|img| flows::save_method(&img, &class_name, side, &source)) {
        Ok(selector) => ok(&selector),
        Err(e) => err(&e),
    }
}

/// `removeMethodFor:side:selector:` — `Image::remove_method`, exactly the
/// web's BrowserRemoveMethod image half.
extern "C" fn imp_remove_method(
    _this: *mut c_void,
    _cmd: *mut c_void,
    class_name: Id,
    side: Id,
    selector: Id,
) -> Id {
    let class_name = ns_to_string(class_name);
    let side = side_from(&ns_to_string(side));
    let selector = ns_to_string(selector);
    match writer().map(|img| img.remove_method(&class_name, side, &selector)) {
        Ok(Ok(true)) => ok(""),
        Ok(Ok(false)) => err("no such method in the image"),
        Ok(Err(e)) => err(&e.to_string()),
        Err(e) => err(&e),
    }
}

/// `removeClassNamed:` — `Image::remove_class` (BrowserRemoveClass's image half).
extern "C" fn imp_remove_class(_this: *mut c_void, _cmd: *mut c_void, class_name: Id) -> Id {
    let class_name = ns_to_string(class_name);
    match writer().map(|img| img.remove_class(&class_name)) {
        Ok(Ok(true)) => ok(""),
        Ok(Ok(false)) => err("no such class in the image"),
        Ok(Err(e)) => err(&e.to_string()),
        Err(e) => err(&e),
    }
}

/// `addVarFor:kind:name:` — `flows::add_variable` (SmapplAddVar's image
/// half); `kind` is `'instance'` / `'class'`.
extern "C" fn imp_add_var(
    _this: *mut c_void,
    _cmd: *mut c_void,
    class_name: Id,
    kind: Id,
    name: Id,
) -> Id {
    let class_name = ns_to_string(class_name);
    let is_class_var = ns_to_string(kind) == "class";
    let name = ns_to_string(name);
    match writer().and_then(|img| flows::add_variable(&img, &class_name, is_class_var, &name)) {
        Ok(()) => ok(""),
        Err(e) => err(&e),
    }
}

/// `implementorsOf:` — `Image::implementors_of` (the web find's own query):
/// one line per implementor, `Class␟side`, newline-joined.
extern "C" fn imp_implementors_of(_this: *mut c_void, _cmd: *mut c_void, selector: Id) -> Id {
    let selector = ns_to_string(selector);
    let text = Image::open_read_only(&image_path())
        .ok()
        .and_then(|img| img.implementors_of(&selector).ok())
        .map(|rows| {
            rows.iter()
                .map(|(cls, side)| format!("{cls}{SEP}{}", if matches!(side, Side::Class) { "class" } else { "instance" }))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    objc::nsstring(&text)
}

/// `sendersOf:` — `Image::senders_of` (the `method_sends`-backed query):
/// one line per sending method, `Class␟selector␟side`.
extern "C" fn imp_senders_of(_this: *mut c_void, _cmd: *mut c_void, selector: Id) -> Id {
    let selector = ns_to_string(selector);
    let text = Image::open_read_only(&image_path())
        .ok()
        .and_then(|img| img.senders_of(&selector).ok())
        .map(|rows| {
            rows.iter()
                .map(|(cls, sel, side)| format!("{cls}{SEP}{sel}{SEP}{}", if matches!(side, Side::Class) { "class" } else { "instance" }))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    objc::nsstring(&text)
}

/// `definitionsOf:` — the web "Find Definition" kind: classes whose NAME
/// contains the term, case-insensitively (`Image::class_names` filtered —
/// the same rule the Smalltalk DefinitionsView's `contains:sub:` applies).
/// One class name per line.
extern "C" fn imp_definitions_of(_this: *mut c_void, _cmd: *mut c_void, term: Id) -> Id {
    let term = ns_to_string(term).to_lowercase();
    let text = Image::open_read_only(&image_path())
        .ok()
        .and_then(|img| img.class_names().ok())
        .map(|names| {
            names
                .into_iter()
                .filter(|n| n.to_lowercase().contains(&term))
                .collect::<Vec<_>>()
                .join("\n")
        })
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
    type Imp1 = extern "C" fn(*mut c_void, *mut c_void, Id) -> Id;
    type Imp3 = extern "C" fn(*mut c_void, *mut c_void, Id, Id, Id) -> Id;
    let methods: [(&str, *const c_void, &str); 11] = [
        ("implementorsOf:", imp_implementors_of as Imp1 as *const c_void, "@@:@"),
        ("sendersOf:", imp_senders_of as Imp1 as *const c_void, "@@:@"),
        ("definitionsOf:", imp_definitions_of as Imp1 as *const c_void, "@@:@"),
        ("sourceForClass:side:selector:", imp_source_for as Imp3 as *const c_void, "@@:@@@"),
        ("classSourceFor:", imp_class_source_for as Imp1 as *const c_void, "@@:@"),
        ("classShellFor:", imp_class_shell_for as Imp1 as *const c_void, "@@:@"),
        ("newClassFrom:", imp_new_class_from as Imp1 as *const c_void, "@@:@"),
        ("saveMethodFor:side:source:", imp_save_method as Imp3 as *const c_void, "@@:@@@"),
        ("removeMethodFor:side:selector:", imp_remove_method as Imp3 as *const c_void, "@@:@@@"),
        ("removeClassNamed:", imp_remove_class as Imp1 as *const c_void, "@@:@"),
        ("addVarFor:kind:name:", imp_add_var as Imp3 as *const c_void, "@@:@@@"),
    ];
    for (sel_name, imp, types) in methods {
        let types = CString::new(types).expect("types");
        unsafe { add(cls, objc::sel(sel_name), imp, types.as_ptr()) };
    }
    unsafe { reg(cls) };
}
