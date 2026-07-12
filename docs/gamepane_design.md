# GamePane — MacGamePane integration design

**Status:** design pass. Product of a source survey of both repos
(`gamepane-understand` workflow, 2026-07-12). No VM source modified yet.

**Goal.** Let a 2D retro game be written **in Smalltalk** and run on MACVM,
with its game loop JIT-compiled, drawing through the
[MacGamePane](https://github.com/albanread/MacGamePane) engine
(`~/claudeprojects/MacGamePane`) — the palette-indexed, sprite-driven,
chiptune-scored engine already built as a standalone Rust crate specifically
so MACVM could consume it (its README names MACVM as the intended consumer;
MACVM's own `docs/CANVAS.md` records it as "not yet consumed").

**Chosen render model (user decision):** a **native Metal game pane** — a
real Metal-backed `NSView` embedded in the MACVM GUI window — *not* pixels
rasterized into the WKWebView `<canvas>`. This keeps GPU sprites, per-scanline
palettes, and shader backgrounds at full speed.

---

## 1. What the engine gives us (survey, source-grounded)

MacGamePane is three crates: `macgamepane-graphics`, `macgamepane-audio`
(depends only on `-objc`, **not** on graphics — drivable alone), and
`macgamepane-objc` (a raw dlsym/objc_msgSend bridge, a near-copy of MACVM's
own `gui/src/objc.rs`).

The single most important finding: **the panes are already host-drivable.**
Every graphics layer — `ShaderPane`, `IndexedPane`, `Sprites`, `TextOverlay`,
`Blitter` — is constructed from a **borrowed `&metal::Device`** and every
`render()` takes a **host-supplied `&metal::CommandBufferRef` + `&metal::TextureRef`**.
No pane owns a drawable, a command queue, a window, or a run loop.
`GameWindow` (the only thing that creates an `NSWindow`/`CAMetalLayer` and
pumps events) is **optional** — a host that owns its own view + layer + device
+ queue drops `GameWindow` entirely and pumps one frame at a time. So the pane
**`render()` call shape** is exactly what we reuse. The **frame *driver*** is
*not* reused: `GameWindow` is a single-threaded main-thread sleep-loop, and
neither repo uses `CVDisplayLink` — MACVM's own frame driver (a main-thread
timer, §3) is new code with its own threading contract, not a lift from the
engine.

Public surface we will expose to Smalltalk (subset to start):

- **`IndexedPane`** — `cls`, `pset`, `pget`, `line`, `circle`, `disc`,
  `fill_rect`, `set_scroll`, palette `set_rgb`/`set_line_rgb` (per-scanline!),
  `swap_buffers`, `upload`, `render`. 8 buffer slots; colour index 0 is
  transparent.
- **`Sprites`** — `define_sprite(rows: &str)` — art is a `/`-separated string of
  **hex digits 0–f** (4 bits/pixel, 16 colours); width is *derived* from row
  length, so there are **no `w:`/`h:` parameters** — plus `add_frame`,
  `sprite_rgb`, `place`, `move_to`, `set_scale`/`rotation`/`alpha`, `animate`,
  `tick`, `hit` (bbox), `render`. (There is **no** byte-array loader today.)
- **`ShaderPane`** — a fullscreen MSL fragment background (`set_param`).
- **`TextOverlay`** — `draw_text` (seven-segment glyphs today).
- **`input`** — `key_held(code)` polling; `Gamepad`.
- **`audio`** — `Sfx::new/start/define/play` (10 named presets: coin, jump,
  zap, shoot, explode, powerup, hurt, click, bang, blip — plus generic
  `beep`/`tone`/`noise` generators), `play_tune_non_blocking` (ABC → MIDI), the
  pure-Rust `VoiceBank` mixer. Note: `Sfx::play(id)` triggers a numbered *slot*
  that must first be `define(id, &Sound)`'d against the one started `Sfx`; a
  Smalltalk `Sound` wraps that slot bookkeeping (§4).

### Three hard constraints the design must respect

1. **`metal` crate version coupling.** MacGamePane's boundary types
   (`metal::Device`, `CommandBufferRef`, `TextureRef`, `MetalLayer`) come from
   the `metal` crate **v0.33**. MACVM does **not** currently depend on `metal`
   at all — so this is a *clean new dependency*, not a conflict, but the GUI
   crate must pin `metal = "0.33"` to match, or the handles won't type-match
   across the boundary.
2. **Keyboard input needs a first-responder `NSView`.** `key_held` reads a
   process-global `HELD_KEYS` table written only by MacGamePane's
   `MacGamePaneKeyView` `keyDown:`/`keyUp:` IMPs; there is no public setter.
   The game pane's `NSView` must therefore *be* (or embed) that subclass and
   hold first responder while the game is active. **Focus model (decided): the
   game pane takes and holds focus while running; `Esc` (or `GamePane>>close`)
   relinquishes first responder back to the WKWebView.**
3. **Fixed pane dimensions.** Pane textures/palette buffers are sized at
   construction; a window/drawable resize means reconstructing the panes.
   The pane will start at a fixed logical resolution (letterboxed), with a
   resize hook deferred.

Plus an **audio hazard**: starting two `AVAudioEngine`s concurrently from
different threads aborts the process uncatchably — so there is exactly **one**
long-lived `Sfx` per session, created once.

---

## 2. The MACVM seams (survey, source-grounded)

- **View swap.** `gui/src/main.rs`'s `navigate_to` special-cases marker paths
  (`browser_view_marker`, `workspace_view_marker`, `canvas_view_marker`,
  `class_outliner_view_marker`, lines ~414–437): navigating to a sentinel path
  swaps a Rust-generated view into the window instead of loading HTML. **A new
  `game_view_marker` follows this exact pattern** to swap the Metal pane in and
  out, with back/forward working uniformly.
- **Threading (the crux).** `gui/src/vm_host.rs` documents the model
  precisely: *AppKit owns the main thread; the VM runs on a dedicated worker
  (`std::thread::spawn`) thread; they communicate only through two channels —
  GUI→VM `VmRequest`s executed between interpreter turns, and VM→GUI responses
  (including bulk `Arc<[u8]>` RGBA payloads) drained on the main thread
  (`vm.drain_responses()`, main.rs:1331) into the view.* No shared mutable
  state. This is the backbone the game pane rides on.
- **Primitive path.** A `<primitive: N>` method dispatches through
  `src/interpreter/send.rs::try_primitive` into the `PRIMITIVES: &[PrimDesc]`
  table (`src/runtime/primitives.rs`), each entry `{ id, name, f, argc,
  can_allocate, can_fail }`; the JIT shims most primitives (`docs/prim_shims.md`).
  A new **game-primitive group** (a fresh id block) is the Smalltalk→Rust seam,
  exactly like the existing SIMD/Alien/FFI groups.
- **Native-handle GC safety.** A retained `Sprite`/`GamePane` holds a small
  **integer handle into a main-thread registry** (not a raw pointer): the
  GPU-owning structs stay on the main thread, the GC never scans them, and a
  stale handle fails a bounds check rather than dangling. The existing `Alien`
  type (`src/runtime/alien.rs`) is the fallback shape if a raw pointer ever must
  cross the boundary, but the integer handle is preferred here.
- **No `metal` crate today** — confirmed; adding it is additive.

---

## 3. The central decision — the frame/threading architecture

> This section was **corrected after an adversarial review** (2026-07-12) that
> grounded every claim in real source. The first draft's frame driver
> ("the main thread's CVDisplayLink owns the render") was wrong — a
> `CVDisplayLink` callback fires on a **dedicated Core Video thread, never the
> main thread**, and the pull model as drafted **deadlocked** the serial VM
> worker. The corrected model below is what the review's findings force.

The game is a **retained-mode GPU scene, not an immediate-mode framebuffer**
(user's model): *load the graphics once — sprite art, palettes, shader params,
background tiles → uploaded to GPU textures — then render at 60 fps while the
per-frame work is almost entirely **control plane**: sprite transforms, scroll
offsets, animation frames, palette tweaks, and input. The GPU does the blitting
(sprite compositing, scrolling, palette LUT, shaders).* This is exactly
MacGamePane's shape — `Sprites` holds definitions uploaded once; per frame you
only `move_to`/`animate`/`set_scroll` and `render`.

### Three named threads

1. **VM worker thread** — runs Smalltalk (game logic + `onStep:`). It is
   **strictly serial**: it services one `VmRequest` at a time and only
   **between whole doits** — there is *no* mid-`eval` preemption, and a nested
   `eval` on this thread is unsafe (it clobbers the per-thread `sigsetjmp`
   recovery slot, the S21 machinery). Consequence: whatever drives the game
   loop must submit **one top-level `GameStep` to an *idle* worker**, never a
   blocking loop and never a nested call.
2. **Main thread** — owns AppKit, the `CAMetalLayer` + its `NSView`, **all the
   panes, and all Metal** (encode + `next_drawable` + `present` + `commit`).
   It also owns the frame driver (below). Everything that touches Metal or a
   pane happens here — single-owner, so no lock and no `&mut` aliasing.
3. *(Deliberately no Core Video thread.)* We do **not** use `CVDisplayLink`,
   precisely because its callback is off-main and would put Metal on a third
   thread. MacGamePane's own `GameWindow` is a main-thread sleep-loop pump, and
   MACVM already runs everything off `NSApp run` on main — the frame driver
   stays on that main run loop.

### The frame driver

A **main-thread timer on the AppKit run loop** — an `NSTimer`/`CADisplayLink`
(`NSView.displayLink(target:selector:)`, macOS 14+, which unlike `CVDisplayLink`
fires on the main run loop) ticking at the display rate. Because it fires on
main, render + present stay on main **by construction** — the "everything Metal
on main" invariant genuinely holds (it is not asserted "for free"; it is true
because we chose a main-thread driver). This mirrors MacGamePane's existing
main-thread pump rather than inventing a cross-thread one.

### The drive model — decoupled render, single-outstanding step

- **Load once.** `GamePane>>loadSprite:` / `palette:` / `background:` submit
  bulk asset data (sprite hex-row art, RGBA palettes, an indexed tile buffer,
  shader params) as a **create/upload delta** over the VM→main channel; the
  **main thread** applies it to the panes' GPU resources (the *only* thread that
  touches them). Rare — start-of-level, not per-frame.
