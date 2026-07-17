# Async I/O over the POSIX FFI

**Status:** slice A built + gated 2026-07-16. Roadmap item 3 — turn the worker
model into a *system* (async I/O so I/O never blocks a VM). Pure Smalltalk + the
S20 FFI; **zero new Rust**, by design — this exercises the FFI on its intended
real workload.

## The bet, and the two constraints that shape it

Isolation + many-core + let-it-crash is the Erlang *server* recipe; a server
needs I/O that doesn't block. Two hard constraints decided the whole approach:

1. **The FFI cannot call variadic functions.** Apple arm64 passes variadic args
   on the stack; the trampoline loads registers (`docs/FFI.md` §6.2). So
   `fcntl(fd, F_SETFL, O_NONBLOCK)` — the textbook "make this fd non-blocking" —
   is **out of reach**. Every binding must be fixed-arity.
2. **It must also be unnecessary**, and it is: **`kevent(2)` reports readiness
   *with amounts*.** A returned event's `data` is the readable byte count (for
   `EVFILT_READ`), the writable space (`EVFILT_WRITE`), or the pending-connection
   count (a listening socket). A `read`/`write`/`accept` **bounded by what kevent
   reported cannot block, even on an ordinary blocking fd.** So we never need to
   set `O_NONBLOCK` at all — readiness + bounded ops replaces non-blocking mode.

This is the crux: `kqueue` sidesteps the one thing the FFI can't do, and its
amount-carrying events make blocking fds safe to use without `fcntl`.

## Slice A — the readiness engine (built: `world/61_posix_io.mst`)

Three pure-Smalltalk classes over the FFI:

- **`Posix`** — raw class-side bindings, every one fixed-arity: `kqueue`,
  `kevent` (6 args, the mmap capstone's proven arity), `pipe`, `read`, `write`,
  `close`, `socketpair`, `mmap`, and `errno` (via `__error()` → an indirect
  4-byte Alien). No variadic call anywhere.
- **`NativeBuffer`** — one `mmap`'d anonymous page whose address is (a) a plain
  `SmallInteger` for `#g` syscall args and (b) an indirect `Alien` for byte
  access. Crucially its address is **GC-stable** (external memory, not a heap
  object), so it is safe to hand to a syscall that writes into it — a heap
  object could move under a blocking call. Adds 16/32-bit little-endian
  accessors (Alien natively does 1- and 8-byte) and `struct kevent` /
  `struct timespec` pack/unpack for macOS/arm64.
- **`Kqueue`** — `watchRead:`/`watchWrite:`/`unwatch…:` register fds;
  `pollMs:` returns an `Array` of `{ident. filter. data. flags}` per ready
  event. `EV_EOF` (flags top bit) distinguishes "peer gone" from "nothing yet".

**Gate (`world/tests/44_posix_io_tests.mst`, 6 tests in the it_world suite):**
signed FFI returns (`close(-1) = -1`, `errno = 9`); real bytes across a real
pipe; read readiness with `data` = byte count; write readiness with space;
`EV_EOF` on peer-close. Headless, network-free (pipes + kqueue), verified to
redden when broken.

## Slice B — the `IoWorker` (next; the payoff)

A dedicated **I/O worker VM** running a `kevent`-driven event loop, so no other
VM ever blocks on I/O. Shape:

```
IoWorker loop:
    events := kqueue pollMs: <timeout>.
    events do: [:e |
        route e to the registered continuation for its (fd, filter),
        doing a bounded read/write using e.data,
        and message the requesting worker with the result ].
    service any new watch/unwatch requests from its mailbox.
```

- Requests arrive as ordinary worker messages ("read fd into a reply", "watch
  this listener"); results go back as messages. This rides the **existing**
  worker channel + wake/inbox — no new transport.
- One `IoWorker` can multiplex thousands of fds on one thread (the whole point
  of kqueue), while compute workers run in parallel and never see a blocking
  syscall.
- On error: an fd op that fails is reported as a message (fail-fast at the
  *caller's* choosing), and a wedged `IoWorker` is a throwaway that the
  supervisor respawns — the same `ErrorPolicy::Die` story as any worker. It fits
  the fail-fast philosophy: I/O errors are *values in messages*, not exceptions.

Open questions for slice B (deliberately deferred):
- The blocking-poll problem: `kevent` with a real timeout blocks the IoWorker's
  thread — fine (that thread's whole job is to block on kevent), but the loop's
  cadence vs. servicing its own mailbox needs a design (a self-pipe / the
  existing inbox-wake writing a byte to a watched fd, so a new request wakes the
  poll). This is the one genuinely new bit of engineering.
- Sockets: `socket`/`bind`/`listen`/`accept` are all fixed-arity (already
  reachable); `connect` too. A TCP echo server is the natural slice-B capstone.

## Slice C — a stream library (later)

`Socket`/`FileStream`-style objects over the IoWorker: `readInto:`, `write:`,
`accept`, buffered line reading — the ergonomic surface. Pure Smalltalk on
slice B.

## Reuse map

| need | exists |
|---|---|
| call libc | S20 FFI (`<primitive: FFI …>`, `#g`/`#f` marshaling) |
| external GC-stable memory | `mmap` + indirect `Alien` (the S20 capstone) |
| readiness without `fcntl` | `kqueue`/`kevent` (fixed-arity, amount-carrying) |
| worker transport for IoWorker | the existing worker channel + wake/inbox |
| I/O-worker fault handling | `ErrorPolicy::Die` + supervisor respawn |

## Why this is the right item now

It's a **capability**, not a speculative speedup (contrast escape analysis,
parked on its inlining prerequisite): you either can do non-blocking I/O or you
can't, and the win is unambiguous. It's additive (library + FFI, no VM-core
surgery), it makes the FFI earn its keep, and it's the piece that lets the
worker model reach the workload it was built for — network services.
