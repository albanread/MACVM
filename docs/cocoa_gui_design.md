# A native Cocoa GUI for MACVM — the environment, written in itself

**Status: design.** A second, flagged GUI mode. The current shell
(`gui/`, `macvm-gui`) renders its interface as HTML in a `WKWebView`. This
document designs the alternative: **a native AppKit interface built from
Smalltalk, through the Cocoa bridge**, in which the interface is a
Smalltalk VM pinned to the main thread and the persistent environment is a
Smalltalk VM behind it. It is the capstone of the Cocoa bridge (C0–C5,
`cocoa_bridge_design.md`) and the reflective RPC work
(`perform:withArguments:`), and needs one new bridge capability, developed
here as **C6: reverse dispatch (delegates)**.

Both GUIs coexist; the WKWebView shell stays the default. Cocoa mode is a
deliberate, flagged choice (`macvm-gui --cocoa` / `macvm-cocoa`).

```smalltalk
"the interface is Smalltalk calling AppKit; the environment is a VM behind it"
UI window: (Workspace on: 'Transcript showCr: 3 + 4').
UI window: (ClassBrowser on: Integer).
UI run.        "= [NSApp run] on the main thread; events dispatch to Smalltalk"
```

## 1. Three VMs, three tiers — and which thread each lives on

The decisive design fact is the **thread/role assignment**. AppKit forces
one thing only: the interface must run on the process's main thread.
Everything else is a choice, and the right choice is to keep the
*persistent brain off the main thread* so it can neither freeze the UI nor
take the process down with it.

| Tier | Thread | Role | Risk profile |
|---|---|---|---|
| **UI worker VM** | **main (pinned by AppKit)** | a *dumb terminal*: builds native views, runs `[NSApp run]`, answers AppKit delegate calls from a **local snapshot**, forwards events | thin, low-risk; failures mostly per-event-recoverable |
| **Primary ST VM** | **2nd (background)** | the **persistent state**: live objects, classes-being-edited, workspace vars; boots the world, **spawns the workers (incl. the UI worker)**, runs doits, blasts snapshots to the UI | the real logic; **restartable** because it is not the main thread |
| **Compute worker VMs** | other background | parallel work spawned by the primary | isolated; die+respawn (M0–M4) |

The two moves that make this work, and distinguish it from the naïve
"VM on main" arrangement:

1. **The UI is a *worker*, not the primary.** The persistent VM — where
   your code and objects actually live — is a background thread. The main
   thread hosts a second VM whose only job is the display, pinned to main
   only because Cocoa requires it. The interface is demoted from *the
   brain* to *a display device that happens to be on a specific thread*.
2. **The UI worker is a *dumb terminal* (`feedback_blast_dont_patch`).** It
   holds a **local snapshot** of what each view should show; it renders
   that snapshot and forwards events; it never holds authoritative state
   and never patches incrementally. When the model changes, the primary
   *blasts a whole fresh snapshot* and the terminal re-renders. This is the
   editor-view lesson made structural.

Everything below follows from those two moves. Three consequences are
load-bearing and get honest treatment: **the crash model is recovered
(§5)**, **the UI never freezes on model work (§6)**, and **delegates are
answerable synchronously without a cross-thread wait (§4)** — the last of
which is *why* the terminal holds a snapshot.

## 2. Why this shape (and why "VM on main" was wrong)

An earlier draft of this design put the *persistent* VM on the main thread
and called it "the UI." That is the obvious arrangement and it is wrong,
for two reasons this shape fixes:

- **Crash safety.** A fatal doit (`error:`, DNU, heap exhaustion, a native
  fault) ends in `fatal_exit`. On the main thread the only options are
  `pthread_exit` (tears the process down) or a blind `siglongjmp` through
  AppKit frames (undefined behaviour, the reason S21 rejected unwinding
  through JIT frames) — so "VM on main" *loses* the S21 restart-on-death
  model. With the persistent VM on a **background** thread, a fatal doit
  `pthread_exit`s that thread, a supervisor respawns it, and the UI
  terminal shows "reconnecting" and re-syncs — **S21's model, recovered
  (§5).**
