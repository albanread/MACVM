//! Cocoa bridge C3 gate (docs/cocoa_bridge_design.md §4 path 2): the sync
//! main-thread hop, proven END TO END against the real main queue.
//!
//! This is a `harness = false` integration test on purpose: libtest runs
//! ordinary `#[test]`s on WORKER threads while the process's real main
//! thread sits parked in `join()` — so nothing drains the main dispatch
//! queue and a genuine `dispatch_sync` hop could never complete under the
//! default harness. Here `main()` IS the real main thread: it boots a VM
//! on a worker thread (the S21 model, exactly the GUI's arrangement),
//! enables the hop, and drains the main queue via `CFRunLoopRunInMode`
//! while the VM thread performs Smalltalk `sendMain:` calls.
//!
//! The proof is self-verifying: `NSThread isMainThread` answers per the
//! CALLING thread — `false` for a plain VM-thread send, `true` through
//! the hop. No AppKit needed (and none is initialized); the mechanism is
//! identical for AppKit calls, which the CocoaPad capstone (C5) will
//! exercise on-screen.

use macvm::embed::VmHandle;
use macvm::runtime::vm_state::VmOptions;
use macvm::runtime::JitMode;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRunLoopRunInMode(
        mode: *const std::os::raw::c_void,
        seconds: f64,
        return_after_source_handled: u8,
    ) -> i32;
    static kCFRunLoopDefaultMode: *const std::os::raw::c_void;
}

