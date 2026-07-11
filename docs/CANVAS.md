# MACVM Canvas Widget — a Smalltalk-Drawable HTML5 Canvas

Design for a Smalltalk-allocatable drawing surface: Smalltalk code (once
the compiler/VM side is ready) allocates a canvas of a given size and
sends it batches of drawing commands, rendered via the browser's own
Canvas 2D API. Same posture as `docs/FFI.md`/`docs/ASM.md`: designed in
full, but only the GUI-side half is built now — the VM-side `Canvas` class
and its `<primitive:>` bodies simply haven't been built yet (the GUI-side
rendering half, committed 3daaf72, is the only implemented portion), so
this stays a side track that touches neither `src/compiler` nor
`src/interpreter`.

## 0. Scope and non-goals

- **Designed now**: the whole feature — Smalltalk-facing API sketch, the
  wire protocol between VM and GUI, and exactly why it's shaped the way
  it is.
- **Built now**: the GUI-side half — a Canvas view (§4), the VM-request
  plumbing to create a canvas and push a batch of draw commands (§5), a
  JS-side interpreter executing that batch against a real
  `CanvasRenderingContext2D` (§6), and a "Run Demo" mechanism that proves
  the whole pipeline end-to-end today, using the mock world in place of a
  real VM.
- **Not built now**: any VM-side primitive letting real Smalltalk code
  reach this. A future `Canvas` class's methods (`moveTo:`/`lineTo:`/
  `fillRect:width:height:`/...) would need `<primitive: N>` pragmas with
  no primitive behind them yet, exactly matching `docs/ASM.md`/
  `docs/FFI.md`'s own "forward-declared, not yet callable" contract.
- **Related but distinct from `smappl`** (`gui/smappl.md`): smappl
  (`<smappl visual="...">`) evaluates Smalltalk once, at page-render
  time, embedding a static result. This is the opposite shape — a
  long-lived widget that keeps receiving new drawing commands over time
  (an animation, a live diagram, incremental redraws) — so it's a new
  mechanism, not a smappl variant, even though both are ultimately
  "Smalltalk controls something embedded in the page."

## 1. Ground truth: the existing VM↔GUI bridge

Three facts about the current bridge (`gui/src/vm_host.rs`,
`gui/src/main.rs`) that this design leans on directly, confirmed by
reading the real source rather than assumed:

**Rust→JS is `evaluateJavaScript:completionHandler:` with a `nil`
handler** (`main.rs`'s `eval_js`) — fire-and-forget, no return-value path.
Every existing page update (`replace_pane`, `append_transcript`,
`WorkspacePrintResult`) already works this way.

**Responses already batch before waking the main thread.** The worker
loop processes one `VmRequest` to a whole `Vec<VmResponse>`, sends every
element, *then* wakes the main thread exactly once
(`performSelectorOnMainThread:...waitUntilDone:false`). A request that
internally produces many drawing operations can already return many
`VmResponse` values (or, as designed here, one response whose own payload
is a whole batch) and still cost only one wake — this is the entire
mechanism behind "fast channel," not something new to build.

**`LatestSlot<T>` exists, documented for exactly this kind of feature, but
deliberately NOT used here** — see §5.3 for why.

## 2. Precedent check

Checked Strongtalk's real `Canvas.str` directly (a `Protocol`, not a
class): `bitBlt:extent:op:`, `palette: <Win32Handle>`, raster operations
tied to Win32 GDI bitmap blitting. Not transferable — this is Win32-era
bitmap manipulation, a different paradigm from the vector/path-oriented
Web Canvas 2D API the user asked for, and squarely `docs/WORLD.md` Layer C
("UI, replaced by WKWebView"). This is a greenfield design, not a port,
same situation `docs/ASM.md` was in.

## 3. The Smalltalk-facing API (designed, not built)

Once a VM-side primitive exists, the intended shape:

