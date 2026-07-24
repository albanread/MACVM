# PolyCmpFuse — poly-identity compare dispatch chains (2026-07-25, 776f3e2)

The fix that took dict from 6.26 ms to 3.35 ms (−46%; 4.01× over Cog) and
deltablue from 2.8 to 1.9 ms. This doc records the diagnosis replay recipe,
the exact soundness argument, and — because the question matters for what
gets built next — precisely how generic the mechanism is and where its
deliberate boundaries lie.

## 1. Symptom and evidence chain (the replay recipe)

Profiling showed benchDict interpreter-bound (`rt_interpret_call` 170
samples, `try_primitive` 130) despite `Dictionary>>at:`/`at:put:`/`scanFor:`
all compiling. The chain that turned that into a root cause, each step a
measurement:

1. **`MACVM_C2I_CENSUS=1`** (stubs.rs, inside `rt_interpret_call`): every
   dispatch through a c2i adapter is counted, with a (klass≫selector)
   histogram and a compiled-nmethod-available check. Result: **4M
   dispatches per 200 benchDict reps, 99.9% `SmallInteger>>=`,
   compiled_avail=0%**. (The 0% also *falsified* the competing hypothesis —
   S11-step-10 adapter repatching — before it was built; see §7.)
2. **`MACVM_DBG_IR='scanFor:'`** (debug build; pool oops now print klass
   names): v0 lowered `probe = key` as
   `GuardKlass(probe, Symbol, KlassTest) + RefCmpVal`, fail edge =
   `UncommonTrap`. v1 (a later version) lowered the same send as a plain
   `CallSend`.
3. **`MACVM_TRACE=deopt`**: `Dictionary>>scanFor: bci=47` trap storm,
   `recompiled nm=2 v0 -> nm=9 v1 (trap storm)`.

## 2. Root cause: a decision-ladder hole, not a bug

`Dictionary>>scanFor:`'s `probe = key` inline cache is **shared by every
Dictionary in the system**. The boot world's dictionaries key on Symbols,
so the site's IC is mono-Symbol when scanFor: first compiles, and the mono
splicer correctly speculates `Symbol>>=` (`^self == other`) as a
klass-guarded identity compare with a trap fail edge.

benchDict then probes with SmallInteger keys: every probe fails the Symbol
guard → trap → deopt → storm detector → recompile. The recompile sees a
poly `{Symbol, SmallInteger}` IC that **no existing decision can serve**:

- `SameTargetPoly` needs all arms to resolve to ONE method; `Symbol>>=`
  and `SmallInteger>>=` differ.
- `DominantWithSlowPath` needs a spliceable dominant; `SmallInteger>>=` is
  a **primitive** (prim 14), and primitives are never spliced (their
  bytecode is only the failure fallback).
- The mono smi fuse (`is_smi_inlinable`) needs a mono-smi IC; the site is
  poly.

So v1 fell to the ladder's bottom: a generic `CallSend`, whose callee
(`SmallInteger>>=`, a tiny primitive method that never tiers up) runs
through a c2i adapter into the interpreter — **every key comparison,
forever**. The hole: *a poly site whose arms are individually fusible but
target-distinct had no arm in the ladder.*

## 3. The fix

### 3.1 Decision — `InlineDecision::PolyCmpFuse { legs: Vec<PolyCmpLeg> }`

In `decide_with_budget`'s Poly arm, checked after `SameTargetPoly`, before
`DominantWithSlowPath` (inline.rs). Fires iff `cases.len() >= 2` and
**every** observed arm is one of:

- **Smi leg**: `klass == smi_klass && method.primitive() == 14 && argc == 1`
  (`SmallInteger>>=`'s surface).
- **Ident leg**: `method_is_ident_eq(method)` — the body is *literally*
  `[PushSelf, PushTemp(0), Send{#==}, ReturnTos]` with no primitive
  (`Symbol>>=`'s exact shape). The `#==` selector is matched by name;
  identity is pinned non-redefinable in MACVM (the same pin `Ir::RefCmpVal`
  itself rests on), so the name is a stable proxy.

Legs are emitted in the cases' order — count-descending from the M1 poly
count tail, so the hottest receiver klass is tested first.

**Deliberately NO evidence floor.** Two reasons, both load-bearing:

1. *The real lifecycle can never accumulate one.* The site reaches Poly at
   the storm recompile with counts ≈ {Symbol: 0, smi: 1} and **freezes**
   there — compiled sends never bump interpreter ICs, and v1 (no traps)
   never recompiles. Any floor permanently locks the site into its give-up
   `Call`. (This was measured, not theorized: the first cut carried the
   16-sample floor and the fuse never fired.)
2. *A floor buys protection this decision doesn't need.* A "wrongly" fused
   site pays a couple of guard tests before the same rejoining send it
   would have made anyway — never a trap, never a wrong answer.

### 3.2 Lowering — a no-trap dispatch chain (ir.rs, root translator)

```
current block:  GuardKlass(probe, SmiTest)  ──miss──▶ leg1
                GuardKlass(key,   SmiTest)  ──miss──▶ slow   (coercion route)
                RefCmpVal(probe, key) → t;  dst ← t
                (falls through to continuation)

leg1:           GuardKlass(probe, Symbol, KlassTest) ──miss──▶ slow
                RefCmpVal(probe, key) → t;  dst ← t;  Jump continuation

slow:           CallSend `=` (probe, key) → dst;  Jump continuation
                [SafepointKind::Call deopt site — the ONLY safepoint]

continuation:   dst holds the answer from whichever route ran
```

The slow block is `SameTargetPoly`'s rejoining-send recipe verbatim (one
`CallSiteInfo` + `site_feedback` row, `reexecute: false`). The guard legs
carry **no deopt records at all** — they are ordinary control flow, not
speculation. Consequences:

- **The site cannot storm.** There is no trap to storm through. The
  failure mode of every mis-prediction — unseen klass, coercing argument,
  drifted receiver mix — is *slow, never wrong, never deoptimizing*.
- Regalloc/emit needed **zero changes**: the shape composes existing ops
  (`GuardKlass` fail edges were already CFG successors; `RefCmpVal` is the
  S24 unguarded raw-bits compare; the slow block is the existing rejoin
  pattern). L1 copy-propagation even optimizes the inline leg's operands
  unprompted.

### 3.3 Soundness, per leg

- **Ident leg** (`K>>= ≡ ^self == other`): once the receiver's klass is
  proven K, `=` *is* identity for **any** argument — `RefCmpVal` is exact.
  No argument guard needed.
- **Smi leg**: raw-bits equality of two smis IS smi numeric equality (same
  tag, same value bits ⟺ same integer) — prim 14's exact fast case. But
  prim 14 *fails* for a non-smi argument, where `SmallInteger>>=`'s
  fallback does real work (`3 = 3.0` coerces via `asDouble`, LargeInteger
  via `asLargeInteger`). Hence the **key SmiTest whose miss routes to the
  slow send** — the coercion semantics live in the real method, reached
  through real dispatch. A raw-bits compare there would answer `false` to
  `3 = 3.0`; the e2e pins this route.
- **Redefinition**: `record_inline_dep(K, selector)` per fused leg — a
  live redefinition of `Symbol>>=` or `SmallInteger>>=` invalidates the
  nmethod through the existing key-selector dependency machinery.
- **Unseen klasses**: fall through every guard to the slow send — the
  fully general dispatch. Correct without recompilation.

## 4. Is it generic? — yes along four axes, bounded on four

**Generic:**

1. **Selector-agnostic.** The decision never examines the selector — it
   classifies the arm *methods*. Any poly site (any selector name) whose
   arms are all identity-shaped bodies fuses; the smi leg keys on
   primitive **14**, which in practice only `=` carries. A user class
   hierarchy with `sameAs: [ ^self == other ]` across two klasses gets the
   fuse for free.
2. **Klass-agnostic.** Nothing names Symbol. Any klass whose `=` (or other
   selector) is textually `^self == other` qualifies — world classes and
   user classes alike, discovered by bytecode shape, not by list.
3. **Arm-count-agnostic.** Legs chain: `{smi, K1, K2, K3}` emits a
   four-leg chain (bounded by `IC_POLY_MAX_PAIRS = 4` upstream). deltablue
   proved multi-site generality — its constraint-strength `=` sites gained
   −32% at the harness warmup with zero dict-specific code.
4. **Backend- and port-agnostic.** The entire fix is decision + lowering
   over **existing IR ops** — no new `Ir` variant, no emitter or regalloc
   change. It ports to WINVM/x64 essentially verbatim (both crates already
   share `GuardKlass`/`RefCmpVal`/the rejoin recipe from the S24/M2 arcs).

**Bounded (each boundary deliberate):**

1. **Identity-only arms; all arms must qualify.** A value-comparing arm
   (String content `=`, Double `=`) declines the whole site — raw bits
   would be *wrong* there, and this is enforced by construction. A
   *partial* fuse (legs for qualifying arms, slow send for the rest) would
   still be sound — non-qualifying klasses would simply fall through like
   unseen ones — but if the hot arm were the non-qualifying one, every hot
   call would pay the guard chain before its send, a small constant
   regression that frozen post-storm counts can't warn about. v1 takes the
   never-regress rule: all-or-nothing.
2. **`~=` is not covered.** The smi side would key on prim 15 and the heap
   side on `^(self = x) not`-shaped bodies (which are *not* the `==` shape
   and often live on a superclass); `RefCmpVal{neq:true}` is sitting there
   ready. Mechanical extension, unbuilt because no benchmark asked.
3. **Root translator only.** The splice walks (leaf/CFG grafts) still
   lower an inlined body's own poly `=` sites as generic `CallSend`s. The
   other fuses (RefCmpVal for `==`, BoolNot, the smi ops) exist in both
   places; porting the arm into the splice walk is the same mirroring
   exercise they went through. Today's win didn't need it — scanFor: runs
   as its own nmethod.
4. **Poly only, not Mega.** A site that blows past `IC_POLY_MAX_PAIRS`
   klasses reports `Mega` and the fuse is never consulted. (A Mega variant
   guarded on the two hottest klasses would need counts Mega doesn't
   keep.)

**The deeper genericity** — this is the first instance of a reusable
pattern: *per-arm dispatch trees with one shared rejoining send and no
traps*. `SameTargetPoly` is the one-body case (membership guard, single
splice); PolyCmpFuse is the many-bodies case where each body happens to be
a single IR op. The fully general form — different *spliced method bodies*
per klass arm — is exactly the polymorphic inliner that
`guard_elision_findings.md` identified as the keystone ("the jaw"). The
block discipline built here (chained guard legs, shared `dst`, one slow
block, safepoints only where a real call lives) is the skeleton that
generalization reuses; what it adds is per-arm splice budgeting, which is
dart124 item 2's worklist territory.

## 5. Lifecycle note — the storm still happens once

Mono-Symbol sites still compile through the ordinary mono splice
(guard + trap). The convergence path for a site that later goes poly is:
v0 mono speculation → **one** storm when the second klass arrives → the
storm recompile's IC is now poly → PolyCmpFuse → stable forever (v1 has no
traps left to fire). This is intentional: the mono splice is shared
machinery for *all* mono inlines and is the right shape when the
speculation holds (the common case); teaching it a rejoining fail edge
would trade away nothing measurable here and touch everything. If a
future workload shows the single storm mattering, the mono ident-eq case
can be lowered through the same chain shape with a one-leg chain.

