//! T1 (`docs/typechecker_design.md` §4/§6): the checker's own error
//! catalog — so far only the THREE variants T1 itself reports
//! (`MalformedTypeExpr`, `UndeclaredTypeName`, `GenericArityMismatch`); the
//! v1 catalog's other ~9 kinds (send/method/assignment rules — T2/T3) get
//! their own variants when those stages land, not stubbed out now.

use std::collections::HashMap;
use std::fmt;

use super::interface::WorldModel;
use super::type_expr::{self, TypeExpr};

/// Where a checked annotation lives, for the human-readable report.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AnnotationSite {
    pub class_name: String,
    pub class_side: bool,
    /// `None` for an instance-variable annotation; `Some(selector)` for a
    /// method's own param/return/temp annotation.
    pub selector: Option<String>,
    /// A plain label (`"return"`, `"param 0"`, `"temp 1"`, `"ivar 0"`) —
    /// this only needs to be human-readable, not a structured index for a
    /// later stage to key on.
    pub slot: String,
}

impl fmt::Display for AnnotationSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.selector {
            Some(sel) if self.class_side => {
                write!(f, "{} class>>{} ({})", self.class_name, sel, self.slot)
            }
            Some(sel) => write!(f, "{}>>{} ({})", self.class_name, sel, self.slot),
            None => write!(f, "{} ({})", self.class_name, self.slot),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypeError {
    /// The raw captured annotation text didn't parse as a well-formed
    /// `TypeExpr` (`type_expr`'s grammar) — e.g. unbalanced brackets, a
    /// bare `Cltn[]` with no type argument, or trailing text after an
    /// otherwise-complete parse.
    MalformedTypeExpr {
        site: AnnotationSite,
        text: String,
        reason: String,
    },
    /// A `Named`/`Generic` leaf names a class that resolves nowhere in the
    /// world (not a real class, and not `Self`) — most often a typo.
    UndeclaredTypeName { site: AnnotationSite, name: String },
    /// The SAME generic head name (e.g. `Cltn`) is applied with a
    /// DIFFERENT number of type arguments at two use sites. A purely
    /// SYNTACTIC self-consistency check — v1 has no declared generic
    /// formal-parameter list to check an application's arity AGAINST
    /// (checking generics for real is the optional T5); this only flags
    /// the world disagreeing with itself about how many arguments `head`
    /// takes.
    GenericArityMismatch {
        head: String,
        site: AnnotationSite,
        arity: usize,
        first_seen_site: AnnotationSite,
        first_seen_arity: usize,
    },
}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeError::MalformedTypeExpr { site, text, reason } => {
                write!(f, "{site}: malformed type annotation '{text}' — {reason}")
            }
            TypeError::UndeclaredTypeName { site, name } => {
                write!(f, "{site}: undeclared type name '{name}'")
            }
            TypeError::GenericArityMismatch {
                head,
                site,
                arity,
                first_seen_site,
                first_seen_arity,
            } => write!(
                f,
                "{site}: '{head}' applied with {arity} type argument(s), \
                 but {first_seen_site} applied it with {first_seen_arity}"
            ),
        }
    }
}

