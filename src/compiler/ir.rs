//! SSA-lite IR + bytecode-to-IR conversion (`sprint_s10_detail.md` D2, D3.2,
//! D3.3). Consumes a [`crate::compiler::decode::Cfg`] and a method's real
//! bytecode/literals/ICs and produces an [`IrMethod`] already shaped for
//! `regalloc.rs`'s linear scan and `emit.rs`'s direct per-op AArch64
//! sequences — D3.3's own call: "the SSA-lite IR is machine-shaped and
//! emit walks it directly," no separate lowering stage in v1.
//!
//! "SSA-lite": every value-producing bytecode effect gets its own fresh
//! [`VReg`], but the per-method "unified temp/arg slot" vregs
//! (`self_vreg`/`temp_vregs`) allow multiple defs (a real `store_temp`
//! reassigns the SAME vreg) — not textbook SSA, cheap enough for a tier-1
//! compiler that never runs SSA-only optimizations.

use std::collections::HashMap;

use crate::bytecode::opcode::{decode_at, Instr};
use crate::compiler::assembler::RelocKind;
use crate::compiler::decode::{BlockIndex, Cfg, Terminator};
use crate::interpreter::ic::InterpreterIc;
use crate::oops::layout::BODY_OFFSET;
use crate::oops::wrappers::MethodOop;
use crate::runtime::vm_state::VmState;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct VReg(pub u32);
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BlockId(pub u32);
/// Id into an [`IrMethod`]'s own `pool` — a compiler-stage literal table,
/// distinct from (and translated into, at emit time) the vendored
/// assembler's own `LiteralId`/literal pool (S9).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PoolLit(pub u32);

/// Identifies a runtime stub `Ir::CallRuntime` calls into — provisional
/// here; owned for real by `codecache::stubs` once S11 actually emits
/// `CallRuntime`. S10 never constructs one (`stub_poll`, the one S10 stub,
/// is called directly from emitted `Poll` sequences, not through this Ir).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StubId(pub u32);

/// A position `regalloc.rs`'s `linearize` (S11+) records as needing an
/// oop map. Always empty in S10 (D1: compiled code never allocates or
/// calls Rust, so it has no safepoints) — the field exists now so S11 only
/// adds population, not plumbing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SafepointId(pub u32);

