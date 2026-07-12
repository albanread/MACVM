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

/// A structured command from a game-primitive to the native game pane
/// (`docs/gamepane_design.md`). The core VM defines only this vocabulary; the
/// GUI applies each to the real Metal pane (`gui/src/game_pane.rs`). Drawing
/// commands mutate the pane's CPU buffer only; `Present` uploads and shows the
/// frame — so a whole frame's drawing costs one present, not one per op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameCommand {
    /// Set palette entry `index` (16..=255) to an opaque RGB colour.
    PaletteAt { index: u8, r: u8, g: u8, b: u8 },
    /// Clear the pane to palette `index` (0..=255).
    Cls { index: u8 },
    /// Clear the pane to an opaque RGB colour (convenience: uses palette 16).
    ClearTo { r: u8, g: u8, b: u8 },
    /// Plot a pixel at `(x, y)` in palette `index`.
    Pset { x: i64, y: i64, index: u8 },
    /// Draw a line from `(x0, y0)` to `(x1, y1)` in palette `index`.
    Line {
        x0: i64,
        y0: i64,
        x1: i64,
        y1: i64,
        index: u8,
    },
    /// Fill a `w`x`h` rectangle at `(x, y)` in palette `index`.
    FillRect {
        x: i64,
        y: i64,
        w: i64,
        h: i64,
        index: u8,
    },
    /// Fill a disc of radius `r` centred at `(cx, cy)` in palette `index`.
    Disc {
        cx: i64,
        cy: i64,
        r: i64,
        index: u8,
    },
    /// Upload the CPU buffer and present the frame.
    Present,
    /// Start the frame loop: the GUI's main-thread timer begins pulling one
    /// `GameStep` per tick (single-outstanding) to run the registered step
    /// block. `GamePane>>run`.
    StartLoop,
    /// Stop the frame loop. `GamePane>>stop`.
    StopLoop,
    /// Define a sprite from `/`-separated hex-row art (4 bits/pixel, palette
    /// index per cell) and place an instance of it, both keyed by the VM-chosen
    /// `id` so later `SpriteColor`/`MoveSprite` can address it. `id` is a
    /// monotonic counter minted Smalltalk-side; the GUI registry maps it to
    /// MacGamePane's own def/instance ids.
    DefineSprite { id: i64, rows: String },
    /// Set sprite `id`'s palette entry `index` (0..15) to an opaque RGB colour.
    SpriteColor {
        id: i64,
        index: u8,
        r: u8,
        g: u8,
        b: u8,
    },
    /// Move sprite `id`'s instance to `(x, y)`.
    MoveSprite { id: i64, x: i64, y: i64 },
    /// Play a named SFX preset (0=coin, 1=jump, 2=zap, 3=shoot, 4=explode,
    /// 5=powerup, 6=hurt, 7=click, 8=bang, 9=blip) on the one shared `Sfx`
    /// engine. `Sound coin play`.
    PlaySound { preset: u8 },
}

/// Where game-primitive commands go — the game analogue of [`TranscriptSink`].
/// `Send` because the GUI's sink hands commands across the worker-to-main
/// thread channel, exactly like the transcript sink hands text.
pub trait GameSink: Send {
    fn emit(&mut self, cmd: GameCommand);
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
    /// `(Visual coerce: ([<code>] value)) htmlFragment` — the
    /// `ElementSMAPPL.dlt` shape (`gui/smappl.md` §2) with the body run through
    /// a block — and hands back the resulting `String`'s raw bytes.
    ///
    /// The `[…] value` wrapper is load-bearing: several corpus visuals are
    /// multi-statement with temp declarations (`progenv2.html`'s
    /// `| h v | h := (ClassHierarchyOutliner for: …) filterOn…; orSubclasses.
    /// v := … . v`). Those can't be spliced straight into `(Visual coerce:
    /// (…))` — `(| h v | …)` is a parse error — but a block accepts temps and
    /// statements and answers its last expression, so wrapping evaluates both
    /// single-expression and multi-statement bodies uniformly.
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
        let source = format!("(Visual coerce: ([{code}] value)) htmlFragment.");
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

