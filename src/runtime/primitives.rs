//! The primitive mechanism (SPEC §10): pinned ids/semantics for the smi,
//! oops, bytes, and system (dev-hook) groups. Every `PrimFn` validates its
//! own receiver/arg tags and formats — a violation is always `PrimResult::Fail`
//! (bytecode-body fallback), never a Rust panic; `args[0]` is always the
//! receiver.
//!
//! Layer boundary (`sprint_s03_detail.md` §Layer boundaries): this module
//! reads/writes the operand stack only through `VmState::prim_arg`/the
//! `args` slice handed in by the interpreter — it never pushes a frame.

use crate::memory::alloc;
use crate::oops::klass::Format;
use crate::oops::layout::{HEADER_WORDS, SMI_MAX, SMI_MIN};
use crate::oops::smi::SmallInt;
use crate::oops::wrappers::{ArrayOop, ByteArrayOop, KlassOop, MemOop};
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;

use std::io::Write;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PrimResult {
    Ok(Oop),
    Fail,
    /// The primitive already replaced the sender's continuation — pushed a
    /// real frame and set `vm.regs` itself (SPEC §10, S4: the `value`
    /// family, `ensure:`, `ifCurtailed:`). The interpreter's primitive-call
    /// site must push NO result and just return to dispatch; pushing one
    /// would corrupt the new frame's temp area by one slot.
    Activated,
    /// S24 A1: a COMPILED block body invoked by `activate_block`'s
    /// enter-compiled fast path performed a non-local return, and
    /// `continue_unwind` has ALREADY run — this is its outcome, to be
    /// relayed exactly like `EnterResult::Nlr`/`SendOutcome::Nlr` (S11
    /// D6.3). Produced ONLY by `interpreter::blocks::activate_block`; the
    /// consumer must NOT touch the operand stack (`sp = base` would be
    /// wrong — after an NLR the stack belongs to a different activation).
    Nlr(crate::interpreter::unwind::UnwindStep),
}

pub type PrimFn = fn(vm: &mut VmState, args: &[Oop]) -> PrimResult;

pub struct PrimDesc {
    pub id: u16,
    pub name: &'static str,
    pub f: PrimFn,
    /// Argument count EXCLUDING the receiver.
    pub argc: u8,
    pub can_allocate: bool,
    pub can_fail: bool,
}

/// Binary-searched by [`prim_by_id`] — MUST stay sorted by `id` (the
/// `prim_table_sorted_unique` test enforces this).
pub static PRIMITIVES: &[PrimDesc] = &[
    PrimDesc {
        id: 1,
        name: "+",
        f: prim_add,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 2,
        name: "-",
        f: prim_sub,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 3,
        name: "*",
        f: prim_mul,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 4,
        name: "//",
        f: prim_div,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 5,
        name: "\\\\",
        f: prim_mod,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 6,
        name: "bitAnd:",
        f: prim_bit_and,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 7,
        name: "bitOr:",
        f: prim_bit_or,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 8,
        name: "bitXor:",
        f: prim_bit_xor,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 9,
        name: "bitShift:",
        f: prim_bit_shift,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 10,
        name: "<",
        f: prim_lt,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 11,
        name: "<=",
        f: prim_le,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 12,
        name: ">",
        f: prim_gt,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 13,
        name: ">=",
        f: prim_ge,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 14,
        name: "=",
        f: prim_eq,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 15,
        name: "~=",
        f: prim_ne,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 20,
        name: "identityHash",
        f: prim_identity_hash,
        argc: 0,
        can_allocate: false,
        can_fail: false,
    },
    PrimDesc {
        id: 21,
        name: "class",
        f: prim_class,
        argc: 0,
        can_allocate: false,
        can_fail: false,
    },
    PrimDesc {
        id: 22,
        name: "==",
        f: prim_identical,
        argc: 1,
        can_allocate: false,
        can_fail: false,
    },
    PrimDesc {
        id: 23,
        name: "basicNew",
        f: prim_basic_new,
        argc: 0,
        can_allocate: true,
        can_fail: true,
    },
    PrimDesc {
        id: 24,
        name: "basicNew:",
        f: prim_basic_new_colon,
        argc: 1,
        can_allocate: true,
        can_fail: true,
    },
    PrimDesc {
        id: 25,
        name: "instVarAt:",
        f: prim_inst_var_at,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 26,
        name: "at:",
        f: prim_at,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 27,
        name: "at:put:",
        f: prim_at_put,
        argc: 2,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 28,
        name: "size",
        f: prim_size,
        argc: 0,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 40,
        name: "byteAt:",
        f: prim_byte_at,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 41,
        name: "byteAt:put:",
        f: prim_byte_at_put,
        argc: 2,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 42,
        name: "byteSize",
        f: prim_byte_size,
        argc: 0,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 43,
        name: "replaceFrom:to:with:",
        f: prim_replace_from_to_with,
        argc: 3,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 44,
        name: "hashBytes",
        f: prim_hash_bytes,
        argc: 0,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 45,
        name: "compare:",
        f: prim_compare,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 50,
        name: "value",
        f: prim_value0,
        argc: 0,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 51,
        name: "value:",
        f: prim_value1,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 52,
        name: "value:value:",
        f: prim_value2,
        argc: 2,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 53,
        name: "value:value:value:",
        f: prim_value3,
        argc: 3,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 54,
        name: "valueWithArguments:",
        f: prim_value_with_arguments,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 60,
        name: "ensure:",
        f: prim_ensure,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 61,
        name: "ifCurtailed:",
        f: prim_if_curtailed,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 90,
        name: "quit",
        f: prim_quit,
        argc: 0,
        can_allocate: false,
        can_fail: false,
    },
    PrimDesc {
        id: 91,
        name: "printOnStdout:",
        f: prim_print_on_stdout,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 92,
        name: "millisecondClock",
        f: prim_millisecond_clock,
        argc: 0,
        can_allocate: false,
        can_fail: false,
    },
    PrimDesc {
        id: 93,
        name: "gcScavenge",
        f: prim_gc_scavenge,
        argc: 0,
        can_allocate: true,
        can_fail: false,
    },
    PrimDesc {
        id: 94,
        name: "gcFull",
        f: prim_gc_full,
        argc: 0,
        can_allocate: true,
        can_fail: false,
    },
    PrimDesc {
        id: 95,
        name: "error:",
        f: prim_error,
        argc: 1,
        can_allocate: false,
        can_fail: false,
    },
    PrimDesc {
        id: 96,
        name: "quit:",
        f: prim_quit_colon,
        argc: 1,
        can_allocate: false,
        can_fail: false,
    },
    PrimDesc {
        id: 97,
        name: "gcStats",
        f: prim_gc_stats,
        argc: 0,
        can_allocate: true,
        can_fail: false,
    },
    PrimDesc {
        id: 98,
        name: "allClasses",
        f: prim_all_classes,
        argc: 0,
        can_allocate: true,
        can_fail: false,
    },
    // --- Double group (S6, SPEC §1.3) ------------------------------------
    PrimDesc {
        id: 100,
        name: "+",
        f: prim_double_add,
        argc: 1,
        can_allocate: true,
        can_fail: true,
    },
    PrimDesc {
        id: 101,
        name: "-",
        f: prim_double_sub,
        argc: 1,
        can_allocate: true,
        can_fail: true,
    },
    PrimDesc {
        id: 102,
        name: "*",
        f: prim_double_mul,
        argc: 1,
        can_allocate: true,
        can_fail: true,
    },
    PrimDesc {
        id: 103,
        name: "/",
        f: prim_double_div,
        argc: 1,
        can_allocate: true,
        can_fail: true,
    },
    PrimDesc {
        id: 104,
        name: "<",
        f: prim_double_lt,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 105,
        name: "=",
        f: prim_double_eq,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 106,
        name: "sqrt",
        f: prim_double_sqrt,
        argc: 0,
        can_allocate: true,
        can_fail: true,
    },
    PrimDesc {
        id: 107,
        name: "floor",
        f: prim_double_floor,
        argc: 0,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 108,
        name: "asDouble",
        f: prim_smi_as_double,
        argc: 0,
        can_allocate: true,
        can_fail: true,
    },
    PrimDesc {
        id: 109,
        name: "printDigits",
        f: prim_double_print_digits,
        argc: 0,
        can_allocate: true,
        can_fail: true,
    },
    // --- Symbol group (S6) -------------------------------------------------
    PrimDesc {
        id: 110,
        name: "asSymbol",
        f: prim_string_as_symbol,
        argc: 0,
        can_allocate: true,
        can_fail: true,
    },
    // --- S15 A8 dev hook ----------------------------------------------------
    PrimDesc {
        id: 111,
        name: "__vmStats",
        f: prim_vm_stats,
        argc: 0,
        can_allocate: true,
        can_fail: false,
    },
    // --- Alien group (S20 step 5) --------------------------------------------
    // docs/FFI.md §4 "Representation": `Alien`'s own typed byte-level
    // accessors + constructors (`runtime::alien`'s own module doc has the
    // full design rationale — reused `Format::IndexableBytes` shape, one
    // named external-address field, direct-vs-indirect split). Every one of
    // these validates its own receiver/arg tags exactly like every other
    // group in this table; `can_allocate` is true only for `doubleAt:` (the
    // one allocating accessor — see `runtime::alien::prim_alien_double_at`'s
    // own doc comment) and the two constructors (`new:`/`forAddress:size:`,
    // which allocate the fresh Alien itself).
    PrimDesc {
        id: 112,
        name: "byteAt:",
        f: crate::runtime::alien::prim_alien_byte_at,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 113,
        name: "byteAt:put:",
        f: crate::runtime::alien::prim_alien_byte_at_put,
        argc: 2,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 114,
        name: "signedLongAt:",
        f: crate::runtime::alien::prim_alien_signed_long_at,
        argc: 1,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 115,
        name: "signedLongAt:put:",
        f: crate::runtime::alien::prim_alien_signed_long_at_put,
        argc: 2,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 116,
        name: "doubleAt:",
        f: crate::runtime::alien::prim_alien_double_at,
        argc: 1,
        can_allocate: true,
        can_fail: true,
    },
    PrimDesc {
        id: 117,
        name: "doubleAt:put:",
        f: crate::runtime::alien::prim_alien_double_at_put,
        argc: 2,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 118,
        name: "size",
        f: crate::runtime::alien::prim_alien_size,
        argc: 0,
        can_allocate: false,
        can_fail: true,
    },
    PrimDesc {
        id: 119,
        name: "new:",
        f: crate::runtime::alien::prim_alien_new,
        argc: 1,
        can_allocate: true,
        can_fail: true,
    },
    PrimDesc {
        id: 120,
        name: "forAddress:size:",
        f: crate::runtime::alien::prim_alien_for_address_size,
        argc: 2,
        can_allocate: true,
        can_fail: true,
    },
];

