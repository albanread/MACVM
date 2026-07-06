# MACVM Debugger Design — HALT and PROBE

**Status:** Design proposal (no code yet)
**Prereqs read:** SPEC §5/§8.7/§16.3, APPS.md §3 (R1–R5) & §5.7, tracing_debugging.md,
`src/runtime/{frames,deopt,vm_state}.rs`, `src/codecache/{deopt_trap,nmethod,flush}.rs`,
`src/compiler/scopes.rs`, `src/interpreter/mod.rs`
**Fills in:** the deferred **R5 debugger** primitive group (APPS.md §3) and the
`W-debugger` sprint row (SPRINTS.md:293), plus a new low-level tier the plan
did not yet have.

---

## 0. One principle, two debuggers

MACVM runs the same Smalltalk method in two engines — tier-0 bytecode
interpreter and tier-1 compiled nmethod — and moves activations between them
through deopt. A debugger has to pick which representation it shows the user,
and the right answer differs by audience:

| | **HALT** (level 1) | **PROBE** (level 2) |
|---|---|---|
| For | Smalltalk programmers | MACVM/compiler developers |
| Debugs | *the program*, as bytecode + source | *the VM's own output*, as machine code |
| Unit shown | activation / bci / source range | nmethod / native pc / register |
| Strategy | **deoptimize to tier-0, step bytecode** | **freeze the physical frame, never deopt** |
| Trigger | high-level breakpoint, `halt`, DNU/`error:` | crash (`brk`, SIGSEGV/SIGBUS in code cache), low-level breakpoint |
| Trust model | trusts the VM (heap, walker, deopt) | trusts as little as possible — the VM is the suspect |

The split follows from one observation: **deopt destroys the evidence.** For a
user chasing their own logic bug, deopting to bytecode is exactly right — the
interpreter is the semantic reference, and stepping it is stepping the
language. For a VM developer chasing a miscompile (BUG D…), the compiled
frame's registers, spill slots, and instruction stream *are* the bug; the
moment you materialize interpreter frames you've replaced the crime scene with
the VM's own *story* about the crime scene — reconstructed by the very
machinery under suspicion. So HALT's first act is deopt, and PROBE's first
rule is *never* deopt.

Both run **inside the VM process** — no external debugger, no ptrace, no
second process. HALT is (eventually) Smalltalk-in-the-image over R5
primitives, Strongtalk-style; PROBE is Rust inside `macvm` itself, entered
from the signal path.

---

## 1. What already exists (the substrate)

The design deliberately adds **no new architecture** — every mechanism below
is in the tree today and gets reused, not rebuilt:

| Mechanism | Where | Debugger use |
|---|---|---|
| Frame walk, innermost-first, mixed-tier | `frames.rs:269` `walk_frames` → `FrameView::{Interpreted, Compiled{fp,ret_pc,nm}, Adapter, CallStub}` | both: the backtrace is this walk, verbatim |
| Frame layout (plain oop array, FP links, serials) | SPEC §5.1; `interpreter/stack.rs` | HALT: per-frame receiver/temps/operand-stack reads are the existing `Frame` accessors |
| Scope descs + PcDescs (per-safepoint virtual-frame chain, per-slot `ValueLoc`) | `scopes.rs:400,466,629` (`ScopeDescData`, `PcDesc`, `DecodedScope`), `nmethod.rs:292` `bci_at` | HALT: deopt input. PROBE: *read-only* map of where the compiler claims every value lives — diffable against reality |
| Full deopt: materialize interpreter frames, resume mid-method | `deopt.rs:264` `deoptimize_frame` → `DeoptResume`; `interpreter/mod.rs:296` `interpret_active`; `dispatch_from(method, bci)` enters at any bci | HALT: "debug this compiled activation" = exactly this path |
| Lazy deopt of mid-stack frames | `flush.rs:77,98` `make_not_entrant{,_lazy}`; `frames.rs:553` `redirect_returns_into_nm`; `vm_state.rs:661` `pending_deopts` | HALT: breakpointed method with live compiled activations → they convert on return, no stack surgery |
| Back-edge validity poll | SPEC §8.7; DeoptStats `Poll` reason | HALT: a breakpointed method spinning in a compiled loop still comes down at the next back edge |
| `brk #imm16` trap namespace + SIGTRAP handler + trampoline escape | `deopt_trap.rs:76,300` (`0xDE00` trap / `0xDE01` stress / `0xDE02` assert) | PROBE: the entry path; new immediates below |
| Per-method `compile_disabled` bit | `method.rs:147,154`; `driver.rs:462` | HALT: pin a method to tier-0 while it has breakpoints |
| Per-bytecode hook point in the dispatch loop | `interpreter/mod.rs:377-383` (trace/count checks already live there) | HALT: the breakpoint/step check goes in the same slot |
| Bytecode disassembler, pinned format | `bytecode/disasm.rs`, ISA.md | both: method view |
| Stack traces, compiled-frame aware | `error:`/DNU paths (tracing_debugging.md) | superseded by the walker-backed backtrace |
| Source registry (`Smalltalk methodSources`, A16) | SPEC §16.4 | HALT: source pane; needs a bci↔source-range map added (§5.7 APPS.md already calls for it) |
| Mirror architecture + GuiHost seam | SPEC §16.2/16.3, APPS.md §3 | HALT: the UI story |
| Heap verify, stack-write auditor, `MACVM_DBG_OOP` | `memory/verify.rs`, `stack.rs` (MACVM_DBG_STACK_WRITES), tracing_debugging.md | PROBE: forensic checks run at the frozen point |