- **Responsiveness.** If the persistent VM is the main thread, a long doit
  freezes the UI *and* the coordinator. With doits on the background
  primary, a long doit blocks the primary — not the paint loop — so the UI
  stays live and can show progress (§6).

The interface must be on main; the *environment* must not be. That is the
whole idea.

## 3. Boot, the flag, and how the UI worker gets onto the main thread

The two modes share the crate, the world/image, the Cocoa bridge, the
GamePane, and `objc::bootstrap()`. `fn main()` (`gui/src/main.rs`) already
dispatches on the first CLI arg; Cocoa mode is one more arm:

```
macvm-gui               → WKWebView mode (default, unchanged)
macvm-gui --cocoa       → Cocoa mode
macvm-cocoa             → alias (a second bin, same entry)
```

The chicken-and-egg — *the primary spawns the UI worker, but the UI worker
must land on the main thread the process started on* — is resolved by
parking the main thread and letting the primary drive it:

1. **(main thread)** `objc::bootstrap()` — `NSApplication`, activation
   policy, menu bar. AppKit init must be on main. (Shared with WKWebView
   mode.)
2. **(main thread)** Spawn the **primary VM on a background thread** and
   then *park*, waiting on a channel for its "become the UI worker"
   instruction.
3. **(background)** The primary boots the world (persistent state), then
   **spawns workers** — the first of which is the special **UI worker**.
   "Spawning" the UI worker is not creating a thread: it sends the parked
   main thread a boot payload (the UI worker's Smalltalk boot code + its
   link back to the primary).
4. **(main thread)** Unparks, boots the **UI worker VM in place**, builds
   the initial windows (a Workspace + a Transcript), installs their C6
   delegates, orders them front, and calls `UI run` = `[NSApp run]`.
5. From here the AppKit event loop *is* the UI worker's main loop. Each
   event dispatches, through a C6 delegate/action trampoline, into the UI
   worker's Smalltalk (§4); anything needing the environment is a message
   to the primary (§6); the primary blasts snapshots back, drained on the
   main thread between events (the M3 wake, reused, §8).

`enable_main_hop()` (C3) is **not** used the way the WKWebView GUI uses it:
there is no Rust-owned main to hop to. It stays available for the *reverse*
direction — a background VM asking the UI worker to do something on main
(§8). Neither VM's live objects are persisted (MACVM has no running-image
save, by design); each launch rebuilds the windows by re-running the UI
worker's boot code, exactly as the world boots from source. Durable UI
preferences (window frames) are saved as *data* (a small plist /
image_store table), never as pickled views.

## 4. C6 — reverse dispatch: delegates answered from the local snapshot

Everything an IDE does past "click a button" is a **delegate or data
source** — `NSTableViewDataSource` answering `numberOfRowsInTableView:`,
`NSOutlineViewDataSource` answering `outlineView:child:ofItem:`,
`NSTextViewDelegate` answering `textDidChange:`. These are **synchronous
calls from AppKit that expect a return value now**, to paint. C4's
`MacvmAction` is fire-and-forget target/action (void); it cannot answer a
data source. C6 is the mechanism that can.

### 4.1 Why "answer from a local snapshot" is the crux

AppKit calls the data source **synchronously on the main thread** and
blocks until it returns. The authoritative model lives on the **primary
(background) VM**. If the delegate had to *ask the primary* for each row —
a synchronous main→primary wait — a busy primary would freeze the paint,
and a primary waiting on the UI would deadlock. That is the exact wait
cycle the worker threading model forbids.

The dumb-terminal design dissolves it: the UI worker answers **from its own
local snapshot**, on its own thread, with no wait. The snapshot is a copy
the primary blasted (via the pickle) the last time the model changed — a
plain Smalltalk data structure living in the UI worker's own heap. So
`numberOfRowsInTableView:` is `^snapshot rowCount`, answered instantly and
locally. This is *why* the terminal holds a snapshot: to make AppKit's
synchronous callbacks answerable without ever crossing a thread.

### 4.2 The mechanism — one forwarding trampoline, C2's marshalling in reverse

