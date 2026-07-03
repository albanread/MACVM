//! `BytecodeBuilder` — the bytecode assembler (SPEC §4). Test-only in
//! spirit for S2 (all bytecode comes from hand-built kernels), but S5's
//! source compiler reuses it directly, so it lives in the lib rather than
//! behind `#[cfg(test)]`.
//!
//! **Jump base convention (pinned, SPEC §4.2 gives widths not the base):**
//! all jump distances are measured from `next_bci` (the bci immediately
//! after the whole 3-byte instruction). `jump_fwd`/`br_*_fwd`:
//! `target = next_bci + distance` (distance 0 = fall straight through).
//! `jump_back`: `target = next_bci - distance`.
//!
//! **S7-9/S7-10 handle discipline.** `literals`/`ic_sites` accumulate oop
//! values across an entire build session (every `push_literal`/`send`/
//! `send_super`/`add_send` call), which can span many intervening
//! allocations (nested block compiles, other literals, sends) — exactly
//! the invisible-root shape S7-9's `Handle` exists to close: a bare
//! `Vec<Oop>`/`Vec<(SymbolOop, u8)>` accumulated this way is untouched by
//! a scavenge and goes stale the moment anything referenced moves.
//! `literals`/`ic_sites` are therefore `Handle`-backed, and every method
//! that can add to them takes `vm: &mut VmState` — confirmed necessary (not
//! just theoretical) by S7-10's `MACVM_GC_STRESS=1` run, which failed on
//! even a trivial `Transcript show:` before this fix. `BytecodeBuilder`
//! itself still needs no `vm` at construction (`new()`'s signature is
//! unchanged — its own `HandleScope` is entered lazily, on the first call
//! that needs to protect a value), so the ~50 test-only `BytecodeBuilder::
//! new()` call sites that never push a literal or a send are untouched.

use std::collections::HashMap;

use crate::memory::handles::{Handle, HandleScope};
use crate::oops::layout::{
    BCI_SENTINEL_BASE, IC_GUARD_OFFSET, IC_META_ARGC_SHIFT, IC_META_OFFSET, IC_SEL_OFFSET,
    IC_STRIDE, IC_TARGET_OFFSET, METHOD_ARGC_MAX, METHOD_NTEMPS_MAX,
};
use crate::oops::wrappers::{ArrayOop, MethodOop, SymbolOop};
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;

use super::opcode::*;
use super::Instr;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Label(usize);

enum LabelState {
    Bound(usize),
    Unbound(Vec<usize>), // byte offsets of the u16 operand to patch
}

pub struct BytecodeBuilder {
    code: Vec<u8>,
    literals: Vec<Handle<Oop>>,
    /// Dedup cache: `Oop::raw()` at the MOMENT of insertion. Never updated
    /// when the underlying value later moves — a stale entry just misses a
    /// dedup opportunity (an extra literal-frame slot), never a
    /// correctness issue, since the actual storage (`literals`) is
    /// handle-backed regardless.
    literal_index: HashMap<u64, usize>,
    n_ics: usize,
    ic_sites: Vec<(Handle<SymbolOop>, u8)>,
    labels: Vec<LabelState>,
    /// Lazily entered on the first `push_literal`/`add_send` call — kept
    /// alive for this builder's whole session so every `Handle` created
    /// through it stays valid until `finish()` (or `Drop`, for a session
    /// abandoned mid-build, e.g. a compile error) consumes/releases it.
    scope: Option<HandleScope>,
}

impl Default for BytecodeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl BytecodeBuilder {
    pub fn new() -> Self {
        BytecodeBuilder {
            code: Vec::new(),
            literals: Vec::new(),
            literal_index: HashMap::new(),
            n_ics: 0,
            ic_sites: Vec::new(),
            labels: Vec::new(),
            scope: None,
        }
    }

    #[inline]
    fn emit_u8(&mut self, b: u8) {
        self.code.push(b);
    }

    #[inline]
    fn emit_u16(&mut self, v: u16) {
        self.code.push((v & 0xFF) as u8);
        self.code.push((v >> 8) as u8);
    }

    // --- straight-line opcodes ------------------------------------------

