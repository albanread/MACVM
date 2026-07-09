# Compiled-Send Auditor — PROBE live mode (DBG5 candidate)

**Status:** design, pre-review. Extends `docs/DEBUGGER.md` (§0, §4, §6).
Sprint on clean `main` (HEAD 96faa0a, S24 A3a); the S24 A3b WIP is stashed and
returns after this lands — its GC crash is DBG5's first real customer.

## 0. The gap this closes

`docs/DEBUGGER.md` §0 defines two debuggers by *which representation they
show* and *whether they deopt*:

- **HALT** (level 1, Smalltalk programmers): first act is **deopt to tier-0**,
  then step bytecode. Triggers on high-level breakpoints / `halt` / `error:`.
- **PROBE** (level 2, VM/compiler developers): **never deopt** — "deopt
  destroys the evidence… you've replaced the crime scene with the VM's own
  *story* about the crime scene." Freezes the physical frame; shows nmethod /
  native pc / registers / spill slots.

The unclosed hole: **PROBE only fires *post-mortem*** — on a `brk`, a
SIGSEGV/SIGBUS in the code cache, or a planted low-level breakpoint (and
planted breakpoints are themselves deferred in §6: they need hardware
single-step, "Mach exception server territory"). There is **no way to watch
compiled code execute live, step by step, before anything crashes.**

§6 recorded the deferral and its escape hatch:

> *Step-into compiled callees* — vanishes under breakpoint pinning; not worth
> a per-call debug check in compiled code (§3.4).
> *Hardware watchpoints / single-step* — Mach exception server territory; **the
> auditor choke point covers the known cases.**

The deferral's premise — "pin the method to tier-0 and use HALT instead" —
**fails for an entire bug class the deferral did not anticipate: bugs that
exist only in compiled execution and vanish when you deopt.** GC-root /
oopmap / deopt-metadata / spill-aliasing bugs (BUG D, task #94, and the live
S24 A3b materialized-Context crash) are all of this shape. Pinning the
suspect to tier-0 makes the phenomenon *disappear* — HALT can only ever show a
green interpreter run. The crash a developer *can* observe (PROBE
post-mortem) is downstream: A3b faults in `scavenge_oop` dereferencing a bad
oop, but the bad oop (spill slot 4 = raw `0x1`, a mem-tag on a null base) was
left stale at an *earlier* safepoint. Post-mortem shows the symptom; the root
cause is a live-execution fact that was already gone.

**DBG5 = PROBE, live.** Same trust model (never deopt, freeze the frame,
range-validate every dereference), but triggered at **every compiled send
boundary**, not only at a crash — so the developer can watch the exact
sequence of sends, and inspect each live compiled frame's oopmap slots, *while
the evidence is still on the stack.*

The mechanism is precisely the "auditor choke point" §6 already blessed:
`rt_resolve_send`.

## 1. The choke point (already load-bearing)

Every compiled send that is not a cache *hit* funnels through
`rt_resolve_send` (`src/codecache/stubs.rs:740`). Hits never leave compiled
code — they tail-jump the baked/`bl`-patched target — so today compiled
dispatch is invisible between misses (`MACVM_DBG_RESOLVE`, DEBUGGER.md §5, logs
exactly the misses and nothing else).

The one lever that makes *every* send observable without touching codegen:
**force-cold inline caches.** In auditor mode, `rt_resolve_send` computes and
returns the correct target but **does not patch the site** and **does not
advance its `IcState`** — so the site stays `Unresolved` and the *next* send
through it re-enters `rt_resolve_send`. No `brk`, no instruction patching, no
hardware single-step, no external debugger. Correctness is unchanged: the
`Unresolved` arm already resolves the true target for whatever receiver klass
arrives (`resolve_target_entry(vm, k, selector, method, false)`,
`stubs.rs:812-823`); we simply skip the memoization.

