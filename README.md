# MACVM inspired by Strongtalk

## Motivation

A from-scratch Apple Silicon compiler for Smalltalk — the most complex
compiler project in my repos, and like the others, it may take a while
before it turns into a useful system.

This isn't a history lesson, just my own experience of one. Strongtalk was
released to the public in 2002 — first as documentation I thoroughly
enjoyed reading, then as full C++ source. At the time it executed Smalltalk
at high speed, and the released repo was fascinating, ambitious, and richly
engineered. I spent many happy hours exploring it and came away impressed
by the design: Strongtalk — and Self before it — pioneered adaptive
optimization (polymorphic inline caches, type feedback, deoptimization),
the ideas that went on to power the Java HotSpot VM, and added on top of
that an optional static type system and a live, hypertext programming
environment. There's a great deal of brilliant engineering there to learn
from and build on.

Decades later, software technology and AI have made life far simpler — it's
much easier to write compilers now, and I find re-implementing a strong,
well-documented design one of the most rewarding ways to work. So MACVM is
built to a large extent on Strongtalk's own design and documentation. I'm
cheating to the maximum extent possible: the bytecode interpreter and
compiler are written in Rust, my own assembler is reused in the compiler,
and only the GC had to be entirely new. It also carries the almost absurd
level of introspection, debugging, and testing a project this complex
needs, in the hope it adds up to reliability.

MACVM is not a port. It's a research virtual machine for macOS on Apple
Silicon (arm64), in the **Self → Strongtalk** lineage: a **class-based
object model** with an **adaptive optimizing compiler** driven by type
feedback. It takes the adaptive-optimization machinery both VMs share
(inline caches, PICs, type feedback, deoptimization) and Strongtalk's
representation (classes + direct pointers, no object table), reimplemented
in Rust for 64-bit Apple Silicon. Both reference VMs are cloned alongside
this repo (`../self-repo`, `../strongtalk-repo`); the source-level analysis
that drove the design is in
[`docs/reference-vm-analysis.md`](docs/reference-vm-analysis.md).

## Status — working, and it compiles

MACVM boots a real Smalltalk object world and runs programs on a **two-tier
engine**: a simple dispatch-based bytecode interpreter plus a **tier-1
optimizing JIT** that
recompiles hot code with type feedback and deoptimizes safely. On the standard
benchmarks the JIT owns essentially all of the runtime:

| benchmark | interpreter | JIT (tier-1) | speedup |
|-----------|-------------|--------------|---------|
| deltablue | 214 ms | **4 ms** | **53×** |
| richards  | ~205 ms | 6–7 ms | ~30× |
| sieve     | 88 ms | 9 ms | ~10× |
| ctxloop (closure/OSR) | 134 ms | 1 ms | 134× |

**Compiler coverage is achieved**: ~98.7% of methods that actually run compile
(the remainder are native primitives, which lose nothing by staying native),
and on real workloads **98.6–99.8% of executed bytecode-work runs as compiled
native code** — including closures, which compile and splice inline rather than
allocating. See [`docs/next_architecture.md`](docs/next_architecture.md) for
the coverage arc and [`docs/PERF.md`](docs/PERF.md) for the benchmark-by-benchmark
measurements.

### Measured against Cog — a yardstick, not a competition

MACVM does not compete with Squeak, Pharo, or Cog in any way. Those are
mature production systems with decades of engineering and real communities
behind them; this is a from-scratch macOS VM exploring the Strongtalk
lineage. But a JIT still needs an honest yardstick, and Cog — the
OpenSmalltalk JIT that powers Squeak and Pharo — is the meaningful one:
same language, same benchmarks, and it sets a high bar.

So the suite runs head-to-head against Pharo 13's Cog on the same machine
([`scripts/cog-bench.sh`](scripts/cog-bench.sh)): a **microsecond clock on
both sides**, **checksum-verified identical workloads** (the Pharo side is
machine-translated from the same `world/` sources), interleaved same-session
rounds, best-of-rounds. Current scoreboard (Apple M-series, warm ms per ×10
reps, 2026-07-22):

| benchmark | MACVM | Cog | |
|-----------|------:|----:|---|
| arith     |  34.0 | 51.1 | **1.50× faster** |
| fib       | 135.4 | 181.0 | **1.34× faster** |
| sieve     |   2.3 |  3.5 | **1.48× faster** |
| dict      |   7.7 | 12.0 | **1.55× faster** |
| alloc     |  12.5 | 14.2 | **1.13× faster** |
| richards  |  18.8 | 21.9 | **1.17× faster** |
| deltablue |   2.7 |  3.5 | **1.27× faster** |

