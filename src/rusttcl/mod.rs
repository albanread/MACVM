//! RUSTTCL — MACVM's live VM-introspection shell, built on the vendored
//! [`crate::vendor::rust_tcl`] Tcl implementation. Not part of the
//! Smalltalk image or the SPEC-numbered sprint sequence: this is a
//! debugging tool, in the same spirit as the `MACVM_TRACE`/`MACVM_DBG_*`
//! env-var channels it complements, except queryable interactively and
//! scriptable with real control flow (`if`/`while`/`foreach`/`proc`)
//! instead of fixed at process start.
//!
//! [`verbs::register_macvm_verbs`] adds `disasm`/`methods`/`nmethods`/
//! `ic`/`stats`/`trace`/`load`/`help`/`quit` on top of
//! `rust_tcl::Registry::with_core()`'s ordinary Tcl verb set. Every one of
//! those closures needs to reach the live [`crate::runtime::vm_state::
//! VmState`] they're introspecting, but `Registry::register`'s handler
//! type is `Fn(..) -> .. + Send + Sync + 'static` — no lifetime-bound
//! capture of `&mut VmState` is possible. [`bridge`] resolves this the
//! same way MACVM's own compiled-code runtime stubs already do
//! ([`crate::codecache::stubs`]'s `unsafe extern "C" fn foo(vm: *mut
//! VmState, ..)`): a raw pointer, valid only for the scope of one
//! dispatch, reconstituted with a documented safety contract rather than
//! threaded through as a typed borrow.

mod bridge;
mod verbs;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::oops::wrappers::{KlassOop, MethodOop};
use crate::runtime::vm_state::VmState;
use crate::vendor::rust_tcl::{compile, Registry, Vm};

/// One shell session: the VM it's introspecting, the world directory it
/// booted from, and the quit flag the `quit`/`exit` verbs set (checked by
/// `run_repl`/`run_script` after every dispatched line).
pub struct RusttclCtx {
    pub vm: VmState,
    pub world_dir: PathBuf,
    pub quit: bool,
}

impl RusttclCtx {
    /// Boots a fresh `VmState` from the ambient environment (matching
    /// `macvm run`/`macvm repl` — so `MACVM_JIT`, `MACVM_TRACE`,
    /// `MACVM_GC_STRESS`, etc. all apply exactly as they would to a
    /// normal run) and loads `world_dir`, warning (not failing) if it has
    /// no `world.list` — an empty-world shell is still useful for
    /// genesis-klass introspection (`methods Object`, etc.).
    pub fn new(world_dir: PathBuf) -> RusttclCtx {
        let mut vm = VmState::new();
        match crate::frontend::world::load_world(&mut vm, &world_dir) {
            Ok(true) => {}
            Ok(false) => eprintln!(
                "warning: no world.list found at {} — continuing without a world",
                world_dir.display()
            ),
            Err(e) => eprintln!("{e}"),
        }
        RusttclCtx {
            vm,
            world_dir,
            quit: false,
        }
    }
}

/// Resolves a bare class name (e.g. `TaskControlBlock`) to its `KlassOop`
/// via the globals table — the same binding `Object subclass: Foo [...]`
/// installs and `classdef.rs`'s own superclass resolution reads
/// ([`crate::runtime::globals::global_lookup`]). Shared by every verb that
/// takes a class-name argument.
pub fn resolve_klass(ctx: &mut RusttclCtx, name: &str) -> Result<KlassOop, String> {
    let sym = ctx.vm.universe.intern(name.as_bytes());
    let assoc = crate::runtime::globals::global_lookup(&ctx.vm, sym)
        .ok_or_else(|| format!("no such global: {name}"))?;
    let value = crate::oops::wrappers::MemOop::try_from(assoc)
        .expect("global association is a mem oop")
        .body_oop(1);
    KlassOop::try_from(value).ok_or_else(|| format!("{name} is not a class"))
}

/// Resolves `selector` on `klass` via the real method-lookup walk (so an
/// inherited method resolves exactly as a real send would), not a
/// same-class-only method-dictionary probe.
pub fn resolve_method(
    ctx: &mut RusttclCtx,
    klass: KlassOop,
    selector: &str,
) -> Result<MethodOop, String> {
    let sel = ctx.vm.universe.intern(selector.as_bytes());
    crate::runtime::lookup::lookup(&mut ctx.vm, klass, sel)
        .ok_or_else(|| format!("{selector} not understood by this class or its superclasses"))
}

/// Builds the one `Registry` a RUSTTCL session uses: every core Tcl verb
/// (`set`/`if`/`while`/`foreach`/`proc`/`list`/`dict`/`expr`/...) plus
/// MACVM's own introspection verbs layered on top.
fn build_registry() -> Registry {
    let mut registry = Registry::with_core();
    verbs::register_macvm_verbs(&mut registry);
    registry
}

