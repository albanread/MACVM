//! Raw heap access — the ONLY module in the crate that dereferences heap
//! addresses. Everything above this layer (typed wrappers, `memory::`,
//! `runtime::`) reaches memory only through the methods here, all of which
//! take/return **values** (`Oop`, `u64`, …) — never a reference into the
//! heap. That discipline is deliberate and permanent (SPEC §7.6): when S7's
//! scavenger starts moving objects, the only retrofit needed is `Handle`s
//! around oop-holding *locals* across allocation, not a rewrite of every
//! accessor signature.
//!
//! Field offsets are pre-biased by the mem tag exactly once, in
//! [`MemOop::addr`] (S0). Every other offset in this module is a plain,
//! untagged byte or word offset from that address — the bias must never be
//! applied a second time, or S10's compiled loads will double-bias.

use super::layout::{BODY_OFFSET, HEADER_WORDS, KLASS_OFFSET, MARK_OFFSET, WORD_SIZE};
use super::mark::Mark;
use super::smi::SmallInt;
use super::wrappers::{KlassOop, MemOop};
use super::{klass::Format, Oop};

impl MemOop {
    fn word_ptr(self, byte_offset: usize) -> *mut u64 {
        (self.addr() + byte_offset) as *mut u64
    }

    fn body_ptr(self, index: usize) -> *mut u64 {
        self.word_ptr(BODY_OFFSET + WORD_SIZE * index)
    }

    // --- header -------------------------------------------------------

    pub fn mark(self) -> Mark {
        // SAFETY: every allocated object has a valid 2-word header at
        // [addr, addr+16).
        Mark::from_word(unsafe { self.word_ptr(MARK_OFFSET).read() })
    }

    pub fn set_mark(self, m: Mark) {
        // SAFETY: as above.
        unsafe { self.word_ptr(MARK_OFFSET).write(m.word()) }
    }

    /// The raw header word, with NO validation that it's actually a mark
    /// (SPEC §2.2 / `sprint_s07_detail.md` §Design: once a scavenge
    /// installs forwarding, the header word is a tagged oop, not a mark —
    /// `mark()`/`Mark::from_word` would panic on it). Use this, plus
    /// [`is_forwarded`](MemOop::is_forwarded)/[`forwardee`](MemOop::forwardee),
    /// whenever the object might already be forwarded.
    pub fn mark_word_raw(self) -> u64 {
        // SAFETY: as `mark()` — every allocated object has a header.
        unsafe { self.word_ptr(MARK_OFFSET).read() }
    }

    /// `true` iff this object's header has already been overwritten with a
    /// forwarding pointer by a scavenge in progress.
    pub fn is_forwarded(self) -> bool {
        super::mark::word_is_forwarded(self.mark_word_raw())
    }

    /// The forwardee oop. Caller must ensure `is_forwarded()`.
    pub fn forwardee(self) -> Oop {
        super::mark::forwardee(self.mark_word_raw())
    }

    /// Install forwarding: overwrite this (from-space) copy's header with
    /// `target`'s oop (SPEC §2.2's tag-01 discrimination). The body is left
    /// untouched — callers that still need to read it (e.g. sizing a
    /// not-yet-scanned object during the Cheney loop) must do so before
    /// calling this, or via the target copy instead.
    pub fn install_forwarding(self, target: Oop) {
        // SAFETY: as `set_mark` — overwrites the same header word with a
        // different tag; the slot itself always exists.
        unsafe { self.word_ptr(MARK_OFFSET).write(target.raw()) }
    }

    /// The raw klass field, without validating it holds a mem oop. Use this
    /// (never [`MemOop::klass`]) on objects whose klass field may still be
    /// a genesis placeholder — it never panics and never performs a wild
    /// read beyond the header.
    pub fn klass_oop(self) -> Oop {
        // SAFETY: header word 1 always exists.
        Oop::from_raw(unsafe { self.word_ptr(KLASS_OFFSET).read() })
    }

    /// Write an arbitrary oop (including a genesis placeholder smi) into the
    /// klass field. Prefer [`MemOop::set_klass`] once a real `KlassOop`
    /// exists; this is the escape hatch genesis and the fresh-header write
    /// in `alloc.rs` use before the klass knot is closed.
    pub fn set_klass_raw(self, o: Oop) {
        // SAFETY: header word 1 always exists.
        unsafe { self.word_ptr(KLASS_OFFSET).write(o.raw()) }
    }

    /// The object's klass. Panics if the klass field does not hold a mem
    /// oop (a genesis placeholder or corruption) — every object reachable
    /// outside genesis is expected to have a valid klass; callers that must
    /// tolerate a placeholder use [`MemOop::klass_oop`] directly.
    pub fn klass(self) -> KlassOop {
        KlassOop::try_from(self.klass_oop()).expect("klass field does not hold a mem oop")
    }

