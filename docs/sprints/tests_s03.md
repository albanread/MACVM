# Sprint S03 — Test Plan

## Acceptance gate

Restated from SPRINTS.md S3, made checkable. All of the following, plus every
S0–S2 test, green under `just gate-s03` (= `cargo test` + `cargo clippy`
clean; no GC stress modes exist yet):

1. **IC lattice unit tests** drive every row of the transition table in
   `sprint_s03_detail.md` §Algorithms-2 explicitly, asserting `ic_state`
   before and after each send (empty→mono→poly(2..4)→mega, plus the DNU
   "IC untouched" rows).
2. **Guard self-heal**: after `install_method` redefines a cached method
   (SPEC §6.2), the next send through a mono and a poly IC dispatches the
   *new* method, with exactly one extra lookup (observable via a lookup-count
   test hook or `MACVM_TRACE=ic`).
3. **Golden programs** (BytecodeBuilder-assembled; transcripts captured via
   `vm.out` — `.mst` goldens start in S5): dispatch through a 3-class
   hierarchy, super sends, DNU trace with the pinned fallback format.
4. **Primitive failure → bytecode fallback**: a primitive method whose
   primitive Fails executes its bytecode body and returns its value.

## Unit tests

| test name | module | assertion | rationale |
|---|---|---|---|
| `mdict_insert_probe` | oops::method_dict | insert 3 selectors, probe returns each method; absent selector → None | basic correctness |
| `mdict_collision_chain` | oops::method_dict | symbols forced to same home slot (craft hashes) all retrievable | linear probing correctness |
| `mdict_growth` | oops::method_dict | insert past 3/4 load: capacity doubles, tally preserved, all old entries probe-able, klass `methods` slot updated | rehash-into-new |
| `mdict_reopen_overwrite` | oops::method_dict | inserting an existing selector replaces the method, tally unchanged | class reopening (§1.2) |
| `mdict_power_of_two` | oops::method_dict | capacity invariant after growth ×3 | mask-based probing soundness |
| `symbol_hash_eager` | memory::symbols | freshly interned symbol has nonzero identity hash | probe must not mutate marks |
| `lookup_walks_chain` | runtime::lookup | 3-deep hierarchy: selector on root found from leaf klass | hierarchy walk |
| `lookup_shadowing` | runtime::lookup | override on middle klass shadows root's method from leaf | correct walk order |
| `lookup_miss` | runtime::lookup | absent selector → None from every level | DNU precondition |
| `cache_probe_insert` | runtime::lookup | insert then probe returns method; different (k,sel) misses | cache basics |
| `cache_victim_promote` | runtime::lookup | two colliding keys: both retrievable; second probe of the victim promotes it to primary (assert slot positions) | 2-way victim design |
| `cache_flush` | runtime::lookup | after flush, probe misses; lookup still succeeds via walk and refills | flush contract |
| `cache_keyed_by_receiver_klass` | runtime::lookup | inherited method cached under the *receiver* klass key | LookupKey shape (SPEC §6.1) |
| `install_flushes_cache_and_bumps_epoch` | runtime | `install_method` → cache probe misses; `vm.ic_epoch` incremented | §6.2 triggers |
| `ic_empty_to_mono` | interpreter::ic | row 1: one send → `IcState::Mono`, guard = receiver klass | lattice |
| `ic_empty_dnu_untouched` | interpreter::ic | row 2: DNU send leaves `IcState::Empty` | lattice |
| `ic_mono_hit_no_write` | interpreter::ic | row 3: repeat send, IC words bit-identical before/after | fast path is read-only |
| `ic_mono_selfheal` | interpreter::ic | row 4: redefine method (epoch bump), same-klass send → new method ran, state still Mono, epoch restamped | guard self-heal |
| `ic_mono_selfheal_to_empty` | interpreter::ic | row 4b: after redefining klass so selector unfindable (install on fresh hierarchy), stale mono → Empty + DNU | heal-to-empty edge |
| `ic_mono_to_poly` | interpreter::ic | row 5: second receiver klass → `Poly(2)`, pairs = [k0,m0,k1,m1] in order | promotion |
| `ic_poly_append` | interpreter::ic | row 9: 3rd, 4th klasses → Poly(3), Poly(4) | arity growth |
| `ic_poly_to_mega` | interpreter::ic | row 10: 5th klass → `IcState::Mega`, target slot = nil | cap at 4 (tunable) |
| `ic_poly_reverify_all` | interpreter::ic | row 8: build Poly(3); redefine ONE pair's method; send hitting a *different* pair → after send, ALL pairs' targets current + epoch restamped (assert the redefined pair's target is the new method without ever sending to it) | the all-pairs-then-restamp rule |
| `ic_poly_reverify_drops_dead` | interpreter::ic | row 8: pair whose lookup now fails is removed and pairs compacted | dead-pair deletion |
| `ic_mega_no_writes` | interpreter::ic | rows 12–13: sends with 6 klasses; IC words identical across further sends | mega is a sink |
| `ic_dnu_rows_untouched` | interpreter::ic | rows 6, 11: DNU with mono/poly state leaves state + arity unchanged | lattice completeness |
| `ic_super_static_target` | interpreter::send | super site: two receiver klasses → Poly(2) with identical targets; dispatched method is holder's superclass's, 3-deep hierarchy | §5.3 super rule |
| `activate_counter_bump` | interpreter::send | N sends → invocation counter = N | S10 trigger input |
| `counter_saturates` | interpreter::send | force counter to 0xFFFF; more sends keep it 0xFFFF | no wrap |
| `counter_bumped_on_prim_success` | interpreter::send | primitive-success sends still bump | feedback completeness |
| `prim_smi_matrix` | runtime::primitives | table-driven: each smi prim id × {ok case, each fail condition from the appendix table} | exact semantics |
| `prim_floored_division` | runtime::primitives | `-7//2=-4`, `7//-2=-4`, `-7\\2=1`, `7\\-2=-1`, `//0`→Fail | truncation bug guard |
| `prim_bitshift_edges` | runtime::primitives | `1 bitShift: 61`→Fail (overflow), `1 bitShift: 60` ok, shift ±62→Fail, negative shifts arithmetic | overflow policy |
| `prim_smi_overflow_fails` | runtime::primitives | max-smi + 1 → Fail (not wrap, not panic) | LargeInteger fallback contract |
| `prim_oops_matrix` | runtime::primitives | ids 20–28: ok + each fail row (incl. `basicNew` on Double-format klass → Fail; `at:` on bytes object → Fail) | format checking |
| `prim_bytes_matrix` | runtime::primitives | ids 40–45: ok + fail rows (Symbol immutability on 41/43; overlap copy same-object both directions; `to<from` no-op; prefix compare) | exact semantics |
| `prim_identity_hash_stable` | runtime::primitives | two calls return same smi; distinct objects differ | lazy hash |
| `prim_table_sorted_unique` | runtime::primitives | PRIMITIVES sorted by id, no duplicates, argc matches selector colon count | table integrity |
| `stack_receiver_index` | interpreter::frame | `receiver_index` arithmetic vs a hand-built frame | sp-convention pin |
| `prim_replace_overlap_forward_back` | runtime::primitives | same-object `replaceFrom:to:with:` shifted +1 and -1 both correct | memmove semantics |
| `prim_compare_prefix` | runtime::primitives | `'ab' compare: 'abc'` = -1, reverse = 1, equal = 0 | prefix rule |
| `message_shape` | runtime | DNU-built Message: `instVarAt: 1` = selector, `instVarAt: 2` = args Array of argc | Message layout pin |
| `ic_meta_packing` | oops::layout | meta smi pack/unpack: argc 0..15 × epoch 0..(2^24-1) edges isolated | slot-1 encoding |
| `epoch_wrap_debug_assert` | runtime | forcing `ic_epoch` to 2^24 fires debug_assert | accepted-risk guard |

## Integration/golden tests

All in `tests/it_sends.rs` / `tests/it_ic.rs`, using `BytecodeBuilder` (no
source compiler until S5). Transcripts captured by replacing `vm.out` with a
`Vec<u8>` buffer; expected text inline in the test (moved to
`tests/golden/*.mst` form in S5).

Shared harness (`tests/common/mod.rs`):

- `fn boot() -> VmState` — genesis + interned test selectors.
- `fn class(vm, name, superclass) -> KlassOop` and
  `fn method(vm, klass, sel, |b: &mut BytecodeBuilder|)` — one-line world
  building for every test in this file.
- `fn run_expect(vm, entry: MethodOop, expected_out: &str, expected: Oop)` —
  runs to completion, asserts transcript and result oop.
- Lookup-count hook: `vm.stats.full_lookups: u64` (cfg(test)-friendly plain
  counter, also feeds `MACVM_TRACE=ic`) — used by the self-heal and
  cache-effectiveness assertions below.

1. **`hierarchy_dispatch`** — setup: classes `A ← B ← C`; `A>>tag` returns 1,
   `B>>tag` returns 2 (C inherits B's). Program sends `tag` to instances of
   A, B, C and prints results. Expected: `1 2 2`, and the send site ends
   `Poly(3)` (A, B, C are distinct klasses even where the method is shared).
2. **`super_send_chain`** — `A>>describe` prints "A"; `B>>describe` prints
   "B" then `super describe`; `C>>describe` prints "C" then `super describe`.
   Send `describe` to a C. Expected transcript: `C B A`. Asserts both the
   holder-based lookup start and IC caching on the super sites.
3. **`dnu_trace_golden`** — send `#frobnicate` to a Point-like instance from
   a named method, no `doesNotUnderstand:` installed. Expected output exactly:

   ```
   DNU #frobnicate (receiver class Point)
     Point>>poke @<bci>
     Main>>run @<bci>
   ```
   (bci values pinned by the builder program), process exit code 1.
4. **`dnu_smalltalk_handler`** — install `Object>>doesNotUnderstand:` (builder
   method) that reads `Message` selector + arguments via `instVarAt:` and
   returns argument count. Send `#foo:bar:` with 2 args; expected result smi 2.
   Asserts Message construction (selector symbol identity, arguments Array
   contents in push order).
5. **`prim_fallback`** — method `+` on a test klass bound to primitive 1 with
   fallback bytecode returning smi 999. Send with a non-smi argument →
   result 999 (fallback ran); send with smi args → primitive result. Also:
   max-smi + 1 → 999 (overflow falls back).
6. **`mixed_arity_sends`** — unary, binary, 3-keyword sends interleaved on
   one method's IC table; asserts operand-stack depth returns to baseline
   after each statement (frame-discipline regression from S2 now with args).
7. **`send_w_wide`** — method with > 256 IC sites; site 300 dispatches
   correctly via `send_w`. (Builder generates the sites mechanically.)

## In-language tests

None. `world/tests/*.mst` requires the S5 source compiler and S6 SUnit-lite;
this sprint's semantics get in-language coverage in S6 (see Non-goals).

## Stress/negative tests

| test | provocation | expected |
|---|---|---|
| `deep_hierarchy_32` | lookup from a 32-deep chain, selector on root | correct method; cache makes second send 1 probe (hook-counted) |
| `megamorphic_20_klasses` | one site, 20 receiver klasses, 3 rounds | correct results every round; state Mega after round 1; no IC writes in rounds 2–3 |
| `redefinition_storm` | 1000 `install_method` calls interleaved with sends through mono + poly sites | every send dispatches the then-current method; epoch strictly increases; no panic |
| `dict_growth_storm` | install 500 selectors on one klass | all retrievable; capacity is next power of two ≥ ceil(500·4/3) |
| `dnu_of_dnu` | send unknown selector with NO `doesNotUnderstand:` anywhere | fallback printer runs once, process terminates, no recursion/stack overflow |
| `prim_wrong_argc_debug` | builder installs primitive with mismatched argc | debug build: `debug_assert` fires; release: primitive Fails (documented behavior) |
| `cache_flush_mid_run` | flush LookupCache between sends (test hook) | results unchanged; only cost differs |
| `epoch_bump_poly_alternation` | Poly(2) site, redefine, then alternate sends between the two klasses ×100 | exactly one reverification pass (all-pairs rule prevents ping-pong re-lookups) |
| `stack_overflow_send_recursion` | method that sends itself unconditionally | VM stack-overflow error (not a Rust panic/segfault), trace printed |

## Non-goals

- No GC interaction tests: no GC exists. The `gc_epilogue`/flush contract is
  asserted only structurally (`cache_flush`); real coverage lands in S7/S8
  gates (`MACVM_GC_STRESS`).
- No handle-discipline enforcement tests — S7. S3 only follows the
  stack-parking pattern by convention.
- No block/`value`/`ensure:`/`mustBeBoolean`/`cannotReturn:` coverage → S4.
- No `.mst` golden files or in-language suite → S5/S6.
- No nmethod-id IC targets, no counter-triggered compilation → S10.
- No performance assertions (interpreter throughput recorded first in S6,
  SPRINTS standing rule 3).
