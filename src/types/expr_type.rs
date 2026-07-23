//! T2/T3 (`docs/typechecker_design.md` §4/§6): expression-type synthesis
//! and the per-method BODY walk that finds every assignment, return, and
//! send site. A `Send`/`Cascade` now resolves to `send::check_send`'s real
//! result type (T3) rather than a hardcoded `Dynamic` — but the check
//! itself still only fires when the RECEIVER's own type is known, so an
//! unannotated receiver (the overwhelming majority of the real world
//! today) is never checked at all, matching the design's own "gradual
//! typing finds little until signatures exist."

use std::collections::HashMap;

use crate::frontend::ast::{Expr, Literal, MethodNode};

use super::check::AnnotationSite;
use super::interface::{ClassInterface, WorldModel};
use super::subtype::{subtype_of, ResolvedType, Trail};
use super::type_expr::{self, TypeExpr};

/// A lexical scope stack for the name lookups an assignment/return check
/// needs. `Some(None)` at a given level means "declared here, but with NO
/// captured type" (every block-local name — T0′ never captures block-level
/// annotations at all — or an unannotated method param/temp): this still
/// SHADOWS an outer binding of the same name, which is why it's distinct
/// from "not declared at this level" (`None`, keep looking outward).
#[derive(Default)]
struct Scope {
    frames: Vec<HashMap<String, Option<TypeExpr>>>,
}

impl Scope {
    fn push_frame(&mut self, names: HashMap<String, Option<TypeExpr>>) {
        self.frames.push(names);
    }

    fn pop_frame(&mut self) {
        self.frames.pop();
    }

    /// `Some(declared_type_or_none)` if `name` is bound at ANY lexical
    /// level (innermost wins); `None` if it isn't bound at all (a global or
    /// class reference — T2 doesn't attempt to type those).
    fn lookup(&self, name: &str) -> Option<Option<TypeExpr>> {
        self.frames
            .iter()
            .rev()
            .find_map(|frame| frame.get(name).cloned())
    }
}

/// Parses a captured annotation's text, if any — `None` for "unannotated"
/// AND for "annotated but malformed" (T1's `check_world` already reports
/// `MalformedTypeExpr` for the latter in the SAME overall run; T2 doesn't
/// re-report it, it just treats the slot as unresolved/`Dynamic` for its
/// own purposes).
fn resolve_captured(ann: &Option<crate::frontend::ast::TypeAnnotation>) -> Option<TypeExpr> {
    ann.as_ref().and_then(|a| type_expr::parse(&a.text).ok())
}

fn declared_or_dynamic(ann: &Option<crate::frontend::ast::TypeAnnotation>) -> ResolvedType {
    resolve_captured(ann)
        .map(ResolvedType::Written)
        .unwrap_or(ResolvedType::Dynamic)
}

