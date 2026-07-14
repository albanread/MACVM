# MACVM inspired by Strongtalk

## Motivation

A new from scratch - Apple Silicon compiler for Smalltalk.

This is the most complex compiler project here in my repos, and like the other 
projects, it may take a while before it is turned into a useful system.

This is not a history lesson, it just my experience.
Strongtalk was released to the public first (2002) as interesting documentation, which I really enjoyed reading, 
then as full c++ source code; at the time it was able to execute Smalltalk at high
speed, and the repo that was released was fascinating, ambitious, and richly engineered.

I spent many enjoyable hours exploring it, and came away impressed by the design. Strongtalk — and Self before it — pioneered adaptive optimization: polymorphic inline caches, type feedback, and deoptimization, the ideas that went on to power the Java HotSpot VM. On top of that, Strongtalk added an optional static type system and a live, hypertext programming environment. There is a great deal of brilliant engineering there to learn from and build on.

Decades later software technology and AI have made life far simpler, it is much easier to write compilers now, and I find re-implementing a strong, well-documented design one of the most rewarding ways to work.

So the compiler here is a project based to a large extent on the design and documentation of Strongtalk, I am cheating to the maximum extent possible, the bytecode interpreter and compiler are written in rust, my assembler is reused in the compiler, the gc unfortunately has to be new. 
This compiler also has the almost absurd level of introspection and debugging needed to create something this complex, and extensive tests, which I hope will lead to reliability.

--

A research virtual machine for macOS on Apple Silicon (arm64), in the
**Self → Strongtalk** lineage: a **class-based object model** with an
**adaptive optimizing compiler** driven by type feedback.

MACVM is not a port. It takes the adaptive-optimization machinery both VMs share
(inline caches, PICs, type feedback, deoptimization) and Strongtalk's
representation (classes + direct pointers, no object table), reimplemented in
Rust for 64-bit Apple Silicon. Both reference VMs are cloned alongside this repo
(`../self-repo`, `../strongtalk-repo`); the source-level analysis that drove the
design is in [`docs/reference-vm-analysis.md`](docs/reference-vm-analysis.md).

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
allocating. See [`docs/PERF.md`](docs/PERF.md) for the arc and methodology.

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
- **Image store** — offline SQLite image editing + a DB→VM boot loader that
  reconstructs the world byte-identically to a `.mst` boot ([`docs/IMAGE.md`](docs/IMAGE.md)).
- **Embedding + GUI** — a `VmHandle` library API and a Cocoa/WKWebView
  Strongtalk-style **programming environment** that runs the language on a
  dedicated thread and survives a guest-thread crash: a live **class browser**
  whose accepts compile into the running VM *and* persist to the image, an
  outliner (new class / new method / add variable), **find tools**
  (definitions, implementors, senders — SQLite-indexed), a **Workspace** with
  do-it/print-it, a **Canvas** drawing widget, a live **VM/GC metrics
  dashboard** in the toolbar, and a built-in help + tour
  ([`docs/vm_handle.md`](docs/vm_handle.md), [`gui/PLAN.md`](gui/PLAN.md)).
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
  Fully asynchronous: replies run as `send:onReply:` continuations, delivery is
  event-driven (the send itself wakes the sleeping receiver), and there is no
  polling anywhere. A crashed worker dies alone and is reported as an ordinary
  `#workerDied` message. `ParallelMandel` measures **~2.65 CPUs of sustained
  utilization with 4 workers** on the live zooming Mandelbrot — visibly faster
  than the single-VM dive ([`docs/multi-smalltalk-worker.md`](docs/multi-smalltalk-worker.md)).
- **The object world** — ~82 classes / 840+ methods of hand-written and
  Strongtalk-ported library (`world/*.mst`): full collections + streams
  protocol, Dictionary/Set/OrderedCollection, String/Character text utilities,
  Fraction and LargeInteger arithmetic, an in-language test suite, and the
  Richards / DeltaBlue / Stanford benchmark ports in `world/bench/`.
- **Scripting** — an embedded RUSTTCL console for driving the VM and its
  debugger ([`docs/RUSTTCL.md`](docs/RUSTTCL.md)).

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

Measured on the GUI's Mandelbrot demo (420×220, release, Apple Silicon), each
layer removing a *category* of cost:

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
| **MACVM tier‑1 JIT** | **25.2 ms** | **5.5×** |
| MACVM interpreter | 3406 ms | 745× |

So the tier‑1 JIT lands **~1.9× off *unoptimized* C and ~5.5× off optimized
C** — solid-baseline-JIT territory for a dynamic language (the "~30×" above is
against our own interpreter floor, not against C; absolute times are the fair
measure). The remaining gap to ‑O2 is specific and known: no FMA fusion
(`fmul; fadd` vs a single `fmadd`), and `escapeAtRe:im:` is still a per‑pixel
compiled *send* rather than inlined into the pixel loop the way C inlines
`escape()`.

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
| [`docs/SIMD.md`](docs/SIMD.md) | SIMD vector support (built): `Float64x2`/`Float32x4`/`Int32x4` value classes fused to NEON by the JIT, plus `FloatArray` bulk kernels + reductions |
| [`docs/FFI.md`](docs/FFI.md) | The foreign-function interface: `dlsym` resolution, shape-keyed trampolines, the `<primitive: FFI …>` pragma, and the `Alien` raw-memory type |
| [`docs/DEBUGGER.md`](docs/DEBUGGER.md) | The debugging ladder: PROBE crash dossiers, breakpoints, mixed-tier backtraces, the a64 disassembler, IR dumps |
| [`docs/ASM.md`](docs/ASM.md) / [`docs/CANVAS.md`](docs/CANVAS.md) | Side-track designs with working preview tools: hand-written native-AArch64 methods (`<asm:>`), and the GUI Canvas widget |
| [`docs/gamepane_design.md`](docs/gamepane_design.md) | The native Metal game engine driven from Smalltalk (MacGamePane): the frame/threading architecture, drawing/sprite/audio command channel, and the milestone ladder |
| [`docs/multi-smalltalk-worker.md`](docs/multi-smalltalk-worker.md) | Primary/worker VM parallelism (built, M0–M4): spawn worker VMs from Smalltalk, communicate by deep-copy message passing (the MOP pickle), no shared state — Erlang-style share-nothing across heaps; capstone = the 4-worker parallel Mandelbrot |
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
| `src/embed.rs` | `VmHandle` embedding API |
| `src/rusttcl/` | Embedded RUSTTCL console |
| `world/` | The object world / image sources, tests, benchmarks |
| `gui/` | Strongtalk-style HTML GUI (Cocoa + WKWebView) |
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

The GUI launches with `./run-gui.sh` (release build; the **Demos** menu holds
Breakout and the three Mandelbrots, including the 4-worker parallel dive).
`./run-mandelvm.sh` runs the standalone one-dive demo window that exits itself.
The GUI boots from a SQLite **image** (`world/image.sqlite3`) rebuilt from the
`world/*.mst` source. After changing a world class, rebuild it with
`./reseed-world.sh` (build + fresh reseed + boot-check) — see
[`docs/managingtheworld.md`](docs/managingtheworld.md) for the full workflow and
gotchas.

## Lineage & licensing

Self and Strongtalk were released under BSD-style licenses. Code adapted from
them retains its original notices; new MACVM code is under the license in
[`LICENSE`](LICENSE). See `docs/DESIGN.md` for provenance tracking.
