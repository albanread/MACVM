//! Safe, text-free entry point into the vendored QBE-fork pipeline:
//! construct a function directly via [`IrFunction`]'s builder methods — no
//! QBE IL text, no `parse()` tokenize/parse step — through the *same*
//! optimizer pipeline and `JitInst[]` collection [`crate::compile::compile_il_jit`]
//! uses. See `jit/c/ir_builder.h`'s module doc for the full design
//! rationale (what QBE already provides for free vs. what had to be
//! built) and `../REVIEW.md`'s "text-IL question" addendum for why this
//! exists at all.
//!
//! Entry point: [`build_function_jit`]. Builds and compiles exactly one
//! function per call — matches the actual use case (an adaptive tier
//! recompiling one hot method at a time), and keeps the C-side builder
//! state (`ir_builder.c`'s own `curb`/`blink`/`nblk`, mirroring
//! `parse.c`'s identically-named statics) unambiguous: one function is
//! ever under construction at a time, exactly like the text path's
//! `parsefn()`.

use crate::compile::{JitCollectorHandle, JitCompileError};
use crate::ffi;
use crate::ir_ffi::{self, QbeBlk, QbeFn, QbeRef};
use libc::{c_char, c_int, c_void};
use std::ffi::CString;
use std::marker::PhantomData;

/// A value class: QBE's `w`/`l`/`s`/`d`. Mirrors `ir_ffi::cls::*`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cls {
    W,
    L,
    S,
    D,
}
impl Cls {
    fn raw(self) -> c_int {
        match self {
            Cls::W => ir_ffi::cls::W,
            Cls::L => ir_ffi::cls::L,
            Cls::S => ir_ffi::cls::S,
            Cls::D => ir_ffi::cls::D,
        }
    }
}

/// `None` maps to `QBE_K_X` (void) at every call site that accepts an
/// `Option<Cls>` (a function's return type, a call's return type).
fn cls_or_void(cls: Option<Cls>) -> c_int {
    cls.map(Cls::raw).unwrap_or(ir_ffi::cls::X)
}

/// An opcode for [`IrFunction::ins`]/[`IrFunction::ins1`]. Just the raw
/// `ir_ffi::op::*` constant — see that module's doc comment for the exact
/// (curated) set and why a plain `c_int` alias was chosen over an ~80-arm
/// enum (mechanical 1:1 wrapping with no behavioral difference; the C
/// side already validates the range and calls `err()` on an out-of-range
/// value, so an enum would only move that check earlier, not add safety
/// this crate doesn't already have).
pub type Op = c_int;
pub use crate::ir_ffi::op;

/// An SSA value produced by some earlier builder call, or [`Ref::NONE`].
/// Opaque — construct one only by calling an `IrFunction` method, never
/// by hand. Mirrors `QbeRef`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ref(QbeRef);
impl Ref {
    /// The "no value" sentinel — QBE's `R`. Matches `ir_builder.c`'s
    /// `const QbeRef QBE_REF_NONE = {0};` exactly (see that file: `R`'s
    /// bit pattern is all-zero under any bitfield packing, so this is a
    /// real invariant, not a guess at the C side's layout).
    pub const NONE: Ref = Ref(QbeRef { bits: 0 });
}

/// A basic block within a function under construction. Opaque, `Copy`
/// (it's just a pointer QBE's arena owns) — pass it to
/// [`IrFunction::jmp`]/[`jnz`](IrFunction::jnz)/[`set_current_block`](IrFunction::set_current_block).
/// Only valid for the duration of the [`build_function_jit`] call that
/// produced it.
#[derive(Clone, Copy)]
pub struct Block(*mut QbeBlk);

/// A function under construction. Handed to the `build` closure passed to
/// [`build_function_jit`]; not constructible any other way, and not valid
/// to retain past that closure's return (the C-side arena backing it is
/// freed by `qbe_ir_compile_jit` immediately after).
pub struct IrFunction {
    raw: *mut QbeFn,
}

