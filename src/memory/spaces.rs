//! Mutable per-space allocation state (SPEC §7.1–§7.3): `Eden` (S1) plus
//! S7's `SurvivorSpace` (bump-allocated, same shape as `Eden` — used as
//! both scavenge copy targets) and `OldGen` (bump-allocated within a
//! lazily-growing committed prefix of its reserved range). Bounds come
//! from `layout::HeapLayout`; only the `top`/`committed_end` pointers here
//! are mutable.

use super::layout::SpaceBounds;
use super::offsets::OffsetTable;

/// A contiguous bump-allocation region: `[start, top)` is live, `[top, end)`
/// is free. `start`/`end` are fixed once committed; only `top` moves.
pub struct Eden {
    pub start: usize,
    pub top: usize,
    pub end: usize,
}

impl Eden {
    pub fn new(start: usize, size: usize) -> Eden {
        Eden {
            start,
            top: start,
            end: start + size,
        }
    }

    #[inline]
    pub fn remaining(&self) -> usize {
        self.end - self.top
    }
}

/// One of the two survivor spaces (`from`/`to`) — a scavenge copy
/// destination, bump-allocated exactly like `Eden`. Whichever one is
/// currently `to` receives this cycle's copies; after a scavenge the two
/// are swapped (S7-7).
pub struct SurvivorSpace {
    pub start: usize,
    pub top: usize,
    pub end: usize,
}

impl SurvivorSpace {
    pub fn new(bounds: SpaceBounds) -> SurvivorSpace {
        SurvivorSpace {
            start: bounds.start,
            top: bounds.start,
            end: bounds.end,
        }
    }

    #[inline]
    pub fn remaining(&self) -> usize {
        self.end - self.top
    }

    /// Reset to empty (start of a scavenge: the space about to become `to`
    /// is reset before copying into it; the space about to become the
    /// freed `from` is reset after the swap).
    pub fn reset(&mut self) {
        self.top = self.start;
    }
}

/// The old generation: reserved to `bounds.end` (the rest of the
/// reservation), but only `[bounds.start, committed_end)` is actually
/// committed and thus allocatable; `top` bump-allocates within that
/// committed prefix. Growing `committed_end` further (beyond the boot-time
/// first segment) is S8's concern — S7 exhausting the initial segment is
/// the designed `GcStallError` cascade endpoint, not a bug.
pub struct OldGen {
    pub bounds: SpaceBounds,
    pub committed_end: usize,
    pub top: usize,
}

impl OldGen {
    pub fn new(bounds: SpaceBounds, committed_end: usize) -> OldGen {
        debug_assert!(committed_end <= bounds.end);
        OldGen {
            bounds,
            committed_end,
            top: bounds.start,
        }
    }

    #[inline]
    pub fn remaining(&self) -> usize {
        self.committed_end - self.top
    }

    /// Free committed bytes below `committed_end` — what a promotion or direct
    /// large allocation can still take without growing (S8 promotion guarantee
    /// / post-full-GC headroom policy read this).
    #[inline]
    pub fn free_bytes(&self) -> usize {
        self.committed_end - self.top
    }

    /// Commit the next `segment`-byte slice of the reserved old range (S8 step
    /// 2), advancing `committed_end`. Returns the number of bytes newly
    /// committed — 0 iff the reservation is already exhausted (`committed_end
    /// == bounds.end`), at which point the caller escalates to the cascade's
    /// terminal `GcStallError`. Growth clamps to `bounds.end`, so the last
    /// segment may be short.
    ///
    /// `is_old` is untouched — the young/old boundary is the reservation slice
    /// (`bounds`), never `committed_end`. The card and offset tables already
    /// cover old's FULL reserved range (committed in full at genesis, not
    /// per-segment), so growth extends only the heap commit here; a
    /// freshly-committed segment is zero-filled by the OS and `OldGen::allocate`
    /// fills each object body before use, so no stale bytes are ever read.
    pub fn grow(&mut self, reservation: &super::reservation::Reservation, segment: usize) -> usize {
        let new_end = self
            .committed_end
            .saturating_add(segment.max(1))
            .min(self.bounds.end);
        let grown = new_end - self.committed_end;
        if grown == 0 {
            return 0; // reservation exhausted — terminal stall upstream
        }
        reservation.commit(self.committed_end - reservation.base(), grown);
        self.committed_end = new_end;
        grown
    }

