# 2× loop unrolling — design (measured first, then scoped)

*2026-08-05. Payoff measured BEFORE design, per this repo's rule.*

## Why (measured, not argued)

Source-level hand-unrolling of sieve's marking loop, `count=1899` preserved
on every arm:

| variant | run 1 | run 2 |
|---|---|---|
| 1× baseline | 49 µs | 46 µs |
| **2× serial** (plain body duplication) | **42 µs** | **43 µs** |
| 2× independent (indices strength-reduced off one `k`) | 43 µs | 46 µs |
| 4× | 38 | 37 |
| 8× | 40 | 39 — regressing |

Two conclusions that shape the whole design:

1. **~10%, and 2× captures all of it.** 4× adds nothing, 8× regresses into
   code bloat and register pressure. So: unroll factor 2, full stop.
2. **Index independence is worth nothing.** The *serial* unroll — where the
   second store's index waits on the first `k` update — is as fast as the
   independent one. So the pass needs NO strength reduction, NO induction
   analysis, NO proof about the stride. **It only has to emit the body
   twice.** The win is amortising loop-carried overhead: the back-branch,
   the safepoint `Poll`, the `k` update and its overflow check, and the
   per-iteration spill of `k` (`stur x8,[x29,#-112]`, +0x03b8).

Ceiling context: bounds-check elision on the same loop is 15.5% but
unsound-probe-only and needs a real analysis; this is ~10% and sound.
See `critical_path_findings.md`.

## What the IR actually looks like (verified, not assumed)

- `IrBlock` has **no terminator field**. Control flow is ordinary ops
  inside `code` — `Jump`, `SmiCmpBr`, `BoolBr`, `FCmpBr`, `RefCmpBr`, plus
  every fallible op's `fail` edge. The authoritative target list is
  `regalloc::successors` (regalloc.rs:173-231).
- `blocks[i].id == BlockId(i)` is load-bearing. **Append only** — never
  insert, reorder or remove.
- **No phis.** Loop-carried values are fixed slot vregs — `VReg(0)` = self,
  `VReg(1..=argc+ntemps)` = unified arg/temp slots — reassigned by plain
  `Ir::Move`. Operand-stack joins use pre-allocated merge vregs written by
  `Move` in each predecessor (`emit_merges`).
- **The body is a CHAIN of blocks, not one.** Every fallible smi op ends its
  block with a `Jump` to a freshly minted continuation
  (`fail_and_continue`, ir.rs:1913). sieve's marking loop is
  header(7) → 8 → 30 → … → latch → back to 7, about 5–6 blocks.
- `try_inline_cfg` is **not** a block cloner — it re-decodes the callee's
  bytecode (`decode::decode`, ir.rs:3671). There is no IR-level cloning
  machinery to reuse; this pass writes the first one.

## Duplicate `bci` is SAFE — the risk that could have sunk this

Deopt state is keyed on **native code offset**, not bci: `PcDesc.code_off`
(scopes.rs:498), with resume bci, reexecute flag and operand stack stored
per-site (`SafepointState`, scopes.rs:482), and vreg→slot resolved per-site
at that site's own linear position (`resolve_frame_loc`, driver.rs:1587).
Two sites sharing a bci are simply two entries.

Duplicate bci **already ships**: the inliner splicing one callee at two call
sites produces two trap blocks with identical bci in one nmethod — verified
at runtime with two distinct trap pcs both reporting `bci=8`, result
byte-identical to `MACVM_JIT=off`. The one bci-keyed map,
`deopt_live_slots` (ir.rs:1019), is default-off and is a pure function of
the *bytecode* bci, so two copies legitimately share one live set.

## The pass

Gated `MACVM_UNROLL=1`, default OFF until the A/B clears the bar. Runs
**after `copy_propagate`, immediately before `range_reduce`**
(ir.rs:12089) — range_reduce proves bounds per block, so it must see the
cloned blocks.

**Accept only** (narrow on purpose — firing on sieve's marking loop is the
success condition, generality is not):
- exactly one back-edge into a block that dominates it (the header);
- the header ends in a two-way compare-branch (`SmiCmpBr`/`BoolBr`);
- the region header..latch contains no `CallSend`/`CallRuntime`/`Alloc`
  (keeps the first version off the inlining/GC interaction entirely);
- region size under a small block/op budget.

Decline everything else, silently.

**Transform.** For region R = blocks from header's `if_true` target through
the latch:
1. Clone every block in R, appending with `BlockId(m.blocks.len())`, in R's
   order, so all copies land after all originals (preserves the OSR
   first-match convention at driver.rs:1004 and ir.rs:9828).
2. Clone `code` **and `deopt_sites` together**, always — a clone with stale
   or unre-keyed `deopt_sites` is a silent wrong-frame deopt, and no IR
   verifier would catch it (`verify_spill_all` and gated `oopmap::verify`
   check neither).
