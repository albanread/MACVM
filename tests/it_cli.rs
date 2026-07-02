//! CLI integration tests (`sprint_s05_detail.md` §Design "CLI",
//! `tests_s05.md` §Integration/golden "CLI tests"). Drives the built
//! `macvm` binary as a real subprocess with piped stdin/stdout — the only
//! way to observe another process's own exit code and exhaustive stderr.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn bin_path() -> PathBuf {
    // Cargo sets this for integration tests; points at target/{debug,release}.
    PathBuf::from(env!("CARGO_BIN_EXE_macvm"))
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

struct Output {
    status: i32,
    stdout: String,
    stderr: String,
}

fn run(args: &[&str]) -> Output {
    let out = Command::new(bin_path())
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("spawn macvm");
    Output {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn run_repl(input: &str) -> Output {
    let mut child = Command::new(bin_path())
        .arg("repl")
        .arg("--world")
        .arg(golden_dir().join("mini_world"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn macvm repl");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait for repl");
    Output {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[test]
fn run_ok() {
    let out = run(&[
        "run",
        "tests/golden/point_demo.mst",
        "--world",
        "tests/golden/nonexistent_world",
    ]);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    let expected =
        std::fs::read_to_string(golden_dir().join("point_demo.expected")).expect("read golden");
    assert_eq!(out.stdout, expected);
}

/// sprint_s06_detail.md step 6: "point_demo.mst now prints via
/// printString" — a separate fixture from `point_demo.mst` (which must
/// stay runnable with NO world at all, see `world_missing`) exercised
/// against the real `world/world.list`.
#[test]
fn run_ok_real_world_printstring() {
    let out = run(&[
        "run",
        "tests/golden/point_demo_real_world.mst",
        "--world",
        "world",
    ]);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    let expected = std::fs::read_to_string(golden_dir().join("point_demo_real_world.expected"))
        .expect("read golden");
    assert_eq!(out.stdout, expected);
}

#[test]
fn run_compile_err() {
    let dir = std::env::temp_dir().join(format!("macvm_cli_err_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("bad.mst");
    std::fs::write(&file, "a + + b.\n").unwrap();

    let out = run(&[
        "run",
        file.to_str().unwrap(),
        "--world",
        "tests/golden/nonexistent_world",
    ]);
    assert_ne!(out.status, 0);
    assert!(
        out.stderr.contains("1:") || out.stderr.contains(":1:"),
        "expected a line:col in stderr, got: {}",
        out.stderr
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn run_missing_file() {
    let out = run(&[
        "run",
        "tests/golden/does_not_exist.mst",
        "--world",
        "tests/golden/nonexistent_world",
    ]);
    assert_ne!(out.status, 0);
    assert!(!out.stderr.is_empty());
}

#[test]
fn world_override() {
    let dir = std::env::temp_dir().join(format!("macvm_cli_world_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mini.mst"),
        "Object subclass: MiniWorldMarker [\n\
         \x20   MiniWorldMarker class >> new [ <primitive: 23> ^self ]\n\
         \x20   ping: aString [ <primitive: 91> ^self ]\n\
         ]\n",
    )
    .unwrap();
    std::fs::write(dir.join("world.list"), "mini.mst\n").unwrap();
    let script = dir.join("use.mst");
    std::fs::write(&script, "MiniWorldMarker new ping: 'seen'.\n").unwrap();

    let out = run(&[
        "run",
        script.to_str().unwrap(),
        "--world",
        dir.to_str().unwrap(),
    ]);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout, "seen");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn world_missing() {
    let out = run(&[
        "run",
        "tests/golden/point_demo.mst",
        "--world",
        "tests/golden/nonexistent_world",
    ]);
    assert!(out.stderr.contains("no world.list"));
    assert_eq!(out.status, 0);
}

#[test]
fn repl_arith() {
    let out = run_repl("3 + 4.\n");
    assert!(out.stdout.contains('7'), "stdout: {}", out.stdout);
}

#[test]
fn repl_continuation() {
    let out = run_repl("[:x |\nx] value: 5.\n");
    assert!(out.stdout.contains('5'), "stdout: {}", out.stdout);
}

#[test]
fn repl_error_reset() {
    let out = run_repl("a + + b.\n1 + 1.\n");
    assert!(out.stdout.contains('2'), "stdout: {}", out.stdout);
}

#[test]
fn repl_global() {
    let out = run_repl("Foo := 3.\nFoo * 2.\n");
    assert!(out.stdout.contains('6'), "stdout: {}", out.stdout);
}

#[test]
fn repl_pre_s6_print() {
    // No `printString` is understood by a plain object in v1 — the
    // print_oop fallback must still produce SOME line, not crash/hang.
    let out = run_repl("3.\n");
    assert!(out.stdout.contains('3'), "stdout: {}", out.stdout);
}