pub struct VRegInfo {
    pub is_oop: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmiOp {
    Add,
    Sub,
    Mul,
    And,
    Or,
    Xor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BailoutReason {
    SmiOpFailed,
}

/// One compiler-stage literal-pool entry (D2) — `emit.rs` walks these at
/// `finish` time and calls `Assembler::literal_u64(value, kind)` for each,
/// getting back the vendored assembler's own `LiteralId`.
#[derive(Clone, Copy, Debug)]
pub struct PoolEntry {
    pub value: u64,
    pub kind: Option<RelocKind>,
}

/// One IR instruction. 19 variants total; `CallSend`/`CallRuntime`/`Alloc`/
/// `GuardKlass`/`StoreField` (and `LoadKlass`, needed by none of D3.2's
/// translation rows or D5.3's emit table either) are defined here for the
/// C+D-phase enum shape but never *constructed* by S10's `convert` —
/// they're S11's to emit.
pub enum Ir {
    ConstSmi {
        dst: VReg,
        value: i64,
    },
    ConstPool {
        dst: VReg,
        lit: PoolLit,
    },
    Move {
        dst: VReg,
        src: VReg,
    },
    /// `index` 0 = receiver, 1.. = args (SPEC §5.1 unified numbering).
    /// Entry block only.
    Param {
        dst: VReg,
        index: u8,
    },
    LoadKlass {
        dst: VReg,
        obj: VReg,
    },
    /// `byte_off` is untagged-address-relative; `emit.rs` applies the −1
    /// tagged-pointer bias itself (D5.3 P4).
    LoadField {
        dst: VReg,
        obj: VReg,
        byte_off: i32,
    },
    StoreField {
        obj: VReg,
        byte_off: i32,
        val: VReg,
        barrier: bool,
    },
    /// Tag check + op + overflow check fused into one emit sequence
    /// (D5.3); `fail` is always the method's one shared bailout block.
    SmiArith {
        op: SmiOp,
        dst: VReg,
        a: VReg,
        b: VReg,
        fail: BlockId,
    },
    /// The fusion peephole (D3.2): a comparison send whose ONLY consumer
    /// is an immediately-following `br_true_fwd`/`br_false_fwd`.
    SmiCmpBr {
        op: CmpOp,
        a: VReg,
        b: VReg,
        if_true: BlockId,
        if_false: BlockId,
        fail: BlockId,
    },
    /// A comparison send NOT fused with a branch — materializes
    /// `true`/`false`.
    SmiCmpVal {
        op: CmpOp,
        dst: VReg,
        a: VReg,
        b: VReg,
        fail: BlockId,
    },
    Jump {
        target: BlockId,
    },
    BoolBr {
        val: VReg,
        if_true: BlockId,
        if_false: BlockId,
        not_bool: BlockId,
    },
    GuardKlass {
        obj: VReg,
        expect: PoolLit,
        fail: BlockId,
    },
    CallSend {
        dst: VReg,
        site: u16,
        args: Vec<VReg>,
    },
    CallRuntime {
        dst: Option<VReg>,
        stub: StubId,
        args: Vec<VReg>,
    },
    Alloc {
        dst: VReg,
        klass: PoolLit,
        size_words: u32,
        slow: BlockId,
    },
    /// Loop back-edge flag check (D5.3); reads `VmRegBlock::poll_flag`.
    Poll,
    Ret {
        val: VReg,
    },
    RetSelf,
    Bailout {
        reason: BailoutReason,
    },
}

impl Ir {
    /// Every vreg this op reads. Regalloc liveness and the (future) S12
    /// verifier both consume this — a new variant that forgets an operand
    /// here is caught in one place (D2).
    pub fn uses(&self, mut f: impl FnMut(VReg)) {
        match self {
            Ir::Move { src, .. } => f(*src),
            Ir::LoadKlass { obj, .. } => f(*obj),
            Ir::LoadField { obj, .. } => f(*obj),
            Ir::StoreField { obj, val, .. } => {
                f(*obj);
                f(*val);
            }
            Ir::SmiArith { a, b, .. } | Ir::SmiCmpBr { a, b, .. } | Ir::SmiCmpVal { a, b, .. } => {
                f(*a);
                f(*b);
            }
            Ir::BoolBr { val, .. } => f(*val),
            Ir::GuardKlass { obj, .. } => f(*obj),
            Ir::CallSend { args, .. } | Ir::CallRuntime { args, .. } => {
                for &v in args {
                    f(v);
                }
            }
            Ir::Ret { val } => f(*val),
            Ir::ConstSmi { .. }
            | Ir::ConstPool { .. }
            | Ir::Param { .. }
            | Ir::Jump { .. }
            | Ir::Alloc { .. }
            | Ir::Poll
            | Ir::RetSelf
            | Ir::Bailout { .. } => {}
        }
    }

    pub fn defs(&self, mut f: impl FnMut(VReg)) {
        match self {
            Ir::ConstSmi { dst, .. }
            | Ir::ConstPool { dst, .. }
            | Ir::Move { dst, .. }
            | Ir::Param { dst, .. }
            | Ir::LoadKlass { dst, .. }
            | Ir::LoadField { dst, .. }
            | Ir::SmiArith { dst, .. }
            | Ir::SmiCmpVal { dst, .. }
            | Ir::CallSend { dst, .. }
            | Ir::Alloc { dst, .. } => f(*dst),
            Ir::CallRuntime { dst: Some(d), .. } => f(*d),
            Ir::StoreField { .. }
            | Ir::SmiCmpBr { .. }
            | Ir::Jump { .. }
            | Ir::BoolBr { .. }
            | Ir::GuardKlass { .. }
            | Ir::CallRuntime { dst: None, .. }
            | Ir::Poll
            | Ir::Ret { .. }
            | Ir::RetSelf
            | Ir::Bailout { .. } => {}
        }
    }
}

pub struct IrBlock {
    pub id: BlockId,
    /// First bytecode index this block covers — a `PcDesc` seed. The one
    /// synthetic block (the shared bailout target) uses 0: a bailout
    /// restarts interpretation from bci 0 (D1), so "back to the start" is
    /// the closest thing it has to a real bci.
    pub bci: usize,
    pub code: Vec<Ir>,
    /// Merge vregs (D3.3) this block's predecessor(s) must feed on entry.
    pub entry_stack: Vec<VReg>,
}

pub struct IrMethod {
    pub blocks: Vec<IrBlock>,
    pub vregs: Vec<VRegInfo>,
    pub pool: Vec<PoolEntry>,
    pub argc: u8,
    pub ntemps: u8,
    pub safepoints: Vec<SafepointId>,
    /// Always present (`convert` interns them unconditionally) — `emit.rs`
    /// has no `VmState` of its own to intern `true_obj`/`false_obj` on
    /// demand for `SmiCmpVal`/`BoolBr`'s sequences (D5.3), so it reads
    /// these instead of relying on a fixed pool-index convention.
    pub true_lit: PoolLit,
    pub false_lit: PoolLit,
}

// ── conversion ───────────────────────────────────────────────────────────

/// Net operand-stack effect of one decoded instruction — the raw material
/// for `compute_entry_depths`' worklist. `Send`'s effect depends on the
/// site's own IC (`argc` isn't in the bytecode operand, only the IC index
/// is), so this needs `method` to look it up.
fn instr_stack_delta(method: MethodOop, instr: &Instr) -> i32 {
    match instr {
        Instr::PushSelf
        | Instr::PushNil
        | Instr::PushTrue
        | Instr::PushFalse
        | Instr::PushSmi(_)
        | Instr::PushLiteral(_)
        | Instr::PushTemp(_)
        | Instr::PushInstvar(_)
        | Instr::PushGlobal(_)
        | Instr::Dup
        | Instr::PushCtxTemp { .. }
        | Instr::PushClosure { .. } => 1,
        Instr::StoreTemp(_) => 0,
        Instr::StoreTempPop(_)
        | Instr::StoreInstvarPop(_)
        | Instr::StoreGlobalPop(_)
        | Instr::StoreCtxTempPop { .. }
        | Instr::Pop => -1,
        Instr::Send { ic, .. } => -(InterpreterIc::at(method, *ic).argc() as i32),
        Instr::JumpFwd(_) | Instr::JumpBack(_) => 0,
        Instr::BrTrueFwd(_) | Instr::BrFalseFwd(_) => -1,
        Instr::ReturnTos => -1,
        Instr::ReturnSelf | Instr::BlockReturnTos | Instr::NlrTos => 0,
    }
}

/// D3.2's worklist pass: `entry_depth[0] = 0`; every successor either
/// receives the propagated depth (first visit) or must already agree with
/// it (frontend-emitted bytecode guarantees this — an `assert!`, not a
/// `debug_assert!`, since a mismatch means malformed/untrusted bytecode
/// reached the compiler, not merely an internal invariant). Unreached
/// blocks (`unreachable_after_return`'s dead tail) are left at depth 0 —
/// harmless, since nothing ever reads an unreached block's entry_stack.
fn compute_entry_depths(method: MethodOop, cfg: &Cfg) -> Vec<i32> {
    let mut entry_depth: Vec<Option<i32>> = vec![None; cfg.blocks.len()];
    entry_depth[0] = Some(0);
    let mut worklist = vec![0usize];

    fn record(
        entry_depth: &mut [Option<i32>],
        worklist: &mut Vec<BlockIndex>,
        b: BlockIndex,
        depth: i32,
    ) {
        match entry_depth[b] {
            Some(d) => assert_eq!(
                d, depth,
                "compute_entry_depths: block {b} reached at two different simulated stack \
                 depths ({d} vs {depth}) -- malformed bytecode (frontend is trusted to keep \
                 depth consistent across every merge)"
            ),
            None => {
                entry_depth[b] = Some(depth);
                worklist.push(b);
            }
        }
    }

    while let Some(b) = worklist.pop() {
        let block = &cfg.blocks[b];
        let mut depth = entry_depth[b].expect("worklist entries always have a known depth");
        let mut bci = block.bci_start;
        while bci < block.bci_end {
            let (instr, next) = decode_at(method, bci);
            depth += instr_stack_delta(method, &instr);
            bci = next;
        }
        debug_assert!(
            depth >= 0,
            "compute_entry_depths: block {b} simulated to a negative depth ({depth})"
        );
        match block.terminator {
            Terminator::Fallthrough(t) | Terminator::Jump { target: t, .. } => {
                record(&mut entry_depth, &mut worklist, t, depth);
            }
            Terminator::Branch { if_true, if_false } => {
                record(&mut entry_depth, &mut worklist, if_true, depth);
                record(&mut entry_depth, &mut worklist, if_false, depth);
            }
            Terminator::Return => {}
        }
    }
    entry_depth.into_iter().map(|d| d.unwrap_or(0)).collect()
}

/// D3.2's merge rule, evaluated once every block's `entry_depth` is known.
#[derive(Clone, Copy)]
enum EntryStackSource {
    /// `entry_depth == 0`: `entry_stack = []`, unconditionally.
    Empty,
    /// Exactly one predecessor, reached by a non-backward edge: inherits
    /// that predecessor's exit stack verbatim (same vregs, no `Move`s).
    Inherit(BlockIndex),
    /// A join, or a loop header with nonzero entry depth: owns fresh
    /// merge vregs; every predecessor `Move`s into them.
    Merge,
}

fn entry_stack_sources(cfg: &Cfg, entry_depth: &[i32]) -> Vec<EntryStackSource> {
    let mut predecessors: Vec<Vec<(BlockIndex, bool)>> = vec![Vec::new(); cfg.blocks.len()];
    for (i, block) in cfg.blocks.iter().enumerate() {
        match block.terminator {
            Terminator::Fallthrough(t) => predecessors[t].push((i, false)),
            Terminator::Jump {
                target,
                is_backward,
            } => predecessors[target].push((i, is_backward)),
            Terminator::Branch { if_true, if_false } => {
                predecessors[if_true].push((i, false));
                predecessors[if_false].push((i, false));
            }
            Terminator::Return => {}
        }
    }
    (0..cfg.blocks.len())
        .map(|b| {
            if entry_depth[b] == 0 {
                EntryStackSource::Empty
            } else if predecessors[b].len() == 1 && !predecessors[b][0].1 {
                EntryStackSource::Inherit(predecessors[b][0].0)
            } else {
                EntryStackSource::Merge
            }
        })
        .collect()
}

/// Which smi-inlineable shape a mono smi-guarded IC's target primitive
/// maps to (D1 item 2's `SMI_INLINE` set; ids from `runtime::primitives`:
/// 1=+ 2=- 3=* 6=bitAnd: 7=bitOr: 8=bitXor: 10=< 11=<= 12=> 13=>= 14== 15=~=).
enum SmiSendKind {
    Arith(SmiOp),
    Cmp(CmpOp),
}

fn classify_smi_send(vm: &VmState, method: MethodOop, ic_idx: u16) -> SmiSendKind {
    let ic = InterpreterIc::at(method, ic_idx);
    assert_eq!(
        ic.guard().raw(),
        vm.universe.smi_klass.oop().raw(),
        "classify_smi_send: IC {ic_idx} is not mono-smi-guarded -- driver::eligible should \
         have rejected this method (compiler bug if this fires)"
    );
    let target = MethodOop::try_from(ic.target())
        .expect("classify_smi_send: mono IC target must be a CompiledMethod");
    match target.primitive() {
        1 => SmiSendKind::Arith(SmiOp::Add),
        2 => SmiSendKind::Arith(SmiOp::Sub),
        3 => SmiSendKind::Arith(SmiOp::Mul),
        6 => SmiSendKind::Arith(SmiOp::And),
        7 => SmiSendKind::Arith(SmiOp::Or),
        8 => SmiSendKind::Arith(SmiOp::Xor),
        10 => SmiSendKind::Cmp(CmpOp::Lt),
        11 => SmiSendKind::Cmp(CmpOp::Le),
        12 => SmiSendKind::Cmp(CmpOp::Gt),
        13 => SmiSendKind::Cmp(CmpOp::Ge),
        14 => SmiSendKind::Cmp(CmpOp::Eq),
        15 => SmiSendKind::Cmp(CmpOp::Ne),
        other => panic!(
            "classify_smi_send: primitive {other} is not in SMI_INLINE -- driver::eligible \
             should have rejected this method (compiler bug if this fires)"
        ),
    }
}

/// Compiler-stage literal pool, deduplicated by `(value, kind)` — same
/// rationale as `JasmAssembler`'s own pool dedup (P10): a value that is
/// both a plain constant and separately relocation-bearing must not share
/// a slot.
struct PoolBuilder {
    entries: Vec<PoolEntry>,
    dedup: HashMap<(u64, Option<RelocKind>), PoolLit>,
}

impl PoolBuilder {
    fn new() -> PoolBuilder {
        PoolBuilder {
            entries: Vec::new(),
            dedup: HashMap::new(),
        }
    }

    fn intern(&mut self, value: u64, kind: Option<RelocKind>) -> PoolLit {
        let key = (value, kind);
        if let Some(&id) = self.dedup.get(&key) {
            return id;
        }
        let id = PoolLit(self.entries.len() as u32);
        self.entries.push(PoolEntry { value, kind });
        self.dedup.insert(key, id);
        id
    }
}

/// Owns the state accumulated across every block's translation (`vregs`,
/// `pool`) plus the read-only per-method context every block needs
/// (`self_vreg`/`temp_vregs`) — a struct instead of a long parameter list
/// threaded through every helper.
struct Translator<'a> {
    vm: &'a VmState,
    method: MethodOop,
    vregs: Vec<VRegInfo>,
    pool: PoolBuilder,
    self_vreg: VReg,
    temp_vregs: Vec<VReg>,
    /// The one shared bailout block's id — known from `cfg.blocks.len()`
    /// before any translation starts, so every `SmiArith`/`SmiCmpVal`'s
    /// `fail` field can reference it directly.
    bailout_id: BlockId,
}

impl<'a> Translator<'a> {
    fn fresh(&mut self, is_oop: bool) -> VReg {
        let id = self.vregs.len() as u32;
        self.vregs.push(VRegInfo { is_oop });
        VReg(id)
    }

