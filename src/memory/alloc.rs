//! The allocation choke point (SPEC §7.2, S1 subset: bump-allocate from
//! eden; abort the process — not a Rust panic, an environment limit — when
//! full). No code outside this module bumps `eden.top`.
//!
//! Two layers: the `_raw` primitives operate directly on an [`Eden`] plus an
//! explicit nil-fill [`Oop`], and are what [`crate::memory::universe`]'s
//! genesis calls (a full `VmState` does not exist yet while the `Universe`
//! it will contain is still being constructed). The public, `VmState`-based
//! functions below them are what every later sprint allocates through.

use crate::oops::layout::{
    HEADER_WORDS, KLASS_SIZE_WORDS, MEM_TAG, METHOD_COUNTERS_INDEX, METHOD_FLAGS_INDEX,
    METHOD_HOLDER_INDEX, METHOD_ICS_INDEX, METHOD_LITERALS_INDEX, METHOD_PRIMITIVE_INDEX,
    METHOD_SELECTOR_INDEX, METHOD_SIZE_INDEX, WORD_SIZE,
};
use crate::oops::mark::Mark;
use crate::oops::smi::SmallInt;
use crate::oops::wrappers::{
    ArrayOop, ByteArrayOop, ClosureOop, ContextOop, DoubleOop, KlassOop, MemOop, MethodOop,
};
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;

use super::spaces::Eden;

// --- raw layer: usable before a VmState exists (genesis) -------------------

/// Writes a fresh object's header (mark + klass) and nil/zero-fills its
/// body — the tail shared by every allocation path (eden bump, old-gen
/// direct) once an address has been carved out. Caller guarantees `[addr,
/// addr + words*8)` is freshly reserved, uninitialized, 8-byte-aligned
/// space this call owns exclusively.
fn init_object_at(addr: usize, words: usize, klass: Oop, tagged: bool, nil_fill: Oop) -> MemOop {
    // SAFETY: per this function's own doc contract, established by every
    // call site below.
    let obj = unsafe { MemOop::from_oop_unchecked(Oop::from_raw(addr as u64 + MEM_TAG)) };
    obj.set_mark(Mark::pristine().with_tagged_contents(tagged));
    obj.set_klass_raw(klass);

    let body_words = words - HEADER_WORDS;
    let fill_word = if tagged { nil_fill.raw() } else { 0 };
    for i in 0..body_words {
        obj.set_raw_body_word(i, fill_word);
    }
    obj
}

/// Bumps `eden.top` by `words*8` bytes if it fits; `None` (never exits)
/// otherwise — the fallible primitive both the genesis-only raw layer and
/// the public A2 cascade build on.
fn try_bump_eden(eden: &mut Eden, words: usize) -> Option<usize> {
    let size = words
        .checked_mul(WORD_SIZE)
        .expect("alloc_words: size overflow");
    let new_top = eden
        .top
        .checked_add(size)
        .expect("alloc_words: eden.top overflow");
    if new_top > eden.end {
        return None;
    }
    let addr = eden.top;
    eden.top = new_top;
    Some(addr)
}

/// The core bump-allocation primitive. `nil_fill` is the value tagged
/// (oop-bearing) allocations fill their body with; genesis passes its
/// in-progress `nil_obj` (a placeholder for the very first call, the real
/// `nil_obj` from then on — SPEC-aligned with `sprint_s01_detail.md`'s
/// pitfalls). Exits the process (code 70) on exhaustion: genesis runs with
/// GC disabled (SPEC §7.3 A1 — the half-built metaobject knot must never be
/// scanned), so there is no scavenger to fall back on here; the public
/// layer below (`alloc_words`) is what implements the real A2 cascade.
pub(crate) fn alloc_words_raw(
    eden: &mut Eden,
    nil_fill: Oop,
    words: usize,
    klass: Oop,
    tagged: bool,
) -> MemOop {
    debug_assert!(
        words >= HEADER_WORDS,
        "alloc_words: {words} words is smaller than a header"
    );
    match try_bump_eden(eden, words) {
        Some(addr) => init_object_at(addr, words, klass, tagged, nil_fill),
        None => {
            let size = words * WORD_SIZE;
            eprintln!("macvm: eden exhausted ({size} bytes requested)");
            std::process::exit(70);
        }
    }
}

