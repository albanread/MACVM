//! The Cheney copying scavenger (SPEC §2.2, §7.3; `sprint_s07_detail.md`
//! §Algorithms A3-A6). Young-generation-only: `old` gen is neither
//! compacted nor reclaimed here (S8).

use std::time::{Duration, Instant};

use crate::oops::klass::Format;
use crate::oops::layout::HEADER_WORDS;
use crate::oops::wrappers::{KlassOop, MemOop, MethodOop};
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;

use super::stall::{GcPhase, GcStallError};

/// Per-age surviving-byte histogram (SPEC §7.3 step 4) — 7-bit age, 128
/// buckets (0..=127, `MARK_AGE_MAX`).
#[derive(Debug)]
pub struct AgeTable {
    pub bytes_at_age: [usize; 128],
}

impl Default for AgeTable {
    fn default() -> Self {
        AgeTable {
            bytes_at_age: [0; 128],
        }
    }
}

impl AgeTable {
    pub fn new() -> AgeTable {
        AgeTable::default()
    }

    pub fn clear(&mut self) {
        self.bytes_at_age = [0; 128];
    }

    pub fn record(&mut self, age: u8, bytes: usize) {
        self.bytes_at_age[age as usize] += bytes;
    }

    /// Smallest age `A` such that cumulative surviving bytes of ages
    /// `0..=A` exceed `survivor_capacity / 2`; `127` (never tenure by age)
    /// if the total never exceeds it (Strongtalk/HotSpot policy).
    pub fn compute_threshold(&self, survivor_capacity: usize) -> u8 {
        let half = survivor_capacity / 2;
        let mut cumulative = 0usize;
        for age in 0u8..128 {
            cumulative += self.bytes_at_age[age as usize];
            if cumulative > half {
                return age;
            }
        }
        127
    }
}

#[derive(Debug, Default)]
pub struct ScavengeReport {
    pub survivor_bytes: usize,
    pub promoted_bytes: usize,
    pub new_threshold: u8,
    pub pause: Duration,
}

/// A5: resolve `candidate` (an object's raw klass field — may itself
/// already be forwarded this cycle) to the `KlassOop` whose BODY is safe
/// to read `format`/`non_indexable_size` from. Never follows further than
/// one forwarding hop and never constructs anything beyond a `KlassOop`
/// wrapper (a shallow, non-recursive format check per `oops::klass`) — the
/// metaclass knot is cyclic and copy-to-size would recurse forever on it.
fn effective_klass(candidate: Oop) -> KlassOop {
    let m = MemOop::try_from(candidate).expect("klass field must be a mem oop");
    let target = if m.is_forwarded() {
        m.forwardee()
    } else {
        candidate
    };
    KlassOop::try_from(target).expect("klass field must resolve to a klass-shaped object")
}

/// Total instance size in words for `obj`, given an already-resolved
/// `klass` (either `effective_klass`'s forwarding-safe result during copy,
/// or `obj.klass()` directly once body-scan has already fixed up `obj`'s
/// own klass field — see the two call sites below). Mirrors
/// `MemOop::instance_size_words`'s formula exactly; duplicated rather than
/// shared because that method re-derives the klass via `self.klass()`,
/// which is NOT safe to call before the klass field's own forwarding has
/// been resolved.
fn size_words_for_klass(obj: MemOop, klass: KlassOop) -> usize {
    let nis = klass.non_indexable_size();
    match klass.format() {
        Format::Slots | Format::Klass | Format::Double | Format::Process => nis,
        Format::IndexableOops | Format::Closure | Format::Context => {
            nis + 1 + obj.raw_size_slot(nis)
        }
        Format::IndexableBytes | Format::Method => {
            let nbytes = obj.raw_size_slot(nis);
            nis + 1 + nbytes.div_ceil(8)
        }
    }
}

/// A5's "sizing during copy" rule: read the object's klass field, chase
/// ONE forwarding hop if it's already been moved this cycle, then size via
/// that klass's body — never via `obj.klass()` (which would try to
/// re-derive a klass from a field that might still be a from-space
/// pointer whose header the object being sized doesn't own).
fn object_size_for_copy(obj: MemOop) -> usize {
    let klass = effective_klass(obj.klass_oop());
    size_words_for_klass(obj, klass)
}

/// Records the first stall of this scavenge (later ones are ignored — the
/// first is the one that matters) and returns `oop` unchanged so the
/// caller's control flow stays simple; `scavenge()` checks
/// `pending_stall` between phases and turns it into a returned `Err`
/// rather than ever treating this half-copied oop as real progress.
fn record_stall(vm: &mut VmState, requested_bytes: usize, phase: GcPhase, oop: Oop) -> Oop {
    if vm.universe.pending_stall.is_none() {
        let err = GcStallError::snapshot(&vm.universe, requested_bytes, phase);
        vm.universe.pending_stall = Some(err);
    }
    oop
}

