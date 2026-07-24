# MACVM Help

*A digested, summarized copy of the WKWebView GUI's own help pages
(`gui/reference/pages/macvm-help/`), reformatted for the native Cocoa GUI's
Help tab — plain Markdown, since this window has no HTML renderer. For the
full, unabridged version see the source pages, or `README.md` and `docs/`
for the authoritative, up-to-date record.*

## What MACVM is

MACVM is a virtual machine for Smalltalk, written from scratch in Rust for
macOS on Apple Silicon. It is in the **Self → Strongtalk** lineage: a
class-based object model with an *adaptive optimizing compiler* driven by
type feedback. It runs a real object world, and on the standard benchmarks
the optimizing compiler owns essentially all of the runtime.

Strongtalk (Animorphic Systems, mid-1990s, later released by Sun) paired
adaptive optimization — run first, then recompile hot code with type
feedback, the technique the Self group pioneered — with the first workable
*optional* static type system for Smalltalk and a live, hypertext
programming environment. MACVM is a fresh implementation from Strongtalk's
published design, not a fork of its source: the bytecode interpreter and
optimizing compiler are new Rust, the assembler is reused from a sister
project, the garbage collector is new.

## The virtual machine

**Object model.** Class-based, not Self's prototype/map model. An object
reference is a **direct tagged pointer** — the reference *is* the machine
address, with a 2-bit tag distinguishing a `SmallInteger` (value rides in the
pointer itself, never allocates) from an ordinary object pointer. A heap
object has a two-word `[mark][klass]` header, then its own fields. That's
the whole overhead.

**No object table.** Classic Smalltalk systems index every reference through
an object table, which makes `become:` nearly free and relocation a
single-slot patch — at the cost of an indirection on *every* field access,
forever. MACVM has no object table: a pointer is the address, so reading a
field is one load. The cost comes due when the collector moves an object —
it must find and rewrite every pointer to it, wherever it lives.

**The garbage collector.** Two collectors of a familiar shape: a
generational scavenger for the young generation, and a full compacting
collector for the whole heap. The interesting part is that both run
**underneath live, moving compiled frames** — native code holds `oop`s in
registers and stack slots, and a compacting collector must find and update
every one of them, precisely, without ever mistaking a raw integer for a
pointer. Two mechanisms make that safe:

**Precise oop-maps** — at every safepoint, the compiler has recorded exactly which registers/stack slots hold oops vs. raw bits.

**A mixed-tier frame walker** — understands a stack that interleaves interpreter and compiled frames from different methods and tiers.

The payoff: compiled code can keep values in registers **across a GC
safepoint** — no forced spill-everything discipline, no forbidding
collection mid-method.

## The optimizing compiler

Two tiers: a simple dispatch-based bytecode interpreter that starts
instantly, and a tier-1 optimizing JIT that recompiles hot methods with type
feedback. **Run first, then recompile** — you never pay to optimize code
that runs once, and by the time a method is hot the compiler knows what
types actually flow through it.

**Type feedback.** Every send site starts monomorphic: the interpreter
records the receiver's klass right at the call site (an inline cache). A
second klass grows it into a **polymorphic inline cache** (PIC) — a
measured record of every receiver klass actually seen, in this program, on
this data. The compiler never guesses.

**What the feedback buys:** method inlining (a monomorphic call becomes
straight-line code), block inlining (literal blocks — including ones with a
non-local `^` return — splice inline rather than allocate a closure),
per-klass customization (a method compiles once per receiver klass, so its
own `self` is a compile-time constant), and devirtualization (self-sends and
block invocations stop being dynamic dispatch).

**Deoptimization and OSR.** All of this is speculation, and speculation can
be wrong (a redefinition, a klass this site never saw before). When a guard
fails, the compiled frame is **deoptimized**: reconstructed as interpreter
frames with live values in their proper slots, and execution continues in
the interpreter as if the method had never been compiled — never a wrong
answer, never a fast approximation. **On-stack replacement (OSR)** is the
reverse door: a loop that turns hot *while already running* can be entered
in compiled form mid-flight. Because bailing out is always correct, the
compiler can afford to be greedy.

**Coverage achieved, measured on a fanless MacBook Air:**

**Methods that run, compiled to native code** — ~98.7%

**Executed bytecode-work running as compiled code** (real workloads) — 98.6–99.8%

**`deltablue`** interpreter → JIT — 214 ms → 4 ms (~53×)

