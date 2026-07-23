//! T1/T2 integration tests (`docs/typechecker_design.md` §6). Two layers:
//! direct calls into the public `macvm::types` API (as any external
//! consumer — the GUI, a future editor integration — would use it), and a
//! real subprocess drive of the `macvm typecheck` CLI (`it_cli.rs`'s own
//! pattern, self-contained here since integration test binaries don't
//! share code across files).

use std::path::{Path, PathBuf};
use std::process::Command;

use macvm::types::check::TypeError;
use macvm::types::typecheck_world;

fn temp_world_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("macvm_it_typecheck_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The real, checked-in world has zero annotations today — a REGRESSION
/// GUARD: if anyone writes a broken annotation into `world/*.mst` in the
/// future, this is what catches it in CI (the unit tests in `src/types/`
/// only ever exercise synthetic fixtures, never this real corpus).
#[test]
fn real_world_has_zero_type_findings() {
    let (model, errors) = typecheck_world(Path::new("world")).expect("typecheck the real world");
    assert!(
        model.classes.len() > 100,
        "sanity: the real world has 100+ classes, got {}",
        model.classes.len()
    );
    assert_eq!(errors, vec![], "the unannotated real world must have ZERO findings");
}

#[test]
fn direct_api_reports_each_t1_error_kind() {
    let dir = temp_world_dir("direct_api");
    std::fs::write(dir.join("01_object.mst"), "nil subclass: Object [\n]\n").unwrap();
    std::fs::write(
        dir.join("02_foo.mst"),
        "Object subclass: Foo [\n\
         \x20   | bad <Cltn[]> |\n\
         \x20   one: n <Cltn[Object]> [ ^n ]\n\
         \x20   two: n <Cltn[Object, Object]> [ ^n ]\n\
         \x20   three: n <StillUndeclared> [ ^n ]\n\
         ]\n",
    )
    .unwrap();
    std::fs::write(dir.join("world.list"), "01_object.mst\n02_foo.mst\n").unwrap();

    let (_model, errors) = typecheck_world(&dir).expect("typecheck the fixture");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, TypeError::MalformedTypeExpr { .. })),
        "expected a MalformedTypeExpr from '<Cltn[]>', got {errors:#?}"
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, TypeError::UndeclaredTypeName { name, .. } if name == "StillUndeclared")),
        "expected an UndeclaredTypeName from '<StillUndeclared>', got {errors:#?}"
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, TypeError::GenericArityMismatch { head, .. } if head == "Cltn")),
        "expected a GenericArityMismatch between 'Cltn[Object]' and \
         'Cltn[Object, Object]', got {errors:#?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// T2's two new error kinds, through the SAME public API a T1 caller
/// would use. `types::expr_type`'s own `src/types/` unit tests already
/// cover the underlying rules in depth (shadowing, non-local returns,
/// the implicit `^self` case, …) — this just confirms they surface
/// correctly through `typecheck_world`, not only from inside the module.
#[test]
fn direct_api_reports_each_t2_error_kind() {
    let dir = temp_world_dir("direct_api_t2");
    std::fs::write(dir.join("00_object.mst"), "nil subclass: Object [\n]\n").unwrap();
    std::fs::write(
        dir.join("01_tower.mst"),
        "Object subclass: Integer [\n]\nObject subclass: String [\n]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("02_foo.mst"),
        "Object subclass: Foo [\n\
         \x20   badAssign [\n\
         \x20       | x <Integer> |\n\
         \x20       x := 'hello'.\n\
         \x20       ^x\n\
         \x20   ]\n\
         \x20   badReturn ^ <Integer> [\n\
         \x20       ^'hello'\n\
         \x20   ]\n\
         ]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("world.list"),
        "00_object.mst\n01_tower.mst\n02_foo.mst\n",
    )
    .unwrap();

    let (_model, errors) = typecheck_world(&dir).expect("typecheck the fixture");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, TypeError::AssignmentNotSubtype { var_name, .. } if var_name == "x")),
        "expected an AssignmentNotSubtype from x := 'hello', got {errors:#?}"
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, TypeError::ReturnNotSubtype { .. })),
        "expected a ReturnNotSubtype from ^'hello' against ^<Integer>, got {errors:#?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_macvm"))
}