## 6. Verification record

- Unit (inline.rs): fuses `{smi, ident}` in count order; fires on frozen
  `{1, 0}` counts (the floor regression pinned as a test); declines any
  value-`=` arm.
- E2E (`it_tier1::poly_cmp_fuse_dispatches_all_legs_and_slow_path`):
  structural (exactly 1 compiled IC site = the slow send; 2 inline deps)
  and interpreter==compiled across all eight routes — both fast legs hit
  and miss, smi-probe/heap-key (the coercion route through the slow send),
  heap-probe/smi-key, and an unseen identity klass both hit and miss.
- Suites: lib 838, it_tier1 104, and the 5-mode release differential
  (JIT-off / t=1 / GC_STRESS=1 / GC_STRESS=full:64 / DEOPT_STRESS=64) all
  clean on the checksummed 7-bench workload.
- Measurement: interleaved A/B (this tree vs HEAD^) dict 6258→3354 µs;
  interleaved Cog head-to-head re-stamp in `cog_bench.md` (dict 4.01×,
  deltablue 1.84×, all seven ahead, commit-stamped 776f3e2).

## 7. Appendix — the falsified sibling hypothesis

The investigation started at the *documented* gap (adapters.rs: c2i
adapters are never repatched when their method tiers up). Two findings
before any build, both from the census: (a) `compiled_avail = 0%` — the
interpreted callees had no compiled nmethod for the receiver klass at all,
so repatching had nothing to link; (b) a naive per-method
`adapter → entry` branch patch **hangs the VM**: adapters are keyed
per-method but nmethod entries are customized per-(klass, method), so a
caller whose receiver klass differs from the customization loops
guard-miss → `stub_resolve` → the same patched adapter, forever. Any
future S11-step-10 revival needs per-klass adapter keying (or entry
selection at patch time), and should start from `MACVM_C2I_CENSUS=1`
evidence that compiled targets actually exist to link to.