One ObjC class, `MacvmDelegate : NSObject`, registered once, responding to
arbitrary selectors via Objective-C's own dynamic message forwarding:

- `respondsToSelector:` → true for the selectors the bound Smalltalk
  object declares it handles (a per-instance allow-list, so optional
  delegate methods stay optional and AppKit's probes answer correctly).
- `methodSignatureForSelector:` → an `NSMethodSignature` from the
  selector's `@encode` string (`+[NSMethodSignature signatureWithObjCTypes:]`).
  We already resolve that string at runtime for the forward direction
  (`objc_bridge::resolve_shape`, C2).
- `forwardInvocation:` — the heart. Read the selector; for each argument
  index read the raw bytes with `-getArgument:atIndex:` and **unmarshal to
  a Smalltalk oop using C2's `classify_arg`** (native→Smalltalk: id→wrap
  `ObjcRef`, integer→SmallInteger, double→Double, `NSString`→String,
  struct→Array); build an argument Array; run
  `delegate perform: selector withArguments: args` — **the reflective
  primitive built for the worker RPC, reused verbatim** — then **marshal
  the Smalltalk result back into the return slot with C2's `classify_ret`**
  via `-setReturnValue:`.

So C6 is **C2's marshalling run backwards**, glued by `perform:`. The
`@encode` vocabulary, the struct set, the width rules — all already exist
and are already tested. New code: the two override IMPs, the NSInvocation
get/set-argument plumbing, and a per-instance `(delegate-oop, allow-list)`
registry (the C4 ticket registry generalized from "one action block" to
"one Smalltalk object + its handled selectors"). It runs entirely on the
**main thread** against the UI worker's local objects — the same-thread
case C0–C5's callbacks made safe.

### 4.3 Ownership of delegate objects

A window/table/textview **retains** its delegate (an `ObjcRef` wrapping a
`MacvmDelegate`), which holds a **ticket** into the GC-rooted registry
naming the UI worker's Smalltalk delegate object — no oop is ever stored
ObjC-side (the §2 bridge contract holds). Delegates are **long-lived**:
built outside any `poolDo:` scope, released on `windowWillClose:`. A
never-closed window leaks its ticket, acceptably; a double-registered
delegate is refused (monotonic tickets, C4 rule).

## 5. The crash model — recovered, because the brain is off-main

The persistent VM being a **background** thread restores S21's
restart-on-death model that "VM on main" would have lost:

- **A fatal doit runs on the primary (thread 2).** `error:`/DNU/stack
  overflow/heap-exhaustion → `fatal_exit` → `pthread_exit` of the *primary*
  thread (a background thread, so this is legal and clean) → a supervisor
  respawns the primary from source → the UI terminal, on the main thread,
  detects the drop, shows "environment restarting…", and re-syncs when the
  fresh primary is up. **The UI never goes down; the environment recovers
  exactly as a dead worker recovers today** (`feedback_recover_clean_or_die`:
  respawn to a clean state, per-VM `ErrorPolicy::Die` for the throwaway
  case). The persistent objects that were *not* mid-mutation are rebuilt
  from source/image_store; a half-run doit's partial effects are lost with
  the dead VM — which is the honest, clean outcome, not a fake rollback.
- **A recoverable error inside a UI-worker delegate/action** (a display
  callback that itself sends `error:`) unwinds to a **per-callback
  `sigsetjmp` boundary** established at the C6/C4 trampoline before it calls
  Smalltalk, via a new `FatalMode::AbortToEventLoop` that `siglongjmp`s
  there instead of exiting. The jump target is the trampoline AppKit just
  called, so it unwinds only the UI worker's own frames and returns
  normally to the run loop. The one action reports and the app keeps
  pumping. (This is S21's `sigsetjmp` recovery, relocated to the callback
  door.)

Two conditions stay genuinely fatal, stated plainly:

