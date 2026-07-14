# Multi-Smalltalk workers — primary/worker VMs with copy-passing messages

**Status: design.** Grounded in the proven spawned-VM demo (the Demos menu's
"Mandelbrot — in a spawned VM", 2af5eb8): two full VM instances — each with its
own heap, JIT, and code cache — already run in parallel on their own OS threads
inside one process, coordinated only by channels. This design turns that
one-off demo into a **general primary/worker facility**: Smalltalk code in the
primary VM spawns worker VMs, exchanges messages with them by **deep copy**
(never by reference), and tears them down — Erlang-style share-nothing
parallelism, driven entirely from the language.

```smalltalk
| w |
w := Worker spawn.
w send: { #lengthOf. 'hello' }
  onReply: [:r | Transcript show: r printString].   "fires when the reply lands"
"the primary is already free — it sleeps until the router wakes it"
w terminate.
```

## 1. Goal and non-goals

**Goal.** True multicore parallelism for Smalltalk programs: N workers = N
cores computing simultaneously, with a communication model simple enough to
reason about — *everything that crosses a VM boundary is a copy*.

**Async by design.** The primary **never waits** on a worker. Every send is
fire-and-forget; every reply arrives as an *event* that wakes the primary,
which spends its idle time genuinely asleep (today's primary VM thread already
sleeps in `request_rx.recv()` between doits — `vm_host::worker_loop`; this
design rides that same sleep). Replies are handled by **continuations**
(`send:…onReply: [:r | …]`, correlation-id matched), never by blocking. There
is no polling anywhere in the system: the event router (§3.1) is the inbox
channel plus a wake hook, and the *send itself is the wake*.

**Non-goals (v1):**

- **No shared state.** No shared heap, no shared globals, no proxies, no
  remote references. A message is pickled in the sender's heap and rebuilt in
  the receiver's; the two graphs never alias. (This is the same stance as the
  README's `become:` section: identity never crosses a heap boundary.)
