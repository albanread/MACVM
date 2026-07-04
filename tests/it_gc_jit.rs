//! Sprint S12 integration tests (`tests_s12.md`). This file is allowed
//! `unsafe` (it lives in `tests/`, a separate crate from `macvm` itself) --
//! not that anything here needs it directly; the walker/hook machinery
//! being exercised is `unsafe` internally, not this file's own code.

use macvm::bytecode::builder::BytecodeBuilder;
use macvm::codecache::nmethod::IcState;
use macvm::compiler::driver;
use macvm::frontend::{classdef, parser};
use macvm::interpreter::compiled_call::{enter_compiled, EnterResult};
use macvm::memory::alloc;
use macvm::memory::fullgc;
use macvm::memory::scavenge::scavenge;
use macvm::oops::layout::HEADER_WORDS;
use macvm::oops::smi::SmallInt;
use macvm::oops::wrappers::{KlassOop, MemOop};
use macvm::oops::Format;
use macvm::runtime::lookup::{install_method, klass_of};
use macvm::runtime::{JitMode, VmOptions, VmState};

fn test_vm() -> VmState {
    VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: Default::default(),
        gc_stress: false,
        gc_stress_full_period: None,
        // A smaller-than-default eden (default is a meaningful fraction of
        // `heap_mib`) so this file's own filler loop (see
        // `compiled_frame_spill_slot_survives_forced_scavenge`) genuinely,
        // honestly tops it up with a reasonable number of allocations
        // rather than tens of thousands -- but still comfortably bigger
        // than class-creation's own genesis overhead (4 KiB turned out too
        // small: `Object subclass: AllocTarget [ ]` alone exhausted it).
        eden_kb: Some(256),
        jit: JitMode::Off,
    })
}

