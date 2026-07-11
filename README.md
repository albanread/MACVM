# MACVM

## Motivation

A new from scratch - Apple Silicon compiler for Smalltalk.

This is the most complex compiler project here in my repos, and like the other 
projects, it may take a while before it is turned into a useful system.

This is not a history lesson, it just my experience.
Strongtalk was released first as interesting documentation, which I really enjoyed reading, 
then as full c++ source code, at the time it was able to execute Smalltalk at high
speed, the repo that was released was fascinating, complex and completely unstable. 

It operated well enough to run for a few minutes before crashing, so it might have worked as a ten minute demo, but was totally unusable. 
I wasted a lot of enjoyable time with a debugger attempting to debug and stabilize it, which I was never able to do.

Decades later software technology and AI has made life far simpler, it is much easier to write compilers now, and I typically find re-implementation is much easier than trying to get any original broken code to work.

So the compiler here is a project based to a large extent on the design and documentation of Strongtalk, I am cheating to the maximum extent possible, the bytecode interpeter and compiler are written in rust, my assembler is reused in the compiler, the gc unfortunately has to be new. 
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
| richards  | ~205 ms | ~6 ms | ~34× |
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
- **Debugger** — crash-dossier (PROBE), breakpoints, mixed-tier backtrace, an
  a64 disassembler, IR dumps, and step-between-calls ([`docs/DEBUGGER.md`](docs/DEBUGGER.md)).
- **Image store** — offline SQLite image editing + a DB→VM boot loader that
  reconstructs the world byte-identically to a `.mst` boot ([`docs/IMAGE.md`](docs/IMAGE.md)).
- **Embedding + GUI** — a `VmHandle` library API and a Cocoa/WKWebView
  Strongtalk-style environment that runs the language on a dedicated thread and
  survives a guest-thread crash ([`docs/vm_handle.md`](docs/vm_handle.md),
  [`gui/PLAN.md`](gui/PLAN.md)).
- **Scripting** — an embedded RUSTTCL console for driving the VM and its
  debugger ([`docs/RUSTTCL.md`](docs/RUSTTCL.md)).

### Fast floating point

Strongtalk's tour promised "fast floats" — eliminating the allocation for
intermediate results within a method. MACVM implements that promise in the
tier-1 JIT as **float regions**: a mono-`Double` send site (the inline cache
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

### Design & planning docs

| Doc | Contents |
|-----|----------|
| [`docs/SPEC.md`](docs/SPEC.md) | The full engineering specification — language, object model, bytecode, interpreter, GC, adaptive compiler, deopt, primitives, bootstrap, testing |
| [`docs/SPRINTS.md`](docs/SPRINTS.md) | The phased implementation plan (S0–S15 core, S16+ stretch) and its status |
| [`docs/DESIGN.md`](docs/DESIGN.md) | High-level architecture + decisions of record (D1–D13) |
| [`docs/PERF.md`](docs/PERF.md) | The performance record: every optimization arc, measured |
| [`docs/float_fastpath_design.md`](docs/float_fastpath_design.md) | Unboxed float regions: the IR review, the reducer, the `d`-register file, `DoubleSlot` deopt |
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

## Lineage & licensing

Self and Strongtalk were released under BSD-style licenses. Code adapted from
them retains its original notices; new MACVM code is under the license in
[`LICENSE`](LICENSE). See `docs/DESIGN.md` for provenance tracking.
