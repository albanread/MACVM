//! DBG0 gate (docs/DEBUGGER.md §6): the PROBE crash dossier, driven
//! through real subprocesses — the only way to observe another process's
//! exit code, death-by-signal status, and exhaustive stderr (`main.rs`'s
//! own selftest rationale). Every assertion targets the dossier MARKER +
//! section shape, never the bare exit code: exit(70) is shared with heap
//! exhaustion and process-stack overflow.

use std::process::Command;

fn run_selftest(flag: &str, extra_env: &[(&str, &str)]) -> (Option<i32>, Option<i32>, String) {
    let exe = env!("CARGO_BIN_EXE_macvm");
    let mut cmd = Command::new(exe);
    cmd.arg(flag);
    // Inherited stress/config vars can invalidate a selftest's premise
    // (the it_memory.rs precedent).
    cmd.env_remove("MACVM_GC_STRESS");
    cmd.env_remove("MACVM_JIT");
    cmd.env_remove("MACVM_PROBE");
    cmd.env_remove("MACVM_PROBE_DUMP");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn macvm");
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        out.status.signal()
    };
    (
        out.status.code(),
        signal,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn probe_assert_emits_full_dossier_and_exits_70() {
    let (code, _sig, stderr) = run_selftest("--selftest-probe-assert", &[]);
    assert_eq!(code, Some(70), "stderr: {stderr}");
    assert!(
        stderr.contains("==== MACVM PROBE DOSSIER v1 ===="),
        "missing dossier marker: {stderr}"
    );
    assert!(stderr.contains("trigger: brk #0xDE02"), "stderr: {stderr}");
    // Section shape: verdict, registers, walkback, heap verify, ring, end.
    for section in [
        "[1] verdict:",
        "[3] x0 ",
        "[6] native walk:",
        "[7] heap verify:",
        "[9] recent history:",
        "==== END DOSSIER (exit 70) ====",
    ] {
        assert!(stderr.contains(section), "missing {section:?} in: {stderr}");
    }
    // The receiver (nil) travels in x0 through the call stub — the register
    // annotator must see a plausible heap oop, proving the capture is the
    // REAL register file, not zeros.
    assert!(
        stderr.contains("[3] x0 ") && stderr.contains("mark:ok"),
        "x0 should annotate as a plausible mem-oop: {stderr}"
    );
}

#[test]
fn probe_segv_in_cache_emits_dossier_and_exits_70() {
    let (code, _sig, stderr) = run_selftest("--selftest-probe-segv", &[]);
    assert_eq!(code, Some(70), "stderr: {stderr}");
    assert!(
        stderr.contains("trigger: SIGSEGV (pc in code cache)"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("==== END DOSSIER (exit 70) ===="),
        "dossier must run to completion: {stderr}"
    );
}

#[test]
fn probe_foreign_fault_prints_verdict_and_dies_by_signal() {
    let (code, signal, stderr) = run_selftest("--selftest-probe-foreign", &[]);
    // Killed by the re-raised default disposition, NOT a dossier exit.
    assert_ne!(
        code,
        Some(70),
        "foreign crashes must not fake a dossier exit"
    );
    assert_eq!(signal, Some(libc_sigsegv()), "stderr: {stderr}");
    assert!(
        stderr.contains("FOREIGN (not in any code cache)"),
        "missing foreign verdict: {stderr}"
    );
    assert!(
        !stderr.contains("==== MACVM PROBE DOSSIER"),
        "foreign crashes must not emit a dossier (x28 untrustworthy): {stderr}"
    );
}

#[test]
fn probe_json_dump_written_with_pinned_schema() {
    let dir = std::env::temp_dir().join(format!("macvm_probe_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dossier.json");
    let (code, _sig, stderr) = run_selftest(
        "--selftest-probe-assert",
        &[("MACVM_PROBE_DUMP", path.to_str().unwrap())],
    );
    assert_eq!(code, Some(70), "stderr: {stderr}");
    let json = std::fs::read_to_string(&path).expect("json dossier written");
    assert!(
        json.starts_with("{\"schema\": 1"),
        "schema field must be pinned first: {json}"
    );
    assert!(json.contains("\"verdict\""), "json: {json}");
    assert!(json.contains("\"registers\""), "json: {json}");
    let _ = std::fs::remove_dir_all(&dir);
}

fn libc_sigsegv() -> i32 {
    11
}
