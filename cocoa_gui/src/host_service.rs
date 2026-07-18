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

/// The world source tree the editor accept also exports to (surgical `.mst`
/// splice) — `MACVM_WORLD` or `world`, matching the primary's boot convention.
fn world_dir() -> PathBuf {
    std::env::var_os("MACVM_WORLD")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("world"))
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

/// `classNames` — every class name in the image, newline-joined (the editor's
/// picker options — the Cocoa analog of the web editor's `<datalist>` fed by
/// `Image::class_names`). Read-only image open, same as every other fetch here.
extern "C" fn imp_class_names(_this: *mut c_void, _cmd: *mut c_void) -> Id {
    let text = Image::open_read_only(&image_path())
        .ok()
        .and_then(|img| img.class_names().ok())
        .map(|names| names.join("\n"))
        .unwrap_or_default();
    objc::nsstring(&text)
}

/// `allSelectors` — every distinct selector in the image, newline-joined (the
/// Find field's autocomplete options for Implementors/Senders).
extern "C" fn imp_all_selectors(_this: *mut c_void, _cmd: *mut c_void) -> Id {
    let text = Image::open_read_only(&image_path())
        .ok()
        .and_then(|img| img.all_selectors().ok())
        .map(|sels| sels.join("\n"))
        .unwrap_or_default();
    objc::nsstring(&text)
}

