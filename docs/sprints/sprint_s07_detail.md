# Sprint S7 — Scavenger + write barrier

**Objective.** The young generation collects: address-space layout per SPEC
§7.1, Cheney copying scavenger with forwarding-in-the-mark-word per SPEC §2.2
and §7.3, card-table write barrier through one store choke point per SPEC §7.4,
Handle/HandleScope discipline per SPEC §7.6, and the GC-stress + verification
machinery of SPEC §12.3–12.4. After S7, allocation is no longer mortal;
long-lived data accumulates in old gen (reclaimed in S8).

SPEC sections implemented: §2.2 (forwarding), §7.1, §7.2 (slow path), §7.3,
§7.4, §7.6, §12.3 item 4 (GC torture, scavenge mode).

## Prerequisites

From S0–S6 (all exist and are green before S7 starts):
- `src/oops/` (or flat `src/oops.rs` — convert to a directory this sprint per
  CONVENTIONS §1): `Oop`, typed wrappers (`MemOop`, `KlassOop`, `ArrayOop`,
  `ByteArrayOop`, `SymbolOop`, `MethodOop`, `ClosureOop`, `ContextOop`,
  `DoubleOop`), `Mark` pack/unpack with age/hash/tagged_contents fields and
  forwarding discrimination (`(mark & 3) == 0b01` ⇒ forwarded), `Format` enum,
  `src/oops/layout.rs` constants.
- `src/memory.rs` (S1): eden bump allocator over a single mmap, aborts when
  full; `Universe::genesis()`; symbol table; identity-hash counter. S7
  replaces the S1 arena with the full layout — convert to `src/memory/`.
- `src/runtime/`: `VmState` (no globals), primitives table, lookup cache.
- `src/interpreter/`: dispatch loop, `ProcessStack` (contiguous `Vec<Oop>`,
  frame layout per SPEC §5.1), send protocol, ICs, closures/Contexts/NLR.
- `src/frontend/`: source compiler; `world/` library + SUnit-lite suite
  (the S7 gate reruns all of it under stress).

## Deliverables

- `src/memory/reservation.rs` — `Reservation` (mmap PROT_NONE reserve /
  commit / decommit; Box-backed test fallback).
- `src/memory/layout.rs` — heap geometry, `SpaceBounds`, the `is_old` boundary.
- `src/memory/spaces.rs` — `Eden`, `SurvivorSpace`, `OldGen` + old allocation.
- `src/memory/cards.rs` — lock-free `CardTable` (AtomicU8, 512-byte cards,
  dirty = 0), `record_multistores`.
- `src/memory/offsets.rs` — old-space object-start offset table.
- `src/memory/store.rs` — `memory::store`, the single store choke point.
- `src/memory/scavenge.rs` — Cheney scavenger, `AgeTable`, adaptive tenuring.
- `src/memory/handles.rs` — `HandleArena`, `HandleScope`, `Handle<T>`.
- `src/memory/stall.rs` — `GcStallError`.
- `src/memory/verify.rs` — debug heap verifier + cross-check passes.
- `src/memory/stats.rs` — `GcStats`.
- `MACVM_GC_STRESS=1` mode; `MACVM_DBG_OOP=<addr>` trace hook;
  `MACVM_TRACE=gc` output.
- Handle-discipline retrofit across S1–S6 runtime code (categories in
  Algorithms A8).
- `world/tests/gc_stress_test.mst` (see tests file).

## Design

### Data structures

All constants (card shift, space sizes, poison patterns, mark-word shifts) go
in `src/oops/layout.rs` / `src/memory/layout.rs` — single source of truth
(CONVENTIONS §2).

```rust
// src/memory/reservation.rs  — the MacNCL `Backing` pattern, proven on macOS.
pub struct Reservation {
    base: NonNull<u8>,            // reservation base, page-aligned
    len: usize,                   // total reserved bytes
    committed: Mutex<RangeSet>,   // committed sub-ranges; commit is idempotent
    test_box: Option<Box<[u8]>>,  // Box-backed fallback: small deterministic
}                                 //   test heaps, no mmap, commit = no-op

impl Reservation {
    /// mmap(len, PROT_NONE, MAP_PRIVATE|MAP_ANON). No memory is committed.
    pub fn reserve(len: usize) -> io::Result<Reservation>;
    /// Deterministic small heap for unit tests (8-byte aligned Box).
    pub fn test_heap(len: usize) -> Reservation;
    /// mprotect(PROT_READ|PROT_WRITE) over [offset, offset+len), rounded OUT
    /// to the host page size (16 KiB on Apple Silicon — never assume 4 KiB).
    /// Idempotent: re-committing a committed range is a no-op (mutex-guarded
    /// range bookkeeping, exactly newgc-core's Backing::commit).
    pub fn commit(&self, offset: usize, len: usize) -> io::Result<()>;
    /// madvise(MADV_FREE) then mprotect(PROT_NONE). Idempotent.
    pub fn decommit(&self, offset: usize, len: usize) -> io::Result<()>;
    pub fn base(&self) -> usize;
    pub fn len(&self) -> usize;
}
```

