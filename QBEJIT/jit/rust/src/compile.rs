//! Safe entry point into the vendored QBE-fork pipeline: QBE IL text in,
//! a [`JitCollectorHandle`] (an owned, RAII-wrapped `JitInst[]`) out.

use crate::ffi;
use libc::{c_int, c_void};
use std::ffi::{CStr, CString};
use std::sync::Mutex;

/// QBE's C globals (`Target T`, `char debug[]`, the JIT bridge's
/// file-scoped state in `qbe_bridge.c`/`jit_collect.c`, and — for the
/// text-free path — `ir_builder.c`'s own `curb`/`blink`/`nblk` equivalents
/// plus the *shared* `insb`/`curi` scratch buffer both paths write
/// through) are not thread-safe — `qbe_bridge.h` says so explicitly. This
/// crate enforces that at the API boundary instead of pushing the
/// obligation onto every caller: only one compile runs at a time,
/// process-wide. `pub(crate)` so `crate::ir` can take the *same* lock —
/// this is one shared resource with two entry points (text and
/// text-free), not two independent ones; a separate lock per entry point
/// would not actually prevent the race.
pub(crate) static COMPILE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug)]
pub enum JitCompileError {
    /// `il_text` was empty, or contained interior NUL bytes (can't be
    /// passed through the C API's `(ptr, len)` pair safely as a C string
    /// target name — the IL text itself is passed by explicit length, so
    /// only the *target* string needs to be NUL-free).
    InvalidInput(String),
    /// QBE rejected the target name (`qbe_available_targets()`).
    UnknownTarget(String),
    /// QBE's parser hit a fatal error and called `basic_exit()` — see
    /// `csrc/rust_bridge.c`. The diagnostic text QBE printed went to
    /// stderr (that's upstream `err()`/`die_()` behavior, unchanged here);
    /// this variant only tells you compilation didn't produce anything.
    ParseAborted,
    /// `qbe_compile_il_jit` returned a QBE_ERR_* code without hitting the
    /// longjmp path, or the collector's own `error` field was set.
    Failed { code: i32, message: String },
}

impl std::fmt::Display for JitCompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput(s) => write!(f, "invalid input: {s}"),
            Self::UnknownTarget(t) => write!(f, "unknown QBE target: {t}"),
            Self::ParseAborted => {
                write!(f, "QBE aborted compilation (fatal parse error; see stderr)")
            }
            Self::Failed { code, message } => write!(f, "QBE compile failed ({code}): {message}"),
        }
    }
}
impl std::error::Error for JitCompileError {}

/// Owned handle to a completed `JitCollector`. Frees the collector's
/// malloc'd instruction array on drop.
pub struct JitCollectorHandle {
    raw: ffi::JitCollector,
}

// SAFETY: JitCollectorHandle owns its `insts` allocation exclusively (no
// other C or Rust code holds a pointer to it once compile_il_jit returns),
// and QBE's global mutable state is only touched while COMPILE_LOCK is
// held during compilation — not afterward, when only this Rust-owned
// buffer is being read. Safe to move/access from another thread post-hoc.
unsafe impl Send for JitCollectorHandle {}

impl JitCollectorHandle {
    /// The collected instruction stream, in program order (functions and
    /// data definitions interleaved, delimited by FUNC_BEGIN/FUNC_END and
    /// DATA_START/DATA_END — see `jit_collect.h`).
    pub fn instructions(&self) -> &[ffi::JitInst] {
        if self.raw.insts.is_null() || self.raw.ninst == 0 {
            return &[];
        }
        // SAFETY: `insts` was allocated by jit_collector_init/jit_emit as
        // a contiguous array of `ninst` valid, initialized JitInst records
        // (see jit_collect.c) and is not mutated again after
        // qbe_compile_il_jit returns successfully.
        unsafe { std::slice::from_raw_parts(self.raw.insts, self.raw.ninst as usize) }
    }

    pub fn function_count(&self) -> u32 {
        self.raw.nfunc
    }

    pub fn data_count(&self) -> u32 {
        self.raw.ndata
    }

    /// Wrap an already-populated `JitCollector` (e.g. from
    /// [`crate::ir::build_function_jit`], which drives `qbe_ir_compile_jit`
    /// directly rather than `qbe_compile_il_jit`) — the two entry points
    /// share this same owned-handle type since both produce the identical
    /// C struct.
    pub(crate) fn from_raw(raw: ffi::JitCollector) -> Self {
        Self { raw }
    }
}

impl Drop for JitCollectorHandle {
    fn drop(&mut self) {
        // SAFETY: `raw` was initialized by jit_collector_init and is only
        // ever freed here, exactly once.
        unsafe { ffi::jit_collector_free(&mut self.raw) };
    }
}

struct CompileCtx {
    il_ptr: *const libc::c_char,
    il_len: usize,
    jc: *mut ffi::JitCollector,
    target: *const libc::c_char,
    // Populated by the trampoline; read back by compile_il_jit after
    // rust_qbe_protected_call returns, so a real QBE_ERR_* code isn't
    // conflated with RUST_QBE_LONGJMP_SENTINEL's own return channel.
    rc: c_int,
}

