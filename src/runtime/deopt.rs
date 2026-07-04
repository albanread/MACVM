//! S13 step 6 — frame materialization (`sprint_s13_detail.md` D5 M0–M7).
//!
//! Turns one trapped/returning COMPILED physical frame back into the N
//! interpreter frame(s) it stands for (N = 1 in S13; ≥1 once S14 inlining
//! records `SenderLink` chains) and pushes them onto the `ProcessStack`,
//! byte-identical to frames the interpreter's own `push_frame` builds — so
//! the resumed activation cannot tell it was ever compiled. This module
//! BUILDS the frames; it does NOT run them. The nested interpreter resume
//! (D5 M8, `interpret_active`) and the trampoline handoff (the
//! `rt_uncommon_trap` seam in `codecache::deopt_trap`) are S13 step 7.
//!
//! **Safe module.** `#![deny(unsafe_code)]`: every raw native-stack /
//! code-cache read goes through a licensed helper in
//! `codecache::deopt_trap` (`read_frame_slot` for a `FrameSlot`, `read_pool_
//! oop` for a `ConstPool`). No `unsafe` appears below by construction; if a
//! new raw read is ever needed, it is added there (documented), never here.
//!
//! **Layer boundary.** This is the ONLY `runtime` module that reads a
//! compiled frame's native slots (via those two doors) AND builds interpreter
//! frames — the deopt seam sits exactly between the two representations, so
//! it necessarily touches both. It allocates (Contexts, M6) and therefore may
//! GC; every value read out of the compiled frame is either pushed onto the
//! `ProcessStack` (a GC root) before the next allocation or held in a
//! `HandleScope` across it — no bare oop is carried live across an allocating
//! call (the `M6`/`M0` ordering below makes this concrete).

#![deny(unsafe_code)]

use crate::codecache::deopt_trap::{read_frame_slot, read_pool_oop};
use crate::codecache::nmethod::{Nmethod, NmethodId};
use crate::compiler::scopes::{CtxLoc, DecodedScope, DeoptState, ValueLoc};
use crate::interpreter::stack::Frame;
use crate::oops::smi::SmallInt;
use crate::oops::wrappers::MethodOop;
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;

/// The compiled physical frame to deoptimize (`sprint_s13_detail.md` D5).
/// Holds no oops — just the coordinates the materializer reads slots against
/// — so it cannot go stale across the allocations M6 performs.
#[derive(Copy, Clone, Debug)]
pub struct FrameView {
    /// The trapped / returning compiled frame's FP (a raw native-stack
    /// address, the base `FrameSlot` offsets are measured from).
    pub fp: usize,
    /// Trap pc (uncommon-trap path) or `orig_ret_pc` (return-path deopt) —
    /// the native pc whose `PcDesc` names this frame's deopt state.
    pub pc: usize,
    /// The nmethod owning `pc` (its oop pool is where `ConstPool` /
    /// `method_pool_ix` values live, GC-current).
    pub nm: NmethodId,
    /// `Some(result)` on a `reexecute == false` (call-return) site: the
    /// completed call's value, pushed onto the innermost materialized frame's
    /// operand stack (D5 M3.3). `None` on a `reexecute == true` site (the
    /// recorded stack already holds the re-executing op's inputs).
    pub incoming_result: Option<Oop>,
}

/// One decoded virtual frame plus the two per-scope facts M3 needs from the
/// site/chain (its resume bci and pending operand stack). Built in M0,
/// consumed outermost → innermost in M3.
struct VirtualFrame {
    scope: DecodedScope,
    /// `saved_bci` this frame resumes its CALLER at (outer frames:
    /// `sender_bci + len(send)`; the innermost frame's own resume bci is set
    /// separately in M5 and is NOT this field).
    caller_resume_bci: usize,
    /// The frame's operand stack at the deopt point. Outer frames: their
    /// child's `SenderLink.pending_stack`. Innermost: the site's own
    /// recorded `stack` (plus `incoming_result`, handled in M3.3).
    pending_stack: Vec<ValueLoc>,
    /// True for exactly the innermost frame (the one the site names).
    is_innermost: bool,
}

/// Read one `ValueLoc` out of the compiled frame `fv` against `nm` (D5 M3.1).
/// The four arms are the whole location vocabulary:
/// - `FrameSlot(off)` → the live native-stack word at `fp + off` (S12 spill
///   convention),
/// - `ConstPool(i)` → the live nmethod oop-pool word `i` (GC keeps it
///   current, so this reads it NOW, not a snapshot),
/// - `ConstSmi(v)` → tag `v` as a smi,
/// - `Nil` → the well-known `nil` oop.
///
/// S13's own recorder (`resolve_frame_loc`) only ever emits `FrameSlot`/
/// `Nil`; `ConstPool`/`ConstSmi` come from the IR constant layer (wired in a
/// later step) and from S14, but all four are handled here for correctness
/// and exercised by the hand-built test blob.
fn read_value(vm: &VmState, nm: &Nmethod, fv: &FrameView, loc: ValueLoc) -> Oop {
    match loc {
        ValueLoc::FrameSlot(off) => read_frame_slot(fv.fp, off),
        ValueLoc::ConstPool(ix) => read_pool_oop(nm, ix),
        ValueLoc::ConstSmi(v) => SmallInt::new(v).oop(),
        ValueLoc::Nil => vm.universe.nil_obj,
    }
}

