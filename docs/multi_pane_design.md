# Several demos at once — what multiple panes actually costs

Discussion record, 2026-08-15, after S6 put each demo in a VM of its own
(`docs/process_services.md` S6). The threading question is settled: demos
already run on their own threads, with their own clocks, reading input
themselves. What remains is that they all draw into **one surface**.

## 1. Where the "one" actually lives

Not in the VMs, and not in the transport. Four places, all in the host:

| the single thing | where | what it means |
|---|---|---|
| `static GAME: RefCell<Option<NativeGame>>` | `cocoa_gui/game.rs` | one window, one Metal layer, one sprite set, one HUD |
| `GameCommand` has no pane field | `src/embed.rs` | a command says *what* to draw, never *where* |
| `SCREEN_PTRS` / `SCREEN_STRIDE` / … | `src/embed.rs` | ONE published direct framebuffer, process-wide |
| input snapshot, `GAME_ACTIVE`, `SESSION_OPEN` | `game_input.rs`, `game.rs` | one keyboard, one pointer, one session |

The command **queue** is not on that list, and that matters: it is already a
shared, main-drained, multi-writer channel. Two demos writing to it today
would interleave their commands into one pane — the transport is ready, the
*addressing* is missing.

## 2. The change, in dependency order

**a. Commands get a destination.** `GameCommand` becomes `(PaneId,
GameCommand)` at the queue, and the drain routes by id instead of assuming
`GAME`. `PaneId` is minted by the host when a VM first draws, and belongs to
the VM that opened it — which is exactly the `(id, generation)` handle idea
the fleet already uses, so a stale id from a dead demo can never address a
live pane.

The cost is one field, and it is the only *invasive* edit: every emit site and
the whole drain match arm list touches it. Nothing about it is difficult;
there is simply a lot of it.

**b. `NativeGame` becomes a small map.** `RefCell<Option<NativeGame>>` →
`RefCell<HashMap<PaneId, NativeGame>>`, still main-thread, still thread-local.
Window creation, `close_window`, the frame timer and `ensure_pane` each take a
`PaneId`. The frame timer is per-pane or one timer that ticks every open pane
— the latter is simpler and matches how AppKit wants to drive layers.

**c. Input has to acquire a subject.** This is the real design question, not
the plumbing. Today one keyboard and one pointer feed one demo. With several
panes, `GamePane inputState` must answer *this VM's* input, which means the
service keys its snapshot by pane and the host attributes each event to the
pane whose window has focus. The natural rule — and the one every OS already
implements — is **the focused window owns the keyboard; the pointer belongs to
the window under it**. An unfocused demo reads "no keys, pointer outside",
which is exactly right: a background game should not respond to typing aimed
at the Browser.

**d. Direct framebuffers become per-pane.** `publish_screen_memory` and its
generation counter are process-global singletons today. They become a small
table keyed by `PaneId`; the generation bump per entry keeps the existing
"stale Alien reads as no-buffer" safety. Demos using the direct path
(Plasma, MandelZoom) are the ones affected.

**e. The one-at-a-time policy dissolves.** `DemoVmHost stopAll` becomes
`stop:` for a named demo, `GAME_ACTIVE`/`SESSION_OPEN` become per-pane state,
and `request_launch`'s "tear down the previous one first" simply goes away.
The Demos menu stops being radio buttons.

## 3. What it buys, honestly

The demos are the *demonstration*, not the point. Several panes at once is
what makes the multi-VM claim visible in one screenshot: Life, Breakout and
Mandelbrot animating side by side, each on its own core, each stoppable
without touching the others — and one of them crashing while the rest keep
running, which is the property that actually matters and currently has no
way to be seen.

It also generalizes past demos. A pane is the only surface a Smalltalk
program can paint pixels into at speed; per-pane addressing is what lets a
TOOL own one (the sprite editor's preview, a profiler's flame chart) while a
demo owns another. AppSpec's `rgbaPane` already crossed VMs in a different
way (blits shipped beside the spec — `93_appvm.mst`), and the two mechanisms
should not stay unaware of each other forever: the same `PaneId` should
eventually name both.

## 4. What it costs

Sizeable but not deep: (a) is mechanical breadth, (b) and (d) are contained
rewrites of two host files, (e) is deletion. **(c) is the only part with a
genuine design decision in it**, and it is a decision about focus semantics
rather than about VMs.

The risk worth naming: every pane is a Metal layer with its own drawable, so
N demos at 60 Hz is N times the GPU work and N times the presents. Three or
four is comfortable; a dozen is a different conversation about frame
budgeting, and the frame timer should probably tick a pane only while its
window is visible.

## 5. Recommendation

Worth doing, in the order above, as its own sprint — and worth doing *after*
the sprite editor and the demos have lived in their own VMs for a bit, since
they are the two consumers whose real usage should shape the `PaneId` API
rather than the other way round. Nothing in S6 blocks it; nothing in it
requires revisiting S6.

The one thing to decide before starting: **focus semantics** (§2c). Everything
else follows mechanically from it.
