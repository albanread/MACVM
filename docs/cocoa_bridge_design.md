# The Cocoa bridge — a moving GC meets a reference-counted runtime

**Status: design.** This document closes the point `FFI.md` §4 deliberately
left open — *"an `id` a Smalltalk oop wraps needs a retain on wrap and a
release when the wrapping oop is collected … a real design point, sketched
but not closed here"* — and adds the threading, callback, and exception
models. It does **not** re-design dispatch or marshalling: Tier-2 DNU
dispatch, PIC-cached resolution, shape-keyed AAPCS64 trampolines, and the
`cocoa_data` ABI database are `FFI.md` §3–§6 and stand as written. Read that
first; this is the memory-model half.

```smalltalk
| win |
Cocoa poolDo: [
    win := (Cocoa classNamed: 'NSWindow') alloc
        initWithContentRect: (CGRect x: 200 y: 200 w: 420 h: 160)
        styleMask: 15 backing: 2 defer: false.
    win setTitle: 'Hello from Smalltalk'.
    win makeKeyAndOrderFront: nil ].
```

## 1. The problem, stated honestly

MACVM and Cocoa disagree about everything that matters to a pointer:

| | MACVM | Cocoa / Objective-C |
|---|---|---|
| Lifetime | tracing GC (scavenge + full compact) | reference counting (retain/release + autorelease pools) |
| Motion | objects **move** (eden→survivor→old, compaction) | objects never move |
| Liveness | reachability from roots, decided by the collector | a counter, decided by whoever calls `release` |
| Threading | one heap = one thread, strictly | per-class rules; AppKit = main thread only |
| Failure | deopt/trap machinery, S21 recovery | `NSException` unwinding, undefined across foreign frames |

A naïve integration fails in both directions at once: a Cocoa object holding
a raw oop is corrupted the first time a scavenge moves it, and a Smalltalk
object holding a bare `id` leaks it (no release ever) or kills the process
(a release after Cocoa already deallocated).

**The size asymmetry the title names is real and shapes the whole design:**
Cocoa is half a million methods, sixty frameworks, and thirty years of
ownership conventions. We cannot audit it, model it, or teach our GC about
it. The only stable posture for a small VM is to touch it through a
**narrow, mechanically-checkable contract** and refuse everything the
contract doesn't cover — the same instinct as MOP's loud refusals, applied
to a runtime instead of a pickle.

## 2. The contract: copies and tickets, never live pointers

The workers arc (docs/multi-smalltalk-worker.md) already proved the
discipline this bridge needs, on an easier opponent: **two memory managers
never see each other's pointers; everything that crosses is a copy or a
stable ticket**. Cocoa is a third memory manager. Same rule, three clauses:

1. **A Cocoa reference lives in Smalltalk only inside an `ObjcRef`** — the
   `Alien` mechanism (`runtime/alien.rs`), precisely: the raw `id` is stored
   **smi-tagged in a named slot** (`AlienOop::external_addr`'s exact idiom —
   `SmallInt` in, `SmallInt` out). The GC *does* scan that slot and sees a
   perfectly valid SmallInteger — it never chases it, because a smi IS an
   oop, just an immediate one. (macOS arm64 userspace addresses are ≤ 2^47,
   far inside smi range, asserted at wrap time.) The GC may move the
   *wrapper* freely; the ObjC heap never moves the *target*; neither
   collector ever traverses the other's graph.
2. **No oop is ever stored ObjC-side.** A Cocoa object that needs to refer
   back to Smalltalk (a delegate, a target/action, a callback) holds a
   **ticket**: a plain integer index into a VM-side, GC-rooted registry
   (§6). Tickets survive every GC trivially because the GC updates the
   registry's oops, not the ticket. This is the GamePane pattern
   (`StepBlock` class var + monotonic sprite ids) promoted to a rule.
3. **Data crosses by copy.** A Smalltalk `String` argument becomes a fresh
   `NSString`; an `NSString` result becomes a fresh Smalltalk `String`.
   Structs (`CGRect`…) marshal by value through the ABI trampolines. No
   Cocoa API is ever handed the address of anything the GC owns — which is
   also why FFI calls need no new GC cooperation: the VM is single-threaded,
   the collector only runs at safepoints on the VM thread, and a thread
   inside `objc_msgSend` is by definition not at a safepoint.

