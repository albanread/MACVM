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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Star-topology cap (§5 prim 220): far above any sane core count, far
/// below a runaway spawn loop.
pub const MAX_WORKERS: usize = 16;

// ─────────────── the global worker table + primary epochs ───────────────
// Supervision of LAST RESORT lives ABOVE all VMs, in plain process-global
// Rust — because any monitor that lives inside a VM inherits VM mortality.
// The primary looks like the natural warden of its workers (it owns the
// links), but a fatal primary death exits by `pthread_exit` with NO Drop
// glue: the links — the only copy of every worker's channel sender — leak,
// and each worker parks on `recv()` forever, heap mapped, thread alive,
// unreachable from the respawned generation. The fix is an INVARIANT, not a
// policy: "your primary epoch has ended, therefore no message can ever
// reach you again, therefore exit." Idle-TTLs, permanence, respawn — all
// policy — stay in the Smalltalk supervisors (74_supervisor.mst); this
// layer only enforces what is unconditionally true.
//
// Mechanism: every Primary WorkerState is minted a process-unique EPOCH id
// (per-primary, NOT a single global counter — parallel tests run many
// primaries in one process, and one primary's death must never reap
// another's workers). Workers remember their parent's epoch and wake from
// `recv_timeout` every couple of seconds to check it; the watchdog's fatal
// hook (and, for every clean path, `Drop for WorkerState`) marks the epoch
// dead. The table itself is the observability half: one row per spawn with
// the shared `alive` flag the worker clears on ANY exit — readable by the
// Monitor's host verbs and asserted on by the embed gates.

/// Mints per-primary epoch ids. Starts at 1 so 0 can never name a real one.
static NEXT_PRIMARY_EPOCH: AtomicU64 = AtomicU64::new(1);

/// Epochs whose primary is GONE (fatal or clean — either way the star's
/// center is dead). A tiny grow-only list: one entry per primary death in
/// the whole process lifetime.
static DEAD_EPOCHS: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// One spawn's row in the global table. `alive` is shared with the worker
/// thread, which clears it on every exit path (retire, crash, reap).
struct GlobalWorkerRow {
    worker_id: u32,
    primary_epoch: u64,
    spawned_at: std::time::Instant,
    alive: Arc<AtomicBool>,
}

/// A read-side copy of one row.
pub struct WorkerTableRow {
    pub worker_id: u32,
    pub primary_epoch: u64,
    pub age: Duration,
    pub alive: bool,
}

static WORKER_TABLE: Mutex<Vec<GlobalWorkerRow>> = Mutex::new(Vec::new());

/// Record that a primary epoch has ended. Idempotent; called from the
/// watchdog's fatal hook (the path where Drop never runs) and from
/// `Drop for WorkerState` (every clean path). Any thread.
pub fn note_primary_dead(epoch: u64) {
    let mut dead = DEAD_EPOCHS.lock().unwrap_or_else(|e| e.into_inner());
    if !dead.contains(&epoch) {
        dead.push(epoch);
    }
}

/// Is this epoch's primary still with us?
pub fn primary_epoch_alive(epoch: u64) -> bool {
    !DEAD_EPOCHS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&epoch)
}

/// The primary epoch of `vm`, if it IS a primary.
pub fn primary_epoch(vm: &VmState) -> Option<u64> {
    match vm.workers.as_deref() {
        Some(WorkerState::Primary { epoch, .. }) => Some(*epoch),
        _ => None,
    }
}

fn worker_table_register(worker_id: u32, primary_epoch: u64, alive: Arc<AtomicBool>) {
    let mut table = WORKER_TABLE.lock().unwrap_or_else(|e| e.into_inner());
    // Bounded history: spawn churn (a game fleet per launch) must not grow
    // the table forever — once it gets long, dead rows have told their story.
    if table.len() >= 64 {
        table.retain(|r| r.alive.load(Ordering::Relaxed));
    }
    table.push(GlobalWorkerRow {
        worker_id,
        primary_epoch,
        spawned_at: std::time::Instant::now(),
        alive,
    });
}

/// Read-side: copy the table (any thread).
pub fn worker_table_snapshot() -> Vec<WorkerTableRow> {
    WORKER_TABLE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|r| WorkerTableRow {
            worker_id: r.worker_id,
            primary_epoch: r.primary_epoch,
            age: r.spawned_at.elapsed(),
            alive: r.alive.load(Ordering::Relaxed),
        })
        .collect()
}

