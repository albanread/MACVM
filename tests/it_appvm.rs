//! AppSpec A5, end to end and HEADLESS: an app in a VM of its own drives a
//! window on the display worker, with the primary doing nothing but spawn.
//!
//! This test exists because the live path shipped broken while every piece of
//! it was green: the transport worked, the app VM worked, the display's guard
//! had a `doesNotUnderstand` in it (`and:and:`), and the only witness was a
//! stderr line nobody was reading. The whole chain — spawn, introduction,
//! frame, realize, event, re-frame, patch — now runs here, in-process, with
//! no Cocoa and no control channel, so a break in any seam is a red test
//! rather than a quiet drain error.
//!
//! The topology is the GUI's, faithfully: a primary that boots the real world
//! and can spawn (`set_worker_boot`), a hosted worker registered as the
//! display (`register_hosted_worker` + `set_ui_peer`) whose "thread" is this
//! test draining its inbox — exactly main's role in macvm-cocoa — and an app
//! VM the primary spawns, which is introduced to the display at birth
//! (`docs/worker_peer_links.md` §3) and talks to it directly from then on.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use macvm::embed::VmHandle;
use macvm::runtime::{JitMode, VmOptions};

fn opts() -> VmOptions {
    VmOptions {
        heap_mib: 64,
        jit: JitMode::Off,
        ..Default::default()
    }
}

/// Drain the display's inbox into its VM until `done` answers true — the
/// test-thread version of `drain_perform`, with a deadline so a broken seam
/// fails loudly instead of hanging the suite.
fn drain_until(
    ui: &mut VmHandle,
    inbox: &macvm::runtime::workers::HostedInbox,
    what: &str,
    mut done: impl FnMut(&mut VmHandle) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        while let Some(env) = inbox.poll() {
            ui.dispatch_hosted_envelope(env)
                .unwrap_or_else(|e| panic!("display drain error while {what}: {e}"));
        }
        if done(ui) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn eval(vm: &mut VmHandle, src: &str) -> String {
    vm.eval(src)
        .unwrap_or_else(|e| panic!("eval {src:?}: {e}"))
        .trim()
        .to_string()
}

#[test]
fn an_app_in_its_own_vm_drives_a_window_and_a_click_round_trips() {
    // ── the primary: boots the world, can spawn, knows the display ──────
    let mut primary = VmHandle::boot(opts(), Path::new("world")).expect("primary boots");
    primary.set_worker_boot(Arc::new(|| VmHandle::boot(opts(), Path::new("world"))));
    let (ui_id, ui_inbox, to_primary) = primary
        .register_hosted_worker(Arc::new(|| {}))
        .expect("register the display");
    assert!(primary.set_ui_peer(ui_id), "the display is named");

    // ── the display: a worker-role VM whose thread is this test ─────────
    let mut ui = VmHandle::boot(opts(), Path::new("world")).expect("display boots");
    ui.install_worker_role(ui_id, to_primary);
    ui.exec("AppVmDisplay install.").expect("display listens");

    // ── the app: spawned by the primary, introduced at birth ────────────
    primary
        .exec("AppVmHost start: #hx tool: 'WinDemoTool'.")
        .expect("app VM starts");
    assert_eq!(
        eval(&mut primary, "(AppVmHost named: #hx) isAlive printString"),
        "'true'"
    );

    // Its first frame arrives DIRECTLY (the primary is parked in this test
    // doing nothing) and becomes a window on the null realizer.
    drain_until(&mut ui, &ui_inbox, "waiting for the first frame", |ui| {
        eval(ui, "(AppVmDisplay frameFor: #hx) isNil not printString") == "'true'"
    });
    assert_eq!(
        eval(&mut ui, "(AppToolWindow named: #hx) isOpen printString"),
        "'true'",
        "the frame opened a window"
    );
    let placed: i64 = eval(
        &mut ui,
        "(AppToolWindow named: #hx) realizer placedIds size printString",
    )
    .trim_matches('\'')
    .parse()
    .expect("a control count");
    assert!(placed > 10, "the demo tool's face was realized ({placed} controls)");

    // ── a click: display → app VM → handler there → new frame → patch ──
    ui.exec("AppToolWindow resetCounts.").expect("counters zeroed");
    ui.exec("(AppToolWindow named: #hx) dispatch: #bump value: nil.")
        .expect("the event ships");
    drain_until(&mut ui, &ui_inbox, "waiting for the re-frame", |ui| {
        eval(
            ui,
            "[ | out | out := ''. (AppVmDisplay frameFor: #hx) body children do: [ :c | \
               c idOrEmpty = 'stateCard' ifTrue: [ c children do: [ :k | \
                 k idOrEmpty = 'clicks' ifTrue: [ out := k at: #text ] ] ] ]. out ] value",
        ) == "'clicks: 1'"
    });
    // And it PATCHED — the id discipline held across the VM crossing.
    assert_eq!(
        eval(&mut ui, "AppToolWindow rebuildCount printString"),
        "'0'",
        "a re-frame must patch the open window, never rebuild it"
    );
    let patches: i64 = eval(&mut ui, "AppToolWindow patchCount printString")
        .trim_matches('\'')
        .parse()
        .expect("a patch count");
    assert!(patches >= 1, "the caption change arrived as a patch");

    // ── the app dies; the window survives, and says so ──────────────────
    primary.exec("AppVmHost stop: #hx.").expect("app stopped");
    assert_eq!(
        eval(&mut primary, "AppVmHost liveCount printString"),
        "'0'"
    );
    assert_eq!(
        eval(&mut ui, "(AppToolWindow named: #hx) isOpen printString"),
        "'true'",
        "the window outlives the VM behind it — the display keeps the last frame"
    );
    // An event into the corpse is REPORTED, not raised, and the frame stays.
    ui.exec("(AppToolWindow named: #hx) dispatch: #bump value: nil.")
        .expect("an event to a dead app must not take the display down");
    assert_eq!(
        eval(&mut ui, "(AppVmDisplay frameFor: #hx) isNil not printString"),
        "'true'"
    );
}
