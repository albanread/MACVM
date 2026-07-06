//! DBG1/DBG2 gate (docs/DEBUGGER.md §6): scripted debugger sessions driven
//! through real subprocesses — commands in on stdin, transcript asserted on
//! stderr (the halt loop's channel; stdout stays the guest program's own).

use std::io::Write;
use std::process::{Command, Stdio};

fn scripted_session(env_debug: &str, script: &str, program: &str) -> (i32, String, String) {
    // Unique per CALL, not per process — the test harness runs these in
    // parallel threads within one process.
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let exe = env!("CARGO_BIN_EXE_macvm");
    let dir = std::env::temp_dir().join(format!("macvm_dbg_{}_{seq}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("prog.mst");
    std::fs::write(&file, program).unwrap();
    let mut child = Command::new(exe)
        .arg("run")
        .arg(&file)
        .arg("--world")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/world"))
        .env("MACVM_DEBUG", env_debug)
        .env("MACVM_PROBE", "off")
        .env_remove("MACVM_GC_STRESS")
        .env_remove("MACVM_JIT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn macvm");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait macvm");
    let _ = std::fs::remove_dir_all(&dir);
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const COUNTER: &str = r#"
Object subclass: DbgCounter [
    | count |
    init [ count := 0 ]
    bump: n [ | doubled | doubled := n * 2. count := count + doubled. ^count ]
    count [ ^count ]
]
C := DbgCounter new.
C init.
C bump: 5.
C bump: 7.
Transcript show: C count printString; cr.
"#;

#[test]
fn breakpoint_halts_and_program_completes_correctly() {
    let (code, stdout, stderr) = scripted_session(
        "break:DbgCounter>>bump:@0",
        "bt\ntemps\ncontinue\ncontinue\n",
        COUNTER,
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    // Deferred set + two hits.
    assert!(
        stderr.contains("breakpoint set: DbgCounter>>bump: @0 (deferred)"),
        "stderr: {stderr}"
    );
    assert_eq!(
        stderr
            .matches("halt: breakpoint DbgCounter>>bump: @0")
            .count(),
        2,
        "stderr: {stderr}"
    );
    // bt shows the halted frame; temps shows the arg.
    assert!(
        stderr.contains("[interp]   DbgCounter>>bump:"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("arg [0] = 5"), "stderr: {stderr}");
    // The debugger is a pure observer: the program's answer is untouched.
    assert!(stdout.contains("24"), "stdout: {stdout}");
}

#[test]
fn print_evaluates_against_receiver_ivars() {
    let (code, stdout, stderr) = scripted_session(
        "break:DbgCounter>>bump:@0",
        "print count\ncontinue\nprint count\ncontinue\n",
        COUNTER,
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    // First hit: count = 0; second hit: count = 10 (after bump: 5).
    assert!(stderr.contains("(halt) 0"), "stderr: {stderr}");
    assert!(stderr.contains("(halt) 10"), "stderr: {stderr}");
    assert!(stdout.contains("24"), "stdout: {stdout}");
}

#[test]
fn stepping_advances_bci_and_finish_returns() {
    let (code, stdout, stderr) = scripted_session(
        "break:DbgCounter>>bump:@0",
        "step\nstep\nfinish\ncontinue\ncontinue\n",
        COUNTER,
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    // Two Into steps advance within bump:.
    assert!(
        stderr.contains("halt: step DbgCounter>>bump: @"),
        "stderr: {stderr}"
    );
    // finish lands back in the caller (the doIt).
    assert!(stderr.contains("halt: step ?>>doIt @"), "stderr: {stderr}");
    assert!(stdout.contains("24"), "stdout: {stdout}");
}

#[test]
fn eof_means_continue_and_never_hangs() {
    // No commands at all: every halt sees EOF and resumes. The run must
    // complete with the right answer.
    let (code, stdout, stderr) = scripted_session("break:DbgCounter>>bump:@0", "", COUNTER);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("24"), "stdout: {stdout}");
    assert!(stderr.contains("(eof — continuing)"), "stderr: {stderr}");
}

#[test]
fn guest_error_halts_when_debug_active_then_still_exits_fatally() {
    let program = r#"
Object subclass: DbgErr [
    boom [ self error: 'planted' ]
]
E := DbgErr new.
E boom.
"#;
    let (code, _stdout, stderr) = scripted_session("1", "bt\ncontinue\n", program);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(
        stderr.contains("halt: Error: planted"),
        "the error must open a halt for inspection: {stderr}"
    );
    assert!(
        stderr.contains("DbgErr>>boom"),
        "bt at the error must show the erring frame: {stderr}"
    );
}

#[test]
fn bad_bci_rejected_at_set_time() {
    // bci 1 of bump: is mid-instruction (push_temp is 2 bytes) — the spec
    // must be rejected loudly, not become a silent never-hit.
    let (code, stdout, stderr) = scripted_session("break:DbgCounter>>bump:@1", "", COUNTER);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stderr.contains("not an instruction boundary"),
        "stderr: {stderr}"
    );
    assert!(stdout.contains("24"), "stdout: {stdout}");
}
