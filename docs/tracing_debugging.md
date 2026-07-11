# Tracing & Debugging MACVM

MACVM doesn't just run your Smalltalk program — it also decides, moment to
moment and invisibly, which of two very different engines should run it, when
to move objects around in memory, and when a shortcut it took earlier has
stopped being safe and needs to be undone. Most of the time none of that is
observable: a program that adds two numbers together prints the same answer
whether the interpreter did the add or compiled machine code did, whether the
receiver object has moved three times during the call or not at all.

But when you're developing the VM itself — or chasing down a bug that only
reproduces "sometimes" — you need windows into that invisible layer. This
article is a tour of every such window MACVM currently has: environment
variables that turn on logging or deliberately break things, in-language
primitives your Smalltalk code can call to ask the VM about itself, the
stack-trace and disassembly machinery, and the test/gate infrastructure that
turns all of the above into repeatable, automated checks.

Everything below is grounded in what the code actually does today, not what a
design doc once proposed. Where the two disagree, this article says so.

## The environment-variable toolbox

All of these are read once, at startup, into a `VmOptions` struct held on
`VmState`. Nothing here can be toggled mid-run.

| Variable | Values | What it does |
|---|---|---|
| `MACVM_HEAP` | `<MiB>` | Total heap reservation size. Defaults to 8192 MiB. |
| `MACVM_EDEN` | `<KiB>` | Shrinks the young-generation (eden) space to the given size — mainly useful for making scavenges happen often on a small program, without needing `MACVM_GC_STRESS`. |
| `MACVM_GC_STRESS` | `1` or `full[:N]` | Forces extra garbage collection. `=1` scavenges before *every* allocation through the public allocation choke point, not just when eden is actually full. `=full[:N]` instead runs a full mark-slide-compact collection every `N`th allocation (default `N=100` if you just write `=full`). The two are mutually exclusive. |
| `MACVM_JIT` | `off` or `threshold=N` | Controls the tier-1 compiler. `off` disables compilation entirely. `threshold=N` compiles a method after `N` sends (or loop back-edges). **Unset, or any value that doesn't parse, means `off`** — deliberately: a typo'd flag must never silently turn compilation on when you meant to test the interpreter, or vice versa. |
| `MACVM_DEOPT_STRESS` | `1` | Periodically forces a live compiled method to deoptimize as if it had just been redefined, round-robining through whichever nmethods are currently alive. Exists to drive the deopt machinery hard under a real workload instead of only under hand-written unit tests. |
| `MACVM_TRACE` | comma-separated channel list | Turns on one or more trace channels — see the next section. |
| `MACVM_DBG_OOP` | `<hex address>` | Follows one specific object through garbage collection (see below). |

### `MACVM_GC_STRESS` and `MACVM_JIT` together

These two are designed to be combined. A collector that only ever gets
exercised while the JIT is off would never prove it can relocate objects
referenced from *compiled* code — registers, spill slots, inline caches. The
project's own acceptance gates run the full test suite under
`MACVM_GC_STRESS=1 MACVM_JIT=threshold=1` together specifically to catch that
class of bug (see "Gate recipes" below) — and historically, this exact
combination is what surfaced a real use-after-free in scavenged inline-cache
keys, not code review.

### `MACVM_DBG_OOP` — following one object

Most GC bugs aren't "the collector crashed," they're "some object silently
ended up pointing at garbage after a collection that appeared to succeed."
`MACVM_DBG_OOP=<addr>` names one object (by its current heap address) that the
scavenger and full collector will each print a line about, at their own entry
and exit points and at every internal phase change, showing where that object
currently lives. Both collectors also retarget the traced address themselves
whenever the object they're watching actually moves, so the trace keeps
following the *same object*, not the same (stale) address, across a
collection that relocates it.

## `MACVM_TRACE`: the channel system

`MACVM_TRACE=<a>,<b>,<c>` is parsed into a set of channel names — the parser
itself doesn't know or care what any given name means; it just tests
membership. That means any code anywhere in the VM can gate a trace line
behind a new channel name without touching the parser. As of this writing,
nine channel names actually gate something:

