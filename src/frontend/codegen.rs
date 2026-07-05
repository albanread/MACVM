//! AST → `CompiledMethod`/`CompiledBlock` (`sprint_s05_detail.md` §Design
//! "Codegen patterns", "Inlined control-flow selectors", "Literal frame &
//! IC table construction"). One `BytecodeBuilder` session per compiled
//! unit (method or block); nested blocks build their own session and get
//! interned into the enclosing one's literal frame (the S4 nested-builder
//! pattern, generalized to arbitrary AST-driven recursion).

use std::collections::HashMap;

use crate::bytecode::BytecodeBuilder;
use crate::memory::alloc;
use crate::memory::handles::{Handle, HandleScope};
use crate::oops::layout::{METHOD_ARGC_MAX, METHOD_NCTX_MAX, METHOD_NTEMPS_MAX};
use crate::oops::smi::SmallInt;
use crate::oops::wrappers::{ArrayOop, KlassOop, MemOop, MethodOop};
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;

use super::ast::{BlockNode, Expr, Literal, MethodNode};
use super::capture::{self, BlockPos, CaptureInfo, Resolved, ScopeInfo};
use super::lexer::Span;
use super::CompileError;

// --- name resolution (SPEC "Name resolution") -------------------------------

enum VarRef {
    Local(u8),
    Captured {
        depth: u8,
        ctx_slot: u8,
    },
    InstVar(u8),
    /// An Association oop (global OR class variable — same push_global/
    /// store_global_pop mechanism, SPEC §4.1).
    Global(Oop),
}

/// Root-first flattened instance-variable-name list (`Oop`s, each a
/// Symbol) — index = the `push_instvar`/`store_instvar_pop` operand.
/// Class-side methods see the metaclass's chain, empty in v1 (SPEC-QUESTION
/// A5) — callers pass `class_side` to skip this walk entirely.
fn flatten_inst_vars(klass: KlassOop) -> Vec<Oop> {
    let mut chain = Vec::new();
    let mut k = Some(klass);
    while let Some(kk) = k {
        chain.push(kk);
        k = KlassOop::try_from(kk.superclass());
    }
    chain.reverse();
    let mut names = Vec::new();
    for kk in chain {
        if let Some(arr) = ArrayOop::try_from(kk.inst_var_names()) {
            for i in 0..arr.len() {
                names.push(arr.at(i));
            }
        }
    }
    names
}

pub(crate) fn find_class_var(klass: KlassOop, name: Oop) -> Option<Oop> {
    let mut k = Some(klass);
    while let Some(kk) = k {
        if let Some(arr) = ArrayOop::try_from(kk.class_vars()) {
            for i in 0..arr.len() {
                let assoc = arr.at(i);
                if let Some(m) = MemOop::try_from(assoc) {
                    if m.body_oop(0).raw() == name.raw() {
                        return Some(assoc);
                    }
                }
            }
        }
        k = KlassOop::try_from(kk.superclass());
    }
    None
}

// --- literal frame construction (dedup rules) -------------------------------

/// Per-compiled-unit value-keyed dedup cache (`sprint_s05_detail.md`
/// §Literal frame: "Dedupe within the unit by value" for Doubles/Strings/
/// Characters/BigInts; Symbols dedupe for free via interning; Arrays/
/// ByteArrays are explicitly NOT deduped). Fresh per method/block — never
/// shared across a nested block's own compilation.
///
/// Values are stored as `Handle<Oop>`, not `Oop`: a hit returned much
/// later in the same compile unit, after any number of intervening
/// allocations, must read the object's CURRENT address, not the one it
/// had when cached (S7-9 handle discipline — a bare `HashMap<_, Oop>` is
/// exactly the "invisible root" MacNCL lesson 13 warns about).
///
/// Deliberately owns no `HandleScope` of its own (S7-10 — found via
/// `MACVM_GC_STRESS=1`): two independently, lazily-entered scopes have no
/// fixed relative entry order, since it depends on whichever of
/// cache-a-literal / push-a-literal happens first in the source being
/// compiled. Whichever scope drops first then truncates the other's
/// still-live entries out from under it — the exact failure that
/// corrupted a compiled block's literal frame. Every handle this cache
/// hands out instead comes from `BytecodeBuilder::make_handle`, i.e. the
/// SAME scope as the builder's own literal-frame handles, which is always
/// consumed last (via `finish`) — removing the ordering hazard structurally
/// rather than requiring callers to get manual drop order right.
#[derive(Default)]
struct LitCache {
    doubles: HashMap<u64, Handle<Oop>>,
    strings: HashMap<String, Handle<Oop>>,
    chars: HashMap<char, Handle<Oop>>,
    bigints: HashMap<(bool, Vec<u8>), Handle<Oop>>,
}

impl LitCache {
    fn new() -> LitCache {
        LitCache::default()
    }
}

fn build_literal_value(
    vm: &mut VmState,
    cache: &mut LitCache,
    bb: &mut BytecodeBuilder,
    lit: &Literal,
) -> Oop {
    match lit {
        Literal::Int(v) => SmallInt::new(*v).oop(),
        Literal::BigInt { negative, digits } => {
            let key = (*negative, digits.clone());
            if let Some(h) = cache.bigints.get(&key) {
                return h.get(vm);
            }
            let klass = if *negative {
                vm.universe.large_neg_int_klass
            } else {
                vm.universe.large_pos_int_klass
            };
            let b = alloc::alloc_indexable_bytes(vm, klass, digits.len());
            for (i, &byte) in digits.iter().enumerate() {
                b.byte_at_put(i, byte);
            }
            let h = bb.make_handle(vm, b.oop());
            cache.bigints.insert(key, h);
            b.oop()
        }
        Literal::Float(v) => {
            let bits = v.to_bits();
            if let Some(h) = cache.doubles.get(&bits) {
                return h.get(vm);
            }
            let d = alloc::alloc_double(vm, *v);
            let h = bb.make_handle(vm, d.oop());
            cache.doubles.insert(bits, h);
            d.oop()
        }
        Literal::Char(c) => {
            if let Some(h) = cache.chars.get(c) {
                return h.get(vm);
            }
            let klass = vm.universe.character_klass;
            let obj = alloc::alloc_slots(vm, klass);
            obj.set_body_oop(0, SmallInt::new(*c as i64).oop());
            let h = bb.make_handle(vm, obj.oop());
            cache.chars.insert(*c, h);
            obj.oop()
        }
        Literal::Str(s) => {
            if let Some(h) = cache.strings.get(s) {
                return h.get(vm);
            }
            let klass = vm.universe.string_klass;
            let b = alloc::alloc_indexable_bytes(vm, klass, s.len());
            for (i, byte) in s.bytes().enumerate() {
                b.byte_at_put(i, byte);
            }
            let h = bb.make_handle(vm, b.oop());
            cache.strings.insert(s.clone(), h);
            b.oop()
        }
        Literal::Symbol(s) => vm.universe.intern(s.as_bytes()).oop(),
        Literal::Array(items) => {
            let klass = vm.universe.array_klass;
            let arr = alloc::alloc_indexable_oops(vm, klass, items.len());
            let arr_h = bb.make_handle(vm, arr.oop());
            for (i, it) in items.iter().enumerate() {
                let v = build_literal_value(vm, cache, bb, it);
                // Re-fetched fresh: `build_literal_value`'s own recursive
                // call (or the allocation inside it) may have moved `arr`.
                let arr = ArrayOop::try_from(arr_h.get(vm)).expect("array literal stays an array");
                arr.at_put(i, v);
            }
            arr_h.get(vm)
        }
        Literal::ByteArray(bytes) => {
            let klass = vm.universe.bytearray_klass;
            let b = alloc::alloc_indexable_bytes(vm, klass, bytes.len());
            for (i, &byte) in bytes.iter().enumerate() {
                b.byte_at_put(i, byte);
            }
            b.oop()
        }
        Literal::Nil => vm.universe.nil_obj,
        Literal::True => vm.universe.true_obj,
        Literal::False => vm.universe.false_obj,
    }
}