    pub fn push_self(&mut self) -> &mut Self {
        self.emit_u8(OP_PUSH_SELF);
        self
    }
    pub fn push_nil(&mut self) -> &mut Self {
        self.emit_u8(OP_PUSH_NIL);
        self
    }
    pub fn push_true(&mut self) -> &mut Self {
        self.emit_u8(OP_PUSH_TRUE);
        self
    }
    pub fn push_false(&mut self) -> &mut Self {
        self.emit_u8(OP_PUSH_FALSE);
        self
    }

    pub fn push_smi_i8(&mut self, v: i8) -> &mut Self {
        self.emit_u8(OP_PUSH_SMI_I8);
        self.emit_u8(v as u8);
        self
    }

    /// Interns `o` into the literal frame (deduplicating by raw oop
    /// identity) and emits `push_literal`, auto-widening to the `u16` form
    /// (0x12) when the literal's index exceeds `u8::MAX`.
    pub fn push_literal(&mut self, vm: &mut VmState, o: Oop) -> &mut Self {
        let idx = self.intern_literal(vm, o);
        self.emit_literal_ref(idx);
        self
    }

    fn intern_literal(&mut self, vm: &mut VmState, o: Oop) -> usize {
        if let Some(&idx) = self.literal_index.get(&o.raw()) {
            // VALIDATE the hit against the handle-backed truth: the map is
            // keyed by raw addresses as of insert time, and a moving GC can
            // recycle a dead literal's old address for a DIFFERENT object —
            // an unvalidated hit then aliases two distinct literals into
            // one slot (S7-10, found via `MACVM_GC_STRESS=1`: a method's
            // `$)` char literal resolved to its `$(`, printing "(4, 6(").
            // A stale entry that merely MISSES costs one duplicate slot;
            // a false HIT is a miscompile.
            if self.literals[idx].get(vm).raw() == o.raw() {
                return idx;
            }
        }
        let h = self.make_handle(vm, o);
        let idx = self.literals.len();
        self.literals.push(h);
        self.literal_index.insert(o.raw(), idx);
        idx
    }

    /// Wraps `o` in a `Handle` backed by this builder's own (lazily
    /// entered) session scope. Exposed so `frontend::codegen`'s `LitCache`
    /// — a purely local dedup helper that must never outlive the builder
    /// it feeds — shares THIS scope instead of owning an independent one.
    /// Two independently, lazily-entered scopes have no fixed relative
    /// entry order (whichever construct happens to push first "wins" the
    /// smaller `saved_len`), so whichever one drops first can truncate the
    /// other's still-live entries out from under it; sharing one scope
    /// (this builder's, always the one consumed last, via `finish`)
    /// removes the ordering hazard entirely rather than requiring every
    /// caller to get manual drop ordering right (S7-10, found via
    /// `MACVM_GC_STRESS=1` corrupting a compiled block's literal frame).
    pub(crate) fn make_handle(&mut self, vm: &mut VmState, o: Oop) -> Handle<Oop> {
        if self.scope.is_none() {
            self.scope = Some(HandleScope::enter(vm));
        }
        self.scope.as_ref().unwrap().handle(vm, o)
    }

    fn emit_literal_ref(&mut self, idx: usize) {
        if idx <= u8::MAX as usize {
            self.emit_u8(OP_PUSH_LITERAL);
            self.emit_u8(idx as u8);
        } else {
            self.emit_u8(OP_PUSH_LITERAL_W);
            self.emit_u16(idx as u16);
        }
    }

    pub fn push_temp(&mut self, t: u8) -> &mut Self {
        self.emit_u8(OP_PUSH_TEMP);
        self.emit_u8(t);
        self
    }
    pub fn store_temp(&mut self, t: u8) -> &mut Self {
        self.emit_u8(OP_STORE_TEMP);
        self.emit_u8(t);
        self
    }
    pub fn store_temp_pop(&mut self, t: u8) -> &mut Self {
        self.emit_u8(OP_STORE_TEMP_POP);
        self.emit_u8(t);
        self
    }

    pub fn push_instvar(&mut self, i: u8) -> &mut Self {
        self.emit_u8(OP_PUSH_INSTVAR);
        self.emit_u8(i);
        self
    }
    pub fn store_instvar_pop(&mut self, i: u8) -> &mut Self {
        self.emit_u8(OP_STORE_INSTVAR_POP);
        self.emit_u8(i);
        self
    }

