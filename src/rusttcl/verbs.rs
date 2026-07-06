//! The MACVM introspection verbs registered on top of `rust_tcl::Registry
//! ::with_core()`'s ordinary Tcl verb set. Each closure's first line is
//! `bridge::active_ctx()` — see `bridge.rs` for why that's the only way
//! to reach the live `VmState`.
//!
//! There's no way to enumerate an already-built `Registry`'s verbs from
//! the outside (`registry.rs`'s `names`/`verbs` fields are private, by
//! design — see its own module doc on why this tree stays byte-identical
//! to upstream). So `TABLE` below is this file's own record of what it
//! registers, used both to build the `Registry` and to answer `help`.

use crate::rusttcl::bridge;
use crate::vendor::rust_tcl::{Arity, Error as TclError, Registry, Result as TclResult, Value, Vm};

struct VerbDoc {
    name: &'static str,
    usage: &'static str,
    help: &'static str,
}

const TABLE: &[VerbDoc] = &[
    VerbDoc {
        name: "disasm",
        usage: "disasm <Class> <selector>",
        help: "Disassemble one compiled method's bytecode.",
    },
    VerbDoc {
        name: "methods",
        usage: "methods <Class>",
        help: "List a class's own method dictionary: selector, argc, ntemps, primitive.",
    },
    VerbDoc {
        name: "nmethods",
        usage: "nmethods",
        help: "List every nmethod in the JIT code table: id, state, version, key klass>>selector, trap count, frame slots, code size.",
    },
    VerbDoc {
        name: "ic",
        usage: "ic <Class> <selector>",
        help: "Dump every send site's inline-cache state in one method: bci, selector, Empty/Mono/Poly/Mega, and the resolved klass(es).",
    },
    VerbDoc {
        name: "stats",
        usage: "stats",
        help: "Print the full VM counter dump on demand (same counters as MACVM_TRACE=stats' exit-time dump).",
    },
    VerbDoc {
        name: "trace",
        usage: "trace [channel] [on|off]",
        help: "No args: list enabled MACVM_TRACE channels. One arg: report whether it's on. Two args: enable/disable it live.",
    },
    VerbDoc {
        name: "flag",
        usage: "flag [name] [value]",
        help: "No args: list every VM flag and its current value. One arg: report one flag. Two args: set it live — same grammar as its MACVM_* env var. Flags: jit (off|threshold=N), gc_stress (0|1|full|full:N), deopt_stress (0|N), dbg_oop (0x<hex>|off).",
    },
    VerbDoc {
        name: "load",
        usage: "load <file.mst>",
        help: "Compile and run a .mst file into the current VM (its classes become visible to disasm/methods/ic/nmethods afterward).",
    },
    VerbDoc {
        name: "dbg",
        usage: "dbg [on|off]",
        help: "No args: report whether the HALT debugger is armed. on/off: arm/disarm it live (docs/DEBUGGER.md DBG1). While armed, breakpoints, `error:`, and DNU halt into the (halt) command loop instead of dying.",
    },
    VerbDoc {
        name: "bp",
        usage: "bp <Class> <selector> <bci>",
        help: "Set a HALT breakpoint (side-table, method pinned to tier-0; existing nmethods invalidated through the redefinition path). Use \"Class class\" for the metaclass side. Arms dbg.",
    },
    VerbDoc {
        name: "bp-clear",
        usage: "bp-clear <Class> <selector> <bci>",
        help: "Clear one breakpoint; the method's tier-up eligibility is restored once its last breakpoint is gone.",
    },
    VerbDoc {
        name: "bp-list",
        usage: "bp-list",
        help: "List every live breakpoint as Class>>selector @bci.",
    },
    VerbDoc {
        name: "ring",
        usage: "ring",
        help: "Dump the PROBE recent-history ring (DBG0): the last N compile / deopt / invalidate events, oldest first — the crash dossier's step 9, on demand.",
    },
    VerbDoc {
        name: "help",
        usage: "help [verb]",
        help: "List every RUSTTCL verb, or show one verb's full help text.",
    },
    VerbDoc {
        name: "quit",
        usage: "quit | exit",
        help: "End the RUSTTCL session.",
    },
];

