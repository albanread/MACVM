# Adversarial Review: closure_compilation_design.md

Status: IN PROGRESS — findings appended incrementally as verified.
Reviewer: adversarial challenger agent, 2026-07-08.
Rule: no source or design-doc modifications; this file is the only output.

Findings are ranked most-severe-first WITHIN each section; a final ranked
summary + verdict appears at the end once all attack surfaces are covered.

## 1. Citation audit (file:line claims vs real code)

Verified accurate so far (checked against working tree 2026-07-09):

- `blocks.rs:52-123` activate_block, `:57` bump_invocation, `:70` receiver=copied(0),
  `:85-95` ctx wiring (real lines 81-95), `:113-118` value captures, `:142-177`
  make_closure, `:186-204` home_ref_for_new_closure, `:193` receiver-slot read,
  `:499-526` ctxless_block_aliases, `:579-587` block_own_ics_counters — ALL CORRECT.
- `compiled_call.rs:61-200` enter_compiled shape, `:64-68` nlr debug_assert (real
  65-68), `:150` BAILOUT check, `:156-158` emit_nlr_check comment, `:179-187` NLR arm
  (take nlr_state + continue_unwind) — ALL CORRECT.
- `compiled_call.rs:85-97` cited for "NotEntrant's patched entry routes to the deopt
  path": the real lines 85-89 are a COMMENT documenting that the S13 10b poll-deopt
  path deliberately enters a NotEntrant nmethod via its patched entry. The citation is
  fair, but note it documents entry via `enter_compiled` (a `bl`-shaped call), not via
  a tail-jump `br` — see §4 finding on the not-entrant entry patch.
- DEOPT_STRESS victim selection (`stress_tick`, compiled_call.rs:209-242) iterates
  `code_table.iter_alive()` — arena-level, so block nmethods WILL be round-robined as
  the design claims in §4. CONFIRMED-SOUND.

## 1a. Early findings from blocks.rs / compiled_call.rs

**[MAJOR] — `prim_valueN`'s `Completed => return Ok(...)` arm double-adjusts the stack
as written.** Design §2.1 pseudocode: `Completed => return Ok(top-of-stack result
convention)`. Real code: `enter_compiled` on the Completed path ALREADY does
`vm.stack.sp = base; push(result)` (compiled_call.rs:197-198) — receiver+args are gone
and the result is deposited. If `prim_valueN` then returns a `PrimResult::Ok(v)`-shaped
value, the prim call site in `interpreter::send` will do its own pop-args-push-result
fixup over an already-fixed stack (the doc of EnterResult::Completed says "the caller
does nothing further — identical to PrimResult::Ok's own handling", i.e. Completed IS
the post-fixup state). The design needs either (a) a `PrimResult` arm meaning
"already deposited, do nothing" (Activated is close but sets regs.method/bci), or (b)
`prim_valueN` must pop the deposited result and return it as `Ok(popped)` so the
caller's fixup is a net no-op — and (b) must be argued safe against the send-path's
sp bookkeeping. The doc's one-line pseudocode hides a stack-discipline decision in
exactly the class of code (activation boundaries) where this VM's worst historical
bugs lived. Fix: specify the arm precisely; add a differential test with a value: send
whose result is consumed immediately (`(blk value: x) + 1`).

## 2. NLR scope cut

