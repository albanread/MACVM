//! T3 (`docs/typechecker_design.md` §4/§6): the send rule — static-DNU +
//! per-argument subtype checks. Only ever checks a send whose RECEIVER
//! type is KNOWN (not `Dynamic`) — an unannotated receiver (still the
//! overwhelming majority of the real world today) is never checked at
//! all, matching the design's own "gradual typing finds little until
//! signatures exist." Cascades reuse this over the SAME receiver type for
//! every segment (`expr_type.rs`'s own Cascade handling), matching
//! `DeltaCascadedSend`'s rule.
//!
//! Known v1 simplification, stated rather than silently ignored: this
//! type system has no metaclass representation at all (a receiver's
//! `ResolvedType` only ever means "an instance of class X," never "the
//! class X itself"). A class-side send (`Foo new`) is unreachable by
//! construction — `expr_type::synthesize`'s `Var` case already answers
//! `Dynamic` for a bare global/class-name reference, so `find_method`
//! only ever needs to search `instance_methods`. The one place this
//! WOULD otherwise bite is `self`/`Self`/`super` used as a receiver
//! INSIDE a class-side method (there, `self` really means "the class,"
//! not "an instance of it") — `check_send` explicitly declines those
//! rather than silently checking them against the wrong table.

use crate::frontend::ast::MethodNode;

use super::check::{AnnotationSite, TypeError};
use super::interface::WorldModel;
use super::subtype::{subtype_of, ResolvedType, Trail};
use super::type_expr::{self, TypeExpr};

/// Finds `selector` on `start_class`'s own INSTANCE-method dictionary, or
/// (Smalltalk inheritance) any ancestor's — the same chain
/// `subtype::superclass_chain_contains` walks, but looking for a method
/// instead of comparing two names. A cycle guard mirrors that function's
/// own (defensive only — a real Smalltalk hierarchy can't cycle).
///
/// If the ancestor climb fails, ALSO checks whether any DESCENDANT of
/// `start_class` implements the selector — found empirically necessary:
/// the ordinary Template Method pattern (`Number`/`Integer` declare
/// arithmetic ALGORITHMS in terms of `+`/`-`/`*`/etc, but never implement
/// those operators themselves — only their concrete leaf subclasses
/// `SmallInteger`/`LargeInteger`/`Double` do) is pervasive in real
/// Smalltalk code, and is exactly as common for `Integer`-typed LITERALS
/// as it is for `self`-sends inside an abstract superclass's own methods.
/// A v1 checker with no protocol/abstract-method-declaration machinery
/// (`docs/typechecker_design.md`'s own non-goal) can't safely determine
/// WHICH concrete subclass's implementation would run, so it can't check
/// arguments against one — but the WEAKER question "does ANYONE in this
/// class's whole family tree implement this at all" is a much more
/// reliable signal of a genuine typo than "does the static type's OWN
/// ancestor chain."
fn find_method<'m>(model: &'m WorldModel, start_class: &str, selector: &str) -> Lookup<'m> {
    let mut current = Some(start_class.to_string());
    let mut steps = 0usize;
    while let Some(name) = current {
        steps += 1;
        if steps > model.classes.len() + 1 {
            break;
        }
        let Some(class) = model.classes.get(&name) else {
            break;
        };
        if let Some(m) = class.instance_methods.get(selector) {
            return Lookup::Found(m);
        }
        current = class.superclass.clone();
    }
    if model
        .classes
        .iter()
        .any(|(name, class)| {
            class.instance_methods.contains_key(selector) && is_descendant_of(model, name, start_class)
        })
    {
        return Lookup::PlausibleViaDescendant;
    }
    Lookup::NotFound
}

/// Does `candidate`'s own superclass chain (inclusive of `candidate`
/// itself, harmlessly redundant with `find_method`'s own prior ancestor
/// climb) eventually reach `ancestor`?
fn is_descendant_of(model: &WorldModel, candidate: &str, ancestor: &str) -> bool {
    let mut current = Some(candidate.to_string());
    let mut steps = 0usize;
    while let Some(name) = current {
        steps += 1;
        if steps > model.classes.len() + 1 {
            return false;
        }
        if name == ancestor {
            return true;
        }
        let Some(class) = model.classes.get(&name) else {
            return false;
        };
        current = class.superclass.clone();
    }
    false
}

enum Lookup<'m> {
    /// Found via the ancestor chain — safe to fully check (arguments,
    /// propagate the declared return type).
    Found(&'m MethodNode),
    /// Not found via ancestors, but SOME descendant implements it — the
    /// Template Method pattern. Not a bug: decline further checking
    /// (there's no single safe signature to check arguments against when
    /// we don't know which concrete subclass will actually run).
    PlausibleViaDescendant,
    /// Understood NOWHERE in this class's whole family tree — a much
    /// stronger signal of a genuine typo or missing method.
    NotFound,
}