- **A native fault inside AppKit on the main thread** (a SIGSEGV in
  framework code called by a UI delegate) is caught by PROBE's foreign-fault
  recovery (S21 §1c) so the *signal* doesn't kill us — but it survives by
  `siglongjmp`-ing out of Apple's frames mid-mutation. The UI worker runs
  deliberately thin code precisely to keep this rare; when it happens the
  honest posture is: report, poison the UI subsystem, relaunch. The
  **primary's persistent state is unaffected** (different thread, different
  heap), so a relaunch loses only the window layout, not the environment.
- **Heap exhaustion on the UI worker** has nowhere to fail over; fatal
  with a crash dossier. Kept unlikely by the terminal holding only
  snapshots, not the world.

The net: the *environment* (primary) is restartable and its state is the
durable thing; the *interface* (UI worker) is disposable and mostly
per-event-recoverable. That is the right durability split for an IDE.

## 6. Responsiveness — the UI stays live because the brain is elsewhere

A long or infinite doit blocks the **primary (thread 2)**, not the main
thread. The UI worker keeps painting, scrolling, and accepting input; it
can show a spinner or a "still running…" status because its run loop is
never held by the doit. This is the second dividend of keeping the
persistent VM off main.

The doit path makes this concrete: a Workspace `doIt` **ships its source to
the primary** (a message; the doit runs where the persistent objects live),
and the primary replies the result, which the UI displays. So:

- **short doits** (a `printIt`, a browser refresh request) round-trip fast
  and feel synchronous;
- **long doits** run on the primary while the UI stays responsive, and the
  primary can *further* dispatch heavy work to **compute worker VMs**
  (`multi-smalltalk-worker.md`) so it too stays available — the same
  `call:on:args:onReply:` RPC just built.

The GUI framework should offer this as one line — `aView do: source then:
[:r | …]` — so "the interface never blocks" is the default path, not a
discipline. The primary→UI reply and the primary→UI snapshot-blast are both
drained on the main thread *between* AppKit events (the M3 wake as a
run-loop source), never mid-event, preserving the strictly-serial invariant.

## 7. The views — reuse the models, render to AppKit

The web GUI's views are `Visual` subclasses in the image with real *model*
logic — `ClassHierarchyOutliner`, `ClassOutliner`, `ImplementorsView`,
`SendersView`, `DefinitionsView`, `CodeView`, plus `Workspace`,
`CodeEditor`, `ClassMirror`, and the reflection tools (`world/33_smappl.mst`,
`34_tools.mst`, `60_editor.mst`). They answer `htmlFragment` today. The
model logic runs on the **primary** (it is the environment's knowledge of
itself). What crosses to the UI worker is a **snapshot** the model
produces — the same information the HTML renderer consumes, shaped as data
instead of markup — which the UI worker's native view + data-source
delegate renders. HTML and AppKit are two faces of one model
(`feedback_dual_placement_not_migration`): a `htmlFragment` renderer on the
primary, a `snapshot`/`buildCocoaView` renderer split across primary
(snapshot) and UI worker (AppKit).

The Smalltalk GUI framework splits across the two VMs:

- **On the primary:** the models (reusing `ClassMirror` etc.) and a
  `snapshotFor:` method per view that produces the copyable data the
  terminal renders; the doit executor; the snapshot-blaster.
- **On the UI worker** (`world/5x_cocoaui/`): thin AppKit wrappers, each
  with its C6 delegate answering from the last snapshot:
  - **UI / CocoaApp** — menu bar, window set, the `[NSApp run]` bracket,
    the `do:then:` worker helper.
  - **Workspace** — an `NSTextView`; ⌘D/⌘P ship the selection to the
    primary, append the reply.
  - **Transcript** — an append-only `NSTextView`; the primary's transcript
    sink targets it (a `TranscriptSink` that blasts text lines).
  - **ClassBrowser** — the classic Smalltalk browser is `NSBrowser`/
    `NSOutlineView`-shaped; its data source answers rows from a snapshot
    the primary built from `ClassMirror`/`ClassHierarchyOutliner`. Selecting
    a method opens its source in a **CodeView**.
  - **CodeView** — an `NSTextView` with Smalltalk syntax colour
    (`NSAttributedString` ranges — a modest marshalling add); Save ships
    the source to the primary to recompile + image_store-write (the web
    edit path).
  - **Find views** — `Implementors/Senders/Definitions` as `NSTableView`s
    over primary-built snapshots (accurate via the persisted `method_sends`
    table).
  - **GamePane** — drops in unchanged; already a native Metal view
    (`gamepane_design.md`), now a window content view instead of a panel
    beside a `WKWebView`. (A game pane driven by a compute worker updates on
    the main thread the same way any snapshot does.)

