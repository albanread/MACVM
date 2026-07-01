# Sprint S2 — Test Plan

## Acceptance gate

Restated from SPRINTS.md S2, made checkable:

1. `just gate-s02` green (fmt, clippy `-D warnings`, all tests incl. S0/S1).
2. **Golden disassembly**: builder-built methods disassemble to checked-in
   `tests/golden/*.bc.expected` files, byte-for-byte (`UPDATE_GOLDEN=1`
   regenerates; the first three goldens are hand-written, never
   regenerated — see Pitfalls in the detail doc).
3. **Execution kernels**: arithmetic-free stack/temp/jump kernels run via
   `run_method` and return the exact expected oop AND leave the process
   stack exactly as found (`sp`/`fp` restored, no residue).
4. **Stack discipline asserts**: frame teardown leaves the caller's stack
   exact; every frame slot is a valid oop at every step (checked by a
   debug-mode stack verifier run after each test kernel).

## Unit tests

| Test name | Module | Assertion | Rationale |
|---|---|---|---|
| `opcode_values_pinned` | bytecode::opcode | `OP_PUSH_SELF==0x00 … OP_NLR_TOS==0x43` — every constant against its SPEC §4.1/§4.2 literal | wire format is external contract |
| `decode_roundtrip_all` | bytecode::opcode | for each opcode: hand-pack bytes into a method (via `alloc_method` + raw byte writes), `decode_at`, assert `Instr` fields and `next_bci` (1, 2, or 3 bytes as per operand table) | decode agrees with the operand-width table |
| `decode_le_operands` | bytecode::opcode | `jump_fwd` with distance 0x0102 encodes bytes `30 02 01`; decode returns 0x0102 | little-endian pinned |
| `decode_i8_sign` | bytecode::opcode | `push_smi_i8` operands 0xFF→−1, 0x80→−128, 0x7F→127 | sign-extension |
| `alloc_method_shape` | memory::alloc | `alloc_method(vm, 5)`: size slot == smi 5; consumed words == 2+8+1 (header + 7 named + size + ceil(5/8)); named fields nil; flags/prim/counters smi 0; mark tagged_contents == false | Method format layout (SPEC §4.4) incl. byte-tail padding |
| `method_flags_pack` | bytecode::method | set flags smi for argc=15, ntemps=255, has_ctx, is_block, prim_fails; each getter isolates its field | 4/8/1/1/1 packing |
| `builder_dedup_literals` | bytecode::builder | pushing the same Symbol literal twice yields one literal-frame entry, two identical operand bytes | literal frame hygiene (S5 relies on it) |
| `builder_wide_literal` | bytecode::builder | force 300 literals: entry 299 emits `push_literal_w` (0x12) with u16 operand; entry 5 emits 0x05 | auto-widening threshold at index > 255 |
| `builder_forward_patch` | bytecode::builder | `jump_fwd` to a label bound 7 bytes later: operand == 7 − 3… concretely: emit jump at bci 0, bind after 2 more 2-byte instrs (target bci 7): operand == 4 (7 − next_bci 3) | jump base = next_bci, the pinned convention |
| `builder_backward_distance` | bytecode::builder | bind label at bci 0, 2-byte instr, `jump_back` at bci 2: operand == 5 − 0 = 5 | backward arithmetic incl. own width |
| `builder_unbound_label_panics` | bytecode::builder | `finish` with an unbound label → panic (builder misuse = VM bug) | fail fast at build time, not run time |
| `builder_requires_return` | bytecode::builder | `finish` on code not ending in 0x40/0x41 → panic | "fell off end" is unrepresentable |
| `builder_argc_limit` | bytecode::builder | `finish(argc=16)` panics (4-bit field) | flags overflow guard |
| `stack_push_pop` | interpreter::stack | push 3 oops, pop 3, sp restored; overflow at capacity exits — tested via the checked `try_push` internal returning Err (unit-testable without a subprocess) | stack mechanics |
| `frame_slot_table` | interpreter::stack | build a frame by hand for (argc=2, ntemps=2): receiver at fp−3, arg1 at fp−1, temp index 2 at fp+5, temp index 3 at fp+6; `Frame::temp` resolves all four unified indexes | THE slot-offset table from the detail doc, verified literally |
| `activate_then_return` | interpreter | push receiver+2 args, `activate_method`, immediately `return_from_frame(smi 9)`: returns Some for entry frame; stack sp == pre-push sp; no residue above sp | teardown exactness without executing bytecode |
| `activate_inits_slots` | interpreter | after activate: `fp+1` smi(−1) sentinel, `fp+2` smi(0), `fp+3` nil, `fp+4` == receiver, temps nil | frame init contract (S13 rematerialization depends on it) |
| `dispatch_no_alloc` | interpreter | record `eden.top`, run the largest kernel, assert `eden.top` unchanged | S2 loop allocates nothing (pre-Handle safety) |
| `disasm_annotates_jumps` | bytecode::disasm | the k_bool_loop method: `jump_back` line reads `11: jump_back 11 -> 3`; `br_false_fwd` line ends `-> 14` | jump annotation arithmetic uses the same base convention |
| `disasm_header_line` | bytecode::disasm | method with argc=1 ntemps=2 prim=0: first line == `method #foo argc=1 ntemps=2 prim=0 flags=` exactly | pinned golden format, field order |
| `receiver_copies_agree` | interpreter | after activate with argc=2: `slots[fp+4] == slots[fp-3]` (raw word equality) | the duplicated receiver slots must be born identical (S4/S13 read the canonical one) |
| `entry_frame_sentinel` | interpreter::stack | entry frame's `fp+1` == `SmallInt::new(-1).oop()`; a nested fake frame's saved_fp round-trips through smi encode/decode | smi-encoded frame links (exact-scan invariant) |
| `ics_empty_but_present` | bytecode::builder | S2 `finish` produces `ics()` = Array of length 0 (not nil) | S3 indexes this array; nil would be a latent NPE-alike |

