# SIMD vector support — design

**Status:** levels 1 AND 2 shipped. Level 1 — `Float64x2` (`70b9475`),
`Float32x4`, and `Int32x4` value classes with the NEON JIT fuse
`Ir::VecArith{kind}` → float `fadd/… .2d`/`.4s` or integer `add/sub/mul .4s`
(`541b1b2`, `cfd8c4f`, `b7f6eab`), ~13–15× over the interpreter. `Int32x4`
arithmetic wraps on 32-bit overflow (no vector integer divide); its reductions
are bit-exact (integer add is associative). Level 2 — `FloatArray` + explicit
hand-written NEON bulk-kernel primitives `+@`/`sum`/`dot:` (`1f23ba7`), in
`src/runtime/simd_kernels.rs`. Reductions ship in the
**fast pairwise order** (a `float64x2_t` accumulator + `faddp.2d`), the chosen
tradeoff (Part D) — deterministic (so interpreter ≡ JIT, both being one
primitive) but NOT the scalar left-fold, which `FloatArray>>sequentialSum`
provides for bit-parity. All verified byte-identical across `MACVM_JIT`
off/threshold and `MACVM_GC_STRESS`. Still designed-not-built: the reducer/q-pool
generalization (Part C1/C2, so a vector stays live in a `q`-register across ops
instead of box-per-op), the v8–v15 residency subtlety (C3),
`ValueLoc::VectorSlot` deopt (C4) — **the full implementation-grade design for
those is now `docs/simd_qpool_design.md`** (milestones Q1–Q4; it refines C2/C3:
Q values reside in `q16`–`q31`, not the scalar `d8`–`d15` pool, because no
register survives a call at 128 bits) — plus JIT auto-vectorization of
`1 to: n do:` loops (E2 research). The interpreter's dispatch loop and the object
model's core stay untouched throughout.

**Goal.** Use the hardware's NEON lanes the way you would in a low-level
language: a value type that *is* a bundle of 2/4/8/16 contiguous lanes, whole-
vector arithmetic on it, the same operations scaled up over arrays of values,
and horizontal reductions. Two levels:

1. **Vector values** — `Float64x2`, `Float32x4`, `Int32x4`, … : perform a
   function on two (or four, …) values at once.
2. **Vectorized arrays** — stream those operations over `FloatArray`s (`a +@ b`,
   `a dot: b`, `a sum`) via NEON loops.

The scalar float fast-path (`docs/float_fastpath_design.md`) is the width-1
special case of all of this, and it is already built — so most of the machinery
generalizes rather than being written from scratch.

---

## Part A — What is already in the tree (the substrate)

**The scalar float region** gives the whole shape at width 1: a mono-`Double`
send compiles to `FUnbox` (guarded load into a `d`-register) / native
`fadd`/`fmul` / `FBox` (alloc + store), with a box/unbox cancellation reducer, a
`d0`–`d15` register file (write-through residency in the callee-saved
`d8`–`d15`), and a `ValueLoc::DoubleSlot` that boxes a raw-`f64` frame slot back
on deopt. SIMD is this, one register-width up.

**The encoder already speaks NEON** (`src/vendor/wfasm/a64/encode.rs`,
confirmed):

- Vector arithmetic with an arrangement: `fadd`/`fsub`/`fmul`/`fdiv` `.2s/.4s/.2d`,
  integer `add`/`mul`, `fmla` (fused — deliberately *unused*, see §C4).
- Pairwise + across-lane reductions: `addp`, `faddp`, `fmaxp`, `addv`,
  `saddlv`/`uaddlv`, and the `simd_across_base` family (`fmaxv`/`fminv`-style
  "scalar `Vd`, `Vn.<arr>`").
- Lane move/extract/broadcast: `ins`, `umov`/`smov`, `dup`, lane `mov`.
- Structured load/store `ld1`/`st1`…`ld4`/`st4`, plus scalar `ldr q`/`str q`.
- Vector immediate `movi`; a `VReg { num, arr }` / `VElem` operand model.

So the *codegen substrate is done* — this design is about the IR, the value
model, the reducer generalization, and one AAPCS64 subtlety.

**The object model has the right precedent.** `Format::Double` (`oops/klass.rs`)
is a fixed, non-indexable object whose body is *raw bytes the GC never scans*
(its mark carries `tagged_contents = false`). A SIMD value is exactly that, just
16 bytes of body instead of 8. `ALLOC_ALIGN` is 8, **not** 16 — which is fine:
AArch64 `ldr q`/`str q` handle 8-aligned (indeed arbitrary) addresses
transparently, so none of the x86 `movaps`-must-be-16-aligned pain applies.
Alignment only ever matters at the box boundary, and there it is a non-issue.

