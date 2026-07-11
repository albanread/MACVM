# RUSTTCL — MACVM's live VM-introspection shell

A debugging tool, not a Smalltalk-facing feature and not part of the
SPEC-numbered sprint sequence — same posture as `docs/ASM.md`'s
`asm_preview` or the class browser in `image_store/`: a standalone,
non-disruptive side track that helps investigate the compiler/GC/runtime
from outside, built on a real Tcl implementation reused wholesale rather
than hand-rolled.

## 1. What it is

`macvm rusttcl` starts an interactive shell (`rusttcl> ` prompt) — or,
given a script path, runs one non-interactively — that can inspect and
poke a **live, running `VmState`**: disassemble compiled methods, walk a
class's method dictionary, dump the JIT code-cache table, read an inline
cache's state and resolved klass(es), print the full stats counter block
on demand, toggle `MACVM_TRACE` channels and a handful of other
operational flags without restarting the process, and load `.mst` files
into the same session. It complements — doesn't replace — the existing
`MACVM_TRACE=<flags>`/`MACVM_DBG_*` env-var channels (`tracing_debugging.md`),
by making the same kind of information interactively queryable instead of
fixed at process start.

Because it's a real Tcl (`set`/`if`/`while`/`foreach`/`proc`/`upvar`/
`uplevel`/`list`/`dict`/`expr`, plus every verb below), a diagnostic
session is a *script*, not just a sequence of one-shot commands — e.g.
looping `disasm` over every selector a class defines, or scripting a
repro that loads a file, provokes a condition, and dumps the exact state
that matters, replayable with one shell invocation.

## 2. Invocation

```
macvm rusttcl [--world <dir>]              # interactive rusttcl> prompt
macvm rusttcl [--world <dir>] <script.tcl> # run a script non-interactively
```

Same `--world` convention as `run`/`repl` (`world/` if omitted). The VM
itself boots exactly like `macvm run`/`macvm repl` — `MACVM_JIT`,
`MACVM_TRACE`, `MACVM_GC_STRESS`, `MACVM_HEAP`, etc. all apply from the
ambient environment, so e.g. `MACVM_JIT=threshold=1 macvm rusttcl` starts
a shell already compiling aggressively.

## 3. Verb reference

| Verb | Usage | What it does |
|---|---|---|
| `disasm` | `disasm <Class> <selector>` | Disassemble one method's bytecode (`bytecode::disassemble`, the same output `MACVM_TRACE=bytecode`'s golden tests use). |
| `methods` | `methods <Class>` | List a class's own method dictionary: selector, argc, ntemps, primitive number, sorted. |
| `nmethods` | `nmethods` | Dump the whole JIT code-cache table: id, state (Alive/NotEntrant/Zombie), version, key klass>>selector, trap count, frame slots, code size. |
| `ic` | `ic <Class> <selector>` | Dump every send site's inline-cache state in one method: bci, selector, Empty/Mono/Poly(n)/Mega, and the resolved klass(es) — Mono shows one, Poly shows all `n`. |
| `stats` | `stats` | The full `__vmStats`-equivalent counter dump, on demand (same counters `MACVM_TRACE=stats` prints at exit — `runtime::vm_state::format_vm_stats` is the single shared formatter both use). |
| `trace` | `trace [channel] [on\|off]` | No args: every currently-enabled `MACVM_TRACE` channel. One arg: is this one on? Two args: flip it live. |
| `flag` | `flag [name] [value]` | Same shape as `trace`, for VM options that aren't trace channels: `jit` (`off`\|`threshold=N`), `gc_stress` (`0`\|`1`\|`full`\|`full:N`), `deopt_stress` (`0`\|`N`), `dbg_oop` (`0x<hex>`\|`off`) — identical grammar to each one's `MACVM_*` env var. Deliberately excludes `heap_mib`/`eden_kb`: those size the heap once at construction, so setting them after the fact wouldn't resize anything already allocated. |
| `load` | `load <file.mst>` | Compile and run a `.mst` file into the current session — its classes become visible to every verb above afterward. |
| `help` | `help [verb]` | List every verb, or one verb's full text. |
| `quit` / `exit` | `quit` | End the session. |

Plus every core Tcl verb from the vendored language itself: `set`,
`append`, `puts`, `incr` (a MACVM addition — see §4), `expr`, `if`/
`elseif`/`else`/`then`, `while`, `foreach` (with `break`/`continue`),
`proc` (with default arguments), `upvar`, `uplevel`, `list`/`llength`/
`lindex`/`lrange`/`lappend`, `dict` (`create`/`set`/`get`/`exists`/
`keys`/`values`), `add`/`sub`/`mul`/`div`/`eq`, `error`. **Not** present:
`for` (only `while`/`foreach` loop) — see §4.

## 4. Architecture

**Vendored, not a dependency.** `src/vendor/rust_tcl/` is a full copy of
the `rust-tcl` sister-repo crate (`~/claudeprojects/locus/rust-tcl` —
lexer → parser → sema → CFG → bytecode → VM, extensible via
`registry::Registry::register`), compiled as part of the `macvm` crate
itself rather than a Cargo dependency — the same choice S9 made for
JASM's `wfasm` encoder. See `src/vendor/rust_tcl/VENDOR.md` for the exact
source commit, the one mechanical `crate::` path rewrite every file
needed, and the two small deliberate additions (`incr`, below) that make
this copy diverge from upstream on purpose.

