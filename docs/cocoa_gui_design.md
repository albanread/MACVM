# A native Cocoa GUI for MACVM — the environment, written in itself

**Status: design.** A second, flagged GUI mode. The current shell
(`gui/`, `macvm-gui`) renders its interface as HTML in a `WKWebView`, with
the VM on a worker thread. This document designs the alternative: **the
primary Smalltalk VM runs as the main UI thread and builds the interface
directly in Cocoa/AppKit, from Smalltalk, through the bridge** — the
Smalltalk-80 proposition, that the environment is written in the language
it hosts. It is the natural capstone of the Cocoa bridge (C0–C5,
`cocoa_bridge_design.md`) and the reflective RPC work
(`perform:withArguments:`), and it requires exactly one new bridge
capability, developed here as **C6: reverse dispatch (delegates)**.

The two GUIs coexist. Neither replaces the other; the WKWebView shell
stays the default. Cocoa mode is `macvm-gui --cocoa` (or the `macvm-cocoa`
alias) — a deliberate, flagged choice.

```smalltalk
"the whole GUI is Smalltalk calling AppKit through the bridge:"
CocoaApp new
    addWindow: (Workspace new openOn: 'Transcript showCr: 3 + 4');
    addWindow: (ClassBrowser new openOn: Integer);
    run.        "= [NSApp run] — the AppKit event loop IS the VM's main loop"
```

## 1. The one inversion that shapes everything

Today (S21) the threads are arranged one way; Cocoa mode flips them.

| | WKWebView mode (today) | **Cocoa mode (this design)** |
|---|---|---|
| Main / UI thread | Rust AppKit shell + `WKWebView` | **the primary Smalltalk VM** |
| The VM | a worker thread (`VmHandle`, `FatalMode::ExitThread`) | **is the main thread** |
| main→VM comms | always async (submit + drain-on-wake) | n/a — they are one thread |
| VM→AppKit | the C3 sync hop (`onMain`) | **direct calls — already on main** |
| Interface | HTML fragments the VM renders | **native AppKit views the VM builds** |
| Delegates/data sources | none (HTML has no callbacks) | **synchronous Smalltalk methods (C6)** |
| A bad doit | kills+restarts the worker VM; GUI survives | **aborts the one event handler; app survives** |
| Long doit | runs off-thread; UI stays live | **blocks the UI** — must be pushed to a worker |
| Compute VMs | workers, post results to the GUI | workers, post results to the **main VM** |

Everything below is the consequence of the top-left→top-right move.
Three consequences are load-bearing and get their own honest treatment:
**the crash model inverts (§5)**, **the main thread can block (§6)**, and
**delegates become the enabling capability (§4)**. The rest is a mapping
exercise, because the *view logic already exists in the image* (§7).

## 2. Why do this at all

Three reasons, in order of weight:

1. **It is the Smalltalk thesis.** Squeak/Pharo/Strongtalk all build their
   IDE in the image. MACVM's whole "code as truth, no image, boots from
   source" stance (`project_design_philosophy_roadmap`) has so far stopped
   at an HTML skin the VM feeds. A native GUI *written in Smalltalk* closes
   the loop: the environment is the language.
2. **It is the bridge's full-scale dogfood.** C0–C5 built windows, sends,
   the main-thread hop, and target/action callbacks — and CocoaPad proved
   them at toy scale. A real IDE exercises the parts the C-ladder
   deliberately deferred (delegates, data sources, drawing, text), which is
   exactly where a bridge's latent bugs live. Building the GUI *is* the
   proof the bridge is real.
3. **It removes the `WKWebView` dependency** for users who want a native
   environment, and it makes the GamePane (already native Metal) a
   first-class citizen instead of a panel wedged beside a browser.

Non-reason: performance. The WKWebView GUI is not slow. This is about
*what the GUI is made of*, not how fast it paints.

## 3. Boot, the flag, and run-loop ownership

The two modes share the crate, the world/image, the Cocoa bridge, the
GamePane, and `objc::bootstrap()`. They differ only in **who owns
`[NSApp run]`** and **what gets built after bootstrap**.

`fn main()` (`gui/src/main.rs`) already dispatches on the first CLI arg
(`render`/`seed`/`export`/`mandelvm`). Cocoa mode is one more arm:

```
macvm-gui               → WKWebView mode (default, unchanged)
macvm-gui --cocoa       → Cocoa mode
macvm-cocoa             → alias (a second bin target, same entry)
```

