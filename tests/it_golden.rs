//! Golden disassembly tests (tests_s02.md §Integration/golden tests). Each
//! case builds a method with `BytecodeBuilder`, disassembles it, and
//! compares to a checked-in `tests/golden/*.bc.expected` file byte-for-byte.
//! `UPDATE_GOLDEN=1` regenerates — but `bc_minimal`, `bc_straightline`, and
//! `bc_jumps` are hand-written from independent SPEC arithmetic and must
//! never be regenerated from the disassembler's own output (that would make
//! the jump-base convention untestable — the classic "measured from the
//! opcode byte, every target lands 3 short" bug would pass silently).

mod common;

use macvm::bytecode::disasm::disassemble;
use macvm::bytecode::BytecodeBuilder;
use macvm::memory::alloc;
use macvm::oops::smi::SmallInt;
use macvm::runtime::VmState;

fn golden_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn check_golden(name: &str, actual: &str) {
    let path = golden_dir().join(format!("{name}.bc.expected"));
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading golden {}: {e}", path.display()));
    assert_eq!(
        actual, expected,
        "golden {name} mismatch (run with UPDATE_GOLDEN=1 to inspect/regenerate)"
    );
}

#[test]
fn bc_minimal() {
    let mut vm = common::test_vm();
    let mut b = BytecodeBuilder::new();
    b.ret_self();
    let sel = vm.universe.intern(b"minimal");
    let m = b.finish(&mut vm, sel, 0, 0);
    let text = disassemble(&vm.universe, m);
    check_golden("bc_minimal", &text);
}

#[test]
fn bc_straightline() {
    let mut vm = common::test_vm();
    let mut b = BytecodeBuilder::new();
    b.push_smi_i8(5);
    b.store_temp_pop(0);
    b.push_temp(0);
    b.dup();
    b.pop();
    b.ret_tos();
    let sel = vm.universe.intern(b"straightline");
    let m = b.finish(&mut vm, sel, 0, 1);
    let text = disassemble(&vm.universe, m);
    check_golden("bc_straightline", &text);
}

#[test]
fn bc_jumps() {
    let mut vm = common::test_vm();
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
    let sel = vm.universe.intern(b"jumps");
    let m = b.finish(&mut vm, sel, 0, 0);
    let text = disassemble(&vm.universe, m);
    check_golden("bc_jumps", &text);
}

#[test]
fn bc_loop() {
    let mut vm = common::test_vm();
    let mut b = BytecodeBuilder::new();
    let l = b.new_label();
    b.bind(l);
    b.push_nil();
    b.pop();
    b.jump_back(l);
    b.ret_self(); // unreachable, but finish() requires a terminal return
    let sel = vm.universe.intern(b"loop");
    let m = b.finish(&mut vm, sel, 0, 0);
    let text = disassemble(&vm.universe, m);
    check_golden("bc_loop", &text);
}

fn build_string(vm: &mut VmState, s: &str) -> macvm::oops::Oop {
    let klass = vm.universe.string_klass;
    let obj = alloc::alloc_indexable_bytes(vm, klass, s.len());
    for (i, b) in s.bytes().enumerate() {
        obj.byte_at_put(i, b);
    }
    obj.oop()
}

#[test]
fn bc_literals() {
    let mut vm = common::test_vm();
    let foo = vm.universe.intern(b"foo").oop();
    let bar = build_string(&mut vm, "bar");
    let assoc_klass = vm.universe.association_klass;
    let assoc = alloc::alloc_slots(&mut vm, assoc_klass).oop();

    let mut b = BytecodeBuilder::new();
    b.push_literal(&mut vm, foo);
    b.push_literal(&mut vm, bar);
    b.push_global(&mut vm, assoc);
    b.ret_tos();
    let sel = vm.universe.intern(b"literals");
    let m = b.finish(&mut vm, sel, 0, 0);
    let text = disassemble(&vm.universe, m);
    check_golden("bc_literals", &text);
}

#[test]
fn bc_wide() {
    let mut vm = common::test_vm();
    let mut b = BytecodeBuilder::new();
    for i in 0..260i64 {
        b.push_literal(&mut vm, SmallInt::new(i).oop());
    }
    b.ret_self();
    let sel = vm.universe.intern(b"wide");
    let m = b.finish(&mut vm, sel, 0, 0);
    let text = disassemble(&vm.universe, m);
    check_golden("bc_wide", &text);
}

/// `store_temp` (0x07, non-popping) has no S5 codegen pattern that emits it
/// (assignment-in-value-position uses `dup`+`store_temp_pop` uniformly) —
/// covered here directly so the sprint's full opcode-coverage meta-test
/// (`opcode_coverage_meta`) sees it at least once, per SPEC §4.1's pinned
/// set (tests_s05.md acceptance gate item 1).
#[test]
fn bc_store_temp_nonpop() {
    let mut vm = common::test_vm();
    let mut b = BytecodeBuilder::new();
    b.push_smi_i8(9);
    b.store_temp(0);
    b.pop();
    b.push_temp(0);
    b.ret_tos();
    let sel = vm.universe.intern(b"storeTempNonpop");
    let m = b.finish(&mut vm, sel, 0, 1);
    let text = disassemble(&vm.universe, m);
    check_golden("bc_store_temp_nonpop", &text);
}

/// Narrow `send`(0x20)/`send_super`(0x21) — S3's own goldens exercise real
/// dispatch semantics; this one just pins the disassembly shape (and gives
/// the opcode-coverage meta-test a `send_super` narrow-form hit alongside
/// `tests/it_sends.rs`'s behavioral coverage).
#[test]
fn bc_send_narrow() {
    let mut vm = common::test_vm();
    let mut b = BytecodeBuilder::new();
    let foo_sel = vm.universe.intern(b"foo");
    let bar_sel = vm.universe.intern(b"bar");
    b.push_self();
    b.send(&mut vm, foo_sel, 0);
    b.pop();
    b.push_self();
    b.send_super(&mut vm, bar_sel, 0);
    b.ret_tos();
    let sel = vm.universe.intern(b"sendNarrow");
    let m = b.finish(&mut vm, sel, 0, 0);
    let text = disassemble(&vm.universe, m);
    check_golden("bc_send_narrow", &text);
}

/// `send_w` (0x22): forcing IC site index > 255 auto-widens the builder's
/// emitted opcode (mirrors `bc_wide`'s literal-index widening).
#[test]
fn bc_send_wide() {
    let mut vm = common::test_vm();
    let mut b = BytecodeBuilder::new();
    let sel_each = vm.universe.intern(b"foo");
    for _ in 0..260 {
        b.push_self();
        b.send(&mut vm, sel_each, 0);
        b.pop();
    }
    b.ret_self();
    let sel = vm.universe.intern(b"sendWide");
    let m = b.finish(&mut vm, sel, 0, 0);
    let text = disassemble(&vm.universe, m);
    check_golden("bc_send_wide", &text);
}

/// `send_super_w` (0x23): same widening, for super sends.
#[test]
fn bc_send_super_wide() {
    let mut vm = common::test_vm();
    let mut b = BytecodeBuilder::new();
    let sel_each = vm.universe.intern(b"foo");
    for _ in 0..260 {
        b.push_self();
        b.send_super(&mut vm, sel_each, 0);
        b.pop();
    }
    b.ret_self();
    let sel = vm.universe.intern(b"sendSuperWide");
    let m = b.finish(&mut vm, sel, 0, 0);
    let text = disassemble(&vm.universe, m);
    check_golden("bc_send_super_wide", &text);
}
