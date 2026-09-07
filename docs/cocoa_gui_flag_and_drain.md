# Flag-and-drain — how `cocoa_gui` touches VM-level state safely

Status: implemented, in force since the CG9 UI-worker-rebuild work (`8266381`,
`22a0daf`) and extended several times since (most recently `47d0758`,
`56a9aad`). This document is the authority for *why* the mechanism exists,
*how* it's shaped, and *when* a new piece of Cocoa-GUI code needs to use it.

§1–6 are the mechanism that keeps VM work out of callbacks. **§7 is its other
half** — the rule that stops the drain re-entering itself, which is what the
GUI's display-server aborts turned out to be.

## 1. Motivation

The UI worker VM runs pinned to the main thread (AppKit requires it). Every
AppKit callback that reaches Smalltalk — a button click, a menu action, a
table's `numberOfRowsInTableView:`, a text view's delegate method — does it
through **C6 reverse dispatch**: ObjC calls into Rust
(`objc_delegate::dispatch_callback`), which runs the corresponding Smalltalk
handler as a *nested* entry into the same VM that's already active on that
thread.

That's fine for cheap, self-contained work (compute a row count, answer a
string). It breaks for anything that needs to touch state living **outside**
the callback's own nested activation:

- **Replace the VM itself.** A UI-worker rebuild drops the current
  `VmHandle` and boots a fresh one. Dropping a `VmHandle` from inside a
  callback *running on that same VmHandle* is unsound — you'd be tearing
  down the ground you're standing on.
- **Reach `DrainState`.** The `PrimarySupervisor`, the hosted inbox, the
  reply link — all owned by `main.rs`'s `DrainState`, which a static
  `extern "C"` IMP has no pointer to at all. There is no path from "a menu
  item fired" to "call a method on the supervisor" that doesn't go through
  something that owns `DrainState`.
- **Query + rebuild a data-backed view.** A DB query followed by
  `NSOutlineView reloadData` / `NSComboBox addItemWithObjectValue:` looks
  harmless, but `reloadData` synchronously fires the view's *own* C6
  data-source callbacks — a second nested VM entry, stacked inside the
  first.

Two real, reproduced failure modes came out of getting this wrong, both in
this codebase's own history:

1. **Fails closed, silently.** The nested callback's own VM entry either
   never runs (the query and the reload happen, but nothing visible
   changes) or runs against inconsistent state. Symptom: "the browser shows
   no data" / "the Find buttons don't do anything at all" — `47d0758`
   reproduced this precisely for `CocoaFind`'s three query buttons: the
   query ran and the underlying `Rows` data was correct, but
   `Results reloadData`, called from inside the button's own callback,
   never made it to screen.
