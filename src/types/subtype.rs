//! T2 (`docs/typechecker_design.md` §4/§6): the subtyping relation —
//! "nominal + trivially-terminating." `S <: T` iff `S = T`, `S` or `T` is
//! `Dynamic`, `T` is `Object`, or `T` is on `S`'s superclass chain; block
//! types check component-wise (contravariant args, covariant return —
//! ported from `DeltaBlockApplicationType`).

use std::collections::HashSet;

use super::interface::WorldModel;
use super::type_expr::TypeExpr;

/// The domain `subtype_of` and expression-type synthesis operate over —
/// distinct from [`TypeExpr`] (the pure ANNOTATION-SYNTAX AST: nobody
/// writes "Dynamic" as an annotation, it's what an absent one MEANS).
/// Every `TypeExpr` embeds via `Written`; `Dynamic` is the extra case an
/// annotation's ABSENCE resolves to.
#[derive(Clone, Debug, PartialEq)]
pub enum ResolvedType {
    Dynamic,
    Written(TypeExpr),
}

impl ResolvedType {
    pub fn named(name: impl Into<String>) -> Self {
        ResolvedType::Written(TypeExpr::Named(name.into()))
    }
}

impl std::fmt::Display for ResolvedType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolvedType::Dynamic => write!(f, "Dynamic"),
            ResolvedType::Written(e) => write!(f, "{e}"),
        }
    }
}

/// Strongtalk's `DeltaGlobalTrail` seam (design doc §4: "the API still
/// takes an assumption-trail parameter... so T5's structural/generic rules
/// land without resignaturing"). Nominal class-hierarchy subtyping is a
/// TREE by construction (a real Smalltalk world can't have a superclass
/// cycle), so v1's leaf-level chain walk never actually needs this — it's
/// a real, working coinductive guard (insert before recursing, remove
/// after) wired through so a future STRUCTURAL rule (protocols, generics
/// variance) can start using it without changing every caller's signature.
#[derive(Default)]
pub struct Trail {
    assumed: HashSet<(String, String)>,
}

impl Trail {
    pub fn new() -> Self {
        Trail::default()
    }
}

