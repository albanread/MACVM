//! GC statistics (SPEC §12.4-ish observability): counters read by
//! `Smalltalk`-level introspection later, by `GcStallError`'s progress
//! numbers, and by the S6 PERF procedure's bytecode-count sibling for GC
//! pauses.

use std::time::Duration;

#[derive(Debug, Default)]
pub struct GcStats {
    pub scavenge_count: u64,
    /// Always 0 until S8 — a full (old-gen) collector doesn't exist yet.
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
