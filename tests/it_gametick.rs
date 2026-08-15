//! A REAL GAME, FRAME-STEPPED BY THE VM TIMER, IN A WORKER OF ITS OWN — the
//! first consumer of `Worker tickEvery:` (docs/appspec.md; the author's
//! design: "timers could be managed by the VM to send workers a wakeup").
//!
//! Today's demos run on the PRIMARY: its supervisor loop is their 60 Hz pump
//! (`poll_primary_step`), and the Demos menu says so. This test is the proof
//! that the pump is no longer structural: give a spawned worker a game sink
//! and a tick, and the SAME unedited game runs there — `Life launch`, an
//! `onTick:` that calls the same `GamePane stepWithKeys:` the primary's pump
//! formats, and a cadence. Three lines of Smalltalk; the sink is one line in
//! the boot closure. What still keeps the GUI's demos on the primary is not
//! the frame loop — it is input routing, stop routing, and the
//! one-demo-at-a-time policy, all recorded in the Demos menu's comment.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use macvm::embed::{GameCommand, GameSink, VmHandle};
use macvm::runtime::{JitMode, VmOptions};

fn opts() -> VmOptions {
    VmOptions {
        heap_mib: 64,
        jit: JitMode::Off,
        ..Default::default()
    }
}

/// The capture side of the seam: exactly what the Metal pane is to the GUI,
/// minus the pixels. `Present` marks a finished frame, so counting Presents
/// IS counting frames.
struct CapSink(Arc<Mutex<Vec<GameCommand>>>);
impl GameSink for CapSink {
    fn emit(&mut self, cmd: GameCommand) {
        self.0.lock().unwrap().push(cmd);
    }
}

/// The two tests below share one process — and one process-global timer
/// service, which the shutdown test deliberately stops. Serialize them.
static EXCLUSIVE: Mutex<()> = Mutex::new(());

fn presents_in(cmds: &Arc<Mutex<Vec<GameCommand>>>) -> usize {
    cmds.lock()
        .unwrap()
        .iter()
        .filter(|c| matches!(c, GameCommand::Present))
        .count()
}

#[test]
fn a_game_in_its_own_vm_renders_frames_on_the_vm_timer() {
    let _x = EXCLUSIVE.lock().unwrap_or_else(|e| e.into_inner());
    // The transcript SERVICE (the author's design): every VM writes to it
    // directly, and this test reads it directly — the game worker's words
    // are visible with no primary pump and no envelope chain, which is the
    // whole point of having it.
    macvm::runtime::transcript_service::activate();
    let mut primary = VmHandle::boot(opts(), Path::new("world")).expect("primary boots");

    // The one piece of substrate a game-hosting worker needs that a compute
    // worker does not: somewhere for GamePane's commands to go. The boot
    // closure installs it, exactly where the GUI would install its own.
    let captured: Arc<Mutex<Vec<GameCommand>>> = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    primary.set_worker_boot(Arc::new(move || {
        let mut h = VmHandle::boot(opts(), Path::new("world"))?;
        h.set_game_sink(Box::new(CapSink(cap.clone())));
        Ok(h)
    }));

    // THE WHOLE GAME-IN-A-WORKER, three statements: launch the unedited
    // game, name the step as the tick function, ask for a cadence. This is
    // the code a demo author would write, and there is deliberately nothing
    // else — no loop, no sleep, no thread: the VM owns the time.
    primary
        .exec(concat!(
            "GameVm := Worker spawn: '[ ",
            "Transcript showCr: ''L:'' , ([ Life launch. ''ok'' ] on: Error do: [ :e | e messageText ]). ",
            "Worker onTick: [ GamePane stepWithKeys: 0 mouseX: 0 y: 0 buttons: 0 ]. ",
            "Worker tickEvery: 16 ] value.'."
        ))
        .expect("spawn the game VM");


    // Nobody talks to it. The ONLY thing that can advance the game is the
    // VM's own wakeup. Sample the frame count twice so the assertion is
    // about an ONGOING cadence, not a launch-time burst.
    std::thread::sleep(Duration::from_millis(700));
    let at_400ms = presents_in(&captured);
    std::thread::sleep(Duration::from_millis(700));
    let at_800ms = presents_in(&captured);

    // The worker's words, straight from the service — no pump, no chain.
    let (_, words) = macvm::runtime::transcript_service::drain_since(0);
    eprintln!("WORKER SAID: {words:?}");
    eprintln!("ALIVE: {:?}", primary.eval("GameVm isAlive printString"));

    // 16ms over 800ms is ~50 frames; a loaded CI machine still clears 10
    // easily, and the old message-driven worker — whose only wakeup was the
    // 2-second pulse — presents exactly ZERO in this window.
    assert!(
        at_800ms >= 10,
        "expected a frame cadence, got {at_800ms} presents in ~800ms \
         (a message-driven worker gives 0)"
    );
    assert!(
        at_800ms > at_400ms,
        "frames stopped between samples ({at_400ms} -> {at_800ms}) — \
         a burst at launch is not a frame loop"
    );

    // And the game actually RAN — generations advanced, cells changed hands:
    // ask the game itself, in its own VM, through the ordinary RPC seam.
    primary.exec("GamePop := nil.").expect("scoreboard");
    primary
        .exec("GameVm call: #population on: #LifeProbe args: #() onReply: [ :r | GamePop := r ].")
        .expect("ask the game (fails soft if LifeProbe is absent)");

    // The probe class may not exist in the world; the load-bearing
    // assertions above are the frame cadence. Terminate and make sure the
    // capture STOPS growing — a dead VM must not keep ticking.
    primary.exec("GameVm terminate.").expect("stop the game VM");
    std::thread::sleep(Duration::from_millis(120));
    let after_kill = presents_in(&captured);
    std::thread::sleep(Duration::from_millis(300));
    let later = presents_in(&captured);
    assert_eq!(
        after_kill, later,
        "a terminated game VM kept presenting frames — the tick outlived its VM"
    );
}

