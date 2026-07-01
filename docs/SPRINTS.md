# MACVM Sprint Plan

A series of achievable, individually-testable sprints implementing
[`SPEC.md`](SPEC.md). Every sprint ends **green**: all prior tests still pass,
plus the sprint's own acceptance gate. Section references (§) are into SPEC.md.

**Per-sprint implementation guidance lives in [`sprints/`](sprints/)**: each
sprint has a `sprint_sNN_detail.md` (design advice for the implementing agent)
and a `tests_sNN.md` (test plan); [`sprints/CONVENTIONS.md`](sprints/CONVENTIONS.md)
binds naming, layout, and templates. SPEC.md §15 logs the amendments that came
out of writing them.

Phases: **A** object world & interpreter (S0–S6) → **B** garbage collection
(S7–S8) → **C** native code substrate (S9–S11) → **D** adaptive optimization
(S12–S15) → **E** stretch (S16+).

Sizing: S = a focused day or two, M = up to a week, L = 1–2 weeks of part-time
research pace. Order is dependency-driven; A and the JASM spike (S9) can
overlap if desired.

---

## Phase A — object world & interpreter

### S0 — Skeleton, tags, mark word `S`
Goal: the value representation, pinned and tested, plus CI habits.
- `Oop` + typed wrappers (§2.1, §2.5); mark-word pack/unpack (§2.2); smi
  arithmetic helpers with overflow detection.
- `cargo test` + `cargo clippy` clean; a `justfile`/script for test+stress runs.
- **Gate:** unit tests — tag round-trips, smi min/max/overflow edges, mark-word
  field isolation (set each field, assert others unchanged), forwarding-bit
  discrimination.

### S1 — Heap arena, allocation, genesis `M`
Goal: allocate real objects; the metaobject knot exists.
- Address-space reservation + eden bump allocator (no GC — abort when full)
  (§7.1–7.2); object formats & instance creation (§2.3); klass objects (§2.4);
  `Universe::genesis()` (§3.2 step 1); symbol table with interning; identity
  hash assignment; a debug object printer (`print_oop`).
- **Gate:** unit tests — genesis invariants (`nil.klass.name == #UndefinedObject`,
  metaclass chain closes: `Object class class == Metaclass`), symbol interning
  identity, indexable alloc + at/put via Rust API, alignment & size math.

### S2 — Bytecode + interpreter core (no sends) `M`
Goal: execute hand-assembled straight-line bytecode.
- Bytecode definition + a Rust `BytecodeBuilder` (test-only assembler);
  CompiledMethod objects (§4.4); disassembler; interpreter loop + frame layout
  (§5.1–5.2) for opcodes 00–12, 30–33, 40–41 (no sends, no closures).
