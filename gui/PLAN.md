# MACVM GUI — Strongtalk HTML-Environment Recreation Plan

Goal: recreate the **Strongtalk programming-environment GUI** — its hypertext
"live HTML" model and its exact mid-90s visual style — as the MACVM user
interface, hosted in a **native Cocoa window containing a WKWebView**, with a
native macOS **menu bar**, an in-page Strongtalk-style **toolbar**, and a
**status bar**.

All visual facts below are anchored in the Strongtalk source at
`../../strongtalk-repo` (checked directly, file:line cited). Copied artifacts
live in [`reference/`](reference/) (see [`README.md`](README.md)).

---

## 1. What the Strongtalk GUI actually is

Strongtalk's environment is **not** a set of fixed-layout windows. Every tool
is a *page*: a flow-layout hypertext document rendered by Strongtalk's own
widget framework (glyphs + Visuals: `Row`, `Column`, `Glue`, `Frame`,
`Border` — `StrongtalkSource/Visual.dlt`, `Glyph.dlt`, `Glue.dlt`,
`Border.dlt`). The HTML browser (`HTMLView`, `HTMLBuilder.dlt`) renders real
HTML files extended with two tags that make pages *live*:

- **`<a doit="Smalltalk code">…</a>`** — a link that evaluates code in the
  image instead of navigating (`ElementA.dlt`, `HTMLLink.dlt`).
- **`<smappl visual="Smalltalk code">`** — evaluates the code and embeds the
  resulting Visual (a button, a class-hierarchy outliner, a live code editor)
  inline in the page flow (`startPage.html`, tour pages). See
  [`smappl.md`](smappl.md) for the full grounding: exact parse/eval
  semantics (`ElementSMAPPL.dlt`), the complete `visual=`/`doit=` vocabulary
  used across the corpus, and why D-G5's HTML-fragment rendering has no
  upstream Strongtalk precedent to follow.

The programming tools are **outliners**: indented, bullet-list trees where
every level expands in place — a class browser shows its categories, a
category its methods, a method its source *in an embedded editor*, all in one
scrolling page (`Outliner.dlt`, `ClassOutliner`, `ClassHierarchyOutliner`,
tour `progenv2.html`–`progenv4.html`). Expand/collapse is driven by the
`closedItem.bmp` / `openItem.bmp` toggle glyphs.

A window is: title bar → pull-down menus → **toolbar** (icon buttons on a
gray backdrop) → scrolling page content → optional status/transcript area
(`Window.dlt`, `ToolBar.dlt`). The **Launcher** is the first window: a
transcript plus a toolbar of nine buttons (home, find definition,
implementors, senders, user hierarchy, full hierarchy, workspace, editor,
documentation — enumerated with icons in `documentation/tour/progenv.html`).

## 2. Visual specification (ground truth)

### 2.1 Palette — `StrongtalkSource/Paint.dlt:76-85`

| Name | RGB | Hex | Use |
|------|-----|-----|-----|
| White | 255,255,255 | `#FFFFFF` | page background, bevel highlight |
| Black | 0,0,0 | `#000000` | body text, bevel outer shadow |
| Gray | 128,128,128 | `#808080` | bevel inner shadow, secondary text |
| **BackgroundGray** | 192,192,192 | `#C0C0C0` | window/toolbar/outliner chrome |
| Blue | 0,0,255 | `#0000FF` | **links** (`HTMLBuilder.dlt:515` — `linkPaint: Paint blue`) |
| BlueGreen | 0,128,128 | `#008080` | accent (also the teal in the icon set) |
| Red / Green / Yellow | primaries | | error/status accents |

Pages render on white (`<body bgcolor="#FFFFFF">` throughout the tour);
window chrome, toolbars and outliner headers on BackgroundGray.

### 2.2 Typography

