# Sprint S04 — Blocks, closures, non-local return, ensure

Objective: implement the hard Smalltalk semantics — CompiledBlock objects and
`push_closure`, heap Contexts with cross-depth `ctx_temp` addressing, the
BlockClosure `value` primitives, non-local return with dead-home detection and
unwinding through `ensure:`/`ifCurtailed:` markers, `cannotReturn:`, and
`mustBeBoolean`. Correctness before performance: everything here is
interpreter-only and becomes the semantic oracle for S12–S14. Implements SPEC
§2.3 (Closure/Context formats), §4.2 (opcodes 0F/10/11, 42/43), §5.4, §6.3
(partial), §10 (BlockClosure + control groups).

## Prerequisites

- **S3**: `send_generic`/`activate_method`, IC machinery, `lookup`,
  `install_method`, primitive table (`PrimResult { Ok, Fail }`), DNU path,
  `vm.out`, `InterpRegs` in `VmState`, well-known selectors
  `#mustBeBoolean` / `#cannotReturn:` already interned.
- **S2**: dispatch loop with opcodes 0F/10/11 (ctx temps, push_closure) and
  42/43 (block_return_tos, nlr_tos) still stubbed/trapping — they go live now.
- Frame layout constants in `src/oops/layout.rs`; all frame access via named
  constants (S3 requirement) — this sprint changes `FRAME_TEMPS_BASE`.

## Deliverables

- Frame extension: `FRAME_SERIAL=5`, `FRAME_MARKER=6`, `FRAME_TEMPS_BASE=7`;
  per-VM frame-serial counter; `Frame` marker/serial accessors.
- Universe extensions: `block_closure_klass`, `context_klass` (genesis).
- `ClosureOop`, `ContextOop` wrappers in `src/oops/` (formats per SPEC §2.3);
  home-ref smi pack/unpack in `layout.rs`.
- `CompiledBlock` support: `is_block` flag live, new flag bits `nctx:8`,
  `captures_ctx:1`; `BytecodeBuilder::build_block` (+ holder patch-up).
- Opcodes live: `push_ctx_temp`, `store_ctx_temp_pop`, `push_closure`,
  `block_return_tos`, `nlr_tos`.
- `src/interpreter/blocks.rs`: closure creation, `activate_block`, Context
  allocation (`has_ctx` methods *and* blocks).
- `src/interpreter/unwind.rs`: NLR, `continue_unwind`, marker interception in
  the return path, resume sentinels, `cannotReturn:`.
- Primitives 50–54 (`value` family), 60 (`ensure:`), 61 (`ifCurtailed:`);
  `PrimResult::Activated` variant.
- `mustBeBoolean` send from `br_true_fwd`/`br_false_fwd`.
- `tests/it_blocks.rs`, `tests/it_nlr.rs` (see `tests_s04.md`).

## Design

### Data structures

**Frame layout (extended).** Two slots inserted after the receiver:

```
FP+0 method   FP+1 saved_fp   FP+2 saved_bci   FP+3 context
FP+4 receiver
FP+5 serial   (smi: bits 31:0 = frame serial; bit 32 = marker kind,
               0 = ensure, 1 = ifCurtailed — meaningful only while the
               marker slot holds a closure)
FP+6 marker   (nil | ClosureOop [armed handler] | ArrayOop [UnwindToken])
FP+7… temps, then operand stack
```

Every frame (method and block alike) gets a serial from
`VmState.next_frame_serial: u32` (post-incremented at each push; per-process
when S17 arrives). Both slots are smis/oops, so the stack stays exactly
scannable (SPEC §5.1) — marker contents are GC-visible by construction.

> **SPEC-QUESTION:** SPEC §5.1 pins temps at FP+5 and has no serial or marker
> slot, but §5.4 requires frame serials (dead-home detection) and ensure
> markers "carried by frames". This sprint inserts FP+5/FP+6 and moves temps
> to FP+7; §5.1 should be amended. Cost: 2 words per frame.

**Frame accessors** (`src/interpreter/frame.rs`):

