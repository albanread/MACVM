# Sprint S8 — Test Plan

## Acceptance gate

Restated from SPRINTS.md S8, made checkable (`just gate-s08`):
1. **Entire suite green under `MACVM_GC_STRESS=full`** (full GC every N=100
   allocations) AND under `full:1` in debug for the in-language suite, AND
   still green under `MACVM_GC_STRESS=1` and with stress off (standing
   rule 1: all stress modes that exist so far).
2. **Fragmentation/checksum compaction test** passes: fragment old gen,
   full-GC, live object graph checksum identical before/after, space
   measurably reclaimed.
3. **Identity-hash stability**: identityHash values and identity-keyed
   Dictionary lookups survive compaction.
4. **1-hour soak** with a flat memory ceiling (CI runs the 2-minute
   variant; the 1-hour run is executed once per sprint sign-off and its
   numbers recorded in `docs/PERF.md`).
5. Debug builds run `verify_heap(FullGcEntry/FullGcExit)` (incl. F1–F3
   cross-checks) throughout all of the above with zero `VerifyError`s.

## Unit tests

On `Reservation::test_heap` with Rust-built object populations.

| Test name | Module | Assertion | Rationale |
|---|---|---|---|
| `mark_bit_roundtrip` | oops | set/clear `MARK_GC_BIT`; hash/age/flags unaffected | §2.2 field isolation extends to bit 44 |
| `mark_graph_diamond` | memory::fullgc | A→{B,C}→D marks each exactly once (push count == 4) | set-on-push discipline |
| `mark_graph_cycle` | memory::fullgc | 3-cycle and self-referential object terminate, all marked | worklist termination |
| `mark_tagged_fast_path_equiv` | memory::fullgc | same heap marked via fast path and via format dispatch yields identical mark sets | fast path is an optimization, not a semantic |
| `mark_skips_raw_bodies` | memory::fullgc | Double bits / ByteArray bytes / Method bytecode crafted to look like oops are not pushed | corruption guard |
| `forward_compute_holes` | memory::fullgc | live/dead/live/dead pattern: new addresses coalesce at bottom, entries ascending, `new <= old` | phase B arithmetic |
| `forward_compute_all_live` | memory::fullgc | zero dead ⇒ all self-forwarded, slide is byte-identical no-op | edge case |
| `forward_compute_empty_space` | memory::fullgc | empty eden ⇒ zero entries, no faults | edge case |
| `side_map_matrix` | memory::fullgc | marks with (hash only)/(age only)/(near_death)/(all)/(pristine): first four saved+restored bit-exact minus gc bit; pristine reinitialized with `tagged_contents` recomputed from klass | §2.2 displaced-mark contract |
| `slide_overlap_safe` | memory::fullgc | object sliding into a range overlapping its old bytes copies correctly (`ptr::copy` semantics) | the memmove trap |
| `offset_table_rebuilt` | memory::fullgc | after compaction every card below `old.top` resolves to the true covering header (F2), incl. a multi-card object that moved | phase D step 2 |
| `card_reset_semantics` | memory::fullgc | after full GC: cards over `[old.start, old.top)` dirty, above clean | phase F step 1 |
| `rewrite_covers_frames` | memory::fullgc | interpreter frames (incl. Context oops, method oops) rewritten; `saved_fp`/`saved_bci` smis bit-identical | §7.5 step 3 stack fixup |
| `rewrite_handles_and_symbols` | memory::fullgc | handle-arena slots and symbol-table entries point at new addresses; interning `#foo` after GC returns the SAME oop as a pre-GC `#foo` handle | update-not-flush for symbols |
| `lookup_cache_flushed` | memory::fullgc | cache probe after full GC misses (then refills correctly) | flush-not-update for the cache |
| `plan_vs_marks_crosscheck` | memory::verify | verifier counts marked objects == CompactPlan entries; a planted mismatch is reported | lesson 8, new surfaces |
| `grow_commits_tables` | memory::spaces | `OldGen::grow` extends committed_end; card+offset tables writable over the new segment; `is_old` boundary unchanged | growth mechanics |
| `grow_exhaustion_stalls` | memory::spaces | growth past the reservation yields the terminal `GcStallError` with `full_gc_count > 0` and progress numbers | designed end of cascade |
| `promotion_guarantee_triggers_full` | memory | scavenge request with `old.free < eden_used + from_used` runs full GC (or grows) FIRST; `ScavengePromote` stall never constructed | cascade step 4; nested-GC impossibility |
| `cascade_sequential_not_nested` | memory | stress `full:1` allocation path: full_gc then scavenge in one pause, `verify_heap` between them clean; `in_gc` toggles twice, never overlaps | lesson 9 audit, mechanized |
| `ages_survive_full_gc` | memory::fullgc | from-space survivor with age 5 has age 5 after full GC; next scavenge tenures it on schedule | side map carries age |

## Integration/golden tests

`tests/it_gc_full.rs`:

1. **`fragmentation_checksum`** (gate item 2) — via the Rust API: fill old
   gen with ~10K objects (mixed formats: Slots, IndexableOops, ByteArrays
   with odd lengths, Doubles, a class object); drop every second one (remove
   from the keeper array); record `old_used_before`; compute a structural
   checksum of the live graph (recursive fold over klass name, format, smi
   values, byte contents, Double bits, structure — NOT addresses, NOT
   identity hashes of unhashed objects); `gcFull`; recompute checksum from
   the (relocated) keeper roots. Assert checksums equal AND
   `old_used_after <= old_used_before * 0.6`.
