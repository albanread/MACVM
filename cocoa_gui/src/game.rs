//! CG10 — the native game pane, REUSED not rebuilt. The MacGamePane engine
//! (`macgamepane-graphics`, the same crate + `metal` the WKWebView GUI drives)
//! renders; the demos run on the PRIMARY VM (only a primary can spawn the
//! compute workers ParallelMandel needs); their `GameCommand`s cross to the
//! main thread over the SAME worker→main transport `cocoa_gui` already uses for
//! everything else — a `GameSink` that pushes onto a queue and wakes the run
//! loop. Main drains the queue and applies each command to the pane.
//!
//! The one bit that isn't pure reuse: MacGamePane's `GameWindow::run` rolls its
//! own blocking event pump, which we can't use — AppKit's `[NSApp run]` already
//! owns the main loop. So we take `GameWindow::create` (window + `CAMetalLayer`
//! + key-capable view + device, all in one call) and drive frames from a
//! **default-mode** `NSTimer` on the shared loop — which makes the game
//! tracking-safe (§8) and keeps the IDE window live alongside it, for free.
//!
//! Frame loop: the primary's `GamePane>>run` emits `StartLoop` → main opens the
//! window + starts the timer → each tick submits `GamePane stepWithKeys:` to
//! the primary (single-outstanding: not until the last frame's `Present` landed)
//! → the step's draw commands stream back over the sink → main applies them,
//! and the frame's own `Present` shows it.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex;

use macgamepane_graphics::indexed_pane::IndexedPane;
use macgamepane_graphics::sprites::Sprites;
use macgamepane_graphics::window::GameWindow;
use macvm::embed::{GameCommand, GameSink};
use metal::MTLLoadAction;

use crate::objc;

const PANE_W: u32 = 320;
const PANE_H: u32 = 240;
/// Palette index reused as the `ClearTo` fill colour (matches game_pane.rs).
const PAL_BG: u8 = 16;
/// macOS virtual key code for Escape (ends the game).
const KEY_ESCAPE: u16 = 53;

// ── worker→main transport (the "report to main" protocol) ────────────────────

/// Commands emitted by the primary's game sink, drained on main. A plain queue
/// + the existing run-loop wake — the same shape as `ChannelGameSink`.
static GAME_CMDS: Mutex<VecDeque<GameCommand>> = Mutex::new(VecDeque::new());
static GAME_ACTIVE: AtomicBool = AtomicBool::new(false);
/// The timer set "a frame tick is due"; the primary's supervisor loop consumes
/// it and runs the step at TOP LEVEL (never a nested VM entry — a nested entry
/// running JIT-compiled compute trips the frame-walk invariant under GC).
static STEP_DUE: AtomicBool = AtomicBool::new(false);
/// Escape was seen — request the primary stop the loop.
static STOP_DUE: AtomicBool = AtomicBool::new(false);
/// This tick's held-key bitmask (bit 0=Left…5=B), read on the tick, sent with
/// the step.
static GAME_KEY_MASK: AtomicI64 = AtomicI64::new(0);

/// The primary's `GameSink`: push each command onto the shared queue and wake
/// main. Runs on the PRIMARY's thread (`Send`); main drains. Installed by the
/// supervisor after the primary boots.
pub struct PrimaryGameSink;
impl GameSink for PrimaryGameSink {
    fn emit(&mut self, cmd: GameCommand) {
        if let Ok(mut q) = GAME_CMDS.lock() {
            q.push_back(cmd);
        }
        objc::wake_main_runloop();
    }
}

// ── the native pane (main-thread only) ───────────────────────────────────────

struct NativeGame {
    win: GameWindow,
    pane: IndexedPane,
    sprites: Sprites,
    sprite_ids: HashMap<i64, (usize, usize)>,
    timer: objc::Id,
}

thread_local! {
    static GAME: std::cell::RefCell<Option<NativeGame>> = const { std::cell::RefCell::new(None) };
}

/// Open the game window + pane once (main thread), lazily on the FIRST command
/// of a session — a demo sets its palette + draws its opening scene BEFORE
/// `GamePane>>run` (StartLoop), so waiting for StartLoop would drop all of that
/// (the Breakout "all-red palette" bug). `None` if the Mac has no Metal device.
/// `GameWindow::create` does the whole NSWindow + CAMetalLayer + key-capable-view
/// wiring we would otherwise hand-roll. The frame timer is NOT started here —
/// only on StartLoop (`start_frame_timer`).
fn ensure_pane() {
    GAME.with(|cell| {
        if cell.borrow().is_some() {
            return;
        }
        let Some(win) = GameWindow::create("MACVM Game", PANE_W as f64, PANE_H as f64) else {
            eprintln!("macvm-cocoa: no Metal device — game pane unavailable");
            return;
        };
        let Ok(pane) = IndexedPane::new(&win.device, PANE_W, PANE_H, PANE_W, PANE_H) else {
            return;
        };
        let Ok(sprites) = Sprites::new(&win.device) else {
            return;
        };
        *cell.borrow_mut() = Some(NativeGame {
            win,
            pane,
            sprites,
            sprite_ids: HashMap::new(),
            timer: objc::NIL,
        });
    });
}

