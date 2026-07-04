//! The full mark-slide-compact GC (SPEC §7.5, §2.2's displaced marks;
//! `sprint_s08_detail.md`). Runs the whole heap in one pause: worklist mark
//! (phase A), per-space forwarding computation with a displaced-mark side
//! map (phase B), reference rewrite over every root and heap slot while
//! nothing has moved yet (phase C), the slide itself plus old-gen offset-
//! table rebuild (phase D), mark restoration (phase E), and post-GC
//! bookkeeping — card reset, lookup-cache flush, age-table reset (phase F).
//!
//! Unlike the scavenger (`memory::scavenge`, young-gen only, copies eden+
//! from into to), this traces and compacts the WHOLE heap — eden, from, AND
//! old — each space in place toward its own bottom. `to` stays untouched
//! and must be empty (asserted): full GC only ever runs between scavenge
//! cycles, never nested inside one (design rule: collections are
//! sequential — `sprint_s08_detail.md`'s cascade section, MacNCL lesson 9).
//!
//! Deliberately deferred to a later pass, not a correctness gap: phase A's
//! `tagged_contents` fast path (SPEC's "every body word is an oop, skip
//! format dispatch" optimization) — `MemOop::for_each_oop_field` always
//! format-dispatches, which is behaviorally identical and already proven
//! correct via the scavenger's test suite; perf work is out of scope before
//! S15 (SPRINTS.md's own stated rule).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::oops::layout::{MEM_TAG, WORD_SIZE};
use crate::oops::mark::Mark;
use crate::oops::wrappers::{KlassOop, MemOop};
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;

use super::stall::GcStallError;
use super::verify::VerifyPoint;

/// One live object's relocation (SPEC §7.5 phase B/C/D). Built once, while
/// every klass body is still readable at its OLD address (nothing has moved
/// yet) — this is the answer to "how do later phases know an object's size
/// without re-deriving it from a klass field that phase C has since
/// rewritten to a not-yet-populated new address" (invariant F3: no phase
/// after C computes a size from a klass field; sizes come from here only).
#[derive(Copy, Clone)]
pub struct CompactEntry {
    pub old: usize,
    pub new: usize,
    pub size_words: u32,
}

/// Every live object of one space, in ascending `old`-address order (so
/// phase D's `ptr::copy` slide, which can overlap a live object with its
/// own future location, never clobbers a not-yet-copied source).
pub type CompactPlan = Vec<CompactEntry>;

/// Displaced-mark side map (SPEC §2.2 Δ): original marks that carry STATE
/// — hash, age, or near_death; `tagged_contents` is never state, it is
/// always recomputed from the (already-rewritten, already-slid) klass's
/// format on restore (phase E) — parked here during phases B–E, keyed by
/// the object's NEW address. Lifetime is exactly one full GC's phases B–E
/// (MacNCL lesson 9: never let per-cycle state leak across cycles); dropped
/// automatically with `full_gc`'s local at phase E's end.
type SideMarks = HashMap<usize, Mark>;

#[derive(Debug, Default)]
pub struct FullGcReport {
    /// Bytes phase A found reachable (SPEC lesson 15's "measure marked
    /// bytes vs the expected working set before touching policy").
    pub marked_bytes: usize,
    /// Bytes actually occupied post-compaction, summed independently from
    /// phase D's per-space placement — mathematically must equal
    /// `marked_bytes` (compaction repositions live data, never creates or
    /// loses any); debug-asserted equal as a lesson-8 cross-check ("bugs
    /// live in disagreements between separately-correct structures").
    pub live_after: usize,
    pub reclaimed_bytes: usize,
    pub pause: Duration,
}

/// A — Mark (SPEC §7.5 step 1, explicit worklist — SPEC's own Δ from
/// pointer-reversal, heap-bounded, plain `Vec` growth is fine). Sets
/// `MARK_GC_BIT` on every object reachable from a root, set-on-push (not
/// on-pop) so a diamond's shared node or a cycle enters the stack exactly
/// once. Nothing is rewritten here (`for_each_root`'s transform returns its
/// input unchanged) — that is phase C's job, once every live object's
/// final address is known.
fn mark(vm: &mut VmState) -> usize {
    let mut marked_bytes = 0usize;
    let mut stack: Vec<MemOop> = Vec::new();

    super::roots::for_each_root(vm, |_vm, oop| {
        mark_push(oop, &mut stack, &mut marked_bytes);
        oop
    });

    // S10/S11/S12 D4.1/D5: nmethod/adapter/PIC/mega pool oops, and (S12)
    // any live compiled/adapter frame's own spill/RootSpill slots, are
    // roots too — `each_code_root` is the single visitor for all of this
    // (`memory::roots`'s own doc, `memory::scavenge`'s identical call
    // site). `false`: this is full GC's own MARK pass, so the
    // customization key klass is deliberately left UNMARKED (D5's weak
    // rule — `weak_sweep`, called right after this function returns, reads
    // exactly the mark bits this pass leaves behind to find nmethods whose
    // key klass turned out to be genuinely dead). `mark_push` needs no
    // `vm` (unlike `scavenge_oop`), so this needs no `std::mem::take`
    // dance of its own — `each_code_root` already does that internally,
    // for the ONE table (code_table) that actually needs it.
    super::roots::each_code_root(vm, false, |_vm, oop| {
        mark_push(oop, &mut stack, &mut marked_bytes);
        oop
    });

    while let Some(obj) = stack.pop() {
        let klass_field = obj.klass_oop();
        mark_push(klass_field, &mut stack, &mut marked_bytes);
        // Reading the klass's body (format/non_indexable_size, inside
        // for_each_oop_field) is safe regardless of whether the klass
        // ITSELF has already been mark-pushed this pass: marking never
        // moves anything or rewrites a klass field, so every body is
        // exactly where it started until phase D.
        let klass = KlassOop::try_from(klass_field).expect("mark: klass field not klass-shaped");
        obj.for_each_oop_field(klass, |v| {
            mark_push(v, &mut stack, &mut marked_bytes);
            v
        });
    }

    marked_bytes
}

/// Marks `oop` if it's an unmarked mem object — pushing it and counting its
/// bytes toward the running total — else a no-op. Unlike the scavenger,
/// full GC traces BOTH generations (no `is_old` early-out): everything
/// reachable gets marked, young or old.
fn mark_push(oop: Oop, stack: &mut Vec<MemOop>, marked_bytes: &mut usize) {
    if !oop.is_mem() {
        return;
    }
    let obj = MemOop::try_from(oop).expect("mark_push: not mem-tagged");
    let m = obj.mark();
    if !m.gc_mark() {
        obj.set_mark(m.with_gc_mark(true));
        *marked_bytes += obj.instance_size_words() * WORD_SIZE;
        stack.push(obj);
    }
}

