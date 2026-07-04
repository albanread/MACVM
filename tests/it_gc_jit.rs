//! Sprint S12 integration tests (`tests_s12.md`). This file is allowed
//! `unsafe` (it lives in `tests/`, a separate crate from `macvm` itself) --
//! not that anything here needs it directly; the walker/hook machinery
//! being exercised is `unsafe` internally, not this file's own code.

use macvm::bytecode::builder::BytecodeBuilder;
use macvm::compiler::driver;
use macvm::frontend::{classdef, parser};
use macvm::interpreter::compiled_call::{enter_compiled, EnterResult};
use macvm::memory::alloc;
use macvm::memory::scavenge::scavenge;
use macvm::oops::wrappers::{KlassOop, MemOop};
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