/// A klass-shaped (10-word, tagged) object whose klass field is `meta`.
/// Does NOT set `format`/`non_indexable_size`/`superclass` — those default
/// to the nil fill (or 0, for the case they're read as smis before being
/// set) and callers (genesis, `alloc_klass`) must set them immediately.
pub(crate) fn alloc_klass_raw(eden: &mut Eden, nil_fill: Oop, meta: Oop) -> KlassOop {
    let obj = alloc_words_raw(eden, nil_fill, KLASS_SIZE_WORDS, meta, true);
    // SAFETY: freshly allocated with KLASS_SIZE_WORDS words; format is set
    // to Klass by the caller before this value's typed accessors are used.
    unsafe { KlassOop::from_oop_unchecked(obj.oop()) }
}

/// A `Slots`-shaped instance of `klass` (`klass.non_indexable_size()`
/// words). Used by genesis-adjacent tests (`Universe`'s own unit tests) that
/// have `&mut Universe` but no `VmState` to allocate through.
#[allow(dead_code)] // exercised by tests now; a genesis-adjacent counterpart to alloc_slots
pub(crate) fn alloc_slots_raw(eden: &mut Eden, nil_fill: Oop, klass: KlassOop) -> MemOop {
    let words = klass.non_indexable_size();
    alloc_words_raw(eden, nil_fill, words, klass.oop(), true)
}

/// An `IndexableBytes`-shaped object: `nis` (from `klass`'s
/// `non_indexable_size`, passed explicitly since genesis's Symbol klass may
/// itself still be under construction) named words, then `[size][bytes…]`,
/// zero-padded to a word boundary. The size slot holds the TRUE byte count;
/// padding bytes are zero forever (SPEC-pinned: never written after this
/// call, so word-wise hashing/compare may read whole words safely).
pub(crate) fn alloc_indexable_bytes_raw(
    eden: &mut Eden,
    nil_fill: Oop,
    klass: Oop,
    nis: usize,
    nbytes: usize,
) -> ByteArrayOop {
    let padded_words = nbytes.div_ceil(8);
    let words = nis
        .checked_add(1)
        .and_then(|w| w.checked_add(padded_words))
        .expect("alloc_indexable_bytes: size overflow");
    let obj = alloc_words_raw(eden, nil_fill, words, klass, false);
    let size_idx = nis - HEADER_WORDS;
    obj.set_raw_body_word(size_idx, SmallInt::new(nbytes as i64).oop().raw());
    // SAFETY: freshly allocated with klass's IndexableBytes shape.
    unsafe { ByteArrayOop::from_oop_unchecked(obj.oop()) }
}

/// A `GcStallError` from `scavenge()` (old gen's committed prefix full mid-
/// cycle, S7's designed cascade endpoint — growth is S8) can NEVER be
/// silently discarded and retried: by the time it's returned, the root
/// scan has already rewritten some roots (handles, well-known oops, ...)
/// in place to point at their `to`-space/promoted copies, but the from→to
/// swap that would make those addresses valid never ran. A caller that
/// ignores the `Err` and calls `scavenge()` again reuses that same `to`
/// region for a fresh copy pass, silently corrupting every root the first,
/// aborted pass had already rewritten (a live handle ends up pointing at
/// whatever the retry happened to copy to that address instead). So this
/// is always fatal, matching the pre-existing "old gen truly out of room"
/// exit path one step down in the same cascade.
///
/// `pub(crate)`, not private: `runtime::primitives`' `gcScavenge` calls a
/// scavenge directly (not through the allocation cascade), so it needs the
/// exact same terminal handling on a stall rather than a second copy.
pub(crate) fn stall_exit(err: crate::memory::stall::GcStallError) -> ! {
    eprintln!("{err}");
    std::process::exit(70);
}

// --- public layer: VmState-based, used from S2 onward -----------------------

