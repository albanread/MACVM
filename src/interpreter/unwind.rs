//! Return interception, non-local return, and `ensure:`/`ifCurtailed:`
//! unwind (SPEC §5.4, S4). `do_return` is the single choke point every
//! return path flows through — `return_tos`, `return_self`,
//! `block_return_tos`, the home-return inside `continue_unwind`, and both
//! resume sentinels. A marker missed on any one path breaks `ensure:`
//! ordering silently (`sprint_s04_detail.md` §Pitfalls).

use crate::oops::home_ref::{pack_home_ref, unpack_home_ref, HomeRef};
use crate::oops::layout::{
    BCI_RESUME_CANNOT_RETURN, BCI_RESUME_ENSURE_RET, BCI_RESUME_UNWIND, ENTRY_FRAME_SENTINEL,
    FRAME_MARKER, FRAME_TEMPS_BASE,
};
use crate::oops::smi::SmallInt;
use crate::oops::wrappers::{ArrayOop, ClosureOop};
use crate::oops::Oop;
use crate::runtime::primitives::PrimResult;
use crate::runtime::vm_state::VmState;

use super::stack::{Frame, MarkerKind};

fn current_frame(vm: &VmState) -> Frame {
    Frame { fp: vm.stack.fp }
}

enum MarkerClass {
    None,
    Handler(ClosureOop),
    Token(ArrayOop),
}

fn marker_class(vm: &VmState, frame: Frame) -> MarkerClass {
    let m = frame.marker(&vm.stack);
    if m.raw() == vm.universe.nil_obj.raw() {
        return MarkerClass::None;
    }
    if let Some(cl) = ClosureOop::try_from(m) {
        return MarkerClass::Handler(cl);
    }
    if let Some(arr) = ArrayOop::try_from(m) {
        return MarkerClass::Token(arr);
    }
    panic!("marker_class: marker is neither nil, a Closure, nor an Array");
}

/// The single return choke point (SPEC §5.4 Algorithm 7). `Some(result)`
/// when the ENTRY frame returns (the dispatch loop should stop); `None`
/// when a caller frame — real or a resume-sentinel destination already
/// fully unwound — is now active (reload method/bci and continue).
pub fn do_return(vm: &mut VmState, result: Oop) -> Option<Oop> {
    let frame = current_frame(vm);
    match marker_class(vm, frame) {
        MarkerClass::None => pop_and_deliver(vm, result),
        MarkerClass::Token(_) => {
            // "A MarkerToken frame returning normally is impossible" (SPEC
            // §5.4): RESUME_UNWIND always clears the marker before this
            // path could be reached on a real return.
            unreachable!("do_return: marker is still an UnwindToken on a normal return")
        }
        MarkerClass::Handler(h) => match frame.marker_kind(&vm.stack) {
            MarkerKind::IfCurtailed => {
                let nil = vm.universe.nil_obj;
                frame.clear_marker(&mut vm.stack, nil);
                pop_and_deliver(vm, result)
            }
            MarkerKind::Ensure => intercept_ensure_return(vm, frame, h, result),
        },
    }
}

/// SPEC §5.4 Algorithm 7's `pop_and_deliver` — S3's original single-frame
/// return: write `result` into the canonical receiver slot, truncate `sp`,
/// restore the caller's fp/bci. If the restored bci is a resume sentinel,
/// dispatches to its resume routine instead of returning to the bytecode
/// fetch loop — which may itself activate a FRESH frame (another handler)
/// rather than truly "resuming" anything, so the dispatch loop cannot
/// assume `None` always means "a caller's own bci". Every `None`-returning
/// path here — the plain caller-resume case AND both resume-sentinel
/// routines' further activations — leaves `vm.regs.method`/`vm.regs.bci`
/// as the single, uniform source of truth for "where dispatch continues
/// next"; callers must read `vm.regs`, never re-derive it from the current
/// frame plus a separately-tracked scratch bci.
fn pop_and_deliver(vm: &mut VmState, result: Oop) -> Option<Oop> {
    let frame = current_frame(vm);
    let argc = frame.method(&vm.stack).argc();
    let base = vm.stack.fp - argc - 1;
    let saved_fp = frame.saved_fp(&vm.stack);
    let saved_bci = frame.saved_bci(&vm.stack);

    vm.stack.set(base, result);
    vm.stack.sp = base + 1;

    if saved_fp == ENTRY_FRAME_SENTINEL {
        vm.stack.sp = base;
        vm.stack.deactivate();
        return Some(result);
    }
    vm.stack.activate_frame(saved_fp as usize);

    match saved_bci {
        BCI_RESUME_ENSURE_RET => resume_ensure_ret(vm),
        BCI_RESUME_UNWIND => resume_unwind(vm),
        BCI_RESUME_CANNOT_RETURN => {
            eprintln!("macvm: resumed from cannotReturn: (VM error) — a #cannotReturn: handler must never return normally");
            let _ = vm.out.flush();
            crate::runtime::vm_state::fatal_exit(1);
        }
        _ => {
            vm.regs.method = Some(current_frame(vm).method(&vm.stack));
            vm.regs.bci = saved_bci;
            None
        }
    }
}