/// Tracks unescaped, unquoted `{`/`[` nesting depth across an
/// accumulating multi-line buffer — the REPL's "is this enough of a
/// command to try compiling yet" gate. rust-tcl's own lexer treats an
/// unterminated brace/bracket/quote as a hard error (no distinguishable
/// "need more input" signal, unlike the Smalltalk parser's own `eof`
/// flag — see `main.rs::cmd_repl`), so this heuristic runs BEFORE ever
/// calling `compile`, keeping a `proc`/`if`/`while` block typed across
/// several physical lines from ever reaching the lexer half-finished.
/// Deliberately simple (Tcl's real brace/backslash lexing has more
/// corners than this counts) — an exotic edge case just falls through to
/// a reported Lex error and a cleared buffer, not a shell crash.
fn brace_depth(text: &str) -> i32 {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next(); // skip whatever's escaped, brace or not
            }
            '"' => in_string = !in_string,
            '{' | '[' if !in_string => depth += 1,
            '}' | ']' if !in_string => depth -= 1,
            _ => {}
        }
    }
    depth
}

/// `macvm rusttcl [--world <dir>]`: prompts `rusttcl> ` (or `...> ` while
/// a brace-balanced command is still being typed across several lines),
/// runs each complete chunk against one persistent Tcl `Vm` (so `set`
/// variables and `proc`s survive across input lines, matching a real
/// `tclsh` session), and prints results. EOF (Ctrl-D) or the `quit`/`exit`
/// verb ends the session.
pub fn run_repl(ctx: &mut RusttclCtx) {
    let registry = build_registry();
    let mut tcl_vm = Vm::new(&registry);
    let mut output_shown = 0usize;
    let stdin = std::io::stdin();
    let mut buf = String::new();
    loop {
        print!("{}", if buf.is_empty() { "rusttcl> " } else { "...> " });
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => break,
        }
        buf.push_str(&line);
        if brace_depth(&buf) > 0 {
            continue; // still accumulating a multi-line block
        }
        let chunk = std::mem::take(&mut buf);
        run_chunk_with_registry(ctx, &mut tcl_vm, &registry, &mut output_shown, &chunk);
        if ctx.quit {
            break;
        }
    }
}

/// The non-interactive counterpart to `run_repl`: runs every line of
/// `path` in sequence against one persistent Tcl `Vm` — a saved
/// diagnostic recipe (e.g. "load this repro, dump its nmethods,
/// disassemble the one method I care about") replayed with one shell
/// invocation. Lines are joined with the same brace-depth gate `run_repl`
/// uses, so a multi-line `proc`/`if`/`while` in the script file works
/// identically to typing it interactively.
pub fn run_script(ctx: &mut RusttclCtx, path: &Path) -> std::io::Result<()> {
    let registry = build_registry();
    let mut tcl_vm = Vm::new(&registry);
    let mut output_shown = 0usize;
    let text = std::fs::read_to_string(path)?;
    let mut buf = String::new();
    for line in text.lines() {
        buf.push_str(line);
        buf.push('\n');
        if brace_depth(&buf) > 0 {
            continue;
        }
        let chunk = std::mem::take(&mut buf);
        run_chunk_with_registry(ctx, &mut tcl_vm, &registry, &mut output_shown, &chunk);
        if ctx.quit {
            break;
        }
    }
    Ok(())
}

/// Runs one chunk and prints its result. `output_shown` is how much of
/// `tcl_vm`'s cumulative `puts`-style output was already printed by a
/// PREVIOUS call — the vendored `Vm`'s own output buffer (`vm.rs`'s
/// `write`/`write_line`) accumulates for the session's whole lifetime and
/// is never cleared between `run()` calls (upstream's own `eval()`
/// convenience function never notices, since it builds a fresh `Vm` every
/// call; reusing one persistent `Vm` across a whole REPL session, so
/// `set` variables and `proc`s survive between input lines, is what
/// exposes this). Printing only the slice past `output_shown` each time —
/// instead of the whole buffer `RunResult.output` hands back — is what
/// stops every later command from re-printing the entire session's
/// history.
///
/// A chunk that errors mid-way (e.g. a loop body's first statement
/// succeeds, its second doesn't) leaves whatever it already wrote sitting
/// in that buffer — `Vm::run`'s `?` on `execute` returns bare `Err` with
/// no `RunResult` to read in the `Err` arm below, and `Vm`'s own output
/// field is private (no public accessor to read it directly either). So
/// `output_shown` is left UNADVANCED on error; the next SUCCESSFUL call's
/// slice naturally includes that hidden partial output concatenated
/// ahead of its own — correct content, just surfaced one call later than
/// the write that produced it, rather than lost.
fn run_chunk_with_registry(
    ctx: &mut RusttclCtx,
    tcl_vm: &mut Vm<'_>,
    registry: &Registry,
    output_shown: &mut usize,
    source: &str,
) {
    if source.trim().is_empty() {
        return;
    }
    let program = match compile(source, registry) {
        Ok(p) => p,
        Err(e) => {
            println!("{e}");
            let _ = std::io::stdout().flush();
            return;
        }
    };
    let result = bridge::with_ctx_active(ctx, || tcl_vm.run(&program));
    match result {
        Ok(r) => {
            print!("{}", &r.output[*output_shown..]);
            if !r.value.as_str().is_empty() {
                println!("{}", r.value);
            }
            *output_shown = r.output.len();
        }
        Err(e) => println!("{e}"),
    }
    let _ = std::io::stdout().flush();
}