/// S12 D4.1 (`tests_s12.md`'s `mid_loop_forced_scavenge`, REDUCED — see
/// this test's own note below for why): proves `memory::roots::
/// each_code_root`'s new compiled-frame-spill-slot scanning finds and
/// correctly relocates a live oop held ONLY in a live compiled frame's own
/// spill slot, across a REAL scavenge. Forced directly
/// (`VmState::test_force_scavenge_in_alloc_slow`, wired into
/// `codecache::stubs::rt_alloc_slow`), bypassing only `scavenge`'s own
/// internal `debug_assert` via the narrow, default-off
/// `test_allow_moving_gc_under_compiled` flag — none of the S11 D8
/// bridge's three PRODUCTION doors (`alloc::alloc_words`'s diversion arm,
/// `prim_gc_scavenge`/`prim_gc_full`'s own `gc_pending` defer) are touched
/// at all, so ordinary Smalltalk code still cannot reach a moving
/// collector under a live compiled frame until step 7 opens all three
/// together (see `memory::scavenge::scavenge`'s own doc comment).
///
/// NOT literally `mid_loop_forced_scavenge` as `tests_s12.md` describes it
/// (a `T>>run:` loop sending the REAL `Gc scavenge` primitive 100 times,
/// asserting `gc_under_compiled >= 100`): that design needs
/// `prim_gc_scavenge`'s OWN defer-guard lifted too, which `alloc.rs`'s own
/// doc comment groups with the rest of the D8 bridge as one interlock,
/// deleted together in step 7 once full-GC integration (step 5) and
/// nmethod flushing (step 6) make opening every door safe. The full
/// loop-based golden (100 iterations, real `Gc scavenge` sends) is
/// deferred to step 7/8; this test proves the SAME underlying mechanism —
/// a real scavenge, a real live compiled frame, a real relocation — today,
/// via a direct collector call instead of a Smalltalk-level send.
#[test]
fn compiled_frame_spill_slot_survives_forced_scavenge() {
    let mut vm = test_vm();
    for item in parser::parse_file("Object subclass: AllocTarget [ ]").expect("parse") {
        classdef::execute_top_item(&mut vm, item).expect("execute");
    }

    // Tenure EVERYTHING now (the class, its metaclass, its method dict —
    // all fresh eden allocations from `execute_top_item` just above) BEFORE
    // deriving any Rust-local handle onto them: a `KlassOop` local
    // computed before this point would itself go stale the moment this
    // scavenge promotes/relocates it — the exact hazard this test's own
    // forced scavenge (below) exists to probe, just one step earlier than
    // intended. `it_tier1.rs`'s own `full_gc_updates_pool_word_and_key_
    // klass` test hits the same shape ("look up the FRESH post-GC value,
    // not a stale pre-GC local"); this sidesteps it by simply not HAVING a
    // pre-GC local yet. Old gen is never touched by a scavenge (`memory::
    // scavenge`'s own module doc), so once tenured, `target_klass` (etc.,
    // derived fresh right after) stays valid for the rest of this test —
    // nothing else scavenges until this test's own forced one, deliberately
    // isolated to `rt_alloc_slow`'s hook far below.
    vm.universe.tenuring_threshold = 0;
    scavenge(&mut vm).expect("scavenge must promote AllocTarget's klass into old gen");
    vm.universe.tenuring_threshold = 127;

    let target_sym = vm.universe.intern(b"AllocTarget");
    let target_assoc =
        macvm::runtime::globals::global_lookup(&vm, target_sym).expect("AllocTarget global");
    let target_klass =
        KlassOop::try_from(MemOop::try_from(target_assoc).unwrap().body_oop(1)).unwrap();

    // A bare `test_vm()` has no world loaded, so `basicNew` (a world
    // method) isn't installed -- install the real basicNew primitive (id
    // 23) on AllocTarget's own metaclass, mirroring `it_tier1.rs`'s own
    // `allocation_fast_and_slow`.
    let basic_new_sel = vm.universe.intern(b"basicNew");
    let target_meta = klass_of(&vm, target_klass.oop());
    let basic_new_method = {
        let mut nb = BytecodeBuilder::new();
        nb.ret_self(); // fallback body -- never reached (prim always succeeds here)
        let m = nb.finish(&mut vm, basic_new_sel, 0, 0);
        m.set_primitive(23);
        m.set_flags(0, 0, false, false, true, false, 0);
        m
    };
    install_method(&mut vm, target_meta, basic_new_sel, basic_new_method);

    // S12 D4.1's own eventual hazard (not this test's job to fix): once
    // step 7 removes the OTHER two D8 bridge doors, `rt_alloc_slow`'s
    // `klass_bits`/`size_bytes` parameters — copied out of the Alloc
    // site's own RootSpill slots into plain Rust locals BEFORE its own
    // `alloc_slots` call (`stub_alloc_slow`'s `mov x1,x0`/`mov x2,x1` run
    // AFTER `emit_stub_prologue`'s `stp` already parked the ORIGINAL
    // x0/x1 to RootSpill, so a native memory copy and a separate Rust
    // copy coexist) — would go stale if a real scavenge relocated the
    // klass INSIDE that call: `each_code_root` correctly updates the
    // NATIVE copy, but nothing re-derives `rt_alloc_slow`'s own Rust
    // locals from it afterward. Moot today (no caller reaches a real GC
    // from inside any of the six stub handlers before step 7), tracked in
    // this sprint's own STEP-4 NOTES for when it stops being moot. This
    // test sidesteps it by keeping `AllocTarget`'s klass tenured (above)
    // for its OWN different reason (a stale Rust-local `KlassOop` in the
    // TEST itself, not in `rt_alloc_slow`) — same underlying shape, worth
    // naming once rather than twice.

    // `keepSelfAlive [ AllocTarget basicNew. ^self ]` -- the alloc's own
    // result is discarded (popped); `self` (the receiver) is used again
    // AFTER it, so it must stay live across the Alloc safepoint. D1's
    // spill-all invariant then forces the register allocator to park it in
    // a spill slot -- exactly the scenario `each_code_root`'s new
    // compiled-frame path exists to scan.
    let mut b = BytecodeBuilder::new();
    b.push_global(&mut vm, target_assoc);
    b.send(&mut vm, basic_new_sel, 0);
    b.pop();
    b.push_self();
    b.ret_tos();
    let sel = vm.universe.intern(b"keepSelfAlive");
    let method = b.finish(&mut vm, sel, 0, 0);
    install_method(&mut vm, target_klass, sel, method);

    // Warm interpreted (mono the basicNew site) on a throwaway receiver --
    // never used again.
    let warm_recv = alloc::alloc_slots(&mut vm, target_klass).oop();
    let warm_result = macvm::interpreter::run_method(&mut vm, method, warm_recv, &[]);
    assert_eq!(
        klass_of(&vm, warm_result).oop().raw(),
        target_klass.oop().raw(),
        "warmup must produce a real AllocTarget"
    );

    assert!(
        driver::eligible(&vm, method),
        "a mono basicNew site is eligible"
    );
    let nm_id = driver::compile_method(&mut vm, target_klass, method).expect("must compile");

    // A FRESH, young receiver -- allocated AFTER the tenuring scavenge
    // above, so it lands in eden and genuinely needs relocating.
    let recv = alloc::alloc_slots(&mut vm, target_klass).oop();

    // Force the Alloc slow edge HONESTLY: top up eden with real, fully
    // valid throwaway instances until less than one more `AllocTarget`
    // fits, rather than lying about `eden.top` (`it_tier1.rs`'s own
    // `allocation_fast_and_slow` does exactly that lie -- `vm.universe.
    // eden.top = vm.universe.eden.end` -- but that test never scavenges,
    // so the gap of never-initialized memory between the real frontier and
    // the lied-about `top` never gets walked. THIS test's own forced
    // scavenge (below) walks the WHOLE heap up to `eden.top` as part of
    // `scavenge`'s own entry-verify (always on in debug builds,
    // `verify::verify_enabled`) -- found empirically the first time this
    // test ran, via a "malformed mark word" panic reading straight into
    // that uninitialized gap.
    let need = target_klass.non_indexable_size() * macvm::oops::layout::WORD_SIZE;
    while vm.universe.eden.end - vm.universe.eden.top >= need {
        alloc::alloc_slots(&mut vm, target_klass);
    }
    vm.test_force_scavenge_in_alloc_slow = true;
    vm.stack.push(recv);
    let gc_under_compiled_before = vm.universe.gc_stats.gc_under_compiled;
    assert_eq!(enter_compiled(&mut vm, nm_id, 0), EnterResult::Completed);

    let (before, after) = vm
        .test_scavenge_probe
        .take()
        .expect("rt_alloc_slow's hook must have fired")
        .expect("the forced scavenge must not have panicked");
    assert_eq!(
        before,
        recv.raw(),
        "the slot's bits before the scavenge must be the receiver this test pushed"
    );
    assert_ne!(
        before, after,
        "the compiled frame's own spill slot must show DIFFERENT bits after a real scavenge \
         relocated the young receiver held there"
    );

    let result = vm.stack.pop();
    assert_eq!(
        result.raw(),
        after,
        "the compiled method's OWN control flow, resuming after rt_alloc_slow returns and \
         reloading self from the (now-updated) spill slot, must observe the SAME relocated \
         address the hook already saw -- not just the hook's own isolated snapshot"
    );
    assert_eq!(
        klass_of(&vm, result).oop().raw(),
        target_klass.oop().raw(),
        "the relocated object must still be a valid, correctly-classed AllocTarget"
    );
    assert!(
        vm.universe.gc_stats.gc_under_compiled > gc_under_compiled_before,
        "the forced scavenge must have counted itself as running under a live compiled frame"
    );
}