| Channel | Where | What it prints |
|---|---|---|
| `bytecode` | interpreter dispatch loop | One `[bc]` line per bytecode executed |
| `count` | interpreter dispatch loop, `main.rs` at exit | A running count of bytecodes executed, printed once at exit |
| `gc` | scavenger, full collector, `main.rs` at exit | One line per collection, plus a summary counter at exit |
| `jit` | tier-1 compiler driver | Compilation attempts, successes, and failures |
| `deopt` | deopt/recompile machinery | One line per deoptimization or recompilation decision |
| `dnu` | interpreter DNU path | An interpreter `doesNotUnderstand:` mini-dossier |
| `calls` | DBG5: compiled send sites | Forces ICs cold; one line per compiled send |
| `oops` | DBG5: every GC | Per-GC compiled-frame oop-map slot auditor |
| `stats` | `main.rs` at exit | The `__vmStats` counter block dumped at exit |

One more name — `ic` — appears in the project's own conventions
document as reserved for future use. It parses fine (the channel set is
open-ended by design) but nothing currently reads it. If you pass
`MACVM_TRACE=ic`, it is accepted and does precisely nothing yet.

All trace output goes to **stderr**, never stdout — deliberately, so that a
golden-file test asserting exact stdout output stays byte-identical whether
or not tracing is on.

### `bytecode` — one line per instruction

```
[bc] #diamond @0: 0x00
[bc] #diamond @1: 0x33
[bc] #diamond @4: 0x04
[bc] #diamond @6: 0x30
[bc] #diamond @11: 0x40
```

(real output from `macvm --selftest-trace-diamond`, one of the hidden
self-test flags covered later in this article). Each line is `[bc]
<selector> @<bci>: <opcode hex>` — the method currently
executing, the bytecode index, and the raw opcode byte (cross-reference
against the mnemonic table in [`docs/ISA.md`](ISA.md) to read it). This is
the closest thing MACVM has to `strace` for its own instruction stream: turn
it on, run one method, and you get an exact, ordered account of every
instruction the interpreter dispatched. It's expensive enough (one `eprintln!`
per bytecode) that you'd only reach for it on a small, isolated repro — not a
whole program run.

### `count` — how much work actually happened

```
bytecodes: 41922
```

Printed once, to stderr, when the process exits — a running tally
incremented once per dispatched bytecode for the lifetime of the run. Unlike
`bytecode`, this is cheap enough to leave on for real workloads (it's one
integer increment per instruction, unconditionally compiled in — the `if` is
just whether the exit-time line gets printed). Use it to answer "did my
change to the interpreter or the compiler actually reduce the amount of
interpretation happening?" — compare the count with a change against the
count without it.

### `gc` — collector activity, per-event and at exit

Every scavenge prints a line of this shape (example values):

```
[gc] scavenge #14: eden 512K->0K, copied 38K, promoted 2K, thr 3, 0.4ms
```

Every full collection prints a line of this shape:

```
[gc] full #2: marked 12M, live 9M, reclaimed 3M, old 14M, 6.2ms
```

And, once, at process exit, a summary counter of this shape:

```
gc: gc_under_compiled=137
```

The first two are per-collection lines showing exactly what each collection
did — how much of eden survived, how much got promoted to the old
generation, the adaptive tenuring threshold in use, and how long the pause
took. The exit-time counter answers a different, higher-level question:
across the whole run, how many of those collections happened while a
*compiled* method had a live frame on the native stack? That number needs to
be genuinely greater than zero for a test run that's meant to be exercising
the moving collector against JIT-compiled code — a run where it's zero didn't
actually test the hard case, whatever else it checked. (See "Gate recipes"
below — this exact line is asserted by name in one of the project's own CI
scripts.)

### `jit` — what the compiler decided, and why

Four line shapes (example values), one per decision the driver can make:

```
[jit] compiled #foo -> nmethod 3 (212 bytes, 6 frame slots)
[jit] ineligible, compile_disabled: #bar
[jit] not yet eligible (cold inner IC), retry next call: #baz
[jit] code cache exhausted (4096 bytes wanted), disabling JIT for the rest of this run
```

This channel is the answer to "why didn't my method get compiled?" as much
as "which methods got compiled?" — a method can be **permanently** ineligible
(some construct it uses can never be compiled, so the VM sets a
"don't-bother-again" bit) or just **not eligible yet** (an inline cache inside
it is still cold, so compiling now would bake in too little type information
— it'll be retried the next time the method is called, once that inner site
has warmed up). The code-cache-exhausted line is a hard stop: once the one
`mmap`'d region for compiled code fills up, the JIT turns itself off for the
rest of the process rather than trying to reclaim space.