**`richards`** interpreter → JIT — ~205 ms → ~6 ms (~34×)

**`sieve`** interpreter → JIT — 88 ms → 9 ms (~10×)

**`ctxloop`** (closure/OSR microbenchmark) — 134 ms → 1 ms (~134×)

The system reaches steady state and holds it — closures included — rather
than thrashing between tiers.

## Fast floating point

Strongtalk sketched an experimental fast-float scheme (a `<FloatVal>`
annotation, hand-written box/unbox, no crossing a method boundary) and left
it unfinished. MACVM finishes it with **no annotation required**: ordinary
Smalltalk float arithmetic gets the fast path automatically.

The mechanism is the **float region**: at a send site where the inline
cache has only ever seen `Double`, the compiler emits a guarded unbox,
native `fmul`/`fadd`/`fcmp`, and a re-box only where a boxed value is
actually observed — no allocation, no GC interaction, no send, inside the
region. It rests on a second, independent register file for unboxed floats
(invisible to the moving GC — never in an oop map), a box/unbox reducer that
cancels redundant conversions and sinks boxing onto the deopt-only path, and
one new deopt-map kind (`DoubleSlot`) that lets an unboxed double travel
across a method boundary and still be reconstructed correctly on a bail-out.

**Measured on the Mandelbrot demo** (420×220, release build, fanless Air —
each row removes one whole category of cost):

**boxed sends** (baseline) — 746 ms, 708 MB/render

**pixel-buffer output** — 458 ms, 595 MB/render

**float-region fuse** — 180 ms, 595 MB/render

**sunk boxing + temp promotion** — 166 ms, 4 MB/render

**strength-reduced coordinates** — 38 ms, 0 allocation

**d-register residency** — 25 ms, 0 allocation

Roughly 30× end to end; the last two rows are zero allocation, zero deopts,
one scavenge-free heap per render. A hand-ported C build of the identical
kernel, best of thirty runs, lands in the same neighbourhood. Full
derivation: `docs/float_fastpath_design.md`.

## SIMD vector arithmetic

The same idea one register width up: the NEON unit's 128-bit vectors (two
64-bit lanes or four 32-bit lanes), exposed directly as immutable value
classes — `Float64x2`, `Float32x4`, `Int32x4`. Each is a fixed 16-byte value
the GC never scans, boxed at rest, register-resident only inside a compiled
region. Protocol: `splat:`/`x:y:z:w:` to build, `+`/`*`/`min:`/`sqrt`
elementwise, `at:`/`sum`/`dot:` to extract or reduce. A mono-vector-class
send fuses into a short NEON run — guard, `ldr q`, the native op, re-box —
roughly **13–15× over the interpreter**.

Elementwise ops keep bit-for-bit parity with the scalar path (lane *i* of a
vector op is exactly the scalar op on lane *i*). Reductions are the one
place that needs honesty: floating-point addition isn't associative, so a
pairwise-tree `sum` isn't bit-identical to a sequential fold — it's its own
defined operation, verified against that definition, with a separate
`sequentialSum` when the exact scalar order matters. Integer reductions have
no such caveat.

Above the individual vectors, `FloatArray` is a contiguous GC-skip buffer
with bulk NEON kernels (`+@`, `scale:`, `dot:`, `sum`, `max`, `min`) written
as **explicit NEON intrinsics** — never a scalar loop left to auto-vectorize.
The vector path drops in wherever work is lane-independent (array math,
colour/coordinate transforms, DSP, pixel packing); it does *not* drop in
where control flow is data-dependent per-lane (Mandelbrot's escape loop is
the standing counter-example). Full design: `docs/SIMD.md`.

## Cocoa from Smalltalk

MACVM talks to macOS directly: Foundation and AppKit objects are ordinary
Smalltalk receivers. Look a class up once; from there Objective-C messages
are just keyword sends, selector text identical in both languages:

```smalltalk
s := (Cocoa classNamed: 'NSMutableString') alloc init.
s appendString: 'hello'.
s length.                "→ 5"
s asString.              "→ its -description, as a Smalltalk String"
s release.
```

**How it works.** A Cocoa object lives in Smalltalk as an `ObjcRef` — a
tiny wrapper holding one retained reference; the two memory managers never
see each other's pointers. Any message an `ObjcRef` doesn't itself
understand is forwarded to Objective-C, with argument and return types read
from the *live runtime's own method signatures* — integers, floats,
booleans, strings, and the small structs (`NSRange`, `CGPoint`, `CGRect`)
all convert automatically, both directions.

