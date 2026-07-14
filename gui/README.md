# MACVM GUI

The MACVM graphical environment: a recreation of the **Strongtalk
HTML-like GUI** — the hypertext programming environment of the 1996
Animorphic/Strongtalk system — rendered in a native Cocoa window via
`WKWebView`, with a native menu bar, toolbar, and status bar.

The plan of record is [`PLAN.md`](PLAN.md). Reference material copied from
`../../strongtalk-repo` lives under [`reference/`](reference/):

| Path | Contents |
|------|----------|
| `reference/icons-bmp/` | The original 37 toolbar/outliner icons (`resources/*.bmp`, 16-color, mostly 25×25) — ground truth for the look |
| `reference/icons-png/` | Same icons converted to PNG (usable directly by WKWebView / Cocoa) |
| `reference/pages/startPage.html` | The Strongtalk start page — canonical example of the `<smappl visual="...">` and `<a doit="...">` HTML extensions |
| `reference/pages/tour/` | The full Strongtalk tour — written *for* the Strongtalk HTML browser, so it doubles as a rendering/behavior test corpus (live embedded outliners, doit links, toolbar documentation) |
| `assets/` | Processed assets that ship with the MACVM GUI (populated during implementation) |

MACVM's **own** in-app documentation — written for this system, not copied
from Strongtalk — lives under
[`reference/pages/macvm-help/`](reference/pages/macvm-help/): a
[MACVM tour](reference/pages/macvm-help/tour/toc.html) modelled on the
Strongtalk tour above but describing MACVM's real design and measured
results, plus GUI-shell/architecture/toolbar help. It is reached from the
start page's "Browse Documentation" link, and its `doit` links execute in the
embedded VM for real.

Two HTML extensions define the "live page" model (see any page in
`reference/pages/`):

- `<a doit="Smalltalk code">…</a>` — a link that *executes* code in the VM
  instead of navigating.
- `<smappl visual="Smalltalk code">` — embeds a live widget (button, class
  outliner, code editor) computed by evaluating the code, inline in the page
  flow.

Everything else — outliner browsers with open/close toggles
(`openItem.bmp`/`closedItem.bmp`), 3D-raised borders, launcher toolbar —
is documented with source anchors in `PLAN.md`.

## Native game pane

Beyond the WKWebView, the window can host a native **Metal game pane**
(`src/game_pane.rs`) driven entirely from Smalltalk via the
[MacGamePane](https://github.com/albanread/MacGamePane) engine — an 8-bit indexed
drawing surface, retained GPU sprites, a 60 fps frame loop with keyboard input,
and sound + ABC music. The **Demos** menu launches `Breakout`, the zooming
`Mandelbrot`, the same dive in a *spawned second VM*, and the multi-VM
showpiece — `ParallelMandel`, every frame computed in bands by **4 worker VMs**
(docs/multi-smalltalk-worker.md); Escape closes the pane back to the browser. It boots the real embedded VM
from the SQLite world image (not the mock). See
[`../docs/gamepane_design.md`](../docs/gamepane_design.md) for the architecture
and [`../docs/managingtheworld.md`](../docs/managingtheworld.md) for the
world/image reseed workflow the GUI depends on.