/// `browseRecords` — the whole class hierarchy as flat records for the browser
/// tree, ONE per class, fields `␟`-separated:
///   `name ␟ superclass ␟ instVars ␟ classVars ␟ instSelectors ␟ classSelectors`
/// (the four var/selector lists are space-separated names), records `\n`-joined.
/// This is the DATABASE-query replacement for the primary VM's live reflection
/// snapshot (`UiBrowserService browseSnapshot`) — the image is the source of
/// truth, same as the editor + the Find views. The UI rebuilds the nested tree
/// from the `superclass` links.
extern "C" fn imp_browse_records(_this: *mut c_void, _cmd: *mut c_void) -> Id {
    let text = Image::open_read_only(&image_path())
        .ok()
        .map(|img| {
            let mut out = String::new();
            for name in img.class_names().unwrap_or_default() {
                let cls = img.class_named(&name).ok().flatten();
                let (superc, ivars, cvars) = match &cls {
                    Some(c) => (
                        c.superclass.clone().unwrap_or_default(),
                        c.instance_vars.clone(),
                        c.class_vars.clone(),
                    ),
                    None => (String::new(), String::new(), String::new()),
                };
                let methods = img.all_methods_of(&name).unwrap_or_default();
                let inst = methods
                    .iter()
                    .filter(|m| m.side != Side::Class)
                    .map(|m| m.selector.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                let clss = methods
                    .iter()
                    .filter(|m| m.side == Side::Class)
                    .map(|m| m.selector.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                out.push_str(&format!(
                    "{name}{SEP}{superc}{SEP}{ivars}{SEP}{cvars}{SEP}{inst}{SEP}{clss}\n"
                ));
            }
            out
        })
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

/// `colorizeStorage:` — apply Smalltalk syntax colour to an `NSTextStorage`
/// in ONE bridge send: read its string, scan spans ([`crate::colorize`] — the
/// web highlighter's exact six categories), then attribute-only mutations
/// (`beginEditing` … `endEditing`). Attributes never move the caret, so this
/// is safe on every keystroke (the text delegate calls it from
/// `textDidChange:`). Answers nil (no reply wrapper for the caller to leak).
extern "C" fn imp_colorize_storage(_this: *mut c_void, _cmd: *mut c_void, storage: Id) -> Id {
    use crate::colorize::{spans_utf16, Kind};
    if storage.is_null() {
        return std::ptr::null_mut();
    }
    let ns = objc::send0(storage, objc::sel("string"));
    let text = ns_to_string(ns);
    let total = objc::send0_int(ns, objc::sel("length")) as u64;
    let spans = spans_utf16(&text);

    let name_color = objc::nsstring("NSColor"); // NSForegroundColorAttributeName
    let name_font = objc::nsstring("NSFont"); // NSFontAttributeName
    let nscolor = objc::get_class("NSColor");
    let color_named =
        |sel_name: &str| -> Id { objc::send0(nscolor, objc::sel(sel_name)) };
    let base_color = color_named("labelColor");
    let nsfont = objc::get_class("NSFont");
    // The pane's own face at its own size; bold via the monospaced system
    // font at bold weight (0.4 = NSFontWeightBold).
    let base_font = objc::send1_f64(nsfont, objc::sel("userFixedPitchFontOfSize:"), 12.0);
    let bold_font = objc::send2_f64(
        nsfont,
        objc::sel("monospacedSystemFontOfSize:weight:"),
        12.0,
        0.4,
    );

    objc::send0(storage, objc::sel("beginEditing"));
    // Reset the whole range to base colour + face (clears stale spans).
    objc::send_attr(storage, name_color, base_color, 0, total);
    objc::send_attr(storage, name_font, base_font, 0, total);
    for (start, len, kind) in spans {
        if start >= total {
            continue;
        }
        let len = len.min(total - start); // defensive clamp: never out-of-range
        // The web palette, in appearance-adaptive NSColors: comment gray,
        // string brown-red, symbol teal, keyword blue bold, pseudo purple,
        // number bold.
        let (color_sel, bold) = match kind {
            Kind::Comment => ("secondaryLabelColor", false),
            Kind::Str => ("systemBrownColor", false),
            Kind::Symbol => ("systemTealColor", false),
            Kind::Keyword => ("systemBlueColor", true),
            Kind::Pseudo => ("systemPurpleColor", false),
            Kind::Number => ("labelColor", true),
        };
        objc::send_attr(storage, name_color, color_named(color_sel), start, len);
        if bold {
            objc::send_attr(storage, name_font, bold_font, start, len);
        }
    }
    objc::send0(storage, objc::sel("endEditing"));
    std::ptr::null_mut()
}

/// `requestUiRebuild` — CG9 Layer-2 trigger: flag a full UI-worker rebuild and
/// wake the run loop. A menu item calls this (optionally after deliberately
/// faulting, to prove a Layer-1-recovered fault can escalate to a rebuild). The
/// flag is serviced by `drain_perform` on the next main-thread pass, never
/// inside this callback. Answers nil.
extern "C" fn imp_request_ui_rebuild(_this: *mut c_void, _cmd: *mut c_void) -> Id {
    crate::rebuild::request();
    crate::objc::wake_main_runloop();
    std::ptr::null_mut()
}

/// `requestBrowserRefresh` / `requestFindRefresh` — flag the Browser tree /
/// Find options for a DB re-query and wake the run loop. Serviced by
/// `drain_perform` on its next pass (a fresh top-level `exec`, never nested in
/// the callback that asked) — see view_refresh.rs for why. Answers nil.
extern "C" fn imp_request_browser_refresh(_this: *mut c_void, _cmd: *mut c_void) -> Id {
    crate::view_refresh::request_browser();
    crate::objc::wake_main_runloop();
    std::ptr::null_mut()
}
extern "C" fn imp_request_find_refresh(_this: *mut c_void, _cmd: *mut c_void) -> Id {
    crate::view_refresh::request_find();
    crate::objc::wake_main_runloop();
    std::ptr::null_mut()
}
extern "C" fn imp_request_outliner_refresh(_this: *mut c_void, _cmd: *mut c_void) -> Id {
    crate::view_refresh::request_outliner();
    crate::objc::wake_main_runloop();
    std::ptr::null_mut()
}

/// `showCanvasPixelsOn:base64:width:height:` — decode `base64` (the Pixmap's
/// raw RGBA bytes, world/36_pixmap.mst) and set it on `view` (an NSImageView
/// Smalltalk built and passed in directly — arrives here as the real ObjC id,
/// no conversion needed, since a non-String `@` arg crosses as an ObjcRef's
/// raw id). `view` is the RAW Id itself: this IMP receives it as `Id` already
/// (the bridge's `ObjcArg::Id` marshaller passes an ObjcRef's `read_id`
/// straight through — only a String-shaped arg gets NSString-converted).
extern "C" fn imp_show_canvas_pixels(
    _this: *mut c_void,
    _cmd: *mut c_void,
    view: Id,
    base64: Id,
    width: Id,
    height: Id,
) -> Id {
    let b64 = ns_to_string(base64);
    let w: u32 = ns_to_string(width).parse().unwrap_or(0);
    let h: u32 = ns_to_string(height).parse().unwrap_or(0);
    crate::canvas::show_pixels_base64(view, &b64, w, h);
    std::ptr::null_mut()
}

/// `showCanvasCommandsOn:commands:width:height:` — the vector-command path
/// (docs/CANVAS.md §5.2), e.g. `BenchmarkDashboard chartForWidth:height:`'s
/// own JSON, reused verbatim.
extern "C" fn imp_show_canvas_commands(
    _this: *mut c_void,
    _cmd: *mut c_void,
    view: Id,
    commands: Id,
    width: Id,
    height: Id,
) -> Id {
    let ops = ns_to_string(commands);
    let w: u32 = ns_to_string(width).parse().unwrap_or(0);
    let h: u32 = ns_to_string(height).parse().unwrap_or(0);
    crate::canvas::show_commands(view, &ops, w, h);
    std::ptr::null_mut()
}

/// `launchDemo:` — CG10: launch a GamePane demo by its entry doit (e.g.
/// `'MandelZoom launch'`). Flags it to run TOP-LEVEL on the primary's supervisor
/// loop, NOT via a nested `uiDoit`/`primEvalDoit` (which corrupts the frame
/// loop's GC roots). Answers nil.
extern "C" fn imp_launch_demo(_this: *mut c_void, _cmd: *mut c_void, entry: Id) -> Id {
    crate::game::request_launch(ns_to_string(entry));
    std::ptr::null_mut()
}

/// `acceptEditorClass:` — the editor's whole-class accept (docs/editor_design.md
/// M4/M5). Dual-placed here (composing the same `image_store` primitives the web
/// GUI's `persist_editor_class` uses) so the Cocoa editor persists WITHOUT
/// touching `gui/vm_host` — reuse without breaking the web GUI. Syntax-gate with
/// the REAL compiler parser (a red here is a genuine compile error, the same one
/// the next boot would hit — and there is no live install, per the design), then
/// write ONLY the diff to the image (a byte-identical method keeps its version —
/// no history churn, no nmethod invalidation), then splice the world `.mst` tree.
/// Answers `OK <summary>` / `ERR <why>` for the transcript.
extern "C" fn imp_accept_editor_class(_this: *mut c_void, _cmd: *mut c_void, text: Id) -> Id {
    let text = ns_to_string(text);
    if let Err(e) = macvm::frontend::parser::parse_file(&text) {
        return objc::nsstring(&format!("ERR not saved — {e}"));
    }
    let img = match writer() {
        Ok(i) => i,
        Err(e) => return objc::nsstring(&format!("ERR no image database ({e})")),
    };
    let db = persist_editor_class(&img, &text);
    let world = match image_store::export::export_world_dir(&img, &world_dir()) {
        Ok(s) => format!(
            "; world +{} file(s), {} updated, {} added",
            s.files_changed, s.methods_updated, s.methods_added
        ),
        Err(e) => format!("; world export failed: {e}"),
    };
    objc::nsstring(&format!("OK {db}{world} (takes effect on next launch)"))
}

/// Persist the editor buffer's class to the image, writing only the DIFF — the
/// web GUI's `persist_editor_class` logic, over the shared `image_store` API.
/// Re-parses the (already syntax-checked) buffer with `image_store::mst` purely
/// to diff against what's stored; unchanged methods are left alone, changed/new
/// are reopened, vanished ones removed; the shell (superclass + ivars) is set
/// via `set_class_definition` (the one edit a live redefinition can't do — why
/// Save goes to the DB, taking effect next boot). Comment/classVars not yet
/// round-tripped (first-cut scope, same as the web GUI). Returns a one-line
/// summary.
fn persist_editor_class(img: &Image, text: &str) -> String {
    use std::collections::{HashMap, HashSet};
    let parsed = image_store::mst::parse_mst_source(text);
    let cls = match parsed.first() {
        Some(c) => c,
        None => return "nothing to save (no class definition in the buffer)".to_string(),
    };
    let _ = img.create_or_reopen_class(
        &cls.name,
        cls.superclass.as_deref(),
        "",
        "",
        &cls.instance_vars,
    );
    let _ = img.set_class_definition(&cls.name, cls.superclass.as_deref(), &cls.instance_vars);

    let stored: HashMap<(String, bool), String> = img
        .all_methods_of(&cls.name)
        .unwrap_or_default()
        .into_iter()
        .map(|m| ((m.selector, m.side == Side::Class), m.source))
        .collect();

    let (mut changed, mut added) = (0u32, 0u32);
    for m in &cls.methods {
        let side = if m.is_class_side { Side::Class } else { Side::Instance };
        match stored.get(&(m.selector.clone(), m.is_class_side)) {
            Some(old) if *old == m.source => {}
            Some(_) => {
                let _ = img.create_or_reopen_method(&cls.name, side, &m.selector, "", &m.source);
                changed += 1;
            }
            None => {
                let _ = img.create_or_reopen_method(&cls.name, side, &m.selector, "", &m.source);
                added += 1;
            }
        }
    }

    let present: HashSet<(String, bool)> = cls
        .methods
        .iter()
        .map(|m| (m.selector.clone(), m.is_class_side))
        .collect();
    let mut removed = 0u32;
    for (sel, is_cls) in stored.keys() {
        if !present.contains(&(sel.clone(), *is_cls)) {
            let side = if *is_cls { Side::Class } else { Side::Instance };
            let _ = img.remove_method(&cls.name, side, sel);
            removed += 1;
        }
    }

    if changed + added + removed == 0 {
        format!("{} — no changes", cls.name)
    } else {
        format!(
            "saved {} ({changed} changed, {added} added, {removed} removed)",
            cls.name
        )
    }
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
    type Imp0 = extern "C" fn(*mut c_void, *mut c_void) -> Id;
    type Imp1 = extern "C" fn(*mut c_void, *mut c_void, Id) -> Id;
    type Imp3 = extern "C" fn(*mut c_void, *mut c_void, Id, Id, Id) -> Id;
    type Imp4 = extern "C" fn(*mut c_void, *mut c_void, Id, Id, Id, Id) -> Id;
    let methods: [(&str, *const c_void, &str); 23] = [
        ("requestUiRebuild", imp_request_ui_rebuild as Imp0 as *const c_void, "@@:"),
        ("requestBrowserRefresh", imp_request_browser_refresh as Imp0 as *const c_void, "@@:"),
        ("requestFindRefresh", imp_request_find_refresh as Imp0 as *const c_void, "@@:"),
        ("requestOutlinerRefresh", imp_request_outliner_refresh as Imp0 as *const c_void, "@@:"),
        (
            "showCanvasPixelsOn:base64:width:height:",
            imp_show_canvas_pixels as Imp4 as *const c_void,
            "@@:@@@@",
        ),
        (
            "showCanvasCommandsOn:commands:width:height:",
            imp_show_canvas_commands as Imp4 as *const c_void,
            "@@:@@@@",
        ),
        ("classNames", imp_class_names as Imp0 as *const c_void, "@@:"),
        ("allSelectors", imp_all_selectors as Imp0 as *const c_void, "@@:"),
        ("browseRecords", imp_browse_records as Imp0 as *const c_void, "@@:"),
        ("acceptEditorClass:", imp_accept_editor_class as Imp1 as *const c_void, "@@:@"),
        ("launchDemo:", imp_launch_demo as Imp1 as *const c_void, "@@:@"),
        ("colorizeStorage:", imp_colorize_storage as Imp1 as *const c_void, "@@:@"),
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
