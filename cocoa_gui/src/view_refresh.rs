//! View refreshes that must never run inline from a C6 callback — the
//! Browser's class tree and the Find field's autocomplete options (DB
//! queries), and the Outliner's class tree (LIVE `ClassMirror` reflection on
//! the UI worker's own VM) — the SAME flag-and-drain pattern CG9's
//! `rebuild.rs` uses, for the same reason.
//!
//! A callback (a tab's `onShow`, a search) must NEVER run the actual query +
//! Smalltalk tree-build + `NSOutlineView reloadData` / `NSComboBox
//! addItemWithObjectValue:` storm itself: that is a SECOND VM entry nested
//! inside the first (the tab-switch or search callback is already a C6
//! `dispatch_callback` top-level entry), and `reloadData`/`addItemWithObjectValue:`
//! fire AppKit data-source callbacks that are C6 entries too — so the refresh
//! silently fails closed behind the reentrancy guard (the "browser shows no
//! data" symptom) or, once the refresh Smalltalk gets JIT-compiled, corrupts
//! the tier-link invariant `walk_frames` asserts (`ENTRY_FRAME_SENTINEL must
//! pair with IntoInterpreter`) — a hard crash.
//!
//! So a callback only sets a flag + wakes the run loop; `drain_perform`, which
//! runs on the main thread with the VM quiescent and is NEVER itself inside a
//! callback, services it with a fresh top-level `exec` — exactly how CG9
//! rebuilds and CG10 launches demos.

use std::sync::atomic::{AtomicBool, Ordering};

static BROWSER_REFRESH_REQUESTED: AtomicBool = AtomicBool::new(false);
static FIND_REFRESH_REQUESTED: AtomicBool = AtomicBool::new(false);
static OUTLINER_REFRESH_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn request_browser() {
    BROWSER_REFRESH_REQUESTED.store(true, Ordering::Release);
}
pub fn request_find() {
    FIND_REFRESH_REQUESTED.store(true, Ordering::Release);
}
pub fn request_outliner() {
    OUTLINER_REFRESH_REQUESTED.store(true, Ordering::Release);
}

/// Take (and clear) all pending requests — called once per drain pass.
/// Returns `(browser, find, outliner)`.
pub fn take_requests() -> (bool, bool, bool) {
    (
        BROWSER_REFRESH_REQUESTED.swap(false, Ordering::AcqRel),
        FIND_REFRESH_REQUESTED.swap(false, Ordering::AcqRel),
        OUTLINER_REFRESH_REQUESTED.swap(false, Ordering::AcqRel),
    )
}
