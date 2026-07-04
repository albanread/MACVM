//! `enter_compiled` (S10 D4) — the interpreter's own half of a compiled
//! call: reads receiver+args straight off the process stack (no frame of
//! its own is ever pushed for a compiled activation, D1), invokes the real
//! `call_stub` through [`crate::codecache::stubs::Stubs::invoke`], and
//! either deposits a result exactly like a primitive's direct return
//! (`send::activate_method`'s `PrimResult::Ok` arm) or signals bailout so
//! the caller falls back to a normal interpreted activation. Entirely safe
//! Rust — the one unsafe FFI call this needs lives in `codecache::stubs`
//! instead (that module's own "sole owner of raw MAP_JIT pointer calls"
//! boundary, `codecache::mod`'s doc).

use crate::codecache::nmethod::NmethodId;
use crate::oops::Oop;
use crate::runtime::vm_state::{TierLink, VmState};

use super::push;

/// SPEC §2.1's reserved tag (mem=`0b01`, smi=`0b00`, this=`0b10`, the 4th
/// 2-bit pattern unused) — never a real oop, returned by an inlined smi
/// op's slow path (D1's bailout-by-restart rule) instead of calling back
/// into Rust.
const BAILOUT_SENTINEL: u64 = 0b10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EnterResult {
    /// The compiled call ran to completion; its result is already pushed
    /// onto `vm.stack` in place of `[receiver, args...]` — the caller does
    /// nothing further (identical to `PrimResult::Ok`'s own handling).
    Completed,
    /// Compiled code hit an inlined smi op's slow path. `vm.stack` is
    /// untouched (still `[receiver, args...]`, exactly as the caller left
    /// it) — the caller must fall back to a normal interpreted activation
    /// of the same method, from bci 0 (D1: sound because no observable
    /// effect can precede a bailout).
    Bailout,
    /// S11 D6.3: the compiled call was unwound by a non-local return — the
    /// callee (transitively, through a c2i re-entry) escaped rather than
    /// returning, the compiled frame is already gone (it returned the
    /// `NLR_SENTINEL` through its own ordinary epilogue), and
    /// `enter_compiled` has ALREADY RESUMED the unwind on this (home-ward)
    /// side. The payload is that resumption's outcome; the caller maps it
    /// onto the dispatch loop's own continuation exactly like `OP_NLR_TOS`
    /// does (`Escaped` → keep propagating the sentinel outward;
    /// `ReturnedFromHome(Some(v))` → this dispatch's entry frame returned;
    /// everything else → `vm.regs` is the stamped source of truth).
    Nlr(crate::interpreter::unwind::UnwindStep),
}

