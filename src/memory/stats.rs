//! GC statistics (SPEC §12.4-ish observability): counters read by
//! `Smalltalk`-level introspection later, by `GcStallError`'s progress
//! numbers, and by the S6 PERF procedure's bytecode-count sibling for GC
//! pauses.

use std::time::Duration;

#[derive(Debug, Default)]
pub struct GcStats {
    pub scavenge_count: u64,
    pub full_gc_count: u64,
    pub total_scavenge_pause: Duration,
    pub bytes_allocated: u64,
    pub bytes_copied: u64,
    pub bytes_promoted: u64,
    pub tenuring_threshold: u8,
    /// S14 asserts context-allocation elision against this — every
    /// `Context` the interpreter allocates (not merely activates) bumps
    /// it, regardless of generation.
    pub context_allocs: u64,
    /// Reclaimed bytes from the most recently completed scavenge — one of
    /// `GcStallError`'s "is GC actually helping" progress numbers.
    pub last_reclaimed_bytes: u64,
    /// S8: bytes phase A found reachable in the most recent full GC —
    /// `gcStats`'s `markedBytesLast` and lesson 15's "measure marked bytes
    /// vs expected working set before touching policy" diagnostic.
    pub marked_bytes_last: u64,
    /// S8: cumulative full-GC pause time, alongside `total_scavenge_pause`.
    pub full_pause_total: Duration,
    /// S8: `OldGen::grow` calls that actually committed a segment (0 return
    /// excluded — that's a no-op at the reservation ceiling, not growth).
    pub old_grow_count: u64,
    /// S11 D8 (pre-S12 bridge): allocations diverted straight to old gen
    /// because a compiled frame was live (`compiled_depth > 0`) and moving
    /// GC is forbidden until S12 can find compiled-frame oops. The cost S12
    /// removes — `tests_s11.md`'s bridge-accounting gate reads this, and
    /// `allocation_fast_and_slow` asserts it is `> 0` on the forced-slow
    /// path. See `alloc::alloc_words`' own bridge arm.
    pub bridge_old_allocs: u64,
    /// S11 D8 step 10: counts every `scavenge`/`full_gc` entry that
    /// observed `compiled_depth > 0` — i.e. every time the bridge's
    /// invariant was actually VIOLATED. Both collectors also
    /// `debug_assert_eq!(compiled_depth, 0)` at the same point, but that
    /// compiles out in `--release`; this counter is the independent,
    /// always-live proof `tests_s11.md`'s "Bridge accounting" gate reads —
    /// it must be `0` at the end of any run, combined GC-stress +
    /// `threshold=1` included. Bumped BEFORE the debug_assert so a debug
    /// build's panic still leaves the violation visible to a
    /// `catch_unwind`-wrapped test.
    pub gc_under_compiled: u64,
}

impl GcStats {
    pub fn new() -> GcStats {
        GcStats::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_stats_are_zero() {
        let s = GcStats::new();
        assert_eq!(s.scavenge_count, 0);
        assert_eq!(s.full_gc_count, 0);
        assert_eq!(s.total_scavenge_pause, Duration::ZERO);
        assert_eq!(s.bytes_allocated, 0);
        assert_eq!(s.tenuring_threshold, 0);
    }
}
