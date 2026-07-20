//! The primary watchdog supervisor (`cocoa_gui_design.md` §5.1, sprint CG4).
//!
//! The persistent primary VM — the environment's state and brain — runs on its
//! own OS thread with a Rust-driven dispatch loop (`Worker pumpInbox:`, one beat
//! per iteration). A **separate** watchdog thread owns that thread's life: it
//! boots each primary *generation*, waits for its death, and on death (a fatal
//! doit `pthread_exit`s the generation thread) or an explicit restart **respawns
//! the primary from source** and hands the fresh `(id, to_primary, hosted_inbox)`
//! link to the main thread as a **re-sync**. The UI worker holds no durable
//! state, so it survives the primary's death untouched
//! (`feedback_recover_clean_or_die`); its work is the primary's, on the primary's
//! thread.
//!
//! **Never `.join()`/`.is_finished()` a generation thread** (the S21 rule,
//! `gui/src/vm_host.rs`): a thread terminated by `FatalMode::ExitThread`'s
//! `pthread_exit` runs no unwinding and hangs `join`. Death is observed instead
//! as an **exact death signal** — the generation's `fatal` hook
//! ([`macvm::embed::set_thread_fatal_hook`]) posts `Died` the instant before its
//! `pthread_exit` — so a busy-but-live primary (a multi-second Workspace doit) is
//! NEVER mistaken for dead (the CG4 review's must-fix; an earlier
//! heartbeat-timeout draft would have respawned it, discarding the computation
//! and all session-defined state). Detached generation threads are simply
//! abandoned, never joined — the S21 model, re-homed to the watchdog thread.
//!
//! This module owns ONLY the primary lifecycle. `main.rs` owns the UI worker (on
//! main), applies each re-sync ([`PrimarySupervisor::poll_resync`]) by re-pointing
//! the UI worker's reply link + drain inbox, and drives the run loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use macvm::embed::VmMetrics;
use macvm::runtime::workers::{HostedInbox, InboxSender, InboxWakeFn, WorkerBootFn};
use macvm::runtime::VmError;

/// One primary generation's link to the UI worker: the worker id in the (fresh)
/// primary's registry, the reply link the UI worker `send`s along, and the inbox
/// the UI worker drains for primary→UI traffic. A re-sync replaces all three
/// together — never a torn mix of an old link with a new inbox (the S21
/// `WorkerHandles` discipline).
pub struct PrimaryLink {
    pub hosted_id: u32,
    pub to_primary: InboxSender,
    pub hosted_inbox: HostedInbox,
}

/// The dispatch-loop beat: how long each `pumpInbox:` blocks in the inbox before
/// returning to Rust to re-check its stop flag. It bounds only how quickly a
/// clean retire (a `Restart`) takes effect — liveness is NOT inferred from beat
/// timing (see [`Event`]).
const PUMP_BEAT_MS: u64 = 250;

/// Bounded backoff between respawn attempts when a fresh generation fails to
/// boot, and the cap after which the watchdog gives up: a deterministic boot
/// failure is unrecoverable, so spinning backoff-free (the first draft) would
/// peg a core forever with no way out.
const RESPAWN_BACKOFF: Duration = Duration::from_millis(200);
const MAX_CONSECUTIVE_BOOT_FAILURES: u32 = 5;

/// What the watchdog blocks on. There is deliberately **no heartbeat and no
/// timeout**: a primary running a multi-second Workspace doit (a benchmark, a
/// loop, image work — the routine load of a Smalltalk environment) is ALIVE,
/// not dead, and must never be respawned (that would discard the computation
/// AND every class/global defined this session, since a respawn boots from
/// source — CG4 review). So death is signalled EXACTLY: `Died` is posted by the
/// generation's fatal hook ([`macvm::embed::set_thread_fatal_hook`]) the instant
/// before its `pthread_exit`, and `Restart` is the explicit user/test respawn.
enum Event {
    Died,
    #[allow(dead_code)]
    Restart,
}

