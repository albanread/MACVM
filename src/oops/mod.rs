//! Object references ("oops") and immediate tagging — class-based object model.
//!
//! MACVM adopts Strongtalk's representation (SPEC §2), widened to 64-bit:
//! heap objects have a 2-word header `[mark][klass]` with a **direct
//! pointer** to their class — no object table. The tag scheme and mark-word
//! field layout are pinned (SPEC §2.1–§2.2); see [`layout`] for the bit
//! constants. Everything in this module goes through checked predicates —
//! nothing outside `oops::layout` names a tag value, shift, or mask.
//!
//! `unsafe` is confined to this module tree (CONVENTIONS §1); this file is
//! the module-scoped opt-back-in from the crate's `#![deny(unsafe_code)]`.

#![allow(unsafe_code)]

pub mod layout;
pub mod mark;
pub mod smi;
pub mod wrappers;

pub use mark::Mark;
pub use smi::SmallInt;
pub use wrappers::{
    ArrayOop, ByteArrayOop, ClosureOop, ContextOop, DoubleOop, KlassOop, MemOop, MethodOop,
    SymbolOop,
};

use layout::{MARK_TAG, MEM_TAG, RESERVED_TAG, TAG_MASK};

/// A tagged 64-bit machine word — the only type crossing the unsafe boundary
/// (SPEC §2.1). Thin value type so it stays in a register.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct Oop(u64);

impl Oop {
    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Build an `Oop` from a raw word. In debug builds, rejects words whose
    /// tag is `RESERVED_TAG` or `MARK_TAG` — mark words are never general
    /// oops, and the reserved tag is unused in v1 (SPEC §2.1).
    #[inline]
    pub fn from_raw(w: u64) -> Oop {
        debug_assert!(
            w & TAG_MASK != RESERVED_TAG,
            "reserved tag: word {w:#x} has the unused RESERVED_TAG (0b10)"
        );
        debug_assert!(
            w & TAG_MASK != MARK_TAG,
            "mark tag: word {w:#x} has MARK_TAG (0b11) — mark words are not oops"
        );
        Oop(w)
    }

    /// Escape hatch without the tag `debug_assert!`s, for the mark-word slot
    /// and tests.
    #[inline]
    pub const fn from_raw_unchecked(w: u64) -> Oop {
        Oop(w)
    }

    #[inline]
    pub const fn tag(self) -> u64 {
        self.0 & TAG_MASK
    }

    #[inline]
    pub const fn is_smi(self) -> bool {
        self.tag() == layout::INT_TAG
    }

    #[inline]
    pub const fn is_mem(self) -> bool {
        self.tag() == MEM_TAG
    }

    /// Untagged address of a heap object. Caller must ensure `is_mem()`.
    #[inline]
    pub fn mem_addr(self) -> usize {
        debug_assert!(self.is_mem(), "mem_addr() on a non-mem oop {:#x}", self.0);
        (self.0 - MEM_TAG) as usize
    }
}

impl core::fmt::Debug for Oop {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let kind = match self.tag() {
            layout::INT_TAG => "smi",
            MEM_TAG => "mem",
            RESERVED_TAG => "reserved",
            MARK_TAG => "mark",
            _ => unreachable!("tag() masks to 2 bits"),
        };
        write!(f, "Oop({kind} {:#018x})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use layout::{INT_TAG, MEM_TAG, RESERVED_TAG};

    #[test]
    fn oop_tag_predicates() {
        let smi = Oop::from_raw(0b100);
        assert!(smi.is_smi());
        assert!(!smi.is_mem());

        let mem = Oop::from_raw(0b101);
        assert!(!mem.is_smi());
        assert!(mem.is_mem());
        assert_eq!(mem.mem_addr(), 0b100);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "reserved tag")]
    fn oop_from_raw_rejects_reserved() {
        let _ = Oop::from_raw(0b10);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "mark tag")]
    fn oop_from_raw_rejects_mark() {
        let _ = Oop::from_raw(0b11);
    }

    #[test]
    fn oop_tag_exhaustive() {
        // Each base already has its low 2 bits clear, so `base` alone
        // exercises INT_TAG; tag() must mask only the low 2 bits regardless
        // of the high bits.
        for base in [0u64, 0x1000, !3u64] {
            assert_eq!(Oop::from_raw_unchecked(base).tag(), INT_TAG);
            assert_eq!(Oop::from_raw_unchecked(base | 1).tag(), MEM_TAG);
            assert_eq!(Oop::from_raw_unchecked(base | 2).tag(), RESERVED_TAG);
            assert_eq!(Oop::from_raw_unchecked(base | 3).tag(), MARK_TAG);
        }
    }

    #[test]
    fn oop_debug_format() {
        let smi = smi::SmallInt::new(1).oop();
        let s = format!("{smi:?}");
        assert!(s.contains("smi"), "expected 'smi' in {s}");

        let mem = Oop::from_raw(0x1000 + 1);
        let s = format!("{mem:?}");
        assert!(s.contains("mem"), "expected 'mem' in {s}");
    }

    #[test]
    fn oop_size_static() {
        assert_eq!(core::mem::size_of::<Oop>(), 8);
        assert_eq!(core::mem::size_of::<Mark>(), 8);
        // repr(transparent) newtype over a non-niche u64: no niche to exploit,
        // so Option<SmallInt> is 16 bytes, not 8. Documented, not a bug.
        assert_eq!(core::mem::size_of::<Option<SmallInt>>(), 16);
    }
}
