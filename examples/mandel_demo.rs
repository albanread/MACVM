//! Spin up a FRESH MACVM instance, run the MandelZoom demo through the game
//! channel for one zoom dive, then drop the instance (it exits). A standalone
//! demonstrator of the "new VM -> run demo -> exit" lifecycle built on nothing
//! but the `VmHandle` embedding API (`src/embed.rs`) — no GUI, no window, no
//! AppKit main thread.
//!
//! It's headless, so instead of a Metal pane it captures each frame's `Blit`
//! (the whole 320x240 palette-indexed buffer MandelZoom hands over per frame)
//! and writes the final frame to a PPM so you can actually see the result:
//!
//!   cargo run --release --example mandel_demo               # 120 frames -> mandel.ppm
//!   cargo run --release --example mandel_demo -- 300 dive.ppm
//!
//! The on-screen version is the SAME VM half; the only additions are an
//! NSWindow + CAMetalLayer and a 60 Hz NSTimer driving these exact
//! `stepWithKeys:` calls plus a present — i.e. precisely what
//! `gui/src/game_pane.rs` + the game-loop timer already do.

use macvm::embed::{GameCommand, GameSink, VmHandle};
use macvm::runtime::vm_state::{JitMode, VmOptions};
use std::path::Path;
use std::sync::{Arc, Mutex};

const W: usize = 320;
const H: usize = 240;

/// The headless stand-in for the GUI's Metal pane: it remembers the palette and
/// the most recent full frame, and counts frames.
struct FrameGrab {
    palette: [(u8, u8, u8); 256],
    last_frame: Option<Vec<u8>>,
    frames: usize,
}

impl FrameGrab {
    fn new() -> Self {
        FrameGrab {
            palette: [(0, 0, 0); 256],
            last_frame: None,
            frames: 0,
        }
    }
}

/// A local newtype so we can implement the foreign `GameSink` trait on it (the
/// orphan rule forbids implementing it directly on `Arc<Mutex<..>>`).
struct Sink(Arc<Mutex<FrameGrab>>);

impl GameSink for Sink {
    fn emit(&mut self, cmd: GameCommand) {
        let mut g = self.0.lock().unwrap();
        match cmd {
            GameCommand::PaletteAt {
                index,
                r,
                g: green,
                b,
            } => {
                g.palette[index as usize] = (r, green, b);
            }
            GameCommand::Blit { data } => {
                g.last_frame = Some(data);
                g.frames += 1;
            }
            // Cls/Present/StartLoop/... aren't needed to reconstruct a blitted
            // frame headlessly; a real pane would honour them.
            _ => {}
        }
    }
}

fn write_ppm(path: &str, g: &FrameGrab) -> std::io::Result<bool> {
    let Some(buf) = &g.last_frame else {
        return Ok(false);
    };
    if buf.len() < W * H {
        return Ok(false);
    }
    let mut out = Vec::with_capacity(16 + W * H * 3);
    out.extend_from_slice(format!("P6\n{W} {H}\n255\n").as_bytes());
    for &idx in &buf[..W * H] {
        let (r, green, b) = g.palette[idx as usize];
        out.push(r);
        out.push(green);
        out.push(b);
    }
    std::fs::write(path, out)?;
    Ok(true)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let frames: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(120);
    let out = args.next().unwrap_or_else(|| "mandel.ppm".to_string());

    // 1. Spin up a fresh VM instance: genesis + load the whole world from
    //    world/. JIT on (Threshold(1)) so the Double escape-time math runs as
    //    compiled native code, exactly like the GUI runs it.
    let t0 = std::time::Instant::now();
    let mut vm = VmHandle::boot(
        VmOptions {
            heap_mib: 128,
            jit: JitMode::Threshold(1),
            ..Default::default()
        },
        Path::new("world"),
    )
    .expect("boot a fresh VM against world/");
    let boot_ms = t0.elapsed().as_millis();

    // 2. Route the VM->GUI game channel to our frame grabber.
    let grab = Arc::new(Mutex::new(FrameGrab::new()));
    vm.set_game_sink(Box::new(Sink(grab.clone())));

    // 3. Run the demo. `launch` registers the per-frame step block (in a class
    //    var, GC-rooted) and returns immediately — the frame loop is external.
    vm.exec("MandelZoom launch.")
        .expect("MandelZoom launch must run cleanly");

    // 4. Drive the frames. In the GUI a 60 Hz NSTimer submits one GameStep per
    //    tick; headless we just call the same per-tick entry point in a plain
    //    loop. `frames` ~= 120 is roughly one full zoom dive (scale 3.5 -> reset).
    let t1 = std::time::Instant::now();
    for _ in 0..frames {
        vm.exec("GamePane stepWithKeys: 0.")
            .expect("a frame step must not error");
    }
    let render_ms = t1.elapsed().as_millis().max(1);

    // 5. Proof it rendered: write the final captured frame.
    let g = grab.lock().unwrap();
    let painted = write_ppm(&out, &g).unwrap_or(false);

    // 6. Report, then drop the instance — the VM (its heap, code cache, world)
    //    is gone when `vm` falls out of scope.
    let m = vm.metrics();
    println!(
        "spun up in {boot_ms} ms | {} frames in {render_ms} ms ({:.0} fps) | \
         GC {}\u{b7}{} | allocated {} KiB",
        g.frames,
        g.frames as f64 / (render_ms as f64 / 1000.0),
        m.scavenges,
        m.full_gcs,
        m.bytes_allocated / 1024,
    );
    if painted {
        println!("wrote {out} ({W}x{H}, view it with any image tool)");
    }
    drop(g);
    drop(vm);
    println!("VM instance dropped — exited cleanly.");
}