/// S3's gate (`docs/process_services.md` §4): the exit sequence. Two ticking
/// game VMs are alive; `orderly_shutdown` stops the timers, poisons both,
/// and the process-level flags confirm every VM thread exited — with never a
/// join anywhere.
#[test]
fn orderly_shutdown_stops_every_ticking_worker() {
    let _x = EXCLUSIVE.lock().unwrap_or_else(|e| e.into_inner());
    let mut primary = VmHandle::boot(opts(), Path::new("world")).expect("primary boots");
    let captured: Arc<Mutex<Vec<GameCommand>>> = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    primary.set_worker_boot(Arc::new(move || {
        let mut h = VmHandle::boot(opts(), Path::new("world"))?;
        h.set_game_sink(Box::new(CapSink(cap.clone())));
        Ok(h)
    }));
    for n in 0..2 {
        primary
            .exec(&format!(
                "G{n} := Worker spawn: '[ Life launch. \
                 Worker onTick: [ GamePane stepWithKeys: 0 mouseX: 0 y: 0 buttons: 0 ]. \
                 Worker tickEvery: 16 ] value.'."
            ))
            .expect("spawn a ticking game VM");
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        presents_in(&captured) > 0,
        "the games never started — nothing to shut down"
    );

    let all_dead = primary.orderly_shutdown(1000);
    assert!(all_dead, "a worker survived the exit sequence's bounded wait");
    let after = presents_in(&captured);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        after,
        presents_in(&captured),
        "frames after shutdown — a timer or a worker outlived the sequence"
    );
}