```rust
// src/memory/layout.rs
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SpaceBounds { pub start: usize, pub end: usize }   // byte addresses
impl SpaceBounds { pub fn contains(self, addr: usize) -> bool; pub fn len(self) -> usize; }

pub struct HeapLayout {
    pub reservation: Reservation,   // one 8 GiB PROT_NONE reservation (tunable)
    pub eden:  SpaceBounds,         // 4 MiB   (tunable via VmOptions)
    pub from:  SpaceBounds,         // 512 KiB
    pub to:    SpaceBounds,         // 512 KiB
    pub old:   SpaceBounds,         // reserved max old range; committed lazily
    pub old_start: usize,           // == old.start — THE generation boundary
}
impl HeapLayout {
    /// Single-compare generation test. Works on TAGGED oops unchanged:
    /// Mem_Tag = 1 biases an 8-byte-aligned address by +1, which can never
    /// cross the page-aligned old_start boundary.
    #[inline] pub fn is_old(&self, oop_or_addr: usize) -> bool { oop_or_addr >= self.old_start }
    #[inline] pub fn is_new(&self, oop_or_addr: usize) -> bool { oop_or_addr <  self.old_start }
}
```

Layout low→high per SPEC §7.1: `[eden][from][to][old…]`. Eden and both
survivors are committed at boot. Old gen commits one 16 MiB segment at boot;
growth is S8 (S7: old exhaustion → `GcStallError`).

```rust
// src/memory/spaces.rs
pub struct Eden          { pub bounds: SpaceBounds, pub top: usize }
pub struct SurvivorSpace { pub bounds: SpaceBounds, pub top: usize }
pub struct OldGen {
    pub bounds: SpaceBounds,       // reserved max
    pub committed_end: usize,      // grows by segment (S8)
    pub top: usize,
}
impl OldGen {
    /// The ONLY way memory enters old gen (direct large alloc + promotion).
    /// Updates the offset table for every card the new object spans, and
    /// nil-fills the body (MacNCL lesson 5). Returns None when full — the
    /// caller decides the cascade step; no silent capping (lesson 4).
    pub fn allocate(&mut self, offsets: &mut OffsetTable, size_bytes: usize) -> Option<usize>;
}
```

```rust
// src/memory/cards.rs — MacNCL's lock-free card table, polarity flipped.
pub const CARD_SHIFT: u32 = 9;            // 512-byte cards (SPEC §7.4)
pub const CARD_SIZE:  usize = 1 << CARD_SHIFT;
pub const CARD_DIRTY: u8 = 0;             // dirty = 0 ⇒ `strb wzr` in S11
pub const CARD_CLEAN: u8 = 1;

pub struct CardTable {
    backing: Reservation,          // one byte per card over old's reserved max;
    old_start: usize,              //   committed in lockstep with old segments
    n_cards: usize,
}
impl CardTable {
    fn entry(&self, i: usize) -> &AtomicU8;        // internal
    #[inline] pub fn card_index(&self, slot_addr: usize) -> usize
        { (slot_addr - self.old_start) >> CARD_SHIFT }
    #[inline] pub fn dirty_for_slot(&self, slot_addr: usize)
        { self.entry(self.card_index(slot_addr)).store(CARD_DIRTY, Relaxed); }
    /// Dirty every card overlapping [start, end) — used by promotion (SPEC
    /// §7.3 step 2) and future bulk primitives (replaceFrom:to:with:).
    pub fn record_multistores(&self, start: usize, end: usize);
    pub fn is_dirty(&self, i: usize) -> bool;
    pub fn set_clean(&self, i: usize);
    pub fn card_base(&self, i: usize) -> usize;
}
```
Single mutator thread ⇒ `Relaxed` everywhere; `AtomicU8` is kept anyway so the
same table works unchanged if parallel mutators ever appear, and because it is
the exact shape proven in MacNCL.

```rust
// src/memory/offsets.rs — object-start table, one u8 per old-gen card.
pub struct OffsetTable { backing: Reservation, old_start: usize, n_cards: usize }
```
Entry semantics for card *i* (must let a dirty-card scan find the header of
the object covering `card_base(i)`):
- `0 ..= 64` — that object's header is at `card_base(i) − entry*8` bytes
  (0 = header exactly at the card base; up to one full card back).
- `65 ..= 255` — back-skip `entry − 64` cards (1..=191) and re-consult that
  card's entry; entries chain for objects spanning > 191 cards (256 KiB direct
  allocations span up to 512 cards — the chain is exercised, test it).

Maintenance: `OldGen::allocate` sets entries for every card whose base falls
inside the new object (`[c0+1 ..= c_last]`, plus `c0` itself iff the header
sits exactly at `card_base(c0)`). Entries for cards beyond `old.top` are
meaningless and never consulted (scans are bounded by `top`).