2. **Crashes the process.** Once the Smalltalk method doing the nested work
   gets JIT-compiled, the interpreter's tier-link bookkeeping
   (`walk_frames`, `TierLink`) can be entered in a shape it was never
   designed for from a compiled caller, and a GC landing at the wrong
   moment aborts the whole process (`987e6a5`'s root cause). This one only
   shows up after enough repeated uses to cross the JIT threshold, which is
   exactly what made it look intermittent when it was first reported.

So the rule, applied everywhere in `cocoa_gui`: **a callback never does the
work itself. It only sets a flag and wakes the run loop.** The actual work
runs later, on the main thread, outside any callback — the one place it's
sound to drop a `VmHandle`, reach `DrainState`, or run a query-and-reload
pair.

## 2. The three-part shape

**A flag.** A tiny dedicated module — usually one `AtomicBool`, sometimes a
few side by side — with a `request()` and a `take_request()`:

```rust
static REBUILD_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Request a UI-worker rebuild (idempotent until serviced).
pub fn request() {
    REBUILD_REQUESTED.store(true, Ordering::Release);
}

/// True (once) if a rebuild was requested since the last check — clears it.
pub fn take_request() -> bool {
    REBUILD_REQUESTED.swap(false, Ordering::AcqRel)
}
```

`request()` is the only thing a callback (or a host-service IMP, or the TCL
control channel) is allowed to call. `take_request()` is called exactly
once per drain pass, from `drain_perform` — nowhere else.

**A wake.** [`objc::wake_main_runloop`](../cocoa_gui/src/objc.rs) signals a
`CFRunLoopSource` (`CFRunLoopSourceSignal`) and pokes the run loop
(`CFRunLoopWakeUp`). Both are documented thread-safe, so this is callable
from *any* thread — a C6 callback on the main thread, or a background
thread noticing a primary VM message arrived.

**A drain.** [`drain_perform`](../cocoa_gui/src/main.rs) — installed once at
startup via `objc::install_default_mode_drain` as a `CFRunLoopSource` in
**`NSDefaultRunLoopMode` only**, deliberately not the common modes: a
common-mode source would also fire inside AppKit's own *nested* run loops
(menu tracking, live window resize, a modal panel), and swapping VM-backed
state mid-tracking throws AppKit consistency exceptions. The UI is allowed
to go briefly stale during a tracking session; `drain_perform` catches it up
the instant the loop returns to default mode.

`drain_perform` owns `DrainState` (the `VmHandle`, the `PrimarySupervisor`,
the hosted inbox) and runs *only* on the main thread, *never* nested inside
a callback — the one place every flag above gets serviced for real.

## 3. Worked example — "Rebuild UI", start to finish

1. User clicks **Debug ▸ Rebuild UI**. AppKit invokes the item's
   target-action — a C6 callback.
2. The callback runs `CocoaUI requestRebuild` (Smalltalk), which sends the
   ObjC message `requestUiRebuild` to `MacvmHostService`.
3. That resolves to `host_service::imp_request_ui_rebuild` — a static IMP
   with zero captured state — which calls `rebuild::request()` (sets the
   flag) then `objc::wake_main_runloop()`. **That's the whole callback.**
   Nothing VM-level has happened yet; the callback returns immediately.
4. The main run loop returns to default mode and fires `drain_perform`.
5. First line of real work: `if rebuild::take_request() { rebuild_ui(st);
   return; }` — reads and clears the flag, then does the actual teardown
   (release every AppKit wrapper the old VM held, drop the old `VmHandle`)
   and rebuild (fresh `VmHandle`, `CocoaUI startup` again) — entirely
   outside any callback.

Every other flag in the table below follows the identical shape; only steps
2–3 differ in which Smalltalk method and host-service IMP they go through.

## 4. Every current instance

| Flag module | Set from | Serviced by (in `drain_perform`) |
|---|---|---|
| `rebuild.rs` | Debug ▸ Rebuild UI; Debug ▸ Force Fault (a Layer-1 recovery that also requests a Layer-2 rebuild, to prove escalation works); the TCL `rebuild` verb | `rebuild_ui(st)` — full UI-worker teardown + fresh `VmHandle`, re-adopting the same hosted peer id |
| `primary_restart.rs` | Debug ▸ Restart Primary VM | `st.supervisor.restart()` — kicks off the same respawn-from-source a fatal doit triggers automatically; the fresh generation is picked up by the `poll_resync()` loop that already runs right after these checks |
| `view_refresh.rs` — 4 flags | Browser tab shown (`onShow`); Find tab shown; Outliner tab shown; a Find button clicked (Implementors/Senders/Definitions) or Enter pressed in the search field | `st.ui.exec("CocoaBrowser doRefresh.")` / `"CocoaFind doRefreshOptions."` / `"CocoaOutliner doRefresh."` / `"CocoaFind doFindQuery."` |

`rebuild.rs` and `primary_restart.rs` are near-identical in shape but kept
as separate modules deliberately — restarting the primary and rebuilding
the UI worker are different operations with different blast radii (one
discards the whole object heap and reboots from world source; the other
discards the window set and reconnects to the *same*, unaffected primary).
Folding both into one file's doc comment would blur that distinction for
the next person reading it.

`view_refresh.rs` groups four *unrelated* flags in one file instead, on the
opposite reasoning: all four exist for the exact same reason (a DB query or
a live-VM query followed by an AppKit reload/repopulate), so grouping them
keeps that one hazard's explanation in one place rather than repeated four
times.

## 5. Design notes

- **Ordering in `drain_perform` matters.** The rebuild check runs first and
  `return`s immediately on a hit — a rebuild drops `st.ui`, so nothing else
  in that pass may touch it afterward; the fresh generation drains on the
  *next* pass. `primary_restart` and the `view_refresh` flags don't have
  that hazard (nothing they do invalidates `st.ui`), so they run in the
  same pass, after the rebuild check, before `poll_resync()`.
- **There's a free safety net.** `PrimarySupervisor`'s own watchdog loop
  wakes the main thread roughly every 250 ms regardless (`PUMP_BEAT_MS`,
  for inbox draining and the toolbar's metrics readout) — so even if an
  explicit `wake_main_runloop()` call were somehow lost, a flag would still
  be noticed within a quarter second. This is also why the metrics readout
  needs no `NSTimer` of its own.
- **A flag is idempotent, not a queue.** `request()` just sets a bool;
  firing it twice before the next drain pass services it once, not twice.
  None of the current uses need more than "has this been asked for since
  last time" — if a future one needs to carry a payload (which kind of
  query, which row), thread it through Smalltalk class-var state the way
  `CocoaFind`'s `Kind` does (set right before the flag, read back inside
  the handler the flag triggers), not through the flag module itself.
