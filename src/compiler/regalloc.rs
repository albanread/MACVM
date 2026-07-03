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
        Ir::CallSend { .. } | Ir::CallRuntime { .. } | Ir::Alloc { .. }
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
            Ir::Alloc { slow, .. } => succs.push(*slow),
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
    let mut postorder = Vec::with_capacity(n);
    for b in 0..n {
        dfs(b, &method.blocks, &mut visited, &mut postorder);
    }
    postorder.reverse();
    postorder
}

/// D3.4: number every instruction sequentially (walking blocks in
/// `reverse_postorder`), then fold each vreg's defs/uses into one
/// conservative `[min, max]` interval — a single linear pass, no separate
/// per-block live-in/live-out fixpoint (every def and use is already
/// explicit in the Ir stream — `ir.rs`'s own Move-based merge handling
/// means nothing needs inferring across a block boundary).
pub fn compute_intervals(method: &IrMethod) -> (Vec<BlockId>, Vec<LiveInterval>) {
    let block_order = reverse_postorder(method);

    let mut pos: u32 = 0;
    let mut safepoint_positions: Vec<u32> = Vec::new();
    let mut min_def: HashMap<u32, u32> = HashMap::new();
    let mut max_use: HashMap<u32, u32> = HashMap::new();

    for &bid in &block_order {
        let block = &method.blocks[bid.0 as usize];
        for ir in &block.code {
            if is_safepoint(ir) {
                safepoint_positions.push(pos);
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

    (block_order, intervals)
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

    for iv in intervals.iter() {
        debug_assert!(
            !(iv.crosses_safepoint && matches!(iv.assignment, Some(Assignment::Reg(_)))),
            "regalloc: {:?} crosses a safepoint but holds a register (S12's oop-map \
             invariant: registers are all spilled at safepoints)",
            iv.vreg
        );
    }

    (slot_is_oop.len() as u16, slot_is_oop)
}

pub struct RegallocResult {
    pub block_order: Vec<BlockId>,
    /// Final intervals, `assignment` populated — indexed arbitrarily (by
    /// `compute_intervals`' own vreg-ascending order), not by vreg id;
    /// look up by `.vreg` if you need a specific one.
    pub intervals: Vec<LiveInterval>,
    pub frame_slots: u16,
    pub slot_is_oop: Vec<bool>,
}

pub fn regalloc(method: &IrMethod) -> RegallocResult {
    let (block_order, mut intervals) = compute_intervals(method);
    let (frame_slots, slot_is_oop) = allocate(&mut intervals);
    RegallocResult {
        block_order,
        intervals,
        frame_slots,
        slot_is_oop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::ir::{BailoutReason, PoolLit, StubId, VRegInfo};

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
        };
        let method = hand_method(
            vec![block],
            vec![VRegInfo { is_oop: true }, VRegInfo { is_oop: true }],
        );

        let (_order, intervals) = compute_intervals(&method);
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
        };
        let block1 = IrBlock {
            id: BlockId(1),
            bci: 10,
            code: vec![Ir::ConstSmi { dst: v0, value: 2 }, Ir::Ret { val: v0 }],
            entry_stack: Vec::new(),
        };
        let method = hand_method(vec![block0, block1], vec![VRegInfo { is_oop: true }]);

        let (order, intervals) = compute_intervals(&method);
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
        };
        let method = hand_method(vec![block], vec![VRegInfo { is_oop: true }]);

        let (_order, mut intervals) = compute_intervals(&method);
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
        };
        let dead = IrBlock {
            id: BlockId(1),
            bci: 5,
            code: vec![Ir::Bailout {
                reason: BailoutReason::SmiOpFailed,
            }],
            entry_stack: Vec::new(),
        };
        let method = hand_method(vec![block0, dead], Vec::new());
        let (order, _intervals) = compute_intervals(&method);
        assert_eq!(
            order.len(),
            2,
            "both blocks get a position, reachable or not"
        );
    }
}