- **`GamePane>>run` returns immediately.** It registers the `onStep:` block
  (rooted worker-side, below) and signals main to start the frame timer. It
  **must not block** — a blocking `[true] whileTrue:` loop would wedge the
  serial worker so no `GameStep` could ever be dequeued (the deadlock the review
  caught). The game loop is owned by the **main-thread timer**, not by a
  Smalltalk loop.
- **Each main-thread tick:**
  1. **Render** the current **scene shadow** (last-known sprite transforms /
     scroll / palette) through the panes and present. This never blocks and
     never waits on the worker — a slow, busy, or dead worker just means the
     last frame is re-shown.
  2. **If the worker is idle** (no `GameStep` outstanding), submit **one**
     `VmRequest::GameStep`. **Single-outstanding**: never enqueue a second while
     one is in flight — this *is* the backpressure/coalescing (no unbounded
     queue, no stale-step pileup, no reliance on the dead `LatestSlot` code). If
     the worker is still busy (a prior step, or a Workspace doit), this tick
     simply skips the step; the render still ran.
  3. When the `GameStep` result lands (drained on main), it updates the scene
     shadow for subsequent renders.
- **`onStep:` runs as a fresh top-level request on the idle worker** — never
  nested inside a still-running `eval` — so the `sigsetjmp` slot is safe.
