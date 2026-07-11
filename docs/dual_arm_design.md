# Dual-arm branch storm — design (sieve canonical repro)

> **OUTCOME NOTE (2026-07-09).** This design was written against the sieve
> repro before the dominant Richards storm was root-caused. Two of its
> findings were independently confirmed and remain the durable record:
> (1) ordinary two-way branches ALREADY compile as two real native arms —
> there is no missing two-arm machinery; (2) sieve's traps are cold
> `Empty`-IC sends from compiling (via OSR/threshold=1) before the
> interpreter profile warms — a real but mild issue, distinct from the
> storm. The RICHARDS storm turned out to be a third thing neither this
> doc nor its predecessors saw: `activate_method` stomping polymorphic
> ICs back to Mono-compiled, starving the recompiler of Poly feedback —
> fixed in commit a2bfd8b (richards 16×, deopts 160,674→2; see
> `docs/PERF.md` "RESOLVED" entry). This doc's own fix proposal is
> therefore NOT implemented; its cold-OSR-profile analysis stands as the
> open, lower-priority sieve follow-up.
>
> **UPDATE 2026-07-11:** fixed in de7e20e — OSR compiles no longer lower
> Untaken sites to UncommonTrap (sieve ~90ms/storm -> ~9ms, deopts 30->0).
> The two durable findings above still stand.

Status: DESIGN ONLY (no source changed). Written 2026-07-08.
Grounded in release+debug runs of `world/bench/sieve.mst`, the debug
`MACVM_DBG_IR` dump, `rusttcl disasm`/`ic`/`nmethods`, and full reads of
`ir.rs`/`inline.rs`/`feedback.rs`/`recompile.rs`/`decode.rs`/`osr.rs`.

> **Headline correction to the prior analysis.** `weekend_work.md` Gap 1 and
> `PERF.md`'s "Dual-arm branch storm" entry diagnose this as *balanced two-way
> branches whose dominant arm keeps shifting*, to be fixed by "compile BOTH
> arms as native code." That is **not** what sieve actually does. Evidence
> below shows: (1) ordinary two-way branches **already** compile as two real
> native arms (`Ir::BoolBr`/`Ir::SmiCmpBr`, both carry `if_true` AND
> `if_false`) — there is no missing two-arm machinery; and (2) the storming
> sites are **cold sends** (`count := count + 1`, loop-counter `+`), lowered
> to `Ir::UncommonTrap` by the S14 step-3 `Untaken → Trap` rule because their
> inline caches are `Empty` at the moment the JIT compiles the method. The
> real bug is **cold-profile speculation forced by OSR** (a single long-running
> activation is only ever compiled via OSR, which fires before the interpreter
> ICs have warmed), and a `recompile.rs` that re-snapshots a profile that is
> *still cold* and so re-emits the identical trap set. `recompiles=18 /
> declined=0` is NOT "one site chasing a moving target" — it is **18 distinct
> nmethods each recompiled exactly once**, cold→cold.

---

## 1. Pinpoint: what `nm=8 bci=155` actually is

`Sieve class >> run` (`world/bench/sieve.mst:11-27`). I replicated its body
verbatim as an instance method (`SieveInst>>run`, identical bytecode — the
class/instance distinction does not change the body) and disassembled it via
`rusttcl disasm`. The relevant tail:

```
149: push_nil
150: pop
151: push_temp 1        ; count
153: push_smi 1
155: send 13; #+        ; <-- nm=8 bci=155  ==  count + 1
157: dup
158: store_temp_pop 1   ; count :=
160: jump_fwd 1 -> 164
163: push_nil           ; the (flags at:i) ifFalse value (nil; no else)
164: pop
165: push_temp 11       ; i
167: push_smi 1
169: send 14; #+        ; <-- bci 169  ==  i + 1   (inner `1 to: size do:` increment)
171: store_temp_pop 11  ; i :=
173: jump_back 95 -> 81 ; inner loop back-edge (header bci 81)
176: pop
177: push_temp 8        ; timesRepeat counter
179: push_smi 1
181: send 15; #+        ; <-- nm=9 bci=181 == outer `10 timesRepeat:` increment
183: store_temp_pop 8
185: jump_back 175 -> 13; outer loop back-edge (header bci 13)
```

So, precisely:

- **bci 155** = the smi `#+` of `count := count + 1`, **inside the
  `(flags at: i) ifTrue: [...]` arm** (bci 94 `at:`, bci 96 `br_false_fwd`
  guard the body).
- **bci 169** = the smi `#+` incrementing the inner `1 to: size do: [:i|...]`
  loop variable.
