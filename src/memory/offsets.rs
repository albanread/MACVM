//! The object-start offset table (SPEC §7.4/7.5): one `u8` per old-gen
//! card, letting a dirty-card scan find the header of the object covering
//! any given card without a linear walk from `old.bounds.start`.
//!
//! Entry semantics for card *i* (must let a scan find the header of the
//! object covering `card_base(i)`):
//! - `0..=64` — that object's header is at `card_base(i) − entry*8` bytes
//!   (0 = header exactly at the card base; up to one full card back).
//! - `65..=255` — back-skip `entry − 64` cards (1..=191) and re-consult
//!   that card's entry; entries chain for objects spanning > 191 cards
//!   (a 256 KiB direct allocation spans up to 512 cards — the chain is
//!   exercised in practice, not just a theoretical case).
//!
//! Card `c0` (the card *containing* an object's header, as opposed to a
//! card the object merely *covers* further in) only gets an entry if the
//! header sits exactly at `card_base(c0)` — otherwise whatever object
//! precedes it in memory is what covers `card_base(c0)`, and this table
//! must not overwrite that entry. `c0 + 1` is always safely resolvable in
//! DIRECT form (its distance from any header inside card `c0` is at most
//! one card, i.e. `<= 64` words) — every chain in this module is
//! constructed to bottom out there.

use super::cards::{CARD_SHIFT, CARD_SIZE};
use super::layout::SpaceBounds;
use super::reservation::Reservation;
use crate::oops::layout::WORD_SIZE;

/// Direct-form entries are `0..=64`; chain-form entries `65..=255` encode
/// a back-skip of `1..=MAX_CHAIN_SKIP` cards.
const MAX_DIRECT: u8 = 64;
const MAX_CHAIN_SKIP: usize = 191;

pub struct OffsetTable {
    backing: Reservation,
    old_start: usize,
    n_cards: usize,
}

impl OffsetTable {
    /// `old_reserved` is `HeapLayout::old` — sized identically to (and for
    /// the same reason as) `CardTable`'s backing.
    pub fn new(old_reserved: SpaceBounds) -> OffsetTable {
        let n_cards = old_reserved.len().div_ceil(CARD_SIZE).max(1);
        let backing = Reservation::reserve(n_cards);
        backing.commit(0, n_cards);
        OffsetTable {
            backing,
            old_start: old_reserved.start,
            n_cards,
        }
    }

    fn entry_ptr(&self, i: usize) -> *mut u8 {
        debug_assert!(
            i < self.n_cards,
            "card index {i} out of range ({})",
            self.n_cards
        );
        (self.backing.base() + i) as *mut u8
    }

    fn set_entry(&self, i: usize, v: u8) {
        // SAFETY: `i < n_cards` (checked in `entry_ptr`); `backing`
        // commits exactly `n_cards` bytes at construction.
        unsafe { self.entry_ptr(i).write(v) }
    }

    fn get_entry(&self, i: usize) -> u8 {
        // SAFETY: as above.
        unsafe { self.entry_ptr(i).read() }
    }

    #[inline]
    pub fn card_index(&self, addr: usize) -> usize {
        (addr - self.old_start) >> CARD_SHIFT
    }

    #[inline]
    pub fn card_base(&self, i: usize) -> usize {
        self.old_start + (i << CARD_SHIFT)
    }

    /// Zero every entry (S8 phase D: full GC rebuilds the whole table from
    /// scratch as it places compacted objects, rather than patching the
    /// pre-compaction one — old.top can shrink, and every surviving object's
    /// address changes, so nothing in the old table is trustworthy). Only
    /// cards below the (possibly new, smaller) `old.top` are ever consulted
    /// (`dirty_card_scan`/`resolve` callers all stop there), so a zeroed
    /// entry past it is inert, never misread as a real header distance.
    pub fn clear(&mut self) {
        for i in 0..self.n_cards {
            self.set_entry(i, 0);
        }
    }

    /// Maintenance, called by `OldGen::allocate` for every freshly
    /// allocated object: records entries for every card whose base falls
    /// inside `[addr, addr + size_bytes)`, per this module's encoding.
    pub fn record_object(&mut self, addr: usize, size_bytes: usize) {
        debug_assert_eq!(addr % WORD_SIZE, 0, "object header must be word-aligned");
        debug_assert!(size_bytes > 0);
        let c0 = self.card_index(addr);
        let c_last = self.card_index(addr + size_bytes - 1);
        if addr == self.card_base(c0) {
            self.set_entry(c0, 0);
        }
        for i in (c0 + 1)..=c_last {
            let distance_words = (self.card_base(i) - addr) / WORD_SIZE;
            if distance_words <= MAX_DIRECT as usize {
                self.set_entry(i, distance_words as u8);
            } else {
                // Skip back as far as possible (up to MAX_CHAIN_SKIP)
                // without passing c0+1, which is always DIRECT-resolvable
                // (see module doc) — landing there or on an
                // already-processed earlier card in this same object.
                let skip = (i - (c0 + 1)).min(MAX_CHAIN_SKIP);
                self.set_entry(i, MAX_DIRECT + skip as u8);
            }
        }
    }