**[BLOCKER] — Dead-home NLR from a compiled block panics the VM (or sends
#cannotReturn: to the wrong object).**
- Design claim (§2.4 "BlockCannotReturn timing"): "the compiled origination does the
  same (park + sentinel without checking) — liveness is evaluated exactly once, at the
  same point in the sequence the interpreter evaluates it. No behavioral drift for the
  differential tests to catch." And §2.4's table row 3: "on this path the current frame
  IS an interpreter frame ... so that read is unchanged".
- The attack (all existing code, verified): `cannot_return_current_closure`
  (unwind.rs:374-380) reads the closure from **the current frame's receiver-arg slot**
  and `.expect()`s it to be a Closure:
  `ClosureOop::try_from(closure_oop).expect("...receiver slot is not a Closure")`.
  Today that is sound because `nlr_tos` only executes inside the block's OWN
  interpreter frame, whose receiver-arg slot holds the closure (activate_block shape,
  blocks.rs:63-70). With compiled origination the sequence is: `rt_nlr_originate` parks
  {home, value} WITHOUT checking liveness → block nmethod's native frame unwinds via
  its epilogue (gone) → sentinel reaches `enter_compiled`'s NLR arm
  (compiled_call.rs:179-187) → `continue_unwind(vm, home, value)` → `home_is_live`
  fails FIRST (unwind.rs:176) → `cannot_return_current_closure` reads the receiver
  slot of whatever interpreter frame is current — which is the **value:-sender's
  frame** (activate_block never ran; no block frame exists). Its receiver-arg is the
  sender's own receiver: almost never a Closure.
- Concrete input: `m := self makeBlock. m value` where `makeBlock ^[^1]` (home
  returned before the block ran — the design's own repro (3) scenario). Result: the
  `.expect()` panic aborts the VM — in RELEASE too — on legal guest code that today
  produces a clean `cannotReturn:`. If the sender's receiver happens to be some other
  closure, release builds instead send #cannotReturn: to the WRONG closure silently.
- What's wrong in the design's reasoning: liveness's VALUE is indeed the same at
  origination and at the boundary (nothing between can kill an interpreter home), but
  the CONTEXT of the check (which frame is current when it fails) is what
  `cannot_return_current_closure` depends on — and that context IS different.
- Minimal fix: carry the originating closure through the escape. Either (a) extend
  `NlrState` (vm_state.rs:261-264, currently {home, value}; NOT a GC root today —
  fine, it rides the same no-alloc-in-transit contract, compiled_call.rs:170-178) with
  the closure oop, and make `continue_unwind`'s dead-home arm call the EXISTING
  public `cannot_return(vm, closure, value)` (unwind.rs:391) when a parked closure is
  present; or (b) have `rt_nlr_originate` do the `home_is_live` check itself and take
  a dedicated cannot-return path that has the closure in hand (x0). Option (a) is
  smaller and keeps the single-evaluation property. Interpreted `nlr_tos` origination
  is untouched either way.

**[MAJOR] — A3's transitive NLR-free check is dismissed as "vacuous"; it is
load-bearing, and skipping it re-creates the dead-home bug on legal code.**
- Design claim (§2.7): "the block B_s (and transitively any blocks IT creates —
  vacuous in v1 since block bodies with `PushClosure` don't compile anyway) contains
  NO `NlrTos`".
- The attack: whether B_s itself COMPILES is irrelevant — B_s runs INTERPRETED and
  still creates its inner blocks. `home_ref_for_new_closure` (blocks.rs:186-204,
  verified) copies the enclosing closure's home **verbatim** when the current frame is
  a block frame (line 192-196). Sequence: A3-compiled M does `Ir::AllocClosure` for
  B_s = `[:x | coll do: [:y | ^y]]` with `home = HOME_DEAD_SENTINEL`. B_s has no
  `NlrTos` of its own (the `^y` is in the inner literal), so the NON-transitive scan
  passes and M compiles. B_s is invoked → runs interpreted (PushClosure gate blocks A1
  compilation, which changes nothing) → `make_closure` for `[:y | ^y]` copies B_s's
  closure's home verbatim = HOME_DEAD_SENTINEL. The inner block's `^y` — while M's
  compiled frame is STILL LIVE on the stack and Smalltalk semantics require a
  successful NLR to M — hits `home_is_live` → proc!=0 → false → `cannotReturn:` (or
  the design's own proposed debug assert `home.proc == 1 → panic`). Legal code, wrong
  answer in release, VM panic in debug.
- Note the check IS statically decidable: `PushClosure <lit>` names a CompiledBlock
  literal, so the eligibility scan can recurse through the literal tree
  (B_s's literals → inner CompiledBlocks → their bytecode) looking for `NlrTos`.
  ifCurtailed:/ensure:-handler blocks created inside B_s are covered by the same
  recursion.
- Minimal fix: strike the word "vacuous"; specify the transitive scan as a real
  implemented check in A3's eligibility (recursive walk over PushClosure literals of
  every escaping B_s). Cheap (compile-time, per-site, memoizable on the CompiledBlock).

**[CONFIRMED-SOUND] — sentinel relay through compiled middle frames, and the §2.4
case table's rows 1, 2, 5.** Verified against real code: every compiled send site gets
`emit_nlr_check` after the call (design's mechanism claim matches
compiled_call.rs:154-187 and unwind.rs); `continue_unwind` re-validates liveness on
every entry (unwind.rs:175-176); the scan reports `Marked` before `CrossedBoundary`
(unwind.rs:316-331, marker check at line 322 precedes the boundary check at line 326),
so handlers on interpreter frames run innermost-first regardless of how many compiled
block frames sit between origin and home; `resume_unwind` re-parks on a re-escape
(unwind.rs:259-266). A block that `value:`s another closure which NLRs through it
relays via the middle block's own site NLR check — same mechanism, no new plumbing.
Row 5 ("ensure: BETWEEN two compiled frames — impossible") holds: prim_ensure_like
(primitives.rs:1017-1046) arms markers only on interpreter frames it just pushed.

## 2a. The activate_block split — two call sites the design never mentions

**[MAJOR] — unwind.rs's handler activations (lines 131 and 228) call
`activate_block` directly and are assigned to NEITHER half of the design's split; if
they get the compiled-entry path, the BCI_RESUME_* protocols collapse.**
- Design claim (§2.1): split `activate_block` into "the public trigger-bearing entry
  (used by prims 50-53) and `activate_block_interp` (used by 54/60/61 and by the
  deopt/fallback paths)". Also: "The trigger mirrors `activate_method`'s: ... on
  success, enter compiled immediately".
- Reality: `activate_block` has EXACTLY 8 callers (verified by grep): prims 50-53
  (primitives.rs:966,973,980,987), prim 54 (1010), prims 60/61 (1038), **and the two
  the design never lists: `intercept_ensure_return` (unwind.rs:131) and
  `continue_unwind`'s Marked arm (unwind.rs:228)**. Both activate ensure:/ifCurtailed:
  HANDLER closures and both depend on the interpreter-frame resume protocol: they set
  `vm.regs.bci = BCI_RESUME_ENSURE_RET / BCI_RESUME_UNWIND` immediately before the
  call so activate_block bakes it into the new frame's saved_bci (blocks.rs:66-68);
  the handler's normal return then routes through `pop_and_deliver` →
  `resume_ensure_ret`/`resume_unwind` (unwind.rs:99-101). "Mirrors activate_method"
  is not hypothetical: activate_method really does compile AND `enter_compiled`
  inline at the trigger (send.rs:257-265). If activate_block does the same and a
  handler block is warm (threshold=1 in the standing test matrix: the second
  activation of any given handler), the handler body runs compiled, no interpreter
  frame carries the resume sentinel, the parked result/token is orphaned on the
  operand stack, and dispatch resumes at a sentinel bci — stack corruption. The
  debug_asserts at unwind.rs:132/229 (`activated == Activated`) only catch it if the
  entry returns something else; they do not protect the state.
- This failure is LOUD (world tests at threshold=1 hit it immediately), so it costs a
  bring-up cycle rather than shipping a silent bug — but the design text as written
  guarantees a first implementation gets it wrong or right by luck.
- Minimal fix (one paragraph in §2.1): enumerate all 8 call sites; route unwind.rs:131
  and :228 to `activate_block_interp` explicitly; state that compiled entry happens
  ONLY on the prim 50-53 path. (Handler activations must stay interpreted in this arc
  — their resume protocol has no compiled counterpart, same reasoning §2.6 already
  applies to 60/61's marker arming.)

**[MINOR] — `prim_valueN`'s `Completed => Ok(...)` works only because
`try_primitive`'s Ok arm restores sp ABSOLUTELY; pin that with a comment/assert.**
(Supersedes the preliminary MAJOR in §1a; severity downgraded after reading
send.rs:96-106.) `enter_compiled` Completed deposits the result itself
(`sp = base; push(result)`, compiled_call.rs:197-198). If prim_valueN then returns
`PrimResult::Ok(popped_result)`, `try_primitive` does `vm.stack.sp = base` — an
ABSOLUTE restore computed before the prim ran (send.rs:96-97,104-106) — then the
dispatch pushes the value; net effect correct. But this correctness hangs on the
absolute (not relative) restore and on nothing allocating between the pop and the
push. Also the new `PrimResult::Nlr` arm must NOT go through the `sp = base` path at
all (after an NLR the stack belongs to a different activation; `base` may be past sp
or inside a live frame) — try_primitive needs a fourth `PrimitiveOutcome` arm that
touches nothing, and `run_method`'s try_primitive caller (the rt_interpret_call
door, send.rs:51-69) needs the same mapping. All mechanical, all
exhaustive-match-enforced; listing them in the design prevents an ad-hoc scramble.

## 2b. Ground-truth errors found while auditing

**[MAJOR] — §1.1's "its `holder` is the home method, not a class" is FALSE for every
installed method; SPEC.md:339-341 is stale against the code.**
- Design claim (§1.1): "its `holder` is the **home method**, not a class
  (`docs/SPEC.md:339-341`)" — presented as verified ground truth.
- Reality: `install_method` runs `patch_block_holders` (src/runtime/lookup.rs:210-226),
  which recursively stamps **the KLASS** into the holder of every CompiledBlock
  reachable through the installed method's literal pool — precisely so that super
  sends inside blocks resolve (`ic::resolve` reads `caller.holder()` where caller is
  the executing CompiledBlock, ic.rs:176-196). The design quotes SPEC accurately; SPEC
  is stale vs the working tree. "All file:line references verified against the working
  tree" verified that SPEC says it, not that it is true.
- Why it matters (mostly benign, one real consequence + one hidden win):
  (a) Any implementation logic derived from "holder = home method" (e.g. using holder
  to reach the home method for debugging/tooling, or reasoning about the by_block
  weak-key sweep skip) would be wrong — there is currently NO in-object path from a
  CompiledBlock to its home method; the design should say so explicitly.
  (b) HIDDEN WIN the design missed: super sends inside compiled block bodies actually
  WORK through the standard path — ir.rs:2918-2921 does
  `KlassOop::try_from(self.method.holder())`, which succeeds for installed blocks
  because holder IS the klass. The design never mentions super-in-block at all (silent
  gap): A1's eligibility text should state super sends in block bodies are allowed and
  why they are sound without an entry klass guard (super lookup start is site-static
  `holder.superclass()`, ic.rs:176-180 + ir.rs:2896-2921, and instvar layouts are
  prefix-stable down the subtree — the same argument §2.1 makes for instvars; S14
  step-5's no-guard super inlining stays sound for the same reason even though a block
  nmethod has no entry check to "prove" the receiver klass).
  (c) Edge: blocks in a never-installed doIt have an unpatched holder; a super send
  there panics the interpreter today (ic.rs:189-190 `.expect`) before any compile
  could — pre-existing, not this design's problem, but worth one sentence.
- Minimal fix: correct §1.1; add the super-send paragraph to §2.1; file a SPEC.md
  correction as a separate follow-up.

## 3. ensure:/ifCurtailed: interaction

**[CONFIRMED-SOUND] — the §2.6 deferral is coherent; every ensure:-vs-compiled-block
sandwich I could construct resolves through existing machinery** (given the §2 and
§2a fixes). Constructions attempted, all traced through real code:
- *Compiled block frames BETWEEN the NLR origin and an armed marker*: markers only
  ever live on interpreter frames (prim_ensure_like arms the frame it just pushed via
  activate_block_interp, primitives.rs:1031-1043). The unwind visits each interpreter
  segment exactly when the escape crosses it (`enter_compiled`'s NLR arm resumes
  `continue_unwind` per segment; the scan reports `Marked` before `CrossedBoundary`,
  unwind.rs:311-331), so handlers run innermost-first regardless of how many compiled
  block frames sit in the native segments between. Compiled frames only relay the
  sentinel — they cannot "hide" a marker because they cannot carry one.
- *ensure: sent from INSIDE a compiled block body*: the ensure: method is
  PRIM_ACTIVATES_FRAME-excluded (driver.rs:52), so the send c2i's; prim 60 runs
  interpreted and pushes an interpreted protected frame with the marker. The
  compiled block frame below it relays any later sentinel. Sound.
- *Protected block or handler block is itself hot/compiled*: the activation paths
  for 54/60/61 and the two unwind.rs handler sites MUST stay interpreted — this is
  finding §2a (the design covers 54/60/61 but not unwind.rs:131/228).
- *Handler re-escape mid-unwind*: resume_unwind → continue_unwind re-parks
  (unwind.rs:259-266); rt_nlr_originate's debug assert mirrors
  enter_compiled:65-68's. The design's Risk 4 analysis is accurate.
- Residual (documented, acceptable): blocks invoked BY ensure:/ifCurtailed:/
  valueWithArguments: never run compiled in this arc; ensure: frequency in the
  DeltaBlue tail is nil (verified: no ensure: in the planner hot loops,
  deltablue.mst).

## 4. by_block registry + stub_value_dispatch (GC rehash, NotEntrant races, redefinition)

**[BLOCKER] — §2.3's NotEntrant dispatch rule is built on a misreading of two
mechanisms; as designed it ABORTS the VM (A1 door) or livelocks (A2 door) the moment
a block nmethod is invalidated — which MACVM_DEOPT_STRESS does continuously.**
- Design claim (§2.3, rt_value_target step 3): "`by_block.get(blk)` → `Some(nm)` with
  `nm.state` Alive-or-NotEntrant → return `nm.entry` (NotEntrant's patched entry
  routes to the deopt path by design — the same self-healing property
  `enter_compiled` relies on, `compiled_call.rs:85-97`)". §2.1's prim pseudocode has
  the same probe feeding `enter_compiled`.
- Reality, three separate misreadings (all verified):
  1. A NotEntrant nmethod's patched entry is `b not_entrant_stub`
     (flush.rs §2b, nmethod entry patch), and `not_entrant_stub` is a RESOLVE stub,
     not a deopt path: it tags KIND_RESOLVE and calls
     `rt_resolve_send(vm, ret_addr=x30, argv)` then `br`s the result
     (stubs.rs:858-876).
  2. `rt_resolve_send` begins with `find_caller_site(vm, ret_addr)`, which does
     `code_table.find_by_pc(ret_addr).expect("ret_addr must fall inside a published
     nmethod (D4.1)")` and then requires an IcSite at exactly `ret_addr - 4`
     (stubs.rs, find_caller_site). It PANICS for any arrival whose return address is
     not a compiled send site.
  3. `enter_compiled` has NO self-healing for NotEntrant targets — the opposite: its
     callers guarantee Alive before entry. send_generic's IC-smi dispatch
     re-validates `matches!(nm.state, NmState::Alive)` plus key identity
     (send.rs:319-330, "Stale-id self-heal" = validate-then-skip, not
     enter-and-recover), and activate_method's trigger filters Alive
     (send.rs:203-207). compiled_call.rs:85-97 (the cited lines) are about the
     zombie SWEEP's Alive guard, and the "poll-deopt enters NotEntrant" comment
     refers to compiled SEND-SITE entries (`bl` from a real site, where
     rt_resolve_send works because ret_addr IS a site).
- Concrete failure sequences:
  - A1 door: DEOPT_STRESS invalidates block nmethod NB (stress_tick round-robins all
    Alive nmethods, compiled_call.rs:209-242). Next `blk value:` → prim_valueN →
    by_block probe returns NB (NotEntrant per the design's rule) → `enter_compiled` →
    call stub → patched entry → not_entrant_stub → rt_resolve_send with ret_addr =
    a CALL-STUB-internal pc → `find_by_pc(...).expect(...)` → **VM abort**. The
    design's own §4 stress matrix (DEOPT_STRESS=64 deltablue soak) hits this within
    the first invalidation cycle.
  - A2 door: stub_value_dispatch `br`s to NB's patched entry with lr = the original
    value: site (a real site, so no panic) → rt_resolve_send re-resolves selector
    `value:` → per A2's own routing rule the site re-routes to stub_value_dispatch →
    `br` back into the stub → by_block probe returns the same NotEntrant NB → **br
    patched entry → resolve → stub → ... infinite loop** (no progress: nothing on
    this cycle bumps a counter, recompiles B, or removes the by_block entry).
- Note the design also contradicts itself: §2.1 says by_block entries are "removed on
  flush exactly where by_key entries are removed" — but by_key is unhooked at
  INVALIDATION time, inside `set_not_entrant` (nmethod.rs:664-678, with the
  install-replacement-first successor guard), not at zombie/flush time. Following
  "same place as by_key" literally means a NotEntrant nm is never in by_block at all
  — which is the CORRECT behavior and directly contradicts §2.3's
  "Alive-or-NotEntrant".
- Minimal fix (three coordinated edits to §2.1/§2.3):
  1. by_block unhooks in `set_not_entrant` exactly where by_key does (same successor
     guard: only remove if the entry still points at this id).
  2. `rt_value_target` and prim_valueN's probe require `NmState::Alive`; anything
     else → 0 / interpret. Drained blocks recompile via the existing
     activate_block counter trigger; add the send.rs:203-207-style reuse-Alive check
     to compile_block so re-triggers reuse a fresh replacement instead of storming.
  3. Delete the "routes to the deopt path" sentence; in-flight invalidation coverage
     comes from §2c return-redirect + §2d poll (which DO work for block frames —
     they operate on frames/return addresses, not entries).

**[MAJOR] — "Mirror the adapters' now-fixed rehash hook" — no such hook exists; the
#93 fix was verify-on-hit, and the real rehash precedent is CodeTable's own
update_keys/rehash.**
- Design claim (§2.1): by_block entries "(b) rehashed after every moving GC — the
  AdapterTable S15 #93 lesson ... Mirror the adapters' now-fixed rehash hook, same GC
  phase."
- Reality: AdapterTable's keys are STILL never rehashed. The module doc says so
  explicitly ("Rehashing `by_method` after a moving GC ... deferred",
  adapters.rs:9-13), and the #93 fix (adapters.rs:51-70, `get_or_make`) is
  **verify-on-hit**: compare the cached blob's embedded, oops_do-maintained method
  pool word against the probe key and rebuild on mismatch. The thing the design
  should tell the implementer to mirror is `CodeTable::update_keys` + `rehash`
  (nmethod.rs:695-800), which run under BOTH collectors (scavenge passes a
  scavenge_oop closure; full GC passes forward_chase — the comment block spells out
  the exact use-after-free that skipping scavenge caused historically). by_block
  lives in CodeTable per the design, so riding the same two call sites is natural.
- Why the mechanism choice matters for by_block: verify-on-hit ALONE (the adapters'
  actual fix) is correct but leaky here — after a full GC moves B1, B2 can be
  allocated at B1's old address; the probe for B2 false-hits B1's stale entry, the
  verify rejects, and the rebuild path INSERTS B2 under that key, silently orphaning
  B1's nmethod from the registry (B1's closures then re-compile a duplicate — a slow
  registry-churn leak under live-edit/GC pressure). Rehash-on-move (keys are always
  live: the nmethod pool pins B, so every key has a forwarding pointer at GC time)
  keeps the table exact. Recommend: rehash via the CodeTable phases + keep the
  design's debug validate pass; optionally ALSO verify-on-hit as defense in depth
  (one load + compare against the pool word).
- Also fix the wording "removed on flush": removal is at set_not_entrant +
  mark_zombie successor-guard (see BLOCKER above).

**[MINOR] — Redefinition/live-edit (S22-E) semantics check out, with a bounded leak
the design should name.** Browser redefinition of a method M whose literal frame
holds B: make_not_entrant targets M's nmethod via by_key; NOTHING invalidates B's
nmethod NB — and that is semantically CORRECT (old closures over B must keep running
old code; new M' has a NEW CompiledBlock B' which compiles fresh). But NB + B are
then immortal: NB never goes NotEntrant (nothing invalidates it), its pool pins B,
and by_block pins nothing extra but never shrinks. Under live-edit churn this is a
monotonic code-cache + heap leak with no reclamation trigger (zombie sweep only
frees drained NotEntrant code). Cheap mitigation worth one design sentence: when
redefinition invalidates M's nmethod, ALSO make_not_entrant_lazy every by_block
nmethod whose B sits in M's OLD literal frame IF no live closure references it —
or simply accept and document the leak with a MACVM_TRACE=jit counter so it is
observable. (deps.rs inlining dependencies are separate and already handled for
Phase B splices by the existing S13/S14 dependency machinery.)

**[MINOR] — A2 routing must live in `resolve_target_entry` (or equivalent), and the
shared stub's fallback needs a specified source for the value:-method hint.**
Two underspecifications in §2.3: (a) "value-family site resolution routes to it" —
if the routing check lives only in rt_resolve_send's Unresolved arm, then a value:
site that ever goes polymorphic (a non-closure receiver arrives: Mono(closure_klass)
→ PIC promotion) builds its closure_klass PIC pair via `resolve_target_entry`, which
would hand back the c2i adapter — silently dropping compiled-block dispatch for that
site forever (correct but slow, and invisible). Putting the selector∈{value..
value:value:value:} ∧ target-prim∈{50..53} check inside resolve_target_entry makes
every lattice state inherit the routing. (b) The stub's fallback "behaves exactly
like c2i_shared with method = the value: method" — c2i adapters bake a per-method
pool word kept current by AdapterTable::oops_do; a SHARED per-argc stub cannot bake
one method for all receivers, and a baked-but-unmaintained oop would be a #93-family
stale-oop bug. Simplest: the fallback calls a Rust helper that re-looks-up
(klass_of(x0), selector) itself — no baked oop at all — before rt_interpret_call
(which re-validates dispatch truth anyway, stubs.rs:1107-1149). Say which.

## 5. Deopt / materializer / OSR inside block bodies

**[MAJOR] — The design converts every block bailout into an UncommonTrap but never
extends the S14 recompile-on-trap loop to block nmethods; block trap storms can never
resolve, and `note_uncommon_trap`'s key lookup misfires on the filler key.**
- Design claims: §2.1 "The converter must lower every would-be-bailout in a block
  compilation as an S13 `UncommonTrap`"; §2.1 "`is_block` nmethods simply skip the S12
  D5 weak-key sweep (their `key_klass` field is set to a well-known filler and never
  consulted)". The design never mentions recompile.rs at all.
- Reality: `rt_uncommon_trap` calls `note_uncommon_trap` after EVERY trap
  (recompile.rs:42). That function reads `nm.key_klass`/`nm.key_selector` (lines
  50-51) and resolves the method via `lookup(vm, key_klass, method_sel)` (line 69).
  "Never consulted" is false — this path consults it on every second trap
  (UNCOMMON_TRAP_LIMIT=2). For a block nmethod: (a) if the filler key makes lookup
  fail, the function returns early — a cold-compiled block (Empty inner ICs at
  threshold=1, or any persistent guard storm) re-traps FOREVER; this re-opens for
  blocks the exact S14 deopt-storm (~6x slower than interpreting) that recompile.rs
  exists to close, in the very code the design makes trap-dense by outlawing
  `Ir::Bailout`. (b) If the filler is a real klass and its lookup of the block's
  placeholder selector succeeds — every CompiledBlock's selector is the interned
  symbol `#aBlock` (src/bytecode/builder.rs:597: "CompiledBlocks are never looked up
  by selector; a fixed placeholder") — the function snapshot-profiles and can
  version-recompile an UNRELATED method under (filler, #aBlock), churning by_key with
  spurious entries. Not wrong-code, but exactly the "raw-key table does something
  surprising" family the design cites #93 to avoid.
- Minimal fix: `note_uncommon_trap` branches on `nm.is_block` (or on the filler)
  BEFORE the lookup: resolve B from the nmethod's own oop pool, `snapshot_profile`
  over B's own ics (works unchanged — blocks have their own ics, SPEC §4.4), recompile
  via a `compile_block_versioned` that replaces the by_block entry install-first, then
  `make_not_entrant_lazy` the old one — mirroring lines 88-107. Also give the design's
  "filler" a concrete value lookup() can never succeed on, and say what
  `key_selector` holds for a block nmethod (it will naturally be `#aBlock` — fine
  once the branch exists).

**[CONFIRMED-SOUND, with one citation nuance] — the root-is_block materializer arm's
shape (§2.5).** Verified against deopt.rs:440-620: the M3 loop pushes receiver, args,
then `push_frame`, then overwrites temps — so overloading `scope.receiver` with the
closure gives exactly activate_block's caller-side shape (closure at fp-argc-1), and
step 3's FP+4 overwrite + step 4's FP+3 alias slot in cleanly next to the existing
is_block arm (deopt.rs:533-541). `materialize_closure` (deopt.rs:153-177) and the
root-home comments (146-147) are as cited. Nuance: the design says "the code even
`expect`s 'is_block frame is never outermost' (deopt.rs:535)" — that expect is on
`root_fp` INSIDE the non-outermost is_block arm; an outermost is_block scope today
would take the ELSE branch (`materialize_context`) silently, not panic. The new arm
must therefore be keyed on "is_block AND outermost" exactly as §2.5 says — but don't
expect the existing code to loudly reject the unhandled case during bring-up; add the
arm's own assert.
- Two things §2.5 must ALSO pin that the doc leaves implicit: (i) the root block
  scope's `slots` must cover argc + ntemps(B) unified slots (the M3 loop
  debug_asserts this count, deopt.rs:471-477 — value-capture temps INCLUDED, from
  recorded ValueLocs, matching design step 5); (ii) after the nested resume, a
  `nlr_tos` in the re-interpreted block reads the closure from the receiver-arg slot
  — the materialized closure must be the REAL closure from the scope's receiver
  ValueLoc, never a re-allocation (design says this; keep it an assert too, since a
  fresh closure would break home/`copied[1]` identity — Risk 2's exact failure).

**[MINOR] — OSR exists (src/runtime/osr.rs, live since S15) and the design never
mentions it; the protections that make it safe are incidental and should be pinned.**
- Block frames never OSR today: `rt_osr_request`'s dispatch-truth guard
  (osr.rs:94) requires `lookup(klass_of(home receiver), method.selector()) == method`,
  which can never hold for a CompiledBlock (selector is the `#aBlock` placeholder) —
  every hot loop inside an interpreted block body declines OSR forever. Fine for the
  gate (DeltaBlue's hot loops live in methods), but it is a standing perf cliff for
  block-internal loops (`[... whileTrue ...] value` shapes) that A1 does NOT fix —
  worth one sentence in §2.1 and one in §4's tail-attribution discussion.
- A3 interacts with OSR only through `driver.rs:601`:
  `if osr_bci.is_some() && (method.has_ctx() || method_has_closure(method))` →
  decline. This ALREADY fences OSR away from every A3-shaped method (escaping
  closures/materialized Context) — but it is a separate guard from
  `eligibility_detail`, and A3's §2.7 text ("the driver's has_ctx reject ... relax
  accordingly") could plausibly be read as touching it. The design must state
  explicitly: driver.rs:601 STAYS. Removing it would let an OSR entry bind a
  compiled ctx_vreg to a FRESH Context while live closures alias the interpreted
  frame's old one — Risk 2's world-split, in the flagship benchmark
  (`removePropagateFrom:` = whileTrue: + escaping closures = an OSR trigger the day
  A3 lands).
- Deopt-in-block-then-loop: after a mid-loop trap in a compiled block, the
  re-interpreted block frame's loop counter WILL fire `rt_osr_request` on the block
  frame — declined per above (safe), but this means trap-then-reinterpret of a
  loopy block never tiers back up mid-activation; only the next `value:` does. Note
  it in §4's perf expectations.

## 5a. Phase B gate arithmetic

**[MAJOR] — B4 cannot splice `inputsKnown:`: its block is multi-basic-block, and NO
B-slice widens the single-straight-line spliceability requirement; the design's
"zero DeltaBlue methods need A4 once B4 splices inputsKnown:" claim (§6) is
unsupported as scoped.**
- Design claims: §3 B4 — "inputsKnown:'s block (`^false` + sends) trips
  `has_nlr && has_send` (escape.rs:170-178) ... the fix is mechanical: record the
  block's receiver-slot as ValueLoc::ElidedClosure"; §2.7 — "`inputsKnown:` (NLR
  block) needs Phase B's splice widening instead"; §6 — "§1.6's census shows zero
  DeltaBlue methods need it [A4] once B4 splices `inputsKnown:`".
- Reality: the block is
  `[:v | ((v mark = mark) or: [ v stay or: [...] ]) ifFalse: [ ^false ]]`
  (deltablue.mst:322-324, verified). The or:/ifFalse: chains frontend-inline into
  BRANCHES, so the block body is multi-basic-block. `block_is_spliceable` rejects it
  at escape.rs:140 (`cfg.blocks.len() != 1`) BEFORE the has_nlr/has_send check the
  design's B4 fixes — the design's own §1.6 table even lists both gates
  ("escape.rs:140,176-178") for this method, but B4 addresses only the second.
  And the restriction is structural, not just a gate: every splice mechanism in
  ir.rs enforces single-BB + Return (try_inline_leaf ir.rs:1259, the nonleaf splice
  ir.rs:1536-1542, DominantWithSlowPath ir.rs:3446) — splicing a branchy block body
  means nested CFG inlining at a value: site inside an already-CFG-inlined callee, a
  real compiler-construction chunk, not §3's "each one small".
- Consequence for the gates: after A1+A2, inputsKnown:'s BLOCK compiles standalone
  (A1 allows NlrTos + multi-BB) and `inputsDo:` compiles, so only the creator method
  itself stays interpreted (it is not A3-eligible — its closure contains NlrTos —
  and not spliceable). Its own bytecode is small (push_closure + one send + returns),
  so the gate may still pass — but the design must redo §4's tail attribution
  honestly: inputsKnown: contributes its creator-method bytecodes × call count to
  the residual tail, forever, in the committed scope.
- Minimal fix: rewrite B4's claim to "makes deopt-under-spliced-NLR-blocks sound
  (removing the escape.rs:176-178 gate)" and EITHER add a B5 (multi-BB block-body
  CFG splicing — honestly sized) or drop the "inputsKnown: via Phase B" claim and
  re-justify skipping A4 with the smaller residual-tail argument above.

## 6. Dispatch from interpreted callers + tail arithmetic

**[CONFIRMED-SOUND] — interpreted callers DO reach compiled block bodies in A1.**
The hook lives in prims 50-53 themselves, and BOTH doors funnel through them:
interpreted `value:` sends via send_generic → activate_method → try_primitive
(send.rs:270), and compiled `value:` sends (pre-A2) via c2i → rt_interpret_call →
run_method's try_primitive (the factoring at send.rs:51-69 exists precisely so the
c2i door gets primitives). The six orchestrators' per-element sends therefore hit
compiled block bodies from A1 on, not from A2 — the design's phasing claim holds.

**[CONFIRMED-SOUND] — §0's measurements reproduce EXACTLY (release build,
2026-07-09, this review's own run):** `MACVM_JIT=off` → deltablue 265 ms /
102,901,182 bytecodes (design: 265 / 102,901,182); `threshold=1000` → 72 ms /
16,732,417 (design: 71 / 16,732,414; counter noise ±3). Tail = 16.26% ✓ "16.3%".
The compile_disabled census also reproduces: execute, constraintsConsuming:do:,
addConstraintsConsuming:to:, removePropagateFrom: (x2), markInputs:, inputsKnown:,
value:, plus the PRIM_ALREADY_FUSED arithmetic — the design's §0 list is real, not
curated.

**[MINOR] — the <10% gate rests on a qualitative census; the design provides no
per-slice bytecode attribution.** §4 gate 2 says "A1 should cut the block-body
share; A3 the orchestrator share; B the remainder" with no numbers — nothing splits
the 16.7M warm-tail bytecodes between block bodies, orchestrator method bodies,
warmup, and the permanently-interpreted residue (54/60/61-protected blocks,
inputsKnown: proper per §5a, PushClosure-bearing blocks, has_ctx blocks — the last
two are also uncompilable under A1's own gate and appear in
constraintsConsuming:do:-style code the moment a block body keeps a nested literal
block that or:/and: could not inline). The design's own fallback ("re-run the census
before reaching for A4") is honest, but a 2-hour instrumentation (per-method
interpreted-bytecode counters under MACVM_TRACE=count) would convert the gate from
hope to arithmetic BEFORE committing three slices. Recommend doing it during A1.

## 7. Eligibility circularity + fusion detectors + PRIM_ACTIVATES_FRAME

**[CONFIRMED-SOUND] — the fusion detectors are insulated, because the design (perhaps
without saying so) never changes what INTERPRETER ICs hold.** The prim-shims lesson
(driver.rs:54-72: detectors do `MethodOop::try_from(ic.target())` and
classify_smi_send has a hard `.expect`) applies to interpreter ICs read at
caller-compile time. Traced: (a) value:-family METHODS stay PRIM_ACTIVATES_FRAME-
excluded, so `set_mono_compiled` never fires for them (activate_method's trigger
gets compiled=None) — a value: site's interpreter IC target remains the plain
value: MethodOop forever; (b) the design's prim hook deliberately makes "no IC-shape
changes" (§2.1), so block nmethod ids NEVER enter interpreter ICs; (c) detectors
only fuse specific selectors (+, at:, basicNew...), none in the value family; (d)
mono sites whose target is an nmethod-id smi are already handled tolerantly
(driver.rs:441-452 returns Yes; escape.rs:377-381 bails conservatively — B1's
target). The one place new state appears is COMPILED IcSites (a routed site's
`IcState::Mono{klass: closure_klass, target: stub addr}`) — consumed only by
rt_resolve_send/patching, which are target-address-agnostic. No detector reads
compiled IcSites. Keep §2.3's claim honest by stating this insulation explicitly.

**[CONFIRMED-SOUND] — observable-behavior consistency between tiers at value: sites.**
Non-closure receiver: A2 stub → rt_value_target step 1 fails → fallback → c2i-style
re-lookup on the ACTUAL receiver klass (rt_interpret_call re-derives dispatch truth,
verified NLR relay + re-lookup in stubs.rs) → Association>>value / DNU — identical
to today's compiled path and to the interpreter. Wrong argc: rt_value_target step 2
→ 0 → fallback → interpreted prim Fail → value: method's bytecode fallback — the
guest-visible wrongArgc error, same as interpreter (prim_valueN's A1 hook checks
argc BEFORE the by_block probe, preserving order). `valueWithArguments:` (54) stays
un-routed by selector and reads vm.prim_arg_base, which only interpreted dispatch
sets (verified primitives.rs:1005) — consistent.

**[CONFIRMED-SOUND, closes the design's own open item] — `Ir::Bailout` is ALREADY
never constructed by convert(); §1.9's "open verification item" has a clean answer.**
convert()'s dead-block placeholder is `Ir::RetSelf` (ir.rs:5016 — the comment at
ir.rs:4990 saying "trivial Ir::Bailout blocks" is STALE, a leftover), and an
existing test asserts the invariant outright: "Ir::Bailout must never be constructed
by convert()" (ir.rs:5725-5730, post-S14-step-3 when smi fail paths became
UncommonTraps). So no current nmethod can return BAILOUT_SENTINEL mid-body;
`EnterResult::Bailout` is legacy belt-and-braces; compiled-to-compiled linking of
"bailing nmethods" cannot arise. §2.1's no-bailout-in-block-IR rule is therefore
already the global invariant — keep the assert, cite the existing test, and strike
the "slice 2 pre-work" item. (The design's §2.2 return-convention claim "never
BAILOUT_SENTINEL" also gets stronger: there is no emitted producer at all; the
prologue has no stack-limit bailout either — grep of emit.rs found the only
BAILOUT producer at the dead Ir::Bailout lowering, emit.rs:1643-1649.)

---

*Sections 8-10 and the Verdict completed by the reviewing lead (2026-07-09) after the
challenger's time was called (the agent was stopped mid-§8 with sections 1-7 + 2a/2b/5a
substantively complete). Before acceptance, the lead independently re-verified both
BLOCKERs against the working tree: unwind.rs:374-380 (`cannot_return_current_closure`
reads the CURRENT frame's receiver slot and `.expect`s a Closure — with the public
`cannot_return(vm, closure, value)` entry sitting directly below as the fix vehicle);
stubs.rs:858-876 (`not_entrant_stub` = KIND_RESOLVE + `rt_resolve_send`, not a deopt
path); nmethod.rs:664-680 (`by_key` unhooks in `set_not_entrant` under a successor
guard); and the two unwind.rs handler-activation call sites (:131, :228).*

