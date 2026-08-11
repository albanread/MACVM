# Alien — bytes that live outside the object heap

*MACVM's foreign-memory type: a length-bounded, GC-safe view over a block of
bytes, whether those bytes are on the Smalltalk heap or somewhere the VM does
not own at all. Implemented in S20 step 5; the design rationale is in
[`FFI.md`](FFI.md) §4 and the module doc of `src/runtime/alien.rs`.*

## 1. What it is, and why it exists

Smalltalk objects live in a **moving** heap: a `ByteArray`'s address is not
stable, so you cannot hand it to a syscall, to a GPU, or to anything that will
still be looking at it after the next collection. `Alien` is the type that
bridges that gap. It is an object — GC-scanned, printable, ordinary — whose
*contents* may be memory the collector has no say over.

Every serious use of foreign memory in this system goes through it: `timespec`
structs for `clock_gettime` (`world/30_date_time.mst`), `mmap`'d pages behind
`NativeBuffer` (`world/61_posix_io.mst`), Accelerate's float lanes
(`world/61a_accelerate.mst`), `getaddrinfo` results (`world/75_dns.mst`), and —
the newest and largest — the GPU framebuffer itself
([`DIRECT_SCREEN.md`](DIRECT_SCREEN.md)).

## 2. Direct and indirect

An Alien is one of two things, and every accessor branches on this once:

- **direct** — the bytes are the object's own indexable tail, on the MACVM
  heap, moved by GC like any `ByteArray` body. Made with `Alien new: n`.
- **indirect** — the bytes are at a raw external address the VM does not own.
  Made with `Alien forAddress: addr size: n`.

The discriminator is a single named field: `external_addr()` is `0` for direct,
and *is the address* for indirect. Nothing else distinguishes them, and the
Smalltalk protocol is identical across both.

```smalltalk
a := Alien new: 16.                          "16 bytes, on the heap"
b := Alien forAddress: somePointer size: 4096. "someone else's page"
```

## 3. The protocol

| selector | prim | notes |
|---|---:|---|
| `Alien class >> new: n` | 119 | a direct Alien of `n` bytes |
| `Alien class >> forAddress: addr size: n` | 120 | an indirect view; `addr` is a SmallInteger |
| `byteAt:` / `byteAt:put:` | 112 / 113 | one byte, 1-based |
| `signedLongAt:` / `signedLongAt:put:` | 114 / 115 | 8 bytes, little-endian, signed |
| `doubleAt:` / `doubleAt:put:` | 116 / 117 | an IEEE double; **the one allocating accessor** |
| `size` | 118 | the declared byte length |
| `replaceFrom:to:with:startingAt:` | 274 | bulk copy from a `ByteArray` |

Indices are **1-based and inclusive**, matching the rest of the collection
protocol. `doubleAt:` allocates (it must box its result in a real `DoubleOop`),
which is why it is the only accessor callers must treat as GC-capable.

`replaceFrom:to:with:startingAt:` is the newest and the reason large uses are
practical at all: every other accessor moves one value, which is right when you
are computing each value anyway and wrong when you already *have* the bytes.
Filling a text page one cell at a time is 6,360 sends; the bulk copy is one
`memcpy`. It is indirect-only — bulk-copying into a moving heap object is a
different question, and not the one it exists to answer.

## 4. Bounds are the point

**Every accessor is bounds-checked against the Alien's declared size, in both
modes.** This is the property that makes it safe to hand one to guest code:

```smalltalk
fb := pane screenMemory.   "an Alien over the framebuffer, exactly its size"
fb byteAt: 999999 put: 1.  "refused — cannot touch what follows the screen"
```

A raw pointer would lose that. It is why `screenMemory` returns an Alien and
not an address, and why a demo with an off-by-one produces a missing pixel
rather than a corrupted heap.

**The refusal is silent.** `byteAt:put:` is `<primitive: 113> ^self` — a failed
primitive falls through to the method body, which answers the receiver. So an
out-of-bounds write does not land *and* does not raise. The memory is
protected; the programmer is not warned. Worth knowing before you spend an
afternoon on a write that seems to do nothing.

## 5. What it does not protect you from

An indirect Alien wraps an address you supplied. If that address is wrong, or
freed, or unmapped, the accessor will dereference it and the process can die.
The bounds check enforces *"within the size you declared"*, not *"the region is
still valid"* — no length check can know that.

So lifetime is a contract, and it belongs to whoever owns the memory:

- **Retract before freeing.** The GUI clears its published framebuffer pointer
  *before* dropping the Metal buffers, so a demo still holding an Alien gets
  `nil` on its next request rather than a dangling view.
- **Stop the writers first.** Retracting is not enough if another thread
  already holds an Alien and is mid-write. `ParallelMandel`'s workers are
  stopped before its buffers go.

Getting that order wrong is not hypothetical: a test that let workers outlive
its buffer crashed the suite with `SIGSEGV far 0x1010101010101018`, in an
unrelated test two positions later.

## 6. Why the shape is what it is

Real Strongtalk's `Alien` encodes direct-vs-indirect by **sign-flipping the
size field** it shares with the object's ordinary indexable size slot. Porting
that literally would have meant teaching `raw_size_slot` / `indexable_len` —
functions every `ByteArray`, `String` and `Symbol` in the VM depends on — to
interpret magnitude via `.abs()`. A small change with a very large blast
radius, for a feature only one type needs.

Instead Alien reuses a mechanism the VM already has and already trusts:
`CompiledMethod` mixes named header fields with a trailing indexable byte tail
in one object, via the generic `nis_words` parameter every klass takes. Alien
is the same shape — `Format::IndexableBytes` with exactly **one** extra named
field for the external address. No changes to `raw_size_slot`,
`indexable_len`, `tail_byte_at` or `tail_start_word`; no new `Format` variant.

One consequence is deliberate and documented: an indirect Alien's declared size
is its *bounds*, and its own heap tail is empty — wrapping an 8 MB framebuffer
costs no VM heap. (An earlier revision allocated a real tail of the same size
and wasted it; that was fixed in 2026-07.)

Its methods are also unusual in where they live: they are compiled at VM boot
from an embedded Rust string constant, not from `world/*.mst`, because Alien's
shape can only be expressed by `Universe::genesis` — the ordinary `subclass:`
parser has no syntax for choosing a `Format` or an `nis_words`. Genesis fixes
the shape; the bootstrap string adds the methods.

## 7. Worked examples in the tree

| file | what it wraps |
|---|---|
| `world/30_date_time.mst` | a 16-byte `timespec` for `clock_gettime` |
| `world/61_posix_io.mst` | `NativeBuffer` — one `mmap`'d page, with typed accessors over it |
| `world/61a_accelerate.mst` | vDSP/vForce float lanes, GC-stable so Accelerate can write them |
| `world/75_dns.mst` | a 48-byte `addrinfo` from `getaddrinfo` |
| `world/45d_plasma.mst` | the GPU framebuffer — 76,800 stores a frame, no copy |
| `world/43a_textscreen.mst` | a text page blasted in with the bulk copy |

## 8. Where the code is

`src/runtime/alien.rs` — the primitives, the direct/indirect split, and the
bootstrap source. `src/oops/wrappers.rs` — `AlienOop` and its
`external_addr` / `indirect_size` accessors. `src/oops/layout.rs` —
`ALIEN_EXTERNAL_ADDR_INDEX` and `ALIEN_NAMED_WORDS`.
