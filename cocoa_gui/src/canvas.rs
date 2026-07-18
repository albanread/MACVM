//! The Canvas view's native rendering backend (docs/CANVAS.md, the Cocoa
//! counterpart). Two independent paths into the SAME NSImageView — a view
//! Smalltalk builds and holds itself (`world/7?_cocoacanvas.mst`'s `CocoaCanvas`
//! class var), exactly like every other Cocoa tab's AppKit views; this module
//! holds NO persistent view state of its own, only pure rendering functions:
//!
//! - **Pixels** (`show_pixels_base64`): the RGBA `Pixmap` path
//!   (world/36_pixmap.mst) — Smalltalk base64-encodes the raw bytes (the same
//!   reason the web GUI's own pixel path does: the bridge's `@`-arg marshaller
//!   only carries Strings/ObjcRefs across an ordinary send, no ByteArray->NSData
//!   case — src/runtime/objc_bridge.rs `ObjcArg::Id`), this module decodes and
//!   blits them into an `NSBitmapImageRep`.
//! - **Commands** (`show_commands`): the vector command-batch path
//!   (docs/CANVAS.md §5.2's `[["op", ...args], ...]` wire format) — the SAME
//!   format `BenchmarkDashboard>>chartForWidth:height:` already emits for the
//!   web GUI's JS interpreter, reused here VERBATIM (zero Smalltalk changes)
//!   via a small hand-rolled parser (no JSON crate in this workspace) and
//!   native AppKit drawing (NSBezierPath/NSColor/NSString) instead of
//!   CanvasRenderingContext2D — a `lockFocus`/`unlockFocus`-drawn NSImage.
//!
//! Both paths answer an `NSImage` `Id`, set on the caller-supplied
//! `NSImageView` via `setImage:` — a plain, synchronous host call. Callers
//! MUST run this top-level, never from inside a C6 callback (the same
//! flag-and-drain rule view_refresh.rs documents): AppKit drawing here can run
//! arbitrarily long for a large command batch, and nesting a second AppKit
//! event loop turn inside a callback is the reentrancy hazard this session's
//! browser crash fix (view_refresh.rs) exists to avoid.

use crate::objc::{self, Id};

// ── base64 (decode only — Smalltalk's Base64 class, world/58a, encodes) ─────

