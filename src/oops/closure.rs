//! `ClosureOop` field accessors (SPEC §2.3, §5.4, S4). Format `Closure`: 2
//! named fields (`method`, `home`), then the generic size-slot indexable
//! tail `[ncopied: smi][copied…]` (`oops::heap`'s `Format::Closure` case).
//!
//! **Pinned `copied[]` convention** (`sprint_s04_detail.md` §Data
//! structures — S5 codegen and every builder test must follow it):
//! `copied[0]` is always the home receiver (`self` inside the block,
//! `ncopied >= 1`); `copied[1]` is the enclosing `ContextOop`, present iff
//! `method.captures_ctx()`; the rest are read-only value captures in the
//! order the compiler pushed them.

use crate::oops::layout::{CLOSURE_HOME_INDEX, CLOSURE_METHOD_INDEX};
use crate::oops::smi::SmallInt;
use crate::oops::wrappers::{ClosureOop, MethodOop};
use crate::oops::Oop;

impl ClosureOop {
    pub fn method(self) -> MethodOop {
        MethodOop::try_from(self.as_mem().body_oop(CLOSURE_METHOD_INDEX))
            .expect("ClosureOop::method: method field is not a CompiledBlock")
    }

    pub fn set_method(self, m: MethodOop) {
        self.as_mem().set_body_oop(CLOSURE_METHOD_INDEX, m.oop());
    }

    /// The packed `HomeRef` smi (SPEC §5.4) — unpack via
    /// `oops::home_ref::unpack_home_ref`.
    pub fn home(self) -> SmallInt {
        SmallInt::try_from(self.as_mem().body_oop(CLOSURE_HOME_INDEX))
            .expect("ClosureOop::home: home field is not a smi")
    }

    pub fn set_home(self, h: SmallInt) {
        self.as_mem().set_body_oop(CLOSURE_HOME_INDEX, h.oop());
    }

    pub fn ncopied(self) -> usize {
        self.as_mem().indexable_len()
    }

    pub fn copied(self, i: usize) -> Oop {
        let n = self.ncopied();
        debug_assert!(i < n, "ClosureOop::copied: index {i} out of bounds ({n})");
        self.as_mem().tail_oop_at(i)
    }

    pub fn set_copied(self, i: usize, v: Oop) {
        let n = self.ncopied();
        debug_assert!(
            i < n,
            "ClosureOop::set_copied: index {i} out of bounds ({n})"
        );
        self.as_mem().set_tail_oop_at(i, v);
    }
}

#[cfg(test)]
mod tests {
    use crate::memory::alloc;
    use crate::oops::smi::SmallInt;
    use crate::runtime::vm_state::{VmOptions, VmState};

    fn test_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            eden_kb: None,
        })
    }

    fn trivial_block(vm: &mut VmState) -> crate::oops::wrappers::MethodOop {
        let mut b = crate::bytecode::BytecodeBuilder::new();
        b.push_self();
        b.ret_tos();
        let sel = vm.universe.intern(b"blk");
        b.finish(vm, sel, 0, 0)
    }

    #[test]
    fn closure_accessors_roundtrip() {
        let mut vm = test_vm();
        let blk = trivial_block(&mut vm);
        let cl = alloc::alloc_closure(&mut vm, 3);
        assert_eq!(cl.ncopied(), 3);

        cl.set_method(blk);
        assert_eq!(cl.method(), blk);

        let home = SmallInt::new(12345);
        cl.set_home(home);
        assert_eq!(cl.home().value(), home.value());

        let nil = vm.universe.nil_obj;
        assert_eq!(cl.copied(0), nil);
        let v = SmallInt::new(7).oop();
        cl.set_copied(1, v);
        assert_eq!(cl.copied(1), v);
        assert_eq!(cl.copied(0), nil);
        assert_eq!(cl.copied(2), nil);
    }
}
