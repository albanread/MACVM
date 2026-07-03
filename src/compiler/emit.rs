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
use crate::compiler::ir::{BailoutReason, BlockId, CmpOp, Ir, IrMethod, PoolLit, SmiOp, VReg};
use crate::compiler::regalloc::{Assignment, RegallocResult};
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

struct Emitter<'a> {
    asm: &'a mut dyn Assembler,
    assignment: Vec<Option<Assignment>>,
    literal_ids: Vec<LiteralId>,
    labels: Vec<Label>,
    epilogue: Label,
    true_lit: PoolLit,
    false_lit: PoolLit,
    /// Absolute address of the once-published `stub_poll` (`codecache::
    /// stubs`) — a runtime value `Poll`'s far call embeds as a pool
    /// constant, since `bl`'s ±128 MB range can't reach it directly and
    /// its address isn't known until stub publish time (before any real
    /// method is compiled, but still not a compile-time constant).
    stub_poll_lit: LiteralId,
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
        self.asm.bind(skip);
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

/// D3.6/D5: emit `method`'s whole body — prologue, every block in
/// `regalloc`'s linearized order, shared epilogue — and finish into a
/// `CodeBlob`. `stub_poll_addr` is `stub_poll`'s already-published address
/// (irrelevant if `method` has no `Poll` op at all, but always required by
/// this signature — S10's own methods are simple enough that threading an
/// `Option` through for the rare case wasn't worth it).
pub fn emit(
    asm: &mut dyn Assembler,
    method: &IrMethod,
    regalloc: &RegallocResult,
    stub_poll_addr: u64,
) -> (CodeBlob, Vec<BlockPc>) {
    let literal_ids = intern_pool(asm, method);

    let mut assignment: Vec<Option<Assignment>> = vec![None; method.vregs.len()];
    for iv in &regalloc.intervals {
        assignment[iv.vreg.0 as usize] = iv.assignment;
    }

    let labels: Vec<Label> = (0..method.blocks.len()).map(|_| asm.new_label()).collect();
    let epilogue = asm.new_label();
    let stub_poll_lit = asm.literal_u64(stub_poll_addr, Some(RelocKind::RuntimeAddr));

    let mut e = Emitter {
        asm,
        assignment,
        literal_ids,
        labels,
        epilogue,
        true_lit: method.true_lit,
        false_lit: method.false_lit,
        stub_poll_lit,
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

        let next_in_order = regalloc.block_order.get(i + 1).copied();
        for ir in &block.code {
            emit_ir(&mut e, ir, next_in_order);
        }
    }

    e.asm.bind(epilogue);
    e.asm.emit("mov", &[sp(), x(29)]);
    e.asm.emit(
        "ldp",
        &[x(29), x(30), crate::compiler::assembler::mem_post(31, 16)],
    );
    e.asm.emit("ret", &[]);

    (e.asm.finish(), block_pcs)
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
        Ir::LoadKlass { .. } | Ir::StoreField { .. } | Ir::GuardKlass { .. } => {
            unreachable!("S11-only Ir variant; S10's convert() never constructs one")
        }
        Ir::LoadField { dst, obj, byte_off } => e.emit_load_field(dst, obj, byte_off),
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
        Ir::CallSend { .. } | Ir::CallRuntime { .. } | Ir::Alloc { .. } => {
            unreachable!("S11-only Ir variant; S10's convert() never constructs one")
        }
        Ir::Poll => e.emit_poll(),
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
        };
        let block3 = IrBlock {
            id: BlockId(3),
            bci: 30,
            code: vec![Ir::Bailout {
                reason: BailoutReason::SmiOpFailed,
            }],
            entry_stack: Vec::new(),
        };
        hand_method(vec![block0, block1, block2, block3], vregs, 2)
    }

    /// tests_s10.md: pcdescs sorted by pc_off, bcis match block bcis.
    #[test]
    fn pcdesc_block_starts() {
        let method = branchy_method();
        let ra = regalloc::regalloc(&method);
        let mut asm = JasmAssembler::new();
        let (_blob, block_pcs) = emit(&mut asm, &method, &ra, 0);

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
        };
        let block1 = IrBlock {
            id: BlockId(1),
            bci: 10,
            code: vec![Ir::Bailout {
                reason: BailoutReason::SmiOpFailed,
            }],
            entry_stack: Vec::new(),
        };
        let method = hand_method(vec![block0, block1], vregs, 2);
        let ra = regalloc::regalloc(&method);
        let mut asm = JasmAssembler::new();
        let (blob, _pcs) = emit(&mut asm, &method, &ra, 0);

        fn mnemonic(l: &str) -> &str {
            // Listing format: "<pc_off>  <hex_word>  <mnemonic> [<operands>]".
            l.split_whitespace().nth(2).unwrap_or("")
        }
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
            };
            hand_method(vec![block0], vregs, 1)
        };

        let mnemonic = |l: &str| l.split_whitespace().nth(2).unwrap_or("").to_string();

        let near = make(30);
        let ra = regalloc::regalloc(&near);
        let mut asm = JasmAssembler::new();
        let (blob, _pcs) = emit(&mut asm, &near, &ra, 0);
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
        let (blob2, _pcs2) = emit(&mut asm2, &far, &ra2, 0);
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
}