/// Standard RFC 4648 decode. Unknown bytes (whitespace, anything outside the
/// alphabet) are skipped rather than erroring — defensive, not RFC-strict;
/// the only producer is our own `Base64 class>>encode:`, always well-formed.
fn base64_decode(s: &str) -> Vec<u8> {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut table = [255u8; 256];
    for (i, &c) in ALPHA.iter().enumerate() {
        table[c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 3);
    let mut buf = [0u8; 4];
    let mut n = 0usize;
    for &b in s.as_bytes() {
        if b == b'=' {
            continue;
        }
        let v = table[b as usize];
        if v == 255 {
            continue; // whitespace/newline — skip
        }
        buf[n] = v;
        n += 1;
        if n == 4 {
            out.push((buf[0] << 2) | (buf[1] >> 4));
            out.push((buf[1] << 4) | (buf[2] >> 2));
            out.push((buf[2] << 6) | buf[3]);
            n = 0;
        }
    }
    if n == 2 {
        out.push((buf[0] << 2) | (buf[1] >> 4));
    } else if n == 3 {
        out.push((buf[0] << 2) | (buf[1] >> 4));
        out.push((buf[1] << 4) | (buf[2] >> 2));
    }
    out
}

// ── the pixel path: raw RGBA bytes -> NSBitmapImageRep -> NSImage ───────────

/// `NSBitmapImageRep`'s long initializer — planes:NULL so Cocoa allocates the
/// buffer itself; bytesPerRow:0/bitsPerPixel:0 so it computes sane defaults
/// for 8-bit-per-sample, 4-sample (RGBA), non-planar. A one-off custom-arity
/// transmute (mirrors objc.rs's own `send_attr` for the same reason: this
/// exact 10-argument shape has no other caller).
fn new_bitmap_rgba(width: u32, height: u32) -> Id {
    type InitPlanes = extern "C" fn(
        Id,   // self
        objc::Sel,
        Id,   // planes (NULL)
        i64,  // pixelsWide
        i64,  // pixelsHigh
        i64,  // bitsPerSample
        i64,  // samplesPerPixel
        bool, // hasAlpha
        bool, // isPlanar
        Id,   // colorSpaceName (an NSString)
        i64,  // bytesPerRow
        i64,  // bitsPerPixel
    ) -> Id;
    let cls = objc::get_class("NSBitmapImageRep");
    let alloc = objc::send0(cls as Id, objc::sel("alloc"));
    let sel = objc::sel(
        "initWithBitmapDataPlanes:pixelsWide:pixelsHigh:bitsPerSample:samplesPerPixel:\
         hasAlpha:isPlanar:colorSpaceName:bytesPerRow:bitsPerPixel:",
    );
    let f: InitPlanes = unsafe { std::mem::transmute(objc::msg_send_ptr()) };
    let device_rgb = objc::nsstring("NSDeviceRGBColorSpace");
    f(
        alloc,
        sel,
        objc::NIL,
        width as i64,
        height as i64,
        8,
        4,
        true,
        false,
        device_rgb,
        0,
        0,
    )
}

/// Build an `NSImage` from raw row-major RGBA bytes (the `Pixmap`/`ImageData`
/// layout) by memcpy-ing straight into a fresh `NSBitmapImageRep`'s own
/// buffer, then wrapping it. `pixels.len()` is clamped to the rep's actual
/// buffer size (`bytesPerRow * height`) — a short/long buffer from a
/// mismatched width/height never overruns.
fn image_from_rgba(pixels: &[u8], width: u32, height: u32) -> Option<Id> {
    if width == 0 || height == 0 {
        return None;
    }
    let rep = new_bitmap_rgba(width, height);
    if rep.is_null() {
        return None;
    }
    let data_ptr = objc::send0(rep, objc::sel("bitmapData")) as *mut u8;
    if data_ptr.is_null() {
        return None;
    }
    // Copy ROW BY ROW: `pixels` is packed with NO padding (source stride =
    // width*4), but NSBitmapImageRep's own `bytesPerRow` (we passed 0, letting
    // Cocoa choose) may pad each row to an alignment boundary — a single
    // contiguous memcpy assuming the two strides match produces exactly the
    // "sheared and mixed up" artifact seen on first light: each row lands
    // further from where it should as the row-index-dependent offset drifts.
    let dst_stride = objc::send0_int(rep, objc::sel("bytesPerRow")).max(0) as usize;
    let src_stride = (width as usize).saturating_mul(4);
    let row_bytes = src_stride.min(dst_stride);
    for row in 0..height as usize {
        let src_off = row * src_stride;
        let dst_off = row * dst_stride;
        if src_off + row_bytes > pixels.len() {
            break;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                pixels.as_ptr().add(src_off),
                data_ptr.add(dst_off),
                row_bytes,
            );
        }
    }

    let img_cls = objc::get_class("NSImage");
    let img = objc::send0(img_cls as Id, objc::sel("alloc"));
    let img = objc::send0(img, objc::sel("init"));
    objc::send1_id(img, objc::sel("addRepresentation:"), rep);
    Some(img)
}

/// Decode base64 RGBA bytes and set them on `view` (an `NSImageView` Id the
/// Smalltalk side owns and passed in). Top-level caller only (see module doc).
pub fn show_pixels_base64(view: Id, b64: &str, width: u32, height: u32) -> bool {
    if view.is_null() {
        return false;
    }
    let bytes = base64_decode(b64);
    match image_from_rgba(&bytes, width, height) {
        Some(img) => {
            objc::send1_id(view, objc::sel("setImage:"), img);
            true
        }
        None => false,
    }
}

// ── the vector-command path: docs/CANVAS.md §5.2's JSON-ish batch ───────────

enum Arg {
    Str(String),
    Num(f64),
}

struct Op {
    name: String,
    args: Vec<Arg>,
}

