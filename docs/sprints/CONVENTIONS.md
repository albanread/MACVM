# Sprint Documentation & Codebase Conventions

Binding conventions for all `sprint_sNN_detail.md` / `tests_sNN.md` files and
for the implementation itself. The implementing agent reads
[`../SPEC.md`](../SPEC.md) (the contract), the sprint's two files, and this
one. When a sprint doc and SPEC.md disagree, **SPEC.md wins** — flag the
conflict, don't improvise.

## 1. Crate layout (pinned)

```
src/
  main.rs            CLI: `macvm run file.mst`, `macvm repl`, flags (§ env vars)
  lib.rs             module roots + crate-level lints
  oops/              Oop, tags, mark word, typed wrappers, klass access   [unsafe OK]
  memory/            Universe, spaces, allocator, scavenger, full GC,
                     card table, Handle/HandleScope                       [unsafe OK]
  runtime/           VmState, lookup + lookup cache, primitives, errors,
                     process/stack objects
  bytecode/          opcode enum + operand decode, BytecodeBuilder,
                     disassembler
  interpreter/       dispatch loop, frames, send protocol, IC transitions
  frontend/          lexer, parser, AST, capture analysis, codegen
  compiler/          tier-1: IR, feedback, inlining, regalloc, emit,
                     scope descs; assembler.rs (backend trait)
  codecache/         nmethod store, stubs, patching, MacJit wrapper       [unsafe OK]
  vendor/wfasm/      vendored JASM slice (S9; original headers retained)  [unsafe OK]
world/               .mst sources + world.list;  world/tests/ = in-language suite
tests/               Rust integration tests;  tests/golden/ = golden files
docs/sprints/        this directory
```
`#![forbid(unsafe_code)]` everywhere except the four bracketed modules
(enforced via per-module `#![deny]` exceptions — SPRINTS.md standing rule 4).
The existing flat files (`src/oops.rs` etc.) are converted to directories as
each sprint first touches them.

## 2. Names (pinned — use exactly these)

Core types: `Oop`, `SmallInt`, `MemOop`, `KlassOop`, `ArrayOop`,
`ByteArrayOop`, `SymbolOop`, `MethodOop`, `ClosureOop`, `ContextOop`,
`DoubleOop`, `Mark` (mark-word wrapper), `Format` (object-format enum).
Runtime: `VmState` (the one god-struct threaded as `&mut VmState`; **no
statics/globals**), `Universe`, `Handle<T>`, `HandleScope`, `LookupCache`,
`PrimResult` (`Ok(Oop) | Fail`), `PrimFn`.
Interpreter: `Frame`, `ProcessStack`, `InterpreterIc` (the 4-slot IC view),
`activate_method`, `send_generic`.
Tier 1: `Nmethod`, `NmethodId`, `CodeTable`, `Ir`, `VReg`, `OopMap`,
`ScopeDesc`, `PcDesc`, `Assembler` (trait), `JasmAssembler`, `MacJit`.
Constants live in `src/oops/layout.rs` (tag values, header offsets, mark-word
shifts) and are the **single source of truth** — no re-derived magic numbers.

## 3. Environment flags (pinned)

`MACVM_GC_STRESS=1|full[:N]`, `MACVM_JIT=off|threshold=N`,
`MACVM_DEOPT_STRESS=1`, `MACVM_TRACE=<channels>` (comma list; the channel set
is open-ended — `bytecode gc jit ic deopt stats count` so far),
`MACVM_HEAP=<MiB>`, `MACVM_EDEN=<KiB>` (shrink eden for GC stressors),
`MACVM_DBG_OOP=<addr>` (single-oop GC phase tracing). Parsed once into
`VmOptions` inside `VmState`.

## 4. Error & panic policy

Rust `panic!`/`assert!` = VM bug only. All guest-visible failures are
Smalltalk sends (DNU, `mustBeBoolean`, `error:`) or `PrimResult::Fail`.
Debug builds carry heavy `debug_assert!` invariant checks (tag checks on every
typed-wrapper construction, frame-layout checks on push/pop); release builds
lean. Verification helpers live in `memory::verify` (heap walker used by
tests and stress modes).

## 5. Testing layout (pinned)

- Unit tests: `#[cfg(test)]` in-module.
- Integration: `tests/it_<area>.rs`, one file per sprint area.
- Golden: `tests/golden/<name>.mst` + `.expected` (stdout transcript) and
  `<name>.bc.expected` (disassembly). Runner: `tests/it_golden.rs` walks the
  directory; `UPDATE_GOLDEN=1` regenerates.
- In-language: `world/tests/*.mst` using SUnit-lite (`TestCase subclass:`,
  `assert:`, `assert:equals:`, `should:raise:` later); runner invoked from
  `tests/it_world.rs` and by `just test-all`.
- Stress gates run in CI script `just gate-sNN` per sprint (documented in each
  tests file).

## 6. Sprint detail file template (`sprint_sNN_detail.md`)

```
# Sprint SNN — <title>
Objective (2-3 sentences) + SPEC sections implemented.
## Prerequisites            — what exists from prior sprints, exact modules
## Deliverables             — bullet list of files/modules/features
## Design
  ### Data structures       — Rust type signatures, field-by-field, invariants
  ### Algorithms            — step-by-step; edge cases enumerated
  ### Layer boundaries      — what each new module may/may not touch
## Implementation order     — numbered steps, each independently compilable
## Pitfalls                 — concrete gotchas (from reference-VM analysis,
                              MacNCL lessons, Rust/arm64 specifics)
## Interfaces for later sprints — signatures later sprints will call
## Out of scope             — explicitly deferred items (with target sprint)
```

## 7. Tests file template (`tests_sNN.md`)

```
# Sprint SNN — Test Plan
## Acceptance gate          — restated from SPRINTS.md, made checkable
## Unit tests               — table: test name | module | assertion | rationale
## Integration/golden tests — per test: setup, input, expected output
## In-language tests        — new world/tests/ cases (list assertions)
## Stress/negative tests    — failure modes deliberately provoked
## Non-goals                — what this sprint's tests do NOT cover (and where
                              that coverage lands)
```

## 8. Writing style for sprint docs

Written FOR an implementing agent (Sonnet 5) with **no conversation context**:
every fact needed must be in SPEC.md, this file, or the sprint files —
never "as discussed". Cite SPEC sections (`SPEC §5.3`) rather than restating
normative content; DO restate anything ambiguous as concrete Rust signatures.
Prefer tables and numbered steps to prose. 300–600 lines per detail file,
150–350 per tests file. If the author believes SPEC.md is wrong or
underspecified, add a `> **SPEC-QUESTION:**` blockquote rather than silently
diverging.
