# Frameless leaf methods — Cog's `needsFrame`, MACVM-shaped

*Ranked #1 in `cog_send_portability.md`: the one whole mechanism Cog has that
MACVM lacks. Cog's `StackToRegisterMappingCogit>>scanMethod` computes
`needsFrame` per method (most bytecodes are `needsFrameNever`);
`compileFrameBuild` then emits NOTHING for frameless methods — no fp push, no
stack-limit check, args stay in registers, bare `RetN`. Richards' swarm of
tiny accessors/predicates activates for free. This doc is the MACVM design.*

## 1. Why this is especially right on arm64

AArch64 makes the frameless leaf *literally* zero-overhead in a way x86-family
ports never quite get:

- **The return address never touches memory.** `bl` puts it in `x30`; a leaf
  that makes no calls returns with a bare `ret` through `x30`. No push, no
  pop, no store-forwarding traffic, no return-stack-buffer disturbance.
- **`sp` is never adjusted** — no `sub sp`/`mov sp` pair, no frame-pointer
  chain update (`x29` still points at the CALLER's frame, which stays the
  correct top-of-chain for every walker, see §4).
- What a framed activation costs today (`emit.rs` ~1856+): `stp x29,x30,
  [sp,#-16]!` + `mov x29,sp` + `sub sp,#frame` on entry, the Task-#94
  **nil-fill of every deopt-referenced spill slot** (a store per slot, per
  activation — BUG D root cause 4), and `mov sp,x29` + `ldp` + `ret` on exit.
  For a `^next` accessor that is ~8-10 instructions + 4 memory ops wrapping a
  2-instruction body.

## 2. Eligibility — exhaustive over the IR, not heuristic

Decided per compiled unit AFTER lowering, by one scan over the final IR
(`driver.rs`, before `emit()`). The `Ir` enum (ir.rs:161) makes the rule
exhaustive-by-construction — new ops fail closed because the match must name
them:

**Frameless-compatible ops** (no call, no safepoint, no trap, no allocation):
`ConstSmi`, `ConstPool`, `Move`, `Param`, `LoadKlass`, `LoadField`,
`StoreField`*, `SmiCmpBr`, `SmiCmpVal`, `RefCmpVal`, `BoolNot`, `FUnbox`,
`FArith`, `FCmpBr`, `FCmpVal`, `FConst`, `Jump`, `BoolBr`, `Ret`, `RetSelf`.

