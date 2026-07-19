# How the Cocoa GUI actually works — classes, messages, memory, two VMs

Status: implementation walkthrough, current source. This document does not
re-argue design decisions — `cocoa_bridge_design.md` (the C0–C5 memory-model
contract) and `cocoa_gui_design.md` (the three-VM architecture, adversarially
reviewed) already do that, and `cocoa_gui_flag_and_drain.md` already covers one
mechanism (how a callback safely reaches cross-VM state) in depth. This doc
sits underneath all three: it walks the **actual, current code** — real
function names, real file paths — for four questions asked together often
enough to deserve one answer: how does Smalltalk find a Cocoa class, how does
a Smalltalk message become (and receive) an Objective-C call, how do a moving
GC and manual reference counting coexist in one process, and how does the
main-thread UI VM actually talk to the persistent VM behind it.

## 1. The shape: two GUIs, one core

```
macvm-gui     (gui/)        WKWebView — HTML rendered by a Smalltalk VM on a worker thread
macvm-cocoa   (cocoa_gui/)  AppKit — a Smalltalk VM pinned to the main thread, driving Cocoa directly
```

Both binaries depend on the same `macvm` core crate: the interpreter, the JIT,
the GC, the world/image, and the Cocoa bridge (`src/runtime/objc_bridge.rs`,
`src/runtime/objc_delegate.rs`) all live once, in core, and are exercised by
both GUIs and by the headless test suite. `cocoa_gui/` adds only the AppKit
glue: process bootstrap, the boot handshake, the delegate-role classes'
`extern "C"` IMPs, and the flag-and-drain modules. Nothing in this document is
Cocoa-GUI-only except §7 (the UI VM ↔ primary VM link) — §4–6 apply equally
to `Cocoa action:` calls made from a plain script running under
`macvm repl`.

## 2. Three VMs, one process

| Tier | Thread | Role |
|---|---|---|
| **UI worker VM** | main (pinned by AppKit) | a dumb terminal: builds native views, answers AppKit delegate/data-source calls from a local snapshot. Quiescent between callbacks. |
| **Primary VM** | background (spawned by `cocoa_gui`) | the persistent state: live objects, classes-being-edited, workspace vars. Boots the world, registers the UI worker, runs doits. |
| **Compute worker VMs** | other background threads | parallel work the primary spawns (M0–M4), unrelated to the GUI split. |

The chicken-and-egg — the primary registers the UI worker, but the UI worker
must run on the thread the process started on — is resolved by booting the UI
worker **in place on main** and letting Rust own the run loop
(`cocoa_gui/src/boot.rs`). The real sequence, as implemented:

1. **(main)** `main.rs` does the AppKit bootstrap (`NSApplication
   sharedApplication`, activation policy) — no Smalltalk yet.
2. **(main)** `boot::handshake_wire_vms` spawns the **primary VM on a
   background thread** and blocks on a ready channel.
