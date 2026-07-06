//! Single point of truth for GC root enumeration (SPEC §7.3 step 1, §7.5
//! steps 1 and 3). Every Rust-side source of live oops outside the heap
//! itself, visited in one fixed order by [`for_each_root`].
//!
//! S7's scavenger originally hand-duplicated this as five separate
//! `scavenge_*_roots` functions in `scavenge.rs`. S8's full GC needs the
//! IDENTICAL root set for two more passes (mark, reference-rewrite) with two
//! more transforms — keeping three hand-written copies in sync by hand is
//! exactly the shape of bug this project spent S7-9 through S7-11 chasing
//! (a root source present in one collector's list but not another's dangles
//! silently until something reads through it). One list, every collector.
//!
//! Deliberately NOT included: the dirty-card scan (old→new remembered-set
//! edges). That is scavenge-specific bookkeeping, not a root source — a full
//! GC traces the whole heap directly and ignores the card table entirely for
//! marking (SPEC §7.5: "old gen is traced, not card-scanned"). It stays in
//! `scavenge.rs`, called separately after `for_each_root`.

use crate::oops::layout::ROOTSPILL_BYTES;
use crate::oops::wrappers::{KlassOop, MethodOop, SymbolOop};
use crate::oops::Oop;
use crate::runtime::frames::{walk_frames, AdapterKind, FrameView};
use crate::runtime::vm_state::VmState;

