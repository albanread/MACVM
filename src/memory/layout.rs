//! Heap geometry (SPEC §7.1, `sprint_s07_detail.md` §Design): fixed,
//! immutable space BOUNDS carved out of one [`Reservation`](super::reservation::Reservation),
//! low→high `[eden][from][to][old…]`. Mutable per-space allocation state
//! (bump `top` pointers) lives in [`super::spaces`], not here — this module
//! owns only the address-range geometry and the old/new generation test.
//! (`oops::layout` owns bit constants; this module owns geometry — the
//! sprint's own layer-boundary rule.)

/// Default eden size *(tunable via `VmOptions`/`MACVM_EDEN`)* — SPEC §7.1.
pub const DEFAULT_EDEN_SIZE: usize = 4 << 20;
/// Each survivor space's fixed size *(tunable)* — SPEC §7.1.
pub const SURVIVOR_SIZE: usize = 512 << 10;
/// Old gen's first committed segment at boot *(tunable, S8 grows further)*;
/// capped to whatever's actually left in the reservation for tiny test
/// heaps (`old.len()` can be smaller than this for a `heap_mib: 16` test
/// heap once eden + 2 survivors are carved out).
pub const OLD_INITIAL_SEGMENT: usize = 16 << 20;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SpaceBounds {
    pub start: usize,
    pub end: usize,
}

impl SpaceBounds {
    pub fn contains(self, addr: usize) -> bool {
        addr >= self.start && addr < self.end
    }
    pub fn len(self) -> usize {
        self.end - self.start
    }
    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// The heap's fixed address-range geometry within one reservation.
/// `old` is the RESERVED maximum range — only a prefix of it is committed
/// at any time (tracked by `spaces::OldGen::committed_end`).
pub struct HeapLayout {
    pub eden: SpaceBounds,
    pub from: SpaceBounds,
    pub to: SpaceBounds,
    pub old: SpaceBounds,
    /// `== old.start` — THE generation boundary.
    pub old_start: usize,
}

impl HeapLayout {
    /// Lays out `[eden][from][to][old…]` starting at `base`, given a total
    /// reservation of `total_len` bytes and an eden size (the caller
    /// resolves the `MACVM_EDEN`-vs-default choice before calling this).
    /// `total_len` must be large enough to fit eden + both survivors — a
    /// caller violating that has already misconfigured a heap too small to
    /// boot genesis into eden alone.
    pub fn new(base: usize, total_len: usize, eden_size: usize) -> HeapLayout {
        let eden = SpaceBounds {
            start: base,
            end: base + eden_size,
        };
        let from = SpaceBounds {
            start: eden.end,
            end: eden.end + SURVIVOR_SIZE,
        };
        let to = SpaceBounds {
            start: from.end,
            end: from.end + SURVIVOR_SIZE,
        };
        let old_start = to.end;
        let reservation_end = base + total_len;
        debug_assert!(
            old_start <= reservation_end,
            "reservation of {total_len} bytes too small for eden ({eden_size}) + 2 survivors ({SURVIVOR_SIZE} each)"
        );
        let old = SpaceBounds {
            start: old_start,
            end: reservation_end,
        };
        HeapLayout {
            eden,
            from,
            to,
            old,
            old_start,
        }
    }

    /// Single-compare generation test. Works on TAGGED oops unchanged:
    /// `Mem_Tag = 1` biases an 8-byte-aligned address by +1, which can
    /// never cross the page-aligned `old_start` boundary — callers pass
    /// either a raw address or a tagged oop's `.raw()`/`.mem_addr()`
    /// indiscriminately.
    #[inline]
    pub fn is_old(&self, oop_or_addr: usize) -> bool {
        oop_or_addr >= self.old_start
    }
    #[inline]
    pub fn is_new(&self, oop_or_addr: usize) -> bool {
        oop_or_addr < self.old_start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_ordering_and_sizes() {
        let l = HeapLayout::new(0x1000, 64 << 20, DEFAULT_EDEN_SIZE);
        assert_eq!(l.eden.start, 0x1000);
        assert_eq!(l.eden.len(), DEFAULT_EDEN_SIZE);
        assert_eq!(l.from.start, l.eden.end);
        assert_eq!(l.from.len(), SURVIVOR_SIZE);
        assert_eq!(l.to.start, l.from.end);
        assert_eq!(l.to.len(), SURVIVOR_SIZE);
        assert_eq!(l.old.start, l.to.end);
        assert_eq!(l.old_start, l.old.start);
        assert_eq!(l.old.end, 0x1000 + (64 << 20));
    }

    /// `is_old(addr)` and `is_old(addr+1)` (the Mem_Tag bias) must agree on
    /// both sides of `old_start` — the one boundary check compiled code
    /// runs on tagged oops.
    #[test]
    fn is_old_boundary_tagged() {
        let l = HeapLayout::new(0, 64 << 20, DEFAULT_EDEN_SIZE);
        let b = l.old_start;
        assert!(!l.is_old(b - 8));
        assert!(!l.is_old(b - 8 + 1)); // tagged form of the last new-gen word
        assert!(l.is_old(b));
        assert!(l.is_old(b + 1)); // tagged form of the first old-gen word
        assert!(l.is_new(b - 8));
        assert!(!l.is_new(b));
    }

    #[test]
    fn small_reservation_still_fits_two_survivors() {
        // The smallest heap_mib used anywhere in the test suite (16 MiB):
        // eden(4) + from(0.5) + to(0.5) = 5 MiB, leaving 11 MiB for old.
        let l = HeapLayout::new(0, 16 << 20, DEFAULT_EDEN_SIZE);
        assert!(!l.old.is_empty());
        assert_eq!(
            l.old.len(),
            (16 << 20) - DEFAULT_EDEN_SIZE - 2 * SURVIVOR_SIZE
        );
    }
}
