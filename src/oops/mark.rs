//! `Mark` — the header mark word (SPEC §2.2): identity hash, age, and GC
//! flags. A `Mark` is a distinct type from `Oop` (unlike Strongtalk's
//! `markOop`, which is an oop subtype) — mark words and oops must never mix,
//! and the type system enforces that here.
//!
//! Forwarding discrimination operates on **raw header words**, not `Mark`
//! values: once a scavenge overwrites a header with a forwardee oop, the
//! word is no longer a mark at all (its tag is `MEM_TAG`, not `MARK_TAG`).
//! Use the free functions [`word_is_forwarded`] / [`forwardee`] on the raw
//! word; never call [`Mark::from_word`] on a header that might already be
//! forwarded.

use super::layout::{
    MARK_AGE_MASK, MARK_AGE_MAX, MARK_AGE_SHIFT, MARK_GC_MARK_MASK, MARK_HASH_MASK,
    MARK_HASH_SHIFT, MARK_NEAR_DEATH_MASK, MARK_PRISTINE, MARK_SENTINEL_MASK, MARK_TAG,
    MARK_TAGGED_CONTENTS_MASK, MEM_TAG, TAG_MASK,
};
use super::Oop;

#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Mark(u64);

impl Mark {
    /// The mark of a freshly allocated object: tag=MARK_TAG, sentinel=1,
    /// everything else zero.
    #[inline]
    pub const fn pristine() -> Mark {
        Mark(MARK_PRISTINE)
    }

    /// Build a `Mark` from a raw header word. In debug builds, asserts the
    /// word is actually a mark (tag == MARK_TAG and sentinel == 1) — a
    /// forwarded header must never reach this constructor.
    #[inline]
    pub fn from_word(w: u64) -> Mark {
        debug_assert!(
            w & TAG_MASK == MARK_TAG,
            "from_word: {w:#x} does not have MARK_TAG — is this a forwarded header?"
        );
        debug_assert!(
            w & MARK_SENTINEL_MASK != 0,
            "from_word: {w:#x} has MARK_TAG but sentinel bit is 0"
        );
        Mark(w)
    }

    #[inline]
    pub const fn word(self) -> u64 {
        self.0
    }

    #[inline]
    pub fn age(self) -> u8 {
        ((self.0 & MARK_AGE_MASK) >> MARK_AGE_SHIFT) as u8
    }

    #[inline]
    #[must_use]
    pub fn with_age(self, age: u8) -> Mark {
        debug_assert!(age <= MARK_AGE_MAX, "with_age: {age} exceeds MARK_AGE_MAX");
        Mark((self.0 & !MARK_AGE_MASK) | ((age as u64) << MARK_AGE_SHIFT))
    }

    /// The identity hash. `0` means "not yet assigned" (SPEC §2.2) — the
    /// assignment counter (S1) must never hand out 0, and comparisons must
    /// treat 0 as "no hash", never as a valid one.
    #[inline]
    pub fn hash(self) -> u32 {
        ((self.0 & MARK_HASH_MASK) >> MARK_HASH_SHIFT) as u32
    }

    #[inline]
    #[must_use]
    pub fn with_hash(self, h: u32) -> Mark {
        Mark((self.0 & !MARK_HASH_MASK) | ((h as u64) << MARK_HASH_SHIFT))
    }

    #[inline]
    pub fn near_death(self) -> bool {
        self.0 & MARK_NEAR_DEATH_MASK != 0
    }

    #[inline]
    #[must_use]
    pub fn with_near_death(self, v: bool) -> Mark {
        if v {
            Mark(self.0 | MARK_NEAR_DEATH_MASK)
        } else {
            Mark(self.0 & !MARK_NEAR_DEATH_MASK)
        }
    }

    #[inline]
    pub fn tagged_contents(self) -> bool {
        self.0 & MARK_TAGGED_CONTENTS_MASK != 0
    }

    #[inline]
    #[must_use]
    pub fn with_tagged_contents(self, v: bool) -> Mark {
        if v {
            Mark(self.0 | MARK_TAGGED_CONTENTS_MASK)
        } else {
            Mark(self.0 & !MARK_TAGGED_CONTENTS_MASK)
        }
    }

    /// The full-GC mark bit (SPEC §2.2 bit 44, S8). Set only *between* the
    /// mark and forwarding-compute phases of a full GC to record liveness;
    /// **INVARIANT 0** for the mutator and at every other time — `verify_heap`
    /// and the mark-word field-isolation unit test assert it stays clear
    /// outside a collection, and `is_pristine` (a freshly allocated header)
    /// implies it is clear.
    #[inline]
    pub fn gc_mark(self) -> bool {
        self.0 & MARK_GC_MARK_MASK != 0
    }

    #[inline]
    #[must_use]
    pub fn with_gc_mark(self, v: bool) -> Mark {
        if v {
            Mark(self.0 | MARK_GC_MARK_MASK)
        } else {
            Mark(self.0 & !MARK_GC_MARK_MASK)
        }
    }

    #[inline]
    pub fn is_pristine(self) -> bool {
        self.0 == MARK_PRISTINE
    }
}

/// `true` iff the raw header word `w` is a forwarding pointer (a scavenged
/// or compacted object's original header, overwritten with its forwardee).
/// SPEC §2.2: `is_forwarded ⇔ (word & TAG_MASK) == MEM_TAG`.
#[inline]
pub fn word_is_forwarded(w: u64) -> bool {
    w & TAG_MASK == MEM_TAG
}

