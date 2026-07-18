//! CG9 — UI-worker restart-in-place (design §5 Layers 2 & 3).
//!
//! When the UI worker VM is compromised (an AppKit-internal fault that Layer 1
//! recovered but left the view tree suspect, or an explicit rebuild request),
//! the whole window set is torn down and rebuilt on a FRESH UI worker VM — while
//! the PRIMARY, on its own thread, keeps every object it holds. The channel
//! endpoints (`to_primary`, the hosted inbox) live in Rust (`DrainState`),
//! independent of the VM, so the fresh worker re-adopts the SAME hosted id — no
//! cross-thread re-registration on the live primary is needed.
//!
//! The request is a flag, never an immediate call: a rebuild DROPS the UI
//! worker `VmHandle`, which is unsound from inside a callback executing in that
//! very VM. So a callback (a menu item, a forced-fault handler) only SETS the
//! flag and wakes the run loop; `drain_perform` — which runs on the main thread
//! with the VM quiescent, never inside a callback — performs the rebuild on its
//! next pass. The `VmHandle` drop is proven leak-free across many cycles by
//! `embed::ui_worker_restart_lifecycle_leaks_no_registry_slots`.
//!
//! Layer 3 backstop: N rebuilds within T seconds is a rebuild storm (a fault
//! that reproduces the instant the GUI is back) — write a dossier and
//! `ExitProcess`, the honest end rather than an infinite rebuild loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Set by a callback that wants a full UI rebuild; consumed by `drain_perform`.
static REBUILD_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Layer-3 backstop: rebuild timestamps within the window. `N` rebuilds inside
/// `T` trips `ExitProcess`.
static REBUILD_HISTORY: Mutex<Vec<Instant>> = Mutex::new(Vec::new());
const BACKSTOP_N: usize = 5;
const BACKSTOP_T: Duration = Duration::from_secs(8);

/// Request a UI-worker rebuild (idempotent until serviced). Called from the
/// host-service `requestUiRebuild` IMP and the control channel; the caller must
/// also wake the run loop so `drain_perform` runs promptly.
pub fn request() {
    REBUILD_REQUESTED.store(true, Ordering::Release);
}

/// True (once) if a rebuild was requested since the last check — clears the
/// flag. Called at the top of every drain pass.
pub fn take_request() -> bool {
    REBUILD_REQUESTED.swap(false, Ordering::AcqRel)
}

/// Record this rebuild and, if `N` have happened within `T`, write a dossier
/// and `ExitProcess` (Layer 3). Called at the START of a rebuild so a storm
/// trips before it can recurse.
pub fn note_and_check_backstop() {
    let now = Instant::now();
    let mut hist = REBUILD_HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    hist.retain(|t| now.duration_since(*t) < BACKSTOP_T);
    hist.push(now);
    if hist.len() >= BACKSTOP_N {
        eprintln!(
            "macvm-cocoa: DOSSIER — UI rebuild storm: {} rebuilds within {}s. The UI worker \
             faults again the instant it is rebuilt; this is not recoverable in place. \
             ExitProcess (Layer 3 backstop).",
            hist.len(),
            BACKSTOP_T.as_secs()
        );
        std::process::exit(70);
    }
}
