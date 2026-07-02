//! `CompiledMethod` field accessors (SPEC §4.4). No `unsafe` — everything
//! here goes through `oops` wrappers (`body_oop`, `tail_byte_at`, …), never
//! raw pointer arithmetic.

use crate::oops::layout::{
    METHOD_COUNTERS_INDEX, METHOD_FLAGS_ARGC_BITS, METHOD_FLAGS_ARGC_MASK, METHOD_FLAGS_ARGC_SHIFT,
    METHOD_FLAGS_HAS_CTX_MASK, METHOD_FLAGS_INDEX, METHOD_FLAGS_IS_BLOCK_MASK,
    METHOD_FLAGS_NTEMPS_BITS, METHOD_FLAGS_NTEMPS_MASK, METHOD_FLAGS_NTEMPS_SHIFT,
    METHOD_FLAGS_PRIM_FAILS_MASK, METHOD_HOLDER_INDEX, METHOD_ICS_INDEX, METHOD_LITERALS_INDEX,
    METHOD_PRIMITIVE_INDEX, METHOD_SELECTOR_INDEX,
};
use crate::oops::smi::SmallInt;
use crate::oops::wrappers::{ArrayOop, MethodOop};
use crate::oops::Oop;

impl MethodOop {
    pub fn selector(self) -> Oop {
        self.as_mem().body_oop(METHOD_SELECTOR_INDEX)
    }

    pub fn set_selector(self, s: Oop) {
        self.as_mem().set_body_oop(METHOD_SELECTOR_INDEX, s);
    }

    pub fn holder(self) -> Oop {
        self.as_mem().body_oop(METHOD_HOLDER_INDEX)
    }

    pub fn set_holder(self, h: Oop) {
        self.as_mem().set_body_oop(METHOD_HOLDER_INDEX, h);
    }

    fn flags_value(self) -> i64 {
        SmallInt::try_from(self.as_mem().body_oop(METHOD_FLAGS_INDEX))
            .expect("method flags field is not a smi")
            .value()
    }

    fn set_flags_value(self, v: i64) {
        self.as_mem()
            .set_body_oop(METHOD_FLAGS_INDEX, SmallInt::new(v).oop());
    }

    pub fn argc(self) -> usize {
        ((self.flags_value() & METHOD_FLAGS_ARGC_MASK) >> METHOD_FLAGS_ARGC_SHIFT) as usize
    }

    pub fn ntemps(self) -> usize {
        ((self.flags_value() & METHOD_FLAGS_NTEMPS_MASK) >> METHOD_FLAGS_NTEMPS_SHIFT) as usize
    }

    pub fn has_ctx(self) -> bool {
        self.flags_value() & METHOD_FLAGS_HAS_CTX_MASK != 0
    }

    pub fn is_block(self) -> bool {
        self.flags_value() & METHOD_FLAGS_IS_BLOCK_MASK != 0
    }

    pub fn prim_fails(self) -> bool {
        self.flags_value() & METHOD_FLAGS_PRIM_FAILS_MASK != 0
    }

    /// Packs argc/ntemps/has_ctx/is_block/prim_fails into the flags field.
    /// `argc <= 15` (4-bit), `ntemps <= 255` (8-bit) — callers (the builder)
    /// must have already validated these; this fn just asserts them.
    pub fn set_flags(
        self,
        argc: usize,
        ntemps: usize,
        has_ctx: bool,
        is_block: bool,
        prim_fails: bool,
    ) {
        debug_assert!(
            argc < (1 << METHOD_FLAGS_ARGC_BITS),
            "argc {argc} exceeds 4 bits"
        );
        debug_assert!(
            ntemps < (1 << METHOD_FLAGS_NTEMPS_BITS),
            "ntemps {ntemps} exceeds 8 bits"
        );
        let mut v = (argc as i64) << METHOD_FLAGS_ARGC_SHIFT;
        v |= (ntemps as i64) << METHOD_FLAGS_NTEMPS_SHIFT;
        if has_ctx {
            v |= METHOD_FLAGS_HAS_CTX_MASK;
        }
        if is_block {
            v |= METHOD_FLAGS_IS_BLOCK_MASK;
        }
        if prim_fails {
            v |= METHOD_FLAGS_PRIM_FAILS_MASK;
        }
        self.set_flags_value(v);
    }

    pub fn primitive(self) -> i64 {
        SmallInt::try_from(self.as_mem().body_oop(METHOD_PRIMITIVE_INDEX))
            .expect("method primitive field is not a smi")
            .value()
    }

    pub fn set_primitive(self, id: i64) {
        self.as_mem()
            .set_body_oop(METHOD_PRIMITIVE_INDEX, SmallInt::new(id).oop());
    }

    pub fn counters(self) -> i64 {
        SmallInt::try_from(self.as_mem().body_oop(METHOD_COUNTERS_INDEX))
            .expect("method counters field is not a smi")
            .value()
    }

    pub fn set_counters(self, v: i64) {
        self.as_mem()
            .set_body_oop(METHOD_COUNTERS_INDEX, SmallInt::new(v).oop());
    }

    pub fn literals(self) -> ArrayOop {
        ArrayOop::try_from(self.as_mem().body_oop(METHOD_LITERALS_INDEX))
            .expect("method literals field is not an Array")
    }

    pub fn set_literals(self, a: ArrayOop) {
        self.as_mem().set_body_oop(METHOD_LITERALS_INDEX, a.oop());
    }

    pub fn ics(self) -> ArrayOop {
        ArrayOop::try_from(self.as_mem().body_oop(METHOD_ICS_INDEX))
            .expect("method ics field is not an Array")
    }

    pub fn set_ics(self, a: ArrayOop) {
        self.as_mem().set_body_oop(METHOD_ICS_INDEX, a.oop());
    }

    /// The bytecode byte count (the size slot).
    pub fn bytecode_len(self) -> usize {
        self.as_mem().indexable_len()
    }

    /// Byte `bci` of the bytecode tail. Debug bounds-checked against
    /// `bytecode_len()`.
    pub fn bytecode_byte(self, bci: usize) -> u8 {
        let len = self.bytecode_len();
        debug_assert!(
            bci < len,
            "bytecode_byte: bci {bci} out of bounds (len {len})"
        );
        self.as_mem().tail_byte_at(bci)
    }

    /// Write byte `bci` of the bytecode tail. Used by the builder (and
    /// hand-packing tests) while constructing a method; bytecode is
    /// immutable once a method is installed (SPEC §4).
    pub fn set_bytecode_byte(self, bci: usize, b: u8) {
        let len = self.bytecode_len();
        debug_assert!(
            bci < len,
            "set_bytecode_byte: bci {bci} out of bounds (len {len})"
        );
        self.as_mem().set_tail_byte_at(bci, b);
    }
}
