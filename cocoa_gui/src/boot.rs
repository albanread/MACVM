//! The parked-main boot handshake (`cocoa_gui_design.md` §3, sprint CG2).
//!
//! The chicken-and-egg — *the primary registers the UI worker, but the UI
//! worker must run on the main thread the process started on* — is resolved by
//! booting the UI worker VM **in place on main** and letting Rust own the run
//! loop. This module holds the machine-checkable half of that: the two-VM
//! wiring up to (but not entering) `[NSApp run]`. It touches **no AppKit** — the
//! caller (`main.rs`) does the AppKit bootstrap, builds the window from
//! Smalltalk, and enters the run loop — so [`handshake_wire_vms`] is unit-
//! testable headless (no window server), which is exactly what the CG2 gate
//! asks of it.
//!
//! Boot sequence (design §3):
//! 1. *(main)* AppKit bootstrap + `[NSApplication sharedApplication]` — in
//!    `main.rs`, before this.
//! 2. *(main)* Spawn the **primary VM on a background (watchdog) thread**, then
//!    park awaiting the primary's "ready" signal — [`handshake_wire_vms`] does
//!    both (the park is the blocking `recv` on the ready channel).
//! 3. *(background)* The primary boots the world, becomes a primary
//!    ([`VmHandle::set_worker_boot`]), **registers the UI worker as an
//!    externally-hosted peer** (CG1, no `thread::spawn`), and signals ready with
//!    the boot payload.
//! 4. *(main)* Boots the **UI worker VM in place** (`FatalMode::ExitProcess`,
//!    CG0), loads the conditional `cocoaui.list` layer, takes on its Worker role
//!    (so its future replies reach the primary), and hands the caller a live
//!    [`WiredVms`]. The caller then publishes the thread-local `*mut VmHandle`
//!    ([`publish_ui_vm`]) and runs `CocoaUI startup`.
//! 5. *(main, Rust)* `main.rs` calls `[NSApp run]` with the VM at rest.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

use macvm::embed::{FatalMode, VmHandle};
use macvm::runtime::workers::{HostedInbox, InboxSender, InboxWakeFn};
use macvm::runtime::{JitMode, VmError, VmOptions};

/// Address-space reservation for each VM, in MiB (reservation, not commitment —
/// the world boots inside a small committed working set). Overridable per
/// `VmOptions::from_env`'s `MACVM_HEAP`.
const DEFAULT_HEAP_MIB: usize = 512;

/// The two wired VMs handed back to `main.rs` after the handshake, with the run
/// loop **not yet entered**. `ui_worker` is booted in place on the current
/// (main) thread; the primary lives on `primary_thread`.
///
/// `hosted_id`/`hosted_inbox`/`primary_thread` are the primary↔UI wiring the
/// CG4 drain + supervisor consume; in CG2 the bin only holds them alive (the
/// handshake gate reads them). `#[allow(dead_code)]` marks them as that
/// forward-looking, held-alive surface rather than accidental cruft.
#[allow(dead_code)]
pub struct WiredVms {
    /// The UI worker VM (the dumb terminal), booted in place on the current
    /// thread. The caller drives it — runs `CocoaUI startup`, then rests while
    /// `[NSApp run]` pumps.
    pub ui_worker: VmHandle,
    /// The UI worker's id in the primary's registry (`Worker spawn`'s id-space).
    pub hosted_id: u32,
    /// The channel the UI worker drains for primary→UI traffic (snapshot blasts,
    /// doit replies — CG4). In CG2 the only thing on it is the boot connectivity
    /// poke.
    pub hosted_inbox: HostedInbox,
    /// The (detached) background thread hosting the persistent primary VM. It is
    /// parked for the process lifetime (S21: never `.join()` a VM thread); the
    /// CG4 supervisor loop replaces the park.
    pub primary_thread: JoinHandle<()>,
}

/// VM options for a Cocoa-GUI VM — `VmOptions::from_env` (so
/// `MACVM_JIT`/`MACVM_HEAP`/… are honored) with the JIT defaulting to ON when
/// `MACVM_JIT` is unset. The GUI runs real programs interactively and **the
/// embedded VM must support the JIT** (`feedback_jit_must_be_supported_embedded`)
/// — mirrors `gui::vm_host::gui_vm_options`. `MACVM_JIT=off` still overrides.
fn vm_options() -> VmOptions {
    let mut opts = VmOptions::from_env();
    if opts.heap_mib == VmOptions::default().heap_mib && std::env::var_os("MACVM_HEAP").is_none() {
        opts.heap_mib = DEFAULT_HEAP_MIB;
    }
    if std::env::var_os("MACVM_JIT").is_none() {
        opts.jit = JitMode::Threshold(10);
    }
    opts
}

