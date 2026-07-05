//! Linearization + live intervals + linear-scan register allocation
//! (`sprint_s10_detail.md` D3.4/D3.5). Operates purely on an [`IrMethod`] —
//! no `VmState`/bytecode/`MethodOop` involved, so every test here builds
//! its `IrMethod` (or, for `allocate` alone, its `LiveInterval`s) by hand.
//!
//! Two independently useful stages, matching tests_s10.md's own split:
//! [`compute_intervals`] (linearize + conservative `[min def, max use]`
//! intervals per vreg) and [`allocate`] (the spill-all-at-safepoints +
//! classic linear-scan policy, D3.5), plus [`regalloc`] gluing both for
//! `driver.rs`'s pipeline.

use std::collections::HashMap;

use crate::compiler::ir::{BlockId, Ir, IrBlock, IrMethod, VReg};
use crate::compiler::scopes::SafepointKind;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Assignment {
    Reg(u8),
    Spill(SpillSlot),
}

/// Slot `i` lives at `[x29 − 8·(i+1)]` (D3.4) — an opaque index here;
/// `emit.rs` computes the real frame offset.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpillSlot(pub u16);

/// One vreg's conservative live range: `[start, end]` (both inclusive,
/// instruction positions) covering every def and every use, holes ignored
/// (classic Poletto/Sarkar linear scan — D3.4's own call: this is
/// deliberately simpler than precise per-branch liveness, and correct for
/// SSA-lite's multiple-defs-per-temp-vreg shape, not merely convenient for
/// it: `interval_multi_def_union` is the intended behavior, not a
/// tolerated approximation).
#[derive(Debug)]
pub struct LiveInterval {
    pub vreg: VReg,
    pub start: u32,
    pub end: u32,
    pub is_oop: bool,
    /// True iff some `CallSend`/`CallRuntime`/`Alloc` position `p` satisfies
    /// `start <= p && end > p` — defined by, not merely used at, that
    /// safepoint (an interval whose only reference IS the safepoint's own
    /// argument list ends exactly at `p` and does not need to survive it).
    pub crosses_safepoint: bool,
    pub assignment: Option<Assignment>,
}

fn is_safepoint(ir: &Ir) -> bool {
    matches!(
        ir,
        // S13 step 7b: `UncommonTrap` is a safepoint too — every oop live
        // across it (the re-executing send's `a`/`b`/`self`, kept live by the
        // fail block's `DeoptRaw.stack`) must be spilled (spill-all) and get
        // an OopMap, exactly like a call, so the deopt materializer can read
        // them from the frame. Its position keys BOTH the S12 OopMap and the
        // S13 deopt scope at the brk offset.
        //
        // S13 step 10b: a loop back-edge `Poll` is now a safepoint too — its
        // `bl stub_poll` may deopt the frame (if the loop's own nmethod became
        // `NotEntrant`), so the loop-carried operand stack + receiver + slots
        // (its `DeoptRaw.stack`, forced live-across by `deopt_live` below) must
        // be spilled to frame slots the materializer reads. Its position keys
        // the OopMap (over the `bl` call) AND the LoopPoll deopt scope, at the
        // poll's return offset.
        Ir::CallSend { .. }
            | Ir::CallRuntime { .. }
            | Ir::Alloc { .. }
            | Ir::UncommonTrap { .. }
            | Ir::Poll
    )
}

/// Every block a given block's terminator can transfer control to —
/// includes `fail`/`not_bool`/`slow` edges (the bailout block, or an S11
/// deopt/slow-path block), not just the "normal" successors.
fn successors(block: &IrBlock) -> Vec<BlockId> {
    let mut succs = Vec::new();
    for ir in &block.code {
        match ir {
            Ir::Jump { target } => succs.push(*target),
            Ir::BoolBr {
                if_true,
                if_false,
                not_bool,
                ..
            } => {
                succs.push(*if_true);
                succs.push(*if_false);
                succs.push(*not_bool);
            }
            Ir::SmiCmpBr {
                if_true,
                if_false,
                fail,
                ..
            } => {
                succs.push(*if_true);
                succs.push(*if_false);
                succs.push(*fail);
            }
            Ir::SmiArith { fail, .. } | Ir::SmiCmpVal { fail, .. } => succs.push(*fail),
            Ir::GuardKlass { fail, .. } => succs.push(*fail),
            // S11 D7: `Alloc` is self-contained (fast path + internal slow
            // call, `emit::emit_alloc`) — no slow CFG successor. It stays a
            // safepoint via `is_safepoint` so live-across vregs spill before
            // the internal `bl`; it just doesn't branch to another block.
            _ => {}
        }
    }
    succs
}