// --- hoisted-local overrides (inlined control flow) -------------------------

/// A stack of name→frame-slot overrides for dissolved blocks' locals — see
/// `capture::inline_block_positions`' doc: capture analysis already folded
/// these names into the enclosing `ScopeInfo`, so `CaptureInfo::resolve`
/// alone would find them too, but doesn't know which FRESH frame slot each
/// dissolved name landed on (assigned here, at codegen time, past the end
/// of the scope's own `frame_temp_count`) — this stack is checked first.
struct HoistScope {
    names: HashMap<String, u8>,
}

// --- codegen context ---------------------------------------------------------

struct Ctx<'v> {
    vm: &'v mut VmState,
    /// A `Handle`, not a bare `KlassOop`: the class being compiled is read
    /// (via `find_class_var` in `resolve_var`) throughout a compile unit that
    /// spans many intervening allocations (literals, nested block compiles),
    /// any of which can move it. A bare `KlassOop` here was a live production
    /// GC bug — a class-variable reference in an `.mst` method compiled it into
    /// a stale klass under `MACVM_GC_STRESS=1` (SPEC §7.6.1; found via the
    /// in-language suite). Rooted in `scope`, same as `inst_var_names`.
    holder: Handle<KlassOop>,
    class_side: bool,
    top_level: bool,
    capture: CaptureInfo,
    /// Handles, not bare `Oop`s (S7-9): this list is built once, up front,
    /// then read throughout a compile unit that can span many intervening
    /// allocations (literals, nested block compiles) — any of which can
    /// move these symbols.
    inst_var_names: Vec<Handle<Oop>>,
    /// Whether the compiled unit CURRENTLY being emitted (saved/restored
    /// around `emit_block_literal`) is a block — decides which opcode an
    /// explicit `^` inside a DISSOLVED inlined body (`emit_statement_seq`)
    /// must use: `nlr_tos` if we're lexically inside a block, `ret_tos`
    /// otherwise (a dissolved body never gets its own real frame, so this
    /// can't be read off `is_block` the way `emit_body`'s own top-level
    /// Return handling does).
    in_block: bool,
    /// Backs `inst_var_names`; lives for the whole compile unit (outlives
    /// every per-block `LitCache`'s own nested scope). Never read directly
    /// — held only so its `Drop` doesn't truncate `inst_var_names`' handles
    /// early.
    #[allow(dead_code)]
    scope: HandleScope,
}

impl Ctx<'_> {
    fn error(&self, span: Span, msg: impl Into<String>) -> CompileError {
        CompileError {
            path: None,
            span,
            msg: msg.into(),
            eof: false,
        }
    }

    fn resolve_var(
        &mut self,
        hoist: &[HoistScope],
        scope_id: u32,
        name: &str,
        span: Span,
        is_write: bool,
    ) -> Result<VarRef, CompileError> {
        for h in hoist.iter().rev() {
            if let Some(&slot) = h.names.get(name) {
                return Ok(VarRef::Local(slot));
            }
        }
        if let Some(r) = self.capture.resolve(scope_id, name) {
            return Ok(match r {
                Resolved::Local(s) => VarRef::Local(s as u8),
                Resolved::Captured { depth, ctx_slot } => VarRef::Captured { depth, ctx_slot },
            });
        }
        let sym = self.vm.universe.intern(name.as_bytes());
        if !self.class_side {
            let target = sym.oop().raw();
            let idx = self
                .inst_var_names
                .iter()
                .position(|&h| h.get(self.vm).raw() == target);
            if let Some(idx) = idx {
                return Ok(VarRef::InstVar(idx as u8));
            }
        }
        // Read `holder` fresh through its handle: `intern` above (and the
        // whole compile unit before this) can have scavenged and moved it.
        let holder = self.holder.get(self.vm);
        if let Some(assoc) = find_class_var(holder, sym.oop()) {
            return Ok(VarRef::Global(assoc));
        }
        if let Some(assoc) = crate::runtime::globals::global_lookup(self.vm, sym) {
            return Ok(VarRef::Global(assoc));
        }
        if self.top_level && is_write && name.starts_with(|c: char| c.is_uppercase()) {
            return Ok(VarRef::Global(crate::runtime::globals::global_declare(
                self.vm, sym,
            )));
        }
        Err(self.error(span, format!("undeclared variable '{name}'")))
    }
}

fn check_limits(
    cx: &Ctx,
    span: Span,
    argc: usize,
    ntemps: usize,
    nctx: usize,
) -> Result<(), CompileError> {
    if argc > METHOD_ARGC_MAX {
        return Err(cx.error(
            span,
            format!("too many parameters ({argc} > {METHOD_ARGC_MAX})"),
        ));
    }
    if ntemps > METHOD_NTEMPS_MAX {
        return Err(cx.error(
            span,
            format!("too many temporaries/locals ({ntemps} > {METHOD_NTEMPS_MAX})"),
        ));
    }
    if nctx > METHOD_NCTX_MAX {
        return Err(cx.error(
            span,
            format!("too many captured variables ({nctx} > {METHOD_NCTX_MAX})"),
        ));
    }
    Ok(())
}