/// How often a parked worker wakes to check its primary's pulse. Reap
/// latency is at most this; the cost is one wakeup per idle worker per
/// interval — noise.
const EPOCH_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// One message crossing a VM boundary. `from` is a worker id (0 = the
/// primary); `corr` is the sender-assigned correlation id that routes a
/// reply to its `send:onReply:` continuation (0 = uncorrelated); `bytes` is
/// a MOP pickle (`runtime::mop`).
pub struct Envelope {
    pub from: u32,
    pub corr: u64,
    pub bytes: Vec<u8>,
    /// The SENDER'S OWN INBOX, when it has one to offer — the whole of
    /// `docs/worker_peer_links.md` in one field. Envelopes cross between VMs
    /// as Rust structs down a channel, never as serialized bytes, so an
    /// envelope can carry a live link; a receiver that has been messaged
    /// therefore already knows how to answer, and a peer link is LEARNED
    /// rather than registered. That removes the registry, the name service
    /// and the round trip a general peer-addressing scheme would need.
    ///
    /// `None` is the ordinary case and means exactly what it did before this
    /// field existed: nothing to learn, reply through the parent.
    pub reply_to: Option<InboxSender>,
}

impl Envelope {
    /// The plain envelope — no link offered. Every pre-peer-links call site
    /// means this, so it is spelled once rather than nine times.
    pub fn plain(from: u32, corr: u64, bytes: Vec<u8>) -> Envelope {
        Envelope {
            from,
            corr,
            bytes,
            reply_to: None,
        }
    }
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
            // Poison-tolerant: the critical section (here and at every other
            // lock of `wake`) only clones/replaces the `Arc`'d hook, so a
            // panic elsewhere while holding it leaves nothing torn — and one
            // poisoned sender must not turn every future cross-VM send into
            // a panic cascade.
            let hook = self
                .wake
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
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
    /// WHICH OCCUPANT OF THIS SLOT — bumped every time the slot is reclaimed
    /// by a new spawn. See the handle helpers below for why this exists.
    gen: u32,
}

// ── worker handles: a slot, and which occupant of it ────────────────────
//
// A worker id as the rest of the system sees it is a HANDLE, not an index:
// the slot in `links` in the low bits, the slot's GENERATION in the high
// ones. So a handle names one particular worker for the life of the process
// and is never handed out twice, while `links` stays an O(1) `Vec` and a
// dead slot is still RECLAIMED — which is load-bearing, because MAX_WORKERS
// caps CONCURRENT workers and reclamation is what fixed the exhaustion bug
// documented in `spawn` (rounds of spawn-then-terminate hitting the cap with
// nothing alive).
//
// It matters because ids key things that outlive the worker they named:
// `send`/`terminate`/`alive` look a handle up in `links`, reply
// continuations are keyed by peer, and a Smalltalk `Worker` holds its id
// across the death of the VM behind it. Without a generation, all of those
// silently address whoever holds the slot NOW; with one, a stale handle is
// refused. Peer links need no such check — a link is a channel endpoint to
// one VM's inbox, so a dead peer's send simply fails.
//
// 16 bits of slot (MAX_WORKERS is 16) and 16 of generation. A slot would
// have to be recycled 65_535 times in one process to wrap, which no workload
// approaches; if one ever did, the wrap reintroduces exactly the ambiguity
// this removes, so it is worth knowing rather than assuming away.

const SLOT_BITS: u32 = 16;
const SLOT_MASK: u32 = (1 << SLOT_BITS) - 1;

/// The handle naming generation `gen` of slot `slot` (1-based, as ids have
/// always been).
fn make_handle(slot: u32, gen: u32) -> u32 {
    (gen << SLOT_BITS) | (slot & SLOT_MASK)
}

/// The `links` index a handle names, or `None` for handle 0 (the parent) and
/// anything out of range.
fn slot_index_of(handle: u32) -> Option<usize> {
    let slot = handle & SLOT_MASK;
    (slot != 0).then(|| slot as usize - 1)
}

fn generation_of(handle: u32) -> u32 {
    handle >> SLOT_BITS
}

/// The SLOT a handle names — what a human means by "worker 3". Anything
/// user-facing (a transcript tag, a Monitor row) should render this rather
/// than the raw handle, which is an internal token and reads as a large
/// meaningless number.
pub fn handle_slot(handle: u32) -> u32 {
    handle & SLOT_MASK
}

/// Which occupant of that slot — 1 for the first, bumped on every reclaim.
pub fn handle_generation(handle: u32) -> u32 {
    generation_of(handle)
}

/// The link a handle names, ONLY if the slot is still on that generation —
/// the one check that turns a stale handle into a refusal instead of a
/// message delivered to a stranger.
fn link_for(links: &[WorkerLink], handle: u32) -> Option<&WorkerLink> {
    let link = links.get(slot_index_of(handle)?)?;
    (link.gen == generation_of(handle)).then_some(link)
}