| Role | Face | Size | Weight | Source |
|------|------|------|--------|--------|
| Body text | Times New Roman | 12 pt | regular | `HTMLBuilder.dlt:360` (`defaultPainter`) |
| H1–H6 | Times New Roman | 24/18/14/12/12/10 pt | boldness 0.8 | `ElementH1.dlt:29`…`ElementH6.dlt` |
| Code / `<pre>` / editors | Lucida Console | 10 pt | regular | `ElementPRE.dlt:24` |
| Buttons / native widgets | MS Sans Serif | 8 pt | regular | `Button.dlt:311` |
| Links | inherit + underline | | blue | `ElementA.dlt`, `HTMLLink.dlt` |
| List bullets | Symbol char 183 (·) | | | list element source |

Initial page left margin: **30 px** (`HTMLBuilder.dlt` `initialLeftMargin`);
default page width 750 px (`HTMLVisual`).

macOS font mapping: Times New Roman ships with macOS — use it directly.
Lucida Console → `'Lucida Console', 'Menlo', monospace`. MS Sans Serif →
`'Tahoma', 'Geneva', sans-serif` at 11px. (Optionally bundle free
metric-compatible faces later; start with the stacks.)

### 2.3 Bevels and borders — `Border.dlt` (`standard3DRaised:`)

The signature Win95-era 2px bevel:

- **Raised**: outer top/left `#FFFFFF`, outer bottom/right `#000000`,
  inner top/left `#DFDFDF`, inner bottom/right `#808080`.
- **Lowered/inset**: the same colors mirrored.

CSS recipe (pixel-exact, no anti-aliasing):

```css
.bevel-raised  { box-shadow: inset -1px -1px 0 #000, inset 1px 1px 0 #fff,
                             inset -2px -2px 0 #808080, inset 2px 2px 0 #dfdfdf; }
.bevel-lowered { box-shadow: inset -1px -1px 0 #fff, inset 1px 1px 0 #000,
                             inset -2px -2px 0 #dfdfdf, inset 2px 2px 0 #808080; }
```

Spacing comes from `Glue` with rigid values of **2, 6, 10 px** — use those as
the spacing scale.

### 2.4 Icons

37 originals in [`reference/icons-bmp/`](reference/icons-bmp/) (16-color,
mostly 25×25; teal/gray/yellow pixel art), PNG conversions in
[`reference/icons-png/`](reference/icons-png/). Render at 1:1 or integer
scale with `image-rendering: pixelated`. Toggle glyphs: `closedItem` (small
circle/ball = collapsed), `openItem` (yellow wedge = expanded).

## 3. Architecture

```
┌─ NSWindow (native) ─────────────────────────────────────┐
│ macOS menu bar (NSMenu): File Edit Browse Filtering Meta │  ← native
├──────────────────────────────────────────────────────────┤
│ WKWebView                                                │
│  ┌ toolbar row: beveled icon buttons on #C0C0C0 ┐        │  ← in-page,
│  ├ page content: white, Times 12pt, live HTML   ┤        │    pixel-faithful
│  └ status bar: lowered-bevel strip on #C0C0C0   ┘        │
└──────────────────────────────────────────────────────────┘
            ▲ webkit.messageHandlers / evaluateJavaScript ▼
   Rust shell (raw objc_msgSend bridge): NSApplication, NSWindow, WKWebView, NSMenu
            ▲ eval / mirrors / transcript ▼
                    MACVM core (Rust)
```

Decisions (mirroring `docs/DESIGN.md` style):

- **D-G1 — WKWebView renders everything inside the window.** The Strongtalk
  look (bevels, C0C0C0 chrome, pixel icons) cannot be faked with native
  NSToolbar/NSButton, so the toolbar and status bar are HTML *inside* the web
  view, styled per §2. Only the menu bar is native (macOS owns it anyway);
  Strongtalk's per-window pull-down menus (File / Filtering / Meta …) map to
  NSMenu items populated by the active tool.
