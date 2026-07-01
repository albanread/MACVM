# Sprint S8 — Full GC (mark + slide compact)

**Objective.** Unbounded-lifetime programs run: worklist marking, per-space
slide compaction with forwarding through the mark word and a displaced-mark
side map (SPEC §2.2, §7.5), reference rewrite over every root and heap slot,
old-gen segment growth, the scavenge→full-GC allocation cascade, the `gcFull`
primitive, and heap statistics. After S8 the heap has a flat ceiling under
arbitrary churn.

SPEC sections implemented: §2.2 (displaced marks), §7.2 (cascade completion),
§7.5 (all), §12.3 item 4 (`MACVM_GC_STRESS=full[:N]`).

## Prerequisites

All of S7, specifically: `HeapLayout`/spaces/`OldGen`, `CardTable` +
`OffsetTable`, `memory::store`, `scavenge` + `ScavengeReport`,
`for_each_root(vm, f)` (single root enumeration — S8 reuses it verbatim for
mark and rewrite), `Handle`/`HandleScope` (arena never truncated by GC),
`GcStallError`, `verify_heap` with the S7 invariants list, `GcStats`,
`MACVM_DBG_OOP`, POISON fill discipline. The S7 gate (S6 suite under
`MACVM_GC_STRESS=1`) is green.

No nmethods exist yet (S9+): full GC has no code-cache phase in S8; the hook
points are named in "Interfaces for later sprints".

## Deliverables

- `src/memory/fullgc.rs` — mark, forwarding-compute, rewrite, slide, restore.
- `OldGen::grow()` + growth policy; promotion-guarantee check in the cascade.
- Displaced-mark side map; mark-bit definition in `src/oops/layout.rs`.
- Offset-table rebuild during compaction; card-table reset semantics.
- Rust-side address-keyed table flush/update inventory (below) executed.
- `gcFull` primitive (SPEC §10 system group) + `gcStats` dev primitive.
- `MACVM_GC_STRESS=full[:N]` mode; `FullGcEntry/Exit` verifier points.
- `world/tests/gc_full_test.mst`; `world/bench/soak.mst` + `just soak-s08`.

## Design

### Data structures

```rust
// src/oops/layout.rs — NEW constant. The full-GC mark bit lives in the mark
// word's reserved range (SPEC §2.2 bits 63:44). Set only between mark and
// forwarding-compute phases of a full GC; always 0 for the mutator.
pub const MARK_GC_BIT: u64 = 1 << 44;
```

> **SPEC-QUESTION:** SPEC §7.5 requires a "mark bit in the mark word" but
> §2.2 does not allocate one. S8 pins bit 44 (lowest reserved bit). Amend
> §2.2's table: `bit 44: gc_mark (full-GC only, invariant-0 outside GC)`.

```rust
// src/memory/fullgc.rs
pub struct MarkStack(Vec<MemOop>);            // explicit worklist (SPEC Δ:
                                              // no pointer reversal), heap-
                                              // bounded, Vec growth is fine

/// One entry per LIVE object, built during forwarding-compute, consumed by
/// rewrite and slide. This is the answer to "how do we size objects after
/// klass fields are rewritten": we never re-size — sizes are computed once,
/// here, while every klass body is still readable at its old address.
#[derive(Copy, Clone)]
pub struct CompactEntry { pub old: usize, pub new: usize, pub size_words: u32 }

pub struct CompactPlan {
    pub eden: Vec<CompactEntry>,    // per space, in ascending old-address
    pub from: Vec<CompactEntry>,    //   order (slide low→high is overlap-
    pub old:  Vec<CompactEntry>,    //   safe with ptr::copy)
}

/// Displaced-mark side map (SPEC §2.2 Δ): original marks that carry state
/// are parked here during phases B–E, keyed by the object's NEW address.
/// "Carry state" ⇔ hash != 0 || age != 0 || near_death — everything the
/// mark holds that cannot be recomputed. `tagged_contents` is NOT state:
/// it is recomputed from the klass format on restore.
pub struct SideMarks(HashMap<usize /* new_addr */, u64 /* saved mark, gc bit cleared */>);

pub struct FullGcReport { pub marked_bytes: usize, pub live_after: usize,
                          pub reclaimed_bytes: usize, pub pause: Duration }

pub fn full_gc(vm: &mut VmState) -> Result<FullGcReport, GcStallError>;
```

```rust
// src/memory/spaces.rs additions
impl OldGen {
    /// Commit the next segment (16 MiB, tunable) from the reservation, plus
    /// the matching card-table and offset-table ranges. Err ⇒ reservation
    /// exhausted (feeds GcStallError, never a panic).
    pub fn grow(&mut self, cards: &CardTable, offsets: &OffsetTable) -> Result<usize, GrowError>;
    pub fn free_bytes(&self) -> usize;   // committed_end - top
}
```

