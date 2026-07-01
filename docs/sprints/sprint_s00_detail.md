# Sprint S0 — Skeleton, tags, mark word

Objective: implement the pinned 64-bit value representation of SPEC §2.1–§2.2
and §2.5 — the `Oop` tagged word, the `Mark` mark-word wrapper, the typed-oop
newtypes, and smi arithmetic with overflow detection — plus the `justfile`/CI
habits every later sprint relies on. No heap exists yet; everything in this
sprint is pure bit manipulation, testable without allocation.

SPEC sections implemented: §2.1 (tagged oops), §2.2 (mark word), §2.5 (typed
wrapper surface, tag-level checks only — format checks arrive in S1), §12.1
(unit-test layer), SPRINTS standing rule 4 (unsafe containment).

## Prerequisites

- The repository scaffold as it stands: `Cargo.toml` (edition 2021, lib
  `macvm` + bin `macvm`), `src/lib.rs` with module stubs, `src/oops.rs`,
  `src/memory.rs`, `src/lookup.rs`, `src/interpreter.rs`, `src/runtime.rs`,
  `src/utils.rs`, `src/compiler/`.
- **`src/oops.rs` is superseded and must be deleted.** It encodes a
  *provisional 1-bit* tag scheme (`SMI_TAG_MASK = 0b1`, `SMI_SHIFT = 1`,
  `Oop(pub isize)`) that contradicts SPEC §2.1's pinned 2-bit scheme. Nothing
  else in the crate references it (the only other occurrence of the token
  `Oop` is an unrelated enum variant in `src/compiler/assembler.rs`). Replace
  the file with the `src/oops/` directory described below. Do not keep the old
  predicates "for compatibility" — there must be exactly one tag scheme in the
  tree.
- No other sprint has run. `src/memory.rs`, `src/lookup.rs`, etc. stay as
  stubs; per CONVENTIONS §1 they are converted to directories only when a
  sprint first touches them.

## Deliverables

- `src/oops/` module directory (converts `src/oops.rs`):
  - `src/oops/mod.rs` — `Oop`, tag predicates, re-exports; module-level
    `#![allow(unsafe_code)]` (though S0 itself should need no `unsafe`).
  - `src/oops/layout.rs` — ALL bit/offset constants (single source of truth,
    CONVENTIONS §2).
  - `src/oops/mark.rs` — `Mark` wrapper, field pack/unpack, forwarding
    discrimination.
  - `src/oops/smi.rs` — `SmallInt` wrapper + overflow-checked arithmetic.
  - `src/oops/wrappers.rs` — heap-oop newtype declarations (`MemOop`,
    `KlassOop`, `ArrayOop`, `ByteArrayOop`, `SymbolOop`, `MethodOop`,
    `ClosureOop`, `ContextOop`, `DoubleOop`) with tag-level checked
    constructors. Their header/format accessors are `todo!()`-free — simply
    absent until S1.
- `src/lib.rs` updated: crate-level `#![deny(unsafe_code)]`, doc comment
  pointing at SPEC.md, module list unchanged otherwise.
- `justfile` at repo root with `test`, `lint`, `ci`, `gate-s00` targets.
- Unit tests per `tests_s00.md`.

## Design

### Data structures

All constants live in `src/oops/layout.rs` and are used by name everywhere
else — no re-derived magic numbers (CONVENTIONS §2).