    /// Fires a live widget's stored action closure (`SmapplRegistry fire:
    /// '<id>'`) and, if that closure answers a `String`, hands back its raw
    /// bytes — the HTML overlay a dialog action produces (`Visual>>promptOk:…`,
    /// the differences2.html "Press Me!" demo). A non-`String` answer is a
    /// pure side-effect action (an icon button's `[:b | …]`) and yields
    /// `Ok(None)` — no overlay. Any `Transcript` output the action makes still
    /// flows separately via the transcript sink.
    ///
    /// This is [`render_fragment`](Self::render_fragment)'s sibling: same
    /// signal-guarded top-item execution, but it wraps the `fire:` send instead
    /// of `htmlFragment` and treats a non-`String` result as "nothing to show"
    /// rather than a render error.
    #[allow(unsafe_code)]
    pub fn fire_widget_action(&mut self, action_id: &str) -> Result<Option<String>, GuestError> {
        // action_id is a worker-minted 'wN' id (SmapplRegistry), never user
        // text, so it needs no quoting — but guard the assumption cheaply.
        debug_assert!(action_id.bytes().all(|b| b.is_ascii_alphanumeric()));
        let source = format!("SmapplRegistry fire: '{action_id}'.");
        let slot = deopt_trap::claim_jmp_slot();
        // SAFETY: as `render_fragment` — `sigsetjmp` inline at this call site,
        // whose frame stays live for the whole recovery window.
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
            Ok(None) => return Ok(None),
            Err(e) => return Err(GuestError::Compile(e)),
        };
        match frontend::classdef::execute_top_item(&mut self.vm, item) {
            // A String answer is the dialog overlay; anything else (self, nil)
            // is a side-effect-only action, so there is nothing to inject.
            Ok(Some(result)) => Ok(fragment_bytes(result)),
            Ok(None) => Ok(None),
            Err(e) => Err(GuestError::Compile(e)),
        }
    }

    /// Evaluates `code` (wrapped `[<code>] value`, so multi-statement bodies
    /// with temps are fine — see [`render_fragment`](Self::render_fragment))
    /// and returns the answered `String`'s raw bytes. Used for image-side code
    /// that builds a plain string payload rather than a widget fragment — e.g.
    /// `Mandelbrot new commandsForWidth:height:` answering a Canvas
    /// draw-command batch (`docs/CANVAS.md` §5.2). A non-`String` answer is an
    /// `Err`, like `render_fragment`'s own non-`String` case.
    #[allow(unsafe_code)]
    pub fn eval_to_string(&mut self, code: &str) -> Result<String, GuestError> {
        let source = format!("([{code}] value).");
        let slot = deopt_trap::claim_jmp_slot();
        // SAFETY: as `render_fragment` — `sigsetjmp` inline at this call site,
        // whose frame stays live for the whole recovery window.
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
            Ok(Some(result)) => fragment_bytes(result).ok_or_else(|| {
                GuestError::RuntimeError("expression did not answer a String".to_string())
            }),
            Ok(None) => Ok(String::new()),
            Err(e) => Err(GuestError::Compile(e)),
        }
    }

    /// Evaluates `code` (wrapped `[<code>] value`, like
    /// [`eval_to_string`](Self::eval_to_string)) and returns the answered
    /// `ByteArray`/`String`'s bytes RAW — no UTF-8 conversion, so arbitrary
    /// binary is preserved. Used for bulk pixel data: `Mandelbrot new
    /// pixelsForWidth:height:` answers a `w*h*4` RGBA `ByteArray`
    /// (`world/36_pixmap.mst`, `docs/CANVAS.md` pixel path). A non-byte-indexable
    /// answer is an `Err`.
    #[allow(unsafe_code)]
    pub fn eval_to_bytes(&mut self, code: &str) -> Result<Vec<u8>, GuestError> {
        let source = format!("([{code}] value).");
        let slot = deopt_trap::claim_jmp_slot();
        // SAFETY: as `render_fragment` — `sigsetjmp` inline at this call site,
        // whose frame stays live for the whole recovery window.
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
            Ok(None) => return Ok(Vec::new()),
            Err(e) => return Err(GuestError::Compile(e)),
        };
        match frontend::classdef::execute_top_item(&mut self.vm, item) {
            Ok(Some(result)) => {
                let b = crate::oops::wrappers::ByteArrayOop::try_from(result).ok_or_else(|| {
                    GuestError::RuntimeError("expression did not answer a ByteArray".to_string())
                })?;
                let mut bytes = Vec::new();
                b.copy_bytes_out(&mut bytes);
                Ok(bytes)
            }
            Ok(None) => Ok(Vec::new()),
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

    /// Installs `sink` as where game-primitive commands go
    /// (`docs/gamepane_design.md` M3) — the game analogue of `set_transcript`.
    /// Default is `None` (a headless VM silently drops game commands); the GUI
    /// installs a channel-backed sink once, right after `boot`.
    pub fn set_game_sink(&mut self, sink: Box<dyn GameSink>) {
        self.vm.game_sink = Some(sink);
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

    /// A bare Do-it (no terminating period) must evaluate, not fail with
    /// "expected '.' after statement". This is how every GUI doit arrives —
    /// the tour's `doit="Mandelbrot new launch"` and a Workspace "Do it" on a
    /// selected expression both lack a trailing period.
    #[test]
    fn eval_tolerates_a_missing_trailing_period() {
        let mut vm = boot_test_vm(JitMode::Off);
        assert_eq!(
            vm.eval("3 + 4").expect("a bare expression must evaluate without a trailing period"),
            "7"
        );
        // A trailing period still works (unchanged).
        assert_eq!(vm.eval("10 * 2.").expect("with period still fine"), "20");
        // Genuine trailing garbage after a complete statement is still an error.
        assert!(
            vm.eval("3 + 4  5").is_err(),
            "two statements without a separating period must still be rejected"
        );
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

    /// The differences2.html "Press Me!" demo: a labeled button whose action
    /// is `b promptOk:title:type:action:`. Firing it must answer the modal
    /// dialog's HTML (a String) — `fire_widget_action` surfaces that as the
    /// overlay to float; a pure side-effect action instead answers `None`.
    #[test]
    fn firing_a_promptok_action_yields_the_dialog_overlay_html() {
        let mut vm = boot_test_vm(JitMode::Off);

        // The exact corpus shape (differences2.html), collapsed to one line.
        let html = vm
            .render_fragment(
                "Button labeled: 'Press Me!' action: [ :b | \
                 b promptOk: 'The UI can do native things easily.' \
                 title: 'It works!' type: #info action: [] ]",
            )
            .expect("the Press Me button must render");
        let id_start = html.find("data-widget-action=\"").expect("has an action id")
            + "data-widget-action=\"".len();
        let id_end = html[id_start..].find('"').unwrap() + id_start;
        let id = html[id_start..id_end].to_string();

        let overlay = vm
            .fire_widget_action(&id)
            .expect("firing must succeed")
            .expect("a promptOk action must answer dialog HTML, not None");
        assert!(
            overlay.contains("st-modal")
                && overlay.contains("st-modal-info")
                && overlay.contains("It works!")
                && overlay.contains("The UI can do native things easily.")
                && overlay.contains("st-modal-ok"),
            "overlay must be an info modal carrying title, message and an OK button, got {overlay:?}"
        );

        // A side-effect-only action (no String answer) yields no overlay.
        let side = vm
            .render_fragment("Button labeled: 'x' action: [ :b | 1 + 1 ]")
            .expect("button renders");
        let s0 = side.find("data-widget-action=\"").unwrap() + "data-widget-action=\"".len();
        let s1 = side[s0..].find('"').unwrap() + s0;
        let sid = side[s0..s1].to_string();
        assert_eq!(
            vm.fire_widget_action(&sid).expect("firing must succeed"),
            None,
            "a non-String action result must not produce an overlay"
        );
    }

    /// The Canvas Mandelbrot demo (`world/35_mandelbrot.mst`,
    /// `docs/CANVAS.md` pixel path): `Mandelbrot new pixelsForWidth:height:`
    /// computes the set in real Smalltalk `Double` arithmetic and fills a
    /// `Pixmap`, answering its raw `w*h*4` RGBA `ByteArray`. The buffer must be
    /// exactly the right size, fully opaque, and contain both interior (black)
    /// and escaped (coloured) pixels — i.e. the float compute really
    /// discriminated points, not painted one flat colour.
    #[test]
    fn mandelbrot_fills_an_rgba_pixmap() {
        let mut vm = boot_test_vm(JitMode::Threshold(1));
        let (w, h) = (120usize, 90usize);
        let bytes = vm
            .eval_to_bytes(&format!("Mandelbrot new pixelsForWidth: {w} height: {h}"))
            .expect("Mandelbrot must answer an RGBA ByteArray");

        assert_eq!(bytes.len(), w * h * 4, "buffer must be exactly w*h*4 RGBA");
        // Every alpha byte (every 4th) must be fully opaque.
        assert!(
            bytes.iter().skip(3).step_by(4).all(|&a| a == 255),
            "every pixel must be opaque (alpha 255)"
        );
        // A black interior pixel exists (some point reached maxIter)...
        let has_black = bytes
            .chunks(4)
            .any(|p| p[0] == 0 && p[1] == 0 && p[2] == 0);
        // ...and a non-black escaped pixel exists (the bands).
        let has_colour = bytes
            .chunks(4)
            .any(|p| p[0] != 0 || p[1] != 0 || p[2] != 0);
        assert!(
            has_black && has_colour,
            "the fractal must have both interior (black) and escaped (coloured) pixels"
        );
    }

    /// Float fast-path deopt-sunk boxing (`docs/float_fastpath_design.md`):
    /// an intermediate `FBox` pinned only by a later fused op's reexecute
    /// stack moves into that op's fail block. If the later op deopts (a
    /// non-Double ARG — the IC only guards receivers), the interpreter
    /// re-executes the send and must see the CORRECT boxed intermediate,
    /// built by the sunk box on the cold path. Wrong bits here would be a
    /// silent wrong answer, so this pins the exact fallback values.
    #[test]
    fn float_fuse_deopt_reboxes_sunk_intermediates_correctly() {
        let mut vm = boot_test_vm(JitMode::Threshold(10));
        vm.exec(
            "Object subclass: FSinkT [ mix: a with: b plus: c [ ^a * b + c ] \
             chain: a with: b [ ^2.0 * a * b + 0.5 ] ]",
        )
        .expect("class definition");
        // Warm the fused sites mono-Double, past the threshold.
        vm.exec(
            "[ | f | f := FSinkT new. 1 to: 500 do: [ :i | \
             f mix: 3.5 with: 2.0 plus: 1.25. f chain: 1.5 with: 0.25 ] ] value.",
        )
        .expect("warmup");
        // The + arg-unbox fails: reexec needs the sunk box of 3.5*2.0 = 7.0.
        assert_eq!(
            vm.eval("FSinkT new mix: 3.5 with: 2.0 plus: 1").expect("mixed"),
            "8.0"
        );
        // Mid-chain deopt: the second * fails; its reexec stack holds the
        // sunk box of 2.0*1.5 = 3.0 → 3.0*2 = 6.0 (int fallback) + 0.5.
        assert_eq!(
            vm.eval("FSinkT new chain: 1.5 with: 2").expect("chained"),
            "6.5"
        );
        // And the pure-Double path still answers exactly.
        assert_eq!(
            vm.eval("FSinkT new mix: 3.5 with: 2.0 plus: 1.25").expect("pure"),
            "8.25"
        );
    }

    /// Float fast-path step 5b (`docs/float_fastpath_design.md` B5): PROMOTED
    /// float temps live as raw f64 frame slots across safepoints; the deopt
    /// map records `ValueLoc::DoubleSlot` and the materializer boxes them.
    /// This test forces an uncommon trap IN THE MIDDLE of a loop whose float
    /// temps are promoted (a fused site warmed mono-Double, then handed an
    /// integer receiver at iteration 500): the materializer must rebuild
    /// `a`/`b` exactly from their raw slots, and the remaining ~500
    /// iterations run interpreted on the rebuilt frame. Wrong bits anywhere
    /// would change the final sum. Asserts a deopt actually fired, so the
    /// path can never silently stop being exercised.
    #[test]
    fn float_temp_promotion_materializes_raw_slots_on_a_midloop_deopt() {
        let mut vm = boot_test_vm(JitMode::Threshold(10));
        vm.exec(
            "Object subclass: WTestT [ run: coll [ | a b i x | \
             a := 1.5. b := 0.25. i := 0. x := 0.0. \
             [ i < 1000 ] whileTrue: [ \
                 a := a * 1.001. b := b + a. \
                 x := (coll at: i + 1) + 2.0. i := i + 1 ]. \
             ^b + x ] ]",
        )
        .expect("class definition");
        vm.exec(
            "[ | w good | w := WTestT new. good := Array new: 1000. \
             1 to: 1000 do: [ :k | good at: k put: 0.5 ]. \
             1 to: 30 do: [ :k | w run: good ] ] value.",
        )
        .expect("warmup");
        let clean = vm
            .eval("WTestT new run: ((1 to: 1000) inject: (Array new: 1000) into: [ :a :k | a at: k put: 0.5. a ])")
            .unwrap_or_default();
        let deopts_before = vm.vm.stats.deopt_count;
        // Poison element 501: iteration 500's fused `+ 2.0` receiver is an
        // integer → trap mid-loop with promoted a/b live.
        let poisoned = vm
            .eval(
                "[ | bad | bad := Array new: 1000. \
                 1 to: 1000 do: [ :k | bad at: k put: 0.5 ]. \
                 bad at: 501 put: 7. \
                 WTestT new run: bad ] value.",
            )
            .expect("poisoned run");
        assert!(
            vm.vm.stats.deopt_count > deopts_before,
            "the poisoned run must actually deopt mid-loop (else this test \
             is not exercising DoubleSlot materialization)"
        );
        // Interpreter truth for the poisoned input: 0.5+2.0 everywhere except
        // element 501 (7 + 2.0 = 9.0 — but x is overwritten each iteration,
        // so only the LAST element's x survives; the sum differs from `clean`
        // only through the b accumulation being identical and x identical) —
        // compare against the same expression run fully interpreted instead
        // of hand-computing.
        let mut interp = boot_test_vm(JitMode::Off);
        interp
            .exec(
                "Object subclass: WTestT [ run: coll [ | a b i x | \
                 a := 1.5. b := 0.25. i := 0. x := 0.0. \
                 [ i < 1000 ] whileTrue: [ \
                     a := a * 1.001. b := b + a. \
                     x := (coll at: i + 1) + 2.0. i := i + 1 ]. \
                 ^b + x ] ]",
            )
            .expect("class definition (interp)");
        let interp_poisoned = interp
            .eval(
                "[ | bad | bad := Array new: 1000. \
                 1 to: 1000 do: [ :k | bad at: k put: 0.5 ]. \
                 bad at: 501 put: 7. \
                 WTestT new run: bad ] value.",
            )
            .expect("poisoned run (interp)");
        assert_eq!(
            poisoned, interp_poisoned,
            "mid-loop deopt with promoted float temps must be byte-identical \
             to the interpreter"
        );
        assert!(!clean.is_empty(), "clean run sanity");
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

    /// progenv2.html's filtered-hierarchy visual is multi-statement with temp
    /// declarations (`| h v | h := (ClassHierarchyOutliner for: …) filterOn…;
    /// orSubclasses. v := (h topVisualWithHRule: false) withBorder: …. v`).
    /// The `[…] value` wrapper must let it render (a live, unfiltered outliner)
    /// rather than trip a parse error on the leading `| h v |`.
    #[test]
    fn render_fragment_handles_a_multi_statement_visual_with_temps() {
        let mut vm = boot_test_vm(JitMode::Off);
        let html = vm
            .render_fragment(
                "| h v | \
                 h := (ClassHierarchyOutliner for: (ClassMirror on: Object)) \
                     filterOnCommentsContaining: '%HTML'; orSubclasses. \
                 v := (h topVisualWithHRule: false) \
                     withBorder: (Border standard3DRaised: true). \
                 v",
            )
            .expect("a multi-statement visual with temps must render, not parse-fail");
        assert!(
            html.contains("st-outliner") && html.contains("data-hierarchy-root=\"Object\""),
            "the cascade+temps body must yield the live Object outliner, got {html:?}"
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

    /// `primitiveOf:selector:` (R2) distinguishes a primitive method (VM code,
    /// shown read-only in the browser) from an ordinary Smalltalk one — and
    /// the ClassOutliner renders the two differently.
    #[test]
    fn primitive_methods_render_read_only() {
        let mut vm = boot_test_vm(JitMode::Off);
        // Alien>>byteAt: is a table primitive; Point>>x is ordinary Smalltalk.
        assert_ne!(
            vm.eval("ClassMirror primitiveOf: Alien selector: #byteAt:.").unwrap(),
            "0",
            "byteAt: must report a primitive number"
        );
        assert_eq!(
            vm.eval("ClassMirror primitiveOf: Point selector: #x.").unwrap(),
            "0",
            "an ordinary method has no primitive"
        );
        // Alien's browser shows read-only primitive notes and no editors.
        let alien = vm
            .render_fragment("ClassOutliner for: (ClassMirror on: Alien)")
            .expect("render Alien");
        assert!(
            alien.contains("st-prim-note") && !alien.contains("st-smappl-src"),
            "primitive methods must be read-only notes, not editors"
        );
        // Point's browser is all editable.
        let point = vm
            .render_fragment("ClassOutliner for: (ClassMirror on: Point)")
            .expect("render Point");
        assert!(
            point.contains("st-smappl-src") && !point.contains("st-prim-note"),
            "ordinary methods must be editable"
        );
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
    fn set_game_sink_routes_game_commands_from_a_smalltalk_doit() {
        // The M3 vertical slice end to end: a Smalltalk doit -> GamePane
        // primitive (id 200) -> GameCommand -> the installed sink. Headless,
        // deterministic, no GPU/window — this is the real proof of the VM->GUI
        // game channel (docs/gamepane_design.md M3).
        struct VecGameSink(Arc<Mutex<Vec<GameCommand>>>);
        impl GameSink for VecGameSink {
            fn emit(&mut self, cmd: GameCommand) {
                self.0.lock().unwrap().push(cmd);
            }
        }

        let mut vm = boot_test_vm(JitMode::Off);
        let captured = Arc::new(Mutex::new(Vec::new()));
        vm.set_game_sink(Box::new(VecGameSink(captured.clone())));
        vm.eval("GamePane new clearR: 200 g: 40 b: 40.")
            .expect("GamePane>>clearR:g:b: must evaluate cleanly");
        assert_eq!(
            *captured.lock().unwrap(),
            vec![GameCommand::ClearTo { r: 200, g: 40, b: 40 }],
            "the game sink must capture exactly the ClearTo command"
        );
    }

    #[test]
    fn game_primitive_fails_on_out_of_range_colour_and_emits_nothing() {
        // r=300 is out of 0..=255, so `smi_byte` fails, the primitive fails,
        // and the method falls through to `^self` — no command emitted. This
        // is the design's rule: validate at the primitive boundary before a
        // value can reach an assert!-panicking engine setter.
        struct VecGameSink(Arc<Mutex<Vec<GameCommand>>>);
        impl GameSink for VecGameSink {
            fn emit(&mut self, cmd: GameCommand) {
                self.0.lock().unwrap().push(cmd);
            }
        }

        let mut vm = boot_test_vm(JitMode::Off);
        let captured = Arc::new(Mutex::new(Vec::new()));
        vm.set_game_sink(Box::new(VecGameSink(captured.clone())));
        vm.eval("GamePane new clearR: 300 g: 0 b: 0.")
            .expect("an out-of-range colour must not crash — the primitive just fails");
        assert!(
            captured.lock().unwrap().is_empty(),
            "an out-of-range colour must emit no game command"
        );
    }

    #[test]
    fn game_drawing_commands_reach_the_sink_in_order() {
        struct VecGameSink(Arc<Mutex<Vec<GameCommand>>>);
        impl GameSink for VecGameSink {
            fn emit(&mut self, cmd: GameCommand) {
                self.0.lock().unwrap().push(cmd);
            }
        }

        let mut vm = boot_test_vm(JitMode::Off);
        let captured = Arc::new(Mutex::new(Vec::new()));
        vm.set_game_sink(Box::new(VecGameSink(captured.clone())));
        // A cascade (all messages to one `GamePane new`) — top-level `| temp |`
        // declarations aren't valid in the doit dialect.
        vm.eval(
            "GamePane new \
               paletteAt: 16 r: 10 g: 20 b: 30; \
               cls: 16; \
               point: 5 y: 7 color: 16; \
               line: 0 y: 0 to: 9 y: 9 color: 16; \
               fill: 1 y: 2 width: 3 height: 4 color: 16; \
               disc: 8 y: 8 radius: 2 color: 16; \
               present.",
        )
        .expect("the drawing doit must evaluate cleanly");
        assert_eq!(
            *captured.lock().unwrap(),
            vec![
                GameCommand::PaletteAt { index: 16, r: 10, g: 20, b: 30 },
                GameCommand::Cls { index: 16 },
                GameCommand::Pset { x: 5, y: 7, index: 16 },
                GameCommand::Line { x0: 0, y0: 0, x1: 9, y1: 9, index: 16 },
                GameCommand::FillRect { x: 1, y: 2, w: 3, h: 4, index: 16 },
                GameCommand::Disc { cx: 8, cy: 8, r: 2, index: 16 },
                GameCommand::Present,
            ],
            "every drawing method must reach the sink as its command, in order"
        );
    }

    #[test]
    fn frame_loop_run_registers_a_step_block_the_gui_can_pull() {
        struct VecGameSink(Arc<Mutex<Vec<GameCommand>>>);
        impl GameSink for VecGameSink {
            fn emit(&mut self, cmd: GameCommand) {
                self.0.lock().unwrap().push(cmd);
            }
        }

        let mut vm = boot_test_vm(JitMode::Off);
        let captured = Arc::new(Mutex::new(Vec::new()));
        vm.set_game_sink(Box::new(VecGameSink(captured.clone())));

        // Registering a step block and calling `run` emits StartLoop and
        // returns immediately (no blocking loop).
        vm.eval("GamePane new onStep: [ GamePane new cls: 3 ]; run.")
            .expect("onStep:/run must evaluate cleanly");
        assert_eq!(*captured.lock().unwrap(), vec![GameCommand::StartLoop]);

        // A GUI frame tick (`GamePane stepWithKeys:`) runs the step block, so
        // its drawing reaches the sink — the pull the GUI timer performs.
        captured.lock().unwrap().clear();
        vm.eval("GamePane stepWithKeys: 0.")
            .expect("stepWithKeys: must run the step block");
        assert_eq!(
            *captured.lock().unwrap(),
            vec![GameCommand::Cls { index: 3 }]
        );
    }

    #[test]
    fn frame_loop_keyheld_reads_the_tick_key_mask() {
        struct VecGameSink(Arc<Mutex<Vec<GameCommand>>>);
        impl GameSink for VecGameSink {
            fn emit(&mut self, cmd: GameCommand) {
                self.0.lock().unwrap().push(cmd);
            }
        }

        let mut vm = boot_test_vm(JitMode::Off);
        let captured = Arc::new(Mutex::new(Vec::new()));
        vm.set_game_sink(Box::new(VecGameSink(captured.clone())));

        // The step block draws cls:7 only when Left is held, cls:8 only when
        // Right is. A tick with mask 5 (bits 0=Left and 2=Up) must run cls:7
        // and not cls:8 — proving keyHeld: reads the mask stepWithKeys: set.
        vm.eval(
            "GamePane new onStep: [ \
               (GamePane keyHeld: GamePane keyLeft)  ifTrue: [ GamePane new cls: 7 ]. \
               (GamePane keyHeld: GamePane keyRight) ifTrue: [ GamePane new cls: 8 ] ].",
        )
        .expect("onStep: must evaluate cleanly");
        vm.eval("GamePane stepWithKeys: 5.")
            .expect("stepWithKeys: must run");
        assert_eq!(
            *captured.lock().unwrap(),
            vec![GameCommand::Cls { index: 7 }],
            "Left held -> cls:7 only; Right not held -> no cls:8"
        );
    }

    #[test]
    fn sprite_commands_reach_the_sink_from_smalltalk() {
        struct VecGameSink(Arc<Mutex<Vec<GameCommand>>>);
        impl GameSink for VecGameSink {
            fn emit(&mut self, cmd: GameCommand) {
                self.0.lock().unwrap().push(cmd);
            }
        }

        let mut vm = boot_test_vm(JitMode::Off);
        let captured = Arc::new(Mutex::new(Vec::new()));
        vm.set_game_sink(Box::new(VecGameSink(captured.clone())));

        // defineSprite: mints id 1 and emits DefineSprite; the returned Sprite's
        // cascade emits SpriteColor then MoveSprite for that same id.
        vm.eval(
            "(GamePane new defineSprite: 'f0f/0f0/f0f') \
               colorAt: 15 r: 240 g: 240 b: 0; \
               moveTo: 100 y: 80.",
        )
        .expect("the sprite doit must evaluate cleanly");
        assert_eq!(
            *captured.lock().unwrap(),
            vec![
                GameCommand::DefineSprite { id: 1, rows: "f0f/0f0/f0f".to_string() },
                GameCommand::SpriteColor { id: 1, index: 15, r: 240, g: 240, b: 0 },
                GameCommand::MoveSprite { id: 1, x: 100, y: 80 },
            ],
            "define/color/move must reach the sink for the minted sprite id"
        );
    }

    #[test]
    fn sound_play_reaches_the_sink_from_smalltalk() {
        struct VecGameSink(Arc<Mutex<Vec<GameCommand>>>);
        impl GameSink for VecGameSink {
            fn emit(&mut self, cmd: GameCommand) {
                self.0.lock().unwrap().push(cmd);
            }
        }

        let mut vm = boot_test_vm(JitMode::Off);
        let captured = Arc::new(Mutex::new(Vec::new()));
        vm.set_game_sink(Box::new(VecGameSink(captured.clone())));

        // `Sound <preset> play` reaches the sink as PlaySound{preset} — the
        // named presets map to 0..9. (Headless: the command, not actual audio.)
        vm.eval("Sound coin play.")
            .expect("Sound coin play must evaluate cleanly");
        vm.eval("Sound bang play.")
            .expect("Sound bang play must evaluate cleanly");
        assert_eq!(
            *captured.lock().unwrap(),
            vec![
                GameCommand::PlaySound { preset: 0 }, // coin
                GameCommand::PlaySound { preset: 8 }, // bang
            ]
        );
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