Cocoa-mode boot sequence, all on the main thread:

1. `objc::bootstrap()` — `NSApplication sharedApplication`, activation
   policy, the menu bar. (Shared with WKWebView mode.)
2. `enable_main_hop()` is **not** called — there is nothing to hop *to*;
   the VM is already main. (The C3 machinery stays dormant; workers still
   use it in reverse — §8.)
3. Boot the VM **on this thread** via a Cocoa-mode `VmHandle` variant
   (§5): genesis + world/image load, `FatalMode` set to the new
   `AbortToEventLoop` policy, the reverse-dispatch delegate class
   registered (§4).
4. Run the Smalltalk GUI-build doit: `CocoaApp startup` — Smalltalk code
   that constructs the initial windows (a Workspace + a Transcript, say),
   installs their delegates, and orders them front. This is *the GUI,
   programmed in Smalltalk*; it lives in a new world layer (§7,
   `world/5x_cocoaui/*.mst`).
5. `CocoaApp startup` ends by sending `Cocoa runApplication` = `[NSApp
   run]`. **This blocks forever, pumping the AppKit event loop** — and
   every event it dispatches lands, through a delegate/action trampoline,
   in Smalltalk. The AppKit run loop *is* the VM's main loop from here on.

There is no separate "VM thread" and no supervisor restarting it: in Cocoa
mode the VM's lifetime is the process's lifetime. Recovery granularity
moves from "restart the VM" to "abort the event handler" (§5).

The GUI is **not** persisted as live objects (MACVM has no running-image
save, by design — `reference_no_become_static_shape`,
`reference_gui_boots_from_image`). Each launch *rebuilds* the windows by
re-running `CocoaApp startup`, exactly as the world boots from source.
Window layout/state that should survive a restart is saved as data
(a small prefs plist or an image_store table), not as pickled views.

## 4. C6 — reverse dispatch: delegates as synchronous Smalltalk methods

This is the enabling capability, and the only genuinely new bridge work.
Everything an IDE does past "click a button" is a **delegate or data
source**: `NSTableViewDataSource` answers `numberOfRowsInTableView:` and
`tableView:objectValueForTableColumn:row:`; `NSOutlineViewDataSource`
answers `outlineView:child:ofItem:` and `outlineView:isItemExpandable:`;
`NSTextViewDelegate` answers `textDidChange:`; `NSWindowDelegate` answers
`windowShouldClose:`. These are **synchronous calls from AppKit that
expect a return value** — an integer, an object, a BOOL. C4's `MacvmAction`
is fire-and-forget target/action (void, async-friendly); it cannot answer
a data source.

VM-on-main makes the general case *easy*, because there is no thread to
cross and nothing to pickle: when AppKit calls a delegate method, the
Smalltalk delegate object is right here, on this thread, reachable as a
root. The callback is an ordinary synchronous Smalltalk method call.

### 4.1 The mechanism — one forwarding trampoline, C2's marshalling in reverse

One ObjC class, `MacvmDelegate : NSObject`, registered once, that responds
to **arbitrary** selectors via Objective-C's own dynamic message
forwarding:

- `- (BOOL)respondsToSelector:` → true for any selector the bound
  Smalltalk object claims to handle (a Smalltalk-declared allow-list per
  instance, so AppKit's `respondsToSelector:` probes answer correctly and
  optional delegate methods stay optional).
- `- (NSMethodSignature *)methodSignatureForSelector:` → an
  `NSMethodSignature` built from the selector's `@encode` type string. We
  already resolve that string at runtime for the *forward* direction
  (`objc_bridge::resolve_shape`, C2, via `method_getTypeEncoding` /
  protocol type strings); the same string builds a signature with
  `+[NSMethodSignature signatureWithObjCTypes:]`.
- `- (void)forwardInvocation:(NSInvocation *)inv` — the heart. Read the
  invocation's selector; for each argument index (2..n, skipping self/_cmd)
  read the raw argument bytes with `-getArgument:atIndex:` and **unmarshal
  it to a Smalltalk oop using C2's `classify_arg`** (native→Smalltalk: id→
  wrap `ObjcRef`, integer→SmallInteger, double→Double, `NSString`→String,
  struct→Array-of-numbers); build a Smalltalk Array; run
  `delegate perform: selector withArguments: args` — **the reflective
  primitive built for the worker RPC, reused verbatim** — then take the
  Smalltalk result and **marshal it back into the invocation's return slot
  with C2's `classify_ret`** via `-setReturnValue:`.