3. **(background)** The primary thread boots `VmHandle::boot` against the base
   world, calls `set_worker_boot` (so it can spawn compute workers later), and
   **registers the UI worker as an externally-hosted peer** — a worker id +
   `InboxSender` + a run-loop-poke `InboxWakeFn`, with **no `thread::spawn`**
   (the UI worker's thread is main, which already exists). It signals ready.
4. **(main)** `boot_ui_worker` boots a **second, independent** `VmHandle` in
   place — its own heap reservation (`DEFAULT_HEAP_MIB = 512`), its own
   `FatalMode::ExitProcess` (not `boot()`'s default `ExitThread`, which on
   main would leave a headless zombie) — layers the conditional
   `world/cocoaui.list` (files 63+, loaded by no other host), and takes on the
   Worker role so its replies route back to the primary. `main.rs` then
   publishes the thread-local `*mut VmHandle` (`boot::publish_ui_vm`, which
   also bumps the process-wide UI-VM generation counter — §7.5) and runs the
   Smalltalk startup doit `CocoaUI startup`: builds the menu bar and initial
   windows, installs their C6 delegates, orders them front.
5. **(main, Rust)** `main.rs` calls `[NSApp run]`. The VM is now at rest.
   Every future AppKit event dispatches through a C4/C6 trampoline as a
   **top-level entry** into the UI worker (§5.2); every primary→UI inbox
   drain runs the same way, scheduled on the run loop in default mode only
   (`cocoa_gui_flag_and_drain.md`).

Two independent `VmHandle`s, two independent heaps, two independent GCs —
nothing is shared by reference. Everything that crosses between them crosses
by value, through the worker-inbox message protocol (§7.2) that already
existed for compute workers.

## 3. Finding a class: `objc_getClass`, both directions

There is no compile-time binding to any Cocoa class anywhere in this bridge —
no generated stubs, no header parsing at build time. Every class reference is
a runtime, string-keyed lookup, in whichever direction it happens.

### 3.1 Forward — Smalltalk asks for an existing class

`objc_bridge::class_named(name)` (`src/runtime/objc_bridge.rs:1057`) is the
whole mechanism:

```rust
pub fn class_named(name: &str) -> Option<*mut c_void> {
    if check_appkit_main_thread(name).is_err() { return None; }
    let s = syms()?;
    ensure_pool(s);
    let c = CString::new(name).ok()?;
    let cls = unsafe { (s.objc_get_class)(c.as_ptr()) };
    if cls.is_null() { None } else { Some(cls) }
}
```

`objc_get_class` is `objc_getClass`, resolved once via `dlsym` into a
process-wide `OnceLock<Syms>` (`syms()`), alongside `sel_registerName`,
`objc_retain`/`objc_release`, the pool push/pop entry points, and the C2 shape
introspection pair (§5.1). The same `syms()` init also `dlsym`-touches
`NSStringFromClass` inside
`/System/Library/Frameworks/Foundation.framework/Foundation` — a
dlopen-by-path with `RTLD_GLOBAL` that makes Foundation's classes visible to
`objc_getClass` at all. `libobjc` itself is link-time linked
(`build.rs: rustc-link-lib=objc`), so its own symbols resolve from the process
image without `dlsym`.

The one guard on this path is `check_appkit_main_thread`
(`objc_bridge.rs:1047`): **armed only when the native Cocoa GUI is running**
(`cocoa_ui_mode()`), it refuses an AppKit UI class (an exact-name list —
`NSWindow`, `NSButton`, `NSTableView`, … — deliberately not a prefix test,
since Foundation shares the `NS` prefix) resolved from any thread other than
main. Every Foundation class, and every class on the main thread, resolves
unconditionally; outside Cocoa mode the guard is a complete no-op (the
WKWebView GUI legitimately resolves AppKit classes off-main as half of its own
C3 resolve-then-hop pattern).

### 3.2 Reverse — Rust mints brand-new classes at startup

The other direction is how AppKit calls *into* Smalltalk at all: the bridge
doesn't just look classes up, it **registers new ones**. `register_class`
(`src/runtime/objc_delegate.rs:475`) is the whole mechanism, called once per
role behind a `OnceLock`:

```rust
fn register_class(name: &str, methods: &[(&str, *const c_void, &str)]) -> Option<*mut c_void> {
    let superclass = objc_bridge::class_named("NSObject")?;
    let cls = objc_allocateClassPair(superclass, name, 0);
    for (selector, imp, types) in methods {
        let sel = objc_bridge::register_selector(selector)?;
        class_addMethod(cls, sel, *imp, types);   // types = an @encode string, e.g. "q@:@"
    }
    objc_registerClassPair(cls);
    Some(cls)
}
```

Two independent generations of this pattern exist, for two different needs:

- **C4 — `MacvmAction`** (`objc_bridge.rs:1472`, `macvm_action_class`). One
  selector, `macvmFire:` (`v@:@`, void). Firing posts a pickled
  `{#cocoaEvent. ticket}` message to a worker's inbox
  (`workers.rs:522`) rather than returning anything — fire-and-forget,
  usable from any thread because it needs no synchronous answer. This is the
  general bridge's own primitive, reachable from plain Smalltalk as
  `Cocoa action: aBlock` (`world/49_cocoa.mst`, primitive 243) — nothing
  Cocoa-GUI-specific about it.
- **C6 — the `MacvmDelegate` role family** (`objc_delegate.rs:520–645`). Five
  classes, each carrying only the selectors its role answers, generalizing
  C4's pattern to **synchronous, return-value-bearing** callbacks:

  | Role | Class | Selectors |
  |---|---|---|
  | `#window` | `MacvmWindowDelegate` | `windowShouldClose:` (`B@:@`), `windowWillClose:` (`v@:@`) |
  | `#text` | `MacvmTextDelegate` | `textDidChange:` (`v@:@`) |
  | `#table` | `MacvmTableSource` | `numberOfRowsInTableView:` (`q@:@`), `tableView:objectValueForTableColumn:row:` (`@@:@@q`), `tableViewSelectionDidChange:` (`v@:@`) |
  | `#outline` | `MacvmOutlineSource` | `outlineView:numberOfChildrenOfItem:` (`q@:@@`), `outlineView:isItemExpandable:` (`B@:@@`), `outlineView:child:ofItem:` (`@@:@q@`), `outlineView:objectValueForTableColumn:byItem:` (`@@:@@@`), `outlineViewSelectionDidChange:` (`v@:@`) |
  | `#action` | `MacvmActionTarget` | `macvmDoIt:`, `macvmPrintIt:`, `macvmAction:` (all `v@:@`) |

  Because each class carries only its own role's selectors,
  `respondsToSelector:` is natively correct with no allow-list — an
  `NSTableView` probing for an unimplemented optional method gets a true
  negative, not a caught exception. `MacvmActionTarget` (the `#action` role)
  is what every button and menu item in `cocoa_gui` binds to via
  `MacvmDelegate class >> actionTargetOn:` — **not** C4's `MacvmAction` —
  because a target/action send in this GUI always runs on the same
  (main) thread as its handler and can afford to wait for the real answer
  inline (§5.2), where C4 exists for the cross-thread, no-answer-needed case.

Instances are mint-once-per-role, reused for every window/table/menu of that
role; what's unique per *instance* is not the ObjC class but the ticket in the
registry the instance is bound to (§6.5).

## 4. Sending a message: Smalltalk → Objective-C → Smalltalk

### 4.1 Forward — a Smalltalk send becomes `objc_msgSend`

`try_send_full` (`objc_bridge.rs:1083`) is the general entry. It resolves
`sel_registerName` for the selector, then calls `macvm_try_msgsend` — a small
hand-written shim (`objc_shim.m`, compiled by `build.rs`) that performs the
*actual* `objc_msgSend` **inside an Objective-C `@try`/`@catch`**. A raised
`NSException` never unwinds into Rust or Smalltalk frames; the shim catches it
and returns a status code plus the exception's description as bytes, which
`try_send_full` turns into `Result::Err(String)`.

The shim's calling convention is fixed and AAPCS64-shaped: 6 GPR argument
words (`x2..x7` — `x0`/`x1` are always `self`/`_cmd`), 8 FPR doubles
(`d0..d7`), 4 shared stack/spill words in; a `RetKind` token selects which
result registers the caller reads back afterward (`Gpr`/`Fpr`/`Hfa2` for
`CGPoint`/`CGSize`/`Hfa4` for `CGRect`/`IntPair` for `NSRange`/`Void`/`F32`).
Nothing about this is selector-specific — the same shim call shape sends
`length`, `setTitle:`, and `tableView:objectValueForTableColumn:row:` alike.

What makes each send type-correct is **shape resolution**, done by asking the
*live* ObjC runtime, never by hand-written per-selector glue. `resolve_shape`
(`objc_bridge.rs:685`) does:

```rust
let cls = object_getClass(target);                    // works for both instance and class sends —
let m   = class_getInstanceMethod(cls, sel);           // a class object's metaclass IS where its
let enc = method_getTypeEncoding(m);                   // "class methods" live
let shape = parse_type_encoding(enc);                  // "@encode" string -> Shape { ret, args }
```

`parse_type_encoding` (`objc_bridge.rs:634`) walks the `@encode` grammar
token-by-token (`split_type` handles `{struct=fields}` brace matching and
`^pointer` indirection), classifying each into `ObjcArg`/`ObjcRet` — integers
carry their *declared* width (`l`/`L` are 32-bit by `@encode` definition even
on LP64; only 64-bit integers may spill to the stack, since Darwin packs
narrower stack args to their natural size and an 8-byte spill word would shift
every later offset), `{CGPoint=dd}`/`{CGRect=...}`/`{_NSRange=QQ}` become
`Point`/`Rect`/`Range`, everything else (raw pointers, unknown structs,
unions) is `None` — an unmarshalable shape, which fails the send cleanly
rather than guessing.

Resolved shapes are cached process-wide, keyed by `(Class pointer, selector)`
in an `RwLock<HashMap>`, with hit/miss counters — the design's "one process-
wide PIC" rather than a per-call-site cache. A **negative** result (method not
found *today*) is deliberately **not** cached, since a category or `dlopen`'d
framework can add methods to an existing class later (S20); an **unparseable
encoding** *is* cached, because that's a stable property of an existing
method, not something that can newly appear.

