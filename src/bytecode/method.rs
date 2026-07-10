//! `CompiledMethod` field accessors (SPEC §4.4). No `unsafe` — everything
//! here goes through `oops` wrappers (`body_oop`, `tail_byte_at`, …), never
//! raw pointer arithmetic.

use crate::oops::layout::{
    METHOD_COUNTERS_INDEX, METHOD_FLAGS_ARGC_BITS, METHOD_FLAGS_ARGC_MASK, METHOD_FLAGS_ARGC_SHIFT,
    METHOD_FLAGS_CAPTURES_CTX_MASK, METHOD_FLAGS_HAS_CTX_MASK, METHOD_FLAGS_INDEX,
    METHOD_FLAGS_IS_BLOCK_MASK, METHOD_FLAGS_NCTX_BITS, METHOD_FLAGS_NCTX_MASK,
    METHOD_FLAGS_NCTX_SHIFT, METHOD_FLAGS_NTEMPS_BITS, METHOD_FLAGS_NTEMPS_MASK,
    METHOD_FLAGS_NTEMPS_SHIFT, METHOD_FLAGS_PRIM_FAILS_MASK, METHOD_HOLDER_INDEX, METHOD_ICS_INDEX,
    METHOD_LITERALS_INDEX, METHOD_PRIMITIVE_INDEX, METHOD_SELECTOR_INDEX,
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

    /// Whether this *block*'s closure must capture the enclosing
    /// `ContextOop` (SPEC §2.3/§5.4, S4) — `copied[1]` is present iff this
    /// is set.
    pub fn captures_ctx(self) -> bool {
        self.flags_value() & METHOD_FLAGS_CAPTURES_CTX_MASK != 0
    }

    /// Number of heap-Context slots to allocate on activation when
    /// `has_ctx` (SPEC §5.4, S4).
    pub fn nctx(self) -> usize {
        ((self.flags_value() & METHOD_FLAGS_NCTX_MASK) >> METHOD_FLAGS_NCTX_SHIFT) as usize
    }

    /// Packs argc/ntemps/has_ctx/is_block/prim_fails/captures_ctx/nctx into
    /// the flags field. `argc <= 15` (4-bit), `ntemps <= 255` (8-bit),
    /// `nctx <= 255` (8-bit) — callers (the builder) must have already
    /// validated these; this fn just asserts them.
    #[allow(clippy::too_many_arguments)]
    pub fn set_flags(
        self,
        argc: usize,
        ntemps: usize,
        has_ctx: bool,
        is_block: bool,
        prim_fails: bool,
        captures_ctx: bool,
        nctx: usize,
    ) {
        debug_assert!(
            argc < (1 << METHOD_FLAGS_ARGC_BITS),
            "argc {argc} exceeds 4 bits"
        );
        debug_assert!(
            ntemps < (1 << METHOD_FLAGS_NTEMPS_BITS),
            "ntemps {ntemps} exceeds 8 bits"
        );
        debug_assert!(
            nctx < (1 << METHOD_FLAGS_NCTX_BITS),
            "nctx {nctx} exceeds 8 bits"
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
        if captures_ctx {
            v |= METHOD_FLAGS_CAPTURES_CTX_MASK;
        }
        v |= (nctx as i64) << METHOD_FLAGS_NCTX_SHIFT;
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

    /// S10 D1: set once `driver::eligible` rejects this method, so the
    /// counter-overflow trigger stops re-attempting it.
    pub fn compile_disabled(self) -> bool {
        self.counters() & crate::oops::layout::COUNTERS_COMPILE_DISABLED_BIT != 0
    }

    /// Sets the compile-disabled bit and resets the invocation count to 0
    /// (D1: "Ineligible methods set a dont-compile bit... and reset the
    /// invocation count") — one write, since both live in the same smi.
    /// NOTE (DBG1): this WHOLE-WORD write also clears `has_bp` — callers
    /// that combine the two (`debug::set_breakpoint`) must re-set `has_bp`
    /// after, which they do.
    pub fn set_compile_disabled(self) {
        self.set_counters(crate::oops::layout::COUNTERS_COMPILE_DISABLED_BIT);
    }

    /// S24 L2 step 2 (trigger unification, user-decided policy): "the loop
    /// counters have detected in a different way that the method containing
    /// the loop is hot; the method is now hot." Called by
    /// `compile_method_full` after a successful `by_key` install — raises the
    /// invocation counter to (at least) the tier-up threshold so
    /// `activate_method`'s EXISTING `bumped >= n` gate routes every future
    /// call into the nmethod, unifying the two profile triggers (invocation
    /// count and loop backedges) with zero new dispatch state. Masked RMW on
    /// bits 0-15 only; never lowers; a threshold-triggered install is a
    /// no-op (counter already ≥ n). Returns true when it actually raised the
    /// counter (i.e. this install was loop/backedge-earned — the
    /// `trigger_unifications` stat).
    pub fn saturate_invocation(self, n: i64) -> bool {
        use crate::oops::layout::{COUNTERS_INVOCATION_MASK, COUNTERS_INVOCATION_MAX};
        let c = self.counters();
        let cur = c & COUNTERS_INVOCATION_MASK;
        let want = n.clamp(0, COUNTERS_INVOCATION_MAX);
        if cur >= want {
            return false;
        }
        self.set_counters((c & !COUNTERS_INVOCATION_MASK) | want);
        true
    }

    /// DBG1: clears ONLY the compile-disabled bit (breakpoint removal
    /// restores tier-up eligibility; recompilation then happens naturally
    /// by counters).
    pub fn clear_compile_disabled(self) {
        self.set_counters(self.counters() & !crate::oops::layout::COUNTERS_COMPILE_DISABLED_BIT);
    }

    /// DBG1 (docs/DEBUGGER.md §2): does this method carry at least one
    /// breakpoint? The dispatch fast path's second gate after
    /// `debug.active`.
    pub fn has_bp(self) -> bool {
        self.counters() & crate::oops::layout::COUNTERS_HAS_BP_BIT != 0
    }

    pub fn set_has_bp(self) {
        self.set_counters(self.counters() | crate::oops::layout::COUNTERS_HAS_BP_BIT);
    }

    pub fn clear_has_bp(self) {
        self.set_counters(self.counters() & !crate::oops::layout::COUNTERS_HAS_BP_BIT);
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

#[cfg(test)]
mod tests {
    use crate::runtime::vm_state::{VmOptions, VmState};

    fn test_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            gc_stress_full_period: None,
            eden_kb: None,
            jit: crate::runtime::JitMode::Off,
        })
    }

    /// S24 L2 step 1 (T14): `compile_disabled` must survive the S15 loop
    /// counter's masked RMWs. The original bit 16 sat INSIDE the loop field
    /// (bits 16-31), so one `bump_loop_counter` clobbered it and a loopy
    /// NoPermanent method re-attempted compilation forever.
    #[test]
    fn compile_disabled_survives_loop_counter_rmw() {
        let mut vm = test_vm();
        let m = crate::memory::alloc::alloc_method(&mut vm, 1);

        m.set_compile_disabled();
        for _ in 0..3 {
            crate::runtime::osr::bump_loop_counter(m);
        }
        assert!(
            m.compile_disabled(),
            "bump_loop_counter's bits-16-31 RMW must not clobber the disable bit"
        );
        crate::runtime::osr::reset_loop_counter(m);
        assert!(
            m.compile_disabled(),
            "reset_loop_counter must not clobber the disable bit"
        );
        // And the flag must not alias INTO the counter: with the bit set,
        // the first bump yields loop-count 1, not a poisoned value.
        let crossed = crate::runtime::osr::bump_loop_counter(m);
        assert!(!crossed, "count 1 must not read as >= LOOP_COUNTER_LIMIT");
    }

    /// S24 L2 step 2: trigger unification's counter mechanics.
    #[test]
    fn saturate_invocation_raises_never_lowers() {
        let mut vm = test_vm();
        let m = crate::memory::alloc::alloc_method(&mut vm, 1);

        // Cold method: saturation raises to the threshold and reports it.
        assert!(m.saturate_invocation(200));
        assert_eq!(
            m.counters() & crate::oops::layout::COUNTERS_INVOCATION_MASK,
            200
        );
        // Idempotent / never lowers.
        assert!(!m.saturate_invocation(200));
        assert!(!m.saturate_invocation(100));
        assert_eq!(
            m.counters() & crate::oops::layout::COUNTERS_INVOCATION_MASK,
            200
        );
        // Preserves the flag bits outside the invocation field.
        m.set_has_bp();
        assert!(m.saturate_invocation(300));
        assert!(m.has_bp());
        // Clamped to the field ceiling for absurd thresholds.
        assert!(m.saturate_invocation(1 << 40));
        assert_eq!(
            m.counters() & crate::oops::layout::COUNTERS_INVOCATION_MASK,
            crate::oops::layout::COUNTERS_INVOCATION_MAX
        );
    }

    #[test]
    fn flags_nctx_captures() {
        let mut vm = test_vm();
        let m = crate::memory::alloc::alloc_method(&mut vm, 1);

        m.set_flags(3, 5, false, false, false, false, 0);
        assert_eq!(m.argc(), 3);
        assert_eq!(m.ntemps(), 5);
        assert!(!m.has_ctx());
        assert!(!m.is_block());
        assert!(!m.prim_fails());
        assert!(!m.captures_ctx());
        assert_eq!(m.nctx(), 0);

        // Flipping only the new fields must not disturb the old ones.
        m.set_flags(3, 5, true, true, true, true, 200);
        assert_eq!(m.argc(), 3);
        assert_eq!(m.ntemps(), 5);
        assert!(m.has_ctx());
        assert!(m.is_block());
        assert!(m.prim_fails());
        assert!(m.captures_ctx());
        assert_eq!(m.nctx(), 200);

        // Edges.
        m.set_flags(15, 255, false, false, false, true, 255);
        assert_eq!(m.argc(), 15);
        assert_eq!(m.ntemps(), 255);
        assert!(m.captures_ctx());
        assert_eq!(m.nctx(), 255);

        m.set_flags(0, 0, false, false, false, false, 0);
        assert!(!m.captures_ctx());
        assert_eq!(m.nctx(), 0);
    }
}