pub fn register_macvm_verbs(registry: &mut Registry) {
    registry.register("disasm", Arity::exact(2), verb_disasm);
    registry.register("methods", Arity::exact(1), verb_methods);
    registry.register("nmethods", Arity::exact(0), verb_nmethods);
    registry.register("ic", Arity::exact(2), verb_ic);
    registry.register("stats", Arity::exact(0), verb_stats);
    registry.register("trace", Arity::range(0, 2), verb_trace);
    registry.register("flag", Arity::range(0, 2), verb_flag);
    registry.register("load", Arity::exact(1), verb_load);
    registry.register("dbg", Arity::range(0, 1), verb_dbg);
    registry.register("bp", Arity::exact(3), verb_bp);
    registry.register("bp-clear", Arity::exact(3), verb_bp_clear);
    registry.register("bp-list", Arity::exact(0), verb_bp_list);
    registry.register("ring", Arity::exact(0), verb_ring);
    registry.register("help", Arity::range(0, 1), verb_help);
    registry.register("quit", Arity::exact(0), verb_quit);
    registry.register("exit", Arity::exact(0), verb_quit);
}

fn verb_disasm(_vm: &mut Vm<'_>, args: &[Value]) -> TclResult<Value> {
    let ctx = bridge::active_ctx();
    let klass = super::resolve_klass(ctx, args[0].as_str()).map_err(TclError::runtime)?;
    let method = super::resolve_method(ctx, klass, args[1].as_str()).map_err(TclError::runtime)?;
    Ok(Value::new(crate::bytecode::disassemble(
        &ctx.vm.universe,
        method,
    )))
}

fn verb_methods(_vm: &mut Vm<'_>, args: &[Value]) -> TclResult<Value> {
    let ctx = bridge::active_ctx();
    let klass = super::resolve_klass(ctx, args[0].as_str()).map_err(TclError::runtime)?;
    let dict =
        crate::oops::method_dict::MethodDictOop::try_from(klass.methods()).ok_or_else(|| {
            TclError::runtime(format!("{} has no method dictionary", args[0].as_str()))
        })?;
    let mut rows: Vec<String> = Vec::new();
    dict.each_pair(&ctx.vm, |sel, m| {
        rows.push(format!(
            "{} argc={} ntemps={} primitive={}",
            sel.as_string(),
            m.argc(),
            m.ntemps(),
            m.primitive()
        ));
    });
    rows.sort();
    if rows.is_empty() {
        rows.push("(no methods)".to_string());
    }
    Ok(Value::new(rows.join("\n")))
}

fn verb_nmethods(_vm: &mut Vm<'_>, _args: &[Value]) -> TclResult<Value> {
    let ctx = bridge::active_ctx();
    let mut rows: Vec<String> = Vec::new();
    for nm in ctx.vm.code_table.iter_all() {
        let klass_name = crate::memory::print_oop(&ctx.vm.universe, nm.key_klass.name());
        rows.push(format!(
            "nm={} state={:?} v{} {klass_name}>>{} trap_count={} frame_slots={} code_bytes={}",
            nm.id.0,
            nm.state,
            nm.version,
            nm.key_selector.as_string(),
            nm.trap_count,
            nm.frame_slots,
            nm.code.len
        ));
    }
    if rows.is_empty() {
        rows.push("(no nmethods compiled yet)".to_string());
    }
    Ok(Value::new(rows.join("\n")))
}

