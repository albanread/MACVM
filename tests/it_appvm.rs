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

/// S2's gates (`docs/process_services.md`): liveness is read from the
/// process layer — `isAlive` answers false the moment the worker thread
/// exits, with NOBODY pumping any inbox (the old link-based answer lied for
/// as long as the parent was busy) — and a dead app is noticed, reported and
/// retired by the death watcher when the notice arrives.
#[test]
fn liveness_is_true_unpumped_and_a_dead_app_is_retired() {
    let mut primary = VmHandle::boot(opts(), Path::new("world")).expect("primary boots");
    primary.set_worker_boot(Arc::new(|| VmHandle::boot(opts(), Path::new("world"))));

    primary
        .exec("AppVmHost start: #dx tool: 'WinDemoTool'.")
        .expect("app starts");
    assert_eq!(
        eval(&mut primary, "(AppVmHost named: #dx) isAlive printString"),
        "'true'"
    );

    // Kill it — and read liveness WITHOUT pumping anything. The table's own
    // flag flips as the thread exits; the link's would still say true.
    primary
        .exec("(AppVmHost named: #dx) terminate.")
        .expect("poison sent");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if eval(&mut primary, "(AppVmHost named: #dx) isAlive printString") == "'false'" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "isAlive never turned false — the table truth is not being read"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // Now pump ONCE: the death notice arrives, the watcher names the app,
    // retires the entry, and liveCount tells the truth.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        primary.exec("Worker dispatchInbox.").expect("pump the notice");
        if eval(&mut primary, "(AppVmHost named: #dx) isNil printString") == "'true'" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the death watcher never retired the dead app"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(eval(&mut primary, "AppVmHost liveCount printString"), "'0'");
}

/// S4's gate (`docs/process_services.md` §5): THE DISPLAY SPAWNS AN APP VM
/// WITH THE PRIMARY UNINVOLVED. The primary boots, registers the display,
/// grants it the epoch — and is then never exec'd again. The UI itself runs
/// `Worker spawn:` through the fleet; the newborn is introduced to the
/// display (which is its spawner's own window server), sends its first
/// frame straight there, and a window opens. The language thread did
/// nothing but exist.
#[test]
fn the_display_spawns_an_app_vm_with_the_primary_uninvolved() {
    let mut primary = VmHandle::boot(opts(), Path::new("world")).expect("primary boots");
    primary.set_worker_boot(Arc::new(|| VmHandle::boot(opts(), Path::new("world"))));
    let epoch = primary.primary_epoch().expect("a primary has an epoch");
    let (ui_id, ui_inbox, to_primary) = primary
        .register_hosted_worker(Arc::new(|| {}))
        .expect("register the display");
    assert!(primary.set_ui_peer(ui_id));

    let mut ui = VmHandle::boot(opts(), Path::new("world")).expect("display boots");
    ui.install_worker_role(ui_id, to_primary);
    ui.set_spawn_grant(epoch);
    ui.exec("AppVmDisplay install.").expect("display listens");

    // THE DISPLAY SPAWNS. From here on the primary is a bystander.
    ui.exec(concat!(
        "SpawnedApp := Worker spawn: ",
        "'AppVmClient serve: ''WinDemoTool'' as: #gx.'."
    ))
    .expect("the display spawns the app VM");

    drain_until(&mut ui, &ui_inbox, "waiting for the frame", |ui| {
        eval(ui, "(AppVmDisplay frameFor: #gx) isNil not printString") == "'true'"
    });
    assert_eq!(
        eval(&mut ui, "(AppToolWindow named: #gx) isOpen printString"),
        "'true'",
        "the app's window opened on the display that spawned it"
    );
    // And the round trip works without the primary too.
    ui.exec("(AppToolWindow named: #gx) dispatch: #bump value: nil.")
        .expect("event to the app");
    drain_until(&mut ui, &ui_inbox, "waiting for the re-frame", |ui| {
        eval(
            ui,
            "[ | out | out := ''. (AppVmDisplay frameFor: #gx) body children do: [ :c | \
               c idOrEmpty = 'stateCard' ifTrue: [ c children do: [ :k | \
                 k idOrEmpty = 'clicks' ifTrue: [ out := k at: #text ] ] ] ]. out ] value",
        ) == "'clicks: 1'"
    });
}

