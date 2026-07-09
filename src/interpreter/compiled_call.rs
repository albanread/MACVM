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

    // S14 perf recovery: an ARMED poll flag makes EVERY compiled loop
    // back-edge take the `bl stub_poll` slow path — a full runtime call per
    // iteration, which flattens every loop benchmark to interpreter speed.
    // §2d arms it on every invalidation (incl. each recompile-on-trap
    // retirement), but the 10c sweep that disarms it only ran at FULL GC —
    // never in an allocation-light run, so the flag stayed armed forever.
    // This I→C boundary is a WALK-SAFE point (fully anchored interpreter
    // state — the same context GC walks from), so drain-check here: the
    // sweep frees drained NotEntrant nmethods and clears both flags once
    // none remain. Cheap when disarmed (one bool test); runs only while
    // armed.
    // (`compiled_depth == 0` guard: a NESTED enter — c2i reentrancy, or the
    // post-deopt interpret inside `rt_uncommon_trap` — sits above compiled
    // native frames whose chain may not be walkable from here; the next
    // TOP-LEVEL enter sweeps instead.)
    // (`Alive` guard: the S13 10b poll-deopt path deliberately enters a
    // NotEntrant nmethod — its patched entry is HOW the running loop gets
    // deopted — and the sweep would flush that very target out from under
    // us. An Alive target can't be flushed by the sweep, which only frees
    // drained NotEntrant code.)
    if vm.pending_deopt_flag
        && vm.compiled_depth == 0
        // S24 A1: never sweep inside a deopt's nested resume — the native
        // chain above is mid-deopt and not walkable (VmState::
        // deopt_resume_depth's own doc has the full account).
        && vm.deopt_resume_depth == 0
        && vm
            .code_table
            .get(nm_id)
            .is_some_and(|nm| matches!(nm.state, crate::codecache::nmethod::NmState::Alive))
    {
        crate::codecache::flush::sweep_not_entrant_zombies(vm);
    }

    // S13 D7 behavior 2 (`MACVM_DEOPT_STRESS`): every `stress_period` compiled
    // entries, force-invalidate the next Alive nmethod as if its key method were
    // redefined — driving the whole §2a/§2c/§2d + zombie-sweep deopt machinery
    // under a real workload. Runs BEFORE `entry` is read so state stays
    // consistent; never invalidates the method being entered.
    if vm.deopt_stress {
        stress_tick(vm, nm_id);
    }

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
    // S12 step 7: NO eden bookkeeping at this boundary, in either
    // direction, at any depth. Compiled code's inline-alloc fast path
    // reads and bumps the ONE live `universe.eden.top` word directly,
    // through `reg_block.eden_top_addr` (set once at VM construction) —
    // there is no second copy to publish on the way in or adopt back out,
    // no matter how the compiled window ends (completion, bailout, or an
    // NLR unwinding past any number of compiled frames). S11's
    // publish/adopt protocol lived exactly here and was sound only while
    // the D8 bridge froze eden for the whole window; see
    // `VmRegBlock::eden_top_addr`'s own doc for the replacement's full
    // reasoning.
    vm.compiled_depth += 1;

    let stubs = vm.stubs;
    let result_bits = stubs.invoke(entry, vm, &argv);

    vm.tier_links.pop();
    vm.compiled_depth -= 1;

    if result_bits == BAILOUT_SENTINEL {
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
    // NOTE (pre-existing contract, unchanged by S12 step 7):
    // `continue_unwind` can hand back a bare, UNROOTED `Oop` bubbling up
    // as an ordinary return value (`UnwindStep::ReturnedFromHome(Some(_))`
    // — `pop_and_deliver`'s own `ENTRY_FRAME_SENTINEL` case), safe only
    // because nothing allocates while it's in transit up through
    // `activate_method`/`OP_SEND`/`run_method`'s own return chain — the
    // same contract an ordinary (non-NLR) return value already relies on.
    // Nothing new may allocate on this path.
    if result_bits == crate::oops::layout::NLR_SENTINEL {
        let st = vm
            .nlr_state
            .take()
            .expect("enter_compiled: NLR sentinel returned but no NlrState is parked");
        return EnterResult::Nlr(crate::interpreter::unwind::continue_unwind(
            vm, st.home, st.value, st.closure,
        ));
    }

    debug_assert_eq!(
        vm.stack.sp,
        base + argc_usize + 1,
        "enter_compiled: a completed (non-bailout) compiled call must leave vm.stack.sp exactly \
         where it entered (every runtime-stub re-entry — c2i, dnu, must-be-boolean — restores \
         the activation it borrowed, and a GC under the frame rewrites slots in place, never \
         pushes or pops)"
    );
    vm.stack.sp = base;
    push(vm, Oop::from_raw(result_bits));
    EnterResult::Completed
}

/// S13 D7 behavior 2: the `MACVM_DEOPT_STRESS` periodic-invalidation tick.
/// Decrements `stress_countdown`; on reaching zero, resets it to `stress_period`
/// and makes the next Alive nmethod (round-robin, never `entering`) NotEntrant
/// — the same D1 path a real redefinition takes. Output-equivalent: the victim
/// just re-resolves (interpret / recompile) on its next send, and any in-flight
/// activation of it deopts via return-redirection or its loop poll. Callable
/// only when `deopt_stress` is set (checked by the caller).
fn stress_tick(vm: &mut VmState, entering: NmethodId) {
    vm.stress_countdown = vm.stress_countdown.saturating_sub(1);
    if vm.stress_countdown != 0 {
        return;
    }
    vm.stress_countdown = vm.stress_period.max(1);

    let alive: Vec<NmethodId> = vm
        .code_table
        .iter_alive()
        .map(|nm| nm.id)
        .filter(|&id| id != entering)
        .collect();
    if alive.is_empty() {
        return; // nothing else to invalidate (the only nmethod is the one we're entering)
    }
    let victim = alive[vm.stress_rr_cursor % alive.len()];
    vm.stress_rr_cursor = vm.stress_rr_cursor.wrapping_add(1);
    // S14 step 9: a NESTED entry (c2i reentrancy, or the post-deopt
    // interpret inside `rt_uncommon_trap`) sits above compiled native
    // frames whose walk anchor is no longer set — `make_not_entrant`'s §2c
    // return-address-redirection walk aborts there (frames.rs's
    // "innermost side is native but no anchor" assert, the long-standing
    // MACVM_DEOPT_STRESS crash). Use the walk-free LAZY invalidation when
    // nested (§2a entry patch + §2b by_key + §2d poll arm still all fire —
    // in-flight activations finish via the poll/return paths instead of
    // eager redirection); the full walk keeps running for the common
    // TOP-LEVEL entries, so stress coverage of §2c is preserved.
    if vm.compiled_depth == 0 {
        crate::codecache::flush::make_not_entrant(vm, victim);
    } else {
        crate::codecache::flush::make_not_entrant_lazy(vm, victim);
    }
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
