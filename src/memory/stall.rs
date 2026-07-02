//! `GcStallError` (SPEC §7.2 step 6; MacNCL lessons 6 and 7): allocation
//! failure is always a structured, readable report — per-space
//! occupancy/capacity and GC progress numbers — never a bare panic or an
//! "out of memory" with no context. Every allocation-failure path in the
//! designed cascade (A2) constructs one; `main` prints it and exits
//! non-zero.

use std::fmt;

use super::universe::Universe;

/// Which phase of the allocator/collector was asking for memory when it
/// ran out (SPEC §7.2 step 6's three phases; more join in S8 alongside
/// full-GC).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GcPhase {
    Mutator,
    ScavengeCopy,
    ScavengePromote,
}

impl fmt::Display for GcPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            GcPhase::Mutator => "mutator allocation",
            GcPhase::ScavengeCopy => "scavenge copy (survivor space)",
            GcPhase::ScavengePromote => "scavenge promotion (old gen)",
        };
        write!(f, "{s}")
    }
}

/// A structured, non-fatal-to-construct report of an allocation failure —
/// every field a human (or a later retry policy) needs to tell "genuinely
/// out of memory" from "a bug capped a grant early" apart. Lesson 4: this
/// is what replaces a silently-capped allocation; lesson 7: this replaces
/// a bare panic/OOM message.
#[derive(Debug)]
pub struct GcStallError {
    pub requested_bytes: usize,
    pub phase: GcPhase,
    pub eden_used: usize,
    pub eden_capacity: usize,
    pub from_used: usize,
    pub to_used: usize,
    pub survivor_capacity: usize,
    pub old_used: usize,
    pub old_committed: usize,
    pub old_reserved: usize,
    pub scavenge_count: u64,
    pub full_gc_count: u64,
    pub last_survivor_bytes: u64,
    pub last_promoted_bytes: u64,
    pub last_reclaimed_bytes: u64,
}

impl GcStallError {
    /// Snapshots every space's occupancy/capacity and the running GC
    /// counters at the moment `requested_bytes` couldn't be granted while
    /// in `phase`.
    pub fn snapshot(universe: &Universe, requested_bytes: usize, phase: GcPhase) -> GcStallError {
        GcStallError {
            requested_bytes,
            phase,
            eden_used: universe.eden.top - universe.eden.start,
            eden_capacity: universe.eden.end - universe.eden.start,
            from_used: universe.from.top - universe.from.start,
            to_used: universe.to.top - universe.to.start,
            survivor_capacity: universe.from.end - universe.from.start,
            old_used: universe.old.top - universe.old.bounds.start,
            old_committed: universe.old.committed_end - universe.old.bounds.start,
            old_reserved: universe.old.bounds.len(),
            scavenge_count: universe.gc_stats.scavenge_count,
            full_gc_count: universe.gc_stats.full_gc_count,
            last_survivor_bytes: 0, // filled in by scavenge.rs once it tracks per-cycle bytes (S7-7)
            last_promoted_bytes: 0,
            last_reclaimed_bytes: universe.gc_stats.last_reclaimed_bytes,
        }
    }
}

impl fmt::Display for GcStallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "macvm: allocation of {} bytes failed during {} — heap exhausted",
            self.requested_bytes, self.phase
        )?;
        writeln!(
            f,
            "  eden:      {}/{} bytes used",
            self.eden_used, self.eden_capacity
        )?;
        writeln!(
            f,
            "  survivors: from={} to={} (capacity {} each)",
            self.from_used, self.to_used, self.survivor_capacity
        )?;
        writeln!(
            f,
            "  old gen:   {}/{} committed, {} reserved",
            self.old_used, self.old_committed, self.old_reserved
        )?;
        writeln!(
            f,
            "  gc:        {} scavenges, {} full GCs",
            self.scavenge_count, self.full_gc_count
        )?;
        write!(
            f,
            "  progress:  last cycle survived {} / promoted {} / reclaimed {} bytes",
            self.last_survivor_bytes, self.last_promoted_bytes, self.last_reclaimed_bytes
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::vm_state::{VmOptions, VmState};

    /// `tests_s07.md`'s `stall_error_fields`: a provoked stall carries
    /// `requested_bytes`, per-space counts, `scavenge_count`, and progress
    /// numbers. "Provoked" here just means constructed directly against a
    /// fresh `Universe` — the allocator's own cascade (A2) doesn't call
    /// this yet (S7-7 wires that in), so this exercises the snapshot
    /// contract in isolation.
    #[test]
    fn stall_error_fields() {
        let vm = VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            eden_kb: None,
        });
        let err = GcStallError::snapshot(&vm.universe, 4096, GcPhase::Mutator);
        assert_eq!(err.requested_bytes, 4096);
        assert_eq!(err.phase, GcPhase::Mutator);
        assert!(err.eden_capacity > 0);
        assert!(err.eden_used <= err.eden_capacity);
        assert!(err.old_reserved > 0);
        assert_eq!(err.scavenge_count, 0);
        assert_eq!(err.full_gc_count, 0);

        let text = err.to_string();
        assert!(text.contains("4096"));
        assert!(text.contains("mutator allocation"));
    }
}
