//! Bytecode → CFG of basic blocks (`sprint_s10_detail.md` D3.1) — the first
//! stage of the tier-1 pipeline. Two passes over a method's (immutable,
//! SPEC §4) bytecode using the S2 decoder (`bytecode::opcode::decode_at`,
//! written with exactly this consumer in mind).
//!
//! Deliberately independent of [`crate::compiler::ir`]: this module only
//! discovers block boundaries and edges from raw bytecode shape — it knows
//! nothing about vregs, smi-send inlining, or eligibility (that's D1,
//! `compiler::driver`'s job, checked *before* a method ever reaches here).
//! [`BlockIndex`] is a plain position into [`Cfg::blocks`]; `ir.rs`'s
//! conversion assigns its own `ir::BlockId` 1:1 by position when it walks
//! this output, rather than this module depending on `ir`'s types.

use std::collections::{BTreeSet, HashSet};

use crate::bytecode::opcode::{decode_at, Instr};
use crate::oops::wrappers::MethodOop;

/// Position in a [`Cfg`]'s block list.
pub type BlockIndex = usize;

/// How a basic block's bytecode ends, and where control goes next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Terminator {
    /// No jump/branch/return opcode at all — this block's bytecode simply
    /// runs into the next leader (e.g. a `push_*`/`send` block that falls
    /// straight into a branch target it isn't itself the source of).
    Fallthrough(BlockIndex),
    /// `jump_fwd`/`jump_back`. `is_backward` is `decode.rs`'s only opinion
    /// on loops — it flags the edge; inserting an `Ir::Poll` before it is
    /// `ir.rs`'s job (D3.1/D3.2), not this module's.
    Jump {
        target: BlockIndex,
        is_backward: bool,
    },
    /// `br_true_fwd`/`br_false_fwd`, normalized to "taken" vs "not taken"
    /// regardless of which of the two actual opcodes produced it — SPEC's
    /// branches are always forward, so there is no backward variant.
    Branch {
        if_true: BlockIndex,
        if_false: BlockIndex,
    },
    /// `return_tos`/`return_self`/`block_return_tos`/`nlr_tos` — no
    /// successor within this method's CFG. The four are CFG-equivalent
    /// here (none fall through or jump elsewhere); which one it actually
    /// was is a `compiler::driver` eligibility concern (D1 excludes block
    /// returns and NLR from compilation), not a decode-time one.
    Return,
}

#[derive(Clone, Debug)]
pub struct BasicBlock {
    pub bci_start: usize,
    /// Exclusive — equal to the next block's `bci_start`, or the method's
    /// bytecode length for the last block.
    pub bci_end: usize,
    /// Set iff some block's `Terminator::Jump { is_backward: true, .. }`
    /// targets this block (D3.1).
    pub is_loop_header: bool,
    pub terminator: Terminator,
}

#[derive(Clone, Debug)]
pub struct Cfg {
    /// `blocks[0]` is always the entry block (`bci_start == 0`), and blocks
    /// are sorted by `bci_start`.
    pub blocks: Vec<BasicBlock>,
}

/// Pass 1 (D3.1): bci 0, every jump/branch target, and every bci
/// immediately following a `jump_*`/`br_*`/return-family opcode.
fn find_leaders(m: MethodOop) -> BTreeSet<usize> {
    let mut leaders = BTreeSet::new();
    leaders.insert(0);
    let len = m.bytecode_len();
    let mut bci = 0;
    while bci < len {
        let (instr, next) = decode_at(m, bci);
        match instr {
            Instr::JumpFwd(d) => {
                leaders.insert(next + d as usize);
                leaders.insert(next);
            }
            Instr::JumpBack(d) => {
                leaders.insert(next - d as usize);
                leaders.insert(next);
            }
            Instr::BrTrueFwd(d) | Instr::BrFalseFwd(d) => {
                leaders.insert(next + d as usize);
                leaders.insert(next);
            }
            Instr::ReturnTos | Instr::ReturnSelf | Instr::BlockReturnTos | Instr::NlrTos => {
                leaders.insert(next);
            }
            _ => {}
        }
        bci = next;
    }
    leaders
}