/// Marks the protected block's frame's marker cleared (re-entrancy: a
/// second `do_return` from this same frame — e.g. the handler triggering
/// its own return path back through here — must not intercept again),
/// parks `result` across the handler's activation, and activates the
/// handler with its `saved_bci` forced to `BCI_RESUME_ENSURE_RET`.
fn intercept_ensure_return(
    vm: &mut VmState,
    frame: Frame,
    handler: ClosureOop,
    result: Oop,
) -> Option<Oop> {
    let nil = vm.universe.nil_obj;
    frame.clear_marker(&mut vm.stack, nil);
    vm.stack.push(result); // parked below the handler's own activation
    vm.stack.push(handler.oop());
    vm.regs.bci = BCI_RESUME_ENSURE_RET; // consumed by activate_block as the new frame's saved_bci
    let activated = super::blocks::activate_block(vm, handler, 0);
    debug_assert_eq!(
        activated,
        PrimResult::Activated,
        "intercept_ensure_return: an ensure:/ifCurtailed: handler is always argc=0"
    );
    None
}

/// The handler pushed by `intercept_ensure_return` has just returned
/// normally: discard its result, recover the parked original `result`, and
/// re-run `do_return` — the marker is nil now, so this delivers for real.
fn resume_ensure_ret(vm: &mut VmState) -> Option<Oop> {
    vm.stack.sp -= 1; // discard the handler's own result
    let result = vm.stack.pop(); // the original protected-block result
    do_return(vm, result)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnwindStep {
    /// A handler is now the active frame (`vm.regs` already points at it);
    /// the dispatch loop should reload method/bci (fresh activation, bci 0)
    /// and continue.
    RanHandler,
    /// The home frame delivered normally. `Some(v)` means the entry frame
    /// returned (stop dispatch); `None` means a caller frame is active
    /// (`vm.regs` already reloaded — see `pop_and_deliver`'s doc).
    ReturnedFromHome(Option<Oop>),
    /// `#cannotReturn:` is now the active frame (or the process already
    /// exited via the DNU fallback); the dispatch loop should reload
    /// method/bci (fresh activation, bci 0) and continue.
    CannotReturn,
    /// S11 D6.3: `home` is on the far side of a compiled frame. This whole
    /// interpreter activation (the one a c2i `rt_interpret_call` entered) has
    /// been discarded, `vm.nlr_state` parked; the dispatch loop must return
    /// the `NLR_SENTINEL` so `rt_interpret_call`→the compiled caller→
    /// `enter_compiled` can resume the unwind on the home side.
    Escaped,
}

/// SPEC §5.4 Algorithm 9: walk from the current frame toward `home`,
/// running marked handlers innermost-first. Re-validates `home`'s liveness
/// on *every* entry (a handler may have changed the world before this is
/// re-entered via `RESUME_UNWIND`).
///
/// `originating_closure` (S24 A1): the closure whose `nlr_tos` started this
/// unwind, when the caller has it — the interpreted origination in
/// `dispatch` reads it off the block frame anyway, and COMPILED origination
/// (`rt_nlr_originate`) parks it in `NlrState.closure` — so the dead-home
/// arm can deliver `#cannotReturn:` to the REAL closure instead of reading
/// the current frame's receiver slot. That read is only valid when the
/// current frame IS the originating block's own interpreter frame; with a
/// compiled origination the block's native frame is already gone and the
/// current frame is the value:-sender's (adversarial-review BLOCKER §2).
/// `None` (the `resume_unwind` token path) keeps the legacy current-frame
/// fallback, whose reachable cases are all interpreter-origination shapes.
pub fn continue_unwind(
    vm: &mut VmState,
    home: HomeRef,
    value: Oop,
    originating_closure: Option<Oop>,
) -> UnwindStep {
    if !home_is_live(vm, home) {
        // `Some(step)`: the `cannotReturn:` handler was compiled and itself
        // unwound by a further NLR (see `cannot_return`'s own doc) — the
        // nested unwind's outcome supersedes the plain `CannotReturn`.
        let nested = match originating_closure
            .and_then(|o| crate::oops::wrappers::ClosureOop::try_from(o))
        {
            Some(closure) => cannot_return(vm, closure, value),
            None => cannot_return_current_closure(vm, value),
        };
        return match nested {
            None => UnwindStep::CannotReturn,
            Some(step) => step,
        };
    }
    match innermost_marked_frame(vm, vm.stack.fp, home.fp) {
        ScanResult::ReachedHome => {
            pop_frames_above(vm, home.fp);
            let r = do_return(vm, value);
            UnwindStep::ReturnedFromHome(r)
        }
        ScanResult::CrossedBoundary(entry_fp) => {
            // S11 D6.3: `home` is beyond a c2i boundary. Every armed handler
            // between here and `entry_fp` has already run (the scan reports
            // `Marked` before `CrossedBoundary`), so the whole interpreter
            // activation this c2i entered is now just dead frames. Discard it
            // exactly the way `pop_and_deliver`'s ENTRY_FRAME_SENTINEL path
            // does — `sp` back to the c2i receiver slot, deactivated — but
            // deliver NO value here (it's parked for the home-side resume).
            let entry = Frame { fp: entry_fp };
            let argc = entry.method(&vm.stack).argc();
            let base = entry_fp - argc - 1;
            vm.stack.sp = base;
            vm.stack.deactivate();
            debug_assert!(
                vm.nlr_state.is_none(),
                "continue_unwind: an NLR is escaping while one is already parked"
            );
            vm.nlr_state = Some(crate::runtime::vm_state::NlrState {
                home,
                value,
                closure: originating_closure,
            });
            UnwindStep::Escaped
        }
        ScanResult::Marked(mfp, handler) => {
            pop_frames_above(vm, mfp);
            // `handler` (pulled off the marked frame) and `value` are bare
            // locals held across the token allocation — both need handles
            // (S7-10 GC_STRESS audit). `alloc_unwind_token` protects
            // `value` internally for its own store; this re-read covers
            // the uses AFTER it here.
            let scope = crate::memory::handles::HandleScope::enter(vm);
            let handler_h = scope.handle(vm, handler);
            let token = alloc_unwind_token(vm, home, value);
            let handler = handler_h.get(vm);
            Frame { fp: mfp }.set_marker(&mut vm.stack, token.oop(), MarkerKind::Ensure);
            // `activate_block` reads its receiver-arg slot as `sp - 1` —
            // the closure must actually be on the stack there (matching a
            // plain `value` send's shape), same as `intercept_ensure_return`.
            vm.stack.push(handler.oop());
            vm.regs.bci = BCI_RESUME_UNWIND; // consumed by activate_block as the new frame's saved_bci
            let activated = super::blocks::activate_block(vm, handler, 0);
            debug_assert_eq!(
                activated,
                PrimResult::Activated,
                "continue_unwind: an ensure:/ifCurtailed: handler is always argc=0"
            );
            UnwindStep::RanHandler
        }
    }
}

/// Frame `mfp` (marked with an `UnwindToken`, per `continue_unwind`'s
/// `Some` branch) resumes after its unwind handler returned normally:
/// discard the handler's own result, clear the token, and resume the
/// suspended unwind from exactly where it left off. `mfp` is now unmarked,
/// so a re-entrant `continue_unwind`'s scan proceeds past it (it is popped
/// either by the next handler's `pop_frames_above` or by the final home
/// return).
fn resume_unwind(vm: &mut VmState) -> Option<Oop> {
    vm.stack.sp -= 1; // discard the handler's own result
    let frame = current_frame(vm);
    let token = match marker_class(vm, frame) {
        MarkerClass::Token(t) => t,
        _ => panic!("resume_unwind: frame's marker is not an UnwindToken"),
    };
    let nil = vm.universe.nil_obj;
    frame.clear_marker(&mut vm.stack, nil);
    let home = unpack_home_ref(
        SmallInt::try_from(token.at(0)).expect("resume_unwind: token[0] is not a smi"),
    );
    let value = token.at(1);
    // Token path: the originating closure's identity is not carried in the
    // UnwindToken (home/value only) — the legacy current-frame fallback
    // covers this path's reachable (interpreter-origination) shapes.
    match continue_unwind(vm, home, value, None) {
        UnwindStep::ReturnedFromHome(r) => r,
        UnwindStep::RanHandler | UnwindStep::CannotReturn => None,
        // S11 D6.3: the resumed unwind crossed a c2i boundary — propagate
        // the sentinel up as the "return value" so `dispatch`→`run_method`→
        // `rt_interpret_call` hand it to the compiled caller.
        UnwindStep::Escaped => Some(Oop::from_raw_unchecked(crate::oops::layout::NLR_SENTINEL)),
    }
}

/// SPEC §5.4's dead-home detection: `(proc, bounds, serial)`, checked in
/// that order — bounds BEFORE the serial read, so a shrunken stack never
/// reads garbage; a valid-looking smi at a stale, reused index could
/// otherwise false-positive as live.
pub fn home_is_live(vm: &VmState, home: HomeRef) -> bool {
    if home.proc != 0 {
        return false; // v1: one process; any other proc id names nothing here
    }
    if home.fp + FRAME_MARKER >= vm.stack.sp {
        return false;
    }
    // `home.fp` is in bounds, but that only means SOME activation now
    // occupies that address — not necessarily one shaped like the frame
    // that used to be there (a dead frame's slot can be reused by an
    // unrelated activation whose word at that offset isn't even a smi).
    // Read defensively: anything that doesn't parse as a matching serial
    // is "not live", never a panic.
    match SmallInt::try_from(vm.stack.get(home.fp + crate::oops::layout::FRAME_SERIAL)) {
        Some(s) => {
            let bits = s.value() & crate::oops::layout::FRAME_SERIAL_MASK;
            (bits as u32) == home.serial
        }
        None => false,
    }
}

/// The outcome of scanning the fp chain from `from_fp` toward `home_fp`
/// (S11 D6.3 generalizes the old `Option<(fp, handler)>`).
enum ScanResult {
    /// The innermost armed handler between `from_fp` and `home_fp`.
    Marked(usize, ClosureOop),
    /// `home_fp` was reached with no armed handler in between — the ordinary
    /// same-activation NLR: pop to home and deliver.
    ReachedHome,
    /// An `ENTRY_FRAME_SENTINEL` (a c2i activation base) was reached before
    /// `home_fp`, with no armed handler in between — `home` is on the FAR
    /// side of a compiled frame (S11 D6.3). The `usize` is that entry
    /// frame's own fp. The NLR must ESCAPE this activation and propagate the
    /// sentinel up through the compiled frame(s).
    CrossedBoundary(usize),
}

/// Scans the fp chain from `from_fp` toward `home_fp` for the innermost
/// (closest to `from_fp`) frame with an armed handler marker — `from_fp`
/// itself is included. The marker check precedes the boundary check, so an
/// `ensure:`/`ifCurtailed:` handler ON the c2i entry frame is reported as
/// `Marked` (run first), not swallowed by `CrossedBoundary`.
fn innermost_marked_frame(vm: &VmState, from_fp: usize, home_fp: usize) -> ScanResult {
    let mut fp = from_fp;
    loop {
        if fp == home_fp {
            return ScanResult::ReachedHome;
        }
        if let MarkerClass::Handler(h) = marker_class(vm, Frame { fp }) {
            return ScanResult::Marked(fp, h);
        }
        let saved_fp = Frame { fp }.saved_fp(&vm.stack);
        if saved_fp == ENTRY_FRAME_SENTINEL {
            return ScanResult::CrossedBoundary(fp);
        }
        fp = saved_fp as usize;
    }
}

/// Discards every frame from the current one down to (exclusive of)
/// `target_fp` — no `do_return`, no handler runs; any marker/token an
/// intermediate frame carries is abandoned wholesale (SPEC §5.4's
/// token-abandonment rule: "a frame whose marker is a token is popped
/// without action").
fn pop_frames_above(vm: &mut VmState, target_fp: usize) {
    let mut fp = vm.stack.fp;
    while fp != target_fp {
        let saved_fp = Frame { fp }.saved_fp(&vm.stack);
        debug_assert_ne!(
            saved_fp, ENTRY_FRAME_SENTINEL,
            "pop_frames_above: walked past the entry frame without reaching target_fp {target_fp}"
        );
        fp = saved_fp as usize;
    }
    vm.stack.activate_frame(target_fp);
    let ntemps = Frame { fp: target_fp }.method(&vm.stack).ntemps();
    vm.stack.sp = target_fp + FRAME_TEMPS_BASE + ntemps;
}

/// `[packed_home_ref: smi, return_value: Oop]` (SPEC §5.4). `value` is
/// parked on the operand stack across the allocation (S7 choke-point
/// pattern) — `home`'s smi encoding never needs parking, immediates are
/// never affected by GC.
fn alloc_unwind_token(vm: &mut VmState, home: HomeRef, value: Oop) -> ArrayOop {
    let home_smi = pack_home_ref(home).oop();
    vm.stack.push(value);
    let array_klass = vm.universe.array_klass;
    let token = crate::memory::alloc::alloc_indexable_oops(vm, array_klass, 2);
    let value = vm.stack.pop();
    token.at_put(0, home_smi);
    token.at_put(1, value);
    token
}

/// SPEC §5.4 Algorithm 12. Sends `#cannotReturn:` to the current frame's
/// own closure (whichever `value` activated it) with `value` as the
/// argument; no handler → the S3 DNU fallback prints and terminates
/// (never returns). A handler that itself returns normally is a VM error
/// (`BCI_RESUME_CANNOT_RETURN`, `pop_and_deliver`) — the failed NLR cannot
/// be resumed.
fn cannot_return_current_closure(vm: &mut VmState, value: Oop) -> Option<UnwindStep> {
    let frame = current_frame(vm);
    let closure_oop = vm.stack.get(frame.receiver_slot(&vm.stack));
    let closure = ClosureOop::try_from(closure_oop)
        .expect("cannot_return_current_closure: current frame's receiver slot is not a Closure");
    cannot_return(vm, closure, value)
}

/// Returns `None` in the ordinary case (the `cannotReturn:` handler is now
/// the active frame, `vm.regs` stamped — the caller reports `CannotReturn`).
/// `Some(step)` is S11 D6.3's exotic corner: the handler itself got
/// COMPILED (the S10 trigger fires for full-lookup activations too) and was
/// then unwound by a further NLR from within it — the nested unwind already
/// redirected control, and `step` describes where; the caller propagates it
/// INSTEAD of `CannotReturn` (both belong to the same "reload `vm.regs` and
/// continue" family for every non-return variant, so the mapping is exact,
/// not a compromise).
pub fn cannot_return(vm: &mut VmState, closure: ClosureOop, value: Oop) -> Option<UnwindStep> {
    vm.stack.push(closure.oop());
    vm.stack.push(value);
    let klass = super::send::klass_of(vm, closure.oop());
    let sel = vm.universe.sel_cannot_return;
    match crate::runtime::lookup::lookup(vm, klass, sel) {
        Some(m) => {
            vm.regs.bci = BCI_RESUME_CANNOT_RETURN; // consumed by activate_method as the new frame's saved_bci
            match super::send::activate_method(vm, m, 1, None) {
                super::send::SendOutcome::Normal => None,
                super::send::SendOutcome::Nlr(step) => Some(step),
            }
        }
        None => crate::runtime::error::dnu_fallback(vm, sel, klass), // never returns
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::BytecodeBuilder;
    use crate::oops::wrappers::MethodOop;
    use crate::runtime::lookup::install_method;
    use crate::runtime::vm_state::VmOptions;

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

    fn trivial_method(vm: &mut VmState) -> MethodOop {
        let mut b = BytecodeBuilder::new();
        b.ret_self();
        let sel = vm.universe.intern(b"m");
        b.finish(vm, sel, 0, 0)
    }

    /// Pushes a nil receiver then a frame for `method` whose caller link is
    /// `caller_fp` (or the entry sentinel, for the bottommost frame),
    /// returning the new frame's `fp`.
    fn push_nested_frame(vm: &mut VmState, method: MethodOop, caller_fp: Option<usize>) -> usize {
        let nil = vm.universe.nil_obj;
        vm.stack.push(nil);
        let saved_fp = caller_fp.map_or(ENTRY_FRAME_SENTINEL, |f| f as i64);
        crate::interpreter::push_frame(vm, method, 0, saved_fp, 0);
        vm.stack.fp
    }

    #[test]
    fn marker_classification() {
        let mut vm = test_vm();
        let method = trivial_method(&mut vm);
        let fp = push_nested_frame(&mut vm, method, None);
        let frame = Frame { fp };

        assert!(matches!(marker_class(&vm, frame), MarkerClass::None));

        let handler = crate::memory::alloc::alloc_closure(&mut vm, 1);
        frame.set_marker(&mut vm.stack, handler.oop(), MarkerKind::Ensure);
        assert!(matches!(marker_class(&vm, frame), MarkerClass::Handler(_)));

        let array_klass = vm.universe.array_klass;
        let token = crate::memory::alloc::alloc_indexable_oops(&mut vm, array_klass, 2);
        frame.set_marker(&mut vm.stack, token.oop(), MarkerKind::Ensure);
        assert!(matches!(marker_class(&vm, frame), MarkerClass::Token(_)));
    }

    #[test]
    fn home_dead_bounds() {
        let vm = test_vm();
        let home = HomeRef {
            proc: 0,
            serial: 0,
            fp: vm.stack.sp + 100, // beyond the current top: no read past it
        };
        assert!(!home_is_live(&vm, home));
    }

    #[test]
    fn home_dead_serial() {
        let mut vm = test_vm();
        let method = trivial_method(&mut vm);
        let fp = push_nested_frame(&mut vm, method, None);
        let real_serial = Frame { fp }.serial(&vm.stack);
        let home = HomeRef {
            proc: 0,
            serial: real_serial,
            fp,
        };
        assert!(home_is_live(&vm, home));

        // Pop back to before this frame's receiver, then push a NEW frame
        // at the exact same index: the index is reused, the serial is not.
        vm.stack.sp = fp - 1;
        vm.stack.deactivate();
        let fp2 = push_nested_frame(&mut vm, method, None);
        assert_eq!(fp2, fp, "must reuse the same stack index");
        assert!(
            !home_is_live(&vm, home),
            "a stale serial at a reused index must not read as live"
        );
    }

    #[test]
    fn home_dead_process() {
        let mut vm = test_vm();
        let method = trivial_method(&mut vm);
        let fp = push_nested_frame(&mut vm, method, None);
        let real_serial = Frame { fp }.serial(&vm.stack);
        let home = HomeRef {
            proc: 1, // forged: v1 has exactly one process (id 0)
            serial: real_serial,
            fp,
        };
        assert!(!home_is_live(&vm, home));
    }

    #[test]
    fn innermost_marked_scan() {
        let mut vm = test_vm();
        let method = trivial_method(&mut vm);
        let handler = crate::memory::alloc::alloc_closure(&mut vm, 1);

        let fp1 = push_nested_frame(&mut vm, method, None); // #1, bottom (home)
        let fp2 = push_nested_frame(&mut vm, method, Some(fp1)); // #2
        let fp3 = push_nested_frame(&mut vm, method, Some(fp2)); // #3
        let fp4 = push_nested_frame(&mut vm, method, Some(fp3)); // #4, top (current)

        Frame { fp: fp2 }.set_marker(&mut vm.stack, handler.oop(), MarkerKind::Ensure);
        Frame { fp: fp4 }.set_marker(&mut vm.stack, handler.oop(), MarkerKind::Ensure);

        // Scanning from the top (#4) toward home=#1 (exclusive): #4 itself
        // is checked first (the scan is inclusive of the starting frame).
        let found_fp = match innermost_marked_frame(&vm, fp4, fp1) {
            ScanResult::Marked(fp, _) => fp,
            _ => panic!("must find a marked frame"),
        };
        assert_eq!(found_fp, fp4);

        // With #4 unmarked, the same scan must skip #3 (unmarked) and land
        // on #2 — never on #1 (home is excluded from the scan).
        let nil = vm.universe.nil_obj;
        Frame { fp: fp4 }.clear_marker(&mut vm.stack, nil);
        let found_fp2 = match innermost_marked_frame(&vm, fp4, fp1) {
            ScanResult::Marked(fp, _) => fp,
            _ => panic!("must find #2"),
        };
        assert_eq!(found_fp2, fp2);
    }

    #[test]
    fn token_fields() {
        let mut vm = test_vm();
        let home = HomeRef {
            proc: 0,
            serial: 42,
            fp: 7,
        };
        let value = SmallInt::new(99).oop();
        let token = alloc_unwind_token(&mut vm, home, value);
        let home_smi = SmallInt::try_from(token.at(0)).expect("token[0] must be a smi");
        assert_eq!(unpack_home_ref(home_smi), home);
        assert_eq!(token.at(1), value);
    }

    /// End-to-end sanity check of the core NLR mechanism (no markers in
    /// play yet — those get their own coverage once `ensure:`/`ifCurtailed:`
    /// exist): `run` builds `[^42]` and sends it `value` directly. The NLR
    /// must skip `run`'s own subsequent bytecode entirely and deliver 42 as
    /// `run`'s own return value.
    #[test]
    fn nlr_direct_to_home() {
        let mut vm = test_vm();
        let closure_klass = vm.universe.closure_klass;
        let value_sel = vm.universe.intern(b"value");
        let mut value_body = BytecodeBuilder::new();
        value_body.push_self();
        value_body.ret_self();
        let value_method = value_body.finish(&mut vm, value_sel, 0, 0);
        value_method.set_primitive(50);
        crate::runtime::lookup::install_method(&mut vm, closure_klass, value_sel, value_method);

        let mut home = BytecodeBuilder::new();
        let lit = home.build_block(&mut vm, 0, 0, false, 0, false, |b, _vm| {
            b.push_smi_i8(42);
            b.nlr_tos();
        });
        home.push_closure(lit, 0);
        home.send(&mut vm, value_sel, 0);
        home.push_smi_i8(99); // must never be reached
        home.ret_tos();
        let sel = vm.universe.intern(b"run");
        let method = home.finish(&mut vm, sel, 0, 0);

        let recv = vm.universe.nil_obj;
        let result = crate::interpreter::run_method(&mut vm, method, recv, &[]);
        assert_eq!(result, SmallInt::new(42).oop());
    }

    // --- ensure:/ifCurtailed: golden-style sanity checks --------------------

    fn string_literal(vm: &mut VmState, s: &str) -> Oop {
        let bytearray_klass = vm.universe.bytearray_klass;
        let b = crate::memory::alloc::alloc_indexable_bytes(vm, bytearray_klass, s.len());
        for (i, byte) in s.bytes().enumerate() {
            b.byte_at_put(i, byte);
        }
        b.oop()
    }

    /// Installs `value` (50), `ensure:` (60), `ifCurtailed:` (61) on
    /// `closure_klass`, and `print:` (91, `printOnStdout:`) on
    /// `object_klass` — everything the ensure:/ifCurtailed: goldens need.
    fn install_block_prims(vm: &mut VmState) {
        let closure_klass = vm.universe.closure_klass;
        let object_klass = vm.universe.object_klass;

        let mk = |vm: &mut VmState, name: &[u8], argc: usize, prim: i64| {
            let mut b = BytecodeBuilder::new();
            b.push_self();
            b.ret_self(); // unreachable: these primitives never Fail in these tests
            let sel = vm.universe.intern(name);
            let m = b.finish(vm, sel, argc, 0);
            m.set_primitive(prim);
            (sel, m)
        };

        let (sel, m) = mk(vm, b"value", 0, 50);
        install_method(vm, closure_klass, sel, m);
        let (sel, m) = mk(vm, b"ensure:", 1, 60);
        install_method(vm, closure_klass, sel, m);
        let (sel, m) = mk(vm, b"ifCurtailed:", 1, 61);
        install_method(vm, closure_klass, sel, m);
        let (sel, m) = mk(vm, b"print:", 1, 91);
        install_method(vm, object_klass, sel, m);
    }

    /// `[T print: 'body'. 7] ensure: [T print: 'cleanup']` — handler runs
    /// *after* the body, *before* the result is delivered; the delivered
    /// value is the body's own (7), the handler's own result is discarded.
    #[test]
    fn ensure_normal_completion() {
        let mut vm = test_vm();
        install_block_prims(&mut vm);
        let print_sel = vm.universe.intern(b"print:");
        let ensure_sel = vm.universe.intern(b"ensure:");
        let body_str = string_literal(&mut vm, "body");
        let cleanup_str = string_literal(&mut vm, "cleanup");

        let mut home = BytecodeBuilder::new();
        let protected_lit = home.build_block(&mut vm, 0, 0, false, 0, false, |b, vm| {
            b.push_self();
            b.push_literal(vm, body_str);
            b.send(vm, print_sel, 1);
            b.pop();
            b.push_smi_i8(7);
            b.ret_tos();
        });
        let handler_lit = home.build_block(&mut vm, 0, 0, false, 0, false, |b, vm| {
            b.push_self();
            b.push_literal(vm, cleanup_str);
            b.send(vm, print_sel, 1);
            b.ret_self();
        });
        home.push_closure(protected_lit, 0);
        home.push_closure(handler_lit, 0);
        home.send(&mut vm, ensure_sel, 1);
        home.ret_tos();
        let sel = vm.universe.intern(b"run");
        let method = home.finish(&mut vm, sel, 0, 0);

        let buf = crate::runtime::vm_state::OutputBuffer::new();
        vm.out = Box::new(buf.clone());
        let recv = vm.universe.nil_obj;
        let result = crate::interpreter::run_method(&mut vm, method, recv, &[]);
        assert_eq!(result, SmallInt::new(7).oop());
        assert_eq!(buf.as_string(), "bodycleanup");
    }

    /// Gate 3: `run = [[^42] ensure: [T print: 'inner']] ensure: [T print:
    /// 'outer']`, NLR from the innermost block whose home is `run`. Two
    /// marked frames sit between the NLR site and home; handlers must run
    /// innermost-first, and `run`'s caller must see the delivered value.
    #[test]
    fn nlr_two_frames_ensure_order() {
        let mut vm = test_vm();
        install_block_prims(&mut vm);
        let print_sel = vm.universe.intern(b"print:");
        let ensure_sel = vm.universe.intern(b"ensure:");
        let run_sel = vm.universe.intern(b"run");
        let inner_str = string_literal(&mut vm, "inner");
        let outer_str = string_literal(&mut vm, "outer");
        let done_str = string_literal(&mut vm, "done");

        // Innermost blocks first, standalone (nested `build_block` calls
        // can't share one `&mut VmState` borrow — see
        // `BytecodeBuilder::intern_block_literal`'s doc).
        let handler_inner_blk =
            crate::bytecode::build_standalone_block(&mut vm, 0, 0, false, 0, false, |h, vm| {
                h.push_self();
                h.push_literal(vm, inner_str);
                h.send(vm, print_sel, 1);
                h.ret_self();
            });
        let nlr_blk =
            crate::bytecode::build_standalone_block(&mut vm, 0, 0, false, 0, false, |b, _vm| {
                b.push_smi_i8(42);
                b.nlr_tos();
            });

        let mut run_b = BytecodeBuilder::new();
        // The inner-most block's home propagates from L_outer's own closure
        // (L_outer is itself a block created directly in `run`, so ITS home
        // is `run`) — this is the classic NLR-propagation case.
        let outer_lit = run_b.build_block(&mut vm, 0, 0, false, 0, false, |lo, vm| {
            let handler_inner_lit = lo.intern_block_literal(vm, handler_inner_blk);
            let nlr_lit = lo.intern_block_literal(vm, nlr_blk);
            lo.push_closure(nlr_lit, 0);
            lo.push_closure(handler_inner_lit, 0);
            lo.send(vm, ensure_sel, 1);
            lo.ret_tos(); // unreachable: the NLR above always fires first
        });
        let handler_outer_lit = run_b.build_block(&mut vm, 0, 0, false, 0, false, |h, vm| {
            h.push_self();
            h.push_literal(vm, outer_str);
            h.send(vm, print_sel, 1);
            h.ret_self();
        });
        run_b.push_closure(outer_lit, 0);
        run_b.push_closure(handler_outer_lit, 0);
        run_b.send(&mut vm, ensure_sel, 1);
        run_b.ret_tos(); // unreachable: home receives the NLR directly
        let run_method_oop = run_b.finish(&mut vm, run_sel, 0, 0);
        let object_klass = vm.universe.object_klass;
        install_method(&mut vm, object_klass, run_sel, run_method_oop);

        let mut driver = BytecodeBuilder::new();
        driver.push_self();
        driver.send(&mut vm, run_sel, 0);
        driver.store_temp_pop(0);
        driver.push_self();
        driver.push_literal(&mut vm, done_str);
        driver.send(&mut vm, print_sel, 1);
        driver.pop();
        driver.push_temp(0);
        driver.ret_tos();
        let driver_sel = vm.universe.intern(b"driver");
        let driver_method = driver.finish(&mut vm, driver_sel, 0, 1);

        let buf = crate::runtime::vm_state::OutputBuffer::new();
        vm.out = Box::new(buf.clone());
        let recv = crate::memory::alloc::alloc_slots(&mut vm, object_klass).oop();
        let result = crate::interpreter::run_method(&mut vm, driver_method, recv, &[]);
        assert_eq!(result, SmallInt::new(42).oop());
        assert_eq!(
            buf.as_string(),
            "innerouterdone",
            "handlers must run innermost-first, before the NLR value is delivered"
        );
    }

    /// `ifCurtailed:`'s handler runs on unwind (NLR) but is skipped on
    /// normal completion — the opposite of `ensure:`.
    #[test]
    fn if_curtailed_both_paths() {
        let if_curtailed_sel_and_handler = |vm: &mut VmState, do_nlr: bool| -> (Oop, String) {
            install_block_prims(vm);
            let if_curtailed_sel = vm.universe.intern(b"ifCurtailed:");
            let print_sel = vm.universe.intern(b"print:");
            let marker_str = string_literal(vm, "curtailed");

            let handler_lit =
                crate::bytecode::build_standalone_block(vm, 0, 0, false, 0, false, |h, vm| {
                    h.push_self();
                    h.push_literal(vm, marker_str);
                    h.send(vm, print_sel, 1);
                    h.ret_self();
                });

            let mut run_b = BytecodeBuilder::new();
            let protected_lit = run_b.build_block(vm, 0, 0, false, 0, false, |b, _vm| {
                if do_nlr {
                    b.push_smi_i8(1);
                    b.nlr_tos();
                } else {
                    b.push_smi_i8(7);
                    b.ret_tos();
                }
            });
            run_b.push_closure(protected_lit, 0);
            let hl = run_b.intern_block_literal(vm, handler_lit);
            run_b.push_closure(hl, 0);
            run_b.send(vm, if_curtailed_sel, 1);
            run_b.ret_tos();
            let run_sel = vm.universe.intern(b"run");
            let method = run_b.finish(vm, run_sel, 0, 0);

            let buf = crate::runtime::vm_state::OutputBuffer::new();
            vm.out = Box::new(buf.clone());
            let recv = vm.universe.nil_obj;
            let result = crate::interpreter::run_method(vm, method, recv, &[]);
            (result, buf.as_string())
        };

        let mut vm = test_vm();
        let (result, transcript) = if_curtailed_sel_and_handler(&mut vm, false);
        assert_eq!(result, SmallInt::new(7).oop());
        assert_eq!(transcript, "", "normal completion must NOT run the handler");

        let mut vm2 = test_vm();
        let (result2, transcript2) = if_curtailed_sel_and_handler(&mut vm2, true);
        assert_eq!(result2, SmallInt::new(1).oop());
        assert_eq!(
            transcript2, "curtailed",
            "an NLR (curtailment) must run the handler"
        );
    }
}