/// The main thread's handle onto the supervised primary: request a restart, and
/// poll for a re-sync link to re-point the UI worker onto a fresh primary.
pub struct PrimarySupervisor {
    /// Written by [`PrimarySupervisor::restart`] (the reserved explicit-restart
    /// path); the automatic fatal-recovery respawn arrives on the same channel
    /// from a generation's own `Died` hook, so it needs no poke from here.
    #[allow(dead_code)]
    events: Sender<Event>,
    resync_rx: Receiver<PrimaryLink>,
    /// The latest `VmMetrics` sample (CG5), written by the CURRENT generation's
    /// own beat loop on ITS thread (`primary.metrics()` — a cheap field read of
    /// its own live `VmState`, never a cross-thread VmState touch) and read by
    /// main's toolbar-refresh timer via [`metrics`](Self::metrics). Survives a
    /// respawn: a fresh generation just starts writing into the same cell.
    metrics: Arc<Mutex<VmMetrics>>,
    /// Detached — never joined (the watchdog outlives every generation and the
    /// process ends by `[NSApp terminate:]`).
    _watchdog: JoinHandle<()>,
}

impl PrimarySupervisor {
    /// Boot the first primary generation and start the watchdog. Blocks until the
    /// primary is up and has registered the UI worker (or reports a boot
    /// failure), returning the supervisor + the first [`PrimaryLink`] for
    /// `main.rs` to wire the UI worker onto. `ui_wake` is the UI worker inbox's
    /// run-loop poke, fired whenever a primary generation `send`s it (and once
    /// per re-sync, so main's drain source picks the new link up).
    pub fn spawn(
        world_boot: WorkerBootFn,
        ui_wake: InboxWakeFn,
    ) -> Result<(PrimarySupervisor, PrimaryLink), VmError> {
        let (events_tx, events_rx) = mpsc::channel::<Event>();
        let (resync_tx, resync_rx) = mpsc::channel::<PrimaryLink>();
        // The first generation's link comes back on its own channel so `spawn`
        // can surface a boot failure synchronously; later generations arrive as
        // re-syncs on `resync_rx`.
        let (first_tx, first_rx) = mpsc::channel::<Result<PrimaryLink, VmError>>();
        let metrics: Arc<Mutex<VmMetrics>> = Arc::new(Mutex::new(VmMetrics::default()));

        let events_for_hb = events_tx.clone();
        let metrics_for_watchdog = metrics.clone();
        let watchdog = std::thread::Builder::new()
            .name("macvm-cocoa-watchdog".into())
            .spawn(move || {
                watchdog_main(
                    world_boot,
                    ui_wake,
                    events_for_hb,
                    events_rx,
                    first_tx,
                    resync_tx,
                    metrics_for_watchdog,
                );
            })
            .map_err(|e| VmError {
                msg: format!("could not spawn the primary watchdog thread: {e}"),
            })?;

        let first = match first_rx.recv() {
            Ok(Ok(link)) => link,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(VmError {
                    msg: "the primary watchdog died before the first generation registered".into(),
                })
            }
        };
        Ok((
            PrimarySupervisor {
                events: events_tx,
                resync_rx,
                metrics,
                _watchdog: watchdog,
            },
            first,
        ))
    }

    /// The most recent `VmMetrics` sample from the CURRENT primary generation
    /// (CG5) — a cheap lock+copy, safe to poll from a main-thread timer at any
    /// rate. `VmMetrics::default()` (all zero) before the first sample lands.
    pub fn metrics(&self) -> VmMetrics {
        *self.metrics.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Request an immediate respawn of the primary from source — the explicit
    /// restart: the Debug menu's "Restart Primary VM" (`primary_restart.rs`) and
    /// the deterministic trigger the headless supervisor gate uses. The automatic
    /// fatal-recovery path does not need this: a fatal doit's own `Died` hook
    /// wakes the watchdog directly. The fresh generation's link arrives on
    /// [`poll_resync`](Self::poll_resync).
    pub fn restart(&self) {
        let _ = self.events.send(Event::Restart);
    }

    /// A pending re-sync link, or `None` — the fresh primary generation the
    /// watchdog booted after a death/restart. `main.rs` calls this from its
    /// run-loop drain and, on `Some`, re-points the UI worker's reply link +
    /// drain inbox onto the new primary.
    pub fn poll_resync(&self) -> Option<PrimaryLink> {
        self.resync_rx.try_recv().ok()
    }
}