fn link_for_mut(links: &mut [WorkerLink], handle: u32) -> Option<&mut WorkerLink> {
    let idx = slot_index_of(handle)?;
    let link = links.get_mut(idx)?;
    (link.gen == generation_of(handle)).then_some(link)
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
        /// This primary's process-unique epoch (the global-table section
        /// above): workers born of it exit when it is marked dead.
        epoch: u64,
        /// WHICH LINK IS THE UI, if this embedding has one. The primary is
        /// the only VM that knows everyone, so it is the only one that can
        /// introduce a newly spawned worker to the display and vice versa
        /// (`docs/worker_peer_links.md` §3). `None` in a headless embedding,
        /// where nothing is introduced and workers simply talk to their
        /// parent as they always did.
        ui_peer: Option<u32>,
    },
    Worker {
        self_id: u32,
        /// The staging slot (the `GameStep` pattern): the host loop parks
        /// the inbound envelope here, then execs `Worker dispatchPending.`,
        /// whose `primPoll` takes it. Rust bytes — invisible to GC.
        pending: Option<Envelope>,
        to_primary: InboxSender,
        /// THIS worker's own inbox, when it has one to hand out — what goes
        /// in an outgoing envelope's `reply_to` so a peer can answer directly
        /// (`docs/worker_peer_links.md`). A spawned worker gets one from
        /// `spawn`; an externally-hosted worker may have none, and then it
        /// simply offers no link and behaves exactly as it did before peer
        /// links existed.
        self_inbox: Option<InboxSender>,
        /// A VM-MANAGED FRAME TICK, in milliseconds; 0 is off. The worker
        /// sets it on itself (`Worker tickEvery:`) and the loop below wakes
        /// on it — a worker asleep in its inbox is woken by the VM at a
        /// cadence it asked for, which is what makes a 60 Hz frame loop
        /// possible in a VM that is otherwise purely message-driven. A
        /// SERVICE, not a thread: no extra thread exists, the existing
        /// `recv_timeout` deadline is simply shortened to the next tick.
        tick_ms: u64,
        /// LINKS THIS WORKER HOLDS, not addresses it can resolve — id ->
        /// inbox for the peers it was introduced to at spawn and the ones it
        /// has learned by being messaged. There is no directory and no
        /// lookup service: a worker can reach its parent, whoever it was
        /// introduced to, and whoever has spoken to it. Two or three entries
        /// in practice, which is why a Vec is the right shape.
        peers: Vec<(u32, InboxSender)>,
    },
}

impl Drop for WorkerState {
    fn drop(&mut self) {
        // Every CLEAN end of a primary — VmHandle drop, test teardown, a
        // supervisor generation returning — retires its epoch here. The
        // fatal path (`pthread_exit`, no Drop glue) is covered by the
        // watchdog's fatal hook calling `note_primary_dead` directly.
        if let WorkerState::Primary { epoch, .. } = self {
            note_primary_dead(*epoch);
        }
    }
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
            epoch: NEXT_PRIMARY_EPOCH.fetch_add(1, Ordering::Relaxed),
            ui_peer: None,
        }
    }

    pub fn new_worker(self_id: u32, to_primary: InboxSender) -> WorkerState {
        WorkerState::Worker {
            self_id,
            pending: None,
            to_primary,
            self_inbox: None,
            tick_ms: 0,
            peers: Vec::new(),
        }
    }

    /// Hand this worker its own inbox — the link it offers as `reply_to`.
    /// Separate from `new_worker` because the role is installed by the worker
    /// thread before the spawner's side of the channel is available to it.
    pub fn set_self_inbox(&mut self, inbox: InboxSender) {
        if let WorkerState::Worker { self_inbox, .. } = self {
            *self_inbox = Some(inbox);
        }
    }

    /// Introduce a peer: remember `id`'s inbox so a later `send` can resolve
    /// it. Replaces an existing entry for the same id — a respawned worker
    /// reusing an id must not be reachable through its predecessor's link.
    pub fn add_peer(&mut self, id: u32, inbox: InboxSender) {
        if let WorkerState::Worker { peers, .. } = self {
            if let Some(slot) = peers.iter_mut().find(|(pid, _)| *pid == id) {
                slot.1 = inbox;
            } else {
                peers.push((id, inbox));
            }
        }
    }

    /// LEARN FROM AN ARRIVING ENVELOPE — the rule that makes peer links cost
    /// no registry: whoever just spoke to us handed over the link to answer
    /// them on, so remember it. A `None` reply_to teaches nothing, which is
    /// every pre-peer-links message and is why this is safe to run on all of
    /// them.
    pub fn learn_peer_from(&mut self, env: &Envelope) {
        if let Some(link) = env.reply_to.clone() {
            self.add_peer(env.from, link);
        }
    }

    /// CONTRACT (C4 review): the wake hook runs on whatever thread sends
    /// an envelope — worker threads AND the Cocoa fire IMP (any thread,
    /// possibly main, holding the bridge's action-registry read lock). It
    /// must NOT block and must NOT re-enter the bridge/VM; enqueue-and-
    /// return only (the GUI's is an unbounded-channel send + async wake).
    pub fn set_wake(&self, f: InboxWakeFn) {
        if let WorkerState::Primary { inbox_tx, .. } = self {
            // Poison-tolerant for the same reason as `InboxSender::send`.
            *inbox_tx.wake.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
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
        reply_to: None,
    }
}