/// Checks one method's body: every `var := expr` against `var`'s declared
/// type, and every `^expr` (however deeply nested inside blocks — a `^`
/// lexically inside a block literal is a NON-LOCAL RETURN from THIS
/// enclosing method, not from the block, so it's checked against the SAME
/// declared return type) against the method's own declared return type. A
/// method with NO explicit return anywhere answers `self` on falling off
/// the end (`DeltaMethod>>typecheck`'s own rule) — checked too.
pub fn check_method(
    model: &WorldModel,
    class_name: &str,
    class_side: bool,
    selector: &str,
    m: &MethodNode,
    errors: &mut Vec<super::check::TypeError>,
) {
    let declared_return = declared_or_dynamic(&m.type_sig.return_type);

    let mut method_frame = HashMap::new();
    for (name, ty) in m.params.iter().zip(m.type_sig.param_types.iter()) {
        method_frame.insert(name.clone(), resolve_captured(ty));
    }
    for (name, ty) in m.temps.iter().zip(m.type_sig.temp_types.iter()) {
        method_frame.insert(name.clone(), resolve_captured(ty));
    }

    let mut scope = Scope::default();
    scope.push_frame(method_frame);

    let mut found_return = false;
    let mut ctx = WalkCtx {
        model,
        class_name,
        class_side,
        selector,
        declared_return: &declared_return,
        found_return: &mut found_return,
        errors,
    };
    for stmt in &m.body {
        walk_expr(&mut ctx, &mut scope, stmt);
    }
    scope.pop_frame();

    // A method whose body is a BARE primitive pragma with NO Smalltalk
    // fallback code (`m.body` empty, `m.primitive` set — e.g. `asDouble [
    // <primitive: 108> ]`) never actually "falls off the end of real
    // code" in the sense the implicit-`^self` rule means: its real return
    // value is whatever the primitive computes, which is completely
    // opaque to this checker (T4, found empirically: annotating exactly
    // these type-CONVERTING primitives, like `asDouble ^<Double>` on
    // SmallInteger, tripped this check every time, since SmallInteger is
    // obviously not a subtype of Double -- the checker was insisting the
    // primitive returns `self` when it demonstrably does not). Only an
    // otherwise-EMPTY method (no primitive at all) genuinely falls off
    // the end per ordinary Smalltalk semantics, and only a method WITH
    // fallback code after its primitive pragma can meaningfully "fall
    // off the end of its own Smalltalk statements" -- both keep the
    // implicit-self check as before.
    let is_bare_primitive = m.body.is_empty() && m.primitive.is_some();
    if !found_return && !is_bare_primitive {
        let implicit = ResolvedType::named("Self");
        let mut trail = Trail::new();
        if !subtype_of(model, class_name, &implicit, &declared_return, &mut trail) {
            errors.push(super::check::TypeError::ReturnNotSubtype {
                site: AnnotationSite {
                    class_name: class_name.to_string(),
                    class_side,
                    selector: Some(selector.to_string()),
                    slot: "implicit return (falls off the end)".to_string(),
                },
                declared: declared_return.to_string(),
                actual: implicit.to_string(),
            });
        }
    }
}

/// Threaded through the recursive walk so each site only needs to carry
/// what changes per-recursion (the `Expr` itself) — everything else is the
/// same for the whole method being checked.
struct WalkCtx<'a> {
    model: &'a WorldModel,
    class_name: &'a str,
    class_side: bool,
    selector: &'a str,
    declared_return: &'a ResolvedType,
    found_return: &'a mut bool,
    errors: &'a mut Vec<super::check::TypeError>,
}

