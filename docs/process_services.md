# Process services — the layer above the VMs

Decision of record, 2026-08-15. The direction, in the author's words: *"rust
runtime code now provides some services to workers, these services are above
or parallel to each VM we are running. We have messaging, timers, and the
transcript … I suspect that these services should be in their own thread, and
we really need to address VM lifetimes, we seem to have no good exit and no
worker table cleanup."* And on the fleet: *"I like the fleet state management
as well."*

## 1. The layer exists; it has no name and no owner

A census of the process-level state as of this record, scattered across four
files, each global with its own ad-hoc policy:

| state | where | policy today |
|---|---|---|
| worker table (id, epoch, alive) | `workers.rs` | lazy purge at 64 rows |
| dead-epoch list | `workers.rs` | grows; read by orphan pulse checks |
| primary epoch counter | `workers.rs` | monotonic, fine |
| transcript buffer | `transcript_service.rs` | serialized, bounded, loss reported |
| monitor roster | `embed.rs` | bounded, label-reuse rows |
| game command queue + input atomics | `cocoa_gui/game.rs` | main-thread drained |
| tick deadline | inside each worker's loop | dies with the loop |

**PROCESS SERVICES** is this layer, named: state and machinery that belongs to
the *process*, not to any VM. Its laws:

1. **VM-independent.** A service never holds `&VmState`, never depends on any
   VM pumping anything, and survives every VM's death. (The transcript is the
   model citizen; the old worker→primary→UI transcript relay was the model
   violation, and the hour lost to a silently-dead worker whose `isAlive`
   read *true* is the case study — both fixed by moving the state here.)
2. **Delivery is by the door.** A service that needs a VM to *run* something
   sends an ordinary envelope; the VM dispatches it top-level like any other
   message, one thing at a time. Terminate-as-poison set the precedent.
3. **Serialized or thread-owned, stated per service.** A passive store takes
   a lock (transcript). An active service owns a thread (timers). Nothing is
   both.
4. **Never join a VM thread** (the S21 law). Shutdown is poison + bounded
   waits on liveness flags, then exit regardless.

Physically the modules stay where they are for now (`transcript_service.rs`,
`timer_service.rs`, the tables in `workers.rs`); S4 regroups them under one
roof when the fleet state moves. The layer is the *contract*, not a
directory.

## 2. Per-service design

### Messaging — no thread, and none wanted

The transport is mpsc channels; delivery runs on the receiver's thread, which
IS the one-thing-at-a-time rule. A router thread would add a hop and a second
ordering domain for nothing. Messaging's real defect is *ownership*: the
fleet registry (links, id allocation, liveness, introductions) lives inside
`WorkerState::Primary`, entangling fleet operations with one VM's health.
That is S4's subject, not a threading question.

### Transcript — done; the reference implementation

Passive store, one lock, sequence numbers, bounded with reported loss,
wake-on-write, viewers own their cursors. Everything else here should look
like this when it can.

### Timers — the one service that owns a thread