- **D-G2 — Rust owns the shell via a hand-rolled `dlopen`/`objc_msgSend`
  bridge (`src/objc.rs`), not the `objc2` crate ecosystem.** Originally
  planned as `objc2`/`objc2-app-kit`/`objc2-web-kit`; superseded once
  implementation started by mirroring the proven, dependency-free pattern
  already shipping in the sibling MacModula2 project
  (`src/newm2-runtime/src/objc.rs`). Same outcome — no Swift/Xcode target,
  GUI stays in the cargo workspace — with zero external crate dependencies
  and no exposure to an evolving crate's macro API; `objc.rs` resolves
  `dlopen`/`dlsym(RTLD_DEFAULT, …)` at startup and each message send
  transmutes `objc_msgSend` to the exact ABI shape (self/`_cmd` + typed args,
  per AAPCS64) that call site needs. (Fallback seam unchanged: the shell
  talks to the VM through a narrow `GuiHost` trait, so a Swift shell remains
  possible.)
- **D-G3 — Live-page protocol is JSON over `webkit.messageHandlers.macvm`.**
  Page → VM: `{kind:"doit", code}`, `{kind:"navigate", href}`,
  `{kind:"toggle", nodeId}`, `{kind:"toolbar", button}`. VM → page:
  `evaluateJavaScript` calls that insert rendered fragments (smappl results,
  expanded outliner nodes, transcript appends).
- **D-G4 — Strongtalk HTML extensions are preprocessed, not hacked into the
  DOM.** A small Rust translator rewrites `<a doit=…>` →
  `<a class="doit" data-code=…>` and `<smappl visual=…>` → placeholder
  `<span class="smappl" data-code=…>` before the page is loaded; a JS runtime
  (`smtk.js`) wires the handlers. This keeps the original `.html` files
  byte-identical as a test corpus.
- **D-G5 — Outliners are server-rendered HTML fragments.** The VM (via its
  mirror/reflection layer) renders each outliner node to an HTML `<div>` when
  expanded; the web view only handles toggling and editing. This matches
  Strongtalk (pages are built image-side) and avoids duplicating reflection
  in JS.

New crate: `gui/` becomes a cargo member (`macvm-gui`), with:

```
gui/
  PLAN.md, README.md, smappl.md  (this plan; folder map; the smappl deep-dive)
  reference/pages/            (copied Strongtalk artifacts — startPage.html,
                                tour/, documentation/ — kept byte-identical,
                                per D-G4)
  reference/pages/macvm-help/  (MACVM's own authored help viewer — not
                                copied Strongtalk content; deliberately a
                                separate path from documentation/, since
                                that path is the *real* Strongtalk docs
                                corpus startPage.html's own "Browse
                                Documentation" link and the toolbar's
                                Documentation button both point at —
                                reachable instead from the native Help menu)
  reference/icons-bmp/, icons-png/  (copied Strongtalk icon art)
  assets/                     (strongtalk.css, hidef.css, smtk.js,
                                icons-hidef/ shipped at runtime)
  src/                        (shell: app, window, webview, menus, bridge,
                                vm_host: the GUI↔VM worker-thread scaffold)
```

## 4. Phases

Dependencies on the core VM are explicit; G0–G1 need **no** VM at all, so GUI
work can proceed while core sprints (S0+) continue. Each phase ends green
with a demo gate.

### G0 — Faithful static shell `M`
- `strongtalk.css`: the complete §2 token set (palette, type scale, bevels,
  spacing, pixelated icons) + retro scrollbar styling (`::-webkit-scrollbar`
  with beveled thumb on `#C0C0C0`).
- Rust/objc2 app: NSWindow + WKWebView; native menu bar with File/Edit/Help;
  loads local pages; in-page toolbar (the nine Launcher buttons, §1) and
  status bar strip (shows link target on hover, "Ready" otherwise).
- Renders `reference/pages/startPage.html` and the whole tour through the
  D-G4 preprocessor — doits/smappls appear as correctly-styled but inert
  elements (smappl placeholder = lowered-bevel box naming its code).
- **Gate:** side-by-side eyeball vs the icon art and tour markup: start page
  and `progenv*.html` render in the period style; back/forward/home
  navigation works; status bar live.
- **Status: done.** Verified by actually running the shell (screenshots +
  accessibility-tree-driven clicks), not just code review — see the
  WKWebView note under §5 Risks for two real bugs this caught.

### G1 — Live-page runtime, stub VM `S`
- `smtk.js`: doit clicks, toolbar clicks, outliner toggles → JSON messages
  (D-G3); fragment-insertion entry points for VM → page.
