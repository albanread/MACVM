//! Heap-oop newtype wrappers (SPEC §2.5). `MemOop` is the base: any tagged
//! heap pointer, tag-level check only. The typed siblings (`KlassOop`,
//! `ArrayOop`, …) additionally check the candidate's **klass format**
//! (SPEC §2.3) — re-checked at every construction, never cached, so a klass
//! reopened with a different format is reflected immediately (no stale
//! wrapper can outlive a format change).

use super::klass::Format;
use super::Oop;

#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct MemOop(Oop);

impl MemOop {
    /// Tag-level check only (`is_mem()`) — no format check exists at this
    /// layer; `oops::heap` adds the header/body accessors this type needs.
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

/// Declares a format-checked newtype over `MemOop`. `try_from` never panics
/// on a malformed candidate: it reads the candidate's klass field via the
/// non-panicking [`super::klass::format_of_klass_field`] path (a shallow,
/// non-recursive read — see that function's doc for why this must NOT go
/// through a fully-validating `KlassOop::try_from` on the klass field), so a
/// genesis placeholder or a corrupted klass word yields `None`, not a wild
/// read, a panic, or infinite recursion.
macro_rules! oop_newtype {
    ($name:ident, $format:expr) => {
        #[repr(transparent)]
        #[derive(Copy, Clone, PartialEq, Eq, Debug)]
        pub struct $name(MemOop);

        impl $name {
            pub fn try_from(o: Oop) -> Option<$name> {
                let m = MemOop::try_from(o)?;
                let fmt = super::klass::format_of_klass_field(m.klass_oop())?;
                if fmt == $format {
                    Some($name(m))
                } else {
                    None
                }
            }

            /// # Safety
            /// Caller guarantees `o` is a mem oop of the right klass format.
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

            // Unused for types with no dedicated accessors yet (MethodOop,
            // ClosureOop, ContextOop arrive in S2/S4) — the seam every
            // wrapper needs is declared uniformly here rather than only on
            // the types that currently exercise it.
            #[inline]
            #[allow(dead_code)]
            pub(crate) fn as_mem(self) -> MemOop {
                self.0
            }
        }
    };
}

oop_newtype!(KlassOop, Format::Klass);
oop_newtype!(ArrayOop, Format::IndexableOops);
oop_newtype!(ByteArrayOop, Format::IndexableBytes);
// S20 step 5 (docs/FFI.md §4): `Alien` shares `Format::IndexableBytes` with
// `ByteArrayOop`/`SymbolOop` (`Universe::alien_klass`'s own doc comment
// explains why it's the same underlying format), so `try_from` here is
// format-only — same as `ByteArrayOop`/`StringOop`, NOT `SymbolOop::
// try_from_exact`'s stricter exact-klass check. This is exactly as sound as
// it already is for those two: ordinary Smalltalk method dispatch already
// guarantees a primitive attached only to Alien's own method dict ever
// runs with an actual Alien receiver (the interpreter looks up the method
// via the RECEIVER's klass, so `Alien>>byteAt:` is simply never found —
// let alone invoked — for a non-Alien receiver in the first place).
oop_newtype!(AlienOop, Format::IndexableBytes);
oop_newtype!(MethodOop, Format::Method);
oop_newtype!(ClosureOop, Format::Closure);
oop_newtype!(ContextOop, Format::Context);
oop_newtype!(DoubleOop, Format::Double);

// SymbolOop is hand-written rather than macro-generated: format alone
// (IndexableBytes) does not distinguish a Symbol from a String or a plain
// ByteArray, which share it. `try_from` therefore only narrows to "some
// IndexableBytes object" (parity with the macro-generated siblings);
// `try_from_exact` additionally requires the exact `symbol_klass` identity
// and is what code that must reject e.g. a String needs to call.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct SymbolOop(MemOop);

impl SymbolOop {
    pub fn try_from(o: Oop) -> Option<SymbolOop> {
        let m = MemOop::try_from(o)?;
        let fmt = super::klass::format_of_klass_field(m.klass_oop())?;
        if fmt == Format::IndexableBytes {
            Some(SymbolOop(m))
        } else {
            None
        }
    }

