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

**Cost, corrected** (the author's own reading, and the right one): these are
LOW-RESOLUTION game panes — 320×240 of palette indices, one small texture
upload and one composite each. A pane is not a retina-sized surface with a
deep layer tree; it costs almost nothing to have several of them ticking. So
"how many panes can we afford" is not the constraint this design has to be
shaped around, and multiple ticking panes should simply work.

What is still worth doing, cheaply: tick a pane only while its window is
visible — not because the GPU could not take it, but because a minimised demo
computing frames nobody sees is wasted CPU, and it costs one `isVisible`
check per pane per tick to avoid.

## 5. Recommendation

Worth doing, in the order above, as its own sprint — and worth doing *after*
the sprite editor and the demos have lived in their own VMs for a bit, since
they are the two consumers whose real usage should shape the `PaneId` API
rather than the other way round. Nothing in S6 blocks it; nothing in it
requires revisiting S6.

The one thing to decide before starting: **focus semantics** (§2c). Everything
else follows mechanically from it.

---

## 6. Verification pass, 2026-08-16 — and the thing §1 missed

Re-read against the code as it stands after S6, the demo-VM arc and the
frame-rate fix. Two outcomes: the table in §1 is **exact**, and it is
**incomplete**.

### Still true, verbatim

Every singleton in §1 is where it said, unchanged:

| claim | confirmed at |
|---|---|
| one `GAME` cell | `cocoa_gui/src/game.rs` — `static GAME: RefCell<Option<NativeGame>>` |
| commands carry no destination | `src/embed.rs` — `enum GameCommand`, no pane field on any arm |
| one published framebuffer | `src/embed.rs` — `SCREEN_PTRS` / `SCREEN_STRIDE` / `SCREEN_GENERATION` |
| one session, one input | `game.rs` — `GAME_ACTIVE` / `SESSION_OPEN`; `runtime/game_input.rs` — `static STATE: Mutex<InputState>` |

§2c is confirmed as the real decision rather than plumbing: **prim 279 takes
no key at all**. `prim_game_input_state` reads `game_input::snapshot()` and
hands the same four numbers to whichever VM asked. Two demos running today
would both respond to the same keystrokes — not because anything routes
wrongly, but because there is nothing to route *by*.

### What §1 missed: sound is a queue, not a mixer

The audio path was never in the table, and it turns out to have a
single-instance of its own — a different shape from the video one.

`Sfx` (MacGamePane `audio/src/playback.rs`) holds **64 buffers and ONE
`AVAudioPlayerNode`**, attached once to the engine's main mixer. `play(id)`
calls `scheduleBuffer:completionHandler:` on that one node, and a player node
plays what it is handed **in order**. So sounds do not overlap: they queue.

This is not only a multi-demo problem. It is already visible with one demo —
galaxigans' own comment says running at the wrong rate "piles up sound
triggers", which is this queue growing faster than it drains. (Read from the
code and corroborated by that comment; not measured with an audio capture.)

The fix is unlike the video one, and cheaper: **a voice pool**. Attach N
player nodes to the main mixer and round-robin `play` across them, so
overlapping triggers land on different voices and the mixer does what mixers
do. Worth noting what this does *not* need:

> **Audio needs no addressing.** A pane must know which window it belongs to;
> a sound does not, because mixing IS the wanted semantic — when Breakout and
> Galaxigans both play, you want to hear both. So sound needs no `PaneId`, no
> focus rule and no ownership; it needs voices. That asymmetry is why it can
> land first and independently.

One shared slot does still need a decision: slot 16 is *the* parametric
audition slot and auditions deliberately replace each other, which is right
for one editor and wrong for two demos playing custom effects. A voice pool
makes the natural answer available (allocate a slot per trigger, round-robin
like the voices).

### Revised order

1. **Voice pool** — independent of everything else, fixes a defect that exists
   today with a single demo, and needs no design decision.
2. **Focus semantics** (§2c) — the one genuine call. Everything in §2 follows
   from it mechanically.
3. Then §2a → §2b → §2d → §2e as written.

The two-live-sessions race found on 2026-08-15 (two launches slipping past
`GAME_ACTIVE` before it flips) is a symptom of §2e rather than a separate bug:
under per-pane state the check it races on stops existing. Worth leaving alone
until then rather than growing a lock that step 2e deletes.