/// B — Forwarding compute for ONE space (SPEC §7.5 step 2): a linear walk
/// of `[start, top)` object by object — still nothing has moved, so sizing
/// via `instance_size_words` (klass bodies read in place) is always safe,
/// live or dead. A marked object gets the next `compaction_pointer` slot,
/// a `CompactEntry`, its state-carrying mark fields parked in `side_marks`
/// (gc_mark cleared — that bit is full-GC-only, invariant-0 the instant
/// this cycle ends), and its header overwritten with a forwarding oop to
/// its new address — even when `new == old` (self-forwarding), so phase C
/// never needs an "did this one actually move" branch. An unmarked
/// (unreachable) object is skipped: its bytes are silently reclaimed by
/// the slide (or POISONed after, in debug builds).
///
/// Returns the space's new top (`start` if nothing survived).
fn forwarding_compute(
    start: usize,
    top: usize,
    side_marks: &mut SideMarks,
) -> (CompactPlan, usize) {
    let mut plan = Vec::new();
    let mut addr = start;
    let mut compaction_pointer = start;
    while addr < top {
        let oop = Oop::from_raw(addr as u64 + MEM_TAG);
        let obj = MemOop::try_from(oop).expect("forwarding_compute: not mem-tagged");
        let size_words = obj.instance_size_words();
        let size_bytes = size_words * WORD_SIZE;
        let m = obj.mark();
        if m.gc_mark() {
            let new = compaction_pointer;
            compaction_pointer += size_bytes;
            debug_assert!(
                size_words <= u32::MAX as usize,
                "object too large for CompactEntry"
            );
            plan.push(CompactEntry {
                old: addr,
                new,
                size_words: size_words as u32,
            });
            if m.hash() != 0 || m.age() != 0 || m.near_death() {
                side_marks.insert(new, m.with_gc_mark(false));
            }
            obj.install_forwarding(Oop::from_raw(new as u64 + MEM_TAG));
        }
        addr += size_bytes;
    }
    (plan, compaction_pointer)
}

/// C: resolve `oop` to its post-slide address. Every live-graph reference
/// is, by construction, something phase A already marked and phase B
/// already forwarded — a non-mem oop (smi) passes through unchanged; an
/// unforwarded mem-oop target would mean phase A missed a reachable
/// object, so debug builds assert it never happens rather than silently
/// rewrite to a wrong/stale address.
fn forward_chase(oop: Oop) -> Oop {
    if !oop.is_mem() {
        return oop;
    }
    let obj = MemOop::try_from(oop).expect("forward_chase: not mem-tagged");
    debug_assert!(
        obj.is_forwarded(),
        "phase C: object at {:#x} is reachable but was not forwarded by phase B \
         (a live object phase A didn't mark?)",
        oop.mem_addr()
    );
    if obj.is_forwarded() {
        obj.forwardee()
    } else {
        oop
    }
}

/// C: rewrite one heap object's klass field and body fields to their
/// forward-chased (post-slide) values, entirely at the object's OLD
/// address — nothing has been copied yet (that's phase D). Body first,
/// klass field last: `for_each_oop_field`'s format dispatch needs a klass
/// to read the shape from, and this object's own klass field is only safe
/// to use for that BEFORE it's rewritten (after, it names a new address
/// phase D hasn't populated — invariant F3).
///
/// The klass used for dispatch is constructed WITHOUT the validating
/// `KlassOop::try_from` (found via a genuine full-GC bug, not a defensive
/// guess): that constructor checks the CANDIDATE'S OWN klass field's format
/// — a second hop — and by the time an arbitrary entry is rewritten, some
/// EARLIER-processed entry (address-order, not necessarily related to this
/// object) may already have had ITS klass field rewritten to a new address
/// phase D hasn't populated. `old_klass_field` was already established as
/// legitimately klass-shaped by phases A and B (both read its body
/// successfully to compute sizes, before anything was rewritten), so
/// re-validating it here is not just redundant — during phase C, it is
/// actively unsafe.
fn rewrite_entry(entry: &CompactEntry) {
    let oop = Oop::from_raw(entry.old as u64 + MEM_TAG);
    let obj = MemOop::try_from(oop).expect("rewrite_entry: not mem-tagged");
    let old_klass_field = obj.klass_oop();
    // SAFETY: see the doc comment above — phases A and B already
    // established `old_klass_field` names a klass-shaped object; only the
    // validating constructor's extra hop is unsafe here, not the address.
    let klass = unsafe { KlassOop::from_oop_unchecked(old_klass_field) };
    obj.for_each_oop_field(klass, forward_chase);
    obj.set_klass_raw(forward_chase(old_klass_field));
}

/// D: slide one space's live objects into place (SPEC §7.5 step 2's move).
/// `ptr::copy` — NOT `copy_nonoverlapping` — because a surviving object's
/// new location can overlap its old one; ascending `old` order (guaranteed
/// by `forwarding_compute`'s linear walk) means `new <= old` for every
/// entry, so no later source is ever clobbered before it's copied.
fn slide_young(plan: &CompactPlan) {
    for e in plan {
        let words = e.size_words as usize;
        // SAFETY: `e.old` names `words*8` bytes of a live object this same
        // full GC already validated (phase A marked it, phase B sized it);
        // `e.new` is that same space's own bottom-up compaction range,
        // which phase B computed to end at or below `e.old`'s original
        // extent — never outside the space's committed bounds.
        unsafe {
            std::ptr::copy(e.old as *const u8, e.new as *mut u8, words * WORD_SIZE);
        }
    }
}

/// D, old-gen variant: same slide, plus the offset-table rebuild SPEC §7.5
/// requires (`OldGen::allocate`'s own maintenance rule, replayed here
/// because compaction is the one other place objects get placed in old
/// gen). The pre-compaction table is unconditionally stale after this
/// (every surviving object's address changed, and `old.top` can shrink),
/// so it is cleared first rather than patched.
fn slide_old(vm: &mut VmState, plan: &CompactPlan) {
    vm.universe.offsets.clear_through(vm.universe.old.top);
    for e in plan {
        let words = e.size_words as usize;
        // SAFETY: as `slide_young`.
        unsafe {
            std::ptr::copy(e.old as *const u8, e.new as *mut u8, words * WORD_SIZE);
        }
        vm.universe.offsets.record_object(e.new, words * WORD_SIZE);
    }
}

/// E: restore a real mark word at every surviving object's NEW address
/// (SPEC §7.5 step 4). The bytes just copied there by the slide still
/// carry a stale forwarding tag (this entry's OWN, self-referential after
/// the copy) — overwritten unconditionally, never read. `tagged_contents`
/// is always recomputed from the klass's format (its NEW, already-rewritten
/// field — safe: the klass's body was already slid into place by the time
/// ANY entry's restore runs against it, since phase D fully completes
/// before phase E starts) — it is never state, per `SideMarks`' own doc.
fn restore_marks(plan: &CompactPlan, side_marks: &SideMarks) {
    for e in plan {
        let oop = Oop::from_raw(e.new as u64 + MEM_TAG);
        let obj = MemOop::try_from(oop).expect("restore_marks: not mem-tagged");
        let klass = obj.klass();
        let tagged = !klass.has_untagged_contents();
        let base = side_marks
            .get(&e.new)
            .copied()
            .unwrap_or_else(Mark::pristine);
        obj.set_mark(base.with_tagged_contents(tagged).with_gc_mark(false));
    }
}