fn verb_ic(_vm: &mut Vm<'_>, args: &[Value]) -> TclResult<Value> {
    let ctx = bridge::active_ctx();
    let klass = super::resolve_klass(ctx, args[0].as_str()).map_err(TclError::runtime)?;
    let method = super::resolve_method(ctx, klass, args[1].as_str()).map_err(TclError::runtime)?;

    let mut rows: Vec<String> = Vec::new();
    let mut bci = 0usize;
    while bci < method.bytecode_len() {
        let (instr, next) = crate::bytecode::decode_at(method, bci);
        if let crate::bytecode::Instr::Send { ic, super_ } = instr {
            let icv = crate::interpreter::ic::InterpreterIc::at(method, ic);
            let sel_str = icv.selector().as_string();
            let state = crate::interpreter::ic::ic_state(method, ic);
            let mut row = format!("@{bci} ic={ic} super={super_} sel={sel_str} state={state:?}");
            match state {
                crate::interpreter::ic::IcState::Mono => {
                    if let Some(k) = crate::oops::wrappers::KlassOop::try_from(icv.guard()) {
                        row.push_str(&format!(
                            " klass={}",
                            crate::memory::print_oop(&ctx.vm.universe, k.name())
                        ));
                    }
                }
                crate::interpreter::ic::IcState::Poly(n) => {
                    if let Some(pairs) = crate::oops::wrappers::ArrayOop::try_from(icv.target()) {
                        for i in 0..n as usize {
                            if let Some(k) =
                                crate::oops::wrappers::KlassOop::try_from(pairs.at(2 * i))
                            {
                                row.push_str(&format!(
                                    " {}",
                                    crate::memory::print_oop(&ctx.vm.universe, k.name())
                                ));
                            }
                        }
                    }
                }
                crate::interpreter::ic::IcState::Empty | crate::interpreter::ic::IcState::Mega => {}
            }
            rows.push(row);
        }
        bci = next;
    }
    if rows.is_empty() {
        rows.push("(no send sites)".to_string());
    }
    Ok(Value::new(rows.join("\n")))
}

fn verb_stats(_vm: &mut Vm<'_>, _args: &[Value]) -> TclResult<Value> {
    let ctx = bridge::active_ctx();
    Ok(Value::new(crate::runtime::vm_state::format_vm_stats(
        &ctx.vm,
    )))
}

fn verb_trace(_vm: &mut Vm<'_>, args: &[Value]) -> TclResult<Value> {
    let ctx = bridge::active_ctx();
    match args {
        [] => {
            let channels = ctx.vm.options.trace.list();
            if channels.is_empty() {
                Ok(Value::new("(no trace channels enabled)"))
            } else {
                Ok(Value::new(channels.join(" ")))
            }
        }
        [channel] => {
            let on = ctx.vm.options.trace.is_enabled(channel.as_str());
            Ok(Value::new(if on { "on" } else { "off" }))
        }
        [channel, setting] => match setting.as_str() {
            "on" => {
                ctx.vm.options.trace.enable(channel.as_str());
                Ok(Value::empty())
            }
            "off" => {
                ctx.vm.options.trace.disable(channel.as_str());
                Ok(Value::empty())
            }
            other => Err(TclError::runtime(format!(
                "trace: expected on|off, got {other:?}"
            ))),
        },
        _ => unreachable!("Arity::range(0, 2) already rejects more than 2 args"),
    }
}

/// Every flag `flag` knows how to get/set. One name per operationally
/// live-tunable `VmOptions`/`VmState` field — deliberately NOT
/// `heap_mib`/`eden_kb` (sized once at `VmState::new()`; mutating the
/// field after construction wouldn't resize anything already allocated).
const FLAG_NAMES: &[&str] = &["jit", "gc_stress", "deopt_stress", "dbg_oop"];

fn verb_flag(_vm: &mut Vm<'_>, args: &[Value]) -> TclResult<Value> {
    let ctx = bridge::active_ctx();
    match args {
        [] => {
            let rows: Vec<String> = FLAG_NAMES
                .iter()
                .map(|n| {
                    format!(
                        "{n}={}",
                        flag_get(ctx, n).expect("name drawn from FLAG_NAMES")
                    )
                })
                .collect();
            Ok(Value::new(rows.join(" ")))
        }
        [name] => flag_get(ctx, name.as_str()).map(Value::new).ok_or_else(|| {
            TclError::runtime(format!(
                "unknown flag {:?} — try `flag` for the list",
                name.as_str()
            ))
        }),
        [name, value] => {
            flag_set(ctx, name.as_str(), value.as_str())?;
            Ok(Value::empty())
        }
        _ => unreachable!("Arity::range(0, 2) already rejects more than 2 args"),
    }
}