`VmState` additions: `gc_stress_full_period: Option<u64>` (parsed from
`MACVM_GC_STRESS=full[:N]`, default N = 100 *(tunable)*), allocation counter
for the stress trigger; `GcStats` gains `full_gc_count, marked_bytes_last,
full_pause_total, old_grow_count`.

### Which spaces compact

Per SPEC §7.5 step 2 ("slide live objects toward each space's bottom"), full
GC marks the WHOLE heap and slide-compacts **each occupied space in place**:
eden → eden bottom, from-survivor → from bottom, old → old bottom. New-gen
objects stay new-gen (ages preserved via the side map), old stays old. The
to-survivor is empty between scavenges (assert it). No promotion happens
during a full GC.

> **SPEC-QUESTION:** an alternative reading (Strongtalk practice) evacuates
> the new gen entirely during full GC. SPEC §7.5's per-space wording is
> implemented literally here; if the maintainer prefers empty-new-gen-after-
> full-GC, only the destination computation in phase B changes. Flagging
> rather than improvising.

### Algorithms

Phases run in one pause, strictly ordered A→F. `verify_heap(FullGcEntry)`
before A, `verify_heap(FullGcExit)` after F, `dbg_oop_trace` at every phase
boundary (it also retargets the traced address when its object is assigned a
new address in phase B).

**A — Mark** (explicit worklist; SPEC §7.5 step 1):
1. Assert: no `MARK_GC_BIT` set anywhere (post-GC invariant); no forwarded
   marks (assert sentinel on every mark touched).
2. `for_each_root(vm, f)` with f = *mark-push*: if oop is a mem oop and its
   `MARK_GC_BIT` is clear → set the bit, push onto `MarkStack`, return oop
   unchanged (rewrite happens in phase C, not here).
   Root sources are exactly S7's list (well-known, symbol table, handle
   arena, process stacks, and — no longer as *roots* but as heap — old gen
   is traced, not card-scanned: full GC ignores the card table for marking).
3. Loop: pop obj; scan its klass field + body:
   - `tagged_contents` fast path: every body word is an oop — iterate
     `size_words − 2` words with mark-push, no format dispatch.
   - Else format dispatch per the S7 A5 table (skip byte parts, skip Double
     bodies, Method bytecode, Closure smi ref).
   For each field: mark-push. Set-bit-on-push (not on-pop) so shared objects
   enter the stack once (diamonds, cycles, self-references all terminate).
4. Sizes during marking read klass bodies in place — nothing has moved, no
   forwarding exists yet; the Metaclass 2-cycle is harmless (marking needs
   no copy, S7 A5 rationale).
5. Accumulate `marked_bytes` (lesson 15's diagnostic: compare against the
   expected working set before ever touching policy).

**B — Forwarding compute** (per space: eden, from, old; SPEC §7.5 step 2):
1. Linear walk the space `[bounds.start, top)` object by object (sizing via
   klass bodies — still nothing moved). For each object:
   - marked → assign `new = compaction_pointer` (starts at space bottom),
     `compaction_pointer += size`; push `CompactEntry{old, new, size}`;
     if the original mark carries state (hash ≠ 0 || age ≠ 0 || near_death)
     → `side_marks.insert(new, mark & !MARK_GC_BIT)`;
     overwrite the mark word with the forwarding oop for `new` (tag 01).
     Objects that don't move (`new == old`) are still forwarded (self-
     forwarding keeps phase C uniform).
   - unmarked → skip (dead; its bytes are overwritten by the slide or
     POISONed after).
2. Edge cases: empty space (no entries); zero dead objects (all self-
   forwarded, slide is a no-op); dead object whose klass is also dead
   (klass body still physically present until phase E — sizing is safe).

**C — Reference rewrite** (SPEC §7.5 step 3) — rewrite BEFORE moving, while
every forwarding mark is still readable at the old address:
1. Roots: `for_each_root(vm, f)` with f = *forward-chase*: mem oop with
   forwarded mark → return forwardee; else unchanged. This covers process
   stacks (interpreter-stack fixup: every frame slot; `saved_fp`/`saved_bci`
   are smis, pass through), handle arena, well-known oops, symbol table.
2. Heap: iterate every `CompactEntry` of every space; for the object at
   `entry.old`, rewrite its klass field + each body field (format dispatch /
   `tagged_contents` fast path) to forward-chased values. Sizing is NOT
   needed — `entry.size_words` is authoritative (this is why CompactPlan
   exists: after this phase klass fields point at *new* addresses that are
   not populated until phase D, so nothing may size anything anymore —
   invariant F3 below).
