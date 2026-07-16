# MACVM roadmap

Agreed 2026-07-16. This is a direction, not a schedule.

## The bet, and the yardstick

MACVM is a lean, memory-safe (Rust), embeddable, **many-core**, aggressively
JIT-compiled Smalltalk-lineage VM built for the 10-to-128-core era — closer in
spirit to Erlang and Rust than to Squeak. Every candidate addition is judged by
one question:

> **Does it double down on the bet — or just make MACVM more like Pharo?**

Additions that amplify isolation, many-core parallelism, the compiler, or
embedding are on-roadmap. Additions that only close the distance to Squeak's
semantics are not — because several of those "gaps" are deliberate choices (see
below), and the one that's genuinely just missing (library breadth) is answered
by time and porting, not design.

### What is deliberately NOT on the roadmap (choices, not omissions)

- **Green threads / in-image `Process`/`Semaphore`.** Cooperative green threads
  give ~zero parallelism (one core per image). MACVM uses share-nothing worker
  VMs + message passing to get *real* multi-core. That is the point, not a gap.
- **A living object-memory image.** The image is an unversionable blob that
  conflates code with accumulated runtime state. MACVM makes *code* the source
  of truth (versioned SQLite + `.mst` export) and rebuilds the runtime
  reproducibly. Pharo has been moving this way for years (Iceberg/git); MACVM
  committed to the endpoint.
- **A full ANSI exception system.** Fault isolation is handled by worker
  crash-recovery + supervisor respawn ("let it crash"); ordinary error handling
  by the explicit-result style (`ifAbsent:`, sentinels — Rust's `Result`/
  `Option`). `ensure:`/`ifCurtailed:` already run on the error path. Item 2 below
  closes the one honest residual without dragging in resumable-exception
  machinery.
- **Live `become:` / class reshaping.** Shape edits take effect on reboot; a
  save-time live migration is *designed* (`docs/become_design.md`) but shelved —
  a ~1 s reboot makes it a minor convenience, not a need.

## What already exists (what the roadmap builds on)

- A tier-1 JIT with **deoptimization**, inline caches + PICs, IC-driven type
  specialization, an **unboxed-float fast path**, explicit-NEON **SIMD** fusion,
  and register residency.
- A moving, generational **GC** with safepoints that work *under live compiled
  frames*, and stress-tested complete reference enumeration (`for_each_root` /
  `for_each_oop_field`).
- **Multi-VM workers**: isolated heaps, message passing, guest-fatal
  recover-clean-or-die, supervisor respawn.
- **FFI** (dlsym, trampolines, Alien) and a **Cocoa/Metal** native bridge.
- The **code database** (versioned class/method store) and the live editor.

---

## Priorities (most on-bet first)

### 1. An OTP-style worker framework
**Turn the differentiator from a property into a platform.** Today there are
workers plus supervisor-respawn — the safety *net*. Erlang's power is the layer
above it: supervision **trees** with restart strategies, **links/monitors** (a
worker learns when a peer it depends on dies), **named** workers, worker
**pools**, and **selective receive**. "Let it crash" only becomes *productive*
once crashes are routine and recovery is declarative.

- *Why highest leverage:* it makes the many-core + let-it-crash bet pay off; it's
  the thing that most makes MACVM *itself* rather than a faster Smalltalk.
- *Builds on:* the existing worker channel, MOP-pickle, wake/inbox, and the
  recover-clean-or-die + respawn machinery.
- *Effort/risk:* medium, mostly in-image Smalltalk + a little runtime; low risk
  (no GC/codegen surgery).
- *First step:* links/monitors + a `Supervisor` with one restart strategy
  (one-for-one), driving the existing respawn.

### 2. A scoped, non-resumable `catch:` / `throw:`
**Close the one honest residual — intra-computation recovery — cheaply.** "Step
3 of a doit failed; recover locally and continue" currently has no answer between
worker-level and top-level abort. A block-scoped `[...] catch: [:err | ...]`
(or `ifError:`) that unwinds to the handler, runs `ensure:` cleanups on the way,
and delivers a value — **no `resume:`, no retry state machine** — fills it.