fn flag_get(ctx: &mut super::RusttclCtx, name: &str) -> Option<String> {
    Some(match name {
        "jit" => match ctx.vm.options.jit {
            crate::runtime::JitMode::Off => "off".to_string(),
            crate::runtime::JitMode::Threshold(n) => format!("threshold={n}"),
        },
        "gc_stress" => match (
            ctx.vm.options.gc_stress,
            ctx.vm.options.gc_stress_full_period,
        ) {
            (true, _) => "1".to_string(),
            (false, Some(n)) => format!("full:{n}"),
            (false, None) => "0".to_string(),
        },
        "deopt_stress" => {
            if ctx.vm.deopt_stress {
                ctx.vm.stress_period.to_string()
            } else {
                "0".to_string()
            }
        }
        "dbg_oop" => match ctx.vm.dbg_oop {
            Some(addr) => format!("{addr:#x}"),
            None => "off".to_string(),
        },
        _ => return None,
    })
}

/// Same grammar as each flag's `MACVM_*` env var (`VmOptions::parse_jit`/
/// `parse_gc_stress`/`VmState::parse_deopt_stress` — the exact parse
/// functions `from_env`/`new` call, `pub(crate)` for this reuse) — so
/// `flag jit threshold=1` reads identically to `MACVM_JIT=threshold=1`.
fn flag_set(ctx: &mut super::RusttclCtx, name: &str, value: &str) -> TclResult<()> {
    match name {
        "jit" => {
            ctx.vm.options.jit = crate::runtime::vm_state::VmOptions::parse_jit(Some(value));
            Ok(())
        }
        "gc_stress" => {
            let (on, period) = crate::runtime::vm_state::VmOptions::parse_gc_stress(Some(value));
            ctx.vm.options.gc_stress = on;
            ctx.vm.options.gc_stress_full_period = period;
            Ok(())
        }
        "deopt_stress" => {
            let (on, period) = crate::runtime::vm_state::VmState::parse_deopt_stress(Some(value));
            ctx.vm.deopt_stress = on;
            ctx.vm.stress_period = period;
            ctx.vm.stress_countdown = period; // re-arm the live counter too, not just the config
            Ok(())
        }
        "dbg_oop" => {
            if value == "off" || value == "none" {
                ctx.vm.dbg_oop = None;
            } else {
                let s = value.trim();
                let s = s.strip_prefix("0x").unwrap_or(s);
                let addr = usize::from_str_radix(s, 16).map_err(|_| {
                    TclError::runtime(format!("dbg_oop: not a hex address: {value:?}"))
                })?;
                ctx.vm.dbg_oop = Some(addr);
            }
            Ok(())
        }
        other => Err(TclError::runtime(format!(
            "unknown flag {other:?} — try `flag` for the list"
        ))),
    }
}

fn verb_load(_vm: &mut Vm<'_>, args: &[Value]) -> TclResult<Value> {
    let ctx = bridge::active_ctx();
    let path = std::path::PathBuf::from(args[0].as_str());
    crate::frontend::world::load_file(&mut ctx.vm, &path)
        .map_err(|e| TclError::runtime(e.to_string()))?;
    Ok(Value::new(format!("loaded {}", path.display())))
}

fn verb_help(_vm: &mut Vm<'_>, args: &[Value]) -> TclResult<Value> {
    match args {
        [] => {
            let mut out = String::from("RUSTTCL verbs (`help <verb>` for detail; core Tcl verbs like set/if/while/proc are also available):\n");
            let mut seen = std::collections::HashSet::new();
            for v in TABLE {
                if !seen.insert(v.usage) {
                    continue;
                }
                out.push_str(&format!("  {:<28} {}\n", v.usage, v.help));
            }
            Ok(Value::new(out.trim_end().to_string()))
        }
        [name] => {
            // "exit" is `quit`'s alias (see `register_macvm_verbs`) and
            // isn't its own `TABLE` row — its usage text already reads
            // "quit | exit", so look that entry up under either name.
            let key = if name.as_str() == "exit" {
                "quit"
            } else {
                name.as_str()
            };
            match TABLE.iter().find(|v| v.name == key) {
                Some(v) => Ok(Value::new(format!("{}\n  {}", v.usage, v.help))),
                None => Err(TclError::runtime(format!(
                    "unknown verb {:?} — `help` lists them all",
                    name.as_str()
                ))),
            }
        }
        _ => unreachable!("Arity::range(0, 1) already rejects more than 1 arg"),
    }
}

