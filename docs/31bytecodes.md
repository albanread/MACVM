# 31 bytecodes — how the MACVM compiler works

MACVM's entire instruction set is **31 bytecodes**. That is not a limitation
the compiler works around; it is the design that makes the compiler possible.
This document explains what the 31 are, why so few suffice, what *else* the
compiler knows beyond them, and how a stack of pushes and sends becomes the
native code the benchmarks run on.

## Part 1 — The instruction set

The full set (`src/bytecode/opcode.rs`), in its five natural families:

| family | opcodes |
|---|---|
| **Push** (9) | `PUSH_SELF` `PUSH_NIL` `PUSH_TRUE` `PUSH_FALSE` `PUSH_SMI_I8` `PUSH_LITERAL` (+`_W`) `PUSH_TEMP` `PUSH_INSTVAR` `PUSH_GLOBAL` |
| **Store** (5) | `STORE_TEMP` (+`_POP`) `STORE_INSTVAR_POP` `STORE_GLOBAL_POP` `STORE_CTX_TEMP_POP` |
| **Stack & closures** (5) | `POP` `DUP` `PUSH_CTX_TEMP` `PUSH_CLOSURE` |
| **Send** (4) | `SEND` `SEND_SUPER` (+ wide forms) |
| **Control & return** (8) | `JUMP_FWD` `JUMP_BACK` `BR_TRUE_FWD` `BR_FALSE_FWD` `RETURN_TOS` `RETURN_SELF` `BLOCK_RETURN_TOS` `NLR_TOS` |

Notice what is **missing**: there is no add, no compare, no array access, no
allocation, no type test. `3 + 4` compiles to `PUSH_SMI 3; PUSH_SMI 4;
SEND #+`. Everything that looks like an "operation" in other instruction sets
is a **message send** here. Four of the 31 carry essentially all of the
language's semantics; the other 27 just move values into position.

This is the classic Smalltalk bargain, inherited through Self and Strongtalk:
the instruction set is trivial *because* the dispatch is universal — and the
whole burden of performance moves to one question: **how do you make a send
cheap?** The compiler is the answer to that question.

The one deliberate exception is **control flow**. `ifTrue:`, `ifFalse:`,
`and:`, `or:`, `whileTrue:`, `whileFalse:`, `timesRepeat:`, `to:do:`, the
`ifNil:`/`ifNotNil:` family, and `repeat` *look* like ordinary sends to
block arguments, but the AST→bytecode front-end recognizes them and emits
`BR_TRUE_FWD`/`JUMP_BACK` directly, dissolving the block bodies inline
instead of allocating closures. This is the sole front-end "optimization,"
and it earns its place for a reason nothing else does: it is what gives the
JIT a real control-flow graph to work on, and what keeps a method that
branches from being dragged out-of-line by an escaping closure. Every other
optimization is deferred downstream to the JIT, where the type feedback and
deoptimization that make optimism safe actually live — the front-end has
neither, so anything it folded would be weaker (no types) and unsound (no
guard to deoptimize when a redefinition breaks the assumption).

## Part 2 — What the compiler knows that the bytecode doesn't say

If the bytecode were all the compiler had, `SEND #+` could never become an
`adds` instruction — `+` might be defined on anything. The bytecode says
*what* to do; five other sources say *what actually happens*:

**1. Type feedback — the interpreter's inline caches.** Nothing compiles
cold. By the time a method is hot enough to compile, it has run interpreted,
and every send site's IC has recorded which receiver classes *actually
arrived* (`site_feedback`: Mono / Poly / Mega per site). This is the primary
fuel. A `SEND #+` whose IC says Mono(SmallInteger) compiles to a tag test and
an `adds`; a `SEND #foo` whose IC says Mono(Point) inlines `Point>>foo`'s
body; a `SEND #at:` observed on Arrays becomes a bounds-checked load. It is
measurement, not proof — so every use is guarded, and guards can fail (Part 5).

**2. Customization — the receiver class being compiled for.** Every nmethod
is compiled for one `key_klass`; `Point>>x` compiled-for-Point is a different
nmethod than the same source compiled-for-Circle. Knowing the exact class
statically resolves every self-send (inlined with no guard at all) and turns
instance-variable access into a fixed byte offset. The nmethod's entry guard
re-checks the receiver on each out-of-line call — which is why the guard *is*
the monomorphic inline cache (Part 4).

**3. The live object model.** The compiler runs inside the VM, so real heap
facts embed directly into code: klass oops as pool literals for guards,
nil/true/false as pool words, the smi/Double/Float64x2 klasses for the
numeric fast paths. The GC keeps those pool words current when objects move.

**4. Other methods' bodies.** Callee `MethodOop`s come straight out of the
method dictionaries for inlining, under size/depth budgets, with accessors
and quick-returns recognized specially. Literal blocks compile and **splice
inline** — including conditional non-local returns — rather than allocating
closures.

