//! S3's exit sequence, GUI side (`docs/process_services.md` §4). ⌘Q used to
//! kill detached VM threads mid-instruction; now `applicationWillTerminate:`
//! REQUESTS the sequence and the primary's supervisor beat — the thread that
//! owns the primary — runs it: timers stopped, every live worker poisoned,
//! a bounded wait on the process-level liveness flags, then exit regardless.
//! Never a join (S21); the OS still reaps whatever ignored the ask.

use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);
static ACKED: AtomicBool = AtomicBool::new(false);

/// Called on the main thread by `applicationWillTerminate:`. Stops the
/// timers immediately (that half needs no VM), flags the supervisor, and
/// waits — bounded — for the acknowledgement or the flags themselves.
pub fn run_from_will_terminate() {
    macvm::runtime::timer_service::stop_all();
    REQUESTED.store(true, Ordering::Release);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(400);
    while std::time::Instant::now() < deadline {
        if ACKED.load(Ordering::Acquire) || macvm::runtime::workers::all_workers_dead() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // Timed out: the supervisor is mid-doit or a worker ignored the poison.
    // Exit proceeds; that is the sequence's own contract.
}

pub fn requested() -> bool {
    REQUESTED.load(Ordering::Acquire)
}

pub fn acknowledge() {
    ACKED.store(true, Ordering::Release);
}
