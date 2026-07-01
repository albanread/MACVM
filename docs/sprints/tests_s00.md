# Sprint S0 — Test Plan

## Acceptance gate

Restated from SPRINTS.md S0, made checkable:

1. `just gate-s00` passes, which requires:
   - `cargo fmt --check` clean,
   - `cargo clippy --all-targets -- -D warnings` clean,
   - `cargo test` green.

   Expected `cargo test` shape at sprint end: all unit tests in
   `oops::{layout, smi, mark, wrappers}` and `oops` root — on the order of
   25 test fns, 0 ignored. Anything `#[ignore]`d is a gate failure (a
   deferred test is a hidden red).
2. Unit tests exist and pass for: tag round-trips, smi min/max/overflow
   edges, mark-word field isolation (set each field, assert all others
   unchanged), forwarding-bit discrimination.
3. `src/oops.rs` (flat file, 1-bit provisional scheme) no longer exists;
   `grep -rn "SMI_SHIFT: u32 = 1" src/` finds nothing.
4. Crate root has `#![deny(unsafe_code)]`; the only `#![allow(unsafe_code)]`
   is in `src/oops/mod.rs`.

## Unit tests

All in-module `#[cfg(test)]` (CONVENTIONS §5). Values below are exact —
compute no expected value from the code under test.

