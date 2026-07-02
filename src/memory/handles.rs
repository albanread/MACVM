//! Handles (SPEC §7.6): scoped, GC-safe storage for an oop held across a
//! call that might allocate (and therefore might scavenge, moving
//! everything). A bare Rust local holding an `Oop` across such a call is
//! an invisible root — MacNCL lesson 13 — the scavenger has no way to
//! find and rewrite it, so it silently dangles the moment a scavenge runs.
//!
//! **Deliberate simplification from the sprint doc's exact API.** The doc
//! sketches `HandleScope<'vm>`/`Handle<'s, T>` with lifetime parameters
//! tying handles to their scope. Literally implementing that alongside the
//! doc's own `fn handle(&self, vm: &mut VmState, val: T) -> Handle<'_, T>`
//! signature doesn't typecheck: if `HandleScope::enter(vm: &mut VmState)`
//! ties its return lifetime to `vm`'s borrow (the only lifetime Rust's
//! elision rules have to work with), `vm` stays mutably borrowed for the
//! scope's entire lifetime, and no subsequent call can pass `&mut vm`
//! again — including `.handle()` itself. The doc's own "scopes are
//! strictly LIFO (debug-checked)" phrasing already signals a runtime
//! check, not a compile-time one, so this drops the lifetime parameters
//! and relies on: (1) Rust's natural lexical drop order giving LIFO
//! scope-nesting for the common `let scope = HandleScope::enter(vm);
//! ...; /* drop */` pattern, same as any other RAII guard; (2) a
//! `debug_assert` at truncation time as a sanity net. A `Handle` used
//! after its originating scope has dropped reads whatever the arena slot
//! now holds (documented misuse, not a soundness hole GC itself could
//! trip on — the arena's OWN slots are always valid oops or the shared
//! `nil`, since truncation never reads-then-lies, only shrinks `len`).

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::oops::method_dict::MethodDictOop;
use crate::oops::wrappers::{
    ArrayOop, ByteArrayOop, ClosureOop, ContextOop, DoubleOop, KlassOop, MemOop, MethodOop,
    SymbolOop,
};
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;

/// Implemented by `Oop` and every typed wrapper — the value types a
/// `Handle<T>` can hold.
pub trait OopRepr: Copy {
    fn as_oop(self) -> Oop;
    /// # Safety
    /// Caller guarantees `o` is actually shaped as `Self` (the arena
    /// slot was last written through a `Handle<Self>`, or `Self` is the
    /// untyped `Oop` itself).
    unsafe fn from_oop_unchecked(o: Oop) -> Self;
}

macro_rules! oop_repr {
    ($t:ty) => {
        impl OopRepr for $t {
            fn as_oop(self) -> Oop {
                self.oop()
            }
            unsafe fn from_oop_unchecked(o: Oop) -> Self {
                // SAFETY: forwarded to the caller's own contract.
                unsafe { <$t>::from_oop_unchecked(o) }
            }
        }
    };
}

impl OopRepr for Oop {
    fn as_oop(self) -> Oop {
        self
    }
    unsafe fn from_oop_unchecked(o: Oop) -> Self {
        o
    }
}
oop_repr!(MemOop);
oop_repr!(KlassOop);
oop_repr!(ArrayOop);
oop_repr!(ByteArrayOop);
oop_repr!(SymbolOop);
oop_repr!(MethodOop);
oop_repr!(ClosureOop);
oop_repr!(ContextOop);
oop_repr!(DoubleOop);
oop_repr!(MethodDictOop);

/// The backing store: a flat, append-only (until scope-drop truncates it)
/// `Vec<Oop>`. Boxed inside `VmState` (`VmState::handle_arena`) so its own
/// address is stable across `Vec` growth — `HandleScope` points at the
/// `HandleArena` struct itself (fixed-size, never reallocated), not at the
/// `Vec`'s backing storage (which does move on growth; every access goes
/// through `vm.handle_arena`, never a cached slice).
#[derive(Default)]
pub struct HandleArena {
    slots: Vec<Oop>,
}

impl HandleArena {
    pub fn new() -> HandleArena {
        HandleArena::default()
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// GC root-scan entry point (SPEC §7.6): every live slot gets
    /// scavenged and rewritten in place. The arena is NEVER truncated by
    /// GC (only `HandleScope::drop` does that) — lesson 9.
    pub fn slots_mut(&mut self) -> &mut [Oop] {
        &mut self.slots
    }
}

/// A LIFO scope: every `Handle` pushed while it's alive is truncated away
/// when it drops. Nest scopes the same way you'd nest any other RAII
/// guard (`let outer = HandleScope::enter(vm); { let inner =
/// HandleScope::enter(vm); ... } // inner's handles gone here`).
pub struct HandleScope {
    arena: NonNull<HandleArena>,
    saved_len: usize,
}

impl HandleScope {
    pub fn enter(vm: &mut VmState) -> HandleScope {
        let saved_len = vm.handle_arena.len();
        let arena = NonNull::from(vm.handle_arena.as_mut());
        HandleScope { arena, saved_len }
    }