- **A classVar cannot be used to remember state *across* a UI rebuild.**
  This bit a real fix (`56a9aad`): a rebuild boots an entirely fresh UI
  worker VM from source, so every Smalltalk-side classVar — including any
  "what did I do last time" bookkeeping — is `nil` again at the top of
  every method, on every single generation, forever. Only native AppKit
  objects (the menu bar, in that case) survive a rebuild; nothing on the
  Smalltalk side does. State that must survive a rebuild has to live either
  in a native object you can query directly, or in Rust (`DrainState`,
  which `rebuild_ui` deliberately carries forward across the swap).

## 6. Adding a new flag-and-drain action

1. New module (or a new flag in an existing one, if it's clearly the same
   family as `view_refresh.rs`'s four): one `AtomicBool`, `request()`,
   `take_request()`. Doc comment states what triggers it and what it does
   — copy the shape of an existing module rather than inventing a new one.
2. `host_service.rs`: a new `extern "C" fn imp_request_<x>` that calls
   `<module>::request()` then `objc::wake_main_runloop()`. Register it in
   the `methods` array (bump the array's length literal).
3. `main.rs`: `mod <module>;`, and in `drain_perform`, `if <module>::
   take_request() { <do the real work using st>; }` — placed before
   `poll_resync()` unless the work invalidates `st.ui`, in which case
   `return` immediately after, matching the rebuild check.
4. World-side Smalltalk: a `CocoaUI class >> request<X>` (or wherever it
   belongs) that sends the new host-service selector, and whatever menu
   item / button wires to it via the existing `CocoaToolbarAction` +
   `MacvmDelegate actionTargetOn:` pattern every other button already uses.
5. Verify by driving the *real* callback path, not just the Rust-side flag
   — `performClick:` for a button (fires the same target-action message a
   real click's tracking machinery sends) is the cheapest faithful
   reproduction available without OS-level mouse control on a bare,
   non-`.app`-bundled binary.

## 7. One thing at a time — the drain refusing to re-enter itself

Status: **implemented `a1c2595` (2026-08-14), corrected `69887b8`.** §1–6 keep
VM work *out* of callbacks and hand it to the drain, on the premise that the
drain is safe ground. This section is the other half of that premise: what
happens when the drain runs while the VM is already busy, and the rule that
now prevents it.

### 7.1 The hazard

`drain_perform` is a `CFRunLoopSource`. A window op inside one of its own execs
— `makeKeyAndOrderFront:`, a fit-to-content resize — pumps the run loop, and
the pumped loop fires that same source again. The nested pass then `exec`s into
a VM whose interpreter is mid-flight, `vm.stack.fp` is left pointing at
ordinary data, and the next stack walk tears:
`Frame::method: frame method slot is not a CompiledMethod` (`stack.rs`). The
panic reaches `drain_perform`'s `extern "C"` boundary, cannot unwind, and
aborts the whole GUI.

It survived the C6 re-entrancy guard because this path **is not a delegate
callback** — `callback_active()` answers "is a callback live?", and the
question that matters is "is a VM entry live?".

The same tear is reachable from the AppleScript surface, where it is easy to
misread: the capture inside a scripting verb pumps the run loop, the drain
fires under it, and the app aborts. AppleScript's *next* command then reports
`-609 Connection is invalid` — that is the corpse of an already-dead process,
not an Apple Event fault, and chasing it as one wastes a day.

### 7.2 The mechanism

Eight top-level guest entries — `exec`, `eval` and its typed twins,
`load_source`, `render_fragment`, `fire_widget_action`, `dispatch_callback` —
each hold a `GuestEntry` RAII depth bump. `guest_active()` is the predicate
both doors read.

RAII is sound here precisely where `callback_active()`'s manual flag is not,
and the difference is which frame holds the state: every entry fn keeps its
`sigsetjmp` **inline in its own frame**, so a guest fault `siglongjmp`s back
*into* that frame and leaves by an ordinary `return` — the guard's `Drop`
always runs. Manual discipline is for state that must survive inner frames
being skipped, which this is not.

**The drain door.** `drain_perform` returns immediately on `guest_active()`,
at the very top, before any `take_request()` — so no flag is consumed and no
work is lost. The pass already on the stack drains its queues to empty when
control returns to it, and later arrivals re-fire the source. §5's free safety
net still applies underneath: the supervisor beat wakes the main thread every
250 ms regardless.

**The C6 door, and the distinction the first cut missed.** Gating *every*
callback while the VM is busy is wrong, and the failures were visible on
screen. The door tells **queries** from **events**:

- **Queries pass.** Data-source and toolbar reads answer from snapshots and
  have nested inside execs since CG5. Refusing them hands AppKit `nil` for the
  toolbar item list (every icon vanishes) or `NO` for expandability — and since
  reloads *run* inside refresh execs, every outline then caches
  `expandable=false`, draws no disclosure triangles, and tree expansion breaks
  app-wide.
- **Events are refused** with the shape default while the VM is busy, and
  arrive normally when it is idle. Actions, text changes, and the
  `windowShouldClose:` consent are the mutating deliveries whose interleaving
  is the whole point of the rule.

Return shape was the first classifier and it was **one selector too coarse**:
query-vs-event is a property of the *selector*, and `Bool` has members on both
sides. `is_bool_query` now names the Bool-shaped reads that belong with the
queries (`outlineView:isItemExpandable:`,
`application:delegateHandlesKey:`) — the `69887b8` correction, bisected to
`a1c2595`'s first cut.

### 7.3 What this changes for scripted driving

Deliberately: a `performClick:` inside a doit **is** a mid-exec synchronous
action and is now refused. Probes written that way were riding the hazard, not
testing through it. The honest scripted surface is `dispatch:value:` — exactly
what the action handler calls. Real user clicks arrive from the idle event loop
and are unchanged.

### 7.4 If you are testing this

Two traps in the setup, both of which have cost real time here:

- **Target by bundle id.** `tell application "macVM"` resolves through
  LaunchServices to `/Applications/macVM.app` and *launches it* — so a fix in
  `target/release` is never under test and looks ineffective. `tell application
  id "com.macvm.cocoa"` binds to the running instance.
- **`/Applications` cannot be written to** under macOS App Management. Build
  the bundle with `tools/make-macapp.sh cocoa` (unsigned unless `SIGN_ID` is
  set), ad-hoc sign it with `tools/macvm.entitlements` so the JIT entitlement
  still applies, and launch that binary directly. Swapping only the binary into
  an older bundle leaves a stale world payload behind it.

The recipe that reproduced the abort by round 2–3 on a pre-`a1c2595` build,
and that now runs clean:

```
osascript -e 'tell application id "com.macvm.cocoa"
  set current view to help
  snapshot in POSIX file "/tmp/a.png"
  evaluate "3 + 4"
  snapshot in POSIX file "/tmp/b.png"
end tell'
```

It needs a view switch **and** an `evaluate` before the capture; either alone
survives a dozen rounds, which is what made it look unreproducible when it was
first reported as "crash!".
