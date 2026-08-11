# Direct screen access — writing the picture instead of sending it

*How a Smalltalk demo draws by storing bytes into memory the GPU is already
looking at. Implemented in SM0–SM4; the design and its rejected alternatives
are in [`shared_screen_memory_design.md`](shared_screen_memory_design.md).*

## 1. What this actually is

A synthetic video chip, emulated by a shader.

That framing is not a flourish — it is the most useful way to hold the whole
design. Between them the four planes are the parts a 1980s home computer had:
a **framebuffer** of palette indices, a **colour table** with per-scanline
entries, a **character generator** with its own font ROM, and a **sprite**
layer. The fragment shader is the CRT: it reads that memory sixty times a
second and turns it into light.

Which means the way to program it is the way you programmed those machines —
**poke the memory**. Not "call a drawing API that eventually reaches the GPU",
but store a byte and watch the screen change. The whole of SM0–SM4 is really
one idea: give the guest the memory, and let the hardware emulation do what
hardware does.

The oddity, and the fun of it, is that underneath sits a modern operating
system, a Metal command queue and a JIT — none of which the demo can see. A
Smalltalk program writing `fb byteAt: y * stride + x + 1 put: 3` is doing
exactly what a POKE did, and getting the same deal: no protocol, no
permission, no copy.

## 2. The idea in one paragraph

Apple Silicon has unified memory: an `MTLStorageModeShared` buffer's
`contents()` is ordinary CPU-writable memory, and a linear texture view over
that same buffer is what a shader samples. So the bytes a demo writes **are**
the screen. There is no upload, no copy, and no command — a pixel write is a
store, and the frame appears because the GPU was already reading the memory it
was stored into.

Three things work this way. The **pixel plane** is palette indices, one byte
per pixel; the **text plane** is a grid of character cells composited over it;
and the **palette** — the colour table both of them look through — is RGBA
bytes. Nothing bulk travels as a command any more.

## 3. Pixels

```smalltalk
pane := GamePane new.
pane openDirect: 320 height: 240.       "before drawing anything"

"...each frame:"
fb := pane screenMemory.                 "an Alien over the framebuffer"
s  := pane screenStride.
fb byteAt: (y * s) + x + 1 put: colourIndex.
pane present.
```

Colours are palette indices exactly as everywhere else, so `paletteAt:r:g:b:`
still applies and palette cycling costs nothing — the shader does the lookup.

### The two rules that are easy to get wrong

**The stride is not the width.** A buffer-backed Metal texture's `bytesPerRow`
is rounded up to the device's linear alignment, so a row occupies
`screenStride` bytes even when only `width` of them are visible. Address
`y * stride + x`. Using `y * width + x` shears the picture diagonally, and it
looks like your arithmetic is wrong when it is your addressing. On a 320-wide
pane the two happen to be equal on this machine, so a demo can be wrong and
still look right until someone picks an awkward width — which is why
`direct_pane`'s own test uses 321.

**Refetch `screenMemory` every frame.** Presenting rotates which of three
buffers you are writing. An Alien kept from last frame names the buffer the GPU
is now reading.

### Why rotation is counted, not published

This is the part that bit, and it is worth understanding before changing
anything here.

The first implementation published one pointer — "the current write buffer" —
and republished it after each present. It **tore**, badly. The rotation happens
on the host's main thread when the `Present` command is drained, but the VM
does not wait for that and cannot: a demo sends `present` and starts the next
frame immediately. So it fetched the pointer published *before* the rotation
and began writing the very buffer the GPU was about to read. Triple buffering
could not help, because both sides were pointing at the same one.
ParallelMandel showed it worst — four workers piling into the displayed buffer.

The cure is agreement rather than waiting. The host publishes **all** the
buffers once, and each side picks `frame % count` from its own count of the
same ordered presents: the VM increments when it *sends* `present`, the host
when it *renders* one. The command stream is ordered, so the two counts
describe the same frame, and neither blocks on the other.

## 4. Text

The overlay is a screen buffer too, not a chain of commands.

```smalltalk
pane textPut: 'SCORE 100' at: 2 row: 0 color: GamePane white.
pane textClearPlane.
```

A cell is four bytes — `[char, fg, bg, flags]`:

| field | meaning |
|---|---|
| `char` | ASCII. **0 means UNUSED and draws nothing** — not a black square |
| `fg`, `bg` | palette indices into the text plane's own 256-entry palette |
| `flags` | bit 0 = transparent background, so text sits over the picture |

Char 0 drawing nothing is what lets this plane exist over every pane always: a
demo that never touches it sees no difference.

Cells are 6×8 (the 5×7 glyph plus a pixel of leading each way), so a 320×240
pane is **53×30**. The plane is viewport-fixed by default, so a HUD holds still
while the picture scrolls or shakes underneath it.

### Whole pages

```smalltalk
help := TextScreen cols: pane textCols rows: pane textRows.
help box: GamePane darkGrey.
help centre: 'MACVM' row: 2 color: GamePane white.
pane textBlast: help bytes.       "one memcpy, whatever is on it"
```

Composing costs ordinary Smalltalk work, once, off the frame path. Showing
costs a copy that does not depend on how full the page is — which is what makes
a menu or a help screen instant. A wrong-sized page is refused outright rather
than applied half-way.

## 5. Colour

The palette is memory too, and it is the one that changes what is *possible*
rather than merely what is cheap.

```smalltalk
pane paletteMemoryAt: (pane paletteGlobalBase + 4) put: 16rFF00FF.
pane lineMemoryAt: y index: 1 put: 16r3080FF.   "scanline y, colour 1"
```