So C6 is **C2's marshalling run backwards**, glued by `perform:`. The
argument/return `@encode` vocabulary, the struct set, the width rules — all
already exist and are already tested. The new code is: the two ObjC
override IMPs, the NSInvocation get/set-argument plumbing, and a per-
instance `(delegate-oop, allow-list)` registry (the C4 ticket registry,
generalized from "one action block" to "one Smalltalk object + its handled
selectors").

### 4.2 Why synchronous-on-main is sound (and why it wasn't, off-main)

The C-ladder deferred delegates because *off* the main thread they are a
minefield: AppKit calls the delegate on the main thread, but the Smalltalk
object lives on the VM worker thread, so the callback would need a
**synchronous main→VM hop** — and the design's own invariant (§4 of
`cocoa_bridge_design.md`) is *main→VM is always async*, precisely to keep
the deadlock impossible. A data source that AppKit blocks on, waiting for
the VM worker, is the wait cycle the whole threading model forbids.

In Cocoa mode that tension evaporates: AppKit and the Smalltalk delegate
are the **same thread**. `forwardInvocation:` runs Smalltalk directly and
returns — no hop, no inbox, no pickling, no deadlock possible. The
capability the worker model couldn't safely have is the one the on-main
model gets for free. This is the core reason the native GUI wants the VM
on main, stated precisely.

### 4.3 Ownership of delegate objects

A window/table/textview **retains** its delegate (an `ObjcRef` wrapping a
`MacvmDelegate` instance). That `MacvmDelegate` holds a **ticket** into the
GC-rooted registry naming the Smalltalk delegate object — so the Smalltalk
side is a root and survives every GC, and no oop is ever stored ObjC-side
(the §2 contract holds unchanged). Delegates are **long-lived**: they are
*not* created inside a `poolDo:` scope and *not* swept — they are released
when their window closes (`windowWillClose:` → `unregisterDelegate:` +
`release`). This is the one place the leak-side bias needs discipline:
a never-closed window leaks its delegate ticket, acceptably; a
double-registered delegate is a bug the registry refuses (monotonic
tickets, C4 rule).

## 5. The crash model, inverted — abort to the event loop

In WKWebView mode a guest-fatal (`error:`, DNU, stack overflow, heap
exhaustion, a native fault) hits `fatal_exit`, which `pthread_exit`s the
*worker* thread; the supervisor restarts it and the GUI lives
(S21). **On the main thread that is not available** — `pthread_exit` on
main tears down the process, and we cannot `siglongjmp` blindly out of
arbitrary AppKit dispatch frames (the same undefined-behavior reason S21
rejected unwinding through JIT frames).

The answer is a **per-event recovery boundary**, and it fits the existing
machinery exactly. Every entry from AppKit into Smalltalk goes through
*one* trampoline — the C4 action IMP and the C6 `forwardInvocation:` IMP
are the only two doors. Each establishes a `sigsetjmp` recovery point
**at the trampoline, before calling Smalltalk** (the same `sigsetjmp`
discipline S21 proved for `VmHandle::eval`, relocated to the callback
door). A new `FatalMode::AbortToEventLoop` makes `fatal_exit`
`siglongjmp` back to *that* point instead of `pthread_exit`ing. Because the
jump target is the trampoline AppKit just called, the longjmp unwinds only
Smalltalk's own frames down to the trampoline, and the trampoline then
returns **normally** to AppKit — the run loop keeps pumping. The failed
action reports to the Transcript and to a status line; the window, the app,
and every other view survive.

So the recovery unit changes from *the whole VM* to *the current event
handler* — which, for an interactive environment, is the better
granularity: one broken doit does not take down your other windows.

Two conditions remain genuinely fatal, and the design says so plainly:

- **A native fault inside AppKit** (a SIGSEGV in framework code called by
  a delegate) is caught by PROBE's foreign-fault recovery (S21 §1c), so the
  *process* survives the signal — but it survives by `siglongjmp`-ing out
  of Apple's frames mid-mutation, abandoning whatever AppKit lock or
  internal state was in flight. Exactly as `cocoa_bridge_design.md` §5
  states: survivable-but-suspect. In a GUI whose main thread *is* AppKit,
  the honest posture is to report it, mark the UI subsystem poisoned, and
  recommend relaunch — not pretend the window server state is intact.
- **Heap exhaustion / a GC failure on the main VM** has nowhere to fail
  over to; it is fatal, with a crash dossier (PROBE, DBG0). This is the
  same ceiling every single-heap VM has.