/// A VM's transcript, forwarded through the inbox as `{#workerTranscript. id.
/// text}` control envelopes — one delivery mechanism for output too. Two
/// directions use it:
///
/// * **worker → primary (M2):** everything a spawned worker writes to its
///   `vm.out` (`Transcript show:`, error traces) reaches the primary's
///   transcript, `[w<id>]`-tagged (`id` ≥ 1) so the primary can tell workers
///   apart.
/// * **primary → UI worker (Cocoa GUI CG4, `cocoa_gui_design.md` §7.4):** the
///   primary's own transcript is forwarded to the UI worker's inbox, whose
///   `dispatchOne:` appends it to the Transcript view. `id` is 0 here (the
///   primary is the environment's *own* console, not a sub-worker), so it is
///   emitted UNTAGGED — `[w0]` would be noise on the primary's own lines.
pub struct ForwardTranscript {
    id: u32,
    dest: InboxSender,
    /// Are we at the start of an output line? `vm.out` writes arrive as many
    /// small fragments (an error trace is dozens of `write!` pieces); tagging
    /// every fragment would shred the output with `[w1]`s. Instead each
    /// fragment forwards immediately (nothing is ever held back — an
    /// unterminated `Transcript show:` still arrives at once) and the tag is
    /// inserted only at line starts.
    at_line_start: bool,
}

impl ForwardTranscript {
    /// A transcript sink that forwards every write to `dest`'s inbox as a
    /// `{#workerTranscript. id. text}` envelope. `id` ≥ 1 tags each line
    /// `[w<id>]` (a spawned worker); `id == 0` forwards untagged (the primary's
    /// own transcript, CG4 §7.4).
    pub fn to(id: u32, dest: InboxSender) -> ForwardTranscript {
        ForwardTranscript {
            id,
            dest,
            at_line_start: true,
        }
    }
}