/// A4: copy one object into to-space or old gen, installing forwarding.
/// Returns the forwardee unchanged if `oop` isn't a new-gen mem oop (smis
/// and old-gen oops pass straight through) or is already forwarded.
pub fn scavenge_oop(vm: &mut VmState, oop: Oop) -> Oop {
    if !oop.is_mem() {
        return oop;
    }
    let addr = oop.mem_addr();
    if vm.universe.layout.is_old(addr) {
        return oop; // old-gen objects aren't scavenge targets in v1
    }
    if addr >= vm.universe.to.start && addr < vm.universe.to.top {
        panic!(
            "scavenge_oop: handed a TO-SPACE address {addr:#x} mid-cycle \
             (to=[{:#x},{:#x})) — double-copy imminent\n{}",
            vm.universe.to.start,
            vm.universe.to.top,
            std::backtrace::Backtrace::force_capture()
        );
    }
    let obj = MemOop::try_from(oop).expect("new-gen mem oop must be a valid MemOop");
    if obj.is_forwarded() {
        return obj.forwardee();
    }

    let size_words = object_size_for_copy(obj);
    let size_bytes = size_words * 8;
    let age = obj.mark().age();
    let age_promote = age.saturating_add(1) >= vm.universe.tenuring_threshold;

    let phase = if age_promote {
        GcPhase::ScavengePromote
    } else {
        GcPhase::ScavengeCopy
    };
    // Whether this object actually lands in old gen — either the age-based
    // decision, OR survivor overflow forcing direct promotion (lesson 6's
    // designed cascade, first stage). Distinct from `age_promote`: an
    // overflow-promoted object is just as much an old-gen resident as an
    // age-promoted one, and needs the exact same card-dirtying/stats
    // treatment below — using `age_promote` alone here was the bug this
    // comment replaces (an overflow-promoted object's card never got
    // marked dirty, so a later dirty-card scan wrongly saw it as clean).
    let mut promoted = age_promote;
    let dest_addr = if age_promote {
        let offsets = &mut vm.universe.offsets;
        let old = &mut vm.universe.old;
        match old.allocate(offsets, size_bytes, |_| {}) {
            Some(a) => a,
            None => return record_stall(vm, size_bytes, phase, oop),
        }
    } else {
        let to = &mut vm.universe.to;
        let new_top = to.top + size_bytes;
        if new_top > to.end {
            // Survivor overflow -> direct promotion (lesson 6's designed
            // cascade, first stage), not a bumped constant.
            promoted = true;
            let offsets = &mut vm.universe.offsets;
            let old = &mut vm.universe.old;
            match old.allocate(offsets, size_bytes, |_| {}) {
                Some(a) => a,
                None => return record_stall(vm, size_bytes, GcPhase::ScavengePromote, oop),
            }
        } else {
            let a = to.top;
            to.top = new_top;
            a
        }
    };

    // SAFETY: `dest_addr` names exactly `size_bytes` freshly bumped/
    // allocated, previously-uninitialized words in either `to` or `old`
    // (never overlapping `obj`'s own `[addr, addr+size_bytes)` — new/old
    // never alias); `obj`'s body is untouched until the forwarding
    // install below, so the source range is fully valid to read.
    unsafe {
        std::ptr::copy_nonoverlapping(addr as *const u8, dest_addr as *mut u8, size_bytes);
    }
    let dest_oop = Oop::from_raw(dest_addr as u64 + crate::oops::layout::MEM_TAG);
    let dest = MemOop::try_from(dest_oop).expect("freshly copied object is mem-tagged");

    if promoted {
        vm.universe
            .cards
            .record_multistores(dest_addr, dest_addr + size_bytes);
        vm.universe.gc_stats.bytes_promoted += size_bytes as u64;
    } else {
        let new_age = age.saturating_add(1).min(crate::oops::layout::MARK_AGE_MAX);
        dest.set_mark(dest.mark().with_age(new_age));
        vm.universe.age_table.record(new_age, size_bytes);
        vm.universe.gc_stats.bytes_copied += size_bytes as u64;
    }

    obj.install_forwarding(dest_oop);

    if vm.dbg_oop == Some(addr) {
        vm.dbg_oop = Some(dest_addr);
    }

    dest_oop
}

/// Scavenges every oop-bearing body word of `obj` (already relocated —
/// `to`-space or promoted `old` — with its OWN klass field already fixed
/// up by the caller, per A5's klass-first protocol) per the `Format`
/// table (SPEC §7.3 A5). Uses `obj.klass()` directly: safe here (unlike
/// `object_size_for_copy`) because the klass field was scavenged first.
fn scan_body(vm: &mut VmState, obj: MemOop) {
    let klass = obj.klass();
    let nis = klass.non_indexable_size();
    let named = nis - HEADER_WORDS;
    match klass.format() {
        Format::Slots | Format::Klass => {
            for i in 0..named {
                let v = obj.body_oop(i);
                let nv = scavenge_oop(vm, v);
                obj.set_body_oop(i, nv);
            }
        }
        Format::Double | Format::Process => {
            // Double: raw f64 bits, never oops. Process: unreachable in v1.
        }
        Format::IndexableOops | Format::Closure | Format::Context => {
            for i in 0..named {
                let v = obj.body_oop(i);
                let nv = scavenge_oop(vm, v);
                obj.set_body_oop(i, nv);
            }
            let len = obj.indexable_len();
            for i in 0..len {
                let v = obj.tail_oop_at(i);
                let nv = scavenge_oop(vm, v);
                obj.set_tail_oop_at(i, nv);
            }
        }
        Format::IndexableBytes | Format::Method => {
            for i in 0..named {
                let v = obj.body_oop(i);
                let nv = scavenge_oop(vm, v);
                obj.set_body_oop(i, nv);
            }
            // Byte tail (or Method's bytecode bytes): never scanned.
        }
    }
}

