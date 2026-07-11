//! Backend-neutral code-emission interface — S9 structured rewrite
//! (`docs/sprints/sprint_s09_detail.md` D2.1; supersedes the S0-era
//! text-oriented sketch this file used to hold).
//!
//! The optimizing compiler emits through this trait *only*, in structured
//! operands — no text is ever formatted and re-parsed at the MACVM/backend
//! seam (the vendored encoder's own text front end, `crate::vendor::wfasm::
//! a64::parse`, exists purely so `corpus_replay` can replay the frozen
//! corpus; the compiler never calls it — P6). [`JasmAssembler`]
//! (`crate::compiler::jasm_assembler`) is the only implementor.

pub use crate::vendor::wfasm::a64::parse::{Addr, Arr, ElemSize, Mem, Operand, Reg, RegClass};

/// Why a code word / pool word needs runtime attention. Recorded per site.
/// `Hash` (beyond the `Eq` an enum this small would derive anyway) is for
/// `JasmAssembler`'s literal-pool dedup key, `(u64, Option<RelocKind>)`
/// (P10 — dedup by value AND kind together, never value alone).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RelocKind {
    /// 8-byte literal-pool word holding an Oop; GC updates it in place.
    Oop,
    /// As `Oop`, but the nmethod's customization-key klass: treated WEAKLY
    /// at full GC (SPEC §8.5; consumed in S12). Emitted from S11 prologues.
    KeyKlassOop,
    /// A patchable `bl` send site (compiled inline cache). S11.
    InlineCache,
    /// 8-byte pool word holding an absolute Rust/stub address (`ldr;blr`).
    RuntimeAddr,
    /// 8-byte pool word holding an address inside the code cache.
    InternalWord,
}

/// One relocation site within a [`CodeBlob`]. For pool kinds (`Oop`,
/// `KeyKlassOop`, `RuntimeAddr`, `InternalWord`) `offset` is the byte offset
/// of the 8-byte pool word within the blob; for `InlineCache` it is the
/// offset of the `bl` instruction word itself.
#[derive(Clone, Copy, Debug)]
pub struct Reloc {
    pub offset: u32,
    pub kind: RelocKind,
}

/// A position in the code buffer that can be bound and branched to —
/// intra-blob control flow only. Opaque index into the assembler's own
/// label table; `bind`/branch-emitting methods resolve it, never the
/// caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Label(pub u32);

/// A handle to an interned literal-pool constant, returned by
/// [`Assembler::literal_u64`] and consumed by [`Assembler::ldr_literal`] /
/// [`Assembler::call_far`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LiteralId(pub u32);

/// AArch64 condition codes, for [`Assembler::b_cond`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cond {
    Eq,
    Ne,
    Hs,
    Lo,
    Mi,
    Pl,
    Vs,
    Vc,
    Hi,
    Ls,
    Ge,
    Lt,
    Gt,
    Le,
}

/// Finished, position-independent code + metadata, ready for
/// `CodeCache::publish` (S9 step 6). Instructions first, then an 8-byte-
/// aligned literal pool (`literal_off` marks the boundary) — pool at the
/// end so hot code prefetches cleanly and a GC rewriting pool words never
/// touches instruction bytes (D3.3; `arm64.md` §4.1). All label/literal
/// fixups are resolved by [`Assembler::finish`]; `InlineCache` sites still
/// hold `bl .` self-branch placeholders for S11 to wire up.
pub struct CodeBlob {
    pub code: Vec<u8>,
    pub literal_off: u32,
    pub relocs: Vec<Reloc>,
    /// One human-readable line per emitted instruction. Populated in debug
    /// builds and under `cfg(test)`; empty in release (S10's disassembly
    /// goldens read this).
    pub listing: Vec<String>,
}

/// Backend-neutral emitter. A concrete implementor owns a growable code
/// buffer and knows the target ISA; the compiler only ever holds a
/// `&mut dyn Assembler`.
pub trait Assembler {
    /// Current write position (bytes from blob start). Always 4-aligned.
    fn offset(&self) -> u32;

    /// Encode one instruction via the vendored structured encoder. Panics
    /// on encode failure — a mnemonic/operand mistake is a compiler bug,
    /// never a guest-visible condition (CONVENTIONS §4).
    fn emit(&mut self, mnemonic: &str, ops: &[Operand]);

    /// Emit a raw pre-encoded word (`ldr`-literal, D4; `brk #imm` uncommon
    /// traps in S13 — the vendored encoder doesn't support either form).
    fn emit_u32(&mut self, word: u32);

    // ── labels: intra-blob control flow (branch fixups resolved in-house,
    //    NOT via encode()'s string-target Fixups — P6) ───────────────────
    fn new_label(&mut self) -> Label;
    /// Fix `l` at the current offset.
    fn bind(&mut self, l: Label);
    /// Unconditional branch (Branch26, ±128 MB).
    fn b(&mut self, l: Label);
    /// Conditional branch (Branch19, ±1 MiB).
    fn b_cond(&mut self, c: Cond, l: Label);
    /// Compare-and-branch-if-zero (Branch19).
    fn cbz(&mut self, r: Reg, l: Label);
    /// Compare-and-branch-if-nonzero (Branch19).
    fn cbnz(&mut self, r: Reg, l: Label);
    /// Test-bit-and-branch-if-zero (Branch14, ±32 KiB).
    fn tbz(&mut self, r: Reg, bit: u8, l: Label);
    /// Test-bit-and-branch-if-nonzero (Branch14, ±32 KiB).
    fn tbnz(&mut self, r: Reg, bit: u8, l: Label);