```rust
// src/oops/layout.rs  — SPEC §2.1, §2.2. Pinned.
pub const TAG_BITS: u32 = 2;
pub const TAG_MASK: u64 = 0b11;
pub const INT_TAG: u64 = 0b00;        // smi
pub const MEM_TAG: u64 = 0b01;        // heap oop; address = word - MEM_TAG
pub const RESERVED_TAG: u64 = 0b10;   // future immediate Character; illegal in v1
pub const MARK_TAG: u64 = 0b11;       // only as header word 0

pub const SMI_SHIFT: u32 = 2;
pub const SMI_BITS: u32 = 62;
pub const SMI_MAX: i64 = (1i64 << 61) - 1;   //  2_305_843_009_213_693_951
pub const SMI_MIN: i64 = -(1i64 << 61);      // -2_305_843_009_213_693_952

pub const WORD_SIZE: usize = 8;
pub const ALLOC_ALIGN: usize = 8;     // SPEC §2.1: objects 8-byte aligned (NOT 16)
pub const MARK_OFFSET: usize = 0;     // header word 0
pub const KLASS_OFFSET: usize = 8;    // header word 1
pub const BODY_OFFSET: usize = 16;    // first body word
pub const HEADER_WORDS: usize = 2;

// Mark word fields — SPEC §2.2. bit positions, low to high.
pub const MARK_SENTINEL_SHIFT: u32 = 2;         // always 1 in a real mark
pub const MARK_NEAR_DEATH_SHIFT: u32 = 3;
pub const MARK_TAGGED_CONTENTS_SHIFT: u32 = 4;
pub const MARK_AGE_SHIFT: u32 = 5;              // bits 11:5
pub const MARK_AGE_BITS: u32 = 7;
pub const MARK_AGE_MAX: u8 = 127;
pub const MARK_HASH_SHIFT: u32 = 12;            // bits 43:12
pub const MARK_HASH_BITS: u32 = 32;
// bits 63:44 reserved, must stay zero in v1.
pub const MARK_AGE_MASK: u64 = ((1u64 << MARK_AGE_BITS) - 1) << MARK_AGE_SHIFT;
pub const MARK_HASH_MASK: u64 = ((1u64 << MARK_HASH_BITS) - 1) << MARK_HASH_SHIFT;
```

```rust
// src/oops/mod.rs
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct Oop(u64);        // SPEC §2.1: the only type crossing the unsafe boundary

impl Oop {
    pub const fn raw(self) -> u64;
    /// From a raw word. debug_asserts tag != RESERVED_TAG and tag != MARK_TAG
    /// (mark words are never general oops).
    pub fn from_raw(w: u64) -> Oop;
    /// Escape hatch WITHOUT the debug_assert, for the mark-word slot and tests.
    pub const fn from_raw_unchecked(w: u64) -> Oop;
    pub const fn tag(self) -> u64;              // self.0 & TAG_MASK
    pub const fn is_smi(self) -> bool;          // tag == INT_TAG
    pub const fn is_mem(self) -> bool;          // tag == MEM_TAG
    pub fn mem_addr(self) -> usize;             // debug_assert!(is_mem); (raw - MEM_TAG)
}
```

`Oop` implements `Debug` (hex word + tag name) but **not** `Default`,
`PartialOrd`, or arithmetic — accidental untagged math must not compile.
There is no "nil constant" in S0; nil is a heap object created in S1.

```rust
// src/oops/smi.rs
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct SmallInt(Oop);

impl SmallInt {
    pub const MAX: i64 = layout::SMI_MAX;
    pub const MIN: i64 = layout::SMI_MIN;
    /// None if v outside [SMI_MIN, SMI_MAX].
    pub fn try_new(v: i64) -> Option<SmallInt>;
    /// Panics (VM bug) if out of range — for VM-internal constants.
    pub fn new(v: i64) -> SmallInt;
    pub fn value(self) -> i64;                  // (raw as i64) >> SMI_SHIFT (arithmetic)
    pub fn oop(self) -> Oop;
    pub fn try_from(o: Oop) -> Option<SmallInt>;

    // Overflow-checked arithmetic on the *tagged* representation where the
    // tag scheme allows it (INT_TAG = 0 ⇒ tagged add/sub are exact):
    pub fn checked_add(self, rhs: SmallInt) -> Option<SmallInt>;
    pub fn checked_sub(self, rhs: SmallInt) -> Option<SmallInt>;
    pub fn checked_mul(self, rhs: SmallInt) -> Option<SmallInt>;
    pub fn checked_div(self, rhs: SmallInt) -> Option<SmallInt>;  // floored (Smalltalk //)
    pub fn checked_rem(self, rhs: SmallInt) -> Option<SmallInt>;  // floored (Smalltalk \\)
    pub fn checked_shl(self, by: u32) -> Option<SmallInt>;
}
```

