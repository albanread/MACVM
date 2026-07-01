# Sprint S2 — Bytecode + interpreter core (no sends)

Objective: define the full MACVM bytecode instruction set (immutable, side-IC
design per SPEC §4) with decoding, a test-only `BytecodeBuilder` assembler,
CompiledMethod object construction, a disassembler, and the tier-0
interpreter loop with its VM-managed `ProcessStack` and `Frame` discipline —
executing the non-send opcode subset. Hand-assembled straight-line and
looping kernels run and return correct oops.

SPEC sections implemented: §4.1–§4.2 (instruction set — decode/disassemble
ALL opcodes; execute the S2 subset), §4.4 (CompiledMethod layout), §5.1
(execution stack + frame layout), §5.2 (dispatch loop), §5.5 (jump_back poll
point, stubbed). Deferred within these sections: §4.3 IC table population
(S3), closures/contexts (S4).

## Prerequisites

From S0: `oops/` value representation, `layout.rs`. From S1: allocation API
(`alloc_indexable_oops`, byte objects), `Universe` + genesis (notably
`method_klass` with `non_indexable_size = 9`, `array_klass`,
`symbol_klass`), `intern`, `print_oop`, `VmState`. The stubs
`src/interpreter.rs` and (untouched, empty-ish) `src/lookup.rs` exist;
`interpreter.rs` is converted to a directory now. `src/bytecode/` is new.

## Deliverables

- `src/bytecode/` (new module; **no `unsafe`** — it manipulates the heap only
  through `oops` wrappers):
  - `opcode.rs` — opcode constants + `Instr` decoded form + `decode_at`.
  - `builder.rs` — `BytecodeBuilder` with labels and method finishing.
  - `method.rs` — CompiledMethod field accessors + `alloc_method`
    counterpart glue (raw alloc itself goes in `memory/alloc.rs`).
  - `disasm.rs` — disassembler.
- `src/interpreter/` (replaces `src/interpreter.rs`; no `unsafe`):
  - `stack.rs` — `ProcessStack`, `Frame` view, frame constants.
  - `mod.rs` — `run_method` entry, `dispatch` loop, `activate_method`,
    `return_from_frame`.
- `VmState` gains `stack: ProcessStack` and `pending: bool` (poll flag,
  no-op in S2).
- `tests/it_bytecode.rs`, `tests/it_interp.rs`, first golden files under
  `tests/golden/`.

## Design

### Data structures

**Opcode constants** (`src/bytecode/opcode.rs`) — numeric values are pinned
by SPEC §4.1–§4.2 and are the wire format; keep them as `pub const OP_*: u8`
plus a `#[repr(u8)] enum Opcode` with exactly these discriminants:

```rust
pub const OP_PUSH_SELF: u8          = 0x00;
pub const OP_PUSH_NIL: u8           = 0x01;
pub const OP_PUSH_TRUE: u8          = 0x02;
pub const OP_PUSH_FALSE: u8         = 0x03;
pub const OP_PUSH_SMI_I8: u8        = 0x04;  // <i8>
pub const OP_PUSH_LITERAL: u8       = 0x05;  // <u8>
pub const OP_PUSH_TEMP: u8          = 0x06;  // <u8>  unified arg/temp
pub const OP_STORE_TEMP: u8         = 0x07;  // <u8>
pub const OP_STORE_TEMP_POP: u8     = 0x08;  // <u8>
pub const OP_PUSH_INSTVAR: u8       = 0x09;  // <u8>
pub const OP_STORE_INSTVAR_POP: u8  = 0x0A;  // <u8>
pub const OP_PUSH_GLOBAL: u8        = 0x0B;  // <u8>  literal = Association
pub const OP_STORE_GLOBAL_POP: u8   = 0x0C;  // <u8>
pub const OP_POP: u8                = 0x0D;
pub const OP_DUP: u8                = 0x0E;
pub const OP_PUSH_CTX_TEMP: u8      = 0x0F;  // <u8 depth> <u8 idx>   S4 exec
pub const OP_STORE_CTX_TEMP_POP: u8 = 0x10;  // <u8 depth> <u8 idx>   S4 exec
pub const OP_PUSH_CLOSURE: u8       = 0x11;  // <u8 lit> <u8 ncopied> S4 exec
pub const OP_PUSH_LITERAL_W: u8     = 0x12;  // <u16>
pub const OP_SEND: u8               = 0x20;  // <u8 ic>               S3 exec
pub const OP_SEND_SUPER: u8         = 0x21;  // <u8 ic>               S3
pub const OP_SEND_W: u8             = 0x22;  // <u16 ic>              S3
pub const OP_SEND_SUPER_W: u8       = 0x23;  // <u16 ic>              S3
pub const OP_JUMP_FWD: u8           = 0x30;  // <u16>
pub const OP_JUMP_BACK: u8          = 0x31;  // <u16>
pub const OP_BR_TRUE_FWD: u8        = 0x32;  // <u16>
pub const OP_BR_FALSE_FWD: u8       = 0x33;  // <u16>
pub const OP_RETURN_TOS: u8         = 0x40;
pub const OP_RETURN_SELF: u8        = 0x41;
pub const OP_BLOCK_RETURN_TOS: u8   = 0x42;  //                       S4
pub const OP_NLR_TOS: u8            = 0x43;  //                       S4
```