    /// `depth` counts `home_hint` hops (SPEC §5.4) — only `has_ctx` scopes,
    /// never lexical block nesting.
    pub fn push_ctx_temp(&mut self, depth: u8, idx: u8) -> &mut Self {
        self.emit_u8(OP_PUSH_CTX_TEMP);
        self.emit_u8(depth);
        self.emit_u8(idx);
        self
    }
    pub fn store_ctx_temp_pop(&mut self, depth: u8, idx: u8) -> &mut Self {
        self.emit_u8(OP_STORE_CTX_TEMP_POP);
        self.emit_u8(depth);
        self.emit_u8(idx);
        self
    }

    /// `assoc` is an Association literal (SPEC §4.1: index 0 = key, index 1
    /// = value, pinned in `sprint_s01`'s genesis); `push_global` pushes its
    /// value, `store_global_pop` writes it.
    pub fn push_global(&mut self, vm: &mut VmState, assoc: Oop) -> &mut Self {
        let idx = self.intern_literal(vm, assoc);
        self.emit_u8(OP_PUSH_GLOBAL);
        self.emit_u8(idx as u8);
        self
    }
    pub fn store_global_pop(&mut self, vm: &mut VmState, assoc: Oop) -> &mut Self {
        let idx = self.intern_literal(vm, assoc);
        self.emit_u8(OP_STORE_GLOBAL_POP);
        self.emit_u8(idx as u8);
        self
    }

    pub fn pop(&mut self) -> &mut Self {
        self.emit_u8(OP_POP);
        self
    }
    pub fn dup(&mut self) -> &mut Self {
        self.emit_u8(OP_DUP);
        self
    }

    pub fn ret_tos(&mut self) -> &mut Self {
        self.emit_u8(OP_RETURN_TOS);
        self
    }
    pub fn ret_self(&mut self) -> &mut Self {
        self.emit_u8(OP_RETURN_SELF);
        self
    }

    /// A block's implicit fall-off-the-end return (SPEC §5.4, S4) — pops
    /// TOS and delivers it to whoever `value`d the block; NOT a non-local
    /// return (that's [`Self::nlr_tos`]).
    pub fn block_return_tos(&mut self) -> &mut Self {
        self.emit_u8(OP_BLOCK_RETURN_TOS);
        self
    }

    /// `^expr` inside a block (SPEC §5.4, S4): pops TOS and non-locally
    /// returns it to the block's *home* method activation.
    pub fn nlr_tos(&mut self) -> &mut Self {
        self.emit_u8(OP_NLR_TOS);
        self
    }

    // --- sends (SPEC §5.3, S3) ---------------------------------------------

    /// Appends a new 4-word IC site for `(selector, argc)` and returns its
    /// index — the low-level primitive `send`/`send_super` build on.
    pub fn add_send(&mut self, vm: &mut VmState, selector: SymbolOop, argc: u8) -> u16 {
        let idx = self.n_ics;
        assert!(idx <= u16::MAX as usize, "add_send: too many send sites");
        self.n_ics += 1;
        if self.scope.is_none() {
            self.scope = Some(HandleScope::enter(vm));
        }
        let h = self.scope.as_ref().unwrap().handle(vm, selector);
        self.ic_sites.push((h, argc));
        idx as u16
    }

    fn emit_send(
        &mut self,
        vm: &mut VmState,
        op_narrow: u8,
        op_wide: u8,
        selector: SymbolOop,
        argc: u8,
    ) -> &mut Self {
        let ic_idx = self.add_send(vm, selector, argc);
        if ic_idx <= u8::MAX as u16 {
            self.emit_u8(op_narrow);
            self.emit_u8(ic_idx as u8);
        } else {
            self.emit_u8(op_wide);
            self.emit_u16(ic_idx);
        }
        self
    }

    pub fn send(&mut self, vm: &mut VmState, selector: SymbolOop, argc: u8) -> &mut Self {
        self.emit_send(vm, OP_SEND, OP_SEND_W, selector, argc)
    }

    pub fn send_super(&mut self, vm: &mut VmState, selector: SymbolOop, argc: u8) -> &mut Self {
        self.emit_send(vm, OP_SEND_SUPER, OP_SEND_SUPER_W, selector, argc)
    }

