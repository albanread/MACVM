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
use crate::compiler::scopes::SafepointKind;
use crate::interpreter::ic::InterpreterIc;
use crate::oops::layout::BODY_OFFSET;
use crate::oops::wrappers::{KlassOop, MethodOop, SymbolOop};
use crate::runtime::vm_state::VmState;

/// `primitives.rs`'s pinned id for `basicNew` (`Object>>basicNew`,
/// `<primitive: 23>`) — the target an inline-allocatable `X basicNew` site
/// must resolve to (`Translator::alloc_site_klass`, S11 D7). `pub(crate)`:
/// `driver::mono_smi_inline_send` reads it too, to let a mono basicNew site
/// clear eligibility (both readers must agree on the same id).
pub(crate) const PRIM_BASIC_NEW: i64 = 23;

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

impl StubId {
    /// D1/D5: `codecache::stubs::Stubs::must_be_boolean` (S11 step 6) —
    /// the only `CallRuntime` target step 7 wires up; `emit.rs`'s own
    /// `Ir::CallRuntime` dispatch asserts this is the only value it ever
    /// sees, since `rt_alloc_slow`/`stub_nlr_unwind` (steps 8/9) don't
    /// exist as `CallRuntime` targets yet either.
    pub const MUST_BE_BOOLEAN: StubId = StubId(0);
}

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
#[derive(Debug)]
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
    /// S11 D7 inline allocation of a fixed-size `Format::Slots` object
    /// (`X new` where `X` is a compile-time-known class constant). `emit.rs`
    /// lowers this to a SELF-CONTAINED fast-path-plus-slow-call sequence
    /// (bump `reg_block.eden_top`, bounds-check `eden_end`, init header +
    /// nil body, mem-tag; on overflow `bl stub_alloc_slow`) with its own
    /// internal labels — no separate slow CFG block, mirroring how
    /// `StoreField`'s barrier is self-contained. Still a regalloc SAFEPOINT
    /// (`is_safepoint`), so every vreg live across it is already spilled
    /// before the internal slow call's `bl` can clobber a caller-saved
    /// register. `klass` is the class oop (`RelocKind::Oop` pool entry, read
    /// from the `push_global` class constant at the send site); `size_words`
    /// is `klass.non_indexable_size()`, fixed at compile time (guarded by
    /// S13 deps — until then a klass-format redefinition is a documented
    /// stale-code hole flushed by S12's sweep, same class of hole as
    /// `stale_mono_documented_hole`).
    Alloc {
        dst: VReg,
        klass: PoolLit,
        size_words: u32,
    },
    /// Loop back-edge flag check (D5.3); reads `VmRegBlock::poll_flag`.
    Poll,
    /// S13 step 7b (D3, first "organic trap client"): an uncommon-trap
    /// terminator — control leaves the compiled method entirely via a
    /// `brk #0xDE00` (`emit.rs` lowers it), so it has NO `dst` and NO fall-
    /// through. `emit.rs`'s lowering records ITS OWN offset as a safepoint pc
    /// (a trap keys on the brk instruction itself, not a return address); the
    /// SIGTRAP handler → `rt_uncommon_trap` deopts the frame and resumes the
    /// re-executing send in the interpreter. `bci` is the resume point (the
    /// failing send's own bci) — carried here so `driver::build_deopt_metadata`
    /// and the emitted `SafepointPc` agree with the recorded `DeoptRaw.bci`.
    /// It is a regalloc SAFEPOINT (see `regalloc::is_safepoint`), which is what
    /// spills every oop live across it (the re-executing send's `a`/`b`/`self`)
    /// into the frame so the deopt materializer can read them.
    UncommonTrap {
        bci: usize,
    },
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
            // `self` is always `VReg(0)` by this module's own construction
            // (`convert()`'s `let self_vreg = VReg(0);` below) -- RetSelf
            // reads it exactly like `Ret{val}` reads `val`. Omitting this
            // use let regalloc's `compute_intervals` treat self as
            // dead-on-arrival (a trivial `[def,def]` interval) whenever a
            // later argc param's own `Param` vreg outlived it and got
            // handed the same register on retire -- emit.rs's sequential
            // (not parallel) ABI-register shuffle would then clobber that
            // register before `RetSelf`'s own "nothing to do" handler ever
            // ran, so a compiled `^self` could return the wrong value
            // whenever an explicit argument went unused (found via
            // `compiled_entry_stack_discipline_across_argc`).
            Ir::RetSelf => f(VReg(0)),
            Ir::ConstSmi { .. }
            | Ir::ConstPool { .. }
            | Ir::Param { .. }
            | Ir::Jump { .. }
            | Ir::Alloc { .. }
            | Ir::Poll
            // S13 step 7b: an UncommonTrap reads NO vreg directly — the
            // re-executing send's inputs (`a`/`b`/`self`) are kept live
            // across it by the fail block's own `DeoptRaw.stack`, not by an
            // operand of this op, so its liveness comes from `deopt_sites`,
            // not from `uses` (mirrors how a `CallSend`'s recorded stack, not
            // its `args`, is what a deopt reads).
            | Ir::UncommonTrap { .. }
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
            | Ir::UncommonTrap { .. }
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
    /// S13 step 3b: the deopt safepoints WITHIN this block, keyed by the
    /// `code` index of the op that emits the safepoint (a `CallSend`/`Alloc`)
    /// — the converter records one here for each of the 3 clean safepoint
    /// sites (main + super `CallSend`, `Alloc`), carrying the abstract
    /// operand stack captured at that point plus its bci/kind/reexecute.
    /// `driver.rs` re-walks `block_order` to correlate each entry's op
    /// position with the emitted [`crate::compiler::emit::SafepointPc`]
    /// (hence its return-address `pc_off`) and resolves every `VReg` to a
    /// [`crate::compiler::scopes::ValueLoc`]. The synthesized smi-fallback /
    /// not-boolean blocks record NONE (their safepoints can't deopt until
    /// step 7), so this is empty for them — every safepoint-emitting op
    /// without an entry here is simply skipped by the driver.
    pub deopt_sites: Vec<(u32, DeoptRaw)>,
}

