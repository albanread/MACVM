# Shared screen memory — the VM writes video memory, not messages

*Design, 2026-08-10. Status: SM0–SM3 planned, nothing built.*

## 0. The thesis

`GameCommand` currently carries two different kinds of traffic: **control**
(open a pane, set the frame rate, start the loop, play a sound, scroll the
view) and **state** (every pixel, every glyph). Control belongs on a channel.
State does not — and every problem this document exists to fix comes from
putting it there.

The correction: **`GameCommand` becomes control-plane only, and all bulk state
lives in shared memory the VM writes directly.** Two planes — pixels and text —
each a buffer in GPU-visible memory, each addressed by ordinary byte stores.

This is not speculative. The pixel half is **built and working in MACDART**
(`macdart/cocoa/gamepane/gp_engine.mm`, `GpDirectPane`; designed in
`GAMEPANE_PLAN.md` §6b) and this design is a port of it, with the parts that
would otherwise have been guesswork taken from that implementation rather than
re-derived.

## 1. What the two problems actually were

Worth stating precisely, because they are not the same problem and only one of
them is about bandwidth.

**The text flood (Minesweeper, fixed by discipline).** The text overlay is a
*retained* RGBA layer. Redrawing it re-sent one `Text` command per revealed
number, every frame — ~100 commands/frame for a board that had not changed. The
fix was "only redraw when dirty": correct, but it is a rule every future demo
author must remember, and the first author to meet it got it wrong.

**The pixel copies (everywhere, unfixed).** `blit:` was never a flood — it is
one command per frame. But it costs **three full-frame copies** and a
per-frame allocation:

1. Smalltalk `ByteArray` → `copy_bytes_out` into a fresh `Vec<u8>`
   (`prim_game_blit`, src/runtime/primitives.rs)
2. that `Vec` moves through the `Mutex<VecDeque>` to the main thread
3. `IndexedPane::blit` memcpys it into `buffers[slot]`
4. `upload()` → `texture.replace_region(...)`

At 320×240 that is noise. It scales linearly with any ambition about
resolution, and it is pure waste: the VM already had the pixels.

Shared memory fixes the second problem directly and the first one
*structurally* — writing a character becomes a store, so there is nothing left
to flood and no discipline to remember.

## 2. Grounding — what already exists (verified)

- **`Alien forAddress: ptr size: n`** — primitive 120, with `byteAt:` (112) and
  `byteAt:put:` (113) and the typed accessors beside them (docs/FFI.md §4). It
  is `Format::IndexableBytes` with an external-address field: a **length-bounded
  view over foreign memory**. Already in real use — `world/30_date_time.mst:194`,
  `world/61a_accelerate.mst:79,271`, `world/75_dns.mst:104`. This is the whole
  enabling mechanism, and it is done.
- **`NativeBuffer`** (world/61_posix_io.mst) is the established pattern for
  wrapping a foreign page in a Smalltalk object with a GC-stable address.
- **The pane is already multi-buffered**: `IndexedPane` holds
  `buffers: Vec<Vec<u8>>` with per-slot dirty flags and `NUM_BUFFERS` slots.
- **Palette-in-shader already exists** — `palette_buffer` is created
  `StorageModeShared` and updated by `copy_nonoverlapping` into `contents()`.
  The pane already does the exact trick this design generalises.
- **Font**: 5×7 glyphs, advance 6 (`text_overlay.rs:22-25`).

## 3. The pixel plane — port of MACDART `GpDirectPane`

`MTLStorageModeShared` buffers whose `contents()` is CPU-writable memory the
GPU samples, with a **linear `R8Uint` texture view** over each
(`newTextureWithDescriptor:offset:bytesPerRow:`). No upload, no copy: the bytes
the VM writes *are* the bytes the GPU reads.

Four mechanics taken from the working implementation rather than invented:

- **Stride is not width.** `bytesPerRow` must be a multiple of
  `minimumLinearTextureAlignmentForPixelFormat:`. The buffer is
  `stride * h`, and a demo addresses `fb[y * stride + x]`. **This must be
  exposed** (`pane stride`); a demo that assumes `y * w + x` draws a sheared
  picture. It is the single most likely porting bug.
- **Three rotating buffers, no fence.** The VM writes buffer `write_`; present
  renders it and then advances `write_`. Three-deep means the buffer being
  written is ≥2 frames past the GPU's last read. A completion-handler fence is
  the belt-and-braces upgrade, not a requirement.
- **Palette stays in its own shared buffer**, sampled by the fragment shader —
  so index→colour and palette cycling cost nothing and need no per-frame work.
- **Present stays on the main thread.** Writes are off-thread and always have
  been thread-agnostic; what has thread affinity is command encoding,
  `nextDrawable`, and AppKit — none of which is a pixel write.

Smalltalk surface:

```smalltalk
fb := pane screenMemory.        "an Alien over the current write buffer"
s  := pane stride.
fb byteAt: (y * s) + x + 1 put: colourIndex.
pane present.                   "renders it, rotates the write buffer"
```

`screenMemory` is re-fetched each frame (it names the *current* write buffer).

## 4. The text plane — the overlay becomes a screen buffer

Same idea, one level up: **stop treating the overlay as a chain of commands and
treat it as a screen buffer.** A character grid in shared memory, rendered by a
shader that indexes a font atlas.

- **Geometry**: cell 6×8 (the 5×7 glyph plus one column and one row of
  leading), so a 320×240 viewport is **53×30 cells**.
- **Cell = 4 bytes**: `char`, `fg` palette index, `bg` palette index, `flags`
  (bit 0 = transparent background). Four bytes keeps shader indexing trivial
  and the whole plane is 53×30×4 = **6,360 bytes**.
- **Font atlas**: the existing 5×7 table baked once into a 256-glyph texture at
  startup. No per-frame rasterisation at all.
- **Its own scroll offset**, defaulting to fixed-to-viewport — which preserves
  the behaviour Minesweeper deliberately relies on, where the HUD holds still
  while the board shakes — but scrollable on request.

Smalltalk surface:

```smalltalk
tm := pane textMemory.
tm byteAt: (row * pane textCols + col) * 4 + 1 put: $A asInteger.
```

and, because it is just memory, **a whole text screen is one copy**:

```smalltalk
tm replaceFrom: 1 to: page size with: page    "a prepared 6,360-byte page"
```

That is the capability the flood was hiding: menus, help pages, consoles,
listings and score screens can be composed offline and shown instantly, with
zero commands.

**Honest limit.** A cell grid snaps glyphs to 6-pixel boundaries. It does not
replace object-attached text at arbitrary offsets — Minesweeper's digits centred
in 16px cells and FreeCell's ranks at +3,+4 inside a 34px card on a 38px pitch
are neither multiples of 6. Those stay pixel art in the pixel plane, and that is
the right answer for them. The text plane is for text *screens*, which is
exactly what it is being asked for.

## 5. The payoff that is not about speed

Several VMs can write **disjoint regions of the same plane with no copy between
them**. MACDART calls this out as the feature rather than the limit, and it is
the strongest single argument here: ParallelMandel currently computes tiles in
worker VMs and pickles them back to the primary to be blitted. With shared
screen memory each worker writes its own band directly — no serialisation, no
round-trip, genuinely parallel rasterisation.

## 6. Hazards, and how each is closed

- **Stride ≠ width** — §3. Expose it; test a non-aligned width.
- **Lifetime.** Buffers are freed only on pane close, and the writer VM is
  stopped *first*, so no live `Alien` ever views freed memory (MACDART's rule,
  adopted verbatim). The S21 supervisor can respawn the primary, so the Alien
  must carry the pane **generation** — a stale one fails its primitive rather
  than writing freed memory. Same discipline the sprite registry already uses.
- **Scribbling past the end** — closed by construction: an `Alien` is
  length-bounded and its accessors range-check. A raw pointer would not be.
- **Tearing** — three buffers plus present-advances-write (§3).
- **Blit stays.** It is a fine convenience for a demo that has a `ByteArray`
  already, and removing it would break Life, Minesweeper, FreeCell and
  MandelZoom for no benefit. Shared memory is the fast path, not a replacement.

## 7. Sprints

- **SM0 — the pixel plane.** Shared buffers + linear texture views + stride +
  palette shader; `screenMemory`, `stride`; the pane gains a direct mode.
  *Proof:* a full-screen plasma or Julia written pixel-by-pixel that emits
  **zero `Blit` commands** — asserted by a headless test counting commands, plus
  a rasterised readback.
- **SM1 — the text plane.** Cell buffer + font atlas + shader; `textMemory`,
  `textCols`, `textRows`. Port the Life/Minesweeper/FreeCell HUDs onto it.
  *Proof:* the idle-frame budget test tightens from "at most 2 blits" to
  **zero commands other than `Present`**.
- **SM2 — worker bands.** Several VMs writing disjoint regions; ParallelMandel
  rewritten to write straight to screen memory. *Proof:* the pickle round-trip
  disappears from the profile.
- **SM3 — text screens.** `replaceFrom:to:with:` page blasting, a `TextScreen`
  helper (menus/help/console), and a demo that proves "instant".

## 8. Rejected

- **A glyph display list** (records of x/y/char/colour) instead of a cell grid:
  more general, handles object-attached text — but the user's framing is right,
  the overlay should *be* a screen buffer, and object glyphs are already served
  by pixel art. Revisit only if a real case appears.
- **Handing out a raw pointer** instead of an `Alien`: loses the length bound
  that makes a scribble impossible.
- **Dropping `blit:`** — §6.
- **A fence in v1**: triple buffering plus pull pacing already gives ≥2 frames
  of separation; add the completion handler only if measurement shows tearing.