fn verb_quit(_vm: &mut Vm<'_>, _args: &[Value]) -> TclResult<Value> {
    bridge::active_ctx().quit = true;
    Ok(Value::empty())
}

// ── DBG1 (docs/DEBUGGER.md): the debugger's TCL surface ─────────────────

fn verb_dbg(_vm: &mut Vm<'_>, args: &[Value]) -> TclResult<Value> {
    let ctx = bridge::active_ctx();
    match args {
        [] => Ok(Value::new(if ctx.vm.debug.active { "on" } else { "off" })),
        [setting] => match setting.as_str() {
            "on" => {
                ctx.vm.debug.active = true;
                Ok(Value::empty())
            }
            "off" => {
                ctx.vm.debug.active = false;
                Ok(Value::empty())
            }
            other => Err(TclError::runtime(format!(
                "dbg: expected on|off, got {other:?}"
            ))),
        },
        _ => unreachable!("Arity::range(0, 1) already rejects more than 1 arg"),
    }
}

fn verb_bp(_vm: &mut Vm<'_>, args: &[Value]) -> TclResult<Value> {
    let ctx = bridge::active_ctx();
    let bci: u16 = args[2].as_str().parse().map_err(|_| {
        TclError::runtime(format!("bp: bci must be a number: {:?}", args[2].as_str()))
    })?;
    crate::runtime::debug::set_breakpoint_by_name(
        &mut ctx.vm,
        args[0].as_str(),
        args[1].as_str(),
        bci,
    )
    .map(Value::new)
    .map_err(TclError::runtime)
}

fn verb_bp_clear(_vm: &mut Vm<'_>, args: &[Value]) -> TclResult<Value> {
    let ctx = bridge::active_ctx();
    let bci: u16 = args[2].as_str().parse().map_err(|_| {
        TclError::runtime(format!(
            "bp-clear: bci must be a number: {:?}",
            args[2].as_str()
        ))
    })?;
    crate::runtime::debug::clear_breakpoint_by_name(
        &mut ctx.vm,
        args[0].as_str(),
        args[1].as_str(),
        bci,
    )
    .map(Value::new)
    .map_err(TclError::runtime)
}

fn verb_bp_list(_vm: &mut Vm<'_>, _args: &[Value]) -> TclResult<Value> {
    let ctx = bridge::active_ctx();
    let rows = ctx.vm.debug.list();
    if rows.is_empty() {
        Ok(Value::new("(no breakpoints)"))
    } else {
        Ok(Value::new(rows.join("\n")))
    }
}

fn verb_ring(_vm: &mut Vm<'_>, _args: &[Value]) -> TclResult<Value> {
    let ctx = bridge::active_ctx();
    use crate::runtime::vm_state::ProbeEvent;
    let mut rows: Vec<String> = vec![format!(
        "showing last {} of {} events (oldest first)",
        ctx.vm.probe_ring.iter_oldest_first().count(),
        ctx.vm.probe_ring.total
    )];
    for e in ctx.vm.probe_ring.iter_oldest_first() {
        rows.push(match e {
            ProbeEvent::Compile { nm, version } => format!("compile   nm={nm} v{version}"),
            ProbeEvent::Deopt { nm, bci, reexecute } => {
                format!("deopt     nm={nm} bci={bci} reexecute={reexecute}")
            }
            ProbeEvent::Invalidate { nm } => format!("invalidate nm={nm}"),
        });
    }
    Ok(Value::new(rows.join("\n")))
}
