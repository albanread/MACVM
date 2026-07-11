# Unboxed method-local float math — IR review + reducer design

**Status:** designed, not built. Same posture as `docs/CANVAS.md`/`docs/ASM.md`
— the shape is pinned in full here; no code has landed. This is tier-1
compiler work only (`src/compiler/`); the interpreter and the runtime
primitive table are unchanged.

**Motivation (measured).** The Canvas Mandelbrot (`world/35_mandelbrot.mst`)
spends ~460 ms and allocates ~595 MB per 420×220 render. A disassembly of the
compiled `Mandelbrot>>escapeAtRe:im:` (via the RUSTTCL `disasm-native` verb)
showed the inner loop is **nine `bl` sends per iteration**, each into a
separately-compiled `Double>>*` / `+` / `-` / `<` nmethod that is only a
primitive shim:

```
; Double>>*  (nm=1)
movz x10, #0x66      ; primitive 102 (Double *)
blr  x16             ; → rt_call_primitive → prim_double_mul
```
and `prim_double_mul` (`src/runtime/primitives.rs`) is:
```rust
PrimResult::Ok(alloc::alloc_double(vm, a.value() * b.value()).oop())
```
i.e. every multiply is *two nested calls + a heap allocation to box the
result*. The `fmul` is one cycle; the boxing and call overhead around it is
everything else. The locals `zr/zi/zr2/zi2` never escape, yet every
intermediate is boxed. This is the "fast floats" gap the tour page names:
Strongtalk got the speedup by "eliminating all the allocation for intermediate
results" within a single method (`fastfloats.html`), and — like this design —
deliberately did **not** pass unboxed doubles across method boundaries.

---

## Part A — How float sends are represented today

### A1. The IR is register-based and block-structured

`src/compiler/ir.rs`'s `Ir` enum is a flat op list over `VReg`s, grouped into
`IrBlock`s. Ops name their `dst`/source vregs explicitly; there is no
value-graph/SSA layer. Relevant existing ops:

- `CallSend { dst, site, args }` — an **opaque monomorphic-or-generic send**
  through the site's inline cache (→ mono nmethod, or c2i into the
  interpreter). **This is what a `Double *` is today.**
- `SmiArith { op, dst, a, b, fail }`, `SmiCmpBr`, `SmiCmpVal`, `ArrayAt`,
  `ArrayAtPut` — arithmetic/array sends **already intrinsified** to native
  guarded sequences.
- `GuardKlass { obj, expect, fail, kind }` — a receiver-klass guard in front of
  an inlined body; `fail` is a cold `UncommonTrap` block.
- `Alloc { dst, klass, size_words }` — inline bump allocation; a regalloc
  **safepoint**.
- `Poll` — loop back-edge GC-poll flag check; `UncommonTrap { bci }` — the
  deopt exit (`brk #0xDE00`); `CallRuntime { stub, args }` — a runtime stub
  call.

### A2. Smi arithmetic is already reduced — but smis are immediates

The template already exists. During convert (`ir.rs`), `classify_smi_send`
reads the send's IC guard klass and, if it is the smi klass, maps the callee's
primitive number to a `SmiSendKind` (`1 → Add`, `2 → Sub`, `3 → Mul`,
`10 → Lt`, …) and pushes `Ir::SmiArith`/`SmiCmpBr` instead of a `CallSend`.
The physical shape `emit.rs` lowers is: tag-test the operands, native `add`
with an overflow check, and a `fail` edge (a wrong-type/overflow operand
re-executes the send at tier-0 via `UncommonTrap`).

**But a SmallInteger is a tagged immediate** — `SmiArith` produces the result
value directly in a GPR with no heap traffic. There is no box, no unbox, and
therefore nothing to cancel. That is the whole reason floats need more than
`SmiArith`'s one-shot recognition.

### A3. A Double send is opaque, and its boxing is invisible to the IR

There is no `classify_double_send`. A `Double *` stays a `CallSend`, and the
allocation happens *inside the runtime primitive* (`prim_double_mul →
alloc_double`), a layer the IR cannot see. Consequences:

- The IR today has **no float ops, no box/unbox nodes, and no FP registers.**
- You cannot "cancel box/unbox" yet, because those operations are not IR nodes
  — they are buried in the primitive. **Step one is to lift them into the IR**
  (make box/unbox/native-arith explicit ops); only then can a reducer cancel
  them.

### A4. The register file is a clean slate

`regalloc.rs`: `NUM_ALLOCATABLE_REGS = 16` — x0–x15 GPRs, linear-scan
allocated; x16/x17 scratch, x18 platform, x19–x23 unused/residency. **The
entire `d0`–`d31` SIMD/FP file is untouched by the compiler.** That is an
advantage, not a gap: unboxed floats get an FP allocator that is *fully
independent* of the GPR linear-scan — no shared interference graph, no
cross-class spill decisions, and (since a `d`-reg is never an oop) no presence
in any oop map, so the GC root walker already ignores it.

### A5. The type oracle already exists

`feedback.rs::read_send_site` returns `SiteFeedback::Mono { klass, method }`
from the interpreter IC's recorded receiver. For a `*` site whose IC observed
only Doubles, that is `Mono { klass: Double }` — the exact evidence the reducer
matches on. **No separate type-inference pass is needed**; the send carries its
own type history.

---

## Part B — The reducer design

### B0. The contract (what a float region is)

A **float region** is a maximal run of IR bounded by *reify points*. Its
contract is deliberately narrow:

> **arguments in (boxed) → unbox at the boundary → in between: only assembler
> maths and libm calls, no allocation, no GC, no message sends → box the
> result at the boundary → result out.**

A **reify point** is anything that must observe Smalltalk-level (boxed) state:
a heap allocation, a GC poll that can collect, or a real message send
(`CallSend`). At a reify point, live unboxed floats are boxed first.

A **libm call is NOT a reify point.** `sqrt`/`sin`/`cos`/`exp` are leaf math
that follow AAPCS64 and preserve callee-saved `d8`–`d15`, so a region may
contain them without spilling anything held in `d8`–`d15` — this is why the
register split in B4 matters. (`Double>>sqrt` is primitive 106,
`f64::sqrt`; the transcendentals would be added the same way.)

### B1. New IR ops (lift box/unbox into the IR)

Add, alongside `SmiArith`:

| Op | Meaning | Lowers to |
|---|---|---|
| `FUnbox { dst_f, src, fail }` | boxed Double oop → `d`-reg; **guarded** — `src` must be a Double, else `fail` (deopt) | reject-smi + klass-word check, then `ldr d,[src,#off]` |
| `FBox { dst, src_f }` | `d`-reg → freshly allocated boxed Double | inline `alloc_double` (a safepoint) + `str d` |
| `FArith { op, dst_f, a_f, b_f }` | `op ∈ {add,sub,mul,div}` | `fadd/fsub/fmul/fdiv d,d,d` |
| `FCmpBr { op, a_f, b_f, if_true, if_false }` / `FCmpVal` | Double comparison (fused or materialized) | `fcmp d,d` + `b.cond` / `cset` |
| `FConst { dst_f, bits }` | a Double literal | `fmov`/pool-load into a `d`-reg |
| `IToF { dst_f, src, fail }` | SmallInteger → `d`-reg (mixed math) | untag + `scvtf d,x` |

`VReg` gains a **class tag** (`Gpr` | `Fp`) so regalloc routes each vreg to the
correct physical file. Everything else about `VReg` is unchanged.

### B2. The rule set (wide → narrow → fixpoint)

Applied repeatedly until no rule fires. Ordered widest-first, because the
widest pattern is the send itself and the IC is what licenses collapsing it —
go narrow-first and you shred the send into micro-ops and lose the idiom.