/// SPEC §7.2 step 4's promotion guarantee (S8 step 7), checked before EVERY
/// scavenge call in the cascade below (including the `MACVM_GC_STRESS=1`
/// one, which under stress mode runs on literally every allocation): commit
/// enough old-gen room to promote the ENTIRE young generation in the worst
/// case (every eden+from byte ages past threshold, or overflows survivor
/// space) before the scavenge is allowed to start. This is what makes a
/// scavenge's own `ScavengePromote` stall a guarantee VIOLATION rather than
/// a legitimate outcome — but only when this actually succeeds
/// (`vm.universe.promotion_guarantee_met`, read by `scavenge::record_stall`):
/// with a bounded reservation, the guarantee can genuinely be unmeetable
/// (`eden_exhaustion_aborts` deliberately drives a tiny heap into exactly
/// this), and that is real, terminal memory exhaustion, not a bug — so it
/// must still fall through to the ordinary `GcStallError` path, not a
/// hard-coded "impossible". A full GC is tried first — compaction alone may
/// free enough room without growing at all; growth is the fallback.
fn ensure_promotion_guarantee(vm: &mut VmState) {
    let worst_case = (vm.universe.eden.top - vm.universe.eden.start)
        + (vm.universe.from.top - vm.universe.from.start);
    if vm.universe.old.free_bytes() < worst_case {
        crate::memory::fullgc::full_gc(vm)
            .expect("full_gc: pure compaction never returns Err (see its own doc)");
        while vm.universe.old.free_bytes() < worst_case {
            if vm
                .universe
                .grow_old(crate::memory::layout::OLD_GROWTH_SEGMENT)
                == 0
            {
                // Reservation exhausted: the guarantee genuinely cannot be
                // met. Let the scavenge run anyway and stall for real if it
                // actually needs the room — recorded below so record_stall
                // can tell this apart from a guarantee VIOLATION.
                break;
            }
        }
    }
    vm.universe.promotion_guarantee_met = vm.universe.old.free_bytes() >= worst_case;
}

