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
   result). **FIXED — root cause was repro 4's c2i guard hole (see
   below, same commit).** Historically: MACVM_JIT=threshold=1000 ->
   "Projection test 3/4 failed" (the exact test number shifted across
   investigation passes — timing-sensitive, matching which loop OSRs
   when); interpreted answer 224775. Passed at threshold=100/10000/
   100000. Verified fixed: off and threshold=1000 now print IDENTICAL
   output (224775), and the full deltablue bench passes its checksum at
   threshold=1000/10000.

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

   **The mid-threshold silent wrong-answer band — FIXED (repro 4's c2i
   guard hole, same commit).** Richards was wrong at
   `threshold=100..20000` (and DeltaBlue at 1000 — confirmed to BE BUG
   C's own mechanism) but correct interpreted, at t≤10, and at
   t≥100000. The trigger was always "invocation-triggered compilation
   lands mid-run": the freshly compiled caller's first sends resolve
   while callees are still interpreted, i.e. exactly when Mono→c2i
   links with no receiver guard get minted. Verified fixed: Richards
   passes its checksum at off/100/1000/20000/100000; DeltaBlue at
   off/1000/10000.

   **STILL OPEN (all pre-existing, stash-bisected against the pre-fix
   tree — none are regressions from the c2i fix):**
     - This repro under `MACVM_GC_STRESS=1` + `threshold=1` —
       **FIXED (task #94): `extra_oop_live` covered a trap's slots at
       the TRAP position only, leaving them stale across every EARLIER
       safepoint's GC.** Full mechanism, established layer by layer
       (halt bt → `MACVM_DBG_REEXEC` → the emitted LISTING →
       `run_method` outcome probe): under scavenge-per-allocation
       every fresh object is momentarily at EDEN BASE, so ONE address
       recycles endlessly. `WriteStream class>>on:` compiled with its
       `setOn:` send Untaken→Trap-lowered (S14 step 3 — first-compile
       feedback had never run bci 9); every execution runs `self
       basicNew` (a c2i CallSend whose interpreted callee's allocation
       scavenges), then traps and reexecutes `s setOn: aCollection`
       from recorded FrameSlots. `aCollection`'s organic liveness ends
       at its entry spill, so the oop map at the CallSend safepoint —
       where the GC actually struck — did not cover its slot: the
       String moved, the slot kept the old eden-base address, and
       `basicNew`'s result (the fresh WriteStream) was allocated at
       that exact recycled address. The materializer then read
       `[s, aCollection]` as the SAME object: the interpreter
       reexecuted `s setOn: s`, `collection := self`, and thousands of
       sends later `collection size` DNU'd on "a WriteStream" — a
       wrong-but-VALID oop no auditor could flag. (Both REEXEC entries
       reading `0x300000001` — eden base — was the tell.)

       Fix (same commit): (1) `regalloc::compute_intervals` records
       each deopt-referenced vreg's `extra_oop_live` fact at EVERY
       safepoint up to its trap, not just at the trap — a mid-callee
       GC now keeps the slots current; (2) `emit`'s prologue nil-fills
       exactly those slots (`RegallocResult::deopt_nil_init_slots`,
       the normal-entry counterpart of the OSR entry's f62a1e4
       nil-fill), which is what makes (1) sound on paths that never
       wrote the slot — the root-cause-1/3 lesson that path-blind
       liveness is only safe over always-initialized memory. Listing
       goldens regenerated (debug build). Verified: this repro's
       small/mid variants answer correctly under GC_STRESS;
       **richards under GC_STRESS=1+threshold=1 (release) completes
       checksum-clean for the first time**; full suite/clippy green;
       arith ratio ~267x (the prologue fills cost nothing measurable).
       Practical gate (the full 2×50000 repro under GC_STRESS runs
       >10min in debug; 500 iterations reproduce the historical DNU
       in ~2min):
         sed 's/1 to: 50000 do:/1 to: 500 do:/g' \
           tests/repros/cold_branch_recompile_spill_corruption.mst > /tmp/small94.mst
         MACVM_GC_STRESS=1 MACVM_JIT=threshold=1 \
           ./target/debug/macvm run /tmp/small94.mst --world world  # must print 3781
     - deltablue at `threshold=100` — **FIXED (the deopt-trampoline
       anchor fix, task #92, same commit as this note).** Both deopt
       trampolines (`build_uncommon_trampoline`,
       `build_deopt_return_trampoline`) built their walker-visible
       frame record but never PUBLISHED it as the anchor
       (`last_compiled_fp/pc/kind`) — the S12-step-7 walker's start
       rule reads the anchor when the innermost tier crossing is
       `IntoCompiled`, so a GC inside `deoptimize_frame`'s materializer
       allocations (e.g. `alloc_closure` for a block-carrying trap
       scope) hit the frames.rs:347 assert — and in nested tier
       configurations would instead SILENTLY SKIP the deoptee frame's
       oops (a real corruption mechanism, see the next item). Fixed by
       publishing the record as the anchor (new `KIND_DEOPT_BRIDGE` →
       `AdapterKind::DeoptBridge`, 0 RootSpill slots — the deoptee's
       oops come from its own oop map at the recorded pc) and clearing
       it after the Rust call, exactly per `emit_stub_prologue`/
       `emit_stub_epilogue`. Verified: full richards+deltablue matrix
       8/8 checksum-clean incl. this exact point. (Pre-c2i-fix the
       same threshold died of heap exhaustion allocating a
       garbage-sized 67,108,888-byte object — BUG C's wrong value fed
       to `new:` — which the c2i fix cured first.)
     - richards under `MACVM_DEOPT_STRESS=1` + `threshold=100`, STILL
       OPEN but the anchor fix CHANGED it: the historical dossier
       "heap verify FAILED (klass not klass-shaped)" signature is GONE
       (consistent with the silent-skip corruption mechanism above);
       remaining failures are wrong-value family with heap verify OK —
       nondeterministic mix of a host panic (`store_instvar_pop:
       receiver is not a mem oop`, interpreter/mod.rs) and a guest
       error (`large division unimplemented` at `SmallInteger>>// @27`)
       under the same nm=218/nm=279 bci=21 reexecute churn. Also
       Trap-reason throughout (`poll 0` even here — Richards' loops
       apparently never take a live loop-poll deopt under this
       harness, so `fe27eff`'s rt_poll anchor fix — real, but for a
       DIFFERENT door — doesn't touch this signature either).

       **CORE MECHANISM FIXED (97a99ae): `AdapterTable::by_method`'s
       stale hashmap key.** c2i adapters are cached keyed by a
       MethodOop's raw pointer bits; `oops_do` keeps a LIVE entry's own
       embedded pool word current across every GC, but the HASHMAP KEY
       itself is never rehashed. Once a method moves off its old
       address, a wholly unrelated, LATER-allocated method landing at
       that exact vacated address (the same eden-base-recycling
       mechanism task #94 fixed elsewhere, here for METHOD oops
       instead of process-stack spill slots) collides with the stale
       key: a false HIT, not the miss the module's own doc assumed was
       the only possible outcome. A site's `#not` send (say) gets
       permanently linked to whatever OTHER selector's adapter used to
       live at that address — with a real, valid receiver every call,
       so no plausibility check anywhere catches it. Traced live (a
       temporary probe) to `nm=247` (`isTaskHoldingOrWaiting`)'s `#not`
       site silently running `#==`/`#bitAnd:`/etc. adapters instead —
       exactly this repro's whole DNU/wrong-arithmetic zoo. Fixed by
       re-deriving the cached entry's identity from its OWN embedded
       pool word (kept current, unlike the key) and rejecting a
       mismatched hit. Also hardened the adjacent c2i-repatch path
       (`rt_interpret_call`): its captured `ret_addr` can, under
       DEOPT_STRESS, land on a coincidentally-reused code-cache region
       after the nested run flushes the caller — guarded with a
       selector-equality check before repatching.

       Verified: the exact scenario (richards under
       `MACVM_DEOPT_STRESS=1 threshold=100`, RELEASE binary) — 20/20 +
       15/15 clean runs across two sweeps (previously intermittent);
       deltablue same stress, 10/10 clean.

       **STILL OPEN, narrower**: under DEBUG builds specifically (the
       `poison_range` diagnostic — `#[cfg(debug_assertions)]`, never
       compiled into release — that fills a scavenged region with a
       sentinel so any stale reference "fails fast" instead of
       "masquerading as an unrelated GC panic multiple cycles
       downstream", per its own doc), the same richards+DEOPT_STRESS
       scenario still trips a mark-tag-POISON debug_assert reading
       `rt_interpret_call`'s `method_bits` — 14/15 debug runs vs 0/30
       release runs. Per `poison_range`'s OWN documented purpose, this
       is likely the SAME staleness class (a genuinely stale, not just
       key-recycled, method reference), just far lower-probability of
       landing on already-reused memory within one short release run —
       not proven absent there, only unobserved so far. Root cause not
       yet localized: neither a scavenge's own pre-scan root audit nor
       a post-full-gc adapter-pool-word audit (both temporarily added,
       reverted) caught a bad entry before the crash, and there is no
       Smalltalk-heap allocation between a c2i adapter's own `ldr`
       (reading its pool word) and `rt_interpret_call`'s use of it, so
       the staleness must predate that specific call entirely. Next
       step: trace WHERE a live adapter's pool word first goes bad —
       likely needs an audit hook inside `AdapterTable::oops_do`'s OWN
       callback (comparing `*word` immediately before vs. after the
       relocation transform `f`, not just before-vs-after a whole GC
       cycle) to catch the exact GC and exact object it happens to.

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

4. c2i_mono_klass_mismatch.mst — BUG C / the mid-threshold band's root
   cause (task #88). **FIXED (same commit as this repro).** A c2i
   adapter (`adapters.rs::build_c2i_adapter`) is `ldr method; ldr
   c2i_shared; br` — NO receiver-klass guard, unlike a real nmethod
   entry (`emit_entry_guard`). So a compiled Mono IC site whose target
   resolved to a c2i adapter (callee still interpreted — precisely the
   mid-run-compile window) dispatched ANY receiver klass into the one
   baked method: `Mono{True -> c2i_(True>>not)}` ran `True>>not`
   (`^false`) on a `false` receiver — a silent wrong boolean, which
   Richards' `isTaskHoldingOrWaiting` then consumed. PIC-linked c2i
   targets were never affected (the PIC stub guards klass upstream);
   Mono links were the hole. Fixed in `stubs.rs::rt_interpret_call`:
   the adapter's baked method is only a hint — an ordinary send
   re-derives dispatch truth via `lookup(klass_of(receiver), selector)`
   before running anything, with the baked ancestor method reserved for
   super sites (`site.super_klass`). The site itself stays Mono and
   self-heals into a PIC once the callee compiles (real entry guard
   misses -> rt_resolve_send).

   Localized with the S-debugger tooling in one pass: `MACVM_DBG_IR`
   proved both `not` compiles and the caller's IR correct;
   `disasm-native` (pool resolution) proved the emitted guards correct;
   `MACVM_DBG_RESOLVE` then showed the single Unresolved->Mono{True}
   transition with c2i=true and NO further misses while `false`
   receivers kept arriving — dispatch, not compilation.

   Gate: prints "0" (wrong-answer count over 300 alternating poly
   sends) at EVERY threshold; historically printed 1+ at any threshold
   landing the caller's compile mid-run:
     for t in off 1 10 100 1000; do MACVM_JIT=threshold=$t \
       ./target/debug/macvm run tests/repros/c2i_mono_klass_mismatch.mst --world world; done

5. closure_nlr_ensure_two_frames.mst — S24 A1 shakeout bug 7 (silent
   wrong answer: SKIPPED ensure: handler). A compiled `[^7]`'s NLR
   through two ensure-protected interpreted frames ran only the OUTER
   handler (prints "7 1 #outer" then a guest index error instead of
   "7 2 #inner #outer"). Root cause: `prim_ensure_like` activated the
   PROTECTED block via the trigger-bearing `activate_block`, which at
   low thresholds compiles the block and completes it INSIDE the
   primitive — no interpreter frame, so the `Activated` arm that arms
   the ensure marker never ran. The marker protocol (normal-return
   interception in `do_return` AND the NLR scan) requires the marker to
   live on the protected block's interpreter frame; the fix routes the
   activation through `activate_block_interp`, exactly like the handler
   activations in `unwind.rs`. Found by the world-suite byte-differential
   at threshold=1 (DispatchTests>>testEnsureOrderThroughTwoFrames), which
   also gates it permanently.

   Gate: prints "7 2 #inner #outer" (4 lines, exit 0) under BOTH
   MACVM_JIT=off and MACVM_JIT=threshold=1.

6. closure_dead_home_cannot_return.mst — S24 A1 dead-home delivery
   (adversarial-review BLOCKER 2's scenario, expect-death). A COMPILED
   NLR block whose home frame already returned must deliver
   `#cannotReturn:` to the ORIGINATING closure — parked in
   `NlrState.closure` by `rt_nlr_originate` — not to the current frame's
   receiver slot (the block's native frame is gone by liveness-check
   time; the current frame is the value-sender's). The world defines no
   `cannotReturn:`, so correct behavior is the fatal DNU cascade NAMING
   the closure's class.

   Gate: exits nonzero with "DNU #cannotReturn: (receiver class
   BlockClosure)" on stdout under BOTH MACVM_JIT=off and
   MACVM_JIT=threshold=1 (automated as it_world.rs's
   `dead_home_nlr_names_the_closure_both_tiers`). Stack-trace depth
   legitimately differs by tier (compiled frames don't print as
   interpreted lines).

7. closure_deopt_in_block_ctx_writes.mst — S24 A1 deopt-in-compiled-block
   (design 4's repro 2, distilled from the sieve benchmark during the A1
   shakeout). Captured-ctx blocks (`a`/`c`/`k` are home-method temps
   mutated inside `do:`/`whileTrue:` blocks) compiled at threshold=1 and
   deopted mid-iteration must land their ctx writes in the HOME frame the
   interpreter resumes from — the root-is_block materializer arm rebuilds
   the block's interpreter frame with copied(0)/copied(1) ALIASED to the
   live closure's own fields (identity, never fresh copies), so writes
   made before the trap stay visible after it. During the shakeout this
   scenario surfaced the regalloc closure-vreg pin, the Frame::verify
   block shapes, and the `note_uncommon_trap` is_block branch.

   Gate: prints "1 1 1" (3 lines, exit 0) under MACVM_JIT=off AND under
   MACVM_JIT=threshold=1 with MACVM_DEOPT_STRESS=64 (round-robin
   invalidation forces mid-block deopts continuously).

All repros above also reproduce (or, for BUG A/C, used to) via the full
benchmarks:
  MACVM_JIT=threshold=1 ./target/release/macvm run world/bench/richards.mst  --world world
  MACVM_JIT=threshold=1000 ./target/release/macvm run world/bench/deltablue.mst --world world
