# A native Cocoa GUI for MACVM — the environment, written in itself

**Status: design (adversarially reviewed 2026-07-17).** A second, flagged
GUI mode. The current shell (`gui/`, `macvm-gui`) renders its interface as
HTML in a `WKWebView`. This document designs the alternative: **a native
AppKit interface built from Smalltalk, through the Cocoa bridge**, in which
a Smalltalk VM pinned to the main thread *is* the interface and a Smalltalk
VM behind it is the persistent environment. It is the capstone of the Cocoa
bridge (C0–C5, `cocoa_bridge_design.md`) and the reflective RPC work
(`perform:withArguments:`), and needs one new bridge capability (**C6:
reverse dispatch, delegates**) plus a small amount of new *infrastructure*
the review surfaced (§9).

Both GUIs coexist; the WKWebView shell stays the default. Cocoa mode is a
deliberate, flagged choice.

> **Two design reviews reshaped this document (§9.0).** The load-bearing
> correction: the AppKit run loop is entered **from Rust glue with the VM
> quiescent**, so every AppKit→Smalltalk callback is an ordinary *top-level*
> VM entry through the existing `VmHandle::eval` door — not a re-entrant
> call into a live `&mut VmState`. That single decision makes the crash
> recovery, the delegate dispatch, and the drain all reuse machinery that
> exists, instead of needing new nested-recovery machinery that does not.

```smalltalk
"the interface is Smalltalk calling AppKit; the environment is a VM behind it"
CocoaUI window: (Workspace on: 'Transcript showCr: 3 + 4').
CocoaUI window: (ClassBrowser on: Integer).
"...then Rust calls [NSApp run]; events dispatch into these objects."
```

## 1. Three VMs, three tiers — and which thread each lives on

The decisive design fact is the **thread/role assignment**. AppKit forces
one thing: the interface runs on the process's main thread. Everything else
is a choice, and the right choice keeps the *persistent brain off the main
thread* so it can neither freeze the UI nor take the process down with it.

| Tier | Thread | Role | Risk profile |
|---|---|---|---|
| **UI worker VM** | **main (pinned by AppKit)** | a *dumb terminal*: builds native views, answers AppKit delegate/data-source calls from a **local snapshot**, forwards events. Quiescent between callbacks. | thin, low-risk; mostly per-event recoverable |
| **Primary ST VM** | **2nd (background)** | the **persistent state**: live objects, classes-being-edited, workspace vars; boots the world, **registers the UI worker**, spawns compute workers, runs doits, blasts snapshots | the real logic; **restartable** because it is not the main thread |
| **Compute worker VMs** | other background | parallel work spawned by the primary | isolated; die+respawn (M0–M4) |

Two moves distinguish this from the naïve "VM on main" arrangement:

1. **The UI is a *worker*, not the primary.** The persistent VM — where your
   code and objects live — is a background thread. The main thread hosts a
   second VM whose only job is display, pinned to main only because Cocoa
   requires it. The interface is demoted from *the brain* to *a display
   device on a specific thread*.
2. **The UI worker is a *dumb terminal* (`feedback_blast_dont_patch`).** It
   holds a **local snapshot** of what each view should show; it renders that
   snapshot and forwards events; it never holds authoritative state and never
   patches incrementally. When the model changes, the primary *blasts a whole
   fresh snapshot* and the terminal re-renders.

Everything below follows. Three consequences are load-bearing and get honest
treatment: **the crash model is recovered (§5)**, **the UI never freezes on
model work (§6)**, and **delegates are answered synchronously from a local
snapshot without a cross-thread wait (§4)** — the last of which is *why* the
terminal holds a snapshot.

## 2. Why this shape (and why "VM on main" was wrong)

An earlier draft put the *persistent* VM on the main thread and called it
"the UI." That is the obvious arrangement and it is wrong, for two reasons
this shape fixes:

- **Crash safety.** A fatal doit (`error:`, DNU, heap exhaustion, a native
  fault) ends in `fatal_exit`. On the main thread the only options are
  `pthread_exit` (tears the process down) or a blind `siglongjmp` through
  AppKit frames (undefined behaviour — the reason S21 rejected unwinding
  through JIT frames). So "VM on main" *loses* the S21 restart-on-death
  model. With the persistent VM on a **background** thread, a fatal doit
  `pthread_exit`s that thread, a supervisor respawns it, and the UI terminal
  shows "reconnecting" and re-syncs — **S21's model, recovered (§5).**
- **Responsiveness.** If the persistent VM is the main thread, a long doit
  freezes the UI. With doits on the background primary, a long doit blocks
  the primary — not the paint loop — so the UI stays live (§6).