// --- variable load/store -----------------------------------------------------

fn emit_load(
    cx: &mut Ctx,
    hoist: &[HoistScope],
    b: &mut BytecodeBuilder,
    scope_id: u32,
    name: &str,
    span: Span,
) -> Result<(), CompileError> {
    match cx.resolve_var(hoist, scope_id, name, span, false)? {
        VarRef::Local(s) => {
            b.push_temp(s);
        }
        VarRef::Captured { depth, ctx_slot } => {
            b.push_ctx_temp(depth, ctx_slot);
        }
        VarRef::InstVar(i) => {
            b.push_instvar(i);
        }
        VarRef::Global(assoc) => {
            b.push_global(cx.vm, assoc);
        }
    }
    Ok(())
}

fn emit_store(
    cx: &mut Ctx,
    hoist: &[HoistScope],
    b: &mut BytecodeBuilder,
    scope_id: u32,
    name: &str,
    span: Span,
) -> Result<(), CompileError> {
    match cx.resolve_var(hoist, scope_id, name, span, true)? {
        VarRef::Local(s) => {
            b.store_temp_pop(s);
        }
        VarRef::Captured { depth, ctx_slot } => {
            b.store_ctx_temp_pop(depth, ctx_slot);
        }
        VarRef::InstVar(i) => {
            b.store_instvar_pop(i);
        }
        VarRef::Global(assoc) => {
            b.store_global_pop(cx.vm, assoc);
        }
    }
    Ok(())
}

// --- literal expression -------------------------------------------------------

fn emit_literal(cx: &mut Ctx, cache: &mut LitCache, b: &mut BytecodeBuilder, lit: &Literal) {
    match lit {
        Literal::Int(v) if (-128..=127).contains(v) => {
            b.push_smi_i8(*v as i8);
        }
        Literal::Nil => {
            b.push_nil();
        }
        Literal::True => {
            b.push_true();
        }
        Literal::False => {
            b.push_false();
        }
        other => {
            let v = build_literal_value(cx.vm, cache, b, other);
            b.push_literal(cx.vm, v);
        }
    }
}

// --- expressions (always leave exactly 1 value) ------------------------------

#[allow(clippy::too_many_arguments)]
fn emit_expr(
    cx: &mut Ctx,
    hoist: &mut Vec<HoistScope>,
    cache: &mut LitCache,
    b: &mut BytecodeBuilder,
    scope_id: u32,
    next_hoist_slot: &mut usize,
    e: &Expr,
) -> Result<(), CompileError> {
    match e {
        Expr::SelfRef(_) => {
            b.push_self();
        }
        Expr::CascadeRcvr(_) => {} // already on the stack — see emit_cascade
        Expr::Var { name, span } => emit_load(cx, hoist, b, scope_id, name, *span)?,
        Expr::Lit { value, .. } => emit_literal(cx, cache, b, value),
        Expr::Assign { name, value, span } => {
            emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, value)?;
            b.dup();
            emit_store(cx, hoist, b, scope_id, name, *span)?;
        }
        Expr::Send {
            receiver,
            selector,
            args,
            is_super,
            span,
        } => emit_send(
            cx,
            hoist,
            cache,
            b,
            scope_id,
            next_hoist_slot,
            receiver,
            selector,
            args,
            *is_super,
            *span,
        )?,
        Expr::Cascade {
            receiver, segments, ..
        } => {
            emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, receiver)?;
            let n = segments.len();
            for (i, seg) in segments.iter().enumerate() {
                if i + 1 < n {
                    b.dup();
                    emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, seg)?;
                    b.pop();
                } else {
                    emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, seg)?;
                }
            }
        }
        Expr::Block(blk) => emit_block_literal(cx, cache, b, blk)?,
        Expr::Return { .. } => {
            unreachable!("Return is statement-position only (parser-guaranteed)")
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_send(
    cx: &mut Ctx,
    hoist: &mut Vec<HoistScope>,
    cache: &mut LitCache,
    b: &mut BytecodeBuilder,
    scope_id: u32,
    next_hoist_slot: &mut usize,
    receiver: &Expr,
    selector: &str,
    args: &[Expr],
    is_super: bool,
    span: Span,
) -> Result<(), CompileError> {
    if !is_super {
        if let Some(positions) = capture::inline_block_positions(selector, receiver, args) {
            return emit_inlined(
                cx,
                hoist,
                cache,
                b,
                scope_id,
                next_hoist_slot,
                selector,
                receiver,
                args,
                &positions,
                span,
            );
        }
    }
    emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, receiver)?;
    for a in args {
        emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, a)?;
    }
    let sel = cx.vm.universe.intern(selector.as_bytes());
    if is_super {
        b.send_super(cx.vm, sel, args.len() as u8);
    } else {
        b.send(cx.vm, sel, args.len() as u8);
    }
    Ok(())
}

/// Allocates fresh frame slots (past the scope's already-analyzed
/// `frame_temp_count`) for a dissolved block's params+temps and pushes a
/// `HoistScope` mapping them.
fn push_hoist_scope(hoist: &mut Vec<HoistScope>, next_hoist_slot: &mut usize, blk: &BlockNode) {
    let mut names = HashMap::new();
    for p in blk.params.iter().chain(blk.temps.iter()) {
        let slot = *next_hoist_slot as u8;
        *next_hoist_slot += 1;
        names.insert(p.clone(), slot);
    }
    hoist.push(HoistScope { names });
}

#[allow(clippy::too_many_arguments)]
fn emit_dissolved_body(
    cx: &mut Ctx,
    hoist: &mut Vec<HoistScope>,
    cache: &mut LitCache,
    b: &mut BytecodeBuilder,
    scope_id: u32,
    next_hoist_slot: &mut usize,
    blk: &BlockNode,
) -> Result<(), CompileError> {
    push_hoist_scope(hoist, next_hoist_slot, blk);
    emit_statement_seq(cx, hoist, cache, b, scope_id, next_hoist_slot, &blk.body)?;
    hoist.pop();
    Ok(())
}

