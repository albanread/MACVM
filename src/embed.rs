//! The embedding API (`docs/SPEC.md` §16.2, amendment A17): boot a
//! `VmState`, evaluate Smalltalk source, and route guest output through a
//! caller-supplied sink — the one library-consumable entry point besides
//! the CLI (`main.rs`). `gui/`'s worker thread (SPEC §16.1) is the first
//! real caller (S21 step 3).
//!
//! # Safety model (S21)
//!
//! A `VmHandle` MUST be driven from a dedicated thread the caller is
//! prepared to see disappear out from under it — `boot` arms
//! `FatalMode::ExitThread` (`runtime::vm_state`), so every guest-fatal
//! condition (`error:`, DNU, stack overflow, heap exhaustion, a genesis-time
//! mmap failure...) terminates only the calling thread
//! (`libc::pthread_exit`, which does not unwind — safe regardless of any
//! JIT-compiled frames on the native stack, sidestepping the
//! panic-through-hand-assembled-code hazard entirely rather than working
//! around it) instead of the whole process. `eval` additionally recovers a
//! genuine native fault outside the JIT code cache (`Alien`'s raw pointer
//! accessors, S20) as an ordinary `Err`, via `codecache::deopt_trap`'s
//! `sigsetjmp`/`siglongjmp` registry — see `eval`'s own doc for why that one
//! case does NOT terminate the thread.
//!
//! The caller must never call `.join()`/`.is_finished()` on that thread's
//! `JoinHandle` — a thread that exits via `pthread_exit` never completes
//! `JoinHandle`'s own bookkeeping (`std::thread`'s `Arc<Packet>` handshake
//! requires the spawned thread's normal-or-panicking completion path, which
//! `pthread_exit` skips), so `join`/`is_finished` panics/hangs. Detect death
//! via a crash-report message sent before termination instead; dropping an
//! unjoined handle afterward is safe.

use std::path::Path;

use crate::codecache::deopt_trap;
use crate::frontend::{self, CompileError};
use crate::oops::Oop;
use crate::runtime::{VmError, VmOptions, VmState};

pub use crate::runtime::vm_state::FatalMode;

/// Overrides how a guest-fatal condition (`error:`, DNU, stack overflow,
/// heap exhaustion) terminates on the CURRENT thread. [`VmHandle::boot`]/
/// [`VmHandle::boot_without_world`] set [`FatalMode::ExitThread`] (the GUI's
/// "kill the language thread, not the process" model, module doc). An
/// embedder that would rather a guest-fatal condition abort the whole
/// process — e.g. a test or a batch CLI runner, where a silently-dying
/// thread would just hang the harness — calls this with
/// [`FatalMode::ExitProcess`] AFTER booting (both settings are thread-local,
/// so this affects every `VmHandle` driven from this thread).
pub fn set_fatal_mode(mode: FatalMode) {
    crate::runtime::vm_state::set_fatal_mode(mode);
}

/// A running, embedded VM instance — owns its `VmState` (and, through it,
/// the whole heap, code cache, and loaded world) outright. See the module
/// doc for the thread-lifetime contract every method here assumes.
pub struct VmHandle {
    vm: VmState,
}

/// A guest-visible evaluation failure — never a Rust panic (module doc's
/// safety model). `Compile` covers lex/parse/codegen errors (`eval`'s
/// source didn't compile). `NativeFault` is a genuinely recovered SIGSEGV/
/// SIGBUS in ordinary (non-JIT) native code — reachable today only through
/// `Alien`'s raw pointer accessors (S20) — turned into an ordinary `Err`
/// rather than terminating the thread, because `eval`'s own call frame is
/// the recovery point (see `eval`'s body).
#[derive(Debug)]
pub enum GuestError {
    Compile(CompileError),
    NativeFault { sig: i32, pc: u64, far: u64 },
}

impl std::fmt::Display for GuestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuestError::Compile(e) => write!(f, "{e}"),
            GuestError::NativeFault { sig, pc, far } => write!(
                f,
                "native fault (signal {sig}) at pc=0x{pc:x} far=0x{far:x} — recovered, this eval aborted"
            ),
        }
    }
}

