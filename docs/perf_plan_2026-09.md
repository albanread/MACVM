# Closing the MACDART gap — a plan from the instruction listings (2026-09-07)

Status: plan. Nothing here is built yet. Every number below was measured today
on the Mac Studio at commit `7a9e4c3`, with MACDART rebuilt from
`dartui-workspace` and Cog 13 as the control; the anatomy counts come from
`rusttcl disasm-native` on the warm nmethods, not from estimates.

## 1. Where we are

Three-way, 7 interleaved rounds, worst per-row MAD 1–5%:

| bench | MACDART | Cog | MACVM | Dart vs MACVM |
|---|--:|--:|--:|---|
| arith | 675 µs | 4868 | 1337 | **Dart 1.98×** |
| fib | 6586 | 17558 | 8495 | Dart 1.29× |
| sieve | 169 | 299 | 161 | MACVM 1.05× |
| dict | 556 | 1140 | 256 | **MACVM 2.17×** |
| alloc | 377 | 695 | 465 | Dart 1.23× |
| richards | 545 | 2075 | 1012 | **Dart 1.86×** |
| deltablue | 255 | 254 | 128 | **MACVM 1.99×** |

The 4–3 split by workload shape is exactly where it was on 2026-08-05. Warm
JIT throughput has not moved in a month; the work since went to the
interpreter (−13–19%, real, but invisible here), boot, and features.
Compiled-code coverage on arith and richards is 99.8% (`PERF.md`,
"Dynamic compiled-code coverage"), so the gap is **codegen quality inside
stable compiled bodies** — nothing about tiering, dispatch or the runtime.

## 2. Why "everything we try" has gone nowhere — the substrate

`regalloc.rs`, D3.5 policy, first sentence:

> every `crosses_safepoint` interval spills unconditionally, whole-lifetime,
> before the main scan even starts — the invariant S12's oop maps stand on
> (registers are never live across a safepoint; maps cover stack slots only)

`verify_spill_all` asserts it in release builds. `assign_residents` then hands
spilled loop-carried values a callee-saved *shadow* register (x21–x27) — but
"the slot stays canonical (write-through)". So a value that crosses any
safepoint lives in a register **and** is stored to its slot at every safepoint
it crosses, so that the deopt materializer can read it back from the slot.

Every micro-optimization tried on top of this substrate was measured to lose,
and was correctly rejected: constant LVN ("an anti-optimization under
spill-all"), the immediate-fold peephole ("even strictly-work-removing folds
lose to regalloc perturbation"), the spill-cost model, inlining depth (level 4:
richards 3× worse — "big spliced bodies lose on slot traffic"). The record's
own conclusion (`regalloc_findings.md` §Next, `macvm-intrinsics-arc`) is that
the slot-traffic arc must come first, and it names the step that "changes the
asymptote": **Stage 4 — deopt environments: record register locations in
pcdescs so deopt reconstructs frames from wherever values live; spilling around
safepoints disappears as a category.**

That step was then **abandoned before being built**, on two premises
(`cog_bench.md`, 2026-08-05): "frame traffic is only 13% of instructions in hot
methods", and "the change needs GC register maps that do not exist". Both are
wrong in a specific, measurable way — §3 and §4.

## 3. The arith loop, instruction by instruction

`BenchmarkDashboard class>>benchArith` is `1 to: 1500000 do: [:i | s := s +
(i * i) - (i * 3)]`. The warm nmethod's hot path, one iteration (`disasm-native`,
offsets `+0x8c..+0x130`):