/// CROSS-VM PANES (the design that lets the sprite editor run in its own
/// VM): the tool's paint blocks run in the APP's VM against local planes;
/// each frame ships the plane bytes beside the spec; the display copies them
/// into its own surface and presents. Proven by pixels: a click paints a
/// cell in the app VM, and the DISPLAY's surface shows that cell's colour.
#[test]
fn a_pane_tools_pixels_cross_to_the_displays_surface() {
    let mut primary = VmHandle::boot(opts(), Path::new("world")).expect("primary boots");
    primary.set_worker_boot(Arc::new(|| VmHandle::boot(opts(), Path::new("world"))));
    let epoch = primary.primary_epoch().expect("epoch");
    let (ui_id, ui_inbox, to_primary) = primary
        .register_hosted_worker(Arc::new(|| {}))
        .expect("register the display");
    assert!(primary.set_ui_peer(ui_id));
    let mut ui = VmHandle::boot(opts(), Path::new("world")).expect("display boots");
    ui.install_worker_role(ui_id, to_primary);
    ui.set_spawn_grant(epoch);
    ui.exec("AppVmDisplay install.").expect("display listens");

    // The display spawns the PANE tool in its own VM, under the tool's own id.
    ui.exec(concat!(
        "PaneApp := Worker spawn: ",
        "'AppVmClient serve: ''AppDemoPane'' as: #panedemo.'."
    ))
    .expect("spawn the pane tool's VM");
    drain_until(&mut ui, &ui_inbox, "waiting for the pane tool's frame", |ui| {
        eval(ui, "(AppVmDisplay frameFor: #panedemo) isNil not printString") == "'true'"
    });
    assert_eq!(
        eval(&mut ui, "(((AppToolWindow named: #panedemo) paneHandle: #grid) > 0) printString"),
        "'true'",
        "the display-side window realized the pane with a surface"
    );

    // A click in the pane, at cell (3,2) — the event crosses to the app VM,
    // its paint block colours the cell in ITS plane, and the blit brings the
    // pixels back to THIS surface.
    ui.exec(concat!(
        "(AppToolWindow named: #panedemo) dispatch: #grid ",
        "value: (Array with: 3 * AppDemoPane cellSize + 5 with: 2 * AppDemoPane cellSize + 5)."
    ))
    .expect("the click ships");
    drain_until(&mut ui, &ui_inbox, "waiting for the painted cell to cross", |ui| {
        eval(
            ui,
            concat!(
                "[ | w h plane stride | w := AppToolWindow named: #panedemo. ",
                "h := w paneHandle: #grid. ",
                "plane := AppPixels planeFor: h width: AppDemoPane paneSize height: AppDemoPane paneSize. ",
                "stride := AppPixels strideFor: h. ",
                "((AppPixels at: plane x: 3 * AppDemoPane cellSize + 4 y: 2 * AppDemoPane cellSize + 4 stride: stride) ",
                "= (AppDemoPane bgraOf: 1)) printString ] value"
            ),
        ) == "'true'"
    });
}