pub fn prim_by_id(id: u16) -> Option<&'static PrimDesc> {
    PRIMITIVES
        .binary_search_by_key(&id, |d| d.id)
        .ok()
        .map(|i| &PRIMITIVES[i])
}

// --- smi group (SPEC §10 appendix) ------------------------------------------

fn smi2(args: &[Oop]) -> Option<(SmallInt, SmallInt)> {
    Some((SmallInt::try_from(args[0])?, SmallInt::try_from(args[1])?))
}

fn prim_add(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    match smi2(args).and_then(|(a, b)| a.checked_add(b)) {
        Some(r) => PrimResult::Ok(r.oop()),
        None => PrimResult::Fail,
    }
}

fn prim_sub(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    match smi2(args).and_then(|(a, b)| a.checked_sub(b)) {
        Some(r) => PrimResult::Ok(r.oop()),
        None => PrimResult::Fail,
    }
}

fn prim_mul(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    match smi2(args).and_then(|(a, b)| a.checked_mul(b)) {
        Some(r) => PrimResult::Ok(r.oop()),
        None => PrimResult::Fail,
    }
}

fn prim_div(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    match smi2(args).and_then(|(a, b)| a.checked_div(b)) {
        Some(r) => PrimResult::Ok(r.oop()),
        None => PrimResult::Fail,
    }
}

fn prim_mod(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    match smi2(args).and_then(|(a, b)| a.checked_rem(b)) {
        Some(r) => PrimResult::Ok(r.oop()),
        None => PrimResult::Fail,
    }
}

fn prim_bit_and(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    match smi2(args) {
        Some((a, b)) => PrimResult::Ok(SmallInt::new(a.value() & b.value()).oop()),
        None => PrimResult::Fail,
    }
}

fn prim_bit_or(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    match smi2(args) {
        Some((a, b)) => PrimResult::Ok(SmallInt::new(a.value() | b.value()).oop()),
        None => PrimResult::Fail,
    }
}

fn prim_bit_xor(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    match smi2(args) {
        Some((a, b)) => PrimResult::Ok(SmallInt::new(a.value() ^ b.value()).oop()),
        None => PrimResult::Fail,
    }
}

/// `count > 0`: left shift (Fail on overflow past the smi range). `count <
/// 0`: arithmetic right shift (never overflows). `|count| >= 62`: Fail
/// regardless of the receiver's value (pinned appendix rule, independent of
/// `SmallInt::checked_shl`'s own range check).
fn prim_bit_shift(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some((r, c)) = smi2(args) else {
        return PrimResult::Fail;
    };
    let count = c.value();
    if !(-61..=61).contains(&count) {
        return PrimResult::Fail;
    }
    if count >= 0 {
        match r.checked_shl(count as u32) {
            Some(v) => PrimResult::Ok(v.oop()),
            None => PrimResult::Fail,
        }
    } else {
        PrimResult::Ok(SmallInt::new(r.value() >> (-count)).oop())
    }
}

fn prim_lt(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    smi_cmp(_vm, args, |a, b| a < b)
}
fn prim_le(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    smi_cmp(_vm, args, |a, b| a <= b)
}
fn prim_gt(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    smi_cmp(_vm, args, |a, b| a > b)
}
fn prim_ge(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    smi_cmp(_vm, args, |a, b| a >= b)
}
fn prim_eq(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    smi_cmp(_vm, args, |a, b| a == b)
}
fn prim_ne(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    smi_cmp(_vm, args, |a, b| a != b)
}

fn smi_cmp(vm: &mut VmState, args: &[Oop], f: impl Fn(i64, i64) -> bool) -> PrimResult {
    match smi2(args) {
        Some((a, b)) => PrimResult::Ok(bool_oop(vm, f(a.value(), b.value()))),
        None => PrimResult::Fail,
    }
}

fn bool_oop(vm: &VmState, b: bool) -> Oop {
    if b {
        vm.universe.true_obj
    } else {
        vm.universe.false_obj
    }
}

// --- oops group --------------------------------------------------------------

pub fn klass_of(vm: &VmState, o: Oop) -> KlassOop {
    crate::runtime::lookup::klass_of(vm, o)
}

fn prim_identity_hash(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let recv = args[0];
    if recv.is_smi() {
        PrimResult::Ok(recv)
    } else {
        let h = vm.universe.identity_hash(recv);
        PrimResult::Ok(SmallInt::new(h as i64).oop())
    }
}

fn prim_class(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    PrimResult::Ok(klass_of(vm, args[0]).oop())
}

fn prim_identical(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    PrimResult::Ok(bool_oop(_vm, args[0].raw() == args[1].raw()))
}

fn prim_basic_new(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(klass) = KlassOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    match klass.format() {
        Format::Slots => PrimResult::Ok(alloc::alloc_slots(vm, klass).oop()),
        Format::IndexableOops => PrimResult::Ok(alloc::alloc_indexable_oops(vm, klass, 0).oop()),
        Format::IndexableBytes => PrimResult::Ok(alloc::alloc_indexable_bytes(vm, klass, 0).oop()),
        _ => PrimResult::Fail,
    }
}

fn prim_basic_new_colon(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(klass) = KlassOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    let Some(n) = SmallInt::try_from(args[1]) else {
        return PrimResult::Fail;
    };
    if n.value() < 0 {
        return PrimResult::Fail;
    }
    let n = n.value() as usize;
    match klass.format() {
        Format::IndexableOops => PrimResult::Ok(alloc::alloc_indexable_oops(vm, klass, n).oop()),
        Format::IndexableBytes => PrimResult::Ok(alloc::alloc_indexable_bytes(vm, klass, n).oop()),
        _ => PrimResult::Fail,
    }
}

fn prim_inst_var_at(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(m) = MemOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    let Some(idx) = SmallInt::try_from(args[1]) else {
        return PrimResult::Fail;
    };
    let i = idx.value();
    let named = (m.klass().non_indexable_size() - HEADER_WORDS) as i64;
    if i < 1 || i > named {
        return PrimResult::Fail;
    }
    PrimResult::Ok(m.body_oop((i - 1) as usize))
}

fn prim_at(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(a) = ArrayOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    let Some(idx) = SmallInt::try_from(args[1]) else {
        return PrimResult::Fail;
    };
    let i = idx.value();
    if i < 1 || i as usize > a.len() {
        return PrimResult::Fail;
    }
    PrimResult::Ok(a.at((i - 1) as usize))
}

fn prim_at_put(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(a) = ArrayOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    let Some(idx) = SmallInt::try_from(args[1]) else {
        return PrimResult::Fail;
    };
    let i = idx.value();
    if i < 1 || i as usize > a.len() {
        return PrimResult::Fail;
    }
    crate::memory::store::store_tail_oop(vm, a.as_mem(), (i - 1) as usize, args[2]);
    PrimResult::Ok(args[2])
}