/// S12 D5/D6 (`tests_s12.md`'s `key_klass_death_flushes`, REDUCED — see
/// this test's own note, and its companion
/// `compiled_mono_caller_guard_keeps_key_klass_alive` just below, for why):
/// a class with NO global binding and NO live instances, referenced ONLY
/// via a compiled method's own (weakly-treated) `key_klass` field, must
/// actually die on a real full GC — and that death must actually flush
/// the nmethod (gone from `code_table`, its own code-cache space
/// reusable).
///
/// NOT literally `key_klass_death_flushes` as `tests_s12.md` describes it
/// (ALSO driving a compiled caller mono to the dying method, and an
/// interpreter IC to it): tracing `CodeTable::update_keys`'s own existing,
/// correct behavior (D4.1's "everything else in pools stays STRONG" —
/// unconditional, not gated by the weak-key flag at all) shows that ANY
/// live Mono/PIC caller's own guard-klass mirror — or an interpreter IC's
/// own `guard()` oop, an ordinary strong heap field — would ALSO
/// reference the same dying klass, keeping it marked regardless of the
/// dying nmethod's own `key_klass` being correctly excluded. A klass can
/// only actually die once EVERYTHING that ever compared against it —
/// compiled guards, interpreter ICs, the customizing nmethod's own key —
/// has ALSO become unreachable, not just the one nmethod being tested.
/// This test proves the achievable core: the customization-only case.
/// `compiled_mono_caller_guard_keeps_key_klass_alive` proves the OTHER
/// half directly (a surviving caller genuinely blocks the death) — real
/// executed evidence for why the fuller scenario doesn't apply here,
/// documented in `sprint_s12_detail.md`'s own STEP-6 NOTES.
#[test]
fn key_klass_death_flushes() {
    let mut vm = test_vm();
    let object_klass = vm.universe.object_klass;
    let tmp_klass = vm
        .universe
        .new_klass(object_klass, "Tmp", Format::Slots, false, HEADER_WORDS);

    let hot_sel = vm.universe.intern(b"hot");
    let mut b = BytecodeBuilder::new();
    b.push_smi_i8(42);
    b.ret_tos();
    let hot_method = b.finish(&mut vm, hot_sel, 0, 0);
    install_method(&mut vm, tmp_klass, hot_sel, hot_method);

    // Warm interpreted (no sends in `hot`'s own body, so nothing to
    // resolve -- this just satisfies `compile_method`'s own established
    // "warm before compiling" idiom).
    let tmp_instance = alloc::alloc_slots(&mut vm, tmp_klass).oop();
    let warm = macvm::interpreter::run_method(&mut vm, hot_method, tmp_instance, &[]);
    assert_eq!(warm, SmallInt::new(42).oop());

    assert!(driver::eligible(&vm, hot_method), "hot must be eligible");
    let hot_id = driver::compile_method(&mut vm, tmp_klass, hot_method).expect("must compile");
    assert_eq!(
        vm.code_table.get(hot_id).unwrap().key_klass.oop().raw(),
        tmp_klass.oop().raw(),
        "hot's own nmethod must be customized (keyed) for Tmp"
    );

    // `tmp_instance`/`tmp_klass` are now Rust locals ONLY -- never pushed
    // onto `vm.stack`, never stored in a handle, no global binding was
    // ever created (`Universe::new_klass` doesn't install one, unlike
    // `Object subclass: ... [...]` -- confirmed directly from its own
    // source before relying on it here). Nothing but hot's own key_klass
    // field references Tmp at all by this point.
    fullgc::full_gc(&mut vm).expect("full gc must succeed");

    assert!(
        vm.code_table.get(hot_id).is_none(),
        "hot's own nmethod must have been flushed once Tmp became truly unreachable"
    );
    let reused = vm
        .code_cache
        .alloc(1)
        .expect("the code cache must still be able to allocate (no leak/corruption)");
    let _ = reused;
}

