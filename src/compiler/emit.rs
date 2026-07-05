//! IR → `Assembler` calls (`sprint_s10_detail.md` D3.6, D5). Walks
//! `regalloc`'s linearized block order, binds one label per block, and
//! emits each `Ir` op's AArch64 sequence via [`crate::compiler::assembler::Assembler`].
//!
//! **Deviations from D5.3's literal per-op sequences.** D5.3's table
//! writes each sequence in terms of stable-looking `xa`/`xb`/`xd` names,
//! but doesn't work through what happens when one of those names is
//! *itself* a scratch register (x16/x17) because its vreg is spilled —
//! several of the literal sequences alias a spilled operand's holding
//! register with an intermediate result the SAME sequence still needs
//! later, silently corrupting it. Found by tracing through the spilled
//! case by hand before writing any test, not by running one:
//! - **Tag checks** (`SmiArith`/`SmiCmpBr`/`SmiCmpVal`): D5.3 computes
//!   `orr x16, xa, xb` as a combined scratch value to `tst`. If `xa` is
//!   itself x16 (spilled), this overwrites it before the real op reads it
//!   again. Fixed by testing each operand's own tag bits directly
//!   (`tst xa, #3; tst xb, #3`) — logically identical (`(a|b)&3 != 0` iff
//!   `a&3!=0 or b&3!=0`), and `tst` never writes a register at all, so
//!   there is nothing to alias.
//! - **`SmiArith::Mul`**: needs the SAME two operands read by BOTH `mul`
//!   and `smulh` (the overflow check needs the full 128-bit product).
//!   D5.3's sequence writes `mul`'s result into x17 — fine if `b` is a
//!   real register, but if `b` is spilled (loaded into x17), that write
//!   destroys it before `smulh` reads it again. Fixed by re-resolving `b`
//!   fresh (a second reload if spilled — the "at most 2 spilled sources"
//!   accounting D3.5 promises doesn't hold across two SEPARATE
//!   instructions needing the same spilled value) and parking `mul`'s
//!   result in `dst`'s own home (real register, or its own spill slot
//!   used as scratch memory) before computing `smulh`.
//! - **`BoolBr`**: loads `true_obj` into x16 before comparing — if `val`
//!   itself is x16 (spilled), the load destroys it before the compare
//!   reads it. Fixed by always resolving `val` into x16 (this op's only
//!   source) and running BOTH literal loads through x17 instead, which
//!   `val`'s resolution never touches.
//!
//! None of this affects `LoadField`, `Move`, `ConstSmi`/`ConstPool`, or
//! the simple `SmiArith` ops (`Add`/`Sub`/`And`/`Or`/`Xor`) — each reads
//! its spilled-and-reloaded operand(s) exactly once, in a single
//! instruction, so read-then-write self-aliasing (standard ISA semantics)
//! is always safe there.
//!
//! **`mov`'s wide-immediate gap.** The vendored encoder's `mov` does NOT
//! auto-expand an arbitrary 64-bit immediate into a movz/movk sequence —
//! `mov_imm` explicitly `bail!`s if the value isn't a single movz/movn/
//! bitmask-orr pattern (checked directly against the vendored source, not
//! assumed from D5.3's own "(or movz/movk pair for wide — via emit(\"mov\",
//! …) which encode.rs expands)" parenthetical, which does not hold).
//! [`emit_mov_imm64`] does the multi-instruction expansion itself.

use crate::compiler::assembler::{
    imm, mem, sp, x, xr, Assembler, CodeBlob, Cond, Label, LiteralId, Operand, Reg, RelocKind,
};
use crate::compiler::ir::{
    BailoutReason, BlockId, CallSiteInfo, CmpOp, Ir, IrMethod, PoolLit, SmiOp, StubId, VReg,
};
use crate::compiler::regalloc::{Assignment, RegallocResult};
use crate::oops::wrappers::SymbolOop;
use crate::vendor::wfasm::a64::parse::Shift;

/// Byte offset a spilled `pos`'th slot lives at, relative to `x29` (D3.4:
/// `[x29 − 8·(i+1)]`).
fn spill_offset(slot: crate::compiler::regalloc::SpillSlot) -> i64 {
    -8 * (slot.0 as i64 + 1)
}

fn spill_mem(slot: crate::compiler::regalloc::SpillSlot) -> Operand {
    mem(29, spill_offset(slot))
}

/// Emit `dst = val` for an arbitrary 64-bit `val` — one `movz` for the
/// lowest 16-bit group, then one `movk` per further group that's actually
/// nonzero (skipping zero groups; always emitting at least the `movz`).
fn emit_mov_imm64(asm: &mut dyn Assembler, dst: Reg, val: u64) {
    let group = |i: u32| ((val >> (16 * i)) & 0xFFFF) as i64;
    asm.emit("movz", &[Operand::Reg(dst), imm(group(0))]);
    for i in 1..4 {
        let g = group(i);
        if g != 0 {
            asm.emit("movk", &[Operand::Reg(dst), Operand::ImmShift(g, 16 * i)]);
        }
    }
}

fn cmp_op_to_cond(op: CmpOp) -> Cond {
    match op {
        CmpOp::Lt => Cond::Lt,
        CmpOp::Le => Cond::Le,
        CmpOp::Gt => Cond::Gt,
        CmpOp::Ge => Cond::Ge,
        CmpOp::Eq => Cond::Eq,
        CmpOp::Ne => Cond::Ne,
    }
}

/// The text form `csel`'s condition operand needs (an `Operand::Sym`,
/// resolved immediately by the vendored encoder's own `cond_code` — never
/// a fixup, so this does not run afoul of P6, which guards against
/// `Operand::Sym` used for *branch targets* specifically).
fn cond_str(c: Cond) -> &'static str {
    match c {
        Cond::Eq => "eq",
        Cond::Ne => "ne",
        Cond::Hs => "hs",
        Cond::Lo => "lo",
        Cond::Mi => "mi",
        Cond::Pl => "pl",
        Cond::Vs => "vs",
        Cond::Vc => "vc",
        Cond::Hi => "hi",
        Cond::Ls => "ls",
        Cond::Ge => "ge",
        Cond::Lt => "lt",
        Cond::Gt => "gt",
        Cond::Le => "le",
    }
}

/// `self_vreg` is always `VReg(0)` by `ir::convert`'s own construction
/// (the very first vreg it allocates) — a deliberate, documented
/// convention rather than a field on `IrMethod`, since nothing else needs
/// to name it.
const SELF_VREG: VReg = VReg(0);

/// One block's start, for the caller (`nmethod.rs`, S10 step 6) to build
/// `PcDesc`s from — `PcDesc` itself doesn't exist until then, so this is a
/// minimal, self-contained stand-in rather than a forward dependency.
#[derive(Clone, Copy, Debug)]
pub struct BlockPc {
    pub pc_off: u32,
    pub bci: usize,
}

/// One `Ir::CallSend`'s emitted `bl` site, for the caller (`driver.rs`) to
/// build real `codecache::nmethod::IcSite`s from — a minimal, self-
/// contained stand-in rather than a forward dependency on `codecache`
/// (same reasoning as [`BlockPc`] for `PcDesc`): every fresh site starts
/// `Unresolved`, which only `driver.rs` (the module that actually knows
/// about `IcState`) needs to say explicitly.
///
/// `site` is the SAME `Ir::CallSend.site` index this came from, carried
/// through so `driver.rs` can look back up `IrMethod.call_sites[site]`
/// (specifically `static_klass`, D4.6) — `regalloc`'s own linearized
/// block order (what `emit` actually walks) isn't guaranteed to match
/// `convert`'s own traversal order (what assigned `call_sites`' own
/// indices), so `emitted_ic_sites`'s OWN position can't be assumed to
/// line up with `call_sites`' position; only the shared `site` value can.
#[derive(Clone, Copy, Debug)]
pub struct EmittedIcSite {
    pub off: u32,
    pub site: u16,
    pub selector: SymbolOop,
    pub argc: u8,
}

/// S12 D2: one REAL safepoint's (a `CallSend`/`CallRuntime`/`Alloc`
/// slow-edge's own `bl`) emitted return-address offset + its enclosing
/// block's bci (trace-path granularity, matching `poll_bci`'s own
/// precedent) + regalloc's own linear position — a minimal, self-contained
/// stand-in for the caller (`driver.rs`) to build a real `OopMap` from
/// (`compiler::oopmap::build_for_position`, which needs
/// `RegallocResult::intervals` — data `emit.rs` never touches, only each
/// vreg's ALREADY-DECIDED assignment), same "accumulate at emit time,
/// resolve at driver.rs time" split as [`BlockPc`]/[`EmittedIcSite`].
#[derive(Clone, Copy, Debug)]
pub struct SafepointPc {
    pub pc_off: u32,
    pub bci: usize,
    pub position: u32,
}

/// S11 D2: parameters for the klass-guard prologue (`entry`, checking the
/// receiver against this nmethod's own customization key, falling through
/// to `verified_entry` on a match or bailing out to `stub_resolve` — acting
/// as `stub_ic_miss`'s other door, D4.1 — on a mismatch). `None` skips the
/// guard entirely, producing exactly S10's old bare-`verified_entry`-only
/// shape: unit tests that aren't about the guard itself (regalloc/spill/
/// overflow-sequencing tests predating S11) pass `None` to keep their own
/// focus narrow, unchanged from before this struct existed.
pub struct EntryGuard {
    pub smi_klass_bits: u64,
    pub key_klass_bits: u64,
    /// `stub_resolve`'s published address (`codecache::stubs::Stubs::
    /// resolve_addr`) — the guard's own miss path reaches it via `ldr
    /// x16,<pool>; br x16` rather than D2's own suggested `b.eq
    /// verified_entry; b stub_ic_miss` (Branch26) form: an indirect branch
    /// through a pool-embedded address sidesteps Branch26/Branch19 range
    /// reasoning entirely (the same reason `Poll`'s own far call already
    /// does this for `stub_poll`), and critically still never touches
    /// x30 the way `blr`/`bl` would (D4.1's own invariant: the guard's
    /// miss path must leave x30 as the ORIGINAL send site's return
    /// address).
    pub resolve_addr: u64,
}