/// The forwardee oop of a forwarded header word. Caller must ensure
/// `word_is_forwarded(w)`.
#[inline]
pub fn forwardee(w: u64) -> Oop {
    debug_assert!(word_is_forwarded(w), "forwardee: {w:#x} is not forwarded");
    Oop::from_raw_unchecked(w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oops::layout::SMI_SHIFT;

    #[test]
    fn mark_pristine() {
        let m = Mark::pristine();
        assert_eq!(m.word(), 0b111);
        assert_eq!(m.age(), 0);
        assert_eq!(m.hash(), 0);
        assert!(!m.near_death());
        assert!(!m.tagged_contents());
        assert!(m.is_pristine());
    }

    #[test]
    fn mark_field_isolation_age() {
        let base = Mark::pristine()
            .with_hash(0xDEAD_BEEF)
            .with_near_death(true);
        let m = base.with_age(127);
        assert_eq!(m.age(), 127);
        assert_eq!(m.hash(), 0xDEAD_BEEF);
        assert!(m.near_death());
        assert!(!m.tagged_contents());
        assert_eq!(m.word() & TAG_MASK, MARK_TAG);
        assert_ne!(m.word() & MARK_SENTINEL_MASK, 0);
        assert_eq!(m.word() >> 44, 0);
    }

    #[test]
    fn mark_field_isolation_hash() {
        let base = Mark::pristine()
            .with_age(127)
            .with_near_death(true)
            .with_tagged_contents(true);
        let m1 = base.with_hash(u32::MAX);
        assert_eq!(m1.hash(), u32::MAX);
        assert_eq!(m1.age(), 127);
        assert!(m1.near_death());
        assert!(m1.tagged_contents());

        let m2 = m1.with_hash(0);
        assert_eq!(m2.hash(), 0);
        assert_eq!(m2.age(), 127);
        assert!(m2.near_death());
        assert!(m2.tagged_contents());
    }

    #[test]
    fn mark_field_isolation_flags() {
        let base = Mark::pristine().with_age(64).with_hash(1);
        let m1 = base.with_near_death(true);
        assert!(m1.near_death());
        assert!(!m1.tagged_contents());
        assert_eq!(m1.age(), 64);
        assert_eq!(m1.hash(), 1);

        let m2 = base.with_tagged_contents(true);
        assert!(!m2.near_death());
        assert!(m2.tagged_contents());
        assert_eq!(m2.age(), 64);
        assert_eq!(m2.hash(), 1);
    }

    #[test]
    fn mark_gc_mark_bit() {
        // Pristine (freshly allocated) is unmarked.
        assert!(!Mark::pristine().gc_mark());

        // Set/clear round-trips, and setting it disturbs no other field —
        // even a fully-populated mark word.
        let base = Mark::pristine()
            .with_age(127)
            .with_hash(0xDEAD_BEEF)
            .with_near_death(true)
            .with_tagged_contents(true);
        let marked = base.with_gc_mark(true);
        assert!(marked.gc_mark());
        assert_eq!(marked.age(), 127);
        assert_eq!(marked.hash(), 0xDEAD_BEEF);
        assert!(marked.near_death());
        assert!(marked.tagged_contents());
        assert_eq!(marked.word() & TAG_MASK, MARK_TAG);
        assert_ne!(marked.word() & MARK_SENTINEL_MASK, 0);

        // Clearing restores the exact pre-mark word (invariant-0 outside GC).
        let cleared = marked.with_gc_mark(false);
        assert!(!cleared.gc_mark());
        assert_eq!(cleared.word(), base.word());

        // The other setters never touch bit 44 (mirrors the `>> 44 == 0`
        // assertions in the field-isolation tests).
        assert!(!base.gc_mark());
    }

    #[test]
    fn mark_age_range() {
        let _ = Mark::pristine().with_age(127);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "exceeds MARK_AGE_MAX")]
    fn mark_age_range_overflow_panics() {
        let _ = Mark::pristine().with_age(128);
    }

    #[test]
    fn mark_forwarding_discrimination() {
        assert!(!word_is_forwarded(Mark::pristine().word()));

        let fake_mem_word = 0x1000u64 + 1;
        assert!(word_is_forwarded(fake_mem_word));
        assert_eq!(forwardee(fake_mem_word).raw(), fake_mem_word);

        let smi_word = 1u64 << SMI_SHIFT;
        assert!(!word_is_forwarded(smi_word));
    }

    #[test]
    fn mark_from_word_validates() {
        let _ = Mark::from_word(Mark::pristine().word());
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "sentinel bit is 0")]
    fn mark_from_word_rejects_no_sentinel() {
        // MARK_TAG set (0b11) but sentinel bit (bit 2) is 0.
        let _ = Mark::from_word(0b011);
    }

    #[test]
    fn mark_word_roundtrip() {
        let m = Mark::pristine()
            .with_age(9)
            .with_hash(12345)
            .with_tagged_contents(true);
        assert_eq!(Mark::from_word(m.word()), m);
        let expected: u64 = 0b11 | 0b100 | (1 << 4) | (9 << 5) | (12345 << 12);
        assert_eq!(m.word(), expected);
    }
}
