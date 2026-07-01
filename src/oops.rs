//! Object references ("oops") and immediate tagging — class-based object model.
//!
//! MACVM adopts Strongtalk's representation (decision D2 in `docs/DESIGN.md`),
//! widened to 64-bit (D3): heap objects have a 2-word header `[mark][klass]` with
//! a **direct pointer** to their class — no object table. The exact tag scheme
//! and mark-word field layout are OPEN (see `docs/DESIGN.md` §3.1 and
//! `docs/arm64.md` §1). Everything below is provisional — go through the
//! predicates, do not hard-code the bit patterns elsewhere.

/// A tagged machine word. Thin value type so it stays in a register.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct Oop(pub isize);

impl Oop {
    // Provisional low-bit tag scheme on aligned pointers (Strongtalk widened).
    // `…0` = smi (tag 0 so ALU add/sub work untagged), `…1` = heap object.
    const SMI_TAG_MASK: isize = 0b1;
    const SMI_TAG: isize = 0b0;
    const MEM_TAG: isize = 0b1;
    const SMI_SHIFT: u32 = 1;

    #[inline]
    pub fn is_smi(self) -> bool {
        self.0 & Self::SMI_TAG_MASK == Self::SMI_TAG
    }

    #[inline]
    pub fn is_heap_object(self) -> bool {
        self.0 & Self::SMI_TAG_MASK == Self::MEM_TAG
    }

    #[inline]
    pub fn from_smi(v: isize) -> Oop {
        Oop((v << Self::SMI_SHIFT) | Self::SMI_TAG)
    }

    #[inline]
    pub fn to_smi(self) -> isize {
        self.0 >> Self::SMI_SHIFT
    }

    /// Untagged address of a heap object (biased by the tag, as in Strongtalk).
    /// Caller must ensure `is_heap_object()`. Provisional.
    #[inline]
    pub fn heap_addr(self) -> *mut u8 {
        (self.0 - Self::MEM_TAG) as *mut u8
    }
}
