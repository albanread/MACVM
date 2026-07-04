//! Unified stack walker (`sprint_s12_detail.md` D3): [`FrameView`] +
//! [`walk_frames`], classifying every activation on the (single) process
//! while walking outward from the innermost one — interpreted frames on
//! `vm.stack`, and native frames (compiled nmethods, the runtime stubs
//! that call into Rust, the `call_stub` I→C trampoline) that have no
//! Rust-visible representation at all. Primary consumer: `memory::roots`'
//! `each_code_root` (S12 D4.1, GC root enumeration — not yet wired up,
//! S12 step 4/5's job).
//!
//! `runtime::error::print_stack_trace` is deliberately NOT re-pointed onto
//! this, despite D3's own text suggesting it: `stub_poll` never sets the
//! anchor (S10 D5.6 — it never allocates/GCs, so `walk_frames`' actual
//! consumer never needs to see a Poll-reached frame), but a
//! `MACVM_TRACE`-style trace triggered from INSIDE `rt_poll`
//! (`trace_on_poll`, `tests_s10.md`'s `mixed_trace_golden`) very much DOES
//! need to see the compiled frame that called `Poll` — `AdapterKind::Poll`
//! exists for exactly this reason, but making it reachable would mean
//! teaching `stub_poll`'s own hand-rolled prologue/epilogue (deliberately
//! NOT `emit_stub_prologue`, since it saves x0-x15, not just x0-x5) to
//! ALSO tag the anchor — a real, separate change to an already-working,
//! heavily-exercised mechanism, not something this step's own walker work
//! should fold in under its own momentum. `print_stack_trace` keeps its
//! existing, narrower `tier_links.last()`-based mechanism for now; SPEC-
//! QUESTION tracked in `sprint_s12_detail.md`'s own STEP-3 NOTES.
//!
//! # The anchor, and why classifying its own frame needs a fourth field
//!
//! A runtime stub (`codecache::stubs`'s `resolve`/`c2i_shared`/
//! `mega_shared`/`dnu`/`must_be_boolean`/`alloc_slow` — the six that call
//! `emit_stub_prologue`) writes `vm.reg_block.last_compiled_{fp,pc}`
//! before calling into Rust: `last_compiled_fp` = the stub's own x29,
//! `last_compiled_pc` = x30 at that moment. Standard AArch64 frame-pointer
//! convention means x30, at a callee's own prologue, is ALWAYS the
//! address inside its CALLER's code where control resumes — i.e.
//! `last_compiled_pc` describes the stub's CALLER (a real compiled
//! method), never the stub's own code. None of the six is ever reached
//! via a `bl`/`blr` FROM another stub or adapter (per-instance c2i/mega
//! trampolines and PICs are confirmed tail-jump-only, `b`, never touching
//! x30 — S11's own P2 invariant), so there is no pc anywhere that's
//! "inside this stub's own code" to classify it with — `last_compiled_kind`
//! (`layout::VMREG_LAST_COMPILED_KIND_OFFSET`, written by each of the six
//! stubs' own preamble, `codecache::stubs::KIND_*`) exists precisely to
//! answer "which of the six" directly, since no code-range lookup can.
//!
//! # Where a walk starts: the tier journal decides, not the anchor alone
//!
//! During a reentrant C→I call the anchor legitimately stays pointed at
//! the outer `c2i_shared` frame the whole time — even while the nested
//! interpreted dispatch is the true innermost activation. Step 3
//! originally resolved that tension by having `run_method_reentrant`
//! CLEAR the anchor around itself (and this walker start anchor-first);
//! step 7 replaced that with the journal-first start rule documented at
//! `walk_frames`' own head: `vm.tier_links.last()` — the one record that
//! cannot misorder the crossings — picks the innermost side, and the
//! anchor is only consulted when the journal says native is innermost
//! (or as a loud-failure breadcrumb when the journal is empty/torn). The
//! clearing approach turned out to HIDE the whole native chain whenever
//! the nested interpreted target was a frameless successful primitive —
//! see the start-rule comment in `walk_frames` for the full account.

// This module only (not the rest of `runtime`, whose own doc comment
// still says "no unsafe here") — mirrors `codecache::mod`'s own
// `#![allow(unsafe_code)]` boundary, for the same reason: walking raw
// native stack memory has no safe-Rust equivalent.
#![allow(unsafe_code)]

