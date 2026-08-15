//! THE TIMER SERVICE — the one process service that owns a thread
//! (`docs/process_services.md` §2). A VM asks to be woken every N ms; the
//! service delivers each wake as an ORDINARY ENVELOPE (`{#tick}`) down that
//! VM's inbox, where it dispatches top-level like any other message — one
//! thing at a time, by construction.
//!
//! WHY A THREAD, having built the piggybacked version first: shortening each
//! worker's inbox wait worked, and taught its three defects — only spawned
//! workers could tick (the primary's wait belongs to its host loop), the
//! tick had to interleave with the pulse check by hand, and nothing could
//! CANCEL a timer (the terminated game VM that ticked forever). One wheel on
//! one thread fixes all three: any `InboxSender` can be a target, the worker
//! loop goes back to `recv` + pulse, and every fire checks the target's
//! liveness flag — a ghost cannot tick.
//!
//! CADENCE POLICY, unchanged from v1: the next deadline is measured from the
//! END of the fire (`now + interval`), so a VM that cannot keep up loses
//! rate instead of accumulating a burst debt.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::runtime::workers::{Envelope, InboxSender};

struct Entry {
    /// The registrant's worker handle (0 = a primary). One repeating timer
    /// per VM — re-registration replaces, interval 0 removes: the same
    /// semantics `tickEvery:` always had.
    key: u32,
    interval: Duration,
    next: Instant,
    target: InboxSender,
    /// The target's process-level liveness flag (the worker table's own).
    /// `None` for a primary, whose timer only explicit cancellation or
    /// shutdown ends.
    alive: Option<Arc<AtomicBool>>,
}

struct Service {
    entries: Mutex<Vec<Entry>>,
    cv: Condvar,
    stopped: AtomicBool,
}

static SERVICE: OnceLock<Arc<Service>> = OnceLock::new();

fn service() -> &'static Arc<Service> {
    SERVICE.get_or_init(|| {
        let s = Arc::new(Service {
            entries: Mutex::new(Vec::new()),
            cv: Condvar::new(),
            stopped: AtomicBool::new(false),
        });
        let t = s.clone();
        std::thread::Builder::new()
            .name("macvm-timer-service".into())
            .spawn(move || run(&t))
            .expect("spawn the timer service thread");
        s
    })
}

fn run(s: &Service) {
    let mut entries = s.entries.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if s.stopped.load(Ordering::Acquire) {
            // Parked, not dead: entries are gone, but a later registration
            // re-arms (tests exercise shutdown and then keep living; a real
            // exit never registers again and this just sleeps into the
            // process's end).
            entries.clear();
            let (guard, _) = s
                .cv
                .wait_timeout(entries, Duration::from_secs(3600))
                .unwrap_or_else(|e| e.into_inner());
            entries = guard;
            continue;
        }
        // Sleep to the earliest deadline (or park until somebody registers).
        let now = Instant::now();
        let wait = entries
            .iter()
            .map(|e| e.next.saturating_duration_since(now))
            .min()
            .unwrap_or(Duration::from_secs(3600));
        let (guard, _) = s
            .cv
            .wait_timeout(entries, wait)
            .unwrap_or_else(|e| e.into_inner());
        entries = guard;
        let now = Instant::now();
        entries.retain_mut(|e| {
            if e.next > now {
                return true;
            }
            // Cancel-on-death: a dead target's entry is dropped, never fired.
            if let Some(a) = &e.alive {
                if !a.load(Ordering::Acquire) {
                    return false;
                }
            }
            // Deliver the tick as a message; a closed inbox is a dead target.
            if e
                .target
                .send(Envelope::plain(0, 0, crate::runtime::mop::encode_tick()))
                .is_err()
            {
                return false;
            }
            e.next = Instant::now() + e.interval;
            true
        });
    }
}

/// Ask the service to tick `target` every `ms` milliseconds (0 removes the
/// registration). `key` identifies the registrant so re-registration
/// replaces; `alive` is its process-level liveness flag when it has one.
pub fn set_tick(key: u32, ms: u64, target: InboxSender, alive: Option<Arc<AtomicBool>>) {
    let s = service();
    let mut entries = s.entries.lock().unwrap_or_else(|e| e.into_inner());
    // A registration re-arms a parked service (see `run`'s stopped arm).
    s.stopped.store(false, Ordering::Release);
    entries.retain(|e| e.key != key);
    if ms > 0 {
        entries.push(Entry {
            key,
            interval: Duration::from_millis(ms),
            next: Instant::now() + Duration::from_millis(ms),
            target,
            alive,
        });
    }
    s.cv.notify_all();
}

/// S3's half of the exit sequence: no new ticks, entries cleared, thread
/// parks out. Idempotent.
pub fn stop_all() {
    if let Some(s) = SERVICE.get() {
        s.stopped.store(true, Ordering::Release);
        s.cv.notify_all();
    }
}
