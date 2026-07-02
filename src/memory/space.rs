//! `Eden` — the bump-allocated young space (SPEC §7.1–§7.2, S1 subset: no
//! survivors, no old generation yet — S7 adds those inside the same
//! `Reservation`).

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