/// D3.4: entry first, loop bodies contiguous. A plain postorder DFS,
/// reversed — any block unreachable from the entry (dead code, e.g.
/// decode's own `unreachable_after_return` case surviving into the IR)
/// still needs a position, so any DFS root left unvisited afterward is
/// walked too, appended in index order (dead code never affects real
/// blocks' relative order, only its own).
/// D3.4/D5's own hard requirement: block 0 (the method's real entry) MUST
/// come first in the returned order, unconditionally — `emit.rs`'s prologue
/// falls straight through into whichever block is emitted first, with no
/// guard, so anything else there runs before the method's own body ever
/// does. Standard reverse-postorder-of-a-single-DFS-tree guarantees the
/// root is last in postorder (hence first after reversing THAT tree's own
/// segment) — but block 0 frequently has NO graph successors at all (any
/// straight-line method with no inlined branch/smi-arith fail edge, e.g. a
/// bare accessor like `^value` or `^false` — LoadField/Ret/RetSelf/Bailout
/// aren't matched in `successors` at all), making it its own singleton DFS
/// component. A version of this function that looped over every root
/// in order but reversed the WHOLE accumulated postorder only ONCE, at the
/// end, inverted the relative order BETWEEN components too, not just
/// within each one — block 0's tiny (or singleton) component, visited and
/// pushed first, ended up LAST after a single global reversal. Reversing
/// each root's own segment separately, then concatenating the segments in
/// root order, is what actually preserves "block 0's component comes
/// first" for a forest, not just for one connected tree.
fn reverse_postorder(method: &IrMethod) -> Vec<BlockId> {
    fn dfs(b: usize, blocks: &[IrBlock], visited: &mut [bool], postorder: &mut Vec<BlockId>) {
        if visited[b] {
            return;
        }
        visited[b] = true;
        for succ in successors(&blocks[b]) {
            dfs(succ.0 as usize, blocks, visited, postorder);
        }
        postorder.push(BlockId(b as u32));
    }

    let n = method.blocks.len();
    let mut visited = vec![false; n];
    let mut order = Vec::with_capacity(n);
    for b in 0..n {
        if visited[b] {
            continue;
        }
        let mut postorder = Vec::new();
        dfs(b, &method.blocks, &mut visited, &mut postorder);
        postorder.reverse();
        order.extend(postorder);
    }
    order
}

