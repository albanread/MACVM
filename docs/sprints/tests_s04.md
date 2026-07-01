# Sprint S04 — Test Plan

## Acceptance gate

Restated from SPRINTS.md S4, made checkable. All S0–S3 tests stay green
(`just gate-s04` = `cargo test` + clippy; no stress modes exist yet). The
five named golden programs pass with pinned transcripts:

1. **Counter closure** — shared mutable capture through a heap Context.
2. **Nested blocks 3 deep** with ctx-temp access across depths 0/1/2.
3. **NLR through 2 frames** running `ensure:` handlers **in innermost-first
   order** (transcript order is the assertion).
4. **`cannotReturn:` on an escaped block** (home returned; NLR attempted).
5. **Block re-entry after home return** for a non-NLR block (legal, must
   work repeatedly).

Plus ensure-ordering assertions: handler runs exactly once per completion
path, `ifCurtailed:` skipped on normal return, handler results discarded.

## Unit tests

| test name | module | assertion | rationale |
|---|---|---|---|
| `home_ref_roundtrip` | oops::layout | pack/unpack identity across field extremes (proc 0/255, serial 0/u32::MAX, fp 0/(1<<22)-1) | packing correctness |
| `frame_serial_monotonic` | interpreter::frame | 100 pushes → strictly increasing serials; pop+push reuses index with new serial | dead-home foundation |
| `marker_accessors` | interpreter::frame | set/clear marker; kind bit round-trips; serial bits 31:0 preserved across `set_marker` | read-modify-write rule |
| `marker_classification` | interpreter::frame | nil / ClosureOop / ArrayOop marker discriminated by klass | token-vs-handler rule |
| `flags_nctx_captures` | oops | new flag fields pack/unpack; old fields (argc, ntemps, has_ctx, is_block, prim_fails) unaffected | layout extension |
| `context_alloc_method` | interpreter::blocks | `has_ctx` method entry: FP+3 is a Context of `nctx` nil slots, `home_hint` nil | §5.4 entry alloc |
| `context_chain_block` | interpreter::blocks | block with `has_ctx` + `captures_ctx`: new Context's `home_hint` == copied[1] | static chain link |
| `ctxless_block_aliases` | interpreter::blocks | block without own ctx: FP+3 == enclosing ContextOop (identity) | depth-uniformity design |
| `ctx_temp_rw_depth0` | interpreter | `store_ctx_temp_pop 0 i` then `push_ctx_temp 0 i` round-trips | opcode basics |
| `ctx_temp_depth_walk` | interpreter | hand-built M⊃B1(no ctx)⊃B2(ctx)⊃B3: from B3, depth 1 reaches M's slot; depth 0 reaches B2's | hop-counting (not lexical depth) |
| `push_closure_copied_layout` | interpreter::blocks | closure after `push_closure`: copied[0] == home receiver, copied[1] == ctx (when captures_ctx), user captures in push order | pinned convention |
| `push_closure_home_method` | interpreter::blocks | closure made in a method frame: unpacked home == (0, that frame's serial, that fp) | home ref |
| `push_closure_home_propagates` | interpreter::blocks | closure made inside a block: home identical to the enclosing closure's home (method frame, not block frame) | the classic NLR bug |
| `activate_block_frame_shape` | interpreter::blocks | after `value`: FP+0 is the CompiledBlock, receiver slot == copied[0], value-captures in temps[0..], locals nil, sp correct | frame contract |
| `activate_block_argc_mismatch` | runtime::primitives | `value:` on a 2-arg block → PrimResult::Fail (fallback runs) | prim failure |
| `value_family_matrix` | runtime::primitives | prims 50–53 with 0–3 args return `Activated`; non-closure receiver → Fail | prim table |
| `value_with_arguments` | runtime::primitives | 54: Array spread in place (stack depth checked); non-Array → Fail; size ≠ argc → Fail | stack surgery |
| `activated_no_result_push` | interpreter | after an `Activated` primitive, new frame's temp[0] is nil (not a stray result) | 3-arm call site |
| `home_dead_bounds` | interpreter::unwind | HomeRef with fp beyond top → not live (no read past top) | check order |
| `home_dead_serial` | interpreter::unwind | pop home, push new frame at same fp: not live | serial detection |
| `home_dead_process` | interpreter::unwind | forged HomeRef with proc=1 → CannotReturn before any stack access | future-process design |
| `innermost_marked_scan` | interpreter::unwind | 4 frames, markers on #2 and #4 (from bottom): scan from top picks #4 first | ordering primitive |
| `token_fields` | interpreter::unwind | UnwindToken carries packed home + value; cleared after resume | suspension state |
| `sentinel_bci_guard` | bytecode | building a method with bytecode_len ≥ BCI_SENTINEL_BASE panics (debug) | sentinel safety |
| `block_own_ics_counters` | interpreter::blocks | send inside a block uses the block's IC table (home method's ICs untouched); block counter bumps | per-block feedback |
| `blocks_temps_base_constant` | interpreter::frame | S2/S3 frame tests re-run green with FRAME_TEMPS_BASE = 7 | constant discipline |