- **bci 181** = the smi `#+` incrementing the outer `10 timesRepeat:` counter
  (this is the `nm=9 bci=181` co-storming site in the release trace).

None of these is a `SmiCmpBr`-fused `ifTrue:ifFalse:`, an array-op guard, a
`must_be_boolean` branch, or a smi-overflow guard. **They are plain smi `+`
sends that were lowered to `Ir::UncommonTrap` because their IC was `Untaken`
(Empty) when the method was compiled.** `result=false` in the deopt trace is
NOT "the false arm" — `deopt.rs:394` prints `frame.incoming_result.is_some()`,
which is always `false` for a reexecute (uncommon-trap) site.

Confirmed cold-lowering via the debug `MACVM_DBG_IR=run` dump — every compile
of `run` contains `block @bci155: UncommonTrap { bci: 155 }` and the full set:

```
compile #1 (base v0):   1 trap  -> [17]
compile #2 (OSR   v0):  17 traps -> [17,26,43,53,60,85,94,96,103,107,115,123,133,140,155,169,181]
compile #3 (OSR   v0):  17 traps -> [ identical set ]
compile #4 (recompile v1): 17 traps -> [ identical set ]
compile #5 (recompile v1): 17 traps -> [ identical set ]
```

Every send site (16 of them) plus the `(flags at:i) ifTrue:` branch at bci 96
is a trap, in **every** compile. The base compile (nm=7) is even worse: it
traps at the very first send (bci 17, the `timesRepeat` `<=`) and everything
after is unreachable — a compiled method that does nothing but immediately
deopt.

### Why these ICs are Empty at compile time (the actual mechanism)

- `run` is a **single activation** (called once; the `10 timesRepeat:` and the
  two `1 to: size do:` loops are *inside* it). The method-invocation threshold
  is therefore never reached for `run` — **the only thing that ever compiles it
  is OSR** (`osr.rs::rt_osr_request`, fired from the interpreter's `jump_back`
  slow path once the per-method loop counter crosses `LOOP_COUNTER_LIMIT =
  10_000`, `layout.rs:305`). This is why `PERF.md` shows sieve flat at **both**
  `t=1` and `t=1000` — the threshold is irrelevant here.
- With `threshold=1`, `run` compiles on its first call *before it ever runs
  interpreted*: the base nmethod (nm=7) is entered as compiled code, traps at
  bci 17, and only THEN does any interpretation happen (via the deopt resume).
  So the profile the compiler sees is genuinely empty.
- After a full run, **all 16 sites are `Mono/SmallInteger`** (verified with
  `rusttcl ic SieveInst run` after both an interp run and a `threshold=1` JIT
  run — identical Mono result). The profile *does* warm; the JIT just never
  compiles against the warm version.

---

## 2. The speculation machinery (where the trap decision is made)

**The "which arm to trap" decision does not exist as such.** The decision that
matters is per **send site**, made in one place:

- `src/compiler/inline.rs:271-278` — `decide(feedback)`:
  `SiteFeedback::Untaken => InlineDecision::Trap`; `Mono|Poly|Mega =>
  InlineDecision::Call`. This is the entire policy.
- `src/compiler/ir.rs:3343-3396` — the converter reads the site feedback
  (`feedback::read_send_site`), and if `decide` says `Trap`, emits
  `Ir::UncommonTrap { bci }` with a reexecute deopt scope and sets
  `*trapped = true` (the rest of the block is unreachable). The long comment
  there (ir.rs:3370-3381) explicitly names this the "INTERIM DEOPT-STORM … NOT
  solved here," deferring the fix to recompile.rs.
- The profile signal is `feedback::read_send_site` (`feedback.rs:65-94`), which
  reads the **interpreter IC side table** for that site: `Empty→Untaken`,
  klassOop guard→`Mono`, smi-tagged guard→`Poly`/`Mega`. Cold = Empty =
  Untaken = Trap.
- Same Untaken→Trap lowering is repeated inside the inline splicers
  (`ir.rs:1865-1919`, `ir.rs:2503-2570`) for sends in an inlined callee body.

**Ordinary two-way branches already compile as two real native arms — the
"two-arm machinery" exists.** Confirmed in three places:

- `decode.rs` turns `br_true_fwd`/`br_false_fwd` into
  `Terminator::Branch { if_true, if_false }` (both real block indices).
- `ir.rs:2682-2705` (CFG-inline splice) lowers `Terminator::Branch` to
  `Ir::BoolBr { val, if_true, if_false, not_bool }` — `if_true` and `if_false`
  are **both** real compiled arms; only `not_bool` (a non-boolean condition
  value) is a trap.
