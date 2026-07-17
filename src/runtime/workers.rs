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

    /// A sender with no wake hook — a *spawned* worker's inbound link (it
    /// sleeps in `recv()` inside [`worker_main`], so a wake would be dead
    /// weight; the coalesced flag is set-once-and-ignored, `send` behaves as a
    /// bare channel push). The hosted-worker path builds its `InboxSender`
    /// inline with a real wake instead ([`register_hosted_worker`]).
    fn detached(tx: Sender<Envelope>) -> InboxSender {
        InboxSender {
            tx,
            wake_pending: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(Mutex::new(None)),
        }
    }
}

/// The primary's handle onto one leaf VM (a *spawned* worker OR an
/// *externally-hosted* one — the UI worker, `cocoa_gui_design.md` §3): the
/// outbound inbox and liveness. The `JoinHandle` is deliberately NOT kept
/// (detached; S21). The inbound side is an [`InboxSender`], not a bare
/// `Sender`, so a hosted worker's link can carry a run-loop-poke wake
/// ([`register_hosted_worker`]); a spawned worker's is [`InboxSender::detached`]
/// (no hook — it wakes by returning from `recv()`), so `send` for it is the
/// old bare push plus one uncontended `AtomicBool::swap` — behaviorally
/// identical (the `None` hook can never suppress the recv wake).
pub struct WorkerLink {
    inbox: InboxSender,
    alive: bool,
}

/// The receiving side of an *externally-hosted* worker's inbound inbox
/// ([`register_hosted_worker`]): the channel the host thread drains and the
/// coalesced-wake flag it must clear at the start of each drain — the §3.1
/// eventfd discipline, exactly as [`WorkerState::poll`] does for the primary's
/// own inbox. The host thread owns this; it is NOT the VM (the host also owns
/// a `Worker`-role [`crate::embed::VmHandle`] it stages drained envelopes into,
/// then execs `Worker dispatchPending.`). Handed out INSTEAD of a
/// `thread::spawn`, so the caller — main, blocked in `[NSApp run]`, or a test
/// thread parked on a condvar — drives its own drain loop.
pub struct HostedInbox {
    rx: Receiver<Envelope>,
    wake_pending: Arc<AtomicBool>,
}

impl HostedInbox {
    /// Clear the coalesced-wake flag, then take the next envelope if any — one
    /// drain step, mirroring [`WorkerState::poll`]. Clearing *first* is the
    /// no-lost-wakeup rule: a `send` racing in after the clear re-sets the flag
    /// and re-fires the wake, costing at most one harmless extra drain. The
    /// host loops `while let Some(env) = inbox.poll()` to drain the burst, then
    /// parks on its own wake until the hook fires again.
    pub fn poll(&self) -> Option<Envelope> {
        self.wake_pending.store(false, Ordering::Release);
        self.rx.try_recv().ok()
    }
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