/// Emits `body`'s statements as plain VALUE-producing code (no
/// return/nlr/block_return_tos terminator) — used for dissolved bodies,
/// whose final value the surrounding inlined construct consumes directly
/// (e.g. `whileTrue:`'s body value is discarded by the loop; `ifTrue:`'s is
/// the branch's overall value). Empty body -> `nil`.
#[allow(clippy::too_many_arguments)]
fn emit_statement_seq(
    cx: &mut Ctx,
    hoist: &mut Vec<HoistScope>,
    cache: &mut LitCache,
    b: &mut BytecodeBuilder,
    scope_id: u32,
    next_hoist_slot: &mut usize,
    body: &[Expr],
) -> Result<(), CompileError> {
    if body.is_empty() {
        b.push_nil();
        return Ok(());
    }
    let in_block = cx.in_block;
    for stmt in &body[..body.len() - 1] {
        emit_statement_discard(
            cx,
            hoist,
            cache,
            b,
            scope_id,
            next_hoist_slot,
            stmt,
            in_block.then_some(()),
        )?;
    }
    let last = &body[body.len() - 1];
    if let Expr::Return { value, .. } = last {
        // An explicit `^` inside a DISSOLVED body (e.g. `cond ifTrue:
        // [^1]`) never gets its own real frame — it must use whichever
        // return opcode the enclosing (real) unit needs.
        emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, value)?;
        if in_block {
            b.nlr_tos();
        } else {
            b.ret_tos();
        }
        Ok(())
    } else {
        emit_statement_value(cx, hoist, cache, b, scope_id, next_hoist_slot, last)
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_inlined(
    cx: &mut Ctx,
    hoist: &mut Vec<HoistScope>,
    cache: &mut LitCache,
    b: &mut BytecodeBuilder,
    scope_id: u32,
    next_hoist_slot: &mut usize,
    selector: &str,
    receiver: &Expr,
    args: &[Expr],
    positions: &[BlockPos],
    _span: Span,
) -> Result<(), CompileError> {
    let _ = positions;
    let arg_block = |i: usize| match &args[i] {
        Expr::Block(bx) => bx.as_ref(),
        _ => unreachable!("inline_block_positions guarantees a literal block"),
    };
    let recv_block = || match receiver {
        Expr::Block(bx) => bx.as_ref(),
        _ => unreachable!("inline_block_positions guarantees a literal block"),
    };

    match selector {
        "ifTrue:" | "ifFalse:" => {
            emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, receiver)?;
            let l1 = b.new_label();
            let l2 = b.new_label();
            if selector == "ifTrue:" {
                b.br_false_fwd(l1);
            } else {
                b.br_true_fwd(l1);
            }
            emit_dissolved_body(cx, hoist, cache, b, scope_id, next_hoist_slot, arg_block(0))?;
            b.jump_fwd(l2);
            b.bind(l1);
            b.push_nil();
            b.bind(l2);
        }
        "ifTrue:ifFalse:" | "ifFalse:ifTrue:" => {
            emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, receiver)?;
            let l1 = b.new_label();
            let l2 = b.new_label();
            let (true_idx, false_idx) = if selector == "ifTrue:ifFalse:" {
                (0, 1)
            } else {
                (1, 0)
            };
            b.br_false_fwd(l1);
            emit_dissolved_body(
                cx,
                hoist,
                cache,
                b,
                scope_id,
                next_hoist_slot,
                arg_block(true_idx),
            )?;
            b.jump_fwd(l2);
            b.bind(l1);
            emit_dissolved_body(
                cx,
                hoist,
                cache,
                b,
                scope_id,
                next_hoist_slot,
                arg_block(false_idx),
            )?;
            b.bind(l2);
        }
        "and:" | "or:" => {
            emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, receiver)?;
            let l1 = b.new_label();
            let l2 = b.new_label();
            if selector == "and:" {
                b.br_false_fwd(l1);
            } else {
                b.br_true_fwd(l1);
            }
            emit_dissolved_body(cx, hoist, cache, b, scope_id, next_hoist_slot, arg_block(0))?;
            b.jump_fwd(l2);
            b.bind(l1);
            if selector == "and:" {
                b.push_false();
            } else {
                b.push_true();
            }
            b.bind(l2);
        }
        "whileTrue:" | "whileFalse:" => {
            let l0 = b.new_label();
            let l2 = b.new_label();
            b.bind(l0);
            emit_dissolved_body(cx, hoist, cache, b, scope_id, next_hoist_slot, recv_block())?;
            if selector == "whileTrue:" {
                b.br_false_fwd(l2);
            } else {
                b.br_true_fwd(l2);
            }
            emit_dissolved_body(cx, hoist, cache, b, scope_id, next_hoist_slot, arg_block(0))?;
            b.pop();
            b.jump_back(l0);
            b.bind(l2);
            b.push_nil();
        }
        // S15: `n timesRepeat: [body]` — `to:do:`'s shape minus the loop
        // variable. Answers the receiver (Smalltalk's own contract); zero
        // and negative receivers run the body zero times (`1 <= n` fails
        // immediately).
        "timesRepeat:" => {
            let body_blk = arg_block(0);

            emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, receiver)?;
            b.dup();
            let lim_slot = *next_hoist_slot as u8;
            *next_hoist_slot += 1;
            b.store_temp_pop(lim_slot);
            let i_slot = *next_hoist_slot as u8;
            *next_hoist_slot += 1;
            b.push_smi_i8(1);
            b.store_temp_pop(i_slot);

            let l0 = b.new_label();
            let l2 = b.new_label();
            b.bind(l0);
            b.push_temp(i_slot);
            b.push_temp(lim_slot);
            let le_sel = cx.vm.universe.intern(b"<=");
            b.send(cx.vm, le_sel, 1);
            b.br_false_fwd(l2);

            emit_statement_seq(
                cx,
                hoist,
                cache,
                b,
                scope_id,
                next_hoist_slot,
                &body_blk.body,
            )?;
            b.pop();

            b.push_temp(i_slot);
            b.push_smi_i8(1);
            let plus_sel = cx.vm.universe.intern(b"+");
            b.send(cx.vm, plus_sel, 1);
            b.store_temp_pop(i_slot);
            b.jump_back(l0);
            b.bind(l2);
            // The dup'd receiver from setup is now TOS = the result.
        }
        "to:do:" => {
            let body_blk = arg_block(1);
            let loop_var = &body_blk.params[0];

            // Receiver `a`, kept live under the loop (result of the whole
            // expression), then the limit `b_lim` in a fresh hidden slot.
            emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, receiver)?;
            b.dup();
            let i_slot = *next_hoist_slot as u8;
            *next_hoist_slot += 1;
            b.store_temp_pop(i_slot);
            emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, &args[0])?;
            let lim_slot = *next_hoist_slot as u8;
            *next_hoist_slot += 1;
            b.store_temp_pop(lim_slot);

            let l0 = b.new_label();
            let l2 = b.new_label();
            b.bind(l0);
            b.push_temp(i_slot);
            b.push_temp(lim_slot);
            let le_sel = cx.vm.universe.intern(b"<=");
            b.send(cx.vm, le_sel, 1);
            b.br_false_fwd(l2);

            hoist.push(HoistScope {
                names: [(loop_var.clone(), i_slot)].into_iter().collect(),
            });
            emit_statement_seq(
                cx,
                hoist,
                cache,
                b,
                scope_id,
                next_hoist_slot,
                &body_blk.body,
            )?;
            hoist.pop();
            b.pop();

            b.push_temp(i_slot);
            b.push_smi_i8(1);
            let plus_sel = cx.vm.universe.intern(b"+");
            b.send(cx.vm, plus_sel, 1);
            b.store_temp_pop(i_slot);
            b.jump_back(l0);
            b.bind(l2);
            // The dup'd receiver from setup is now TOS = the result.
        }
        _ => unreachable!("inline_block_positions returned a shape emit_inlined doesn't handle"),
    }
    Ok(())
}