    /// The ONLY way memory enters old gen (direct large alloc + promotion,
    /// S7-7). `fill` initializes the freshly bumped body (nil-oop fill or
    /// byte-zero fill, per the object's own format — never trust reclaimed
    /// memory, lesson 5) before the offset table is updated, so a
    /// concurrent-with-construction dirty-card scan (there is none yet in
    /// v1's single-mutator model, but the ordering is the honest one to
    /// keep) never observes a recorded-but-uninitialized object. Returns
    /// `None` when the committed prefix is full — the caller decides the
    /// cascade step (S8 growth, full GC); no silent capping (lesson 4).
    pub fn allocate(
        &mut self,
        offsets: &mut OffsetTable,
        size_bytes: usize,
        fill: impl FnOnce(usize),
    ) -> Option<usize> {
        let size_bytes = (size_bytes + 7) & !7; // 8-byte align
        let new_top = self.top.checked_add(size_bytes)?;
        if new_top > self.committed_end {
            return None;
        }
        let addr = self.top;
        self.top = new_top;
        fill(addr);
        offsets.record_object(addr, size_bytes);
        Some(addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn old_gen_and_offsets(len: usize) -> (OldGen, OffsetTable) {
        let bounds = SpaceBounds {
            start: 0x30_0000,
            end: 0x30_0000 + len,
        };
        (OldGen::new(bounds, bounds.end), OffsetTable::new(bounds))
    }

    /// S8 step 2: `OldGen::grow` commits successive segments up to the
    /// reservation ceiling, then returns 0 (the cascade's terminal stall),
    /// and a grown region is real, writable memory.
    #[test]
    fn oldgen_grows_and_exhausts() {
        use crate::memory::reservation::Reservation;
        const SEG: usize = 64 * 1024; // a multiple of any target's page size

        let backing = Reservation::reserve(4 * SEG);
        let base = backing.base();
        let bounds = SpaceBounds {
            start: base,
            end: base + 4 * SEG,
        };
        backing.commit(0, SEG); // only the first segment committed at boot
        let mut old = OldGen::new(bounds, base + SEG);
        let mut offsets = OffsetTable::new(bounds);

        // Fill the committed first segment exactly; the next bump misses.
        assert_eq!(old.free_bytes(), SEG);
        assert!(old.allocate(&mut offsets, SEG, |_| {}).is_some());
        assert!(old.allocate(&mut offsets, 8, |_| {}).is_none());

        // Grow one segment; the retry now fits AND the memory is writable
        // (proves the freshly-committed range is backed, not just bookkept).
        assert_eq!(old.grow(&backing, SEG), SEG);
        assert_eq!(old.committed_end, base + 2 * SEG);
        let addr = old
            .allocate(&mut offsets, 4096, |a| unsafe {
                (a as *mut u64).write(0xDEAD_BEEF)
            })
            .expect("alloc into the grown segment fits");
        assert!(addr >= base + SEG && addr < old.committed_end);
        assert_eq!(unsafe { (addr as *const u64).read() }, 0xDEAD_BEEF);

        // Grow to the reservation ceiling; the last grow clamps, then 0.
        assert_eq!(old.grow(&backing, SEG), SEG); // -> 3*SEG
        assert_eq!(old.grow(&backing, 10 * SEG), SEG); // clamped to bounds.end
        assert_eq!(old.committed_end, bounds.end);
        assert_eq!(old.grow(&backing, SEG), 0); // reservation exhausted
        assert_eq!(old.committed_end, bounds.end); // unchanged after a 0-grow
    }

    /// `tests_s07.md`'s `oldgen_alloc_none_when_full`: returns `None`, not
    /// a capped grant, not a panic.
    #[test]
    fn oldgen_alloc_none_when_full() {
        let (mut old, mut offsets) = old_gen_and_offsets(64);
        let first = old.allocate(&mut offsets, 64, |_| {});
        assert_eq!(first, Some(0x30_0000));
        let second = old.allocate(&mut offsets, 8, |_| {});
        assert_eq!(second, None, "committed prefix is exactly full");
        // Retrying doesn't panic or silently succeed either.
        assert_eq!(old.allocate(&mut offsets, 1, |_| {}), None);
    }

    #[test]
    fn oldgen_allocate_calls_fill_and_records_offsets() {
        let (mut old, mut offsets) = old_gen_and_offsets(4096);
        let mut filled = None;
        let addr = old
            .allocate(&mut offsets, 40, |a| filled = Some(a))
            .unwrap();
        assert_eq!(filled, Some(addr));
        assert_eq!(offsets.resolve(offsets.card_index(addr)), addr);
    }

    /// Consecutive allocations (`tests_s07.md`'s `offset_updated_on_alloc`,
    /// exercised through the real `OldGen::allocate` path this time) leave
    /// every card below `top` resolvable.
    #[test]
    fn offset_updated_on_alloc_via_oldgen() {
        let (mut old, mut offsets) = old_gen_and_offsets(4 << 20);
        let mut headers = Vec::new();
        for size in [8usize, 3000, super::super::cards::CARD_SIZE, 40] {
            let addr = old.allocate(&mut offsets, size, |_| {}).unwrap();
            headers.push((addr, (size + 7) & !7));
        }
        let last_card = offsets.card_index(old.top - 8);
        for i in 0..=last_card {
            let resolved = offsets.resolve(i);
            let covers = headers
                .iter()
                .any(|&(h, s)| resolved == h && offsets.card_base(i) < h + s);
            assert!(covers, "card {i} resolved to {resolved:#x}");
        }
    }
}