All seven ahead — a bar this VM only cleared after the harness itself was
made honest: earlier comparisons were wrong in *both* directions
(millisecond clocks truncating the sub-5 ms benches, and an unfaithful
Cog-side translation), and the fixes that followed the honest numbers
(special-selector inlining, nursery sizing, frameless leaf methods, the
prologue nil-fill shrink) are what closed the real gaps they exposed. The
full measured record, including those corrections and the commit stamp of
every scoreboard, is [`docs/cog_bench.md`](docs/cog_bench.md).

### What's implemented

- **Object model** — Strongtalk-style classes, direct tagged pointers, **no
  object table**, a 2-word `[mark][klass]` header.
- **Garbage collection** — generational scavenge + a full compacting collector,
  both running **under live, moving compiled frames** via precise oop-maps and a
  mixed-tier frame walker.
- **Interpreter** — a simple dispatch-based bytecode baseline tier (a
  fetch-decode-`match` loop) with inline caches.
- **Tier-1 optimizing JIT** — a vendored pure-Rust AArch64 encoder (JASM) behind
  the `Assembler` trait; PICs and type feedback; method + block inlining;
  per-klass **customization** with self-send and block-arg **devirtualization**;
  **deoptimization**, **on-stack replacement (OSR)**, and recompile-on-trap.
- **Closure compilation** — literal blocks compile and splice inline, including
  multi-basic-block conditional-`^` (non-local-return) blocks, with `Context`
  elision / materialization / adoption across the tier boundary.
- **FFI** — Tier-1 POSIX via `dlsym` + shape-keyed native-call trampolines +
  an `Alien` raw-memory type ([`docs/FFI.md`](docs/FFI.md)).
- **SIMD** — NEON vector support in two layers: `Float64x2` / `Float32x4` /
  `Int32x4` **value classes** whose arithmetic the JIT fuses to single NEON
  instructions, and `FloatArray` **bulk kernels** (`+@`, `sum`, `dot:`,
  `scale:`, `min`/`max`) as explicit hand-written NEON in Rust
  ([`docs/SIMD.md`](docs/SIMD.md)).
- **Debugger** — crash-dossier (PROBE), breakpoints, mixed-tier backtrace, an
  a64 disassembler, IR dumps, and step-between-calls ([`docs/DEBUGGER.md`](docs/DEBUGGER.md)).
- **Optional static types** — a Strongtalk-style optional type checker:
  annotate parameters/returns/instance variables
  (`aNumber <Number> ^ <Boolean>`), get nominal + block + union subtyping, a
  real `Self`, and a **static-DNU** send rule flagging a selector no
  reachable class implements — strictly advisory (a byte-identical
  differential gate proves annotations never reach codegen) and gradual (an
  unannotated program checks clean by construction). **The entire core
  library is annotated** — 739 method signatures, real world, 0 findings —
  staged T0′→T4 against Strongtalk's own signature sources, fixing five
  genuine soundness bugs in the checker itself along the way, each caught
  only by running it against the real ~150-class world
  ([`docs/typechecker_design.md`](docs/typechecker_design.md)).
- **Image store** — offline SQLite image editing + a DB→VM boot loader that
  reconstructs the world byte-identically to a `.mst` boot ([`docs/IMAGE.md`](docs/IMAGE.md)).
- **Embedding + two GUIs** — a `VmHandle` library API embeds the language on
  a dedicated thread that survives a guest-thread crash, behind **two
  independent front-ends** built on the same primitives:
  - **`gui/` (`macvm-gui`)** — a faithful recreation of the 1996
    **Strongtalk hypertext programming environment**, rendered as HTML in a
    `WKWebView` inside a native Cocoa window/menu bar/toolbar. The truer
    read of the original interface, and the one with the built-in help +
    tour.
  - **`cocoa_gui/` (`macvm-cocoa`)** — a lighter, **native AppKit shell
    whose own interface is written in Smalltalk**: real Cocoa views
    (`NSButton`, `NSOutlineView`, `NSTextView`, …), driven by a Smalltalk VM
    pinned to the main thread through the Cocoa bridge — no HTML, no JS, no
    WebKit process. The environment *is* the language, all the way up.

  Both ship the same core toolset — a live **class browser** whose accepts
  compile into the running VM *and* persist to the image, an outliner,
  **find tools** (definitions, implementors, senders — SQLite-indexed), a
  **Workspace** with do-it/print-it, a **Canvas** drawing widget, and a live
  **VM/GC metrics dashboard** — each built the way its own front-end works
  best (`gui/`'s tools are DB-and-JS-driven; `cocoa_gui/`'s browser is
  DB-backed while its outliner reflects the live VM directly)
  ([`docs/vm_handle.md`](docs/vm_handle.md), [`gui/PLAN.md`](gui/PLAN.md),
  [`docs/cocoa_gui_design.md`](docs/cocoa_gui_design.md)).