/// A minimal hand-rolled parser for EXACTLY `[["op", arg, ...], ...]` where
/// each arg is a double-quoted string (no escapes — chart text is plain
/// ASCII) or a bare integer (`BenchmarkDashboard>>num:` always emits
/// `truncated`, so no floats ever appear on the wire) — not general JSON.
/// No dependency: this workspace has no JSON crate, and the wire vocabulary
/// (docs/CANVAS.md §5.2) is narrow enough that hand-rolling is simpler and
/// safer than half-using a general parser for a shape it doesn't need.
fn parse_ops(s: &str) -> Vec<Op> {
    let b = s.as_bytes();
    let mut i = 0usize;
    let mut ops = Vec::new();
    let skip_ws = |b: &[u8], i: &mut usize| {
        while *i < b.len() && (b[*i] == b' ' || b[*i] == b'\n' || b[*i] == b'\r' || b[*i] == b'\t')
        {
            *i += 1;
        }
    };
    let read_string = |b: &[u8], i: &mut usize| -> Option<String> {
        if *i >= b.len() || b[*i] != b'"' {
            return None;
        }
        *i += 1;
        let start = *i;
        while *i < b.len() && b[*i] != b'"' {
            *i += 1;
        }
        let text = String::from_utf8_lossy(&b[start..*i]).into_owned();
        if *i < b.len() {
            *i += 1; // closing quote
        }
        Some(text)
    };
    let read_num = |b: &[u8], i: &mut usize| -> Option<f64> {
        let start = *i;
        if *i < b.len() && (b[*i] == b'-' || b[*i] == b'+') {
            *i += 1;
        }
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        if *i == start {
            return None;
        }
        std::str::from_utf8(&b[start..*i]).ok()?.parse().ok()
    };

    skip_ws(b, &mut i);
    if i < b.len() && b[i] == b'[' {
        i += 1;
    }
    loop {
        skip_ws(b, &mut i);
        if i >= b.len() || b[i] == b']' {
            break;
        }
        if b[i] == b',' {
            i += 1;
            continue;
        }
        if b[i] != b'[' {
            break; // malformed — stop rather than loop forever
        }
        i += 1;
        skip_ws(b, &mut i);
        let Some(name) = read_string(b, &mut i) else { break };
        let mut args = Vec::new();
        loop {
            skip_ws(b, &mut i);
            if i >= b.len() || b[i] == b']' {
                break;
            }
            if b[i] == b',' {
                i += 1;
                skip_ws(b, &mut i);
            }
            if i < b.len() && b[i] == b'"' {
                if let Some(s) = read_string(b, &mut i) {
                    args.push(Arg::Str(s));
                }
            } else if let Some(n) = read_num(b, &mut i) {
                args.push(Arg::Num(n));
            } else {
                break;
            }
        }
        skip_ws(b, &mut i);
        if i < b.len() && b[i] == b']' {
            i += 1;
        }
        ops.push(Op { name, args });
    }
    ops
}

/// Parse a CSS-ish `rgb(r,g,b)` string (the only colour shape
/// `BenchmarkDashboard`'s chart ever emits) to `(r,g,b)` in 0..255. Falls back
/// to mid-grey for anything else, rather than failing the whole batch over one
/// unparseable colour.
fn parse_rgb(s: &str) -> (f64, f64, f64) {
    let inner = s
        .strip_prefix("rgb(")
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or("");
    let mut parts = inner.split(',').map(|p| p.trim().parse::<f64>().unwrap_or(150.0));
    let r = parts.next().unwrap_or(150.0);
    let g = parts.next().unwrap_or(150.0);
    let bl = parts.next().unwrap_or(150.0);
    (r / 255.0, g / 255.0, bl / 255.0)
}

fn nscolor_srgb(r: f64, g: f64, b: f64) -> Id {
    let cls = objc::get_class("NSColor");
    objc::send4_f64(
        cls as Id,
        objc::sel("colorWithSRGBRed:green:blue:alpha:"),
        r,
        g,
        b,
        1.0,
    )
}