- **No worker↔worker channels.** Star topology: the primary talks to each
  worker; workers talk only to the primary. (Worker-to-worker is a v2 item —
  the registry design below doesn't preclude it.)
- **No distribution.** Same process, same machine. The pickle format is
  deliberately position-independent bytes, so a future remote transport can
  reuse it, but that is out of scope.
- **Not green processes.** SPRINTS Phase E's S17 (`Processor yield`, delays)
  is concurrency *within one heap* — cooperative, shared-state, zero-copy.
  Workers are parallelism *across heaps* — preemptive (the OS schedules the
  threads), share-nothing. They are orthogonal and compose: a future S17
  process could sleep in `Worker runLoopWhile:` without blocking its
  siblings.

**Why copy-passing first?** It requires **zero changes to the VM core's
execution model**. Each VM stays strictly single-threaded on its own OS
thread — the interpreter, JIT, GC, inline caches, and every invariant built
across S8–S24 are untouched. The entire feature is: one new primitive group,
a pickler, and channel plumbing in the embedding layer — all of which already
have proven precedents in this codebase (see §2).

## 2. What already exists (the proof this is cheap)

Every load-bearing mechanism is already built and battle-tested:

| Need | Existing mechanism |
|---|---|
| A second VM instance in-process | `VmHandle::boot` — ~20 ms to genesis + full world (measured, `examples/mandel_demo.rs`); heap/code-cache/world all per-`VmState`; multi-VM safety proven by the spawned-VM demo and the per-VM `Arc<VmLiveStats>` metrics design |
| A VM on its own thread, serviced by channels | `gui/src/vm_host.rs` `worker_loop` — requests in, responses out, `WorkerIdle` backpressure marker, main-thread wake |
| Surviving a worker crash without killing the process | S21: `FatalMode::ExitThread` (`pthread_exit`), PROBE foreign-fault `sigsetjmp` recovery, the never-`join()` rule, death detection by channel-disconnect + bounded timeout |
| VM→host emission from a primitive | The `GameSink` pattern (`src/embed.rs`): a trait object installed on `VmState`, primitives emit through it, silently no-op when absent |
| Host→VM data delivery into a doit | The `GameStep` pattern: the host thread submits a constant doit; the data rides a staging slot read back by a primitive |
| Rooting a half-built object graph across GC | `HandleScope`/`HandleArena` (`src/memory/handles.rs`), already used by `alloc.rs`; plus S14's materializer lesson on pending-graph ordering |
| Class-by-name in the receiving VM | `runtime::globals::global_lookup(vm, sym)` (how every doit resolves `OrderedCollection`) |
| Symbol re-interning | `Universe::intern(&mut self, bytes)` (`src/memory/universe.rs:831`) |
| Object shape enumeration for the pickler | `klass.non_indexable_size()`, `heap.indexable_len()`, `byte_at` (`src/oops/`) |
| A free primitive-id block | Game group ends at 215; workers take **220–229** (216–219 left as game-group headroom) |

The one genuinely new component is the **pickler** (§4). Everything else is
assembly of proven parts.

## 3. Architecture

```
                         ┌────────────────────────────────────────────┐
                         │                 one process                │
   main thread (GUI)     │   primary VM thread        worker threads  │
  ┌───────────────┐      │  ┌──────────────────┐     ┌─────────────┐  │
  │ AppKit/pump   │◄─────┼──│ VmState #0        │────►│ VmState #1  │  │
  │ (existing     │ wake │  │ WorkerRegistry:   │ mpsc│ (own heap,  │  │
  │  vm_host      │      │  │  per-worker tx ───┼────►│  JIT, GC)   │  │
  │  drain)       │      │  │  shared inbox rx ◄┼─────│             │  │
  └───────────────┘      │  └──────────────────┘     └─────────────┘  │
                         │        ▲       ▲            ┌─────────────┐ │
                         │        │       └────────────│ VmState #2  │ │
                         │   prims 220-229  (bytes     └─────────────┘ │
                         │                   only!)                    │
                         └────────────────────────────────────────────┘
```

- **The primary VM** is wherever Smalltalk already runs: the GUI's `vm_host`
  worker thread, or the CLI's main thread, or a test's `VmHandle`. It gains a
  `WorkerRegistry` (a new field on `VmState`, like `game_sink`):
  - `workers: Vec<Option<WorkerLink>>` where
    `WorkerLink { tx: Sender<Vec<u8>>, alive: bool }` — the per-worker
    outbound channel (worker id = index + 1).
  - `inbox_rx: Receiver<Envelope>` / `inbox_tx: Sender<Envelope>` — ONE
    shared inbound channel; every worker holds a clone of `inbox_tx`.
    `Envelope { from: u32, corr: u64, bytes: Vec<u8> }` (`corr` = a
    sender-assigned correlation id, §6, so replies can find their
    continuation).
  - `inbox_wake: Option<InboxWakeFn>` + `wake_pending: AtomicBool` — the
    router's wake hook and its coalescing flag (§3.1).
  - The registry holds **no oops and no JoinHandles it ever joins** — only
    channel endpoints and detached thread handles (dropped, never `.join()`ed,
    per the S21 rule).
- **A worker VM** = one OS thread owning a fresh `VmHandle` + the receiving
  end of its inbound channel + a clone of the primary's `inbox_tx`. Its thread
  body is a miniature `worker_loop`:

  ```text
  boot VmHandle (FatalMode::ExitThread, JIT on)
  install its own WorkerRegistry in REPLY mode (self_id = i, inbox_tx = clone)
  loop:
      recv() inbound bytes            (blocks; Err(disconnected) → exit thread)
      stage bytes in the VM's pending-message slot
      exec "Worker dispatchPending."  (the GameStep pattern — constant doit,
                                       data via the staging slot + poll prim)
  ```

  A crash inside `dispatchPending` takes the *thread* (S21 machinery), never
  the process; the registry synthesizes a `#workerDied` envelope (§8).
- **Bytes only.** Nothing but `Vec<u8>` ever crosses a thread boundary. No
  oop is ever visible to two VMs; GC on either side needs no coordination
  whatsoever. This single invariant is what makes the whole design safe.

### 3.1 The event router — how a sleeping primary is woken

The primary spends most of its life asleep, and the design keeps it that way:
**no polling exists anywhere in the system.** The "router" is not a thread or
a component — it is the inbox channel plus a registered wake hook, and the
send itself is the wake event:

```
worker thread:  inbox_tx.send(envelope);
                if !wake_pending.swap(true) { inbox_wake(); }   // coalesced
```

```rust
// src/embed.rs — registered by the embedder, the GameSink pattern again
pub type InboxWakeFn = Arc<dyn Fn() + Send + Sync>;
impl VmHandle { pub fn set_inbox_wake(&mut self, f: InboxWakeFn); }
```

- **In the GUI:** the wake fn pokes the main thread
  (`performSelectorOnMainThread` — the exact `ChannelGameSink::emit`
  send-then-`wake.notify()` pattern already shipping), and the main thread
  submits one `Worker dispatchInbox.` doit to the primary. The primary VM
  thread wakes from its ordinary `request_rx.recv()` sleep, dispatches
  *every* queued envelope serially **between doits** (never mid-doit — the
  strictly-serial invariant holds), and goes back to sleep. If the primary is
  busy in a long doit, the request simply queues; delivery latency is "end of
  current doit", not a timer tick.
- **Headless/CLI:** there is no host loop, so the primary's event loop *is*
  the inbox: `Worker runLoop` blocks in `inbox_rx.recv_timeout()` inside a
  primitive. That block **is the sleep** — the OS channel wakeup is the
  router, with zero latency and zero spin. (This is not "the primary
  waiting" on any particular worker: it is the idle state itself, identical
  in character to `worker_loop`'s own `recv()`.)
- **Coalescing:** a burst of N envelopes produces one wake (`wake_pending`,
  the game pane's `DIRTY`-flag discipline). The flag clears at the *start*
  of a drain; a send racing in after the clear sets it again and produces at
  most one harmless extra `dispatchInbox` — the classic eventfd pattern,
  never a lost wakeup.
- **Worker death rides the same path:** the registry synthesizes the
  `#workerDied` envelope and fires the same wake — one delivery mechanism
  for everything, including failure.

### How a worker boots: the registered boot-closure

The core crate cannot boot a GUI-style world (image boot lives in
`gui/src/world_boot.rs`, on `image_store`). Rather than move that, the
embedder registers **how to make a worker** — the `GameSink` pattern again:

```rust
// src/embed.rs
pub type WorkerBootFn = Arc<dyn Fn() -> Result<VmHandle, VmError> + Send + Sync>;
impl VmHandle {
    pub fn set_worker_boot(&mut self, f: WorkerBootFn);
}
```

- The CLI/tests register `|| VmHandle::boot(opts, world_dir)`.
- The GUI registers its image-boot path (`load_world_from_image`), so a
  worker's world is byte-identical to the primary's (S22's differential
  guarantee).
- No registration → `Worker spawn` fails cleanly (`PrimResult::Fail`), so the
  world class is harmless in any embedding — same posture as the game
  primitives headless.

The closure runs **on the new worker thread** (boot is ~20 ms; `spawn` returns
the id immediately and the worker buffers inbound messages until its boot
completes — the channel is created before the thread starts).

## 4. The pickle: MOP (MACVM Object Pickle)

A self-contained, versioned, tagged binary encoding of one object graph.
Design criteria: preserve **sharing and cycles within a message**, refuse
**unpicklable** things loudly, and stay cheap for the hot cases (byte arrays
and float arrays — the ParallelMandel payload — are `memcpy`).

### 4.1 Wire form

```
message  := MAGIC('M','O','P',1) object
object   := tag payload
varint   := LEB128 unsigned (as scopes.rs already uses)
```

| tag | payload | rebuilt as |
|---|---|---|
| `0` nil | — | the receiver's `nil` |
| `1` false / `2` true | — | singletons |
| `3` SmallInteger | zig-zag varint | immediate |
| `4` Double | 8 bytes IEEE-754 bits | boxed Double |
| `5` Character | varint code point | `Character value:` |
| `6` Symbol | varint len + UTF-8 bytes | **re-interned** (`Universe::intern`) |
| `7` String | varint len + bytes | fresh String |
| `8` ByteArray | varint len + bytes | fresh ByteArray (memcpy) |
| `9` FloatArray | varint n + n×8 bytes | fresh FloatArray (memcpy) |
| `10` Array | varint n + n×object | fresh Array |
| `11` Object | varint classname-len + name + varint nslots + nslots×object [+ varint idx + idx×object if indexable] | `basicNew` of the named class, slots filled positionally |
| `12` BackRef | varint index | the already-rebuilt object #index |
| `13` LargeInteger | sign byte + varint len + magnitude bytes | rebuilt large int |

- **Sharing/cycles:** the pickler keeps an identity→index map (indices
  assigned in first-encounter order over tags 6–11,13); a revisit emits
  `BackRef`. The unpickler keeps the mirror `Vec<Oop>`. A cyclic structure
  (`a at: 1 put: a`) round-trips correctly. Note: `BackRef` to an object
  whose slots are still being filled is exactly the pending-graph case S14's
  deopt materializer solved — the unpickler allocates the object *before*
  descending into its slots (spine-first), so a back-reference always has a
  target oop even mid-cycle.
- **Class resolution (tag 11):** intern the name, `global_lookup`; the found
  global must be a class. Then a **shape check**: the receiving class's
  `non_indexable_size` must match the sender's slot count, and its
  indexability kind must match. Both VMs boot the same world so this holds;
  it is checked anyway (worlds can differ mid-development — a live-compiled
  browser edit in the primary does NOT exist in an already-running worker).
- **Refused at pickle time** (primitive fails; `send:` raises a clean
  Smalltalk error): `BlockClosure`, `Context`, classes/metaclasses
  themselves (pass `#name` instead), `Alien` (a raw pointer is meaningless in
  another heap), `CompiledMethod`, `Worker` handles, and anything whose klass
  is a VM-internal kind. **Depth/size guard:** a configurable cap
  (default 64 MiB, 1 M objects) so an accidental `send: Smalltalk` fails
  fast instead of OOMing.

### 4.2 Where it runs, GC-safely

- **Pickling** runs inside a primitive in the *sender's* VM: a read-only walk
  (plus a Rust-side seen-map keyed by oop address) emitting to a `Vec<u8>`.
  It allocates nothing in the guest heap → no GC can move anything mid-walk.
- **Unpickling** runs inside the *receiver's* VM and does allocate — every
  allocation may scavenge. Two rules, both with existing precedent:
  1. every rebuilt oop lives in a `HandleScope` (`memory/handles.rs`) for
     the duration, so a moving GC rewrites the pickler's working set;
  2. spine-first construction (allocate object, root it, then fill slots),
     which also makes cycles work (above).
- The staging slot (`pending_message: Option<Vec<u8>>` on `VmState`) holds
  **Rust bytes, not oops** — invisible to GC by construction.

### 4.3 Testability as a standalone unit (M0)

Pickle/unpickle are exposed as their own primitives (`primPickle:` →
ByteArray, `primUnpickle:` → object) so the whole format is testable in **one
VM with zero threads**: round-trip every type, cycles, shared substructure,
refusal cases, the shape-mismatch error, and a differential
`x = (Worker unpickle: (Worker pickle: x))` structural-equality sweep over
random graphs. This is the non-disruptive first milestone, same posture as
`asm_preview`/`ffi_gen`.

## 5. The primitive group (ids 220–229)

Follows every game-group convention: validate args, `PrimResult::Fail` on any
misuse (never panic), return `self` where there's nothing to answer, and
no-op/fail harmlessly when the facility isn't wired (no boot-fn, bad id).

| id | selector (world-side) | semantics |
|---|---|---|
| 220 | `Worker class >> primSpawn` | boot a worker via the registered boot-fn; answer its SmallInteger id (≥1); fail if no boot-fn or at the worker cap (default 16) |
| 221 | `primSend: id corr: c bytes: aByteArray` | enqueue an envelope on worker `id`'s channel **and fire the coalesced wake** (§3.1); fail if unknown/dead id. From a *worker*, `id` must be 0 (the primary) — the reply path echoes the inbound `corr` |
| 222 | `primPoll` | next `Envelope` for THIS vm as a 3-slot Array `{fromId. corr. bytes}`, or nil — non-blocking, used *inside* a dispatch that the wake already triggered (never as a poll loop). In the primary: reads the shared inbox. In a worker: reads the staged pending message (set by its host loop) |
| 223 | `primAwaitInbox: timeoutMs` | the headless run-loop's heart: sleep in `inbox_rx.recv_timeout()` until an envelope arrives (→ as 222) or the timeout (→ nil). This block IS the primary's idle sleep — the channel send is the wake; zero spin, zero polling. **Primary-only** (a worker's host loop already sleeps in Rust between doits) |
| 224 | `primTerminate: id` | drop worker `id`'s tx (its thread exits on next recv) and mark it dead; idempotent |
| 225 | `primAlive: id` | boolean; false after death is *detected* (send-failure or died-envelope), not instantly at crash |
| 226 | `primSelfId` | 0 in the primary, i ≥ 1 in worker i — lets shared world code know which side it's on |
| 227 | `primPickle: anObject` | → ByteArray (MOP), or fail (unpicklable/limits) |
| 228 | `primUnpickle: aByteArray` | → object, or fail (bad magic/version, unknown class, shape mismatch, truncated) |
| 229 | *(reserved: worker→worker introduction, bounded-mailbox control)* | |

## 6. The world-side library (`world/47_worker.mst`)

One class, mirroring `GamePane`'s class-var-rooted-handler pattern:

```smalltalk
Object subclass: Worker [
    | id |
    <classVars: Handler ReplyHandler PendingReplies NextCorr CurrentCorr>

    "── spawning (primary side) ──"
    Worker class >> spawn [
        | i | i := self primSpawn. ^self new setId: i
    ]
    setId: anId [ id := anId. ^self ]
    id [ ^id ]

    "── sending: always async, always a copy, never a wait ──"
    send: aPayload [
        "Fire-and-forget; any eventual reply goes to the general onReply:."
        self primSend: id corr: 0 bytes: (Worker pickle: aPayload). ^self
    ]
    send: aPayload onReply: aBlock [
        "The async request pattern: send now, run the continuation when THIS
         message's reply arrives (correlation-id matched). The primary never
         blocks — it sleeps until the router wakes it (§3.1)."
        | c |
        c := Worker nextCorr.
        PendingReplies isNil ifTrue: [ PendingReplies := Dictionary new ].
        PendingReplies at: c put: aBlock.               "GC-rooted class var"
        self primSend: id corr: c bytes: (Worker pickle: aPayload).
        ^self
    ]
    terminate [ self primTerminate: id. ^self ]
    isAlive [ ^self primAlive: id ]

    "── receiving, worker side: the host loop execs `Worker dispatchPending.`
       per inbound message (the GameStep pattern) ──"
    Worker class >> onMessage: aBlock [ Handler := aBlock ]      "GC-rooted"
    Worker class >> dispatchPending [
        | env | env := self primPoll.  env isNil ifTrue: [ ^self ].
        CurrentCorr := env at: 2.        "so reply: can echo it back"
        Handler isNil ifFalse: [
            Handler value: (WorkerMessage from: (env at: 1)
                            payload: (self unpickle: (env at: 3))) ]
    ]
    Worker class >> reply: aPayload [
        "From a worker's handler: answer the primary, echoing the inbound
         correlation id so the sender's continuation fires."
        self primSend: 0 corr: CurrentCorr bytes: (self pickle: aPayload)
    ]

    "── receiving, primary side: dispatchInbox is only ever run because the
       router woke us — it is never called in a poll loop ──"
    Worker class >> onReply: aBlock [ ReplyHandler := aBlock ]
    Worker class >> dispatchInbox [
        "Handle every queued worker→primary envelope: a matching pending
         continuation wins; otherwise the general reply handler."
        | env corr k |
        [ (env := self primPoll) notNil ] whileTrue: [
            corr := env at: 2.
            k := (PendingReplies notNil and: [ corr ~= 0 ])
                ifTrue: [ PendingReplies removeKey: corr ifAbsent: [ nil ] ]
                ifFalse: [ nil ].
            k isNil
                ifTrue: [ ReplyHandler isNil ifFalse: [
                    ReplyHandler value: (WorkerMessage from: (env at: 1)
                                         payload: (self unpickle: (env at: 3))) ] ]
                ifFalse: [ k value: (self unpickle: (env at: 3)) ] ]
    ]

    "── the headless event loop: sleep-until-woken, dispatch, repeat ──"
    Worker class >> runLoopWhile: conditionBlock [
        "A CLI program's main loop after kicking off its sends: sleep in the
         inbox (primAwaitInbox: blocks in recv — the OS wake IS the router),
         dispatch what arrived, until the condition turns false. The GUI never
         calls this — its run loop is AppKit's, fed by the wake hook."
        [ conditionBlock value ] whileTrue: [
            (self primAwaitInbox: 250) isNil ifFalse: [ self dispatchInbox ] ]
    ]
    Worker class >> nextCorr [
        NextCorr isNil ifTrue: [ NextCorr := 0 ].
        NextCorr := NextCorr + 1. ^NextCorr
    ]

    Worker class >> pickle: x [ <primitive: 227> self error: 'unpicklable' ]
    Worker class >> unpickle: b [ <primitive: 228> self error: 'bad pickle' ]
    "…primSpawn/primSend:corr:bytes:/primPoll/primAwaitInbox:/…
       <primitive: 220..226> stubs…"
]

"WorkerMessage: from (0 = primary, else worker id), payload, and
 #workerDied / #workerStarted system tags."
```

**Death notification is just a message:** when the registry detects a dead
worker it synthesizes an envelope whose payload unpickles to
`{#workerDied. id}`, delivered through the same handler — one delivery
mechanism, no special cases (mirrors how the S21 supervisor keys on the
crash-report *message*).

**Worker transcript forwarding:** each worker's `TranscriptSink` is a channel
sink tagging lines `[w1] …` and shipping them as system envelopes; the
primary's drain forwards them to the real transcript. `Transcript show:` in a
worker "just works".

## 7. Delivery semantics (the contract)

1. **Asynchronous end to end.** `send:` never blocks; replies never poll.
   Delivery is event-driven: enqueue → coalesced wake → dispatched between
   doits (§3.1). The primary's idle state is a genuine sleep (`recv()`), and
   an arriving envelope is what ends it. There is **no blocking request
   primitive**: "waiting for a reply" is expressed as a continuation
   (`send:onReply:`), and a CLI program that has nothing else to do sleeps in
   `runLoopWhile:` — which is the idle sleep itself, not a wait on any
   particular worker. Channels are buffered and unbounded (v1; bounded
   mailboxes ride the reserved prim 229 later).
2. **Per-pair FIFO.** Messages primary→worker-i arrive in send order
   (mpsc guarantee); likewise worker-i→primary. **No global order** across
   different workers.
3. **Serial dispatch.** A worker handles one message at a time, to
   completion, between doits — the same single-outstanding discipline as
   `GameStep`. No re-entrancy, ever.
4. **At-most-once.** A message to a worker that dies before dispatch is
   lost; the death notification is the recovery signal. (No acks/retries —
   this is in-process, not a network.)
5. **Copies, not identity.** `x == y` never holds across a boundary;
   mutating a received object never affects the sender's copy. Symbols are
   the one identity-healing case (re-interned, so `#foo == #foo` holds
   *within* each VM as always).

## 8. Failure model

| failure | detection | consequence |
|---|---|---|
| worker guest error / native fault mid-dispatch | S21 machinery ends the *thread*; its channel endpoints drop | primary's next `send:` fails → registry marks dead + synthesizes `#workerDied`; also detected lazily by a periodic registry sweep |
| worker wedged (infinite loop) | none in v1 (a Rust thread can't be safely killed — same stance as `VmHost::restart`) | `terminate` marks it dead and drops its channel; the thread leaks until its current doit ends (documented, like the GUI's abandoned-worker case) |
| primary dies | process exits; workers are process-local threads | everything dies with it — by design |
| unpicklable payload | pickle-time, in the sender | clean Smalltalk error in the sender; nothing sent |
| shape mismatch / unknown class | unpickle-time, in the receiver | the *dispatch* fails for that message; a `#badMessage` system envelope goes back to the sender; worker survives |

## 9. GUI integration (one small pump)

The primary VM in the GUI is `vm_host`'s worker thread, so worker channels
hang off *that* `VmState` with no GUI knowledge. Reactive delivery is the wake
hook from §3.1, wired at boot alongside `set_game_sink`:

```rust
// gui boot: the wake fn pokes the main thread; the main thread submits the
// dispatch doit. Both hops are the existing ChannelGameSink machinery.
vm.set_inbox_wake(Arc::new(move || wake.notify_worker_inbox()));
// main-thread handler:  VM.submit(Doit { "Worker dispatchInbox." })
```

End to end: worker `send` → coalesced main-thread poke → one queued doit →
the primary wakes from `recv()`, runs `dispatchInbox` (all queued envelopes,
continuations first), sleeps again. **No timer is involved and nothing polls**;
latency is one runloop turn (µs–ms) when idle, or "end of the current doit"
when busy — the same delivery character as every game/transcript event today.
A slow supervisor sweep (piggybacked on the existing metrics tick) exists only
as the *dead-worker* fallback detector, mirroring `WORKER_RESPONSE_TIMEOUT` —
it plays no role in message delivery. Headless embeddings need none of this:
`runLoopWhile:` sleeps directly in the inbox (§5, prim 223).

## 10. Verification plan (per the standing rules)

- **M0 unit:** MOP round-trip per type; cycles; shared substructure
  (`{a. a}` unpickles to two references to ONE object); refusal of blocks/
  contexts/aliens/classes; size caps; truncated-input fuzz (must fail, never
  panic); differential structural-equality sweep on random graphs.
- **M1 integration (headless, `embed.rs` style):** spawn an echo worker,
  ping-pong 10k messages **through `runLoopWhile:`/`primAwaitInbox:` with no
  polling anywhere**, assert order + content + that every wake was coalesced
  (wake count ≤ envelope count, ≥ drain count); correlation ids route each
  reply to its own continuation with interleaved workers; spawn-to-cap;
  terminate + re-spawn; `GC_STRESS` on both sides while unpickling large
  graphs (the HandleScope proof).
- **M2:** worker that `error:`s mid-dispatch → primary gets `#workerDied`,
  process survives, remaining workers unaffected (the S21 test, N-wide);
  worker Transcript forwarding.
- **Soak:** N=8 workers × 100k mixed-size messages, RELEASE + PARALLEL +
  threshold≥200 *and* t=1 (the standing stress matrix), plus the full-GC
  cascade running in the primary throughout.
- **Perf markers:** ping-pong round-trip latency; pickle throughput MB/s for
  ByteArray/FloatArray (should be ~memcpy); ParallelMandel frames/s vs
  worker count 1/2/4/8 (interleaved A/B runs — fanless-Air rule).

## 11. Milestones

| # | contents | size | gate |
|---|---|---|---|
| **M0** ✅ | MOP pickler/unpickler + prims 227/228, one-VM round-trip suite (1ea6984) | `M` | every §10-M0 test green; zero other code touched |
| **M1** ✅ | `WorkerState` on `VmState`, `set_worker_boot`, worker thread loop, prims 220–226, `world/47_worker.mst` (c95f0be) | `M` | headless echo ping-pong + crash-isolation tests green |
| **M2** ✅ | `send:onReply:` continuations + dispatchInbox, `runLoopWhile:`, died-notification, `[wN]`-tagged transcript forwarding, spawn cap | `M` | in-language echo + died-notification + forwarding tests, event-driven end to end |
| **M3** ✅ | GUI wake hook: `set_inbox_wake` queues a `VmRequest::WorkerInbox` from the worker's thread → the primary VM thread's own `recv()` sleep services a silent `Worker dispatchInbox.` (no main-thread hop needed) | `S` | vm_host test: a reply reaches its handler with no timer, no manual dispatch |
| **M4** ✅ | **ParallelMandel** (`world/48`): each frame of the dive split into N bands, one per worker VM, primary assembles + blits; Demos menu item | `M` | headless gate: full frame, every band computed by its worker, recognizable set |

**As-built amendments:** `Worker spawn: initSource` — the spawn primitive (220)
takes an init doit run ONCE in the fresh worker, which is how its
`onMessage:` handler gets installed (the design's bare `spawn` is sugar for
`spawn: nil`). Envelopes surface guest-side as 3-slot `{from. corr. bytes}`
Arrays. `Dictionary` has no `removeKey:`, so fired continuations tombstone
their entry with nil (correlation ids are unique — never rekeyed). Control
messages (`#workerDied`, `#workerTranscript`) are hand-encoded MOP built
Rust-side (`mop::encode_worker_*`) and route in `Worker>>dispatchOne:`.

M4 is the capstone for the same reason Catcher/Breakout were: it exercises
everything at once — spawn, big ByteArray payloads both ways, assembly,
the pump, teardown — and it's *visible*. Today MandelZoom computes its
320×240 frame single-threaded; N workers ≈ N× the pixels per second on a
compute-bound dive, on screen.

## 12. Future work (explicitly deferred)

- **Worker↔worker** channels via a primary-brokered introduction (prim 229).
- **Bounded mailboxes / backpressure** for streaming workloads.
- **A shared immutable arena** (bytes-only, e.g. big read-only tables) —
  the first deliberate crack in share-nothing, only if profiling demands it.
- **Pool sugar:** `aCollection parCollect: aBlock` over a worker pool — needs
  block *sources* (not closures) to cross the boundary, i.e. send the block's
  source text and compile it worker-side; a natural follow-on once
  `sourceCompile:` meets MOP.
- **Remote transport:** MOP is already position-independent bytes; a socket
  transport + handshake versioning would make workers distributable.

## 13. Cross-references

- `docs/SPEC.md` §11 (concurrency deferred by design) — unchanged: each heap
  remains strictly single-threaded; this design adds parallelism *between*
  heaps, not concurrency within one.
- `docs/SPRINTS.md` Phase E S17 (green processes) — orthogonal, composes.
- README "Why there's no `become:`" — copy semantics across heaps is the
  same philosophy: identity never crosses; direct pointers stay fast.
- S21 (`docs/vm_handle.md`, `src/embed.rs`) — the crash-safety substrate.
- S22 (`docs/IMAGE.md`, `docs/managingtheworld.md`) — worker worlds boot from
  the same image as the primary via the registered boot-closure.
- `docs/gamepane_design.md` — the sink/staging-slot/single-outstanding
  patterns this design reuses wholesale.