/// Every heap object allocates through here (SPEC §7.2 A2's designed
/// cascade, complete as of S8 step 7): (1) `MACVM_GC_STRESS=1` scavenges
/// before every allocation once the universe is past genesis, guarantee-
/// checked first; (2) bump eden; (3) on failure, guarantee-check, scavenge,
/// retry the bump; (4) on failure, allocate directly into old gen (also
/// covers objects too big for eden to ever hold), growing one segment and
/// retrying once if the committed prefix is full; (5) still nothing → a
/// full GC (may free enough room via compaction alone), grow old gen if
/// that still isn't enough, retry both eden and old; (6) still nothing → a
/// structured `GcStallError` report, then exit non-zero (lesson 7: never
/// panic on OOM) — the designed end of the cascade, not a bug.
///
/// `klass` is handle-protected for the whole call: once this function can
/// itself trigger a scavenge, its own `klass` parameter is exactly the kind
/// of bare-local invisible root S7-9 exists to close (a caller like
/// `alloc_slots` passes a `KlassOop` that may still be new-gen and
/// unrelated to any other root at the moment of this call).
pub fn alloc_words(vm: &mut VmState, words: usize, klass: Oop, tagged: bool) -> MemOop {
    debug_assert!(
        words >= HEADER_WORDS,
        "alloc_words: {words} words is smaller than a header"
    );
    let size_bytes = words
        .checked_mul(WORD_SIZE)
        .expect("alloc_words: size overflow");

    let scope = crate::memory::handles::HandleScope::enter(vm);
    let klass_h = scope.handle(vm, klass);

    // S12 step 7: the S11 D8 bridge's diversion arm (`if compiled_depth >
    // 0 { allocate old-direct, never touch eden/GC }`) lived exactly here
    // and is DELETED, not relocated. A live compiled frame's own oops are
    // now first-class GC roots — spill slots via `Nmethod::oopmap_at`,
    // stub RootSpill slots per-kind, all through `roots::each_code_root`
    // (steps 1-5) — and the eden bump pointer is one shared word compiled
    // code reads through `reg_block.eden_top_addr` (never a stale copy),
    // so the full cascade below is safe at ANY compiled depth, exactly
    // like at depth 0. `compiled_depth` itself stays: `gc_under_compiled`
    // (both collectors) counts the hard case actually running, and
    // `rt_alloc_slow` still asserts it's positive on entry.

    if vm.options.gc_stress && vm.universe.gc_enabled {
        ensure_promotion_guarantee(vm);
        if let Err(err) = crate::memory::scavenge::scavenge(vm) {
            stall_exit(err);
        }
    }

    // `MACVM_GC_STRESS=full[:N]` (S8 step 8): mutually exclusive with the
    // `=1` scavenge-stress hook above (env parsing only ever sets one of
    // `gc_stress`/`gc_stress_full_period`) — a full GC every Nth allocation
    // through this same choke point, the harness that exercises phase C's
    // reference-rewrite path (and found the `for_each_root` bug fixed in
    // this same sprint pass: the scavenger-only `=1` mode never calls
    // `full_gc` at all, so it could never have caught it).
    if let Some(period) = vm.options.gc_stress_full_period {
        if vm.universe.gc_enabled {
            vm.universe.full_stress_alloc_count += 1;
            if vm.universe.full_stress_alloc_count >= period {
                vm.universe.full_stress_alloc_count = 0;
                crate::memory::fullgc::full_gc(vm)
                    .expect("full_gc: pure compaction never returns Err (see its own doc)");
            }
        }
    }

    let nil_fill = vm.universe.nil_obj;
    if let Some(addr) = try_bump_eden(&mut vm.universe.eden, words) {
        return init_object_at(addr, words, klass_h.get(vm), tagged, nil_fill);
    }

    if vm.universe.gc_enabled {
        ensure_promotion_guarantee(vm);
        if let Err(err) = crate::memory::scavenge::scavenge(vm) {
            stall_exit(err);
        }
        let nil_fill = vm.universe.nil_obj;
        if let Some(addr) = try_bump_eden(&mut vm.universe.eden, words) {
            return init_object_at(addr, words, klass_h.get(vm), tagged, nil_fill);
        }
    }

    // Direct old-gen allocation for objects eden can't hold (or that
    // survived both scavenges). On a committed-prefix miss, grow one
    // segment and retry once.
    if let Some(obj) = try_old_alloc(vm, words, size_bytes, klass_h.get(vm), tagged) {
        return obj;
    }
    if vm
        .universe
        .grow_old(crate::memory::layout::OLD_GROWTH_SEGMENT)
        > 0
    {
        if let Some(obj) = try_old_alloc(vm, words, size_bytes, klass_h.get(vm), tagged) {
            return obj;
        }
    }

    // Full GC: the last resort before stalling. Compaction alone may free
    // enough contiguous room in either generation without growing at all;
    // if not, one more growth segment closes the gap. Eden is retried
    // first (the common case — most allocations are eden-sized), then old.
    crate::memory::fullgc::full_gc(vm)
        .expect("full_gc: pure compaction never returns Err (see its own doc)");
    let nil_fill = vm.universe.nil_obj;
    if let Some(addr) = try_bump_eden(&mut vm.universe.eden, words) {
        return init_object_at(addr, words, klass_h.get(vm), tagged, nil_fill);
    }
    if let Some(obj) = try_old_alloc(vm, words, size_bytes, klass_h.get(vm), tagged) {
        return obj;
    }
    if vm
        .universe
        .grow_old(crate::memory::layout::OLD_GROWTH_SEGMENT)
        > 0
    {
        if let Some(obj) = try_old_alloc(vm, words, size_bytes, klass_h.get(vm), tagged) {
            return obj;
        }
    }

    let err = crate::memory::stall::GcStallError::snapshot(
        &vm.universe,
        size_bytes,
        crate::memory::stall::GcPhase::Mutator,
    );
    stall_exit(err);
}

/// One direct old-gen allocation attempt: bump the committed prefix, init the
/// header/body, and dirty the object's card range. `None` iff the committed
/// prefix is full (caller decides grow/stall). Born-in-old objects get their
/// whole range dirtied unconditionally (mirrors A4 step 8's promotion policy):
/// the klass field, and a body the caller fills right after, may point into
/// new gen; the next dirty-card scan clears cards that turn out clean (A6).
fn try_old_alloc(
    vm: &mut VmState,
    words: usize,
    size_bytes: usize,
    klass: Oop,
    tagged: bool,
) -> Option<MemOop> {
    let nil_fill = vm.universe.nil_obj;
    let addr = {
        let offsets = &mut vm.universe.offsets;
        let old = &mut vm.universe.old;
        old.allocate(offsets, size_bytes, |_| {})?
    };
    let obj = init_object_at(addr, words, klass, tagged, nil_fill);
    vm.universe
        .cards
        .record_multistores(addr, addr + size_bytes);
    Some(obj)
}