2. **`identity_hash_stability`** (gate item 3) — assign identityHash to
   1000 objects; store them in an identity-keyed Dictionary (world-level)
   and in a Rust `Vec<(Handle, u64)>`; `gcFull` twice; assert every hash
   unchanged and every Dictionary lookup still hits.
3. **`old_growth_ladder`** — allocate live data past 1 segment; assert
   `old_grow_count` climbs, program completes, and a final `gcFull` +
   drop-everything returns `old_used` to near-baseline.
4. **`stall_after_full`** — unbounded live growth in a small reservation:
   terminal `GcStallError` printed with `full_gc_count > 0`,
   `last_reclaimed_bytes` visibly shrinking (progress numbers tell the
   truth: GC ran and stopped helping); exit non-zero, no panic.
5. **`dbg_oop_through_compaction`** — trace one object across scavenge +
   full GC; log must show phase boundaries, the phase-B retarget, and the
   restored mark.

## In-language tests

`world/tests/gc_full_test.mst`:

```smalltalk
TestCase subclass: GcFullTest [
  testChurnWithGrowthAndRelease [
    | keep sum |
    keep := OrderedCollection new.
    1 to: 200000 do: [:i |
      keep add: (Array with: i with: i * 2).
      (i \\ 1000) = 0 ifTrue: [ keep := OrderedCollection new ]].
    System gcFull.
    sum := 0. keep do: [:a | sum := sum + (a at: 1)].
    self assert: sum > 0
  ]
  testLiveGraphIntactAfterGcFull [
    | d |
    d := Dictionary new.
    1 to: 500 do: [:i | d at: i put: i printString].
    System gcFull.  System gcFull.
    1 to: 500 do: [:i | self assert: (d at: i) equals: i printString]
  ]
  testIdentityDictionaryAfterGcFull [
    | keys d |
    keys := (1 to: 100) collect: [:i | Object new].
    d := IdentityDictionary new.        "or Dictionary keyed on identityHash"
    keys doWithIndex: [:k :i | d at: k put: i].
    System gcFull.
    keys doWithIndex: [:k :i | self assert: (d at: k) equals: i]
  ]
]
```

Plus the S7 exact-oracle stressor (`GcStressTest>>testListWalkOracle`,
sum == 5000000) reruns under `MACVM_GC_STRESS=full` — the lesson-3 oracle now
guards the compactor too. The whole `world/tests/` suite reruns under
`=full` and `=full:1` (debug).

## Stress/negative tests

| Test | Provocation | Expected |
|---|---|---|
| `verifier_catches_leftover_markbit` | backdoor sets `MARK_GC_BIT` on one object post-GC | `FullGcExit` / next `FullGcEntry` reports F1 violation |
| `verifier_catches_stale_forwarding` | backdoor leaves one forwarding mark after phase E | F1 violation named with the address |
| `verifier_catches_offset_drift` | backdoor corrupts one rebuilt offset entry | F2 violation names the card |
| `sizing_after_rewrite_forbidden` | (compile-time) phases D/E take `&CompactPlan`, not `&HeapLayout` sizing access | API shape enforces F3 — document, no runtime test possible |
| `poison_after_slide` | hold a raw pre-compaction oop (test backdoor) and read post-GC | deterministic POISON-mark debug assert |
| `full_stress_world_load` | boot the entire world under `full:1` (debug) | loads green — the source compiler's handle discipline under full-GC fire |

## Soak test design (gate item 4)

`world/bench/soak.mst`, parameterized by iteration count; `just soak-s08`
(1 hour) and `just soak-s08-ci` (2 minutes):
- **Workload mix per cycle**: (a) short-lived churn — 10K temp collections;
  (b) medium-lived — a 5K-entry cache with random (LCG, seeded — the
  MacNCL-style *stochastic corpus with a fixed seed*: reproducible)
  insertion/eviction; (c) long-lived — a slowly growing then plateauing
  symbol/string population; (d) byte-heavy — Strings built and discarded;
  (e) periodic `gcFull` every 50 cycles plus organic full GCs from pressure.
- **Continuous integrity verification**: every cycle recomputes a running
  checksum over the medium- and long-lived structures against a shadow model
  (a parallel Array of expected values) — corruption is caught the cycle it
  happens, not at the end.
- **Flat-ceiling assertion**: the Rust harness samples `gcStats` after each
  full GC; after a 10-full-GC warmup, assert
  `max(oldUsed) − min(oldUsed) < 10%` of mean and `oldCommitted` stops
  growing; also sample process RSS and assert the same shape.
- Failure dumps: last 50 `[gc]` trace lines + final `gcStats` array.

## Non-goals

- **Compiled-frame roots, nmethod literal-pool updates, code-table
  weakness** → S12 tests (the `threshold=1` + GC-stress flagship gate).
- **Deopt/invalidation interactions with full GC** (not_entrant sweeps) →
  S13.
- **Scavenge-only correctness** — covered by `tests_s07.md`, rerun here
  only as part of the combined gate.
- **Weak references, weak symbol table** → S22.
- **Pause-time targets** (< 50 ms at 100 MB live) — recorded in
  `docs/PERF.md`, not gated (standing rule 3).
- Snapshot/image round-trips → S16.