/// `S <: T` in the current checking context. `self_class` is the class the
/// method BEING CHECKED belongs to — the only thing the literal pseudo-type
/// `Self` needs to resolve against (whichever side it's written on): `self`
/// used as an expression's SOURCE type, or `Self` written as a DECLARED
/// target type, both mean "an instance of `self_class` (or a subclass)."
pub fn subtype_of(
    model: &WorldModel,
    self_class: &str,
    s: &ResolvedType,
    t: &ResolvedType,
    trail: &mut Trail,
) -> bool {
    if s == t {
        return true;
    }
    if matches!(s, ResolvedType::Dynamic) || matches!(t, ResolvedType::Dynamic) {
        return true;
    }
    if matches!(t, ResolvedType::Written(TypeExpr::Named(n)) if n == "Object") {
        return true;
    }
    let ResolvedType::Written(s_expr) = s else {
        unreachable!("Dynamic already handled above")
    };
    let ResolvedType::Written(t_expr) = t else {
        unreachable!("Dynamic already handled above")
    };

    // Structural unwraps first (Union/InferenceDef on EITHER side) — these
    // never touch the trail; only a nominal head-vs-head comparison can.
    if let TypeExpr::InferenceDef(inner) = s_expr {
        return subtype_of(model, self_class, &ResolvedType::Written((**inner).clone()), t, trail);
    }
    if let TypeExpr::InferenceDef(inner) = t_expr {
        return subtype_of(model, self_class, s, &ResolvedType::Written((**inner).clone()), trail);
    }
    if let TypeExpr::Union(variants) = s_expr {
        // A union AS A SOURCE: every alternative must satisfy T (the value
        // could actually BE any of them).
        return variants
            .iter()
            .all(|v| subtype_of(model, self_class, &ResolvedType::Written(v.clone()), t, trail));
    }
    if let TypeExpr::Union(variants) = t_expr {
        // A union AS A TARGET: S just needs to satisfy ONE alternative.
        return variants
            .iter()
            .any(|v| subtype_of(model, self_class, s, &ResolvedType::Written(v.clone()), trail));
    }

    if let (TypeExpr::Block { arg_types: sa, return_type: sr }, TypeExpr::Block { arg_types: ta, return_type: tr }) =
        (s_expr, t_expr)
    {
        if sa.len() != ta.len() {
            return false; // arity mismatch -- not comparable (no bespoke error kind for this yet)
        }
        // Contravariant args: EACH target arg must satisfy the CORRESPONDING
        // source arg (reversed direction from the return check).
        let args_ok = sa.iter().zip(ta.iter()).all(|(sa_i, ta_i)| {
            subtype_of(
                model,
                self_class,
                &ResolvedType::Written(ta_i.clone()),
                &ResolvedType::Written(sa_i.clone()),
                trail,
            )
        });
        if !args_ok {
            return false;
        }
        // Covariant return.
        return match (sr, tr) {
            (Some(sr), Some(tr)) => subtype_of(
                model,
                self_class,
                &ResolvedType::Written((**sr).clone()),
                &ResolvedType::Written((**tr).clone()),
                trail,
            ),
            (None, None) => true,
            // One side declares a return annotation and the other doesn't --
            // an absent block return type is Dynamic-like (compatible either way).
            _ => true,
        };
    }
    if matches!(s_expr, TypeExpr::Block { .. }) || matches!(t_expr, TypeExpr::Block { .. }) {
        return false; // a block type is never a subtype of/supertype of a nominal one
    }

    // Both sides are now nominal (`Named` or `Generic`) — resolve `Self` to
    // the enclosing class's own name, then walk the superclass chain. A
    // `Generic`'s subtyping is based purely on its HEAD name (generics are
    // parsed+stored but unchecked in v1 -- design doc §2/§4).
    let s_name = nominal_head(s_expr, self_class);
    let t_name = nominal_head(t_expr, self_class);
    let (Some(s_name), Some(t_name)) = (s_name, t_name) else {
        return false; // shouldn't happen -- both were confirmed non-Block above
    };
    superclass_chain_contains(model, &s_name, &t_name, trail)
}

fn nominal_head(expr: &TypeExpr, self_class: &str) -> Option<String> {
    match expr {
        TypeExpr::Named(n) if n == "Self" => Some(self_class.to_string()),
        TypeExpr::Named(n) => Some(n.clone()),
        TypeExpr::Generic { head, .. } if head == "Self" => Some(self_class.to_string()),
        TypeExpr::Generic { head, .. } => Some(head.clone()),
        TypeExpr::Block { .. } | TypeExpr::Union(_) | TypeExpr::InferenceDef(_) => None,
    }
}

