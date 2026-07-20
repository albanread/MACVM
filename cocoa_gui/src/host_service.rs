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

/// Defense-in-depth for the `US`/newline-framed record replies
/// (`browse_records_text`, `package_tree_text`): every interpolated field is
/// a DB-validated identifier today, but a malformed image row holding a
/// framing char would silently corrupt the whole record stream on the
/// Smalltalk side (review latent-hazard finding) — scrub rather than trust.
fn clean_field(s: &str) -> String {
    if s.contains(SEP) || s.contains('\n') {
        s.replace(SEP, " ").replace('\n', " ")
    } else {
        s.to_string()
    }
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
/// `pub(crate)`: `boot.rs` reuses this exact resolution (M4,
/// `docs/package_aware_editing_design.md` §4.5) rather than a second copy.
pub(crate) fn image_path() -> PathBuf {
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
///   `name ␟ superclass ␟ instVars ␟ classVars ␟ instSelectors ␟ classSelectors ␟ loaded`
/// (the four var/selector lists are space-separated names; `loaded` is `1`/`0`),
/// records `\n`-joined. This is the DATABASE-query replacement for the primary
/// VM's live reflection snapshot (`UiBrowserService browseSnapshot`) — the image
/// is the source of truth, same as the editor + the Find views. The UI rebuilds
/// the nested tree from the `superclass` links.
///
/// `loaded` (M6, `docs/package_aware_editing_design.md` §4.3): the tree is the
/// WHOLE database (every package), but the primary VM boots only
/// [`crate::boot::PRIMARY_LISTS`]' packages — so a class the primary never
/// booted (`CocoaHelp`, a `cocoaui` class) still appears here and is editable,
/// but M5's gate applies its edit to the image ONLY, not live. This flag marks
/// which classes the primary actually has, so the browser can annotate the
/// database-only rows. Sound by construction: a class whose package is in one of
/// the primary's booted lists IS booted there, so `loaded == 1` never
/// over-claims (it can only under-claim a class defined live this session, whose
/// `interactive` package isn't in `world` — the safe direction: the annotation
/// never promises "live" for an edit M5 would gate).
/// The pure record-builder behind [`imp_browse_records`] — separated so the
/// 7-field record shape + the M6 `loaded` flag are unit-testable without the
/// Objective-C runtime. `primary_lists` is which lists the primary booted
/// ([`crate::boot::PRIMARY_LISTS`] in the bin); a class is `loaded` iff its
/// package is in one of them.
fn browse_records_text(img: &Image, primary_lists: &[&str]) -> String {
    let primary_pkgs: std::collections::HashSet<String> = primary_lists
        .iter()
        .flat_map(|l| img.packages_in_list(l).unwrap_or_default())
        .collect();
    let mut out = String::new();
    for name in img.class_names().unwrap_or_default() {
        let cls = img.class_named(&name).ok().flatten();
        let (superc, ivars, cvars, category) = match &cls {
            Some(c) => (
                c.superclass.clone().unwrap_or_default(),
                c.instance_vars.clone(),
                c.class_vars.clone(),
                c.category.clone(),
            ),
            None => (String::new(), String::new(), String::new(), String::new()),
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
        let loaded = if primary_pkgs.contains(&category) { "1" } else { "0" };
        let (name, superc, ivars, cvars, inst, clss) = (
            clean_field(&name),
            clean_field(&superc),
            clean_field(&ivars),
            clean_field(&cvars),
            clean_field(&inst),
            clean_field(&clss),
        );
        out.push_str(&format!(
            "{name}{SEP}{superc}{SEP}{ivars}{SEP}{cvars}{SEP}{inst}{SEP}{clss}{SEP}{loaded}\n"
        ));
    }
    out
}

extern "C" fn imp_browse_records(_this: *mut c_void, _cmd: *mut c_void) -> Id {
    let text = Image::open_read_only(&image_path())
        .ok()
        .map(|img| browse_records_text(&img, crate::boot::PRIMARY_LISTS))
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
/// accept sequence). `OK <name>` / `ERR <why>`. `None` for `default_source_file`
/// (docs/package_aware_editing_design.md §4.2): CocoaBrowser's create flow
/// isn't package-scoped, same as the web outliner's own NewClass arm — the
/// new class's methods fall back to `flows::INTERACTIVE_SOURCE_FILE`.
extern "C" fn imp_new_class_from(_this: *mut c_void, _cmd: *mut c_void, text: Id) -> Id {
    let text = ns_to_string(text);
    match writer().and_then(|img| flows::new_class_from_source(&img, &text, None)) {
        Ok(name) => ok(&name),
        Err(e) => err(&e),
    }
}

/// `saveMethodFor:side:source:` — `flows::save_method` (the web's NewMethod/
/// Method accept sequence, versioned create-or-update). `OK <selector>`.
/// `None` for `default_source_file` — see `imp_new_class_from`; an edit of
/// an already-imported method keeps its real home regardless (`save_method`'s
/// own doc comment).
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
    match writer().and_then(|img| flows::save_method(&img, &class_name, side, &source, None)) {
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
/// `requestPrimaryRestart` — a Debug-menu action that flags an immediate
/// primary respawn-from-source (the same watchdog path a fatal doit
/// triggers automatically, CG4 §5.1) and wakes the run loop. Serviced by
/// `drain_perform` on its next pass, which calls `PrimarySupervisor::
/// restart()` itself — never directly from this callback, since the
/// supervisor instance lives in `DrainState`, unreachable from a static
/// `extern "C"` IMP (see primary_restart.rs). Answers nil.
extern "C" fn imp_request_primary_restart(_this: *mut c_void, _cmd: *mut c_void) -> Id {
    crate::primary_restart::request();
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
/// `requestFindQuery` — flag a pending Implementors/Senders/Definitions
/// query (`CocoaFind`'s own `Kind`/search-term class vars already hold
/// which) and wake the run loop. Serviced by `drain_perform` on its next
/// pass, never nested in the button-click callback that asked — the query
/// + `Results reloadData` used to run there directly, which is exactly the
/// "fails closed" case view_refresh.rs's own doc describes: the query ran
/// and populated `Rows` correctly, but the table never visually redrew.
/// Answers nil.
extern "C" fn imp_request_find_query(_this: *mut c_void, _cmd: *mut c_void) -> Id {
    crate::view_refresh::request_find_query();
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

/// `commentFor:` — the selected class's stored comment (`""` if none). The V2
/// browser's "Comment" source-mode reads THIS; the comment is also the leading
/// string of `classSourceFor:`, but the stored field is exact and cheap.
extern "C" fn imp_comment_for(_this: *mut c_void, _cmd: *mut c_void, class_name: Id) -> Id {
    let class_name = ns_to_string(class_name);
    let text = Image::open_read_only(&image_path())
        .ok()
        .and_then(|img| img.class_named(&class_name).ok())
        .flatten()
        .map(|c| c.comment)
        .unwrap_or_default();
    objc::nsstring(&text)
}

/// `setCommentFor:comment:` — persist an edited class comment to the image
/// (`Image::set_class_comment`, the same versioned write the web editor uses).
/// A comment is metadata, so it takes effect immediately for the browser (it
/// reads the image) with no live recompile. `OK` / `ERR <why>`.
extern "C" fn imp_set_comment_for(
    _this: *mut c_void,
    _cmd: *mut c_void,
    class_name: Id,
    comment: Id,
) -> Id {
    let class_name = ns_to_string(class_name);
    let comment = ns_to_string(comment);
    match writer().map(|img| img.set_class_comment(&class_name, &comment)) {
        Ok(Ok(true)) => ok(""),
        Ok(Ok(false)) => err("no such class in the image"),
        Ok(Err(e)) => err(&e.to_string()),
        Err(e) => err(&e),
    }
}

/// `packageTree` — the V2 browser's system-category pane: the package lists
/// (M1) as a two-level grouping, one line per `(list, package)`:
///   `list ␟ package ␟ class1 class2 …`   (classes space-separated, sorted)
/// records `\n`-joined. A class's category IS its package (its source-file
/// stem, e.g. `ordered`); `package_lists`/`packages_in_list` group packages
/// into `world` / `cocoaui`. Every class already carries a category (0
/// orphans today), but categories that belong to no list — the empty category
/// of an interactively-created class, say — are emitted under a synthetic
/// `(other)` list so NO class ever disappears from the pane (the display side
/// of "a class with no category falls back to its list").
fn package_tree_text(img: &Image) -> String {
    use std::collections::{HashMap, HashSet};
    let mut by_cat: HashMap<String, Vec<String>> = HashMap::new();
    for name in img.class_names().unwrap_or_default() {
        let cat = img
            .class_named(&name)
            .ok()
            .flatten()
            .map(|c| c.category)
            .unwrap_or_default();
        by_cat.entry(cat).or_default().push(name);
    }
    for classes in by_cat.values_mut() {
        classes.sort();
    }
    let mut out = String::new();
    let mut emitted: HashSet<String> = HashSet::new();
    for list in img.package_lists().unwrap_or_default() {
        for pkg in img.packages_in_list(&list).unwrap_or_default() {
            emitted.insert(pkg.clone());
            let classes = by_cat.get(&pkg).cloned().unwrap_or_default();
            let (list, pkg) = (clean_field(&list), clean_field(&pkg));
            out.push_str(&format!(
                "{list}{SEP}{pkg}{SEP}{}\n",
                clean_field(&classes.join(" "))
            ));
        }
    }
    let mut orphans: Vec<(&String, &Vec<String>)> =
        by_cat.iter().filter(|(k, _)| !emitted.contains(*k)).collect();
    orphans.sort_by(|a, b| a.0.cmp(b.0));
    for (cat, classes) in orphans {
        let label = clean_field(if cat.is_empty() { "(uncategorized)" } else { cat });
        out.push_str(&format!(
            "(other){SEP}{label}{SEP}{}\n",
            clean_field(&classes.join(" "))
        ));
    }
    out
}

extern "C" fn imp_package_tree(_this: *mut c_void, _cmd: *mut c_void) -> Id {
    let text = Image::open_read_only(&image_path())
        .ok()
        .map(|img| package_tree_text(&img))
        .unwrap_or_default();
    objc::nsstring(&text)
}

/// `setJitCompile:` — the Debug menu's Compiler (JIT) toggle. `'1'` enables JIT
/// compilation, `'0'` disables it (the whole system then runs interpreted for
/// anything not already compiled). A plain global atomic, safe to flip from the
/// main thread; each VM's own thread reads it at its next compile trigger.
extern "C" fn imp_set_jit_compile(_this: *mut c_void, _cmd: *mut c_void, flag: Id) -> Id {
    macvm::runtime::set_jit_compile_enabled(ns_to_string(flag) == "1");
    std::ptr::null_mut()
}

/// `formatSource:` — Source ▸ Format: the comment/string-preserving
/// re-indenter (`crate::format`). Pure text → text; the Smalltalk side swaps
/// the buffer.
extern "C" fn imp_format_source(_this: *mut c_void, _cmd: *mut c_void, text: Id) -> Id {
    objc::nsstring(&crate::format::format_source(&ns_to_string(text)))
}

/// `analyzeSource:` — Source ▸ Analyze: the REAL compiler front end
/// (`frontend::parser::parse_file`, the same gate `acceptEditorClass:` uses).
/// Answers `OK` or `ERR <line> <col> <message>` (message newline-flattened) —
/// the Smalltalk side reports it and marks/scrolls to the line.
extern "C" fn imp_analyze_source(_this: *mut c_void, _cmd: *mut c_void, text: Id) -> Id {
    match macvm::frontend::parser::parse_file(&ns_to_string(text)) {
        Ok(_) => objc::nsstring("OK"),
        Err(e) => objc::nsstring(&format!(
            "ERR {} {} {}",
            e.span.line,
            e.span.col,
            e.msg.replace('\n', " ")
        )),
    }
}

/// `markErrorStorage:line:` — paint an analyze error: clear any previous mark
/// (whole-range background → clearColor), then, for `line` ≥ 1, background
/// that line in translucent red. `line` `'0'` = clear only (an OK analyze).
/// The colorize discipline: attribute-only mutations inside begin/endEditing,
/// UTF-16 offsets, never moves the caret. Answers nil.
extern "C" fn imp_mark_error_storage(
    _this: *mut c_void,
    _cmd: *mut c_void,
    storage: Id,
    line: Id,
) -> Id {
    if storage.is_null() {
        return std::ptr::null_mut();
    }
    let line: u64 = ns_to_string(line).parse().unwrap_or(0);
    let ns = objc::send0(storage, objc::sel("string"));
    let text = ns_to_string(ns);
    let total = objc::send0_int(ns, objc::sel("length")) as u64;

    // UTF-16 start/len of 1-based line N.
    let (mut off, mut start, mut len_line) = (0u64, 0u64, 0u64);
    let mut cur = 1u64;
    for ch in text.chars() {
        let w = ch.len_utf16() as u64;
        if cur == line {
            if ch == '\n' {
                break;
            }
            len_line += w;
        } else if ch == '\n' {
            cur += 1;
            if cur == line {
                start = off + w;
            }
        }
        off += w;
    }
    if line == 1 {
        start = 0;
    }

    let name_bg = objc::nsstring("NSBackgroundColor"); // NSBackgroundColorAttributeName
    let nscolor = objc::get_class("NSColor");
    let clear = objc::send0(nscolor as Id, objc::sel("clearColor"));
    objc::send0(storage, objc::sel("beginEditing"));
    objc::send_attr(storage, name_bg, clear, 0, total);
    if line >= 1 && start < total && len_line > 0 {
        let red = objc::send0(nscolor as Id, objc::sel("systemRedColor"));
        let red = objc::send1_f64(red, objc::sel("colorWithAlphaComponent:"), 0.25);
        let len = len_line.min(total - start);
        objc::send_attr(storage, name_bg, red, start, len);
    }
    objc::send0(storage, objc::sel("endEditing"));
    std::ptr::null_mut()
}

/// `fileInPath:` — File ▸ File In. THE CONTRACT: a fresh world, then the
/// file (filein.rs's module doc) — park the path for after-restart, then
/// request the same primary respawn the Debug menu's Restart uses. The new
/// generation's pump runs `VmHandle::run_file` top-level; the outcome is
/// reported through the primary's transcript forwarding. Answers nil.
extern "C" fn imp_file_in_path(_this: *mut c_void, _cmd: *mut c_void, path: Id) -> Id {
    crate::filein::request_after_restart(ns_to_string(path));
    crate::primary_restart::request();
    crate::objc::wake_main_runloop();
    std::ptr::null_mut()
}

/// `addToWorldPath:` — File ▸ Add to World: graduate the user's file INTO the
/// world. Copies it to `world/user_<stem>.mst` (the `user_` prefix makes a
/// core-file collision impossible; re-adding the same stem overwrites — the
/// update flow), appends it to `world.list` if absent, re-imports the world
/// into the image (M1 `import_all_lists` — package `user_<stem>`, list
/// `world`), then requests a primary restart: every FRESH world from now on —
/// including File In's — contains these classes, and reseeds keep them (the
/// list entry is on disk). Syntax-gated first: a broken file is refused with
/// the parser's own line/col, nothing written. `OK <summary>` / `ERR <why>`.
extern "C" fn imp_add_to_world(_this: *mut c_void, _cmd: *mut c_void, path: Id) -> Id {
    let src_path = ns_to_string(path);
    let text = match std::fs::read_to_string(&src_path) {
        Ok(t) => t,
        Err(e) => return err(&format!("cannot read {src_path}: {e}")),
    };
    if let Err(e) = macvm::frontend::parser::parse_file(&text) {
        return err(&format!("not added — {e}"));
    }
    let stem: String = std::path::Path::new(&src_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("classes")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    let fname = format!("user_{stem}.mst");
    let world = world_dir();
    if let Err(e) = std::fs::write(world.join(&fname), &text) {
        return err(&format!("cannot write world/{fname}: {e}"));
    }
    let list_path = world.join("world.list");
    let list = std::fs::read_to_string(&list_path).unwrap_or_default();
    if !list.lines().any(|l| l.trim() == fname) {
        let mut list = list;
        if !list.is_empty() && !list.ends_with('\n') {
            list.push('\n');
        }
        list.push_str(&format!("# user-added via the editor's Add to World\n{fname}\n"));
        if let Err(e) = std::fs::write(&list_path, list) {
            return err(&format!("cannot update world.list: {e}"));
        }
    }
    match writer().and_then(|img| image_store::import::import_all_lists(&img, &world)) {
        Ok(stats) => {
            crate::primary_restart::request();
            crate::objc::wake_main_runloop();
            ok(&format!(
                "added world/{fname} ({} class(es), {} method(s) imported) — fresh world restarting",
                stats.classes, stats.methods
            ))
        }
        Err(e) => err(&format!("world import failed: {e}")),
    }
}

/// `requestOpenPanel` / `requestSavePanel` — File ▸ Open… / Save As…: flag a
/// modal panel for `drain_perform` (top-level, VM quiescent — a modal event
/// pump must never spin inside this callback). The drain hands the chosen
/// path back via `CocoaEditor openedPath:` / `savePathChosen:`. Answers nil.
extern "C" fn imp_request_open_panel(_this: *mut c_void, _cmd: *mut c_void) -> Id {
    crate::panels::request_open();
    crate::objc::wake_main_runloop();
    std::ptr::null_mut()
}
extern "C" fn imp_request_save_panel(_this: *mut c_void, _cmd: *mut c_void) -> Id {
    crate::panels::request_save();
    crate::objc::wake_main_runloop();
    std::ptr::null_mut()
}

/// `requestBrowser2Refresh` — flag the V2 browser for a DB re-query and wake
/// the run loop; serviced top-level by `drain_perform` (`CocoaBrowser2
/// doRefresh`), never inside the calling callback — the view_refresh.rs
/// pattern, same as the V1 browser. Answers nil.
extern "C" fn imp_request_browser2_refresh(_this: *mut c_void, _cmd: *mut c_void) -> Id {
    crate::view_refresh::request_browser2();
    crate::objc::wake_main_runloop();
    std::ptr::null_mut()
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
    type Imp2 = extern "C" fn(*mut c_void, *mut c_void, Id, Id) -> Id;
    type Imp3 = extern "C" fn(*mut c_void, *mut c_void, Id, Id, Id) -> Id;
    type Imp4 = extern "C" fn(*mut c_void, *mut c_void, Id, Id, Id, Id) -> Id;
    let methods: [(&str, *const c_void, &str); 37] = [
        ("addToWorldPath:", imp_add_to_world as Imp1 as *const c_void, "@@:@"),
        ("setJitCompile:", imp_set_jit_compile as Imp1 as *const c_void, "@@:@"),
        ("formatSource:", imp_format_source as Imp1 as *const c_void, "@@:@"),
        ("analyzeSource:", imp_analyze_source as Imp1 as *const c_void, "@@:@"),
        ("markErrorStorage:line:", imp_mark_error_storage as Imp2 as *const c_void, "@@:@@"),
        ("fileInPath:", imp_file_in_path as Imp1 as *const c_void, "@@:@"),
        ("requestOpenPanel", imp_request_open_panel as Imp0 as *const c_void, "@@:"),
        ("requestSavePanel", imp_request_save_panel as Imp0 as *const c_void, "@@:"),
        ("requestUiRebuild", imp_request_ui_rebuild as Imp0 as *const c_void, "@@:"),
        ("requestPrimaryRestart", imp_request_primary_restart as Imp0 as *const c_void, "@@:"),
        ("requestBrowserRefresh", imp_request_browser_refresh as Imp0 as *const c_void, "@@:"),
        ("requestBrowser2Refresh", imp_request_browser2_refresh as Imp0 as *const c_void, "@@:"),
        ("packageTree", imp_package_tree as Imp0 as *const c_void, "@@:"),
        ("commentFor:", imp_comment_for as Imp1 as *const c_void, "@@:@"),
        ("setCommentFor:comment:", imp_set_comment_for as Imp2 as *const c_void, "@@:@@"),
        ("requestFindRefresh", imp_request_find_refresh as Imp0 as *const c_void, "@@:"),
        ("requestOutlinerRefresh", imp_request_outliner_refresh as Imp0 as *const c_void, "@@:"),
        ("requestFindQuery", imp_request_find_query as Imp0 as *const c_void, "@@:"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn world_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../world")
    }

    /// M6 (`docs/package_aware_editing_design.md` §4.3): `browse_records_text`
    /// tags each class with a 7th field, `loaded` (1/0), computed from whether
    /// its package is in the primary's booted lists. A base-world class
    /// (`Object`) is `1`; a `cocoaui`-only class (`CocoaHelp`) is `0` when the
    /// primary boots only `["world"]` — the DATABASE tree carries the whole
    /// world, but the flag lets the browser annotate the rows the primary
    /// never loaded. The same class flips to `1` once `cocoaui` is in the
    /// booted set, proving the flag tracks the requested lists, not the class.
    #[test]
    fn browse_records_flags_each_class_loaded_iff_its_package_is_in_the_booted_lists() {
        let image_path = std::env::temp_dir()
            .join(format!("macvm_m6_browse_{}.sqlite3", std::process::id()));
        std::fs::remove_file(&image_path).ok();
        // Seeds every *.list (world + cocoaui), so CocoaHelp is in the image.
        let img = image_store::import::open_or_seed(&world_dir(), &image_path)
            .expect("seed the whole world");

        // field 7 (the loaded flag) of the record whose field 1 == `class`.
        let loaded_field = |text: &str, class: &str| -> String {
            text.lines()
                .find(|line| line.split(SEP).next() == Some(class))
                .unwrap_or_else(|| panic!("no record for {class}"))
                .split(SEP)
                .nth(6)
                .unwrap_or("<missing field 7>")
                .to_string()
        };

        // Primary boots world only: Object loaded, CocoaHelp not.
        let world_only = browse_records_text(&img, &["world"]);
        assert_eq!(loaded_field(&world_only, "Object"), "1", "Object is base-world");
        assert_eq!(
            loaded_field(&world_only, "CocoaHelp"),
            "0",
            "CocoaHelp (a cocoaui class) is NOT loaded on a world-only primary"
        );

        // Add cocoaui to the booted set: the SAME CocoaHelp record flips to 1
        // — the flag tracks the requested lists, not the class.
        let with_cocoaui = browse_records_text(&img, &["world", "cocoaui"]);
        assert_eq!(loaded_field(&with_cocoaui, "CocoaHelp"), "1");
        assert_eq!(loaded_field(&with_cocoaui, "Object"), "1");

        std::fs::remove_file(&image_path).ok();
    }

    /// The V2 browser's `packageTree`: one `list ␟ package ␟ classes` line per
    /// package, classes grouped by their category (= package) and packages by
    /// their list. `Object`'s package `object` is in the `world` list and holds
    /// the `Object` class; a `cocoaui` package (`cocoabrowser`) sits under the
    /// `cocoaui` list — the two-level grouping the category pane renders.
    #[test]
    fn package_tree_groups_classes_by_list_then_package() {
        let image_path =
            std::env::temp_dir().join(format!("macvm_v2_tree_{}.sqlite3", std::process::id()));
        std::fs::remove_file(&image_path).ok();
        let img = image_store::import::open_or_seed(&world_dir(), &image_path).expect("seed");

        let text = package_tree_text(&img);
        let line_for = |pkg: &str| -> String {
            text.lines()
                .find(|l| l.split(SEP).nth(1) == Some(pkg))
                .unwrap_or_else(|| panic!("no package line for {pkg}"))
                .to_string()
        };

        let obj = line_for("object");
        let f: Vec<&str> = obj.split(SEP).collect();
        assert_eq!(f[0], "world", "the `object` package is in the `world` list");
        assert!(
            f[2].split(' ').any(|c| c == "Object"),
            "the Object class groups under its `object` package: {obj}"
        );

        let cb = line_for("cocoabrowser");
        assert_eq!(
            cb.split(SEP).next(),
            Some("cocoaui"),
            "a cocoaui package sits under the cocoaui list"
        );

        std::fs::remove_file(&image_path).ok();
    }
}