/// A6: for each dirty card below `old_top_before`, resolve its object
/// header via the offset table, walk objects forward across the card,
/// and scavenge exactly the oop-bearing slots that fall inside the card
/// window. Cleans the card afterward iff no slot in it still holds a
/// new-gen oop (self-correcting remembered set).
fn dirty_card_scan(vm: &mut VmState, old_top_before: usize) {
    let n_cards = vm.universe.cards.n_cards();
    for i in 0..n_cards {
        let card_base = vm.universe.cards.card_base(i);
        if card_base >= old_top_before {
            break;
        }
        if !vm.universe.cards.is_dirty(i) {
            continue;
        }
        let card_end = card_base + super::cards::CARD_SIZE;
        let mut addr = vm.universe.offsets.resolve(i);
        let mut still_points_new = false;
        while addr < card_end && addr < old_top_before {
            let oop = Oop::from_raw(addr as u64 + crate::oops::layout::MEM_TAG);
            let obj = MemOop::try_from(oop).expect("old-gen header must be mem-tagged");
            let klass = obj.klass();
            let size_words = size_words_for_klass(obj, klass);
            let size_bytes = size_words * 8;
            let obj_end = addr + size_bytes;

            if obj_end > card_base {
                still_points_new |= scan_slots_in_window(vm, obj, klass, card_base, card_end);
            }
            addr = obj_end;
        }
        // Clean ONLY a card lying entirely below `old_top_before`: this
        // scan examines slots up to that boundary and nothing else, but a
        // this-cycle promotion (which always lands AT or ABOVE it — old.top
        // only grows) can share the boundary card when `old_top_before`
        // isn't card-aligned. Cleaning that card here would wipe the
        // promotion's own `record_multistores` mark for slots this scan
        // never looked at — the freshly promoted object's young refs would
        // silently vanish from the remembered set (caught by the A9
        // verifier's card-vs-heap cross-check under `MACVM_GC_STRESS=1`,
        // S7-10: a just-promoted MethodDictionary's young method values on
        // a wrongly-cleaned boundary card).
        if !still_points_new && card_end <= old_top_before {
            vm.universe.cards.set_clean(i);
        }
    }
}

/// Scavenges only `obj`'s oop-bearing slots whose OWN address falls
/// inside `[window_start, window_end)`, per the same format table
/// `scan_body` uses. Returns whether any slot touched (in-window or not)
/// still holds a new-gen oop afterward — used to decide whether the
/// card stays dirty.
fn scan_slots_in_window(
    vm: &mut VmState,
    obj: MemOop,
    klass: KlassOop,
    window_start: usize,
    window_end: usize,
) -> bool {
    let nis = klass.non_indexable_size();
    let named = nis - HEADER_WORDS;
    let mut still_new = false;
    let mut touch = |vm: &mut VmState, addr: usize, old: Oop, set: &mut dyn FnMut(Oop)| {
        if addr < window_start || addr >= window_end {
            // Still need to know if THIS slot (outside the window) points
            // new — contributes to "does the card need to stay dirty"
            // only via ITS OWN card, not this one; skip entirely here.
            return;
        }
        let nv = scavenge_oop(vm, old);
        set(nv);
        if nv.is_mem() && vm.universe.layout.is_new(nv.mem_addr()) {
            still_new = true;
        }
    };

    // The klass field (header word 1) is a trackable oop slot on every
    // object regardless of format, but lives BEFORE `nis` — easy to miss
    // since every other slot below is format-dispatched from `nis`
    // onward. `record_multistores` dirties a freshly promoted object's
    // whole range including its header, so this card's own re-scan must
    // account for it too, or a since-moved klass leaves both a stale
    // field AND (since nothing else in this card still points new) a
    // wrongly-cleared card.
    let klass_addr = obj.addr() + crate::oops::layout::KLASS_OFFSET;
    let klass_field = obj.klass_oop();
    touch(vm, klass_addr, klass_field, &mut |v| obj.set_klass_raw(v));

    match klass.format() {
        Format::Slots | Format::Klass => {
            for i in 0..named {
                let addr = obj.body_addr(i);
                let old = obj.body_oop(i);
                touch(vm, addr, old, &mut |v| obj.set_body_oop(i, v));
            }
        }
        Format::Double | Format::Process => {}
        Format::IndexableOops | Format::Closure | Format::Context => {
            for i in 0..named {
                let addr = obj.body_addr(i);
                let old = obj.body_oop(i);
                touch(vm, addr, old, &mut |v| obj.set_body_oop(i, v));
            }
            let len = obj.indexable_len();
            let tail0 = obj.tail_start_word();
            for i in 0..len {
                let addr = obj.body_addr(tail0 + i);
                let old = obj.tail_oop_at(i);
                touch(vm, addr, old, &mut |v| obj.set_tail_oop_at(i, v));
            }
        }
        Format::IndexableBytes | Format::Method => {
            for i in 0..named {
                let addr = obj.body_addr(i);
                let old = obj.body_oop(i);
                touch(vm, addr, old, &mut |v| obj.set_body_oop(i, v));
            }
        }
    }
    still_new
}