## 8. Environment/GC: captured-state vregs, RootSpill accounting

**[CONFIRMED-SOUND, one convention pin]** (reviewed against the CallPrimitive RootSpill
arm this reviewer built in the prim-shims work):

- `KIND_VALUE_DISPATCH` live count "1 + argc" is EXACTLY the caller IcSite's own `argc`
  field, which already includes the receiver (`CallSiteInfo.argc = ic.argc() + 1` — the
  precise off-by-one roots.rs:361-367 documents from its own first draft). So the arm is
  byte-identical to the `Resolve|C2i|Mega|Dnu` arm: recover via `find_by_pc(caller_pc)`
  + IcSite at `off+4`, use `site.argc` AS-IS. Pin that wording in the design so nobody
  re-adds the receiver.
- `KIND_NLR_ORIGINATE` = fixed 2 (closure, value), same shape as the fixed
  `MustBeBoolean=1`/`AllocSlow=1` arms. Both new kinds need the per-kind unit test the
  design already promises (§2.5) — treat as non-optional.
- The tail-jump discipline is sound: `stub_value_dispatch` runs `emit_stub_epilogue`
  (anchor cleared, x0..x7 restored) BEFORE `br` — the established P4 pattern
  (resolve/mega) — so no stale anchor exists while the block body runs.
