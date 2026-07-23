# OTP-style workers — supervision trees over the worker fleet

**Status: design.** The roadmap's next system item (design-philosophy roadmap;
`docs/asyncio_design.md`'s own reuse map already names it: "I/O-worker fault
handling: `ErrorPolicy::Die` + supervisor respawn"). The worker substrate
(`docs/multi-smalltalk-worker.md`, M0–M4 all shipped) gives us Erlang's
*process* layer: isolated heaps, copy-only messages, let-it-crash, death as an
ordinary message. What it does not give us is Erlang's *recovery* layer — the
thing OTP actually is: **supervisors** that own a set of children, restart
them by declared policy when they die, give up and escalate when restarting
stops helping, and compose into trees. This design adds exactly that layer.

```smalltalk
| sup |
sup := WorkerSupervisor named: #services strategy: #oneForOne.
sup superviseNamed: #sieve
    init: 'Worker onMessage: [:m | Worker reply: (Primes below: m payload)]'.
sup superviseNamed: #io init: IoWorker bootSource restart: #permanent.
sup start.
"…a worker crashes mid-job: the supervisor gets the #workerDied, respawns it
 from its stored init source, and rebinds the name — callers using
 (WorkerNames named: #sieve) never hold a stale handle…"
```

**The spec is OTP itself.** Exactly as the type checker ported Strongtalk's
`.dlt` sources rather than inventing rules (`docs/typechecker_design.md`), this
design ports the documented semantics of Erlang/OTP's `supervisor` behavior —
child specs, restart policies, `one_for_one`/`one_for_all`/`rest_for_one`,
restart intensity, reverse-order shutdown, escalation — deviating only where
MACVM's reality genuinely differs, and saying so each time (§2.3).

## 1. Goal and non-goals

**Goal.** Long-running *services* built out of crashable workers: a worker
fleet where any worker can die at any time — guest error, native fault, wedged
loop — and the system converges back to a running state by declared policy,
not by hand-written recovery code at every call site. This is the project's
error-handling story, stated positively: scoped catch was considered and
REJECTED (design-philosophy memory; there is no exception system, and `error:`
stops the program) because **let-it-crash + supervision** is the intended
model. This arc is where that intention becomes machinery.

**Non-goals (v1):**

- **Not lightweight processes.** An Erlang node runs millions of processes; a
  MACVM worker is a whole VM on an OS thread, capped at 16 (`MAX_WORKERS`).
  This is OTP's *supervision* layer applied to a small fleet of heavyweight
  services — the shape of a typical OTP application's top two tree levels,
  not its leaf processes. Green processes within one heap (SPRINTS S17)
  remain orthogonal future work.
- **No worker-hosted supervisors.** v1 topology is a star: only the primary
  spawns and only the primary hears `#workerDied` (`workers.rs`). Therefore
  supervisors are **ordinary Smalltalk objects living in the primary VM** —
  the tree is a tree of objects, not a tree of processes. Escalation between
  supervisors is a same-heap method send, which removes a whole class of OTP
  edge cases (a supervisor cannot itself crash independently of the primary).