impl IrFunction {
    /// Allocate a new block — does *not* make it current (call
    /// [`set_current_block`](Self::set_current_block) when ready to start
    /// appending to it). Safe to call for a pure forward-reference handle
    /// (e.g. a loop's target blocks, allocated before the block that will
    /// jump to them has been filled in) without disturbing whatever block
    /// is currently open. The first block created becomes the function's
    /// entry block, regardless of when — or whether — it's later made
    /// current.
    pub fn new_block(&mut self, name: &str) -> Block {
        let name_c = CString::new(name).unwrap_or_default();
        // SAFETY: `self.raw` is a live Fn* for the duration of this call
        // (enforced by IrFunction's construction contract); `name_c`
        // outlives the call.
        let b = unsafe { ir_ffi::qbe_ir_blk_new(self.raw, name_c.as_ptr()) };
        Block(b)
    }

    /// Switch which block subsequent instruction/terminator calls append
    /// to. A block can be made current, filled in, and left exactly
    /// once — reopening an already-closed, non-empty block is a caller
    /// error the C side detects and rejects (not something it silently
    /// corrupts); see `qbe_ir_blk_set_current`'s doc comment in
    /// `ir_builder.h` for why. Calling this again on the block that's
    /// *already* current is a safe no-op.
    pub fn set_current_block(&mut self, b: Block) {
        // SAFETY: see new_block.
        unsafe { ir_ffi::qbe_ir_blk_set_current(self.raw, b.0) };
    }

    pub fn con_int(&mut self, value: i64) -> Ref {
        // SAFETY: see new_block.
        Ref(unsafe { ir_ffi::qbe_ir_con_int(self.raw, value) })
    }
    pub fn con_double(&mut self, value: f64) -> Ref {
        Ref(unsafe { ir_ffi::qbe_ir_con_double(self.raw, value) })
    }
    pub fn con_single(&mut self, value: f32) -> Ref {
        Ref(unsafe { ir_ffi::qbe_ir_con_single(self.raw, value) })
    }
    /// The address of a global symbol, optionally offset by `addend`
    /// bytes. Resolved later by [`crate::linker`] against either this
    /// module's own symbol table or a caller-supplied `RuntimeContext` —
    /// exactly the same symbol-name mechanism a `$sym` reference in QBE
    /// IL text already uses (see `crate::module`'s `OOP_SYMBOL_PREFIX`/
    /// `IC_SITE_PREFIX` conventions for how MACVM would use this to mark
    /// oop slots / IC sites without any text at all).
    pub fn con_addr(&mut self, sym_name: &str, addend: i64) -> Ref {
        let name_c = CString::new(sym_name).unwrap_or_default();
        Ref(unsafe { ir_ffi::qbe_ir_con_addr(self.raw, name_c.as_ptr(), addend) })
    }

    /// A 2-argument instruction (most `op::*` opcodes) — allocates a
    /// fresh destination temporary of class `cls` and returns it.
    pub fn ins(&mut self, op: Op, cls: Cls, arg0: Ref, arg1: Ref) -> Ref {
        // SAFETY: see new_block; `op` is range-checked C-side (err() on
        // an invalid index, which unwinds via the protected-call guard
        // build_function_jit establishes around the whole session).
        Ref(unsafe { ir_ffi::qbe_ir_ins(self.raw, op, cls.raw(), arg0.0, arg1.0) })
    }

    /// A 1-argument instruction (`neg`, `ext*`, `trunc*`, `*tosi`/`*tof`,
    /// `cast`, `copy`, `load*`) — shorthand for `ins(op, cls, arg0,
    /// Ref::NONE)`.
    pub fn ins1(&mut self, op: Op, cls: Cls, arg0: Ref) -> Ref {
        self.ins(op, cls, arg0, Ref::NONE)
    }

    /// A store — no result. `op` must be one of `op::STOREB`/`STOREH`/
    /// `STOREW`/`STOREL`/`STORES`/`STORED` (picks the width; the C side
    /// rejects anything else via `err()`). Argument order matches QBE IL
    /// text's own `storew VALUE, ADDR` — value first, then address.
    pub fn store(&mut self, op: Op, value: Ref, addr: Ref) {
        unsafe { ir_ffi::qbe_ir_store(self.raw, op, value.0, addr.0) };
    }

    /// Stack allocation (`alloc4`/`alloc8`/`alloc16` — `align` must be 4,
    /// 8, or 16). Returns a fresh pointer-width (`l`) temporary. This is
    /// how a mutable local variable should be represented — see
    /// `qbe_ir_alloc`'s doc comment in `ir_builder.h` for why (the
    /// standard optimizer pipeline this crate runs includes `promote()`,
    /// which lifts SSA-promotable stack slots automatically; no explicit
    /// Phi construction needed).
    pub fn alloc(&mut self, align: i32, size: Ref) -> Ref {
        Ref(unsafe { ir_ffi::qbe_ir_alloc(self.raw, align, size.0) })
    }

