# Q-wide float residency — the SIMD register fast-path (Parts C1–C4, in full)

**Status:** designed, not built. This elaborates `docs/SIMD.md` Part C into an
implementation-grade design, the way `docs/float_fastpath_design.md` did for
scalar floats (that design is SHIPPED; this one generalizes it to 128-bit
vectors). Tier-1 compiler work only (`src/compiler/`); the interpreter, the
three vector primitives blocks, and the `FloatArray` kernels are untouched.

**Motivation (measured shape, not yet timed).** Today every vector operation is
one `Ir::VecArith` (`src/compiler/ir.rs:313`) — a *collapsed* guard + unbox +
NEON op + **box**. The NEON instruction is 1 cycle; the box is an eden bump
allocation (~20 bytes header+tail) *per operation*. A chained expression

```smalltalk
accum := accum + (v * w).      "Float64x2"
```

compiles to TWO `VecArith`s = two klass-guard pairs, two `ldr q` unbox pairs,
and **two boxed results** — the `v * w` intermediate is allocated only to be
immediately unboxed by the `+`. In a loop that is the exact disease the scalar
float fast-path cured (`Mandelbrot`: 708 MB → 4 MB): the arithmetic is free,
the boxing is the program. The cure generalizes; this document pins how.

---

## 1. IR — split `VecArith` into the scalar trio's vector twins

Mirror the shipped scalar ops (`ir.rs:255–270`) exactly:

| new op | scalar twin | semantics |
|---|---|---|
| `VUnbox { dst, src, kind, fail }` | `FUnbox` | klass-guard `src` against `kind`'s klass literal (pool: `float64x2_klass_lit` / `float32x4` / `int32x4`, `emit.rs:266–272`); `ldr q` the 16-byte tail into a Q vreg; wrong klass → `fail` (uncommon trap, same as today's `VecArith` guard) |
| `VArith { dst, op, a, b, kind }` | `FArith` | one NEON instruction on Q vregs: float `fadd/fsub/fmul .2d`/`.4s`, integer `add/sub/mul .4s` — the exact instructions the fused arm emits today (`emit.rs:1034`), minus its unbox/box halves |
| `VBox { dst, src, kind }` | `FBox` | allocate a fresh vector of `kind`'s klass (inline eden bump; overflow edge `bl`s the EXISTING `stub_box_float64x2` family — `emit.rs:295` — unchanged) and `str q` the lanes into its tail |

`kind` is the existing `VecKind` (`ir.rs:94`). `VecArith` itself is then
*retired at build time*: the fuse arm (`classify_float64x2_send` and siblings)
emits the trio instead of the collapsed op. The collapsed op's emit arm stays
until the last milestone as the flag-off fallback (see §8), then dies.

**Bit-parity is untouched:** `VArith` emits the same single NEON instruction
per op as today; no `fmla` fusion (the SIMD.md Part B4 golden posture is
inherited wholesale — reordering/contracting is still forbidden).

## 2. The reducer — `reduce_float_boxes` generalizes by kind

`reduce_float_boxes(m, osr)` (`ir.rs:6326`) already implements every rule this
needs; the rules become kind-indexed:

- **Cancellation:** `VUnbox(VBox(x, kind), kind) → x` — *same-kind only*. A
  cross-kind pair never cancels: the box's klass is program-visible (identity,
  `class`, DNU routing), and `Int32x4` bits under a `Float32x4` klass would be
  a type confusion. The scalar rule needed no kind check (one Double klass);
  the vector rule makes it explicit.
- **Dead-box elimination:** a `VBox` whose result is never observed (no use, or
  uses only by cancelled `VUnbox`es) is deleted — this is what erases the
  `v * w` intermediate above.
- **Deopt-sunk boxing:** a `VBox` needed only by deopt metadata migrates into
  the trap's own cold block (the scalar machinery, reused: the sink test is
  "all uses are deopt-map references", and the cold-block emission point
  already exists).
- **Vector-temp promotion (the B5 twin):** a temp that provably always holds
  one `kind` of vector across a loop is promoted to a raw Q vreg with a
  write-through frame slot — the loop-carried `accum` above never boxes inside
  the loop. Same fixpoint, same promotion test as scalar float temps, plus the
  kind check.

The guard dedup falls out for free, exactly as it did for scalars: after
cancellation, only the *leaves* of a vector expression carry `VUnbox` guards —
`accum + (v * w)` guards `accum`, `v`, `w` once each and boxes once.

## 3. Registers — `q16–q31`, and why not `v8–v15`

**The hardware fact (SIMD.md C3):** AAPCS64 preserves only the LOW 64 bits of
`v8`–`v15` across a call. A 128-bit resident in `v9` loses its top half at any
`bl` — including `rt_poll`, every `CallSend`, and every stub call. So, unlike
the scalar `d8–d15` residency (whose 64 bits are exactly what the ABI
preserves), **no register in the machine keeps a Q value alive across a call.**