- **Input.** `keyHeld:` polls MacGamePane's process-global `HELD_KEYS`, written
  by the first-responder `MacGamePaneKeyView` on **main** and read on the worker
  inside the `GameStep` — a lock-free shared table, no channel traffic.

**Consequences (documented honestly, not hidden):** render is **decoupled**
from step — render runs at display rate; the step runs at whatever rate the
serial worker sustains, so input→pixel is a **≥1-frame-latency pipeline**
(tick → submit → worker `eval` → main wake → drain → next render), *not*
tick-synchronous. A running game and a long blocking Workspace doit **cannot
run at the same time** (one serial worker) — expected and acceptable. This
resolves §6's old "pull vs decoupled" open question decisively in favour of
**decoupled render + single-outstanding step**.

Per-frame payload is tiny — 256 live sprites × ~24 bytes ≈ 6 KB, usually far
less — so an immediate-mode "ship a framebuffer" cost never arises; the
one-time asset upload is the only bulk transfer.

**Rejected — pane on the VM thread, present marshaled to main.** Directly
calling `pane.pset` from a worker primitive is cheapest per-op, but it splits
pane ownership across threads and would present a `CAMetalLayer` drawable
off-main (needing explicit `CATransaction` management). The retained model makes
the per-op speed advantage moot — there is no per-op traffic to optimize — and
keeping *all* Metal on the main thread is simpler and matches the rest of MACVM.

