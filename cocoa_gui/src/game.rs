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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::Mutex;

use macgamepane_graphics::direct_pane::DirectPane;
use macgamepane_graphics::indexed_pane::IndexedPane;
use macgamepane_graphics::shader_pane::ShaderPane;
use macgamepane_graphics::sprites::Sprites;
use macgamepane_graphics::text_overlay::TextOverlay;
use macgamepane_graphics::text_plane::TextPlane;
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

/// Which pane a command is FOR (`docs/multi_pane_design.md` §2a).
///
/// Minted per game-sink grant, so it names the VM that opened the pane — the
/// sink is already per-VM, which is why the id lives there rather than on
/// every `GameCommand` variant as the design first sketched. That difference
/// is the whole reason this step touches none of the thirty-one emit sites.
///
/// Monotonic and never reused: a stale id from a demo that has ended can
/// therefore never address a live pane, which is the same safety the fleet's
/// generational handles buy, for free.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PaneId(u32);

static NEXT_PANE_ID: AtomicU32 = AtomicU32::new(1);

impl PaneId {
    fn mint() -> Self {
        PaneId(NEXT_PANE_ID.fetch_add(1, Ordering::Relaxed))
    }
    /// The pane every command addressed before there was addressing.
    pub fn legacy() -> Self {
        PaneId(0)
    }
    /// The bare number, for the parts that cross into the VM crate (the input
    /// service's subject, via `GameSink::pane_id`).
    pub fn raw(self) -> u32 {
        self.0
    }
}

/// Commands emitted by the game sinks, drained on main. A plain queue + the
/// existing run-loop wake — the same shape as `ChannelGameSink` — now carrying
/// WHO each command is for beside it.
static GAME_CMDS: Mutex<VecDeque<(PaneId, GameCommand)>> = Mutex::new(VecDeque::new());
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
// THIS TICK'S INPUT LIVES IN THE VM NOW (`macvm::runtime::game_input`, process
// services S6). It used to be four atomics here, which meant only code that
// could see THIS crate could read them — so a demo learned what the keyboard
// was doing only because the supervisor formatted the numbers into a doit
// string and exec'd it on the primary. Moved into the VM, any VM's own tick
// can ask, which is what lets a demo run in a VM of its own. The frame tick
// still WRITES here (it alone knows this session's pane size, so it alone can
// convert the pointer into pane pixels); only the ownership of the state moved.
use macvm::runtime::game_input;
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
pub struct PrimaryGameSink {
    /// The pane this VM draws into. Stamped on every command it emits, so the
    /// drain can tell two demos apart without either of them saying so.
    pane: PaneId,
}

impl PrimaryGameSink {
    /// A sink for a VM that may draw. Each grant mints its own pane id — the
    /// worker-boot fn hands one to every spawned VM, so a demo in a VM of its
    /// own is addressable from the moment it can draw at all.
    pub fn new() -> Self {
        Self {
            pane: PaneId::mint(),
        }
    }
    pub fn pane(&self) -> PaneId {
        self.pane
    }
}

impl Default for PrimaryGameSink {
    fn default() -> Self {
        Self::new()
    }
}

impl GameSink for PrimaryGameSink {
    fn pane_id(&self) -> u32 {
        self.pane.raw()
    }