---

## Part B — The value model (level 1)

### B1. The classes

A small family of **immutable value classes**, one per 128-bit NEON bundle
(128 bits is the natural unit; a class-per-width because each is a different
NEON *arrangement*):

| class | arrangement | lanes |
|-------|-------------|-------|
| `Float64x2` | `.2d` | 2 × f64 |
| `Float32x4` | `.4s` | 4 × f32 |
| `Int32x4`   | `.4s` | 4 × i32 |
| `Int16x8`   | `.8h` | 8 × i16 |
| `Int8x16`   | `.16b`| 16 × i8 |

Each instance is a fixed **16-byte raw body** (a `Format::Vector` — modeled on
`Format::Double`: GC skips the body, size fixed by the klass). Boxed at rest
(128 bits cannot ride in a 64-bit tagged word, exactly like `Double`);
register-resident only inside a JIT region.

`Float32x8` (8 × f32) and wider want *two* q-registers or SVE — deferred (§G);
NEON's fixed 128-bit lane bundles are the v1 target.

### B2. The protocol

```smalltalk
Float32x4 splat: 1.5.                 "broadcast — dup"
Float32x4 x: 1.0 y: 2.0 z: 3.0 w: 4.0. "lane constructor"
Float32x4 fromArray: fa at: i.        "load 4 lanes from a FloatArray offset"

v1 + v2.   v1 - v2.   v1 * v2.   v1 / v2.   "elementwise"
v1 min: v2.  v1 max: v2.  v1 abs.  v1 sqrt. "elementwise, libm/NEON"
v1 at: 2.                             "lane extract — umov/mov"
v1 at: 2 put: 9.0.                    "→ a NEW vector (immutable); ins"
v1 sum.  v1 dot: v2.  v1 maxLane.     "reductions (§D)"
```

### B3. The IR ops

Generalize the scalar float ops; a **width/arrangement tag** rides on each op
and on the vreg (the scalar `VRegInfo::is_fp` becomes a `RegClass` with a lane
width — `Gpr | F64 | V128{arr}`):

| op | lowers to |
|----|-----------|
| `VUnbox { dst, src, arr, fail }` | reject-smi + klass guard, then `ldr q,[src+16]` |
| `VBox { dst, src, arr }` | alloc 32 B (2-word header + 2-word raw body) + `str q` |
| `VArith { op, arr, dst, a, b }` | `fadd/fmul/… v.<arr>` |
| `VSplat { dst, src, arr }` | `dup v.<arr>, r/v[0]` |
| `VExtract { dst, src, arr, lane }` | `umov`/`mov` a lane to a gpr/`d` |
| `VReduce { kind, arr, dst, src }` | across-lane / pairwise tree (§D) |
| `VConst { dst, bits: [u8;16] }` | `movi`/pool-load a constant vector |

**As built (`Float64x2` + `Float32x4`).** The shipped slice does NOT split
`VUnbox`/`VArith`/`VBox` into separate ops with vector vregs — instead a single
fused `Ir::VecArith { kind, op, dst, a, b, fail }` (where `a`/`b`/`dst` are
ordinary **oop** vregs and `kind ∈ {F64x2, F32x4}`) lowers the whole guard →
`ldr q` → `f…v.<arr>` → inline-box run internally with FIXED scratch `q16`/`q17`
(v16/v17 are caller-saved, disjoint from the fp allocatable pool `d0–d7` and the
residency pool `d8–d15`, so no live fp vreg is clobbered). `kind` selects only
three things — the guard/box klass literal, the arrangement (`.2d`/`.4s`), and
the box stub — because both classes are 16-byte raw bodies, so the
guard/unbox/box skeleton and the two-64-bit-half `umov` are width-agnostic. This
"box-per-op" shape means **zero allocator, reducer, or `RegClass` changes** —
the tradeoff is one box alloc per op and no cross-op vector-value residency.
Splitting into the `VUnbox`/`VArith`/`VBox` table above (so the reducer can
cancel adjacent box/unbox and keep a vector live in a `q`-register across ops) is
Increment 2b, and is what unlocks C1–C4 below.

### B4. Bit-exact parity is preserved for elementwise ops

Each lane of `VArith` is an independent IEEE operation — `Float64x2 + Float64x2`
lane `i` is exactly `Double + Double` for lane `i`. So a vectorized elementwise
op is **bit-identical to the per-lane scalar op**, and the verification
discipline the scalar arc established (JIT ≡ interpreter, sharp `assert_eq`
goldens) carries straight over: a `Float32x4 +` and four scalar `Float32 +`s
produce the same bits. Consequently elementwise SIMD uses the *non-fused*
`fmul`/`fadd`, **not** `fmla` — same policy, and same reason, as the scalar
FMA decision (`float_fastpath_design.md` addendum): fusion rounds once, breaks
per-lane parity. Reductions are the one exception, and are handled honestly
in §D.

