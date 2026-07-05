//! Vendored `rust-tcl`: a small dependency-free Tcl implementation
//! (lexer/parser/sema/CFG/bytecode/VM), extensible via
//! [`registry::Registry::register`]. See `VENDOR.md` for the exact source
//! commit and per-file edits (a single mechanical `crate::` path rewrite,
//! nothing else).
//!
//! `src/rusttcl/` is the only consumer outside this tree — it registers
//! MACVM-introspection verbs against a [`registry::Registry`] and drives
//! [`eval`]/[`compile`] from `macvm rusttcl`'s REPL and script-file modes.

// Vendored/derived code (this whole tree) keeps upstream's original style
// per VENDOR.md's "byte-for-byte copy" rule — these are the three
// specific clippy lints that style trips (an `Option::map_or(true, ..)`
// in `registry.rs`, a manual `% 2` parity check in `verbs.rs`'s `dict`
// argument-count guard, and a redundant closure wrapping `Value::new` in
// `vm.rs`'s default-argument handling), not a blanket suppression for
// anything MACVM writes fresh under this tree.
#![allow(
    clippy::unnecessary_map_or,
    clippy::manual_is_multiple_of,
    clippy::redundant_closure
)]

pub mod ast;
pub mod bytecode;
pub mod cfg;
pub mod codegen;
pub mod error;
pub mod expr;
pub mod lexer;
pub mod list_value;
pub mod parser;
pub mod registry;
pub mod sema;
pub mod span;
pub mod value;
pub mod verbs;
pub mod vm;

pub use bytecode::{Instr, Program};
pub use error::{Error, ErrorKind, Result};
pub use registry::{Arity, Registry, VerbId};
pub use value::Value;
pub use vm::{Flow, RunResult, Vm};

pub fn compile(source: &str, registry: &Registry) -> Result<Program> {
    let tokens = lexer::lex(source)?;
    let ast = parser::parse(&tokens)?;
    let bound = sema::analyze(&ast, registry)?;
    let cfg = cfg::build(&bound);
    codegen::compile(&cfg, registry)
}

