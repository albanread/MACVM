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