/// Wire the primary (background) and UI worker (in place, on the current thread)
/// VMs and return once both are up and linked — **without** touching AppKit or
/// entering the run loop (design §3 steps 2–4). This is the CG2 handshake's
/// machine-checkable seam.
///
/// * `world_dir` — the base world (`world/`) both VMs boot from.
/// * `cocoaui_list` — the conditional Cocoa GUI layer (`world/cocoaui.list`),
///   loaded ONLY onto the UI worker (files 63+, §12.3).
/// * `ui_fatal_mode` — the UI worker's fatal policy: `FatalMode::ExitProcess`
///   in the bin (CG0 — a true fatal on main must exit the process, not zombify
///   the UI thread); the test passes `FatalMode::ExitThread` to stay harness-safe.
/// * `wake` — the UI worker inbox's run-loop poke, fired (coalesced) whenever the
///   primary `send`s it: `objc::wake_main_runloop` in the bin, a counter in the
///   gate.
///
/// Returns `Err` if either VM fails to boot or the primary cannot register the
/// UI worker — a clean error the caller reports, never a hang or a partial run
/// loop.
pub fn handshake_wire_vms(
    world_dir: PathBuf,
    cocoaui_list: PathBuf,
    ui_fatal_mode: FatalMode,
    wake: InboxWakeFn,
) -> Result<WiredVms, VmError> {
    // The primary's "ready" payload: the UI worker id + the channel to drain +
    // the reply link back to the primary. All `Send` (the whole point — the
    // primary mints them on its thread, main receives them on the run-loop
    // thread).
    #[allow(clippy::type_complexity)]
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(u32, HostedInbox, InboxSender), VmError>>();

    // (design §3 step 2) Spawn the primary on the watchdog thread; park here on
    // the ready channel. The primary drives its own thread — it is never
    // `.join()`ed (S21), so this JoinHandle is only a liveness token.
    let world_for_primary = world_dir.clone();
    let primary_thread = std::thread::Builder::new()
        .name("macvm-cocoa-primary".into())
        .spawn(move || primary_thread_main(world_for_primary, wake, ready_tx))
        .map_err(|e| VmError {
            msg: format!("could not spawn the primary VM thread: {e}"),
        })?;

    // Park until the primary is up and serving (or reports a boot failure /
    // dies before signalling).
    let (hosted_id, hosted_inbox, to_primary) = match ready_rx.recv() {
        Ok(Ok(payload)) => payload,
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(VmError {
                msg: "the primary VM thread died before signalling ready".into(),
            })
        }
    };

    // (design §3 step 4) Boot the UI worker VM IN PLACE on THIS thread. In the
    // bin this is main; boot must run on the driving thread (its foreign-fault
    // handler + sigaltstack are thread-scoped).
    let mut ui_worker = VmHandle::boot(vm_options(), &world_dir).map_err(|e| VmError {
        msg: format!("UI worker boot failed: {}", e.msg),
    })?;
    // CG0: on the main thread a true fatal (heap exhaustion, stack overflow)
    // must exit the PROCESS, not `pthread_exit` main into a headless zombie.
    // `boot` armed `ExitThread`; flip it now. (The test passes `ExitThread` to
    // stay harness-safe — an ordinary guest `error:` is recovered by the
    // default `ErrorPolicy::Resume` and never reaches this anyway.)
    macvm::embed::set_fatal_mode(ui_fatal_mode);
    // The conditional Cocoa GUI world layer (CocoaUI + the view classes, files
    // 63+), loaded ONLY here — the CLI, the WKWebView GUI, and the base test
    // suite carry none of it.
    ui_worker.load_list(&cocoaui_list).map_err(|e| VmError {
        msg: format!("loading {} failed: {}", cocoaui_list.display(), e.msg),
    })?;
    // Take on the Worker role so the UI worker's future `reply:`/`send:` reach
    // the primary (CG4) — the same wiring a spawned `worker_main` installs.
    ui_worker.install_worker_role(hosted_id, to_primary);

    Ok(WiredVms {
        ui_worker,
        hosted_id,
        hosted_inbox,
        primary_thread,
    })
}