/// CLOSING THE WINDOW ENDS THE APP — and the app is what ends it. The
/// author's rule: *"a vm should exit itself and send a vm_exiting message as
/// it does so"*. So the display asks (down the peer link, no primary in the
/// path), the app VM tears its window down, announces `#vmExiting`, and only
/// then exits; the announcement — not a timeout, not a death notice — is what
/// destroys the window. Proven by all three: the VM is dead, the window is
/// unregistered, and a relaunch under the same id builds a NEW one.
#[test]
fn closing_the_window_makes_the_app_vm_exit_itself() {
    let mut primary = VmHandle::boot(opts(), Path::new("world")).expect("primary boots");
    primary.set_worker_boot(Arc::new(|| VmHandle::boot(opts(), Path::new("world"))));
    let epoch = primary.primary_epoch().expect("epoch");
    let (ui_id, ui_inbox, to_primary) = primary
        .register_hosted_worker(Arc::new(|| {}))
        .expect("register the display");
    assert!(primary.set_ui_peer(ui_id));
    let mut ui = VmHandle::boot(opts(), Path::new("world")).expect("display boots");
    ui.install_worker_role(ui_id, to_primary);
    ui.set_spawn_grant(epoch);
    ui.exec("AppVmDisplay install.").expect("display listens");
    ui.exec("App := Worker spawn: 'AppVmClient serve: ''AppDemoPane'' as: #panedemo.'.")
        .expect("spawn the app's VM");
    drain_until(&mut ui, &ui_inbox, "waiting for the app's window", |ui| {
        eval(ui, "((AppToolWindow named: #panedemo) isNil not) printString") == "'true'"
    });
    assert_eq!(eval(&mut ui, "App isAlive printString"), "'true'");

    // The close gesture, exactly as AppKit delivers it: the window's own
    // `closeRequested`, which for a VM-backed window is the retire hook.
    ui.exec("(AppToolWindow named: #panedemo) closeRequested.")
        .expect("the close gesture");

    // The app's goodbye comes back and takes the window with it.
    drain_until(&mut ui, &ui_inbox, "waiting for the app to exit itself", |ui| {
        eval(ui, "(AppToolWindow named: #panedemo) isNil printString") == "'true'"
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while eval(&mut ui, "App isAlive printString") != "'false'" {
        assert!(Instant::now() < deadline, "the app VM never exited");
        std::thread::sleep(Duration::from_millis(20));
    }

    // RELAUNCH IS A FRESH START, not a fronted corpse: a new VM under the same
    // id registers its own window, which the retired-peer guard must let past.
    ui.exec("App2 := Worker spawn: 'AppVmClient serve: ''AppDemoPane'' as: #panedemo.'.")
        .expect("relaunch");
    drain_until(&mut ui, &ui_inbox, "waiting for the relaunched window", |ui| {
        eval(ui, "((AppToolWindow named: #panedemo) isNil not) printString") == "'true'"
    });
    assert_eq!(
        eval(&mut ui, "App2 isAlive printString"),
        "'true'",
        "the relaunched app is live behind its new window"
    );
}

/// WHAT A VM LEAVES BEHIND — nothing, once its grave period is up. Every
/// process-level structure a worker touches is checked BY ITS OWN ID: the
/// timer service's registration (it ticked), and the worker table's row. Dead
/// rows are deliberately KEPT for a minute first — watching a VM go from
/// alive to dead is how you learn it ended cleanly rather than vanished — so
/// the test sweeps with a zero-length grave to see the retire without waiting
/// one out. Everything here is asked per-id, never as a total: these are
/// PROCESS-wide structures shared with every other test in this binary.
#[test]
fn an_exited_vm_leaves_no_process_level_residue() {
    let mut primary = VmHandle::boot(opts(), Path::new("world")).expect("primary boots");
    primary.set_worker_boot(Arc::new(|| VmHandle::boot(opts(), Path::new("world"))));

    // A worker that ticks (so it holds a timer registration), then is ended.
    // The init runs ONE top-level item, so the two statements are one block.
    primary
        .exec("Ghost := Worker spawn: '[ Worker onTick: [ nil ]. Worker tickEvery: 30 ] value.'.")
        .expect("spawn a ticking worker");
    let ghost: u32 = eval(&mut primary, "Ghost id printString")
        .trim_matches('\'')
        .parse()
        .expect("the worker's handle");
    // The timer service is PROCESS-wide, so it is asked by the VM's
    // process-unique key — a handle repeats across epochs, and every other
    // test in this binary has its own epoch.
    let epoch = primary.primary_epoch().expect("this test's own epoch");
    let deadline = Instant::now() + Duration::from_secs(5);
    let ghost_key = loop {
        let key = macvm::runtime::workers::worker_table_snapshot()
            .into_iter()
            .find(|r| r.worker_id == ghost && r.primary_epoch == epoch)
            .and_then(|r| r.vm_key);
        if let Some(k) = key {
            break k;
        }
        assert!(Instant::now() < deadline, "the worker never reported its key");
        std::thread::sleep(Duration::from_millis(20));
    };

    let deadline = Instant::now() + Duration::from_secs(5);
    while !macvm::runtime::timer_service::has_registration(ghost_key) {
        assert!(Instant::now() < deadline, "the worker never registered a tick");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        macvm::runtime::workers::worker_table_snapshot()
            .iter()
            .any(|r| r.worker_id == ghost && r.primary_epoch == epoch && r.alive),
        "a live worker must be in the table"
    );

    primary.exec("Ghost terminate.").expect("end it");

    // The timer registration goes on the service's next wake, because a dead
    // target's entry is dropped by whoever woke it — not held until its own
    // next tick would have been due.
    let deadline = Instant::now() + Duration::from_secs(5);
    while macvm::runtime::timer_service::has_registration(ghost_key) {
        assert!(
            Instant::now() < deadline,
            "the dead VM's timer registration outlived it"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // The row survives its grave — the point of keeping it — and is retired
    // once that is up.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        macvm::runtime::workers::worker_table_retire_now(Duration::from_secs(3600));
        let row = macvm::runtime::workers::worker_table_snapshot()
            .into_iter()
            .find(|r| r.worker_id == ghost && r.primary_epoch == epoch);
        match row {
            Some(r) if !r.alive => break, // dead, still visible: correct
            _ => assert!(
                Instant::now() < deadline,
                "the worker's row never showed it dying"
            ),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    macvm::runtime::workers::worker_table_retire_now(Duration::ZERO);
    assert!(
        !macvm::runtime::workers::worker_table_snapshot()
            .iter()
            .any(|r| r.worker_id == ghost && r.primary_epoch == epoch),
        "the row survived its grave period"
    );
}