/// Runs every T1 check over the whole model. Deterministic order (follows
/// `WorldModel::class_order`, then instance methods before class methods,
/// each in a stable per-class order) so two runs over the same source
/// produce the same report — load-bearing for the differential test that
/// compares this against a golden.
pub fn check_world(model: &WorldModel) -> Vec<TypeError> {
    let mut errors = Vec::new();
    let mut generic_arity: HashMap<String, (usize, AnnotationSite)> = HashMap::new();

    for class_name in &model.class_order {
        let class = &model.classes[class_name];

        for (i, ty) in class.inst_var_types.iter().enumerate() {
            if let Some(text) = ty {
                let site = AnnotationSite {
                    class_name: class_name.clone(),
                    class_side: false,
                    selector: None,
                    slot: format!("ivar {i}"),
                };
                check_annotation(model, site, text, &mut errors, &mut generic_arity);
            }
        }

        for (side_methods, class_side) in [
            (&class.instance_methods, false),
            (&class.class_methods, true),
        ] {
            let mut selectors: Vec<&String> = side_methods.keys().collect();
            selectors.sort(); // HashMap iteration order isn't stable across runs
            for selector in selectors {
                let m = &side_methods[selector];
                if let Some(t) = &m.type_sig.return_type {
                    let site = AnnotationSite {
                        class_name: class_name.clone(),
                        class_side,
                        selector: Some(selector.clone()),
                        slot: "return".to_string(),
                    };
                    check_annotation(model, site, &t.text, &mut errors, &mut generic_arity);
                }
                for (i, pt) in m.type_sig.param_types.iter().enumerate() {
                    if let Some(t) = pt {
                        let site = AnnotationSite {
                            class_name: class_name.clone(),
                            class_side,
                            selector: Some(selector.clone()),
                            slot: format!("param {i}"),
                        };
                        check_annotation(model, site, &t.text, &mut errors, &mut generic_arity);
                    }
                }
                for (i, tt) in m.type_sig.temp_types.iter().enumerate() {
                    if let Some(t) = tt {
                        let site = AnnotationSite {
                            class_name: class_name.clone(),
                            class_side,
                            selector: Some(selector.clone()),
                            slot: format!("temp {i}"),
                        };
                        check_annotation(model, site, &t.text, &mut errors, &mut generic_arity);
                    }
                }
            }
        }
    }
    errors
}

fn check_annotation(
    model: &WorldModel,
    site: AnnotationSite,
    text: &str,
    errors: &mut Vec<TypeError>,
    generic_arity: &mut HashMap<String, (usize, AnnotationSite)>,
) {
    match type_expr::parse(text) {
        Err(type_expr::ParseError(reason)) => errors.push(TypeError::MalformedTypeExpr {
            site,
            text: text.to_string(),
            reason,
        }),
        Ok(expr) => walk_type_expr(model, &site, &expr, errors, generic_arity),
    }
}

fn walk_type_expr(
    model: &WorldModel,
    site: &AnnotationSite,
    expr: &TypeExpr,
    errors: &mut Vec<TypeError>,
    generic_arity: &mut HashMap<String, (usize, AnnotationSite)>,
) {
    match expr {
        TypeExpr::Named(name) => check_name(model, site, name, errors),
        TypeExpr::Generic { head, args } => {
            check_name(model, site, head, errors);
            record_arity(generic_arity, head, args.len(), site, errors);
            for a in args {
                walk_type_expr(model, site, a, errors, generic_arity);
            }
        }
        TypeExpr::Block {
            arg_types,
            return_type,
        } => {
            for a in arg_types {
                walk_type_expr(model, site, a, errors, generic_arity);
            }
            if let Some(r) = return_type {
                walk_type_expr(model, site, r, errors, generic_arity);
            }
        }
        TypeExpr::Union(variants) => {
            for v in variants {
                walk_type_expr(model, site, v, errors, generic_arity);
            }
        }
        TypeExpr::InferenceDef(inner) => walk_type_expr(model, site, inner, errors, generic_arity),
    }
}

/// `Self` is the one pseudo-type name that resolves without naming a real
/// world class; everything else — including a generic's HEAD name — is
/// looked up exactly like any other type name. No allowlist of Strongtalk's
/// own abbreviations (`Cltn`, `OrdCltn`, …): if this world doesn't declare
/// a class by that name, using it is flagged the same as any other typo.
fn check_name(model: &WorldModel, site: &AnnotationSite, name: &str, errors: &mut Vec<TypeError>) {
    if name == "Self" {
        return;
    }
    if !model.classes.contains_key(name) {
        errors.push(TypeError::UndeclaredTypeName {
            site: site.clone(),
            name: name.to_string(),
        });
    }
}

