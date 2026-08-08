//! Stress/negative tests (`tests_s05.md` §Stress/negative tests):
//! deliberate parse/compile failures, each asserting an exact `line:col`
//! and a message substring (Pitfalls P10 — `file:line:col: error: msg`).

use macvm::frontend::parser::parse_file;
use macvm::frontend::{codegen, CompileError};
use macvm::runtime::vm_state::{VmOptions, VmState};

fn test_vm() -> VmState {
    VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: Default::default(),
        gc_stress: false,
        gc_stress_full_period: None,
        eden_kb: None,
        jit: macvm::runtime::JitMode::Off,
    })
}

fn parse_err(src: &str) -> CompileError {
    parse_file(src).expect_err(&format!("expected a parse error for: {src}"))
}

#[test]
fn unterminated_string_errors_at_opening_position() {
    let e = parse_err("'no closing quote.");
    assert_eq!(e.span.line, 1);
    assert_eq!(e.span.col, 1);
    assert!(e.eof);
}

#[test]
fn unterminated_comment_errors_at_opening_position() {
    let e = parse_err("\"no closing quote.");
    assert_eq!(e.span.line, 1);
    assert_eq!(e.span.col, 1);
    assert!(e.eof);
}

#[test]
fn unterminated_pragma_errors() {
    let src = "Object subclass: X [ foo [ <primitive: 1\n";
    let e = parse_err(src);
    assert!(e.eof || e.msg.contains("unterminated") || e.msg.contains("expected"));
}

#[test]
fn bad_radix_literal_16r_no_digits() {
    let e = parse_err("16r.");
    assert_eq!(e.span, macvm::frontend::lexer::Span { line: 1, col: 1 });
    assert!(e.msg.contains("radix") || e.msg.contains("digit"));
}

#[test]
fn bad_radix_literal_digit_out_of_range() {
    let e = parse_err("8r9.");
    assert_eq!(e.span, macvm::frontend::lexer::Span { line: 1, col: 1 });
}

#[test]
fn dollar_at_eof_errors() {
    let e = parse_err("$");
    assert!(e.eof);
}

#[test]
fn caret_mid_expression_errors() {
    let e = parse_err("x foo: ^y.");
    assert!(e.msg.to_lowercase().contains("expression") || e.msg.contains("^"));
}

#[test]
fn cascade_on_non_send_errors() {
    let e = parse_err("3; foo.");
    assert!(e.msg.contains("cascade"));
}

#[test]
fn binary_op_chain_errors() {
    let e = parse_err("a + + b.");
    assert_eq!(e.span.line, 1);
}

#[test]
fn unclosed_block_is_eof_continuation() {
    let e = parse_err("[:x | x");
    assert!(
        e.eof,
        "an unclosed block must be an EOF-continuation error, got: {e}"
    );
}

#[test]
fn duplicate_primitive_pragma_errors() {
    let e = parse_err("Object subclass: X [ foo [ <primitive: 7> <primitive: 8> ^1 ] ]");
    assert!(e.msg.contains("duplicate"));
}

#[test]
fn duplicate_temp_declared_self_errors() {
    let e = parse_err("Object subclass: X [ foo [ | self | ^1 ] ]");
    assert!(e.msg.contains("reserved"));
}

#[test]
fn undeclared_variable_in_method_is_a_codegen_error() {
    let mut vm = test_vm();
    let object_klass = vm.universe.object_klass;
    let mut items = parse_file("Object subclass: X [ foo [ ^Zork ] ]").unwrap();
    let macvm::frontend::ast::TopItem::ClassDef(mut c) = items.remove(0) else {
        panic!("expected a class def")
    };
    let err = codegen::compile_method(&mut vm, object_klass, false, &mut c.methods[0])
        .expect_err("Zork is not declared anywhere");
    assert!(err.msg.contains("Zork"));
    assert!(err.span.line >= 1);
}

fn compile_first_method(vm: &mut VmState, src: &str) -> Result<(), CompileError> {
    let mut items = parse_file(src).expect("parses");
    let macvm::frontend::ast::TopItem::ClassDef(mut c) = items.remove(0) else {
        panic!("expected a class def")
    };
    let object_klass = vm.universe.object_klass;
    codegen::compile_method(vm, object_klass, false, &mut c.methods[0]).map(|_| ())
}

