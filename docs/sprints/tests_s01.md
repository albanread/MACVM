# Sprint S1 — Test Plan

## Acceptance gate

Restated from SPRINTS.md S1, made checkable:

1. `just gate-s01` (= `just ci`) green: fmt, clippy `-D warnings`, all tests.
2. Genesis invariants hold in a fresh `VmState::new()`:
   - `nil.klass.name == #UndefinedObject`,
   - `Object class class == Metaclass` and the knot closes
     (`Metaclass class class == Metaclass`),
   - `Object superclass == nil`, `Object class superclass == Class`.
3. Symbol interning is identity-preserving (`intern(b"foo")` twice → same
   oop) and content-correct.
4. Indexable allocation + at/put works via the Rust API; size/alignment math
   is exact for odd byte lengths.
5. `memory::verify::verify_heap` walks the entire post-genesis eden without
   error.
6. All S0 tests still green (standing rule 1).

## Unit tests

| Test name | Module | Assertion | Rationale |
|---|---|---|---|
| `reserve_commit_rw` | memory::reservation | reserve 64 MiB, commit first 1 MiB, write/read u64 at offset 0 and at 1 MiB−8 | mmap/mprotect plumbing before anything depends on it |
| `commit_page_rounding` | memory::reservation | commit(0, 4097) then write at offset 8100 succeeds | page-rounding of commit length |
| `bump_alloc_sequential` | memory::alloc | two `alloc_words(4,…)` return addresses 32 bytes apart; both 8-aligned; `eden.top` advanced by 64 | bump arithmetic |
| `alloc_writes_header` | memory::alloc | fresh object: `mark().word()` has MARK_TAG+sentinel, hash 0, age 0; klass word = passed oop | header init contract |
| `alloc_tagged_contents_bit` | memory::alloc | `alloc_slots` mark has tagged_contents=1; `alloc_indexable_bytes` has 0; `alloc_double` has 0 | GC-critical bit set per format at birth |
| `alloc_nil_fill` | memory::alloc | post-genesis `alloc_slots(association_klass)`: body words 0,1 == `nil_obj` | heap always parsable |
| `slots_size_math` | memory::alloc | Association instance consumes exactly 4 words (next alloc address proves it) | `non_indexable_size` semantics |
| `indexable_oops_size_math` | memory::alloc | `alloc_indexable_oops(array_klass, 3)` consumes 2+1+3 = 6 words; `len()==3`; size slot at body index 0 is smi 3 | indexable layout: [size][elems] after named part |
| `indexable_bytes_padding` | memory::alloc | `alloc_indexable_bytes(bytearray_klass, n)` for n ∈ {0,1,7,8,9,16}: consumed words = 3+ceil(n/8) (i.e. 2+1+pad); `len()==n`; padding bytes read back 0 | odd-size padding, size slot = TRUE length; n=0 legal |
| `double_roundtrip` | memory::alloc | `alloc_double(3.5).value()==3.5`; `alloc_double(f64::NAN).value().is_nan()`; consumed words = 3 | raw (non-oop) body word; NaN bit pattern must not be interpreted as tags |
| `array_at_put` | oops::wrappers | `at_put(0, smi 7); at(0)==smi 7`; debug bounds: `at(3)` on len-3 array panics (debug test) | 0-based indexing + bounds discipline |
| `bytearray_at_put` | oops::wrappers | write bytes 0..9, read back; `byte_at(9)` on len-9 panics in debug | byte accessors |
| `wrapper_format_checks` | oops::wrappers | `ArrayOop::try_from(bytearray oop).is_none()`; `KlassOop::try_from(array_klass.oop()).is_some()`; `SymbolOop::try_from(string oop).is_none()` (String vs Symbol klass) | S1's tightened `try_from`; Symbol requires exact klass |
| `genesis_knot` | memory::universe | `object_klass.klass().klass() == metaclass_klass`; `metaclass_klass.klass().klass() == metaclass_klass`; `class_klass.klass().klass() == metaclass_klass` | the metaclass knot (gate) |
| `genesis_superclasses` | memory::universe | `object_klass.superclass()==nil`; `object_klass.klass().superclass()==class_klass`; `class_klass.superclass()==object_klass`; `metaclass_klass.superclass()==class_klass`; `true_klass.superclass()==boolean_klass`; `true_klass.klass().superclass()==boolean_klass.klass()` | full chain incl. metaclass-parallel rule and the pinned Δ simplifications |
| `genesis_nil_true_false` | memory::universe | `nil_obj.klass()==undefined_object_klass`; klass name symbols `#UndefinedObject #True #False`; `true_obj != false_obj` | gate invariant |
| `genesis_no_placeholders` | memory::universe | walk every klass in the well-known list: klass word is MEM_TAG; superclass is nil or Klass-format; name is Symbol; mixin slot == nil | every genesis patch step actually ran |
| `genesis_formats_and_sizes` | memory::universe | table check: each well-known klass's `format()`/`non_indexable_size()`/`has_untagged_contents()` equals the pinned table in sprint_s01_detail §Design step 9 (method_klass nis 9, association 4, double 3, …) | the table is load-bearing for S2/S3 |
| `metaclass_names` | memory::universe | `object_klass.klass().name() == intern("Object class")`; same pattern for Array and Metaclass itself (`#'Metaclass class'`) | pinned metaclass-name convention |
| `metaclass_shape` | memory::universe | every well-known klass K: `K.klass().format() == Format::Klass`, `K.klass().non_indexable_size() == KLASS_SIZE_WORDS`, `K.klass().klass() == metaclass_klass` | a metaclass's instances are 10-word class objects — the easy-to-swap number from the detail doc's pitfalls |
| `new_klass_regular_path` | memory::universe | after genesis, call the (pub-in-crate) `new_klass` again for a fake `#Zork < Object`: knot properties hold for it too (`Zork class class == Metaclass`, `Zork class superclass == Object class`) | the helper S5's `subclass:` will drive is correct beyond the genesis set |
| `symbol_intern_identity` | memory::symbols | `intern(b"foo")` twice → same raw word; `intern(b"bar")` differs; `intern(b"")` legal and stable | interning is the identity guarantee Symbols exist for |
| `symbol_intern_content` | memory::symbols | interned `#at:put:` has len 7, bytes match, klass == symbol_klass | Symbol object shape |
| `symbol_rehash` | memory::symbols | intern 2000 distinct symbols (generated names); all 2000 re-intern to their original oops; table count == 2000 | growth path preserves identity |
| `identity_hash_lazy` | memory::universe | fresh object mark hash == 0; first `identity_hash` != 0; second call returns the same value; two objects get different hashes | SPEC §2.2 lazy assignment, 0=unassigned |
| `identity_hash_smi` | memory::universe | `identity_hash(smi 5)` == `identity_hash(smi 5)`; no heap write occurs (eden.top unchanged) | smis have no mark word |
| `identity_hash_counter_skips_zero` | memory::universe | force `hash_counter = u32::MAX` (test-only setter/cfg(test) field access), hash two objects: neither gets 0 | wrap edge |
| `print_oop_basics` | oops::print | smi −42 → `-42`; nil/true/false → `nil true false`; `#foo` → `#foo`; string → `'hi'`; array_klass.oop() → `Array`; fresh Association → `an Association`… **pinned**: `a Association` (no vowel logic, per detail doc) | golden-stability of the printer S2's disassembler embeds |
| `print_oop_depth_cap` | oops::print | Array containing itself (write via at_put) prints without hanging (depth cap 3) | recursion guard |
| `verify_heap_post_genesis` | memory::verify | full eden walk: object count > 0, every header valid, walk terminates exactly at `eden.top` | heap parsability invariant |
| `verify_heap_detects_corruption` | memory::verify | in a debug test: overwrite a live object's klass word with smi 0 via a test-only raw hook, assert `verify_heap` reports an error (Result-returning API, not panic) | the walker must actually detect what it claims to check — test the tester |
| `association_ivar_names` | memory::universe | `association_klass.inst_var_names()` is a 2-element Array of `#key`, `#value` (interned identity) | S2's push_instvar tests and S5's name resolution index off this |
| `eden_initial_state` | memory::universe | post-genesis: `eden.start % 8 == 0`; `eden.top > eden.start`; `eden.top - eden.start` < 256 KiB (genesis is small); `eden.end - eden.start ==` 4 MiB | committed-region accounting; catches an eden sized in words vs bytes |
| `print_oop_double` | oops::print | `print_oop(alloc_double(0.5)) == "0.5"`; `1e10` prints round-trippably (`10000000000.0` — pin the exact string in the test) | Double rendering feeds disassembler goldens |
| `print_oop_bad_input` | oops::print | release-mode behavior: printing `Oop::from_raw_unchecked(0x2)` yields a string containing `bad oop` (test compiled only in release, `#[cfg(not(debug_assertions))]`) | printer never panics on garbage — it is the debugging tool of last resort |

