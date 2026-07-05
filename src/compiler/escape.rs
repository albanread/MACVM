//! S14 step 7-I: closure escape analysis (`s14_step7I_spec.md` "Escape
//! analysis" — the soundness crux). A PURE pre-pass over a method M's decoded
//! CFG (no allocation, no IR) that proves whether every literal closure M
//! creates via `push_closure` is IMMEDIATELY INVOKED via a `value`-family send
//! in M and NEVER escapes (stored, returned, passed as an arg, merged into an
//! opaque value, or itself unspliceable). If — and only if — every site is so
//! provable, the tier-1 gate lets M compile, splicing each proven block's body
//! inline at its value-send (`ir::convert`). A closure that escapes needs a
//! process-stack `home_frame_ref` a native compiled frame cannot cheaply
//! provide (SPEC §8.4), so if we CANNOT elide a closure we must NOT compile the
//! method — an escaping closure wrongly elided is a heap-corruption-grade bug.
//!
//! The abstract interpretation carries an [`AV`] (`Site(push_closure bci)` or
//! `Other`) on every operand-stack slot AND every local temp slot, iterating to
//! a fixpoint over CFG merges. The escape rules (which uses of a `Site` are
//! GOOD — only a matching-argc `value`-family send with the site as the
//! RECEIVER and no `Site` among the args — and which ESCAPE) are exactly the
//! spec's. Elidability of a site further requires the block itself be
//! spliceable (no captured/owned Context, no closure/ctx/nlr/super opcode in
//! its body).

use std::collections::{HashMap, HashSet};

use crate::bytecode::opcode::{decode_at, Instr};
use crate::compiler::decode::{self, Cfg, Terminator};
use crate::interpreter::ic::InterpreterIc;
use crate::oops::wrappers::MethodOop;

/// Abstract value of one operand-stack slot or temp slot. `Site(bci)` names the
/// `push_closure` bci that produced this (still-tracked) closure reference;
/// `Other` is any value whose closure-identity we don't (or no longer) track.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AV {
    Site(usize),
    Other,
}

/// The result of [`analyze`]. `all_elidable` is the gate the driver reads: true
/// iff every `push_closure` site in M is a GOOD use (an immediately-invoked,
/// non-escaping, spliceable non-capturing literal block). `value_send_target`
/// maps a `value`-send bci → the site id it splices (only for proven good
/// uses), which `ir::convert` consults to splice the block body.
pub struct ClosureEscape {
    pub all_elidable: bool,
    /// Every `push_closure` bci discovered in M. Read by the unit tests (and a
    /// future `MACVM_TRACE=jit` site-count log); `all_elidable` is the only
    /// field the driver consults, so this is otherwise inert in a release lib.
    #[cfg_attr(not(test), allow(dead_code))]
    sites: HashSet<usize>,
    /// Site bcis proven to escape (an unspliceable block is recorded here too).
    escaping: HashSet<usize>,
    /// `value`-send bci → the site id it invokes, recorded ONLY for good uses
    /// (matching-argc value-family send whose receiver is a live `Site`).
    value_sends: HashMap<usize, usize>,
}

impl ClosureEscape {
    /// The site id a `value`-send at `bci` splices — `Some` only for a proven
    /// good use (a matching-argc value-family send on a live, elidable Site).
    /// `ir::convert`'s splicer uses this; a `None` here at a value-send bci
    /// means the analysis did NOT prove that send inlinable (it stays a plain
    /// send), which can only happen for a site that is ALSO in `escaping`, so a
    /// method with such a send is not `all_elidable` and never compiles anyway.
    pub fn value_send_target(&self, bci: usize) -> Option<usize> {
        self.value_sends
            .get(&bci)
            .copied()
            .filter(|s| !self.escaping.contains(s))
    }
}

/// The `value`-family selectors 7-I splices (`value`, `value:`, `value:value:`,
/// `value:value:value:`). `valueWithArguments:` is deliberately excluded (spec).
/// Matched by exact selector bytes, NOT by argc alone — a non-value send that
/// happens to share a block's argc must NEVER be mistaken for a good use.
fn value_family_argc(sel_bytes: &[u8]) -> Option<usize> {
    match sel_bytes {
        b"value" => Some(0),
        b"value:" => Some(1),
        b"value:value:" => Some(2),
        b"value:value:value:" => Some(3),
        _ => None,
    }
}