```smalltalk
| c |
c := Canvas extent: 400 @ 300.
c fillStyle: 'steelblue'.
c fillRect: 10 y: 10 width: 100 height: 60.
c beginPath; moveTo: 20 y: 200; lineTo: 380 y: 200; stroke.
c displayCommands.
```

`Canvas` accumulates a batch internally (mirroring `WriteStream`'s own
accumulate-then-produce shape, `world/18_writestream.mst`) rather than
sending one VM request per drawing call — `displayCommands` (or an
implicit flush at the next safepoint) is what actually crosses into a
`VmResponse::CanvasDraw` batch. This is *why* the channel is fast: the
batching happens on the Smalltalk side, before anything crosses into
Rust at all, not as a channel-level optimization bolted on afterward.

## 4. The Canvas view (built now)

A new generated view, `gui/src/canvas_render.rs`, following the exact
convention `workspace_render.rs` already established (a `render_*`
function returning a `<div>` fragment, wrapped by
`preprocess::render_generated_page`, opened via a toolbar button and a
`view_marker()`/`open_*()`/`display_*()` trio in `main.rs`). Reuses the
existing `.st-lowered` themed container class for the canvas's own
surface rather than adding new per-theme CSS — a bare `<canvas>` element
has no default visual styling to override, so no theme file needed
touching for this.

Two buttons for now: **Run Demo** (exercises the full pipeline via the
mock world, §5.2) and **Clear**.

## 5. The VM request/response plumbing (built now)

### 5.1 New types (`gui/src/vm_host.rs`)

```rust
pub enum VmRequest {
    // ...existing...
    CanvasCreate { width: u32, height: u32 },
    CanvasRunDemo,
    CanvasClear,
}

pub enum VmResponse {
    // ...existing...
    CanvasCreated { id: u32, width: u32, height: u32 },
    CanvasDraw { id: u32, commands_json: String },
    CanvasCleared { id: u32 },
}
```

`commands_json` is a plain `String`, not a structured Rust command enum
serialized on the fly — matching every other field already crossing this
channel (`Doit { code: String }`, `BrowserSaveSource { text: String, ...
}`). This is deliberate: the real Smalltalk side would build this string
itself (via something `WriteStream`-shaped), so the channel's job is
purely to move an opaque string, exactly like it already does for
`Doit`'s code. Rust never needs to understand canvas-command structure —
only the JS interpreter (§6) does.

### 5.2 The wire format: a JSON array of `[opName, ...args]`

```json
[["fillStyle", "steelblue"], ["fillRect", 10, 10, 100, 60],
 ["beginPath"], ["moveTo", 20, 200], ["lineTo", 380, 200], ["stroke"]]
```

Each element's first slot is either a real `CanvasRenderingContext2D`
**method** name (called as `ctx[name](...rest)`) or a **property** name
(assigned as `ctx[name] = rest[0]`) — the JS interpreter (§6) checks two
explicit allowlists before doing either, so an unrecognized or malformed
op name is a clean, logged no-op rather than a thrown exception or (worse)
blindly indexing into `ctx` with an attacker/bug-controlled string.

v1 vocabulary (a useful, bounded subset, not the full spec):
- **Path/shape**: `beginPath`, `closePath`, `moveTo`, `lineTo`, `rect`,
  `arc`, `arcTo`, `quadraticCurveTo`, `bezierCurveTo`
- **Paint**: `fill`, `stroke`, `clip`, `fillRect`, `strokeRect`, `clearRect`
- **Text**: `fillText`, `strokeText`
- **State/transform**: `save`, `restore`, `translate`, `rotate`, `scale`,
  `resetTransform`
- **Properties**: `fillStyle`, `strokeStyle`, `lineWidth`, `lineCap`,
  `lineJoin`, `font`, `textAlign`, `textBaseline`, `globalAlpha`

Deferred (§7): gradients/patterns, `drawImage`, compositing modes,
`ImageData` pixel access, hit-region/pointer-event support.

### 5.3 Why the ordered channel, not `LatestSlot`