fn prim_size(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(m) = MemOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    match m.klass().format() {
        Format::IndexableOops | Format::IndexableBytes => {
            PrimResult::Ok(SmallInt::new(m.indexable_len() as i64).oop())
        }
        _ => PrimResult::Fail,
    }
}

// --- bytes group (receiver format must be IndexableBytes) --------------------

fn is_symbol(vm: &VmState, b: ByteArrayOop) -> bool {
    b.as_mem().klass() == vm.universe.symbol_klass
}

fn prim_byte_at(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(b) = ByteArrayOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    let Some(idx) = SmallInt::try_from(args[1]) else {
        return PrimResult::Fail;
    };
    let i = idx.value();
    if i < 1 || i as usize > b.len() {
        return PrimResult::Fail;
    }
    PrimResult::Ok(SmallInt::new(b.byte_at((i - 1) as usize) as i64).oop())
}

fn prim_byte_at_put(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(b) = ByteArrayOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    if is_symbol(vm, b) {
        return PrimResult::Fail;
    }
    let Some(idx) = SmallInt::try_from(args[1]) else {
        return PrimResult::Fail;
    };
    let Some(val) = SmallInt::try_from(args[2]) else {
        return PrimResult::Fail;
    };
    let i = idx.value();
    let v = val.value();
    if i < 1 || i as usize > b.len() {
        return PrimResult::Fail;
    }
    if !(0..=255).contains(&v) {
        return PrimResult::Fail;
    }
    b.byte_at_put((i - 1) as usize, v as u8);
    PrimResult::Ok(args[2])
}

fn prim_byte_size(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(b) = ByteArrayOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    PrimResult::Ok(SmallInt::new(b.len() as i64).oop())
}

/// Copies `arg3[1 .. to-from+1]` (source always addressed from 1,
/// independent of `from`) into `receiver[from..to]`. `to < from` is a
/// no-op. Same-object overlap is handled via an intermediate buffer — the
/// heap accessor discipline (`oops::heap`) never exposes a raw `&mut [u8]`
/// into the heap, so there is no slice to `copy_within` on directly.
fn prim_replace_from_to_with(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(recv) = ByteArrayOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    if is_symbol(vm, recv) {
        return PrimResult::Fail;
    }
    let Some(from) = SmallInt::try_from(args[1]) else {
        return PrimResult::Fail;
    };
    let Some(to) = SmallInt::try_from(args[2]) else {
        return PrimResult::Fail;
    };
    let Some(src) = ByteArrayOop::try_from(args[3]) else {
        return PrimResult::Fail;
    };
    let (from, to) = (from.value(), to.value());
    if to < from {
        return PrimResult::Ok(args[0]);
    }
    if from < 1 {
        return PrimResult::Fail;
    }
    let count = (to - from + 1) as usize;
    if to as usize > recv.len() || count > src.len() {
        return PrimResult::Fail;
    }

    let mut buf = vec![0u8; count];
    for (i, slot) in buf.iter_mut().enumerate() {
        *slot = src.byte_at(i);
    }
    let base = (from - 1) as usize;
    for (i, &b) in buf.iter().enumerate() {
        recv.byte_at_put(base + i, b);
    }
    PrimResult::Ok(args[0])
}

fn prim_hash_bytes(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(b) = ByteArrayOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a 64 offset basis
    for i in 0..b.len() {
        h ^= b.byte_at(i) as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3); // FNV prime
    }
    PrimResult::Ok(SmallInt::new((h & 0x3FFF_FFFF) as i64).oop())
}

fn prim_compare(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(a) = ByteArrayOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    let Some(b) = ByteArrayOop::try_from(args[1]) else {
        return PrimResult::Fail;
    };
    let (la, lb) = (a.len(), b.len());
    for i in 0..la.min(lb) {
        let (ba, bb) = (a.byte_at(i), b.byte_at(i));
        if ba != bb {
            return PrimResult::Ok(SmallInt::new(if ba < bb { -1 } else { 1 }).oop());
        }
    }
    let r = match la.cmp(&lb) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    PrimResult::Ok(SmallInt::new(r).oop())
}

// --- block-value group (SPEC §10, S4) ----------------------------------------
// Layer boundary (sprint_s04_detail.md §Layer boundaries): the arrow from
// runtime/ into interpreter/ is allowed ONLY for this Activated family —
// these primitives never touch InterpRegs directly, they just delegate to
// `interpreter::blocks::activate_block`.

fn prim_value0(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(cl) = crate::oops::wrappers::ClosureOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    crate::interpreter::blocks::activate_block(vm, cl, 0)
}

fn prim_value1(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(cl) = crate::oops::wrappers::ClosureOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    crate::interpreter::blocks::activate_block(vm, cl, 1)
}

fn prim_value2(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(cl) = crate::oops::wrappers::ClosureOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    crate::interpreter::blocks::activate_block(vm, cl, 2)
}

fn prim_value3(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(cl) = crate::oops::wrappers::ClosureOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    crate::interpreter::blocks::activate_block(vm, cl, 3)
}

/// Spreads `args[1]` (an `Array` whose size must equal the block's argc) in
/// place on the operand stack — pure stack surgery, no allocation. Checks
/// the size *before* popping the array (Pitfalls: never allocate/mutate on
/// a still-unvalidated argument).
fn prim_value_with_arguments(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(cl) = crate::oops::wrappers::ClosureOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    let Some(arr) = ArrayOop::try_from(args[1]) else {
        return PrimResult::Fail;
    };
    let n = arr.len();
    if n != cl.method().argc() {
        return PrimResult::Fail;
    }
    let array_slot = vm.prim_arg_base + 1;
    vm.stack.sp = array_slot; // drop the Array argument itself
    for i in 0..n {
        vm.stack.push(arr.at(i));
    }
    crate::interpreter::blocks::activate_block_interp(vm, cl, n)
}

/// SPEC §5.4 Algorithm 6: `ensure:`/`ifCurtailed:` both activate the
/// receiver (the protected block, argc 0), then arm the handler as that
/// *new* activation's marker — the marker lives on the protected block's
/// own frame, never on a frame of its own for `ensure:` itself.
fn prim_ensure_like(
    vm: &mut VmState,
    args: &[Oop],
    kind: crate::interpreter::stack::MarkerKind,
) -> PrimResult {
    let Some(protected) = crate::oops::wrappers::ClosureOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    let Some(handler) = crate::oops::wrappers::ClosureOop::try_from(args[1]) else {
        return PrimResult::Fail;
    };
    if protected.method().argc() != 0 || handler.method().argc() != 0 {
        return PrimResult::Fail;
    }
    // `activate_block` computes the new frame's receiver-arg slot as
    // `sp - argc - 1` on the CALLER's stack — but the caller's stack here
    // still holds the handler as `ensure:`'s own keyword argument, one
    // slot above `protected`. Drop it (already captured in `handler`
    // above) so `sp - 1` lands on `protected`, matching a plain `value`
    // send's stack shape exactly.
    vm.stack.sp -= 1;
    // `activate_block_interp`, NOT the trigger-bearing `activate_block`
    // (S24 A1): the marker set below lives ON the protected block's
    // interpreter frame — it is what `do_return` intercepts for the
    // normal-completion handler run and what `continue_unwind`'s scan
    // finds for the NLR case. The compiled path completes the whole block
    // INSIDE the primitive (no frame, `Completed`/`Nlr`, never
    // `Activated`), so a compiled protected block would skip its handler
    // on BOTH paths.
    match crate::interpreter::blocks::activate_block_interp(vm, protected, 0) {
        PrimResult::Activated => {
            let frame = crate::interpreter::stack::Frame { fp: vm.stack.fp };
            frame.set_marker(&mut vm.stack, handler.oop(), kind);
            PrimResult::Activated
        }
        other => other, // unreachable given the argc pre-check above; propagate defensively
    }
}

fn prim_ensure(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    prim_ensure_like(vm, args, crate::interpreter::stack::MarkerKind::Ensure)
}

fn prim_if_curtailed(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    prim_ensure_like(vm, args, crate::interpreter::stack::MarkerKind::IfCurtailed)
}

// --- system group (dev hooks) --------------------------------------------------

fn prim_quit(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    vm.exit_requested = true;
    PrimResult::Ok(args[0])
}

fn prim_print_on_stdout(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(b) = ByteArrayOop::try_from(args[1]) else {
        return PrimResult::Fail;
    };
    let mut buf = Vec::new();
    b.copy_bytes_out(&mut buf);
    let _ = vm.out.write_all(&buf);
    PrimResult::Ok(args[0])
}

fn prim_millisecond_clock(vm: &mut VmState, _args: &[Oop]) -> PrimResult {
    let millis = vm.start_instant.elapsed().as_millis() as i64;
    PrimResult::Ok(SmallInt::new(millis).oop())
}