/// Companion to `key_klass_death_flushes` just above — real executed
/// proof of the finding that test's own doc explains: a LIVE compiled
/// Mono caller's own guard-klass mirror (kept unconditionally strong by
/// `CodeTable::update_keys`, D4.1's "everything else stays STRONG", NOT
/// gated by S12's weak-key flag at all) is a SEPARATE, independent root
/// for the same klass — so a class customizing a dying-looking nmethod
/// does NOT actually die while any such caller survives, regardless of
/// that nmethod's own `key_klass` being correctly excluded from marking.
///
/// `callHot`'s own send to `hot` is built as HAND-CONSTRUCTED `IrMethod`
/// (an `Ir::CallSend`), bypassing `driver::compile_method`'s own
/// eligibility gate entirely, exactly like `it_tier1.rs`'s own
/// `mono_resolve_patches_call_site_and_dispatches` — that gate's own D1
/// point 2 restricts a compiled `Send` to sites ALREADY interpreter-IC
/// Mono AND guarded on `SmallInteger` targeting an `SMI_INLINE`
/// primitive (a real, separate finding from writing this test: an
/// ordinary dynamic dispatch to a non-smi receiver, like `self hot` here,
/// can never pass `driver::eligible` at all today — S11's own
/// conservative gate, not something this test needed to work around by
/// accident).
#[test]
fn compiled_mono_caller_guard_keeps_key_klass_alive() {
    use macvm::codecache::nmethod::{IcSite, NmState, Nmethod, NmethodId};
    use macvm::codecache::stubs::CallStubFn;
    use macvm::compiler::emit;
    use macvm::compiler::ir::{
        BlockId, CallSiteInfo, Ir, IrBlock, IrMethod, PoolLit, VReg, VRegInfo,
    };
    use macvm::compiler::jasm_assembler::JasmAssembler;
    use macvm::compiler::regalloc;

    let mut vm = test_vm();
    let object_klass = vm.universe.object_klass;
    let tmp_klass = vm
        .universe
        .new_klass(object_klass, "Tmp", Format::Slots, false, HEADER_WORDS);

    let hot_sel = vm.universe.intern(b"hot");
    let mut hb = BytecodeBuilder::new();
    hb.push_smi_i8(42);
    hb.ret_tos();
    let hot_method = hb.finish(&mut vm, hot_sel, 0, 0);
    install_method(&mut vm, tmp_klass, hot_sel, hot_method);
    let tmp_instance = alloc::alloc_slots(&mut vm, tmp_klass).oop();
    let warm = macvm::interpreter::run_method(&mut vm, hot_method, tmp_instance, &[]);
    assert_eq!(warm, SmallInt::new(42).oop());
    assert!(driver::eligible(&vm, hot_method));
    let hot_id = driver::compile_method(&mut vm, tmp_klass, hot_method).expect("hot must compile");
    let hot_nm = vm.code_table.get(hot_id).unwrap();
    let hot_entry = unsafe { hot_nm.code.base.add(hot_nm.entry_off as usize) } as u64;

    // `callHot`: one param (the receiver), one send of `hot` against it --
    // self=x0 -- mirrors `mono_resolve_patches_call_site_and_dispatches`'s
    // own caller shape exactly.
    let vregs: Vec<VRegInfo> = (0..2).map(|_| VRegInfo { is_oop: true }).collect();
    let block0 = IrBlock {
        id: BlockId(0),
        bci: 0,
        code: vec![
            Ir::Param {
                dst: VReg(0),
                index: 0,
            },
            Ir::CallSend {
                dst: VReg(1),
                site: 0,
                args: vec![VReg(0)],
            },
            Ir::Ret { val: VReg(1) },
        ],
        entry_stack: Vec::new(),
    };
    let call_hot_method_ir = IrMethod {
        blocks: vec![block0],
        vregs,
        pool: Vec::new(),
        argc: 1,
        ntemps: 0,
        safepoints: Vec::new(),
        true_lit: PoolLit(0),
        false_lit: PoolLit(0),
        nil_lit: PoolLit(0),
        mark_slots_lit: PoolLit(0),
        call_sites: vec![CallSiteInfo {
            selector: hot_sel,
            argc: 0,
            static_klass: None,
        }],
    };
    let ra = regalloc::regalloc(&call_hot_method_ir);
    let mut asm = JasmAssembler::new();
    let (blob, _pcs, _verified_entry_off, emitted_ic_sites, _safepoints) = emit::emit(
        &mut asm,
        &call_hot_method_ir,
        &ra,
        vm.stubs.stub_poll_addr(),
        vm.stubs.must_be_boolean_addr(),
        vm.stubs.alloc_slow_addr(),
        None,
    );
    assert_eq!(emitted_ic_sites.len(), 1, "exactly one Ir::CallSend");

    let h = vm.code_cache.alloc(blob.code.len()).unwrap();
    vm.code_cache.publish(h, &blob);
    let resolve_addr = vm.stubs.resolve_addr();
    for site in &emitted_ic_sites {
        vm.code_cache.patch_branch26_at(h, site.off, resolve_addr);
    }
    let call_hot_sel = vm.universe.intern(b"callHotProbe");
    let ic_sites: Vec<IcSite> = emitted_ic_sites
        .iter()
        .map(|s| IcSite {
            off: s.off,
            selector: s.selector,
            argc: s.argc,
            state: IcState::Unresolved,
        })
        .collect();
    let call_hot_nm = Nmethod {
        id: NmethodId(0),
        key_klass: tmp_klass,
        key_selector: call_hot_sel,
        code: h,
        entry_off: 0,
        verified_entry_off: 0,
        state: NmState::Alive,
        level: 1,
        version: 0,
        literal_off: blob.literal_off,
        relocs: blob.relocs.clone(),
        frame_slots: ra.frame_slots,
        slot_is_oop: ra.slot_is_oop.clone(),
        pcdescs: Vec::new(),
        oopmaps: Vec::new(),
        ic_sites,
        poll_bci: None,
    };
    let call_hot_id = vm.code_table.install(call_hot_nm);
    let call_hot_entry = h.base as u64; // entry_off == verified_entry_off == 0 (no guard, `None`)

    // First call resolves the site to Mono{klass: Tmp, target: hot_entry}
    // (through stub_resolve, exactly `mono_resolve_patches_call_site_
    // and_dispatches`'s own first-dispatch arm).
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let argv = [tmp_instance.raw()];
    let result = unsafe { call(call_hot_entry, vm_ptr, argv.as_ptr(), 1) };
    assert_eq!(result, SmallInt::new(42).oop().raw());
    match vm.code_table.get(call_hot_id).unwrap().ic_sites[0].state {
        IcState::Mono { klass, target } => {
            assert_eq!(klass, tmp_klass, "must record the receiver's own klass");
            assert_eq!(target, hot_entry, "must record hot's own entry");
        }
        other => panic!("expected Mono after the first resolve, got {other:?}"),
    }

    // Same "no global, no live instance" setup as `key_klass_death_
    // flushes` -- the ONLY difference is callHot's own live Mono guard,
    // which still names Tmp.
    fullgc::full_gc(&mut vm).expect("full gc must succeed");

    assert!(
        vm.code_table.get(hot_id).is_some(),
        "hot must survive: callHot's own live Mono guard still names Tmp, keeping it \
         reachable regardless of hot's own key_klass being correctly excluded from marking"
    );
    assert!(
        vm.code_table.get(call_hot_id).is_some(),
        "callHot must survive too -- its own key_klass is ALSO Tmp, kept alive the same way"
    );
    assert!(
        matches!(
            vm.code_table.get(call_hot_id).unwrap().ic_sites[0].state,
            IcState::Mono { .. }
        ),
        "callHot's own site must be untouched -- nothing was flushed"
    );
}
