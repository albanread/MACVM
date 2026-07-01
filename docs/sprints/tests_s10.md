# Sprint S10 — Test Plan

## Acceptance gate

Restated from SPRINTS.md S10, made checkable:

1. **Differential**: the full in-language suite (`world/tests/*.mst`) passes
   with `MACVM_JIT=threshold=1` (every eligible method compiles on first
   send) AND with `MACVM_JIT=off`, and the two stdout transcripts are
   byte-identical (SPEC §12.5).
2. **Listing goldens**: 3 reference methods produce checked-in
   `.lst.expected` listings (see below) — the "disasm golden" (no
   disassembler exists; `CodeBlob.listing` is the artifact, S10 P10).
3. **Perf marker**: a send-free arithmetic kernel runs ≥ 5× interpreter
   speed compiled. Per standing rule 3, the number is *recorded* in
   `docs/PERF.md`; the gate script WARNS below 5× and fails only below 2×
   (an architectural-mistake tripwire, not a perf gate).
4. **Mixed traces**: a stack trace taken while a compiled frame is active
   prints interleaved compiled/interpreted frames correctly.
5. All prior gates (S0–S9 tests, `MACVM_GC_STRESS=1` and `=full` on the
   interpreter suite) still green — note stress modes run with the DEFAULT
   `MACVM_JIT` (off) in S10; jit+stress combined is S12's flagship.

`just gate-s10`, in order:

```sh
cargo test                                             # unit + integration + goldens
MACVM_JIT=off              just run-world-tests  > /tmp/s10_off.txt
MACVM_JIT=threshold=1      just run-world-tests  > /tmp/s10_t1.txt
diff /tmp/s10_off.txt /tmp/s10_t1.txt                  # byte-identical or fail
MACVM_GC_STRESS=1          just run-world-tests       # JIT default (off) — regression
MACVM_GC_STRESS=full:64    just run-world-tests
just bench-s10                                         # perf kernel → docs/PERF.md
cargo clippy -- -D warnings
```

The perf kernel (`world/bench/arith.mst`): `sumTo: 5_000_000` timed via
`millisecondClock` under `MACVM_JIT=off` vs `threshold=1` after one warm-up
call; the bench script appends `date, commit, interp_ms, jit_ms, ratio` to
`docs/PERF.md` and applies the warn/fail thresholds from gate item 3.

## Unit tests