/// `Smalltalk gcScavenge` (SPEC §10 system group): runs one young-gen
/// collection, answers the receiver. A stall here is handled exactly like
/// the allocation cascade's own terminal stall (`alloc::stall_exit`) — an
/// explicit `gcScavenge` call finding no way forward is just as fatal as
/// one the allocator triggered itself, not a different situation.
fn prim_gc_scavenge(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    // S12 step 7: S11 D8's defer arm (`if compiled_depth > 0 {
    // request_pending_gc(...); return }`) lived here and is DELETED — a
    // scavenge under a live compiled frame is now an ordinary, fully
    // rooted collection (`roots::each_code_root`), so this primitive
    // always collects immediately, at any depth.
    let _ = args;
    if let Err(err) = crate::memory::scavenge::scavenge(vm) {
        alloc::stall_exit(err);
    }
    // `prim_arg(0)`, NOT `args[0]`: the scavenge above may have MOVED the
    // receiver, and `args` is a pre-call copy of raw bits (SPEC §10
    // Pitfalls — re-read every arg through the live stack slot after any
    // allocating/collecting call). Latent since S8, unreachable until now:
    // every real sender was `Smalltalk gcScavenge`, and the `smalltalk`
    // singleton is tenured by the time user code runs, so the stale copy
    // always happened to equal the live slot. A YOUNG receiver — exactly
    // what `mid_loop_forced_scavenge`'s own `p scav` sends — moves, and
    // returning the stale copy would resurrect its vacated address as a
    // value.
    PrimResult::Ok(vm.prim_arg(0))
}

/// `Smalltalk gcFull` (SPEC §10 system group): runs the full mark-slide-
/// compact collection, answers the receiver. `full_gc` never actually
/// returns `Err` (pure compaction needs no new memory — see its own doc);
/// the `expect` documents that rather than silently discarding a `Result`.
fn prim_gc_full(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    // S12 step 7: defer arm deleted, receiver re-read live — both for
    // exactly `prim_gc_scavenge`'s reasons (see its comments; a full GC's
    // compaction slides young AND old objects, so the stale-`args[0]`
    // hazard is even broader here).
    let _ = args;
    crate::memory::fullgc::full_gc(vm)
        .expect("full_gc: pure compaction never returns Err (see its own doc)");
    PrimResult::Ok(vm.prim_arg(0))
}

/// `Smalltalk gcStats` (SPEC §10 dev hook, S8): answers an 8-element Array
/// of smis in the SPEC-pinned order `(scavengeCount fullGcCount edenUsed
/// oldUsed oldCommitted bytesPromoted markedBytesLast contextAllocs)` —
/// used by the soak harness and S14's Context-elision gate, which both
/// index into it by POSITION, so this order is load-bearing, not
/// cosmetic. `can_allocate: true` (the result Array itself allocates); no
/// arg oops are read after the allocation, so there is nothing here for
/// that to invalidate.
/// R1 reflection (`docs/APPS.md` §3): every class object in the system, as an
/// `Array`. Walks the global namespace (`vm.universe.smalltalk`: slot 0 is the
/// tally, then `tally` Associations) and collects each association value that
/// is a klass — the image-side `ClassMirror` filters these by `superclass` to
/// compute subclasses (the VM keeps no subclass index, exactly as Strongtalk's
/// `ClassVMMirror` walks `Smalltalk classesDo:`). Receiver/args are ignored.
///
/// GC-safe two-pass: pass 1 counts (no allocation); `alloc_indexable_oops` may
/// then scavenge, so pass 2 re-reads `vm.universe.smalltalk` (a scanned root,
/// updated across the collection) rather than caching any oop across the alloc.
fn prim_all_classes(vm: &mut VmState, _args: &[Oop]) -> PrimResult {
    fn tally_of(vm: &VmState) -> (ArrayOop, usize) {
        let arr = ArrayOop::try_from(vm.universe.smalltalk)
            .expect("allClasses: the global namespace is not an Array");
        let tally = SmallInt::try_from(arr.at(0))
            .expect("allClasses: globals tally is not a smi")
            .value() as usize;
        (arr, tally)
    }
    let is_class = |assoc: Oop| -> Option<Oop> {
        let value = MemOop::try_from(assoc)
            .expect("allClasses: globals slot is not an Association")
            .body_oop(1);
        KlassOop::try_from(value).map(|_| value)
    };

    // Pass 1: count (allocation-free).
    let (arr, tally) = tally_of(vm);
    let mut count = 0usize;
    for i in 0..tally {
        if is_class(arr.at(1 + i)).is_some() {
            count += 1;
        }
    }

    // Allocate the result (may move the heap).
    let array_klass = vm.universe.array_klass;
    let result = alloc::alloc_indexable_oops(vm, array_klass, count);

    // Pass 2: re-read the (possibly moved) namespace root and fill.
    let (arr, tally) = tally_of(vm);
    let mut j = 0usize;
    for i in 0..tally {
        if let Some(value) = is_class(arr.at(1 + i)) {
            if j < count {
                result.at_put(j, value);
                j += 1;
            }
        }
    }
    PrimResult::Ok(result.oop())
}

fn prim_gc_stats(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let smi = |v: u64| SmallInt::new(v as i64).oop();
    let u = &vm.universe;
    let fields = [
        smi(u.gc_stats.scavenge_count),
        smi(u.gc_stats.full_gc_count),
        smi((u.eden.top - u.eden.start) as u64),
        smi((u.old.top - u.old.bounds.start) as u64),
        smi((u.old.committed_end - u.old.bounds.start) as u64),
        smi(u.gc_stats.bytes_promoted),
        smi(u.gc_stats.marked_bytes_last),
        smi(u.gc_stats.context_allocs),
    ];
    let array_klass = vm.universe.array_klass;
    let result = alloc::alloc_indexable_oops(vm, array_klass, fields.len());
    for (i, &v) in fields.iter().enumerate() {
        result.at_put(i, v);
    }
    let _ = args;
    PrimResult::Ok(result.oop())
}

/// `__vmStats` (S15 A8 dev hook): answers a 16-element Array of smis in
/// PINNED order — `(icMisses picExtends megaTransitions compilations
/// recompiles recompileDeclined deoptCount deoptTrap deoptReturn deoptPoll
/// osrEntries osrDeclined scavengeCount fullGcCount contextsAllocated
/// bytesPromoted)` — the tier/dispatch/speculation twin of `gcStats`
/// (same positional-array convention, same reason: harnesses index by
/// position, so the order is load-bearing). Values are captured BEFORE
/// the result Array allocates, so the allocation cannot skew them beyond
/// its own GC side effects (which land AFTER the snapshot — acceptable
/// for a diagnostics hook).
fn prim_vm_stats(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let smi = |v: u64| SmallInt::new(v as i64).oop();
    let st = vm.stats;
    let (g_scav, g_full, g_ctx, g_promoted) = (
        vm.universe.gc_stats.scavenge_count,
        vm.universe.gc_stats.full_gc_count,
        vm.universe.gc_stats.context_allocs,
        vm.universe.gc_stats.bytes_promoted,
    );
    let fields = [
        smi(st.ic_misses),
        smi(st.pic_extends),
        smi(st.mega_transitions),
        smi(st.compilations),
        smi(st.recompiles),
        smi(st.recompile_declined_ineffective),
        smi(st.deopt_count),
        smi(st.deopt_by_reason[0]),
        smi(st.deopt_by_reason[1]),
        smi(st.deopt_by_reason[2]),
        smi(st.osr_entries),
        smi(st.osr_declined),
        smi(g_scav),
        smi(g_full),
        smi(g_ctx),
        smi(g_promoted),
    ];
    let array_klass = vm.universe.array_klass;
    let result = alloc::alloc_indexable_oops(vm, array_klass, fields.len());
    for (i, &v) in fields.iter().enumerate() {
        result.at_put(i, v);
    }
    let _ = args;
    PrimResult::Ok(result.oop())
}

