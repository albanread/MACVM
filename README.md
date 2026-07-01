# MACVM

A research virtual machine for macOS on Apple Silicon (arm64), in the
**Self → Strongtalk** lineage: a **class-based object model** with an
**adaptive optimizing compiler** driven by type feedback.

MACVM is not a port. It takes the adaptive-optimization machinery both VMs share
(inline caches, PICs, type feedback, deoptimization) and Strongtalk's
representation (classes + direct pointers, no object table), reimplemented in
Rust for 64-bit Apple Silicon. Both reference VMs are cloned alongside this repo
(`../self-repo`, `../strongtalk-repo`); the source-level analysis that drove the
design is in [`docs/reference-vm-analysis.md`](docs/reference-vm-analysis.md).

## Goals

- **Class-based object model** — Strongtalk-style classes with direct tagged
  pointers and **no object table**; a 2-word `[mark][klass]` header. (Self's
  prototype/map model was evaluated and set aside — see `docs/DESIGN.md` D2.)
- **Adaptive optimization** — start executing quickly, then recompile hot code
  with type feedback gathered from polymorphic inline caches (PICs), the
  technique Self pioneered and Strongtalk and HotSpot inherited.
- **Apple Silicon first** — arm64 calling convention, W^X / JIT hardening
  (`MAP_JIT`, `pthread_jit_write_protect_np`), pointer-authentication awareness.
  Machine-level design in [`docs/arm64.md`](docs/arm64.md).
- **macOS native integration** — designed to interoperate with the Cocoa /
  `objc_msgSend` bridge story already proven in the sibling projects.

## Status

Design-complete, pre-implementation. The full engineering specification and the
sprint plan are written ahead of the code:

| Doc | Contents |
|-----|----------|
| [`docs/SPEC.md`](docs/SPEC.md) | **The full design** — language, object-model bits, bytecode set, interpreter, GC, adaptive compiler, deopt, primitives, bootstrap, testing |
| [`docs/SPRINTS.md`](docs/SPRINTS.md) | Phased implementation plan of individually-testable sprints |
| [`docs/DESIGN.md`](docs/DESIGN.md) | High-level architecture + decisions of record (D1–D13) |
| [`docs/arm64.md`](docs/arm64.md) | Machine-level design: MAP_JIT/W^X, AAPCS64, PAC, relocs, oop maps, deopt glue |
| [`docs/reference-vm-analysis.md`](docs/reference-vm-analysis.md) | Source-anchored analysis of Self, Strongtalk, JASM, and the MacNCL GC |
| [`docs/sprints/`](docs/sprints/README.md) | Per-sprint implementation guidance + test plans (S0–S15) for the implementing agent |
| [`gui/PLAN.md`](gui/PLAN.md) | The GUI track (Phase G): Strongtalk live-HTML environment in Cocoa+WKWebView; core seam in SPEC §16 |
| [`docs/WORLD.md`](docs/WORLD.md) | The library design (Phase W): what to adopt/reimplement/skip from StrongtalkSource, the `.dlt→.mst` converter, exceptions, SUnit, benchmarks |
| [`docs/APPS.md`](docs/APPS.md) | The apps design (Phase W): Smalltalk mirrors over VM primitives R1–R5, ToolNode outliners, Inspector/Workspace/find tools, HtmlWriter |

The codegen backend is a vendored copy of JASM's pure-Rust, LLVM-MC-verified
AArch64 encoder behind the `Assembler` trait
([`src/compiler/assembler.rs`](src/compiler/assembler.rs)); LLVM or
interpreter-only remain possible behind the same seam.

## Layout

| Path                       | Contents                                                   |
|----------------------------|------------------------------------------------------------|
| `src/oops.rs`              | Object model — tagged pointers, 2-word headers, classes    |
| `src/memory.rs`            | Object memory, allocation, garbage collector               |
| `src/lookup.rs`            | Method lookup, inline caches, PICs, type feedback          |
| `src/interpreter.rs`       | Threaded-code interpreter (the baseline tier)              |
| `src/compiler/`            | Adaptive optimizing compiler + abstract codegen backend    |
| `src/runtime.rs`           | Runtime support, stacks, activation frames, deoptimization |
| `src/utils.rs`             | Shared utilities                                           |
| `world/`                   | The object world / image sources                           |
| `gui/`                     | Strongtalk-style HTML GUI: plan, reference artifacts, CSS  |
| `docs/`                    | Design notes                                               |
| `tools/`                   | Build & development tooling                                |
| `test/`                    | Tests                                                      |

## Building

```sh
cargo build          # builds the (currently scaffold-only) VM
cargo run            # prints a banner; no execution yet
```

The codegen backend is selected by Cargo feature (none enabled yet); see
`docs/DESIGN.md` for the open build/codegen decisions.

## Lineage & licensing

Self and Strongtalk were released under BSD-style licenses. Any code adapted
from them will retain its original notices; new MACVM code is under the license
in [`LICENSE`](LICENSE). See `docs/DESIGN.md` for provenance tracking.