The interface must be on main; the *environment* must not be.

## 3. Boot — the run loop is Rust's; the callbacks are Smalltalk's

The two modes share the crate, the world/image, the Cocoa bridge, the
GamePane, and `objc::bootstrap()` (factored out of the `gui` crate so both
bins call it — §12). `fn main()` dispatches on the first CLI arg; Cocoa mode
is one more arm:

```
macvm-gui               → WKWebView mode (default, unchanged)
macvm-cocoa             → Cocoa mode (a dedicated bin in the cocoa_gui crate)
```

The chicken-and-egg — *the primary registers the UI worker, but the UI
worker must run on the main thread the process started on* — is resolved by
booting the UI worker VM **in place on main** and letting Rust own the run
loop. The critical correction from review (R1/item-1): **`[NSApp run]` is
called by the Rust `cocoa_gui` glue with the UI worker VM at rest, never from
inside a Smalltalk doit.** The VM is quiescent whenever AppKit calls back, so
every callback is a *top-level* `VmHandle` entry (the existing `eval` door),
not a re-entrant call into a live `&mut VmState`.

Boot sequence:

1. **(main)** `objc::bootstrap()` — `NSApplication`, activation policy. (Menu
   bar is built later, in Smalltalk — §7/R5.) AppKit init must be on main.
2. **(main)** Spawn the **primary VM on a background thread** (the S21
   supervisor owns this thread — §5). Park main on a channel awaiting the
   primary's "UI ready" signal (the primary has booted its world and is
   serving).
3. **(background)** The primary boots the world (persistent state), then
   **registers the UI worker as an externally-hosted peer** (a new
   `workers.rs` surface, R-item9): a worker id + a real inbox
   `Sender<Envelope>` + a **run-loop wake** (a `performSelectorOnMainThread`
   poke, R-item8), *without* spawning a thread — the UI worker's thread is
   main, which already exists. It sends main the boot payload.
4. **(main)** Boots the **UI worker VM in place** (own heap, `FatalMode::
   ExitProcess` — R-item6, *not* `boot()`'s default `ExitThread`, which on
   main is a headless zombie), publishes a **thread-local `*mut VmHandle`**
   the callback trampolines read (R1), then runs the Smalltalk startup doit
   `CocoaUI startup` — builds the menu bar, the initial windows (a Workspace
   + a Transcript), installs their C6 delegates, orders them front — and
   **returns to Rust**.
5. **(main, Rust)** Calls `[NSApp run]`. The VM is now at rest. Each AppKit
   event dispatches through a C6/C4 trampoline as a *top-level* entry into
   the UI worker (§4); each inbox drain (snapshots, doit replies from the
   primary) runs as a top-level entry scheduled on the run loop in **default
   mode only** (§8/R-item12).

`enable_main_hop()` (C3) is not used the WKWebView way (no Rust-owned main to
hop to). Neither VM's live objects are persisted (no running-image save, by
design); each launch rebuilds the windows by re-running `CocoaUI startup`,
exactly as the world boots from source. Durable UI prefs (window frames) are
data (a plist / image_store table), never pickled views.

## 4. C6 — reverse dispatch: delegates answered from the local snapshot

Everything an IDE does past "click a button" is a **delegate or data source**
— `NSTableViewDataSource` answering `numberOfRowsInTableView:`,
`NSOutlineViewDataSource` answering `outlineView:child:ofItem:`,
`NSWindowDelegate` answering `windowShouldClose:`. These are **synchronous
calls from AppKit that expect a return value now**, to paint. C4's
`MacvmAction` is fire-and-forget (void); it cannot answer a data source. C6
is the mechanism that can.

### 4.1 Why "answer from a local snapshot" is the crux (review: SOUND)

AppKit calls the data source **synchronously on the main thread** and blocks
until it returns. The authoritative model lives on the **primary
(background) VM**. If the delegate had to *ask the primary* per row — a
synchronous main→primary wait — a busy primary would freeze the paint, and a
primary waiting on the UI would deadlock (the exact wait cycle the worker
threading model forbids). The dumb-terminal design dissolves it: the UI
worker answers **from its own local snapshot**, on its own thread, with no
wait. `numberOfRowsInTableView:` is `^snapshot rowCount` — instant, local.
This is *why* the terminal holds a snapshot.

### 4.2 The mechanism — a registered-IMP table (review: chosen over forwardInvocation)