`LatestSlot<T>` (`vm_host.rs`'s own doc comment, lines 38-52) is built
for a *continuous* stream where a stale intermediate value is genuinely
worthless once a newer one exists — its own doc comment even names "a
redrawing pixel buffer" as the motivating example. Canvas commands were
seriously considered for this, then deliberately rejected: unlike a full
pixel-buffer frame, a command batch here is usually **cumulative** — a
`lineTo`/`stroke` batch draws relative to whatever the canvas already
shows, exactly like the real Canvas 2D API always has. Dropping an
intermediate batch under `LatestSlot`'s "latest wins" discipline wouldn't
just show a stale-but-complete frame (fine to skip) — it would
permanently lose real drawing operations that were never redundant with
anything later (not fine to skip, ever).

The existing ordered `mpsc`-backed channel already gives "many commands,
one round trip" for free (§1) via one `VmResponse::CanvasDraw` whose own
`commands_json` holds a whole batch — so the one property `LatestSlot`
would add here (dropping backlog under a slow UI thread) isn't worth its
correctness risk for this command shape. If a *true* full-frame-redraw
mode is ever wanted (e.g. `Canvas>>clearAndDisplay:`, always starting
from an implicit clear), that's expressible as a *convention inside one
batch* (a `clearRect` covering the whole canvas as the batch's first
command) — it doesn't need a different delivery mechanism, just a
different command sequence.

## 6. The JS-side interpreter (built now, `gui/assets/smtk.js`)

```js
const CANVAS_METHODS = new Set([
  'beginPath', 'closePath', 'moveTo', 'lineTo', 'rect', 'arc', 'arcTo',
  'quadraticCurveTo', 'bezierCurveTo', 'fill', 'stroke', 'clip',
  'fillRect', 'strokeRect', 'clearRect', 'fillText', 'strokeText',
  'save', 'restore', 'translate', 'rotate', 'scale', 'resetTransform',
]);
const CANVAS_PROPERTIES = new Set([
  'fillStyle', 'strokeStyle', 'lineWidth', 'lineCap', 'lineJoin', 'font',
  'textAlign', 'textBaseline', 'globalAlpha',
]);

function macvmCanvasDraw(id, commandsJson) {
  const canvas = document.getElementById('macvm-canvas-' + id);
  if (!canvas) return;
  const ctx = canvas.getContext('2d');
  for (const cmd of JSON.parse(commandsJson)) {
    const [name, ...args] = cmd;
    if (CANVAS_METHODS.has(name)) ctx[name](...args);
    else if (CANVAS_PROPERTIES.has(name)) ctx[name] = args[0];
    else console.warn('macvm canvas: unknown op', name);
  }
}
```

Called from Rust exactly like every other page mutation:
`eval_js(&format!("window.macvmCanvasDraw({id}, {})", js_string_literal(&commands_json)))`.

## 6a. Generic "Smalltalk drives the canvas" (built now)

`Run Demo` (§4) still uses Rust-mock content, but there is now a **generic**
path for real Smalltalk to draw, added without any per-drawing GUI code:

- `VmRequest::CanvasEval { code: String }` — the worker evaluates `code`
  (`VmHandle::eval_to_string`, wrapping `[<code>] value` so multi-statement
  bodies work) and, if it answers a `String`, returns it as an ordinary
  `VmResponse::CanvasDraw` batch. The GUI carries an opaque code string in,
  gets an opaque command string back — it holds no knowledge of what is drawn,
  exactly like `Doit`. A non-`String`/failed eval degrades to a transcript
  note, never a broken draw.
- The trigger is data-driven: any control with a `data-canvas-eval="<expr>"`
  attribute (canvas action `"eval"`) posts its expression through this one
  path (`smtk.js` → `main.rs`). Adding another drawing is a new button with a
  different expression (or a Workspace eval), never new Rust/JS.
- **Animation-ready**: a frame is just another `CanvasEval` (`anim frameAt:
  k`); a future GUI-side loop can post them on a timer/`requestAnimationFrame`
  without any new message type.

### The pixel-buffer (pixmap) path — built now

For a per-pixel image (the Mandelbrot), vector `fillRect` commands are the
wrong primitive: even run-length-encoded, a 420x220 fractal is a ~600 KB JSON
batch of ~10k ops that the JS side must parse and replay. The pixel path
replaces that with a raw buffer blitted in ONE `putImageData`:

- `world/36_pixmap.mst` — `Pixmap` (a `width*height*4` RGBA `ByteArray`,
  `atX:y:r:g:b:` writing four bytes per pixel, row-major, matching
  `ImageData.data`). No strings, no commands — the hot path only does primitive
  byte stores.
- `VmRequest::CanvasEvalPixels { code, width, height }` — generic, like
  `CanvasEval` but the code answers an RGBA `ByteArray`
  (`VmHandle::eval_to_bytes`, raw bytes, no UTF-8). The worker checks the size,
  base64s it, and answers `VmResponse::CanvasPixels`. `main.rs` →
  `smtk.js macvmCanvasPutPixels`, which `atob`s it into an `ImageData` and
  `putImageData`s it. Base64 because `evaluateJavaScript` carries a source
  string, not binary; the `WKURLSchemeHandler` `macvm-pixels://<id>` path (the
  `vm_host.rs` module doc) is the scale-up for animation/high-frequency frames.
- The Canvas view's **Mandelbrot** button carries `Mandelbrot new
  pixelsForWidth:height:` in `data-canvas-eval` with action `eval-pixels`.

**Measured vs the fillRect path** (release, warmed, native 420x220): compute
746 ms → **458 ms** (the ~288 ms of string-building is gone), recompiles 2 →
**0**, allocation 708 MB → 595 MB, and the JS side does one blit instead of
replaying 10,685 ops. What remains (595 MB / ~9 deopts) is the boxed-`Double`
arithmetic in `escapeAtRe:im:` — the "fast floats" gap the tour names, which
needs unboxed-double JIT support (a separate VM feature, not this path).

**The `Mandelbrot` itself** computes the set in JIT-compiled `Double`
arithmetic and writes RGB straight into the Pixmap — no VM-side `Canvas`
primitive is needed (§7 stays deferred); the buffer is built entirely in
image-side Smalltalk.

**Method-arity note (docs/VMregisters.md §2, weekend_work.md Gap 2):** the JIT
caps a compilable method at 5 args. `Pixmap>>atX:y:r:g:b:` is exactly 5, so it
compiles; the earlier `emitOn:color:x:y:w:h:` (6 args) never did and ran
interpreted. Raising the cap is deferred — for now the fix is to keep
image-side methods ≤ 5 args.

## 7. Deferred

- The VM-side `Canvas` class and its `<primitive: N>` bodies — the real
  point at which this becomes usable from Smalltalk at all. Simply haven't
  been built yet (this whole feature is explicitly a side track staying out
  of the compiler/deopt work's way); the GUI-side rendering half, committed
  3daaf72, is the only implemented portion.
- Multiple simultaneous canvases (`id` is already threaded through the
  protocol for this reason, but v1's GUI side only ever creates/targets a
  single `macvm-canvas-0`).
- Pointer/keyboard input events flowing *from* the canvas back to
  Smalltalk (this design is draw-only, one direction).
- The full Canvas 2D vocabulary (gradients, patterns, `drawImage`,
  `ImageData`, compositing modes).
- A `WKURLSchemeHandler`-based pixel-buffer path (`vm_host.rs`'s own
  documented plan for bulk RGBA data) — not needed here since vector path
  commands are cheap as JSON text; only relevant if a future feature wants
  raw pixel buffers instead of vector drawing.

## 8. Cross-references

- `gui/smappl.md` — the related-but-distinct static-embedding mechanism.
- `gui/PLAN.md` — the Phase G GUI-shell plan this widget extends.
- `docs/SPEC.md` decision log gains an entry recording this design.
