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

/// S14 step 4b: which receiver-klass test an [`Ir::GuardKlass`] lowers to
/// (mirrors `inline::GuardKind`'s `SmiTest`/`KlassTest`, minus its `None` — a
/// `None` guard emits no `GuardKlass` op at all). Kept as its own tiny enum so
/// `emit.rs` needn't depend on `compiler::inline`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardShape {
    /// `tst rcvr, #3; b.ne cold` — the receiver must be a smi (2-bit tag 00).
    SmiTest,
    /// reject-smi, load the klass word, compare against the expected klass
    /// literal, `b.ne cold` — the receiver must be a heap oop of that klass.
    KlassTest,
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
    /// S14 step 4b: a receiver-klass guard in front of an inlined leaf body.
    /// On a MATCH, control falls through to the inlined body; on a MISMATCH it
    /// branches to `fail` — a cold block that is a single `Ir::UncommonTrap`
    /// (reexecute=true at the inlined send's own bci), so a wrong receiver
    /// deopts and re-executes the send generically (S14 step-3 trap shape).
    /// `kind` selects the test emit.rs lowers: `SmiTest` (`tst rcvr,#3; b.ne`)
    /// when the speculated klass is the smi klass, `KlassTest`
    /// (reject-smi-then-compare-klass-word against `expect`) otherwise.
    /// `expect` is the expected klassOop, interned as a `RelocKind::Oop` pool
    /// literal (a moving GC keeps it current) — read only by `KlassTest`;
    /// `SmiTest` ignores it. This op is NOT itself a regalloc safepoint (it is
    /// a pure test); the safepoint that spills the reexecute stack is the
    /// `UncommonTrap` in the `fail` block.
    GuardKlass {
        obj: VReg,
        expect: PoolLit,
        fail: BlockId,
        kind: GuardShape,
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
    /// S14 step 4c: `Some` iff this safepoint lives INSIDE an inlined callee's
    /// body (a `CallSend`/trap the non-leaf splicer emitted). The driver reads
    /// it to record a NESTED deopt scope — the inlined callee's own receiver/
    /// slots/method + a `SenderLink` back to the caller — instead of the root
    /// method's depth-1 scope. `None` for every root-method safepoint (S13's
    /// exact behaviour, `sender: None`, so no regression).
    pub inline: Option<InlineSite>,
}