/// Start the default-mode 60Hz frame timer (tracking-safe): its `gameTick:`
/// flags a step due + reads keys; the primary's supervisor loop runs the step.
/// Idempotent — invalidates any prior timer first.
fn start_frame_timer() {
    GAME.with(|cell| {
        if let Some(g) = cell.borrow_mut().as_mut() {
            if g.timer != objc::NIL {
                objc::send0(g.timer, objc::sel("invalidate"));
            }
            g.timer = objc::scheduled_timer(
                1.0 / 60.0,
                game_timer_target(),
                objc::sel("gameTick:"),
                true,
            );
        }
    });
}

/// Close the game window + drop its GPU resources (main thread). Stops the
/// timer first, orders the window out.
fn close_window() {
    GAME.with(|cell| {
        if let Some(g) = cell.borrow_mut().take() {
            objc::send0(g.timer, objc::sel("invalidate"));
            objc::send1_id(g.win.view, objc::sel("removeFromSuperview"), objc::NIL);
            // The GameWindow's own `window` field is private; ordering it out via
            // the view's window keeps it from lingering after the pane drops.
            let w = objc::send0(g.win.view, objc::sel("window"));
            if !w.is_null() {
                objc::send1_id(w, objc::sel("orderOut:"), objc::NIL);
            }
        }
    });
    GAME_ACTIVE.store(false, Ordering::Release);
}

/// Upload + present one frame into the layer's next drawable (main thread).
fn present(g: &mut NativeGame) {
    g.pane.upload();
    let Some(drawable) = g.win.layer.next_drawable() else {
        return; // compositor has none this instant — skip, not an error
    };
    let cb = g.win.command_queue.new_command_buffer();
    g.pane.render(cb, drawable.texture(), MTLLoadAction::Clear);
    g.sprites
        .render(cb, drawable.texture(), 0.0, 0.0, PANE_W as f64, PANE_H as f64);
    cb.present_drawable(drawable);
    cb.commit();
}

/// Apply one `GameCommand` to the pane (main thread) — the exact vocabulary
/// game_pane.rs's `apply_command` handles. Loop control (`StartLoop`/`StopLoop`)
/// is handled by the caller; here we only draw/present.
fn apply(g: &mut NativeGame, cmd: &GameCommand) {
    use GameCommand as C;
    match cmd {
        C::PaletteAt { index, r, g: gg, b } => g.pane.set_rgb(*index, *r, *gg, *b),
        C::Cls { index } => g.pane.cls(*index),
        C::ClearTo { r, g: gg, b } => {
            g.pane.set_rgb(PAL_BG, *r, *gg, *b);
            g.pane.cls(PAL_BG);
        }
        C::Pset { x, y, index } => g.pane.pset(*x, *y, *index),
        C::Line { x0, y0, x1, y1, index } => g.pane.line(*x0, *y0, *x1, *y1, *index),
        C::FillRect { x, y, w, h, index } => g.pane.fill_rect(*x, *y, *w, *h, *index),
        C::Disc { cx, cy, r, index } => g.pane.disc(*cx, *cy, *r, *index),
        C::Blit { data } => g.pane.blit(data),
        C::DefineSprite { id, rows } => {
            if let Some(def) = g.sprites.define_sprite(rows) {
                let inst = g.sprites.place(def, 0.0, 0.0);
                g.sprite_ids.insert(*id, (def, inst));
            }
        }
        C::SpriteColor { id, index, r, g: gg, b } => {
            if let Some(&(def, _)) = g.sprite_ids.get(id) {
                g.sprites.sprite_rgb(def, *index, *r, *gg, *b);
            }
        }
        C::MoveSprite { id, x, y } => {
            if let Some(&(_, inst)) = g.sprite_ids.get(id) {
                g.sprites.move_to(inst, *x as f64, *y as f64);
            }
        }
        C::Present => present(g),
        C::StartLoop | C::StopLoop | C::PlaySound { .. } | C::PlayTune { .. } => {}
    }
}

/// Drain the game command queue and apply it (main thread, from `drain_perform`).
/// Loop control and audio are handled here (they work regardless of pane state).
pub fn drain() {
    let cmds: Vec<GameCommand> = {
        let Ok(mut q) = GAME_CMDS.lock() else { return };
        if q.is_empty() {
            return;
        }
        q.drain(..).collect()
    };
    for cmd in &cmds {
        match cmd {
            GameCommand::StartLoop => {
                ensure_pane();
                start_frame_timer();
                GAME_ACTIVE.store(true, Ordering::Release);
            }
            GameCommand::StopLoop => close_window(),
            GameCommand::PlaySound { preset } => audio::play_sound(*preset),
            GameCommand::PlayTune { abc } => audio::play_tune(abc),
            // Every draw/palette/present command: create the pane on the FIRST
            // one (a demo paints its opening scene before StartLoop), then apply.
            _ => {
                ensure_pane();
                GAME.with(|cell| {
                    if let Some(g) = cell.borrow_mut().as_mut() {
                        apply(g, cmd);
                    }
                });
            }
        }
    }
}

