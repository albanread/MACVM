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
use crate::interpreter::ic::{ic_state, IcState, InterpreterIc};
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
    /// S14 step 7-IV-c: block-arg send bci → `(site id, arg index)` — a mono
    /// send passing the site's block as its ONLY block argument to a callee
    /// proven BLOCK-ARG TRANSPARENT ([`block_arg_transparent`]): the callee
    /// will be CFG-inlined and the block spliced at its `value` sites inside.
    blockarg_sends: HashMap<usize, (usize, usize)>,
}

impl ClosureEscape {
    /// S24 A3: is the `push_closure` at `bci` an ESCAPING site (flows out /
    /// unspliceable / branch-consumed / cross-block-live)? `ir::convert`
    /// consults this to decide whether to SPLICE the block (elidable) or
    /// ALLOCATE a real closure (`Ir::AllocClosure`, escaping). A site is
    /// escaping iff it is in `self.escaping`.
    pub fn is_escaping_site(&self, bci: usize) -> bool {
        self.escaping.contains(&bci)
    }

    /// S24 A3a (design §2.7): can EVERY escaping site in M be compiled as an
    /// ALLOCATED closure? True iff every escaping site's block is transitively
    /// NLR-free (the dead-home soundness gate) AND does NOT capture the home's
    /// Context (the has_ctx path is A3b). Combined with the driver's own
    /// `!method.has_ctx()` guard, this is the A3a eligibility relaxation:
    /// `all_elidable` (splice everything, S14) OR every escaping closure is an
    /// allocatable non-ctx NLR-free block. An empty `escaping` set (the
    /// all-elidable case) returns true vacuously.
    pub fn all_escaping_a3a_compilable(&self, method: MethodOop) -> bool {
        self.escaping.iter().all(|&bci| {
            let (instr, _) = decode_at(method, bci);
            let Instr::PushClosure { lit, .. } = instr else {
                return false;
            };
            let Some(block) = MethodOop::try_from(method.literals().at(lit as usize)) else {
                return false;
            };
            !block.captures_ctx() && block_transitively_nlr_free(block)
        })
    }

    /// S24 A3b (design §2.7): can EVERY escaping site be compiled as an
    /// allocated closure, allowing `captures_ctx` blocks? True iff every
    /// escaping site's block is transitively NLR-free — the ONLY hard gate
    /// (the dead-home soundness rule). A3a's `all_escaping_a3a_compilable`
    /// additionally required `!captures_ctx`; A3b lifts that because a
    /// `has_ctx` creator now MATERIALIZES its Context (`ir::convert`'s
    /// `method_ctx_vreg`) so an escaping capturing closure gets a real
    /// `copied[1]`. The driver's own `has_ctx`-materialize logic covers the
    /// Context; this only gates NLR-freeness.
    pub fn all_escaping_nlr_free(&self, method: MethodOop) -> bool {
        self.escaping.iter().all(|&bci| {
            let (instr, _) = decode_at(method, bci);
            let Instr::PushClosure { lit, .. } = instr else {
                return false;
            };
            let Some(block) = MethodOop::try_from(method.literals().at(lit as usize)) else {
                return false;
            };
            block_transitively_nlr_free(block)
        })
    }

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

    /// S14 step 7-IV-c: the `(site id, arg index)` a BLOCK-ARG send at `bci`
    /// threads into its inlined callee — `Some` only for a proven good use.
    pub fn blockarg_send_target(&self, bci: usize) -> Option<(usize, usize)> {
        self.blockarg_sends
            .get(&bci)
            .copied()
            .filter(|(s, _)| !self.escaping.contains(s))
    }
}