```rust
// src/memory/store.rs — SPEC §7.4, verbatim. THE choke point (lesson 4:
// keep it single; every heap field/element store in the VM goes through it).
#[inline]
pub fn store(vm: &mut VmState, obj: MemOop, word_offset: usize, val: Oop) {
    // debug_assert: word_offset within object size; obj not forwarded.
    let slot = obj.field_addr(word_offset);        // untagged address
    unsafe { *(slot as *mut Oop) = val; }
    if vm.heap.layout.is_old(obj.addr()) && val.is_mem() && vm.heap.layout.is_new(val.raw() as usize) {
        vm.heap.cards.dirty_for_slot(slot);        // value-conditional (Self's form)
    }
}
```
Frames/operand stacks on `ProcessStack` are Rust-side roots scanned exactly
every GC (SPEC §5.1) — writes to them are *not* heap stores and do not use
`store`. Initializing stores into a freshly allocated object may use `store`
unconditionally (a new object is never old unless directly allocated old, in
which case the nil-fill plus subsequent `store` calls are still correct).

```rust
// src/memory/scavenge.rs
pub struct AgeTable { pub bytes_at_age: [usize; 128] }   // 7-bit age (SPEC §2.2)
impl AgeTable {
    pub fn clear(&mut self);
    pub fn record(&mut self, age: u8, bytes: usize);
    /// Smallest age A such that cumulative surviving bytes of ages 0..=A
    /// exceed survivor_capacity/2; if the total never exceeds it, 127
    /// (= never tenure by age). Strongtalk/HotSpot policy, SPEC §7.3 step 4.
    pub fn compute_threshold(&self, survivor_capacity: usize) -> u8;
}

pub struct ScavengeReport { pub survivor_bytes: usize, pub promoted_bytes: usize,
                            pub new_threshold: u8, pub pause: Duration }

pub fn scavenge(vm: &mut VmState) -> Result<ScavengeReport, GcStallError>;
```

```rust
// src/memory/stall.rs — MacNCL's GcStallError analog (lessons 6, 7).
// Constructed from cycle 0; every allocation failure carries one. Never panic.
#[derive(Debug)]
pub struct GcStallError {
    pub requested_bytes: usize,
    pub phase: GcPhase,                    // Mutator | ScavengeCopy | ScavengePromote
    pub eden_used: usize,  pub eden_capacity: usize,
    pub from_used: usize,  pub to_used: usize, pub survivor_capacity: usize,
    pub old_used: usize,   pub old_committed: usize, pub old_reserved: usize,
    pub scavenge_count: u64, pub full_gc_count: u64,          // full = 0 until S8
    pub last_survivor_bytes: usize, pub last_promoted_bytes: usize,
    pub last_reclaimed_bytes: usize,       // progress numbers: is GC helping?
}
impl Display for GcStallError { /* one readable block, printed on fatal exit */ }
```

```rust
// src/memory/handles.rs — SPEC §7.6.
pub struct HandleArena { slots: Vec<Oop> }        // Boxed inside VmState so its
                                                  // address is stable
pub struct HandleScope<'vm> {
    arena: NonNull<HandleArena>,
    saved_len: usize,
    _vm: PhantomData<&'vm ()>,                    // documents intent; scopes are
}                                                 // strictly LIFO (debug-checked)
impl<'vm> HandleScope<'vm> {
    pub fn enter(vm: &mut VmState) -> HandleScope<'_>;
    /// Push val into the arena; returns an index-based handle bound to this
    /// scope's lifetime. &mut vm is passed per call (the scope itself holds
    /// no borrow of VmState, so allocation calls can interleave).
    pub fn handle<T: OopRepr>(&self, vm: &mut VmState, val: T) -> Handle<'_, T>;
}
impl Drop for HandleScope<'_> { /* truncate arena.slots to saved_len */ }

#[derive(Copy, Clone)]
pub struct Handle<'s, T> { index: u32, _s: PhantomData<&'s ()>, _t: PhantomData<T> }
impl<'s, T: OopRepr> Handle<'s, T> {
    pub fn get(self, vm: &VmState) -> T;          // ALWAYS re-read after any
    pub fn set(self, vm: &mut VmState, val: T);   //   call that can allocate
    pub fn oop(self, vm: &VmState) -> Oop;
}
```
`OopRepr` is a small trait implemented by `Oop` and every typed wrapper
(`fn as_oop(self) -> Oop; unsafe fn from_oop_unchecked(Oop) -> Self`). Handles
are **indices**, never pointers — `Vec` growth relocates the slots (do not
cache `&Oop` into the arena). GC scans and rewrites `arena.slots[..len]` as
roots; **GC never truncates the arena** (only scope Drop does — lesson 9).