| Test name | Module | Assertion | Rationale |
|---|---|---|---|
| `leaders_if_else` | `compiler::decode` | builder method with `br_false_fwd`/`jump_fwd` splits into 4 blocks with correct edges | CFG discovery |
| `leaders_while_loop` | `compiler::decode` | `jump_back` marks target `is_loop_header`; `Poll` inserted at source | loop plumbing |
| `unreachable_after_return` | `compiler::decode` | code after `return_tos` before a leader is dropped (or empty block) — no panic | frontend emits this for trailing `^` |
| `stack_sim_depths_agree` | `compiler::ir` | ifTrue:ifFalse: value pattern → join block entry_depth 1, merge vreg created, both preds end in `Move` | D3.2 merge rule |
| `single_pred_inherits_stack` | `compiler::ir` | straight-line split blocks share exit/entry vreg vectors (no Moves) | no gratuitous copies |
| `temp_slots_single_vreg` | `compiler::ir` | store_temp then push_temp in a later block reference the same vreg | SSA-lite temp rule |
| `smi_send_inlined_mono` | `compiler::ir` | send site with mono smi IC + prim `+` becomes `SmiArith{Add, fail:bailout}` | D1 item 2 |
| `smi_cmp_fused_with_branch` | `compiler::ir` | `<` immediately consumed by `br_false_fwd` → single `SmiCmpBr` | fusion peephole |
| `eligibility_rejects_each` | `compiler::driver` | one test per rejection cause: closure op, ctx, instvar store, poly IC, empty IC, argc 6, primitive method | D1 completeness; each sets `compile_disabled` |
| `intervals_basic` | `compiler::regalloc` | hand IR: def at 2, uses at 5 and 9 → interval [2,9] | liveness math |
| `interval_multi_def_union` | `compiler::regalloc` | temp vreg defined in 2 blocks → single covering interval | SSA-lite conservatism |
| `spill_all_crossing_safepoint` | `compiler::regalloc` | synthetic IR containing a `CallRuntime`: every oop interval live across it gets `Spill`, none `Reg` | THE S12 invariant, enforced early |
| `spill_slot_oopness_recorded` | `compiler::regalloc` | `slot_is_oop` bit set for oop vregs, clear otherwise | oop-map raw material |
| `furthest_end_spilled_under_pressure` | `compiler::regalloc` | 17 overlapping call-free intervals → exactly one spilled, the furthest-ending | linear-scan core |
| `codetable_install_get_find` | `codecache::nmethod` | install → get by id, lookup by key, find_by_pc inside code range and at last byte | table basics |
| `codetable_rehash_after_move` | `codecache::nmethod` | mutate key oop bits (simulated move), rehash, lookup by new key works, old key misses | P7 |
| `bailout_sentinel_not_oop` | `interpreter::compiled_call` | `0b10` fails every typed-wrapper `try_from` | P1 |
| `pcdesc_block_starts` | `compiler::emit` | pcdescs sorted by pc_off, bcis match block bcis | trace/D7 plumbing |
| `emit_smi_mul_overflow_seq` | `compiler::emit` | Mul listing contains asr/mul/smulh/cmp-asr63 exactly (P5) | `b.vs` does not exist for mul |
| `emit_ldur_vs_untag_split` | `compiler::emit` | instvar index 5 → `ldur …, #47`; index 40 → `sub x16,…,#1; ldr …, #336` | P4 boundary at ±255 |
| `poll_stub_preserves_x0_x15` | `codecache::stubs` | stub listing saves/restores exactly the allocatable set | S10 P6 |
| `vmregblock_offsets_pinned` | `oops::layout` | `offset_of!` each VmRegBlock field == layout const | emit hard-codes these |
| `jit_flag_parsing` | `runtime` | `off`, `threshold=1`, `threshold=10000`, absent → correct `JitMode`; garbage → default + warning | CONVENTIONS §3 |

## Integration/golden tests

`tests/it_tier1.rs` + goldens under `tests/golden/`.

1. **Listing goldens (gate item 2).** Three reference methods, compiled via
   the real driver from `.mst` source, listings written to
   `tests/golden/s10_<name>.lst.expected` (`UPDATE_GOLDEN=1` regenerates):
   - `sumTo:` — `| s | s := 0. 1 to: n do: [:i | s := s + i]. ^s`
     (loop, Poll, SmiArith Add, SmiCmpBr).
   - `absDiff:` — nested ifTrue:ifFalse: with a value merge (BoolBr/merge
     vregs, SmiArith Sub).
   - `bitsOf:` — bitAnd:/bitOr:/bitXor: chain + literals (ConstSmi widths,
     ConstPool).
   Golden compares the full listing INCLUDING frame prologue — layout changes
   must be conscious.
2. **`compiled_result_equals_interpreted`** — for ~10 arithmetic methods
   (add/sub/mul overflow edges at ±2^61 boundaries, comparison chains,
   temp shuffles): run under `off` and `threshold=1`, assert identical
   result oops (smi equality) — the micro-differential harness.
3. **`bailout_falls_back_correctly`** — method compiled with smi-mono IC,
   then called with a Double receiver (guard klass mismatch → IC miss path,
   NOT compiled entry) AND with smi receiver but overflowing operands
   (compiled entry → BAILOUT → interpreter → LargeInteger result). Assert the
   LargeInteger value is correct and a subsequent smi call still uses the
   nmethod (id still installed, `MACVM_TRACE=jit` log shows no recompile).
