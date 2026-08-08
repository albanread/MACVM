# Exceptions — a design review, and why they are cheaper here than they look

**Status: design review, nothing built.** Written 2026-08-07 after repeated
requests for exceptions, to answer three questions honestly: *what does MACVM
actually lack, what would it cost, and is the current position defensible?*

## 1. The claim under review

`README.md` says: *"MACVM has no exception system (`self error:` stops the
computation outright, and scoped `catch` was deliberately rejected)."*

The second half of that sentence is doing more work than it should. The
rejection of scoped `catch` was a real decision; but the sentence reads as
"MACVM has no unwinding machinery", and **that is not true.** The hard half
of an exception system is already built, shipped, and hardened.

What exists today, in `src/interpreter/unwind.rs`:

| Piece | Where | What it does |
|---|---|---|
| Frame markers | `frames.rs:247` `FRAME_MARKER` | **One slot per frame**, already in the layout, holding an armed closure or token |
| `MarkerClass` | `unwind.rs:25` | `None` / `Handler(ClosureOop)` / `Token(ArrayOop)` |
| `continue_unwind` | `unwind.rs:187` | Walks frames toward a target, **running marked handlers innermost-first**, re-validating liveness at every step |
| `UnwindStep` | `unwind.rs:150` | `RanHandler` / `ReturnedFromHome` / `CannotReturn` / **`Escaped`** — the last being unwinding *across compiled frames* |
| `run_curtailment_blocks_on_error` | `unwind.rs:424` | On error: collect armed handlers up the stack, run them, with a re-entrancy guard |
| `ensure:` / `ifCurtailed:` | prims 60/61 | Already work, already excluded from JIT shimming (`PRIM_ACTIVATES_FRAME`) |

Read that table again with exceptions in mind. *Mark a frame; signal; walk
outward innermost-first; run the marked thing; unwind across mixed
interpreter/compiled frames; survive GC during the walk.* That is an
exception mechanism with the selection step missing.

**So the honest position is not "MACVM has no exceptions" but "MACVM's
unwinder has no conditional handlers and no way to answer a value."**

## 2. Why people keep asking (and why workers do not answer it)

The supervisor/worker story answers *process*-level failure: a worker dies,
`#workerDied` arrives, a policy restarts it. That is genuinely good, and it is
the right answer for a crashed VM.

It is not an answer for **expression-level** failure, which is what library
code needs:

```smalltalk
"Today: this kills the whole computation. There is no other option."
value := dict at: missingKey.

"What every Smalltalker expects to be able to write:"
value := [ dict at: missingKey ] on: KeyNotFound do: [ :e | defaultValue ].
```

Every collection lookup, every parse, every conversion, every I/O call in the
library has exactly one failure verb — `self error:` — and it is terminal.
That means **a library cannot report a recoverable condition to its caller**
except by inventing an out-of-band convention (`ifAbsent:` blocks, nil
returns, sentinel values). MACVM's own world is full of those conventions
precisely because the alternative does not exist. `ifAbsent:` is an
exception system with one handler, hand-threaded through every call site.

That is the real cost of the status quo, and it is paid by every library
author, forever. It is a different and larger cost than "a crashed worker
needs restarting".

## 3. The design: `on:do:` is `ensure:`'s sibling, not a new mechanism

**The core claim: adding exceptions is mostly adding a *predicate* to a walk
that already happens.**

`ensure:` marks its frame and the unwinder runs the mark unconditionally.
`on:do:` marks its frame with a *pair* — (exception class, handler block) —
and the unwinder runs the mark **only if the signalled exception is a kind of
that class**. The walk, the liveness re-validation, the compiled-frame
`Escaped` path, the GC discipline, the re-entrancy guard: all reused, none
rewritten.

### 3.1 Cost, which is the part the request asked about

**Zero added cost when nothing signals.** Entering `on:do:` is: one store
into `FRAME_MARKER` (a slot that already exists and is already scanned), then
call the protected block. No side table, no `setjmp`, no per-call-site unwind
metadata, no landing pads. It is *exactly* as cheap as `ensure:` is today,
which the world already uses freely.

That is worth contrasting with the two mainstream alternatives:

| Approach | Non-throwing cost | Throwing cost | Implementation cost here |
|---|---|---|---|
| Table-driven / zero-cost (LLVM, JVM) | zero | expensive (unwind tables, personality routines) | **high** — needs per-call-site metadata through the JIT, deopt, and OSR |
| `setjmp` per protected scope (C-style) | a register save per `on:do:` | cheap | low, but pays on the common path |
| **Frame marker (proposed)** | **one store** | O(depth to handler) | **low — the walk already exists** |

The frame-marker approach is not the theoretically fastest, but it is within
noise of zero-cost on entry, and it is the only one whose implementation is
*already 80% written*. On a VM whose whole design record says "prefer the
structural change you can gate", that is the right trade.

