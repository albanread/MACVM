# Cocoa GUI — sprint plan (Phase CG)

Implements [`cocoa_gui_design.md`](cocoa_gui_design.md): a native AppKit
programming environment as a **second, flagged GUI mode** (`macvm-cocoa`),
built as a new `cocoa_gui` cargo workspace member, in which a UI worker VM
pinned to the main thread is a dumb terminal and the persistent primary VM
lives on a background thread. The design was adversarially reviewed
(design §9.0); these sprints carry the review's corrections as gates.

Every sprint ends **green**: all prior tests pass, plus the sprint's own
acceptance gate. On-screen behaviour is the **user's** verification (no
WindowServer in the agent shell); every sprint also carries a **headless
gate** on its model / marshalling / recovery / protocol seam, so progress
is machine-checkable without a display (`feedback_gui_visual_verification`).

**Sizing:** S = a focused day or two; M = up to a week; L = 1–2 weeks
part-time. **Order is dependency-driven.** CG0–CG1 are core-only soundness/
infrastructure and can land before any AppKit code exists. CG2 opens the
window; CG3 is the one genuinely new bridge capability; CG4+ are mapping
work over a proven base. This track is **parallel to the core** and does
not gate core sprints.

The design's G0–G5 ladder maps to these sprints as noted per entry.

---

## CG0 — Signal-infra prerequisites `S` (core only, no AppKit)

**Goal.** The two soundness fixes the review found are prerequisites for
*any* multi-VM-on-a-GUI-thread arrangement, and they help every existing
mode too. Land them first, in core, provable headless.

**Deliverables.**
- **Per-thread signal alt-stacks.** Replace the single shared static
  `ALT_STACK` (`src/codecache/deopt_trap.rs`) that every thread's
  `sigaltstack` aliases — a documented "one VM on one thread" assumption
  already stale under the worker fleet — with a per-thread alt-stack
  (thread-local, allocated on first `arm_foreign_fault_handler`). Without
  this, a fault on the primary concurrent with a fault in a UI callback
  corrupts the recovery machinery §5 leans on.
- **`FatalMode::ExitProcess` selectable at boot.** `VmHandle::boot`
  unconditionally arms `ExitThread` (`src/embed.rs`); a VM booted on the
  main thread must instead exit the *process* on a true fatal (heap
  exhaustion, stack overflow), or it leaves a headless zombie. Add a boot
  option; default unchanged.

**Acceptance gate (headless).** A test spawning two VMs on two threads,
each triggering a genuine `SIGSEGV` (bad `Alien` read) *concurrently*,
both recover cleanly (per-thread stacks proven); the existing single-VM
foreign-fault gates stay green. A VM booted `ExitProcess` on a true fatal
exits the process (observed via a subprocess harness), not `pthread_exit`.

**Design ref:** §5 prerequisite infra, §9.1 item 2.

---

## CG1 — Externally-hosted worker + run-loop wake + conditional world load `M` (core only)

**Goal.** The multi-VM machinery assumes a worker is spawned onto a *new*
thread and wakes by blocking in `recv()`. The UI worker runs on an
*existing* thread (main) and is blocked in `[NSApp run]`, not `recv()`.
Add the surface for that, plus the conditional world layer — all core,
provable headless with an ordinary thread standing in for "main".

