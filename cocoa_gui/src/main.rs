//! `macvm-cocoa` — the native AppKit programming environment (the second,
//! flagged GUI mode; `cocoa_gui_design.md`). A UI worker VM pinned to the main
//! thread is a dumb terminal that builds native views from Smalltalk through the
//! Cocoa bridge; the persistent primary VM lives on a background thread. This
//! bin owns the AppKit run loop; the callbacks are Smalltalk's.
//!
//! CG2 opens the window: bootstrap AppKit, run the parked-main boot handshake
//! ([`boot::handshake_wire_vms`]), build ONE `NSWindow` from Smalltalk (`CocoaUI
//! startup`), and enter `[NSApp run]` with the VM at rest. ⌘Q quits clean. The
//! delegates/reverse dispatch (CG3), the request protocol + Workspace/Transcript
//! (CG4), and the browser (CG5+) build on this base.

mod boot;
mod objc;

use std::path::PathBuf;
use std::sync::Arc;

use macvm::embed::FatalMode;
use macvm::runtime::workers::InboxWakeFn;

fn main() {
    // (design §3 step 1) AppKit init MUST be on main, before anything AppKit.
    objc::bootstrap();
    let app = objc::app_shared();
    objc::set_activation_policy_regular(app);

    // The base world + the conditional Cocoa layer. `MACVM_WORLD` overrides the
    // directory (default `world`), mirroring the CLI/GUI convention.
    let world_dir =
        PathBuf::from(std::env::var_os("MACVM_WORLD").unwrap_or_else(|| "world".into()));
    let cocoaui_list = world_dir.join("cocoaui.list");

    // (design §3 steps 2–4) The parked-main boot handshake: spawn the primary on
    // the watchdog thread, park awaiting ready, then boot the UI worker VM in
    // place on THIS main thread (`ExitProcess` — CG0). The run-loop wake is the
    // real main-run-loop poke (CG4 adds the default-mode drain source it feeds).
    let wake: InboxWakeFn = Arc::new(objc::wake_main_runloop);
    let mut wired =
        match boot::handshake_wire_vms(world_dir, cocoaui_list, FatalMode::ExitProcess, wake) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("macvm-cocoa: boot handshake failed: {}", e.msg);
                std::process::exit(1);
            }
        };

    // (design §3 step 4) Publish the thread-local `*mut VmHandle` the CG3
    // callback trampolines will read (a stub now — no trampolines until CG3).
    boot::publish_ui_vm(&mut wired.ui_worker as *mut _);

    // (design §8) Arm the AppKit main-thread guard now the native GUI is live:
    // from here the main thread is AppKit's, and an off-main AppKit send from a
    // background VM is refused. Armed AFTER the boot handshake (which loads no
    // AppKit) so boot behaves exactly as the headless handshake test, and
    // BEFORE the first window build below. This flag is what keeps the guard a
    // no-op for the shipping WKWebView GUI, whose worker VM resolves AppKit
    // classes off-main by design.
    macvm::runtime::objc_bridge::enable_cocoa_ui_mode();

    // (design §3 step 4) Run the Smalltalk startup doit: build ONE NSWindow
    // through the Cocoa bridge and order it front. Runs INLINE on main (the UI
    // worker IS main, so the bridge's `onMain` hop is a no-op), then returns
    // here with the VM at rest.
    if let Err(e) = wired.ui_worker.exec("CocoaUI startup.") {
        eprintln!("macvm-cocoa: CocoaUI startup failed: {e}");
        std::process::exit(1);
    }

    // A minimal main menu with a Quit item (⌘Q → `terminate:`) so the app quits
    // cleanly. The full Smalltalk-built menu bar is CG4 (design §7/R5).
    objc::install_quit_menu(app);

    // (design §3 step 5) Enter `[NSApp run]` with the VM quiescent. Blocks until
    // ⌘Q → `terminate:` → process exit. Every future AppKit callback is a
    // top-level VM entry (CG3).
    objc::activate(app);
    objc::run(app);
}