    fn emit(&mut self, cmd: GameCommand) {
        if let Ok(mut q) = GAME_CMDS.lock() {
            q.push_back((self.pane, cmd));
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
    /// The TEXT PLANE (SM1): a cell grid in shared memory the VM writes with
    /// stores instead of `Text` commands. Always present, always composited
    /// last; an untouched grid is all char-0 cells and draws nothing, so it
    /// costs a demo that ignores it exactly nothing.
    text_plane: Option<TextPlane>,
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
    /// Every open pane, by id (`docs/multi_pane_design.md` §2b). Main-thread
    /// and thread-local exactly as the single cell it replaces; only the
    /// cardinality changed.
    ///
    /// It holds at most one entry today, because a launch still tears the
    /// previous demo down (§2e, the last step) — but the *shape* is what the
    /// rest of the design needs: a command routes to its own pane, and two
    /// demos stop being a contradiction.
    static GAMES: std::cell::RefCell<HashMap<PaneId, NativeGame>> =
        std::cell::RefCell::new(HashMap::new());
}

/// The pane the session-wide operations act on — the frame timer, Escape, the
/// close button, scripted input, the screenshot verb.
///
/// These are the parts that have no command to carry an id: a keystroke does
/// not say which window it is for, the timer belongs to no demo, and `snapgame`
/// is asked from outside. Under the adopted focus rule the answer will be *the
/// focused window*; until panes can coexist there is only ever one open, so
/// "the one that exists" and "the focused one" are the same pane, and this
/// helper is where that assumption is written down instead of being spread
/// across ten call sites.
fn current_pane() -> Option<PaneId> {
    GAMES.with(|cell| cell.borrow().keys().copied().next())
}

/// Run `f` against the session's current pane, if there is one.
fn with_current<R>(f: impl FnOnce(&mut NativeGame) -> R) -> Option<R> {
    let id = current_pane()?;
    GAMES.with(|cell| cell.borrow_mut().get_mut(&id).map(f))
}

/// Open the game window + pane once (main thread), lazily on the FIRST command
/// of a session — a demo sets its palette + draws its opening scene BEFORE
/// `GamePane>>run` (StartLoop), so waiting for StartLoop would drop all of that
/// (the Breakout "all-red palette" bug). `None` if the Mac has no Metal device.
/// `GameWindow::create` does the whole NSWindow + CAMetalLayer + key-capable-view
/// wiring we would otherwise hand-roll. The frame timer is NOT started here —
/// only on StartLoop (`start_frame_timer`).
fn ensure_pane_for(id: PaneId) {
    GAMES.with(|cell| {
        if cell.borrow().contains_key(&id) {
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
        // The text plane is built with the pane and lives as long as it. An
        // untouched grid draws nothing, so this is free for every demo that
        // does not use it, and instantly there for every demo that does.
        let pane_palette_ptr = pane.palette_ptr();
        let pane_palette_entries = pane.palette_entries();
        let pane_palette_global_base = pane.palette_global_base();
        let text_plane = TextPlane::new(&win.device, w, h).ok();
        if let Some(tp) = text_plane.as_ref() {
            macvm::embed::publish_text_memory(tp.cells_ptr(), tp.cols() as usize, tp.rows() as usize);
        }
        // A NEW PANE TAKES THE KEYBOARD. That is what happens on screen — the
        // window opens in front and becomes key — and saying so here is what
        // gives `primInputState` a subject to answer for. When panes can
        // coexist this same call moves to `windowDidBecomeKey:`, which is the
        // signal that actually means focus rather than merely "newest".
        game_input::set_focus(id.raw());
        // `id` is the PaneId; the `pane` field below is the IndexedPane.
        cell.borrow_mut().insert(id, NativeGame {
            win,
            pane,
            direct: None,
            text_plane,
            sprites,
            text,
            shader: None,
            sprite_ids: HashMap::new(),
            w,
            h,
            timer: objc::NIL,
        });
        // Publish the palette buffer too (SM4): a demo can rewrite colours —
        // including all 240 per-scanline entries, every frame — with byte
        // stores instead of a command each. No coordination is needed with
        // `paletteAt:`, because the pane's setters write THIS SAME buffer:
        // there is one palette and no CPU mirror, so there is nothing that
        // could copy stale colours over a demo's work. Published at creation
        // because the buffer lives as long as the pane and never moves.
        macvm::embed::publish_palette_memory(
            pane_palette_ptr,
            pane_palette_entries,
            pane_palette_global_base,
        );

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
    // Rebuild the SAME pane: drop its window and recreate under its own id, so
    // a mid-session resize does not hand the demo a different pane than the one
    // its commands are addressed to.
    let Some(id) = current_pane() else { return };
    GAMES.with(|cell| {
        if let Some(g) = cell.borrow_mut().remove(&id) {
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
    ensure_pane_for(id);
    if GAME_ACTIVE.load(Ordering::Acquire) {
        start_frame_timer();
    }
}

/// Start the default-mode frame timer (tracking-safe) at the requested rate
/// (`REQ_FPS`, default 60; a demo may request another via `GamePane>>frameRate:`).
/// Its `gameTick:` flags a step due + reads keys; the primary's supervisor loop
/// runs the step. Idempotent — invalidates any prior timer first.
fn start_frame_timer() {
    with_current(|g| {
        {
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
    // THE PER-DEMO REQUESTS ARE RESET AT LAUNCH, NOT HERE (`request_launch`).
    // They used to be cleared on this path, and that handed galaxigans back
    // its 60fps default while it was still playing: a demo asks for a rate
    // ONCE at startup, so anything that clears the request mid-run is
    // unrecoverable. This runs on every teardown — including the transient
    // ones — and a late frame arriving behind it reopens the session through
    // the friendly-implicit path, which restarts the frame timer at whatever
    // REQ_FPS then says. Cleared here, that is the DEFAULT, and a demo tuned
    // for 30 runs at 60: twice too fast, with its sound triggers piling up
    // (the exact symptom galaxigans' own comment warns about).
    //
    // Resetting at launch keeps the guarantee that motivated the clear —
    // galaxigans' 640x360 must not leak into the next Breakout — because a
    // launch is where the next demo actually begins, and its own requests land
    // immediately after.
    // Retract the framebuffer BEFORE the buffers are dropped below: any Alien
    // the demo still holds must read as "no screen" rather than as memory that
    // is about to be freed.
    macvm::embed::clear_screen_memory();
    macvm::embed::clear_text_memory();
    macvm::embed::clear_palette_memory();
    // Forget where the mouse was; the next demo's first frame should not see a
    // pointer position left behind by this one.
    game_input::clear_mouse();
    let closing = current_pane();
    GAMES.with(|cell| {
        if let Some(g) = closing.and_then(|id| cell.borrow_mut().remove(&id)) {
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
        if let Some(tp) = g.text_plane.as_mut() {
            tp.render(cb, tex);
        }
        g.text.render(cb, tex);
        cb.present_drawable(drawable);
        cb.commit();
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
    if let Some(tp) = g.text_plane.as_mut() {
        tp.render(cb, tex);
    }
    g.text.render(cb, tex);
    cb.present_drawable(drawable);
    cb.commit();
}

/// Tell the VM about the direct framebuffer's whole ROTATING SET, once.
///
/// Deliberately not "the current write buffer, republished after each
/// present": the VM cannot observe the rotation at the instant this thread
/// performs it. A demo sends `present` and starts the next frame immediately,
/// so it would fetch the pre-rotation pointer and write the very buffer about
/// to be displayed — which is precisely the tearing ParallelMandel showed.
///
/// Publishing the set lets both sides pick `frame % count` from their own
/// count of the same ordered events, and agree with no synchronisation.
fn publish_direct(g: &NativeGame) {
    if let Some(d) = g.direct.as_ref() {
        macvm::embed::publish_screen_buffers(
            &d.buffer_ptrs(),
            d.stride(),
            d.height() as usize,
        );
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
    let cmds: Vec<(PaneId, GameCommand)> = {
        let Ok(mut q) = GAME_CMDS.lock() else { return };
        if q.is_empty() {
            return;
        }
        q.drain(..).collect()
    };
    // ROUTING, one step at a time (`docs/multi_pane_design.md` §2a): every
    // command now says which pane it is for, and this drain still applies all
    // of them to the one window. That is deliberate — addressing lands first,
    // on its own, provably changing nothing — and `_pane` is where the lookup
    // goes when `NativeGame` becomes a map.
    for (_pane, cmd) in &cmds {
        match cmd {
            GameCommand::StartLoop => {
                SESSION_OPEN.store(true, Ordering::Release);
                ensure_pane_for(*_pane);
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
                let exists = current_pane().is_some();
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
                ensure_pane_for(*_pane);
                GAMES.with(|cell| {
                    if let Some(g) = cell.borrow_mut().get_mut(_pane) {
                        match DirectPane::new(&g.win.device, *w, *h) {
                            Ok(d) => {
                                macvm::embed::publish_screen_buffers(
                                    &d.buffer_ptrs(),
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
                let exists = current_pane().is_some();
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
                let running = with_current(|g| g.timer != objc::NIL).unwrap_or(false);
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
                    ensure_pane_for(*_pane);
                    start_frame_timer();
                }
                if SESSION_OPEN.load(Ordering::Acquire) {
                    ensure_pane_for(*_pane);
                }
                // THE ROUTING, live: a command is applied to the pane it was
                // addressed to, not to "the" pane. Two demos drawing at once
                // reach two different NativeGames from here.
                GAMES.with(|cell| {
                    if let Some(g) = cell.borrow_mut().get_mut(_pane) {
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
    // A fresh session must not inherit the last one's Escape.
    game_input::reset();
    // …nor its pane size, frame rate or overscan. This is the one boundary
    // where "the last demo's requests no longer apply" is true: the next
    // demo's own `resizeTo:`/`frameRate:`/`overscan:` land in the very next
    // batch, so clearing here cannot strand a demo at a default it did not
    // choose — which clearing at teardown demonstrably could.
    REQ_W.store(PANE_W as i64, Ordering::Release);
    REQ_H.store(PANE_H as i64, Ordering::Release);
    REQ_FPS.store(DEFAULT_FPS, Ordering::Release);
    REQ_OVERSCAN.store(0, Ordering::Release);
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
    // The demo READS this and ends itself (`GamePane exitRequested`,
    // DemoVmClient's tick) — a demo in a VM of its own is asked to quit, not
    // shot. The host-side close below still runs for the IN-PROCESS path,
    // whose demo has no VM of its own to do the asking to.
    game_input::request_exit();
    STOP_DUE.store(true, Ordering::Release);
    objc::wake_main_runloop();
}

/// Press the game window's red close button for real (scripted verification of
/// the close-button path): `performClose:` runs AppKit's full close sequence,
/// firing our `windowWillClose:` delegate exactly as a click would. Main-thread
/// only (the control drain calls this). Returns false if no window is open.
pub fn press_close_button() -> bool {
    GAMES.with(|cell| {
        let borrowed = cell.borrow();
        if let Some(g) = current_pane().and_then(|id| borrowed.get(&id)) {
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
    GAMES.with(|cell| {
        let borrowed = cell.borrow();
        let Some(g) = current_pane().and_then(|id| borrowed.get(&id)) else {
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
    GAMES.with(|cell| {
        let borrowed = cell.borrow();
        let g = current_pane().and_then(|id| borrowed.get(&id))?;
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
    GAMES.with(|cell| {
        let borrowed = cell.borrow();
        let Some(g) = current_pane().and_then(|id| borrowed.get(&id)) else {
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
        // BOTH DEMO HOMES, in one statement, because the host does not know
        // which one this demo used. `GamePane reset` clears an IN-PROCESS
        // demo's step block on the primary (as it always did); `DemoVmHost
        // stopAll` asks any demo running in a VM OF ITS OWN to end itself —
        // it stops its clock, resets its own pane and announces its exit
        // (docs/process_services.md S5/S6). With no demo VMs it is a no-op,
        // so the in-process path is unchanged.
        return Some("GamePane reset. DemoVmHost stopAll".to_string());
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
    // The IN-PROCESS demo path, unchanged: the host formats the step and the
    // supervisor execs it on the primary. A demo in a VM of its own never
    // comes through here — it reads the same snapshot itself, on its own
    // thread, from its own tick (`GamePane stepFromInput`).
    let s = game_input::snapshot();
    let (mask, mx, my, mb) = (s.keys, s.mouse_x, s.mouse_y, s.buttons);
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
    game_input::set_keys(mask);
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
    // The pointer is reported in the CURRENT pane's pixels — under the focus
    // rule that becomes "the window under the pointer", which is the same
    // answer while only one pane can be open.
    let (px, py) = match with_current(|g| (g.w, g.h)) {
        Some((w, h)) => (
            ((fx * w as f64) as i64).min(w as i64 - 1),
            ((fy * h as f64) as i64).min(h as i64 - 1),
        ),
        None => (-1, -1),
    };
    game_input::set_mouse(px, py, i64::from(left) | (i64::from(right) << 1));
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