#[test]
fn primitive_with_too_many_args_is_a_codegen_error() {
    // 8 keyword args + a numbered primitive: within METHOD_ARGC_MAX (15) but
    // over MAX_PRIMITIVE_ARGS (7), so it must be rejected at compile time rather
    // than overflowing try_primitive's fixed arg buffer at call time.
    let mut vm = test_vm();
    let err = compile_first_method(
        &mut vm,
        "Object subclass: X [ p: a q: b r: c s: d t: e u: f v: g w: h [ <primitive: 99> ^self ] ]",
    )
    .expect_err("an 8-arg numbered primitive must be rejected");
    assert!(err.msg.contains("primitive"), "msg: {}", err.msg);
    assert!(err.msg.contains("at most"), "msg: {}", err.msg);
}

#[test]
fn seven_arg_primitive_and_eight_arg_ordinary_method_are_ok() {
    // The guard is primitive-specific and boundary-correct: exactly
    // MAX_PRIMITIVE_ARGS (7) args on a primitive is fine, and an ordinary
    // (non-primitive) method may exceed it up to METHOD_ARGC_MAX.
    let mut vm = test_vm();
    compile_first_method(
        &mut vm,
        "Object subclass: X [ a: a b: b c: c d: d e: e f: f g: g [ <primitive: 99> ^self ] ]",
    )
    .expect("a 7-arg primitive fits the buffer");
    compile_first_method(
        &mut vm,
        "Object subclass: Y [ a: a b: b c: c d: d e: e f: f g: g h: h [ ^self ] ]",
    )
    .expect("an 8-arg ordinary method is not bound by the primitive limit");
}

#[test]
fn reopen_with_new_instvars_errors() {
    let mut vm = test_vm();
    let mut items1 = parse_file("Object subclass: X [ | a | ]").unwrap();
    let macvm::frontend::ast::TopItem::ClassDef(mut c1) = items1.remove(0) else {
        unreachable!()
    };
    macvm::frontend::classdef::install_class_def(&mut vm, &mut c1).unwrap();

    let mut items2 = parse_file("Object subclass: X [ | b | ]").unwrap();
    let macvm::frontend::ast::TopItem::ClassDef(mut c2) = items2.remove(0) else {
        unreachable!()
    };
    let err = macvm::frontend::classdef::install_class_def(&mut vm, &mut c2)
        .expect_err("reopen must reject new instvars");
    assert!(err.msg.contains("cannot change shape"));
}

/// Identical-shape tolerance: re-declaring the class's own inst vars
/// VERBATIM is a plain method reopen — declared methods are REPLACED,
/// undeclared ones KEPT — so re-running a whole class-def doc example (or
/// re-filing an unchanged one) is idempotent instead of a shape error.
#[test]
fn reopen_with_identical_instvars_is_a_method_reopen() {
    let mut vm = test_vm();
    let run = |vm: &mut VmState, src: &str| -> Option<macvm::oops::Oop> {
        let mut items = parse_file(src).unwrap();
        macvm::frontend::classdef::execute_top_item(vm, items.remove(0)).unwrap()
    };
    // A bare test VM has no world (no Object>>new), so the class carries its
    // own instantiation via the basicNew primitive.
    run(
        &mut vm,
        "Object subclass: X [ | a b | X class >> make [ <primitive: 23> ] v [ ^1 ] w [ ^9 ] ]",
    );
    // Same ivars, same order: accepted, and v's body is the NEW one.
    run(
        &mut vm,
        "Object subclass: X [ | a b | X class >> make [ <primitive: 23> ] v [ ^2 ] ]",
    );
    let v = run(&mut vm, "X make v.").expect("doit answers a value");
    assert_eq!(
        macvm::oops::smi::SmallInt::try_from(v).unwrap().value(),
        2,
        "the redefined method body must win"
    );
    // A method the reopen did not declare survives (classic reopen).
    let w = run(&mut vm, "X make w.").expect("doit answers a value");
    assert_eq!(macvm::oops::smi::SmallInt::try_from(w).unwrap().value(), 9);
}

/// Order matters — slots are positional. `| a b |` vs `| b a |` is a real
/// shape change and stays the hard error.
#[test]
fn reopen_with_reordered_instvars_errors() {
    let mut vm = test_vm();
    let mut items1 = parse_file("Object subclass: X [ | a b | ]").unwrap();
    let macvm::frontend::ast::TopItem::ClassDef(mut c1) = items1.remove(0) else {
        unreachable!()
    };
    macvm::frontend::classdef::install_class_def(&mut vm, &mut c1).unwrap();

    let mut items2 = parse_file("Object subclass: X [ | b a | ]").unwrap();
    let macvm::frontend::ast::TopItem::ClassDef(mut c2) = items2.remove(0) else {
        unreachable!()
    };
    let err = macvm::frontend::classdef::install_class_def(&mut vm, &mut c2)
        .expect_err("reordered instvars are a shape change");
    assert!(err.msg.contains("cannot change shape"));
}

