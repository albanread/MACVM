# Sprint S13 — Test Plan

## Acceptance gate

Restated from SPRINTS.md S13, made checkable:

1. Entire in-language suite (`world/tests/`) green under
   `MACVM_DEOPT_STRESS=1` (every trap-eligible guard fires once via stress
   instrumentation; one nmethod invalidated per 1,000 compiled calls), with
   `MACVM_JIT=threshold=1`.
2. The same, additionally combined with `MACVM_GC_STRESS=1` (standing rule 1:
   all existing stress modes stay green).
3. Unit: LEB128 + scope-desc pack/decode round-trips; PcDesc binary-search
   edges.
4. Integration: materialized-frame-equivalence differential test passes;
   mid-loop-redefinition test passes with the pinned old-code semantics.
5. `just gate-s13` runs 1–4.

## Unit tests

| Test name | Module | Assertion | Rationale |
|---|---|---|---|
| `leb_roundtrip_uleb` | compiler/scopes | write/read ULEB for 0, 1, 127, 128, 16383, 16384, u32::MAX, u64::MAX | 7-bit boundary edges are where LEB bugs live |
| `leb_roundtrip_sleb` | compiler/scopes | SLEB for 0, ±1, ±63, ±64, ±8191, ±8192, i64::MIN/MAX | sign-extension boundary edges |
| `valueloc_roundtrip` | compiler/scopes | each `ValueLoc` variant packs/unpacks identically, incl. negative `FrameSlot` and negative `ConstSmi` | tag byte + payload encoding pinned |
| `scope_pack_depth1` | compiler/scopes | recorder with 1 scope, 3 sites → pack → decode: fields equal input | S13's real shape |
| `scope_pack_depth3_forward_compat` | compiler/scopes | hand-built 3-deep chain with `SenderLink` + pending stacks + `CtxLoc::Elided` round-trips | the S14 format must work NOW, before S14 exists |
| `scope_dedup` | compiler/scopes | 2 sites in one scope share one packed scope record (blob size checked) | dedup is part of the format contract |
| `pcdesc_search_exact` | compiler/scopes | `find` hits first, middle, last of a 7-entry table; misses (off±4) return None | exact-match convention; binary-search edges |
| `pcdesc_sorted_invariant` | compiler/scopes | `pack` output offsets strictly increasing; unsorted recorder input still packs sorted | recorder must sort, not trust emit order |
| `brk_imm_decode` | codecache/deopt_trap | encoder for `brk #0xDE00..02` matches the handler's decode mask; `brk #1` (Rust abort) is rejected | the foreign-trap namespace check |
| `handler_redirect_smoke` | codecache/deopt_trap | emit a stub that `brk #0xDE01`s; a test-mode trampoline records (pc, x16) and returns; assert pc stashed correctly | signal → ucontext rewrite → trampoline path in isolation |
| `deps_record_lookup` | runtime/deps | record 3 nmethods over 2 (klass, sel) keys; `affected_by_install` returns exactly the right ids | multimap basics |
| `deps_subclass_rule` | runtime/deps | dep on (C, #foo), install #foo in B where C ⊂ B ⊂ A: affected; install in sibling D: not affected; install #bar in B: not affected | D2's super-chain walk |
| `deps_rebuild_after_gc` | runtime/deps | `invalidate_cache`, force full GC (klasses move), query again → same NmethodIds | cache is oop-keyed; rebuild from pool-index deps is the correctness story |
| `pending_deopt_map` | runtime/deopt | insert/remove keyed by fp; double-fire returns None the second time | return trampoline must consume its entry exactly once |
| `stack_height_model` | runtime/deopt | `interpreter_model_height` agrees with the compiler decode stage's abstract stack on a corpus of 10 methods at every bci | M4's checker must not diverge from the emitter's model |

## Integration/golden tests

### T1. Materialized-frame-equivalence differential test (the flagship)

**Design.** Execute both paths and compare the frame the interpreter builds
against the frame deopt materializes, at the same bci, slot by slot.

- New debug-only primitive `__frameSnapshot` (system group, dev hook): returns
  an Array `{method selector. bci. receiver. temps asArray. operandStack
  asArray. context isNil}` describing the *caller's* frame (the frame that
  invoked the primitive), built by walking the ProcessStack frame under the
  interpreter's own layout accessors.
- New debug-only primitive `__forceDeopt`: interpreted → no-op returning nil;
  in compiled code the compiler treats a send of `__forceDeopt` as an
  unconditional uncommon-trap site (`brk #0xDE00`, `reexecute=false` at the
  send, result nil).
- Test method (in `tests/golden/deopt_equiv.mst`): builds nontrivial state —
  3 temps holding a smi, a String, and an Array; two values live on the
  operand stack across the probe (achieved with nested expression
  `a + (self probe: b)`), a `has_ctx` variant with a block capturing a temp —
  then calls `__forceDeopt` followed by `__frameSnapshot`, prints both.
- **Run A** `MACVM_JIT=off`: snapshot printed comes from a pure interpreter
  frame. **Run B** `MACVM_JIT=threshold=1`: the method compiles, traps at
  `__forceDeopt`, materializes, and the *materialized* frame answers
  `__frameSnapshot`. The golden `.expected` transcript is shared: **byte-equal
  output is the pass condition.**
- Additionally, debug builds run `Frame::verify` inside materialization (M7),
  so run B also proves verifier acceptance, and the M4 height assert proves
  stack-height equality — three independent checks on one test.