**Deliverables.**
- **Register a worker on an existing thread.** A `workers.rs` path that
  creates a worker id + a real inbound `Sender<Envelope>` + a **wake hook**
  and hands the caller the `Receiver` + boot payload — *without*
  `thread::spawn`. The hosting thread drives its own drain loop
  (recv → stage → `Worker dispatchPending.`) instead of `worker_main`'s
  built-in recv loop. (`spawn`'s hard-coded `std::thread::spawn` cannot do
  this today — the review's item-9.)
- **A run-loop-poke wake.** The primary→worker link is a bare `Sender` with
  no wake hook (workers wake only by blocking in `recv()`); the M3 wake is
  Primary-inbox-only. Give the externally-hosted worker's inbound inbox an
  `InboxSender`-style wrapper whose wake is a caller-supplied `Fn()` — the
  `performSelectorOnMainThread` pattern that today lives in `gui/src/
  vm_host.rs` (`CrossThreadObjcRef::notify`), promoted to a general hook.
  (In CG1 the wake hook is an ordinary condvar/channel poke for the test;
  the AppKit poke arrives in CG2.)
- **`load_list(vm, path)`** — a public fn to load an extra world list after
  the base world, and a `world/cocoaui.list` stub (empty for now). The
  loader hardcodes `dir/world.list` today; this is the conditional-layer
  mechanism §12.3 named but did not have. Files reserved **63+** (51–62 are
  taken).

**Acceptance gate (headless).** A test registers an externally-hosted
worker on the *current* thread (no spawn), the primary `send:`s it an
envelope, the wake hook fires, the host thread drains it and replies, the
reply reaches the primary's continuation. `load_list` loads a two-method
extra list on top of the base world and both resolve. The existing worker
ping-pong / RPC gates stay green.

**Design ref:** §3 steps 3–4, §8 (the wake), §9.1 items 3 & 10.

---

## CG2 — The crate, the boot, the window (design G0) `M`

**Goal.** Open a native window. The `cocoa_gui` crate boots the primary on
thread 2 and the UI worker on main, runs one Smalltalk startup doit that
builds an empty `NSWindow`, and Rust enters `[NSApp run]` with the VM at
rest. ⌘Q quits clean.

**Deliverables.**
- **The `cocoa_gui` workspace crate** + the `macvm-cocoa` bin; `objc::
  bootstrap()` factored out of the `gui` crate so both call it. (Inherits
  the GamePane path-deps for later — CG10 — but no GamePane yet.)
- **The parked-main boot handshake** (§3): main bootstraps AppKit, spawns
  the primary on the watchdog thread, parks; the primary boots the world,
  registers the UI worker (CG1) and signals ready; main boots the UI worker
  VM in place (`ExitProcess`, CG0), publishes the **thread-local
  `*mut VmHandle`** the trampolines will read, runs `CocoaUI startup` (build
  one `NSWindow`, order front), returns to Rust; Rust calls `[NSApp run]`.
- **`world/cocoaui.list`** gains its first file: `CocoaUI` (startup, the
  window) — pure bridge calls, on the main thread, VM quiescent between.
- **The AppKit main-thread guard** (§8): in the native Cocoa GUI, an AppKit
  send from a non-main VM fails loudly (an exact-name curated UI-class list).
  **Armed only under a `COCOA_UI_MODE` flag** that `macvm-cocoa` sets at
  startup — it is a *no-op* for every other host. This gating is load-bearing,
  not incidental: the shipping WKWebView GUI runs its single VM on a **worker**
  thread and legitimately resolves an AppKit class off-main as the first half
  of the C3 resolve-then-`onMain` pattern (CocoaPad, C5). An unconditional
  guard broke that shipping demo (caught in CG2 review); the flag confines the
  §8 rule to the mode it actually describes. Class *resolution* is thread-safe
  and never itself the fault — the guard exists to catch an off-main AppKit
  *use* by a background VM.

**Acceptance gate.** *On-screen (user):* `macvm-cocoa` opens a native
window titled from Smalltalk; ⌘Q quits with no crash. *Headless:* the
two-VM boot handshake completes and returns `Err` cleanly on a machine with
no window server (or in a `NSApp`-less harness); the pure guard decision is
proven both ways (Cocoa mode on → off-main AppKit refused / on-main allowed;
Cocoa mode off → nothing refused, the CocoaPad anti-regression) and
`cocoa_main_hop` arms the guard on a genuine main thread.

**Design ref:** §3, §10 G0, §9.1 items 6 & 10.

---

## CG3 — C6 reverse dispatch: delegates as top-level entries (design G1) `L`

**Goal.** The one genuinely new bridge capability: AppKit calls a Smalltalk
delegate synchronously and gets a return value, dispatched as a *top-level*
VM entry, with Layer-1 recovery.

**Deliverables.**
- **The per-role delegate class family** (`src/runtime/objc_delegate.rs`,
  in core beside `MacvmAction`): `MacvmWindowDelegate`, `MacvmTextDelegate`,
  `MacvmTableSource`, `MacvmOutlineSource` — each `class_addMethod`-registered
  with only its own selectors (so `respondsToSelector:` is natively correct,
  no allow-list). ~7 typed `extern "C" fn` IMPs across the 8 shapes, all in
  the existing `classify_arg`/`classify_ret` vocabulary.
- **Each IMP as a top-level entry** (the review keystone): read the
  ABI-delivered args, unmarshal, read the thread-local `*mut VmHandle`, run
  `delegate perform: selector withArguments: args` through the existing
  `eval`/`perform` door, marshal the return. No re-entrant `&mut VmState`.
- **The VM-tagged instance→ticket registry** — a stale delegate (from a
  not-yet-closed window after a future restart) fails *closed*, returning a
  defined default (0 / `nil` / `NO`) for its return shape.
- **`prim_cocoa_new_action` re-plumbed** so a *Worker* VM (the UI worker)
  can mint delegates — it refuses non-primary VMs today (review item-5).
- **Layer-1 recovery** (§5): a delegate doit that `error:`s, or a native
  fault in *our* code inside a callback, unwinds via the existing per-entry
  `sigsetjmp` back to the trampoline, which returns the defined default and
  reports; the run loop pumps on. No new `FatalMode` variant.
- **Re-entrancy guard** (CG3 review, folded before commit): the top-level-entry
  assumption is that the VM is quiescent when AppKit calls back. A *nested*
  callback — an AppKit modal / menu-tracking / live-resize run loop pumped from
  inside a handler — would alias a live `&mut VmState`, clobber the single
  per-thread `sigsetjmp` slot, and overwrite the idle-baseline watermark. No
  such path exists in CG3, but rather than rely on the assumption, a thread-local
  `callback_active` flag makes a re-entrant callback fail **closed** (shape
  default) BEFORE the trampoline re-borrows the `VmHandle`. This is what keeps
  the door sound as CG7 introduces the first nesting paths.

**Acceptance gate (headless, proxied — no display needed).** A registered
`MacvmTableSource` bound to a Smalltalk object answers
`numberOfRowsInTableView:` with a real integer when sent
`objc_msgSend`-style from the test; a delegate whose handler raises returns
the shape's default and the *next* delegate call still dispatches; a forced
`SIGSEGV` inside a delegate handler recovers and the next call succeeds; and a
re-entrant callback fails closed before borrowing the VM.

**Design ref:** §4, §5 Layer 1, §9.1 item 1.

> **CG4 follow-up (from CG3 review, not a CG3 defect).** Delegate *lifetime*: a
> minted delegate instance is registered process-wide, but nothing yet pins it
> alive against GC of its guest `ObjcRef` (AppKit sets delegates *unretained*).
> The instance→ticket registry keys on the raw pointer, so a freed-then-reused
> pointer could return a wrong entry. Not triggerable in CG3 (delegates aren't
> wired to real views yet, and the gate keeps its delegate alive), but CG4's
> request protocol / world-side delegate ownership must give minted delegates a
> definite process-lifetime pin (an extra retain, matching C4's "tickets live
> for the process") so the registry pointer is stable.

---

## CG4 — Request protocol, Transcript, Workspace, primary restart (design G2) `L`

**Goal.** The UI talks to the primary. Doits run on the primary and their
results/transcript come back; the watchdog restarts the primary on a fatal
doit and the UI re-syncs.

**Deliverables.**
- **The request protocol** (§7.3): `{#uiReq. corr. verb. …}` UI→primary
  (`#doit`, `#refresh`, `#select`, `#saveMethod`), `{#uiReply. corr. payload}`
  and `{#snapshot. …}` primary→UI. Primary-side `dispatchOne:` grows an
  `isUiReq:` branch (capturing `CurrentCorr`); `reply:` works primary-side.
- **`(peer, corr)` namespacing** — the real corr-collision fix: `Pending
  Replies` keyed by originating peer + corr, so the two VMs' independent
  `NextCorr` counters cannot fire the wrong continuation (review R4).
- **The Transcript sink** = `ForwardTranscript` with the destination flipped
  to the UI worker's inbox (~15 lines); the UI worker's Transcript
  `NSTextView` appends per line.
- **Workspace** (`NSTextView`): ⌘P/⌘D ship the *selection* text as a
  `#doit` request; the primary evaluates through the existing `execute_do_it`
  path; the `#uiReply` result appends. Text is local to the NSTextView.
- **The watchdog supervisor** (§5.1): the primary-spawning (watchdog) thread
  owns the primary's death signal; on a fatal doit it respawns the primary
  from source and posts a snapshot-invalidate; the UI shows "restarting…"
  and re-syncs.

**CG4-review must-fix (folded before commit): death is signalled EXACTLY, not
inferred from a heartbeat.** The first draft heartbeated between `pumpInbox:`
beats and respawned on a timeout — but a beat doesn't return until its doit
completes, so any Workspace computation over the timeout (a benchmark, a loop,
image work — routine load) starved the heartbeat and spuriously respawned a
*live* primary, discarding the result AND every class/global defined that
session (a respawn boots from source). Fixed by a new core hook
(`embed::set_thread_fatal_hook`, fired inside `fatal_exit` the instant before
`pthread_exit`): only a genuine fatal posts `Died`; the watchdog blocks on it
with no timeout, so a busy primary is never respawned. Four paired should-fixes
also folded: the `#uiReq` reply now routes from a `(peer, corr)` **snapshotted
before** the doit runs (a doit that dispatches can't corrupt its own reply
routing); a re-sync clears the UI worker's pending continuations
(`Worker abandonPending` — recover clean, no cross-restart leak) after draining
the dying generation's inbox (its last transcript line isn't lost); and a
bounded backoff caps the respawn loop on repeated boot failure.

**Acceptance gate.** *On-screen (user):* type `3 + 4`, ⌘P → `7`; `Transcript
showCr:` appears; a deliberately fatal doit restarts the environment and the
next doit works. *Headless:* the `#uiReq`/`#uiReply` round-trip; a
constructed `(peerA, corr=1)` + `(peerB, corr=1)` pair fires the *right* two
continuations (non-collision); a scripted primary death → respawn → re-sync;
**and a fatal (stack-overflow) doit posts `Died` → respawn → next doit works,
while a live primary is never respawned on time alone.**

**Design ref:** §6, §7.3, §7.4, §5.1, §9.1 items 4 & 9.

---

## CG5 — Native app shell: toolbar, metrics, theming, view switcher (design G2b) `M`

**Goal.** CG4 proved the window works; nothing built after it has anywhere to
live. Every later view (ClassBrowser, CodeView, Outliner) needs a shell that
already knows how to host a swappable content view, show a real menu, and
carry the toolbar/metrics/theme chrome — so build the shell BEFORE the views,
not implicitly alongside the first one. Layout (user-specified): a toolbar at
the top, the current view in the middle, a small reverse-chronological
Transcript dock at the bottom, and a real macOS menu bar.

**Deliverables.**
- **A real menu bar** — beyond CG2's bare Quit item: an App menu (About/
  Preferences stub/Quit), a **View menu** listing every registered persistent
  view (Workspace now; Browser/Editor/Outliner land as CG6+ register
  themselves — the menu is built FROM the registry, never hand-maintained),
  and the existing Workspace submenu (Do it/Print it) folded in as a
  per-view menu contribution (§ design below).