/// Pass 2 (D3.1): split at leaders, decode each block's own bytecode to
/// find its terminator, then mark loop headers from the finished edge set.
/// Malformed bytecode (a jump/branch target that isn't a leader — i.e. not
/// an instruction boundary `find_leaders` itself discovered) is a `panic!`:
/// the frontend is trusted, matching `decode_at`'s own policy for an
/// undefined opcode.
pub fn decode(m: MethodOop) -> Cfg {
    let len = m.bytecode_len();
    // `find_leaders` can insert `len` itself (the bci "following" a
    // terminator that happens to be the method's last instruction) — that
    // is a fencepost, not a real block.
    let starts: Vec<usize> = find_leaders(m)
        .into_iter()
        .filter(|&bci| bci < len)
        .collect();
    let index_of = |bci: usize| -> BlockIndex {
        starts.binary_search(&bci).unwrap_or_else(|_| {
            panic!("decode: jump/branch target {bci} is not a leader (malformed bytecode)")
        })
    };

    let mut blocks = Vec::with_capacity(starts.len());
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(len);
        let mut bci = start;
        let mut last: Option<(Instr, usize)> = None;
        while bci < end {
            let (instr, next) = decode_at(m, bci);
            last = Some((instr, next));
            bci = next;
        }
        let terminator = match last {
            // Two adjacent leaders with nothing between them (e.g. a
            // branch's not-taken target coincides with a jump's own
            // target) — an empty block that falls straight through.
            None => Terminator::Fallthrough(i + 1),
            Some((Instr::JumpFwd(d), next)) => Terminator::Jump {
                target: index_of(next + d as usize),
                is_backward: false,
            },
            Some((Instr::JumpBack(d), next)) => Terminator::Jump {
                target: index_of(next - d as usize),
                is_backward: true,
            },
            Some((Instr::BrTrueFwd(d), next)) => Terminator::Branch {
                if_true: index_of(next + d as usize),
                if_false: index_of(next),
            },
            Some((Instr::BrFalseFwd(d), next)) => Terminator::Branch {
                if_true: index_of(next),
                if_false: index_of(next + d as usize),
            },
            Some((
                Instr::ReturnTos | Instr::ReturnSelf | Instr::BlockReturnTos | Instr::NlrTos,
                _,
            )) => Terminator::Return,
            Some((_, next)) => {
                // Ran out of bytecode at a non-terminating instruction —
                // only reachable if a leader falls right after one without
                // an intervening jump/branch/return, which `find_leaders`
                // never produces for well-formed bytecode. Stay defensive
                // (fall through) rather than panic: `unreachable_after_
                // return`'s contract is "no panic", and a stray leader
                // here is harmless either way.
                debug_assert_eq!(next, end, "block does not end exactly at the next leader");
                Terminator::Fallthrough(i + 1)
            }
        };
        blocks.push(BasicBlock {
            bci_start: start,
            bci_end: end,
            is_loop_header: false,
            terminator,
        });
    }

    let loop_headers: HashSet<BlockIndex> = blocks
        .iter()
        .filter_map(|b| match b.terminator {
            Terminator::Jump {
                target,
                is_backward: true,
            } => Some(target),
            _ => None,
        })
        .collect();
    for (i, b) in blocks.iter_mut().enumerate() {
        b.is_loop_header = loop_headers.contains(&i);
    }

    Cfg { blocks }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::BytecodeBuilder;
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

    /// The `ifTrue:ifFalse:` value-pattern diamond: a `br_false_fwd`
    /// (branch) and a `jump_fwd` (merge) split the method into exactly 4
    /// blocks with the expected edges — condition -> {true, false} ->
    /// merge -> return.
    #[test]
    fn leaders_if_else() {
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

        let cfg = decode(method);
        assert_eq!(cfg.blocks.len(), 4, "condition/true/false/merge");

        assert_eq!(
            cfg.blocks[0].terminator,
            Terminator::Branch {
                if_true: 1,
                if_false: 2
            },
            "block 0: push_true; br_false_fwd -> true falls to block 1, false jumps to block 2"
        );
        assert_eq!(
            cfg.blocks[1].terminator,
            Terminator::Jump {
                target: 3,
                is_backward: false
            },
            "block 1 (true arm): jump_fwd to the merge block"
        );
        assert_eq!(
            cfg.blocks[2].terminator,
            Terminator::Fallthrough(3),
            "block 2 (false arm): falls straight into the merge block"
        );
        assert_eq!(cfg.blocks[3].terminator, Terminator::Return);
        assert!(cfg.blocks.iter().all(|b| !b.is_loop_header));
    }

    /// `jump_back` marks its target block `is_loop_header`, and decode.rs
    /// flags the edge `is_backward` — the raw material `ir.rs` uses to
    /// insert an `Ir::Poll` before the jump (D3.1/D3.2; decode.rs itself
    /// has no `Ir` type at all and inserts nothing).
    #[test]
    fn leaders_while_loop() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        let header = b.new_label();
        let after = b.new_label();
        b.bind(header);
        b.push_true();
        b.br_false_fwd(after);
        b.push_smi_i8(1);
        b.jump_back(header);
        b.bind(after);
        b.ret_self();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 0, 0);

        let cfg = decode(method);

        let header_block = cfg
            .blocks
            .iter()
            .position(|blk| blk.bci_start == 0)
            .expect("header is bci 0");
        assert!(cfg.blocks[header_block].is_loop_header);

        let back_edge = cfg
            .blocks
            .iter()
            .find_map(|blk| match blk.terminator {
                Terminator::Jump {
                    target,
                    is_backward: true,
                } => Some(target),
                _ => None,
            })
            .expect("exactly one backward jump");
        assert_eq!(back_edge, header_block);
    }

    /// Bytecode after an early `return_tos`, before the method's real end
    /// (the frontend emits this for a trailing implicit `^self` appended
    /// after a method whose last real statement already returns) — decode
    /// must not panic; the trailing block simply ends up with no
    /// predecessor in the edge set built here.
    #[test]
    fn unreachable_after_return() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        b.push_smi_i8(5);
        b.ret_tos();
        b.push_smi_i8(99); // unreachable
        b.ret_self();
        let sel = vm.universe.intern(b"m");
        let method = b.finish(&mut vm, sel, 0, 0);

        let cfg = decode(method);
        assert_eq!(cfg.blocks.len(), 2);
        assert_eq!(cfg.blocks[0].terminator, Terminator::Return);
        assert_eq!(cfg.blocks[1].terminator, Terminator::Return);
        // No block's terminator ever names block 1 as a successor.
        assert!(cfg.blocks.iter().all(|blk| !matches!(
            blk.terminator,
            Terminator::Fallthrough(1)
                | Terminator::Jump { target: 1, .. }
                | Terminator::Branch { if_true: 1, .. }
                | Terminator::Branch { if_false: 1, .. }
        )));
    }
}
