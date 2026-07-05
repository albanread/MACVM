//! The `RusttclCtx` <-> Tcl `Registry` bridge (see the module doc on
//! `super` for why one is needed at all: `Registry::register`'s handler
//! type demands `Send + Sync + 'static`, ruling out any lifetime-bound
//! capture of `&mut RusttclCtx`).
//!
//! `with_ctx_active` stores `ctx` as a raw pointer in a thread-local for
//! the exact duration of one `f()` call, clearing it again on the way out
//! — including on unwind, via the `ClearOnDrop` guard, so a panicking
//! verb never leaves a dangling pointer behind. `active_ctx` is the other
//! half: called from inside a verb closure, it reads that pointer back
//! and reconstitutes a `&mut RusttclCtx`.
//!
//! # Why this is sound
//! RUSTTCL is single-threaded by construction — one REPL/script loop
//! drives one Tcl `Vm::run` call at a time, and no verb closure spawns a
//! thread or re-enters `with_ctx_active` reentrantly (nothing in
//! `verbs.rs` calls back into `compile`/`Vm::run`). So at any instant
//! there is at most one live `&mut RusttclCtx` derived from the raw
//! pointer, and the ORIGINAL `&mut RusttclCtx` binding in `run_repl`/
//! `run_script` is not touched again until `with_ctx_active` returns (it
//! is reborrowed into `with_ctx_active`'s own parameter and never read
//! through the outer binding while `f` runs) — the same raw-pointer
//! round-trip [`crate::codecache::stubs`]'s `unsafe extern "C" fn foo(vm:
//! *mut VmState, ..)` runtime stubs already rely on for compiled code
//! calling back into the VM, just via a thread-local instead of a
//! register.
#![allow(unsafe_code)]

use std::cell::Cell;

use super::RusttclCtx;

thread_local! {
    static ACTIVE_CTX: Cell<*mut RusttclCtx> = const { Cell::new(std::ptr::null_mut()) };
}

struct ClearOnDrop;

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        ACTIVE_CTX.with(|cell| cell.set(std::ptr::null_mut()));
    }
}

/// Makes `ctx` reachable from any verb closure invoked during `f` (via
/// [`active_ctx`]), for exactly the duration of this call.
pub fn with_ctx_active<R>(ctx: &mut RusttclCtx, f: impl FnOnce() -> R) -> R {
    ACTIVE_CTX.with(|cell| cell.set(ctx as *mut RusttclCtx));
    let _guard = ClearOnDrop;
    f()
}

/// Every registered verb's first line. Panics if called outside a
/// `with_ctx_active` scope — unreachable in practice, since every verb
/// closure only ever runs from inside a `Vm::run` call, and `run_repl`/
/// `run_script` always wrap that call in `with_ctx_active`.
pub fn active_ctx() -> &'static mut RusttclCtx {
    let ptr = ACTIVE_CTX.with(|cell| cell.get());
    assert!(
        !ptr.is_null(),
        "rusttcl: verb called outside with_ctx_active"
    );
    // SAFETY: this module's own doc above — single-threaded, non-
    // reentrant, and the pointee (borrowed for exactly this scope by
    // `with_ctx_active`) outlives every use made during it.
    unsafe { &mut *ptr }
}