All operands little-endian (SPEC §4). Decoded form:

```rust
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Instr {
    PushSelf, PushNil, PushTrue, PushFalse,
    PushSmi(i8), PushLiteral(u16 /*widened; also carries wide form*/),
    PushTemp(u8), StoreTemp(u8), StoreTempPop(u8),
    PushInstvar(u8), StoreInstvarPop(u8),
    PushGlobal(u8), StoreGlobalPop(u8),
    Pop, Dup,
    PushCtxTemp { depth: u8, idx: u8 }, StoreCtxTempPop { depth: u8, idx: u8 },
    PushClosure { lit: u8, ncopied: u8 },
    Send { ic: u16, super_: bool },
    JumpFwd(u16), JumpBack(u16), BrTrueFwd(u16), BrFalseFwd(u16),
    ReturnTos, ReturnSelf, BlockReturnTos, NlrTos,
}

/// Decode the instruction starting at `bci`. Returns (instr, next_bci).
/// Reads bytes one at a time via MethodOop::bytecode_byte — never takes a
/// slice of heap memory. Unknown opcode ⇒ panic (VM bug: bytecode is
/// VM-generated and immutable).
pub fn decode_at(m: MethodOop, bci: usize) -> (Instr, usize);
```

Note the interpreter's hot loop does NOT go through `Instr` (it matches the
raw opcode byte and reads operands inline, SPEC §5.2); `Instr`/`decode_at`
serve the disassembler, the builder's verifier, and S10's CFG decoder. Keep
the two agreeing via a unit test that cross-checks operand widths.

**CompiledMethod accessors** (`src/bytecode/method.rs`) — layout per SPEC
§4.4, body-word indexes appended to `layout.rs`:

```rust
pub const METHOD_SELECTOR_INDEX: usize = 0;   // Symbol
pub const METHOD_HOLDER_INDEX: usize   = 1;   // klassOop | nil (nil until S3 install)
pub const METHOD_FLAGS_INDEX: usize    = 2;   // smi, packing below
pub const METHOD_PRIMITIVE_INDEX: usize= 3;   // smi, 0 = none
pub const METHOD_COUNTERS_INDEX: usize = 4;   // smi (invocation:16 | reserved)
pub const METHOD_LITERALS_INDEX: usize = 5;   // Array
pub const METHOD_ICS_INDEX: usize      = 6;   // Array (stride 4, SPEC §4.3)
pub const METHOD_NAMED_WORDS: usize    = 7;   // ⇒ klass nis = 9
pub const METHOD_SIZE_INDEX: usize     = 7;   // smi: bytecode byte count
pub const METHOD_BYTECODE_BYTE_OFFSET: usize = BODY_OFFSET + 8 * (METHOD_SIZE_INDEX + 1); // 80

// flags packing — SPEC §4.4 "argc:4 | ntemps:8 | has_ctx:1 | is_block:1 | prim_fails:1"
pub const METHOD_FLAGS_ARGC_SHIFT: u32 = 0;  pub const METHOD_FLAGS_ARGC_BITS: u32 = 4;
pub const METHOD_FLAGS_NTEMPS_SHIFT: u32 = 4; pub const METHOD_FLAGS_NTEMPS_BITS: u32 = 8;
pub const METHOD_FLAGS_HAS_CTX_SHIFT: u32 = 12;
pub const METHOD_FLAGS_IS_BLOCK_SHIFT: u32 = 13;
pub const METHOD_FLAGS_PRIM_FAILS_SHIFT: u32 = 14;
```

