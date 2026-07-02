//! Sprint S3 integration/golden tests (tests_s03.md §Integration/golden
//! tests): dispatch through a real hierarchy, super sends, DNU (both the
//! Smalltalk-handler path and the process-exiting fallback), primitive
//! Fail → bytecode fallback, mixed-arity sends, and the wide (`send_w`)
//! encoding.

mod common;

use macvm::bytecode::BytecodeBuilder;
use macvm::interpreter::run_method;
use macvm::memory::alloc;
use macvm::oops::layout::HEADER_WORDS;
use macvm::oops::smi::SmallInt;
use macvm::oops::wrappers::{KlassOop, MethodOop, SymbolOop};
use macvm::oops::Format;
use macvm::runtime::lookup::install_method;
use macvm::runtime::VmState;

fn new_klass(vm: &mut VmState, superclass: KlassOop, name: &str) -> KlassOop {
    vm.universe
        .new_klass(superclass, name, Format::Slots, false, HEADER_WORDS)
}

fn method_returning(vm: &mut VmState, name: &str, tag: i64) -> MethodOop {
    let mut b = BytecodeBuilder::new();
    b.push_smi_i8(tag as i8);
    b.ret_tos();
    let sel = vm.universe.intern(name.as_bytes());
    b.finish(vm, sel, 0, 0)
}

fn build_caller(vm: &mut VmState, sel: SymbolOop, argc: u8) -> MethodOop {
    let mut b = BytecodeBuilder::new();
    for t in 0..=argc {
        b.push_temp(t);
    }
    b.send(sel, argc);
    b.ret_tos();
    let name = vm.universe.intern(b"caller");
    b.finish(vm, name, (argc + 1) as usize, 0)
}

/// A 3-class hierarchy `Root <- Mid <- Leaf`, all under `Object`.
fn three_deep(vm: &mut VmState) -> (KlassOop, KlassOop, KlassOop) {
    let object_klass = vm.universe.object_klass;
    let root = new_klass(vm, object_klass, "Root");
    let mid = new_klass(vm, root, "Mid");
    let leaf = new_klass(vm, mid, "Leaf");
    (root, mid, leaf)
}

#[test]
fn hierarchy_dispatch() {
    let mut vm = common::test_vm();
    let (root, mid, leaf) = three_deep(&mut vm);
    let sel = vm.universe.intern(b"who");
    let m_root = method_returning(&mut vm, "whoRoot", 1);
    install_method(&mut vm, root, sel, m_root);
    let caller = build_caller(&mut vm, sel, 0);

    // Leaf inherits Root's method (nothing shadows it on Mid or Leaf).
    let recv = alloc::alloc_slots(&mut vm, leaf).oop();
    let nil = vm.universe.nil_obj;
    let result = run_method(&mut vm, caller, nil, &[recv]);
    assert_eq!(result, SmallInt::new(1).oop());

    // Mid shadows it.
    let m_mid = method_returning(&mut vm, "whoMid", 2);
    install_method(&mut vm, mid, sel, m_mid);
    let recv2 = alloc::alloc_slots(&mut vm, leaf).oop();
    let nil = vm.universe.nil_obj;
    let result2 = run_method(&mut vm, caller, nil, &[recv2]);
    assert_eq!(result2, SmallInt::new(2).oop());
}

#[test]
fn super_send_chain() {
    let mut vm = common::test_vm();
    let (root, mid, leaf) = three_deep(&mut vm);
    let sel = vm.universe.intern(b"greet");
    let root_impl = method_returning(&mut vm, "greetRoot", 100);
    install_method(&mut vm, root, sel, root_impl);

    // Mid's `greet` calls `super greet`, statically resolving to Root's
    // implementation regardless of the actual (possibly more-derived)
    // receiver klass.
    let mut b = BytecodeBuilder::new();
    b.push_self();
    b.send_super(sel, 0);
    b.ret_tos();
    let mid_impl = b.finish(&mut vm, sel, 0, 0);
    install_method(&mut vm, mid, sel, mid_impl);

    let caller = build_caller(&mut vm, sel, 0);
    let recv_leaf = alloc::alloc_slots(&mut vm, leaf).oop();
    let nil = vm.universe.nil_obj;
    let result = run_method(&mut vm, caller, nil, &[recv_leaf]);
    assert_eq!(result, SmallInt::new(100).oop());
}