/// Length in bytes of the bytecode at `bci` — `decode_at`'s `next - bci`
/// (D5 M5's `bytecode_len_at`). Reuses the single decode stage so the
/// materializer and the interpreter agree on instruction widths by
/// construction.
fn bytecode_len_at(method: MethodOop, bci: usize) -> usize {
    let (_instr, next) = crate::bytecode::opcode::decode_at(method, bci);
    next - bci
}

/// The compiler's bytecode abstract-interpreter model of the operand-stack
/// height at `resume_bci` (D5 M4). A straight-line accumulation of
/// `instr_stack_delta` from bci 0 to `resume_bci` — correct for the common
/// linear-region resume point; at a control-flow merge the linear scan can
/// disagree with the CFG's fixpoint height, so this is used ONLY inside the
/// debug-build `debug_assert_eq!` cross-check, never to size the real frame
/// (the recorded `stack` is the source of truth). Returns `None` when the
/// walk can't reach `resume_bci` exactly (a mid-instruction bci — itself a
/// bug the caller's assert will surface), so the check is skipped rather than
/// panicking on the model's own limitation.
#[cfg(debug_assertions)]
fn interpreter_model_height(method: MethodOop, resume_bci: usize) -> Option<i32> {
    let mut bci = 0usize;
    let mut height = 0i32;
    while bci < resume_bci {
        let (instr, next) = crate::bytecode::opcode::decode_at(method, bci);
        height += crate::compiler::ir::instr_stack_delta(method, &instr);
        bci = next;
    }
    if bci == resume_bci {
        Some(height)
    } else {
        None
    }
}

