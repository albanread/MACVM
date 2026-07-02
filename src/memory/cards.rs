//! The card table (SPEC §7.4): one byte per 512-byte old-gen "card",
//! `AtomicU8` (single mutator thread ⇒ `Relaxed` everywhere; kept atomic so
//! the same table works unchanged if parallel mutators ever appear, and
//! because it's the exact shape proven in MacNCL). Polarity is flipped
//! from the usual GC convention: **dirty = 0**, so S11's compiled fast
//! path can dirty a card with `strb wzr, [xcards, xtmp]` (a zero-register
//! store) instead of loading a `1` constant first.

use std::sync::atomic::{AtomicU8, Ordering};

use super::layout::SpaceBounds;
use super::reservation::Reservation;

/// 512-byte cards.
pub const CARD_SHIFT: u32 = 9;
pub const CARD_SIZE: usize = 1 << CARD_SHIFT;
pub const CARD_DIRTY: u8 = 0;
pub const CARD_CLEAN: u8 = 1;

/// Backed by its own small `Reservation` sized to cover OLD'S RESERVED
/// MAXIMUM (not just the currently-committed prefix) — committed in full
/// at construction, since one byte per card is cheap even for an 8 GiB old
/// gen (~16 MiB of card bytes), and it avoids having to grow the card
/// table in lockstep with S8's old-gen growth later.
pub struct CardTable {
    backing: Reservation,
    old_start: usize,
    n_cards: usize,
}

impl CardTable {
    /// `old_reserved` is `HeapLayout::old` — the full reserved range, not
    /// just what's committed. All cards start `CLEAN` (a fresh `mmap`
    /// commit is zero-filled by the OS, which is `CARD_DIRTY` under this
    /// module's flipped polarity — every entry is explicitly set `CLEAN`
    /// after commit, never left at its zero-fill default).
    pub fn new(old_reserved: SpaceBounds) -> CardTable {
        let n_cards = old_reserved.len().div_ceil(CARD_SIZE).max(1);
        let backing = Reservation::reserve(n_cards);
        backing.commit(0, n_cards);
        let table = CardTable {
            backing,
            old_start: old_reserved.start,
            n_cards,
        };
        for i in 0..n_cards {
            table.entry(i).store(CARD_CLEAN, Ordering::Relaxed);
        }
        table
    }

    fn entry(&self, i: usize) -> &AtomicU8 {
        debug_assert!(
            i < self.n_cards,
            "card index {i} out of range ({})",
            self.n_cards
        );
        // SAFETY: `i < n_cards` (checked above), and `backing` reserves
        // and commits exactly `n_cards` bytes at construction — the byte
        // at offset `i` is always valid, and `AtomicU8` has the same
        // layout as `u8` so aliasing this committed memory is sound
        // (single mutator thread; no other reference to this byte exists
        // outside this table).
        unsafe { &*(self.backing.base() as *const AtomicU8).add(i) }
    }

    #[inline]
    pub fn card_index(&self, slot_addr: usize) -> usize {
        (slot_addr - self.old_start) >> CARD_SHIFT
    }

    #[inline]
    pub fn card_base(&self, i: usize) -> usize {
        self.old_start + (i << CARD_SHIFT)
    }

    #[inline]
    pub fn dirty_for_slot(&self, slot_addr: usize) {
        self.entry(self.card_index(slot_addr))
            .store(CARD_DIRTY, Ordering::Relaxed);
    }

    /// Dirty every card overlapping `[start, end)` — used by promotion
    /// (SPEC §7.3 step 2, an object's whole body may reference new-gen
    /// survivors) and future bulk primitives (`replaceFrom:to:with:`).
    pub fn record_multistores(&self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        let first = self.card_index(start);
        let last = self.card_index(end - 1);
        for i in first..=last {
            self.entry(i).store(CARD_DIRTY, Ordering::Relaxed);
        }
    }

    pub fn is_dirty(&self, i: usize) -> bool {
        self.entry(i).load(Ordering::Relaxed) == CARD_DIRTY
    }

    pub fn set_clean(&self, i: usize) {
        self.entry(i).store(CARD_CLEAN, Ordering::Relaxed);
    }

    pub fn n_cards(&self) -> usize {
        self.n_cards
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(old_len: usize) -> CardTable {
        CardTable::new(SpaceBounds {
            start: 0x10_0000,
            end: 0x10_0000 + old_len,
        })
    }

    /// `tests_s07.md`'s `card_index_math`: slot→card index for card 0
    /// start, card 0 end−8, card 1 start, last committed slot.
    #[test]
    fn card_index_math() {
        let t = table(4 << 20);
        let base = 0x10_0000;
        assert_eq!(t.card_index(base), 0);
        assert_eq!(t.card_index(base + CARD_SIZE - 8), 0);
        assert_eq!(t.card_index(base + CARD_SIZE), 1);
        let last_card = t.n_cards() - 1;
        let last_slot = base + last_card * CARD_SIZE + CARD_SIZE - 8;
        assert_eq!(t.card_index(last_slot), last_card);
    }

    #[test]
    fn fresh_table_all_clean() {
        let t = table(4 << 20);
        for i in 0..t.n_cards() {
            assert!(!t.is_dirty(i), "card {i} should start clean");
        }
    }

    #[test]
    fn dirty_for_slot_and_set_clean() {
        let t = table(4 << 20);
        let base = 0x10_0000;
        let slot = base + 5 * CARD_SIZE + 16;
        assert!(!t.is_dirty(5));
        t.dirty_for_slot(slot);
        assert!(t.is_dirty(5));
        t.set_clean(5);
        assert!(!t.is_dirty(5));
    }

    /// `tests_s07.md`'s `record_multistores_range`: ranges spanning
    /// 1/2/513 cards dirty exactly the overlapped cards.
    #[test]
    fn record_multistores_range() {
        let t = table(600 * CARD_SIZE);
        let base = 0x10_0000;

        // Exactly 1 card.
        t.record_multistores(base + 10 * CARD_SIZE, base + 10 * CARD_SIZE + 8);
        assert!(t.is_dirty(10));
        assert!(!t.is_dirty(9));
        assert!(!t.is_dirty(11));

        // Exactly 2 cards (crossing one boundary).
        let s = base + 20 * CARD_SIZE + CARD_SIZE - 8;
        let e = s + 16; // spills 8 bytes into card 21
        t.record_multistores(s, e);
        assert!(t.is_dirty(20));
        assert!(t.is_dirty(21));
        assert!(!t.is_dirty(22));

        // 513 cards.
        let s2 = base + 30 * CARD_SIZE;
        let e2 = s2 + 513 * CARD_SIZE;
        t.record_multistores(s2, e2);
        for i in 30..30 + 513 {
            assert!(t.is_dirty(i), "card {i} should be dirty");
        }
        assert!(!t.is_dirty(30 + 513));
    }

    /// `tests_s07.md`'s `store_barrier_matrix`'s card-table half: a mid-card
    /// slot address dirties only its own card, none of its neighbors.
    #[test]
    fn dirty_for_slot_isolates_card() {
        let t = table(4 << 20);
        let base = 0x10_0000;
        t.dirty_for_slot(base + 100 * CARD_SIZE + 256);
        for i in 0..t.n_cards() {
            let expected = i == 100;
            assert_eq!(t.is_dirty(i), expected, "card {i}");
        }
    }
}