/// F1 (SPEC §7.5 append): outside a full GC, no mark word anywhere has
/// `MARK_GC_BIT` set and no mark is a forwarding word. Checked before phase
/// A (nothing should have leaked from a prior cycle — full GCs never nest)
/// and after phase E (every mark this cycle touched must have been
/// restored to a real mark, not left forwarding or GC-marked). Debug-only:
/// an O(live) heap walk, not a release-mode invariant.
#[cfg(debug_assertions)]
fn debug_assert_clean(vm: &VmState) {
    let u = &vm.universe;
    for &(start, top) in &[
        (u.eden.start, u.eden.top),
        (u.from.start, u.from.top),
        (u.old.bounds.start, u.old.top),
    ] {
        let mut addr = start;
        while addr < top {
            let oop = Oop::from_raw(addr as u64 + MEM_TAG);
            let obj = MemOop::try_from(oop).expect("debug_assert_clean: not mem-tagged");
            let raw = obj.mark_word_raw();
            debug_assert!(
                !crate::oops::mark::word_is_forwarded(raw),
                "F1: object at {addr:#x} still carries a forwarding mark outside a full GC"
            );
            let m = obj.mark();
            debug_assert!(
                !m.gc_mark(),
                "F1: object at {addr:#x} still has MARK_GC_BIT set outside a full GC"
            );
            addr += obj.instance_size_words() * WORD_SIZE;
        }
    }
}

/// S12 D5 point 2's own invariant: a dead key klass implies no live
/// activation of that nmethod exists (any live receiver would have marked
/// the klass via its own header during phase A) — walks every currently
/// live frame (`runtime::frames::walk_frames`) and asserts none of them is
/// a `Compiled` activation of an nmethod in `flush_set`. Debug-only, like
/// `debug_assert_clean` just above (an O(frames) walk, not a release-mode
/// invariant — the plain `assert!` right at this function's own call site
/// already gives release builds a real safety net for `flush_set` itself
/// being non-empty; this is the deeper, diagnostic-only cross-check).
/// Moot today (the S11 D8 bridge forbids a live compiled frame from ever
/// coexisting with a running full GC at all, so `walk_frames` can only
/// ever find interpreted frames here) — genuinely load-bearing the moment
/// step 7 removes that bridge.
#[cfg(debug_assertions)]
fn debug_assert_weak_sweep_invariant(
    vm: &VmState,
    flush_set: &[crate::codecache::nmethod::NmethodId],
) {
    if flush_set.is_empty() {
        return;
    }
    let mut frames = Vec::new();
    crate::runtime::frames::walk_frames(vm, |fv| frames.push(fv));
    for fv in frames {
        if let crate::runtime::frames::FrameView::Compiled { nm, .. } = fv {
            debug_assert!(
                !flush_set.contains(&nm),
                "weak_sweep: nmethod {nm:?} is in flush_set (its key_klass is unmarked) but a \
                 live compiled activation of it still exists on the native stack -- a dead key \
                 klass must imply no live activation (any live receiver would have marked the \
                 klass via its own header during phase A)"
            );
        }
    }
}

/// F: post-GC bookkeeping (SPEC §7.5 step 4, second half). Card-table reset
/// is deliberately conservative — compaction moved old objects that may
/// reference (also-moved) new-gen objects, and recomputing exact old→new
/// cards costs a heap walk for no benefit, since the next scavenge's
/// dirty-card scan self-corrects quiet cards anyway (S7 A6) — so every card
/// covering the live old range is simply marked dirty rather than derived
/// precisely. The lookup cache is flushed (klass addresses moved); the age
/// table is reset (ages travelled via the side map, but the per-scavenge
/// histogram itself is stale — S8 pitfalls: do not "helpfully" refill it
/// here). Old-gen growth policy is deliberately NOT checked here — that is
/// the cascade's job (S8 step 7), not a bare `full_gc` call's.
fn post(vm: &mut VmState) {
    // Bounded to the live range, not `cards.n_cards()` (every card in the
    // WHOLE reservation): a real VM's default heap reserves 8 GiB, ~16.7M
    // cards, and this ran on every single full GC — found via a genuine
    // multi-minute full_gc under MACVM_GC_STRESS=full:1 on the actual CLI
    // binary's default heap (a 64 MiB test heap's ~120K cards hid this
    // completely). Nothing beyond `old.top` is ever dirtied (nothing is
    // ever written past the allocation frontier) and `dirty_card_scan`
    // itself never looks past it either, so cleaning only up to there is
    // both correct and exactly what "reset" needs. Same inclusive-last-
    // card pattern as `record_multistores` right below (an exclusive
    // `card_index(top)` would floor-round away the card actually
    // containing `top` whenever `top` isn't itself card-aligned).
    let old = &vm.universe.old;
    if old.top > old.bounds.start {
        let last = vm.universe.cards.card_index(old.top - 1);
        for i in 0..=last {
            vm.universe.cards.set_clean(i);
        }
    }
    vm.universe
        .cards
        .record_multistores(vm.universe.old.bounds.start, vm.universe.old.top);
    crate::runtime::lookup::gc_epilogue(vm);
    vm.universe.age_table.clear();
}

/// `MACVM_DBG_OOP` (S8 phase-C table item, `sprint_s08_detail.md`'s "table
/// inventory sweep"): retarget the traced address if phase B just gave its
/// object a new home — mirrors `scavenge_oop`'s own inline retarget.
/// `dbg_oop_trace` is deliberately read-only and never chases forwarding
/// itself (see its own doc comment); the mover is responsible for keeping
/// the traced address current, and here that's one step removed from the
/// per-object copy (full GC computes the whole plan before moving anything,
/// unlike the scavenger's eager per-object copy), so it's a small pass over
/// the finished `CompactPlan` rather than an inline check during the walk.
fn retarget_dbg_oop(vm: &mut VmState, plan: &CompactPlan) {
    let Some(addr) = vm.dbg_oop else { return };
    if let Some(e) = plan.iter().find(|e| e.old == addr) {
        vm.dbg_oop = Some(e.new);
    }
}