    // --- closures (SPEC §2.3, §4.2, §5.4, S4) -------------------------------

    /// Builds a `CompiledBlock` (a `MethodOop` with `is_block=true`) via a
    /// fresh nested builder session (`body` emits the block's own
    /// bytecode), and interns it as a literal in `self` — the *enclosing*
    /// (home) builder — returning its literal index for [`Self::push_closure`].
    /// `holder` is patched later, transitively, by `install_method`
    /// (`runtime::lookup`) once the enclosing method itself is installed —
    /// never set here, since the enclosing method has no holder yet.
    #[allow(clippy::too_many_arguments)]
    pub fn build_block(
        &mut self,
        vm: &mut VmState,
        argc: usize,
        ntemps: usize,
        has_ctx: bool,
        nctx: usize,
        captures_ctx: bool,
        body: impl FnOnce(&mut BytecodeBuilder, &mut VmState),
    ) -> usize {
        let blk = build_standalone_block(vm, argc, ntemps, has_ctx, nctx, captures_ctx, body);
        self.intern_block_literal(vm, blk)
    }

    /// Interns an already-built `CompiledBlock` (e.g. from
    /// [`build_standalone_block`]) as a literal in `self`, returning its
    /// index for [`Self::push_closure`]. The escape hatch nested block
    /// building needs: [`Self::build_block`] cannot itself be called
    /// re-entrantly (its `body` closure would need a second concurrent
    /// `&mut VmState`) — build inner blocks standalone first, then intern
    /// them into the outer block's own builder from *within* its `body`.
    pub fn intern_block_literal(&mut self, vm: &mut VmState, blk: MethodOop) -> usize {
        self.intern_literal(vm, blk.oop())
    }

    /// `lit` must fit the opcode's `u8` literal-index operand (SPEC §4.2
    /// pins `push_closure` narrow-only, no wide form) — methods with more
    /// than 256 literals total and a block among the high ones are out of
    /// scope for v1's hand-built/S5-compiled programs.
    pub fn push_closure(&mut self, lit: usize, n_value_captures: u8) -> &mut Self {
        debug_assert!(
            lit <= u8::MAX as usize,
            "push_closure: literal index {lit} exceeds the u8 operand"
        );
        self.emit_u8(OP_PUSH_CLOSURE);
        self.emit_u8(lit as u8);
        self.emit_u8(n_value_captures);
        self
    }

    // --- control flow -----------------------------------------------------

    pub fn new_label(&mut self) -> Label {
        let idx = self.labels.len();
        self.labels.push(LabelState::Unbound(Vec::new()));
        Label(idx)
    }

    /// Binds `l` to the current position (`here`). Patches every forward
    /// reference recorded against it.
    pub fn bind(&mut self, l: Label) {
        let bci = self.code.len();
        let prev = std::mem::replace(&mut self.labels[l.0], LabelState::Bound(bci));
        match prev {
            LabelState::Bound(_) => panic!("BytecodeBuilder::bind: label {} already bound", l.0),
            LabelState::Unbound(sites) => {
                for site in sites {
                    let next_bci = site + 2;
                    assert!(
                        bci >= next_bci,
                        "bind: forward-patch site resolves backward — use jump_back"
                    );
                    let distance = bci - next_bci;
                    assert!(distance <= u16::MAX as usize, "jump distance overflows u16");
                    self.code[site] = (distance & 0xFF) as u8;
                    self.code[site + 1] = (distance >> 8) as u8;
                }
            }
        }
    }

    fn emit_fwd_jump(&mut self, op: u8, l: Label) {
        self.emit_u8(op);
        let operand_pos = self.code.len();
        self.emit_u16(0xFFFF); // placeholder, patched by `bind` or below
        match &mut self.labels[l.0] {
            LabelState::Unbound(sites) => sites.push(operand_pos),
            LabelState::Bound(target) => {
                let target = *target;
                let next_bci = operand_pos + 2;
                assert!(
                    target >= next_bci,
                    "emit_fwd_jump: label already bound behind this site — use jump_back"
                );
                let distance = target - next_bci;
                assert!(distance <= u16::MAX as usize, "jump distance overflows u16");
                self.code[operand_pos] = (distance & 0xFF) as u8;
                self.code[operand_pos + 1] = (distance >> 8) as u8;
            }
        }
    }