This is the whole answer to "how does a Smalltalk `aWindow setTitle: 'Hi'`
become a real call": there is no generated glue for `setTitle:` anywhere. The
bridge asks the OS what `setTitle:`'s shape is, marshals one `String`
argument into the `Id` slot the shape says to use, and lets the shim send it.

### 4.2 Reverse — an AppKit callback becomes a Smalltalk send

This is C6's job, and it is the mirror image of §4.1: instead of Smalltalk
asking the runtime for a shape and marshaling out, a typed `extern "C"` IMP —
one per selector, registered in §3.2 — receives already-ABI-shaped arguments
and marshals *in*. Full round trip, `imp_number_of_rows` as the example
(the shape for every other IMP is the same skeleton):

1. **AppKit calls the IMP directly** — `numberOfRowsInTableView:` on a live
   `MacvmTableSource` instance, ordinary `objc_msgSend` from AppKit's side,
   no bridge involved yet.
2. **`dispatch`** (`objc_delegate.rs:132`) looks the instance pointer up in
   the process-wide `DELEGATES` registry. Three fail-closed checks, in order,
   each answering the shape's zero default (`0` rows / `NO` / `nil`) rather
   than touching the VM: the instance is unknown; its recorded generation
   doesn't match `current_ui_vm_generation()` (§7.5 — a delegate minted
   against a since-restarted UI worker); a delegate callback is *already*
   running on this thread (`callback_active()` — no legitimate nested-callback
   path exists yet, but the check keeps the single per-thread recovery slot
   sound in advance).
