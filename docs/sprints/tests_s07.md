# Sprint S7 — Test Plan

## Acceptance gate

Restated from SPRINTS.md S7, made checkable (`just gate-s07` runs all of it):
1. **Entire S6 suite green under `MACVM_GC_STRESS=1`** (a scavenge before
   every allocation): all Rust unit tests, all golden tests, the whole
   `world/tests/` in-language suite — plus the same suite *without* stress
   (both must pass; SPRINTS standing rule 1).
2. Unit tests below green: forwarding, tenuring histogram math, card
   indexing, dirty-card scan finds exactly the recorded old→new refs.
3. Allocation-torture: 10M short-lived objects complete in bounded memory
   (RSS ceiling asserted).
4. The exact-oracle stressor (`gc_stress_test.mst`) passes with
   `sum == 5000000` exactly, with ≥ 33 scavenges observed in `GcStats`.
5. Debug build runs the A9 verifier at every scavenge entry/exit throughout
   all of the above without a single `VerifyError`.

`just gate-s07` (CONVENTIONS §5), in order:

```
cargo test                                   # all unit + integration + golden
cargo test --release
MACVM_GC_STRESS=1 cargo test                 # gate item 1 (debug: verifier on)
MACVM_GC_STRESS=1 cargo test --release
MACVM_EDEN=64 cargo test --test it_world gc_stress   # gate item 4 (oracle)
cargo test --test it_gc allocation_torture --release # gate item 3
```

> **SPEC-QUESTION:** the stressor needs a shrunken eden. CONVENTIONS §3 pins
> the env-flag list without an eden knob (`MACVM_HEAP` is total MiB only).
> This plan assumes a new `MACVM_EDEN=<KiB>` flag parsed into `VmOptions`
> (test-only in spirit, harmless in general). Amend CONVENTIONS §3.

## Unit tests

In-module `#[cfg(test)]`, all on `Reservation::test_heap` where a heap is
needed (deterministic, no mmap). Names are binding.

| Test name | Module | Assertion | Rationale |
|---|---|---|---|
| `reserve_commit_idempotent` | memory::reservation | double `commit` of same range succeeds; write/read through committed range works | Backing pattern contract |
| `commit_rounds_to_host_page` | memory::reservation | commit of 1 byte makes the whole 16 KiB page writable | Apple Silicon page size |
| `decommit_then_recommit` | memory::reservation | decommit → recommit → memory readable again (contents unspecified) | lesson 5: reclaimed ≠ zeroed |
| `test_heap_deterministic` | memory::reservation | `test_heap` needs no mmap; commit/decommit are no-ops | unit-test substrate |
| `is_old_boundary_tagged` | memory::layout | `is_old(addr)`, `is_old(addr+1)` (mem-tagged) agree on both sides of `old_start` | the one-boundary check on tagged oops |
| `card_index_math` | memory::cards | slot→card index for card 0 start, card 0 end−8, card 1 start, last committed slot | §7.4 shift-9 arithmetic |
| `record_multistores_range` | memory::cards | ranges [mid-card, mid-card), spanning 1/2/513 cards dirty exactly the overlapped cards | promotion dirtying |
| `store_barrier_matrix` | memory::store | old-obj/new-val → dirty; old/old → clean; new-obj/new-val → clean; old-obj/smi → clean; old-obj/nil → clean | exact SPEC §7.4 condition |
| `offset_direct_entry` | memory::offsets | object header 0/8/512 bytes before a card base resolves via entries 0/1/64 | direct encoding |
| `offset_chain_long_object` | memory::offsets | a 256 KiB old object (512 cards) resolves from its last card through ≥ 2 chained entries | 65..=255 back-skip encoding |
| `offset_updated_on_alloc` | memory::spaces | consecutive `OldGen::allocate` calls leave every card below `top` resolvable | maintenance invariant 5 |
| `oldgen_alloc_none_when_full` | memory::spaces | returns `None`, not a capped grant, not a panic | lesson 4 |
| `mark_forward_roundtrip` | oops | install forwarding, `is_forwarded`, read forwardee; non-forwarded mark says no | §2.2 discrimination |
| `age_table_threshold` | memory::scavenge | crafted histograms: all-young ⇒ 127; heavy age-0 > ½ capacity ⇒ 0-boundary case; cumulative crossing at age 3 ⇒ 3 | tenuring formula pinned |
| `copy_empty_from_space` | memory::scavenge | scavenge with empty from-space and eden-only survivors is correct | edge case |
| `copy_exact_fill_survivor` | memory::scavenge | survivors exactly filling to-space (`top == end`) succeed; +8 bytes overflows into promotion | off-by-one on the bump check |
| `promotion_reenters_scan` | memory::scavenge | object graph where a promoted object references eden objects: promoted-region scan copies them (both scan pointers reach fixed point) | A3 step 4 |
| `byte_object_padding` | memory::scavenge | 13-byte ByteArray copies with padding intact, byte part never scanned | A5 table row |
| `double_body_not_scanned` | memory::scavenge | a Double whose f64 bits look like a valid oop survives uncorrupted | A5 table row |
| `closure_context_scan` | memory::scavenge | Closure (method + copied[]) and Context (all slots) fully relocated; smi `home_frame_ref` untouched | A5 table rows |
| `method_literals_ics_scan` | memory::scavenge | CompiledMethod's literals/ics arrays relocate as ordinary oops; bytecode bytes bit-identical | A5 table row |
| `klass_first_protocol` | memory::scavenge | hand-built class + instance both in eden (incl. a Metaclass-style klass 2-cycle): scavenge terminates, instance sized correctly via forwardee | A5 — the subtle one; the cycle must not recurse |
| `hash_survives_copy` | memory::scavenge | assigned identity hash identical after scavenge; age incremented | mark-word transport |
| `handle_scope_lifo` | memory::handles | nested scopes push/truncate the arena correctly; handles read/write through vm | §7.6 mechanics |
| `handle_updated_by_gc` | memory::handles | handle to an eden object reads the to-space copy after scavenge | handles are roots |
| `stall_error_fields` | memory::stall | provoked stall carries requested_bytes, per-space counts, scavenge_count, progress numbers | lesson 7 |

