//! `VmState` — the one god-struct threaded as `&mut VmState` throughout the
//! VM (CONVENTIONS §2: no statics/globals). S1 gives it a `Universe` and the
//! options env parsing produces; later sprints add process/stack state.

use std::collections::HashSet;
use std::io::Write;
use std::time::Instant;

use crate::codecache::nmethod::CodeTable;
use crate::codecache::{CodeCache, DEFAULT_CODE_CACHE_CAPACITY};
use crate::interpreter::stack::{ProcessStack, DEFAULT_STACK_CAPACITY};
use crate::memory::Universe;
use crate::oops::wrappers::MethodOop;
use crate::runtime::lookup::LookupCache;

/// `VmState`'s fixed-offset prefix (S10 D6) — see `oops::layout`'s
/// `VMREG_*_OFFSET` consts, the single source of truth compiled code's
/// direct `[x28, #N]` loads/stores bake in (`vmregblock_offsets_pinned`
/// below asserts the two never drift apart). Only `poll_flag` is live in
/// S10 (the `Poll` Ir op reads it, D5.3); the rest are laid out now so the
/// offsets S11 compiles against are already stable.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VmRegBlock {
    /// S11 inline alloc.
    pub eden_top: u64,
    /// S11 inline alloc.
    pub eden_end: u64,
    /// S11 write barrier.
    pub old_start: u64,
    /// S11 write barrier: `cards − (old_start >> 9)`.
    pub card_base_biased: u64,
    /// Nonzero => an interrupt/trace poll is due (S10 `Poll` Ir op reads
    /// this; nothing sets it nonzero yet — mirrors `VmState::pending`'s own
    /// pre-S10 status as a wired-but-inert hook). nonzero comparison, not a
    /// specific sentinel value, so `cbnz` (not a full compare) suffices.
    pub poll_flag: u32,
    _pad: u32,
    /// S11 runtime entries: the anchor frame-pointer/return-pc pair native
    /// stack-walking code reads when re-entering Rust from compiled code.
    pub last_compiled_fp: u64,
    pub last_compiled_pc: u64,
}

impl VmRegBlock {
    pub fn new() -> VmRegBlock {
        VmRegBlock {
            eden_top: 0,
            eden_end: 0,
            old_start: 0,
            card_base_biased: 0,
            poll_flag: 0,
            _pad: 0,
            last_compiled_fp: 0,
            last_compiled_pc: 0,
        }
    }
}

impl Default for VmRegBlock {
    fn default() -> Self {
        Self::new()
    }
}

/// `MACVM_JIT` (CONVENTIONS §3). `Off` never triggers `compile_method`
/// regardless of invocation counts; `Threshold(n)` compiles a method the
/// moment its invocation counter reaches `n` (SPEC §8.1's
/// `InvocationCounterLimit`) — `n = 1` is the differential-testing gate
/// (tests_s10.md gate item 1): every eligible method compiles on its first
/// send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JitMode {
    Off,
    Threshold(u32),
}

impl JitMode {
    /// Provisional (SPRINTS.md standing rule 3: S10 has no perf gate beyond
    /// a 2× tripwire) — plausible enough to exercise real compilation in an
    /// ordinary run without recompiling on every send; not perf-tuned.
    pub const DEFAULT_THRESHOLD: u32 = 1000;
}

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
    /// `MACVM_GC_STRESS=full[:N]` (S8 step 8, SPEC §12.3 item 4): run a full
    /// mark-slide-compact collection every `N` allocations through the same
    /// choke point (default `N` = 100 — `Some(100)` when the env var is
    /// exactly `"full"` with no suffix). Mutually exclusive with `gc_stress`
    /// (`=1` scavenges every allocation; `=full` full-GCs every Nth) — the
    /// harness that flushes out the reference-rewrite class of bug
    /// `for_each_root`'s S8-8 fix closed (found by running real source, not
    /// by the scavenger-only `=1` mode, which never calls `full_gc` at all).
    pub gc_stress_full_period: Option<u64>,
    /// `MACVM_EDEN=<KiB>` (S7 A1 step 3's "tests" override): eden size in
    /// KiB, overriding `layout::DEFAULT_EDEN_SIZE`. `None` keeps the
    /// default — used to shrink eden in tests so a scavenge is reachable
    /// without allocating megabytes of filler first.
    pub eden_kb: Option<usize>,
    /// `MACVM_JIT=off|threshold=N` (S10, CONVENTIONS §3). Unset => `Off` —
    /// deliberately NOT the SPEC-intent "on by default" reading: hundreds
    /// of pre-S10 tests were written and verified against pure-interpreter
    /// behavior, and `tests/common/mod.rs`'s `test_vm()` starts from
    /// `VmOptions::from_env()` precisely so a gate script's env var (same
    /// pattern as `MACVM_GC_STRESS`) reaches every in-process test VM —
    /// defaulting this on would silently let ambient shell state change
    /// unrelated test behavior. `just gate-s10` opts in explicitly.
    pub jit: JitMode,
}