That fact makes the pool choice for v1:

- **Rejected: share the scalar residency pool (`fp_pool = 8..16`,
  `regalloc.rs:688`).** Q values gain nothing from callee-saved-ness they
  can't use, and every Q interval would interfere with the scalar write-through
  residents (`d8` *is* the low half of `v8`), evicting proven scalar wins to
  hold values that die at the next call anyway.
- **Chosen: `q16–q31` — the caller-saved upper file.** Sixteen registers, zero
  prologue/epilogue cost (nothing to save: our caller expects nothing of
  them), zero interference with `d0–d7` scratch, `d8–d15` residents, or the
  two emit-scratch vector regs the fused arm uses today (`v0`/`v1`). Residency
  semantics are honest: *valid within a call-free span only* — which is the
  only semantics a Q value can have (§4).

The allocator change is small: the fp linear scan gains a width tag on each
vreg (`D64` | `Q128`, alongside the existing float class tag from the scalar
arc) and draws Q intervals from a second range. One allocator, two ranges,
width picks the range. Scalar behavior is byte-identical by construction —
the `8..16` pool and its intervals are untouched.

## 4. The residency contract — write-through + reload-at-safepoint

The scalar invariant is kept verbatim, with one addition:

1. **Write-through:** every `VArith`/`VUnbox` def of a promoted vector ALSO
   `str q`s to its canonical frame slot at def time. The slot is the single
   source of truth at every safepoint — the S12 pin (`ValueLoc` has no
   register variant, `scopes.rs:278`) is preserved, and deopt never reads a
   register.
2. **Safepoint invalidation (the new rule):** any instruction that can enter
   Rust or another nmethod — `CallSend`, `rt_poll`, every stub `bl` including
   `VBox`'s own eden-overflow edge — marks ALL Q residents *stale*. The next
   read of a stale resident emits `ldr q` from its slot and re-marks it valid.
   This is a per-block emit-time flag on the vreg (the emitter already walks
   linearly and knows where the safepoints are); no new IR is needed.

   The scalar residents are exempt — `d8–d15` low halves survive calls, which
   is precisely the scalar design's §register-residency argument, unchanged.
3. **Cost model:** one `ldr q` (≈4 cycles, L1) per resident per crossed
   safepoint, *instead of* one allocation + box + unbox round trip (~30+
   cycles and GC pressure) per op. Inside a call-free arithmetic span — the
   hot case, e.g. an unrolled lane loop — residents never touch memory except
   the write-through stores.

**Explicitly deferred (measure first):** widening `call_stub` to save/restore
full `q8`–`q15` (+128 bytes of frame) to make residency survive calls, as
SIMD.md C3 sketches. v1's reload-from-slot needs no frame change and its cost
is bounded and local; adopt the wide save only if a real workload shows
safepoint reloads dominating.

## 5. Frame slots — 16 bytes, and their alignment

A promoted vector's canonical slot is **two consecutive 8-byte spill slots**,
allocated by the same FP-relative allocator that hands out `DoubleSlot` homes
(`scopes.rs:82` site). Rules:

- The pair is contiguous; `off` names the low word. `str q`/`ldr q` on Apple
  silicon do not fault on 8-byte alignment, but the allocator rounds the
  vector-slot region to 16 anyway (one pad slot at worst) — predictable
  performance and STP-friendliness cost one word.
- The slots are **non-oop**: never in an oop map, invisible to the moving GC —
  identical posture to `DoubleSlot` (raw f64 bits) and the byte tails the GC
  already skips. `MACVM_GC_STRESS` differential gates prove it (§8).

## 6. Deopt — `ValueLoc::VectorSlot(off, kind)` and the materializer

One new `ValueLoc` variant (`scopes.rs:278`), payload = slot offset + `VecKind`
(the kind picks the klass to rebox into — all three kinds share the same
16-byte tail layout, so the byte count is constant):

- **Recording:** a promoted vector temp records `VectorSlot(off, kind)` in the
  scope map exactly where a promoted float temp records `DoubleSlot(off)` —
  same write-through guarantee makes it valid at every safepoint.
- **Materialization:** one new arm at each of the three materializer sites
  (`runtime/deopt.rs:513` scope slots, `:581` pending stack, `:700` reexecute
  stack), mirroring the `DoubleSlot` arms beside them: read 16 raw bytes as
  two `read_frame_slot_raw` words (NEVER through `Oop::from_raw` — its debug
  validation rejects arbitrary bit patterns, the exact trap the scalar arc
  documented), allocate via the existing `alloc_float64x2`/`alloc_float32x4`/
  `alloc_int32x4` genesis paths, store the two words into the tail.