No model logic is duplicated: the primary owns *what to show*, the UI
worker owns *how to draw it in AppKit*, and the snapshot is the wire
between them.

## 8. Workers, and the star with a background centre

Multi-VM is unchanged in shape. The **primary is the star's centre**
(background thread); the **UI worker and the compute workers are leaves**.
Two specifics:

- **Only the UI worker may touch AppKit.** Neither the primary nor a
  compute worker is on the main thread, so none may call an AppKit class
  directly; UI work is a message to the UI worker, whose C6 handler does the
  AppKit call on main. The bridge enforces it — an AppKit-prefixed send from
  a non-main VM fails loudly (the curated main-thread class list,
  `cocoa_bridge_design.md` §7, made a real guard). This is C3/C4 machinery
  *in reverse and re-homed*: instead of workers hopping to a Rust-owned
  main, they message the **UI worker VM** that owns main.
- **Both inbox drains are run-loop sources.** The UI worker's inbox (fed by
  the primary: snapshots, doit replies) drains on the main thread between
  events. The primary's inbox (fed by the UI worker: events, doit requests;
  and by compute workers: results) drains on its own background loop. The M3
  coalesced wake schedules each without a race.

## 9. Reused vs. genuinely new

**Reused wholesale:** the Cocoa bridge C0–C5; `perform:withArguments:` (the
delegate dispatch core); the C2 `@encode` marshalling (run in reverse for
C6); the `VmHandle` boot + the S21 supervisor/restart + `sigsetjmp`
recovery (the primary is restarted exactly like today's worker; the UI
worker relocates the `sigsetjmp` to the callback door); the multi-VM star,
the pickle (snapshots ride it), and the `call:on:args:onReply:` RPC; the
M3 wake (both drains); the image/world and `image_store`; every view
*model*; the GamePane.

**Genuinely new:**
1. **C6 reverse dispatch** — `MacvmDelegate` forwarding + NSInvocation
   plumbing + the per-instance delegate registry (§4). ~1 file
   (`src/runtime/objc_delegate.rs`) + world methods.
2. **`FatalMode::AbortToEventLoop`** + the per-callback `sigsetjmp` boundary
   in the two trampolines (§5). Small, localized.
3. **The parked-main-thread boot handshake** (§3) — main parks, primary
   spawns the UI worker onto it. Localized to `gui/src/` + the embed boot.
4. **A modest marshalling top-up** for text UIs (`NSAttributedString`
   ranges, `NSColor`/`NSFont`), grown row-by-row (§7).
5. **The two-sided Smalltalk GUI framework** — primary-side `snapshotFor:`/
   blaster + UI-worker-side `world/5x_cocoaui/` wrappers. The bulk of the
   typing, the least of the risk (ordinary Smalltalk over a proven bridge).
6. **The `--cocoa` flag / `macvm-cocoa` bin** (§3).

## 10. Milestone ladder (each lands green and alone)

