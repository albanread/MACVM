//! `MethodDictOop` — an open-addressed `(Symbol, CompiledMethod)` table
//! (SPEC §2.4). Format `IndexableOops`: one named field (`tally`), then an
//! indexable `[k0,v0,k1,v1,…]` tail; `capacity = indexable_len()/2`, always
//! a power of two. No tombstones — v1 has no method removal. Growth
//! allocates a new dictionary and rehashes every pair into it; the caller
//! (`runtime::install_method`) is responsible for storing the (possibly
//! new) dictionary back into the klass's `methods` slot immediately.

use crate::oops::klass::Format;
use crate::oops::layout::METHODDICT_TALLY_INDEX;
use crate::oops::smi::SmallInt;
use crate::oops::wrappers::{KlassOop, MemOop, MethodOop, SymbolOop};
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;

#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct MethodDictOop(MemOop);

impl MethodDictOop {
    pub fn try_from(o: Oop) -> Option<MethodDictOop> {
        let m = MemOop::try_from(o)?;
        let fmt = crate::oops::klass::format_of_klass_field(m.klass_oop())?;
        if fmt == Format::IndexableOops {
            Some(MethodDictOop(m))
        } else {
            None
        }
    }

    /// # Safety
    /// Caller guarantees `o` is a MethodDictionary.
    pub unsafe fn from_oop_unchecked(o: Oop) -> MethodDictOop {
        MethodDictOop(MemOop::from_oop_unchecked(o))
    }

    pub fn oop(self) -> Oop {
        self.0.oop()
    }

    pub fn tally(self) -> usize {
        SmallInt::try_from(self.0.body_oop(METHODDICT_TALLY_INDEX))
            .expect("MethodDictOop: tally is not a smi")
            .value() as usize
    }

    fn set_tally(self, n: usize) {
        self.0
            .set_body_oop(METHODDICT_TALLY_INDEX, SmallInt::new(n as i64).oop());
    }

    /// Number of (key,value) slots — half the indexable element count.
    pub fn capacity(self) -> usize {
        self.0.indexable_len() / 2
    }

    fn key_at(self, slot: usize) -> Oop {
        self.0.tail_oop_at(2 * slot)
    }

    fn value_at(self, slot: usize) -> Oop {
        self.0.tail_oop_at(2 * slot + 1)
    }

    fn set_key_at(self, slot: usize, k: Oop) {
        self.0.set_tail_oop_at(2 * slot, k);
    }

    fn set_value_at(self, slot: usize, v: Oop) {
        self.0.set_tail_oop_at(2 * slot + 1, v);
    }

    fn probe_slot(self, nil: Oop, selector: SymbolOop) -> Option<usize> {
        let capacity = self.capacity();
        if capacity == 0 {
            return None;
        }
        let mask = capacity - 1;
        let hash = selector.as_mem().mark().hash();
        debug_assert!(hash != 0, "probe: selector has no eager identity hash");
        let mut i = (hash as usize) & mask;
        let start = i;
        loop {
            let k = self.key_at(i);
            if k.raw() == nil.raw() {
                return None; // empty slot: selector not present
            }
            if k.raw() == selector.oop().raw() {
                return Some(i);
            }
            i = (i + 1) & mask;
            if i == start {
                return None; // table full and selector absent (should not
                             // happen given the 3/4 load-factor growth rule)
            }
        }
    }

    /// The bound method for `selector`, or `None`.
    pub fn probe(self, vm: &VmState, selector: SymbolOop) -> Option<MethodOop> {
        let nil = vm.universe.nil_obj;
        let slot = self.probe_slot(nil, selector)?;
        MethodOop::try_from(self.value_at(slot))
    }