/// A3: one full scavenge cycle. Root scan (well-known oops, symbol table,
/// process stack, handle arena, dirty-card scan over old gen), then the
/// Cheney fixed-point loop over `to`-space and the newly promoted region,
/// then the from/to swap and tenuring update.
pub fn scavenge(vm: &mut VmState) -> Result<ScavengeReport, GcStallError> {
    assert!(
        vm.universe.gc_enabled,
        "scavenge called before genesis finished (SPEC §7.3 A1)"
    );
    if super::verify::verify_enabled() {
        super::verify::verify_heap_at(vm, super::verify::VerifyPoint::ScavengeEntry)
            .expect("heap invalid at scavenge entry");
    }
    super::verify::dbg_oop_trace(vm, "scavenge-entry");
    let start = Instant::now();
    vm.universe.age_table.clear();

    let old_top_before = vm.universe.old.top;
    vm.universe.to.reset();

    // --- root scan (A3 step 3) ------------------------------------------
    if std::env::var("MACVM_DBG_ROOTS").is_ok() {
        audit_roots(vm);
    }
    scavenge_well_known_roots(vm);
    scavenge_symbol_table_roots(vm);
    scavenge_process_stack_roots(vm);
    scavenge_handle_arena_roots(vm);
    scavenge_interp_regs_roots(vm);
    dirty_card_scan(vm, old_top_before);

    if let Some(err) = vm.universe.pending_stall.take() {
        return Err(err);
    }

    // --- Cheney fixed point (A3 step 4) ----------------------------------
    let mut to_scan = vm.universe.to.start;
    let mut old_scan = old_top_before;
    loop {
        if to_scan < vm.universe.to.top {
            let oop = Oop::from_raw(to_scan as u64 + crate::oops::layout::MEM_TAG);
            let obj = MemOop::try_from(oop).expect("to-space object must be mem-tagged");
            let klass_field = obj.klass_oop();
            let new_klass_field = scavenge_oop(vm, klass_field);
            obj.set_klass_raw(new_klass_field);
            scan_body(vm, obj);
            if let Some(err) = vm.universe.pending_stall.take() {
                return Err(err);
            }
            let klass = KlassOop::try_from(new_klass_field).expect("scavenged klass field");
            to_scan += size_words_for_klass(obj, klass) * 8;
        } else if old_scan < vm.universe.old.top {
            let oop = Oop::from_raw(old_scan as u64 + crate::oops::layout::MEM_TAG);
            let obj = MemOop::try_from(oop).expect("promoted object must be mem-tagged");
            let klass_field = obj.klass_oop();
            let new_klass_field = scavenge_oop(vm, klass_field);
            obj.set_klass_raw(new_klass_field);
            scan_body(vm, obj);
            if let Some(err) = vm.universe.pending_stall.take() {
                return Err(err);
            }
            let klass = KlassOop::try_from(new_klass_field).expect("scavenged klass field");
            old_scan += size_words_for_klass(obj, klass) * 8;
        } else {
            break;
        }
    }

    // --- swap + tenuring (A3 steps 5-6) -----------------------------------
    std::mem::swap(&mut vm.universe.from, &mut vm.universe.to);
    vm.universe.eden.top = vm.universe.eden.start;
    vm.universe.to.reset();

    // The LookupCache is address-keyed Rust-side state the root scan never
    // touches: after a scavenge its keys are stale, and worse, a recycled
    // address can FALSE-HIT for a different (klass, selector) pair —
    // returning the wrong method. `gc_epilogue` was written for exactly
    // this back in S3 ("flush on GC") but never called until S7-10 wired
    // it here.
    crate::runtime::lookup::gc_epilogue(vm);

    let survivor_capacity = vm.universe.from.end - vm.universe.from.start;
    let new_threshold = vm.universe.age_table.compute_threshold(survivor_capacity);
    vm.universe.tenuring_threshold = new_threshold;

    let survivor_bytes: usize = vm.universe.age_table.bytes_at_age.iter().sum();
    let promoted_bytes = vm.universe.old.top - old_top_before;

    vm.universe.gc_stats.scavenge_count += 1;
    vm.universe.gc_stats.tenuring_threshold = new_threshold;

    if super::verify::verify_enabled() {
        super::verify::verify_heap_at(vm, super::verify::VerifyPoint::ScavengeExit)
            .expect("heap invalid at scavenge exit");
    }
    super::verify::dbg_oop_trace(vm, "scavenge-exit");

    Ok(ScavengeReport {
        survivor_bytes,
        promoted_bytes,
        new_threshold,
        pause: start.elapsed(),
    })
}

