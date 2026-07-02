//! The `smalltalk` global namespace (SPEC §3.1: "a Dictionary of
//! Association"). S5's minimal representation: `Universe::smalltalk` is
//! `nil` until the first declaration, then an `ArrayOop` laid out
//! `[tally][assoc-or-nil…]` — dense, append-only, doubling growth. A real
//! `SystemDictionary` (S6, A5) replaces this; every caller goes through
//! `global_lookup`/`global_declare` so that swap is confined to this module.

use crate::memory::alloc;
use crate::oops::smi::SmallInt;
use crate::oops::wrappers::{ArrayOop, MemOop, SymbolOop};
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;

const INITIAL_CAPACITY: usize = 16;

fn assoc_key(assoc: Oop) -> Oop {
    MemOop::try_from(assoc)
        .expect("globals: slot is not an Association")
        .body_oop(0)
}

/// Binds every genesis well-known klass under its own name (SPEC §3.2:
/// `Object subclass: Foo` — and everything else source can name — needs
/// `Object` etc. resolvable as an ordinary global). Called once, right
/// after genesis, by `VmState::with_options`; idempotent (re-running would
/// just no-op via `global_declare`'s existing-lookup fast path), so tests
/// that build a `Universe` directly (bypassing `VmState`) are unaffected.
pub(crate) fn bootstrap_well_known(vm: &mut VmState) {
    let klasses = [
        vm.universe.metaclass_klass,
        vm.universe.class_klass,
        vm.universe.object_klass,
        vm.universe.undefined_object_klass,
        vm.universe.boolean_klass,
        vm.universe.true_klass,
        vm.universe.false_klass,
        vm.universe.smi_klass,
        vm.universe.character_klass,
        vm.universe.double_klass,
        vm.universe.string_klass,
        vm.universe.symbol_klass,
        vm.universe.array_klass,
        vm.universe.bytearray_klass,
        vm.universe.association_klass,
        vm.universe.methoddict_klass,
        vm.universe.method_klass,
        vm.universe.closure_klass,
        vm.universe.context_klass,
        vm.universe.process_klass,
        vm.universe.message_klass,
        vm.universe.large_pos_int_klass,
        vm.universe.large_neg_int_klass,
        vm.universe.behavior_klass,
        vm.universe.magnitude_klass,
        vm.universe.number_klass,
        vm.universe.integer_klass,
        vm.universe.large_integer_klass,
        vm.universe.collection_klass,
        vm.universe.sequenceable_collection_klass,
        vm.universe.arrayed_collection_klass,
        vm.universe.system_dictionary_klass,
    ];
    for k in klasses {
        let name_sym =
            SymbolOop::try_from(k.name()).expect("genesis klass name is always a Symbol");
        let assoc = global_declare(vm, name_sym);
        MemOop::try_from(assoc)
            .expect("global association is a mem oop")
            .set_body_oop(1, k.oop());
    }

    // `Smalltalk` itself (SPEC §3.2 step 3's `Smalltalk startUp` send target)
    // — bound last, once the namespace's own backing array definitely
    // exists (any `global_declare` call above already forced it).
    let smalltalk_sym = vm.universe.intern(b"Smalltalk");
    let assoc = global_declare(vm, smalltalk_sym);
    let smalltalk_obj = vm.universe.smalltalk;
    MemOop::try_from(assoc)
        .expect("global association is a mem oop")
        .set_body_oop(1, smalltalk_obj);
}

/// The bound `Association` for `name`, if declared.
pub fn global_lookup(vm: &VmState, name: SymbolOop) -> Option<Oop> {
    let arr = ArrayOop::try_from(vm.universe.smalltalk)?;
    let tally = SmallInt::try_from(arr.at(0))
        .expect("globals: tally is not a smi")
        .value() as usize;
    for i in 0..tally {
        let assoc = arr.at(1 + i);
        if assoc_key(assoc).raw() == name.oop().raw() {
            return Some(assoc);
        }
    }
    None
}