/// A subset is not identical — dropping an ivar is a shape change too.
#[test]
fn reopen_with_fewer_instvars_errors() {
    let mut vm = test_vm();
    let mut items1 = parse_file("Object subclass: X [ | a b | ]").unwrap();
    let macvm::frontend::ast::TopItem::ClassDef(mut c1) = items1.remove(0) else {
        unreachable!()
    };
    macvm::frontend::classdef::install_class_def(&mut vm, &mut c1).unwrap();

    let mut items2 = parse_file("Object subclass: X [ | a | ]").unwrap();
    let macvm::frontend::ast::TopItem::ClassDef(mut c2) = items2.remove(0) else {
        unreachable!()
    };
    let err = macvm::frontend::classdef::install_class_def(&mut vm, &mut c2)
        .expect_err("declaring fewer instvars is a shape change");
    assert!(err.msg.contains("cannot change shape"));
}

#[test]
fn reopen_with_indexable_errors() {
    let mut vm = test_vm();
    let mut items1 = parse_file("Object subclass: X [ ]").unwrap();
    let macvm::frontend::ast::TopItem::ClassDef(mut c1) = items1.remove(0) else {
        unreachable!()
    };
    macvm::frontend::classdef::install_class_def(&mut vm, &mut c1).unwrap();

    let mut items2 = parse_file("Object subclass: X [ <indexable: bytes> ]").unwrap();
    let macvm::frontend::ast::TopItem::ClassDef(mut c2) = items2.remove(0) else {
        unreachable!()
    };
    let err = macvm::frontend::classdef::install_class_def(&mut vm, &mut c2)
        .expect_err("reopen must reject a shape-changing <indexable:>");
    assert!(err.msg.contains("cannot change shape"));
}

#[test]
fn reopen_with_changed_superclass_errors() {
    let mut vm = test_vm();
    let mut items1 = parse_file("Object subclass: X [ ]").unwrap();
    let macvm::frontend::ast::TopItem::ClassDef(mut c1) = items1.remove(0) else {
        unreachable!()
    };
    macvm::frontend::classdef::install_class_def(&mut vm, &mut c1).unwrap();

    // X's real superclass is Object; declare-reopen against a different one.
    let mut items2 = parse_file("Boolean subclass: X [ ]").unwrap();
    let macvm::frontend::ast::TopItem::ClassDef(mut c2) = items2.remove(0) else {
        unreachable!()
    };
    let err = macvm::frontend::classdef::install_class_def(&mut vm, &mut c2)
        .expect_err("reopen with a different declared superclass must error");
    assert!(err.msg.contains("cannot change shape") || err.msg.contains("superclass"));
}

#[test]
fn unknown_superclass_errors() {
    let mut vm = test_vm();
    let mut items = parse_file("Zork subclass: Foo [ ]").unwrap();
    let macvm::frontend::ast::TopItem::ClassDef(mut c) = items.remove(0) else {
        unreachable!()
    };
    let err = macvm::frontend::classdef::install_class_def(&mut vm, &mut c)
        .expect_err("unknown superclass must error");
    assert!(err.msg.contains("not found"));
}

#[test]
fn class_method_name_mismatch_errors() {
    let e = parse_err("Object subclass: Baz [ Foo class >> bar [ ^1 ] ]");
    assert!(e.msg.contains("does not match"));
}

#[test]
fn too_many_params_overflow_errors() {
    let mut vm = test_vm();
    let object_klass = vm.universe.object_klass;
    let names: Vec<String> = (0..16).map(|i| format!("p{i}")).collect();
    let pattern: String = names.iter().map(|n| format!("k{n}: {n} ")).collect();
    let src = format!("Object subclass: X [ {pattern} [ ^1 ] ]");
    let mut items = parse_file(&src).unwrap();
    let macvm::frontend::ast::TopItem::ClassDef(mut c) = items.remove(0) else {
        unreachable!()
    };
    let err = codegen::compile_method(&mut vm, object_klass, false, &mut c.methods[0])
        .expect_err("16 params must exceed the 4-bit argc field");
    assert!(err.msg.contains("parameters"));
}

#[test]
fn byte_array_element_out_of_range_errors() {
    let e = parse_err("#[300].");
    assert!(e.msg.contains("range") || e.msg.contains("byte"));
    let e2 = parse_err("#[-1].");
    assert!(e2.msg.contains("integer") || e2.msg.contains("range"));
    let e3 = parse_err("#[foo].");
    assert!(e3.msg.contains("integer"));
}