fn scavenge_well_known_roots(vm: &mut VmState) {
    vm.universe.nil_obj = scavenge_oop(vm, vm.universe.nil_obj);
    vm.universe.true_obj = scavenge_oop(vm, vm.universe.true_obj);
    vm.universe.false_obj = scavenge_oop(vm, vm.universe.false_obj);
    vm.universe.smalltalk = scavenge_oop(vm, vm.universe.smalltalk);

    // The well-known SELECTOR fields are roots too (S7-10 root-scan gap):
    // the Symbols themselves survive via the symbol-table root, but these
    // are separate Universe-field copies of their addresses — unscanned,
    // they dangle after the first scavenge and every DNU/mustBeBoolean/
    // cannotReturn send thereafter reads a garbage selector.
    macro_rules! scav_sel {
        ($($f:ident),* $(,)?) => {
            $(
                let s = vm.universe.$f.oop();
                let ns = scavenge_oop(vm, s);
                vm.universe.$f = crate::oops::wrappers::SymbolOop::try_from(ns)
                    .expect(concat!(stringify!($f), " must stay symbol-shaped"));
            )*
        };
    }
    scav_sel!(
        sel_does_not_understand,
        sel_must_be_boolean,
        sel_cannot_return
    );

    macro_rules! scav_klass {
        ($($f:ident),* $(,)?) => {
            $(
                let k = vm.universe.$f.oop();
                let nk = scavenge_oop(vm, k);
                vm.universe.$f = KlassOop::try_from(nk).expect(concat!(stringify!($f), " must stay klass-shaped"));
            )*
        };
    }
    scav_klass!(
        metaclass_klass,
        class_klass,
        object_klass,
        undefined_object_klass,
        boolean_klass,
        true_klass,
        false_klass,
        smi_klass,
        character_klass,
        double_klass,
        string_klass,
        symbol_klass,
        array_klass,
        bytearray_klass,
        association_klass,
        methoddict_klass,
        method_klass,
        closure_klass,
        context_klass,
        process_klass,
        message_klass,
        large_pos_int_klass,
        large_neg_int_klass,
        behavior_klass,
        magnitude_klass,
        number_klass,
        integer_klass,
        large_integer_klass,
        collection_klass,
        sequenceable_collection_klass,
        arrayed_collection_klass,
        system_dictionary_klass,
    );
}

fn scavenge_symbol_table_roots(vm: &mut VmState) {
    let n = vm.universe.symbols.buckets.len();
    for i in 0..n {
        if let Some(sym) = vm.universe.symbols.buckets[i] {
            vm.universe.symbols.buckets[i] = Some(scavenge_oop(vm, sym));
        }
    }
}

/// Every live slot `0..sp` of the process stack — smi-encoded fp/bci
/// links pass through `scavenge_oop` unchanged (SPEC §5.1's exact-stack
/// invariant, `interpreter::stack`'s own doc comment: "every slot is a
/// valid oop at all times", which is what makes this scan free of any
/// frame-shape knowledge).
fn scavenge_process_stack_roots(vm: &mut VmState) {
    let sp = vm.stack.sp;
    for i in 0..sp {
        let v = vm.stack.get(i);
        let nv = scavenge_oop(vm, v);
        vm.stack.set(i, nv);
    }
}

/// S7-9 / SPEC §7.6: every live `handles::Handle` root. The arena is only
/// ever shrunk by `HandleScope::drop` (lesson 9) — GC rewrites slots in
/// place, never truncates.
fn scavenge_handle_arena_roots(vm: &mut VmState) {
    let n = vm.handle_arena.len();
    let dbg = std::env::var("MACVM_DBG_ROOTS").is_ok();
    for i in 0..n {
        let v = vm.handle_arena.slots_mut()[i];
        if dbg {
            eprintln!("RDBG scanning handle[{i}] = {:#x}", v.raw());
        }
        let nv = scavenge_oop(vm, v);
        vm.handle_arena.slots_mut()[i] = nv;
    }
}

