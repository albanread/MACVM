//! `OopMap` builder + verifier (`sprint_s12_detail.md` D2). The format
//! itself (`OopMap`, `PcDesc`) lives in `codecache::nmethod` — this module
//! is purely the compiler-side logic that PRODUCES a map from regalloc's
//! own interval data, and the independent check that a published
//! `Nmethod`'s maps are internally consistent. `compiler` already depends
//! on `codecache::nmethod` directly (`driver.rs`'s own imports), so this
//! is not a new dependency direction, just a new file.

use crate::codecache::nmethod::{Nmethod, OopMap};
use crate::compiler::regalloc::{Assignment, LiveInterval};

/// D2: the map of which spill slots hold a LIVE oop at exactly `position`
/// (regalloc's own linear numbering, `RegallocResult::safepoint_positions`'
/// own scheme) — `slot_is_oop ∧ slot_live_at(position)`, computed directly
/// rather than via a separately-maintained `slot_is_oop` array intersected
/// after the fact: an interval only ever occupies ONE physical slot for
/// its ENTIRE lifetime (regalloc's monotonic spill-slot allocation, never
/// reused across intervals — D3.5 point 3), so "is slot i live-and-oop at
/// `position`" reduces to "does the interval CURRENTLY assigned to slot i
/// span `position`", checked directly against `intervals`.
///
/// Liveness matches `LiveInterval::crosses_safepoint`'s own comparison
/// exactly (`start <= position && end > position`, not `>=`): an interval
/// whose only reference IS this safepoint's own argument list ends AT the
/// call and does not need to survive it (that value is already consumed
/// by the time the callee could observe the stack).
pub fn build_for_position(intervals: &[LiveInterval], frame_slots: u16, position: u32) -> OopMap {
    let mut map = OopMap::empty();
    for iv in intervals {
        if !iv.is_oop {
            continue;
        }
        let Some(Assignment::Spill(slot)) = iv.assignment else {
            continue;
        };
        if iv.start <= position && iv.end > position {
            debug_assert!(
                slot.0 < frame_slots,
                "build_for_position: slot {} out of range (frame_slots={frame_slots})",
                slot.0
            );
            map.set(slot.0);
        }
    }
    map
}

/// D2's dedup rule ("two safepoints with identical liveness share one
/// oopmaps index"): finds `map` in `oopmaps` by CONTENT (not identity —
/// `OopMap`'s own `PartialEq`) and returns its index, appending only if no
/// equal map already exists. `driver.rs`'s `compile_method` is the only
/// real caller (called once per real safepoint, `oopmaps` starting
/// pre-seeded with the reserved `OopMap::empty()` at index 0 — a safepoint
/// with no live oops interns right back onto that same reserved slot, not
/// a fresh duplicate).
pub fn intern(oopmaps: &mut Vec<OopMap>, map: OopMap) -> u16 {
    match oopmaps.iter().position(|m| *m == map) {
        Some(idx) => idx as u16,
        None => {
            oopmaps.push(map);
            (oopmaps.len() - 1) as u16
        }
    }
}

