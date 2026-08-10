# The macVM UI — a visual reference

Screenshots of the shipping macOS app (`cocoa_gui/`), captured 2026-08-10 from
the signed build, at 1400px wide. Every image is the real window, not a mockup.

**Why this exists:** a Windows port needs to know what it is porting. The
Cocoa app is the reference implementation of the environment, and a
description in prose ("there is a browser tab") loses the layout, the pane
proportions, the toolbar order and the affordances that make it usable. Read
the picture, then the notes under it for what the port has to reproduce.

Captured through the control channel (`MACVM_COCOA_CTL=7644`, verb
`snap <path>`), which is also how a port could produce its own comparison
shots.

---

## The window

One window, a toolbar of view switchers, and a Transcript docked at the bottom
that every view shares. The metrics cluster at the toolbar's right edge is
live VM telemetry (memory, JIT compiles, code size, allocation rate, GC
counts) and is always visible, whichever view is showing.

Toolbar order is deliberate: Workspace, Browser, Outliner, Find, Editor,
Canvas, Docs, Debugger, Monitor — the class library first, tools after.

| | |
|---|---|
| ![Workspace](01-view-workspace.png) | **Workspace** — the scratchpad. Type an expression, **Cmd-D** runs it (Do It), **Cmd-P** prints the result inline (Print It). Syntax colouring is live. This is where nearly every session starts. |
| ![Browser](02-view-browser2.png) | **Browser** — the classic Smalltalk-80 four-pane category browser: categories, classes, protocols, methods, with a source pane below. Fully writable: accept a method and it compiles into the running world *and* persists to the image. |
| ![Outliner](03-view-outliner.png) | **Outliner** — the Strongtalk-style unfolding tree over live class reflection, a tribute to Strongtalk's own browser. Sits beside the Browser because they are two views of the same thing: one queries the image, the other reflects on live classes. |
| ![Find](04-view-find.png) | **Find** — search selectors, senders, implementors and source across the whole world. Selecting a hit reveals it in the Browser. |
| ![Editor](05-view-editor.png) | **Editor** — a plain class-text editor with live recompile on save, plus **File In** (restart the world clean, then load your file) and **Add to World** (graduate a file into `world/` permanently). |
| ![Canvas](06-view-canvas.png) | **Canvas** — an RGBA drawing surface driven from Smalltalk, either as a pixel buffer or as a list of drawing commands. The benchmark chart is drawn here by ordinary world code. |
| ![Docs](07-view-help.png) | **Docs** — the documentation browser: topic sidebar, find-in-page, styled markdown, and a ▶ button beside every code example that runs it. Its content lives in the world as Smalltalk methods, so the system documents itself. |
| ![Debugger](08-view-debugger.png) | **Debugger** — the native face of the VM's halt loop: stack, source, frame, with Step Into / Over / Finish / Continue / Abort. Fronts itself automatically when the primary halts. |
| ![Monitor](09-view-monitor.png) | **Monitor** — one row per running VM (primary, UI worker, and any spawned workers) with memory, heap, GC, allocation rate, nmethods, compiles, deopts and IC counts, plus the UI bridge's own pulse. |

---

## Demos

Launched from the **Demos** menu, which starts each one *top-level on the
primary VM* — a frame loop has to own its activation, so it cannot be started
from a Workspace doit. Each opens its own 320×240 Metal window; **Escape**
ends it. One runs at a time.

| | |
|---|---|
| ![Breakout](10-demo-breakout.png) | **Breakout** — the reference game. Paddle on the arrow keys, brick collision, lives, score, sound. One readable class; the best first read for anyone learning the engine. |
| ![MandelZoom](11-demo-mandelzoom.png) | **MandelZoom** — per-pixel Mandelbrot with a zoom. Pure compute, and the clearest demonstration of the JIT at full stretch. |
| ![Worms](12-demo-worms.png) | **Worms** — sprite handling and swarm movement. |
| ![Galaxigans](13-demo-galaxigans.png) | **Galaxigans** — a full arcade game: sprite sheets, waves, score, HUD, attract mode. |
| ![ParallelMandel](14-demo-parallelmandel.png) | **Parallel Mandelbrot** — the same fractal computed across several worker VMs at once, with the bands composited into one frame. The demo that exercises the multi-VM machinery. |

---

## Tools

Under the **Tools** menu, each in its own window.

| | |
|---|---|
| ![Sprite Editor](15-tool-spriteeditor.png) | **Sprite Editor** — draw GamePane sprites as indexed-colour hex rows, animate frames, and save them out as sheet classes. Has a live preview that launches the sprite in a real game window. |
| ![Sound Editor](16-tool-soundeditor.png) | **Sound Editor** — the ADSR instrument panel over the parametric synthesizer, for designing sound effects beyond the three built-in arcade presets. |

---

## Notes for a port

- **The Transcript is shared, not per-view.** It docks at the bottom and
  survives view switches, with a remembered collapse state.
- **Views build lazily** on first visit, and that is load-bearing: on a fresh
  launch the app opens on Docs, so the Workspace does not exist yet. Anything
  that pokes a view's contents must switch to it first.
- **Two VMs.** The primary runs the user's code; the interface VM owns the
  window and everything AppKit. The Monitor shot shows both. A port needs the
  same split, or an equivalent story for why it does not.
- **The metrics cluster is always live** — it reads the VM directly, not a
  cached snapshot, and it is a large part of why the environment feels alive.
- **Game windows are separate top-level windows**, not panes, and belong to a
  session the host opens on launch.
