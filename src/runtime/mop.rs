//! MOP — the MACVM Object Pickle (docs/multi-smalltalk-worker.md §4, M0).
//!
//! A self-contained, versioned, tagged binary encoding of one object graph,
//! built for the multi-Smalltalk-worker facility: everything that crosses a
//! VM boundary is a *deep copy* carried as plain bytes, so no oop is ever
//! visible to two VMs. This module is deliberately testable in ONE VM with
//! zero threads (pickle → unpickle round-trip), which is milestone M0.
//!
//! Wire form (all multi-byte integers are LEB128 varints; `zigzag` for
//! signed): `MAGIC('M','O','P',1)` then one `object`:
//!
//! | tag | payload |
//! |-----|---------|
//! | 0 nil / 1 false / 2 true | — |
//! | 3 SmallInteger | zigzag varint |
//! | 4 Double | 8 bytes IEEE-754 bits, little-endian |
//! | 5 Character | varint code point |
//! | 6 Symbol | varint len + UTF-8 bytes (re-interned on rebuild) |
//! | 7 String | varint len + bytes |
//! | 8 ByteArray | varint len + bytes |
//! | 9 FloatArray | varint lanes + lanes×8 bytes |
//! | 10 Array | varint n + n×object |
//! | 11 Object | varint namelen + name + varint nslots + subkind u8 (0 slots-only / 1 +oops / 2 +bytes) + [varint n] + nslots×object + [payload] |
//! | 12 BackRef | varint index into the encode-order object table |
//! | 13 LargeInteger | sign u8 (0 pos / 1 neg) + varint len + magnitude bytes |
//!
//! Tag 11's header (name/nslots/subkind/n) precedes its slot objects so the
//! decoder can allocate the object FIRST and only then descend — spine-first
//! construction, which is what makes shared substructure and cycles work
//! (`BackRef` always has a live target, even mid-cycle; the S14 materializer
//! pending-graph lesson). Every heap object gets a table index in first-
//! encounter order; a revisit emits `BackRef`, so sharing within a message is
//! preserved exactly.
//!
//! GC safety: `pickle` takes `&VmState` — it cannot allocate, so oop
//! addresses are stable for its identity map. `unpickle` allocates freely;
//! every rebuilt oop lives in a [`HandleScope`] for the duration, so a moving
//! collection mid-build rewrites the working set (fillers always re-fetch the
//! container through its handle after any child decode).
//!
//! Refused at pickle time (`PickleErr::Unpicklable`): blocks, contexts,
//! methods, classes/metaclasses, processes, and `Alien` (a raw pointer is
//! meaningless in another heap). Class-named objects rebuild by
//! `global_lookup` + a positional shape check (`UnpickleErr::ShapeMismatch`
//! if the receiving world's class disagrees).

use crate::memory::alloc;
use crate::memory::handles::{Handle, HandleScope};
use crate::oops::klass::Format;
use crate::oops::layout::{HEADER_WORDS, SMI_MAX, SMI_MIN};
use crate::oops::smi::SmallInt;
use crate::oops::wrappers::{ArrayOop, ByteArrayOop, DoubleOop, KlassOop, MemOop, SymbolOop};
use crate::oops::Oop;
use crate::runtime::globals::global_lookup;
use crate::runtime::vm_state::VmState;
use std::collections::HashMap;

const MAGIC: [u8; 4] = [b'M', b'O', b'P', 1];

/// Guard rails (docs/multi-smalltalk-worker.md §4.1): an accidental
/// `send: Smalltalk` fails fast instead of OOMing.
pub const MAX_BYTES: usize = 64 << 20;
pub const MAX_OBJECTS: usize = 1_000_000;
/// Both walks are recursive, so the cap must fit the RUST stack, not just
/// taste: a test thread gets 2 MiB and a debug-build frame here is ~600 B,
/// so 10k recursions overflow before the guard fires (measured — the
/// too-deep test caught exactly that). 1 000 nested *containers* is already
/// far beyond any sane message while staying ~600 KiB of stack worst-case.
pub const MAX_DEPTH: usize = 1_000;

const TAG_NIL: u8 = 0;
const TAG_FALSE: u8 = 1;
const TAG_TRUE: u8 = 2;
const TAG_SMI: u8 = 3;
const TAG_DOUBLE: u8 = 4;
const TAG_CHAR: u8 = 5;
const TAG_SYMBOL: u8 = 6;
const TAG_STRING: u8 = 7;
const TAG_BYTE_ARRAY: u8 = 8;
const TAG_FLOAT_ARRAY: u8 = 9;
const TAG_ARRAY: u8 = 10;
const TAG_OBJECT: u8 = 11;
const TAG_BACKREF: u8 = 12;
const TAG_LARGE_INT: u8 = 13;