    /// CONTRACT (C4 review): the wake hook runs on whatever thread sends
    /// an envelope — worker threads AND the Cocoa fire IMP (any thread,
    /// possibly main, holding the bridge's action-registry read lock). It
    /// must NOT block and must NOT re-enter the bridge/VM; enqueue-and-
    /// return only (the GUI's is an unbounded-channel send + async wake).
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

/// A worker's transcript, forwarded (M2): everything the worker writes to
/// its `vm.out` — `Transcript show:`, error traces — becomes a
/// `{#workerTranscript. id. text}` control envelope through the same inbox
/// as every data message; the primary's dispatch shows it, `[w<id>]`-tagged,
/// on ITS transcript. A worker never owns a console of its own.
struct ForwardTranscript {
    id: u32,
    to_primary: InboxSender,
    /// Are we at the start of an output line? `vm.out` writes arrive as many
    /// small fragments (an error trace is dozens of `write!` pieces); tagging
    /// every fragment would shred the output with `[w1]`s. Instead each
    /// fragment forwards immediately (nothing is ever held back — an
    /// unterminated `Transcript show:` still arrives at once) and the tag is
    /// inserted only at line starts.
    at_line_start: bool,
}

impl crate::embed::TranscriptSink for ForwardTranscript {
    fn show(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let tag = format!("[w{}] ", self.id);
        let mut out = String::with_capacity(text.len() + tag.len());
        for piece in text.split_inclusive('\n') {
            if self.at_line_start {
                out.push_str(&tag);
            }
            out.push_str(piece);
            self.at_line_start = piece.ends_with('\n');
        }
        let _ = self.to_primary.send(Envelope {
            from: self.id,
            corr: 0,
            bytes: crate::runtime::mop::encode_worker_transcript(i64::from(self.id), &out),
        });
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
    links.push(WorkerLink {
        inbox: InboxSender::detached(tx),
        alive: true,
    });
    Some(id)
}

/// Register an *externally-hosted* worker on an EXISTING thread — no
/// `thread::spawn` (`cocoa_gui_design.md` §3 step 3, §9.1 item 3). The UI
/// worker's thread is `main`, already alive and blocked in `[NSApp run]`, so
/// its VM cannot be born inside a spawned `worker_main`. This mints the same
/// registry entry `spawn` does — a normal-numbered [`WorkerLink`] so `send`
/// (prim 221), `alive` (225) and `terminate` (224) target it with no
/// special-casing, and `MAX_WORKERS` counts it — but hands the CALLER the
/// receiving side + boot payload instead of driving a recv loop itself:
///
/// * `id` — the worker id, `links.len()+1`, sharing the spawned id-space (a
///   hosted and a spawned worker can never collide; both are positions in the
///   one `links` Vec).
/// * [`HostedInbox`] — the channel the host drains + the coalesced-wake flag.
/// * [`InboxSender`] — a clone of the primary's own inbox, for the caller to
///   pass to `VmHandle::install_worker_role` so the hosted VM's `reply:`
///   reaches the primary (the `to_primary` a spawned `worker_main` gets).
///
/// `wake` is the caller-supplied run-loop poke, fired (coalesced) whenever the
/// primary `send`s this worker — in the Cocoa GUI a `performSelectorOnMainThread`
/// (CG2), in the CG1 gate an ordinary condvar/flag poke. Same non-blocking,
/// no-reentry contract as [`WorkerState::set_wake`]. `None` if this VM is not a
/// primary (a worker cannot register peers — v1 star topology) or at the cap.
pub fn register_hosted_worker(
    vm: &mut VmState,
    wake: InboxWakeFn,
) -> Option<(u32, HostedInbox, InboxSender)> {
    let ws = vm.workers.as_mut()?;
    let WorkerState::Primary {
        links, inbox_tx, ..
    } = &mut **ws
    else {
        return None; // only the primary registers peers (v1 star topology)
    };
    if links.len() >= MAX_WORKERS {
        return None;
    }
    let (tx, rx) = channel::<Envelope>();
    let id = links.len() as u32 + 1;
    let wake_pending = Arc::new(AtomicBool::new(false));
    let inbox = InboxSender {
        tx,
        wake_pending: wake_pending.clone(),
        wake: Arc::new(Mutex::new(Some(wake))),
    };
    let to_primary = inbox_tx.clone();
    links.push(WorkerLink { inbox, alive: true });
    Some((id, HostedInbox { rx, wake_pending }, to_primary))
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
    // From here on, everything the worker prints (Transcript, error traces)
    // reaches the primary's transcript instead of a stray stdout (M2).
    handle.set_transcript(Box::new(ForwardTranscript {
        id,
        to_primary: to_primary.clone(),
        at_line_start: true,
    }));
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
            // `InboxSender::send` enqueues then fires the (coalesced) wake —
            // a no-op for a spawned worker (detached hook), a run-loop poke
            // for a hosted one. Its `Err` is the same dead-receiver signal the
            // old bare `tx.send` gave.
            if link.inbox.send(env).is_err() {
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
    link.inbox = InboxSender::detached(dead_tx);
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

/// The PRIMARY's own inbox sender, cloned for the Cocoa bridge (C4): a
/// `MacvmAction` fire on the main thread posts its `{#cocoaEvent. ticket}`
/// envelope here — the same transport, delivery, and coalesced wake worker
/// messages use, unmodified (design §6). `None` in a worker VM (its inbox
/// is the router-fed staging slot, not a channel — Cocoa UI belongs to the
/// primary) or when no worker role exists.
pub fn primary_inbox_sender(vm: &crate::runtime::vm_state::VmState) -> Option<InboxSender> {
    match vm.workers.as_deref() {
        Some(WorkerState::Primary { inbox_tx, .. }) => Some(inbox_tx.clone()),
        _ => None,
    }
}

/// THIS VM's own outbound inbox sender, whatever its role — the sender a Cocoa
/// trampoline minted here should post its `{#cocoaEvent. ticket}` envelope to.
/// A **Primary** answers its own inbox (`inbox_tx`, byte-identical to
/// [`primary_inbox_sender`], so C4/CocoaPad are unchanged); a **Worker** — the
/// Cocoa GUI's UI worker (`cocoa_gui_design.md` §4.3) — answers its `to_primary`
/// link, lifting C4's primary-only refusal (review item 5) so a Worker VM can
/// mint an action at all. `None` only when no worker role exists (a bare CLI VM
/// with no Cocoa callbacks). NB (CG3 scope): the *synchronous* C6 delegate path
/// posts nothing and so needs no sender — it dispatches straight through the
/// callback door; this helper is only for the C4 fire-and-forget action path.
pub fn self_inbox_sender(vm: &crate::runtime::vm_state::VmState) -> Option<InboxSender> {
    match vm.workers.as_deref() {
        Some(WorkerState::Primary { inbox_tx, .. }) => Some(inbox_tx.clone()),
        Some(WorkerState::Worker { to_primary, .. }) => Some(to_primary.clone()),
        None => None,
    }
}