struct Emitter<'a> {
    asm: &'a mut dyn Assembler,
    assignment: Vec<Option<Assignment>>,
    literal_ids: Vec<LiteralId>,
    labels: Vec<Label>,
    epilogue: Label,
    true_lit: PoolLit,
    false_lit: PoolLit,
    /// S11 D7 `Alloc` header/body constants — see `IrMethod::nil_lit`/
    /// `mark_slots_lit`.
    nil_lit: PoolLit,
    mark_slots_lit: PoolLit,
    /// Absolute address of the once-published `stub_poll` (`codecache::
    /// stubs`) — a runtime value `Poll`'s far call embeds as a pool
    /// constant, since `bl`'s ±128 MB range can't reach it directly and
    /// its address isn't known until stub publish time (before any real
    /// method is compiled, but still not a compile-time constant).
    stub_poll_lit: LiteralId,
    /// Same reasoning as `stub_poll_lit`, for `codecache::stubs::Stubs::
    /// must_be_boolean` (S11 step 6) — `Ir::CallRuntime{stub:
    /// StubId::MUST_BE_BOOLEAN}`'s own far call embeds this (S11 step 7).
    must_be_boolean_lit: LiteralId,
    /// Same reasoning again, for `codecache::stubs::Stubs::alloc_slow` — the
    /// `Ir::Alloc` fast path's overflow edge `bl`s here (S11 step 8, D7).
    alloc_slow_lit: LiteralId,
    /// `IrMethod.call_sites`, indexed by `Ir::CallSend.site` — see that
    /// field's own doc.
    call_sites: &'a [CallSiteInfo],
    /// Accumulates one entry per `Ir::CallSend` emitted so far, in
    /// encounter order — handed back to the caller alongside the
    /// `CodeBlob` (mirrors `block_pcs`' own "accumulate during the walk,
    /// return at the end" shape, D3).
    ic_sites: Vec<EmittedIcSite>,
    /// S12: the CURRENT `Ir` op's own linear position, in the EXACT same
    /// numbering `regalloc::compute_intervals` computed
    /// `LiveInterval::start/end/crosses_safepoint` against — `emit()`'s own
    /// loop increments this once per op, walking `block_order` identically,
    /// so a safepoint recorded here always matches the interval data
    /// `driver.rs` later intersects it against.
    pos: u32,
    /// S12: the CURRENT block's own bci — `poll_bci`'s own established
    /// block-granularity precedent, reused here for every real safepoint's
    /// `SafepointPc::bci` too (trace-path only; GC correctness only ever
    /// needs `oopmap`, never `bci`).
    current_bci: usize,
    /// S12: accumulates one entry per REAL safepoint (CallSend/
    /// CallRuntime/Alloc's slow edge) emitted so far — see
    /// [`SafepointPc`]'s own doc.
    safepoints: Vec<SafepointPc>,
}

impl<'a> Emitter<'a> {
    fn assignment_of(&self, v: VReg) -> Assignment {
        self.assignment[v.0 as usize]
            .expect("emit: every vreg an Ir op references must have a regalloc assignment")
    }

    fn block_label(&self, b: BlockId) -> Label {
        self.labels[b.0 as usize]
    }

    /// Resolve `v` as a source for the CURRENT instruction: its own
    /// register if allocated, or a fresh load into `scratch` if spilled.
    /// Safe to call more than once for the same vreg within one Ir's
    /// emission — each call re-emits its own load if spilled, trading a
    /// little redundancy for never needing to reason about whether an
    /// earlier resolution is still valid (see `Mul`'s own handling, and
    /// this module's own doc).
    fn resolve(&mut self, v: VReg, scratch: u8) -> Reg {
        match self.assignment_of(v) {
            Assignment::Reg(r) => xr(r),
            Assignment::Spill(slot) => {
                self.asm.emit("ldr", &[x(scratch), spill_mem(slot)]);
                xr(scratch)
            }
        }
    }

    /// Where the CURRENT instruction should write `dst`'s result: its own
    /// register, or x16 (D3.5's dest-scratch convention) if spilled — call
    /// [`Self::commit`] afterward to store x16 out if it was.
    fn dest_target(&self, dst: VReg) -> Reg {
        match self.assignment_of(dst) {
            Assignment::Reg(r) => xr(r),
            Assignment::Spill(_) => xr(16),
        }
    }

    fn commit(&mut self, dst: VReg, computed_in: Reg) {
        if let Assignment::Spill(slot) = self.assignment_of(dst) {
            self.asm
                .emit("str", &[Operand::Reg(computed_in), spill_mem(slot)]);
        }
    }

    fn emit_tag_check(&mut self, ra: Reg, rb: Reg, fail: BlockId) {
        let fail_label = self.block_label(fail);
        self.asm.emit("tst", &[Operand::Reg(ra), imm(3)]);
        self.asm.b_cond(Cond::Ne, fail_label);
        self.asm.emit("tst", &[Operand::Reg(rb), imm(3)]);
        self.asm.b_cond(Cond::Ne, fail_label);
    }

    fn emit_smi_arith_simple(&mut self, op: SmiOp, dst: VReg, a: VReg, b: VReg, fail: BlockId) {
        let ra = self.resolve(a, 16);
        let rb = self.resolve(b, 17);
        self.emit_tag_check(ra, rb, fail);
        let d = self.dest_target(dst);
        let mnem = match op {
            SmiOp::Add => "adds",
            SmiOp::Sub => "subs",
            SmiOp::And => "and",
            SmiOp::Or => "orr",
            SmiOp::Xor => "eor",
            SmiOp::Mul => unreachable!("Mul is dispatched to emit_smi_mul, never here"),
        };
        self.asm
            .emit(mnem, &[Operand::Reg(d), Operand::Reg(ra), Operand::Reg(rb)]);
        if matches!(op, SmiOp::Add | SmiOp::Sub) {
            let fail_label = self.block_label(fail);
            self.asm.b_cond(Cond::Vs, fail_label);
        }
        self.commit(dst, d);
    }

    /// See this module's own doc for why this differs from D5.3's literal
    /// sequence: `mul` and `smulh` both need `shifted_a` and `b`, so
    /// neither's result may land where the other still needs to read from.
    fn emit_smi_mul(&mut self, dst: VReg, a: VReg, b: VReg, fail: BlockId) {
        let ra = self.resolve(a, 16);
        let rb0 = self.resolve(b, 17);
        self.emit_tag_check(ra, rb0, fail);

        self.asm.emit("asr", &[x(16), Operand::Reg(ra), imm(2)]); // shifted_a
        let rb1 = self.resolve(b, 17); // fresh: tag check didn't write it, but be explicit
        self.asm.emit("mul", &[x(17), x(16), Operand::Reg(rb1)]);

        // Park the low 64 bits somewhere smulh's own x16/x17 traffic can't
        // reach: dst's real register, or its own spill slot used as
        // scratch memory (already holds the correct final value; the
        // reload below is a read-back for the comparison, not a second,
        // different write).
        match self.assignment_of(dst) {
            Assignment::Reg(r) => self.asm.emit("mov", &[x(r), x(17)]),
            Assignment::Spill(slot) => self.asm.emit("str", &[x(17), spill_mem(slot)]),
        }

        let rb2 = self.resolve(b, 17); // fresh again: mul just overwrote x17
        self.asm.emit("smulh", &[x(16), x(16), Operand::Reg(rb2)]);

        let low = match self.assignment_of(dst) {
            Assignment::Reg(r) => xr(r),
            Assignment::Spill(slot) => {
                self.asm.emit("ldr", &[x(17), spill_mem(slot)]);
                xr(17)
            }
        };
        self.asm
            .emit("cmp", &[x(16), Operand::RegShift(low, Shift::Asr, 63)]);
        let fail_label = self.block_label(fail);
        self.asm.b_cond(Cond::Ne, fail_label);
    }

    fn emit_load_field(&mut self, dst: VReg, obj: VReg, byte_off: i32) {
        let ra = self.resolve(obj, 16);
        let d = self.dest_target(dst);
        let biased = byte_off as i64 - 1;
        if (-256..=255).contains(&biased) {
            self.asm
                .emit("ldur", &[Operand::Reg(d), mem(ra.num, biased)]);
        } else {
            self.asm.emit("sub", &[x(16), Operand::Reg(ra), imm(1)]);
            self.asm
                .emit("ldr", &[Operand::Reg(d), mem(16, byte_off as i64)]);
        }
        self.commit(dst, d);
    }

    /// D3/P7: mirrors [`Self::emit_load_field`]'s own MEM_TAG-bias split
    /// (`ldur`/±255 vs `sub`+`ldr`) for the STORE direction (`stur`/`str`),
    /// then, if `barrier`, appends the write barrier AFTER the store
    /// (`memory::store::store`'s own order: `*slot = val;` first, THEN the
    /// conditional dirty — SPEC §7.4).
    ///
    /// Every comparison here is on raw ADDRESSES, so uses the UNSIGNED
    /// conditions (`Lo`/`Hs`, never `Lt`/`Ge`) — matching
    /// `memory::layout::HeapLayout::is_old`/`is_new`'s own plain `usize`
    /// `>=`/`<`, which is unsigned by construction (same reasoning:
    /// `is_old`/`is_new`'s own doc notes tagged oops compare directly
    /// against `old_start` unchanged, no untagging needed, since MEM_TAG's
    /// +1 bias can never cross a page-aligned boundary — used here too:
    /// `robj`/`rval` are compared TAGGED, exactly as `resolve` hands them
    /// back).
    ///
    /// Register discipline: `robj`/`rval` (`x16`/`x17` if their vregs are
    /// spilled) must stay valid from the store through the LAST barrier
    /// check that reads each — so unlike `emit_load_field`'s own far-store
    /// case (which safely clobbers `x16` since `ra` is never read again
    /// after the load), the far-STORE case here uses `x19` for the
    /// untagged base instead, leaving `x16`/`x17` untouched for the
    /// barrier. `x19`/`x20` (regalloc's own "unused" range, never assigned
    /// a real vreg — `regalloc.rs`'s module doc) are the two extra scratch
    /// temps the barrier itself needs (`old_start`, then the slot/card
    /// computation once `old_start`'s own last read has passed);
    /// recomputing the untagged slot address fresh here (rather than
    /// trying to reuse whatever the near/far store path above left behind)
    /// trades one redundant `add` in the far case for not having to reason
    /// about two different leftover-register shapes.
    fn emit_store_field(&mut self, obj: VReg, byte_off: i32, val: VReg, barrier: bool) {
        let robj = self.resolve(obj, 16);
        let rval = self.resolve(val, 17);
        let biased = byte_off as i64 - 1;
        if (-256..=255).contains(&biased) {
            self.asm
                .emit("stur", &[Operand::Reg(rval), mem(robj.num, biased)]);
        } else {
            self.asm
                .emit("add", &[x(19), Operand::Reg(robj), imm(biased)]);
            self.asm.emit("str", &[Operand::Reg(rval), mem(19, 0)]);
        }

        if !barrier {
            return;
        }

        let skip = self.asm.new_label();
        self.asm.emit(
            "ldr",
            &[
                x(20),
                mem(28, crate::oops::layout::VMREG_OLD_START_OFFSET as i64),
            ],
        );
        self.asm.emit("cmp", &[Operand::Reg(robj), x(20)]);
        self.asm.b_cond(Cond::Lo, skip); // obj < old_start -> young -> no barrier
        self.asm.emit("tst", &[Operand::Reg(rval), imm(3)]);
        self.asm.b_cond(Cond::Eq, skip); // val is smi -> no barrier
        self.asm.emit("cmp", &[Operand::Reg(rval), x(20)]);
        self.asm.b_cond(Cond::Hs, skip); // val >= old_start -> old, not new -> no barrier

        self.asm
            .emit("add", &[x(19), Operand::Reg(robj), imm(biased)]);
        self.asm.emit(
            "lsr",
            &[x(19), x(19), imm(crate::memory::cards::CARD_SHIFT as i64)],
        );
        self.asm.emit(
            "ldr",
            &[
                x(20),
                mem(
                    28,
                    crate::oops::layout::VMREG_CARD_BASE_BIASED_OFFSET as i64,
                ),
            ],
        );
        self.asm.emit("add", &[x(20), x(20), x(19)]);
        self.asm.emit("strb", &[x(31), mem(20, 0)]); // xzr: CARD_DIRTY == 0

        self.asm.bind(skip);
    }

