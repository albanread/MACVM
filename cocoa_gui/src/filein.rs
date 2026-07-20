//! File ▸ File In — load a user's `.mst` file of classes into the RUNNING
//! primary VM, top-level.
//!
//! The same flag pattern as game launches (`game::PENDING_LAUNCH`): the host
//! verb (`fileInPath:`, a C6-callback context) only parks the path here; the
//! supervisor's primary pump loop — which runs ON the primary's own thread
//! with the VM between top-level entries — takes it and runs
//! `VmHandle::run_file`, which executes EVERY top-level item in order exactly
//! as `macvm run <file>` does (class definitions compile, doits run). Running
//! it there, never as a nested `uiDoit`, is what keeps a file full of
//! `subclass:` forms clear of the nested-VM-entry crash class.

use std::sync::Mutex;

static PENDING_FILE_IN: Mutex<Option<String>> = Mutex::new(None);

/// Park a path for the next primary pump pass (last request wins — a user
/// clicking File In twice before a beat wants the file once).
pub fn request(path: String) {
    if let Ok(mut slot) = PENDING_FILE_IN.lock() {
        *slot = Some(path);
    }
}

/// Take (and clear) the pending path — called by the supervisor pump.
pub fn take() -> Option<String> {
    PENDING_FILE_IN.lock().ok().and_then(|mut s| s.take())
}

/// Escape arbitrary text for interpolation into a Smalltalk 'string' literal
/// (single quotes doubled, newlines flattened) — used when reporting the
/// file-in outcome through a `Transcript showCr:` exec on the primary.
pub fn escape_st(s: &str) -> String {
    s.replace('\'', "''").replace('\n', " ").replace('\r', " ")
}