impl VmOptions {
    pub const DEFAULT_HEAP_MIB: usize = 8192;
    /// `MACVM_GC_STRESS=full` with no `:N` suffix — SPEC §12.3's own
    /// tunable default period.
    pub const DEFAULT_FULL_STRESS_PERIOD: u64 = 100;

    /// Pure parse of `MACVM_GC_STRESS`'s raw value into the mutually-
    /// exclusive `(gc_stress, gc_stress_full_period)` pair — split out from
    /// `from_env` so it's unit-testable without touching the real
    /// environment (`tests/common/mod.rs`'s own doc comment: env mutation
    /// races across the multi-threaded test runner, so tests only ever
    /// READ it). `None` (the var unset) yields `(false, None)`: stress off.
    fn parse_gc_stress(raw: Option<&str>) -> (bool, Option<u64>) {
        let Some(s) = raw.map(str::trim) else {
            return (false, None);
        };
        if s == "1" {
            return (true, None);
        }
        let period = s.strip_prefix("full").and_then(|rest| match rest {
            "" => Some(Self::DEFAULT_FULL_STRESS_PERIOD),
            _ => rest.strip_prefix(':').and_then(|n| n.parse::<u64>().ok()),
        });
        (false, period)
    }

    /// Pure parse of `MACVM_JIT`'s raw value (same testability rationale as
    /// `parse_gc_stress`). Unset => `Off` (see `jit`'s own doc comment for
    /// why). An unrecognized value warns and falls back to `Off` too — a
    /// typo'd flag must never silently turn compilation on.
    fn parse_jit(raw: Option<&str>) -> JitMode {
        let Some(s) = raw.map(str::trim) else {
            return JitMode::Off;
        };
        if s == "off" {
            return JitMode::Off;
        }
        if let Some(n) = s
            .strip_prefix("threshold=")
            .and_then(|n| n.parse::<u32>().ok())
        {
            return JitMode::Threshold(n);
        }
        eprintln!("MACVM_JIT: unrecognized value {s:?}, defaulting to off");
        JitMode::Off
    }

    pub fn from_env() -> VmOptions {
        let heap_mib = std::env::var("MACVM_HEAP")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(Self::DEFAULT_HEAP_MIB);
        let trace = std::env::var("MACVM_TRACE")
            .ok()
            .map(|s| TraceFlags::parse(&s))
            .unwrap_or_default();
        let raw_gc_stress = std::env::var("MACVM_GC_STRESS").ok();
        let (gc_stress, gc_stress_full_period) = Self::parse_gc_stress(raw_gc_stress.as_deref());
        let eden_kb = std::env::var("MACVM_EDEN")
            .ok()
            .and_then(|s| s.parse::<usize>().ok());
        let jit = Self::parse_jit(std::env::var("MACVM_JIT").ok().as_deref());
        VmOptions {
            heap_mib,
            trace,
            gc_stress,
            gc_stress_full_period,
            eden_kb,
            jit,
        }
    }
}

