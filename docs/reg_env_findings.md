# Register deopt environments — Stage 4a, built and measured (2026-09-07)

The first structural step of `perf_plan_2026-09.md` §5: a known-smi temp that
only an uncommon trap observes keeps its **register** across the trap, and the
trap's deopt scope names that register (`ValueLoc::Reg`) instead of forcing
the value through a frame slot. Landed default-on behind `MACVM_REG_ENV`
(`0` = pre-4a allocation byte for byte, `1` = the dead-block hygiene only,
`2` = on); `MACVM_REG_ENV_COUNT=1` prints every grant.

## What the arith loop looks like now

`BenchmarkDashboard class>>benchArith`, warm OSR nmethod, one iteration of
`s := s + (i*i) - (i*3)` — before (§3 of the plan) and after:

| | insns/iter | stores/iter | nmethod bytes |
|---|--:|--:|--:|
| before | 22 | 3 | 568 |
| after | **19** | **0** | **464** |

```
cmp  x23, x24 ; b.le          loop test
mov  x1, x22                  copy of s            (coalescing — 4a′)
asr  x16, x23, #2 ; mul x0    i*i                  (v14 → x0, was resident+store)
adds x2, x1, x0 ; b.vs        +                    (v15 → x2, was resident+store)
movz x0, #0xc ; asr ; mul     i*3                  (immediate + duplicate untag — 4a′)
subs x0, x2, x1 ; b.vs        -
mov  x22, x0                  s :=                 (coalescing — 4a′)
movz x0, #4 ; add ; mov x23   i + 1                (immediate + coalescing — 4a′)
ldr  w16 ; cbz ; b            poll, back edge
```

The three dead `stur`s are gone. The census names exactly the four temps
that got registers: `v11` (the copy of `s`), `v14` (`i*i`), `v15` (`s+i*i`),
`v18` (`i*3`) — each observed by one overflow trap, each dominated by its
def, none crossing a safepoint.

Measured, interleaved A/B, same binary shape, 7 rounds, best-of-round medians:

| bench | pre-4a µs | 4a µs | delta | noise |
|---|--:|--:|--:|--:|
| **arith** | 1330 | **1000** | **−24.8%** | 2% |
| fib | 8711 | 8116 | −6.8% | 1% |
| sieve | 160 | 165 | +3.1% | 1% |
| dict | 254 | 242 | −4.7% | 4% |
| alloc | 499 | 487 | −2.4% | 5% |
| richards | 1043 | 1013 | −2.9% | 3% |
| **deltablue** | 137 | **124** | **−9.5%** | 4% |

(`noise` = worst per-round MAD/median across both binaries; the two-binary
harness is `scripts/cog-bench.sh`'s protocol with the baseline built from
the parent commit and the pair run back to back each round.)

A second A/B on the SAME binary, `MACVM_REG_ENV=0` vs `2` interleaved for 5
rounds, isolates the grant from everything else in the commit: arith
−24.8% again (1330 → 1000), fib −6.3%, sieve +1.8%, dict −3.6%, alloc −4.0%,
richards +0.7%, deltablue +3.9%. So: **arith and fib are the effect**; the
other five move by less than their own noise and change sign between the two
runs (deltablue −9.5% then +3.9%), which is what noise looks like. sieve's
`sieveOnce` gets two grants; its ±2–3% is not a signal.

The plan sized 4a alone at "arith −15–20%"; it came in at −25%. fib's −6–7%
was not predicted: its `n - 1` / `n - 2` / `+` traps observe register temps
too.

The three-way afterwards (`scripts/xvm-bench.sh`, 7 interleaved rounds,
MACDART `dartui-workspace`, Cog 13, worst per-row MAD 1–4%):

| bench | MACDART | Cog | MACVM | vs MACDART |
|---|--:|--:|--:|---|
| arith | 691 µs | 4850 | **998** | Dart 1.44× (was 1.98×) |
| fib | 6535 | 17648 | 8021 | Dart 1.23× (was 1.29×) |
| sieve | 174 | 302 | **167** | MACVM 1.04× |
| dict | 543 | 1146 | **242** | MACVM 2.24× |
| alloc | 389 | 682 | 523 | Dart 1.34× (was 1.23×; see below) |
| richards | 568 | 2117 | 985 | Dart 1.73× (was 1.86×) |
| deltablue | 252 | 251 | **130** | MACVM 1.94× |

