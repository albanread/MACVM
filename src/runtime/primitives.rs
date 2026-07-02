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
use crate::oops::layout::HEADER_WORDS;
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

fn prim_at_put(_vm: &mut VmState, args: &[Oop]) -> PrimResult {
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
    a.at_put((i - 1) as usize, args[2]);
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
    crate::interpreter::blocks::activate_block(vm, cl, n)
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
    match crate::interpreter::blocks::activate_block(vm, protected, 0) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oops::layout::SMI_MAX;
    use crate::runtime::vm_state::{VmOptions, VmState};

    fn test_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
        })
    }

    fn smi(v: i64) -> Oop {
        SmallInt::new(v).oop()
    }

    fn call(id: u16, vm: &mut VmState, args: &[Oop]) -> PrimResult {
        (prim_by_id(id).unwrap().f)(vm, args)
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
            let is_binary = !d.name.chars().next().unwrap().is_alphabetic();
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
        let lit = home.build_block(vm, argc, argc, false, 0, false, |b| {
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
}
