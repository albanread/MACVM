//! Scope tree + capture analysis (`sprint_s05_detail.md` §Algorithms
//! "Capture analysis"). Runs once per `MethodNode` before codegen: builds
//! the scope tree (pre-order `scope_id` numbering, written back into every
//! `BlockNode`), marks which slot variables are captured (referenced across
//! a block boundary — read-or-write, no read-only-copy optimization in v1,
//! `copied[] ` stays empty per the SPEC-QUESTION), and assigns each
//! variable its final storage: a Context slot (captured) or a frame temp
//! slot (not captured). `CaptureInfo::resolve` is codegen's single lookup
//! entry point for every `Var`/`Assign` reference.

use std::collections::HashMap;

use super::ast::{BlockNode, Expr, MethodNode};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VarSlot {
    pub captured: bool,
    /// Always `Some` for a parameter (its fixed 0..argc-1 position, per the
    /// unified arg/temp frame convention — captured params still receive
    /// their value there and get copied into the Context by a codegen
    /// prologue move). `Some` for a temp only when NOT captured.
    pub frame_slot: Option<usize>,
    /// `Some` iff `captured` — this variable's index into its OWN scope's
    /// Context (params-before-temps declaration order among the captured
    /// subset only).
    pub ctx_slot: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct ScopeInfo {
    pub parent: Option<u32>,
    pub is_method: bool,
    pub argc: usize,
    /// One entry per declared name (params then temps, declaration order).
    pub vars: Vec<(String, VarSlot)>,
    /// Context slot count (`MethodOop::nctx`).
    pub nctx: usize,
    /// Non-captured-temp frame slot count (`MethodOop::ntemps`) — params are
    /// NOT part of this count (SPEC §4.4/S2's unified arg/temp convention).
    pub frame_temp_count: usize,
    pub has_ctx: bool,
    /// `MethodOop::captures_ctx` (S4, only meaningful for a block: never set
    /// for the method scope). True iff SOME reference anywhere in this
    /// scope's own subtree (itself or any nested block, transitively)
    /// resolves to a captured variable — i.e. this scope's frame `context`
    /// slot must hold a real, non-nil value (own fresh Context or an alias)
    /// for that access, or a deeper descendant's access, to work at all
    /// (`interpreter::blocks::activate_block`'s `enclosing` computation).
    pub captures_ctx: bool,
}

impl ScopeInfo {
    fn find(&self, name: &str) -> Option<&VarSlot> {
        self.vars.iter().find(|(n, _)| n == name).map(|(_, s)| s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// `push_temp`/`store_temp_pop <frame_slot>`.
    Local(usize),
    /// `push_ctx_temp`/`store_ctx_temp_pop <depth> <ctx_slot>`.
    Captured { depth: u8, ctx_slot: u8 },
}

pub struct CaptureInfo {
    /// Indexed by `scope_id`; `0` is always the method's own scope.
    pub scopes: Vec<ScopeInfo>,
}

impl CaptureInfo {
    /// Resolves a slot-variable reference from `referencing_scope`, walking
    /// the static (lexical) parent chain with shadowing (innermost
    /// declaration wins). `None` means `name` is not a slot variable
    /// anywhere in the chain — codegen falls through to instvar/classvar/
    /// global resolution.
    pub fn resolve(&self, referencing_scope: u32, name: &str) -> Option<Resolved> {
        let mut sid = referencing_scope;
        loop {
            let scope = &self.scopes[sid as usize];
            if let Some(slot) = scope.find(name) {
                return Some(if slot.captured {
                    Resolved::Captured {
                        depth: self.depth_to(referencing_scope, sid),
                        ctx_slot: slot.ctx_slot.expect("captured var has no ctx_slot") as u8,
                    }
                } else {
                    Resolved::Local(slot.frame_slot.expect("local var has no frame_slot"))
                });
            }
            sid = scope.parent?;
        }
    }

    /// SPEC §5.4 depth numbering: the count of `has_ctx` scopes on the
    /// parent chain from `from` (inclusive) up to but excluding `to` — a
    /// ctx-less scope's frame `context` slot already aliases the nearest
    /// REAL enclosing Context (S4's `activate_block`), so walking past one
    /// costs no hop.
    fn depth_to(&self, from: u32, to: u32) -> u8 {
        let mut depth = 0u32;
        let mut sid = from;
        while sid != to {
            if self.scopes[sid as usize].has_ctx {
                depth += 1;
            }
            sid = self.scopes[sid as usize]
                .parent
                .expect("depth_to: ran off the root before reaching the defining scope");
        }
        depth as u8
    }
}

struct ScopeBuilder {
    parent: Option<u32>,
    is_method: bool,
    argc: usize,
    names: Vec<String>,
    captured: Vec<bool>,
}

fn new_scope_names(params: &[String], temps: &[String]) -> Vec<String> {
    params.iter().chain(temps.iter()).cloned().collect()
}

/// Which position(s) of an inlinable-shaped `Send` hold the literal blocks
/// that get dissolved (`sprint_s05_detail.md` §Algorithms "Inlined
/// control-flow selectors"). Shared verbatim between capture analysis
/// (which must NOT create a separate scope for a dissolved block — its
/// temps become the enclosing scope's own slots) and codegen (which must
/// emit jumps, not a real send, for the exact same shapes) — `codegen.rs`
/// calls this same function so the two can never disagree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockPos {
    Receiver,
    Arg(usize),
}

/// `None` = not inlinable (compile a real send). Preconditions (verbatim):
/// controlling block(s) must be literal `[...]` expressions written
/// directly at the call site, with the exact arity listed; `whileTrue:`/
/// `whileFalse:`'s receiver must also be a literal block; `to:do:` bails to
/// a real send if its loop-variable param is captured by a block nested
/// inside the body (freshness — Pitfalls P6).
pub fn inline_block_positions(
    selector: &str,
    receiver: &Expr,
    args: &[Expr],
) -> Option<Vec<BlockPos>> {
    fn block_argc(e: &Expr) -> Option<usize> {
        match e {
            Expr::Block(b) => Some(b.params.len()),
            _ => None,
        }
    }
    match (selector, args.len()) {
        ("ifTrue:", 1) | ("ifFalse:", 1) | ("and:", 1) | ("or:", 1)
            if block_argc(&args[0]) == Some(0) =>
        {
            Some(vec![BlockPos::Arg(0)])
        }
        ("ifTrue:ifFalse:", 2) | ("ifFalse:ifTrue:", 2)
            if block_argc(&args[0]) == Some(0) && block_argc(&args[1]) == Some(0) =>
        {
            Some(vec![BlockPos::Arg(0), BlockPos::Arg(1)])
        }
        ("whileTrue:", 1) | ("whileFalse:", 1)
            if block_argc(receiver) == Some(0) && block_argc(&args[0]) == Some(0) =>
        {
            Some(vec![BlockPos::Receiver, BlockPos::Arg(0)])
        }
        // S15 (the sieve unlock): `n timesRepeat: [body]` — a zero-arg
        // literal block repeated receiver-many times. Not inlining it made
        // any method using it permanently uncompilable (the block escapes
        // into a real send), which is exactly how sieve's whole `run` — and
        // every hot loop inside it — stayed interpreted forever.
        ("timesRepeat:", 1) if block_argc(&args[0]) == Some(0) => Some(vec![BlockPos::Arg(0)]),
        // The nil-guard family. `ifNotNil:`'s block optionally takes the
        // (non-nil) receiver value as its single param — so 0 OR 1 arg is
        // inlinable there; the `ifNil:` block is always 0-arg. All desugar
        // to an identity test against nil plus dissolved bodies (codegen).
        ("ifNil:", 1) if block_argc(&args[0]) == Some(0) => Some(vec![BlockPos::Arg(0)]),
        ("ifNotNil:", 1) if matches!(block_argc(&args[0]), Some(0) | Some(1)) => {
            Some(vec![BlockPos::Arg(0)])
        }
        ("ifNil:ifNotNil:", 2)
            if block_argc(&args[0]) == Some(0)
                && matches!(block_argc(&args[1]), Some(0) | Some(1)) =>
        {
            Some(vec![BlockPos::Arg(0), BlockPos::Arg(1)])
        }
        ("ifNotNil:ifNil:", 2)
            if matches!(block_argc(&args[0]), Some(0) | Some(1))
                && block_argc(&args[1]) == Some(0) =>
        {
            Some(vec![BlockPos::Arg(0), BlockPos::Arg(1)])
        }
        // `[body] repeat` — an unconditional loop over a literal 0-arg block
        // receiver. Exits only via a `^`/non-local return inside the body;
        // the back-edge poll keeps it interruptible and GC-safe.
        ("repeat", 0) if block_argc(receiver) == Some(0) => Some(vec![BlockPos::Receiver]),
        ("to:do:", 2) if block_argc(&args[1]) == Some(1) => {
            let Expr::Block(b) = &args[1] else {
                unreachable!()
            };
            let loop_var = &b.params[0];
            if to_do_needs_real_send(loop_var, &b.body) {
                None
            } else {
                Some(vec![BlockPos::Arg(1)])
            }
        }
        _ => None,
    }
}

/// P6: does `body` (a `to:do:` body block's statements) reference
/// `loop_var` from within a block NESTED inside it (any depth) — a
/// reference at `body`'s own top level is the normal, correctly-scoped
/// case and does not count.
fn to_do_needs_real_send(loop_var: &str, body: &[Expr]) -> bool {
    fn scan(e: &Expr, loop_var: &str, in_nested_block: bool, found: &mut bool) {
        if *found {
            return;
        }
        match e {
            Expr::Var { name, .. } => {
                if in_nested_block && name == loop_var {
                    *found = true;
                }
            }
            Expr::Assign { name, value, .. } => {
                if in_nested_block && name == loop_var {
                    *found = true;
                }
                scan(value, loop_var, in_nested_block, found);
            }
            Expr::Send {
                receiver,
                selector,
                args,
                ..
            } => {
                // S15: a send whose literal-block arguments will themselves
                // be DISSOLVED (ifTrue:/whileTrue:/to:do:/timesRepeat: — the
                // recursive `inline_block_positions` check) creates no
                // closure: references from inside those blocks compile in
                // the SAME scope, so they don't force a real `to:do:` send.
                // Only blocks in genuinely non-inlinable positions keep
                // marking `in_nested_block`. (Before this, sieve's second
                // loop — `i` referenced inside its `ifTrue:` arm — was
                // declined, which left the whole enclosing method with an
                // escaping closure and thus permanently uncompilable.)
                let dissolving = inline_block_positions(selector, receiver, args);
                let pos_dissolves =
                    |p: BlockPos| dissolving.as_ref().is_some_and(|v| v.contains(&p));
                match receiver.as_ref() {
                    Expr::Block(b) if pos_dissolves(BlockPos::Receiver) => {
                        for stmt in &b.body {
                            scan(stmt, loop_var, in_nested_block, found);
                        }
                    }
                    r => scan(r, loop_var, in_nested_block, found),
                }
                for (i, a) in args.iter().enumerate() {
                    match a {
                        Expr::Block(b) if pos_dissolves(BlockPos::Arg(i)) => {
                            for stmt in &b.body {
                                scan(stmt, loop_var, in_nested_block, found);
                            }
                        }
                        a => scan(a, loop_var, in_nested_block, found),
                    }
                }
            }
            Expr::Cascade {
                receiver, segments, ..
            } => {
                scan(receiver, loop_var, in_nested_block, found);
                for s in segments {
                    scan(s, loop_var, in_nested_block, found);
                }
            }
            Expr::Return { value, .. } => scan(value, loop_var, in_nested_block, found),
            Expr::DynArray { elems, .. } => {
                for el in elems {
                    scan(el, loop_var, in_nested_block, found);
                }
            }
            Expr::Block(b) => {
                for stmt in &b.body {
                    scan(stmt, loop_var, true, found);
                }
            }
            Expr::SelfRef(_) | Expr::CascadeRcvr(_) | Expr::Lit { .. } => {}
        }
    }
    let mut found = false;
    for stmt in body {
        scan(stmt, loop_var, false, &mut found);
    }
    found
}

/// Extends `scope_id`'s OWN declared names with `b`'s params+temps (a
/// dissolved block owns no scope of its own — its locals become ordinary
/// slots of the scope it was inlined into) and walks its body under that
/// same `scope_id`, so nested references resolve exactly as if `b`'s
/// statements had been written directly inline.
fn dissolve_block_into(
    b: &mut BlockNode,
    scope_id: u32,
    builders: &mut Vec<ScopeBuilder>,
    next_id: &mut u32,
    refs: &mut Vec<(u32, String)>,
) {
    let names = new_scope_names(&b.params, &b.temps);
    let n_new = names.len();
    builders[scope_id as usize].names.extend(names);
    builders[scope_id as usize]
        .captured
        .extend(std::iter::repeat_n(false, n_new));
    walk_block_body(&mut b.body, scope_id, builders, next_id, refs);
}

fn walk_expr(
    e: &mut Expr,
    scope_id: u32,
    builders: &mut Vec<ScopeBuilder>,
    next_id: &mut u32,
    refs: &mut Vec<(u32, String)>,
) {
    match e {
        Expr::SelfRef(_) | Expr::CascadeRcvr(_) => {}
        Expr::Var { name, .. } => refs.push((scope_id, name.clone())),
        Expr::Lit { .. } => {}
        Expr::Assign { name, value, .. } => {
            refs.push((scope_id, name.clone()));
            walk_expr(value, scope_id, builders, next_id, refs);
        }
        Expr::Send {
            receiver,
            selector,
            args,
            is_super,
            ..
        } => {
            let positions = if *is_super {
                None
            } else {
                inline_block_positions(selector, receiver, args)
            };
            match positions {
                Some(positions) => {
                    if positions.contains(&BlockPos::Receiver) {
                        if let Expr::Block(b) = receiver.as_mut() {
                            dissolve_block_into(b, scope_id, builders, next_id, refs);
                        }
                    } else {
                        walk_expr(receiver, scope_id, builders, next_id, refs);
                    }
                    for (i, a) in args.iter_mut().enumerate() {
                        if positions.contains(&BlockPos::Arg(i)) {
                            if let Expr::Block(b) = a {
                                dissolve_block_into(b, scope_id, builders, next_id, refs);
                            }
                        } else {
                            walk_expr(a, scope_id, builders, next_id, refs);
                        }
                    }
                }
                None => {
                    walk_expr(receiver, scope_id, builders, next_id, refs);
                    for a in args.iter_mut() {
                        walk_expr(a, scope_id, builders, next_id, refs);
                    }
                }
            }
        }
        Expr::Cascade {
            receiver, segments, ..
        } => {
            walk_expr(receiver, scope_id, builders, next_id, refs);
            for s in segments.iter_mut() {
                walk_expr(s, scope_id, builders, next_id, refs);
            }
        }
        Expr::DynArray { elems, .. } => {
            for el in elems.iter_mut() {
                walk_expr(el, scope_id, builders, next_id, refs);
            }
        }
        Expr::Return { value, .. } => walk_expr(value, scope_id, builders, next_id, refs),
        Expr::Block(blk) => {
            let this_id = *next_id;
            *next_id += 1;
            blk.scope_id = this_id;
            builders.push(ScopeBuilder {
                parent: Some(scope_id),
                is_method: false,
                argc: blk.params.len(),
                names: new_scope_names(&blk.params, &blk.temps),
                captured: vec![false; blk.params.len() + blk.temps.len()],
            });
            walk_block_body(&mut blk.body, this_id, builders, next_id, refs);
        }
    }
}

fn walk_block_body(
    body: &mut [Expr],
    scope_id: u32,
    builders: &mut Vec<ScopeBuilder>,
    next_id: &mut u32,
    refs: &mut Vec<(u32, String)>,
) {
    for e in body.iter_mut() {
        walk_expr(e, scope_id, builders, next_id, refs);
    }
}

fn find_defining_scope(builders: &[ScopeBuilder], mut sid: u32, name: &str) -> Option<u32> {
    loop {
        if builders[sid as usize].names.iter().any(|n| n == name) {
            return Some(sid);
        }
        sid = builders[sid as usize].parent?;
    }
}

fn finalize_scope(b: ScopeBuilder, captures_ctx: bool) -> ScopeInfo {
    let argc = b.argc;
    let n = b.names.len();
    let mut ctx_slots: Vec<Option<usize>> = vec![None; n];
    let mut next_ctx = 0usize;
    for (i, slot) in ctx_slots.iter_mut().enumerate() {
        if b.captured[i] {
            *slot = Some(next_ctx);
            next_ctx += 1;
        }
    }
    let mut frame_slots: Vec<Option<usize>> = vec![None; n];
    let mut next_frame_temp = argc;
    for (i, slot) in frame_slots.iter_mut().enumerate() {
        if i < argc {
            *slot = Some(i);
        } else if !b.captured[i] {
            *slot = Some(next_frame_temp);
            next_frame_temp += 1;
        }
    }
    let nctx = next_ctx;
    let vars = b
        .names
        .into_iter()
        .enumerate()
        .map(|(i, name)| {
            (
                name,
                VarSlot {
                    captured: b.captured[i],
                    frame_slot: frame_slots[i],
                    ctx_slot: ctx_slots[i],
                },
            )
        })
        .collect();
    ScopeInfo {
        parent: b.parent,
        is_method: b.is_method,
        argc,
        vars,
        nctx,
        frame_temp_count: next_frame_temp - argc,
        has_ctx: nctx > 0,
        captures_ctx,
    }
}

/// Runs capture analysis over `method`'s whole body, writing each nested
/// `BlockNode`'s `scope_id` in place (pre-order, scope `0` = the method).
pub fn analyze(method: &mut MethodNode) -> CaptureInfo {
    let mut builders = vec![ScopeBuilder {
        parent: None,
        is_method: true,
        argc: method.params.len(),
        names: new_scope_names(&method.params, &method.temps),
        captured: vec![false; method.params.len() + method.temps.len()],
    }];
    let mut next_id = 1u32;
    let mut refs = Vec::new();
    walk_block_body(&mut method.body, 0, &mut builders, &mut next_id, &mut refs);

    // Mark captures: a reference crosses a scope boundary iff its defining
    // scope differs from where it was seen. The SAME crossing also tells us
    // which scopes need a working context chain (`captures_ctx`): every
    // scope strictly between the reference site and its defining scope
    // (reference site inclusive, defining scope exclusive — the exact set
    // `depth_to` walks) must relay a real Context, own or aliased.
    let mut to_mark: HashMap<(u32, usize), ()> = HashMap::new();
    let mut needs_chain = vec![false; builders.len()];
    for (ref_scope, name) in &refs {
        if let Some(def_scope) = find_defining_scope(&builders, *ref_scope, name) {
            if def_scope != *ref_scope {
                let idx = builders[def_scope as usize]
                    .names
                    .iter()
                    .position(|n| n == name)
                    .expect("find_defining_scope returned a scope without `name`");
                to_mark.insert((def_scope, idx), ());

                let mut sid = *ref_scope;
                while sid != def_scope {
                    needs_chain[sid as usize] = true;
                    sid = builders[sid as usize]
                        .parent
                        .expect("walked off the root before reaching the defining scope");
                }
            }
        }
    }
    for (sid, idx) in to_mark.keys() {
        builders[*sid as usize].captured[*idx] = true;
    }

    let scopes = builders
        .into_iter()
        .enumerate()
        .map(|(i, b)| {
            // `captures_ctx` is a block-only concept — the method's own
            // Context (if any) never links to anything further out.
            let cc = !b.is_method && needs_chain[i];
            finalize_scope(b, cc)
        })
        .collect();
    CaptureInfo { scopes }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::ast::BlockNode;
    use crate::frontend::lexer::Span;

    fn sp() -> Span {
        Span { line: 0, col: 0 }
    }

    fn var(name: &str) -> Expr {
        Expr::Var {
            name: name.to_string(),
            span: sp(),
        }
    }

    fn block(params: &[&str], temps: &[&str], body: Vec<Expr>) -> Expr {
        Expr::Block(Box::new(BlockNode {
            params: params.iter().map(|s| s.to_string()).collect(),
            temps: temps.iter().map(|s| s.to_string()).collect(),
            body,
            span: sp(),
            scope_id: 0,
        }))
    }

    /// Descends `path` (a sequence of body-indices, each expected to land
    /// on a nested block) and returns that block's `scope_id`. MUST be
    /// called AFTER `analyze` — a pre-analyze `Expr::Block` still carries
    /// its placeholder `scope_id: 0`, indistinguishable from the method's
    /// own real scope 0 (a footgun `analyze`'s own doc comment warns about).
    fn scope_at(body: &[Expr], path: &[usize]) -> u32 {
        let mut cur = body;
        let mut found = None;
        for &i in path {
            match &cur[i] {
                Expr::Block(b) => {
                    found = Some(b.scope_id);
                    cur = &b.body;
                }
                _ => panic!("scope_at: expected a block at this path position"),
            }
        }
        found.expect("scope_at: empty path")
    }

    fn method(params: &[&str], temps: &[&str], body: Vec<Expr>) -> MethodNode {
        MethodNode {
            pattern_selector: "m".to_string(),
            params: params.iter().map(|s| s.to_string()).collect(),
            primitive: None,
            ffi: None,
            temps: temps.iter().map(|s| s.to_string()).collect(),
            body,
            class_side: false,
            span: sp(),
        }
    }

    #[test]
    fn capture_basic() {
        // method | t | [ t ]  -- a single directly-nested block reading its
        // enclosing method's temp. Per S4's real activation semantics, a
        // ctx-less block's own `context` slot already ALIASES its
        // immediately-enclosing (context-owning) frame's Context — so
        // reading straight through it costs 0 hops, not 1: `push_ctx_temp 0
        // <slot>` reads `t` directly via the aliased context.
        let blk = block(&[], &[], vec![var("t")]);
        let mut m = method(&[], &["t"], vec![blk]);
        let info = analyze(&mut m);
        let bscope = scope_at(&m.body, &[0]);

        assert!(info.scopes[0].has_ctx);
        assert_eq!(info.scopes[0].nctx, 1);
        let (_, slot) = &info.scopes[0].vars[0];
        assert!(slot.captured);
        assert_eq!(slot.ctx_slot, Some(0));

        assert_eq!(
            info.resolve(bscope, "t"),
            Some(Resolved::Captured {
                depth: 0,
                ctx_slot: 0
            })
        );
    }

    #[test]
    fn capture_none() {
        // method | t | [ ^t ]  read only in the method body itself -> a
        // plain frame slot, no Context at all.
        let mut m = method(&[], &["t"], vec![var("t")]);
        let info = analyze(&mut m);
        assert!(!info.scopes[0].has_ctx);
        assert_eq!(info.scopes[0].frame_temp_count, 1);
        let (_, slot) = &info.scopes[0].vars[0];
        assert!(!slot.captured);
        assert_eq!(slot.frame_slot, Some(0)); // argc=0, first temp slot
        assert_eq!(info.resolve(0, "t"), Some(Resolved::Local(0)));
    }

    #[test]
    fn capture_param() {
        // method: p [ p ]  -- `p` captured -> frame slot 0 (ABI) AND ctx
        // slot 0 (the prologue-move destination codegen emits).
        let blk = block(&[], &[], vec![var("p")]);
        let mut m = method(&["p"], &[], vec![blk]);
        let info = analyze(&mut m);
        let (_, slot) = &info.scopes[0].vars[0];
        assert!(slot.captured);
        assert_eq!(slot.frame_slot, Some(0));
        assert_eq!(slot.ctx_slot, Some(0));
    }

    #[test]
    fn capture_depth3() {
        // method | mt | [ B1 | t1 | [ B2 | t2 | [ B3: refs mt, t1, t2 ] ] ]
        // Each enclosing level owns exactly one var captured by B3 — this
        // makes every intermediate level `has_ctx`, so depth counts up by
        // exactly one per level: t2 (own parent) -> 0, t1 (grandparent) ->
        // 1, mt (great-grandparent) -> 2.
        let b3 = block(&[], &[], vec![var("mt"), var("t1"), var("t2")]);
        let b2 = block(&[], &["t2"], vec![b3]);
        let b1 = block(&[], &["t1"], vec![b2]);
        let mut m = method(&[], &["mt"], vec![b1]);
        let info = analyze(&mut m);
        let b3_scope = scope_at(&m.body, &[0, 0, 0]);

        assert_eq!(
            info.resolve(b3_scope, "t2"),
            Some(Resolved::Captured {
                depth: 0,
                ctx_slot: 0
            })
        );
        assert_eq!(
            info.resolve(b3_scope, "t1"),
            Some(Resolved::Captured {
                depth: 1,
                ctx_slot: 0
            })
        );
        assert_eq!(
            info.resolve(b3_scope, "mt"),
            Some(Resolved::Captured {
                depth: 2,
                ctx_slot: 0
            })
        );
    }

    #[test]
    fn capture_skip_level() {
        // method | mt | [ B(no ctx of its own) [ C | ct | [ D: refs mt, ct ] ] ]
        // B declares nothing and captures nothing, so it never becomes
        // `has_ctx` — walking past it from D to M must cost 0 hops, not 1:
        // naive lexical nesting is 3 levels (D->C->B->M) but the
        // context-owning-only count is just 1 (through C alone).
        let d = block(&[], &[], vec![var("mt"), var("ct")]);
        let c = block(&[], &["ct"], vec![d]);
        let b = block(&[], &[], vec![c]);
        let mut m = method(&[], &["mt"], vec![b]);
        let info = analyze(&mut m);
        let d_scope = scope_at(&m.body, &[0, 0, 0]);

        // Confirm the middle level really is ctx-less (the interesting
        // precondition this test exists to exercise).
        let b_scope = scope_at(&m.body, &[0]);
        assert!(!info.scopes[b_scope as usize].has_ctx);
        // ...but B must still relay a working context chain down to C/D
        // (`captures_ctx`), else D's own aliasing/linkage would see nil.
        assert!(info.scopes[b_scope as usize].captures_ctx);

        assert_eq!(
            info.resolve(d_scope, "mt"),
            Some(Resolved::Captured {
                depth: 1,
                ctx_slot: 0
            })
        );
    }

    #[test]
    fn capture_slot_order() {
        // Declaration order (params before temps) determines ctx slot
        // index, regardless of the order references occur in.
        let blk = block(&[], &[], vec![var("t2"), var("t1"), var("p2"), var("p1")]);
        let mut m = method(&["p1", "p2"], &["t1", "t2"], vec![blk]);
        let info = analyze(&mut m);
        let bscope = scope_at(&m.body, &[0]);

        assert_eq!(
            info.resolve(bscope, "p1"),
            Some(Resolved::Captured {
                depth: 0,
                ctx_slot: 0
            })
        );
        assert_eq!(
            info.resolve(bscope, "p2"),
            Some(Resolved::Captured {
                depth: 0,
                ctx_slot: 1
            })
        );
        assert_eq!(
            info.resolve(bscope, "t1"),
            Some(Resolved::Captured {
                depth: 0,
                ctx_slot: 2
            })
        );
        assert_eq!(
            info.resolve(bscope, "t2"),
            Some(Resolved::Captured {
                depth: 0,
                ctx_slot: 3
            })
        );
    }
}
