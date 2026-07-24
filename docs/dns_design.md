# DNS name resolution over the async worker fleet

*`world/75_dns.mst`. Zero new Rust: the resolver is S20 FFI + world/61's
NativeBuffer/Alien, and the async face is O0-O3 supervision + the stock
worker RPC service. The design slots into the same ladder as sockets
(`sockets_design.md`) and async I/O (`asyncio_design.md`).*

## 1. Why a worker, not a kqueue

`getaddrinfo(3)` **blocks** — against a dead resolver it blocks for
seconds — and it is not fd-based, so the IoWorker's kevent readiness
model cannot host it (there is nothing to watch). The worker model's
first law (the primary VM never blocks) therefore dictates the shape:
the blocking call runs to completion on a dedicated worker VM's thread,
and the primary's continuation fires when the reply lands. This is the
classic "async DNS" dodge every runtime uses (glibc's
`getaddrinfo_a`, libuv's threadpool) — ours just falls out of
machinery that already exists.

## 2. The blocking half: `Dns` (runs on whichever VM calls it)

Fixed-arity FFI only — `getaddrinfo(node, service, hints, res)` is four
pointers, `inet_ntop(af, src, dst, size)` four registers, so the S20
trampoline calls both directly (the variadic restriction never bites).
New `Posix` bindings: `getaddrinfoNode:service:hints:result:`,
`freeaddrinfo:` (`ret: #v` — void), `gaiStrerror:`, `inetNtop:src:dst:size:`.

`Dns blockingResolve: hostString` stages the name as a C string in a
reused per-VM scratch `NativeBuffer` page, zeroes a 48-byte hints struct
with `ai_socktype = SOCK_STREAM` (dedupes libc's one-entry-per-socket-
type triplication; `ai_family` stays `AF_UNSPEC` so IPv4 and IPv6 both
answer), and walks the returned chain through arbitrary-address
`Alien forAddress:size:` wrappers — the same idiom `Posix errno` uses.
Each node's binary address converts to text with `inet_ntop` into the
scratch page. `freeaddrinfo` runs on success only (on failure `res` is
undefined).

Struct offsets, macOS/arm64 — **verified by a compiled `offsetof`
program, not assumed** (2026-07-24):

| struct addrinfo (48 bytes) | off | | off |
|---|---|---|---|
| `ai_flags` | 0 | `ai_addrlen` | 16 |
| `ai_family` | 4 | `ai_canonname` | 24 |
| `ai_socktype` | 8 | `ai_addr` | 32 |
| `ai_protocol` | 12 | `ai_next` | 40 |

`sockaddr_in.sin_addr` at +4; `sockaddr_in6.sin6_addr` at +8.
`AF_INET` 2, `AF_INET6` **30** (Darwin, not Linux's 10), `SOCK_STREAM` 1,
`EAI_NONAME` 8, `INET6_ADDRSTRLEN` 46. Note Darwin's field order:
`ai_canonname` BEFORE `ai_addr` — the reverse of Linux; the offsets
above are the ground truth this file is written against.

**Failure is a value, not a raise.** NXDOMAIN is a normal outcome, and a
raise on a worker aborts the dispatch and sends NO reply — the caller
would see a timeout instead of the message (`47_worker.mst` v1 rule). So
`blockingResolve:` answers either an `Array` of address strings
(`#('::1' '127.0.0.1')`) or the `{#dnsError. code. message}` shape, with
the message read from `gai_strerror`'s static C string.

## 3. The async half: `DnsService` (primary)

First use spawns a supervised tree: `(WorkerSupervisor named: #dnsSvc
strategy: #oneForOne) superviseNamed: #dns init: 'nil'` — `#permanent`,
so a crashed resolver respawns (recover clean or die). The init doit is
`'nil'` because a stock worker with **no handler installed is already an
RPC server** (`47_worker.mst` §5): the primary ships
`{#rpc. #Dns. #blockingResolve:. {host}}` and the worker performs it by
name — both VMs boot the same world, so `Dns` exists there.

`DnsService resolve:timeoutMs:onReply:onError:` goes through
`ServiceWorker call:` (O3), which buys the whole failure contract for
free: **onError: gets exactly one of** `#timeout`, `#workerDied`, an RPC
error string, **or** the unwrapped `{#dnsError...}` message — and a
normal reply cancels the deadline before `onReply:` sees the address
Array. Nothing on the primary ever blocks; a dead resolver costs the
caller its timeout, not the UI. The two-argument
`resolve:onReply:` convenience defaults to 5000 ms with a Transcript
line on error.

## 4. What this deliberately is NOT

- **Not a caching resolver.** libc (and mDNSResponder behind it) already
  caches; an image-side TTL cache would just go stale differently.
- **Not `getaddrinfo_a`/raw-DNS-over-UDP.** The sockets layer could
  speak port 53 by hand (the ICMP ping already crafts packets), but then
  we own /etc/hosts, mDNS, search domains, IPv6 policy… libc's resolver
  is the OS's; we rent it on a thread we can afford to block.
- **Not per-call worker spawns.** One long-lived `#dns` worker
  serializes resolves (fine — libc serializes internally per thread
  anyway); callers multiplex by correlation id as everywhere else. If
  parallel resolution ever matters, N workers under the same supervisor
  is a config change, not a design change.

## 5. Gates

- CLI smoke (run directly, blocking half): `localhost` →
  `#('::1' '127.0.0.1')`; `'127.0.0.1'` → itself (numeric passthrough);
  `''` → `{#dnsError. 8. 'nodename nor servname provided, or not known'}`.
  All offline-deterministic (/etc/hosts + immediate EAI_NONAME).
- `embed.rs` `dns_service_resolves_on_a_supervised_worker`: the full
  async path — supervised spawn on first use, RPC to the worker, reply
  through the deadline machinery; success funnels the Array (must
  include `127.0.0.1`), the doomed `''` resolve funnels the message
  string to `onError:` with `onReply:` untouched.
