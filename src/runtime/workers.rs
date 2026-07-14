//! Multi-Smalltalk workers, M1 — the primary/worker registry and channels
//! (docs/multi-smalltalk-worker.md §3, §5).
//!
//! A worker VM is one OS thread owning a fresh [`crate::embed::VmHandle`] +
//! the receiving end of its inbound channel + a clone of the primary's inbox
//! sender. **Bytes only** ever cross a thread boundary ([`Envelope`] carries
//! a MOP pickle, `runtime::mop`): no oop is visible to two VMs, so the GCs
//! never coordinate.
//!
//! The event router (§3.1) is not a component: it is the inbox channel plus
//! a registered wake hook, and *the send itself is the wake* —
//! [`InboxSender::send`] fires the (coalesced) hook after enqueueing, the
//! shipping `ChannelGameSink` send-then-notify pattern. The coalescing flag
//! clears at the start of a drain ([`WorkerState::poll`]); a send racing in
//! after the clear sets it again and costs at most one harmless extra
//! dispatch — the classic eventfd discipline, never a lost wakeup.
//!
//! Threads are DETACHED, never `.join()`ed — the S21 rule: a worker that
//! died via `pthread_exit` (guest fatal) panics/hangs `join()`. Death is a
//! *message*: the worker's thread body (or the primary's failed send)
//! synthesizes a `#workerDied` envelope through the same inbox as everything
//! else — one delivery mechanism, including for failure (§8).

use crate::runtime::vm_state::VmState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Star-topology cap (§5 prim 220): far above any sane core count, far
/// below a runaway spawn loop.
pub const MAX_WORKERS: usize = 16;

/// One message crossing a VM boundary. `from` is a worker id (0 = the
/// primary); `corr` is the sender-assigned correlation id that routes a
/// reply to its `send:onReply:` continuation (0 = uncorrelated); `bytes` is
/// a MOP pickle (`runtime::mop`).
pub struct Envelope {
    pub from: u32,
    pub corr: u64,
    pub bytes: Vec<u8>,
}

/// How the embedder boots a worker's world — registered on the PRIMARY via
/// [`crate::embed::VmHandle::set_worker_boot`] (the `GameSink` pattern): the
/// CLI/tests pass a `VmHandle::boot(opts, world_dir)` closure, the GUI its
/// image-boot path, so a worker's world matches the primary's. Runs ON the
/// new worker thread.
pub type WorkerBootFn =
    Arc<dyn Fn() -> Result<crate::embed::VmHandle, crate::runtime::VmError> + Send + Sync>;

/// The wake hook the router fires when an envelope lands in a sleeping
/// primary's inbox (§3.1) — in the GUI, a `performSelectorOnMainThread`
/// poke; headless, unset (the run loop sleeps in the channel itself).
pub type InboxWakeFn = Arc<dyn Fn() + Send + Sync>;

/// The cloneable sending half of the primary's inbox: channel + coalesced
/// wake. Every worker thread holds one; the primary holds one for
/// synthesizing control envelopes (e.g. `#workerDied` on a failed send).
#[derive(Clone)]
pub struct InboxSender {
    tx: Sender<Envelope>,
    wake_pending: Arc<AtomicBool>,
    wake: Arc<Mutex<Option<InboxWakeFn>>>,
}

/// The primary's inbox receiver is gone — its whole process is exiting;
/// the sending thread just winds down.
#[derive(Debug)]
pub struct InboxClosed;

impl InboxSender {
    /// Enqueue + (coalesced) wake. An `Err` means the primary is gone —
    /// its whole process is exiting; the caller's thread just winds down.
    pub fn send(&self, env: Envelope) -> Result<(), InboxClosed> {
        self.tx.send(env).map_err(|_| InboxClosed)?;
        if !self.wake_pending.swap(true, Ordering::AcqRel) {
            let hook = self.wake.lock().unwrap().clone();
            if let Some(w) = hook {
                w();
            }
        }
        Ok(())
    }
}

/// The primary's handle onto one spawned worker: the outbound channel and
/// liveness. The `JoinHandle` is deliberately NOT kept (detached; S21).
pub struct WorkerLink {
    tx: Sender<Envelope>,
    alive: bool,
}