3. **`dispatch_callback`** (`src/embed.rs:744`) runs the handler as a
   **top-level VM entry** — the same per-entry `sigsetjmp` recovery door
   `VmHandle::eval` uses. This is sound specifically because the UI worker
   VM is quiescent in `[NSApp run]` between callbacks (§2 step 5): the
   trampoline is never a re-entrant borrow of a live `&mut VmState`, so a
   handler that raises, or a native fault inside the marshalling, unwinds
   cleanly back here and the run loop just keeps pumping.
4. **`perform_delegate`** (`objc_delegate.rs:170`) marshals each ABI arg to a
   Smalltalk oop — `ArgVal::Id` through `objc_bridge::wrap` (a fresh,
   retained `ObjcRef` — §6), `ArgVal::Int` through `SmallInt::new` — builds a
   Smalltalk `Array` of them, and calls the world-side entry point directly
   by method lookup: `MacvmDelegate class >> dispatchTicket:selector:
   arguments:`.
5. **`MacvmDelegate class >> dispatchTicket:selector:arguments:`**
   (`world/65_cocoadelegate.mst:66`) is plain Smalltalk: look `ticket` up in
   a class-var `Registry` `Dictionary`, and if it names a live receiver,
   `^receiver perform: aSelector withArguments: args` — **the reflective RPC
   primitive built for worker `call:on:args:onReply:`, reused verbatim.**
   The ObjC selector name *is* the Smalltalk selector the handler
   implements — no separate selector-name map.
