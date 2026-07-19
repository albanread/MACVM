//! Manual primary-VM restart — a Debug-menu action distinct from CG9's
//! UI-worker rebuild (`rebuild.rs`): this respawns the PERSISTENT primary
//! from source (discarding its whole object-heap state), not the UI shell.
//!
//! `PrimarySupervisor::restart()` already does the real work — it is the
//! exact same respawn-from-source path a fatal doit triggers automatically
//! (CG4 §5.1), and the headless supervisor gate's own deterministic
//! trigger — so this module is only the bridge a C6 callback needs to
//! reach it: `restart()` itself is a channel send, safe from any thread,
//! but the `PrimarySupervisor` instance lives in `DrainState`, which a
//! static `extern "C"` IMP has no access to. So a callback (the Debug menu
//! item) only sets the flag and wakes the run loop; `drain_perform` — which
//! owns `DrainState` and runs on the main thread, never inside a callback —
//! calls `st.supervisor.restart()` on its next pass. The fresh generation's
//! link arrives on the SAME `poll_resync()` a fatal-doit respawn already
//! uses, so no separate re-sync path was needed.

use std::sync::atomic::{AtomicBool, Ordering};

static PRIMARY_RESTART_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Request an immediate primary respawn (idempotent until serviced). Called
/// from the host-service `requestPrimaryRestart` IMP; the caller must also
/// wake the run loop so `drain_perform` runs promptly.
pub fn request() {
    PRIMARY_RESTART_REQUESTED.store(true, Ordering::Release);
}

/// True (once) if a restart was requested since the last check — clears the
/// flag. Called at the top of every drain pass.
pub fn take_request() -> bool {
    PRIMARY_RESTART_REQUESTED.swap(false, Ordering::AcqRel)
}