/// Render a docs/CANVAS.md §5.2 command batch into a fresh `NSImage` via
/// `lockFocus`/`unlockFocus`. AppKit's un-flipped coordinate space is
/// bottom-up (origin bottom-left); the chart's own geometry is Canvas2D-style
/// top-down, so every y is flipped here (`height - y [- h]`) rather than
/// touching the shared Smalltalk layout code.
///
/// v1 scope (docs/CANVAS.md's own "useful, bounded subset" posture):
/// `clearRect`/`fillStyle`/`fillRect`/`fillText` — exactly what
/// `BenchmarkDashboard>>chartForWidth:height:` emits. `font`/`textAlign` are
/// accepted and ignored (text draws in the system default font, left-aligned
/// — matches the chart's own only-ever-'left' usage); text colour is
/// AppKit's default (`drawAtPoint:withAttributes:nil`) rather than tracking
/// fillStyle for text specifically, since bars (the dominant visual weight)
/// DO get their exact colour. An unrecognized op is a logged no-op, never a
/// failed batch — the same discipline smtk.js's own interpreter uses.
pub fn render_commands(ops_json: &str, width: u32, height: u32) -> Option<Id> {
    if width == 0 || height == 0 {
        return None;
    }
    let ops = parse_ops(ops_json);

    let img_cls = objc::get_class("NSImage");
    let img = objc::send0(img_cls as Id, objc::sel("alloc"));
    // initWithSize: takes ONE NSSize (a 2-double HFA => d0,d1) — the exact
    // shape send2_f64 already implements.
    let img = objc::send2_f64(img, objc::sel("initWithSize:"), width as f64, height as f64);
    if img.is_null() {
        return None;
    }
    objc::send0(img, objc::sel("lockFocus"));

    let flip_rect_y = |y: f64, h: f64| (height as f64) - y - h;
    let flip_point_y = |y: f64| (height as f64) - y;

    for op in &ops {
        match op.name.as_str() {
            "clearRect" => {
                if let [Arg::Num(x), Arg::Num(y), Arg::Num(w), Arg::Num(h)] = op.args.as_slice() {
                    nscolor_white().let_setfill();
                    let path = bezier_rect(*x, flip_rect_y(*y, *h), *w, *h);
                    objc::send0(path, objc::sel("fill"));
                }
            }
            "fillStyle" => {
                if let [Arg::Str(c)] = op.args.as_slice() {
                    let (r, g, b) = parse_rgb(c);
                    let colour = nscolor_srgb(r, g, b);
                    objc::send0(colour, objc::sel("setFill"));
                }
            }
            "fillRect" => {
                if let [Arg::Num(x), Arg::Num(y), Arg::Num(w), Arg::Num(h)] = op.args.as_slice() {
                    let path = bezier_rect(*x, flip_rect_y(*y, *h), *w, *h);
                    objc::send0(path, objc::sel("fill"));
                }
            }
            "fillText" => {
                if let [Arg::Str(text), Arg::Num(x), Arg::Num(y)] = op.args.as_slice() {
                    let ns = objc::nsstring(text);
                    let pt_y = flip_point_y(*y);
                    objc::send2_f64_id(ns, objc::sel("drawAtPoint:withAttributes:"), *x, pt_y, objc::NIL);
                }
            }
            "font" | "textAlign" => {} // v1: accepted, no-op (see doc comment)
            other => {
                eprintln!("macvm-cocoa: canvas — unknown op {other:?}");
            }
        }
    }

    objc::send0(img, objc::sel("unlockFocus"));
    Some(img)
}

fn bezier_rect(x: f64, y: f64, w: f64, h: f64) -> Id {
    let cls = objc::get_class("NSBezierPath");
    objc::send4_f64(cls as Id, objc::sel("bezierPathWithRect:"), x, y, w, h)
}

// A tiny extension trait so `nscolor_white().let_setfill()` reads as one
// expression above — `setFill` is `send0`, this just names the pattern.
trait SetFillExt {
    fn let_setfill(self);
}
impl SetFillExt for Id {
    fn let_setfill(self) {
        objc::send0(self, objc::sel("setFill"));
    }
}
fn nscolor_white() -> Id {
    let cls = objc::get_class("NSColor");
    objc::send0(cls as Id, objc::sel("whiteColor"))
}

/// Interpret a command batch and set the result on `view`. Top-level caller
/// only (see module doc). `false` if the batch produced nothing displayable.
pub fn show_commands(view: Id, ops_json: &str, width: u32, height: u32) -> bool {
    if view.is_null() {
        return false;
    }
    match render_commands(ops_json, width, height) {
        Some(img) => {
            objc::send1_id(view, objc::sel("setImage:"), img);
            true
        }
        None => false,
    }
}
