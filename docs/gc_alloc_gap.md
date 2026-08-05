# The allocation gap vs Cog — root cause and ranked fixes

Status: investigated 2026-07-22, no fixes built. The matched-source Cog
comparison (Pharo 13.1/Cog, identical checksums; see the benchmark story in
`docs/deopt_fixes.md`'s sibling investigations) left ONE bench to Cog:

    alloc x5 (200k Association chain): Cog 7 ms, MACVM 46 ms  (6.6x)

The headline of the investigation: **it is mostly not the GC.** The gap
decomposes into three independent costs, separated by a 2x2x2 experiment
matrix — inline-fused vs send-path allocation, surviving vs dying objects,
4 MB vs 32 MB eden:

| variant (ms per 5x200k) | eden 4 MB | eden 32 MB |
|---|---|---|
| chain — send-alloc, survives (THE bench) | 44 | 32 |
| chainFused — inline-alloc, survives | 16 | **2** |
| dead — send-alloc, dies immediately | 31 | 31 |
| deadFused — inline-alloc, dies | 3 | 2 |

**Bound: chainFused @ 32 MB eden = 2 ms vs Cog's 7 — with both pathologies
removed, MACVM is 3.5x FASTER than Cog on this bench.** The collector's
core is competitive; the losses are a compiler gap and a configuration gap
stacked together.

## Cost 1 (~65%): the allocation SEND path, not collection

`Association key: i value: last` allocates via `self basicNew` inside the
class-side constructor, which compiles as a generic
`CallSend -> shim -> rt_call_primitive -> prim_basic_new` — ~28 ns/object.
The `dead` row (31 ms) is invariant to eden size: zero GC involvement.
The fused literal `X basicNew` (the `Ir::Alloc` inline eden bump) costs
2-3 ns. `ir::alloc_site_klass` only fires for a literal class-constant
receiver, so every real constructor in the world — `key:value:`, every
`X new` -> `self basicNew` chain — misses the fusion.

**Fix:** extend the alloc fusion to customized `self basicNew`: under
customization the receiver klass IS the metaclass, whose sole instance
(the class) and fixed instance size are compile-time derivable. Wide
payoff — accelerates essentially all object construction. Medium effort.

## Cost 2 (~30%): nursery geometry

`DEFAULT_EDEN_SIZE` = 4 MB; `SURVIVOR_SIZE` = 512 KB (layout.rs). The
bench's live chain is 4.8 MB — larger than eden — so every mid-build
scavenge overflows the survivor space and the designed cascade promotes
nearly everything: 65% of ALL allocated bytes were promoted; the promoted
chain then dies IN OLD SPACE, old fills with garbage, and full GCs fire
(7 per baseline run, 4.8 ms max pause). At `MACVM_EDEN=32768` (the env
var takes KiB) the whole cost class vanishes.

**Fix:** raise the default eden (16 MB ≈ +24 MB/VM covers this class),
and/or adaptive nursery growth on a promote-storm signal (promoted-bytes
ratio per scavenge is already in gc_stats). Trivial-to-small effort.

## Cost 3 (~2x residual): scavenge mechanics at matched geometry

~1.9 GB/s effective. The copy loop itself is sound (`copy_nonoverlapping`
bulk copy + forwarding install, scavenge.rs), but each PROMOTED object
pays: an individual `old.allocate` call, a per-object
`cards.record_multistores` range, two validated `MemOop::try_from`s, and
age-table bookkeeping. Batching (one old-space carve per scavenge,
coalesced card ranges) closes most of it. Fix AFTER 1+2 — it only shows
once they are gone.

## Recommended order

1. Eden default bump (one line + the standard battery).
2. Customized-`self basicNew` alloc fusion (the big general win).
3. Scavenge batching (secondary).

---

## Outcome (same day): both fixes landed, gap closed to a TIE

- `40fc343` — default eden 4 -> 16 MiB (cost 2).
- `ad2846f` — `alloc_site_klass_on`: spliced constructors' `self basicNew`
  fuses to the inline eden bump in both in-body splice walks (cost 1; the
  constructor pattern routes through the CFG splicer — its `| a |` temp
  makes the nonleaf splicer decline, which is where a first attempt
  silently missed; found by probing the check, not by re-reading it).

Measured end state (matched-source harness): **alloc x5 = 7 ms on BOTH
VMs — a dead tie**, from 46 ms (6.6x behind). The full comparison now
reads: MACVM wins or ties every bench. Cost 3 (scavenge batching) was
never needed for parity and remains unimplemented.

## 2026-08-06 — the scavenger-throughput root cause: SURVIVOR_SIZE

The Z arc retired the allocation call-out (prim_basic_new no longer
appears in benchAlloc's profile); the residual alloc gap vs MACDART
(~575 vs ~380 µs) is collector time, and the counters name the cause.

**Measured, 3000 × benchAlloc (MACVM_TRACE=gc):** 572 scavenges and
**98 full mark-slide-compact collections** — one full GC per ~30
iterations of an allocation bench. Scavenge lines read
`copied 0–512K, promoted 1924–4960K`: survivors bypass young almost
entirely. The tenuring threshold oscillates 127 ↔ 1 (never-tenure ↔
tenure-everything).

**Root cause:** `memory/layout.rs`: `SURVIVOR_SIZE = 512 << 10` — a
FIXED 512KB, while this workload's per-scavenge live set (the current
iteration's Association chain) is ~3–6MB. Ungar's adaptive threshold
(`compute_threshold`: keep survivors under half capacity) has no legal
answer but "tenure everything", so each iteration's chain — dead
microseconds later — is promoted into old space, which fills at
~1.5MB/iteration and triggers a 2.5–7.5ms full compaction every ~30
iterations. The full-GC bill is ~130µs/iteration ≈ **23% of the bench**.

**Falsified variant, for the record:** MACVM_EDEN=131072 (4× eden) cuts
GC *counts* 4× (572→143 scavenges, 98→25 fulls) but the bench moves only
~3% — total collector work is survival-driven, not trigger-driven, and
512KB survivors still force the same promotion volume in bigger batches.
(A note here previously claimed MACVM_EDEN=262144 + MACVM_HEAP=1024
"silently fell back" — that was the MEASURING SHELL's word-splitting
passing one malformed variable to env(1), not the VM: correctly passed,
the same configuration runs 71 scavenges / 2 fulls. The override is
honored as-is, exactly as its doc says. Retracted with apologies to
`universe::genesis`.)

**The fix shape:** survivor capacity proportional to eden (eden/4-ish, or
≥8MB at the default 32MB eden) so a transient live set can AGE: with
eden 32MB the chain spans ≤2 scavenges of its lifetime, so threshold ~3
holds it in young at the same copy cost promotion already pays, old
receives only genuinely old data, and the full-GC category ~vanishes on
this shape. Estimated recovery ≈ the full-GC bill: alloc ~575 → ~445µs
(MACDART: ~380). Touches the reservation layout math — gate behind the
S7/S8 stress suites and the soak protocol before default-on.

### The fix, landed 2026-08-06: eden-proportional survivors (default ON)

`survivor_size_for(eden) = max(512K, eden/4)`, `MACVM_SURVIVOR` (KiB)
overrides — `=512` reproduces the old geometry, which is what makes the
one-binary env-flip A/B below possible. Two test adjustments:
`copy_exact_fill_survivor` pins the old geometry via a 2 MiB explicit
eden (its root-keeping pushes every survivor onto the guest stack, so
its fill count must stay bounded); the two layout geometry tests now
assert `survivor_size_for` instead of the fixed constant.

Gates: differential byte-identical, GC-stress 1/full/full:64 green,
400-cycle soak clean, full release suite green.

**Measured (3000 × benchAlloc, one binary, env flip, reproduced ×2):**

| | old (512K) | new (eden/4) |
|---|---|---|
| scavenges | 572 (981 ms) | 572 (**742 ms**) |
| full compactions | 98 (618 ms) | **30 (163 ms)** |
| GC total | 1599 ms | **905 ms (−43%)** |
| wall clock | 3.39 s | **2.71 s (−20%)** |

Survivor copying beats premature promotion twice over: the scavenges
themselves got cheaper (1.72 → 1.30 ms — old-space bump/card work is
dearer than a survivor memcpy) AND two-thirds of the full compactions
vanished. Classic-seven spot: every row within noise between arms.

**A protocol finding worth its own line:** `cog-bench`'s median-of-41
warm_us is structurally BLIND to GC improvements on alloc-shaped work —
a full GC lands in 2–3 samples of 41 and the median discards them, so a
−43% GC-time change moves warm_us not at all (it even read +3% once).
GC work must be judged by whole-run wall clock or the trace totals,
never by the median protocol. The residual alloc-bench gap vs MACDART
(~580 vs ~380 µs median) is now MUTATOR cost — the allocation loop's
codegen and ivar writes — not collector time; it belongs to the
codegen arcs.

## Eden sizing measured and REJECTED; the lever is promotion, not nursery size

*(2026-08-05, M4. Follows the W^X and code-layout work in
`critical_path_findings.md`.)*

**alloc is GC-frequency-bound — established.** Sweeping `MACVM_EDEN` against an
isolated `benchAlloc` driver, alloc time tracks scavenge count almost exactly:

| eden | alloc us | scavenges | scav ms | fulls | full ms |
|---|---|---|---|---|---|
| 4M | 3960 | 763 | 1105 | 145 | 575 |
| 8M | 2550 | 381 | 537 | 105 | 413 |
| 16M | 1304 | 190 | 252 | 20 | 85 |
| **32M (default)** | **920** | **95** | **110** | **6** | **35** |
| 64M | 722 | 47 | 67 | 2 | 15 |
| 128M | 661 | 23 | 33 | 2 | 25 |

The diagnostic detail is the **per-scavenge cost, which is constant**:
1105/763 = 1.45 ms at 4M, 33/23 = 1.43 ms at 128M. Each scavenge costs the
same regardless of nursery size, because what it copies is the LIVE SET, not
the nursery. `benchAlloc` builds a linked list, so ~200k objects are live
simultaneously (~8 MB) and get copied again and again until promoted (one
scavenge promotes 5.7 MB, which is what drives the full GCs). GC is ~30% of
the benchmark (145 ms of ~460 ms at the default).

**But raising the default is REJECTED — it was benchmark overfitting.** Run
across all seven instead of the isolated driver (3 rounds, mean):

| bench | 128M vs 32M |
|---|---|
| arith | +0.0% |
| fib | +0.2% |
| sieve | −2.9% |
| **dict** | **+20.4% WORSE** |
| alloc | **−6.5%** (not −28%) |
| richards | +0.6% |
| **deltablue** | **+5.3% WORSE** |

Two corrections fall out. First, alloc's gain shrinks from −28% to −6.5%: the
isolated driver ran `benchAlloc` 500x back-to-back, maximising the pathological
liveness, while the real harness does not. (Same error shape as R4's sieve-only
+3.1% that vanished on `benchSieve` — an isolated driver is not the benchmark.)
Second, a bigger nursery is a bigger working set to walk, and `dict` — already
our most cache-sensitive row — degrades 20.4%. Net across the suite it is
clearly negative. **The 32 MB default stands.**

**Where the real lever is.** Nursery size only defers the copying; the cost is
that long-lived data is copied repeatedly before it promotes. The gap to Dart
V1 on this row (1.66x) is therefore most plausibly in **promotion policy — when
survivors stop being copied** — not in nursery geometry or in codegen. That is
also the one direction on the whole board with ~30% of a benchmark behind it
rather than ~3%.

### CORRECTION: GC is zero on five of seven — it cannot explain the Dart gap

The section above ended by calling promotion policy "the strongest remaining
lead". That over-generalised from one benchmark. Measured GC share across all
seven (200 timed iterations each, `MACVM_TRACE=gc`):

| bench | total ms | gc ms | gc share | gap vs Dart V1 |
|---|---|---|---|---|
| arith | 305 | 0 | **0%** | 1.77x |
| fib | 1852 | 0 | **0%** | 1.45x |
| sieve | 9 | 0 | **0%** | 2.59x |
| **richards** | 224 | 0 | **0%** | **2.31x** |
| dict | 61 | 3 | 4% | 1.27x |
| deltablue | 33 | 4 | 12% | *MACVM wins 1.70x* |
| **alloc** | 173 | 63 | **36%** | 1.66x |

**GC is zero on five of seven, including richards — the second-worst row.** It
is material only on `alloc`, and mildly on `deltablue`, which is a row MACVM
already wins. Even eliminating GC entirely would take alloc from 1.66x to
~1.13x (617us at 36% GC -> ~395us against Dart's 351us) and change NOTHING on
arith, fib, sieve or richards.

So the gap to Dart V1 is overwhelmingly in **generated-code execution**, not in
memory management. The GC work above stands as a correct account of `alloc`
specifically and nothing more.

Worth recording alongside it: Dart's own background-compilation advantage is
NOT present in these numbers — `scripts/dart-bench.sh` runs every benchmark
with `--no_background_compilation` because that install path SIGILLs on modern
arm64. Dart's column is a floor, and its compiler threads are not what is
beating us here either.

The honest summary of the whole 2026-08-05 arc: every mechanism tested was
either below the measured noise floor (five gated features, 64-byte loop
alignment, 2x unrolling, W^X toggling) or real but confined to one benchmark
(bounds elision 15.5% on sieve; GC 36% on alloc). Nothing found is a general
1.5x. That is consistent with the remaining gap being **distributed across many
small things Dart's pipeline does that MACVM's does not** — unboxed
type-specialised fields, LICM, allocation sinking, and register residency
across safepoints — rather than concentrated in one undiscovered lever.