3. Rust-side table inventory — every address-keyed structure, exhaustively,
   as of S8 (audit performed by grepping for `HashMap<`/`Vec<Oop>`/`KlassOop`
   fields outside the heap):

| Structure | Kind | Action in phase C |
|---|---|---|
| Lookup cache (§6.1: `(klassOop, Symbol) → CompiledMethod`, hashed on klass *address*) | cache | **FLUSH** (SPEC §7.5: klass addresses move; rebuilding is lazy and free) |
| Symbol table (§3.1) | root, probed by *content* hash | **UPDATE in place** via `for_each_root` — bucket positions depend on string content, not addresses; do NOT flush (interning identity would break) |
| Universe well-known list | root schema | UPDATE (already in `for_each_root`) |
| Handle arena | root | UPDATE (already in `for_each_root`) |
| Character flyweight table (256 entries) | part of well-known/universe | UPDATE (root walk) |
| Per-klass dependency versions (§6.2) | — | stored IN the klass object (a smi field), moves with it: **nothing to do**. If S3 implemented it as a Rust-side `HashMap<KlassOop, u64>` instead, it must be rebuilt here — check and fix to in-object storage now |
| `dbg_oop` traced address | debug | retargeted in phase B |
| Code table / nmethod keys / PICs | — | do not exist until S9/S11; S12 adds "nmethod literal pools + deps" to this table — the inventory lives here and is append-only |

MethodDictionaries probe on symbol **identity hash**, which lives in the mark
word and travels via the side map — no rehash needed. World-level
Dictionaries hashing on `identityHash` are stable for the same reason (this
is exactly what the identity-hash gate test proves).

**D — Slide** (SPEC §7.5 step 2's move, done after rewrite):
1. Per space, iterate `CompactEntry`s in ascending `old` order:
   `ptr::copy(old, new, size_bytes)` — `ptr::copy` (memmove), NOT
   `copy_nonoverlapping`: an object may slide into a range overlapping its
   old location. Ascending order guarantees `new <= old` per entry, so no
   later source is clobbered before it is copied.
2. Old gen only: **rebuild the offset table** as objects are placed — clear
   all entries first, then per placed object set the entries for every card
   whose base falls inside it (same maintenance rule as `OldGen::allocate`).
   The pre-compaction offset table is garbage after the slide; rebuilding
   during placement is O(live) and keeps invariant 5 true at `FullGcExit`.
3. Set each space's `top = compaction_pointer`; POISON-fill `[top,
   old_top_before_gc)` in debug builds (lesson 5/13).

**E — Restore marks** (SPEC §7.5 step 4):
1. Per `CompactEntry`: the object at `entry.new` currently carries a stale
   forwarding mark (copied bytes). If `side_marks` has `entry.new` → store
   the saved mark. Else → fresh pristine mark: tag 11, sentinel, age 0,
   hash 0, `tagged_contents` recomputed from the (already-rewritten, already-
   slid) klass's format, `near_death` 0.
2. Assert after the pass: no `MARK_GC_BIT`, no forwarding tag anywhere
   (walk in debug). Drop `side_marks` and the `CompactPlan` — their lifetime
   is exactly phases B–E of ONE full GC (lesson 9: never let per-cycle state
   leak across cycles or get wiped mid-cycle by a sub-phase).

**F — Post** (SPEC §7.5 step 4 second half):
1. **Card-table reset semantics**: reset ALL cards to CLEAN, then
   `record_multistores(old.bounds.start, old.top)` — conservatively dirty
   every card of the live old range. Rationale: compaction moved old objects
   that may reference (also-moved) new-gen objects; recomputing exact
   old→new cards costs a heap walk for no benefit — the next scavenge's
   dirty-card scan re-cleans quiet cards (S7 A6 step 4). Cards above
   `old.top` stay clean.
2. Flush the lookup cache (phase C table). Reset the age table.
3. Growth policy check (below). Update `GcStats`; `verify_heap(FullGcExit)`.

Invariant additions for full GC (append to the S7 frozen list; the verifier
checks F1–F3 at `FullGcExit`):
- F1: outside a full GC, no mark word has `MARK_GC_BIT` set and no mark is a
  forwarding word.
- F2: the offset table is exact (not just resolvable) after compaction —
  every entry chain reaches the object actually covering its card base.
- F3: no phase after C computes an object size from a klass field; sizes
  come from `CompactEntry` only (enforced by API: phases D/E take the plan,
  not the heap layout).

### Old-gen segment growth policy

- **When**: (a) `OldGen::allocate` fails during direct large allocation —
  grow, retry once; (b) after a full GC, if `old.free_bytes() <
  old_growth_headroom` (default: 20% of committed *(tunable)*) — grow, so
  the mutator doesn't immediately re-trigger; (c) the promotion guarantee
  (below) cannot be met even after a full GC — grow until it can.
- **How**: commit the next 16 MiB *(tunable)* contiguous segment; commit the
  matching card-table and offset-table byte ranges (both are `Reservation`s
  sized for the full old range — S7 design). `is_old` is untouched (the
  boundary is the reservation slice, not the committed end).
- **Limit**: reservation exhausted ⇒ the cascade's terminal `GcStallError`.

### The cascade (scavenge → full GC in one pause — MacNCL lesson 9)

Design rule: collections are **sequential, never nested**. A full GC never
runs *inside* a scavenge; the case that would force it (promotion failure
mid-Cheney, with forwarding marks half-installed) is made impossible by a
**promotion guarantee** checked before scavenging:

```
allocate(vm, size) slow path (replaces S7 A2 steps 4–6):
 1. stress hooks (=1: scavenge; =full: full_gc every N allocations)
 2. eden bump; fit → done
 3. size > 256 KiB → OldGen::allocate → grow+retry → step 5 on failure
 4. if old.free_bytes() < eden_used + from_used:      // promotion guarantee:
        full_gc()                                     // worst case promotes
        if still not guaranteed: grow until it is     // everything
    scavenge(); retry eden bump; fit → done
 5. full_gc(); grow if policy says; retry (eden AND old) → done
 6. GcStallError { full_gc_count, last_reclaimed_bytes, … }   // designed end
                                                              // of cascade