| Test name | Module | Assertion | Rationale |
|---|---|---|---|
| `layout_relations` | oops::layout | `MARK_HASH_SHIFT == MARK_AGE_SHIFT + MARK_AGE_BITS`; `MARK_AGE_SHIFT == MARK_TAGGED_CONTENTS_SHIFT + 1`; `SMI_MAX == -(SMI_MIN + 1)`; `SMI_BITS == 64 - TAG_BITS` | Constants are hand-typed; catch typos structurally |
| `layout_values_pinned` | oops::layout | `INT_TAG==0 && MEM_TAG==1 && RESERVED_TAG==2 && MARK_TAG==3`; `SMI_MAX == 2_305_843_009_213_693_951`; `ALLOC_ALIGN == 8`; `BODY_OFFSET == 16` | SPEC §2.1–§2.2 literal values, independent of relations |
| `smi_zero_and_small` | oops::smi | `SmallInt::new(0).oop().raw() == 0`; `new(1).oop().raw() == 4`; `new(-1).oop().raw() == 0xFFFF_FFFF_FFFF_FFFC` | Tag-0 encoding is exactly value×4; negative words are all-ones-above-tag |
| `smi_roundtrip_edges` | oops::smi | `value(new(v)) == v` for v ∈ {0, 1, −1, 42, −42, SMI_MAX, SMI_MIN, SMI_MAX−1, SMI_MIN+1} | Arithmetic-shift sign extension at both extremes |
| `smi_try_new_range` | oops::smi | `try_new(SMI_MAX).is_some()`; `try_new(SMI_MIN).is_some()`; `try_new(SMI_MAX+1).is_none()`; `try_new(SMI_MIN−1).is_none()`; `try_new(i64::MAX).is_none()` | The 62-bit boundary, not the 64-bit one |
| `smi_try_from_oop` | oops::smi | `SmallInt::try_from(Oop::from_raw(4)) == Some(1)`-shaped; `try_from` of a MEM_TAG word is None | Tag dispatch |
| `smi_checked_add_overflow` | oops::smi | `new(SMI_MAX).checked_add(new(1)).is_none()`; `new(SMI_MIN).checked_add(new(-1)).is_none()`; `new(SMI_MAX).checked_add(new(SMI_MIN))` = Some(−1) | 62-bit overflow detection both directions; large-magnitude legal sum |
| `smi_checked_sub_overflow` | oops::smi | `new(SMI_MIN).checked_sub(new(1)).is_none()`; `new(0).checked_sub(new(SMI_MIN)).is_none()` (−SMI_MIN > SMI_MAX) | Asymmetric range: −MIN is not representable |
| `smi_checked_mul` | oops::smi | `new(3).checked_mul(new(-7)) == Some(-21)`; `new(SMI_MAX).checked_mul(new(2)).is_none()`; `new(SMI_MIN).checked_mul(new(-1)).is_none()`; `new(1<<31).checked_mul(new(1<<31)).is_none()` (2^62) | Untag-before-multiply scale; MIN×−1; product exactly one past MAX |
| `smi_div_floored` | oops::smi | `new(-7).checked_div(new(2)) == Some(-4)`; `new(7).checked_div(new(-2)) == Some(-4)`; `new(-7).checked_rem(new(2)) == Some(1)`; `new(7).checked_rem(new(-2)) == Some(-1)` | Smalltalk // and \\ are floored; Rust's are truncating |
| `smi_div_edge` | oops::smi | `checked_div(new(0)).is_none()`; `new(SMI_MIN).checked_div(new(-1)).is_none()`; `new(SMI_MIN).checked_rem(new(-1)) == Some(0)` | ÷0; MIN/−1 overflows the smi range even though i64 holds it… verify against try_new |
| `smi_checked_shl` | oops::smi | `new(1).checked_shl(60) == Some(1<<60)`; `new(1).checked_shl(61).is_none()`; `new(-1).checked_shl(61) == Some(SMI_MIN)` | Shift overflow is range-asymmetric |
| `oop_tag_predicates` | oops | word 0b100 → `is_smi`; 0b101 → `is_mem`, `mem_addr()==0b100`; `is_smi(0b101)==false` | Predicate truth table |
| `oop_from_raw_rejects_bad_tags` | oops | `#[should_panic]` (debug): `from_raw(0b10)`; second test for `from_raw(0b11)` | RESERVED_TAG/MARK_TAG words must not enter oop position |
| `mark_pristine` | oops::mark | `Mark::pristine().word() == 0b111`; age 0, hash 0, no flags, `is_pristine()` | SPEC §2.2: tag 11 + sentinel |
| `mark_field_isolation_age` | oops::mark | Start from pristine `with_hash(0xDEAD_BEEF).with_near_death(true)`; `with_age(127)`: age reads 127, hash still 0xDEAD_BEEF, near_death still true, tagged_contents still false, tag/sentinel intact, bits 63:44 zero | The gate's field-isolation requirement, age axis |
| `mark_field_isolation_hash` | oops::mark | Symmetric: preload age=127, flags set; `with_hash(u32::MAX)` then `with_hash(0)`; only hash changes each time | Hash axis, incl. full-width hash |
| `mark_field_isolation_flags` | oops::mark | Toggle near_death and tagged_contents independently over a mark with age=64, hash=1; each toggle changes exactly its bit | Flag axes |
| `mark_age_range` | oops::mark | `with_age(127)` ok; `with_age(128)` panics in debug (`#[should_panic]`) | 7-bit field; saturation is the scavenger's job, not Mark's |
| `mark_forwarding_discrimination` | oops::mark | `word_is_forwarded(Mark::pristine().word()) == false`; for a fake mem oop word `w = 0x1000 + 1`: `word_is_forwarded(w)`, `forwardee(w).raw() == w`; `word_is_forwarded(smi word) == false` | The scavenge-critical discriminator: mark(11) vs forwardee(01) vs never-a-smi |
| `mark_from_word_validates` | oops::mark | `from_word(pristine.word())` ok; `from_word(0b011)` (correct MARK_TAG but sentinel 0) panics in debug | Sentinel enforcement |
| `mark_word_roundtrip` | oops::mark | for a mark built as `pristine().with_age(9).with_hash(12345).with_tagged_contents(true)`: `Mark::from_word(m.word()) == m`; `m.word() == 0b11 \| 0b100 \| (1<<4) \| (9<<5) \| (12345<<12)` (typed as a literal) | full word composition against an independently hand-computed literal |
| `smi_eq_is_value_eq` | oops::smi | `new(5) == new(5)`; `new(5) != new(6)`; `new(5).oop() == new(5).oop()` | derived Eq on the tagged word coincides with value equality (canonical encoding) |
| `wrapper_tag_checks` | oops::wrappers | `MemOop::try_from(smi oop).is_none()`; `try_from(0x1008+1 word).is_some()`; `.addr() == 0x1008`; same via `ArrayOop` etc. (macro-generated tests acceptable) | S0-level wrapper contract |
| `wrapper_oop_roundtrip` | oops::wrappers | for each of the nine typed wrappers: `w.oop().raw()` == the input word; `unsafe from_oop_unchecked` then `oop()` is the identity | repr(transparent) newtypes add no bits |
| `mask_disjointness` | oops::layout | `MARK_AGE_MASK & MARK_HASH_MASK == 0`; neither mask overlaps `TAG_MASK`, the sentinel bit, or the flag bits; `(MARK_AGE_MASK \| MARK_HASH_MASK \| TAG_MASK \| 0b11100) >> 44 == 0` | field masks partition bits 43:0 with no overlap and leave 63:44 reserved |
| `oop_tag_exhaustive` | oops | for base words {0, 0x1000, u64::MAX & !3}: `tag(base\|0)==INT_TAG`, `tag(base\|1)==MEM_TAG`, `tag(base\|2)==RESERVED_TAG`, `tag(base\|3)==MARK_TAG` | tag() masks only the low 2 bits regardless of high bits |
| `oop_debug_format` | oops | `format!("{:?}", SmallInt::new(1).oop())` contains `smi` and the hex word; a mem oop's Debug contains `mem` | Debug is the first diagnostics tool; pin its shape loosely (contains, not equals) |
| `oop_size_static` | oops | `size_of::<Oop>()==8`, `size_of::<Option<SmallInt>>()` documented (16 — no niche), `size_of::<Mark>()==8` | repr(transparent) sanity; flags accidental fatness |