- Variants: probe at `reexecute=true` (smi-overflow trap: compute
  `SmallInteger maxVal + 1` mid-expression and check the Large result) and at
  `reexecute=false` (`__forceDeopt` under a live operand stack).

### T2. Mid-loop redefinition test

`tests/golden/deopt_redefine.mst`:

- Class `R` with `step: i [ ^i * 2 ]` and
  `runTo: n [ | s | s := 0. 1 to: n do: [:i |
      i = 500 ifTrue: [ R recompileStepAsTriple ]. s := s + (self step: i) ].
      ^s ]`
  where `recompileStepAsTriple` uses the `sourceCompile:` dev hook (SPEC §10)
  to install `step: i [ ^i * 3 ]`.
- Under `threshold=1`, `runTo:` and `step:` are compiled; `runTo:`'s IC holds
  `step:`'s nmethod. Installing the new `step:` fires D1: `step:`'s nmethod
  goes NotEntrant; `runTo:`'s frames are checked for redirected returns; the
  loop-poll flag drains any fully-compiled loop frames.
- **Pinned semantics asserted by the golden transcript**: iterations 1–500 use
  ×2; iterations 501–n use ×3 (new sends see the new method after the
  lookup-cache flush); the loop *completes* and prints the exact expected sum.
  Run the same file under `MACVM_JIT=off` — identical transcript (differential
  form of the test).
- Second scenario in the same file: the redefined method is the one *currently
  on stack* (`runTo:` redefines itself via a helper): assert the running
  activation completes under **old** code (old MethodOop in the scope desc)
  and only the *next* call of `runTo:` behaves differently.

### T3. Return-path deopt

Compiled `A>>outer` calls interpreted-forced `B>>slow` (kept interpreted with
a `<primitive:>`-free long body under a raised threshold for it — or by
invalidating `outer` while `slow` is on stack via the stress hook): while
`slow` runs, invalidate `outer`; on `slow`'s return the redirected LR fires
`deopt_return_trampoline`; assert `outer` finishes interpreted with the
correct result (golden transcript equal to `MACVM_JIT=off`).

### T4. Organic trap clients

Golden programs exercising the two S13 trap emitters end-to-end under
`threshold=1`: (a) smi `+` overflow inside a compiled loop → LargeInteger
result correct; (b) non-boolean receiver of compiled `ifTrue:` →
`mustBeBoolean` DNU-style error transcript identical to interpreter mode.

### T5. Zombie sweep

Rust integration test: compile a method, invalidate it, ensure no frames
reference it, run full GC twice → assert state Zombie then freed (code-cache
free-list byte count recovers); `CodeTable` lookup misses; dependency index no
longer returns it.

## In-language tests

`world/tests/deopt.mst` (runs under all modes; assertions must hold in pure
interpreter mode too — they test *semantics*, the stress modes test the
mechanism):

- `testOverflowMidLoop` — sum crossing smi max inside `to:do:`; assert exact
  Large result.
- `testRedefineSemantics` — T2's semantics as SUnit-lite assertions (old
  activation finishes old, next call sees new).
- `testMustBeBooleanFromHotLoop` — provoke after 20k iterations; assert error
  message text.
- `testEnsureAcrossDeopt` — `ensure:` block runs exactly once when its frame
  deoptimizes (trap between mark and return); ordering asserted via a side
  buffer.

## Stress/negative tests

- **Full suite × stress matrix** (the gate): `{DEOPT_STRESS=1} × {JIT
  threshold=1} × {GC_STRESS off, 1}` — 2 configurations beyond the normal
  runs, transcripts diffed against `MACVM_JIT=off` baselines (SPEC §12.5–6).
- `stress_every_site_fired`: after a stress run, assert every nmethod's stress
  bitmap is all-ones (no trap site was left uncovered) — Rust-side check via a
  test hook.
- `foreign_brk_fatal`: a test binary installs the handler then executes
  `brk #1`; assert the process dies with SIGTRAP default disposition (spawned
  subprocess test) — the namespace check must not swallow Rust aborts.
- `invalidate_under_gc_stress`: T2 under `MACVM_GC_STRESS=1` — materialization
  allocates Contexts with a scavenge on every allocation; handle-discipline
  violations in M6 die here deterministically.
- `deopt_recursion_bound`: a method that traps, resumes interpreted, is
  re-invoked and traps again, 100× in a loop — asserts nested `interpret_active`
  depth stays 1 per activation (no unbounded Rust-stack growth; counter
  exposed via `__vmStats` test hook).
- Negative: corrupt a packed scope blob in a Rust test (truncate mid-LEB) —
  decoder returns a decode error / panics with nmethod id in debug, never
  reads out of bounds.

## Non-goals

- Depth-N inlined-chain materialization with real inlined frames — format is
  round-trip-tested here (unit `scope_pack_depth3_forward_compat`), behavior
  lands with S14's tests.
- `CtxLoc::Elided` end-to-end (allocation-on-deopt of elided contexts) —
  exercised for real in `tests_s14.md` (Context-elision suite); here only the
  decode path + M6 logic get a Rust unit test with a hand-packed blob.
- OSR-frame deopt — `tests_s15.md`.
- Performance of deopt (it may be slow; only correctness is gated).
- Mach-exception-port delivery, debugger interplay — documented caveat, not
  tested.