    /// Resolve card `i`'s chain to the header address of the object
    /// covering `card_base(i)`. Callers must only call this for a card
    /// that actually has a recorded entry (below `old.top`, per the
    /// module's maintenance contract) — an unrecorded (stale/never-set)
    /// entry has no defined meaning.
    pub fn resolve(&self, mut i: usize) -> usize {
        loop {
            let e = self.get_entry(i);
            if e <= MAX_DIRECT {
                return self.card_base(i) - (e as usize) * WORD_SIZE;
            }
            let skip = (e - MAX_DIRECT) as usize;
            debug_assert!(skip <= i, "offset chain walked before old.bounds.start");
            i -= skip;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(old_len: usize) -> OffsetTable {
        OffsetTable::new(SpaceBounds {
            start: 0x20_0000,
            end: 0x20_0000 + old_len,
        })
    }

    /// `tests_s07.md`'s `offset_direct_entry`: object header 0/8/512 bytes
    /// before a card base resolves via entries 0/1/64.
    #[test]
    fn offset_direct_entry() {
        let mut t = table(8 << 20);
        let base = 0x20_0000;

        // Header exactly at a card base (card 3): entry 0.
        let hdr0 = base + 3 * CARD_SIZE;
        t.record_object(hdr0, 8);
        assert_eq!(t.resolve(3), hdr0);

        // Header 8 bytes before card 6's base (so card 6's distance is 1
        // word): object spans cards 5..=6.
        let hdr1 = base + 6 * CARD_SIZE - 8;
        t.record_object(hdr1, 16);
        assert_eq!(t.resolve(6), hdr1);

        // Header exactly 512 bytes (one full card, 64 words) before card
        // 10's base: object starts at card 9's base and spans to card 10.
        let hdr2 = base + 9 * CARD_SIZE;
        t.record_object(hdr2, CARD_SIZE + 8);
        assert_eq!(t.resolve(10), hdr2);
        assert_eq!(t.resolve(9), hdr2);
    }

    /// `tests_s07.md`'s `offset_chain_long_object`: a 256 KiB old object
    /// (512 cards) resolves from its last card through >= 2 chained
    /// entries.
    #[test]
    fn offset_chain_long_object() {
        let mut t = table(1 << 20); // 1 MiB reserved, plenty of cards
        let base = 0x20_0000;
        let hdr = base; // card-aligned, card 0
        let size = 256 << 10; // 256 KiB => 512 cards
        t.record_object(hdr, size);

        let c_last = t.card_index(hdr + size - 1);
        assert_eq!(c_last, 511);
        // The last card's entry must itself be a chain (can't reach 512
        // cards back directly — MAX_DIRECT is 64 words = 1 card).
        assert!(t.get_entry(c_last) > MAX_DIRECT);
        assert_eq!(t.resolve(c_last), hdr);

        // Spot-check a handful of interior cards too.
        for &i in &[1usize, 63, 64, 191, 192, 383, 500] {
            assert_eq!(t.resolve(i), hdr, "card {i} must resolve to the header");
        }
    }

    /// `tests_s07.md`'s `offset_updated_on_alloc`: consecutive
    /// `OldGen::allocate` calls (simulated here via direct `record_object`
    /// calls at increasing addresses) leave every card below `top`
    /// resolvable.
    #[test]
    fn offset_updated_on_alloc() {
        let mut t = table(4 << 20);
        let base = 0x20_0000;
        let mut addr = base;
        let mut headers = Vec::new();
        // A mix of small and multi-card objects, back to back.
        for size in [8usize, 3000, CARD_SIZE, 4 * CARD_SIZE + 16, 40] {
            headers.push((addr, size));
            t.record_object(addr, size);
            addr += (size + 7) & !7;
        }
        let top = addr;
        let last_card = t.card_index(top - 8);
        for i in 0..=last_card {
            let resolved = t.resolve(i);
            let covers = headers
                .iter()
                .any(|&(h, s)| resolved == h && t.card_base(i) < h + s);
            assert!(
                covers,
                "card {i} resolved to {resolved:#x}, which doesn't cover its base"
            );
        }
    }
}