    /// Insert or overwrite `selector -> method`. Returns the dictionary to
    /// use from now on (may be a freshly grown one — the caller MUST store
    /// it into the klass's `methods` slot right away, per the module doc).
    pub fn insert(self, vm: &mut VmState, selector: SymbolOop, method: MethodOop) -> MethodDictOop {
        let nil = vm.universe.nil_obj;

        // Reopening an existing selector overwrites in place — no growth,
        // tally unchanged.
        if let Some(slot) = self.probe_slot(nil, selector) {
            self.set_value_at(slot, method.oop());
            return self;
        }

        let capacity = self.capacity();
        let tally = self.tally();
        let dict = if capacity == 0 || 4 * (tally + 1) > 3 * capacity {
            let new_capacity = (capacity * 2).max(8);
            let grown = alloc_method_dict(vm, new_capacity);
            // `self` (the old dict) stays reachable via the klass's
            // `methods` slot (not yet overwritten) for the duration of this
            // reinsert loop — safe pre-GC; S7 wraps this in a HandleScope.
            for i in 0..self.capacity() {
                let k = self.key_at(i);
                if k.raw() != nil.raw() {
                    let ks = SymbolOop::try_from(k).expect("MethodDictOop: key is not a Symbol");
                    let v = MethodOop::try_from(self.value_at(i))
                        .expect("MethodDictOop: value is not a CompiledMethod");
                    grown.raw_insert_new(nil, ks, v);
                }
            }
            grown.set_tally(tally);
            grown
        } else {
            self
        };

        dict.raw_insert_new(nil, selector, method);
        dict.set_tally(dict.tally() + 1);
        dict
    }

    /// Inserts a KNOWN-absent (selector, method) pair via linear probing.
    /// Does not touch `tally`.
    fn raw_insert_new(self, nil: Oop, selector: SymbolOop, method: MethodOop) {
        let capacity = self.capacity();
        let mask = capacity - 1;
        let hash = selector.as_mem().mark().hash();
        let mut i = (hash as usize) & mask;
        loop {
            if self.key_at(i).raw() == nil.raw() {
                self.set_key_at(i, selector.oop());
                self.set_value_at(i, method.oop());
                return;
            }
            i = (i + 1) & mask;
        }
    }
}

/// Allocates a fresh `capacity`-slot (power-of-two) MethodDictionary, all
/// slots nil (empty), tally 0.
pub fn alloc_method_dict(vm: &mut VmState, capacity: usize) -> MethodDictOop {
    debug_assert!(
        capacity.is_power_of_two(),
        "capacity must be a power of two"
    );
    let klass = vm.universe.methoddict_klass;
    let dict = crate::memory::alloc::alloc_indexable_oops(vm, klass, capacity * 2);
    // SAFETY: freshly allocated with klass's IndexableOops shape.
    let dict = unsafe { MethodDictOop::from_oop_unchecked(dict.oop()) };
    dict.set_tally(0);
    dict
}