### `deopt` — every deoptimization and recompilation decision

Three line shapes (example values):

```
[deopt] nm=3 pc=0x1042a0 fp=0x16f2e18f0 bci=12 reexecute=true frames=2 result=false
[deopt] recompile declined (profile unchanged) nm=3 v1
[deopt] recompiled nm=3 v1 -> nm=5 v2 (trap storm)
```

The first line shape is emitted every time a compiled frame actually gets
deoptimized back into one or more interpreter frames: which nmethod, the
faulting/return pc and frame pointer, which bytecode index execution resumes
at, whether that bytecode needs to be *re*-executed from scratch or treated
as already-completed, how many interpreter frames the compiled frame expanded
into (inlining can turn one compiled frame into several virtual ones), and
whether a pending return value was in flight at the moment of the deopt.

The other two only show up once a method has deoptimized enough times to
trip the recompilation machinery: either the VM declines to recompile because
the type feedback hasn't actually changed since the last compile (recompiling
would just reproduce the same bad guess — this specifically prevents a
"deopt, recompile, deopt again" thrash loop when the real problem can't be
fixed by recompiling at all), or it recompiles against fresher feedback and
retires the old version.

### Telling deopt *causes* apart: `DeoptStats`

Every deopt also bumps a small in-memory counter struct, `DeoptStats`,
independent of whether tracing is on. It tracks a total (`deopt_count`) and a
three-way breakdown by *why* the deopt happened (`deopt_by_reason`):

- **Trap** — an uncommon trap fired: a runtime assumption the compiler baked
  in turned out to be false for this call (e.g. a small-integer add that
  actually overflowed, or a branch that assumed its condition would always
  be a real boolean and got something else).
- **Return** — a method was redefined (or the stress harness forced the
  same effect) while one of its compiled activations was still on the stack;
  the *return address* that activation is going to jump back to gets
  redirected into deopt handling instead.
- **Poll** — the same redefinition case, but caught the other way: a
  long-running compiled loop periodically checks (at each back-edge) whether
  its own method is still the current, valid version, and bails out through
  deopt if not.

This split exists so a stress run can show not just "N deopts happened" but
*how* — which matters because Trap-triggered deopts are a signal about the
compiler's own assumptions being too optimistic for real data, while
Return/Poll-triggered ones are just the ordinary, expected cost of the
language allowing live redefinition.

### The trap namespace: reading a crash from compiled code

Uncommon traps and the deopt-stress harness are both implemented the same
low-level way: a hand-emitted AArch64 `brk` (breakpoint) instruction with a
specific 16-bit immediate, caught by a signal handler MACVM installs itself.
Three immediates are reserved:

| Immediate | Meaning |
|---|---|
| `0xDE00` | An ordinary uncommon trap — a guard failed, deoptimize normally. |
| `0xDE01` | A forced trap from `MACVM_DEOPT_STRESS` — deoptimize, but count it separately so it doesn't pollute the "real" trap-storm-detection counters. |
| `0xDE02` | A compiled-code internal assertion — this means a VM bug, not a guest-program condition. |

If you ever end up reading a raw crash log or disassembly and see a `brk`
instruction with one of these immediates, that's what it is — and `0xDE02`
specifically is the one to treat as "the compiler emitted something it
shouldn't have," worth a bug report against MACVM itself rather than against
whatever Smalltalk code triggered it.

## Errors and stack traces

Two different mechanisms print a Smalltalk-level stack trace, for two
different failure modes.

**`error:` and ordinary guest-level errors.** Sending `error:` to any object
prints the argument message plus a full stack trace to *stdout* (not
stderr — this is guest-visible output, not VM tracing) and terminates the
process. It's intentionally not catchable or resumable; it's the "this is
fatal, here's why" primitive, not a signal.

**The `doesNotUnderstand:` fallback.** If a message genuinely can't be
delivered — the selector isn't understood *and* `doesNotUnderstand:` itself
can't be found (only possible very early in bootstrap, or with a badly broken
image) — a separate, deliberately minimal fallback prints `DNU #<selector>
(receiver class <ClassName>)`, then the same stack trace format, then exits.
This path is not allowed to send any further messages or recurse, since the
thing that would normally handle "message didn't work" is exactly what's
missing.

Both print frames in the same format, top (currently executing) frame first
(this is real output from `macvm --selftest-dnu-fallback`):