/// Visits every live root oop, replacing each in place with `f`'s result.
/// Order: well-known singleton oops, well-known selectors, well-known
/// klasses, symbol table, process stack, handle arena, interpreter regs
/// mirror — matching S7's original scan order exactly (some tests pin
/// observable scavenge behavior against it).
///
/// `f` takes `&mut VmState` because a scavenge's transform (`scavenge_oop`)
/// needs it for `to`-space bookkeeping; a full GC's mark-push and
/// forward-chase transforms don't touch `vm`, but share the signature so one
/// generic walker serves all three collector-supplied transforms.
pub fn for_each_root<F>(vm: &mut VmState, mut f: F)
where
    F: FnMut(&mut VmState, Oop) -> Oop,
{
    // --- well-known singleton oops -----------------------------------------
    let o = vm.universe.nil_obj;
    vm.universe.nil_obj = f(vm, o);
    let o = vm.universe.true_obj;
    vm.universe.true_obj = f(vm, o);
    let o = vm.universe.false_obj;
    vm.universe.false_obj = f(vm, o);
    let o = vm.universe.smalltalk;
    vm.universe.smalltalk = f(vm, o);

    // --- well-known selectors (separate Universe-field copies of Symbols
    // already covered by the symbol-table root below — S7-10 root-scan gap:
    // these dangle on their own after the first collection otherwise) ------
    //
    // Rewrapped via `from_oop_unchecked`, not the validating `try_from`
    // (found via a genuine full-GC bug running real source, not a defensive
    // guess): `try_from` confirms shape by reading the CANDIDATE'S OWN
    // klass field's format — a second hop. For the scavenger's transform
    // that's harmless (an eagerly-copied object's new address is already
    // fully populated), but for full GC's phase C the transform is
    // forward-chase, which can return an address phase D hasn't copied
    // ANY bytes to yet — reading through it is invariant F3's exact trap,
    // one more call site than `fullgc::rewrite_entry` already covered.
    // The unchecked cast is sound regardless: this root was symbol/klass/
    // method-shaped before the collection, and a collector transform never
    // changes an object's TYPE, only (at most) its address — re-deriving
    // that already-known shape by reading memory is pure redundancy that
    // happens to be unsafe in exactly this one case.
    macro_rules! root_sel {
        ($($field:ident),* $(,)?) => {
            $(
                let s = vm.universe.$field.oop();
                let ns = f(vm, s);
                // SAFETY: see the comment above this macro.
                vm.universe.$field = unsafe { SymbolOop::from_oop_unchecked(ns) };
            )*
        };
    }
    root_sel!(
        sel_does_not_understand,
        sel_must_be_boolean,
        sel_cannot_return
    );

    // --- well-known klasses --------------------------------------------------
    macro_rules! root_klass {
        ($($field:ident),* $(,)?) => {
            $(
                let k = vm.universe.$field.oop();
                let nk = f(vm, k);
                // SAFETY: see root_sel!'s comment above — identical reasoning.
                vm.universe.$field = unsafe { KlassOop::from_oop_unchecked(nk) };
            )*
        };
    }
    root_klass!(
        metaclass_klass,
        class_klass,
        object_klass,
        undefined_object_klass,
        boolean_klass,
        true_klass,
        false_klass,
        smi_klass,
        character_klass,
        double_klass,
        string_klass,
        symbol_klass,
        array_klass,
        bytearray_klass,
        association_klass,
        methoddict_klass,
        method_klass,
        closure_klass,
        context_klass,
        process_klass,
        message_klass,
        large_pos_int_klass,
        large_neg_int_klass,
        behavior_klass,
        magnitude_klass,
        number_klass,
        integer_klass,
        large_integer_klass,
        collection_klass,
        sequenceable_collection_klass,
        arrayed_collection_klass,
        system_dictionary_klass,
    );

    // --- symbol table (probed by content hash — bucket positions never
    // depend on address, so this is an in-place UPDATE for every collector,
    // never a flush; see fullgc.rs's phase-C table for why that matters) ----
    let n = vm.universe.symbols.buckets.len();
    for i in 0..n {
        if let Some(sym) = vm.universe.symbols.buckets[i] {
            vm.universe.symbols.buckets[i] = Some(f(vm, sym));
        }
    }

    // --- process stack: every live slot 0..sp. Smi-encoded saved_fp/bci
    // links pass through any of the three transforms unchanged (none of
    // scavenge_oop/mark-push/forward-chase touch a non-mem oop) — SPEC
    // §5.1's exact-stack invariant is what makes this scan format-free. ----
    let sp = vm.stack.sp;
    for i in 0..sp {
        let v = vm.stack.get(i);
        let nv = f(vm, v);
        vm.stack.set(i, nv);
    }

    // --- handle arena: every slot 0..len (SPEC §7.6). Never truncated by
    // any collector, only by `HandleScope::drop` — rewritten in place. -----
    let n = vm.handle_arena.len();
    let dbg = std::env::var("MACVM_DBG_ROOTS").is_ok();
    for i in 0..n {
        let v = vm.handle_arena.slots_mut()[i];
        if dbg {
            eprintln!("RDBG root handle[{i}] = {:#x}", v.raw());
        }
        let nv = f(vm, v);
        vm.handle_arena.slots_mut()[i] = nv;
    }

    // --- interpreter regs mirror: `vm.regs.method`, a second copy of the
    // executing frame's method the process-stack scan doesn't reach
    // (S7-10) --------------------------------------------------------------
    if let Some(m) = vm.regs.method {
        let nm = f(vm, m.oop());
        // SAFETY: see root_sel!'s comment above (same reasoning) — found by
        // this EXACT root, running `Smalltalk gcFull` from real source with
        // a genuine executing frame: the synthetic fullgc.rs unit tests
        // never set vm.regs.method, so they could never have caught this.
        vm.regs.method = Some(unsafe { MethodOop::from_oop_unchecked(nm) });
    }
}