```

Step 4's guarantee makes `ScavengePromote` stalls unreachable — assert that
in the scavenger (S7's `GcStallError{phase: ScavengePromote}` becomes a
debug assert + release-mode stall, belt and braces).

Per-cycle-state audit (lesson 9 — cascaded phases in ONE pause, e.g. stress-
mode full_gc immediately followed by scavenge for the same allocation):

| State | Lifetime | Hazard audited |
|---|---|---|
| `CompactPlan`, `SideMarks` | phases B–E of one full GC | dropped in E; scavenge can never observe them |
| Handle arena | VM lifetime | rewritten by BOTH collections; truncated by neither |
| `old_top_before` (scavenge) | one scavenge | recomputed per scavenge; a preceding full GC changing `old.top` is picked up |
| Age table | between scavenges | reset by full GC (ages were re-filed via side map; histogram is stale) — reset in phase F, not mid-phase |
| Card table | continuous | full GC resets it (F1); a following scavenge sees the conservative dirtying — correct, just unclean |
| Root *snapshots* | none exist | `for_each_root` enumerates live sources each call — by construction no stale snapshot can survive a cascade |
| `in_gc` flag | one collection | set/cleared per collection; cascade toggles it twice — allocation inside either collection still asserts |

### `gcFull` and statistics primitives

- `gcFull` (SPEC §10, system group): runs the full cascade's `full_gc`;
  answers the receiver. Wired as `System gcFull` in `world/`.
- `gcStats` (dev hook): answers an Array of smis in pinned order:
  `(scavengeCount fullGcCount edenUsed oldUsed oldCommitted bytesPromoted
  markedBytesLast contextAllocs)`. Used by the soak harness and S14's
  Context-elision gate.

> **SPEC-QUESTION:** SPEC §10 lists `gcScavenge gcFull` but no statistics
> primitive; the soak test and S14's gate need programmatic access. S8 adds
> `gcStats` under §10's "dev hooks" umbrella — amend the table.

`MACVM_TRACE=gc` prints one line per collection:
`[gc] scavenge #n: eden 4096K->0K, copied 312K, promoted 12K, thr 3, 0.4ms`
`[gc] full #m: marked 18.2M, live 17.9M, reclaimed 41.3M, old 64M, 22ms`.

### Layer boundaries

As S7, plus: `fullgc.rs` may reach into `spaces`/`cards`/`offsets`/`handles`
freely (same module family), calls `for_each_root` for every root
interaction, and touches interpreter state ONLY through the process-stack
root source. The lookup-cache flush is a call to `runtime::LookupCache::
flush_all(&mut)` — memory does not know its internals. No `dyn` (lesson 1).

## Implementation order

1. `MARK_GC_BIT` + mark-bit helpers on `Mark`; unit tests.
2. `OldGen::grow` + reservation-backed card/offset commit growth; wire
   growth into direct large allocation; tests.