/// S13 step 3b: one deopt-recordable safepoint, as the converter sees it —
/// the abstract operand stack (`stack`, a list of live `VReg`s below the
/// safepoint), the resume `bci`, its [`SafepointKind`], and `reexecute`
/// (THE source of truth for stack height: `false` = a call-return site
/// whose recorded stack excludes the popped receiver+args and the
/// materializer pushes the result; `true` = re-execute at `bci` with all
/// inputs still on the recorded stack). `driver.rs` maps each `VReg` to a
/// [`crate::compiler::scopes::ValueLoc`] via `resolve_frame_loc` once the
/// regalloc position is known.
#[derive(Clone, Debug)]
pub struct DeoptRaw {
    pub stack: Vec<VReg>,
    pub bci: usize,
    pub kind: SafepointKind,
    pub reexecute: bool,
}

/// S13 step 7b: the deopt info a fused comparison (`SmiCmpBr`) carries from
/// its own smi-inline site (in `translate_instr`) forward to the block's
/// Branch terminator (in `convert`), where the fail block is actually built.
/// The fail block's reexecute deopt must resume at the SEND's own `bci` with
/// `stack` (= `[below..., a, b]`, captured before the send's operand pops)
/// still present — neither of which is recoverable at the terminator (`bci`
/// there is the block start, and `a`/`b` were already popped), so both ride
/// along here.
#[derive(Clone, Debug)]
struct PendingCmpDeopt {
    bci: usize,
    stack: Vec<VReg>,
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
    /// S11 D7 `Alloc`: `nil_lit` is the `nil` oop the inline fast path
    /// stores into every body slot (nil-init is MANDATORY — a GC at the
    /// next safepoint would otherwise scan a garbage body); `mark_slots_lit`
    /// is the pristine `Format::Slots` mark word
    /// (`Mark::pristine().with_tagged_contents(true)`), stored into the
    /// object header. Both are `emit.rs`-facing constants with no `VmState`
    /// to intern on demand, exactly like `true_lit`/`false_lit`.
    pub nil_lit: PoolLit,
    pub mark_slots_lit: PoolLit,
    /// S11 D3: one entry per `Ir::CallSend` site, indexed by its own
    /// `site: u16` — mirrors `pool`'s own "small side table, indexed by a
    /// compact id embedded in the `Ir`" shape. `emit.rs` has no `VmState`/
    /// `MethodOop` of its own to pull a selector from at emit time (same
    /// reasoning as `true_lit`/`false_lit` above), so `convert()` resolves
    /// it once, here.
    pub call_sites: Vec<CallSiteInfo>,
    /// S13: the physical oop-pool index holding THIS method's own compiled
    /// `MethodOop`, interned (as `RelocKind::Oop`, so a moving GC keeps it
    /// current) at the end of `convert` — but ONLY when the method has at
    /// least one deopt site (a real `CallSend`/`Alloc` safepoint). A deopt
    /// materializer (`runtime::deopt`) reads it via `read_pool_oop` to fill a
    /// reconstructed frame's `FRAME_METHOD` slot (D5 M3); every S13 scope is
    /// this same method (depth-1), so `driver::build_deopt_metadata` stamps
    /// every scope record with this one index. `None` for a method with no
    /// deopt sites (all-smi-inline / no sends) — it never deopts, so it needs
    /// no method-oop pool word, which keeps such methods' listing goldens
    /// byte-stable (the reason step 3b left `method_pool_ix` a placeholder).
    pub method_pool_ix: Option<u32>,
}

/// One `Ir::CallSend` site's selector/argc — see `IrMethod::call_sites`.
#[derive(Clone, Copy, Debug)]
pub struct CallSiteInfo {
    pub selector: SymbolOop,
    pub argc: u8,
    /// D4.6: `Some(holder.superclass())` for a `send_super` site — the
    /// STATIC starting klass for lookup, fixed at compile time (method
    /// holders don't move between klasses in v1) and independent of the
    /// runtime receiver's own klass. `None` for an ordinary dynamic send,
    /// whose starting klass is the receiver's own, only known at runtime.
    /// `driver::compile_method` reads this to pre-resolve a super site's
    /// own target and seed `IcState::Mono` directly, instead of patching
    /// to `stub_resolve` and starting `Unresolved` like every other site.
    pub static_klass: Option<KlassOop>,
}

// ── conversion ───────────────────────────────────────────────────────────