### 3.2 One-pass or two? The decision that shapes everything

This is the only genuinely hard design question, and it is worth stating
precisely because it forecloses or preserves `resume:`.

- **One-pass**: unwind as you search — pop frames, running `ensure:` blocks,
  until a matching handler is found. Simple, and it is what
  `run_curtailment_blocks_on_error` does today. **But the signalling context
  is gone by the time the handler runs**, so `resume:` (continue from the
  signal point with a value) is impossible forever.
- **Two-pass** (SEH, Ada, ANSI Smalltalk): *pass 1* walks outward reading
  markers, **without unwinding**, to find the handler; *pass 2* then unwinds
  to it, running `ensure:` blocks in between, and activates the handler.

**Recommendation: two-pass.** It costs nothing extra when no handler matches
(pass 1 finds nothing, and the fallback in §3.4 fires exactly as today), and
it is the difference between "we have exceptions" and "we have exceptions and
can never add `resume:`". Given this project's stated preference for designs
that do not need re-doing, paying one extra walk on the *signalling* path to
keep resumption open is clearly right.

### 3.3 The staged shape

- **E0 — the hierarchy, in the world.** `Exception` → `Error` → `ZeroDivide`,
  `MessageNotUnderstood`, `KeyNotFound`, `IndexOutOfBounds`; `Warning` as a
  resumable sibling. Plus `signal`, `signal:`, `messageText`, `description`,
  `selector`/`receiver` on DNU. Pure Smalltalk, no VM change, no behaviour
  change — the classes simply exist.
- **E1 — `on:do:` + `signal`, non-resumable.** The marker gains a class
  predicate; `signal` does pass 1 (search) then pass 2 (unwind + activate).
  Handler value becomes `on:do:`'s value. `on:do:` joins
  `PRIM_ACTIVATES_FRAME`. This is the slice that makes the library writable.
- **E2 — handler verbs. BUILT.** `return:`/`return`, `retry`, `pass`,
  `outer`, and the instance-side `signal:` E0 specified and never got.

  Building it moved one thing. E1 pushed the handler WRAPPED as
  `[ :ex | ^aHandlerBlock value: ex ]`, and that `^` — returning the
  handler's value from `on:do:` — was the default action, hard-coded into
  the wrapper and therefore unreachable from inside the handler. E2 pushes
  the handler RAW and makes the default explicit as `signal`'s closing
  `^self return: result`. Once the default is a verb, the other verbs are
  the same shape: each is one non-local return through a block created in
  the protected activation, which the stack entry now carries alongside the
  handler. No unwinder change, no new primitive — as E1 needed none.

  `retry` is a LOOP, not recursion: `on:do:` runs `protect:handler:` in a
  `whileTrue:` and re-runs it when that answers a private token, so an
  unbounded retry (a reconnect loop is the motivating case) costs no stack.
  Gated at 5000 retries, which recursion would not survive.

  `pass` needs no new search: `signal` already truncates the stack past the
  handler it chose before running it, so re-signalling the same exception
  searches strictly outward and cannot re-enter its own handler.

  The escape routes live on the EXCEPTION, not on the class, so two
  exceptions in flight cannot clobber each other's — and `pass`, which
  re-signals the same exception outward, captures its own route first
  because signalling rebinds it to whichever handler catches next.

  One placement constraint worth recording: the verbs name `Error` (for the
  outside-a-handler guards) and the loader resolves globals at compile time,
  so they live in an Exception block RE-OPENED after the subclasses — the
  same reason the file re-opens `BlockClosure` at the end rather than
  declaring it early.
- **E3 — `resume:`.** Only reachable because E1 chose two-pass: pass 1 has not
  unwound, so the signalling frame is still live and can be continued with the
  handler's value. Restricted to exceptions that declare themselves resumable
  (`Warning`, not `Error`) — the ANSI rule, and it keeps the dangerous case
  opt-in.
- **E4 — the VM's own errors become signals.** `prim_error`, `dnu_fallback`,
  division by zero, index errors: each signals its class **and, if no handler
  matches, does exactly what it does today** — print, PROBE dossier,
  `run_curtailment_blocks_on_error`, `raise_guest_fatal`.

That last clause is the compatibility hinge and deserves emphasis: **E4 is
purely additive.** A world with no `on:do:` anywhere behaves byte-identically
to today, which means the differential gate (`MACVM_JIT=off` vs
`threshold=1`, 6200 world tests) stays valid throughout the arc, and the
existing debugger/halt-on-error behaviour is untouched.

### 3.4 The interactions that will actually bite

Recorded now, because each is a place where a naive implementation breaks
something that currently works:

1. **Compiled frames.** `Escaped` exists precisely because a home frame can
   be on the far side of compiled code. `on:do:` must inherit that path, not
   invent one. The `PRIM_ACTIVATES_FRAME` exclusion is the precedent and the
   protection.