impl std::error::Error for GuestError {}

/// Where `Transcript show:`/`printOnStdout:` output goes (SPEC §16.2).
/// `Send` because the GUI's sink hands output across the worker-to-main-
/// thread channel.
pub trait TranscriptSink: Send {
    fn show(&mut self, text: &str);
}

/// Adapts a `TranscriptSink` into the plain `std::io::Write` that
/// `VmState::out` already expects — SPEC §16.2's sink trait is
/// guest-output-shaped (whole strings), `Write` is byte-shaped; this is the
/// one place that gap is bridged. Guest output is always valid UTF-8
/// (Smalltalk `String`s produced by `printString`/`displayString`), so a
/// lossy conversion here would only ever mask a pre-existing bug elsewhere —
/// hence `from_utf8` + `expect` rather than `from_utf8_lossy`.
struct SinkWriter(Box<dyn TranscriptSink>);

impl std::io::Write for SinkWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let text = std::str::from_utf8(buf).expect(
            "guest output must be valid UTF-8 (VmState::out only ever \
             receives printString/displayString bytes)",
        );
        self.0.show(text);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl VmHandle {
    /// Boots a fresh VM: arms `FatalMode::ExitThread` and this thread's
    /// foreign-fault handler (module doc), runs genesis, and loads
    /// `world_dir/world.list` — the same base image the CLI's `run`/`repl`
    /// subcommands load via `--world` (default `"world"`, `main.rs`'s
    /// `load_world_with_warning`). A missing `world.list` is not an error
    /// (matches `load_world_with_warning`'s own `Ok(false)` handling — the
    /// VM boots successfully with just genesis's built-in classes); a
    /// `world.list` that exists but fails to load (a real compile error)
    /// surfaces as `Err(VmError)`.
    ///
    /// Takes `world_dir` explicitly rather than hardcoding `"world"`
    /// internally (`docs/SPEC.md` §16.2's sketch shows `boot(opts)` alone) —
    /// deliberate: a hardcoded relative path would be untestable (exercising
    /// the `Err` path would need mutating the whole test process's cwd, a
    /// global, unsynchronized change unsafe under a parallel test runner)
    /// and would silently assume a launch-directory convention no embedder
    /// has actually agreed to. A future Browser bridge may need a different
    /// world source entirely (the `image_store` SQLite image, not a `.mst`
    /// file tree) — deferred, per `shiny-snacking-pine.md`'s own Deferred
    /// section; `gui/`'s Workspace-only caller (S21 step 3) just always
    /// passes `Path::new("world")`, the same effective default as today's
    /// CLI.
    ///
    /// MUST be called on the dedicated thread the caller is prepared to see
    /// terminate out from under it (module doc) — `set_fatal_mode` and the
    /// foreign-fault handler's sigaltstack are both thread-scoped, so
    /// calling `boot` on the wrong thread arms the wrong one.
    pub fn boot(opts: VmOptions, world_dir: &Path) -> Result<VmHandle, VmError> {
        set_fatal_mode(FatalMode::ExitThread);
        // Regardless of `opts.jit`: `VmState::with_options` only arms
        // PROBE's SIGSEGV/SIGBUS handler when the JIT is enabled (a pure
        // interpreter never emits a deopt trap), but an embedded VM needs
        // foreign-fault recovery either way — `docs/SPEC.md` §16.5 itself
        // requires the GUI's Browser accept path to run with
        // `MACVM_JIT=off`. See `arm_foreign_fault_handler`'s own doc.
        deopt_trap::arm_foreign_fault_handler();
        let mut vm = VmState::with_options(opts);
        if let Err(e) = frontend::world::load_world(&mut vm, world_dir) {
            return Err(VmError { msg: e.to_string() });
        }
        Ok(VmHandle { vm })
    }

    /// Boots a bare, genesis-only VM — the built-in classes exist (`Object`,
    /// `Behavior`, the immediates, …) but no `world/` library is loaded. Same
    /// thread-safety arming as [`boot`] (module doc). For an embedder that
    /// supplies the library some other way than a `.mst` file tree — notably
    /// loading it from the versioned image database class-by-class via
    /// `eval` (the GUI's "load the world from the database" path, S22): boot
    /// genesis-only, then replay each stored class definition in load order.
    /// Never fails via `Result` (genesis itself uses `fatal_exit` for a
    /// heap-reservation failure, like every other VM entry point).
    pub fn boot_without_world(opts: VmOptions) -> VmHandle {
        set_fatal_mode(FatalMode::ExitThread);
        deopt_trap::arm_foreign_fault_handler();
        VmHandle {
            vm: VmState::with_options(opts),
        }
    }

    /// Compiles `source` as a single top-level item (SPEC §16.2: "compile as
    /// a doit, S5 REPL machinery, run, answer printString") and, for a doit,
    /// evaluates it and answers its `printString` — the same logic
    /// `main.rs`'s REPL uses (`print_result`). A class definition has no
    /// result value; answers `""`, same as an empty/whitespace-only
    /// `source`.
    ///
    /// Never panics or exits the process/thread on a GUEST failure (a
    /// compile error, or a recovered native fault) — both become `Err`, per
    /// the module doc's safety model. A VM-fatal condition (guest `error:`,
    /// DNU, stack overflow, heap exhaustion...) still terminates the calling
    /// thread via `fatal_exit` — there is no Rust-level `Result` for those
    /// today (`shiny-snacking-pine.md`'s Context section: panic/
    /// `catch_unwind` was rejected as the mechanism because it cannot safely
    /// unwind through a JIT-compiled frame), so `eval` simply never returns
    /// in that case; the failure message was already written to the
    /// transcript sink first.
    #[allow(unsafe_code)]
    pub fn eval(&mut self, source: &str) -> Result<String, GuestError> {
        let slot = deopt_trap::claim_jmp_slot();
        // SAFETY: `sigsetjmp` is called directly, inline, at this exact call
        // site — its frame (this `eval` invocation) stays live for the whole
        // recovery window: control does not return to the caller until
        // either the guest code below completes (normally or with a compile
        // error) or a foreign fault `siglongjmp`s straight back to here.
        // Calling `sigsetjmp` through an intervening wrapper function that
        // itself returns before the fault happens is unsound (the S21
        // setjmp-into-a-returned-frame bug found and fixed in
        // `codecache::deopt_trap`) — see `deopt_trap::sigsetjmp`'s own doc.
        let rc = unsafe { deopt_trap::sigsetjmp(deopt_trap::jmp_buf_ptr(slot), 1) };
        if rc != 0 {
            let (sig, pc, far) = deopt_trap::take_last_crash_info()
                .expect("sigsetjmp returned nonzero without a recorded crash");
            return Err(GuestError::NativeFault { sig, pc, far });
        }

        let item = match frontend::parser::parse_one_top_item(source) {
            Ok(Some(item)) => item,
            Ok(None) => return Ok(String::new()),
            Err(e) => return Err(GuestError::Compile(e)),
        };
        match frontend::classdef::execute_top_item(&mut self.vm, item) {
            Ok(Some(result)) => Ok(print_result(&mut self.vm, result)),
            Ok(None) => Ok(String::new()),
            Err(e) => Err(GuestError::Compile(e)),
        }
    }

    /// Like [`eval`](Self::eval) but runs `source` purely for effect and does
    /// NOT compute the result's `printString`. Use this for loading class
    /// definitions and initialization doIts: computing a result's printString
    /// can itself invoke guest code that isn't ready yet during a boot (e.g.
    /// `Character value:` before `Character initTable` has populated its
    /// table), and a load doesn't want the printed value anyway. A `.mst`
    /// file load has the same property — it executes each top item and
    /// discards the value.
    #[allow(unsafe_code)]
    pub fn exec(&mut self, source: &str) -> Result<(), GuestError> {
        let slot = deopt_trap::claim_jmp_slot();
        // SAFETY: as `eval` — `sigsetjmp` inline at this call site, whose
        // frame stays live for the whole recovery window.
        let rc = unsafe { deopt_trap::sigsetjmp(deopt_trap::jmp_buf_ptr(slot), 1) };
        if rc != 0 {
            let (sig, pc, far) = deopt_trap::take_last_crash_info()
                .expect("sigsetjmp returned nonzero without a recorded crash");
            return Err(GuestError::NativeFault { sig, pc, far });
        }
        let item = match frontend::parser::parse_one_top_item(source) {
            Ok(Some(item)) => item,
            Ok(None) => return Ok(()),
            Err(e) => return Err(GuestError::Compile(e)),
        };
        frontend::classdef::execute_top_item(&mut self.vm, item).map_err(GuestError::Compile)?;
        Ok(())
    }

    /// Installs `sink` as where guest output (`Transcript show:`,
    /// `printOnStdout:`) goes from now on. Default is stdout
    /// (`VmState::with_options`) — an embedder calls this once, right after
    /// `boot`, before the first `eval`.
    pub fn set_transcript(&mut self, sink: Box<dyn TranscriptSink>) {
        self.vm.out = Box::new(SinkWriter(sink));
    }
}

