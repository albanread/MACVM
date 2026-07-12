//! The native Metal game pane (`docs/gamepane_design.md`).
//!
//! The GUI crate owns AppKit, so the MacGamePane engine is built in here. This
//! module currently provides the **render side** (milestone M2): constructing
//! the panes and drawing a frame through the GPU pipeline. The on-screen
//! `NSView` + `CAMetalLayer` embedding (M2b), the VM->gui command channel (M3),
//! and the frame loop (M4) land in later milestones.
//!
//! The render logic is deliberately factored so the *same* scene can be drawn
//! into an offscreen texture (unit-tested, read back pixel-exact) and, later,
//! into a live `CAMetalLayer` drawable — the on-screen path adds only the
//! present, never a second copy of the drawing.
#![allow(dead_code)] // on-screen embedding (M2b) consumes the render helpers below

use macgamepane_graphics::indexed_pane::IndexedPane;

/// Palette indices for the M2 test scene. Index 0 is transparent; `set_rgb`
/// asserts index >= 16, so user colours start at 16.
pub mod pal {
    pub const BG: u8 = 16;
    pub const WHITE: u8 = 17;
    pub const RED: u8 = 18;
    pub const GREEN: u8 = 19;
    pub const CYAN: u8 = 20;
}

/// Load the M2 test palette into `pane`.
pub fn load_test_palette(pane: &mut IndexedPane) {
    pane.set_rgb(pal::BG, 20, 24, 48); // dark blue field
    pane.set_rgb(pal::WHITE, 240, 240, 240);
    pane.set_rgb(pal::RED, 220, 60, 60);
    pane.set_rgb(pal::GREEN, 70, 200, 90);
    pane.set_rgb(pal::CYAN, 80, 200, 220);
}

/// Draw a recognizable static scene into `pane`'s active buffer: a bordered
/// dark field crossed by a cyan X, with a red block and a green disc — chosen
/// so the rendered frame is unmistakable when the pixels are inspected.
pub fn draw_test_scene(pane: &mut IndexedPane, w: i64, h: i64) {
    pane.cls(pal::BG);
    // White border.
    pane.line(0, 0, w - 1, 0, pal::WHITE);
    pane.line(0, h - 1, w - 1, h - 1, pal::WHITE);
    pane.line(0, 0, 0, h - 1, pal::WHITE);
    pane.line(w - 1, 0, w - 1, h - 1, pal::WHITE);
    // Cyan X across the field.
    pane.line(0, 0, w - 1, h - 1, pal::CYAN);
    pane.line(0, h - 1, w - 1, 0, pal::CYAN);
    // A red block, centred.
    pane.fill_rect(w / 2 - 40, h / 2 - 30, 80, 60, pal::RED);
    // A green disc, upper-left quadrant.
    pane.disc(w / 4, h / 4, 24, pal::GREEN);
}

/// Build a pane, draw the test scene, and render it into an offscreen
/// `BGRA8Unorm` texture; returns the read-back pixel buffer (top row first,
/// 4 bytes/pixel, B,G,R,A). `None` if the machine has no Metal device.
pub fn render_test_scene_offscreen(w: u32, h: u32) -> Option<Vec<u8>> {
    let device = metal::Device::system_default()?;

    let mut pane = IndexedPane::new(&device, w, h, w, h).expect("IndexedPane::new");
    load_test_palette(&mut pane);
    draw_test_scene(&mut pane, w as i64, h as i64);
    pane.upload();

    let tex_desc = metal::TextureDescriptor::new();
    tex_desc.set_texture_type(metal::MTLTextureType::D2);
    tex_desc.set_pixel_format(metal::MTLPixelFormat::BGRA8Unorm);
    tex_desc.set_width(w as u64);
    tex_desc.set_height(h as u64);
    tex_desc.set_usage(metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead);
    tex_desc.set_storage_mode(metal::MTLStorageMode::Shared);
    let target = device.new_texture(&tex_desc);

    let queue = device.new_command_queue();
    let cb = queue.new_command_buffer();
    pane.render(cb, &target, metal::MTLLoadAction::Clear);
    cb.commit();
    cb.wait_until_completed();
    assert_eq!(cb.status(), metal::MTLCommandBufferStatus::Completed);

    let mut buf = vec![0u8; (w * h * 4) as usize];
    target.get_bytes(
        buf.as_mut_ptr() as *mut std::ffi::c_void,
        (w * 4) as u64,
        metal::MTLRegion {
            origin: metal::MTLOrigin { x: 0, y: 0, z: 0 },
            size: metal::MTLSize {
                width: w as u64,
                height: h as u64,
                depth: 1,
            },
        },
        0,
    );
    Some(buf)
}

