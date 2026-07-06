//! MACVM entry point (placeholder).
//!
//! The VM is at the scaffold stage; this just proves the crate builds and
//! links. Hidden test hooks observed via a real subprocess (integration
//! tests can't otherwise see this process's own exit code or exhaustive
//! stderr output): `--selftest-alloc-loop` allocates rooted objects until
//! the heap is genuinely exhausted (`tests/it_memory.rs::eden_exhaustion_aborts`);
//! `--selftest-stack-overflow` pushes until the process stack is exhausted
//! (`tests/it_interp.rs::process_stack_overflow_exits_cleanly`);
//! `--selftest-trace-diamond` runs the k_diamond kernel under
//! `MACVM_TRACE=bytecode` so the caller can count emitted trace lines
//! (`tests/it_interp.rs::trace_mode_line_count`); `--selftest-dnu-fallback`
//! sends an unrecognized selector with no `doesNotUnderstand:` installed
//! anywhere, exercising `runtime::error::dnu_fallback`'s pinned stdout
//! format and its real `exit(1)`.

use std::io::{BufRead, Write as _};
use std::path::{Path, PathBuf};

use macvm::bytecode::BytecodeBuilder;
use macvm::memory::alloc;
use macvm::oops::smi::SmallInt;
use macvm::oops::Oop;
use macvm::runtime::{VmOptions, VmState};

fn main() {
    if std::env::args().any(|a| a == "--selftest-alloc-loop") {
        selftest_alloc_loop();
    }
    if std::env::args().any(|a| a == "--selftest-stack-overflow") {
        selftest_stack_overflow();
    }
    if std::env::args().any(|a| a == "--selftest-trace-diamond") {
        selftest_trace_diamond();
    }
    if std::env::args().any(|a| a == "--selftest-dnu-fallback") {
        selftest_dnu_fallback();
    }
    if std::env::args().any(|a| a == "--selftest-probe-assert") {
        selftest_probe_crash(ProbeCrashKind::Assert);
    }
    if std::env::args().any(|a| a == "--selftest-probe-segv") {
        selftest_probe_crash(ProbeCrashKind::Segv);
    }
    if std::env::args().any(|a| a == "--selftest-probe-foreign") {
        selftest_probe_foreign();
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("run") => cmd_run(&args[1..]),
        Some("repl") => cmd_repl(&args[1..]),
        Some("rusttcl") => cmd_rusttcl(&args[1..]),
        _ => println!("MACVM — Self/Strongtalk-lineage research VM (arm64). Scaffold only."),
    }
}

/// `--world <dir>` parsing shared by `run`/`repl`; any other args are
/// returned as the positional leftovers (`run`'s `<file.mst>`).
fn parse_world_flag(args: &[String]) -> (Option<PathBuf>, Vec<String>) {
    let mut world_dir = None;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--world" {
            i += 1;
            world_dir = args.get(i).map(PathBuf::from);
        } else {
            rest.push(args[i].clone());
        }
        i += 1;
    }
    (world_dir, rest)
}