> **SPEC-QUESTION:** SPEC §7.6 says raw-`Oop` locals are "poisoned by a
> collection". Rust cannot rewrite live locals. S7 implements the enforceable
> equivalent: after every collection (debug builds), eden and from-space are
> filled with `POISON = 0xF0F0_F0F0_F0F0_F0F3` (tag 11, sentinel 0 — an
> impossible mark), so any dereference through a stale oop trips the typed
> wrappers' `debug_assert!` tag checks immediately and deterministically under
> `MACVM_GC_STRESS=1`. Suggest amending §7.6 wording to "the spaces a stale
> oop points into are poison-filled".
>
> **RESOLUTION (S7-11, 2026-07-02): this was never actually built.** "S7
> implements" above describes the intended design, not committed code —
> `POISON` did not exist anywhere in `src/` until S7-11 added it
> (`oops::layout::POISON`, `memory::scavenge::poison_range`), a full
> project-age later. In its absence, every stale-handle bug in the interim
> (S7-9, S7-10) was found as an actionable-but-confusing `to`-space bounds
> panic several calls removed from the real misuse, not the immediate,
> pinpointed failure this section promises. Wording is amended in SPEC.md
> §7.6 as suggested, plus a longer post-mortem: poisoning alone is a
> detection backstop, and once it actually ran it immediately surfaced that
> the same "bare oop-wrapper local held across an allocation" bug is
> structural, not a one-off — `Handle<T>` is used as a function-internal
> convenience throughout this codebase (one `pub fn` signature total: its
> own constructor) rather than as API-boundary currency the way V8/JNI use
> their handle types, so allocating functions' own signatures (starting
> with `install_method` right below) invite the exact bug this section
> warns about at every call site. See SPEC.md §7.6's S7-11 amendment (A21)
> for the full finding and the corrective direction — reshaping allocating
> functions to take/return `Handle<T>` at their boundary, not yet scheduled.

```rust
// src/memory/verify.rs — debug cross-check verifier (lesson 8).
pub enum VerifyPoint { ScavengeEntry, ScavengeExit, FullGcEntry, FullGcExit, Manual }
pub fn verify_heap(vm: &VmState, at: VerifyPoint) -> Result<(), VerifyError>;
pub fn verify_object(vm: &VmState, obj: MemOop) -> Result<(), VerifyError>;
pub fn dbg_oop_trace(vm: &VmState, phase: &str);   // MACVM_DBG_OOP hook (lesson 11)
```
Runs at every `VerifyPoint` when `debug_assertions || MACVM_GC_VERIFY=1`.

```rust
// src/memory/stats.rs
pub struct GcStats {
    pub scavenge_count: u64, pub full_gc_count: u64,
    pub total_scavenge_pause: Duration,
    pub bytes_allocated: u64, pub bytes_copied: u64, pub bytes_promoted: u64,
    pub tenuring_threshold: u8,
    pub context_allocs: u64,           // S14 asserts elision against this
}
```

`VmState` additions: `heap: Heap { layout, eden, from, to, old, cards,
offsets }`, `handle_arena: Box<HandleArena>`, `age_table: AgeTable`,
`tenuring_threshold: u8`, `gc_stats: GcStats`, `universe_ready: bool`,
`in_gc: bool`, `dbg_oop: Option<Cell<usize>>` (address updated as the traced
object moves), `VmOptions { gc_stress, eden_kb, heap_mib, verify, .. }`.

### The S7 invariants list (frozen before bring-up — MacNCL lesson 12)

Debug against this list, not against symptoms. The verifier checks 2, 4, 5,
7, 9 mechanically; the rest are enforced by asserts in the scavenger.

1. Every reachable object is copied exactly once per scavenge; forwarding is
   installed in the from-copy's mark before any second visit can observe it.
2. After a scavenge, no live slot anywhere (roots, handles, stacks, old gen,
   to-space) contains an oop into eden or from-space.
3. A slot is only ever rewritten to a forwardee whose bytes are fully copied
   (copy completes before the forwarding mark is published).
4. The remembered set is complete after every scavenge: every old-gen slot
   holding a new-gen oop lies in a card marked dirty.
5. Card table and offset table agree with the heap: every offset-table chain
   consulted for a card below `old.top` resolves to a real object header, and
   walking objects from it covers the card.
6. Handle-arena slots are updated by every collection; the arena is never
   truncated by GC (only by `HandleScope::drop`).
7. Every object has mark tag `11` with sentinel 1, or (during a collection
   only) forwarding tag `01`; no third state exists.
8. `object_size` computed for an object before and after its move agree.
9. Space invariant: `bounds.start <= top <= committed_end <= bounds.end` for
   every space; all objects 8-byte aligned; no object straddles a space.
10. Cheney invariant: every object below the to-space (and promoted-region)
    scan pointer contains no unforwarded new-gen references.
11. AgeTable totals for a scavenge equal exactly the bytes that survived it.
12. Interpreter frames contain only valid oops and valid smis (`saved_fp`/
    `saved_bci` are smis) — the stack scan may treat every slot as an oop.

