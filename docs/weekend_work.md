# Weekend work: closing the Richards JIT perf gap (S15 T5 gate)

Picking this up when there's more token budget to spend on it. This doc
captures exactly what's already measured and known — the goal on the day is
to go straight to implementation, not re-derive the diagnosis.

## The gate, and where it stands

`tests_s15.md` T5: "Richards ratio ≥ 5.0" (compiled vs. interpreted). Current
state, recorded `docs/PERF.md` ("S15 A6/A7 — Richards/DeltaBlue perf
recording", 2026-07-06, commit f62a1e4):

| Benchmark | interp_ms | jit t=1 | best/interp |
|---|---|---|---|
| richards | 204 | 193 | **1.1x** |

Richards is **correct** at `t=1` (23246/9297 golden values match) — this is
purely a perf gap, not a correctness bug. (A *separate*, real correctness
bug also exists — mid-threshold silent wrong answers at `t=100..20000` — but
that's not this doc's concern; don't conflate the two.)

Two distinct, independently-measured, currently-unimplemented causes.

## Gap 1 — no polymorphic/two-way branch compilation (the dominant cost)

**Where:** `world/bench/richards.mst:330`, `HandlerTask>>processWork:`:

```smalltalk
work kind = self workPacketKind
    ifTrue: [ data workInAdd: work ]
    ifFalse: [ data deviceInAdd: work ]
```

**What happens today:** `work kind = self workPacketKind` classifies as a
`SmiCmpVal`/`SmiCmpBr`-style comparison (`src/compiler/ir.rs`, the
`SMI_INLINE`-fused path, `driver.rs:33`'s `SMI_INLINE` table). The compiler
speculates on whichever branch it observed dominant at compile time and
lowers the *other* arm to `Ir::UncommonTrap` (S14 step 3, "Untaken→Trap
lowering" — see `Ir::SmiCmpVal`'s `fail: BlockId` field, `ir.rs:205-211`,
and the `KlassGuard`-style doc comment right above it at `ir.rs:221-229`
describing the same trap-on-mismatch shape for the analogous receiver-klass
guard case).

Both arms of this specific branch are genuinely, regularly taken at
runtime — packets legitimately alternate kind as the scheduler round-robins
work/device packets. So this isn't a rare misprediction; it's a branch with
no real dominant side once the scheduler is running steady-state.

**Measured cost:** 60,000+ uncommon traps in a single run (`docs/PERF.md`,
same section, 2026-07-06 entry). Each trap = tear down the compiled frame,
reconstruct an equivalent interpreter frame (S13's materializer,
`src/runtime/deopt.rs`), re-execute the send interpreted, then presumably
re-enter compiled code on the next call. This dominates the 1.1x number —
PERF.md's own words: "most of the win is eaten by deopt/reexecute churn."

**What's missing:** what PERF.md itself calls "poly-arm compilation" —
compiling *both* arms as native code with a real conditional branch between
them, instead of speculating on one and permanently trapping the other. Two
possible angles worth evaluating on the day:

1. Extend from **S14 step 6, "DominantWithSlowPath poly inlining"**
   (`src/compiler/ir.rs`, search for how it handles a polymorphic *send*
   site — multiple receiver klasses at one call). That machinery already
   solves a structurally similar problem (one dominant case fast, the rest
   handled without a full trap) for *sends*; check whether it's reusable or
   adaptable for a two-way *value* branch (`ifTrue:ifFalse:` on a
   `SmiCmpVal`) rather than a polymorphic dispatch.
2. **`src/compiler/recompile.rs`** (S14 step 8, "recompile-on-trap loop,
   closes cold deopt-storm") already exists specifically to handle
   *repeated* traps at the same site by triggering a recompile. Worth
   checking FIRST, empirically, why it isn't already healing this site —
   either it doesn't fire for this trap shape, or it fires but the
   recompiled version still can't do better than trap-the-other-arm (in
   which case the real fix is elsewhere: teaching the recompiled version to
   actually compile both arms, not just re-guess which one is dominant).

**Where to start looking, concretely:** `src/compiler/ir.rs` (`Ir::SmiCmpVal`,
`Ir::SmiCmpBr`, `Ir::UncommonTrap`, `classify_smi_send`), `src/runtime/recompile.rs`,
`docs/PERF.md`'s S15 A6/A7 section, `world/bench/richards.mst:330`.

**Confirmed a general problem, and `sieve` is the CANONICAL repro (2026-07-08).**
Gap 1 is not Richards-specific — `world/bench/sieve.mst` hits the identical
storm and is a far cleaner development target (one small method, one hot
bytecode, no scheduler machinery or 7-arg constructors around it). Evidence
(release, `MACVM_JIT=threshold=1`): sieve is dead flat vs the interpreter
(~87 ms both), yet it DOES compile (`MACVM_TRACE=stats`: `compilations=60`,
`osr_entries=30`, `contexts_allocated=0`) — it just storms on one branch
(`MACVM_TRACE=deopt`: `nm=8 bci=155` repeatedly, `deopt_count=65 [trap 65]`,
`recompiles=18`, `recompile_declined_ineffective=0`). The
`recompiles=18` / `declined=0` pair is the precise fingerprint of "two arms
equally likely but the dominant one keeps *shifting*": every recompile finds
a changed profile, so `recompile.rs` keeps chasing a moving target instead
of converging (exactly the "it fires but still re-guesses" outcome point 2
above predicted). Full numbers in `docs/PERF.md` ("Dual-arm branch storm"
entry). Develop and gate the fix against sieve first, then re-check Richards.

## Gap 2 — methods with argc > 5 are never compile targets, period

**Where:** `world/bench/richards.mst:384-407`, the `Scheduler` task
constructors — e.g.:

```smalltalk
createIdler: identity priority: priority work: work state: state [
    self addTask: (IdleTask link: taskList identity: identity
            priority: priority work: work state: state
            scheduler: self data: IdleTaskDataRecord create)
        identity: identity
]
```

`link:identity:priority:work:state:scheduler:data:` (on `TaskControlBlock`
or a subclass — didn't chase down the exact defining class, just confirmed
the send site) has **7 keyword parts** — 7 real arguments.

**The exact mechanism — confirmed at two SEPARATE checks in
`src/compiler/driver.rs`, don't conflate them:**

- `driver.rs:114`, `eligibility_detail`: `method.argc() > 5` → the method is
  `NoPermanent` — it can **never** become a compiled nmethod, full stop,
  regardless of who calls it. `link:identity:priority:work:state:scheduler:data:`
  (argc=7) trips this directly. This is the same blanket-rejection style as
  `method.primitive() != 0` (discussed earlier this session) — a different
  reason, same outcome: permanently interpreter-only as a callee.
- `driver.rs:146`: `InterpreterIc::at(method, ic).argc() > 7` — a
  *different* check, on a send SITE inside a method being considered for
  compilation (matching the `emit_call_send` register-marshaling comment at
  `driver.rs:132-136`: receiver+args go in `x0..x7`, 8 GPRs, so a send with
  more than 7 real args — i.e. 9+ values including receiver — has no
  register home). **This check does NOT reject the 7-arg call above** — 7
  is not `> 7`. So `createIdler:priority:work:state:` itself (argc=4,
  well under the 5-arg cap) is plausibly still compile-*eligible* on its
  own account.

**So, precisely:** the `link:identity:...` constructor itself never
compiles (check #1). Any call to it — including from an otherwise-compiled
`createIdler:...` — is an ordinary c2i-adapter hop into the interpreter
(`src/codecache/adapters.rs`, `AdapterTable`, S11 D6.1) to run it, then
returns. This is **not a trap/deopt of the caller** — it's the same routine
"compiled code calling a permanently-interpreted method" path discussed
earlier this session, just landing on an argc-based exclusion instead of a
primitive-based one.

**Likely much smaller than Gap 1**, worth confirming with real numbers
before spending effort here: Richards creates a small, fixed number of
tasks once at startup (not per scheduler tick), so this is a bounded,
one-time cost — nothing like Gap 1's 60k+ traps in the hot loop. Don't
prioritize this over Gap 1 without first measuring how much it actually
costs (e.g. `MACVM_TRACE=stats` invocation counts on these constructors vs.
the scheduler's steady-state tick count).

**Do not casually raise the `argc() > 7` send-site cap. This is scar
tissue, not an oversight** — read `src/oops/layout.rs:383-396`
(`ROOTSPILL_SLOTS`'s own doc comment) before touching this. `ROOTSPILL_SLOTS`
used to be 6. **Richards' own 7-arg task initializer** — this exact send —
is what broke it: a c2i adapter read args 6/7 from past the 6-slot area,
landing on the stub frame's own saved `fp`/`lr`. Those values happen to
satisfy the SmallInteger tag check, so they flowed silently into the new
object's last two ivars as bogus-but-tag-valid integers — no crash, no
trap, just quietly wrong data. It surfaced *thousands of sends later* as an
unrelated `doesNotUnderstand:` on a SmallInteger, with no visible link back
to the real defect. That's task #87 / S15 "BUG D root cause 4", fixed in
`f62a1e4` by widening 6→8 slots and adding today's `argc() > 7` gate as the
backstop for anything even the widened area can't cover.

8 is AAPCS64's argument-GPR budget (`x0`-`x7`) — receiver+7 args exactly
fills it. That does NOT mean this is uncharted territory: the standard ABI
answer for argument 9+ has always been "caller pushes it on the stack,
callee reads it FP/SP-relative" — completely ordinary, every native ABI
does this. MACVM's JIT just hasn't generated that path yet because nothing
compiled has needed it. Two real options, both discussed 2026-07-08 —
pick based on fresh measurement, not preference, when this is picked up:

**Option A — stack-passed overflow args (native-ABI-style).** The
codebase already has the conceptual piece for the register-resident half:
`emit_stub_prologue` (`src/codecache/stubs.rs:70-84`) spills x0-x7 into the
8-slot `RootSpill` stack area *specifically* so adapters/GC can read them
as memory instead of registers — a shadow space in every meaningful sense.
A genuine caller-pushed overflow tail is a natural extension of that
pattern, not a new invention. But the real cost isn't "can the stack hold N
args" (trivially yes) — it's keeping THREE independently-written consumers
in lockstep on the resulting layout: (a) the caller-side push in
`emit_call_send`, (b) every c2i/mega/dnu stub's read side, and (c) the GC
root-walker (`src/memory/roots.rs:276`, `each_code_root`), which today
hard-`assert`s `n <= ROOTSPILL_SLOTS` and has no notion of a variable-sized
region at all. That synchronization is *exactly* what broke last time —
6→8 slots, done reactively after the fact — enforced today only by a
`debug_assert`, silent in release builds.

**Option B — fixed prefix + rest-array in the last slot (preferred
candidate, refined 2026-07-08).** Better than boxing the whole argument
list: keep x0=receiver, x1..x6=args 1-6 EXACTLY as today — completely
undisturbed, zero new cost — and redefine the LAST slot (x7) so that
whenever total argc ≥ 7, x7 is always an `Oop` reference to an `Array`
holding args 7..N, instead of being unrepresentable past N=7. This is a
plain rest-parameter/varargs-tail convention (same shape as JS `...rest`,
Lisp `&rest`) — and notably it's already native to Smalltalk itself, not
borrowed: `BlockClosure>>valueWithArguments:` (`world/04a_blockclosure.mst:21`,
primitive 54) already does exactly this unpacking, today, for blocks —
`src/runtime/primitives.rs:994-1011`:
```rust
let n = arr.len();
...
for i in 0..n {
    vm.stack.push(arr.at(i));
}
crate::interpreter::blocks::activate_block(vm, cl, n)
```
That's a real, already-tested, already-shipping precedent for "unpack an
Array's elements back into the interpreter's own ordinary stack-based
argument convention" — a c2i adapter's tail-unpack for a rest-array
argument is the same loop shape, just starting at position 7 instead of 0.
(`src/compiler/escape.rs:87` separately excludes `valueWithArguments:` from
inlining — but that's a different, harder problem: ITS array length is a
runtime value, unknown at compile time. A keyword send's arity is baked
into the selector itself — `link:identity:priority:work:state:scheduler:data:`
is always exactly 7, every time — so this design's arity is always
statically known at compile time; it doesn't hit the reason
`valueWithArguments:` is excluded.)

Why this is strictly better than boxing everything: the call's live
register footprint is now ALWAYS exactly 8 slots (receiver + 6 fixed + 1
rest-ref), for ANY N — 7, 70, doesn't matter. `roots.rs`'s root-walker and
`RootSpill`'s `n <= ROOTSPILL_SLOTS` invariant need ZERO changes, not
because we were careful around them, but because the shape they already
support (a small fixed number of Oop-bearing register slots) is *never
exceeded regardless of arity*. It also cleanly retires BOTH Gap 2
sub-checks if adopted as the method's real entry convention, not just a
call-site trick: a compiled method whose own argc > 6 gains a small,
bounded "if argc > 6, destructure x7's array into locals 7..N" prologue
sequence instead of being permanently `NoPermanent` — codegen difference,
not an eligibility rejection.

Two symmetric pieces of genuinely new work, both bounded and mechanical:
c2i (compiled caller → interpreted callee) unpacks the tail array with the
loop shown above; **i2c (interpreted caller → compiled callee) needs the
mirror** — build the tail array before jumping into compiled code. Don't
build only one direction and assume the other falls out for free.

Cost, honestly: one allocation for the tail array, only on sends with
argc ≥ 7 — irrelevant for Richards (one-time startup constructors, not
per-tick). For a hypothetical future HOT high-arity site, S14 step 7's
Context-elision precedent (promote to vregs, skip the allocation when
provably non-escaping) is the natural extension — not built, not needed
here.

**MVP for Richards specifically**: no new Klass — plain `Array new: <tail
length>`, unpacked with `at:`/`at:put:`, both already SMI_INLINE-fused
compiled fast paths (S15, `array_op_kind_on` in `ir.rs`, primitives 26/27).
The genuinely new work is an IR-level rewrite (send with argc≥7 → build
tail-array literal for positions 7..N, marshal receiver+6+arrayref as
today) plus the two prologue/adapter unpack loops above — no new runtime
mechanism, reuses `valueWithArguments:`'s already-proven unpack shape. This
also fails loud (visibly wrong value) rather than silent (tag-valid
garbage) if gotten wrong — the failure mode this project has specifically
been burned by before.

If the napkin math doesn't clearly beat Gap 1's payoff, don't do either
option — leave it capped at 7.

## Gap 3 — S14 residency pool may be leaving free registers unused (unmeasured, found 2026-07-08)

Not from Richards specifically — found while documenting register
conventions ([`VMregisters.md`](VMregisters.md)). Flagging here since it's
a real, very-low-risk perf candidate, distinct from Gap 1/2's Richards-
specific focus.

`assign_residents` (`src/compiler/regalloc.rs:631-644`, S14 perf recovery —
the same mechanism that fixed a 135x regression) gives call-free spilled
intervals a real register instead of a memory round-trip. Its own pool
isn't fixed at `x21`-`x23` — it's already designed to extend itself with
"every `x6`-`x15` register the main allocator left globally unused" for
that method (comment right above the function). That's the mechanism's own
stated intent: use whatever's free instead of spilling it. It stops one
step short of applying that same intent to `x24`-`x27` —
[`VMregisters.md`](VMregisters.md) §1 confirms these four registers are
unclaimed by anything, in every method, always (not just opportunistically
free the way `x6`-`x15`'s slack is).

**Not a confirmed perf loss — a conditional one.** Because the pool already
extends into `x6`-`x15`'s slack, the gap only bites in a method where the
main allocator's `x6`-`x15` usage is already high (little slack to extend
with) *and* has more call-free spill-eligible intervals than the pool ends
up with in that case. Whether any real hot method (Richards, DeltaBlue, or
otherwise) actually hits that condition hasn't been measured — check that
first, e.g. instrument how often/by how much the pool is exhausted in a
real run, before assuming this is worth the change.

**If it does check out, the fix is unusually cheap** for a JIT change: a
one-line extension of the pool initializer (`vec![21, 22, 23]` →
`vec![21, 22, 23, 24, 25, 26, 27]`, `regalloc.rs:634`). Unlike Gap 2, this
needs zero new save/restore machinery — `build_call_stub` already saves and
restores the entire `x19`-`x28` block unconditionally
(`VMregisters.md` §1/§4), so `x24`-`x27` are already covered by every
boundary crossing that matters. No new mechanism, no new invariant to keep
in sync — just measure first.

## Suggested order of attack

1. **Go straight to Gap 1.** It's the real prize, it's contained (one IR
   shape, no ABI/GC redesign), and it's the dominant cost by a wide margin.
   Read `recompile.rs` first (cheapest possible fix if it's "almost"
   already handling this), then `ir.rs`'s `SmiCmpVal`/`DominantWithSlowPath`
   machinery if it isn't.
2. **Leave Gap 2 alone unless the numbers say otherwise.** Confirm its
   actual cost with real trace data if curious, but given it's a bounded,
   one-time startup cost *and* touching it means reopening a boundary that
   already produced a real silent-corruption bug (see above), the bar for
   "worth it" is high. Don't start here.
3. **Gap 3 is worth a quick measurement pass regardless** — cheap to check,
   cheap to fix if confirmed, and orthogonal to Gap 1/2 (touches regalloc's
   residency pool, not branch compilation or arity caps). Don't spend real
   implementation time until the pool-exhaustion check actually shows it
   matters.
4. Re-run `scripts/perf.sh --release` (the T5 procedure) after each change,
   same as every other perf entry in `docs/PERF.md` — keep the same
   before/after table format so this integrates with the existing log
   rather than starting a new one.