The v1 timer piggybacked on each worker's inbox wait
(`recv_timeout` shortened to the next tick). It worked, and taught why it is
wrong: only spawned workers can have timers (the primary's wait belongs to
its host loop, the UI's to AppKit); the tick had to interleave with the
pulse check by hand; and nothing *cancels* a timer — it just dies with the
loop, or doesn't (the terminated game VM that ticked forever).

S1 replaces it: **one timer thread, one wheel, ticks delivered as
envelopes.** The thread sleeps to the earliest deadline (condvar-woken on
registration changes), and on firing sends the target VM a `#(#tick)`
payload down its ordinary inbox. Consequences, each worth the thread:

- Any VM can tick — the service needs only an `InboxSender`, and every VM
  has one. The primary gets timers for free.
- The worker loop returns to `recv` + pulse: no deadline math, no
  interleave, no starvation case.
- A tick is a message: top-level, serialized with everything else, by
  construction.
- Cancellation is a service operation: explicit (`tickEvery: 0`), and
  automatic — each fire checks the target's liveness flag and drops dead
  entries. A ghost cannot tick.

The Smalltalk face (`Worker tickEvery:`, `onTick:`, `dispatchTick`) does not
change; the gate for S1 is that the existing tick and game tests pass
untouched.

### Monitor and game state — annexed later

Both already behave like services (global, main-drained). They adopt the
laws when touched; nothing forces a move now.

## 3. VM lifetimes

The lifecycle, with one owner per transition:

```
SPAWNED ── boot fails ──────────────► DEAD (thread flips alive, sends notice)
   │
RUNNING ── guest fatal / init error ─► DEAD (same)
   │       poison envelope ──────────► DEAD (same; the ONLY external kill)
   │       primary epoch dies ───────► DEAD (self-reap at pulse)
   ▼
DEAD ── history row retained (bounded) ── purged
```

The holes this record closes:

1. **Liveness must be read from the process layer, not from an unread
   letter.** `isAlive` today reads the primary's link, updated only when the
   primary pumps its death notice — so it lies, for as long as the parent is
   busy, about a worker that died at birth. The truth already exists: the
   worker table row's `alive: Arc<AtomicBool>`, flipped by the dying thread
   itself. S2 points `primAlive:` at it. (Hosted peers — the UI — have no
   table row and keep the link check.)
2. **Deaths get watchers.** `onAnyWorkerDied` exists and nothing subscribes.
   S2 wires `AppVmHost` to it: a dead app is *reported* and its entry
   removed, so `liveCount` is true and the window's last frame is a known
   corpse rather than a mystery.
3. **Live set ≠ history.** The table serves both today. The live set must be
   exact (it is — the alive flags); the history is for the Monitor and stays
   bounded with the existing lazy purge. Stated so nobody "fixes" one at the
   other's expense.
4. **Timers, peers, registries clean on death.** Timers: S1's liveness check.
   Peer links: already dropped on failed send. App registries: S2's watcher.

### A VM ends itself — S5, the author's rule

*"A vm should exit itself and send a vm_exiting message as it does so."*

Every ending the ladder above describes is somebody ELSE's decision arriving
from outside: a poison envelope from the parent, a fatal error, a reaped
epoch. Each leaves the peers to INFER the death from a runtime notice sent
afterwards — which is fine for an accident and wrong for a decision. A window
closes, a job finishes, an app concludes it is done: those are the VM's own
call, and the VM should make it, announce it, and go.

- `Worker exit: aReason` — announces `{#vmExiting. selfId. reason}` to the
  display and the parent, THEN requests the exit. Order is the guarantee: the
  goodbye is queued ahead of the death by construction, so a peer always hears
  the VM's own word before the runtime's report.
- The announcement uses `primSendQuiet:` (the same prim 221, with a
  non-raising fallback) because it runs below the world's exception classes
  and must never be stoppable by an audience that left first.
- The exit itself is prim 278 → `workers::exit_self`, which posts the SAME
  poison envelope an external terminate posts, to this VM's own inbox. The
  work in flight finishes — a VM does one thing at a time, and an exit is not
  an exception to that — then the loop's existing terminate arm runs. **One
  exit path, reached two ways.**
- Listeners: `Worker alsoOnVmExiting: [:id :why | …]`, separate from the death
  watchers. An exit is a decision and a death is an accident; a registry that
  reports them in one sentence is lying about one of them. Whoever retires the
  entry on the exit leaves the following `#workerDied` nothing to say, so one
  ending stays one line.
- Routing law learned twice: a control message must NOT be consumed in
  `dispatchOne:`. The display is a shape-routed VM whose whole handler is the
  reply hook, so anything that returns early there never reaches it.

**The first consumer — a window close ends its app (`world/93_appvm.mst`).**
`AppToolWindow`'s default is still "close is hide, never die", which is right
for an in-process tool whose state is class-side in this image. A VM-backed
window sets a close HOOK instead, because hiding one leaves a thread, an
inbox and a ticking timer publishing frames at glass nobody can see. The hook
does only what is safe inside AppKit's own `windowShouldClose:` — hide, forget
the bookkeeping, and SEND — because `teardown` clears the window's delegate
and destroys its views, and doing that while AppKit is mid-call on that very
delegate is the hazard this codebase refuses everywhere else. The app's
`#vmExiting` comes back through the door and destroys the window in a drain.
In-flight frames from the retired VM are judged BY PEER, not by id, so a
relaunch under the same name — a new VM, a new generation — registers a fresh
window while a ghost cannot resurrect the old one.

Consequence for S4b: a spawn-granted VM can now also READ liveness for its
grant's epoch (`workers::alive`). Authority to spawn into a fleet and
visibility of what you spawned are the same grant; without it the display
could not tell a live app from a corpse behind the same window.

## 4. Exit — the sequence that does not exist today

Today ⌘Q kills detached VM threads mid-instruction; the CLI end drops the
primary and lets orphans discover it at their next pulse. S3 gives both hosts
one sequence:

1. stop the timer service (no new ticks);
2. poison-broadcast every live worker (`terminate_all` — the same envelope
   `terminate` sends, to every live link);
3. bounded wait (≤ ~250ms total) on the table's alive flags — **never a
   join**;
4. exit regardless. Anything still running is killed by the OS as before —
   but now *after* being asked, with its sinks flushed by its own exit path.

The transcript needs no flushing (writes land synchronously); the image is
journaled SQLite; so the sequence is about giving guest code its `ensure:`
blocks and the monitor its true final states, not about data loss.

## 5. Fleet state management — S4, the arc's spine

Approved in direction; its own implementation arc. The fleet registry moves
out of `WorkerState::Primary` into process services:

- **The kernel owns:** id/generation allocation, the link table, liveness,
  introductions (who is the display), poison delivery, the worker table.
- **The primary keeps:** the boot closure (what world a worker gets) and
  spawn *policy* (who may ask). `WorkerState::Primary` shrinks toward "a VM
  that registered a boot fn"; `WorkerState::Worker` is unchanged.
- **What it buys:** `spawn`/`terminate`/`introduce` become kernel calls any
  authorized VM can make (the UI launching an app VM stops being a
  primary-relayed doit); liveness and cleanup have one owner; a dead primary
  no longer takes the *registry* down with it (its workers already outlive
  it via epoch reaping — after S4 the bookkeeping does too).
- **What it deliberately does not change:** workers still don't spawn
  (policy, not plumbing); peer links stay learned; delivery stays by the
  door.

Gate: the whole existing suite unchanged, plus the UI spawning an app VM
through the kernel with the primary uninvolved.

## 6. Sprint ladder

| sprint | contents | gate |
|---|---|---|
| **S1** | timer thread; ticks as envelopes; worker loop simplified; cancel-on-death | existing tick + game tests pass UNCHANGED; a primary can tick |
| **S2** | `primAlive:` reads the table; death watcher wired to `AppVmHost`; live/history split stated | `isAlive` false immediately after terminate, unpumped; a dead app is reported |
| **S3** | `terminate_all` + the exit sequence in CLI and GUI | quit leaves no ticking threads; monitor rows reach DEAD |
| **S4** | fleet registry into process services | suite unchanged; UI spawns an app VM with the primary uninvolved |