And one honesty note inherited from `feedback_recover_clean_or_die`: an
aborted doit leaves the image in *whatever partial state it reached* before
the error (a global half-assigned, a collection half-built). For a user's
own workspace that is usually acceptable — it is their code — but the GUI
should surface "that action failed partway" rather than imply a clean
rollback there is no mechanism to perform.

## 6. The main thread can block — and the answer is workers

This is the real cost of VM-on-main, stated without euphemism: **a
long-running or infinite doit freezes the UI**, because it holds the main
thread and starves the run loop, exactly like a synchronous handler in any
native app. WKWebView mode does not have this problem (the VM is
off-thread); Cocoa mode does.

The architecture already contains the answer, and it is the correct Cocoa
idiom regardless: **don't compute on the UI thread — dispatch to a worker
VM and update the UI on the reply.** The multi-VM machinery is exactly
this (`multi-smalltalk-worker.md`): the main (UI) VM is the star's centre;
worker VMs run on their own threads; a worker posts its result to the main
VM's inbox, whose drain runs on the main thread between events. The
`perform:`-based RPC (`call:on:args:onReply:`, just built) is the natural
API:

```smalltalk
"heavy work off the UI thread; the result updates the view on reply:"
worker call: #sieveUpTo: on: #PrimeSieve args: #(1000000)
    onReply: [:primes | resultView setStringValue: primes size printString ].
```

The GUI framework (§7) should make this a first-class, one-line pattern
(`aView compute: [...] then: [:r | ...]`) so that "the main thread stays
responsive" is the path of least resistance, not a discipline the
programmer must remember. The inbox drain is wired to the run loop as a
main-thread source (the M3 wake, reused): a worker reply schedules a
main-thread drain, which runs the `onReply:` block between AppKit events —
never mid-event, preserving the strictly-serial invariant.

Short doits (a workspace `printIt`, a browser refresh) run inline and are
fine; the boundary is "work you would not do in a native app's button
handler," and it is the same boundary every native GUI programmer already
knows.

## 7. The views — reuse the models, render to AppKit

The web GUI's views are not just HTML: each is a `Visual` subclass in the
image with real *model* logic — `ClassHierarchyOutliner`, `ClassOutliner`,
`ImplementorsView`, `SendersView`, `DefinitionsView`, `CodeView`, plus
`Workspace`, `CodeEditor`, `ClassMirror`, and the reflection tools
(`world/33_smappl.mst`, `34_tools.mst`, `60_editor.mst`). They currently
answer `htmlFragment`. In Cocoa mode they keep their model — *what classes
exist, what a class's protocols and methods are, who sends #foo, the source
of a method* — and grow a second rendering: **build a native view and feed
it via a data-source delegate**, instead of emitting HTML. This is
`feedback_dual_placement_not_migration` applied to rendering: HTML and
AppKit are both first-class faces of the same model, not a migration.

The Smalltalk GUI framework (new world layer, `world/5x_cocoaui/`):

- **`CocoaApp`** — owns the menu bar, the window set, startup, and the
  `run`/`terminate` bracket around `[NSApp run]`. The single entry point
  `main()` invokes.
- **`CocoaWindow`** — an `NSWindow` wrapper: content view, title, close
  handling (its `NSWindowDelegate` is a `MacvmDelegate` bound to the
  `CocoaWindow`), and the reusable `compute:then:` worker bracket (§6).
- **View wrappers**, thin over AppKit, each with its delegate:
  - **Workspace** — an `NSTextView` in a scroll view + a doit/printit
    action (⌘D / ⌘P via key-equivalent menu items or an
    `NSTextViewDelegate`); evaluates the selection through the same
    `execute_do_it` path the WKWebView Workspace uses, appends the result.
  - **Transcript** — an append-only `NSTextView`; `Transcript show:`
    writes to it (the transcript sink now targets a native view, not the
    HTML pane — a `TranscriptSink` implementation, `src/embed.rs` shape).
  - **ClassBrowser** — the classic Smalltalk browser is *literally*
    `NSBrowser`-shaped (columns: class categories → classes → protocols →
    methods) or an `NSOutlineView`; its data source is a `MacvmDelegate`
    bound to a `ClassBrowser` model that reuses `ClassMirror` /
    `ClassHierarchyOutliner` to answer "children of", "is expandable",
    "display string". Selecting a method opens its source in a **CodeView**.
  - **CodeView** — an `NSTextView` with Smalltalk syntax colouring
    (`NSAttributedString` / text-storage attributes — a modest marshalling
    add, `NSColor`/`NSFont` + attributed-range setting), backed by the
    existing `CodeView`/editor model for source; Save recompiles + writes
    image_store, exactly as the WKWebView browser's edit path already does.
  - **Find views** — `ImplementorsView`, `SendersView`,
    `DefinitionsView` become `NSTableView`s whose data source is the
    existing model (already accurate via the persisted `method_sends`
    table, `project_gui_browser_tools`).
  - **GamePane** — drops in unchanged: it is already a native Metal view
    (`gamepane_design.md`); in Cocoa mode it is simply another `CocoaWindow`
    content view rather than a panel beside a `WKWebView`.

