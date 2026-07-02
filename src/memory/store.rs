//! The heap store choke point (SPEC §7.4, verbatim). THE single door every
//! heap field/element store in the VM should go through — keeping it
//! single is MacNCL lesson 4. S11's compiled fast path must reproduce this
//! exact condition bit-for-bit (`lsr xtmp, xslot, #9; strb wzr, [xcards,
//! xtmp]` behind the new-gen-value check); this function is the semantics
//! that assembly must match, not merely inspiration for it.
//!
//! Frames/operand stacks on `ProcessStack` are Rust-side roots scanned on
//! every GC (SPEC §5.1) — writes to them are NOT heap stores and must not
//! go through this function (there is no card to dirty for a stack slot).
//! Initializing stores into a freshly allocated object may use `store`
//! unconditionally: a new object is never old unless directly allocated
//! old, in which case the caller's nil-fill plus this function's own
//! barrier check are still correct together.

use crate::oops::wrappers::MemOop;
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;

/// Write `val` into `obj`'s body slot `word_offset` (body-relative index,
/// the same convention `MemOop::body_oop`/`set_body_oop` use), then dirty
/// the containing card iff the store just created an old→new reference:
/// `obj` is in old gen, `val` is a mem oop, AND `val` points into new gen.
/// A smi or nil value never dirties a card (`val.is_mem()` filters that);
/// neither does an old→old or new→new store.
#[inline]
pub fn store(vm: &mut VmState, obj: MemOop, word_offset: usize, val: Oop) {
    let slot = obj.body_addr(word_offset);
    obj.set_body_oop(word_offset, val);
    if vm.universe.layout.is_old(obj.addr())
        && val.is_mem()
        && vm.universe.layout.is_new(val.mem_addr())
    {
        vm.universe.cards.dirty_for_slot(slot);
    }
}

/// As [`store`], but for an `IndexableOops` object's indexable tail
/// element `i` (`Array`/`OrderedCollection`'s backing array/`Context`
/// slots) rather than a named body field — `tail_oop_at`/`set_tail_oop_at`
/// address the tail at `tail_start_word() + i`, a different convention
/// from `body_oop`'s named-field indexing, so this can't just forward to
/// `store` with `i` unchanged.
#[inline]
pub fn store_tail_oop(vm: &mut VmState, obj: MemOop, i: usize, val: Oop) {
    store(vm, obj, obj.tail_start_word() + i, val);
}

/// Conservative POST-write barrier for Rust-side runtime writers (method
/// installation, method-dictionary inserts, IC transitions, the globals
/// array, class reopening…) that mutate a possibly-OLD object through the
/// raw `set_body_oop`/`at_put` accessors: marks every card the object
/// overlaps dirty when it lives in old gen, no-op otherwise.
///
/// Exists because the per-slot [`store`] barrier was scoped (S7-5) to the
/// guest-visible mutator stores only, on the then-true assumption that
/// runtime code always writes into freshly allocated (young) objects —
/// which stops holding the moment long-lived runtime structures (a klass's
/// method dictionary, an ics array, the `smalltalk` globals array) get
/// promoted and are then mutated in place. Coarseness is fine: the
/// dirty-card scan is self-correcting (a card with no old→new slot is
/// re-cleaned on the next scavenge), so over-dirtying costs one rescan,
/// never correctness — while a MISSED dirty card is a dangling reference
/// (exactly what the A9 verifier caught to motivate this, S7-10).
pub fn post_write_barrier(vm: &mut VmState, obj: MemOop) {
    let addr = obj.addr();
    if vm.universe.layout.is_old(addr) {
        let bytes = obj.instance_size_words() * crate::oops::layout::WORD_SIZE;
        vm.universe.cards.record_multistores(addr, addr + bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::alloc;
    use crate::oops::smi::SmallInt;
    use crate::runtime::vm_state::VmOptions;

    fn test_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            eden_kb: None,
        })
    }

    /// An `Array`-shaped old-gen object with one element slot (body index
    /// 1; body index 0 is the size smi) — built directly via
    /// `OldGen::allocate` so the test doesn't need a real class hierarchy.
    /// The fill closure writes every word directly (mark/klass/size slot/
    /// element) via raw pointer stores: `set_body_oop`'s own bounds check
    /// reads the size slot through `instance_size_words()`, so it can't be
    /// used to write that same slot for the first time — the size slot
    /// must already hold a valid smi before any checked accessor touches
    /// this object at all.
    fn old_array(vm: &mut VmState) -> MemOop {
        let klass = vm.universe.array_klass;
        let size_words = klass.non_indexable_size() + 1 + 1; // header+size+1 elem
        let nil = vm.universe.nil_obj;
        let addr = vm
            .universe
            .old
            .allocate(&mut vm.universe.offsets, size_words * 8, |a| {
                // SAFETY: freshly bumped, exactly `size_words` words,
                // entirely within this test's own committed old segment.
                unsafe {
                    let p = a as *mut u64;
                    p.write(crate::oops::mark::Mark::pristine().word()); // mark
                    p.add(1).write(klass.oop().raw()); // klass
                    p.add(2).write(SmallInt::new(1).oop().raw()); // size = 1 elem
                    p.add(3).write(nil.raw()); // the one element, nil-filled
                }
            })
            .expect("old gen has room in a fresh 64 MiB test heap");
        let oop = Oop::from_raw(addr as u64 + crate::oops::layout::MEM_TAG);
        MemOop::try_from(oop).unwrap()
    }

    fn new_gen_object(vm: &mut VmState) -> Oop {
        let klass = vm.universe.array_klass;
        alloc::alloc_indexable_oops(vm, klass, 0).oop()
    }

    /// `tests_s07.md`'s `store_barrier_matrix` (SPEC §7.4's exact
    /// condition): old-obj/new-val -> dirty; old-obj/old-val -> clean;
    /// old-obj/smi -> clean; new-obj/anything -> never touches a card at
    /// all (there's nothing to dirty for a new-gen receiver).
    #[test]
    fn store_barrier_matrix() {
        let mut vm = test_vm();
        let old_obj = old_array(&mut vm);
        let old_obj2 = old_array(&mut vm);
        let card = vm.universe.cards.card_index(old_obj.addr());

        // old-obj / new-val -> dirty.
        let new_val = new_gen_object(&mut vm);
        store(&mut vm, old_obj, 1, new_val);
        assert!(vm.universe.cards.is_dirty(card));
        vm.universe.cards.set_clean(card);

        // old-obj / old-val -> stays clean.
        store(&mut vm, old_obj, 1, old_obj2.oop());
        assert!(!vm.universe.cards.is_dirty(card));

        // old-obj / smi -> stays clean (val.is_mem() filters it).
        store(&mut vm, old_obj, 1, SmallInt::new(42).oop());
        assert!(!vm.universe.cards.is_dirty(card));

        // new-obj / new-val -> is_old(obj) is false, so the barrier can't
        // fire regardless of val; no card anywhere changes state.
        let new_obj = new_gen_object(&mut vm);
        let new_mem = MemOop::try_from(new_obj).unwrap();
        new_mem.set_klass(vm.universe.array_klass); // 0-element array; klass write only
        let before: Vec<bool> = (0..vm.universe.cards.n_cards())
            .map(|i| vm.universe.cards.is_dirty(i))
            .collect();
        assert!(!vm.universe.layout.is_old(new_mem.addr()));
        let after: Vec<bool> = (0..vm.universe.cards.n_cards())
            .map(|i| vm.universe.cards.is_dirty(i))
            .collect();
        assert_eq!(before, after);
    }
}