- **The `read_value` unreachable arm** (`deopt.rs:144`) gains `VectorSlot` in
  its message for the same reason `DoubleSlot` is there: the variant is only
  legal in slot/stack positions.
- **Gates:** pinned forced-deopt-mid-loop regressions — a vector accumulator
  loop deopted at its poll must materialize the accumulator with bit-exact
  lanes, under `MACVM_DEOPT_STRESS` and at both JIT thresholds. This is the
  scalar arc's discipline (`float_fastpath_design.md` "honest deoptimization"),
  re-run for Q.

## 7. What it buys — the worked example

```smalltalk
"today: 2 allocations, 4 klass guards, 4 ldr q, 2 str q + 2 headers"
accum := accum + (v * w).

"after C1+C2 (reducer), inside a loop with accum promoted:"
    ldr  q16, [fp, #v_slot]      ; loop-invariant loads hoistable later (LICM)
    ldr  q17, [fp, #w_slot]
    fmul v18.2d, q16, q17        ; VArith mul
    fadd v19.2d, q19, v18.2d     ; VArith add into the resident accumulator
    str  q19, [fp, #accum_slot]  ; write-through — the only memory traffic
"0 allocations in the loop; klass guards only at the loop preheader"
```

The box appears once, after the loop, where `accum` is last observed — or in
the deopt cold block if only deopt observes it. This is the Mandelbrot story
(708 MB → 4 MB, 4.5×) transplanted to vector code; the expected magnitude on a
chained-vector microbench is the same order: the NEON op is ~1 cycle, so the
speedup IS the removed allocation rate.

Bench: `world/bench/vecchain.mst` — a `Float64x2` (and `Float32x4`) accumulator
loop in the shape above, timed cold/warm, plus alloc-rate deltas from the
metrics counters (`bytes_allocated` before/after — the dashboard's own signal).

## 8. Milestones — each lands green, gated, committed

The scalar ladder's staging, reused:

- **Q1 — IR split + flag.** Add `VUnbox`/`VArith`/`VBox`; fuse arm emits the
  trio under `MACVM_QPOOL=1`, collapsed `VecArith` otherwise (default). Emit
  arms for the trio in the naive form (unbox/op/box adjacent — behaviorally
  identical to today). Gates: vector goldens byte-identical flag-on vs flag-off
  vs interpreter; `prim_ids_frozen` untouched (no primitive changes anywhere in
  this arc).
- **Q2 — reducer.** Kind-indexed cancellation + dead-box + guard dedup in
  `reduce_float_boxes` (rename to `reduce_fp_boxes` when the vector arms land).
  Gates: goldens again; `disasm-native` on the chain shape shows ONE box and no
  `bl stub_box_*` between the NEON ops; alloc-rate collapse on `vecchain`.
- **Q3 — residency + write-through.** The `q16–q31` range, width tags, the
  safepoint-invalidation rule, 16-byte slots. Gates: `MACVM_GC_STRESS` full
  differential (raw q bits invisible to the collector), threshold-1 and
  threshold-200 parallel soak per the standing stress rule.
- **Q4 — deopt.** `VectorSlot`, the three materializer arms, temp promotion
  switched on. Gates: forced-deopt-mid-loop lane-exactness, `MACVM_DEOPT_STRESS`
  round-robin soak, and the full-world boot+bench suite. Then the flag flips
  default-on, the collapsed `VecArith` emit arm is deleted, and SIMD.md Part C
  is marked SHIPPED pointing here.

**Scope cuts (explicit):** no `fmla` (parity); no q-wide `call_stub` save (§4,
measure first); reductions unchanged (they are their own ops with a defined
lane order — SIMD.md Part D governs); `FloatArray` bulk kernels unaffected (they
are single primitives; there is nothing to fuse across); no auto-vectorization
(E2 stays research).

## Cross-references

- `docs/SIMD.md` Part C — the sketch this document expands; its status line now
  points here.
- `docs/float_fastpath_design.md` — the shipped scalar design every mechanism
  here mirrors (reducer rules, write-through residency, `DoubleSlot`, the
  deopt-sunk cold-block box, the gate discipline).
- `docs/VMregisters.md` — the register conventions the `q16–q31` choice slots
  into; `docs/arm64.md` for the AAPCS64 v8–v15 half-preservation fact.
- Shipped substrate this builds on: `Ir::VecArith` + klass literals
  (`src/compiler/ir.rs:313`, `emit.rs:266–272`), the fused NEON arm
  (`emit.rs:1034`), `stub_box_*` (`emit.rs:295`), the fp pool
  (`regalloc.rs:688`), `reduce_float_boxes` (`ir.rs:6326`), `DoubleSlot`
  materializer arms (`src/runtime/deopt.rs:513/581/700`).
