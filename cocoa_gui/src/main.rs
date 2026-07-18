//! `macvm-cocoa` — the native AppKit programming environment (the second,
//! flagged GUI mode; `cocoa_gui_design.md`). A UI worker VM pinned to the main
//! thread is a dumb terminal that builds native views from Smalltalk through the
//! Cocoa bridge; the persistent primary VM lives on a background thread under a
//! watchdog. This bin owns the AppKit run loop; the callbacks are Smalltalk's.
//!
//! CG4 wires the environment: the watchdog [`supervisor::PrimarySupervisor`]
//! boots the primary on its own thread and respawns it from source on a fatal
//! doit; the UI worker ships Workspace selections as `#uiReq`/`#doit` requests,
//! the primary evaluates them and replies, and a **default-mode** `CFRunLoopSource`
//! drains the UI worker's inbox on the main thread (§8). ⌘Q quits clean.

mod boot;
mod canvas;
mod colorize;
mod control;
mod game;
mod host_service;
mod objc;
mod rebuild;
mod snapshot;
mod supervisor;
mod view_refresh;

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Arc;

use macvm::embed::{FatalMode, VmHandle};
use macvm::runtime::workers::{HostedInbox, InboxSender, InboxWakeFn, WorkerBootFn};

use supervisor::{PrimaryLink, PrimarySupervisor};

/// The main-thread-owned drain state the default-mode `CFRunLoopSource`'s
/// `perform` reads (its raw pointer is the source's `info`). It owns the UI
/// worker VM and the supervisor, and holds the CURRENT primary→UI inbox — swapped
/// wholesale on a re-sync. Only ever touched on the main thread (the run loop is
/// serial there), so the raw-pointer aliasing with `publish_ui_vm`'s pointer is
/// never a concurrent access.
struct DrainState {
    ui: VmHandle,
    inbox: HostedInbox,
    supervisor: PrimarySupervisor,
    /// CG5: the previous tick's `bytes_allocated`, to derive a per-tick alloc
    /// RATE (the toolbar shows B/s, `VmMetrics` only carries the running
    /// total). `None` before the first sample (no rate yet, not zero).
    prev_alloc: Option<u64>,
    /// The RUSTTCL control channel's request queue (`MACVM_COCOA_CTL`), served
    /// on the main thread each drain pass. `None` when the channel is off.
    ctl: Option<std::sync::mpsc::Receiver<control::CtlReq>>,
    /// CG9: the ingredients to rebuild the UI worker in place — the current
    /// hosted peer id + upstream sender (re-adopted by a fresh VM), and the
    /// world paths to boot from. Updated on a primary re-sync too.
    hosted_id: u32,
    to_primary: InboxSender,
    world_dir: PathBuf,
    cocoaui_list: PathBuf,
}

/// `1536` -> `"1.5K"`, base-1024, one decimal past the first suffix — the
/// same compact style the WKWebView GUI's own toolbar uses for MEM/ALLOC.
fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n}{}", UNITS[0])
    } else {
        format!("{v:.1}{}", UNITS[u])
    }
}

/// Sample the primary's live `VmMetrics` and push a formatted readout — plus a
/// used/capacity percentage for the toolbar's MEM bar — into the toolbar (CG5):
/// `CocoaUI updateMetricsMem:jit:code:alloc:gc:memPct:`. Called from
/// `drain_perform`, which now fires ~4Hz regardless of real UI traffic (the
/// supervisor's beat loop wakes unconditionally) — a free, already-proven
/// tick, not a new NSTimer.
fn refresh_metrics(st: &mut DrainState) {
    let m = st.supervisor.metrics();
    // `old_committed` (the actually-mapped, currently-usable portion), NOT
    // `old_reserved` (the full virtual-address reservation upfront for
    // growth-without-remap — often gigabytes even on a small idle VM, whose
    // formatted string overflowed the toolbar label and got clipped).
    let mem = format!(
        "{}/{}",
        format_bytes(m.eden_used + m.old_used),
        format_bytes(m.eden_capacity + m.old_committed)
    );
    let jit = if m.compilations == 0 {
        "—".to_string()
    } else {
        format!("{}c", m.compilations)
    };
    let code = format!("{} nm", m.nmethods);
    let alloc_rate = match st.prev_alloc {
        Some(prev) => {
            let delta = m.bytes_allocated.saturating_sub(prev);
            // One beat is PUMP_BEAT_MS (250ms); *4 approximates bytes/sec.
            format!("{}/s", format_bytes(delta.saturating_mul(4)))
        }
        None => "—".to_string(),
    };
    st.prev_alloc = Some(m.bytes_allocated);
    let gc = format!("{}·{}", m.scavenges, m.full_gcs);
    let cap = (m.eden_capacity + m.old_committed).max(1);
    let used = m.eden_used + m.old_used;
    let mem_pct = ((used as f64 / cap as f64) * 100.0).round().clamp(0.0, 100.0) as i64;

    let doit = format!(
        "CocoaUI updateMetricsMem: '{mem}' jit: '{jit}' code: '{code}' alloc: '{alloc_rate}' gc: '{gc}' memPct: {mem_pct}."
    );
    let _ = st.ui.exec(&doit);
}