fn load_world_with_warning(vm: &mut VmState, world_dir: &Path) {
    match macvm::frontend::world::load_world(vm, world_dir) {
        Ok(true) => {}
        Ok(false) => eprintln!(
            "warning: no world.list found at {} — continuing without a world",
            world_dir.display()
        ),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

/// `macvm run <file.mst> [--world <dir>]` (SPEC §3.2, `sprint_s05_detail.md`
/// §Design "CLI"). Exit 0 unless a compile error / uncaught VM error.
fn cmd_run(args: &[String]) {
    let (world_dir, rest) = parse_world_flag(args);
    let Some(file) = rest.first() else {
        eprintln!("usage: macvm run <file.mst> [--world <dir>]");
        std::process::exit(2);
    };
    let mut vm = VmState::new();
    load_world_with_warning(
        &mut vm,
        &world_dir.unwrap_or_else(|| PathBuf::from("world")),
    );

    let result = macvm::frontend::world::load_file(&mut vm, Path::new(file));
    print_bytecode_count(&vm);
    print_gc_bridge_stats(&vm);
    print_vm_stats(&vm);
    match result {
        Ok(()) => std::process::exit(vm.exit_code.unwrap_or(0)),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

/// `MACVM_TRACE=count` (S6 PERF procedure) — printed to stderr so golden
/// stdout transcripts (fib/sieve/point_demo) stay exact regardless of
/// whether the flag is set.
fn print_bytecode_count(vm: &VmState) {
    if vm.options.trace.is_enabled("count") {
        eprintln!("bytecodes: {}", vm.bytecode_count);
    }
}

/// `MACVM_TRACE=stats` (S15 A8): the full counter dump at process exit, to
/// stderr (golden stdout transcripts stay exact), grep-friendly one line per
/// counter. Code-cache byte totals are computed here from the live tables
/// rather than counted incrementally (they are exact by construction and
/// cost nothing off this path).
fn print_vm_stats(vm: &VmState) {
    if !vm.options.trace.is_enabled("stats") {
        return;
    }
    eprintln!("{}", macvm::runtime::vm_state::format_vm_stats(vm));
}

/// `MACVM_TRACE=gc`: a grep-friendly one-line counter summary printed to
/// stderr at process exit, mirroring `print_bytecode_count`'s own
/// convention. S12 step 7 inverted its meaning (P10): under S11's D8
/// bridge a shell recipe asserted `gc_under_compiled=0` (the bridge
/// held); with the bridge deleted the same counter is the proof the hard
/// case — a collection with live compiled frames on the native stack —
/// genuinely ran (`just bridge-stats-s11` now asserts it is > 0 under the
/// combined stress gate). `bridge_old_allocs` is gone with the bridge.
fn print_gc_bridge_stats(vm: &VmState) {
    if vm.options.trace.is_enabled("gc") {
        eprintln!(
            "gc: gc_under_compiled={}",
            vm.universe.gc_stats.gc_under_compiled
        );
    }
}

/// `macvm repl [--world <dir>]`: prompts `mst> `, accumulates lines until a
/// complete statement parses (an "unexpected EOF" parse error keeps
/// reading; any other error reports and resets the buffer), executes each
/// complete doIt, and prints its result via `printString` if understood,
/// else the Rust `print_oop` fallback (pre-S6 worlds).
fn cmd_repl(args: &[String]) {
    let (world_dir, _rest) = parse_world_flag(args);
    let mut vm = VmState::new();
    load_world_with_warning(
        &mut vm,
        &world_dir.unwrap_or_else(|| PathBuf::from("world")),
    );

    let stdin = std::io::stdin();
    let mut buf = String::new();
    loop {
        print!("{}", if buf.is_empty() { "mst> " } else { "...> " });
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => break,
        }
        buf.push_str(&line);

        match macvm::frontend::parser::parse_one_top_item(&buf) {
            Ok(None) => buf.clear(),
            Ok(Some(item)) => {
                buf.clear();
                match macvm::frontend::classdef::execute_top_item(&mut vm, item) {
                    Ok(Some(result)) => println!("{}", print_result(&mut vm, result)),
                    Ok(None) => {}
                    Err(e) => println!("{e}"),
                }
            }
            Err(e) if e.eof => {} // keep buffering
            Err(e) => {
                println!("{e}");
                buf.clear();
            }
        }
    }
}

/// `macvm rusttcl [--world <dir>] [script.tcl]`: the live VM-introspection
/// shell (see `macvm::rusttcl`'s module doc) — `disasm`/`methods`/
/// `nmethods`/`ic`/`stats`/`trace`/`load`/`help`, plus the full vendored
/// Tcl language for scripting them. A positional script path runs
/// non-interactively (one shell invocation replaying a saved diagnostic
/// recipe); with none, it's an interactive `rusttcl> ` prompt.
fn cmd_rusttcl(args: &[String]) {
    let (world_dir, rest) = parse_world_flag(args);
    let mut ctx =
        macvm::rusttcl::RusttclCtx::new(world_dir.unwrap_or_else(|| PathBuf::from("world")));
    match rest.first() {
        Some(script) => {
            if let Err(e) = macvm::rusttcl::run_script(&mut ctx, Path::new(script)) {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        None => macvm::rusttcl::run_repl(&mut ctx),
    }
}

fn print_result(vm: &mut VmState, result: Oop) -> String {
    let klass = macvm::runtime::lookup::klass_of(vm, result);
    let sel = vm.universe.intern(b"printString");
    if let Some(m) = macvm::runtime::lookup::lookup(vm, klass, sel) {
        let s = macvm::interpreter::run_method(vm, m, result, &[]);
        if let Some(b) = macvm::oops::wrappers::ByteArrayOop::try_from(s) {
            let mut bytes = Vec::new();
            b.copy_bytes_out(&mut bytes);
            return String::from_utf8_lossy(&bytes).into_owned();
        }
    }
    macvm::memory::print_oop(&vm.universe, result)
}

/// Allocates rooted (process-stack-pushed) arrays until the heap is
/// genuinely exhausted (S7-10: with a real scavenger wired into the
/// allocation choke point, unrooted garbage would just get reclaimed
/// forever and this would hang instead of exiting — `klass` is re-read
/// from `vm.universe` every iteration rather than captured once outside
/// the loop, since a bare local can go stale across the scavenges this
/// loop now triggers).
fn selftest_alloc_loop() -> ! {
    let mut vm = VmState::new();
    loop {
        let klass = vm.universe.array_klass;
        let arr = alloc::alloc_indexable_oops(&mut vm, klass, 1000);
        vm.stack.push(arr.oop());
    }
}

fn selftest_stack_overflow() -> ! {
    let mut vm = VmState::new();
    let v = SmallInt::new(0).oop();
    loop {
        vm.stack.push(v);
    }
}

fn selftest_trace_diamond() -> ! {
    let mut vm = VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: macvm::runtime::TraceFlags::parse("bytecode"),
        gc_stress: false,
        gc_stress_full_period: None,
        eden_kb: None,
        jit: macvm::runtime::JitMode::Off,
    });
    let mut b = BytecodeBuilder::new();
    let l1 = b.new_label();
    let l2 = b.new_label();
    b.push_self();
    b.br_false_fwd(l1);
    b.push_smi_i8(1);
    b.jump_fwd(l2);
    b.bind(l1);
    b.push_smi_i8(2);
    b.bind(l2);
    b.ret_tos();
    let sel = vm.universe.intern(b"diamond");
    let m = b.finish(&mut vm, sel, 0, 0);
    let true_obj = vm.universe.true_obj;
    let _ = macvm::interpreter::run_method(&mut vm, m, true_obj, &[]);
    std::process::exit(0)
}

fn selftest_dnu_fallback() -> ! {
    let mut vm = VmState::new();
    let object_klass = vm.universe.object_klass;
    let sel = vm.universe.intern(b"bar");
    let mut b = BytecodeBuilder::new();
    b.push_temp(0);
    b.send(&mut vm, sel, 0);
    b.ret_tos();
    let caller_sel = vm.universe.intern(b"caller");
    let caller = b.finish(&mut vm, caller_sel, 1, 0);
    let recv = alloc::alloc_slots(&mut vm, object_klass).oop();
    let nil = vm.universe.nil_obj;
    let _ = macvm::interpreter::run_method(&mut vm, caller, nil, &[recv]);
    unreachable!("dnu_fallback must have exited the process");
}

/// DBG0 gates (docs/DEBUGGER.md §6): which planted crash a
/// `--selftest-probe-*` flag drives through the PROBE dossier machinery.
enum ProbeCrashKind {
    /// A hand-published blob whose first instruction is `brk #0xDE02` —
    /// the compiled-assert trigger.
    Assert,
    /// A hand-published blob that loads from address 0 — a SIGSEGV whose
    /// pc is inside the registered code cache.
    Segv,
}

/// Publish a tiny crashing blob into a JIT VM's code cache and invoke it
/// through the real call stub (which establishes the x28 = &VmState
/// invariant the PROBE handlers rely on). The dossier exits 70; reaching
/// the end of this function is the failure mode.
fn selftest_probe_crash(kind: ProbeCrashKind) -> ! {
    use macvm::compiler::assembler::{imm, mem, x, Assembler};
    use macvm::compiler::jasm_assembler::JasmAssembler;

    let mut vm = VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: Default::default(),
        gc_stress: false,
        gc_stress_full_period: None,
        eden_kb: None,
        jit: macvm::runtime::JitMode::Threshold(1),
    });

    let mut a = JasmAssembler::new();
    match kind {
        ProbeCrashKind::Assert => {
            macvm::codecache::deopt_trap::emit_brk(
                &mut a,
                macvm::codecache::deopt_trap::TRAP_ASSERT,
            );
        }
        ProbeCrashKind::Segv => {
            a.emit("movz", &[x(16), imm(0)]);
            a.emit("ldr", &[x(0), mem(16, 0)]); // load from address 0 → SIGSEGV
        }
    }
    let blob = a.finish();
    let h = vm
        .code_cache
        .alloc(blob.code.len())
        .expect("selftest-probe: code cache alloc");
    vm.code_cache.publish(h, &blob);
    let entry = h.base as u64;
    let nil = vm.universe.nil_obj.raw();
    let stubs = vm.stubs;
    let _ = stubs.invoke(entry, &mut vm, &[nil]);
    eprintln!("selftest-probe: crash did not fire (BUG)");
    std::process::exit(1);
}

/// A fault whose pc is OUTSIDE every registered code cache — plain Rust
/// null-page read. PROBE must print only the one-line FOREIGN verdict and
/// let the default disposition kill the process (killed-by-signal, not
/// exit 70). The JIT VM exists solely to arm the handlers.
fn selftest_probe_foreign() -> ! {
    let vm = VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: Default::default(),
        gc_stress: false,
        gc_stress_full_period: None,
        eden_kb: None,
        jit: macvm::runtime::JitMode::Threshold(1),
    });
    let _ = &vm;
    // SAFETY deliberately violated — this selftest IS the crash. The
    // address is computed through a volatile read of a runtime value so
    // neither rustc nor clippy can prove (or lint) the dereference away.
    unsafe {
        let addr: usize = std::ptr::read_volatile(&8usize);
        std::ptr::read_volatile(addr as *const u64);
    }
    eprintln!("selftest-probe-foreign: read of address 8 did not fault (BUG)");
    std::process::exit(1);
}
