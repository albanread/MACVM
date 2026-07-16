# Smalltalk → bytecode → machine code: the Mandelbrot inner loop

A worked example of the whole translation, on one real method:
`Mandelbrot>>escapeAtRe:im:` (`world/35_mandelbrot.mst`) — the escape-time
loop behind the `MandelZoom` demo, and the method the unboxed-float fast path
(`float_fastpath_design.md`) was built for. It is a good specimen because it
is *pure Smalltalk arithmetic*: every operation in it is an ordinary message
send, and every one of them has to disappear for the loop to be fast.

Companion to `31bytecodes.md` (which explains the machinery in general); this
document is the same story told once, concretely, with the real output.

**Reproduce any listing here:**

```
$ target/release/macvm rusttcl --world world
  flag jit threshold=50
  load /tmp/warm.mst              "anything that calls escapeAtRe:im: ~300 times"
  disasm Mandelbrot escapeAtRe:im:          "bytecode"
  disasm-native Mandelbrot escapeAtRe:im:   "machine code"
$ MACVM_DBG_IR=escapeAtRe:im: ./target/debug/macvm run /tmp/warm.mst --world world
```
(Native listings need a **release** binary — `cargo test` leaves `run` stale.)

## 1. The Smalltalk

```smalltalk
escapeAtRe: cr im: ci [
    | zr zi zr2 zi2 n |
    zr := 0.0. zi := 0.0. zr2 := 0.0. zi2 := 0.0. n := 0.
    [ (zr2 + zi2 < 4.0) and: [ n < maxIter ] ] whileTrue: [
        zi := (2.0 * zr * zi) + ci.
        zr := (zr2 - zi2) + cr.
        zr2 := zr * zr.
        zi2 := zi * zi.
        n := n + 1 ].
    ^n
]
```

Nothing here is an "operation" in the machine sense. `+`, `*`, `-`, `<` are
messages to `Double` objects; `whileTrue:` and `and:` are messages taking
block arguments; `zr2` and friends hold *pointers to heap-allocated Doubles*.
A literal reading says every iteration is **11 message sends and 5 heap
allocations**.

## 2. The bytecode

The front-end's only structural liberty is inlining the control flow
(`31bytecodes.md` Part 1): `whileTrue:` and `and:` become jumps, their blocks
dissolved. Everything else stays a send.

```
 20: push_temp 4          "zr2"          ← loop head (jump_back target)
 22: push_temp 5          "zi2"
 24: send 0; #+
 26: push_literal 1; 4.0
 28: send 1; #<
 30: br_false_fwd 9 -> 42                 ← `and:` short-circuit, inlined
 33: push_temp 6          "n"
 35: push_instvar 0       "maxIter"
 37: send 2; #<
 39: jump_fwd 1 -> 43
 42: push_false
 43: br_false_fwd 57 -> 103               ← `whileTrue:` exit, inlined
 46: push_literal 2; 2.0
 48: push_temp 2 ; 50: send 3; #*         "2.0 * zr"
 52: push_temp 3 ; 54: send 4; #*         "     * zi"
 56: push_temp 1 ; 58: send 5; #+         "     + ci"
 60: store_temp_pop 3     "zi :="
 62: push_temp 4 ; 64: push_temp 5 ; 66: send 6; #-    "zr2 - zi2"
 68: push_temp 0 ; 70: send 7; #+                      "     + cr"
 72: store_temp_pop 2     "zr :="
 74: push_temp 2 ; 76: push_temp 2 ; 78: send 8; #*    "zr * zr"
 80: store_temp_pop 4
 82: push_temp 3 ; 84: push_temp 3 ; 86: send 9; #*    "zi * zi"
 88: store_temp_pop 5
 90: push_temp 6 ; 92: push_smi 1 ; 94: send 10; #+    "n + 1"
 96: dup ; 97: store_temp_pop 6 ; 99: pop
100: jump_back 83 -> 20                   ← the loop
103: push_nil ; 104: pop ; 105: push_temp 6 ; 107: return_tos
```

Two jumps and a back-edge for the control flow; **11 real sends** for the
arithmetic. The bytecode has learned nothing about floats — it cannot: `+`
might be anything until you have watched the program run.

## 3. What the JIT knows that the bytecode doesn't