### Handles, registry, and lifecycle

- **Generation-tagged handles.** Native pane/sprite/sound structs live on the
  **main thread**. A Smalltalk `Sprite`/`GamePane` holds a **`{index,
  generation}` integer handle** (a SmallInt pair), not a raw pointer. IDs are
  minted **worker-side** from a monotonic counter and shipped in the
  create-delta; the main thread builds the struct into a registry slot carrying
  that generation. Every lookup validates index **and** generation — so a stale
  id from a previous VM generation can never resolve to a reused live slot
  (this is exactly the S15 #93 recycled-address aliasing class of bug, designed
  out here rather than left to a bare bounds check). Explicit allocate/free;
  the registry is bumped/cleared on VM restart.
- **`onStep:` block is GC-rooted worker-side.** After `run` returns and the
  defining doit completes, the `onStep:` closure (and any `Sprite`-referencing
  closures) must survive across every subsequent `GameStep`. A **worker-side
  registry** retains them as GC roots for the running game's lifetime —
  analogous to the existing `SmapplRegistry`/`fire_widget_action` mechanism for
  widget action closures. The handle-registry above roots the *native* structs;
  this roots the *Smalltalk* block.
- **Lifecycle is coupled to the worker generation and the view.**
  - **VM restart (S21).** On a guest crash the supervisor spawns a fresh
    `VmHandle` from the image — all Smalltalk pane/sprite oops die with the old
    heap, but the main-thread registry, GPU resources, and frame timer are *not*
    VM-owned and would otherwise be orphaned. A **main-thread restart hook**
    (fired by the supervisor on respawn) **stops the frame timer, frees the GPU
    resources, and clears/bumps the registry**. The new VM does **not** inherit
    the pane; the user re-opens it if wanted. Because the timer is
    single-outstanding and gated on worker-idle, a worker that dies mid-step
    stops receiving steps immediately while render keeps showing the last frame;
    its in-flight `GameStep` times out on the existing `WORKER_RESPONSE_TIMEOUT`
    and the supervisor respawns. (A shorter game-specific liveness budget is a
    possible refinement.)
  - **View swap-out / window close.** Swapping `game_view_marker` out
    (navigation away) **pauses** the timer; closing the window **tears down**
    the timer + GPU resources + registry entries. The `NSView`'s `CADisplayLink`
    lifetime is tied to the view, so it does not keep firing invisibly.

---

## 4. The Smalltalk-facing API (first cut)