**What converts to what.** Smalltalk `String`s become temporary `NSString`s;
`NSString`-returning selectors (via `sendString:`/`asString`) come back as
Smalltalk Strings. Integers, Doubles, and `true`/`false` map to the declared
C types. `NSRange` results are 2-element Arrays `{location. length}`;
`CGPoint`/`CGRect` are Arrays of 2/4 Doubles (struct *arguments* are written
the same way). Everything else comes back as another `ObjcRef`.

**Ownership.** Every wrapper owns exactly one reference; send `release` when
done. A released wrapper is *poisoned* — later sends fail cleanly, a double
release refuses. Cocoa's own naming conventions are honored automatically:
`alloc`/`new`/`copy`/`init` results arrive already-owned, and `init`
invalidates the receiver it consumed (always use init's result, never
alloc's). For bursts of temporaries:

```smalltalk
s := Cocoa poolDo: [:pool |
    (Cocoa nsString: 'scratch').          "swept on exit"
    pool keep: (Cocoa nsString: 'mine')]. "survives; release it yourself"
```

The bridge always errs toward a *leak*, never a double-free — manual
reference counting (`retain`/`dealloc`…) is refused outright at the send
level. An `NSException` is caught and re-raised as an ordinary Smalltalk
error; the VM never dies from one.

**AppKit: onMain and actions.** AppKit is main-thread-only; the VM runs on
its own thread. Prefix any UI send with `onMain` to run it on the main
thread, synchronously:

```smalltalk
win onMain makeKeyAndOrderFront: nil.
```

Buttons call back the other way — `Cocoa action:` turns a block into a
target/action pair; the click is queued and the block runs on the VM thread
between doits, so a callback never interrupts running Smalltalk:

```smalltalk
act := Cocoa action: [ Transcript showCr: 'clicked!' ].
btn onMain setTarget: act.
btn onMain setAction: 'macvmFire:'.
```

**Name collisions.** A few selectors are Smalltalk's own and never reach
Objective-C: `class` (use `objcClass`), `hash`, `printString`, `isValid`,
`release`, and `=`/`==` (wrapper identity — use `isEqual:` for Cocoa
equality). For those, or any unusual shape, the explicit form is always
available: `ref send: 'class'` or `ref send: 'sel:' args: {…} ret: #i64`.

The full internal design — memory model, thread rules, exception handling —
is in `docs/cocoa_bridge_design.md`; the native GUI you are looking at right
now, built the same way, is `docs/cocoa_gui_implementation.md`.

## The programming environment

Two ideas from Strongtalk's "live page" concept carry through regardless of
which GUI you're using: a **class browser** where picking a method shows
editable source, and accepting an edit **live-compiles** it into the running
world (the next send goes to the new code, deoptimizing any stale compiled
version out from under itself) — and an **image store**, a SQLite database
holding the object world as editable rows (classes, methods, source) that a
boot loader reconstructs *byte-identically* to a classic source-file boot.
An edit you accept is persisted there, not lost on quit.

The WKWebView GUI (`macvm-gui`) additionally renders its whole interface as
HTML — plain pages extended with a `doit="..."` link that executes Smalltalk
instead of navigating, and a `smappl` tag that embeds a live widget. That
mechanism is specific to that shell; **this window has neither** — it's a
native AppKit interface, and this same tab you're reading is plain Markdown,
not a live page. The native shell has its own toolbar, a tabbed content
area, and View/Theme/Demos/Debug menus built directly from Cocoa, described
in `docs/cocoa_gui_implementation.md`.

## Introspection & reliability

A layered debugger, `DBG0` through `DBG5`, for a machine where interpreted
and compiled frames stack on top of each other: a **PROBE crash dossier**
(registers + a full mixed-tier backtrace, written to survive a foreign
fault rather than take the process out silently), breakpoints, an **arm64
disassembler** and **IR dumps**, and step-between-calls across the tier
boundary. Below that, always-available tracing channels via `MACVM_TRACE`:
`bytecode`/`count` (the interpreter stream), `gc`/`jit`/`deopt`
(collections, compilations, every deopt with its reason), `dnu`/`calls`/
`oops`/`stats`.

The rule that actually keeps it stable: **the interpreter is the oracle, and
the compiler is not allowed to disagree with it.** The small,
trust-by-inspection interpreter defines what a program *means*; the
optimizing compiler — inline caches, customization, inlining,
deoptimization, OSR and all — is held to **byte-identical** output. Every
JIT change is gated against that equality *and* against GC/deopt stress
matrices (the same work run while the collector moves frames underneath
live compiled code, and while speculation is deliberately broken). A change
that's faster but shifts one bit does not land.

## Multi-Smalltalk workers & supervision

Concurrency comes from **multiple whole VMs**, not green threads in one
heap: a primary plus up to 16 **worker VMs**, each with its own heap, JIT
and GC on its own OS thread, exchanging messages by **deep copy** — no
shared state, no identity across heaps, Erlang-style. Every send is
fire-and-forget; a reply runs later as a continuation
(`Worker spawn` / `send:onReply:`), never by blocking — the primary sleeps
until the next envelope wakes it, never polls. The Demos menu's
"Mandelbrot — parallel workers" computes every frame in bands across 4
worker VMs this way.

MACVM has no exception system — `self error:` stops the current
computation outright, and scoped `catch` handling was deliberately
rejected. The **supervision layer** answers the same problem the way
Erlang/OTP does: a crashed worker is reported as an ordinary message
(`#workerDied`), and a `WorkerSupervisor` restarts it by policy —
`#oneForOne` (just the dead child), `#oneForAll` (every sibling),
`#restForOne` (the dead child and everything started after it), with
supervisors nesting into trees and a child that exhausts its own restart
budget escalating to its parent. `WorkerNames` rebinds a service's name on
every restart, so calling code never holds a handle to a corpse, and
`ServiceWorker`'s deadline-bounded `call:timeoutMs:onReply:onError:`
funnels every failure mode — timeout, the target's death, an RPC error —
into one `onError:`, never a block. The `IoWorker` (a dedicated worker
multiplexing file descriptors through one `kqueue`, so no other VM ever
blocks on I/O) is the first real service built this way: its kernel-level
watches live in the *primary*, so a supervised restart needs no
re-registration at all.

Full design: `docs/multi-smalltalk-worker.md` and
`docs/otp_workers_design.md`.

## How MACVM relates to Strongtalk

**Keeps:** the class model (tagged pointers, no object table); adaptive
optimization (ICs, PICs, inlining, customization, deoptimization); the
live-HTML environment idea; the fast-float ambition — now actually finished.

**Drops, or hasn't built yet:** the optional static type system;
mixin-based inheritance (MACVM uses ordinary single inheritance); the
glyph-based flyweight UI (MACVM hosts a native window instead — either a
`WKWebView` or, here, real AppKit). Green/native threads with async C
callouts take a different shape entirely: MACVM keeps each VM strictly
single-threaded and gets concurrency from multiple whole VMs instead (see
"Multi-Smalltalk workers & supervision" above). C callouts are
synchronous by design: a POSIX FFI tier plus the full Cocoa bridge
described above.

**New:** the whole VM in Rust, owing nothing to Strongtalk's source;
Apple-Silicon-first (arm64, `MAP_JIT`/W^X, pointer-authentication aware);
the SQLite image store with byte-identical DB→VM boot; the differential
reliability discipline; and near-total compiled coverage that *stays*
compiled.

The language itself hasn't moved: ordinary Smalltalk-80 syntax and
semantics throughout, binary operators evaluated strictly left to right —
`3 + 4 * 2` is `14`, not `11`.

## Still open

An in-image inspector (class mirrors and browsers already live in the
image — `ClassMirror`, `ClassHierarchyOutliner`, `ClassOutliner` — beside
their Rust-side counterparts); the optional static type system, if it is
ever built. *Not* on that list, by choice: green threads — a fresh VM
thread starts in well under 25 ms, so a second (or tenth) VM is cheap, and
real OS threads already give the concurrency a cooperative scheduler would
only approximate.

## Where to go next

**`README.md`** — the project overview, and the authoritative claims about what's built.

**`docs/SPEC.md`** / **`docs/DESIGN.md`** — the full engineering specification and the architecture decisions of record.

**`docs/cocoa_gui_implementation.md`** — how *this* window works: class lookup, message dispatch, the two memory models, the UI-VM ↔ primary-VM split.

**`docs/PERF.md`** — every optimization arc, measured, with the methodology spelled out.

The **Demos** menu — Breakout and the zooming Mandelbrot in the native Metal game pane, the same dive run in a spawned VM, and parallel workers (every frame computed in bands across 4 worker VMs).
