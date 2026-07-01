# Sprint S11 — Test Plan

## Acceptance gate

Restated from SPRINTS.md S11, made checkable:

1. Full in-language suite green at `MACVM_JIT=threshold=1`, transcript
   byte-identical to `MACVM_JIT=off`.
2. IC-patch unit tests: a LIVE compiled call site transitions
   Unresolved→Mono→PIC(2..4)→Mega under receivers of growing polymorphism,
   with correct results at every step; the veneer path is exercised by a
   forced far target.
3. Mixed-tier call matrix passes: I→C, C→I, C→C, super-from-compiled, DNU
   from compiled — each verified for result AND for stack-trace shape.
4. Dispatch micro-benchmark ≥ 5× interpreter (recorded in `docs/PERF.md`;
   warn <5×, fail <2× — standing rule 3 discipline as in S10).
5. All existing stress gates green (`MACVM_GC_STRESS` modes still run with
   JIT off; combined mode is S12).

`just gate-s11`, in order:

```sh
cargo test
MACVM_JIT=off         just run-world-tests > /tmp/s11_off.txt
MACVM_JIT=threshold=1 just run-world-tests > /tmp/s11_t1.txt
diff /tmp/s11_off.txt /tmp/s11_t1.txt
MACVM_GC_STRESS=1       just run-world-tests    # JIT off — regression only
MACVM_GC_STRESS=full:64 just run-world-tests
just bench-s11          # dispatch micro → docs/PERF.md
cargo clippy -- -D warnings
```

Dispatch micro (`world/bench/dispatch.mst`): a 3-class polymorphic
`area`-summing loop over a preallocated 30k-element array (mono per site
after warm-up in the compiled version — one PIC of 3), timed off vs
`threshold=1`, ratio appended to `docs/PERF.md` (warn <5×, fail <2×).

Test-only hooks required by this file (behind `#[cfg(any(test,
feature = "vm-test-hooks"))]`, exposed to `.mst` via dev primitives):
`ic_state_of(nmethod, site_index) -> IcState`, `ic_target_of(...) -> u64`,
`stats` counters (`resolves`, `pic_builds`, `mega_calls`,
`bridge_old_allocs`, `gc_under_compiled`).

## Unit tests

| Test name | Module | Assertion | Rationale |
|---|---|---|---|
| `entry_guard_smi_and_heap` | `compiler::emit` | listing golden: entry prologue is exactly the D2 sequence; verified_entry_off == entry_off + 7*4? (assert recorded offsets match emitted labels) | guard ABI is load-bearing |
| `ic_site_recorded_per_send` | `compiler::emit` | N sends → N IcSites, offsets point at `bl` words, selectors correct | miss protocol depends on site lookup |
| `resolve_patches_mono` | `codecache::icpatch` | fake site + fake CodeTable entry: after `rt_resolve_send`, bl field targets nmethod entry; IcState::Mono | state machine row 1 |
| `mono_to_pic_rebuild` | `codecache::icpatch` | second klass → PIC allocated, site repatched, old target still reachable via PIC pair 1 | row 3 |
| `pic_grows_by_rebuild` | `codecache::icpatch` | 3rd/4th klass → NEW stub each time, old stub freed (cache free-list grows), pairs preserved in order | P1/P2 — no in-place edits |
| `pic_full_to_mega` | `codecache::icpatch` | 5th klass → site targets per-selector mega stub; PIC freed | row 5 |
| `pic_contains_no_bl` | `codecache::icpatch` | scan generated PIC words: no 0x94xxxxxx | P2 invariant |
| `mega_stub_per_selector_cached` | `codecache::icpatch` | two sites, same selector → same stub handle | table hygiene |
| `patch_is_single_word` | `codecache::icpatch` | before/after byte diff of a transition == exactly 4 bytes at the site | P1 |
| `veneer_on_far_patch` | `codecache` | forced target cache_base+512 MiB → veneer allocated, bl targets veneer (reuses S9 test path on an IC site) | gate item 2 |
| `barrier_emitted_conditions` | `compiler::emit` | StoreField{barrier:true} listing contains the card sequence; barrier:false doesn't | P7 |
| `card_dirtied_by_compiled_store` | integration | compiled instvar store of young obj into old obj dirties exactly the right card (reuse S7 card assertions) | barrier correctness |
| `alloc_fast_path_layout` | `compiler::emit` | listing golden for Alloc: bump, bounds, mark init from layout const, klass store, nil init, tag | D7 sequence pinned |
| `adapters_cached_and_swept` | `codecache::icpatch` | c2i created once per method; after nmethod install for that method, dependent sites repatched to nmethod entry | D6.1 eager repatch |
| `rehash_mega_and_adapter_maps` | `codecache` | simulated key-oop move + rehash → lookups work | P11 |
| `rootspill_offsets_pinned` | `codecache::stubs` | stub/adapter frame offsets equal `layout.rs` consts | P8 ABI |
| `anchor_set_and_cleared` | `codecache::stubs` | after a stubbed runtime call returns, `last_compiled_fp == 0` | P9 |
| `nlr_sentinel_check_emitted` | `compiler::emit` | every CallSend in a listing is followed by `cmp x0, #6; b.eq` | D6.3/P10 — one missing check = silent oop confusion |
| `super_send_static_target` | `compiler::emit` | compiled `super describe` emits a Mono IcSite bound at compile time, no resolve on first call (stats.resolves unchanged) | D4.6 |
| `smi_fail_edge_is_callsend` | `compiler::ir` | S11 conversion: SmiArith fail block contains CallSend to the SAME ic index, no Bailout variant anywhere | D1 — restart trick is dead |
| `eligibility_relaxed_matrix` | `compiler::driver` | instvar-store method now eligible; closure/ctx methods still not; per-cause table extended from S10 | D1 |