/// Copy `sel`'s bytes out for a value-family name comparison (selectors are
/// short; this is a compile-time pre-pass, not a hot path). The `value`-family
/// selectors are pure ASCII, so `as_string`'s UTF-8 round-trip is exact.
fn selector_bytes(method: MethodOop, ic: u16) -> Vec<u8> {
    InterpreterIc::at(method, ic)
        .selector()
        .as_string()
        .into_bytes()
}

/// Is the CompiledBlock at `push_closure` site `s` (the literal `MethodOop`)
/// itself spliceable? Defers captured/owned-Context blocks and any body
/// containing a super-send / closure / ctx-temp / NLR opcode (later slices). A
/// `block_return_tos` IS allowed (it is the block's own return). Requires the
/// single-straight-line shape the splicer handles: exactly one basic block
/// ending in a return terminator.
///
/// **7-II**: an ordinary (non-super) `Send` inside the block body IS allowed —
/// `splice_block` lowers it to a `CallSend`/cold-trap inside the inlined block,
/// recording an `is_block` in-body deopt scope. A deopt there rebuilds the
/// block's own (method-shaped) activation frame soundly, because the block
/// frame's only structural difference — the closure in its receiver-arg slot —
/// is read ONLY by `nlr_tos` / nested `push_closure`, both still gated out.
/// (7-I had restricted this to send-free blocks while `runtime/deopt.rs`'s
/// `is_block` handling was unproven; 7-II proves it and lifts the restriction.)
fn block_is_spliceable(block: MethodOop) -> bool {
    // Non-capturing, no owned Context. A block that captures the enclosing
    // Context or owns its own heap Context needs captured-temp promotion (the
    // next slice) — gated out here.
    // A block that owns its OWN heap Context (`has_ctx`) needs nested-context
    // depth machinery — gated. A `captures_ctx` block (it reads/writes the HOME
    // method's captured temps) IS spliceable in 7-II-b (its ctx-temps promote to
    // the home's vregs).
    if block.has_ctx() {
        return false;
    }
    // Single straight-line block ending in a return — the exact shape
    // `splice_block` handles. Validated against the SAME decode CFG the splicer
    // re-validates against, so the two agree by construction.
    let cfg = decode::decode(block);
    if cfg.blocks.len() != 1 || !matches!(cfg.blocks[0].terminator, Terminator::Return) {
        return false;
    }
    // A `captures_ctx` block accesses M's ctx-temps; 7-II-b-i restricts such a
    // block to be SEND-FREE (an in-block deopt of a capturing block needs the
    // block frame's Context to alias M's, which is 7-II-b-ii's materializer
    // change). A NON-capturing block may still have ordinary sends (7-II).
    let captures = block.captures_ctx();
    let len = block.bytecode_len();
    let mut bci = 0;
    while bci < len {
        let (instr, next) = decode_at(block, bci);
        match instr {
            // Always gated: super sends, nested closures, NLR.
            Instr::Send { super_: true, .. } | Instr::PushClosure { .. } | Instr::NlrTos => {
                return false
            }
            // A capturing block must be send-free (7-II-b-i).
            Instr::Send { .. } if captures => return false,
            // ctx-temp access is DEPTH 0 only (M's own Context, which a ctx-less
            // block's frame aliases). A `depth != 0` (nested context) is gated.
            Instr::PushCtxTemp { depth, .. } | Instr::StoreCtxTempPop { depth, .. }
                if depth != 0 =>
            {
                return false
            }
            _ => {}
        }
        bci = next;
    }
    true
}

/// Per-block abstract entry state: the operand stack (bottom-first) and the
/// unified arg/temp array, both carrying [`AV`]s.
#[derive(Clone, PartialEq, Eq)]
struct State {
    stack: Vec<AV>,
    temps: Vec<AV>,
}

/// Merge `incoming` into `slot`, marking `escaping` when two DIFFERENT abstract
/// values meet on a slot that held a Site — we lose precise track of that Site,
/// so we can no longer prove all its uses are good. Returns the merged value.
fn merge_slot(slot: AV, incoming: AV, escaping: &mut HashSet<usize>) -> AV {
    match (slot, incoming) {
        (AV::Site(a), AV::Site(b)) if a == b => AV::Site(a),
        (a, b) if a == b => a,
        // A Site meeting anything else (or a different Site): both sites lose
        // precise tracking → escape, and the merged slot is opaque.
        (a, b) => {
            if let AV::Site(s) = a {
                escaping.insert(s);
            }
            if let AV::Site(s) = b {
                escaping.insert(s);
            }
            AV::Other
        }
    }
}

