# Regalloc arc — Stage 1: residency across calls

2026-08-02. The compiler review (profiling richards/arith/fib, reading emitted
nmethods) put MACVM's compute-bench losses in **register-allocation policy**,
not dispatch — a `sample` profile of a hot bench is 100% JIT code, 0 named Rust
frames, `ic_misses=0`. Stage 1 attacks the largest single item. Result:
**richards −17.8%, dict −11%, deltablue −6.8%, fib −4.1%, alloc −3.7%**, landed
default-on.

## First: a correction to the review's own diagnosis

The review (and this repo's `regalloc.rs` module doc) describe the policy as
"spill-all-at-safepoints". **That phrase is misleading and the review repeated
it.** `allocate()` already spills only intervals where `crosses_safepoint` is
true, and that flag is computed as `start <= p && end > p` — genuinely
*live-across*. "Spill only live-across values" was already implemented; it was
never the bug.

The real tax is two layers in:

1. A `crosses_safepoint` interval is spilled **for its entire lifetime**, not
   just around the safepoint (no interval splitting). Its canonical home is the
   frame slot.
2. That is mitigated by `resident_reg` — a pool register (x21–x27 / d8–d15)
   mirroring the slot, write-through, reads preferred — **but residency was
   denied to any interval with `crosses_call`**.

So *any value live across a real send was memory-resident for its whole life*.
In `fib:`, `n` and `self` cross the two recursive calls, so every use is an
`ldur` — **including the four uses before the first call ever executes**.

## The change

Allow residency for call-crossing intervals, and reload the resident register
from its canonical slot after every call it spans. One `ldur` per crossed call
replaces one `ldur` per use.

The `!crosses_call` gate existed for two real reasons, and the post-call reload
answers **both at once**:

- **Clobbering** — a compiled callee uses the same x21–x27 pool as its own
  residents. Irrelevant if we reload after the call.
- **GC staleness** — a GC inside the call moves the oops the register points
  at, updating only the oopmap'd *slot*. Reloading *from that slot* after the
  call yields the relocated pointer.

Soundness rests on slot currency at the call, which was already guaranteed:
`emit_call_send` runs `emit_s2_spill_stores()` (S2 defers slot stores; this
flushes every resident's slot) **immediately before** the `bl`. So: slots
current → call → GC updates slots → reload reads GC-updated values. The other
GC-capable points inside an interval (Poll/Alloc/FBox slow paths) already
reloaded residents; `UncommonTrap` and trap fail-edges are terminating.

Implementation: `resident_across_calls()` in `regalloc.rs` (relaxes the gate),
`emit_resident_reloads_at_excluding()` in `emit.rs` (the existing reload helper,
plus a skip for the call's own destination — its interval starts *at* the call,
so it would otherwise be loaded with a stale slot one instruction before
`commit` overwrites it). Applies to `CallSend` and `CallRuntime` (both are
`call_positions` entries).

`fib:` codegen: **`ldur` 11 → 6**; `n`/`self` now live in x22/x21 across the
recursive calls. Code size 280 → 296 bytes (the reloads).

## Measurement

A/B methodology, strongest of the session: **one binary, env flip** — no build,
code-layout, or ASLR difference between arms. Cooled machine + 150 s settle,
alternated, 3 rounds × 41 samples, run **twice** (the second run also carried a
refinement, below). Best warm µs per arm:

| bench | off | on | Δ |
|---|--:|--:|:--|
| richards | 1458 | **1199** | **−17.8%** |
| dict | 300 | **267** | **−11.0%** |
| deltablue | 176 | 164 | −6.8% |
| fib | 10988 | 10533 | −4.1% |
| alloc | 597 | 575 | −3.7% |
| arith | 1359 | 1432 | (+5.4% — artifact, see below) |
| sieve | 177 | 181 | (+2.3% — artifact) |

**The arith/sieve "regressions" are a harness artifact, proven not inferred.**
`benchArith` contains no sends, so no interval in it can cross a call and the
flag cannot change its code — confirmed by disassembling it under both settings:
**opcode-identical**. The delta came from always running the `off` arm *first*
within each round, i.e. in the coolest slot.

> **Harness law:** alternate arm **order**, not just arm. `off,on / on,off` per
> round pair. A fixed order gives the first arm a systematic thermal advantage
> of a few percent — enough to invent a regression, or hide one.

### A rejected refinement (kept as a record)

Hypothesis for the apparent arith regression: newly-eligible call-crossers
crowd out tight-loop values in the resident pool, because the priority key is
interval *length*. Fix attempted: sort non-call-crossing intervals first.
**Measured effect: zero** (richards/dict/deltablue/arith all within noise of the
unrefined arm). Reverted — the hypothesis was wrong, and the disassembly test
above explains why there was nothing to fix. The observation that motivated it
still stands and is Stage 1b:

> `order.sort_by_key(|&i| Reverse(end - start))` uses interval **length** as a
> proxy for value. The right key is a spill-cost model:
> **benefit ≈ Σ uses (weighted by loop depth) − calls crossed.** Both terms are
> static (IR use counts, CFG loop nesting). This is what HotSpot/LLVM do, and it
> is deterministic — no profile needed.

## Correctness gates

Every one run in the default (on) configuration:

- Release test suite: **828 passed**, 3 failed — the known release-mode
  `should_panic`/`debug_assert` artifacts, identical with the flag off.
- All seven benchmark **checksums** verified (cog-bench fails hard on a wrong
  result).
- **`MACVM_GC_STRESS=1`** (scavenge per allocation) — DeltaBlue + Richards ×20,
  checksums verified.
- **`MACVM_GC_STRESS=full`** (moving/compacting collector) — checksums verified.
  This is the direct test of the stale-resident-oop hazard.
- **`MACVM_DEOPT_STRESS=1`** (periodic nmethod invalidation) — checksums verified.
- World boot clean.

## Next

- **Stage 1b — spill-cost model** (above). Deterministic, static, and the thing
  the length heuristic is standing in for.
- **Stage 2 — parameter register promotion**: stop homing args to frame slots at
  entry; frame stores only on safepoint slow paths.
- **Stage 3 — frame-init by liveness**: the 12-`stur` nil prologue shrinks to
  slots a safepoint can observe before first definition. (`entry_early_defs`
  already exists — start by finding why it isn't firing.)
- **Stage 4 — deopt environments**: record register *locations* in `pcdescs` so
  deopt reconstructs frames from wherever values live; spilling around
  safepoints disappears as a category.
- **Then re-gate the parked peepholes** (`MACVM_PEEP_IMM`, and constant LVN once
  `range_reduce`'s `MoveFrom`-of-`NonSmiConst` resolution is fixed) — see
  `peephole_findings.md`. Their verdicts were taken on the *old* substrate.

### On using runtime profile data for these decisions

The compiler already consumes runtime feedback: `IrMethod.site_feedback` carries
per-send-site receiver classes **with execution counts** (interpreter-PIC
sourced; compiled-PIC counter words are noted in `feedback.rs` as a later step),
and it already changes allocation indirectly — inlining runs *before* regalloc,
so a monomorphic-inlined send never becomes a `call_position` at all.

Worth keeping straight: for the residency decision the missing input is
**static** (use counts × loop depth), not runtime. Profile data earns its keep
one level up — discounting cold guard arms, and real loop trip counts, which no
static estimate can supply. When that lands, keep the policy a pure function of
a *recorded* profile snapshot (`snapshot_profile` exists for exactly this) and
log the decision: profile-dependent codegen is otherwise non-reproducible, which
breaks both A/B gating and bug reproduction.
