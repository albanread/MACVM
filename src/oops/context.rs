//! `ContextOop` field accessors (SPEC §2.3, §5.4, S4). Format `Context`: 1
//! named field (`home_hint`), then the generic size-slot indexable tail
//! `[size: smi][slot…]` (`oops::heap`'s `Format::Context` case). `home_hint`
//! is the static enclosing-scope chain link: `nil` for a method's own
//! Context, or the enclosing `ContextOop` for a block's — `ctx_temp`
//! addressing (SPEC §5.4) walks this chain, one hop per `has_ctx` scope
//! (never per lexical block nesting — a ctx-less block's frame *aliases*
//! its enclosing Context directly, see `interpreter::blocks`).

use crate::oops::layout::CONTEXT_HOME_HINT_INDEX;
use crate::oops::wrappers::ContextOop;
use crate::oops::Oop;

impl ContextOop {
    pub fn home_hint(self) -> Oop {
        self.as_mem().body_oop(CONTEXT_HOME_HINT_INDEX)
    }

    pub fn set_home_hint(self, h: Oop) {
        self.as_mem().set_body_oop(CONTEXT_HOME_HINT_INDEX, h);
    }

    pub fn size(self) -> usize {
        self.as_mem().indexable_len()
    }

    pub fn slot(self, i: usize) -> Oop {
        let n = self.size();
        debug_assert!(i < n, "ContextOop::slot: index {i} out of bounds ({n})");
        self.as_mem().tail_oop_at(i)
    }

    pub fn set_slot(self, i: usize, v: Oop) {
        let n = self.size();
        debug_assert!(i < n, "ContextOop::set_slot: index {i} out of bounds ({n})");
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
            gc_stress_full_period: None,
            eden_kb: None,
            jit: crate::runtime::JitMode::Off,
        })
    }

    #[test]
    fn context_accessors_roundtrip() {
        let mut vm = test_vm();
        let ctx = alloc::alloc_context(&mut vm, 2);
        let nil = vm.universe.nil_obj;
        assert_eq!(ctx.size(), 2);
        assert_eq!(ctx.home_hint(), nil, "home_hint defaults to nil");
        assert_eq!(ctx.slot(0), nil);
        assert_eq!(ctx.slot(1), nil);

        let enclosing = alloc::alloc_context(&mut vm, 1);
        ctx.set_home_hint(enclosing.oop());
        assert_eq!(ctx.home_hint(), enclosing.oop());

        let v = SmallInt::new(99).oop();
        ctx.set_slot(0, v);
        assert_eq!(ctx.slot(0), v);
        assert_eq!(ctx.slot(1), nil);
    }
}