/// Merge `incoming` into block `b`'s recorded entry `State`, growing the
/// worklist if anything changed. Depths always agree (the frontend guarantees a
/// consistent operand-stack depth at every CFG join, same invariant
/// `ir::compute_entry_depths` asserts), so the merge is slot-wise.
fn merge_into(
    entry: &mut [Option<State>],
    escaping: &mut HashSet<usize>,
    worklist: &mut Vec<usize>,
    b: usize,
    incoming: &State,
) {
    match &mut entry[b] {
        None => {
            entry[b] = Some(incoming.clone());
            worklist.push(b);
        }
        Some(existing) => {
            debug_assert_eq!(
                existing.stack.len(),
                incoming.stack.len(),
                "escape: operand-stack depth mismatch at CFG join (malformed bytecode)"
            );
            debug_assert_eq!(existing.temps.len(), incoming.temps.len());
            let mut changed = false;
            for i in 0..existing.stack.len() {
                let merged = merge_slot(existing.stack[i], incoming.stack[i], escaping);
                if merged != existing.stack[i] {
                    existing.stack[i] = merged;
                    changed = true;
                }
            }
            for i in 0..existing.temps.len() {
                let merged = merge_slot(existing.temps[i], incoming.temps[i], escaping);
                if merged != existing.temps[i] {
                    existing.temps[i] = merged;
                    changed = true;
                }
            }
            if changed {
                worklist.push(b);
            }
        }
    }
}

/// The escape pre-pass. `method` is M (the ROOT method being considered for
/// compilation) — NOT a block. Abstract-interprets M's CFG to a fixpoint,
/// classifying every `push_closure` site as elidable (good use) or escaping.
///
/// A method with NO `push_closure` yields `all_elidable == true` trivially (an
/// empty `sites` set) — the driver only runs this when M contains a
/// `push_closure`, but the result is well-defined either way.
pub fn analyze(method: MethodOop) -> ClosureEscape {
    let cfg = decode::decode(method);
    let n_slots = method.argc() + method.ntemps();

    let mut sites: HashSet<usize> = HashSet::new();
    let mut escaping: HashSet<usize> = HashSet::new();
    let mut value_sends: HashMap<usize, usize> = HashMap::new();

    // Discover every push_closure site up front + validate block spliceability
    // once. An unspliceable block's site is escaping from the start (it is not
    // elidable), which also stops us from recording a value-send on it as good.
    {
        let len = method.bytecode_len();
        let mut bci = 0;
        while bci < len {
            let (instr, next) = decode_at(method, bci);
            if let Instr::PushClosure { lit, .. } = instr {
                sites.insert(bci);
                let block = MethodOop::try_from(method.literals().at(lit as usize))
                    .expect("push_closure literal is not a CompiledBlock");
                if !block_is_spliceable(block) {
                    escaping.insert(bci);
                }
            }
            bci = next;
        }
    }

    // No closures → nothing to prove. (The driver gates on a push_closure being
    // present, but keep the function total.)
    if sites.is_empty() {
        return ClosureEscape {
            all_elidable: true,
            sites,
            escaping,
            value_sends,
        };
    }

    let mut entry: Vec<Option<State>> = vec![None; cfg.blocks.len()];
    entry[0] = Some(State {
        stack: Vec::new(),
        // Method args/temps start Other (an incoming arg is never a
        // locally-created closure Site).
        temps: vec![AV::Other; n_slots],
    });
    let mut worklist = vec![0usize];

    while let Some(b) = worklist.pop() {
        let mut st = entry[b]
            .clone()
            .expect("escape: worklisted block has no entry state");
        transfer_block(
            method,
            &cfg,
            b,
            &mut st,
            &sites,
            &mut escaping,
            &mut value_sends,
        );
        // Propagate the block's exit state to its successors.
        let cfg_block = &cfg.blocks[b];
        match cfg_block.terminator {
            Terminator::Return => {}
            Terminator::Fallthrough(t) | Terminator::Jump { target: t, .. } => {
                merge_into(&mut entry, &mut escaping, &mut worklist, t, &st);
            }
            Terminator::Branch { if_true, if_false } => {
                // The branch pops the condition (a `br_*` consumes one slot).
                let mut succ = st.clone();
                pop_escape(&mut succ, &mut escaping);
                merge_into(&mut entry, &mut escaping, &mut worklist, if_true, &succ);
                merge_into(&mut entry, &mut escaping, &mut worklist, if_false, &succ);
            }
        }
    }

    let all_elidable = sites.iter().all(|s| !escaping.contains(s));
    ClosureEscape {
        all_elidable,
        sites,
        escaping,
        value_sends,
    }
}