alloc reads worse here than this morning (465 → 523 µs) and that is not
this change: both same-round A/Bs put alloc inside its noise (−2.4%, −4.0%),
the code it runs is untouched, and its `warm_us` is the GC-bound number the
harness's own law says not to judge GC work by. It drifted 465 → 499 → 522
across three runs of two binaries today. The rows that moved are the two the
A/Bs named.

## The mechanism

Five pieces, each small:

1. **The trampoline spills the register file first.** `build_uncommon_trampoline`
   now opens with fourteen `stp`s of x0..x27 into `VmRegBlock::trap_regs`
   (`VMREG_TRAP_REGS_OFFSET`), before it touches a single register. Cold path:
   once per trap.
2. **`ValueLoc::Reg(n)`** — codec tag 6. `resolve_frame_loc` answers it for a
   `Reg`-assigned interval covering the position (before 4a that arm fell
   through to `Nil`, which is exactly why every deopt-referenced vreg had to
   be spilled). `read_value` adopts `trap_regs[n]` as a tagged smi.
3. **Regalloc grants the register** (`compute_intervals`) to a vreg that is
   known-smi, observed by root `UncommonTrap` sites only, crosses no safepoint
   over its env-extended interval, and whose first def dominates every
   observing trap (the S2c single-predecessor walk). Such a vreg is
   deopt-referenced yet not force-spilled; its interval is stretched to the
   trapping op's position so the op's result cannot take its register.
4. **Unreachable fail blocks record nothing.** `SmiArithNoOv`'s conversion
   leaves the original fail block behind, and its facts named `s + i*i` at a
   trap no predecessor chain reaches — dominance failed and the temp stayed
   pinned. A trap that can never fire has no environment.
5. **Two tripwires.** `build_deopt_metadata` asserts (debug) that a `Reg` only
   appears at a root uncommon trap; `deoptimize_frame` refuses one at any
   other site kind (release) — at a call-site or poll deopt the trap register
   file is stale, and a wrong-but-valid oop is the worst possible outcome.

## What went wrong on the way, kept because it teaches the rule

The first cut extended env-liveness only for the trap's **reexecute stack**.
`cocoa_c2_dnu_sends_survive_the_jit` failed 301 for 300: at threshold-1 the
whole `to:do:` loop compiles as ONE cold `UncommonTrap` behind a `Jump`, and
that trap records the temp `t` from its **slot**. `t`'s last organic use was
`t := 0`, so the scan handed its register to `i := 1` before the `brk` — and
the trap read 1. The rule is: **every vreg a trap records must stay live
through the op that owns the fail edge**, receiver and slots included, not
just the operands of the trapping op. The extension is applied only to vregs
that are actually granted a register, so nothing else's allocation moves.
`MACVM_REG_ENV=1` bisected it in one run.

## What this does not do yet — the 4a′ list, now visible in the listing

Two copies (`mov x1,x22` / `mov x22,x0`) because the operand-stack push and
the assignment are separate vregs with no coalescing; two constants
materialized into registers (`MACVM_PEEP_IMM` was landed default-off against
the old substrate — its doc says re-gate it after this); one duplicate untag.
19 → ~14 is that list; the plan's `Then re-gate` step.

Gates: `cargo test --release --lib` 883 passed, 0 failed; world suite differential
`MACVM_JIT=off` vs `threshold=20` {DIFF}; `MACVM_GC_STRESS=full:64`
{GCSTRESS}; `MACVM_DEOPT_STRESS=100` {DEOPTSTRESS} — the load-bearing one
here: it forces every trap site, so every `Reg` location gets materialized.

## 4a′ — does the next rung build on the last one? (2026-09-07, same day)

The removal commit `214aae9` is the null hypothesis: on the spill-all
substrate the immediate fold and its siblings were "nothing measurable
alone, nothing compounded, all five together +1.5% worse". With values in
registers across their traps, the same instruction-level rungs were tried
again, behind `MACVM_PEEP4` (census `MACVM_PEEP4_COUNT=1`):