- `Ir::SmiCmpBr { op, a, b, if_true, if_false, fail }` (`ir.rs:195-202`) — the
  fused compare+branch — likewise carries **both** arms; only `fail`
  (non-smi operands) is a trap.

So a plain `ifTrue:ifFalse:` on a boolean, or a fused smi comparison branch,
is NOT the storming shape and needs no new codegen. (bci 96, the
`(flags at:i) ifTrue:` branch, does appear in the trap set — but that is a
*consequence* of its guarding `at:` at bci 94 being cold, discussed in §5, not
a balanced-arm problem.)

- S14 step 6 `DominantWithSlowPath` (`inline.rs:356-375`, emitted at
  `ir.rs:3433-3508`) handles a **polymorphic send** (two receiver klasses at
  one call) by inlining the dominant leaf behind a klass guard whose fail edge
  is a **real compiled send that rejoins at a continuation — never a trap**
  (inline.rs:347-355 spells out the anti-storm rationale). This is the closest
  existing precedent for "guarded fast path + non-trapping fallback that
  rejoins," and the fix in §4 reuses exactly this shape.

---

## 3. Why `recompile.rs` cannot heal it

`src/runtime/recompile.rs` (read in full) is trap-count driven:
`rt_uncommon_trap` (`deopt_trap.rs:1169`) calls `note_uncommon_trap(vm, nm_id)`
*after* the trap's nested interpreted run. At `trap_count >= UNCOMMON_TRAP_LIMIT
(=2)` it re-snapshots `feedback::snapshot_profile` and, if the hash differs
from compile time, recompiles `version+1` and makes the old nmethod
NotEntrant; if the hash is unchanged it DECLINES
(`recompile_declined_ineffective++`).

It never converges here for concrete, proven reasons:

1. **The recompile snapshots a still-cold profile.** The debug IR dump proves
   the v1 recompiles emit the **identical 17-trap set** as v0 — not one site
   healed. The recompile fires deep inside the storm's own recursion (see
   below), at a point where the interpreter ICs have not yet warmed, so
   `read_send_site` still returns `Untaken` for every site and the "warm"
   recompile is byte-for-byte as cold as the original.

2. **`declined=0` is a red herring.** It does not mean "the profile keeps
   changing (arms oscillating)." After the first recompile makes the nmethod
   NotEntrant, every subsequent `note_uncommon_trap` for that id returns early
   at the `!matches!(nm.state, Alive)` guard (`recompile.rs:46`) — it never
   reaches the profile comparison, so it can neither recompile again nor
   increment `declined`. `recompiles=18` is **18 different nmethods each
   recompiled once** (release trace: nm=8,9,23,30,34,33,31,46,12,18,21,20,26,
   24,29,44,43,15 — all distinct), the cold→(still cold) transition for each of
   the many methods sieve touches (run, OrderedCollection>>do:, String>>
   copyFrom:to:, …), capped at `MAX_VERSIONS=3` per method.

3. **Even a hypothetically-warm recompile would not be re-entered.** The
   recompile path calls `compile_method_versioned` → a **base** nmethod
   (`osr_bci = None`). A base nmethod is only entered on a fresh *call*. `run`
   is never called again, so the healed base version is dead weight; the loop
   keeps re-entering via OSR (`osr.rs:99-112`), which reuses the installed OSR
   entry while it is `Alive` and otherwise recompiles a *fresh* OSR entry —
   again against the current (cold) profile.

**The storm's recursion (why the profile stays cold at recompile time).**
`rt_uncommon_trap` deopts and calls `interpret_active` to run the resumed
activation. That interpreter run hits the loop back-edge, `rt_osr_request`
re-enters the *same still-Alive cold OSR nmethod*, which traps again → nested
`rt_uncommon_trap` → nested `interpret_active` → … The traps therefore fire on
the way *in* (all the `[deopt] … bci=155` lines print first) and
`note_uncommon_trap` fires on the way *out*. Between re-entries the interpreter
advances only a little, so ICs warm slowly while the recompile checks are
already firing against the cold snapshot. Net effect: the compiled code runs a
few iterations, traps, and the bulk of the sieve executes interpreted deep in
the recursion — sieve stays at **interpreter speed (87→88 ms, ~1.0x)**.

The signal that *should* trigger a permanent non-speculative compile is **not**
a changed profile hash (it isn't changing) — it is the **repeated trap at a
site** itself, and/or the structural fact that the site is a smi/array
fast-op selector whose cold state is far more likely "not yet warmed" than
"dead."

---

## 4. The fix (recommendation)

(Refined below in a later pass — this is the initial recommendation.)

The minimal, root-cause fix is to **stop speculating on a cold profile**:
`Untaken` must not lower to `UncommonTrap`. Two tiers, smallest first:

**Tier 1 — Untaken → non-trapping compile (kills the storm).**
Change the cold-site lowering so an `Untaken` send compiles to a real
`Ir::CallSend` (a plain dynamic send that resolves via the compiled IC / c2i
on first execution) instead of `Ir::UncommonTrap`. This is a targeted change
at `inline.rs:271-278` (`decide`) plus the mirror sites in `ir.rs` (3343-3396
root path; 1865-1919 and 2503-2570 splicer paths). A cold site is then handled
exactly like a `Mega` site — already-shipping, safe codegen — and can never
storm. Cost: genuinely-dead cold code gets a few bytes of never-run call
sequence (negligible); genuinely-live-but-cold code runs one slowish dynamic
send instead of a deopt.

**Tier 2 — speculate the smi/array fast path for cold arithmetic selectors
(recovers the speed).** Tier 1 removes the storm but leaves sieve's `+`/`<=`
/`at:` as generic dynamic sends (no `SmiArith` fusion → not the >100x arith
gets). Because the compiler already knows the *selector* at a cold site (it is
in the bytecode even when the IC is Empty), it can speculate the smi/array
fast path for a known fast-op selector — emit the same `SmiArith` /
`SmiCmpBr` / `ArrayAt(Put)` the warm `Mono` path emits, guarded by a smi/array
test whose fail edge is a **non-trapping rejoining slow call** (the S14 step-6
`DominantWithSlowPath` shape, inline.rs:347-375), not a deopt. For sieve every
operand is a SmallInteger, so the guard always passes and the fast path runs at
full arith speed with zero traps.

Recommendation: **do both**, gated behind the selector. Tier 1 is the
correctness/robustness floor (no site ever storms again); Tier 2 is what makes
sieve actually fast. Full landing-point step list, merge/GC-map analysis, and
the interaction with recompile.rs / OSR are in §4-detail below (to be
appended).

---

## 5. Correctness + risk (initial)

- **Do not regress the genuinely-cold-then-warm good case.** Tier 1 keeps that
  case correct (a cold site is a real call; when it warms, a later OSR/recompile
  can still upgrade it). Tier 2's guard fail edge must REJOIN (compiled slow
  call), never trap, so a mis-speculated smi op self-heals without a storm.
- **bci 96 (the `ifTrue:` branch) in the trap set** is a consequence, not an
  independent branch bug: its guarding `at:` at bci 94 is cold → trap →
  unreachable tail; once bci 94 is a real `ArrayAt`/call (Tier 1/2), bci 96
  reverts to an ordinary `BoolBr` with two real arms. Verify this in the IR
  after the change (expect the bci-96 trap to disappear).
- **Merge-point / GC-map:** a non-speculative branch needs *less* deopt
  metadata than a trap, not more (a `BoolBr`/`SmiCmpBr` records no reexecute
  scope on its taken arms; only its `not_bool`/`fail` edge does). Confirm the
  rejoining slow-call fail edge records a normal call-return safepoint
  (reexecute=false), identical to `DominantWithSlowPath`'s slow block
  (ir.rs:3482-3508).
- **Adversarial:** the only value that lives *only on today's deopt path* is
  the reexecute operand stack the trap records; replacing the trap with a
  real op removes that obligation entirely (the operands become ordinary vreg
  inputs to `SmiArith`/`CallSend`). No new value must be manufactured.

---

## 6. Test + measurement plan (initial)

1. **Storm gone:** `MACVM_JIT=threshold=1 MACVM_TRACE=stats ./target/release/
   macvm run world/bench/sieve.mst --world world` → `deopt_count` for the
   run/OSR sites → ~0 (from 65); `recompiles` drops sharply.
2. **Speed:** sieve `t=1` must beat interp by ≥1.5x (target: approach the
   arith >100x since the body is a fused smi loop). Compare to the 87→88 ms
   baseline.
3. **Correctness:** `run` returns the prime count; the transcript must still
   print **1899** (differential oracle — the interp result). Keep
   `compiled_result_equals_interpreted` green.
4. **No IR regression elsewhere:** re-dump `MACVM_DBG_IR=run`; the bci-155/169/
   181 (and 96) traps must be gone and replaced by `SmiArith`/`ArrayAt`/
   `BoolBr`.
5. **Richards re-check:** re-run `scripts/perf.sh --release`; confirm richards
   ratio improves and nothing else regresses (same before/after table format
   as `PERF.md`).