fn record_arity(
    seen: &mut HashMap<String, (usize, AnnotationSite)>,
    head: &str,
    arity: usize,
    site: &AnnotationSite,
    errors: &mut Vec<TypeError>,
) {
    match seen.get(head) {
        None => {
            seen.insert(head.to_string(), (arity, site.clone()));
        }
        Some((first_arity, first_site)) if *first_arity != arity => {
            errors.push(TypeError::GenericArityMismatch {
                head: head.to_string(),
                site: site.clone(),
                arity,
                first_seen_site: first_site.clone(),
                first_seen_arity: *first_arity,
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::super::interface::build_world_model;
    use super::*;
    use std::fs;

    fn temp_world_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("macvm_types_check_test_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn unannotated_world_has_zero_findings() {
        let dir = temp_world_dir("unannotated");
        fs::write(
            dir.join("01_foo.mst"),
            "Object subclass: Foo [\n    | count |\n    bar: n [\n        ^n\n    ]\n]\n",
        )
        .unwrap();
        fs::write(dir.join("world.list"), "01_foo.mst\n").unwrap();

        let model = build_world_model(&dir).unwrap();
        assert_eq!(check_world(&model), vec![]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn undeclared_type_name_is_flagged() {
        let dir = temp_world_dir("undeclared");
        fs::write(
            dir.join("01_foo.mst"),
            "Object subclass: Foo [\n    bar: n <Frobnicator> [\n        ^n\n    ]\n]\n",
        )
        .unwrap();
        fs::write(dir.join("world.list"), "01_foo.mst\n").unwrap();

        let model = build_world_model(&dir).unwrap();
        let errors = check_world(&model);
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            &errors[0],
            TypeError::UndeclaredTypeName { name, .. } if name == "Frobnicator"
        ));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn declared_type_name_resolves_cleanly() {
        let dir = temp_world_dir("declared");
        fs::write(
            dir.join("01_object.mst"),
            "nil subclass: Object [\n]\n",
        )
        .unwrap();
        fs::write(
            dir.join("02_integer.mst"),
            "Object subclass: Integer [\n]\n",
        )
        .unwrap();
        fs::write(
            dir.join("03_foo.mst"),
            "Object subclass: Foo [\n    bar: n <Integer> ^ <Integer> [\n        ^n\n    ]\n]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "01_object.mst\n02_integer.mst\n03_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        assert_eq!(check_world(&model), vec![]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn self_pseudo_type_always_resolves() {
        let dir = temp_world_dir("selfty");
        fs::write(
            dir.join("01_foo.mst"),
            "Object subclass: Foo [\n    Foo class >> make ^ <Self> [\n        ^self new\n    ]\n]\n",
        )
        .unwrap();
        fs::write(dir.join("world.list"), "01_foo.mst\n").unwrap();

        let model = build_world_model(&dir).unwrap();
        assert_eq!(check_world(&model), vec![]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_annotation_is_flagged() {
        let dir = temp_world_dir("malformed");
        fs::write(
            dir.join("01_foo.mst"),
            "Object subclass: Foo [\n    | count <Cltn[]> |\n]\n",
        )
        .unwrap();
        fs::write(dir.join("world.list"), "01_foo.mst\n").unwrap();

        let model = build_world_model(&dir).unwrap();
        let errors = check_world(&model);
        assert_eq!(errors.len(), 1);
        assert!(matches!(errors[0], TypeError::MalformedTypeExpr { .. }));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generic_arity_mismatch_across_two_sites() {
        let dir = temp_world_dir("arity");
        fs::write(
            dir.join("01_object.mst"),
            "nil subclass: Object [\n]\n",
        )
        .unwrap();
        fs::write(
            dir.join("02_cltn.mst"),
            "Object subclass: Cltn [\n]\n",
        )
        .unwrap();
        fs::write(
            dir.join("03_foo.mst"),
            "Object subclass: Foo [\n\
             \x20   one: n <Cltn[Object]> [\n        ^n\n    ]\n\
             \x20   two: n <Cltn[Object, Object]> [\n        ^n\n    ]\n\
             ]\n",
        )
        .unwrap();
        fs::write(
            dir.join("world.list"),
            "01_object.mst\n02_cltn.mst\n03_foo.mst\n",
        )
        .unwrap();

        let model = build_world_model(&dir).unwrap();
        let errors = check_world(&model);
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            &errors[0],
            TypeError::GenericArityMismatch { head, arity, first_seen_arity, .. }
                if head == "Cltn" && *arity == 2 && *first_seen_arity == 1
        ));

        fs::remove_dir_all(&dir).ok();
    }
}