- *Why:* best value-per-effort here, and it stays in the explicit-error
  philosophy (no invisible non-local unwinding across arbitrary code). It is
  emphatically *not* full ANSI exceptions.
- *Builds on:* the unwinding substrate already present (`ensure:`/`ifCurtailed:`,
  the marker-frame walk, the guest-fatal `siglongjmp` path in `unwind.rs`).
- *Effort/risk:* small–medium; the mechanism exists, this scopes and exposes it.
- *First step:* a handler-marker frame + a one-shot unwind-to-marker that
  reuses the curtailment walk.

### 3. Non-blocking I/O + sockets
**Turn the compute engine into a system.** Isolation + many-core + let-it-crash
is the Erlang *server* recipe, and a server needs async I/O and sockets —
ideally as I/O workers that own their descriptors and hand results back as
messages. This unlocks the most *new* territory of anything on the list.

- *Why:* without it the worker model is a parallel calculator, not a platform;
  with it, MACVM is something you'd build a network service on.
- *Builds on:* the FFI (for syscalls/`kqueue`) and the worker mailbox model.
- *Effort/risk:* medium; primitives + an event-loop worker + a stream library.
- *First step:* non-blocking sockets + a `kqueue`-driven I/O worker delivering
  readable/writable events into a mailbox.

### 4. Escape analysis / scalar replacement in the JIT
**Delete allocation — because our own measurements say allocation is the cost.**
Mandelbrot's 746→166 ms came from removing five heap `Double`s per iteration,
not cleverer math; and "instructions aren't time" (a 21% instruction cut bought
0.35%) — but *allocation is* time. Smalltalk allocates relentlessly. The
float fast path already proved the pattern (unbox → compute in registers → sink
the box past the cold paths) for `Double`; generalize it to arbitrary
short-lived, non-escaping objects.

- *Why:* the highest-value place for compiler effort — more than O2/O4, which
  the same measurements showed are nearly free.
- *Builds on:* the deopt infrastructure (an object that turns out to escape
  deopts to a real allocation), the float-fastpath box-sinking, the IC feedback.
- *Effort/risk:* large but tractable and *measured* to be real; the risk is
  ordinary codegen/deopt correctness (GC-stress-verified).
- *First step:* escape analysis for objects that never leave their creating
  method, starting with `Point`/small fixed-shape temporaries.

### 5. Shared immutable data between workers
**Remove the copy tax that is the one real downside of share-nothing.** A
read-only shared region (or passing large immutables by reference rather than by
copy) makes the worker model viable for big data, not just small messages —
BEAM does exactly this for binaries.

- *Why:* directly de-risks the central bet at scale.
- *Builds on:* the GC's generation/space machinery and the message-passing path.
- *Effort/risk:* medium–large; real memory-model design (immutability invariant,
  cross-heap references, GC of the shared region).
- *First step:* a shared, immutable, never-collected region for large byte/
  string blobs passed between workers.

---

## Below the line (good, more incremental)

- **Finish the in-flight JIT payoff:** S24 B5 multi-BB block splicing; retain
  Strongtalk type annotations for sharper specialization; bounds-check
  elimination for array loops (`docs` + `project_strongtalk_port_survey`).
- **Explicit object-graph persistence:** elevate the worker MOP-pickle into a
  real "persist / restore this graph to the DB" facility — durable state without
  the image's downsides, fully in the code-as-truth model.
- **Richer native bridge:** the deferred Tier-2 Cocoa FFI (blocks-as-callbacks,
  delegates, async) — amplifies the embedding differentiator.
- **A tier-2 optimizing compiler** eventually (real inlining, LICM, on top of
  tier-1 + deopt) — the natural V8/Sista evolution, but a big project and not
  urgent while tier-1 is already strong.
- **Ecosystem breadth** (networking beyond sockets, more collections/streams,
  tooling) — the one genuinely *missing* thing rather than a divergence; closed
  only by time and porting.

## How to read the priority order

1 and 2 are the cheapest high-leverage moves (framework + the honest gap). 3
opens the most new ground. 4 is where compiler investment pays, on the evidence
of our own profiling. 5 hardens the bet for scale. Pick by appetite: **2** for
maximum value per hour, **1** to invest in MACVM's identity, **3** to unlock a
new class of program.