- **Game engine** — a native Metal game pane driven entirely from Smalltalk: an
  8-bit indexed drawing surface, retained GPU sprites, a 60 fps frame loop with
  keyboard input, and sound effects + ABC-notation music through AVFoundation,
  via the [MacGamePane](https://github.com/albanread/MacGamePane) engine
  ([`docs/gamepane_design.md`](docs/gamepane_design.md)). The GUI's **Demos**
  menu ships four, all written in Smalltalk: `Breakout`
  ([`world/44_breakout.mst`](world/44_breakout.mst)), a small but complete
  paddle-ball-bricks game; `MandelZoom`
  ([`world/45_mandelzoom.mst`](world/45_mandelzoom.mst)), a live zooming
  Mandelbrot (the JIT-compiled escape-time float math); the same dive run in a
  **spawned second VM**; and `ParallelMandel`
  ([`world/48_parallelmandel.mst`](world/48_parallelmandel.mst)) — the dive
  with **every frame computed in parallel bands by 4 worker VMs** (below).
- **Multi-VM workers** — true multicore parallelism, driven entirely from
  Smalltalk: `Worker spawn:` boots **worker VMs** (each its own heap, JIT, and
  GC on its own OS thread) that communicate with the primary by **deep-copy
  message passing** (the MOP pickle) — Erlang-style share-nothing, no shared
  state, no identity across heaps, consistent with the `become:` stance below.
  A primary can hold a pool of **up to 16 concurrent worker VMs**, each
  independently addressable (`send:onReply:` per worker) — a star topology:
  every worker talks only to the primary, and workers don't spawn sub-workers
  (a v1 rule the registry design doesn't preclude lifting later).
  Fully asynchronous: replies run as `send:onReply:` continuations and delivery
  is event-driven — the send itself wakes the sleeping receiver (a coalesced,
  never-lost wake), so **no one ever polls for a message** and a worker with
  nothing to do sleeps at zero CPU. (Honesty note: that claim is about the
  message plane. The Cocoa GUI's supervisor does run a deliberate slow
  heartbeat — ~4 Hz, control-plane housekeeping only: stop flags, toolbar
  metrics, servicing parked requests such as File In — and the shell's
  flag-and-drain pattern sweeps its request flags on each pass, made prompt by
  a run-loop wake. Bounded ticks by design, not message delivery; the headless
  worker system runs with no beat at all.) A crashed worker dies alone and is
  reported as an ordinary `#workerDied` message. `ParallelMandel` measures **~2.65 CPUs of sustained
  utilization with 4 workers** on the live zooming Mandelbrot — visibly faster
  than the single-VM dive ([`docs/multi-smalltalk-worker.md`](docs/multi-smalltalk-worker.md)).
- **OTP-style supervision** — a supervision layer over the worker fleet:
  MACVM has no exception system (`self error:` stops the computation
  outright, and scoped `catch` was deliberately rejected), so a crashed
  worker is answered Erlang/OTP-style instead — reported as the ordinary
  `#workerDied` message above, and a `WorkerSupervisor` restarts it by
  declared policy (`#oneForOne` / `#oneForAll` / `#restForOne`), with
  supervisors nesting into trees and a child that exhausts its own restart
  budget escalating to its parent. `WorkerNames` rebinds a service's name on
  every restart so callers never hold a handle to a corpse; `ServiceWorker`'s
  deadline-bounded `call:timeoutMs:onReply:onError:` funnels every failure
  mode — timeout, death, an RPC error — into one callback, never a block,
  with nothing ever blocking to enforce it. The `IoWorker` (a
  `kqueue`-multiplexing I/O service) is the first real service supervised
  this way: its kernel-level watch registrations live in the *primary* and
  survive a worker's crash untouched, so a supervised restart needs no
  re-registration at all ([`docs/otp_workers_design.md`](docs/otp_workers_design.md)).
- **The object world** — 155 classes / 1,872 methods of hand-written and
  Strongtalk-ported library (`world/*.mst`, `world.list`'s own 74 files;
  counted via `ClassMirror allClasses`, own — not inherited — selectors):
  full collections + streams protocol, Dictionary/Set/OrderedCollection,
  String/Character text utilities, Fraction and LargeInteger arithmetic, an
  in-language test suite, and the Richards / DeltaBlue / Stanford benchmark
  ports in `world/bench/` (counted separately — loaded on demand, not part
  of the boot-time figure above).
- **Scripting** — an embedded RUSTTCL console for driving the VM and its
  debugger ([`docs/RUSTTCL.md`](docs/RUSTTCL.md)).

### Cocoa from Smalltalk

MACVM talks to macOS directly. Foundation and AppKit objects are ordinary
Smalltalk receivers — look a class up once and Objective-C messages are
plain keyword sends, with argument and return types read from the live
runtime's own method signatures:

```smalltalk
s := (Cocoa classNamed: 'NSMutableString') alloc init.
s appendString: 'hello'.
s length.                        "→ 5"

win onMain makeKeyAndOrderFront: nil.          "AppKit runs on the main thread"
act := Cocoa action: [ Transcript showCr: 'clicked!' ].
btn onMain setTarget: act.  btn onMain setAction: 'macvmFire:'.
```

A Cocoa object lives in Smalltalk as an `ObjcRef` holding one retained
reference — the moving GC and Objective-C's reference counting never see
each other's pointers, exceptions are caught at the boundary, and the
bridge always errs toward a leak, never a double-free (`release`,
`poolDo:`, and ARC's naming conventions do the bookkeeping). Button
clicks travel back over the same inbox the worker VMs use and run
between doits on the VM thread. The **Demos → CocoaPad** menu item
builds a native `NSWindow` with a live button entirely from
`world/50_cocoapad.mst`; the design is in
[`docs/cocoa_bridge_design.md`](docs/cocoa_bridge_design.md), the user
guide in the in-app help (Help → MACVM Documentation → Cocoa from
Smalltalk).

### Fast floating point

Strongtalk's tour introduced the idea of "fast floats" — eliminating the
allocation for intermediate results within a method — and sketched an
experimental scheme for it. MACVM builds that idea out fully in the tier-1
JIT as **float regions**: a mono-`Double` send site (the inline cache
is the type oracle) compiles to a guarded unbox, native `fmul`/`fadd`/`fcmp`,
and a box only where a boxed value is actually observed. Inside a region there
is **no allocation, no GC interaction, and no message send — just assembler
maths and libm calls**:

- **A second register file.** Unboxed floats live in `d0`–`d7` scratch plus a
  `d8`–`d15` write-through residency tier, fully independent of the GPR
  allocator. A raw `f64` is invisible to the moving GC (never in an oop map,
  never scanned), which is what makes registers-across-safepoints cheap here.
- **A box/unbox reducer.** `FUnbox(FBox x) → x` cancellation, dead-box
  elimination, deopt-sunk boxing (an intermediate needed only by deopt
  metadata is boxed *in the trap's own cold block*), literal folding, and
  **float-temp promotion** — a temp that provably always holds a `Double`
  lives as a raw `f64` across the whole loop, safepoints included.
- **Honest deoptimization.** One new deopt-map kind (`DoubleSlot`) tells the
  materializer "this frame slot is raw float bits — box it back"; everything
  else reuses the existing trap/reexecute machinery, verified by pinned
  forced-deopt-mid-loop regressions.
- **libm transcendentals** — `sin cos tan exp ln atan sqrt` as primitives;
  libm preserves the callee-saved `d`-registers, so a plotted curve costs one
  library call per point plus register arithmetic.

Measured on the WKWebView GUI's Mandelbrot demo (420×220, release, Apple
Silicon), each layer removing a *category* of cost:

| stage | time | allocation per render |
|-------|------|-----------------------|
| boxed sends (before) | 746 ms | 708 MB |
| pixel-buffer output | 458 ms | 595 MB |
| float-region fuse | 180 ms | 595 MB |
| sunk boxing + temp promotion | 166 ms | 4 MB |
| strength-reduced coordinates | 38 ms | 0 |
| **d-register residency** | **25 ms** | **0** |

**~30× end to end, with zero allocation, zero deopts, and one scavenge-free
heap per render.** Full design, the measured-and-rejected variants included,
in [`docs/float_fastpath_design.md`](docs/float_fastpath_design.md).

**How close is that to C?** The honest external yardstick — the *identical*
Mandelbrot kernel hand-ported to C (same 420×220, same escape loop and
coordinate accumulation, checksum-verified equal work), compiled with the same
Apple `clang`, warmed, best-of-30 on the same machine:

| engine | time | vs C ‑O2 |
|--------|------|----------|
| C, ‑O2 (== ‑O3 ‑march=native) | 4.6 ms | 1.0× |
| C, ‑O0 | 13.1 ms | 2.9× |
| **MACVM tier‑1 JIT** | **25.2 ms**¹ | **5.5×** |
| MACVM interpreter | 3406 ms¹ | 745× |

¹ Independently re-timed 2026-07-19 (23 ms / 3337 ms — within noise; see
[`docs/float_fastpath_design.md`](docs/float_fastpath_design.md)'s own
verification note). This repo has no committed C source or build script for
the two C rows, so only MACVM's own two rows are source-verifiable here.

So the tier‑1 JIT lands **~1.9× off *unoptimized* C and ~5.5× off optimized
C** — solid-baseline-JIT territory for a dynamic language (the "~30×" above is
against our own interpreter floor, not against C; absolute times are the fair
measure). The remaining gap to ‑O2 is specific and known: no FMA fusion
(`fmul; fadd` vs a single `fmadd`), and `escapeAtRe:im:` is still a per‑pixel
compiled *send* rather than inlined into the pixel loop the way C inlines
`escape()`.

### Optional static types

Strongtalk's tour named two headline ideas beyond adaptive optimization: a
live hypertext environment (both GUIs, above) and an *optional* static type
system layered over ordinary dynamic Smalltalk — types where you want them,
nothing else changes where you don't. MACVM ports that idea as a genuine
Rust reimplementation (not a Smalltalk port), staged T0′→T4 against
Strongtalk's own `.dlt` sources as the executable spec:

```smalltalk
Magnitude subclass: Number [
    max: aMagnitude <Magnitude> ^ <Magnitude> [
        ^self < aMagnitude ifTrue: [ aMagnitude ] ifFalse: [ self ]
    ]
]
```

- **T0′** — the parser captures annotations instead of discarding them.
- **T1** — a `TypeExpr` grammar (named / generic / block / union types) and
  a VM-free `WorldModel`, built by re-parsing the world with no `VmState`
  involved at all.
- **T2** — real subtyping: nominal (superclass chain), `Self` resolved
  against the enclosing class, blocks checked contravariantly on arguments
  and covariantly on return, unions distributing.
- **T3** — the send rule: **static-DNU** (a selector no reachable class
  implements, on a receiver whose type is known) plus per-argument subtype
  checks.
- **T4** — the entire core library annotated, signatures ported from
  Strongtalk's own where they exist and inferred mechanically from our own
  code where they don't: **739 method signatures, real world, 0 findings.**

Two things make it safe to ship as advisory-only:

- **Isolation.** `src/types/` is reachable from exactly one place — the
  `macvm typecheck` subcommand — verified by grep after every stage.
  Nothing it does can ever reach codegen.
- **A byte-identical differential gate.** The world compiles to the *exact
  same bytecode* with or without every annotation ever written — proven on
  every commit, the same discipline this project applies to every JIT
  change.

That safety net is what makes the interesting part possible: **finding
real soundness bugs in the checker itself by actually running it against a
substantial, real library**, rather than by inspection — five of them,
across five stages. The first: a naive `Self`-typed dispatch check flagged
97 false positives the moment it met real code — `Number>>log` sends `self
asFloat`, but `Number` itself never implements `asFloat`, only its concrete
subclasses do (the ordinary Template Method pattern). The fix — a
`Self`-typed receiver *declines* checking rather than guesses — is the same
stance this whole project takes toward any claim it can't verify. Full
design and every stage's gate:
[`docs/typechecker_design.md`](docs/typechecker_design.md).

### Replace, don't mutate — there is no persistent image

MACVM never mutates a persistent image. Where classic Smalltalk carries one
long-lived heap snapshot forward across years of in-place modification, MACVM
keeps its truth in a **source-code database** — the `.mst` world files and the
SQLite image they seed — and spins up VMs from it in well under a second. VMs
are plural and disposable: the system would always rather **throw a VM away and
rebuild it from source than mutate one in place**.

You can feel the difference in the tools. When you file development code into
the GUI, it is not patched into the long-running VM: a **fresh VM is recreated
from the world and your file loads on top of it**. Filing in the same file
twenty times just works — there is no accumulated state to collide with,
because there is no accumulated state at all. To a Smalltalker raised on the
image this reads as less dynamic, almost static. In operation, though, MACVM is
a true Smalltalk system — live objects, live compilation, everything inspectable
while it runs. The difference is only in how change lands: **replacement instead
of mutation**, with every piece of state visible in source you can read, diff,
and version — never implicit in a heap that remembers things no one can point
to.

### Why there's no `become:`

MACVM has no `become:` — the Smalltalk primitive that swaps one object's
identity for another's, redirecting every reference in the system at once. This
is a deliberate omission, and it's worth being honest about the cost before the
justification.

**What we lose.** `become:` is the classic tool for three things, and we give
all three up:

- **Live schema migration.** Add an instance variable to a class and, with
  `become:`, you can reshape every *existing* instance in place while preserving
  its identity and every reference to it. Without it, instances keep their old
  shape until they are recreated — so objects built up in memory during a
  session can't be upgraded live.
- **Transparent replacement.** Proxies, lazy-loading stubs that resolve into the
  real object, futures, copy-on-write — anything that substitutes one object for
  another while everyone holding a reference keeps working. `become:` does this
  atomically; we must use explicit indirection (a handle, or `doesNotUnderstand:`
  forwarding), which leaks into the API and means identity (`==`) is the
  wrapper's, not the target's.
- **`becomeForward:` bulk redirection**, used by image loaders and some
  compaction tricks.

These are real capabilities, not corner cases — a Smalltalker who reaches for
`become:` will find it missing.

**Why it's gone.** MACVM — like Strongtalk and Self before it — represents an
object reference as the **raw machine address of the object body**, not as an
index into an object table. That is the choice that makes a field access a
single load and lets the JIT cache classes at send sites, build PICs, and
inline — the whole basis of the adaptive optimizer. But it also means
"redirect every reference to A so it points to B" has no cheap implementation:
there is no table slot to swap, only every pointer in both heap generations,
every root, every live stack frame, and every machine register to find and
rewrite. Strongtalk *does* keep a `become:` primitive and implements it exactly
that way — `deoptimize_all()` followed by a full-heap scan — and its own tour
calls it "prohibitively slow" and "not supported." For an optimizing VM the scan
is only half the cost: a `become:` that changes an object's class invalidates
every cached class in the code cache, and one that reshapes an object
invalidates the fixed field offsets baked into compiled code, forcing a global
deoptimization. `become:` fights everything the compiler is built to do.

**Why we can afford to skip it — MACVM is not image-based.** There is no
persistent snapshot of the live object heap (the `image.sqlite3` MACVM can boot
from is a database of class/method *source*, not a heap dump). A snapshot is
what historically *made* `become:` load-bearing: a decades-old living image can
never be restarted, so its objects must be migrated in place. MACVM instead
rebuilds its entire world from source — `.mst` files, or the SQLite source
database — on every boot, and that boot takes **well under a second** for the
whole standard world. So the dominant use of `become:` — evolving a class whose
instances you can't afford to lose — is answered by editing the source and
restarting, not by mutating a live heap. Class redefinition itself already goes
through the deoptimize-and-recompile path the VM has for exactly this. The
residual, real loss is schema-migrating or transparently proxying objects
*within a single running session* — and that is the price we pay, knowingly,
for direct pointers and a fast JIT.

### Design & planning docs

| Doc | Contents |
|-----|----------|
| [`docs/SPEC.md`](docs/SPEC.md) | The full engineering specification — language, object model, bytecode, interpreter, GC, adaptive compiler, deopt, primitives, bootstrap, testing |
| [`docs/SPRINTS.md`](docs/SPRINTS.md) | The phased implementation plan (S0–S15 core, S16+ stretch) and its status |
| [`docs/DESIGN.md`](docs/DESIGN.md) | High-level architecture + decisions of record (D1–D13) |
| [`docs/PERF.md`](docs/PERF.md) | The performance record: every optimization arc, measured |
| [`docs/float_fastpath_design.md`](docs/float_fastpath_design.md) | Unboxed float regions: the IR review, the reducer, the `d`-register file, `DoubleSlot` deopt |
| [`docs/mandelbrot_walkthrough.md`](docs/mandelbrot_walkthrough.md) | The Mandelbrot flagship as a teaching example: 746 ms → 166 ms, 708 MB → 4 MB allocation, walked through arithmetic-vs-representation cost |
| [`docs/next_architecture.md`](docs/next_architecture.md) | The compiler-coverage arc (now met — ~98.7% of run methods compile): why MACVM's interpreter/JIT boundary exists and what closing it further would take |
| [`docs/SIMD.md`](docs/SIMD.md) | SIMD vector support (built): `Float64x2`/`Float32x4`/`Int32x4` value classes fused to NEON by the JIT, plus `FloatArray` bulk kernels + reductions |
| [`docs/FFI.md`](docs/FFI.md) | The foreign-function interface: `dlsym` resolution, shape-keyed trampolines, the `<primitive: FFI …>` pragma, and the `Alien` raw-memory type |
| [`docs/cocoa_bridge_design.md`](docs/cocoa_bridge_design.md) | The Cocoa bridge (designed): how the moving GC and Cocoa's reference counting coexist — retain-on-wrap `ObjcRef` tickets, zero GC changes, main-thread hops, callback tickets, the C0–C5 ladder |
| [`docs/cocoa_gui_design.md`](docs/cocoa_gui_design.md) | The native Cocoa GUI (built, `cocoa_gui/`): the environment written in itself — a Smalltalk VM pinned to the main thread *is* the interface, a second VM behind it is the persistent environment; C6 reverse-dispatch delegates, the restart-in-place supervisor |
| [`docs/cocoa_gui_flag_and_drain.md`](docs/cocoa_gui_flag_and_drain.md) | Why a C6 callback may never touch VM-level state directly (two real failure modes: fails closed silently, or crashes the process) and the flag/wake/drain mechanism every UI rebuild, primary restart, and data-backed view refresh uses instead — plus a checklist for adding a new one |
| [`docs/cocoa_gui_implementation.md`](docs/cocoa_gui_implementation.md) | Implementation walkthrough, real source cited: how a Cocoa class is found at runtime (both directions), how a Smalltalk send becomes `objc_msgSend` and back, how the moving GC and manual refcounting coexist with zero GC changes, how the UI VM and the persistent VM actually talk |
| [`docs/DEBUGGER.md`](docs/DEBUGGER.md) | The debugging ladder: PROBE crash dossiers, breakpoints, mixed-tier backtraces, the a64 disassembler, IR dumps |
| [`docs/typechecker_design.md`](docs/typechecker_design.md) | The optional Strongtalk-style type checker (built, T0′–T4): capture → parse+model → subtype+local rules → send rule → the entire core library annotated; the isolation gate and the byte-identical differential gate that make it safe to ship advisory-only |
| [`docs/ASM.md`](docs/ASM.md) / [`docs/CANVAS.md`](docs/CANVAS.md) | Side-track designs with working preview tools: hand-written native-AArch64 methods (`<asm:>`), and the GUI Canvas widget |
| [`docs/gamepane_design.md`](docs/gamepane_design.md) | The native Metal game engine driven from Smalltalk (MacGamePane): the frame/threading architecture, drawing/sprite/audio command channel, and the milestone ladder |
| [`docs/multi-smalltalk-worker.md`](docs/multi-smalltalk-worker.md) | Primary/worker VM parallelism (built, M0–M4): spawn worker VMs from Smalltalk, communicate by deep-copy message passing (the MOP pickle), no shared state — Erlang-style share-nothing across heaps; capstone = the 4-worker parallel Mandelbrot |
| [`docs/otp_workers_design.md`](docs/otp_workers_design.md) | OTP-style supervision over the worker fleet (built, O0–O3): restart policies (`#oneForOne`/`#oneForAll`/`#restForOne`), supervisor trees + escalation, `ServiceWorker`'s deadline-bounded request/reply, `IoWorker` adopted as the first supervised service |
| [`docs/IMAGE.md`](docs/IMAGE.md) / [`docs/managingtheworld.md`](docs/managingtheworld.md) | The versioned SQLite world image, and the practical world/image reseed workflow (`./reseed-world.sh`) |
| [`docs/arm64.md`](docs/arm64.md) | Machine-level design: MAP_JIT/W^X, AAPCS64, PAC, relocs, oop maps, deopt glue |
| [`docs/reference-vm-analysis.md`](docs/reference-vm-analysis.md) | Source-anchored analysis of Self, Strongtalk, JASM, and the MacNCL GC |
| [`docs/sprints/`](docs/sprints/README.md) | Per-sprint implementation guidance + test plans (the sprint logs) |

## Layout

| Path | Contents |
|------|----------|
| `src/oops/` | Object model — tagged pointers, 2-word headers, classes |
| `src/memory/` | Object memory, allocation, generational + full GC |
| `src/interpreter/` | Threaded-code interpreter (the baseline tier) |
| `src/bytecode/` | Bytecode format, decoder, CFG |
| `src/compiler/` | Tier-1 optimizing compiler + JASM AArch64 backend |
| `src/codecache/` | Native code cache, stubs, deopt trap machinery |
| `src/runtime/` | Dispatch, frames, deopt materializer, OSR, recompile, debugger |
| `src/frontend/` | `.mst` parser + class-definition loader |
| `src/types/` | The optional static type checker — isolated, reachable only from `macvm typecheck` |
| `src/embed.rs` | `VmHandle` embedding API |
| `src/rusttcl/` | Embedded RUSTTCL console |
| `world/` | The object world / image sources, tests, benchmarks |
| `gui/` | The Strongtalk-style HTML GUI (`macvm-gui`) — rendered in a `WKWebView` |
| `cocoa_gui/` | The native Cocoa GUI (`macvm-cocoa`) — its own interface written in Smalltalk, driving AppKit directly |
| `image_store/` | The versioned SQLite class/method source database (importer, exporter, send-index) |
| `examples/` | Embedding examples (`mandel_demo`: boot a fresh VM, run a demo headless, exit) |
| `docs/` | Design notes, specs, per-sprint guidance |

## Building & running

```sh
cargo build --release
target/release/macvm run world/bench/deltablue.mst --world world   # runs it
MACVM_JIT=off   target/release/macvm run <prog>.mst --world world   # interpreter only
MACVM_JIT=threshold=200 …                                            # JIT (default gate)
MACVM_TRACE=stats|jit|deopt|count …                                  # instrumentation
```

The JIT is on by default. `MACVM_JIT=off` selects the interpreter, which is the
differential oracle every JIT change is gated against (compiled output must be
byte-identical to interpreted output). Tests: `cargo test`; the stress matrix
(GC / deopt) and world differentials are in `tests/` and the `justfile` gates.

Either GUI launches with a release build by default (both default to `dev`
under a bare `cargo run`, which measures tens of times slower on
compute-heavy demos like the Mandelbrot dives — these scripts exist
specifically to avoid that trap):

```sh
./run-gui.sh      # the WKWebView Strongtalk-style environment (macvm-gui)
./run-cocoa.sh    # the native AppKit environment, written in Smalltalk (macvm-cocoa)
```

Both share the **Demos** menu (Breakout and the three Mandelbrots, including
the 4-worker parallel dive); `./run-mandelvm.sh` runs the standalone
one-dive demo window that exits itself. The WKWebView GUI boots its whole
interface from a SQLite **image** (`world/image.sqlite3`) rebuilt from the
`world/*.mst` source; the Cocoa GUI boots classes straight from `.mst`
source every launch, but its DB-backed browser/find tools read the same
image. After changing a world class, rebuild the image with
`./reseed-world.sh` (build + fresh reseed + boot-check) — see
[`docs/managingtheworld.md`](docs/managingtheworld.md) for the full workflow and
gotchas.

## Lineage & licensing

Self and Strongtalk were released under BSD-style licenses. Code adapted from
them retains its original notices; new MACVM code is under the license in
[`LICENSE`](LICENSE). See `docs/DESIGN.md` for provenance tracking.

## Further reading

MACVM's technical origin is **Strongtalk** — the Motivation section above is
about exactly that, and the original system lives on at
[strongtalk.org](https://strongtalk.org/) and
[talksmall/Strongtalk](https://github.com/talksmall/Strongtalk).

**Cog** is the other great branch of the Self family tree — the production
JIT that, from a deliberately simpler baseline design, still keeps pace with
Strongtalk's — and it is by far the best-documented. Eliot Miranda's
[Cog Blog](http://www.mirandabanda.org/cogblog/) is the clearest published
explanation anywhere of the machinery a Smalltalk VM actually needs
(remarkably, Cog is itself written in Smalltalk, translated to C for the
build). If the internals here interest you, read him:

- [About Cog](http://www.mirandabanda.org/cogblog/about-cog/) — what Cog is
  and how its pieces fit
- [Closures Part I](http://www.mirandabanda.org/cogblog/2008/06/07/closures-part-i/),
  [Part II — the Bytecodes](http://www.mirandabanda.org/cogblog/2008/07/22/closures-part-ii-the-bytecodes/),
  [Part III — the Compiler](http://www.mirandabanda.org/cogblog/2008/07/24/closures-part-iii-the-compiler/)
- [Under Cover Contexts and the Big Frame-Up](http://www.mirandabanda.org/cogblog/2009/01/14/under-cover-contexts-and-the-big-frame-up/)
  — mapping contexts to stack frames, the heart of making Smalltalk fast
- [Build me a JIT as fast as you can](http://www.mirandabanda.org/cogblog/2011/03/01/build-me-a-jit-as-fast-as-you-can/)
- [A Spur gear for Cog](http://www.mirandabanda.org/cogblog/2013/09/05/a-spur-gear-for-cog/)
  — the Spur object representation

Cog itself lives at
[OpenSmalltalk/opensmalltalk-vm](https://github.com/OpenSmalltalk/opensmalltalk-vm).