By compile time, each of those 11 send sites' inline caches has recorded the
receiver classes that actually arrived: `Double` for ten of them, `SmallInteger`
for `n < maxIter` and `n + 1`. That measurement is what licenses the rewrite —
under a guard, because it is measurement and not proof.

The IR (`MACVM_DBG_IR=escapeAtRe:im:`) shows the result — the sends are gone:

| op | count | what it is |
|---|---|---|
| `FArith` | 8 | the Double `+ - *` sends, now raw `fadd`/`fsub`/`fmul` on unboxed values |
| `FConst` | 10 | `0.0`, `2.0`, `4.0` as immediate bit patterns, not pool objects |
| `FCmpBr` | 1 | `zr2 + zi2 < 4.0`, fused straight into the loop's branch (§5.1) |
| `SmiCmpVal` + `BoolBr` | 1 + 1 | `n < maxIter` — *not* fused, because the inlined `and:` makes its result a phi consumed in another block (§5.2) |
| `FBox` / `FUnbox` | 4 / 2 | the *only* survivors of boxing — see §5 |
| `UncommonTrap` | 12 | every guard's fail edge: "the IC was wrong, rebuild an interpreter frame" |

## 4. The machine code

The loop body is `+0x0164`–`+0x0384`: **137 instructions**, ending in the
back-edge `b -0x220`. The arithmetic the Smalltalk actually asked for is these
nine:

```asm
+0x0174  fadd  d16, d10, d11     ; zr2 + zi2
+0x0194  fcmp  d13, d0           ;        < 4.0
+0x022c  fmul  d16, d0,  d8      ; 2.0 * zr
+0x0240  fmul  d16, d12, d9      ;      * zi
+0x0274  fadd  d1,  d14, d0      ;      + ci      → zi
+0x0294  fsub  d16, d10, d11     ; zr2 - zi2
+0x02c8  fadd  d0,  d15, d1      ;      + cr      → zr
+0x02e8  fmul  d0,  d8,  d8      ; zr * zr        → zr2
+0x0308  fmul  d0,  d9,  d9      ; zi * zi        → zi2
```

Eleven message sends became **nine floating-point instructions**. `zr`, `zi`,
`zr2`, `zi2`, `cr`, `ci` live in `d8`–`d15` — the callee-saved residency tier —
across the whole loop. There are **no boxes, no allocation, and no calls** in
the loop body; the one `blr` in range is the safepoint poll, dormant:

```asm
+0x0344  ldr  w16, [x28, #32]    ; the global poll word — 0 in steady state
+0x0348  cbz  x16, +0x3c         ; not armed → skip
+0x034c  ldr  x16, =stub_poll    ; (armed only when a GC or an invalidation
+0x0350  blr  x16                ;  is waiting for this loop to yield)
```

This is the fast-floats result in one screen: allocation per iteration went
from five Doubles to **zero**, which is why Mandelbrot's render went 746 ms →
166 ms and its allocation 708 MB → 4 MB.

## 5. Where the other 128 instructions go — read honestly

Nine of 137 instructions (~7%) are the program. The rest is the cost of being
a *safe, live, deoptimizable* Smalltalk, and it is worth naming precisely:

| what | count | why it is there | avoidable? |
|---|---|---|---|
| `stur`/`str` write-through | 27 | **The deopt tax.** S12 pins every interpreter-visible value in its canonical frame slot at every safepoint, so the GC and the deoptimizer can read a compiled frame with no register maps. Each def writes its slot *and* its register. | Only by giving deopt a register map (`ValueLoc::Register`) — a real design step, sound for non-oop values like these first. **But cost it against §5.1 first.** |
| `ldur`/`ldr` reloads + pool | 29 | The other half of write-through, plus literal-pool loads. | Partly, same way. |
| `fmov d…, d…` | 24 | Shuffles between the `d8`–`d15` residents and emit's `d16`/`d17` scratch. | Largely, by computing straight into the resident (the scalar O1 change did this for GPRs; the FP path has not had it yet). Same caveat. |
| branches | 19 | guard/trap edges, the `and:` merge, the back-edge | Mostly inherent |
| **FP math** | **9** | **the program** | — |
| poll | 3 | GC/invalidation liveness | No (2 dormant instructions is already the floor) |
| `csel`, `tst`, `cmp`, `movz/k`, `sub` | rest | smi tag checks, overflow edges, the `and:` phi, loop counter | Mostly inherent |

