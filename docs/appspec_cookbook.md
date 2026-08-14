# AppSpec cookbook — writing a tool, and everything you'll trip on

The practical companion to `docs/appspec.md` (the decision record). Everything
here runs today; every claim is pinned by a test named beside it.

## 1. A tool is three things

A title, a block that answers a spec, a block that handles an event. No window
code, no control code, no layout code, no teardown code — the framework owns
every native object, which is why you cannot leak one.

```smalltalk
Object subclass: Counter [
    <classVars: Count>
    Counter class >> count [ ^Count isNil ifTrue: [ 0 ] ifFalse: [ Count ] ]

    Counter class >> spec [
        ^AppSpec window: 'Counter' id: #counterWin body: (AppSpec stack: {
            AppSpec heading: 'A tiny app' id: #head.
            AppSpec text: 'count: ' , self count printString id: #readout.
            AppSpec button: 'Bump' event: #bump id: #bumpBtn
        } id: #root)
    ]

    Counter class >> handle: event value: value [
        event == #bump ifTrue: [ Count := self count + 1 ]
    ]

    Counter class >> install [
        AppToolWindow
            register: #counter title: 'Counter' width: 320 height: 240
            render: [ Counter spec ]
            handler: [ :e :v | Counter handle: e value: v.
                               (AppToolWindow named: #counter) rerender ]
    ]
]
```

`Counter install.` then `AppToolWindow open: #counter.` — or just pick it from
the **Tools menu**, which lists every registered tool and appends new ones the
moment they register (`AppToolWindow onRegister:`).

**The one discipline: name every node.** Identity is `(type, id)`. A named
tree diffs to "set this caption"; an unnamed one diffs to "rebuild this
window". The counters tell you which you got — `patchCount` should move,
`rebuildCount` should not. Put them on your face while developing; the demo
tools all do.

## 2. Republish, don't poke

The programming model is: change state, `rerender`. The differ makes it
affordable — an unchanged republish costs nothing (`AppToolWindowTests`), N
changes in one handler coalesce to one reconcile, and a changed caption
arrives as one `setNodeProps`. You never touch a control, so the screen
cannot disagree with your model.

Vocabulary: containers `stack`/`row` (+`row:weights:` for columns)/`grid`/
`card`/`divider`; controls `text`/`heading`/`button`/`input`/`checkbox`/
`slider`/`select`/`listBox`*; panes `rgbaPane`/`indexedPane`*/`textGrid`*;
and `foreignView` (§5). Starred ones currently realize as visible
placeholders on the Mac.

## 3. Panes: pixels you own, damage you declare

A pane's contents are pixels no diff can carry, so you draw them and say what
you touched:

```smalltalk
"in the handler:"
tool invalidatePane: #grid rect: (Array with: x with: y with: w with: h).

"the paint block (register:...paint:) — same five sentences as WINARMVM:"
paint: [ :tool :paneId :region |
    | h plane stride |
    h := tool paneHandle: #grid.
    plane := AppPixels planeFor: h width: 288 height: 288.
    stride := AppPixels strideFor: h.
    AppPixels at: plane x: px y: py stride: stride
              put: (AppPixels bgraR: 220 g: 60 b: 60).
    AppRender present: h ]
```

The plane is retained between paints (that's what makes partial repaint
*sound*), a click arrives in the pane's own top-left pixels (the same space
your damage rect speaks), and a paint block is only called for panes somebody
invalidated. `AppPaneTests` proves damage by staleness, not call counts.
Word order is BGRA — Windows' — so a drawing loop is the same text on both
platforms; the Mac presenter shuffles once per present.

## 4. An app in its own VM

Same tool, zero changes — `spec` + `handle:value:` is the whole interface:

```smalltalk
"on the primary:"
AppVmHost start: #counter tool: 'Counter'.
```

The primary spawns it and is then out of the path: frames go straight to the
display worker, events straight back (`docs/worker_peer_links.md`). Your
model, render and handler run on their own core; the window is drawn by the
display and **outlives your VM** — if your app dies, the last frame stays up
and events into the corpse are reported, not raised (`tests/it_appvm.rs`).

## 5. The escape hatch: `foreignView`

The framework is a default, not a fence. A spec can name a hole and let the
platform fill it with a view somebody built by hand:

```smalltalk
"in the spec (portable — names the hole, never the view):"
AppSpec foreignView: #meter width: 430 height: 64.

"on the Cocoa side (a provider, registered once):"
CocoaRealize provideForeign: #meter with: [ :node |
    | box | box := ((Cocoa classNamed: 'NSBox') onMain alloc) onMain
        initWithFrame: (Array with: 0.0 with: 0.0 with: 430.0 with: 64.0).
    "…hand-build to taste; wire buttons to `w dispatch: #event value: v`…"
    box ]
```

The provider runs realizer-side and never crosses a VM, so the spec stays
picklable and the tool stays portable; on a platform with no provider the
hole renders as a visible placeholder saying whose it is. The Mixed Demo
(`world/91d_appmixed.mst` + `world/92a_appmixedstrip.mst`) is the worked
example: its native button dispatches the same `#bump` as its declarative
one, and the model cannot tell them apart. And nothing stops you ignoring
all of this and writing a raw NSWindow — the sprite/sound editors' native
versions still do.

## 6. Reaching the image and the shell

Six verbs, late-bound, failure-soft (`world/90a_apphost.mst`):
`AppHost persistClass:` / `methodSourceFor:side:selector:` /
`implementorsOf:`, `AppShell clipboardText:` / `launchDemo:named:` /
`editorMetricsDpi`. Wrap calls in `on: Error do:` as the vendored editors do;
headless they answer nil and that is a correct state.

## 7. The traps, so you pay for them once

- **The GUI boots from the image.** After ANY world edit: `./reseed-world.sh`.
  Symptom of forgetting: `undeclared variable`.
- **Trailing doits never run under image boot** — that's why `install` is a
  class-side method the Tools menu calls lazily, and why the realizer is
  resolved *by name*, not installed by a doit.
- **`exec` runs ONE top-level item.** An init source or scripted doit with
  two statements runs its first. Wrap in `[ ... ] value` when you need more.
- **The UI worker is shape-routed**: unsolicited messages land on
  `Worker onReply:`, never `onMessage:`. A spawned worker is the opposite.
  `AppVmDisplay` chains, so add handlers through it, not over it.
- **`and:and:` is not a selector.** Nest: `(a and: [ b and: [ c ] ])`.
- **Blocks don't pickle** — a spec carries event *names*; handlers stay home.
- **Every interactive node needs an event** (a pane's defaults to its id) —
  the "silent control" family cost three bugs upstream.
- **A hand-wired action inside a provider must RETAIN its delegate.** An
  NSControl's target is weak; the framework's `wire:to:` keeps its delegates
  in a collection and so must you (`CocoaMixedStrip`'s `Delegates`). The
  failure is vicious: the button works, then a GC later it silently doesn't.
- **Read stderr.** A raise inside the UI drain surfaces only as
  `macvm-cocoa: UI worker drain error: …` on the launch terminal.