/// Per-VM worker state, hung off [`VmState::workers`] — `Primary` on the VM
/// that spawns, `Worker` inside each spawned VM.
pub enum WorkerState {
    Primary {
        links: Vec<WorkerLink>,
        inbox_rx: Receiver<Envelope>,
        /// For cloning to new workers and for synthesizing control
        /// envelopes into our own inbox.
        inbox_tx: InboxSender,
        boot: WorkerBootFn,
    },
    Worker {
        self_id: u32,
        /// The staging slot (the `GameStep` pattern): the host loop parks
        /// the inbound envelope here, then execs `Worker dispatchPending.`,
        /// whose `primPoll` takes it. Rust bytes — invisible to GC.
        pending: Option<Envelope>,
        to_primary: InboxSender,
    },
}

impl WorkerState {
    pub fn new_primary(boot: WorkerBootFn) -> WorkerState {
        let (tx, rx) = channel::<Envelope>();
        WorkerState::Primary {
            links: Vec::new(),
            inbox_rx: rx,
            inbox_tx: InboxSender {
                tx,
                wake_pending: Arc::new(AtomicBool::new(false)),
                wake: Arc::new(Mutex::new(None)),
            },
            boot,
        }
    }

    pub fn new_worker(self_id: u32, to_primary: InboxSender) -> WorkerState {
        WorkerState::Worker {
            self_id,
            pending: None,
            to_primary,
        }
    }

    pub fn set_wake(&self, f: InboxWakeFn) {
        if let WorkerState::Primary { inbox_tx, .. } = self {
            *inbox_tx.wake.lock().unwrap() = Some(f);
        }
    }

    pub fn self_id(&self) -> u32 {
        match self {
            WorkerState::Primary { .. } => 0,
            WorkerState::Worker { self_id, .. } => *self_id,
        }
    }

    /// Non-blocking next envelope for THIS vm. Primary: drains the shared
    /// inbox (clearing the wake flag first — §3.1 coalescing). Worker: takes
    /// the staged pending message.
    pub fn poll(&mut self) -> Option<Envelope> {
        match self {
            WorkerState::Primary {
                inbox_rx, inbox_tx, ..
            } => {
                inbox_tx.wake_pending.store(false, Ordering::Release);
                inbox_rx.try_recv().ok()
            }
            WorkerState::Worker { pending, .. } => pending.take(),
        }
    }

    /// The headless run loop's sleep (§5 prim 223): block in the inbox up
    /// to `ms`. The channel send IS the wake — zero spin. Primary only.
    pub fn await_inbox(&mut self, ms: u64) -> Option<Envelope> {
        match self {
            WorkerState::Primary {
                inbox_rx, inbox_tx, ..
            } => {
                inbox_tx.wake_pending.store(false, Ordering::Release);
                inbox_rx.recv_timeout(Duration::from_millis(ms)).ok()
            }
            WorkerState::Worker { .. } => None,
        }
    }
}

/// A `#workerDied` control envelope (§8): death is delivered through the
/// same inbox as every ordinary message — one mechanism, no special cases.
fn died_envelope(id: u32) -> Envelope {
    Envelope {
        from: id,
        corr: 0,
        bytes: crate::runtime::mop::encode_worker_died(i64::from(id)),
    }
}

/// Spawn a worker VM (prim 220). Fails (None) with no registered primary
/// role/boot fn, or at the cap. The `init` doit (if any) runs once in the
/// fresh worker before its dispatch loop — how a worker gets its
/// `Worker onMessage:` handler installed.
pub fn spawn(vm: &mut VmState, init: Option<String>) -> Option<u32> {
    let ws = vm.workers.as_mut()?;
    let WorkerState::Primary {
        links,
        inbox_tx,
        boot,
        ..
    } = &mut **ws
    else {
        return None; // workers don't spawn workers (v1 star topology)
    };
    if links.len() >= MAX_WORKERS {
        return None;
    }
    let (tx, rx) = channel::<Envelope>();
    let id = links.len() as u32 + 1;
    let boot = boot.clone();
    let to_primary = inbox_tx.clone();
    // Detached on purpose (S21: never join a VM worker thread).
    std::thread::spawn(move || worker_main(id, &boot, &rx, &to_primary, init.as_deref()));
    links.push(WorkerLink { tx, alive: true });
    Some(id)
}

