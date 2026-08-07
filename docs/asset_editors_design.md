# Asset editors — a Sprite Editor and a Sound Editor, under a Tools menu

**Status: design (stage 1 in progress).** Ports MACDART's sprite and sound
editors (`~/claudeprojects/MACDART/SPRITE_EDITOR_PLAN.md`,
`SOUND_EDITOR_PLAN.md`) to macVM: two utility windows that graphically edit
the 16-colour sprites and parametric sound effects the GamePane renders,
preview/audition them through the *real* engine, and save them as **source
in the image** — a sheet class any game files in and uses with one line.
They are the first genuine *applications* hosted by macVM, which is the
point: the platform claim is not "macVM has a GUI" but "you can build apps
in it".

## 1. What recon established (verified in both codebases)

The port is far cheaper than it looks, because both ends descend from the
same lineage.

**The sprite art format is identical.** MACDART and macVM both speak
'/'-separated hex-row art, 4 bits per pixel, index 0 transparent, per-sprite
16-entry palette. macVM's consumption API
([43_gamepane.mst](world/43_gamepane.mst)) is selector-for-selector what the
MACDART sheet format's `installOn:` emits: `defineSprite:` → Sprite,
`addFrame:`, `colorAt:r:g:b:`, `moveTo:y:frame:`, `hide`. A sheet class
saved by one editor would *install on the other's pane unchanged*.

**The synth recipe is identical.** MacGamePane's
`Effect` (`audio/src/synth.rs:70`) is field-for-field MACDART's gp_synth
`Effect` — both transcribe the same `Audio.mod`: duration + ≤4 `Oscillator`s
(sine/square/saw/triangle/noise/pulse × freq/amp/phase/pulse-width) +
`Adsr` + linear sweep + noise mix + tanh distortion + echo
(count/delay/decay), `render(effect, rng)` → PCM, deterministic per `Lcg`
seed. **The synth needs zero changes** — only the wire, exactly the one
native addition MACDART's plan scoped (`gpeffect`).

**The store path already exists.** MACDART's doctrine — save must STORE,
not live-reload — is macVM's `acceptEditorClass:`
([host_service.rs](cocoa_gui/src/host_service.rs)): syntax-gate with the
real compiler parser, write only the diff to the image, splice the world
`.mst` tree, no live install. The editors call it and are done; persistence
costs a verb call, not a design.

**The pieces that differ:**

| Concern | MACDART | macVM |
|---|---|---|
| Editor code runs on | UI isolate (Dart) | UI worker VM (Smalltalk, Cocoa bridge) |
| Game engine driven by | UI isolate (`gpOpen` returns the NSView, parented anywhere) | **primary VM** via the game channel; the pane is its own NSWindow ([game.rs](cocoa_gui/src/game.rs)) |
| Paint surface | NSImageView + `renderInto` native draw ops | NSImageView + the canvas base64 blast (`showCanvasPixelsOn:base64:width:height:`, the CocoaCanvas path) |
| Mouse | gesture recognizers → generic `onAction` proxy with `locationInView` | gesture recognizers → `MacvmDelegate` action target — **but `macvmAction:` drops the sender** (`CocoaToolbarAction>>macvmAction: sender [ ^Block value ]`), so a 1-arg holder variant is needed for `locationInView:` |

## 2. Decisions

**A Tools menu.** `installToolsMenuOn:` between View and Demos: *Sprite
Editor…* now, *Sound Editor…* when stage 6 lands (no dead menu items). Menu,
not toolbar segments — the view switcher is a group of always-available
IDE surfaces; editors are utilities you summon.

**Each editor is its own window.** Not a tab in the view registry: the
registry's segmented control is for the nine persistent IDE views, and an
asset editor wants to sit *beside* the game window it previews into.
`setReleasedWhenClosed: false`; the window's `#window` delegate role answers
`windowShouldClose:` by hiding (`orderOut:`) and refusing — MACDART chose
hide-don't-close for want of close notifications; we have them and choose it
deliberately (state survives, reopening is instant).

**The model is pure Smalltalk, the same split as MACDART's.** `SpriteDoc` /
`SoundDoc`: no Cocoa sends, headless-testable. Pixels are one ByteArray of
palette indices per frame; palette is 16 `#(r g b)` rows (DB16 default);
operations are setPx / fill / shift-wrap / resize-pad-crop / frame
add-dup-del, hex-rows emit+parse (`.` ≡ `0` on parse), sheet-source emit,
name validation. The MACDART models (`spriteed_model.dart` 331 lines,
`sounded_model.dart` 287) are the transcription source.

**The sheet format is MACDART's, verbatim in shape:**