/// The primary (watchdog) thread body: boot the persistent world, become a
/// primary, register the UI worker, signal ready, poke the link once, then park.
fn primary_thread_main(
    world_dir: PathBuf,
    wake: InboxWakeFn,
    ready_tx: mpsc::Sender<Result<(u32, HostedInbox, InboxSender), VmError>>,
) {
    // (design §3 step 3) Boot the persistent primary VM — the environment's
    // state. `ExitThread` (boot's default) is right here: the CG4 supervisor
    // respawns the primary on a fatal doit.
    let mut primary = match VmHandle::boot(vm_options(), &world_dir) {
        Ok(h) => h,
        Err(e) => {
            let _ = ready_tx.send(Err(VmError {
                msg: format!("primary boot failed: {}", e.msg),
            }));
            return;
        }
    };
    // Installing a worker-boot fn makes this VM the PRIMARY (creates its inbox +
    // registry) — required before `register_hosted_worker`, and it lets the
    // primary spawn compute workers later (CG8). A spawned compute worker boots
    // the same world.
    let boot_world = world_dir.clone();
    primary.set_worker_boot(Arc::new(move || VmHandle::boot(vm_options(), &boot_world)));

    // (design §3 step 3) Register the UI worker as an externally-hosted peer
    // (CG1) — no `thread::spawn`; its thread is main. `wake` fires whenever the
    // primary `send`s it.
    let Some((id, hosted_inbox, to_primary)) = primary.register_hosted_worker(wake) else {
        let _ = ready_tx.send(Err(VmError {
            msg: "register_hosted_worker failed (not a primary, or the fleet is at its cap)".into(),
        }));
        return;
    };

    // Signal ready: hand main the id + the drain channel + the reply link.
    if ready_tx.send(Ok((id, hosted_inbox, to_primary))).is_err() {
        return; // main gone — nothing to serve
    }

    // Boot connectivity poke: exercise the primary→UI link + its run-loop wake
    // once, right after registration — fail-fast if the wiring is broken, and
    // the exact path the design blasts initial snapshots along in CG4. Empty
    // bytes = a bare nudge (CG4's drain treats a payload-less envelope as a
    // no-op). This is what makes the CG2 gate's "the two VMs are wired"
    // assertion concrete.
    primary.send_to_worker(id, 0, Vec::new());

    // (CG2) Park, holding the primary VM alive for the process lifetime. The
    // CG4 supervisor loop — drain the inbox, dispatch `#uiReq`, respawn on a
    // fatal doit — replaces this park. Process exit (⌘Q → `terminate:`) tears
    // the thread down; `primary` (a `VmHandle` with a `Drop`) stays live in
    // scope until then.
    loop {
        std::thread::park();
    }
}

// ── The thread-local `*mut VmHandle` the callback trampolines read (CG3) ──────
//
// design §3 step 4: the UI worker publishes a thread-local raw pointer to its
// own `VmHandle` so an AppKit→Smalltalk callback trampoline (C6 reverse
// dispatch, `macvm::runtime::objc_delegate`, CG3) can read it and dispatch as a
// top-level `eval`/`perform` entry. The CANONICAL thread-local lives in the core
// `macvm` crate (`macvm::embed`) — a core trampoline cannot reach a
// `cocoa_gui`-crate thread-local, and a headless core gate must publish it. These
// are thin re-exports so `main.rs` keeps calling `boot::publish_ui_vm`.

/// Publish this thread's UI worker `VmHandle` for the CG3 callback trampolines
/// (design §3 step 4) — the core [`macvm::embed::publish_ui_vm`]. Call on the
/// main thread after the handshake, before running `CocoaUI startup` / entering
/// the run loop. Bumps the UI-VM generation (stale delegates from a prior UI
/// worker then fail closed — design §4.3).
pub fn publish_ui_vm(p: *mut VmHandle) {
    macvm::embed::publish_ui_vm(p);
}

