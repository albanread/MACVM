# Sprint S10 — Compile straight-line methods (tier 1 exists)

Objective: the simplest hot methods run as native AArch64 code — bytecode →
CFG → SSA-lite IR → linear-scan regalloc → emit via the S9 `Assembler`, stored
as `Nmethod`s in a `CodeTable`, dispatched from interpreter ICs, triggered by
invocation counters. Scope: **send-free methods plus inlined monomorphic-smi
primitive sends** (arith/compare); everything else stays interpreted.
Implements SPEC §8.1 (trigger, synchronous compile), §8.2 (nmethod, code
table), §8.3 (pipeline, minus feedback/inlining), the IC nmethod-id state of
§4.3, and the `MACVM_JIT` flag (CONVENTIONS §3).

## Prerequisites

- S0–S8 complete: `src/oops/` (tags, `layout.rs` constants), `src/memory/`
  (Universe, allocator, scavenger, full GC), `src/runtime/` (`VmState`,
  lookup), `src/bytecode/` (opcode enum, decoder, disassembler),
  `src/interpreter/` (frames on the process stack per SPEC §5.1, send
  protocol §5.3, `InterpreterIc`, invocation counters already counting),
  `src/frontend/` (source compiler).
- S9 complete: `Assembler` trait + `JasmAssembler`, `CodeBlob`, `CodeCache`,
  `JitWriteGuard`, `Reloc{Kind}`, listing format.

## Deliverables

- `src/compiler/decode.rs` — bytecode → CFG of basic blocks.
- `src/compiler/ir.rs` — `Ir` enum, `VReg`, blocks, stack→vreg conversion.
- `src/compiler/regalloc.rs` — live intervals, linear scan, spill slots.
- `src/compiler/emit.rs` — IR → `Assembler` calls, frame prologue/epilogue.
- `src/compiler/driver.rs` — eligibility scan + `compile_method` pipeline.
- `src/codecache/nmethod.rs` — `Nmethod`, `NmethodId`, `CodeTable`.
- `src/codecache/stubs.rs` — `call_stub` (Rust↔compiled trampoline),
  `stub_poll` (skeleton).
- Interpreter changes: counter-overflow trigger, IC dispatch on nmethod-id
  smis, `enter_compiled`, bailout handling, mixed-tier stack-trace support.
- `VmState` changes: pinned register block (D6), `VmOptions.jit`,
  `tier_links`, `compiled_depth`.
- GC change: `CodeTable::oops_do` wired as a root iterator (D8 — small but
  mandatory the moment code embeds oops).

## Design

### D1. Eligibility (what compiles in S10)

`fn eligible(vm: &VmState, method: MethodOop) -> bool` — a single linear scan
of the bytecode. ALL must hold:

1. Opcodes ∈ { `push_self, push_nil, push_true, push_false, push_smi_i8,
   push_literal, push_literal_w, push_temp, store_temp, store_temp_pop,
   push_instvar, push_global, pop, dup, jump_fwd, jump_back, br_true_fwd,
   br_false_fwd, return_tos, return_self, send, send_w` }.
   Excluded (→ interpreted): instvar/global/ctx stores, closures, ctx temps,
   super sends, block returns, NLR. (`store_instvar_pop` needs the write
   barrier + real slow-path sends — S11.)
2. Every `send` site's IC is **monomorphic with guard == smi_klass** and the
   cached target has `primitive ∈ SMI_INLINE` where
   `SMI_INLINE = { +, -, *, bitAnd:, bitOr:, bitXor:, <, <=, >, >=, =, ~= }`
   (primitive ids from `src/runtime/primitives.rs`; division excluded in v1).
   Empty/poly/mega/non-smi ICs → ineligible.
3. `flags`: `is_block == 0`, `has_ctx == 0`, `argc <= 5`, `primitive == 0`
   (primitive methods stay interpreted — their Rust fast path is already
   fast), `ntemps + max_stack` fits the frame budget (≤ 60 slots).
4. Method bytecode length ≤ 2 KiB *(tunable)*.

Ineligible methods set a **dont-compile bit** so the counter doesn't re-fire
every 10k sends: pin `counters` bit 16 as `compile_disabled` (SPEC §4.4 marks
bits ≥16 reserved) and reset the invocation count.