2. **The debugger.** `halt_on_error` currently parks the primary at a raise.
   With handlers, "error" and "unhandled error" stop being the same event —
   the halt must fire on *unhandled* only, or every `on:do:`-protected library
   call would open the debugger. (This exact confusion is what made a scripted
   `evaluate "1/0"` hang for a whole session — see
   `applescript_design.md`.)
3. **`ensure:` ordering — and this note was WRONG.** It claimed `ensure:`
   blocks must run *before* the handler activates. ANSI is the opposite, and
   for a reason that matters: the handler runs while the signalling stack is
   still live (which is what makes resumption possible at all), so the
   `ensure:` blocks between run when the handler *returns*. The order is
   **body, handler, ensure** — now pinned by
   `testEnsureRunsAfterHandler`.
4. **GC during the walk.** `run_curtailment_blocks_on_error` already models
   the discipline — collect into a `HandleScope` first, run second, because
   running a block re-enters the interpreter and moves closures.
5. **Re-entrancy.** A handler that itself signals must not re-enter its own
   frame's marker. The existing `curtailing_on_error` guard is the shape;
   markers must be cleared or shadowed before activation.
6. **Worker boundaries.** An exception must not cross a `Worker` deep-copy
   boundary as an object graph. It signals *within* a VM; a worker that dies
   still reports `#workerDied`. The two mechanisms compose, they do not merge.

## 3.5 E0+E1 as built (2026-08-07) — and the VM change was not needed

Both slices shipped in **one world file** (`world/78_exceptions.mst`), with
**no VM change at all**: no new primitive, no new marker kind, no
`PRIM_ACTIVATES_FRAME` entry, no unwinder edit. §3's frame-marker predicate
turned out to be unnecessary for the non-resumable case, because the two
pieces the job needs already exist and compose:

- the **handler stack can live in the world** (`Exception class >> handlers`),
  so `signal` finds its handler by *searching*, never by touching frames; and
- **non-local return already unwinds correctly** from arbitrary depth to a
  specific home frame, running every `ensure:`/`ifCurtailed:` in between.

So `on:do:` pushes `[ :ex | ^aHandlerBlock value: ex ]` — a block whose *home
is the `on:do:` activation* — and `signal` calls it. The `^` does the
returning, and the existing unwinder does the rest.

**This is still two-pass, which is the point.** The search does not unwind:
`signal` locates the handler and calls it with the stack intact. A handler
that answers *without* a `^` simply returns its value to `signal` — so E3
(`resume:`) remains reachable without redesigning any of this, exactly as
§3.2 required.

Gated by `world/tests/50_exception_tests.mst` (8 cases): matching, subclass
matching, the no-signal path, non-matching-inner-passes-outward,
re-signalling from inside a handler, ANSI `ensure:` ordering, message text,
and — the one most likely to rot silently — that the handler stack is left
empty on *every* exit path. Suite 6200 → **6215, 0 failed**, differential
byte-identical off-vs-JIT, GC-stress green, and verified hot (33,000
JIT-compiled `on:do:` activations, no handler-stack leak).

One bug worth keeping: `unhandled` first read `self class name , ': '`, and
class names are **Symbols**, which are immutable — so the error *reporter*
raised "symbols are immutable" instead of reporting. `asString` first. A
reporting path that fails while reporting is the same family of bug as §6.3
in the scripting arc.

**Still not done (E4):** the VM's own failures do not signal yet. `1/0` and
DNU terminate exactly as before, so this file is purely additive — which is
what keeps the differential gate meaningful. `on: ZeroDivide do: […]` today
catches a ZeroDivide someone *signals*, not one the VM raises.

## 4. Is the current position defensible?

Partly, and it is worth separating the two halves that the README sentence
runs together.

**Defensible:** rejecting a *scoped `catch`* bolted onto a VM whose unwinder
was not ready, and preferring share-nothing worker supervision for
*process*-level failure. Both are consistent with the project's record.

**Not defensible any more:** the implication that exceptions would be a large
new mechanism. They would be a predicate, a second walk, and a class
hierarchy, on top of an unwinder that already survives moving GC, mixed
tiers, and non-local return. The machinery was built for `^` out of a block;
it turns out that is the same machinery.

**The strongest argument for building them** is not developer comfort. It is
that **the library cannot express recoverable failure at all today**, and
every `ifAbsent:`-style workaround in the world is a hand-rolled, one-handler
exception system paid for at every call site. E1 alone — non-resumable
`on:do:` — removes that tax permanently, and it is the smallest slice that
does.

**The strongest argument against** is scope discipline: this is a language
feature, not a performance fix, and the record shows this project's wins come
from structural changes it can gate. So the recommendation is E0+E1 behind
the existing differential gate, with E2–E4 judged on whether the library
actually gets simpler — measured, as usual, rather than assumed.