use crate::codecache::nmethod::NmethodId;
use crate::codecache::stubs::{
    KIND_ALLOC_SLOW, KIND_C2I, KIND_DNU, KIND_MEGA, KIND_MUST_BE_BOOLEAN, KIND_RESOLVE,
};
use crate::interpreter::stack::Frame;
use crate::oops::layout::ENTRY_FRAME_SENTINEL;
use crate::runtime::vm_state::{TierLink, VmState};

/// Which of the six anchor-setting runtime stubs a `FrameView::Adapter`
/// belongs to (module doc above) — plus `Poll`, kept only for this enum's
/// own completeness: `stub_poll` never calls `emit_stub_prologue` at all
/// (S10 D5.6 — it never allocates or GCs, so nothing beneath it on the
/// native stack could ever trigger a walk), so `Poll` is provably never
/// actually produced by [`walk_frames`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AdapterKind {
    Resolve,
    C2i,
    Mega,
    Dnu,
    MustBeBoolean,
    AllocSlow,
    Poll,
}

impl AdapterKind {
    /// `codecache::stubs::KIND_*`'s own numbers — ownership of the mapping
    /// lives there (it's the module that actually emits the `movz`/`str`
    /// writing them); this just mirrors it for the read side. Panics on an
    /// unrecognized value: a VM-internal-consistency bug (a stub added
    /// later that forgot to tag itself, or memory corruption), not a
    /// user-triggerable condition — same posture as `oopmap::verify`.
    fn from_raw(raw: u64) -> AdapterKind {
        match raw {
            KIND_RESOLVE => AdapterKind::Resolve,
            KIND_C2I => AdapterKind::C2i,
            KIND_MEGA => AdapterKind::Mega,
            KIND_DNU => AdapterKind::Dnu,
            KIND_MUST_BE_BOOLEAN => AdapterKind::MustBeBoolean,
            KIND_ALLOC_SLOW => AdapterKind::AllocSlow,
            other => panic!(
                "AdapterKind::from_raw: unrecognized last_compiled_kind {other} -- the anchor's \
                 kind tag and this mapping have drifted apart, or the anchor is corrupt"
            ),
        }
    }
}

/// One activation, classified (S12 D3). `Interpreted`/`Compiled` are the
/// two "ordinary" tiers; `Adapter` is one of the six runtime stubs,
/// currently mid-call into Rust; `CallStub` marks the I→C trampoline
/// boundary, where native walking hands back to the process stack.
///
/// `Adapter` can ONLY ever be the first frame of a native-mode segment —
/// reached either from the anchor itself, or from a `TierLink::
/// IntoInterpreter` transition (both come with fp/pc/kind already known:
/// the anchor's own fields, or `AdapterKind::C2i` unconditionally for
/// `IntoInterpreter`, since `rt_interpret_call` is the ONLY place that
/// variant is ever constructed). Stepping outward via a raw `[fp]`/`[fp+8]`
/// read can never land back inside one of the six stubs' own code — none
/// is ever the target of a `bl`/`blr` from another stub or from compiled
/// code in a way that would leave a chaseable return address pointing
/// back into it — so a stepped-to native frame is always either a real
/// nmethod or `call_stub`'s own frame, never `Adapter` again.
#[derive(Clone, Copy, Debug)]
pub enum FrameView {
    Interpreted {
        fp: usize,
    },
    Compiled {
        fp: u64,
        ret_pc: u64,
        nm: NmethodId,
    },
    Adapter {
        fp: u64,
        kind: AdapterKind,
        /// The stub's OWN caller's resume address — `last_compiled_pc`
        /// (or a `TierLink::IntoInterpreter.compiled_ret_pc`), i.e. the
        /// address inside a REAL compiled nmethod where the original send/
        /// alloc/runtime site lives. Root-scanning (S12 step 4/5, not this
        /// module) uses it to look up that site's own real argc — e.g. a
        /// `resolve`/`mega`/`dnu`/`c2i` frame's RootSpill area holds
        /// whatever was in x0..x5 BEFORE the call, which is only
        /// meaningful up to the site's own real arg count; the remaining
        /// slots may be stale, non-oop register content from the
        /// compiled method's own unrelated register allocation.
        caller_pc: u64,
    },
    CallStub {
        fp: u64,
    },
}

enum Mode {
    /// Just entered native mode — from the anchor, or from a
    /// `TierLink::IntoInterpreter` transition. `(fp, pc, kind)` are ALL
    /// already known; the corresponding `FrameView` is always `Adapter`.
    Anchor(u64, u64, AdapterKind),
    /// Stepping via a raw fp-chain read — `pc` must be classified (an
    /// nmethod's own range, or `call_stub`'s fixed range; never one of
    /// the six stubs, per this module's own invariant, documented on
    /// [`FrameView::Adapter`]).
    NativeStep(u64, u64),
    Interp(usize),
    Done,
}

