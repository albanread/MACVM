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
    /// Per-slot generation, parallel to `slots` but **persistent**: never
    /// truncated when a scope drops (only `slots` is). A slot's generation is
    /// bumped when its owning scope drops (vacate-bump, `HandleScope::drop`),
    /// so any `Handle` that outlives its scope carries an obsolete generation
    /// and fails validation in `Handle::slot` — a use-after-scope becomes a
    /// precise panic at the misuse site instead of a silent foreign-oop read
    /// (SPEC §7.6.1 change 1). MACVM has no free-list/tombstone like the Locus
    /// reference this is ported from, so vacate-bump is what gives a vacated
    /// slot the "reused ⇒ prior handles invalid" property Locus gets from
    /// tombstones. `gen.len()` only ever grows, to the arena's high-water
    /// depth, so `gen[i]` is always readable for any minted handle's `i`.
    gen: Vec<u32>,
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
        let index = vm.handle_arena.slots.len();
        let generation = if index < vm.handle_arena.gen.len() {
            // Reusing a slot position a prior scope vacated: its generation was
            // already bumped on that scope's drop, so it differs from anything
            // the prior occupant minted. Use it as-is.
            vm.handle_arena.gen[index]
        } else {
            debug_assert!(index == vm.handle_arena.gen.len());
            vm.handle_arena.gen.push(0);
            0
        };
        if std::env::var("MACVM_DBG_ROOTS").is_ok() {
            eprintln!(
                "RDBG handle-create [{index}] = {:#x} (scav#{})",
                val.as_oop().raw(),
                vm.universe.gc_stats.scavenge_count,
            );
            // Catch a stale capture at its source: a mem oop whose mark word
            // no longer parses as a live header was already collected/moved —
            // the caller is holding a pre-GC Rust-side copy.
            let o = val.as_oop();
            if o.is_mem() {
                let m = crate::oops::wrappers::MemOop::try_from(o).unwrap();
                let w = m.mark_word_raw();
                let plausible = w & crate::oops::layout::TAG_MASK == crate::oops::layout::MARK_TAG
                    && w & 0b100 != 0;
                if !plausible {
                    eprintln!(
                        "RDBG handle-create STALE CAPTURE [{index}] = {:#x}\n{}",
                        o.raw(),
                        std::backtrace::Backtrace::force_capture()
                    );
                }
            }
        }
        vm.handle_arena.slots.push(val.as_oop());
        Handle {
            index: index as u32,
            generation,
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
        // Vacate-bump (SPEC §7.6.1 change 1): bump the generation of every slot
        // this scope is about to drop, so any `Handle` still naming one is
        // immediately stale — the next `Handle::slot` on it sees the mismatch
        // and panics at the misuse site. Cheap (one increment per handle in the
        // scope, ~1 on the send path) and always-on so debug and release keep
        // identical `gen` state; only the *check* in `Handle::slot` is
        // debug-gated. `gen` is not truncated with `slots`, so it persists.
        let live_top = arena.slots.len();
        for g in &mut arena.gen[self.saved_len..live_top] {
            *g = g.wrapping_add(1);
        }
        arena.slots.truncate(self.saved_len);
    }
}

/// An index into the handle arena — never a pointer, so `Vec` growth
/// (reallocating the backing storage) never invalidates it — plus the
/// generation the slot carried when this handle was minted (SPEC §7.6.1).
#[derive(Copy, Clone)]
pub struct Handle<T> {
    index: u32,
    generation: u32,
    _t: PhantomData<T>,
}

impl<T: OopRepr> Handle<T> {
    /// Resolve to a validated live slot index. Two guards (SPEC §7.6.1):
    /// (i) `index < slots.len()` — enforced in *all* builds by the `Vec`
    /// index in the callers below (a vacated slot is truncated away, so a
    /// dropped-scope handle is out of range); (ii) `gen[index] == generation`
    /// — catches an *in-range* slot that a later scope re-occupied, and is
    /// **debug-gated**, because handles are on the send hot path
    /// (`activate_method` mints one per activation) and this is a validation
    /// backstop, not a release-mode invariant. A pure-LIFO arena has no holes,
    /// so these two subsume Locus's three-guard (bounds/tombstone/gen) resolve:
    /// "in range" here *is* "not vacated".
    #[inline(always)]
    fn slot(self, vm: &VmState) -> usize {
        let i = self.index as usize;
        debug_assert!(
            i < vm.handle_arena.slots.len() && vm.handle_arena.gen[i] == self.generation,
            "stale handle: slot {i} is now generation {}, handle is {} (use-after-scope — \
             the handle outlived the HandleScope that minted it)",
            vm.handle_arena.gen.get(i).copied().unwrap_or(u32::MAX),
            self.generation,
        );
        i
    }

    /// Always re-read after any call that can allocate — the arena slot
    /// may have been rewritten in place by an intervening scavenge.
    pub fn get(self, vm: &VmState) -> T {
        let o = vm.handle_arena.slots[self.slot(vm)];
        // SAFETY: this slot was populated by `HandleScope::handle::<T>`
        // (or later overwritten by `set::<T>`) — always `T`-shaped. The
        // generation guard in `slot` upholds this once handles cross API
        // boundaries (SPEC §7.6.1 change 2): a stale handle whose slot was
        // reused for a different `T` panics rather than mis-transmuting.
        unsafe { T::from_oop_unchecked(o) }
    }

    pub fn set(self, vm: &mut VmState, val: T) {
        let i = self.slot(vm);
        vm.handle_arena.slots[i] = val.as_oop();
    }

    pub fn oop(self, vm: &VmState) -> Oop {
        vm.handle_arena.slots[self.slot(vm)]
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
            gc_stress_full_period: None,
            eden_kb: None,
            jit: crate::runtime::JitMode::Off,
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

    /// SPEC §7.6.1 change 1: a `Handle` used after its originating scope drops
    /// (its slot since reused by a later scope) panics precisely at the misuse
    /// site via the generation guard — instead of silently reading the foreign
    /// oop the reusing scope stored there. This is the deterministic
    /// regression the S7.5 gate calls for; it is the exact shape of the
    /// stale-handle bugs S7-9/10/11 hit, now caught at the source. Debug-gated
    /// because the guard is (handles are on the send hot path).
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "stale handle")]
    fn stale_handle_panics() {
        let mut vm = test_vm();
        let stale = {
            let s = HandleScope::enter(&mut vm);
            s.handle(&mut vm, SmallInt::new(1).oop())
        }; // `s` drops here: slot 0 vacated, its generation bumped.

        // A new scope reuses slot 0 for a different value at the bumped
        // generation — a valid, live handle.
        let s2 = HandleScope::enter(&mut vm);
        let fresh = s2.handle(&mut vm, SmallInt::new(2).oop());
        assert_eq!(
            fresh.get(&vm).raw(),
            SmallInt::new(2).oop().raw(),
            "the reusing scope's own handle must resolve fine"
        );

        // `stale` names slot 0 at the OLD generation → must panic, not read 2.
        let _ = stale.get(&vm);
    }
}