pub fn install_method_dictionary(klass: KlassOop, dict: MethodDictOop) {
    klass.set_methods(dict.oop());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::vm_state::{VmOptions, VmState};

    fn test_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
        })
    }

    fn trivial_method(vm: &mut VmState, name: &[u8]) -> MethodOop {
        let mut b = crate::bytecode::BytecodeBuilder::new();
        b.ret_self();
        let sel = vm.universe.intern(name);
        b.finish(vm, sel, 0, 0)
    }

    /// Forces a symbol's eager identity hash to an arbitrary value, so
    /// collision/growth tests don't depend on the real hash function's
    /// distribution.
    fn force_hash(sym: SymbolOop, h: u32) {
        let mem = sym.as_mem();
        mem.set_mark(mem.mark().with_hash(h));
    }

    #[test]
    fn mdict_insert_probe() {
        let mut vm = test_vm();
        let dict = alloc_method_dict(&mut vm, 8);

        let sel_a = vm.universe.intern(b"a");
        let sel_b = vm.universe.intern(b"b");
        let sel_c = vm.universe.intern(b"c");
        let sel_absent = vm.universe.intern(b"absent");
        let m_a = trivial_method(&mut vm, b"a");
        let m_b = trivial_method(&mut vm, b"b");
        let m_c = trivial_method(&mut vm, b"c");

        let dict = dict.insert(&mut vm, sel_a, m_a);
        let dict = dict.insert(&mut vm, sel_b, m_b);
        let dict = dict.insert(&mut vm, sel_c, m_c);

        assert_eq!(dict.probe(&vm, sel_a), Some(m_a));
        assert_eq!(dict.probe(&vm, sel_b), Some(m_b));
        assert_eq!(dict.probe(&vm, sel_c), Some(m_c));
        assert_eq!(dict.probe(&vm, sel_absent), None);
        assert_eq!(dict.tally(), 3);
    }

    #[test]
    fn mdict_collision_chain() {
        let mut vm = test_vm();
        let dict = alloc_method_dict(&mut vm, 8);

        let sel_a = vm.universe.intern(b"a");
        let sel_b = vm.universe.intern(b"b");
        let sel_c = vm.universe.intern(b"c");
        // Force all three selectors into the same home slot (hash & 7 == 0).
        // 0 itself is reserved for "unassigned" (SPEC §2.2), so start at 8.
        force_hash(sel_a, 8);
        force_hash(sel_b, 16);
        force_hash(sel_c, 24);
        let m_a = trivial_method(&mut vm, b"a");
        let m_b = trivial_method(&mut vm, b"b");
        let m_c = trivial_method(&mut vm, b"c");

        let dict = dict.insert(&mut vm, sel_a, m_a);
        let dict = dict.insert(&mut vm, sel_b, m_b);
        let dict = dict.insert(&mut vm, sel_c, m_c);

        assert_eq!(dict.probe(&vm, sel_a), Some(m_a));
        assert_eq!(dict.probe(&vm, sel_b), Some(m_b));
        assert_eq!(dict.probe(&vm, sel_c), Some(m_c));
    }

    #[test]
    fn mdict_growth() {
        let mut vm = test_vm();
        let klass = vm.universe.object_klass;
        let mut dict = alloc_method_dict(&mut vm, 8);
        klass.set_methods(dict.oop());

        // 3/4 of 8 is 6: the 7th distinct insert must trigger growth
        // (4*(tally+1) > 3*capacity first fails at tally=6 -> 4*7=28 > 24).
        let mut selectors = Vec::new();
        let mut methods = Vec::new();
        for i in 0..7 {
            let name = format!("sel{i}");
            let sel = vm.universe.intern(name.as_bytes());
            let m = trivial_method(&mut vm, name.as_bytes());
            dict = dict.insert(&mut vm, sel, m);
            klass.set_methods(dict.oop());
            selectors.push(sel);
            methods.push(m);
        }

        assert_eq!(dict.tally(), 7);
        assert!(dict.capacity() > 8, "capacity must have doubled");
        assert_eq!(
            klass.methods(),
            dict.oop(),
            "klass methods slot must be updated"
        );
        for (sel, m) in selectors.iter().zip(methods.iter()) {
            assert_eq!(dict.probe(&vm, *sel), Some(*m));
        }
    }

    #[test]
    fn mdict_reopen_overwrite() {
        let mut vm = test_vm();
        let dict = alloc_method_dict(&mut vm, 8);
        let sel = vm.universe.intern(b"foo");
        let m1 = trivial_method(&mut vm, b"foo1");
        let m2 = trivial_method(&mut vm, b"foo2");

        let dict = dict.insert(&mut vm, sel, m1);
        assert_eq!(dict.tally(), 1);
        let dict = dict.insert(&mut vm, sel, m2);
        assert_eq!(dict.tally(), 1, "reopening must not bump tally");
        assert_eq!(dict.probe(&vm, sel), Some(m2));
    }

    #[test]
    fn mdict_power_of_two() {
        let mut vm = test_vm();
        let klass = vm.universe.object_klass;
        let mut dict = alloc_method_dict(&mut vm, 4);
        klass.set_methods(dict.oop());
        assert_eq!(dict.capacity(), 4);

        // Insert enough distinct selectors to force growth three times.
        for i in 0..40 {
            let name = format!("s{i}");
            let sel = vm.universe.intern(name.as_bytes());
            let m = trivial_method(&mut vm, name.as_bytes());
            dict = dict.insert(&mut vm, sel, m);
            klass.set_methods(dict.oop());
            assert!(
                dict.capacity().is_power_of_two(),
                "capacity {} not a power of two after inserting {i}",
                dict.capacity()
            );
        }
    }
}