Both reviews recommended a **registered-IMP table over `forwardInvocation:`**,
and the inventory is decisive: G2–G5 need ~13 delegate/data-source selectors
across ~8 type shapes, all already in the bridge's `classify_arg`/
`classify_ret` vocabulary (id / NSInteger / BOOL / NSRange). `forwardInvocation:`
would need those same 8 shapes *plus* NSInvocation get/set-argument plumbing
*plus* three registered override IMPs (`respondsToSelector:`,
`methodSignatureForSelector:`, `forwardInvocation:`) *plus* a per-instance
allow-list — strictly more code, slower on the per-cell paint path, for
generality an IDE does not need.

So C6 is a **small family of per-role ObjC delegate classes**, each with
exactly the selectors that role answers — the `MacvmAction` pattern
(`objc_bridge.rs`, `macvm_action_class`) and the WKWebView menu targets
(`gui/src/main.rs`, `add_method(cls, sel("selectTheme:"), …, "v@:@")`)
generalized:

- `MacvmWindowDelegate` — `windowShouldClose:` (`B@:@`), `windowWillClose:`
  (`v@:@`), and the app-lifecycle pair `applicationShouldTerminate:`
  (`Q@:@`) / `applicationShouldTerminateAfterLastWindowClosed:` (`B@:@`).
- `MacvmTextDelegate` — `textDidChange:` (`v@:@`).
- `MacvmTableSource` — `numberOfRowsInTableView:` (`q@:@`),
  `tableView:objectValueForTableColumn:row:` (`@@:@@q`),
  `tableViewSelectionDidChange:` (`v@:@`).
- `MacvmOutlineSource` — `outlineView:numberOfChildrenOfItem:` (`q@:@@`),
  `child:ofItem:` (`@@:@q@`), `isItemExpandable:` (`B@:@@`),
  `objectValueForTableColumn:byItem:` (`@@:@@@`),
  `outlineViewSelectionDidChange:` (`v@:@`).

Because each class carries only its own selectors, `respondsToSelector:` is
**natively correct** — optional delegate methods stay optional with no
allow-list. Each IMP is one typed `extern "C" fn` that: reads the
ABI-delivered arguments, unmarshals each with `classify_arg` (native→Smalltalk
oop), looks up its bound Smalltalk delegate object in the C6 registry, runs
`delegate perform: selector withArguments: args` — **the reflective primitive
built for the worker RPC, reused verbatim** — and marshals the result back
with `classify_ret`. Each runs on the **main thread as a top-level VM entry**
(R1): the trampoline reads the thread-local `*mut VmHandle`, calls
`eval`/`perform` through the existing door (which already carries the
per-entry `sigsetjmp` guard — §5), and returns to AppKit.

`forwardInvocation:`/NSInvocation is deferred to an appendix, for the day an
arbitrary-protocol delegate is actually needed.

### 4.3 The C6 registry — instance→ticket, VM-tagged

One process-wide registry maps an ObjC delegate instance pointer → a ticket
naming the UI worker's Smalltalk delegate object (a GC root; no oop ever
stored ObjC-side — the §2 contract holds). Entries carry a **generation/VM
tag** (R-item4): after a UI-worker restart (§5) a stale delegate instance
from a not-yet-closed window fails *closed* — the IMP returns a defined
default (0 rows, `nil`, `NO`) rather than dispatching into a dead VM.
Delegates are **long-lived** (built outside any `poolDo:`, released in the
teardown of §5), so the mint path must work from a *Worker* VM — which C4's
`prim_cocoa_new_action` currently refuses (R-item5); that gate is lifted for
the UI worker, routing through the Worker's own `to_primary`/registration
sender rather than `primary_inbox_sender`.

## 5. Crash recovery — three layers, all reusing the top-level-entry door

Because callbacks are top-level VM entries (§3/R1), recovery reuses the S21
machinery instead of needing new nested-recovery machinery. Three layers,
split by *where the fault happened*:

**Layer 1 — a recoverable error, or a fault in *our* code, inside a
callback** (a delegate doit that sends `error:`; a bridge-marshalling bug; a
bad `Alien`). The callback is a top-level `eval`/`perform` entry, so it
*already* has the per-entry `sigsetjmp` guard the S21 `VmHandle` door
installs (`embed.rs`). PROBE redirects `SIGSEGV`/`SIGBUS` too, not only guest
`error:`. The entry unwinds its own frames back to the trampoline, which
returns a **defined default** to AppKit (0 rows / `nil` / `NO` per the
selector's return shape — the delegate contract must never leave the ObjC
return slot undefined), reports to the Transcript, and the same run loop
keeps pumping. **No new `FatalMode` variant is required for this** — it is
the existing `eval` recovery, at the callback door. (The earlier draft's
`FatalMode::AbortToEventLoop` is dropped; review R-item5 showed a *nested*
recovery mode would be unsound given one jmp slot per thread, and the
top-level-entry decision removes the need for it.)

