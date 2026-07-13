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

### S7.5 — Handle hardening (generational, validated, currency-level) `M`
Goal: close a **structural** flaw in the Handle/GC contract before S8 builds
more moving-GC machinery on top of it. Discovered S7-11 (2026-07-02): the
`Handle<T>` bug class (a bare oop-wrapper held across an allocation) was being
independently reintroduced across S7-9/S7-10/S7-11 — including inside fixes for
earlier instances and in the GC's own unit tests — because the *unsafe* type
(`Oop`/`KlassOop`/…) is the pervasive currency and `Handle<T>` an opt-in,
unvalidated, function-internal convenience (one `pub fn` signature total). No
amount of care fixes a bug class the API shape regenerates. Design of record:
**SPEC §7.6.1**, adopting the **Locus** sister VM's proven handle layer.
Which change fixes what (adversarial review, 2026-07-02 — do not repeat the
first draft's overclaim): the bugs actually hit were **bare oops, not misused
`Handle`s**, so **change 2 is the primary fix**; change 1 is a soundness
backstop + the prerequisite for change 2's type-safety, and catches only
handle-use-after-scope.
- Change 1 (do first — isolated to `memory/handles.rs`): persistent `gen`
  vector + `generation` field + **three-guard access** (in-range, not-vacated,
  gen-match) + **bump `gen` on vacate** (scope drop), not only on re-push —
  MACVM has no free-list/tombstone like Locus, so vacate-bump is what closes
  the false-negative. Debug-gated assert (handles are on the send hot path).
- Regression test (before change 2): reproduce the real failure — a helper that
  returns a bare oop past its scope no longer compiles once reshaped; a
  deterministic `stale_handle_panics`. A21's premise is that tests+stress alone
  were insufficient, so pin the fixed behavior with a test.
- Change 2 (the invasive part): reshape allocating functions
  (`install_method`, `MethodDictOop::insert`, `alloc_*`, `BytecodeBuilder`
  public surface) to take/return `Handle<T>`; ripple through **every** call
  site + tests. Ship an ergonomic klass-handle minting helper or the reshape
  gets resisted the way opt-in `Handle` was. **Reshape leaf allocators last**
  (they're called by `handles.rs`'s own machinery — circular otherwise).
- Change 4: primitives are a third, currently-unprotected surface
  (`prim_basic_new` holds `args[0]` across `alloc_slots`, safe only by the
  accident that klasses are old-gen). Bring them into the contract (hand
  `can_allocate` primitives handles, or enforce `prim_arg` re-read + lint).
- Explicitly NOT in scope: Locus's `newgc-core` collector (rejected,
  ref-analysis §5) and its conservative-scan MAGIC tag (MACVM uses precise
  oop-maps). See §7.6.1's "deliberately NOT imported."
- **Gate:** full suite green under `MACVM_GC_STRESS=1` *to completion* (the two
  hang-forever tests fixed in S7-11 were why this stress run had never actually
  finished — the precondition for trusting any GC gate); no `pub fn` that
  allocates internally takes/returns a bare oop-wrapper for a value held across
  the allocation; no `can_allocate` primitive reads its `args` copy after an
  alloc; `stale_handle_panics` + the bare-return-won't-compile test both
  present. Full reasoning: SPEC §7.6.1 + A21.

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

## Phase G — GUI track (parallel)

The MACVM user interface: the **Strongtalk live-HTML programming environment**
recreated in a native Cocoa window (WKWebView), built as the separate cargo
workspace member `gui/` (`macvm-gui`). Plan of record with visual ground truth
and decisions D-G1…D-G5: [`../gui/PLAN.md`](../gui/PLAN.md); the core-side
contract (VmHandle, TranscriptSink, mirrors, source registry, threading) is
SPEC §16. This track runs **concurrently** with the core phases; its gates
never block core sprints, and core stress gates never require the GUI.

| Phase | Size | Needs from core | Gate (from PLAN.md) |
|---|---|---|---|
| G0 static shell | `M` | nothing | start page + tour render period-faithful; nav + status bar live |
| G1 live-page runtime, stub host | `S` | nothing | doit click → transcript echo, full JS↔Rust round trip |
| G2 VM bridge | `M` | **S5** (eval/doit compile) + **S6** (Transcript, world) + SPEC §16.1–16.2 | original `startPage.html` fully live against MACVM |
| G3 outliner tools | `L` | S6 world + **W2/W3** (mirrors, ToolNode, HtmlWriter — see APPS.md) | browse Object hierarchy → class → category → method source, lazily |
| G4 code editing & find tools | `L` | **W4** (accept path) + source registry (A16); **S13 for JIT-on redefinition** (else `MACVM_JIT=off`) | tour's "little project" workflow end to end |
| G5 polish & parity sweep | `S` | — | every toolbar icon wired or consciously deferred |

Sequencing guidance: G0–G1 can start **immediately** (no VM dependency) and
are a natural parallel workstream during Phases A–B. If the GUI outruns the
core, extend G1's stub host with fixture data (PLAN.md §5) rather than
blocking. The S5/S6 sprint docs carry addenda for the two core-side
obligations this track adds (source registry; TranscriptSink routing).

## Phase W — world track (library + apps, parallel after S6)

The Smalltalk side beyond the S6 seed: the full library and the programming
tools, designed from a file-level survey of the real Strongtalk source.
Design docs: [`WORLD.md`](WORLD.md) (library, `.dlt→.mst` converter, load
order) and [`APPS.md`](APPS.md) (mirrors, tools, HtmlWriter, reflection
primitives R1–R5). Like Phase G, this track runs parallel to the core and
interleaves with it; every wave lands with in-language tests.

