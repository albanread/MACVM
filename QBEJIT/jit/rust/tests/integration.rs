//! End-to-end: QBE IL text → compile → encode → allocate → link → execute.
//!
//! This is the check `../README.md`'s "What's verified, and what's still
//! owed" section says is necessary but not sufficient — it proves the
//! whole pipeline wired together correctly (FFI boundary, encoder, memory
//! manager, linker all agreeing on the same contract), not that the port
//! is bug-free in general. Run with `cargo test --test integration`.

use qbejit::{compile, encode, linker, memory};

/// `add(a, b) = a + b` — smallest useful test of the whole pipeline:
/// exercises parameter passing, a register-register ALU op, and `ret`.
#[test]
fn compile_encode_link_execute_add() {
    let il = "\
export function w $add(w %a, w %b) {
@start
\t%c =w add %a, %b
\tret %c
}
";

    let collector = compile::compile_il_jit(il, Some("arm64_apple")).expect("QBE compile failed");
    let insts = collector.instructions();
    assert!(!insts.is_empty(), "collector produced no instructions");

    let module = encode::encode(insts, 4096, 4096);
    assert!(module.errors.is_empty(), "encode() reported errors: {:?}", module.errors);

    let sym = module
        .symbols
        .get("add")
        .or_else(|| module.symbols.get("$add"))
        .unwrap_or_else(|| panic!("no \"add\" symbol in {:?}", module.symbols.keys().collect::<Vec<_>>()));
    assert!(sym.is_code, "\"add\" symbol should be a code symbol");
    let fn_offset = sym.offset;

    let mut region = memory::JitMemoryRegion::allocate(4096, 4096).expect("allocate JIT memory failed");
    let (result, executable) = linker::link_and_finalize(&module, &mut region, None);
    assert!(
        executable,
        "link_and_finalize did not produce executable code: stats={:?} diagnostics={:?}",
        result.stats, result.diagnostics
    );

    let ptr = region.get_function_ptr(fn_offset as usize).expect("get_function_ptr failed");
    // SAFETY: `ptr` points at the start of `add`'s machine code within a
    // region `make_executable()` just switched to RX, produced by encoding
    // + linking a QBE function with signature `w $add(w, w)` — the AAPCS64
    // calling convention for `(i32, i32) -> i32` matches exactly.
    let add_fn: extern "C" fn(i32, i32) -> i32 = unsafe { std::mem::transmute(ptr) };

    assert_eq!(add_fn(19, 23), 42);
    assert_eq!(add_fn(-5, 5), 0);
    assert_eq!(add_fn(i32::MAX, 1), i32::MIN); // wraps, matching QBE's `w add` semantics
}

/// A second, independent function shape (a loop with a backward branch and
/// a conditional) — the first test never exercises `resolve_fixups`'s
/// backward-branch path or `B_COND`, both real risk areas in the port.
#[test]
fn compile_encode_link_execute_loop_sum() {
    // sum_to(n) = 0 + 1 + ... + n, via a WHILE-shaped loop, forcing a
    // backward branch (loop back-edge) and a forward conditional exit.
    let il = "\
export function w $sum_to(w %n) {
@start
\t%sum =w copy 0
\t%i =w copy 1
@loop
\t%cond =w csgtw %i, %n
\tjnz %cond, @done, @body
@body
\t%sum1 =w add %sum, %i
\t%i1 =w add %i, 1
\t%sum =w copy %sum1
\t%i =w copy %i1
\tjmp @loop
@done
\tret %sum
}
";

    let collector = compile::compile_il_jit(il, Some("arm64_apple")).expect("QBE compile failed");
    let module = encode::encode(collector.instructions(), 4096, 4096);
    assert!(module.errors.is_empty(), "encode() reported errors: {:?}", module.errors);

    let sym = module.symbols.get("sum_to").expect("no \"sum_to\" symbol");
    let fn_offset = sym.offset;

    let mut region = memory::JitMemoryRegion::allocate(4096, 4096).expect("allocate JIT memory failed");
    let (result, executable) = linker::link_and_finalize(&module, &mut region, None);
    assert!(executable, "link failed: stats={:?} diagnostics={:?}", result.stats, result.diagnostics);

    let ptr = region.get_function_ptr(fn_offset as usize).expect("get_function_ptr failed");
    // SAFETY: see compile_encode_link_execute_add — same reasoning, signature `w(w)`.
    let sum_to: extern "C" fn(i32) -> i32 = unsafe { std::mem::transmute(ptr) };

    assert_eq!(sum_to(0), 0);
    assert_eq!(sum_to(5), 15);
    assert_eq!(sum_to(10), 55);
}