/// S6: AN EXISTING DEMO — Life, unedited — RUNS IN A VM OF ITS OWN. The last
/// thing the primary still pumped was the 60 Hz step, formatted as a STRING
/// (`GamePane stepWithKeys: 4 mouseX: …`) and exec'd on the language thread.
/// Here `DemoVmHost start: 'Life launch'` — the Demos menu's own entry — gets
/// its beat from the timer service and reads the input itself: the test sets
/// keys on the process service and then asks the DEMO VM (an RPC to the
/// class-side `GamePane inputState`, the exact read its every step makes)
/// what it sees. Frames flow the whole time, and `stopAll` ends the VM by its
/// own exit.
#[test]
fn an_existing_demo_runs_in_a_vm_of_its_own() {
    let _x = EXCLUSIVE.lock().unwrap_or_else(|e| e.into_inner());
    macvm::runtime::game_input::reset();
    macvm::runtime::transcript_service::activate();
    let mut primary = VmHandle::boot(opts(), Path::new("world")).expect("primary boots");
    let captured: Arc<Mutex<Vec<GameCommand>>> = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    // Exactly what the GUI's boot closure does now: every worker gets the same
    // game sink the primary has, so its commands reach the same pane. (S6's
    // live failure was precisely this line missing from the REAL closure:
    // Life ran perfectly in its VM and every command no-op'd, silently.)
    primary.set_worker_boot(Arc::new(move || {
        let mut h = VmHandle::boot(opts(), Path::new("world"))?;
        h.set_game_sink(Box::new(CapSink(cap.clone())));
        Ok(h)
    }));

    primary
        .exec("DemoVmHost start: 'Life launch'.")
        .expect("spawn the demo VM");
    std::thread::sleep(Duration::from_millis(500));
    let frames_early = presents_in(&captured);
    let (_, said) = macvm::runtime::transcript_service::drain_since(0);
    assert!(
        frames_early > 0,
        "Life never presented a frame from its own VM; it said: {said:?}"
    );

    // THE INPUT CROSSES. Set the process service (as the GUI's frame tick
    // does), then ask the DEMO VM what its next read answers — the same
    // class-side `GamePane inputState` its every step goes through.
    macvm::runtime::game_input::set_keys(0b1010);
    macvm::runtime::game_input::set_mouse(77, 55, 1);
    primary.exec("DemoSaw := nil.").expect("scoreboard");
    primary
        .exec(
            "(DemoVmHost vms at: 1) call: #inputState on: #GamePane args: #()              onReply: [ :r | DemoSaw := r ].",
        )
        .expect("ask the demo VM for its own input read");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        primary.exec("Worker dispatchInbox.").expect("pump replies");
        let got = primary.eval("DemoSaw printString").unwrap_or_default();
        if got.contains("10") && got.contains("77") && got.contains("55") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the demo VM's own input read never showed the keys (got {got})"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    // Still stepping, on its own clock, while all of that happened.
    assert!(
        presents_in(&captured) > frames_early,
        "Life stopped presenting"
    );

    // The stop path the GUI uses (Escape / close / relaunch): ask every demo
    // VM to end. Life's VM stops its clock, resets the pane, announces, goes.
    primary.exec("DemoVmHost stopAll.").expect("stop");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        primary.exec("Worker dispatchInbox.").expect("pump");
        if primary
            .eval("DemoVmHost liveCount printString")
            .unwrap_or_default()
            .trim()
            == "'0'"
        {
            break;
        }
        assert!(Instant::now() < deadline, "the demo VM never exited");
        std::thread::sleep(Duration::from_millis(25));
    }
    let after = presents_in(&captured);
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(after, presents_in(&captured), "a stopped demo kept stepping");
}

