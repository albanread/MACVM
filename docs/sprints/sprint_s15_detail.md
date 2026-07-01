# Sprint S15 — OSR + performance hardening

Hot loops enter compiled code without waiting for method re-invocation:
loop-counter overflow compiles the method **with an OSR entry** at the loop
header and transfers the running interpreter frame into a compiled frame
(the reverse of S13 deopt). Then the sprint hardens performance: Richards and
DeltaBlue are ported to `world/bench/`, the SPEC §13 target table is measured
into `docs/PERF.md`, and the VM grows the stats counters that make profiling
honest. Implements the OSR half of SPEC §8.7 plus SPEC §13; evidence:
`reference-vm-analysis.md` §2.5 (Self `replaceOnStack`), `arm64.md` §7.

## Prerequisites

- **S5.5/S10** — `jump_back` loop counter folded into `MethodOop.counters`
  with `LoopCounterLimit = 10_000` (SPEC §5.5); until now overflow triggered a
  plain method recompile (S10 behavior, replaced this sprint).
- **S13** — scope descs/PcDescs, `deoptimize_frame` + `FrameView` +
  `interpret_active` (nested resume), loop-poll deopt slow path
  (`pending_deopt_flag`).
- **S14** — full inlining pipeline with `osr_bci` plumbing point reserved
  (`Inliner` unchanged; the pipeline takes `osr_bci: Option<u16>`),
  `trigger`/`pick_recompilee` gates (version cap, effectiveness),
  `__vmStats` primitive, `contexts_allocated` etc.
- **S6/S12** — bytecode abstract interpreter (stack model per bci) shared by
  compiler decode and the S13 M4 height checker.
- Mixed-tier calling: interpreter enters compiled code via a Rust→JIT call
  returning the result oop in `x0` (S11); compiled frames live on the native
  stack, interpreter frames on the `ProcessStack` (SPEC §5.1).

## Deliverables

- `src/compiler/` — OSR entry block generation (`osr_bci` parameter through
  decode/inline/lower/emit), `OsrMap` emission into the nmethod.
- `src/runtime/osr.rs` — `rt_osr_request`, frame-conversion packing, the
  interpreter-side transition; `osr_table` in `CodeTable`
  (`lookup_osr/install_osr`).
- `jump_back` slow path rewritten: counter overflow → `trigger_osr`.
- `world/bench/` — `Bench.mst` harness, `richards.mst`, `deltablue.mst`,
  `fib.mst`, `sieve.mst`, `dispatch.mst`, `churn.mst` (the SPEC §12.8 set),
  `bench.list`.
- `docs/PERF.md` + `just perf` measurement script.
- Stats counters + `MACVM_TRACE=stats` exit dump; profiling notes in this doc
  (Instruments/samply).

## Design

### Data structures

#### OsrMap — the interpreter→compiled frame-conversion map

```rust
// src/compiler/scopes.rs (same blob family as ScopeDescs; packed with the
// same LEB128 primitives; stored at nmethod header.osrmap_off)
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OsrSource {
    Receiver,
    Slot(u16),        // unified arg/temp slot index (SPEC §4.1 numbering)
    StackSlot(u16),   // interpreter operand-stack element i (0 = bottom)
    Context,          // the frame's heap Context oop (methods with has_ctx)
}

#[derive(Copy, Clone, Debug)]
pub struct OsrSlot {
    pub src: OsrSource,
    /// Destination: canonical spill home (byte offset from the compiled
    /// frame's FP) of the vreg that is live-in at the OSR entry block.
    pub dst_frame_off: i32,
}

#[derive(Clone, Debug)]
pub struct OsrMap {
    pub osr_bci: u16,        // the loop-header bci this entry serves
    pub entry_off: u32,      // code offset of the OSR entry block
    pub frame_words: u32,    // compiled frame size the OSR prologue allocates
    pub slots: Vec<OsrSlot>, // in buffer order (see A3)
}
```

One nmethod has **at most one** OSR entry in v1 (the loop that triggered).
An OSR nmethod is otherwise a normal nmethod: same key `(klass, selector)`,
normal entry points (it also serves future calls), scope descs everywhere —
an OSR frame can deopt like any other (A5).