3. Remap **intra-region** targets to the clones; leave exit targets
   (including the header's `if_false`) pointing at the originals.
4. Clone the region's trap blocks too, so each copy owns its deopt site —
   do NOT share, because vreg→slot is resolved at the single site's linear
   position and the two copies may differ.
5. **Vreg policy — the correctness crux.** Keep `VReg(0)` and
   `VReg(1..=argc+ntemps)` IDENTICAL in the clone (they are hardcoded in
   `driver::build_deopt_metadata`; renaming them destroys every deopt in
   the method, not just the loop's). Mint fresh vregs for values *defined
   inside* the region and used only there. Merge vregs at the copy boundary
   keep their `Move`-in-each-predecessor discipline.
6. Rewire: original latch → header of copy; copy's latch → original header.
   **Keep both `Poll`s** in v1 — dropping one changes GC/interrupt latency
   for a few percent that is not on the measured critical path.

## Gates

Correctness (all must be byte-identical off-vs-threshold with correct
checksums):

```
cargo test --release
MACVM_UNROLL=1 cargo test --release
MACVM_UNROLL=1 just run-world-tests
MACVM_UNROLL=1 MACVM_GC_STRESS=1 just run-world-tests
MACVM_UNROLL=1 MACVM_GC_STRESS=full:64 just run-world-tests
MACVM_UNROLL=1 MACVM_DEOPT_STRESS=64 just run-world-tests
```

Plus `MACVM_UNROLL_COUNT=1` reporting loops-unrolled/declined, so a flat A/B
can be told apart from a pass that never fired — the failure mode P1 needed
a whole trace channel to rule out.

Performance: the interleaved A/B from `critical_path_findings.md`, 8 rounds,
alternating order, load-gated.

**Acceptance bar, fixed in advance: ≥6% end-to-end on sieve.** The
source-level ceiling is ~10%; a compiler pass that captures less than
two-thirds of a hand-written transform is not worth carrying. Below the bar
the pass stays gated off and this document records why.

## RESULT: built, correct, +1.3% — BELOW the bar, stays gated OFF

Landed and measured 2026-08-05. The pass fires (`[unroll] 2 loop(s) unrolled
2x`), is correct under every gate — 829 lib tests green with `MACVM_UNROLL=1`,
and `count=1899` under plain / `GC_STRESS=1` / `DEOPT_STRESS=64` — and the
interleaved 8-round A/B says:

| | mean |
|---|---|
| gate OFF | 47.6 µs |
| gate ON | 47.0 µs |
| **delta** | **+1.3%** |

The bar fixed in advance was ≥6%. **It stays gated off.**

### Why it missed — my design premise was wrong

The pass emits `H(test) → body → L → H'(test) → body' → L' → H`: **two
condition tests per two stores**, exactly as before. All it removes is one
unconditional back-branch per two iterations, which is worth ~1%.

The source-level 2× that measured ~10% did something different: it tested
`k <= size - prime` **once per two stores** and left a tail loop for the
remainder. The win was never body duplication — it was halving the
*condition test and the safepoint `Poll`*.

I concluded "no induction analysis needed" from the serial-vs-independent
experiment. That experiment ruled out **index** strength reduction (computing
the second index off one `k` rather than serially). It said nothing about
**guard** restructuring, which is where the win actually lives — and that
does need the induction variable and its stride, to strengthen the header
test to `k <= bound - stride` and emit a tail loop.

So the honest cost of the real win is materially higher than this pass:
induction-variable recognition, a strengthened guard, a cloned tail loop, and
the overflow reasoning for `bound - stride`. That is a range-analysis-shaped
slice, and it overlaps heavily with R4 in `range_analysis_design.md` — which
is measured at 15.5% on the same loop. **If either gets built, build R4
first**: it is worth more and the induction machinery it needs is the same
machinery this would need.

### What was kept

The pass itself is retained, gated off, because it is the repo's first
IR-level block cloner and a v2 would extend rather than replace it: region
discovery (natural loop body via forward∩backward reachability), append-only
cloning with the `blocks[i].id == BlockId(i)` invariant asserted, target
remapping over every branch form, and the shared-trap-block argument. One
pre-existing test premise was corrected en route —
`loop_poll_records_loop_poll_deopt_scope` asserted *exactly one* LoopPoll for
one back-edge, which is only true when nothing duplicates the latch.

## Risks, ranked

1. **Silent wrong-frame deopt** from cloned-but-unre-keyed `deopt_sites`.
   No verifier catches it. Mitigate with a debug_assert that every
   `deopt_sites` index is `< code.len()` and strictly ascending, plus the
   `DEOPT_STRESS=64` gate.
2. **Renaming a slot vreg** — destroys deopt method-wide. Mitigate with an
   explicit allowlist: only rename vregs `> argc+ntemps`.
3. **Breaking `blocks[i].id == BlockId(i)`** or the originals-before-copies
   ordering — produces wrong-block emission, not a clean error. Mitigate:
   append only, assert the invariant after the pass.
4. **Compile-time blowup** on big methods. Mitigate with the region budget;
   measure compile time on richards/deltablue.
