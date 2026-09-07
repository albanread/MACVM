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
