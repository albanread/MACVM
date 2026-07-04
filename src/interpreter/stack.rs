//! `ProcessStack` and `Frame` (SPEC §5.1). The stack grows upward and is
//! **index-based, never pointer-based** — the backing `Vec` may reallocate,
//! and every slot is a valid oop at all times (smi-encoded fp/bci links
//! included), which is what makes S7's exact stack scan free.

use crate::oops::layout::{
    FRAME_CONTEXT, FRAME_MARKER, FRAME_MARKER_KIND_MASK, FRAME_METHOD, FRAME_RECEIVER,
    FRAME_SAVED_BCI, FRAME_SAVED_FP, FRAME_SERIAL, FRAME_SERIAL_MASK, FRAME_TEMPS_BASE,
};
use crate::oops::smi::SmallInt;
use crate::oops::wrappers::MethodOop;
use crate::oops::Oop;

/// Default operand-stack capacity, in oop slots *(tunable)*.
pub const DEFAULT_STACK_CAPACITY: usize = 64 * 1024;

/// The process stack is full — `try_push`'s error, so overflow can be
/// tested without a real `exit(70)` subprocess.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct StackOverflow;

/// Opaque snapshot returned by [`ProcessStack::save_activation`] — see that
/// method's own doc.
#[derive(Copy, Clone, Debug)]
pub struct FrameActivation {
    fp: usize,
    has_frame: bool,
}

impl FrameActivation {
    /// Whether a frame was active at the moment this snapshot was taken. The
    /// snapshot is otherwise opaque (only `restore_activation` consumes it);
    /// this is exposed for S13 deopt's `DeoptResume` cross-checks — the
    /// ambient outer activation the nested run must preserve.
    #[inline]
    pub fn was_active(self) -> bool {
        self.has_frame
    }
}

pub struct ProcessStack {
    slots: Vec<Oop>,
    pub sp: usize,
    pub fp: usize,
    /// Whether any frame has been activated yet. Distinguishes "no frame
    /// exists, `fp` is meaningless" (the initial state, and what bare
    /// push/pop mechanics tests exercise) from "a real frame is active at
    /// `fp`" — the two need different underflow floors in `pop()`.
    has_frame: bool,
}

impl ProcessStack {
    pub fn with_capacity(capacity: usize) -> ProcessStack {
        ProcessStack {
            slots: vec![Oop::from_raw_unchecked(0); capacity],
            sp: 0,
            fp: 0,
            has_frame: false,
        }
    }

    /// Marks a frame as active at `fp`. Called by `interpreter::activate_method`
    /// once the new frame's fixed slots are pushed, and by
    /// `interpreter::return_from_frame` when restoring a caller frame —
    /// never set `fp` directly without also calling this (or `pop()`'s
    /// underflow check degrades incorrectly).
    #[inline]
    pub fn activate_frame(&mut self, fp: usize) {
        self.fp = fp;
        self.has_frame = true;
    }

    /// Whether a frame is currently active — see the field's own doc.
    /// Mainly for tests: real code either already knows (it just called
    /// `activate_frame`/`deactivate` itself) or goes through
    /// `save_activation`/`restore_activation`, neither of which needs to
    /// read this directly.
    #[inline]
    pub fn has_frame(&self) -> bool {
        self.has_frame
    }

    /// Marks no frame as active — called when the entry frame returns.
    #[inline]
    pub fn deactivate(&mut self) {
        self.has_frame = false;
    }

    /// S11 D6.1: snapshots "is a frame active, and if so where" —
    /// `interpreter::run_method_reentrant`'s own save/restore pair around
    /// a nested re-entrant call (compiled code calling back into an
    /// interpreted method, C→I). `run_method`'s own completion always
    /// [`Self::deactivate`]s unconditionally (`unwind::pop_and_deliver`'s
    /// `ENTRY_FRAME_SENTINEL` case) — correct for the genuine top-level
    /// entry point it was designed for, but wrong for a NESTED call: if an
    /// outer interpreter activation is currently paused (a compiled frame
    /// running above it, itself reached via a normal send), the nested
    /// call's own completion would silently drop that outer activation's
    /// `fp`/`has_frame` bookkeeping even though its frame data is still
    /// sitting untouched in `slots`. `sp` needs no equivalent treatment —
    /// `run_method`'s own push/pop nets to exactly zero stack effect once
    /// its entry frame returns, by construction.
    #[inline]
    pub fn save_activation(&self) -> FrameActivation {
        FrameActivation {
            fp: self.fp,
            has_frame: self.has_frame,
        }
    }