## Integration/golden tests

`tests/it_gc.rs` (new; runs release + debug):

1. **`dirty_card_scan_exact`** — build (via the Rust API) an old-gen object
   population with exactly K recorded old→new stores through `memory::store`
   spread over known cards, plus old→old and smi stores as decoys. Scavenge.
   Assert: every new-gen target survived and every old slot was rewritten to
   its forwardee (gate item 2: the scan finds *exactly* the recorded refs);
   cards with no remaining old→new refs are clean afterwards.
2. **`allocation_torture`** — loop allocating 10M short-lived pairs/arrays,
   keeping only a rolling window of 100. Assert completion, `GcStats.
   scavenge_count > 0`, and process RSS stays under 4× the committed heap.
3. **`tenuring_promotes_long_lived`** — keep a structure alive across > 127
   scavenges; assert it ends up in old gen (`is_old`) and that subsequent
   scavenges do not copy it (copied-bytes counter flat).
4. **`stall_is_structured`** — `MACVM_HEAP` tiny + unbounded live list: the
   process exits non-zero printing a `GcStallError` block (no panic, no
   abort); golden-match the field *names* not values.
5. **`dbg_oop_smoke`** — run a scavenging workload with `MACVM_DBG_OOP` set
   to a known object's address; assert the trace names each phase boundary
   and logs the move with old→new addresses.

## In-language tests

`world/tests/gc_stress_test.mst` — **the exact-oracle stressor (MacNCL
lesson 3, ported to Smalltalk).** Run by `tests/it_world.rs` with
`MACVM_EDEN=64` (KiB) so the workload forces roughly 33+ scavenges; the Rust
harness asserts `GcStats.scavenge_count >= 33` after the run.

```smalltalk
TestCase subclass: GcStressTest [
  testListWalkOracle [
    | sum |
    sum := 0.
    1 to: 100000 do: [:i |
      | list |
      list := OrderedCollection new.
      1 to: 50 do: [:j | list add: 1].
      list do: [:e | sum := sum + e]].
    self assert: sum equals: 5000000       "EXACT — a GC dropping 3 objects
                                            per 100K iterations passes every
                                            other test; this one it fails"
  ]
  testSurvivorsAcrossScavenges [
    | keep |
    keep := Array new: 100.
    1 to: 100 do: [:i | keep at: i put: (Association key: i value: i * i)].
    1 to: 50000 do: [:i | OrderedCollection new add: i; yourself].
    1 to: 100 do: [:i |
      self assert: (keep at: i) value equals: i * i]
  ]
  testIdentityHashStableAcrossScavenge [
    | o h |
    o := Object new.  h := o identityHash.
    1 to: 10000 do: [:i | Array new: 8].
    self assert: o identityHash equals: h
  ]
]
```

Plus: every existing `world/tests/*.mst` file reruns unchanged under
`MACVM_GC_STRESS=1` — that rerun IS the primary S7 gate (lesson 2: guest-
language workloads through the production allocator, not mechanics tests).

## Stress/negative tests

Failure modes deliberately provoked (debug builds):

| Test | Provocation | Expected |
|---|---|---|
| `verifier_catches_undirtied_card` | tests-only backdoor writes an old→new ref bypassing `memory::store` | `verify_heap(ScavengeEntry)` returns `VerifyError` naming the card (lesson 8 cross-check a) |
| `verifier_catches_bad_offset` | corrupt one offset-table entry via backdoor | cross-check b flags the card whose chain fails to resolve |
| `verifier_catches_stale_handle` | plant a from-space POISON oop in the handle arena | cross-check c flags the slot |
| `poison_trips_stale_oop` | hold a raw `Oop` across a forced scavenge (test-only), then read a field | typed-wrapper `debug_assert` fires on the POISON mark — deterministic, not flaky |
| `scavenge_asserts_pre_universe` | call `scavenge` before `universe_ready` | debug assert (genesis knot must never be scanned) |
| `no_alloc_inside_gc` | allocation choke point invoked with `in_gc` set (test-only) | debug assert |
| `stress_mode_every_alloc` | `MACVM_GC_STRESS=1`: assert `scavenge_count == allocation_count` over a counted run (post-genesis) | stress hook wired at the single choke point |

## Non-goals

- **Full-GC correctness, compaction checksums, identity-hash stability
  across compaction, soak** → `tests_s08.md`.
- **Old-gen growth / promotion-guarantee cascade** → S8 (S7 stall is final).
- **Compiled-code barrier equivalence** → S11 tests; **moving GC under
  compiled frames** → S12 (the flagship `threshold=1` + `GC_STRESS=1` gate).
- **Weak references / symbol-table weakness** → S22.
- Performance of scavenge pauses is recorded (`docs/PERF.md`), not gated
  (SPRINTS standing rule 3; the < 1 ms target gates nothing until S15).