**5. Its own history and obligations.** Recompilation is driven by *trap
counts* (how often this code's assumptions failed), not invocation counters —
compiled code carries no profiling at all. And every optimization is bounded
by an obligation: at every safepoint the compiler must record where every
interpreter-visible value lives (the deopt scope maps), so any failed
assumption can reconstruct honest interpreter frames mid-flight.

What it deliberately does **not** have: static types (Strongtalk-style
annotations are parsed and discarded), whole-program analysis, external
profiles. What the program *did*, what the heap *is*, and the ability to
*undo* — that triad is the entire adaptive-optimization thesis.

## Part 3 — The pipeline

```
bytecode + IC feedback + key_klass
        │  translate (compiler/ir.rs)
        ▼
   IR (SSA-lite, basic blocks)      sends classified per IC: inline / fuse / call
        │  reduce                    float & vector box/unbox cancellation,
        ▼                            dead-box elimination, temp promotion
   IR (reduced)
        │  regalloc (compiler/regalloc.rs)
        ▼
   linear-scan assignment            spill-all-at-safepoints; residency tier
        │  emit (compiler/emit.rs → JASM)
        ▼
   nmethod                           code + literal pool + oop maps
                                     + deopt scopes + pc descriptors
```

**Translate.** A simulated evaluation of the bytecode: the translator walks
the 31 opcodes keeping a virtual operand stack of vregs, so stack traffic
disappears immediately — `PUSH_TEMP 1; PUSH_SMI 1; SEND #+` becomes one IR
node with two operands. Each `SEND` is classified against its IC feedback
right here: smi arithmetic fuses to `SmiArith` (tag-checked add with an
overflow trap edge), float arithmetic to unboxed `FArith`, vector math to a
NEON `VecArith`, array access to guarded intrinsics, mono sends to inlined
bodies or direct calls, and everything unproven stays a generic `CallSend`.

**Reduce.** Fixpoint rewrites on the IR. The float fast-path's
box-cancellation (`FUnbox(FBox x) → x`), dead-box elimination, and
deopt-sunk boxing mean a chain of float operations boxes once, at the end —
this is what took Mandelbrot's inner loop from 708 MB of allocation to 4 MB.

**Allocate.** Classic Poletto/Sarkar linear scan with one load-bearing
policy: **every value live across a safepoint is spilled to a canonical frame
slot** ("spill-all"). That single invariant is what lets the moving GC and
the deoptimizer read compiled frames without any register maps — the frame
slot is always the truth. The cost (a write-through store per definition) is
bought back by the *residency tier*: call-free spilled values also live in a
callee-saved register (`x21–x27` plus whatever of `x6–x15` is idle), so reads
are register-speed while the slot stays canonical for GC and deopt.

**Emit.** Straight-line lowering through the vendored JASM assembler into a
~35-mnemonic subset of AArch64 (fixed 32-bit instructions — which is what
makes in-place code patching atomic). Alongside the instructions, emit
records the metadata that makes the code *safe*: an oop map per safepoint
(which frame slots hold pointers, for the GC) and a deopt scope per safepoint
(where every interpreter-level value lives, for the deoptimizer).

## Part 4 — What comes out: anatomy of an nmethod

`SendProbe>>bump:` (`^x + 1`), compiled for SendProbe:

```
entry:                                 ; the customization guard = the mono IC
  tst   x0, #3                         ; smi receiver?
  b.eq  miss                           ; smi can never match a heap key klass
  ldur  x17, [x0, #7]                  ; receiver's klass word
  ldr   x16, =SendProbe                ; pool literal (GC keeps it current)
  cmp   x17, x16
  b.eq  verified_entry                 ; hit → run the body
miss:
  ldr   x16, =stub_resolve             ; wrong class → re-resolve the send
  br    x16
verified_entry:                        ; callers that already proved the
  stp   x29, x30, [sp, #-16]!          ; class (PICs) enter HERE
  ...body: tag-check, adds, overflow→trap...
  ret
```

A **monomorphic send site** in some caller is one patched `bl` straight at
`entry` — the six-instruction guard above is the *entire* dispatch cost.
Polymorphic sites get a small per-site PIC stub (up to 4 classes) that jumps
to `verified_entry`, having proved the class itself. And a send the compiler
inlined has **no dispatch at all** — in the hot loops of the benchmarks,
98–99% of executed work runs as compiled, mostly-inlined code, and loops like
`s := self bump: s. i := i + 1` contain literally zero sends: two tagged
adds, a compare, and a safepoint poll (two instructions, dormant).

## Part 5 — Deoptimization: the license for all of it

Everything above is *optimistic*: ICs observe, they don't prove. The reason
optimism is safe is that every compiled frame can be **unmade**:

- every guard's fail edge leads to an uncommon trap;
- the trap's deopt scope says where every interpreter-visible value lives
  (always a frame slot, by spill-all);
- the materializer rebuilds honest interpreter frames from those slots and
  resumes interpreting mid-method, as if the compiled code had never existed;
- repeated traps feed recompilation with better assumptions.

Redefine a method in the browser, load a class that makes a mono site poly,
overflow a smi — the machinery is the same. This obligation is also the
honest cost of the design: the write-through stores and scope metadata are
the "bookkeeping tax" visible in tight loops, and shaving it *without
weakening the obligation* is exactly the ongoing optimization work
(`docs/PERF.md`; the send-path review).

## Coda — why 31 is enough

The instruction set is small because it only has to *describe* programs, not
*optimize* them. Description is stable — 31 opcodes have carried this VM from
the first interpreted expression through NEON vector fusion without gaining a
single opcode. Optimization is volatile — it lives in the compiler, driven by
feedback that changes run to run, method to method. Keeping the two concerns
in separate layers, with deoptimization as the bridge back, is the Self →
Strongtalk → HotSpot lineage in one sentence: **compile the program you
observed, guard the observation, and keep the receipts to take it all back.**

## Cross-references

- `src/bytecode/opcode.rs` — the 31, with encoding notes
- `src/compiler/ir.rs` / `regalloc.rs` / `emit.rs` — translate / allocate / emit
- `docs/PERF.md` — the benchmark arc and methodology
- `docs/float_fastpath_design.md`, `docs/SIMD.md` — the numeric fast paths
- `docs/DEBUGGER.md` — disasm-native, IR dumps, and the tooling used to
  inspect everything this document describes
