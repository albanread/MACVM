//! The cargo gate for the in-image test suite (`docs/sunit_design.md` S1).
//!
//! `world/tests/` is already gated — `tests/it_world.rs` loads it and runs all
//! 549 library tests. The `tests` PACKAGE (`world/tests.list`) is not: it ships
//! inside the image, and its suites run through `macvm-gui test`. Without this
//! file they run only when somebody remembers to type that, which is the wrong
//! way round for the framework everything else is measured with.
//!
//! So this drives the REAL binary and pins the contract the command line, the
//! Tests tab and CI all depend on: green exits 0, red exits 1, and a failure
//! names the test that failed. It deliberately does not re-test SUnit's
//! semantics — `world/t10_sunit_tests.mst` does that in-language, against a
//! nested runner, which is the only place it can be done honestly.

use std::path::PathBuf;
use std::process::Command;

fn world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../world")
}

/// Runs `macvm-gui test` with extra args, answering (exit code, stdout).
fn run_test_cmd(extra: &[&str]) -> (i32, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_macvm-gui"));
    cmd.arg("test").arg("--world").arg(world_dir());
    for a in extra {
        cmd.arg(a);
    }
    let out = cmd.output().expect("macvm-gui test must run");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Writes a one-file suite plus its list into a fresh temp directory and
/// answers the list's absolute path. `cmd_test` joins `--list` onto the world
/// directory, and joining an ABSOLUTE path replaces the base — and
/// `world::load_list` resolves entries against the list file's own directory —
/// so a suite outside the repo loads cleanly without polluting `world/`.
fn temp_suite(tag: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("macvm_sunit_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the temp suite dir");
    std::fs::write(dir.join("suite.mst"), source).expect("write the suite");
    let list = dir.join("suite.list");
    std::fs::write(&list, "suite.mst\n").expect("write the list");
    list
}

#[test]
fn the_shipped_suite_is_green_and_exits_zero() {
    let (code, out) = run_test_cmd(&[]);
    assert_eq!(code, 0, "the shipped suite must be green; output:\n{out}");
    let summary = out
        .lines()
        .find(|l| l.contains(" run, ") && l.contains(" errors, "))
        .unwrap_or_else(|| panic!("no summary line in:\n{out}"));
    assert!(
        summary.contains(", 0 failures, 0 errors,"),
        "expected a green summary, got: {summary}"
    );
    // A runner that finds nothing also reports zero failures. Insist it
    // actually ran something, or this test passes forever on an empty suite.
    let run_count: u64 = summary
        .split(' ')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("couldn't parse the run count from: {summary}"));
    assert!(run_count > 0, "the suite must run at least one test");
}

#[test]
fn a_failing_assertion_fails_the_command_and_names_the_test() {
    // The red half of red-then-green. Without this the gate cannot tell a
    // passing suite from a runner that never reports anything.
    let list = temp_suite(
        "red",
        "TestCase subclass: BridgeRedTest [\n\
             testThisOneFails [ self assert: 1 equals: 2 ]\n\
         ]\n",
    );
    let (code, out) = run_test_cmd(&["--list", list.to_str().expect("utf-8 temp path")]);
    assert_eq!(
        code, 1,
        "a failing assertion must exit non-zero; got:\n{out}"
    );
    assert!(
        out.contains("testThisOneFails"),
        "the failure must name the test that failed, got:\n{out}"
    );
    assert!(
        out.contains("expected 2 but was 1"),
        "assert:equals: must say what it expected, got:\n{out}"
    );
}

#[test]
fn an_unexpected_error_is_reported_as_an_error_not_a_failure() {
    // The distinction the whole framework turns on, checked at the CLI
    // boundary: a broken assertion and a broken TEST must not look alike to
    // anything downstream.
    let list = temp_suite(
        "err",
        "TestCase subclass: BridgeErrTest [\n\
             testThisOneExplodes [ nil frobnicateWildly ]\n\
         ]\n",
    );
    let (code, out) = run_test_cmd(&["--list", list.to_str().expect("utf-8 temp path")]);
    assert_eq!(code, 1, "an erroring test must exit non-zero; got:\n{out}");
    assert!(
        out.contains("ERROR BridgeErrTest testThisOneExplodes"),
        "must be reported as an ERROR, got:\n{out}"
    );
    assert!(
        out.contains(", 0 failures, 1 error,"),
        "the summary must count it as an error, not a failure, got:\n{out}"
    );
}

#[test]
fn the_suite_survives_its_own_warm_up() {
    // `--repeat 2` runs everything a second time in the SAME image, which is
    // the first run whose methods are JIT-compiled. That is not padding: it is
    // how the `super new` miscompile was caught (06d2176), where run 1 was
    // green and run 2 reported `nil does not understand add:` because
    // `TestResult`'s instance variables were never initialized.
    let (code, out) = run_test_cmd(&["--repeat", "2"]);
    assert_eq!(code, 0, "the suite must survive a second, warm run:\n{out}");
    let greens = out
        .lines()
        .filter(|l| l.contains(", 0 failures, 0 errors,"))
        .count();
    assert_eq!(greens, 2, "expected two green summaries, got:\n{out}");
}
