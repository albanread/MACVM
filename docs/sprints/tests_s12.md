# Sprint S12 — Test Plan

## Acceptance gate

Restated from SPRINTS.md S12, made checkable:

1. **THE FLAGSHIP GATE**: the entire in-language suite passes with
   `MACVM_JIT=threshold=1` **and** `MACVM_GC_STRESS=1` simultaneously
   (scavenge on every allocation, everything eligible compiled), transcript
   byte-identical to `MACVM_JIT=off` without stress. Also with
   `MACVM_GC_STRESS=full:64` (full GC every 64 allocations) at
   `threshold=1`.
2. Unit: oop-map encode/decode round-trip; a compiled frame's spill slots
   relocated correctly across a forced scavenge mid-loop.
3. Unit: full GC moves an object referenced from an nmethod literal pool;
   compiled code observes the moved object.
4. Key-klass death flushes the nmethod; all referencing compiled sites and
   PICs reset; interpreter ICs self-heal.
5. All prior gates green; the S11 bridge counters inverted (see below).

## The flagship gate — step-by-step procedure (`just gate-s12`)

1. `cargo test` (all unit + integration, includes everything below).
2. Baseline transcripts: run `tests/it_world.rs` runner over
   `world/tests/*.mst` with `MACVM_JIT=off`, no stress → `base.txt`.
3. Combined run A: `MACVM_JIT=threshold=1 MACVM_GC_STRESS=1` → `a.txt`.
   Every allocation scavenges; every eligible method is compiled before its
   first execution; every send-site slow path, adapter, PIC transition, and
   inline-alloc slow edge executes under a moving young collector with
   compiled frames on the native stack.
4. Combined run B: `MACVM_JIT=threshold=1 MACVM_GC_STRESS=full:64` →
   `b.txt`. Full GCs exercise: compaction sliding pool-referenced objects,
   key rehashes, weak key-klass sweep (no klass dies in the suite — the
   sweep must be a no-op, which IS the assertion), update-phase ordering.