/// The worker thread body: boot (via the registered closure), take on the
/// Worker role, run the optional init doit, then serve — one envelope, one
/// `Worker dispatchPending.` doit, strictly serial, sleeping in `recv()`
/// between messages. Any failure ends in a `#workerDied` envelope; a closed
/// channel (terminate/primary exit) ends in a silent clean unwind.
fn worker_main(
    id: u32,
    boot: &WorkerBootFn,
    rx: &Receiver<Envelope>,
    to_primary: &InboxSender,
    init: Option<&str>,
) {
    let Ok(mut handle) = boot() else {
        let _ = to_primary.send(died_envelope(id));
        return;
    };
    handle.install_worker_role(id, to_primary.clone());
    if let Some(src) = init {
        if handle.exec(src).is_err() {
            let _ = to_primary.send(died_envelope(id));
            return;
        }
    }
    while let Ok(env) = rx.recv() {
        handle.stage_pending(env);
        // A guest error mid-dispatch (error:, DNU, even a native fault —
        // S21's recovery surfaces all of them as Err) retires this worker:
        // its state is suspect, so report death and unwind. The VmHandle
        // drops normally (heap unmapped) — pthread_exit is only for the
        // truly unrecoverable path inside the fatal machinery itself.
        if handle.exec("Worker dispatchPending.").is_err() {
            let _ = to_primary.send(died_envelope(id));
            return;
        }
    }
}

/// Send bytes (prim 221). From the primary: to worker `id` (marking it dead
/// and synthesizing `#workerDied` into our own inbox if its channel is
/// gone). From a worker: `id` must be 0 — the reply path to the primary.
pub fn send(vm: &mut VmState, id: u32, corr: u64, bytes: Vec<u8>) -> bool {
    let Some(ws) = vm.workers.as_mut() else {
        return false;
    };
    match &mut **ws {
        WorkerState::Primary {
            links, inbox_tx, ..
        } => {
            if id == 0 {
                return false;
            }
            let Some(link) = links.get_mut(id as usize - 1) else {
                return false;
            };
            if !link.alive {
                return false;
            }
            let env = Envelope {
                from: 0,
                corr,
                bytes,
            };
            if link.tx.send(env).is_err() {
                // The worker's receiver is gone — it died between messages.
                // Mark it and deliver the death notice through the inbox.
                link.alive = false;
                let _ = inbox_tx.send(died_envelope(id));
                return false;
            }
            true
        }
        WorkerState::Worker {
            self_id,
            to_primary,
            ..
        } => {
            if id != 0 {
                return false; // v1: workers talk only to the primary
            }
            to_primary
                .send(Envelope {
                    from: *self_id,
                    corr,
                    bytes,
                })
                .is_ok()
        }
    }
}

/// Terminate worker `id` (prim 224): drop its channel — its thread exits on
/// the next `recv()` — and mark it dead. Idempotent.
pub fn terminate(vm: &mut VmState, id: u32) -> bool {
    let Some(ws) = vm.workers.as_mut() else {
        return false;
    };
    let WorkerState::Primary { links, .. } = &mut **ws else {
        return false;
    };
    let Some(link) = links.get_mut(id as usize - 1) else {
        return false;
    };
    link.alive = false;
    // Replace the sender with a dead-ended one so the worker's receiver
    // disconnects (there is no Option-dance: a fresh channel's tx dropped
    // immediately leaves our field valid but the old channel closed).
    let (dead_tx, _) = channel::<Envelope>();
    link.tx = dead_tx;
    true
}

/// Is worker `id` believed alive (prim 225)? False once death is DETECTED
/// (failed send / terminate) — not instantly at crash (§5).
pub fn alive(vm: &VmState, id: u32) -> bool {
    let Some(ws) = vm.workers.as_ref() else {
        return false;
    };
    let WorkerState::Primary { links, .. } = &**ws else {
        return false;
    };
    links.get(id as usize - 1).map(|l| l.alive).unwrap_or(false)
}