```rust
// src/oops/mark.rs
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct Mark(u64);

impl Mark {
    /// tag=MARK_TAG, sentinel=1, everything else 0: the mark of a fresh object.
    pub const fn pristine() -> Mark;
    pub fn from_word(w: u64) -> Mark;           // debug_assert!(w & TAG_MASK == MARK_TAG && sentinel)
    pub const fn word(self) -> u64;

    pub fn age(self) -> u8;
    #[must_use] pub fn with_age(self, age: u8) -> Mark;     // debug_assert!(age <= MARK_AGE_MAX)
    pub fn hash(self) -> u32;                    // 0 = not yet assigned (SPEC §2.2)
    #[must_use] pub fn with_hash(self, h: u32) -> Mark;
    pub fn near_death(self) -> bool;
    #[must_use] pub fn with_near_death(self, v: bool) -> Mark;
    pub fn tagged_contents(self) -> bool;
    #[must_use] pub fn with_tagged_contents(self, v: bool) -> Mark;
    pub fn is_pristine(self) -> bool;            // hash==0 && age==0 && no flags

    /// Forwarding discrimination on a raw header word (NOT on a Mark — a
    /// forwarded header no longer contains a mark). SPEC §2.2:
    /// is_forwarded ⇔ (word & TAG_MASK) == MEM_TAG.
    pub fn word_is_forwarded(w: u64) -> bool;
    pub fn forwardee(w: u64) -> Oop;             // debug_assert!(word_is_forwarded(w))
}
```