```
cmp  x23, x24            ; i <= limit            } loop test
b.le
mov  x26, x22            ; copy s                 } COPY (no coalescing)
stur x26, [x29,#-56]     ; write-through          } DEAD on the hot path
asr  x16, x23, #2        ; untag i
mul  x27, x16, x23       ; i*i  (range-proven, no smulh — good)
stur x27, [x29,#-64]     ; write-through          } DEAD
adds x26, x26, x27       ; + ; overflow safepoint
b.vs -> brk
stur x26, [x29,#-72]     ; write-through          } DEAD
movz x27, #0xc           ; 3                      } should be an immediate/shift
asr  x16, x23, #2        ; untag i AGAIN          } redundant
mul  x27, x16, x27       ; i*3
subs x1,  x26, x27       ; - ; overflow safepoint
b.vs -> brk
mov  x22, x1             ; s := result            } COPY
movz x26, #0x4           ; 1                      } should be an immediate
add  x1,  x23, x26       ; i + 1
mov  x23, x1             ; i := result            } COPY
ldr  w16, [x28,#32]      ; back-edge poll         } inherent (Dart polls too)
cbz  x16
b    header              ; unconditional          } loop not rotated
```

**22 instructions; the essential work is ~11** (untag, two muls, add, sub, the
two overflow branches, i+1, test, poll). Ten are the substrate: three dead
stores that exist solely so an overflow *trap* could find `s`, `i*i` and
`s+i*i` in canonical slots; three copies the allocator emits because the slot,
not the register, is canonical; two constants materialized into registers;
one redundant untag; one branch. MACDART's loop is ~11–13 instructions. The
measured 1.98× is this listing.

The three stores are not 13% of the cost. They are stores on the loop-carried
dependency chain, and the copies and rejected peepholes are *consequences of
the same contract* — which is why "13% slot traffic" undercounted: it counted
loads and stores and missed everything the contract forces around them.

## 4. Why the GC objection does not apply to the first slice

Two facts, both already in the tree:

1. **The deopt trap already captures the whole register file.**
   `deopt_trap::capture_regs` copies x0–x28, fp, lr, sp, pc, cpsr into
   `CAPTURED`; `read_captured()` hands them to the handler. The materializer
   simply never learned to look there: `ValueLoc` is `ConstPool | ConstSmi |
   FrameSlot | Nil | ElidedClosure | DoubleSlot` — no register variant.
2. **A safepoint is not a safepoint.** A *call* clobbers registers and lets the
   callee GC while this frame sits suspended — values crossing a call must be
   in slots (Dart 1.24 spills across calls too; fib's reloads after each `bl`
   are the same cost on both VMs). A *trap-only* safepoint (`b.vs`,
   `UncommonTrap`, `brk`) does neither: registers are intact when the handler
   runs, and a compiled frame is never scanned suspended at a trap pc — the
   only way to be at that pc is to have trapped, and then the materializer
   runs first with the values in hand. For **smi** values there is nothing for
   GC to see at all.

So the contract only needs to be split, not abolished: `crosses_safepoint`
becomes `crosses_call` (spill-all stays, `verify_spill_all` stays) and
`crosses_trap_only`. A proven-smi interval that crosses only trap safepoints
keeps its register, its pcdescs say `ValueLoc::Reg(n)`, and the trap
materializer reads `CAPTURED[n]`. No oop map changes. That is **Stage 4a**.

## 5. The plan, in order

Each step lands behind the repo's usual gates: byte-identical differential
(`MACVM_JIT=off` vs `threshold=20` world output), `MACVM_GC_STRESS=full`,
**`MACVM_DEOPT_STRESS=100`** (the load-bearing one here — it forces every trap
site to fire, so every `Reg` location gets materialized), then same-round
interleaved A/B. Cross-day numbers are not evidence: Cog drifted +23% today on
unchanged software.

**4a. Smi register deopt environments.** `ValueLoc::Reg`; scope writer +
materializer; `crosses_call`/`crosses_trap_only` split in `compute_intervals`;
exempt F2-proven-smi trap-only intervals from spill-all; stop the write-through
`stur`s for them in emit. Acceptance test: the arith listing loses its three
dead stores. Expected: arith −15–20% alone.
*Landed 2026-09-07 — `docs/reg_env_findings.md`. The listing lost all three
stores (22 → 19 instructions, 568 → 464 bytes); interleaved A/B: arith −24.8%,
fib −6.8%; everything else inside its noise on two A/Bs. Behind
`MACVM_REG_ENV` (default on), census `MACVM_REG_ENV_COUNT=1`.*

