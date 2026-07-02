//! `VmState` — the one god-struct threaded as `&mut VmState` throughout the
//! VM (CONVENTIONS §2: no statics/globals). S1 gives it a `Universe` and the
//! options env parsing produces; later sprints add process/stack state.

use std::collections::HashSet;
use std::io::Write;
use std::time::Instant;

use crate::interpreter::stack::{ProcessStack, DEFAULT_STACK_CAPACITY};
use crate::memory::Universe;
use crate::oops::wrappers::MethodOop;
use crate::runtime::lookup::LookupCache;

/// Which `MACVM_TRACE` channels are enabled. The channel set is open-ended
/// (CONVENTIONS §3); S1 only stores membership, nothing reads it yet.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct TraceFlags {
    channels: HashSet<String>,
}

impl TraceFlags {
    pub fn parse(s: &str) -> TraceFlags {
        TraceFlags {
            channels: s
                .split(',')
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect(),
        }
    }

    pub fn is_enabled(&self, channel: &str) -> bool {
        self.channels.contains(channel)
    }
}

/// A `Write` sink backed by a shared `Vec<u8>` — what tests substitute for
/// `VmState::out` (stdout by default) so transcript assertions work without
/// a subprocess, before S5's golden-transcript runner exists. `Clone`s share
/// the same buffer (an `Arc<Mutex<_>>`), so a test can install one half via
/// `vm.out = Box::new(buf.clone())` and read the other after running.
#[derive(Clone, Default)]
pub struct OutputBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl OutputBuffer {
    pub fn new() -> OutputBuffer {
        OutputBuffer::default()
    }

    pub fn contents(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }

    pub fn as_string(&self) -> String {
        String::from_utf8_lossy(&self.contents()).into_owned()
    }
}

impl Write for OutputBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct VmOptions {
    /// Address-space reservation size, in MiB. Default 8192 (SPEC §7.1's
    /// tunable default), overridden by `MACVM_HEAP`.
    pub heap_mib: usize,
    pub trace: TraceFlags,
    /// `MACVM_GC_STRESS=1` (S7-10, SPEC §7.2 A2 step 1): scavenge before
    /// every allocation through the public choke point (`memory::alloc::
    /// alloc_words`), not just on eden exhaustion — the harness that flushes
    /// out invisible-root bugs (S7-9's `Handle` discipline) deterministically
    /// instead of waiting for an unlucky eden-boundary allocation.
    pub gc_stress: bool,
    /// `MACVM_EDEN=<KiB>` (S7 A1 step 3's "tests" override): eden size in
    /// KiB, overriding `layout::DEFAULT_EDEN_SIZE`. `None` keeps the
    /// default — used to shrink eden in tests so a scavenge is reachable
    /// without allocating megabytes of filler first.
    pub eden_kb: Option<usize>,
}

impl VmOptions {
    pub const DEFAULT_HEAP_MIB: usize = 8192;

    pub fn from_env() -> VmOptions {
        let heap_mib = std::env::var("MACVM_HEAP")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(Self::DEFAULT_HEAP_MIB);
        let trace = std::env::var("MACVM_TRACE")
            .ok()
            .map(|s| TraceFlags::parse(&s))
            .unwrap_or_default();
        let gc_stress = std::env::var("MACVM_GC_STRESS")
            .ok()
            .map(|s| s.trim() == "1")
            .unwrap_or(false);
        let eden_kb = std::env::var("MACVM_EDEN")
            .ok()
            .and_then(|s| s.parse::<usize>().ok());
        VmOptions {
            heap_mib,
            trace,
            gc_stress,
            eden_kb,
        }
    }
}

impl Default for VmOptions {
    fn default() -> Self {
        VmOptions {
            heap_mib: Self::DEFAULT_HEAP_MIB,
            trace: TraceFlags::default(),
            gc_stress: false,
            eden_kb: None,
        }
    }
}