/// D3.4: number every instruction sequentially (walking blocks in
/// `reverse_postorder`), then fold each vreg's defs/uses into one
/// conservative `[min, max]` interval — a single linear pass, no separate
/// per-block live-in/live-out fixpoint (every def and use is already
/// explicit in the Ir stream — `ir.rs`'s own Move-based merge handling
/// means nothing needs inferring across a block boundary).
///
/// That last claim is true for values that flow through the explicit
/// merge-vreg mechanism at a join — but a temp vreg (`ir.rs`'s "SSA-lite
/// temp rule": one persistent vreg per source temp, reused directly, never
/// re-merged) that's both defined AND used inside the SAME loop body block
/// is a real gap in it: the body block appears exactly once in this linear
/// position space even though it runs many times at runtime, so a def near
/// the block's own end feeding a use near its own start (the next
/// iteration, via the back-edge) has its def position AFTER its use
/// position in linear terms — invisible to a plain `[min_def, max_use]`
/// fold, which would let some OTHER vreg's interval start immediately
/// after that "last" use and steal the same register out from under a
/// value the loop is still very much using. `reverse_postorder` itself
/// only promises block 0 first (S10 step 9's own bug) and every
/// predecessor-except-back-edges before its successors — it does not, and
/// for an if/else-vs-loop-body sibling pair generally cannot, promise a
/// loop body's blocks all precede whatever follows the loop. The fix below
/// doesn't try to fix the linearization further; it widens intervals after
/// the fact: any back edge B->A (A's block starting at or before B's own
/// start, by position) defines a loop range `[start of A, end of B]`, and
/// every interval touching that range at all gets conservatively widened
/// to cover the whole thing — sound (if pessimistic) for nested loops too,
/// via a fixpoint over every back edge found.
///
/// The third return value, `safepoint_positions`, is S12's own addition:
/// the exact linear position of every `CallSend`/`CallRuntime`/`Alloc` op,
/// in this SAME numbering — `compiler::oopmap::build_for_position` (and
/// `emit.rs`'s own position counter, which walks `block_order` identically)
/// depend on this being the exact same sequence `crosses_safepoint` above
/// was computed against, not a re-derivation that could drift out of sync.
pub fn compute_intervals(method: &IrMethod) -> (Vec<BlockId>, Vec<LiveInterval>, Vec<u32>) {
    let block_order = reverse_postorder(method);

    let mut pos: u32 = 0;
    let mut safepoint_positions: Vec<u32> = Vec::new();
    let mut min_def: HashMap<u32, u32> = HashMap::new();
    let mut max_use: HashMap<u32, u32> = HashMap::new();
    let mut block_start_pos: HashMap<u32, u32> = HashMap::new();
    let mut block_end_pos: HashMap<u32, u32> = HashMap::new();
    // S13 step 7b: every vreg an UNCOMMON-TRAP deopt site reads must be LIVE
    // ACROSS its safepoint (spilled to a frame slot the materializer can
    // read), not merely live UP TO it. `driver::build_deopt_metadata` resolves,
    // for each site: the receiver (VReg 0), every arg/temp slot
    // (VReg 1..=argc+ntemps), and the recorded operand `stack` — so all three
    // must cross. This matters for a reexecute UncommonTrap because its fail
    // block has NO fall-through and is linearized LAST (a DFS dead end), so
    // NOTHING is naturally live across it: a value "used after the send" is
    // used in the CONTINUATION block, which linearizes BEFORE the trap, so its
    // interval ends before the trap position, it keeps a register, and it
    // would resolve to Nil — a silently-wrong deopt. Collected here (with the
    // safepoint's own position) and forced to `end > pos` below, so
    // `crosses_safepoint` fires and spill-all pins them.
    //
    // Scoped to `UncommonTrap` and `LoopPoll` — NOT `Call`/`Alloc`: those (S13
    // step 3b) sit inline in a block whose successors run AFTER them, so their
    // recorded vregs are already naturally live-across (used later) and already
    // spilled; widening THOSE would spill genuinely-dead values (a call-return
    // site's popped receiver/args, an Alloc's class const) into their OopMaps,
    // needlessly enlarging them and disturbing S12's GC-root tests — and is
    // unnecessary, since natural liveness already covers exactly what those
    // sites read.
    //
    // S13 step 10b: a `LoopPoll` site (an `Ir::Poll` at a loop back-edge) needs
    // the SAME widening. Its recorded `stack` is the loop-carried operand stack
    // — genuinely live (re-read on the next loop iteration), NOT dead like a
    // call-return's popped operands. Loop-range widening (below) already extends
    // loop-carried intervals to `loop_end`, but the poll can sit AT `loop_end`,
    // so those intervals may `end == poll_pos` rather than STRICTLY across it
    // (`crosses_safepoint` needs `end > pos`). Forcing `end > pos` here pins
    // receiver + slots + the recorded stack to canonical frame slots the deopt
    // materializer reads, exactly as for an UncommonTrap.
    let mut deopt_live: Vec<(u32, u32)> = Vec::new(); // (vreg, safepoint pos)
    let n_slots = method.argc as u32 + method.ntemps as u32;

    for &bid in &block_order {
        let block = &method.blocks[bid.0 as usize];
        block_start_pos.insert(bid.0, pos);
        for (idx, ir) in block.code.iter().enumerate() {
            if is_safepoint(ir) {
                safepoint_positions.push(pos);
            }
            if let Some((_, raw)) = block.deopt_sites.iter().find(|(ci, raw)| {
                *ci == idx as u32
                    && matches!(
                        raw.kind,
                        SafepointKind::UncommonTrap | SafepointKind::LoopPoll
                    )
            }) {
                // Receiver (0) + every unified arg/temp slot + the recorded
                // operand stack are exactly the vregs the driver resolves for
                // this site.
                deopt_live.push((0, pos));
                for s in 1..=n_slots {
                    deopt_live.push((s, pos));
                }
                for &v in &raw.stack {
                    deopt_live.push((v.0, pos));
                }
            }
            ir.uses(|v| {
                max_use
                    .entry(v.0)
                    .and_modify(|e| *e = (*e).max(pos))
                    .or_insert(pos);
                min_def.entry(v.0).or_insert(pos);
            });
            ir.defs(|v| {
                min_def
                    .entry(v.0)
                    .and_modify(|e| *e = (*e).min(pos))
                    .or_insert(pos);
                max_use.entry(v.0).or_insert(pos);
            });
            pos += 1;
        }
        // `pos - 1`: the position of this block's own last instruction (a
        // block always has at least one instruction — every terminator is
        // itself an Ir op); every block gets a position above, so `pos`
        // has advanced past `block_start_pos[bid]` by the time we get here.
        block_end_pos.insert(bid.0, pos.saturating_sub(1));
    }

    // Back-edge loop-range widening (see this function's own doc above).
    let mut loop_ranges: Vec<(u32, u32)> = Vec::new();
    for &bid in &block_order {
        let b = bid.0 as usize;
        let b_start = block_start_pos[&bid.0];
        for succ in successors(&method.blocks[b]) {
            if let Some(&a_start) = block_start_pos.get(&succ.0) {
                if a_start <= b_start {
                    let loop_end = block_end_pos[&bid.0];
                    loop_ranges.push((a_start, loop_end));
                }
            }
        }
    }
    let mut changed = true;
    while changed {
        changed = false;
        for &(loop_start, loop_end) in &loop_ranges {
            // Any vreg with EITHER endpoint inside the loop range gets
            // both endpoints widened to at least cover it — a vreg used
            // only once near a loop's start but never redefined inside it
            // is still safe to widen (widening an interval never makes it
            // wrong, only more conservative).
            let touched: Vec<u32> = min_def
                .keys()
                .chain(max_use.keys())
                .copied()
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .filter(|&v| {
                    let s = *min_def.get(&v).unwrap_or(&u32::MAX);
                    let e = *max_use.get(&v).unwrap_or(&0);
                    s <= loop_end && e >= loop_start
                })
                .collect();
            for v in touched {
                let s = min_def.entry(v).or_insert(loop_start);
                if *s > loop_start {
                    *s = loop_start;
                    changed = true;
                }
                let e = max_use.entry(v).or_insert(loop_end);
                if *e < loop_end {
                    *e = loop_end;
                    changed = true;
                }
            }
        }
    }

    // S13 step 7b: force every deopt-referenced vreg to live STRICTLY ACROSS
    // its safepoint (`end > pos`), so `crosses_safepoint` fires and spill-all
    // pins it to a canonical frame slot the deopt materializer reads (see
    // `deopt_live`'s own doc above). Applied AFTER loop-widening (which only
    // ever extends), so this is the final word on each interval's end. A vreg
    // referenced only here (never defined by any op — a slot for an argument
    // that the body never touches) still needs a `min_def`, so seed it at 0
    // (the entry `Param`/`ConstPool` that establishes every slot always runs
    // first).
    for &(v, sp_pos) in &deopt_live {
        min_def.entry(v).or_insert(0);
        let e = max_use.entry(v).or_insert(sp_pos + 1);
        if *e <= sp_pos {
            *e = sp_pos + 1;
        }
    }

    let intervals = (0..method.vregs.len() as u32)
        .filter_map(|vid| {
            let start = *min_def.get(&vid)?;
            let end = *max_use.get(&vid).unwrap_or(&start);
            let crosses_safepoint = safepoint_positions
                .iter()
                .any(|&sp| start <= sp && end > sp);
            Some(LiveInterval {
                vreg: VReg(vid),
                start,
                end,
                is_oop: method.vregs[vid as usize].is_oop,
                crosses_safepoint,
                assignment: None,
            })
        })
        .collect();

    (block_order, intervals, safepoint_positions)
}