| Wave | Size | Contents | Needs |
|---|---|---|---|
| W0 image store | `S` | `image_store` crate: versioned SQLite class/method source database + `.mst` importer + GUI class-browser wiring ([`IMAGE.md`](IMAGE.md)) | nothing — `.mst` text already exists |
| W1 library wave 1 | `M` | `tools/dlt2mst` converter (WORLD §10); full collections protocol, Fraction, Character tables, ReadStream; two-pass world loader; Strongtalk test-oracle port | S6 |
| W2 reflection base | `M` | Mirror library (Object/Class/Method mirrors), R1+R2 primitives, HtmlWriter, ToolNode framework (APPS §2–§6) | S6 (+A16) |
| W3 tools wave 1 | `M` | Inspector, Workspace/eval wiring (R3), find tools (senders/implementors sweeps) | W2; pairs with G2/G3 |
| W4 tools wave 2 | `L` | Browser node suite (hierarchy/class/category/method), accept path, update protocol | W3; pairs with G4 |
| W5 exceptions + SUnit | `M` | ANSI exception layer (~10 classes, zero new VM features — WORLD §7), SUnit 3.1 convergence; adds exception tests to S13+ stress gates | S4 solid, W1 |
| W6 benchmark harvest | `S` | Richards (13 cls), DeltaBlue (12 cls), Stanford suite + seeded LCG, BenchmarkRunner-style harness in `world/bench/` | W1; **feeds S15** |
| W-debugger | `L` | StackTraceInspector + ActivationMirror + R5 primitives (single-step interpreter mode) | W4; schedule with Phase-E processes |

## Phase E — stretch (unordered)

- **S16 Snapshot/image** — save/load the world (root schema = Universe list;
  no Rust vtables in-heap makes this mechanical §3.2).
- **S17 Green processes + scheduler** (§11); `Processor yield`, delays.
- **S18 Exceptions** — ANSI `Exception` hierarchy over NLR/ensure.
- **S19 Splitting & advanced opts** — Self-style splitting, better regalloc.
- **S20 Guest-language Cocoa + POSIX/BSD bridge** — Smalltalk code sending
  messages to Cocoa objects via `objc_msgSend` (the MacModula2 pattern) and
  calling curated libc functions directly, both ABI-driven by `cocoa_data`
  (a sibling repo's shared SQLite mirror of the macOS Obj-C + POSIX surface).
  Full design in [`docs/FFI.md`](FFI.md) (written as a non-disruptive side
  track, alongside but independent of S11–S14): two tiers (dynamic
  `doesNotUnderstand:`-based Cocoa dispatch reusing S11's PIC machinery for
  caching; a direct compiler-primitive path for POSIX calls), an `Alien`-style
  byte-array-backed representation reusing existing `IndexableBytes`
  primitives, and a working, tested offline generator crate (`ffi_gen`,
  already built) that emits real `.mst` bindings — forward-declared against a
  VM-side primitive this sprint still has to build. Distinct from the Phase G
  GUI shell, which is Rust-side hand-rolled `dlopen`/`objc_msgSend`
  (`gui/src/objc.rs`) and needs none of this.
- **S21 Mixins** — Strongtalk's mixin model on the reserved klass slot.
- **S22 Weak refs + finalization; weak symbol table.**
- **S23 ASM methods** — a method whose entire body is hand-written native
  AArch64, not compiled Smalltalk. Full design in [`docs/ASM.md`](ASM.md)
  (written as a non-disruptive side track, same posture as S20): a new
  `<asm: 'text'>` pragma (string-wrapped so `.mst`'s bracket-matcher never
  sees ARM64 addressing-mode `[`/`]` unescaped), a precise register contract
  reusing the calling convention `docs/arm64.md` §3 already defines for
  compiled code, and a v1 restriction to leaf routines only (no allocation,
  no calls, no safepoints — what licenses skipping oop-maps entirely).
  Reuses the existing `Nmethod`/`CodeTable` install mechanism verbatim, just
  fed bytes from JASM's real text assembler (`wfasm::a64::assemble`,
  vendored S9, otherwise unused in the current codebase) instead of the
  tier-1 compiler's own pipeline. No Strongtalk precedent exists for this
  (checked directly against the source) — genuinely novel for this lineage.
  A working, tested preview tool (`asm_preview`, already built) proves the
  mechanism against real worked examples; the frontend parser and installer
  themselves are this sprint's still-to-build work.
- **Native game engine** — a retro game pane driven entirely from Smalltalk:
  a linked-in primitive group (ids 200–215) emits drawing/sprite/audio commands
  over a `GameSink` channel (mirroring `TranscriptSink`) that the GUI renders on
  a native Metal pane via the `MacGamePane` sister crate (Metal graphics +
  AVFoundation audio). Frame loop is a main-thread `NSTimer` pulling one
  `GameStep` per tick (the worker stays strictly serial); `run` returns
  immediately with the step-block GC-rooted in a class variable. Full design and
  the M0–M4 milestone ladder in [`gamepane_design.md`](gamepane_design.md);
  `world/43_gamepane.mst` (GamePane/Sprite/Sound/Tune) + the `Catcher`
  (`world/44`) and `MandelZoom` (`world/45`) demos, reachable from the GUI's
  native **Demos** menu. Same non-disruptive side-track posture as S20/S23.

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