4. **`trigger_and_ic_rewrite`** — threshold=3: call a method 3 times, assert
   IC target became a smi id on the 3rd, `code_table.get` alive, 4th call
   does not re-enter `compile_method` (trace-log scrape).
5. **`stale_id_self_heals`** — install nmethod, dispatch once, then
   `code_table` slot forcibly cleared (test hook): next send re-looks-up,
   reinstalls a CompiledMethod target, executes correctly.
6. **`mixed_trace_golden` (gate item 4)** — interpreted `a` calls compiled
   `b`; a test primitive `printStackTrace` invoked from... `b` cannot call
   (send-free) — instead: take the trace from `rt_poll` via a test flag set
   before entering `b` (poll fires on `b`'s loop back-edge). Golden:
   `b (compiled)` above `a (interpreted)` with selectors + bcis.
7. **`gc_moves_nmethod_pool_literal`** — compile a method whose literal frame
   holds a fresh (eden) Array literal; force scavenge from the INTERPRETER
   (no compiled frame active); re-run compiled method; assert it still
   pushes the (moved) array and `oops_do` visited the pool word (counter
   hook). Covers D8 now rather than waiting for S12.
8. **`run_ir_raw`** (implementation-order step 5's artifact, kept) —
   hand-construct an `IrMethod` (no bytecode, no interpreter): two blocks,
   one SmiArith, one SmiCmpBr; run through regalloc + emit + publish + call
   stub with a stack-allocated fake `VmState` register block. Proves the
   back half of the pipeline in isolation — the first test to write, the
   first to consult when the full pipeline misbehaves.
9. **`compiled_frame_teardown_exact`** — debug build: record native sp
   before `enter_compiled` and after; assert equality for normal return AND
   for bailout (the epilogue path is shared — verify it stays that way).

## In-language tests

`world/tests/tier1.mst` (runs under both jit modes in the gate):

- Arithmetic kernel assertions: `sumTo: 1000 = 500500`, overflow boundary
  `(2 raisedTo: 61) - 1 + 1` falls back to LargeInteger correctly,
  comparison ladders, bit-op identities (`x bitXor: x = 0` etc.).
- A `whileTrue:` countdown and a `to:do:` accumulation (loop poll exercised
  ≥ 10k iterations so the trigger fires mid-suite at default threshold too).
- Re-runs of the S6 arithmetic sections verbatim (they must not notice tier 1
  exists).

## Stress/negative tests

- **Suite at `threshold=1`** — the sprint's torture mode: everything
  eligible compiles before first execution; bailout paths and IC-rewrite
  races (single-threaded, but ordering bugs) surface here.
- **`threshold=1` + tiny code cache** (`test hook: 64 KiB`) — cache exhausts
  mid-suite; compilation stops gracefully (log line, no panic), suite still
  passes interpreted.
- **`compile_disabled` churn** — ineligible hot method called 100k times:
  assert exactly one eligibility scan (trace scrape), counters stay reset.
- **Debug-build frame asserts** — compiled entry/exit under
  `debug_assertions`: process-stack sp unchanged by a compiled call except
  the argc+1→1 replacement (stack-discipline assert from S2 extended to
  `enter_compiled`).

## Non-goals

- No compiled→compiled or compiled→interpreter calls, no DNU from compiled,
  no allocation in compiled code (all → `tests_s11.md`).
- No GC-while-compiled-frames-active coverage: S10 code cannot trigger GC
  under a compiled frame BY CONSTRUCTION (bailout-by-restart, S10 D1); the
  moving-GC integration tests land in `tests_s12.md`.
- No deopt-stress (`MACVM_DEOPT_STRESS`) — S13.
- No perf gating beyond the 2× tripwire; targets tracked in `docs/PERF.md`
  (standing rule 3; S15 hardens).
