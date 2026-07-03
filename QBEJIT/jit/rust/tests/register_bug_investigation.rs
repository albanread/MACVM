//! Reproduction attempt for the arm64 register-allocation bug documented in
//! `../../aot/known_issues/BUG_REPORT.md`: a value computed before a
//! branch (there: an array data pointer) gets clobbered by the time
//! control reaches a post-merge block, because a register assigned to one
//! predecessor path isn't consistently available at the merge point.
//!
//! The report explicitly ruled out the MADD fusion peephole ("Disabling
//! the MADD peephole optimization does NOT fix this bug... indicating the
//! problem is in register assignment, not instruction selection") — so
//! this reproduces the *shape* (WHILE loop writing an array via mul+add
//! address computation, then a branch-then-merge, then re-reading the
//! same array using the same base pointer) without depending on fusion at
//! all, to isolate whether the underlying regalloc bug is present in the
//! `jit/c` fork this crate drives.

use qbejit::linker::{JumpTableEntry, RuntimeContext};
use qbejit::{compile, encode, linker, memory};

/// A no-op "bounds error" stand-in, resolved through the jump table below —
/// mirrors the original bug's slow path being a real external call
/// (`basic_array_bounds_error`), which the fast path never reaches. Marked
/// `extern "C"` so its address is directly usable as a `JumpTableEntry`.
extern "C" fn dummy_bounds_error(_idx: i32) {
    // Intentionally does nothing — the whole point is that the fast path
    // in these tests never calls it; if it fires the test's assertions
    // will already have caught wrong control flow before this matters.
}

/// Direct transliteration of `test_array_after_while_if.bas`'s shape:
/// a WHILE loop stores `i*10` into `arr[i]` for i=1..10 (backward branch,
/// mul+add address computation, loop-carried base pointer), then an
/// IF-shaped branch-then-merge, then `arr[1]` is read again through the
/// same address-computation pattern. Expected: 10.
#[test]
fn while_loop_then_branch_merge_array_reread() {
    let il = "\
export function w $test() {
@start
\t%arr =l alloc4 404
\t%i =l alloc4 4
\tstorew 1, %i
@loop
\t%iv =w loadw %i
\t%c1 =w csgtw %iv, 10
\tjnz %c1, @after_init, @loop_body
@loop_body
\t%mul10 =w mul %iv, 10
\t%off =l extsw %iv
\t%off4 =l mul %off, 4
\t%addr =l add %arr, %off4
\tstorew %mul10, %addr
\t%iv4 =w add %iv, 1
\tstorew %iv4, %i
\tjmp @loop
@after_init
\t%off1 =l extsw 1
\t%off1b =l mul %off1, 4
\t%addr1 =l add %arr, %off1b
\t%v1 =w loadw %addr1
\t%c2 =w ceqw %v1, 10
\tjnz %c2, @correct, @wrong
@correct
\tjmp @merge
@wrong
\tjmp @merge
@merge
\t%off1c =l extsw 1
\t%off1d =l mul %off1c, 4
\t%addr1e =l add %arr, %off1d
\t%final =w loadw %addr1e
\tret %final
}
";

    let collector = compile::compile_il_jit(il, Some("arm64_apple")).expect("QBE compile failed");
    let module = encode::encode(collector.instructions(), 8192, 4096);
    assert!(module.errors.is_empty(), "encode() reported errors: {:?}", module.errors);

    let sym = module.symbols.get("test").expect("no \"test\" symbol");
    let fn_offset = sym.offset;

    let mut region = memory::JitMemoryRegion::allocate(8192, 4096).expect("allocate JIT memory failed");
    let (result, executable) = linker::link_and_finalize(&module, &mut region, None);
    assert!(executable, "link failed: stats={:?} diagnostics={:?}", result.stats, result.diagnostics);

    let ptr = region.get_function_ptr(fn_offset as usize).expect("get_function_ptr failed");
    // SAFETY: `test` takes no arguments and returns `w` (i32) per the IL above.
    let test_fn: extern "C" fn() -> i32 = unsafe { std::mem::transmute(ptr) };

    let actual = test_fn();
    assert_eq!(
        actual, 10,
        "BUG REPRODUCED: expected arr[1]=10 after WHILE-loop-init + branch-merge \
         re-read, got {actual} (matches BUG_REPORT.md's corruption shape: a \
         plausible-looking garbage/stale value instead of the loop-written one)"
    );
}

