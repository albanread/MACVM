# The macVM UI — a visual reference

Screenshots of the shipping macOS app (`cocoa_gui/`), captured from the signed
build at 1400px wide. Every image is the real window, not a mockup.

Most shots date from 2026-08-10; the **Docs** and **Monitor** views and the
whole **Apps** section were recaptured 2026-08-15 from the notarized
[v2026.08.15](https://github.com/albanread/MACVM/releases/latest) build, which
is where this environment changed most.

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
| ![Docs](07-view-help.png) | **Docs** — the manual, and the system documents itself: 22 articles in 5 sections, indexed by an expanding tree (sections open on build, so nothing is hidden behind a disclosure triangle on first sight). Find-in-page, styled markdown, and a ▶ button beside every runnable code example. Content lives in the world as Smalltalk methods — `world/71_cocoahelp.mst` is the whole manual. |
| ![Debugger](08-view-debugger.png) | **Debugger** — the native face of the VM's halt loop: stack, source, frame, with Step Into / Over / Finish / Continue / Abort. Fronts itself automatically when the primary halts. |
| ![Monitor](09-view-monitor.png) | **Monitor** — one row per running VM with memory, heap, GC, allocation rate, nmethods, compiles, deopts and IC counts, plus the UI bridge's own pulse. The shot has three: the **primary**, the **ui** worker, and **worker 3** — an app from the Tools menu running in a VM of its own. A VM that ends stays on the roster for a minute, marked, so a death is a fact you can see rather than a row that vanished; right-click **Send exit** asks one to leave. |

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

## Apps — windows made of data

A second way to build a window, added since the first capture: **AppSpec**, a
declarative layer where a window *is* data. Your code holds state and answers a
**spec** (a tree of typed nodes); the differ compares it to the last one and
patches only what changed; you never touch a control. Handlers are not in the
tree — a control carries an event *name* — which is what lets a whole window
travel between VMs.

Where they run is worth being exact about, because the framework is the same
either way. The first two are built **live in the interface VM** by a single
runnable block in the Docs tab — that is what a ▶ button on a `ui` example
does. The third was opened from the **Tools** menu, which spawns a **VM of its
own** for it (Debug ▸ Launch in own VM, on by default) — it is `worker 3` in
the Monitor shot above. A tool does not know or care which side of that
boundary it is on: same `spec`, same `handle:value:`.

| | |
|---|---|
| ![The Scale of Everything](17-app-scale.png) | **The Scale of Everything** — the *Build an App* guide's worked example: twelve landmarks from absolute zero to the surface of the sun, C/F/K readouts, and a colour bar that is *painted*, not composed. The controls are spec nodes the differ patches; the bar is an `rgbaPane` a paint block fills pixel by pixel — the two halves of the framework in one small window. |
| ![Weather](18-app-weather.png) | **A little weather app** — the *A Little Weather App* guide, entire: resolve a host, open a socket, speak plain HTTP, put the answer in a window. Real forecast, fetched live. The whole client is visible Smalltalk; there is no library between the program and the wire. |
| ![Declarative Demo](19-app-declarative.png) | **Declarative Demo** (Tools ▸ Declarative Demo) — the reference AppSpec tool, showing every control kind and its own reconciliation counters. It is `WinDemoTool`, the Windows port's file running here **byte-unedited**: the spec layer is the shared contract, and only the realizer underneath is per-platform. |

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
- **Apps and demos run in VMs of their own**, one per window, each with its own
  heap, JIT and OS thread. The window belongs to the interface VM; the model,
  the render block and the handler live in the app's VM, and the two talk
  directly. Closing the window *asks* the app to end — it tears down, runs its
  `ensure:` blocks and exits, reporting `exited (#closed)` rather than dying.
- **The declarative layer is the portable one.** `world/87_appspec.mst` (the
  spec, the differ) and `world/88_appspec_layout.mst` (the layout arithmetic)
  carry no OS at all and are meant to stay byte-identical across ports; the
  realizer (`world/92_cocoarealize.mst` here) is the only platform half. A tool
  written against it moves without edits — which the Declarative Demo above,
  and both AppSpec asset editors, already prove in both directions.