fn run(args: &[&str]) -> (i32, String) {
    let out = Command::new(bin_path())
        .args(args)
        .output()
        .expect("spawn macvm");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn cli_reports_and_exits_zero_without_strict() {
    let dir = temp_world_dir("cli_no_strict");
    std::fs::write(dir.join("01_object.mst"), "nil subclass: Object [\n]\n").unwrap();
    std::fs::write(
        dir.join("02_foo.mst"),
        "Object subclass: Foo [\n    bar: n <Missing> [ ^n ]\n]\n",
    )
    .unwrap();
    std::fs::write(dir.join("world.list"), "01_object.mst\n02_foo.mst\n").unwrap();

    let (status, stdout) = run(&["typecheck", "--world", dir.to_str().unwrap()]);
    assert_eq!(status, 0, "advisory: findings must not fail the exit code by default");
    assert!(stdout.contains("undeclared type name 'Missing'"), "got: {stdout}");
    assert!(stdout.contains("1 finding"), "got: {stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cli_strict_exits_nonzero_on_findings() {
    let dir = temp_world_dir("cli_strict");
    std::fs::write(dir.join("01_object.mst"), "nil subclass: Object [\n]\n").unwrap();
    std::fs::write(
        dir.join("02_foo.mst"),
        "Object subclass: Foo [\n    bar: n <Missing> [ ^n ]\n]\n",
    )
    .unwrap();
    std::fs::write(dir.join("world.list"), "01_object.mst\n02_foo.mst\n").unwrap();

    let (status, _) = run(&["typecheck", "--world", dir.to_str().unwrap(), "--strict"]);
    assert_eq!(status, 1);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cli_class_filter_narrows_the_report() {
    let dir = temp_world_dir("cli_filter");
    std::fs::write(dir.join("01_object.mst"), "nil subclass: Object [\n]\n").unwrap();
    std::fs::write(
        dir.join("02_foo.mst"),
        "Object subclass: Foo [\n    bar: n <Missing1> [ ^n ]\n]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("03_bar.mst"),
        "Object subclass: Bar [\n    baz: n <Missing2> [ ^n ]\n]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("world.list"),
        "01_object.mst\n02_foo.mst\n03_bar.mst\n",
    )
    .unwrap();

    let (_, all) = run(&["typecheck", "--world", dir.to_str().unwrap()]);
    assert!(all.contains("2 finding"), "got: {all}");

    let (_, filtered) = run(&[
        "typecheck",
        "--world",
        dir.to_str().unwrap(),
        "--class",
        "Bar",
    ]);
    assert!(filtered.contains("1 finding"), "got: {filtered}");
    assert!(filtered.contains("Missing2"));
    assert!(!filtered.contains("Missing1"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cli_json_output_is_well_formed_per_finding() {
    let dir = temp_world_dir("cli_json");
    std::fs::write(dir.join("01_object.mst"), "nil subclass: Object [\n]\n").unwrap();
    std::fs::write(
        dir.join("02_foo.mst"),
        "Object subclass: Foo [\n    bar: n <Missing> [ ^n ]\n]\n",
    )
    .unwrap();
    std::fs::write(dir.join("world.list"), "01_object.mst\n02_foo.mst\n").unwrap();

    let (status, stdout) = run(&["typecheck", "--world", dir.to_str().unwrap(), "--json"]);
    assert_eq!(status, 0);
    let stdout = stdout.trim();
    assert!(stdout.starts_with('[') && stdout.ends_with(']'), "got: {stdout}");
    assert!(stdout.contains("\"kind\":\"UndeclaredTypeName\""), "got: {stdout}");

    std::fs::remove_dir_all(&dir).ok();
}