    /// The other half of [`Self::save_activation`].
    #[inline]
    pub fn restore_activation(&mut self, saved: FrameActivation) {
        self.fp = saved.fp;
        self.has_frame = saved.has_frame;
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    #[inline]
    pub fn get(&self, idx: usize) -> Oop {
        self.slots[idx]
    }

    #[inline]
    pub fn set(&mut self, idx: usize, v: Oop) {
        self.slots[idx] = v;
    }

    /// Pushes `v` if there's room; `Err(StackOverflow)` on overflow (never a
    /// Rust index panic). S2's overflow POLICY is exit(70) at the call site
    /// (matching `alloc_words_raw`'s exhaustion contract); a send-depth
    /// check replaces this in S3, a Smalltalk-visible error in S6.
    #[inline]
    pub fn try_push(&mut self, v: Oop) -> Result<(), StackOverflow> {
        if self.sp >= self.slots.len() {
            return Err(StackOverflow);
        }
        self.slots[self.sp] = v;
        self.sp += 1;
        Ok(())
    }

    #[inline]
    pub fn push(&mut self, v: Oop) {
        if self.try_push(v).is_err() {
            eprintln!("macvm: process stack overflow");
            std::process::exit(70);
        }
    }

    #[inline]
    pub fn pop(&mut self) -> Oop {
        let floor = if self.has_frame {
            self.fp + FRAME_TEMPS_BASE + self.ntemps_at_fp()
        } else {
            // No frame activated yet — bare stack mechanics (S2's
            // `stack_push_pop`-style tests): the only sane floor is 0.
            0
        };
        debug_assert!(
            self.sp > floor,
            "pop: operand stack underflow into frame slots"
        );
        self.sp -= 1;
        self.slots[self.sp]
    }

    #[inline]
    pub fn top(&self) -> Oop {
        self.slots[self.sp - 1]
    }

    fn ntemps_at_fp(&self) -> usize {
        match MethodOop::try_from(self.slots[self.fp + FRAME_METHOD]) {
            Some(method) => method.ntemps(),
            None => 0,
        }
    }
}

/// The kind of an armed `ensure:`/`ifCurtailed:` handler, packed into
/// `FRAME_SERIAL`'s bit 32 (SPEC §5.4, S4) — meaningful only while
/// `FRAME_MARKER` holds a `ClosureOop` (an armed handler), not while it
/// holds `nil` or an `UnwindToken` (`ArrayOop`).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MarkerKind {
    Ensure,
    IfCurtailed,
}

/// A lightweight, `Copy` view onto one frame — holds no oops itself, so it
/// cannot go stale across a push/pop that reallocates `ProcessStack`'s
/// backing `Vec`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Frame {
    pub fp: usize,
}

impl Frame {
    pub fn method(self, st: &ProcessStack) -> MethodOop {
        MethodOop::try_from(st.get(self.fp + FRAME_METHOD))
            .expect("Frame::method: frame method slot is not a CompiledMethod")
    }

    pub fn saved_fp(self, st: &ProcessStack) -> i64 {
        SmallInt::try_from(st.get(self.fp + FRAME_SAVED_FP))
            .expect("Frame::saved_fp: not a smi")
            .value()
    }

    pub fn saved_bci(self, st: &ProcessStack) -> usize {
        SmallInt::try_from(st.get(self.fp + FRAME_SAVED_BCI))
            .expect("Frame::saved_bci: not a smi")
            .value() as usize
    }

    pub fn context(self, st: &ProcessStack) -> Oop {
        st.get(self.fp + FRAME_CONTEXT)
    }

    pub fn set_context(self, st: &mut ProcessStack, c: Oop) {
        st.set(self.fp + FRAME_CONTEXT, c);
    }

    fn serial_word(self, st: &ProcessStack) -> i64 {
        SmallInt::try_from(st.get(self.fp + FRAME_SERIAL))
            .expect("Frame::serial_word: not a smi")
            .value()
    }

    /// The frame-serial this activation was pushed with (SPEC §5.4's
    /// dead-home detection).
    pub fn serial(self, st: &ProcessStack) -> u32 {
        (self.serial_word(st) & FRAME_SERIAL_MASK) as u32
    }

