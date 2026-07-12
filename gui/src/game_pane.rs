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