// --- statement sequencing (method/block bodies) -----------------------------

#[allow(clippy::too_many_arguments)]
fn emit_statement_discard(
    cx: &mut Ctx,
    hoist: &mut Vec<HoistScope>,
    cache: &mut LitCache,
    b: &mut BytecodeBuilder,
    scope_id: u32,
    next_hoist_slot: &mut usize,
    stmt: &Expr,
    is_block: Option<()>,
) -> Result<(), CompileError> {
    match stmt {
        Expr::Return { value, .. } => {
            emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, value)?;
            if is_block.is_some() {
                b.nlr_tos();
            } else {
                b.ret_tos();
            }
        }
        Expr::Assign { name, value, span } => {
            emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, value)?;
            emit_store(cx, hoist, b, scope_id, name, *span)?;
        }
        other => {
            emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, other)?;
            b.pop();
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_statement_value(
    cx: &mut Ctx,
    hoist: &mut Vec<HoistScope>,
    cache: &mut LitCache,
    b: &mut BytecodeBuilder,
    scope_id: u32,
    next_hoist_slot: &mut usize,
    stmt: &Expr,
) -> Result<(), CompileError> {
    match stmt {
        Expr::Assign { name, value, span } => {
            emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, value)?;
            b.dup();
            emit_store(cx, hoist, b, scope_id, name, *span)?;
        }
        other => emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, other)?,
    }
    Ok(())
}

/// A compiled unit's fall-off-the-end behavior (Pitfalls P9). `Method`:
/// every statement (even the textually last one) is ordinary statement
/// position — falling off ALWAYS returns `self`, regardless of what the
/// last statement computed. `DoIt`: the same explicit-`^`/`ret_tos`
/// mechanics as `Method` (a doIt is a real entry-frame activation, not a
/// block), but falling off delivers the last statement's OWN value (a
/// workspace "print it" must see `3 + 4.`'s `7`, not `self`/nil) — SPEC
/// §3.2 step 3, `sprint_s05_detail.md` §Algorithms "Class-definition
/// execution" top-level `do_it`. `Block`: `nlr_tos`/`block_return_tos`,
/// last-statement-value fall-off (a body whose last item is a statement
/// uses that statement's value).
#[derive(Clone, Copy, PartialEq, Eq)]
enum BodyKind {
    Method,
    DoIt,
    Block,
}

#[allow(clippy::too_many_arguments)]
fn emit_body(
    cx: &mut Ctx,
    hoist: &mut Vec<HoistScope>,
    cache: &mut LitCache,
    b: &mut BytecodeBuilder,
    scope_id: u32,
    next_hoist_slot: &mut usize,
    body: &[Expr],
    kind: BodyKind,
) -> Result<(), CompileError> {
    if body.is_empty() {
        match kind {
            BodyKind::Block => {
                b.push_nil();
                b.block_return_tos();
            }
            BodyKind::Method | BodyKind::DoIt => {
                b.ret_self();
            }
        }
        return Ok(());
    }
    if kind == BodyKind::Method {
        // `ret_self` is always appended; if the body already ended in an
        // explicit `^`, that `ret_tos` already left balanced stack shape
        // and this trailing `ret_self` is simply unreachable dead code
        // (still satisfies `BytecodeBuilder::finish`'s "ends in a return").
        for stmt in body {
            emit_statement_discard(cx, hoist, cache, b, scope_id, next_hoist_slot, stmt, None)?;
        }
        b.ret_self();
        return Ok(());
    }
    let is_block = kind == BodyKind::Block;
    for stmt in &body[..body.len() - 1] {
        emit_statement_discard(
            cx,
            hoist,
            cache,
            b,
            scope_id,
            next_hoist_slot,
            stmt,
            is_block.then_some(()),
        )?;
    }
    let last = &body[body.len() - 1];
    match last {
        Expr::Return { value, .. } => {
            emit_expr(cx, hoist, cache, b, scope_id, next_hoist_slot, value)?;
            if is_block {
                b.nlr_tos();
            } else {
                b.ret_tos();
            }
        }
        _ => {
            emit_statement_value(cx, hoist, cache, b, scope_id, next_hoist_slot, last)?;
            if is_block {
                b.block_return_tos();
            } else {
                b.ret_tos();
            }
        }
    }
    Ok(())
}

fn emit_prologue(b: &mut BytecodeBuilder, info: &ScopeInfo) {
    if !info.has_ctx {
        return;
    }
    for (i, (_name, slot)) in info.vars.iter().enumerate().take(info.argc) {
        if slot.captured {
            b.push_temp(i as u8);
            b.store_ctx_temp_pop(
                0,
                slot.ctx_slot.expect("captured param has no ctx_slot") as u8,
            );
        }
    }
}

// --- blocks -------------------------------------------------------------------

