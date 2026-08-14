# AppSpec — the declarative app framework

Decision of record, 2026-08-14. The direction, in the author's words: *"we keep
our native cocoa ui, and use the declarative ui for tools, as the app
portability layer, we dont prevent the mac user from writing an NSWindow. we
provide a friendly standard way for them to create reactive apps, the memory
management etc should be taken care of by the realizer, and just like the metal
game panes, it may even be more efficient, part of the VMs library of powerful
features smalltalk drives."*

This is **not** a GUI migration. It is a new VM library feature — a reactive-app
framework that sits beside GamePane, Canvas, Accelerate, sockets and FFI in the
library of powerful things Smalltalk drives.

## 1. What this is, and what it is not

| | |
|---|---|
| **IS** | a standard, friendly way to write reactive apps and tools; the app *portability layer* between MACVM and WINARMVM |
| **IS NOT** | a replacement for the native Cocoa IDE, or a wall around AppKit |

The native Cocoa IDE **stays exactly as it is** — ClassBrowser v3, the step
debugger, the Monitor tab, the 13 themes, the NSToolbar shell. Nothing in this
document touches them. This is the same split WINARMVM already runs: its shell
(browser, editor panes, monitor, debugger — the WG4–WG13 arc) is native Win32 and
DirectWrite, and the declarative layer (`world/122`–`128`) is only the
**tools/apps** surface reached through the Tools menu.

**Writing a raw NSWindow remains completely available.** A user may write a fully
declarative app, a mostly-declarative app with one hand-built NSView escape hatch
(the `foreignView` leaf, §5.6), or ignore this framework entirely and write Cocoa
by hand. Three tiers, nothing forbidden.

## 2. Why — the evidence

WINARMVM built this first (branch `wg3-subfloor-fix`, `docs/win_declarative_ui.md`),
and the source was read in full before this was written. Two measurements decided
it:

**The seam is real.** `world/122_winui_spec.mst` (688 lines — the builder,
`normalize`, `validate`, `diff:against:`) and `world/123_winui_layout.mst` (411
lines — the whole layout arithmetic) contain **~0 OS or primitive references**.
They are platform-neutral by construction. The only Windows thing about them is
the class *names*.

**A real tool leaks nothing.** `world/126_winui_spriteed.mst` — the complete sprite
editor, 699 lines — contains **zero** Win32 references: no HWND, no
`CreateWindowExW`, no `WinApi`, no `WM_`. Its entire vocabulary is `WinSpec`
constructors, `rgbaPane` nodes, `invalidatePane:` calls, and the shared `SpriteDoc`
model that already ships on every platform. Compare the Mac's hand-rolled
equivalent, `world/76_spriteed.mst`, at 906 lines of NSWindow prologue, control
factories and hand-stepped absolute floats.

That is the whole thesis: **the tool is portable; the platform lives entirely in
the realizer.**

The Mac's own costs, surveyed (`64_cocoaui.mst`, `76_spriteed.mst`, `77_sounded.mst`):
each editor hand-rolls its own `NSWindow` prologue (`CocoaSpriteEditor class >>
open` builds `Cocoa classNamed: 'NSWindow'` itself), duplicates the control
factories, is torn down by hard-coded name, owns no timer (it rides the shell's
beat), and **silently misses the font and theme broadcasts**. There is no
tool-window abstraction at all. Six files each roll their own window.

## 3. The decision

**Adopt wingui's spec+bind CONTRACT, vendored from WINARMVM's pure half. Implement
the realizer natively for Cocoa. Run each app in its own worker VM.**

The contract is a tree of typed nodes with ids, props and children; handlers are
never in the tree, only event *names*; and the programming model is **republish the
whole spec**, made affordable by an id-based differ that emits patch ops and is
allowed to answer `nil` for "I cannot express this" — whereupon the caller
republishes the window. That escape hatch is what keeps a differ small enough to
stay right.

### 3.1 Node identity is `(type, id)`

Same identity with changed props patches in place; a different identity rebuilds
that subtree. A child list reconciles by id **only when every sibling on both sides
is named**, and declines otherwise. **Every node gets an explicit id in house
style** — auto-ids exist (`__auto__:<path>`) but are positional, so anything whose
sibling order can change must be named. This is the single highest-leverage rule in
the design.

### 3.2 Each app runs in its own worker VM

The architecture the author chose, and the one that uses the hardware: *"I do like
apps running in their own VMs and it scales, at least the logic in the app runs in
its own thread; we have so many cores on systems now."*

- An app's **logic and its diff** run in its own worker VM, off the main thread, on
  its own core. The diff is deliberately kept in Smalltalk (it is the portable
  contract) — which means the expensive half fans out across cores.