    /// `nil` (no marker) | `ClosureOop` (an armed handler) | `ArrayOop` (an
    /// `UnwindToken`, while that handler runs during an unwind).
    pub fn marker(self, st: &ProcessStack) -> Oop {
        st.get(self.fp + FRAME_MARKER)
    }

    /// Only meaningful while `marker()` is an armed handler closure.
    pub fn marker_kind(self, st: &ProcessStack) -> MarkerKind {
        if self.serial_word(st) & FRAME_MARKER_KIND_MASK != 0 {
            MarkerKind::IfCurtailed
        } else {
            MarkerKind::Ensure
        }
    }

    /// Read-modify-write: the serial slot's low 32 bits (the actual serial)
    /// must survive — only the kind bit changes.
    pub fn set_marker(self, st: &mut ProcessStack, m: Oop, kind: MarkerKind) {
        let serial_bits = self.serial_word(st) & FRAME_SERIAL_MASK;
        let kind_bit = match kind {
            MarkerKind::Ensure => 0,
            MarkerKind::IfCurtailed => FRAME_MARKER_KIND_MASK,
        };
        st.set(
            self.fp + FRAME_SERIAL,
            SmallInt::new(serial_bits | kind_bit).oop(),
        );
        st.set(self.fp + FRAME_MARKER, m);
    }

    pub fn clear_marker(self, st: &mut ProcessStack, nil: Oop) {
        st.set(self.fp + FRAME_MARKER, nil);
    }

    /// The canonical receiver: the fast copy at `fp+4`. Distinct from the
    /// caller's pushed receiver at `fp - argc - 1` (which `return_from_frame`
    /// overwrites with the result) — the two are born equal (`self` is not
    /// assignable in Smalltalk) but S4's Context and S13's deopt read this
    /// one; do not "optimize" either copy away.
    pub fn receiver(self, st: &ProcessStack) -> Oop {
        st.get(self.fp + FRAME_RECEIVER)
    }

    /// Unified arg/temp index: `t < argc` addresses the caller's pushed
    /// argument area (`fp - argc + t`); `t >= argc` addresses a fixed local
    /// temp slot (`fp + FRAME_TEMPS_BASE + (t - argc)`).
    fn temp_index(self, st: &ProcessStack, t: usize) -> usize {
        let argc = self.method(st).argc();
        debug_assert!(
            t < argc + self.method(st).ntemps(),
            "temp index {t} out of range (argc {argc})"
        );
        if t < argc {
            self.fp - argc + t
        } else {
            self.fp + FRAME_TEMPS_BASE + (t - argc)
        }
    }

    pub fn temp(self, st: &ProcessStack, t: usize) -> Oop {
        let idx = self.temp_index(st, t);
        st.get(idx)
    }

    pub fn set_temp(self, st: &mut ProcessStack, t: usize, v: Oop) {
        let idx = self.temp_index(st, t);
        st.set(idx, v);
    }

    /// The receiver's raw stack slot (`fp - argc - 1`) — the one `return`
    /// overwrites with the result.
    pub fn receiver_slot(self, st: &ProcessStack) -> usize {
        let argc = self.method(st).argc();
        self.fp - argc - 1
    }