extern "C" fn compile_trampoline(ctx: *mut c_void) -> c_int {
    // SAFETY: `ctx` is always `&mut CompileCtx` cast to `*mut c_void` by
    // compile_il_jit below, immediately before this call, and not aliased
    // elsewhere during the call.
    let ctx = unsafe { &mut *(ctx as *mut CompileCtx) };
    // SAFETY: called only from within rust_qbe_protected_call's setjmp
    // guard (that's the whole point of this trampoline existing), with
    // il_ptr/il_len describing a valid, live `&str`'s bytes and jc a live
    // `*mut JitCollector`.
    let rc = unsafe { ffi::qbe_compile_il_jit(ctx.il_ptr, ctx.il_len, ctx.jc, ctx.target) };
    ctx.rc = rc;
    rc
}

/// Compiles a QBE IL translation unit through QBE's full optimizer +
/// register allocator, collecting the result as a flat `JitInst[]` instead
/// of emitting assembly text.
///
/// `target`: one of `"arm64_apple"`, `"arm64"`, `"amd64_apple"`,
/// `"amd64_sysv"`, `"rv64"`, or `None` for the build's default
/// (`arm64_apple`, per `build.rs`). MACVM only needs `arm64_apple`; the
/// others are exposed because `qbe_bridge.c`'s target table compiles them
/// in regardless (see `build.rs`), not because this crate tests them.
///
/// Blocks other callers on this process until it returns — see
/// [`COMPILE_LOCK`].
pub fn compile_il_jit(
    il_text: &str,
    target: Option<&str>,
) -> Result<JitCollectorHandle, JitCompileError> {
    if il_text.is_empty() {
        return Err(JitCompileError::InvalidInput("empty IL text".into()));
    }

    let target_cstring = target
        .map(CString::new)
        .transpose()
        .map_err(|_| JitCompileError::InvalidInput("target name contained a NUL byte".into()))?;
    let target_ptr = target_cstring
        .as_ref()
        .map(|c| c.as_ptr())
        .unwrap_or(std::ptr::null());

    let _guard = COMPILE_LOCK.lock().unwrap_or_else(|poisoned| {
        // A prior compile panicked while holding the lock, which — given
        // QBE's global C state — leaves that state in an unknown shape.
        // There is no safe way to recover it; surface the poison instead
        // of silently compiling against corrupted globals.
        poisoned.into_inner()
    });

    let mut jc = ffi::JitCollector::zeroed();
    // SAFETY: jc is a valid, zeroed JitCollector; jit_collector_init only
    // requires that (see jit_collect.c).
    if unsafe { ffi::jit_collector_init(&mut jc) } != 0 {
        return Err(JitCompileError::Failed {
            code: -1,
            message: "jit_collector_init: allocation failure".into(),
        });
    }

    let mut ctx = CompileCtx {
        il_ptr: il_text.as_ptr() as *const libc::c_char,
        il_len: il_text.len(),
        jc: &mut jc,
        target: target_ptr,
        rc: 0,
    };

    // SAFETY: compile_trampoline only touches `ctx` (valid for the
    // duration of this call) and calls qbe_compile_il_jit inside the
    // setjmp guard rust_qbe_protected_call establishes, so a longjmp from
    // basic_exit() unwinds no further than that guard — never through
    // this Rust frame.
    let outer_rc =
        unsafe { ffi::rust_qbe_protected_call(compile_trampoline, &mut ctx as *mut _ as *mut c_void) };

    if outer_rc == ffi::RUST_QBE_LONGJMP_SENTINEL {
        // SAFETY: qbe_jit_cleanup is safe to call any time (guarded
        // internally); required here to close the fmemopen handle parse()
        // was reading from and release QBE's arena, both skipped by the
        // longjmp.
        unsafe { ffi::qbe_jit_cleanup() };
        unsafe { ffi::jit_collector_free(&mut jc) };
        return Err(JitCompileError::ParseAborted);
    }

    // outer_rc == ctx.rc whenever the trampoline returned normally
    // (rust_qbe_protected_call passes through non-longjmp results as-is).
    debug_assert_eq!(outer_rc, ctx.rc);

    if ctx.rc == ffi::qbe_status::ERR_TARGET {
        unsafe { ffi::jit_collector_free(&mut jc) };
        return Err(JitCompileError::UnknownTarget(
            target.unwrap_or("<default>").to_string(),
        ));
    }
    if ctx.rc != ffi::qbe_status::OK || jc.error != 0 {
        let message = collector_error_message(&jc);
        unsafe { ffi::jit_collector_free(&mut jc) };
        return Err(JitCompileError::Failed {
            code: ctx.rc,
            message,
        });
    }

    Ok(JitCollectorHandle { raw: jc })
}

pub(crate) fn collector_error_message(jc: &ffi::JitCollector) -> String {
    // SAFETY: error_msg is a fixed, NUL-terminated (or all-zero) buffer
    // owned by `jc`, valid for the duration of this call.
    unsafe { CStr::from_ptr(jc.error_msg.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// The default QBE target name this build was configured for
/// (`"arm64_apple"` — see `build.rs`).
pub fn default_target() -> String {
    // SAFETY: qbe_default_target() returns a pointer to static storage
    // that's never freed (see qbe_bridge.c).
    unsafe { CStr::from_ptr(ffi::qbe_default_target()) }
        .to_string_lossy()
        .into_owned()
}