/// The default-mode drain `perform` (CG4 §8): apply any pending re-sync (re-point
/// the UI worker onto a freshly respawned primary, swap its drain inbox), then
/// drain inbound `#uiReply`/`#snapshot`/transcript envelopes into the UI worker.
/// Runs on the main thread ONLY in `NSDefaultRunLoopMode`, so a snapshot never
/// swaps mid-tracking/modal.
extern "C" fn drain_perform(info: *mut c_void) {
    // SAFETY: `info` is the `&mut *Box<DrainState>` main installed; this runs on
    // the main thread where nothing else borrows it concurrently.
    let st = unsafe { &mut *(info as *mut DrainState) };

    // (CG9) Service a UI-worker rebuild request FIRST, before any envelope
    // dispatch — we run here on the main thread with the VM quiescent (never
    // inside a callback), the only place it is sound to drop the UI worker's
    // VmHandle. A request set from inside a callback only flags + wakes us.
    if rebuild::take_request() {
        rebuild_ui(st);
        return; // the fresh generation drains on the next pass
    }

    // View refreshes (Browser tree, Find options, Outliner tree): a fresh
    // top-level `exec` here, never inside a callback — see view_refresh.rs.
    let (browser_due, find_due, outliner_due) = view_refresh::take_requests();
    if browser_due {
        let _ = st.ui.exec("CocoaBrowser doRefresh.");
    }
    if find_due {
        let _ = st.ui.exec("CocoaFind doRefreshOptions.");
    }
    if outliner_due {
        let _ = st.ui.exec("CocoaOutliner doRefresh.");
    }

    while let Some(link) = st.supervisor.poll_resync() {
        // Drain the DYING generation's inbox before dropping it (CG4 review):
        // its last buffered envelopes — the fatal doit's transcript line, any
        // reply that landed before it died — are delivered now, else they are
        // lost with the old inbox. Replies whose continuations DID land fire
        // here; the never-answerable ones are abandoned by `environmentRestarted`
        // below.
        while let Some(env) = st.inbox.poll() {
            let _ = st.ui.dispatch_hosted_envelope(env);
        }
        // Re-sync onto the fresh primary generation: re-point the reply link +
        // swap the drain inbox (never a torn mix — replace both together).
        st.ui
            .install_worker_role(link.hosted_id, link.to_primary.clone());
        st.hosted_id = link.hosted_id;
        st.to_primary = link.to_primary.clone();
        st.inbox = link.hosted_inbox;
        // Let the Smalltalk terminal recover clean (abandon dead-primary
        // continuations) and note the restart.
        let _ = st.ui.exec("CocoaUI environmentRestarted.");
    }
    while let Some(env) = st.inbox.poll() {
        if let Err(e) = st.ui.dispatch_hosted_envelope(env) {
            eprintln!("macvm-cocoa: UI worker drain error: {e}");
        }
    }
    if let Some(rx) = &st.ctl {
        control::serve(rx, &mut st.ui);
    }
    // (CG10) Service a stop request (close the game window on main), then apply
    // any game commands the primary's sink reported. The frame STEP itself runs
    // top-level on the primary's own supervisor loop (never a nested VM entry —
    // that trips the frame-walk invariant under GC).
    game::service_stop_on_main();
    game::drain();
    refresh_metrics(st);
}