The division of labour: the **model** stays in the image (and is shared,
byte-for-byte, with the WKWebView GUI); the **AppKit wrapper + delegate**
is the new, thin, Cocoa-mode-only layer. No model logic is duplicated;
`htmlFragment` and `buildCocoaView` are two renderers over one model.

## 8. Workers still work — the star just has a Cocoa centre

Multi-VM is unchanged in shape and *strengthened* in fit. The main VM is
the star's primary; worker VMs run compute on their own threads and post
results to the primary's inbox (`multi-smalltalk-worker.md`, M0–M4). Two
Cocoa-mode specifics:

- **Only the main VM may touch AppKit.** A worker VM must never call an
  AppKit class directly (it is not the main thread); if a worker needs the
  UI it posts a message to the main VM, whose handler does the AppKit call
  on the main thread. This is the C3/C4 machinery *in reverse*: in
  WKWebView mode workers hop to a Rust-owned main; in Cocoa mode they hop
  to the **Smalltalk-owned** main VM. The bridge should enforce it — an
  AppKit-prefixed send from a non-main VM fails loudly (the curated
  main-thread class list, `cocoa_bridge_design.md` §7 non-goal, now
  becomes a real guard).
- **The inbox drain is a run-loop source.** The M3 coalesced wake
  schedules a main-thread drain that runs *between* AppKit events, so a
  worker reply updates a view without a race and without interrupting a
  live event handler.

## 9. What is reused vs. genuinely new

**Reused wholesale:** the Cocoa bridge C0–C5 (wrapping, sends, ownership,
the exception shim, callbacks, the mint-list); `perform:withArguments:`
(the delegate dispatch core); the C2 `@encode` marshalling machinery (run
in reverse for C6); the `VmHandle` boot path (`src/embed.rs`, with a new
main-thread FatalMode); PROBE + the `sigsetjmp` recovery (relocated to the
callback door); the image/world and `image_store`; every view *model* in
the image; the GamePane; the multi-VM workers + the RPC layer.

**Genuinely new:**
1. **C6 reverse dispatch** — the `MacvmDelegate` forwarding class + the
   NSInvocation get/set-argument plumbing + the per-instance delegate
   registry (§4). ~1 file, `src/runtime/objc_delegate.rs`, + a handful of
   world methods.
2. **`FatalMode::AbortToEventLoop`** + the per-callback `sigsetjmp` boundary
   in the two trampolines (§5). Small, localized to `objc_bridge.rs` /
   `vm_state.rs`.
3. **A modest marshalling top-up** for text UIs: `NSAttributedString`
   attribute ranges, `NSColor`/`NSFont` construction (§7 CodeView). Grown
   row-by-row from real need, as the C-ladder grew (`cocoa_data`-driven).
4. **The Smalltalk GUI framework** — `world/5x_cocoaui/` (`CocoaApp`,
   `CocoaWindow`, the view wrappers). This is the bulk of the *typing* but
   the least of the *risk*: it is ordinary Smalltalk against a bridge that
   already works.
5. **The `--cocoa` flag / `macvm-cocoa` bin** + the main-thread boot path
   in `gui/src/` (§3).

## 10. Milestone ladder (each lands green and alone, per the standing rules)