// ── On-screen native pane (M2b) ────────────────────────────────────────────
//
// A layer-hosting `NSView` backed by a `CAMetalLayer`, rendered into on the
// **main thread** — the design's rule that all Metal + all panes are
// main-thread single-owned. The view is installed as the window's content view
// (swapping the WKWebView out) and restored when navigating away. Mirrors
// MacGamePane's own `GameWindow` layer wiring, minus the window (MACVM already
// has one). Input, the VM->gui command channel, and the frame loop are later
// milestones; this renders one static frame.

use crate::objc;
use metal::foreign_types::ForeignType;

/// The live native pane: its GPU objects, its `CAMetalLayer`-hosting `NSView`,
/// and the CPU-side `IndexedPane`. All fields are touched only on the main
/// thread (see the module doc), so it lives in a main-thread `thread_local`.
struct NativePane {
    view: objc::Id,
    device: metal::Device,
    queue: metal::CommandQueue,
    layer: metal::MetalLayer,
    pane: IndexedPane,
    /// The GPU sprite layer, composited over the indexed background each frame.
    sprites: macgamepane_graphics::sprites::Sprites,
    /// VM-minted sprite id -> (MacGamePane def id, instance id). Monotonic keys
    /// (Smalltalk-side counter), so a stale key from a previous VM generation
    /// simply misses this map rather than aliasing a live sprite
    /// (docs/gamepane_design.md — a HashMap miss is safe; array-index reuse was
    /// the S15 #93 hazard). Registry teardown on VM restart is a follow-up.
    sprite_ids: std::collections::HashMap<i64, (usize, usize)>,
    w: u32,
    h: u32,
}

thread_local! {
    static NATIVE: std::cell::RefCell<Option<NativePane>> =
        const { std::cell::RefCell::new(None) };
}

/// Build the native pane once (main thread) and return its `NSView` to install
/// as the window content view. `None` if this Mac has no Metal device.
pub fn ensure_native_view(w: u32, h: u32) -> Option<objc::Id> {
    NATIVE.with(|cell| {
        {
            let mut slot = cell.borrow_mut();
            if slot.is_none() {
                let device = metal::Device::system_default()?;
                let queue = device.new_command_queue();

                // A key-capable view (MacGamePane's MacGamePaneKeyView) so
                // keyDown:/keyUp: populate HELD_KEYS while the game is focused
                // (docs/gamepane_design.md M4). Register the class first.
                macgamepane_graphics::input::key_capable_view_class();
                let view = objc::send_frame_init(
                    objc::send0(objc::get_class("MacGamePaneKeyView"), objc::sel("alloc")),
                    objc::sel("initWithFrame:"),
                    0.0,
                    0.0,
                    w as f64,
                    h as f64,
                );
                // Layer-hosting: set our CAMetalLayer, then wantsLayer. The
                // fixed-size drawable is upscaled by CA to fill the window.
                let layer = metal::MetalLayer::new();
                layer.set_device(&device);
                layer.set_pixel_format(metal::MTLPixelFormat::BGRA8Unorm);
                layer.set_drawable_size(core_graphics_types::geometry::CGSize::new(
                    w as f64, h as f64,
                ));
                objc::send1_id(view, objc::sel("setLayer:"), layer.as_ptr() as objc::Id);
                objc::send1_bool(view, objc::sel("setWantsLayer:"), true);
                objc::send1_i64(view, objc::sel("setAutoresizingMask:"), 18); // width|height sizable

                let mut pane = IndexedPane::new(&device, w, h, w, h).ok()?;
                load_test_palette(&mut pane);
                let sprites = macgamepane_graphics::sprites::Sprites::new(&device).ok()?;

                // `device` is moved in last; every borrow of it above is done.
                *slot = Some(NativePane {
                    view,
                    device,
                    queue,
                    layer,
                    pane,
                    sprites,
                    sprite_ids: std::collections::HashMap::new(),
                    w,
                    h,
                });
            }
        }
        cell.borrow().as_ref().map(|n| n.view)
    })
}

/// Upload the pane's CPU buffer and present it into the layer's next drawable
/// (main thread). A no-op if no drawable is ready (the compositor occasionally
/// has none — skip the frame, not an error).
fn present_native(n: &mut NativePane) {
    n.pane.upload();
    let Some(drawable) = n.layer.next_drawable() else {
        return;
    };
    let cb = n.queue.new_command_buffer();
    n.pane
        .render(cb, drawable.texture(), metal::MTLLoadAction::Clear);
    // Composite the sprite layer over the indexed background (its own render
    // loads rather than clears the target).
    n.sprites
        .render(cb, drawable.texture(), 0.0, 0.0, n.w as f64, n.h as f64);
    cb.present_drawable(drawable);
    cb.commit();
}

