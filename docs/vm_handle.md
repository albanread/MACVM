# `VmHandle`: embedding MACVM in another program

`VmHandle` is MACVM's one library-consumable entry point besides the CLI
(`macvm run`/`macvm repl`). If you're writing a Rust program that wants to
boot a MACVM instance, feed it Smalltalk source, and read back results —
a GUI shell, a test harness, a REPL of your own — this is what you use
instead of shelling out to the `macvm` binary. It lives in `src/embed.rs`
and is small on purpose: three methods, two error types, one trait.

The design is covered at a higher level in [`docs/SPEC.md`](SPEC.md) §16.2;
this article is the practical "how do I actually use this" guide, and covers
one thing the SPEC section doesn't dwell on: a real, load-bearing safety
contract you have to follow, or your embedding program will behave
strangely in exactly the moment it matters most (a guest program crashing).

## The one rule: give it a thread you're prepared to lose

**A `VmHandle` must be driven from a dedicated thread, and that thread can
disappear out from under you at any `eval` call.** This isn't a suggestion —
it's how MACVM makes it safe to run a real JIT compiler inside your process
without a misbehaving guest program being able to take your whole
application down.

Here's the reasoning, briefly: when a guest program hits something truly
fatal — an uncaught error, a `doesNotUnderstand:` with no handler, a stack
overflow, the heap genuinely running out of memory — the ordinary MACVM CLI
just exits the process. An embedded VM can't do that; your program is
presumably doing other things. The obvious alternative, Rust's `panic!` +
`catch_unwind`, was considered and rejected: MACVM can have live,
JIT-compiled machine code frames on the stack at the moment of failure, and
unwinding a panic through hand-assembled native code that never registered
unwind tables is not something Rust can do safely. So MACVM does the next
best thing: it terminates **only the thread that was running**, via
`libc::pthread_exit`, which throws no exception and unwinds nothing. Your
process survives; that one thread is just gone.

This has a sharp edge you need to know about:

> **Never call `.join()` or `.is_finished()` on the `JoinHandle` of a
> thread you booted a `VmHandle` on.** `pthread_exit` bypasses the normal
> thread-completion bookkeeping `JoinHandle` relies on — `join()` will
> panic, and `is_finished()` will simply never return `true`, even though
> the thread is, in every real sense, dead.

How do you tell it died, then? The pattern the VM's own tests use: have the
worker thread send a heartbeat (or a result) over an `mpsc` channel, and
detect death by that message **failing to arrive** within a reasonable
timeout — not by any thread-handle API. Drop the `JoinHandle` when you're
done with it; that part is safe, you just never call anything on it.

## Quick start

```rust
use macvm::embed::{TranscriptSink, VmHandle};
use macvm::runtime::VmOptions;
use std::path::Path;
use std::sync::mpsc;

struct StdoutSink;
impl TranscriptSink for StdoutSink {
    fn show(&mut self, text: &str) {
        print!("{text}");
    }
}

let (tx, rx) = mpsc::channel();
std::thread::spawn(move || {
    let mut vm = VmHandle::boot(VmOptions::default(), Path::new("world"))
        .expect("boot failed");
    vm.set_transcript(Box::new(StdoutSink));

    match vm.eval("3 + 4.") {
        Ok(result) => println!("=> {result}"),   // => 7
        Err(e) => eprintln!("eval failed: {e}"),
    }
    tx.send(()).ok(); // heartbeat: "still alive, done with this eval"
});

// Detect death by a MISSING heartbeat, never by handle.join()/is_finished().
if rx.recv_timeout(std::time::Duration::from_secs(5)).is_err() {
    eprintln!("the VM thread is gone — it hit something fatal");
}
```

This mirrors `embed.rs`'s own test suite almost exactly (see
`eval_arithmetic_returns_printstring` and
`eval_fatal_condition_kills_only_the_worker_thread` for the real versions
this is adapted from) — it's a genuinely representative shape, not a
simplified toy.

## The three methods

### `VmHandle::boot(opts, world_dir) -> Result<VmHandle, VmError>`

Boots a fresh VM on the calling thread: arms the thread-death safety model
above, runs genesis, and loads `world_dir/world.list` — the same base image
`macvm run --world <dir>` loads. Two things worth knowing:

- **A missing `world.list` is not an error** — you get a working VM with
  just the built-in genesis classes, nothing more. This matches
  `load_world_with_warning`'s own behavior for the CLI.
- **A `world.list` that references a file that fails to load** *is* a real
  `Err(VmError)` — a bad embedding setup fails loudly at `boot`, not
  silently or three `eval` calls later.

`world_dir` is a required, explicit argument — there's no hardcoded
default path baked into `boot` itself. Pass `Path::new("world")` for the
same effective default the CLI uses.

`opts` is the same `VmOptions` struct the CLI builds from environment
variables (see [`tracing_debugging.md`](tracing_debugging.md) for what each
field controls) — build one directly rather than going through env vars if
you're embedding.

### `vm.eval(source: &str) -> Result<String, GuestError>`

Compiles `source` as a single top-level item — exactly what the REPL's
doit machinery does — runs it, and returns its `printString`. A class
definition (no result value to speak of) and an empty/whitespace-only
input both return `""`, not an error.