/// (CG9 §5 Layer 2) Rebuild the UI worker in place: ordered teardown of the old
/// window set, a clean drop of the old `VmHandle`, a fresh UI worker re-adopting
/// the SAME hosted peer id, and `CocoaUI startup` again — the primary and all
/// its state untouched throughout. Runs on the main thread, VM quiescent (from
/// `drain_perform`, never a callback). The Layer-3 backstop trips first on a
/// rebuild storm.
fn rebuild_ui(st: &mut DrainState) {
    eprintln!("macvm-cocoa: rebuilding the UI worker in place…");
    // (Layer 3) A storm trips ExitProcess before it can recurse.
    rebuild::note_and_check_backstop();

    // 1. No callback may dispatch into the VM while we tear it down and drop it:
    //    unpublish the door (ui_vm() → null → every trampoline fails closed).
    boot::publish_ui_vm(std::ptr::null_mut());

    // 2. Ordered teardown of the OLD window set (orderOut/close/release windows
    //    + delegates + tickets — CocoaUI teardown is idempotent). Best-effort:
    //    if the compromised VM faults here too, the per-exec sigsetjmp recovers
    //    it to an Err and we press on — the drop below reclaims everything the
    //    VM owned regardless.
    let _ = st.ui.exec("CocoaUI teardown.");

    // 3. Drain + discard the OLD generation's inbox (its buffered snapshots /
    //    transcript lines reference the dead UI's state).
    while st.inbox.poll().is_some() {}

    // 4. Boot a FRESH UI worker re-adopting the same hosted id + upstream sender
    //    (the channel is Rust-owned, unaffected by the VM swap), then assign —
    //    which DROPS the old handle here (Reservation munmap + deopt deregister +
    //    setjmp-slot release; proven leak-free in embed's restart-lifecycle
    //    gate). Boot failure is genuinely fatal — there is no GUI without a UI
    //    worker.
    let fresh = match boot::boot_ui_worker(
        &st.world_dir,
        &st.cocoaui_list,
        FatalMode::ExitProcess,
        st.hosted_id,
        st.to_primary.clone(),
    ) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("macvm-cocoa: UI worker rebuild boot failed: {} — ExitProcess", e.msg);
            std::process::exit(71);
        }
    };
    st.ui = fresh; // old VmHandle drops here

    // 5. Republish the fresh door (bumps the UI-VM generation, so any old C6
    //    delegate ticket still held by a not-yet-freed AppKit object fails
    //    closed), then rebuild the window set.
    boot::publish_ui_vm(&mut st.ui as *mut _);
    if let Err(e) = st.ui.exec("CocoaUI startup.") {
        eprintln!("macvm-cocoa: CocoaUI startup after rebuild failed: {e} — ExitProcess");
        std::process::exit(72);
    }
    eprintln!("macvm-cocoa: UI worker rebuilt; primary state intact.");
}