- **The view-switcher content host** — a Rust-side `ViewRegistry` (name →
  `{activate, deactivate, menuContribution}` triple) and ONE `NSView`
  container in the window's middle region that swaps its single subview on
  switch. `CocoaUI` keeps owning the window/toolbar/transcript; each
  registered view owns only its own content `NSView` + Smalltalk-side
  controller — the same "dual placement, not migration" split the WKWebView
  GUI's view models already follow (`feedback_dual_placement_not_migration`).
  Workspace becomes the first (and initially only) registered view, migrated
  onto this host unchanged in behavior.
- **The toolbar** — a custom `NSView` strip (not `NSToolbar`; its fixed
  macOS-native chrome doesn't fit a themeable, icon-driven design), left
  side = view-switcher buttons (icons below), right side = the live metrics
  readout.
  - **Icons, reused not forked**: the SAME `gui/assets/icons-mono/*.svg`
    files the web GUI already ships, loaded as `NSImage(contentsOfFile:)`
    with `isTemplate = true` — AppKit's template-image mechanism IS the
    native equivalent of the web toolbar's "currentColor mask" trick
    (`project_gui_themes`): one glyph asset, tinted per-theme via
    `contentTintColor`, zero duplicate art.
  - **Metrics, reused not reinvented**: `src/embed.rs`'s existing
    `VmMetrics`/`VmHandle::metrics()` (the same struct the web GUI's
    toolbar already polls, `gui/src/main.rs::metrics_on_snapshot`) —
    sampled straight off the PRIMARY's own `VmHandle`, inside
    `supervisor.rs`'s existing `primary_generation_main` beat (already
    running every 250ms on the primary's own thread — a free, in-process,
    zero-new-protocol read), published to a shared cell
    `PrimarySupervisor::metrics()` reads cross-thread. A ~1s `NSTimer`
    on main renders it into the toolbar's readout (heap used/cap, nmethods,
    alloc rate, scavenges/full GCs — the same fields, same semantics as the
    web toolbar's MEM/JIT/CODE/ALLOC/GC row).
