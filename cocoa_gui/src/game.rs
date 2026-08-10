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
//! window + starts the timer → each tick submits
//! `GamePane stepWithKeys:mouseX:y:buttons:` (this frame's held keys and mouse)
//! to the primary (single-outstanding: not until the last frame's `Present` landed)
//! → the step's draw commands stream back over the sink → main applies them,
//! and the frame's own `Present` shows it.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex;

use macgamepane_graphics::direct_pane::DirectPane;
use macgamepane_graphics::indexed_pane::IndexedPane;
use macgamepane_graphics::shader_pane::ShaderPane;
use macgamepane_graphics::sprites::Sprites;
use macgamepane_graphics::text_overlay::TextOverlay;
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
/// A game SESSION is open — from the moment a demo is dispatched until its stop
/// closes the window. Gates pane creation on draw commands: a demo paints its
/// scene before StartLoop (so we create on the first command), but a stray
/// Blit/Present from the last frame arriving AFTER a stop must NOT re-create the
/// window we just closed.
static SESSION_OPEN: AtomicBool = AtomicBool::new(false);
/// The timer set "a frame tick is due"; the primary's supervisor loop consumes
/// it and runs the step at TOP LEVEL (never a nested VM entry — a nested entry
/// running JIT-compiled compute trips the frame-walk invariant under GC).
static STEP_DUE: AtomicBool = AtomicBool::new(false);
/// Escape / close-button — request the primary stop the loop.
static STOP_DUE: AtomicBool = AtomicBool::new(false);
/// This tick's held-key bitmask (bit 0=Left…5=B), read on the tick, sent with
/// the step.
static GAME_KEY_MASK: AtomicI64 = AtomicI64::new(0);
/// This tick's mouse, sent with the step alongside the keys. Position is in
/// PANE PIXELS (not view points and not the engine's normalized fraction):
/// the tick is the one place that knows both the fraction and this session's
/// pane size, so it converts once and Smalltalk gets coordinates in the same
/// space as every drawing method. `-1` means "no pane, nothing sensible to
/// report". Buttons are a bitmask: bit 0 = left, bit 1 = right.
static GAME_MOUSE_X: AtomicI64 = AtomicI64::new(-1);
static GAME_MOUSE_Y: AtomicI64 = AtomicI64::new(-1);
static GAME_MOUSE_BUTTONS: AtomicI64 = AtomicI64::new(0);
/// A demo entry doit to launch, run TOP-LEVEL on the primary's supervisor loop
/// (never a nested `uiReq`/`primEvalDoit` — that corrupts the interp-regs-mirror
/// root and the next frame's compile-GC scavenge double-copies). One demo at a
/// time: launched only once any prior game has fully torn down.
static PENDING_LAUNCH: Mutex<Option<String>> = Mutex::new(None);
/// After a host-side stop closed the window, tell the supervisor to run
/// `GamePane reset` (class-side — clears the demo's step block + keys) so the
/// old demo stops producing frames and releases its object.
static RESET_DUE: AtomicBool = AtomicBool::new(false);

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
    /// The DIRECT framebuffer, when a demo asked for one
    /// (`GamePane>>openDirect:height:`, docs/shared_screen_memory_design.md).
    /// `Some` replaces the indexed pane as what `present` draws: the VM has
    /// been writing pixels straight into this buffer's shared memory, so there
    /// is nothing to upload and nothing to composite under.
    direct: Option<DirectPane>,
    sprites: Sprites,
    /// The topmost HUD/text layer (RGB, real 5x7 font) — always composited
    /// last. Retained between frames; a demo `TextClear`s it each frame.
    text: TextOverlay,
    /// The layer-0 background fragment shader, created lazily on the first
    /// `Shader` command (`None` = no shader; the indexed pane clears the frame).
    shader: Option<ShaderPane>,
    sprite_ids: HashMap<i64, (usize, usize)>,
    /// This session's actual pane size (may be `SetPaneSize`d away from the
    /// 320x240 default — galaxigans is 640x360).
    w: u32,
    h: u32,
    timer: objc::Id,
}

