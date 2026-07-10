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

/// DBG5 (docs/compiled_send_auditor_design.md §D1): the FORCE-COLD
/// compiled-send auditor. `MACVM_TRACE=calls` routes every compiled send
/// through `rt_resolve_send` (never memoizing the IC site) and logs one
/// `[calls]` line each. Runs a subprocess with env vars only (no stdin
/// script), mirroring `it_tier1.rs`'s `compile_disabled_churn`.
fn run_env(program: &str, jit: &str, trace: &str) -> (i32, String, String) {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let exe = env!("CARGO_BIN_EXE_macvm");
    let dir = std::env::temp_dir().join(format!("macvm_dbg5_{}_{seq}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("prog.mst");
    std::fs::write(&file, program).unwrap();
    let out = Command::new(exe)
        .arg("run")
        .arg(&file)
        .arg("--world")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/world"))
        .env("MACVM_JIT", jit)
        .env("MACVM_TRACE", trace)
        .env("MACVM_PROBE", "off")
        .env_remove("MACVM_GC_STRESS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn macvm");
    let _ = std::fs::remove_dir_all(&dir);
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A polymorphic `#speak` send (Dog/Cat/Animal at one site) that the compiler
/// CANNOT devirtualize into a single inline — so it survives as a real
/// compiled send that the auditor observes. Trivial self-sends inline away and
/// would leave nothing to trace (the whole point of the poly shape).
const AUDITOR_POLY: &str = r#"
Object subclass: Animal [ speak [ ^1 ] ]
Animal subclass: Dog [ speak [ ^2 ] ]
Animal subclass: Cat [ speak [ ^3 ] ]
Object subclass: Zoo [
    total: coll [ | s | s := 0. coll do: [:a | s := s + a speak ]. ^s ]
    run: n [ | coll s | coll := OrderedCollection new.
        coll add: Dog new; add: Cat new; add: Animal new.
        s := 0. 1 to: n do: [:i | s := s + (self total: coll) ]. ^s ]
]
Zoo2 := Zoo new.
Transcript show: 'sum='; show: (Zoo2 run: 500) printString; cr.
"#;

#[test]
fn auditor_calls_traces_compiled_sends_without_changing_result() {
    // Reference answer with the auditor OFF (realistic threshold=200).
    let (code_off, stdout_off, stderr_off) = run_env(AUDITOR_POLY, "threshold=200", "");
    assert_eq!(code_off, 0, "off stderr: {stderr_off}");
    assert!(
        stdout_off.contains("sum=3000"),
        "500*(2+3+1)=3000, off stdout: {stdout_off}"
    );

    // Auditor ON: force-cold is observation-only, so the guest's answer is
    // byte-identical, AND the polymorphic #speak send shows up with each
    // real receiver klass.
    let (code, stdout, stderr) = run_env(AUDITOR_POLY, "threshold=200", "calls");
    assert_eq!(code, 0, "calls stderr tail: {}", tail(&stderr));
    assert_eq!(
        stdout, stdout_off,
        "the auditor must not change the guest's answer (force-cold is observation-only)"
    );
    for klass in ["#Dog", "#Cat", "#Animal"] {
        assert!(
            stderr.lines().any(|l| l.starts_with("[calls]")
                && l.contains("#speak")
                && l.contains(&format!("recv={klass}"))),
            "expected a compiled-send trace of #speak to {klass}; [calls] sample:\n{}",
            stderr
                .lines()
                .filter(|l| l.starts_with("[calls]") && l.contains("#speak"))
                .take(6)
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

fn tail(s: &str) -> String {
    s.lines().rev().take(12).collect::<Vec<_>>().join("\n")
}

/// DBG5 §D3: the interactive step-call auditor. `MACVM_STEP_CALLS=1` stops at
/// each compiled send boundary and services read-only inspection commands.
/// Scripted stdin (commands in, transcript on stderr), same discipline as the
/// HALT session goldens above — but at a COMPILED send, showing the compiled
/// frame + its oop slots, never deopting.
fn run_step_calls(program: &str, script: &str) -> (i32, String, String) {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let exe = env!("CARGO_BIN_EXE_macvm");
    let dir = std::env::temp_dir().join(format!("macvm_dbg5sc_{}_{seq}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("prog.mst");
    std::fs::write(&file, program).unwrap();
    let mut child = Command::new(exe)
        .arg("run")
        .arg(&file)
        .arg("--world")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/world"))
        .env("MACVM_STEP_CALLS", "1")
        .env("MACVM_JIT", "threshold=200")
        .env("MACVM_PROBE", "off")
        .env_remove("MACVM_GC_STRESS")
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

#[test]
fn step_call_stops_at_compiled_send_and_inspects_without_changing_result() {
    // bt at the first send, dump the compiled frame's slots, advance one send,
    // then run to completion.
    let (code, stdout, stderr) = run_step_calls(AUDITOR_POLY, "bt\nslots\nstep\ncontinue\n");
    assert_eq!(code, 0, "stderr tail: {}", tail(&stderr));
    // Result is unchanged — the auditor is a pure observer at the send boundary.
    assert!(stdout.contains("sum=3000"), "stdout: {stdout}");
    // Stopped at a compiled #speak send boundary.
    assert!(
        stderr.contains("▸ send #speak"),
        "expected a send-boundary prompt; stderr tail:\n{}",
        tail(&stderr)
    );
    // `bt` shows the compiled callee over its interpreted callers (mixed-tier).
    assert!(
        stderr.contains("[compiled] Zoo>>") && stderr.contains("[interp]   Zoo>>total:"),
        "bt must show the mixed-tier stack; stderr tail:\n{}",
        tail(&stderr)
    );
    // `slots` dumps the live compiled frame's oop slots (all healthy here).
    assert!(
        stderr.contains("[oops] nm=") && stderr.contains("mark:ok"),
        "slots must dump the frame's oop slots; stderr tail:\n{}",
        tail(&stderr)
    );
    // `step` advanced to a second send before `continue` finished the run.
    assert!(
        stderr.matches("▸ send #speak").count() >= 2,
        "step must advance to the next send; stderr:\n{stderr}"
    );
}