/// S24 A3 (design §2.7, review §2 MAJOR — this scan is LOAD-BEARING, not
/// vacuous): is `block`, AND every block transitively reachable through its
/// own `push_closure` literals, free of `nlr_tos`? A creator M may stamp the
/// dead-home sentinel (`home_dead_sentinel`) into a closure ONLY when this
/// holds. The reason is the inner-block case: an interpreted inner block
/// created while B_s runs gets its home copied VERBATIM from B_s's own
/// closure (`home_ref_for_new_closure`, blocks.rs) — so a dead-home sentinel
/// on B_s would poison a nested legal `[:y | ^y]`'s home, turning a valid NLR
/// (M's compiled frame still live) into a wrong `#cannotReturn:`. Statically
/// decidable and cheap: scan B_s's bytecode for `NlrTos`; recurse through
/// each `PushClosure` literal. `visited` guards a (pathological) cyclic
/// literal graph — a re-visit returns "free so far" because any `NlrTos`
/// under it was already found on the first descent.
pub fn block_transitively_nlr_free(block: MethodOop) -> bool {
    fn scan(m: MethodOop, visited: &mut HashSet<u64>) -> bool {
        if !visited.insert(m.oop().raw()) {
            return true;
        }
        let len = m.bytecode_len();
        let mut bci = 0;
        while bci < len {
            let (instr, next) = decode_at(m, bci);
            match instr {
                Instr::NlrTos => return false,
                Instr::PushClosure { lit, .. } => {
                    if let Some(inner) = MethodOop::try_from(m.literals().at(lit as usize)) {
                        if inner.is_block() && !scan(inner, visited) {
                            return false;
                        }
                    }
                }
                _ => {}
            }
            bci = next;
        }
        true
    }
    let mut visited = HashSet::new();
    scan(block, &mut visited)
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
    // 7-II-b-ii: a `captures_ctx` block MAY have ordinary sends. 7-III: a block
    // MAY contain `nlr_tos` (`^expr`) — inlined into its home M, the NLR is just
    // a return from M (`Ir::Ret`); the `ensure:` decline (SPEC A7 Step 1) is
    // AUTOMATIC because any M with `ensure:`/`ifCurtailed:` fails the escape gate
    // (its handler block escapes as a non-value arg). Still gated: super sends,
    // nested closures, depth>0 (nested-context) ctx access.
    let mut has_nlr = false;
    let mut has_send = false;
    let len = block.bytecode_len();
    let mut bci = 0;
    while bci < len {
        let (instr, next) = decode_at(block, bci);
        match instr {
            Instr::Send { super_: true, .. } | Instr::PushClosure { .. } => return false,
            Instr::Send { .. } => has_send = true,
            Instr::NlrTos => has_nlr = true,
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
    // 7-III: an NLR block must be SEND-FREE for now. A send-ful NLR block that
    // deopted at an inner send would rebuild an is_block frame and then run the
    // interpreter's `nlr_tos`, which reads a real closure's `home_ref` from the
    // block frame's receiver slot — but we elided the closure. A send-free NLR
    // block never deopts inside, so the interpreter never runs its `nlr_tos`.
    // (Synthesizing a home-ref closure for send-ful NLR blocks is a later slice.)
    if has_nlr && has_send {
        return false;
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

/// S14 step 7-IV-c: is callee `d` BLOCK-ARG TRANSPARENT in its arg `arg_ix`
/// for a block of `blk_argc` args? I.e., bound to a phantom (elided) closure,
/// does `d` use that arg ONLY as the receiver of matching-argc value-family
/// sends? Then `d` can be CFG-inlined with the phantom threaded through and the
/// block spliced at each `value` site. The check is a per-block linear scan
/// (no fixpoint): the phantom's only durable home is the IMMUTABLE arg temp —
/// it may ride the operand stack transiently within a block (even below other
/// sends' operands: those sites record `ValueLoc::ElidedClosure`), but must
/// not survive to a block boundary on the stack, be stored anywhere, be
/// dup'd, be passed as an argument, or be returned.
fn block_arg_transparent(d: MethodOop, arg_ix: usize, blk_argc: usize) -> bool {
    let cfg = decode::decode(d);
    let (entry_depth, _) = crate::compiler::ir::compute_entry_depths(d, &cfg);
    for (b, block) in cfg.blocks.iter().enumerate() {
        // Entry stacks never carry the phantom (boundary rule below).
        let mut stack: Vec<bool> = vec![false; entry_depth[b].max(0) as usize];
        let mut bci = block.bci_start;
        while bci < block.bci_end {
            let (instr, next) = decode_at(d, bci);
            match instr {
                Instr::PushTemp(t) => stack.push(t as usize == arg_ix),
                Instr::StoreTempPop(t) => {
                    let v = stack.pop().unwrap_or(false);
                    // The phantom may not be stored anywhere, and its home
                    // temp may not be overwritten.
                    if v || t as usize == arg_ix {
                        return false;
                    }
                }
                Instr::StoreTemp(t) => {
                    let v = *stack.last().unwrap_or(&false);
                    if v || t as usize == arg_ix {
                        return false;
                    }
                }
                Instr::Dup => {
                    if *stack.last().unwrap_or(&false) {
                        return false;
                    }
                    stack.push(false);
                }
                Instr::Pop => {
                    // Discarding a pushed phantom copy is dead — fine.
                    stack.pop();
                }
                Instr::StoreInstvarPop(_) | Instr::StoreGlobalPop(_) => {
                    if stack.pop().unwrap_or(false) {
                        return false;
                    }
                }
                Instr::Send { ic, super_ } => {
                    let argc = InterpreterIc::at(d, ic).argc() as usize;
                    let mut arg_phantom = false;
                    for _ in 0..argc {
                        if stack.pop().unwrap_or(false) {
                            arg_phantom = true;
                        }
                    }
                    let recv_phantom = stack.pop().unwrap_or(false);
                    if arg_phantom || (super_ && recv_phantom) {
                        return false;
                    }
                    if recv_phantom {
                        let sel = selector_bytes(d, ic);
                        let good = value_family_argc(&sel) == Some(argc)
                            && argc == blk_argc
                            // Exactly ONE phantom at the value site: none may
                            // remain below (it would land in the spliced
                            // block's pending stack, which cannot carry one).
                            && !stack.iter().any(|&ph| ph);
                        if !good {
                            return false;
                        }
                    }
                    stack.push(false);
                }
                Instr::ReturnTos => {
                    if stack.pop().unwrap_or(false) {
                        return false;
                    }
                }
                Instr::ReturnSelf
                | Instr::JumpFwd(_)
                | Instr::JumpBack(_)
                | Instr::BrTrueFwd(_)
                | Instr::BrFalseFwd(_) => {}
                // Every other opcode either pushes one opaque value or is
                // rejected up front by `is_inline_eligible_cfg(d)`
                // (super/ctx/closure/NLR never reach here).
                _ => stack.push(false),
            }
            bci = next;
        }
        // Boundary rule: the phantom never leaves a block on the stack.
        if stack.iter().any(|&ph| ph) {
            return false;
        }
    }
    true
}

/// S14 step 7-IV-c: full candidate check for a BLOCK-ARG send: the site's IC
/// is mono to a non-primitive, CFG-inlinable callee within the DOUBLED
/// block-arg budget (SPEC §8.4's block-argument bonus — inlining the callee is
/// what unlocks inlining its block argument), the block itself fits the plain
/// budget, and the callee is transparent in that arg.
fn blockarg_candidate_ok(method: MethodOop, ic: u16, arg_ix: usize, site_bci: usize) -> bool {
    let (instr, _) = decode_at(method, site_bci);
    let Instr::PushClosure { lit, .. } = instr else {
        return false;
    };
    let Some(blk) = MethodOop::try_from(method.literals().at(lit as usize)) else {
        return false;
    };
    if !block_is_spliceable(blk) {
        return false;
    }
    if !matches!(ic_state(method, ic), IcState::Mono) {
        return false;
    }
    let site = InterpreterIc::at(method, ic);
    let Some(d) = MethodOop::try_from(site.target()) else {
        // A mono site whose target is an nmethod id — resolvable in principle,
        // conservative for now.
        return false;
    };
    if d.primitive() != 0
        || d.argc() != site.argc() as usize
        || !crate::compiler::inline::is_inline_eligible_cfg(d)
    {
        return false;
    }
    let budget = crate::compiler::inline::budget_for_level(1);
    if crate::compiler::inline::inline_cost(d) > budget.per_call_cost * 2
        || crate::compiler::inline::inline_cost(blk) > budget.per_call_cost
    {
        return false;
    }
    block_arg_transparent(d, arg_ix, blk.argc())
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
    let mut blockarg_sends: HashMap<usize, (usize, usize)> = HashMap::new();
    // site bci → does its block body contain any `Send`? (Its splice then has
    // in-body safepoints — see the stale-alias rule in `transfer_block`.)
    let mut site_has_sends: HashMap<usize, bool> = HashMap::new();

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
                site_has_sends.insert(bci, !crate::compiler::inline::is_leaf(block));
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
            blockarg_sends,
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
            &site_has_sends,
            &mut escaping,
            &mut value_sends,
            &mut blockarg_sends,
        );
        // SOUNDNESS (S14 7-IV-b): a Site still LIVE at a block boundary WITH
        // SUCCESSORS escapes. A phantom closure has no compiled location — any
        // safepoint in a successor (a send, a loop poll on a backward edge)
        // would record its slot as dead/Nil, and a deopt there would hand the
        // interpreter nil where it expects the closure. Killing cross-block
        // liveness outright is conservative (a cross-block good use is
        // rejected rather than miscompiled); ElidedClosure materialization can
        // lift this later. A `Return`-terminated block has no successor and no
        // further safepoint — a stale alias surviving to it is harmless
        // (`b := [7]. ^b value`).
        let cfg_block = &cfg.blocks[b];
        if !matches!(cfg_block.terminator, Terminator::Return) {
            for &slot in st.stack.iter().chain(st.temps.iter()) {
                if let AV::Site(s) = slot {
                    escaping.insert(s);
                }
            }
        }
        // Propagate the block's exit state to its successors.
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
        blockarg_sends,
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
    site_has_sends: &HashMap<usize, bool>,
    escaping: &mut HashSet<usize>,
    value_sends: &mut HashMap<usize, usize>,
    blockarg_sends: &mut HashMap<usize, (usize, usize)>,
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

                // S14 step 7-IV-c: a Site among the ARG slots is a GOOD use
                // iff this is a BLOCK-ARG send: exactly one Site arg, a plain
                // (non-super) send with a non-Site receiver, whose mono callee
                // is CFG-inlinable within the doubled budget and BLOCK-ARG
                // TRANSPARENT in that arg — the callee inlines and the block
                // splices at its `value` sites. Anything else escapes the arg
                // Site(s) (the callee could stash them anywhere).
                // NOTE `arg_avs` is popped top-first: `arg_avs[k]` is source
                // arg `argc - 1 - k` (= the callee's unified temp index).
                let phantom_args: Vec<(usize, usize)> = arg_avs
                    .iter()
                    .enumerate()
                    .filter_map(|(k, &a)| match a {
                        AV::Site(s) => Some((argc - 1 - k, s)),
                        AV::Other => None,
                    })
                    .collect();
                if !phantom_args.is_empty() {
                    let good_blockarg = phantom_args.len() == 1
                        && matches!(recv, AV::Other)
                        && !super_
                        && blockarg_candidate_ok(method, ic, phantom_args[0].0, phantom_args[0].1);
                    if good_blockarg {
                        let (arg_ix, s) = phantom_args[0];
                        blockarg_sends.insert(bci, (s, arg_ix));
                    } else {
                        for &(_, s) in &phantom_args {
                            escaping.insert(s);
                        }
                    }
                }

                let sel_bytes = selector_bytes(method, ic);
                let vf_argc = value_family_argc(&sel_bytes);
                // Does THIS send introduce a SAFEPOINT while other phantoms are
                // live? A real (non-value) send always does (CallSend / cold
                // trap). A GOOD value-send introduces one only if the spliced
                // block body itself has sends (its in-body CallSends/traps).
                let mut send_is_safepoint = true;
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
                            // The splice replaces the dispatch: a SEND-FREE
                            // block body has no safepoint at all.
                            send_is_safepoint = site_has_sends.get(&s).copied().unwrap_or(true);
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
                // SOUNDNESS (S14 7-IV-b): any Site still LIVE (stack or temp)
                // across a SAFEPOINT escapes. A phantom closure has no compiled
                // location — a deopt at this send would materialize its
                // temp/stack slot as dead/Nil, and the interpreter's later
                // `value` of it would be a nil send. This hits (a) sites whose
                // liveness spans an unrelated real send — `b := [..]. self
                // poke. ^b value` — and (b) stale temp aliases of a consumed
                // site whose SPLICED body itself contains sends (the in-body
                // safepoints would record the alias slot as Nil). A send-free
                // block's splice has no safepoint, so a stale alias of it is
                // harmless (`b := [7]. ^b value` stays elidable).
                if send_is_safepoint {
                    for &slot in st.stack.iter().chain(st.temps.iter()) {
                        if let AV::Site(s) = slot {
                            escaping.insert(s);
                        }
                    }
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

    // --- S24 A3: transitive NLR-free scan + escaping-site classification ---

    #[test]
    fn transitive_nlr_free_flat_block() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        // A block with no NlrTos anywhere.
        let free = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, _vm| {
            blk.push_smi_i8(7);
            blk.block_return_tos();
        });
        // A block that DOES an NLR.
        let nlr = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, _vm| {
            blk.push_smi_i8(7);
            blk.nlr_tos();
        });
        b.push_closure(free, 0);
        b.push_closure(nlr, 0);
        b.ret_self();
        let sel = vm.universe.intern(b"m");
        let m = b.finish(&mut vm, sel, 0, 0);
        let free_blk = MethodOop::try_from(m.literals().at(free)).unwrap();
        let nlr_blk = MethodOop::try_from(m.literals().at(nlr)).unwrap();
        assert!(block_transitively_nlr_free(free_blk), "no NlrTos -> free");
        assert!(
            !block_transitively_nlr_free(nlr_blk),
            "own NlrTos -> not free"
        );
    }

    #[test]
    fn transitive_nlr_free_recurses_into_inner_block() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        // Outer block has no NlrTos of its own, but creates an inner block that
        // DOES — the transitive scan must reject the outer (an interpreted
        // outer still creates the NLR-bearing inner, whose home the dead-home
        // sentinel would poison).
        let outer = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, vm2| {
            let inner = blk.build_block(vm2, 0, 0, false, 0, false, |i, _vm| {
                i.push_smi_i8(1);
                i.nlr_tos();
            });
            blk.push_closure(inner, 0);
            blk.block_return_tos();
        });
        b.push_closure(outer, 0);
        b.ret_self();
        let sel = vm.universe.intern(b"m");
        let m = b.finish(&mut vm, sel, 0, 0);
        let outer_blk = MethodOop::try_from(m.literals().at(outer)).unwrap();
        assert!(
            !block_transitively_nlr_free(outer_blk),
            "an inner block's NlrTos is transitively visible -> not free"
        );
    }

    #[test]
    fn a3a_classifies_escaping_nlr_free_non_ctx_block() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        // A non-capturing, NLR-free block stored into an instvar → escapes,
        // and is A3a-compilable (allocatable).
        let lit = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, _vm| {
            blk.push_smi_i8(1);
            blk.block_return_tos();
        });
        let site = b.here();
        b.push_closure(lit, 0);
        b.store_instvar_pop(0);
        b.ret_self();
        let sel = vm.universe.intern(b"m");
        let m = b.finish(&mut vm, sel, 0, 0);
        let e = analyze(m);
        assert!(!e.all_elidable, "stored closure escapes");
        assert!(
            e.is_escaping_site(site),
            "the push_closure site is escaping"
        );
        assert!(
            e.all_escaping_a3a_compilable(m),
            "an escaping NLR-free non-ctx block is A3a-compilable"
        );
    }

    #[test]
    fn a3a_rejects_escaping_nlr_bearing_block() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        // An NLR-bearing block stored into an instvar → escapes AND is NOT
        // A3a-compilable (the dead-home sentinel would misdeliver its `^`).
        let lit = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, _vm| {
            blk.push_smi_i8(1);
            blk.nlr_tos();
        });
        b.push_closure(lit, 0);
        b.store_instvar_pop(0);
        b.ret_self();
        let sel = vm.universe.intern(b"m");
        let m = b.finish(&mut vm, sel, 0, 0);
        let e = analyze(m);
        assert!(!e.all_elidable, "stored closure escapes");
        assert!(
            !e.all_escaping_a3a_compilable(m),
            "an escaping NLR-bearing block is NOT A3a-compilable"
        );
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

    /// 7-II-b-ii: a captures_ctx block WITH an ordinary send IS spliceable — an
    /// in-block deopt rebuilds the block frame with its Context aliasing the
    /// home's, so a post-deopt ctx-temp write reaches M's Context.
    #[test]
    fn capturing_block_with_send_is_elidable() {
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
            e.all_elidable,
            "a send-ful capturing block is spliceable in 7-II-b-ii"
        );
    }
}