**Layer 2 — a fault *inside* AppKit** (framework code dereferenced something
bad; its internals are now suspect). Don't limp on a suspect run loop —
**restart the GUI in place.** But the review corrected *how*: the in-place
path, unlike a `pthread_exit` worker death, **can and must cleanly Drop the
old UI worker `VmHandle`** (R-item3a). The handle is kept in a slot *outside*
the top-level recovery `sigsetjmp` scope; the recovery handler:
1. **Tears down the old UI cleanly first** (R-item3b): `orderOut:` + `close`
   every old window, nil their delegates, invalidate the old C6/C4 registry
   tickets, and **retire the old UI-worker peer registration** on the primary
   (so its dead inbox stops swallowing snapshot blasts).
2. **Drops the old `VmHandle` normally** — `Reservation::drop` munmaps the
   heap (committed pages and all), `CodeCache` runs `deopt_trap::deregister`,
   the thread's setjmp slot is released. This is essential, not optional:
   leaking instead hits `REGISTRY_CAP = 128` in PROBE's registry and
   **panics on the main thread after ~128 lifetime restarts**. Clean teardown
   is reachable here (it is not on the `pthread_exit` path), so the design
   takes it.
3. **Boots a fresh UI worker VM** on main, re-registers it with the primary,
   re-runs `CocoaUI startup`, re-syncs snapshots, returns to Rust, re-enters
   `[NSApp run]`. `NSApplication` (the process singleton) is kept, not
   recreated.

Lossless, because the UI worker holds **no durable state** — the user's work
is on the primary, on its own thread, untouched.

**Layer 3 — process-global AppKit corruption** (a broken window-server
connection, a global lock left held) survives a rebuild and re-crashes.
Bounded by a **restart-loop backstop**: N rebuilds within T seconds → write
the dossier, `ExitProcess`. Honest give-up, never a silent infinite
re-crash. (Honesty note the review demanded: re-entering `[NSApp run]` after
a mid-`run` `siglongjmp` across the live dispatch stack rests on
`NSApplication` being near-stateless at the top; this is asserted, not
proven, and is the reason Layer 3 exists as a bounded fallback rather than a
guarantee.)

**Fatal on the primary** (a fatal doit) → `pthread_exit` of the background
thread → the S21 supervisor (§5.1) respawns it from source → the UI terminal
shows "environment restarting…" and re-syncs. The persistent object graph
that was mid-mutation is lost with the dead VM (an honest clean loss, not a
fake rollback — `feedback_recover_clean_or_die`); everything else rebuilds
from source/image_store.

**Two prerequisite infra fixes the review found, without which none of the
above is trustworthy (§9.1):**
- **Per-thread signal alt-stacks.** Today every thread's `sigaltstack`
  points at one shared static `ALT_STACK` (`deopt_trap.rs`) — a
  documented "one VM on one thread" assumption already stale under workers,
  and this design makes concurrent faults (primary + a UI callback) *likely*;
  two handlers on one aliased stack corrupt each other. Per-thread alt-stacks
  are a prerequisite.
- **`FatalMode` on main = `ExitProcess`.** `boot()` unconditionally arms
  `ExitThread`; on the main thread a true fatal (heap exhaustion, stack
  overflow) would `pthread_exit` main and leave a headless zombie. The UI
  worker boots with `ExitProcess`.

### 5.1 The supervisor (review: unspecified — resolved)

The S21 supervisor lives today in `VmHost::submit` *on the main thread* — the
thread this design repurposes. It moves to the **thread that spawns the
primary** (§3 step 2): that thread stays alive as a thin watchdog, owns the
primary's `JoinHandle`/death signal, and on primary death respawns it and
notifies the UI worker (a snapshot-invalidate). It is not the main thread and
not the primary, so it can outlive either.

## 6. Responsiveness — the UI stays live because the brain is elsewhere

A long or infinite doit blocks the **primary (thread 2)**, not the main
thread. The UI worker keeps painting, scrolling, accepting input, and can
show a spinner. A Workspace `doIt` **ships its selection source to the
primary** (a request — §7.3; the doit runs where the persistent objects
live), and the primary replies the result. Short doits round-trip fast; long
doits run on the primary while the UI stays responsive, and the primary can
*further* dispatch heavy work to **compute worker VMs** (`call:on:args:
onReply:`, the RPC just built) so it too stays available. The framework
offers this as one line — `aView do: source then: [:r | …]` — so "the
interface never blocks" is the default path. Replies and snapshot blasts
drain on the main thread in **default run-loop mode only** (§8).

