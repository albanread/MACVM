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

/// Releases this thread's `sigsetjmp` recovery slot when the handle is
/// dropped. `eval`/`exec` claim one via `deopt_trap::claim_jmp_slot`
/// (idempotent per thread); in the embedded model the worker thread owns its
/// `VmHandle` and drops it on its own way out — a clean `worker_loop` return,
/// the common restart-on-death path where an idle worker exits as its request
/// channel is dropped — so `deregister_setjmp` runs on the very thread that
/// claimed the slot and frees it. Without this, every respawn would strand a
/// slot owned by a now-dead `pthread_t`, overflowing the fixed-size registry
/// after `JMP_REGISTRY_CAP` restarts. `deregister_setjmp` is keyed by
/// `pthread_self()`, so a `VmHandle` dropped on a thread that never claimed a
/// slot is simply a no-op there — safe on any thread. (A worker torn down via
/// `pthread_exit` — a genuinely fatal, unrecovered condition — skips `Drop`
/// by design; that path is rare now that DNU/`error:` recover in-thread, and
/// its slot is reclaimed if that `pthread_t` value is ever reused.)
impl Drop for VmHandle {
    fn drop(&mut self) {
        crate::codecache::deopt_trap::deregister_setjmp();
    }
}

/// A guest-visible evaluation failure — never a Rust panic (module doc's
/// safety model). `Compile` covers lex/parse/codegen errors (`eval`'s
/// source didn't compile). `RuntimeError` is an unhandled DNU or explicit
/// `self error:` — genuinely terminal for the CURRENT computation in
/// Smalltalk's own terms (no proceed semantics in v1), but NOT a sign the
/// VM itself is broken, so it's recovered at this same boundary rather than
/// tearing down the whole worker thread the way it did before this existed
/// (`runtime::error::dnu_fallback`/`primitives::prim_error`, via
/// `codecache::deopt_trap::raise_guest_fatal`) — this is what makes an
/// everyday Workspace typo an ordinary recoverable error instead of a full
/// VM respawn, matching real Smalltalk's own recoverable
/// `doesNotUnderstand:`. `NativeFault` is a genuinely recovered SIGSEGV/
/// SIGBUS in ordinary (non-JIT) native code — reachable today only through
/// `Alien`'s raw pointer accessors (S20) — turned into an ordinary `Err`
/// rather than terminating the thread, because `eval`'s own call frame is
/// the recovery point (see `eval`'s body).
#[derive(Debug)]
pub enum GuestError {
    Compile(CompileError),
    RuntimeError(String),
    NativeFault { sig: i32, pc: u64, far: u64 },
}