```smalltalk
"SpriteSheet: Ship — WRITTEN BY THE SPRITE EDITOR. Edit via Tools ▸ Sprite Editor."
Object subclass: Ship [
    Ship class >> isSpriteSheet [ ^true ]
    Ship class >> frames [ ^#('0ff0/f11f/…' '…') ]
    Ship class >> palette [ ^#( #(0 0 0) #(230 240 255) … ) ]   "16 rows"
    Ship class >> installOn: aPane [
        | s |
        s := aPane defineSprite: self frames first.
        self frames allButFirst do: [ :f | s addFrame: f ].
        1 to: 15 do: [ :i | | rgb | rgb := self palette at: i + 1.
            s colorAt: i r: (rgb at: 1) g: (rgb at: 2) b: (rgb at: 3) ].
        ^s ]
]
```

`installOn:` is the point of the format — a game does
`ship := Ship installOn: pane.` and has art, frames and palette in one send.
`isSpriteSheet` (`isSoundSheet` for sounds) is the discovery marker for
listing. Saved via `acceptEditorClass:`; the sheet lands in the image *and*
the world tree like anything the class editor accepts.

**Preview is the real engine, driven the way demos are.** The editor
composes a small entry doit — resize, background fill, the sheet install,
three spawns at 1×/2×/4× (`moveTo:y:frame:` + engine `frameRate:` when
Play is on) — and hands it to `launchDemo:`
([host_service.rs](cocoa_gui/src/host_service.rs)), the exact path the
Demos menu uses. The game window opens beside the editor; a rebuild is a
relaunch with the regenerated doit. Coalesced: dirty-flag + ~300 ms idle
timer, MACDART's own cap. The editor never *edits* engine state — every
preview is a full rebuild from the model, which is also why it cannot drift.

**Sound audition needs no primary round-trip for presets** —
`game::play_sound(preset)` ([game.rs:609](cocoa_gui/src/game.rs:609)) is
callable straight from a host-service verb. Parametric audition (stage 6)
ships the flat parameter list through the new command.

## 3. The one native addition (stage 6): the parametric effect wire

MACDART's plan, transposed:

```
['effect', slot, duration, a, d, s, r, sweepStart, sweepEnd, noiseMix,
 distortion, echoCount, echoDelay, echoDecay, seed, oscCount,
 (wave, freq, amp, phase, pulseWidth) × oscCount]
```

- **Prim 263** (`43_gamepane.mst` stops at 262): takes the flat params as a
  Smalltalk Array, emits `GameCommand::Effect { … }`.
- **cocoa_gui** `game.rs` apply: build `Effect`, `Lcg::new(seed)` (the seed
  crosses the wire so noisy recipes reproduce exactly),
  `synth::render`, hand the PCM to playback — beside the existing preset
  path.
- **Smalltalk face**: `Sound class >> effect: paramsArray` +
  `Sound class >> playEffect:` (or slot-scoped if slots become real).
- **The flat parameter order is THE contract** — one canonical order shared
  by the prim, the Rust apply, `SoundDoc paramsList`, and saved sheets. It
  is written down once, here; every consumer cites this section.

MacGamePane's `synth.rs` is untouched: `Effect`, `render`, `Lcg` all exist.
Clamps live in the **model** (duration ≤ 4 s, echo tail ≤ remainder) so the
native side never truncates.

## 4. Layout (transcribed from MACDART, adjusted to our controls)

Sprite Editor (~980×560, fixed):

```
+------------------------------------------------------------------+
| [grid canvas ~432×432 NSImageView]  | Name [______] W [_] H [_]  |
|   checkerboard = transparent       | [Resize]                   |
|   cell size auto from sprite size  | palette: 16 swatches (2×8) |
|                                    | R ▓▓▓ G ▓▓▓ B ▓▓▓  #hex    |
| tools: (Pencil)(Fill)(Pick)        | Frame 2/5 [|<][<][>][Add]  |
| [◀][▶][▲][▼] shift   [Clear]       |           [Dup][Del]       |
|                                    | anim fps [slider] [x] Play |
| status: …                          | [Preview] (game window)    |
|                                    | [Save][Load ▾][Copy Code]  |
+------------------------------------------------------------------+
```

Sound Editor (stage 6, ~980×560): left = envelope/sweep visualization —
drawn on the *game pane* with the drawing prims, MACDART's own trick (the
editor renders its UI with the engine that plays its sounds) — over the
four oscillator rows (wave popup, freq/amp/phase/pw); right = labelled
sliders (duration, A, D, S, R, sweep ×2, noise, distortion, echo ×3), seed
field, preset popup seeded from the 12 transcribed presets, Play,
Randomize/Mutate (the sfxr joy, bounded to musical ranges),
New/Save/Load/Copy Code, status.

## 5. Mechanics worth writing down before they bite