    /// S11 D7: inline allocation of a fixed-size `Format::Slots` object.
    /// Self-contained fast-path-plus-slow-call, no separate CFG block
    /// (mirrors `emit_store_field`'s barrier). Fast path: bump the LIVE
    /// eden bump pointer — dereferenced through `reg_block.eden_top_addr`
    /// (S12 step 7: the address of `universe.eden.top` itself, the same
    /// word every Rust-side allocator and both collectors use; a value
    /// copy here would go stale the moment a nested allocation or a GC
    /// under this very frame moved the real pointer) — bounds-check
    /// against `eden_end` (a genesis-fixed bound, safe as a value copy),
    /// stamp the pristine Slots mark + klass oop + nil body, mem-tag the
    /// result. Overflow → `bl stub_alloc_slow` (a real allocation, which
    /// post-D8-bridge may itself scavenge — coherent for the SAME reason:
    /// the next fast-path bump re-reads the live word).
    ///
    /// Register discipline: `x20` holds the eden-top ADDRESS live until
    /// the committing `str`; `x19` holds the raw object base across the
    /// whole header/body init — deliberately NOT x16, which `dest_target`
    /// hands back for a spilled `dst` and would alias the base. `x19` is
    /// briefly clobbered by the bounds check (end loaded into it while
    /// `x17` holds new_top and `x20` the address — only 3 scratches
    /// exist) and recovered arithmetically (`obj = new_top − size`).
    /// `x17` is the store-value scratch afterward (reloaded per
    /// constant). Both paths land the tagged result in `d`, then one
    /// `commit`. `Alloc` is a regalloc SAFEPOINT (`regalloc::
    /// is_safepoint`), so every vreg live across it is already spilled
    /// before the slow path's `bl` clobbers a caller-saved register.
    fn emit_alloc(&mut self, dst: VReg, klass: PoolLit, size_words: u32) {
        use crate::oops::layout::{
            HEADER_WORDS, MEM_TAG, VMREG_EDEN_END_OFFSET, VMREG_EDEN_TOP_ADDR_OFFSET, WORD_SIZE,
        };
        let size_bytes = size_words as i64 * WORD_SIZE as i64;
        // ir.rs's own Alloc detection only fires for a header-plus-body that
        // fits a 12-bit `add`/`str` immediate; a pathological giant class
        // stays an ordinary generic `basicNew` send instead. (The
        // recovering `sub` below shares the same 12-bit immediate range.)
        debug_assert!(
            size_words as usize >= HEADER_WORDS && size_bytes < 4096,
            "emit_alloc: size_bytes {size_bytes} out of the inline range -- ir.rs must gate this"
        );
        let d = self.dest_target(dst);
        let slow = self.asm.new_label();
        let done = self.asm.new_label();

        // Fast path: addr = &eden.top; obj = *addr; new_top = obj + size;
        // if new_top > end (unsigned) -> slow; else *addr = new_top.
        self.asm
            .emit("ldr", &[x(20), mem(28, VMREG_EDEN_TOP_ADDR_OFFSET as i64)]);
        self.asm.emit("ldr", &[x(19), mem(20, 0)]);
        self.asm.emit("add", &[x(17), x(19), imm(size_bytes)]);
        self.asm
            .emit("ldr", &[x(19), mem(28, VMREG_EDEN_END_OFFSET as i64)]);
        self.asm.emit("cmp", &[x(17), x(19)]);
        self.asm.b_cond(Cond::Hi, slow);
        self.asm.emit("str", &[x(17), mem(20, 0)]);
        self.asm.emit("sub", &[x(19), x(17), imm(size_bytes)]);

        // Header: pristine Slots mark at [obj+0], klass oop at [obj+8].
        self.asm
            .ldr_literal(xr(17), self.literal_ids[self.mark_slots_lit.0 as usize]);
        self.asm.emit("str", &[x(17), mem(19, 0)]);
        self.asm
            .ldr_literal(xr(17), self.literal_ids[klass.0 as usize]);
        self.asm.emit("str", &[x(17), mem(19, WORD_SIZE as i64)]);

        // Nil-init the named body (slots 2..size_words) -- MANDATORY (D7):
        // a GC at the next safepoint would scan a garbage body otherwise.
        let body_words = size_words as usize - HEADER_WORDS;
        if body_words > 0 {
            self.asm
                .ldr_literal(xr(17), self.literal_ids[self.nil_lit.0 as usize]);
            for i in 0..body_words {
                let off = ((HEADER_WORDS + i) * WORD_SIZE) as i64;
                self.asm.emit("str", &[x(17), mem(19, off)]);
            }
        }

        // Mem-tag: d = obj + MEM_TAG.
        self.asm
            .emit("add", &[Operand::Reg(d), x(19), imm(MEM_TAG as i64)]);
        self.asm.b(done);

        // Slow path: bl stub_alloc_slow(klass -> x0, size_bytes -> x1) -> x0.
        self.asm.bind(slow);
        self.asm
            .ldr_literal(xr(0), self.literal_ids[klass.0 as usize]);
        emit_mov_imm64(self.asm, xr(1), size_bytes as u64);
        self.asm.call_far(self.alloc_slow_lit);
        // S12 D2: `Ir::Alloc`'s ONE safepoint position (`is_safepoint`
        // treats fast+slow paths as a single program point) — only the
        // slow edge actually calls into Rust, so this is the only place
        // within this op a real return address exists to record.
        self.safepoints.push(SafepointPc {
            pc_off: self.asm.offset(),
            bci: self.current_bci,
            position: self.pos,
        });
        if d.num != 0 {
            self.asm.emit("mov", &[Operand::Reg(d), x(0)]);
        }

        self.asm.bind(done);
        self.commit(dst, d);
    }

    fn emit_smi_cmp_br(
        &mut self,
        op: CmpOp,
        a: VReg,
        b: VReg,
        if_true: BlockId,
        if_false: BlockId,
        fail: BlockId,
    ) {
        let ra = self.resolve(a, 16);
        let rb = self.resolve(b, 17);
        self.emit_tag_check(ra, rb, fail);
        self.asm.emit("cmp", &[Operand::Reg(ra), Operand::Reg(rb)]);
        let true_label = self.block_label(if_true);
        self.asm.b_cond(cmp_op_to_cond(op), true_label);
        let false_label = self.block_label(if_false);
        self.asm.b(false_label);
    }

    fn emit_smi_cmp_val(&mut self, op: CmpOp, dst: VReg, a: VReg, b: VReg, fail: BlockId) {
        let ra = self.resolve(a, 16);
        let rb = self.resolve(b, 17);
        self.emit_tag_check(ra, rb, fail);
        self.asm.emit("cmp", &[Operand::Reg(ra), Operand::Reg(rb)]);
        // cmp doesn't write ra/rb -- free to reuse x16/x17 for the two
        // literal loads regardless of what they held a moment ago.
        self.asm
            .ldr_literal(xr(16), self.literal_ids[self.true_lit.0 as usize]);
        self.asm
            .ldr_literal(xr(17), self.literal_ids[self.false_lit.0 as usize]);
        let d = self.dest_target(dst);
        let cond_sym = Operand::Sym(cond_str(cmp_op_to_cond(op)).to_string());
        self.asm
            .emit("csel", &[Operand::Reg(d), x(16), x(17), cond_sym]);
        self.commit(dst, d);
    }

    /// `val` always resolves into x16 (its only possible spilled slot,
    /// D3.5's first-source convention) — both literal loads go through
    /// x17 instead, which `val`'s own resolution never touches, so a
    /// spilled `val` is never at risk (see this module's own doc).
    fn emit_bool_br(&mut self, val: VReg, if_true: BlockId, if_false: BlockId, not_bool: BlockId) {
        let rv = self.resolve(val, 16);
        self.asm
            .ldr_literal(xr(17), self.literal_ids[self.true_lit.0 as usize]);
        self.asm.emit("cmp", &[Operand::Reg(rv), x(17)]);
        let true_label = self.block_label(if_true);
        self.asm.b_cond(Cond::Eq, true_label);
        self.asm
            .ldr_literal(xr(17), self.literal_ids[self.false_lit.0 as usize]);
        self.asm.emit("cmp", &[Operand::Reg(rv), x(17)]);
        let false_label = self.block_label(if_false);
        self.asm.b_cond(Cond::Eq, false_label);
        let not_bool_label = self.block_label(not_bool);
        self.asm.b(not_bool_label);
    }