impl Default for VmOptions {
    fn default() -> Self {
        VmOptions {
            heap_mib: Self::DEFAULT_HEAP_MIB,
            trace: TraceFlags::default(),
            gc_stress: false,
            gc_stress_full_period: None,
            eden_kb: None,
            jit: JitMode::Off,
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

/// `#[repr(C)]` (S10 D6): `reg_block` MUST be the first field, at
/// `VmState`'s own byte offset 0, so x28 (established `&VmState` by the
/// call stub) reaches its fields directly — see `VmRegBlock`'s own doc.
/// Every other field keeps ordinary (unspecified) layout; only the prefix
/// is pinned.
#[repr(C)]
pub struct VmState {
    pub reg_block: VmRegBlock,
    pub universe: Universe,
    pub options: VmOptions,
    pub stack: ProcessStack,
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
    /// The one `MAP_JIT` region tier-1 compiled methods and their stubs
    /// live in (S10 D6). Reserved unconditionally in [`VmState::with_options`]
    /// regardless of `options.jit` — one `mmap`, nothing written until the
    /// first real `publish`.
    pub code_cache: CodeCache,
    /// Installed nmethods, keyed by `(receiver klass, selector)` (S10 D6,
    /// D8). Empty for the lifetime of a `JitMode::Off` run.
    pub code_table: CodeTable,
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
            reg_block: VmRegBlock::new(),
            universe,
            options,
            stack: ProcessStack::with_capacity(DEFAULT_STACK_CAPACITY),
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
            code_cache: CodeCache::new(DEFAULT_CODE_CACHE_CAPACITY)
                .expect("VmState::with_options: failed to reserve JIT code cache"),
            code_table: CodeTable::new(),
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

    #[test]
    fn gc_stress_unset_means_off() {
        assert_eq!(VmOptions::parse_gc_stress(None), (false, None));
    }

    #[test]
    fn gc_stress_1_scavenges_every_alloc() {
        assert_eq!(VmOptions::parse_gc_stress(Some("1")), (true, None));
    }

    #[test]
    fn gc_stress_full_uses_default_period() {
        assert_eq!(
            VmOptions::parse_gc_stress(Some("full")),
            (false, Some(VmOptions::DEFAULT_FULL_STRESS_PERIOD))
        );
    }

    #[test]
    fn gc_stress_full_with_explicit_period() {
        assert_eq!(
            VmOptions::parse_gc_stress(Some("full:50")),
            (false, Some(50))
        );
    }

    #[test]
    fn gc_stress_modes_are_mutually_exclusive() {
        // "1" never also sets a full-GC period, and "full[:N]" never also
        // sets the scavenge-every-alloc bool -- every valid value picks
        // exactly one mode, matching alloc_words' cascade (which checks
        // both but only one is ever Some/true for a given env value).
        let (scavenge, full) = VmOptions::parse_gc_stress(Some("full:7"));
        assert!(!scavenge);
        assert_eq!(full, Some(7));
    }

    #[test]
    fn gc_stress_garbage_value_is_off() {
        // Neither "1" nor "full"-prefixed, and "full:" with an unparseable
        // suffix -- both must fail closed (stress off), never panic or
        // silently pick a default the user didn't ask for.
        assert_eq!(VmOptions::parse_gc_stress(Some("nonsense")), (false, None));
        assert_eq!(
            VmOptions::parse_gc_stress(Some("full:notanumber")),
            (false, None)
        );
        assert_eq!(VmOptions::parse_gc_stress(Some("")), (false, None));
    }

    #[test]
    fn gc_stress_trims_whitespace() {
        assert_eq!(
            VmOptions::parse_gc_stress(Some(" full:12 ")),
            (false, Some(12))
        );
        assert_eq!(VmOptions::parse_gc_stress(Some(" 1 ")), (true, None));
    }

    /// tests_s10.md's `jit_flag_parsing`: `off`, `threshold=1`,
    /// `threshold=10000`, absent, and garbage all resolve to the correct
    /// `JitMode` (garbage falls back to `Off`, same as `parse_gc_stress`'s
    /// own precedent — never silently pick a mode the user didn't ask for).
    #[test]
    fn jit_flag_parsing() {
        assert_eq!(VmOptions::parse_jit(None), JitMode::Off);
        assert_eq!(VmOptions::parse_jit(Some("off")), JitMode::Off);
        assert_eq!(
            VmOptions::parse_jit(Some("threshold=1")),
            JitMode::Threshold(1)
        );
        assert_eq!(
            VmOptions::parse_jit(Some("threshold=10000")),
            JitMode::Threshold(10000)
        );
        assert_eq!(VmOptions::parse_jit(Some("nonsense")), JitMode::Off);
        assert_eq!(VmOptions::parse_jit(Some("threshold=")), JitMode::Off);
        assert_eq!(VmOptions::parse_jit(Some("threshold=abc")), JitMode::Off);
    }

    /// S10 D6: every `VmRegBlock` field's real, compiler-computed offset
    /// matches the `oops::layout::VMREG_*_OFFSET` constant compiled code's
    /// direct `[x28, #N]` accesses bake in — the two are hand-kept in sync
    /// (there's no `#[derive(offset_consts)]`), so this is the tripwire
    /// that catches them drifting apart.
    #[test]
    fn vmregblock_offsets_pinned() {
        use crate::oops::layout::{
            VMREG_BLOCK_SIZE, VMREG_CARD_BASE_BIASED_OFFSET, VMREG_EDEN_END_OFFSET,
            VMREG_EDEN_TOP_OFFSET, VMREG_LAST_COMPILED_FP_OFFSET, VMREG_LAST_COMPILED_PC_OFFSET,
            VMREG_OLD_START_OFFSET, VMREG_POLL_FLAG_OFFSET,
        };
        assert_eq!(
            std::mem::offset_of!(VmRegBlock, eden_top),
            VMREG_EDEN_TOP_OFFSET
        );
        assert_eq!(
            std::mem::offset_of!(VmRegBlock, eden_end),
            VMREG_EDEN_END_OFFSET
        );
        assert_eq!(
            std::mem::offset_of!(VmRegBlock, old_start),
            VMREG_OLD_START_OFFSET
        );
        assert_eq!(
            std::mem::offset_of!(VmRegBlock, card_base_biased),
            VMREG_CARD_BASE_BIASED_OFFSET
        );
        assert_eq!(
            std::mem::offset_of!(VmRegBlock, poll_flag),
            VMREG_POLL_FLAG_OFFSET
        );
        assert_eq!(
            std::mem::offset_of!(VmRegBlock, last_compiled_fp),
            VMREG_LAST_COMPILED_FP_OFFSET
        );
        assert_eq!(
            std::mem::offset_of!(VmRegBlock, last_compiled_pc),
            VMREG_LAST_COMPILED_PC_OFFSET
        );
        assert_eq!(std::mem::size_of::<VmRegBlock>(), VMREG_BLOCK_SIZE);
        // D6's other half: reg_block must sit at VmState's own offset 0, or
        // x28 == &VmState would not equal x28 == &VmState.reg_block.
        assert_eq!(std::mem::offset_of!(VmState, reg_block), 0);
    }
}