impl std::fmt::Display for GuestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuestError::Compile(e) => write!(f, "{e}"),
            GuestError::RuntimeError(msg) => write!(f, "{msg}"),
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
    /// compile error, an unhandled DNU/`error:`, or a recovered native
    /// fault) — all three become `Err`, per the module doc's safety model.
    /// A truly VM-fatal condition (stack overflow, heap exhaustion — the
    /// VM's OWN invariants/resources, not the guest program's correctness)
    /// still terminates the calling thread via `fatal_exit`: there is no
    /// Rust-level `Result` for those, and shouldn't be — a full worker
    /// respawn is the right response when the VM itself may be compromised,
    /// unlike an ordinary DNU (`shiny-snacking-pine.md`'s Context section:
    /// panic/`catch_unwind` was rejected as the *general* unwind mechanism
    /// here because it cannot safely cross a JIT-compiled frame — DNU/
    /// `error:` recovery below reuses `sigsetjmp`/`siglongjmp` instead,
    /// precisely because that mechanism doesn't do Rust-style unwinding at
    /// all and is already trusted through JIT frames for the native-fault
    /// case). `eval` simply never returns for a genuinely VM-fatal
    /// condition; the failure message was already written to the
    /// transcript sink first.
    #[allow(unsafe_code)]
    pub fn eval(&mut self, source: &str) -> Result<String, GuestError> {
        let slot = deopt_trap::claim_jmp_slot();
        // SAFETY: `sigsetjmp` is called directly, inline, at this exact call
        // site — its frame (this `eval` invocation) stays live for the whole
        // recovery window: control does not return to the caller until
        // either the guest code below completes (normally, with a compile
        // error, or with an unhandled DNU/`error:` recovered via
        // `deopt_trap::raise_guest_fatal`) or a foreign fault `siglongjmp`s
        // straight back to here. Calling `sigsetjmp` through an intervening
        // wrapper function that itself returns before the fault happens is
        // unsound (the S21 setjmp-into-a-returned-frame bug found and fixed
        // in `codecache::deopt_trap`) — see `deopt_trap::sigsetjmp`'s own
        // doc.
        let rc = unsafe { deopt_trap::sigsetjmp(deopt_trap::jmp_buf_ptr(slot), 1) };
        if rc == deopt_trap::GUEST_FATAL_JMP_VAL {
            let message = deopt_trap::take_last_guest_fatal_message().expect(
                "sigsetjmp returned GUEST_FATAL_JMP_VAL without a recorded guest-fatal message",
            );
            return Err(GuestError::RuntimeError(message));
        }
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

    /// Evaluates a `<smappl visual="...">` expression and returns the HTML
    /// fragment the image renders for it (GUI D-G5 / `docs/APPS.md` §6: the
    /// Visual renders *itself* to HTML; Rust only transports the string).
    /// `code` is the raw `visual=` source; this wraps it as
    /// `(Visual coerce: (<code>)) htmlFragment` — the exact
    /// `ElementSMAPPL.dlt` shape (`gui/smappl.md` §2) — and hands back the
    /// resulting `String`'s raw bytes.
    ///
    /// Unlike [`eval`](Self::eval) this does NOT run `printString` on the
    /// result: `htmlFragment` already answers a `String`, and printString
    /// would re-quote it. A non-`String` result (a widget shape whose
    /// `htmlFragment` isn't built yet, so the send DNUs, or `coerce:` let a
    /// non-Visual through) surfaces as `Err` and the caller shows the G0
    /// placeholder box — errors are swallowed to a fallback, never a broken
    /// page, matching `ElementSMAPPL`'s own `ifError:` discipline.
    #[allow(unsafe_code)]
    pub fn render_fragment(&mut self, code: &str) -> Result<String, GuestError> {
        let source = format!("(Visual coerce: ({code})) htmlFragment.");
        let slot = deopt_trap::claim_jmp_slot();
        // SAFETY: as `eval` — `sigsetjmp` inline at this call site, whose frame
        // stays live for the whole recovery window.
        let rc = unsafe { deopt_trap::sigsetjmp(deopt_trap::jmp_buf_ptr(slot), 1) };
        if rc == deopt_trap::GUEST_FATAL_JMP_VAL {
            let message = deopt_trap::take_last_guest_fatal_message().expect(
                "sigsetjmp returned GUEST_FATAL_JMP_VAL without a recorded guest-fatal message",
            );
            return Err(GuestError::RuntimeError(message));
        }
        if rc != 0 {
            let (sig, pc, far) = deopt_trap::take_last_crash_info()
                .expect("sigsetjmp returned nonzero without a recorded crash");
            return Err(GuestError::NativeFault { sig, pc, far });
        }
        let item = match frontend::parser::parse_one_top_item(&source) {
            Ok(Some(item)) => item,
            Ok(None) => return Ok(String::new()),
            Err(e) => return Err(GuestError::Compile(e)),
        };
        match frontend::classdef::execute_top_item(&mut self.vm, item) {
            Ok(Some(result)) => match fragment_bytes(result) {
                Some(html) => Ok(html),
                // The fragment method answered a non-String — treat as a
                // render failure so the caller falls back to the placeholder.
                None => Err(GuestError::RuntimeError(
                    "smappl visual did not render to a String".to_string(),
                )),
            },
            Ok(None) => Ok(String::new()),
            Err(e) => Err(GuestError::Compile(e)),
        }
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

/// Raw bytes of a guest `String`/`ByteArray` result, or `None` if `result`
/// isn't byte-indexable. Used by [`VmHandle::render_fragment`] to return an
/// HTML fragment verbatim, without the printString requoting `print_result`
/// would apply.
fn fragment_bytes(result: Oop) -> Option<String> {
    let b = crate::oops::wrappers::ByteArrayOop::try_from(result)?;
    let mut bytes = Vec::new();
    b.copy_bytes_out(&mut bytes);
    Some(String::from_utf8_lossy(&bytes).into_owned())
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

    /// G2 smappl slice: a `visual=` labeled-button expression renders to a
    /// live beveled `<button>` fragment (image-side, per D-G5) whose
    /// `data-widget-action` id fires the stored action closure on click.
    #[test]
    fn render_fragment_builds_a_button_and_fires_its_action() {
        struct VecSink(Arc<Mutex<Vec<String>>>);
        impl TranscriptSink for VecSink {
            fn show(&mut self, text: &str) {
                self.0.lock().unwrap().push(text.to_string());
            }
        }

        let mut vm = boot_test_vm(JitMode::Off);
        let captured = Arc::new(Mutex::new(Vec::new()));
        vm.set_transcript(Box::new(VecSink(captured.clone())));

        let html = vm
            .render_fragment(
                "Button labeled: 'Press Me!' action: [ :b | Transcript show: 'clicked' ]",
            )
            .expect("a labeled button must render to an HTML fragment");
        assert!(
            html.contains("smappl-button") && html.contains("Press Me!"),
            "fragment must be a beveled button carrying its label, got {html:?}"
        );
        // The id the fragment advertises is what a click posts back.
        let id_start = html.find("data-widget-action=\"").expect("fragment has an action id")
            + "data-widget-action=\"".len();
        let id_end = html[id_start..].find('"').unwrap() + id_start;
        let id = &html[id_start..id_end];

        vm.exec(&format!("SmapplRegistry fire: '{id}'."))
            .expect("firing the registered widget id must run its action");
        let lines = captured.lock().unwrap();
        assert!(
            lines.iter().any(|l| l.contains("clicked")),
            "firing the button's id must run its action closure, got {lines:?}"
        );
    }

    /// A `visual=` that returns a `Glue` spacer (the side-effecting shape,
    /// gui/smappl.md §3 shape 6) renders to an invisible fixed-width span.
    #[test]
    fn render_fragment_glue_is_an_invisible_spacer() {
        let mut vm = boot_test_vm(JitMode::Off);
        let html = vm
            .render_fragment("Glue xRigid: 12")
            .expect("Glue must render to a fragment");
        assert!(
            html.contains("class=\"glue\"") && html.contains("width:12px"),
            "Glue must render as a 12px spacer, got {html:?}"
        );
    }

    /// Phase-W first tool: the start page's own smappl
    /// (`ClassHierarchyOutliner imbeddedVisualForClass: Object`) renders to a
    /// real class-hierarchy tree — the `allClasses` reflection primitive →
    /// `ClassMirror` subclass sweep → `HtmlWriter` fragment path, end to end.
    #[test]
    fn render_fragment_class_hierarchy_outliner() {
        let mut vm = boot_test_vm(JitMode::Off);
        let html = vm
            .render_fragment("ClassHierarchyOutliner imbeddedVisualForClass: Object")
            .expect("the hierarchy outliner must render");
        assert!(
            html.contains("st-outliner") && html.contains("Object"),
            "must be an outliner tree rooted at Object, got {html:?}"
        );
        // Real subclasses computed from the allClasses sweep must appear —
        // Behavior and Magnitude are both direct or indirect subclasses of
        // Object in the seed world.
        assert!(
            html.contains("Behavior") && html.contains("Magnitude"),
            "the tree must include Object's subclasses, got {html:?}"
        );
        // Collapsible structure: a toggle glyph, nested children containers,
        // and descendants collapsed by default (only the root is open).
        assert!(
            html.contains("st-tw") && html.contains("st-children"),
            "nodes must be collapsible (toggle glyph + children container), got {html:?}"
        );
        assert!(
            html.contains("style=\"display:none\""),
            "descendant subtrees must start collapsed, got {html:?}"
        );
    }

    /// Phase-W method nodes: `ClassOutliner for: (ClassMirror on: Point)`
    /// renders the class's own instance- and class-side selectors (the
    /// `selectorsOf:` R2 primitive → sorted selector leaves), including the
    /// full corpus decoration chain (`topVisualWithHRule:`/`withBorder:`/
    /// `Border standard3DRaised:`, all identity for HTML).
    #[test]
    fn render_fragment_class_outliner_lists_selectors() {
        let mut vm = boot_test_vm(JitMode::Off);
        // The corpus decoration chain is two sends (note the inner parens):
        // `(x topVisualWithHRule: false) withBorder: (...)` — gui/smappl.md §3.4.
        let html = vm
            .render_fragment(
                "((ClassOutliner for: (ClassMirror on: Point)) topVisualWithHRule: false) \
                 withBorder: (Border standard3DRaised: true)",
            )
            .expect("the class outliner must render");
        assert!(
            html.contains("st-classoutliner") && html.contains("Point"),
            "must be a class outliner for Point, got {html:?}"
        );
        assert!(
            html.contains("instance side") && html.contains("class side"),
            "must show both sides, got {html:?}"
        );
        // Point's own instance selectors (x, +, printOn:) and a class-side one
        // (origin) must appear.
        assert!(
            html.contains(">x<") || html.contains("st-selector\">x"),
            "instance selectors must be listed, got {html:?}"
        );
        assert!(html.contains("printOn:") && html.contains("origin"), "got {html:?}");
    }

    /// The `selectorsOf:` primitive answers a class's own selectors and fails
    /// gracefully on a non-behavior.
    #[test]
    fn selectors_of_primitive() {
        let mut vm = boot_test_vm(JitMode::Off);
        let n = vm
            .eval("(ClassMirror selectorsOf: Point) size.")
            .expect("selectorsOf: must run")
            .parse::<i64>()
            .expect("size is an integer");
        assert!(n > 5, "Point defines many instance selectors, got {n}");
        // A non-behavior fails the primitive and hits the method's fallback
        // body (an empty Array) rather than erroring.
        let empty = vm
            .eval("(ClassMirror selectorsOf: 3) size.")
            .expect("selectorsOf: on a non-behavior falls back cleanly");
        assert_eq!(empty, "0", "a non-behavior has no selectors");
    }

    /// A `visual=` shape not yet buildable (needs a later Phase-W wave — the
    /// `CodeView` substrate) surfaces as `Err`, so the GUI falls back to the
    /// G0 placeholder box rather than breaking the page.
    #[test]
    fn render_fragment_unbuilt_shape_is_err_not_panic() {
        let mut vm = boot_test_vm(JitMode::Off);
        let err = vm
            .render_fragment("CodeView forString")
            .expect_err("an unbuilt widget shape must fail, not render");
        match err {
            GuestError::Compile(_) | GuestError::RuntimeError(_) => {}
            other => panic!("expected Compile/RuntimeError, got {other:?}"),
        }
    }

    /// The `allClasses` reflection primitive answers a non-empty set that
    /// includes the well-known genesis classes.
    #[test]
    fn all_classes_primitive_enumerates_the_world() {
        let mut vm = boot_test_vm(JitMode::Off);
        let count = vm
            .eval("ClassMirror allClasses size.")
            .expect("allClasses must run")
            .parse::<i64>()
            .expect("size is an integer");
        assert!(count > 20, "the seed world has many classes, got {count}");
    }

    /// Regression: `dispatch_ffi_primitive` left the receiver+args on the
    /// operand stack instead of truncating to `base` like every table
    /// primitive. Masked in the interpreter (the calling method's return
    /// truncates them), but a COMPILED caller tracks the stack statically, so
    /// the divergence tripped `enter_compiled`'s sp assert
    /// (`compiled_call.rs`) — `Time millisecondClockValue` twice under the JIT
    /// aborted the process. Under `Threshold(1)` the second call runs the
    /// compiled `millisecondClockValue` (which calls an FFI primitive), so a
    /// clean return proves the stack is balanced.
    #[test]
    fn ffi_primitive_under_jit_keeps_the_operand_stack_balanced() {
        let mut vm = boot_test_vm(JitMode::Threshold(1));
        let out = vm
            .eval("[ Time millisecondClockValue. Time millisecondClockValue ] value.")
            .expect("an FFI primitive called from a compiled method must not corrupt the stack");
        assert!(
            out.parse::<i64>().map(|n| n > 0).unwrap_or(false),
            "expected a positive epoch-ms value, got {out:?}"
        );
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
            // DNU/`error:` no longer belong here — `raise_guest_fatal`
            // recovers those at `eval`'s own boundary now (see
            // `eval_dnu_recovers_as_runtime_error_and_vm_stays_usable`
            // below); this test exists to prove the *actually* fatal path
            // (the VM's own invariants/resources, not the guest program's
            // correctness) still correctly kills the thread. Unbounded
            // self-recursion exhausts `ProcessStack` ->
            // `interpreter::stack`'s own "process stack overflow" fatal
            // path -> `fatal_exit`, untouched by this fix.
            vm.eval("Object subclass: MacvmInfiniteRecursionProbe [ go [ ^self go ] ].")
                .expect("defining the recursive-probe class must succeed");
            tx.send("reached-pre-crash-checkpoint").unwrap();
            // Never returns if FatalMode::ExitThread correctly pthread_exits.
            let _ = vm.eval("MacvmInfiniteRecursionProbe new go.");
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

    /// The actual fix: an unhandled DNU used to be indistinguishable from a
    /// genuinely fatal condition (see the previous test's own history) —
    /// every everyday Workspace typo paid a full worker respawn, exactly
    /// the "any mistake kills the VM" experience real Smalltalk's own
    /// recoverable `doesNotUnderstand:` exists to avoid. Proves both
    /// halves: the failure surfaces as an ordinary `Err`, AND the same
    /// `VmHandle` keeps serving requests afterward — the second half is
    /// the one that actually matters; a DNU that merely fails to crash but
    /// leaves the VM unusable wouldn't be a real fix.
    #[test]
    fn eval_dnu_recovers_as_runtime_error_and_vm_stays_usable() {
        let mut vm = boot_test_vm(JitMode::Off);
        let err = vm
            .eval("3 thisSelectorDoesNotExistAnywhereInTheBaseWorld.")
            .expect_err("an unhandled DNU must surface as Err, not run to completion");
        match &err {
            GuestError::RuntimeError(msg) => assert!(
                msg.contains("thisSelectorDoesNotExistAnywhereInTheBaseWorld"),
                "message: {msg}"
            ),
            other => panic!("expected GuestError::RuntimeError, got {other:?}"),
        }
        let result = vm
            .eval("6 * 7.")
            .expect("VM must still be usable after a recovered DNU");
        assert_eq!(result, "42");
    }

    /// `error:`'s own doc comment: "has no proceed semantics in v1" — this
    /// only asserts it's recoverable at `eval`'s OWN boundary (abort this
    /// one doIt, VM stays usable for the next), not that the erroring
    /// computation itself can be resumed mid-flight.
    #[test]
    fn eval_error_colon_recovers_as_runtime_error_and_vm_stays_usable() {
        let mut vm = boot_test_vm(JitMode::Off);
        let err = vm
            .eval("3 error: 'boom'.")
            .expect_err("an unhandled error: must surface as Err, not run to completion");
        match &err {
            GuestError::RuntimeError(msg) => assert!(msg.contains("boom"), "message: {msg}"),
            other => panic!("expected GuestError::RuntimeError, got {other:?}"),
        }
        let result = vm
            .eval("6 * 7.")
            .expect("VM must still be usable after a recovered error:");
        assert_eq!(result, "42");
    }

    /// The riskiest part of the fix, tested directly rather than only by
    /// analogy to the already-proven native-fault case: `raise_guest_fatal`
    /// reuses `siglongjmp` specifically because it's already trusted to
    /// cross JIT-compiled frames soundly (never `catch_unwind` through
    /// them — this project's standing rule). This exercises that for real:
    /// `go` compiles under threshold=1, and its OWN send is what DNUs, so
    /// `go`'s COMPILED frame is still live on the stack when
    /// `dnu_fallback` fires (S11 step 6's "DNU... from compiled code"
    /// path) — not just an interpreter-only DNU.
    #[test]
    fn eval_dnu_from_a_compiled_caller_recovers_cleanly() {
        let mut vm = boot_test_vm(JitMode::Threshold(1));
        vm.eval(
            "Object subclass: MacvmDnuFromCompiledProbe [ \
                go [ ^3 thisSelectorDoesNotExistAnywhereInTheBaseWorld ] \
            ].",
        )
        .expect("defining the probe class must succeed");
        let err = vm
            .eval("MacvmDnuFromCompiledProbe new go.")
            .expect_err("a DNU reached through a compiled caller must still surface as Err");
        assert!(matches!(err, GuestError::RuntimeError(_)), "{err:?}");
        let result = vm
            .eval("6 * 7.")
            .expect("VM must still be usable after recovering through a compiled frame");
        assert_eq!(result, "42");
    }
}