    /// `ldr w16, [x28, #POLL_OFF]; cbz w16, skip; bl stub_poll; skip:` —
    /// simpler than D5.3's "shared per-method tail block" framing (which
    /// is ambiguous for a method with more than one loop: which Poll site
    /// does a single shared tail belong to?) and provably correct for any
    /// number of Poll sites, since each is fully self-contained: `bl`
    /// itself guarantees `stub_poll`'s own `ret` resumes right after the
    /// call, so nothing needs a hand-rolled "jump back to the right
    /// place".
    fn emit_poll(&mut self) {
        let skip = self.asm.new_label();
        // ldr w16, [x28, #VMREG_POLL_FLAG_OFFSET]; cbz w16, skip
        self.asm.emit(
            "ldr",
            &[
                crate::compiler::assembler::w(16),
                mem(28, crate::oops::layout::VMREG_POLL_FLAG_OFFSET as i64),
            ],
        );
        self.asm.cbz(xr(16), skip);
        self.asm.call_far(self.stub_poll_lit);
        // S13 step 10b: the poll is a deopt SAFEPOINT. Record its `SafepointPc`
        // at the `bl stub_poll` RETURN address — the offset right AFTER the
        // call, which is exactly where `skip` binds (`cbz` also lands here on a
        // dormant flag). `stub_poll`'s `rt_poll` passes this same address as
        // `ret_pc`, and `driver::build_deopt_metadata` keys the LoopPoll deopt
        // scope's `PcDesc.code_off` on it — the SAME "return-address safepoint"
        // convention `emit_call_send` uses (read `asm.offset()` fresh, not
        // derived from any encoding length). Recording it here (before `bind`)
        // vs. after is equivalent: `bind` emits no code, so the offset is
        // identical either way; recording BEFORE keeps `self.pos` reading this
        // `Ir::Poll`'s own position (the driver/regalloc share numbering).
        self.safepoints.push(SafepointPc {
            pc_off: self.asm.offset(),
            bci: self.current_bci,
            position: self.pos,
        });
        self.asm.bind(skip);
    }

    /// D3 step 1-4: marshal receiver+args into x0..x5, `bl_patchable` the
    /// site, record it, land the result in `dst`. Every fresh site's `bl`
    /// self-targets (D3: "no per-site pre-resolution") — `driver.rs`
    /// patches it to `stub_resolve` once the blob is published (the
    /// address isn't known here, at emit time, any more than a real send
    /// target would be).
    ///
    /// The marshaling is a REAL parallel move, not a naive sequential
    /// `resolve(args[i], i)` per slot: regalloc's spill-all-at-safepoint
    /// policy (`LiveInterval::crosses_safepoint`'s own doc, regalloc.rs)
    /// only forces `Spill` on a vreg whose interval EXTENDS PAST the
    /// safepoint — an arg whose only use IS this call ends exactly here,
    /// so it can legitimately still be plain `Assignment::Reg`, including
    /// in a DIFFERENT x{j} another arg also needs to land in. A sequential
    /// per-slot move can clobber one arg's source register before it's
    /// read — the RetSelf bug's exact failure shape, in a different
    /// instruction — which is exactly what a first draft of this function
    /// did, caught by its own now-removed debug assert once a real
    /// register-pressure test (not just a spilled one) exercised it.
    fn emit_call_send(&mut self, dst: VReg, site: u16, args: &[VReg]) {
        let sources: Vec<Assignment> = args.iter().map(|&a| self.assignment_of(a)).collect();

        // A single parallel-move problem over ALL args, register- and
        // spill-assigned alike -- NOT spill-loads-first-then-register-
        // shuffle (an earlier draft's bug): a spilled arg's destination
        // x{i} can alias a DIFFERENT arg's CURRENT register (e.g. arg 0
        // spilled, arg 1 presently sitting in x0 because its whole live
        // range ends at this call, D3.6's own "spill-all-at-safepoint only
        // forces `Spill` on a vreg whose interval extends PAST the
        // safepoint" -- see this fn's own doc above). Loading arg 0's
        // spill straight into x0 first would clobber arg 1's value before
        // its own move ever reads it -- the exact hazard the register-only
        // shuffle below already guards against, just not (until now) for a
        // spill-load's write. `pending`: (dest, source) pairs still
        // needing a move, skipping any register source already in place
        // (src reg == dest).
        #[derive(Clone, Copy)]
        enum Src {
            Reg(u8),
            Mem(crate::compiler::regalloc::SpillSlot),
        }
        let mut pending: Vec<(u8, Src)> = sources
            .iter()
            .enumerate()
            .filter_map(|(i, src)| match *src {
                Assignment::Reg(r) if r != i as u8 => Some((i as u8, Src::Reg(r))),
                Assignment::Reg(_) => None,
                Assignment::Spill(slot) => Some((i as u8, Src::Mem(slot))),
            })
            .collect();
        while !pending.is_empty() {
            // An entry is safe to emit now iff no OTHER pending entry
            // still needs to READ x{i} as ITS OWN register source
            // (emitting this one first -- register move or spill load
            // alike -- would clobber that other entry's source). A `Mem`
            // source is never itself a reader of anyone else's
            // destination, so it can block others but can never sit in a
            // genuine cycle (see the cycle-break branch below).
            if let Some(pos) = pending.iter().position(|&(i, _)| {
                !pending
                    .iter()
                    .any(|&(_, s)| matches!(s, Src::Reg(r) if r == i))
            }) {
                let (i, s) = pending.remove(pos);
                match s {
                    Src::Reg(r) => self.asm.emit("mov", &[x(i), x(r)]),
                    Src::Mem(slot) => self.asm.emit("ldr", &[x(i), spill_mem(slot)]),
                }
            } else {
                // A genuine cycle (e.g. x0<-x1, x1<-x0): only possible
                // among `Reg` entries -- a `Mem` entry has no register
                // source of its own to need reading, so it can be BLOCKED
                // but never a participant in the mutual-block that makes
                // this branch necessary. Break it via x16, preserving the
                // about-to-be-overwritten destination's current value for
                // whichever other pending move still needs to read it.
                let (i0, r0) = match pending[0] {
                    (i, Src::Reg(r)) => (i, r),
                    (_, Src::Mem(_)) => {
                        unreachable!("a Mem source is never part of a genuine cycle")
                    }
                };
                self.asm.emit("mov", &[x(16), x(i0)]);
                for (_, s) in pending.iter_mut() {
                    if let Src::Reg(r) = s {
                        if *r == i0 {
                            *r = 16;
                        }
                    }
                }
                self.asm.emit("mov", &[x(i0), x(r0)]);
                pending.remove(0);
            }
        }

        let off = self.asm.bl_patchable(RelocKind::InlineCache);
        // S12 D2: the safepoint is THIS call's own return address — read
        // `asm.offset()` again right here (not `off + 4`) so this doesn't
        // hardcode `bl`'s own encoding length; sound regardless of how
        // `bl_patchable` is actually implemented, matching `block_pcs`'
        // own "current offset" idiom.
        self.safepoints.push(SafepointPc {
            pc_off: self.asm.offset(),
            bci: self.current_bci,
            position: self.pos,
        });
        let info = self.call_sites[site as usize];
        self.ic_sites.push(EmittedIcSite {
            off,
            site,
            selector: info.selector,
            argc: info.argc,
        });
        self.emit_nlr_check();
        let d = self.dest_target(dst);
        if d.num != 0 {
            self.asm.emit("mov", &[Operand::Reg(d), x(0)]);
        }
        self.commit(dst, d);
    }

    /// S13 step 7b (D3, the first "organic trap client"): lower an
    /// `Ir::UncommonTrap` to a `brk #0xDE00`. Unlike `emit_call_send`/
    /// `emit_alloc`, whose safepoint keys on a RETURN address (the pc AFTER
    /// the `bl`), a trap keys on the `brk` instruction's OWN offset — the
    /// trapping pc IS the brk (the SIGTRAP handler reads `__pc` = this exact
    /// offset). So record `pc_off = self.asm.offset()` BEFORE emitting the
    /// brk (not after). The driver's existing oopmap loop + `build_deopt_
    /// metadata` both iterate `safepoints`/`deopt_sites` keyed by this same
    /// `position`, so this one `SafepointPc` push gets the trap BOTH an OopMap
    /// (S12) and a deopt scope (S13), correlated at the brk offset. No result,
    /// no fall-through — control leaves the method via the trap.
    fn emit_uncommon_trap(&mut self) {
        let pc_off = self.asm.offset();
        self.safepoints.push(SafepointPc {
            pc_off,
            bci: self.current_bci,
            position: self.pos,
        });
        crate::codecache::deopt_trap::emit_brk(
            self.asm,
            crate::codecache::deopt_trap::TRAP_UNCOMMON,
        );
    }

    /// S11 D6.3: the per-call-site NLR-escape check (P10's "2 words per
    /// site", v1: after EVERY send/runtime call unconditionally). A callee
    /// that was unwound by a non-local return hands back the `NLR_SENTINEL`
    /// (a RESERVED_TAG word no real oop can equal) instead of a result;
    /// this method must then IMMEDIATELY return the sentinel to ITS caller
    /// — via its own ordinary epilogue, x0 untouched — so the escape
    /// propagates one native frame at a time all the way back to
    /// `enter_compiled`, which resumes the interpreter-side unwind. The
    /// sprint doc's original mechanism (a `stub_nlr_unwind` "restoring
    /// sp/fp from the tier link") was unimplementable as written — the
    /// tier link holds PROCESS-stack indices, not native registers, and a
    /// `bl`'d stub would `ret` right back into this site (see the D6.3
    /// SPEC-QUESTION); branching to the epilogue needs no native-frame
    /// surgery at all. `sub`+`cbz` rather than `cmp #imm; b.eq` — both
    /// encodings are already proven in this backend (`emit_alloc`'s own
    /// `sub`, `emit_poll`'s own `cbz`), and x17 is free scratch right
    /// after any call.
    fn emit_nlr_check(&mut self) {
        self.asm.emit(
            "sub",
            &[x(17), x(0), imm(crate::oops::layout::NLR_SENTINEL as i64)],
        );
        let epi = self.epilogue;
        self.asm.cbz(xr(17), epi);
    }

    /// S11 step 7 (D1/D5): calls a FIXED runtime stub address (no IC state
    /// machine, no site table — unlike `emit_call_send`, this target never
    /// changes). Only ever `StubId::MUST_BE_BOOLEAN` today (one argument,
    /// arriving in x0 — `codecache::stubs::build_stub_must_be_boolean`'s
    /// own callee-shaped, plain-`ret` contract, so this is a completely
    /// ordinary call from the emitted code's own perspective: marshal into
    /// x0, `bl`, result in x0). No parallel-move shuffle needed the way
    /// `emit_call_send` requires: with exactly one argument there is
    /// nothing for it to collide with.
    fn emit_call_runtime(&mut self, dst: Option<VReg>, stub: StubId, args: &[VReg]) {
        assert_eq!(
            stub,
            StubId::MUST_BE_BOOLEAN,
            "emit_call_runtime: only MUST_BE_BOOLEAN is wired up (S11 step 7) -- \
             rt_alloc_slow/stub_nlr_unwind aren't CallRuntime targets yet (steps 8/9)"
        );
        assert_eq!(
            args.len(),
            1,
            "emit_call_runtime: MUST_BE_BOOLEAN takes exactly one argument"
        );
        let ra = self.resolve(args[0], 16);
        if ra.num != 0 {
            self.asm.emit("mov", &[x(0), Operand::Reg(ra)]);
        }
        self.asm.call_far(self.must_be_boolean_lit);
        // S12 D2: same reasoning as `emit_call_send`'s own safepoint push —
        // this call's return address, read fresh rather than derived from
        // `call_far`'s own internal encoding.
        self.safepoints.push(SafepointPc {
            pc_off: self.asm.offset(),
            bci: self.current_bci,
            position: self.pos,
        });
        // S11 D6.3: a #mustBeBoolean handler is guest code too — if it (or
        // anything it re-enters) NLRs past this frame, the sentinel comes
        // back here exactly like at a send site.
        self.emit_nlr_check();
        let dst = dst.expect("MUST_BE_BOOLEAN always produces a result (a coerced boolean)");
        let d = self.dest_target(dst);
        if d.num != 0 {
            self.asm.emit("mov", &[Operand::Reg(d), x(0)]);
        }
        self.commit(dst, d);
    }
}

