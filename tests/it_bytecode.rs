//! Sprint S2 integration test: the `bytecode` module's public API surface,
//! exercised from outside the crate (parity with `tests/it_memory.rs`'s
//! `boot_and_verify` — this is what S3+ builds on).

mod common;

use macvm::bytecode::{decode_at, BytecodeBuilder, Instr};
use macvm::oops::smi::SmallInt;

#[test]
fn build_disassemble_and_decode_from_outside() {
    let mut vm = common::test_vm();
    let mut b = BytecodeBuilder::new();
    b.push_smi_i8(-5);
    b.ret_tos();
    let sel = vm.universe.intern(b"probe");
    let m = b.finish(&mut vm, sel, 0, 0);

    assert_eq!(m.argc(), 0);
    assert_eq!(m.ntemps(), 0);
    assert_eq!(m.bytecode_len(), 3);
    assert_eq!(m.literals().len(), 0);
    assert_eq!(m.ics().len(), 0);

    let (instr, next) = decode_at(m, 0);
    assert_eq!(instr, Instr::PushSmi(-5));
    assert_eq!(next, 2);
    assert_eq!(decode_at(m, 2).0, Instr::ReturnTos);

    let text = macvm::bytecode::disassemble(&vm.universe, m);
    assert!(text.starts_with("method #probe argc=0 ntemps=0 prim=0 flags=\n"));
}

#[test]
fn method_flags_pack() {
    let mut vm = common::test_vm();
    let mut b = BytecodeBuilder::new();
    b.ret_self();
    let sel = vm.universe.intern(b"m");
    let m = b.finish(&mut vm, sel, 15, 255);
    m.set_flags(15, 255, true, true, true);
    assert_eq!(m.argc(), 15);
    assert_eq!(m.ntemps(), 255);
    assert!(m.has_ctx());
    assert!(m.is_block());
    assert!(m.prim_fails());
}

#[test]
fn alloc_method_shape() {
    let mut vm = common::test_vm();
    let m = macvm::memory::alloc::alloc_method(&mut vm, 5);
    assert_eq!(m.bytecode_len(), 5);
    assert_eq!(m.selector(), vm.universe.nil_obj);
    assert_eq!(m.holder(), vm.universe.nil_obj);
    assert_eq!(m.primitive(), 0);
    assert_eq!(m.counters(), 0);
    let mark = macvm::oops::wrappers::MemOop::try_from(m.oop())
        .unwrap()
        .mark();
    assert!(!mark.tagged_contents());
}

#[test]
fn method_size_math() {
    let mut vm = common::test_vm();
    for n in [0usize, 1, 7, 8, 9, 16] {
        let before = vm.universe.eden.top;
        let m = macvm::memory::alloc::alloc_method(&mut vm, n);
        let consumed = (vm.universe.eden.top - before) / 8;
        let nis = vm.universe.method_klass.non_indexable_size();
        assert_eq!(consumed, nis + 1 + n.div_ceil(8), "n={n}");
        assert_eq!(m.bytecode_len(), n);
        for i in 0..n {
            assert_eq!(m.bytecode_byte(i), 0, "n={n} i={i}");
        }
    }
}

#[test]
fn wide_literal_index() {
    let mut vm = common::test_vm();
    let mut b = BytecodeBuilder::new();
    for i in 0..300i64 {
        b.push_literal(SmallInt::new(i).oop());
    }
    b.ret_self();
    let sel = vm.universe.intern(b"m");
    let m = b.finish(&mut vm, sel, 0, 0);
    assert_eq!(m.literals().len(), 300);
    // Entry 299 is stored as push_literal_w (verified precisely in
    // bytecode::builder::tests::builder_wide_literal); here just check the
    // decode agrees end to end via the public API.
    let mut bci = 0usize;
    let mut count = 0usize;
    loop {
        let (instr, next) = decode_at(m, bci);
        if let Instr::PushLiteral(idx) = instr {
            assert_eq!(idx as i64, count as i64);
        } else {
            break; // reached return
        }
        count += 1;
        bci = next;
    }
    assert_eq!(count, 300);
}