fn emit_block_literal(
    cx: &mut Ctx,
    _outer_cache: &mut LitCache,
    outer_b: &mut BytecodeBuilder,
    blk: &BlockNode,
) -> Result<(), CompileError> {
    let scope_id = blk.scope_id;
    let info = cx.capture.scopes[scope_id as usize].clone();
    let argc = blk.params.len();
    check_limits(cx, blk.span, argc, info.frame_temp_count, info.nctx)?;

    let mut inner_b = BytecodeBuilder::new();
    let mut inner_cache = LitCache::new();
    let mut hoist: Vec<HoistScope> = Vec::new();
    let mut next_hoist_slot = info.frame_temp_count + argc;
    emit_prologue(&mut inner_b, &info);
    let saved_in_block = cx.in_block;
    cx.in_block = true;
    let body_result = emit_body(
        cx,
        &mut hoist,
        &mut inner_cache,
        &mut inner_b,
        scope_id,
        &mut next_hoist_slot,
        &blk.body,
        BodyKind::Block,
    );
    cx.in_block = saved_in_block;
    body_result?;
    let total_ntemps = next_hoist_slot - argc;
    let sel = cx.vm.universe.intern(b"aBlock");
    let built = inner_b.finish(cx.vm, sel, argc, total_ntemps);
    built.set_flags(
        argc,
        total_ntemps,
        info.has_ctx,
        true,
        false,
        info.captures_ctx,
        info.nctx,
    );
    let lit = outer_b.intern_block_literal(cx.vm, built);
    outer_b.push_closure(lit, 0);
    Ok(())
}

// --- top-level entry points --------------------------------------------------

/// Compiles a single method (SPEC §4.5, S5). `holder` is used for instance-
/// variable/class-variable resolution; class-side methods see the
/// metaclass's (empty, v1) chain instead.
pub fn compile_method(
    vm: &mut VmState,
    holder: KlassOop,
    class_side: bool,
    method: &mut MethodNode,
) -> Result<MethodOop, CompileError> {
    compile_method_inner(vm, holder, class_side, false, method)
}

/// Wraps `stmt` as an anonymous `#doIt` method (SPEC §3.2/§4.5's top-level
/// execution path) with `top_level` resolution rules (declare-on-assign for
/// an unresolvable, uppercase-leading global name).
pub fn compile_doit(
    vm: &mut VmState,
    holder: KlassOop,
    stmt: Expr,
) -> Result<MethodOop, CompileError> {
    let span = stmt.span();
    let mut method = MethodNode {
        pattern_selector: "doIt".to_string(),
        params: vec![],
        primitive: None,
        temps: vec![],
        body: vec![stmt],
        class_side: false,
        span,
    };
    compile_method_inner(vm, holder, false, true, &mut method)
}