/// D3.4: pull `IrMethod.pool` into the vendored assembler's own literal
/// pool, 1:1 by `PoolLit` index, so every `ConstPool`/`SmiCmpVal`/`BoolBr`
/// reference resolves to a real `LiteralId`.
fn intern_pool(asm: &mut dyn Assembler, method: &IrMethod) -> Vec<LiteralId> {
    method
        .pool
        .iter()
        .map(|e| asm.literal_u64(e.value, e.kind))
        .collect()
}

/// S11 D2: the klass-guard prologue (`entry`, checking the receiver
/// against `guard.key_klass_bits`, falling through to `verified_entry` on
/// a match or bailing out to `stub_resolve` — `stub_ic_miss`'s other door,
/// D4.1 — on a mismatch). Emits nothing else; the caller's own next
/// instruction IS `verified_entry` (no label needed for that side — the
/// guard's `b.eq` target is bound at the very end of this function, which
/// is exactly that point).
fn emit_entry_guard(asm: &mut dyn Assembler, guard: &EntryGuard) {
    let smi_lit = asm.literal_u64(guard.smi_klass_bits, Some(RelocKind::Oop));
    let key_lit = asm.literal_u64(guard.key_klass_bits, Some(RelocKind::KeyKlassOop));
    let resolve_lit = asm.literal_u64(guard.resolve_addr, Some(RelocKind::RuntimeAddr));

    let smi_case = asm.new_label();
    let after_klass_load = asm.new_label();
    let matched = asm.new_label();

    asm.emit("tst", &[x(0), imm(3)]);
    asm.b_cond(Cond::Eq, smi_case);
    // Heap case: klass word is at untagged-address + KLASS_OFFSET(8), and
    // MEM_TAG(1) biases x0 by -1 -- ldur (unscaled) since 7 isn't 8-aligned.
    asm.emit("ldur", &[x(17), mem(0, 7)]);
    asm.b(after_klass_load);
    asm.bind(smi_case);
    asm.ldr_literal(xr(17), smi_lit);
    asm.bind(after_klass_load);
    asm.ldr_literal(xr(16), key_lit);
    asm.emit("cmp", &[x(17), x(16)]);
    asm.b_cond(Cond::Eq, matched);
    // Miss: an indirect branch through a pool-embedded address, not D2's
    // own suggested `b.eq verified_entry; b stub_ic_miss` (Branch26) form
    // -- sidesteps Branch26/19 range reasoning entirely (same reason
    // Poll's own far call already does this for stub_poll) while still
    // never touching x30 the way `blr`/`bl` would (D4.1's own invariant:
    // this path must leave x30 as the send site's own return address).
    asm.ldr_literal(xr(16), resolve_lit);
    asm.emit("br", &[x(16)]);
    asm.bind(matched);
}

/// D3.6/D5: emit `method`'s whole body — the klass-guard prologue (S11 D2,
/// when `guard` is `Some`), the S10-era `verified_entry` prologue, every
/// block in `regalloc`'s linearized order, shared epilogue — and finish
/// into a `CodeBlob`. `stub_poll_addr` is `stub_poll`'s already-published
/// address (irrelevant if `method` has no `Poll` op at all, but always
/// required by this signature — S10's own methods are simple enough that
/// threading an `Option` through for the rare case wasn't worth it).
/// Returns `verified_entry_off` (== 0 when `guard` is `None`, matching
/// S10's own `entry_off == verified_entry_off` convention), one
/// [`EmittedIcSite`] per `Ir::CallSend` encountered in encounter order, and
/// (S12) one [`SafepointPc`] per real safepoint (`CallSend`/`CallRuntime`/
/// `Alloc`'s slow edge) encountered, also in encounter order.
pub fn emit(
    asm: &mut dyn Assembler,
    method: &IrMethod,
    regalloc: &RegallocResult,
    stub_poll_addr: u64,
    must_be_boolean_addr: u64,
    alloc_slow_addr: u64,
    guard: Option<EntryGuard>,
) -> (
    CodeBlob,
    Vec<BlockPc>,
    u32,
    Vec<EmittedIcSite>,
    Vec<SafepointPc>,
) {
    let literal_ids = intern_pool(asm, method);

    let mut assignment: Vec<Option<Assignment>> = vec![None; method.vregs.len()];
    for iv in &regalloc.intervals {
        assignment[iv.vreg.0 as usize] = iv.assignment;
    }

    let labels: Vec<Label> = (0..method.blocks.len()).map(|_| asm.new_label()).collect();
    let epilogue = asm.new_label();
    let stub_poll_lit = asm.literal_u64(stub_poll_addr, Some(RelocKind::RuntimeAddr));
    let must_be_boolean_lit = asm.literal_u64(must_be_boolean_addr, Some(RelocKind::RuntimeAddr));
    let alloc_slow_lit = asm.literal_u64(alloc_slow_addr, Some(RelocKind::RuntimeAddr));

    if let Some(g) = &guard {
        emit_entry_guard(asm, g);
    }
    let verified_entry_off = asm.offset();

    let mut e = Emitter {
        asm,
        assignment,
        literal_ids,
        labels,
        epilogue,
        true_lit: method.true_lit,
        false_lit: method.false_lit,
        nil_lit: method.nil_lit,
        mark_slots_lit: method.mark_slots_lit,
        stub_poll_lit,
        must_be_boolean_lit,
        alloc_slow_lit,
        call_sites: &method.call_sites,
        ic_sites: Vec::new(),
        pos: 0,
        current_bci: 0,
        safepoints: Vec::new(),
    };

    // Prologue (D5.2): frame_bytes = 8*frame_slots, rounded to 16.
    let frame_bytes = ((8 * regalloc.frame_slots as i64) + 15) & !15;
    e.asm.emit(
        "stp",
        &[x(29), x(30), crate::compiler::assembler::mem_pre(31, -16)],
    );
    e.asm.emit("mov", &[x(29), sp()]);
    if frame_bytes > 0 {
        e.asm.emit("sub", &[sp(), sp(), imm(frame_bytes)]);
    }

    let mut block_pcs = Vec::with_capacity(method.blocks.len());
    for (i, &bid) in regalloc.block_order.iter().enumerate() {
        let block = &method.blocks[bid.0 as usize];
        e.asm.bind(e.labels[bid.0 as usize]);
        block_pcs.push(BlockPc {
            pc_off: e.asm.offset(),
            bci: block.bci,
        });
        // S12: `current_bci` tracks the ENCLOSING block for any safepoint
        // emitted within it (block-granularity, matching `poll_bci`'s own
        // precedent — GC correctness never needs bci, only trace does).
        e.current_bci = block.bci;

        let next_in_order = regalloc.block_order.get(i + 1).copied();
        for ir in &block.code {
            // S12: `e.pos` is THIS ir's own position, in the EXACT same
            // linear numbering `regalloc::compute_intervals` used —
            // incremented AFTER emitting (matching that function's own
            // `pos += 1` placement), never before, so a safepoint recorded
            // mid-emission reads the position it was actually computed at.
            emit_ir(&mut e, ir, next_in_order);
            e.pos += 1;
        }
    }

    e.asm.bind(epilogue);
    e.asm.emit("mov", &[sp(), x(29)]);
    e.asm.emit(
        "ldp",
        &[x(29), x(30), crate::compiler::assembler::mem_post(31, 16)],
    );
    e.asm.emit("ret", &[]);

    let ic_sites = e.ic_sites;
    let safepoints = e.safepoints;
    (
        e.asm.finish(),
        block_pcs,
        verified_entry_off,
        ic_sites,
        safepoints,
    )
}