/// A generous but finite bound (`tests_s12.md`'s
/// `walker_terminates_on_torn_tierlinks`): a genuinely well-formed
/// process/native stack is always far shorter than this. Hitting it means
/// `tier_links` or the native fp-chain is corrupt (torn, mismatched, or a
/// cycle) — looping forever trying to walk it would be worse than a clean
/// panic naming the problem.
const MAX_WALK_STEPS: usize = 100_000;

/// # Safety
/// `addr` must be a live native-stack address belonging to a frame this
/// walker's own classification just established (the anchor's own fp, a
/// `TierLink`'s own recorded fp, or the result of a previous, already-
/// validated step) — never called on an arbitrary or externally-derived
/// value. Sound in this single-threaded VM because nothing can pop a
/// frame out from under a walk that runs to completion without yielding.
unsafe fn read_u64(addr: u64) -> u64 {
    unsafe { *(addr as *const u64) }
}

fn call_stub_contains(vm: &VmState, pc: u64) -> bool {
    let h = vm.stubs.call_stub;
    let base = h.base as u64;
    pc >= base && pc < base + h.len as u64
}

/// D3: walk every activation of the (single) process, innermost first,
/// interleaving native compiled/stub frames and process-stack interpreter
/// frames via `vm.tier_links` + the anchor. `f` is called once per
/// activation, innermost first; this function itself never allocates or
/// touches `vm.stack`'s own `sp`/`fp` (a pure read-only walk).
///
/// # Panics
/// If the native fp-chain or `vm.tier_links` turn out to be inconsistent
/// with each other (an unclassifiable native pc, a boundary crossing that
/// doesn't find the tier-link kind it expects, or exceeding
/// [`MAX_WALK_STEPS`]) — always a VM-internal-consistency bug, never a
/// condition guest code can trigger.
pub fn walk_frames(vm: &VmState, mut f: impl FnMut(FrameView)) {
    let mut link_idx = vm.tier_links.len();
    // Start-of-walk rule (S12 step 7, revised twice from step 3 — both
    // predecessors were wrong in ways only the fallen bridge could
    // expose): the innermost side is decided by the most recent TIER
    // CROSSING, i.e. `tier_links.last()` — the one journal that cannot
    // lie about ordering — never by the anchor alone and never by
    // `has_frame` alone.
    //
    // - Last crossing was C→I (`IntoInterpreter`): the interpreter side
    //   is innermost. If it has live frames, walk them (its entry
    //   sentinel consumes that same link on the way back out, as
    //   always). If it has NO frames — a reentrant send whose target's
    //   PRIMITIVE succeeded collects BEFORE any frame is pushed
    //   (`try_primitive` precedes activation) — there is nothing
    //   interpreted to visit (the receiver/args live in plain `vm.stack`
    //   slots `for_each_root` already covers), so consume the link HERE
    //   and enter the native chain it records. Step 3's design instead
    //   had `run_method_reentrant` clear the anchor and keyed the start
    //   on anchor-nonzero — which left this exact frameless case with
    //   NOTHING to start from, silently skipping every compiled frame's
    //   spill slots (found by `mid_loop_forced_scavenge` the moment the
    //   bridge came down; the interim "has_frame first" revision fixed
    //   that but misrouted the opposite case, a live PAUSED interpreted
    //   caller beneath a compiled frame's own alloc-slow collection).
    // - Last crossing was I→C (`IntoCompiled`): the native side is
    //   innermost; the anchor names the stub currently in Rust (it must
    //   be set — GC only ever starts inside an anchor-setting stub's own
    //   runtime call).
    // - No crossings at all: pure single-tier state — a still-set anchor
    //   (possible only if something upstream tore the journal) is still
    //   honored so the walk fails LOUDLY at the call_stub boundary
    //   instead of silently reporting an empty world
    //   (`walker_terminates_on_torn_tierlinks` pins exactly this); else
    //   plain interpreter frames or nothing.
    // "Live interpreted frame" needs BOTH checks: `has_frame`, AND fp not
    // being `run_method`'s own temporary ENTRY_FRAME_SENTINEL spoof — for
    // exactly the duration of an entry's `try_primitive` attempt, fp is
    // deliberately set to the sentinel (see `run_method`'s own doc) while
    // `has_frame` still reflects the OUTER paused activation. A
    // `can_allocate` primitive collecting inside that window (every
    // allocation, under GC_STRESS=1) must be treated exactly like the
    // frameless case: nothing interpreted is live for THIS entry (its
    // receiver/args are plain `vm.stack` slots `for_each_root` covers),
    // and blindly walking `fp == usize::MAX` overflows on the first slot
    // read (found by the flagship gate the first time a reentrant
    // allocating primitive scavenged for real).
    let live_interp =
        vm.stack.has_frame() && vm.stack.fp != crate::oops::layout::ENTRY_FRAME_SENTINEL as usize;
    let mut mode = match vm.tier_links.last() {
        Some(TierLink::IntoInterpreter {
            compiled_fp,
            compiled_ret_pc,
        }) => {
            if live_interp {
                Mode::Interp(vm.stack.fp)
            } else {
                link_idx -= 1;
                Mode::Anchor(*compiled_fp, *compiled_ret_pc, AdapterKind::C2i)
            }
        }
        Some(TierLink::IntoCompiled { .. }) => {
            assert_ne!(
                vm.reg_block.last_compiled_fp, 0,
                "walk_frames: innermost side is native (last tier crossing was IntoCompiled) \
                 but no anchor is set -- GC can only start inside an anchor-setting stub"
            );
            Mode::Anchor(
                vm.reg_block.last_compiled_fp,
                vm.reg_block.last_compiled_pc,
                AdapterKind::from_raw(vm.reg_block.last_compiled_kind),
            )
        }
        None => {
            if vm.reg_block.last_compiled_fp != 0 {
                Mode::Anchor(
                    vm.reg_block.last_compiled_fp,
                    vm.reg_block.last_compiled_pc,
                    AdapterKind::from_raw(vm.reg_block.last_compiled_kind),
                )
            } else if live_interp {
                Mode::Interp(vm.stack.fp)
            } else {
                Mode::Done
            }
        }
    };

    for _ in 0..MAX_WALK_STEPS {
        mode = match mode {
            Mode::Done => return,
            Mode::Anchor(fp, pc, kind) => {
                f(FrameView::Adapter {
                    fp,
                    kind,
                    caller_pc: pc,
                });
                // SAFETY: `fp` is `last_compiled_fp` or a prior
                // `TierLink::IntoInterpreter.compiled_fp`, both written
                // only by a live stub's own `stp x29,x30` prologue at the
                // moment control entered Rust — a live native frame for
                // as long as this walk runs (single-threaded VM).
                let fp_next = unsafe { read_u64(fp) };
                // The next frame's own classifying pc is the CALLER's
                // resume point, which is exactly `pc` here already (==
                // `[fp+8]` by construction — the stub's own prologue wrote
                // the same x30 value to both the anchor and its own
                // saved-lr slot) — no separate memory read needed.
                Mode::NativeStep(fp_next, pc)
            }
            Mode::NativeStep(fp, pc) => {
                if let Some(nm_id) = vm.code_table.find_by_pc(pc) {
                    f(FrameView::Compiled {
                        fp,
                        ret_pc: pc,
                        nm: nm_id,
                    });
                    // SAFETY: `fp` is a live compiled frame's own x29,
                    // established by `emit()`'s own
                    // `stp x29,x30,...; mov x29,sp` prologue, shared by
                    // every published nmethod.
                    let fp_next = unsafe { read_u64(fp) };
                    let pc_next = unsafe { read_u64(fp + 8) };
                    Mode::NativeStep(fp_next, pc_next)
                } else if call_stub_contains(vm, pc) {
                    f(FrameView::CallStub { fp });
                    link_idx = link_idx.checked_sub(1).unwrap_or_else(|| {
                        panic!(
                            "walk_frames: reached call_stub with no matching \
                             TierLink::IntoCompiled left -- tier_links and the native fp-chain \
                             disagree"
                        )
                    });
                    match vm.tier_links[link_idx] {
                        TierLink::IntoCompiled { interp_frame, .. } => Mode::Interp(interp_frame),
                        TierLink::IntoInterpreter { .. } => panic!(
                            "walk_frames: call_stub's own boundary must pair with \
                             TierLink::IntoCompiled, found IntoInterpreter instead"
                        ),
                    }
                } else {
                    panic!(
                        "walk_frames: native pc {pc:#x} is not inside any alive nmethod or \
                         call_stub -- classification invariant broke"
                    );
                }
            }
            Mode::Interp(fp) => {
                f(FrameView::Interpreted { fp });
                let saved_fp = Frame { fp }.saved_fp(&vm.stack);
                if saved_fp == ENTRY_FRAME_SENTINEL {
                    if link_idx == 0 {
                        Mode::Done
                    } else {
                        link_idx -= 1;
                        match vm.tier_links[link_idx] {
                            TierLink::IntoInterpreter {
                                compiled_fp,
                                compiled_ret_pc,
                            } => Mode::Anchor(compiled_fp, compiled_ret_pc, AdapterKind::C2i),
                            TierLink::IntoCompiled { .. } => panic!(
                                "walk_frames: an ENTRY_FRAME_SENTINEL must pair with \
                                 TierLink::IntoInterpreter, found IntoCompiled instead"
                            ),
                        }
                    }
                } else {
                    Mode::Interp(saved_fp as usize)
                }
            }
        };
    }
    panic!(
        "walk_frames: exceeded {MAX_WALK_STEPS} steps -- tier_links or the native fp-chain is \
         corrupt (torn, mismatched, or cyclic), not merely a very deep call stack"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::BytecodeBuilder;
    use crate::compiler::driver;
    use crate::interpreter::compiled_call::{enter_compiled, EnterResult};
    use crate::oops::smi::SmallInt;
    use crate::oops::wrappers::{KlassOop, MethodOop};
    use crate::runtime::lookup::install_method;
    use crate::runtime::vm_state::{VmOptions, VmState};

    fn test_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            gc_stress_full_period: None,
            eden_kb: None,
            jit: crate::runtime::JitMode::Threshold(1),
        })
    }

    /// A bare `test_vm()` has no world loaded — installs a real
    /// primitive-backed method by pinned id (mirrors `it_tier1.rs`'s own
    /// `install_prim` helper).
    fn install_prim(vm: &mut VmState, klass: KlassOop, name: &[u8], argc: usize, prim: i64) {
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.ret_self(); // never reached; the primitive always succeeds here
        let sel = vm.universe.intern(name);
        let m = b.finish(vm, sel, argc, 0);
        m.set_primitive(prim);
        install_method(vm, klass, sel, m);
    }

    fn compile(vm: &mut VmState, rcvr_klass: KlassOop, method: MethodOop) -> NmethodId {
        driver::compile_method(vm, rcvr_klass, method).expect("must compile")
    }

    /// Builds `AllocTarget` (a real class, via the frontend, so
    /// `basicNew`'s own `Format::Slots` klass shape is genuine) and a
    /// compiled `spawn` method (`^AllocTarget basicNew`, S11 D7's inline
    /// `Ir::Alloc`) — driven to its SLOW edge (`rt_alloc_slow`, one of the
    /// six anchor-setting stubs), so `walk_frames` has a real, in-flight
    /// native chain to observe via the `test_walk_capture` hook. Returns
    /// the compiled id and a receiver ready to push.
    fn build_and_compile_spawn(vm: &mut VmState) -> (NmethodId, crate::oops::Oop) {
        let target_sel = vm.universe.intern(b"AllocTarget");
        for item in
            crate::frontend::parser::parse_file("Object subclass: AllocTarget [ ]").expect("parse")
        {
            crate::frontend::classdef::execute_top_item(vm, item).expect("execute");
        }
        let target_assoc =
            crate::runtime::globals::global_lookup(vm, target_sel).expect("AllocTarget global");
        let target_klass = KlassOop::try_from(
            crate::oops::wrappers::MemOop::try_from(target_assoc)
                .unwrap()
                .body_oop(1),
        )
        .unwrap();
        let target_meta = crate::runtime::lookup::klass_of(vm, target_klass.oop());
        install_prim(vm, target_meta, b"basicNew", 0, 23);

        let mut b = BytecodeBuilder::new();
        b.push_global(vm, target_assoc);
        let basic_new_sel = vm.universe.intern(b"basicNew");
        b.send(vm, basic_new_sel, 0);
        b.ret_tos();
        let spawn_sel = vm.universe.intern(b"spawn");
        let method = b.finish(vm, spawn_sel, 0, 0);

        let smi_klass = vm.universe.smi_klass;
        let recv = SmallInt::new(1).oop();
        // Warm the site (mono, smi receiver, basicNew primitive) so it's
        // eligible, matching `allocation_fast_and_slow`'s own recipe.
        crate::interpreter::run_method(vm, method, recv, &[]);
        let nm_id = compile(vm, smi_klass, method);

        // Force the slow edge by filling eden HONESTLY (real, walkable
        // objects). The pre-S12-step-7 `eden.top = eden.end` lie is no
        // longer survivable here: with the D8 bridge gone, `rt_alloc_slow`'s
        // own ordinary allocation genuinely scavenges, and the scavenge's
        // entry verify walks eden up to `top` — straight into the lie's
        // uninitialized gap (the exact failure `it_gc_jit.rs`'s own
        // step-4 test already hit and documented).
        let array_klass = vm.universe.array_klass;
        let chunk_elems = 4096usize;
        let chunk_bytes = (crate::oops::layout::HEADER_WORDS + chunk_elems) * 8;
        while vm.universe.eden.end - vm.universe.eden.top >= chunk_bytes {
            crate::memory::alloc::alloc_indexable_oops(vm, array_klass, chunk_elems);
        }
        // Tail-fill until less than ONE MORE AllocTarget fits, so the
        // compiled inline bump is guaranteed to overflow.
        let need = target_klass.non_indexable_size() * crate::oops::layout::WORD_SIZE;
        while vm.universe.eden.end - vm.universe.eden.top >= need {
            crate::memory::alloc::alloc_slots(vm, target_klass);
        }
        (nm_id, recv)
    }

    /// `tests_s12.md`'s `walker_classifies_all_kinds`: a real I→C→(native
    /// safepoint) chain, executed for real (not hand-faked memory) —
    /// `walk_frames`, run from inside `rt_alloc_slow` via `test_walk_
    /// capture`, must report exactly, innermost first: the alloc_slow
    /// stub itself, `spawn`'s own compiled frame, the `call_stub`
    /// boundary, then the interpreted entry.
    #[test]
    fn walker_classifies_all_kinds() {
        let mut vm = test_vm();
        let (nm_id, recv) = build_and_compile_spawn(&mut vm);

        vm.stack.push(recv);
        vm.test_walk_capture = Some(Ok(Vec::new()));
        // `enter_compiled` calls the REAL call_stub -> spawn's compiled
        // code -> its Alloc's slow edge -> the REAL rt_alloc_slow, which
        // captures a walk_frames snapshot (the test hook) before
        // returning normally.
        assert_eq!(enter_compiled(&mut vm, nm_id, 0), EnterResult::Completed);

        let seen = vm
            .test_walk_capture
            .take()
            .expect("hook must have fired")
            .expect("walk_frames must not have panicked");
        assert_eq!(seen.len(), 4, "{seen:?}");
        assert!(
            matches!(
                seen[0],
                FrameView::Adapter {
                    kind: AdapterKind::AllocSlow,
                    ..
                }
            ),
            "innermost must be the alloc_slow stub itself: {seen:?}"
        );
        assert!(
            matches!(seen[1], FrameView::Compiled { nm, .. } if nm == nm_id),
            "next must be spawn's own compiled frame: {seen:?}"
        );
        assert!(
            matches!(seen[2], FrameView::CallStub { .. }),
            "next must be the call_stub boundary: {seen:?}"
        );
        assert!(
            matches!(seen[3], FrameView::Interpreted { .. }),
            "outermost must be the interpreted entry: {seen:?}"
        );
    }

    /// `tests_s12.md`'s `walker_terminates_on_torn_tierlinks`: with
    /// `test_tear_tier_links_before_walk` armed, `rt_alloc_slow` pops the
    /// `TierLink::IntoCompiled` `enter_compiled` itself just pushed —
    /// so when the walker steps outward from `spawn`'s own compiled frame
    /// and reaches `call_stub`'s boundary, no matching tier link is left
    /// to resume interpreted walking from. Must panic with a clear
    /// message (caught by `rt_alloc_slow`'s own `catch_unwind`, reported
    /// back via `Err`), never loop forever or silently misclassify.
    #[test]
    fn walker_terminates_on_torn_tierlinks() {
        let mut vm = test_vm();
        let (nm_id, recv) = build_and_compile_spawn(&mut vm);

        vm.stack.push(recv);
        vm.test_walk_capture = Some(Ok(Vec::new()));
        vm.test_tear_tier_links_before_walk = true;
        assert_eq!(enter_compiled(&mut vm, nm_id, 0), EnterResult::Completed);

        let err = vm
            .test_walk_capture
            .take()
            .expect("hook must have fired")
            .expect_err("walk_frames must have panicked against the torn tier_links");
        assert!(
            err.contains("no matching TierLink::IntoCompiled left"),
            "got: {err}"
        );
    }
}
