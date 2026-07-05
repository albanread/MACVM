//! Benchmark smoke tests (`tests_s06.md` "Integration / golden tests"):
//! `world/bench/fib.mst` and `world/bench/sieve.mst` at reduced sizes so
//! CI stays fast — full-size fib(25)/fib(30)/sieve(10 iters) numbers are
//! recorded in `docs/PERF.md`, not gated here.

use std::path::PathBuf;
use std::process::Command;

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_macvm"))
}

fn world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("world")
}

fn run_script(name: &str, src: &str) -> (i32, String) {
    let dir = std::env::temp_dir().join(format!("macvm_bench_smoke_{}_{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("smoke.mst");
    std::fs::write(&script, src).unwrap();

    let out = Command::new(bin_path())
        .args([
            "run",
            script.to_str().unwrap(),
            "--world",
            world_dir().to_str().unwrap(),
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("spawn macvm");

    std::fs::remove_dir_all(&dir).ok();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// fib(15) = 610, at a size small enough to stay fast under a debug build.
#[test]
fn fib_smoke() {
    let src = "Number subclass: Integer [\n\
        fib [\n\
            self < 2 ifTrue: [ ^self ].\n\
            ^(self - 1) fib + (self - 2) fib\n\
        ]\n\
    ]\n\
    Transcript show: 15 fib printString.\n\
    Transcript cr.\n";
    let (status, stdout) = run_script("fib", src);
    assert_eq!(status, 0, "stdout: {stdout}");
    assert_eq!(stdout, "610\n");
}

/// The full benchmark runs 10 iterations purely for timing stability — the
/// prime count each iteration computes is the same regardless, so 1
/// iteration is enough to check correctness (1899 for size 8190).
#[test]
fn sieve_smoke() {
    let src = "Object subclass: Sieve [\n\
        Sieve class >> run [\n\
            | flags count k prime size |\n\
            size := 8190.\n\
            flags := Array new: size.\n\
            1 to: size do: [:x | flags at: x put: true ].\n\
            count := 0.\n\
            1 to: size do: [:i |\n\
                (flags at: i) ifTrue: [\n\
                    prime := i + i + 1.\n\
                    k := i + prime.\n\
                    [ k <= size ] whileTrue: [\n\
                        flags at: k put: false.\n\
                        k := k + prime ].\n\
                    count := count + 1 ] ].\n\
            ^count\n\
        ]\n\
    ]\n\
    Transcript show: Sieve run printString.\n\
    Transcript cr.\n";
    let (status, stdout) = run_script("sieve", src);
    assert_eq!(status, 0, "stdout: {stdout}");
    assert_eq!(stdout, "1899\n");
}

/// Runs `src` with an explicit `MACVM_JIT` mode and returns the parsed
/// `ms: NNN` self-timing line (the arith.mst bracketing convention).
fn run_timed(name: &str, src: &str, jit: &str) -> u64 {
    let dir = std::env::temp_dir().join(format!("macvm_bench_smoke_{}_{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("smoke.mst");
    std::fs::write(&script, src).unwrap();

    let out = Command::new(bin_path())
        .args([
            "run",
            script.to_str().unwrap(),
            "--world",
            world_dir().to_str().unwrap(),
        ])
        .env("MACVM_JIT", jit)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("spawn macvm");
    std::fs::remove_dir_all(&dir).ok();

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(
        out.status.code(),
        Some(0),
        "MACVM_JIT={jit} stdout: {stdout}"
    );
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("ms: "))
        .unwrap_or_else(|| panic!("no 'ms: ' line in MACVM_JIT={jit} output:\n{stdout}"))
        .trim()
        .parse()
        .expect("ms value parses")
}

/// The re-armed S10 perf tripwire (docs/PERF.md's own "FAILS <2x" rule),
/// now a standing test instead of a manually-run `bench-s10` recipe: that
/// recipe was never re-run after S10, which let the S12xS13
/// spill-all-at-safepoint interaction quietly regress arith from 135x to
/// 1.0x over four sprints, and let the recompilation by_key-orphaning bug
/// pin every compiled call site at interpreter speed. Compiled code that
/// fails to DOUBLE interpreter speed on a pure smi loop means the tier-1
/// backend has stopped earning its complexity -- fail loudly.
///
/// n is reduced from arith.mst's 5M so the debug-build interpreter side
/// stays CI-fast; the ratio is profile-independent (both sides run the
/// same binary). The callSumTo: indirection keeps all three invocations
/// on one send site, arith.mst's own warmup convention.
#[test]
fn arith_compiled_beats_interpreter_2x() {
    let src = "Object subclass: ArithSmoke [\n\
        ArithSmoke class >> sumTo: n [\n\
            | s |\n\
            s := 0.\n\
            1 to: n do: [:i | s := s + i].\n\
            ^s\n\
        ]\n\
        ArithSmoke class >> callSumTo: n [\n\
            ^self sumTo: n\n\
        ]\n\
        ArithSmoke class >> runTimed [\n\
            | startMs endMs result |\n\
            self callSumTo: 3.\n\
            self callSumTo: 3.\n\
            startMs := Smalltalk millisecondClock.\n\
            result := self callSumTo: 1000000.\n\
            endMs := Smalltalk millisecondClock.\n\
            Transcript show: 'ms: '.\n\
            Transcript show: (endMs - startMs) printString.\n\
            Transcript cr.\n\
            ^result\n\
        ]\n\
    ]\n\
    Transcript show: ArithSmoke runTimed printString.\n\
    Transcript cr.\n";

    let interp_ms = run_timed("tripwire_interp", src, "off");
    let compiled_ms = run_timed("tripwire_jit", src, "threshold=1");

    // Timer-resolution guard: the ratio is only meaningful if the
    // interpreted side registered real time (it does at n=1M in every
    // build profile; this catches a future n reduction going too far).
    assert!(
        interp_ms >= 50,
        "interpreted arith ran too fast to gate a ratio ({interp_ms}ms) -- raise n"
    );
    assert!(
        compiled_ms * 2 <= interp_ms,
        "PERF TRIPWIRE: compiled arith must be at least 2x the interpreter \
         (docs/PERF.md, tests_s10.md gate item 3) -- got compiled {compiled_ms}ms \
         vs interpreted {interp_ms}ms"
    );
}