fn emit_ir(e: &mut Emitter, ir: &Ir, next_in_order: Option<BlockId>) {
    match *ir {
        Ir::ConstSmi { dst, value } => {
            let d = e.dest_target(dst);
            emit_mov_imm64(e.asm, d, (value << 2) as u64);
            e.commit(dst, d);
        }
        Ir::ConstPool { dst, lit } => {
            let lit_id = e.literal_ids[lit.0 as usize];
            match e.assignment_of(dst) {
                Assignment::Reg(r) => e.asm.ldr_literal(xr(r), lit_id),
                Assignment::Spill(slot) => {
                    e.asm.ldr_literal(xr(16), lit_id);
                    e.asm.emit("str", &[x(16), spill_mem(slot)]);
                }
            }
        }
        Ir::Move { dst, src } => {
            let rs = e.resolve(src, 16);
            let d = e.dest_target(dst);
            if d.num != rs.num {
                e.asm.emit("mov", &[Operand::Reg(d), Operand::Reg(rs)]);
            }
            e.commit(dst, d);
        }
        Ir::Param { dst, index } => {
            let abi = xr(index);
            match e.assignment_of(dst) {
                Assignment::Reg(r) => {
                    if r != index {
                        e.asm.emit("mov", &[x(r), Operand::Reg(abi)]);
                    }
                }
                Assignment::Spill(slot) => {
                    e.asm.emit("str", &[Operand::Reg(abi), spill_mem(slot)]);
                }
            }
        }
        Ir::LoadKlass { .. } | Ir::GuardKlass { .. } => {
            unreachable!(
                "S11-only Ir variant; nothing constructs one yet (LoadKlass: needed by \
                          no D3.2 row; GuardKlass: S14+ inlining guards)"
            )
        }
        Ir::LoadField { dst, obj, byte_off } => e.emit_load_field(dst, obj, byte_off),
        Ir::StoreField {
            obj,
            byte_off,
            val,
            barrier,
        } => e.emit_store_field(obj, byte_off, val, barrier),
        Ir::SmiArith {
            op: SmiOp::Mul,
            dst,
            a,
            b,
            fail,
        } => e.emit_smi_mul(dst, a, b, fail),
        Ir::SmiArith {
            op,
            dst,
            a,
            b,
            fail,
        } => e.emit_smi_arith_simple(op, dst, a, b, fail),
        Ir::SmiCmpBr {
            op,
            a,
            b,
            if_true,
            if_false,
            fail,
        } => e.emit_smi_cmp_br(op, a, b, if_true, if_false, fail),
        Ir::SmiCmpVal {
            op,
            dst,
            a,
            b,
            fail,
        } => e.emit_smi_cmp_val(op, dst, a, b, fail),
        Ir::Jump { target } => {
            if next_in_order != Some(target) {
                let l = e.block_label(target);
                e.asm.b(l);
            }
        }
        Ir::BoolBr {
            val,
            if_true,
            if_false,
            not_bool,
        } => e.emit_bool_br(val, if_true, if_false, not_bool),
        Ir::CallSend {
            dst,
            site,
            ref args,
        } => e.emit_call_send(dst, site, args),
        Ir::CallRuntime {
            dst,
            stub,
            ref args,
        } => e.emit_call_runtime(dst, stub, args),
        Ir::Alloc {
            dst,
            klass,
            size_words,
        } => e.emit_alloc(dst, klass, size_words),
        Ir::Poll => e.emit_poll(),
        Ir::UncommonTrap { .. } => e.emit_uncommon_trap(),
        Ir::Ret { val } => {
            match e.assignment_of(val) {
                Assignment::Reg(r) => {
                    if r != 0 {
                        e.asm.emit("mov", &[x(0), x(r)]);
                    }
                }
                Assignment::Spill(slot) => {
                    e.asm.emit("ldr", &[x(0), spill_mem(slot)]);
                }
            }
            let epi = e.epilogue;
            e.asm.b(epi);
        }
        Ir::RetSelf => {
            match e.assignment_of(SELF_VREG) {
                Assignment::Reg(r) => {
                    if r != 0 {
                        e.asm.emit("mov", &[x(0), x(r)]);
                    }
                }
                Assignment::Spill(slot) => {
                    e.asm.emit("ldr", &[x(0), spill_mem(slot)]);
                }
            }
            let epi = e.epilogue;
            e.asm.b(epi);
        }
        Ir::Bailout {
            reason: BailoutReason::SmiOpFailed,
        } => {
            e.asm.emit("mov", &[x(0), imm(2)]); // BAILOUT sentinel (SPEC §2.1 reserved tag)
            let epi = e.epilogue;
            e.asm.b(epi);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::ir::{IrBlock, VRegInfo};
    use crate::compiler::jasm_assembler::JasmAssembler;
    use crate::compiler::regalloc;
    use crate::runtime::vm_state::{VmOptions, VmState};
    use crate::runtime::JitMode;

    /// Listing format: "<pc_off>  <hex_word>  <mnemonic> [<operands>]".
    fn mnemonic(l: &str) -> &str {
        l.split_whitespace().nth(2).unwrap_or("")
    }

    /// `pc_off` field is hex, zero-padded to 6 digits (`JasmAssembler::
    /// push_listing`'s own `{offset:06x}`) -- NOT decimal.
    fn line_pc_off(l: &str) -> u32 {
        u32::from_str_radix(
            l.split_whitespace()
                .next()
                .expect("listing line must start with a pc_off field"),
            16,
        )
        .expect("pc_off field must be a hex u32")
    }

    /// A real interned `SymbolOop` for tests that need a genuine selector.
    /// `emit.rs` sits outside `codecache`'s `#![allow(unsafe_code)]`
    /// exemption, so `nmethod.rs`'s own `from_oop_unchecked` shortcut isn't
    /// available here -- a bare throwaway `VmState` + real `intern` is.
    /// Callers only ever move/compare the returned `SymbolOop`'s raw bits
    /// (never dereference through it once this function returns), so the
    /// backing `VmState` going out of scope here is fine.
    fn test_selector(name: &[u8]) -> SymbolOop {
        let mut vm = VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            gc_stress_full_period: None,
            eden_kb: None,
            jit: JitMode::Off,
        });
        vm.universe.intern(name)
    }

    fn hand_method(blocks: Vec<IrBlock>, vregs: Vec<VRegInfo>, argc: u8) -> IrMethod {
        IrMethod {
            blocks,
            vregs,
            pool: Vec::new(),
            argc,
            ntemps: 0,
            safepoints: Vec::new(),
            true_lit: PoolLit(0),
            false_lit: PoolLit(0),
            nil_lit: PoolLit(0),
            mark_slots_lit: PoolLit(0),
            call_sites: Vec::new(),
            method_pool_ix: None,
        }
    }

    /// `(a + b) < 10 ? 1 : 0` — `run_ir_raw`'s own shape (`tests/it_tier1.rs`),
    /// four blocks with distinct `bci`s, reused here since it's already
    /// proven correct end to end and gives `pcdesc_block_starts` a real
    /// multi-block method to check without re-deriving one.
    fn branchy_method() -> IrMethod {
        let vregs: Vec<VRegInfo> = (0..7).map(|_| VRegInfo { is_oop: true }).collect();
        let block0 = IrBlock {
            id: BlockId(0),
            bci: 0,
            code: vec![
                Ir::Param {
                    dst: VReg(0),
                    index: 0,
                },
                Ir::Param {
                    dst: VReg(1),
                    index: 1,
                },
                Ir::Param {
                    dst: VReg(2),
                    index: 2,
                },
                Ir::SmiArith {
                    op: SmiOp::Add,
                    dst: VReg(3),
                    a: VReg(1),
                    b: VReg(2),
                    fail: BlockId(3),
                },
                Ir::ConstSmi {
                    dst: VReg(4),
                    value: 10,
                },
                Ir::SmiCmpBr {
                    op: CmpOp::Lt,
                    a: VReg(3),
                    b: VReg(4),
                    if_true: BlockId(1),
                    if_false: BlockId(2),
                    fail: BlockId(3),
                },
            ],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        let block1 = IrBlock {
            id: BlockId(1),
            bci: 10,
            code: vec![
                Ir::ConstSmi {
                    dst: VReg(5),
                    value: 1,
                },
                Ir::Ret { val: VReg(5) },
            ],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        let block2 = IrBlock {
            id: BlockId(2),
            bci: 20,
            code: vec![
                Ir::ConstSmi {
                    dst: VReg(6),
                    value: 0,
                },
                Ir::Ret { val: VReg(6) },
            ],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        let block3 = IrBlock {
            id: BlockId(3),
            bci: 30,
            code: vec![Ir::Bailout {
                reason: BailoutReason::SmiOpFailed,
            }],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        hand_method(vec![block0, block1, block2, block3], vregs, 2)
    }

    /// tests_s10.md: pcdescs sorted by pc_off, bcis match block bcis.
    #[test]
    fn pcdesc_block_starts() {
        let method = branchy_method();
        let ra = regalloc::regalloc(&method);
        let mut asm = JasmAssembler::new();
        let (_blob, block_pcs, _verified_entry_off, _ic_sites, _safepoints) =
            emit(&mut asm, &method, &ra, 0, 0, 0, None);

        assert_eq!(
            block_pcs.len(),
            4,
            "one BlockPc per block, including the bailout block"
        );
        let mut sorted = block_pcs.clone();
        sorted.sort_by_key(|bp| bp.pc_off);
        assert_eq!(
            block_pcs.iter().map(|bp| bp.pc_off).collect::<Vec<_>>(),
            sorted.iter().map(|bp| bp.pc_off).collect::<Vec<_>>(),
            "block_pcs must already be sorted by pc_off (emission-order == \
             address-order, since code only ever grows as blocks emit)"
        );
        // Every recorded bci matches the IR block it was taken from,
        // regardless of `block_order`'s own (possibly reshuffled, S10 step
        // 9's own bug) emission sequence.
        let bci_by_block: std::collections::HashMap<usize, usize> = block_pcs
            .iter()
            .enumerate()
            .map(|(i, bp)| (i, bp.bci))
            .collect();
        for (i, &bid) in ra.block_order.iter().enumerate() {
            let expected_bci = method.blocks[bid.0 as usize].bci;
            assert_eq!(
                bci_by_block[&i], expected_bci,
                "block_pcs[{i}] (block_order[{i}]={bid:?}) must report that block's own bci"
            );
        }
    }

    /// tests_s10.md: Mul listing contains asr/mul/smulh/cmp-asr63 exactly
    /// (P5 — `b.vs` does not exist for a 64x64->128 overflow check the way
    /// it does for add/sub, so this sequence is the only correct one).
    #[test]
    fn emit_smi_mul_overflow_seq() {
        let vregs: Vec<VRegInfo> = (0..4).map(|_| VRegInfo { is_oop: true }).collect();
        let block0 = IrBlock {
            id: BlockId(0),
            bci: 0,
            code: vec![
                Ir::Param {
                    dst: VReg(0),
                    index: 0,
                },
                Ir::Param {
                    dst: VReg(1),
                    index: 1,
                },
                Ir::Param {
                    dst: VReg(2),
                    index: 2,
                },
                Ir::SmiArith {
                    op: SmiOp::Mul,
                    dst: VReg(3),
                    a: VReg(1),
                    b: VReg(2),
                    fail: BlockId(1),
                },
                Ir::Ret { val: VReg(3) },
            ],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        let block1 = IrBlock {
            id: BlockId(1),
            bci: 10,
            code: vec![Ir::Bailout {
                reason: BailoutReason::SmiOpFailed,
            }],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        let method = hand_method(vec![block0, block1], vregs, 2);
        let ra = regalloc::regalloc(&method);
        let mut asm = JasmAssembler::new();
        let (blob, _pcs, _verified_entry_off, _ic_sites, _safepoints) =
            emit(&mut asm, &method, &ra, 0, 0, 0, None);

        let mnemonics: Vec<&str> = blob.listing.iter().map(|l| mnemonic(l)).collect();
        let asr_pos = mnemonics.iter().position(|&m| m == "asr");
        let mul_pos = mnemonics.iter().position(|&m| m == "mul");
        let smulh_pos = mnemonics.iter().position(|&m| m == "smulh");
        let cmp_pos = mnemonics.iter().position(|&m| m == "cmp");
        assert!(
            asr_pos.is_some() && mul_pos.is_some() && smulh_pos.is_some() && cmp_pos.is_some(),
            "Mul must emit asr, mul, smulh, and cmp -- got listing:\n{}",
            blob.listing.join("\n")
        );
        assert!(asr_pos < mul_pos, "asr (shift off the tag) before mul");
        assert!(
            mul_pos < smulh_pos,
            "mul (low 64 bits) before smulh (high 64 bits)"
        );
        assert!(smulh_pos < cmp_pos, "smulh before the overflow cmp");
        let cmp_line = &blob.listing[cmp_pos.unwrap()];
        assert!(
            cmp_line.contains("Asr") && cmp_line.contains("63"),
            "the overflow check must compare against the low word's own arithmetic-shift-right-63 \
             (sign extension), the only correct 64x64->128 overflow test -- got: {cmp_line}"
        );
        assert!(
            !mnemonics.contains(&"b.vs") && !blob.listing.iter().any(|l| l.contains("Vs")),
            "P5: b.vs is an add/sub-only overflow-flag branch, meaningless for mul's 128-bit \
             overflow check -- must not appear in a Mul sequence"
        );
    }

    /// tests_s10.md: instvar index boundary — `ldur` while `byte_off - 1`
    /// fits ldur's imm9 (±255), `sub x16,·,#1; ldr ·,[x16,#byte_off]`
    /// once it doesn't. `byte_off = BODY_OFFSET(16) + 8*index`, so the
    /// boundary is `index == 30` (biased=255, still ldur) vs `index == 31`
    /// (biased=263, sub+ldr) — computed from `oops::layout::BODY_OFFSET`
    /// directly rather than copying tests_s10.md's own example numbers,
    /// which were written before this offset's exact value was pinned
    /// down (verified cross-checked against `MemOop::body_ptr`'s own
    /// formula in this sprint's own commit history).
    #[test]
    fn emit_ldur_vs_untag_split() {
        let make = |index: u8| {
            let vregs: Vec<VRegInfo> = (0..2).map(|_| VRegInfo { is_oop: true }).collect();
            let byte_off = crate::oops::layout::BODY_OFFSET as i32 + 8 * index as i32;
            let block0 = IrBlock {
                id: BlockId(0),
                bci: 0,
                code: vec![
                    Ir::Param {
                        dst: VReg(0),
                        index: 0,
                    },
                    Ir::LoadField {
                        dst: VReg(1),
                        obj: VReg(0),
                        byte_off,
                    },
                    Ir::Ret { val: VReg(1) },
                ],
                entry_stack: Vec::new(),
                deopt_sites: Vec::new(),
            };
            hand_method(vec![block0], vregs, 1)
        };

        let mnemonic = |l: &str| l.split_whitespace().nth(2).unwrap_or("").to_string();

        let near = make(30);
        let ra = regalloc::regalloc(&near);
        let mut asm = JasmAssembler::new();
        let (blob, _pcs, _verified_entry_off, _ic_sites, _safepoints) =
            emit(&mut asm, &near, &ra, 0, 0, 0, None);
        let near_mnemonics: Vec<String> = blob.listing.iter().map(|l| mnemonic(l)).collect();
        assert!(
            near_mnemonics.iter().any(|m| m == "ldur"),
            "index 30 (biased offset 255, within ldur's imm9) must use ldur -- got:\n{}",
            blob.listing.join("\n")
        );
        assert!(
            !near_mnemonics.iter().any(|m| m == "sub"),
            "index 30 must not need the untag-then-ldr split -- got:\n{}",
            blob.listing.join("\n")
        );

        let far = make(31);
        let ra2 = regalloc::regalloc(&far);
        let mut asm2 = JasmAssembler::new();
        let (blob2, _pcs2, _verified_entry_off2, _ic_sites2, _safepoints) =
            emit(&mut asm2, &far, &ra2, 0, 0, 0, None);
        let mnemonics: Vec<String> = blob2.listing.iter().map(|l| mnemonic(l)).collect();
        assert!(
            mnemonics.iter().any(|m| m == "sub"),
            "index 31 (biased offset 263, past ldur's imm9) must untag via sub first -- got:\n{}",
            blob2.listing.join("\n")
        );
        assert!(
            mnemonics.iter().any(|m| m == "ldr"),
            "index 31 must follow the sub with a plain ldr (not ldur) at the untagged base"
        );
        assert!(
            !blob2.listing.iter().any(|l| l.contains("ldur")),
            "index 31 must NOT use ldur at all -- got:\n{}",
            blob2.listing.join("\n")
        );
    }

    /// tests_s11.md's `barrier_emitted_conditions`: `StoreField{barrier:
    /// true}`'s listing contains the card-marking sequence (P7); `barrier:
    /// false` emits only the bare store, no barrier at all.
    #[test]
    fn barrier_emitted_conditions() {
        let make = |barrier: bool| {
            let vregs: Vec<VRegInfo> = (0..2).map(|_| VRegInfo { is_oop: true }).collect();
            let block0 = IrBlock {
                id: BlockId(0),
                bci: 0,
                code: vec![
                    Ir::Param {
                        dst: VReg(0),
                        index: 0,
                    },
                    Ir::Param {
                        dst: VReg(1),
                        index: 1,
                    },
                    Ir::StoreField {
                        obj: VReg(0),
                        byte_off: crate::oops::layout::BODY_OFFSET as i32,
                        val: VReg(1),
                        barrier,
                    },
                    Ir::RetSelf,
                ],
                entry_stack: Vec::new(),
                deopt_sites: Vec::new(),
            };
            hand_method(vec![block0], vregs, 2)
        };

        let mnemonic = |l: &str| l.split_whitespace().nth(2).unwrap_or("").to_string();

        let with_barrier = make(true);
        let ra = regalloc::regalloc(&with_barrier);
        let mut asm = JasmAssembler::new();
        let (blob, _pcs, _verified_entry_off, _ic_sites, _safepoints) =
            emit(&mut asm, &with_barrier, &ra, 0, 0, 0, None);
        let mnemonics: Vec<String> = blob.listing.iter().map(|l| mnemonic(l)).collect();
        assert!(
            mnemonics.iter().any(|m| m == "stur" || m == "str"),
            "barrier:true must still emit the store itself -- got:\n{}",
            blob.listing.join("\n")
        );
        assert!(
            mnemonics.iter().any(|m| m == "strb"),
            "barrier:true must emit the card-dirtying strb -- got:\n{}",
            blob.listing.join("\n")
        );
        assert!(
            mnemonics.iter().any(|m| m == "lsr"),
            "barrier:true must emit the card-index shift -- got:\n{}",
            blob.listing.join("\n")
        );

        let without_barrier = make(false);
        let ra2 = regalloc::regalloc(&without_barrier);
        let mut asm2 = JasmAssembler::new();
        let (blob2, _pcs2, _verified_entry_off2, _ic_sites2, _safepoints) =
            emit(&mut asm2, &without_barrier, &ra2, 0, 0, 0, None);
        let mnemonics2: Vec<String> = blob2.listing.iter().map(|l| mnemonic(l)).collect();
        assert!(
            mnemonics2.iter().any(|m| m == "stur" || m == "str"),
            "barrier:false must still emit the store itself -- got:\n{}",
            blob2.listing.join("\n")
        );
        assert!(
            !mnemonics2.iter().any(|m| m == "strb"),
            "barrier:false must NOT emit any card-dirtying strb -- got:\n{}",
            blob2.listing.join("\n")
        );
    }

    /// S11 D7: `alloc_fast_path_layout` — pins the inline-alloc sequence's
    /// shape AND its register discipline (the ABI-correctness the whole
    /// step stands on). A single 4-word (2 header + 2 body) `Slots` alloc,
    /// dst kept live by a `Ret`.
    #[test]
    fn alloc_fast_path_layout() {
        use crate::compiler::assembler::RelocKind;
        use crate::compiler::ir::PoolEntry;
        // pool[0]=nil, [1]=mark(raw imm), [2]=klass oop.
        let method = IrMethod {
            blocks: vec![IrBlock {
                id: BlockId(0),
                bci: 0,
                code: vec![
                    Ir::Alloc {
                        dst: VReg(0),
                        klass: PoolLit(2),
                        size_words: 4,
                    },
                    Ir::Ret { val: VReg(0) },
                ],
                entry_stack: Vec::new(),
                deopt_sites: Vec::new(),
            }],
            vregs: vec![VRegInfo { is_oop: true }],
            pool: vec![
                PoolEntry {
                    value: 0x1111,
                    kind: Some(RelocKind::Oop),
                },
                PoolEntry {
                    value: 0x2222,
                    kind: None,
                },
                PoolEntry {
                    value: 0x3333,
                    kind: Some(RelocKind::Oop),
                },
            ],
            argc: 0,
            ntemps: 0,
            safepoints: Vec::new(),
            true_lit: PoolLit(0),
            false_lit: PoolLit(0),
            nil_lit: PoolLit(0),
            mark_slots_lit: PoolLit(1),
            call_sites: Vec::new(),
            method_pool_ix: None,
        };
        let ra = regalloc::regalloc(&method);
        let mut asm = JasmAssembler::new();
        let (blob, _pcs, _ve, _ic, _safepoints) = emit(&mut asm, &method, &ra, 0, 0, 0xAABB, None);
        let listing = blob.listing.join("\n");
        let mnemonic = |l: &str| l.split_whitespace().nth(2).unwrap_or("").to_string();
        let mnemonics: Vec<String> = blob.listing.iter().map(|l| mnemonic(l)).collect();

        // Fast path (S12 step 7, single-source-of-truth eden): load the
        // eden.top ADDRESS from the reg block, load the live top THROUGH
        // it, bump, load eden_end (clobbering the obj register), bounds
        // cmp, conditional slow branch, commit the bump back THROUGH the
        // address, recover obj arithmetically. Pinned as an exact
        // contiguous mnemonic window — the pre-S12 direct-value sequence
        // (ldr,add,ldr,cmp,b.,str — no second leading ldr, no recovering
        // sub) must NOT silently reappear, since it reads a copy that a
        // GC under this very frame would leave stale.
        let fast_path_shape = ["ldr", "ldr", "add", "ldr", "cmp", "b.", "str", "sub"];
        let window_found = mnemonics.windows(fast_path_shape.len()).any(|w| {
            w.iter().zip(fast_path_shape.iter()).all(|(m, want)| {
                if *want == "b." {
                    m.starts_with("b.")
                } else {
                    m == want
                }
            })
        });
        assert!(
            window_found,
            "the through-the-pointer fast-path shape (ldr addr, ldr top, add, ldr end, cmp, \
             b.cond, str commit, sub recover) must appear contiguously:\n{listing}"
        );
        assert!(
            mnemonics.iter().filter(|m| *m == "str").count() >= 4,
            "commit + mark + klass + 2 body stores (>=4 str):\n{listing}"
        );
        assert!(
            mnemonics.iter().any(|m| m == "blr"),
            "slow-path call:\n{listing}"
        );
        // ABI: the object base must NOT be x16 (dest_target's spilled-dst
        // reg) and must be a callee-saved scratch (x19); the eden.top
        // ADDRESS rides in x20 (also callee-saved, live until the commit
        // str). Assert both appear, and x18 (Darwin platform reg) never.
        assert!(
            listing.contains("num: 19"),
            "obj base must be x19 (callee-saved scratch), never x16/x18:\n{listing}"
        );
        assert!(
            listing.contains("num: 20"),
            "the eden.top address must ride in x20:\n{listing}"
        );
        assert!(
            !listing.contains("num: 18"),
            "x18 is the Darwin platform register -- must never appear:\n{listing}"
        );
    }

    /// D2: `entry`'s klass-guard prologue is exactly the bytes before
    /// `verified_entry_off` -- checked structurally (mnemonic shape), not
    /// against a fixed byte count (both the smi and heap paths are always
    /// emitted, so the guard's encoded length doesn't depend on which
    /// branch a real receiver would take).
    #[test]
    fn entry_guard_smi_and_heap() {
        let vregs = vec![VRegInfo { is_oop: true }];
        let block0 = IrBlock {
            id: BlockId(0),
            bci: 0,
            code: vec![
                Ir::Param {
                    dst: VReg(0),
                    index: 0,
                },
                Ir::RetSelf,
            ],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        let method = hand_method(vec![block0], vregs, 1);
        let ra = regalloc::regalloc(&method);
        let mut asm = JasmAssembler::new();
        let guard = EntryGuard {
            smi_klass_bits: 0x1000,
            key_klass_bits: 0x2000,
            resolve_addr: 0x3000,
        };
        let (blob, _pcs, verified_entry_off, _ic_sites, _safepoints) =
            emit(&mut asm, &method, &ra, 0, 0, 0, Some(guard));

        // `verified_entry_off` must land exactly on the S10-era prologue's
        // own first instruction (`stp x29,x30,...`) -- found empirically in
        // the listing, not assumed from a guessed byte count.
        let stp_line = blob
            .listing
            .iter()
            .find(|l| mnemonic(l) == "stp")
            .unwrap_or_else(|| panic!("no stp in listing:\n{}", blob.listing.join("\n")));
        assert_eq!(
            verified_entry_off,
            line_pc_off(stp_line),
            "verified_entry_off must be exactly the guard's own first prologue instruction \
             (stp x29,x30,...) -- got listing:\n{}",
            blob.listing.join("\n")
        );

        let guard_lines: Vec<&str> = blob
            .listing
            .iter()
            .filter(|l| line_pc_off(l) < verified_entry_off)
            .map(|s| s.as_str())
            .collect();
        let guard_mnemonics: Vec<&str> = guard_lines.iter().map(|l| mnemonic(l)).collect();
        assert_eq!(
            guard_mnemonics.first(),
            Some(&"tst"),
            "guard must open by testing the receiver's tag bits -- got:\n{}",
            guard_lines.join("\n")
        );
        assert!(
            guard_mnemonics.contains(&"ldur"),
            "guard's heap case must load the klass word via ldur (unscaled, MEM_TAG bias) -- \
             got:\n{}",
            guard_lines.join("\n")
        );
        assert!(
            guard_mnemonics.iter().filter(|&&m| m == "ldr").count() >= 3,
            "guard must load smi_klass, key_klass, and resolve_addr as literals (>=3 ldr) -- \
             got:\n{}",
            guard_lines.join("\n")
        );
        assert!(
            guard_mnemonics.contains(&"cmp"),
            "guard must compare the actual klass against key_klass -- got:\n{}",
            guard_lines.join("\n")
        );
        assert!(
            guard_mnemonics.contains(&"br"),
            "guard's miss path must reach stub_resolve via an indirect br -- got:\n{}",
            guard_lines.join("\n")
        );
        assert!(
            !guard_mnemonics.contains(&"blr") && !guard_mnemonics.contains(&"bl"),
            "guard must NEVER touch x30 (blr/bl) -- D4.1's invariant that the send site's own \
             return address survives a guard miss untouched -- got:\n{}",
            guard_lines.join("\n")
        );
    }

    /// `tests_s12.md`'s `pool_relocs_after_literal_off` (P4): every
    /// `Oop`/`KeyKlassOop` reloc must sit at or past `literal_off` — GC
    /// rewrites ONLY reloc-recorded pool words; one embedded via
    /// movz/movk or adrp/add INSIDE the instruction stream (before
    /// `literal_off`) would need re-encoding + an icache flush mid-
    /// collection, forbidden by S9 P5. Reuses `entry_guard_smi_and_heap`'s
    /// own setup (its `EntryGuard` embeds one `Oop` reloc, smi_klass, and
    /// one `KeyKlassOop` reloc, key_klass) plus a body with its own real
    /// oop literal (`ConstPool`), so this covers BOTH the guard's own
    /// literals and an ordinary body literal in one method.
    #[test]
    fn pool_relocs_after_literal_off() {
        let vregs = vec![VRegInfo { is_oop: true }];
        let block0 = IrBlock {
            id: BlockId(0),
            bci: 0,
            code: vec![
                Ir::ConstPool {
                    dst: VReg(0),
                    lit: PoolLit(0),
                },
                Ir::Ret { val: VReg(0) },
            ],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        let mut method = hand_method(vec![block0], vregs, 1);
        method.pool = vec![crate::compiler::ir::PoolEntry {
            value: 0x4000,
            kind: Some(RelocKind::Oop),
        }];
        let ra = regalloc::regalloc(&method);
        let mut asm = JasmAssembler::new();
        let guard = EntryGuard {
            smi_klass_bits: 0x1000,
            key_klass_bits: 0x2000,
            resolve_addr: 0x3000,
        };
        let (blob, _pcs, _verified_entry_off, _ic_sites, _safepoints) =
            emit(&mut asm, &method, &ra, 0, 0, 0, Some(guard));

        let oop_relocs: Vec<&crate::compiler::assembler::Reloc> = blob
            .relocs
            .iter()
            .filter(|r| matches!(r.kind, RelocKind::Oop | RelocKind::KeyKlassOop))
            .collect();
        assert!(
            oop_relocs.len() >= 3,
            "expected smi_klass (Oop) + key_klass (KeyKlassOop) + the body's own ConstPool \
             (Oop), got {}: {:?}",
            oop_relocs.len(),
            blob.relocs
        );
        for r in &oop_relocs {
            assert!(
                r.offset >= blob.literal_off,
                "reloc at offset {} (kind {:?}) is BEFORE literal_off {} -- an oop embedded \
                 inside the instruction stream itself, not the pool, which GC must never rewrite \
                 in place",
                r.offset,
                r.kind,
                blob.literal_off
            );
        }
    }

    /// D3/D4: one `EmittedIcSite` per `Ir::CallSend`, in encounter order.
    /// This is also the test that originally caught `emit_call_send`'s
    /// false "every arg is spilled" assumption (see that method's own doc
    /// comment): `foo`'s two args are fresh off `Param`, each used exactly
    /// once (at this very call) and so legitimately register-assigned, not
    /// spilled, per `crosses_safepoint`'s documented semantics -- building
    /// a real method and regalloc'ing it for real is what makes that case
    /// actually occur, rather than merely being reasoned about.
    #[test]
    fn ic_site_recorded_per_send() {
        let vregs: Vec<VRegInfo> = (0..6).map(|_| VRegInfo { is_oop: true }).collect();
        let block0 = IrBlock {
            id: BlockId(0),
            bci: 0,
            code: vec![
                Ir::Param {
                    dst: VReg(0),
                    index: 0,
                },
                Ir::Param {
                    dst: VReg(1),
                    index: 1,
                },
                Ir::Param {
                    dst: VReg(2),
                    index: 2,
                },
                Ir::CallSend {
                    dst: VReg(3),
                    site: 0,
                    args: vec![VReg(0), VReg(1)],
                },
                Ir::CallSend {
                    dst: VReg(4),
                    site: 1,
                    args: vec![VReg(3), VReg(2)],
                },
                Ir::CallSend {
                    dst: VReg(5),
                    site: 2,
                    args: vec![VReg(2), VReg(4)],
                },
                Ir::Ret { val: VReg(5) },
            ],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        let method = IrMethod {
            blocks: vec![block0],
            vregs,
            pool: Vec::new(),
            argc: 3,
            ntemps: 0,
            safepoints: Vec::new(),
            true_lit: PoolLit(0),
            false_lit: PoolLit(0),
            nil_lit: PoolLit(0),
            mark_slots_lit: PoolLit(0),
            call_sites: vec![
                CallSiteInfo {
                    selector: test_selector(b"foo"),
                    argc: 2,
                    static_klass: None,
                },
                CallSiteInfo {
                    selector: test_selector(b"bar"),
                    argc: 2,
                    static_klass: None,
                },
                CallSiteInfo {
                    selector: test_selector(b"baz"),
                    argc: 2,
                    static_klass: None,
                },
            ],
            method_pool_ix: None,
        };
        let ra = regalloc::regalloc(&method);
        let mut asm = JasmAssembler::new();
        let (blob, _pcs, _verified_entry_off, ic_sites, _safepoints) =
            emit(&mut asm, &method, &ra, 0, 0, 0, None);

        assert_eq!(
            ic_sites.len(),
            3,
            "one EmittedIcSite per CallSend -- got {ic_sites:?}"
        );

        for (i, site) in ic_sites.iter().enumerate() {
            let expected = method.call_sites[i].selector;
            assert_eq!(
                site.selector.oop().raw(),
                expected.oop().raw(),
                "ic_sites[{i}] selector must match call_sites[{i}]'s own"
            );
            assert_eq!(site.argc, 2, "ic_sites[{i}] must carry its site's own argc");
            let line = blob
                .listing
                .iter()
                .find(|l| line_pc_off(l) == site.off)
                .unwrap_or_else(|| {
                    panic!(
                        "ic_sites[{i}].off {} has no matching listing line:\n{}",
                        site.off,
                        blob.listing.join("\n")
                    )
                });
            assert_eq!(
                mnemonic(line),
                "bl",
                "ic_sites[{i}].off must point at the CallSend's own bl -- got: {line}"
            );
        }

        assert!(
            ic_sites[0].off < ic_sites[1].off && ic_sites[1].off < ic_sites[2].off,
            "ic_sites must be in encounter order with strictly ascending offsets -- got {ic_sites:?}"
        );
    }
}