fn walk_expr(ctx: &mut WalkCtx, scope: &mut Scope, expr: &Expr) {
    match expr {
        Expr::SelfRef(_) | Expr::Var { .. } | Expr::Lit { .. } | Expr::CascadeRcvr(_) => {
            // Leaves -- nothing to descend into, nothing to check (a bare
            // reference or literal is never itself an assignment/return/
            // send site; SYNTHESIZING these only matters as a larger
            // expression's sub-part, handled where those are found below).
        }
        Expr::Assign { name, value, .. } => {
            walk_expr(ctx, scope, value);
            let declared = match scope.lookup(name) {
                Some(t) => t.map(ResolvedType::Written).unwrap_or(ResolvedType::Dynamic),
                None => ivar_type(ctx.model, ctx.class_name, name),
            };
            let actual = synthesize(ctx, scope, value);
            let mut trail = Trail::new();
            if !subtype_of(ctx.model, ctx.class_name, &actual, &declared, &mut trail) {
                ctx.errors.push(super::check::TypeError::AssignmentNotSubtype {
                    site: AnnotationSite {
                        class_name: ctx.class_name.to_string(),
                        class_side: ctx.class_side,
                        selector: Some(ctx.selector.to_string()),
                        slot: format!("assignment to '{name}'"),
                    },
                    var_name: name.clone(),
                    declared: declared.to_string(),
                    actual: actual.to_string(),
                });
            }
        }
        Expr::Send {
            receiver,
            selector,
            args,
            is_super,
            ..
        } => {
            walk_expr(ctx, scope, receiver);
            for a in args {
                walk_expr(ctx, scope, a);
            }
            let receiver_type = synthesize(ctx, scope, receiver);
            let arg_types: Vec<ResolvedType> = args.iter().map(|a| synthesize(ctx, scope, a)).collect();
            let site = AnnotationSite {
                class_name: ctx.class_name.to_string(),
                class_side: ctx.class_side,
                selector: Some(ctx.selector.to_string()),
                slot: format!("send '{selector}'"),
            };
            super::send::check_send(
                ctx.model,
                ctx.class_name,
                ctx.class_side,
                *is_super,
                &receiver_type,
                selector,
                &arg_types,
                &site,
                ctx.errors,
            );
        }
        Expr::Cascade {
            receiver, segments, ..
        } => {
            walk_expr(ctx, scope, receiver);
            let receiver_type = synthesize(ctx, scope, receiver);
            for s in segments {
                // Every segment is, by construction, a `Send` whose OWN
                // `receiver` field is `CascadeRcvr` (`parser::parse_cascade`)
                // -- substitute the cascade's ONE real receiver type here
                // (`DeltaCascadedSend`'s rule: every segment checks against
                // the SAME receiver, not the previous segment's result).
                let Expr::Send {
                    selector,
                    args,
                    is_super,
                    ..
                } = s
                else {
                    continue; // parser-guaranteed shape; skip rather than panic if it ever isn't
                };
                for a in args {
                    walk_expr(ctx, scope, a);
                }
                let arg_types: Vec<ResolvedType> =
                    args.iter().map(|a| synthesize(ctx, scope, a)).collect();
                let site = AnnotationSite {
                    class_name: ctx.class_name.to_string(),
                    class_side: ctx.class_side,
                    selector: Some(ctx.selector.to_string()),
                    slot: format!("cascade send '{selector}'"),
                };
                super::send::check_send(
                    ctx.model,
                    ctx.class_name,
                    ctx.class_side,
                    *is_super,
                    &receiver_type,
                    selector,
                    &arg_types,
                    &site,
                    ctx.errors,
                );
            }
        }
        Expr::DynArray { elems, .. } => {
            for e in elems {
                walk_expr(ctx, scope, e);
            }
        }
        Expr::Block(b) => {
            let mut frame = HashMap::new();
            for p in &b.params {
                frame.insert(p.clone(), None); // T0′ never captures block-level annotations
            }
            for t in &b.temps {
                frame.insert(t.clone(), None);
            }
            scope.push_frame(frame);
            for stmt in &b.body {
                walk_expr(ctx, scope, stmt);
            }
            scope.pop_frame();
        }
        Expr::Return { value, .. } => {
            *ctx.found_return = true;
            walk_expr(ctx, scope, value);
            let actual = synthesize(ctx, scope, value);
            let mut trail = Trail::new();
            if !subtype_of(ctx.model, ctx.class_name, &actual, ctx.declared_return, &mut trail) {
                ctx.errors.push(super::check::TypeError::ReturnNotSubtype {
                    site: AnnotationSite {
                        class_name: ctx.class_name.to_string(),
                        class_side: ctx.class_side,
                        selector: Some(ctx.selector.to_string()),
                        slot: "return".to_string(),
                    },
                    declared: ctx.declared_return.to_string(),
                    actual: actual.to_string(),
                });
            }
        }
    }
}

/// This class's instance-variable type for `name`, or `Dynamic` if it
/// isn't one of this class's ivars at all (a genuinely unresolved
/// name — a global/class reference, out of scope for T2).
fn ivar_type(model: &WorldModel, class_name: &str, name: &str) -> ResolvedType {
    let Some(class): Option<&ClassInterface> = model.classes.get(class_name) else {
        return ResolvedType::Dynamic;
    };
    class
        .inst_vars
        .iter()
        .position(|v| v == name)
        .and_then(|i| class.inst_var_types.get(i).cloned().flatten())
        .and_then(|text| type_expr::parse(&text).ok())
        .map(ResolvedType::Written)
        .unwrap_or(ResolvedType::Dynamic)
}