- `rt_value_target`/`rt_nlr_originate` never allocate (design states it), so no GC can
  begin inside them in production; the RootSpill counts still matter for
  DEOPT_STRESS-era walks (make_not_entrant's redirect walk traverses live stub frames),
  so they are correctness-relevant, not belt-and-braces.

## 9. Phasing realism (A1 dead code? gate reachability)

**[CONFIRMED-SOUND]** — A1 is not dead code: the challenger's §6 finding traced both
doors (interpreted sends AND pre-A2 compiled sends via c2i → run_method's
try_primitive) reaching the prim 50-53 hook, so compiled block bodies execute from A1
day one, and the threshold=1 world-test matrix exercises every block's second
activation immediately. A1-first is right for the design's own stated reason: all
three hazard machines (frame shape, materializer arm, NLR origination) soak behind the
safest door. Gate reachability: with §5a's amendment the 5x gate rests on
A1+A2+A3+B1-B3; the challenger's §6 MINOR (per-method interpreted-bytecode attribution,
~2h of instrumentation during A1) is ADOPTED as a must-do — it converts the <10% gate
from a qualitative census into arithmetic before three slices are committed.

## 10. Silent gaps (things the design never mentions)

Beyond the challenger's own catches (§2b super-in-block hidden win; §5 recompile.rs
omission; §5 OSR omission; §7's stale ir.rs:4990 comment):