/// Same shape, but the two merge-predecessor blocks do visibly different
/// amounts of work (more live temporaries on one path than the other)
/// before converging — widens the register-pressure asymmetry between
/// predecessors, which is the condition under which a parallel-copy /
/// live-range-merge bug at a join point is most likely to surface.
#[test]
fn asymmetric_branch_merge_preserves_base_pointer() {
    let il = "\
export function w $test2(l %data, w %idx) {
@start
\t%cond =w csgew %idx, 0
\tjnz %cond, @patha, @pathb
@patha
\t%x1 =w add %idx, 1
\t%x2 =w add %x1, 1
\t%x3 =w add %x2, 1
\t%x4 =w add %x3, 1
\t%x5 =w add %x4, 1
\t%x6 =w add %x5, 1
\t%x7 =w add %x6, 1
\t%x8 =w add %x7, 1
\tjmp @merge
@pathb
\tjmp @merge
@merge
\t%off =l extsw %idx
\t%off4 =l mul %off, 4
\t%addr =l add %data, %off4
\t%val =w loadw %addr
\tret %val
}
";

    let collector = compile::compile_il_jit(il, Some("arm64_apple")).expect("QBE compile failed");
    let module = encode::encode(collector.instructions(), 4096, 4096);
    assert!(module.errors.is_empty(), "encode() reported errors: {:?}", module.errors);

    let sym = module.symbols.get("test2").expect("no \"test2\" symbol");
    let fn_offset = sym.offset;

    let mut region = memory::JitMemoryRegion::allocate(4096, 4096).expect("allocate JIT memory failed");
    let (result, executable) = linker::link_and_finalize(&module, &mut region, None);
    assert!(executable, "link failed: stats={:?} diagnostics={:?}", result.stats, result.diagnostics);

    let ptr = region.get_function_ptr(fn_offset as usize).expect("get_function_ptr failed");
    // SAFETY: `test2` takes (l, w) and returns w per the IL above.
    let test_fn: extern "C" fn(*const i32, i32) -> i32 = unsafe { std::mem::transmute(ptr) };

    let buf: [i32; 8] = [100, 200, 300, 400, 500, 600, 700, 800];
    for idx in 0..8i32 {
        let actual = test_fn(buf.as_ptr(), idx);
        assert_eq!(actual, buf[idx as usize], "mismatch at idx={idx} (data pointer corrupted across branch-merge?)");
    }
}

/// Closest match to the original bug's actual assembly shape: the slow
/// path is a real external call (caller-saved-register clobbering, exactly
/// like `bl _basic_array_bounds_error` in the report's disassembly), both
/// paths converge, and the post-merge code reuses the pointer argument
/// that was live across the call on the slow path. Exercised through both
/// branches (`take_slow` selects which).
#[test]
fn branch_merge_across_external_call_preserves_pointer() {
    let il = "\
export function w $test3(l %data, w %idx, w %take_slow) {
@start
\tjnz %take_slow, @slow, @fast
@slow
\tcall $dummy_bounds_error(w %idx)
\tjmp @merge
@fast
\tjmp @merge
@merge
\t%off =l extsw %idx
\t%off4 =l mul %off, 4
\t%addr =l add %data, %off4
\t%val =w loadw %addr
\tret %val
}
";

    let collector = compile::compile_il_jit(il, Some("arm64_apple")).expect("QBE compile failed");
    let module = encode::encode(collector.instructions(), 4096, 4096);
    assert!(module.errors.is_empty(), "encode() reported errors: {:?}", module.errors);

    let sym = module.symbols.get("test3").expect("no \"test3\" symbol");
    let fn_offset = sym.offset;

    let mut region = memory::JitMemoryRegion::allocate(4096, 4096).expect("allocate JIT memory failed");

    // QBE's arm64 backend emits external call symbols with a leading `_`
    // (Apple C ABI convention) — the jump table does an exact string match
    // (see linker::RuntimeContext::lookup, ported faithfully from the Zig
    // original, which does the same), so entries must be registered under
    // that exact prefixed name, matching jit_design.md's own example
    // (`"_basic_print_int"`).
    let entries = [JumpTableEntry {
        name: "_dummy_bounds_error".to_string(),
        address: dummy_bounds_error as *const () as u64,
    }];
    let ctx = RuntimeContext { entries: &entries, dlsym_handle: None };

    eprintln!("ext_calls recorded: {:?}", module.ext_calls.iter().map(|e| &e.sym_name).collect::<Vec<_>>());

    let (result, executable) = linker::link_and_finalize(&module, &mut region, Some(&ctx));
    eprintln!(
        "link stats={:?}\nresolved_symbols={:?}\ntrampoline_stubs={:?}\ndiagnostics={:?}",
        result.stats, result.resolved_symbols, result.trampoline_stubs, result.diagnostics
    );
    assert!(executable, "link failed: stats={:?} diagnostics={:?}", result.stats, result.diagnostics);

    let ptr = region.get_function_ptr(fn_offset as usize).expect("get_function_ptr failed");
    // SAFETY: `test3` takes (l, w, w) and returns w per the IL above.
    let test_fn: extern "C" fn(*const i32, i32, i32) -> i32 = unsafe { std::mem::transmute(ptr) };

    let buf: [i32; 8] = [11, 22, 33, 44, 55, 66, 77, 88];
    for idx in 0..8i32 {
        for take_slow in [0i32, 1i32] {
            let actual = test_fn(buf.as_ptr(), idx, take_slow);
            assert_eq!(
                actual,
                buf[idx as usize],
                "mismatch at idx={idx}, take_slow={take_slow} (data pointer corrupted \
                 across a branch-merge where one predecessor makes an external call?)"
            );
        }
    }
}