```rust
// CodeTable additions
pub fn lookup_osr(&self, key: (KlassOop, SymbolOop), bci: u16) -> Option<NmethodId>;
pub fn install_osr(&mut self, key: (KlassOop, SymbolOop), bci: u16, nm: NmethodId);
```

(Backed by a side map `osr_table: HashMap<(Oop, Oop, u16), NmethodId>` with
the same rebuild-after-GC discipline as the S13 dependency index.)

#### Runtime entry

```rust
// src/runtime/osr.rs
pub enum OsrOutcome {
    /// Compiled execution completed the whole activation; here is its result.
    Completed(Oop),
    /// Compilation declined (version cap / effectiveness / not compilable):
    /// keep interpreting; counter reset so we don't re-trigger every backedge.
    Declined,
}

/// Called from the interpreter's jump_back slow path on loop-counter
/// overflow. `fp` is the current interpreter frame; `target_bci` is the
/// backward jump's destination (the loop header).
pub fn rt_osr_request(vm: &mut VmState, fp: FrameIx, target_bci: u16) -> OsrOutcome;

pub fn trigger_osr(vm: &mut VmState, receiver: Oop, method: MethodOop, bci: u16)
    -> Option<NmethodId>;   // compile-with-OSR-entry via the S14 pipeline
```

### Algorithms

#### A1. Why the OSR entry can only be at a loop header (backedge target)

Three independent reasons — all load-bearing, spell them out in code comments:

1. **Detection point**: the interpreter only checks the loop counter at
   `jump_back` (SPEC §5.5), so the only interpreter states ever offered for
   conversion are "about to (re)enter the header bci".
2. **Canonical frame shape**: at a bytecode-boundary join point the operand
   stack has a single well-defined shape — the source compiler guarantees
   balanced stacks at jump targets (its abstract stack model must merge
   consistently at the header or codegen would have been rejected). Mid-
   expression pcs have transient stack states that differ between the
   interpreter's model and the optimizer's value numbering; the header is
   where both agree by construction. In practice the operand stack at an
   inlined-`whileTrue:`/`to:do:` header is empty or holds only loop-invariant
   values; the OsrMap handles either via `StackSlot`.
3. **Compiled-side landing pad**: the compiled CFG has a basic-block boundary
   exactly at the header (it is a jump target in decode), so "live-in vregs of
   the header block" is a well-defined set the regalloc can report; any other
   pc would land mid-block where no consistent vreg assignment exists.

#### A2. Compile-with-OSR-entry

