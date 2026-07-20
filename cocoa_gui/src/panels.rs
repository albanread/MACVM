//! File ▸ Open… / Save As… — NSOpenPanel/NSSavePanel, run TOP-LEVEL.
//!
//! A modal panel spins its own AppKit event pump, so it must never run inside
//! a C6 callback (the menu item's action) where the UI worker VM is live on
//! the stack: the flag-and-drain law (view_refresh.rs). The menu handler only
//! calls the host verb (`requestOpenPanel`/`requestSavePanel`), which sets a
//! flag + wakes the run loop; `drain_perform` — main thread, VM quiescent —
//! runs the panel and hands the chosen path back to Smalltalk with a fresh
//! top-level exec (`CocoaEditor openedPath:` / `savePathChosen:`).

use std::sync::atomic::{AtomicBool, Ordering};

use crate::objc::{self, Id};

static OPEN_PANEL_REQUESTED: AtomicBool = AtomicBool::new(false);
static SAVE_PANEL_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn request_open() {
    OPEN_PANEL_REQUESTED.store(true, Ordering::Release);
}
pub fn request_save() {
    SAVE_PANEL_REQUESTED.store(true, Ordering::Release);
}

/// Take (and clear) both pending requests — `(open, save)`, one drain pass.
pub fn take_requests() -> (bool, bool) {
    (
        OPEN_PANEL_REQUESTED.swap(false, Ordering::AcqRel),
        SAVE_PANEL_REQUESTED.swap(false, Ordering::AcqRel),
    )
}

/// `NSModalResponseOK`.
const MODAL_OK: i64 = 1;

fn panel_path(panel: Id) -> Option<String> {
    // -[NSSavePanel URL] (NSOpenPanel inherits) → -[NSURL path] → UTF-8.
    let url = objc::send0(panel, objc::sel("URL"));
    if url.is_null() {
        return None;
    }
    let ns_path = objc::send0(url, objc::sel("path"));
    if ns_path.is_null() {
        return None;
    }
    let p = objc::send0(ns_path, objc::sel("UTF8String")) as *const std::os::raw::c_char;
    if p.is_null() {
        return None;
    }
    Some(unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// Run the open panel modally (top-level only); `None` on cancel.
pub fn run_open_panel() -> Option<String> {
    let cls = objc::get_class("NSOpenPanel");
    let panel = objc::send0(cls as Id, objc::sel("openPanel")); // autoreleased
    if panel.is_null() {
        return None;
    }
    if objc::send0_int(panel, objc::sel("runModal")) != MODAL_OK {
        return None;
    }
    panel_path(panel)
}

/// Run the save panel modally (top-level only); `None` on cancel.
pub fn run_save_panel() -> Option<String> {
    let cls = objc::get_class("NSSavePanel");
    let panel = objc::send0(cls as Id, objc::sel("savePanel")); // autoreleased
    if panel.is_null() {
        return None;
    }
    if objc::send0_int(panel, objc::sel("runModal")) != MODAL_OK {
        return None;
    }
    panel_path(panel)
}