    pub fn set_klass(self, k: KlassOop) {
        self.set_klass_raw(k.oop());
    }

    // --- body (crate-internal, unchecked — see instance_size_words) ---

    /// Read body word `index` with no bounds check of its own — every
    /// caller already guarantees the word exists before reaching here:
    /// [`KlassOop`]'s statically-bounded accessors (`oops::klass`), this
    /// module's own `instance_size_words` size-slot read, and `body_oop`/
    /// `set_body_oop` below (their own `index < body_word_count()` check
    /// runs first). A fixed sanity ceiling used to live here too, but S6's
    /// Array/OrderedCollection/Dictionary workloads routinely exceed a few
    /// thousand elements — a ceiling low enough to catch a wild index would
    /// also reject legitimate large bodies, so bounds-checking is left
    /// entirely to each caller instead.
    pub(crate) fn raw_body_word(self, index: usize) -> u64 {
        // SAFETY: caller guarantees the word exists (see above).
        unsafe { self.body_ptr(index).read() }
    }

    pub(crate) fn set_raw_body_word(self, index: usize, w: u64) {
        // SAFETY: as above.
        unsafe { self.body_ptr(index).write(w) }
    }

    /// The size slot value (element count / byte count / ncopied / nslots)
    /// of an indexable-shaped object, given its klass's `non_indexable_size`.
    /// The slot itself is always within the guaranteed-allocated minimum
    /// (`nis + 1` words), so this bypasses the dynamic bounds check that
    /// would otherwise need the very value being computed. `pub(crate)`:
    /// `memory::scavenge`'s `object_size_for_copy` (SPEC §7.3 A5) needs
    /// this directly — it computes size using a caller-resolved klass
    /// (chasing that klass's OWN forwarding first) rather than
    /// `instance_size_words()`'s `self.klass()`, which would try to
    /// re-derive the klass from a field that might itself be forwarded.
    pub(crate) fn raw_size_slot(self, nis: usize) -> usize {
        let idx = nis - HEADER_WORDS;
        let raw = self.raw_body_word(idx);
        SmallInt::try_from(Oop::from_raw(raw))
            .expect("size slot is not a smi")
            .value() as usize
    }

    /// Total instance size in words, including the header. Pinned per
    /// format in `sprint_s01_detail.md` §Design (SPEC §2.3 states the rule
    /// only for `Slots`).
    pub fn instance_size_words(self) -> usize {
        let k = self.klass();
        let nis = k.non_indexable_size();
        match k.format() {
            Format::Slots | Format::Klass | Format::Double | Format::Process => nis,
            Format::IndexableOops => nis + 1 + self.raw_size_slot(nis),
            Format::IndexableBytes | Format::Method => {
                let nbytes = self.raw_size_slot(nis);
                nis + 1 + nbytes.div_ceil(8)
            }
            Format::Closure => nis + 1 + self.raw_size_slot(nis),
            Format::Context => nis + 1 + self.raw_size_slot(nis),
        }
    }

    fn body_word_count(self) -> usize {
        self.instance_size_words() - HEADER_WORDS
    }

    // --- body (general, dynamically bounds-checked) --------------------

    /// The untagged address of body slot `index` — `memory::store`'s only
    /// use for this (SPEC §7.4): it needs the slot's own address to
    /// compute which card to dirty, not just to write through it.
    pub fn body_addr(self, index: usize) -> usize {
        // The bound feeds ONLY the debug assert, but its loads (klass →
        // format → size slot) survive release builds unless the whole
        // computation is compiled out with the assert: LLVM must not
        // eliminate raw-pointer loads it can't prove safe. Profiled at a
        // combined ~57% of interpreter time (sample, richards, JIT=off).
        #[cfg(debug_assertions)]
        {
            let bound = self.body_word_count();
            debug_assert!(            index < bound,
            "body_addr: index {index} out of bounds ({bound})"
        );
        }
        self.body_ptr(index) as usize
    }

    pub fn body_oop(self, index: usize) -> Oop {
        // The bound feeds ONLY the debug assert, but its loads (klass →
        // format → size slot) survive release builds unless the whole
        // computation is compiled out with the assert: LLVM must not
        // eliminate raw-pointer loads it can't prove safe. Profiled at a
        // combined ~57% of interpreter time (sample, richards, JIT=off).
        #[cfg(debug_assertions)]
        {
            let bound = self.body_word_count();
            debug_assert!(            index < bound,
            "body_oop: index {index} out of bounds ({bound})"
        );
        }
        Oop::from_raw(self.raw_body_word(index))
    }