```
DNU #bar (receiver class Object)
  ?>>caller @4
```

Each line is `  <Holder>>><selector> @<bci>`. The `?` here isn't a
placeholder in this article — it's what the VM itself prints when it can't
resolve a class's name to a symbol, and this particular self-test
deliberately runs against a bare `VmState::new()` with no world/library
loaded, so `caller`'s holder class genuinely has no resolvable name yet. In
an ordinary program running against a loaded image, that slot shows the real
class name instead. If a **compiled** frame is
currently active, its line prints *above* the interpreted frames, tagged
`(compiled)` — because a compiled frame doesn't push anything onto the
VM-managed interpreter stack the ordinary frame walk follows, so without this
special case a trace captured while running compiled code would silently
skip the very frame that was executing and show only its interpreted caller.
(This is also, honestly, the trace's current limit: only the single
*innermost* compiled activation is checked for — a deliberate, documented
scope cut rather than an oversight, tracked as an open question for a later
sprint.)

## In-language introspection primitives

Your Smalltalk code itself has a handful of direct hooks into VM state —
these aren't trace channels, they're ordinary message sends that happen to
answer questions about the runtime instead of about your objects.

| Selector | What it does |
|---|---|
| `printOnStdout:` | Writes the argument's raw bytes straight to the VM's stdout stream — the lowest-level "just print this" hook, underneath whatever `printString`/`displayNl` machinery is built on top of it. |
| `millisecondClock` | Milliseconds elapsed since VM startup, as a small integer — for hand-rolled timing without shelling out. |
| `gcScavenge` | Forces one young-generation collection right now. |
| `gcFull` | Forces one full mark-slide-compact collection right now. |
| `gcStats` | Answers an 8-element `Array`: scavenge count, full-GC count, eden bytes used, old-generation bytes used, old-generation bytes committed, bytes promoted (lifetime total), marked bytes (from the *last* full GC), and context-allocation count — in that fixed, pinned order. |
| `error:` | Prints the argument plus a stack trace and terminates (see above). |
| `quit` / `quit:` | Requests VM shutdown, optionally with an explicit process exit code. |

`gcStats` in particular is worth calling out: it's what lets a Smalltalk-level
test or benchmark script assert things like "this loop promoted zero bytes
across 10,000 iterations" without needing `MACVM_TRACE=gc` and stderr
scraping at all — the same numbers the trace lines print, but readable from
inside the running program.

## Reading compiled code: the disassembler

MACVM has a real bytecode disassembler with a pinned, exact output format
(deliberately pinned — golden tests break if it drifts):

```
method #jumps argc=0 ntemps=0 prim=0 flags=
  0: push_true
  1: br_false_fwd 5 -> 9
  4: push_smi 1
  6: jump_fwd 2 -> 11
  9: push_smi 2
  11: return_tos
literals:
ics: 0
```