5. `diff base.txt a.txt && diff base.txt b.txt` — byte-identical or fail.
6. Invariant counters asserted at each combined run's exit:
   - `vm.stats.gc_under_compiled > 0` (the bridge is gone and the hard case
     actually ran — inverts S11's bridge test, S12 P10);
   - `vm.stats.bridge_old_allocs` counter DELETED (compile error if
     referenced — the bridge code is gone);
   - zero `oopmap_at` panics, zero verifier failures (they abort anyway).
7. Soak: `world/bench/churn.mst` (10M short-lived objects through a
   compiled allocation loop) at `threshold=1`, default GC, assert flat
   memory ceiling (reuses the S7 torture harness, now with tier 1 on).
8. `cargo clippy -- -D warnings`.

Expected wall time is dominated by step 3 (scavenge per allocation ×
O(nmethod pools) code-root scan — S12 P9); if > ~10 min, apply P9's side
list, do not weaken the gate.

## Unit tests

| Test name | Module | Assertion | Rationale |
|---|---|---|---|
| `oopmap_roundtrip` | `compiler::oopmap` | set slots {0, 5, 63, 64} on a 70-slot map → decode yields exactly those | format, word boundary |
| `oopmap_dedup` | `codecache::nmethod` | two safepoints with identical liveness share one oopmaps index | builder economy |
| `oopmap_liveness_intersection` | `compiler::oopmap` | slot whose oop interval ENDS before safepoint → bit clear even though `slot_is_oop` | D2/P2 — dead slots excluded |
| `verify_spill_all_catches_reg` | `compiler::regalloc` | hand-built intervals with a Reg assignment across a safepoint → `verify_spill_all` panics | D1 enforcement point 1 |
| `oopmap_verifier_catches_nonoop_bit` | `compiler::oopmap` | map bit on a non-oop slot → `oopmap::verify` fails | D1 enforcement point 2 |
| `pool_relocs_after_literal_off` | `compiler::emit` | every Oop/KeyKlassOop reloc offset ≥ literal_off for all S10/S11 goldens | P4 — no instruction-embedded oops |
| `pcdesc_exact_match_required` | `codecache::nmethod` | `oopmap_at(ret_pc ± 4)` panics; exact hits | P1 |
| `walker_classifies_all_kinds` | `runtime::frames` | synthetic stack (real frames built by executing a crafted I→C→adapter→I→C chain, halted via test hook in rt_poll) → FrameView sequence matches expectation | D3 |
| `walker_adapter_fixed_slots` | `runtime::frames` | Adapter{C2i} root visit touches exactly the 7 pinned RootSpill offsets | layout ABI |
| `flush_resets_dependent_sites` | `codecache` | nm A called by compiled B (mono) and by a PIC in C: flush A → B's site Unresolved (bl → stub_resolve), C's PIC freed + site reset, A's cache range on the free list | D6 sweep |
| `flush_id_reuse_safe` | `codecache` + interpreter | interpreter IC holds A's smi id; flush A; install unrelated nm reusing the id; dispatch through the stale IC → validation miss → correct method runs, IC rewritten | D6.3 / S10 D4 safety net |
| `weak_key_skipped_in_mark` | `memory` | mark-phase visitor never receives KeyKlassOop words; update-phase visitor does | D5 filtering |
| `each_code_root_covers_pics_adapters_mega` | `memory` | counters per table > 0 after a run that created all three | D4.1 completeness |

## Integration/golden tests

`tests/it_gc_jit.rs`.

1. **`mid_loop_forced_scavenge` (gate item 2) — full design.**
   - *Setup*: via `BytecodeBuilder` (not source — the test controls exact
     bytecode), build class `T` with method `run:` —
     `| p s | p := Point x: 3 y: 4.  s := 0.
      1 to: n do: [:i | Gc scavenge. s := s + p x + i].  ^ s @ p`.
     `Point x:y:` and `Gc scavenge` sites are seeded MONO (call once
     interpreted first) so the method is eligible; `p` is an EDEN object
     held in a spill slot across the `Gc scavenge` send (a CallSend
     safepoint) on every iteration.
   - *Act*: compile `run:` (`threshold=1`), call with n=100.
   - *Assert*:
     a. result `s == 100·3 + 5050` and the returned `p` is a Point with
        x==3 (values correct through ≥100 relocations — survivor
        ping-pong moves `p` every scavenge until tenuring);
     b. instrumentation hook `vm.test_hooks.on_scavenge` records `p`'s oop
        BITS before/after the first scavenge: they DIFFER (it really
        moved) and the frame slot (read via a walker hook at the safepoint)
        holds the new bits;
     c. `gc_under_compiled` incremented ≥ 100;
     d. debug walker asserted exact PcDesc match at every scavenge.
   - *Variant*: same test with the scavenge forced from the ALLOC slow edge
     instead of a send (pre-fill eden each iteration via hook) — covers the
     Alloc safepoint map.
2. **`fullgc_moves_pool_literal` (gate item 3)** — method with a literal
   Array in its pool; fragment old gen (S8 harness), run full GC while an
   interpreted caller is mid-loop calling the compiled method between
   collections; assert: pool word rewritten (read via reloc iteration),
   compiled result still uses the correct (moved) array, `by_key`/mega/
   adapter rehashes happened (lookup works post-GC).
3. **`fullgc_during_compiled_frame`** — as (2) but the full GC is forced
   from a send INSIDE the compiled method (`Gc full` mono site): compiled
   frame slots updated by the slide, execution resumes at the same return
   address (code didn't move — D4.2), result correct.
4. **`key_klass_death_flushes` (gate item 4)** — define class `Tmp`,
   compile `Tmp>>hot`, drive a compiled caller mono to it AND an
   interpreter IC to its nmethod id; then remove `Tmp` from the `smalltalk`
   namespace + drop all instances; force full GC. Assert: nmethod flushed
   (slot None, cache space freed), caller's site back to `stub_resolve`,
   subsequent interpreter dispatch through the stale IC self-heals to DNU
   or re-lookup (Tmp gone ⇒ lookup fails ⇒ DNU path — assert the DNU),
   `flush_set` sweep happened BEFORE update phase (trace-order scrape,
   P8).
5. **`mixed_trace_under_stress`** — the S10/S11 stack-trace goldens re-run
   at `threshold=1 + GC_STRESS=1` (walker + trace stability while
   everything moves).

## In-language tests

`world/tests/gc_tier1.mst` (runs inside the flagship combined modes like
the rest of the suite):

- A compiled hot loop building and discarding 100k Points/Associations
  while keeping 100 survivors in an OrderedCollection — checksum the
  survivors after (values survive arbitrary relocation histories).
- Deep C→I→C recursion (fib-shaped with an ineligible middle method) to
  depth 200 under stress — tier links + adapters under GC.
- String/Symbol identity checks across full GCs from compiled frames
  (`assert: #foo == #foo` in a compiled method post-GC — symbol table +
  pool coherence).

## Stress/negative tests

- **Flagship steps 3–4** are the stress core (every allocation collects).
- **`poisoned_anchor_detected`** — debug: clear the anchor inside a
  runtime call from compiled code (test hook) → the next allocation's
  poison-check assert fires (S12 P6) — proves the guard-rail guards.
- **`missing_pcdesc_is_fatal`** — hand-publish an nmethod with one PcDesc
  deleted (test hook); force scavenge at that site → clean panic naming the
  nmethod + pc (P1), not corruption.
- **`walker_terminates_on_torn_tierlinks`** — corrupt `tier_links` in a
  debug test → walker assert (bounded steps + monotonic fp check), no
  infinite loop.
- **1-hour soak** (manual, `#[ignore]`): S8's churn soak re-run at
  `threshold=1`, default GC, flat ceiling.

## Non-goals

- Method-redefinition invalidation, not_entrant patching, deopt of live
  activations, `MACVM_DEOPT_STRESS` — S13 (S12 only flushes on klass
  death; the S11 `#[ignore]` stale-mono test stays ignored).
- Scope-desc contents beyond `PcDesc.bci` — S13.
- Register maps / poll safepoints / interior pointers — explicitly rejected
  for v1 (S12 D1); no tests pretend otherwise.
- Perf of the code-root scan under stress — tracked (gate step 8 timing
  note), optimized only via P9's side list if needed; never gated here.