    pub fn jump_fwd(&mut self, l: Label) -> &mut Self {
        self.emit_fwd_jump(OP_JUMP_FWD, l);
        self
    }
    pub fn br_true_fwd(&mut self, l: Label) -> &mut Self {
        self.emit_fwd_jump(OP_BR_TRUE_FWD, l);
        self
    }
    pub fn br_false_fwd(&mut self, l: Label) -> &mut Self {
        self.emit_fwd_jump(OP_BR_FALSE_FWD, l);
        self
    }

    /// `l` must already be bound (a backward reference into code already
    /// emitted).
    pub fn jump_back(&mut self, l: Label) -> &mut Self {
        let target = match self.labels[l.0] {
            LabelState::Bound(bci) => bci,
            LabelState::Unbound(_) => panic!("jump_back: label {} not yet bound", l.0),
        };
        let opcode_pos = self.code.len();
        let next_bci = opcode_pos + 3;
        assert!(
            target <= next_bci,
            "jump_back: target is ahead of this site"
        );
        let distance = next_bci - target;
        assert!(distance <= u16::MAX as usize, "jump distance overflows u16");
        self.emit_u8(OP_JUMP_BACK);
        self.emit_u16(distance as u16);
        self
    }

    // --- finishing ----------------------------------------------------------

    /// Builds the heap objects: literal Array, empty-but-sized IC Array (S2
    /// methods have `n_ics == 0`; S3's `send`/`send_super` emitters bump
    /// it), and the bytecode bytes. Panics (builder misuse = VM bug) if any
    /// label is unbound, the code does not end in a return, `argc > 15`, or
    /// `ntemps > 255`.
    pub fn finish(
        self,
        vm: &mut VmState,
        selector: SymbolOop,
        argc: usize,
        ntemps: usize,
    ) -> MethodOop {
        assert!(
            argc <= METHOD_ARGC_MAX,
            "argc {argc} exceeds the 4-bit field"
        );
        assert!(
            ntemps <= METHOD_NTEMPS_MAX,
            "ntemps {ntemps} exceeds the 8-bit field"
        );
        debug_assert_bci_sentinel_headroom(self.code.len());
        for (i, l) in self.labels.iter().enumerate() {
            if let LabelState::Unbound(_) = l {
                panic!("BytecodeBuilder::finish: label {i} was never bound");
            }
        }
        assert!(
            !self.code.is_empty(),
            "BytecodeBuilder::finish: empty method body"
        );

        // `method` is fresh, unrooted eden garbage the moment it's
        // allocated, and `selector` (the parameter, reachable via the
        // symbol table root but a separate unrooted copy here) is held
        // across two further allocating calls below — both need
        // protecting (S7-9). `self.literals`/`self.ic_sites` are already
        // handle-backed by `intern_literal`/`add_send`.
        let scope = crate::memory::handles::HandleScope::enter(vm);
        let selector_h = scope.handle(vm, selector.oop());

        let method = crate::memory::alloc::alloc_method(vm, self.code.len());
        let method_h = scope.handle(vm, method.oop());
        for (i, &b) in self.code.iter().enumerate() {
            method.set_bytecode_byte(i, b);
        }

        // Verify the stream ends in a return by decode-walking it — the
        // only reliable way; the raw last BYTE could be an operand of an
        // earlier instruction that coincidentally equals a return opcode.
        let mut bci = 0usize;
        let mut last_instr = None;
        while bci < method.bytecode_len() {
            let (instr, next) = super::opcode::decode_at(method, bci);
            last_instr = Some(instr);
            bci = next;
        }
        match last_instr {
            Some(Instr::ReturnTos)
            | Some(Instr::ReturnSelf)
            | Some(Instr::BlockReturnTos)
            | Some(Instr::NlrTos) => {}
            _ => panic!("BytecodeBuilder::finish: method does not end in a return"),
        }

        let array_klass = vm.universe.array_klass;
        let literals =
            crate::memory::alloc::alloc_indexable_oops(vm, array_klass, self.literals.len());
        let literals_h = scope.handle(vm, literals.oop());
        for (i, &lit) in self.literals.iter().enumerate() {
            let v = lit.get(vm);
            literals.at_put(i, v);
        }

        // Re-read fresh: `array_klass` above was captured before the
        // `literals` allocation, which — under `MACVM_GC_STRESS=1` — can
        // scavenge and move it. A stale `KlassOop` local re-used here
        // isn't caught by the usual "already forwarded" check the way a
        // single re-use would be: `alloc_words`'s own internal handle
        // protects whatever value it's GIVEN, but a value that's already
        // TWO scavenges stale resolves its one-hop forwarding to an
        // address that has itself since moved again (S7-10, found via
        // `MACVM_GC_STRESS=1` corrupting a compiled method's `ics` array).
        let array_klass = vm.universe.array_klass;
        let ics: ArrayOop =
            crate::memory::alloc::alloc_indexable_oops(vm, array_klass, self.n_ics * IC_STRIDE);
        let nil = vm.universe.nil_obj;
        for (i, &(selector, argc)) in self.ic_sites.iter().enumerate() {
            let base = i * IC_STRIDE;
            // Epoch 0 matches a fresh `VmState::ic_epoch` — harmless for an
            // empty-guard IC, since rows 1/2 never check the epoch.
            let meta = (argc as i64) << IC_META_ARGC_SHIFT;
            ics.at_put(base + IC_SEL_OFFSET, selector.get(vm).oop());
            ics.at_put(
                base + IC_META_OFFSET,
                crate::oops::smi::SmallInt::new(meta).oop(),
            );
            ics.at_put(base + IC_GUARD_OFFSET, nil);
            ics.at_put(base + IC_TARGET_OFFSET, nil);
        }

        // Re-derived fresh: `ics`'s own allocation just above may have
        // moved `method`/`literals`/`selector`.
        let method = MethodOop::try_from(method_h.get(vm)).expect("finish: method stays a Method");
        let literals =
            ArrayOop::try_from(literals_h.get(vm)).expect("finish: literals stays an array");
        let selector = selector_h.get(vm);

        method.set_selector(selector);
        method.set_holder(nil); // nil until S3 install
        method.set_flags(argc, ntemps, false, false, false, false, 0);
        method.set_primitive(0);
        method.set_counters(0);
        method.set_literals(literals);
        method.set_ics(ics);

        method
    }
}