```rust
impl MethodOop {
    pub fn selector(self) -> Oop;
    pub fn argc(self) -> usize;              // ≤ 15 (4-bit field)
    pub fn ntemps(self) -> usize;            // non-arg temps, ≤ 255
    pub fn has_ctx(self) -> bool;  pub fn is_block(self) -> bool;
    pub fn primitive(self) -> i64;
    pub fn literals(self) -> ArrayOop;  pub fn ics(self) -> ArrayOop;
    pub fn bytecode_len(self) -> usize;      // size slot
    pub fn bytecode_byte(self, bci: usize) -> u8;   // debug bounds-checked
}
```

`ntemps` is pinned as **non-arg temps only** (the count of `| a b |` slots);
args are addressed through the same `push_temp` index space but live in the
caller's pushed area (frame layout below).

**BytecodeBuilder** (`src/bytecode/builder.rs`) — test-only in spirit, but
S5's codegen reuses it, so it lives in the lib:

```rust
pub struct Label(usize);                      // index into builder's label table
pub struct BytecodeBuilder {
    code: Vec<u8>, literals: Vec<Oop>, n_ics: usize,
    labels: Vec<LabelState>,                  // Bound(bci) | Unbound(Vec<patch_site>)
}
impl BytecodeBuilder {
    pub fn new() -> Self;
    // one emitter per opcode, e.g.:
    pub fn push_smi_i8(&mut self, v: i8) -> &mut Self;
    pub fn push_literal(&mut self, o: Oop) -> &mut Self;   // dedups; auto-widens 05→12
    pub fn store_temp_pop(&mut self, t: u8) -> &mut Self;
    // control flow:
    pub fn new_label(&mut self) -> Label;
    pub fn bind(&mut self, l: Label);                       // here := target
    pub fn jump_fwd(&mut self, l: Label) -> &mut Self;      // l must bind later/here-after
    pub fn br_true_fwd(&mut self, l: Label) -> &mut Self;
    pub fn br_false_fwd(&mut self, l: Label) -> &mut Self;
    pub fn jump_back(&mut self, l: Label) -> &mut Self;     // l must already be bound
    pub fn ret_tos(&mut self) -> &mut Self;  pub fn ret_self(&mut self) -> &mut Self;
    /// Builds the heap objects: literal Array, empty-but-sized IC Array
    /// (S3 fills states; S2 methods have n_ics == 0), bytecode bytes.
    /// Asserts: all labels bound; last instruction is a return; argc ≤ 15;
    /// ntemps ≤ 255; bytecode_len ≤ SmallInt range (trivially).
    pub fn finish(self, vm: &mut VmState, selector: SymbolOop,
                  argc: usize, ntemps: usize) -> MethodOop;
}
```

**Jump offset encoding (pinned — SPEC §4.2 gives widths, not the base):**
all four jump opcodes carry `u16 distance` measured **from `next_bci`** (the
bci immediately after the 3-byte instruction):
`jump_fwd/br_*_fwd: target = next_bci + distance` (distance 0 = fall
through); `jump_back: target = next_bci - distance` (distance 3 = tight
self-loop; distance ≤ next_bci enforced by the builder). Builder patching:
forward emitters write `0xFFFF` placeholder and record the operand's byte
position; `bind` back-patches `target - next_bci`, erroring (Rust panic —
builder misuse is a VM bug) if the distance exceeds `u16::MAX` or is
negative; `jump_back` computes immediately and asserts the label is bound.