- **Gate:** golden tests — disassembler output for builder-built methods;
  execute arithmetic/temp/jump kernels, assert result oop + final stack shape;
  stack-discipline asserts (frame teardown leaves caller's stack exact).

### S3 — Sends, ICs, primitives `L`
Goal: real message dispatch end-to-end.
- MethodDictionary + hierarchy lookup + lookup cache (§6.1); send/return
  protocol + IC side tables with full state lattice (§4.3, §5.3); DNU with
  `Message`; primitive mechanism + smi/oops/ByteArray groups (§10); invocation
  counters (counting only — no trigger yet).
- **Gate:** unit tests driving IC transitions explicitly (empty→mono→poly→mega,
  guard self-heal after method redefinition §6.2); golden programs: dispatch
  through a 3-class hierarchy, super sends, DNU trace; primitive failure →
  bytecode fallback path.

### S4 — Blocks, closures, NLR, ensure `L`
Goal: the hard Smalltalk semantics, correct before performance ever matters.
- CompiledBlock, `push_closure`, Contexts + captured-temp slots (§5.4); block
  `value` primitives; NLR with unwind + dead-home detection; `ensure:` /
  `ifCurtailed:` markers; `mustBeBoolean`.
- **Gate:** golden programs — counter closure (shared mutable capture), nested
  blocks 3 deep with ctx-temp access, NLR through 2 frames running `ensure:`
  blocks in order, `cannotReturn:` on escaped block, block re-entry after home
  return (non-NLR use is legal).

### S5 — Source compiler `L`
Goal: `.mst` in, running code out; hand-assembly retires.
- Lexer/parser (full expression grammar incl. cascades, literal arrays,
  pragmas-ignored) → AST → codegen (§4.5): name resolution, capture analysis,
  inlined control selectors, literal frames, IC tables; class-definition brace
  syntax + world.list loader (§1.2, §3.2 step 2); REPL + script runner
  (`macvm run foo.mst`).
- **Gate:** golden AST→bytecode listings for a corpus of methods (each opcode
  exercised); parse-error messages with line/col; end-to-end: `Point.mst`
  program prints expected transcript; every S2–S4 golden program re-expressed
  in source and passing.

### S6 — Core library + in-language test suite `L`
Goal: enough of a class library that the VM tests itself in Smalltalk.
- `world/`: Object, Boolean/True/False, UndefinedObject, Magnitude,
  SmallInteger (+ LargeInteger fallback arithmetic), Double, Character
  (flyweight), String/Symbol, Array/ByteArray, OrderedCollection, Dictionary,
  Association, Interval, WriteStream basics, Transcript (stdout).
- SUnit-lite (§12.3) + first ~200 assertions covering the library and S3–S4
  semantics.
- **Gate:** `cargo test` runs the in-language suite green; fib(25) and sieve
  run with correct answers; interpreter throughput measured & recorded
  (baseline for §13).

## Phase B — garbage collection

### S7 — Scavenger + write barrier `L`
Goal: the young generation collects; allocation is no longer mortal.
- Survivor spaces, Cheney scavenge with forwarding, age + adaptive tenuring
  (§7.3); card table + store barrier through the one store choke point (§7.4);
  old-gen object-start offset tables; Handle/HandleScope discipline retrofit
  over the runtime (§7.6); `MACVM_GC_STRESS=1` mode.
- **Gate:** entire S6 suite green under `MACVM_GC_STRESS=1` (scavenge every
  allocation); unit tests — forwarding, tenuring histogram math, card indexing,
  dirty-card scan finds exactly the recorded old→new refs; allocation-torture
  program (10M short-lived objects) completes in bounded memory.

### S8 — Full GC (mark-compact) `L`
Goal: unbounded-lifetime programs; heap grows and compacts.
- Worklist mark, slide compact, displaced-mark side map, reference rewrite,
  interpreter-stack fixup, lookup-cache flush (§7.5); old-gen segment growth;
  `gcFull` primitive; heap statistics.
- **Gate:** suite green under `MACVM_GC_STRESS=full`; compaction test —
  fragment old gen, full-GC, assert live set intact (checksum object graph
  before/after) and space reclaimed; identity-hash stability across compaction;
  soak: 1-hour churn run with flat memory ceiling.

## Phase C — native code substrate

### S9 — Vendor JASM + code cache spike `M`  *(can start any time after S0)*
Goal: emit and execute arm64 code from MACVM, trust intact.
- Vendor `wfasm` slice (a64 encoder, backend contract, `MacJit`, relocpatch) +
  its frozen 1,181-form corpus test (analysis §4.4); implement the `Assembler`
  trait over `encode::encode` (structured, no text); code cache with stub
  segments (§9); literal-pool emission + Oop reloc records.
- **Gate:** corpus test green in-tree; JIT smoke — emit `(x+1)*2`, an internal
  branch, a call to a Rust `extern "C"` fn, execute all three; write-protect
  toggle + icache-flush round-trip test; literal-pool word patch-and-rerun test.

### S10 — Compile straight-line methods `L`
Goal: tier 1 exists — the simplest methods run native.
- Bytecode→CFG decode + SSA-lite IR + lowering + linear scan + emit (§8.3) for
  send-free methods (arith, instvar/temp access, jumps, return); nmethod format
  + code table (§8.2); interpreter IC dispatches to nmethod ids; invocation-
  counter trigger + synchronous compile (§8.1); `MACVM_JIT=off|threshold=N`.
- **Gate:** differential — suite green with `threshold=1` (everything eligible
  compiles immediately); disasm golden for 3 reference methods; compiled
  arithmetic speed as a **tripwire** (warn < 5×, fail < 2× interpreter — perf
  is not gated before S15, rule 3); frame walk still prints mixed
  compiled/interpreted traces. Includes the nmethod literal-pool `oops_do`
  root hook (first oop-bearing nmethod lands here — SPEC §15 A10).

### S11 — Compiled sends, PICs, patching `L`
Goal: compiled code calls compiled code; the IC story is complete.
- Klass-guard prologue + verified entry (§8.2); mono call sites with
  InlineCache relocs + patch protocol (Branch26/veneer + icache flush); PIC
  stubs; megamorphic lookup stub; compiled→interpreter calls (and back) via
  adapter frames; runtime stubs (§9); allocation fast path in compiled code.
- **Gate:** suite green at `threshold=1`; IC-patch unit tests (mono→PIC→mega on
  live call sites, veneer path forced by far target); mixed-tier call matrix
  test (I→C, C→I, C→C, super, DNU from compiled); dispatch micro as a tripwire
  (warn < 5×, fail < 2× — rule 3). Ships a **temporary GC bridge** (old-direct
  allocation while compiled frames are live) so this gate runs before oop maps
  exist; S12's first commit deletes it (SPEC §15 A13).

### S12 — Moving GC under compiled frames `L`
Goal: the two hard subsystems coexist — the biggest integration risk, retired
before inlining lands.
- Oop maps at safepoints from regalloc oop-ness (§8.3, §8.5); compiled-frame
  stack walking (FP chain + PcDesc lookup); scavenge + full GC with compiled
  frames on stack (registers spilled at safepoints in v1 — no live-register
  maps yet, calls/allocs are the only safepoints); nmethod literal-pool oop
  update in GC; code-cache↔heap invariants.
- **Gate:** suite green with `threshold=1` **and** `MACVM_GC_STRESS=1`
  simultaneously (the flagship gate); unit tests — oop map decode, a compiled
  frame's spill slots relocated correctly across a forced scavenge mid-loop;
  full-GC moves an object referenced from an nmethod literal pool.

## Phase D — adaptive optimization

### S13 — Deoptimization `L`
Goal: the safety net that makes speculation legal.
- Scope descs + PcDescs emission (LEB128) (§8.7); uncommon trap (`brk`) +
  Mach/SIGTRAP handler → frame materialization onto the interpreter stack;
  not_entrant patching + lazy invalidation deopt on return; dependency index
  (klass/selector → nmethods) wired to method redefinition (§8.6);
  `MACVM_DEOPT_STRESS=1`.
- **Gate:** suite green under deopt-stress (every guard fires once; periodic
  invalidation); unit — scope-desc round-trip, materialized frame equals the
  frame the interpreter would have built (checked by executing both paths);
  redefine a method mid-loop and observe correct continuation.

### S14 — Type feedback, inlining, customization `L`
Goal: the actual point of the lineage — optimized sends.
- Feedback read from IC tables/PICs; inlining with budgets + levels;
  customization keyed nmethods; block inlining + Context elision; uncommon
  traps for cold IC states; recompilation policy (caller preference, levels
  1–4, version cap) (§8.1, §8.4).
- **Gate:** suite green under all three stress modes combined; fib/sieve ≥ 10×
  interpreter; `do:`/`inject:into:` loops show Context-elision (assert zero
  Context allocs in steady state via GC stats); recompilation-level ladder
  observable in `-v` logs and capped.

### S15 — OSR + performance hardening `M`
Goal: hot loops enter compiled code without waiting for re-invocation.
- Loop-counter OSR entries + interpreter-frame→compiled-frame conversion
  (§8.7); Richards + DeltaBlue ported to `world/bench/`; profile-guided fixes;
  performance-target table (§13) measured and recorded in `docs/PERF.md`.
- **Gate:** long-running-loop benchmark reaches compiled speed without method
  re-entry; Richards ≥ 5× interpreter; no regression in stress suites.

## Phase E — stretch (unordered)

- **S16 Snapshot/image** — save/load the world (root schema = Universe list;
  no Rust vtables in-heap makes this mechanical §3.2).
- **S17 Green processes + scheduler** (§11); `Processor yield`, delays.
- **S18 Exceptions** — ANSI `Exception` hierarchy over NLR/ensure.
- **S19 Splitting & advanced opts** — Self-style splitting, better regalloc.
- **S20 Cocoa bridge** — `objc_msgSend` via the MacModula2 pattern; Transcript
  window as the hello-world.
- **S21 Mixins** — Strongtalk's mixin model on the reserved klass slot.
- **S22 Weak refs + finalization; weak symbol table.**

---

## Standing rules

1. A sprint is done only when **all** stress modes that exist so far are green.
2. Every bug found post-sprint gets a regression test in the layer where it
   *should* have been caught.
3. Performance numbers are recorded (`docs/PERF.md`), never gated until S15.
4. No new `unsafe` outside `src/oops`, `src/memory`, `src/codecache`,
   `src/jit` (enforced by `#![forbid(unsafe_code)]` elsewhere).
5. SPEC.md is amended (with a Δ note) whenever implementation teaches us the
   spec was wrong — the spec stays true.
