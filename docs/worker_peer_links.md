# Worker peer links — a worker may message the UI directly

Decision of record, 2026-08-14. The direction, in the author's words: *"the UI
is only meant to talk to cocoa/metal on behalf of VMs. Primary was meant to be
the primary that can spawn workers, this does not mean that workers should hop
through primary to the UI, they should just message the UI."*

## 1. What is true today

The roles are already right and neither changes here:

- **The primary spawns.** `set_worker_boot` makes a VM the primary; it owns
  `links` (id → inbox), an inbox of its own, the boot closure and the epoch.
- **The UI worker is a service.** It is an externally-hosted worker whose
  thread is main, and its whole job is talking to Cocoa/Metal on behalf of
  other VMs.

What is missing is that a worker can address nothing but its parent. One line
says so, and says it is provisional:

```rust
WorkerState::Worker { self_id, to_primary, .. } => {
    if id != 0 {
        return false; // v1: workers talk only to the primary
    }
```
`src/runtime/workers.rs`. So an app VM that wants a window has no way to reach
the UI, and AppSpec A5 (`docs/appspec.md` §3.2) stalls there.

**Relaying through the primary was considered and rejected** — it makes the
primary a participant in traffic it has no interest in, costs a hop on every
frame, and grows a forwarding protocol nobody wants to maintain.

## 2. The change

**A worker may hold links to peers, and an envelope carries the sender's own
inbox so a peer link can be LEARNED rather than registered.**

That second half is what keeps this small. Envelopes move between VMs as Rust
structs down a channel — they are not serialized — so an envelope can carry an
`InboxSender`. A receiver that has been messaged therefore knows how to answer,
with no registry, no name service, and no round trip to the primary.

Three pieces:

1. `Envelope` gains `reply_to: Option<InboxSender>` — the sender's own inbox.
2. `WorkerState::Worker` gains `self_inbox: InboxSender` (so it has something
   to put in `reply_to`) and `peers: Vec<(u32, InboxSender)>`.
3. Delivery into a worker caches `(from, reply_to)` in `peers`. Sending from a
   worker resolves `id` against `peers`, with `id == 0` still meaning the
   parent.

## 3. How the first message finds its way

Learning answers *how do I reply*, not *how do I start*. Two well-known cases,
both filled in by the primary because it is the only VM that knows everyone:

- **A spawned worker is told about the UI.** The primary records which of its
  links is the UI (`set_ui_peer`, called by the Cocoa host after
  `register_hosted_worker`), and `spawn` installs `(ui_id, ui_inbox)` into the
  new worker's `peers` before its first bytecode runs.
- **The UI is told about the new worker.** At the same moment the primary
  sends the UI an *introduction* — an envelope `from` the new worker carrying
  that worker's inbox as `reply_to`, which the UI's ordinary learning rule
  caches. No new mechanism, and it is why the introduction is an envelope
  rather than an API.

After that the two talk directly and the primary is never in the path.

## 4. What this deliberately does NOT do

- **Workers still do not spawn.** Spawning stays the primary's role, exactly as
  the author described it. `WorkerState::Worker` gains links, not a boot fn,
  an epoch or a registry.
- **No general peer discovery.** A worker can reach its parent, the UI, and
  anyone who has messaged it. There is no directory and no "connect to worker
  7" — if a use case wants that, it is a later decision with its own record.
- **The epoch/reaping invariant is untouched.** Peer links are senders, not
  registry rows; a peer link to a dead worker fails its `send` and is dropped,
  exactly as the primary's own links do.

## 5. Why this is safe

A `send` that cannot resolve its target still answers false — the existing
contract for a dead or unknown worker, which Smalltalk already surfaces as
`cannot send (unknown or dead worker)`. Learning is additive: a VM that never
receives a `reply_to` behaves exactly as it does today, which is what keeps
every existing worker test honest about the old paths.

## 6. Verification

- Rust unit tests over the transport: a worker sends to a learned peer; a peer
  link to a terminated worker fails soft; `id == 0` still reaches the parent.
- The world-level proof is AppSpec A5: an app VM sends its spec straight to the
  UI worker, which realizes it, and events come straight back — with the
  primary having done nothing but spawn.