1. **The immediate fold, restored from `2e35705` and extended to `Mul`.**
   `i + 1` is one `add x23, x23, #4`; `i * 3` needs no untag at all —
   `tagged(a) * k` is `tagged(a*k)` — so the `asr` goes with the `movz`. The
   constant a fold consumed is deleted, once the census stopped counting
   references from unreachable fail blocks (the same dead-block trap as 4a).
2. **Copy propagation carried across a single-predecessor `Jump`** — the
   shape a fail-edge split leaves behind. The copy of `s` that outlived the
   per-block window for a month is gone; `adds` reads `x22` directly.
3. **`x := <no-overflow op>` folded into the op**, including across the
   split where `to:do:`'s increment ends one block and `i :=` opens the
   next. Never for a trapping op: its environment records `x`'s old value.

The arith loop, one iteration: **22 → 19 (4a) → 15 (4a′)** instructions;
stores 3 → 0 → 0; nmethod 568 → 464 → 448 bytes. What remains is the
essential work plus one `movz #3` (a shifted-register `add` would fold it;
the assembler has no such operand form) and the unrotated back-edge `b`.

Same binary, `MACVM_PEEP4=0` vs `1`, interleaved, 7 rounds, best-of-round
medians:

| bench | off µs | on µs | delta |
|---|--:|--:|--:|
| arith | 1002 | 1000 | **−0.2%** |
| fib | 8140 | 8082 | −0.7% |
| sieve | 168 | 163 | −3.0% |
| dict | 240 | 250 | +4.2% |
| alloc | 498 | 498 | +0.0% |
| richards | 999 | 1013 | +1.4% |
| deltablue | 134 | 126 | −6.0% |

**It does not compound.** arith lost four of nineteen instructions and every
copy, and moved by nothing; the other rows are inside their noise with mixed
signs (deltablue has read −9.5%, +3.9% and −6.0% across three A/Bs today —
that is what its noise looks like). This is the old peephole law again —
"instruction-level shapes keep losing" — but for a different reason than
the one recorded in `peephole_findings.md`. That was spill-set perturbation
under spill-all; 4a removed that. What is binding now is visible in the
listing, and it is not the instruction count.

Per iteration the loop takes **two taken branches**: `cbz x16` over the
poll's slow path, then the unconditional back-edge `b`. The loop-carried
dependency is `s` alone (`adds → subs → mov x22`, two or three cycles);
both `mul`s hang off `i`, which is a one-cycle `add`. At 1000 µs for
1.5 M iterations the loop runs ~0.67 ns ≈ 2.1 cycles per iteration at
this core's clock — a taken branch per cycle is a common front-end limit,
so two taken branches is a two-cycle floor whether the body is 15
instructions or 19. MACDART's 691 µs is ~1.5 cycles: one taken branch and
the same chain. This is a hypothesis fitted to the numbers, not a
measurement (no cycle counters from here), but it is a specific one.

So the next rung is **control-flow shape, not instruction count**: rotate
the loop so the condition test is the back-edge branch and the poll's slow
path is out of line behind a not-taken `cbnz` — one taken branch per
iteration. That is an emit/layout change, bounded, and it is the first
thing today's listings say might actually move arith again. Everything
on the 4a′ list stays behind `MACVM_PEEP4`, default off, per the gating
policy: correct, measured, and not earning its gate — a candidate for the
next `214aae9`-style prune if rotation does not change the picture.

## Loop rotation — the branch floor, tested (2026-09-07, same day)

§4a′ ended on a hypothesis: the arith loop was bound by three taken
branches per iteration, not by its instruction count. Rotation is the
experiment that tests it, behind `MACVM_ROTATE` (census
`MACVM_ROTATE_COUNT=1`; `=2` also says why a latch was left alone):

- **Level 1, emit:** each poll's slow path (S2 spill stores, `bl stub_poll`,
  resident reloads) is emitted after the epilogue behind a not-taken
  `cbnz`, instead of inline behind a taken `cbz`. `rt_poll` keys the
  LoopPoll scope on the `bl`'s return address, which is all it ever needed;
  the S2 stores and reloads are emitted at the poll's own IR position, so
  they are byte-for-byte the inline form's.