## Integration/golden tests

`tests/it_blocks.rs`, `tests/it_nlr.rs`; BytecodeBuilder-assembled (source
form arrives S5 and must re-express every one — SPRINTS S5 gate). Transcripts
via `vm.out` buffer; expected text pinned inline. `T print:` below = a
builder method bound to primitive 91. Reuses the S3 harness
(`tests/common/mod.rs`) plus two S4 additions:

- `fn block(vm, home_builder, |b: &mut BytecodeBuilder|) -> u8` — builds a
  CompiledBlock literal (holder patched after the home method finishes) and
  returns its literal index for `push_closure`.
- `fn stack_clean(vm) -> bool` — post-run walker asserting no residual
  frames, markers, or tokens (used by every golden's epilogue and the
  `unwind_token_not_leaked` stress test).

1. **`counter_closure`** (gate 1) — home method: ctx temp `n := 0`; builds
   block `[n := n + 1. n]`; home returns the closure. Driver `value`s it 3
   times **after the home method returned**, printing each result. Expected:
   `1 2 3`. Asserts shared mutable capture via heap Context outliving its
   frame (also covers gate 5 partially).
2. **`nested_blocks_3deep`** (gate 2) — M(ctx: a=10) ⊃ B1 ⊃ B2(ctx: b=20) ⊃
   B3 = `[a + b + c]` with `c` a value-capture (30). `value` chain computes
   60; also B3 *stores* into `a` (depth 1) and M prints `a` afterwards → 99.
   Expected: `60 99`. Exercises depths 0/1, value captures, store-through.
3. **`nlr_two_frames_ensure_order`** (gate 3) —
   `run` = `[[^42] ensure: [T print: 'inner']] ensure: [T print: 'outer']`
   shape built as: method `run` calls method `mid`; `mid` activates a
   protected block (marker A, prints 'inner' in handler); inside it a second
   protected block (marker B, prints 'B' — wait, keep it exactly two marked
   frames): NLR from the innermost block whose home is `run`. Expected
   transcript: `inner outer done-42` where `done-42` is printed by `run`'s
   caller with the NLR value. Assertion is strict output ordering
   (innermost-first) and value delivery.
4. **`cannot_return_escaped`** (gate 4) — home method returns a `[^1]`
   closure; driver installs `BlockClosure>>cannotReturn:` (builder method)
   printing `cannotReturn <value>` and then quitting; `value` the closure.
   Expected: `cannotReturn 1`, clean exit. A variant *without* the handler
   expects the S3 DNU fallback trace and exit code 1.
5. **`block_reentry_after_home_return`** (gate 5) — closure `[x * 2]`
   (value-capture x=21) whose home has returned; `value` it 3 times.
   Expected: `42 42 42`. No NLR, no error — legality assertion.
6. **`ensure_normal_completion`** — `[T print: 'body'. 7] ensure:
   [T print: 'cleanup']` returning to a printer. Expected: `body cleanup 7`
   — handler runs *after* body, *before* value delivery; value is the
   body's 7 (handler result discarded — handler returns 99 to prove it).
