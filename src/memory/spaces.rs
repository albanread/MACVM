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