const SUBKIND_SLOTS: u8 = 0;
const SUBKIND_OOPS: u8 = 1;
const SUBKIND_BYTES: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickleErr {
    /// The graph contains something that must not cross a VM boundary.
    Unpicklable(&'static str),
    TooDeep,
    TooBig,
    TooManyObjects,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnpickleErr {
    BadMagic,
    BadTag(u8),
    /// Input ended (or a declared length overran the input) — the fuzz
    /// contract: truncation is always an error, never a panic.
    Truncated,
    Malformed(&'static str),
    UnknownClass(String),
    NotAClass(String),
    ShapeMismatch(String),
    TooDeep,
    TooManyObjects,
    BadBackRef,
}

// ── encoding helpers ────────────────────────────────────────────────────────

fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cursor<'a> {
    fn u8(&mut self) -> Result<u8, UnpickleErr> {
        let v = *self.b.get(self.i).ok_or(UnpickleErr::Truncated)?;
        self.i += 1;
        Ok(v)
    }

    fn exact(&mut self, n: usize) -> Result<&'a [u8], UnpickleErr> {
        let end = self.i.checked_add(n).ok_or(UnpickleErr::Truncated)?;
        if end > self.b.len() {
            return Err(UnpickleErr::Truncated);
        }
        let s = &self.b[self.i..end];
        self.i = end;
        Ok(s)
    }

    fn varint(&mut self) -> Result<u64, UnpickleErr> {
        let mut v: u64 = 0;
        for shift in 0..10 {
            let byte = self.u8()?;
            v |= u64::from(byte & 0x7f) << (7 * shift);
            if byte & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(UnpickleErr::Malformed("varint too long"))
    }

    /// A declared element/byte count, sanity-checked against the remaining
    /// input so a corrupt length can never drive a huge allocation.
    fn len_capped(&mut self, min_bytes_per: usize) -> Result<usize, UnpickleErr> {
        let n = self.varint()? as usize;
        let remaining = self.b.len() - self.i;
        if n.checked_mul(min_bytes_per.max(1))
            .is_none_or(|need| need > remaining)
        {
            return Err(UnpickleErr::Truncated);
        }
        Ok(n)
    }

    fn remaining(&self) -> usize {
        self.b.len() - self.i
    }
}

// ── pickle ──────────────────────────────────────────────────────────────────

/// Serialize `root`'s object graph. `&VmState` (not `&mut`) is load-bearing:
/// pickling cannot allocate, so the identity map's raw-address keys stay
/// stable for the whole walk.
pub fn pickle(vm: &VmState, root: Oop) -> Result<Vec<u8>, PickleErr> {
    pickle_with_limits(vm, root, MAX_BYTES, MAX_OBJECTS)
}

/// The cap-parameterized core, exposed so tests can exercise the guards
/// without building 64 MiB graphs.
pub fn pickle_with_limits(
    vm: &VmState,
    root: Oop,
    max_bytes: usize,
    max_objects: usize,
) -> Result<Vec<u8>, PickleErr> {
    let mut out = MAGIC.to_vec();
    let mut seen: HashMap<u64, u32> = HashMap::new();
    pk(vm, root, &mut out, &mut seen, 0, max_bytes, max_objects)?;
    Ok(out)
}

fn read_bytes_out(o: Oop) -> Vec<u8> {
    let b = ByteArrayOop::try_from(o).expect("caller checked IndexableBytes format");
    let mut buf = Vec::new();
    b.copy_bytes_out(&mut buf);
    buf
}

/// A hand-built control message for the worker registry (workers M1, §8):
/// the MOP encoding of `{#workerDied. id}`, produced Rust-side (no VM in
/// hand — the sender may be a dying worker thread) yet unpickling exactly
/// like any guest-made message. One delivery mechanism for everything.
pub(crate) fn encode_worker_died(id: i64) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.push(TAG_ARRAY);
    write_varint(&mut out, 2);
    let name = b"workerDied";
    out.push(TAG_SYMBOL);
    write_varint(&mut out, name.len() as u64);
    out.extend_from_slice(name);
    out.push(TAG_SMI);
    write_varint(&mut out, zigzag(id));
    out
}

/// The transcript-forwarding control message (workers M2): the MOP encoding
/// of `{#workerTranscript. id. text}` — a worker's `Transcript show:` (and
/// its error traces, which also write through `vm.out`) delivered to the
/// primary through the ordinary inbox and shown on ITS transcript.
pub(crate) fn encode_worker_transcript(id: i64, text: &str) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.push(TAG_ARRAY);
    write_varint(&mut out, 3);
    let name = b"workerTranscript";
    out.push(TAG_SYMBOL);
    write_varint(&mut out, name.len() as u64);
    out.extend_from_slice(name);
    out.push(TAG_SMI);
    write_varint(&mut out, zigzag(id));
    out.push(TAG_STRING);
    write_varint(&mut out, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
    out
}

#[allow(clippy::too_many_lines)] // one exhaustive dispatch, mirrored by `up`
fn pk(
    vm: &VmState,
    o: Oop,
    out: &mut Vec<u8>,
    seen: &mut HashMap<u64, u32>,
    depth: usize,
    max_bytes: usize,
    max_objects: usize,
) -> Result<(), PickleErr> {
    if depth >= MAX_DEPTH {
        return Err(PickleErr::TooDeep);
    }
    if out.len() > max_bytes {
        return Err(PickleErr::TooBig);
    }
    let u = &vm.universe;

    // Immediates + singletons (identity, no table entry — they're values).
    if o.raw() == u.nil_obj.raw() {
        out.push(TAG_NIL);
        return Ok(());
    }
    if o.raw() == u.false_obj.raw() {
        out.push(TAG_FALSE);
        return Ok(());
    }
    if o.raw() == u.true_obj.raw() {
        out.push(TAG_TRUE);
        return Ok(());
    }
    if let Some(smi) = SmallInt::try_from(o) {
        out.push(TAG_SMI);
        write_varint(out, zigzag(smi.value()));
        return Ok(());
    }
    let Some(m) = MemOop::try_from(o) else {
        return Err(PickleErr::Unpicklable("not a pickleable oop kind"));
    };
    let k = m.klass();
    let kraw = k.oop().raw();

    // Boxed values (no identity tracking — a Double is a value).
    if kraw == u.double_klass.oop().raw() {
        let d = DoubleOop::try_from(o).expect("double_klass oop wraps as DoubleOop");
        out.push(TAG_DOUBLE);
        out.extend_from_slice(&d.value().to_bits().to_le_bytes());
        return Ok(());
    }
    if kraw == u.character_klass.oop().raw() {
        let code = SmallInt::try_from(m.body_oop(0))
            .ok_or(PickleErr::Unpicklable("Character with a non-smi code"))?;
        out.push(TAG_CHAR);
        write_varint(out, code.value() as u64);
        return Ok(());
    }

    // Heap objects with identity: table them (first-encounter order), so a
    // revisit — sharing or a cycle — becomes a BackRef.
    if let Some(&idx) = seen.get(&o.raw()) {
        out.push(TAG_BACKREF);
        write_varint(out, u64::from(idx));
        return Ok(());
    }
    if seen.len() >= max_objects {
        return Err(PickleErr::TooManyObjects);
    }
    seen.insert(o.raw(), seen.len() as u32);

    if kraw == u.symbol_klass.oop().raw() {
        let bytes = read_bytes_out(o);
        out.push(TAG_SYMBOL);
        write_varint(out, bytes.len() as u64);
        out.extend_from_slice(&bytes);
        return Ok(());
    }
    if kraw == u.string_klass.oop().raw() {
        let bytes = read_bytes_out(o);
        out.push(TAG_STRING);
        write_varint(out, bytes.len() as u64);
        out.extend_from_slice(&bytes);
        return Ok(());
    }
    if kraw == u.bytearray_klass.oop().raw() {
        let bytes = read_bytes_out(o);
        out.push(TAG_BYTE_ARRAY);
        write_varint(out, bytes.len() as u64);
        out.extend_from_slice(&bytes);
        return Ok(());
    }
    if kraw == u.float_array_klass.oop().raw() {
        let bytes = read_bytes_out(o);
        if !bytes.len().is_multiple_of(8) {
            return Err(PickleErr::Unpicklable("ragged FloatArray"));
        }
        out.push(TAG_FLOAT_ARRAY);
        write_varint(out, (bytes.len() / 8) as u64);
        out.extend_from_slice(&bytes);
        return Ok(());
    }
    if kraw == u.large_pos_int_klass.oop().raw() || kraw == u.large_neg_int_klass.oop().raw() {
        let bytes = read_bytes_out(o);
        out.push(TAG_LARGE_INT);
        out.push(u8::from(kraw == u.large_neg_int_klass.oop().raw()));
        write_varint(out, bytes.len() as u64);
        out.extend_from_slice(&bytes);
        return Ok(());
    }
    if kraw == u.array_klass.oop().raw() {
        let arr = ArrayOop::try_from(o).expect("array_klass oop wraps as ArrayOop");
        out.push(TAG_ARRAY);
        write_varint(out, arr.len() as u64);
        for i in 0..arr.len() {
            pk(vm, arr.at(i), out, seen, depth + 1, max_bytes, max_objects)?;
        }
        return Ok(());
    }

    // The general case: an ordinary named-class object, encoded positionally.
    // Refuse everything whose meaning is tied to THIS vm (§4.1).
    if kraw == u.alien_klass.oop().raw() {
        return Err(PickleErr::Unpicklable("Alien (raw pointer)"));
    }
    let subkind = match k.format() {
        Format::Slots => SUBKIND_SLOTS,
        Format::IndexableOops => SUBKIND_OOPS,
        Format::IndexableBytes => SUBKIND_BYTES,
        Format::Method => return Err(PickleErr::Unpicklable("CompiledMethod")),
        Format::Klass => return Err(PickleErr::Unpicklable("a class")),
        Format::Closure => return Err(PickleErr::Unpicklable("BlockClosure")),
        Format::Context => return Err(PickleErr::Unpicklable("Context")),
        Format::Process => return Err(PickleErr::Unpicklable("Process")),
        Format::Double => unreachable!("double_klass handled above"),
    };
    let Some(name) = SymbolOop::try_from(k.name()) else {
        return Err(PickleErr::Unpicklable("instance of an anonymous class"));
    };
    let name_bytes = read_bytes_out(name.oop());
    let nslots = k.non_indexable_size() - HEADER_WORDS;
    out.push(TAG_OBJECT);
    write_varint(out, name_bytes.len() as u64);
    out.extend_from_slice(&name_bytes);
    write_varint(out, nslots as u64);
    out.push(subkind);
    match subkind {
        SUBKIND_SLOTS => {}
        SUBKIND_OOPS => {
            let arr = ArrayOop::try_from(o).expect("IndexableOops wraps as ArrayOop");
            write_varint(out, arr.len() as u64);
        }
        _ => {
            let b = ByteArrayOop::try_from(o).expect("IndexableBytes wraps as ByteArrayOop");
            write_varint(out, b.len() as u64);
        }
    }
    for i in 0..nslots {
        pk(
            vm,
            m.body_oop(i),
            out,
            seen,
            depth + 1,
            max_bytes,
            max_objects,
        )?;
    }
    match subkind {
        SUBKIND_OOPS => {
            let arr = ArrayOop::try_from(o).expect("checked above");
            for i in 0..arr.len() {
                pk(vm, arr.at(i), out, seen, depth + 1, max_bytes, max_objects)?;
            }
        }
        SUBKIND_BYTES => {
            let bytes = read_bytes_out(o);
            out.extend_from_slice(&bytes);
        }
        _ => {}
    }
    if out.len() > max_bytes {
        return Err(PickleErr::TooBig);
    }
    Ok(())
}

// ── unpickle ────────────────────────────────────────────────────────────────

/// Rebuild an object graph in THIS vm's heap. Allocates freely; the whole
/// working set is rooted in a [`HandleScope`], so a moving GC mid-build is
/// safe (the gc-stress test drives exactly that).
pub fn unpickle(vm: &mut VmState, bytes: &[u8]) -> Result<Oop, UnpickleErr> {
    if bytes.len() < MAGIC.len() || bytes[..MAGIC.len()] != MAGIC {
        return Err(UnpickleErr::BadMagic);
    }
    let scope = HandleScope::enter(vm);
    let mut table: Vec<Handle<Oop>> = Vec::new();
    let mut cur = Cursor {
        b: bytes,
        i: MAGIC.len(),
    };
    up(vm, &mut cur, &scope, &mut table, 0)
}

#[allow(clippy::too_many_lines)] // one exhaustive dispatch, mirrored by `pk`
fn up(
    vm: &mut VmState,
    cur: &mut Cursor,
    scope: &HandleScope,
    table: &mut Vec<Handle<Oop>>,
    depth: usize,
) -> Result<Oop, UnpickleErr> {
    if depth >= MAX_DEPTH {
        return Err(UnpickleErr::TooDeep);
    }
    if table.len() >= MAX_OBJECTS {
        return Err(UnpickleErr::TooManyObjects);
    }
    let tag = cur.u8()?;
    match tag {
        TAG_NIL => Ok(vm.universe.nil_obj),
        TAG_FALSE => Ok(vm.universe.false_obj),
        TAG_TRUE => Ok(vm.universe.true_obj),
        TAG_SMI => {
            let v = unzigzag(cur.varint()?);
            if !(SMI_MIN..=SMI_MAX).contains(&v) {
                return Err(UnpickleErr::Malformed("SmallInteger out of range"));
            }
            Ok(SmallInt::new(v).oop())
        }
        TAG_DOUBLE => {
            let raw = cur.exact(8)?;
            let bits = u64::from_le_bytes(raw.try_into().expect("exact(8) is 8 bytes"));
            Ok(alloc::alloc_double(vm, f64::from_bits(bits)).oop())
        }
        TAG_CHAR => {
            let code = cur.varint()?;
            if code > SMI_MAX as u64 {
                return Err(UnpickleErr::Malformed("Character code out of range"));
            }
            let obj = {
                let k = vm.universe.character_klass;
                alloc::alloc_slots(vm, k)
            };
            obj.set_body_oop(0, SmallInt::new(code as i64).oop());
            Ok(obj.oop())
        }
        TAG_SYMBOL => {
            let n = cur.len_capped(1)?;
            let bytes = cur.exact(n)?.to_vec();
            let sym = vm.universe.intern(&bytes);
            table.push(scope.handle(vm, sym.oop()));
            Ok(sym.oop())
        }
        TAG_STRING | TAG_BYTE_ARRAY => {
            let n = cur.len_capped(1)?;
            let bytes = cur.exact(n)?.to_vec();
            let klass = if tag == TAG_STRING {
                vm.universe.string_klass
            } else {
                vm.universe.bytearray_klass
            };
            let b = alloc::alloc_indexable_bytes(vm, klass, n);
            for (i, byte) in bytes.iter().enumerate() {
                b.byte_at_put(i, *byte);
            }
            table.push(scope.handle(vm, b.oop()));
            Ok(b.oop())
        }
        TAG_FLOAT_ARRAY => {
            let lanes = cur.len_capped(8)?;
            let bytes = cur.exact(lanes * 8)?.to_vec();
            let b = {
                let k = vm.universe.float_array_klass;
                alloc::alloc_indexable_bytes(vm, k, lanes * 8)
            };
            for (i, byte) in bytes.iter().enumerate() {
                b.byte_at_put(i, *byte);
            }
            table.push(scope.handle(vm, b.oop()));
            Ok(b.oop())
        }
        TAG_LARGE_INT => {
            let neg = cur.u8()?;
            if neg > 1 {
                return Err(UnpickleErr::Malformed("LargeInteger sign byte"));
            }
            let n = cur.len_capped(1)?;
            let bytes = cur.exact(n)?.to_vec();
            let klass = if neg == 1 {
                vm.universe.large_neg_int_klass
            } else {
                vm.universe.large_pos_int_klass
            };
            let b = alloc::alloc_indexable_bytes(vm, klass, n);
            for (i, byte) in bytes.iter().enumerate() {
                b.byte_at_put(i, *byte);
            }
            table.push(scope.handle(vm, b.oop()));
            Ok(b.oop())
        }
        TAG_ARRAY => {
            let n = cur.len_capped(1)?;
            // Spine first: allocate (nil-filled), root, THEN decode children —
            // a BackRef into this array mid-decode must find a live target.
            let arr = {
                let k = vm.universe.array_klass;
                alloc::alloc_indexable_oops(vm, k, n)
            };
            let h = scope.handle(vm, arr.oop());
            table.push(h);
            for i in 0..n {
                let child = up(vm, cur, scope, table, depth + 1)?;
                // Re-fetch through the handle: the child's decode may have
                // moved this array (scavenge/full GC).
                let a = ArrayOop::try_from(h.get(vm)).expect("handle preserves array-ness");
                a.at_put(i, child);
            }
            Ok(h.get(vm))
        }
        TAG_BACKREF => {
            let idx = cur.varint()? as usize;
            let h = table.get(idx).ok_or(UnpickleErr::BadBackRef)?;
            Ok(h.get(vm))
        }
        TAG_OBJECT => {
            let name_len = cur.len_capped(1)?;
            let name = cur.exact(name_len)?.to_vec();
            let nslots = cur.varint()? as usize;
            let subkind = cur.u8()?;
            let idx_n = match subkind {
                SUBKIND_SLOTS => 0,
                SUBKIND_OOPS | SUBKIND_BYTES => cur.len_capped(1)?,
                _ => return Err(UnpickleErr::Malformed("bad object subkind")),
            };
            // Resolve the class by name in THIS world (the receiving side's
            // definition wins), then shape-check before allocating.
            let sym = vm.universe.intern(&name);
            let name_str = || String::from_utf8_lossy(&name).into_owned();
            let Some(assoc) = global_lookup(vm, sym) else {
                return Err(UnpickleErr::UnknownClass(name_str()));
            };
            let assoc_m =
                MemOop::try_from(assoc).ok_or_else(|| UnpickleErr::NotAClass(name_str()))?;
            let Some(kls) = KlassOop::try_from(assoc_m.body_oop(1)) else {
                return Err(UnpickleErr::NotAClass(name_str()));
            };
            if kls.non_indexable_size() - HEADER_WORDS != nslots {
                return Err(UnpickleErr::ShapeMismatch(name_str()));
            }
            let format_ok = matches!(
                (subkind, kls.format()),
                (SUBKIND_SLOTS, Format::Slots)
                    | (SUBKIND_OOPS, Format::IndexableOops)
                    | (SUBKIND_BYTES, Format::IndexableBytes)
            );
            if !format_ok {
                return Err(UnpickleErr::ShapeMismatch(name_str()));
            }
            if nslots
                .checked_add(idx_n)
                .is_none_or(|total| total > cur.remaining() + idx_n)
            {
                return Err(UnpickleErr::Truncated);
            }
            // Spine first (as TAG_ARRAY): allocate nil-filled, root, descend.
            let obj_h = match subkind {
                SUBKIND_SLOTS => {
                    let o = alloc::alloc_slots(vm, kls);
                    scope.handle(vm, o.oop())
                }
                SUBKIND_OOPS => {
                    let o = alloc::alloc_indexable_oops(vm, kls, idx_n);
                    scope.handle(vm, o.oop())
                }
                _ => {
                    let o = alloc::alloc_indexable_bytes(vm, kls, idx_n);
                    scope.handle(vm, o.oop())
                }
            };
            table.push(obj_h);
            for i in 0..nslots {
                let child = up(vm, cur, scope, table, depth + 1)?;
                let m = MemOop::try_from(obj_h.get(vm)).expect("handle preserves mem-ness");
                m.set_body_oop(i, child);
            }
            match subkind {
                SUBKIND_OOPS => {
                    for i in 0..idx_n {
                        let child = up(vm, cur, scope, table, depth + 1)?;
                        let a = ArrayOop::try_from(obj_h.get(vm))
                            .expect("handle preserves indexable-oops-ness");
                        a.at_put(i, child);
                    }
                }
                SUBKIND_BYTES => {
                    let bytes = cur.exact(idx_n)?.to_vec();
                    let b = ByteArrayOop::try_from(obj_h.get(vm))
                        .expect("handle preserves indexable-bytes-ness");
                    for (i, byte) in bytes.iter().enumerate() {
                        b.byte_at_put(i, *byte);
                    }
                }
                _ => {}
            }
            Ok(obj_h.get(vm))
        }
        other => Err(UnpickleErr::BadTag(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// The HandleScope proof-vm: every allocation triggers a collection.
    fn stress_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: true,
            gc_stress_full_period: None,
            eden_kb: None,
            jit: crate::runtime::JitMode::Off,
        })
    }

    fn smi(v: i64) -> Oop {
        SmallInt::new(v).oop()
    }

    fn round_trip(vm: &mut VmState, o: Oop) -> Oop {
        let bytes = pickle(vm, o).expect("pickle must succeed");
        unpickle(vm, &bytes).expect("unpickle must succeed")
    }

    fn new_string(vm: &mut VmState, s: &str) -> Oop {
        let k = vm.universe.string_klass;
        let b = alloc::alloc_indexable_bytes(vm, k, s.len());
        for (i, byte) in s.bytes().enumerate() {
            b.byte_at_put(i, byte);
        }
        b.oop()
    }

    fn string_content(o: Oop) -> Vec<u8> {
        read_bytes_out(o)
    }

    // ── per-type round-trips ────────────────────────────────────────────

    #[test]
    fn immediates_and_values_round_trip() {
        let mut vm = test_vm();
        let nil = vm.universe.nil_obj;
        let t = vm.universe.true_obj;
        let f = vm.universe.false_obj;
        assert_eq!(round_trip(&mut vm, nil).raw(), nil.raw());
        assert_eq!(round_trip(&mut vm, t).raw(), t.raw());
        assert_eq!(round_trip(&mut vm, f).raw(), f.raw());
        for v in [0i64, 1, -1, 42, -12345, SMI_MAX, SMI_MIN] {
            let out = round_trip(&mut vm, smi(v));
            assert_eq!(SmallInt::try_from(out).unwrap().value(), v, "smi {v}");
        }
        for d in [0.0f64, 1.5, -0.0, f64::MAX, f64::NAN, f64::INFINITY] {
            let src = alloc::alloc_double(&mut vm, d).oop();
            let out = round_trip(&mut vm, src);
            let got = DoubleOop::try_from(out).expect("a Double comes back");
            // Bit-compare so NaN and -0.0 are exact.
            assert_eq!(got.value().to_bits(), d.to_bits(), "double {d}");
        }
        // Character: a 1-slot object holding the code point.
        let ch = {
            let k = vm.universe.character_klass;
            alloc::alloc_slots(&mut vm, k)
        };
        ch.set_body_oop(0, smi(65));
        let out = round_trip(&mut vm, ch.oop());
        let m = MemOop::try_from(out).unwrap();
        assert_eq!(
            m.klass().oop().raw(),
            vm.universe.character_klass.oop().raw()
        );
        assert_eq!(SmallInt::try_from(m.body_oop(0)).unwrap().value(), 65);
    }

    #[test]
    fn strings_symbols_bytes_round_trip() {
        let mut vm = test_vm();
        let s = new_string(&mut vm, "hello MOP");
        let out = round_trip(&mut vm, s);
        assert_eq!(string_content(out), b"hello MOP");
        assert_ne!(out.raw(), s.raw(), "a copy, not the same object");

        let empty = new_string(&mut vm, "");
        assert_eq!(string_content(round_trip(&mut vm, empty)), b"");

        let ba = {
            let k = vm.universe.bytearray_klass;
            alloc::alloc_indexable_bytes(&mut vm, k, 4)
        };
        for (i, b) in [1u8, 2, 3, 255].iter().enumerate() {
            ba.byte_at_put(i, *b);
        }
        let out = round_trip(&mut vm, ba.oop());
        assert_eq!(read_bytes_out(out), vec![1, 2, 3, 255]);

        // Symbols re-intern: round-tripping twice yields the SAME oop.
        let sym = vm.universe.intern(b"mopSelector").oop();
        let a = round_trip(&mut vm, sym);
        let b = round_trip(&mut vm, sym);
        assert_eq!(a.raw(), sym.raw(), "interning heals symbol identity");
        assert_eq!(a.raw(), b.raw());
    }

    #[test]
    fn float_array_round_trip() {
        let mut vm = test_vm();
        let lanes = [1.5f64, -2.25, 0.0, f64::MAX];
        let k = vm.universe.float_array_klass;
        let fa = alloc::alloc_indexable_bytes(&mut vm, k, lanes.len() * 8);
        for (i, v) in lanes.iter().enumerate() {
            for (j, byte) in v.to_bits().to_le_bytes().iter().enumerate() {
                fa.byte_at_put(i * 8 + j, *byte);
            }
        }
        let out = round_trip(&mut vm, fa.oop());
        let m = MemOop::try_from(out).unwrap();
        assert_eq!(
            m.klass().oop().raw(),
            vm.universe.float_array_klass.oop().raw()
        );
        assert_eq!(read_bytes_out(out), read_bytes_out(fa.oop()));
    }

    #[test]
    fn large_integer_round_trip() {
        let mut vm = test_vm();
        for (neg, klass) in [
            (false, vm.universe.large_pos_int_klass),
            (true, vm.universe.large_neg_int_klass),
        ] {
            let li = alloc::alloc_indexable_bytes(&mut vm, klass, 3);
            for (i, b) in [0x12u8, 0x34, 0x56].iter().enumerate() {
                li.byte_at_put(i, *b);
            }
            let out = round_trip(&mut vm, li.oop());
            let m = MemOop::try_from(out).unwrap();
            assert_eq!(m.klass().oop().raw(), klass.oop().raw(), "neg={neg}");
            assert_eq!(read_bytes_out(out), vec![0x12, 0x34, 0x56]);
        }
    }

    #[test]
    fn arrays_nested_round_trip() {
        let mut vm = test_vm();
        let scope = HandleScope::enter(&mut vm);
        let inner = {
            let k = vm.universe.array_klass;
            alloc::alloc_indexable_oops(&mut vm, k, 2)
        };
        let inner_h = scope.handle(&mut vm, inner.oop());
        let d = alloc::alloc_double(&mut vm, 4.0);
        ArrayOop::try_from(inner_h.get(&vm))
            .unwrap()
            .at_put(0, d.oop());
        // slot 1 stays nil
        let s = new_string(&mut vm, "two");
        let s_h = scope.handle(&mut vm, s);
        let sym = vm.universe.intern(b"three").oop();
        let sym_h = scope.handle(&mut vm, sym);
        let outer = {
            let k = vm.universe.array_klass;
            alloc::alloc_indexable_oops(&mut vm, k, 4)
        };
        let o = ArrayOop::try_from(outer.oop()).unwrap();
        o.at_put(0, smi(1));
        o.at_put(1, s_h.get(&vm));
        o.at_put(2, sym_h.get(&vm));
        o.at_put(3, inner_h.get(&vm));

        let out = round_trip(&mut vm, outer.oop());
        let arr = ArrayOop::try_from(out).expect("an Array comes back");
        assert_eq!(arr.len(), 4);
        assert_eq!(SmallInt::try_from(arr.at(0)).unwrap().value(), 1);
        assert_eq!(string_content(arr.at(1)), b"two");
        assert_eq!(arr.at(2).raw(), vm.universe.intern(b"three").oop().raw());
        let inner_out = ArrayOop::try_from(arr.at(3)).expect("nested Array");
        assert_eq!(DoubleOop::try_from(inner_out.at(0)).unwrap().value(), 4.0);
        assert_eq!(inner_out.at(1).raw(), vm.universe.nil_obj.raw());
    }

    // ── sharing + cycles ────────────────────────────────────────────────

    #[test]
    fn shared_substructure_is_preserved() {
        let mut vm = test_vm();
        let scope = HandleScope::enter(&mut vm);
        let a = {
            let k = vm.universe.array_klass;
            alloc::alloc_indexable_oops(&mut vm, k, 1)
        };
        let a_h = scope.handle(&mut vm, a.oop());
        ArrayOop::try_from(a_h.get(&vm)).unwrap().at_put(0, smi(42));
        let outer = {
            let k = vm.universe.array_klass;
            alloc::alloc_indexable_oops(&mut vm, k, 2)
        };
        let o = ArrayOop::try_from(outer.oop()).unwrap();
        o.at_put(0, a_h.get(&vm));
        o.at_put(1, a_h.get(&vm));

        let out = ArrayOop::try_from(round_trip(&mut vm, outer.oop())).unwrap();
        assert_eq!(
            out.at(0).raw(),
            out.at(1).raw(),
            "{{a. a}} must rebuild as TWO references to ONE object"
        );
        assert_ne!(out.at(0).raw(), a_h.get(&vm).raw(), "and it is a copy");
    }

    #[test]
    fn cycles_round_trip() {
        let mut vm = test_vm();
        let a = {
            let k = vm.universe.array_klass;
            alloc::alloc_indexable_oops(&mut vm, k, 1)
        };
        ArrayOop::try_from(a.oop()).unwrap().at_put(0, a.oop());

        let out = round_trip(&mut vm, a.oop());
        let arr = ArrayOop::try_from(out).unwrap();
        assert_eq!(
            arr.at(0).raw(),
            out.raw(),
            "a self-cycle must come back as a self-cycle"
        );
    }

    // ── general (tag 11) objects ────────────────────────────────────────

    #[test]
    fn general_object_round_trips_by_class_name() {
        // Association is genesis-declared as a global, 2 named slots.
        let mut vm = test_vm();
        let scope = HandleScope::enter(&mut vm);
        let assoc = {
            let k = vm.universe.association_klass;
            alloc::alloc_slots(&mut vm, k)
        };
        let assoc_h = scope.handle(&mut vm, assoc.oop());
        let key = vm.universe.intern(b"theKey").oop();
        let m = MemOop::try_from(assoc_h.get(&vm)).unwrap();
        m.set_body_oop(0, key);
        m.set_body_oop(1, smi(7));

        let src = assoc_h.get(&vm);
        let out = round_trip(&mut vm, src);
        let om = MemOop::try_from(out).unwrap();
        assert_eq!(
            om.klass().oop().raw(),
            vm.universe.association_klass.oop().raw(),
            "rebuilt under the receiving world's class"
        );
        assert_eq!(
            om.body_oop(0).raw(),
            vm.universe.intern(b"theKey").oop().raw()
        );
        assert_eq!(SmallInt::try_from(om.body_oop(1)).unwrap().value(), 7);
    }

    #[test]
    fn unknown_class_fails_cleanly() {
        let mut vm = test_vm();
        let mut bytes = MAGIC.to_vec();
        bytes.push(TAG_OBJECT);
        let name = b"NoSuchClassXyz";
        write_varint(&mut bytes, name.len() as u64);
        bytes.extend_from_slice(name);
        write_varint(&mut bytes, 0); // nslots
        bytes.push(SUBKIND_SLOTS);
        match unpickle(&mut vm, &bytes) {
            Err(UnpickleErr::UnknownClass(n)) => assert_eq!(n, "NoSuchClassXyz"),
            other => panic!("expected UnknownClass, got {other:?}"),
        }
    }

    #[test]
    fn shape_mismatch_fails_cleanly() {
        let mut vm = test_vm();
        let mut bytes = MAGIC.to_vec();
        bytes.push(TAG_OBJECT);
        let name = b"Association";
        write_varint(&mut bytes, name.len() as u64);
        bytes.extend_from_slice(name);
        write_varint(&mut bytes, 5); // Association really has 2 named slots
        bytes.push(SUBKIND_SLOTS);
        for _ in 0..5 {
            bytes.push(TAG_NIL);
        }
        match unpickle(&mut vm, &bytes) {
            Err(UnpickleErr::ShapeMismatch(n)) => assert_eq!(n, "Association"),
            other => panic!("expected ShapeMismatch, got {other:?}"),
        }
    }

    // ── refusals ────────────────────────────────────────────────────────

    #[test]
    fn vm_internal_kinds_are_refused() {
        let mut vm = test_vm();
        let closure = alloc::alloc_closure(&mut vm, 0).oop();
        let context = alloc::alloc_context(&mut vm, 0).oop();
        let method = alloc::alloc_method(&mut vm, 8).oop();
        let a_class = vm.universe.array_klass.oop();
        let alien = {
            let k = vm.universe.alien_klass;
            alloc::alloc_indexable_bytes(&mut vm, k, 16)
        }
        .oop();
        for (o, what) in [
            (closure, "closure"),
            (context, "context"),
            (method, "method"),
            (a_class, "class"),
            (alien, "alien"),
        ] {
            assert!(
                matches!(pickle(&vm, o), Err(PickleErr::Unpicklable(_))),
                "{what} must refuse to pickle"
            );
        }
    }

    // ── guards: caps, depth, truncation, corruption ─────────────────────

    #[test]
    fn caps_are_enforced() {
        let mut vm = test_vm();
        // Many distinct heap objects -> object cap.
        let arr = {
            let k = vm.universe.array_klass;
            alloc::alloc_indexable_oops(&mut vm, k, 20)
        };
        {
            let scope = HandleScope::enter(&mut vm);
            let h = scope.handle(&mut vm, arr.oop());
            for i in 0..20 {
                let s = new_string(&mut vm, "x");
                ArrayOop::try_from(h.get(&vm)).unwrap().at_put(i, s);
            }
            assert_eq!(
                pickle_with_limits(&vm, h.get(&vm), MAX_BYTES, 10),
                Err(PickleErr::TooManyObjects)
            );
            // Byte cap.
            assert_eq!(
                pickle_with_limits(&vm, h.get(&vm), 16, MAX_OBJECTS),
                Err(PickleErr::TooBig)
            );
        }
    }

    #[test]
    fn too_deep_fails() {
        let mut vm = test_vm();
        let scope = HandleScope::enter(&mut vm);
        let mut prev = {
            let a = {
                let k = vm.universe.array_klass;
                alloc::alloc_indexable_oops(&mut vm, k, 1)
            };
            scope.handle(&mut vm, a.oop())
        };
        for _ in 0..MAX_DEPTH {
            let a = {
                let k = vm.universe.array_klass;
                alloc::alloc_indexable_oops(&mut vm, k, 1)
            };
            let h = scope.handle(&mut vm, a.oop());
            ArrayOop::try_from(h.get(&vm))
                .unwrap()
                .at_put(0, prev.get(&vm));
            prev = h;
        }
        assert_eq!(pickle(&vm, prev.get(&vm)), Err(PickleErr::TooDeep));
    }

    /// Build one graph exercising every tag, for the fuzz + stress tests.
    fn rich_graph(vm: &mut VmState) -> Oop {
        let scope = HandleScope::enter(vm);
        let s = new_string(vm, "rich");
        let s_h = scope.handle(vm, s);
        let sym = vm.universe.intern(b"richSym").oop();
        let sym_h = scope.handle(vm, sym);
        let d = alloc::alloc_double(vm, 3.25).oop();
        let d_h = scope.handle(vm, d);
        let ba = {
            let k = vm.universe.bytearray_klass;
            alloc::alloc_indexable_bytes(vm, k, 3)
        };
        ba.byte_at_put(0, 9);
        let ba_h = scope.handle(vm, ba.oop());
        let assoc = {
            let k = vm.universe.association_klass;
            alloc::alloc_slots(vm, k)
        };
        let assoc_h = scope.handle(vm, assoc.oop());
        {
            let m = MemOop::try_from(assoc_h.get(vm)).unwrap();
            m.set_body_oop(0, sym_h.get(vm));
            m.set_body_oop(1, smi(11));
        }
        let arr = {
            let k = vm.universe.array_klass;
            alloc::alloc_indexable_oops(vm, k, 8)
        };
        let nil = vm.universe.nil_obj;
        let tru = vm.universe.true_obj;
        let a = ArrayOop::try_from(arr.oop()).unwrap();
        a.at_put(0, smi(-5));
        a.at_put(1, s_h.get(vm));
        a.at_put(2, sym_h.get(vm));
        a.at_put(3, d_h.get(vm));
        a.at_put(4, ba_h.get(vm));
        a.at_put(5, assoc_h.get(vm));
        a.at_put(6, nil);
        a.at_put(7, tru);
        // Root `arr` BEFORE allocating `outer`: under gc_stress that next
        // allocation collects, and an unrooted arr would move out from under
        // the raw oop we were about to store (this helper originally had
        // exactly that bug — the stress test caught it).
        let arr_h = scope.handle(vm, arr.oop());
        // A shared reference + a cycle, to keep the fuzz honest.
        let outer = {
            let k = vm.universe.array_klass;
            alloc::alloc_indexable_oops(vm, k, 3)
        };
        let o = ArrayOop::try_from(outer.oop()).unwrap();
        o.at_put(0, arr_h.get(vm));
        o.at_put(1, arr_h.get(vm));
        o.at_put(2, outer.oop());
        outer.oop()
    }

    #[test]
    fn truncation_always_errs_never_panics() {
        let mut vm = test_vm();
        let root = rich_graph(&mut vm);
        let bytes = pickle(&vm, root).expect("pickle rich graph");
        for i in 0..bytes.len() {
            assert!(
                unpickle(&mut vm, &bytes[..i]).is_err(),
                "prefix of {i} bytes must be an error"
            );
        }
    }

    #[test]
    fn corrupt_bytes_never_panic() {
        let mut vm = test_vm();
        let root = rich_graph(&mut vm);
        let bytes = pickle(&vm, root).expect("pickle rich graph");
        for i in 0..bytes.len() {
            for flip in [0x01u8, 0x80, 0xff] {
                let mut corrupt = bytes.clone();
                corrupt[i] ^= flip;
                // Ok or Err both fine — the contract is "never panic".
                let _ = unpickle(&mut vm, &corrupt);
            }
        }
    }

    // ── the HandleScope proof: GC on every allocation mid-unpickle ──────

    #[test]
    fn unpickle_survives_gc_stress() {
        let mut vm = stress_vm();
        let root = rich_graph(&mut vm);
        let bytes = pickle(&vm, root).expect("pickle rich graph");
        // Every allocation in this vm scavenges: the spine handles must keep
        // the half-built graph alive and rewritten across many moves.
        let out = unpickle(&mut vm, &bytes).expect("unpickle under gc stress");
        let o = ArrayOop::try_from(out).expect("outer Array");
        assert_eq!(o.at(0).raw(), o.at(1).raw(), "sharing preserved");
        assert_eq!(o.at(2).raw(), out.raw(), "cycle preserved");
        let arr = ArrayOop::try_from(o.at(0)).unwrap();
        assert_eq!(SmallInt::try_from(arr.at(0)).unwrap().value(), -5);
        assert_eq!(string_content(arr.at(1)), b"rich");
        assert_eq!(arr.at(2).raw(), vm.universe.intern(b"richSym").oop().raw());
        assert_eq!(DoubleOop::try_from(arr.at(3)).unwrap().value(), 3.25);
    }

    // ── random-graph differential ───────────────────────────────────────

    /// Structural equality with cycle tolerance (a pair-visited set), used
    /// by the randomized differential sweep. Never allocates.
    fn structurally_eq(
        vm: &VmState,
        a: Oop,
        b: Oop,
        visited: &mut std::collections::HashSet<(u64, u64)>,
    ) -> bool {
        if a.raw() == b.raw() {
            return true; // nil/true/false/smis/interned symbols
        }
        if let (Some(x), Some(y)) = (SmallInt::try_from(a), SmallInt::try_from(b)) {
            return x.value() == y.value();
        }
        let (Some(ma), Some(mb)) = (MemOop::try_from(a), MemOop::try_from(b)) else {
            return false;
        };
        if ma.klass().oop().raw() != mb.klass().oop().raw() {
            return false;
        }
        if !visited.insert((a.raw(), b.raw())) {
            return true; // already comparing this pair (cycle)
        }
        let k = ma.klass();
        let kraw = k.oop().raw();
        if kraw == vm.universe.double_klass.oop().raw() {
            return DoubleOop::try_from(a).unwrap().value().to_bits()
                == DoubleOop::try_from(b).unwrap().value().to_bits();
        }
        match k.format() {
            Format::IndexableBytes => read_bytes_out(a) == read_bytes_out(b),
            Format::IndexableOops => {
                let (xa, xb) = (
                    ArrayOop::try_from(a).unwrap(),
                    ArrayOop::try_from(b).unwrap(),
                );
                if xa.len() != xb.len() {
                    return false;
                }
                (0..xa.len()).all(|i| structurally_eq(vm, xa.at(i), xb.at(i), visited))
            }
            Format::Slots => {
                let n = k.non_indexable_size() - HEADER_WORDS;
                (0..n).all(|i| structurally_eq(vm, ma.body_oop(i), mb.body_oop(i), visited))
            }
            _ => false,
        }
    }

    /// A tiny deterministic LCG — no external randomness in tests.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, bound: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % bound
        }
    }

    fn random_graph(vm: &mut VmState, rng: &mut Lcg, depth: usize) -> Oop {
        match if depth >= 4 { rng.next(6) } else { rng.next(8) } {
            0 => smi(rng.next(1 << 40) as i64 - (1 << 39)),
            1 => vm.universe.nil_obj,
            2 => alloc::alloc_double(vm, rng.next(1000) as f64 / 8.0).oop(),
            3 => {
                let n = rng.next(6) as usize;
                let mut bytes = Vec::new();
                for _ in 0..n {
                    bytes.push(b'a' + rng.next(26) as u8);
                }
                let b = {
                    let k = vm.universe.string_klass;
                    alloc::alloc_indexable_bytes(vm, k, n)
                };
                for (i, byte) in bytes.iter().enumerate() {
                    b.byte_at_put(i, *byte);
                }
                b.oop()
            }
            4 => {
                let name = [b's', b'y', b'm', b'0' + rng.next(10) as u8];
                vm.universe.intern(&name).oop()
            }
            5 => vm.universe.true_obj,
            _ => {
                let n = rng.next(4) as usize;
                let scope = HandleScope::enter(vm);
                let arr = {
                    let k = vm.universe.array_klass;
                    alloc::alloc_indexable_oops(vm, k, n)
                };
                let h = scope.handle(vm, arr.oop());
                for i in 0..n {
                    let child = random_graph(vm, rng, depth + 1);
                    ArrayOop::try_from(h.get(vm)).unwrap().at_put(i, child);
                }
                h.get(vm)
            }
        }
    }

    #[test]
    fn random_graph_differential_sweep() {
        let mut vm = test_vm();
        let mut rng = Lcg(0x5eed_cafe_f00d);
        for round in 0..200 {
            let scope = HandleScope::enter(&mut vm);
            let original = random_graph(&mut vm, &mut rng, 0);
            let orig_h = scope.handle(&mut vm, original);
            let bytes = pickle(&vm, orig_h.get(&vm)).expect("random graph pickles");
            let rebuilt = unpickle(&mut vm, &bytes).expect("random graph unpickles");
            // Unpickling allocates, so re-fetch the (possibly moved) original.
            let mut visited = std::collections::HashSet::new();
            assert!(
                structurally_eq(&vm, orig_h.get(&vm), rebuilt, &mut visited),
                "round {round}: rebuilt graph differs structurally"
            );
        }
    }

    // ── the primitives (227/228) ────────────────────────────────────────

    #[test]
    fn prims_227_228_round_trip_and_fail_cleanly() {
        use crate::runtime::primitives::{prim_by_id, PrimResult};
        let mut vm = test_vm();
        let nil = vm.universe.nil_obj;
        let s = new_string(&mut vm, "via prim");
        let p = prim_by_id(227).expect("primPickle: registered").f;
        let u = prim_by_id(228).expect("primUnpickle: registered").f;

        let PrimResult::Ok(bytes_oop) = p(&mut vm, &[nil, s]) else {
            panic!("pickle prim must succeed on a String");
        };
        let PrimResult::Ok(back) = u(&mut vm, &[nil, bytes_oop]) else {
            panic!("unpickle prim must succeed on its own output");
        };
        assert_eq!(string_content(back), b"via prim");

        // Refusal surfaces as PrimResult::Fail (the world method's fallback).
        let closure = alloc::alloc_closure(&mut vm, 0).oop();
        assert!(matches!(p(&mut vm, &[nil, closure]), PrimResult::Fail));
        // Unpickle of garbage fails, never panics.
        let garbage = {
            let k = vm.universe.bytearray_klass;
            alloc::alloc_indexable_bytes(&mut vm, k, 3)
        };
        garbage.byte_at_put(0, 0xde);
        assert!(matches!(
            u(&mut vm, &[nil, garbage.oop()]),
            PrimResult::Fail
        ));
        // Unpickle requires a ByteArray receiver argument.
        assert!(matches!(u(&mut vm, &[nil, smi(5)]), PrimResult::Fail));
    }
}