- **[MINOR] PrimResult::Nlr ripple is compile-enforced but should be planned**: every
  `PrimResult` match gains the arm via non-exhaustive-match errors —
  `rt_call_primitive` (codecache/stubs.rs) gets
  `Nlr(_) => unreachable!("50-54/60-61 are PRIM_ACTIVATES_FRAME-excluded")`;
  `try_primitive` gets the fourth `PrimitiveOutcome` arm that must NOT touch `sp`
  (§2a's discipline); `run_method`/`run_method_reentrant` map it like an NLR from a
  send. One sweep; list it in the design so it is planned, not scrambled.
- **[MINOR] Debugger ergonomics**: every CompiledBlock shares the `#aBlock` placeholder
  selector (bytecode/builder.rs:597), so `MACVM_DBG_IR=aBlock` dumps EVERY block
  compile. Acceptable for bring-up; note it in DEBUGGER.md; a home-method filter is a
  cheap later nicety.
- **[MINOR] Code-cache pressure**: many small block nmethods plus per-block versioning
  (§5's fix) raise `code_bytes_alive`; the S14 compile-storm lesson (cache exhaustion
  silently disables the JIT) says carry the reuse-Alive check into the compile_block
  trigger (already part of the BLOCKER-§4 fix) and watch `code_bytes_alive` in each
  per-slice PERF.md row.
- **[NOTE] `snapshot_profile` over block ICs works unchanged** (blocks own their `ics`
  — SPEC §4.4, blocks.rs:579-587); stated because §5's recompile fix depends on it and
  the design never says it.
- **[NOTE] Embed/GUI**: nothing block-specific — `VmHandle::eval` enters through the
  same interpreter door; sigsetjmp recovery and the pthread_exit crash model are
  orthogonal to block frames (same reasoning as any compiled frame).

## Verdict

**IMPLEMENT WITH AMENDMENTS — no redesign required.** The two architectural pillars
SURVIVE the challenge: the NLR scope cut (NLR-bearing closures keep interpreter homes)
is sound once origination carries the closure through the escape — BLOCKER §2 is a
context bug in the dead-home path, not a hole in the cut; and the by_block registry +
tail-jump dispatch stands once the probe rule is corrected to Alive-only — BLOCKER §4
is a misread of two mechanisms, not a flaw in the architecture. Nothing found
invalidates the phasing, the reuse inventory, or the gate strategy. The challenger
also independently REPRODUCED the design's measurements to the byte and CLOSED the
design's one self-declared open item (Ir::Bailout — already a global invariant,
ir.rs:5725-5730).