/// Synthesizes `expr`'s type. Literals and plain variable references
/// (params/temps/ivars, through the SAME scope-then-ivar lookup order
/// `walk_expr`'s assignment rule uses) are fully known without any send-
/// typing machinery; a `Send`/`Cascade` recomputes its result type via
/// `send::check_send`, but with a THROWAWAY errors sink — `walk_expr`'s
/// own recursive descent already visited this exact node (whenever it's
/// reachable as an ordinary statement-tree position, which every `Send`
/// synthesized here is) and reported any real error there EXACTLY once;
/// re-running the same check here only to answer "what type does this
/// produce" must not report it a second time. `CascadeRcvr`/`Block` stay
/// `Dynamic` — a block LITERAL's own arity/component types aren't
/// captured anywhere (T0′ never captures block-level annotations), and a
/// bare `CascadeRcvr` is only ever meant to be substituted by
/// `walk_expr`'s own Cascade handling, never synthesized directly.
fn synthesize(ctx: &mut WalkCtx, scope: &Scope, expr: &Expr) -> ResolvedType {
    match expr {
        Expr::SelfRef(_) => ResolvedType::named("Self"),
        Expr::Var { name, .. } => match scope.lookup(name) {
            Some(t) => t.map(ResolvedType::Written).unwrap_or(ResolvedType::Dynamic),
            None => ivar_type(ctx.model, ctx.class_name, name),
        },
        Expr::Lit { value, .. } => type_of_literal(value),
        Expr::Assign { value, .. } => synthesize(ctx, scope, value),
        Expr::Send {
            receiver,
            selector,
            args,
            is_super,
            ..
        } => {
            let receiver_type = synthesize(ctx, scope, receiver);
            let arg_types: Vec<ResolvedType> = args.iter().map(|a| synthesize(ctx, scope, a)).collect();
            let mut discard = Vec::new();
            super::send::check_send(
                ctx.model,
                ctx.class_name,
                ctx.class_side,
                *is_super,
                &receiver_type,
                selector,
                &arg_types,
                &AnnotationSite {
                    class_name: ctx.class_name.to_string(),
                    class_side: ctx.class_side,
                    selector: Some(ctx.selector.to_string()),
                    slot: format!("send '{selector}'"),
                },
                &mut discard,
            )
        }
        Expr::Cascade {
            receiver, segments, ..
        } => {
            let receiver_type = synthesize(ctx, scope, receiver);
            // A cascade expression's own value is its LAST segment's send
            // result (Smalltalk semantics) -- every earlier segment's
            // result is a discarded intermediate value.
            let Some(Expr::Send {
                selector,
                args,
                is_super,
                ..
            }) = segments.last()
            else {
                return ResolvedType::Dynamic; // parser-guaranteed non-empty; defensive fallback only
            };
            let arg_types: Vec<ResolvedType> = args.iter().map(|a| synthesize(ctx, scope, a)).collect();
            let mut discard = Vec::new();
            super::send::check_send(
                ctx.model,
                ctx.class_name,
                ctx.class_side,
                *is_super,
                &receiver_type,
                selector,
                &arg_types,
                &AnnotationSite {
                    class_name: ctx.class_name.to_string(),
                    class_side: ctx.class_side,
                    selector: Some(ctx.selector.to_string()),
                    slot: format!("cascade send '{selector}'"),
                },
                &mut discard,
            )
        }
        Expr::CascadeRcvr(_) | Expr::Block(_) | Expr::Return { .. } => ResolvedType::Dynamic,
        Expr::DynArray { .. } => ResolvedType::named("Array"),
    }
}