fn compile_method_inner(
    vm: &mut VmState,
    holder: KlassOop,
    class_side: bool,
    top_level: bool,
    method: &mut MethodNode,
) -> Result<MethodOop, CompileError> {
    let capture = capture::analyze(method);
    let raw_inst_var_names = if class_side {
        Vec::new()
    } else {
        flatten_inst_vars(holder)
    };
    let scope0 = capture.scopes[0].clone();
    let argc = method.params.len();

    // `flatten_inst_vars` itself never allocates (pure klass-chain reads),
    // but every symbol it returns needs handle protection before codegen's
    // many subsequent allocations can move it (S7-9).
    let scope = HandleScope::enter(vm);
    let holder = scope.handle(vm, holder);
    let inst_var_names: Vec<Handle<Oop>> = raw_inst_var_names
        .into_iter()
        .map(|o| scope.handle(vm, o))
        .collect();

    let mut cx = Ctx {
        vm,
        holder,
        class_side,
        top_level,
        capture,
        inst_var_names,
        in_block: false,
        scope,
    };
    check_limits(&cx, method.span, argc, scope0.frame_temp_count, scope0.nctx)?;

    let mut b = BytecodeBuilder::new();
    let mut cache = LitCache::new();
    let mut hoist: Vec<HoistScope> = Vec::new();
    let mut next_hoist_slot = scope0.frame_temp_count + argc;
    emit_prologue(&mut b, &scope0);
    emit_body(
        &mut cx,
        &mut hoist,
        &mut cache,
        &mut b,
        0,
        &mut next_hoist_slot,
        &method.body,
        if top_level {
            BodyKind::DoIt
        } else {
            BodyKind::Method
        },
    )?;
    let total_ntemps = next_hoist_slot - argc;

    let sel = cx.vm.universe.intern(method.pattern_selector.as_bytes());
    let prim_fails = method.primitive.is_some() && !method.body.is_empty();
    let m = b.finish(cx.vm, sel, argc, total_ntemps);
    m.set_flags(
        argc,
        total_ntemps,
        scope0.has_ctx,
        false,
        prim_fails,
        false,
        scope0.nctx,
    );
    if let Some(pid) = method.primitive {
        m.set_primitive(pid as i64);
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::ast::TopItem;
    use crate::frontend::lexer::int_lit_magnitude;
    use crate::frontend::parser::parse_file;
    use crate::interpreter::run_method;
    use crate::runtime::vm_state::{OutputBuffer, VmOptions};

    fn test_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            gc_stress_full_period: None,
            eden_kb: None,
            jit: crate::runtime::JitMode::Off,
        })
    }

    fn compile_top(vm: &mut VmState, src: &str) -> MethodOop {
        let mut items = parse_file(&format!("{src}.")).expect("parse");
        let stmt = match items.remove(0) {
            TopItem::DoIt(e) => e,
            _ => panic!("expected a doIt"),
        };
        let holder = vm.universe.undefined_object_klass;
        compile_doit(vm, holder, stmt).expect("compile")
    }

    fn run_top(vm: &mut VmState, src: &str) -> Oop {
        let m = compile_top(vm, src);
        let nil = vm.universe.nil_obj;
        run_method(vm, m, nil, &[])
    }

    #[test]
    fn resolve_order() {
        // instvar shadows global; temp shadows instvar.
        let mut vm = test_vm();
        let object_klass = vm.universe.object_klass;
        let x_klass = vm.universe.new_klass(
            object_klass,
            "RTest",
            crate::oops::Format::Slots,
            false,
            crate::oops::layout::HEADER_WORDS + 1,
        );
        let x_sym = vm.universe.intern(b"x");
        let array_klass = vm.universe.array_klass;
        let ivn = alloc::alloc_indexable_oops(&mut vm, array_klass, 1);
        ivn.at_put(0, x_sym.oop());
        x_klass.set_inst_var_names(ivn.oop());

        let global_assoc = crate::runtime::globals::global_declare(&mut vm, x_sym);
        MemOop::try_from(global_assoc)
            .unwrap()
            .set_body_oop(1, SmallInt::new(1).oop());

        let sp = Span { line: 0, col: 0 };
        let mut m1 = MethodNode {
            pattern_selector: "m".into(),
            params: vec![],
            primitive: None,
            temps: vec![],
            body: vec![Expr::Return {
                value: Box::new(Expr::Var {
                    name: "x".into(),
                    span: sp,
                }),
                span: sp,
            }],
            class_side: false,
            span: sp,
        };
        let compiled = compile_method(&mut vm, x_klass, false, &mut m1).unwrap();
        let recv = alloc::alloc_slots(&mut vm, x_klass).oop();
        MemOop::try_from(recv)
            .unwrap()
            .set_body_oop(0, SmallInt::new(2).oop());
        let result = run_method(&mut vm, compiled, recv, &[]);
        assert_eq!(
            result,
            SmallInt::new(2).oop(),
            "instvar must shadow the global"
        );

        let mut m2 = MethodNode {
            pattern_selector: "m2".into(),
            params: vec![],
            primitive: None,
            temps: vec!["x".to_string()],
            body: vec![
                Expr::Assign {
                    name: "x".into(),
                    value: Box::new(Expr::Lit {
                        value: Literal::Int(3),
                        span: sp,
                    }),
                    span: sp,
                },
                Expr::Return {
                    value: Box::new(Expr::Var {
                        name: "x".into(),
                        span: sp,
                    }),
                    span: sp,
                },
            ],
            class_side: false,
            span: sp,
        };
        let compiled2 = compile_method(&mut vm, x_klass, false, &mut m2).unwrap();
        let result2 = run_method(&mut vm, compiled2, recv, &[]);
        assert_eq!(
            result2,
            SmallInt::new(3).oop(),
            "temp must shadow the instvar"
        );
    }

    #[test]
    fn resolve_unbound() {
        let mut vm = test_vm();
        let mut m = MethodNode {
            pattern_selector: "m".into(),
            params: vec![],
            primitive: None,
            temps: vec![],
            body: vec![Expr::Var {
                name: "Zork".into(),
                span: Span { line: 3, col: 5 },
            }],
            class_side: false,
            span: Span { line: 0, col: 0 },
        };
        let object_klass = vm.universe.object_klass;
        let err = compile_method(&mut vm, object_klass, false, &mut m).unwrap_err();
        assert_eq!(err.span, Span { line: 3, col: 5 });
        assert!(err.msg.contains("Zork"));
    }

    #[test]
    fn resolve_doit_global() {
        let mut vm = test_vm();
        let r = run_top(&mut vm, "Foo := 3");
        assert_eq!(r, SmallInt::new(3).oop());
        let r2 = run_top(&mut vm, "Foo");
        assert_eq!(r2, SmallInt::new(3).oop());

        let mut items = parse_file("foo := 3.").unwrap();
        let stmt = match items.remove(0) {
            TopItem::DoIt(e) => e,
            _ => unreachable!(),
        };
        let holder = vm.universe.undefined_object_klass;
        assert!(compile_doit(&mut vm, holder, stmt).is_err());
    }

    #[test]
    fn litframe_dedupe() {
        let mut vm = test_vm();
        let sp = Span { line: 0, col: 0 };
        let mut m = MethodNode {
            pattern_selector: "m".into(),
            params: vec![],
            primitive: None,
            temps: vec![],
            body: vec![
                Expr::Lit {
                    value: Literal::Symbol("foo".into()),
                    span: sp,
                },
                Expr::Lit {
                    value: Literal::Symbol("foo".into()),
                    span: sp,
                },
                Expr::Lit {
                    value: Literal::Str("hi".into()),
                    span: sp,
                },
                Expr::Lit {
                    value: Literal::Str("hi".into()),
                    span: sp,
                },
                Expr::Lit {
                    value: Literal::Int(1000),
                    span: sp,
                },
                Expr::Lit {
                    value: Literal::Int(1000),
                    span: sp,
                },
            ],
            class_side: false,
            span: sp,
        };
        let object_klass = vm.universe.object_klass;
        let compiled = compile_method(&mut vm, object_klass, false, &mut m).unwrap();
        assert_eq!(compiled.literals().len(), 3);
    }

    #[test]
    fn ic_site_indices() {
        let mut vm = test_vm();
        let sp = Span { line: 0, col: 0 };
        let recv = || Box::new(Expr::SelfRef(sp));
        let mut m = MethodNode {
            pattern_selector: "m".into(),
            params: vec![],
            primitive: None,
            temps: vec![],
            body: vec![
                Expr::Send {
                    receiver: recv(),
                    selector: "foo".into(),
                    args: vec![],
                    is_super: false,
                    span: sp,
                },
                Expr::Send {
                    receiver: recv(),
                    selector: "bar".into(),
                    args: vec![],
                    is_super: false,
                    span: sp,
                },
                Expr::Send {
                    receiver: recv(),
                    selector: "baz".into(),
                    args: vec![],
                    is_super: false,
                    span: sp,
                },
            ],
            class_side: false,
            span: sp,
        };
        let object_klass = vm.universe.object_klass;
        let compiled = compile_method(&mut vm, object_klass, false, &mut m).unwrap();
        assert_eq!(compiled.ics().len(), 12);
        let mut bci = 0usize;
        let mut ops = Vec::new();
        while bci < compiled.bytecode_len() {
            let (instr, next) = crate::bytecode::opcode::decode_at(compiled, bci);
            if let crate::bytecode::Instr::Send { ic, .. } = instr {
                ops.push(ic);
            }
            bci = next;
        }
        assert_eq!(ops, vec![0, 1, 2]);
    }

    #[test]
    fn flags_word() {
        let mut vm = test_vm();
        let object_klass = vm.universe.object_klass;
        let sp = Span { line: 0, col: 0 };

        let mut m1 = MethodNode {
            pattern_selector: "a:b:".into(),
            params: vec!["a".into(), "b".into()],
            primitive: None,
            temps: vec!["t".into()],
            body: vec![],
            class_side: false,
            span: sp,
        };
        let c1 = compile_method(&mut vm, object_klass, false, &mut m1).unwrap();
        assert_eq!(c1.argc(), 2);
        assert_eq!(c1.ntemps(), 1);
        assert!(!c1.has_ctx());
        assert!(!c1.is_block());
        assert!(!c1.prim_fails());

        let mut m2 = MethodNode {
            pattern_selector: "m2".into(),
            params: vec![],
            primitive: None,
            temps: vec!["t".into()],
            body: vec![Expr::Block(Box::new(BlockNode {
                params: vec![],
                temps: vec![],
                body: vec![Expr::Var {
                    name: "t".into(),
                    span: sp,
                }],
                span: sp,
                scope_id: 0,
            }))],
            class_side: false,
            span: sp,
        };
        let c2 = compile_method(&mut vm, object_klass, false, &mut m2).unwrap();
        assert!(c2.has_ctx());
        assert_eq!(c2.nctx(), 1);

        let mut m3 = MethodNode {
            pattern_selector: "m3".into(),
            params: vec![],
            primitive: Some(7),
            temps: vec![],
            body: vec![Expr::SelfRef(sp)],
            class_side: false,
            span: sp,
        };
        let c3 = compile_method(&mut vm, object_klass, false, &mut m3).unwrap();
        assert!(c3.prim_fails());
        assert_eq!(c3.primitive(), 7);

        let mut m4 = MethodNode {
            pattern_selector: "m4".into(),
            params: vec![],
            primitive: Some(7),
            temps: vec![],
            body: vec![],
            class_side: false,
            span: sp,
        };
        let c4 = compile_method(&mut vm, object_klass, false, &mut m4).unwrap();
        assert!(!c4.prim_fails());

        let mut m5 = MethodNode {
            pattern_selector: "m5".into(),
            params: vec![],
            primitive: None,
            temps: vec![],
            body: vec![Expr::Block(Box::new(BlockNode {
                params: vec![],
                temps: vec![],
                body: vec![],
                span: sp,
                scope_id: 0,
            }))],
            class_side: false,
            span: sp,
        };
        let c5 = compile_method(&mut vm, object_klass, false, &mut m5).unwrap();
        assert_eq!(c5.literals().len(), 1);
        let blk = MethodOop::try_from(c5.literals().at(0)).unwrap();
        assert!(blk.is_block());
    }

    #[test]
    fn bigint_literal() {
        let mut vm = test_vm();
        // 18 hex F's = 72 magnitude bits, past the 61-bit smi range.
        let mag = int_lit_magnitude(16, "FFFFFFFFFFFFFFFFFF");
        let mut m = MethodNode {
            pattern_selector: "m".into(),
            params: vec![],
            primitive: None,
            temps: vec![],
            body: vec![Expr::Return {
                value: Box::new(Expr::Lit {
                    value: Literal::BigInt {
                        negative: false,
                        digits: mag,
                    },
                    span: Span { line: 0, col: 0 },
                }),
                span: Span { line: 0, col: 0 },
            }],
            class_side: false,
            span: Span { line: 0, col: 0 },
        };
        let object_klass = vm.universe.object_klass;
        let compiled = compile_method(&mut vm, object_klass, false, &mut m).unwrap();
        let recv = vm.universe.nil_obj;
        let result = run_method(&mut vm, compiled, recv, &[]);
        let big = MemOop::try_from(result).expect("BigInt is a mem oop");
        assert_eq!(big.klass(), vm.universe.large_pos_int_klass);
    }

    fn install_print(vm: &mut VmState) {
        let object_klass = vm.universe.object_klass;
        let print_sel = vm.universe.intern(b"print:");
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.ret_self();
        let m = b.finish(vm, print_sel, 1, 0);
        m.set_primitive(91);
        crate::runtime::lookup::install_method(vm, object_klass, print_sel, m);
    }

    /// A top-level doIt has no `| |` temporaries section of its own (SPEC
    /// §4.5's top-level execution path) — these tests need locals, so they
    /// wrap the body in an immediately-`value`d block, which needs
    /// `BlockClosure>>value` (primitive 50) installed.
    fn install_value(vm: &mut VmState) {
        let closure_klass = vm.universe.closure_klass;
        let value_sel = vm.universe.intern(b"value");
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.ret_self();
        let m = b.finish(vm, value_sel, 0, 0);
        m.set_primitive(50);
        crate::runtime::lookup::install_method(vm, closure_klass, value_sel, m);
    }

    /// No image/library is loaded in these unit tests (that's S6+), so
    /// inlined `to:do:`/`whileTrue:` goldens that emit real `<=`/`+` sends
    /// need those primitives installed by hand on `smi_klass`.
    fn install_smi_arith(vm: &mut VmState) {
        let smi_klass = vm.universe.smi_klass;
        let mk = |vm: &mut VmState, name: &[u8], prim: i64| {
            let sel = vm.universe.intern(name);
            let mut b = BytecodeBuilder::new();
            b.push_self();
            b.ret_self();
            let m = b.finish(vm, sel, 1, 0);
            m.set_primitive(prim);
            (sel, m)
        };
        let (sel, m) = mk(vm, b"+", 1);
        crate::runtime::lookup::install_method(vm, smi_klass, sel, m);
        let (sel, m) = mk(vm, b"<=", 11);
        crate::runtime::lookup::install_method(vm, smi_klass, sel, m);
        let (sel, m) = mk(vm, b">", 12);
        crate::runtime::lookup::install_method(vm, smi_klass, sel, m);
    }

    #[test]
    fn inlined_iftrue_golden() {
        let mut vm = test_vm();
        install_print(&mut vm);
        let r = run_top(&mut vm, "true ifTrue: [3] ifFalse: [4]");
        assert_eq!(r, SmallInt::new(3).oop());
        let r2 = run_top(&mut vm, "false ifTrue: [3] ifFalse: [4]");
        assert_eq!(r2, SmallInt::new(4).oop());
    }

    #[test]
    fn inlined_whiletrue_and_todo() {
        let mut vm = test_vm();
        install_value(&mut vm);
        install_smi_arith(&mut vm);
        let r = run_top(
            &mut vm,
            "[| i sum | i := 1. sum := 0. [i <= 5] whileTrue: [sum := sum + i. i := i + 1]. sum] value",
        );
        assert_eq!(r, SmallInt::new(15).oop());

        let r2 = run_top(
            &mut vm,
            "[| sum | sum := 0. 1 to: 5 do: [:i | sum := sum + i]. sum] value",
        );
        assert_eq!(r2, SmallInt::new(15).oop());
    }

    #[test]
    fn opcode_coverage_smoke() {
        let mut vm = test_vm();
        install_value(&mut vm);
        install_smi_arith(&mut vm);
        let r = run_top(
            &mut vm,
            "[| a b | a := 1. b := a + 2. (b > 2) ifTrue: [b] ifFalse: [a]] value",
        );
        assert_eq!(r, SmallInt::new(3).oop());
        let _ = OutputBuffer::new(); // sanity: type still in scope
    }
}