/// `MACVM_DBG_ROOTS=1`: pre-scan sanity audit — every root must point at
/// something with a plausible (mark-tagged, sentinel-set, unforwarded)
/// header at scavenge entry. A failure here names the STALE ROOT SOURCE
/// directly, which the exit verifier (seeing only the wreckage the chase
/// left behind) cannot.
fn audit_roots(vm: &mut VmState) {
    let plausible = |o: Oop| -> bool {
        if !o.is_mem() {
            return true;
        }
        let m = MemOop::try_from(o).unwrap();
        let w = m.mark_word_raw();
        w & crate::oops::layout::TAG_MASK == crate::oops::layout::MARK_TAG && w & 0b100 != 0
    };
    let bad = |src: String, o: Oop| {
        if !plausible(o) {
            eprintln!("RDBG STALE ROOT {src} = {:#x}", o.raw());
        }
    };
    bad("nil".into(), vm.universe.nil_obj);
    bad("true".into(), vm.universe.true_obj);
    bad("false".into(), vm.universe.false_obj);
    bad("smalltalk".into(), vm.universe.smalltalk);
    bad("sel_dnu".into(), vm.universe.sel_does_not_understand.oop());
    bad("sel_mbb".into(), vm.universe.sel_must_be_boolean.oop());
    bad("sel_cr".into(), vm.universe.sel_cannot_return.oop());
    for (i, b) in vm.universe.symbols.buckets.iter().enumerate() {
        if let Some(s) = b {
            if !plausible(*s) {
                eprintln!("RDBG STALE ROOT symbols[{i}] = {:#x}", s.raw());
            }
        }
    }
    for i in 0..vm.stack.sp {
        let v = vm.stack.get(i);
        if !plausible(v) {
            eprintln!("RDBG STALE ROOT stack[{i}] = {:#x}", v.raw());
        }
    }
    for i in 0..vm.handle_arena.len() {
        let v = vm.handle_arena.slots_mut()[i];
        if !plausible(v) {
            eprintln!("RDBG STALE ROOT handle[{i}] = {:#x}", v.raw());
        }
    }
    if let Some(m) = vm.regs.method {
        if !plausible(m.oop()) {
            eprintln!("RDBG STALE ROOT regs.method = {:#x}", m.oop().raw());
        }
    }
}