Notes for the implementing agent:

1. `#[should_panic]` tests over `debug_assert!` must be gated
   `#[cfg(debug_assertions)]` so `cargo test --release` stays green.
   Pattern:

   ```rust
   #[test]
   #[cfg(debug_assertions)]
   #[should_panic(expected = "reserved tag")]
   fn from_raw_rejects_reserved() { let _ = Oop::from_raw(0b10); }
   ```

   Give every debug_assert a distinguishing message so `expected = "…"`
   pins WHICH assertion fired, not merely that something panicked.
2. Never compute an expected value by calling the code under test with
   different arguments. Expected words in this plan are integer literals;
   type them as literals.
3. Where a test row above lists several assertions, they may live in one
   `#[test]` fn — the row name is the fn name.
4. `just gate-s00` must also be run once as `cargo test --release` locally
   to prove the release profile compiles (debug-gated tests simply drop
   out); add `test-release = cargo test --release` to the justfile if
   desired but do not put it in `ci` (doubles CI time for little S0 value).

## Integration/golden tests

None in S0 — there is no program to run and no output to golden. The first
integration test file (`tests/it_oops.rs`) is OPTIONAL and, if created, only
re-exercises the public API from outside the crate (proves visibility/re-
exports): construct a `SmallInt` via `macvm::oops::SmallInt`, round-trip one
mark word. Keep it under 30 lines or skip it.

## In-language tests

None. `world/tests/` starts at S6 (SPEC §12.3); nothing can execute yet.

## Stress/negative tests

| Scenario | How provoked | Expected |
|---|---|---|
| Reserved-tag word enters oop position | `Oop::from_raw(0b10)` in a debug-build test | debug_assert panic (tested above) |
| Mark word used as oop | `Oop::from_raw(Mark::pristine().word())` | debug_assert panic |
| Mark constructed from a forwardee word | `Mark::from_word(0x1000 + 1)` | debug_assert panic (tag check) — the scavenger must use `word_is_forwarded`, never `from_word`, on a possibly-forwarded header |
| Discarded immutable update | `let _ = mark.with_age(1);` vs bare `mark.with_age(1);` | bare form is a `#[must_use]` warning → error under `-D warnings` (verified by the lint gate, not a runtime test) |
| Exhaustive smi boundary sweep | see procedure below | no divergence over ~6000 points |

**Boundary-sweep procedure** (`smi_boundary_sweep`, one plain `#[test]`, no
proptest dependency):

1. Build the value set: `SMI_MIN..=SMI_MIN+1000`, `-1000..=1000`,
   `SMI_MAX-1000..=SMI_MAX`.
2. For each `v`: assert `SmallInt::try_new(v).unwrap().value() == v` (encode/
   decode round-trip at scale).
3. For each `v`: compute `checked_add(new(v), new(1))` and compare against
   the oracle `v.checked_add(1).filter(|r| *r >= SMI_MIN && *r <= SMI_MAX)`
   — the oracle uses ONLY `i64` std methods plus the layout constants, never
   `oops::smi` code.
4. Repeat step 3 for `checked_sub(_, 1)`, `checked_mul(_, 3)`, and
   `checked_div(_, 7)` (floored-division oracle:
   `v.div_euclid(7)` — note `div_euclid` IS Smalltalk's `//` for positive
   divisors; write the negative-divisor cases as explicit literals in
   `smi_div_floored` instead of oracling them here).
5. The whole sweep must run in well under a second; if it does not,
   something is allocating.

No GC stress, JIT stress, or deopt stress exists yet (`MACVM_GC_STRESS`
arrives S7) — `gate-s00` is exactly `just ci`.

## Non-goals

- Header reads/writes against real memory — covered by S1's tests
  (`it_memory.rs`, genesis invariants).
- Format-checked wrapper constructors (`ArrayOop::try_from` rejecting a
  ByteArray) — S1.
- Identity-hash assignment (the counter, skipping 0) — S1; S0 only tests the
  mark field's mechanical pack/unpack.
- Smalltalk-visible overflow behavior (`SmallInteger>>+` falling back to
  LargeInteger) — S3 primitives + S6 library.
- Performance of tagged arithmetic — never gated before S15 (SPRINTS
  standing rule 3).
- `checked_shl` semantics for negative shift amounts / `bitShift:` with a
  negative argument — that mapping (negative = right shift) is decided at
  the S3 primitive layer; S0's `checked_shl` takes `u32` and shifts left
  only.
- Interaction of the reserved tag (10) with any future immediate Character —
  v1 rejects the tag outright (SPEC §2.1); no forward-compat tests are
  written for a representation that does not exist.
- Cross-platform behavior — the VM targets macOS arm64 only (SPEC title);
  no CI matrix, no 32-bit concerns, no endianness tests (little-endian
  assumed throughout, incl. S2's operand encoding).