/// x0–x15 (`arm64.md` §3); x16/x17 scratch, x18 platform, x19–x23 unused
/// in v1, x24–x28 VM registers, x29/x30/sp — none of those are
/// allocatable.
const NUM_ALLOCATABLE_REGS: u8 = 16;

/// D3.5's policy, in order: (1) every `crosses_safepoint` interval spills
/// unconditionally, whole-lifetime, before the main scan even starts — the
/// invariant S12's oop maps stand on (registers are never live across a
/// safepoint; maps cover stack slots only), enforced here via a
/// `debug_assert!` rather than merely relied upon. (2) Remaining intervals:
/// classic linear scan — sorted by start, an active list expired as
/// intervals end, and when all registers are busy, the active interval
/// with the furthest end is spilled to make room (Poletto/Sarkar). (3)
/// Spill slots are handed out monotonically; each records its interval's
/// `is_oop` — the raw material for S12's `OopMap`s.
pub fn allocate(intervals: &mut [LiveInterval]) -> (u16, Vec<bool>) {
    let mut slot_is_oop: Vec<bool> = Vec::new();
    let spill = |iv: &mut LiveInterval, slot_is_oop: &mut Vec<bool>| {
        let slot = SpillSlot(slot_is_oop.len() as u16);
        slot_is_oop.push(iv.is_oop);
        iv.assignment = Some(Assignment::Spill(slot));
    };

    for iv in intervals.iter_mut() {
        if iv.crosses_safepoint {
            spill(iv, &mut slot_is_oop);
        }
    }

    let mut order: Vec<usize> = (0..intervals.len())
        .filter(|&i| intervals[i].assignment.is_none())
        .collect();
    order.sort_by_key(|&i| intervals[i].start);

    let mut active: Vec<usize> = Vec::new();
    let mut free_regs: Vec<u8> = (0..NUM_ALLOCATABLE_REGS).rev().collect();

    for i in order {
        let start = intervals[i].start;
        active.retain(|&j| {
            if intervals[j].end < start {
                if let Some(Assignment::Reg(r)) = intervals[j].assignment {
                    free_regs.push(r);
                }
                false
            } else {
                true
            }
        });

        if let Some(r) = free_regs.pop() {
            intervals[i].assignment = Some(Assignment::Reg(r));
            active.push(i);
        } else {
            let (pos_in_active, &furthest) = active
                .iter()
                .enumerate()
                .max_by_key(|&(_, &j)| intervals[j].end)
                .expect(
                    "allocate: no free register and no active interval to spill -- \
                     NUM_ALLOCATABLE_REGS must be wrong if this fires",
                );
            if intervals[furthest].end > intervals[i].end {
                let r = match intervals[furthest].assignment {
                    Some(Assignment::Reg(r)) => r,
                    _ => unreachable!("active intervals always hold a register"),
                };
                spill(&mut intervals[furthest], &mut slot_is_oop);
                active.remove(pos_in_active);
                intervals[i].assignment = Some(Assignment::Reg(r));
                active.push(i);
            } else {
                spill(&mut intervals[i], &mut slot_is_oop);
            }
        }
    }

    verify_spill_all(intervals);

    (slot_is_oop.len() as u16, slot_is_oop)
}

