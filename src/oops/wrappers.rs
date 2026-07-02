//! Heap-oop newtype wrappers (SPEC §2.5). `MemOop` is the base: any tagged
//! heap pointer, with tag-level checks only. The typed siblings
//! (`KlassOop`, `ArrayOop`, …) are identical in shape; in S0 their
//! `try_from` delegates to `MemOop::try_from` (tag check only) — S1
//! tightens each to also check the klass format once object headers exist.

use super::Oop;

#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct MemOop(Oop);

impl MemOop {
    /// Tag-level check only (`is_mem()`) — no format check exists yet.
    #[inline]
    pub fn try_from(o: Oop) -> Option<MemOop> {
        if o.is_mem() {
            Some(MemOop(o))
        } else {
            None
        }
    }

    /// # Safety
    /// Caller guarantees `o` is a mem oop of the right shape.
    #[inline]
    pub unsafe fn from_oop_unchecked(o: Oop) -> MemOop {
        MemOop(o)
    }

    #[inline]
    pub fn oop(self) -> Oop {
        self.0
    }

    #[inline]
    pub fn addr(self) -> usize {
        self.0.mem_addr()
    }
}

macro_rules! oop_newtype {
    ($name:ident) => {
        #[repr(transparent)]
        #[derive(Copy, Clone, PartialEq, Eq)]
        pub struct $name(MemOop);

        impl $name {
            /// Tag-level check only in S0; S1 tightens this to also check
            /// the klass format (SPEC §2.5).
            #[inline]
            pub fn try_from(o: Oop) -> Option<$name> {
                MemOop::try_from(o).map($name)
            }

            /// # Safety
            /// Caller guarantees `o` is a mem oop of the right shape/format.
            #[inline]
            pub unsafe fn from_oop_unchecked(o: Oop) -> $name {
                $name(MemOop::from_oop_unchecked(o))
            }

            #[inline]
            pub fn oop(self) -> Oop {
                self.0.oop()
            }

            #[inline]
            pub fn addr(self) -> usize {
                self.0.addr()
            }
        }
    };
}

oop_newtype!(KlassOop);
oop_newtype!(ArrayOop);
oop_newtype!(ByteArrayOop);
oop_newtype!(SymbolOop);
oop_newtype!(MethodOop);
oop_newtype!(ClosureOop);
oop_newtype!(ContextOop);
oop_newtype!(DoubleOop);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_tag_checks() {
        let smi = Oop::from_raw_unchecked(4); // tag INT_TAG
        assert!(MemOop::try_from(smi).is_none());

        let mem = Oop::from_raw(0x1008 + 1);
        let m = MemOop::try_from(mem).expect("mem tag");
        assert_eq!(m.addr(), 0x1008);

        // Macro-generated types share the same tag-check contract.
        assert!(ArrayOop::try_from(smi).is_none());
        let a = ArrayOop::try_from(mem).expect("mem tag");
        assert_eq!(a.addr(), 0x1008);

        assert!(KlassOop::try_from(smi).is_none());
        assert!(KlassOop::try_from(mem).is_some());
        assert!(ByteArrayOop::try_from(smi).is_none());
        assert!(ByteArrayOop::try_from(mem).is_some());
        assert!(SymbolOop::try_from(smi).is_none());
        assert!(SymbolOop::try_from(mem).is_some());
        assert!(MethodOop::try_from(smi).is_none());
        assert!(MethodOop::try_from(mem).is_some());
        assert!(ClosureOop::try_from(smi).is_none());
        assert!(ClosureOop::try_from(mem).is_some());
        assert!(ContextOop::try_from(smi).is_none());
        assert!(ContextOop::try_from(mem).is_some());
        assert!(DoubleOop::try_from(smi).is_none());
        assert!(DoubleOop::try_from(mem).is_some());
    }

    #[test]
    fn wrapper_oop_roundtrip() {
        let word = 0x2000u64 + 1;
        let o = Oop::from_raw(word);

        macro_rules! check {
            ($ty:ty) => {{
                let w = <$ty>::try_from(o).expect("mem tag");
                assert_eq!(w.oop().raw(), word);
                let w2 = unsafe { <$ty>::from_oop_unchecked(o) };
                assert_eq!(w2.oop().raw(), word);
            }};
        }

        check!(MemOop);
        check!(KlassOop);
        check!(ArrayOop);
        check!(ByteArrayOop);
        check!(SymbolOop);
        check!(MethodOop);
        check!(ClosureOop);
        check!(ContextOop);
        check!(DoubleOop);
    }
}
