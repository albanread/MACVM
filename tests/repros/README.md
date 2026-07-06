Tracked JIT-bug repros (S15 step 6 — flushed out by the Richards/DeltaBlue
ports; the benchmarks' own 4-mode gates stay red until these are fixed):

1. ic317_reosr_klass_skew.mst — BUG A (Richards blocker, VM ABORT).
   **FIXED** (commit 6145c63): `ic_transition`'s mono→poly arm read
   `selector` AFTER `alloc_poly_pairs`'s own allocation without protecting
   it first — a scavenge mid-transition left it a stale pre-move address.
   Handle-protected across the allocation, then re-derived. This repro then
   passed (prints "9999 10001", exit 0) at every threshold tried — until it
   recurred (see below).

   **RECURRED, then FIXED AGAIN (commit 8429a7d), same bug class, different
   variable.** Found during final verification of BUG D's fixes: this repro
   started panicking with a POISON-pattern read (`mark tag: word
   0xf0f0f0f0f0f0f0f3 has MARK_TAG` — `scavenge::POISON`'s own fill value
   for vacated from-space memory), backtrace through
   `MethodOop::ics` -> `InterpreterIc::at` -> `send::activate_method` ->
   `send::send_generic` -> `dispatch_from` -> `interpret_active` — a
   DIFFERENT code path from the original fix (`activate_method`'s JIT
   compile-trigger, not `ic_transition`'s mono->poly arm). Bisected via
   `git checkout <pre-BUG-D-commit> -- <BUG-D's 5 files>` to confirm this
   was NOT a regression from BUG D's work — it failed identically on the
   pre-BUG-D-fix code too, meaning this recurrence had been latent all
   along and BUG D's own stress testing was simply the first thing to
   shake it loose.

   Root cause: `send_generic` (`send.rs`) captures `caller` from
   `vm.regs.method` BEFORE calling `ic_transition` — whose klass-skew rows
   5/6 (poly transition) allocate the pairs array and can scavenge — then
   threads that PRE-CALL `caller` into `activate_method`'s `ic_site`
   parameter, which dereferences it (via `InterpreterIc::at`, to patch the
   IC once a JIT compile succeeds) well AFTER that allocation could have
   moved it. `ic_transition` already re-derives its OWN internal `caller`
   after the allocation (`ic.rs`'s `let caller = vm.regs.method.expect(...)`)
   — but it only returns the resolved target method, `Option<MethodOop>`,
   with no way to hand that fresher `caller` back to its own caller.
   `send_generic`'s DNU (`None`) arm already re-derives `caller` the same
   way for the identical reason (see its own comment, one screen down) —
   the `Some(m)` arm just never got the same treatment.

   Fixed by changing `activate_method`'s `ic_site` parameter from
   `Option<(MethodOop, u16)>` to `Option<u16>` — the caller `MethodOop` is
   no longer threaded through at all; instead it is always re-resolved
   fresh from `vm.regs.method` at the point of use, mirroring the pattern
   already established in `ic_transition` and `send_generic`'s DNU path.
   Verified: passes x3 under `threshold=1` plus `off`/`threshold=1000` (all
   agree: "9999 10001"); full lib suite (557 passed) and full integration
   suite clean; the still-open BUG D root cause 4 below reproduces
   identically, confirming this fix doesn't touch that code path.

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

   **Root cause 4 — FIXED (commit f62a1e4): it was TWO distinct
   uninitialized-native-memory reads**, both localized in one session by
   the write-time stack auditor (`MACVM_DBG_STACK_WRITES=1`, debug
   builds — `interpreter/stack.rs`, committed in d65d1dd): every
   process-stack write is validated AT WRITE TIME (mark-plausibility for
   mem oops, plus a heuristic arm flagging "smis" in the code/stack
   address band — dead native-frame `(pc, fp)` words have low bits 00 and
   parse as smis, which is precisely how both bugs' garbage crossed the
   whole VM unseen). The auditor turned each "where did this come from"
   into a panic backtrace at the corrupting write, first shot both times.

   **4a: the OSR entry block left omitted frame slots as native-stack
   garbage.** The synthetic OSR entry branches PAST the normal entry
   block's temp nil-init, so any slot the OsrMap omitted (a temp dead at
   the loop header — `go`'s `b`, only used before the loops) held
   leftover words from dead frames at the same SP depth; in the repro,
   a derived string-body pointer (`tagged_string + 16`, element-1
   address) from a dead `printString` frame — the dossier's original
   "one word too low" decode, exactly. In-loop trap scopes still record
   such temps' slots (`extra_oop_live`), and the same forcing puts them
   in the oop maps GC scans. Fixed three ways (each layer's necessity
   proven by a distinct failure): the OSR entry nil-fills EVERY spill
   slot before its buffer copies; the OsrMap also packs dead-at-header
   temps' TRUE interpreter values into their spill homes (nil was
   GC-sound but a mid-loop deopt then resumed the interpreter with nil
   for a temp it genuinely reads — caught by
   `osr_frame_deopts_mid_loop_and_finishes_interpreted`); each so-packed
   vreg's interval is widened method-wide so GC keeps the slot scanned
   (sound — the slot is written on every entry path first, ctx_vreg's
   own `deopt_live_widen` argument; the missing widening was caught by
   the scavenge-exit verifier on this very repro).

   **4b: every Rust-reaching stub spilled only x0..x5, but compiled sends
   marshal receiver+args into x0..x7.** Richards' 7-arg task initializer
   (`link:identity:priority:work:state:scheduler:data:`) through a c2i
   adapter had args 6-7 read from PAST the 6-slot RootSpill area — the
   stub frame's own saved fp/lr (byte-for-byte: `argv[6]=[x29]`=saved fp
   → `scheduler` ivar held a stack address, `argv[7]=[x29+8]`=saved lr →
   `handle` ivar held a code address), surfacing thousands of sends later
   as `doesNotUnderstand: deviceInAdd:` on a "SmallInteger" (the ivar
   dump of the DNU receiver's task object was the clincher). x6/x7 are
   also C-ABI caller-saved, so every ≥6-arg send through resolve/mega/dnu
   had its tail args clobbered by the Rust call regardless of c2i. Fixed:
   `ROOTSPILL_SLOTS` 6 → 8 (x6/x7 spilled+reloaded in the shared stub
   prologue/epilogue) + an eligibility cap declining methods containing
   send sites with more than 7 real args (the register convention's
   actual limit — such methods stay interpreted).

   RESULT: this repro answers its interpreter value 577783 at
   threshold=1; **Richards completes correctly under threshold=1 for the
   first time ever** (23246/9297). The evidence below is retained as the
   investigation record.

   **NEWLY DOCUMENTED, STILL OPEN (pre-existing, stash-bisected against
   the pre-fix tree — NOT from these fixes):**
     - The mid-threshold silent wrong-answer band: Richards is wrong at
       `threshold=100..20000` (and DeltaBlue at 1000 — very likely BUG
       C's own mechanism) but correct interpreted, at t≤10, and at
       t≥100000 (OSR-only compilation). The scheduler loop exits early
       EXACTLY when invocation-triggered compilation lands mid-run (at
       t=20000 it dies ~92% through, where `queuePacket:` crosses 20000
       invocations) — a method compiled mid-run from warm poly feedback
       immediately returns a wrong value. Runs after the first may
       collapse further (counts like "2/1") or recover (t=20000's runs
       2+ are correct once everything is compiled).
     - The repro fails under `MACVM_GC_STRESS=1` + `threshold=1`
       (`doesNotUnderstand: size`) while passing under
       `MACVM_DEOPT_STRESS` — untriaged.

   Original root-cause-4 crash shape (retained): a debug build panicked
   at `oops/mod.rs:57` ("mark tag: word 0x7 has MARK_TAG") from
   `memory::roots::for_each_root`'s PROCESS STACK scan, nested inside a
   scavenge triggered by a `prim_basic_new_colon` allocation reached via
   `rt_interpret_call`.

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
       values read into handles before any allocation); the JIT's own
       inline allocation fast path (`compiler::emit`'s `emit_alloc`, S11
       D7/step 8 — traced every offset and register through the fast path:
       `x19` briefly holds the object's own start address, gets clobbered
       loading eden_end for the bounds compare, then is exactly recomputed
       as `new_top - size_bytes` before the header/body writes, which is
       arithmetically exact, not an approximation; the header writes land
       at offsets 0/`WORD_SIZE` as expected, and the nil-init loop's own
       bound (`body_words = size_words - HEADER_WORDS`, offsets
       `HEADER_WORDS..size_words`) covers the body exactly with no
       off-by-one in either direction. Still only used for `Ir::Alloc`'s
       fixed-size case, not the variable-sized `String new:` this crash's
       own backtrace involves, so ruling it out doesn't rule out an EARLIER
       allocation via this path being the one actually corrupted by
       something else — it just means this code itself isn't the bug).
     - NOT yet ruled out / not yet checked: whatever specific world-level
       method builds a `printString` result digit-by-digit (a loop-bound off-by-one
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