/// The pane size the NEXT `ensure_pane` will create, set by `SetPaneSize`
/// BEFORE the window exists (a demo sends it first). Defaults to 320x240.
static REQ_W: AtomicI64 = AtomicI64::new(PANE_W as i64);
static REQ_H: AtomicI64 = AtomicI64::new(PANE_H as i64);
/// The requested frame-timer rate in fps (`GamePane>>frameRate:`). Default 60;
/// galaxigans asks for 30. Reset to 60 on window close so it can't leak into
/// the next demo.
const DEFAULT_FPS: i64 = 60;
static REQ_FPS: AtomicI64 = AtomicI64::new(DEFAULT_FPS);
/// The requested overscan margin (`GamePane>>overscan:`): how many pixels
/// larger than the VIEWPORT the pane's WORLD is, on every side. 0 = the world
/// is exactly the viewport, which is how every pane was built before this and
/// what pins `set_scroll` to a no-op. Read when the pane is created, and reset
/// on window close so it cannot leak into the next demo.
static REQ_OVERSCAN: AtomicI64 = AtomicI64::new(0);

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
        let w = REQ_W.load(Ordering::Acquire).clamp(1, 4096) as u32;
        let h = REQ_H.load(Ordering::Acquire).clamp(1, 4096) as u32;
        let Some(win) = GameWindow::create("MACVM Game", w as f64, h as f64) else {
            eprintln!("macvm-cocoa: no Metal device — game pane unavailable");
            return;
        };
        // The WORLD may be larger than the viewport (`GamePane>>overscan:`),
        // which is what makes `Scroll` mean anything: with world == viewport
        // the engine clamps every scroll to (0,0). A demo that asks for
        // overscan starts centred in it, so it can scroll either way.
        let margin = REQ_OVERSCAN.load(Ordering::Acquire).clamp(0, 256) as u32;
        let Ok(mut pane) = IndexedPane::new(&win.device, w + 2 * margin, h + 2 * margin, w, h)
        else {
            return;
        };
        if margin > 0 {
            pane.set_scroll(margin as i64, margin as i64);
        }
        let Ok(sprites) = Sprites::new(&win.device) else {
            return;
        };
        let Ok(text) = TextOverlay::new(&win.device, w, h) else {
            return;
        };
        // The close button (red) must stop the demo + not dangle the window:
        // `setReleasedWhenClosed: false` keeps the NSWindow alive (its raw ptr
        // stays valid until we drop the pane), and a `windowWillClose:` delegate
        // routes the close through the SAME top-level stop path as Escape.
        let window = objc::send0(win.view, objc::sel("window"));
        if !window.is_null() {
            objc::send1_bool(window, objc::sel("setReleasedWhenClosed:"), false);
            objc::send1_id(window, objc::sel("setDelegate:"), game_window_delegate());
        }
        *cell.borrow_mut() = Some(NativeGame {
            win,
            pane,
            direct: None,
            sprites,
            text,
            shader: None,
            sprite_ids: HashMap::new(),
            w,
            h,
            timer: objc::NIL,
        });
        // Start this session with a clean held-key table. HELD_KEYS is only
        // cleared by a keyUp: delivered to the game's OWN view, so an Escape
        // that closed the PREVIOUS demo — its keyUp: has nowhere to go, since
        // that view is already gone — would otherwise still read as held here.
        // The frame loop's level-triggered Escape check (`game_tick`, below)
        // would then close THIS brand-new window on its very first tick,
        // before a single frame is seen: the "second demo exits immediately"
        // bug, fixed once already in the WKWebView GUI's `display_game_pane`
        // (`gui/src/main.rs`) but never carried over here.
        macgamepane_graphics::input::clear_all();
    });
}

/// Rebuild the pane+window at the current `REQ_W`/`REQ_H` WITHOUT ending the
/// session (unlike `close_window`, which stops the demo): release the old
/// window/timer, then let `ensure_pane` recreate at the new size and restart
/// the frame timer if a loop was running. Only the rare mid-session
/// `SetPaneSize` reaches here.
fn rebuild_pane_at_requested_size() {
    GAME.with(|cell| {
        if let Some(g) = cell.borrow_mut().take() {
            objc::send0(g.timer, objc::sel("invalidate"));
            let w = objc::send0(g.win.view, objc::sel("window"));
            if !w.is_null() {
                objc::send1_id(w, objc::sel("setDelegate:"), objc::NIL);
                objc::send1_id(w, objc::sel("orderOut:"), objc::NIL);
                objc::send1_id(w, objc::sel("close"), objc::NIL);
                objc::send0(w, objc::sel("release"));
            }
            objc::send0(g.win.view, objc::sel("release"));
        }
    });
    ensure_pane();
    if GAME_ACTIVE.load(Ordering::Acquire) {
        start_frame_timer();
    }
}