Everything below is the elaboration of these three clauses.

## 3. Ownership: who retains, who releases, and when

### 3.1 Retain on wrap — the one rule that makes autorelease a non-issue

Every `id` that crosses INTO Smalltalk is **retained by the bridge before
boxing** (`objc_retain`), so an `ObjcRef` always owns exactly one strong
reference. This single rule collapses the classic autorelease trap: a
method returning an autoreleased object is safe to wrap even though the
enclosing pool will drain later — the pool releases the *autorelease*
reference; ours survives. Pool timing becomes a memory-pressure question,
never a correctness question.

**But a pool must EXIST.** An `autorelease` on a thread with *no* pool in
place doesn't defer — it leaks outright (with a console warning), and the
VM thread has no pool unless we give it one. So the bridge installs a
**bottom autorelease pool on the VM thread at init** and drains + renews it
at doit boundaries (a natural quiescent point: no Cocoa call is in flight
between doits). `poolDo:` then exists for tight loops *within* a doit, not
as the only line of defense. This is a C0 requirement, not a refinement —
without it every +0 return leaks its autorelease reference invisibly.

### 3.2 The +1 family — mechanize Cocoa's naming convention, don't fight it

Cocoa's ownership rules are conventions over selector names, and they are
mechanical — it's exactly the analysis ARC's compiler performs:

- Selectors beginning `alloc`, `new`, `copy`, `mutableCopy` return an
  object the caller **already owns** (+1). Wrapping one of these results
  skips the bridge retain (retaining again would leak).
- Everything else returns +0 (borrowed / autoreleased) — bridge retains.

The classifier is a prefix test at resolution time, cached in the same PIC
entry as the ABI shape (`FFI.md` §3), so it costs nothing per call. It can
be wrong only where Cocoa itself violates its convention — rare, documented
cases; the escape hatch is an explicit `asRetained` / `asBorrowed` override
on the wrapper for the odd corner, never a global heuristic change.

### 3.3 Release: explicit and poisoning (v1), finalization later (v2)

MACVM has **no finalization** today (weak refs + finalization is Phase E,
unbuilt). The v1 design refuses to pretend otherwise:

- `ref release` is **explicit**: sends `objc_release`, then **poisons the
  wrapper** (zeroes the payload; every subsequent send through it fails
  cleanly — the terminated-worker discipline, not a dangling pointer).
- `Cocoa poolDo: [:pool | …]` is the ergonomic path: it pushes an
  `NSAutoreleasePool`, runs the block, then **releases every ObjcRef minted
  inside the block** (the bridge threads a mint-list through the dynamic
  extent) except those the block explicitly `keep`s — scoped ownership,
  shaped like Smalltalk, covering the overwhelmingly common "make some
  objects, use them, drop them" case with zero manual releases.
- An `ObjcRef` collected **without** release leaks its +1 — deliberately.
  A leak is recoverable and diagnosable (a debug mint-counter,
  `MACVM_TRACE=cocoa`, reports wrap/release imbalance per class); an
  over-release is memory corruption inside a runtime we don't control.
  When in doubt the bridge always chooses the leak.
- **v2 hook**: when Phase E finalization lands, `ObjcRef` gets a finalizer
  that releases the payload, and `poolDo:`/`release` become optimizations
  rather than obligations. The design requires nothing from finalization
  now, and gains from it later — the same posture the pickle took toward
  bounded mailboxes.

### 3.4 What the GC needs to know: nothing

This is the punchline of the contract, and worth stating flatly: **the GC
is not modified.** No new root kind, no scan hook, no pin bit, no
finalizer queue (v1). An `ObjcRef` is an ordinary object whose payload slot
holds a smi — scanned, valid, and inert to the collector by the same proven
mechanism `Alien`'s external addresses already use. The retain count *is* the external root — Cocoa's
own memory manager holds the object alive on our behalf, which is what
reference counting is for. The enormous runtime keeps its memory model; the
small VM keeps its; the contract is eight bytes of opaque payload and a
counter neither side shares.