`trigger_osr` runs the S14 gates first (`MAX_VERSIONS`, effectiveness — a
declined OSR resets the method's loop counter and returns None), then invokes
the normal pipeline with `osr_bci = Some(target_bci)`; the compiler:

1. Compiles the whole method normally (the OSR nmethod is also the method's
   nmethod for future calls; it replaces the CodeTable entry for the key).
2. Marks the CFG block whose bci == `osr_bci` in the **root scope** as the
   OSR target. `osr_bci` must be a root-scope bci: OSR never enters an inlined
   loop of a *different* method — the triggering frame is an interpreter
   activation of exactly this method. (Inlined callees' loops inside this
   compilation are fine; they just aren't OSR targets.)
3. **Context rule** (the pitfall from SPEC §8.7's design intent): if the
   method `has_ctx`, the interpreter frame already owns a materialized heap
   Context. The OSR compilation therefore **disables Context elision for the
   root scope** (S14 A7 Step 4 is skipped for it; inlined scopes keep their
   own elision) and compiles all root ctx-temp access through the incoming
   Context oop. The OSR entry must not be placed in a loop whose body
   references a root Context unless that Context is materialized first — and
   with this rule it always is: the interpreter materialized it at method
   entry (SPEC §5.3), and the OsrMap carries it in via `OsrSource::Context`.
4. Builds a synthetic **OSR entry block**: prologue (push FP/LR, allocate
   `frame_words`), then a copy sequence reading the incoming OSR buffer
   (pointer arrives in `x1`; `x0 = &mut VmState`-compatible… see A3 register
   contract) and storing each value to its `dst_frame_off`, then an
   unconditional branch to the loop-header block. Values that the header
   block expects in registers are loaded by the header block itself from the
   spill homes — v1 keeps the entry block store-only (no shuffle problem).
5. Emits the `OsrMap`: `slots` = for each live-in interpreter entity of the
   header block (receiver if live, each live unified slot, each operand-stack
   element at the header per the abstract stack model, the Context if
   has_ctx), its `OsrSource` and destination spill home. **Dead entities are
   omitted** — deopt back out is still complete because scope descs record
   dead slots as `ValueLoc::Nil` (S13 materialization writes nil, matching
   what a fresh interpreter frame would hold… see Pitfalls for the honest
   caveat and the debug check).
6. A `PcDesc` + safepoint record is emitted for the OSR entry landing point
   (the loop-header block's first pc) with `reexecute=true` at `osr_bci` —
   this is what lets an OSR frame deopt immediately if e.g. the very first
   guard in the loop body fails.

#### A3. The transition sequence (interpreter → compiled, in place)

In `jump_back`'s slow path (counter overflow), all inside the interpreter's
Rust function:

1. `outcome = rt_osr_request(vm, cur_fp, target_bci)`.
2. `rt_osr_request`: look up `lookup_osr(key, bci)` (key = (klass_of(frame
   receiver), method selector)); on miss call `trigger_osr` (synchronous
   compile, SPEC §8.1); on Declined → reset loop counter, return `Declined`
   (interpreter continues at the header as if nothing happened).
3. **Pack the OSR buffer**: allocate a plain `Vec<u64>` (Rust-side, not heap
   — no GC can run during packing because nothing allocates on the VM heap);
   for each `OsrSlot`, read the value from the interpreter frame
   (`Receiver` → FP+4; `Slot(i)` → arg area / FP+5.. per SPEC §5.1 unified
   mapping; `StackSlot(i)` → operand stack base + i; `Context` → FP+3) and
   push it in map order.
4. **Read the return-linkage first, then pop**: save the frame's `saved_fp`
   and `saved_bci` locals, then pop the interpreter frame from the
   ProcessStack (sp drops to the frame's receiver slot — the same point a
   normal return would cut to, SPEC §5.1). The frame is gone **before**
   compiled code runs, so no stale duplicate of the loop state exists for GC
   to scan (contrast with S13's benign-duplication note: there the compiled
   frame couldn't be popped early; here the interpreter frame can).
   Between step 3's reads and step 5's call there is **no allocation and no
   safepoint** — the buffer's raw oops are safe exactly because this window
   is allocation-free; assert it in debug builds (`vm.assert_no_alloc_scope`).
5. **Enter**: Rust→JIT call to `nm.code_base + osr_map.entry_off` with the
   standard S11 compiled-entry register contract (`x28` = VM state, etc.)
   plus `x1 = buffer.as_ptr()`. The OSR entry block builds the compiled frame
   and jumps into the loop header. From the native stack's point of view this
   is an ordinary compiled activation whose caller is the interpreter's Rust
   call site.
6. **Return**: the compiled method eventually returns its result oop in `x0`
   (`Completed(result)`). The interpreter performs the standard method-return
   with the saved linkage from step 4: push `result` onto the caller's
   operand area (receiver slot overwrite per SPEC §5.1), restore
   `fp = saved_fp`, `bci = saved_bci`, continue the loop. The replaced
   activation has been executed **partly interpreted, partly compiled, with
   no method re-entry** — the SPRINTS S15 gate's observable.

#### A4. jump_back integration

The existing `jump_back` handler (SPEC §5.5) keeps its order: (a) GC/interrupt
poll first, (b) loop-counter bump; on overflow, `rt_osr_request`. Overflow
handling resets the counter in **every** outcome (Completed/Declined) so a
long-running *declined* loop re-triggers only every `LoopCounterLimit`
backedges, not on every one.

#### A5. OSR frame deopting back (interaction with S13 — no new machinery)

An OSR frame is a normal compiled frame with normal scope descs, so:

- **Uncommon trap inside the loop** → S13 D3–D5 verbatim: trampoline,
  `rt_uncommon_trap`, materialization onto the ProcessStack, nested
  `interpret_active`, result returned in `x0` to the OSR frame's native
  caller — which is the interpreter's step-5 call site, which then runs
  step 6 unchanged. The activation goes interpreted → compiled → interpreted
  and nobody upstream can tell.
- **Invalidation while the OSR frame is live** → the S13 walk finds callee
  frames whose saved-LR points into the nmethod (return-path redirect) and
  the loop-poll `pending_deopt_flag` path catches fully-inlined loop bodies;
  both end in the same materialize-and-resume flow.
- The only OSR-specific fact: the materialized chain's outermost frame is the
  root method at some mid-loop bci — which is exactly what M3–M5 already
  produce from the scope desc. **No code special-cases "was OSR".**

#### A6. Richards + DeltaBlue porting notes (`world/bench/`)

Both are classic Smalltalk benchmarks with well-known public versions; the
implementer ports from those (the Strongtalk distribution's benchmark suite,
Squeak/Pharo's `RichardsBenchmark`/DeltaBlue packages, and the mirrored
copies in cross-language suites such as *are-we-fast-yet* all agree on
structure and check values). Porting is mechanical to MACVM's `.mst` brace
syntax (SPEC §1.2); neither needs exceptions, `thisContext`, or floats-heavy
code.

- **`richards.mst`** — Martin Richards' OS-scheduler simulation. Classes:
  `RBObject` (root with `append:head:`-style helpers), `Packet`,
  `TaskControlBlock` + task subclasses (`IdleTask`, `WorkerTask`,
  `HandlerTask`, `DeviceTask`), `TaskState`, `RichardsBenchmark` (the
  scheduler). Needs: identity comparisons, `Array`, blocks, symbols; state
  machines are plain instance variables. **Validation (canonical)**: after
  one run, `queuePacketCount = 23246` and `holdCount = 9297`; the benchmark
  method answers false/raises `error:` if the counts differ — the golden
  transcript asserts them.
- **`deltablue.mst`** — Wilson/Freeman-Benson one-way constraint solver.
  Classes: `Strength` (7 class-side singletons: `required`, `strongPreferred`
  … `weakest`), `AbstractConstraint` → `UnaryConstraint` (`StayConstraint`,
  `EditConstraint`) and `BinaryConstraint` (`EqualityConstraint`,
  `ScaleConstraint`), `Variable`, `Planner`, `Plan`. Needs
  `OrderedCollection` (`add:`, `removeFirst`, `do:`), `Dictionary` for
  strength ordering, heavy block usage (`do:`, `satisfy:`, planner
  propagation loops — this is the Context-elision showcase). **Validation**:
  the standard `chainTest:`/`projectionTest:` assert final variable values
  (`v value = expected` else `error:`) — port those asserts as-is.
- **`Bench.mst` harness**: `Benchmark subclass: #<name>` protocol
  `setUp / runOne / checkResult`; class-side
  `Bench run: aBenchClass inner: i outer: o` times with `millisecondClock`
  (SPEC §10 system group), prints `name inner-iterations median-ms` one line
  per benchmark. `bench.list` orders the files; `macvm run world/bench/all.mst`
  runs the suite.

#### A7. PERF.md format + measurement procedure

`docs/PERF.md` is append-only history + a current table:

```markdown
# MACVM performance record
Machine: <chip, RAM, macOS version>. Methodology: see S15 detail A7.

## Current (<date>, <git describe>)
| benchmark | interp (ms) | jit t=1 | jit default | best/interp | target (SPEC §13) | met? |
|---|---|---|---|---|---|---|
| fib(30)        | … | … | … | …× | ≥10× | ✔ |
| sieve          | … | … | … | …× | ≥10× | ✔ |
| dispatch micro | … | … | … | …× | ≥5× (S11 row) | ✔ |
| Richards       | … | … | … | …× | ≥5×  | ✔ |
| DeltaBlue      | … | … | … | …× | (tracking) | – |
| scavenge pause p50/p95 (µs) | … | | | | <1 ms | ✔ |
| full-GC pause @100MB live (ms) | … | | | | <50 ms | ✔ |
| interp throughput (Mbc/s) | … | | | | ≥50 | ✔ |

## History
<one dated block per recording, oldest last — never rewritten>
```

**Procedure** (scripted, `just perf`): per benchmark and mode — 3 in-process
warmup iterations (discarded; lets counters trigger, code compile, heap reach
steady state), then **N = 10 timed iterations, report the median** (median,
not mean — one GC-unlucky run must not skew). Wall clock via
`millisecondClock` inside the harness (not `time(1)` — excludes genesis +
world load). Fixed conditions recorded in the file header: mains power,
`MACVM_HEAP` pinned, no other load; script refuses to run if
`MACVM_GC_STRESS`/`DEOPT_STRESS` are set. Standing rule 3: numbers are
recorded every sprint from here on; only the SPRINTS S15 gate rows gate.

#### A8. Profiling guidance + VM stats counters

- **Instruments** (Time Profiler template) works on the ad-hoc-signed dev
  binary (`arm64.md` §2 — no entitlement ceremony). JIT frames show as
  unsymbolicated addresses inside the MAP_JIT region; correlate with
  `MACVM_TRACE=jit` output (nmethod name → code range log lines). **samply**
  (`samply record ./target/release/macvm run world/bench/all.mst`) gives the
  same view in a browser.
- What to look for, and the counter that quantifies each suspicion
  (all in `VmState.stats`, dumped at exit under `MACVM_TRACE=stats` and
  readable via `__vmStats`):

| Suspicion | Counters (added this sprint unless noted) |
|---|---|
| dispatch overhead | `ic_hits`, `ic_misses`, `pic_extends`, `mega_transitions` (miss rate = misses/(hits+misses); healthy steady state ≪ 1%) |
| GC pressure | `scavenge_count`, `scavenge_us_total/max`, `full_gc_count/us`, `bytes_allocated`, `contexts_allocated` (S14), `tenured_bytes` |
| speculation health | `deopt_count` by reason (S13), `traps_top_sites` (top-10 (nmethod, off, count) dump), `recompile_declined_ineffective` (S14) |
| tier balance | `compilations_by_level[4]`, `osr_entries`, `osr_declined`, `bytecodes_interpreted` (debug builds; sampled counter in release) |
| code cache | `code_bytes_alive`, `code_bytes_zombie`, `stubs_bytes` |

Counter discipline: plain `u64` fields bumped on slow paths only (IC *misses*
not hits — hits are counted by a sampling flag in debug builds so release
fast paths stay untouched; document per-counter cost class in `stats.rs`).

### Layer boundaries

| Module | May touch | Must not touch |
|---|---|---|
| compiler OSR additions | decode/inline/lower/emit internals, OsrMap packing | ProcessStack, interpreter loop |
| `runtime/osr.rs` | interpreter frame accessors, ProcessStack pop, CodeTable, Rust→JIT entry helper (from `codecache`) | signal handling, code patching |
| `world/bench/` | pure Smalltalk + `millisecondClock`/`__vmStats` | dev hooks other than stats/clock (benchmarks must run on a stats-less build with only timing degraded) |

## Implementation order

1. Stats counters + `MACVM_TRACE=stats` + `__vmStats` extension (needed by
   everything below to be observable).
2. `OsrMap` pack/unpack + unit tests.
3. Pipeline `osr_bci` plumbing: entry-block generation + map emission, root-
   scope elision disable; golden disasm for one OSR nmethod.
4. `rt_osr_request` + buffer packing + frame pop + enter/return (A3);
   `jump_back` slow path switched over; `osr_table`.
5. OSR × deopt round-trip hardening (A5) under all stress modes.
6. `world/bench/` ports + harness + golden check-value transcripts.
7. `just perf` + `docs/PERF.md` initial recording; profile-guided fixes
   (bounded: only regressions against SPEC §13 rows get fixed this sprint).

## Pitfalls

- **The pack→enter window must be allocation-free** (A3 step 4): the OSR
  buffer holds raw oops in a Rust `Vec`. Any heap allocation (even a trace
  string) between reading the frame and entering compiled code is a GC hole
  that only `MACVM_GC_STRESS=1` will catch. Guard with the debug
  no-alloc-scope assert.
- **Pop before enter, linkage first**: popping the interpreter frame after
  reading `saved_fp/saved_bci` but before the JIT call is what makes the
  replacement "in place". Entering first and popping later leaves a stale
  frame that a mid-loop scavenge scans — wrong direction; don't.
- **Root-scope Context elision must be off** in OSR compilations (A2 step 3).
  If the optimizer elides the root Context, the incoming materialized Context
  (which the interpreter already handed to created closures!) diverges from
  the vreg copies — closures see stale temps. The rule is absolute, not a
  heuristic.
- **Dead-slot omission vs deopt** (A2 step 5): the OsrMap omits dead
  entities, but a later deopt of the OSR frame materializes them as nil via
  scope descs. If the interpreter would have had a *non-nil stale value* in a
  dead slot, the materialized frame differs — semantically invisible (the
  slot is dead) but it would trip a naive slot-exact differential test. The
  S15 differential test therefore compares **live** state + results, and
  `Frame::verify` checks tags, not values, for dead slots. Do not "fix" this
  by keeping dead slots alive.
- **OSR entry pc vs PcDesc conventions**: the entry block itself is not a
  safepoint (no map — it runs with the buffer as the only root, and the
  buffer is dead the instant copies complete); the first safepoint is the
  header block's (A2 step 6). Emitting a safepoint inside the copy sequence
  would need buffer-as-root support — out of scope, keep the block
  straight-line.
- **Counter reset on Declined** (A4): forgetting it turns every backedge of a
  version-capped loop into a failed compile attempt — a quadratic slowdown
  that looks like "OSR made things slower".
- **Benchmark honesty**: median-of-N with in-process warmup (A7), fixed heap,
  and check values asserted *every* run — a benchmark that stops validating
  its result will eventually be optimized into measuring nothing (classic
  trap with aggressive inlining + dead-code elimination; Richards' counts and
  DeltaBlue's asserts are the defense).
- **Instruments + traps**: profiling a run that deopts under lldb-attached
  Instruments can hit the S13 EXC_BREAKPOINT caveat; profile release runs
  without a debugger attached (samply does not intercept SIGTRAP delivery,
  Instruments' Time Profiler doesn't either — only *debugger*-attached
  sessions do).

## Interfaces for later sprints

- `OsrOutcome`/`rt_osr_request` — S17 green processes reuse the same
  transition when a process's hot loop compiles (per-process ProcessStack is
  already the unit of conversion).
- `stats` module + PERF.md discipline — every subsequent sprint appends
  (standing rule 3 becomes fully mechanical).
- `osr_table` rebuild hook — S16 snapshot save/load must flush it (code cache
  is not snapshotted).
- Bench suite — S19 splitting and any regalloc upgrade re-run `just perf`
  against the recorded history.

## Out of scope

- Multiple OSR entries per nmethod / OSR at non-root scopes — revisit only if
  profiles show nested-loop methods stuck interpreted (they escalate via
  normal recompilation today).
- OSR *between compiled versions* (optimized → newly-optimized on-stack
  replacement, Self's `replaceOnStack`) — not needed while invalidation +
  lazy deopt + re-trigger covers correctness; stretch with S19.
- Background/concurrent compilation, page-poll safepoints (SPEC §8.8 v2),
  code-cache compaction — post-v1.
- Interpreter peak-throughput work (SPEC §13 row 1 was measured in S6; only
  regressions are chased here).

> **SPEC-QUESTION:** SPEC §8.2's nmethod header field list has no
> `osrmap_off`; S15 adds it (alongside S14's counter/profile fields). §8.2
> should be amended with the final header layout.

> **SPEC-QUESTION:** SPEC §5.5 folds the loop counter into `counters`
> (invocation:16 | reserved). Sixteen reserved bits cap a distinct loop
> counter at 65,535 ≥ LoopCounterLimit=10,000 — adequate, but the field
> split (invocation:16 | loop:16 | rest) is not pinned anywhere; proposing
> SPEC §4.4 pin it.