/// Pop one slot; if it was a Site, it is discarded here as a branch condition —
/// which ESCAPES (a Site used as a boolean is not a good use). A well-formed
/// program never branches on a closure, but the analysis stays sound if it does.
fn pop_escape(st: &mut State, escaping: &mut HashSet<usize>) {
    if let Some(AV::Site(s)) = st.stack.pop() {
        escaping.insert(s);
    }
}

/// Abstract-interpret block `b`'s straight-line body, mutating `st` in place and
/// recording escapes / good value-sends. Runs the SAME `decode_at` walk the CFG
/// was built from, over `[bci_start, bci_end)`.
#[allow(clippy::too_many_arguments)]
fn transfer_block(
    method: MethodOop,
    cfg: &Cfg,
    b: usize,
    st: &mut State,
    sites: &HashSet<usize>,
    escaping: &mut HashSet<usize>,
    value_sends: &mut HashMap<usize, usize>,
) {
    let cfg_block = &cfg.blocks[b];
    let mut bci = cfg_block.bci_start;
    while bci < cfg_block.bci_end {
        let (instr, next) = decode_at(method, bci);
        match instr {
            Instr::PushClosure { .. } => {
                debug_assert!(sites.contains(&bci));
                st.stack.push(AV::Site(bci));
            }
            Instr::PushTemp(t) => st.stack.push(st.temps[t as usize]),
            Instr::StoreTempPop(t) => {
                let v = st.stack.pop().unwrap_or(AV::Other);
                st.temps[t as usize] = v;
            }
            Instr::StoreTemp(t) => {
                let v = *st.stack.last().unwrap_or(&AV::Other);
                st.temps[t as usize] = v;
            }
            Instr::Dup => {
                let v = *st.stack.last().unwrap_or(&AV::Other);
                st.stack.push(v);
            }
            Instr::Pop => {
                // Popping a Site = the closure is dead (SPEC A7 "dead" is
                // elidable) — OK, NO escape.
                st.stack.pop();
            }
            // Pushes that never carry a Site — push Other.
            Instr::PushSelf
            | Instr::PushNil
            | Instr::PushTrue
            | Instr::PushFalse
            | Instr::PushSmi(_)
            | Instr::PushLiteral(_)
            | Instr::PushInstvar(_)
            | Instr::PushGlobal(_) => st.stack.push(AV::Other),
            Instr::PushCtxTemp { .. } => st.stack.push(AV::Other),
            // Stores that CONSUME the popped value: if it is a Site, it escapes
            // (stored into an instvar/global/ctx-temp — reachable elsewhere).
            Instr::StoreInstvarPop(_)
            | Instr::StoreGlobalPop(_)
            | Instr::StoreCtxTempPop { .. } => {
                if let Some(AV::Site(s)) = st.stack.pop() {
                    escaping.insert(s);
                }
            }
            Instr::Send { ic, super_ } => {
                let argc = InterpreterIc::at(method, ic).argc() as usize;
                // Pop args (top-first) then the receiver.
                let mut arg_avs: Vec<AV> = Vec::with_capacity(argc);
                for _ in 0..argc {
                    arg_avs.push(st.stack.pop().unwrap_or(AV::Other));
                }
                let recv = st.stack.pop().unwrap_or(AV::Other);

                // ANY Site among the ARG slots escapes (an arg is never a good
                // use — the callee could stash it anywhere).
                for &a in &arg_avs {
                    if let AV::Site(s) = a {
                        escaping.insert(s);
                    }
                }

                let sel_bytes = selector_bytes(method, ic);
                let vf_argc = value_family_argc(&sel_bytes);
                match recv {
                    AV::Site(s) if !super_ => {
                        // A GOOD use iff: value-family selector, argc matches the
                        // block's own argc, and (already handled above) no arg is
                        // a Site. Otherwise the receiver Site ESCAPES.
                        let block_argc = block_argc_of_site(method, s);
                        let good =
                            matches!(vf_argc, Some(a) if a == argc) && block_argc == Some(argc);
                        if good {
                            value_sends.insert(bci, s);
                        } else {
                            escaping.insert(s);
                        }
                    }
                    AV::Site(s) => {
                        // A super send with a Site receiver — cannot be a value
                        // send (super dispatch), escapes.
                        escaping.insert(s);
                    }
                    AV::Other => {}
                }
                // The result of the send is opaque.
                st.stack.push(AV::Other);
            }
            Instr::ReturnTos => {
                // Returning a Site escapes it.
                if let Some(AV::Site(s)) = st.stack.pop() {
                    escaping.insert(s);
                }
            }
            Instr::BlockReturnTos | Instr::NlrTos => {
                // Only appear in a block body M itself never contains at top
                // level (the driver rejects them in M's own scan); if one is
                // reached here, a Site it returns escapes.
                if let Some(AV::Site(s)) = st.stack.pop() {
                    escaping.insert(s);
                }
            }
            Instr::ReturnSelf => {}
            // Branch/jump terminators are handled by the caller (they end the
            // block); a `br_*` pops its condition there via `pop_escape`.
            Instr::JumpFwd(_) | Instr::JumpBack(_) | Instr::BrTrueFwd(_) | Instr::BrFalseFwd(_) => {
            }
        }
        bci = next;
    }
}