/// The main thread's published UI worker `VmHandle` pointer, or null if none —
/// the door a CG3 trampoline reads ([`macvm::embed::ui_vm`]). Null-safe.
#[allow(dead_code)]
pub fn ui_vm_ptr() -> *mut VmHandle {
    macvm::embed::ui_vm()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// `world/` at the workspace root, resolved from THIS crate's manifest dir
    /// so the test is cwd-independent (`cargo test -p cocoa_gui` runs with cwd =
    /// `cocoa_gui/`, not the workspace root).
    fn world_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../world")
    }

    /// CG2 boot-handshake gate (sprint CG2, design §3): the primary-spawn +
    /// register-hosted-worker + ready-signal sequence up to (but NOT entering)
    /// the run loop — asserting the two VMs are wired and the UI worker got its
    /// id, on a machine with no window server. Deliberately stops before
    /// `[NSApp run]` and never builds a window (that is the user's on-screen
    /// gate); it touches no AppKit at all.
    #[test]
    fn boot_handshake_wires_both_vms_headless() {
        let wd = world_dir();
        let list = wd.join("cocoaui.list");

        // The run-loop wake stands in for `objc::wake_main_runloop` (the bin);
        // here it counts fires so the gate can prove the primary→UI link woke.
        let wakes = Arc::new(AtomicUsize::new(0));
        let wakes_hook = wakes.clone();
        let wake: InboxWakeFn = Arc::new(move || {
            wakes_hook.fetch_add(1, Ordering::Relaxed);
        });

        // `ExitThread` (not `ExitProcess`) so a hypothetical fatal cannot take
        // the whole test binary down — the bin uses `ExitProcess`.
        let mut wired = handshake_wire_vms(wd.clone(), list, FatalMode::ExitThread, wake)
            .expect("the two-VM boot handshake must complete cleanly headless");

        // The UI worker got its id (the primary registered it — only a real
        // primary can, so this alone proves the primary booted + became a
        // primary + minted the peer).
        assert!(
            wired.hosted_id >= 1,
            "the UI worker must be assigned a real worker id, got {}",
            wired.hosted_id
        );

        // The two VMs are wired: the primary's boot connectivity poke rode the
        // primary→UI link into the UI worker's inbox and fired the run-loop
        // wake. Poll the inbox (bounded) — the envelope is enqueued before the
        // wake, so waiting for it and then for the wake proves both.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got_poke = false;
        while Instant::now() < deadline {
            if let Some(env) = wired.hosted_inbox.poll() {
                assert_eq!(
                    env.from, 0,
                    "the boot poke must come from the primary (id 0)"
                );
                got_poke = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            got_poke,
            "the primary's boot connectivity poke must reach the UI worker's inbox \
             (the primary→UI link is live)"
        );
        // The wake fired for that poke (it runs right after the enqueue).
        while Instant::now() < deadline && wakes.load(Ordering::Relaxed) == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            wakes.load(Ordering::Relaxed) >= 1,
            "the primary→UI send must fire the run-loop wake"
        );

        // The UI worker VM is genuinely live IN PLACE on this thread.
        assert_eq!(
            wired
                .ui_worker
                .eval("3 + 4")
                .expect("the UI worker must evaluate a doit")
                .trim(),
            "7"
        );

        // The conditional `cocoaui.list` layer loaded onto the UI worker: the
        // CG1 stub marker AND the CG2 CocoaUI class both resolve (proving
        // `load_list` layered files 63 AND 64). No window is built — only the
        // class's title accessor is read, which touches no AppKit.
        assert_eq!(
            wired
                .ui_worker
                .eval("CocoaUIStub ping")
                .expect("CocoaUIStub (file 63) must be present on the UI worker")
                .trim(),
            "#cocoaUiStubReady"
        );
        let title = wired
            .ui_worker
            .eval("CocoaUI title")
            .expect("CocoaUI (file 64) must be present on the UI worker");
        assert!(
            title.contains("MACVM"),
            "CocoaUI title must be the Smalltalk-supplied window title, got {title}"
        );

        // The primary thread is parked (never joined — S21: an `is_finished`/
        // `join` on a VM thread hangs if it died via `pthread_exit`); its
        // liveness is already proven above (it booted, registered the peer, and
        // its boot poke arrived). Dropping `wired` detaches it — exactly the
        // pre-`[NSApp run]` state the bin enters the run loop in.
        drop(wired);
    }

    /// The thread-local `*mut VmHandle` (CG3 stub) round-trips: null by default,
    /// readable after publish. Nothing reads it in CG2, but the door the
    /// trampolines will use must be wired.
    #[test]
    fn ui_vm_pointer_publishes_and_reads_back() {
        assert!(ui_vm_ptr().is_null(), "unpublished, the pointer is null");
        let mut sentinel: usize = 0;
        let p = (&mut sentinel as *mut usize).cast::<VmHandle>();
        publish_ui_vm(p);
        assert_eq!(ui_vm_ptr(), p, "the published pointer reads back");
        publish_ui_vm(std::ptr::null_mut());
        assert!(ui_vm_ptr().is_null(), "it can be cleared again");
    }
}
