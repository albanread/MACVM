//! The Canvas view (`../docs/CANVAS.md`) — a Smalltalk-allocatable HTML5
//! drawing surface. Structurally the simplest of the three generated
//! views (compare `workspace_render.rs`/`browser_render.rs`): a single
//! `<canvas>` element plus two buttons, since all the real behavior lives
//! in `smtk.js`'s `macvmCanvasDraw` interpreter and `vm_host.rs`'s
//! `CanvasCommands` builder, not in the markup itself.
//!
//! No VM yet (`docs/CANVAS.md` §7), so **Run Demo** stands in for a real
//! `Canvas` send the same way Workspace's Do it/Print it stand in for
//! real evaluation — it proves the whole pipeline (`VmRequest::CanvasRunDemo`
//! → the mock world → `VmResponse::CanvasDraw` → `smtk.js`) actually
//! draws something, not that Smalltalk can drive it yet.

/// v1's fixed canvas id/size (`docs/CANVAS.md` §7: multiple canvases are
/// deferred) — matches `vm_host::CANVAS_ID` conceptually, but this module
/// only needs it for the DOM id string, not the numeric value itself.
pub const DEFAULT_WIDTH: u32 = 420;
pub const DEFAULT_HEIGHT: u32 = 220;

/// `width`/`height` become the `<canvas>` element's own `width`/`height`
/// attributes (the canvas's actual pixel buffer size, not CSS size — the
/// two are different for HTML canvases, and getting this one right on
/// first render matters: setting size via CSS alone would stretch/blur
/// whatever gets drawn instead of giving it a native-resolution surface).
pub fn render_canvas(width: u32, height: u32) -> String {
    // The Mandelbrot button carries its Smalltalk in `data-canvas-eval`, sized
    // to this canvas, and posts through the generic `canvasEval` path
    // (smtk.js/main.rs) — the GUI holds no Mandelbrot knowledge, and any other
    // drawing is just another button (or Workspace eval) with a different
    // expression. See `../docs/CANVAS.md`.
    let mandelbrot_code =
        format!("Mandelbrot new commandsForWidth: {width} height: {height}");
    format!(
        "<div class=\"st-canvas-view\" id=\"macvm-canvas-view\">\
         <div class=\"st-browser-action-row st-canvas-actions\">\
         <button type=\"button\" class=\"st-browser-new-button\" data-canvas-action=\"run-demo\">Run Demo</button>\
         <button type=\"button\" class=\"st-browser-new-button\" data-canvas-action=\"eval\" data-canvas-eval=\"{mandelbrot_code}\">Mandelbrot</button>\
         <button type=\"button\" class=\"st-browser-new-button\" data-canvas-action=\"clear\">Clear</button>\
         </div>\
         <div class=\"st-lowered st-canvas-surface\">\
         <canvas id=\"macvm-canvas-0\" width=\"{width}\" height=\"{height}\"></canvas>\
         </div>\
         </div>",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_canvas_includes_element_and_both_action_buttons() {
        let html = render_canvas(DEFAULT_WIDTH, DEFAULT_HEIGHT);
        assert!(html.contains("id=\"macvm-canvas-0\""), "{html}");
        assert!(html.contains("width=\"420\""), "{html}");
        assert!(html.contains("height=\"220\""), "{html}");
        assert!(html.contains("data-canvas-action=\"run-demo\""), "{html}");
        // The Mandelbrot demo routes through the GENERIC eval path, carrying
        // its Smalltalk (sized to this canvas) in a data attribute — no
        // Mandelbrot-specific canvas action exists.
        assert!(html.contains("data-canvas-action=\"eval\""), "{html}");
        assert!(
            html.contains("data-canvas-eval=\"Mandelbrot new commandsForWidth: 420 height: 220\""),
            "{html}"
        );
        assert!(html.contains("data-canvas-action=\"clear\""), "{html}");
    }

    #[test]
    fn render_canvas_uses_the_requested_size_not_just_the_default() {
        let html = render_canvas(800, 600);
        assert!(html.contains("width=\"800\""), "{html}");
        assert!(html.contains("height=\"600\""), "{html}");
    }
}