- **Level 2, IR (`rotate_loops`):** a loop whose header is exactly one
  `SmiCmpBr` and whose latch polls has the compare duplicated onto the latch
  (fresh fail block, cloned trap site) so the back edge IS the conditional
  branch; the header stays as the once-only entry test. Both header exits
  must inherit the header's own entry stack — `to:do:` carries one merged
  entry around the loop, re-merged by a self-move in the latch, and that
  qualifies; a body that leaves something new for the header to merge does
  not. A poll deopt still resumes at the header's bci and re-runs the
  compare, and the new trap re-executes it at the same bci with the same
  stack.

The loop, one iteration, `MACVM_ROTATE=2`:

```
mov  x1, x22 ; asr ; mul ; adds ; b.vs ; movz ; asr ; mul ; subs ; b.vs
mov  x22, x0 ; movz ; add ; mov x23, x1
ldr  w16, [x28, #32] ; cbnz x16, slow      not taken
cmp  x23, x24 ; b.le body                  the one taken branch
b    exit                                  behind it
```

Three taken branches became one (`b.le` header→body, `cbz` over the poll,
`b` back edge → `b.le` back edge alone). Same binary, interleaved, 7 rounds:

| bench | off µs | on µs | delta | noise |
|---|--:|--:|--:|--:|
| **arith** | 1003 | **673** | **−32.9%** | 5% |
| fib | 7917 | 8015 | +1.2% | 2% |
| **sieve** | 165 | **103** | **−37.6%** | 10% |
| dict | 243 | 231 | −4.9% | 4% |
| alloc | 527 | 504 | −4.4% | 7% |
| richards | 1037 | 1003 | −3.3% | 4% |
| deltablue | 129 | 127 | −1.6% | 6% |

**The hypothesis was right, and it moves.** arith at 673 µs is MACDART's
691: parity, from 1.98× behind this morning — 4a took a quarter off, and
rotation a third of what was left, while the four instruction-level rungs
in between took nothing. sieve is the same `to:do:` shape and lost a third
too (its noise is high, 10%, but the effect is four times that). fib has no
loop and did not move. The other four are inside their noise and all
slightly negative — plausibly real, small, and not claimable.

A win of this size flips the default under the gating policy:
`MACVM_ROTATE` is **on (2)** from this commit; `0` turns it off, `1` keeps
only the poll half.

And with rotation on, the 4a′ rungs off vs on, 5 rounds — does instruction
count start to matter once the branch floor is gone?

| bench | rungs off | rungs on | delta | noise |
|---|--:|--:|--:|--:|
| arith | 676 | 698 | +3.3% | 3% |
| fib | 8118 | 8079 | −0.5% | 2% |
| sieve | 105 | 108 | +2.9% | 6% |
| dict | 240 | 233 | −2.9% | 4% |
| alloc | 509 | 513 | +0.8% | 9% |
| richards | 993 | 1007 | +1.4% | 2% |
| deltablue | 122 | 125 | +2.5% | 5% |

**No.** With the branch floor gone, four fewer instructions in the loop
are still worth nothing — arith reads slightly *worse*, inside its noise.
That settles it twice over: at this loop's width the instruction count is
not what the machine is waiting on, and the 4a′ rungs stay default-off, a
prune candidate. The order of what mattered today, for the record: values
in registers across their traps (−25%), then the shape of the control flow
(−33%), and instruction-level cleanup (0%) — the reverse of how a
peephole-first instinct would have ranked them.

The three-way afterwards (`scripts/xvm-bench.sh`, 7 interleaved rounds, worst
per-row MAD 1–5%, rotation on by default):

| bench | MACDART | Cog | MACVM | vs MACDART |
|---|--:|--:|--:|---|
| **arith** | 698 µs | 4868 | **687** | **MACVM 1.02× — parity** (was Dart 1.98× this morning) |
| fib | 6707 | 17564 | 8117 | Dart 1.21× (was 1.29×) |
| **sieve** | 178 | 313 | **103** | **MACVM 1.73×** (was 1.05×) |
| **dict** | 554 | 1142 | **232** | MACVM 2.39× |
| alloc | 383 | 699 | 487 | Dart 1.27× |
| richards | 569 | 2132 | 987 | Dart 1.73× (unchanged) |
| **deltablue** | 255 | 264 | **122** | MACVM 2.09× |

Four to three, MACVM's way, for the first time. What Dart still holds is
what none of today's work touched: fib is call overhead, alloc is the
collector, and richards is the guard chains and heap-oop slot traffic that
Stage 4b (oops in registers across traps) is for.