/// The full mark-slide-compact collection (SPEC §7.5). Traces and compacts
/// eden, from, and old, each toward its own space's bottom; `to` is
/// untouched and must be empty (asserted — full GC never runs mid-scavenge,
/// SPEC §7.2's sequential-collections design rule). `Result` matches the
/// interface later sprints (S8 step 7's cascade) depend on, but pure
/// compaction within already-committed space can never need more memory
/// than it already has, so this never actually returns `Err` today —
/// kept for signature stability, not because failure is reachable yet.
pub fn full_gc(vm: &mut VmState) -> Result<FullGcReport, GcStallError> {
    assert!(
        vm.universe.gc_enabled,
        "full_gc called before genesis finished (SPEC §7.3 A1, shared with scavenge)"
    );
    debug_assert_eq!(
        vm.universe.to.top, vm.universe.to.start,
        "full_gc: to-space must be empty between collections (never nested inside a scavenge)"
    );
    // S11 D8: like `scavenge`, a compacting collection moves objects and
    // must never run while a compiled frame is live (its spill-slot oops
    // are invisible until S12). The `alloc::alloc_words` bridge arm and
    // `prim_gc_full`'s deferral (`gc_pending`) both keep this at 0; the
    // assert (plus `gc_under_compiled`, which stays live in `--release`
    // too) catches a future third door.
    if vm.compiled_depth > 0 {
        vm.universe.gc_stats.gc_under_compiled += 1;
    }
    debug_assert_eq!(
        vm.compiled_depth, 0,
        "full_gc under a live compiled frame (compiled_depth={}) — S11 D8 bridge violated",
        vm.compiled_depth
    );
    if super::verify::verify_enabled() {
        super::verify::verify_heap_at(vm, VerifyPoint::FullGcEntry)
            .expect("heap invalid at full-gc entry");
    }
    #[cfg(debug_assertions)]
    debug_assert_clean(vm);
    super::verify::dbg_oop_trace(vm, "full-gc-entry");

    let start = Instant::now();
    let eden_top_before = vm.universe.eden.top;
    let from_top_before = vm.universe.from.top;
    let old_top_before = vm.universe.old.top;

    // --- A ---------------------------------------------------------------
    let marked_bytes = mark(vm);
    super::verify::dbg_oop_trace(vm, "full-gc-mark");

    // --- A.5: weak sweep (S12 D5 point 2) --------------------------------
    // Must run HERE, strictly between phase A and phase B: `weak_sweep`
    // reads each alive nmethod's `key_klass`'s plain mark bit, which is
    // only meaningful in this exact window (phase B is about to overwrite
    // every MARKED object's header with a forwarding pointer; an
    // UNMARKED/dead one has no such promise at all, so this check would be
    // meaningless either before phase A finishes or after phase B starts
    // touching that specific klass's address).
    let flush_set = vm.code_table.weak_sweep();
    #[cfg(debug_assertions)]
    debug_assert_weak_sweep_invariant(vm, &flush_set);
    // D6 (`codecache::flush::flush_nmethod`): BEFORE phase B ever runs, so
    // no updater downstream can trip over a klass phase B never gave a
    // forwarding entry to (correctly so — it was never marked). Each
    // flush independently re-sweeps every STILL-alive nmethod's own sites
    // (including other, not-yet-processed `flush_set` members), so a
    // chain of mutually-referencing dying nmethods resolves correctly
    // regardless of iteration order — no ordering dependency between
    // members of this loop.
    for id in flush_set {
        crate::codecache::flush::flush_nmethod(vm, id);
    }

    // --- B ----------------------------------------------------------------
    let mut side_marks: SideMarks = HashMap::new();
    let (eden_plan, eden_new_top) =
        forwarding_compute(vm.universe.eden.start, eden_top_before, &mut side_marks);
    let (from_plan, from_new_top) =
        forwarding_compute(vm.universe.from.start, from_top_before, &mut side_marks);
    let (old_plan, old_new_top) = forwarding_compute(
        vm.universe.old.bounds.start,
        old_top_before,
        &mut side_marks,
    );
    retarget_dbg_oop(vm, &eden_plan);
    retarget_dbg_oop(vm, &from_plan);
    retarget_dbg_oop(vm, &old_plan);
    super::verify::dbg_oop_trace(vm, "full-gc-forwarding-compute");

    // --- C ------------------------------------------------------------
    super::roots::for_each_root(vm, |_vm, oop| forward_chase(oop));
    for e in eden_plan
        .iter()
        .chain(from_plan.iter())
        .chain(old_plan.iter())
    {
        rewrite_entry(e);
    }
    // S10/S11/S12 D4.1/D5: nmethod/adapter/PIC/mega pool oops, and (S12)
    // any live compiled/adapter frame's own spill/RootSpill slots, same
    // treatment as every other root — `forward_chase` reads phase B's
    // already-computed (not yet applied) forwarding addresses, so this
    // belongs in phase C alongside `for_each_root` above, ahead of phase
    // D's actual slide. `true`: unlike phase A's own mark call, the
    // customization key klass IS rewritten here — D5's own text, "weak ≠
    // unmaintained": a SURVIVING key's address must still be kept
    // current, only a genuinely dead one (already asserted impossible
    // above, pending step 6) is exempt from being marked THROUGH, never
    // from being maintained. `forward_chase` needs no `vm` (like
    // `mark_push`, unlike `scavenge_oop`), so `each_code_root`'s own
    // internal `std::mem::take` dance (needed only for `code_table`, the
    // one table `scavenge_oop`-style transforms actually require it for)
    // costs nothing extra here.
    super::roots::each_code_root(vm, true, |_vm, oop| forward_chase(oop));
    super::verify::dbg_oop_trace(vm, "full-gc-rewrite");

    // --- D ------------------------------------------------------------
    slide_young(&eden_plan);
    slide_young(&from_plan);
    slide_old(vm, &old_plan);

    vm.universe.eden.top = eden_new_top;
    vm.universe.from.top = from_new_top;
    vm.universe.old.top = old_new_top;

    #[cfg(debug_assertions)]
    {
        super::scavenge::poison_range(eden_new_top, eden_top_before);
        super::scavenge::poison_range(from_new_top, from_top_before);
        super::scavenge::poison_range(old_new_top, old_top_before);
    }
    super::verify::dbg_oop_trace(vm, "full-gc-slide");

    // --- E ------------------------------------------------------------
    restore_marks(&eden_plan, &side_marks);
    restore_marks(&from_plan, &side_marks);
    restore_marks(&old_plan, &side_marks);
    drop(side_marks);

    #[cfg(debug_assertions)]
    debug_assert_clean(vm);
    super::verify::dbg_oop_trace(vm, "full-gc-restore");

    // --- F ------------------------------------------------------------
    post(vm);

    let live_after = (eden_new_top - vm.universe.eden.start)
        + (from_new_top - vm.universe.from.start)
        + (old_new_top - vm.universe.old.bounds.start);
    debug_assert_eq!(
        live_after, marked_bytes,
        "phase A's marked_bytes and phase D's placed bytes disagree — lesson-8 cross-check"
    );
    let reclaimed_bytes = (eden_top_before - eden_new_top)
        + (from_top_before - from_new_top)
        + (old_top_before - old_new_top);

    let pause = start.elapsed();
    vm.universe.gc_stats.full_gc_count += 1;
    vm.universe.gc_stats.marked_bytes_last = marked_bytes as u64;
    vm.universe.gc_stats.full_pause_total += pause;
    vm.universe.gc_stats.last_reclaimed_bytes = reclaimed_bytes as u64;

    if super::verify::verify_enabled() {
        super::verify::verify_heap_at(vm, VerifyPoint::FullGcExit)
            .expect("heap invalid at full-gc exit");
    }
    super::verify::dbg_oop_trace(vm, "full-gc-exit");

    if vm.options.trace.is_enabled("gc") {
        let old_committed = vm.universe.old.committed_end - vm.universe.old.bounds.start;
        eprintln!(
            "[gc] full #{}: marked {}M, live {}M, reclaimed {}M, old {}M, {:.1}ms",
            vm.universe.gc_stats.full_gc_count,
            marked_bytes / (1 << 20),
            live_after / (1 << 20),
            reclaimed_bytes / (1 << 20),
            old_committed / (1 << 20),
            pause.as_secs_f64() * 1000.0,
        );
    }

    Ok(FullGcReport {
        marked_bytes,
        live_after,
        reclaimed_bytes,
        pause,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::alloc;
    use crate::oops::smi::SmallInt;
    use crate::runtime::vm_state::{VmOptions, VmState};

    fn test_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            gc_stress_full_period: None,
            eden_kb: None,
            jit: crate::runtime::JitMode::Off,
        })
    }

    fn is_marked(o: Oop) -> bool {
        MemOop::try_from(o).unwrap().mark().gc_mark()
    }

    /// S11 D8 step 10: same reasoning as `scavenge.rs`'s sibling test —
    /// `gc_under_compiled` is the `--release`-surviving proof the
    /// `debug_assert_eq!` right after it can't give on its own (it compiles
    /// out there), so it must count the violation before the assert panics.
    #[test]
    fn gc_under_compiled_counts_even_though_the_assert_aborts() {
        let mut vm = test_vm();
        vm.compiled_depth = 1; // pretend a compiled frame is on the stack
        if cfg!(debug_assertions) {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = full_gc(&mut vm);
            }));
            assert!(result.is_err(), "must debug_assert when compiled_depth > 0");
            assert_eq!(vm.universe.gc_stats.gc_under_compiled, 1);
        }
    }

    // ======================================================================
    // Phase A — mark (implementation-order step 3): cycles, diamonds,
    // self-refs, unreachable objects left unmarked.
    // ======================================================================

    #[test]
    fn mark_reaches_transitive_graph_and_stops_at_unreachable() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        let a = alloc::alloc_indexable_oops(&mut vm, klass, 1);
        let b = alloc::alloc_indexable_oops(&mut vm, klass, 1);
        let garbage = alloc::alloc_indexable_oops(&mut vm, klass, 1);
        a.at_put(0, b.oop());
        vm.stack.push(a.oop());

        mark(&mut vm);

        assert!(is_marked(a.oop()));
        assert!(is_marked(b.oop()));
        assert!(
            !is_marked(garbage.oop()),
            "unreachable object must NOT be marked"
        );
    }

    #[test]
    fn mark_terminates_on_self_reference() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        let a = alloc::alloc_indexable_oops(&mut vm, klass, 1);
        a.at_put(0, a.oop());
        vm.stack.push(a.oop());

        mark(&mut vm); // must terminate, not infinite-loop

        assert!(is_marked(a.oop()));
    }

    #[test]
    fn mark_terminates_on_a_cycle() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        let a = alloc::alloc_indexable_oops(&mut vm, klass, 1);
        let b = alloc::alloc_indexable_oops(&mut vm, klass, 1);
        a.at_put(0, b.oop());
        b.at_put(0, a.oop());
        vm.stack.push(a.oop());

        mark(&mut vm); // must terminate

        assert!(is_marked(a.oop()));
        assert!(is_marked(b.oop()));
    }

    /// A diamond's shared bottom node (reachable via two distinct paths)
    /// must enter the mark worklist exactly once — set-bit-on-PUSH is what
    /// guarantees this (a set-on-pop design would push it twice). Verified
    /// via an exact byte count, not just a boolean: comparing two freshly
    /// genesis'd VMs (deterministic layout, `it_memory.rs`'s own
    /// `genesis_is_deterministic` already proves this) isolates the
    /// diamond's own contribution from the — much larger — background
    /// root-reachable universe every `mark()` call also traces.
    #[test]
    fn mark_diamond_shares_bottom_node_exactly_once() {
        let background = mark(&mut test_vm());

        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        let bottom = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        let left = alloc::alloc_indexable_oops(&mut vm, klass, 1);
        let right = alloc::alloc_indexable_oops(&mut vm, klass, 1);
        let top = alloc::alloc_indexable_oops(&mut vm, klass, 2);
        left.at_put(0, bottom.oop());
        right.at_put(0, bottom.oop());
        top.at_put(0, left.oop());
        top.at_put(1, right.oop());
        vm.stack.push(top.oop());

        let with_diamond = mark(&mut vm);

        let sz = |o: Oop| MemOop::try_from(o).unwrap().instance_size_words() * WORD_SIZE;
        let expected = sz(bottom.oop()) + sz(left.oop()) + sz(right.oop()) + sz(top.oop());
        assert_eq!(
            with_diamond - background,
            expected,
            "bottom must be counted exactly once despite two inbound edges"
        );
    }

    /// A ByteArray's body is raw bytes, never oop slots (SPEC §7.3 A5
    /// format dispatch) — even when those bytes happen to hold the exact
    /// bit pattern of a live object's mem-tagged oop. Planting one proves
    /// `for_each_oop_field`'s format dispatch, not just convention, is what
    /// keeps `mark` from treating arbitrary bytes as pointers (tests_s08.md
    /// `mark_skips_raw_bodies`: a corruption guard, not an edge case).
    #[test]
    fn mark_skips_raw_bodies() {
        let mut vm = test_vm();
        let array_klass = vm.universe.array_klass;
        let bytes_klass = vm.universe.bytearray_klass;

        // Never rooted except via the planted raw bytes below.
        let victim = alloc::alloc_indexable_oops(&mut vm, array_klass, 0);
        let holder = alloc::alloc_indexable_bytes(&mut vm, bytes_klass, 8);
        // Body word 0 of an IndexableBytes object is its own size slot
        // (`raw_size_slot`) — the actual byte payload starts at word 1, so
        // the fake pointer must land there, not overwrite the size slot.
        for (i, b) in victim.oop().raw().to_le_bytes().into_iter().enumerate() {
            holder.byte_at_put(i, b);
        }
        vm.stack.push(holder.oop());

        mark(&mut vm);

        assert!(is_marked(holder.oop()));
        assert!(
            !is_marked(victim.oop()),
            "a byte pattern matching a live oop must not be scanned as a pointer"
        );
    }

    // ======================================================================
    // Phases B + E (implementation-order step 4): forwarding arithmetic,
    // side-map save/restore, on a single hand-placed range (no references).
    // ======================================================================

    #[test]
    fn forward_compute_all_live_self_forwards_with_no_gaps() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        let range_start = vm.universe.eden.top;

        let a = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        let b = alloc::alloc_indexable_oops(&mut vm, klass, 1);
        let range_end = vm.universe.eden.top;
        for o in [a.oop(), b.oop()] {
            let m = MemOop::try_from(o).unwrap();
            m.set_mark(m.mark().with_gc_mark(true));
        }

        let mut side_marks = HashMap::new();
        let (plan, new_top) = forwarding_compute(range_start, range_end, &mut side_marks);

        assert_eq!(plan.len(), 2);
        for e in &plan {
            assert_eq!(e.old, e.new, "zero dead bytes ⇒ every entry self-forwards");
        }
        assert_eq!(
            new_top, range_end,
            "no reclamation ⇒ the space's new top is unchanged"
        );
    }

    #[test]
    fn forward_compute_empty_space_yields_no_entries() {
        let vm = test_vm();
        let start = vm.universe.eden.top;

        let mut side_marks = HashMap::new();
        let (plan, new_top) = forwarding_compute(start, start, &mut side_marks);

        assert!(plan.is_empty());
        assert_eq!(new_top, start);
        assert!(side_marks.is_empty());
    }

    /// The memmove trap (SPEC §7.5 step 2's `ptr::copy`, not
    /// `copy_nonoverlapping`): a live object big enough that its downward
    /// shift is SMALLER than its own size has old/new byte ranges that
    /// genuinely overlap. `std::ptr::copy` must still carry every body word
    /// across intact.
    #[test]
    fn slide_overlap_safe() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        let range_start = vm.universe.eden.top;

        let dead = alloc::alloc_indexable_oops(&mut vm, klass, 0); // small — shift distance
        let live = alloc::alloc_indexable_oops(&mut vm, klass, 4); // bigger than the shift
        let vals = [11i64, 22, 33, 44];
        for (i, &v) in vals.iter().enumerate() {
            live.at_put(i, SmallInt::new(v).oop());
        }
        let range_end = vm.universe.eden.top;
        let _ = dead; // never marked below — this is what creates the shift

        let m = MemOop::try_from(live.oop()).unwrap();
        m.set_mark(m.mark().with_gc_mark(true));

        let mut side_marks = HashMap::new();
        let (plan, _new_top) = forwarding_compute(range_start, range_end, &mut side_marks);
        assert_eq!(plan.len(), 1);
        let entry = plan[0];
        let shift = entry.old - entry.new;
        assert!(
            shift > 0 && shift < entry.size_words as usize * WORD_SIZE,
            "test must actually exercise overlapping old/new ranges, not merely adjacent ones"
        );

        slide_young(&plan);

        // `array_klass` itself never moved (outside the tested range), so
        // the copied-but-not-yet-rewritten klass field at the new address
        // is still valid — safe to use the typed accessor rather than
        // hand-computing body-word offsets (body word 0 is the size slot,
        // not element 0; `at()` already knows that).
        let moved =
            crate::oops::wrappers::ArrayOop::try_from(Oop::from_raw(entry.new as u64 + MEM_TAG))
                .unwrap();
        for (i, &v) in vals.iter().enumerate() {
            assert_eq!(
                moved.at(i),
                SmallInt::new(v).oop(),
                "element {i} must survive an overlapping ptr::copy intact"
            );
        }
    }

    #[test]
    fn forwarding_compute_skips_dead_compacts_live_downward() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        let range_start = vm.universe.eden.top;

        let dead1 = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        let live1 = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        let dead2 = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        let live2 = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        let range_end = vm.universe.eden.top;
        let _ = (dead1, dead2); // never marked below — that's the point

        for o in [live1, live2] {
            let m = MemOop::try_from(o.oop()).unwrap();
            m.set_mark(m.mark().with_gc_mark(true));
        }

        let mut side_marks = HashMap::new();
        let (plan, new_top) = forwarding_compute(range_start, range_end, &mut side_marks);

        assert_eq!(plan.len(), 2, "only the 2 live objects get a CompactEntry");
        assert_eq!(plan[0].old, live1.oop().mem_addr());
        assert_eq!(plan[1].old, live2.oop().mem_addr());
        assert_eq!(
            plan[0].new, range_start,
            "live1 slides down into dead1's reclaimed slot"
        );
        assert_eq!(
            plan[1].new,
            plan[0].new + plan[0].size_words as usize * WORD_SIZE,
            "live2 packs immediately after live1's NEW position, not its old one"
        );
        assert_eq!(
            new_top,
            plan[1].new + plan[1].size_words as usize * WORD_SIZE
        );
        assert!(new_top < range_end, "dead objects' bytes must be reclaimed");

        // Forwarding is installed immediately (still at the OLD address —
        // phase D hasn't run yet).
        let obj1 = MemOop::try_from(live1.oop()).unwrap();
        assert!(obj1.is_forwarded());
        assert_eq!(obj1.forwardee().raw(), plan[0].new as u64 + MEM_TAG);
    }

    #[test]
    fn forwarding_compute_parks_state_carrying_marks() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        let range_start = vm.universe.eden.top;
        let obj = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        let range_end = vm.universe.eden.top;

        let m0 = MemOop::try_from(obj.oop()).unwrap();
        m0.set_mark(m0.mark().with_age(5).with_hash(0xABCD).with_gc_mark(true));

        let mut side_marks = HashMap::new();
        let (plan, _new_top) = forwarding_compute(range_start, range_end, &mut side_marks);

        let saved = *side_marks
            .get(&plan[0].new)
            .expect("a state-carrying mark must be parked");
        assert_eq!(saved.age(), 5);
        assert_eq!(saved.hash(), 0xABCD);
        assert!(
            !saved.gc_mark(),
            "gc_mark must be cleared before parking — full-GC-only, invariant-0 outside a cycle"
        );
    }

    #[test]
    fn forwarding_compute_does_not_park_a_state_free_mark() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        let range_start = vm.universe.eden.top;
        let obj = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        let range_end = vm.universe.eden.top;
        let m0 = MemOop::try_from(obj.oop()).unwrap();
        m0.set_mark(m0.mark().with_gc_mark(true)); // otherwise pristine

        let mut side_marks = HashMap::new();
        let (plan, _) = forwarding_compute(range_start, range_end, &mut side_marks);

        assert!(
            !side_marks.contains_key(&plan[0].new),
            "a state-free mark should not be parked — phase E's fresh-pristine path covers it for free"
        );
    }

    #[test]
    fn restore_marks_recomputes_tagged_contents_true_and_restores_state() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass; // IndexableOops: tagged
        let range_start = vm.universe.eden.top;
        let obj = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        let range_end = vm.universe.eden.top;
        let m0 = MemOop::try_from(obj.oop()).unwrap();
        m0.set_mark(m0.mark().with_age(9).with_gc_mark(true));

        let mut side_marks = HashMap::new();
        let (plan, _new_top) = forwarding_compute(range_start, range_end, &mut side_marks);
        // A single object alone in its range doesn't move — no need to
        // simulate phase D's slide to test restore in isolation.
        assert_eq!(plan[0].old, plan[0].new);

        restore_marks(&plan, &side_marks);

        let restored = MemOop::try_from(obj.oop()).unwrap();
        let mark = restored.mark();
        assert!(!mark.gc_mark());
        assert_eq!(mark.age(), 9);
        assert!(
            mark.tagged_contents(),
            "IndexableOops must recompute tagged_contents = true"
        );
        assert_eq!(
            restored.mark_word_raw() & crate::oops::layout::TAG_MASK,
            crate::oops::layout::MARK_TAG,
            "must be a real mark word again, not a leftover forwarding tag"
        );
    }

    #[test]
    fn restore_marks_recomputes_tagged_contents_false_for_untagged_format() {
        let mut vm = test_vm();
        let klass = vm.universe.bytearray_klass; // IndexableBytes: untagged
        let range_start = vm.universe.eden.top;
        let obj = alloc::alloc_indexable_bytes(&mut vm, klass, 3);
        let range_end = vm.universe.eden.top;
        let m0 = MemOop::try_from(obj.oop()).unwrap();
        m0.set_mark(m0.mark().with_gc_mark(true));

        let mut side_marks = HashMap::new();
        let (plan, _) = forwarding_compute(range_start, range_end, &mut side_marks);
        restore_marks(&plan, &side_marks);

        assert!(!MemOop::try_from(obj.oop())
            .unwrap()
            .mark()
            .tagged_contents());
    }

    // ======================================================================
    // Phase C rewrite + phase D slide + first end-to-end `full_gc`
    // (implementation-order step 5), plus F1/F3 (checked internally by
    // every `full_gc` call in debug builds) and identity-hash stability.
    // ======================================================================

    #[test]
    fn full_gc_compacts_and_preserves_reachable_graph() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        let child = alloc::alloc_indexable_oops(&mut vm, klass, 1);
        child.at_put(0, SmallInt::new(42).oop());
        let garbage = alloc::alloc_indexable_oops(&mut vm, klass, 5); // never rooted
        let root = alloc::alloc_indexable_oops(&mut vm, klass, 3);
        root.at_put(0, SmallInt::new(7).oop());
        root.at_put(1, vm.universe.true_obj);
        root.at_put(2, child.oop());
        vm.stack.push(root.oop());
        let _ = garbage;

        let report = full_gc(&mut vm).expect("full_gc must succeed");

        assert!(report.marked_bytes > 0);
        assert_eq!(
            report.marked_bytes, report.live_after,
            "phase A's count and phase D's placement must agree exactly"
        );
        assert!(
            report.reclaimed_bytes > 0,
            "the unrooted garbage array must be reclaimed"
        );

        let new_root =
            crate::oops::wrappers::ArrayOop::try_from(vm.stack.get(vm.stack.sp - 1)).unwrap();
        assert_eq!(new_root.at(0), SmallInt::new(7).oop());
        assert_eq!(new_root.at(1), vm.universe.true_obj);
        let new_child = crate::oops::wrappers::ArrayOop::try_from(new_root.at(2)).unwrap();
        assert_eq!(new_child.at(0), SmallInt::new(42).oop());
    }

    /// SPEC's own pitfall: "identity hash is load-bearing — losing a
    /// displaced mark corrupts dictionaries, which presents as random DNU
    /// after full GC, nowhere near the GC in a stack trace."
    #[test]
    fn full_gc_preserves_identity_hash() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        let obj = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        let h = vm.universe.identity_hash(obj.oop());
        vm.stack.push(obj.oop());

        full_gc(&mut vm).expect("full_gc must succeed");

        let moved = vm.stack.get(vm.stack.sp - 1);
        assert_eq!(
            h,
            vm.universe.identity_hash(moved),
            "identity hash must survive via the side map — Dictionary/MethodDictionary probe on it"
        );
    }

    /// Old gen gets fragmented (6 promotions, odd ones abandoned), then
    /// compacted: dead bytes reclaimed, surviving objects still resolve and
    /// are still old-gen.
    #[test]
    fn full_gc_compacts_fragmented_old_gen() {
        let mut vm = test_vm();
        vm.universe.tenuring_threshold = 0; // promote immediately
        let klass = vm.universe.array_klass;

        for _ in 0..6 {
            let o = alloc::alloc_indexable_oops(&mut vm, klass, 0);
            vm.stack.push(o.oop());
        }
        crate::memory::scavenge::scavenge(&mut vm).expect("scavenge must promote all 6");
        let sp0 = vm.stack.sp - 6;
        let promoted: Vec<Oop> = (0..6).map(|i| vm.stack.get(sp0 + i)).collect();
        for &p in &promoted {
            assert!(vm
                .universe
                .layout
                .is_old(MemOop::try_from(p).unwrap().addr()));
        }

        // Abandon the odd-indexed ones: pop the whole run, re-push evens only.
        vm.stack.sp = sp0;
        let mut kept = Vec::new();
        for (i, &p) in promoted.iter().enumerate() {
            if i % 2 == 0 {
                vm.stack.push(p);
                kept.push(p);
            }
        }
        let old_top_before = vm.universe.old.top;

        let report = full_gc(&mut vm).expect("full_gc must succeed");

        assert!(
            vm.universe.old.top < old_top_before,
            "the abandoned old-gen objects must be reclaimed"
        );
        assert!(report.reclaimed_bytes > 0);

        let sp1 = vm.stack.sp - kept.len();
        for i in 0..kept.len() {
            let moved = vm.stack.get(sp1 + i);
            assert!(
                vm.universe
                    .layout
                    .is_old(MemOop::try_from(moved).unwrap().addr()),
                "surviving object must still resolve to a live old-gen address"
            );
        }
    }

    /// Two consecutive full GCs with nothing surviving in between: F1's
    /// "outside a full GC, nothing is GC-marked or forwarded" must hold
    /// after EVERY cycle, not just the first — the second call's own
    /// entry-side `debug_assert_clean` is what actually catches a leak.
    #[test]
    fn full_gc_runs_twice_cleanly() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        let obj = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        vm.stack.push(obj.oop());

        full_gc(&mut vm).expect("first full_gc must succeed");
        full_gc(&mut vm).expect("second full_gc must succeed");
    }

    /// S8 table-inventory item: `MACVM_DBG_OOP` must follow its traced
    /// object through compaction, same as it already does through a
    /// scavenge — otherwise it silently traces a dead (POISONed, in debug
    /// builds) address for the rest of the run.
    #[test]
    fn full_gc_retargets_dbg_oop_to_the_moved_object() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        // A dead object ahead of it in address order forces an actual move
        // (compaction slides `obj` down into the reclaimed gap).
        let dead = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        let obj = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        let _ = dead;
        vm.stack.push(obj.oop());
        vm.dbg_oop = Some(obj.oop().mem_addr());

        full_gc(&mut vm).expect("full_gc must succeed");

        let moved = vm.stack.get(vm.stack.sp - 1);
        assert_ne!(
            moved.raw(),
            obj.oop().raw(),
            "the object must have actually moved"
        );
        assert_eq!(
            vm.dbg_oop,
            Some(moved.mem_addr()),
            "dbg_oop must follow the object to its new address"
        );
    }

    /// Age travels the same channel as identity hash (the displaced-mark
    /// side map, SPEC §2.2 Δ) — a survivor's age must be intact after a
    /// full GC, the same way `full_gc_preserves_identity_hash` proves hash
    /// is (tests_s08.md `ages_survive_full_gc`).
    #[test]
    fn ages_survive_full_gc() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        let dead = alloc::alloc_indexable_oops(&mut vm, klass, 0); // forces an actual move
        let obj = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        let _ = dead;
        let m = MemOop::try_from(obj.oop()).unwrap();
        m.set_mark(m.mark().with_age(5));
        vm.stack.push(obj.oop());

        full_gc(&mut vm).expect("full_gc must succeed");

        let moved = vm.stack.get(vm.stack.sp - 1);
        assert_ne!(
            moved.raw(),
            obj.oop().raw(),
            "the object must have actually moved"
        );
        assert_eq!(
            MemOop::try_from(moved).unwrap().mark().age(),
            5,
            "age must survive via the side map, same channel as identity hash"
        );
    }

    /// SPEC §7.5 phase D step 2 / F2: after compaction, every card covering
    /// a surviving object resolves to that object's NEW header — including
    /// cards in the MIDDLE of an object spanning more than one card, the
    /// case direct per-allocation bookkeeping and a full rebuild both have
    /// to get right independently (tests_s08.md `offset_table_rebuilt`).
    #[test]
    fn offset_table_rebuilt_covers_a_multi_card_object() {
        let mut vm = test_vm();
        vm.universe.tenuring_threshold = 0;
        let klass = vm.universe.array_klass;

        let dead = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        let big = alloc::alloc_indexable_oops(&mut vm, klass, 100); // spans multiple cards
        vm.stack.push(dead.oop());
        vm.stack.push(big.oop());
        crate::memory::scavenge::scavenge(&mut vm).expect("scavenge must promote both");

        // Abandon `dead`: pop both promoted addresses, keep only `big`'s.
        let big_promoted = vm.stack.get(vm.stack.sp - 1);
        vm.stack.sp -= 2;
        vm.stack.push(big_promoted);

        full_gc(&mut vm).expect("full_gc must succeed");

        let moved = vm.stack.get(vm.stack.sp - 1);
        let moved_addr = moved.mem_addr();
        let size_bytes = MemOop::try_from(moved).unwrap().instance_size_words() * WORD_SIZE;
        assert!(
            size_bytes > crate::memory::cards::CARD_SIZE,
            "test must actually exercise an object spanning multiple cards"
        );

        let c0 = vm.universe.offsets.card_index(moved_addr);
        let c_last = vm.universe.offsets.card_index(moved_addr + size_bytes - 1);
        assert!(c_last > c0, "must span at least 2 cards");
        // Card c0 only carries an entry (and thus resolves to `moved_addr`)
        // if the header sits exactly at that card's base (offsets.rs's own
        // module doc) — otherwise card_base(c0) is covered by whatever
        // object precedes `moved` in old gen, by design, so only the
        // interior/tail cards are unambiguous.
        for c in (c0 + 1)..=c_last {
            assert_eq!(
                vm.universe.offsets.resolve(c),
                moved_addr,
                "card {c} must resolve to the moved object's new header"
            );
        }
        if moved_addr == vm.universe.offsets.card_base(c0) {
            assert_eq!(vm.universe.offsets.resolve(c0), moved_addr);
        }
    }

    /// SPEC §7.5 phase F step 1: after a full GC, every card over the live
    /// old-gen range `[old.bounds.start, old.top)` is dirty (the
    /// deliberately-conservative "next scavenge self-corrects" reset) and
    /// every card above `old.top` is clean (tests_s08.md
    /// `card_reset_semantics`).
    #[test]
    fn card_reset_semantics_dirties_live_range_and_cleans_the_rest() {
        let mut vm = test_vm();
        vm.universe.tenuring_threshold = 0;
        let klass = vm.universe.array_klass;
        let obj = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        vm.stack.push(obj.oop());
        crate::memory::scavenge::scavenge(&mut vm).expect("scavenge must promote");

        full_gc(&mut vm).expect("full_gc must succeed");

        let c0 = vm.universe.cards.card_index(vm.universe.old.bounds.start);
        let c_last = vm.universe.cards.card_index(vm.universe.old.top - 1);
        for c in c0..=c_last {
            assert!(
                vm.universe.cards.is_dirty(c),
                "card {c} over the live old range must be dirty"
            );
        }
        let n = vm.universe.cards.n_cards();
        for c in (c_last + 1)..n {
            assert!(
                !vm.universe.cards.is_dirty(c),
                "card {c} above old.top must be clean"
            );
        }
    }

    /// A real, previously-undetected bug (found running `Smalltalk gcFull`
    /// from actual source, not by inspection): `for_each_root` rewrapped a
    /// forward-chased root via the VALIDATING `KlassOop`/`SymbolOop`/
    /// `MethodOop::try_from`, which checks shape by reading through the
    /// candidate's OWN klass field — a second hop that, during phase C, can
    /// land on an address phase D hasn't copied any bytes to yet. Every
    /// fullgc.rs test above sets `vm.regs.method` to `None` (none of them
    /// run the interpreter), so none could have caught this — the method
    /// mirror root was silently untested for the one property that
    /// actually matters: what happens when the traced object MOVES.
    #[test]
    fn full_gc_moves_interp_regs_method_correctly() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        // A dead object ahead of it forces an actual move, same as the
        // dbg_oop test above.
        let dead = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        let method = alloc::alloc_method(&mut vm, 8);
        let _ = dead;
        let sel = vm.universe.intern(b"probe");
        method.set_selector(sel.oop());
        vm.regs.method = Some(method);

        full_gc(&mut vm).expect("full_gc must succeed");

        let moved = vm.regs.method.expect("regs.method must survive a full GC");
        assert_ne!(
            moved.oop().raw(),
            method.oop().raw(),
            "the method must have actually moved"
        );
        // Compare against the symbol's OWN current (post-GC) address, not
        // `sel`'s stale pre-GC value — the symbol moved too. `intern` is
        // idempotent, so re-interning the same name is the correct way to
        // get its live address.
        let sel_now = vm.universe.intern(b"probe");
        assert_eq!(
            moved.selector(),
            sel_now.oop(),
            "the moved method must still be readable — a stale reconstruction \
             would have read garbage from an unpopulated address"
        );
    }
}