/// Literal typing (design doc §4): "SmallInteger/LargeInteger → Integer,
/// Float syntax → Double, `'…'` → String, `#…` → Symbol, `$c` → Character,
/// true/false → Boolean, nil → UndefinedObject." A literal Array's element
/// types aren't examined (no generics semantics in v1 — `Array` alone,
/// same as `DynArray`'s synthesis above).
fn type_of_literal(lit: &Literal) -> ResolvedType {
    match lit {
        Literal::Int(_) | Literal::BigInt { .. } => ResolvedType::named("Integer"),
        Literal::Float(_) => ResolvedType::named("Double"),
        Literal::Char(_) => ResolvedType::named("Character"),
        Literal::Str(_) => ResolvedType::named("String"),
        Literal::Symbol(_) => ResolvedType::named("Symbol"),
        Literal::Array(_) => ResolvedType::named("Array"),
        Literal::ByteArray(_) => ResolvedType::named("ByteArray"),
        Literal::Nil => ResolvedType::named("UndefinedObject"),
        Literal::True | Literal::False => ResolvedType::named("Boolean"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::check::TypeError;
    use super::super::interface::build_world_model;
    use std::fs;

    fn temp_world_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "macvm_types_exprtype_test_{tag}_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A minimal class tower every fixture below can subclass into.
    fn write_number_tower(dir: &std::path::Path) {
        fs::write(dir.join("00_object.mst"), "nil subclass: Object [\n]\n").unwrap();
        fs::write(
            dir.join("01_tower.mst"),
            "Object subclass: Integer [\n]\n\
             Object subclass: Double [\n]\n\
             Object subclass: String [\n]\n\
             Object subclass: Symbol [\n]\n\
             Object subclass: Character [\n]\n\
             Object subclass: Boolean [\n]\n\
             Object subclass: UndefinedObject [\n]\n\
             Object subclass: Array [\n]\n",
        )
        .unwrap();
    }

    #[test]
    fn literal_assigned_to_wrong_declared_temp_is_flagged() {
        let dir = temp_world_dir("assign_literal");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_foo.mst"),
            "Object subclass: Foo [\n\
             \x20   bar [\n\
             \x20       | x <Integer> |\n\
             \x20       x := 'hello'.\n\
             \x20       ^x\n\
             \x20   ]\n\
             ]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        let errors = super::super::check::check_world(&model);
        assert!(
            errors.iter().any(|e| matches!(
                e,
                TypeError::AssignmentNotSubtype { var_name, .. } if var_name == "x"
            )),
            "expected AssignmentNotSubtype for x := 'hello', got {errors:#?}"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn matching_literal_assignment_is_clean() {
        let dir = temp_world_dir("assign_ok");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_foo.mst"),
            "Object subclass: Foo [\n\
             \x20   bar [\n\
             \x20       | x <Integer> |\n\
             \x20       x := 5.\n\
             \x20       ^x\n\
             \x20   ]\n\
             ]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        assert_eq!(super::super::check::check_world(&model), vec![]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wrong_literal_return_is_flagged() {
        let dir = temp_world_dir("return_literal");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_foo.mst"),
            "Object subclass: Foo [\n    bar ^ <Integer> [\n        ^'hello'\n    ]\n]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        let errors = super::super::check::check_world(&model);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, TypeError::ReturnNotSubtype { .. })),
            "expected ReturnNotSubtype, got {errors:#?}"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_local_return_inside_a_block_checks_against_the_enclosing_method() {
        let dir = temp_world_dir("nlr_block");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_foo.mst"),
            "Object subclass: Foo [\n\
             \x20   bar ^ <Integer> [\n\
             \x20       true ifTrue: [ ^'hello' ].\n\
             \x20       ^5\n\
             \x20   ]\n\
             ]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        let errors = super::super::check::check_world(&model);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, TypeError::ReturnNotSubtype { .. })),
            "the ^'hello' INSIDE the block is a non-local return from `bar` \
             itself, so it must be checked against bar's OWN ^Integer, got {errors:#?}"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn implicit_self_return_checked_when_declared_return_disagrees() {
        let dir = temp_world_dir("implicit_return");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_foo.mst"),
            "Object subclass: Foo [\n    bar ^ <Integer> [\n        3 + 4.\n    ]\n]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        let errors = super::super::check::check_world(&model);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, TypeError::ReturnNotSubtype { .. })),
            "no explicit return -> answers self, which isn't an Integer, got {errors:#?}"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn implicit_self_return_clean_when_declared_return_is_self_or_object() {
        let dir = temp_world_dir("implicit_return_ok");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_foo.mst"),
            // Bare literal statements (no message send) -- this test's own
            // concern is the IMPLICIT-return rule, not send-checking; the
            // fixture's Integer/Object classes carry no methods at all, so
            // an incidental `3 + 4` here would (correctly, since T3) trip
            // SelectorUndefined and mask what this test is actually about.
            "Object subclass: Foo [\n\
             \x20   bar ^ <Self> [\n        5.\n    ]\n\
             \x20   baz ^ <Object> [\n        5.\n    ]\n\
             ]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        assert_eq!(super::super::check::check_world(&model), vec![]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn assigning_a_typed_param_to_a_differently_typed_temp_is_flagged() {
        let dir = temp_world_dir("param_to_temp");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_foo.mst"),
            "Object subclass: Foo [\n\
             \x20   take: n <String> [\n\
             \x20       | r <Integer> |\n\
             \x20       r := n.\n\
             \x20       ^r\n\
             \x20   ]\n\
             ]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        let errors = super::super::check::check_world(&model);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, TypeError::AssignmentNotSubtype { .. })),
            "a String-declared param assigned into an Integer-declared temp, got {errors:#?}"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn block_local_shadowing_is_untyped_not_the_outer_binding() {
        // The block param `x` SHADOWS the outer Integer-declared temp `x` --
        // inside the block, `x` must be treated as Dynamic (unknown), not
        // as Integer, so assigning a String literal to the outer `x`
        // FROM OUTSIDE the block still checks against Integer (flagged),
        // while a reference to `x` INSIDE the block never gets a spurious
        // Integer-vs-something-else complaint of its own.
        let dir = temp_world_dir("shadow");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_foo.mst"),
            "Object subclass: Foo [\n\
             \x20   bar [\n\
             \x20       | x <Integer> |\n\
             \x20       x := 5.\n\
             \x20       [:x | x] value: 'hello'.\n\
             \x20       ^x\n\
             \x20   ]\n\
             ]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        assert_eq!(
            super::super::check::check_world(&model),
            vec![],
            "the block's OWN x is untyped (T0′ never captures block-level \
             annotations) -- nothing here is checkable, and nothing should \
             be wrongly flagged either"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unannotated_method_with_no_annotated_variables_has_zero_findings() {
        let dir = temp_world_dir("fully_dynamic");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_foo.mst"),
            "Object subclass: Foo [\n\
             \x20   bar: n [\n\
             \x20       | t |\n\
             \x20       t := n.\n\
             \x20       ^t\n\
             \x20   ]\n\
             ]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        assert_eq!(super::super::check::check_world(&model), vec![]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bare_primitive_with_no_fallback_is_not_checked_against_implicit_self() {
        // `asOther [ <primitive: 1> ]` never runs any Smalltalk code at
        // all -- its real return value is whatever the primitive
        // computes, which this checker can't see. The implicit-`^self`
        // rule must NOT apply here (Foo is not a subtype of Other, so a
        // naive "falls off the end -> self" check would wrongly flag
        // EXACTLY the type-converting primitives (asFloat/asDouble/…)
        // T4 needs to annotate).
        let dir = temp_world_dir("bare_primitive");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_foo.mst"),
            "Object subclass: Foo [\n\
             \x20   asDouble ^ <Double> [\n\
             \x20       <primitive: 1>\n\
             \x20   ]\n\
             ]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        assert_eq!(
            super::super::check::check_world(&model),
            vec![],
            "a bare-primitive method's declared return must NOT be checked \
             against the implicit self -- Foo is not a Double, but the \
             primitive (opaque to this checker) legitimately answers one"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn primitive_with_smalltalk_fallback_still_checks_implicit_self() {
        // Unlike the bare-primitive case, a primitive FOLLOWED BY real
        // Smalltalk statements genuinely can fall off the end of THAT
        // code (if the primitive fails and the fallback itself has no
        // explicit `^`) -- the implicit-self rule must still apply here.
        let dir = temp_world_dir("primitive_with_fallback");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_foo.mst"),
            "Object subclass: Foo [\n\
             \x20   asDouble ^ <Double> [\n\
             \x20       <primitive: 1>\n\
             \x20       3 + 4.\n\
             \x20   ]\n\
             ]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        let errors = super::super::check::check_world(&model);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, super::super::check::TypeError::ReturnNotSubtype { .. })),
            "Foo falling off the end of REAL fallback code must still be \
             checked against ^<Double> (Foo is not a Double), got {errors:#?}"
        );

        fs::remove_dir_all(&dir).ok();
    }

    // ---- T3: the send rule ------------------------------------------------

    #[test]
    fn send_to_a_selector_the_receivers_class_defines_is_clean() {
        let dir = temp_world_dir("send_ok_own");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_foo.mst"),
            "Object subclass: Foo [\n\
             \x20   answer ^ <Integer> [ ^5 ]\n\
             \x20   bar [\n\
             \x20       | x <Foo> |\n\
             \x20       x := self.\n\
             \x20       ^x answer\n\
             \x20   ]\n\
             ]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        assert_eq!(super::super::check::check_world(&model), vec![]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn send_to_a_selector_understood_via_inheritance_is_clean() {
        // `answer` is defined on Base, not Foo itself -- the send-rule's
        // chain walk (`send::find_method`) must find it via inheritance,
        // exactly like `subtype_of`'s own superclass-chain walk.
        let dir = temp_world_dir("send_ok_inherited");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_base.mst"),
            "Object subclass: Base [\n    answer ^ <Integer> [ ^5 ]\n]\n",
        )
        .unwrap();
        fs::write(
            dir.join("03_foo.mst"),
            "Base subclass: Foo [\n\
             \x20   bar [\n\
             \x20       | x <Foo> |\n\
             \x20       x := self.\n\
             \x20       ^x answer\n\
             \x20   ]\n\
             ]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_base.mst\n03_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        assert_eq!(super::super::check::check_world(&model), vec![]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn send_to_an_undefined_selector_is_flagged() {
        let dir = temp_world_dir("send_dnu");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_foo.mst"),
            "Object subclass: Foo [\n\
             \x20   bar [\n\
             \x20       | x <Foo> |\n\
             \x20       x := self.\n\
             \x20       ^x frobnicate\n\
             \x20   ]\n\
             ]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        let errors = super::super::check::check_world(&model);
        assert!(
            errors.iter().any(|e| matches!(
                e,
                super::super::check::TypeError::SelectorUndefined { selector, .. }
                    if selector == "frobnicate"
            )),
            "expected SelectorUndefined for 'x frobnicate' (Foo has no such method \
             and neither does anything on its chain), got {errors:#?}"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn send_with_unresolved_dynamic_receiver_is_never_checked() {
        // `n` is an UNANNOTATED param -- Dynamic -- so `n frobnicate` must
        // never be flagged even though no class anywhere defines
        // `frobnicate`. This is the design's own "gradual typing finds
        // little until signatures exist" promise, load-bearing: it's what
        // keeps the real, unannotated 144-class world at zero findings.
        let dir = temp_world_dir("send_dynamic_receiver");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_foo.mst"),
            "Object subclass: Foo [\n    bar: n [\n        ^n frobnicate\n    ]\n]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        assert_eq!(super::super::check::check_world(&model), vec![]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn send_argument_of_the_wrong_type_is_flagged() {
        let dir = temp_world_dir("send_bad_arg");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_foo.mst"),
            "Object subclass: Foo [\n\
             \x20   take: n <Integer> [ ^n ]\n\
             \x20   bar [\n\
             \x20       | x <Foo> |\n\
             \x20       x := self.\n\
             \x20       ^x take: 'hello'\n\
             \x20   ]\n\
             ]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        let errors = super::super::check::check_world(&model);
        assert!(
            errors.iter().any(|e| matches!(
                e,
                super::super::check::TypeError::SendArgNotSubtype { selector, arg_index: 0, .. }
                    if selector == "take:"
            )),
            "expected SendArgNotSubtype for take: 'hello' (declared <Integer>), got {errors:#?}"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cascade_checks_every_segment_against_the_same_receiver() {
        let dir = temp_world_dir("cascade");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_foo.mst"),
            "Object subclass: Foo [\n\
             \x20   one ^ <Integer> [ ^1 ]\n\
             \x20   bar [\n\
             \x20       | x <Foo> |\n\
             \x20       x := self.\n\
             \x20       ^x one; frobnicate; one\n\
             \x20   ]\n\
             ]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        let errors = super::super::check::check_world(&model);
        assert_eq!(
            errors
                .iter()
                .filter(|e| matches!(
                    e,
                    super::super::check::TypeError::SelectorUndefined { selector, .. }
                        if selector == "frobnicate"
                ))
                .count(),
            1,
            "the middle cascade segment (Foo has no 'frobnicate') must be \
             flagged exactly once against the SAME receiver x, not skipped \
             and not duplicated, got {errors:#?}"
        );
        assert!(
            !errors.iter().any(|e| matches!(
                e,
                super::super::check::TypeError::SelectorUndefined { selector, .. }
                    if selector == "one"
            )),
            "the 'one' segments (before and after) are legitimate sends on \
             the SAME receiver x and must not be flagged, got {errors:#?}"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn super_send_looks_up_starting_at_the_enclosing_classs_superclass() {
        // Base defines `greet` (returns Integer); Foo OVERRIDES `greet`
        // (returns String) and its OWN `bar` sends `super greet`. The
        // super-send must resolve to BASE's `greet` (Integer), NOT Foo's
        // own override (String) -- if it incorrectly used Foo's override,
        // `^n` (an Integer local) would spuriously mismatch against a
        // String-declared return, or vice versa depending on which way
        // the bug ran. Checked by requiring ZERO findings: super correctly
        // reaching Base's Integer-returning `greet` makes everything here
        // consistent; reaching Foo's own String-returning override
        // would not.
        let dir = temp_world_dir("super_send");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_base.mst"),
            "Object subclass: Base [\n    greet ^ <Integer> [ ^1 ]\n]\n",
        )
        .unwrap();
        fs::write(
            dir.join("03_foo.mst"),
            "Base subclass: Foo [\n\
             \x20   greet ^ <String> [ ^'hi' ]\n\
             \x20   bar ^ <Integer> [\n\
             \x20       ^super greet\n\
             \x20   ]\n\
             ]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_base.mst\n03_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        assert_eq!(
            super::super::check::check_world(&model),
            vec![],
            "super greet must resolve to Base's Integer-returning greet, \
             matching bar's own ^<Integer> -- Foo's own String-returning \
             override must NOT be consulted"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn class_side_self_send_is_not_checked() {
        // No metaclass modeling exists (design doc's own stated v1 gap) --
        // `self`/`Self` inside a CLASS-SIDE method must be left Dynamic
        // rather than incorrectly searched against instance_methods.
        let dir = temp_world_dir("class_side_self");
        write_number_tower(&dir);
        fs::write(
            dir.join("02_foo.mst"),
            "Object subclass: Foo [\n\
             \x20   Foo class >> make [\n\
             \x20       ^self frobnicate\n\
             \x20   ]\n\
             ]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "00_object.mst\n01_tower.mst\n02_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        assert_eq!(
            super::super::check::check_world(&model),
            vec![],
            "self inside a class-side method means 'the class', not 'an \
             instance of it' -- must be left unchecked, not searched \
             against instance_methods and wrongly flagged"
        );

        fs::remove_dir_all(&dir).ok();
    }
}
