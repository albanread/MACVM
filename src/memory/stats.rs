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
    /// S15 A7: the single longest scavenge pause so far (PERF.md's pause
    /// rows; percentiles come from `MACVM_TRACE=gc` parsing, but max is
    /// cheap enough to keep always-on).
    pub scavenge_pause_max: Duration,
    /// S15 A7: the single longest full-GC pause so far.
    pub full_pause_max: Duration,
    /// S8: cumulative full-GC pause time, alongside `total_scavenge_pause`.
    pub full_pause_total: Duration,
    /// S8: `OldGen::grow` calls that actually committed a segment (0 return
    /// excluded — that's a no-op at the reservation ceiling, not growth).
    pub old_grow_count: u64,
    /// Counts every `scavenge`/`full_gc` entry that ran with a live
    /// compiled frame on the native stack (`compiled_depth > 0`) — the
    /// hard case S12 exists for. Under S11's D8 bridge this was the
    /// always-live proof the bridge HELD (asserted `== 0` at every gate);
    /// with the bridge deleted (S12 step 7) its meaning INVERTS, per S12
    /// P10: the flagship gate now asserts it lands `> 0`, proving the
    /// hard case genuinely executed rather than being silently routed
    /// around. (`bridge_old_allocs`, this field's S11-era sibling counting
    /// the bridge's old-direct diversions, died with the bridge itself —
    /// deliberately removed rather than left at a constant 0, so anything
    /// still referencing it fails to compile instead of reading a lie.)
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