```rust
pub struct Frame<'a> { pub stack: &'a mut [Oop], pub fp: usize }

#[derive(Copy, Clone, PartialEq)]
pub enum MarkerKind { Ensure, IfCurtailed }

impl Frame<'_> {
    pub fn method(&self) -> MethodOop;              // CompiledMethod or CompiledBlock
    pub fn saved_fp(&self) -> usize;
    pub fn saved_bci(&self) -> u32;
    pub fn context(&self) -> Oop;                   // nil | ContextOop
    pub fn set_context(&mut self, c: Oop);
    pub fn receiver(&self) -> Oop;
    pub fn serial(&self) -> u32;
    pub fn marker(&self) -> Oop;                    // nil | ClosureOop | ArrayOop(UnwindToken)
    pub fn marker_kind(&self) -> MarkerKind;        // decodes serial-slot bit 32
    pub fn set_marker(&mut self, m: Oop, kind: MarkerKind);
    pub fn clear_marker(&mut self);
    pub fn argc(&self) -> u8;                       // from method flags
    pub fn receiver_arg_slot(&self) -> usize;       // fp - argc - 1 (closure lives here for block frames)
}
```

**CompiledMethod/CompiledBlock flag extension.** SPEC §4.4 flags gain:
`nctx:8` (number of heap-Context slots to allocate when `has_ctx`) and
`captures_ctx:1` (this *block* needs the enclosing context chain). New packing
(low→high): `argc:4 | ntemps:8 | has_ctx:1 | is_block:1 | prim_fails:1 |
captures_ctx:1 | nctx:8`. Constants in `layout.rs`.

> **SPEC-QUESTION:** SPEC §4.4's flags word has no field for the Context slot
> count or for whether a block captures the enclosing context; both are
> required to allocate/link Contexts. Proposed amendment above.

**BlockClosure** (`ClosureOop`, format `Closure`, SPEC §2.3):
`method` (CompiledBlock), `home` (packed smi, below), then
`[ncopied: smi][copied…]`. **Pinned copied[] convention** (S5 codegen and all
builder tests must follow it):

- `copied[0]` — the home receiver (`self` inside the block). Always present;
  `ncopied ≥ 1`.
- `copied[1]` — the enclosing `ContextOop`, present iff
  `method.captures_ctx`.
- `copied[2…]` (or `[1…]` when no ctx) — read-only value captures, in the
  order the compiler pushed them.

> **SPEC-QUESTION:** SPEC §2.3/§5.4 say the closure "copies the Context oop"
> and that blocks see `self`, but never say *where* in the closure either
> lives. The convention above pins it; SPEC should record it.

**Home reference** — `(process id, FP index, frame serial)` packed into one
smi (62 bits): `proc:8 (bits 61:54) | serial:32 (bits 53:22) | fp:22
(bits 21:0)`. FP indices are word indices into the process stack — 22 bits
caps the stack at 4M words (32 MiB), asserted at stack creation. Pack/unpack
in `layout.rs`:

```rust
pub struct HomeRef { pub proc: u8, pub serial: u32, pub fp: usize }
pub fn pack_home_ref(h: HomeRef) -> SmallInt;
pub fn unpack_home_ref(s: SmallInt) -> HomeRef;
```

**Dead-home detection (frame serials — how it works).** A `HomeRef` names a
stack *slot index* plus the serial the frame at that index had when the
closure was made. The home is **live** iff all three hold, checked in order:

1. `h.proc == vm.current_process_id` (v1: always 0 — but check it now; a
   future process's stack indices mean nothing here → `cannotReturn:`).
2. `h.fp + FRAME_MARKER < current stack top` — bounds check FIRST, so a
   shrunken stack never reads garbage.
3. `stack[h.fp + FRAME_SERIAL].serial_bits() == h.serial` — serials are
   never reused (monotonic u32 per VM), so a frame popped and later replaced
   by a different frame at the same index has a different serial. u32 wrap
   after 4G pushes is an accepted risk (`debug_assert` on wrap).

**Context** (`ContextOop`, format `Context`, SPEC §2.3): named slot
`home_hint` (nil | enclosing ContextOop — the static chain link), then
`[size: smi][slot…]` heap temps. Slots init to nil.

**UnwindToken** — a 2-element heap `Array` `[packed_home_ref: smi,
return_value: Oop]`, stored in a marker slot while that frame's handler runs
during an unwind. Distinguished from an armed handler by klass
(`array_klass` vs `block_closure_klass`); `nil` = no marker.

**Resume sentinels** — `saved_bci` values above any real bci
(`debug_assert!(bytecode_len < BCI_SENTINEL_BASE)` at method creation):

```rust
pub const BCI_SENTINEL_BASE:      u32 = 0x7FFF_0000;
pub const BCI_RESUME_ENSURE_RET:  u32 = 0x7FFF_0001; // finish an intercepted normal return
pub const BCI_RESUME_UNWIND:      u32 = 0x7FFF_0002; // continue a suspended NLR
```

A handler activation is pushed with its `saved_bci` set to a sentinel; when
it returns, `do_return` restores the sentinel bci and dispatches to the
matching resume routine instead of fetching bytecode.

**PrimResult extension**:

```rust
pub enum PrimResult { Ok(Oop), Fail, Activated }
```
`Activated` = the primitive already replaced the sender's continuation
(pushed a frame and set `vm.regs`); the interpreter must push **no** result
and just re-enter dispatch. Used by 50–54, 60, 61.

> **SPEC-QUESTION:** SPEC §10 and CONVENTIONS §2 pin
> `PrimResult (Ok(Oop) | Fail)`, but block-activation/control primitives
> cannot be expressed as value-returning: `value` must push a frame and
> continue interpreting. Strongtalk marks these `can_perform_NLR`-style flag
> bits on `primitive_desc` for the same reason. Proposal: add `Activated`.

### Algorithms

**1. Context allocation at activation** (`activate_method` step 4, now live;
same logic in `activate_block`): if `flags.has_ctx`, allocate
`Context(nctx)` with `home_hint` = *enclosing* context (nil for methods;
for blocks: the inherited context, below), store into FP+3. The frame is
fully formed (FP+3 pre-set to nil) *before* the allocation, so a stress-GC at
that alloc point sees a valid frame. Captured **arguments** are copied into
ctx slots by compiler-emitted prologue bytecode (`push_temp n;
store_ctx_temp_pop 0 i`), not by VM magic — keeps the VM simple; S5 codegen
owns this.

**2. `push_closure <u8 lit> <u8 ncopied>`**:
1. `blk = method.literals[lit]` (a CompiledBlock; debug_assert `is_block`).
2. Allocate `Closure(ncopied)` **before** popping — the ncopied values stay
   on the operand stack (GC roots) during the alloc (S7 pattern).
3. Pop `ncopied` values into `copied[0..ncopied)`; the value pushed *first*
   (deepest) becomes `copied[0]`. Codegen/builder therefore pushes: receiver,
   [context], value-captures, then `push_closure`.
4. `closure.method = blk`. Home ref (**propagation rule**): if the currently
   executing method `is_block`, copy `home` from the *current frame's own
   closure* (found in `frame.receiver_arg_slot()` — the receiver of the
   `value` send that activated it); else the current frame IS the home:
   `pack_home_ref(proc=0, serial=frame.serial(), fp=vm.regs.fp)`.
5. Push the closure.

**3. `activate_block(vm, closure, argc)`** (used by primitives 50–54, 60–61):
1. `blk = closure.method()`; if `blk.flags().argc != argc` → return `Fail`
   (the `value` method's fallback bytecode handles the error).
2. Bump `blk` invocation counter (blocks have counters too — S14 feedback).
3. Push a frame exactly like `activate_method` step 3 but: `FP+0 = blk`
   (a CompiledBlock); `FP+4 receiver = closure.copied(0)` (home `self`);
   FP+3 context: let `enc = if blk.captures_ctx { closure.copied(1) } else
   { nil }`; if `blk.has_ctx` → allocate `Context(blk.nctx, home_hint: enc)`
   into FP+3, else FP+3 = `enc` **directly** (aliasing the enclosing chain —
   this is what makes depth-addressing uniform).
4. Copy the value captures `copied[1 + captures_ctx as usize ..]` into the
   first temp slots (`ntemps` counts value-captures + block locals; block
   locals follow, nil-initialized).
5. Set `vm.regs = { fp, sp, bci: 0, method: blk }`. The closure itself
   remains in the caller-stack receiver-arg slot — NLR and nested
   `push_closure` read it from there.

**4. `ctx_temp` addressing** (`push_ctx_temp <depth> <idx>`,
`store_ctx_temp_pop`): `c = frame.context(); for _ in 0..depth
{ c = c.home_hint() }` then read/write `c.slot(idx)`. `depth` counts
**home_hint hops** — i.e. only `has_ctx` scopes — not lexical block nesting,
because ctx-less block frames alias the enclosing context at FP+3 (step 3
above). Example, 3-deep: method M (`has_ctx`, temp `a` at ctx slot 0) ⊃
block B1 (no own ctx) ⊃ block B2 (`has_ctx`, `b` at slot 0) ⊃ block B3 (no
own ctx). From B3: `b` = depth 0 idx 0; `a` = depth 1 idx 0 (one hop:
B2's ctx → home_hint → M's ctx). A nil hop result is a compiler bug:
`debug_assert!`, never a guest error.

**5. `value` primitives** (50 `value`, 51 `value:`, 52 `value:value:`, 53
`value:value:value:`, 54 `valueWithArguments:`): receiver must be a Closure
(else Fail). 50–53: `activate_block(vm, rcvr, argc)` → `Activated` (or Fail
on argc mismatch). 54: argument must be an `Array` (else Fail) whose size
equals the block's argc (else Fail); replace the array on the stack by its
spread elements (pop array, push elements — pure stack surgery, no
allocation), then activate. Note these primitives are exactly why
`PrimResult::Activated` exists; the interpreter's primitive-call site
handles it by simply returning to dispatch.

**6. `ensure:` (60) / `ifCurtailed:` (61)**: receiver = protected block,
arg = handler block; both must be Closures with argc 0 (else Fail).
Action: `activate_block(vm, receiver, 0)`, then set the **new** frame's
marker: `frame.set_marker(handler_closure, Ensure | IfCurtailed)`; return
`Activated`. The marker thus lives on the *protected block's activation*;
`ensure:` has no activation of its own, so its caller is the marked frame's
direct caller. Semantics: Ensure runs the handler on normal completion AND
on unwind; IfCurtailed only on unwind.

**7. Return interception** — factor S3's return into one routine used by
`return_tos`, `return_self`, and `block_return_tos`:

```rust
fn do_return(vm: &mut VmState, result: Oop) {
    let f = vm.current_frame();
    match f.marker_class() {
        MarkerNone | MarkerToken(_) => pop_and_deliver(vm, result), // tokens: see step 9
        MarkerHandler(h) => match f.marker_kind() {
            MarkerKind::IfCurtailed => { f.clear_marker(); pop_and_deliver(vm, result) }
            MarkerKind::Ensure      => intercept_ensure_return(vm, h, result),
        },
    }
}
```

`intercept_ensure_return`: (1) `clear_marker()` (re-entrancy: the second
`do_return` from this frame must not intercept again); (2) push `result`
onto this frame's operand stack; (3) push the handler closure and activate
it (`activate_block`, argc 0) with the handler frame's `saved_bci` forced to
`BCI_RESUME_ENSURE_RET`. When the handler returns, its result lands where
the pushed closure was; the frame resumes at the sentinel:
**RESUME_ENSURE_RET handler**: `sp -= 1` (discard handler result), pop
`result`, re-run `do_return(vm, result)` — marker is now nil, so it delivers.
`pop_and_deliver` = S3's return: write result into `stack[fp-argc-1]`,
`sp = fp-argc`, restore regs from saved slots; then if the restored
`saved_bci` is a sentinel, dispatch to its resume routine instead of
fetching bytecode.

A `MarkerToken` frame returning normally is impossible (its resume sentinel
consumes the token first) — `debug_assert`.

**8. `nlr_tos`**:
1. `value = pop`. `debug_assert!(vm.regs.method.is_block())` (`^` in a
   method compiles to `return_tos`; `nlr_tos` appears only in blocks).
2. `closure = stack[frame.receiver_arg_slot()]`;
   `home = unpack_home_ref(closure.home())` — thanks to the propagation rule
   this is always the home *method* frame, however deep the block nesting.
3. Liveness check (Data structures §dead-home). Dead → `cannot_return(vm,
   closure, value)` and stop the unwind.
4. `continue_unwind(vm, home, value)`.

**9. The unwind loop** (`src/interpreter/unwind.rs`):

```rust
pub enum UnwindStep { RanHandler, ReturnedFromHome, CannotReturn }

/// Walk from the current frame toward `home`, running marked handlers
/// innermost-first. Suspends itself (returns RanHandler) whenever a handler
/// must run; resumed via BCI_RESUME_UNWIND.
pub fn continue_unwind(vm: &mut VmState, home: HomeRef, value: Oop) -> UnwindStep {
    // re-validate home every entry: a handler may have changed the world
    if !home_is_live(vm, home) { return cannot_return_current_closure(vm, value); }
    // scan the FP chain from vm.regs.fp down to home.fp, inclusive of the
    // current frame, exclusive of home (home's own marker fires via the
    // do_return interception in step (c)):
    let m = innermost_marked_frame(vm, vm.regs.fp, home.fp);   // marker is a Closure
    match m {
        None => {                                              // (c) finish
            pop_frames_above(vm, home.fp);                     // set regs to home frame
            do_return(vm, value);                              // normal return from home
            UnwindStep::ReturnedFromHome
        }
        Some(mfp) => {                                         // (b) run one handler
            pop_frames_above(vm, mfp);                         // regs now at marked frame M
            let handler = frame(mfp).marker();
            let token = alloc_unwind_token(vm, home, value);   // Array [home_smi, value]
            frame(mfp).set_marker_token(token);                // replaces the handler
            activate_handler(vm, handler, BCI_RESUME_UNWIND);  // argc 0, sentinel resume
            UnwindStep::RanHandler                             // dispatch loop runs handler
        }
    }
}
```

**RESUME_UNWIND handler** (frame M resumes after its unwind handler
returned normally): `sp -= 1` (discard handler result); `token = M.marker()`;
`M.clear_marker()`; `continue_unwind(vm, token.home, token.value)` — M itself
now unmarked, so the scan proceeds past it (M is popped by the next step's
`pop_frames_above` or by the final home return). Handlers therefore run
strictly **innermost-first**, and both Ensure and IfCurtailed kinds run on
unwind.

Token allocation ordering (S7 pattern): `value` is still a Rust local when
the token Array is allocated — push `value` onto M's operand stack before
`alloc_unwind_token`, then store from the stack slot into the token, then
pop. Comment this at the site.

**10. NLR edge cases** (all get tests; spelled out here so implementation
choices are forced):

- **NLR through an `ensure:` whose handler itself does NLR.** While handler
  H (launched from marked frame M) runs, M's marker holds an UnwindToken.
  H's `nlr_tos` starts a *new* unwind from H's frame. Rule: a frame whose
  marker is a **token is popped without action** (`pop_frames_above` skips
  tokens; `do_return` case above) — the suspended original NLR is
  **abandoned**, the new NLR wins. This matches Smalltalk-80/Strongtalk
  semantics. If H's NLR home lies *below* M, M is popped and its token dies
  with it; if H's home is H's own home method above M — impossible, frames
  above M were already popped, so H's home is always at or below M's caller.
- **Handler raises DNU.** A DNU during handler execution is just a send
  (S3): a user-installed `doesNotUnderstand:` that returns normally lets the
  handler continue, then the unwind resumes via the sentinel. With no
  user DNU handler, the S3 fallback prints and terminates the process — the
  suspended unwind is abandoned with it (documented; no double fault).
- **Home on a different process.** `home.proc != current` →
  `cannotReturn:`, checked before any stack index is touched. v1 has one
  process so only tests exercise it (forged HomeRef), but the check ships
  now — S17 must not need to revisit unwinding.
- **Degenerate depth.** Home is the immediate caller of the block's `value`
  send (`[^v] value` inside the home method): scan finds no markers, two
  frames pop, home returns `v`. Must work with current == marked == none.
- **Home is the bottom frame.** NLR from a block whose home is `main`:
  `do_return` from the bottom frame ends interpretation with `value` as the
  program result (same path as normal program end).
- **Nested ensures.** `[[^1] ensure: [T print: 'inner']] ensure:
  [T print: 'outer']` — the NLR-ing frame is itself marked (scan includes
  the current frame), so: inner runs, then outer, then home returns.
  Ordering is the acceptance assertion.
- **Handler result is discarded** on both resume paths (`sp -= 1`).
- **Block re-entry after home return (non-NLR).** A closure whose home is
  dead may be `value`-d any number of times: activation needs only
  `copied[]` and the heap Context, both heap-owned. Only `nlr_tos` consults
  the home. (Gate golden program.)

**11. `mustBeBoolean`.** `br_true_fwd`/`br_false_fwd` pop a non-`true`/
`false` oop: roll `bci` back to the **branch opcode byte** (not its operand),
push the offending value back, and send `#mustBeBoolean` (unary) to it via
the normal send machinery (full lookup; a dedicated hidden IC is not worth a
site — use `lookup` directly and `activate_method`). When the send returns,
the branch re-executes with the result on top. A handler that keeps
returning non-booleans livelocks — that is user error (same as Smalltalk).
No handler installed → DNU fallback → terminate (v1 default per SPEC §6.3).

**12. `cannotReturn:`**:

```rust
fn cannot_return(vm: &mut VmState, closure: ClosureOop, value: Oop)
```
Push `closure` (receiver) and `value` (argument); full lookup + activate
`#cannotReturn:` on the closure's klass. No method → DNU fallback →
terminate. If a user handler *returns*, the failed NLR cannot be resumed:
VM error "resumed from cannotReturn:" — print trace, terminate process.

### Layer boundaries

- `oops/` gains `ClosureOop`/`ContextOop` + home-ref packing: pure
  representation, no interpreter knowledge.
- `interpreter/blocks.rs` is the only place that builds closures, Contexts,
  and block frames. `interpreter/unwind.rs` is the only place that pops more
  than one frame; nothing else may slice the stack.
- `runtime/primitives.rs` may call `activate_block`/`set_marker` (exported
  from `interpreter`) — the arrow is primitives → interpreter for the
  `Activated` family only; it still never touches `InterpRegs` directly.
- `bytecode/` builder additions stay data-only; block/holder patch-up happens
  at build time, not run time.
- Nothing in this sprint touches `runtime/lookup.rs` or the IC machinery.

## Implementation order

1. `layout.rs`: new frame constants, flag fields (`nctx`, `captures_ctx`),
   home-ref pack/unpack + unit tests, BCI sentinels. Re-point
   `FRAME_TEMPS_BASE` to 7 — S2/S3 tests must stay green (they will, if S3
   truly used the constants).
2. Frame serial assignment on every push + `Frame` serial/marker accessors.
3. `ClosureOop`/`ContextOop` wrappers + genesis klasses.
4. Context allocation for `has_ctx` methods; `push_ctx_temp`/
   `store_ctx_temp_pop` (depth 0 only exercisable yet) + tests.
5. `BytecodeBuilder::build_block` + literal wiring + disassembler support.
6. `push_closure` (incl. home propagation rule) + unit tests on closure
   contents.
7. `activate_block` + primitives 50–54 + `PrimResult::Activated` handling in
   the primitive call site. Nested ctx addressing now testable end-to-end.
8. `do_return` factoring + `block_return_tos`. (No markers yet — behavior
   identical; S2/S3 regression checkpoint.)
9. `nlr_tos` + home liveness + `continue_unwind` **without** markers (scan
   always finds none) + `cannotReturn:`.
10. Markers: primitives 60/61, ensure interception on normal return,
    RESUME_ENSURE_RET.
11. Unwind-with-handlers: UnwindToken, RESUME_UNWIND, token-abandonment rule.
12. `mustBeBoolean`.
13. Golden programs + `just gate-s04`.

## Pitfalls

- **Home propagation is the classic NLR bug.** `push_closure` inside a block
  must copy the *current closure's* home, not point at the current (block)
  frame. Getting it wrong makes `[[^1] value] value` return to the
  intermediate block and is invisible in 1-deep tests — test 3-deep.
- **Clear the marker before activating its handler** (both paths). Otherwise
  a handler that returns normally re-triggers interception → infinite
  regress; and an NLR out of the handler re-runs it.
- **`do_return` must be the single return choke point.** `return_tos`,
  `return_self`, `block_return_tos`, the home-return in `continue_unwind`,
  and both sentinels all flow through it — a marker missed on any one path
  breaks `ensure:` ordering silently. S12/S13 will add a compiled-frame case
  *here*, not a parallel mechanism (Self's SPARC-only deopt glue,
  reference-vm-analysis §2.5, is the cautionary tale — one continuation
  path, not per-caller variants).
- **Bounds-check before serial-check** in home liveness: reading
  `stack[h.fp+5]` of a popped region is stale-but-in-`Vec` memory — a valid
  smi that can *false-positive*. The order (proc, bounds, serial) plus
  monotonic serials makes false liveness impossible; document at the site.
- **Blocks have their own `ics` arrays and counters.** A send inside a block
  indexes the *block's* IC table (`vm.regs.method` is the CompiledBlock).
  Builder must allocate them; forgetting yields index-out-of-bounds on the
  home method's table only sometimes — assert `ic_idx*4 < ics.size()` in
  debug.
- **FP+3 aliasing is intentional** for ctx-less blocks (depth counts only
  `has_ctx` scopes). Do not "fix" it by allocating empty Contexts — that
  changes depth numbering and S14's Context-elision math.
- **Raw oops across allocation** (S7 choke-point pattern, binding since S3):
  `push_closure` allocates *before* popping captures (values parked on the
  stack); token allocation parks `value` on the operand stack; Context
  allocation happens after the frame is fully pushed. Never hold
  ClosureOop/ContextOop in Rust locals across a second allocation — when S7
  turns on `MACVM_GC_STRESS=1`, every such local is a moved-from dangling
  oop. The S7 retrofit then touches only the allocator internals + adds
  HandleScopes in `runtime/`, not this sprint's control flow.
- **Sentinel bcis must be unreachable as real bcis** — assert
  `bytecode_len < BCI_SENTINEL_BASE` at method creation, and roll
  `mustBeBoolean`'s bci back to the opcode byte, not into an operand.
- **`Activated` means no result push.** The primitive call site has three
  arms now; pushing a result on `Activated` corrupts the new frame's temp
  area by one slot — caught by S2's stack-discipline asserts if (and only
  if) they run on block frames too; extend them.
- **`valueWithArguments:` stack surgery** must not allocate (spread in
  place); check array size *before* popping it.
- **Don't stash unwind state in Rust-side VmState fields as raw oops.** The
  token lives in a frame slot precisely so the stack root-scan covers it
  (SPEC §5.1's design payoff). A `vm.pending_nlr: Option<(HomeRef, Oop)>`
  field would be an unscanned root — forbidden.
- **Serial slot doubles as marker-kind carrier** (bit 32): `set_marker` must
  preserve bits 31:0; write it via read-modify-write only.

## Interfaces for later sprints

- `activate_block(vm, closure, argc)` — S6 library (`do:`, `inject:into:`
  are plain Smalltalk over `value`), S11 compiled→interpreter block calls.
- `continue_unwind` / `do_return` / sentinels — S13 deopt resumes
  materialized frames through exactly these paths; S18 builds ANSI
  exceptions on `ensure:` + NLR without new VM hooks.
- `HomeRef` packing (proc field) + per-frame serials — S17 green processes
  (per-process serial counters + real proc ids); S13 uses serials as stable
  frame identities across deopt.
- Marker representation (`FRAME_MARKER` + kind bit) — S12's compiled-frame
  walker must recognize marked frames; keep the accessor API the only way
  in.
- `nctx`/`captures_ctx` flags + copied[] convention — S5 codegen contract;
  S14 Context elision reasons over `has_ctx`/`captures_ctx`.
- `ic_state` on CompiledBlocks — S14 reads block send feedback identically.

## Out of scope

- Source-level blocks/capture analysis (compiler decides temp-vs-ctx, emits
  ctx prologue stores, computes depths) → S5. This sprint's programs are
  builder-assembled with hand-computed depths.
- ANSI exceptions, `signal`/`on:do:` → S18 (built on this sprint's NLR +
  ensure).
- `thisContext` reflection → not in v1 (SPEC §1.3).
- "Clean block" optimization (closures without receiver/ctx capture) and
  Context elision → S14.
- Green processes, real `proc` ids, per-process serials → S17.
- Compiled frames in the unwind walk → S12/S13.
- `whileTrue:`/`ifTrue:` inlining decisions → S5 (bytecode already supports
  both inlined jumps and real sends).