    /// Translates every instruction except a block's own terminator
    /// (`Jump`/`Branch`-family and `Return`-family are structurally
    /// distinct: `Return` needs nothing beyond what's already on hand and
    /// is handled directly here, but `Jump`/`Branch` need the *already-
    /// resolved* CFG target — decode.rs's job — plus merge-vreg info that
    /// spans the whole method, so `convert` handles those itself once this
    /// loop finishes). `fusable` tells a comparison send whether it's
    /// eligible for the branch-fusion peephole (D3.2): true only when this
    /// instruction is immediately followed by nothing but the block's own
    /// terminator AND that terminator is specifically a `Branch` (a
    /// comparison can just as easily be immediately followed by a `Jump`
    /// or `Return` instead — `^ x < y` returns the plain boolean, no
    /// branch at all — so both conditions matter, not just position).
    #[allow(clippy::too_many_arguments)]
    fn translate_instr(
        &mut self,
        instr: &Instr,
        fusable: bool,
        stack: &mut Vec<VReg>,
        code: &mut Vec<Ir>,
        pending_cmp: &mut Option<(CmpOp, VReg, VReg)>,
    ) {
        match *instr {
            Instr::PushSelf => stack.push(self.self_vreg),
            Instr::PushNil => {
                stack.push(self.push_well_known(self.vm.universe.nil_obj.raw(), code))
            }
            Instr::PushTrue => {
                stack.push(self.push_well_known(self.vm.universe.true_obj.raw(), code))
            }
            Instr::PushFalse => {
                stack.push(self.push_well_known(self.vm.universe.false_obj.raw(), code))
            }
            Instr::PushSmi(v) => {
                let dst = self.fresh(true);
                code.push(Ir::ConstSmi {
                    dst,
                    value: v as i64,
                });
                stack.push(dst);
            }
            Instr::PushLiteral(idx) => {
                let lit_oop = self.method.literals().at(idx as usize);
                stack.push(self.push_well_known(lit_oop.raw(), code));
            }
            Instr::PushTemp(n) => {
                // Always a fresh copy (v1 rule, D3.2): temp_vregs[n] may be
                // reassigned later on this path; regalloc coalescing away
                // the redundant Move is a later nicety, not S10's job.
                let dst = self.fresh(true);
                code.push(Ir::Move {
                    dst,
                    src: self.temp_vregs[n as usize],
                });
                stack.push(dst);
            }
            Instr::StoreTemp(n) => {
                let tos = *stack.last().expect("store_temp: empty simulated stack");
                code.push(Ir::Move {
                    dst: self.temp_vregs[n as usize],
                    src: tos,
                });
            }
            Instr::StoreTempPop(n) => {
                let v = stack.pop().expect("store_temp_pop: empty simulated stack");
                code.push(Ir::Move {
                    dst: self.temp_vregs[n as usize],
                    src: v,
                });
            }
            Instr::PushInstvar(n) => {
                let dst = self.fresh(true);
                code.push(Ir::LoadField {
                    dst,
                    obj: self.self_vreg,
                    byte_off: (BODY_OFFSET + 8 * n as usize) as i32,
                });
                stack.push(dst);
            }
            Instr::PushGlobal(idx) => {
                let assoc = self.method.literals().at(idx as usize);
                let assoc_vreg = self.push_well_known(assoc.raw(), code);
                let dst = self.fresh(true);
                // Association body word 1 = value (word 0 = key), matching
                // the interpreter's own `assoc.body_oop(1)`.
                code.push(Ir::LoadField {
                    dst,
                    obj: assoc_vreg,
                    byte_off: (BODY_OFFSET + 8) as i32,
                });
                stack.push(dst);
            }
            Instr::Pop => {
                stack.pop().expect("pop: empty simulated stack");
            }
            Instr::Dup => {
                let tos = *stack.last().expect("dup: empty simulated stack");
                stack.push(tos);
            }
            Instr::Send { ic, super_ } => {
                debug_assert!(
                    !super_,
                    "translate: super sends are excluded by eligibility (D1)"
                );
                let b = stack.pop().expect("send: missing arg operand");
                let a = stack.pop().expect("send: missing receiver operand");
                match classify_smi_send(self.vm, self.method, ic) {
                    SmiSendKind::Cmp(op) if fusable => {
                        // `fusable` already confirms the terminator is a
                        // Branch -- `convert` consumes `pending_cmp` there.
                        *pending_cmp = Some((op, a, b));
                    }
                    SmiSendKind::Cmp(op) => {
                        let dst = self.fresh(true);
                        code.push(Ir::SmiCmpVal {
                            op,
                            dst,
                            a,
                            b,
                            fail: self.bailout_id,
                        });
                        stack.push(dst);
                    }
                    SmiSendKind::Arith(op) => {
                        let dst = self.fresh(true);
                        code.push(Ir::SmiArith {
                            op,
                            dst,
                            a,
                            b,
                            fail: self.bailout_id,
                        });
                        stack.push(dst);
                    }
                }
            }
            Instr::JumpFwd(_) | Instr::JumpBack(_) | Instr::BrTrueFwd(_) | Instr::BrFalseFwd(_) => {
                // Deferred to `convert` (see this fn's own doc).
            }
            Instr::ReturnTos => {
                let val = stack.pop().expect("return_tos: empty simulated stack");
                code.push(Ir::Ret { val });
            }
            Instr::ReturnSelf => code.push(Ir::RetSelf),
            Instr::StoreInstvarPop(_)
            | Instr::StoreGlobalPop(_)
            | Instr::PushCtxTemp { .. }
            | Instr::StoreCtxTempPop { .. }
            | Instr::PushClosure { .. }
            | Instr::BlockReturnTos
            | Instr::NlrTos => {
                panic!(
                    "translate_instr: {instr:?} should have been rejected by driver::eligible \
                     (D1) -- compiler bug if this fires"
                )
            }
        }
    }