Entries are **RGBA bytes**, four per entry. The layout has two regions:
`height` groups of 16 **per-line** entries first — line `y`'s colour `i`
(1–15) is entry `y * 16 + i` — then 240 **global** entries, where colour `c`
(16–255) is entry `paletteGlobalBase + c - 16`.

`paletteAt:r:g:b:` and `linePaletteAt:index:rgb:` still work and write **the
same buffer**. There is one palette and no copy of it anywhere — which is not
just tidiness: a CPU mirror would mean the next upload could copy stale
colours over whatever a demo had written directly, and no amount of
coordination makes two writers to two copies safe.

**Why it matters more than it sounds.** Because the picture stores *indices*,
changing the table changes every pixel that refers to it — without touching a
pixel. Two demos live off this:

- **Copper** (`world/45f`) fills the screen once with colour 1 and then never
  writes a pixel again. Each frame it rewrites what colour 1 *means* on each of
  240 scanlines: three bars appear to ride up and down a screen that is,
  strictly, static. 960 bytes a frame.
- **Julia** (`world/45h`) stores escape counts, so cycling the palette
  recolours an entire animated fractal with no pixel read or written.

Before SM4 that cost 240 commands a frame; it now costs none.

## 6. Several VMs, one screen

Workers are `thread::spawn`ed VMs **in this process**, and the framebuffer is
published in a process-global — so a worker asking for `screenMemory` gets the
very same buffer the primary did, with nothing added to make that work.

Several of them writing **disjoint** regions is therefore just several of them
writing. `ParallelMandel` rasterises four horizontal bands in four worker VMs
straight into the screen; a band no longer crosses the worker boundary as a
pickle, is not copied into an assembly buffer, and is not blitted. Its reply
carries a row count.

Nothing synchronises the workers and nothing needs to: their bands are disjoint
by construction, and the primary presents only once every band of the round has
replied — which was already the rule that stopped a torn frame, and now also
keeps them off the buffer being displayed.

## 7. What it costs, measured

| | before | after |
|---|---|---|
| a full-screen frame | 3 full-frame copies + an allocation, 1 `Blit` command | 0 copies, 0 commands |
| a HUD of ~100 numbers | ~100 `Text` commands **per frame** | 0 commands |
| showing a help page | one command per string | 1 memcpy |
| 60 idle Minesweeper frames | up to 2 blits | **60 presents, nothing else** |
| a copper effect | 240 `linePaletteAt:` commands per frame | 960 byte stores, 0 commands |

Asserted, not described:
`a_full_screen_direct_demo_costs_exactly_one_command_per_frame` drives Plasma —
a full-screen field *and* a live frame counter, both rewritten every frame —
and demands exactly five commands for five frames.

## 8. Safety

`screenMemory` and `textMemory` hand back an **`Alien`**, not a raw pointer,
and an Alien is length-bounded (see [`ALIEN.md`](ALIEN.md)). A demo with an
off-by-one physically cannot corrupt whatever the host put after the
framebuffer. A raw pointer would lose exactly that.

One honest caveat: the refusal is **silent**. `Alien>>byteAt:put:` is
`<primitive: 113> ^self`, so an out-of-bounds write returns the receiver and
signals nothing. The bounds check protects the memory; it does not tell the
demo.

**Lifetime.** The buffers are freed only when the pane closes, and the host
retracts the publication *before* freeing them, with the writing VM stopped
first. After that `screenMemory` answers `nil` rather than a dangling view.
Getting this order wrong is not theoretical: a test that left workers running
past the end of its buffer crashed the suite with
`SIGSEGV far 0x1010101010101018` — in an unrelated test two positions later.

## 9. When not to use it

`blit:` still exists and is still right when a demo already has a `ByteArray`
it built for its own reasons — Life, Minesweeper and FreeCell all use it, and
none of them would be faster for changing. Direct screen memory is the path for
**full-frame CPU rendering**, where every pixel changes every frame and there
is nothing small to send: a plasma, a raycaster, a live fractal.

The text plane's grid snaps glyphs to 6-pixel boundaries, so it does not
replace object-attached text at arbitrary offsets — Minesweeper's digits
centred in 16-pixel squares and FreeCell's ranks at +3,+4 inside a 34-pixel
card stay pixel art in the picture. The text plane is for text *screens*.

## 10. Where the code is

| | |
|---|---|
| `src/embed.rs` | the publication (`publish_screen_buffers`, `publish_text_memory`) and `GameCommand` |
| `src/runtime/primitives.rs` | prims 268–276: `openDirect:height:`, `screenMemory`, `screenStride`, `textMemory`, `textCols`, `textRows`, `paletteMemory`, `paletteGlobalBase` |
| `src/runtime/alien.rs` | prim 274, the bulk copy |
| `MacGamePane graphics/src/direct_pane.rs` | shared buffers, linear texture views, palette shader |
| `MacGamePane graphics/src/text_plane.rs` | cell grid, font atlas, blended shader |
| `world/43_gamepane.mst` | the Smalltalk surface |
| `world/43a_textscreen.mst` | `TextScreen` |
| `MacGamePane graphics/src/indexed_pane.rs` | the palette: RGBA bytes, one buffer, no mirror |
| `world/45d_plasma.mst` | pixels: a full-screen field, one command a frame |
| `world/45e_textpages.mst` | text: pages composed once, shown by one copy |
| `world/45f_copper.mst` | colour: a screen filled once and never redrawn |
| `world/45g_attractor.mst` | density accumulated into the picture |
| `world/45h_julia.mst` | both at once — direct pixels AND palette cycling |

The pixel half is a port of MACDART's `GpDirectPane`
(`macdart/cocoa/gamepane/gp_engine.mm`, designed in its `GAMEPANE_PLAN.md`
§6b), which is where the stride rule, the buffer rotation and the
palette-in-shader trick were proven first.