    /// Assert this frame is structurally indistinguishable from one the
    /// interpreter's own `push_frame` built (SPEC §5.1 layout): every fixed
    /// header slot holds a value of the right shape. S13's deopt materializer
    /// (`runtime::deopt`) runs this on every frame it reconstructs — a
    /// materialized frame that fails `verify` is a materializer bug, caught
    /// here rather than three bytecodes into the resumed activation.
    ///
    /// The checks mirror exactly what `push_frame` guarantees about a
    /// freshly-pushed frame and what the `Frame` accessors above already
    /// `expect` at their use sites:
    /// - `FRAME_METHOD` is a `CompiledMethod` (`MethodOop`),
    /// - `FRAME_SAVED_FP`, `FRAME_SAVED_BCI`, `FRAME_SERIAL` are smis,
    /// - `FRAME_RECEIVER` (the fast copy) equals the caller-pushed receiver
    ///   at `fp - argc - 1` (`push_frame` copies one from the other),
    /// - the frame occupies at least its fixed header + `ntemps` slots below
    ///   `sp`, and `fp` clears the header the args sit below it.
    ///
    /// Panics with a precise message on the first violation (debug-oriented,
    /// so it reads like an `assert`), returns `()` on success.
    pub fn verify(self, st: &ProcessStack) {
        // Header slots must land within the live stack region.
        assert!(
            self.fp + FRAME_TEMPS_BASE <= st.sp,
            "Frame::verify: fp {} + header {} exceeds sp {} (truncated frame)",
            self.fp,
            FRAME_TEMPS_BASE,
            st.sp
        );
        // FRAME_METHOD: a real CompiledMethod.
        let method = MethodOop::try_from(st.get(self.fp + FRAME_METHOD))
            .expect("Frame::verify: FRAME_METHOD slot is not a CompiledMethod");
        // saved_fp / saved_bci / serial: all smi-encoded.
        SmallInt::try_from(st.get(self.fp + FRAME_SAVED_FP))
            .expect("Frame::verify: FRAME_SAVED_FP is not a smi");
        SmallInt::try_from(st.get(self.fp + FRAME_SAVED_BCI))
            .expect("Frame::verify: FRAME_SAVED_BCI is not a smi");
        SmallInt::try_from(st.get(self.fp + FRAME_SERIAL))
            .expect("Frame::verify: FRAME_SERIAL is not a smi");
        // The args area must sit fully below fp: the receiver slot
        // `fp - argc - 1` must be a valid index (>= 0), i.e. `fp > argc`.
        let argc = method.argc();
        assert!(
            self.fp > argc,
            "Frame::verify: fp {} does not clear its {argc} args + receiver below it",
            self.fp
        );
        // FRAME_RECEIVER is the fast copy of the caller-pushed receiver
        // (`push_frame` copies `stack[fp - argc - 1]` into `fp + 4`).
        assert_eq!(
            self.receiver(st),
            st.get(self.fp - argc - 1),
            "Frame::verify: FRAME_RECEIVER copy disagrees with the arg-area receiver"
        );
        // The fixed temps must all be present below sp.
        assert!(
            self.fp + FRAME_TEMPS_BASE + method.ntemps() <= st.sp,
            "Frame::verify: fp {} + header + {} temps exceeds sp {}",
            self.fp,
            method.ntemps(),
            st.sp
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oops::layout::ENTRY_FRAME_SENTINEL;
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

    fn trivial_method(vm: &mut VmState, argc: usize, ntemps: usize) -> MethodOop {
        let mut b = crate::bytecode::BytecodeBuilder::new();
        b.ret_self();
        let sel = vm.universe.intern(b"m");
        b.finish(vm, sel, argc, ntemps)
    }

    #[test]
    fn stack_push_pop() {
        let mut st = ProcessStack::with_capacity(8);
        let a = SmallInt::new(1).oop();
        let b = SmallInt::new(2).oop();
        let c = SmallInt::new(3).oop();
        st.push(a);
        st.push(b);
        st.push(c);
        assert_eq!(st.sp, 3);
        assert_eq!(st.pop(), c);
        assert_eq!(st.pop(), b);
        assert_eq!(st.pop(), a);
        assert_eq!(st.sp, 0);
    }

    #[test]
    fn stack_overflow_via_try_push() {
        let mut st = ProcessStack::with_capacity(2);
        assert!(st.try_push(SmallInt::new(1).oop()).is_ok());
        assert!(st.try_push(SmallInt::new(2).oop()).is_ok());
        assert!(st.try_push(SmallInt::new(3).oop()).is_err());
    }

    #[test]
    fn frame_slot_table() {
        let mut vm = test_vm();
        let method = trivial_method(&mut vm, 2, 2);
        let mut st = ProcessStack::with_capacity(64);

        // Hand-build a frame for argc=2, ntemps=2: push receiver + 2 args,
        // then the fixed slots, then 2 nil temps.
        let recv = SmallInt::new(100).oop();
        let arg0 = SmallInt::new(101).oop();
        let arg1 = SmallInt::new(102).oop();
        st.push(recv); // fp - 3
        st.push(arg0); // fp - 2 == fp - argc + 0
        st.push(arg1); // fp - 1 == fp - argc + 1
        let fp = st.sp;
        st.push(method.oop());
        st.push(SmallInt::new(ENTRY_FRAME_SENTINEL).oop());
        st.push(SmallInt::new(0).oop());
        st.push(vm.universe.nil_obj);
        st.push(recv);
        st.push(SmallInt::new(0).oop()); // serial
        st.push(vm.universe.nil_obj); // marker
        st.push(vm.universe.nil_obj); // temp index 2
        st.push(vm.universe.nil_obj); // temp index 3

        let frame = Frame { fp };
        assert_eq!(frame.temp(&st, 0), arg0);
        assert_eq!(frame.temp(&st, 1), arg1);
        assert_eq!(frame.temp(&st, 2), vm.universe.nil_obj);
        assert_eq!(frame.temp(&st, 3), vm.universe.nil_obj);

        assert_eq!(fp - 3, frame.receiver_slot(&st));
        assert_eq!(fp - 1, frame.temp_index(&st, 1));
        assert_eq!(fp + FRAME_TEMPS_BASE, frame.temp_index(&st, 2));
        assert_eq!(fp + FRAME_TEMPS_BASE + 1, frame.temp_index(&st, 3));
        assert_eq!(frame.serial(&st), 0);
        assert_eq!(frame.marker(&st), vm.universe.nil_obj);
    }

    #[test]
    fn entry_frame_sentinel() {
        let mut st = ProcessStack::with_capacity(16);
        st.push(SmallInt::new(ENTRY_FRAME_SENTINEL).oop());
        assert_eq!(
            SmallInt::try_from(st.get(0)).unwrap().value(),
            ENTRY_FRAME_SENTINEL
        );

        // A nested fake frame's saved_fp round-trips through smi encoding.
        let fake_fp = 12345i64;
        st.push(SmallInt::new(fake_fp).oop());
        assert_eq!(SmallInt::try_from(st.get(1)).unwrap().value(), fake_fp);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "underflow")]
    fn pop_on_empty_operand_stack_panics() {
        let mut vm = test_vm();
        let method = trivial_method(&mut vm, 0, 0);
        let mut st = ProcessStack::with_capacity(64);
        let recv = vm.universe.nil_obj;
        st.push(recv);
        let fp = st.sp;
        st.push(method.oop());
        st.push(SmallInt::new(ENTRY_FRAME_SENTINEL).oop());
        st.push(SmallInt::new(0).oop());
        st.push(vm.universe.nil_obj);
        st.push(recv);
        st.push(SmallInt::new(0).oop()); // serial
        st.push(vm.universe.nil_obj); // marker
        st.activate_frame(fp);
        // No temps, no operand pushes — sp == fp+FRAME_TEMPS_BASE exactly.
        let _ = st.pop();
    }

    #[test]
    fn marker_accessors() {
        let mut vm = test_vm();
        let method = trivial_method(&mut vm, 0, 0);
        let mut st = ProcessStack::with_capacity(64);
        let recv = vm.universe.nil_obj;
        st.push(recv);
        let fp = st.sp;
        st.push(method.oop());
        st.push(SmallInt::new(ENTRY_FRAME_SENTINEL).oop());
        st.push(SmallInt::new(0).oop());
        st.push(vm.universe.nil_obj);
        st.push(recv);
        st.push(SmallInt::new(0xDEAD_u32 as i64).oop()); // serial
        st.push(vm.universe.nil_obj); // marker
        st.activate_frame(fp);

        let frame = Frame { fp };
        assert_eq!(frame.serial(&st), 0xDEAD);
        assert_eq!(frame.marker(&st), vm.universe.nil_obj);

        // A dummy "handler" Oop — set_marker/marker/marker_kind don't care
        // what it actually is, just round-trip it.
        let handler = SmallInt::new(42).oop();
        frame.set_marker(&mut st, handler, MarkerKind::Ensure);
        assert_eq!(frame.marker(&st), handler);
        assert_eq!(frame.marker_kind(&st), MarkerKind::Ensure);
        assert_eq!(
            frame.serial(&st),
            0xDEAD,
            "serial bits must survive set_marker"
        );

        frame.set_marker(&mut st, handler, MarkerKind::IfCurtailed);
        assert_eq!(frame.marker_kind(&st), MarkerKind::IfCurtailed);
        assert_eq!(
            frame.serial(&st),
            0xDEAD,
            "serial bits must survive a kind change"
        );

        frame.clear_marker(&mut st, vm.universe.nil_obj);
        assert_eq!(frame.marker(&st), vm.universe.nil_obj);
        assert_eq!(
            frame.serial(&st),
            0xDEAD,
            "serial bits must survive clear_marker"
        );
    }
}