fn main() {
    // (design §3 step 1) AppKit init MUST be on main, before anything AppKit.
    objc::bootstrap();
    // An explicit autorelease pool around ALL pre-`[NSApp run]` work: before the
    // run loop is live there is no CF/AppKit pool on main, so the window/menu
    // build's autoreleased objects would leak "with no pool in place". Drained
    // just before `[NSApp run]`, whose own per-event/per-callout pools take over.
    // (The bridge's bottom pool is disabled on main — it would corrupt CF's pool
    // stack; see `objc_bridge::ensure_pool`.)
    let startup_pool = objc::autorelease_pool_push();
    let app = objc::app_shared();
    objc::set_activation_policy_regular(app);

    // The base world + the conditional Cocoa layer. `MACVM_WORLD` overrides the
    // directory (default `world`), mirroring the CLI/GUI convention.
    let world_dir =
        PathBuf::from(std::env::var_os("MACVM_WORLD").unwrap_or_else(|| "world".into()));
    let cocoaui_list = world_dir.join("cocoaui.list");

    // The primary's boot-from-source closure — the watchdog respawns the primary
    // from it on a fatal doit (§5.1). Same world both generations (and compute
    // workers) boot from.
    let world_for_boot = world_dir.clone();
    let world_boot: WorkerBootFn =
        Arc::new(move || VmHandle::boot(boot::vm_options(), &world_for_boot));

    // The UI worker inbox's run-loop poke: signals the default-mode drain source
    // and wakes the main run loop (§8). Fired by a primary generation whenever it
    // `send`s the UI worker, and once per re-sync.
    let wake: InboxWakeFn = Arc::new(objc::wake_main_runloop);

    // (design §5.1) Boot the supervised primary on its own thread under the
    // watchdog; get the first primary→UI link.
    let (sup, link): (PrimarySupervisor, PrimaryLink) =
        match PrimarySupervisor::spawn(world_boot, wake) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("macvm-cocoa: primary boot failed: {}", e.msg);
                std::process::exit(1);
            }
        };

    // (design §3 step 4) Boot the UI worker VM in place on THIS main thread
    // (`ExitProcess` — CG0), layer the Cocoa world, take on its Worker role.
    let ui = match boot::boot_ui_worker(
        &world_dir,
        &cocoaui_list,
        FatalMode::ExitProcess,
        link.hosted_id,
        link.to_primary.clone(),
    ) {
        Ok(ui) => ui,
        Err(e) => {
            eprintln!("macvm-cocoa: UI worker boot failed: {}", e.msg);
            std::process::exit(1);
        }
    };

    // The RUSTTCL control channel (opt-in, loopback): its requests ride the
    // same main-thread drain as everything else.
    let ctl = control::start(Arc::new(objc::wake_main_runloop));

    // Own the UI worker + supervisor + current inbox in one heap box, whose stable
    // address is the drain source's `info`.
    let mut drain = Box::new(DrainState {
        ui,
        inbox: link.hosted_inbox,
        supervisor: sup,
        prev_alloc: None,
        ctl,
        // CG9 rebuild ingredients — the current peer id + sender the fresh VM
        // re-adopts, and the world paths to boot it from.
        hosted_id: link.hosted_id,
        to_primary: link.to_primary.clone(),
        world_dir: world_dir.clone(),
        cocoaui_list: cocoaui_list.clone(),
    });

    // (design §3 step 4) Publish the thread-local `*mut VmHandle` the CG3/CG4
    // callback trampolines read (the Workspace ⌘P/⌘D action target dispatches
    // through it).
    boot::publish_ui_vm(&mut drain.ui as *mut _);

    // (design §8) Arm the AppKit main-thread guard now the native GUI is live.
    macvm::runtime::objc_bridge::enable_cocoa_ui_mode();

    // (design §8) Install the UI worker's inbox drain as a DEFAULT-MODE
    // CFRunLoopSource; `wake_main_runloop` signals it. Its `info` is the stable
    // heap address of the drain state.
    let info = &mut *drain as *mut DrainState as *mut c_void;
    objc::install_default_mode_drain(info, drain_perform);

    // (CG7) The host-service class the browser's source pane resolves by name —
    // registered before `CocoaUI startup` so `Cocoa classNamed:` always finds it.
    host_service::register();

    // The app menu (⌘Q → `terminate:`) + the quit-on-last-window-close delegate
    // MUST be installed BEFORE `CocoaUI startup`: startup's `installMenu` ADDS
    // its Workspace submenu (⌘P/⌘D) to the EXISTING main menu. If the app menu
    // isn't there first, startup makes its own main menu and this call's
    // `setMainMenu:` would CLOBBER it — the ⌘P-beeps bug. Order: app menu first,
    // Workspace second.
    objc::install_quit_menu(app);
    objc::install_app_delegate(app);

    // (design §3 step 4) Build the window + Workspace/Transcript views + the
    // Workspace menu from Smalltalk (added to the app menu above), then return
    // with the VM at rest.
    if let Err(e) = drain.ui.exec("CocoaUI startup.") {
        eprintln!("macvm-cocoa: CocoaUI startup failed: {e}");
        std::process::exit(1);
    }

    // (design §3 step 5) Enter `[NSApp run]` with the VM quiescent. Every future
    // AppKit callback is a top-level VM entry; the drain fires in default mode.
    objc::activate(app);
    // Drain the startup pool; from here CF's own run-loop pools own the main
    // thread's autorelease lifecycle.
    objc::autorelease_pool_pop(startup_pool);
    // Dev aid: if MACVM_COCOA_SNAP is set, capture a timestamped PNG sequence of
    // the client area from inside the app (so on-screen work is inspectable).
    snapshot::start();
    objc::run(app);

    // Keep the drain state (UI worker VM, supervisor) alive for the whole run.
    drop(drain);
}