(this is a real, pinned golden file — `tests/golden/bc_jumps.bc.expected` —
quoted verbatim, not a made-up example). Each instruction line shows its
bytecode index, mnemonic (matching the [`docs/ISA.md`](ISA.md) table
verbatim), and operands; jump/branch instructions additionally show both
their raw operand and their resolved target bci (`5 -> 9` means "operand
value 5, which resolves to bytecode index 9") so you don't have to compute
offsets by hand.

As of this writing there's no `macvm disasm <file>` CLI command — the
disassembler is invoked from Rust (directly, or via the golden-test
infrastructure: `tests/golden/<name>.bc.expected` files are exact disassembly
snapshots, checked by `tests/it_golden.rs`, and regenerated with
`UPDATE_GOLDEN=1` when a deliberate change needs new goldens). If you want to
inspect a method's bytecode today, the practical path is a small Rust unit
test or an existing golden test, not a standalone command-line tool.

## Breaking it on purpose: the `--selftest-*` flags

The `macvm` binary has four hidden flags, checked before any normal argument
parsing, that exist purely so integration tests can provoke a specific
failure mode in a fresh, isolated subprocess and observe its exact exit code
and output — conditions that would be awkward or slow to reach from inside a
shared test process:

| Flag | Provokes |
|---|---|
| `--selftest-alloc-loop` | Allocates rooted objects in a tight loop until the heap is genuinely, unrecoverably exhausted. |
| `--selftest-stack-overflow` | Pushes onto the VM's own process stack until it overflows. |
| `--selftest-trace-diamond` | Runs a small diamond-shaped control-flow method (`push_self`, a conditional branch, converging paths, return) under `MACVM_TRACE=bytecode` — used to assert an *exact* count of emitted `[bc]` lines (five, for this particular method) as a regression check on the trace mechanism itself. |
| `--selftest-dnu-fallback` | Sends a selector that's genuinely unhandled anywhere, exercising the `doesNotUnderstand:`-fallback path and its exact stdout format and exit code. |

If you're chasing a bug in one of these specific areas — heap exhaustion
behavior, stack-overflow handling, the trace line format itself, or the DNU
fallback — these flags are the fastest way to reproduce the condition
directly, without constructing a guest program that happens to trigger it.

## Gate recipes: tracing as part of CI, not just interactive debugging

The project's `justfile` wires several of the tools above into repeatable
checks, rather than leaving them as things you only run by hand:

- **`just run-world-tests`** concatenates the whole in-language test suite
  into one file and runs it through `macvm run` — the basic building block
  every other recipe below builds on.
- **The off-vs-compiled differential.** Several gates run
  `MACVM_JIT=off just run-world-tests` and `MACVM_JIT=threshold=1 just
  run-world-tests`, capture stdout from each, and `diff` them. They're
  required to be **byte-identical** — the entire point of a JIT is that it
  must never change what a program *does*, only how fast it does it. Any
  observable difference here is a compiler-correctness bug by definition.
- **`just bench-s10` / `just bench-s11`** time a benchmark under both JIT
  modes, compute the speedup ratio, and append it to `docs/PERF.md` — a
  running history of compiled-vs-interpreted performance, with a hard
  failure if the ratio drops below a 2x "something is architecturally
  wrong" tripwire and a soft warning below a 5x target.
- **`just bridge-stats-s11`** runs the combined-stress suite under
  `MACVM_TRACE=gc`, greps stderr for the exact `gc: gc_under_compiled=`
  line described above, and fails the build if the count is zero — turning
  a trace line meant for human eyes into an automated assertion that the
  hard case (moving GC under live compiled frames) actually got exercised.
- **`just soak-s08` / `just soak-s12`** run a long allocation-churn workload
  under `MACVM_TRACE=gc`, either as a short CI-friendly variant or a much
  longer sign-off variant, watching for memory growth that shouldn't be
  there under steady-state churn.

The pattern across all of these is the same: every trace line and counter
described earlier in this article is designed to be **grep-friendly** on
purpose, specifically so it can graduate from "a thing you eyeball while
debugging" into "a thing a shell script asserts on every run."

## Putting it together: diagnosing a slow, wrong, or crashing program

A rough order of operations, cheapest and least invasive first:

1. **Is it slow?** Run once with `MACVM_TRACE=count` and once with
   `MACVM_TRACE=jit` to see whether the methods you expect to be hot are
   actually getting compiled — an "ineligible" or "cold inner IC" line for
   your hot method is usually the real answer, not a generic performance
   problem.
2. **Is it wrong after running for a while (a GC suspect)?** Re-run under
   `MACVM_GC_STRESS=1` (scavenge on every allocation) first — if the bug
   gets *more frequent* or reproduces *faster*, you've confirmed a
   GC-interaction bug and can stop guessing elsewhere. If you can identify
   the specific object involved, `MACVM_DBG_OOP=<its address>` will show you
   exactly what every collection does to it.
3. **Is it wrong right after a specific call is compiled (a JIT suspect)?**
   Compare output under `MACVM_JIT=off` against `MACVM_JIT=threshold=1` —
   any difference at all is a compiler bug, full stop, per the same
   byte-identical contract the CI gates enforce. Add `MACVM_TRACE=deopt` to
   see whether the method is deoptimizing at all (if it never does, the bug
   is in the steady-state compiled code, not the bailout path).
4. **Did it crash outright?** Check whether the crash is a `brk`
   instruction and which immediate — `0xDE02` means the compiler itself
   asserted something impossible, which is a MACVM bug to report, not a
   guest-program error to work around.
5. **Need an exact, repeatable failure to write a regression test against?**
   Look at whether one of the four `--selftest-*` flags already reproduces
   your failure mode in isolation before building a custom repro from
   scratch.

None of these tools replace reading the code — but for the two-tier,
moving-GC, deoptimizing runtime MACVM actually is, they're the difference
between debugging by inspection and debugging by guesswork.