/// `main.rs::print_result`'s exact logic (run `printString`, fall back to
/// the Rust formatter for a pre-S6 world), duplicated rather than shared
/// because `main.rs`'s copy is a private `fn` in a binary crate `embed.rs`
/// cannot depend on.
fn print_result(vm: &mut VmState, result: Oop) -> String {
    let klass = crate::runtime::lookup::klass_of(vm, result);
    let sel = vm.universe.intern(b"printString");
    if let Some(m) = crate::runtime::lookup::lookup(vm, klass, sel) {
        let s = crate::interpreter::run_method(vm, m, result, &[]);
        if let Some(b) = crate::oops::wrappers::ByteArrayOop::try_from(s) {
            let mut bytes = Vec::new();
            b.copy_bytes_out(&mut bytes);
            return String::from_utf8_lossy(&bytes).into_owned();
        }
    }
    crate::memory::print_oop(&vm.universe, result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::JitMode;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn boot_test_vm(jit: JitMode) -> VmHandle {
        VmHandle::boot(
            VmOptions {
                heap_mib: 64,
                jit,
                ..Default::default()
            },
            Path::new("world"),
        )
        .expect("boot against the real world/ directory must succeed")
    }

    #[test]
    fn eval_arithmetic_returns_printstring() {
        let mut vm = boot_test_vm(JitMode::Off);
        let result = vm.eval("3 + 4.").expect("3 + 4 must evaluate cleanly");
        assert_eq!(result, "7");
    }

    /// "the JIT MUST be supported" (the S21 directive this whole module
    /// exists to satisfy) — `boot`/`eval` place no restriction on
    /// `opts.jit` at all. `Threshold(1)` compiles on the very first call,
    /// exercising the compiled path immediately rather than needing a hot
    /// loop to cross a higher threshold.
    #[test]
    fn eval_works_with_jit_enabled_and_aggressive_threshold() {
        let mut vm = boot_test_vm(JitMode::Threshold(1));
        let result = vm.eval("6 * 7.").expect("6 * 7 must evaluate cleanly");
        assert_eq!(result, "42");
    }

    #[test]
    fn eval_compile_error_surfaces_as_err_not_panic() {
        let mut vm = boot_test_vm(JitMode::Off);
        // Same "two consecutive binary operators" shape as tests/it_cli.rs's
        // own run_compile_err ("a + + b.") — a proven-broken source in this
        // codebase's own conventions.
        let err = vm
            .eval("3 + + 4")
            .expect_err("malformed source must fail to compile");
        match err {
            GuestError::Compile(_) => {}
            other => panic!("expected GuestError::Compile, got {other:?}"),
        }
    }

    #[test]
    fn set_transcript_routes_transcript_show_to_the_sink() {
        struct VecSink(Arc<Mutex<Vec<String>>>);
        impl TranscriptSink for VecSink {
            fn show(&mut self, text: &str) {
                self.0.lock().unwrap().push(text.to_string());
            }
        }

        let mut vm = boot_test_vm(JitMode::Off);
        let captured = Arc::new(Mutex::new(Vec::new()));
        vm.set_transcript(Box::new(VecSink(captured.clone())));
        vm.eval("Transcript show: 'hello from embed'.")
            .expect("Transcript show: must evaluate cleanly");
        let lines = captured.lock().unwrap();
        assert!(
            lines.iter().any(|l| l.contains("hello from embed")),
            "sink must have captured the Transcript show: text, got {lines:?}"
        );
    }

    #[test]
    fn boot_surfaces_a_bad_world_list_entry_as_err_not_a_process_exit() {
        let dir =
            std::env::temp_dir().join(format!("macvm_embed_bad_world_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("world.list"), "nonexistent_file.mst\n").unwrap();

        let result = VmHandle::boot(
            VmOptions {
                heap_mib: 64,
                jit: JitMode::Off,
                ..Default::default()
            },
            &dir,
        );
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            result.is_err(),
            "a world.list referencing a nonexistent file must fail boot(), not panic/exit"
        );
    }

    #[test]
    fn boot_with_no_world_list_at_all_still_succeeds() {
        let dir = std::env::temp_dir().join(format!("macvm_embed_no_world_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Deliberately no world.list written — matches load_world's own
        // Ok(false) "no world.list found" case, not an error.
        let result = VmHandle::boot(
            VmOptions {
                heap_mib: 64,
                jit: JitMode::Off,
                ..Default::default()
            },
            &dir,
        );
        std::fs::remove_dir_all(&dir).ok();
        assert!(result.is_ok(), "a missing world.list must not fail boot()");
    }

    /// The test that actually matters for the whole S21 safety model: a
    /// guest-fatal condition (here, an unhandled DNU — the base world's own
    /// `Object>>doesNotUnderstand:` routes to the `error:` primitive, one of
    /// the 8 `fatal_exit`-converted sites, S21 step 1) must terminate ONLY
    /// the worker thread `boot`/`eval` ran on, never the test process
    /// itself. Per Step 1a's validated finding, `.join()`/`.is_finished()`
    /// on a `pthread_exit`-terminated thread's `JoinHandle` panics/hangs —
    /// so this test (like the real GUI supervisor, S21 step 3) never calls
    /// either. It proves the thread died by a channel message NEVER
    /// arriving within a generous timeout (the sending half, moved into the
    /// crashing closure, is never dropped either — `pthread_exit` runs no
    /// `Drop` glue at all — so a real disconnect would never show up as
    /// `RecvTimeoutError::Disconnected`; a plain `Timeout` is the correct,
    /// only-possible signature of "that thread is gone"), then simply keeps
    /// running: the surrounding test binary process surviving to report a
    /// normal pass IS the proof the whole mechanism works.
    #[test]
    fn eval_fatal_condition_kills_only_the_worker_thread() {
        let (tx, rx) = mpsc::channel::<&'static str>();
        let handle = std::thread::spawn(move || {
            let mut vm = boot_test_vm(JitMode::Off);
            tx.send("reached-pre-crash-checkpoint").unwrap();
            // Object>>doesNotUnderstand: -> self error: '...' -> fatal_exit.
            // Never returns if FatalMode::ExitThread correctly pthread_exits.
            let _ = vm.eval("3 thisSelectorDoesNotExistAnywhereInTheBaseWorld.");
            // Only reachable if the thread survived the "fatal" condition —
            // itself exactly the bug this test exists to catch.
            tx.send("UNREACHABLE-thread-survived-a-fatal-condition")
                .unwrap();
        });

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok("reached-pre-crash-checkpoint"),
            "worker thread must have booted and reached the crash trigger"
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "worker thread must NOT have returned from a fatal eval — \
             it should have pthread_exit'd"
        );

        // Deliberately no handle.join()/is_finished() — see this test's own
        // doc comment and the module doc for why that would panic/hang on a
        // thread that called pthread_exit.
        drop(handle);

        // The strongest proof of all: this test process is still alive and
        // able to run more assertions after the fact.
        assert_eq!(2 + 2, 4);
    }
}