/// Start the default-mode frame timer (tracking-safe) at the requested rate
/// (`REQ_FPS`, default 60; a demo may request another via `GamePane>>frameRate:`).
/// Its `gameTick:` flags a step due + reads keys; the primary's supervisor loop
/// runs the step. Idempotent — invalidates any prior timer first.
fn start_frame_timer() {
    GAME.with(|cell| {
        if let Some(g) = cell.borrow_mut().as_mut() {
            if g.timer != objc::NIL {
                objc::send0(g.timer, objc::sel("invalidate"));
            }
            let fps = REQ_FPS.load(Ordering::Acquire).clamp(1, 120) as f64;
            eprintln!("macvm-cocoa: game frame timer at {fps} fps");
            g.timer = objc::scheduled_timer(
                1.0 / fps,
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
    // Close the SESSION first: a Blit/Present from the last in-flight frame may
    // still be queued behind this stop, and the drain must NOT re-create the
    // window we're about to close (the "window reopens after stop" bug).
    SESSION_OPEN.store(false, Ordering::Release);
    // The next demo starts from the default size unless IT requests otherwise
    // — otherwise galaxigans' 640x360 would leak into the next Breakout launch.
    REQ_W.store(PANE_W as i64, Ordering::Release);
    REQ_H.store(PANE_H as i64, Ordering::Release);
    REQ_FPS.store(DEFAULT_FPS, Ordering::Release);
    REQ_OVERSCAN.store(0, Ordering::Release);
    // Retract the framebuffer BEFORE the buffers are dropped below: any Alien
    // the demo still holds must read as "no screen" rather than as memory that
    // is about to be freed.
    macvm::embed::clear_screen_memory();
    // Forget where the mouse was; the next demo's first frame should not see a
    // pointer position left behind by this one.
    GAME_MOUSE_X.store(-1, Ordering::Release);
    GAME_MOUSE_Y.store(-1, Ordering::Release);
    GAME_MOUSE_BUTTONS.store(0, Ordering::Release);
    GAME.with(|cell| {
        if let Some(g) = cell.borrow_mut().take() {
            objc::send0(g.timer, objc::sel("invalidate"));
            // Read the window via the view FIRST (removeFromSuperview would nil
            // view.window), detach the delegate so `close` can't re-fire our
            // windowWillClose:, then close it — releasedWhenClosed:false keeps
            // the object alive for the raw ptrs until this scope drops it.
            let w = objc::send0(g.win.view, objc::sel("window"));
            if !w.is_null() {
                objc::send1_id(w, objc::sel("setDelegate:"), objc::NIL);
                objc::send1_id(w, objc::sel("orderOut:"), objc::NIL);
                objc::send1_id(w, objc::sel("close"), objc::NIL);
                // `setReleasedWhenClosed:false` (ensure_pane) means AppKit will
                // NOT release the window on close — that was only ever to keep
                // the raw ptr valid for OUR OWN use up to this point. Now that
                // every use is done, release the two objects WE alloc'd
                // (`GameWindow::create`'s `window_alloc`/`alloc_init("MacGamePaneKeyView")`
                // each hand back a +1 nothing else was releasing): the window
                // first (its own dealloc drops its retain on the content
                // view), then our own +1 on the view. Every prior session
                // leaked one full NSWindow + CAMetalLayer-backed view +
                // window-server surface — repeated Demos use accumulated them
                // without limit.
                objc::send0(w, objc::sel("release"));
            }
            objc::send0(g.win.view, objc::sel("release"));
        }
    });
    GAME_ACTIVE.store(false, Ordering::Release);
}

/// Upload + present one frame into the layer's next drawable (main thread).
/// Layer order (bottom→top), the starfield_demo compositing: the shader (if
/// any) clears + fills layer 0, the indexed pane composites over it with
/// `Load` (its transparent index 0 lets the shader show through), then sprites,
/// then the text overlay on top. With no shader the pane clears the frame.
fn present(g: &mut NativeGame) {
    // A DIRECT pane short-circuits the whole retained path: the VM has been
    // storing pixels into shared memory the GPU samples, so there is nothing to
    // upload and no indexed buffer to composite. Draw it, then republish —
    // rendering ROTATES which buffer is the write target, and the VM must be
    // told, or its next frame would be drawn into the one being displayed.
    if g.direct.is_some() {
        g.text.upload();
        let Some(drawable) = g.win.layer.next_drawable() else {
            return;
        };
        let tex = drawable.texture();
        let cb = g.win.command_queue.new_command_buffer();
        if let Some(d) = g.direct.as_mut() {
            d.render(cb, tex);
        }
        g.text.render(cb, tex);
        cb.present_drawable(drawable);
        cb.commit();
        publish_direct(g);
        return;
    }
    g.pane.upload();
    g.text.upload();
    let Some(drawable) = g.win.layer.next_drawable() else {
        return; // compositor has none this instant — skip, not an error
    };
    let tex = drawable.texture();
    let cb = g.win.command_queue.new_command_buffer();
    let pane_load = if let Some(sh) = g.shader.as_mut() {
        sh.render(cb, tex);
        MTLLoadAction::Load
    } else {
        MTLLoadAction::Clear
    };
    g.pane.render(cb, tex, pane_load);
    g.sprites
        .render(cb, tex, 0.0, 0.0, g.w as f64, g.h as f64);
    g.text.render(cb, tex);
    cb.present_drawable(drawable);
    cb.commit();
}

/// Tell the VM where the direct framebuffer is right now. Called when the pane
/// is built and again after every present, because presenting rotates the write
/// buffer. This is the one piece of pane state that travels by publication
/// rather than by command — a primitive has to RETURN an address, and the
/// command channel only runs the other way.
fn publish_direct(g: &NativeGame) {
    if let Some(d) = g.direct.as_ref() {
        macvm::embed::publish_screen_memory(d.backbuffer_ptr(), d.stride(), d.height() as usize);
    }
}

/// Apply one `GameCommand` to the pane (main thread) — the exact vocabulary
/// game_pane.rs's `apply_command` handles. Loop control (`StartLoop`/`StopLoop`)
/// is handled by the caller; here we only draw/present.
fn apply(g: &mut NativeGame, cmd: &GameCommand) {
    use GameCommand as C;
    match cmd {
        // Palette entries reach BOTH panes: a direct-mode demo still says
        // `paletteAt:r:g:b:`, and its shader samples its own palette buffer.
        C::PaletteAt { index, r, g: gg, b } => {
            g.pane.set_rgb(*index, *r, *gg, *b);
            if let Some(d) = g.direct.as_mut() {
                d.set_rgb(*index, *r, *gg, *b);
            }
        }
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
                // `moveTo:y:` re-shows a hidden sprite (gamepane.mst contract:
                // "Stop drawing me until my next moveTo:"). Without this a sprite
                // that was hidden — e.g. galaxigans' ship on the attract screen —
                // stays invisible once play starts and only moveTo:y: moves it.
                g.sprites.show(inst);
                g.sprites.move_to(inst, *x as f64, *y as f64);
            }
        }
        // ── GamePane extensions ──
        C::Text { x, y, text, r, g: gg, b, scale } => {
            g.text.draw_text(*x, *y, text, *r, *gg, *b, *scale);
        }
        C::TextClear => g.text.clear(),
        C::AddFrame { id, rows } => {
            if let Some(&(def, _)) = g.sprite_ids.get(id) {
                g.sprites.add_frame(def, rows);
            }
        }
        C::PlaceSprite { id, x, y, frame } => {
            if let Some(&(_, inst)) = g.sprite_ids.get(id) {
                g.sprites.set_frame(inst, *frame as usize);
                g.sprites.show(inst);
                g.sprites.move_to(inst, *x as f64, *y as f64);
            }
        }
        C::HideSprite { id } => {
            if let Some(&(_, inst)) = g.sprite_ids.get(id) {
                g.sprites.hide(inst);
            }
        }
        C::Shader { src } => match ShaderPane::new(&g.win.device, src) {
            Ok(mut sh) => {
                sh.set_aspect(g.w as f32 / g.h as f32);
                g.shader = Some(sh);
            }
            Err(e) => eprintln!("macvm-cocoa: game shader failed to compile: {e}"),
        },
        C::ShaderParam { index, value } => {
            if let Some(sh) = g.shader.as_mut() {
                sh.set_param(*index, *value);
            }
        }
        C::LinePalette { line, index, r, g: gg, b } => {
            g.pane.set_line_rgb(*line, *index, *r, *gg, *b);
        }
        C::Present => present(g),
        // Move the viewport within the world. Deliberately NOT a redraw: it
        // sets a shader uniform, so a demo can pan or shake every frame for
        // the cost of one command. Clamped inside the engine.
        C::Scroll { x, y } => g.pane.set_scroll(*x, *y),
        // Handled in `drain` — it builds the pane and publishes its address.
        C::OpenDirect { .. } => {}
        // SetPaneSize/SetOverscan are handled in `drain` (before the pane
        // exists); loop and audio are handled there too. A stray one here is a
        // harmless no-op.
        C::SetPaneSize { .. }
        | C::SetOverscan { .. }
        | C::SetFrameRate { .. }
        | C::StartLoop
        | C::StopLoop
        | C::PlaySound { .. }
        | C::PlayEffect { .. }
        | C::PlayTune { .. } => {}
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
                SESSION_OPEN.store(true, Ordering::Release);
                ensure_pane();
                start_frame_timer();
                GAME_ACTIVE.store(true, Ordering::Release);
            }
            GameCommand::StopLoop => close_window(),
            GameCommand::PlaySound { preset } => audio::play_sound(*preset),
            GameCommand::PlayEffect { params } => audio::play_effect(params),
            GameCommand::PlayTune { abc } => audio::play_tune(abc),
            // Record the requested size so the NEXT `ensure_pane` builds at it
            // (a demo sends this before any draw — no pane exists yet, the
            // common path). If a pane already exists (a rare mid-session
            // resize), rebuild it at the new size, keeping the session live.
            GameCommand::SetPaneSize { w, h } => {
                REQ_W.store(*w as i64, Ordering::Release);
                REQ_H.store(*h as i64, Ordering::Release);
                let exists = GAME.with(|cell| cell.borrow().is_some());
                if exists && SESSION_OPEN.load(Ordering::Acquire) {
                    rebuild_pane_at_requested_size();
                }
            }
            // Build the direct framebuffer and publish where it is. The pane
            // (and window) must exist first — a demo may send this as its very
            // first command, before anything has drawn.
            GameCommand::OpenDirect { w, h } => {
                if !SESSION_OPEN.load(Ordering::Acquire)
                    && !STOP_DUE.load(Ordering::Acquire)
                    && !RESET_DUE.load(Ordering::Acquire)
                {
                    SESSION_OPEN.store(true, Ordering::Release);
                    GAME_ACTIVE.store(true, Ordering::Release);
                }
                REQ_W.store(*w as i64, Ordering::Release);
                REQ_H.store(*h as i64, Ordering::Release);
                ensure_pane();
                GAME.with(|cell| {
                    if let Some(g) = cell.borrow_mut().as_mut() {
                        match DirectPane::new(&g.win.device, *w, *h) {
                            Ok(d) => {
                                macvm::embed::publish_screen_memory(
                                    d.backbuffer_ptr(),
                                    d.stride(),
                                    d.height() as usize,
                                );
                                eprintln!(
                                    "macvm-cocoa: direct framebuffer {}x{} stride {}",
                                    *w,
                                    *h,
                                    d.stride()
                                );
                                g.direct = Some(d);
                            }
                            Err(e) => eprintln!("macvm-cocoa: direct pane failed: {e}"),
                        }
                    }
                });
            }
            // How much bigger than the viewport the world should be. Like
            // SetPaneSize this decides how the pane is BUILT, so it must
            // arrive before the pane exists; a demo sends it during start.
            GameCommand::SetOverscan { margin } => {
                REQ_OVERSCAN.store(*margin as i64, Ordering::Release);
                let exists = GAME.with(|cell| cell.borrow().is_some());
                if exists && SESSION_OPEN.load(Ordering::Acquire) {
                    rebuild_pane_at_requested_size();
                }
            }
            // Record the requested fps. If the timer is already running (a demo
            // set the rate after StartLoop), restart it so the new interval
            // takes effect; the common case is a demo setting it during start,
            // before StartLoop, where start_frame_timer picks it up directly.
            GameCommand::SetFrameRate { fps } => {
                REQ_FPS.store(*fps as i64, Ordering::Release);
                let running = GAME.with(|cell| {
                    cell.borrow().as_ref().map(|g| g.timer != objc::NIL).unwrap_or(false)
                });
                if running && SESSION_OPEN.load(Ordering::Acquire) {
                    start_frame_timer();
                }
            }
            // Every draw/palette/present command: create the pane on the FIRST
            // one of a session (a demo paints its opening scene before StartLoop).
            // Only while a session is open — a stray frame arriving AFTER a stop
            // must not resurrect the window (SESSION_OPEN gates the create).
            _ => {
                // FRIENDLY IMPLICIT SESSION. Before this, a game command with
                // no session open was silently dropped: someone typing
                // `GamePane new … cls: … present` in the Workspace got no
                // window, no picture and no error — the most disappointing
                // failure in the system, and the first one a beginner meets.
                // Now the first command opens a session and a window around
                // it, so drawing just draws. The frame timer starts too, so
                // Escape closes it exactly like a launched game (Escape is
                // read in the timer callback; without it the window would be
                // unclosable). A launch still owns the frame-loop path — this
                // only rescues drawing that would otherwise vanish.
                //
                // Guarded on STOP_DUE/RESET_DUE so the older hazard stays
                // fixed: a stray frame from a demo that was just stopped must
                // NOT resurrect the window it was closing.
                if !SESSION_OPEN.load(Ordering::Acquire)
                    && !STOP_DUE.load(Ordering::Acquire)
                    && !RESET_DUE.load(Ordering::Acquire)
                {
                    SESSION_OPEN.store(true, Ordering::Release);
                    GAME_ACTIVE.store(true, Ordering::Release);
                    ensure_pane();
                    start_frame_timer();
                }
                if SESSION_OPEN.load(Ordering::Acquire) {
                    ensure_pane();
                }
                GAME.with(|cell| {
                    if let Some(g) = cell.borrow_mut().as_mut() {
                        apply(g, cmd);
                    }
                });
            }
        }
    }
}