**Must-fix design amendments before implementation, ranked:**

1. **[BLOCKER §4]** NotEntrant dispatch rule → probe requires `NmState::Alive`;
   `by_block` unhooks in `set_not_entrant` (same successor guard as `by_key`); delete
   the "routes to the deopt path" sentence; add the send.rs:203-207-style reuse-Alive
   check to the `compile_block` trigger. In-flight invalidation coverage = §2c
   return-redirect + §2d poll, which work on frames, not entries.
2. **[BLOCKER §2]** Dead-home NLR → `NlrState` carries the originating closure;
   `continue_unwind`'s dead-home arm calls the existing
   `cannot_return(vm, closure, value)` when a parked closure is present. Repro (3)
   must cover the compiled-block dead-home path explicitly.
3. **[MAJOR §5]** `note_uncommon_trap` branches on `is_block` BEFORE the key lookup;
   `compile_block_versioned` with install-first replacement — otherwise A1's
   trap-dense block nmethods storm forever by construction.
4. **[MAJOR §2a]** The `activate_block` split must enumerate all 8 callers;
   unwind.rs:131/:228 route to `activate_block_interp` explicitly (handler resume
   protocols have no compiled counterpart).
5. **[MAJOR §2]** A3's transitive NLR-free scan is load-bearing: recursive walk over
   PushClosure literals; strike "vacuous".