/// Materialize interpreter frame(s) for `frame` onto `vm.stack` (D5 M0–M7).
/// Does NOT run them — the caller (step 7's `rt_uncommon_trap`) drives the
/// nested interpreter resume afterward. Allocates (Contexts, M6); holds no
/// bare oop across an allocating call.
pub fn deoptimize_frame(vm: &mut VmState, frame: FrameView) {
    // ── M0: resolve + decode the scope chain, innermost → outermost, then
    //    reverse so the physical push order is outermost-first (a caller
    //    frame must sit BELOW its callee on the stack). ──────────────────
    //
    // The `DeoptState` borrows the nmethod's scopes blob; decode everything
    // it needs into owned `VirtualFrame`s up front, so the `&Nmethod` borrow
    // is dropped before the mutable `vm.stack` pushes (and the allocations
    // M6 performs) begin.
    let mut virtual_frames: Vec<VirtualFrame> = Vec::new();
    let (site_bci, site_reexecute, site_stack) = {
        let nm = vm
            .code_table
            .get(frame.nm)
            .expect("deoptimize_frame: FrameView.nm is not a live nmethod");
        let deopt = DeoptState::at(nm, (frame.pc - nm.code.base as usize) as u32);

        // Walk innermost → outermost. Each scope's OWN pending stack and
        // caller-resume bci come from ITS `sender` link (the send in its
        // caller): pending_stack is the sender's frozen operand stack below
        // the inlined send's receiver+args; caller_resume_bci is
        // `sender_bci + len(send)`. The innermost scope has no incoming
        // sender-of-itself here — its pending stack is the site's recorded
        // `stack` (handled in M3), and its own resume bci is M5's — so it
        // carries empty placeholders that M3 overrides.
        let chain: Vec<DecodedScope> = deopt.scopes().collect();
        for (i, scope) in chain.into_iter().enumerate() {
            let is_innermost = i == 0;
            // A scope's `sender` describes the send in its CALLER that
            // inlined it — i.e. it tells the CALLER (the next scope in the
            // chain) where to resume and what its pending stack is. So the
            // pending stack / caller-resume bci for scope `k` come from scope
            // `k-1`'s (its child's) sender link. S13 is depth-1: only the
            // innermost scope exists, `sender` is None, and both fields are
            // supplied by the site (M3) — the chain-derived values only
            // matter once S14 emits real `SenderLink`s.
            let (caller_resume_bci, pending_stack) = match &scope.sender {
                Some((_sender_off, sender_bci, pending)) => {
                    let sender_method = pool_method(vm, frame.nm, scope.method_pool_ix);
                    let resume =
                        *sender_bci as usize + bytecode_len_at(sender_method, *sender_bci as usize);
                    (resume, pending.clone())
                }
                None => (0, Vec::new()),
            };
            virtual_frames.push(VirtualFrame {
                scope,
                caller_resume_bci,
                pending_stack,
                is_innermost,
            });
        }
        (
            deopt.site.bci as usize,
            deopt.site.reexecute,
            deopt.site.stack.clone(),
        )
    };
    // Outermost first for the physical pushes.
    virtual_frames.reverse();

    // ── M1: bump the deopt counter; trace under MACVM_TRACE=deopt. ───────
    vm.stats.deopt_count += 1;
    if vm.options.trace.is_enabled("deopt") {
        eprintln!(
            "[deopt] nm={:?} pc={:#x} fp={:#x} bci={} reexecute={} frames={} result={}",
            frame.nm,
            frame.pc,
            frame.fp,
            site_bci,
            site_reexecute,
            virtual_frames.len(),
            frame.incoming_result.is_some(),
        );
    }

    // ── M2: record the ProcessStack watermark. The nested run (step 7)
    //    ends when the stack pops back below this. ────────────────────────
    let _base_sp = vm.stack.sp;

    // ── M3: push each virtual frame, outermost → innermost. ──────────────
    // `saved_fp` links each frame to the previous materialized frame's FP;
    // the outermost links to the entry sentinel (there is no materialized
    // caller — the compiled frame's own native caller resumes via the
    // trampoline epilogue once the nested run returns, step 7).
    let mut prev_fp: i64 = crate::oops::layout::ENTRY_FRAME_SENTINEL;
    // Track the innermost frame's fp so M5/M6/M7 can address it after all
    // pushes (a later `push_frame` cannot move an earlier frame — the stack
    // only grows — so this stays valid).
    let mut innermost_fp: usize = 0;
    for vf in virtual_frames.iter() {
        let method = pool_method(vm, frame.nm, vf.scope.method_pool_ix);
        let argc = method.argc();
        let ntemps = method.ntemps();

        // The `saved_bci` this frame resumes its CALLER at. For the innermost
        // frame this is M5's resume bci; for an outer frame it is the
        // post-send resume point derived in M0. (Both are "where THIS frame's
        // pc points" — the header's saved_bci is the frame's own resume
        // point, read by the interpreter when control returns INTO it.)
        let saved_bci = if vf.is_innermost {
            // M5: innermost resume bci — `bci` if re-executing the op,
            // else past the completed send.
            if site_reexecute {
                site_bci
            } else {
                site_bci + bytecode_len_at(method, site_bci)
            }
        } else {
            vf.caller_resume_bci
        };

        // Read receiver + unified slot values BEFORE any push, so a stray
        // realloc of the stack Vec during the pushes can't invalidate a
        // FrameSlot read (they're plain values now).
        let nm_ref = nmethod_ref(vm, frame.nm);
        let receiver = read_value(vm, nm_ref, &frame, vf.scope.receiver);
        let slot_vals: Vec<Oop> = vf
            .scope
            .slots
            .iter()
            .map(|&loc| read_value(vm, nm_ref, &frame, loc))
            .collect();
        debug_assert_eq!(
            slot_vals.len(),
            argc + ntemps,
            "materialize: scope has {} unified slots but method wants argc {argc} + ntemps \
             {ntemps}",
            slot_vals.len()
        );

        // Outer scopes push their pending operand stack FIRST (it belongs to
        // the caller frame, sitting below the callee it inlined). The
        // innermost frame's operand stack is pushed AFTER its header (below).
        if !vf.is_innermost {
            for &loc in &vf.pending_stack {
                let v = read_value(vm, nmethod_ref(vm, frame.nm), &frame, loc);
                vm.stack.push(v);
            }
        }

        // Receiver + args, exactly where the interpreter's caller leaves
        // them (unified slots 0..argc are the callee's negative-offset arg
        // area). `push_frame` reads its receiver copy from `stack[fp-argc-1]`,
        // so push the receiver, then the args, then let `push_frame` lay the
        // header — reusing the interpreter's own frame builder verbatim (the
        // surest way to be byte-identical for M7).
        vm.stack.push(receiver);
        for &v in &slot_vals[..argc] {
            vm.stack.push(v);
        }
        let fp_new = vm.stack.sp;
        crate::interpreter::push_frame(vm, method, argc, prev_fp, saved_bci);

        // Overwrite the nil temps `push_frame` laid down with the scope's
        // real temp values (unified slots argc..). `Frame::set_temp` uses the
        // same unified indexing as the interpreter.
        let f = Frame { fp: fp_new };
        for (t, &v) in slot_vals[argc..].iter().enumerate() {
            f.set_temp(&mut vm.stack, argc + t, v);
        }

        // ── M6: context. ────────────────────────────────────────────────
        materialize_context(vm, frame.nm, &frame, &vf.scope.ctx, fp_new);

        if vf.is_innermost {
            innermost_fp = fp_new;
            // ── M3.3 innermost operand stack: the site's recorded values,
            //    plus `incoming_result` iff the site is a call-return. ─────
            for &loc in &site_stack {
                let v = read_value(vm, nmethod_ref(vm, frame.nm), &frame, loc);
                vm.stack.push(v);
            }
            if site_reexecute {
                debug_assert!(
                    frame.incoming_result.is_none(),
                    "deoptimize_frame: reexecute site must carry no incoming_result \
                     (the recorded stack already holds the op's inputs)"
                );
            } else {
                let result = frame.incoming_result.expect(
                    "deoptimize_frame: a call-return (reexecute == false) site must carry \
                     the completed call's incoming_result",
                );
                vm.stack.push(result);
            }

            // ── M4: operand-stack height cross-check (debug builds). ──────
            #[cfg(debug_assertions)]
            {
                let resume_bci = saved_bci;
                if let Some(model) = interpreter_model_height(method, resume_bci) {
                    let materialized =
                        (vm.stack.sp - (fp_new + frame_header_and_temps(method))) as i32;
                    debug_assert_eq!(
                        materialized, model,
                        "deopt M4: materialized operand-stack height {materialized} != \
                         interpreter model height {model} at resume bci {resume_bci} \
                         (the classic deopt bug — reexecute={site_reexecute})"
                    );
                }
            }
        }

        prev_fp = fp_new as i64;
    }

    // ── M7: every materialized frame must be indistinguishable from an
    //    interpreter-built one. Walk the fp chain from innermost back down
    //    (saved_fp links to the previous materialized frame) and verify. ──
    let mut fp = innermost_fp;
    loop {
        let f = Frame { fp };
        f.verify(&vm.stack);
        let saved = f.saved_fp(&vm.stack);
        if saved == crate::oops::layout::ENTRY_FRAME_SENTINEL {
            break;
        }
        fp = saved as usize;
    }
}