/// Is a game loop currently running, or a launch pending? The primary's
/// supervisor loop uses this to spin fast (so band replies + frame steps flow
/// at ~60Hz) instead of parking in its idle metrics beat.
pub fn is_active() -> bool {
    GAME_ACTIVE.load(Ordering::Acquire)
        || RESET_DUE.load(Ordering::Acquire)
        || PENDING_LAUNCH.lock().map(|g| g.is_some()).unwrap_or(false)
}

/// Service a stop request on the MAIN thread (called from `drain_perform`):
/// close the window + stop the timer HERE (a demo has no host-callable stop —
/// `GamePane>>stop` is an instance method the class doesn't understand), and
/// flag the supervisor to `GamePane reset` the demo's step block. This is what
/// makes Escape / the close button / a re-launch actually dismiss the pane.
pub fn service_stop_on_main() {
    if STOP_DUE.swap(false, Ordering::AcqRel) {
        close_window(); // GAME_ACTIVE → false, timer off, window hidden
        RESET_DUE.store(true, Ordering::Release);
    }
}

/// Launch a demo (from the Demos menu, host-side): run its entry doit TOP-LEVEL
/// on the primary. One at a time — if a game is already running, tear it down
/// first (the launch fires once it has). Waking the run loop is the caller's.
pub fn request_launch(entry: String) {
    if GAME_ACTIVE.load(Ordering::Acquire) {
        STOP_DUE.store(true, Ordering::Release); // stop the current demo first
    }
    if let Ok(mut slot) = PENDING_LAUNCH.lock() {
        *slot = Some(entry);
    }
    objc::wake_main_runloop();
}