fn main() {
    // The inline degenerate case FIRST, from the genuine main thread: a
    // sync hop while already on main must run inline (a Cocoa-callback
    // context can never deadlock itself). Proven through the bridge's own
    // entry point, before any VM exists.
    {
        use macvm::runtime::objc_bridge as ob;
        let cls = ob::class_named("NSThread").expect("Foundation must resolve");
        let out = ob::try_send_full_on_main(
            cls,
            "isMainThread",
            &[0; 6],
            &[0.0; 8],
            &[0; 4],
            ob::RetKind::Gpr,
        )
        .expect("inline-on-main hop must succeed");
        assert_eq!(out.gpr[0] & 0xFF, 1, "on main, isMainThread must be YES");
        let (_, inline0) = ob::hop_stats();
        assert!(inline0 >= 1, "the inline path must have counted");

        // CG2 AppKit main-thread guard (cocoa_gui_design.md §8), the ON-MAIN
        // branch of the ARMED guard — provable only from a genuine main thread
        // (the libtest #[test] gates run off-main and cover the refused branch).
        // Arm Cocoa mode HERE, so this exercises the real native-GUI path (not
        // the inert default): main() IS the real main thread, so an AppKit UI
        // class is ALLOWED (the UI worker is main) even with the guard live.
        // This mirrors CG2's `CocoaUI startup` building NSWindow on main. The
        // later VM-thread sends resolve only Foundation classes off-main, which
        // the guard never blocks — so arming it does not disturb them.
        ob::enable_cocoa_ui_mode();
        assert!(ob::cocoa_ui_mode(), "Cocoa mode must now be armed");
        assert!(ob::on_main_thread(), "this harness=false main() IS main");
        assert!(
            ob::check_appkit_main_thread("NSWindow").is_ok(),
            "on main, AppKit UI classes must be allowed even with the guard armed"
        );
        assert!(ob::check_appkit_main_thread("NSView").is_ok());
        assert!(
            ob::check_appkit_main_thread("NSString").is_ok(),
            "Foundation is always allowed"
        );
    }

    macvm::runtime::objc_bridge::enable_main_hop();

    let done = Arc::new(AtomicBool::new(false));
    let done_vm = done.clone();
    let vm_thread = std::thread::spawn(move || {
        let mut vm = VmHandle::boot(
            VmOptions {
                heap_mib: 64,
                jit: JitMode::Off,
                ..Default::default()
            },
            Path::new("world"),
        )
        .expect("boot against world/");

        // A plain VM-thread send: NOT the main thread.
        let off_main = vm
            .eval("(Cocoa classNamed: 'NSThread') sendBool: 'isMainThread'")
            .expect("plain send");
        assert_eq!(off_main.trim(), "false", "the VM thread is not main");

        // The hop: the SAME question, answered on the main thread.
        let on_main = vm
            .eval("(Cocoa classNamed: 'NSThread') sendMain: 'isMainThread' args: #()")
            .expect("the sync main-thread hop must complete");
        assert_eq!(on_main.trim(), "true", "sendMain: must run ON main");

        // The onMain proxy sugar drives the identical machinery.
        let via_proxy = vm
            .eval("(Cocoa classNamed: 'NSThread') onMain isMainThread")
            .expect("the onMain proxy hop must complete");
        assert_eq!(via_proxy.trim(), "true", "onMain proxy must run ON main");

        // A hopped +0 OBJECT result (the C3 review's cross-thread
        // autorelease finding): processInfo is autoreleased into MAIN's
        // pool; the hop must retain it ON main before returning, so the
        // wrapper stays valid while main keeps draining. Prove it by
        // using the wrapper for further sends, then release it.
        vm.exec("Object subclass: HopT [ <classVars: P> HopT class >> p: x [ P := x ] HopT class >> p [ ^P ] ]")
            .expect("holder");
        vm.exec("HopT p: ((Cocoa classNamed: 'NSProcessInfo') sendMain: 'processInfo' args: #()).")
            .expect("hopped +0 id result");
        let name = vm
            .eval("HopT p sendString: 'processName'")
            .expect("the hopped wrapper must still be alive");
        assert!(
            name.contains("cocoa_main_hop"),
            "processName should be this test binary, got {name}"
        );
        vm.exec("HopT p release.").expect("balanced release");

        // A hopped char* result: UTF8String's buffer is pool-owned on
        // main — the byte COPY must happen inside the hop.
        let s = vm
            .eval("(Cocoa nsString: 'hop bytes') sendMain: 'UTF8String' args: #()")
            .expect("hopped char* result");
        assert!(s.contains("hop bytes"), "got {s}");

        let (dispatched, _) = macvm::runtime::objc_bridge::hop_stats();
        assert!(
            dispatched >= 4,
            "the hops must have DISPATCHED (not run inline): {dispatched}"
        );

        // C4 × C3: a callback FIRED FROM THE REAL MAIN THREAD — exactly
        // what an AppKit button does. The hopped macvmFire: runs the IMP
        // on main; it posts the envelope; the VM dispatches it between
        // doits. The full AppKit wiring, minus the pixels (C5's job).
        vm.set_worker_boot(std::sync::Arc::new(|| {
            VmHandle::boot(
                VmOptions {
                    heap_mib: 64,
                    jit: JitMode::Off,
                    ..Default::default()
                },
                Path::new("world"),
            )
        }));
        vm.exec("Object subclass: HopCb [ <classVars: N A> HopCb class >> reset [ N := 0 ] HopCb class >> bump [ N := N + 1 ] HopCb class >> n [ ^N ] HopCb class >> a: x [ A := x ] HopCb class >> a [ ^A ] ]")
            .expect("holder");
        vm.exec("HopCb reset.").expect("reset");
        vm.exec("HopCb a: (Cocoa action: [ HopCb bump ]).")
            .expect("action");
        vm.exec("HopCb a sendMain: 'macvmFire:' args: (Array with: nil).")
            .expect("fire ON the main thread via the hop");
        vm.exec("Worker dispatchInbox.").expect("dispatch");
        assert_eq!(
            vm.eval("HopCb n").expect("count").trim(),
            "1",
            "a main-thread fire must reach the Smalltalk block"
        );

        done_vm.store(true, Ordering::Release);
    });

    // Drain the main queue until the VM thread finishes (or time out —
    // a hang here IS the failure the test exists to catch).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !done.load(Ordering::Acquire) {
        if std::time::Instant::now() > deadline {
            eprintln!("cocoa_main_hop: FAILED — timed out draining the main queue");
            std::process::exit(1);
        }
        unsafe {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, 0);
        }
    }
    vm_thread.join().expect("the VM thread must exit cleanly");
    println!("cocoa_main_hop: ok (dispatch hop + inline-on-main + onMain proxy)");
}