/// S12 D4.1: the CODE side of root enumeration — every oop reachable only
/// through a live compiled/adapter frame's own registers-parked-to-memory,
/// or through the code cache's own embedded literal pools, none of which
/// `for_each_root` above ever reaches (that one walks `vm.stack`/handles/
/// well-knowns; nothing there touches `codecache` at all). Called
/// separately from `for_each_root`, not folded into it, because it needs a
/// DIFFERENT walk (`runtime::frames::walk_frames`, native-stack-aware) as
/// its first data source. S12 step 4 wired this into `scavenge` (which
/// always passes `include_key_klass: true` — scavenge never flushes, D5's
/// own text: "treat all code-root kinds strongly at scavenge"); step 5
/// wires it into full GC's own mark AND update/rewrite phases too, one
/// call each, with DIFFERENT `include_key_klass` values (`false` for mark,
/// `true` for update — see that parameter's own doc below).
///
/// `include_key_klass` only ever affects `code_table`'s own two calls
/// below (`CodeTable::oops_do`/`update_keys`, S12 D5's weak-key rule) — the
/// native-frame roots (receiver/args/self, never a customization key) and
/// the other three tables (no key-klass concept at all: PIC guard klasses
/// and adapter method oops stay strong unconditionally, D5's own text)
/// are completely unaffected by it.
///
/// Deliberately DEVIATES from `sprint_s12_detail.md`'s own D4.1 pseudocode
/// signature (`fn each_code_root(vm: &mut VmState, f: &mut dyn FnMut(&mut
/// u64))`, `f` untyped over raw words): that shape cannot actually be
/// called with a transform that needs `&mut VmState` (`scavenge_oop`,
/// exactly the transform `scavenge` itself needs) — a closure built to
/// capture `vm` mutably, then passed alongside `vm` itself as this
/// function's own first argument, is two live mutable borrows of the same
/// value for the whole call, rejected unconditionally regardless of what
/// either function body does internally. `for_each_root` above sidesteps
/// this exact trap by threading `vm` as an explicit PER-CALL argument to
/// `f` rather than letting `f` capture it — this function copies that same
/// convention (`F: FnMut(&mut VmState, Oop) -> Oop`, matching
/// `for_each_root`'s own signature exactly) for the same reason, and
/// bridges to the four embedded-pool tables' own fixed `&mut dyn
/// FnMut(&mut u64)` signature internally, via the identical
/// `std::mem::take`-then-restore dance `scavenge.rs` already used at each
/// of those four call sites before this function existed (moving a table
/// out of `vm` first so a `vm`-capturing closure over the REST of `vm`
/// never aliases the field being called through).
pub fn each_code_root<F>(vm: &mut VmState, include_key_klass: bool, mut f: F)
where
    F: FnMut(&mut VmState, Oop) -> Oop,
{
    // --- 1. native frame roots: compiled spill slots + adapter RootSpill
    // slots. Collected into an owned `Vec` FIRST, exactly like
    // `frames.rs`'s own test module does: `walk_frames` only ever hands out
    // `&VmState` (it has no reason to need more — classifying a frame never
    // mutates anything), so its own borrow of `vm` must fully end before
    // any `f(vm, oop)` call below can reborrow `vm` mutably. -------------
    let mut frames = Vec::new();
    walk_frames(vm, |fv| frames.push(fv));

    for fv in frames {
        match fv {
            FrameView::Compiled { fp, ret_pc, nm } => {
                // Collected to an owned `Vec` for the same reason `frames`
                // itself was above: `iter_slots()` borrows the `OopMap`,
                // which borrows `nmethod`, which borrows `vm.code_table` —
                // that whole chain must end before `f(vm, ..)` can reborrow
                // `vm` mutably inside the loop below.
                let slots: Vec<u16> = vm
                    .code_table
                    .get(nm)
                    .unwrap_or_else(|| {
                        panic!(
                            "each_code_root: a live compiled frame's own nmethod {nm:?} is no \
                             longer installed -- a moving GC must never outlive the code it's \
                             currently executing (D4.2: code never moves, but it can still be \
                             FLUSHED — S12 step 6 — which must first prove no live activation \
                             remains, exactly so this can never fire)"
                        )
                    })
                    .oopmap_at(ret_pc)
                    .iter_slots()
                    .collect();
                for slot in slots {
                    // SAFETY: `fp` is a live compiled frame's own x29
                    // (`walk_frames`'s own invariant — established by
                    // `emit()`'s `stp x29,x30,...; mov x29,sp` prologue,
                    // shared by every published nmethod). Slot addresses
                    // are `fp − 8·(slot+1)`, exactly `compiler::emit::
                    // spill_offset`'s own formula; `oopmap_at`'s map only
                    // ever sets bits for slots the emitter reserved frame
                    // space for (`frame_slots`, checked by `oopmap::
                    // verify` at compile time).
                    let addr = (fp - 8 * (slot as u64 + 1)) as *mut u64;
                    let old = Oop::from_raw(unsafe { *addr });
                    let new = f(vm, old);
                    unsafe { *addr = new.raw() };
                }
            }
            FrameView::Adapter {
                fp,
                kind,
                caller_pc,
            } => {
                let n = real_oop_rootspill_slots(vm, kind, caller_pc);
                debug_assert!(
                    n <= crate::oops::layout::ROOTSPILL_SLOTS,
                    "each_code_root: {n} live RootSpill slots claimed, but the area only \
                     holds {} — a send site exceeded the register-marshaling cap the \
                     compiler was supposed to enforce",
                    crate::oops::layout::ROOTSPILL_SLOTS
                );
                for i in 0..n {
                    // SAFETY: `fp` is a live anchor-setting stub's own x29;
                    // RootSpill occupies `[fp − ROOTSPILL_BYTES, fp)`
                    // (`emit_stub_prologue`'s own `sub sp,sp,#ROOTSPILL_BYTES`
                    // executed AFTER `mov x29,sp`, so `x29` stays above the
                    // area while `sp`/the `stp`s address into it) — slot `i`
                    // (x_i) lives at `fp − ROOTSPILL_BYTES + 8·i`, P8's
                    // pinned ABI.
                    let addr = (fp - ROOTSPILL_BYTES as u64 + 8 * i as u64) as *mut u64;
                    let old = Oop::from_raw(unsafe { *addr });
                    let new = f(vm, old);
                    unsafe { *addr = new.raw() };
                }
            }
            FrameView::CallStub { .. } | FrameView::Interpreted { .. } => {
                // `call_stub` keeps no live oop of its own past the marshal
                // that already happened before this walk (`compiled_call`'s
                // own doc); an interpreted activation's slots are already
                // roots via `for_each_root`'s process-stack scan above.
            }
        }
    }

    // --- 2. code-embedded oops: the four tables' own existing `oops_do`/
    // `update_keys`/`rehash` methods (S10/S11, already correct for both
    // collectors since S11 step 10's scavenge-key fix) — bridged to `f`'s
    // `(vm, Oop) -> Oop` shape via the take-and-restore dance explained in
    // this function's own doc comment above. --------------------------
    let mut code_table = std::mem::take(&mut vm.code_table);
    code_table.oops_do(include_key_klass, &mut |word| {
        *word = f(vm, Oop::from_raw(*word)).raw();
    });
    code_table.update_keys(include_key_klass, &mut |oop| f(vm, oop));
    code_table.rehash();
    vm.code_table = code_table;

    let mut adapters = std::mem::take(&mut vm.adapters);
    adapters.oops_do(&mut |word| {
        *word = f(vm, Oop::from_raw(*word)).raw();
    });
    vm.adapters = adapters;

    let mut pic_table = std::mem::take(&mut vm.pic_table);
    pic_table.oops_do(&mut |word| {
        *word = f(vm, Oop::from_raw(*word)).raw();
    });
    pic_table.update_keys(&mut |oop| f(vm, oop));
    vm.pic_table = pic_table;

    let mut mega_table = std::mem::take(&mut vm.mega_table);
    mega_table.oops_do(&mut |word| {
        *word = f(vm, Oop::from_raw(*word)).raw();
    });
    mega_table.rehash();
    vm.mega_table = mega_table;
}