- **No new exception machinery.** A supervised call that fails delivers an
  error *value* to a continuation (the asyncio slice-B rule: "I/O errors are
  values in messages, not exceptions"). Nothing here introduces catch.
- **No distribution, no worker↔worker links.** Same process; star topology;
  both unchanged from the worker design's own non-goals.

## 2. Grounding

### 2.1 What already exists (the reuse map)

| need | exists — verified in source |
|---|---|
| spawn a fresh child with declared init | `Worker spawn: initSource` (prim 220; init doit runs once in the fresh VM — `workers.rs::spawn`, `worker_main`) |
| death notification, precisely for *crashes* | `#workerDied` envelope through the one inbox — synthesized by `worker_main` on boot/init/dispatch failure and by a failed send. **A deliberate `terminate` produces NO death notice** (`workers.rs::terminate` drops the channel; `worker_main`'s recv loop ends silently) — crash vs. intended shutdown are already distinguishable, for free |
| crash isolation | S21: guest error / native fault ends the *thread*, never the process; `ErrorPolicy::Die` documented as "for throwaway/pooled compute workers … a supervisor respawns" (`embed.rs`) |
| cheap restart | worker boot ≈ 20 ms (measured, worker design §2); `spawn` reuses terminated slots, so restart cycles never exhaust the id space (the ParallelMandel cap bug, fixed) |
| async request/reply for the service layer | `send:onReply:` continuations, (peer, corr)-keyed; the `#rpc` protocol (`47_worker.mst`) |
| an event loop to hang sweeps on | `runLoopWhile:` / `pumpInbox:` (headless) and the GUI's supervisor beat (~4 Hz, CG4) — both already iterate on a bounded cadence |
| a monotonic clock for windows/deadlines | `Smalltalk millisecondClock` (prim 92) — the same clock `Time millisecondsToRun:` trusts |
| the first real customers | `IoWorker` (`world/62_ioworker.mst`) — asyncio's own doc defers its fault story to "the supervisor respawns"; ParallelMandel's band workers; future compute pools |

**Expected new Rust: none.** Like asyncio slices A–C ("zero new Rust, by
design"), every mechanism the supervisor needs already has a primitive. The
whole layer is world-side Smalltalk plus one small routing seam in the
existing `Worker class >> dispatchOne:` (§4.1) — itself Smalltalk. If a slice
discovers a genuine primitive gap, that is a finding to record, not silently
patch around.

### 2.2 What OTP defines (the semantics being ported)

From Erlang/OTP's `supervisor` behavior, the load-bearing subset:

1. **Child spec**: `{id, start, restart, shutdown, type}` — who the child is,
   how to (re)start it, when to restart it, how to stop it, and whether it is
   itself a supervisor.
2. **Restart policy** per child: `permanent` (always restart on death),
   `transient` (restart only on *abnormal* exit), `temporary` (never restart).
3. **Strategy** per supervisor: `one_for_one` (restart just the dead child),
   `one_for_all` (a death restarts every child), `rest_for_one` (restart the
   dead child and every child started *after* it).
4. **Restart intensity**: more than `max_restarts` within `max_seconds` and
   the supervisor gives up — terminates all its children and itself fails,
   which its *parent* supervisor sees as a child death and handles by *its*
   policy. At the root, giving up is reported and the tree stays down.
5. **Shutdown**: a stopping supervisor terminates children in **reverse start
   order**.
6. **Registration**: children are found by name (`whereis`), never by holding
   a raw pid across restarts — the name is rebound on every restart.

### 2.3 Deliberate deviations (each grounded in a MACVM fact)

- **`transient` is dropped.** OTP's `transient` needs a "normal exit" a
  worker can perform; a MACVM worker has no exit-with-reason — it serves until
  terminated (silent, intended) or it crashes (`#workerDied`). With only two
  observable ends, `permanent` (restart on crash) and `temporary` (never
  restart) cover the space; `transient` would be `permanent` under another
  name. Rejected rather than aliased, to keep the surface honest.
- **`shutdown` timeouts are dropped (v1).** OTP lets a child spec choose
  `brutal_kill` vs. a grace period for cleanup. MACVM's `terminate` is
  channel-drop — the worker finishes its **current** message (strictly serial
  dispatch guarantees a message is never abandoned mid-handler) and exits on
  the next `recv()`. Since workers are share-nothing and rebuild all state
  from `init` on restart, there is nothing a grace protocol would save.
  Documented as brutal-after-current-message; a cooperative `#shutdown`
  message is v2 if a real workload ever needs flush-on-stop.
- **Supervisors are primary-heap objects, not processes** (§1). Consequences,
  all simplifying: escalation is a method send; a supervisor cannot die
  independently; the whole tree shares the primary's one `#workerDied`
  stream, demultiplexed by child id (§4.1). The primary itself is the tree's
  implicit root — and *it* is already supervised one level up by the S21/CG4
  restart-in-place machinery (`VmHost::restart`), which stays untouched: two
  independent layers of defense, in-tree restarts for workers, whole-world
  restart for the primary.
- **Blocking calls do not exist.** OTP's `gen_server:call` blocks the caller
  up to a timeout. The worker model's first law is "the primary never waits"
  — so the service layer's `call` is `send:onReply:` plus a **deadline**, and
  a timeout is an error *value* delivered to the same continuation path
  (§5). Nothing blocks, ever.

## 3. The classes (all world-side, one new file `world/74_supervisor.mst`)

Numbering note: 74 follows the packed 63–73 cocoaui block; the class is
base-world (loads headless — every primitive it touches fails cleanly with no
worker role, the `GamePane` posture `47_worker.mst` already keeps).

### 3.1 `WorkerSpec` — the child spec

```smalltalk
Object subclass: WorkerSpec [
    | name          "Symbol — unique within its supervisor"
      initSource    "String — the spawn: init doit; THE restart recipe"
      restart       "#permanent | #temporary"
      worker        "current live Worker handle, or nil"
      startedAt |   "millisecondClock at last successful (re)start"
]
```

The **init source is the whole recovery story**: share-nothing workers hold no
state a restart must migrate, so restart ≡ `Worker spawn: initSource` — the
same fresh-init semantics OTP restarts have. A spec whose service needs
warm-up (an IoWorker's watch re-arms, a cache preload) expresses it *in* the
init source or in an optional `afterRestart:` block run primary-side on each
(re)start (§6's IoWorker item shows why that hook earns its place).

### 3.2 `WorkerSupervisor` — the behavior

```smalltalk
Object subclass: WorkerSupervisor [
    | name strategy children     "OrderedCollection of WorkerSpec, START ORDER"
      maxRestarts perSeconds     "intensity window; default 3 in 5 (Elixir
                                  Supervisor's default — Erlang's own is a
                                  stingier 1 in 5; 3 is the friendlier modern
                                  choice, and the value is per-supervisor
                                  configurable either way)"
      restartTimes               "OrderedCollection of recent restart clocks"
      parent                     "the supervising WorkerSupervisor, or nil"
      state |                    "#idle | #running | #givenUp"

    "building the tree"
    WorkerSupervisor class >> named: aSymbol strategy: aStrategy [ … ]
    superviseNamed: aSymbol init: aString [ … #permanent … ]
    superviseNamed: aSymbol init: aString restart: aPolicy [ … ]
    superviseSupervisor: aWorkerSupervisor [ "a child that is itself a tree" ]

    "lifecycle"
    start [ "spawn every child in order; register with SupervisorRegistry" ]
    stop  [ "terminate children in REVERSE start order; deregister" ]

    "the event, routed to us by the registry (§4.1)"
    childDied: aSpec [ "apply strategy + intensity; restart or escalate" ]
]
```

- `strategy` ∈ `#oneForOne | #oneForAll | #restForOne`, OTP's semantics
  verbatim: `#oneForAll` terminates the survivors (reverse order) then
  restarts all in order; `#restForOne` does the same for the suffix of
  `children` after the dead one.
- **Intensity**: `childDied:` prunes `restartTimes` to the last `perSeconds`
  (monotonic `Smalltalk millisecondClock`) and, if a restart would exceed
  `maxRestarts`, **gives up**: terminate all children (reverse order), set
  `#givenUp`, then `parent isNil ifTrue: [report to Transcript] ifFalse:
  [parent childSupervisorGaveUp: self]` — which the parent treats exactly as
  a child death under its own strategy (restart = a fresh `start` of this
  supervisor object; its specs and init sources are all still here).
- A `#temporary` child's death is recorded (Transcript note, spec's `worker`
  nils, name unbound) but triggers no restart and no intensity charge —
  OTP's rule.

### 3.3 `WorkerNames` — the registry (`whereis`)

A class-side `Dictionary` name→`Worker` handle. `WorkerSupervisor` rebinds
the name on every (re)start; `WorkerNames named: #sieve` is how *all* client
code reaches a supervised worker, so no caller ever holds a corpse across a
restart. Unknown name → nil (a value, not an error). Names are global across
the tree — two specs with one name is a spec-time error, same as OTP.

### 3.4 `ServiceWorker` — the thin gen_server face (slice O3)

Not a new mechanism — sugar over what `47_worker.mst` already does, giving
the fleet one vocabulary:

```smalltalk
svc := ServiceWorker named: #sieve.          "resolves via WorkerNames"
svc cast: #(recompute 10000).                "async, fire-and-forget (send:)"
svc call: #(primesBelow 10000)
    timeoutMs: 2000
    onReply: [:r | …]
    onError: [:e | …].    "e = #timeout | #workerDied | the RPC error string"
```

`call:…` is `send:onReply:` + a deadline entry; `onError:` fires on timeout,
on the target's death before reply, or on an `#rpcError` — one error funnel,
all values. This is also where the pending-continuation leak closes: today a
reply that never comes leaves its continuation tombstoned in
`PendingReplies` forever (the UI's `abandonPending` is a blunt whole-table
reset); a deadline entry retires exactly its own key (§5).

## 4. Event plumbing (the one seam in existing code)

### 4.1 Routing `#workerDied` to its supervisor

Today `Worker class >> dispatchOne:` routes shape-tagged control messages
(transcript, cocoa, uiReq, snapshot) before continuations, but `#workerDied`
falls through to the general `ReplyHandler`. Add the twin of the existing
hooks — checked in the same shape-first block:

```smalltalk
Worker class >> onWorkerDied: aBlock [ DiedHandler := aBlock ]   "class-var rooted"
"in dispatchOne:, with the other shape tags:"
msg isWorkerDied ifTrue: [
    DiedHandler isNil ifFalse: [ DiedHandler value: (msg payload at: 2) ].
    ^self ].
```

`SupervisorRegistry` (a small class-side table child-id → owning
`WorkerSupervisor`+`WorkerSpec`, maintained at spawn/terminate) installs one
`DiedHandler` for the whole tree and demultiplexes by id. An id no supervisor
owns (a bare `Worker spawn:` outside any tree — every existing demo) logs and
returns: **unsupervised workers keep today's behavior exactly**, and every
existing test stays green untouched.

Ordering subtlety, stated now so O1's test asserts it: a `send:` to a
crashed-but-undetected worker fails, synthesizes `#workerDied`, and *returns
false to the sender* — so a caller may observe a failed send one dispatch
before the supervisor observes the death. The service layer's `onError:`
funnel (§5) is the answer at call sites; raw `send:` keeps its current
semantics.

### 4.2 The deadline/intensity sweep

Timeouts and intensity windows need time to pass, and the message plane is
purely event-driven — deliberately no timers in it (worker design §3.1). The
sweep therefore piggybacks on cadences that **already tick**:

- headless: `runLoopWhile:`/`pumpInbox:` already wake at least every 250 ms —
  `dispatchInbox` gains a trailing `ServiceWorker sweepDeadlines` (a no-op at
  zero pending calls).
- GUI: the CG4 supervisor beat (~4 Hz) already execs a pump; same trailing
  sweep, no new timer.

Resolution is the cadence (~250 ms), which is the right coarseness for
*service* timeouts (seconds), and the sweep never fires a continuation early
— only after `deadline < now`. No message is ever delivered by the sweep;
the §3.1 "no polling in the message plane" law survives because the sweep
moves no messages, it only expires promises.

## 5. Failure model (the contract, end to end)

| event | detected by | consequence |
|---|---|---|
| child crashes (guest error, native fault, boot/init failure) | `#workerDied` → `SupervisorRegistry` → owning supervisor | strategy applies: restart it (`#oneForOne`) / the suffix (`#restForOne`) / everything (`#oneForAll`); name rebound; intensity charged |
| restart storm | intensity window (`maxRestarts` in `perSeconds`, monotonic clock) | supervisor gives up: children down in reverse order, escalate to parent (a method send) or report at root; state `#givenUp` is inspectable |
| supervised call outlives its deadline | `sweepDeadlines` on the pump cadence | continuation retired, `onError: #timeout` fires, `PendingReplies` entry freed (the leak fix) |
| target dies with calls in flight | death routing (§4.1) cross-checks the deadline table by target id | every pending call to that id fails fast with `onError: #workerDied` — no waiting out the timeout |
| RPC-level error (unknown class, argc) | the existing `#rpcError` reply | `onError:` with the message string — same funnel |
| deliberate `stop`/`terminate` | nothing — by design (no death notice) | no restart, no intensity charge; crash and intent stay distinguishable |
| the primary itself dies | S21/CG4 layer above (untouched) | whole-world restart-in-place; the tree is rebuilt by the boot script that built it the first time |

The property the capstone (§7 O4) proves: **a service's availability is a
supervisor policy, not a property of any worker's luck.**

## 6. First customers (the designs this pays off)

- **`IoWorker`** — asyncio's doc defers its fault story here verbatim. A
  supervised IoWorker restart is unusually cheap because of two facts that
  already fell out of that design: the **kqueue lives in the primary** (it
  "creates the kqueue … bakes the kq fd into the boot doit"), and kernel-side
  watch registrations belong to the kq — both *survive* the worker VM's
  death. The `afterRestart:` hook re-sends the pump kick; O3's gate proves a
  watched pipe keeps delivering across a kill (with the one honest caveat
  investigated then: any batch in flight at crash time is lost — at-most-once
  stands).
- **Compute pools** (ParallelMandel bands, future `parCollect:`) — pool
  workers as `#permanent` children of one `#oneForOne` supervisor; a band
  worker's crash costs one band's recompute, not the frame pipeline.
- **The TCP echo service** — asyncio's capstone rebuilt as a *supervised*
  service (§7 O4), the demonstration that the two arcs compose into "a server
  that stays up".

## 7. Milestones

| # | contents | size | gate |
|---|---|---|---|
| **O0** | `WorkerSpec`, `WorkerSupervisor` (#oneForOne), `WorkerNames`, `SupervisorRegistry`, the `onWorkerDied:` routing seam — **logic proven single-VM, zero threads**: the supervisor is driven by *synthetic* `#workerDied` payloads (the `IoStream` synthetic-batch precedent), with spawn/terminate behind a test double | M | single-VM unit suite: restart-on-death, name rebinding, `#temporary` no-restart, intensity gives up at exactly maxRestarts+1, reverse-order stop; existing suites untouched |
| **O1** | live wiring: real `spawn:`/`terminate`, `embed.rs`-style integration — a supervised echo worker is killed (guest `error:` in its handler) and service resumes; unsupervised-spawn compatibility | M | embed tests: crash→respawn→same name answers again; intensity storm (a child whose init crashes) ends in `#givenUp`, process fine; failed-send-then-died ordering asserted (§4.1) |
| **O2** | `#oneForAll`, `#restForOne`, `superviseSupervisor:` (trees + escalation) | M | strategy-matrix tests: who restarts under each strategy given a death at each position; a giving-up child supervisor is restarted by its parent; root give-up reports and stays down |
| **O3** | `ServiceWorker` call/cast + `sweepDeadlines` (timeout/died/rpcError funnel, PendingReplies retirement); IoWorker adopted (`afterRestart:` re-arm) | M | timeout fires at deadline (and not before) with zero table growth over 10k calls; in-flight calls fail fast on target death; IoWorker killed under an active pipe watch → data flows again after respawn |
| **O4** | **capstone: the supervised echo service** — one tree: `#services` ⟨ IoWorker, N echo workers ⟩; clients hammer the TCP echo while a chaos loop kills random workers | M | service answers 100% of requests issued *after* each recovery window; soak per the standing matrix (RELEASE + PARALLEL + thr≥200 *and* t=1), full-GC cascade in the primary throughout; zero pending-reply leak at end |

Each milestone lands as its own commit with its gate green, per the standing
commit discipline. O0 first and single-VM on purpose: the supervisor's whole
decision logic (strategies, intensity, escalation) is pure Smalltalk over
message values, so it can be proven exhaustively before any thread exists —
the same non-disruptive-first-milestone posture as MOP's M0 and the
typechecker's T0′.

## 8. Future work (explicitly deferred)

- **Cooperative `#shutdown`** (grace period, flush-on-stop) — v2, only if a
  stateful-flush workload appears (§2.3).
- **Green processes (S17) under supervision** — when in-heap processes exist,
  the same supervisor vocabulary should extend down to them; nothing in v1's
  surface presumes the child is a VM (a `WorkerSpec` is a name + a start
  recipe + a policy).
- **Worker-hosted subtrees** — requires worker→worker spawn (the reserved
  prim 229 introduction path); the object-tree design ports cleanly since a
  supervisor is already heap-local wherever it lives.
- **A supervision-tree GUI pane** — live tree + restart counters + give-up
  states; natural once the Cocoa metrics toolbar grows a workers section.
- **Typed specs** — `WorkerSpec` and friends annotated (T4 discipline) once
  the file lands; the checker's static-DNU then covers the new layer like
  the rest of the core library.

## 9. Cross-references

- `docs/multi-smalltalk-worker.md` — the substrate: spawn/init, MOP,
  `#workerDied`, continuations, run loops. This design adds no transport.
- `docs/asyncio_design.md` — the first supervised customer; its reuse map's
  final row ("supervisor respawn") is implemented here.
- `src/embed.rs` `ErrorPolicy` — `Die`'s doc comment names this design's
  role; `Resume` remains right for the interactive primary.
- S21 / `cocoa_gui_design.md` CG4 — the layer *above*: the primary's own
  restart-in-place. Unchanged; the two layers compose (§2.3).
- `docs/typechecker_design.md` — the porting discipline this doc copies:
  an external system's documented semantics as executable spec, deviations
  listed with reasons, staged milestones each with its own gate.
