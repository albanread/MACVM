//! Raw FFI surface for `jit/c/ir_builder.h` — the text-free IR
//! construction API. Mirrors that header exactly, the same way `ffi.rs`
//! mirrors `jit_collect.h`/`qbe_bridge.h`.
//!
//! Nothing here is safe to call directly except through
//! [`crate::ir::build_function_jit`], which takes the same global compile
//! lock [`crate::compile::compile_il_jit`] uses (both touch QBE's same C
//! globals — sharing one lock is load-bearing, not a style choice) and
//! routes every call through the setjmp/longjmp guard, matching
//! `ir_builder.h`'s own documented calling convention (most of these
//! functions can reach `err()` on malformed input).

use crate::ffi::JitCollector;
use libc::{c_char, c_double, c_float, c_int};

/// Opaque QBE `Fn*` — a function under construction.
#[repr(C)]
pub struct QbeFn {
    _private: [u8; 0],
}

/// Opaque QBE `Blk*` — a basic block within a function under construction.
#[repr(C)]
pub struct QbeBlk {
    _private: [u8; 0],
}

/// Mirrors `QbeRef` (`ir_builder.h`) bit-for-bit: a `{ bits: u32 }`
/// wrapper around QBE's packed `Ref` bitfield. Opaque on the Rust side —
/// never construct one by hand; every `QbeRef` in circulation came from a
/// `qbe_ir_*` call that returned one.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct QbeRef {
    pub bits: u32,
}

/// Mirrors `enum { QBE_OP_* }` (`ir_builder.h`) — keep in exact 1:1 order
/// with that C enum; see its own doc comment for why this list is a
/// curated subset, not QBE's full opcode space.
pub mod op {
    use libc::c_int;

    pub const ADD: c_int = 0;
    pub const SUB: c_int = 1;
    pub const NEG: c_int = 2;
    pub const DIV: c_int = 3;
    pub const REM: c_int = 4;
    pub const UDIV: c_int = 5;
    pub const UREM: c_int = 6;
    pub const MUL: c_int = 7;
    pub const AND: c_int = 8;
    pub const OR: c_int = 9;
    pub const XOR: c_int = 10;
    pub const SAR: c_int = 11;
    pub const SHR: c_int = 12;
    pub const SHL: c_int = 13;

    pub const CEQW: c_int = 14;
    pub const CNEW: c_int = 15;
    pub const CSGEW: c_int = 16;
    pub const CSGTW: c_int = 17;
    pub const CSLEW: c_int = 18;
    pub const CSLTW: c_int = 19;
    pub const CUGEW: c_int = 20;
    pub const CUGTW: c_int = 21;
    pub const CULEW: c_int = 22;
    pub const CULTW: c_int = 23;

    pub const CEQL: c_int = 24;
    pub const CNEL: c_int = 25;
    pub const CSGEL: c_int = 26;
    pub const CSGTL: c_int = 27;
    pub const CSLEL: c_int = 28;
    pub const CSLTL: c_int = 29;
    pub const CUGEL: c_int = 30;
    pub const CUGTL: c_int = 31;
    pub const CULEL: c_int = 32;
    pub const CULTL: c_int = 33;

    pub const CEQS: c_int = 34;
    pub const CGES: c_int = 35;
    pub const CGTS: c_int = 36;
    pub const CLES: c_int = 37;
    pub const CLTS: c_int = 38;
    pub const CNES: c_int = 39;
    pub const COS: c_int = 40;
    pub const CUOS: c_int = 41;

    pub const CEQD: c_int = 42;
    pub const CGED: c_int = 43;
    pub const CGTD: c_int = 44;
    pub const CLED: c_int = 45;
    pub const CLTD: c_int = 46;
    pub const CNED: c_int = 47;
    pub const COD: c_int = 48;
    pub const CUOD: c_int = 49;

    pub const STOREB: c_int = 50;
    pub const STOREH: c_int = 51;
    pub const STOREW: c_int = 52;
    pub const STOREL: c_int = 53;
    pub const STORES: c_int = 54;
    pub const STORED: c_int = 55;
    pub const LOADSB: c_int = 56;
    pub const LOADUB: c_int = 57;
    pub const LOADSH: c_int = 58;
    pub const LOADUH: c_int = 59;
    pub const LOADSW: c_int = 60;
    pub const LOADUW: c_int = 61;
    pub const LOAD: c_int = 62;