/// Draw the test scene into the pane and present (main thread). A no-op if the
/// native pane hasn't been built.
pub fn render_native_frame() {
    NATIVE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(n) = slot.as_mut() else { return };
        draw_test_scene(&mut n.pane, n.w as i64, n.h as i64);
        present_native(n);
    });
}

thread_local! {
    /// Set when a drawing command has mutated the pane since the last present,
    /// so a whole batch of draw commands (a frame) costs one present. See
    /// [`present_if_dirty`].
    static DIRTY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Apply a [`macvm::embed::GameCommand`] from the VM to the on-screen native
/// pane (main thread, `docs/gamepane_design.md`). Drawing commands mutate the
/// CPU buffer only and mark the pane dirty; `Present` shows the frame. A no-op
/// if the pane isn't currently built/shown — the command is dropped, matching
/// the headless-VM behaviour on the core side.
pub fn apply_command(cmd: &macvm::embed::GameCommand) {
    use macvm::embed::GameCommand as C;
    // Loop control drives the main-thread frame timer and doesn't touch the
    // pane, so handle it before the pane check (it works even if no pane view
    // is currently shown).
    match cmd {
        C::StartLoop => return crate::start_game_loop_timer(),
        C::StopLoop => return crate::stop_game_loop_timer(),
        C::PlaySound { preset } => return play_sound(*preset),
        C::PlayTune { abc } => return play_tune(abc),
        _ => {}
    }
    NATIVE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(n) = slot.as_mut() else { return };
        match cmd {
            C::PaletteAt { index, r, g, b } => n.pane.set_rgb(*index, *r, *g, *b),
            C::Cls { index } => n.pane.cls(*index),
            C::ClearTo { r, g, b } => {
                // Reuse palette index 16 as the fill colour, clear FRONT to it.
                n.pane.set_rgb(pal::BG, *r, *g, *b);
                n.pane.cls(pal::BG);
            }
            C::Pset { x, y, index } => n.pane.pset(*x, *y, *index),
            C::Line {
                x0,
                y0,
                x1,
                y1,
                index,
            } => n.pane.line(*x0, *y0, *x1, *y1, *index),
            C::FillRect { x, y, w, h, index } => n.pane.fill_rect(*x, *y, *w, *h, *index),
            C::Disc { cx, cy, r, index } => n.pane.disc(*cx, *cy, *r, *index),
            C::DefineSprite { id, rows } => {
                if let Some(def) = n.sprites.define_sprite(rows) {
                    let inst = n.sprites.place(def, 0.0, 0.0);
                    n.sprite_ids.insert(*id, (def, inst));
                }
            }
            C::SpriteColor { id, index, r, g, b } => {
                if let Some(&(def, _)) = n.sprite_ids.get(id) {
                    n.sprites.sprite_rgb(def, *index, *r, *g, *b);
                }
            }
            C::MoveSprite { id, x, y } => {
                if let Some(&(_, inst)) = n.sprite_ids.get(id) {
                    n.sprites.move_to(inst, *x as f64, *y as f64);
                }
            }
            C::Present => {
                present_native(n);
                DIRTY.with(|d| d.set(false));
                return;
            }
            C::StartLoop | C::StopLoop | C::PlaySound { .. } | C::PlayTune { .. } => {
                unreachable!("handled before the pane check")
            }
        }
        DIRTY.with(|d| d.set(true));
    });
}

// ── Audio (docs/gamepane_design.md) ─────────────────────────────────────────

thread_local! {
    /// The one shared SFX engine, created + started lazily on the main thread —
    /// exactly one per session, since starting two `AVAudioEngine`s
    /// concurrently aborts the process. Bit N of `DEFINED_SFX` marks preset N's
    /// slot as already synthesized + defined.
    static SFX: std::cell::RefCell<Option<macgamepane_audio::playback::Sfx>> =
        const { std::cell::RefCell::new(None) };
    static DEFINED_SFX: std::cell::Cell<u16> = const { std::cell::Cell::new(0) };
}

/// Play SFX preset `preset` (0..=9) on the one shared engine, creating and
/// starting it on first use. A no-op if no audio device is available.
pub fn play_sound(preset: u8) {
    SFX.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let mut sfx = macgamepane_audio::playback::Sfx::new();
            if !sfx.start() {
                return; // no audio device — drop the sound silently
            }
            *slot = Some(sfx);
        }
        let sfx = slot.as_mut().unwrap();
        // Synthesize + define each preset's slot exactly once.
        let bit = 1u16 << preset.min(15);
        if DEFINED_SFX.with(|d| d.get()) & bit == 0 {
            let sound = synth_preset(preset);
            sfx.define(preset as usize, &sound);
            DEFINED_SFX.with(|d| d.set(d.get() | bit));
        }
        sfx.play(preset as usize);
    });
}