**Do not read that count column as a time budget.** It is a census of *space*.
§5.1 measured the conversion rate between the two, and it is roughly **60:1
against you**.

### 5.1 The Boolean round-trip — and why counting instructions lied

`zr2 + zi2 < 4.0` is immediately branched on (`28: send #<` then
`30: br_false_fwd`). The machine ought to `fcmp` and branch on the flags. It
did not — it built the result as a heap object and then took that object apart
again:

```asm
+0x01c8  fcmp  d13, d0
+0x01cc  ldr   x16, =true          ; materialize the RESULT as an object
+0x01d0  ldr   x17, =false
+0x01d4  csel  x16, x16, x17, mi   ; pick true or false
   …
+0x01e0  ldr   x17, =true          ; …then compare that object against true
+0x01e4  cmp   x25, x17
+0x01e8  b.eq  +0x28               ; …and finally branch
```

This one is now **fixed** — the float compare fuses to a single `Ir::FCmpBr`,
and the loop's compare is three instructions:

```asm
+0x01c0  fcmp  d13, d0
+0x01c4  b.mi  +0x18               ; branch straight on the flags
+0x01c8  b     +0x4
```

A second, unrelated find came out of the same disassembly. Every float spill
was **two** instructions:

```asm
+0x0188  sub  x16, x29, #80      ; materialize the slot address…
+0x018c  str  d16, [x16, #0]     ; …then store through it
```

while the integer path next to it spilled in one (`stur x16, [x29, #-112]`).
`emit::fp_spill_access` did this unconditionally, on the belief — written in a
comment — that the vendored encoder had no unscaled/negative-offset form for FP
scalars. It never lacked one: `wfasm::a64::encode::ldst` falls back to
`ldst_unscaled` for any negative offset and threads the V bit through, so
`str d16, [x29, #-80]` always encoded fine (as `STUR`, `0xfc1b03b0`). The false
comment cost **26 instructions in this loop alone**. `fp_spill_access` now
mirrors `emit_spill_access` exactly: one access in imm9 range, `sub` only for
deeper slots. (The disassembler could not decode the resulting `stur d` either
— 40 fresh `.word`s — which is the *same* gap as `fmov d,d`. Also fixed.)

### The measurement, and why the census lies

Both changes together, on an Apple M4, interleaved A/B (never single-shot: this
is a fanless Air and it throttles):

| | original | + fused compare | + spill fix |
|---|---|---|---|
| loop body | 174 instructions | 163 (−6.3%) | **137 (−21%)** |
| method | 1536 code bytes | 1480 | **1320 (−14%)** |
| 900×700 @ maxIter 600 | 377.8 ms | — | **376.5 ms (−0.35%)** |

**Cutting 21% of the loop's instructions bought 0.35% of its time.** That is a
~60:1 discount, and it is the most useful number in this document. Both changes
won every interleaved pair, so the sign is real and the magnitude is ~nothing.

The loop runs ~62.7M iterations in ~330–376 ms — about 5.3–6.0 ns, i.e. **~24
cycles per iteration** at ~4 GHz. The FP recurrence alone (`zr2`→`fsub`→`fadd`→
`zr`→`fmul`→`zr2`, and the longer `zi` chain through two `fmul`s) is roughly
10–15 cycles of pure latency. So the loop sits well above its dependency floor
but is *utterly indifferent* to instruction count: 37 instructions per iteration
vanished and the clock did not notice. They were off the critical path, behind
perfectly-predicted branches, on a core wide enough to retire them in slots the
FP chain was not using — and the `cmp`+`b.cond` tail was very likely macro-op
fused in the decoder before it ever issued. (Separating those mechanisms exactly
needs PMU counters, which this measurement does not have. The 60:1 discount does
not depend on the split.)

So both changes were kept for **code size**, not speed: 216 bytes per method,
one whole `UncommonTrap` block deleted (a fused compare-and-branch has no
`not_bool` edge — there is no Boolean object to fail to be a Boolean), and
`Ir::FCmpBr`/`emit_fcmp_br` reachable at last. I-cache pressure across a real
program with thousands of methods is not something one hot loop can show.