    fn push_well_known(&mut self, raw: u64, code: &mut Vec<Ir>) -> VReg {
        let lit = self.pool.intern(raw, Some(RelocKind::Oop));
        let dst = self.fresh(true);
        code.push(Ir::ConstPool { dst, lit });
        dst
    }
}

fn emit_merges(
    sources: &[EntryStackSource],
    entry_stacks: &[Option<Vec<VReg>>],
    target: BlockIndex,
    local_exit: &[VReg],
    code: &mut Vec<Ir>,
) {
    if let EntryStackSource::Merge = sources[target] {
        let merge_vregs = entry_stacks[target]
            .as_ref()
            .expect("emit_merges: merge vregs are pre-allocated before any block is translated");
        assert_eq!(
            merge_vregs.len(),
            local_exit.len(),
            "emit_merges: depth mismatch feeding block {target} (compiler bug)"
        );
        for (&dst, &src) in merge_vregs.iter().zip(local_exit.iter()) {
            code.push(Ir::Move { dst, src });
        }
    }
}

/// D3.2/D3.3: convert `method`'s bytecode (already decoded into `cfg`) into
/// an [`IrMethod`]. `vm` supplies well-known oops (`nil`/`true`/`false`)
/// and the smi klass every inlined send's IC is checked against.
pub fn convert(vm: &VmState, method: MethodOop, cfg: &Cfg) -> IrMethod {
    let entry_depth = compute_entry_depths(method, cfg);
    let sources = entry_stack_sources(cfg, &entry_depth);

    // Known before any translation starts (D2's bailout block sits right
    // after every real CFG block, 1:1 by position).
    let bailout_id = BlockId(cfg.blocks.len() as u32);
    let block_id = |i: BlockIndex| BlockId(i as u32);

    let self_vreg = VReg(0);
    let mut vregs = vec![VRegInfo { is_oop: true }];
    // `temp_vregs[i]` is the unified arg/temp slot `Frame::temp_index` also
    // uses (SPEC §5.1): `0..argc` are args, `argc..argc+ntemps` are the
    // method's own local temps — `method.ntemps()` alone is only the
    // LATTER count (`interpreter::stack::Frame::temp_index`'s own
    // `t < argc + ntemps` bound confirms this), so the vreg vector needs
    // `argc + ntemps` total entries, not `ntemps`.
    let temp_vregs: Vec<VReg> = (0..method.argc() + method.ntemps())
        .map(|i| {
            vregs.push(VRegInfo { is_oop: true });
            VReg((i + 1) as u32)
        })
        .collect();
    let mut t = Translator {
        vm,
        method,
        vregs,
        pool: PoolBuilder::new(),
        self_vreg,
        temp_vregs: temp_vregs.clone(),
        bailout_id,
    };

    // Pre-allocate merge vregs for every block that needs them (D3.2) —
    // predecessors need these ids available when THEY emit their own
    // terminator, which for a forward edge happens before the merge
    // block itself is ever translated.
    let mut entry_stacks: Vec<Option<Vec<VReg>>> = vec![None; cfg.blocks.len()];
    for (b, source) in sources.iter().enumerate() {
        match source {
            EntryStackSource::Empty => entry_stacks[b] = Some(Vec::new()),
            EntryStackSource::Merge => {
                let depth = entry_depth[b] as usize;
                let regs = (0..depth).map(|_| t.fresh(true)).collect();
                entry_stacks[b] = Some(regs);
            }
            EntryStackSource::Inherit(_) => {}
        }
    }

    let nil_lit = t
        .pool
        .intern(vm.universe.nil_obj.raw(), Some(RelocKind::Oop));
    // `SmiCmpVal`/`BoolBr`'s own emit sequences (D5.3) need true_obj/
    // false_obj as pool constants regardless of whether this method's
    // bytecode happens to contain an explicit push_true/push_false —
    // eagerly interning them here (not gated on the fused-comparison rule
    // that produces those Ir ops needing them) guarantees `emit.rs`, which
    // has no VmState access of its own, always finds them via
    // `IrMethod::true_lit`/`false_lit` rather than a fragile fixed-index
    // convention.
    let true_lit = t
        .pool
        .intern(vm.universe.true_obj.raw(), Some(RelocKind::Oop));
    let false_lit = t
        .pool
        .intern(vm.universe.false_obj.raw(), Some(RelocKind::Oop));

    let mut exit_stacks: Vec<Option<Vec<VReg>>> = vec![None; cfg.blocks.len()];
    let mut ir_blocks: Vec<IrBlock> = Vec::with_capacity(cfg.blocks.len() + 1);

    for (b, cfg_block) in cfg.blocks.iter().enumerate() {
        let entry_stack = match sources[b] {
            EntryStackSource::Empty | EntryStackSource::Merge => {
                entry_stacks[b].clone().expect("pre-allocated above")
            }
            EntryStackSource::Inherit(pred) => exit_stacks[pred].clone().expect(
                "entry_stack_sources guarantees a non-backward single predecessor is \
                 translated first (blocks are walked in increasing bci order)",
            ),
        };

        let mut code = Vec::new();
        if b == 0 {
            code.push(Ir::Param {
                dst: self_vreg,
                index: 0,
            });
            for (i, &tv) in temp_vregs.iter().enumerate().take(method.argc()) {
                code.push(Ir::Param {
                    dst: tv,
                    index: (i + 1) as u8,
                });
            }
            for &tv in temp_vregs.iter().skip(method.argc()) {
                code.push(Ir::ConstPool {
                    dst: tv,
                    lit: nil_lit,
                });
            }
        }

        let is_branch_terminator = matches!(cfg_block.terminator, Terminator::Branch { .. });
        let mut stack = entry_stack.clone();
        let mut pending_cmp: Option<(CmpOp, VReg, VReg)> = None;
        let mut bci = cfg_block.bci_start;
        while bci < cfg_block.bci_end {
            let (instr, next) = decode_at(method, bci);
            // Fusable iff nothing but the block's own terminator follows
            // this instruction, AND that terminator is a Branch (D3.2 —
            // see `translate_instr`'s own doc for why position alone,
            // `next == cfg_block.bci_end`, isn't sufficient: it only
            // proves THIS instruction ends the block, not that a Branch
            // immediately follows it -- a block can hold several plain
            // instructions before its one terminator, e.g. `push_self;
            // push_temp; send; br_false_fwd` all in one block).
            let fusable = is_branch_terminator
                && next < cfg_block.bci_end
                && decode_at(method, next).1 == cfg_block.bci_end;
            t.translate_instr(&instr, fusable, &mut stack, &mut code, &mut pending_cmp);
            bci = next;
        }
        let local_exit = stack;

        match cfg_block.terminator {
            Terminator::Return => {}
            Terminator::Fallthrough(target) => {
                emit_merges(&sources, &entry_stacks, target, &local_exit, &mut code);
                code.push(Ir::Jump {
                    target: block_id(target),
                });
            }
            Terminator::Jump {
                target,
                is_backward,
            } => {
                if is_backward {
                    code.push(Ir::Poll);
                }
                emit_merges(&sources, &entry_stacks, target, &local_exit, &mut code);
                code.push(Ir::Jump {
                    target: block_id(target),
                });
            }
            Terminator::Branch { if_true, if_false } => {
                emit_merges(&sources, &entry_stacks, if_true, &local_exit, &mut code);
                emit_merges(&sources, &entry_stacks, if_false, &local_exit, &mut code);
                if let Some((op, a, b_)) = pending_cmp.take() {
                    code.push(Ir::SmiCmpBr {
                        op,
                        a,
                        b: b_,
                        if_true: block_id(if_true),
                        if_false: block_id(if_false),
                        fail: bailout_id,
                    });
                } else {
                    let val = *local_exit
                        .last()
                        .expect("branch terminator with an empty simulated stack (compiler bug)");
                    code.push(Ir::BoolBr {
                        val,
                        if_true: block_id(if_true),
                        if_false: block_id(if_false),
                        not_bool: bailout_id,
                    });
                }
            }
        }

        exit_stacks[b] = Some(local_exit);
        ir_blocks.push(IrBlock {
            id: block_id(b),
            bci: cfg_block.bci_start,
            code,
            entry_stack,
        });
    }

    ir_blocks.push(IrBlock {
        id: bailout_id,
        bci: 0,
        code: vec![Ir::Bailout {
            reason: BailoutReason::SmiOpFailed,
        }],
        entry_stack: Vec::new(),
    });

    IrMethod {
        blocks: ir_blocks,
        vregs: t.vregs,
        pool: t.pool.entries,
        argc: method.argc() as u8,
        ntemps: method.ntemps() as u8,
        safepoints: Vec::new(),
        true_lit,
        false_lit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::BytecodeBuilder;
    use crate::compiler::decode;
    use crate::interpreter::ic::InterpreterIc;
    use crate::runtime::{JitMode, VmOptions, VmState};

    fn test_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            gc_stress_full_period: None,
            eden_kb: None,
            jit: JitMode::Off,
        })
    }

    /// A throwaway method standing in for a real SmallInteger primitive —
    /// `classify_smi_send` only ever reads its `primitive()` field, never
    /// executes its bytecode.
    fn primitive_stub(
        vm: &mut VmState,
        sel: crate::oops::wrappers::SymbolOop,
        prim_id: i64,
    ) -> MethodOop {
        let mut b = BytecodeBuilder::new();
        b.ret_self();
        let m = b.finish(vm, sel, 1, 0);
        m.set_primitive(prim_id);
        m
    }

    /// `send site with mono smi IC + prim '+'` (D1 item 2): a monomorphic,
    /// smi-guarded send whose target's primitive is `+` (id 1) translates
    /// to `SmiArith{Add}` whose `fail` is the method's shared bailout
    /// block.
    #[test]
    fn smi_send_inlined_mono() {
        let mut vm = test_vm();
        let plus_sel = vm.universe.intern(b"+");
        let plus_method = primitive_stub(&mut vm, plus_sel, 1);

        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.push_temp(0);
        b.send(&mut vm, plus_sel, 1);
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 1, 0);

        let ic = InterpreterIc::at(method, 0);
        let smi_klass = vm.universe.smi_klass;
        let epoch = vm.ic_epoch;
        ic.set_mono(&mut vm, smi_klass, plus_method, epoch);

        let cfg = decode::decode(method);
        let ir = convert(&vm, method, &cfg);

        let bailout_id = ir.blocks.last().unwrap().id;
        let fail = ir.blocks[0]
            .code
            .iter()
            .find_map(|op| match op {
                Ir::SmiArith {
                    op: SmiOp::Add,
                    fail,
                    ..
                } => Some(*fail),
                _ => None,
            })
            .expect("send site must translate to SmiArith{Add}");
        assert_eq!(fail, bailout_id);
        assert!(matches!(
            ir.blocks.last().unwrap().code[..],
            [Ir::Bailout {
                reason: BailoutReason::SmiOpFailed
            }]
        ));
    }

    /// The fusion peephole (D3.2): a comparison send immediately consumed
    /// by `br_false_fwd` becomes a single `SmiCmpBr` — no intermediate
    /// `SmiCmpVal` materialization, no separate `BoolBr`.
    #[test]
    fn smi_cmp_fused_with_branch() {
        let mut vm = test_vm();
        let lt_sel = vm.universe.intern(b"<");
        let lt_method = primitive_stub(&mut vm, lt_sel, 10);

        let mut b = BytecodeBuilder::new();
        let l1 = b.new_label();
        let l2 = b.new_label();
        b.push_self();
        b.push_temp(0);
        b.send(&mut vm, lt_sel, 1);
        b.br_false_fwd(l1);
        b.push_smi_i8(1);
        b.jump_fwd(l2);
        b.bind(l1);
        b.push_smi_i8(0);
        b.bind(l2);
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 1, 0);

        let ic = InterpreterIc::at(method, 0);
        let smi_klass = vm.universe.smi_klass;
        let epoch = vm.ic_epoch;
        ic.set_mono(&mut vm, smi_klass, lt_method, epoch);

        let cfg = decode::decode(method);
        let ir = convert(&vm, method, &cfg);

        let block0 = &ir.blocks[0];
        assert!(
            block0
                .code
                .iter()
                .any(|op| matches!(op, Ir::SmiCmpBr { op: CmpOp::Lt, .. })),
            "must fuse into a single SmiCmpBr"
        );
        assert!(
            !block0
                .code
                .iter()
                .any(|op| matches!(op, Ir::SmiCmpVal { .. } | Ir::BoolBr { .. })),
            "fusion must skip both the materialize step and the separate branch"
        );
    }

    /// D3.2's merge rule: the `ifTrue:ifFalse:` value pattern's join block
    /// has entry_depth 1, owns one fresh merge vreg, and both predecessors
    /// end their code with a `Move` into it.
    #[test]
    fn stack_sim_depths_agree() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        let l1 = b.new_label();
        let l2 = b.new_label();
        b.push_true();
        b.br_false_fwd(l1);
        b.push_smi_i8(1);
        b.jump_fwd(l2);
        b.bind(l1);
        b.push_smi_i8(2);
        b.bind(l2);
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 0, 0);

        let cfg = decode::decode(method);
        let ir = convert(&vm, method, &cfg);

        // Same block layout as decode::tests::leaders_if_else: 0=condition,
        // 1=true arm, 2=false arm, 3=merge.
        let merge = &ir.blocks[3];
        assert_eq!(
            merge.entry_stack.len(),
            1,
            "entry_depth 1 -> one merge vreg"
        );
        let merge_vreg = merge.entry_stack[0];

        for pred_idx in [1usize, 2] {
            let pred = &ir.blocks[pred_idx];
            let last_move = pred.code.iter().rev().find_map(|op| match op {
                Ir::Move { dst, src } => Some((*dst, *src)),
                _ => None,
            });
            assert_eq!(
                last_move.map(|(dst, _)| dst),
                Some(merge_vreg),
                "block {pred_idx} must Move its exit value into the merge vreg"
            );
        }
    }

    /// No gratuitous copies: a block with exactly one non-backward
    /// predecessor inherits its exit stack's vregs verbatim.
    #[test]
    fn single_pred_inherits_stack() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        let l = b.new_label();
        b.push_smi_i8(1);
        b.jump_fwd(l);
        b.bind(l);
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 0, 0);

        let cfg = decode::decode(method);
        let ir = convert(&vm, method, &cfg);

        // 2 real blocks (push+jump, then the target) plus the one always-
        // appended synthetic bailout block.
        assert_eq!(ir.blocks.len(), 3);
        let const_dst = ir.blocks[0]
            .code
            .iter()
            .find_map(|op| match op {
                Ir::ConstSmi { dst, value: 1 } => Some(*dst),
                _ => None,
            })
            .expect("block 0 has a ConstSmi(1)");
        assert_eq!(ir.blocks[1].entry_stack, vec![const_dst]);
        assert!(
            !ir.blocks[0]
                .code
                .iter()
                .any(|op| matches!(op, Ir::Move { .. })),
            "a single non-backward predecessor must not insert a copy"
        );
    }

    /// SSA-lite's temp rule: `store_temp_pop` and a later block's
    /// `push_temp` both reference the SAME persistent per-method vreg for
    /// that slot (`push_temp` still makes its own defensive copy of it —
    /// D3.2's v1 rule — but the copy's *source* is the one shared vreg).
    #[test]
    fn temp_slots_single_vreg() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        let l1 = b.new_label();
        let l2 = b.new_label();
        b.push_smi_i8(5);
        b.store_temp_pop(0);
        b.push_true();
        b.br_false_fwd(l1);
        b.push_temp(0);
        b.jump_fwd(l2);
        b.bind(l1);
        b.push_smi_i8(0);
        b.bind(l2);
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 0, 1);

        let cfg = decode::decode(method);
        let ir = convert(&vm, method, &cfg);

        let stored_vreg = ir.blocks[0]
            .code
            .iter()
            .find_map(|op| match op {
                Ir::Move { dst, .. } => Some(*dst),
                _ => None,
            })
            .expect("block 0 stores into temp 0 via a Move");
        let push_temp_src = ir.blocks[1]
            .code
            .iter()
            .find_map(|op| match op {
                Ir::Move { src, .. } => Some(*src),
                _ => None,
            })
            .expect("block 1 reads temp 0 via a Move");
        assert_eq!(push_temp_src, stored_vreg);
    }
}