1. **WIDE — IC-driven send lowering** (the `classify_double_send` analog of
   `classify_smi_send`): a `CallSend(site, [a, b])` whose feedback is
   `Mono { klass: Double }` and whose callee primitive is an arith/compare op
   →
   ```
   FBox(FArith(op, FUnbox(a), FUnbox(b)))          // arith
   FCmpBr(op, FUnbox(a), FUnbox(b), …)             // compare fused with a branch
   ```
   Each `FUnbox`'s `fail` edge is an `UncommonTrap` that re-executes the
   original send at tier-0 (identical to `SmiArith`'s `fail`).
2. **NARROW — box/unbox cancellation:** `FUnbox(FBox(x)) → x`. Fires as soon as
   a chained op's `FBox` sits in front of the next op's `FUnbox`.
3. **NARROWER — dead-box elimination:** an `FBox` whose only consumer was a
   `FUnbox` removed by rule 2 is dead → delete it. *This is where the
   allocation actually disappears.*
4. **CLEANUP:** hoist loop-invariant `FConst`s (`2.0`, `4.0`) out of the loop
   (LICM-ish); coalesce the per-`FUnbox` guards of a straight-line run into one
   region-entry guard (B3).

**Termination:** rule 1 strictly decreases the `CallSend` count; rules 2–3
strictly decrease node count. The system cannot cycle, so the fixpoint is
reached in bounded iterations.

### B3. Guards hoist to the region boundary

The reduction does not delete the type check — it **hoists** it. `N` per-send
IC guards become **one** guard at the region entry ("these operands really are
Doubles"); the body runs unguarded native FP; a single deopt edge covers a
type violation. `N` guarded sends → **1 guard + N unguarded FP ops + 1 deopt
edge.** The region boundary, the safepoint boundary, and the box/reify boundary
are all the same line — which is what keeps this simple.

### B4. Registers — two sub-pools of the empty `d`-file

- **`d0`–`d7` (caller-saved):** scratch for call-free spans. Zero prologue cost,
  but clobbered by any call (including `rt_call_primitive`/alloc/GC), so only
  valid where the region is call-free.
- **`d8`–`d15` (callee-saved):** loop-carried values and anything live across a
  **libm** call (libm preserves them per AAPCS64). Using any of these adds a
  one-time `stp/ldp` of the used `d`-regs to the method prologue/epilogue — the
  only frame-layout change, and only when the method actually uses float
  fast-paths.

Allocated by a small **independent** free-list, disjoint from the x0–x15 linear
scan. **No FP spill logic in v1:** 32 registers against float expression trees
that are a handful of values deep means overflow is effectively impossible; if
a pathological method would overflow, the reducer simply does **not** unbox the
offending value (it stays a boxed `CallSend`) — a correctness-preserving
fallback, never a spill slot.

### B5. Safepoints & deopt (the correctness crux) — REVISED

**Scope review (2026-07-11) collapsed the original v1/v2 split.** The key
structural fact: spill-all-at-safepoints exists solely so the **moving GC**
can find and update live oops, and the GC only scans **frame slots** — never
registers (`oopmap.rs` is slot-indexed; spill-all is what makes that sound).
A raw `f64` is exempt from that entire reason: the GC neither scans `d`-regs
nor needs to update a float. So floats may legitimately stay in registers
across safepoints; the only consumer that needs the value there is the
**deopt materializer**.

The mechanism is already in the tree: S14's **write-through residency**
(x21–x23) — the value lives in a callee-saved register, every def ALSO writes
the canonical frame slot, and deopt/oop-maps read slots unchanged. Applied to
floats:

- A loop-carried float lives in a **callee-saved `d8`–`d15`** register for its
  whole interval, crossing `Poll`s (and libm calls) without dying.
- Each def write-throughs to its frame slot (`str dK, [fp,#slot]` — ~1 cycle,
  off the dependency path). The oop map marks the slot **non-oop**.
- The deopt map gains ONE new entry kind — `DeoptSlot::Double { slot }`:
  "this interpreter temp is a raw f64 in frame slot N; box it on
  materialize." No trampoline changes, no reading registers out of signal
  frames.

Reify (actually box) is then needed only where boxed state is *observed*: a
real `CallSend`'s argument, a store to a heap slot, or `^` of a Double.
Given this VM's deopt-materializer history (S13/S14), `DeoptSlot::Double` is
the piece to verify hardest: differential golden values with `MACVM_PIN`,
`MACVM_DEOPT_STRESS`, and a forced-deopt-mid-loop test that checks the boxed
temps match the interpreter's.

### B5a. Scope review: method-local vs class-wide (decided)

Considered and REJECTED: spreading unboxed representation to **instance
variables** ("all the slots in a class"). That changes *object layout*, which
leaves the compiler entirely: per-class layout maps in the scavenger/full GC
(today every slots-object slot is scanned as an oop), representation checks in
the interpreter's `push_ivar`/`store_ivar`, a class-layout migration or
permanent guard when a non-Double is stored (V8's field-deprecation tarpit;
SpiderMonkey shipped "unboxed objects" and later deleted the feature),
plus reflection/`image_store`/redefinition fidelity. A VM-wide representation
project, not compiler work — and Strongtalk itself drew the same line
(fast floats were method-local).

The goal *behind* class-wide — covering a class's float helpers — has a
sanctioned path already in the tree: the **depth-1 inliner + S24 splicing**.
Inlining a small float method into its hot caller merges their regions, so the
operands never box to cross the call, because there is no call. Widen regions
by inlining, not by new object layout.

Mandelbrot check: method-local + across-safepoint registers makes
`escapeAtRe:im:`'s loop allocate **zero** (2 `FUnbox` per pixel at entry;
`^n` is a smi; `maxIter` is a smi ivar — no float ivar exists in the hot
path). Class-wide would buy it nothing.

### B6. Scope (matches Strongtalk and keeps it small)

- **IN:** method-local, non-escaping `Double` temps. Pure-double first
  (`escapeAtRe:im:`, the dominant cost); mixed int×double (`IToF`/`scvtf`, the
  `py * step` coordinate setup) second.
- **OUT:** passing/returning unboxed doubles across method boundaries
  (Strongtalk didn't either — the region ends at the `^`); FP spilling;
  polymorphic sites (a `Poly`/`Mega` site stays a `CallSend`).

### B7. Worked example — `escapeAtRe:im:`

Before (per iteration): 9 `CallSend`s → 9 boxed `alloc_double`s.

After the reducer:
```
; region entry: FUnbox zr,zi,zr2,zi2,ci,cr,c2.0,c4.0  (one guard covers them)
fmul  d4, dZR, dZI        ; 2.0*zr*zi   (2.0 hoisted to a const reg)
fmul  d4, d4, d2_0
fadd  dZI, d4, dCI        ; zi := … + ci
fsub  d5, dZR2, dZI2      ; zr := (zr2 - zi2) + cr
fadd  dZR, d5, dCR
fmul  dZR2, dZR, dZR
fmul  dZI2, dZI, dZI
fadd  d6, dZR2, dZI2      ; zr2 + zi2
fcmp  d6, d4_0            ; < 4.0  → loop branch
; n := n + 1 stays a SmiArith (integer path, untouched)
```
No sends, no allocation inside the loop. Under v1, `zr/zi/zr2/zi2` are boxed
once at the `Poll`; under v2, zero. `^n` boxes nothing (it returns a
SmallInteger).

### B8. Where it lands in `src/compiler/`

- `ir.rs` — the new float ops (B1), and `classify_double_send` mirroring
  `classify_smi_send` for rule 1.
- a new **reduce pass** over `IrBlock`s (rules 2–4 to fixpoint) between convert
  and regalloc.
- `regalloc.rs` — `VReg` class tag + an independent `d`-pool free-list (B4).
- `emit.rs` — lower `FArith`→`fmul`/…, `FUnbox`→guard+`ldr`, `FBox`→inline
  alloc, `FCmpBr`→`fcmp`+`b.cond`, `IToF`→`scvtf`.
- `oopmap.rs` + the deopt path — the v2 `DeoptSlot::Double { dreg }` only.

### B9. Cross-references

- `docs/VMregisters.md` §2 — the x0–x15 pool and the arity caps this does not
  touch. `weekend_work.md` Gap 2 — the separate arity-cap raise.
- `docs/CANVAS.md` — the pixmap path whose remaining cost this design removes.
- `src/compiler/ir.rs` `classify_smi_send` / `Ir::SmiArith` — the working
  template rule 1 mirrors.
- `docs/SPEC.md` decision log should gain an entry recording this design.