## 4. Threading: the VM thread, the main thread, and why sync is safe here

Foundation is broadly thread-safe; **AppKit is main-thread-only**. The VM
lives on its own thread (S21). Three call paths:

1. **VM-thread direct** (default): Foundation-ish calls run right on the VM
   thread through the trampolines — synchronous, fast, no hop. This is C0's
   whole world.
2. **Sync hop to main**: `performSelectorOnMainThread:…waitUntilDone: YES`
   (already in `gui/src/objc.rs`). Normally sync-waiting on another thread
   invites deadlock — but this architecture has already paid for the proof
   that it can't: **the main thread never synchronously waits on the VM**
   (vm_host is async by construction — submit + drain-on-wake, the S21/M3
   design). No wait cycle can close. The invariant to preserve, stated as a
   rule: *main→VM communication is always async (queued requests); VM→main
   may therefore be sync.* A debug assert in the hop primitive checks the
   calling thread isn't main (a Cocoa callback must never sync-hop).
3. **Async hop with continuation**: for fire-and-forget AppKit work and for
   anything initiated from a callback, the bridge reuses the **worker
   inbox** transport wholesale: the main thread holds an `InboxSender`
   clone (they're `Send + Clone` by design), and a completed async Cocoa
   call posts an envelope whose reply routes through the same
   `send:onReply:` continuation machinery workers use. Cocoa-on-main
   becomes, in effect, one more star-topology peer with a reserved id — no
   new dispatch surface, no new wake path, the coalesced wake already built
   in M3 carries it.

## 5. Exceptions: catch them in Objective-C or don't play

An `NSException` unwinding through Rust (or JIT) frames is undefined
behavior — the same reason S21 rejected `catch_unwind` across JIT frames.
The bridge therefore never lets one reach us: the trampoline family gains a
variant compiled from a **small Objective-C shim** (a `.m` built by
`build.rs` — the one new build dependency this design admits, and only for
targets where the bridge is enabled):

```objc
macvm_try_msgsend(shape, target, sel, args, out_result, out_exc_desc)
// @try { CALL the shape trampoline — a genuine call, never a tail call:
//        the @catch personality scope lives in THIS frame, so the frame
//        must still be on the stack when the exception unwinds }
// @catch (NSException *e)
// { copy e.description into out_exc_desc; return 1; }
```

An exception surfaces as a **primitive failure** carrying the description —
the world method's fallback raises an ordinary Smalltalk error, S21's
recovery applies if unhandled, the process never dies. (Cocoa exceptions
are programmer errors by Apple's own doctrine, so "Smalltalk error, doit
aborted, VM fine" is exactly proportionate.) A genuine crash *inside* a
Cocoa call — a `SIGSEGV` in framework code — is already covered: PROBE's
foreign-fault recovery (S21 step 1c) was built for precisely this and has
been catching bad `Alien` dereferences since S20.

## 6. Callbacks: Cocoa calls Smalltalk

The reverse direction is where naïve bridges corrupt themselves, because it
tempts you to hand Cocoa an oop. The contract forbids it; the mechanism is
the one the GUI already uses for its own delegates
(`allocate_class`/`add_method`, `MacvmDemosDelegate` et al.):

- **One** ObjC trampoline class, `MacvmAction`, created at bridge init. Its
  IMPs never touch an oop: each instance carries a **ticket** (an `i64` in
  an ivar) and its action IMP posts an envelope
  `{#cocoaEvent. ticket. pickled-args}` through the main thread's
  `InboxSender` — then returns. Delivery, wake, and dispatch are the
  worker inbox path, unmodified.
- VM-side, a GC-rooted registry (class-var `Dictionary`, ticket → block —
  the `PendingReplies` pattern) maps the ticket to its handler when the
  envelope dispatches, **between doits, on the VM thread** — a callback
  never interrupts Smalltalk mid-send, preserving the strictly-serial
  invariant that everything since the game loop has relied on.
- Callback arguments cross as **copies** (scalars, strings) or as freshly
  wrapped `ObjcRef`s (retained per §3.1). v1 supports target/action and
  simple delegate methods whose arguments the marshaller covers; blocks-as-
  arguments and delegate protocols with exotic signatures are explicit
  non-goals until the need is real.
- Tickets are monotonic and never reused (the sprite-id rule);
  unregistering poisons the registry entry, and a late event for a dead
  ticket is dropped with a trace line, not an error.

## 7. What v1 refuses (the non-goals that keep the VM small)

- **No oop pinning, no conservative scanning, no GC changes.** If a design
  option requires the collector to learn about Cocoa, the option is wrong.
- **No automatic reclamation** until finalization exists — `poolDo:` and
  explicit `release`, with leak-side failure bias (§3.3).
- **No ObjC blocks as arguments, no protocol conformance synthesis, no
  subclassing Cocoa classes from Smalltalk** (the `MacvmAction` trampoline
  is bridge-internal, not a user surface). Each is a self-contained later
  arc; none is needed for windows, buttons, text, timers, or menus.
- **No struct coverage beyond the HFA/small-aggregate set** the trampolines
  already classify (`CGRect`/`CGPoint`/`CGSize`/`NSRange` first) — grown
  row by row from `cocoa_data`, never speculatively.
- **No thread-affinity inference.** v1 ships a curated main-thread-only
  class list (AppKit/WebKit prefixes); getting it wrong fails loudly via
  Cocoa's own main-thread checker rather than silently via ours.

## 8. Milestones (each lands green and alone, per the standing rules)

| | contents | gate |
|---|---|---|
| **C0** | `ObjcRef` klass (Alien-mechanism payload) + retain-on-wrap / explicit-release-with-poison + the exception shim + id/scalar shapes through the existing FFI trampolines. Foundation only, VM thread only, headless. | unit + world tests: `NSProcessInfo processName` round-trips; poisoned ref fails cleanly; wrap/release counters balance under `MACVM_TRACE=cocoa`; GC_STRESS soak with churning wrappers |
| **C1** | Marshalling breadth from `cocoa_data`: BOOL/NSUInteger/double, `NSString`↔`String` copy bridging, HFA struct set, the +1-family classifier | differential tests against known ABI shapes; leak-counter gate |
| **C2** | `doesNotUnderstand:` dispatch on `ObjcRef` + PIC-cached resolution (+ the `#class`-collision escape prefix, per `ObjectiveCAlien`) | a Workspace doit drives Foundation with keyword sends; PIC hit-rate visible in stats |
| **C3** | Threading: sync main-thread hop (with the thread assert) + async hop riding the worker-inbox transport | vm_host test: an AppKit call from the VM thread completes; deadlock invariant test |
| **C4** | Callbacks: `MacvmAction` tickets + registry + target/action | GUI test: an `NSButton` runs a Smalltalk block |
| **C5** | **Capstone: CocoaPad** — a native `NSWindow` + text field + button built entirely from a Workspace doit, event round-trip and all | on-screen (user), plus a headless registry/envelope test |

Verification throughout follows the standing rules: RELEASE + PARALLEL
stress, GC_STRESS with wrapper churn, t=1 and threshold≥200, and the
wrap/release balance counter as a permanent tripwire (the FFI arc's
`prim_fails`-panic and BUG-A lessons say boundary code earns its stress
tests before it earns features).

## 9. Relationship to what exists

- **`FFI.md` §3–§6** — dispatch, PIC reuse, trampolines, `cocoa_data`,
  `ffi_gen`: the substrate, unchanged. This doc plugs its flagged §4 gap.
- **`runtime/alien.rs`** — the byte-payload wrapper mechanism `ObjcRef`
  reuses (a distinct klass, so `Alien`'s raw-memory semantics and
  `ObjcRef`'s ownership semantics never blur).
- **`gui/src/objc.rs` / MacGamePane's bridge** — the proven Rust-side
  `objc_msgSend` technique; the bridge generalizes it behind the trampoline
  ABI rather than hand-writing one Rust function per shape.
- **docs/multi-smalltalk-worker.md** — the boundary philosophy (copies and
  tickets), the inbox transport the async hop and callbacks ride, and the
  poison-on-death discipline the wrapper borrows.
- **SPRINTS Phase E, S20 step 7** — this design is that step's design
  deliverable; implementation follows the milestone ladder above as its own
  side-track arc.
