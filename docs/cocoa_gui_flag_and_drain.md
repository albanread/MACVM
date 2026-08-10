# Flag-and-drain — how `cocoa_gui` touches VM-level state safely

Status: implemented, in force since the CG9 UI-worker-rebuild work (`8266381`,
`22a0daf`) and extended several times since (most recently `47d0758`,
`56a9aad`). This document is the authority for *why* the mechanism exists,
*how* it's shaped, and *when* a new piece of Cocoa-GUI code needs to use it.

§1–6 describe the mechanism as built. **§7 is an open hole in it** — the drain
assumes it never runs inside a callback and does not check, which aborts the
app through the AppleScript `snapshot` verb. Diagnosed, not yet fixed; that
section is the plan.

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

## 7. The converse hole — the drain running *inside* a callback

Status: **diagnosed 2026-08-10, not yet fixed.** This section is the plan.

Everything above keeps VM work *out* of callbacks and hands it to the drain,
on the premise that the drain itself is the safe place. `drain_perform` states
that premise four times in its own comments — "main thread, VM quiescent",
"never inside a callback", "a fresh top-level `exec` here" — and **never checks
it**. It is a run-loop source. Anything that pumps the run loop while a
callback is live fires it, and it runs `st.ui.exec(…)` — a fresh top-level VM
entry stacked on the callback's paused activation.

The AppleScript `snapshot` verb does exactly that, and aborts the app.

### 7.1 The proved path

From an instrumented backtrace (a temporary probe in `VmHandle::exec` printing
whenever it is entered while `callback_active()`):

```
cocoa_send_n                        the world calls the host
→ imp_snapshot_window_to            host_service.rs — the scripting verb's capture
→ objc::snapshot_window             AppKit capture; PUMPS THE RUN LOOP
→ drain_perform                     main.rs — the default-mode drain source fires
→ VmHandle::exec("CocoaUI updateMetricsMem: …")     ← second top-level VM entry
```

The outer entry is the scripting verb's own `dispatch_callback`, paused
mid-activation inside `performSnapshotCommand:`. The inner one runs the
metrics readout against the same `&mut VmState`, and leaves `vm.stack.fp`
pointing at ordinary data.

The damage surfaces later, wherever the next stack walk happens — in the
observed case `enter_compiled`'s zombie sweep:

```
WALKDUMP fp=42 sp=72 has_frame=true compiled_depth=0 deopt_resume_depth=0
         link_idx=0 last_compiled_fp=0x0 tier_links=[]
         slots 42/43/44 = three heap pointers, 32 bytes apart
```

`walk_frames` starts at `Mode::Interp(vm.stack.fp)`, finds three consecutive
heap objects where a frame header should be, and panics with
`Frame::saved_fp: not a smi`. The panic cannot unwind across the `extern "C"`
scripting IMP, so `panic_cannot_unwind` aborts the process. AppleScript's
*next* command then reports `-609 Connection is invalid`: that is the corpse,
not a second bug, and chasing it as an Apple Event problem wastes a day.

This is §1's failure mode 2 again — `walk_frames`, tier links, an abort that
looks intermittent — reached from the opposite direction. §1 is a callback
doing the work it should have deferred; this is the deferred work running
inside a callback anyway.

### 7.2 Why the existing guards miss it

`callback_active()` is consulted in exactly two places, both on the way *in* to
a callback: `dispatch_callback` and `objc_delegate::dispatch`. Its doc calls
itself a re-entrancy guard, and for nested callbacks it is one.

`exec` and `eval` consult nothing and set nothing. So the flag answers "is a
callback live?" but nobody can ask "is a VM entry live?", which is the question
that matters — and the doors that skip the check (`drain_perform`'s six
`exec`s, the control channel's `eval`/`exec`, `refresh_metrics`) are precisely
the ones the run loop can fire from underneath a callback.

`enter_compiled`'s sweep guards (`compiled_depth == 0`, `deopt_resume_depth ==
0`) are irrelevant here: both are legitimately zero. The stack is already
corrupt by the time the sweep looks at it.

### 7.3 The fix

1. **Make the predicate honest.** Replace the callback-only flag with a VM
   entry depth in `embed.rs`, maintained by all three doors — `exec`, `eval`,
   `dispatch_callback` — with `callback_active()` kept as-is on top of it so
   the existing nested-callback guard keeps its current meaning. Then a
   `vm_entry_active()` exists for anyone who needs the real question.
2. **Guard the drain.** `drain_perform` bails at the very top when
   `vm_entry_active()` — *before* any `take_request()`, so no flag is consumed
   and no work is lost. This is free: the supervisor's beat loop wakes the main
   thread every 250 ms unconditionally (§5's "free safety net"), so a skipped
   pass costs at most a quarter second of metrics staleness. `game::drain()`
   and `refresh_metrics` sit inside `drain_perform` and are covered by the same
   early return.
3. **Stop the abort, and restore the VM.** `script_command!` already wraps its
   `dispatch` in `catch_unwind` (verified: 15/15 hammer rounds no longer kill
   the app) and reports a script error instead. That is necessary but not
   sufficient — a panic unwinds *past* `run_method_reentrant`'s
   `restore_activation`, so the VM stays wedged and every later verb answers
   `-10000`. The catch arm must rewind to the idle baseline the way the
   guest-fatal arm does (`restore_after_guest_fatal`, today private to
   `VmHandle`) before returning the error.
4. **Keep the `settle_run_loop` suppression.** `snapshot_client_area` skips its
   settle inside a script dispatch (`in_script_dispatch()`). With (1) and (2)
   in place this is no longer load-bearing for soundness, but it is still
   right: a scripted capture should not be pumping AppKit's queue at all. The
   control-channel and menu paths keep settling, so the stale-pane behaviour it
   was added for is unaffected.

Steps 1 and 2 are the actual fix; 3 turns any *future* escape into an error
instead of an abort. Do not stop at 3 — it converts a crash into a silently
dead VM, which is worse.

### 7.4 Reproducing it

Fails by round 2–3. It needs a view switch **and** an `evaluate` before the
capture; either alone survives a dozen rounds, which is what made it look
unreproducible when it was first reported as "crash!".

```
osascript -e 'tell application id "com.macvm.cocoa"
  set current view to help
  snapshot in POSIX file "/tmp/t.png"
  evaluate "3 + 4"
  snapshot in POSIX file "/tmp/t.png"
end tell'
```

Two traps in the setup, both of which cost real time:

- **Target by bundle id, not by name.** `tell application "macVM"` resolves
  through LaunchServices to `/Applications/macVM.app` and *launches it* — so a
  fix in `target/release` is never under test, and looks ineffective. `tell
  application id "com.macvm.cocoa"` binds to the running instance.
- **`/Applications` cannot be written to.** macOS App Management blocks it, so
  the build under test has to be a copy: `ditto` the installed bundle
  somewhere writable, drop in the fresh binary, then `codesign --force
  --options runtime --entitlements tools/macvm.entitlements -s -` (ad-hoc is
  enough locally; the JIT entitlement still applies). Launch that binary
  directly and it registers under the same bundle id.

A headless regression test is worth having and does not need AppKit: two
`VmHandle` entries nested on one `VmState` is the whole bug. Assert that the
inner one fails closed rather than corrupting `vm.stack.fp` — that is a unit
test in `embed.rs`, not a GUI test, and it is what would have caught this.