    // ── literal pool ────────────────────────────────────────────────────
    /// Intern an 8-byte pool constant, deduplicated by `(v, kind)` (P10:
    /// never by `v` alone — an address that is both a plain `RuntimeAddr`
    /// and separately an `Oop` must not share a word, or GC would rewrite
    /// the runtime address).
    fn literal_u64(&mut self, v: u64, kind: Option<RelocKind>) -> LiteralId;
    /// `ldr <dst,x-reg>, <pc-relative literal>` (raw-encoded — D4/D3.4, the
    /// vendored encoder's documented gap).
    fn ldr_literal(&mut self, dst: Reg, lit: LiteralId);

    // ── patchable call sites ────────────────────────────────────────────
    /// Emit `bl .` (self-branch placeholder) plus an `InlineCache` reloc.
    /// Returns the site's byte offset. `CodeCache::patch_branch26` (S9 step
    /// 7) does the actual patch; S11 wires the initial target.
    fn bl_patchable(&mut self, kind: RelocKind) -> u32;
    /// `ldr x16, <pool RuntimeAddr>; blr x16` — call an absolute address.
    /// Rust runtime functions live far outside a compiled blob's ±128 MB
    /// branch range in practice; `x16` is IP0 (`arm64.md` §3).
    fn call_far(&mut self, target: LiteralId);

    /// Resolve every pending label/literal fixup, pad to 8 bytes, append
    /// the literal pool, rebase pool relocs, and hand over the finished
    /// blob. The assembler is consumed (conceptually — see the
    /// implementor) afterward; calling `finish` again is a compiler bug.
    /// Panics on any unbound label.
    fn finish(&mut self) -> CodeBlob;
}

// ── Operand helpers — free functions so call sites stay short ───────────

pub fn x(n: u8) -> Operand {
    Operand::Reg(Reg {
        class: RegClass::X,
        num: n,
        is_sp: false,
    })
}
pub fn w(n: u8) -> Operand {
    Operand::Reg(Reg {
        class: RegClass::W,
        num: n,
        is_sp: false,
    })
}
/// A double-precision FP/SIMD register (`d0`..`d31`) — S20 FFI's own
/// trampolines (`codecache::ffi_stubs`) are the first user; every other
/// emitter in this codebase is GPR-only (Smalltalk values are oops/smis,
/// never native floats carried in the FP register file).
pub fn d(n: u8) -> Operand {
    Operand::Reg(Reg {
        class: RegClass::D,
        num: n,
        is_sp: false,
    })
}
/// The stack pointer, as an X-class operand (`sp`, not `xzr` — register 31
/// means different things in different syntactic positions; `is_sp`
/// disambiguates for the encoder's ALU/move paths. Memory-base position
/// register 31 is unconditionally SP regardless of this flag — the ISA has
/// no zero-register-as-base form — so [`mem`]/[`mem_pre`]/[`mem_post`] set
/// it from `base == 31` directly rather than requiring callers to route
/// through this helper.)
/// A 128-bit FP/SIMD register (`q0`..`q31`) — the whole vector; `d(n)` is its
/// low 64 bits. SIMD (`docs/SIMD.md`): `ldr q`/`str q` move a Float64x2's
/// 16-byte body.
pub fn q(n: u8) -> Operand {
    Operand::Reg(Reg {
        class: RegClass::Q,
        num: n,
        is_sp: false,
    })
}
/// A `.2d` vector operand (`v3.2d`) — two f64 lanes, for `fadd`/`fmul`/… .
pub fn v2d(n: u8) -> Operand {
    Operand::VReg { num: n, arr: Arr::D2 }
}
/// A `.4s` vector operand (`v3.4s`) — four f32 lanes (SIMD `Float32x4`).
pub fn v4s(n: u8) -> Operand {
    Operand::VReg { num: n, arr: Arr::S4 }
}
/// A double lane element (`v3.d[i]`) — `umov`/`ins` a single f64 lane.
pub fn vd_lane(n: u8, lane: u8) -> Operand {
    Operand::VElem {
        num: n,
        esize: ElemSize::D,
        index: lane,
    }
}
pub fn sp() -> Operand {
    Operand::Reg(Reg {
        class: RegClass::X,
        num: 31,
        is_sp: true,
    })
}
/// A bare `Reg` (not wrapped in `Operand`) — for `cbz`/`tbz`/`ldr_literal`,
/// whose `Assembler` methods take a register directly.
pub fn xr(n: u8) -> Reg {
    Reg {
        class: RegClass::X,
        num: n,
        is_sp: false,
    }
}
pub fn imm(v: i64) -> Operand {
    Operand::Imm(v)
}
/// `[xN, #off]`.
pub fn mem(base: u8, off: i64) -> Operand {
    Operand::Mem(Mem {
        base: mem_base(base),
        addr: Addr::Offset(off),
    })
}
/// `[xN, #off]!` (pre-index writeback).
pub fn mem_pre(base: u8, off: i64) -> Operand {
    Operand::Mem(Mem {
        base: mem_base(base),
        addr: Addr::PreIndex(off),
    })
}
/// `[xN], #off` (post-index writeback).
pub fn mem_post(base: u8, off: i64) -> Operand {
    Operand::Mem(Mem {
        base: mem_base(base),
        addr: Addr::PostIndex(off),
    })
}
fn mem_base(n: u8) -> Reg {
    Reg {
        class: RegClass::X,
        num: n,
        is_sp: n == 31,
    }
}