| | Deliverable | Proof |
|---|---|---|
| **G0** | `--cocoa`; parked-main boot handshake: primary on thread 2 spawns the UI worker onto main; one empty `NSWindow`; `[NSApp run]`; ⌘Q quits clean | on-screen (user); headless: the two-VM boot returns `Err` cleanly with no AppKit |
| **G1** | **C6**: `MacvmDelegate` forwarding + NSInvocation marshalling + the `sigsetjmp` abort boundary; a bad UI delegate doit aborts to the run loop, app survives | headless: a delegate `perform:` returns a value AppKit-side (proxied); a raising delegate unwinds clean, next event still dispatches |
| **G2** | Workspace + Transcript over the primary: ⌘P ships source → primary evaluates → `7`; `Transcript showCr:` blasts to the native view; **kill the primary mid-doit → it respawns, UI shows "restarting" and recovers** | on-screen; headless: sink receives text; the primary-restart path re-syncs |
| **G3** | ClassBrowser: `NSOutlineView` data-sourced from a primary-built snapshot (`ClassMirror`/`ClassHierarchyOutliner`); select class → methods → CodeView source | on-screen; headless: the data-source callbacks answer the same rows the `htmlFragment` model does (differential vs. the web model) |
| **G4** | CodeView editing: syntax colour, Save → ship to primary → recompile + image_store (the web edit path); Find views as `NSTableView`s | on-screen; headless: an edit round-trips through image_store byte-identically to the web path |
| **G5** | `do:then:` worker bracket; a heavy doit runs on the primary / a compute worker and updates a view while the UI stays live; GamePane as a window | on-screen: the UI stays responsive during a parallel-Mandelbrot dive driven from a native window |

G0–G2 are the real design risk (the boot handshake, reverse dispatch, the
abort boundary, and — the payoff — proving the primary-restart recovery on
screen). G3–G5 are mapping work over a proven base. On-screen verification
is the user's; headless gates cover the model/marshalling/recovery seams
(`feedback_gui_visual_verification`).

## 11. Non-goals, risks, open questions

**Non-goals (v1):** Interface Builder / nib loading (views built
imperatively); full Auto Layout (start springs/struts); persisting live
window objects (rebuilt from source; only prefs persist); replacing the
WKWebView GUI (it stays default).

**Risks, honestly:**
- **A native framework fault is a main-thread fault (§5)** — session-poison,
  not a survivable worker death. Mitigated by the UI worker running thin,
  snapshot-only code, and by the primary's state surviving on its own
  thread — but it is the highest-stakes failure and the reason the terminal
  is deliberately dumb.
- **Snapshot cost / staleness** — blasting whole snapshots (blast-don't-
  patch) is simple and drift-proof but re-sends unchanged data; for a large
  browser this is a real cost. Mitigation: snapshot *per view*, blast only
  the changed view, and keep snapshots coarse-but-cheap; measure before
  optimizing toward diffs (the editor's incremental patching is the
  cautionary tale — `feedback_blast_dont_patch`).
- **C6 correctness** — `forwardInvocation:` marshalling is C2 in reverse;
  its review surface (struct straddles, width truncation, ownership of
  returned objects, a delegate that raises mid-invocation and must still
  leave the NSInvocation return slot defined) is the C1/C2 surface and gets
  the same adversarial treatment.
- **Retain cycles** — window↔delegate↔UI-worker-object lifetimes need the
  C4 ticket discipline held for long-lived objects.

**Open questions for the build:**
1. `forwardInvocation:`/NSInvocation (general) vs. a registered-IMP table
   per selector-shape (C1-style, simpler). §4 assumes the former; G1 spikes
   both and measures the NSInvocation cost first.
2. Snapshot granularity and shape — per-view coarse snapshots vs. a shared
   model cache on the UI worker; start coarse (§7).
3. `--cocoa` flag vs. a separate `macvm-cocoa` bin — a separate bin keeps
   the parked-main boot path out of the WKWebView `main()`.
4. Does the UI worker run *any* IDE logic locally (fast local echo) or is
   it purely a terminal? Start purely-terminal (all logic on the primary);
   add local fast paths only where round-trip latency is felt.

## 12. Relationship to the existing docs

Sits on top of, and does not re-open: `cocoa_bridge_design.md` (C0–C5),
`multi-smalltalk-worker.md` (the star, the pickle, the RPC, the wake),
`docs/vm_handle.md` / S21 (the boot + the restart-on-death supervisor this
re-homes to the primary), `gamepane_design.md`, and the
`perform:withArguments:` RPC. It is where the Cocoa bridge stops being a way
to *call* macOS and becomes the substrate the environment is *built from* —
with the persistent environment kept safely off the main thread, so the
interface can be Smalltalk without the environment being hostage to it.