### Algorithms

**A1 — Boot layout.** 1. Parse `VmOptions`. 2. `Reservation::reserve(8 GiB)`.
3. Slice bounds low→high: eden (default 4 MiB, `MACVM_EDEN` override for
tests), from, to (512 KiB each), old (rest; `old_start` = old.start).
4. Commit eden+from+to; commit first 16 MiB old segment; commit matching card
and offset table ranges. 5. Debug: fill everything committed with POISON.
Genesis runs with GC *disabled* (`universe_ready = false`; scavenge asserts
`universe_ready` — the half-built metaobject knot must never be scanned);
world loading runs with GC enabled.

**A2 — Allocation slow path** (SPEC §7.2; single choke point `memory::
allocate(vm, size_bytes, format-ish init info) -> Result<MemOop, GcStallError>`):
1. If `MACVM_GC_STRESS=1` and `universe_ready` and `!in_gc`: scavenge first.
2. Bump eden; on fit, return (body nil-filled or byte-zero-filled — never
   trust reclaimed memory, lesson 5).
3. If `size > 256 KiB` *(tunable)*: `OldGen::allocate` directly; on `None` →
   step 6.
4. Scavenge. Retry eden bump.
5. Still no fit → `OldGen::allocate`; on `None` → step 6.
6. Designed cascade exhausted for S7 (survivor overflow → direct promotion →
   old growth *(S8)* → full GC *(S8)*): return `GcStallError` with all counts
   filled (lessons 6, 7). `main` prints it and exits non-zero — no panic.

**A3 — Scavenge** (`scavenge(vm)`):
1. Assert `universe_ready && !in_gc`; set `in_gc`. `verify_heap(ScavengeEntry)`;
   `dbg_oop_trace("scavenge-entry")`. Start pause timer. `age_table.clear()`.