#[test]
fn dnu_smalltalk_handler() {
    let mut vm = common::test_vm();
    let object_klass = vm.universe.object_klass;
    let dnu_sel = vm.universe.sel_does_not_understand;

    // `instVarAt:` (primitive id 25), installed on Object so a Message
    // instance can respond to it.
    let instvar_at_sel = vm.universe.intern(b"instVarAt:");
    let mut ivat_b = BytecodeBuilder::new();
    ivat_b.push_self();
    ivat_b.ret_self(); // unreachable: the primitive never Fails here
    let ivat_method = ivat_b.finish(&mut vm, instvar_at_sel, 1, 0);
    ivat_method.set_primitive(25);
    install_method(&mut vm, object_klass, instvar_at_sel, ivat_method);

    // A `doesNotUnderstand:` handler that reads the Message's selector
    // (instVarAt: 1) back out and returns it — proves the DNU path built a
    // real, well-shaped Message and dispatched through a real lookup.
    let mut b = BytecodeBuilder::new();
    b.push_temp(0); // the Message argument (argc=1: t=0 is the sole arg)
    b.push_smi_i8(1);
    b.send(instvar_at_sel, 1);
    b.ret_tos();
    let handler = b.finish(&mut vm, dnu_sel, 1, 0);
    install_method(&mut vm, object_klass, dnu_sel, handler);

    let missing_sel = vm.universe.intern(b"totallyMissing");
    let caller = build_caller(&mut vm, missing_sel, 0);
    let recv = alloc::alloc_slots(&mut vm, object_klass).oop();
    let nil = vm.universe.nil_obj;
    let result = run_method(&mut vm, caller, nil, &[recv]);
    assert_eq!(result, missing_sel.oop());
}

#[test]
fn mixed_arity_sends() {
    let mut vm = common::test_vm();
    let object_klass = vm.universe.object_klass;
    let a = new_klass(&mut vm, object_klass, "A");

    // The method bodies below send `+` to smi accumulators — install the
    // smi `+` primitive (id 1) on `smi_klass` so those inner sends resolve
    // instead of DNU-ing (no image/library is loaded in this test).
    let smi_klass = vm.universe.smi_klass;
    let plus_sel = vm.universe.intern(b"+");
    let mut plus_b = BytecodeBuilder::new();
    plus_b.push_self();
    plus_b.ret_self(); // unreachable: primitive 1 never Fails for two smis
    let plus_method = plus_b.finish(&mut vm, plus_sel, 1, 0);
    plus_method.set_primitive(1);
    install_method(&mut vm, smi_klass, plus_sel, plus_method);

    for argc in 0u8..=3 {
        let sel = vm.universe.intern(format!("m{argc}").as_bytes());
        // Returns the sum of its args as a smi (argc=0 returns 0).
        let mut b = BytecodeBuilder::new();
        b.push_smi_i8(0);
        for t in 0..argc {
            b.push_temp(t); // unified indexing: t < argc addresses arg t directly
            b.send(plus_sel, 1);
        }
        b.ret_tos();
        let m = b.finish(&mut vm, sel, argc as usize, 0);
        install_method(&mut vm, a, sel, m);

        let caller = build_caller(&mut vm, sel, argc);
        let recv = alloc::alloc_slots(&mut vm, a).oop();
        let mut call_args = vec![recv];
        for i in 0..argc as i64 {
            call_args.push(SmallInt::new(i + 1).oop());
        }
        let nil = vm.universe.nil_obj;
        let result = run_method(&mut vm, caller, nil, &call_args);
        let expected: i64 = (1..=argc as i64).sum();
        assert_eq!(result, SmallInt::new(expected).oop(), "argc={argc}");
    }
}

#[test]
fn send_w_wide() {
    let mut vm = common::test_vm();
    let object_klass = vm.universe.object_klass;
    let a = new_klass(&mut vm, object_klass, "A");

    let target_sel = vm.universe.intern(b"target");
    let target_m = method_returning(&mut vm, "targetImpl", 77);
    install_method(&mut vm, a, target_sel, target_m);

    // Pad the caller's own IC table past 255 sites so the real (last) send
    // must be emitted in the wide `send_w` form.
    let mut b = BytecodeBuilder::new();
    b.push_temp(0);
    for i in 0..300 {
        let sel = vm.universe.intern(format!("pad{i}").as_bytes());
        b.add_send(sel, 0); // IC site only, no matching opcode needed
    }
    b.send(target_sel, 0);
    b.ret_tos();
    let caller_sel = vm.universe.intern(b"wideCaller");
    let caller = b.finish(&mut vm, caller_sel, 1, 0);

    // Confirm the emitted opcode is really the wide form.
    assert_eq!(caller.bytecode_byte(2), macvm::bytecode::opcode::OP_SEND_W);

    let recv = alloc::alloc_slots(&mut vm, a).oop();
    let nil = vm.universe.nil_obj;
    let result = run_method(&mut vm, caller, nil, &[recv]);
    assert_eq!(result, SmallInt::new(77).oop());
}

/// The pinned `DNU #<selector> (receiver class <KlassName>)` fallback
/// format and its real `exit(1)`, observed via a subprocess
/// (`std::process::exit` cannot be caught in-process).
#[test]
fn dnu_trace_golden() {
    let exe = env!("CARGO_BIN_EXE_macvm");
    let output = std::process::Command::new(exe)
        .arg("--selftest-dnu-fallback")
        .output()
        .expect("failed to run macvm subprocess");

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("DNU #bar (receiver class Object)\n"),
        "unexpected stdout: {stdout}"
    );
    assert!(
        stdout.contains(">>caller @"),
        "missing frame line: {stdout}"
    );
}
