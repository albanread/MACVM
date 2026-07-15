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
| `FCmpVal` | 1 | `zr2 + zi2 < 4.0` |
| `SmiCmpVal` | 1 | `n < maxIter` (tag-checked, with a trap edge) |
| `FBox` / `FUnbox` | 4 / 2 | the *only* survivors of boxing — see §5 |
| `UncommonTrap` | 13 | every guard's fail edge: "the IC was wrong, rebuild an interpreter frame" |

## 4. The machine code

The loop body is `+0x018c`–`+0x0440`: **174 instructions**, ending in the
back-edge `b -0x2b4`. The arithmetic the Smalltalk actually asked for is these
nine:

```asm
+0x01a4  fadd  d16, d10, d11     ; zr2 + zi2
+0x01c8  fcmp  d13, d0           ;        < 4.0
+0x0290  fmul  d16, d0,  d8      ; 2.0 * zr
+0x02ac  fmul  d16, d12, d9      ;      * zi
+0x02e4  fadd  d1,  d14, d0      ;      + ci      → zi
+0x0310  fsub  d16, d10, d11     ; zr2 - zi2
+0x0348  fadd  d0,  d15, d1      ;      + cr      → zr
+0x0374  fmul  d0,  d8,  d8      ; zr * zr        → zr2
+0x03a0  fmul  d0,  d9,  d9      ; zi * zi        → zi2
```

Eleven message sends became **nine floating-point instructions**. `zr`, `zi`,
`zr2`, `zi2`, `cr`, `ci` live in `d8`–`d15` — the callee-saved residency tier —
across the whole loop. There are **no boxes, no allocation, and no calls** in
the loop body; the one `blr` in range is the safepoint poll, dormant:

```asm
+0x03e0  ldr  w16, [x28, #32]    ; the global poll word — 0 in steady state
+0x03e4  cbz  x16, +0x5c         ; not armed → skip
+0x03e8  ldr  x16, =stub_poll    ; (armed only when a GC or an invalidation
+0x03ec  blr  x16                ;  is waiting for this loop to yield)
```

This is the fast-floats result in one screen: allocation per iteration went
from five Doubles to **zero**, which is why Mandelbrot's render went 746 ms →
166 ms and its allocation 708 MB → 4 MB.

## 5. Where the other 165 instructions go — read honestly

Nine of 174 instructions (~5%) are the program. The rest is the cost of being
a *safe, live, deoptimizable* Smalltalk, and it is worth naming precisely:

| what | count | why it is there | avoidable? |
|---|---|---|---|
| `str d …` write-through | 18 | **The deopt tax.** S12 pins every interpreter-visible value in its canonical frame slot at every safepoint, so the GC and the deoptimizer can read a compiled frame with no register maps. Each def writes its slot *and* its register. | Only by giving deopt a register map (`ValueLoc::Register`) — a real design step, sound for non-oop values like these first. |
| `fmov d…, d…` | 22 | Shuffles between the `d8`–`d15` residents and emit's `d16`/`d17` scratch. | Largely, by computing straight into the resident (the scalar O1 change did this for GPRs; the FP path has not had it yet). |
| `csel` + `ldr =true/false` | 2 + 8 | **The Boolean round-trip** — see below. | Yes, and cheaply. |
| poll | 3 | GC/invalidation liveness | No (2 dormant instructions is already the floor) |
| trap/guard edges, `n` bookkeeping | rest | smi tag checks, overflow edges, loop counter | Mostly inherent |

### The Boolean round-trip — a real, findable gap

`zr2 + zi2 < 4.0` is immediately branched on (`28: send #<` then
`30: br_false_fwd`). The machine ought to `fcmp` and branch on the flags. It
does not:

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

The IR explains it: the compare is **`FCmpVal`** (value-producing) feeding a
separate **`BoolBr`**, not the fused **`FCmpBr`** — which exists in the IR
vocabulary and is exactly this pattern. The integer compare in the same loop
has the identical shape (`SmiCmpVal` + `BoolBr`, not `SmiCmpBr`). So both
compares in the hottest loop in the demo pay a ~9-instruction Boolean
round-trip to express a branch the hardware already computed in `fcmp`.

That is not a design compromise like the deopt tax — it is a selection gap:
the fused ops are there and are not being chosen when the compare's result
flows into an inlined `and:`/`whileTrue:` branch. Filed as a follow-on rather
than fixed here, because the fix belongs in the translator's branch-shape
recognition, not in this document.

## 6. What the example is meant to show

- **The instruction set is not the story.** 31 bytecodes describe the loop; a
  measurement (the ICs) and a guard turn 11 sends into 9 FP instructions.
- **Optimism is the whole engine, and it is paid for.** The 13 uncommon traps
  and 18 write-through stores are the receipts that let the VM be wrong about
  `+` being Double arithmetic and still recover honestly.
- **"Fast" here means the allocation is gone, not that the math got clever.**
  The arithmetic was always nine instructions. The 746 → 166 ms came from the
  five heap Doubles per iteration that are no longer there.
- **Reading the output finds real bugs.** Writing this document surfaced two:
  the disassembler could not decode `fmov d,d` — the most common instruction
  in float code, printed as a bare `.word` (fixed, `disasm_a64.rs`) — and the
  Boolean round-trip above.

## Cross-references

- `31bytecodes.md` — the instruction set and the compiler pipeline in general
- `float_fastpath_design.md` — why the boxes are gone (and where the surviving
  `FBox`es went: sunk into the cold trap blocks past `+0x0440`)
- `PERF.md` — the Mandelbrot numbers in their arc
- `DEBUGGER.md` — `disasm`, `disasm-native`, `MACVM_DBG_IR`, and the rest of
  the tooling every listing here came from
- `SPEC.md` §7/§9 — the safepoint and deoptimization contracts the tax pays for