- `GuiHost` trait in the shell; a stub host that logs doits to a transcript
  pane and answers canned smappl renders.
- Launcher window: transcript (Lucida Console 10pt, white, lowered bevel) +
  toolbar; "doit → transcript echo" round trip.
- **Gate:** clicking `Collect Garbage` on the start page produces a
  transcript entry via the full JS↔Rust round trip.
- **Status: done** for the stub host (no VM yet, so doits echo their source
  text rather than evaluate — real evaluation is G2). Also went slightly
  beyond the stub scope: `reference/pages/documentation/` is now the *real*
  Strongtalk documentation corpus (copied byte-identical, including
  `keyboard.html`/`internal/`/`mixins/`/`type-system/`), giving "Browse
  Documentation" and the toolbar's Documentation button somewhere real and
  faithful to go; a separately-authored MACVM help viewer
  (`reference/pages/macvm-help/`) covers the shell itself (doit/smappl/
  Theme menu explanations) and is reachable from the native Help menu, so
  it doesn't collide with the real corpus's own `documentation/index.html`.
- **The `../docs/SPEC.md` §16.1 threading model (decision A18) is built,
  not just pinned on paper**: `src/vm_host.rs` spawns a genuine
  `std::thread::spawn` worker; doits cross a real channel to it and back
  (`main.rs` no longer runs the stub inline). Delivery back to the main
  thread uses `-[NSObject performSelectorOnMainThread:withObject:
  waitUntilDone:]` (`objc.rs`), the one NSObject entry point documented as
  safe to call from any thread — chosen specifically because this bridge
  has no GCD/block-literal ABI to lean on otherwise. Verified against
  `cocoa_data`'s `cocoa.sqlite` runtime-method index (an earlier version
  sent the wrong selector, `performSelector:withObject:waitUntilDone:`
  — nonexistent — which triggered `-doesNotRecognizeSelector:` and
  crashed the process with "Rust cannot catch foreign exceptions"; the
  real selector's encoding was confirmed there before re-verifying live).
  Also pinned for when there's something real to route: discrete events
  (transcript lines, doit results) use the ordered channel; continuous/bulk
  updates (a redrawing pixel buffer, a live status string) should use
  `vm_host::LatestSlot<T>` instead — newest-value-wins rather than queuing
  stale frames behind a slow UI thread — and a future per-widget click
  (once smappl builds real closures, `../smappl.md` §4.1) should route by
  opaque id through the request channel to a handler table owned solely by
  the worker thread, needing no additional lock beyond the channel itself.

### G2 — VM bridge `M` — *needs core eval (interpreter sprints) + a
reflection surface*
- `GuiHost` backed by MACVM: doits evaluate in the VM; results/errors to the
  transcript; `Transcript show:` from the world reaches the Launcher.
- smappl evaluation for the button/image/glue subset used by the start page
  and tour (render as HTML fragments, D-G5).
- Native menus populated per-tool; Meta menu opens an inspector page on the
  tool's model (Strongtalk's "Meta" convention, tour `ui.html`).
- **Gate:** the *original* `startPage.html` is fully live: every doit and
  smappl on it works against MACVM.

### G3 — Outliner tools `L` — *needs mirrors on the class world*
- Outliner component: indented tree, `closedItem`/`openItem` toggles,
  BackgroundGray headers, raised-bevel embedded frames
  (`Border standard3DRaised:` look), lazy node expansion via D-G5.
- Class Hierarchy Outliner (with comment-substring filtering — "Filter: %HTML
  with subclasses" header line, superclasses in lighter face) and Class
  Outliner (header, categories, method signatures with type annotations
  shown as written).
- Right-click context menus per node (HTML menu styled as beveled popup, or
  NSMenu — decide by feel in G3).
- **Gate:** browse `Object` hierarchy → open a class → expand a category →
  read a method, all in one page, all lazily fetched from the VM.

### G4 — Code editing & find tools `L`
- Embedded code editor in method nodes (expand method → editable source,
  accept/typecheck-later hooks; syntax painters per `ProgrammingEnvironment`
  colors — comments gray, errors red-backed).
- Workspace and CodeEditor tools; find definition / implementors / senders
  toolbar buttons wired to VM queries (wildcards allowed, per tour).
- **Gate:** the tour's "little project" workflow (progenv4: add a method via
  the browser, accept it, run it) works end to end in MACVM.