    /// Declare the next function parameter, in left-to-right order — must
    /// be called in the entry block before any other instruction. Returns
    /// a fresh temporary holding the parameter value.
    pub fn par(&mut self, cls: Cls) -> Ref {
        Ref(unsafe { ir_ffi::qbe_ir_par(self.raw, cls.raw()) })
    }

    /// Declare the next call argument, immediately before the
    /// [`call`](Self::call) it belongs to. Do not interleave `arg` calls
    /// for two different pending calls.
    pub fn arg(&mut self, cls: Cls, value: Ref) {
        unsafe { ir_ffi::qbe_ir_arg(self.raw, cls.raw(), value.0) };
    }

    /// Emit the call itself, after all its [`arg`](Self::arg) calls.
    /// `func` is typically [`con_addr`](Self::con_addr) (a direct call)
    /// but can be any `Ref` (an indirect call through a computed address
    /// — e.g. a vtable slot). `ret_cls: None` is a void call. Marks the
    /// function non-leaf.
    pub fn call(&mut self, func: Ref, ret_cls: Option<Cls>) -> Ref {
        Ref(unsafe { ir_ffi::qbe_ir_call(self.raw, func.0, cls_or_void(ret_cls)) })
    }

    /// Force the function to be treated as non-leaf without an actual
    /// call — see `qbe_ir_set_nonleaf`'s doc comment.
    pub fn set_nonleaf(&mut self) {
        unsafe { ir_ffi::qbe_ir_set_nonleaf(self.raw) };
    }

    pub fn jmp(&mut self, target: Block) {
        unsafe { ir_ffi::qbe_ir_jmp(self.raw, target.0) };
    }
    pub fn jnz(&mut self, cond: Ref, if_true: Block, if_false: Block) {
        unsafe { ir_ffi::qbe_ir_jnz(self.raw, cond.0, if_true.0, if_false.0) };
    }
    /// `cls: None` is a value-less return (`ret` / void function).
    pub fn ret(&mut self, cls: Option<Cls>, value: Ref) {
        let raw_value = if cls.is_some() { value.0 } else { Ref::NONE.0 };
        unsafe { ir_ffi::qbe_ir_ret(self.raw, cls_or_void(cls), raw_value) };
    }
    pub fn hlt(&mut self) {
        unsafe { ir_ffi::qbe_ir_hlt(self.raw) };
    }
}

/// Function-level metadata for [`build_function_jit`] — the text-free
/// equivalent of a QBE IL text file's `export function w $name(...) {`
/// line.
pub struct FnSpec<'a> {
    pub name: &'a str,
    /// `None` for a void-returning function.
    pub ret_cls: Option<Cls>,
    pub is_export: bool,
}

struct BuildCtx<'a, F: FnOnce(&mut IrFunction)> {
    name: *const c_char,
    ret_cls: c_int,
    is_export: c_int,
    target: *const c_char,
    jc: *mut ffi::JitCollector,
    build: Option<F>,
    rc: c_int,
    _marker: PhantomData<&'a ()>,
}

extern "C" fn build_trampoline<F: FnOnce(&mut IrFunction)>(ctx: *mut c_void) -> c_int {
    // SAFETY: `ctx` is always `&mut BuildCtx<F>` cast to `*mut c_void` by
    // build_function_jit below, immediately before this call, not
    // aliased elsewhere during it.
    let ctx = unsafe { &mut *(ctx as *mut BuildCtx<F>) };

    // SAFETY: called only from within rust_qbe_protected_call's setjmp
    // guard (the whole point of this trampoline), with valid C strings
    // and a live JitCollector*.
    let raw_fn = unsafe { ir_ffi::qbe_ir_func_begin(ctx.name, ctx.ret_cls, ctx.is_export) };

    let mut irfn = IrFunction { raw: raw_fn };
    if let Some(build) = ctx.build.take() {
        build(&mut irfn);
    }

    // SAFETY: `raw_fn` was just built above in this same protected scope;
    // qbe_ir_func_end/qbe_ir_compile_jit are the documented next steps
    // (see ir_builder.h) and both can reach err(), which is fine — we're
    // still inside the guard.
    let raw_fn = unsafe { ir_ffi::qbe_ir_func_end(raw_fn) };
    let rc = unsafe { ir_ffi::qbe_ir_compile_jit(raw_fn, ctx.jc, ctx.target) };
    ctx.rc = rc;
    rc
}