/// The currently executing activation's bytecode position and method,
/// mirrored into `VmState` (S3, SPEC §5.3 design) so `interpreter::send`,
/// `runtime::lookup`, and `runtime::error` can read/write it without the
/// dispatch loop threading `method`/`bci` through every call. `fp`/`sp`
/// deliberately stay OUT of this struct and live only on `VmState::stack`
/// (`ProcessStack::fp`/`sp`) — mirroring them here would be a second source
/// of truth for the same state. `method` is `None` only before the first
/// activation of a freshly booted `VmState`; every send/DNU helper that
/// reads it is called while dispatch is active, so `.expect(...)` at the
/// read site documents that invariant.
#[derive(Copy, Clone, Default)]
pub struct InterpRegs {
    pub bci: usize,
    pub method: Option<MethodOop>,
}

pub struct VmState {
    pub universe: Universe,
    pub options: VmOptions,
    pub stack: ProcessStack,
    /// GC/interrupt poll flag, checked at `jump_back` (SPEC §5.5). A no-op
    /// hook in S2 (never set) so the dispatch loop's shape never changes
    /// once S7 wires a real poll behind it.
    pub pending: bool,
    /// The active send/DNU's view of "where execution currently is" (SPEC
    /// §5.3, S3). Written by the dispatch loop immediately before every
    /// `send`/`send_super`; read by `interpreter::send`'s super-lookup
    /// (`regs.method.holder()`) and by `runtime::error::print_stack_trace`
    /// for the top frame's `@<bci>`.
    pub regs: InterpRegs,
    /// The global IC dependency-versioning counter (SPEC §6.2, coarsened —
    /// see `sprint_s03_detail.md`'s SPEC-QUESTION). Bumped by every
    /// `install_method`; an IC entry is current iff its stamped epoch
    /// equals this. `debug_assert!(ic_epoch < 1 << 24)` at the bump site —
    /// wraparound is an accepted, unreachable-in-v1 ABA risk.
    pub ic_epoch: u32,
    pub lookup_cache: LookupCache,
    /// `sp - argc - 1` at the moment a primitive was entered — lets
    /// `VmState::prim_arg` re-read a live stack slot instead of the
    /// `[Oop; 6]` copy handed to the `PrimFn`, which primitives with
    /// `can_allocate = true` must do after their first allocating call
    /// (SPEC §10, the S7 choke-point pattern).
    pub prim_arg_base: usize,
    /// Where guest-visible output goes (`printOnStdout:`, S3's dev-hook
    /// primitive group). Stdout by default; tests substitute a `Vec<u8>` so
    /// transcript assertions don't need a subprocess before S5's golden
    /// runner exists.
    pub out: Box<dyn Write>,
    /// Set by the `quit`/`quit:` primitive; the dispatch loop exits (after
    /// the current activation returns) once this is true.
    pub exit_requested: bool,
    /// The process exit code `quit:` (S6) requested, if any — `None` means
    /// `quit` (0-arg) or no `quit` at all, both of which exit 0. Read by
    /// `main.rs`'s `cmd_run` after `world::load_file` returns.
    pub exit_code: Option<i32>,
    /// VM boot time, for the `millisecondClock` primitive.
    pub start_instant: Instant,
    /// Total bytecodes dispatched, counted only while `MACVM_TRACE=count` is
    /// enabled (S6 PERF procedure: cost is one `add` per dispatch,
    /// unconditionally compiled in — CONVENTIONS §3's channel list). Printed
    /// to stderr at process exit by `main.rs`.
    pub bytecode_count: u64,
    /// The next frame serial to hand out (SPEC §5.4, S4) — monotonic,
    /// post-incremented at every `push_frame`. Never reused, which is what
    /// makes a `HomeRef`'s `(fp, serial)` pair a reliable dead-home check:
    /// a frame popped and later replaced by a different activation at the
    /// same `fp` always has a different serial. u32 wrap after 4G pushes is
    /// an accepted, `debug_assert`-guarded risk.
    next_frame_serial: u32,
    /// `MACVM_DBG_OOP=<hex-addr>` (S7, lesson 11): the single-address trace
    /// hook, updated to the object's new address every time it moves.
    /// `None` unless set by [`VmState::new`] (which reads the env var);
    /// `with_options` always starts `None` — tests set it directly if they
    /// need it, avoiding the env-var-race concern `MACVM_HEAP`/
    /// `MACVM_TRACE` are parsed once for in `VmOptions::from_env`, not here.
    pub dbg_oop: Option<usize>,
    /// Scoped GC roots for oops held across an allocating call (S7-9, SPEC
    /// §7.6) — see `memory::handles`. Boxed so its address is stable across
    /// this struct moving; `HandleScope` points directly at the box's
    /// pointee.
    pub handle_arena: Box<crate::memory::handles::HandleArena>,
}