What does **not** exist and must be built: a breakpoint store, a step machine,
the R5 primitives, SIGSEGV/SIGBUS handlers, a machine-code disassembler, a
bci↔source map, and the two command loops. That's the whole build list; each
is small because the substrate is not.

---

## 2. Shared core: `vm.debug`

One new `VmState` field, `debug: DebugState`, owned by a new
`src/runtime/debug.rs`:

```rust
pub struct DebugState {
    /// Master switch — the ONLY thing the interpreter fast path reads.
    /// false ⇒ every per-bytecode debug check is one predictable branch.
    pub active: bool,
    /// (method identity-hash, bci) → Breakpoint. Identity hashes are stable
    /// across compaction (A16/S8 gate), so this map survives full GC with no
    /// remap pass — the same GC-safety trick methodSources uses.
    pub breakpoints: HashMap<(i64, u16), Breakpoint>,
    pub step: Option<StepPlan>,          // §3.4
    pub session_depth: u32,              // §3.6 reentrancy guard
    pub probe: ProbeConfig,              // §4
}
```

**Breakpoint identity is (method, bci), not a patched bytecode.** MACVM
already chose side tables over self-modifying bytecode for ICs (SPEC §4.3);
breakpoints make the same choice for the same reasons: methods stay pure,
golden disassembly stays byte-identical, and no W^X/flush question exists at
tier 0. The per-bytecode cost is gated behind `debug.active` plus a new
method-flags bit `METHOD_FLAGS_HAS_BP` (set on any method carrying at least
one breakpoint), so the hash lookup runs only inside methods that actually
have breakpoints:

```rust
// dispatch_from, alongside the existing trace checks (interpreter/mod.rs:378)
if vm.debug.active && method.has_bp() {
    if let Some(bp) = vm.debug.hit(method, bci) { debug::on_breakpoint(vm, bp); }
}
if vm.debug.step.is_some() { debug::step_check(vm, method, bci); }
```

`debug.active` is false in every non-debugging run, so the entire feature
costs one untaken branch per bytecode — the same budget the `trace` checks
already spend. No opcode is consumed (the 0x44+ space stays free), and
`UPDATE_GOLDEN` never notices.

### 2.1 Setting a breakpoint = pinning the method to tier-0

`debug::set_breakpoint(vm, method, bci)`:

1. Insert into `debug.breakpoints`; set `METHOD_FLAGS_HAS_BP`.
2. `method.set_compile_disabled()` (`method.rs:154`) — the driver already
   honors this bit (`driver.rs:462`), so the method never compiles again
   while the breakpoint lives. Record whether the bit was previously clear so
   `clear_breakpoint` can restore it.