**`src/rusttcl/`** is the MACVM-side integration:
- `mod.rs` — `RusttclCtx` (owns the live `VmState`), the REPL/script-file
  drivers, and the `resolve_klass`/`resolve_method` helpers every
  class/selector-taking verb shares.
- `verbs.rs` — every verb in §3 above, plus the `TABLE` that answers
  `help` (the vendored `Registry` has no public way to enumerate its own
  contents by design, so this file keeps its own parallel record).
- `bridge.rs` — the part worth understanding before touching any of this:
  `Registry::register`'s handler type demands `Fn(..) + Send + Sync +
  'static`, which rules out capturing a lifetime-bound `&mut VmState` in
  a verb closure. The bridge stores `&mut RusttclCtx` as a raw pointer in
  a thread-local for the exact duration of one Tcl `Vm::run` call
  (`with_ctx_active`), and every verb closure's first line
  (`bridge::active_ctx()`) reconstitutes it. Sound because RUSTTCL is
  single-threaded and non-reentrant — the same raw-pointer round-trip
  `codecache::stubs`'s `unsafe extern "C" fn foo(vm: *mut VmState, ..)`
  runtime stubs already rely on for compiled code calling back into the
  VM, just via a thread-local instead of a register. `#![allow(unsafe_code)]`
  at this module only (the crate-wide `#![deny(unsafe_code)]`'s
  module-scoped opt-back-in, same pattern as `oops`/`memory`/`codecache`/
  `vendor`).

**One persistent `Vm` per session**, not upstream's own `eval()`
convenience (which builds a fresh one per call) — so `set` variables and
`proc` definitions survive across separate input lines, matching a real
`tclsh` session. This exposes one thing upstream's `Vm` doesn't handle
for a multi-call session: its `output` buffer accumulates for the whole
session's lifetime and is never cleared between `run()` calls. `mod.rs`'s
`run_chunk_with_registry` tracks how much of that buffer it already
printed and shows only the new suffix each time — otherwise every later
command would re-print the entire session's `puts` history.

No distinguishable "need more input" signal exists in the vendored
lexer's own errors (an unterminated brace/bracket/quote is a hard `Lex`
error, not an incomplete-input marker), unlike the Smalltalk parser's own
`eof` flag (`main.rs::cmd_repl`). So multi-line input (a `proc`/`if`/
`while` block typed across several physical lines) is gated by a small
brace/bracket-depth counter (`brace_depth`) checked *before* ever calling
`compile` — deliberately simple, not real Tcl's full backslash/quote
lexing; an exotic edge case just falls through to a reported error and a
cleared buffer, not a shell crash.

## 5. Deliberate deviations from upstream `rust-tcl`

Two small additions to `src/vendor/rust_tcl/verbs.rs`, not present in the
`locus` source (documented in both `VENDOR.md` and this file's own
registration-site comments, so a future re-vendor pass doesn't silently
drop them):

- **`incr varName ?increment?`** — real Tcl has this; the vendored crate
  didn't. Missing it turned "loop N times counting up" (the single most
  common thing a diagnostic script does) into a workaround
  (`set i [expr {$i + 1}]`) every time, so it was worth the small,
  well-isolated addition rather than living with the papercut.
- **`for`** was considered and deliberately NOT added: unlike `incr` (an
  ordinary verb), `for` is real control flow needing dedicated compiler
  support (`codegen.rs`'s `while`/`foreach` are hard-coded special cases,
  not verbs) — a bigger, riskier change for a construct `while` already
  covers.

## 6. A worked example

Loading a benchmark and inspecting the inline-cache state at a specific
send site — exactly the kind of query that motivated this tool (real
transcript, `macvm rusttcl --world world`):

```
rusttcl> load world/bench/richards.mst
Richards 1 204
loaded world/bench/richards.mst
rusttcl> ic TaskControlBlock isTaskHoldingOrWaiting
@7 ic=0 super=false sel=not state=Poly(2) #False #True
rusttcl> flag jit threshold=1
rusttcl> trace deopt on
rusttcl> nmethods
(no nmethods compiled yet)
rusttcl> quit
```

`nmethods` correctly reports nothing compiled yet: `flag jit` only takes
effect for sends that happen *after* it's set, and `load` already ran the
whole benchmark under the (default) interpreter before this point — it
doesn't retroactively compile anything. Setting `MACVM_JIT=threshold=1`
before booting the shell, so `load` itself compiles aggressively, is the
more useful order for a real investigation — but see the caveat below
first.

**Caveat**: `load` (like `macvm run`) has no protection against a runtime
error with no `#doesNotUnderstand:`/handler installed reaching MACVM's
`error:` primitive, which hard-exits the whole process (`-> !`, by
design — DBG0-5 exist, but the debugger is not armed on the `load`
path). Loading a file that hits one takes the *entire RUSTTCL session*
down with it, not just that command: an unhandled error reaching
`error:` with no handler hard-exits the shell. If a `load`
needs to survive whatever the file does, run it once via plain `macvm
run` first to characterize the risk.