```smalltalk
| pane ship |
pane := GamePane width: 320 height: 240.        "opens the native pane, takes focus"

"— load once (bulk, rare) —"
pane paletteAt: 16 r: 255 g: 200 b: 0.
ship := pane defineSprite: 'f0f/0f0/f0f'.        "hex-row art (0-f, 4bpp) → a Sprite handle"
pane background: aTileByteArray.                 "indexed tile layer → FRONT slot, occasional"
coin := pane defineSound: Sound coin.            "reserves + defines an Sfx slot"

"— per frame: mutate the retained scene, no pixels —"
pane onStep: [:dt |
    (pane keyHeld: KeyLeft)  ifTrue: [ship moveBy: -2 @ 0].
    (pane keyHeld: KeyRight) ifTrue: [ship moveBy:  2 @ 0].
    (pane keyHeld: KeyEsc)   ifTrue: [pane close].     "release focus"
    pane scrollBy: 1 @ 0].
pane run.                                         "registers onStep:, starts the main-thread timer, RETURNS"

coin play.                                        "triggers the reserved SFX slot"
(Tune fromAbc: '...') playOnce.                   "chiptune"
```

Retained objects (backed by the built-in primitive group): **`GamePane`** (the
pane + its frame timer + focus), **`Sprite`** (a `{index,generation}` handle;
`moveTo:`/`moveBy:`/`frame:`/`hit:`), **`Sound`** (a reserved `Sfx` slot),
**`Tune`** (chiptune). Sprite art is a hex-row *string*, not a byte array, and
`defineSound:` reserves a slot against the one shared started `Sfx` before
`play`. `run` **returns immediately** (it does not block — see §3); `onStep:`
registers the per-frame block the main-thread timer submits as a single
outstanding `GameStep`; `keyHeld:` polls input; **`Esc`** (or `close`)
relinquishes first responder back to the browser view.

Every primitive that forwards a Smalltalk integer into an engine setter
**range-checks and fails the primitive first**: `set_rgb`/`set_line_rgb`/
`set_active` `assert!`-panic on out-of-range indices/lines/slots, which in an
embedded VM would abort the process — so the primitive boundary validates and
returns `prim_fails` rather than passing a raw value through.

---

## 5. Milestone ladder