/// The watchdog thread body: boot generations, block until each one's `Died`
/// signal (or an explicit restart), and respawn from source — the S21 supervisor,
/// re-homed to a thread that is neither the primary nor main so it outlives
/// either.
fn watchdog_main(
    world_boot: WorkerBootFn,
    ui_wake: InboxWakeFn,
    events_tx: Sender<Event>,
    events_rx: Receiver<Event>,
    first_tx: Sender<Result<PrimaryLink, VmError>>,
    resync_tx: Sender<PrimaryLink>,
    metrics: Arc<Mutex<VmMetrics>>,
) {
    let mut generation: u64 = 0;
    let mut consecutive_boot_failures: u32 = 0;
    loop {
        generation += 1;
        // Each generation runs to death or until its stop flag is set (a clean
        // retire on restart — a fatal instead `pthread_exit`s the thread, which
        // this flag can no longer reach, and that is fine: it is detached).
        let stop = Arc::new(AtomicBool::new(false));
        let (reg_tx, reg_rx) = mpsc::channel::<Result<PrimaryLink, VmError>>();

        let boot = world_boot.clone();
        let wake = ui_wake.clone();
        let hb = events_tx.clone();
        let stop_for_gen = stop.clone();
        let metrics_for_gen = metrics.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("macvm-cocoa-primary-gen{generation}"))
            .spawn(move || {
                primary_generation_main(boot, wake, reg_tx, hb, stop_for_gen, metrics_for_gen)
            });
        if let Err(e) = spawned {
            let _ = first_tx.send(Err(VmError {
                msg: format!("could not spawn primary generation {generation}: {e}"),
            }));
            return;
        }

        // Wait for this generation to register the UI worker (or report a boot
        // failure / die before registering).
        let link = match reg_rx.recv() {
            Ok(Ok(link)) => {
                consecutive_boot_failures = 0;
                link
            }
            Ok(Err(e)) => {
                if generation == 1 {
                    let _ = first_tx.send(Err(e));
                    return;
                }
                // A respawn that failed to boot: retire the flag, back off, and
                // try again (the process is only useful with a live primary) —
                // but give up after a run of failures rather than spin a core.
                stop.store(true, Ordering::Release);
                consecutive_boot_failures += 1;
                if consecutive_boot_failures >= MAX_CONSECUTIVE_BOOT_FAILURES {
                    // The honest end (the rebuild.rs Layer-3 doctrine): a
                    // `return` here left the window alive but permanently
                    // primary-less — every doit hanging forever with nothing
                    // surfaced (e.g. a corrupted image reproduces this
                    // deterministically). Exit instead.
                    eprintln!(
                        "macvm-cocoa: primary respawn failed {consecutive_boot_failures}× \
                         in a row ({}); exiting — the GUI is useless without a primary \
                         (is world/image.sqlite3 corrupt?)",
                        e.msg
                    );
                    std::process::exit(73);
                }
                std::thread::sleep(RESPAWN_BACKOFF);
                continue;
            }
            Err(_) => {
                // The generation thread vanished before registering.
                if generation == 1 {
                    let _ = first_tx.send(Err(VmError {
                        msg: "primary generation 1 died before registering the UI worker".into(),
                    }));
                    return;
                }
                stop.store(true, Ordering::Release);
                consecutive_boot_failures += 1;
                if consecutive_boot_failures >= MAX_CONSECUTIVE_BOOT_FAILURES {
                    // Same honest-end doctrine as the boot-failure arm above.
                    eprintln!(
                        "macvm-cocoa: primary respawn died before registering \
                         {consecutive_boot_failures}× in a row; exiting — the GUI \
                         is useless without a primary"
                    );
                    std::process::exit(73);
                }
                std::thread::sleep(RESPAWN_BACKOFF);
                continue;
            }
        };

        // Hand the link to main: the first generation synchronously (so `spawn`
        // can report failure), later generations as a re-sync + a run-loop poke
        // so main's drain source picks it up.
        if generation == 1 {
            if first_tx.send(Ok(link)).is_err() {
                return; // main gone before it could receive — nothing to serve
            }
        } else {
            if resync_tx.send(link).is_err() {
                return;
            }
            ui_wake();
        }

        // Supervise: BLOCK until this generation posts `Died` (its fatal hook,
        // the instant before `pthread_exit`) or an explicit `Restart` arrives.
        // No timeout — a primary busy in a long doit is alive, not dead, and is
        // never respawned. `stop` is best-effort (a `pthread_exit`ed thread
        // ignores it and is simply abandoned, never joined — the S21 rule).
        match events_rx.recv() {
            Ok(Event::Died) | Ok(Event::Restart) => stop.store(true, Ordering::Release),
            Err(_) => return, // supervisor dropped — nothing left to serve
        }
    }
}