**Disqualifiers** (any one ⇒ framed, today's codegen unchanged):
- `CallSend`, `CallRuntime` — any `bl` clobbers `x30`; a "leaf" is defined by
  their absence. (Also: sends need the NLR check, runtime calls need a walkable
  caller frame.)
- `Alloc`, `FBox`, `VecArith` — can scavenge ⇒ safepoint ⇒ the frame must be
  scannable.
- `Poll` — the safepoint itself.
- `UncommonTrap`, `GuardKlass`, `SmiArith` (overflow edge), `ArrayAt`/
  `ArrayAtPut` (bounds edge) — deopt needs the frame's slots + metadata to
  rebuild the interpreter activation. A frameless method must be
  **deopt-free**, which is what lets it drop its nil-fills and deopt records
  entirely.
- Any block/context machinery (closes over the frame by definition).

\* `StoreField` is eligible **iff** its write barrier is the inline
card-dirty sequence; if the current lowering routes any barrier case through
`CallRuntime`, that case disqualifies (the scan sees the `CallRuntime`, so
this is automatic, not a special rule).

**Register discipline:** after regalloc, the unit must have used **zero spill
slots and only caller-save registers** (x0-x17 minus reserved). A leaf that
spills needs stack ⇒ falls back to framed. Small bodies won't spill; this is
a verification, not a planner.

**Expected coverage:** exactly richards' shape — ivar readers (`^link`),
writers (`link := aPacket`), literal/arg returns, and the `RefCmpVal`/
`BoolNot` predicates (`^dest ~~ nil`) the S24 wave made trap-free. Arithmetic
accessors (`^count + 1`) stay framed in v1 (overflow edge); that's fine —
they're not the swarm.

## 3. Codegen changes

- `driver.rs`: the eligibility scan sets `frameless: bool` on the compile;
  stored as a flag on the `Nmethod` (nmethod.rs:159) for tooling honesty
  (§4), not for correctness — nothing at runtime branches on it.
- `emit.rs`:
  - **Prologue**: keep `emit_entry_guard` (emit.rs:1679) exactly as-is — the
    IC-dispatched checked entry is orthogonal to framing. Skip the
    `stp/mov/sub` triple and ALL nil-fills.
  - **Epilogue**: bare `ret` (no `mov sp,x29`/`ldp`).
  - **`verified_entry_off`** currently asserts it lands on the prologue's
    `stp` (emit.rs test ~2879, "found empirically in the listing, not
    assumed"). For frameless units the verified entry is the first BODY
    instruction; the golden/assert updates with it.
  - No deopt records, no oopmap entries, no reloc for stubs — asserted empty
    for frameless units (a cheap invariant check at blob-finish time).
- `regalloc`: unchanged algorithm; add the post-hoc "no spills, caller-save
  only" verification that flips the unit back to framed on violation.

**What does NOT change:** callers. A send site cannot know its callee is
frameless (ICs relink freely between framed and frameless targets, and
c2i/i2c adapters sit on the same entry protocol). The caller's `emit_nlr_check`
after the `bl` stays in v1 — a frameless callee can never *originate* an NLR
(no sends, no blocks) but can still be *returned through* by… no: NLR unwinds
via the sentinel through CALLERS of the block's home; a frameless leaf is
never on that path mid-body (it has no calls), and its ordinary return
feeds the caller's existing check harmlessly. Retiring the check is plan #2
(`cog_send_portability.md`), deliberately not coupled to this.

## 4. Why every walker stays sound (the load-bearing argument)

The single invariant that makes framelessness safe:

> **A frameless method contains no safepoint, no trap, and no call — so no
> GC, no deopt, no halt, and no stack inspection can ever observe one
> mid-execution.**

Consequences, walker by walker:

- **GC (`runtime::frames::walk_frames`, oopmap-driven):** scans only run at
  safepoints (`Poll`/`Alloc`/send boundaries). None exist inside a frameless
  body, so the leaf's registers (including the oop args in x0..xN and the
  return pc in `x30`) are never live at a scan. The caller — whose `x29`
  chain is untouched — is correctly the youngest scannable frame.
- **Deopt/recompile:** deopt-free by eligibility; version invalidation still
  works (relink the IC target; the old blob dies with the zone epoch, same as
  today).
- **Debugger (DBG2/DBG4):** halts happen at interpreter bytecode boundaries;
  a breakpointed method is tier-0-pinned (never the compiled frameless copy).
  `bt`'s compiled-frame expansion never meets a frameless activation for the
  same no-safepoint reason.
- **PROBE (crash dossier):** a native fault inside a frameless body (should
  one ever occur) resolves its pc to the nmethod by code-range as today; the
  raw-fp walk starts at the CALLER — one frame "missing" relative to the pc.
  The `frameless` nmethod flag lets the dossier print "(frameless leaf)" so
  the report is honest rather than confusing. This is the one tooling edge,
  and it is cosmetic.
- **Supervisor/watchdog, NLR, adapters:** unaffected — see §3.

## 5. Build ladder (each rung gated)

- **F0 — eligibility scan + flag, emit unchanged.** Scan lands with a
  counter: compile log reports `frameless-eligible: N of M units` on a world
  boot + richards run. Gate: the count matches a hand-audit of a sample;
  zero behavior change (byte-identical blobs).
- **F1 — frameless emission behind `MACVM_FRAMELESS=1`.** Prologue/epilogue
  elision + empty-metadata asserts + `verified_entry_off` golden update.
  Gates: full debug+release suites, tier-parity (results byte-identical to
  JIT=off) on richards/deltablue/bench.list, `MACVM_GC_STRESS=1` release
  richards+deltablue (the no-safepoint invariant means stress-GC cannot even
  touch these units — the gate proves the ELIGIBILITY scan is right, i.e.
  nothing that can trap was misclassified).
- **F2 — measure (the only justification that counts).** Interleaved A/B,
  fresh binaries, `uptime` first: richards + deltablue, JIT threshold
  config, ≥3 pairs. Keep iff wall-clock wins; `instructions-are-not-time`
  applies in full.

  **MEASURED (d9587cc, same binary, env as the interleave lever):** richards
  ×100 iterations: 237/204, 207/198, 210/206 ms — frameless wins every pair,
  ~2-4% steady-state. deltablue ×160: 63/61, 62/62 — ~0-3%. Real but MODEST:
  the likely explanation is that the inliner already swallows the hottest
  accessors at their monomorphic call sites, so the standalone frameless
  units execute mainly at cold/polymorphic sites. The mechanism is sound,
  fully gated, and costs nothing when off — but the remaining richards gap
  (if any post-RefCmpVal; the Cog re-measure is still step 0) lives in plan
  #2 (per-send NLR sentinel) / #3 (spill-all), not here.
- **F3 — default-on + flag retirement** once F2 holds, then the cog-bench
  re-measure (step 0 of `cog_send_portability.md`, needs the Pharo install
  restored) to restate the honest Cog gap.

## 6. Risks / open questions

- **Task-#94 lineage:** the nil-fills exist because BUG D's deopt read
  uninitialized slots. Frameless units drop them by dropping DEOPT ITSELF —
  the safe direction — but F1's stress gate exists precisely to catch an
  eligibility hole that lets a trapping op through frameless.
- **`SmiArith` in v1:** excluded (overflow edge). If measurement shows the
  swarm includes hot `+ 1` accessors, v2 can add an overflow-trap-to-framed
  RECOMPILE (trap once, recompile framed) — machinery `note_uncommon_trap`
  already has.
- **Profiler/`sample` attribution:** leaf time attributes to the leaf's pc
  normally; nothing changes for the `sample`-based workflow that found the
  2.6× interpreter win.
- **Entry-guard-only methods:** a frameless unit is guard + body + ret; if
  the body is ≤2 instructions the unit is smaller than its IC padding —
  harmless, but the zone's min-unit rounding should be checked in F1.