/// D1 enforcement point 2 ("`oopmap::verify(nm)` (debug + stress): decodes
/// every map and checks `bit ⇒ slot < frame_slots` and `bit ⇒
/// slot_is_oop[slot]`") — an INDEPENDENT cross-check against `Nmethod::
/// slot_is_oop` (regalloc's own per-slot ground truth), not a tautology:
/// `build_for_position` above already only ever sets a bit for an
/// `is_oop` interval, so a passing verify here doesn't just confirm that
/// function ran correctly once — it catches ANY later corruption of
/// `oopmaps`/`slot_is_oop` (a bad patch, a future refactor that
/// reconstructs a map differently) that would otherwise trace a raw,
/// non-oop bit pattern as a pointer. Panics on the first violation found
/// (a VM-internal-consistency bug, not a user-triggerable condition —
/// same posture as `regalloc::verify_spill_all`).
pub fn verify(nm: &Nmethod) {
    for (map_idx, map) in nm.oopmaps.iter().enumerate() {
        for slot in map.iter_slots() {
            assert!(
                slot < nm.frame_slots,
                "oopmap::verify: nmethod {:?} oopmaps[{map_idx}] sets slot {slot} >= \
                 frame_slots ({})",
                nm.id,
                nm.frame_slots
            );
            assert!(
                nm.slot_is_oop[slot as usize],
                "oopmap::verify: nmethod {:?} oopmaps[{map_idx}] marks slot {slot} live but \
                 regalloc never assigned it an oop-typed interval (slot_is_oop[{slot}] = false)",
                nm.id
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::ir::VReg;
    use crate::compiler::regalloc::SpillSlot;

    /// `tests_s12.md`'s `oopmap_dedup`: two safepoints with IDENTICAL
    /// liveness intern to the SAME index; a genuinely different one gets
    /// its own; the pre-seeded reserved empty map (index 0) is reused for
    /// an empty safepoint rather than duplicated.
    #[test]
    fn oopmap_dedup() {
        let mut oopmaps = vec![OopMap::empty()];

        let mut a = OopMap::empty();
        a.set(1);
        let idx_a = intern(&mut oopmaps, a.clone());
        assert_eq!(oopmaps.len(), 2, "a genuinely new map appends");

        let idx_a_again = intern(&mut oopmaps, a);
        assert_eq!(
            idx_a_again, idx_a,
            "identical content reuses the same index"
        );
        assert_eq!(oopmaps.len(), 2, "must NOT append a duplicate");

        let mut b = OopMap::empty();
        b.set(1);
        b.set(3);
        let idx_b = intern(&mut oopmaps, b);
        assert_ne!(idx_b, idx_a, "a different live set gets its own index");
        assert_eq!(oopmaps.len(), 3);

        let idx_empty = intern(&mut oopmaps, OopMap::empty());
        assert_eq!(
            idx_empty, 0,
            "an empty safepoint reuses the reserved slot 0"
        );
        assert_eq!(
            oopmaps.len(),
            3,
            "must not append for the empty case either"
        );
    }

    fn spilled(vreg: u32, slot: u16, is_oop: bool, start: u32, end: u32) -> LiveInterval {
        LiveInterval {
            vreg: VReg(vreg),
            start,
            end,
            is_oop,
            crosses_safepoint: true,
            assignment: Some(Assignment::Spill(SpillSlot(slot))),
        }
    }

    /// D2/P2 (`tests_s12.md`'s `oopmap_liveness_intersection`): a slot
    /// whose oop interval ENDS strictly before the safepoint position must
    /// be excluded, even though `is_oop` is true and the slot itself would
    /// otherwise qualify — liveness is per-POSITION, not per-slot.
    #[test]
    fn oopmap_liveness_intersection() {
        let intervals = vec![
            spilled(0, 0, true, 0, 5),  // ends at 5, safepoint is at 10: dead
            spilled(1, 1, true, 0, 20), // spans 10: live
        ];
        let map = build_for_position(&intervals, 2, 10);
        assert!(
            !map.is_oop(0),
            "interval ending before the safepoint must be excluded"
        );
        assert!(
            map.is_oop(1),
            "interval spanning the safepoint must be included"
        );
    }

    /// An interval ending EXACTLY at the safepoint position is consumed BY
    /// it (the safepoint's own argument), not live ACROSS it — matches
    /// `crosses_safepoint`'s own `end > sp` (not `>=`) convention exactly.
    #[test]
    fn oopmap_excludes_interval_ending_at_position() {
        let intervals = vec![spilled(0, 0, true, 0, 10)];
        let map = build_for_position(&intervals, 1, 10);
        assert!(!map.is_oop(0));
    }

    /// A non-oop interval (e.g. a raw untagged scratch value) sharing the
    /// safepoint's own live range must never set its bit, regardless of
    /// timing.
    #[test]
    fn oopmap_excludes_non_oop_interval() {
        let intervals = vec![spilled(0, 0, false, 0, 20)];
        let map = build_for_position(&intervals, 1, 10);
        assert!(!map.is_oop(0));
    }

    /// `tests_s12.md`'s `oopmap_verifier_catches_nonoop_bit`: hand-corrupt
    /// a map to set a bit `slot_is_oop` disagrees with — `verify` must
    /// panic, not silently accept it.
    #[test]
    #[should_panic(expected = "regalloc never assigned it an oop-typed interval")]
    fn oopmap_verifier_catches_nonoop_bit() {
        let mut bad_map = OopMap::empty();
        bad_map.set(0); // slot 0 is NOT oop per slot_is_oop below
        let nm = Nmethod::test_fake(2, vec![false, true], vec![bad_map]);
        verify(&nm);
    }

    /// The verifier's own happy path: a correctly-built map over a
    /// genuinely oop-typed slot passes cleanly.
    #[test]
    fn oopmap_verifier_accepts_consistent_map() {
        let mut good_map = OopMap::empty();
        good_map.set(1);
        let nm = Nmethod::test_fake(2, vec![false, true], vec![good_map]);
        verify(&nm); // must not panic
    }

    /// The verifier's OTHER check (D1 point 2's two halves are independent):
    /// a bit set past `frame_slots` must panic too — P3's own reasoning
    /// (an off-by-one here would read the saved-fp/lr pair as an "oop").
    /// Constructed by hand (not via `build_for_position`, which has its own
    /// separate debug_assert against this) so `verify`'s own check is what
    /// actually fires.
    #[test]
    #[should_panic(expected = "slot 5 >= frame_slots")]
    fn oopmap_verifier_catches_out_of_range_slot() {
        let mut bad_map = OopMap::empty();
        bad_map.set(5); // frame_slots is only 2 below
        let nm = Nmethod::test_fake(2, vec![false, true], vec![bad_map]);
        verify(&nm);
    }
}