- **Theme infrastructure** — a `CocoaTheme` value (background/text/accent
  `NSColor`s + a monospace font choice), a `Theme` menu, and NSAppearance
  wiring so System/Light/Dark at minimum work end to end (System driven by
  `NSApp.effectiveAppearance`, Light/Dark forced). This is infrastructure
  for the eventual 13-theme parity with `Theme::ALL`
  (`project_gui_themes`) — NOT full parity in this sprint: the web GUI's
  pixel-art/CRT-scanline themes are CSS-rendering tricks with no direct
  AppKit-control equivalent, and are explicitly deferred, not blocking.
  Every icon send must go through the tint color so a theme swap repaints
  the toolbar with zero new art (proving the reuse actually holds).
- **The Transcript, corrected**: CG4 built it append-at-bottom, full-height,
  as the primary output surface — wrong on both counts once Workspace
  Print It exists (results insert inline in the Workspace, per CG6 below).
  Re-dock it as a SMALL, fixed-height strip at the window bottom, newest
  line FIRST (reverse chronological) — a log to glance at, not read top to
  bottom.

**Acceptance gate.** *On-screen (user):* the toolbar shows live, moving
metrics while a Workspace doit runs; the View menu (currently one item)
switches content correctly; the Theme menu changes toolbar icon tint +
window colors; the Transcript is small, newest-first, and doesn't crowd the
Workspace. *Headless:* `ViewRegistry` register/switch/menu-build is a pure
Rust unit test; `PrimarySupervisor::metrics()` returns a real `VmMetrics`
sampled from a live primary in the existing supervisor test harness; a
`MACVM_COCOA_SNAP` sequence (this sprint's own build-verification tool —
`cocoa_gui/src/snapshot.rs`, `objc::snapshot_client_area`) captures real
on-screen PNGs proving the shell renders before any human looks at it.