7. **`if_curtailed_both_paths`** — normal completion: handler NOT run
   (transcript lacks the marker line); NLR path: handler runs. Two programs,
   one assertion each.
8. **`nlr_from_handler_abandons`** — outer NLR (value 1, home `run`) unwinds
   into an `ensure:` handler that itself does `^2` to *its* home (below the
   marked frame). Expected: `run`'s caller sees **2**, and a probe print
   after the original NLR target confirms the value-1 return never
   completed. Token-abandonment rule.
9. **`dnu_in_handler`** — handler body sends an unknown selector.
   (a) with user `doesNotUnderstand:` returning nil: unwind completes,
   NLR value delivered, transcript shows handler-DNU-resume order.
   (b) without: S3 fallback trace, exit 1, no double fault.
10. **`nested_ensure_single_marked_frame_chain`** — three nested `ensure:`s,
    NLR from innermost. Expected handler order: `e1 e2 e3` (innermost →
    outermost), each exactly once.
11. **`must_be_boolean`** — `br_true_fwd` on smi 5 with user
    `SmallInteger>>mustBeBoolean` (builder) returning `true`. Expected: the
    true-branch executes (branch re-run with handler result). Variant
    without handler: DNU fallback, exit 1.
12. **`ensure_around_home_of_nlr`** — the *home* frame's own return path
    runs through `do_return`: home method's body is a protected block
    activation... structure: `mid` = `[inner-NLR-block value] ensure:
    [T print: 'e']`; NLR passes through and `e` prints exactly once (via
    the home-return interception, not the unwind scan). Guards the
    do_return-choke-point design.

## In-language tests

None — `world/tests/*.mst` needs S5+S6. Every golden above is re-expressed in
source as part of the S5 gate, and S6's SUnit-lite adds assertion-style
coverage (`assert:equals:` over closure results).

## Stress/negative tests

| test | provocation | expected |
|---|---|---|
| `deep_block_nesting_16` | 16 lexical block levels, ctx access to depth 5 | correct values; no depth-counting drift |
| `nlr_depth_64` | NLR unwinding 64 frames, markers on every 8th | all 8 handlers, innermost-first, then home return |
| `handler_runs_once_per_path` | counters in every handler across all goldens | each handler count == 1 per completion |
| `reentrant_ensure_closure` | ONE handler closure used as marker on two frames simultaneously | runs twice (once per frame), no marker cross-talk |
| `escaped_block_value_loop` | dead-home closure `value`-d 10_000 times | no error, bounded stack, serials never confuse it |
| `serial_reuse_probe` | craft: pop home, push replacement frame with identical shape at same fp, NLR | CannotReturn (serial mismatch), never a wrong-frame return |
| `nlr_to_bottom_frame` | block whose home is the entry frame | interpretation ends; NLR value is the program result |
| `value_on_non_closure` | `value` sent to smi/Array (prim 50 on a forged method) | Fail → fallback bytecode runs |
| `unwind_token_not_leaked` | after any golden completes | walk stack: no frame retains a token/marker (heap verify hook) |
| `must_be_boolean_livelock_doc` | handler returning smi forever | documented livelock — test bounds it with `MACVM_TEST_STEP_LIMIT` hook and asserts the loop is the mustBeBoolean retry, not a VM hang elsewhere |

## Non-goals

- No GC/stress coverage: Contexts, closures, tokens under `MACVM_GC_STRESS`
  are S7's gate (this sprint's alloc-ordering discipline is asserted only by
  code review + the S7 gate later).
- No source-syntax blocks, capture analysis, or depth computation — S5
  (which must regenerate every golden here from `.mst` source).
- No exception classes / `on:do:` — S18.
- No performance assertions (block-heavy benchmarks recorded from S6 on).
- No compiled-frame unwinding or deopt interaction — S12/S13 re-run this
  entire file under `MACVM_JIT=threshold=1` as *their* gate.
- No green-process NLR-across-process runtime behavior — the proc-id check
  exists, S17 tests it for real.
