Tracked JIT-bug repros (S15 step 6 — flushed out by the Richards/DeltaBlue
ports; the benchmarks' own 4-mode gates stay red until these are fixed):

1. ic317_reosr_klass_skew.mst — BUG A (Richards blocker, VM ABORT).
   **FIXED** (commit 6145c63): `ic_transition`'s mono→poly arm read
   `selector` AFTER `alloc_poly_pairs`'s own allocation without protecting
   it first — a scavenge mid-transition left it a stale pre-move address.
   Handle-protected across the allocation, then re-derived. This repro now
   passes (prints "9999 10001", exit 0) at every threshold tried.

2. deltablue_projection_t1000_differential.mst — BUG C (silent wrong
   result, STILL OPEN): MACVM_JIT=threshold=1000 -> "Projection test
   3/4 failed" (the exact test number has shifted across investigation
   passes — timing-sensitive, matching which loop OSRs when); interpreted
   answer 224775. Passes at threshold=100/10000/100000. Not fixed by BUG
   D's three root causes below (re-tested after landing them — still
   fails); may share BUG D's 4th, still-open issue instead (both are "a
   value read again well after an intervening branch/call" shapes) — not
   yet confirmed either way.

3. cold_branch_recompile_spill_corruption.mst — BUG D (Richards blocker,
   JIT correctness / memory corruption class). THREE root causes found
   and FIXED (below); a FOURTH, DISTINCT bug still blocks this repro's
   full run (and Richards itself) — see "STILL OPEN" at the end.

   Symptom progression as each layer got fixed (same repro throughout —
   `MACVM_JIT=threshold=1 ./target/debug/macvm run
   tests/repros/cold_branch_recompile_spill_corruption.mst --world world`):
   debug build panics at `oops/mod.rs:53` ("reserved tag") -> fixed ->
   panics at `oops/mod.rs:57` ("mark tag") from a DIFFERENT root cause,
   twice more, each with a different bad value/address -> the current,
   still-open one. Release builds SIGSEGV at the same underlying points
   (the debug tag-check panics are strictly earlier warnings of the same
   corruption release builds hit as a raw bad dereference).

   **Root cause 1 (FIXED): `regalloc::compute_intervals`'s back-edge
   loop-widening over-widened sibling blocks laid out inside a loop's
   numeric position range.** `go`'s own two sequential `1 to: 50000 do:`
   loops recompile while the SECOND loop's own call site is still cold
   (never executed) — `reverse_postorder` lays the second loop's own init
   + untaken-trap blocks positionally BETWEEN the first loop's header and
   body (a valid block order — sibling branches off a loop header have no
   guaranteed position relative to the loop's own back edge). The
   loop-widening pass widened ANY vreg whose interval merely overlapped
   the loop's `[start,end]` range to span the WHOLE range, with no check
   that the vreg's own definition was reachable from within the loop at
   all — smearing the second loop's own short-lived setup temps across the
   first loop's entire body, so real call sites inside it wrongly saw them
   as live oops, reading uninitialized stack memory. Fixed by requiring at
   least one interval endpoint to sit STRICTLY OUTSIDE the loop range
   before widening (`regalloc.rs`'s `touched` filter) — a vreg entirely
   contained within the range needs no widening; one that's genuinely
   loop-carried always has an endpoint outside it (a pre-loop init or a
   post-loop use).

   **Root cause 2 (FIXED): the debug-only M4 cross-check
   (`deopt.rs::interpreter_model_height`) modeled operand-stack height
   with a blind linear byte-address scan, not real control flow.** Once
   root cause 1 stopped masking it, `process:`'s own `ifFalse: [ data
   add2: 7 ]` arm — untaken (and hence a trap) until a late recompile —
   exposed this: the model walked straight through the `ifTrue:` arm's
   bytes (physically earlier in the linear stream) even when resuming
   inside the OTHER arm, wrongly folding in a result the `ifTrue:` arm's
   own send left on its model stack (not popped until the shared merge
   point past BOTH arms). Fixed by making the model follow each forward
   branch's resolved direction (an unconditional `JumpFwd` is always
   taken; a conditional `BrTrueFwd`/`BrFalseFwd`'s own target position
   relative to `resume_bci` tells you which arm was actually reached)
   instead of scanning address order. This was a false-positive in a
   diagnostic-only assertion, not a real runtime bug — but a debug build
   couldn't get past it to reach the real (still-live) bugs beneath.

   **Root cause 3 (FIXED, and the trickiest of the three):
   `scopes::resolve_frame_loc` — which `driver::build_deopt_metadata` uses
   to build the ACTUAL recorded reexecute-stack/slot values a trap's
   materializer reads — used the exact same "does `[start,end]` span
   `position`" interval check `oopmap::build_for_position` does.** The
   FIRST attempt at fixing root cause 1's underlying pattern (a vreg's
   interval blanket-widened to reach a far-away trap, which is unsound
   whenever that widening also wrongly covers a sibling branch) replaced
   `regalloc.rs`'s widening with `RegallocResult::extra_oop_live` — exact
   `(vreg, position)` facts checked in ADDITION to the plain interval, so
   `oopmap::build_for_position` stopped seeing a vreg as live at unrelated
   safepoints. But `resolve_frame_loc` has the identical shape and was
   missed the first time: a vreg needed only by one far-away trap, once no
   longer blanket-widened, now resolved to `Nil` AT THE TRAP ITSELF (the
   one place it's most needed) instead of its real frame slot — the
   deopt materializer pushed `Nil` in place of the real value (`nil + 5`
   instead of a real addition), and the resulting DNU-handling cascade was
   deep enough to overflow the native stack. Fixed by threading
   `extra_oop_live` through `resolve_frame_loc` too (and all 8 of its call
   sites in `build_deopt_metadata`/the OSR live-in resolver), checked as
   an ADDITIONAL exact-position fact alongside the interval, exactly
   mirroring `build_for_position`'s own fix. **Lesson recorded directly in
   both functions' doc comments: any future consumer of a plain
   `LiveInterval` "does this vreg span this position" check must ALSO
   consult `extra_oop_live`, or it inherits this exact class of bug.**
   A dedicated regression test now locks in the general non-widening
   invariant (`compiler::oopmap::tests::
   oopmap_extra_oop_live_is_exact_not_a_widened_range`) alongside an
   end-to-end one against real regalloc output
   (`deopt_resolve_frame_loc_from_real_regalloc` in `it_tier1.rs`, updated
   to match the method's ACTUAL compiled shape — S14 self-send
   devirtualization inlines both trivial `^self` sends away entirely,
   leaving two `GuardKlass`-guarded traps and no real `CallSend` at all).

   Note for future spill-slot investigations: `regalloc::allocate`'s
   spill-slot numbering is monotonic and NEVER reused across intervals
   (confirmed directly reading the code, not assumed) — a bad value in a
   slot is therefore never "some other vreg's stale write bleeding
   through," only ever "this vreg's own slot, marked live at a position
   its own value was never actually written for."

   **STILL OPEN (root cause 4, NOT YET FIXED):** with all three of the
   above landed, the full repro (and Richards itself, same command against
   `world/bench/richards.mst`) now gets much further before failing
   differently: a debug build panics at `oops/mod.rs:57` ("mark tag: word
   0x7 has MARK_TAG") from `memory::roots::for_each_root`'s own PROCESS
   STACK scan (not `each_code_root`'s compiled-frame scan this time),
   nested inside a scavenge triggered by a `prim_basic_new_colon`
   allocation reached via `rt_interpret_call` (a compiled frame calling
   into the interpreter). This is a genuinely different bug class from
   roots 1-3 above (a real pointer/memory corruption, not a GC
   liveness-tracking gap).

   Confirmed facts, strongest evidence first (the existing, already-wired
   `MACVM_DBG_ROOTS=1` audit — `scavenge::audit_roots`, a pre-scan sanity
   check that was ALREADY in the codebase, not added this pass — flags the
   exact same slot independently of the tag-check panic, via a completely
   different test, mark-word-sentinel-based rather than klass-field-based;
   two independent checks agreeing rules out either one being a false
   positive of its own logic):
     - The bad value is a single, specific, deterministic process-stack
       slot (same index, same shape, every run) — NOT a scan-order or
       timing artifact. `scavenge::audit_roots` (gate it with
       `MACVM_DBG_ROOTS=1`) already catches it as a "STALE ROOT" before
       the scavenge that trips the tag-check panic even begins, so the bad
       value predates this specific scavenge — it was written at some
       earlier point in execution and simply sat there, unread, until a
       GC happened to examine it.
     - Reading the object's header words directly (bypassing
       `Oop::from_raw`'s tag check, which would otherwise panic first):
       the word at the stack value's own claimed address decodes as
       PLAIN ASCII TEXT — a 6-digit number string (e.g. "218406", a
       plausible `SmallInteger>>printString` result given this repro's own
       arithmetic) — not a mark word at all. The word 8 bytes higher is
       `0x7` (`MARK_PRISTINE` — a fresh, GC-untouched object's own real
       mark word), and the word 16 bytes higher is a plausible mem-tagged
       klass pointer. This is internally consistent with exactly one
       reading: the REAL object's true start is 8 bytes (one word) higher
       than where the stack slot actually points — the stack holds a
       pointer that is off by exactly one word, into what reads as the
       tail of a DIFFERENT (preceding, already-fully-written) allocation's
       own body content instead of the intended object's own header.
     - Ruled out (read closely, look correct, not the source): the core
       bump allocator (`alloc::try_bump_eden`, `alloc::init_object_at` —
       address captured before the bump, mark/klass written atomically at
       that exact address, no off-by-one in sight); the two indexable
       allocators built on it (`alloc_indexable_oops`/`alloc_indexable_bytes`
       — both return the same already-correct tagged oop `init_object_at`
       produced, no independent address arithmetic of their own); the
       bounds-checked byte-write primitive behind `byteAt:put:`/
       `basicByteAt:put:` (`primitives::prim_byte_at_put` — validates
       `1 <= i <= b.len()` before writing, so it cannot itself be the
       source of an out-of-bounds write corrupting an adjacent object);
       `runtime::deopt`'s `materialize_closure`/`materialize_context`
       (klass set atomically as part of `alloc_closure`/`alloc_context`,
       values read into handles before any allocation).
     - NOT yet ruled out / not yet checked: the JIT's own inline
       allocation fast path (`compiler::emit`'s `emit_alloc`, S11 D7/step
       8 — hand-written arm64, a plausible place for a hand-rolled
       off-by-one, though it is only used for `Ir::Alloc`'s fixed-size
       case, not the variable-sized `String new:` this specific crash's
       own backtrace involves — worth checking anyway since OTHER
       allocations earlier in the same run could be the ones actually
       corrupted); whatever specific world-level method builds a
       `printString` result digit-by-digit (a loop-bound off-by-one
       there, writing one byte past the allocated string's own end, would
       produce exactly this symptom — corrupting the NEXT object's own
       header — but this would be a `world/*.mst` library bug, not a VM
       one, and is outside this investigation's file-ownership scope to
       fix directly even if confirmed); the C2I/argument-marshaling
       boundary (`stubs::rt_interpret_call`) for a receiver/arg computed
       from the wrong frame offset.
   Reproduces via the same minimal repro AND the full Richards run;
   confirmed NOT present on the simpler isolate-process/single-loop
   variants added during this investigation (see below), so it needs the
   fuller repro's own scale/shape (or specifically, a `printString` call
   on a number in roughly the 200000+ range, six digits) to trigger.
   Next step: since the corruption predates the scavenge that discovers
   it, the productive angle is finding WHERE a six-digit `printString`
   result gets built and used, not the allocator/GC layer (already
   audited clean) — a temporary write-time validator on `ProcessStack::
   push`/`set`, or on `alloc_indexable_bytes`'s own return value
   immediately after construction, gated to fire only near the point in
   execution this reproduces, would confirm whether the bad pointer is
   ever CORRECT immediately after allocation and gets clobbered later
   (pointing at a real, later, unrelated JIT/marshaling bug) or is wrong
   from the moment it's first computed (pointing at the allocator or its
   caller instead, despite the above audit).

   Minimal repros added during this investigation (not shipped as fixed
   assets, but the exact commands to rebuild them, since each isolates a
   different layer):
     - Single `1 to: 4 do:` loop, kind=1 only, no second loop at all: was
       enough by itself to exercise root cause 1 once traced back that far
       (removing the second loop does NOT require the "recompile while a
       sibling site is cold" shape to still trip the loop-widening bug —
       any second loop-body-local vreg positioned inside another loop's
       range by `reverse_postorder`'s own layout choice can trigger it).
     - `process:` called with kind=1 three times then kind=2 once, no
       outer loop, direct statements (not `to:do:`): isolates root cause 3
       from root cause 1 — this shape has no loop at all, so only the
       resolve_frame_loc gap was in play.
     - `process:` called with kind=2 only (kind=1 never exercised): same
       root cause 3, confirming it's not specifically about a kind=1→2
       transition, just about a devirtualized/customized trap whose
       recorded value isn't organically read again.
   All three of the above now complete cleanly (interpreter-matching
   results) with the three fixes landed; only the full-scale repro/
   Richards still hits root cause 4.

All repros above also reproduce (or, for BUG A, used to) via the full
benchmarks:
  MACVM_JIT=threshold=1 ./target/release/macvm run world/bench/richards.mst  --world world
  MACVM_JIT=threshold=1000 ./target/release/macvm run world/bench/deltablue.mst --world world