/// D4.1's per-kind RootSpill interpretation: how many of the EIGHT
/// generically-spilled `x0..x7` words (starting from slot 0) are genuinely
/// live oops for `kind` — the rest may hold stale, non-oop register content
/// from the compiled caller's own unrelated register allocation (a dead
/// value regalloc never bothered to clear) or a raw non-oop argument
/// (`AllocSlow`'s own `size_bytes`), and must NOT be traced as a possible
/// oop: an adversarial-review finding from step 3's own design pass —
/// blindly scanning all 8 for every kind risks tracing garbage bits as a
/// pointer, corrupting unrelated heap memory the moment anything moves.
///
/// `MustBeBoolean`/`AllocSlow` are FIXED (their own call shape never
/// varies): `MustBeBoolean` has exactly one real argument (`val`, x0).
/// `AllocSlow` has `klass` (x0, an oop) then `size_bytes` (x1) — a raw byte
/// count computed by the emitter's own arithmetic (`emit_mov_imm64`/size
/// folding), never smi-tagged, so treating it as a possible oop is
/// actively unsafe, not merely imprecise. `Resolve`/`C2i`/`Mega`/`Dnu` are
/// all reached from an ordinary `Ir::CallSend` site, whose own live word
/// count varies per call site — recovered by looking up the ORIGINAL
/// compiled caller (`caller_pc`, `FrameView::Adapter`'s own field, exactly
/// why it exists) and reading that site's own recorded `IcSite::argc`,
/// which ALREADY INCLUDES the receiver (`ir.rs` builds `CallSiteInfo.argc
/// = ic_view.argc() + 1`, and `rt_dnu` decodes the same field as
/// `real_argc = argc_total - 1` — verified at both ends, because this
/// helper's own first draft wrote `1 + site.argc`, silently re-adding a
/// receiver the count already contains: one EXTRA RootSpill slot scanned,
/// i.e. stale caller register content traced as an oop, the precise
/// corruption this per-kind function exists to prevent. Caught by
/// re-deriving the convention while designing `mid_loop_forced_scavenge`,
/// whose real c2i send under a real scavenge is exactly the test that
/// covers this arm). `Poll` can never
/// reach here at all — `walk_frames`'s own documented invariant is that it
/// never produces `FrameView::Adapter { kind: Poll, .. }` (`stub_poll`
/// never tags the anchor), so this arm defends a static impossibility
/// rather than checking a real runtime condition, matching `AdapterKind::
/// from_raw`'s own "panic on can't-happen" posture.
fn real_oop_rootspill_slots(vm: &VmState, kind: AdapterKind, caller_pc: u64) -> usize {
    match kind {
        AdapterKind::MustBeBoolean => 1,
        AdapterKind::AllocSlow => 1,
        // S14 step 9: the synthetic deopt-bridge anchor owns NO spill slots —
        // it marks an ABANDONED compiled frame whose oops were already
        // materialized into interpreter frames (covered by the linear stack
        // scan); the walk merely passes through to the caller chain.
        AdapterKind::DeoptBridge => 0,
        AdapterKind::Poll => unreachable!(
            "each_code_root: stub_poll never tags the anchor (S10 D5.6) -- walk_frames must \
             never produce FrameView::Adapter{{kind: Poll, ..}} (see its own module doc)"
        ),
        AdapterKind::Resolve | AdapterKind::C2i | AdapterKind::Mega | AdapterKind::Dnu => {
            let nm_id = vm.code_table.find_by_pc(caller_pc).unwrap_or_else(|| {
                panic!(
                    "each_code_root: {kind:?}'s own caller_pc {caller_pc:#x} is not inside any \
                     alive nmethod -- the anchor/tier-link chain and the code table disagree"
                )
            });
            let nmethod = vm.code_table.get(nm_id).expect("just found by find_by_pc");
            let off = (caller_pc - nmethod.code.base as u64) as u32;
            let site = nmethod
                .ic_sites
                .iter()
                .find(|s| s.off + 4 == off)
                .unwrap_or_else(|| {
                    panic!(
                        "each_code_root: {kind:?}'s own caller_pc {caller_pc:#x} (blob offset \
                         {off:#x}) doesn't match any of nmethod {nm_id:?}'s own IcSites"
                    )
                });
            site.argc as usize
        }
    }
}