/// One primary generation: boot the world from source, become a primary,
/// register the UI worker, register its `Died` fatal hook, hand its link back,
/// then run the Rust-driven dispatch loop — one `Worker pumpInbox:` beat per
/// iteration — until its stop flag is set. A fatal doit `pthread_exit`s this
/// thread mid-beat, but posts `Died` first so the watchdog respawns. An ordinary
/// recoverable error (`ErrorPolicy::Resume`) surfaces as an `Err` from the beat:
/// the VM has
/// already rewound to its clean idle baseline, so serving simply continues — a
/// bad doit never restarts the whole environment.
fn primary_generation_main(
    world_boot: WorkerBootFn,
    ui_wake: InboxWakeFn,
    reg_tx: Sender<Result<PrimaryLink, VmError>>,
    events: Sender<Event>,
    stop: Arc<AtomicBool>,
    metrics: Arc<Mutex<VmMetrics>>,
) {
    let mut primary = match world_boot() {
        Ok(h) => h,
        Err(e) => {
            let _ = reg_tx.send(Err(VmError {
                msg: format!("primary boot failed: {}", e.msg),
            }));
            return;
        }
    };
    // Installing a worker-boot fn makes this VM the PRIMARY (creates its inbox +
    // registry). It boots compute workers from the same source (CG8).
    primary.set_worker_boot(world_boot.clone());

    // (CG10) The primary drives the game demos (only a primary can spawn the
    // compute workers ParallelMandel needs). Its GameCommands cross to the main
    // thread over the shared queue + run-loop wake — the same worker→main
    // transport everything else uses. Re-installed on every respawned generation.
    primary.set_game_sink(Box::new(crate::game::PrimaryGameSink));

    // DBG4: the GUI debugger frontend — the halt loop publishes to the host
    // cell and blocks on the command channel; `ui_wake` rides along because
    // the pump (the usual wake source) is parked during a halt. PRIMARY only
    // (a UI-worker halt would park the main thread). Re-installed per
    // generation, like the game sink.
    crate::debugger::install(&mut primary, ui_wake.clone());

    // Register the UI worker as an externally-hosted peer (CG1). `ui_wake` fires
    // whenever this primary `send`s the UI worker — cloned here (it's an `Arc`)
    // because the beat loop below also calls it directly, unconditionally,
    // every beat (CG5 metrics tick).
    let Some((id, hosted_inbox, to_primary)) = primary.register_hosted_worker(ui_wake.clone())
    else {
        let _ = reg_tx.send(Err(VmError {
            msg: "register_hosted_worker failed (not a primary, or the fleet is at its cap)".into(),
        }));
        return;
    };

    // The primary's own transcript now forwards to the UI worker's Transcript
    // view (§7.4), re-aimed at THIS generation's link.
    primary.forward_transcript_to_ui(id);

    // Register the death signal: the instant before a GENUINE fatal (heap/stack
    // exhaustion, or a Die-policy error) `pthread_exit`s THIS thread, post `Died`
    // so the watchdog respawns. Because `pthread_exit` runs no Drop glue this is
    // the ONLY signal a dead generation can send — and it fires ONLY on a real
    // fatal, never merely because the primary is busy in a long doit (the CG4
    // review's must-fix: a busy primary must never be respawned).
    macvm::embed::set_thread_fatal_hook(Box::new(move || {
        let _ = events.send(Event::Died);
    }));

    if reg_tx
        .send(Ok(PrimaryLink {
            hosted_id: id,
            to_primary,
            hosted_inbox,
        }))
        .is_err()
    {
        return; // watchdog gone
    }

    // The dispatch loop: one `pumpInbox:` beat per iteration, re-checking the
    // stop flag between beats so a clean `Restart` retires within one beat. A
    // fatal doit ends this thread inside `exec` (the fatal hook posts `Died`
    // first). A recoverable guest error rewinds the VM to its clean idle
    // baseline and returns `Err`: keep serving — a bad doit never restarts the
    // whole environment, and a long-running one is not death.
    //
    // CG5: sample `primary.metrics()` (a cheap field read of THIS thread's own
    // live VmState — never a cross-thread touch) into the shared cell every
    // beat, then wake the UI worker's inbox UNCONDITIONALLY (not just when a
    // real send happened) — this turns the already-existing default-mode drain
    // source into a de-facto ~4Hz toolbar-refresh tick, reusing 100% proven
    // wiring instead of a new NSTimer/CFRunLoopTimer.
    // This generation's file-in birth stamp (filein.rs): only requests made
    // BEFORE this generation existed are ours to run — the fresh-world half
    // of File In's contract.
    let filein_birth = crate::filein::birth_stamp();
    while !stop.load(Ordering::Acquire) {
        *metrics.lock().unwrap_or_else(|e| e.into_inner()) = primary.metrics();
        ui_wake();
        // (CG10) While a game runs, spin fast — a short inbox timeout so band
        // replies dispatch promptly — and run each frame step at TOP LEVEL here
        // (the timer on main gates it to ~60Hz via STEP_DUE). Running the step
        // as a fresh top-level entry, not a nested uiReq, is what keeps its
        // JIT-compiled compute clear of the frame-walk invariant under GC.
        // DBG4: keep the Halt on Error toggle in sync (a cheap field write).
        primary.set_halt_on_error(
            crate::debugger::HALT_ON_ERROR.load(std::sync::atomic::Ordering::Acquire),
        );
        let beat = if crate::game::is_active() { 4 } else { PUMP_BEAT_MS };
        let _ = primary.exec(&format!("Worker pumpInbox: {beat}."));
        if let Some(step) = crate::game::poll_primary_step() {
            let _ = primary.exec(&step);
        }
        // File ▸ File In (filein.rs): run the user's .mst HERE — the primary's
        // own thread, between top-level entries — via run_file (every
        // top-level item, exactly `macvm run <file>`). The outcome rides the
        // primary's transcript forwarding back to the UI Transcript view.
        if let Some(path) = crate::filein::take_for(filein_birth) {
            let note = match primary.run_file(std::path::Path::new(&path)) {
                Ok(()) => format!("file-in: loaded {path}"),
                Err(e) => format!("file-in FAILED: {}", e.msg),
            };
            eprintln!("macvm-cocoa: {note}");
            let _ = primary.exec(&format!(
                "Transcript showCr: '{}'.",
                crate::filein::escape_st(&note)
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use macvm::embed::VmHandle;
    use macvm::runtime::{JitMode, VmOptions};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    fn world_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../world")
    }

    fn boot_fn() -> WorkerBootFn {
        let world = world_dir();
        Arc::new(move || {
            VmHandle::boot(
                VmOptions {
                    heap_mib: 64,
                    jit: JitMode::Off,
                    ..Default::default()
                },
                &world,
            )
        })
    }

    /// Boot a UI worker on THIS thread, pointed at `link`, with a result
    /// scoreboard — the arrangement `main.rs` builds on the main thread.
    fn boot_ui(link: &PrimaryLink) -> VmHandle {
        let mut ui = VmHandle::boot(
            VmOptions {
                heap_mib: 64,
                jit: JitMode::Off,
                ..Default::default()
            },
            &world_dir(),
        )
        .expect("boot the UI worker");
        ui.install_worker_role(link.hosted_id, link.to_primary.clone());
        ui.exec("Object subclass: UiT [ <classVars: R> UiT class >> r: x [ R := x ] UiT class >> r [ ^R ] ]")
            .expect("UI scoreboard");
        ui
    }

    /// Ship a `#doit` and wait (bounded) for its reply to drain back through
    /// `link.hosted_inbox` — the primary generation's own loop serves it on its
    /// thread. Asserts the result equals `expect`.
    fn round_trip(ui: &mut VmHandle, link: &PrimaryLink, src: &str, expect: &str) {
        ui.exec("UiT r: nil.").unwrap();
        ui.exec(&format!("Worker uiDoit: '{src}' onReply: [:r | UiT r: r]."))
            .expect("ship the doit to the supervised primary");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            while let Some(env) = link.hosted_inbox.poll() {
                ui.dispatch_hosted_envelope(env)
                    .expect("UI worker routes the reply");
            }
            if ui.eval("UiT r").unwrap().trim() == expect {
                return;
            }
            if Instant::now() > deadline {
                panic!(
                    "no #uiReply for '{src}' within the deadline (got {:?})",
                    ui.eval("UiT r")
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn primary_supervisor_restarts_from_source_and_the_ui_re_syncs() {
        // CG4 §5.1: the watchdog boots the primary on its own thread, serves a
        // doit round-trip, then on restart respawns the primary FROM SOURCE and
        // re-syncs the UI worker onto it — the next doit works. A real threaded
        // supervisor (the primary generation runs its own pumpInbox loop); the
        // restart stands in for the on-screen "a fatal doit is detected and the
        // environment restarts", which drives the identical respawn path.
        let wakes = Arc::new(AtomicUsize::new(0));
        let wk = wakes.clone();
        let ui_wake: InboxWakeFn = Arc::new(move || {
            wk.fetch_add(1, Ordering::Relaxed);
        });

        let (sup, mut link) =
            PrimarySupervisor::spawn(boot_fn(), ui_wake).expect("boot the supervised primary");
        let mut ui = boot_ui(&link);

        // Generation 1 serves the doit.
        round_trip(&mut ui, &link, "6 * 7", "'42'");

        // Scripted death → respawn from source. The watchdog delivers a fresh
        // link as a re-sync; wait for it (bounded).
        sup.restart();
        let deadline = Instant::now() + Duration::from_secs(5);
        let new_link = loop {
            if let Some(l) = sup.poll_resync() {
                break l;
            }
            assert!(
                Instant::now() < deadline,
                "the watchdog must deliver a re-sync link after a restart"
            );
            std::thread::sleep(Duration::from_millis(5));
        };

        // Re-sync the UI worker onto the fresh primary (what main.rs does on the
        // run loop): re-point the reply link + swap the drain inbox.
        ui.install_worker_role(new_link.hosted_id, new_link.to_primary.clone());
        link = new_link;

        // The next doit works — the environment recovered and the UI re-synced.
        round_trip(&mut ui, &link, "100 + 1", "'101'");
        assert!(
            wakes.load(Ordering::Relaxed) >= 1,
            "the primary→UI link must have fired the run-loop wake"
        );
    }

    /// CG4 review must-fix: death is signalled EXACTLY, by the generation's own
    /// fatal hook — NOT inferred from a heartbeat/timeout that a long doit would
    /// starve. Prove the real fatal path: a doit that stack-overflows the primary
    /// (a genuine `fatal_exit`, `ExitThread` → `pthread_exit`) fires the hook,
    /// the watchdog respawns from source, and the next doit works. A long *live*
    /// doit takes this same `exec` path WITHOUT a fatal, so it posts no `Died`
    /// and is never respawned — the property the removed timeout used to violate.
    #[test]
    fn a_fatal_doit_signals_death_and_the_primary_respawns() {
        let ui_wake: InboxWakeFn = Arc::new(|| {});
        let (sup, mut link) =
            PrimarySupervisor::spawn(boot_fn(), ui_wake).expect("boot the supervised primary");
        let mut ui = boot_ui(&link);

        // gen1 is alive and defines a self-recursive class (a class def answers
        // nil), then we fire the fatal call WITHOUT awaiting a reply — the
        // generation `pthread_exit`s mid-doit, so no reply ever comes.
        round_trip(
            &mut ui,
            &link,
            "Object subclass: Cg4Boom [ Cg4Boom class >> boom [ ^self boom ] ]",
            "'nil'",
        );
        // No respawn has happened up to now (a live primary is never respawned).
        assert!(
            sup.poll_resync().is_none(),
            "a live primary must not be respawned before any fatal"
        );

        // Fire the fatal doit: unbounded recursion overflows the process stack →
        // `fatal_exit(70)` → the fatal hook posts `Died` → `pthread_exit`.
        ui.exec("Worker uiDoit: 'Cg4Boom boom' onReply: [:r | UiT r: r].")
            .expect("ship the fatal doit");

        // The watchdog observes `Died` and respawns from source; wait (bounded)
        // for the re-sync link.
        let deadline = Instant::now() + Duration::from_secs(5);
        let new_link = loop {
            if let Some(l) = sup.poll_resync() {
                break l;
            }
            assert!(
                Instant::now() < deadline,
                "a fatal doit must post Died and the watchdog must respawn"
            );
            std::thread::sleep(Duration::from_millis(5));
        };

        // Re-sync onto the fresh primary; the environment recovered.
        ui.install_worker_role(new_link.hosted_id, new_link.to_primary.clone());
        link = new_link;
        round_trip(&mut ui, &link, "100 + 1", "'101'");
    }

    /// JIT ON (`crate::boot::vm_options`'s own default) -- unlike `boot_fn`/
    /// `boot_ui` above, which pin `JitMode::Off` for tests that don't want
    /// compiler variability. This one specifically NEEDS the JIT to reach the
    /// bug it pins.
    fn boot_fn_jit() -> WorkerBootFn {
        let world = world_dir();
        Arc::new(move || VmHandle::boot(crate::boot::vm_options(), &world))
    }

    fn boot_ui_jit(link: &PrimaryLink) -> VmHandle {
        let mut ui = VmHandle::boot(crate::boot::vm_options(), &world_dir())
            .expect("boot the UI worker (JIT on)");
        ui.install_worker_role(link.hosted_id, link.to_primary.clone());
        ui.exec("Object subclass: UiT [ <classVars: R> UiT class >> r: x [ R := x ] UiT class >> r [ ^R ] ]")
            .expect("UI scoreboard");
        ui
    }

    /// Bug report: "bug was from repeatedly running new benchmark: walk_frames:
    /// an ENTRY_FRAME_SENTINEL must pair with TierLink::IntoInterpreter, found
    /// IntoCompiled instead" -- a real VM-internal panic (non-unwinding, aborts
    /// the process) after ~20 sequential Cocoa "Benchmark Chart" button clicks.
    ///
    /// Root cause: `Worker uiDoit:`'s relay runs each doit through
    /// `dispatchInbox` -> `dispatchOne:` -> `serveUiDoit:` -> `primEvalDoit:`
    /// (primitive 250) -> `run_method_reentrant` (the SAME nested-interpreter
    /// door `perform:withArguments:`, primitive 64, also uses). Every
    /// compiled-caller -> interpreter crossing must be journaled with a
    /// `TierLink::IntoInterpreter` push (`rt_interpret_call`'s own bracket);
    /// prim 250's was not. That is invisible while `serveUiDoit:`'s call
    /// chain is still interpreted (no anchor, `link_idx == 0`, the walk stops
    /// at `Mode::Done`). Once ~10 relays (`JitMode::Threshold(10)`) compile
    /// that chain, the shimmed prim 250 started running through
    /// `rt_call_primitive` (a COMPILED caller, anchor SET, nothing pushed) --
    /// and the first GC inside the nested doit reached its
    /// `ENTRY_FRAME_SENTINEL` and consumed the tier_links entry belonging to
    /// the OUTER interpreted-doIt -> compiled-`dispatchInbox` crossing
    /// (`compiled_call.rs`'s correctly-paired `IntoCompiled`), panicking on
    /// the variant mismatch. A single continuous CLI loop running the same
    /// Smalltalk never crosses through prim 250, which is why it never
    /// reproduced.
    ///
    /// Fix: prims 250/64 are excluded from primitive shims
    /// (`compiler::driver::PRIM_REENTERS_INTERPRETER`) -- a compiled caller
    /// now reaches them via the c2i adapter, whose `rt_interpret_call`
    /// brackets the crossing correctly. This test drives 40 SEPARATE
    /// sequential relays through the real `Worker uiDoit:` path with JIT on
    /// (`crate::boot::vm_options`), each allocating enough to make a GC
    /// inside the nested run likely; before the fix it aborted the primary
    /// thread at iteration ~15-20 with the exact reported signature.
    #[test]
    fn many_sequential_uidoit_relays_do_not_corrupt_tier_links() {
        let ui_wake: InboxWakeFn = Arc::new(|| {});
        let (_sup, link) = PrimarySupervisor::spawn(boot_fn_jit(), ui_wake)
            .expect("boot the supervised primary (JIT on)");
        let mut ui = boot_ui_jit(&link);

        for _ in 0..40 {
            round_trip(&mut ui, &link, "(Array new: 50000) size > 0", "'true'");
        }
    }

    /// M5 (`docs/package_aware_editing_design.md` §4.4): the live-compile
    /// gate is TWO round trips (check, then reopen) — NOT the design doc's
    /// original one-doit fold (`ifFalse: [ Super subclass: Cls [...] ]`), and
    /// NOT a `.`-separated guard-then-reopen in a single doit either. Both of
    /// those were tried and looked like they worked (no error, a `'nil'`
    /// result matching the established success convention) right up until a
    /// direct `ClassMirror selectorsOf:` check showed the method was never
    /// actually installed. Root cause, found by reading the real compiler
    /// (`src/embed.rs`'s `eval`/`exec`): a doit compiles/runs its SOLE
    /// top-level item (`frontend::parser::parse_one_top_item`, SPEC §16.2) —
    /// a `.`-separated second statement is simply never reached, and
    /// `X subclass: Y [...]` is ALSO a top-level-only special form (nested in
    /// a block it fails to parse: "expected ']' to close block"). So the
    /// check and the reopen must be two separate `Worker uiDoit:onReply:`
    /// calls, the second chained from the first's callback — this pins that
    /// shape against a real primary (`CocoaBrowser acceptMethod`/
    /// `addVarEntered` build the identical two-round-trip shape, just with
    /// the real edited text spliced in; driving THEM needs the full AppKit
    /// view stack, verified separately, on-screen, via `MACVM_COCOA_CTL`).
    #[test]
    fn the_m5_gate_two_round_trips_skip_on_missing_class_and_apply_when_present() {
        let ui_wake: InboxWakeFn = Arc::new(|| {});
        let (_sup, link) =
            PrimarySupervisor::spawn(boot_fn(), ui_wake).expect("boot the supervised primary");
        let mut ui = boot_ui(&link);

        // Missing: the existence check answers false, so the caller never
        // even ships the reopen — no wrong-shape shell gets defined under
        // this name (the §3 bug this gate exists to close).
        round_trip(
            &mut ui,
            &link,
            "(Worker classNamed: #M5GateProbeMissing) notNil",
            "'false'",
        );

        // Present (UndefinedObject — superclass Object — always exists on a
        // booted VM): the check answers true, so the caller ships the
        // reopen as a SEPARATE, second round trip — and the method really
        // is live afterward. (Reopening Object ITSELF needs `nil subclass:
        // Object [...]`, not `Object subclass: Object [...]` — its real
        // superclass is nil, not itself; UndefinedObject sidesteps that
        // unrelated wrinkle while still proving the same thing.)
        round_trip(
            &mut ui,
            &link,
            "(Worker classNamed: #UndefinedObject) notNil",
            "'true'",
        );
        round_trip(
            &mut ui,
            &link,
            "Object subclass: UndefinedObject [ m5GateProbeApplied [ ^1 ] ]",
            "'nil'",
        );
        round_trip(&mut ui, &link, "nil m5GateProbeApplied", "'1'");
    }
}