- **M0 — wiring & de-risk.** Add MacGamePane as path deps to the MACVM
  workspace; pin `metal = "0.33"` in `gui/`. A Rust test constructs a pane
  against a headless/off-screen device and renders one frame — proves the
  metal-crate coupling (constraint #1) *before* any Smalltalk work.
- **M1 — audio from Smalltalk.** A game-primitive sub-group for audio (one
  shared `Sfx`); `Sound coin play` and `Tune` playback work end to end. The
  simplest bridge (no rendering thread), a fast confidence win.
- **M2 — a native pane shows one frame.** `game_view_marker` swaps a real
  `CAMetalLayer` `NSView` into the window; the main thread builds the panes and
  renders one static indexed frame from Rust (no Smalltalk yet). Proves the
  native-view embed + Metal-on-main — the *same* thread M4's timer renders on,
  so M2 genuinely de-risks M4's render path (not a different, safer thread).
- **M3 — load assets + mutate the scene.** The load-once path (hex-row sprite +
  palette → GPU) and the small scene-shadow handoff; a doit defines a sprite,
  sets its transform via the generation-tagged handle, and renders one frame.
  Proves the retained interchange + the registry.
- **M4 — the frame loop.** The **main-thread frame timer** (`NSTimer`/
  `CADisplayLink` on the main run loop — *not* `CVDisplayLink`) renders the
  scene shadow each tick and submits a **single-outstanding** `VmRequest::
  GameStep` to the idle worker; `run` returns immediately; a Smalltalk `onStep:`
  block; `keyHeld:`; `Esc` releases focus; a sprite moved by the arrow keys,
  with a sound — the loop running JIT-compiled. Also proves the **restart hook**
  (kill the worker mid-game; the timer stops stepping, render holds the last
  frame, the supervisor respawns, the pane tears down cleanly).
- **M5 — starfield in Smalltalk (capstone).** Reimplement `starfield_demo`
  (shader bg + scroll + sprite + SFX + tune) in Smalltalk on the JIT.

Each milestone: differential-safe where applicable, `cargo` green, visually
confirmed (per `docs/CANVAS.md`'s GUI-verification approach — a static
render + the Preview tools, since computer-use can't attach to the bare
binary), committed.

---

## 6. Risks & decisions

- **metal 0.33 coupling** (constraint #1) — retire it in M0 (a headless render
  test) before any Smalltalk work.
- **Input focus (decided).** The game pane holds first responder while active;
  `Esc`/`close` returns it to the WKWebView. The pane's `NSView` is (or embeds)
  MacGamePane's `MacGamePaneKeyView` so `HELD_KEYS` is populated.
- **Consumption model (decided).** The game pane is **built in** — MacGamePane
  as a workspace **path dependency + a linked-in primitive group**, not a
  dylib/FFI plugin. It compiles into MACVM.
- **Frame driver + drive model (decided — was the review's biggest fix).**
  Main-thread timer (`NSTimer`/main-run-loop `CADisplayLink`, **not**
  `CVDisplayLink`); **decoupled render + single-outstanding `GameStep`**; `run`
  returns immediately; all panes + Metal single-owned on the main thread. See §3
  for the full corrected model and why the old "main-thread CVDisplayLink" /
  "pull, synced at 60 fps" framing was wrong.
- **Restart & teardown lifecycle (decided).** GamePane lifetime is coupled to
  the worker generation and the view: a main-thread restart hook stops the
  timer + frees GPU + clears the (generation-tagged) registry on S21 respawn;
  swap-out pauses, window-close tears down. See §3.
- **Per-frame payload — resolved by the retained model.** Only a small scene
  delta crosses per frame (a few KB of sprite transforms + scroll + palette
  deltas), never pixels. The single bulk transfer is the one-time asset upload;
  a large scrolling *tile background* that changes occasionally goes through the
  `set_buffer` path below, not per frame.
- **The `set_buffer` addition to MacGamePane** — a bulk indexed-buffer load for
  the background tile layer (panes expose only per-pixel `pset`; `buffers` is
  `pub(crate)`, so no public `&[u8]` loader exists). `IndexedPane::render`
  samples the **FRONT** slot only, so `background:` must load into (or
  `swap_buffers`/blit to) FRONT to be visible. Small, in a repo we own; land it
  before M3. Sprites need no such addition (retained).
- **Engine setters `assert!`-panic on bad input** (`set_rgb` needs index ≥ 16,
  `set_line_rgb` needs 1 ≤ index < 16 and line < viewport height, `set_active`
  needs slot < 8). The primitive group **range-checks and `prim_fails`** before
  every such call — never passes a raw Smalltalk integer into an asserting
  engine method (it would abort the embedded process).
- **Sprite art is a hex-row string** (`define_sprite(&str)`, `/`-separated hex
  digits, 4bpp/16-colour, width derived) — *not* a byte array with `w:`/`h:`.
  The binding converts Smalltalk art to that string form (or a
  `define_sprite_bytes` entry point is added to the engine — decide at M3).
- **Audio single-engine discipline** (the `AVAudioEngine` abort hazard) — one
  `Sfx` per session, created on M1's first use; `Sound` handles wrap a reserved,
  `define`'d slot on that shared engine.
- **Resize** — panes are fixed-size; start letterboxed, defer live resize.

---

## Cross-references

- `~/claudeprojects/MacGamePane` — the engine (`docs/DESIGN.md` there for its
  own research trail).
- `docs/CANVAS.md` — the existing pixels-into-WKWebView Canvas (the *other*
  render model, and where the "not yet consumed" note lives).
- `gui/src/vm_host.rs` — the VM-worker/main-thread channel model this rides on.
- `gui/src/main.rs` — the `*_view_marker` view-swap pattern `game_view_marker`
  copies, and `drain_responses` (the main-thread frame hook).
- `src/runtime/primitives.rs`, `docs/prim_shims.md` — the primitive seam.
- `src/runtime/alien.rs` — the native-handle representation.