    /// Push `val` into the arena; returns a `Handle` valid until this
    /// scope drops. `vm` is passed per call (the scope itself holds no
    /// live borrow of it) so allocation calls can interleave between
    /// `handle()` calls.
    pub fn handle<T: OopRepr>(&self, vm: &mut VmState, val: T) -> Handle<T> {
        debug_assert!(
            std::ptr::eq(
                self.arena.as_ptr(),
                vm.handle_arena.as_mut() as *mut HandleArena
            ),
            "HandleScope used with a different VmState than it was entered from"
        );
        let index = vm.handle_arena.slots.len() as u32;
        vm.handle_arena.slots.push(val.as_oop());
        Handle {
            index,
            _t: PhantomData,
        }
    }
}

impl Drop for HandleScope {
    fn drop(&mut self) {
        // SAFETY: `arena` points at `vm.handle_arena`'s heap allocation
        // (stable address inside its `Box`, per the struct's own doc);
        // `vm.handle_arena` — and therefore this pointee — outlives every
        // `HandleScope`, which are stack-local RAII guards nested no
        // deeper than the call stack that created them.
        let arena = unsafe { self.arena.as_mut() };
        debug_assert!(
            self.saved_len <= arena.slots.len(),
            "HandleScope dropped out of LIFO order (arena shrank below its own saved_len \
             before this scope's own drop ran)"
        );
        arena.slots.truncate(self.saved_len);
    }
}

/// An index into the handle arena — never a pointer, so `Vec` growth
/// (reallocating the backing storage) never invalidates it.
#[derive(Copy, Clone)]
pub struct Handle<T> {
    index: u32,
    _t: PhantomData<T>,
}

impl<T: OopRepr> Handle<T> {
    /// Always re-read after any call that can allocate — the arena slot
    /// may have been rewritten in place by an intervening scavenge.
    pub fn get(self, vm: &VmState) -> T {
        let o = vm.handle_arena.slots[self.index as usize];
        // SAFETY: this slot was populated by `HandleScope::handle::<T>`
        // (or later overwritten by `set::<T>`) — always `T`-shaped.
        unsafe { T::from_oop_unchecked(o) }
    }

    pub fn set(self, vm: &mut VmState, val: T) {
        vm.handle_arena.slots[self.index as usize] = val.as_oop();
    }

    pub fn oop(self, vm: &VmState) -> Oop {
        vm.handle_arena.slots[self.index as usize]
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

    /// `tests_s07.md`'s `handle_scope_lifo`: nested scopes push/truncate
    /// the arena correctly; handles read/write through vm.
    #[test]
    fn handle_scope_lifo() {
        let mut vm = test_vm();
        assert_eq!(vm.handle_arena.len(), 0);

        let outer = HandleScope::enter(&mut vm);
        let h1 = outer.handle(&mut vm, SmallInt::new(1).oop());
        assert_eq!(vm.handle_arena.len(), 1);
        {
            let inner = HandleScope::enter(&mut vm);
            let h2 = inner.handle(&mut vm, SmallInt::new(2).oop());
            let h3 = inner.handle(&mut vm, SmallInt::new(3).oop());
            assert_eq!(vm.handle_arena.len(), 3);
            assert_eq!(h2.get(&vm).raw(), SmallInt::new(2).oop().raw());
            assert_eq!(h3.get(&vm).raw(), SmallInt::new(3).oop().raw());
        }
        // inner dropped: truncated back to outer's length.
        assert_eq!(vm.handle_arena.len(), 1);
        assert_eq!(h1.get(&vm).raw(), SmallInt::new(1).oop().raw());
        drop(outer);
        assert_eq!(vm.handle_arena.len(), 0);
    }

    #[test]
    fn handle_get_set_roundtrip() {
        let mut vm = test_vm();
        let scope = HandleScope::enter(&mut vm);
        let klass = vm.universe.array_klass;
        let arr = alloc::alloc_indexable_oops(&mut vm, klass, 0);
        let h = scope.handle(&mut vm, arr);
        assert_eq!(h.get(&vm), arr);

        let arr2 = alloc::alloc_indexable_oops(&mut vm, klass, 1);
        h.set(&mut vm, arr2);
        assert_eq!(h.get(&vm), arr2);
        assert_eq!(h.oop(&vm), arr2.oop());
    }

    /// `tests_s07.md`'s `handle_updated_by_gc`: a handle to an eden object
    /// reads the to-space copy after scavenge — this is the entire point
    /// of handles existing.
    #[test]
    fn handle_updated_by_gc() {
        let mut vm = test_vm();
        let scope = HandleScope::enter(&mut vm);
        let klass = vm.universe.array_klass;
        let arr = alloc::alloc_indexable_oops(&mut vm, klass, 1);
        arr.at_put(0, SmallInt::new(77).oop());
        let h = scope.handle(&mut vm, arr);
        let old_addr = arr.oop().raw();

        crate::memory::scavenge::scavenge(&mut vm).expect("scavenge must succeed");

        let moved = h.get(&vm);
        assert_ne!(moved.oop().raw(), old_addr, "the object must have moved");
        assert_eq!(moved.at(0), SmallInt::new(77).oop());
    }
}