/// Is `target` `s_name` itself, or on `s_name`'s superclass chain? A cycle
/// guard via `trail` — defensive against a malformed model, never actually
/// triggered by a real Smalltalk world (a superclass chain is a tree by
/// construction; `interface::build_world_model` never validates a dangling
/// superclass reference, so this is the one place a corrupt chain could
/// otherwise loop forever).
fn superclass_chain_contains(
    model: &WorldModel,
    s_name: &str,
    target: &str,
    trail: &mut Trail,
) -> bool {
    if s_name == target {
        return true;
    }
    let key = (s_name.to_string(), target.to_string());
    if trail.assumed.contains(&key) {
        return false; // already assuming this pair elsewhere on the stack -- cut the cycle
    }
    trail.assumed.insert(key.clone());
    let result = match model.classes.get(s_name).and_then(|c| c.superclass.as_deref()) {
        Some(super_name) => superclass_chain_contains(model, super_name, target, trail),
        None => false, // hit the nil-superclass root without finding `target`
    };
    trail.assumed.remove(&key);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn model_with_chain(tag: &str, chain: &[(&str, Option<&str>)]) -> WorldModel {
        // Builds a WorldModel directly (no .mst parsing needed for these
        // pure subtyping unit tests) via the real loader on a tiny temp
        // fixture -- simplest way to get a `ClassInterface` per entry
        // without hand-constructing private fields. Keyed by `tag` (NOT
        // just pid): tests run in parallel within the SAME process, so
        // every caller needs its OWN directory, matching every other
        // `temp_world_dir(tag)` helper elsewhere in `src/types/`.
        let dir = std::env::temp_dir().join(format!(
            "macvm_types_subtype_test_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut list = String::new();
        for (i, (name, super_name)) in chain.iter().enumerate() {
            let file = format!("{i:02}_{name}.mst");
            let src = match super_name {
                Some(s) => format!("{s} subclass: {name} [\n]\n"),
                None => format!("nil subclass: {name} [\n]\n"),
            };
            std::fs::write(dir.join(&file), src).unwrap();
            list.push_str(&file);
            list.push('\n');
        }
        std::fs::write(dir.join("world.list"), list).unwrap();
        let model = super::super::interface::build_world_model(&dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        model
    }

    fn named(s: &str) -> ResolvedType {
        ResolvedType::named(s)
    }

    #[test]
    fn reflexivity_and_dynamic_and_object() {
        let model = model_with_chain("reflexivity", &[("Object", None), ("Foo", Some("Object"))]);
        let mut trail = Trail::new();
        assert!(subtype_of(&model, "Foo", &named("Foo"), &named("Foo"), &mut trail));
        assert!(subtype_of(&model, "Foo", &ResolvedType::Dynamic, &named("Foo"), &mut trail));
        assert!(subtype_of(&model, "Foo", &named("Foo"), &ResolvedType::Dynamic, &mut trail));
        assert!(subtype_of(&model, "Foo", &named("Frobnicator"), &named("Object"), &mut trail));
    }

    #[test]
    fn superclass_chain_walk() {
        let model = model_with_chain("chain", &[
            ("Object", None),
            ("Magnitude", Some("Object")),
            ("Number", Some("Magnitude")),
            ("Integer", Some("Number")),
        ]);
        let mut trail = Trail::new();
        assert!(subtype_of(&model, "Integer", &named("Integer"), &named("Number"), &mut trail));
        assert!(subtype_of(&model, "Integer", &named("Integer"), &named("Magnitude"), &mut trail));
        assert!(!subtype_of(&model, "Integer", &named("Magnitude"), &named("Integer"), &mut trail));
        assert!(!subtype_of(&model, "Integer", &named("Number"), &named("String"), &mut trail));
    }

    #[test]
    fn self_resolves_to_the_enclosing_class() {
        let model = model_with_chain("self_ctx", &[("Object", None), ("Foo", Some("Object"))]);
        let mut trail = Trail::new();
        // `self` used where `Object` (an ancestor) is declared -- fine.
        assert!(subtype_of(&model, "Foo", &named("Self"), &named("Object"), &mut trail));
        // `^Self` declared, returning `self` from a method on Foo -- fine.
        assert!(subtype_of(&model, "Foo", &named("Self"), &named("Self"), &mut trail));
        // A DIFFERENT class's name is not satisfied by Foo's `self`.
        assert!(!subtype_of(&model, "Foo", &named("Self"), &named("Bar"), &mut trail));
    }

    #[test]
    fn block_type_variance() {
        let model = model_with_chain("block_variance", &[
            ("Object", None),
            ("Magnitude", Some("Object")),
            ("Integer", Some("Magnitude")),
        ]);
        let mut trail = Trail::new();
        let block = |arg: &str, ret: &str| {
            ResolvedType::Written(TypeExpr::Block {
                arg_types: vec![TypeExpr::Named(arg.to_string())],
                return_type: Some(Box::new(TypeExpr::Named(ret.to_string()))),
            })
        };
        // Covariant return: a block returning Integer satisfies a slot
        // wanting one that returns Magnitude (a supertype).
        assert!(subtype_of(
            &model,
            "Object",
            &block("Object", "Integer"),
            &block("Object", "Magnitude"),
            &mut trail
        ));
        // Contravariant arg: a block accepting Object (broader) satisfies a
        // slot wanting one that accepts only Integer (narrower) -- reversed.
        assert!(subtype_of(
            &model,
            "Object",
            &block("Object", "Integer"),
            &block("Integer", "Integer"),
            &mut trail
        ));
        assert!(!subtype_of(
            &model,
            "Object",
            &block("Integer", "Integer"),
            &block("Object", "Integer"),
            &mut trail
        ));
    }

    #[test]
    fn union_both_sides() {
        let model = model_with_chain("union", &[
            ("Object", None),
            ("Integer", Some("Object")),
            ("Str", Some("Object")),
        ]);
        let mut trail = Trail::new();
        let union = |a: &str, b: &str| {
            ResolvedType::Written(TypeExpr::Union(vec![
                TypeExpr::Named(a.to_string()),
                TypeExpr::Named(b.to_string()),
            ]))
        };
        // Source union: every variant must satisfy T.
        assert!(subtype_of(&model, "Object", &union("Integer", "Str"), &named("Object"), &mut trail));
        assert!(!subtype_of(&model, "Object", &union("Integer", "Str"), &named("Integer"), &mut trail));
        // Target union: S just needs to satisfy one variant.
        assert!(subtype_of(&model, "Object", &named("Integer"), &union("Integer", "Str"), &mut trail));
        assert!(!subtype_of(&model, "Object", &named("Object"), &union("Integer", "Str"), &mut trail));
    }

    #[test]
    fn unrelated_generic_heads_are_not_subtypes() {
        let model = model_with_chain("generic_heads", &[
            ("Object", None),
            ("Cltn", Some("Object")),
            ("Str", Some("Object")),
        ]);
        let mut trail = Trail::new();
        let generic = |head: &str| {
            ResolvedType::Written(TypeExpr::Generic {
                head: head.to_string(),
                args: vec![TypeExpr::Named("Object".to_string())],
            })
        };
        assert!(!subtype_of(&model, "Object", &generic("Str"), &generic("Cltn"), &mut trail));
        assert!(subtype_of(&model, "Object", &generic("Cltn"), &named("Object"), &mut trail));
    }

    #[test]
    fn cycle_guard_never_loops_forever() {
        // A malformed model with a superclass CYCLE -- shouldn't arise from
        // real .mst source in one pass, but `ClassInterface::merge`
        // overwrites `superclass` unconditionally on every reopen with no
        // validation, so a THREE-FILE sequence (declare A under Object,
        // declare B under A, then REOPEN A naming B as its superclass)
        // legitimately builds an A -> B -> A cycle through the real loader
        // -- exactly the shape the trail's cycle guard exists to survive.
        let dir = std::env::temp_dir().join(format!(
            "macvm_types_subtype_cycle_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("01_object.mst"), "nil subclass: Object [\n]\n").unwrap();
        std::fs::write(dir.join("02_a.mst"), "Object subclass: A [\n]\n").unwrap();
        std::fs::write(dir.join("03_b.mst"), "A subclass: B [\n]\n").unwrap();
        std::fs::write(dir.join("04_reopen_a.mst"), "B subclass: A [\n]\n").unwrap();
        std::fs::write(
            dir.join("world.list"),
            "01_object.mst\n02_a.mst\n03_b.mst\n04_reopen_a.mst\n",
        )
        .unwrap();
        let model = super::super::interface::build_world_model(&dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        let mut trail = Trail::new();
        assert!(!subtype_of(&model, "A", &named("A"), &named("Zorp"), &mut trail));
    }
}