---

## Part C — The JIT vector fast-path

### C1. The reducer generalizes

The wide→narrow→fixpoint reducer (`reduce_float_boxes`) applies unchanged in
shape: a mono-`VectorClass` arith send (the IC is the type oracle) →
`VBox(VArith(op, VUnbox a, VUnbox b))`, then `VUnbox(VBox x) → x` cancellation,
dead-box elimination, deopt-sunk boxing, and vector-temp promotion. The rules
are width-agnostic; only the emit arms and the guard's klass differ.

### C2. One shared FP register file

The `q`-registers ARE the `d`-registers — `d8` is the low 64 bits of `v8`. So
scalar floats and vectors allocate from **one** FP pool (`regalloc`'s fp pool,
already independent of the GPR linear-scan); a vreg's width tag decides whether
a def touches 64 or 128 bits. Scalar and vector float code compose for free in
the same method.

### C3. The one real hardware subtlety — v8–v15 upper halves

**AAPCS64 preserves only the low 64 bits of `v8`–`v15` across a call.** The
upper 64 bits are caller-saved *even for the callee-saved vector registers*.
This is exactly why the scalar residency trick worked (64 bits is what's
preserved) — but a **128-bit** vector resident across a safepoint (`rt_poll`,
any `CallSend`) loses its top half. So a vector interval that crosses a
safepoint must **reload the full `q` from its canonical frame slot** after the
safepoint. That slot is already the source of truth for deopt (the write-through
residency invariant), so the reload is correct by construction — it just costs
one `ldr q` per crossed safepoint that the scalar path did not need.

Two implementations, pick per-measurement:
- **(v1) reload-from-slot:** a vector's residency register is valid only within
  a call-free span; at/after a safepoint, reads reload from the slot. Zero new
  prologue cost. Recommended first.
- **(later) widen `call_stub`:** save/restore full `q8`–`q15` (frame grows
  another 128 bytes), making 128-bit residency survive calls like the scalar
  case. Trade frame size for the reload; measure before adopting.

### C4. Deopt — one new `ValueLoc` kind

`ValueLoc::VectorSlot(off, arr)`: the frame slot at `off` holds `16` raw bytes
(not an oop — the GC skips it, same as `DoubleSlot`). The materializer reads the
bytes and boxes them into a fresh vector object of the arrangement's klass. One
new arm at the three materializer sites (scope slots / pending / reexecute
stacks), exactly mirroring `DoubleSlot`, and — as there — the bytes are read via
`read_frame_slot_raw` (never through `Oop::from_raw`, whose debug validation
rejects arbitrary bit patterns). Pinned forced-deopt-mid-loop regressions, per
the scalar arc's discipline.

---

## Part D — Reductions (the honest part)

Horizontal ops — `sum`, `product`, `dot:`, `maxLane`, `minLane` — combine the
lanes of one vector (or reduce a whole array, §E) to a scalar. NEON does this
with pairwise (`faddp`) trees or across-lane (`addv`/`fmaxv`) instructions.

**The numerical fact:** a tree/pairwise floating-point reduction reorders the
additions relative to a sequential left-fold, and FP addition is **not
associative** — so a SIMD `Float` reduction is *not* bit-identical to a scalar
`inject: 0.0 into: [:a :b | a + b]`. This is the same class of issue as FMA, and
the resolution is cleaner: **a reduction is its own operation, not sugar for a
scalar loop, so there is no scalar result it must be identical to.**

- **Elementwise vector ops MUST match per-lane scalar** (they do — §B4), and the
  goldens stay sharp.
- **FP horizontal reductions have a DEFINED lane-combination order** (the
  documented pairwise tree), and are verified against *that* definition, not
  against a scalar fold. A separate scalar `inject:into:` left-fold is a
  genuinely *different* operation and is expected to differ in the low bits;
  the docs say so. (`sum` may also offer an explicit `sequentialSum` that IS
  the scalar left-fold, for code that needs the exact scalar result.)
- **Integer reductions are exact** — integer addition/min/max are associative,
  so `Int32x4 sum` is bit-identical regardless of order. No caveat.

This keeps the parity discipline intact where it can hold (maps) and is honest
where it cannot (FP folds), without softening any golden.

---

## Part E — Scaling over arrays (level 2)

### E1. `FloatArray`