/// Build and compile exactly one function via `build`, entirely without
/// QBE IL text — construct it using `IrFunction`'s methods, called from
/// inside `build`. Everything `build` does runs inside one
/// `rust_qbe_protected_call` scope alongside the optimizer pipeline and
/// `JitInst` collection that follow it, so a malformed function (caught
/// by `typecheck()` or an assertion deeper in the pipeline) unwinds
/// cleanly instead of taking down the process — see `ir_builder.h`'s top
/// comment and `csrc/rust_bridge.c`'s module doc.
///
/// Takes the same process-wide lock [`crate::compile::compile_il_jit`]
/// does (both touch QBE's same C globals) — blocks other callers on this
/// process until it returns.
pub fn build_function_jit<F: FnOnce(&mut IrFunction)>(
    spec: FnSpec,
    target: Option<&str>,
    build: F,
) -> Result<JitCollectorHandle, JitCompileError> {
    let name_cstring = CString::new(spec.name)
        .map_err(|_| JitCompileError::InvalidInput("function name contained a NUL byte".into()))?;
    let target_cstring = target
        .map(CString::new)
        .transpose()
        .map_err(|_| JitCompileError::InvalidInput("target name contained a NUL byte".into()))?;
    let target_ptr = target_cstring.as_ref().map(|c| c.as_ptr()).unwrap_or(std::ptr::null());

    let _guard = crate::compile::COMPILE_LOCK.lock().unwrap_or_else(|poisoned| {
        // See compile::compile_il_jit's identical comment: a prior call
        // panicking while holding this lock leaves QBE's global C state
        // in an unknown shape — surface the poison rather than silently
        // building against it.
        poisoned.into_inner()
    });

    let mut jc = ffi::JitCollector::zeroed();
    // SAFETY: jc is a valid, zeroed JitCollector.
    if unsafe { ffi::jit_collector_init(&mut jc) } != 0 {
        return Err(JitCompileError::Failed {
            code: -1,
            message: "jit_collector_init: allocation failure".into(),
        });
    }

    let mut ctx = BuildCtx {
        name: name_cstring.as_ptr(),
        ret_cls: cls_or_void(spec.ret_cls),
        is_export: spec.is_export as c_int,
        target: target_ptr,
        jc: &mut jc,
        build: Some(build),
        rc: 0,
        _marker: PhantomData,
    };

    // SAFETY: build_trampoline only touches `ctx` (valid for the
    // duration of this call) and every C call it makes happens inside
    // the setjmp guard rust_qbe_protected_call establishes, so a longjmp
    // from basic_exit() (via err()/die() in typecheck() or the optimizer
    // pipeline) unwinds no further than that guard — never through this
    // Rust frame.
    let outer_rc = unsafe {
        ffi::rust_qbe_protected_call(build_trampoline::<F>, &mut ctx as *mut _ as *mut c_void)
    };

    if outer_rc == ffi::RUST_QBE_LONGJMP_SENTINEL {
        // SAFETY: qbe_jit_cleanup is safe to call any time (guarded
        // internally); required here to release whatever partial state
        // (Fn/Blk/Ins arena allocations, possibly a half-built JitInst
        // stream) the aborted build/compile left behind — same recovery
        // path compile_il_jit uses for the text path's longjmp case.
        unsafe { ffi::qbe_jit_cleanup() };
        unsafe { ffi::jit_collector_free(&mut jc) };
        return Err(JitCompileError::ParseAborted);
    }

    debug_assert_eq!(outer_rc, ctx.rc);

    if ctx.rc == ffi::qbe_status::ERR_TARGET {
        unsafe { ffi::jit_collector_free(&mut jc) };
        return Err(JitCompileError::UnknownTarget(target.unwrap_or("<default>").to_string()));
    }
    if ctx.rc != ffi::qbe_status::OK || jc.error != 0 {
        let message = crate::compile::collector_error_message(&jc);
        unsafe { ffi::jit_collector_free(&mut jc) };
        return Err(JitCompileError::Failed { code: ctx.rc, message });
    }

    Ok(JitCollectorHandle::from_raw(jc))
}
