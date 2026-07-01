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