/// Request the running demo stop (Escape, the close button). The supervisor
/// runs the stop top-level; its StopLoop closes the window.
pub fn request_stop() {
    STOP_DUE.store(true, Ordering::Release);
    objc::wake_main_runloop();
}

/// Press the game window's red close button for real (scripted verification of
/// the close-button path): `performClose:` runs AppKit's full close sequence,
/// firing our `windowWillClose:` delegate exactly as a click would. Main-thread
/// only (the control drain calls this). Returns false if no window is open.
pub fn press_close_button() -> bool {
    GAME.with(|cell| {
        if let Some(g) = cell.borrow().as_ref() {
            let w = objc::send0(g.win.view, objc::sel("window"));
            if !w.is_null() {
                objc::send1_id(w, objc::sel("performClose:"), objc::NIL);
                return true;
            }
        }
        false
    })
}

/// Deliver a synthetic mouse press or release at a PANE PIXEL, for scripted
/// verification of the mouse path (`gamemouse` on the control channel) — the
/// same role `press_close_button` plays for the red close button. Main-thread
/// only (the control drain calls this). Returns false if no game window is open.
///
/// This routes through `-[NSApplication sendEvent:]` rather than calling the
/// view's `mouseDown:` directly, ON PURPOSE: the interesting question is
/// whether AppKit delivers a mouse event to MacGamePane's key-capable view at
/// all, and calling the IMP by hand would assume the very thing under test.
/// What it does NOT cover is the hardware/WindowServer leg in front of that —
/// a real click also proves the window is front and accepting input.
///
/// `button`: 0 = left, 1 = right. `down`: press, else release.
pub fn synth_mouse(pane_x: i64, pane_y: i64, button: i64, down: bool) -> bool {
    // NSEventType: LeftMouseDown 1, LeftMouseUp 2, RightMouseDown 3,
    // RightMouseUp 4.
    let event_type: u64 = match (button, down) {
        (0, true) => 1,
        (0, false) => 2,
        (_, true) => 3,
        (_, false) => 4,
    };
    GAME.with(|cell| {
        let borrowed = cell.borrow();
        let Some(g) = borrowed.as_ref() else {
            return false;
        };
        let window = objc::send0(g.win.view, objc::sel("window"));
        if window.is_null() {
            return false;
        }
        // Pane pixel -> the CENTRE of that pixel as a fraction of the pane,
        // then up to the view's real size in points (which is not the pane
        // size once the window has been resized), then flipped: an unflipped
        // NSView and the window's base coordinates both count y from the
        // BOTTOM, while a pane pixel counts from the top.
        let bounds = objc::send0_rect(g.win.view, objc::sel("bounds"));
        let fx = (pane_x as f64 + 0.5) / g.w as f64;
        let fy = (pane_y as f64 + 0.5) / g.h as f64;
        let x = bounds.x + fx * bounds.w;
        let y = bounds.y + (1.0 - fy) * bounds.h;
        let event = objc::send_mouse_event(
            objc::get_class("NSEvent"),
            objc::sel(
                "mouseEventWithType:location:modifierFlags:timestamp:windowNumber:context:\
                 eventNumber:clickCount:pressure:",
            ),
            event_type,
            x,
            y,
            0,
            0.0,
            objc::window_number(window),
            objc::NIL,
            0,
            1,
            if down { 1.0 } else { 0.0 },
        );
        if event.is_null() {
            return false;
        }
        // Send to the GAME WINDOW, not to NSApp. `-[NSApplication sendEvent:]`
        // routes a key event to whichever window is KEY, so with the IDE
        // window in front a scripted keystroke lands in the documentation
        // browser instead of the game (observed exactly that). The window's
        // own `sendEvent:` still runs AppKit's real dispatch — hit-testing for
        // mouse, first responder for keys — into the view under test.
        objc::send1_id(window, objc::sel("sendEvent:"), event);
        true
    })
}