2. Snapshot `old_top_before = old.top` (start of this cycle's promoted region).
   Reset `to.top = to.bounds.start`.
3. **Root scan** — rewrite every root slot via `s = scavenge_oop(vm, s)`:
   a. Universe well-known oops (the macro-generated list, SPEC §3.1).
   b. Symbol table backing store (strong in v1).
   c. Handle arena `slots[..len]`.
   d. Process stacks: every slot of every live frame region (`0..sp`); smis
      pass through `scavenge_oop` unchanged (invariant 12).
   e. Dirty-card scan (A6) over `[old.bounds.start, old_top_before)`.
   f. (nmethod oops — none until S9; the root walk is a single
      `for_each_root(vm, f)` so later sprints add sources in one place.)
4. **Cheney loop** — two scan pointers: `to_scan` from `to.bounds.start`,
   `old_scan` from `old_top_before`. While `to_scan < to.top ||
   old_scan < old.top`: scan the object at whichever pointer lags its top
   (either order is correct; alternate), advancing by `object_size`.
   Promotion during scanning grows `old.top`, re-entering the loop — the
   fixed point is both pointers reaching their (moving) tops.
5. Swap `from`/`to` (swap the structs' bounds/top). Reset eden top and the
   now-empty survivor's top. Debug: POISON-fill eden and the empty survivor.
6. `tenuring_threshold = age_table.compute_threshold(survivor_capacity)`.
7. Update `GcStats`; `verify_heap(ScavengeExit)`; `dbg_oop_trace
   ("scavenge-exit")`; clear `in_gc`; return report.

Edge cases: *empty from-space* (first scavenge — loop still correct);
*to-space exactly full* (`top == end` is legal; the overflow test is
`new_top > end`); *zero roots into new gen* (loop exits immediately).

**A4 — Copy one object** (`scavenge_oop(vm, oop) -> Oop`):
1. If `oop` is not a mem oop, or `is_old(oop)` → return unchanged.
2. Read mark. If forwarded → return forwardee.
3. `size = object_size_for_copy(vm, oop)` (A5 — the subtle one).
4. Read `age` from the mark. Destination: if `age + 1 >= tenuring_threshold`
   → promote; else try `to`-space bump; if `to` overflows → promote (survivor
   overflow is the first cascade stage, lesson 6). Promote =
   `OldGen::allocate(size)`; on `None` → `GcStallError{phase: ScavengePromote}`.
5. `ptr::copy_nonoverlapping(src, dst, size)` — full object incl. header.
6. Rewrite the *copy's* mark: `age += 1` (saturate at 127), all else
   preserved (hash travels — identity-hash stability). `age_table.record
   (new_age, size)` for survivor-space copies (promoted bytes are counted in
   stats, not the age table — the table drives the *survivor* occupancy rule).
7. Install forwarding in the from-copy: overwrite mark word with the
   forwardee oop (tag 01). The body of the from-copy is NOT destroyed —
   A5 relies on this.
8. If promoted: `cards.record_multistores(dst, dst+size)` — its slots may
   point at new-gen survivors; next scavenge's dirty-card scan picks them up
   (this cycle scans it via `old_scan`). Also update the offset table
   (inside `OldGen::allocate`).
9. If `dbg_oop` matches `oop`: log the move, retarget `dbg_oop` to the copy.
10. Return the forwardee oop.

**A5 — Sizing and the klass-first scan protocol.** MACVM sizes a `Slots`
object via `klass.non_indexable_size` (SPEC §2.3), and the klass itself may
be a new-gen object that is being moved. Two distinct rules:

- **Sizing during copy** (`object_size_for_copy`): read the object's klass
  field; if that klass oop is forwarded, chase the forwarding to the copy;
  otherwise read the original in place — *in both cases only the klass's
  BODY* (`format`, `non_indexable_size`) is read, which forwarding never
  clobbers (only the mark word is overwritten, and copies are complete
  before publication). **Never recursively copy a klass just to size an
  object.** Rationale: the metaobject knot is cyclic (`Metaclass`'s klass is
  `Metaclass class`, whose klass is `Metaclass`) — copy-to-size recurses
  forever on it; read-only sizing terminates trivially.
- **Scanning during the Cheney loop**: for the object at the scan pointer,
  scavenge the **klass field FIRST** (slot at offset 8): this copies the
  klass if needed and rewrites the field to the forwardee. Then size and
  format-dispatch the object **via the forwardee** (its body is a complete
  copy). Then scan the body per format. This ordering guarantees the scan
  never dispatches through a from-space klass image that a later phase might
  poison, and it is the rule that makes invariant 2 hold for klass fields.

Body scan per `Format` (smis and non-oops pass through `scavenge_oop`
unchanged, so "scan" = call it on every oop-bearing word):
| Format | Scanned words |
|---|---|
| `Slots`, `Klass` | all `non_indexable_size − 2` body words (smis harmless) |
| `IndexableOops` | named words, `size` smi (harmless), then `size` elements |
| `IndexableBytes` | named words only; skip `[size][bytes…pad]` — padding copied verbatim in A4, never scanned |
| `Method` | named oops (`selector holder literals ics` — literals/ics are ordinary heap Arrays, nothing special) + smi fields; skip bytecode bytes |
| `Double` | none (8 raw bytes; scanning them as oops is a corruption bug) |
| `Closure` | `method`, then `copied[0..ncopied]`; `home_frame_ref` is smi-encoded (process id, FP index, serial — SPEC §5.4), harmless |
| `Context` | home hint + all `size` slots |
| `Process` | deferred (S17); assert-unreachable in v1 |

Objects with `tagged_contents` set may scan every body word without dispatch
(fast path — used heavily by full GC in S8; harmless here).

**A6 — Dirty-card scan** (root source e, over old gen up to `old_top_before`):
1. For each card `i` with `is_dirty(i)` and `card_base(i) < old_top_before`:
2. Resolve the offset-table chain (offsets.rs semantics) to the header of the
   object covering `card_base(i)`.
3. Walk objects forward from that header. For each object overlapping
   `[card_base(i), card_base(i+1))`: scavenge exactly the oop-bearing slots
   (per the A5 table) whose slot addresses fall inside the card window.
   Stop when the next object's start ≥ card end or ≥ `old.top`.
4. Track `still_points_new`: after updating, does any slot in this card still
   hold a new-gen oop (survivors in to-space)? If no → `set_clean(i)`;
   if yes → leave dirty (invariant 4 the cheap way; self-correcting).
Edge cases: object spanning many cards (only the slots inside this card are
scanned — the other cards are dirty independently or don't need scanning);
card window intersecting the free area above `top` (bounded by `top`); byte
part of an `IndexableBytes` object inside the window (never scanned).

**A7 — Store barrier**: exactly the `store` listing above. S11's compiled
fast path must match it bit-for-bit (`lsr xtmp, xslot, #9; strb wzr,
[xcards, xtmp]` behind the new-gen value check — SPEC §7.4); S7 defines the
semantics it must reproduce.

**A8 — Handle retrofit over S1–S6** (the biggest diff of the sprint; the
rule: any Rust local holding an oop **across a possible allocation** goes
into a `Handle`; oops living in frames/process stacks are already roots and
need nothing). Call-site categories to sweep, in order:
1. **Primitives that allocate** (`can_allocate` metadata, SPEC §10):
   `basicNew/basicNew:`, block `value*` (Context allocation), smi overflow →
   LargeInteger construction, Double-boxing results, `replaceFrom:to:with:`,
   stream/string growth helpers.
2. **String/Symbol building**: `print_oop`, `printString` support,
   Transcript formatting — anything concatenating while holding pieces.
3. **DNU `Message` construction** (SPEC §5.3): receiver, selector, args array
   all live across two allocations (Message + Array).
4. **Symbol interning**: the byte source and the table live across the
   Symbol allocation.
5. **Source compiler / world loader**: CompiledMethod assembly (literal
   frame, ics array, bytecode object), class creation/reopening — long
   allocation chains; give `compile_method` one `HandleScope` and handles
   for klass/selector/literal-vec.
6. **Interpreter internals**: Context allocation at method entry and
   `push_closure` — receivers/args are on the process stack (roots), but any
   Rust temporaries (e.g. the method oop held in a local while allocating
   the Context) must be re-read from the frame after allocation or handled.
Procedure: grep every `fn` that takes or returns `Oop`/wrappers and
transitively reaches `memory::allocate`; annotate; convert; then run the
whole S6 suite under `MACVM_GC_STRESS=1` — remaining violations fail fast
and deterministically against POISON.

**A9 — Verifier** (`verify_heap`): walk eden `[start, top)`, from-survivor
`[start, top)`, old `[start, top)`; for each object: mark word valid
(invariant 7 — POISON and forwarding outside GC are errors); klass oop is a
mem oop into committed heap whose own format is `Klass`; body oops per the
A5 table are smis or mem oops into committed live ranges (never into
to-space/POISON areas). Cross-checks (lesson 8 — bugs live in the
*disagreements*): (a) card table vs heap: every old slot holding a new oop
is in a dirty card; (b) offset table vs heap: for every card below `old.top`
the chain resolves to a header and forward-walking covers the card; (c)
handle arena entries are valid oops; (d) process-stack slots are valid oops
or smis. `MACVM_DBG_OOP=<hex-addr>`: `dbg_oop_trace` logs the traced oop's
space, mark decode (age/hash/flags/forwarded→where), and containing-card
dirty state at every phase boundary, and A4 step 9 logs moves.

### Layer boundaries

- `src/memory/` is `unsafe`-permitted (SPRINTS rule 4); nothing outside it
  touches raw heap pointers, `mmap`, cards, or offsets.
- Interpreter/runtime/frontend may call ONLY: `memory::allocate`,
  `memory::store`, `HandleScope`/`Handle`, `scavenge` (via the `gcScavenge`
  primitive), `verify_heap(Manual)`, `GcStats` (read).
- `memory` may not call into the interpreter (no upcalls from GC — no
  finalizers, no dyn callbacks; one concrete collector, lesson 1: **no `dyn`
  heap-backend traits**, no abstraction over "a collector").
- `oops::layout` owns bit constants; `memory::layout` owns geometry.

## Implementation order

1. Convert `src/memory.rs` → `src/memory/` (mechanical; S1 arena becomes a
   temporary shim). `reservation.rs` + unit tests (incl. `test_heap`).
2. `layout.rs` + `spaces.rs`; re-home the S1 eden allocator onto `HeapLayout`;
   boot path A1. All prior tests green (GC still absent).
3. Forwarding + age helpers on `Mark` (extend S0's wrapper if missing).
4. `cards.rs` + `offsets.rs` + `OldGen::allocate` + unit tests (pure math —
   test exhaustively now, they are miserable to debug later).
5. `store.rs`; mechanical sweep replacing every raw field store in
   runtime/interpreter with `memory::store` (barrier is a no-op until
   promotion exists — safe to land alone).
6. `stall.rs`, `stats.rs`, `MACVM_DBG_OOP` plumbing (lesson 11: build the
   trace hook EARLY — before the scavenger, not after the first bughunt).
7. `scavenge.rs`: A4/A5 copy + sizing with unit tests on hand-built objects
   in a `test_heap`; then A3 skeleton + A6; tenuring wired but threshold
   forced to 127 (no promotion) first; then enable promotion.
8. `verify.rs` (A9) — run at every GC entry/exit from day one of scavenger
   bring-up (lesson 12: the invariants list above is frozen NOW).
9. `handles.rs` + retrofit A8, category by category, suite green after each.
10. `MACVM_GC_STRESS=1` in the allocate choke point; drive the whole S6
    suite under it; fix fallout; land `world/tests/gc_stress_test.mst`.

## Pitfalls

- **MacNCL lesson 1**: no `dyn` heap-backend traits. One collector, concrete
  code. Do not "abstract" spaces or barriers behind trait objects.
- **MacNCL lesson 2**: unit tests test GC *mechanics*, not *semantics* —
  MacNCL had 312 passing unit tests "and it didn't work on `(+ 1 2)`". The
  smallest meaningful test is a guest-language workload through the
  production allocator with real scavenges and an exact result oracle.
- **MacNCL lesson 3**: ship the deliberate stressor with an exact-value
  oracle on day one of bring-up (tests file, `gc_stress_test.mst`): a GC
  silently dropping 3 objects per 100K iterations passes every other test.
- **MacNCL lesson 4**: allocator contracts return granted state — no
  silently capped grants, and keep the single choke point single
  (`memory::allocate` and `memory::store` are the only doors).
- **MacNCL lesson 5**: never assume reclaimed memory is zeroed. Fill
  discipline: nil-oop fill for oop bodies at allocation, zero for byte
  bodies, POISON for reset spaces in debug.
- **MacNCL lesson 6**: "reserve more headroom" is a workaround, not a fix.
  Scavenger OOM is the designed cascade in A2/A4 (survivor overflow → direct
  promotion → old growth (S8) → full GC (S8)), never a bumped constant.
- **MacNCL lesson 7**: structured GC-failure errors from cycle 0 —
  `GcStallError` with per-space counts and progress numbers, never a panic
  or a bare "out of memory".
- **MacNCL lesson 8**: memory-safe Rust is the wrong line of defence. Every
  MacNCL bug was DISAGREEMENT BETWEEN SEPARATELY-CORRECT DATA STRUCTURES
  (card table vs offset table vs marks vs handle scopes). The A9 cross-check
  verifier runs at every GC entry/exit in debug builds — that is the actual
  defence.
- **MacNCL lesson 9**: per-sub-collection cleanup must not wipe per-cycle
  state. S7 pre-audits for S8's cascade: the handle arena is never truncated
  by GC; `old_top_before` and the age table are per-scavenge locals/fields
  with documented lifetimes; nothing in scavenge-exit resets state a
  same-pause full GC (S8) will need. (Full audit table lands in S8's doc.)
- **MacNCL lesson 10**: a plausible fix that doesn't move the symptom means
  the real bug is deeper. Re-run the exact reproducer after every fix; keep
  reproducers as regression tests (SPRINTS standing rule 2).
- **MacNCL lesson 11**: build the single-address trace hook EARLY —
  `MACVM_DBG_OOP=<addr>` is implementation-order step 6, before the
  scavenger works, not after the first three-day bughunt.
- **MacNCL lesson 12**: freeze the invariants list BEFORE bring-up — it is
  the section above; debug against it, not against symptoms.
- **MacNCL lesson 13**: Rust containers holding raw oops are invisible roots
  and dangle after any move. Enforce Handle discipline mercilessly (A8);
  POISON-fill makes violations crash deterministically under stress. Audit
  especially: the lookup cache (flushed on full GC only, but it holds
  new-gen method oops — it must either be a scanned root or hold only
  old-gen-safe entries; S7 decision: scan it as a root and rewrite, it is
  cheap), and any `Vec<Oop>` in the frontend.
- **MacNCL lesson 14**: precise-only is a cliff-edge decision — never "just
  conservatively scan for now". If a root source can't be enumerated
  exactly, that is a design bug to fix, not a scan to widen.
- **MacNCL lesson 15**: when survivors look too big, compare marked/copied
  bytes vs the expected working set before touching tenuring policy — the
  problem is almost always in the roots (a stale root keeping garbage
  alive), not in the threshold.
- Apple Silicon pages are **16 KiB** — all `mprotect`/`madvise` rounding uses
  the queried page size; `MADV_FREE` (not `MADV_DONTNEED`) is the effective
  advice on macOS.
- The `is_old` compare works on tagged oops (bias +1 never crosses the
  page-aligned boundary) — but only for *mem-tagged* values; smis must be
  filtered first in the barrier (`val.is_mem()` before the range test).
- `ptr::copy_nonoverlapping` for A4 step 5 (never overlapping in a scavenge);
  full GC's slide (S8) uses `ptr::copy`.
- Do not hold `&`/`&mut` into heap memory across `scavenge_oop` calls in
  the Cheney loop — recompute raw pointers from addresses each step
  (promotion may `commit` and the borrow checker cannot see the aliasing).
- The scavenger runs with `to`-space as the *only* bump destination plus
  `OldGen::allocate` — never allocate through `memory::allocate` inside GC
  (assert `!in_gc` there).

## Interfaces for later sprints

- `pub fn scavenge(vm: &mut VmState) -> Result<ScavengeReport, GcStallError>`
  — S8's cascade calls it; `gcScavenge` primitive wraps it.
- `pub fn for_each_root(vm: &mut VmState, f: &mut impl FnMut(Oop) -> Oop)` —
  S8 marks/rewrites through the same enumeration; S12 adds nmethod sources
  here and nowhere else.
- `memory::store` semantics — S11 compiled barrier must be bit-identical.
- `CardTable::record_multistores` — S8 re-dirtying after compaction.
- `OldGen { committed_end, allocate }` — S8 adds `grow()`.
- `verify_heap(vm, VerifyPoint)` — S8 adds `FullGcEntry/Exit` calls.
- `Handle`/`HandleScope` — used by every later sprint's runtime code.
- `GcStats.context_allocs` — S14's Context-elision gate reads it.

## Out of scope

- Full GC, old-gen growth, compaction, displaced-mark side map → **S8**.
- Card-table reset after compaction → **S8**.
- nmethod/code-cache roots and oop maps → **S9/S12**.
- Weak refs, `near_death`, weak symbol table → **S22**.
- Green processes / multiple process stacks (the root walk already iterates
  "all process stacks"; there is exactly one) → **S17**.
- Compiled-code write-barrier emission → **S11**.