Debug-only (`#[cfg(debug_assertions)]`) panic tests: `dup` on empty operand
stack, `push_temp 5` with argc+ntemps=4, undefined opcode 0x50, executing
`OP_SEND` (unimplemented), bci past end (builder bypassed via raw byte
patching).

Test scaffolding note: kernels need a `VmState` per test — provide
`fn test_vm() -> VmState` in a shared `tests/common/mod.rs` using a direct
`VmOptions { heap_mib: 64, .. }` constructor, never `std::env::set_var`
(the test runner is multi-threaded; env mutation races). The same helper is
reused by S3+.

## Integration/golden tests

Golden runner: `tests/it_golden.rs` walks `tests/golden/` (CONVENTIONS §5);
S2 adds `.bc.expected` handling: each case is a Rust `#[test]` that builds a
method with `BytecodeBuilder`, disassembles, compares to the file.

| Golden | Method built | Checks |
|---|---|---|
| `bc_minimal.bc.expected` | `return_self` only (1 byte) | header line, empty literals, `ics: 0` — hand-written golden |
| `bc_straightline.bc.expected` | push_smi 5; store_temp_pop 0; push_temp 0; dup; pop; return_tos | operand rendering, temp indexes — hand-written |
| `bc_jumps.bc.expected` | if/else diamond: push_true; br_false_fwd L1; push_smi 1; jump_fwd L2; L1: push_smi 2; L2: return_tos | forward patching + `->` targets — hand-written |
| `bc_loop.bc.expected` | countdown loop (temp; jump_back) | backward distance |
| `bc_literals.bc.expected` | push_literal #foo; push_literal 'bar'; push_global (Association); return_tos | literal frame section incl. print_oop rendering of Symbol/String/Association |
| `bc_wide.bc.expected` | 260 distinct smi literals then push_literal_w | 0x12 rendering |

Execution kernels in `tests/it_interp.rs` — each runs `run_method` on a
fresh `VmState`, asserts the result oop, then asserts `vm.stack.sp == 0`:

1. **`k_return_self`** — receiver smi 7, method `return_self` → result smi 7.
2. **`k_push_const_matrix`** — one kernel per push_nil/true/false/smi(−128,
   127)/literal(Symbol) returning it → identity vs universe oops.