/// SPEC §6.3: prints the message + a VM stack trace, terminates. Never
/// returns (its Rust return type is only `PrimResult` so it fits the
/// `PrimFn` signature — `std::process::exit`'s `!` unifies with it).
fn prim_error(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let text = match ByteArrayOop::try_from(args[1]) {
        Some(b) => {
            let mut buf = Vec::new();
            b.copy_bytes_out(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        }
        None => "(non-string error message)".to_string(),
    };
    let _ = writeln!(vm.out, "Error: {text}");
    crate::runtime::error::print_stack_trace(vm);
    let _ = vm.out.flush();
    // DBG1 (docs/DEBUGGER.md §3.1): when the debugger is active, a fatal
    // guest error becomes an inspectable stop — the halt loop opens on the
    // erring activation. The error stays terminal on resume (`error:` has
    // no proceed semantics in v1); the halt is for looking, not healing.
    if vm.debug.active && vm.debug.session_depth == 0 {
        if let Some(m) = vm.regs.method {
            let bci = vm.regs.bci;
            crate::runtime::debug::halt(
                vm,
                m,
                bci,
                crate::runtime::debug::HaltReason::GuestError(format!("Error: {text}")),
            );
        }
    }
    // DBG0 (docs/DEBUGGER.md §4.1): a fatal guest error gets the PROBE
    // mini-dossier — walkback + tier-link/anchor state + recent-history
    // ring + heap verify. No signal machinery: the interpreter is coherent
    // here. `MACVM_PROBE=off` silences it for exact-stderr golden tests.
    // Skipped when a recovery is actually about to happen (embedded
    // `VmHandle::eval`, `deopt_trap::raise_guest_fatal` below) — see
    // `dnu_fallback`'s identical reasoning.
    if !crate::codecache::deopt_trap::has_registered_jmp_slot()
        && crate::runtime::probe::guest_report_enabled()
    {
        crate::runtime::probe::fatal_guest_report(vm, &format!("error: {text}"));
    }
    crate::codecache::deopt_trap::raise_guest_fatal(format!("Error: {text}"))
}

fn prim_quit_colon(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let code = SmallInt::try_from(args[1])
        .map(|s| s.value() as i32)
        .unwrap_or(0);
    vm.exit_requested = true;
    vm.exit_code = Some(code);
    PrimResult::Ok(args[0])
}

// --- Double group (S6, SPEC §1.3) --------------------------------------------

fn double2(
    args: &[Oop],
) -> Option<(
    crate::oops::wrappers::DoubleOop,
    crate::oops::wrappers::DoubleOop,
)> {
    Some((
        crate::oops::wrappers::DoubleOop::try_from(args[0])?,
        crate::oops::wrappers::DoubleOop::try_from(args[1])?,
    ))
}

fn prim_double_add(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    match double2(args) {
        Some((a, b)) => PrimResult::Ok(alloc::alloc_double(vm, a.value() + b.value()).oop()),
        None => PrimResult::Fail,
    }
}

fn prim_double_sub(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    match double2(args) {
        Some((a, b)) => PrimResult::Ok(alloc::alloc_double(vm, a.value() - b.value()).oop()),
        None => PrimResult::Fail,
    }
}

fn prim_double_mul(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    match double2(args) {
        Some((a, b)) => PrimResult::Ok(alloc::alloc_double(vm, a.value() * b.value()).oop()),
        None => PrimResult::Fail,
    }
}

fn prim_double_div(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    match double2(args) {
        // IEEE division by zero yields inf/nan, never fails (pinned,
        // sprint_s06_detail.md §08 Double) — only a non-Double arg fails.
        Some((a, b)) => PrimResult::Ok(alloc::alloc_double(vm, a.value() / b.value()).oop()),
        None => PrimResult::Fail,
    }
}

fn prim_double_lt(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    match double2(args) {
        Some((a, b)) => PrimResult::Ok(bool_oop(vm, a.value() < b.value())),
        None => PrimResult::Fail,
    }
}

fn prim_double_eq(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    match double2(args) {
        Some((a, b)) => PrimResult::Ok(bool_oop(vm, a.value() == b.value())),
        None => PrimResult::Fail,
    }
}

fn prim_double_sqrt(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    match crate::oops::wrappers::DoubleOop::try_from(args[0]) {
        Some(a) => PrimResult::Ok(alloc::alloc_double(vm, a.value().sqrt()).oop()),
        None => PrimResult::Fail,
    }
}

fn prim_double_floor(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
    match crate::oops::wrappers::DoubleOop::try_from(args[0]) {
        Some(a) => {
            let f = a.value().floor();
            if !f.is_finite() || f < SMI_MIN as f64 || f > SMI_MAX as f64 {
                return PrimResult::Fail;
            }
            PrimResult::Ok(SmallInt::new(f as i64).oop())
        }
        None => PrimResult::Fail,
    }
}

fn prim_smi_as_double(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    match SmallInt::try_from(args[0]) {
        Some(s) => PrimResult::Ok(alloc::alloc_double(vm, s.value() as f64).oop()),
        None => PrimResult::Fail,
    }
}

/// Shortest round-trip decimal text for a Double (SPEC §1.3's `printOn:`
/// support) — mirrors `memory::print::print_f64` but without that
/// function's debug-printer framing (no `nan`/`inf` word wrapping beyond
/// what Rust's own `Display` gives; String result, not a Rust `String`
/// consumed by a printer).
fn prim_double_print_digits(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(d) = crate::oops::wrappers::DoubleOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    let v = d.value();
    let text = if v.is_nan() {
        "nan".to_string()
    } else if v.is_infinite() {
        if v > 0.0 { "inf" } else { "-inf" }.to_string()
    } else {
        let s = format!("{v}");
        if s.contains('.') || s.contains('e') || s.contains('E') {
            s
        } else {
            format!("{s}.0")
        }
    };
    let klass = vm.universe.string_klass;
    let b = alloc::alloc_indexable_bytes(vm, klass, text.len());
    for (i, byte) in text.bytes().enumerate() {
        b.byte_at_put(i, byte);
    }
    PrimResult::Ok(b.oop())
}

// --- Symbol group (S6) -----------------------------------------------------

fn prim_string_as_symbol(vm: &mut VmState, args: &[Oop]) -> PrimResult {
    let Some(b) = ByteArrayOop::try_from(args[0]) else {
        return PrimResult::Fail;
    };
    let mut buf = Vec::new();
    b.copy_bytes_out(&mut buf);
    PrimResult::Ok(vm.universe.intern(&buf).oop())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oops::layout::SMI_MAX;
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

    fn smi(v: i64) -> Oop {
        SmallInt::new(v).oop()
    }

    fn call(id: u16, vm: &mut VmState, args: &[Oop]) -> PrimResult {
        (prim_by_id(id).unwrap().f)(vm, args)
    }

    /// `tests_s06.md`'s `prim_ids_frozen`: a regression lock on the id→name
    /// map every `.mst` `<primitive: N>` binds against. This registry
    /// deliberately diverges from `sprint_s06_detail.md`'s suggested
    /// numbering (S3/S4 pinned ids 1-61/90-96 first; the doc's own text
    /// permits this) — the table below is MY pinned numbering, not the
    /// doc's. Adding/renumbering a primitive must update this table
    /// deliberately, not silently.
    #[test]
    fn prim_ids_frozen() {
        let expected: &[(u16, &str)] = &[
            (1, "+"),
            (2, "-"),
            (3, "*"),
            (4, "//"),
            (5, "\\\\"),
            (6, "bitAnd:"),
            (7, "bitOr:"),
            (8, "bitXor:"),
            (9, "bitShift:"),
            (10, "<"),
            (11, "<="),
            (12, ">"),
            (13, ">="),
            (14, "="),
            (15, "~="),
            (20, "identityHash"),
            (21, "class"),
            (22, "=="),
            (23, "basicNew"),
            (24, "basicNew:"),
            (25, "instVarAt:"),
            (26, "at:"),
            (27, "at:put:"),
            (28, "size"),
            (40, "byteAt:"),
            (41, "byteAt:put:"),
            (42, "byteSize"),
            (43, "replaceFrom:to:with:"),
            (44, "hashBytes"),
            (45, "compare:"),
            (50, "value"),
            (51, "value:"),
            (52, "value:value:"),
            (53, "value:value:value:"),
            (54, "valueWithArguments:"),
            (60, "ensure:"),
            (61, "ifCurtailed:"),
            (90, "quit"),
            (91, "printOnStdout:"),
            (92, "millisecondClock"),
            (93, "gcScavenge"),
            (94, "gcFull"),
            (95, "error:"),
            (96, "quit:"),
            (97, "gcStats"),
            (100, "+"),
            (101, "-"),
            (102, "*"),
            (103, "/"),
            (104, "<"),
            (105, "="),
            (106, "sqrt"),
            (107, "floor"),
            (108, "asDouble"),
            (109, "printDigits"),
            (110, "asSymbol"),
            (111, "__vmStats"),
            (112, "byteAt:"),
            (113, "byteAt:put:"),
            (114, "signedLongAt:"),
            (115, "signedLongAt:put:"),
            (116, "doubleAt:"),
            (117, "doubleAt:put:"),
            (118, "size"),
            (119, "new:"),
            (120, "forAddress:size:"),
        ];
        assert_eq!(
            PRIMITIVES.len(),
            expected.len(),
            "PRIMITIVES table grew/shrank without updating prim_ids_frozen"
        );
        for (d, (id, name)) in PRIMITIVES.iter().zip(expected.iter()) {
            assert_eq!(d.id, *id, "id mismatch at {}", d.name);
            assert_eq!(d.name, *name, "name mismatch at id {}", d.id);
        }
    }

    #[test]
    fn vm_stats_prim_shape_and_positions() {
        let mut vm = test_vm();
        vm.stats.ic_misses = 7;
        vm.stats.compilations = 3;
        vm.universe.gc_stats.context_allocs = 9;
        let nil = vm.universe.nil_obj;
        let r = match prim_vm_stats(&mut vm, &[nil]) {
            PrimResult::Ok(o) => crate::oops::wrappers::ArrayOop::try_from(o).expect("an Array"),
            other => panic!("__vmStats must succeed, got {other:?}"),
        };
        assert_eq!(r.len(), 16, "pinned 16-element shape");
        let at = |i: usize| {
            crate::oops::smi::SmallInt::try_from(r.at(i))
                .expect("all smis")
                .value()
        };
        assert_eq!(at(0), 7, "icMisses is position 0 (pinned)");
        assert_eq!(at(3), 3, "compilations is position 3 (pinned)");
        assert_eq!(at(14), 9, "contextsAllocated is position 14 (pinned)");
    }

    #[test]
    fn prim_table_sorted_unique() {
        let mut last: Option<u16> = None;
        for d in PRIMITIVES {
            if let Some(l) = last {
                assert!(d.id > l, "PRIMITIVES not strictly sorted at id {}", d.id);
            }
            last = Some(d.id);
            let colons = d.name.matches(':').count();
            let expected_argc = if colons == 0 { 0 } else { colons };
            // Binary selectors (+, -, ==, etc.) have argc 1 despite 0 colons.
            // `_`-prefixed dev hooks (`__vmStats`) are ordinary unary names,
            // not operators.
            let first = d.name.chars().next().unwrap();
            let is_binary = !first.is_alphabetic() && first != '_';
            if is_binary {
                assert_eq!(d.argc, 1, "{} expected argc 1", d.name);
            } else {
                assert_eq!(
                    d.argc as usize, expected_argc,
                    "{} argc/colon mismatch",
                    d.name
                );
            }
        }
    }

    #[test]
    fn prim_smi_matrix() {
        let mut vm = test_vm();
        assert_eq!(call(1, &mut vm, &[smi(2), smi(3)]), PrimResult::Ok(smi(5)));
        assert_eq!(call(1, &mut vm, &[smi(SMI_MAX), smi(1)]), PrimResult::Fail);
        let nil = vm.universe.nil_obj;
        assert_eq!(call(1, &mut vm, &[nil, smi(1)]), PrimResult::Fail);

        assert_eq!(call(2, &mut vm, &[smi(5), smi(3)]), PrimResult::Ok(smi(2)));
        assert_eq!(call(3, &mut vm, &[smi(5), smi(3)]), PrimResult::Ok(smi(15)));
        assert_eq!(
            call(6, &mut vm, &[smi(0b110), smi(0b011)]),
            PrimResult::Ok(smi(0b010))
        );
        assert_eq!(
            call(7, &mut vm, &[smi(0b110), smi(0b011)]),
            PrimResult::Ok(smi(0b111))
        );
        assert_eq!(
            call(8, &mut vm, &[smi(0b110), smi(0b011)]),
            PrimResult::Ok(smi(0b101))
        );

        assert_eq!(
            call(10, &mut vm, &[smi(1), smi(2)]),
            PrimResult::Ok(vm.universe.true_obj)
        );
        assert_eq!(
            call(10, &mut vm, &[smi(2), smi(1)]),
            PrimResult::Ok(vm.universe.false_obj)
        );
        assert_eq!(
            call(14, &mut vm, &[smi(2), smi(2)]),
            PrimResult::Ok(vm.universe.true_obj)
        );
        assert_eq!(
            call(15, &mut vm, &[smi(2), smi(2)]),
            PrimResult::Ok(vm.universe.false_obj)
        );
    }

    #[test]
    fn prim_floored_division() {
        let mut vm = test_vm();
        assert_eq!(
            call(4, &mut vm, &[smi(-7), smi(2)]),
            PrimResult::Ok(smi(-4))
        );
        assert_eq!(
            call(4, &mut vm, &[smi(7), smi(-2)]),
            PrimResult::Ok(smi(-4))
        );
        assert_eq!(call(5, &mut vm, &[smi(-7), smi(2)]), PrimResult::Ok(smi(1)));
        assert_eq!(
            call(5, &mut vm, &[smi(7), smi(-2)]),
            PrimResult::Ok(smi(-1))
        );
        assert_eq!(call(4, &mut vm, &[smi(7), smi(0)]), PrimResult::Fail);
    }

    #[test]
    fn prim_bitshift_edges() {
        let mut vm = test_vm();
        assert_eq!(call(9, &mut vm, &[smi(1), smi(61)]), PrimResult::Fail);
        assert_eq!(
            call(9, &mut vm, &[smi(1), smi(60)]),
            PrimResult::Ok(smi(1i64 << 60))
        );
        assert_eq!(call(9, &mut vm, &[smi(1), smi(62)]), PrimResult::Fail);
        assert_eq!(call(9, &mut vm, &[smi(1), smi(-62)]), PrimResult::Fail);
        assert_eq!(
            call(9, &mut vm, &[smi(-8), smi(-1)]),
            PrimResult::Ok(smi(-4))
        );
    }

    #[test]
    fn prim_smi_overflow_fails() {
        let mut vm = test_vm();
        assert_eq!(call(1, &mut vm, &[smi(SMI_MAX), smi(1)]), PrimResult::Fail);
    }

    #[test]
    fn prim_identity_hash_stable() {
        let mut vm = test_vm();
        let object_klass = vm.universe.object_klass;
        let a = alloc::alloc_slots(&mut vm, object_klass).oop();
        let b = alloc::alloc_slots(&mut vm, object_klass).oop();
        let h1 = call(20, &mut vm, &[a]);
        let h1_again = call(20, &mut vm, &[a]);
        assert_eq!(h1, h1_again);
        let h2 = call(20, &mut vm, &[b]);
        assert_ne!(h1, h2);
    }

    #[test]
    fn prim_oops_matrix() {
        let mut vm = test_vm();
        let array_klass = vm.universe.array_klass;
        let double_klass = vm.universe.double_klass;

        // basicNew on an indexable klass succeeds with a size-0 instance;
        // basicNew on a Double-format klass fails (excluded format).
        match call(23, &mut vm, &[array_klass.oop()]) {
            PrimResult::Ok(o) => {
                let a = ArrayOop::try_from(o).expect("basicNew on Array must yield an ArrayOop");
                assert_eq!(a.len(), 0);
            }
            PrimResult::Fail => panic!("basicNew on Array must not fail"),
            PrimResult::Activated => unreachable!("basicNew never activates"),
            PrimResult::Nlr(_) => unreachable!("basicNew never NLRs"),
        }
        assert_eq!(call(23, &mut vm, &[double_klass.oop()]), PrimResult::Fail);

        let arr = alloc::alloc_indexable_oops(&mut vm, array_klass, 3).oop();
        assert_eq!(call(28, &mut vm, &[arr]), PrimResult::Ok(smi(3)));
        assert_eq!(
            call(26, &mut vm, &[arr, smi(1)]),
            PrimResult::Ok(vm.universe.nil_obj)
        );
        assert_eq!(
            call(27, &mut vm, &[arr, smi(1), smi(9)]),
            PrimResult::Ok(smi(9))
        );
        assert_eq!(call(26, &mut vm, &[arr, smi(1)]), PrimResult::Ok(smi(9)));
        assert_eq!(call(26, &mut vm, &[arr, smi(4)]), PrimResult::Fail);

        let bytearray_klass = vm.universe.bytearray_klass;
        let bytes = alloc::alloc_indexable_bytes(&mut vm, bytearray_klass, 2).oop();
        assert_eq!(call(26, &mut vm, &[bytes, smi(1)]), PrimResult::Fail);
    }

    #[test]
    fn prim_bytes_matrix() {
        let mut vm = test_vm();
        let bytearray_klass = vm.universe.bytearray_klass;
        let b = alloc::alloc_indexable_bytes(&mut vm, bytearray_klass, 3).oop();
        assert_eq!(
            call(41, &mut vm, &[b, smi(1), smi(65)]),
            PrimResult::Ok(smi(65))
        );
        assert_eq!(call(40, &mut vm, &[b, smi(1)]), PrimResult::Ok(smi(65)));
        assert_eq!(call(41, &mut vm, &[b, smi(1), smi(300)]), PrimResult::Fail);

        let sym = vm.universe.intern(b"foo").oop();
        assert_eq!(call(41, &mut vm, &[sym, smi(1), smi(1)]), PrimResult::Fail);
    }

    /// The primitive's source is always addressed from 1, so the only
    /// same-object overlap hazard this signature can produce is `from > 1`
    /// (dest shifted right of source — a naive left-to-right in-place copy
    /// would read already-overwritten bytes). `from == 1` is an exact
    /// self-overlap (dest == source) and must be a safe identity copy.
    /// Both are covered here since the buffer-based implementation must get
    /// both right.
    #[test]
    fn prim_replace_overlap_forward_back() {
        let mut vm = test_vm();
        let bytearray_klass = vm.universe.bytearray_klass;
        let b = alloc::alloc_indexable_bytes(&mut vm, bytearray_klass, 5).oop();
        for (i, v) in [1u8, 2, 3, 4, 5].into_iter().enumerate() {
            let _ = call(41, &mut vm, &[b, smi(i as i64 + 1), smi(v as i64)]);
        }
        // Shift +1: replaceFrom:2 to:5 with: self (same object) — copies
        // self[1..4] into self[2..5].
        let _ = call(43, &mut vm, &[b, smi(2), smi(5), b]);
        for (i, expect) in [1u8, 1, 2, 3, 4].into_iter().enumerate() {
            assert_eq!(
                call(40, &mut vm, &[b, smi(i as i64 + 1)]),
                PrimResult::Ok(smi(expect as i64))
            );
        }

        // Exact self-overlap: replaceFrom:1 to:5 with: self is an identity
        // copy (source and destination ranges coincide).
        let _ = call(43, &mut vm, &[b, smi(1), smi(5), b]);
        for (i, expect) in [1u8, 1, 2, 3, 4].into_iter().enumerate() {
            assert_eq!(
                call(40, &mut vm, &[b, smi(i as i64 + 1)]),
                PrimResult::Ok(smi(expect as i64))
            );
        }
    }

    #[test]
    fn prim_compare_prefix() {
        let mut vm = test_vm();
        let ab = vm.universe.intern(b"ab").oop();
        let abc = vm.universe.intern(b"abc").oop();
        assert_eq!(call(45, &mut vm, &[ab, abc]), PrimResult::Ok(smi(-1)));
        assert_eq!(call(45, &mut vm, &[abc, ab]), PrimResult::Ok(smi(1)));
        assert_eq!(call(45, &mut vm, &[ab, ab]), PrimResult::Ok(smi(0)));
    }

    /// A trivial `CompiledBlock` closure with `argc` args and no captures —
    /// enough shape to drive `activate_block`'s argc check and frame push
    /// without needing any real computation inside the block body.
    fn make_block_closure(vm: &mut VmState, argc: usize) -> Oop {
        let mut home = crate::bytecode::BytecodeBuilder::new();
        let lit = home.build_block(vm, argc, argc, false, 0, false, |b, _vm| {
            b.ret_self();
        });
        home.push_closure(lit, 0);
        home.ret_tos();
        let sel = vm.universe.intern(format!("mk{argc}").as_bytes());
        let method = home.finish(vm, sel, 0, 0);
        let recv = vm.universe.nil_obj;
        crate::interpreter::run_method(vm, method, recv, &[])
    }

    #[test]
    fn value_family_matrix() {
        for argc in 0usize..=3 {
            let mut vm = test_vm();
            let closure_oop = make_block_closure(&mut vm, argc);
            vm.stack.push(closure_oop);
            for _ in 0..argc {
                vm.stack.push(vm.universe.nil_obj);
            }
            vm.prim_arg_base = vm.stack.sp - argc - 1;
            let mut buf = [vm.universe.nil_obj; 6];
            for (i, slot) in buf.iter_mut().enumerate().take(argc + 1) {
                *slot = vm.stack.get(vm.prim_arg_base + i);
            }
            let prim_id = 50 + argc as u16;
            let result = call(prim_id, &mut vm, &buf[..=argc]);
            assert_eq!(result, PrimResult::Activated, "argc={argc}");
        }

        // Non-closure receiver -> Fail, for every arity in the family.
        for argc in 0usize..=3 {
            let mut vm = test_vm();
            let prim_id = 50 + argc as u16;
            let mut buf = [smi(1); 4];
            buf[0] = vm.universe.nil_obj; // not a Closure
            let result = call(prim_id, &mut vm, &buf[..=argc]);
            assert_eq!(result, PrimResult::Fail, "argc={argc}");
        }
    }

    #[test]
    fn value_with_arguments() {
        let mut vm = test_vm();
        let closure_oop = make_block_closure(&mut vm, 2);
        let array_klass = vm.universe.array_klass;
        let arr = crate::memory::alloc::alloc_indexable_oops(&mut vm, array_klass, 2);
        arr.at_put(0, smi(10));
        arr.at_put(1, smi(20));

        vm.stack.push(closure_oop);
        vm.stack.push(arr.oop());
        vm.prim_arg_base = vm.stack.sp - 2;
        let buf = [closure_oop, arr.oop()];
        let result = call(54, &mut vm, &buf);
        assert_eq!(result, PrimResult::Activated);
        // The array's 2 elements must have been spread onto the stack in
        // place of the array itself.
        let fp = vm.stack.fp;
        let frame = crate::interpreter::stack::Frame { fp };
        assert_eq!(frame.temp(&vm.stack, 0), smi(10));
        assert_eq!(frame.temp(&vm.stack, 1), smi(20));

        // Size mismatch -> Fail, no stack surgery.
        let mut vm2 = test_vm();
        let closure_oop2 = make_block_closure(&mut vm2, 2);
        let array_klass2 = vm2.universe.array_klass;
        let arr2 = crate::memory::alloc::alloc_indexable_oops(&mut vm2, array_klass2, 3);
        vm2.stack.push(closure_oop2);
        vm2.stack.push(arr2.oop());
        vm2.prim_arg_base = vm2.stack.sp - 2;
        let sp_before = vm2.stack.sp;
        let buf2 = [closure_oop2, arr2.oop()];
        assert_eq!(call(54, &mut vm2, &buf2), PrimResult::Fail);
        assert_eq!(vm2.stack.sp, sp_before, "Fail must not touch the stack");

        // Non-Array argument -> Fail.
        let mut vm3 = test_vm();
        let closure_oop3 = make_block_closure(&mut vm3, 1);
        let buf3 = [closure_oop3, smi(5)];
        assert_eq!(call(54, &mut vm3, &buf3), PrimResult::Fail);
    }

    /// S6: `instVarAt:` (p25) on a `Klass`-format receiver reads its 8
    /// named fields 1-based (SPEC §2.4 order) — Behavior's accessors
    /// (`name`, `superclass`, `instVarNames`, …) depend on this.
    #[test]
    fn prim_instvarat_klass() {
        let mut vm = test_vm();
        let object_klass = vm.universe.object_klass;
        let sym = object_klass.name();
        assert_eq!(
            call(25, &mut vm, &[object_klass.oop(), smi(5)]),
            PrimResult::Ok(sym)
        );
        let sup = object_klass.superclass();
        assert_eq!(
            call(25, &mut vm, &[object_klass.oop(), smi(3)]),
            PrimResult::Ok(sup)
        );
    }

    #[test]
    fn prim_double_matrix() {
        let mut vm = test_vm();
        let d1 = alloc::alloc_double(&mut vm, 1.5).oop();
        let d2 = alloc::alloc_double(&mut vm, 2.5).oop();
        let true_obj = vm.universe.true_obj;
        match call(100, &mut vm, &[d1, d2]) {
            PrimResult::Ok(o) => {
                assert_eq!(
                    crate::oops::wrappers::DoubleOop::try_from(o)
                        .unwrap()
                        .value(),
                    4.0
                )
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        assert_eq!(call(104, &mut vm, &[d1, d2]), PrimResult::Ok(true_obj));
        assert_eq!(call(105, &mut vm, &[d1, d1]), PrimResult::Ok(true_obj));
        // Non-Double arg fails (Smalltalk fallback coerces, SPEC §1.3).
        assert_eq!(call(100, &mut vm, &[d1, smi(1)]), PrimResult::Fail);

        let big = alloc::alloc_double(&mut vm, 1e10).oop();
        let dgt = call(109, &mut vm, &[big]);
        match dgt {
            PrimResult::Ok(o) => {
                let b = ByteArrayOop::try_from(o).unwrap();
                let mut buf = Vec::new();
                b.copy_bytes_out(&mut buf);
                assert_eq!(String::from_utf8(buf).unwrap(), "10000000000.0");
            }
            other => panic!("expected Ok, got {other:?}"),
        }

        match call(108, &mut vm, &[smi(3)]) {
            PrimResult::Ok(o) => {
                assert_eq!(
                    crate::oops::wrappers::DoubleOop::try_from(o)
                        .unwrap()
                        .value(),
                    3.0
                )
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// `tests_s06.md`'s `prim_double_print`: p109 round-trips the shortest
    /// decimal form for a handful of adversarial values (`prim_error`'s own
    /// trace/exit-1 path can't be unit-tested in-process — it calls
    /// `std::process::exit`, so that requirement is covered at the CLI
    /// integration layer instead, e.g. `tests/it_world.rs`).
    #[test]
    fn prim_double_print() {
        let mut vm = test_vm();
        let cases: &[(f64, &str)] = &[
            (0.1, "0.1"),
            (1e10, "10000000000.0"),
            (1.5e-3, "0.0015"),
            (-0.0, "-0.0"),
        ];
        for (v, expected) in cases {
            let d = alloc::alloc_double(&mut vm, *v).oop();
            match call(109, &mut vm, &[d]) {
                PrimResult::Ok(o) => {
                    let b = ByteArrayOop::try_from(o).unwrap();
                    let mut buf = Vec::new();
                    b.copy_bytes_out(&mut buf);
                    assert_eq!(String::from_utf8(buf).unwrap(), *expected, "for {v}");
                }
                other => panic!("expected Ok for {v}, got {other:?}"),
            }
        }
    }

    #[test]
    fn prim_quit_colon_sets_exit_code() {
        let mut vm = test_vm();
        let recv = vm.universe.nil_obj;
        assert_eq!(call(96, &mut vm, &[recv, smi(7)]), PrimResult::Ok(recv));
        assert!(vm.exit_requested);
        assert_eq!(vm.exit_code, Some(7));
    }

    #[test]
    fn prim_string_as_symbol_interns() {
        let mut vm = test_vm();
        let bytearray_klass = vm.universe.bytearray_klass;
        let s = alloc::alloc_indexable_bytes(&mut vm, bytearray_klass, 3);
        s.byte_at_put(0, b'f');
        s.byte_at_put(1, b'o');
        s.byte_at_put(2, b'o');
        let result = call(110, &mut vm, &[s.oop()]);
        let expected = vm.universe.intern(b"foo").oop();
        assert_eq!(result, PrimResult::Ok(expected));
    }

    /// S12 step 7: the GC prims re-read their receiver through
    /// `vm.prim_arg(0)` after collecting (the `args` copy is stale bits
    /// once the receiver itself moves — latent since S8, only reachable
    /// now that a collection can actually run mid-prim with young
    /// receivers). The bare `call` helper above invokes the prim fn
    /// directly with an unrooted slice, so these tests must mimic
    /// `try_primitive`'s own protocol: push the receiver onto the live
    /// stack and point `prim_arg_base` at it, exactly like a real send.
    fn call_rooted(id: u16, vm: &mut VmState, recv: Oop) -> PrimResult {
        let base = vm.stack.sp;
        vm.stack.push(recv);
        vm.prim_arg_base = base;
        let r = (prim_by_id(id).unwrap().f)(vm, &[recv]);
        vm.stack.sp = base;
        r
    }

    /// `Smalltalk gcScavenge` must run a REAL scavenge (id 93), not the old
    /// no-op stub — proven by the counter it bumps, not just "didn't crash".
    /// The answered receiver must be the LIVE (post-scavenge) nil — a young
    /// receiver moves, and the pre-call `recv` bits are the vacated address.
    #[test]
    fn prim_gc_scavenge_runs_a_real_scavenge() {
        let mut vm = test_vm();
        let recv = vm.universe.nil_obj;
        let before = vm.universe.gc_stats.scavenge_count;
        let result = call_rooted(93, &mut vm, recv);
        assert_eq!(vm.universe.gc_stats.scavenge_count, before + 1);
        assert_eq!(
            result,
            PrimResult::Ok(vm.universe.nil_obj),
            "must answer the RELOCATED receiver (nil moved -- it was young)"
        );
        assert_ne!(
            vm.universe.nil_obj.raw(),
            recv.raw(),
            "precondition: genesis nil is young, so the scavenge must actually move it \
             (otherwise this test proves nothing about the stale-args hazard)"
        );
    }

    /// `Smalltalk gcFull` must run a REAL full GC (id 94) — same shape as
    /// the scavenge test above, this time against `fullGcCount`.
    #[test]
    fn prim_gc_full_runs_a_real_full_gc() {
        let mut vm = test_vm();
        let recv = vm.universe.nil_obj;
        let before = vm.universe.gc_stats.full_gc_count;
        let result = call_rooted(94, &mut vm, recv);
        assert_eq!(vm.universe.gc_stats.full_gc_count, before + 1);
        assert_eq!(result, PrimResult::Ok(vm.universe.nil_obj));
    }

    /// S12 step 7 (inverts S11's `prim_gc_scavenge_defers_under_compiled_
    /// frame`): the D8 defer arm is gone, so `gcScavenge` runs a REAL
    /// scavenge immediately even with `compiled_depth > 0` (a faked depth
    /// with an empty native stack is honest here: `walk_frames` starts
    /// from the anchor, which is 0, so the code-root walk correctly sees
    /// no native frames). `gc_under_compiled` must count the hard case.
    #[test]
    fn prim_gc_scavenge_runs_immediately_under_compiled_depth() {
        let mut vm = test_vm();
        let recv = vm.universe.nil_obj;
        let before = vm.universe.gc_stats.scavenge_count;
        let under_before = vm.universe.gc_stats.gc_under_compiled;
        vm.compiled_depth = 1;
        let result = call_rooted(93, &mut vm, recv);
        vm.compiled_depth = 0;
        assert_eq!(result, PrimResult::Ok(vm.universe.nil_obj));
        assert_eq!(
            vm.universe.gc_stats.scavenge_count,
            before + 1,
            "the defer arm is gone -- gcScavenge collects immediately at any depth"
        );
        assert_eq!(vm.universe.gc_stats.gc_under_compiled, under_before + 1);
    }

    /// Sibling of the scavenge test above, for `gcFull` (id 94).
    #[test]
    fn prim_gc_full_runs_immediately_under_compiled_depth() {
        let mut vm = test_vm();
        let recv = vm.universe.nil_obj;
        let before = vm.universe.gc_stats.full_gc_count;
        vm.compiled_depth = 1;
        let result = call_rooted(94, &mut vm, recv);
        vm.compiled_depth = 0;
        assert_eq!(result, PrimResult::Ok(vm.universe.nil_obj));
        assert_eq!(vm.universe.gc_stats.full_gc_count, before + 1);
    }

    /// `Smalltalk gcStats` (id 97) answers an 8-smi Array in the SPEC-pinned
    /// order — checked both structurally (an Array of exactly 8 smis) and
    /// against the actual counters after a real scavenge + full GC, so a
    /// field silently swapped to the wrong position would fail this, not
    /// just "returns something array-shaped".
    #[test]
    fn prim_gc_stats_reports_pinned_fields_in_order() {
        let mut vm = test_vm();
        let recv = vm.universe.nil_obj;
        call_rooted(93, &mut vm, recv); // one scavenge

        // Re-read the LIVE nil for the second call: the scavenge above
        // moved it, and pushing the stale pre-scavenge `recv` bits as a
        // root is exactly the dangling-root shape the full-gc entry
        // verifier (correctly) rejects — this test tripped it for real
        // the first time this line reused `recv`.
        let recv2 = vm.universe.nil_obj;
        call_rooted(94, &mut vm, recv2); // one full GC

        // Snapshot expectations BEFORE calling gcStats: the primitive
        // itself allocates its own result Array (in eden), so reading
        // eden's usage back out AFTER the call would include that very
        // allocation — gcStats correctly reports the state as of the
        // moment it was called, not after its own side effect.
        let expected = (
            vm.universe.gc_stats.scavenge_count as i64,
            vm.universe.gc_stats.full_gc_count as i64,
            (vm.universe.eden.top - vm.universe.eden.start) as i64,
            (vm.universe.old.top - vm.universe.old.bounds.start) as i64,
            (vm.universe.old.committed_end - vm.universe.old.bounds.start) as i64,
            vm.universe.gc_stats.bytes_promoted as i64,
            vm.universe.gc_stats.marked_bytes_last as i64,
            vm.universe.gc_stats.context_allocs as i64,
        );

        let result = call(97, &mut vm, &[recv]);
        let PrimResult::Ok(oop) = result else {
            panic!("gcStats must succeed, got {result:?}")
        };
        let arr = ArrayOop::try_from(oop).expect("gcStats must answer an Array");
        assert_eq!(arr.len(), 8);
        let as_i64 = |i: usize| {
            SmallInt::try_from(arr.at(i))
                .expect("every field is a smi")
                .value()
        };

        assert_eq!(as_i64(0), expected.0, "scavengeCount");
        assert_eq!(as_i64(1), expected.1, "fullGcCount");
        assert_eq!(as_i64(2), expected.2, "edenUsed");
        assert_eq!(as_i64(3), expected.3, "oldUsed");
        assert_eq!(as_i64(4), expected.4, "oldCommitted");
        assert_eq!(as_i64(5), expected.5, "bytesPromoted");
        assert_eq!(as_i64(6), expected.6, "markedBytesLast");
        assert_eq!(as_i64(7), expected.7, "contextAllocs");
    }
}