This is genuinely useful for feeding your embedding one statement or
definition at a time — a line typed into a REPL-like UI, one browser
"accept," one workspace selection — the same granularity `macvm repl`
offers interactively.

### `vm.set_transcript(sink: Box<dyn TranscriptSink>)`

Installs where guest output (`Transcript show:`, `printOnStdout:`) goes.
The default is stdout; call this once, right after `boot`, before your
first `eval`, if you want output routed somewhere else — a GUI's output
pane, a log, a channel back to your main thread. The trait is one method:

```rust
pub trait TranscriptSink: Send {
    fn show(&mut self, text: &str);
}
```

`Send` because the natural use — a GUI's worker thread producing output,
its main thread consuming it — means the sink usually needs to cross a
thread boundary itself (typically by wrapping an `mpsc::Sender` or similar).

## Errors you catch vs. the thread just dying

This is the distinction that matters most in practice, and it's easy to
get backwards if you're used to APIs where every failure is an `Err`:

| What happened | What you get |
|---|---|
| Source didn't lex/parse/compile | `Err(GuestError::Compile(_))` — ordinary, catch it and move on |
| A recovered native fault (SIGSEGV/SIGBUS) from an `Alien` raw-memory access ([`FFI.md`](FFI.md)) outside the JIT code cache | `Err(GuestError::NativeFault { sig, pc, far })` — also ordinary, `eval` returns normally |
| An uncaught `error:`, an unhandled `doesNotUnderstand:`, a stack overflow, real heap exhaustion | **`eval` never returns.** The thread is terminated via `pthread_exit` before it gets back to you. |

The first two are things a well-behaved embedding just handles as normal
control flow — a typo in a workspace, a bad FFI call. The third category is
different in kind: there is currently no `Result` for it at all. If you
need your embedding to survive a guest program that might hit one of
these, the answer is the thread-per-`VmHandle` model above, not a `match`
arm — let that thread die, notice via the missing heartbeat, and boot a
fresh `VmHandle` on a new thread if you want to keep going. The failure
message is written to your transcript sink before the thread exits, so the
*user* still sees why, even though your Rust code doesn't get an `Err` to
inspect.

## The JIT is fully supported — no restrictions

An earlier draft of the embedding design assumed the tier-1 compiler might
need to stay off inside an embedded VM. It doesn't: `boot`/`eval` place no
restriction on `opts.jit` at all, and the thread-death safety model above
is exactly what makes that safe — a JIT-compiled frame crashing is no
different from an interpreted one crashing, from the embedder's point of
view, because either way the whole thread just goes away cleanly. (There's
a separate, narrower constraint in `docs/SPEC.md` §16.5 about running with
`MACVM_JIT=off` specifically for a *future* live-redefinition workflow in
the planned browser/editor UI — that's a product decision about one
specific feature, not a limitation of `VmHandle` itself.)

## What's not here yet

- **`mirrors()`** — `docs/SPEC.md`'s design sketch for `VmHandle` includes
  a `mirrors()` method returning a reflection surface for building class
  browsers, inspectors, and similar tools. It is not implemented — the
  actual `src/embed.rs` today has exactly the three methods above.
- **No real consumer in this repo yet.** The GUI (`gui/`) is the intended
  first user of `VmHandle`, but as of this writing its worker thread
  (`gui/src/vm_host.rs`) still runs a hand-written stub, not the real
  `VmHandle` — its own doc comments say so explicitly. If you're looking
  for a working example of `VmHandle` embedded in a full application,
  there isn't one yet; the test suite in `src/embed.rs` (7 tests, all
  exercising real scenarios against the real `world/` image) is the most
  complete example available today.

## Quick reference

| Method | Signature | Notes |
|---|---|---|
| `boot` | `fn(VmOptions, &Path) -> Result<VmHandle, VmError>` | Call on the dedicated thread. Missing `world.list` is fine; a broken one is `Err`. |
| `eval` | `fn(&mut self, &str) -> Result<String, GuestError>` | One top-level item at a time. Never returns on a VM-fatal condition — see the errors table above. |
| `set_transcript` | `fn(&mut self, Box<dyn TranscriptSink>)` | Call once, right after `boot`. |

| Rule | Why |
|---|---|
| Run every `VmHandle` on its own dedicated thread | Fatal guest conditions terminate the thread, not the process. |
| Never `.join()`/`.is_finished()` that thread's `JoinHandle` | `pthread_exit` skips the bookkeeping both rely on — they panic/hang, not fail cleanly. |
| Detect thread death via a missing heartbeat (channel timeout) | The only sound signal available; a disconnect never fires either, since `pthread_exit` runs no `Drop` glue. |

## See also

- [`docs/SPEC.md`](SPEC.md) §16 — the full embedding & GUI design, including
  the mirrors/reflection plan and the GUI's own threading model.
- [`docs/tracing_debugging.md`](tracing_debugging.md) — what each
  `VmOptions` field controls.
- [`docs/probing_macvm.md`](probing_macvm.md) — PROBE's foreign-fault
  recovery (`codecache::deopt_trap`) is the same machinery `eval` uses to
  turn an `Alien` access's native fault into a `GuestError` instead of a
  crash.