- The **realizer is a main-thread UI feature**. It receives streams of patch ops and
  applies them to AppKit. It is a thin, shared **display server** for many app VMs,
  none of which can block each other.
- The serial part (AppKit mutation) stays minimal and stays where AppKit requires
  it; everything expensive is parallel.
- **Crash isolation falls out free.** Because an app is a worker VM, it inherits
  `ErrorPolicy::{Resume,Die}` and the restart supervisor: one app faulting cannot
  take down the IDE or its neighbours. The realizer holds the last frame and
  re-attaches when the supervisor respawns it.

This generalizes the existing rule (`docs/cocoa_gui_design.md`: UI worker on MAIN,
primary VM off-main) from two VMs to N.

### 3.3 The realizer owns all native lifetime

This is the "memory management taken care of by the realizer" promise, and it is
the same relationship the GamePane's Rust side has with its Metal texture: the VM
owns the native resource, Smalltalk drives it.

Under this framework the app author holds **a handle, a model, and two blocks**
(render, handle). They never allocate, retain, wire, or free a native object. The
realizer owns the whole AppKit graph — the window, the view tree, the per-control
action delegates (which must be retained or they are collected; see
`70_cocoacanvas.mst`'s `ActionDelegates` list), and the teardown order.

The point is not "less code". It is that **you cannot leak or mis-teardown a window
you never held.**

### 3.4 It may well be more efficient

Not a hope — three concrete mechanisms:

1. **Coalesced reconcile.** Event depth is counted; a `rerender` requested inside a
   handler only raises a flag; one diff and one screen update run when depth returns
   to zero. N state changes in a handler cost one AppKit layout pass. Hand-written
   UI code typically relayouts per setter.
2. **Off-main logic.** App logic and diffing run in the app's VM; only patch ops
   cross to the main thread — one bridge batch per update, not one crossing per
   control.
3. **Damage-driven panes.** A handler that changes what a pane shows says so
   (`invalidatePane:rect:`); painting takes the damage. WINARMVM measured a pencil
   stroke going from 186,624 pixel stores to 729 — and it fixed a real behavioural
   bug, because the full repaint had been holding the VM long enough that clicks
   were declined.

It will not beat a single hand-tuned window. It beats typical hand-wired UI, and it
deletes a class of retain/teardown bugs.

## 4. The acceptance test

Stated by the author and adopted verbatim as the definition of done:

> *"Once deployed the sound and sprite editor should just port over, no effort at
> all, that's the point."*

WINARMVM's `126_winui_spriteed.mst` and `127_winui_soundtool.mst` must load on the
Mac **unchanged** and work. Since those files contain no Win32 at all, this reduces
to two obligations on our side:

1. the class names they reference resolve (§5.5, the compatibility shims), and
2. `CocoaRealize` implements the service surface they use — the three pane types,
   `invalidatePane:`/`invalidatePane:rect:`, and sound audition.

Both are the realizer's job, and both are built on substrate MACVM already has
(Canvas and GamePane are the pane analogs; prim 263 is the audition wire).

## 5. The pieces

### 5.1 `AppSpec` + `AppNode` — the pure model (portable, vendored)

Builder, `normalize`, `validate`, `diff:against:`, `asJson:`. No OS, no primitives,
no window. Headless-testable end to end. **Vendored byte-identical from WINARMVM's
`122_winui_spec.mst` modulo the rename** — this file is the contract and must not
drift.

### 5.2 `AppSpecLayout` — the pure arithmetic (portable, vendored)

Two passes, measure then place. Metrics (`{charWidth. lineHeight}`) are **passed
in**, never measured here, so the geometry is checkable to the pixel with no
window. `stack` flows down; `row` gives natural widths with proportional shrink and
a 72px floor, **plus weights** so a landscape column split is sayable; `grid` is
uniform columns; `card` has fixed padding 12 / gap 10 and reserves 26px for a
title. There is **no stretch-to-fill** — it surprises everyone once and is
load-bearing for how the row shrink reads.

### 5.3 `AppToolWindow` — the portable lifecycle half

WINARMVM's `WinToolWindow` mixes portable logic with Win32 realization. **We split
it**, which is a genuine improvement over the source and is what makes the
tool-facing API identical on both platforms:

- **portable**: the registry (`register:title:width:height:render:handler:`,
  `named:`, `open:`), event-depth counting and rerender coalescing, the
  suppress-events guard, the patch/rebuild counters, damage bookkeeping and rect
  union, and the `rerender → currentSpec → validate → diff → ops` pipeline.
- **platform**: create a control per rect, apply an op, route an event, own the
  native lifetime. That is `CocoaRealize`.

Lifecycle owned once, which is the piece the Mac never had: `open` fronts-or-builds;
`#closeRequested` hides (never dies — the Mac's own choice, kept, and bound by the
framework so no tool can forget it); teardown in delegate-detach order; the shell's
beat ticks every open window; **theme, font and environment-restart broadcasts reach
every registered window.**

### 5.4 `CocoaRealize` — the platform half

Spec → live AppKit tree. Builds on what already exists:

| the contract needs | MACVM already has |
|---|---|
| control realizer | `Cocoa classNamed: 'NSButton'` … `onMain alloc/initWithFrame:` |
| per-control event binding | `CocoaToolbarAction on: aBlock` + `MacvmDelegate actionTargetOn:` (a block per control — exactly the shape a realizer wants) |
| `rgbaPane` | the Canvas RGBA surface |
| `indexedPane` / `textGrid` | GamePane's indexed plane + cell grid |
| event transport | the C4/C6 bridge callbacks, main-thread drain |
| theme / font / DPI | the shell's own, already broadcast |

**The Y-flip is solved once, in `createFor:`.** Layout answers top-left rects; an
unflipped `NSView` is bottom-left.

*Corrected after surveying the bridge, and recorded rather than quietly changed.*
This section first specified a **flipped container view** (`isFlipped` → true). The
bridge cannot mint one: `MacvmDelegate` registers a fixed family of per-role ObjC
classes (`#window`, `#text`, `#table`, `#outline`, `#mouseview`, `#action`,
`#toolbar` — `world/65_cocoadelegate.mst`), and none of them is a flipped view, nor
can the world define an ObjC subclass overriding `isFlipped`. So the realizer flips
in the one place that converts a placement into a rect:
`y_cocoa = clientHeight - y_top - h`.

This is what the codebase already does everywhere else (`cocoa_gui/src/canvas.rs:324`
flips every y for the same reason; `76_spriteed.mst:179` flips its sprite row), so it
is house practice rather than a workaround. What matters for portability is unchanged:
the flip is confined to the realizer, and **the vendored layout still needs no
Mac-specific edit.**

### 5.5 Naming, and the compatibility shims

Canonical names are **`AppSpec`, `AppNode`, `AppSpecLayout`, `AppToolWindow`** —
neutral, because this is the shared contract and not a Windows artifact.

But the acceptance test (§4) requires WINARMVM's *unmodified* tools, which say
`WinSpec` and `WinToolWindow`, to load. So we ship **thin subclass shims**:

```smalltalk
AppSpec subclass: WinSpec [ ]        "class-side methods inherit"
AppNode subclass: WinNode [ ]
```

The alias must be the **subclass**, never the superclass: every internal type check
in the vendored file reads `isKindOf: AppNode`, and an instance of a subclass
answers true, while the reverse would not. Verified against the vendored source
before adopting.

The shims are transitional. The better end state is renaming in WINARMVM too, after
which they can be deleted; nothing here depends on that happening.

### 5.6 `foreignView` — the escape hatch

A leaf node carrying a user-built `NSView`, embedded by the realizer exactly as
`rgbaPane` embeds custom-drawn pixels. It is what makes "we don't prevent the Mac
user from writing an NSWindow" a first-class design rule rather than mere
coexistence: a mostly-declarative app can drop to raw AppKit for one pane without
abandoning the framework.

## 6. Deliberate departures, recorded so they are choices

1. **No JSON on the hot path.** Specs are Smalltalk data end to end; `asJson:` is an
   emitter for tests and interop. The contract is the SHAPE, not the text.
2. **Keyword messages, not positional constructors.** Inherited from the source, and
   its reasoning is empirical: winscheme's own shipped demo calls three positional
   constructors with the wrong arity and cannot run.
3. **`AppToolWindow` is split portable/platform** (§5.3) where WINARMVM's is one
   class. Ours is the later design with two realizers in view.
4. **A flipped host view**, not flipped arithmetic (§5.4).
5. **The pure files are vendored, not reimplemented**, and should stay
   byte-identical modulo the rename. A gate asserting that is worth adding once both
   repos settle on `AppSpec`.

   **One annotation diverges, and it is a correction rather than a port artifact.**
   `AppSpecLayout class >> clamp:lo:hi:` declared `^ <Integer>` upstream, but its
   body is `(v max: lo) min: hi` and `Magnitude>>max:`/`min:` answer `<Magnitude>`
   (`world/05_magnitude.mst:24`) — so the declaration claimed more than the body
   supported. MACVM's optional-type checker caught it the moment the file entered
   the world (`it_typecheck::real_world_has_zero_type_findings`), which is exactly
   what that gate is for. Widened to `<Magnitude>` rather than worked around: every
   caller stores the result as a pixel count and none does Integer-only arithmetic
   on it. Worth carrying back to WINARMVM, whose own suite never ran this check on
   its winui files — they were platform-gated to `#windows`.

   **A second divergence, found by A2 and also a real bug.** `measureControl:` had
   no `#divider` case. `measure:` answers a divider's full width and returns before
   reaching it, but `place:` records the rect that `measureControl:` answers — so a
   divider fell through to "the width of my `#text` prop", which a divider does not
   have, and **every divider was placed one pixel wide**. Invisible on Win32 (a 1px
   separator against a 1px separator) and invisible in review; it showed the instant
   a realizer drew it, as a dot in the corner instead of a rule across the card, with
   the frame reading back `{16, 776, 1, 1}`. Fixed in the layout and pinned by
   `testADividerIsPlacedAcrossTheFullWidth`, which was verified to fail without the
   fix. Carry back to WINARMVM.

   Both divergences are the same shape and worth stating as a lesson: **the value of
   a second realizer is that it re-asks every question the first one answered.**
6. **Blast vs patch, scoped rather than repealed.** The house law is
   "VM-owned GUI views send WHOLE state each change, never incremental deltas". That
   stands for VM-owned pixel and state views — GamePane, Canvas, the metrics beat.
   *Control* windows use republish-with-a-provably-safe-patch, which is the same
   spirit: the author always writes a full republish, and the differ is a bounded
   optimization underneath that **declines to a full rebuild** whenever it cannot
   prove a clean patch. The precedent is already set on the Windows side, where
   damage-rect pane repaint was adopted deliberately. **Never patch a framebuffer.**

## 7. Traps carried over, so they are paid once

- **Patch metrics from day one** (`patchCount`, `rebuildCount`) — they are how an
  id-strategy failure is *seen* rather than felt as flicker. The demo tool shows them
  on its own face.
- **The suppress-events guard**, or programmatic updates re-enter as user events and
  loop.
- **Mirror user-applied state into the retained spec** (host echo), or every
  interaction is followed by a needless patch. WINARMVM's worst bug in the whole arc
  was a patch-routing failure whose signature was *"the screen reverts the model"*.
- **The silent control** — a control that works but never notifies. WINARMVM met it
  three times in three disguises (a pane with no event, a checkbox drawing its label
  twice, a list box needing `LBS_NOTIFY`). Cocoa will have its own set. The
  framework's answer: a pane's event **defaults to its id**, and the realizer asserts
  that every interactive node carries an event.
- **Only whitelisted array props patch in place** (`options`, and later `rows`/
  `items`); any other non-scalar change rebuilds. Model props as scalars wherever a
  choice exists.
- **A checkbox is exempt from the label rule** — AppKit, like Win32, draws a
  checkbox's own title beside the box, so a label above it renders the word twice.

## 8. Staging

Each stage is independently valuable and separately committable.

| Stage | Contents | Proof |
|---|---|---|
| **A0** | This record. Vendor the pure half — `AppSpec`/`AppNode` + `AppSpecLayout`, renamed — and port WINARMVM's pure tests. No Cocoa. | the spec/layout tests run green headless on the Mac, in the world suite |
| **A1** | `AppToolWindow` portable half: registry, event depth + coalescing, suppress guard, counters, damage union, the rerender pipeline. Plus the `Win*` compat shims. | headless: a fake realizer records ops; a click diffs to exactly one `setNodeProps` |
| **A2** | `CocoaRealize` MVP — flipped host view, window/stack/row/card/text/heading/button/input/checkbox/divider, the label rule, action binding, op apply. **Port `125_winui_demotool.mst` unchanged.** | on screen: click moves `clicks:` while `rebuilds:` stays at 1 |
| **A3** | Panes (`rgbaPane`, `indexedPane`, `textGrid`) over Canvas/GamePane + damage-driven repaint; `slider`, `select`, `listBox`. | a stroke invalidates one cell; a combo patches in place |
| **A4** | **Port `126_winui_spriteed.mst` + `127_winui_soundtool.mst` unchanged** — the acceptance test of §4. | both editors run on the Mac from the Windows sources |
| **A5** | App-per-worker-VM: each app its own VM, diff off-main, realizer as display server; supervision and restart. | two apps open; one is killed and respawns; the other never stalls |
| **A6** | `foreignView` escape hatch, Tools menu integration, the cookbook. | a mixed declarative/AppKit app |

**Why the isolation comes at A5 and not A0.** Building the realizer and the
multi-VM display server at once means debugging two hard things simultaneously.
A2 proves the reconcile loop with the app running in the UI worker; A5 moves it
behind the same tool-facing API, which does not change. The API is designed for
A5 from the start — a tool never touches a window, so it cannot tell which VM it
is in.

GamePane and Canvas stay on Blast throughout.