> **SPEC-QUESTION:** SPEC §4.2 does not state the jump base (from opcode
> byte vs after operands) nor whether distance 0 is legal. Pinned here:
> base = next_bci, distance unsigned, 0 legal for forward jumps. If a
> different convention was intended, fix it before S5's codegen hardcodes
> this one.

**ProcessStack & Frame** (`src/interpreter/stack.rs`) — SPEC §5.1, grows
upward, index-based (never pointers — the backing `Vec` may reallocate):

```rust
pub struct ProcessStack { slots: Vec<Oop>, pub sp: usize, pub fp: usize }
// capacity: 64 Ki oops default *(tunable)*; push checks sp < capacity and
// exits(70, "process stack overflow") in S2 — a send-depth check replaces
// this in S3 and a Smalltalk-visible error in S6.

pub const FRAME_METHOD: usize = 0;      // CompiledMethod oop
pub const FRAME_SAVED_FP: usize = 1;    // smi(caller fp index); smi(-1) = entry frame
pub const FRAME_SAVED_BCI: usize = 2;   // smi(caller resume bci); smi(0) at entry
pub const FRAME_CONTEXT: usize = 3;     // nil in S2 (Contexts are S4)
pub const FRAME_RECEIVER: usize = 4;    // copy of stack[fp - argc - 1]
pub const FRAME_FIXED_SLOTS: usize = 5; // temps at fp+5 …, operand stack after
pub const ENTRY_FRAME_SENTINEL: i64 = -1;
```