/// ASKED FROM OUTSIDE, THE DEMO STILL TIDIES UP. `#exitPlease` — what the
/// Monitor's `Send exit` sends, and what any worker answers — must not be a
/// shortcut past the VM's own cleanup. It was, once: the first Send exit from
/// the Monitor ended the demo VM and left its game pane on screen with nothing
/// behind it, because the ask went straight to `Worker exit:` while the
/// pane-closing lived in the demo client's own stop path. Cleanup now hangs on
/// `Worker onExit:` and runs on EVERY route out, which is what this asserts:
/// the StopLoop that closes the window is emitted even though nothing called
/// `DemoVmClient shutdown`.
#[test]
fn an_exit_asked_from_outside_still_closes_the_pane() {
    let _x = EXCLUSIVE.lock().unwrap_or_else(|e| e.into_inner());
    macvm::runtime::game_input::reset();
    let mut primary = VmHandle::boot(opts(), Path::new("world")).expect("primary boots");
    let captured: Arc<Mutex<Vec<GameCommand>>> = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    primary.set_worker_boot(Arc::new(move || {
        let mut h = VmHandle::boot(opts(), Path::new("world"))?;
        h.set_game_sink(Box::new(CapSink(cap.clone())));
        Ok(h)
    }));
    primary
        .exec("DemoVmHost start: 'Life launch'.")
        .expect("spawn the demo VM");
    let deadline = Instant::now() + Duration::from_secs(5);
    while presents_in(&captured) == 0 {
        assert!(Instant::now() < deadline, "the demo never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    captured.lock().unwrap().clear();

    // The Monitor's gesture, exactly: address the VM by handle and ASK.
    primary
        .exec("(Worker new setId: (DemoVmHost vms at: 1) id) send: (Array with: #exitPlease).")
        .expect("ask it to leave");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let stopped = captured
            .lock()
            .unwrap()
            .iter()
            .any(|c| matches!(c, GameCommand::StopLoop));
        if stopped {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the demo exited without closing its pane — an outside ask skipped \
             the VM's own cleanup"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // And it really did leave.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        primary.exec("Worker dispatchInbox.").expect("pump");
        if primary
            .eval("DemoVmHost liveCount printString")
            .unwrap_or_default()
            .trim()
            == "'0'"
        {
            break;
        }
        assert!(Instant::now() < deadline, "the demo VM never exited");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// A DEMO LAUNCHED AFTER AN EARLIER ONE WAS STOPPED MUST RUN. The bug this
/// pins, reported from the chair before any test caught it: "games/demos
/// refuse to open and close instantly after a couple have been closed, almost
/// like there is a pending exit message hitting the new worker." Exactly that
/// — the Escape request was a sticky global boolean whose only clear lived on
/// the IN-PROCESS launch path, so one Escape poisoned every demo launched into
/// a VM of its own from then on: each read the stale `true` on its first beat
/// and shut itself down. The request is generational now — a demo reacts only
/// to requests raised AFTER it started — and this walks the reported sequence:
/// stop one demo the Escape way, launch another, and require it to still be
/// running and drawing.
#[test]
fn a_demo_launched_after_a_stop_keeps_running() {
    let _x = EXCLUSIVE.lock().unwrap_or_else(|e| e.into_inner());
    macvm::runtime::game_input::reset();
    let mut primary = VmHandle::boot(opts(), Path::new("world")).expect("primary boots");
    let captured: Arc<Mutex<Vec<GameCommand>>> = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    primary.set_worker_boot(Arc::new(move || {
        let mut h = VmHandle::boot(opts(), Path::new("world"))?;
        h.set_game_sink(Box::new(CapSink(cap.clone())));
        Ok(h)
    }));

    // Two full cycles, because the report said "after a couple".
    for round in 1..=2 {
        primary
            .exec("DemoVmHost start: 'Life launch'.")
            .expect("launch a demo");
        let deadline = Instant::now() + Duration::from_secs(5);
        while presents_in(&captured) == 0 {
            assert!(
                Instant::now() < deadline,
                "round {round}: the demo never drew — it exited on a stop \
                 request raised before it was born"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // Escape (what the host's frame tick does when the key is down).
        macvm::runtime::game_input::request_exit();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            primary.exec("Worker dispatchInbox.").expect("pump");
            if primary
                .eval("DemoVmHost liveCount printString")
                .unwrap_or_default()
                .trim()
                == "'0'"
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "round {round}: the demo ignored Escape"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        captured.lock().unwrap().clear();
    }

    // THE THIRD LAUNCH — the one that used to die instantly — must live and
    // keep drawing, with two stop requests already behind it.
    primary
        .exec("DemoVmHost start: 'Life launch'.")
        .expect("launch after two stops");
    let deadline = Instant::now() + Duration::from_secs(5);
    while presents_in(&captured) == 0 {
        assert!(
            Instant::now() < deadline,
            "a demo launched after two stops never drew — a stale exit request \
             reached a VM that did not exist when it was made"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let early = presents_in(&captured);
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        presents_in(&captured) > early,
        "it drew once and stopped — it is still exiting on somebody else's Escape"
    );
    assert_eq!(
        primary.eval("DemoVmHost liveCount printString").unwrap().trim(),
        "'1'",
        "the third demo should still be alive"
    );
    primary.exec("DemoVmHost stopAll.").expect("tidy up");
}