pub fn alloc_slots(vm: &mut VmState, klass: KlassOop) -> MemOop {
    let words = klass.non_indexable_size();
    alloc_words(vm, words, klass.oop(), true)
}

pub fn alloc_indexable_oops(vm: &mut VmState, klass: KlassOop, n: usize) -> ArrayOop {
    let nis = klass.non_indexable_size();
    let words = nis
        .checked_add(1)
        .and_then(|w| w.checked_add(n))
        .expect("alloc_indexable_oops: size overflow");
    let obj = alloc_words(vm, words, klass.oop(), true);
    let size_idx = nis - HEADER_WORDS;
    obj.set_raw_body_word(size_idx, SmallInt::new(n as i64).oop().raw());
    // SAFETY: freshly allocated with klass's IndexableOops shape.
    unsafe { ArrayOop::from_oop_unchecked(obj.oop()) }
}

/// SPEC §7.2 A2's cascade, not `alloc_indexable_bytes_raw`'s bare eden bump
/// (found via a real crash, not inspection: a ByteArray/String request that
/// merely didn't fit eden's CURRENT free space was killing the whole
/// process with "eden exhausted" instead of falling back to scavenge/
/// old-gen/growth like every other indexable allocator — `_raw` is a
/// genesis-adjacent helper, `non_indexable_size` may still be under
/// construction there, per its own doc comment; this, the general-purpose
/// public entry point, must go through the same `alloc_words` cascade
/// `alloc_indexable_oops` does).
pub fn alloc_indexable_bytes(vm: &mut VmState, klass: KlassOop, nbytes: usize) -> ByteArrayOop {
    let nis = klass.non_indexable_size();
    let padded_words = nbytes.div_ceil(8);
    let words = nis
        .checked_add(1)
        .and_then(|w| w.checked_add(padded_words))
        .expect("alloc_indexable_bytes: size overflow");
    let obj = alloc_words(vm, words, klass.oop(), false);
    let size_idx = nis - HEADER_WORDS;
    obj.set_raw_body_word(size_idx, SmallInt::new(nbytes as i64).oop().raw());
    // SAFETY: freshly allocated with klass's IndexableBytes shape.
    unsafe { ByteArrayOop::from_oop_unchecked(obj.oop()) }
}

/// A `BlockClosure` with `ncopied` captures, all initially `nil` (SPEC
/// §2.3, S4). `method`/`home` are left `nil` too — the caller (`push_closure`)
/// sets them immediately after, before any further allocation, per the S7
/// choke-point pattern.
pub fn alloc_closure(vm: &mut VmState, ncopied: usize) -> ClosureOop {
    let klass = vm.universe.closure_klass;
    let nis = klass.non_indexable_size();
    let words = nis
        .checked_add(1)
        .and_then(|w| w.checked_add(ncopied))
        .expect("alloc_closure: size overflow");
    let obj = alloc_words(vm, words, klass.oop(), true);
    let size_idx = nis - HEADER_WORDS;
    obj.set_raw_body_word(size_idx, SmallInt::new(ncopied as i64).oop().raw());
    // SAFETY: freshly allocated with klass's Closure shape.
    unsafe { ClosureOop::from_oop_unchecked(obj.oop()) }
}

/// A `Context` with `nctx` slots, all initially `nil`, and `home_hint` nil
/// (SPEC §2.3, §5.4, S4) — the caller sets `home_hint` explicitly when the
/// enclosing chain is non-empty.
pub fn alloc_context(vm: &mut VmState, nctx: usize) -> ContextOop {
    let klass = vm.universe.context_klass;
    let nis = klass.non_indexable_size();
    let words = nis
        .checked_add(1)
        .and_then(|w| w.checked_add(nctx))
        .expect("alloc_context: size overflow");
    let obj = alloc_words(vm, words, klass.oop(), true);
    let size_idx = nis - HEADER_WORDS;
    obj.set_raw_body_word(size_idx, SmallInt::new(nctx as i64).oop().raw());
    // SAFETY: freshly allocated with klass's Context shape.
    unsafe { ContextOop::from_oop_unchecked(obj.oop()) }
}

pub fn alloc_double(vm: &mut VmState, v: f64) -> DoubleOop {
    let klass = vm.universe.double_klass;
    let words = klass.non_indexable_size();
    let obj = alloc_words(vm, words, klass.oop(), false);
    obj.set_raw_body_word(0, v.to_bits());
    // SAFETY: freshly allocated with format Double.
    unsafe { DoubleOop::from_oop_unchecked(obj.oop()) }
}