Slot addressing table (`fp` = index of the frame's `FRAME_METHOD` slot,
`argc`/`ntemps` read from the frame's method):

| what | stack index | written by |
|---|---|---|
| receiver (canonical) | `fp - argc - 1` | caller push |
| arg `i` (0-based) | `fp - argc + i` | caller push |
| method | `fp + 0` | activate |
| saved_fp / saved_bci | `fp + 1` / `fp + 2` | activate |
| context | `fp + 3` | activate (nil in S2) |
| receiver (fast copy) | `fp + 4` | activate |
| temp `t` for `t < argc` | `fp - argc + t` | — (unified index space) |
| temp `t` for `t ≥ argc` | `fp + FRAME_FIXED_SLOTS + (t - argc)` | activate (nil-init) |
| operand stack base | `fp + FRAME_FIXED_SLOTS + ntemps` | — |

Every slot in a frame is a valid oop at all times (saved_fp/saved_bci are
smis) — this is what makes S7's exact stack scan free (SPEC §5.1, §7.3).

`Frame` is a lightweight **view** (`Copy`): `struct Frame { pub fp: usize }`
with methods taking `&ProcessStack`/`&mut ProcessStack`
(`method(&self, st) -> MethodOop`, `receiver`, `temp(t)`, `set_temp`,
`saved_fp`, …) — it holds no oops itself, so it cannot go stale.

### Algorithms

**Activation** (`activate_method`) — receiver and `argc` args are already on
the stack (pushed by the caller, or by `run_method` for the entry frame):

1. `let fp_new = sp;`
2. push method oop; push `SmallInt(saved_fp as i64)` (or sentinel −1); push
   `SmallInt(saved_bci)`; push nil (context); push receiver copied from
   `slots[fp_new - argc - 1]`;
3. push `ntemps` nils;
4. `fp = fp_new; bci = 0`.

Overflow check once at entry: `sp + FRAME_FIXED_SLOTS + ntemps +
max_operand_depth` — S2 does not compute `max_operand_depth` (that needs a
verifier); check `sp + FRAME_FIXED_SLOTS + ntemps + 64 ≤ capacity`
*(tunable slack)* and document that S5's compiler will emit a real max-stack
into flags if this ever bites.

**Return** (`return_from_frame(vm, result) -> Option<Oop>`) — SPEC §5.1:
pop down to the receiver slot, overwrite with result:

1. `let base = fp - argc - 1;` (argc from the current frame's method)
2. read `saved_fp`, `saved_bci` smis;
3. `slots[base] = result; sp = base + 1;`
4. if `saved_fp == ENTRY_FRAME_SENTINEL`: `sp = base;` return
   `Some(result)` (loop exits; stack exactly as before `run_method` pushed);
5. else `fp = saved_fp as usize; bci = saved_bci as usize;` reload cached
   `method` from the restored frame; return `None`.

**Dispatch loop** (`src/interpreter/mod.rs`) — the SPEC §5.2 shape; a single
`match` in a hot loop over `&mut VmState`:

```rust
pub fn run_method(vm: &mut VmState, method: MethodOop,
                  receiver: Oop, args: &[Oop]) -> Oop {
    let st = &mut vm.stack;
    st.push(receiver); for &a in args { st.push(a); }
    activate_entry(vm, method, args.len());
    dispatch(vm)
}

fn dispatch(vm: &mut VmState) -> Oop {
    // Cached interpreter registers; re-loaded after any frame change.
    let mut method: MethodOop = current_method(vm);
    let mut bci: usize = 0;
    loop {
        debug_assert!(bci < method.bytecode_len(), "fell off method end");
        let op = method.bytecode_byte(bci);
        if vm.options.trace.bytecode { trace_bc(vm, method, bci, op); }
        match op {
            OP_PUSH_SELF  => { let r = frame_receiver(vm); push(vm, r); bci += 1; }
            OP_PUSH_NIL   => { push(vm, vm.universe.nil_obj); bci += 1; }
            // … one arm per S2 opcode; operand reads inline:
            OP_PUSH_SMI_I8 => {
                let v = method.bytecode_byte(bci + 1) as i8;
                push(vm, SmallInt::new(v as i64).oop()); bci += 2;
            }
            OP_JUMP_BACK => {
                let d = read_u16(method, bci + 1); let next = bci + 3;
                if vm.pending { poll(vm); }          // no-op hook, SPEC §5.5
                bci = next - d as usize;
            }
            OP_BR_FALSE_FWD => {
                let d = read_u16(method, bci + 1); let next = bci + 3;
                let t = pop(vm);
                bci = if t == vm.universe.false_obj { next + d as usize }
                      else if t == vm.universe.true_obj { next }
                      else { must_be_boolean(vm, t) /* S2: panic; S3: send */ };
            }
            OP_RETURN_TOS => {
                let r = pop(vm);
                match return_from_frame(vm, r) {
                    Some(result) => return result,
                    None => { method = current_method(vm); bci = resume_bci(vm); }
                }
            }
            OP_SEND | OP_SEND_SUPER | OP_SEND_W | OP_SEND_SUPER_W =>
                unimplemented!("sends land in S3"),
            OP_PUSH_CTX_TEMP | OP_STORE_CTX_TEMP_POP | OP_PUSH_CLOSURE |
            OP_BLOCK_RETURN_TOS | OP_NLR_TOS =>
                unimplemented!("closures/contexts land in S4"),
            bad => panic!("undefined opcode {bad:#04x} at bci {bci}"),
        }
    }
}
```

Executed subset per SPRINTS S2: hex 00–12 **minus 0F/10/11**, 30–33, 40–41.

> **SPEC-QUESTION:** SPRINTS S2 says "opcodes 00–12 … (no sends, no
> closures)", but 0F/10 (ctx temps) and 11 (push_closure) sit inside 00–12
> and cannot execute before Contexts/closures exist (S4). Pinned resolution:
> S2 decodes and disassembles ALL opcodes; the interpreter arms for
> 0F/10/11/42/43 are `unimplemented!` (a VM bug to reach in S2, since only
> the builder produces bytecode). Same for `mustBeBoolean`: SPEC §4.2
> requires a `#mustBeBoolean` send, impossible before S3 — S2 panics with a
> message naming the deferral; S3 MUST replace both (tracked in its detail
> doc).

Per-opcode notes (edge cases):
- `push_temp t` / `store_temp[_pop] t`: unified index — resolve via the slot
  table above; `debug_assert!(t < argc + ntemps)`.
- `push_instvar i` / `store_instvar_pop i`: receiver must be a mem oop
  (`debug_assert`); body index `i` is relative to the named part
  (`body_oop(i)`); `debug_assert!(i < receiver.klass().non_indexable_size()
  - HEADER_WORDS)`.
- `push_global g`: literal `g` is an Association (Slots, ivars pinned in S1:
  index 0 = key, index 1 = value); pushes `assoc.body_oop(1)`.
  `store_global_pop` writes index 1. (Association field order pinned in
  sprint_s01; SPEC §4.1 says only "literal = Association; pushes value".)
- `push_literal_w`: u16 index; builder auto-selects 05 vs 12 at emit time
  based on the literal-frame index.
- `dup`/`pop`: operand-stack only; `debug_assert!(sp > fp +
  FRAME_FIXED_SLOTS + ntemps)` — never dup/pop a frame slot.
- Empty method: `finish` requires a terminal return, so the minimal method is
  1 byte (`return_self`); the loop's `debug_assert!(bci < len)` catches
  builder bugs.
- `return_self`: pushes nothing — reads the frame receiver and goes straight
  to `return_from_frame`.

**Disassembler** (`src/bytecode/disasm.rs`):
`pub fn disassemble(u: &Universe, m: MethodOop) -> String`. Golden format
(pinned; goldens break if it drifts):

```
method #<selector> argc=<n> ntemps=<n> prim=<n> flags=<has_ctx?,is_block?>
  <bci>: <mnemonic> <operands>        ; jumps annotated "-> <target-bci>"
literals:
  <i>: <print_oop>
ics: <count>
```

Mnemonics = the SPEC §4.1/§4.2 names verbatim (`push_smi`, `br_false_fwd`,
…). Operand rendering: decimal; `push_literal n` appends
`; <print_oop of literal>`. Implemented over `decode_at` — the disassembler
is the decode test.

### Layer boundaries

- `bytecode/` may use `oops`, `memory::alloc`, `Universe` (for intern/print);
  it must not know about frames or `ProcessStack`.
- `interpreter/` may use `bytecode/` constants + `MethodOop` accessors; it
  must not decode via `Instr` in the hot loop, must not allocate in S2
  (assert: eden.top unchanged across `dispatch` — a test), and must not
  touch `memory/` internals.
- Nothing outside `interpreter/stack.rs` manipulates `sp`/`fp` directly.
- `main.rs` stays a stub (REPL is S5).

## Implementation order

1. `opcode.rs` constants + `decode_at` + unit tests (hand-packed byte
   arrays… note: decode needs a MethodOop, so first do step 2's
   `alloc_method` — order: 2 then 1 is fine; keep both compiling).
2. `memory/alloc.rs::alloc_method(vm, nbytes) -> MethodOop` (Method format:
   9 named-part words incl. size slot, `ceil(nbytes/8)` byte words,
   tagged_contents = false, size slot = nbytes, named oop fields nil-filled,
   flags/primitive/counters = smi 0) + `method.rs` accessors + tests.
3. `builder.rs` without labels (straight-line only) + `finish`; disassembler
   skeleton; first golden file.
4. Labels + all four jump forms + patching; goldens for jumps.
5. `stack.rs`: `ProcessStack`, frame constants, `Frame` view, push/pop +
   overflow check; unit tests with a fake frame.
6. `activate_method`/`activate_entry`/`return_from_frame`; unit test frame
   push/pop discipline without running bytecode.
7. `dispatch` loop, straight-line opcodes; `run_method`; kernel tests.
8. Jump/branch opcodes + `pending` poll hook; loop kernels (countdown loop).
9. `MACVM_TRACE=bytecode` tracing; `tests/it_interp.rs` full matrix;
   `just gate-s02`.

## Pitfalls

- **The `Vec<Oop>` stack reallocates.** Any cached raw pointer or `&mut`
  into `slots` across a push is UB-adjacent and, with indexes instead,
  merely stale — use indexes exclusively (`fp`, `sp`, slot numbers). The
  same discipline S1 set for the heap applies to the stack: it is scanned
  and (in S17, per-process) moved.
- **Frame slots are oops, always.** Never store a raw `usize` fp/bci into
  the stack — smi-encode (`SmallInt::new`). The S7 stack scan treats every
  slot as an oop; a raw index that happens to have tag 01 would be chased as
  a pointer. This is the single most important S2 invariant.
- **Receiver duplication.** The receiver lives at `fp - argc - 1` (caller's
  push) AND `fp + 4` (fast copy). `store` into the receiver is impossible in
  Smalltalk (self is not assignable) so they cannot diverge — but S4's
  Context and S13's deopt read the canonical one; pin: `push_self` reads
  `fp + 4`; return writes `fp - argc - 1`. Do not "optimize" one away.
- **Unified temp index across the fp boundary.** `push_temp 0` on a 2-arg
  method is `fp - 2`, not `fp + 5`. Off-by-one here passes single-arg tests
  and fails two-arg ones — test the matrix (argc, ntemps) ∈ {0,1,2} ×
  {0,1,2} explicitly.
- **Jump base convention.** All distances are from `next_bci` (pinned
  above). The classic bug is measuring from the opcode byte — every target
  lands 3 short. The disassembler's `-> target` annotation plus goldens
  catch this only if goldens are written from the SPEC arithmetic by hand,
  not regenerated from the code under test. Write the first three goldens
  BY HAND.
- **`bci` bookkeeping on branch-not-taken**: `br_*` always advances to
  `next_bci` (bci+3) when falling through — forgetting the operand width
  re-executes the operand bytes as opcodes.
- **i8 operand sign.** `push_smi_i8` operand is signed;
  `byte as i8 as i64`, not `byte as i64`. Test −1 and −128.
- **No allocation in the S2 loop.** All values pushed are existing oops or
  smis. If some arm "needs" to allocate (it shouldn't), that's a design
  smell in a pre-Handle world — S3 introduces the allocation-in-interpreter
  discipline together with sends.
- **`counters` smi, not machine int** (SPEC §4.4): leave it smi 0; S3 bumps
  it. Writing a raw 0 u64 would coincidentally be smi 0 — fine — but go
  through `SmallInt` anyway for greppability.
- **Don't build a `Bytecodes`-style 256-entry table** (Strongtalk generated
  threaded code and patched dispatch tables — analysis §3.3). MACVM's
  bytecode is immutable and the `match` compiles to a jump table
  (SPEC §5.2). Resist premature dispatch cleverness; the perf target is S6's
  measurement, not S2.
- **Method objects have a byte tail** — `tagged_contents = false`, so the
  S7 scavenger will scan only the 7 named oop slots + treat the size slot
  as smi; get the `alloc_method` mark bit right NOW or S7's first stress run
  chases bytecode bytes as oops.
- **`Universe` printer in disasm**: `print_oop` never allocates (S1
  guarantee) — safe to call mid-anything.

## Interfaces for later sprints

```rust
// S3 (sends) extends the dispatch loop and calls:
activate_method(vm, method: MethodOop, argc: usize)   // frame push (receiver+args pre-pushed)
return_from_frame(vm, result: Oop) -> Option<Oop>
BytecodeBuilder::{send(selector, argc), send_super(...)} // ADDED IN S3; allocates IC slots (n_ics)
MethodOop::ics() -> ArrayOop                           // stride-4 IC sites, SPEC §4.3
must_be_boolean(vm, Oop) -> usize                      // S3 replaces panic with a send
// S4: FRAME_CONTEXT slot + has_ctx flag are already in place; push_closure &
//     ctx-temp arms replace their unimplemented!()s.
// S5 (codegen) drives BytecodeBuilder + finish() directly.
// S10 (tier-1 decode) consumes Instr/decode_at and the jump-base convention.
// S13 (deopt) re-materializes frames via activate_method + the slot table —
//     keep FRAME_* constants and the table in this file authoritative.
```

## Out of scope

- Sends, IC state machine, lookup, primitives, DNU — S3 (arms present as
  `unimplemented!`).
- Closures, Contexts, `push_ctx_temp`, NLR, `ensure:` — S4.
- `#mustBeBoolean` as a real send — S3 (S2 panics; SPEC-QUESTION above).
- Invocation/loop counters counting — S3 (field exists, stays 0);
  compilation triggers — S10.
- Real GC-poll behavior at `jump_back` — S7 (`pending` flag + `poll()` no-op
  hook wired now so the loop shape never changes).
- Source compiler — S5; all S2 bytecode comes from `BytecodeBuilder`.
- Interpreter performance work (threaded dispatch, computed goto) — never,
  per SPEC §5.2; throughput is measured in S6.