6. **`marshal_ret`** (`objc_delegate.rs:238`) reads the handler's answer back
   into the ABI return register per the callback's static `RetShape`
   (`Void`/`Bool`/`Int`/`Id`/`ItemId` — the last for values AppKit will
   *retain across run-loop turns*, like an outline item; only a
   world-retained `ObjcRef` may cross there, never a freshly autoreleased
   `NSString`, which would dangle once the pool drains).

A data-source callback therefore never asks the primary VM anything — the
handler answers from the UI worker's own **local snapshot** (§7.4), which is
*why* the round trip above never blocks: step 3 through step 6 are all
same-thread, same-VM, synchronous Smalltalk execution.

## 5. Two memory models, one process

Smalltalk objects live under a moving, tracing GC: addresses change, and only
the collector's own root/scan machinery may hold a raw pointer across a
safepoint. Cocoa objects live under manual reference counting: addresses are
stable for life, and ownership is a count, not a scan. Neither model can be
taught the other's rules, so the bridge's job is to make sure **neither one
has to be**.

### 5.1 `ObjcRef` — a raw `id` the GC never scans

Every native object crossing into Smalltalk is wrapped as an `ObjcRef`: a
`MemOop` whose payload is not named ivar slots but an **8-byte indexable byte
tail** (`ByteArrayOop`) holding the raw `id`, little-endian
(`objc_bridge.rs:304`, `wrap`). This is deliberate and specific: `Alien`
(the FFI raw-memory type) stores its address as a `SmallInteger` in a *named*
slot, which the GC's oop-scanner walks looking for heap pointers to relocate.
An arm64 tagged-pointer `id` (a small `NSNumber`, an `NSTaggedPointerString`)
sets bit 63 of the pointer word — out of `SmallInteger` range — so a raw `id`
sitting in an oop-scanned slot risks being misread as a moved heap pointer and
"relocated," corrupting the object. The byte tail is the one storage class
the collector genuinely never walks, for any reason, so it's the only place
an opaque 64-bit foreign value can sit safely.

### 5.2 Retain-on-wrap, and the +1-family classifier

Every `id` crossing in is `objc_retain`'d **before** it is boxed
(`wrap`) — the wrapper's lifetime is the bridge's own strong reference,
independent of whatever transient ownership Cocoa's call convention implied.
Left at that, a `+1` result (from `alloc`/`new`/`copy`/`mutableCopy`, which
already hand the caller ownership) would be double-retained and leak. So
`wrap_result` (`objc_bridge.rs:374`) first classifies the *producing
selector* with `selector_family` (`objc_bridge.rs:349`) — ARC's own family
rule, mechanized verbatim: ignoring a leading underscore, the selector's
first component is in a family if it's followed by a character that is not a
lowercase letter (`copy`, `copyWithZone:`, `_copyDeep` qualify; `copyright`,
`newButtonTitle` do not) — and skips the extra retain for `Plus1`/`Init`
results (`wrap_owned`). The `init` family gets one more step:
`consume_receiver` (`objc_bridge.rs:414`) poisons the *receiver's* wrapper
without releasing it, because an init call may return a different object than
its receiver (class clusters) and the receiver's `+1` was consumed by the
call, not dropped — mechanizing "always use the init result, never the alloc
result" so a caller cannot get this wrong by construction.

### 5.3 Release-with-poison; leaks over corruption, always