Debug-only panic tests gated `#[cfg(debug_assertions)]` as in S0. The S0
notes (assertion messages, literal expected values, one row = one test fn)
carry forward to every sprint's plan.

## Integration/golden tests

`tests/it_memory.rs` (CONVENTIONS §5 — one file per sprint area):

1. **`boot_and_verify`** — setup: `VmState::new()` with `MACVM_HEAP` unset.
   Assert the four gate invariants via public API only, then
   `verify_heap`. Rationale: proves the crate surface (re-exports) suffices
   for an embedder — this is what S2's interpreter tests build on.
2. **`small_heap_boot`** — setup: env `MACVM_HEAP=16` (MiB) via subprocess or
   by constructing `VmOptions{heap_mib:16}` directly (preferred: a
   `VmState::with_options` test constructor — avoids env races under the
   parallel test runner). Genesis must succeed in 16 MiB.
3. **`eden_exhaustion_aborts`** — setup: spawn the `macvm` binary (or a tiny
   test binary) with a small heap and a test hook that allocates in a loop
   (`MACVM_TEST_ALLOC_LOOP=1` handled in `main.rs` behind `cfg(debug_assertions)`
   or a hidden CLI flag `macvm --selftest-alloc-loop`). Expected: process
   exits with code 70 and stderr contains `eden exhausted`. NOT a panic, NOT
   a SIGSEGV. Rationale: pins the S1 slow-path contract that S7 replaces.