    pub fn set_body_oop(self, index: usize, v: Oop) {
        // The bound feeds ONLY the debug assert, but its loads (klass →
        // format → size slot) survive release builds unless the whole
        // computation is compiled out with the assert: LLVM must not
        // eliminate raw-pointer loads it can't prove safe. Profiled at a
        // combined ~57% of interpreter time (sample, richards, JIT=off).
        #[cfg(debug_assertions)]
        {
            let bound = self.body_word_count();
            debug_assert!(            index < bound,
            "set_body_oop: index {index} out of bounds ({bound})"
        );
        }
        self.set_raw_body_word(index, v.raw());
    }

    /// Visits every oop-bearing BODY field (SPEC §7.3 A5's format table),
    /// replacing each in place with `f`'s result. The klass field itself is
    /// the caller's own responsibility (same convention as every caller
    /// below) — this walks only the named/indexable body.
    ///
    /// `klass` is a parameter, not derived via `self.klass()`, because the
    /// collectors sharing this walk (`memory::scavenge`'s copying scavenger,
    /// the coming full mark-slide-compact GC) read it at different times
    /// relative to when THIS object's own klass field gets rewritten: the
    /// scavenger eagerly copies each object (klass field included) before
    /// scanning its body, so `self.klass()` is already the safe, freshly-
    /// copied value by then; a full GC's reference-rewrite phase must
    /// capture the klass BEFORE overwriting the klass field — nothing has
    /// been copied yet at that point, so the OLD klass body is what's still
    /// valid to read format from (SPEC §7.5 phase C, invariant F3).
    ///
    /// Deliberately uses [`raw_body_word`](MemOop::raw_body_word)/
    /// [`set_raw_body_word`](MemOop::set_raw_body_word) — pure offset
    /// arithmetic — rather than [`body_oop`](MemOop::body_oop)/
    /// [`set_body_oop`](MemOop::set_body_oop), which bounds-check via
    /// `self.body_word_count()` → `self.instance_size_words()` →
    /// `self.klass()`, RE-DERIVING the klass from `self`'s OWN klass field
    /// regardless of the `klass` parameter already passed in here. During a
    /// full GC's reference-rewrite phase that re-derivation is actively
    /// unsafe (not just redundant): `self`'s klass field, or — through
    /// `KlassOop::try_from`'s own one-hop validation of ITS klass field —
    /// some other, already-processed object's klass field, may already
    /// have been rewritten to a NEW address phase D hasn't populated yet,
    /// so the re-derivation can read format bits from unpopulated memory
    /// (found via `MACVM_GC_STRESS`-style testing of S8's own mark-compact
    /// pass — the cross-object sibling of invariant F3, one layer deeper
    /// than "don't rewrite THIS object's own klass field before sizing
    /// it"). This function already receives a `klass` its caller vouches
    /// for; it has no need to ask `self` again, checked or not.
    pub(crate) fn for_each_oop_field(self, klass: KlassOop, mut f: impl FnMut(Oop) -> Oop) {
        let nis = klass.non_indexable_size();
        let named = nis - HEADER_WORDS;
        match klass.format() {
            Format::Slots | Format::Klass => {
                for i in 0..named {
                    let v = Oop::from_raw(self.raw_body_word(i));
                    self.set_raw_body_word(i, f(v).raw());
                }
            }
            Format::Double | Format::Process => {
                // Double: raw f64 bits, never oops. Process: unreachable in v1.
            }
            Format::IndexableOops | Format::Closure | Format::Context => {
                for i in 0..named {
                    let v = Oop::from_raw(self.raw_body_word(i));
                    self.set_raw_body_word(i, f(v).raw());
                }
                let len = self.raw_size_slot(nis);
                let tail_start = named + 1;
                for i in 0..len {
                    let v = Oop::from_raw(self.raw_body_word(tail_start + i));
                    self.set_raw_body_word(tail_start + i, f(v).raw());
                }
            }
            Format::IndexableBytes | Format::Method => {
                for i in 0..named {
                    let v = Oop::from_raw(self.raw_body_word(i));
                    self.set_raw_body_word(i, f(v).raw());
                }
                // Byte tail (or Method's bytecode bytes): never oop-scanned.
            }
        }
    }

    /// A raw (non-oop) body word, e.g. a `Double`'s f64 payload. Deliberately
    /// distinct from [`MemOop::body_oop`]: an arbitrary bit pattern (a float)
    /// must never be run through `Oop::from_raw`'s tag `debug_assert`s.
    pub fn body_word_raw(self, index: usize) -> u64 {
        // The bound feeds ONLY the debug assert, but its loads (klass →
        // format → size slot) survive release builds unless the whole
        // computation is compiled out with the assert: LLVM must not
        // eliminate raw-pointer loads it can't prove safe. Profiled at a
        // combined ~57% of interpreter time (sample, richards, JIT=off).
        #[cfg(debug_assertions)]
        {
            let bound = self.body_word_count();
            debug_assert!(            index < bound,
            "body_word_raw: index {index} out of bounds ({bound})"
        );
        }
        self.raw_body_word(index)
    }