All `with_*` methods are pure (return a new `Mark`); the mutation point is
whoever writes the header word (S1's allocator and, later, the GC).

```rust
// src/oops/wrappers.rs — declarations only in S0; tag-level checks.
macro_rules! oop_newtype { ... }   // generates the boilerplate below per type

#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct MemOop(Oop);
impl MemOop {
    pub fn try_from(o: Oop) -> Option<MemOop>;   // is_mem() only in S0
    /// # Safety: caller guarantees o is a mem oop of the right shape.
    pub unsafe fn from_oop_unchecked(o: Oop) -> MemOop;
    pub fn oop(self) -> Oop;
    pub fn addr(self) -> usize;                  // untagged address
}
// KlassOop, ArrayOop, ByteArrayOop, SymbolOop, MethodOop, ClosureOop,
// ContextOop, DoubleOop: identical shape over MemOop. In S0 their try_from
// delegates to MemOop::try_from (tag check only) — S1 tightens each to also
// check the klass format (SPEC §2.5) once headers exist.
```

Invariants (each backed by a `debug_assert!` and a unit test):
- A `SmallInt` oop always has tag `INT_TAG`; `value()` round-trips for every
  in-range i64.
- A `Mark` word always has tag `MARK_TAG` and sentinel = 1; reserved bits
  63:44 are zero.
- `Oop::from_raw` rejects `RESERVED_TAG` and `MARK_TAG` words in debug builds.
- `with_x` changes field x and nothing else (bit-isolation tests).

### Algorithms

**Smi tagging.** `SmallInt::new(v)`: check range, then `Oop(((v as u64) <<
SMI_SHIFT))`. Because `INT_TAG == 0` no OR is needed. `value()`: cast to
`i64`, arithmetic shift right by 2 (sign-preserving).

**Checked add/sub on tagged words.** With `INT_TAG = 0`, the tagged word of a
smi is exactly `v * 4` as an i64. So
`(self.0.raw() as i64).checked_add(rhs.0.raw() as i64)` overflows i64 exactly
when `v_l + v_r` overflows the *62-bit* range... **it does not** — i64 checked
math catches only 64-bit overflow. The correct sequence is:

1. `let sum = (a_tagged as i64).checked_add(b_tagged as i64)?;` — catches the
   extreme 64-bit case,
2. `if sum > SMI_MAX << 2 || sum < SMI_MIN << 2 { return None }` — catches the
   62-bit boundary. Equivalent single test: untag and compare, or check
   `sum << 2 >> 2 == sum`-style sign-extension idempotence on the *untagged*
   value. Implement as: `let v = sum >> 2; if v > SMI_MAX || v < SMI_MIN {
   None }` — note `sum >> 2` cannot itself lose information since tag bits
   are 0.

Simplest correct alternative (use this if in doubt): untag both, use
`i64::checked_add`, then `SmallInt::try_new(result)` — two range regimes, one
code path. `checked_mul` MUST use this untag-first path (tagged×tagged is
`16·v_l·v_r`, wrong scale). `checked_div`/`checked_rem` implement Smalltalk's
**floored** semantics (`-7 // 2 = -4`, `-7 \\ 2 = 1`), not Rust's truncating
`/`/`%`; guard `rhs == 0` (return `None`) and the `MIN / -1` case, which
overflows the smi range but not i64 — `try_new` catches it.

**Mark field update.** `with_age`: `Mark((self.0 & !MARK_AGE_MASK) |
((age as u64) << MARK_AGE_SHIFT))`. Same pattern for hash and single-bit
flags. `pristine()`: `MARK_TAG | (1 << MARK_SENTINEL_SHIFT)` = `0b111` = 7.

**Forwarding discrimination** operates on raw `u64` header words, not `Mark`s:
after a scavenge overwrites the header with a forwardee oop, the word has tag
`MEM_TAG` (01); a live mark has `MARK_TAG` (11). `word_is_forwarded(w)` is
`w & TAG_MASK == MEM_TAG`. The sentinel bit is SPEC-mandated belt-and-braces
(a mem oop's bit 2 is part of its address; sentinel=1 in real marks gives an
extra distinguisher for heap-verification code) — keep it, and have
`memory::verify` (S7) check both.

Edge cases to handle and test (see tests_s00.md for the full table):
- `SMI_MAX + 1`, `SMI_MIN - 1`, `SMI_MIN * -1`, `SMI_MAX * 2`, `SMI_MIN / -1`
  all return `None`.
- `SmallInt::new(-1)` word is `0xFFFF_FFFF_FFFF_FFFC` (all ones above the
  tag); `value()` must sign-extend correctly (arithmetic, not logical shift).
- `try_new(SMI_MAX)` and `try_new(SMI_MIN)` succeed and round-trip.
- `with_hash(u32::MAX)` then `age()` still 0; reserved bits still 0.
- Age saturation is the CALLER's business (scavenger, S7) — `with_age(128)`
  is a debug_assert failure, not saturation.

### Layer boundaries

- `oops/` depends on nothing in the crate. No `memory::`, no `runtime::`
  imports, no allocation, no `VmState`.
- `layout.rs` contains constants only — no functions, no types.
- Nothing outside `src/oops/` may name a tag value, shift, or mask; they
  import from `oops::layout` (re-exported via `oops::layout` path).
- `unsafe` policy: crate root gets `#![deny(unsafe_code)]`;
  `src/oops/mod.rs` gets `#![allow(unsafe_code)]` (module-scoped opt-back-in,
  the mechanism CONVENTIONS §1 prescribes). S0's only `unsafe` item is
  `from_oop_unchecked` (an unsafe *fn*, no unsafe *blocks* needed yet).

## Implementation order

1. Create `justfile` (targets below) and confirm `cargo test`/`cargo clippy`
   run clean on the untouched scaffold. `justfile` targets:
   `test` = `cargo test`; `lint` = `cargo fmt --check && cargo clippy
   --all-targets -- -D warnings`; `ci` = `just lint && just test`;
   `gate-s00` = `just ci` (later sprints append stress runs to their gates).
2. Delete `src/oops.rs`; create `src/oops/mod.rs` declaring submodules
   `layout`, `mark`, `smi`, `wrappers` and re-exporting `Oop`, `SmallInt`,
   `Mark`, and the wrapper types at `oops::` level. Update `src/lib.rs` with
   `#![deny(unsafe_code)]` (compiles: no unsafe exists yet).
3. `layout.rs` — all constants above, plus a `#[cfg(test)]` module asserting
   derived relationships (e.g. `MARK_HASH_SHIFT == MARK_AGE_SHIFT +
   MARK_AGE_BITS`, `SMI_MAX == -(SMI_MIN + 1)`).
4. `mod.rs` — `Oop` with predicates. Compile, test.
5. `smi.rs` — `SmallInt` + checked arithmetic. Compile, test.
6. `mark.rs` — `Mark`. Compile, test.
7. `wrappers.rs` — the newtype macro + nine types. Compile, test.
8. Run `just gate-s00`.

Each step leaves the crate compiling; commit granularity is per step if the
project is put under git (it currently is not — initializing a repo is
recommended but out of this sprint's scope).

## Pitfalls

- **The scaffold's tag scheme is a trap.** `src/oops.rs` uses 1-bit tags and
  `isize`. If any of it survives (e.g. the `SMI_SHIFT = 1`), every later
  sprint corrupts values silently. Delete the file in the same change that
  adds the directory; grep for `SMI_TAG_MASK`/`heap_addr` afterwards.
- **`u64` vs `isize`.** The pinned representation is `u64` (SPEC §2.1's
  `pub struct Oop(u64)`). Signed interpretation happens only inside
  `SmallInt::value()` via an explicit `as i64` + arithmetic shift. Do not
  make `Oop` signed "for convenience" — address arithmetic and tag tests
  want unsigned.
- **Logical vs arithmetic shift.** `raw >> 2` on `u64` is logical and breaks
  negative smis. Always `(raw as i64) >> 2`.
- **62-bit overflow is not 64-bit overflow.** `i64::checked_add` alone does
  NOT implement smi overflow; the ±2^61 range check is separate (algorithm
  above). This is the exact bug class SPEC §1.3's LargeInteger fallback
  depends on catching — an arithmetic primitive that misses overflow produces
  wrong *values*, not crashes, and surfaces sprints later.
- **Floored vs truncated division.** Smalltalk `//` and `\\` are floored.
  Rust `/`/`%` truncate. Get it right in S0 so S3's primitives are a thin
  wrapper; test `-7//2`, `7//-2`, `-7\\2`.
- **Alignment is 8, not 16** (SPEC §2.1: heap tag works because objects are
  8-byte aligned). Strongtalk/HotSpot habits suggest 16; resist — the tag
  scheme only needs 4-byte alignment mathematically, and the pinned
  `ALLOC_ALIGN` is 8. Encoding it wrong here poisons all S1 size math.
- **Mark words are not oops.** Strongtalk's `markOop` *is* an oop subtype;
  MACVM's is not (SPEC §2.2 layout differs — hash is 32 bits at 43:12, age 7
  bits at 11:5, sentinel at bit 2; do NOT copy Strongtalk's 32-bit
  `sentinel:1 near_death:1 tagged_contents:1 age:7 hash:20 tag:2` order from
  the reference analysis §3.1). Keep `Mark` a separate type over `u64` so the
  type system stops mark/oop mixing.
- **No age/size punning.** Strongtalk stuffs object size into the mark's age
  field during GC (analysis §3.1 calls this out as a 32-bit-cramped hack;
  SPEC §2.2 Δ explicitly drops it). Do not "reserve" that behavior; S8 uses a
  side map.
- **hash = 0 means UNASSIGNED** (SPEC §2.2). The identity-hash assignment
  path (S1) must never hand out 0, and any code comparing hashes must treat 0
  as "no hash yet", never as a valid hash. Encode this in `Mark::hash()` docs
  now.
- **`#[must_use]` on `with_*`.** `mark.with_age(3);` silently discarding the
  result is the classic immutable-update bug; the attribute makes it a
  warning, and `-D warnings` in the lint target makes it an error.
- **Don't implement `Deref`/`From<u64>` conveniences** on `Oop`. Every
  implicit conversion is a place a mark word or raw address can leak into oop
  position. Conversions stay named and greppable.

## Interfaces for later sprints

S1 (allocator, genesis) will call:
```rust
oops::layout::{WORD_SIZE, ALLOC_ALIGN, MARK_OFFSET, KLASS_OFFSET, BODY_OFFSET, HEADER_WORDS};
Mark::pristine() -> Mark;  Mark::word(self) -> u64;
Mark::with_hash / hash / with_tagged_contents;
Oop::from_raw / raw / is_smi / is_mem / mem_addr;
SmallInt::{new, try_new, value, oop, try_from};
MemOop::{try_from, from_oop_unchecked, oop, addr};   // + the typed siblings
```
S1 adds to (does not change) these: header read/write accessors on `MemOop`
(`klass()`, `mark()`, `set_klass()`, …) and format-checking `try_from`
upgrades on the typed wrappers. S3's smi primitives will wrap
`SmallInt::checked_*` directly; their `None` becomes `PrimResult::Fail`.

## Out of scope

- Heap objects, headers in memory, allocation — S1.
- Format-based wrapper validation (`ArrayOop::try_from` checking the klass
  format) — S1, once klass headers exist.
- `Handle`/`HandleScope` — S7 (retrofit), though the accessor discipline that
  makes it possible is designed in S1.
- Immediate `Character` on `RESERVED_TAG` — explicitly unused in v1
  (SPEC §2.1); `from_raw` rejects it.
- Large-integer fallback on smi overflow — S6 library code over S3
  primitives; S0 only reports `None`.
- GitHub Actions / hosted CI — the `justfile` is the CI contract for now; the
  directory is not a git repository yet.