/// Is a game loop currently running? The primary's supervisor loop uses this to
/// spin fast (so band replies + frame steps flow at ~60Hz) instead of parking
/// in its idle metrics beat.
pub fn is_active() -> bool {
    GAME_ACTIVE.load(Ordering::Acquire)
}

/// Called by the PRIMARY's supervisor loop each iteration: run one frame step at
/// TOP LEVEL if the timer flagged one due, or stop the loop on Escape. Returns
/// the Smalltalk to `exec` on the primary (top-level, never nested), or `None`.
/// This is the crux fix — the step runs as a fresh top-level entry, so its
/// JIT-compiled compute never sits under an ENTRY_FRAME_SENTINEL the frame walk
/// mispairs during GC.
pub fn poll_primary_step() -> Option<String> {
    if STOP_DUE.swap(false, Ordering::AcqRel) {
        return Some("GamePane stop. GamePane reset.".to_string());
    }
    if !GAME_ACTIVE.load(Ordering::Acquire) {
        return None;
    }
    if !STEP_DUE.swap(false, Ordering::AcqRel) {
        return None;
    }
    let mask = GAME_KEY_MASK.load(Ordering::Relaxed);
    Some(format!("GamePane stepWithKeys: {mask}"))
}

// ── the frame timer's target (an ObjC object with a `gameTick:` method) ──────

/// Called ~60Hz on the main run loop (default mode). Reads Escape + the held-key
/// mask, flags a step due, and lets the drain do the VM work (it holds the UI
/// worker). Keeps zero VM access here — a timer IMP has no `DrainState`.
extern "C" fn game_tick(_this: objc::Id, _cmd: objc::Sel, _timer: objc::Id) {
    use macgamepane_graphics::input::key_held;
    if key_held(KEY_ESCAPE) {
        STOP_DUE.store(true, Ordering::Release);
        objc::wake_main_runloop();
        return;
    }
    // Left, Right, Up, Down, Space(A), Z(B) — GamePane class>>keyLeft…keyB order.
    const CODES: [u16; 6] = [123, 124, 126, 125, 49, 6];
    let mut mask = 0i64;
    for (bit, code) in CODES.iter().enumerate() {
        if key_held(*code) {
            mask |= 1 << bit;
        }
    }
    GAME_KEY_MASK.store(mask, Ordering::Relaxed);
    STEP_DUE.store(true, Ordering::Release);
    objc::wake_main_runloop();
}

/// Register a `MacvmGameTimer` class with the `gameTick:` method once, and
/// return a shared instance to use as the timer target.
fn game_timer_target() -> objc::Id {
    use std::sync::OnceLock;
    static TARGET: OnceLock<usize> = OnceLock::new();
    *TARGET.get_or_init(|| {
        type ImpV1 = extern "C" fn(objc::Id, objc::Sel, objc::Id);
        objc::register_class(
            "MacvmGameTimer",
            &[("gameTick:", game_tick as ImpV1 as *const std::ffi::c_void, "v@:@")],
        );
        objc::alloc_init("MacvmGameTimer") as usize
    }) as objc::Id
}

// ── audio (reused MacGamePane synth, main thread) ────────────────────────────

mod audio {
    thread_local! {
        static SFX: std::cell::RefCell<Option<macgamepane_audio::playback::Sfx>> =
            const { std::cell::RefCell::new(None) };
        static DEFINED: std::cell::Cell<u16> = const { std::cell::Cell::new(0) };
    }

    pub fn play_sound(preset: u8) {
        SFX.with(|cell| {
            let mut slot = cell.borrow_mut();
            if slot.is_none() {
                let mut sfx = macgamepane_audio::playback::Sfx::new();
                if !sfx.start() {
                    return;
                }
                *slot = Some(sfx);
            }
            let sfx = slot.as_mut().unwrap();
            let bit = 1u16 << preset.min(15);
            if DEFINED.with(|d| d.get()) & bit == 0 {
                sfx.define(preset as usize, &synth_preset(preset));
                DEFINED.with(|d| d.set(d.get() | bit));
            }
            sfx.play(preset as usize);
        });
    }

    pub fn play_tune(abc: &str) {
        if let Some(tune) = macgamepane_audio::abc::parse_tune(abc) {
            let _ = macgamepane_audio::playback::play_tune_non_blocking(&tune);
        }
    }

    fn synth_preset(preset: u8) -> macgamepane_audio::synth::Sound {
        use macgamepane_audio::synth as s;
        let mut rng = s::Lcg::new(0x9E37_79B9 ^ preset as u32);
        match preset {
            0 => s::coin(0.15),
            1 => s::jump(0.20),
            2 => s::zap(0.30, &mut rng),
            3 => s::shoot(0.15, &mut rng),
            4 => s::explode(1.0, 0.50, &mut rng),
            5 => s::powerup(0.40),
            6 => s::hurt(0.30, &mut rng),
            7 => s::click(0.05, &mut rng),
            8 => s::bang(0.40, &mut rng),
            _ => s::blip(660.0, 0.12),
        }
    }
}