/// The `Association` for `name`, creating it (value `nil`) if absent.
pub fn global_declare(vm: &mut VmState, name: SymbolOop) -> Oop {
    if let Some(assoc) = global_lookup(vm, name) {
        return assoc;
    }
    ensure_capacity(vm);

    let association_klass = vm.universe.association_klass;
    let assoc = alloc::alloc_slots(vm, association_klass);
    let nil = vm.universe.nil_obj;
    assoc.set_body_oop(0, name.oop());
    assoc.set_body_oop(1, nil);

    let arr = ArrayOop::try_from(vm.universe.smalltalk)
        .expect("globals: smalltalk is not an Array after ensure_capacity");
    let tally = SmallInt::try_from(arr.at(0)).unwrap().value() as usize;
    arr.at_put(1 + tally, assoc.oop());
    arr.at_put(0, SmallInt::new((tally + 1) as i64).oop());
    assoc.oop()
}

/// Grows (or allocates for the first time) so at least one more slot is
/// free past the current tally.
fn ensure_capacity(vm: &mut VmState) {
    // Same `[tally][assoc-or-nil…]` `IndexableOops` layout `array_klass`
    // itself uses — `system_dictionary_klass` just reclassifies the object
    // (SPEC-QUESTION A5: `smalltalk` is a SystemDictionary, not a bare
    // Array), no representation change.
    let sysdict_klass = vm.universe.system_dictionary_klass;
    match ArrayOop::try_from(vm.universe.smalltalk) {
        None => {
            let arr = alloc::alloc_indexable_oops(vm, sysdict_klass, 1 + INITIAL_CAPACITY);
            arr.at_put(0, SmallInt::new(0).oop());
            vm.universe.smalltalk = arr.oop();
        }
        Some(arr) => {
            let tally = SmallInt::try_from(arr.at(0)).unwrap().value() as usize;
            let cap = arr.len() - 1;
            if tally < cap {
                return;
            }
            let new_cap = cap * 2;
            let grown = alloc::alloc_indexable_oops(vm, sysdict_klass, 1 + new_cap);
            let arr = ArrayOop::try_from(vm.universe.smalltalk).unwrap();
            grown.at_put(0, SmallInt::new(tally as i64).oop());
            for i in 0..tally {
                grown.at_put(1 + i, arr.at(1 + i));
            }
            vm.universe.smalltalk = grown.oop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::vm_state::VmOptions;

    fn test_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
        })
    }

    #[test]
    fn declare_then_lookup() {
        let mut vm = test_vm();
        let name = vm.universe.intern(b"Foo");
        assert!(global_lookup(&vm, name).is_none());
        let assoc = global_declare(&mut vm, name);
        assert_eq!(global_lookup(&vm, name), Some(assoc));
        assert_eq!(assoc_key(assoc), name.oop());
        assert_eq!(
            MemOop::try_from(assoc).unwrap().body_oop(1),
            vm.universe.nil_obj
        );
    }

    #[test]
    fn redeclare_is_idempotent() {
        let mut vm = test_vm();
        let name = vm.universe.intern(b"Foo");
        let a1 = global_declare(&mut vm, name);
        let a2 = global_declare(&mut vm, name);
        assert_eq!(a1, a2);
    }

    #[test]
    fn grows_past_initial_capacity() {
        let mut vm = test_vm();
        let mut assocs = Vec::new();
        for i in 0..(INITIAL_CAPACITY * 3) {
            let name = vm.universe.intern(format!("G{i}").as_bytes());
            assocs.push((name, global_declare(&mut vm, name)));
        }
        for (name, assoc) in assocs {
            assert_eq!(global_lookup(&vm, name), Some(assoc));
        }
    }

    #[test]
    fn value_write_persists() {
        let mut vm = test_vm();
        let name = vm.universe.intern(b"Foo");
        let assoc = global_declare(&mut vm, name);
        let m = MemOop::try_from(assoc).unwrap();
        let v = SmallInt::new(42).oop();
        m.set_body_oop(1, v);
        let assoc2 = global_lookup(&vm, name).unwrap();
        assert_eq!(MemOop::try_from(assoc2).unwrap().body_oop(1), v);
    }
}