**Bailout-by-restart rule (S10-only, replaced in S11).** The inlined smi ops'
slow paths (non-smi operand, overflow) do not call anything: they return the
`BAILOUT` sentinel and the trampoline re-runs the whole method in the
interpreter from bci 0. This is only sound if no observable effect can precede
a bailout — guaranteed by eligibility: no stores to instvars/globals, no real
sends, no allocation. Temps are activation-local, so restarting is invisible.
Consequence: **S10 compiled code never allocates and never calls Rust** (its
only runtime touch is the rare poll stub, D5.6), so a GC can never occur under
an S10 compiled frame. State machine honest, hole-free.

### D2. Data structures

```rust
// src/compiler/ir.rs
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)] pub struct VReg(pub u32);
#[derive(Clone, Copy, PartialEq, Eq, Debug)]       pub struct BlockId(pub u32);
#[derive(Clone, Copy, PartialEq, Eq, Debug)]       pub struct PoolLit(pub u32); // compiler literal table id

pub struct VRegInfo { pub is_oop: bool }

#[derive(Clone, Copy, Debug)] pub enum SmiOp  { Add, Sub, Mul, And, Or, Xor }
#[derive(Clone, Copy, Debug)] pub enum CmpOp  { Lt, Le, Gt, Ge, Eq, Ne }
#[derive(Clone, Copy, Debug)] pub enum BailoutReason { SmiOpFailed, /* S11 grows */ }

pub enum Ir {
    // ── constants & moves ────────────────────────────────────────────────
    ConstSmi   { dst: VReg, value: i64 },              // emits tagged value<<2
    ConstPool  { dst: VReg, lit: PoolLit },            // ldr-literal (oops, addrs)
    Move       { dst: VReg, src: VReg },
    Param      { dst: VReg, index: u8 },               // 0=rcvr, 1..=args; entry block only
    // ── object access ────────────────────────────────────────────────────
    LoadKlass  { dst: VReg, obj: VReg },               // smi→smi_klass pool word
    LoadField  { dst: VReg, obj: VReg, byte_off: i32 },// tagged base, biased −1 at emit
    StoreField { obj: VReg, byte_off: i32, val: VReg, barrier: bool },   // S11 activates
    // ── smi primitives (tag checks + overflow fused; fail = bailout blk) ─
    SmiArith   { op: SmiOp, dst: VReg, a: VReg, b: VReg, fail: BlockId },
    SmiCmpBr   { op: CmpOp, a: VReg, b: VReg, if_true: BlockId, if_false: BlockId, fail: BlockId },
    SmiCmpVal  { op: CmpOp, dst: VReg, a: VReg, b: VReg, fail: BlockId }, // materialize true/false
    // ── control ──────────────────────────────────────────────────────────
    Jump       { target: BlockId },
    BoolBr     { val: VReg, if_true: BlockId, if_false: BlockId, not_bool: BlockId },
    GuardKlass { obj: VReg, expect: PoolLit, fail: BlockId },            // S11 prologue
    // ── calls & safepoints (S10 emits none of the first three) ──────────
    CallSend   { dst: VReg, site: u16, args: Vec<VReg> },                // S11; args[0]=rcvr
    CallRuntime{ dst: Option<VReg>, stub: StubId, args: Vec<VReg> },     // S11
    Alloc      { dst: VReg, klass: PoolLit, size_words: u32, slow: BlockId }, // S11
    Poll,                                              // loop back-edge flag check
    // ── exits ────────────────────────────────────────────────────────────
    Ret        { val: VReg },
    RetSelf,
    Bailout    { reason: BailoutReason },
}

pub struct IrBlock {
    pub id: BlockId,
    pub bci: usize,                    // first bytecode index (PcDesc seed)
    pub code: Vec<Ir>,
    pub entry_stack: Vec<VReg>,        // merge vregs (D3.3)
}

pub struct IrMethod {
    pub blocks: Vec<IrBlock>,          // blocks[0] = entry
    pub vregs: Vec<VRegInfo>,
    pub pool: Vec<PoolEntry>,          // PoolEntry { value: u64, kind: Option<RelocKind> }
    pub argc: u8, pub ntemps: u8,
    pub safepoints: Vec<SafepointId>,  // filled by linearize (S11 populates)
}
```