- **Mouse painting** *(settled in stage 3, the hard way)*: world-minted
  `NSGestureRecognizer`s never fire — wired identically to every working
  button (valid SEL, retained target, clean hitTest), silent for real mice
  AND synthetic `sendEvent:` events; root cause inside AppKit
  unestablished, leading suspect the recognition arbitration an
  NSImageView's own built-in recognizer wins. The shipped mechanism is the
  `#mouseview` role: `MacvmMouseView`, an NSImageView SUBCLASS registered
  through the C6 door (`register_class_under`, the scripting arc's
  superclass generalisation), whose `mouseDown:`/`mouseDragged:` overrides
  dispatch the NSEvent to a per-view `CocoaMouseHandler`. Responder-chain
  delivery is unconditional — no arbitration to lose — and the class
  answers `acceptsFirstMouse:` YES natively so the first click paints.
  The handler converts `locationInWindow` via `convertPoint:fromView:`
  (`#point` structs cross by value), truncates to Integer BEFORE `//`
  (Float has no `//`), flips y, paints. Painting is idempotent per cell
  and each repaint is one canvas blast, so drag-rate delivery is harmless.
  `spedSynthClickAt:y:` mints real NSEvents and pushes them through
  `sendEvent:` — the permanent no-hands gate for this whole path.
- **Coalescing rides CocoaUI's ~4Hz metrics beat**, not an `NSTimer`: a
  world-targeted timer never fires — the gesture recognizers' fate again,
  and together they draw the actual line: *control actions* (buttons,
  menus, sliders) reach world targets; *run-loop-originated callbacks*
  (recognizers, timers) do not. Anything periodic hooks the metrics tick
  (`updateMetricsMem:…` calls `previewTick`, late-bound by name); two
  quiet ticks ≈ half a second, MACDART's own cap.
- **The grid repaint is a full-canvas base64 blast** (~432×432 RGBA ≈ 1 MB
  of base64 per repaint), which is the house doctrine — blast, don't patch
  — at a cost worth measuring. Coalesce repaints behind a dirty flag at
  ~10 Hz; if Smalltalk-side byte-pushing stutters, shrink the grid canvas
  before inventing a patch path.
- **Load/list go through the primary** (the image is its world):
  `uiDoit: 'Ship frames'` / a reflection sweep for `isSpriteSheet`
  implementors, replies parsed from `printString`. Same route the Canvas
  widget's draw-eval already rides.
- **Mutual exclusion with games** is real but simpler than MACDART's: the
  pane is driven by whatever the primary last launched. Opening a preview
  relaunches the pane (stopping any demo); launching a demo replaces the
  preview. The editor's status line says so; its Preview button takes the
  pane back. No ownership flag machinery — `request_launch` is already
  last-writer-wins.
- **Numbering/lists**: editors are UI → new world files go in
  `world/cocoaui.list` (load order after 64/65/66; late-bound lookups by
  name, the CG7/CG8 discipline). Model classes are world files too but
  UI-list members — they touch no Cocoa, yet only the UI worker uses them.

## 6. Staging (a commit per stage, verified before each)

1. **Design doc + Tools menu + window skeleton.** `CocoaSpriteEditor` with
   an empty fixed-size window, hide-on-close, Tools ▸ Sprite Editor…
   reopens it. `CocoaSenderAction` (the 1-arg holder) lands here.
   Gate: menu item exists, window shows/hides, `typecheck` clean.
2. **`SpriteDoc`, headless.** The pure model + a verification doit battery
   (hex round-trip, `.`≡0, resize pad/crop, shift wrap, frame ops, sheet
   emit). Gate: the battery runs green headless (`macvm run --world world`).
3. **Painting.** Grid blast, swatches, RGB sliders, pencil/fill/pick,
   shift/clear, frame strip. Gate: scripted `gui` drive — open, paint two
   cells, read `SpriteDoc` rows back exactly; snapshot for the eyes.
4. **Preview.** The composed doit through `launchDemo:`, idle-coalesced;
   fps slider + Play. Gate: preview window shows the 3-scale scene;
   editing recolours it within ~a second.
5. **Persistence.** Save via `acceptEditorClass:`, Load ▾ from the
   `isSpriteSheet` sweep, Copy Code onto the pasteboard. Gate: save → list
   → mutate → load → rows byte-identical; the saved class visible in the
   Browser; galaxigans still boots (world intact).
6. **Sound Editor.** Prim 263 + `GameCommand::Effect` + `SoundDoc` + the
   window; preset seeds transcribed from `synth.rs`. Gate: audition beeps
   headlessly (command reaches the sink), scripted param round-trip, saved
   sheet's `play` works from a Workspace doit.

Stages 2–5 are sprite-editor-complete without any native change; stage 6
carries the whole native surface of the arc.

## 7. Risks, honestly

- **Gesture recognizers through the bridge — resolved, negatively.** They
  refuse (see §5); the bridge grew the `#mouseview` role instead, which is
  the *stronger* mechanism. Any future control that wants mouse input
  should mint a mouse view, not reach for recognizers.
- **Preview-by-relaunch cost.** `request_launch` tears down and rebuilds
  the pane window per rebuild. At 300 ms idle coalescing this is fine for
  editing; if it flickers, the fix is a `GameCommand`-level soft-reset (a
  cheaper reuse path), not editor-side state surgery.
- **No undo in v1** — MACDART's own call. The model keeps operations small
  and pure so single-level undo is a later afternoon, not a redesign.
- **Reply parsing** (`printString` → arrays) is the same informal contract
  the Canvas widget already lives on; a malformed sheet class breaks its
  own load, not the editor (parse errors surface in the status line).