## 7. The views — models on the primary, snapshots to the terminal

The web GUI's views are `Visual` subclasses in the image with real *model*
logic (`ClassHierarchyOutliner`, `ClassOutliner`, `ImplementorsView`,
`SendersView`, `DefinitionsView`, `CodeView`, plus `Workspace`, `CodeEditor`,
`ClassMirror`; `world/34_tools.mst`, `60_editor.mst`). The model logic runs on
the **primary** (it is the environment's knowledge of itself) and answers
`htmlFragment` today. What crosses to the UI worker is a **snapshot**.

### 7.1 The snapshot rule (review: CONFIRMED trap — snapshots are names, not oops)

The pickle **refuses classes, methods, closures, contexts** (`mop.rs`), and
the view models hold **real klass oops** (`ClassMirror` wraps a `Klass`;
`ClassHierarchyOutliner` holds a `rootMirror`). So a snapshot can **never** be
"pickle the mirror" — it dies at the first slot. The rule, stated explicitly:

> **A snapshot is a plain nested tree of Strings, Symbols, and SmallIntegers
> — projecting class/method *names*, never class or method oops.** This is
> exactly what `htmlFragment` already emits (`mirror name`, not the mirror).

Concrete shape (all pickle-safe — nested `TAG_ARRAY`/`TAG_OBJECT` of
String/Symbol/smi; the 64 MB / 1 M-object caps are ample):

```
{ #snapshot. viewId. generation. dataTree }
```

`viewId` routes the blast to the right terminal-side view; `generation` is a
monotonic staleness guard (a terminal drops a snapshot older than the one it
holds). `dataTree` is per-view: for a browser, a nested Array of
`{ nodeName. childArray }`; for a table, an Array of row Arrays of strings.

**Outline item identity (review: unaddressed — resolved).**
`outlineView:child:ofItem:` must hand AppKit an ObjC `id` it will pass back
later as `item`. The UI worker mints a **stable native node handle** — an
`NSString` (or `NSNumber`) keyed by the node's *path* into the snapshot tree
— and keeps a terminal-local path↔handle map for the current generation;
`child:ofItem:` and `isItemExpandable:` resolve the handle back to a snapshot
node. Handles are per-generation and discarded on re-blast.

### 7.2 The view wrappers (terminal-side, `world/<NN>_cocoaui/`)

Thin AppKit wrappers, each with its C6 delegate answering from the last
snapshot: **Workspace** (`NSTextView`; ⌘D/⌘P ship the *selection* to the
primary — text is local to the NSTextView, §11 Q4/R9); **Transcript**
(append-only `NSTextView`; §7.4); **ClassBrowser** (`NSOutlineView`/
`NSBrowser` data-sourced from a primary-built snapshot; select a method →
source in a CodeView); **CodeView** (`NSTextView` + Smalltalk syntax colour
via `NSAttributedString` ranges — the marshalling top-up; Save ships source
to the primary to recompile + image_store-write, the web edit path); **Find
views** (`NSTableView`s over primary-built snapshots, accurate via the
persisted `method_sends` table); **GamePane** (§7.5).

### 7.3 The request protocol (review: GAP + corr-collision hazard — resolved)

The UI→primary vocabulary the design was silent on, with the review's
corr-collision fix. `PendingReplies` is keyed by `corr` alone and each VM
runs its own `NextCorr`, so a UI-worker-initiated request with corr *c*
arriving at a primary holding its own outstanding corr *c* fires the wrong
continuation. Fix: **tagged requests routed by shape *before* the corr
match**, and corr namespaced by originating peer.

- UI→primary: `{ #uiReq. corr. #doit. source }`, `{ #uiReq. corr. #refresh.
  viewId }`, `{ #uiReq. corr. #select. viewId. path }`, `{ #uiReq. corr.
  #saveMethod. className. side. selector. source }`.
- primary→UI: `{ #uiReply. corr. payload }` (doit results) and `{ #snapshot.
  viewId. generation. dataTree }` (unsolicited blasts, corr-free).
- Primary-side: `dispatchOne:` grows an `isUiReq:` branch that captures
  `CurrentCorr` and dispatches by verb; `reply:` works primary-side (today it
  is worker-side only). Corr keys become `(peer, corr)` so the two VMs'
  independent counters cannot collide.

### 7.4 The transcript sink (review: SOUND — existing machinery, direction-flipped)

`ForwardTranscript` (`workers.rs`) already turns a VM's `vm.out` into
`{ #workerTranscript. id. text }` envelopes through the inbox, line-tagged,
and `dispatchOne:` displays them. The primary→UI transcript sink is this
exact struct with the destination flipped to the UI worker's inbox (~15
lines) — a `TranscriptSink` (`embed.rs::set_transcript`) whose per-line wake
lands in the terminal's Transcript view. Named as reuse, not new.