    pub const EXTSB: c_int = 63;
    pub const EXTUB: c_int = 64;
    pub const EXTSH: c_int = 65;
    pub const EXTUH: c_int = 66;
    pub const EXTSW: c_int = 67;
    pub const EXTUW: c_int = 68;
    pub const EXTS: c_int = 69;
    pub const TRUNCD: c_int = 70;
    pub const STOSI: c_int = 71;
    pub const STOUI: c_int = 72;
    pub const DTOSI: c_int = 73;
    pub const DTOUI: c_int = 74;
    pub const SWTOF: c_int = 75;
    pub const UWTOF: c_int = 76;
    pub const SLTOF: c_int = 77;
    pub const ULTOF: c_int = 78;
    pub const CAST: c_int = 79;
    pub const COPY: c_int = 80;

    pub const COUNT: c_int = 81;
}

/// Mirrors `enum { QBE_K_* }` (`ir_builder.h`).
pub mod cls {
    use libc::c_int;
    pub const W: c_int = 0;
    pub const L: c_int = 1;
    pub const S: c_int = 2;
    pub const D: c_int = 3;
    pub const X: c_int = -1;
}

extern "C" {
    pub fn qbe_ir_func_begin(name: *const c_char, ret_cls: c_int, is_export: c_int) -> *mut QbeFn;
    pub fn qbe_ir_func_end(fn_: *mut QbeFn) -> *mut QbeFn;
    pub fn qbe_ir_set_nonleaf(fn_: *mut QbeFn);

    pub fn qbe_ir_blk_new(fn_: *mut QbeFn, name: *const c_char) -> *mut QbeBlk;
    pub fn qbe_ir_blk_set_current(fn_: *mut QbeFn, b: *mut QbeBlk);

    pub fn qbe_ir_con_int(fn_: *mut QbeFn, value: i64) -> QbeRef;
    pub fn qbe_ir_con_double(fn_: *mut QbeFn, value: c_double) -> QbeRef;
    pub fn qbe_ir_con_single(fn_: *mut QbeFn, value: c_float) -> QbeRef;
    pub fn qbe_ir_con_addr(fn_: *mut QbeFn, sym_name: *const c_char, addend: i64) -> QbeRef;

    pub fn qbe_ir_ins(fn_: *mut QbeFn, op: c_int, cls: c_int, arg0: QbeRef, arg1: QbeRef) -> QbeRef;
    pub fn qbe_ir_store(fn_: *mut QbeFn, op: c_int, value: QbeRef, addr: QbeRef);
    pub fn qbe_ir_alloc(fn_: *mut QbeFn, align: c_int, size: QbeRef) -> QbeRef;

    pub fn qbe_ir_par(fn_: *mut QbeFn, cls: c_int) -> QbeRef;

    pub fn qbe_ir_arg(fn_: *mut QbeFn, cls: c_int, value: QbeRef);
    pub fn qbe_ir_call(fn_: *mut QbeFn, func: QbeRef, ret_cls: c_int) -> QbeRef;

    pub fn qbe_ir_jmp(fn_: *mut QbeFn, target: *mut QbeBlk);
    pub fn qbe_ir_jnz(fn_: *mut QbeFn, cond: QbeRef, if_true: *mut QbeBlk, if_false: *mut QbeBlk);
    pub fn qbe_ir_ret(fn_: *mut QbeFn, cls: c_int, value: QbeRef);
    pub fn qbe_ir_hlt(fn_: *mut QbeFn);

    pub fn qbe_ir_compile_jit(fn_: *mut QbeFn, jc: *mut JitCollector, target_name: *const c_char) -> c_int;

    pub static QBE_REF_NONE: QbeRef;
}

// SAFETY note for callers (not an unsafe impl — these are raw extern
// items): every function above except the constant-returning ones can
// reach QBE's err()/die() (-> basic_exit() -> longjmp) on malformed
// input. Must be called from within crate::compile's
// rust_qbe_protected_call guard — see crate::ir::build_function_jit,
// the only sanctioned entry point.
