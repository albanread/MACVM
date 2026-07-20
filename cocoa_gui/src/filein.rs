//! File ▸ File In — the contract: **a fresh world, then your file**. Each
//! File In RESTARTS the primary from its clean boot state and loads the
//! user's `.mst` on top — exactly `macvm run <file> --world world`, inside
//! the GUI. No persistence between file-ins: filing the same file N times
//! always works (no "cannot change shape of existing class" — the class
//! never pre-exists), and every run starts from the identical, known state.
//!
//! Sequencing (a BIRTH STAMP, race-free by construction): the host verb
//! (`fileInPath:`, a C6-callback context) parks the path tagged with a
//! monotonically increasing request sequence, then requests a primary
//! restart. Every generation's pump records the sequence counter ONCE at
//! its own start (`birth_stamp`) and only takes a request whose sequence is
//! ≤ its birth — i.e. a request made BEFORE this generation existed. The
//! dying old generation was born before the request, so it can never grab
//! the file (the first cut promoted at `poll_resync`, and the old pump's
//! final beats raced the promotion — the file loaded into the dying world
//! and vanished with it, observed live). The fresh generation's pump — the
//! primary's own thread, between top-level entries — takes it and runs
//! `VmHandle::run_file` (every top-level item in order; class definitions
//! compile, doits run). Running it on the pump, never as a nested `uiDoit`,
//! keeps a file full of `subclass:` forms clear of the nested-VM-entry
//! crash class.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Bumped on every file-in request; a generation's `birth_stamp` is the
/// value at its pump's start.
static REQ_SEQ: AtomicU64 = AtomicU64::new(0);

/// `(path, request sequence)` — last request wins (a user clicking File In
/// twice wants the file once).
static PENDING_FILE_IN: Mutex<Option<(String, u64)>> = Mutex::new(None);

/// Park a path to be filed in by the NEXT primary generation (the caller
/// also requests the restart that creates it).
pub fn request_after_restart(path: String) {
    let seq = REQ_SEQ.fetch_add(1, Ordering::AcqRel) + 1;
    eprintln!("macvm-cocoa: file-in parked (awaiting fresh world): {path}");
    if let Ok(mut slot) = PENDING_FILE_IN.lock() {
        *slot = Some((path, seq));
    }
}

/// The sequence counter at THIS generation's start — read once by the pump.
pub fn birth_stamp() -> u64 {
    REQ_SEQ.load(Ordering::Acquire)
}

/// Take the pending path IFF it was requested before this generation was
/// born (`seq <= birth`) — the old generation (born earlier than the
/// request) never matches; only the fresh world does.
pub fn take_for(birth: u64) -> Option<String> {
    let mut slot = PENDING_FILE_IN.lock().ok()?;
    match &*slot {
        Some((_, seq)) if *seq <= birth => slot.take().map(|(p, _)| p),
        _ => None,
    }
}

/// Escape arbitrary text for interpolation into a Smalltalk 'string' literal
/// (single quotes doubled, newlines flattened) — used when reporting the
/// file-in outcome through a `Transcript showCr:` exec on the primary.
pub fn escape_st(s: &str) -> String {
    s.replace('\'', "''").replace('\n', " ").replace('\r', " ")
}