The as-built gate (revised after adversarial review — finding #1): a
`force_cold` **short-circuit placed BEFORE the `match prev_state`**, right
after the receiver's `method` is resolved. It computes the target exactly as a
first-touch `Unresolved` site would (`resolve_target_entry(vm, k, selector,
method, false)`), runs the hook, and `return`s — never entering the match.

Why before, not merely gating the two `patch_branch26_at` sites: the poly arms
(`Mono→Pic`, `Pic→Pic`) call `vm.pic_table.build(...)` **and free the old
stub** *eagerly while computing `patch`*, before any patch gate. Gating only
the final write would, if an ordinary site ever arrived non-`Unresolved`, free
a PIC stub and then skip installing its replacement — a dangling `bl` the
byte-identical gate cannot see (a leak doesn't perturb stdout until cache
exhaustion). Ordinary sites are born `Unresolved` today (`driver.rs`:
`super_resolutions[i]` is `Some` only for super sites), so those arms are
unreachable under force-cold — but short-circuiting ahead of them makes that
robust **by construction** against S24-era compile-time IC seeding, not merely
by that emergent invariant. A `debug_assert!(prev_state == Unresolved)` in the
short-circuit is the tripwire if seeding ever changes the born state. The
`send_super` block (which runs earlier and patches its own site) also skips its
patch under `force_cold`, keeping its `state`/`bl` pair frozen consistently.

**This is a debug mode.** Force-cold throws away all IC caching, so it is far
slower than production dispatch — irrelevant for a debugging tool, and it can
never *change a result*, only its speed and its observability. That invariant
(same results, byte-for-byte) is DBG5's headline gate (§5).

## 2. Trust discipline (inherited from PROBE §4.5, non-negotiable)

The hook runs *inside* `rt_resolve_send`, i.e. at a genuine safepoint: the
calling stub set the walker anchor before the `bl` (P9,
`stubs.rs:749`), so `walk_frames` is valid, and the caller's oopmap at the
return address is a real map. That is the ideal place to inspect — but the
rules are PROBE's, because the whole point is that the VM is the suspect:

1. **Never deopt.** The auditor reads compiled frames in place. It never
   materializes interpreter frames (that is HALT, and it destroys the
   evidence).
2. **Never allocate on the Smalltalk heap.** Formatting strings on the Rust
   heap is fine (PROBE does it); reading a klass *name* (symbol→bytes) is a
   tag-checked, range-validated, best-effort read that prints `<badoop
   0x…>`rather than dereferencing a non-heap-plausible pointer.
3. **Range-validate every dereference** derived from frame slots / registers —
   heap-containment for oop candidates, code-cache bounds for pcs, stack
   bounds for fps — exactly `each_code_root`'s own slot discipline and PROBE's
   §4.5 rule. The debugging tool must not fault while observing a fault.
4. **Reentrancy backstop.** `DebugState.session_depth` (`debug.rs:73`, already
   present) guards the interactive loop; a `AUDITOR_IN_PROGRESS` flag guards
   the non-interactive tracer so a stray send raised by best-effort formatting
   can never recurse.

## 3. Three deliverables

### D1 — `MACVM_TRACE=calls`: the compiled-send tracer (non-interactive)

Force-cold when `vm.trace.is_enabled("calls")` (an open-ended `TraceFlags`
channel, `vm_state.rs:483`; live-toggleable via the existing RUSTTCL `trace`
verb). One stderr line per send, from the hook:

```
[calls] <caller-nm>#<site_idx> → <selector>  recv=<klass>  target=<C nm=NN | I | prim | dnu>  argc=<n>
```

This is `MACVM_DBG_RESOLVE` promoted from "misses only" to every ordinary
dynamic send (force-cold makes those misses total), enriched with the receiver
identity and resolved target tier, and gated as a first-class trace channel.
It answers *"which send is live right now?"* — the question A3b's GC crash
could not otherwise answer.

**Scope (review findings #2/#3), v1:** the tracer sees every ordinary dynamic
send, but NOT (a) `send_super` sends — a super site is born `Mono` with its
`bl` baked straight at the target, so on the happy path it is a *hit* that
never enters `rt_resolve_send`; nor (b) `doesNotUnderstand` sends — both DNU
early-returns precede the hook. Both are correctness-irrelevant tool gaps,
noted so the log is not misread as exhaustive.

**Caveat (review finding #4):** force-cold routes every ordinary send through
`rt_resolve_send`, so `vm.stats.ic_misses` is massively inflated and
`pic_extends`/`mega_transitions` stay 0. These are **stderr-only** stats
(`MACVM_TRACE=stats`); the guest's stdout is unaffected and no test asserts on
them under `calls` — but `MACVM_TRACE=calls` must NEVER be combined with an
IC-transition-asserting harness (e.g. `tests/repros/README.md`'s "NO further
misses" repro), which assumes normal memoizing dispatch.

### D2 — the safepoint oop-slot inspector: `slots` + `MACVM_TRACE=oops`

A read-only function `dump_frame_oops(vm, fp, nm_id, ret_pc) -> impl Report`
that walks the caller frame's oopmap at `ret_pc` — the *same* slot iteration
as `each_code_root`'s `FrameView::Compiled` arm (`roots.rs:273-305`,
`oopmap_at(ret_pc).iter_slots()`), factored into one shared helper so the
inspector and the collector can never diverge — and for each live slot prints:

```
slot=<i>  word=<raw>  <klass-name | NIL | SMI | ⚠BADOOP(near-null) | ⚠BADOOP(non-heap) | to-space>
```

Three ways in:

- **`slots [frameN]`** — a RUSTTCL/step-call verb, on demand for a chosen
  frame.
- **`MACVM_TRACE=oops`** — scan **every** live compiled frame's oopmap slots
  **at every GC entry** (a hook in the collector's root walk), flagging any
  slot whose word is mem-tagged but not heap-plausible. This is the promoted,
  first-class form of the throwaway `DBGA3B NEARNULL` reporter that localized
  A3b to `nm=61 slot=4 word=0x1` — now a permanent tool that screams the
  instant GC would trip, with nmethod id, slot, and safepoint pc.
- **auto** inside the step-call session (D3).

`oops`-at-GC is the single highest-value item for the whole GC/oopmap bug
class: it converts a *downstream* `scavenge_oop` SIGSEGV into an *at-the-slot*
report naming the exact frame and slot, before the deref.

### D3 — `step-call`: interactive PROBE-live REPL at send boundaries

When armed (`MACVM_STEP_CALLS=1`, `macvm debug`, or a RUSTTCL `step-call`
verb), the hook enters a nested command loop at each send boundary — the same
loop shape as HALT's (`debug.rs:280-343`), but showing the *compiled* view and
obeying PROBE's never-deopt rules:

```
▸ about to send  #<selector>  to  <recv-klass>   (from <caller-nm>#<site>)
(auditor) _
```

Commands (read-only; never deopt):

| verb | action |
|---|---|
| `bt` | mixed-tier backtrace (DBG2's `walk_frames` view, unchanged) |
| `slots [N]` | D2 on frame N (default: caller) |
| `regs` | the marshaled receiver/args at this send (argv) + saved caller regs |
| `disasm [N]` | `disasm-native` (DBG3) around frame N's pc |
| `step` \| `s` | resume; stop at the next send (force-cold guarantees one) |
| `over` \| `n` | resume; stop at the next send at caller depth or shallower |
| `finish` | resume; stop at the next send after the caller returns |
| `continue` \| `c` | disarm interactive stop (tracer, if on, keeps logging) |

`print <expr>` is **deliberately excluded** in v1: it evaluates → runs the
interpreter → allocates on the heap — everything PROBE must not trust (§4.5,
"No Smalltalk evaluation"). Slot/register reads cover the need; arbitrary
evaluation is HALT's job.

`over`/`finish` reuse `StepPlan`'s `base_fp`/serial comparison shape
(`debug.rs:229-239`) but keyed on the send-boundary fp, not a bci.

## 4. Sprint steps (each commits to `main`)

1. **Force-cold + `MACVM_TRACE=calls` (D1).** `force_cold` flag in
   `rt_resolve_send`; gate the two patch sites; the tracer line. Gate: world
   differential `MACVM_JIT=off` vs `MACVM_TRACE=calls MACVM_JIT=threshold=200`
   byte-identical (force-cold changes nothing but speed); a scripted golden of
   the `calls` log for one tiny compiled method.
2. **Oop-slot inspector (D2).** Factor the shared oopmap-slot walk out of
   `roots.rs`; `dump_frame_oops`; `MACVM_TRACE=oops` at-GC scan. Gate: run
   richards + deltablue under `GC_STRESS=1` at t=200 — assert **zero** BADOOP
   flags (the tool is silent on correct compiled code; A3b will make it
   scream). Unit test the classifier on hand-built nil/smi/oop/near-null words.
3. **Interactive `step-call` (D3).** The nested loop, the verbs, the CLI +
   RUSTTCL surface. Gate: scripted step-call session golden (commands in,
   transcript out — the disasm-golden discipline).
4. **Docs + verification.** Fold the authority into `DEBUGGER.md` (§4.6 PROBE
   live mode + §5 env rows + §6 build-plan/deferral reversal); `cargo test`,
   clippy, fmt; commit.

Then: unstash A3b, reproduce the materialized-Context crash under
`MACVM_TRACE=oops` at t=200 (warmup-loop repro so it compiles at a realistic
threshold), read the exact `nm/slot/pc`, fix, re-verify silent.

## 5. Gates (the invariant that matters)

- **Results are identical with the auditor on.** Force-cold and every hook are
  observation-only. World differential `off` vs `calls`/`oops`/`step-call`
  (batch-scripted) must be **byte-identical at t=200**, plain and under
  `GC_STRESS=1` / `GC_STRESS=full:64` / `DEOPT_STRESS=64`. If any auditor mode
  changes a single output byte, it is broken by definition.
- **The inspector is silent on correct code** (step-2 gate) and **crash-proof**
  (range-validated; a deliberately-corrupted slot in a unit test yields
  `⚠BADOOP`, never a fault).
- t=200 only — t=1 is the differential oracle elsewhere, not a perf/observation
  regime; auditor repros use warmup loops so methods compile at a realistic
  threshold.

## 6. Risks

- **R1 — force-cold miscompiles a dispatch.** Mitigated: the `Unresolved` arm
  is the *already-correct* general path; we skip memoization, not resolution.
  The byte-identical gate is the proof.
- **R2 — the inspector faults while inspecting.** Mitigated: PROBE §4.5
  range-validation on every derived pointer; the classifier is total (every
  word maps to a label, never an unguarded deref).
- **R3 — heap reads in the tracer perturb GC.** Mitigated: no Smalltalk
  allocation; klass-name reads are best-effort tag-checked; `oops`-at-GC runs
  *within* the existing root walk, touching nothing the walk doesn't already.
- **R4 — scope creep into a full external debugger.** Explicitly out: no DAP,
  no ptrace, no `print`-eval, no fix-and-continue (all already deferred in
  DEBUGGER.md §6 and stay deferred). DBG5 is three read-only observation modes
  over one existing choke point.