/// The game window's window number, for a scripted screenshot of THAT window
/// (the IDE window is often still key, so `snapshot_client_area`'s `keyWindow`
/// would photograph the wrong one). `None` if no game window is open.
pub fn game_window_number() -> Option<i64> {
    GAME.with(|cell| {
        let borrowed = cell.borrow();
        let g = borrowed.as_ref()?;
        let window = objc::send0(g.win.view, objc::sel("window"));
        if window.is_null() {
            return None;
        }
        Some(objc::window_number(window))
    })
}

/// Deliver a synthetic key press or release to the game window, for scripted
/// verification (`gamekey` on the control channel) — the keyboard twin of
/// [`synth_mouse`], and the way a script can pause a demo before drawing on it.
/// `key_code` is a macOS virtual key code (49 = space). Main-thread only.
pub fn synth_key(key_code: u16, down: bool, characters: &str) -> bool {
    // NSEventType: KeyDown 10, KeyUp 11.
    let event_type: u64 = if down { 10 } else { 11 };
    GAME.with(|cell| {
        let borrowed = cell.borrow();
        let Some(g) = borrowed.as_ref() else {
            return false;
        };
        let window = objc::send0(g.win.view, objc::sel("window"));
        if window.is_null() {
            return false;
        }
        let chars = objc::nsstring(characters);
        let event = objc::send_key_event(
            objc::get_class("NSEvent"),
            objc::sel(
                "keyEventWithType:location:modifierFlags:timestamp:windowNumber:context:\
                 characters:charactersIgnoringModifiers:isARepeat:keyCode:",
            ),
            event_type,
            0.0,
            0.0,
            0,
            0.0,
            objc::window_number(window),
            objc::NIL,
            chars,
            chars,
            false,
            key_code,
        );
        if event.is_null() {
            return false;
        }
        // Send to the GAME WINDOW, not to NSApp. `-[NSApplication sendEvent:]`
        // routes a key event to whichever window is KEY, so with the IDE
        // window in front a scripted keystroke lands in the documentation
        // browser instead of the game (observed exactly that). The window's
        // own `sendEvent:` still runs AppKit's real dispatch — hit-testing for
        // mouse, first responder for keys — into the view under test.
        objc::send1_id(window, objc::sel("sendEvent:"), event);
        true
    })
}

