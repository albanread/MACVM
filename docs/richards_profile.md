# Richards, re-profiled after the F-arc (fc8232e, 2026-07-26)

Richards is the widest row vs Dart V1: 16.5 ms vs 4.9 ms = **3.36×**.
The last profile (pre-F-arc, budgeted_inliner_design.md §"Post-I3
profile") could only say "flat JIT + rt_call_primitive ~5%". This one is
the first with per-nmethod attribution, via the new `MACVM_TRACE=nmmap`
exit dump joined against `sample(1)` (same process: 20 000
`benchRichards` calls ≈ 33 s, 15 s sampled, 11 938 top-of-stack
samples).

## Where the time goes (self-time)

| share | where |
|---|---|
| 28.6% | `RichardsBenchmark>>schedule` |
| 32.0% | `processWork:` ×4 (Handler 14.3, Idle 7.7, Device 7.3, Worker 3.7) |
| 10.7% | `runTask` ×4 (Device 5.6, Handler 4.2, Idle 0.9, Worker 0.8) |
| 8.2% | `RichardsBenchmark>>queuePacket:` |
| ~7% | `release:`, `append:head:` ×2, `addInput:checkPriority:` ×3, `taskWaiting:` ×2 |
| ~4.5% | JIT-unattributed cluster (PIC/dispatch stubs — addresses outside every nmethod) |
| ~3.1% | the `//` chain: `SmallInteger>>//` nmethod → `rt_call_primitive` → `prim_div` |
| ~0% | GC, interpreter |

Machinery is CLEAN: the c2i census is **empty** (zero interpreter
entries), and 500 iterations produce exactly 2 traps
(`IdleTask>>processWork:` bci=30, recompiled once to a v1 that holds).
No storms, no adapter overhead, nothing left to heal. The entire 3.36×
lives inside stable compiled bodies.

## Anatomy of the hot bodies (disasm-native, four bodies)

| body | insns | slot ld/st | field ld/st | bl | tag tst | klass ld |
|---|---|---|---|---|---|---|
| `schedule` | 284 | 21/40 (**21%**) | 15/4 | 4 | 8 | 4 |
| `HandlerTask>>processWork:` | 460 | 60/44 (**23%**) | 36/6 | 6 | 24 | 15 |
| `queuePacket:` | 202 | 21/24 (**22%**) | 11/3 | 2 | 8 | 3 |
| `IdleTask>>processWork:` | 304 | 32/58 (**30%**) | 18/4 | 6 | 17 | 8 |

Reading:

1. **The send-bound era is over.** 2–6 `bl` per body; the inliner +
   SameTargetPoly machinery has swallowed the accessors — the disasm
   shows membership-guard chains whose fast legs are inline field
   loads, with the `bl` only as the unseen-klass fallback. Richards is
   now **data-movement-bound**.
2. **Slot traffic is 21–30% of every body.** Every heap-oop
   intermediate (self, tasks, packets, accessor results) is
   written through to its frame slot and reloaded per use — the
   spill-all-at-safepoints contract. S2 lifted this for smis, the
   float arc for doubles; **heap oops are the unlifted class** (their
   slots are what the oop maps scan, so residency needs GC-visible
   registers). The opening of `schedule` is the emblem: `self` is
   stored to `[x29,#-8]` and reloaded from it twice within the next
   four instructions.
3. Of that traffic, only ~10–16 ops/body are provably-local
   redundancies (same-slot store→reload within 3 insns, no branch or
   call between; plus same-slot repeat reloads within 6). A
   slot-aware `resolve()` cache would kill those cheaply — the other
   ~75% of the churn crosses real safepoints and needs the
   architectural fix.
4. **Inlined-accessor ritual ≈ 7–9 insns** where Dart spends 1:
   klass-load + compare chain + inline `ldur` field load + `mov` +
   slot write-through of the result + rejoin branch. The guard chain
   is the price of dynamism; the write-through + `mov` are ours.
5. **Heap-constant shuffles**: `true` is loaded from the pool, moved
   through three registers, and stored to two slots (5 insns) in
   `schedule`'s hottest path. F3 elides ConstSmi slots only — the
   same analysis over **pool-literal constants** (true/false/nil,
   Symbols) is sitting right there.
6. **`//` is a real send**: `control // 2` goes send →
   `SmallInteger>>//` nmethod → prim shim → Rust `prim_div` — ~3.1%
   of the bench for what `sdiv`+floor-correction does in ~4 inline
   instructions.
7. The unattributed ~4.5% is the PIC-stub cluster: `runTask` /
   `taskWaiting:` sites are polymorphic-with-DIFFERENT-targets across
   the four task klasses, so SameTargetPoly cannot apply and they
   dispatch through stubs. Real per-arm poly inlining (the inliner's
   I-series continued) is the only lever there.

## Ranked levers

1. **R1 — heap-oop slot traffic (the jaw of this profile, 21–30% of
   every hot body).** Two rungs:
   - **R1a (cheap, contract-preserving)**: slot-aware `resolve()` —
     track "slot K is currently in register R" per straight-line
     region, invalidated at safepoints/calls/kills; kills the
     store→immediate-reload and repeat-reload patterns (~10–16
     ops/body) plus the accessor-result round-trips inside regions.
   - **R1b (architectural, the Dart-gap closer)**: heap oops resident
     across safepoints — needs oop maps that can name registers (or a
     GC-walked register spill area, the RootSpill idea generalized).
     This is S2's third act: smis (S2), doubles (float arc), oops.
2. **R2 — ConstOop slot elision**: extend F3's const-uniform analysis
   from ConstSmi to pool-literal oops (true/false/nil/Symbol);
   rematerialize with one pool `ldr` at use. Directly observed 5-insn
   shuffles in the hottest body; also shrinks oop maps.
3. **R3 — smi `// ` and `\\` fuse**: `sdiv` + floored-division
   correction inline, guard-free under F2-proven smi-ness. ~3% of
   richards; also shows in sieve/dict via index math.
4. **R4 — per-arm poly inlining** for different-target poly sites
   (`runTask`, `taskWaiting:`): inline the top-count arm(s) under the
   existing membership-guard machinery, stub fallback for the rest.
   Addresses the ~4.5% stub cluster and richards' remaining dispatch
   smear.

R1a+R2+R3 look like a −10–15% richards slice without touching the GC
contract; R1b is the arc that changes the asymptote (and would move
every bench, fib included — its write-throughs are param/self oops).

## Tooling landed with this profile

- `MACVM_TRACE=nmmap`: per-nmethod `base end Klass>>sel vN state` at
  process exit — joins `sample(1)` anonymous code-cache addresses to
  methods (same-process only).
- `rusttcl` `nmethods` now prints `base=` for the same purpose live.
- Fixed: `flag jit <mode>` in a `rusttcl` session that booted with the
  JIT off now arms the SIGTRAP deopt handler; previously the first
  organic deopt killed the shell (exit 133).

## R1a landed — slot-aware resolve (shadow residency in free pool registers)

The mechanism: pool registers (x21–x27) that `assign_residents` left
unassigned this method are written by NOTHING in the body — so `commit`
mirrors its write-through value into the slot's (fixed, modulo-chosen)
free register, and `resolve` serves later reads from it with zero
instructions. Slots stay canonical: every store still happens, GC/deopt/
oop maps see the exact pre-R1a frame; only redundant loads disappear.
Cleared at block starts (joins), before every `is_safepoint_op` (its
callee uses the pool registers as its own residents, and its GC rewrites
slots), and at the OSR tail. `MACVM_R1A=0` opts out; `MACVM_R1A=2` is
the differential checker (every hit also loads the slot and `brk
#0xC1A0`s on disagreement).

Two real bugs found en route, both now structural guards:

1. **Seed-clobbers-outstanding-hit** (the sieve infinite loop): operand
   a's hit returned x27, then operand b's promotion `mov x27, x17`
   retargeted it before the op's `adds` consumed it — computing b+b.
   The poison checker COULDN'T see this (the shadow's value was right;
   the caller's operand register was the casualty); lldb on the hung
   loop named it in one attach. Guard: `r1a_hits_live` — no seed may
   retarget a register handed out as a hit in the current op.
2. **Dead loop-tail seeds** (arith +3%/sieve +8% in the first A/B):
   every in-loop commit seeded, and the header's block-start clear
   killed the entry before any use — one dead `mov` per iteration.
   Guard: `r1a_profitable_positions`, a prepass over the shared
   emit/regalloc numbering marking exactly the positions with a later
   same-region read of the same slot; both seed sites consult it.
   Plus: no seeds at all inside a safepoint op's own lowering (a
   pre-`bl` seed would serve a pre-GC value afterwards).