impl VmState {
    /// Parses options from the environment once and boots a fresh universe.
    pub fn new() -> VmState {
        let mut vm = Self::with_options(VmOptions::from_env());
        vm.dbg_oop = std::env::var("MACVM_DBG_OOP").ok().and_then(|s| {
            let s = s.trim();
            let s = s.strip_prefix("0x").unwrap_or(s);
            usize::from_str_radix(s, 16).ok()
        });
        vm
    }

    /// Bypasses env parsing — required by tests (parallel test runners
    /// cannot race on process-wide env vars) and by any later embedder.
    pub fn with_options(options: VmOptions) -> VmState {
        let universe = Universe::genesis(&options);
        let mut vm = VmState {
            universe,
            options,
            stack: ProcessStack::with_capacity(DEFAULT_STACK_CAPACITY),
            pending: false,
            regs: InterpRegs::default(),
            ic_epoch: 0,
            lookup_cache: LookupCache::new(),
            prim_arg_base: 0,
            out: Box::new(std::io::stdout()),
            exit_requested: false,
            exit_code: None,
            start_instant: Instant::now(),
            bytecode_count: 0,
            next_frame_serial: 0,
            dbg_oop: None,
            handle_arena: Box::new(crate::memory::handles::HandleArena::new()),
        };
        crate::runtime::globals::bootstrap_well_known(&mut vm);
        vm
    }

    /// Re-reads live stack slot `prim_arg_base + i` — the choke-point a
    /// `can_allocate` primitive must use instead of its `args` copy after
    /// any allocating call (SPEC §10 Pitfalls).
    pub fn prim_arg(&self, i: usize) -> crate::oops::Oop {
        self.stack.get(self.prim_arg_base + i)
    }

    /// Hands out the next frame serial, post-incrementing the counter.
    pub fn alloc_frame_serial(&mut self) -> u32 {
        let s = self.next_frame_serial;
        let (next, wrapped) = self.next_frame_serial.overflowing_add(1);
        debug_assert!(
            !wrapped,
            "alloc_frame_serial: wrapped past u32::MAX (accepted risk, 4G pushes)"
        );
        self.next_frame_serial = next;
        s
    }

    /// The serial `alloc_frame_serial` will hand out *next*, without
    /// consuming it — test-only, for predicting a to-be-pushed frame's
    /// serial before triggering the push.
    #[cfg(test)]
    pub(crate) fn peek_next_frame_serial(&self) -> u32 {
        self.next_frame_serial
    }
}

impl Default for VmState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_flags_parse() {
        let t = TraceFlags::parse("gc, jit,, bytecode");
        assert!(t.is_enabled("gc"));
        assert!(t.is_enabled("jit"));
        assert!(t.is_enabled("bytecode"));
        assert!(!t.is_enabled("ic"));
    }

    #[test]
    fn vm_options_default_heap() {
        let o = VmOptions::default();
        assert_eq!(o.heap_mib, VmOptions::DEFAULT_HEAP_MIB);
    }
}
