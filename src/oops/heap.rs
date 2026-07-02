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
    /// would otherwise need the very value being computed.
    fn raw_size_slot(self, nis: usize) -> usize {
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

    pub fn body_oop(self, index: usize) -> Oop {
        let bound = self.body_word_count();
        debug_assert!(
            index < bound,
            "body_oop: index {index} out of bounds ({bound})"
        );
        Oop::from_raw(self.raw_body_word(index))
    }

    pub fn set_body_oop(self, index: usize, v: Oop) {
        let bound = self.body_word_count();
        debug_assert!(
            index < bound,
            "set_body_oop: index {index} out of bounds ({bound})"
        );
        self.set_raw_body_word(index, v.raw());
    }

    /// A raw (non-oop) body word, e.g. a `Double`'s f64 payload. Deliberately
    /// distinct from [`MemOop::body_oop`]: an arbitrary bit pattern (a float)
    /// must never be run through `Oop::from_raw`'s tag `debug_assert`s.
    pub fn body_word_raw(self, index: usize) -> u64 {
        let bound = self.body_word_count();
        debug_assert!(
            index < bound,
            "body_word_raw: index {index} out of bounds ({bound})"
        );
        self.raw_body_word(index)
    }

    pub fn set_body_word_raw(self, index: usize, w: u64) {
        let bound = self.body_word_count();
        debug_assert!(
            index < bound,
            "set_body_word_raw: index {index} out of bounds ({bound})"
        );
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
