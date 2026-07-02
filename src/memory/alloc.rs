//! The allocation choke point (SPEC §7.2, S1 subset: bump-allocate from
//! eden; abort the process — not a Rust panic, an environment limit — when
//! full). No code outside this module bumps `eden.top`.
//!
//! Two layers: the `_raw` primitives operate directly on an [`Eden`] plus an
//! explicit nil-fill [`Oop`], and are what [`crate::memory::universe`]'s
//! genesis calls (a full `VmState` does not exist yet while the `Universe`
//! it will contain is still being constructed). The public, `VmState`-based
//! functions below them are what every later sprint allocates through.

use crate::oops::layout::{HEADER_WORDS, KLASS_SIZE_WORDS, MEM_TAG, WORD_SIZE};
use crate::oops::mark::Mark;
use crate::oops::smi::SmallInt;
use crate::oops::wrappers::{ArrayOop, ByteArrayOop, DoubleOop, KlassOop, MemOop};
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;

use super::space::Eden;

// --- raw layer: usable before a VmState exists (genesis) -------------------

/// The core bump-allocation primitive. `nil_fill` is the value tagged
/// (oop-bearing) allocations fill their body with; genesis passes its
/// in-progress `nil_obj` (a placeholder for the very first call, the real
/// `nil_obj` from then on — SPEC-aligned with `sprint_s01_detail.md`'s
/// pitfalls). Exits the process (code 70) on exhaustion — S7 replaces this
/// branch with the scavenger.
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
    let size = words
        .checked_mul(WORD_SIZE)
        .expect("alloc_words: size overflow");
    let new_top = eden
        .top
        .checked_add(size)
        .expect("alloc_words: eden.top overflow");
    if new_top > eden.end {
        eprintln!("macvm: eden exhausted ({size} bytes requested)");
        std::process::exit(70);
    }
    let addr = eden.top;
    eden.top = new_top;

    // SAFETY: [addr, addr+size) was just carved out of committed,
    // otherwise-unused eden space, and is 8-byte aligned because `addr` is
    // eden.top, which starts aligned and only ever advances by whole words.
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

// --- public layer: VmState-based, used from S2 onward -----------------------

/// Every heap object allocates through here. SPEC §7.2.
pub fn alloc_words(vm: &mut VmState, words: usize, klass: Oop, tagged: bool) -> MemOop {
    let nil_fill = vm.universe.nil_obj;
    alloc_words_raw(&mut vm.universe.eden, nil_fill, words, klass, tagged)
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

pub fn alloc_indexable_bytes(vm: &mut VmState, klass: KlassOop, nbytes: usize) -> ByteArrayOop {
    let nis = klass.non_indexable_size();
    let nil_fill = vm.universe.nil_obj;
    alloc_indexable_bytes_raw(&mut vm.universe.eden, nil_fill, klass.oop(), nis, nbytes)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::vm_state::VmOptions;

    fn boot() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
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
        // usize::MAX bytes overflows `words * WORD_SIZE` in the checked
        // arithmetic (`alloc_words_raw`) — this must panic via `.expect`,
        // which fires in both debug and release (unlike a debug_assert),
        // NOT silently wrap into a small allocation. Deliberately NOT
        // usize::MAX/2: that magnitude doesn't actually overflow the
        // checked chain, so it would instead reach the real eden-exhaustion
        // branch and call `std::process::exit(70)` — fatal to the whole
        // test harness, not a panic `#[should_panic]` can observe.
        let mut vm = boot();
        let bytearray_klass = vm.universe.bytearray_klass;
        let _ = alloc_indexable_bytes(&mut vm, bytearray_klass, usize::MAX);
    }
}