/// Called by the PRIMARY's supervisor loop each iteration: the next thing to
/// `exec` TOP-LEVEL (never nested — a nested entry corrupts the interp-regs
/// mirror root, crashing the next frame's compile-GC). Priority: stop the
/// current demo, then launch a pending one once the old has torn down, then a
/// frame step when the timer says one is due.
pub fn poll_primary_step() -> Option<String> {
    // A host-side stop already closed the window; reset the demo's step block
    // (class-side, valid — unlike `GamePane stop`, which is instance-side).
    if RESET_DUE.swap(false, Ordering::AcqRel) {
        return Some("GamePane reset".to_string());
    }
    // Launch only when no game is active — i.e. the prior demo was torn down,
    // so one runs at a time. Opening the session here (before the demo paints
    // its first scene) lets the drain create the pane on that first command.
    if !GAME_ACTIVE.load(Ordering::Acquire) {
        let launch = PENDING_LAUNCH.lock().ok().and_then(|mut g| g.take());
        if launch.is_some() {
            SESSION_OPEN.store(true, Ordering::Release);
        }
        return launch;
    }
    if !STEP_DUE.swap(false, Ordering::AcqRel) {
        return None;
    }
    let mask = GAME_KEY_MASK.load(Ordering::Relaxed);
    let mx = GAME_MOUSE_X.load(Ordering::Relaxed);
    let my = GAME_MOUSE_Y.load(Ordering::Relaxed);
    let mb = GAME_MOUSE_BUTTONS.load(Ordering::Relaxed);
    Some(format!(
        "GamePane stepWithKeys: {mask} mouseX: {mx} y: {my} buttons: {mb}"
    ))
}

