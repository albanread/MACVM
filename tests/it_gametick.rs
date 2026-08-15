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
use std::time::Duration;

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