3. For every **existing** nmethod whose root or *inlined* scopes include this
   method (walk `CodeTable::iter_alive` + each nmethod's scope chain — the
   deps machinery already knows how to find inliners, §8.6):
   `make_not_entrant_lazy` (`flush.rs:77`). Entry points get the deopt-stub
   patch; live activations convert through the **existing** return-path
   redirect / back-edge poll. No new deopt mode, no mid-stack frame surgery.
4. Reset the method's invocation counters so the tier-up bookkeeping doesn't
   fire pointlessly.

Clearing the last breakpoint reverses all of it (restore `compile_disabled`
only if we set it; recompilation then happens naturally by counters).

This is the whole "high-level breakpoint ⇒ debugging happens on bytecode"
mechanism: by the time execution *reaches* a breakpointed bci, it is
guaranteed to be running in `dispatch_from`, because every compiled body that
could have contained it was invalidated through the standard S13 machinery
and re-entry was blocked by `compile_disabled`.

### 2.2 The bci↔source map (new, compiler-retained)

APPS.md §5.7 already commits to this direction ("MACVM can do better: bci →
source range"). Concretely: the frontend's codegen (`frontend/codegen.rs`)
emits, per CompiledMethod, a packed `Vec<(bci: u16, start: u32, end: u32)>`
sorted by bci, stored in the same A16 world-side registry pattern —
`Smalltalk methodSourceMaps`, an IdentityDictionary parallel to
`methodSources`, gated by the same `MACVM_KEEP_SOURCE`. Lookup = binary
search for the greatest entry ≤ bci. This serves HALT's "highlight the
current range" and costs nothing when disabled.

---

## 3. HALT — the Smalltalk-level debugger

### 3.1 Entry points

All of them funnel into one function, `debug::halt(vm, reason)`:

- **Breakpoint hit** (§2's dispatch check).
- **`halt` / `halt:`** — new primitive on Object (one line in the primitive
  table): unconditionally calls `debug::halt`. This is the programmer's
  `debugger;` statement.
- **`error:` and the DNU fallback** — today these print a trace and kill the
  process (SPEC §6.3). When `debug.active`, they call `debug::halt` instead,
  turning every would-be-fatal guest error into an inspectable stop. (When
  exceptions land in the world later, unhandled-signal joins this list; the
  hook is already in the right place because DNU/`error:` are where SPEC
  routes all user-level failure.)
- **Step completion** (§3.4).

### 3.2 What a halt is

The VM is single-process today (Process is Phase E, SPEC §11), so "suspend
the debuggee" cannot mean suspending one green thread among many. Instead:
**a halt is a nested command loop at a bytecode boundary.** `debug::halt`
stops between bytecodes (dispatch-loop boundaries are GC-safe and
frame-consistent by construction — same reason the GuiHost seam runs doits
"between interpreter turns", SPEC §16.1) and services debugger commands until
told to resume. The Smalltalk program is exactly as paused as it is during a
GC pause, and for the same structural reason.

The command loop has two frontends, one protocol:

- **CLI v1**: a `(halt) ` REPL on stdin/stderr inside `debug::halt` —
  `bt`, `frame N`, `temps`, `step`, `next`, `finish`, `continue`, `disasm`,
  `print <expr>`. Ships first; needs nothing from Phase W/E. `print` runs a
  doit via the R3 compile service against the selected frame's receiver.
- **GUI/W-debugger**: when a `GuiHost` is attached, `debug::halt` instead
  emits a `DebugEvent::Halted` over the existing VM→GUI channel and services
  the existing GUI→VM request channel — mirror queries against the halted
  stack, resume/step commands back. The StackTraceInspector of APPS.md §5.7
  is this loop's client, unchanged in concept.

### 3.3 The backtrace and frame inspection (R5, made concrete)

R5's `processStack: limit:` = `walk_frames` + classification:

- `FrameView::Interpreted{fp}` → an `ActivationMirror` reading the SPEC §5.1
  slots directly: `method` (FP+0), `receiver` (FP+4), temps (FP+7…), operand
  stack, `saved_bci`, Context (FP+3). All existing `Frame` accessors —
  read *and*, for temps, write (assignment in the debugger is a slot store).
- `FrameView::Compiled{fp, ret_pc, nm}` → decode the scope chain at
  `bci_at(ret_pc)` (`nmethod.rs:292`, `scopes.rs:651`) and present **one
  ActivationMirror per virtual frame** — inlined activations appear as
  separate backtrace entries, receiver/temps read through `ValueLoc`
  (`deopt.rs:124` `read_value`, reused verbatim as a read-only inspector).
  **Read-only in v1**: mutating state inside a live compiled frame means
  either deopt-now-in-place or write-back-through-ValueLoc; v1 sidesteps by
  marking these frames read-only in the UI. The user who wants to mutate
  sets a breakpoint (⇒ the frame converts on return / next poll) and mutates
  in the interpreter frame. This avoids the one genuinely new piece of deopt
  machinery ("synchronous deopt of an arbitrary mid-stack frame") the
  current code doesn't have — deferred, not forgotten (§6).
- `Adapter`/`CallStub` frames are elided from the user-facing trace
  (APPS.md §5.7 already plans eliding block-value frames; same posture).

Per-activation R5 primitives (`activationReceiver:`, `activationTempAt:put:`,
`activationBci:`, `activationSourceRange:` via §2.2's map, …) are thin
wrappers over the above — "small, dumb VM primitives" exactly as §16.3 pins.

### 3.4 Stepping

A `StepPlan` armed by the command loop, checked in the dispatch hook:

```rust
pub struct StepPlan {
    pub kind: StepKind,          // Into | Over | Out
    pub base_fp: usize,          // frame the command was issued in
    pub base_serial: i64,        // that frame's FP+5 serial
}
```

- **Into**: halt at the next bci where `fp != base_fp` *or* bci changed —
  i.e. the very next bytecode anywhere; entering a send's callee halts at
  its bci 0. (Primitive-successful sends never enter the loop, so stepping
  *into* a primitive shows the send completing — correct: primitives are
  atomic at this level. That's a feature, not a limitation: the primitive
  *is* the implementation boundary HALT promises.)
- **Over**: halt at the next bci with `fp <= base_fp` (callee frames sit
  above; equality = back in the same frame; less-than = the frame returned
  early, e.g. NLR unwound it — the serial check distinguishes "same fp,
  different activation").
- **Out**: halt at the first bci with `fp < base_fp`, i.e. after this frame
  returns.

Frame serials (SPEC §5.1 FP+5, dead-home detection) make these checks robust
against fp reuse — the exact problem they already solve for closures. NLR and
`ensure:` need no special cases: unwinding runs through the ordinary
interpreter paths, and the fp/serial comparison naturally fires at whatever
frame execution surfaces in. If a step crosses into a *compiled* callee (the
callee wasn't breakpointed and is still hot), v1 steps **over** it (the
dispatch hook can't see inside; the C2I/I2C seams guarantee we regain
control at the return) — with `MACVM_JIT=off` or after §2.1 pinning, this
case vanishes. Honest limitation, documented in the UI ("stepped over
compiled callee #foo — set a breakpoint inside it to descend").

### 3.5 Resumption commands

- `continue` — clear `step`, return from `debug::halt`, dispatch proceeds.
- `step/next/finish` — arm `StepPlan`, continue.
- `return <expr>` — force the current frame to return a value: compile the
  doit (R3), then perform the interpreter's own return sequence (pop to
  receiver slot, overwrite with result — SPEC §5.1). The machinery is the
  ordinary `return_tos` path.
- `restart` — reset the current *interpreter* frame to bci 0: pop the
  operand stack to the frame floor, re-nil temps, resume at 0. (Only valid
  for frames whose method has no already-executed side effects the user
  can't reason about — which is the user's judgment call, as in every
  Smalltalk debugger.)
- **hot recompile-and-continue** (the classic Smalltalk fix-and-go) is *out
  of scope for v1* but the shape is visible: R3 compile + S3 install +
  `restart` of the affected frame. Deferred to the W-debugger wave (§6).

### 3.6 Meta-circularity and reentrancy

Strongtalk's debugger is Smalltalk code in the same image; MACVM's endgame
(Phase W + Phase E) is the same. Until Processes exist, the CLI loop is Rust,
but everything it does goes through the same R5 surface the Smalltalk
`ActivationMirror` library will use — the Rust loop is scaffolding around the
permanent primitives, not a parallel implementation to throw away.

Reentrancy rule v1: while `session_depth > 0`, breakpoint hits and `halt`
are **ignored** (the checks in §2 test `session_depth == 0`). Doits evaluated
*by* the debugger therefore can't recursively open debuggers on themselves.
When real Processes land, this becomes per-process, and "debug the debugger"
becomes possible the Smalltalk way.

---

## 4. PROBE — the compiled-code debugger

PROBE answers one question HALT structurally cannot: **"what did the compiler
actually emit, and what is the machine actually doing?"** Its audience is
whoever is chasing the next BUG D — where the corrupting write and the
crashing read are far apart, and the deopt/materialization machinery is
itself among the suspects.

### 4.1 Triggers

| Trigger | Path today | PROBE change |
|---|---|---|
| `brk #0xDE02` (compiled-code assert) | SIGTRAP handler → `resignal_fatal` → process dies (`deopt_trap.rs:362`) | → enter PROBE instead of dying |
| SIGSEGV/SIGBUS with pc in a registered code-cache range | **no handler — raw crash** | new handlers, same trampoline-escape pattern as SIGTRAP (`deopt_trap.rs:293`: rewrite `__pc` to a probe trampoline, all real work after sigreturn) |
| `brk #0xDE10..0xDE1F` — **new: low-level breakpoint namespace** | (unallocated) | → enter PROBE interactively |
| `probe` CLI command / `MACVM_PROBE=break:<sel>[:bci]` | — | plant a low-level breakpoint (§4.3) |
| SIGSEGV/SIGBUS with pc *outside* any code cache | raw crash | unchanged — a Rust bug is rustc's/lldb's jurisdiction, not PROBE's. PROBE still prints the one-line "pc not in any registered code cache" verdict before re-raising, because *that classification itself* is the first question of every crash triage |

The signal-side additions follow `deopt_trap.rs`'s discipline exactly: the
handler reads only the pre-registered atomic registry (`deopt_trap.rs:100-134`),
never allocates or locks, and escapes to Rust via a ucontext `__pc` rewrite.
The captured `ucontext_t` (all GPRs, sp, fp, lr, faulting pc, and for
SEGV/BUS the fault address) is copied to a static buffer before sigreturn —
it *is* the register view PROBE displays.

### 4.2 The frozen-frame report (post-mortem mode)

On entry from a fatal trigger, PROBE emits a **crash dossier** before
anything else — so even a non-interactive CI run leaves the full evidence
(this is the "black box recorder"; BUG D's dossier was assembled by hand over
days, this automates hour one):

1. **Verdict line** — pc classified: which nmethod (`CodeTable::find_by_pc`,
   `nmethod.rs:409`), or PIC/stub/adapter (their emitters tag ranges), or
   foreign.
2. **Provenance chain** — pc → code offset → `PcDesc::find_in`
   (`scopes.rs:493`) → scope chain → `Holder>>selector @bci`, one line per
   inlined virtual frame. *"You are 3 instructions past the safepoint for
   `Point>>+` bci 7, inlined into `Rect>>area` bci 12, nmethod #23 v2."*
3. **Registers** — from the captured ucontext, each annotated: oop-looking?
   (tag bits) heap-plausible? (the stack-write auditor's mark-word check,
   `stack.rs`) in-heap / in-code-cache / on-stack classification per value.
4. **Compiler's claim vs reality** — decode the nearest safepoint's
   `SafepointState`: for every recorded `ValueLoc` (`scopes.rs:267`) print
   *where the compiler says* each receiver/temp/stack value lives and *what
   is actually there*, with the same plausibility annotation. The oopmap
   (`nmethod.rs:267`) gets the same treatment for GC-slot claims. One
   mismatched line here is a BUG-D-class root cause caught red-handed.
5. **Disassembly window** — ±16 instructions around pc, faulting instruction
   marked (§4.4).
6. **Walkback** — `walk_frames` output *if it survives*: the walker panics on
   corrupt tier links by design (`frames.rs:187`), so PROBE runs it under
   `catch_unwind` and reports "walk died at step N: <panic msg>" as a
   finding, not a failure. A corrupt walk *is* evidence.
7. **Heap spot-checks** — `verify.rs` heap verify under `catch_unwind`, same
   posture.
8. Machine-readable copy (`MACVM_PROBE_DUMP=<path>`, JSON) for regression
   harnesses; human copy to stderr.

Then: if stdin is a tty and `MACVM_PROBE=interactive`, drop into the
`(probe) ` REPL — commands: `regs`, `dis [addr|sym]`, `x/<n>g <addr>`
(bounds-checked raw memory read), `frame`, `scopes`, `oopmap`, `walk`,
`verify`, `bt`, `resume` (only for non-fatal entries), `kill`. Otherwise
exit(70) after the dossier — CI stays non-hanging by default.

### 4.3 Low-level breakpoints (instruction patching)

The `0xDE10..0xDE1F` immediates are PROBE's planted breakpoints: overwrite
one instruction word in an nmethod with `brk #0xDE1n` (the encoder is
`deopt_trap.rs:94` `emit_brk`), remembering `(addr, original_word)` in
`ProbeConfig`. Patching uses the same W^X write path `flush.rs`'s §2b entry
patching already uses (this exact patch — one word, icache flush — is proven
machinery).

Placement is by provenance, not raw address: `probe break Point>>+ @bci 7`
resolves selector → nmethod(s) → `PcDesc` for that bci → code offset. Hitting
one enters the interactive REPL with the full §4.2 context.

**Continuing past** a planted breakpoint: restore the original word, flush,
single-instruction re-arm is *not* attempted in v1 (no hardware single-step
without ptrace/Mach thread-state gymnastics). v1 semantics: a planted
breakpoint is **one-shot** (auto-removed on hit, `rearm` command re-plants it
manually). This is honest and sufficient for the "get me to the Nth entry of
this nmethod with registers visible" use case; loop-iteration-N debugging
composes it with a conditional (`break ... if x0==<val>`, checked in the
probe handler against the captured regs — re-plant + resume until the
condition holds, all inside PROBE).

**Watchpoints are explicitly out of scope** — no debug-register access from
userspace on macOS without a Mach exception server; revisit only if a real
investigation demands it (§6). The stack-write auditor already covers the
highest-value watch (oop plausibility at the write) at the choke point.

### 4.4 Machine-code disassembler

PROBE needs `dis`. Options considered:

1. **Vendor Capstone** (C) — heavy, new build dependency; the QBEJIT
   evaluation explicitly dropped it for the same reason.
2. **Hand-rolled decoder for the emitted subset** — the compiler emits a
   *closed* instruction vocabulary (every word comes from
   `compiler/assembler.rs` / `vendor/wfasm/a64/encode.rs`'s emit functions).
   Write the inverse of exactly that set (~100 patterns), in
   `src/compiler/disasm_a64.rs`, with a `.word 0x????????` fallback for
   anything unrecognized.

**Decision: (2).** The encoder is ours, closed, and unit-tested; its inverse
is a weekend of table-driven code, keeps the zero-C-deps property, and — the
real argument — a *fallback line in our own disassembler is itself a
finding* ("the compiler emitted a word its own vocabulary can't name" ≈ the
QBEJIT trap-stub bug, found exactly this way). Round-trip tests
(`encode(dis(w)) == w` over the emitters' corpus — `wfasm`'s `corpus_replay`
infrastructure is sitting right there) make it trustworthy fast.

### 4.5 What PROBE deliberately does not do

- **No deopt, ever.** `resume` from a planted breakpoint continues the
  compiled frame untouched. If the *user* wants the interpreter view of the
  same halt point, that's HALT's job (plant a high-level breakpoint and
  re-run) — the two-level split is precisely so PROBE never has to.
- **No allocation before the dossier's step 6.** Steps 1–5 read only
  pre-registered/static state, so a corrupted-heap crash still yields a
  useful report.
- **No Smalltalk evaluation.** A doit runs the interpreter runs the heap —
  everything PROBE must not trust.

---

## 5. Environment & CLI surface (additions)

| Surface | Meaning |
|---|---|
| `MACVM_PROBE` | `report` (default when a fatal trigger fires: dossier, exit 70) / `interactive` / `break:<Class>>><sel>[:bci][,…]` |
| `MACVM_PROBE_DUMP` | path for the JSON dossier |
| `MACVM_DEBUG` | `1` — set `debug.active` at boot (halt-on-error behavior, §3.1) |
| `macvm debug <file.mst>` | run with `debug.active`, CLI HALT loop on halt |
| `MACVM_TRACE=debug` | one line per breakpoint set/hit/step — the channel system is open-ended by design; this is just a new name |
| `brk` immediates | `0xDE10–0xDE1F` reserved for PROBE planted breakpoints (extends the tracing_debugging.md table) |

---

## 6. Build plan and deferrals

Ordered by value-per-effort, honestly small steps — each lands testable:

- **DBG0 — PROBE dossier** (no interactivity): SIGSEGV/SIGBUS handlers +
  crash report steps 1–4 + 6–8. *Highest value first: this is the tool BUG D
  needed.* No disassembler yet (step 5 says "disasm not built").
  Gate: a `--selftest-probe-*` flag family (crash a planted `brk 0xDE02`,
  assert exact dossier shape) — the existing selftest pattern.
- **DBG1 — HALT core**: `DebugState`, dispatch hook, `set_breakpoint`
  pinning (§2.1), CLI loop, backtrace/inspection over interpreter frames,
  stepping. Gate: scripted debugger session goldens (commands in, transcript
  out — same golden discipline as disasm).
- **DBG2 — compiled-frame inspection**: read-only scope-chain
  ActivationMirrors for `FrameView::Compiled` (§3.3), R5 primitive layer,
  bci↔source map. Gate: breakpoint in caller, inspect hot compiled callee
  frames mid-stack, values match `MACVM_JIT=off` run byte-for-byte.
- **DBG3 — PROBE interactive**: a64 disassembler + REPL + planted
  breakpoints. Gate: corpus round-trip + scripted probe session.
- **DBG4 — W-debugger** (Phase W/E alignment): Smalltalk `ActivationMirror`
  library + StackTraceInspector over the same R5 primitives; GUI halt events
  over the GuiHost channel. This is SPRINTS.md:293's existing row, now with
  its VM floor fully specified.

**Deferred, with the reason on record:**
- *Synchronous mid-stack deopt* (mutate a live compiled frame's temps) —
  needs new frame-surgery machinery; read-only + convert-on-return covers
  the debugging need (§3.3).
- *Step-into compiled callees* — vanishes under breakpoint pinning; not
  worth a per-call debug check in compiled code (§3.4).
- *Hardware watchpoints / single-step* — Mach exception server territory
  (§4.3); the auditor choke point covers the known cases.
- *Fix-and-continue* — shape known (R3 + install + restart), belongs to the
  W-debugger wave (§3.5).
- *Remote debug protocol* (DAP etc.) — the GuiHost seam is the natural
  transport when wanted; nothing in this design blocks it.

---

## 7. Open questions

1. **Halt inside a primitive** — primitives are atomic to HALT (§3.4); is
   that acceptable for long-running primitives (future FFI calls)? Likely
   yes for v1; revisit with FFI.
2. **Breakpoints in blocks** — a CompiledBlock is its own MethodOop
   (`is_block`, method.rs:57), so (method, bci) keys work unchanged; the UI
   question (how a user names "the block on line 3") waits for the
   bci↔source map to answer it.
3. **`debug.active` and the differential gates** — the off-vs-compiled
   byte-identical gate must also hold under `MACVM_DEBUG=1` with no
   breakpoints set (debug mode must be a pure observer until a breakpoint
   exists). Add that third leg to the gate matrix when DBG1 lands.
4. **Dossier stability** — the JSON dossier will be asserted by tests;
   pin its schema version field from day one so goldens survive additions.