impl crate::embed::TranscriptSink for ForwardTranscript {
    fn show(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let out = if self.id == 0 {
            // The primary's own transcript — untagged.
            self.at_line_start = text.ends_with('\n');
            text.to_string()
        } else {
            // THE SLOT, not the handle: `[w1]` is what a reader means by
            // "the first worker"; the handle carries a generation too and
            // renders as a five-digit token that means nothing in a log.
            let tag = format!("[w{}] ", handle_slot(self.id));
            let mut out = String::with_capacity(text.len() + tag.len());
            for piece in text.split_inclusive('\n') {
                if self.at_line_start {
                    out.push_str(&tag);
                }
                out.push_str(piece);
                self.at_line_start = piece.ends_with('\n');
            }
            out
        };
        let _ = self.dest.send(Envelope {
            from: self.id,
            corr: 0,
            // THE TAG SHOWS THE SLOT, not the handle: `[w1]` is what a
            // reader means by "the first worker", where the handle renders as
            // a five-digit token that means nothing to them.
            bytes: crate::runtime::mop::encode_worker_transcript(
                i64::from(handle_slot(self.id)),
                &out,
            ),
            reply_to: None,
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
        epoch,
        ui_peer,
        ..
    } = &mut **ws
    else {
        return None; // workers don't spawn workers (star topology by design)
    };
    let ui_peer = *ui_peer;
    // Reclaim a TERMINATED slot's index before growing the fleet. `terminate`
    // (below) marks a link dead but never shrinks `links` — a dead slot's id
    // must stay stable in case an in-flight reply is still keyed by it — so
    // without this reuse, a demo that spawns N workers per launch and
    // cleanly terminates them every time (GamePane's `onReset:` hook) still
    // permanently consumes N slots per launch and hits MAX_WORKERS after
    // MAX_WORKERS/N launches even though nothing is actually alive: exactly
    // ParallelMandel's "breaks after a little use" (observed live: launch 5
    // of 4-worker rounds hit the cap with zero live workers). `MAX_WORKERS`
    // is documented as "a pool of up to 16 CONCURRENT worker VMs" (README) —
    // a monotonic ever-spawned counter was never the intent.
    let reuse_idx = links.iter().position(|l| !l.alive);
    // The slot, and WHICH OCCUPANT of it: reclaiming bumps the generation, so
    // the handle handed out below has never been handed out before even
    // though the slot has been used.
    let (slot, gen) = match reuse_idx {
        Some(idx) => ((idx + 1) as u32, links[idx].gen.wrapping_add(1)),
        None => {
            if links.len() >= MAX_WORKERS {
                return None;
            }
            (links.len() as u32 + 1, 1)
        }
    };
    let id = make_handle(slot, gen);
    let (tx, rx) = channel::<Envelope>();
    // The worker's OWN inbox, cloned before the primary's link takes the
    // sender: this is what the new VM offers as `reply_to` so a peer it
    // messages can answer it directly (`docs/worker_peer_links.md`). Detached
    // like the primary's link — a spawned worker parks in `rx.recv()`, so
    // there is no run-loop to poke.
    let self_inbox = InboxSender::detached(tx.clone());
    // The display's link, resolved once: the new worker gets a copy on its own
    // thread (below), and the same link carries the introduction that teaches
    // the display about the new worker.
    let ui_link = ui_peer.and_then(|h| link_for(links, h).map(|l| (h, l.inbox.clone())));
    let ui_link_for_worker = ui_link.clone();
    // The new worker's own inbox again, for the introduction below — cloned
    // here because the `WorkerLink` takes ownership of `tx` further down.
    let intro_reply_to = InboxSender::detached(tx.clone());
    let boot = boot.clone();
    let to_primary = inbox_tx.clone();
    // The global-table row (one per SPAWN, not per id — reused ids get fresh
    // rows) and the shared liveness flag the thread clears on any exit.
    let epoch = *epoch;
    let alive = Arc::new(AtomicBool::new(true));
    worker_table_register(id, epoch, alive.clone());
    // Detached on purpose (S21: never join a VM worker thread).
    std::thread::spawn(move || {
        worker_main(
            id,
            epoch,
            alive,
            &boot,
            &rx,
            &to_primary,
            self_inbox,
            ui_link_for_worker,
            init.as_deref(),
        )
    });
    let link = WorkerLink {
        inbox: InboxSender::detached(tx),
        alive: true,
        gen,
    };
    // ── the two introductions (`docs/worker_peer_links.md` §3) ──────────
    //
    // Learning answers "how do I reply", never "how do I start", so the first
    // link in each direction is handed over here — by the primary, because it
    // is the only VM that knows everyone. After this it is out of the path.
    if let Some((_ui_handle, ui_inbox)) = &ui_link {
        // The DISPLAY learns the new worker. An introduction is an ordinary
        // envelope with an empty payload — the drain treats a payload-less
        // envelope as a no-op, so the Smalltalk side sees nothing at all,
        // while the Rust learning rule caches `from` + `reply_to` on its way
        // in. Being an envelope rather than an API is the point: it needs no
        // new mechanism and travels the exact path every later frame will.
        let _ = ui_inbox.send(Envelope {
            from: id,
            corr: 0,
            bytes: Vec::new(),
            reply_to: Some(intro_reply_to),
        });
    }
    match reuse_idx {
        Some(idx) => links[idx] = link,
        None => links.push(link),
    }
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
    // A hosted worker takes a fresh slot (it is registered once, at boot) and
    // so is always generation 1 — but it is HANDLED the same way, because
    // nothing downstream should care which kind of worker a handle names.
    let slot = links.len() as u32 + 1;
    let gen = 1;
    let id = make_handle(slot, gen);
    let wake_pending = Arc::new(AtomicBool::new(false));
    let inbox = InboxSender {
        tx,
        wake_pending: wake_pending.clone(),
        wake: Arc::new(Mutex::new(Some(wake))),
    };
    let to_primary = inbox_tx.clone();
    links.push(WorkerLink {
        inbox,
        alive: true,
        gen,
    });
    Some((id, HostedInbox { rx, wake_pending }, to_primary))
}

/// The worker thread body: boot (via the registered closure), take on the
/// Worker role, run the optional init doit, then serve — one envelope, one
/// `Worker dispatchPending.` doit, strictly serial, sleeping in `recv()`
/// between messages. Any failure ends in a `#workerDied` envelope; a closed
/// channel (terminate/primary exit) ends in a silent clean unwind.
fn worker_main(
    id: u32,
    epoch: u64,
    alive: Arc<AtomicBool>,
    boot: &WorkerBootFn,
    rx: &Receiver<Envelope>,
    to_primary: &InboxSender,
    self_inbox: InboxSender,
    ui_link: Option<(u32, InboxSender)>,
    init: Option<&str>,
) {
    let Ok(mut handle) = boot() else {
        alive.store(false, Ordering::Relaxed);
        let _ = to_primary.send(died_envelope(id));
        return;
    };
    handle.install_worker_role(id, to_primary.clone());
    // Its own link, so anything it messages can answer it directly rather
    // than through the primary (`docs/worker_peer_links.md`).
    handle.set_self_inbox(self_inbox);
    // And the display's link, installed BEFORE the init doit runs so a worker
    // whose very first act is to draw something already has somewhere to send
    // it. Introductions happen on this thread because the peer list lives in
    // this VM, which the spawner cannot touch.
    if let Some((ui_handle, ui_inbox)) = ui_link {
        handle.add_worker_peer(ui_handle, ui_inbox);
    }
    // Monitor tab: this thread owns the handle, so it is the one place the
    // worker's metrics can be sampled — published at every quiescent point
    // (post-boot, post-dispatch). An idle worker's numbers are frozen, which
    // is exactly right: nothing is running.
    let mon = crate::embed::monitor_register(format!("worker {}", handle_slot(id)), "worker");
    mon.publish(handle.metrics());
    // From here on, everything the worker prints (Transcript, error traces)
    // reaches the primary's transcript instead of a stray stdout (M2).
    handle.set_transcript(Box::new(ForwardTranscript::to(id, to_primary.clone())));
    if let Some(src) = init {
        mon.set_busy(true);
        let ok = handle.exec(src).is_ok();
        mon.set_busy(false);
        mon.publish(handle.metrics());
        if !ok {
            mon.mark_dead();
            alive.store(false, Ordering::Relaxed);
            let _ = to_primary.send(died_envelope(id));
            return;
        }
    }
    // The timer service's own bookkeeping: when the next tick is due, and
    // the interval last asked for (read back from the VM after every entry,
    // since the guest can change or stop it at any point).
    let mut next_tick = std::time::Instant::now();
    loop {
        // Sleep until the sooner of the pulse check and this worker's next
        // frame tick — no extra thread, no busy poll: the deadline the loop
        // was already waiting on is simply the nearer one.
        let tick = handle.worker_tick_ms();
        let wait = if tick == 0 {
            EPOCH_CHECK_INTERVAL
        } else {
            let now = std::time::Instant::now();
            if next_tick <= now {
                Duration::from_millis(0)
            } else {
                (next_tick - now).min(EPOCH_CHECK_INTERVAL)
            }
        };
        match rx.recv_timeout(wait) {
            Ok(env) => {
                handle.stage_pending(env);
                // A guest error mid-dispatch (error:, DNU, even a native
                // fault — S21's recovery surfaces all of them as Err)
                // retires this worker: its state is suspect, so report death
                // and unwind. The VmHandle drops normally (heap unmapped) —
                // pthread_exit is only for the truly unrecoverable path
                // inside the fatal machinery itself.
                mon.set_busy(true);
                let ok = handle.exec("Worker dispatchPending.").is_ok();
                mon.set_busy(false);
                mon.publish(handle.metrics());
                if !ok {
                    mon.mark_dead();
                    alive.store(false, Ordering::Relaxed);
                    let _ = to_primary.send(died_envelope(id));
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // A FRAME TICK, if one is due: run top-level, exactly like an
                // inbound message and under the same rule — one thing at a
                // time. A tick that raises retires the worker like any other
                // guest fatal; a slow tick simply delays the next (the
                // deadline is recomputed from NOW, so a worker that cannot
                // keep up degrades in rate rather than accumulating a debt it
                // then tries to pay off in a burst).
                let tick = handle.worker_tick_ms();
                if tick != 0 && std::time::Instant::now() >= next_tick {
                    mon.set_busy(true);
                    let ok = handle.exec("Worker dispatchTick.").is_ok();
                    mon.set_busy(false);
                    mon.publish(handle.metrics());
                    next_tick = std::time::Instant::now() + Duration::from_millis(tick);
                    if !ok {
                        mon.mark_dead();
                        alive.store(false, Ordering::Relaxed);
                        let _ = to_primary.send(died_envelope(id));
                        return;
                    }
                    continue;
                }
                // The pulse check — supervision of last resort. A FATAL
                // primary death runs no Drop glue: our channel sender leaks
                // inside the dead VM's links and a plain `recv()` would park
                // us forever, heap mapped, invisible to the respawned
                // generation. The epoch verdict is an invariant, not policy:
                // with the star's center gone, no message can ever reach us
                // again. No died-envelope either — it would only queue into
                // the dead primary's leaked inbox.
                if !primary_epoch_alive(epoch) {
                    eprintln!(
                        "macvm: worker {id} reaped — its primary (epoch {epoch}) is gone"
                    );
                    mon.mark_dead();
                    alive.store(false, Ordering::Relaxed);
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // Channel closed: the primary terminated us (or dropped cleanly).
    // Retired, not crashed — same roster outcome either way.
    mon.mark_dead();
    alive.store(false, Ordering::Relaxed);
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
            // A STALE HANDLE IS REFUSED, not redirected: if this slot has
            // been reclaimed since the handle was minted, its generation no
            // longer matches and there is nobody here by that name.
            let Some(link) = link_for_mut(links, id) else {
                return false;
            };
            if !link.alive {
                return false;
            }
            let env = Envelope {
                from: 0,
                corr,
                bytes,
                reply_to: None,
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
            self_inbox,
            peers,
            ..
        } => {
            // A WORKER MAY NOW REACH A PEER IT HOLDS A LINK TO. `id == 0` is
            // still the parent, unchanged and always available; anything else
            // must be a link this worker was introduced to or learned by
            // being messaged (`docs/worker_peer_links.md`). There is
            // deliberately no lookup by number: an id this worker holds no
            // link for answers false, exactly as an unknown or dead worker
            // always has.
            let link = if id == 0 {
                to_primary
            } else {
                match peers.iter().find(|(pid, _)| *pid == id) {
                    Some((_, inbox)) => inbox,
                    None => return false,
                }
            };
            // Offer our own inbox so the far side can answer us directly
            // rather than through the primary — the whole point of the field.
            let env = Envelope {
                from: *self_id,
                corr,
                bytes,
                reply_to: self_inbox.clone(),
            };
            if link.send(env).is_err() {
                // The peer's receiver is gone. Drop the link rather than keep
                // a dead one: a reused id must never be reachable through its
                // predecessor's channel. The parent's link is never dropped —
                // a worker with no parent has nowhere to report anything.
                if id != 0 {
                    peers.retain(|(pid, _)| *pid != id);
                }
                return false;
            }
            true
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
    let Some(link) = link_for_mut(links, id) else {
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
    link_for(links, id).map(|l| l.alive).unwrap_or(false)
}

/// This VM's frame-tick interval in milliseconds, 0 when it has none. Read
/// by the worker loop to shorten its inbox wait (`docs/appspec.md`: the
/// timer service a demo's 60 Hz step needs).
pub fn tick_ms(vm: &VmState) -> u64 {
    match vm.workers.as_deref() {
        Some(WorkerState::Worker { tick_ms, .. }) => *tick_ms,
        _ => 0,
    }
}

/// Ask the VM to wake this worker every `ms` milliseconds (0 stops it). The
/// wake arrives as `Worker dispatchTick.`, run top-level exactly like an
/// inbound message — one thing at a time, never nested inside another.
pub fn set_tick_ms(vm: &mut VmState, ms: u64) -> bool {
    match vm.workers.as_deref_mut() {
        Some(WorkerState::Worker { tick_ms, .. }) => {
            *tick_ms = ms;
            true
        }
        _ => false, // the primary has a host run loop; it needs no service
    }
}

/// The handle this VM knows as the DISPLAY, or 0 if it has none. A primary
/// answers whichever link it was told is the UI; a worker answers the peer it
/// was introduced to at spawn. It is the one piece of the peer-link machinery
/// the guest needs by NAME — a worker holds the display's link, but Smalltalk
/// has to be able to say `send this there`, and a handle is how you say it.
pub fn ui_peer_handle(vm: &VmState) -> u32 {
    match vm.workers.as_deref() {
        Some(WorkerState::Primary { ui_peer, .. }) => ui_peer.unwrap_or(0),
        // A worker was introduced to exactly one display, so the first peer
        // it holds that is not its parent IS the display. There is no
        // ambiguity to resolve because there is no general peer discovery:
        // the only link a worker is GIVEN is the UI's, and everything else it
        // holds it learned from someone who spoke to it first.
        Some(WorkerState::Worker { peers, .. }) => peers.first().map(|(h, _)| *h).unwrap_or(0),
        None => 0,
    }
}

/// Tell this primary which of its registered workers is the UI — the display
/// every app VM it spawns should be introduced to (`docs/worker_peer_links.md`
/// §3). The Cocoa host calls this right after `register_hosted_worker`. A
/// primary that is never told simply introduces nobody.
pub fn set_ui_peer(vm: &mut VmState, handle: u32) -> bool {
    let Some(ws) = vm.workers.as_mut() else {
        return false;
    };
    let WorkerState::Primary { ui_peer, links, .. } = &mut **ws else {
        return false;
    };
    if link_for(links, handle).is_none() {
        return false; // not one of ours, or already superseded
    }
    *ui_peer = Some(handle);
    true
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

/// A clone of the inbox sender for worker `id` in THIS primary's registry —
/// the link the primary `send`s that worker along. The Cocoa GUI's primary uses
/// it to aim its transcript-forward sink at the UI worker (CG4 §7.4:
/// `ForwardTranscript::to(0, ui_inbox)`), reusing the exact transport worker
/// messages ride. `None` if this VM is not a primary, or there is no live worker
/// `id`.
pub fn worker_inbox_sender(vm: &crate::runtime::vm_state::VmState, id: u32) -> Option<InboxSender> {
    let WorkerState::Primary { links, .. } = vm.workers.as_deref()? else {
        return None;
    };
    let link = link_for(links, id)?;
    link.alive.then(|| link.inbox.clone())
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

#[cfg(test)]
mod peer_link_tests {
    //! `docs/worker_peer_links.md` §6 — the transport rules, at the level they
    //! are written at. These need no VM: a `WorkerState` and a channel are the
    //! whole mechanism, which is itself the argument that peer links are small.

    use super::*;

    /// A worker with a channel standing in for its parent, plus the receiving
    /// end so a test can see what actually arrived.
    fn worker(id: u32) -> (WorkerState, Receiver<Envelope>) {
        let (tx, rx) = channel::<Envelope>();
        let mut ws = WorkerState::new_worker(id, InboxSender::detached(tx));
        let (self_tx, _self_rx) = channel::<Envelope>();
        ws.set_self_inbox(InboxSender::detached(self_tx));
        (ws, rx)
    }

    /// The rule that keeps peer links free of a registry: being messaged by
    /// someone who offered a link teaches you how to answer them.
    #[test]
    fn an_arriving_envelope_that_offers_a_link_is_learned() {
        let (mut ws, _parent) = worker(1);
        let (peer_tx, peer_rx) = channel::<Envelope>();
        let env = Envelope {
            from: 7,
            corr: 0,
            bytes: vec![1, 2, 3],
            reply_to: Some(InboxSender::detached(peer_tx)),
        };
        ws.learn_peer_from(&env);
        let WorkerState::Worker { peers, .. } = &ws else {
            panic!("worker role");
        };
        assert_eq!(peers.len(), 1, "the link was remembered");
        assert_eq!(peers[0].0, 7);
        // And it is a live link, not a note: sending down it arrives.
        peers[0].1.send(Envelope::plain(1, 0, vec![9])).unwrap();
        assert_eq!(peer_rx.recv().unwrap().bytes, vec![9]);
    }

    /// An envelope with nothing to offer teaches nothing — which is every
    /// message that predates peer links, and why learning is safe to run on
    /// all of them.
    #[test]
    fn an_envelope_without_a_link_teaches_nothing() {
        let (mut ws, _parent) = worker(1);
        ws.learn_peer_from(&Envelope::plain(7, 0, vec![1]));
        let WorkerState::Worker { peers, .. } = &ws else {
            panic!("worker role");
        };
        assert!(peers.is_empty());
    }

    /// ── generational handles ────────────────────────────────────────
    ///
    /// A handle names a slot AND which occupant of it, so reclaiming a slot
    /// does not make the previous occupant's handle address the new one.

    fn link(gen: u32) -> WorkerLink {
        let (tx, _rx) = channel::<Envelope>();
        WorkerLink {
            inbox: InboxSender::detached(tx),
            alive: true,
            gen,
        }
    }

    #[test]
    fn a_handle_names_a_slot_and_which_occupant_of_it() {
        let h = make_handle(3, 7);
        assert_eq!(handle_slot(h), 3);
        assert_eq!(handle_generation(h), 7);
        assert_eq!(slot_index_of(h), Some(2), "slots are 1-based, links is 0-based");
        assert_eq!(slot_index_of(0), None, "handle 0 is the parent, never a slot");
    }

    /// THE CLAIM. A slot is reclaimed — as it must be, because MAX_WORKERS
    /// caps concurrent workers — and the handle minted for its previous
    /// occupant stops resolving instead of quietly naming the new one.
    #[test]
    fn a_stale_handle_is_refused_rather_than_redirected() {
        let mut links = vec![link(1)];
        let first = make_handle(1, 1);
        assert!(link_for(&links, first).is_some(), "the live handle resolves");

        // The worker dies and the slot is reclaimed by a new spawn.
        links[0] = link(2);
        let second = make_handle(1, 2);

        assert!(
            link_for(&links, first).is_none(),
            "the dead worker's handle names nobody — this is the whole point"
        );
        assert!(link_for(&links, second).is_some(), "the new occupant resolves");
        assert_ne!(first, second, "and the two handles were never equal");
    }

    /// A respawned worker reusing an id must not be reachable through its
    /// predecessor's channel, so an introduction REPLACES rather than appends.
    #[test]
    fn introducing_the_same_id_twice_replaces_the_link() {
        let (mut ws, _parent) = worker(1);
        let (old_tx, old_rx) = channel::<Envelope>();
        let (new_tx, new_rx) = channel::<Envelope>();
        ws.add_peer(7, InboxSender::detached(old_tx));
        ws.add_peer(7, InboxSender::detached(new_tx));
        let WorkerState::Worker { peers, .. } = &ws else {
            panic!("worker role");
        };
        assert_eq!(peers.len(), 1, "one entry, not two");
        peers[0].1.send(Envelope::plain(1, 0, vec![9])).unwrap();
        assert!(new_rx.try_recv().is_ok(), "the new link is the live one");
        assert!(old_rx.try_recv().is_err(), "the old one is gone");
    }
}