19 variants now; `CallSend`/`CallRuntime`/`Alloc`/`GuardKlass`/`StoreField`
are defined here (one enum for C+D phases) but only *emitted* from S11 on.
Every `Ir` implements `fn uses(&self, f: impl FnMut(VReg))` and
`fn defs(&self, f: impl FnMut(VReg))` — regalloc and the S12 verifier consume
these, so a new variant that forgets an operand is caught in one place.

```rust
// src/codecache/nmethod.rs
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)] pub struct NmethodId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NmState { Alive, NotEntrant, Zombie }        // S12/S13 use the tail

pub struct Nmethod {
    pub id: NmethodId,
    pub key_klass: KlassOop,           // customization key — WEAK at full GC (S12)
    pub key_selector: SymbolOop,
    pub code: CodeHandle,
    pub entry_off: u32,                // == verified_entry_off until S11
    pub verified_entry_off: u32,
    pub state: NmState, pub level: u8, pub version: u8,
    pub literal_off: u32,
    pub relocs: Vec<Reloc>,            // from CodeBlob, offsets now cache-relative? NO — blob-relative; add code.base
    pub frame_slots: u16,              // spill-slot count (frame layout D5.4)
    pub pcdescs: Vec<PcDesc>,          // S10: (ret_pc_off, bci) pairs at safepoints; S12 adds oopmap idx
    pub oopmaps: Vec<OopMap>,          // emitted from S10 (D7), consumed by GC in S12
    pub ic_sites: Vec<IcSite>,         // S11
}

pub struct CodeTable {
    slots: Vec<Option<Nmethod>>,                       // NmethodId = index; slots reused
    by_key: HashMap<(u64 /*KlassOop bits*/, u64 /*SymbolOop bits*/), NmethodId>,
    by_addr: Vec<(u64 /*code base*/, NmethodId)>,      // sorted; binary-search PC→nmethod
}
impl CodeTable {
    pub fn install(&mut self, nm: Nmethod) -> NmethodId;
    pub fn get(&self, id: NmethodId) -> Option<&Nmethod>;
    pub fn find_by_pc(&self, pc: u64) -> Option<NmethodId>;
    pub fn lookup(&self, k: KlassOop, sel: SymbolOop) -> Option<NmethodId>;
    /// Visit every embedded-oop word in every alive nmethod (D8).
    pub fn oops_do(&mut self, cache: &mut CodeCache, f: &mut dyn FnMut(&mut u64));
    /// Rebuild by_key after a moving full GC (klass addresses changed).
    pub fn rehash(&mut self);
}
```

`by_key` keys are raw oop bits — they dangle across a moving full GC exactly
like the lookup cache; `rehash()` is called from full-GC's update phase (the
Nmethod fields `key_klass/key_selector` are updated as roots first; S12
refines with weakness — until then they are strong roots).

### D3. Algorithms

**D3.1 Bytecode → CFG** (`decode.rs`). Two passes over the (immutable,
SPEC §4) bytecode using the S2 decoder:

1. **Leaders**: bci 0; every jump/branch target; every bci following a
   `jump_*`/`br_*`/`return_*`. Collect in a `BTreeSet<usize>`.
2. **Blocks**: split at leaders; each block records its bci range and
   terminator successors (fallthrough, jump target, branch pair). Backward
   jumps (`jump_back`) mark the target block `is_loop_header` and insert an
   `Ir::Poll` at the jump source (before the `Jump`). Malformed bytecode
   (target not an instruction start) is a `panic!` — the frontend is trusted;
   the decoder re-verifies in debug builds only.

**D3.2 Stack→vreg conversion — abstract interpretation of the operand
stack.** The interpreter's operand stack disappears; each stack slot becomes a
vreg. Per method:

- `temp_vregs[i]` — ONE vreg per unified arg/temp slot for the whole method
  (multiple defs allowed; this is the "SSA-lite" concession, SPEC §8.3 Δ).
  Entry block emits `Param { dst: temp_vregs[i], index: i }` for receiver +
  args (receiver is `Param{index:0}` → its own vreg `self_vreg`, args map to
  temp slots 0..argc-1 per SPEC §5.1's unified numbering).
- A worklist pass computes `entry_depth[b]` for every block (simulate depth
  only; assert every predecessor agrees — frontend-emitted bytecode
  guarantees consistency, e.g. the `ifTrue:ifFalse:` value pattern).
- **Merge rule**: a block with entry_depth == 0 has `entry_stack = []`. A
  block with exactly one predecessor AND no back-edge into it inherits the
  predecessor's exit stack vector verbatim (no copies). Otherwise (join or
  loop header with nonzero depth) it owns fresh **merge vregs**
  `entry_stack = [m0..m(depth-1)]`; every predecessor appends
  `Move { dst: m_i, src: its_exit[i] }` before its terminator. (Loop headers
  with nonzero entry depth cannot come from our frontend — inlined control
  selectors always close the stack before `jump_back`; assert, don't
  implement the general case.)
- **Translation**: walk each block with a `Vec<VReg>` simulated stack seeded
  from `entry_stack`:

| bytecode | effect |
|---|---|
| `push_self` | push `self_vreg` |
| `push_nil/true/false` | `ConstPool{well-known oop}` → push |
| `push_smi_i8 v` | `ConstSmi{v}` → push |
| `push_literal[_w] n` | literal oop → `ConstPool` → push (literal read at compile time from the method's literal frame; the OOP value goes into the pool with `RelocKind::Oop`) |
| `push_temp n` | push `temp_vregs[n]` — BUT if the temp may be reassigned later on this path, push a copy: v1 rule = always `Move` into a fresh vreg (cheap; regalloc coalescing is a later nicety) |
| `store_temp[_pop] n` | `Move{dst: temp_vregs[n], src: pop/tos}` |
| `push_instvar n` | `LoadField { obj: self_vreg, byte_off: 16 + 8*n }` → push |
| `push_global n` | `ConstPool{Association oop}` then `LoadField{value_off}` → push |
| `pop` / `dup` | sim-stack pop / push(tos) |
| `send ic` (smi-inlined) | pop argc+1; `SmiArith`/`SmiCmpVal` (or `SmiCmpBr` when the ONLY consumer is an immediately-following `br_*` — the fusion peephole); `fail` = the method's shared bailout block |
| `br_true_fwd/br_false_fwd` | pop → `BoolBr` (not_bool = bailout block; interpreter re-execution raises `mustBeBoolean` correctly) |
| `jump_fwd/back` | `Jump` (+ `Poll` for back) |
| `return_tos` | `Ret{pop}` ; `return_self` | `RetSelf` |

  One shared `bailout_block` per method holds `Bailout{SmiOpFailed}`.
- **is_oop bits**: every vreg above is `is_oop = true` (smis are valid oops).
  `is_oop = false` exists only for emit-internal scratch (none in S10's IR —
  the multiply untag lives inside `SmiArith`'s emit sequence using x16/x17).

**D3.3 Lowering.** The `Ir` above is already AArch64-shaped: each variant
emits a fixed instruction sequence (D5). No separate machine IR in v1.

> **SPEC-QUESTION:** SPEC §8.3 lists a distinct "lower → machine-level IR
> (2-address)" stage. S10 collapses it: the SSA-lite IR is machine-shaped and
> emit walks it directly. A separate LIR buys nothing until S14's inliner
> creates non-trivial ops. Proposed Δ note for SPEC §8.3; revisit at S14.

**D3.4 Linearization + live intervals** (`regalloc.rs`). Order blocks in
reverse postorder (entry first; loop bodies contiguous). Number instructions
sequentially (`u32`), record safepoint positions (S10: none, since no
CallSend/CallRuntime/Alloc are emitted; the machinery still runs so S11 only
flips eligibility). Liveness: single backward pass per block with a live-set
`HashSet<VReg>` iterated to fixpoint over the CFG (methods are ≤ a few
hundred ops; O(n·blocks) is fine). Intervals:

```rust
pub struct LiveInterval {
    pub vreg: VReg,
    pub start: u32,            // first def position
    pub end: u32,              // last use position (inclusive)
    pub is_oop: bool,
    pub crosses_safepoint: bool,
    pub assignment: Option<Assignment>,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Assignment { Reg(u8), Spill(SpillSlot) }
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpillSlot(pub u16);         // slot i lives at [x29 − 8·(i+1)]
```

Holes are ignored (one conservative `[min def, max use]` per vreg — classic
Poletto/Sarkar linear scan).

**D3.5 Linear scan + the spill-all policy.** Allocatable set (from
`arm64.md` §3): **x0–x15**. Reserved: x16/x17 (assembler/veneer scratch),
x18 (platform), x19–x23 (unused in v1 — Δ: not allocated, so no callee-save
spills in the prologue), x24–x28 (VM registers; x28 = `VmState`), x29/x30/sp.

Policy, in order:
1. Any interval with `crosses_safepoint` gets `Spill` — unconditionally,
   whole-lifetime, never a register. This is the invariant S12's oop maps
   stand on (**registers are all spilled at safepoints; maps cover stack
   slots only**) and it is enforced HERE, not merely assumed: after
   allocation, `debug_assert!` no `Reg` interval crosses a safepoint
   position. Resist per-range splitting or callee-saved carrying until v2.
2. Remaining intervals: sort by start; active list; expire ended; if 16 regs
   busy, spill the active interval with the furthest end (take its register).
3. Spill slots are allocated monotonically (`frame_slots = max + 1`); each
   `SpillSlot` inherits its interval's `is_oop` (recorded in a per-method
   `slot_is_oop: BitVec` — the raw material of S12's `OopMap`s).

Spilled operands: emit loads/stores around each use/def via x16/x17 scratch
(at most 2 spilled sources + 1 dest per Ir op — verify by construction;
`SmiArith` with both operands spilled uses x16 and x17, writes through x16).

**D3.6 Emit** (`emit.rs`). Walk blocks in linear order; bind one `Label` per
block; emit per-op sequences (D5); `Ret`/`RetSelf` share one epilogue label.
Then `asm.finish()` → `CodeBlob`; `pcdescs` recorded for every block start
(`bci`) — enough for mixed traces now, extended at safepoints in S11/S12.

### D4. nmethod publish + dispatch

**Compile driver** (`driver.rs`):

```rust
pub fn compile_method(vm: &mut VmState, rcvr_klass: KlassOop, method: MethodOop)
    -> Option<NmethodId>
```
1. `eligible`? else set `compile_disabled`, return None.
2. decode → convert → regalloc → emit → `CodeBlob`.
3. `cache.alloc(blob.code.len())`? else log + disable JIT trigger, None.
4. `cache.publish` → construct `Nmethod { key: (rcvr_klass, selector), … }`
   → `code_table.install` → return id. **The compiler allocates NOTHING on
   the Smalltalk heap** (pool literal oops are read, not created), so no
   Handles are needed and no GC can strike mid-compile. Keep it that way
   until S13 (deopt metadata may want heap objects — it must handle-ize).

**Trigger** (interpreter, SPEC §5.3/§8.1): on counter overflow in
`activate_method`, when `VmOptions.jit != Off`:
`compile_method(vm, klass_of(rcvr), method)`; on success, rewrite the sending
IC's target slot to `SmallInt::from(id.0 as i64)` and enter the compiled code
immediately for THIS activation. `MACVM_JIT=threshold=N` overrides
`InvocationCounterLimit` (N=1 → compile on first send: the differential
gate). `MACVM_JIT=off` → never trigger. Loop-counter overflow also calls
`compile_method` in S10 but only benefits the NEXT invocation (OSR is S15).

**IC dispatch on nmethod ids** (SPEC §4.3 mono state, target = smi): in the
send fast path, if `target.is_smi()`:

```text
id  = target.smi_value()
nm  = code_table.get(id)          — None → treat as IC miss (re-lookup, reinstall)
valid = nm.state == Alive
     && nm.key_klass == ic.guard && nm.key_selector == ic.selector
valid?  enter_compiled(vm, nm, argc)   : IC miss path (self-heals the IC)
```

The validity check is the **stale-id self-heal**: freed/reused NmethodIds can
never dispatch wrongly, and S12's nmethod flushing needs no interpreter-IC
sweep (see S12 D6).

**`enter_compiled`** (Rust, `src/interpreter/compiled_call.rs`): receiver+args
already sit contiguously on the process stack (`sp-argc-1 .. sp-1`, SPEC
§5.1). Push `TierLink::IntoCompiled { interp_frame, entry_sp }`, bump
`compiled_depth`, invoke the **call stub**:

```rust
// generated once at startup into the code cache (stubs.rs):
// extern "C" fn(entry: u64, vm: *mut VmState, argv: *const u64, argc: u64) -> u64
// saves x19..x28 + fp/lr, x28 := vm, loads x0..x5 from argv[0..argc],
// blr entry, restores, ret.  Its frame is the anchor the stack walker stops at.
```

Result handling: `BAILOUT` sentinel (`0b10` — reserved tag SPEC §2.1, never a
real oop) → pop TierLink, activate the method in the interpreter as if never
compiled. Otherwise: pop TierLink, pop rcvr+args from the operand stack, push
result, resume caller — identical stack effect to an interpreted return.

### D5. Frame layout & per-op emit sequences

**D5.1 Compiled calling convention (pinned):** receiver in **x0**, args
left-to-right in **x1..x5** (argc ≤ 5 by eligibility), result in **x0**,
x28 = `&VmState` (callee-saved, established by the call stub), x29/x30
standard AAPCS64 frame chain. sp 16-aligned always.

**D5.2 Prologue / epilogue:**

```text
prologue:  stp x29, x30, [sp, #-16]!
           mov x29, sp
           sub sp, sp, #frame_bytes        ; 8·frame_slots rounded to 16
           ; store Param regs to their assigned slots (spilled params only)
epilogue:  mov sp, x29
           ldp x29, x30, [sp], #16
           ret
```

Frame (grows down; the native stack — interpreter frames stay on the process
stack, SPEC §5.1):

```text
x29 + 8   saved lr          x29 + 0   saved x29 (caller chain)
x29 − 8   spill slot 0      x29 − 16  spill slot 1   …   (slot i at x29−8(i+1))
sp        (16-aligned)
```

The x29 chain is unbroken from compiled frames through the call stub to
Rust — "FP chain compatible with interpreter walking" means the mixed-tier
walker (D6) can traverse native frames by `[x29]`, classifying each by return
pc (`code_table.find_by_pc`), and switches to/from process-stack interpreter
frames at `TierLink`s.

**D5.3 Emit table** (xd/xa/xb = allocated or scratch-reloaded regs):

| Ir | AArch64 sequence |
|---|---|
| `ConstSmi v` | `mov xd, #(v<<2)` (or movz/movk pair for wide — via `emit("mov",…)` which encode.rs expands) |
| `ConstPool` | `ldr_literal(xd, lit)` |
| `Move` | `mov xd, xa` |
| `LoadField off` | if `off−1 ∈ [−256,255]`: `ldur xd, [xa, #off−1]`; else `sub x16, xa, #1; ldr xd, [x16, #off]` (P4) |
| `SmiArith Add` | `orr x16, xa, xb; tst x16, #3; b.ne fail; adds xd, xa, xb; b.vs fail` |
| `SmiArith Sub` | tag check as above; `subs xd, xa, xb; b.vs fail` |
| `SmiArith Mul` | tag check; `asr x16, xa, #2; mul x17, x16, xb; smulh x16, x16, xb; cmp x16, x17, asr #63; b.ne fail; mov xd, x17` |
| `SmiArith And/Or/Xor` | tag check; `and/orr/eor xd, xa, xb` (tags 00 ⊕ 00 = 00 — no overflow) |
| `SmiCmpBr op` | tag check both → fail; `cmp xa, xb; b.<cond> if_true; b if_false` |
| `SmiCmpVal op` | tag check; `cmp xa, xb`; `ldr_literal x16,#true; ldr_literal x17,#false; csel xd, x16, x17, <cond>` |
| `BoolBr` | `ldr_literal x16, true_obj; cmp xv, x16; b.eq T; ldr_literal x16, false_obj; cmp xv, x16; b.eq F; b not_bool` |
| `Jump` | `b L` (fallthrough elided when next in linear order) |
| `Poll` | `ldr w16, [x28, #POLL_OFF]; cbnz w16, poll_slow` — `poll_slow` (per-method tail block) does `bl stub_poll` then `b back`; stub_poll saves/restores x0–x15 itself (D5.6) |
| `Ret` | move val to x0 (if not already); `b epilogue` |
| `Bailout` | `mov x0, #2; b epilogue` (BAILOUT sentinel) |

Condition-flag lifetimes never cross an Ir op boundary (each sequence is
self-contained) — document this as an emit invariant.

**D5.6 `stub_poll`** (startup stub): pushes x0–x15 (8 `stp`s), calls the Rust
`extern "C" fn rt_poll(vm: *mut VmState)` via `call_far`, restores, `ret`.
`rt_poll` in S10 handles only the interrupt/trace flag (no GC — nothing can
have requested one, D1). It exists now so loop codegen is final.

### D6. VmState register block

Pin a `#[repr(C)]` prefix struct at offset 0 of `VmState`, offsets as consts
in `src/oops/layout.rs` (single source of truth, CONVENTIONS §2):

```rust
#[repr(C)]
pub struct VmRegBlock {
    pub eden_top: u64,          // +0   (S11 inline alloc)
    pub eden_end: u64,          // +8
    pub old_start: u64,         // +16  (S11 barrier)
    pub card_base_biased: u64,  // +24  cards − (old_start >> 9)
    pub poll_flag: u32,         // +32  POLL_OFF
    pub _pad: u32,
    pub last_compiled_fp: u64,  // +40  anchor (S11 runtime entries)
    pub last_compiled_pc: u64,  // +48
}
```

The allocator/scavenger (S7) must WRITE `eden_top/eden_end` here (or move its
fields into this block — preferred). Also new in `VmState`:
`tier_links: Vec<TierLink>`, `compiled_depth: u32`, `code_cache: CodeCache`,
`code_table: CodeTable`, `options.jit: JitMode`.

```rust
pub enum TierLink {
    IntoCompiled { interp_frame: usize, entry_sp: u64 },   // I→C (call stub)
    IntoInterpreter { compiled_fp: u64, compiled_ret_pc: u64 }, // C→I (S11)
}
```

### D7. Oop-map raw material (emitted now, consumed in S12)

Regalloc already knows `slot_is_oop`. Emit stores, per safepoint (none fire
in S10; the plumbing is exercised the moment S11 emits calls):
`OopMap { bits }` over spill slots + a `PcDesc { pc_off, bci, oopmap: u16 }`
at each call's RETURN address offset. Shipping the emission path now means
S11 only adds call sites and S12 only adds the GC consumer.

### D8. `CodeTable::oops_do` — wired to GC now

The first published nmethod embeds oops (method literals, well-knowns) in its
pool. Method literals are usually tenured by compile time but NOT guaranteed —
so from S10, scavenge root iteration (SPEC §7.3 step 1) and full-GC mark +
update phases call `code_table.oops_do(cache, &mut f)`: for each alive
nmethod, for each `Reloc { kind: Oop | KeyKlassOop }`, present
`&mut *(code.base + literal-relative offset)` to the GC closure. One
`JitWriteGuard` wraps the whole iteration (pool words are data — the guard's
flush is harmless). Full GC additionally calls `code_table.rehash()` and
updates `Nmethod.key_klass/key_selector` fields (Rust-side oops → they are
roots too). S12 takes over the weak treatment of `KeyKlassOop`; until then
both kinds are strong.

> **SPEC-QUESTION:** SPRINTS.md places "nmethod literal-pool oop update in
> GC" under S12, but any S10 nmethod whose literal moves at a scavenge is
> already stale. The hook is ~40 lines; S10 ships it (S12 still owns
> compiled-FRAME scanning, weakness, and the flushing protocol). Proposed
> reading: S12's item covers the *integration + gates*, S10 the mechanism.

## Implementation order

1. `VmRegBlock` + layout consts + allocator field move; `VmOptions.jit`
   parsing. (Interpreter-only tests still green.)
2. `decode.rs` (CFG) + unit tests on builder-assembled bytecode.
3. `ir.rs` conversion (stack sim, merges, smi-send inlining) + IR-dump tests.
4. `regalloc.rs` (intervals, policy, slots) + unit tests.
5. `emit.rs` + `stubs.rs` call stub; execute a hand-fed IR method raw
   (integration test, no interpreter involvement).
6. `nmethod.rs` (`Nmethod`, `CodeTable`) + `oops_do` wired into both
   collectors + `rehash`.
7. `driver.rs` eligibility + pipeline; `MACVM_TRACE=jit` logging.
8. Interpreter: trigger, IC smi dispatch, `enter_compiled`, bailout,
   `TierLink`s; mixed-tier `print_stack_trace`.
9. Gate runs (`tests_s10.md`).

## Pitfalls

- **P1 — the BAILOUT sentinel must be unforgeable.** `0b10` has the reserved
  tag (SPEC §2.1) and is never constructed as an oop; `debug_assert!` in
  `enter_compiled` that a non-sentinel result satisfies oop invariants.
- **P2 — restart-safety is an eligibility theorem, not a hope.** If you relax
  the opcode set (e.g. admit `store_instvar_pop`) without replacing
  bailout-by-restart, a bailout after the store re-executes it. The
  eligibility function and the bailout strategy must change TOGETHER (S11).
- **P3 — counters live in a heap smi field.** `counters` is part of a
  CompiledMethod (SPEC §4.4); bumping it is a plain smi store, no barrier
  concerns (smis), but the compile trigger reads a possibly-moved method oop
  after any allocation — the trigger path allocates nothing before use.
- **P4 — biased offsets vs. `ldur` range.** `off−1` fits ldur only in
  ±255: instvar index ≤ ~29. Beyond: untag the base first (D5.3 row). Do not
  emit a scaled `ldr` on the tagged base with a rounded offset — off-by-one
  reads are silent heap corruption.
- **P5 — multiply must untag exactly one operand** (SPEC §2.1) and detect
  overflow via `smulh` sign-extension comparison — `b.vs` does NOT work for
  `mul` (no flags). The sequence in D5.3 is the whole story; port it exactly.
- **P6 — `Poll` slow path clobbers nothing.** Regalloc treats `Poll` as
  neither call nor safepoint; therefore `stub_poll` must preserve x0–x15 and
  the flags? No — flags need not survive (no Ir sequence spans a Poll), but
  x0–x15 MUST. Assert the stub's save-set against the allocatable set in a
  test.
- **P7 — CodeTable key rehash after full GC.** Raw-oop-keyed hash maps
  dangle after compaction exactly like the lookup cache (SPEC §7.5 step 4);
  forgetting `rehash()` yields "customized nmethod never found again" —
  a silent perf bug, or worse a WRONG hit if a new klass lands on a reused
  address. `rehash()` is mandatory, and `find_by_pc` (address-keyed into the
  non-moving code cache) is the only table that survives untouched.
- **P8 — don't compile while holding `&mut` into the heap.** The driver takes
  `&mut VmState`; it must copy the bytecode + literal oops it needs up front
  (bytecode bytes into a `Vec<u8>`), because Rust borrowck will otherwise
  fight every later stage — and S13's allocating metadata will need the
  discipline anyway.
- **P9 — the call stub must save x19–x28** even though v1 compiled code never
  touches x19–x23: Rust (the caller of the stub) assumes AAPCS64
  callee-saved semantics, and the stub itself writes x28.
- **P10 — listing-based goldens, not a disassembler.** There is no in-tree
  AArch64 disassembler; the "disasm golden" gate uses `CodeBlob.listing`
  (S9 pinned format). Keep listing generation deterministic (no addresses,
  only offsets).

## Interfaces for later sprints

- S11: `Ir::{CallSend, CallRuntime, Alloc, GuardKlass, StoreField}` variants
  (already defined), `IcSite` slot in `Nmethod`, `IrMethod.safepoints`,
  eligibility relaxation points (D1), `entry_off != verified_entry_off`,
  `TierLink::IntoInterpreter`, `VmRegBlock.{eden_top, old_start, …}`.
- S12: `OopMap` + `PcDesc` emission (D7), `slot_is_oop`, the spill-all
  invariant + its debug assert, `CodeTable::{oops_do, find_by_pc}`,
  `walk-by-fp + TierLink` traversal, `compiled_depth`.
- S13: `PcDesc.bci` (scope-desc seed), `NmState::{NotEntrant, Zombie}`.
- S15: loop-header blocks are identified (`is_loop_header`) — OSR entry hook.

## Out of scope

- Compiled sends of any kind, PICs, adapters, allocation in compiled code,
  runtime stub table beyond `call_stub`/`stub_poll` → S11.
- GC scanning compiled FRAMES, oop-map consumption, weakness, flushing → S12.
- Deopt, scope descs, uncommon traps (bailout-by-restart is NOT deopt and
  dies in S11) → S13. Inlining/customized dispatch beyond the key → S14.
  OSR → S15.