/// The nominal class name a receiver type's send should be looked up
/// against — `None` for `Dynamic` (nothing to check), a `Block`/`Union`/
/// `InferenceDef` receiver (not modeled for send-checking in v1: a block's
/// own protocol isn't captured anywhere the checker can consult, and a
/// union receiver would need EVERY variant to understand the selector,
/// deferred rather than guessed at) — OR a bare `Self` receiver, and this
/// is the important one, found empirically: resolving `Self` to
/// `self_class` and searching ITS OWN dictionary is UNSOUND the instant a
/// method calls `self someSelector` where `someSelector` is only ever
/// implemented on CONCRETE SUBCLASSES (the ordinary abstract-superclass /
/// template-method pattern — e.g. `Number>>log` sends `self asFloat`, but
/// `Number` itself never implements `asFloat`, only `Integer`/`Double`/
/// `Fraction` do). Properly checking a `Self`-typed send needs real
/// subclass-responsibility/"MyType" polymorphism machinery (verify every
/// concrete subclass provides the selector, or trust an explicit
/// deferred-method declaration) that v1 doesn't have — so, matching the
/// class-side-self precedent, DECLINE rather than produce a naive,
/// always-wrong-for-abstract-classes result. `subtype::subtype_of`'s OWN
/// `Self` resolution (assignment/return checking) is unaffected and stays
/// sound — `Self <: T` holds whenever `self_class <: T` holds, which is
/// true for every possible concrete subclass, not just an approximation.
fn nominal_head(t: &ResolvedType) -> Option<String> {
    match t {
        ResolvedType::Dynamic => None,
        ResolvedType::Written(TypeExpr::Named(n)) if n == "Self" => None,
        ResolvedType::Written(TypeExpr::Named(n)) => Some(n.clone()),
        ResolvedType::Written(TypeExpr::Generic { head, .. }) if head == "Self" => None,
        ResolvedType::Written(TypeExpr::Generic { head, .. }) => Some(head.clone()),
        ResolvedType::Written(TypeExpr::Block { .. } | TypeExpr::Union(_) | TypeExpr::InferenceDef(_)) => {
            None
        }
    }
}

fn resolve_captured(ann: &Option<crate::frontend::ast::TypeAnnotation>) -> ResolvedType {
    ann.as_ref()
        .and_then(|a| type_expr::parse(&a.text).ok())
        .map(ResolvedType::Written)
        .unwrap_or(ResolvedType::Dynamic)
}

/// Checks one send: static-DNU (`selector` unresolved anywhere on the
/// lookup chain) if the receiver's type is known but doesn't understand
/// it, then each already-synthesized argument against the found method's
/// declared formal (`SendArgNotSubtype`). Answers the send's own result
/// type — the found method's declared return, or `Dynamic` if either the
/// receiver wasn't checkable, the selector wasn't found, or the arity
/// doesn't match (an arity mismatch is a real compile error the VM
/// already catches elsewhere — `interface.rs`'s own stated policy for a
/// dangling superclass reference applies here too: not this checker's job
/// to re-diagnose).
#[allow(clippy::too_many_arguments)]
pub fn check_send(
    model: &WorldModel,
    self_class: &str,
    checking_class_side: bool,
    is_super: bool,
    receiver_type: &ResolvedType,
    selector: &str,
    arg_types: &[ResolvedType],
    site: &AnnotationSite,
    errors: &mut Vec<TypeError>,
) -> ResolvedType {
    // A super send's RECEIVER is `self` (`Named("Self")`, same as any
    // ordinary self-send -- `parser::parse_cascade`'s sibling,
    // `"super" => (Expr::SelfRef(span), true)`), but its LOOKUP starts at
    // a STATIC, lexically-fixed point -- the method's OWN enclosing
    // class's superclass -- regardless of which concrete subclass `self`
    // actually is at runtime. That sidesteps `nominal_head`'s "decline
    // Self" rule entirely: super's start point is EXACT, not an
    // approximation across every possible subclass, so it's handled here
    // first and separately, never falling through to the ordinary
    // (declined) Self-receiver path below.
    let head = if is_super {
        if checking_class_side {
            // No metaclass modeling -- a class-side `super` would need to
            // search the SUPER-METACLASS's own class-side methods, which
            // `find_method` (instance-methods only) can't do correctly.
            return ResolvedType::Dynamic;
        }
        match model.classes.get(self_class).and_then(|c| c.superclass.clone()) {
            Some(super_name) => super_name,
            None => return ResolvedType::Dynamic, // Object's nil superclass, or unmodeled
        }
    } else {
        match nominal_head(receiver_type) {
            Some(h) => h,
            None => return ResolvedType::Dynamic,
        }
    };
    let method = match find_method(model, &head, selector) {
        Lookup::Found(m) => m,
        Lookup::PlausibleViaDescendant => return ResolvedType::Dynamic,
        Lookup::NotFound => {
            errors.push(TypeError::SelectorUndefined {
                site: site.clone(),
                receiver_type: receiver_type.to_string(),
                selector: selector.to_string(),
            });
            return ResolvedType::Dynamic;
        }
    };
    if method.params.len() != arg_types.len() {
        return ResolvedType::Dynamic;
    }
    let mut trail = Trail::new();
    for (i, (formal, actual)) in method
        .type_sig
        .param_types
        .iter()
        .zip(arg_types.iter())
        .enumerate()
    {
        let declared = resolve_captured(formal);
        if !subtype_of(model, self_class, actual, &declared, &mut trail) {
            errors.push(TypeError::SendArgNotSubtype {
                site: site.clone(),
                selector: selector.to_string(),
                arg_index: i,
                declared: declared.to_string(),
                actual: actual.to_string(),
            });
        }
    }
    resolve_captured(&method.type_sig.return_type)
}