4. **`alloc_torture_fill`** — allocate mixed Slots/Arrays/ByteArrays until
   ~1 MiB below eden capacity (deterministic pseudo-random sizes, fixed
   seed, sizes 0–200 elements), verify_heap, then spot-check 100
   deterministically-chosen objects: header valid, Arrays still nil-filled,
   ByteArray padding still zero. Rationale: bump allocator + parsability at
   scale, pre-GC; the fixed seed makes any failure reproducible by line
   number.
5. **`genesis_is_deterministic`** — boot two `VmState`s; for each
   well-known klass assert the *relative* eden offset (`addr - eden.start`)
   is identical across both. Rationale: genesis must be a pure function of
   the options; nondeterminism here (HashMap iteration order, ASLR leaking
   into layout) would make every future golden/stress failure
   unreproducible. This is also the test that fails loudly if someone
   introduces a `HashMap` into genesis ordering.

Test-support requirement: `VmState::with_options(VmOptions) -> VmState`
(bypasses env parsing) is REQUIRED for these tests — add it in
`runtime/vm_state.rs`, not as test-only code, since S2+ integration tests
and the S6 embedded-suite runner all need it.

No golden files yet — `tests/golden/` starts in S2 with the disassembler.

## In-language tests

None. `world/tests/` starts at S6; no interpreter exists. (The genesis
invariants tested here in Rust get re-asserted from inside the language in
S6 — `Object class class == Metaclass` as an SUnit-lite assertion — which is
the cross-check that the Rust-built world and the Smalltalk-visible world
agree.)

## Stress/negative tests

| Scenario | How provoked | Expected |
|---|---|---|
| Placeholder klass dereference | in a debug test, build an object with PLACEHOLDER klass and call `KlassOop::try_from(obj.klass_oop())` | `None` (smi tag) — never a wild read |
| Wrapper misuse | `ArrayOop::try_from` on Double, Method, Klass objects | `None` for each |
| Oversized alloc request | `alloc_indexable_bytes(_, usize::MAX/2)` | debug_assert/checked-arithmetic failure (size math must use `checked_mul/checked_add`), not a wrap-around small allocation |
| at/put out of bounds | index == len | debug panic (bounds debug_asserts) |
| Interning collision pressure | 10k symbols sharing 4-char prefixes | all distinct oops, all re-intern identically |
| Double whose bits mimic a mark word | `alloc_double(f64::from_bits(0b111))`, then `verify_heap` | walker must not misparse (format dispatch before body reads) |
| Double whose bits mimic a heap oop | `alloc_double(f64::from_bits(some_live_addr \| 1))`, verify_heap | as above — the two adversarial bit patterns bracket the format-dispatch requirement |
| Zero-length indexables | `alloc_indexable_oops(_, 0)` and `alloc_indexable_bytes(_, 0)` | legal; `len()==0`; consumed words = nis+1; verify_heap walks past them correctly (the walker's size math for the empty case) |
| Klass reopened as wrong format | debug test: hand-flip a klass's format smi from IndexableOops to IndexableBytes, `ArrayOop::try_from` an existing instance | `None` — wrappers re-check format at every construction, they never cache |

A note on the debug raw hooks used above (corrupting a klass word, flipping
a format smi): these are `#[cfg(test)]` functions in `oops/heap.rs`
(`test_poke_word(addr, word)`), not public API. They exist so negative tests
can construct states the safe API forbids; they must never grow non-test
callers. Grep for `test_poke` in review.

## Non-goals

- GC correctness of any kind (forwarding, roots, barriers) — S7/S8; S1's
  abort-on-full is asserted, nothing more. `MACVM_GC_STRESS` does not exist
  yet.
- Old generation, survivor spaces, card-table math — S7 (`card_shift`,
  offset tables have no tests here because the spaces don't exist).
- Symbol table *weakness* (dead symbols reclaimed) — S22; v1 interning is
  strong by design (SPEC §3.1).
- MethodDictionary probing/growth — S3 tests (`it_send.rs`).
- CompiledMethod object construction — S2 tests (the klass's existence and
  nis=9 are checked here; the layout is exercised there).
- Unicode/UTF-8 semantics of String/Symbol contents — bytes only
  (SPEC §1.3's documented byte-indexing limitation); S6 revisits printing.
- Concurrent allocation — single-threaded by design (SPEC §0, §11).
- Identity-hash *stability across compaction* — S8's gate.