### G5 — Polish & parity sweep `S`
- Text-scale buttons (`biggerText`/`smallerText`), refresh, clone-window;
  Core-Sampler-style DOM/visual inspector as a debug page; keyboard chords
  from `documentation/keyboard.html`.
- Parity checklist pass over every toolbar icon in `reference/icons-png/` —
  each either wired or consciously deferred.

## 5. Risks / open questions

- **Font metrics:** Times New Roman at 12pt CSS ≠ 12pt GDI; tune with
  `font-size` in px against the 750px page width until line breaks in the
  tour pages feel right (no pixel-perfect oracle exists — judgement call).
- **smappl generality:** arbitrary Smalltalk in `visual=` can build any
  Visual. G2 deliberately supports only the vocabulary the shipped pages use
  — `Button withImage:action:`/`labeled:action:`, `Glue`, the
  `ClassHierarchyOutliner`/`ClassOutliner` filter-chain shapes, `CodeView`
  — anything else renders as the G0 placeholder box. Revisit after G3. Full
  vocabulary catalog, exact citations, and evaluation semantics:
  [`smappl.md`](smappl.md).
- **WKWebView sandboxing:** local page + icon loading needs
  `loadFileURL:allowingReadAccessToURL:` on the `gui/` root; doit bridge
  must be registered per-navigation. Confirmed the hard way in G0:
  `loadHTMLString:baseURL:` does **not** grant `file://` subresource access
  outside `baseURL`'s own directory — CSS/JS/icons all silently failed to
  load, and what rendered looked deceptively like Strongtalk styling because
  it was actually just WebKit's default stylesheet. Fixed by writing the
  preprocessed page to `gui/.rendered/current.html` and loading *that* via
  `loadFileURL:allowingReadAccessToURL:<gui root>`, plus injecting
  `<base href="file://{original page dir}/">` so the *page's own* relative
  links still resolve against its real location rather than `.rendered/`.
  Also hit: no page declares `<meta charset>`, which the original corpus
  never needed (ASCII-only) but MACVM's own authored HTML does (em dashes,
  arrows) — fixed by injecting `<meta charset="utf-8">` into the same
  shared chrome so every loaded page gets it.
- **VM dependency pacing:** G2+ gates assume interpreter + mirrors exist; if
  the GUI outruns the core, extend the G1 stub host (canned class data) so
  G3's outliner UI can be built and tested against fixtures.

## 6. Integration with the core plan (added on adoption into SPRINTS.md)

This track is **Phase G** in [`../docs/SPRINTS.md`](../docs/SPRINTS.md); the
core-side contract it builds against is [`../docs/SPEC.md`](../docs/SPEC.md)
**§16** (VmHandle embedding API, TranscriptSink, mirrors, method-source
registry, workspace + threading model — amendments A16–A18).

| GUI phase | Core dependency | Notes |
|---|---|---|
| G0, G1 | none | can start immediately, parallel to core Phases A–B |
| G2 | S5 + S6 + SPEC §16.1–16.2 | doits via `VmHandle::eval`; `Transcript show:` via `TranscriptSink` |
| G3 | S6 + Phase **W2/W3** (Smalltalk mirrors + ToolNode + HtmlWriter — `docs/APPS.md`) | outliner nodes are Smalltalk `ToolNode`s rendering fragments image-side (D-G5, as Strongtalk did) |
| G4 | Phase **W4** (accept path) + source registry (SPEC §16.4 / A16) | **JIT-on redefinition needs S13** — until then run `MACVM_JIT=off` |
| G5 | — | |

Crate wiring (SPEC A18): the repo root is a cargo workspace; this directory
is member `macvm-gui`, depending on the `macvm` lib. AppKit owns the main
thread; the VM runs on a worker thread; all traffic crosses the §16.1 channel
bridge (which is also the D-G3 message protocol's Rust end).