### 7.5 GamePane (review: render side SOUND, drive side is new)

The **render** half transfers unchanged: `NativePane` is a main-thread
`thread_local` with `apply_command`/`present_if_dirty`, independent of
WKWebView. The **drive** half does *not* "drop in": today's 60 Hz `NSTimer`,
the single-outstanding backpressure gated on `VmHost::is_idle`, and
`ChannelGameSink` emitting over the WKWebView response channel are all
VmHost-shaped. Cocoa mode rebuilds the drive: a main-thread timer the UI
worker owns, a GameStep-as-RPC to the game's VM with corr-id backpressure,
and a command path to the native pane (a dedicated Rust channel + run-loop
wake, **not** 60 fps of MOP-pickled doits — the cadence is a real question
flagged for the sprint). §9 lists this as new, not reused.

## 8. Workers, the wake, and the drain

Multi-VM is unchanged in shape (the primary is the star centre; UI worker and
compute workers are leaves), with three review corrections:

- **The wake in the primary→UI direction is NEW (not "M3 reused").** M3's
  coalesced wake is a *Primary*-inbox-only mechanism; the primary→worker link
  is a bare `Sender` with no hook (workers wake by being blocked in
  `recv()`). The UI worker is blocked in `[NSApp run]`, not `recv()`, so its
  inbound inbox needs a **new `InboxSender`-style wrapper whose wake is a
  run-loop poke** — the `performSelectorOnMainThread` pattern that today
  lives in `gui/src/vm_host.rs` (`CrossThreadObjcRef::notify`), promoted into
  `workers.rs` as the externally-hosted-worker's wake (§9.1).
- **The drain is default-mode-only (review: GAP).** A common-modes
  `performSelectorOnMainThread` fires inside AppKit's *nested* run loops
  (menu tracking, live resize, modal sessions) — swapping a snapshot
  mid-tracking (row count changing under a live table drag) throws AppKit
  consistency exceptions. The drain schedules in **`NSDefaultRunLoopMode`
  only** (or a `CFRunLoopSource` added to default mode), and the design
  accepts that **the UI is intentionally stale during a tracking session**,
  re-syncing when it ends.
- **Only the UI worker may touch AppKit, enforced by a new guard (review:
  the guard does not exist yet).** `cocoa_bridge_design.md` §7 explicitly
  ships no in-bridge main-thread guard. This design adds one: an
  AppKit-prefixed send from a non-main VM fails loudly (a curated
  class-prefix list). Listed as new work (§9.1).

## 9. Reused vs. genuinely new — corrected against the reviews

### 9.0 What the reviews changed