`release` (`objc_bridge.rs:433`) calls `objc_release` on the wrapped id, then
**zeroes the byte tail** — every later send through that wrapper hits
`read_id`'s `id == 0` check and fails cleanly (`objc_bridge.rs:290`) instead
of dereferencing a dangling pointer. A double release is refused outright (the
tail is already zero, so there's nothing to release). This is a load-bearing,
explicit bias: `WRAPS`/`RELEASES`/`CONSUMED` are process-wide atomic counters
kept in quiescent balance (`WRAPS == RELEASES + CONSUMED`) so a leak is
*diagnosable* after the fact (`MACVM_TRACE=cocoa`, or the counters directly);
an over-release corrupts a runtime the bridge does not control and cannot
recover from. Given a choice between the two failure modes anywhere in this
module, the code always takes the leak.

### 5.4 The other direction: tickets, never oops

Ownership only has to reconcile one way — because the contract that makes the
reverse direction (§4.2, §3.2) safe is stricter: **no Smalltalk oop is ever
stored ObjC-side, at all.** The `DELEGATES` and `ACTIONS` registries
(`objc_delegate.rs:72`, mirroring `objc_bridge.rs`'s own `ACTIONS`) map an
ObjC instance pointer to a plain **integer ticket**; the actual Smalltalk
receiver lives in a GC-rooted class-var `Dictionary`
(`MacvmDelegate class >> Registry`, world-side). A moving GC is free to
relocate that receiver at any safepoint — nothing ObjC-side ever held its
address, so there is no pointer to fix up. This is why the design doc can
say the GC needed **zero** changes for this bridge: no new root kind, no scan
hook, no pin bit. The ObjC retain count *is* the external root on one side;
a ticket into a rooted Dictionary is the anchor on the other. Neither model
had to learn about the other.

### 5.5 Autorelease pools: two disciplines, because there are two kinds of thread

`autorelease` on a thread with no pool doesn't defer — it leaks outright. So
the first Cocoa call on a VM thread pushes a **bottom pool**
(`ensure_pool`, `objc_bridge.rs:240`), drained and replaced at every doit
boundary (`drain_pool_at_doit_boundary`) — a natural quiescent point, since no
Cocoa call is ever in flight between doits on that thread.

**Except on main.** There, `ensure_pool` is a deliberate no-op
(`objc_bridge.rs:241`): AppKit and CoreFoundation already own the pool stack
on the main thread — a per-event pool around `[NSApp run]`'s dispatch, a
per-callout pool around every run-loop source and timer. Pushing our own
bottom pool there would nest a token *below* theirs; draining it later
(`drain_pool_at_doit_boundary`, also a no-op on main for the same reason)
would pop a page CF still expects live, aborting the process
(`AutoreleasePoolPage::badPop`). On main, every autoreleased object instead
rides whichever CF pool is already open for that callout — `cocoa_gui`'s
`main.rs` pushes exactly one explicit pool around its own pre-`[NSApp run]`
startup work, and nothing after that needs one of its own.

## 6. The UI VM talking to the primary VM

### 6.1 Why two VMs, briefly

AppKit forces exactly one thing: the interface runs on the process's main
thread. Putting the *persistent* VM there too was tried in an earlier draft
and rejected for two reasons `cocoa_gui_design.md` §2 covers in full: a fatal
doit on main has no safe recovery (only `pthread_exit`-the-process or an
unsound `siglongjmp` through AppKit frames), and a long doit on main freezes
painting. Keeping the primary on a background thread gets both back: a fatal
doit `pthread_exit`s a thread that isn't main, a supervisor respawns it
(S21), and a long doit blocks the primary, not the paint loop.

### 6.2 The request protocol

Everything that crosses between the two VMs is a tagged message over the
worker-inbox transport (`runtime::workers`), shaped:

```
UI → primary:   { #uiReq. corr. #doit. source }
                { #uiReq. corr. #refresh. viewId }
                { #uiReq. corr. #select. viewId. path }
                { #uiReq. corr. #saveMethod. className. side. selector. source }
primary → UI:   { #uiReply. corr. payload }              " a doit result "
                { #snapshot. viewId. generation. dataTree } " unsolicited, corr-free "
```

`corr` is namespaced `(peer, corr)` rather than bare — each VM runs its own
independent `NextCorr` counter, and without the peer tag a UI-worker-issued
request could collide with an outstanding request the primary itself has open
under the same number, firing the wrong continuation. Primary-side,
`dispatchOne:` carries an `isUiReq:` branch dispatching by verb, and `reply:`
(previously worker-side only) now also works from the primary.

### 6.3 Wake and drain

Covered in full in `cocoa_gui_flag_and_drain.md`; the short version: the
primary→UI direction needed a *new* wake, because the UI worker blocks in
`[NSApp run]`, not in `recv()` the way a compute worker does — a
`performSelectorOnMainThread`-shaped run-loop poke, not M3's coalesced
inbox wake. The drain that services it is installed as a `CFRunLoopSource`
in `NSDefaultRunLoopMode` **only**, deliberately never the common modes,
because a common-mode source also fires inside AppKit's own nested run loops
(menu tracking, live resize, a modal panel) — swapping VM-backed state
mid-tracking throws AppKit consistency exceptions. The UI is allowed to go
briefly stale during a tracking session; the drain catches it up the instant
control returns to default mode.

### 6.4 Snapshots: names, never oops

The pickle used to move data between VMs refuses classes, methods, closures,
and contexts outright (`mop.rs`) — and the web GUI's view *models*
(`ClassHierarchyOutliner`, `ClassMirror`, …) hold real klass oops, so a
snapshot can never be "pickle the model." What crosses instead is a plain
nested tree of Strings, Symbols, and `SmallInteger`s — exactly what those
models already emit as `htmlFragment` for the web GUI, just captured as data
instead of HTML: `{ #snapshot. viewId. generation. dataTree }`. `viewId`
routes the blast to the right terminal-side view; `generation` is a monotonic
staleness guard a terminal uses to drop an out-of-order blast. This is what
makes §4.2's data-source callbacks free of any cross-VM wait: the UI worker
already has the whole answer sitting locally, re-sent whole (never patched)
every time the primary's model changes — blast, don't patch
(`feedback_blast_dont_patch`).

### 6.5 Generations, and failing closed on a stale delegate

`current_ui_vm_generation()` (`src/embed.rs:134`) is a process-wide counter
bumped every time `publish_ui_vm` publishes a **non-null** handle — i.e.,
every UI-worker boot or restart. A C6 delegate records the generation live at
the moment it's minted (§3.2, `DelegateEntry.gen`); `dispatch` (§4.2 step 2)
refuses to fire a delegate whose recorded generation doesn't match the
current one. This is what makes an in-place UI restart
(`cocoa_gui_design.md` §5, Layer 2 — an AppKit-internal fault rebuilds the
whole UI worker without touching the primary) safe even while a *stale*
delegate instance from a not-yet-closed window is still technically alive:
it fails closed — a defined default, never a dispatch into a VM that no
longer exists — rather than needing every old delegate hunted down and
invalidated individually.

## 7. Where the code lives

| Concern | File |
|---|---|
| Forward send, shape resolution, `ObjcRef` wrap/release | `src/runtime/objc_bridge.rs` |
| C6 reverse dispatch: role classes, registry, `dispatch`/`perform_delegate` | `src/runtime/objc_delegate.rs` |
| The `@try`/`@catch` exception shim | `objc_shim.m` (compiled by `build.rs`) |
| Top-level callback entry + recovery door, UI-VM generation | `src/embed.rs` |
| World-side ticket→receiver registry | `world/65_cocoadelegate.mst` |
| Boot handshake (two VMs, parked main) | `cocoa_gui/src/boot.rs` |
| `main()`, the drain loop, metrics | `cocoa_gui/src/main.rs` |
| Flag-and-drain modules (rebuild / restart / view-refresh) | `cocoa_gui/src/{rebuild,primary_restart,view_refresh}.rs` |
| The memory-model contract (design rationale) | `docs/cocoa_bridge_design.md` |
| The three-VM architecture (design rationale, review) | `docs/cocoa_gui_design.md` |
| The flag/wake/drain mechanism (one mechanism, in depth) | `docs/cocoa_gui_flag_and_drain.md` |