| | Deliverable | Proof |
|---|---|---|
| **G0** | `--cocoa` flag; boot the VM on main; `CocoaApp` opens one empty `NSWindow` and enters `[NSApp run]`; ⌘Q quits cleanly | on-screen (user): a native window titled by Smalltalk; headless: the boot path returns `Err` cleanly with no AppKit |
| **G1** | **C6**: `MacvmDelegate` forwarding + NSInvocation marshalling + the per-callback `sigsetjmp` abort boundary; a bad delegate doit aborts to the run loop, app survives | headless gate: a registered delegate's `perform:` returns a value AppKit-side (proxied test); a raising delegate unwinds cleanly, next event still dispatches |
| **G2** | Workspace + Transcript: type `3 + 4`, ⌘P prints `7`; `Transcript showCr:` appends; the transcript sink targets the native view | on-screen; headless: the sink receives text |
| **G3** | ClassBrowser: `NSOutlineView`/`NSBrowser` data-sourced from `ClassMirror`/`ClassHierarchyOutliner`; select a class → its methods; select a method → source in a CodeView | on-screen; headless: the data-source callbacks answer the same rows the `htmlFragment` model does (differential vs. the web GUI's own model) |
| **G4** | CodeView editing: syntax colour, Save → recompile + image_store write (reuse the web edit path); Find views (implementors/senders/definitions) as `NSTableView`s | on-screen; headless: an edit round-trips through image_store byte-identically to the web path |
| **G5** | The worker bracket (`compute:then:`); a heavy doit runs in a worker and updates a view on reply without freezing the UI; GamePane as a `CocoaWindow` | on-screen: the spinner/UI stays live during a parallel-Mandelbrot dive driven from a native window |

G0–G1 are the real design risk (main-thread boot + reverse dispatch +
abort boundary); G2–G5 are mapping work over a proven base. On-screen
verification is the user's throughout (no WindowServer in the agent
shell), paired with headless gates on the model/marshalling/recovery
seams, per `feedback_gui_visual_verification`.

## 11. Non-goals, risks, open questions

**Non-goals (v1):**
- Interface Builder / nib loading. Views are built imperatively in
  Smalltalk. (A declarative layer could come later; it is not the point.)
- Auto Layout in its full generality — start with springs/struts or
  simple frame math; adopt `NSLayoutConstraint` only where a view genuinely
  needs it.
- Persisting live window objects. Windows rebuild from source each launch;
  only *data* (layout prefs) persists (§3).
- Replacing the WKWebView GUI. It stays the default; this is a second face.

**Risks, honestly:**
- **The main thread blocks (§6)** — the defining cost. Mitigated by the
  worker bracket, but it is a real change in feel from the web GUI and the
  framework must make "compute in a worker" frictionless or users will
  freeze their own UI.
- **Native faults are now main-thread faults (§5)** — a framework crash is
  a session-poison event, not a survivable worker death. The bridge is
  narrow and mechanically-checked precisely to keep this rare, but it is a
  higher-stakes failure than the web GUI's.
- **C6 correctness** — `forwardInvocation:` marshalling is C2 in reverse;
  its adversarial-review surface (struct straddles, width truncation,
  ownership of returned objects, a delegate that raises mid-invocation and
  must still leave the NSInvocation's return slot in a defined state) is
  exactly the C1/C2 review surface and must get the same treatment.
- **Retain cycles** — window↔delegate↔Smalltalk-object lifetimes need the
  C4 ticket discipline held consistently for long-lived objects; a leaked
  window is fine, a use-after-free of a released delegate is not.

**Open questions for the build:**
1. `forwardInvocation:` (fully general, NSInvocation) vs. a registered-IMP
   table per selector-shape (C1-style, simpler, less general). §4 assumes
   the former for generality; G1 should spike both and measure the
   NSInvocation plumbing cost before committing.
2. CodeView: native `NSTextView` text storage as the source of truth, vs.
   the existing rope/`TextBuffer` editor model (`world/60_editor.mst`)
   driving it. Simpler is NSTextView-owns-the-text, sync source strings at
   Save; revisit only if the editor model earns its keep.
3. Whether Cocoa mode is a `--cocoa` flag on `macvm-gui` or a separate
   `macvm-cocoa` bin (shared entry either way). A separate bin keeps the
   main-thread-boot path from complicating the WKWebView `main()`.

## 12. Relationship to the existing docs

This design sits on top of, and does not re-open: `cocoa_bridge_design.md`
(C0–C5 — the bridge this uses), `multi-smalltalk-worker.md` (the workers
that keep the UI responsive), `docs/vm_handle.md` / S21 (the embedding +
recovery model this relocates to the callback door), `gamepane_design.md`
(the native view that drops straight in), and the `perform:withArguments:`
RPC work (the delegate dispatch core). It is the point where the Cocoa
bridge stops being a way to *call* macOS and becomes the substrate the
environment is *built from* — the Smalltalk-80 idea, on Cocoa.