A new `FloatArray` (`Format::IndexableBytes`, element access as `f32` or `f64`
via the existing typed-byte primitives `doubleAt:`/`signedLongAt:` family,
prims 114–117): a contiguous, GC-skip-body buffer of N floats — the "array of
values" the vector ops stream over. `at:`/`at:put:` read/write single lanes;
`at: i asFloat32x4` reads a 4-lane bundle (bridging level 1 ↔ level 2).

### E2. Bulk kernels

The typical SIMD array maths, as **whole-array operations**:

```smalltalk
a +@ b.        "elementwise sum → new FloatArray"
a *@ scalar.   "scale"
a dot: b.      "reduction to a scalar"
a sum.  a max.
a map: [:x | x sqrt].  "elementwise unary"
```

Each is a NEON streaming loop: `ld1 {v.4s}, [pa], #16` / `ld1 {v2.4s}, [pb], #16`
/ `fadd v.4s` / `st1 {v.4s}, [pc], #16`, unrolled by lane width, with a scalar
tail for the non-multiple remainder.

**Two ways to get there, staged:**
- **(v2) hand-written NEON primitives — SHIPPED (`1f23ba7`, `82a45e3`).** `+@`
  (elementwise add), `scale:` (elementwise ×scalar), `sum`, `dot:`, `max`, `min`
  as explicit `core::arch::aarch64` intrinsic kernels in
  `src/runtime/simd_kernels.rs` (a `float64x2_t` stream + a scalar tail; the one
  `runtime` module that opts back into `unsafe`). Explicit intrinsics on
  purpose — a bulk kernel drives the silicon deliberately, NOT a scalar loop
  left to rustc/LLVM to maybe vectorize. The whole loop is one primitive call,
  so the interpreter and the JIT (which shims the primitive) run identical code.
  `max`/`min` are order-independent (bit-exact); the FP `sum`/`dot:` use the
  defined fast pairwise order (Part D).
- **(research) JIT auto-vectorization** of a Smalltalk `1 to: n do:` loop:
  dependence analysis, alignment/tail handling, lane masking. Much harder, a
  separate arc — and note OUR JIT does not auto-vectorize today, so this is net-
  new compiler work, not a switch to flip; the primitives cover the common
  kernels without it.

---

## Part F — Where it pays, and where it does not

**Drops in (elementwise / streaming):** array add/mul/scale, dot products,
color-matrix and coordinate transforms, DSP filters, packing the Mandelbrot
pixmap (`str q` writes 4 RGBA pixels at once), and reductions. These are the
Smalltalk idiom already — `whileTrue:`/`do:` loops over contiguous buffers where
each step is width-N independent work.

**Does NOT drop in (scalar, data-dependent):** the Mandelbrot *escape* loop —
its trip count is per-pixel data-dependent, so vectorizing means computing 4
pixels in lockstep with a **lane mask** that retires escaped lanes and iterates
until all four finish. That is the classic "SIMD Mandelbrot," and it is a
genuine algorithm rewrite (masked lane-parallelism), not a drop-in — worth
noting as the high-value advanced path, out of scope for the value/array model
here.

---

## Part G — Scope & staging

- **v1 (prove the generalization):** `Float64x2` first — it reuses the exact
  `.2d` `fadd`/`fmul` already emitted for scalars, so it is the smallest slice
  that proves the shared FP file, the width tag, the reducer, `VectorSlot`
  deopt, and the reload-from-slot residency. Then `Float32x4`. Elementwise
  `+ - * /`, `min:`/`max:`, lane `at:`, `splat:`.
- **v2:** `FloatArray` + the hand-written NEON bulk kernels (`+@`, `dot:`,
  `sum`, `scale:`, `map:`).
- **v3:** reductions with documented lane order (§D); the integer vector types.
- **Deferred:** 256-bit and SVE (scalable, variable lane count — a different
  model with no fixed arrangement); JIT auto-vectorization of scalar loops;
  masked lane-parallel algorithms (SIMD Mandelbrot); `call_stub` full-`q8–q15`
  preservation (§C3, if measurement justifies it over reload-from-slot).

---

## Cross-references

- `docs/float_fastpath_design.md` — the width-1 case this generalizes; the
  register file, reducer, residency, and `DoubleSlot`↔`VectorSlot` deopt
  machinery, plus the FMA/bit-parity decision the elementwise policy inherits.
- `src/vendor/wfasm/a64/encode.rs` — the NEON encoder surface (§A).
- `src/oops/klass.rs` `Format::Double` — the fixed raw-bytes GC-skipped-object
  precedent for `Format::Vector`.
- `docs/FFI.md` — the hand-written-native-kernel pattern the v2 bulk primitives
  follow.
- `docs/SPEC.md` decision log should gain an entry recording this design.