6. **[MAJOR §5a]** B4 rescoped to deopt-soundness only; redo §4 tail attribution with
   the `inputsKnown:`-creator residual named; drop or re-justify the "zero methods
   need A4" claim.
7. **[MAJOR §4]** Rehash mechanism = ride `CodeTable::update_keys`/`rehash` under both
   collectors (NOT the nonexistent "adapters hook"); optionally verify-on-hit as
   defense in depth.
8. **[MAJOR §2b]** Correct §1.1 (holder = KLASS after `patch_block_holders`); add the
   super-send-in-blocks paragraph (a hidden win); file the SPEC.md:339-341 correction
   separately.

**Then the MINORs**: PrimitiveOutcome fourth arm + sp discipline (§2a/§10);
resolve_target_entry routing + fallback method-hint source (§4); OSR — state
driver.rs:601 STAYS (§5); live-edit leak counter (§4); prim_valueN Completed
comment/assert (§1a/§2a); per-method tail attribution during A1 (§6, adopted as
must-do); tail restatement (§6); debugger/cache-pressure notes (§10); strike the
"slice 2 pre-work" item and cite ir.rs:5725-5730 instead (§7).

**Recommended next step**: one design-doc amendment pass folding the 8 must-fixes into
`closure_compilation_design.md` (each is a paragraph-scale edit; none changes the
architecture), then implement A1 exactly as phased.
