//! DBG4 — the Cocoa bridge for the VM's GUI debugger frontend
//! (docs/gui_debugger_design.md). The PRIMARY's halt loop publishes its FULL
//! report here (blast, don't patch) and blocks on the command channel; the
//! host verbs feed it. Flag-and-drain throughout: `publish` fires the UI
//! wake itself (the pump that normally beats the wake is PARKED inside the
//! halt), and `drain_perform` services the flag top-level with
//! `CocoaDebugger haltArrived.` — never inside a callback.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;

use macvm::embed::VmHandle;
use macvm::runtime::debug::DebugFrontend;
use macvm::runtime::workers::InboxWakeFn;

/// The latest full report (`RUNNING\n` when not halted).
static REPORT: Mutex<String> = Mutex::new(String::new());
/// A fresh report arrived — drain execs `CocoaDebugger haltArrived.`.
static HALT_ARRIVED: AtomicBool = AtomicBool::new(false);
/// The live command sender (replaced per primary generation; a stale sender
/// to a dead generation's channel errors harmlessly).
static CMD_TX: Mutex<Option<Sender<String>>> = Mutex::new(None);
/// Debug ▸ Halt on Error (default ON; synced onto the primary each beat).
pub static HALT_ON_ERROR: AtomicBool = AtomicBool::new(true);

struct GuiFrontend {
    rx: Mutex<Receiver<String>>,
    wake: InboxWakeFn,
}

impl DebugFrontend for GuiFrontend {
    fn publish(&self, report: &str) {
        if let Ok(mut cell) = REPORT.lock() {
            report.clone_into(&mut cell);
        }
        HALT_ARRIVED.store(true, Ordering::Release);
        (self.wake)();
    }
    fn next_command(&self) -> String {
        // A dead sender resumes rather than hangs (belt and braces — the
        // sender is replaced only when a NEW generation installs, and this
        // generation can't be replaced while parked in its own halt).
        self.rx
            .lock()
            .ok()
            .and_then(|rx| rx.recv().ok())
            .unwrap_or_else(|| "continue".into())
    }
}

/// Install the frontend on a fresh PRIMARY generation (supervisor setup,
/// re-run on every respawn like the game sink).
pub fn install(primary: &mut VmHandle, ui_wake: InboxWakeFn) {
    let (tx, rx) = channel();
    if let Ok(mut slot) = CMD_TX.lock() {
        *slot = Some(tx);
    }
    if let Ok(mut cell) = REPORT.lock() {
        "RUNNING\n".clone_into(&mut cell);
    }
    primary.set_debug_frontend(std::sync::Arc::new(GuiFrontend {
        rx: Mutex::new(rx),
        wake: ui_wake,
    }));
    primary.set_halt_on_error(HALT_ON_ERROR.load(Ordering::Acquire));
    // The old generation's halt state (if any) is gone with it.
    HALT_ARRIVED.store(true, Ordering::Release);
}

/// `drain_perform`: a fresh report to render?
pub fn take_halt_arrived() -> bool {
    HALT_ARRIVED.swap(false, Ordering::AcqRel)
}

/// Host verb `dbgReport` — the current full report text.
pub fn report() -> String {
    REPORT.lock().map(|r| r.clone()).unwrap_or_default()
}

/// Host verb `dbgCommand:` — one command line into the parked halt loop.
pub fn send_command(line: String) {
    if let Ok(slot) = CMD_TX.lock() {
        if let Some(tx) = slot.as_ref() {
            let _ = tx.send(line);
        }
    }
}

/// Pending "Break on entry" / "Clear break" requests — `(class, selector,
/// set?)`. Planting a breakpoint mutates the PRIMARY's VmState (pins the
/// method to tier-0), so it must run on the primary's own thread; the host
/// verb parks the request here and the supervisor pump applies it (the
/// filein flag pattern).
static PENDING_BREAKPOINTS: Mutex<Vec<(String, String, bool)>> = Mutex::new(Vec::new());

/// Park a breakpoint request (`set` true = plant, false = clear).
pub fn request_breakpoint(class: String, selector: String, set: bool) {
    if let Ok(mut q) = PENDING_BREAKPOINTS.lock() {
        q.push((class, selector, set));
    }
}

/// Drain the pending requests — called by the supervisor pump each beat.
pub fn take_breakpoints() -> Vec<(String, String, bool)> {
    PENDING_BREAKPOINTS
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default()
}