3. **`k_temps`** — argc=2 ntemps=1: `t2 := arg0; ^t2` shaped bytecode; run
   with (smi 3, smi 4) → 3. Repeat with argc/ntemps matrix {0,1,2}×{0,1,2}
   (generated in a loop, one method each) returning each arg and each temp
   (temps return nil) — this is the unified-index gate.
4. **`k_instvar`** — receiver = fresh Association; store_instvar_pop 1 of
   pushed literal; push_instvar 1; return → the literal; ALSO assert via
   `body_oop(1)` the write really landed in the object.
5. **`k_global`** — Association literal with value smi 5; `push_global;
   return_tos` → 5; then a store variant: `push_smi 9; store_global_pop;
   push_global; return` → 9.
6. **`k_diamond`** — the bc_jumps method with receiver true → 1; rebuilt
   with push_false → 2.
7. **`k_bool_loop`** — a back-jump loop without sends or arithmetic
   (there is no way to decrement a counter in S2, so the loop condition is
   a boolean temp flipped in the body). Pinned kernel (argc=0, ntemps=1):

   ```
    0: push_true
    1: store_temp_pop 0        ; t0 := true
    3: push_temp 0             ; L: loop head
    5: br_false_fwd -> 14      ; distance 6; exits on the 2nd arrival
    8: push_false
    9: store_temp_pop 0        ; t0 := false
   11: jump_back -> 3          ; distance 11
   14: push_smi 42
   16: return_tos
   ```

   Executes exactly: one not-taken `br_false_fwd`, one taken `jump_back`,
   one taken `br_false_fwd`, then terminates → smi 42. This covers every
   S2 control-flow requirement (taken/not-taken branch, executed back
   jump, loop re-entry reading a mutated temp). Assert result == 42 AND
   (via `MACVM_TRACE=bytecode` capture or an instruction counter exposed
   under `cfg(test)`) that exactly 12 instructions executed — pinning both
   the path and the offsets.
8. **`k_deep_operand_stack`** — push 40 smis, pop 39, return_tos → last
   remaining; exercises dup/pop bounds and operand area growth within one
   frame.
9. **`k_stack_restored_after_result`** — call `run_method` twice on the same
   `VmState`; second run unaffected by first (sp truly 0, no residue oops
   consulted).

## In-language tests

None. `world/tests/` starts at S6. (S2 kernels are the Rust-side precursor;
each kernel above gets re-expressed in `.mst` source when S5 lands, per
SPRINTS S5's gate.)

## Stress/negative tests

| Scenario | How provoked | Expected |
|---|---|---|
| Send opcode reached | method bytes patched to contain 0x20 | `unimplemented!` panic naming S3 (debug test) |
| Closure opcode reached | bytes 0x11 | `unimplemented!` panic naming S4 |
| Non-boolean branch | `push_smi 0; br_true_fwd …` | panic naming `mustBeBoolean`/S3 (debug test; becomes a send test in S3) |
| Undefined opcode | byte 0x7F | panic `undefined opcode` |
| Process-stack overflow | tiny test stack (test-only capacity ctor, 64 slots) + argc-0 method with 100 pushes | clean overflow error path (Err/exit-70 contract), not a Rust index panic |
| Operand-stack underflow | `pop` as first instruction | debug_assert panic (frame-slot protection) |
| Trace mode smoke | `MACVM_TRACE=bytecode` (options constructed directly, not env) over k_diamond | one line per executed bytecode on stderr; test asserts line count == executed count |

## Non-goals

- Send/return interplay across ≥2 real frames — S3 (S2's activate/return
  discipline is tested single-frame; multi-frame call matrices are S3's
  gate). The `activate_then_return` unit test intentionally fakes a caller.
- IC table contents/transitions — S3 (S2 only asserts `ics: 0` empty array
  exists).
- `jump_back` counter/poll semantics — S3 (counters) and S7 (GC poll);
  S2 asserts only that the `pending` hook is reached-and-harmless.
- Golden stdout transcripts (`.expected`) — S5, when programs can print.
- Bytecode verification of arbitrary/hostile input — bytecode is
  VM-generated (SPEC §4); malformed bytecode is a VM bug ⇒ debug_asserts,
  not a verifier. Revisit only if methods ever cross a trust boundary
  (snapshot load, S16).
- Interpreter throughput — measured and recorded in S6, gated never
  (standing rule 3).