/// Fixed header (`FRAME_TEMPS_BASE`) + `ntemps` — the count of slots between
/// a frame's FP and the base of its operand stack. Used by M4 to isolate the
/// operand-stack height from the header+temps below it.
#[cfg(debug_assertions)]
fn frame_header_and_temps(method: MethodOop) -> usize {
    crate::oops::layout::FRAME_TEMPS_BASE + method.ntemps()
}

/// Borrow `frame.nm`'s `&Nmethod` (a live-nmethod lookup that panics on a
/// dead id — a materialize against a flushed nmethod is a VM bug). Factored
/// out because the borrow must be re-taken between mutable `vm.stack` pushes
/// (an `&Nmethod` and `&mut vm.stack` cannot coexist).
fn nmethod_ref(vm: &VmState, id: NmethodId) -> &Nmethod {
    vm.code_table
        .get(id)
        .expect("deoptimize_frame: nmethod id is not live")
}

/// Resolve a scope's `method_pool_ix` to its `MethodOop` (D5 M3: the OLD
/// compile-time method — a mid-activation redefinition completes under old
/// code). Reads the LIVE oop-pool word, so a moving GC that relocated the
/// method oop is transparently accounted for (`oops_do` keeps the pool word
/// current).
///
/// `method_pool_ix` is a REAL interned index: `ir::convert` interns a
/// deopt-having method's own compiled `MethodOop` into its pool (as an `Oop`
/// reloc) and `driver::build_deopt_metadata` records that index in every
/// scope. Because the metadata holds the compile-time oop directly, a
/// mid-activation redefinition (a later step's `NotEntrant`) still deopts to
/// the OLD method, exactly as D5 requires — the reason the method lives in
/// the GC-visited pool rather than being re-derived from `key_klass`/
/// `key_selector` (whose current dict lookup would see the NEW method).
fn pool_method(vm: &VmState, id: NmethodId, method_pool_ix: u32) -> MethodOop {
    let nm = nmethod_ref(vm, id);
    let oop = read_pool_oop(nm, method_pool_ix);
    MethodOop::try_from(oop).expect(
        "deoptimize_frame: scope method_pool_ix does not resolve to a CompiledMethod \
         (ir::convert interns a deopt-having method's own oop; a mismatch is a compiler bug)",
    )
}

