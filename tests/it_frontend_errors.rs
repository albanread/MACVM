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