/// S12 D1's spill-all invariant, enforced HERE — not merely assumed, and
/// not merely `debug_assert!`ed: this is the exact guarantee S12's oop maps
/// stand on (registers are never live across a safepoint; maps cover stack
/// slots only), so it runs ALWAYS, release builds included, per the sprint
/// doc's own text ("a release-mode-cheap pass... trivial", O(intervals)).
/// A future regalloc change that lets a `crosses_safepoint` interval keep a
/// register would otherwise corrupt the heap silently instead of panicking
/// at the source — exactly the class of bug a debug-only check would only
/// catch in SOME builds.
pub fn verify_spill_all(intervals: &[LiveInterval]) {
    for iv in intervals {
        assert!(
            !(iv.crosses_safepoint && matches!(iv.assignment, Some(Assignment::Reg(_)))),
            "regalloc: {:?} crosses a safepoint but holds a register (S12's oop-map \
             invariant: registers are all spilled at safepoints)",
            iv.vreg
        );
    }
}

pub struct RegallocResult {
    pub block_order: Vec<BlockId>,
    /// Final intervals, `assignment` populated — indexed arbitrarily (by
    /// `compute_intervals`' own vreg-ascending order), not by vreg id;
    /// look up by `.vreg` if you need a specific one.
    pub intervals: Vec<LiveInterval>,
    pub frame_slots: u16,
    pub slot_is_oop: Vec<bool>,
    /// S12: every safepoint's exact linear position, in the SAME numbering
    /// `intervals`' own `start`/`end`/`crosses_safepoint` were computed
    /// against — `emit.rs` walks `block_order` identically (its own
    /// position counter) to correlate each REAL emitted safepoint with one
    /// of these, and `compiler::oopmap::build_for_position` intersects
    /// `intervals` against it to build that safepoint's own `OopMap`.
    pub safepoint_positions: Vec<u32>,
}