Measured (four interleaved A/Bs vs fc8232e, best-of, load-gated):
**richards −6.5..−8.9% (16.6 → ~15.0-15.5 ms)**, **dict −2.9..−5.7%**,
**deltablue −2.0..−3.5%**, fib/arith/alloc flat; sieve +4% on the final
layout (codegen structurally identical, 288 vs 286 insns — the
alignment-bimodality class fib showed in F2; it swung −5.9..+7.9 across
builds all day; alignment padding remains the standing de-noising
lever). Gates: checksums both thresholds, tier1 104/0, it_gc_jit
(revived — its build had been broken since F3's signature change),
focused debug suites, GC_STRESS both flavors, poison-mode × GC_STRESS,
R1A=0 opt-out, GUI render boot. Known pre-existing failure documented
en route: DEOPT_STRESS=64 aborts ~7/8 runs at deopt.rs:665 ("root-block
scope's receiver ValueLoc must hold the closure") on BASE and R1a trees
alike — task-flagged, not this slice's regression.

R1a is the first rung of R1; **R1b (heap oops resident across
safepoints via register-aware oop maps) remains the asymptote-mover.**

## R2 landed — ConstOop slot elision (flat on micros; the value is GC-side)

F3's sibling, wired the same way: `ir::const_pool_vregs` (every def the
SAME `ConstPool` literal, all-defs poison rule), `resolve_frame_loc` →
`ValueLoc::ConstPool(pool word index)` — the LAST untriggered S13
materializer arm now has a producer (`read_pool_oop` reads the LIVE
nmethod pool word, so a post-GC materialization sees the relocated
oop) — oop maps skip the never-written slots, commit skips the store,
resolve/reloads rematerialize an `ldr` from the pool, S2/R1a exclude
the class, and call marshalling gains `ArgSrc::Lit` (no source
register, can never join a shuffle cycle). `emit` now returns the
PoolLit→LiteralId map (dense id order == pool word order) so the
driver can record the index deopt expects.

Census (MACVM_S2_COUNT): richards 201 pool-const vregs / 23 spilled,
deltablue 472/49, sieve 40/24, dict 47/12, fib 2/0.

Two falsifications recorded on the way to the final shape:

1. **The marshalling `Lit` arm must be Spill-guarded.** The first cut
   fired it for Reg-assigned const args too, replacing a free register
   rename (`mov`) with a pool LOAD at every call site — a uniform
   +1.4..+4.6% regression concentrated on the LOW-spill benches (fib,
   with zero spilled consts, regressed ~+1.6% twice). Pool loads only
   where the alternative was a slot load.
2. **Pool remat is not movz.** F3's rematerialization costs no memory;
   R2's `ldr` from the nmethod pool trades the hottest line in the
   machine (the frame) for a colder per-method pool line. Measured
   consequence: the spilled elisions are cycle-NEUTRAL (5-round A/B vs
   R1a, all seven within ±1.6%, no reproducible win or loss) — the
   stores saved pay for the colder loads, no more. What remains is
   real but not on the clock: 23-49 fewer GC-scanned slots per hot
   bench at every safepoint, smaller interned oop maps, and the
   materializer arm coverage.

Gates: checksums both thresholds, tier1 104/0, it_gc_jit, focused
suites, GC_STRESS ×2, poison×GC_STRESS, GUI render boot (DEOPT_STRESS
still trips the pre-existing deopt.rs:665 block-receiver abort — on
base identically). Verdict: LANDED default-on as sound machinery, with
the honest note that the profile's estimate for R2 ("5-insn shuffles")
over-credited it — the observed schedule shuffles turned out to be
csel-materialized CONDITIONAL booleans (two defs — correctly outside
the all-defs rule). The clock levers remaining from this profile are
R3 (smi `//` fuse) and R4 (per-arm poly inlining); R1b stays the
asymptote-mover.