/// The `argc()` of the CompiledBlock at push_closure site `s` in `method`, or
/// `None` if `s` is not a push_closure (shouldn't happen — `s` came from the
/// `sites` set). Reads the literal freshly (no allocation).
fn block_argc_of_site(method: MethodOop, s: usize) -> Option<usize> {
    let (instr, _) = decode_at(method, s);
    if let Instr::PushClosure { lit, .. } = instr {
        let block = MethodOop::try_from(method.literals().at(lit as usize))?;
        Some(block.argc())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::builder::BytecodeBuilder;
    use crate::runtime::vm_state::{VmOptions, VmState};

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

    /// (a) `^[42] value` → all_elidable: a literal block, immediately invoked.
    #[test]
    fn direct_value_is_elidable() {
        let mut vm = test_vm();
        let value_sel = vm.universe.intern(b"value");
        let mut b = BytecodeBuilder::new();
        let lit = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, _vm| {
            blk.push_smi_i8(42);
            blk.ret_tos();
        });
        b.push_closure(lit, 0);
        b.send(&mut vm, value_sel, 0);
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let m = b.finish(&mut vm, sel, 0, 0);

        let e = analyze(m);
        assert!(
            e.all_elidable,
            "a directly-invoked literal block is elidable"
        );
        assert_eq!(e.sites.len(), 1);
        assert!(e.escaping.is_empty());
        // The one value-send bci maps to the one site.
        assert_eq!(e.value_sends.len(), 1);
    }

    /// (b) a block stored into an instvar → escaping.
    #[test]
    fn stored_to_instvar_escapes() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        let lit = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, _vm| {
            blk.push_smi_i8(1);
            blk.ret_tos();
        });
        b.push_closure(lit, 0);
        b.store_instvar_pop(0);
        b.ret_self();
        let sel = vm.universe.intern(b"m");
        let m = b.finish(&mut vm, sel, 0, 0);

        let e = analyze(m);
        assert!(!e.all_elidable, "a stored closure escapes");
        assert_eq!(e.escaping.len(), 1);
    }

    /// (c) a block passed as an ARG to a non-value send → escaping.
    #[test]
    fn passed_as_arg_escapes() {
        let mut vm = test_vm();
        let foo_sel = vm.universe.intern(b"foo:");
        let mut b = BytecodeBuilder::new();
        b.push_self(); // receiver of foo:
        let lit = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, _vm| {
            blk.push_smi_i8(1);
            blk.ret_tos();
        });
        b.push_closure(lit, 0); // the block, as the ARG
        b.send(&mut vm, foo_sel, 1);
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let m = b.finish(&mut vm, sel, 0, 0);

        let e = analyze(m);
        assert!(!e.all_elidable, "a closure passed as an arg escapes");
        assert_eq!(e.escaping.len(), 1);
    }

    /// (d) a two-site merge (each branch pushes a DIFFERENT block into the same
    /// stack slot, then value'd after the join) → both escaping. The merge
    /// loses precise track of which site the slot holds.
    #[test]
    fn two_site_merge_escapes_both() {
        let mut vm = test_vm();
        let value_sel = vm.universe.intern(b"value");
        // `flag ifTrue: [ b := [1] ] ifFalse: [ b := [2] ]. ^b value` shape,
        // hand-built: push a boolean, br_false to the else, each side stores its
        // own block into temp 0, join, then `^(temp0) value`.
        let mut b = BytecodeBuilder::new();
        b.push_true();
        let else_l = b.new_label();
        let end_l = b.new_label();
        b.br_false_fwd(else_l);
        let lit1 = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, _vm| {
            blk.push_smi_i8(1);
            blk.ret_tos();
        });
        b.push_closure(lit1, 0);
        b.store_temp_pop(0);
        b.jump_fwd(end_l);
        b.bind(else_l);
        let lit2 = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, _vm| {
            blk.push_smi_i8(2);
            blk.ret_tos();
        });
        b.push_closure(lit2, 0);
        b.store_temp_pop(0);
        b.bind(end_l);
        b.push_temp(0);
        b.send(&mut vm, value_sel, 0);
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let m = b.finish(&mut vm, sel, 0, 1);

        let e = analyze(m);
        assert!(
            !e.all_elidable,
            "two distinct sites merging into one temp slot both escape"
        );
        assert_eq!(e.sites.len(), 2);
        assert_eq!(e.escaping.len(), 2, "both sites escape at the merge");
    }

    /// (e) `b := [..]. ^b value` (site lives in a temp, then value'd) →
    /// all_elidable: a Site propagates through a temp store/load.
    #[test]
    fn site_via_temp_is_elidable() {
        let mut vm = test_vm();
        let value_sel = vm.universe.intern(b"value");
        let mut b = BytecodeBuilder::new();
        let lit = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, _vm| {
            blk.push_smi_i8(7);
            blk.ret_tos();
        });
        b.push_closure(lit, 0);
        b.store_temp_pop(0); // b := [7]
        b.push_temp(0); // b
        b.send(&mut vm, value_sel, 0); // b value
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let m = b.finish(&mut vm, sel, 0, 1);

        let e = analyze(m);
        assert!(
            e.all_elidable,
            "a site stored to and reloaded from a temp, then value'd, is elidable"
        );
        assert_eq!(e.escaping.len(), 0);
        assert_eq!(e.value_sends.len(), 1);
    }

    /// (f) a block whose OWN body pushes a closure → escaping (unspliceable —
    /// nested closures defer to a later slice).
    #[test]
    fn block_that_pushes_a_closure_escapes() {
        let mut vm = test_vm();
        let value_sel = vm.universe.intern(b"value");
        // Build the inner block standalone, then intern it into the OUTER
        // block's builder from within its body (the re-entrancy escape hatch).
        let inner = crate::bytecode::builder::build_standalone_block(
            &mut vm,
            0,
            0,
            false,
            0,
            false,
            |ib, _vm| {
                ib.push_smi_i8(1);
                ib.ret_tos();
            },
        );
        let mut b = BytecodeBuilder::new();
        let outer_lit = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, vm| {
            let inner_lit = blk.intern_block_literal(vm, inner);
            blk.push_closure(inner_lit, 0);
            blk.pop();
            blk.push_smi_i8(9);
            blk.ret_tos();
        });
        b.push_closure(outer_lit, 0);
        b.send(&mut vm, value_sel, 0);
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let m = b.finish(&mut vm, sel, 0, 0);

        let e = analyze(m);
        assert!(
            !e.all_elidable,
            "a block whose body itself pushes a closure is not spliceable → escaping"
        );
        assert_eq!(e.escaping.len(), 1);
    }

    /// 7-II: a block whose body contains an ordinary (non-super) `Send` (here a
    /// smi `+`) IS spliceable → elidable when directly invoked. Its `+` becomes a
    /// `CallSend`/cold-trap inside the inlined block; a deopt there rebuilds the
    /// block's own frame. (7-I had gated send-ful blocks out.)
    #[test]
    fn block_with_a_send_is_elidable() {
        let mut vm = test_vm();
        let value_sel = vm.universe.intern(b"value");
        let plus = vm.universe.intern(b"+");
        let mut b = BytecodeBuilder::new();
        // `[ 3 + 4 ] value` — the block body has a `+` send.
        let lit = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, vm| {
            blk.push_smi_i8(3);
            blk.push_smi_i8(4);
            blk.send(vm, plus, 1);
            blk.ret_tos();
        });
        b.push_closure(lit, 0);
        b.send(&mut vm, value_sel, 0);
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let m = b.finish(&mut vm, sel, 0, 0);

        let e = analyze(m);
        assert!(
            e.all_elidable,
            "a directly-invoked block with an ordinary send is spliceable (7-II)"
        );
        assert!(e.escaping.is_empty());
    }

    /// A block containing a SUPER send is still NOT spliceable (needs
    /// static-super scope machinery) → its site escapes.
    #[test]
    fn block_with_a_super_send_escapes() {
        let mut vm = test_vm();
        let value_sel = vm.universe.intern(b"value");
        let foo = vm.universe.intern(b"foo");
        let mut b = BytecodeBuilder::new();
        let lit = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, vm| {
            blk.push_self();
            blk.send_super(vm, foo, 0);
            blk.ret_tos();
        });
        b.push_closure(lit, 0);
        b.send(&mut vm, value_sel, 0);
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let m = b.finish(&mut vm, sel, 0, 0);

        let e = analyze(m);
        assert!(
            !e.all_elidable,
            "a block with a super send is not spliceable → escaping"
        );
        assert_eq!(e.escaping.len(), 1);
    }

    /// A value-send whose argc does NOT match the block's argc is NOT a good use
    /// (the receiver Site escapes). `[:x| x] value` (block argc 1, sent argc 0).
    #[test]
    fn argc_mismatch_escapes() {
        let mut vm = test_vm();
        let value_sel = vm.universe.intern(b"value"); // argc 0
        let mut b = BytecodeBuilder::new();
        let lit = b.build_block(&mut vm, 1, 1, false, 0, false, |blk, _vm| {
            blk.push_temp(0);
            blk.ret_tos();
        });
        b.push_closure(lit, 0);
        b.send(&mut vm, value_sel, 0); // `value` (argc 0) on a 1-arg block
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let m = b.finish(&mut vm, sel, 0, 0);

        let e = analyze(m);
        assert!(
            !e.all_elidable,
            "a value-send whose argc mismatches the block's argc escapes the site"
        );
        assert_eq!(e.escaping.len(), 1);
    }

    /// A capturing block (`captures_ctx`) is not spliceable in 7-I → escaping,
    /// even if directly invoked.
    /// 7-II-b: a SEND-FREE `captures_ctx` block (it captures the home's Context
    /// to read/write ctx-temps) IS spliceable — its ctx-temps promote to the
    /// home's vregs. Elidable when directly invoked.
    #[test]
    fn capturing_send_free_block_is_elidable() {
        let mut vm = test_vm();
        let value_sel = vm.universe.intern(b"value");
        let mut b = BytecodeBuilder::new();
        // captures_ctx = true (6th arg), send-free body.
        let lit = b.build_block(&mut vm, 0, 0, false, 0, true, |blk, _vm| {
            blk.push_smi_i8(1);
            blk.ret_tos();
        });
        b.push_closure(lit, 0);
        b.send(&mut vm, value_sel, 0);
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let m = b.finish(&mut vm, sel, 0, 0);

        let e = analyze(m);
        assert!(
            e.all_elidable,
            "a send-free captures_ctx block is spliceable in 7-II-b"
        );
    }

    /// 7-II-b-i: a captures_ctx block WITH a send is still gated (an in-block
    /// deopt needs the block frame's Context to alias the home's — 7-II-b-ii).
    #[test]
    fn capturing_block_with_send_escapes() {
        let mut vm = test_vm();
        let value_sel = vm.universe.intern(b"value");
        let bar = vm.universe.intern(b"bar");
        let mut b = BytecodeBuilder::new();
        let lit = b.build_block(&mut vm, 0, 0, false, 0, true, |blk, vm| {
            blk.push_self();
            blk.send(vm, bar, 0);
            blk.ret_tos();
        });
        b.push_closure(lit, 0);
        b.send(&mut vm, value_sel, 0);
        b.ret_tos();
        let sel = vm.universe.intern(b"m");
        let m = b.finish(&mut vm, sel, 0, 0);

        let e = analyze(m);
        assert!(
            !e.all_elidable,
            "a send-ful capturing block is gated in 7-II-b-i → escaping"
        );
    }
}