    /// As `try_from`, but additionally requires `o`'s klass to be exactly
    /// `symbol_klass` (identity, not just format) — the check
    /// `wrapper_format_checks` (tests_s01.md) exercises: a `String`
    /// instance shares `IndexableBytes` format with `Symbol` and must NOT
    /// pass here.
    pub fn try_from_exact(o: Oop, symbol_klass: KlassOop) -> Option<SymbolOop> {
        let s = Self::try_from(o)?;
        if s.as_mem().klass().oop() == symbol_klass.oop() {
            Some(s)
        } else {
            None
        }
    }

    /// # Safety
    /// Caller guarantees `o` is a Symbol.
    #[inline]
    pub unsafe fn from_oop_unchecked(o: Oop) -> SymbolOop {
        SymbolOop(MemOop::from_oop_unchecked(o))
    }

    #[inline]
    pub fn oop(self) -> Oop {
        self.0.oop()
    }

    #[inline]
    pub fn addr(self) -> usize {
        self.0.addr()
    }

    #[inline]
    pub(crate) fn as_mem(self) -> MemOop {
        self.0
    }

    pub fn len(self) -> usize {
        self.0.indexable_len()
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Copies the bytes out (debug/printing only — never returns a slice
    /// into the heap, per the module-wide accessor discipline).
    pub fn as_string(self) -> String {
        let len = self.len();
        let mut buf = Vec::with_capacity(len);
        for i in 0..len {
            buf.push(self.0.tail_byte_at(i));
        }
        String::from_utf8_lossy(&buf).into_owned()
    }
}

// --- indexable accessors -----------------------------------------------

impl ArrayOop {
    pub fn len(self) -> usize {
        self.as_mem().indexable_len()
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    pub fn at(self, i: usize) -> Oop {
        // The bound feeds ONLY the debug assert, but its loads (klass →
        // format → size slot) survive release builds unless the whole
        // computation is compiled out with the assert: LLVM must not
        // eliminate raw-pointer loads it can't prove safe. Profiled at a
        // combined ~57% of interpreter time (sample, richards, JIT=off).
        #[cfg(debug_assertions)]
        {
            let len = self.len();
            debug_assert!(i < len, "at: index {i} out of bounds (len {len})");
        }
        self.as_mem().tail_oop_at(i)
    }

    pub fn at_put(self, i: usize, v: Oop) {
        // The bound feeds ONLY the debug assert, but its loads (klass →
        // format → size slot) survive release builds unless the whole
        // computation is compiled out with the assert: LLVM must not
        // eliminate raw-pointer loads it can't prove safe. Profiled at a
        // combined ~57% of interpreter time (sample, richards, JIT=off).
        #[cfg(debug_assertions)]
        {
            let len = self.len();
            debug_assert!(i < len, "at_put: index {i} out of bounds (len {len})");
        }
        self.as_mem().set_tail_oop_at(i, v);
    }
}

impl ByteArrayOop {
    pub fn len(self) -> usize {
        self.as_mem().indexable_len()
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    pub fn byte_at(self, i: usize) -> u8 {
        // The bound feeds ONLY the debug assert, but its loads (klass →
        // format → size slot) survive release builds unless the whole
        // computation is compiled out with the assert: LLVM must not
        // eliminate raw-pointer loads it can't prove safe. Profiled at a
        // combined ~57% of interpreter time (sample, richards, JIT=off).
        #[cfg(debug_assertions)]
        {
            let len = self.len();
            debug_assert!(i < len, "byte_at: index {i} out of bounds (len {len})");
        }
        self.as_mem().tail_byte_at(i)
    }

    pub fn byte_at_put(self, i: usize, b: u8) {
        // The bound feeds ONLY the debug assert, but its loads (klass →
        // format → size slot) survive release builds unless the whole
        // computation is compiled out with the assert: LLVM must not
        // eliminate raw-pointer loads it can't prove safe. Profiled at a
        // combined ~57% of interpreter time (sample, richards, JIT=off).
        #[cfg(debug_assertions)]
        {
            let len = self.len();
            debug_assert!(i < len, "byte_at_put: index {i} out of bounds (len {len})");
        }
        self.as_mem().set_tail_byte_at(i, b);
    }

    /// Copies the bytes out (never returns a slice into the heap).
    pub fn copy_bytes_out(self, buf: &mut Vec<u8>) {
        let len = self.len();
        buf.clear();
        buf.reserve(len);
        for i in 0..len {
            buf.push(self.byte_at(i));
        }
    }
}

impl AlienOop {
    /// Body-word-0 named field (`oops::layout::ALIEN_EXTERNAL_ADDR_INDEX`):
    /// `0` for a direct Alien (its own indexable tail IS the storage),
    /// nonzero for an indirect one (the real external address to read/
    /// write through instead — `runtime::alien`'s accessor primitives are
    /// the only real readers/writers of this distinction). Mirrors
    /// `MethodOop`'s own named-field-accessor idiom (`bytecode::method`):
    /// `body_oop` + `SmallInt::try_from`/`.value()`, never a raw word read,
    /// so the usual oop-tag `debug_assert`s still apply to this slot.
    pub fn external_addr(self) -> u64 {
        crate::oops::smi::SmallInt::try_from(
            self.as_mem()
                .body_oop(crate::oops::layout::ALIEN_EXTERNAL_ADDR_INDEX),
        )
        .expect("Alien external-address field is not a SmallInt")
        .value() as u64
    }

    /// Sets the external-address field — `0` to mark direct (the default a
    /// fresh `Alien class >> new:` leaves it at), nonzero to mark indirect
    /// (`Alien class >> forAddress:size:`). `addr` is a real process
    /// virtual address; on real macOS/arm64 (48-bit-or-smaller user virtual
    /// address space) this always fits `SmallInt`'s 61-bit magnitude —
    /// same trust `runtime::ffi`'s own "g" return-class unmarshal already
    /// places in a native address/pointer value (see that module's own
    /// `FfiRetClass::G` doc for the range argument in full).
    pub fn set_external_addr(self, addr: u64) {
        self.as_mem().set_body_oop(
            crate::oops::layout::ALIEN_EXTERNAL_ADDR_INDEX,
            crate::oops::smi::SmallInt::new(addr as i64).oop(),
        );
    }
}

impl DoubleOop {
    pub fn value(self) -> f64 {
        f64::from_bits(self.as_mem().body_word_raw(0))
    }
}

/// Test-only fixture: a `SymbolOop` at a caller-chosen raw address, `unsafe`
/// confined here (CONVENTIONS §1) since it never needs to point at a real
/// object — `runtime::lookup`'s `LookupCache` tests need this to construct
/// an address that collides with a chosen cache slot, and the cache never
/// dereferences its keys (only compares raw bits).
#[cfg(test)]
pub(crate) fn fake_symbol_for_test(addr_index: u64) -> SymbolOop {
    let raw = (addr_index << 3) | super::layout::MEM_TAG;
    // SAFETY: as above.
    unsafe { SymbolOop::from_oop_unchecked(Oop::from_raw_unchecked(raw)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_oop_roundtrip_tags() {
        // Full-format wrapper testing lives in tests/it_memory.rs and the
        // memory:: unit tests, which have Universe/klass objects to build
        // real instances against. Here we only confirm the tag-level
        // MemOop contract that every wrapper's try_from starts from.
        let smi = Oop::from_raw_unchecked(4);
        assert!(MemOop::try_from(smi).is_none());
    }
}