    pub fn set_body_word_raw(self, index: usize, w: u64) {
        // The bound feeds ONLY the debug assert, but its loads (klass →
        // format → size slot) survive release builds unless the whole
        // computation is compiled out with the assert: LLVM must not
        // eliminate raw-pointer loads it can't prove safe. Profiled at a
        // combined ~57% of interpreter time (sample, richards, JIT=off).
        #[cfg(debug_assertions)]
        {
            let bound = self.body_word_count();
            debug_assert!(            index < bound,
            "set_body_word_raw: index {index} out of bounds ({bound})"
        );
        }
        self.set_raw_body_word(index, w);
    }

    // --- indexable tail helpers, shared by Array/ByteArray/Symbol/Method --

    /// The size-slot value of an indexable-shaped object: element count
    /// (`IndexableOops`), byte count (`IndexableBytes`/`Method`), ncopied
    /// (`Closure`), or nslots (`Context`). Panics for a format with no size
    /// slot.
    pub fn indexable_len(self) -> usize {
        let k = self.klass();
        let nis = k.non_indexable_size();
        match k.format() {
            Format::IndexableOops
            | Format::IndexableBytes
            | Format::Method
            | Format::Closure
            | Format::Context => self.raw_size_slot(nis),
            other => panic!("indexable_len: format {other:?} has no size slot"),
        }
    }

    /// Body-word index of the first element/byte immediately after the size
    /// slot — the same formula for an oop tail (`IndexableOops`) or a byte
    /// tail (`IndexableBytes`/`Method`).
    pub fn tail_start_word(self) -> usize {
        let nis = self.klass().non_indexable_size();
        (nis - HEADER_WORDS) + 1
    }

    /// Byte `i` of a byte tail (`IndexableBytes`/`Method`/`Symbol`), packed
    /// 8 per word.
    pub fn tail_byte_at(self, i: usize) -> u8 {
        let word_idx = self.tail_start_word() + i / 8;
        let word = self.body_word_raw(word_idx);
        ((word >> ((i % 8) * 8)) & 0xFF) as u8
    }

    pub fn set_tail_byte_at(self, i: usize, b: u8) {
        let word_idx = self.tail_start_word() + i / 8;
        let shift = (i % 8) * 8;
        let old = self.body_word_raw(word_idx);
        let mask = !(0xFFu64 << shift);
        self.set_body_word_raw(word_idx, (old & mask) | ((b as u64) << shift));
    }

    /// Element `i` of an oop tail (`IndexableOops`), one per word.
    pub fn tail_oop_at(self, i: usize) -> Oop {
        self.body_oop(self.tail_start_word() + i)
    }

    pub fn set_tail_oop_at(self, i: usize, v: Oop) {
        let idx = self.tail_start_word() + i;
        self.set_body_oop(idx, v);
    }
}

#[cfg(test)]
mod tests {
    use super::super::layout::MEM_TAG;
    use super::*;

    /// A minimal 2-word (mark + klass) fake object over a self-owned
    /// buffer — `oops::` cannot depend on `memory::` (layering: memory
    /// builds on oops, not the reverse), so this can't reuse
    /// `Universe::genesis` the way `memory::` tests do. The returned `Box`
    /// must outlive the `MemOop` (it owns the backing words).
    fn fake_object() -> (Box<[u64]>, MemOop) {
        let mut buf = vec![Mark::pristine().word(), 0].into_boxed_slice();
        let addr = buf.as_mut_ptr() as u64;
        debug_assert_eq!(addr & 0b111, 0, "Vec<u64> must be 8-byte aligned");
        let oop = Oop::from_raw(addr + MEM_TAG);
        let mem = MemOop::try_from(oop).expect("tag-level mem oop construction");
        (buf, mem)
    }

    /// `tests_s07.md`'s `mark_forward_roundtrip`: install forwarding,
    /// `is_forwarded`, read forwardee; a non-forwarded mark says no.
    #[test]
    fn mark_forward_roundtrip() {
        let (_buf, obj) = fake_object();
        assert!(!obj.is_forwarded());

        let target_addr = 0x8000u64;
        let target = Oop::from_raw(target_addr + MEM_TAG);
        obj.install_forwarding(target);

        assert!(obj.is_forwarded());
        assert_eq!(obj.forwardee().raw(), target.raw());
    }
}