**The lesson, which outlives both patches:** on a wide out-of-order core an
instruction count is a bad proxy for time. The §5 table is a map of *space*.
The 27 write-through stores and 24 `fmov` shuffles are off the critical path in
exactly the way the `csel` and the 26 `sub`s were, and should be expected to
carry the same ~60:1 discount. **Giving deopt a register map to kill the
write-through stores is a real design change with real soundness risk, and this
result predicts it would buy approximately nothing in time** — it has to be
justified on other grounds (code size, frame size, simplicity) or not at all.
That is worth knowing before someone spends a sprint on it. If you want this
loop's remaining time, find out where the cycles actually go — measure the
dependency chain and the memory round-trips. Do not read a time budget off the
census; this document just proved that it lies by ~60×.

### 5.2 Why it happened, and the half that is still there

The IR named the bug. The compare was **`FCmpVal`** (value-producing) feeding a
separate **`BoolBr`**, where the fused **`FCmpBr`** belonged — an op that had
existed in the vocabulary all along, with a complete and correct
`emit::emit_fcmp_br` behind it. Nothing had ever *constructed* one. The
translator defers a fusable compare to its block's terminator via
`pending_cmp`, and only the smi arm ever read the `fusable` flag; the double
arm ignored it and always materialized. So `Ir::FCmpBr` was fully-built dead
vocabulary, and the giveaway that it had never run was that nothing tested it.
The fix reads `fusable` in the double arm too (`ir.rs`), and ships with
`double_cmp_fused_with_branch` — the test whose absence *was* the bug.

The integer compare, `n < maxIter`, looks identical in the disassembly but is
**not** the same problem, and is still there:

```
  block 2:  SmiCmpVal { dst: VReg(26), … }   Jump → block 11
  block 11: Move { dst: VReg(8), src: VReg(26) }   Jump → block 4
  block 3:  Move { dst: VReg(8), src: false }      Jump → block 4
  block 4:  BoolBr { val: VReg(8), … }
```

That is the inlined `and:`. Its result is a **phi** — `VReg(8)`, merging the
compare's result with the short-circuit's literal `false` — and the branch that
consumes it lives in a *different block*. `fusable` is correctly false here:
this block's terminator is a `Jump`, not a `Branch`. Fusing it is not a
selection fix but a jump-threading one (constant-fold the branch in block 3,
thread block 11, merge it into block 2, then fuse), which touches merge
bookkeeping and the threaded blocks' deopt records. Left alone deliberately —
and per §5.1 the honest expected payoff is a fraction of a percent, so it
should be done for the IR's sake if at all, not for the clock.

## 6. What the example is meant to show

- **The instruction set is not the story.** 31 bytecodes describe the loop; a
  measurement (the ICs) and a guard turn 11 sends into 9 FP instructions.
- **Optimism is the whole engine, and it is paid for.** The 13 uncommon traps
  and 18 write-through stores are the receipts that let the VM be wrong about
  `+` being Double arithmetic and still recover honestly.
- **"Fast" here means the allocation is gone, not that the math got clever.**
  The arithmetic was always nine instructions. The 746 → 166 ms came from the
  five heap Doubles per iteration that are no longer there.
- **Instructions are not time.** Deleting **21%** of the loop's instructions
  bought **0.35%** (§5.1). The core absorbs anything off the critical path, so
  the census in §5 is a map of *space*; reading a time budget off it will
  mislead you by ~60× about which taxes are worth paying down.
- **Reading the output finds real bugs.** Writing and then re-measuring this
  document surfaced five, none of them hypothetical: the disassembler could not
  decode `fmov d,d` — the most common instruction in float code — nor `stur d`,
  every float spill; the float compare never fused, because `Ir::FCmpBr` had
  been *unreachable* since the float fast path shipped and no test noticed; a
  false comment in `fp_spill_access` doubled every float spill for want of an
  encoding the assembler already had; and the belief that any of it cost
  meaningful time was itself wrong. All five were found by reading the output
  rather than reasoning about the source.

## Cross-references

- `31bytecodes.md` — the instruction set and the compiler pipeline in general
- `float_fastpath_design.md` — why the boxes are gone (and where the surviving
  `FBox`es went: sunk into the cold trap blocks past `+0x0440`)
- `PERF.md` — the Mandelbrot numbers in their arc
- `DEBUGGER.md` — `disasm`, `disasm-native`, `MACVM_DBG_IR`, and the rest of
  the tooling every listing here came from
- `SPEC.md` §7/§9 — the safepoint and deoptimization contracts the tax pays for