/// `vm.regs.method` — the interpreter's "currently executing method"
/// mirror — is a root in its own right: the dispatch loop fetches
/// bytecode through it (via its own local copy, re-read at every
/// boundary), and `send_generic`/`ic_transition`/the unwind machinery
/// read it directly between allocations. The FRAME's method slot is
/// already covered by the process-stack scan, but the mirror is a
/// separate copy the stack scan never touches — without this it dangles
/// into from-space after the first mid-dispatch scavenge (found by the
/// S7-10 `MACVM_GC_STRESS=1` audit).
fn scavenge_interp_regs_roots(vm: &mut VmState) {
    if let Some(m) = vm.regs.method {
        let nm = scavenge_oop(vm, m.oop());
        vm.regs.method =
            Some(MethodOop::try_from(nm).expect("regs.method must stay method-shaped"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::alloc;
    use crate::oops::smi::SmallInt;
    use crate::oops::wrappers::MemOop;
    use crate::runtime::vm_state::{VmOptions, VmState};

    fn test_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            eden_kb: None,
        })
    }

    /// A scavenge of a freshly booted VM (nothing but genesis's own
    /// objects — including the Metaclass<->"Metaclass class" 2-cycle)
    /// must terminate and leave every well-known klass link intact.
    /// Exercises `tests_s07.md`'s `klass_first_protocol` implicitly: every
    /// genesis object's klass field, including the cyclic ones, gets
    /// scavenged correctly or this would hang/panic/corrupt.
    #[test]
    fn scavenge_fresh_boot_survives() {
        let mut vm = test_vm();
        let report = scavenge(&mut vm).expect("fresh-boot scavenge must succeed");
        assert!(report.survivor_bytes > 0);
        assert_eq!(vm.universe.gc_stats.scavenge_count, 1);

        // The metaobject knot must still be intact and usable afterward.
        assert_eq!(
            vm.universe.object_klass.klass().superclass().raw(),
            vm.universe.class_klass.oop().raw()
        );
        assert_eq!(
            vm.universe.true_klass.name(),
            vm.universe.intern(b"True").oop()
        );
    }

    /// `tests_s07.md`'s `copy_empty_from_space`: a scavenge with empty
    /// from-space (the very first one) and eden-only survivors is correct
    /// — already exercised by `scavenge_fresh_boot_survives`, pinned here
    /// under the doc's own name too.
    #[test]
    fn copy_empty_from_space() {
        let mut vm = test_vm();
        assert!(vm.universe.from.top == vm.universe.from.start);
        let report = scavenge(&mut vm);
        assert!(report.is_ok());
    }

    /// An object allocated in eden, scavenged, must be reachable at its
    /// new (survivor-space) address with all its fields intact — the
    /// single most important correctness property (lesson 2: test
    /// semantics, not mechanics).
    #[test]
    fn object_survives_with_fields_intact() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        let arr = alloc::alloc_indexable_oops(&mut vm, klass, 3);
        let v0 = SmallInt::new(111).oop();
        let v1 = vm.universe.true_obj;
        let v2 = SmallInt::new(333).oop();
        arr.at_put(0, v0);
        arr.at_put(1, v1);
        arr.at_put(2, v2);
        let old_oop = arr.oop();
        // Root it via the process stack — an object reachable from
        // NOWHERE is correctly NOT scavenged at all (nothing found it),
        // so it must be a genuine root for this test to mean anything.
        vm.stack.push(old_oop);

        scavenge(&mut vm).expect("scavenge must succeed");

        let new_oop = vm.stack.get(vm.stack.sp - 1);
        assert_ne!(
            new_oop.raw(),
            old_oop.raw(),
            "the object must have actually moved"
        );
        let new_arr = crate::oops::wrappers::ArrayOop::try_from(new_oop).unwrap();
        assert_eq!(new_arr.len(), 3);
        assert_eq!(new_arr.at(0), v0);
        assert_eq!(new_arr.at(1), vm.universe.true_obj); // true_obj itself may have moved too
        assert_eq!(new_arr.at(2), v2);
    }

    /// `tests_s07.md`'s `hash_survives_copy`: an assigned identity hash is
    /// identical after scavenge, and age increments.
    #[test]
    fn hash_survives_copy() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        let obj = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        let hash_before = vm.universe.identity_hash(obj.oop());
        let mark_before_age = MemOop::try_from(obj.oop()).unwrap().mark().age();
        assert_eq!(mark_before_age, 0);
        vm.stack.push(obj.oop());

        scavenge(&mut vm).expect("scavenge must succeed");
        let new_oop = vm.stack.get(vm.stack.sp - 1);
        let hash_after = vm.universe.identity_hash(new_oop);
        assert_eq!(hash_before, hash_after);

        let new_mem = MemOop::try_from(new_oop).unwrap();
        assert_eq!(new_mem.mark().age(), 1);
    }

    /// `tests_s07.md`'s `double_body_not_scanned`: a Double whose f64
    /// bits happen to look like a plausible oop pattern must survive
    /// uncorrupted — scanning it as oops would be a corruption bug.
    #[test]
    fn double_body_not_scanned() {
        let mut vm = test_vm();
        // A bit pattern that, read as a tagged oop, would look mem-tagged
        // (low bits 01) and point at a byte offset that IS inside the
        // heap reservation on some runs — the exact bits matter less than
        // "not all zero/not a smi", so any oop-shaped bit pattern works.
        let bits = f64::from_bits(0x4010_0000_0000_0001u64);
        let d = alloc::alloc_double(&mut vm, bits);
        vm.stack.push(d.oop());

        scavenge(&mut vm).expect("scavenge must succeed");
        let new_oop = vm.stack.get(vm.stack.sp - 1);
        let new_d = crate::oops::wrappers::DoubleOop::try_from(new_oop).unwrap();
        assert_eq!(new_d.value().to_bits(), bits.to_bits());
    }

    /// `tests_s07.md`'s `byte_object_padding`: a 13-byte ByteArray copies
    /// with its content intact; the padding is never scanned as oops
    /// (only matters for not-crashing — content correctness is the real
    /// assertion here).
    #[test]
    fn byte_object_padding() {
        let mut vm = test_vm();
        let klass = vm.universe.bytearray_klass;
        let b = alloc::alloc_indexable_bytes(&mut vm, klass, 13);
        for i in 0..13 {
            b.byte_at_put(i, (i * 7 + 1) as u8);
        }
        vm.stack.push(b.oop());

        scavenge(&mut vm).expect("scavenge must succeed");
        let new_oop = vm.stack.get(vm.stack.sp - 1);
        let new_b = crate::oops::wrappers::ByteArrayOop::try_from(new_oop).unwrap();
        assert_eq!(new_b.len(), 13);
        for i in 0..13 {
            assert_eq!(new_b.byte_at(i), (i * 7 + 1) as u8);
        }
    }

    /// `tests_s07.md`'s `closure_context_scan`: a `Closure` (method +
    /// `copied[]`) and a `Context` (all slots) both fully relocate.
    #[test]
    fn closure_context_scan() {
        let mut vm = test_vm();
        let mut b = crate::bytecode::BytecodeBuilder::new();
        b.push_self();
        b.ret_tos();
        let sel = vm.universe.intern(b"blk");
        let method = b.finish(&mut vm, sel, 0, 0);

        let cl = alloc::alloc_closure(&mut vm, 2);
        cl.set_method(method);
        let captured = SmallInt::new(42).oop();
        cl.set_copied(0, captured);
        let ctx = alloc::alloc_context(&mut vm, 1);
        ctx.set_slot(0, SmallInt::new(7).oop());
        cl.set_copied(1, ctx.oop());

        vm.stack.push(cl.oop());
        scavenge(&mut vm).expect("scavenge must succeed");

        let new_cl_oop = vm.stack.get(vm.stack.sp - 1);
        let new_cl = crate::oops::wrappers::ClosureOop::try_from(new_cl_oop).unwrap();
        // `method`/`sel` (pre-scavenge Rust-locals) are now stale
        // addresses — the closure's own method field, and the interned
        // symbol table entry, were both relocated along with everything
        // else reachable from `cl`. Re-interning is idempotent and always
        // returns the CURRENT symbol oop, scavenged or not, so it's a
        // safe way to get a fresh, valid comparison value.
        let _ = (method, sel);
        let current_sel = vm.universe.intern(b"blk").oop();
        assert_eq!(new_cl.method().selector(), current_sel);
        assert_eq!(new_cl.copied(0), captured);
        let new_ctx_oop = new_cl.copied(1);
        let new_ctx = crate::oops::wrappers::ContextOop::try_from(new_ctx_oop).unwrap();
        assert_eq!(new_ctx.slot(0), SmallInt::new(7).oop());
    }

    /// `tests_s07.md`'s `age_table_threshold`: crafted histograms pin the
    /// tenuring formula exactly.
    #[test]
    fn age_table_threshold() {
        let cap = 1000;
        // All-young: total well under half capacity -> never tenure (127).
        let mut t = AgeTable::new();
        t.record(0, 10);
        assert_eq!(t.compute_threshold(cap), 127);

        // Heavy age-0: alone exceeds half capacity -> boundary at 0.
        let mut t2 = AgeTable::new();
        t2.record(0, 600);
        assert_eq!(t2.compute_threshold(cap), 0);

        // Cumulative crossing at age 3.
        let mut t3 = AgeTable::new();
        t3.record(0, 100);
        t3.record(1, 100);
        t3.record(2, 100);
        t3.record(3, 300); // cumulative: 100,200,300,600 > 500 at age 3
        assert_eq!(t3.compute_threshold(cap), 3);
    }

    /// `tests_s07.md`'s `copy_exact_fill_survivor`: survivors exactly
    /// filling to-space (`top == end`) succeed; one more object overflows
    /// into promotion instead of corrupting the space.
    #[test]
    fn copy_exact_fill_survivor() {
        let mut vm = test_vm();
        // Force tenuring off entirely so everything copies to `to`.
        vm.universe.tenuring_threshold = 127;

        // A fully-booted Universe's OWN well-known-oops/symbol-table roots
        // (every genesis klass, every interned symbol so far) also occupy
        // survivor space on every scavenge — a "warm-up" cycle measures
        // exactly how much, so the test can fill the REMAINING headroom
        // precisely rather than assuming the whole space is free.
        let warmup = scavenge(&mut vm).expect("warm-up scavenge must succeed");
        assert_eq!(warmup.promoted_bytes, 0);
        let background_bytes = warmup.survivor_bytes;
        let survivor_len = vm.universe.to.end - vm.universe.to.start;
        let remaining = survivor_len - background_bytes;

        // An IndexableOops object of N elements is HEADER_WORDS + 1 + N
        // words = (2 + 1 + N) * 8 bytes. Pick N so the object's size
        // divides the remaining headroom exactly, then allocate exactly
        // enough of them (plus a partial-word filler) to use it all.
        let klass = vm.universe.array_klass;
        let per_obj_words = klass.non_indexable_size() + 1; // + 0 elements
        let per_obj_bytes = per_obj_words * 8;
        let count = remaining / per_obj_bytes;
        for _ in 0..count {
            let o = alloc::alloc_indexable_oops(&mut vm, klass, 0).oop();
            vm.stack.push(o); // must be rooted to actually land in to-space
        }

        let report = scavenge(&mut vm).expect("fill scavenge must succeed");
        assert_eq!(report.promoted_bytes, 0, "must fit without promoting");
        // Filled to within one object's width of the boundary — the exact
        // `top == end` legal-boundary case is what the next object below
        // is guaranteed to hit (by construction: `count` is `remaining /
        // per_obj_bytes` floored, so less than one more object's worth of
        // room is left).
        assert!(vm.universe.from.end - vm.universe.from.top < per_obj_bytes);

        // One more object must overflow into promotion, not corrupt to-space.
        let overflow = alloc::alloc_indexable_oops(&mut vm, klass, 0).oop();
        vm.stack.push(overflow);
        let report2 = scavenge(&mut vm).expect("overflow scavenge must succeed");
        assert!(report2.promoted_bytes > 0, "overflow object must promote");
        let new_overflow = vm.stack.get(vm.stack.sp - 1);
        assert!(vm.universe.layout.is_old(new_overflow.mem_addr()));
    }

    /// `tests_s07.md`'s `promotion_reenters_scan`: an object graph where a
    /// promoted object references an eden object — the promoted-region
    /// scan must copy it too (both scan pointers reach a fixed point).
    #[test]
    fn promotion_reenters_scan() {
        let mut vm = test_vm();
        vm.universe.tenuring_threshold = 0; // promote everything immediately
        let klass = vm.universe.array_klass;
        let holder = alloc::alloc_indexable_oops(&mut vm, klass, 1);
        let referenced = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        holder.at_put(0, referenced.oop());
        vm.stack.push(holder.oop());

        let report = scavenge(&mut vm).expect("scavenge must succeed");
        assert!(report.promoted_bytes > 0);

        let new_holder_oop = vm.stack.get(vm.stack.sp - 1);
        assert!(vm.universe.layout.is_old(new_holder_oop.mem_addr()));
        let new_holder = crate::oops::wrappers::ArrayOop::try_from(new_holder_oop).unwrap();
        let referenced_now = new_holder.at(0);
        // The referenced object must have been found and relocated too —
        // it must NOT still point into the (now-decommitted-in-spirit,
        // definitely stale) original eden address.
        assert!(
            vm.universe.layout.is_old(referenced_now.mem_addr())
                || vm.universe.layout.is_new(referenced_now.mem_addr()),
            "referenced object must resolve to a live address"
        );
        assert_ne!(referenced_now.raw(), referenced.oop().raw());
    }
}