3. Phase A mark on synthetic graphs in `test_heap` (cycles, diamonds,
   self-refs, `tagged_contents` fast path vs dispatch equivalence).
4. Phases B+E on a single space with hand-placed live/dead objects (no
   references yet): forwarding arithmetic, side-map save/restore, POISON.
5. Phase C root+heap rewrite; phase D slide + offset-table rebuild; first
   end-to-end `full_gc` on a Rust-built heap; verifier F1–F3.
6. Table inventory sweep (phase C table) — mechanical audit + fixes (esp.
   dependency-version storage location).
7. Cascade rework of the allocation slow path + promotion guarantee;
   `ScavengePromote` becomes unreachable-asserted.
8. `gcFull`/`gcStats` primitives; `MACVM_GC_STRESS=full[:N]`; trace lines.
9. World suite under `=full`; fragmentation/checksum + identity-hash gate
   tests; soak harness last.

## Pitfalls

- **MacNCL lesson 9 is THE S8 lesson**: the nastiest MacNCL bug was per-sub-
  collection cleanup wiping per-cycle state during a cascading cycle. The
  audit table above is normative — extend it (don't just append code) when
  adding any per-cycle structure.
- **MacNCL lesson 8**: the new disagreement surfaces are CompactPlan vs
  marks (every marked object must have exactly one entry — verifier counts
  both), side map vs entries (every side-map key is some entry's `new`),
  offset table vs slid heap (F2). Cross-check all three at `FullGcExit`.
- **Sizing after rewrite is a trap** (invariant F3): once phase C rewrites
  klass fields to not-yet-populated new addresses, any "quick size check"
  reads garbage. All sizes come from `CompactEntry`.
- **`ptr::copy`, ascending order** in the slide — `copy_nonoverlapping` is
  UB here (S7 used it legitimately; do not copy that call site).
- **MacNCL lesson 5/13**: POISON the reclaimed tails after the slide; a
  stale pre-compaction oop must crash in debug, not read a shifted object.
- **MacNCL lesson 10**: keep the fragmentation/checksum reproducer runnable
  in one command (`just gate-s08`); re-run it after every fix — a fix that
  doesn't move the symptom means the bug is deeper (usually phase C missed
  a root source — lesson 15: check roots before policy).
- **MacNCL lesson 11**: `dbg_oop` must follow the object through phase B
  retargeting or it silently traces a dead address for the rest of the run.
- **Identity hash is load-bearing**: MethodDictionary and Dictionary probe
  on it. Losing a displaced mark corrupts *dictionaries*, which presents as
  "random DNU after full GC" — nowhere near the GC in a stack trace. The
  side-map tests exist for this exact failure shape.
- **Symbol table must NOT be flushed** — interning identity breaks (two
  `#foo`s). It is a root to UPDATE. The lookup cache is the opposite: a
  cache to FLUSH, never update (stale method targets self-heal wrongly).
- **Age semantics across full GC**: survivors keep age via the side map;
  the age *table* (histogram) is per-scavenge and reset in F — do not
  "helpfully" refill it during E.
- **`MACVM_GC_STRESS=full:1` + tiny heaps** will full-GC during world
  loading — genesis remains GC-disabled (S7 A1) but world loading is not:
  the source compiler's handle discipline (S7 A8 category 5) is what gets
  stress-tested here first.

## Interfaces for later sprints

- `pub fn full_gc(vm: &mut VmState) -> Result<FullGcReport, GcStallError>` —
  S12 extends phases A/C with nmethod sources; S13 frame-sweeps not_entrant
  nmethods at each full GC (SPEC §8.6).
- Phase C's inventory table — S9/S11/S12 append: code table (nmethod keys
  are weak: SPEC §8.5), nmethod literal pools (rewrite via Oop relocs),
  dependency index. The table in this file is the master list; keep it
  current (it is the audit artifact, not documentation).
- `for_each_root` remains the single enumeration; S12 adds compiled-frame
  oop-map slots as a source there.
- `OldGen::grow` — S8 policy constants become S15 tuning targets.
- `gcStats` array order is pinned — S14's Context-elision gate indexes it.

## Out of scope

- nmethod flushing, code-cache sweeping, weak nmethod keys → **S12/S13**.
- Compacting the code cache → stretch (SPEC §9).
- Weak refs / `near_death` handling in mark (marked strong in v1) → **S22**.
- Precise (non-conservative) post-compaction card recomputation → revisit
  in S15 only if scavenge-after-full-GC pauses actually hurt.
- Snapshot/image interactions → **S16**.
- Parallel/incremental anything (SPEC §11: single thread, one pause).