/// A klass-shaped object whose klass field is `meta`. As with the raw
/// variant, `format`/`non_indexable_size`/`superclass` must be set by the
/// caller immediately after (genesis + S5's `subclass:`).
pub fn alloc_klass(vm: &mut VmState, meta: Oop) -> KlassOop {
    let nil_fill = vm.universe.nil_obj;
    alloc_klass_raw(&mut vm.universe.eden, nil_fill, meta)
}

/// A `CompiledMethod` with `nbytes` of bytecode (SPEC §4.4): 9 named-part
/// words (7 fields + the size slot) then `ceil(nbytes/8)` byte words.
/// `tagged_contents = false` (Method objects have a byte tail — S7's
/// scavenger must scan only the 7 named oop slots, not the whole body), so
/// this is deliberately NOT built via `alloc_indexable_bytes`: that
/// zero-fills the entire body, which would leave `selector`/`holder`/
/// `literals`/`ics` holding smi 0 instead of nil. Named oop fields are
/// nil-filled explicitly; `flags`/`primitive`/`counters` are smi 0 either
/// way (raw zero bits already encode smi 0, SPEC §2.1).
pub fn alloc_method(vm: &mut VmState, nbytes: usize) -> MethodOop {
    let klass = vm.universe.method_klass;
    let nis = klass.non_indexable_size();
    let padded_words = nbytes.div_ceil(8);
    let words = nis
        .checked_add(1)
        .and_then(|w| w.checked_add(padded_words))
        .expect("alloc_method: size overflow");

    let obj = alloc_words(vm, words, klass.oop(), false);
    let nil = vm.universe.nil_obj;
    let zero_smi = SmallInt::new(0).oop().raw();
    obj.set_raw_body_word(METHOD_SELECTOR_INDEX, nil.raw());
    obj.set_raw_body_word(METHOD_HOLDER_INDEX, nil.raw());
    obj.set_raw_body_word(METHOD_FLAGS_INDEX, zero_smi);
    obj.set_raw_body_word(METHOD_PRIMITIVE_INDEX, zero_smi);
    obj.set_raw_body_word(METHOD_COUNTERS_INDEX, zero_smi);
    obj.set_raw_body_word(METHOD_LITERALS_INDEX, nil.raw());
    obj.set_raw_body_word(METHOD_ICS_INDEX, nil.raw());
    obj.set_raw_body_word(METHOD_SIZE_INDEX, SmallInt::new(nbytes as i64).oop().raw());
    // SAFETY: freshly allocated with klass's Method shape.
    unsafe { MethodOop::from_oop_unchecked(obj.oop()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::vm_state::VmOptions;

    fn boot() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            gc_stress_full_period: None,
            eden_kb: None,
            jit: crate::runtime::JitMode::Off,
        })
    }

    #[test]
    fn bump_alloc_sequential() {
        let mut vm = boot();
        let klass = vm.universe.object_klass.oop();
        let top_before = vm.universe.eden.top;
        let a = alloc_words(&mut vm, 4, klass, true);
        let b = alloc_words(&mut vm, 4, klass, true);

        assert_eq!(b.addr() - a.addr(), 32);
        assert_eq!(a.addr() % 8, 0);
        assert_eq!(b.addr() % 8, 0);
        assert_eq!(vm.universe.eden.top - top_before, 64);
    }

    /// S12 step 7 (inverts S11's `bridge_diverts_to_old_under_compiled_
    /// frame`): with the D8 bridge deleted, `compiled_depth > 0` changes
    /// NOTHING about where an ordinary allocation lands — eden, exactly
    /// like depth 0. `compiled_depth = 1` with an EMPTY native stack is
    /// honest here precisely because nothing on this path consults
    /// anything BUT the counter anymore (no walker runs — no GC is
    /// triggered by a plain in-capacity bump).
    #[test]
    fn alloc_lands_in_eden_under_compiled_depth() {
        let mut vm = boot();
        let klass = vm.universe.object_klass.oop();
        vm.compiled_depth = 1;
        let eden_top_before = vm.universe.eden.top;

        let obj = alloc_words(&mut vm, 4, klass, true);

        assert_eq!(
            vm.universe.eden.top,
            eden_top_before + 32,
            "an in-capacity allocation must bump eden regardless of compiled_depth"
        );
        assert!(
            obj.addr() >= vm.universe.eden.start && obj.addr() < vm.universe.eden.end,
            "the object must live IN eden -- the old-direct diversion is gone"
        );
        assert_eq!(obj.klass_oop(), vm.universe.object_klass.oop());
        assert!(obj.mark().tagged_contents());

        vm.compiled_depth = 0;
    }

    #[test]
    fn alloc_writes_header() {
        let mut vm = boot();
        let klass = vm.universe.object_klass;
        let obj = alloc_words(&mut vm, HEADER_WORDS + 1, klass.oop(), true);

        let mark = obj.mark();
        assert_eq!(mark.word() & 0b11, 0b11); // MARK_TAG
        assert_ne!(mark.word() & 0b100, 0); // sentinel
        assert_eq!(mark.hash(), 0);
        assert_eq!(mark.age(), 0);
        assert_eq!(obj.klass_oop(), klass.oop());
    }

    #[test]
    fn alloc_tagged_contents_bit() {
        let mut vm = boot();
        let association_klass = vm.universe.association_klass;
        let assoc = alloc_slots(&mut vm, association_klass);
        assert!(assoc.mark().tagged_contents());

        let bytearray_klass = vm.universe.bytearray_klass;
        let bytes = alloc_indexable_bytes(&mut vm, bytearray_klass, 4);
        assert!(!bytes.as_mem().mark().tagged_contents());

        let d = alloc_double(&mut vm, 1.0);
        assert!(!d.as_mem().mark().tagged_contents());
    }

    #[test]
    fn alloc_nil_fill() {
        let mut vm = boot();
        let nil = vm.universe.nil_obj;
        let association_klass = vm.universe.association_klass;
        let assoc = alloc_slots(&mut vm, association_klass);
        assert_eq!(assoc.body_oop(0), nil);
        assert_eq!(assoc.body_oop(1), nil);
    }

    #[test]
    fn slots_size_math() {
        let mut vm = boot();
        let klass = vm.universe.association_klass;
        let a = alloc_slots(&mut vm, klass);
        let b = alloc_slots(&mut vm, klass);
        // Association's non_indexable_size is HEADER_WORDS + 2 = 4 words.
        assert_eq!(b.addr() - a.addr(), 4 * WORD_SIZE);
    }

    #[test]
    fn indexable_oops_size_math() {
        let mut vm = boot();
        let klass = vm.universe.array_klass;
        let a = alloc_indexable_oops(&mut vm, klass, 3);
        let next_top = vm.universe.eden.top;
        // nis(2) + 1(size slot) + 3(elems) = 6 words.
        assert_eq!(next_top - a.addr(), 6 * WORD_SIZE);
        assert_eq!(a.len(), 3);
        assert_eq!(a.as_mem().raw_body_word(0), SmallInt::new(3).oop().raw());
    }

    #[test]
    fn indexable_bytes_padding() {
        let mut vm = boot();
        let klass = vm.universe.bytearray_klass;
        for n in [0usize, 1, 7, 8, 9, 16] {
            let start_top = vm.universe.eden.top;
            let b = alloc_indexable_bytes(&mut vm, klass, n);
            let consumed = (vm.universe.eden.top - start_top) / WORD_SIZE;
            let expected = HEADER_WORDS + 1 + n.div_ceil(8);
            assert_eq!(consumed, expected, "n={n}");
            assert_eq!(b.len(), n, "n={n}");
            for i in 0..n {
                assert_eq!(b.byte_at(i), 0, "padding not zero at n={n} i={i}");
            }
        }
    }

    #[test]
    fn double_roundtrip() {
        let mut vm = boot();
        let start_top = vm.universe.eden.top;
        let d = alloc_double(&mut vm, 3.5);
        assert_eq!(d.value(), 3.5);
        assert_eq!((vm.universe.eden.top - start_top) / WORD_SIZE, 3);

        let nan = alloc_double(&mut vm, f64::NAN);
        assert!(nan.value().is_nan());
    }

    #[test]
    fn zero_length_indexables() {
        let mut vm = boot();
        let array_klass = vm.universe.array_klass;
        let a = alloc_indexable_oops(&mut vm, array_klass, 0);
        assert_eq!(a.len(), 0);
        let bytearray_klass = vm.universe.bytearray_klass;
        let b = alloc_indexable_bytes(&mut vm, bytearray_klass, 0);
        assert_eq!(b.len(), 0);
        super::super::verify::verify_heap(&vm.universe)
            .expect("must verify with zero-length tails");
    }

    #[test]
    #[should_panic(expected = "size overflow")]
    fn oversized_alloc_request_panics() {
        // usize::MAX bytes overflows `words * WORD_SIZE` in `alloc_words`'s
        // own checked arithmetic — this must panic via `.expect`, which
        // fires in both debug and release (unlike a debug_assert), NOT
        // silently wrap into a small allocation. Deliberately NOT
        // usize::MAX/2: that magnitude doesn't actually overflow the
        // checked chain, so it would instead reach the real cascade's
        // terminal stall and call `std::process::exit(70)` — fatal to the
        // whole test harness, not a panic `#[should_panic]` can observe.
        let mut vm = boot();
        let bytearray_klass = vm.universe.bytearray_klass;
        let _ = alloc_indexable_bytes(&mut vm, bytearray_klass, usize::MAX);
    }

    /// S8 step 7's actual point: not just that `full_gc` works in isolation
    /// (`fullgc.rs`'s own 14 tests already prove that), but that the
    /// allocation cascade genuinely REACHES it. With the default 8 GiB heap
    /// and small test workloads, `ensure_promotion_guarantee`'s `full_gc()`
    /// branch would never fire in practice — old gen's free room always
    /// dwarfs any test's worst-case young-gen usage, so every other test in
    /// this suite passing proves nothing about this wiring specifically.
    /// This test forces the actual trigger: an eden bigger than old gen's
    /// fixed 16 MiB initial segment, filled close to capacity, so the next
    /// allocation's promotion-guarantee check genuinely finds old gen too
    /// small for the worst case and must call `full_gc` for real.
    #[test]
    fn cascade_actually_invokes_full_gc_when_promotion_guarantee_is_short() {
        let mut vm = VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            gc_stress_full_period: None,
            eden_kb: Some(20 * 1024), // > OLD_INITIAL_SEGMENT's 16 MiB
            jit: crate::runtime::JitMode::Off,
        });
        assert!(
            vm.universe.old.free_bytes() < vm.universe.eden.end - vm.universe.eden.start,
            "test setup: eden's capacity must exceed old gen's free room, or the \
             promotion guarantee below never actually gets exercised"
        );
        let klass = vm.universe.array_klass;
        // 60-element arrays via the real allocator (not a raw alloc_words
        // call with an IndexableOops-format klass, which would skip the
        // size-slot write `instance_size_words`/`for_each_oop_field` both
        // depend on to size the object correctly during the coming GC).
        const CHUNK_ELEMS: usize = 60;
        let chunk_bytes = (klass.non_indexable_size() + 1 + CHUNK_ELEMS) * WORD_SIZE;
        // Precisely drain eden to under one more chunk's worth: the loop
        // only fires while a full chunk still fits, so afterward
        // `remaining() < chunk_bytes` is guaranteed — the trigger
        // allocation below (the SAME size) is then certain to miss its
        // first bump attempt, not just likely to.
        while vm.universe.eden.remaining() >= chunk_bytes {
            alloc_indexable_oops(&mut vm, klass, CHUNK_ELEMS);
        }
        assert!(
            vm.universe.eden.remaining() < chunk_bytes,
            "test setup: eden must be drained below one more chunk"
        );
        let full_gc_count_before = vm.universe.gc_stats.full_gc_count;

        // Eden's first bump attempt now certainly fails; this falls into
        // the "on failure" branch, which calls ensure_promotion_guarantee
        // before scavenging.
        alloc_indexable_oops(&mut vm, klass, CHUNK_ELEMS);

        assert!(
            vm.universe.gc_stats.full_gc_count > full_gc_count_before,
            "ensure_promotion_guarantee must have actually invoked full_gc: old \
             gen's free room ({} bytes) was smaller than eden's worst case ({} bytes)",
            vm.universe.old.free_bytes(),
            vm.universe.eden.end - vm.universe.eden.start,
        );
    }
}