/// Net operand-stack effect of one decoded instruction — the raw material
/// for `compute_entry_depths`' worklist. `Send`'s effect depends on the
/// site's own IC (`argc` isn't in the bytecode operand, only the IC index
/// is), so this needs `method` to look it up.
pub(crate) fn instr_stack_delta(method: MethodOop, instr: &Instr) -> i32 {
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
///
/// Also returns the highest operand-stack depth reached anywhere (block
/// entries and every point strictly inside a block's body) — `driver.rs`'s
/// `eligible` reuses this same traversal for D1's frame-budget check
/// (`ntemps + max_stack <= 60`) rather than re-deriving it with a second,
/// separately-maintained scan; `convert` itself has no use for the value,
/// it just threads it back out.
pub(crate) fn compute_entry_depths(method: MethodOop, cfg: &Cfg) -> (Vec<i32>, i32) {
    let mut entry_depth: Vec<Option<i32>> = vec![None; cfg.blocks.len()];
    entry_depth[0] = Some(0);
    let mut worklist = vec![0usize];
    let mut max_depth = 0i32;

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
        max_depth = max_depth.max(depth);
        let mut bci = block.bci_start;
        while bci < block.bci_end {
            let (instr, next) = decode_at(method, bci);
            depth += instr_stack_delta(method, &instr);
            max_depth = max_depth.max(depth);
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
    (
        entry_depth.into_iter().map(|d| d.unwrap_or(0)).collect(),
        max_depth,
    )
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
    /// S11 step 7 (D1): a smi-inlined fail edge, or a `not_bool` edge, no
    /// longer bails the whole method out to the interpreter — it computes
    /// a real fallback value (`CallSend`/`CallRuntime`) and REJOINS normal
    /// control flow, so each one needs its OWN fresh synthetic `IrBlock`
    /// (a fail block can't share the single "restart from scratch" target
    /// `Ir::Bailout` used to, since it has to rejoin at a SPECIFIC point
    /// with a SPECIFIC value). `next_extra_block` hands out ids starting
    /// right after every real CFG block (mirrors the old single
    /// `bailout_id`'s own "`cfg.blocks.len()`" starting point).
    ///
    /// `blocks_by_id[k]` holds `BlockId(k)`'s own finished `IrBlock` once
    /// known, `None` until then — indexed by id, NOT creation order. This
    /// matters: a fail block and its own continuation are allocated
    /// out-of-order relative to when each one's CODE is actually finished
    /// (`fail_and_continue` mints the CONTINUATION's id before the FAIL
    /// block's own, but the fail block's own code is complete immediately
    /// while the continuation's is only finished later, possibly after
    /// ANOTHER split narrows in first) — `emit.rs`'s own `method.blocks
    /// [bid.0 as usize]` (and its parallel per-index `labels` vec) both
    /// assume `IrMethod.blocks[i].id == BlockId(i)` for every `i`, a
    /// (previously implicit, only just discovered to be load-bearing)
    /// invariant a plain "push whatever finishes, in whatever order it
    /// finishes" `Vec` cannot uphold once more than one block can still be
    /// "in flight" at once. Resized to fit whenever `fresh_block_id` mints
    /// an id beyond the current length.
    next_extra_block: u32,
    blocks_by_id: Vec<Option<IrBlock>>,
    /// D4.6: accumulates one entry per `send_super`/generic/smi-fallback
    /// send translated so far, in encounter order — `Ir::CallSend.site`
    /// indexes into this, same shape as `pool`'s own accumulate-then-hand-
    /// to-`IrMethod` pattern.
    call_sites: Vec<CallSiteInfo>,
    /// S11 D7: vregs known at compile time to hold a specific class
    /// constant, i.e. produced by a `push_global` whose Association value
    /// is a `KlassOop`. Keyed by `VReg.0`. Used to recognize `X basicNew`
    /// at a send site (the receiver traces back to a class constant), the
    /// one shape the inline-allocation fast path (`Ir::Alloc`) fires for.
    /// The class oop is read at compile time and baked into the pool — a
    /// deliberate S13-deferred staleness hole (a later `Global := OtherClass`
    /// isn't observed; a klass-format redefinition flushes via S12's sweep,
    /// same class of hole as `stale_mono_documented_hole`).
    const_class: HashMap<u32, KlassOop>,
}

impl<'a> Translator<'a> {
    fn fresh(&mut self, is_oop: bool) -> VReg {
        let id = self.vregs.len() as u32;
        self.vregs.push(VRegInfo { is_oop });
        VReg(id)
    }

    fn fresh_block_id(&mut self) -> BlockId {
        let id = BlockId(self.next_extra_block);
        self.next_extra_block += 1;
        if self.blocks_by_id.len() <= id.0 as usize {
            self.blocks_by_id.resize_with(id.0 as usize + 1, || None);
        }
        id
    }

    /// The ONLY way a finished `IrBlock` ever enters `blocks_by_id` —
    /// indexed by `block.id`, never appended, so a block whose id was
    /// minted early but whose own code is finished late (or vice versa)
    /// always lands in its own correct slot regardless of finishing order.
    fn finish_block(&mut self, block: IrBlock) {
        let idx = block.id.0 as usize;
        if self.blocks_by_id.len() <= idx {
            self.blocks_by_id.resize_with(idx + 1, || None);
        }
        self.blocks_by_id[idx] = Some(block);
    }

    /// A smi-inlined op's fail edge (`SmiArith`/`SmiCmpVal`, both mid-block,
    /// VALUE-PRODUCING ops with no terminator of their own). S13 step 7b (D3,
    /// the first "organic trap client"): the fail block is now a single
    /// `Ir::UncommonTrap{ bci }` — NO `CallSend`, NO jump-to-continuation.
    /// The deopt transfers control to the interpreter, which re-executes the
    /// whole send (its LargeInteger/Double fallback) itself, so there is
    /// nothing to compute back in compiled code and nothing to rejoin.
    ///
    /// The FAST-path CONTINUATION block is STILL minted (the `SmiArith`/
    /// `SmiCmpVal` success falls through to it, exactly as before) — only the
    /// FAIL block changed. Returns `(fail_block_id, continuation_block_id)`.
    ///
    /// The fail block records ONE deopt site (`DeoptRaw`) at its trap op:
    /// `reexecute = true` at the SEND's bci, with the recorded operand stack
    /// = `reexec_stack` — the snapshot the caller captured BEFORE popping the
    /// send's own operands, so `a`/`b` are still on it (see `translate_instr`'s
    /// smi-inline site). Those live-across vregs are what regalloc spill-all
    /// pins into the frame for the materializer to read.
    fn fail_and_continue(
        &mut self,
        fail_bci: usize,
        reexec_stack: Vec<VReg>,
    ) -> (BlockId, BlockId) {
        let continuation_id = self.fresh_block_id();
        let fail_id = self.fresh_block_id();
        self.finish_block(IrBlock {
            id: fail_id,
            bci: fail_bci,
            code: vec![Ir::UncommonTrap { bci: fail_bci }],
            entry_stack: Vec::new(),
            deopt_sites: vec![(
                0,
                DeoptRaw {
                    stack: reexec_stack,
                    bci: fail_bci,
                    kind: SafepointKind::UncommonTrap,
                    reexecute: true,
                },
            )],
        });
        (fail_id, continuation_id)
    }

    /// D1's `BoolBr.not_bool` replacement: `rt_must_be_boolean` (S11 step
    /// 6) then re-tests its own result — a self-loop, exactly mirroring
    /// the interpreter's own documented "a handler that keeps returning
    /// non-booleans livelocks by construction, same as real Smalltalk, not
    /// a VM bug" (`interpreter::mod::must_be_boolean_send`'s doc). Reuses
    /// `val` as BOTH the `CallRuntime`'s arg AND its own `dst` — the
    /// block's only live value is "whatever we're testing this iteration",
    /// so no second vreg is needed, and every caller (an ordinary
    /// `BoolBr`'s own `not_bool` edge, or a fused `SmiCmpBr`'s synthesized
    /// fallback) can hand this whatever vreg already holds the value that
    /// failed the FIRST check, unchanged.
    fn fresh_not_bool_block(&mut self, val: VReg, if_true: BlockId, if_false: BlockId) -> BlockId {
        let not_bool_id = self.fresh_block_id();
        self.finish_block(IrBlock {
            id: not_bool_id,
            bci: 0, // not a real bytecode position, same as the old bailout block's own bci:0
            code: vec![
                Ir::CallRuntime {
                    dst: Some(val),
                    stub: StubId::MUST_BE_BOOLEAN,
                    args: vec![val],
                },
                Ir::BoolBr {
                    val,
                    if_true,
                    if_false,
                    not_bool: not_bool_id,
                },
            ],
            entry_stack: Vec::new(),
            // The synthesized smi-fallback / not-boolean blocks record no
            // deopt sites in v1 (S13 step 3b): their CallSend/CallRuntime
            // safepoints can't deopt until step 7 turns them into real trap
            // clients, so the driver simply finds nothing to attach here.
            deopt_sites: Vec::new(),
        });
        not_bool_id
    }

    /// `SmiCmpBr`'s own fail edge (the FUSED compare+branch case). S13 step
    /// 7b: like `fail_and_continue`, the whole fail block collapses to a
    /// single `Ir::UncommonTrap{ bci }` — the CallSend, the BoolBr, AND its
    /// synthesized `not_bool` retry block are all gone (the deopt re-executes
    /// the comparison send in the interpreter, which runs both the real
    /// selector AND its own `mustBeBoolean` handling if the override returns a
    /// non-boolean). No continuation split is needed here (this fires only at
    /// a block's own Branch terminator, whose `if_true`/`if_false` are already
    /// real CFG targets) — but for a smi-cmp fail those targets are simply
    /// never reached from this fail edge; control leaves via the trap.
    ///
    /// Records ONE deopt site: `reexecute = true` at the comparison send's
    /// bci, recorded stack = `reexec_stack` (captured before the send's
    /// operand pops, so `a`/`b` are present) — same convention as
    /// `fail_and_continue`.
    fn fail_and_branch(&mut self, fail_bci: usize, reexec_stack: Vec<VReg>) -> BlockId {
        let fail_id = self.fresh_block_id();
        self.finish_block(IrBlock {
            id: fail_id,
            bci: fail_bci,
            code: vec![Ir::UncommonTrap { bci: fail_bci }],
            entry_stack: Vec::new(),
            deopt_sites: vec![(
                0,
                DeoptRaw {
                    stack: reexec_stack,
                    bci: fail_bci,
                    kind: SafepointKind::UncommonTrap,
                    reexecute: true,
                },
            )],
        });
        fail_id
    }

    /// D1's "arbitrary send in ANY IC state" relaxation vs. D3.2's own
    /// mono-smi-inline fast path: TRUE iff `ic_idx`'s site is monomorphic,
    /// smi-guarded, and its cached target's primitive is smi-inlinable —
    /// the EXACT condition `driver::eligibility_detail`'s own
    /// `mono_smi_inline_send` allows through as `Eligibility::Yes` via the
    /// fast-path arm (kept in sync by hand, cross-referenced doc comments
    /// on both sides, same as this module's own established tolerance for
    /// controlled duplication over a cross-module coupling for a
    /// three-line predicate — `project_s11_step4_design`'s own "two
    /// conventions, kept intentionally separate" precedent). Every OTHER
    /// IC state (Empty at compile time can't happen — `driver.rs`'s own
    /// `NoRetryLater` keeps a cold site from ever reaching a real compile;
    /// Poly, Mega, mono-but-non-smi, mono-smi-but-not-inlinable) still
    /// compiles now, just as an ordinary generic `Ir::CallSend` instead.
    fn is_smi_inlinable(&self, ic_idx: u16) -> bool {
        let ic = InterpreterIc::at(self.method, ic_idx);
        if ic.guard().raw() != self.vm.universe.smi_klass.oop().raw() {
            return false;
        }
        let Some(target) = MethodOop::try_from(ic.target()) else {
            return false;
        };
        crate::compiler::driver::SMI_INLINE.contains(&target.primitive())
    }

    /// S11 D7: is this send site an inline-allocatable `X basicNew`? Returns
    /// `Some((klass, size_words))` when: the `receiver` vreg is a known
    /// compile-time class constant `X` (from a `push_global`, tracked in
    /// `const_class`); the send takes no arguments; the site's mono target
    /// really is the `basicNew` primitive (guards against a class that
    /// overrides `basicNew` — a Poly/Mega/non-method target fails the
    /// `MethodOop::try_from` exactly as `is_smi_inlinable` relies on); and
    /// `X` is a fixed-size `Format::Slots` class small enough for the inline
    /// fast path's 12-bit `add`/`str` immediates. Anything else stays an
    /// ordinary generic `basicNew` send.
    fn alloc_site_klass(&self, ic_idx: u16, receiver: VReg) -> Option<(KlassOop, u32)> {
        let klass = *self.const_class.get(&receiver.0)?;
        let ic = InterpreterIc::at(self.method, ic_idx);
        if ic.argc() != 0 {
            return None;
        }
        let target = MethodOop::try_from(ic.target())?;
        if target.primitive() != PRIM_BASIC_NEW {
            return None;
        }
        if !matches!(klass.format(), crate::oops::klass::Format::Slots) {
            return None;
        }
        let size_words = klass.non_indexable_size();
        // Header (2 words) .. the 12-bit immediate ceiling emit_alloc's own
        // debug_assert enforces; a giant class stays a generic send.
        if size_words < crate::oops::layout::HEADER_WORDS
            || size_words * crate::oops::layout::WORD_SIZE >= 4096
        {
            return None;
        }
        Some((klass, size_words as u32))
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
    /// Returns `Some(continuation_id)` exactly when this instruction just
    /// split the CURRENT block (a smi-inlined `SmiArith`/`SmiCmpVal` fail
    /// edge, S11 step 7's own `fail_and_continue`) — the caller (`convert`'s
    /// own per-CFG-block loop) must then finish its accumulating `code` vec
    /// with a `Jump { target: continuation_id }` and start a fresh one for
    /// the returned id. `None` for every other instruction, including
    /// `SmiCmpBr`'s own fused fail (handled entirely at the BRANCH
    /// terminator, which already has its own real targets — no split
    /// needed there) and the fast, non-failing smi-inline path.
    #[allow(clippy::too_many_arguments)]
    fn translate_instr(
        &mut self,
        instr: &Instr,
        fusable: bool,
        bci: usize,
        stack: &mut Vec<VReg>,
        code: &mut Vec<Ir>,
        deopt: &mut Vec<(u32, DeoptRaw)>,
        pending_cmp: &mut Option<(CmpOp, VReg, VReg, PendingCmpDeopt)>,
    ) -> Option<BlockId> {
        let mut split: Option<BlockId> = None;
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
                // S11 D7: if this global's CURRENT value is a class, remember
                // `dst` holds that class constant, so an immediately
                // following `X basicNew` send can compile to an inline
                // `Ir::Alloc` (`alloc_site_klass`). Reading the value here at
                // compile time is the deliberate S13-deferred staleness hole
                // (see `const_class`' own doc). The `LoadField` above still
                // emits — it's dead if the send turns into an `Alloc` (which
                // bakes the klass from the pool), a negligible cost a future
                // peephole could elide.
                let value = crate::oops::wrappers::MemOop::try_from(assoc)
                    .map(|a| a.body_oop(1))
                    .unwrap_or(assoc);
                if let Some(klass) = KlassOop::try_from(value) {
                    self.const_class.insert(dst.0, klass);
                }
                stack.push(dst);
            }
            Instr::Pop => {
                stack.pop().expect("pop: empty simulated stack");
            }
            Instr::Dup => {
                let tos = *stack.last().expect("dup: empty simulated stack");
                stack.push(tos);
            }
            Instr::Send { ic, super_ } if super_ => {
                // D4.6: the target METHOD is resolved at compile time
                // (`driver::compile_method`, after this whole IrMethod
                // comes back) via `holder.superclass()` — fixed here only
                // as far as WHICH klass to start that lookup from; method
                // holders don't move between klasses in v1, so this
                // klass itself never goes stale. Ordinary `Ir::CallSend`,
                // same as any other send site — `CallSiteInfo::
                // static_klass` is what tells `compile_method` to
                // pre-resolve and seed `Mono` instead of patching to
                // `stub_resolve` and starting `Unresolved`.
                let ic_view = InterpreterIc::at(self.method, ic);
                let selector = ic_view.selector();
                let real_argc = ic_view.argc();
                let mut real_args: Vec<VReg> = (0..real_argc)
                    .map(|_| stack.pop().expect("send_super: missing arg operand"))
                    .collect();
                real_args.reverse();
                let receiver = stack.pop().expect("send_super: missing receiver operand");
                let mut args = vec![receiver];
                args.append(&mut real_args);

                let holder = KlassOop::try_from(self.method.holder())
                    .expect("send_super: compiled method with no installed holder");
                let super_klass = KlassOop::try_from(holder.superclass())
                    .expect("send_super: holder's own superclass field is not a klass");

                let site = self.call_sites.len() as u16;
                self.call_sites.push(CallSiteInfo {
                    selector,
                    argc: real_argc + 1,
                    static_klass: Some(super_klass),
                });

                let dst = self.fresh(true);
                // S13 step 3b: a call-return safepoint (reexecute=false). The
                // receiver+args are already popped and `dst` is not yet
                // pushed, so `stack` is exactly the operand stack the deopt
                // materializer restores BELOW the send; it pushes the
                // incoming result itself. `code.len()` is the index the
                // CallSend is about to occupy (driver correlates it to the
                // emitted return-address safepoint).
                deopt.push((
                    code.len() as u32,
                    DeoptRaw {
                        stack: stack.clone(),
                        bci,
                        kind: SafepointKind::Call,
                        reexecute: false,
                    },
                ));
                code.push(Ir::CallSend { dst, site, args });
                stack.push(dst);
            }
            Instr::Send { ic, .. } if self.is_smi_inlinable(ic) => {
                // S13 step 7b: a smi-overflow deopt is `reexecute=true` at the
                // SEND's bci — the interpreter re-executes the WHOLE send, so
                // the recorded operand stack must be the state BEFORE this
                // op's own pops, with ITS INPUTS (`a`, `b`) still present.
                // Snapshot `stack` HERE, before the two pops below, so the
                // snapshot is exactly `[below..., a, b]` = the reexecute stack.
                let reexec_stack = stack.clone();
                let b = stack.pop().expect("send: missing arg operand");
                let a = stack.pop().expect("send: missing receiver operand");
                match classify_smi_send(self.vm, self.method, ic) {
                    SmiSendKind::Cmp(op) if fusable => {
                        // `fusable` already confirms the terminator is a
                        // Branch -- `convert` consumes `pending_cmp` there.
                        // Carry the reexecute snapshot AND this send's own bci
                        // to the branch site, which builds the fail block
                        // (after a/b are popped, and where `bci` is the block
                        // start, not this send).
                        *pending_cmp = Some((
                            op,
                            a,
                            b,
                            PendingCmpDeopt {
                                bci,
                                stack: reexec_stack,
                            },
                        ));
                    }
                    SmiSendKind::Cmp(op) => {
                        let dst = self.fresh(true);
                        let (fail_id, continuation_id) = self.fail_and_continue(bci, reexec_stack);
                        code.push(Ir::SmiCmpVal {
                            op,
                            dst,
                            a,
                            b,
                            fail: fail_id,
                        });
                        stack.push(dst);
                        split = Some(continuation_id);
                    }
                    SmiSendKind::Arith(op) => {
                        let dst = self.fresh(true);
                        let (fail_id, continuation_id) = self.fail_and_continue(bci, reexec_stack);
                        code.push(Ir::SmiArith {
                            op,
                            dst,
                            a,
                            b,
                            fail: fail_id,
                        });
                        stack.push(dst);
                        split = Some(continuation_id);
                    }
                }
            }
            // D1's "arbitrary send/send_w in ANY IC state": every send this
            // step's own smi-inline guard above doesn't handle (Poly, Mega,
            // mono-but-non-smi, mono-smi-but-not-inlinable) still compiles
            // now, as an ordinary generic `Ir::CallSend` -- no fast path
            // attempted, `stub_resolve` sorts out Unresolved/Mono/Pic/Mega
            // from here exactly like any other real send site.
            Instr::Send { ic, .. } => {
                // S11 D7: a known `X basicNew` (receiver is a compile-time
                // class constant, target is the basicNew primitive, X a
                // fixed-size Slots class) compiles to an inline `Ir::Alloc`
                // instead of a generic send. `stack.last()` is the receiver
                // exactly when the send takes no arguments -- which
                // `alloc_site_klass` requires (`ic.argc() == 0`) before it
                // ever consults `const_class`, so a non-zero-argc send whose
                // top-of-stack is an arg simply returns `None` here.
                if let Some(receiver) = stack.last().copied() {
                    if let Some((klass, size_words)) = self.alloc_site_klass(ic, receiver) {
                        // S13 step 3b: an `Alloc` deopts by RE-EXECUTING the
                        // `basicNew` send in the interpreter (reexecute=true),
                        // so its recorded stack must still carry the receiver
                        // (the class const) that the send consumes — capture
                        // BEFORE the pop below.
                        let deopt_stack = stack.clone();
                        stack.pop(); // consume the receiver
                        let klass_lit = self.pool.intern(klass.oop().raw(), Some(RelocKind::Oop));
                        let dst = self.fresh(true);
                        deopt.push((
                            code.len() as u32,
                            DeoptRaw {
                                stack: deopt_stack,
                                bci,
                                kind: SafepointKind::Alloc,
                                reexecute: true,
                            },
                        ));
                        code.push(Ir::Alloc {
                            dst,
                            klass: klass_lit,
                            size_words,
                        });
                        stack.push(dst);
                        return split;
                    }
                }

                let ic_view = InterpreterIc::at(self.method, ic);
                let real_argc = ic_view.argc();
                let mut real_args: Vec<VReg> = (0..real_argc)
                    .map(|_| stack.pop().expect("send: missing arg operand"))
                    .collect();
                real_args.reverse();
                let receiver = stack.pop().expect("send: missing receiver operand");
                let mut args = vec![receiver];
                args.append(&mut real_args);

                let site = self.call_sites.len() as u16;
                self.call_sites.push(CallSiteInfo {
                    selector: ic_view.selector(),
                    argc: real_argc + 1,
                    static_klass: None,
                });

                let dst = self.fresh(true);
                // S13 step 3b: call-return safepoint (reexecute=false), same
                // convention as the `send_super` site above — recorded stack
                // is the frame BELOW the send, result pushed by the deopt
                // materializer.
                deopt.push((
                    code.len() as u32,
                    DeoptRaw {
                        stack: stack.clone(),
                        bci,
                        kind: SafepointKind::Call,
                        reexecute: false,
                    },
                ));
                code.push(Ir::CallSend { dst, site, args });
                stack.push(dst);
            }
            Instr::JumpFwd(_) | Instr::JumpBack(_) | Instr::BrTrueFwd(_) | Instr::BrFalseFwd(_) => {
                // Deferred to `convert` (see this fn's own doc).
            }
            Instr::ReturnTos => {
                let val = stack.pop().expect("return_tos: empty simulated stack");
                code.push(Ir::Ret { val });
            }
            Instr::ReturnSelf => code.push(Ir::RetSelf),
            Instr::StoreInstvarPop(n) => {
                // Mirrors `PushInstvar`'s own `byte_off` exactly (D1's
                // store-relaxation is the write-side of that same field
                // access) -- `barrier: true` unconditionally: `val` could
                // be any heap oop at runtime regardless of what this
                // specific store site happens to see in practice, exactly
                // like `memory::store::store`'s own barrier, which never
                // skips the check based on the STATIC shape of a store.
                let val = stack
                    .pop()
                    .expect("store_instvar_pop: empty simulated stack");
                code.push(Ir::StoreField {
                    obj: self.self_vreg,
                    byte_off: (BODY_OFFSET + 8 * n as usize) as i32,
                    val,
                    barrier: true,
                });
            }
            Instr::StoreGlobalPop(idx) => {
                // Mirrors `PushGlobal`'s own association-value convention
                // exactly (word 1 = value, word 0 = key).
                let assoc = self.method.literals().at(idx as usize);
                let assoc_vreg = self.push_well_known(assoc.raw(), code);
                let val = stack
                    .pop()
                    .expect("store_global_pop: empty simulated stack");
                code.push(Ir::StoreField {
                    obj: assoc_vreg,
                    byte_off: (BODY_OFFSET + 8) as i32,
                    val,
                    barrier: true,
                });
            }
            Instr::PushCtxTemp { .. }
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
        split
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
    let (entry_depth, _max_stack_depth) = compute_entry_depths(method, cfg);
    let sources = entry_stack_sources(cfg, &entry_depth);

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
        next_extra_block: cfg.blocks.len() as u32,
        blocks_by_id: (0..cfg.blocks.len()).map(|_| None).collect(),
        call_sites: Vec::new(),
        const_class: HashMap::new(),
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
    // S11 D7: the pristine mark word an inline `Alloc` stamps into a fresh
    // `Format::Slots` object's header — a raw immediate (NOT a
    // `RelocKind::Oop`: it's a mark word, not a heap reference the GC
    // updates), matching `memory::alloc::init_object_at`'s own
    // `Mark::pristine().with_tagged_contents(true)`.
    let mark_slots_lit = t.pool.intern(
        crate::oops::mark::Mark::pristine()
            .with_tagged_contents(true)
            .word(),
        None,
    );

    let mut exit_stacks: Vec<Option<Vec<VReg>>> = vec![None; cfg.blocks.len()];

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
        // S13 step 3b: deopt sites accumulate alongside `code`, keyed by the
        // op's index in THIS code vec — `std::mem::take`n into the finished
        // IrBlock in lockstep with `code` at every split/finish so the
        // indices stay valid against the vec they name (a split resets both
        // to empty, and the continuation's own ops renumber from 0).
        let mut deopt: Vec<(u32, DeoptRaw)> = Vec::new();
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
        let mut pending_cmp: Option<(CmpOp, VReg, VReg, PendingCmpDeopt)> = None;
        // S11 step 7: a fallible smi-inline op mid-block (SmiArith/
        // SmiCmpVal, never a terminator) now needs a REAL fallback that
        // REJOINS this same block's own continuation, not a bailout-and-
        // restart -- so a single `cfg_block` can now emit SEVERAL `IrBlock`s
        // (this one, then one per split), not just one. `cur_id`/`cur_bci`
        // track whichever one is CURRENTLY accumulating into `code`;
        // finished ones are pushed to `ir_blocks` immediately, same as the
        // final one still is after this loop.
        let mut cur_id = block_id(b);
        let mut cur_bci = cfg_block.bci_start;
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
            let split = t.translate_instr(
                &instr,
                fusable,
                bci,
                &mut stack,
                &mut code,
                &mut deopt,
                &mut pending_cmp,
            );
            if let Some(continuation_id) = split {
                code.push(Ir::Jump {
                    target: continuation_id,
                });
                t.finish_block(IrBlock {
                    id: cur_id,
                    bci: cur_bci,
                    code: std::mem::take(&mut code),
                    entry_stack: if cur_id == block_id(b) {
                        entry_stack.clone()
                    } else {
                        Vec::new() // a split-off continuation's own entry_stack is never consulted (only real merge points are)
                    },
                    deopt_sites: std::mem::take(&mut deopt),
                });
                cur_id = continuation_id;
                cur_bci = next;
            }
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
                if let Some((op, a, b_, deopt_info)) = pending_cmp.take() {
                    // S13 step 7b: `deopt_info` was captured at the fused
                    // comparison's own smi-inline site (before its operand
                    // pops) — `deopt_info.stack` carries `a`/`b`, and
                    // `deopt_info.bci` is the comparison SEND's own bci (NOT
                    // `cfg_block.bci_start`, which is the block's first
                    // instruction) — the reexecute resume point.
                    let fail_id = t.fail_and_branch(deopt_info.bci, deopt_info.stack);
                    code.push(Ir::SmiCmpBr {
                        op,
                        a,
                        b: b_,
                        if_true: block_id(if_true),
                        if_false: block_id(if_false),
                        fail: fail_id,
                    });
                } else {
                    let val = *local_exit
                        .last()
                        .expect("branch terminator with an empty simulated stack (compiler bug)");
                    let not_bool_id =
                        t.fresh_not_bool_block(val, block_id(if_true), block_id(if_false));
                    code.push(Ir::BoolBr {
                        val,
                        if_true: block_id(if_true),
                        if_false: block_id(if_false),
                        not_bool: not_bool_id,
                    });
                }
            }
        }

        exit_stacks[b] = Some(local_exit);
        t.finish_block(IrBlock {
            id: cur_id,
            bci: cur_bci,
            code,
            entry_stack: if cur_id == block_id(b) {
                entry_stack
            } else {
                Vec::new()
            },
            deopt_sites: deopt,
        });
    }

    // Indexed by id (`finish_block`'s own doc), never append order --
    // required so `emit.rs`'s own `method.blocks[bid.0 as usize]` (and its
    // parallel per-index `labels` vec) actually find the right block.
    let ir_blocks: Vec<IrBlock> = t
        .blocks_by_id
        .into_iter()
        .enumerate()
        .map(|(i, slot)| {
            slot.unwrap_or_else(|| {
                panic!("block {i} was allocated an id but never finished (compiler bug)")
            })
        })
        .collect();

    // S13: if this method has any deopt site, intern its own compiled
    // `MethodOop` into the pool (as an `Oop` reloc, so `oops_do`/GC keeps the
    // pool word current) so the deopt materializer can name the method for a
    // reconstructed frame's `FRAME_METHOD` slot. Interned LAST (highest pool
    // index) — existing `PoolLit` references keep their indices — and only
    // when needed, so an all-smi-inline method's pool (and its listing golden)
    // is untouched.
    let method_pool_ix = if ir_blocks.iter().any(|b| !b.deopt_sites.is_empty()) {
        Some(t.pool.intern(method.oop().raw(), Some(RelocKind::Oop)).0)
    } else {
        None
    };

    IrMethod {
        blocks: ir_blocks,
        vregs: t.vregs,
        pool: t.pool.entries,
        argc: method.argc() as u8,
        ntemps: method.ntemps() as u8,
        safepoints: Vec::new(),
        true_lit,
        false_lit,
        nil_lit,
        mark_slots_lit,
        call_sites: t.call_sites,
        method_pool_ix,
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
    /// to `SmiArith{Add}`. S13 step 7b: `fail` is now a single-op
    /// `Ir::UncommonTrap` deopt block (a `brk` re-executes the send in the
    /// interpreter — its LargeInteger/Double fallback), carrying ONE
    /// reexecute deopt site whose recorded stack holds the send's own inputs
    /// (`a`, `b`). No `CallSend`, no jump, no `Ir::Bailout`.
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

        let (fail, a, b_) = ir.blocks[0]
            .code
            .iter()
            .find_map(|op| match op {
                Ir::SmiArith {
                    op: SmiOp::Add,
                    fail,
                    a,
                    b,
                    ..
                } => Some((*fail, *a, *b)),
                _ => None,
            })
            .expect("send site must translate to SmiArith{Add}");
        let fail_block = ir
            .blocks
            .iter()
            .find(|b| b.id == fail)
            .expect("fail must name a real synthesized block");
        // S13 step 7b: the fail block is a single-op deopt trap.
        assert!(
            matches!(fail_block.code[..], [Ir::UncommonTrap { .. }]),
            "the fail block must be a single Ir::UncommonTrap, got {:?}",
            fail_block.code
        );
        // ONE reexecute deopt site, keyed at the trap op (code index 0), whose
        // recorded stack holds the send's own inputs `a`,`b` (the reexecute
        // stack `[below.., a, b]`; here `below` is empty).
        assert_eq!(fail_block.deopt_sites.len(), 1);
        let (ci, raw) = &fail_block.deopt_sites[0];
        assert_eq!(*ci, 0);
        assert!(raw.reexecute);
        assert_eq!(raw.kind, SafepointKind::UncommonTrap);
        assert_eq!(raw.stack, vec![a, b_]);
        // No CallSend/Bailout anywhere in the smi fail path anymore.
        assert!(
            !ir.blocks
                .iter()
                .any(|b| b.code.iter().any(|op| matches!(op, Ir::Bailout { .. }))),
            "Ir::Bailout must never be constructed by convert()"
        );
        assert!(
            !fail_block
                .code
                .iter()
                .any(|op| matches!(op, Ir::CallSend { .. })),
            "S13 step 7b: the smi fail block no longer emits a CallSend fallback"
        );
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

        // 2 real blocks (push+jump, then the target) -- S11 step 7: no
        // extra synthetic block anymore unless one is actually NEEDED (a
        // fallible smi-inline op or a Boolean branch), and this method's
        // bytecode has neither.
        assert_eq!(ir.blocks.len(), 2);
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