pub fn eval(source: &str, registry: &Registry) -> Result<RunResult> {
    let program = compile(source, registry)?;
    Vm::new(registry).run(&program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_to_direct_verb_calls() {
        let registry = Registry::with_core();
        let program = compile("set x 1\nadd $x 2", &registry).unwrap();
        assert!(program
            .instructions
            .iter()
            .any(|instr| matches!(instr, Instr::Call(_, 2))));
    }

    #[test]
    fn runs_variables_and_command_substitution() {
        let registry = Registry::with_core();
        let result = eval("set x [add 20 22]\nputs \"answer=$x\"\nset x", &registry).unwrap();
        assert_eq!(result.output, "answer=42\n");
        assert_eq!(result.value.as_str(), "42");
    }

    #[test]
    fn braced_words_are_literal() {
        let registry = Registry::with_core();
        let result = eval("set x 3\nputs {$x [add 1 2]}", &registry).unwrap();
        assert_eq!(result.output, "$x [add 1 2]\n");
    }

    /// MACVM addition (not upstream — see `verbs.rs`'s own comment at the
    /// registration site and `VENDOR.md`): `incr` defaults to step 1 and to
    /// treating a never-`set` variable as starting at 0.
    #[test]
    fn incr_defaults_to_step_one_and_zero_base() {
        let registry = Registry::with_core();
        let result = eval("incr x\nincr x\nincr x 5\nset x", &registry).unwrap();
        assert_eq!(result.value.as_str(), "7");
    }

    #[test]
    fn extension_verbs_are_registered_once_and_called_by_id() {
        let mut registry = Registry::with_core();
        registry.register("twice", Arity::exact(1), |_, args| {
            Ok(Value::new(format!("{}{}", args[0], args[0])))
        });

        let result = eval("set x [twice ab]\nset x", &registry).unwrap();
        assert_eq!(result.value.as_str(), "abab");
    }

    #[test]
    fn unknown_commands_are_runtime_errors_for_tcl_style_proc_dispatch() {
        let registry = Registry::with_core();
        let err = eval("missing 1 2", &registry).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Runtime);
        assert!(err.message.contains("unknown command"));
    }

    #[test]
    fn dynamic_command_names_follow_tcl_surface_syntax() {
        let registry = Registry::with_core();
        let result = eval("set cmd puts\n$cmd hi", &registry).unwrap();
        assert_eq!(result.output, "hi\n");
    }

    #[test]
    fn if_compiles_to_conditional_jump_and_runs_else() {
        let registry = Registry::with_core();
        let source = "set x 7\nif {$x < 3} {set y small} else {set y big}\nset y";
        let program = compile(source, &registry).unwrap();
        assert!(program
            .instructions
            .iter()
            .any(|instr| matches!(instr, Instr::JumpIfFalse(_))));
        let result = Vm::new(&registry).run(&program).unwrap();
        assert_eq!(result.value.as_str(), "big");
    }

    #[test]
    fn while_uses_compiled_expr_and_jumps() {
        let registry = Registry::with_core();
        let result = eval(
            "set x 0\nwhile {$x < 5} {set x [expr {$x + 1}]}\nset x",
            &registry,
        )
        .unwrap();
        assert_eq!(result.value.as_str(), "5");
    }

    #[test]
    fn foreach_iterates_tcl_lists_with_break_and_continue() {
        let registry = Registry::with_core();
        let result = eval(
            "set out {}\nforeach x [list 1 2 3 4] {
                if {$x == 2} {continue}
                if {$x == 4} {break}
                lappend out $x
             }\nset out",
            &registry,
        )
        .unwrap();
        assert_eq!(result.value.as_str(), "1 3");
    }

    #[test]
    fn proc_defines_compiled_user_command_with_default_arguments() {
        let registry = Registry::with_core();
        let result = eval(
            "proc add2 {x {y 2}} {return [expr {$x + $y}]}\nadd2 40",
            &registry,
        )
        .unwrap();
        assert_eq!(result.value.as_str(), "42");
    }

    #[test]
    fn list_verbs_preserve_tcl_string_representation() {
        let registry = Registry::with_core();
        let result = eval(
            "set xs [list alpha {two words} omega]\nputs [llength $xs]\nlindex $xs 1",
            &registry,
        )
        .unwrap();
        assert_eq!(result.output, "3\n");
        assert_eq!(result.value.as_str(), "two words");
    }

    #[test]
    fn dict_verbs_use_tcl_key_value_list_representation() {
        let registry = Registry::with_core();
        let result = eval(
            "set d [dict create name Ada role engineer]\ndict set d role lead\ndict get $d role",
            &registry,
        )
        .unwrap();
        assert_eq!(result.value.as_str(), "lead");
    }

    #[test]
    fn upvar_aliases_a_caller_variable() {
        let registry = Registry::with_core();
        let result = eval(
            "proc bump {varName} {
                upvar $varName v
                set v [expr {$v + 1}]
             }
             set n 41
             bump n
             set n",
            &registry,
        )
        .unwrap();
        assert_eq!(result.value.as_str(), "42");
    }

    #[test]
    fn upvar_can_alias_multiple_variables() {
        let registry = Registry::with_core();
        let result = eval(
            "proc swap {a b} {
                upvar $a x $b y
                set tmp $x
                set x $y
                set y $tmp
             }
             set left red
             set right blue
             swap left right
             list $left $right",
            &registry,
        )
        .unwrap();
        assert_eq!(result.value.as_str(), "blue red");
    }

    #[test]
    fn upvar_absolute_global_aliases_global_scope() {
        let registry = Registry::with_core();
        let result = eval(
            "set g 1
             proc setg {} {
                upvar #0 g local
                set local 9
             }
             setg
             set g",
            &registry,
        )
        .unwrap();
        assert_eq!(result.value.as_str(), "9");
    }

    #[test]
    fn upvar_aliases_work_with_list_mutation() {
        let registry = Registry::with_core();
        let result = eval(
            "proc push {var item} {
                upvar $var xs
                lappend xs $item
             }
             set xs {}
             push xs alpha
             push xs {two words}
             lindex $xs 1",
            &registry,
        )
        .unwrap();
        assert_eq!(result.value.as_str(), "two words");
    }

    #[test]
    fn uplevel_executes_in_caller_scope() {
        let registry = Registry::with_core();
        let result = eval(
            "set x original
             proc change {} {
                uplevel {set x changed}
             }
             change
             set x",
            &registry,
        )
        .unwrap();
        assert_eq!(result.value.as_str(), "changed");
    }

    #[test]
    fn uplevel_accepts_multiword_script_arguments() {
        let registry = Registry::with_core();
        let result = eval(
            "set x original
             proc change {} {
                uplevel set x changed
             }
             change
             set x",
            &registry,
        )
        .unwrap();
        assert_eq!(result.value.as_str(), "changed");
    }

    #[test]
    fn uplevel_absolute_global_executes_in_global_scope() {
        let registry = Registry::with_core();
        let result = eval(
            "set g global
             proc f {} {
                set g local
                uplevel #0 {set g changed}
                return $g
             }
             list [f] $g",
            &registry,
        )
        .unwrap();
        assert_eq!(result.value.as_str(), "local changed");
    }

    #[test]
    fn uplevel_return_propagates_through_current_proc() {
        let registry = Registry::with_core();
        let result = eval(
            "proc f {} {
                uplevel {return 9}
                return 0
             }
             f",
            &registry,
        )
        .unwrap();
        assert_eq!(result.value.as_str(), "9");
    }

    #[test]
    fn proc_local_scope_does_not_leak() {
        let registry = Registry::with_core();
        let err = eval(
            "proc f {} {set local inside}
             f
             set local",
            &registry,
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Runtime);
        assert!(err.message.contains("unknown variable"));
    }

    #[test]
    fn dynamic_proc_command_names_work() {
        let registry = Registry::with_core();
        let result = eval(
            "proc greet {name} {return \"hi $name\"}
             set cmd greet
             $cmd Ada",
            &registry,
        )
        .unwrap();
        assert_eq!(result.value.as_str(), "hi Ada");
    }

    #[test]
    fn elseif_and_then_syntax_is_supported() {
        let registry = Registry::with_core();
        let result = eval(
            "set x 10
             if {$x < 10} then {set y small} elseif {$x == 10} then {set y exact} else {set y large}
             set y",
            &registry,
        )
        .unwrap();
        assert_eq!(result.value.as_str(), "exact");
    }

    #[test]
    fn while_break_and_continue_work_together() {
        let registry = Registry::with_core();
        let result = eval(
            "set x 0
             set out {}
             while {$x < 10} {
                set x [expr {$x + 1}]
                if {$x == 2} {continue}
                if {$x == 5} {break}
                lappend out $x
             }
             set out",
            &registry,
        )
        .unwrap();
        assert_eq!(result.value.as_str(), "1 3 4");
    }

    #[test]
    fn foreach_empty_list_returns_empty_string() {
        let registry = Registry::with_core();
        let result = eval(
            "set touched no
             foreach x {} {set touched yes}
             set touched",
            &registry,
        )
        .unwrap();
        assert_eq!(result.value.as_str(), "no");
    }

    #[test]
    fn expr_obeys_precedence_and_boolean_short_circuit() {
        let registry = Registry::with_core();
        let result = eval("expr {1 + 2 * 3 == 7 && 0 || 4 > 2}", &registry).unwrap();
        assert_eq!(result.value.as_str(), "1");
    }

    #[test]
    fn lrange_supports_end_index() {
        let registry = Registry::with_core();
        let result = eval("lrange [list a b c d] 1 end", &registry).unwrap();
        assert_eq!(result.value.as_str(), "b c d");
    }

    #[test]
    fn dict_exists_keys_and_values_are_supported() {
        let registry = Registry::with_core();
        let result = eval(
            "set d [dict create name Ada role engineer]
             list [dict exists $d role] [dict keys $d] [dict values $d]",
            &registry,
        )
        .unwrap();
        assert_eq!(result.value.as_str(), "1 {name role} {Ada engineer}");
    }

    #[test]
    fn dict_get_without_key_returns_the_dictionary() {
        let registry = Registry::with_core();
        let result = eval("dict get [dict create a 1 b 2]", &registry).unwrap();
        assert_eq!(result.value.as_str(), "a 1 b 2");
    }

    #[test]
    fn procedure_arity_errors_are_runtime_errors() {
        let registry = Registry::with_core();
        let err = eval("proc one {x} {return $x}\none", &registry).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Runtime);
        assert!(err.message.contains("procedure `one`"));
    }

    #[test]
    fn break_outside_loop_is_a_compile_time_error() {
        let registry = Registry::with_core();
        let err = compile("break", &registry).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Sema);
        assert!(err.message.contains("outside loop"));
    }

    #[test]
    fn uplevel_bad_level_reports_runtime_error() {
        let registry = Registry::with_core();
        let err = eval("uplevel 9 {set x no}", &registry).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Runtime);
        assert!(err.message.contains("scope level"));
    }

    #[test]
    fn upvar_bad_level_reports_runtime_error() {
        let registry = Registry::with_core();
        let err = eval("upvar 9 x y", &registry).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Runtime);
        assert!(err.message.contains("scope level"));
    }
}
