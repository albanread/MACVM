//! Boot-phase timing — `MACVM_BOOT_TIMING=1` (§bytecode-boot design, B0).
//!
//! Answers ONE question: of the time a world takes to boot, how much goes to
//! each phase — and above all, how much goes to METHOD COMPILATION, since
//! that is the share a bytecode-record boot (`method_bytecode`) would
//! eliminate. Measured before designing the encoder, so the design starts
//! from a number instead of a hunch.
//!
//! Phases, accumulated in atomics by hooks at the natural seams:
//!
//! - `primordial` — `VmState` creation: heap, universe, primordial classes,
//!   stubs. Untouched by any boot-source change.
//! - `parse`      — `parser::parse_file`/`parse_top_items`: source → AST.
//!   (`parse_file` is hooked as a whole; `parse_top_items` never runs inside
//!   it, so the two never double-count.)
//! - `classdef`   — `classdef::install_class_def`, WHOLE: class shape +
//!   method dictionary + every method compile within.
//! - `methods`    — `codegen::compile_method`, a strict SUBSET of `classdef`;
//!   the report derives `class shape = classdef − methods`.
//! - `doits`      — `classdef::execute_do_it`: compile AND run. A bytecode
//!   boot still runs doits, so this share is kept, not saved.
//! - `world`      — the whole `load_world`/`load_world_from_image` span; the
//!   report derives `other = world − (parse+classdef+doits)` (file I/O, list
//!   handling, the `Smalltalk startUp` send).
//!
//! `report` PRINTS AND RESETS, because one process can boot several VMs (the
//! Cocoa GUI boots the UI worker and the primary): each boot reports its own
//! numbers. Counters are process-global relaxed atomics — two VMs booting
//! CONCURRENTLY would blend, which no current caller does; a diagnostic
//! accepts that over threading a context through every parse call.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

static PRIMORDIAL_NS: AtomicU64 = AtomicU64::new(0);
static PARSE_NS: AtomicU64 = AtomicU64::new(0);
static CLASSDEF_NS: AtomicU64 = AtomicU64::new(0);
static METHOD_NS: AtomicU64 = AtomicU64::new(0);
static DOIT_NS: AtomicU64 = AtomicU64::new(0);
static CLASSES: AtomicU64 = AtomicU64::new(0);
static METHODS: AtomicU64 = AtomicU64::new(0);
static DOITS: AtomicU64 = AtomicU64::new(0);

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("MACVM_BOOT_TIMING").is_some())
}

/// Times `f` into `slot` (with an optional count bump) when enabled;
/// otherwise runs `f` with zero overhead beyond the `enabled()` load.
#[inline]
fn timed<R>(slot: &AtomicU64, count: Option<&AtomicU64>, f: impl FnOnce() -> R) -> R {
    if !enabled() {
        return f();
    }
    let t0 = Instant::now();
    let r = f();
    slot.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
    if let Some(c) = count {
        c.fetch_add(1, Ordering::Relaxed);
    }
    r
}

pub fn primordial<R>(f: impl FnOnce() -> R) -> R {
    timed(&PRIMORDIAL_NS, None, f)
}
pub fn parse<R>(f: impl FnOnce() -> R) -> R {
    timed(&PARSE_NS, None, f)
}
pub fn classdef<R>(f: impl FnOnce() -> R) -> R {
    timed(&CLASSDEF_NS, Some(&CLASSES), f)
}
pub fn method<R>(f: impl FnOnce() -> R) -> R {
    timed(&METHOD_NS, Some(&METHODS), f)
}
pub fn doit<R>(f: impl FnOnce() -> R) -> R {
    timed(&DOIT_NS, Some(&DOITS), f)
}

/// Print the breakdown for the boot that just finished, against the caller's
/// measured whole-boot span, then zero every counter for the next boot.
pub fn report(label: &str, world_ns: u64) {
    if !enabled() {
        return;
    }
    let take = |a: &AtomicU64| a.swap(0, Ordering::Relaxed);
    let (prim, parse, classdef, methods, doits) = (
        take(&PRIMORDIAL_NS),
        take(&PARSE_NS),
        take(&CLASSDEF_NS),
        take(&METHOD_NS),
        take(&DOIT_NS),
    );
    let (n_cls, n_m, n_d) = (take(&CLASSES), take(&METHODS), take(&DOITS));
    let shape = classdef.saturating_sub(methods);
    let other = world_ns.saturating_sub(parse + classdef + doits);
    let total = prim + world_ns;
    let ms = |ns: u64| ns as f64 / 1.0e6;
    let pct = |ns: u64| {
        if total == 0 {
            0.0
        } else {
            ns as f64 * 100.0 / total as f64
        }
    };
    eprintln!("[boot-timing] {label}: total {:.1} ms", ms(total));
    eprintln!(
        "[boot-timing]   primordial   {:>8.1} ms  {:>5.1}%   (VM: heap/universe/stubs)",
        ms(prim),
        pct(prim)
    );
    eprintln!(
        "[boot-timing]   parse        {:>8.1} ms  {:>5.1}%",
        ms(parse),
        pct(parse)
    );
    eprintln!(
        "[boot-timing]   class shape  {:>8.1} ms  {:>5.1}%   ({n_cls} classes)",
        ms(shape),
        pct(shape)
    );
    eprintln!(
        "[boot-timing]   methods      {:>8.1} ms  {:>5.1}%   ({n_m} compiles)  <- the bytecode-boot prize",
        ms(methods),
        pct(methods)
    );
    eprintln!(
        "[boot-timing]   doits        {:>8.1} ms  {:>5.1}%   ({n_d} doits, compile+run)",
        ms(doits),
        pct(doits)
    );
    eprintln!(
        "[boot-timing]   other        {:>8.1} ms  {:>5.1}%   (I/O, lists, startUp)",
        ms(other),
        pct(other)
    );
}