/// Builds a `CompiledBlock` (a `MethodOop` with `is_block=true`) via a
/// fresh nested builder session, WITHOUT interning it into any enclosing
/// builder's literal pool — the caller does that separately (`build_block`
/// does it immediately; nested block building does it from within an
/// outer block's own `body` closure, see [`BytecodeBuilder::intern_block_literal`]).
#[allow(clippy::too_many_arguments)]
pub fn build_standalone_block(
    vm: &mut VmState,
    argc: usize,
    ntemps: usize,
    has_ctx: bool,
    nctx: usize,
    captures_ctx: bool,
    body: impl FnOnce(&mut BytecodeBuilder, &mut VmState),
) -> MethodOop {
    let mut inner = BytecodeBuilder::new();
    body(&mut inner, vm);
    let sel = vm.universe.intern(b"aBlock"); // CompiledBlocks are never looked up by selector; a fixed placeholder.
    let blk = inner.finish(vm, sel, argc, ntemps);
    blk.set_flags(argc, ntemps, has_ctx, true, false, captures_ctx, nctx);
    blk
}

/// A method's bytecode must never reach the BCI sentinel range
/// (`oops::layout::BCI_SENTINEL_BASE`, S4) — those values are reserved to
/// mark a suspended ensure/unwind resume point in a frame's `saved_bci`
/// slot. Split out from `finish()` so the guard is testable without
/// actually allocating a multi-GB bytecode stream.
fn debug_assert_bci_sentinel_headroom(bytecode_len: usize) {
    debug_assert!(
        bytecode_len < BCI_SENTINEL_BASE,
        "BytecodeBuilder::finish: bytecode_len {bytecode_len} reaches the BCI sentinel range"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oops::smi::SmallInt;
    use crate::runtime::vm_state::VmOptions;

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

    #[test]
    fn builder_dedup_literals() {
        let mut vm = test_vm();
        let sym = vm.universe.intern(b"foo");
        let mut b = BytecodeBuilder::new();
        b.push_literal(&mut vm, sym.oop());
        b.push_literal(&mut vm, sym.oop());
        b.ret_self();
        let sel = vm.universe.intern(b"test");

        let m = b.finish(&mut vm, sel, 0, 0);
        assert_eq!(m.literals().len(), 1);
        // Both operand bytes reference literal index 0.
        assert_eq!(m.bytecode_byte(1), 0);
        assert_eq!(m.bytecode_byte(3), 0);
    }

    #[test]
    fn builder_wide_literal() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        for i in 0..300i64 {
            b.push_literal(&mut vm, SmallInt::new(i).oop());
        }
        b.ret_self();
        let sel = vm.universe.intern(b"test");

        let m = b.finish(&mut vm, sel, 0, 0);
        assert_eq!(m.literals().len(), 300);

        // Entry 5: narrow form (0x05, u8 operand) at bci 5*2 = 10.
        assert_eq!(m.bytecode_byte(10), OP_PUSH_LITERAL);
        assert_eq!(m.bytecode_byte(11), 5);

        // Entries 0..256 are 2 bytes each (512 bytes); entry 256 onward is
        // 3 bytes each (wide form). Entry 299 is the 44th wide entry.
        let wide_start_bci = 256 * 2;
        let entry_299_bci = wide_start_bci + (299 - 256) * 3;
        assert_eq!(m.bytecode_byte(entry_299_bci), OP_PUSH_LITERAL_W);
        let lo = m.bytecode_byte(entry_299_bci + 1) as u16;
        let hi = m.bytecode_byte(entry_299_bci + 2) as u16;
        assert_eq!(lo | (hi << 8), 299);
    }

    #[test]
    fn builder_forward_patch() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        let l = b.new_label();
        b.jump_fwd(l); // bci 0..3, next_bci = 3
        b.push_smi_i8(1); // bci 3..5
        b.push_smi_i8(2); // bci 5..7
        b.bind(l); // target bci = 7
        b.ret_self();
        let sel = vm.universe.intern(b"test");

        let m = b.finish(&mut vm, sel, 0, 0);
        // operand at bci 1..3 should be 7 - 3 = 4.
        let lo = m.bytecode_byte(1) as u16;
        let hi = m.bytecode_byte(2) as u16;
        assert_eq!(lo | (hi << 8), 4);
    }

    #[test]
    fn builder_backward_distance() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        let l = b.new_label();
        b.bind(l); // target bci = 0
        b.push_smi_i8(1); // bci 0..2
        b.jump_back(l); // opcode_pos = 2, next_bci = 5
        b.ret_self();
        let sel = vm.universe.intern(b"test");

        let m = b.finish(&mut vm, sel, 0, 0);
        let lo = m.bytecode_byte(3) as u16;
        let hi = m.bytecode_byte(4) as u16;
        assert_eq!(lo | (hi << 8), 5);
    }

    #[test]
    #[should_panic(expected = "was never bound")]
    fn builder_unbound_label_panics() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        let l = b.new_label();
        b.jump_fwd(l);
        b.ret_self();
        let sel = vm.universe.intern(b"test");

        let _ = b.finish(&mut vm, sel, 0, 0);
    }

    #[test]
    #[should_panic(expected = "does not end in a return")]
    fn builder_requires_return() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        b.push_nil();
        b.pop();
        let sel = vm.universe.intern(b"test");

        let _ = b.finish(&mut vm, sel, 0, 0);
    }

    #[test]
    #[should_panic(expected = "argc")]
    fn builder_argc_limit() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        b.ret_self();
        let sel = vm.universe.intern(b"test");

        let _ = b.finish(&mut vm, sel, 16, 0);
    }

    #[test]
    fn ics_empty_but_present() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        b.ret_self();
        let sel = vm.universe.intern(b"test");

        let m = b.finish(&mut vm, sel, 0, 0);
        assert_eq!(m.ics().len(), 0);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "reaches the BCI sentinel range")]
    fn sentinel_bci_guard() {
        debug_assert_bci_sentinel_headroom(crate::oops::layout::BCI_SENTINEL_BASE);
    }

    #[test]
    fn sentinel_bci_guard_headroom_ok() {
        debug_assert_bci_sentinel_headroom(crate::oops::layout::BCI_SENTINEL_BASE - 1);
    }
}