**4a′. Re-gate what the old substrate rejected**, exactly as
`regalloc_findings.md` §Next says: `MACVM_PEEP_IMM=1` (landed, default-off,
"the natural companion of the regalloc rework — re-gate it then"), then
copy coalescing for loop-carried phis (there is none today; `mov x26,x22 … mov
x22,x1` is the whole reason `s` needs a temp), the duplicate-untag CSE, loop
rotation. Each is 1–3 instructions of the 22; together with 4a the loop is
~13–14. **Target: arith 1.98× → ~1.3×.**
*Tried 2026-09-07, same day — `docs/reg_env_findings.md` §4a′. The listing
went 19 → 15 with zero stores; the A/B moved arith −0.2%. It does not
compound: the loop is bound by two taken branches per iteration, not by
its instruction count. Landed behind `MACVM_PEEP4`, default off. The rung
that is left standing is loop rotation.*

**3. Frame-init by liveness + fib's small change.** `entry_early_defs` exists
and "isn't firing" — find why; fib nil-fills four slots per call for two that
no safepoint can observe uninitialized. Add: DCE of the dead `ifTrue:` merge
value (`ldr x0, nil; mov x1, x0` — 2 insns/call), immediates for `n-1`/`n-2`,
and **NLR-check elision at direct calls to NLR-free callees** (`sub x17,x0,#6;
cbz` after both sends: 4 of ~60 insns per call, and fib: cannot NLR — a per-
nmethod bit the direct-call site can consult). **Target: fib 1.29× → ~1.1×.**
Fib is otherwise at its floor: the send is already a direct `bl` to its own
verified entry.

**4b. Oop register deopt environments.** Same split for oops crossing
trap-only safepoints; the materializer stages register-sourced oops into a
rooted scratch array *before* it allocates (a `Context`, an elided closure), so
a GC inside materialization cannot lose them. Still no compiled-frame oop-map
change — those describe suspended-at-call frames, which keep spill-all.
Richards' hot bodies have 2–6 `bl`s but 8–24 tag tests and 3–15 klass loads
each: most of their remaining write-throughs sit at guard/trap safepoints, not
calls, and the "inlined-accessor ritual — 7–9 insns where Dart spends 1" is
mostly write-through + copy. **Target: richards 1.86× → ~1.5×.**

**Then, and only then**, the parked inlining depth (processWork:-into-runTask)
gets its A/B back on the new substrate. Not before — the record shows it loses
on slot traffic, and 4a/4b are what remove the slot traffic.

## 6. What this does not promise

Parity on arith and richards needs what MACDART has and tier-1 does not: a
full SSA optimizer — type propagation that hoists the guard chains, range
analysis over loops, LICM, a linear scan that never spilled-all in the first
place. That is the roadmap's tier-2 ("a big project and not urgent while
tier-1 is already strong"). The plan above is the largest step available
*inside* tier-1, and it is the step the codebase's own findings already named;
it was skipped on a miscount, not on a measurement. If it lands as sized —
arith ~1.3×, richards ~1.5×, fib ~1.1×, alloc unchanged at 1.2× — the board
reads MACVM ahead on three, MACDART ahead on four by ≤1.5×, with no row
outside 1.5× either way. That is the realistic end state for tier-1.

## 7. Tooling to keep this honest

The measurement that produced §3 is the one that should gate every step: warm
the bench under the JIT, `disasm-native` the hot nmethod, count instructions
by category on the hot path. It took one `rusttcl` script:

```
load warm.mst          # 40× benchArith, 6× benchFib
nmethods
disasm-native "BenchmarkDashboard class" benchArith
disasm-native "BenchmarkDashboard class" fib:
```

Worth landing as `scripts/bench-anatomy.sh` so "did this change remove the
instructions it was supposed to?" is a diff, not an argument.
