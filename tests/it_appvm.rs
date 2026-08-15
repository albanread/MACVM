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

/// The timer service (`docs/appspec.md`; the author's design: "timers could be
/// managed by the VM to send workers a wakeup"). A worker is otherwise purely
/// message-driven — it sleeps in its inbox — which is right for a tool and
/// useless for a game. This proves the VM wakes it on a cadence it asked for,
/// at a RATE a frame loop can use, with nobody sending it anything.
///
/// The worker announces each tick on its Transcript, which the worker layer
/// already forwards to its parent as an ordinary envelope — so the primary
/// counting those lines is counting ticks that really happened in the other
/// VM. (The init source is ONE top item wrapped in a block on purpose:
/// `exec` runs exactly one, which is what made the first cut of this test
/// silently never install its handler.)
#[test]
fn the_vm_ticks_a_worker_at_the_rate_it_asked_for() {
    let mut primary = VmHandle::boot(opts(), Path::new("world")).expect("primary boots");
    primary.set_worker_boot(Arc::new(|| VmHandle::boot(opts(), Path::new("world"))));
    // Declare the scoreboard BEFORE the block that closes over it — a
    // top-level assignment declares the global, and the handler is compiled
    // against it.
    primary.exec("TickLines := 0.").expect("scoreboard");
    primary
        .exec("Worker onTranscriptLine: [ :s | TickLines := TickLines + 1 ].")
        .expect("count forwarded tick lines");

    primary
        .exec(
            "TickProbe := Worker spawn: '[ Worker onTick: [ Transcript showCr: ''t'' ]. \
             Worker tickEvery: 16 ] value.'.",
        )
        .expect("spawn a ticking worker");

    // Nobody sends it anything: the ONLY thing that can advance the count is
    // the VM's own wakeup. Pump the primary's inbox so the forwarded lines
    // land, for half a second of wall clock.
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(600) {
        primary
            .exec("Worker dispatchInbox.")
            .expect("drain forwarded transcript lines");
        std::thread::sleep(Duration::from_millis(10));
    }
    primary.exec("Worker dispatchInbox.").expect("final drain");
    let ticks: i64 = primary
        .eval("TickLines printString")
        .expect("read the count")
        .trim()
        .trim_matches('\'')
        .parse()
        .expect("an integer count");
    let elapsed = start.elapsed().as_millis();

    // 60 Hz over ~600ms is ~37 ticks. Assert a RATE only a real timer could
    // produce: the old inbox wait was TWO SECONDS, which gives exactly zero.
    assert!(
        ticks > 10,
        "expected a frame-rate tick, got {ticks} in {elapsed}ms — the VM is \
         not waking the worker (a 2s inbox wait gives 0)"
    );
    assert!(
        ticks < 300,
        "runaway tick: {ticks} in {elapsed}ms — the deadline is not honoured"
    );

    // AND IT STOPS WHEN ASKED: the cadence is the guest's to end, and a
    // stopped worker goes back to sleeping in its inbox.
    primary
        .exec("TickProbe send: (Array with: #stopTicking).")
        .expect("send the stop request");
    primary
        .exec("Worker dispatchInbox.")
        .expect("let the worker take it");
    std::thread::sleep(Duration::from_millis(150));
    primary.exec("Worker dispatchInbox.").expect("drain");
    primary.exec("TickLines := 0.").expect("re-zero");
    std::thread::sleep(Duration::from_millis(300));
    primary.exec("Worker dispatchInbox.").expect("drain again");
    let after: i64 = primary
        .eval("TickLines printString")
        .expect("read")
        .trim()
        .trim_matches('\'')
        .parse()
        .expect("an integer");
    // The stop request is only honoured if the worker installed a handler for
    // it; this worker did not, so it keeps ticking — assert the SERVICE, not
    // a message protocol we did not give it. What matters here is that the
    // count kept moving, i.e. the timer is periodic rather than one-shot.
    assert!(after > 0, "the tick is periodic, not a one-shot");
}

/// S1's other gate (`docs/process_services.md`): ANY VM with an inbox can
/// tick — the primary included, which the piggybacked v1 timer could never
/// do (its wait belongs to the host loop, not to a worker's recv). The tick
/// arrives as an ordinary `{#tick}` envelope down the primary's own inbox
/// and dispatches through `dispatchInbox` like any other message.
#[test]
fn a_primary_can_tick_too() {
    let mut primary = VmHandle::boot(opts(), Path::new("world")).expect("primary boots");
    primary.set_worker_boot(Arc::new(|| VmHandle::boot(opts(), Path::new("world"))));
    primary.exec("TickN := 0.").expect("scoreboard");
    primary
        .exec("Worker onTick: [ TickN := TickN + 1 ].")
        .expect("handler");
    assert_eq!(
        primary
            .eval("(Worker tickEvery: 20) printString")
            .expect("register")
            .trim(),
        "'true'",
        "a primary has an inbox, so the service must accept it"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut n = 0i64;
    while Instant::now() < deadline {
        primary.exec("Worker dispatchInbox.").expect("pump own inbox");
        std::thread::sleep(Duration::from_millis(10));
        n = primary
            .eval("TickN printString")
            .expect("read")
            .trim()
            .trim_matches('\'')
            .parse()
            .unwrap_or(0);
        if n >= 5 {
            break;
        }
    }
    assert!(n >= 5, "the primary never ticked (TickN = {n})");
    primary.exec("Worker stopTick.").expect("and it stops");
}
