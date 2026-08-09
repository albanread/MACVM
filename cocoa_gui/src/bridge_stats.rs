//! Monitor tab: counters for the COCOA side of the app — the work that is
//! invisible to every VM-side counter. The VMs count allocations, GCs and
//! compiles; nothing counted the main-thread machinery those VMs hang off:
//! drain passes, primary→UI envelopes, callback dispatches (delegates,
//! actions, timers — counted in `macvm::embed`), control-channel requests.
//! One static atomic per seam, bumped where the work happens, formatted here
//! into the single line the Monitor tab's UI BRIDGE band shows.
//!
//! All counters are process-lifetime totals (the Monitor diffs successive
//! reads for rates if it wants them); the beat gap is a main-thread-only
//! EWMA, so the one `Mutex` below is never contended.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;

/// `drain_perform` passes — the run-loop beat every deferred flow rides
/// (~4 Hz idle, much faster while a game pins the fast beat).
static DRAINS: AtomicU64 = AtomicU64::new(0);

/// Envelopes drained from the primary into the UI worker (`#uiReply`,
/// snapshots, transcript lines — the primary→UI traffic).
static ENVELOPES: AtomicU64 = AtomicU64::new(0);

/// Control-channel requests served (`MACVM_COCOA_CTL` — the scripting drive).
static CTL_REQS: AtomicU64 = AtomicU64::new(0);

/// EWMA of the gap between drain passes, in µs (⅛ new, ⅞ old). Written only
/// from `beat()` on the main thread.
static GAP_EWMA_US: AtomicU64 = AtomicU64::new(0);
static LAST_BEAT: Mutex<Option<Instant>> = Mutex::new(None);

/// One drain pass. Called at the top of `drain_perform`.
pub fn beat() {
    DRAINS.fetch_add(1, Relaxed);
    let mut last = LAST_BEAT.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    if let Some(prev) = *last {
        let gap = now.duration_since(prev).as_micros().min(u64::MAX as u128) as u64;
        let old = GAP_EWMA_US.load(Relaxed);
        let next = if old == 0 { gap } else { old - old / 8 + gap / 8 };
        GAP_EWMA_US.store(next, Relaxed);
    }
    *last = Some(now);
}

/// One primary→UI envelope dispatched.
pub fn envelope() {
    ENVELOPES.fetch_add(1, Relaxed);
}

/// One control-channel request served.
pub fn ctl_request() {
    CTL_REQS.fetch_add(1, Relaxed);
}

/// The UI BRIDGE band's whole line — totals plus the live beat gap. The
/// callback count comes from `macvm::embed` (bumped at `dispatch_callback`,
/// the one door every delegate/action/timer entry passes through).
pub fn line() -> String {
    let gap_us = GAP_EWMA_US.load(Relaxed);
    let gap = if gap_us == 0 {
        "—".to_string()
    } else {
        format!("~{:.0} ms", gap_us as f64 / 1000.0)
    };
    format!(
        "drain passes {}  (gap {})   ·   callbacks {}   ·   envelopes {}   ·   script requests {}",
        DRAINS.load(Relaxed),
        gap,
        macvm::embed::callbacks_dispatched(),
        ENVELOPES.load(Relaxed),
        CTL_REQS.load(Relaxed),
    )
}