**Design ref:** §7.4 (Transcript), §11 Q4/R9 (Workspace text is local — the
view host doesn't change that), `project_gui_themes`, `project_metrics_dashboard`.

**As built (CG7 close-out note).** The view registry landed **Smalltalk-side**
(`CocoaUI`'s `Views`/`registerViewNamed:title:icon:activate:` +
`installViewMenuOn:`), not as the Rust-side `ViewRegistry` this entry
specified — deliberately: the UI worker owns all view construction, so the
registry lives beside it, and the View menu is still built purely from
registrations. The planned "pure Rust unit test" gate is therefore void; the
registry's machine gate is the CG7 differential/headless suite plus
`MACVM_COCOA_SNAP`. CG6's selection-or-everything + inline Print It shipped
early inside CG5's commits (`64_cocoaui.mst` carries both); its headless
splice/selection gates land with CG7 slice 0.

---

## CG6 — Workspace, properly: selection eval + inline Print it (design G2c) `S`

**Goal.** CG4's Workspace evaluates its ENTIRE fixed buffer on every Do it —
a canned demo, not an editable scratch buffer. Match the WKWebView GUI's
`workspace_render.rs`/`smtk.js` convention exactly (same product, second
renderer — `feedback_dual_placement_not_migration`): Do it/Print it act on
the current TEXT SELECTION, or the whole buffer if nothing's selected;
Print it additionally inserts the result INLINE right after the evaluated
text, at the exact point Print it was invoked (not appended at buffer end).

**Deliverables.**
- **Selection-aware `shipDoit`** — read the `NSTextView`'s
  `selectedRange` (a plain `NSRange`, two `NSUInteger`s — no HFA/struct-by-
  value AppKit pitfall the CG4-fixed autorelease/CG5-fixed-snapshot work
  already hit; verify the marshalling path chosen doesn't reopen it) instead
  of always `workspaceText`; empty selection falls back to the full buffer,
  matching `workspaceEvalTarget()`'s exact selection-or-everything rule.
- **Print it's own round trip** — mirrors the web GUI's split (`doit` vs
  `workspacePrintIt`, `gui/src/vm_host.rs`'s two request kinds): Do it
  discards the result (side-effect only, as today); Print it captures the
  insertion point at invocation time (a selection can move before the async
  `#uiReply` lands — the web GUI's `pendingPrintInsertAt` capture-at-click
  discipline, ported verbatim) and splices the result text in on reply.
- **Starter content** — replace the fixed `'3 + 4'` buffer with the SAME
  placeholder comment + example the web GUI opens with
  (`workspace_render.rs::PLACEHOLDER_TEXT`) — one buffer, one voice, across
  both renderers.
- **A stateful demo worth testing** — the acceptance gate below exercises a
  primary-persists-state proof (a counter), not a single fixed expression;
  this is the sprint's actual point per the standing request ("this is a
  mini demo not a workspace... create something actually worth testing").

**Acceptance gate.** *On-screen (user):* type `Counter := 0.` Do it; type
`Counter := Counter + 1. Counter` and Print it repeatedly — the printed
value climbs each time, proving the primary is a persistent, stateful VM,
not a fixed-expression demo; selecting a sub-expression and Print it inserts
the result at the selection, not the buffer end; a `MACVM_COCOA_SNAP`
sequence captures the climbing counter on screen. *Headless:* a selection-
range round-trip test (select a substring, Do it, confirm only the
substring reached the primary); a Print it round trip confirms the result
splices at the captured insertion point even when the selection moved
before the reply arrived (the async-race case `pendingPrintInsertAt` exists
for).

**Design ref:** §11 Q4/R9, the web GUI's `workspace_render.rs`/`smtk.js`
(§4 "Workspace" section) as the pinned reference behavior.

---

## CG7 — ClassBrowser: names-only snapshots + outline handles (design G3) `M`

**Goal.** The classic Smalltalk browser, native — an `NSOutlineView`
data-sourced from a snapshot that projects **names, not oops**.

**Deliverables.**
- **`snapshotFor:`** on the primary side, per view: the browser model
  (`ClassMirror`/`ClassHierarchyOutliner`) produces a nested Array/Dictionary
  tree of **Strings/Symbols/smis only** — the pickle refuses class oops, so
  a snapshot can never carry a mirror (review R3). `{#snapshot. viewId.
  generation. tree}`, generation-guarded against stale blasts.
- **The outline-handle scheme** (§7.1): the terminal mints a stable native
  node handle (an `NSString` keyed by tree path) per node for the current
  generation, resolving AppKit's `child:ofItem:`/`isItemExpandable:` back to
  snapshot nodes; handles discarded on re-blast.
- **The ClassBrowser view** — `NSOutlineView` + a `MacvmOutlineSource` (CG3)
  answering from the snapshot; select a class → its methods; select a method
  → ship a `#select` request, get the source, show it (CodeView stub for
  now, real in CG6).

**Acceptance gate.** *On-screen (user):* browse the class hierarchy, drill
to a method's source. *Headless (differential):* the data-source callbacks
answer the *same* class/protocol/method rows the existing `htmlFragment`
model produces for the same class (a differential vs. the web GUI's own
model); the snapshot pickles clean (a round-trip proving no class oop
crossed).

**Design ref:** §7.1, §7.2, §10 G3.

**As built.** Rows exactly as designed: `UiBrowserService` (base world,
beside `ClassMirror`) projects the live hierarchy into names-only 6-slot
nodes, served late-bound by the `#refresh(#browser)` verb; `CocoaBrowser`
(`world/66_cocoabrowser.mst`, cocoaui layer) resolves 0-based path handles
(`'2.0.14'` NSStrings, Symbol-key cached for the item-identity stability
NSOutlineView requires) over the snapshot, one `MacvmOutlineSource` serving
as both dataSource and delegate. The outline-source role grew its three
missing IMPs (`child:ofItem:`, `objectValueForTableColumn:byItem:`,
`outlineViewSelectionDidChange:`) — the row-by-row marshalling discipline.
**One deliberate deviation:** the source pane is NOT a `#select` round trip
to the primary — the primary keeps no method source (methods compile and
drop their text), so source is **host-served** from the SQLite image by a
`MacvmHostService` ObjC class (`cocoa_gui/src/host_service.rs`,
`image_store::method_source`/`class_source`) reached as an ordinary C3
bridge send — the exact split the WKWebView GUI uses (`vm_host` →
`image_store`), dual placement: the VM owns the rows, the image owns the
text. Live-defined classes browse with a "no source in the image" note.
`#select` stays reserved for CG8's save path. Gates: the embed differential
(`browse_snapshot_matches…`, incl. pickle round-trip + the two-VM `#refresh`
protocol trip), the path-resolution fail-closed gate
(`cocoa_browser_resolves_paths…`), and the `cocoa_delegate` item round-trip
(pointer stability, nested paths, selection dispatch).

---

## CG8 — CodeView editing + Find views (design G4, part 1) `M`

**Goal.** Edit a method natively and save it; the find tools as tables.

**Deliverables.**
- **The marshalling top-up** — `NSAttributedString` attribute ranges +
  `NSColor`/`NSFont` construction (grown row-by-row), enough for Smalltalk
  syntax colour in an `NSTextView`.
- **CodeView** — `NSTextView` with syntax colour; Save ships a `#saveMethod`
  request; the primary recompiles + writes `image_store` (the exact web edit
  path — `project_gui_browser_tools`). Reseed discipline noted
  (`reference_gui_boots_from_image`).
- **Find views** — `ImplementorsView`/`SendersView`/`DefinitionsView` as
  `NSTableView`s + `MacvmTableSource`s over primary-built snapshots (accurate
  via the persisted `method_sends` table).

**Acceptance gate.** *On-screen (user):* edit a method, Save, re-browse to
see the change. *Headless:* a `#saveMethod` round-trips through `image_store`
byte-identically to the web edit path (differential); a find-view snapshot
lists the same implementors/senders the web model does.

**Design ref:** §7.2, §10 G4.

**As built (first two slices; syntax colour pending).** Editing arrived
ahead of schedule inside Browser v3's Accept machinery: selecting a method
makes the source pane editable in place, Accept saves through the SAME
shared flow as + New Method (`image_store::flows::save_method` — an
upsert), live-compiles the reopen on the primary over `uiDoit`, and reports
honestly (TCL-proven: an edited `String>>asLowercase` answered its new body
on the primary). The find views are ONE native view (`world/67_cocoafind.mst`):
a selector field + Implementors/Senders buttons over `Image::implementors_of`
/ `senders_of` (the persisted `method_sends` index — the web find's own
queries) via two thin host-service adapters; selecting a hit JUMPS to the
browser (view switch, ancestor-disclosing class select, side flip, selector
select — the browser's scripting helpers double as the jump path).
Remaining for CG8: syntax colour (the `NSAttributedString` marshalling
top-up) and the Definitions find kind.

---

## CG9 — UI-worker restart-in-place (design G4, part 2) `M`

**Goal.** An AppKit-internal fault rebuilds the GUI in place, cleanly —
proving the Layer-2/3 recovery the design's crash story rests on.

**Deliverables.**
- **The top-level GUI recovery point** wrapping build-windows + the
  callback-driving, with the UI worker `VmHandle` held in a slot *outside*
  the recovery `sigsetjmp` scope.
- **The ordered teardown** (§5 Layer 2, review R3b): on a foreign fault
  reaching the recovery point — `orderOut:`+`close` every old window, nil
  their delegates, invalidate the old C6/C4 registry tickets, retire the old
  UI-worker peer registration on the primary — *then* **cleanly Drop the old
  `VmHandle`** (`Reservation` munmap + `deopt_trap::deregister` +
  setjmp-slot release; leaking instead panics at `REGISTRY_CAP=128`), *then*
  boot a fresh UI worker, re-register, re-run `CocoaUI startup`, re-sync,
  re-enter `[NSApp run]`.
- **The N/T restart-loop backstop** (§5 Layer 3): N rebuilds within T
  seconds → dossier + `ExitProcess`.

**Acceptance gate.** *On-screen (user):* a menu item that triggers a forced
AppKit-internal fault → the window set rebuilds, the session continues,
the primary's state is intact. *Headless:* a scripted forced foreign fault
at the recovery point drops the old handle with **no reservation and no
PROBE-registry leak** (assert the registry count returns to baseline across
many scripted restarts — proving the 128-cap is never approached) and
reboots; the backstop trips after N/T.

**Design ref:** §5 Layers 2 & 3, §9.1 item 5.

**As built.** A rebuild is a **flag serviced on the main-thread drain**, never
an immediate call: dropping the UI worker's `VmHandle` from inside a callback
executing in that very VM is unsound, so a callback (Debug ▸ Rebuild UI, Debug
▸ Force Fault, the control `rebuild` command, the `requestUiRebuild` host IMP)
only sets `rebuild::REBUILD_REQUESTED` + wakes the run loop; `drain_perform`
performs the rebuild on its next pass, VM quiescent. `rebuild_ui`
(`cocoa_gui/src/main.rs`) is the ordered Layer-2 teardown: unpublish the door
(`ui_vm()`→null, trampolines fail closed) → best-effort `CocoaUI teardown` →
drain+discard the dead generation's inbox → **boot a fresh UI worker
re-adopting the SAME hosted id** (the channel is Rust-owned in `DrainState`, so
no cross-thread re-registration on the live primary is needed — a simplification
over the design's "retire + re-register") → assign, which **drops the old
handle** (Reservation munmap + `deopt_trap::deregister_setjmp` + setjmp-slot
release) → republish (bumps the UI-VM generation; stale C6 tickets fail closed)
→ `CocoaUI startup`. Layer 3: `note_and_check_backstop` (`rebuild.rs`) —
N=5 rebuilds within T=8s → dossier + `ExitProcess`.

The **leak soundness gate** landed as designed but on the tighter same-thread
case: `embed::ui_worker_restart_lifecycle_leaks_no_registry_slots` boots +
publishes + drops a UI worker **40×** on one thread and asserts
`deopt_trap::active_jmp_slots()` / `active_probe_slots()` (new introspection)
return to baseline every cycle — a stranded slot would show as immediate growth,
and the JMP=64 / PROBE=128 caps stay untouched. On-screen (TCL-driven): a
`CG9Holder` class-var set to 4242 on the PRIMARY survived a `gui rebuild` (the
toolbar/transcript came back fresh — metrics reset from 508→23 compilations,
proving a genuinely new UI VM — and the primary answered `4242` through it);
and Debug ▸ Force Fault → signal-11 recovered by the callback door (Layer 1) →
escalated to a full rebuild (Layer 2) → the app pinged alive with the primary
intact.

---

## CG10 — Worker bracket + GamePane drive + tracking-safe drain (design G5) `M`

**Goal.** The payoff: the UI stays live while heavy work runs elsewhere, a
native GamePane window, and a drain that survives menu tracking.

**Deliverables.**
- **`do:then:`** — the one-line worker bracket (§6): heavy work runs on the
  primary or a spawned compute worker; the reply updates a view.
- **The GamePane drive path** (§7.5): the *render* half (`NativePane`)
  drops in; the *drive* half is new — a main-thread timer the UI worker owns,
  a GameStep-as-RPC with corr-id backpressure, and a native-pane command
  channel (a dedicated Rust channel + run-loop wake — **not** 60 fps of
  pickled step-doits; the cadence is settled here, open-question 2).
- **Default-mode-only drain** (§8): the inbox drain schedules in
  `NSDefaultRunLoopMode` only (or a `CFRunLoopSource` in default mode), so a
  snapshot never swaps mid-tracking/modal; the UI is intentionally stale
  during a tracking session and re-syncs on its end.

**Acceptance gate.** *On-screen (user):* a parallel-Mandelbrot dive driven
from a native window keeps the UI responsive; opening a menu during a live
table update does not throw an AppKit consistency exception. *Headless:* the
`do:then:` round-trip; the drain scheduled in default mode is *not* delivered
during a simulated nested-mode session (a mode-gating unit check where
feasible).

**Design ref:** §6, §7.5, §8, §10 G5.

---

## Dependency graph & sequencing

```
CG0 (alt-stacks, ExitProcess)  ─┐
CG1 (hosted worker, wake, load_list) ─┴─► CG2 (crate, boot, window)
                                            └─► CG3 (C6 delegates)
                                                  └─► CG4 (protocol, workspace, restart-primary)
                                                        └─► CG5 (app shell: toolbar, metrics, theme, view switcher)
                                                              ├─► CG6 (workspace, properly: selection + inline print it)
                                                              ├─► CG7 (browser)
                                                              │     └─► CG8 (codeview, find)
                                                              └─► CG9 (restart-UI-in-place)   [needs CG3 registry + CG2 boot]
                                                                    └─► CG10 (workers, gamepane, drain)
```

- **CG0 + CG1 are core-only** and land before any AppKit code — they are
  soundness/infra that also strengthen the existing worker + WKWebView modes.
- **CG2 + CG3** hold the real design risk (top-level-entry dispatch, the boot
  handshake, reverse dispatch). Get them right and CG4+ are mapping work.
- **CG5 (app shell) gates every later view** — CG6–CG10 all need somewhere
  to live; it is deliberately inserted before the browser, not built
  implicitly alongside it (the gap this revision closes).
- **CG9** (UI restart) can follow CG3+CG2 independently of CG5–CG8; schedule
  it early if crash resilience matters more than the views.
- The track is **parallel to the core**; no CG gate blocks a core sprint,
  and no core stress gate needs the Cocoa GUI.

## Standing rules (inherited)

- **On-screen is the user's; headless gates are mandatory** — every sprint's
  machine-checkable seam has a test that runs without a display. CG5+ also
  has `MACVM_COCOA_SNAP` (`cocoa_gui/src/snapshot.rs`): a timestamped PNG
  sequence of the real running app, captured from Rust via
  `CGWindowListCreateImage` — self-verification before a human looks,
  not a substitute for the user's own on-screen check.
- **Commit to main, often, all of it** (`feedback_commit_to_main_often`);
  each CG lands green and alone.
- **Stress/soak in RELEASE + PARALLEL** where a recovery/GC seam is touched
  (CG0, CG9 especially) — `feedback_parallel_release_stress`.
- **Adversarial review before the big commits** (CG3, CG9) — the design
  itself was shaped by two; the recovery and reverse-dispatch code deserve
  the same.
- **Dual placement, not migration** (`feedback_dual_placement_not_migration`):
  the view *models* stay shared with the WKWebView GUI; Cocoa adds a second
  renderer, never a fork of the model.