pub fn regalloc(method: &IrMethod) -> RegallocResult {
    let (block_order, mut intervals, safepoint_positions) = compute_intervals(method);
    let (frame_slots, slot_is_oop) = allocate(&mut intervals);
    RegallocResult {
        block_order,
        intervals,
        frame_slots,
        slot_is_oop,
        safepoint_positions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::ir::{BailoutReason, CmpOp, PoolLit, SmiOp, StubId, VRegInfo};

    fn hand_method(blocks: Vec<IrBlock>, vregs: Vec<VRegInfo>) -> IrMethod {
        IrMethod {
            blocks,
            vregs,
            pool: Vec::new(),
            argc: 0,
            ntemps: 0,
            safepoints: Vec::new(),
            true_lit: PoolLit(0),
            false_lit: PoolLit(0),
            nil_lit: PoolLit(0),
            mark_slots_lit: PoolLit(0),
            call_sites: Vec::new(),
            site_feedback: Vec::new(),
            inline_deps: Vec::new(),
            method_pool_ix: None,
        }
    }

    /// "hand IR: def at 2, uses at 5 and 9" -> interval `[2, 9]`.
    #[test]
    fn intervals_basic() {
        let v0 = VReg(0);
        let filler = VReg(1);
        let block = IrBlock {
            id: BlockId(0),
            bci: 0,
            code: vec![
                Ir::Poll,
                Ir::Poll,
                Ir::ConstSmi { dst: v0, value: 1 }, // pos 2: def
                Ir::Poll,
                Ir::Poll,
                Ir::Move {
                    dst: filler,
                    src: v0,
                }, // pos 5: use
                Ir::Poll,
                Ir::Poll,
                Ir::Poll,
                Ir::Ret { val: v0 }, // pos 9: use
            ],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        let method = hand_method(
            vec![block],
            vec![VRegInfo { is_oop: true }, VRegInfo { is_oop: true }],
        );

        let (_order, intervals, _safepoints) = compute_intervals(&method);
        let iv = intervals
            .iter()
            .find(|iv| iv.vreg == v0)
            .expect("v0 has an interval");
        assert_eq!(iv.start, 2);
        assert_eq!(iv.end, 9);
    }

    /// A temp vreg defined on two different blocks (SSA-lite's multiple-
    /// defs shape) gets ONE interval covering every def and the eventual
    /// use, not two separate intervals.
    #[test]
    fn interval_multi_def_union() {
        let v0 = VReg(0);
        let block0 = IrBlock {
            id: BlockId(0),
            bci: 0,
            code: vec![
                Ir::ConstSmi { dst: v0, value: 1 },
                Ir::Jump { target: BlockId(1) },
            ],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        let block1 = IrBlock {
            id: BlockId(1),
            bci: 10,
            code: vec![Ir::ConstSmi { dst: v0, value: 2 }, Ir::Ret { val: v0 }],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        let method = hand_method(vec![block0, block1], vec![VRegInfo { is_oop: true }]);

        let (order, intervals, _safepoints) = compute_intervals(&method);
        assert_eq!(
            order,
            vec![BlockId(0), BlockId(1)],
            "block0 must be linearized first"
        );
        assert_eq!(intervals.len(), 1, "one vreg -> one interval, never two");
        assert_eq!(intervals[0].start, 0);
        assert_eq!(intervals[0].end, 3);
    }

    /// THE S12 invariant, enforced early: every oop interval live across a
    /// `CallRuntime` gets `Spill`, never `Reg`.
    #[test]
    fn spill_all_crossing_safepoint() {
        let v0 = VReg(0);
        let block = IrBlock {
            id: BlockId(0),
            bci: 0,
            code: vec![
                Ir::ConstPool {
                    dst: v0,
                    lit: PoolLit(0),
                },
                Ir::CallRuntime {
                    dst: None,
                    stub: StubId(0),
                    args: vec![],
                },
                Ir::Ret { val: v0 },
            ],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        let method = hand_method(vec![block], vec![VRegInfo { is_oop: true }]);

        let (_order, mut intervals, _safepoints) = compute_intervals(&method);
        assert!(
            intervals[0].crosses_safepoint,
            "v0 is defined before and used after the call"
        );

        allocate(&mut intervals);
        assert!(matches!(
            intervals[0].assignment,
            Some(Assignment::Spill(_))
        ));
    }

    /// Oop-map raw material: `slot_is_oop` records each spill slot's
    /// interval's own `is_oop`, correctly per-slot.
    #[test]
    fn spill_slot_oopness_recorded() {
        let mut intervals = vec![
            LiveInterval {
                vreg: VReg(0),
                start: 0,
                end: 5,
                is_oop: true,
                crosses_safepoint: true,
                assignment: None,
            },
            LiveInterval {
                vreg: VReg(1),
                start: 0,
                end: 5,
                is_oop: false,
                crosses_safepoint: true,
                assignment: None,
            },
        ];

        let (frame_slots, slot_is_oop) = allocate(&mut intervals);
        assert_eq!(frame_slots, 2);
        assert_eq!(slot_is_oop.len(), 2);

        let slot_of = |iv: &LiveInterval| match iv.assignment {
            Some(Assignment::Spill(s)) => s.0 as usize,
            _ => panic!("expected a spill assignment"),
        };
        assert!(slot_is_oop[slot_of(&intervals[0])]);
        assert!(!slot_is_oop[slot_of(&intervals[1])]);
    }

    /// `tests_s12.md`'s `verify_spill_all_catches_reg` (D1 enforcement
    /// point 1): a hand-built interval claiming BOTH `crosses_safepoint`
    /// AND a `Reg` assignment is exactly the invariant violation S12's oop
    /// maps depend on never happening — `verify_spill_all` must panic on
    /// it directly, independent of whether `allocate` itself could ever
    /// actually produce such a state (this test constructs the bad state
    /// by hand, bypassing `allocate` entirely, to test the CHECK, not the
    /// policy that normally prevents it).
    #[test]
    #[should_panic(expected = "crosses a safepoint but holds a register")]
    fn verify_spill_all_catches_reg() {
        let intervals = vec![LiveInterval {
            vreg: VReg(0),
            start: 0,
            end: 10,
            is_oop: true,
            crosses_safepoint: true,
            assignment: Some(Assignment::Reg(3)),
        }];
        verify_spill_all(&intervals);
    }

    /// The same shape, but `Spill`-assigned (the correct outcome) — must
    /// NOT panic, so the test above is exercising the actual invariant,
    /// not merely "any crosses_safepoint interval panics".
    #[test]
    fn verify_spill_all_accepts_spilled_crossing_interval() {
        let intervals = vec![LiveInterval {
            vreg: VReg(0),
            start: 0,
            end: 10,
            is_oop: true,
            crosses_safepoint: true,
            assignment: Some(Assignment::Spill(SpillSlot(0))),
        }];
        verify_spill_all(&intervals); // must not panic
    }

    /// Linear-scan core: with only 16 allocatable registers, 17 mutually
    /// overlapping (call-free) intervals force exactly one spill — the
    /// furthest-ending one, whether it's encountered first or last.
    #[test]
    fn furthest_end_spilled_under_pressure() {
        let mut intervals = vec![LiveInterval {
            vreg: VReg(0),
            start: 0,
            end: 999,
            is_oop: false,
            crosses_safepoint: false,
            assignment: None,
        }];
        for i in 1..17u32 {
            intervals.push(LiveInterval {
                vreg: VReg(i),
                start: 0,
                end: 10,
                is_oop: false,
                crosses_safepoint: false,
                assignment: None,
            });
        }
        assert_eq!(intervals.len(), 17);

        let (frame_slots, _slot_is_oop) = allocate(&mut intervals);
        assert_eq!(frame_slots, 1, "exactly one spill");

        let spilled: Vec<&LiveInterval> = intervals
            .iter()
            .filter(|iv| matches!(iv.assignment, Some(Assignment::Spill(_))))
            .collect();
        assert_eq!(spilled.len(), 1);
        assert_eq!(
            spilled[0].vreg,
            VReg(0),
            "the furthest-ending interval (end=999) is spilled"
        );

        let regs: std::collections::HashSet<u8> = intervals[1..]
            .iter()
            .map(|iv| match iv.assignment {
                Some(Assignment::Reg(r)) => r,
                _ => panic!("expected every other interval to hold a register"),
            })
            .collect();
        assert_eq!(regs.len(), 16, "all 16 registers used, none double-booked");
    }

    /// Sanity check that `reverse_postorder`/`compute_intervals` don't
    /// panic or hang on a block unreachable from the entry (decode's own
    /// `unreachable_after_return` shape, surviving into the IR).
    #[test]
    fn unreachable_block_gets_a_position_not_a_panic() {
        let block0 = IrBlock {
            id: BlockId(0),
            bci: 0,
            code: vec![Ir::RetSelf],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        let dead = IrBlock {
            id: BlockId(1),
            bci: 5,
            code: vec![Ir::Bailout {
                reason: BailoutReason::SmiOpFailed,
            }],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        let method = hand_method(vec![block0, dead], Vec::new());
        let (order, _intervals, _safepoints) = compute_intervals(&method);
        assert_eq!(
            order.len(),
            2,
            "both blocks get a position, reachable or not"
        );
        // The real bug this hand-built shape once caught (S10 step 9): with
        // NO graph edge from block 0 to `dead` (RetSelf/Bailout are both
        // absent from `successors`' match), block 0 is its own singleton
        // DFS component — a version of `reverse_postorder` that reversed
        // the whole accumulated postorder only once, at the end, put block
        // 0 SECOND, meaning emit.rs's prologue fell straight through into
        // the OTHER block first. `emit.rs` has no guard against this: it
        // just emits `block_order` in order, right after the prologue.
        assert_eq!(
            order[0],
            BlockId(0),
            "block 0 (the method's real entry) must always be emitted first"
        );
    }

    /// The second, deeper S10 step 9 bug this hand-built shape catches: an
    /// accumulator vreg (`s`, matching `sumTo:`'s own `s := s + i` loop)
    /// that is both defined AND used inside the loop body block, and ALSO
    /// read once more after the loop, at the exit block. `reverse_postorder`
    /// only promises block 0 first and predecessors-before-successors
    /// except across back edges — for a loop header with two successors
    /// (body, exit), it does not promise the body block comes before the
    /// exit block in the LINEAR position space (and for this exact shape,
    /// it doesn't: the exit block, a DFS dead end, finishes and gets
    /// pushed to postorder before the body block's own back-edge-laden
    /// subtree does). A `[min_def, max_use]` fold that never widens for
    /// back edges puts `s`'s LAST use at the exit block's read — entirely
    /// missing that the body block, which the linearization places AFTER
    /// the exit block, both reads AND redefines `s` on every iteration.
    /// Without the loop-range widening this test checks for, `s`'s
    /// register could be (and, before this fix, was) handed to some other
    /// vreg live only in the "later" body block, silently corrupting the
    /// accumulator — `sumTo: 10` returned 11 (the loop counter's own final
    /// value) instead of 55 the first time this ran through the real
    /// compiler, in `world/tests/tier1.mst`.
    #[test]
    fn loop_carried_vreg_interval_spans_whole_loop() {
        let s = VReg(0); // accumulator, live across the back edge
        let i = VReg(1); // loop counter
        let bound = VReg(2);
        let tmp = VReg(3);
        let one = VReg(4);
        let result = VReg(5);

        let entry = IrBlock {
            id: BlockId(0),
            bci: 0,
            code: vec![
                Ir::ConstSmi { dst: s, value: 0 },
                Ir::ConstSmi { dst: i, value: 1 },
                Ir::ConstSmi {
                    dst: bound,
                    value: 10,
                },
                Ir::Jump { target: BlockId(1) },
            ],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        let header = IrBlock {
            id: BlockId(1),
            bci: 10,
            code: vec![Ir::SmiCmpBr {
                op: CmpOp::Le,
                a: i,
                b: bound,
                if_true: BlockId(2),
                if_false: BlockId(3),
                fail: BlockId(4),
            }],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        let body = IrBlock {
            id: BlockId(2),
            bci: 20,
            code: vec![
                Ir::SmiArith {
                    op: SmiOp::Add,
                    dst: tmp,
                    a: s,
                    b: i,
                    fail: BlockId(4),
                },
                Ir::Move { dst: s, src: tmp }, // redefines s, deep in the body
                Ir::ConstSmi { dst: one, value: 1 },
                Ir::SmiArith {
                    op: SmiOp::Add,
                    dst: tmp,
                    a: i,
                    b: one,
                    fail: BlockId(4),
                },
                Ir::Move { dst: i, src: tmp },
                Ir::Jump { target: BlockId(1) }, // the back edge
            ],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        let exit = IrBlock {
            id: BlockId(3),
            bci: 30,
            code: vec![
                Ir::Move {
                    dst: result,
                    src: s,
                }, // reads s once, "before" the body in linear position
                Ir::Ret { val: result },
            ],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };
        let bailout = IrBlock {
            id: BlockId(4),
            bci: 40,
            code: vec![Ir::Bailout {
                reason: BailoutReason::SmiOpFailed,
            }],
            entry_stack: Vec::new(),
            deopt_sites: Vec::new(),
        };

        let method = hand_method(
            vec![entry, header, body, exit, bailout],
            (0..6).map(|_| VRegInfo { is_oop: true }).collect(),
        );
        let (order, intervals, _safepoints) = compute_intervals(&method);

        // Confirms this hand-built shape actually reproduces the bug's own
        // precondition: the exit block linearized before the body block.
        let pos_of = |bid: BlockId| order.iter().position(|&b| b == bid).unwrap();
        assert!(
            pos_of(BlockId(3)) < pos_of(BlockId(2)),
            "this test's whole point is a linearization where the loop exit \
             precedes the loop body — order was {order:?}"
        );

        let s_iv = intervals
            .iter()
            .find(|iv| iv.vreg == s)
            .expect("s has an interval");
        let body_last_pos = {
            // Sum instruction counts for every block up to and including
            // the body block (BlockId(2)), in `order`'s own sequence, then
            // back off one for its own LAST instruction's position (not
            // one-past-the-end).
            let mut pos = 0u32;
            for &bid in &order {
                let blk = &method.blocks[bid.0 as usize];
                pos += blk.code.len() as u32;
                if bid == BlockId(2) {
                    break;
                }
            }
            pos - 1
        };
        assert!(
            s_iv.end >= body_last_pos,
            "s's interval (end={}) must extend at least through the loop \
             body's own last position ({body_last_pos}) -- otherwise its \
             register is free to be handed to something else mid-loop",
            s_iv.end
        );
    }
}