// ── the frame timer's target (an ObjC object with a `gameTick:` method) ──────

/// Called ~60Hz on the main run loop (default mode). Reads Escape + the held-key
/// mask, flags a step due, and lets the drain do the VM work (it holds the UI
/// worker). Keeps zero VM access here — a timer IMP has no `DrainState`.
extern "C" fn game_tick(_this: objc::Id, _cmd: objc::Sel, _timer: objc::Id) {
    use macgamepane_graphics::input::key_held;
    if key_held(KEY_ESCAPE) {
        request_stop();
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
    read_mouse_into_pane_pixels();
    STEP_DUE.store(true, Ordering::Release);
    objc::wake_main_runloop();
}

/// Turn the engine's normalized mouse position into this session's pane pixels
/// and stash it for the next step. Runs on the main thread (the timer IMP), the
/// only place `GAME` — and so the session's actual pane size — is reachable;
/// `poll_primary_step` runs on the primary's supervisor thread and just reads
/// the atomics, exactly as it already does for the key mask.
///
/// The right edge is the reason for the `min`: a fraction of exactly 1.0 (the
/// clamp in `record_position`, or a click on the last pixel column) would
/// otherwise index one past the last cell.
fn read_mouse_into_pane_pixels() {
    let (fx, fy, left, right) = macgamepane_graphics::input::mouse_state();
    let (px, py) = GAME.with(|cell| match cell.borrow().as_ref() {
        Some(g) => (
            ((fx * g.w as f64) as i64).min(g.w as i64 - 1),
            ((fy * g.h as f64) as i64).min(g.h as i64 - 1),
        ),
        None => (-1, -1),
    });
    GAME_MOUSE_X.store(px, Ordering::Relaxed);
    GAME_MOUSE_Y.store(py, Ordering::Relaxed);
    GAME_MOUSE_BUTTONS.store(i64::from(left) | (i64::from(right) << 1), Ordering::Relaxed);
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

/// The game window's `NSWindowDelegate`: its close button (red) routes through
/// the same top-level stop as Escape, so closing the window stops the demo and
/// frees the pane for the next one.
extern "C" fn game_window_will_close(_this: objc::Id, _cmd: objc::Sel, _note: objc::Id) {
    request_stop();
}

/// Register a `MacvmGameWindowDelegate` with `windowWillClose:` once; return a
/// shared instance.
fn game_window_delegate() -> objc::Id {
    use std::sync::OnceLock;
    static DELEGATE: OnceLock<usize> = OnceLock::new();
    *DELEGATE.get_or_init(|| {
        type ImpV1 = extern "C" fn(objc::Id, objc::Sel, objc::Id);
        objc::register_class(
            "MacvmGameWindowDelegate",
            &[(
                "windowWillClose:",
                game_window_will_close as ImpV1 as *const std::ffi::c_void,
                "v@:@",
            )],
        );
        objc::alloc_init("MacvmGameWindowDelegate") as usize
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

    /// The parametric wire's landing (asset_editors_design.md §3): rebuild
    /// the `Effect` from the flat contract, render with the SEEDED Lcg (the
    /// seed crosses so noisy recipes reproduce exactly), and play through the
    /// same `Sfx` engine as the presets. Slot 16 — above the preset bitmask's
    /// 0..15, well under `MAX_SFX` (64) — is THE effect-audition slot;
    /// auditions replace each other, which is what an editor wants.
    /// Fail-soft: malformed params are a no-op, never a panic (the drain runs
    /// on the main thread).
    /// The parametric wire's landing (asset_editors_design.md §3): decode
    /// through the synth crate's OWN `effect_from_params` — the contract has
    /// exactly one decoder, shared with the web GUI — render with the seeded
    /// Lcg (noisy recipes reproduce exactly), play through the same `Sfx`
    /// engine as the presets. Slot 16 — above the preset bitmask's 0..15,
    /// well under `MAX_SFX` — is THE audition slot; auditions replace each
    /// other, which is what an editor wants. Fail-soft on malformed params.
    pub fn play_effect(params: &[f64]) {
        use macgamepane_audio::synth as s;
        let Some((e, seed)) = s::effect_from_params(params) else {
            return;
        };
        let mut rng = s::Lcg::new(seed);
        let sound = s::render(&e, &mut rng);
        const EFFECT_SLOT: usize = 16;
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
            sfx.define(EFFECT_SLOT, &sound);
            sfx.play(EFFECT_SLOT);
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
            9 => s::blip(660.0, 0.12),
            10 => s::saucer(0.9),    // UFO warble (galaxigans)
            11 => s::boss_hum(1.8),  // tractor-beam hum (galaxigans)
            _ => s::blip(660.0, 0.12),
        }
    }
}
