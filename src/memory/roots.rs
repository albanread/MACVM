//! Single point of truth for GC root enumeration (SPEC §7.3 step 1, §7.5
//! steps 1 and 3). Every Rust-side source of live oops outside the heap
//! itself, visited in one fixed order by [`for_each_root`].
//!
//! S7's scavenger originally hand-duplicated this as five separate
//! `scavenge_*_roots` functions in `scavenge.rs`. S8's full GC needs the
//! IDENTICAL root set for two more passes (mark, reference-rewrite) with two
//! more transforms — keeping three hand-written copies in sync by hand is
//! exactly the shape of bug this project spent S7-9 through S7-11 chasing
//! (a root source present in one collector's list but not another's dangles
//! silently until something reads through it). One list, every collector.
//!
//! Deliberately NOT included: the dirty-card scan (old→new remembered-set
//! edges). That is scavenge-specific bookkeeping, not a root source — a full
//! GC traces the whole heap directly and ignores the card table entirely for
//! marking (SPEC §7.5: "old gen is traced, not card-scanned"). It stays in
//! `scavenge.rs`, called separately after `for_each_root`.

use crate::oops::wrappers::{KlassOop, MethodOop, SymbolOop};
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;

/// Visits every live root oop, replacing each in place with `f`'s result.
/// Order: well-known singleton oops, well-known selectors, well-known
/// klasses, symbol table, process stack, handle arena, interpreter regs
/// mirror — matching S7's original scan order exactly (some tests pin
/// observable scavenge behavior against it).
///
/// `f` takes `&mut VmState` because a scavenge's transform (`scavenge_oop`)
/// needs it for `to`-space bookkeeping; a full GC's mark-push and
/// forward-chase transforms don't touch `vm`, but share the signature so one
/// generic walker serves all three collector-supplied transforms.
pub fn for_each_root<F>(vm: &mut VmState, mut f: F)
where
    F: FnMut(&mut VmState, Oop) -> Oop,
{
    // --- well-known singleton oops -----------------------------------------
    let o = vm.universe.nil_obj;
    vm.universe.nil_obj = f(vm, o);
    let o = vm.universe.true_obj;
    vm.universe.true_obj = f(vm, o);
    let o = vm.universe.false_obj;
    vm.universe.false_obj = f(vm, o);
    let o = vm.universe.smalltalk;
    vm.universe.smalltalk = f(vm, o);

    // --- well-known selectors (separate Universe-field copies of Symbols
    // already covered by the symbol-table root below — S7-10 root-scan gap:
    // these dangle on their own after the first collection otherwise) ------
    //
    // Rewrapped via `from_oop_unchecked`, not the validating `try_from`
    // (found via a genuine full-GC bug running real source, not a defensive
    // guess): `try_from` confirms shape by reading the CANDIDATE'S OWN
    // klass field's format — a second hop. For the scavenger's transform
    // that's harmless (an eagerly-copied object's new address is already
    // fully populated), but for full GC's phase C the transform is
    // forward-chase, which can return an address phase D hasn't copied
    // ANY bytes to yet — reading through it is invariant F3's exact trap,
    // one more call site than `fullgc::rewrite_entry` already covered.
    // The unchecked cast is sound regardless: this root was symbol/klass/
    // method-shaped before the collection, and a collector transform never
    // changes an object's TYPE, only (at most) its address — re-deriving
    // that already-known shape by reading memory is pure redundancy that
    // happens to be unsafe in exactly this one case.
    macro_rules! root_sel {
        ($($field:ident),* $(,)?) => {
            $(
                let s = vm.universe.$field.oop();
                let ns = f(vm, s);
                // SAFETY: see the comment above this macro.
                vm.universe.$field = unsafe { SymbolOop::from_oop_unchecked(ns) };
            )*
        };
    }
    root_sel!(
        sel_does_not_understand,
        sel_must_be_boolean,
        sel_cannot_return
    );

    // --- well-known klasses --------------------------------------------------
    macro_rules! root_klass {
        ($($field:ident),* $(,)?) => {
            $(
                let k = vm.universe.$field.oop();
                let nk = f(vm, k);
                // SAFETY: see root_sel!'s comment above — identical reasoning.
                vm.universe.$field = unsafe { KlassOop::from_oop_unchecked(nk) };
            )*
        };
    }
    root_klass!(
        metaclass_klass,
        class_klass,
        object_klass,
        undefined_object_klass,
        boolean_klass,
        true_klass,
        false_klass,
        smi_klass,
        character_klass,
        double_klass,
        string_klass,
        symbol_klass,
        array_klass,
        bytearray_klass,
        association_klass,
        methoddict_klass,
        method_klass,
        closure_klass,
        context_klass,
        process_klass,
        message_klass,
        large_pos_int_klass,
        large_neg_int_klass,
        behavior_klass,
        magnitude_klass,
        number_klass,
        integer_klass,
        large_integer_klass,
        collection_klass,
        sequenceable_collection_klass,
        arrayed_collection_klass,
        system_dictionary_klass,
    );

    // --- symbol table (probed by content hash — bucket positions never
    // depend on address, so this is an in-place UPDATE for every collector,
    // never a flush; see fullgc.rs's phase-C table for why that matters) ----
    let n = vm.universe.symbols.buckets.len();
    for i in 0..n {
        if let Some(sym) = vm.universe.symbols.buckets[i] {
            vm.universe.symbols.buckets[i] = Some(f(vm, sym));
        }
    }

    // --- process stack: every live slot 0..sp. Smi-encoded saved_fp/bci
    // links pass through any of the three transforms unchanged (none of
    // scavenge_oop/mark-push/forward-chase touch a non-mem oop) — SPEC
    // §5.1's exact-stack invariant is what makes this scan format-free. ----
    let sp = vm.stack.sp;
    for i in 0..sp {
        let v = vm.stack.get(i);
        let nv = f(vm, v);
        vm.stack.set(i, nv);
    }

    // --- handle arena: every slot 0..len (SPEC §7.6). Never truncated by
    // any collector, only by `HandleScope::drop` — rewritten in place. -----
    let n = vm.handle_arena.len();
    let dbg = std::env::var("MACVM_DBG_ROOTS").is_ok();
    for i in 0..n {
        let v = vm.handle_arena.slots_mut()[i];
        if dbg {
            eprintln!("RDBG root handle[{i}] = {:#x}", v.raw());
        }
        let nv = f(vm, v);
        vm.handle_arena.slots_mut()[i] = nv;
    }

    // --- interpreter regs mirror: `vm.regs.method`, a second copy of the
    // executing frame's method the process-stack scan doesn't reach
    // (S7-10) --------------------------------------------------------------
    if let Some(m) = vm.regs.method {
        let nm = f(vm, m.oop());
        // SAFETY: see root_sel!'s comment above (same reasoning) — found by
        // this EXACT root, running `Smalltalk gcFull` from real source with
        // a genuine executing frame: the synthetic fullgc.rs unit tests
        // never set vm.regs.method, so they could never have caught this.
        vm.regs.method = Some(unsafe { MethodOop::from_oop_unchecked(nm) });
    }
}