## Integration/golden tests

`tests/it_sends.rs`, run at `threshold=1` unless noted.

1. **Call matrix (gate item 3).** A 3-class hierarchy (`A/B/C` overriding
   `describe`) exercised so that, verified via `MACVM_TRACE=jit,ic` scrape +
   results:
   - **I→C**: hot leaf compiled, interpreted caller dispatches via IC smi id.
   - **C→C**: compiled `caller` sends to compiled `leaf`; trace shows one
     resolve then silent mono hits.
   - **C→I**: compiled caller sends to a method kept ineligible (contains a
     closure); adapter created; result correct; stack trace during the callee
     shows `[interpreted callee, ADAPTER, compiled caller, …]`.
   - **super**: compiled `B>>describe` doing `super describe` → static-target
     bl; result matches interpreted run.
   - **DNU**: compiled method sends `#zork` to a Point → `doesNotUnderstand:`
     runs (interpreted), Message selector/args correct, result propagates
     back through the compiled frame.
2. **Live-site lattice walk (gate item 2).** One compiled call site
   `x describe` driven with receivers A, then B, then C, then D, then E
   (5 klasses): assert after each phase — Mono(A) / PIC{A,B} / PIC{A..C} /
   PIC{A..D} / Mega — via the `ic_state_of` hook, AND that every call
   returned the right string. This is the flagship of this file: it patches
   a site that is re-executed between every transition. Additional
   assertions per phase: (a) `stats.resolves` increments exactly once per
   transition and never on a repeat receiver (hits are silent); (b) after
   the Mega transition, driving all five receivers again causes ZERO further
   resolves and `stats.mega_calls` counts them; (c) each PIC rebuild freed
   the previous stub (cache free-list length via hook); (d) re-running phase
   receivers in REVERSE order after each transition still yields correct
   results (PIC order independence).
3. **`nlr_through_compiled_frame`** — interpreted `outer` calls compiled
   `mid`, which sends to interpreted `inner:` receiving a block that NLRs to
   `outer`. Assert: NLR lands in `outer` with the right value; `ensure:`
   blocks in `outer`→`inner` interpreted frames ran in order; `mid`'s
   compiled frame was discarded (compiled_depth back to matching);
   process/native sp both restored (debug asserts).
4. **`allocation_fast_and_slow`** — compiled loop allocating Points:
   (a) normal run: allocations come from eden (stats), values correct;
   (b) eden pre-filled to force the slow edge on iteration 1: slow path
   allocates via `rt_alloc_slow` under the D8 bridge (old-direct,
   `bridge_old_allocs > 0`), values still correct.
5. **`listing_goldens_s11`** — `tests/golden/s11_send.lst.expected` (a
   method with one send: guard prologue, marshaling, bl placeholder, NLR
   check) and `s11_alloc.lst.expected`.
6. **`stale_mono_documented_hole`** — `#[ignore]`d test encoding the KNOWN
   S11 hole: redefine a method under a mono compiled site with unchanged
   receiver klass → old code still runs. The test asserts the WRONG (stale)
   behavior and is flipped to assert freshness in S13 (dependency
   invalidation). Keeping it checked in prevents the hole being forgotten.

## In-language tests

`world/tests/sends_tier1.mst`:

- Polymorphic `printString`-style dispatch over a heterogeneous
  OrderedCollection (drives real PIC/mega states inside the suite).
- Arithmetic overflow → LargeInteger fallback now via the CallSend fail edge
  (replaces S10's bailout): `assert: (2 raisedTo: 62) * 2` class + value.
- Instvar-store-heavy setter loops (barrier under compiled code).
- DNU-based proxy test (a class implementing only `doesNotUnderstand:`).
- The S10 tier1.mst cases rerun unchanged.

## Stress/negative tests

- **Suite at `threshold=1`** — now with nearly every method compiled and
  C→C/C→I traffic dominant; this is the sprint's main stressor.
- **Bridge accounting** — run the suite at `threshold=1` and assert at exit:
  no scavenge/full-GC ever ran while `compiled_depth > 0`
  (`vm.stats.gc_under_compiled == 0` — the counter exists solely to prove
  the D8 bridge held), and log the `bridge_old_allocs` total to PERF.md
  (visibility of the cost S12 removes).
- **Cache exhaustion mid-lattice** — tiny cache: PIC rebuild fails alloc →
  site degrades gracefully to mega (pinned fallback: alloc failure during
  PIC build = skip to Mega, which needs no new stub if cached) — assert no
  panic, correct results.
- **`resolve_reentrancy_assert`** — debug assert that `rt_resolve_send`
  never triggers compilation or GC (P5): run with a poisoning hook
  (`vm.forbid_alloc` scope) around resolution.
- **Anchor-poisoning** — debug mode writes a canary into
  `last_compiled_fp` after clear; walker asserts it never dereferences the
  canary.

## Non-goals

- GC moving objects under live compiled frames (bridge forbids it) — the
  whole point of `tests_s12.md`; the flagship combined gate lives there.
- Method-redefinition freshness under compiled callers (S13 dependency
  tests; the `#[ignore]` hole-test here is the placeholder).
- Deopt-stress mode, uncommon traps, scope-desc round-trips — S13.
- Inlining effects on send counts, Context elision, block compilation —
  S14. Dispatch-perf HARD gating — S15.