Both reviews independently found the keystone: **enter `[NSApp run]` from
Rust with the VM quiescent**, making every callback a top-level entry (R1/
item-1). That correction cascaded — it eliminated the need for a nested
`FatalMode::AbortToEventLoop`, made the per-callback recovery the existing
`eval` door, and removed the aliased-`&mut VmState` hazard. The reviews also
downgraded several "reused wholesale" claims to "needs generalization" (the
wake, C4, GamePane drive, the supervisor) and found real traps (snapshots
can't carry class oops; in-place restart must Drop or panic at 128; the
corr-collision; the shared alt-stack; `boot()`'s main-thread FatalMode).

### 9.1 Genuinely new

1. **C6 reverse dispatch** — the per-role `MacvmDelegate` class family +
   typed IMPs + the VM-tagged instance→ticket registry, all reusing
   `perform:withArguments:` + C2 marshalling. `src/runtime/objc_delegate.rs`
   (**in core**, beside `MacvmAction` — resolves the §12 placement
   contradiction) + world methods.
2. **Prerequisite signal-infra:** per-thread `sigaltstack`s (replace the
   shared `ALT_STACK`); the UI worker boots `FatalMode::ExitProcess` on main.
3. **The externally-hosted worker + its run-loop wake** — a `workers.rs`
   surface to register a worker on an *existing* thread (no `thread::spawn`),
   with a wake that pokes the run loop. (Not "localized to gui/embed".)
4. **The UI→primary request protocol** (§7.3) + primary-side `isUiReq:`
   dispatch, primary-side `reply:`, and `(peer, corr)` namespacing.
5. **In-place UI restart** — the top-level recovery point, the ordered
   teardown (windows/delegates/tickets/link), the clean `VmHandle` Drop, and
   the N/T backstop (§5 Layer 2/3).
6. **The AppKit main-thread guard** (§8) — the curated class-prefix refusal.
7. **The GamePane drive path** (§7.5) — timer + backpressure + native-pane
   command channel.
8. **A modest marshalling top-up** — `NSAttributedString` ranges, `NSColor`/
   `NSFont` — for the CodeView (grown row-by-row).
9. **The two-sided Smalltalk GUI framework** — primary-side `snapshotFor:` +
   blaster; terminal-side `world/<NN>_cocoaui/` wrappers + the menu bar
   (built in Smalltalk — R5). The bulk of the typing, the least of the risk.
10. **The `macvm-cocoa` bin + the parked-main boot** (§3), and the
    **conditional world layer** — a small `load_list(vm, path)` public fn +
    `world/cocoaui.list`, loaded only by the UI worker (R7; use file numbers
    **63+** — 51–62 are taken).

### 9.2 Reused (verified)

The Cocoa bridge C0–C5; `perform:withArguments:` (the delegate dispatch
core, verified real); the C2 `@encode` marshalling (both directions); the
`VmHandle` boot + the S21 sigsetjmp per-entry recovery (now the callback
door) + the supervisor pattern (re-homed to the watchdog thread); the
multi-VM star + the pickle (snapshots ride it, projecting names) + the RPC;
`ForwardTranscript` (the sink, direction-flipped); the image/world +
`image_store`; every view *model*; the GamePane *render* half; N-heap
coexistence (proven by the worker fleet).

## 10. Milestone ladder (each lands green and alone)

| | Deliverable | Proof |
|---|---|---|
| **G0** | Prerequisite infra: per-thread alt-stacks; the externally-hosted worker + run-loop wake; `macvm-cocoa` bin + `objc::bootstrap` factored out; boot the primary (thread 2) + the UI worker on main (`ExitProcess`), one empty `NSWindow` built in Smalltalk, Rust `[NSApp run]`, ⌘Q clean | on-screen (user); headless: the two-VM boot + the externally-hosted-worker registration + a wake delivered to a parked thread; alt-stack per-thread test |
| **G1** | **C6**: the per-role delegate class family + typed IMPs + the VM-tagged registry, dispatched as top-level entries; Layer-1 recovery (a bad delegate doit / a forced native fault in a callback returns a defined default, run loop pumps on) | headless: a delegate `perform:` returns a value AppKit-side (proxied); a raising delegate returns the default + the next event dispatches; a forced `SIGSEGV` in a callback recovers |
| **G2** | The request protocol (§7.3) + `ForwardTranscript`-flipped sink; Workspace + Transcript: ⌘P ships selection → primary evaluates → `7`; **kill the primary mid-doit → the watchdog respawns it, UI shows "restarting" and recovers** | on-screen; headless: the `#uiReq`/`#uiReply` round-trip + `(peer,corr)` non-collision; the primary-restart re-sync |
| **G3** | ClassBrowser: `NSOutlineView` data-sourced from a **names-only** snapshot (`{#snapshot. viewId. gen. tree}`) with the outline-handle scheme; select class → methods → CodeView source | on-screen; headless: the data-source callbacks answer the same rows the `htmlFragment` model does (differential vs. the web model), snapshot pickles clean (no class oop) |
| **G4** | CodeView editing (syntax colour, Save → primary → recompile + image_store, the web edit path); Find views as `NSTableView`s; **UI-worker restart-in-place on a forced AppKit-internal fault** (teardown + clean Drop + reboot; the N/T backstop) | on-screen; headless: an edit round-trips through image_store byte-identically; a scripted restart Drops the old handle (no reservation/PROBE-registry leak) and reboots |
| **G5** | `do:then:` worker bracket; a heavy doit on the primary / a compute worker updates a view while the UI stays live; GamePane as a window (the new drive path + backpressure); default-mode-only drain verified under tracking | on-screen: UI responsive during a parallel-Mandelbrot dive driven from a native window; a menu-tracking session does not corrupt a live table |

G0–G2 hold the real design risk (the infra, top-level-entry dispatch, the
recovery doors, the request protocol, and — the payoff — proving both
restart paths). G3–G5 are mapping over a proven base. On-screen verification
is the user's; headless gates cover the model/marshalling/recovery/protocol
seams (`feedback_gui_visual_verification`).

## 11. Non-goals, residual risks, open questions

**Non-goals (v1):** Interface Builder / nib loading; full Auto Layout (start
springs/struts); persisting live window objects (rebuilt from source; only
prefs persist); replacing the WKWebView GUI.

**Residual risks (post-review):**
- **A native framework fault is a main-thread fault.** Layer-1 catches faults
  in *our* code per-event; Layer-2 rebuilds on an AppKit-internal fault; the
  irreducible risk is **process-global AppKit corruption** that survives a
  rebuild, bounded by the N/T backstop. Highest-stakes class; the reason the
  terminal is deliberately thin.
- **Re-entering `[NSApp run]` after a mid-run `siglongjmp`** rests on
  `NSApplication` being near-stateless at the top — asserted, not proven;
  Layer 3 is the bounded fallback for when it isn't.
- **Snapshot cost.** Blast-don't-patch re-sends unchanged data; per-view,
  changed-view-only blasting keeps it cheap; measure before diffing (the
  editor's incremental patching is the cautionary tale).
- **UI is intentionally stale during tracking/modal sessions** (§8) — a
  correctness *choice*, not a bug, but a visible behaviour.

**Open questions for the build:**
1. Snapshot granularity — per-view coarse trees (start here) vs. a shared
   terminal-side model cache.
2. GamePane cadence — a dedicated Rust command channel to the native pane
   (favoured) vs. pickled step-doits at 60 fps (§7.5).
3. Does the UI worker run *any* IDE logic locally, or is it purely a terminal
   answering snapshots? Start purely-terminal; add local fast paths only where
   round-trip latency is felt (the Workspace's NSTextView already owns its
   text locally, which is text *storage*, not IDE logic).

## 12. Packaging — same repo, a gated workspace crate (not a fork)

The change is big, which tempts a separate repo. It should not be one; the
honest test is coupling and boundary, not size, and the Cocoa GUI fails the
fork test: its load-bearing parts (the recovery changes, the boot handshake,
C6, the reverse marshalling, the signal-infra) are **core-VM changes that
must live in the `macvm` crate**; the view *models* are shared `world/*.mst`;
and none of the fork justifications hold (no independent release cadence,
consumers, or interface boundary — unlike the MacGamePane sister repo, which
*has* a clean producer/consumer boundary). A fork would also forfeit the
decisive property: **one core change tested against both GUIs in a single
`cargo test`.**

The repo is already a cargo workspace (`.` = core `macvm`, `gui` = the
WKWebView bin depending on `macvm`, plus `image_store`/`ffi_gen`/…). The right
modularity is a **workspace member + gates**:

1. **A new `cocoa_gui` workspace crate** for the AppKit Rust glue (the
   `[NSApp run]` owner, the delegate-class registration, the boot handshake,
   the `macvm-cocoa` bin), depending on `macvm` as `gui` does. It also
   inherits the GamePane path-deps (`macgamepane-graphics`/`-audio`, `metal`)
   and the `game_pane.rs` glue — factored from `gui`, not duplicated. A
   default root `cargo build`/`test` never links it.
2. **Core changes inert-when-off.** The C6 `objc_delegate` module (core,
   beside the Cocoa bridge) compiles but is unreachable unless a delegate is
   created; the per-thread alt-stack and `ExitProcess`-on-main changes are
   unconditional soundness fixes that help every mode. Optionally a `cocoa`
   cargo feature gates the delegate module out of the default lib entirely.
3. **A conditionally-loaded world layer** — `world/cocoaui.list` (files 63+),
   loaded only by the UI worker via the new `load_list` fn, so the CLI, the
   WKWebView GUI, and the test suite carry none of it.

Zero-cost when off, one cohesive repo where a core change cannot silently
break the second GUI.

## 13. Relationship to the existing docs

Sits on top of, and does not re-open: `cocoa_bridge_design.md` (C0–C5),
`multi-smalltalk-worker.md` (the star, the pickle, the RPC), `docs/vm_handle.md`
/ S21 (the boot, the sigsetjmp recovery re-homed to the callback door, the
supervisor re-homed to the watchdog thread), `gamepane_design.md` (the render
half), and the `perform:withArguments:` RPC. It is where the Cocoa bridge
stops being a way to *call* macOS and becomes the substrate the environment
is *built from* — with the persistent environment kept safely off the main
thread, so the interface can be Smalltalk without the environment being
hostage to it.