/// S14 step 4c: the per-safepoint description of an inlined callee's virtual
/// frame — everything `driver::build_deopt_metadata` needs to record a nested
/// deopt scope for a safepoint that lives inside a spliced non-leaf body. Every
/// `VReg` here resolves to a canonical [`crate::compiler::scopes::ValueLoc::
/// FrameSlot`] at the site's position (S12 spill-all), exactly like the root
/// scope's own vregs — so the depth-2 chain reuses the depth-1 location format
/// with zero new work.
#[derive(Clone, Debug)]
pub struct InlineSite {
    /// The INLINED callee's own compiled `MethodOop` pool index (distinct from
    /// the root `IrMethod.method_pool_ix`) — the reconstructed inlined frame's
    /// `FRAME_METHOD`.
    pub method_pool_ix: u32,
    /// The callee's `self` vreg (the inlined send's receiver operand).
    pub receiver: VReg,
    /// The callee's unified arg/temp slot vregs (`0..argc+ntemps`) — args alias
    /// the send's operand vregs, temps are the fresh nil vregs the splice mints.
    pub slots: Vec<VReg>,
    /// bci of the inlined send in the CALLER (the root method) — the
    /// `SenderLink.sender_bci`; the caller frame resumes at `sender_bci +
    /// len(send)`.
    pub sender_bci: u16,
    /// The CALLER's operand stack BELOW the inlined send's receiver+args, frozen
    /// for the whole inlined extent (the `SenderLink.pending_stack`).
    pub caller_pending_stack: Vec<VReg>,
    /// S14 step 7-I: `true` iff the inlined callee is a spliced literal BLOCK
    /// (its reconstructed scope is an `is_block` scope — the deopt materializer
    /// pushes a `CompiledBlock` activation frame, not a method frame). `false`
    /// for a step-4c method inline. `driver::build_deopt_metadata` stamps the
    /// inlined scope's `ScopeDescData.is_block` from this.
    pub is_block: bool,
    /// S14 step 7-IV-b: the ENCLOSING inline level, when this callee was itself
    /// spliced inside another inlined extent (a block spliced inside an inlined
    /// `do:`-style callee — depth 3: block ← callee ← root). `None` = this
    /// callee's caller IS the root method (depth 2, every pre-7-IV shape). For
    /// a `parent` level, `sender_bci`/`caller_pending_stack` describe the send
    /// in THAT level's method, not the root. `build_deopt_metadata` walks the
    /// chain outermost-first when beginning scopes.
    pub parent: Option<Box<InlineSite>>,
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

/// S14 step 4c: the result of [`Translator::try_inline_nonleaf`] — either the
/// body ran to its return (`Value`), or an in-body cold send became a
/// terminating uncommon trap (`Trapped`), so the caller must finish the current
/// block here (nothing after the inlined send is reachable).
enum NonLeafOutcome {
    Value(VReg),
    Trapped,
}

pub struct IrMethod {
    pub blocks: Vec<IrBlock>,
    pub vregs: Vec<VRegInfo>,
    pub pool: Vec<PoolEntry>,
    pub argc: u8,
    pub ntemps: u8,
    /// S14 step 7-II-b: the vregs holding M's promoted captured temps (one per
    /// ctx-slot) when M's heap Context is elided; empty otherwise.
    /// `build_deopt_metadata` records `CtxLoc::Elided` over their frame slots so
    /// a deopt rebuilds a real Context.
    pub ctx_vregs: Vec<VReg>,
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
    /// S14 step 2 (A1 feedback annotation): the `SiteFeedback` observed for each
    /// `call_sites` entry, SAME index — the receiver types the interpreter saw
    /// at that send, read from its IC by `feedback::read_send_site` during
    /// `convert`. Pure annotation in this step: the inliner (later steps) reads
    /// it to decide speculate/inline/trap; emit ignores it (no behaviour change,
    /// so listing goldens stay byte-stable). Kept parallel to `call_sites`
    /// rather than a field on `CallSiteInfo` because `SiteFeedback` is not `Copy`
    /// (its `Poly` arm owns a `Vec`) and `CallSiteInfo` is.
    pub site_feedback: Vec<crate::compiler::feedback::SiteFeedback>,
    /// S14 step 4b: one `(receiver_klass, selector)` pair per INLINED leaf
    /// body — the guard's assumption (`lookup(receiver_klass, selector)`
    /// resolves to the callee actually spliced). `driver::compile_method`
    /// copies this verbatim into `Nmethod.inline_deps`, which `deps::
    /// affected_by_install` consults so redefining an inlined callee makes the
    /// caller nmethod `NotEntrant`. Empty for a method that inlined nothing
    /// (keeps such methods' behaviour and goldens unchanged).
    pub inline_deps: Vec<(KlassOop, SymbolOop)>,
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
    /// S14 step 7-II-b: one vreg per M ctx-slot (`method.nctx()`) — the promoted
    /// homes of M's captured temps when M's heap Context is ELIDED (every block
    /// capturing them is inlined). `push_ctx_temp{depth:0,idx}` /
    /// `store_ctx_temp_pop{depth:0,idx}` in M's own body AND in a spliced
    /// (ctx-less, `captures_ctx`) block both address `ctx_vregs[idx]` — a
    /// ctx-less block's frame aliases M's Context, so it reaches M's ctx-temps at
    /// depth 0. Empty when M is not `has_ctx`. Nil-initialised at method entry
    /// (matching `alloc_context`'s nil-fill). The deopt root scope records
    /// `CtxLoc::Elided` over these so the materializer rebuilds a real Context.
    ctx_vregs: Vec<VReg>,
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
    /// S14 step 2: `SiteFeedback` per `call_sites` entry (same index), read from
    /// each send's IC as it is lowered. See `IrMethod::site_feedback`.
    site_feedback: Vec<crate::compiler::feedback::SiteFeedback>,
    /// S14 step 4b: one `(receiver_klass, selector)` per INLINED leaf body — the
    /// dependency the guard assumes (`lookup(receiver_klass, selector)` resolves
    /// to the callee actually spliced). Copied into `IrMethod.inline_deps`; the
    /// driver copies THAT into `Nmethod.inline_deps`, and `deps::
    /// affected_by_install` invalidates this nmethod if the selector is
    /// redefined anywhere on the receiver klass's superchain.
    inline_deps: Vec<(KlassOop, SymbolOop)>,
    /// S14 step 4b: recompilation level driving the inline budget
    /// (`inline::budget_for_level`). Tier-1 compiles are always level 1 today
    /// (`driver::compile_method` sets `level: 1`); threaded here so the budget
    /// tracks it when higher levels arrive.
    level: u8,
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
    /// S14 step 7-I: the escape pre-pass result for M, `Some` iff M creates a
    /// literal closure (`push_closure`) that was proven elidable
    /// (`driver::eligibility_detail` guarantees `all_elidable` before compiling,
    /// so this is only ever `Some` for a method whose EVERY closure site is a
    /// good value-send use). `translate_instr` consults `value_send_target(bci)`
    /// to decide whether a `Send` splices a block body inline.
    escape: Option<crate::compiler::escape::ClosureEscape>,
    /// S14 step 7-I: parallel to `temp_vregs` — `temp_sites[t] == Some(bci)`
    /// means the unified arg/temp slot `t` currently holds the block created at
    /// `push_closure` site `bci` (a phantom value — no real vreg backs it). A
    /// `store_temp_pop` of a block-site stack slot sets this; a `push_temp` of a
    /// block-site temp slot reads it (pushing a phantom onto the operand
    /// `stack_sites` shadow), so a `b := [..]. ^b value` splices without ever
    /// materialising the closure. Tracked linearly as `convert` walks blocks in
    /// bci order; the escape pre-pass already guaranteed no block-site ever
    /// merges lossily (that would have made M `NoPermanent`), so linear tracking
    /// suffices — a site never reaches a value-send by a path convert can't see.
    temp_sites: Vec<Option<usize>>,
    /// S14 step 7-IV-a: when a `Send` arm spliced a MULTI-BLOCK callee
    /// (`try_inline_cfg`), the caller's current block must end with a `Jump`
    /// into the callee's ENTRY block — not the continuation the arm returns as
    /// `split`. The arm sets this; `convert`'s split handling `take()`s it as
    /// the Jump target (falling back to the continuation when unset — the
    /// ordinary smi-split case).
    pending_jump_target: Option<BlockId>,
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
                    inline: None,
                },
            )],
        });
        (fail_id, continuation_id)
    }

    /// S13 step 7b-ii: a `BoolBr.not_bool` edge (the value branched on wasn't
    /// `true`/`false`) DEOPTS — a single `Ir::UncommonTrap{ bci: branch_bci }`,
    /// reexecute=true at the branch opcode. The interpreter re-executes the
    /// branch, sees the non-boolean, and runs its own `must_be_boolean_send`
    /// (SPEC §5.4 Alg 11: push the value back, roll bci to the branch, send
    /// `#mustBeBoolean`, re-test) — so the compiled path stops carrying a
    /// `CallRuntime{MUST_BE_BOOLEAN}` self-loop and defers the whole
    /// (rare, cold) protocol to the interpreter. `reexec_stack` is the
    /// operand stack BEFORE the branch (the value being tested still on top,
    /// since the deferred branch never popped it) — exactly what reexecute
    /// needs. `regalloc::deopt_live` forces every vreg this site reads
    /// (receiver + slots + `reexec_stack`) live-across the trap so it spills.
    fn fresh_not_bool_block(&mut self, branch_bci: usize, reexec_stack: Vec<VReg>) -> BlockId {
        let not_bool_id = self.fresh_block_id();
        self.finish_block(IrBlock {
            id: not_bool_id,
            bci: branch_bci,
            code: vec![Ir::UncommonTrap { bci: branch_bci }],
            entry_stack: Vec::new(),
            deopt_sites: vec![(
                0,
                DeoptRaw {
                    stack: reexec_stack,
                    bci: branch_bci,
                    kind: SafepointKind::UncommonTrap,
                    reexecute: true,
                    inline: None,
                },
            )],
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
                    inline: None,
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
    /// IC state falls to the generic `Instr::Send` arm: S14 step 3 an `Empty`
    /// site now CAN reach a real compile (its guard isn't the smi klass, so
    /// this returns `false`) and lowers there to an uncommon TRAP via
    /// `inline::decide` (`SiteFeedback::Untaken`); Poly, Mega, mono-but-non-smi,
    /// and mono-smi-but-not-inlinable lower to an ordinary generic
    /// `Ir::CallSend`.
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

    /// S14 step 4b: splice a **single-block straight-line leaf** callee inline.
    ///
    /// `receiver`/`args` are the send's OPERAND vregs (already popped by the
    /// caller); they alias the callee's `self`/argument slots directly, no
    /// stores (A2 scope-splicing rule: "callee args map to the vregs holding the
    /// send's operands"). `guard_klass` is the observed receiver klass whose
    /// `expect`-literal the guard compares against, `guard_shape` the test to
    /// emit. `selector` is the send's own selector (for the inline dependency).
    ///
    /// Returns `Some(result_vreg)` — the vreg holding the inlined body's return
    /// value, which the caller pushes onto its operand stack — after appending
    /// the guard + spliced body to `code`, minting the cold trap block (and its
    /// reexecute deopt site), and recording the inline dependency. Returns
    /// `None` (splice DECLINED — the caller falls back to a plain `Call`) when
    /// the callee is not a single straight-line block ending in a return, or
    /// contains an opcode this narrow step doesn't splice.
    ///
    /// No safepoint is recorded for the body: a leaf has no send / no smi-inline
    /// overflow trap / no alloc, so no deopt can occur inside it (the ONLY deopt
    /// is the guard's own cold trap, which lives in the `fail` block). The
    /// caller therefore records NO `CallSiteInfo`/`site_feedback`/`IcSite` for
    /// this site.
    #[allow(clippy::too_many_arguments)]
    fn try_inline_leaf(
        &mut self,
        callee: MethodOop,
        guard_klass: KlassOop,
        guard_shape: GuardShape,
        selector: SymbolOop,
        receiver: VReg,
        args: &[VReg],
        send_bci: usize,
        pre_pop_stack: &[VReg],
        code: &mut Vec<Ir>,
    ) -> Option<VReg> {
        // MINIMUM viable scope: the callee must be a SINGLE straight-line block
        // ending in a return, and its arity must match what we popped. A
        // multi-block leaf (a branch/loop with no sends) or an arity mismatch is
        // out of this narrow step's scope — decline, and the caller does a plain
        // Call. Validate against the CFG BEFORE emitting anything.
        let callee_cfg = crate::compiler::decode::decode(callee);
        if callee_cfg.blocks.len() != 1
            || !matches!(
                callee_cfg.blocks[0].terminator,
                crate::compiler::decode::Terminator::Return
            )
            || callee.argc() != args.len()
        {
            return None;
        }
        // DRY-RUN: confirm every callee opcode is one this narrow step splices
        // BEFORE emitting the guard/cold block/body — a mid-splice decline would
        // leave a guard dominating a half-built body while the caller falls back
        // to a `Call`, corrupting the operand-stack discipline. If any opcode is
        // unsupported, decline cleanly here having emitted NOTHING.
        if !leaf_body_is_spliceable(callee) {
            return None;
        }

        // The callee's unified arg/temp slots: args 0..argc alias the send's
        // operand vregs (NO stores — A2's scope-splicing rule); the callee's own
        // temps argc..argc+ntemps become fresh nil vregs. `callee_self` is the
        // send's receiver vreg.
        let callee_self = receiver;
        let mut callee_slots: Vec<VReg> = Vec::with_capacity(callee.argc() + callee.ntemps());
        callee_slots.extend_from_slice(args);

        // Emit the guard FIRST so it dominates the whole spliced body. On a
        // klass MISMATCH it branches to `cold_id` (a single-op UncommonTrap,
        // reexecute=true at the send's own bci, recorded stack = the operand
        // stack BEFORE the send's pops, so receiver + args are still present) —
        // identical to the S14 step-3 trap: a wrong receiver deopts and
        // re-executes the send generically. On a MATCH it falls through into the
        // body appended below (same block, no branch — the leaf is straight
        // line). `expect` is the observed klass, a moving-GC-tracked Oop lit
        // (read only by KlassTest; SmiTest ignores it).
        let expect = self
            .pool
            .intern(guard_klass.oop().raw(), Some(RelocKind::Oop));
        let cold_id = self.fresh_block_id();
        self.finish_block(IrBlock {
            id: cold_id,
            bci: send_bci,
            code: vec![Ir::UncommonTrap { bci: send_bci }],
            entry_stack: Vec::new(),
            deopt_sites: vec![(
                0,
                DeoptRaw {
                    stack: pre_pop_stack.to_vec(),
                    bci: send_bci,
                    kind: SafepointKind::UncommonTrap,
                    reexecute: true,
                    inline: None,
                },
            )],
        });
        code.push(Ir::GuardKlass {
            obj: callee_self,
            expect,
            fail: cold_id,
            kind: guard_shape,
        });

        // Fresh nil temps for the callee's own locals (after the guard, so a
        // wrong-klass receiver traps before any body op runs).
        let nil_lit = self
            .pool
            .intern(self.vm.universe.nil_obj.raw(), Some(RelocKind::Oop));
        for _ in 0..callee.ntemps() {
            let t = self.fresh(true);
            code.push(Ir::ConstPool {
                dst: t,
                lit: nil_lit,
            });
            callee_slots.push(t);
        }

        // Translate the callee's straight-line body onto a fresh operand stack.
        // A leaf has no send/smi-inline/alloc, so only value-shuffling and
        // field/return opcodes can appear. `return_tos` → the send's result is
        // the callee's TOS; `return_self` → the receiver vreg. The `expect`
        // (single-block Return terminator) means exactly one return terminates.
        let mut cstack: Vec<VReg> = Vec::new();
        let mut result: Option<VReg> = None;
        let len = callee.bytecode_len();
        let mut bci = 0;
        while bci < len {
            let (instr, next) = decode_at(callee, bci);
            match instr {
                Instr::PushSelf => cstack.push(callee_self),
                Instr::PushNil => {
                    cstack.push(self.push_well_known(self.vm.universe.nil_obj.raw(), code))
                }
                Instr::PushTrue => {
                    cstack.push(self.push_well_known(self.vm.universe.true_obj.raw(), code))
                }
                Instr::PushFalse => {
                    cstack.push(self.push_well_known(self.vm.universe.false_obj.raw(), code))
                }
                Instr::PushSmi(v) => {
                    let dst = self.fresh(true);
                    code.push(Ir::ConstSmi {
                        dst,
                        value: v as i64,
                    });
                    cstack.push(dst);
                }
                Instr::PushLiteral(idx) => {
                    let lit_oop = callee.literals().at(idx as usize);
                    cstack.push(self.push_well_known(lit_oop.raw(), code));
                }
                Instr::PushGlobal(idx) => {
                    let assoc = callee.literals().at(idx as usize);
                    let assoc_vreg = self.push_well_known(assoc.raw(), code);
                    let dst = self.fresh(true);
                    code.push(Ir::LoadField {
                        dst,
                        obj: assoc_vreg,
                        byte_off: (BODY_OFFSET + 8) as i32,
                    });
                    cstack.push(dst);
                }
                Instr::PushTemp(n) => {
                    // Fresh copy (same v1 rule as the main path): the slot may be
                    // reassigned later on this path.
                    let dst = self.fresh(true);
                    code.push(Ir::Move {
                        dst,
                        src: callee_slots[n as usize],
                    });
                    cstack.push(dst);
                }
                Instr::StoreTemp(n) => {
                    let tos = *cstack
                        .last()
                        .expect("inline splice: store_temp on empty stack");
                    code.push(Ir::Move {
                        dst: callee_slots[n as usize],
                        src: tos,
                    });
                }
                Instr::StoreTempPop(n) => {
                    let v = cstack
                        .pop()
                        .expect("inline splice: store_temp_pop on empty stack");
                    code.push(Ir::Move {
                        dst: callee_slots[n as usize],
                        src: v,
                    });
                }
                Instr::PushInstvar(n) => {
                    let dst = self.fresh(true);
                    code.push(Ir::LoadField {
                        dst,
                        obj: callee_self,
                        byte_off: (BODY_OFFSET + 8 * n as usize) as i32,
                    });
                    cstack.push(dst);
                }
                Instr::StoreInstvarPop(n) => {
                    let val = cstack
                        .pop()
                        .expect("inline splice: store_instvar_pop on empty stack");
                    code.push(Ir::StoreField {
                        obj: callee_self,
                        byte_off: (BODY_OFFSET + 8 * n as usize) as i32,
                        val,
                        barrier: true,
                    });
                }
                Instr::Pop => {
                    cstack.pop().expect("inline splice: pop on empty stack");
                }
                Instr::Dup => {
                    let tos = *cstack.last().expect("inline splice: dup on empty stack");
                    cstack.push(tos);
                }
                Instr::ReturnTos => {
                    result = Some(
                        cstack
                            .pop()
                            .expect("inline splice: return_tos on empty stack"),
                    );
                    break;
                }
                Instr::ReturnSelf => {
                    result = Some(callee_self);
                    break;
                }
                // Pre-validated by `leaf_body_is_spliceable` above — no opcode
                // outside the supported set (or the multi-block case) reaches
                // here, so a decline past the guard's emission is impossible.
                other => unreachable!(
                    "inline splice: {other:?} passed leaf_body_is_spliceable but has no arm"
                ),
            }
            bci = next;
        }
        let result = result.expect("inline splice: single-return leaf must produce a result");

        self.record_inline_dep(guard_klass, selector);
        Some(result)
    }

    /// S14 step 4c: splice a **single-block straight-line NON-leaf** callee
    /// inline — a callee that HAS its own sends (so it has in-body safepoints
    /// and can deopt), extending [`try_inline_leaf`] with a `Send` arm. The
    /// crux of step 4c: each in-body safepoint records a NESTED deopt scope
    /// (the inlined callee's own receiver/slots/method + a `SenderLink` back to
    /// the caller), so a deopt at one of them rebuilds BOTH the inlined frame
    /// AND the caller frame from the ONE physical compiled frame — the first
    /// time the depth-N materializer runs at depth > 1.
    ///
    /// Same operand/scope-splicing rules as the leaf splice: `receiver`/`args`
    /// alias the send's operand vregs (no stores), temps become fresh nil vregs
    /// with canonical spill homes (S12 spill-all → every entity has a frame slot
    /// the deopt materializer reads via `ValueLoc::FrameSlot`). The KEY new
    /// invariant this builds: the `InlineSite` on each in-body `DeoptRaw`
    /// carries `sender_bci` (the inlined send's bci in the CALLER) and
    /// `caller_pending_stack` (the caller's operand stack BELOW the send's
    /// receiver+args) — exactly what M6 uses to rebuild the caller frame's
    /// resume point + operand stack.
    ///
    /// Appends the guard + spliced body to `code` and records in-body deopt
    /// safepoints into `deopt`. Returns:
    ///   - `Some(NonLeafOutcome::Value(vreg))` — the body ran to its return; the
    ///     caller pushes `vreg` and continues,
    ///   - `Some(NonLeafOutcome::Trapped)` — an in-body send was cold (Untaken)
    ///     and became a terminating `UncommonTrap`; the caller must finish the
    ///     current block here (set `trapped`), nothing after is reachable,
    ///   - `None` — the callee is not the single straight-line shape this
    ///     splices (the caller falls back to a plain `Call`).
    #[allow(clippy::too_many_arguments)]
    fn try_inline_nonleaf(
        &mut self,
        callee: MethodOop,
        guard_klass: KlassOop,
        guard_shape: GuardShape,
        selector: SymbolOop,
        receiver: VReg,
        args: &[VReg],
        send_bci: usize,
        pre_pop_stack: &[VReg],
        code: &mut Vec<Ir>,
        deopt: &mut Vec<(u32, DeoptRaw)>,
    ) -> Option<NonLeafOutcome> {
        // Same MINIMUM viable shape as the leaf splice: a single straight-line
        // block ending in a return, arity matching. Validate against the CFG
        // BEFORE emitting anything. `is_inline_eligible_nonleaf` already proved
        // the single-block/return/no-super shape at decision time, but the
        // arity check is site-specific, so re-confirm here.
        let callee_cfg = crate::compiler::decode::decode(callee);
        if callee_cfg.blocks.len() != 1
            || !matches!(
                callee_cfg.blocks[0].terminator,
                crate::compiler::decode::Terminator::Return
            )
            || callee.argc() != args.len()
        {
            return None;
        }

        // The inlined callee's own compiled MethodOop, interned (idempotent,
        // deduped) into the SAME pool as the root method's — the reconstructed
        // inlined frame's FRAME_METHOD names THIS method, not the caller's.
        let callee_pool_ix = self.pool.intern(callee.oop().raw(), Some(RelocKind::Oop)).0;

        // The caller's operand stack BELOW the inlined send's receiver+args:
        // frozen for the whole inlined extent as the `SenderLink.pending_stack`.
        // `pre_pop_stack` is `[...below, receiver, arg0, ..]`; strip the
        // receiver (1) + args (argc) off the top.
        let n_operands = args.len() + 1;
        debug_assert!(
            pre_pop_stack.len() >= n_operands,
            "inline splice: pre-pop stack shorter than the send's operands"
        );
        let caller_pending_stack = pre_pop_stack[..pre_pop_stack.len() - n_operands].to_vec();

        // Callee slots: args alias the send operands (no stores); temps become
        // fresh nil vregs. `receiver` is the callee's `self`.
        let callee_self = receiver;
        let mut callee_slots: Vec<VReg> = Vec::with_capacity(callee.argc() + callee.ntemps());
        callee_slots.extend_from_slice(args);

        // The guard fronts the whole thing, exactly as the leaf splice: on a
        // klass MISMATCH branch to a single-op reexecute trap (re-executes the
        // send generically); on a MATCH fall through into the body.
        let expect = self
            .pool
            .intern(guard_klass.oop().raw(), Some(RelocKind::Oop));
        let cold_id = self.fresh_block_id();
        self.finish_block(IrBlock {
            id: cold_id,
            bci: send_bci,
            code: vec![Ir::UncommonTrap { bci: send_bci }],
            entry_stack: Vec::new(),
            deopt_sites: vec![(
                0,
                DeoptRaw {
                    stack: pre_pop_stack.to_vec(),
                    bci: send_bci,
                    kind: SafepointKind::UncommonTrap,
                    reexecute: true,
                    // The guard's cold trap re-executes the WHOLE `helper:` send
                    // in the CALLER, so it deopts to the CALLER scope (depth-1,
                    // `inline: None`) — NOT the inlined scope. Only safepoints
                    // INSIDE the body carry an `InlineSite`.
                    inline: None,
                },
            )],
        });
        code.push(Ir::GuardKlass {
            obj: callee_self,
            expect,
            fail: cold_id,
            kind: guard_shape,
        });

        // Fresh nil temps for the callee's own locals (after the guard).
        let nil_lit = self
            .pool
            .intern(self.vm.universe.nil_obj.raw(), Some(RelocKind::Oop));
        for _ in 0..callee.ntemps() {
            let t = self.fresh(true);
            code.push(Ir::ConstPool {
                dst: t,
                lit: nil_lit,
            });
            callee_slots.push(t);
        }

        // Build the InlineSite prototype (shared shape for every in-body
        // safepoint — the slots/receiver/method/sender are constant across the
        // extent; only the operand `stack` varies per site). `sender_bci` is
        // the CALLER's bci of the inlined send.
        let inline_proto = InlineSite {
            method_pool_ix: callee_pool_ix,
            receiver: callee_self,
            slots: callee_slots.clone(),
            sender_bci: send_bci as u16,
            caller_pending_stack: caller_pending_stack.clone(),
            // A step-4c METHOD inline, not a spliced block.
            is_block: false,
            parent: None,
        };

        // Translate the callee's straight-line body onto a fresh operand stack.
        // Value-shuffle / field / return opcodes mirror the leaf splice; the
        // NEW arm is `Send` — a compiled `CallSend` (or a step-3 trap for a cold
        // IC) recording an in-body deopt scope. Do NOT recurse into the inlinee
        // (depth-1 this step): a nested send stays a plain `Call`.
        let mut cstack: Vec<VReg> = Vec::new();
        let mut result: Option<VReg> = None;
        let mut trapped = false;
        let len = callee.bytecode_len();
        let mut bci = 0;
        while bci < len {
            let (instr, next) = decode_at(callee, bci);
            match instr {
                Instr::PushSelf => cstack.push(callee_self),
                Instr::PushNil => {
                    cstack.push(self.push_well_known(self.vm.universe.nil_obj.raw(), code))
                }
                Instr::PushTrue => {
                    cstack.push(self.push_well_known(self.vm.universe.true_obj.raw(), code))
                }
                Instr::PushFalse => {
                    cstack.push(self.push_well_known(self.vm.universe.false_obj.raw(), code))
                }
                Instr::PushSmi(v) => {
                    let dst = self.fresh(true);
                    code.push(Ir::ConstSmi {
                        dst,
                        value: v as i64,
                    });
                    cstack.push(dst);
                }
                Instr::PushLiteral(idx) => {
                    let lit_oop = callee.literals().at(idx as usize);
                    cstack.push(self.push_well_known(lit_oop.raw(), code));
                }
                Instr::PushGlobal(idx) => {
                    let assoc = callee.literals().at(idx as usize);
                    let assoc_vreg = self.push_well_known(assoc.raw(), code);
                    let dst = self.fresh(true);
                    code.push(Ir::LoadField {
                        dst,
                        obj: assoc_vreg,
                        byte_off: (BODY_OFFSET + 8) as i32,
                    });
                    cstack.push(dst);
                }
                Instr::PushTemp(n) => {
                    let dst = self.fresh(true);
                    code.push(Ir::Move {
                        dst,
                        src: callee_slots[n as usize],
                    });
                    cstack.push(dst);
                }
                Instr::StoreTemp(n) => {
                    let tos = *cstack
                        .last()
                        .expect("inline splice: store_temp on empty stack");
                    code.push(Ir::Move {
                        dst: callee_slots[n as usize],
                        src: tos,
                    });
                }
                Instr::StoreTempPop(n) => {
                    let v = cstack
                        .pop()
                        .expect("inline splice: store_temp_pop on empty stack");
                    code.push(Ir::Move {
                        dst: callee_slots[n as usize],
                        src: v,
                    });
                }
                Instr::PushInstvar(n) => {
                    let dst = self.fresh(true);
                    code.push(Ir::LoadField {
                        dst,
                        obj: callee_self,
                        byte_off: (BODY_OFFSET + 8 * n as usize) as i32,
                    });
                    cstack.push(dst);
                }
                Instr::StoreInstvarPop(n) => {
                    let val = cstack
                        .pop()
                        .expect("inline splice: store_instvar_pop on empty stack");
                    code.push(Ir::StoreField {
                        obj: callee_self,
                        byte_off: (BODY_OFFSET + 8 * n as usize) as i32,
                        val,
                        barrier: true,
                    });
                }
                Instr::Pop => {
                    cstack.pop().expect("inline splice: pop on empty stack");
                }
                Instr::Dup => {
                    let tos = *cstack.last().expect("inline splice: dup on empty stack");
                    cstack.push(tos);
                }
                Instr::Send { ic, super_ } => {
                    // `is_inline_eligible_nonleaf` rejected super sends, so this
                    // is always an ordinary dynamic send.
                    debug_assert!(!super_, "non-leaf splice: super send should be gated out");
                    let inner_ic = InterpreterIc::at(callee, ic);
                    let inner_argc = inner_ic.argc();
                    let inner_sel = inner_ic.selector();
                    // The bci of THIS in-body send within the callee — the
                    // reconstructed inlined frame's own resume bci on a deopt.
                    let inner_bci = bci;
                    // Consult the callee's own feedback (its IC). A cold
                    // (Untaken) inner IC lowers to a step-3 uncommon trap INSIDE
                    // the inlined body — the cleanest way to force an in-body
                    // deopt; a warm mono/poly/mega stays a plain compiled
                    // `CallSend`. We do NOT recursively inline (depth-1).
                    let inner_fb =
                        crate::compiler::feedback::read_send_site(self.vm, callee, ic, None);
                    // Pop the inner send's operands off the callee's own stack.
                    let mut inner_args: Vec<VReg> = (0..inner_argc)
                        .map(|_| {
                            cstack
                                .pop()
                                .expect("inline splice: inner send missing arg operand")
                        })
                        .collect();
                    inner_args.reverse();
                    let inner_recv = cstack
                        .pop()
                        .expect("inline splice: inner send missing receiver operand");

                    if let crate::compiler::inline::InlineDecision::Trap =
                        crate::compiler::inline::decide(&inner_fb)
                    {
                        // Cold inner send → terminating uncommon trap INSIDE the
                        // inlined body, re-executing the inner send interpreted.
                        // Its recorded operand stack is the inlined body's stack
                        // WITH the inner send's receiver+args still present
                        // (reexecute=true), plus the receiver+args just popped.
                        let mut reexec_stack = cstack.clone();
                        reexec_stack.push(inner_recv);
                        reexec_stack.extend_from_slice(&inner_args);
                        deopt.push((
                            code.len() as u32,
                            DeoptRaw {
                                stack: reexec_stack,
                                // Resume bci is the INNER send's bci in the
                                // INLINED callee (the inlined frame's own resume
                                // point); the caller frame's resume comes from
                                // the SenderLink.
                                bci: inner_bci,
                                kind: SafepointKind::UncommonTrap,
                                reexecute: true,
                                inline: Some(inline_proto.clone()),
                            },
                        ));
                        code.push(Ir::UncommonTrap { bci: inner_bci });
                        trapped = true;
                        break;
                    }

                    // A real compiled send inside the inlined body. Its site is
                    // the callee's own (klass, selector) speculation-independent
                    // dispatch — a plain S11 compiled IC.
                    let mut send_args = vec![inner_recv];
                    send_args.extend_from_slice(&inner_args);
                    let site = self.call_sites.len() as u16;
                    self.call_sites.push(CallSiteInfo {
                        selector: inner_sel,
                        argc: inner_argc + 1,
                        static_klass: None,
                    });
                    self.site_feedback.push(inner_fb);
                    let dst = self.fresh(true);
                    // Call-return safepoint (reexecute=false): recorded stack is
                    // the inlined body's operand stack BELOW the send (receiver+
                    // args already popped); the materializer pushes the result.
                    deopt.push((
                        code.len() as u32,
                        DeoptRaw {
                            stack: cstack.clone(),
                            bci: inner_bci,
                            kind: SafepointKind::Call,
                            reexecute: false,
                            inline: Some(inline_proto.clone()),
                        },
                    ));
                    code.push(Ir::CallSend {
                        dst,
                        site,
                        args: send_args,
                    });
                    cstack.push(dst);
                }
                Instr::ReturnTos => {
                    result = Some(
                        cstack
                            .pop()
                            .expect("inline splice: return_tos on empty stack"),
                    );
                    break;
                }
                Instr::ReturnSelf => {
                    result = Some(callee_self);
                    break;
                }
                // `is_inline_eligible_nonleaf` rejected super/ctx/closure/NLR;
                // every other opcode is handled above.
                other => {
                    // A shape slipped past the eligibility gate — decline
                    // cleanly. But we have already emitted the guard + cold
                    // block; that is harmless (the guard dominates a body we now
                    // abandon), yet the caller's operand-stack discipline breaks
                    // if we return None mid-splice. So this must be unreachable
                    // by construction (the gate is exact); assert it.
                    unreachable!(
                        "non-leaf inline splice: {other:?} passed is_inline_eligible_nonleaf \
                         but has no arm"
                    )
                }
            }
            bci = next;
        }

        self.record_inline_dep(guard_klass, selector);
        if trapped {
            Some(NonLeafOutcome::Trapped)
        } else {
            let result =
                result.expect("inline splice: single-return non-leaf must produce a result");
            Some(NonLeafOutcome::Value(result))
        }
    }

    /// S14 step 7-IV-a: an uncommon-trap block for a deopt site INSIDE an
    /// inlined extent — like [`Translator::fresh_not_bool_block`] /
    /// [`Translator::fail_and_branch`] but carrying the extent's `InlineSite`,
    /// so the deopt rebuilds BOTH the inlined frame and its caller.
    fn fresh_inlined_trap_block(
        &mut self,
        trap_bci: usize,
        reexec_stack: Vec<VReg>,
        inline: InlineSite,
    ) -> BlockId {
        let trap_id = self.fresh_block_id();
        self.finish_block(IrBlock {
            id: trap_id,
            bci: trap_bci,
            code: vec![Ir::UncommonTrap { bci: trap_bci }],
            entry_stack: Vec::new(),
            deopt_sites: vec![(
                0,
                DeoptRaw {
                    stack: reexec_stack,
                    bci: trap_bci,
                    kind: SafepointKind::UncommonTrap,
                    reexecute: true,
                    inline: Some(inline),
                },
            )],
        });
        trap_id
    }

    /// S14 step 7-IV-a: splice a MULTI-BLOCK callee (branches, loops — the
    /// general CFG form `inline::is_inline_eligible_cfg` admits) inline behind
    /// a receiver-klass guard. Where the leaf/nonleaf splicers paste a single
    /// straight line into the caller's current block, this maps the callee's
    /// WHOLE decoded CFG onto fresh caller `IrBlock`s:
    ///
    ///   - every callee CFG block gets a fresh caller block id; its
    ///     `Jump`/`Branch` targets are remapped through that table;
    ///   - the callee's operand-stack merge discipline is rebuilt with the SAME
    ///     machinery `convert` uses on the root method (`compute_entry_depths` +
    ///     `entry_stack_sources` + `emit_merges` + fresh merge vregs);
    ///   - every `return` becomes `Move {result_vreg, v}; Jump {continuation}` —
    ///     the caller resumes in `continuation` with `result_vreg` as the send's
    ///     value;
    ///   - a backward jump becomes an `Ir::Poll` with an in-body LOOP-POLL deopt
    ///     scope (`InlineSite`-chained, reexecute at the callee's loop-header
    ///     bci) — the first time a loop-poll deopt reconstructs depth 2;
    ///   - a `Branch` becomes a (non-fused) `Ir::BoolBr` whose `not_bool` edge is
    ///     an `InlineSite`-chained trap. The condition is POPPED from the sim
    ///     stack after capturing the trap's reexecute snapshot — the
    ///     strictly-correct accounting (`instr_stack_delta(br) == -1`), keeping
    ///     Inherit/Merge successors depth-exact at ANY stack depth (convert's
    ///     root loop gets away without the pop only because its branch
    ///     successors sit at depth 0 and `Empty` discards the leftover);
    ///   - the callee's own sends lower exactly as in the nonleaf splice: a cold
    ///     (Untaken) IC → a TERMINATING in-body trap; a warm IC → a plain
    ///     compiled `CallSend` with a call-return deopt scope. No smi fusion, no
    ///     branch fusion, no recursive inlining inside the inlinee (correctness
    ///     first; those are optimizations on top).
    ///
    /// On success returns `(result_vreg, callee_entry_id, continuation_id)`:
    /// the caller pushes `result_vreg`, sets `pending_jump_target =
    /// callee_entry_id`, and returns `continuation_id` as its `split`. Returns
    /// `None` (having emitted NOTHING) only on an up-front shape mismatch.
    #[allow(clippy::too_many_arguments)]
    fn try_inline_cfg(
        &mut self,
        callee: MethodOop,
        guard_klass: KlassOop,
        guard_shape: GuardShape,
        selector: SymbolOop,
        receiver: VReg,
        args: &[VReg],
        send_bci: usize,
        pre_pop_stack: &[VReg],
        code: &mut Vec<Ir>,
        deopt: &mut Vec<(u32, DeoptRaw)>,
    ) -> Option<(VReg, BlockId, BlockId)> {
        let _ = deopt; // caller-block deopt vec unused: guard cold path is its own block
        let ccfg = crate::compiler::decode::decode(callee);
        if ccfg.blocks.is_empty() || callee.argc() != args.len() {
            return None;
        }

        let callee_pool_ix = self.pool.intern(callee.oop().raw(), Some(RelocKind::Oop)).0;
        let n_operands = args.len() + 1;
        debug_assert!(pre_pop_stack.len() >= n_operands);
        let caller_pending_stack = pre_pop_stack[..pre_pop_stack.len() - n_operands].to_vec();
        let callee_self = receiver;

        // Guard fronting the whole extent (cold = reexecute the send in the
        // CALLER scope — `inline: None`, exactly the leaf/nonleaf convention).
        let expect = self
            .pool
            .intern(guard_klass.oop().raw(), Some(RelocKind::Oop));
        let cold_id = self.fresh_block_id();
        self.finish_block(IrBlock {
            id: cold_id,
            bci: send_bci,
            code: vec![Ir::UncommonTrap { bci: send_bci }],
            entry_stack: Vec::new(),
            deopt_sites: vec![(
                0,
                DeoptRaw {
                    stack: pre_pop_stack.to_vec(),
                    bci: send_bci,
                    kind: SafepointKind::UncommonTrap,
                    reexecute: true,
                    inline: None,
                },
            )],
        });
        code.push(Ir::GuardKlass {
            obj: callee_self,
            expect,
            fail: cold_id,
            kind: guard_shape,
        });

        // Callee slots: args alias the send operands; temps = fresh nil vregs
        // (initialised in the CALLER's block, dominating the whole extent).
        let mut callee_slots: Vec<VReg> = Vec::with_capacity(callee.argc() + callee.ntemps());
        callee_slots.extend_from_slice(args);
        let nil_lit = self
            .pool
            .intern(self.vm.universe.nil_obj.raw(), Some(RelocKind::Oop));
        for _ in 0..callee.ntemps() {
            let t = self.fresh(true);
            code.push(Ir::ConstPool {
                dst: t,
                lit: nil_lit,
            });
            callee_slots.push(t);
        }

        let inline_proto = InlineSite {
            method_pool_ix: callee_pool_ix,
            receiver: callee_self,
            slots: callee_slots.clone(),
            sender_bci: send_bci as u16,
            caller_pending_stack,
            is_block: false,
            parent: None,
        };

        // ── CFG scaffolding: the same machinery convert runs on the root. ──
        let (entry_depth, _max) = compute_entry_depths(callee, &ccfg);
        let sources = entry_stack_sources(&ccfg, &entry_depth);
        let n = ccfg.blocks.len();
        let ir_ids: Vec<BlockId> = (0..n).map(|_| self.fresh_block_id()).collect();
        let continuation_id = self.fresh_block_id();
        let result_vreg = self.fresh(true);

        let mut entry_stacks: Vec<Option<Vec<VReg>>> = vec![None; n];
        for (b, source) in sources.iter().enumerate() {
            match source {
                EntryStackSource::Empty => entry_stacks[b] = Some(Vec::new()),
                EntryStackSource::Merge => {
                    let depth = entry_depth[b] as usize;
                    entry_stacks[b] = Some((0..depth).map(|_| self.fresh(true)).collect());
                }
                EntryStackSource::Inherit(_) => {}
            }
        }
        let mut exit_stacks: Vec<Option<Vec<VReg>>> = vec![None; n];
        let mut reachable = vec![false; n];
        reachable[0] = true;

        for b in 0..n {
            if !reachable[b] {
                // Dead (post-trap / post-return) callee block: an inert,
                // structurally-valid placeholder, never jumped to (mirrors
                // convert's own dead-block handling).
                self.finish_block(IrBlock {
                    id: ir_ids[b],
                    bci: send_bci,
                    code: vec![Ir::RetSelf],
                    entry_stack: Vec::new(),
                    deopt_sites: Vec::new(),
                });
                continue;
            }
            let entry_stack = match sources[b] {
                EntryStackSource::Empty | EntryStackSource::Merge => {
                    entry_stacks[b].clone().expect("pre-allocated above")
                }
                EntryStackSource::Inherit(pred) => exit_stacks[pred]
                    .clone()
                    .expect("single non-backward predecessor translated first"),
            };
            let cfg_block = &ccfg.blocks[b];
            let mut cstack = entry_stack.clone();
            let mut bcode: Vec<Ir> = Vec::new();
            let mut bdeopt: Vec<(u32, DeoptRaw)> = Vec::new();
            let mut trapped = false;
            let mut returned = false;
            let mut last_instr_bci = cfg_block.bci_start;
            let mut bci = cfg_block.bci_start;
            while bci < cfg_block.bci_end {
                last_instr_bci = bci;
                let (instr, next) = decode_at(callee, bci);
                match instr {
                    Instr::PushSelf => cstack.push(callee_self),
                    Instr::PushNil => cstack
                        .push(self.push_well_known(self.vm.universe.nil_obj.raw(), &mut bcode)),
                    Instr::PushTrue => cstack
                        .push(self.push_well_known(self.vm.universe.true_obj.raw(), &mut bcode)),
                    Instr::PushFalse => cstack
                        .push(self.push_well_known(self.vm.universe.false_obj.raw(), &mut bcode)),
                    Instr::PushSmi(v) => {
                        let dst = self.fresh(true);
                        bcode.push(Ir::ConstSmi {
                            dst,
                            value: v as i64,
                        });
                        cstack.push(dst);
                    }
                    Instr::PushLiteral(idx) => {
                        let lit_oop = callee.literals().at(idx as usize);
                        cstack.push(self.push_well_known(lit_oop.raw(), &mut bcode));
                    }
                    Instr::PushGlobal(idx) => {
                        let assoc = callee.literals().at(idx as usize);
                        let assoc_vreg = self.push_well_known(assoc.raw(), &mut bcode);
                        let dst = self.fresh(true);
                        bcode.push(Ir::LoadField {
                            dst,
                            obj: assoc_vreg,
                            byte_off: (BODY_OFFSET + 8) as i32,
                        });
                        cstack.push(dst);
                    }
                    Instr::PushTemp(t) => {
                        let dst = self.fresh(true);
                        bcode.push(Ir::Move {
                            dst,
                            src: callee_slots[t as usize],
                        });
                        cstack.push(dst);
                    }
                    Instr::StoreTemp(t) => {
                        let tos = *cstack.last().expect("cfg splice: store_temp empty stack");
                        bcode.push(Ir::Move {
                            dst: callee_slots[t as usize],
                            src: tos,
                        });
                    }
                    Instr::StoreTempPop(t) => {
                        let v = cstack
                            .pop()
                            .expect("cfg splice: store_temp_pop empty stack");
                        bcode.push(Ir::Move {
                            dst: callee_slots[t as usize],
                            src: v,
                        });
                    }
                    Instr::PushInstvar(iv) => {
                        let dst = self.fresh(true);
                        bcode.push(Ir::LoadField {
                            dst,
                            obj: callee_self,
                            byte_off: (BODY_OFFSET + 8 * iv as usize) as i32,
                        });
                        cstack.push(dst);
                    }
                    Instr::StoreInstvarPop(iv) => {
                        let val = cstack
                            .pop()
                            .expect("cfg splice: store_instvar_pop empty stack");
                        bcode.push(Ir::StoreField {
                            obj: callee_self,
                            byte_off: (BODY_OFFSET + 8 * iv as usize) as i32,
                            val,
                            barrier: true,
                        });
                    }
                    Instr::StoreGlobalPop(idx) => {
                        let assoc = callee.literals().at(idx as usize);
                        let assoc_vreg = self.push_well_known(assoc.raw(), &mut bcode);
                        let val = cstack
                            .pop()
                            .expect("cfg splice: store_global_pop empty stack");
                        bcode.push(Ir::StoreField {
                            obj: assoc_vreg,
                            byte_off: (BODY_OFFSET + 8) as i32,
                            val,
                            barrier: true,
                        });
                    }
                    Instr::Pop => {
                        cstack.pop().expect("cfg splice: pop empty stack");
                    }
                    Instr::Dup => {
                        let tos = *cstack.last().expect("cfg splice: dup empty stack");
                        cstack.push(tos);
                    }
                    Instr::Send { ic, super_ } => {
                        debug_assert!(!super_, "cfg splice: super gated by is_inline_eligible_cfg");
                        let inner_ic = InterpreterIc::at(callee, ic);
                        let inner_argc = inner_ic.argc();
                        let inner_sel = inner_ic.selector();
                        let inner_bci = bci;
                        let inner_fb =
                            crate::compiler::feedback::read_send_site(self.vm, callee, ic, None);
                        let mut inner_args: Vec<VReg> = (0..inner_argc)
                            .map(|_| cstack.pop().expect("cfg splice: send missing arg"))
                            .collect();
                        inner_args.reverse();
                        let inner_recv = cstack.pop().expect("cfg splice: send missing receiver");

                        if let crate::compiler::inline::InlineDecision::Trap =
                            crate::compiler::inline::decide(&inner_fb)
                        {
                            let mut reexec_stack = cstack.clone();
                            reexec_stack.push(inner_recv);
                            reexec_stack.extend_from_slice(&inner_args);
                            bdeopt.push((
                                bcode.len() as u32,
                                DeoptRaw {
                                    stack: reexec_stack,
                                    bci: inner_bci,
                                    kind: SafepointKind::UncommonTrap,
                                    reexecute: true,
                                    inline: Some(inline_proto.clone()),
                                },
                            ));
                            bcode.push(Ir::UncommonTrap { bci: inner_bci });
                            trapped = true;
                            break;
                        }

                        let mut send_args = vec![inner_recv];
                        send_args.extend_from_slice(&inner_args);
                        let site = self.call_sites.len() as u16;
                        self.call_sites.push(CallSiteInfo {
                            selector: inner_sel,
                            argc: inner_argc + 1,
                            static_klass: None,
                        });
                        self.site_feedback.push(inner_fb);
                        let dst = self.fresh(true);
                        bdeopt.push((
                            bcode.len() as u32,
                            DeoptRaw {
                                stack: cstack.clone(),
                                bci: inner_bci,
                                kind: SafepointKind::Call,
                                reexecute: false,
                                inline: Some(inline_proto.clone()),
                            },
                        ));
                        bcode.push(Ir::CallSend {
                            dst,
                            site,
                            args: send_args,
                        });
                        cstack.push(dst);
                    }
                    Instr::ReturnTos => {
                        let v = cstack.pop().expect("cfg splice: return_tos empty stack");
                        bcode.push(Ir::Move {
                            dst: result_vreg,
                            src: v,
                        });
                        bcode.push(Ir::Jump {
                            target: continuation_id,
                        });
                        returned = true;
                        break;
                    }
                    Instr::ReturnSelf => {
                        bcode.push(Ir::Move {
                            dst: result_vreg,
                            src: callee_self,
                        });
                        bcode.push(Ir::Jump {
                            target: continuation_id,
                        });
                        returned = true;
                        break;
                    }
                    // Branch/jump opcodes are the block's own terminator —
                    // handled below from the decoded `Terminator` (they need the
                    // remapped target ids).
                    Instr::JumpFwd(_)
                    | Instr::JumpBack(_)
                    | Instr::BrTrueFwd(_)
                    | Instr::BrFalseFwd(_) => {}
                    other => unreachable!(
                        "cfg splice: {other:?} passed is_inline_eligible_cfg but has no arm"
                    ),
                }
                bci = next;
            }

            if !trapped && !returned {
                match cfg_block.terminator {
                    crate::compiler::decode::Terminator::Return => {
                        unreachable!(
                            "cfg splice: a Return-terminated callee block must end in a \
                             return instruction"
                        )
                    }
                    crate::compiler::decode::Terminator::Fallthrough(t) => {
                        reachable[t] = true;
                        emit_merges(&sources, &entry_stacks, t, &cstack, &mut bcode);
                        bcode.push(Ir::Jump { target: ir_ids[t] });
                    }
                    crate::compiler::decode::Terminator::Jump {
                        target,
                        is_backward,
                    } => {
                        reachable[target] = true;
                        if is_backward {
                            // In-body loop poll: reexecute at the CALLEE's
                            // loop-header bci, InlineSite-chained (a mid-loop
                            // NotEntrant deopt rebuilds callee + caller frames).
                            let poll_ci = bcode.len() as u32;
                            bcode.push(Ir::Poll);
                            bdeopt.push((
                                poll_ci,
                                DeoptRaw {
                                    stack: cstack.clone(),
                                    bci: ccfg.blocks[target].bci_start,
                                    kind: SafepointKind::LoopPoll,
                                    reexecute: true,
                                    inline: Some(inline_proto.clone()),
                                },
                            ));
                        }
                        emit_merges(&sources, &entry_stacks, target, &cstack, &mut bcode);
                        bcode.push(Ir::Jump {
                            target: ir_ids[target],
                        });
                    }
                    crate::compiler::decode::Terminator::Branch { if_true, if_false } => {
                        reachable[if_true] = true;
                        reachable[if_false] = true;
                        let val = *cstack
                            .last()
                            .expect("cfg splice: branch with empty simulated stack");
                        // not_bool reexecute snapshot INCLUDES the condition
                        // (the interpreter re-tests it); the sim stack pops it
                        // for the successors (strictly-correct accounting).
                        let not_bool_id = self.fresh_inlined_trap_block(
                            last_instr_bci,
                            cstack.clone(),
                            inline_proto.clone(),
                        );
                        cstack.pop();
                        emit_merges(&sources, &entry_stacks, if_true, &cstack, &mut bcode);
                        emit_merges(&sources, &entry_stacks, if_false, &cstack, &mut bcode);
                        bcode.push(Ir::BoolBr {
                            val,
                            if_true: ir_ids[if_true],
                            if_false: ir_ids[if_false],
                            not_bool: not_bool_id,
                        });
                    }
                }
            }

            exit_stacks[b] = Some(cstack);
            self.finish_block(IrBlock {
                id: ir_ids[b],
                bci: send_bci,
                code: bcode,
                entry_stack,
                deopt_sites: bdeopt,
            });
        }

        self.record_inline_dep(guard_klass, selector);
        Some((result_vreg, ir_ids[0], continuation_id))
    }

    fn record_inline_dep(&mut self, klass: KlassOop, selector: SymbolOop) {
        // Dedup: an inlined method reached via two identical sites need only one
        // dependency (the invalidation is idempotent, but keeping the vec tight
        // keeps the driver's copy small).
        let pair = (klass.oop().raw(), selector.oop().raw());
        if !self
            .inline_deps
            .iter()
            .any(|(k, s)| (k.oop().raw(), s.oop().raw()) == pair)
        {
            self.inline_deps.push((klass, selector));
        }
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
    ///
    /// S14 step 3: `trapped` is set `true` when a generic send site's feedback
    /// is `Untaken` and [`crate::compiler::inline::decide`] lowers it to an
    /// uncommon trap (`Ir::UncommonTrap`) INSTEAD of an `Ir::CallSend`. A trap
    /// is a terminator — control leaves the compiled method entirely — so the
    /// caller (`convert`'s per-CFG-block loop) must finish the current `IrBlock`
    /// at the trap and skip both the rest of this CFG block's instructions and
    /// its own terminator (all dead after the trap). Distinct from `split`,
    /// which ends a block WITH a continuation to fall into; a trapped block has
    /// no continuation.
    #[allow(clippy::too_many_arguments)]
    fn translate_instr(
        &mut self,
        instr: &Instr,
        fusable: bool,
        bci: usize,
        stack: &mut Vec<VReg>,
        stack_sites: &mut Vec<Option<usize>>,
        code: &mut Vec<Ir>,
        deopt: &mut Vec<(u32, DeoptRaw)>,
        pending_cmp: &mut Option<(CmpOp, VReg, VReg, PendingCmpDeopt)>,
        trapped: &mut bool,
    ) -> Option<BlockId> {
        debug_assert_eq!(
            stack.len(),
            stack_sites.len(),
            "translate_instr: operand-stack / block-site shadow desync"
        );
        // S14 step 7-I: handle every opcode that can create, move, or consume a
        // phantom block-site FIRST, keeping `stack`/`stack_sites` in lockstep,
        // and return early. A `push_closure` proven elidable emits NO IR (the
        // block body is spliced at its value-send); the temp/dup/pop/value-send
        // arms propagate or consume the site. Reached only for a method the
        // escape pre-pass proved `all_elidable` (`self.escape.is_some()`), so a
        // site here is always a good, spliceable use. Every OTHER opcode falls
        // through to the main match below, after which `stack_sites` is resynced
        // to `stack`'s new length with `None` (those opcodes never touch a site).
        if self.escape.is_some() {
            if let Some(r) =
                self.translate_site_instr(instr, bci, stack, stack_sites, code, deopt, trapped)
            {
                return r;
            }
        }
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
                // S14 step 2: annotate the site with its observed feedback (the
                // inliner prefers the STATIC super_klass above, but the field is
                // kept parallel to `call_sites` for every site uniformly).
                self.site_feedback
                    .push(crate::compiler::feedback::read_send_site(
                        self.vm,
                        self.method,
                        ic,
                        None,
                    ));

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
                        inline: None,
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
                                inline: None,
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

                // S14 step 3: consult the site's type feedback (read here,
                // parallel to S14 step 2's own `read_send_site` call) BEFORE
                // touching the operand stack. An `Untaken` site (IC still
                // `Empty` at compile time — now COMPILABLE as of this step's
                // eligibility relaxation, see `driver::mono_smi_inline_send`)
                // lowers to an uncommon TRAP instead of a real send: it never
                // executed interpreted, so there is no target to speculate on.
                // Everything else (`Mono`/`Poly`/`Mega`) stays a plain generic
                // `Ir::CallSend` (method/poly INLINING is S14 steps 4/6).
                let feedback =
                    crate::compiler::feedback::read_send_site(self.vm, self.method, ic, None);
                if let crate::compiler::inline::InlineDecision::Trap =
                    crate::compiler::inline::decide(&feedback)
                {
                    // The trap re-executes the WHOLE send in the interpreter
                    // (reexecute=true at THIS send's own bci), which populates
                    // the IC and returns the identical result — so the recorded
                    // operand stack must be the state BEFORE the send's pops,
                    // with the receiver + all args STILL PRESENT (snapshot
                    // `stack` HERE, before any pop below). Same
                    // reexecute-stack convention as the smi-overflow /
                    // basicNew-alloc / loop-poll sites; `regalloc::deopt_live`
                    // forces every recorded vreg live-across the trap so
                    // spill-all pins it to a frame slot the deopt materializer
                    // reads.
                    //
                    // We push NEITHER a `CallSiteInfo` NOR a `site_feedback`
                    // entry for a trapped site: it never dispatches, so it owns
                    // no `Ir::CallSend.site` index (the `call_sites`/
                    // `site_feedback` vecs stay parallel, indexed by real send
                    // sites only). And we push no `dst`: control leaves the
                    // method via the trap, so nothing after it in this block is
                    // reachable — `trapped` tells `convert` to finish the block
                    // here and skip the rest.
                    //
                    // INTERIM DEOPT-STORM (documented, NOT solved here): an
                    // EXECUTED trap re-executes the send interpreted and warms
                    // the IC, but this nmethod stays Alive with the trap still
                    // compiled, so the NEXT call re-traps — a storm until the
                    // method is recompiled with the now-warm feedback. That
                    // recompile-on-trap loop is S14 step 8 (recompile.rs) and
                    // needs `trap_counts` / `UNCOMMON_TRAP_LIMIT`, which S13
                    // never built. Step 3 accepts the storm: it is
                    // CORRECTNESS-PRESERVING (every trap re-executes the send
                    // exactly, identical output) — just slow, and bounded per
                    // run by the call count. No trap counting / recompilation
                    // is implemented here.
                    let reexec_stack = stack.clone();
                    deopt.push((
                        code.len() as u32,
                        DeoptRaw {
                            stack: reexec_stack,
                            bci,
                            kind: SafepointKind::UncommonTrap,
                            reexecute: true,
                            inline: None,
                        },
                    ));
                    code.push(Ir::UncommonTrap { bci });
                    *trapped = true;
                    return split; // (always None on the trap path)
                }

                let ic_view = InterpreterIc::at(self.method, ic);
                let real_argc = ic_view.argc();

                // S14 step 4b: leaf-method inlining. Consult the budget-aware
                // decision on the SAME `feedback` already read for the trap
                // check. A `Mono` site whose callee is a cheap leaf splices
                // inline behind a receiver-klass guard (cold path = the SAME
                // step-3 uncommon trap, so a wrong receiver deopts + re-executes
                // the send generically); everything else stays a plain
                // `CallSend` below. The pre-pop operand `stack` (receiver + args
                // still present) is the reexecute stack the guard's cold trap
                // records — snapshot it BEFORE popping.
                let budget = crate::compiler::inline::budget_for_level(self.level);
                let smi_bits = self.vm.universe.smi_klass.oop().raw();
                if let crate::compiler::inline::InlineDecision::Inline { callee, guard } =
                    crate::compiler::inline::decide_with_budget(&feedback, &budget, smi_bits)
                {
                    let guard_shape = match guard {
                        crate::compiler::inline::GuardKind::SmiTest => GuardShape::SmiTest,
                        // `KlassTest` and (defensively) `None` — a `None`-guard
                        // elision is a later step; today `decide_with_budget`
                        // only ever returns SmiTest/KlassTest for a real inline,
                        // so this maps the rest to a full klass test (never
                        // wrong, at worst one redundant compare).
                        _ => GuardShape::KlassTest,
                    };
                    // The observed receiver klass IS the guard klass; the callee
                    // is the resolved leaf method. The guard depends on
                    // `lookup(guard_klass, selector)` == callee.
                    let guard_klass = match &feedback {
                        crate::compiler::feedback::SiteFeedback::Mono { klass, .. } => *klass,
                        // Unreachable: `decide_with_budget` only returns `Inline`
                        // for a `Mono` site.
                        _ => unreachable!("Inline decision from a non-Mono feedback"),
                    };
                    let selector = ic_view.selector();
                    let pre_pop_stack = stack.clone();
                    // Pop the send's operands (they alias the callee's
                    // self/args — no stores).
                    let mut inline_args: Vec<VReg> = (0..real_argc)
                        .map(|_| stack.pop().expect("send: missing arg operand"))
                        .collect();
                    inline_args.reverse();
                    let receiver = stack.pop().expect("send: missing receiver operand");

                    if let Some(result) = self.try_inline_leaf(
                        callee,
                        guard_klass,
                        guard_shape,
                        selector,
                        receiver,
                        &inline_args,
                        bci,
                        &pre_pop_stack,
                        code,
                    ) {
                        // Inlined: push the body's result. NO CallSiteInfo /
                        // site_feedback / IcSite / call-return deopt for this
                        // site — it never dispatches (A2).
                        stack.push(result);
                        return split; // (None here — no smi split on this path)
                    }
                    // S14 step 4c: a NON-leaf callee (the leaf splice declined
                    // because the body has sends). Splice it inline with nested
                    // deopt scopes chained to the caller via a `SenderLink`. Its
                    // own sends become plain `CallSend`s (or step-3 traps) in the
                    // inlined body.
                    match self.try_inline_nonleaf(
                        callee,
                        guard_klass,
                        guard_shape,
                        selector,
                        receiver,
                        &inline_args,
                        bci,
                        &pre_pop_stack,
                        code,
                        deopt,
                    ) {
                        Some(NonLeafOutcome::Value(result)) => {
                            // Inlined: push the body's result. The inlined send
                            // itself dispatches nothing — only the body's OWN
                            // inner sends did (they pushed their CallSiteInfo /
                            // IcSite above).
                            stack.push(result);
                            return split;
                        }
                        Some(NonLeafOutcome::Trapped) => {
                            // An in-body cold send became a terminating uncommon
                            // trap: control leaves the method here, nothing after
                            // the inlined send in THIS block is reachable. Tell
                            // `convert` to finish the block at the trap (same as
                            // a root-level trapped send).
                            *trapped = true;
                            return split; // (always None on the trap path)
                        }
                        None => {}
                    }
                    // S14 step 7-IV-a: a MULTI-BLOCK callee (branches/loops —
                    // the single-straight-line splicers above declined). Map its
                    // whole CFG into fresh caller blocks; the send's value
                    // arrives in `result` via the callee's return edges, and the
                    // caller resumes in the returned continuation block.
                    if let Some((result, entry_id, continuation_id)) = self.try_inline_cfg(
                        callee,
                        guard_klass,
                        guard_shape,
                        selector,
                        receiver,
                        &inline_args,
                        bci,
                        &pre_pop_stack,
                        code,
                        deopt,
                    ) {
                        stack.push(result);
                        debug_assert!(
                            self.pending_jump_target.is_none(),
                            "cfg splice: a pending jump target is already armed"
                        );
                        self.pending_jump_target = Some(entry_id);
                        return Some(continuation_id);
                    }
                    // Declined mid-way (an unspliceable shape slipped past the
                    // budget/leaf gate): fall through to a plain CallSend, but
                    // the operands are already popped — rebuild `args` from them.
                    let mut args = vec![receiver];
                    args.extend_from_slice(&inline_args);
                    let site = self.call_sites.len() as u16;
                    self.call_sites.push(CallSiteInfo {
                        selector,
                        argc: real_argc + 1,
                        static_klass: None,
                    });
                    self.site_feedback.push(feedback);
                    let dst = self.fresh(true);
                    deopt.push((
                        code.len() as u32,
                        DeoptRaw {
                            stack: stack.clone(),
                            bci,
                            kind: SafepointKind::Call,
                            reexecute: false,
                            inline: None,
                        },
                    ));
                    code.push(Ir::CallSend { dst, site, args });
                    stack.push(dst);
                    return split;
                }

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
                // S14 step 2: annotate this send with its interpreter feedback
                // (already read above for the trap decision — kept parallel to
                // `call_sites` for every REAL send site).
                self.site_feedback.push(feedback);

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
                        inline: None,
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
            // S14 step 7-II-b: M's own captured temp, promoted to `ctx_vregs`
            // (M's heap Context is elided). Only depth 0 (M's own Context) is
            // eligible — a `depth != 0` was rejected by `driver::eligible`.
            Instr::PushCtxTemp { depth, idx } => {
                debug_assert_eq!(depth, 0, "compiled ctx-temp access must be depth 0");
                let dst = self.fresh(true);
                code.push(Ir::Move {
                    dst,
                    src: self.ctx_vregs[idx as usize],
                });
                stack.push(dst);
            }
            Instr::StoreCtxTempPop { depth, idx } => {
                debug_assert_eq!(depth, 0, "compiled ctx-temp access must be depth 0");
                let v = stack
                    .pop()
                    .expect("store_ctx_temp_pop: empty simulated stack");
                code.push(Ir::Move {
                    dst: self.ctx_vregs[idx as usize],
                    src: v,
                });
            }
            Instr::PushClosure { .. } | Instr::BlockReturnTos | Instr::NlrTos => {
                panic!(
                    "translate_instr: {instr:?} should have been rejected by driver::eligible \
                     (D1) -- compiler bug if this fires"
                )
            }
        }
        // S14 step 7-I: `stack_sites` is resynced to `stack`'s new length by the
        // CALLER (`convert`'s per-instruction loop), NOT here — `translate_instr`
        // has many early `return split` paths (Alloc, trap, inline, generic
        // send) that would otherwise skip a resync and leave the shadow stale.
        split
    }

    /// S14 step 7-I: intercept the opcodes that create, move, or consume a
    /// phantom block-site (a `push_closure` the escape pre-pass proved elidable)
    /// BEFORE `translate_instr`'s main match, keeping `stack` and its
    /// `stack_sites` shadow in lockstep. Returns:
    ///   - `None` — this opcode does not touch a live block-site here; the caller
    ///     falls through to the main match (which handles the real value) and
    ///     resyncs `stack_sites` afterwards;
    ///   - `Some(split)` — handled here; the caller returns `split` directly.
    ///     A `push_closure` (emits no IR) and the temp/dup/pop bookkeeping return
    ///     `Some(None)`; a value-send splices the block body inline and also
    ///     returns `Some(None)` (a send-free leaf block adds no continuation).
    ///
    /// Only reached for a method the escape pre-pass proved `all_elidable`
    /// (`self.escape.is_some()`), so a live block-site here is always an
    /// immediately-invoked, non-escaping literal block. 7-II: a spliced block
    /// with a cold in-body send lowers to a terminating trap — `splice_block`
    /// returns `true`, which sets `*trapped` so `convert` finishes the block.
    #[allow(clippy::too_many_arguments)]
    fn translate_site_instr(
        &mut self,
        instr: &Instr,
        bci: usize,
        stack: &mut Vec<VReg>,
        stack_sites: &mut Vec<Option<usize>>,
        code: &mut Vec<Ir>,
        deopt: &mut Vec<(u32, DeoptRaw)>,
        trapped: &mut bool,
    ) -> Option<Option<BlockId>> {
        match *instr {
            // A proven-elidable closure: emit NO IR. Push a phantom onto the
            // operand stack (`self_vreg` as a harmless filler — a leaked phantom
            // reads `self`, and `convert`'s block-boundary assert catches any
            // leak in debug) with its site id in the shadow. The real carrier is
            // `stack_sites`; the vreg is never read (its only consumer, the
            // value-send, reads `stack_sites`, not the vreg).
            Instr::PushClosure { .. } => {
                stack.push(self.self_vreg);
                stack_sites.push(Some(bci));
                Some(None)
            }
            // A temp currently holding a block-site pushes the phantom; else fall
            // through so the main match emits the real `Move`.
            Instr::PushTemp(t) => {
                if let Some(site) = self.temp_sites[t as usize] {
                    stack.push(self.self_vreg);
                    stack_sites.push(Some(site));
                    Some(None)
                } else {
                    None
                }
            }
            Instr::StoreTempPop(t) => match stack_sites.last().copied().flatten() {
                Some(site) => {
                    // Top is a block-site: record it in the temp, drop the
                    // phantom, emit no `Move`.
                    stack.pop();
                    stack_sites.pop();
                    self.temp_sites[t as usize] = Some(site);
                    Some(None)
                }
                None => {
                    // Storing a real value: clear any stale site the temp held,
                    // then fall through to emit the real `Move`.
                    self.temp_sites[t as usize] = None;
                    None
                }
            },
            Instr::StoreTemp(t) => match stack_sites.last().copied().flatten() {
                // `store_temp` peeks (no pop) — mirror the shadow, leave the site
                // on the stack.
                Some(site) => {
                    self.temp_sites[t as usize] = Some(site);
                    Some(None)
                }
                None => {
                    self.temp_sites[t as usize] = None;
                    None
                }
            },
            Instr::Dup => match stack_sites.last().copied().flatten() {
                Some(site) => {
                    stack.push(self.self_vreg);
                    stack_sites.push(Some(site));
                    Some(None)
                }
                None => None,
            },
            Instr::Pop => match stack_sites.last().copied().flatten() {
                // Discard a dead closure — SPEC A7 "dead" is elidable, no IR.
                Some(_) => {
                    stack.pop();
                    stack_sites.pop();
                    Some(None)
                }
                None => None,
            },
            Instr::Send { ic, .. } => {
                // A `value`-family send the escape pre-pass mapped to a splice.
                // (`value_send_target` returns a `Copy` `usize`, so the immutable
                // borrow of `self.escape` ends before the `&mut self` splice.)
                let target = self.escape.as_ref().and_then(|e| e.value_send_target(bci));
                if let Some(site) = target {
                    if self.splice_block(site, ic, bci, stack, stack_sites, code, deopt) {
                        *trapped = true;
                    }
                    Some(None)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// S14 step 7-I/7-II: splice a proven-elidable literal block's body inline at
    /// its `value`-family send. Like [`Translator::try_inline_nonleaf`] MINUS the
    /// receiver guard — the receiver is statically the block created at
    /// `site_bci`, so there is no klass to test and no dependency to record. The
    /// block's home `self` is M's receiver (`self_vreg` = the closure's captured
    /// `copied[0]`, SPEC §5.4). Pops the value-send's args + the phantom receiver
    /// (keeping `stack`/`stack_sites` in lockstep).
    ///
    /// **7-II**: the block body MAY contain its own (non-super) sends — each
    /// becomes a plain `Ir::CallSend` (or, for a cold IC, a step-3
    /// `Ir::UncommonTrap`) inside the spliced extent, recording an in-body deopt
    /// scope with an `is_block: true` `InlineSite` chained to M. A deopt there
    /// rebuilds the block's OWN activation frame: it is a method-shaped frame, and
    /// the block frame's only structural difference (the closure in its
    /// receiver-arg slot rather than the home self) is read ONLY by `nlr_tos` /
    /// nested `push_closure`, both still gated out — so the deopt materializer's
    /// method-frame path reconstructs it soundly (`runtime/deopt.rs`).
    ///
    /// Returns `true` iff an in-body send was cold and became a TERMINATING trap
    /// (control leaves the method — the caller must finish the current block,
    /// nothing after is reachable); on a normal return it pushes the block's
    /// result onto `stack` and returns `false`.
    #[allow(clippy::too_many_arguments)]
    fn splice_block(
        &mut self,
        site_bci: usize,
        ic: u16,
        send_bci: usize,
        stack: &mut Vec<VReg>,
        stack_sites: &mut Vec<Option<usize>>,
        code: &mut Vec<Ir>,
        deopt: &mut Vec<(u32, DeoptRaw)>,
    ) -> bool {
        // Resolve the CompiledBlock from the `push_closure` literal at `site_bci`.
        let (site_instr, _) = decode_at(self.method, site_bci);
        let lit = match site_instr {
            Instr::PushClosure { lit, .. } => lit,
            _ => unreachable!("splice_block: site bci is not a push_closure"),
        };
        let block = MethodOop::try_from(self.method.literals().at(lit as usize))
            .expect("splice_block: push_closure literal is not a CompiledBlock");

        let argc = InterpreterIc::at(self.method, ic).argc() as usize;
        debug_assert_eq!(
            argc,
            block.argc(),
            "splice_block: value-send argc must match block argc (escape guaranteed)"
        );

        // Pop the value-send's args (real vregs) + the phantom receiver, keeping
        // `stack`/`stack_sites` in lockstep. An arg is never a block-site (an
        // arg-site would have escaped), so its shadow entry is `None`.
        let mut blk_args: Vec<VReg> = (0..argc)
            .map(|_| {
                stack_sites.pop();
                stack.pop().expect("value-send: missing arg operand")
            })
            .collect();
        blk_args.reverse();
        stack_sites.pop(); // the phantom receiver's site marker
        stack
            .pop()
            .expect("value-send: missing receiver (phantom closure)");

        // The block's home self = M's receiver (only read by push_self / instvar
        // access inside the block body).
        let block_self = self.self_vreg;

        // Re-validate the spliceable shape (escape's `block_is_spliceable` proved
        // it; the arity is site-specific). If it somehow fails we have already
        // popped the operands and cannot cleanly recover — assert.
        let block_cfg = crate::compiler::decode::decode(block);
        assert!(
            block_cfg.blocks.len() == 1
                && matches!(
                    block_cfg.blocks[0].terminator,
                    crate::compiler::decode::Terminator::Return
                )
                && block.argc() == blk_args.len(),
            "splice_block: escape proved spliceable but shape re-check failed"
        );

        // Callee slots: args alias the value-send operands (no stores); temps
        // become fresh nil vregs with canonical spill homes.
        let mut slots: Vec<VReg> = Vec::with_capacity(block.argc() + block.ntemps());
        slots.extend_from_slice(&blk_args);
        let nil_lit = self
            .pool
            .intern(self.vm.universe.nil_obj.raw(), Some(RelocKind::Oop));
        for _ in 0..block.ntemps() {
            let t = self.fresh(true);
            code.push(Ir::ConstPool {
                dst: t,
                lit: nil_lit,
            });
            slots.push(t);
        }

        // 7-II: the InlineSite prototype for every in-body deopt safepoint. The
        // reconstructed inlined scope is an `is_block` scope whose sender is M
        // (this method): `sender_bci` is the value-send's bci and
        // `caller_pending_stack` is M's operand stack BELOW the value-send's
        // receiver+args (which we just popped, so `stack` IS that now). The
        // scope's receiver is the block's home self (M's receiver) and its slots
        // are the block's args+temps.
        let block_pool_ix = self.pool.intern(block.oop().raw(), Some(RelocKind::Oop)).0;
        let inline_proto = InlineSite {
            method_pool_ix: block_pool_ix,
            receiver: block_self,
            slots: slots.clone(),
            sender_bci: send_bci as u16,
            caller_pending_stack: stack.clone(),
            is_block: true,
            parent: None,
        };

        // Translate the block's straight-line body onto a fresh operand stack.
        // Same value-shuffle/field/return arms as the leaf method splice, PLUS
        // `BlockReturnTos` (a block's own fall-off return, which `decode` treats
        // as a `Return` terminator — decode.rs), PLUS the 7-II `Send` arm (a
        // compiled `CallSend` or a step-3 cold trap inside the inlined block,
        // recording an `is_block` in-body deopt scope).
        let mut bstack: Vec<VReg> = Vec::new();
        let mut result: Option<VReg> = None;
        let mut trapped = false;
        let len = block.bytecode_len();
        let mut bci = 0;
        while bci < len {
            let (instr, next) = decode_at(block, bci);
            match instr {
                Instr::PushSelf => bstack.push(block_self),
                Instr::PushNil => {
                    bstack.push(self.push_well_known(self.vm.universe.nil_obj.raw(), code))
                }
                Instr::PushTrue => {
                    bstack.push(self.push_well_known(self.vm.universe.true_obj.raw(), code))
                }
                Instr::PushFalse => {
                    bstack.push(self.push_well_known(self.vm.universe.false_obj.raw(), code))
                }
                Instr::PushSmi(v) => {
                    let dst = self.fresh(true);
                    code.push(Ir::ConstSmi {
                        dst,
                        value: v as i64,
                    });
                    bstack.push(dst);
                }
                Instr::PushLiteral(idx) => {
                    let lit_oop = block.literals().at(idx as usize);
                    bstack.push(self.push_well_known(lit_oop.raw(), code));
                }
                Instr::PushGlobal(idx) => {
                    let assoc = block.literals().at(idx as usize);
                    let assoc_vreg = self.push_well_known(assoc.raw(), code);
                    let dst = self.fresh(true);
                    code.push(Ir::LoadField {
                        dst,
                        obj: assoc_vreg,
                        byte_off: (BODY_OFFSET + 8) as i32,
                    });
                    bstack.push(dst);
                }
                Instr::PushTemp(n) => {
                    let dst = self.fresh(true);
                    code.push(Ir::Move {
                        dst,
                        src: slots[n as usize],
                    });
                    bstack.push(dst);
                }
                Instr::StoreTemp(n) => {
                    let tos = *bstack
                        .last()
                        .expect("block splice: store_temp on empty stack");
                    code.push(Ir::Move {
                        dst: slots[n as usize],
                        src: tos,
                    });
                }
                Instr::StoreTempPop(n) => {
                    let v = bstack
                        .pop()
                        .expect("block splice: store_temp_pop on empty stack");
                    code.push(Ir::Move {
                        dst: slots[n as usize],
                        src: v,
                    });
                }
                Instr::PushInstvar(n) => {
                    let dst = self.fresh(true);
                    code.push(Ir::LoadField {
                        dst,
                        obj: block_self,
                        byte_off: (BODY_OFFSET + 8 * n as usize) as i32,
                    });
                    bstack.push(dst);
                }
                Instr::StoreInstvarPop(n) => {
                    let val = bstack
                        .pop()
                        .expect("block splice: store_instvar_pop on empty stack");
                    code.push(Ir::StoreField {
                        obj: block_self,
                        byte_off: (BODY_OFFSET + 8 * n as usize) as i32,
                        val,
                        barrier: true,
                    });
                }
                Instr::StoreGlobalPop(idx) => {
                    let assoc = block.literals().at(idx as usize);
                    let assoc_vreg = self.push_well_known(assoc.raw(), code);
                    let val = bstack
                        .pop()
                        .expect("block splice: store_global_pop on empty stack");
                    code.push(Ir::StoreField {
                        obj: assoc_vreg,
                        byte_off: (BODY_OFFSET + 8) as i32,
                        val,
                        barrier: true,
                    });
                }
                Instr::Pop => {
                    bstack.pop().expect("block splice: pop on empty stack");
                }
                Instr::Dup => {
                    let tos = *bstack.last().expect("block splice: dup on empty stack");
                    bstack.push(tos);
                }
                Instr::ReturnTos | Instr::BlockReturnTos => {
                    result = Some(bstack.pop().expect("block splice: return on empty stack"));
                    break;
                }
                Instr::ReturnSelf => {
                    result = Some(block_self);
                    break;
                }
                // 7-III: `^expr` (non-local return). The block's home is M (the
                // block is created + value'd in M), so the NLR is just a RETURN
                // FROM M — `Ir::Ret` of the value. Control leaves the method, so
                // (like a trap) the caller must finish the current block. The
                // `ensure:` decline (SPEC A7 Step 1) is automatic: an M with
                // `ensure:` fails the escape gate (its handler block escapes).
                // `block_is_spliceable` restricts NLR blocks to SEND-FREE, so no
                // in-block deopt runs the interpreter's `nlr_tos` (which would
                // need a synthesized home-ref closure).
                Instr::NlrTos => {
                    let val = bstack.pop().expect("block splice: nlr_tos on empty stack");
                    code.push(Ir::Ret { val });
                    trapped = true;
                    break;
                }
                // 7-II: the block's OWN (non-super) sends. A cold IC (Untaken)
                // lowers to a TERMINATING uncommon trap inside the inlined block
                // (re-executing the send interpreted); a warm IC stays a plain
                // compiled `CallSend`. Both record an `is_block` in-body deopt
                // scope. Mirrors `try_inline_nonleaf`'s Send arm exactly, minus
                // recursion (depth-1 into the block: a nested send stays a Call).
                Instr::Send {
                    ic: inner_ic_idx,
                    super_,
                } => {
                    debug_assert!(
                        !super_,
                        "block splice: super send gated out by block_is_spliceable"
                    );
                    let inner_ic = InterpreterIc::at(block, inner_ic_idx);
                    let inner_argc = inner_ic.argc();
                    let inner_sel = inner_ic.selector();
                    let inner_bci = bci;
                    let inner_fb = crate::compiler::feedback::read_send_site(
                        self.vm,
                        block,
                        inner_ic_idx,
                        None,
                    );
                    let mut inner_args: Vec<VReg> = (0..inner_argc)
                        .map(|_| {
                            bstack
                                .pop()
                                .expect("block splice: inner send missing arg operand")
                        })
                        .collect();
                    inner_args.reverse();
                    let inner_recv = bstack
                        .pop()
                        .expect("block splice: inner send missing receiver operand");

                    if let crate::compiler::inline::InlineDecision::Trap =
                        crate::compiler::inline::decide(&inner_fb)
                    {
                        // Cold inner send → terminating trap re-executing it
                        // interpreted. Recorded stack has the receiver+args still
                        // present (reexecute=true), in the BLOCK's own scope.
                        let mut reexec_stack = bstack.clone();
                        reexec_stack.push(inner_recv);
                        reexec_stack.extend_from_slice(&inner_args);
                        deopt.push((
                            code.len() as u32,
                            DeoptRaw {
                                stack: reexec_stack,
                                bci: inner_bci,
                                kind: SafepointKind::UncommonTrap,
                                reexecute: true,
                                inline: Some(inline_proto.clone()),
                            },
                        ));
                        code.push(Ir::UncommonTrap { bci: inner_bci });
                        trapped = true;
                        break;
                    }

                    let mut send_args = vec![inner_recv];
                    send_args.extend_from_slice(&inner_args);
                    let site = self.call_sites.len() as u16;
                    self.call_sites.push(CallSiteInfo {
                        selector: inner_sel,
                        argc: inner_argc + 1,
                        static_klass: None,
                    });
                    self.site_feedback.push(inner_fb);
                    let dst = self.fresh(true);
                    // Call-return safepoint (reexecute=false): recorded stack is
                    // the block's operand stack BELOW the send; the materializer
                    // pushes the result.
                    deopt.push((
                        code.len() as u32,
                        DeoptRaw {
                            stack: bstack.clone(),
                            bci: inner_bci,
                            kind: SafepointKind::Call,
                            reexecute: false,
                            inline: Some(inline_proto.clone()),
                        },
                    ));
                    code.push(Ir::CallSend {
                        dst,
                        site,
                        args: send_args,
                    });
                    bstack.push(dst);
                }
                // S14 step 7-II-b: the block reads/writes its HOME method's
                // captured temps. A ctx-less (`captures_ctx`) block's frame
                // aliases M's Context, so it addresses M's ctx-temps at depth 0 —
                // the SAME promoted `ctx_vregs` M itself uses. `depth != 0` was
                // rejected by `block_is_spliceable`.
                Instr::PushCtxTemp { depth, idx } => {
                    debug_assert_eq!(depth, 0, "block ctx-temp access must be depth 0");
                    let dst = self.fresh(true);
                    code.push(Ir::Move {
                        dst,
                        src: self.ctx_vregs[idx as usize],
                    });
                    bstack.push(dst);
                }
                Instr::StoreCtxTempPop { depth, idx } => {
                    debug_assert_eq!(depth, 0, "block ctx-temp access must be depth 0");
                    let v = bstack
                        .pop()
                        .expect("block splice: store_ctx_temp_pop on empty stack");
                    code.push(Ir::Move {
                        dst: self.ctx_vregs[idx as usize],
                        src: v,
                    });
                }
                // `block_is_spliceable` still rejects super/closure/NLR and
                // depth>0 ctx access, and the single-block shape excludes
                // jumps/branches; every other opcode is handled above.
                other => unreachable!(
                    "block splice: {other:?} passed block_is_spliceable but has no arm"
                ),
            }
            bci = next;
        }

        if trapped {
            // An in-body cold send became a terminating trap: control leaves the
            // method here. Push NO result — the caller (`translate_site_instr`)
            // propagates `trapped` so `convert` finishes the block at the trap.
            return true;
        }
        let result = result.expect("block splice: a returning block must produce a result");
        stack.push(result);
        stack_sites.push(None);
        false
    }

    fn push_well_known(&mut self, raw: u64, code: &mut Vec<Ir>) -> VReg {
        let lit = self.pool.intern(raw, Some(RelocKind::Oop));
        let dst = self.fresh(true);
        code.push(Ir::ConstPool { dst, lit });
        dst
    }
}

/// S14 step 4b: does `callee`'s body consist solely of opcodes
/// [`Translator::try_inline_leaf`] can splice? A dry-run over the bytecode used
/// to decide inlining BEFORE emitting the guard, so a mid-splice bailout (which
/// would leave a guard dominating a half-built body) is impossible. The set
/// mirrors `try_inline_leaf`'s own match arms exactly; a `Send` never appears
/// (the caller only reaches here for a leaf, `inline::is_leaf`).
fn leaf_body_is_spliceable(callee: MethodOop) -> bool {
    let len = callee.bytecode_len();
    let mut bci = 0;
    while bci < len {
        let (instr, next) = decode_at(callee, bci);
        let ok = matches!(
            instr,
            Instr::PushSelf
                | Instr::PushNil
                | Instr::PushTrue
                | Instr::PushFalse
                | Instr::PushSmi(_)
                | Instr::PushLiteral(_)
                | Instr::PushGlobal(_)
                | Instr::PushTemp(_)
                | Instr::StoreTemp(_)
                | Instr::StoreTempPop(_)
                | Instr::PushInstvar(_)
                | Instr::StoreInstvarPop(_)
                | Instr::Pop
                | Instr::Dup
                | Instr::ReturnTos
                | Instr::ReturnSelf
        );
        if !ok {
            return false;
        }
        bci = next;
    }
    true
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
    // S14 step 7-II-b: a vreg per M ctx-slot (only when M is `has_ctx` — its
    // heap Context is elided and its captured temps live in these vregs).
    let ctx_vregs: Vec<VReg> = (0..method.nctx())
        .map(|_| {
            let n = vregs.len() as u32;
            vregs.push(VRegInfo { is_oop: true });
            VReg(n)
        })
        .collect();
    let mut t = Translator {
        vm,
        method,
        vregs,
        pool: PoolBuilder::new(),
        self_vreg,
        temp_vregs: temp_vregs.clone(),
        ctx_vregs: ctx_vregs.clone(),
        next_extra_block: cfg.blocks.len() as u32,
        blocks_by_id: (0..cfg.blocks.len()).map(|_| None).collect(),
        call_sites: Vec::new(),
        site_feedback: Vec::new(),
        inline_deps: Vec::new(),
        // Tier-1 compiles are always level 1 today (`driver::compile_method`
        // sets `level: 1`); the inline budget scales with this when higher
        // levels arrive (S14 step 8).
        level: 1,
        const_class: HashMap::new(),
        // S14 step 7-I: run the escape pre-pass iff M creates a literal closure.
        // The driver already proved `all_elidable` for such a method before
        // reaching here, so a `Some` result always means every closure site is a
        // splicable good use. A closure-free method pays nothing (skips the scan
        // AND the pre-pass) — keeping non-closure methods' listing goldens
        // byte-stable.
        escape: {
            let mut has_closure = false;
            let mut b = 0usize;
            let bc_len = method.bytecode_len();
            while b < bc_len {
                let (instr, next) = decode_at(method, b);
                if matches!(instr, Instr::PushClosure { .. }) {
                    has_closure = true;
                    break;
                }
                b = next;
            }
            if has_closure {
                Some(crate::compiler::escape::analyze(method))
            } else {
                None
            }
        },
        temp_sites: vec![None; method.argc() + method.ntemps()],
        pending_jump_target: None,
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

    // S14 step 3: forward reachability over LIVE (non-trap) control flow. A
    // block that lowers a send to an uncommon TRAP terminates the method there,
    // so its CFG successors are unreachable UNLESS a live edge from another
    // block also reaches them. `compute_entry_depths`/`entry_stack_sources` are
    // computed on the no-trap CFG (they can't see a per-site trap decision), so
    // once a block traps, a downstream merge that was to receive a value from it
    // would otherwise be fed a mismatched (or empty) operand stack — corrupting
    // the merge (an `emit_merges` depth-mismatch panic, or worse a wrong Move).
    // Tracking reachability lets `convert` skip DEAD blocks entirely: they are
    // finished as trivial `Ir::Bailout` blocks (never reached at runtime, and
    // regalloc's own reachability DFS never orders them into the live path — the
    // exact treatment decode's post-return dead blocks already get) and, crucially,
    // never feed a live merge. Blocks are processed in increasing bci order, so
    // every forward predecessor of a merge runs (and marks it reachable) before
    // the merge itself; a backward-edge (loop) target was already reached
    // forward, so its liveness is already settled.
    let mut reachable = vec![false; cfg.blocks.len()];
    reachable[0] = true;

    for (b, cfg_block) in cfg.blocks.iter().enumerate() {
        // A dead block (unreachable once traps are accounted for): finish a
        // trivial placeholder so `blocks_by_id` is complete, but translate
        // nothing and feed no merges. Its `entry_stack`/`exit_stacks` are never
        // consulted (no live successor inherits from it — any `Inherit(dead)`
        // block is itself dead and skipped here too).
        if !reachable[b] {
            t.finish_block(IrBlock {
                id: block_id(b),
                bci: cfg_block.bci_start,
                // `RetSelf`: a valid, operand-free terminator (reads only
                // `VReg(0)`, always present). This block is never reached at
                // runtime and never ordered into the live path by regalloc's DFS
                // from block 0; the terminator exists only so the block is
                // structurally well-formed if the DFS's dead-code sweep appends
                // it.
                code: vec![Ir::RetSelf],
                entry_stack: Vec::new(),
                deopt_sites: Vec::new(),
            });
            continue;
        }

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
            // S14 step 7-II-b: nil-init the promoted ctx-temp vregs (M's elided
            // Context's slots start nil, matching `alloc_context`'s nil-fill —
            // so a ctx-temp read before it is written yields nil).
            for &cv in ctx_vregs.iter() {
                code.push(Ir::ConstPool {
                    dst: cv,
                    lit: nil_lit,
                });
            }
        }

        let is_branch_terminator = matches!(cfg_block.terminator, Terminator::Branch { .. });
        let mut stack = entry_stack.clone();
        // S14 step 7-I: parallel to `stack` — `stack_sites[i] == Some(bci)` iff
        // operand-stack slot `i` currently holds the (phantom) block created at
        // `push_closure` site `bci`. A block-site never survives a block boundary
        // in the 7-I straight-line shapes (push_closure and its value-send are in
        // the same block, adjacent or via a temp), so entry is all-`None`;
        // `translate_instr` maintains it in lockstep with `stack`.
        let mut stack_sites: Vec<Option<usize>> = vec![None; stack.len()];
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
        // S13 step 7b-ii: the bci of the block's LAST instruction. For a
        // `Branch` terminator that is the `br_true`/`br_false` opcode itself —
        // the reexecute resume point for a non-boolean `mustBeBoolean` deopt
        // (the interpreter rolls its own bci back to exactly this opcode).
        let mut last_instr_bci = cfg_block.bci_start;
        // S14 step 3: set true when a generic send in this block lowered to an
        // uncommon TRAP (an `Untaken` site). A trap is a terminator (control
        // leaves the method), so the rest of this CFG block's instructions AND
        // its terminator are dead — finish the block at the trap and move on.
        let mut trapped = false;
        while bci < cfg_block.bci_end {
            last_instr_bci = bci;
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
                &mut stack_sites,
                &mut code,
                &mut deopt,
                &mut pending_cmp,
                &mut trapped,
            );
            // S14 step 7-I: resync the block-site shadow to `stack`'s new length
            // after EVERY `translate_instr` (its many early `return split` paths
            // would skip an in-function resync). A site-carrying opcode kept the
            // two in lockstep itself (this is a no-op then); a plain opcode only
            // ever pushes/pops non-site values, so padding new slots with `None`
            // and truncating popped ones is exact — a real send's receiver/args
            // are never sites (a site receiver is a splice, handled earlier).
            stack_sites.resize(stack.len(), None);
            if trapped {
                // The `Ir::UncommonTrap` is already the last op in `code` (and
                // its deopt site is already in `deopt`). Stop translating this
                // block: everything after the trap is unreachable. `code`/
                // `deopt` are finished into `cur_id` by the trapped-block path
                // below (which skips the terminator entirely).
                break;
            }
            if let Some(continuation_id) = split {
                // S14 step 7-IV-a: a CFG-inlined send set `pending_jump_target`
                // to the callee's ENTRY block — the current block jumps THERE,
                // and control reaches `continuation_id` only via the inlined
                // callee's return edges. Unset (the ordinary smi-split case),
                // the jump goes straight to the continuation.
                let jump_target = t.pending_jump_target.take().unwrap_or(continuation_id);
                code.push(Ir::Jump {
                    target: jump_target,
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
        // S14 step 7-I: a phantom block-site must never survive to a block
        // boundary — the escape pre-pass proved every closure is consumed by a
        // value-send in the straight-line shape 7-I splices, so `stack_sites` is
        // all-`None` here. If one leaked, `emit_merges`/terminator handling would
        // emit an `Ir::Move` of a never-defined phantom vreg. Asserted so a
        // future out-of-scope closure shape is caught in testing, not silently
        // miscompiled.
        debug_assert!(
            stack_sites.iter().all(|s| s.is_none()),
            "S14 7-I: a block-site survived to a block boundary (out-of-scope closure shape)"
        );
        let local_exit = stack;

        // S14 step 3: a trapped block ends at its `Ir::UncommonTrap` (already
        // the last op in `code`, its deopt site already in `deopt`) — control
        // leaves the method, so there is NO terminator to emit and NO merge to
        // propagate. Finish the current accumulating block here and move on.
        // `exit_stacks[b]` is still recorded (with the pre-trap operand stack)
        // so any successor whose `entry_stack_sources` said `Inherit(b)` finds
        // a well-formed stack; that successor is now unreachable from `b` (the
        // trap doesn't branch to it) and, absent another predecessor, becomes
        // dead code that `regalloc`'s reachability DFS simply never orders into
        // the live path (its own `unreachable`-block handling, already exercised
        // by decode's post-return dead blocks).
        if trapped {
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
            continue;
        }

        // S14 step 3: this block is live (reachable) and did NOT trap (a trapped
        // block `continue`d above), so its terminator's successors are live too
        // — mark them reachable BEFORE the terminator match feeds their merges.
        // A `Return` has no successor.
        match cfg_block.terminator {
            Terminator::Return => {}
            Terminator::Fallthrough(t) | Terminator::Jump { target: t, .. } => reachable[t] = true,
            Terminator::Branch { if_true, if_false } => {
                reachable[if_true] = true;
                reachable[if_false] = true;
            }
        }

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
                    // S13 step 10b: a backward-jump `Poll` is a deopt SAFEPOINT
                    // (mirroring the UncommonTrap fail blocks) — if this loop's
                    // own nmethod is made `NotEntrant` mid-loop, only the poll
                    // can deopt it (a call-free loop never returns into a
                    // redirected slot). Record ONE deopt site at the poll:
                    //   - `kind = LoopPoll`, `reexecute = true`;
                    //   - `stack` = `local_exit`, the pre-merge exit operand
                    //     stack live AT the poll (merges are just vreg renames,
                    //     so pre-merge vregs resolve to the same VALUES);
                    //   - `bci` = the LOOP-HEADER bci = the backward-jump
                    //     `target` block's `bci_start`: the interpreter resumes
                    //     there and RE-EXECUTES the loop condition.
                    // The poll's `SafepointPc` is emitted at the `bl stub_poll`
                    // RETURN address (`emit::emit_poll`), so this site keys on
                    // that return offset via the shared position numbering (like
                    // a CallSend), and `driver::build_deopt_metadata` correlates
                    // it exactly like any other `deopt_sites` entry — no
                    // kind-specific handling there. `regalloc::deopt_live` forces
                    // receiver + slots + `local_exit` live-across the poll so
                    // spill-all pins them to frame slots (never Nil).
                    let poll_ci = code.len() as u32;
                    code.push(Ir::Poll);
                    deopt.push((
                        poll_ci,
                        DeoptRaw {
                            stack: local_exit.clone(),
                            bci: cfg.blocks[target].bci_start,
                            kind: SafepointKind::LoopPoll,
                            reexecute: true,
                            inline: None,
                        },
                    ));
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
                    // S13 step 7b-ii: the not_bool edge deopts at the branch
                    // opcode. `local_exit` is the operand stack BEFORE the
                    // branch (the deferred `br_true`/`br_false` never popped
                    // `val`), so it IS the reexecute stack.
                    let not_bool_id = t.fresh_not_bool_block(last_instr_bci, local_exit.to_vec());
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
        ctx_vregs,
        safepoints: Vec::new(),
        true_lit,
        false_lit,
        nil_lit,
        mark_slots_lit,
        call_sites: t.call_sites,
        site_feedback: t.site_feedback,
        inline_deps: t.inline_deps,
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

    /// S14 step 2/3: `convert` annotates each real (non-inlined, non-trapped)
    /// `call_sites` entry with its interpreter feedback, same index. A mono send
    /// to a NON-smi klass stays a real `CallSend` and carries `Mono` feedback;
    /// a fresh (empty) IC is `Untaken` and — S14 step 3 — lowers to an uncommon
    /// TRAP instead of a `CallSend`, so it produces NO `call_sites`/
    /// `site_feedback` entry but DOES record an `UncommonTrap` deopt site.
    #[test]
    fn convert_annotates_site_feedback() {
        use crate::compiler::feedback::SiteFeedback;
        let mut vm = test_vm();
        let foo_sel = vm.universe.intern(b"foo");
        // S14 step 4c: a target that is NOT inline-eligible (its body has a
        // SUPER send, which the inliner gates out) so a warm mono site to it
        // stays a real `CallSend` instead of being spliced inline. A leaf
        // accessor inlines (4b), a plain non-leaf now inlines too (4c); a
        // super-send callee is the shape that still dispatches.
        let inner_sel = vm.universe.intern(b"inner");
        let target = {
            let mut b = BytecodeBuilder::new();
            b.push_self();
            b.send_super(&mut vm, inner_sel, 0);
            b.ret_tos();
            b.finish(&mut vm, foo_sel, 0, 0)
        };

        // `m [ ^self foo ]` — one real send site.
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.send(&mut vm, foo_sel, 0);
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 0, 0);

        // Fresh IC (never dispatched) → Untaken → an uncommon trap: no
        // call_site, no site_feedback entry, but ONE UncommonTrap deopt site.
        {
            let cfg = decode::decode(method);
            let ir = convert(&vm, method, &cfg);
            assert_eq!(
                ir.site_feedback.len(),
                ir.call_sites.len(),
                "one site_feedback per real call site (both empty for a trapped-only method)"
            );
            assert_eq!(
                ir.call_sites.len(),
                0,
                "an Untaken send lowers to a trap, not a CallSend -> no call_site"
            );
            let trap_sites: usize = ir
                .blocks
                .iter()
                .flat_map(|blk| blk.deopt_sites.iter())
                .filter(|(_, raw)| matches!(raw.kind, SafepointKind::UncommonTrap))
                .count();
            assert_eq!(
                trap_sites, 1,
                "the one Untaken `foo` send became exactly one uncommon-trap deopt site"
            );
        }

        // Warm the IC to mono on a NON-smi klass → Mono feedback → real CallSend.
        let klass = vm.universe.object_klass;
        let epoch = vm.ic_epoch;
        InterpreterIc::at(method, 0).set_mono(&mut vm, klass, target, epoch);
        let cfg = decode::decode(method);
        let ir = convert(&vm, method, &cfg);
        assert_eq!(
            ir.call_sites.len(),
            1,
            "a warm mono send is a real CallSend"
        );
        match &ir.site_feedback[0] {
            SiteFeedback::Mono {
                klass: k,
                method: m,
            } => {
                assert_eq!(k.oop().raw(), klass.oop().raw());
                assert_eq!(m.oop().raw(), target.oop().raw());
            }
            other => panic!("expected Mono, got {other:?}"),
        }
    }

    /// S14 step 4b: a warm mono send to a cheap LEAF accessor (`^instvar0`)
    /// splices inline — the IR gains an `Ir::GuardKlass` + a `LoadField` off the
    /// receiver, NO `Ir::CallSend` for the site, and the method records one
    /// `inline_deps` pair `(receiver_klass, selector)`. The cold trap block
    /// carries a reexecute `UncommonTrap` at the send's bci.
    #[test]
    fn convert_inlines_leaf_accessor() {
        let mut vm = test_vm();
        let recv_klass = vm.universe.new_klass(
            vm.universe.object_klass,
            "S14InlineRecv",
            crate::oops::Format::Slots,
            false,
            crate::oops::layout::HEADER_WORDS + 1,
        );
        // `val [ ^instvar0 ]` — a leaf accessor.
        let val_sel = vm.universe.intern(b"val");
        let val = {
            let mut vb = BytecodeBuilder::new();
            vb.push_instvar(0);
            vb.ret_tos();
            vb.finish(&mut vm, val_sel, 0, 0)
        };

        // `m [ ^self val ]`.
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.send(&mut vm, val_sel, 0);
        b.ret_tos();
        let m_sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, m_sel, 0, 0);

        let epoch = vm.ic_epoch;
        InterpreterIc::at(method, 0).set_mono(&mut vm, recv_klass, val, epoch);

        let cfg = decode::decode(method);
        let ir = convert(&vm, method, &cfg);

        // No CallSend for the inlined site.
        assert_eq!(ir.call_sites.len(), 0, "inlined leaf → no CallSend");
        assert_eq!(ir.site_feedback.len(), 0);

        // Exactly one GuardKlass, one instvar LoadField off the receiver, and no
        // CallSend anywhere.
        let (mut guards, mut loads, mut calls) = (0, 0, 0);
        for blk in &ir.blocks {
            for op in &blk.code {
                match op {
                    Ir::GuardKlass { obj, kind, .. } => {
                        guards += 1;
                        assert_eq!(*obj, VReg(0), "guard tests the receiver (self)");
                        assert_eq!(*kind, GuardShape::KlassTest, "non-smi klass → KlassTest");
                    }
                    Ir::LoadField { obj, byte_off, .. } => {
                        // The accessor's instvar0 read off the receiver.
                        if *obj == VReg(0) && *byte_off == BODY_OFFSET as i32 {
                            loads += 1;
                        }
                    }
                    Ir::CallSend { .. } => calls += 1,
                    _ => {}
                }
            }
        }
        assert_eq!(guards, 1, "one receiver-klass guard");
        assert_eq!(loads, 1, "one inlined instvar0 load off the receiver");
        assert_eq!(calls, 0, "no CallSend — the send was inlined");

        // One inline dependency: (recv_klass, val).
        assert_eq!(ir.inline_deps.len(), 1);
        assert_eq!(ir.inline_deps[0].0.oop().raw(), recv_klass.oop().raw());
        assert_eq!(ir.inline_deps[0].1.oop().raw(), val_sel.oop().raw());

        // A cold reexecute trap at the send's bci.
        let trap_sites: usize = ir
            .blocks
            .iter()
            .flat_map(|blk| blk.deopt_sites.iter())
            .filter(|(_, raw)| matches!(raw.kind, SafepointKind::UncommonTrap) && raw.reexecute)
            .count();
        assert_eq!(trap_sites, 1, "the guard's cold path is one reexecute trap");
    }

    /// S14 step 4b: `^self` (a bare return-self leaf) inlines — the result is
    /// the receiver vreg itself (no field load), behind a klass guard, and a
    /// mono SMI receiver picks the `SmiTest` guard shape.
    #[test]
    fn convert_inlines_self_and_arg_smi_guard() {
        let mut vm = test_vm();
        // `id [ ^self ]` on SmallInteger (smi klass → SmiTest guard).
        let id_sel = vm.universe.intern(b"idSelf");
        let id_method = {
            let mut vb = BytecodeBuilder::new();
            vb.ret_self();
            vb.finish(&mut vm, id_sel, 0, 0)
        };
        // `m [ ^self idSelf ]`.
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.send(&mut vm, id_sel, 0);
        b.ret_tos();
        let m_sel = vm.universe.intern(b"mSelf");
        let method = b.finish(&mut vm, m_sel, 0, 0);
        let smi_klass = vm.universe.smi_klass;
        let epoch = vm.ic_epoch;
        InterpreterIc::at(method, 0).set_mono(&mut vm, smi_klass, id_method, epoch);

        let cfg = decode::decode(method);
        let ir = convert(&vm, method, &cfg);
        assert_eq!(ir.call_sites.len(), 0, "^self inlined, no CallSend");
        let smi_guards = ir
            .blocks
            .iter()
            .flat_map(|b| b.code.iter())
            .filter(|op| {
                matches!(
                    op,
                    Ir::GuardKlass {
                        kind: GuardShape::SmiTest,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(smi_guards, 1, "a smi receiver klass → one SmiTest guard");
        assert_eq!(ir.inline_deps.len(), 1);
        assert_eq!(ir.inline_deps[0].1.oop().raw(), id_sel.oop().raw());
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