/// Play an ABC-notation tune once in the background (the engine's ABC->MIDI
/// path spawns its own player thread; non-blocking). A no-op if the ABC doesn't
/// parse to any notes.
pub fn play_tune(abc: &str) {
    if let Some(tune) = macgamepane_audio::abc::parse_tune(abc) {
        let _ = macgamepane_audio::playback::play_tune_non_blocking(&tune);
    }
}

/// Synthesize a named preset `Sound` (deterministic; some presets take an RNG).
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

/// Present the pane if any drawing has happened since the last present (main
/// thread). Called once after each response-drain batch, so a frame's worth of
/// draw commands with no explicit `present` still shows exactly once.
pub fn present_if_dirty() {
    NATIVE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(n) = slot.as_mut() else { return };
        if DIRTY.with(|d| d.replace(false)) {
            present_native(n);
        }
    });
}

/// The pane's logical resolution (fixed for now; the drawable is upscaled to
/// fill the window — see the design's "Resize" note).
pub const PANE_W: u32 = 320;
pub const PANE_H: u32 = 240;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write a top-row-first BGRA buffer as a 32-bit BMP (bottom-up rows,
    /// native BGRA byte order — no conversion needed). Enough to eyeball the
    /// render; `sips` converts it to PNG for viewing.
    fn write_bmp32(path: &std::path::Path, w: u32, h: u32, bgra_top_first: &[u8]) {
        let row_bytes = (w * 4) as usize;
        let pixel_data = (w * h * 4) as u32;
        let file_size = 54 + pixel_data;
        let mut f = std::fs::File::create(path).unwrap();
        // BITMAPFILEHEADER (14 bytes).
        f.write_all(b"BM").unwrap();
        f.write_all(&file_size.to_le_bytes()).unwrap();
        f.write_all(&0u32.to_le_bytes()).unwrap(); // reserved
        f.write_all(&54u32.to_le_bytes()).unwrap(); // pixel-data offset
        // BITMAPINFOHEADER (40 bytes).
        f.write_all(&40u32.to_le_bytes()).unwrap();
        f.write_all(&(w as i32).to_le_bytes()).unwrap();
        f.write_all(&(h as i32).to_le_bytes()).unwrap(); // positive => bottom-up
        f.write_all(&1u16.to_le_bytes()).unwrap(); // planes
        f.write_all(&32u16.to_le_bytes()).unwrap(); // bpp
        f.write_all(&0u32.to_le_bytes()).unwrap(); // BI_RGB
        f.write_all(&pixel_data.to_le_bytes()).unwrap();
        f.write_all(&2835i32.to_le_bytes()).unwrap(); // x ppm
        f.write_all(&2835i32.to_le_bytes()).unwrap(); // y ppm
        f.write_all(&0u32.to_le_bytes()).unwrap(); // palette colours
        f.write_all(&0u32.to_le_bytes()).unwrap(); // important colours
        // Pixel rows, bottom-up.
        for y in (0..h as usize).rev() {
            let start = y * row_bytes;
            f.write_all(&bgra_top_first[start..start + row_bytes]).unwrap();
        }
    }

    #[test]
    fn renders_the_test_scene_and_dumps_a_bmp() {
        let (w, h) = (320u32, 240u32);
        let Some(buf) = render_test_scene_offscreen(w, h) else {
            eprintln!("no Metal device; skipping (render path proven by linking)");
            return;
        };

        // Pixel checks: the field centre is inside the red block; a corner is
        // on the white border. BGRA byte order.
        let at = |x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            (buf[i + 2], buf[i + 1], buf[i]) // (R, G, B)
        };
        assert_eq!(at(w / 2, h / 2), (220, 60, 60), "centre is the red block");
        // Top-edge midpoint is white border and off both diagonals.
        assert_eq!(at(w / 2, 0), (240, 240, 240), "top border is white");
        // The corner sits on both the border and the cyan X; the X is drawn
        // last, so it wins — confirms draw order composites as written.
        assert_eq!(at(0, 0), (80, 200, 220), "corner is the cyan diagonal");

        let out = std::env::temp_dir().join("macvm_gamepane_m2.bmp");
        write_bmp32(&out, w, h, &buf);
        eprintln!("M2 scene written to {}", out.display());
    }
}