/// D4: invokes `nm_id`'s nmethod directly through the real call stub.
/// `argc` must equal the Smalltalk-level arg count the caller's send site
/// used (receiver excluded, matching `MethodOop::argc()`); receiver+args
/// are read from `sp-argc-1 .. sp-1` (SPEC §5.1's pinned convention)
/// without being popped first — nothing here mutates `vm.stack` until the
/// call has actually returned.
///
/// # Panics
/// If `nm_id` is not currently installed — callers (`send::send_generic`'s
/// IC-smi dispatch, `send::activate_method`'s fresh-compile trigger) must
/// only call this with an id they just got back from `CodeTable::install`
/// or already validated via `CodeTable::get`.
pub fn enter_compiled(vm: &mut VmState, nm_id: NmethodId, argc: u8) -> EnterResult {
    // S11 D6.3 (P9-style): no NLR may be in flight when a FRESH compiled
    // call starts — a leaked `nlr_state` from an aborted unwind must be
    // caught here, not silently consumed by this call's own sentinel arm.
    debug_assert!(
        vm.nlr_state.is_none(),
        "enter_compiled: entering compiled code with a parked NLR still in flight"
    );
    let entry = {
        let nm = vm
            .code_table
            .get(nm_id)
            .expect("enter_compiled: nm_id must already be validated as installed");
        nm.code.base as u64 + nm.entry_off as u64
    };

    let argc_usize = argc as usize;
    let base = vm.stack.sp - argc_usize - 1;
    // argv[0] = receiver, argv[1..] = args — Ir::Param{index}'s own
    // convention (ir::convert's entry block), matching what emit.rs's
    // prologue and the call stub's own conditional load agree on.
    let argv: Vec<u64> = (0..=argc_usize)
        .map(|i| vm.stack.get(base + i).raw())
        .collect();

    vm.tier_links.push(TierLink::IntoCompiled {
        interp_frame: vm.stack.fp,
        entry_sp: vm.stack.sp as u64,
        nm_id,
    });
    // S11 D8: the OUTERMOST I→C boundary publishes the interpreter's live
    // eden bump pointer + bounds into `reg_block`, so compiled code's
    // inline-alloc fast path bumps the SAME nursery. A NESTED entry
    // (compiled_depth already > 0) touches nothing: the D8 bridge has
    // frozen eden for the whole window (all Rust allocation under a
    // compiled frame goes old-direct — `alloc::alloc_words`), so
    // `reg_block.eden_top` already reflects every in-window allocation and
    // must not be clobbered back to the frozen `eden.top`.
    if vm.compiled_depth == 0 {
        vm.publish_eden_to_regblock();
    }
    vm.compiled_depth += 1;

    let stubs = vm.stubs;
    let result_bits = stubs.invoke(entry, vm, &argv);

    vm.tier_links.pop();
    vm.compiled_depth -= 1;
    // S11 D8: the OUTERMOST exit reclaims compiled code's bump progress
    // into the interpreter's authoritative `eden.top`. Sound because the
    // bridge froze `eden.top` for the whole window, so `reg_block.eden_top`
    // is the only party that advanced the nursery. Runs on BOTH the
    // completed and bailout paths: a compiled method that allocated and
    // then bailed still produced real eden objects `eden.top` must account
    // for. (Step 9 must run this same adopt + `compiled_depth` fixup on any
    // NLR unwind that crosses a compiled frame — an adversarial-review
    // finding; a stranded `compiled_depth` would freeze eden forever.)
    let outermost_exit = vm.compiled_depth == 0;
    if outermost_exit {
        vm.adopt_eden_from_regblock();
    }

    if result_bits == BAILOUT_SENTINEL {
        // S11 D8 step 10: safe to run a deferred collection right here —
        // `vm.stack` is untouched (still `[receiver, args...]`, this
        // variant's own doc), so nothing is unrooted.
        if outermost_exit {
            vm.run_pending_gc_if_due();
        }
        return EnterResult::Bailout;
    }

    // S11 D6.3: an NLR escaped through the compiled frame we just called —
    // the sentinel propagated back through the frame's own ordinary
    // epilogue (each compiled send site's `sub x17, x0, #NLR_SENTINEL;
    // cbz x17, <epilogue>` check, `emit::emit_nlr_check`), so by the time it reaches
    // here the native frame is already unwound and this function's own
    // depth-decrement/eden-adopt above has already run — no separate native
    // fixup exists or is needed (the sprint doc's original `stub_nlr_unwind`
    // mechanism was unimplementable as written; see the D6.3 SPEC-QUESTION).
    // Resume the suspended unwind HERE, on the home-ward side, where the
    // parked home is one activation closer: the resumption either delivers
    // (home is in this activation), runs a handler, or escapes AGAIN
    // (re-parking `nlr_state`) if yet another compiled frame separates us
    // from home — the caller propagates that exactly like `OP_NLR_TOS`.
    // Checked BEFORE the sp-assert and `Oop::from_raw` below: the sentinel
    // is a RESERVED_TAG word `from_raw` rejects by design, and the escape
    // already popped this side's operand stack.
    //
    // S11 D8 step 10: deliberately NOT a `run_pending_gc_if_due` call site,
    // even when `outermost_exit`. `continue_unwind` can hand back a bare,
    // UNROOTED `Oop` bubbling up as an ordinary return value
    // (`UnwindStep::ReturnedFromHome(Some(_))` — `pop_and_deliver`'s own
    // `ENTRY_FRAME_SENTINEL` case, exactly the shape `nlr_through_compiled_frame`
    // exercises), safe today only because nothing allocates while it's in
    // transit up through `activate_method`/`OP_SEND`/`run_method`'s own
    // return chain — the same pre-existing contract an ordinary (non-NLR)
    // return value already relies on. Running a collection here would be
    // the first thing to ever allocate in that window. Any `gc_pending`
    // request simply waits for the next outermost exit.
    if result_bits == crate::oops::layout::NLR_SENTINEL {
        let st = vm
            .nlr_state
            .take()
            .expect("enter_compiled: NLR sentinel returned but no NlrState is parked");
        return EnterResult::Nlr(crate::interpreter::unwind::continue_unwind(
            vm, st.home, st.value,
        ));
    }

    debug_assert_eq!(
        vm.stack.sp,
        base + argc_usize + 1,
        "enter_compiled: a completed (non-bailout) compiled call must never have touched \
         vm.stack (D1: no allocation, no calls but stub_poll, which itself doesn't touch it)"
    );
    vm.stack.sp = base;
    push(vm, Oop::from_raw(result_bits));
    // S11 D8 step 10: safe here — the result is already pushed onto
    // `vm.stack` (rooted) before a deferred collection could run.
    if outermost_exit {
        vm.run_pending_gc_if_due();
    }
    EnterResult::Completed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oops::smi::SmallInt;
    use crate::oops::wrappers::{ArrayOop, KlassOop, MemOop, MethodOop, SymbolOop};

    /// tests_s10.md: `0b10` fails every typed-wrapper `try_from` — the
    /// defense-in-depth half of this guarantee (`Oop::from_raw`'s own
    /// `debug_assert!` is the primary one: it panics before this bit
    /// pattern can even become an `Oop` through the normal, safe
    /// constructor, which `enter_compiled` itself relies on by checking
    /// `result_bits == BAILOUT_SENTINEL` on the raw `u64` BEFORE ever
    /// calling `Oop::from_raw` on it). `from_raw_unchecked` — documented
    /// as existing "for the mark-word slot and tests" — is what lets this
    /// test construct the value at all.
    #[test]
    fn bailout_sentinel_not_oop() {
        let sentinel = Oop::from_raw_unchecked(BAILOUT_SENTINEL);
        assert!(!sentinel.is_mem(), "0b10 must not read as mem-tagged");
        assert!(
            MemOop::try_from(sentinel).is_none(),
            "MemOop::try_from must reject the bailout sentinel"
        );
        assert!(SmallInt::try_from(sentinel).is_none());
        assert!(KlassOop::try_from(sentinel).is_none());
        assert!(ArrayOop::try_from(sentinel).is_none());
        assert!(MethodOop::try_from(sentinel).is_none());
        assert!(SymbolOop::try_from(sentinel).is_none());
    }

    /// The primary defense: constructing the sentinel through the normal,
    /// safe `Oop` constructor panics outright in a debug build, rather
    /// than silently producing a value the typed wrappers merely happen
    /// to reject.
    #[test]
    #[should_panic(expected = "reserved tag")]
    fn bailout_sentinel_panics_via_normal_from_raw() {
        let _ = Oop::from_raw(BAILOUT_SENTINEL);
    }
}