/// D5 M6: materialize the frame's Context slot.
/// - `Materialized(loc)` → store the read Context oop (compiled code already
///   allocated it at entry; deopt reuses it as-is).
/// - `Elided { temps }` → read every temp value into a `HandleScope` FIRST
///   (the allocation below may GC, and the compiled frame is still walkable
///   per D4, but reading them first and rooting them keeps the fill trivially
///   correct), then allocate a fresh Context, fill it, store it. S13 never
///   emits this (S14 Context-elision does); wired now because it's cheap.
/// - `None` → leave `nil` (`push_frame` already stored nil at FRAME_CONTEXT).
fn materialize_context(
    vm: &mut VmState,
    id: NmethodId,
    frame: &FrameView,
    ctx: &CtxLoc,
    fp: usize,
) {
    match ctx {
        CtxLoc::None => {
            // push_frame already left nil at FRAME_CONTEXT.
        }
        CtxLoc::Materialized(loc) => {
            let c = read_value(vm, nmethod_ref(vm, id), frame, *loc);
            Frame { fp }.set_context(&mut vm.stack, c);
        }
        CtxLoc::Elided { temps } => {
            // Read every temp value first (plain oops, no live raw reads
            // across the allocation), rooted in a HandleScope so the
            // allocation's GC updates them.
            let scope = crate::memory::handles::HandleScope::enter(vm);
            let handles: Vec<_> = temps
                .iter()
                .map(|&loc| {
                    let v = read_value(vm, nmethod_ref(vm, id), frame, loc);
                    scope.handle(vm, v)
                })
                .collect();
            let context = crate::memory::alloc::alloc_context(vm, handles.len());
            for (i, h) in handles.iter().enumerate() {
                context.set_slot(i, h.get(vm));
            }
            Frame { fp }.set_context(&mut vm.stack, context.oop());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::BytecodeBuilder;
    use crate::codecache::nmethod::Nmethod;
    use crate::compiler::scopes::{
        CtxLoc, PcDesc, SafepointKind, SafepointState, ScopeDescData, ScopeDescRecorder, ValueLoc,
    };
    use crate::interpreter::stack::Frame;
    use crate::oops::layout::{
        ENTRY_FRAME_SENTINEL, FRAME_CONTEXT, FRAME_METHOD, FRAME_SAVED_BCI, FRAME_SAVED_FP,
        FRAME_TEMPS_BASE,
    };
    use crate::oops::smi::SmallInt;
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

    /// A method with argc args + ntemps temps whose body is `return self`
    /// (bci 0). We deopt at a re-execute site keyed at bci 0, so the resume
    /// bci is 0 and the model operand-stack height there is 0.
    fn trivial_method(vm: &mut VmState, argc: usize, ntemps: usize) -> MethodOop {
        let mut b = BytecodeBuilder::new();
        b.ret_self();
        let sel = vm.universe.intern(b"deoptee");
        b.finish(vm, sel, argc, ntemps)
    }

    /// Hand-build the physical compiled frame as a stack-local `[u64]` whose
    /// address stands in for the trapped frame's FP, plus a hand-built
    /// nmethod carrying a recorder-produced scope blob whose `FrameSlot`
    /// offsets index into that array, and a `ConstPool` word interned into
    /// the nmethod's own pool. Then materialize and assert the resulting
    /// interpreter frame matches an interpreter-built one (Frame::verify +
    /// field-by-field).
    ///
    /// This is the whole step-6 contract in one test: a compiled frame in →
    /// an interpreter frame out, indistinguishable from a native push.
    #[test]
    fn materialize_reexecute_frame_matches_interpreter() {
        let mut vm = test_vm();

        // The deoptee: argc=1, ntemps=2. Unified slots: [arg0, temp0, temp1].
        // Its bytecode pushes THREE values then returns, so the compiler's
        // abstract-interpreter model height at bci 3 (our re-execute resume
        // point) is exactly 3 — matching the 3-entry recorded operand stack
        // below. (An arbitrary bci-0 site with a non-empty stack is physically
        // impossible and M4 rightly rejects it — a real re-execute site's
        // recorded height always equals the model height there.)
        let method = {
            let mut b = BytecodeBuilder::new();
            b.push_nil(); // bci 0 -> h1
            b.push_nil(); // bci 1 -> h2
            b.push_nil(); // bci 2 -> h3
            b.ret_tos(); // bci 3 (never reached by deopt; we resume AT bci 3)
            let sel = vm.universe.intern(b"deoptee3");
            b.finish(&mut vm, sel, 1, 2)
        };

        // Build the nmethod. Its oop pool must carry (word 0) the method oop
        // — a REAL `method_pool_ix` (see pool_method's S13-gap note; the test
        // sets a real index rather than the production 0 placeholder) — and
        // (word 1) a constant oop we reference via ConstPool(1). We hand-pack
        // a code blob whose literal area holds those two words.
        let receiver_val = SmallInt::new(0x11).oop();
        let arg0_val = SmallInt::new(0x22).oop();
        let temp0_val = SmallInt::new(0x33).oop();
        // A ConstPool-sourced constant (word 1 of the pool). Use a distinct
        // smi so we can tell it apart.
        let pool_const_val = SmallInt::new(0x44).oop();

        // A ConstSmi and Nil in the operand stack, to exercise those arms.
        let const_smi_val: i64 = 0x55;

        let nm = build_nmethod_with_pool(&mut vm, method, pool_const_val);

        // The physical frame: a stack-local array the FrameSlots address.
        // Layout (byte offsets from `fp`, all negative = below-FP spill
        // slots, S12 convention): we place four oops and point `fp` past
        // them so offsets -8/-16/-24 land on real words.
        //   slots[0] = receiver   (FrameSlot -24)
        //   slots[1] = arg0       (FrameSlot -16)
        //   slots[2] = temp0      (FrameSlot -8)
        //   slots[3] = (fp anchor; not read)
        let phys: [u64; 4] = [
            receiver_val.raw(),
            arg0_val.raw(),
            temp0_val.raw(),
            0, // fp points here
        ];
        let fp = (&phys[3]) as *const u64 as usize;

        // Recorder: one depth-1 scope. Unified slots argc..: temp0 from a
        // FrameSlot, temp1 from Nil. Receiver from a FrameSlot. Context None.
        let mut rec = ScopeDescRecorder::new();
        let scope = rec.begin_scope(ScopeDescData {
            method_pool_ix: 0, // real: word 0 of the pool is the method oop
            is_block: false,
            sender: None,
            receiver: ValueLoc::FrameSlot(-24),
            slots: vec![
                ValueLoc::FrameSlot(-16), // arg0
                ValueLoc::FrameSlot(-8),  // temp0
                ValueLoc::Nil,            // temp1 = nil
            ],
            ctx: CtxLoc::None,
        });
        // A re-execute uncommon-trap site at bci 0. Operand stack: a
        // ConstPool oop, a ConstSmi, and a Nil — three values, height 3.
        rec.record_site(
            0x10,
            SafepointState {
                scope,
                bci: 3, // resume AT bci 3; model height there is 3
                kind: SafepointKind::UncommonTrap,
                reexecute: true,
                stack: vec![
                    ValueLoc::ConstPool(1),
                    ValueLoc::ConstSmi(const_smi_val),
                    ValueLoc::Nil,
                ],
            },
        );
        let (blob, pcdescs) = rec.pack();
        let nm_id = install_nmethod(&mut vm, nm, blob, pcdescs);

        // The pc that resolves the site: code_base + 0x10.
        let nm_ref = vm.code_table.get(nm_id).unwrap();
        let pc = nm_ref.code.base as usize + 0x10;

        let sp_before = vm.stack.sp;
        let deopt_before = vm.stats.deopt_count;

        deoptimize_frame(
            &mut vm,
            FrameView {
                fp,
                pc,
                nm: nm_id,
                incoming_result: None, // reexecute site
            },
        );

        // M1: counter bumped exactly once.
        assert_eq!(vm.stats.deopt_count, deopt_before + 1);

        // One frame was pushed. Its fp is right after the pushed
        // receiver+args (receiver + 1 arg = 2 slots) from sp_before.
        let fp_new = sp_before + 2;
        let f = Frame { fp: fp_new };

        // M7 already ran inside deoptimize_frame; re-run for the test's own
        // assurance and then check fields.
        f.verify(&vm.stack);

        // Header.
        assert_eq!(
            f.method(&vm.stack).oop(),
            method.oop(),
            "FRAME_METHOD is the deoptee method"
        );
        assert_eq!(
            f.saved_fp(&vm.stack),
            ENTRY_FRAME_SENTINEL,
            "outermost frame links to the entry sentinel"
        );
        assert_eq!(
            f.saved_bci(&vm.stack),
            3,
            "reexecute site resumes AT bci 3 (the recorded bci, not past it)"
        );
        assert_eq!(
            f.context(&vm.stack),
            vm.universe.nil_obj,
            "CtxLoc::None leaves nil context"
        );

        // Receiver (fast copy + arg-area copy agree).
        assert_eq!(f.receiver(&vm.stack), receiver_val);
        // Unified slots: arg0 (index 0), temp0 (index 1), temp1=nil (index 2).
        assert_eq!(f.temp(&vm.stack, 0), arg0_val, "arg0");
        assert_eq!(f.temp(&vm.stack, 1), temp0_val, "temp0");
        assert_eq!(f.temp(&vm.stack, 2), vm.universe.nil_obj, "temp1 = nil");

        // Operand stack: ConstPool(1) -> pool_const_val, ConstSmi -> tagged
        // smi, Nil -> nil. sp points just past them.
        let opnd_base = fp_new + FRAME_TEMPS_BASE + method.ntemps();
        assert_eq!(vm.stack.sp, opnd_base + 3, "three operand values pushed");
        assert_eq!(
            vm.stack.get(opnd_base),
            pool_const_val,
            "ConstPool(1) resolved to the live pool word"
        );
        assert_eq!(
            vm.stack.get(opnd_base + 1),
            SmallInt::new(const_smi_val).oop(),
            "ConstSmi tagged as a smi"
        );
        assert_eq!(vm.stack.get(opnd_base + 2), vm.universe.nil_obj, "Nil");

        // Cross-check the header against a genuine interpreter-built frame for
        // the SAME method + receiver + args: `push_frame`'s own output must be
        // slot-for-slot identical (except the serial, which is monotonic and
        // deliberately never equal — that's what makes it a dead-home key).
        // Built on `vm` itself AFTER the materialized frame (the stack only
        // grows; the earlier frame stays put), since the method lives in
        // `vm`'s heap.
        let ref_fp = vm.stack.sp + 2; // after we push receiver + 1 arg
        vm.stack.push(receiver_val);
        vm.stack.push(arg0_val);
        // Same linkage as the materialized frame (entry sentinel + resume
        // bci 3) so every header slot is expected to match slot-for-slot.
        crate::interpreter::push_frame(&mut vm, method, 1, ENTRY_FRAME_SENTINEL, 3);
        let rf = Frame { fp: ref_fp };
        assert_eq!(
            vm.stack.get(ref_fp + FRAME_METHOD),
            vm.stack.get(fp_new + FRAME_METHOD),
            "method slot identical to an interpreter push"
        );
        assert_eq!(
            vm.stack.get(ref_fp + FRAME_SAVED_FP),
            vm.stack.get(fp_new + FRAME_SAVED_FP),
            "saved_fp slot identical"
        );
        assert_eq!(
            vm.stack.get(ref_fp + FRAME_SAVED_BCI),
            vm.stack.get(fp_new + FRAME_SAVED_BCI),
            "saved_bci slot identical"
        );
        assert_eq!(
            vm.stack.get(ref_fp + FRAME_CONTEXT),
            vm.stack.get(fp_new + FRAME_CONTEXT),
            "context slot identical (both nil)"
        );
        assert_eq!(
            rf.receiver(&vm.stack),
            f.receiver(&vm.stack),
            "receiver copy"
        );
    }

    /// A call-return site (reexecute == false): the recorded stack EXCLUDES
    /// the send's popped args and the materializer pushes `incoming_result`.
    /// Resume bci is PAST the send. Exercises M3.3's non-reexecute arm and
    /// the M5 `bci + len(send)` computation.
    #[test]
    fn materialize_call_return_frame_pushes_result() {
        let mut vm = test_vm();

        // A method: `push_self; send #foo; return_tos` — so there's a real
        // send at a known bci whose length M5 advances past. argc=0.
        let mut b = BytecodeBuilder::new();
        b.push_self(); // bci 0 (1 byte)
        let foo = vm.universe.intern(b"foo");
        b.send(&mut vm, foo, 0); // bci 1 (send: 2 bytes) -> resume at bci 3
        b.ret_tos(); // bci 3
        let sel = vm.universe.intern(b"caller");
        let method = b.finish(&mut vm, sel, 0, 0);

        let receiver_val = SmallInt::new(0x77).oop();
        let result_val = SmallInt::new(0x88).oop();

        let nil = vm.universe.nil_obj;
        let nm = build_nmethod_with_pool(&mut vm, method, nil);

        // Physical frame: just the receiver at FrameSlot -8.
        let phys: [u64; 2] = [receiver_val.raw(), 0];
        let fp = (&phys[1]) as *const u64 as usize;

        let mut rec = ScopeDescRecorder::new();
        let scope = rec.begin_scope(ScopeDescData {
            method_pool_ix: 0,
            is_block: false,
            sender: None,
            receiver: ValueLoc::FrameSlot(-8),
            slots: vec![], // argc 0, ntemps 0
            ctx: CtxLoc::None,
        });
        // Call site keyed on its RETURN address; reexecute = false; recorded
        // stack is empty (the send's receiver was popped, result comes from
        // incoming_result). bci names the SEND (bci 1).
        rec.record_site(
            0x20,
            SafepointState {
                scope,
                bci: 1,
                kind: SafepointKind::Call,
                reexecute: false,
                stack: vec![],
            },
        );
        let (blob, pcdescs) = rec.pack();
        let nm_id = install_nmethod(&mut vm, nm, blob, pcdescs);
        let pc = vm.code_table.get(nm_id).unwrap().code.base as usize + 0x20;

        let sp_before = vm.stack.sp;
        deoptimize_frame(
            &mut vm,
            FrameView {
                fp,
                pc,
                nm: nm_id,
                incoming_result: Some(result_val),
            },
        );

        let fp_new = sp_before + 1; // receiver only (argc 0)
        let f = Frame { fp: fp_new };
        f.verify(&vm.stack);
        assert_eq!(
            f.saved_bci(&vm.stack),
            3,
            "call-return resumes PAST the 2-byte send at bci 1 -> bci 3"
        );
        // Operand stack: just the incoming result.
        let opnd_base = fp_new + FRAME_TEMPS_BASE; // ntemps 0
        assert_eq!(vm.stack.sp, opnd_base + 1, "one value: the call result");
        assert_eq!(
            vm.stack.get(opnd_base),
            result_val,
            "incoming_result pushed on the operand stack"
        );
    }

    /// MACVM_TRACE=deopt exercises the trace arm without changing behavior.
    #[test]
    fn deopt_count_increments() {
        let mut vm = test_vm();
        let method = trivial_method(&mut vm, 0, 0);
        let nil = vm.universe.nil_obj;
        let nm = build_nmethod_with_pool(&mut vm, method, nil);
        let phys: [u64; 1] = [0];
        let fp = (&phys[0]) as *const u64 as usize;

        let mut rec = ScopeDescRecorder::new();
        let scope = rec.begin_scope(ScopeDescData {
            method_pool_ix: 0,
            is_block: false,
            sender: None,
            receiver: ValueLoc::Nil,
            slots: vec![],
            ctx: CtxLoc::None,
        });
        rec.record_site(
            0x10,
            SafepointState {
                scope,
                bci: 0,
                kind: SafepointKind::UncommonTrap,
                reexecute: true,
                stack: vec![],
            },
        );
        let (blob, pcdescs) = rec.pack();
        let nm_id = install_nmethod(&mut vm, nm, blob, pcdescs);
        let pc = vm.code_table.get(nm_id).unwrap().code.base as usize + 0x10;

        assert_eq!(vm.stats.deopt_count, 0);
        deoptimize_frame(
            &mut vm,
            FrameView {
                fp,
                pc,
                nm: nm_id,
                incoming_result: None,
            },
        );
        assert_eq!(vm.stats.deopt_count, 1);
    }

    // ── Test scaffolding: publish a real code blob whose bytes ARE the oop
    //    pool ([method_oop, extra_const] at literal_off = 0), so
    //    `read_pool_oop` reads live MAP_JIT words. ──────────────────────────

    /// A published nmethod handle whose oop pool (at `literal_off = 0`) holds
    /// word 0 = `method` oop, word 1 = `extra` oop. Returns the code handle;
    /// the caller builds the `Nmethod` around it (so the same handle's
    /// `base`/`len` feed both the nmethod's `code` field and the pc the deopt
    /// resolves against).
    fn publish_pool(
        vm: &mut VmState,
        method: MethodOop,
        extra: Oop,
    ) -> crate::codecache::CodeHandle {
        use crate::compiler::assembler::CodeBlob;
        let mut bytes = Vec::with_capacity(16);
        bytes.extend_from_slice(&method.oop().raw().to_le_bytes());
        bytes.extend_from_slice(&extra.raw().to_le_bytes());
        let blob = CodeBlob {
            code: bytes,
            literal_off: 0,
            relocs: Vec::new(),
            listing: Vec::new(),
        };
        let handle = vm
            .code_cache
            .alloc(blob.code.len())
            .expect("test code cache room");
        vm.code_cache.publish(handle, &blob);
        handle
    }

    /// Build (not yet install) a full `Nmethod` around a real published pool
    /// handle. Everything except `code`/`deopt_*` is inert placeholder shape
    /// (this stub is never executed — deopt only reads its pool + scopes). The
    /// key klass/selector are real interned symbols (valid oops, distinct per
    /// `code.base`) so `CodeTable::by_key` never collides across installs and
    /// this test module needs no `unsafe` (the module denies it).
    fn build_nmethod_with_pool(vm: &mut VmState, method: MethodOop, extra: Oop) -> Nmethod {
        use crate::codecache::nmethod::{NmState, NmethodId};
        let code = publish_pool(vm, method, extra);
        // A real klass + a distinct real selector per published base, so
        // `CodeTable::by_key` never collides across this module's installs
        // (only `.oop().raw()` is read from either).
        let sel = format!("s{:x}", code.base as usize);
        let sel_sym = vm.universe.intern(sel.as_bytes());
        Nmethod {
            id: NmethodId(0),
            key_klass: vm.universe.object_klass,
            key_selector: sel_sym,
            code,
            entry_off: 0,
            verified_entry_off: 0,
            state: NmState::Alive,
            level: 1,
            version: 0,
            literal_off: 0,
            relocs: Vec::new(),
            frame_slots: 0,
            slot_is_oop: Vec::new(),
            pcdescs: Vec::new(),
            oopmaps: Vec::new(),
            ic_sites: Vec::new(),
            poll_bci: None,
            deopt_scopes: Vec::new(),
            deopt_pcdescs: Vec::new(),
        }
    }

    fn install_nmethod(
        vm: &mut VmState,
        mut nm: Nmethod,
        blob: Vec<u8>,
        pcdescs: Vec<PcDesc>,
    ) -> NmethodId {
        nm.deopt_scopes = blob;
        nm.deopt_pcdescs = pcdescs;
        vm.code_table.install(nm)
    }
}
