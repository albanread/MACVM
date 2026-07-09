# Closure Compilation Design (S24 candidate)

STATUS: COMPLETE design draft, 2026-07-09. Design-only — no VM source was modified.
All file:line references verified against the working tree on this date; the §0/§1.6
measurements were taken live from the release build during writing.

Scope in one line: give escaping/argument blocks a compiled representation (Phase A:
compiled block bodies, registry-dispatched `value:` sites, NLR-free escaping creators)
and widen S14's existing iterator inlining until DeltaBlue's block-iteration tail
compiles (Phase B) — gated on DeltaBlue >= 5x, interpreted tail < 10%, richards >= 16x.

## 0. Problem statement and goal

DeltaBlue plateaus at ~3.4x vs the 5x target (`docs/PERF.md` results table, 2026-07-09:
off 208ms → t=2000 62ms) because a hard core of its execution can never compile. Fresh
measurement while writing this doc (release build, 2026-07-09):

```
MACVM_TRACE=count MACVM_JIT=off          → deltablue 265 ms, 102,901,182 interpreted bytecodes
MACVM_TRACE=count MACVM_JIT=threshold=1000 → deltablue  71 ms,  16,732,414 interpreted bytecodes
```

16.3% of the interpreted-bytecode volume survives warmup (the sprint brief's 17.6% at
t=2000 is the same phenomenon). Amdahl: even if compiled code were infinitely fast, a 16%
interpreted tail caps the speedup at ~6x — and the tail isn't free, so passing 5x requires
driving it well under 10%.

A live `MACVM_TRACE=jit` census on the same run names the tail precisely. compile_disabled
(NoPermanent) in DeltaBlue: `execute` (Plan's), `constraintsConsuming:do:`,
`addConstraintsConsuming:to:`, `removePropagateFrom:`, `markInputs:`, `inputsKnown:` — all
block-iteration code — plus `value:` itself (prim 51) and the by-design
`PRIM_ALREADY_FUSED` arithmetic methods. Meanwhile `do:` DOES compile (2 customized
copies) — but every `aBlock value: x` inside compiled `do:` is a plain CallSend whose
callee (the `value:` method, prim 51) is interpreter-only, so **every per-element block
body runs interpreted** via the c2i path, forever.

Two structural bail-outs cause this:

1. **The `value:` family (prims 50-54, 60, 61) returns `PrimResult::Activated`** —
   interpreter-only by design, excluded from prim shims (`docs/prim_shims.md` §3,
   `PRIM_ACTIVATES_FRAME`, `src/compiler/driver.rs:52`). Compiled code can *reach* a block
   activation but the block body always runs interpreted.
2. **`is_block` methods are never independently compiled**
   (`src/compiler/driver.rs:190`). A CompiledBlock is only ever executed by the
   interpreter, or spliced away entirely by S14 step 7 when it provably never escapes.

Richards hits 16x because its blocks are non-escaping — S14 step 7 inlines them and elides
their Contexts. DeltaBlue is the benchmark for exactly what S14 deferred: idiomatic
Smalltalk passes blocks as ARGUMENTS into helper methods (`do:`, `inputsDo:`,
`constraintsConsuming:do:`), where today's escape analysis must declare them escaping.
`docs/next_architecture.md:158-163` already calls escaping closures "the long pole ...
closer to genuine open compiler-construction work than anything else on this list".

**Goal**: (A) give escaping/argument blocks a compiled representation — compiled block
bodies plus compiled `value:` dispatch — so block-heavy code stops being interpreter-bound;
(B) inline the iterator methods themselves so the common mono case pays zero closure cost.
Gate: DeltaBlue >= 5x, interpreted tail < 10%, no richards/fib regression (§4).

## 1. Ground truth survey (what exists today, with file:line evidence)

### 1.1 Object model: CompiledBlock and BlockClosure (SPEC §4.4, §5.4)

- A `CompiledBlock` is a full CompiledMethod object with `is_block=1`. **[REVIEW
  CORRECTION §2b]** Its `holder` is the **KLASS**, not the home method:
  `install_method` runs `patch_block_holders` (`src/runtime/lookup.rs:210-226`), which
  recursively stamps the installing klass into every CompiledBlock reachable through the
  method's literal pool — precisely so super sends inside blocks resolve
  (`ic::resolve` reads `caller.holder()` where caller is the executing CompiledBlock,
  `ic.rs:176-196`). SPEC.md:339-341's "holder is the home method" is STALE against the
  code (separate SPEC correction to file). Consequence: there is NO in-object path from
  a CompiledBlock to its home method — nothing in this design may assume one. It has
  its own `literals`, its own `ics` Array, and — critically for triggers — its own
  `counters` word. Flags word packs
  `argc:4 | ntemps:8 | nctx:8 | has_ctx:1 | captures_ctx:1 | is_block:1 | prim_fails:1`
  (`docs/SPEC.md:331-332`).
- `BlockClosure { method, home_frame_ref, copied[] }` (`docs/SPEC.md:413-424`). Pinned
  `copied[]` convention: `copied[0]` = home **receiver** (always), `copied[1]` = home
  **Context** iff `captures_ctx`; explicit captures are routed through the Context by the
  source compiler, so bytecode-level `ncopied` (extra by-value captures) is normally 0 — but
  the VM machinery supports them (`src/interpreter/blocks.rs:113-118` copies them into the
  first local temp slots on activation).
- `home_frame_ref` = packed (proc id, frame FP index, frame **serial**) — a safe reference
  that detects a dead home frame (`docs/SPEC.md:414-416`, `src/oops/home_ref.rs`). Frame
  serial lives at FP+5 of every interpreter frame (`docs/SPEC.md:375`).
- The `ics` side-table means a block's sends have their own IC state, independent of the
  home method's (`src/interpreter/blocks.rs:579-587` test `block_own_ics_counters`).

### 1.2 Interpreter block activation (src/interpreter/blocks.rs)

`activate_block` (`src/interpreter/blocks.rs:52-123`) is the single interpreter entry:

1. `blk.argc() != argc` → `PrimResult::Fail` (guest-visible wrongArgc error via the
   installed `value:` method's bytecode fallback).
2. **Bumps the block's own invocation counter** (`blocks.rs:57`,
   `super::send::bump_invocation(blk)`) — a compile trigger point already exists.
3. Pushes a frame shaped exactly like a method activation EXCEPT:
   - `FRAME_RECEIVER` (FP+4) = `closure.copied(0)` — the captured home `self`, never the
     value at fp-argc-1 (`blocks.rs:60-70`);
   - the caller-side receiver slot (fp-argc-1) holds the **closure itself** — this is what
     `home_ref_for_new_closure` reads for nested-block home propagation (`blocks.rs:193`).
4. Context wiring (`blocks.rs:85-95`): a `has_ctx` block allocates a fresh Context whose
   `home_hint` links to `closure.copied(1)`; a ctx-less block **aliases** the enclosing
   Context directly into its FP+3 slot (uniform `push_ctx_temp` depth-walking).
5. Copies value captures into the first local temp slots (`blocks.rs:113-118`).
6. Sets `vm.regs.method = blk`, `bci = 0`, returns `PrimResult::Activated` — the prim call
   site must push no result; the interpreter loop simply continues in the block's bytecode.

`make_closure` (`blocks.rs:142-177`): VM supplies `copied[0]`/`copied[1]` itself; operand
stack carries only value captures. `home_ref_for_new_closure` (`blocks.rs:186-204`): a
closure made inside a block frame copies the enclosing closure's home **verbatim** (the
classic NLR bug is computing it fresh from the block frame).

### 1.3 The value: family primitives (50-54, 60, 61)

All seven live in `src/runtime/primitives.rs`:

- `prim_value0..3` (`primitives.rs:962-988`): downcast receiver to ClosureOop (else Fail —
  that is how `Object>>value` DNU-ish failures surface), then delegate to
  `interpreter::blocks::activate_block`. **No allocation** (`can_allocate: false`,
  `primitives.rs:289-343`), `can_fail: true` (argc mismatch, non-closure receiver).
- `prim_value_with_arguments` (id 54, `primitives.rs:994-1011`): validates array size ==
  block argc, then **stack surgery**: `vm.stack.sp = vm.prim_arg_base + 1` drops the Array
  arg and re-pushes its elements in place, then activates with dynamic argc. Reads
  `vm.prim_arg_base`, which only interpreted dispatch sets (`docs/prim_shims.md:94-103`).
- `prim_ensure_like` (60/61, `primitives.rs:1017-1046`): activates the protected block
  (argc 0), then arms the handler closure as the NEW activation's FP+6 marker
  (`frame.set_marker(handler, kind)`); `ensure:` gets no frame of its own. Drops the
  handler from the operand stack first so `activate_block`'s `sp-argc-1` math lands on the
  protected closure.
- All seven return `PrimResult::Activated`, which `docs/prim_shims.md:71-77` proves is
  produced ONLY by `activate_block` and is excluded from the S-prim-shim machinery
  (`PRIM_ACTIVATES_FRAME` = {50,51,52,53,54,60,61}, `src/compiler/driver.rs:52`): the
  generic call-and-branch shim cannot represent "control transferred to a new activation".
  This is bail path (1): compiled code sending `value:` today goes through a c2i-ish path
  into the interpreter and STAYS there for the block body.

### 1.4 NLR machinery today (src/interpreter/unwind.rs + S11 step 9 sentinel)

The interpreter side is complete and battle-tested; the compiled side already knows how to
*propagate* an escape, just not how to *originate* one:

- `nlr_tos` (bytecode 43) → `continue_unwind(vm, home, value)`
  (`src/interpreter/unwind.rs:175-237`): re-validates `home_is_live` on every entry
  (`unwind.rs:273-293`: proc, bounds-BEFORE-serial-read, serial match), then scans the fp
  chain (`innermost_marked_frame`, `unwind.rs:316-331`) for the innermost armed
  ensure:/ifCurtailed: handler. Four outcomes (`UnwindStep`, `unwind.rs:149-169`):
  - `ReachedHome` → `pop_frames_above` + `do_return` at home;
  - `Marked` → arm an UnwindToken `[packed_home, value]` on the marked frame, activate the
    handler with saved_bci = `BCI_RESUME_UNWIND`; on handler return `resume_unwind`
    re-enters `continue_unwind` (`unwind.rs:246-267`);
  - `CrossedBoundary(entry_fp)` → home is beyond a c2i boundary: discard the whole
    interpreter activation, park `vm.nlr_state = NlrState { home, value }`, return
    `UnwindStep::Escaped`; the dispatch loop returns the raw `NLR_SENTINEL`
    (`unwind.rs:191-210`, `unwind.rs:265`);
  - home dead → `cannot_return_current_closure` sends `#cannotReturn:` to the closure
    sitting in the current frame's receiver slot (`unwind.rs:374-406`).
- `NLR_SENTINEL = 0b0110` (`src/oops/layout.rs:27`), a RESERVED_TAG word no oop can equal.
  Every compiled send site already checks the return value: `sub x17, x0, #NLR_SENTINEL;
  cbz x17, <epilogue>` (`emit::emit_nlr_check`, cited at
  `src/interpreter/compiled_call.rs:156-158`) — so a sentinel returned by ANY callee routes
  straight out through the compiled frame's ordinary epilogue, unwinding the native frame
  for free.
- `enter_compiled`'s NLR arm (`src/interpreter/compiled_call.rs:179-187`): when the stub
  returns the sentinel, take the parked `NlrState` and resume `continue_unwind` on the
  home-ward side; `EnterResult::Nlr(step)` is mapped by callers exactly like `OP_NLR_TOS`'s
  own outcome. Escapes can re-park and re-propagate through any number of alternating
  compiled/interpreted segments.
- Marker discipline: ensure:/ifCurtailed: handlers live in the FP+6 marker slot of the
  PROTECTED BLOCK's frame; the scan checks markers BEFORE the c2i boundary so a handler on
  the boundary frame runs first (`unwind.rs:311-315`).

**Implication for this design**: compiled code never needs a new unwinding mechanism — a
compiled block that executes `^expr` needs only to (a) run any handler protocol correctly
and (b) return the NLR_SENTINEL with `vm.nlr_state` parked, and every existing compiled
frame between it and home already relays the escape. The missing pieces are origination
(a compiled block frame executing `nlr_tos`) and the home-side TERMINATION when the home
itself is a compiled frame (today `CrossedBoundary` only fires for c2i entry frames;
"home IS a compiled activation" cannot happen yet because compiled frames are never homes —
only interpreter frames have HomeRefs pointing at them).

### 1.5 Escape analysis + Context elision (src/compiler/escape.rs, S14 step 7)

`escape::analyze(method)` (`src/compiler/escape.rs:404-515`) abstract-interprets M's CFG
to a fixpoint, tracking `AV::Site(push_closure bci) | AV::Other` per stack/temp slot. A
site is elidable iff every use is GOOD; the driver compiles M only if `all_elidable`
(`driver.rs:262-267`). GOOD uses are exactly two:

1. **Receiver of a matching-argc `value`-family send** (`value`..`value:value:value:`
   only; `valueWithArguments:` deliberately excluded, `escape.rs:86-98`) → the block body
   is spliced inline at the send (`ir::convert`).
2. **The single block argument of a mono send to a BLOCK-ARG TRANSPARENT callee**
   (S14 7-IV-c, `blockarg_candidate_ok`, `escape.rs:362-395`): callee must be mono-IC, a
   plain `MethodOop` target (an nmethod-id target returns false, `escape.rs:377-381`),
   `primitive()==0`, CFG-inlinable, within DOUBLED budget, and use the arg ONLY as
   receiver of matching value-sends (`block_arg_transparent`, `escape.rs:266-355`).

Everything else escapes: stored anywhere, passed as a non-receiver arg to any other send,
returned, merged with a different value at a CFG join (`merge_slot`, `escape.rs:193-209`),
live at a non-return block boundary (`escape.rs:473-490`), or receiver of a non-value send
(sprint_s14_detail.md "Step 2 — escape condition (precise)", ~line 366-380).

Spliceability of the block itself (`block_is_spliceable`, `escape.rs:125-179`) requires:
no own `has_ctx`, **single straight-line basic block** ending in a return, no super-send /
nested closure / depth>0 ctx access, and an NLR block must be send-free (`escape.rs:176-178`
— a send-ful NLR block could deopt inside and the interpreter's `nlr_tos` would need a real
closure home_ref that was elided away).

Context elision (7-II-b): all-elidable ⟹ M's ctx-temps promote to vregs, M's Context is
never allocated; deopt records `CtxLoc::Elided` and the materializer rebuilds a real
Context (§1.7). Pinned rule: elision is all-or-nothing per method
(sprint_s14_detail.md "Step 4 — Context elision").

### 1.6 Eligibility gates that reject DeltaBlue's hot methods (src/compiler/driver.rs)

`eligibility_detail` (`driver.rs:183-396`) rejections relevant here:

- `method.is_block()` → NoPermanent (`driver.rs:190`) — **gate (2)**: no CompiledBlock
  ever compiles standalone.
- `method.primitive() != 0 && !is_shimmable_primitive(...)` (`driver.rs:192`), where
  `PRIM_ACTIVATES_FRAME = [50,51,52,53,54,60,61]` (`driver.rs:52`) — **gate (1)**: the
  `value:` methods themselves never get compiled entries.
- escape pre-pass `!all_elidable` → NoPermanent (`driver.rs:262-267`) — **gate (3)**: any
  method creating a closure it cannot prove non-escaping.
- `has_ctx` with no elidable closure → NoPermanent (`driver.rs:277-279`).
- ctx-temp access at depth != 0 → NoPermanent (`driver.rs:353-357`).
- top-level `BlockReturnTos`/`NlrTos` → NoPermanent (`driver.rs:361`).

Which gate each hot DeltaBlue method trips (verified live 2026-07-09, `MACVM_TRACE=jit
MACVM_JIT=threshold=1000`: all six print `compile_disabled` with NO flags-gate reason line,
i.e. all fall to the escape pre-pass; source refs into `world/bench/deltablue.mst`):

| method | source shape | why it escapes today |
|---|---|---|
| `Plan>>execute` (deltablue.mst:173) | `self do: [:c | c execute]` | block IS spliceable and `do:` IS transparent — but 7-IV-c requires the mono target be a plain `MethodOop`; once `do:` itself compiles, the IC target is an nmethod id → `blockarg_candidate_ok` bails (`escape.rs:377-381`). Cold-start order dependence aside, `do:`'s inline_cost (its loop body) can also exceed `per_call_cost*2 = 60` at level 1 |
| `Planner>>constraintsConsuming:do:` (deltablue.mst:265) | creates `[:c | (c ~~ determining) and: [...] ifTrue: [aBlock value: c]]`, passes it to `constraints do:` | the block body has inlined `and:`/`ifTrue:` → multi-basic-block → `block_is_spliceable` fails (`escape.rs:140`); also captures `determining`/`aBlock` (has_ctx home) |
| `Planner>>addConstraintsConsuming:to:` (deltablue.mst:272) | `self constraintsConsuming: v do: [:c | aCollection addLast: c]` | block spliceable, but callee `constraintsConsuming:do:` contains its own `push_closure` → `is_inline_eligible_cfg` false (`inline.rs:184-186`) → blockarg candidate fails → escaping |
| `Planner>>removePropagateFrom:` (deltablue.mst:225) | two blocks into `do:`/`constraintsConsuming:do:` inside a whileTrue: | first block multi-BB (`ifFalse:` inside); second passes to a closure-bearing callee — both escape |
| `AbstractConstraint>>markInputs:` (deltablue.mst:317) | `self inputsDo: [:in | in mark: mark]` | `inputsDo:` dispatches across Unary/Binary constraint hierarchies → IC not Mono → `blockarg_candidate_ok` fails (`escape.rs:373-375`) |
| `AbstractConstraint>>inputsKnown:` (deltablue.mst:321) | `inputsDo:` block containing `^false` | NLR block WITH sends + multi-BB (`or:` chains) → unspliceable (`escape.rs:140,176-178`); plus the poly `inputsDo:` |

What DOES compile in the same run: `do:` (2 customized copies), `inputsDo:` (4),
`satisfy:`, `recalculate`, accessors — i.e. the iteration DRIVERS compile but each
per-element `value:` send inside them c2i's into the interpreter, and the six ORCHESTRATOR
methods above run fully interpreted. That combination is the 16.3% tail.

### 1.7 Deopt metadata: ScopeDesc, CtxLoc, SenderLink, materializer

- `ScopeDescData { method_pool_ix, is_block, sender, receiver, slots, ctx }`
  (`src/compiler/scopes.rs:400-412`). **`is_block: bool` already exists** — S14 step 7-II
  uses it for block scopes spliced INTO a home compilation.
- `ValueLoc` (`scopes.rs:267-289`): ConstPool / ConstSmi / FrameSlot / Nil /
  **ElidedClosure(pool_ix)** — no Register variant in v1 (every deopt-named value is in its
  canonical frame slot at every safepoint).
- `CtxLoc::{None, Materialized(ValueLoc), Elided{temps}}` (`scopes.rs:332-341`). The packed
  format has carried `Materialized` since S13 — i.e. **a compiled frame owning a real heap
  Context is already representable**, it just is not emitted today (elision-only).
- `SenderLink { sender, sender_bci, pending_stack }` (`scopes.rs:385-393`) chains inlined
  scopes; `SafepointState { scope, bci, kind, reexecute, stack }` (`scopes.rs:448-458`).
- The materializer (`runtime/deopt.rs:264+`, M0-M8): decodes the scope chain, pushes
  interpreter frames outermost-first via the interpreter's own `push_frame`
  (`deopt.rs:485-490` — byte-identical frames by construction). Three block-relevant
  behaviors, all built on ONE assumption:
  - `materialize_closure` (`deopt.rs:153-177`) rebuilds an elided closure with
    `home = (root_fp, root serial)`, `copied[0] = root receiver`, `copied[1] = root
    Context` — "the block's home is always the compilation ROOT in v1"
    (`deopt.rs:146-147`, `scopes.rs:283-287`).
  - An `is_block` scope's frame gets its FP+3 Context by ALIASING the root frame's Context
    (`deopt.rs:522-541`); the code even `expect`s `"is_block frame is never outermost"`
    (`deopt.rs:535`).
  - Reexecute stacks can carry `ElidedClosure` and materialize it on the spot
    (`deopt.rs:566-577`).

  **Phase A breaks exactly this root-home assumption**: an independently compiled block's
  home is a SEPARATE activation (usually interpreted), and the block scope IS the
  compilation root. §2.5 designs the extension.

### 1.8 Frame walking + GC roots for compiled/adapter frames

- `FrameView::{Interpreted, Compiled{fp, ret_pc, nm}, Adapter{fp, kind, caller_pc},
  CallStub}` (`src/runtime/frames.rs:150-178`). Classification is **by pc**
  (`code_table.find_by_pc`), not by anything about the method — so **a compiled block
  activation needs NO new frame kind**: its nmethod is found by pc and walked as
  `Compiled`, oopmaps and all. This is the single biggest reuse win in the design.
- `walk_frames` (`frames.rs:282+`) alternates process-stack and native segments via
  `tier_links` (`IntoCompiled`/`IntoInterpreter`/`DeoptBridge`) and the stub anchor
  (`reg_block.last_compiled_{fp,pc,kind}`).
- GC roots for native frames (`src/memory/roots.rs:217-337`, `each_code_root`): a
  `Compiled` frame's roots come from its safepoint's OopMap over spill slots; an `Adapter`
  frame's roots come from its RootSpill area `[fp - ROOTSPILL_BYTES, fp)` with a per-kind
  live-slot count (`real_oop_rootspill_slots`, `roots.rs:339-411` — e.g. C2i/Resolve scan
  `1 + site argc` slots derived from `caller_pc`'s IcSite). Any NEW stub this design adds
  must (a) tag itself with a kind and (b) teach `real_oop_rootspill_slots` its live count —
  the RootSpill 6→8 history says treat this as a first-class deliverable, not a footnote.

### 1.9 Stub patterns and c2i adapters (src/codecache/stubs.rs)

- `emit_stub_prologue` saves x0..x7 into the 8-slot RootSpill area and anchors
  `last_compiled_fp/pc`; `emit_stub_kind_tag` stamps the kind via x9
  (`stubs.rs:73-122`). `stub_call_primitive` (S-prim shims) is the newest template:
  scalars ride x10/x11 because x0..x5 are live receiver+args; distinct result sentinel
  `PRIM_FAIL_SENTINEL = 0b1010` alongside `BAILOUT_SENTINEL = 0b10` and
  `NLR_SENTINEL = 0b0110` (`docs/prim_shims.md:59-63`).
- Compiled send sites: `emit_call_send` marshals receiver+args into x0..xN via a
  parallel-move solver, `bl`s a patchable target, records a safepoint at the return
  address, then `emit_nlr_check` (`src/compiler/emit.rs:1012-1112`, `emit.rs:1155-1162`).
- Dispatch state machine (`rt_resolve_send`, `stubs.rs:657+`): Unresolved → Mono (patched
  `bl`, target re-verifies via `entry`) → PIC (guard chain, `verified_entry`) → Mega.
  `resolve_target_entry` (`stubs.rs:520-549`): compiled target's entry, else the method's
  **c2i adapter** (`AdapterTable::get_or_make`, keyed by method — the S15 #93 lesson:
  raw-oop-keyed tables MUST rehash after moving GC).
- The c2i path (`build_c2i_shared` `stubs.rs:1035-1054` → `rt_interpret_call`
  `stubs.rs:1080+`): baked method is a HINT, dispatch truth is re-looked-up on the
  receiver's actual klass (`stubs.rs:1107-1149`); pushes `TierLink::IntoInterpreter`; runs
  `run_method_reentrant`; has its own compile trigger + site re-patch so c2i callees don't
  interpret forever (`stubs.rs:1174-1204`).
- **How a `value:` send from compiled code works TODAY**: the site resolves
  Mono{closure_klass → c2i_value:}; `rt_interpret_call` runs the `value:` method
  interpreted; its prim 51 → `activate_block` → `PrimResult::Activated` → the nested
  dispatch loop runs the whole block body interpreted (the S11 step 7
  "entry-sentinel spoof around an Activated primitive" makes this nest correctly). The
  klass guard is USELESS for discrimination — every closure is a `closure_klass` instance,
  so the site is permanently "mono" while dispatching arbitrarily many distinct blocks.
- ~~One open verification item~~ **RESOLVED by review §7**: `Ir::Bailout` is ALREADY
  never constructed by `convert()` — the dead-block placeholder is `Ir::RetSelf`
  (`ir.rs:5016`; the comment at `ir.rs:4990` mentioning "trivial Ir::Bailout blocks" is
  stale), and an existing test asserts the invariant outright ("Ir::Bailout must never
  be constructed by convert()", `ir.rs:5725-5730`, standing since S14 step 3 made smi
  fail paths UncommonTraps). So no current nmethod can return `BAILOUT_SENTINEL`
  mid-body; `EnterResult::Bailout` is legacy belt-and-braces, and §2.1's
  no-bailout-in-block-IR rule is simply the existing global invariant made explicit.
  Keep the `compile_block` assert and cite the existing test; no slice-2 pre-work
  needed.

## 2. Phase A — compiled block bodies + compiled value: dispatch

Phase A splits into three shippable slices (full phasing in §4):

- **A1 — compiled block bodies**, entered from the existing interpreter/c2i activation
  path. Homes are always interpreter frames (nothing about closure creation changes).
- **A2 — direct `value:`-site dispatch** from compiled code (skip the c2i round trip).
- **A3 — compile closure-CREATING methods** whose closures escape but never NLR
  (real BlockClosure allocation + real Context in compiled code).

A deliberately-deferred **A4** (NLR into a COMPILED home) is sketched in §2.4 but cut
from the committed scope; the DeltaBlue gate does not need it.

### 2.1 CompiledBlock as an independently compiled nmethod

**What compiles.** A `CompiledBlock` B becomes an ordinary nmethod through the existing
pipeline (`decode → escape-irrelevant → ir::convert → regalloc → emit → install`), with a
block-specific eligibility wrapper replacing the `is_block` reject at `driver.rs:190`:

- `B.argc() <= 3` — the `value` family's arity range; `valueWithArguments:` callees are
  out of scope (§2.6).
- `!B.has_ctx()` and no `PushClosure` in B's body (v1): a block that allocates its own
  Context or creates nested closures is a later slice. (`captures_ctx` blocks — reading
  the HOME's Context — are IN scope; that is the common DeltaBlue shape.)
- `PushCtxTemp`/`StoreCtxTempPop` allowed at depth 0 only (same v1 line the method gate
  draws, `driver.rs:353-357`). Depth 0 in a ctx-less block names the enclosing Context
  that `activate_block` aliases into FP+3 (`blocks.rs:84-95`) — in compiled form, that is
  simply `closure.copied[1]`.
- `BlockReturnTos` allowed (it is the block's own return → `Ir::Ret`). `NlrTos` allowed
  (§2.4) — in A1 the home of any compilable closure is an interpreter frame, which the
  existing unwind machinery fully handles.
- All standard gates reused verbatim: per-site argc <= 7, frame budget, bytecode length,
  the send-site IC scans. B has its own `ics` array and its own counters
  (`SPEC.md:334-336`, verified by `blocks.rs:579-587`), so site feedback and smi-inline
  fusion work unchanged inside block bodies.
- **No `Ir::Bailout` in block IR** — a block nmethod can be entered via `blr` from a
  compiled `value:` site (§2.3), where a returned `BAILOUT_SENTINEL` would be
  indistinguishable from a result (§1.9). The converter must lower every would-be-bailout
  in a block compilation as an S13 `UncommonTrap` (deopt), and `compile_block` asserts the
  final IR contains no `Ir::Bailout`. (Entry from `enter_compiled` still tolerates
  bailouts; this rule just makes the stronger guarantee unconditional.)

**Customization key.** Blocks get NO receiver-klass customization: `key_klass` is
meaningless (every closure is a `closure_klass` instance; `self` inside the body is the
HOME's receiver, of any klass in the home holder's subtree). The identity that
discriminates block code is **the CompiledBlock oop itself** — one nmethod per
CompiledBlock. Two consequences:

- *Registry*: `code_table.by_key` is (klass, selector)-keyed; block nmethods instead
  register in a new `CodeTable::by_block: HashMap<u64 /*CompiledBlock oop bits*/,
  NmethodId>`. **[AMENDED per review §4]** Lifecycle mirrors `by_key` EXACTLY:
  (a) entries are unhooked in `set_not_entrant` (`nmethod.rs:664-680`) under the same
  successor guard (only remove if the entry still names this id — recompilation
  installs the replacement FIRST), NOT "on flush" (by_key is unhooked at invalidation
  time; a NotEntrant nmethod is never in either table); (b) keys are **rehashed under
  both collectors** by riding `CodeTable::update_keys`/`rehash` (`nmethod.rs:695-800`
  — scavenge passes its scavenge_oop closure, full GC passes forward_chase), which is
  the REAL raw-oop-key precedent. (The review corrected this design's earlier claim:
  AdapterTable's #93 fix was verify-on-hit, and its keys are still never rehashed —
  verify-on-hit alone would leak here via address-reuse false-hits orphaning nmethods,
  so by_block rides the CodeTable phases; optionally ALSO verify-on-hit against the
  nmethod's pool word as defense in depth, plus a debug validate pass after full GC.)
- *Liveness*: the block nmethod's oop pool holds the CompiledBlock (methods already ride
  pools — `Nmethod::oops_do`, `nmethod.rs:485+`), keeping it alive while the code lives.
  No `key_klass` weakness games needed: `is_block` nmethods simply skip the S12 D5
  weak-key sweep (their `key_klass` field is set to a well-known filler and never
  consulted).
- *Instvar soundness without a klass guard*: `push_instvar i` inside B indexes the HOME
  method's holder-klass layout. Any receiver the home method legitimately ran with is an
  instance of the holder or a subclass, and instvar layouts are prefix-stable down the
  subtree — the same argument that makes the interpreter's block execution sound with zero
  guards. So a block nmethod's entry needs NO klass verification; its `verified_entry ==
  entry`.
- *Self-sends inside B* dispatch through ordinary CallSend ICs on the (dynamic) home
  receiver — never `self_devirt` (that customization is exactly what block nmethods
  cannot have). B's IR conversion runs with `rcvr_klass = closure_klass`… no: with a
  sentinel "no customization" klass; every fusion decision that would consult
  `rcvr_klass` for self-sends must see "unknown" (self-sends stay generic sends in block
  bodies, v1).
- *Super-sends inside B* **[ADDED per review §2b — a hidden win]**: work through the
  standard path with NO new machinery, because `patch_block_holders` stamps the KLASS
  into every installed CompiledBlock's holder (§1.1 correction) — `ir.rs:2918-2921`'s
  `KlassOop::try_from(self.method.holder())` succeeds for block compilations exactly as
  for methods. Sound without an entry klass guard: super lookup start is site-static
  (`holder.superclass()`, ic.rs:176-180), and instvar layouts are prefix-stable down
  the subtree — the same argument that makes instvar access guard-free above. (Edge,
  pre-existing: a block in a never-installed doIt has an unpatched holder and a super
  send there panics the interpreter TODAY, ic.rs:189-190 — not this design's problem;
  the eligibility scan may simply decline `send_super` when
  `KlassOop::try_from(holder)` fails.)

**Invocation counting + trigger.** `activate_block` already bumps B's own invocation
counter on every activation (`blocks.rs:57`), from BOTH doors (interpreted `value:` sends,
and compiled `value:` sends that today detour through c2i → `rt_interpret_call` →
`try_primitive` → `activate_block`). The trigger mirrors `activate_method`'s: in
`activate_block`, after `bump_invocation`, if `JitMode::Threshold(n)` and counter >= n and
!B.compile_disabled(), **first probe `by_block` for an existing Alive nmethod and reuse
it** (the send.rs:203-207-style reuse-Alive check — without it, invalidation churn under
DEOPT_STRESS re-compiles storms), else call `compile_block(vm, B)`; on success, enter
compiled immediately (below); on `NoPermanent`, set `compile_disabled` so the check never
re-fires. Placing the trigger in `activate_block` (not at value-send sites) gets every
call path for free and needs no IC-shape changes.

**Recompile-on-trap for block nmethods** **[ADDED per review §5 — A1 scope, not
optional]**: `rt_uncommon_trap` → `note_uncommon_trap` (`recompile.rs:42`) resolves the
method via `lookup(nm.key_klass, nm.key_selector)` on every LIMIT-th trap — meaningless
for a block nmethod (filler key → early return → a trap-dense block, which §2.1's
no-Bailout rule guarantees, re-traps FOREVER; or worse, a filler that accidentally
resolves version-churns an unrelated method). `note_uncommon_trap` must branch on
`nm.is_block` BEFORE the key lookup: resolve B from the nmethod's own oop pool,
`snapshot_profile` over B's own ics (works unchanged — blocks own their `ics`,
blocks.rs:579-587), recompile via `compile_block_versioned` (install-first into
by_block, then `make_not_entrant_lazy` the old one — mirroring recompile.rs:88-107).
The nmethod gains an `is_block` flag (or the filler key is a documented sentinel value
that `lookup()` can never resolve); `key_selector` for a block nmethod naturally holds
the `#aBlock` placeholder (builder.rs:597) — fine once the branch exists. Also note:
block-internal loops never OSR (`rt_osr_request`'s dispatch-truth guard, osr.rs:94, can
never hold for the placeholder selector) — acceptable for the gate; a trap-then-
reinterpret loopy block tiers back up only at its next `value:`.

**Entering compiled block code from the interpreter.** `enter_compiled`
(`compiled_call.rs:61-200`) already does EXACTLY the right thing: it reads
`argv[0..=argc]` from `sp-argc-1..sp-1` — and at a `value:` send, `sp-argc-1` holds the
CLOSURE (the send's receiver). With the §2.2 convention (x0 = closure), a compiled block
is invoked as `enter_compiled(vm, nm_id, argc)` with zero changes to that function.
`prim_value0..3` become:

```
fn prim_valueN(vm, args):
    closure = ClosureOop::try_from(args[0])?          // unchanged
    blk = closure.method()
    if blk.argc() != N: return Fail                    // unchanged semantics; argc check
                                                       // BEFORE the by_block probe (tier
                                                       // consistency, review §7)
    if let Some(nm) = vm.code_table.by_block(blk):     // NEW fast path — Alive ONLY
        match enter_compiled(vm, nm, N) {
            Completed  => return Ok(vm.stack.pop())    // see stack-discipline note
            Bailout    => fall through to activate_block (restart-from-bci-0 is sound:
                          the block prologue is pure loads)
            Nlr(step)  => return the new PrimResult::Nlr(step)   // see below
        }
    activate_block(vm, closure, N)                     // unchanged slow path
```

Two integration notes:

- **Stack discipline of the Completed arm [PINNED per review §1a/§2a]**:
  `enter_compiled` on Completed has ALREADY done `vm.stack.sp = base; push(result)`
  (compiled_call.rs:197-198) — receiver+args gone, result deposited. `prim_valueN`
  therefore POPS the deposited result and returns it as `PrimResult::Ok(popped)`;
  `try_primitive`'s Ok arm then restores `vm.stack.sp = base` ABSOLUTELY (an index
  computed before the prim ran, send.rs:96-106) and the dispatch pushes the value —
  net effect exactly correct. This correctness hangs on (a) the restore being absolute,
  not relative, and (b) nothing allocating between the pop and the push — pin BOTH with
  a comment + debug_assert at the prim_valueN site, and cover with a differential whose
  result is consumed immediately (`(blk value: x) + 1`). The new `PrimResult::Nlr` arm
  must NOT go through the `sp = base` path at all (after an NLR the stack belongs to a
  different activation) — `try_primitive` gains a fourth `PrimitiveOutcome` arm that
  touches nothing, and `run_method`'s try_primitive caller (send.rs:51-69) gets the
  same mapping. All arms are exhaustive-match-enforced; the full ripple list is:
  `try_primitive`, `run_method`/`run_method_reentrant`, and `rt_call_primitive`
  (codecache/stubs.rs), which gets
  `Nlr(_) => unreachable!("50-54/60-61 are PRIM_ACTIVATES_FRAME-excluded")`.
- `PrimResult` gains an `Nlr(UnwindStep)` arm, produced ONLY here. The prim call sites in
  `interpreter::send` already handle `SendOutcome::Nlr` for the compiled-send path (S11
  step 7's fixes, cited at `driver.rs:171-182`); this arm maps onto the same handling.
  Alternative considered and rejected: intercepting at the c2i layer only — that would
  leave interpreted `value:` sends (the six orchestrators!) on the interpreted-body path
  forever, exactly the wrong half to miss.
- `prim_ensure_like` (60/61) must KEEP calling the interpreted `activate_block`
  unconditionally: it arms the handler as a marker on the protected block's interpreter
  frame (`primitives.rs:1038-1043`); a compiled activation has no marker slot. Split
  `activate_block` into the public trigger-bearing entry (used by prims 50-53) and
  `activate_block_interp` (used by everything else). **[AMENDED per review §2a — the
  split must route ALL EIGHT callers explicitly; two were previously unlisted and are
  correctness-critical]**: prims 50-53 (primitives.rs:966,973,980,987) → trigger-bearing
  entry; prim 54 (:1010), prims 60/61 (:1038), AND the two unwind.rs handler
  activations — `intercept_ensure_return` (unwind.rs:131) and `continue_unwind`'s
  Marked arm (unwind.rs:228) — → `activate_block_interp`, ALWAYS. The unwind sites set
  `vm.regs.bci = BCI_RESUME_ENSURE_RET / BCI_RESUME_UNWIND` immediately before the call
  so the new frame's saved_bci carries the resume sentinel (blocks.rs:66-68); a
  compiled handler activation would have no interpreter frame to carry it — the parked
  result/token would be orphaned and dispatch would resume at a sentinel bci (stack
  corruption, loud at threshold=1). Handler activations stay interpreted in this arc,
  same rationale as 60/61's marker arming. Blocks invoked under `ensure:` run
  interpreted in v1 — correct, just not accelerated.

### 2.2 Calling convention for a compiled block

```
entry:   x0 = the BlockClosure oop          (never the home receiver)
         x1..x3 = block arguments           (B.argc() of them)
return:  x0 = result, or NLR_SENTINEL (never BAILOUT_SENTINEL — §2.1)
```

Chosen because it is **identical to what every `value:`-family send site already
marshals** — receiver in x0, args in x1.. (`emit_call_send`, `emit.rs:1012+`) — and
identical to what `enter_compiled` already passes (`argv[0] = stack[sp-argc-1]` = the
closure). No adapter, no re-shuffle, at either door.

**Prologue-synthesized environment** (all plain `LoadField`s, no allocation, no
safepoint):

- `self_vreg  = closure.copied[0]` — the home receiver (`SPEC.md:419-421` pinned
  convention; `blocks.rs:70` is the interpreter twin). `push_self`, `push_instvar`,
  `store_instvar_pop` all key off `self_vreg`.
- `ctx_vreg = closure.copied[1]` iff `B.captures_ctx()` — the home Context.
  `push_ctx_temp 0 <i>` / `store_ctx_temp_pop 0 <i>` become Context slot loads/stores
  through `ctx_vreg` (with write barrier, same as `StoreField{barrier:true}`). This is
  the compiled mirror of the FP+3 aliasing at `blocks.rs:84-95`.
- Value captures (rare; `ncopied > 1 + captures_ctx`): `copied[base+i]` pre-loaded into
  the vregs backing local temps `argc+i` — mirroring `blocks.rs:113-118`.
- Block args: `Ir::Param(1..=argc)` map to unified temps `0..argc`.

The closure vreg itself stays live (spill-homed, oopmap'd) for the whole body: deopt needs
it (the materialized block frame's receiver-arg slot, §2.5) and `nlr_tos` reads
`closure.home` (§2.4). This is ordinary S12 discipline — it is in a canonical frame slot
at every safepoint like any other named value.

**Frame shape**: a completely ordinary tier-1 native frame. The walker classifies it by pc
(§1.8) — no new FrameView, no oopmap changes, no RootSpill changes for the body itself.

### 2.3 value:-site dispatch from compiled code

**The problem**: all closures share one klass, so the existing klass-guarded IC lattice
cannot discriminate — a `value:` site is "mono" on `closure_klass` while dispatching
arbitrarily many distinct blocks. Worse, the hottest such sites are SHARED:
`OrderedCollection>>do:` has ONE `value:` site executing every block in the system that
ever gets `do:`-ed. Method-identity PICs alone would go mega immediately on shared
iterators. The dispatch must key on `closure.method` and must be cheap at high
polymorphism.

**Design: dispatch through the block registry, vtable-style** (A2 slice). A value-family
selector site keeps its ordinary shape (`bl` + NLR check — 2 words, unchanged), but
resolves to a NEW shared stub instead of the generic c2i adapter:

```
stub_value_dispatch (kind tag KIND_VALUE_DISPATCH, one per argc 0..3 or argc in x16):
    emit_stub_prologue                       ; x0..x7 → RootSpill, anchor set
    x10 = site argc                          ; from the per-argc entry or x16
    bl  rt_value_target(vm, x0=closure_bits… ; via the x10/x11 scalar convention)
    cbz x0, .fallback                        ; 0 = no compiled block
    ldp/ldr x0..xN from RootSpill            ; restore closure+args exactly as saved
    emit_stub_epilogue-without-ret
    br  x0-holding-entry (via x16)           ; TAIL-jump to the block nmethod entry
.fallback:
    ; behave exactly like c2i_shared with method = the value: method:
    ; rt_interpret_call(vm, value:_method_bits, argv) → nested interpret
```

`rt_value_target(vm, closure_bits, argc)` (Rust, no allocation, no GC):
1. `ClosureOop::try_from` — non-closure → 0 (fallback replicates today's Fail →
   `value:` bytecode-fallback error semantics, byte-for-byte).
2. `blk = closure.method(); blk.argc() != argc` → 0 (wrongArgc → same interpreted error
   path).
3. `by_block.get(blk)` → `Some(nm)` **with `nm.state == NmState::Alive` ONLY** →
   return `nm.entry`. Anything else → 0 (interpret). **[AMENDED per review §4 BLOCKER
   — the previous "Alive-or-NotEntrant, patched entry routes to the deopt path" was a
   misreading of two mechanisms and would abort the VM (A1 door: not_entrant_stub is a
   RESOLVE stub, stubs.rs:858-876, whose rt_resolve_send panics on a non-send-site
   return address) or livelock (A2 door: entry→resolve→re-route→entry cycle).
   `enter_compiled` has NO NotEntrant self-healing; its callers guarantee Alive
   (send.rs:319-330, :203-207).]** With the §2.1 amendment, a NotEntrant nmethod is
   never IN by_block at all (unhooked in set_not_entrant under the successor guard), so
   the Alive check is belt-and-braces; in-flight activations of an invalidated block
   nmethod drain via the standard §2c return-redirect + §2d poll paths, which operate
   on frames and return addresses, not entries. Drained blocks recompile via the
   activate_block counter trigger (with its reuse-Alive check).
   Optionally: bump B's invocation counter here on the fallback path so warmup
   works even for blocks only ever invoked from compiled sites (mirrors the c2i escape
   hatch's rationale, `stubs.rs:1174-1189`).

Because the stub TAIL-jumps, the block nmethod returns directly to the original send
site: a normal result lands in x0; an `NLR_SENTINEL` hits the site's existing
`emit_nlr_check` and relays outward — **zero new NLR plumbing at the site**.

Why this shape and not the alternatives:

- *Method-identity mono patch / block-PICs* (patch the site to `cmp closure.method,
  #baked_block; b.eq entry`): fastest for private sites, but the shared-`do:` case goes
  mega instantly, and PIC stubs would need a new guard flavor (load-field-then-compare
  instead of klass-load-then-compare). Kept as a LATER optimization layered on the same
  registry (the resolve path can specialize a site that has seen exactly one block);
  not needed to hit the gate, so not in v1.
- *Entry-address word inside the CompiledBlock object*: dispatch in 2 loads, no Rust
  round-trip — but requires either a Method-format layout change or stealing `counters`
  reserved bits, plus repatch-on-invalidation walks. Measurable follow-up (v1.5) if
  `rt_value_target`'s call cost shows up; the registry stays authoritative either way.
- *Keying `by_key` with (closure_klass, synthetic per-block selector)*: abuses the symbol
  table, leaks synthetic symbols, breaks `by_key` invalidation semantics. Rejected.
- *Doing nothing at compiled sites* (A1 only, keep the c2i detour): functional, and A1
  alone already moves the needle (block bodies stop interpreting), but leaves ~3 runtime
  crossings + a nested interpreter activation per element on the hottest path. The
  A2 stub reduces that to stub-prologue + one Rust probe + tail-jump.

GC/walker integration for the new stub: `KIND_VALUE_DISPATCH` AdapterKind; RootSpill live
slots = `1 + argc` (closure + args), argc recoverable exactly like C2i does it — from the
caller site's IcSite via `caller_pc` (`roots.rs:411+` arm). `rt_value_target` never
allocates, but the stress walker must still classify the frame; this is the §1.8
"first-class deliverable" item.

Redefinition/recompilation coherence: the site never caches the block entry (it
re-probes every call), so `make_not_entrant(block nm)` takes effect on the NEXT probe
with no site patching; in-flight activations drain via the standard poll/return-redirect
deopt paths. The one hazard (probe-to-`br` window vs invalidation) is Risk 5 in §5.
**[ADDED per review §4 MINORs]** (a) The value-family routing check
(selector ∈ {value..value:value:value:} ∧ target primitive ∈ {50..53}) must live in
`resolve_target_entry` (stubs.rs:520-549) — not only in rt_resolve_send's Unresolved
arm — so every lattice state (mono re-patch, PIC promotion after a non-closure
receiver) inherits it; otherwise a site that ever goes poly silently falls back to c2i
forever. (b) The stub's fallback bakes NO method oop (a shared per-argc stub cannot
carry a per-method pool word the way c2i adapters do — a baked-but-unmaintained oop is
a #93-family bug): the fallback calls a Rust helper that re-looks-up
(klass_of(x0), selector) itself before `rt_interpret_call`, which re-validates
dispatch truth anyway (stubs.rs:1107-1149). (c) Live-edit leak, accepted + observable:
redefining a method M does NOT invalidate the block nmethods of M's OLD literal frame
(semantically correct — old closures must keep running old code), so they are immortal
until flush; add a MACVM_TRACE=jit counter (`block_nmethods_orphaned`) so churn under
browser live-edit is visible rather than silent.

### 2.4 NLR from a compiled block

**Key structural fact**: in A1-A3 as scoped, the home of ANY closure whose block contains
`nlr_tos` is ALWAYS an interpreter frame:

- A1/A2 change nothing about closure creation — closures are made by `make_closure` in
  interpreter frames; their `home_ref` is a valid interpreter (fp, serial) pair
  (`blocks.rs:174-176, 186-204`).
- A3 compiles a closure-creating method ONLY if every closure it creates is
  transitively NLR-free (§2.7). So a compiled-frame home and a `^`-bearing block never
  coexist.

That reduces "NLR from a compiled block" to ORIGINATION — everything downstream already
exists (§1.4):

**Origination**: `nlr_tos` in a block compilation lowers to `Ir::NlrReturn { value }`:

```
emit:  x0 = closure vreg, x1 = value vreg
       bl  stub_nlr_originate          ; anchor + kind tag; RootSpill live = 2 (x0, x1)
       ; rt_nlr_originate(vm, closure_bits, value_bits):
       ;     debug_assert!(vm.nlr_state.is_none())    // enter_compiled:64-68 precedent
       ;     vm.nlr_state = Some(NlrState { home: unpack(closure.home()), value,
       ;                                    closure: Some(closure) })   // review §2 BLOCKER
       ;     no allocation, no lookup, no GC window
       x0 = NLR_SENTINEL
       b   epilogue
```

**[AMENDED per review §2 BLOCKER — the originating closure MUST ride the escape.]**
`cannot_return_current_closure` (unwind.rs:374-380) reads the closure from the CURRENT
frame's receiver-arg slot and `.expect`s a Closure — sound today only because
interpreted `nlr_tos` runs inside the block's own frame. With compiled origination the
block's native frame is GONE by the time `continue_unwind` runs the liveness check; the
current interpreter frame is the value:-SENDER's, whose receiver-arg is almost never a
Closure — `m value` after `makeBlock ^[^1]`'s home returned would PANIC the VM in
release (or silently send #cannotReturn: to a wrong closure). Fix: `NlrState` gains an
`Option<ClosureOop>` (parked only by rt_nlr_originate; interpreted origination leaves
it None and is untouched); `continue_unwind`'s dead-home arm calls the EXISTING public
`cannot_return(vm, closure, value)` (unwind.rs:391) when a parked closure is present,
falling back to `cannot_return_current_closure` otherwise. NlrState is not a GC root
today and stays that way — the no-alloc-in-transit contract (compiled_call.rs:170-178)
covers the added oop for exactly the same reason it covers `value`. Repro (3) in §4
MUST exercise this compiled-block dead-home path specifically.

The block's native frame unwinds through its own ordinary epilogue carrying the sentinel;
every compiled frame above relays via its existing per-site check
(`emit_nlr_check`, `emit.rs:1155-1162`); the first boundary —
`enter_compiled`'s NLR arm (`compiled_call.rs:179-187`) or `rt_interpret_call`'s nested
run returning the sentinel — resumes `continue_unwind` on the interpreter side. From
there the four required cases are EXISTING behavior, not new code:

| case | mechanism (all existing) |
|---|---|
| home interpreted + live | `continue_unwind` scan → `ReachedHome` → `pop_frames_above` + `do_return` (`unwind.rs:185-189`) |
| home interpreted + live, ensure:/ifCurtailed: between | scan reports `Marked` before boundaries (`unwind.rs:311-315`); handlers run innermost-first with UnwindTokens; re-escapes re-park (`unwind.rs:211-235, 246-267`) |
| home dead (returned before the block ran) | `home_is_live` fails → **the parked `NlrState.closure` routes to `cannot_return(vm, closure, value)` directly** (amendment above — the current-frame receiver-slot read is WRONG for compiled origination; the sender's frame is current, not a block frame) → `#cannotReturn:` (`unwind.rs:176-184, 374-406`) |
| home compiled | **cannot occur in A1-A3** (above). A4 sketch below |
| ensure: BETWEEN two compiled frames | impossible — prims 60/61 are interpreter-only (§2.6), so markers live only on interpreter frames, which sit in interpreter segments the scan already covers |

One new assertion carries the S11 P9 discipline: `rt_nlr_originate` debug-asserts no NLR
is already in flight, mirroring `enter_compiled:64-68`. A handler that itself NLRs while
unwinding re-parks state only AFTER the previous state was consumed by `continue_unwind`
(`resume_unwind`'s re-entry, `unwind.rs:259-266`) — same invariant as today.

**BlockCannotReturn timing**: the interpreter delays the liveness check until
`continue_unwind` runs it; the compiled origination does the same (park + sentinel without
checking) — liveness is evaluated exactly once, at the same point in the sequence the
interpreter evaluates it. The VALUE of the check is timing-identical; what differed —
and what the amendment above fixes — was the CONTEXT of the failure handler (which
frame is current when the check fails), which `cannot_return_current_closure` depended
on. With the parked closure, the handler no longer reads frame context at all on the
compiled-origination path.

**A4 sketch (deferred, for the record)**: a compiled home would need (a) a frame serial
stamped into a fixed spill slot at prologue of any closure-creating compiled method,
(b) `HomeRef` extended with a compiled flavor, (c) a per-method NLR landing pad that, on
sentinel arrival at any site, compares `vm.nlr_state.home` against its own (serial, fp)
and either consumes-and-delivers (x0 = value → epilogue) or keeps relaying, and (d) deopt
of the home preserving the ORIGINAL serial in the materialized frame so parked HomeRefs
stay valid. It is ~a sprint of its own. **[AMENDED per review §5a]** The gate case for
cutting it is the residual-tail argument, NOT "inputsKnown: via Phase B" (B4 cannot
splice inputsKnown:'s multi-basic-block body — §3): under A1+A2, inputsKnown:'s BLOCK
compiles standalone (A1 allows NlrTos + multi-BB) and `inputsDo:` compiles; only the
small creator method stays interpreted (push_closure + one send + returns), a bounded,
named residual in §4's tail arithmetic.

### 2.5 Deopt correctness for block frames

**ScopeDesc identity.** A block compilation's ROOT scope is
`ScopeDescData { method_pool_ix: B, is_block: true, sender: None, receiver: <closure
slot>, slots, ctx: CtxLoc::None }`. Overloading `receiver` to mean "the closure" for
root-block scopes matches the interpreter frame shape (the closure sits in the
receiver-ARG slot; the FP+4 receiver copy is DERIVED as `copied[0]`) and needs no new
metadata field. What identifies "block activation of B with home H" is therefore: the
scope's `is_block` + root position gives B; H is not IN the metadata at all — it is
carried by the closure object itself (`closure.home()`), which the materializer re-reads.
This is deliberate: the home is per-ACTIVATION dynamic state, not per-code static state.

**Materializing an interpreter block frame** (the `deopt.rs:533-541` extension): today an
`is_block` scope is never outermost (`expect` at `deopt.rs:535`). New root-is_block arm,
mirroring `activate_block` (`blocks.rs:60-122`) exactly:

1. read the closure from the scope's `receiver` ValueLoc (a real, live BlockClosure —
   never elided in a standalone block compilation);
2. push closure, push args (slots 0..argc), `push_frame(B, argc, ENTRY_FRAME_SENTINEL or
   the bridge link, header_saved_bci)` — receiver-arg slot now holds the closure, exactly
   like `activate_block`;
3. overwrite FP+4 receiver with `closure.copied(0)`;
4. FP+3 context := `closure.copied(1)` if `B.captures_ctx()` else nil — **identity, not a
   copy** (the `ctxless_block_aliases` test's invariant, `blocks.rs:499-526`);
5. temps: value captures then locals from the recorded slots (they may have been mutated
   since entry — the recorded ValueLocs, not `copied[]`, are the truth for temps).

The nested resume then runs B's bytecode from the recorded bci; a subsequent interpreted
`nlr_tos` reads a REAL closure with a REAL home_ref from the receiver slot — the
`escape.rs:170-175` hazard (elided closure + interpreted nlr) structurally cannot occur
here.

Inlined scopes INSIDE a block compilation (a leaf accessor inlined into B's body) chain
via `SenderLink` off the root block scope as usual; the existing non-root `is_block`
handling (`deopt.rs:533-541`) remains for S14 splices where the root is a METHOD scope.
The materializer distinguishes the two by "is_block AND outermost".

**Deopt of the home while block frames reference it.** In A1-A3 the home is an
interpreter frame — deopt never touches interpreter frames, so the (fp, serial) HomeRef
stays valid by construction. The home's METHOD being redefined mid-flight already
completes under old code (SPEC §6.2 posture); closures hold the old CompiledBlock oop and
the old home frame — no new interaction. (A4 is where this becomes a real hazard; §5
Risk 2 records the design rule for it.)

**Context aliasing between home and block** (Risk 3 in §5): the block's compiled code
holds `ctx_vreg = copied[1]` — the SAME heap Context object the home frame's FP+3 slot
holds. There is exactly one object; compiled block writes go through `StoreField`-style
barriered stores to it; the home (interpreted) reads the same object. Deopt step 4 above
re-uses the same oop. The only way to corrupt this is to MATERIALIZE A FRESH Context for
the block — which step 4 syntactically cannot do (it copies the oop). The
GC_STRESS matrix plus a targeted differential (block increments a captured counter across
a deopt; home reads it after) pins this.

**GC.** Nothing new: block frames are `FrameView::Compiled` classified by pc (§1.8);
their oopmaps cover the closure vreg's spill home like any other value. The two new stubs
(`KIND_VALUE_DISPATCH`, `KIND_NLR_ORIGINATE`) get RootSpill live-counts (1+argc; 2) in
`real_oop_rootspill_slots` — with the §1.8 warning treated as a deliverable: a unit test
per kind, like the existing per-kind arms have.

### 2.6 ensure:/ifCurtailed: (60/61) and valueWithArguments: (54)

**All three stay interpreter-only in this arc. Explicitly deferred, with the interpreter
fallback preserved untouched.**

- `ensure:`/`ifCurtailed:` — the handler is armed as a MARKER on the protected block's
  own interpreter frame (`primitives.rs:1013-1046`; `frame.set_marker`), and the unwind
  scan reads markers off interpreter frames (`unwind.rs:31-43, 316-331`). Giving a
  COMPILED protected-block activation a marker would mean either a parallel native-frame
  marker table or marker slots in compiled frames, plus scan support — a genuinely
  separate protocol (`next_architecture.md:149-155` reaches the same verdict). Cost of
  deferral: blocks invoked BY `ensure:` run interpreted (`activate_block_interp`, §2.1);
  `ensure:` frequency in the DeltaBlue tail is nil (it appears in benchmark scaffolding,
  not the hot planner loops). The NLR interaction keeps working because handlers/markers
  stay interpreter-side, which §2.4's table already covers.
- `valueWithArguments:` — dynamic argc defeats both the fixed-register convention (x1..x3)
  and the per-site static argc the RootSpill/oopmap machinery keys on
  (`primitives.rs:990-1011` is pure stack surgery + `prim_arg_base`, which compiled code
  never sets — same exclusion class as prims 93/94, `driver.rs:80-93`). A compiled
  spread-call is its own small project (`next_architecture.md:149-155` lists it with
  ensure:). Blocks CALLED via `valueWithArguments:` still compile and still run compiled
  when ALSO called via `value:` — but a 54-site itself always takes the interpreted path.

Both deferrals are safe-by-construction: prims 54/60/61 keep returning
`PrimResult::Activated` through the unchanged `activate_block_interp`, and the
`PRIM_ACTIVATES_FRAME` exclusion (`driver.rs:52`) keeps their METHODS out of the shim
pipeline. No compiled path needs to know they exist.

### 2.7 A3 — compiling closure-creating methods (escaping, NLR-free closures)

The six DeltaBlue orchestrators stay interpreted under A1+A2 (they trip the escape gate,
§1.6). A3 opens `eligibility_detail` for a method M with escaping closures when EVERY
escaping closure site s in M satisfies:

- the block B_s — **and transitively every block reachable through B_s's `PushClosure`
  literals** — contains NO `NlrTos`. **[AMENDED per review §2 MAJOR: the transitive
  scan is LOAD-BEARING, not vacuous.]** Whether B_s itself compiles is irrelevant: an
  interpreted B_s still creates its inner blocks, and `home_ref_for_new_closure`
  (blocks.rs:186-204) copies the enclosing closure's home VERBATIM when the current
  frame is a block frame — so an A3 dead-home sentinel would propagate into a nested
  `[:y | ^y]`'s home, turning a legal NLR (M's compiled frame still live!) into
  `cannotReturn:` in release / the debug assert's panic. The scan is statically
  decidable and cheap: `PushClosure <lit>` names a CompiledBlock literal, so recurse
  through the literal tree (B_s's literals → inner CompiledBlocks → their bytecode)
  looking for `NlrTos`; memoize per CompiledBlock;
- B_s is otherwise arbitrary (multi-basic-block bodies are FINE — B_s is not being
  spliced, it runs as its own A1 nmethod or interpreted; `block_is_spliceable` is NOT the
  gate here);
- M itself still passes the ordinary scans (sites, budget, etc.).

Elidable sites keep the S14 splice path (strictly better); the NEW lowering applies to
sites the escape pass marks escaping:

- `push_closure <lit> <ncopied>` → `Ir::AllocClosure`: allocate a `BlockClosure`
  (existing alloc fast/slow path machinery; `AllocSlow` adapter), store `method = B_s`,
  `copied[0] = self_vreg`, `copied[1] = ctx_vreg` iff `B_s.captures_ctx()`, value
  captures from the modeled stack, and `home = HOME_DEAD_SENTINEL` — a packed HomeRef
  that `home_is_live` rejects structurally (proc != 0 is the existing "names nothing"
  arm, `unwind.rs:274-276`; reserve proc = 1 with a doc comment). Sound because the only
  READERS of `home` are `nlr_tos` (statically absent from B_s — the gate),
  `home_ref_for_new_closure` (absent — B_s creates no closures in v1), and
  `cannot_return` paths reachable only from `nlr_tos`. A debug assert in
  `continue_unwind` (`home.proc == 1 → panic("NLR through an A3 dead-home closure")`)
  turns any future violation into a loud failure instead of a wrong `cannotReturn:`.
- M with `has_ctx` → the prologue ALLOCATES a real Context (nctx slots, home_hint nil —
  M is a method, `blocks.rs:20-22`), kept in a ctx vreg; `push_ctx_temp 0 i` /
  `store_ctx_temp_pop 0 i` become slot loads/barriered stores through it. Deopt metadata:
  `CtxLoc::Materialized(<ctx slot>)` — **already representable and already handled by the
  materializer** (`scopes.rs:337`, `deopt.rs:734-737`); S13 built it, S14 just never
  emitted it. The driver's `has_ctx && escape.is_none()` reject (`driver.rs:277-279`) and
  the all-elidable requirement (`driver.rs:262-267`) relax accordingly: has_ctx compiles
  when (elidable ∨ NLR-free-escaping) covers every site. **[PINNED per review §5]**:
  the SEPARATE OSR guard at `driver.rs:601` (`osr_bci.is_some() && (has_ctx ||
  method_has_closure)` → decline) **STAYS UNTOUCHED** — it is not part of this
  relaxation. Removing it would let an OSR entry bind a compiled ctx_vreg to a FRESH
  Context while live closures alias the interpreted frame's old one — Risk 2's
  world-split, in the flagship benchmark (`removePropagateFrom:` = whileTrue: +
  escaping closures = an OSR trigger the day A3 lands).
- The materializer needs NO ElidedClosure for these sites — the closure is a real object
  in a real slot; `ValueLoc::FrameSlot` covers it.

What A3 unlocks (per the §1.6 table): `Plan>>execute` (even without Phase B),
`constraintsConsuming:do:`, `addConstraintsConsuming:to:`, `removePropagateFrom:`,
`markInputs:` — five of the six. `inputsKnown:` (NLR block) needs Phase B's splice
widening instead. The per-iteration closure ALLOCATION cost remains in A3 (one small
young-gen object per do: call — not per element); Phase B then erases even that for mono
sites.

## 3. Phase B — iterator inlining (the multiplier)

**The machinery already exists** — S14 step 7-IV-c built exactly this shape: a mono send
passing a literal block as an argument to a BLOCK-ARG TRANSPARENT callee gets the callee
CFG-inlined with the block spliced at the callee's internal `value:` sites, after which
the closure is local, elided, and the per-element send disappears entirely
(`escape.rs:55-59, 256-395`; `ClosureEscape::blockarg_send_target`). `ir::try_inline_cfg`
already handles loops in the callee (`inline.rs:162-192` — "the callee may have BRANCHES
and LOOPS ... its backward jumps become `Poll`s"), and `block_arg_transparent` already
verifies `OrderedCollection>>do:`'s exact shape: `firstIndex to: lastIndex do: [:i |
aBlock value: (array at: i)]` (`world/15_ordered.mst:87-89`) frontend-inlines to a loop
whose only use of `aBlock` is as the receiver of a matching-argc `value:` — the phantom
rides the operand stack transiently below the `at:` send's operands, which the checker
explicitly permits (`escape.rs:260-265, 306-331`).

**The block-parameter mapping question** (how does the caller's block argument reach the
callee's `value:` sites?) is therefore ANSWERED code, not a design blank:
`blockarg_sends: send bci → (site, arg index)` (`escape.rs:55-59`) tells `ir::convert`
which push_closure site flows into which argument position; the inliner binds the
callee's arg-temp `arg_ix` to the PHANTOM (elided) closure; `block_arg_transparent`
guarantees that temp is immutable and only ever `value:`-received, so every such site
inside the inlined callee splices the block's CFG directly. Deopt anywhere inside
materializes the closure for the rebuilt frames via `ValueLoc::ElidedClosure`
(`scopes.rs:277-289`, `deopt.rs:452-470, 543-550`).

Phase B is the set of WIDENINGS that make 7-IV-c actually fire on the census methods —
each one small, each independently testable:

- **B1 — nmethod-id IC targets.** `blockarg_candidate_ok` bails when the mono site's
  target is an nmethod id rather than a plain MethodOop (`escape.rs:377-381` — "resolvable
  in principle, conservative for now"). Since `do:` compiles early (§1.6: 2 copies in the
  live run), `Plan>>execute`'s `do:` site holds an nmethod id by the time `execute` warms
  — the conservative bail is exactly what keeps the flagship case dead today. Fix:
  resolve the id through `code_table.get(id)` to its key method (nmethods know their
  method via the oop pool) and proceed. Symmetric fix in the splicer's site-resolution.
- **B2 — budget.** `do:`'s inline_cost is its bytecode length (`inline.rs:67-75`), against
  `per_call_cost * 2 = 60` at level 1 (`escape.rs:388-391`, `inline.rs:38-42`). Measure
  `do:`'s actual length; if it misses, either the block-arg bonus widens (SPEC §8.4
  already frames the doubled budget as "inlining the callee is what unlocks inlining its
  block argument" — a tunable, `inline.rs:22-31`) or level-2 recompiles catch it. Decide
  from data, not in this doc.
- **B3 — poly-dominant block-arg callees.** `markInputs:`'s `inputsDo:` is polymorphic
  across constraint subclasses, so the Mono requirement (`escape.rs:373-375`) rejects it.
  Widen to the existing `DominantWithSlowPath` pattern (S14 step 6, `inline.rs:233-237`):
  inline the dominant `inputsDo:` under a receiver-klass guard; the guard's cold edge is
  an `UncommonTrap` whose reexecute stack carries the block as `ValueLoc::ElidedClosure`
  — the trap materializes a REAL closure and re-executes the send interpreted
  (`deopt.rs:566-577` — this exact flow already works for guard-cold block-arg sends).
  No "real send on the slow path" variant in v1: a real send would need the closure to
  EXIST on that path, i.e. partial escape — trap-and-materialize sidesteps that whole
  problem and matches the project's Self-style lazy-cold-path posture (`driver.rs:407-418`).
- **B4 — deopt-soundness for spliced NLR blocks** **[RESCOPED per review §5a — B4 does
  NOT unlock `inputsKnown:`]**: removes the `has_nlr && has_send` gate
  (`escape.rs:170-178`) — a deopt inside a spliced NLR block currently would hand the
  interpreter an `nlr_tos` with no real closure. With A1's materializer extension in
  hand the fix is mechanical: record the block's receiver-slot as
  `ValueLoc::ElidedClosure` in the spliced `is_block` scope, so ANY deopt inside it
  materializes a real closure whose `home = (root fp, serial)` — and root IS the home
  for a spliced block (`deopt.rs:146-147`) — making the interpreted `nlr_tos` sound.
  The spliced fast path's own NLR remains what 7-III already does: a branch to the
  root's epilogue, with the same ensure:-decline rule. **What B4 does NOT do**:
  `inputsKnown:`'s block is MULTI-basic-block (or:/ifFalse: chains,
  deltablue.mst:322-324) and `block_is_spliceable` rejects it at `escape.rs:140`
  (`cfg.blocks.len() != 1`) BEFORE the gate B4 removes — and single-BB is structural
  across every splice mechanism (ir.rs:1259, :1536-1542, :3446). Widening that is a
  real compiler-construction chunk (nested CFG inlining at an interior value: site) —
  a possible future B5, NOT in this arc. `inputsKnown:` in the committed scope: its
  block compiles standalone via A1, `inputsDo:` compiles, and its small creator method
  stays interpreted — the named residual in §4's tail arithmetic.

**Why B composes with A rather than replacing it**: B erases closure + dispatch + frame
for the MONO/dominant fast path of each caller×iterator pair, but every path B cannot
prove — poly beyond dominant, mega `value:` sites, iterators too big to inline, blocks
stored into collections, callers that are themselves blocks — falls back to A's compiled
blocks and registry dispatch instead of the interpreter. A also keeps the SHARED `do:`
fast when called from still-interpreted or never-inlined callers. Conversely A alone
leaves a closure allocation per `do:` call (A3) and a registry probe per element (A2); B
removes both on the hot mono paths. The gate needs both: A is the floor, B is the
multiplier.

## 4. Phasing and gates

Ordering principle (this VM's proven cadence): each slice is independently correct,
independently landable, and observable on its own — never "big-bang at the end".
The smallest shippable slice is A1, and it is chosen deliberately: it adds the new frame
kind, the new deopt shape, and the new NLR origination — ALL the correctness-hazard
machinery — while changing NEITHER closure creation NOR site dispatch. Every hard
invariant gets soak time behind the safest possible entry path (the interpreter's own
`prim_value`) before any fast path exists.

**Why not "argc 0/1, no-NLR blocks first" as the very first cut** (the brief's
suggestion): argc 2/3 costs nothing extra (same convention, same code paths — the arity
is a parameter, not a design axis), so restricting it buys no risk reduction. No-NLR-first
is superficially attractive but backwards: A1's NLR case (compiled block → interpreted
home) is pure REUSE of the S11 sentinel path plus one 10-line origination stub, while
DEFERRING it would mean shipping block compilation that silently declines `inputsKnown:`-
shaped blocks — the exact shape the target workload is full of — and would leave the
riskiest reuse claim (§2.4's table) unverified until late. The genuinely hard NLR (compiled
HOME) is what gets cut, and it is cut from the whole arc (A4), not just the first slice.

| slice | contents | lands when |
|---|---|---|
| A1 | block eligibility + `compile_block` + by_block registry (unhook in set_not_entrant; rehash via CodeTable::update_keys/rehash) + convention prologue + root-is_block materializer arm + `Ir::NlrReturn`/`rt_nlr_originate` (NlrState carries the closure) + `prim_value0..3` enter-compiled hook (Alive-only probe; PrimResult::Nlr + PrimitiveOutcome 4th arm ripple) + `activate_block_interp` split routing ALL 8 callers (54/60/61 + unwind.rs:131/:228) + `note_uncommon_trap` is_block branch + `compile_block_versioned` + per-method tail attribution | tests below green |
| A2 | `stub_value_dispatch` + `rt_value_target` + `KIND_VALUE_DISPATCH` walker/roots arm + value-family site resolution routes to it | A1 soaked |
| A3 | `Ir::AllocClosure` + dead-home sentinel + `CtxLoc::Materialized` emission + eligibility relax (NLR-free escaping sites) | A2 soaked |
| B1-B4 | escape.rs widenings per §3, each its own commit | A-series soaked; B1 first (flagship unlock), then B2 (budget, measured), B3, B4 |

**Per-slice test plan** (the standing bar: `feedback_gc_verify_under_stress`,
`feedback_right_size_verification`):

- *Unit*: extend `driver.rs`-style eligibility tests for the block gate; a
  `compiled_result_equals_interpreted`-family differential for block bodies (the S10
  task-#32 oracle, explicitly kept per `next_architecture.md:183-190`); materializer unit
  tests for the root-is_block arm mirroring `deopt.rs`'s existing
  `materialize_*` suite (incl. context-identity assertion, §2.5 step 4); per-kind
  RootSpill live-count tests for the two new stubs.
- *Repro .mst files* (`tests/repros/` convention): (1) NLR from a compiled block through
  a compiled `do:` to an interpreted home under nested `ensure:` — asserting handler
  order AND delivered value (the `nlr_two_frames_ensure_order` scenario with the block
  compiled, `unwind.rs:681-756` as the template); (2) deopt inside a compiled block
  mid-`do:` (DEOPT_STRESS-driven), asserting the captured-ctx write survives; (3) dead
  home → `cannotReturn:` with a compiled block; (4) A3: escaped closure re-invoked after
  its creator returned (legal — no NLR — must produce identical results to interpreted).
- *Differential*: full `it_tier1` + the 4,609 world tests at threshold=1, byte-identical
  vs `MACVM_JIT=off`.
- *Stress matrix* (every slice, release AND debug): world tests + richards + deltablue
  soak under {GC_STRESS=1, GC_STRESS=full:64, DEOPT_STRESS=64} × threshold=1 — the exact
  matrix that closed S15 (0 failures bar). DEOPT_STRESS matters doubly here: it
  round-robins block nmethods too, exercising §2.3's invalidation coherence and §2.5's
  materializer arm continuously.
- *Perf* (release, `scripts/perf.sh` discipline — never under stress instrumentation,
  `perf.sh:9-17`; goldens regenerated in DEBUG builds only per the JIT-perf-investigation
  rule): after each slice, record the full bench.list table in `docs/PERF.md`'s running
  format.

**The measurable gates** (from `docs/PERF.md`'s current baseline: deltablue off 208ms /
warm 62ms = 3.4x; this doc's fresh run: off 265ms/102.9M bytecodes, warm 71ms/16.7M):

1. **DeltaBlue >= 5.0x** interp/best (perf.sh table row).
2. **Interpreted tail < 10%**: `MACVM_TRACE=count` bytecodes at `MACVM_JIT=off` vs warm
   threshold run; warm/off < 0.10 (today 0.163). **[ADOPTED per review §6 — must-do
   during A1, not optional]**: add per-METHOD interpreted-bytecode attribution (a
   counter keyed by the executing method under MACVM_TRACE=count, ~2h) so the tail
   split — block bodies vs orchestrator bodies vs warmup vs the permanent residue
   (54/60/61-protected blocks, `inputsKnown:`'s creator per §3-B4, PushClosure-bearing
   and has_ctx blocks, which A1's own gate also declines) — is ARITHMETIC before three
   slices are committed, not a qualitative census. Track per-slice: A1 should cut the
   block-body share; A3 the orchestrator share; B the remainder. If the tail stalls
   above 10% with A+B landed, the attribution names the residue directly.
3. **No regression**: richards >= 16x (the a2bfd8b result), fib/arith/sieve within noise.
   Richards is the canary for B-series changes (its blocks are the ELIDED kind — B must
   not perturb S14's existing splices), and for A2's resolve-path change (value-family
   selector routing must not slow ordinary sends).

## 5. Risks — five hardest correctness hazards, ordered

Ordered by (blast radius × subtlety). For each: what it corrupts, the mitigation, and the
test that would catch it. The brief's candidate list is kept but re-ranked, and one of its
five (NLR-through-compiled-home) is re-scoped because the design CUTS it rather than
mitigates it.

**Risk 1 — GC of the closure/environment vregs across in-body safepoints.**
The block prologue loads `self_vreg`/`ctx_vreg`/closure vreg once; every later use
assumes they survived every intervening safepoint (sends, allocs, polls). A missed oopmap
bit or a stale-copy-after-move on ANY of them corrupts silently: `ctx_vreg` pointing at a
from-space Context makes captured-temp writes vanish (the home reads the to-space copy);
a stale closure vreg feeds `rt_nlr_originate` a dead `home` (arbitrary misdelivered NLR).
This is the S7-10/S12 bug family wearing a new coat — the eden-base diagnostic law
(`0x...00000001` under GC_STRESS) will likely be the first symptom seen.
*Mitigation*: the S12 rule is already structural (every deopt-named value in its canonical
slot at every safepoint, `scopes.rs:260-265`); the block-specific addition is that the
closure vreg is ALWAYS live (pinned into the oopmap at every safepoint of a block
compilation, not liveness-derived) — cheap (one slot) and removes the whole analysis
dimension. *Test*: GC_STRESS=1 + full:64 over a block that allocates, then writes a
captured temp, then NLRs — asserting the home observes the write and the NLR value;
plus the standing world-tests matrix.

**Risk 2 — home-Context aliasing between the compiled block frame and its home.**
The design's invariant is ONE Context object, shared by identity (`copied[1]` ==
home FP+3 == block frame FP+3 == `ctx_vreg`). Three places can break it: the block
prologue (must LOAD copied[1], never clone), the §2.5 materializer arm (step 4: alias,
never allocate), and A3's creator lowering (store the SAME ctx vreg into `copied[1]`
that the method's own ctx-temp accesses use). A fresh Context anywhere splits the world:
writes on one side invisible on the other — wrong ANSWERS, no crash, the worst failure
class this VM knows (cf. #93's "permanently running the wrong selector").
*Mitigation*: identity assertions in debug materializer (`materialized block FP+3 ==
closure.copied(1)` — mirror `ctxless_block_aliases`, `blocks.rs:499-526`); the
`transfer`-style differential in §4 repro (2). *Test*: block increments captured counter
N times across a forced mid-loop deopt; home asserts exactly N — under all three stress
modes.

**Risk 3 — deopt/materialization of the block frame itself.**
The root-is_block materializer arm rebuilds a frame shape the materializer has never
built (closure in receiver-arg slot, derived FP+4, aliased FP+3, captures in temp slots).
Any slot off by one and the nested interpreted resume reads garbage that LOOKS like oops
(the S14 pending-stack-ordering bug's exact signature — "silent wrong answers, caught by
soak on first contact"). The reexecute-vs-call-return bci split (`scopes.rs:442-447`)
applies to block bodies unchanged but now with `block_return_tos` as the return op.
*Mitigation*: build the arm by delegating to / mirroring `activate_block` line-for-line
(§2.5's five steps name their `blocks.rs` twins); keep the debug cross-check
(`interpreter_model_height`, `deopt.rs:228-249`) active for block methods — it works on
any bytecode. *Test*: DEOPT_STRESS=64 soak (round-robin invalidation WILL deopt live
block frames constantly); repro (2); `MACVM_DBG_REEXEC` spot-audit during bring-up.

**Risk 4 — NLR origination/propagation state discipline.**
`vm.nlr_state` is a single cell with a strict ownership protocol (park exactly once,
consume exactly once, never enter compiled code with it set — `compiled_call.rs:64-68`,
`unwind.rs:204-208`). `rt_nlr_originate` adds a NEW producer outside `continue_unwind`;
the hazard is a code path where the sentinel gets lost (a site that forgets
`emit_nlr_check` — impossible for CallSend, but the new `stub_value_dispatch` TAIL-JUMP
must be audited: the block's sentinel return flows to the ORIGINAL site's check, which
§2.3 gets by construction) or double-parked (block NLRs while a handler-driven unwind is
mid-flight — cannot happen because origination runs in compiled code, which the unwind
only re-enters after consuming state, but the debug assert makes the argument enforced
rather than believed). Corruption mode: a lost sentinel returns `0b0110` as a "result"
oop → immediate `from_raw` panic in debug, heap nonsense in release.
*Mitigation*: the two debug asserts (originate-side + enter-side, both existing
patterns); grep-level audit that every new `bl`-into-guest-code site is followed by
`emit_nlr_check` (only stub_value_dispatch's `br` is new, and a tail-jump needs none by
construction). *Test*: repro (1) (NLR through compiled do: under nested ensure:), plus
`if_curtailed_both_paths`-style goldens with compiled blocks; the differential suite
catches any lost-delivery as a wrong result.

**Risk 5 — `value:`-site dispatch racing invalidation/recompilation.**
`rt_value_target` returns an entry address that the stub `br`s to with no safepoint
between probe and jump (single-threaded VM: nothing can run in that window — the probe
itself is the last Rust the thread executes before the jump). The REAL hazards are
adjacent: (a) a NotEntrant block nmethod's entry must be its PATCHED entry (routing into
the deopt path) — satisfied because `make_not_entrant` patches the entry itself and the
registry returns the entry address fresh on every probe, never caching stale copies (the
by_key-orphaning lesson, JIT-perf-investigation 027bb7c: the bug was a table entry
outliving the patch); (b) the by_block registry going stale across a MOVING GC (raw-oop
keys — the #93 pattern) — mitigated by the same-phase rehash the adapters now get, plus a
debug validate pass after full GC (every key resolves to an is_block MethodOop whose
registered nmethod's pool agrees); (c) flush ordering: zombie sweep must remove by_block
entries in the same breath as by_key (one shared removal site, not two).
Corruption mode: `br` into freed/rewritten code — arbitrary native crash; or the WRONG
block's code with a valid closure — silent wrong answers.
*Mitigation*: as above; plus `MACVM_DEOPT_STRESS` inherently hammers exactly this
(invalidate-while-hot, every N entries). *Test*: DEOPT_STRESS=64 deltablue soak (blocks
recompile/invalidate mid-`do:` constantly); a targeted repro that redefines a method
whose literal frame holds a hot block (killing the home method) mid-loop and asserts the
in-flight closure keeps executing old code to completion.

*(Re-scoped from the brief's list: "NLR-through-compiled-block HOME identification" — the
A-series never creates the configuration (NLR blocks always have interpreter homes;
compiled homes only exist for NLR-free closures), so it is a CUT, not a risk carried. It
returns as the headline risk if A4 is ever picked up — §2.4's sketch records the serial
registry + landing-pad shape a future design must flesh out. "Home deopt under live block
frames" similarly cannot occur before A4: interpreter homes don't deopt.)*

## 6. Considered and rejected

- **Shimming prims 50-54 like the S-prim shims**: the Activated protocol means "control
  transferred to a new activation" — `docs/prim_shims.md:71-77` already proves the
  call-and-branch shim shape cannot express it. The §2.3 stub is the bespoke protocol
  that CAN (tail-jump into the activation's code).
- **Klass-guarded PICs on `value:` sites** (status quo lattice): cannot discriminate —
  one klass, many blocks. Method-identity PICs: die at shared iterators (`do:`'s one site
  sees every block in the image). Registry dispatch chosen; identity-mono patching kept
  as a measured later optimization for private sites.
- **Synthetic per-block selectors in `by_key`** ((closure_klass, gensym) keys): leaks
  symbols, corrupts the by_key invalidation model, and `lookup()` semantics were never
  meant to see them. by_block table instead.
- **Entry-address word in the CompiledBlock object** (Method-format change or counters
  reserved bits): fastest dispatch, but a layout change ripples through
  wrappers/frontend/image tooling for a cost A2's probe may not justify — measure first,
  keep as v1.5. (S22's DB stores SOURCE, so layout changes are cheaper here than in
  binary-image VMs — noted for the future.)
- **Customizing block nmethods by home-receiver klass** (key_klass = home receiver):
  buys self-send devirt inside blocks at the price of an entry guard keyed on
  `copied[0]`'s klass + one nmethod per (block, klass) pair + a miss path. DeltaBlue's
  hot blocks send to their ARGS, not self — no payoff for the gate. Deferred with the
  recompilation system as the natural home for it (a level-2 block compile could
  customize off observed `copied[0]` feedback).
- **Extending HomeRef to name compiled frames now** (A4 in v1): the serial-registry +
  landing-pad design (§2.4 sketch) is coherent but is a sprint of its own, and the
  committed scope's residual is small and bounded (**[corrected per review §5a]**: NOT
  "zero methods need it once B4 splices inputsKnown:" — B4 cannot splice it; the
  residual is inputsKnown:'s small creator method, named in §4's tail arithmetic). The
  project's history (S14 deferring exactly this, correctly) supports cutting it again —
  this time with the boundary drawn PRECISELY (transitive NLR-free gate at A3,
  statically checked).
- **Materializing a real Context for every compiled block activation** (Self-style
  uniform environments): would erase S14's elision wins and regress richards — the
  design keeps elision as the preferred path and adds compiled REAL contexts only where
  escape forces them (A3).
- **Handling `ensure:` compiled in this arc**: needs a native-frame marker protocol; the
  census shows no hot-path need; deferred with its interpreter fallback intact (§2.6).
- **`catch_unwind`/Rust-stack tricks for NLR**: never on the table — the sentinel path
  is proven, and the embedded-VM constraint (JIT supported, thread-boundary crash
  safety — `feedback_jit_must_be_supported_embedded`) rules out anything that fights the
  epilogue-propagation model.

## 7. Summary of what gets reused vs built

| reused verbatim | extended | new |
|---|---|---|
| NLR sentinel relay (`emit_nlr_check`), `continue_unwind`, handler/token machinery | materializer: root-is_block arm | `compile_block` eligibility wrapper |
| `enter_compiled` (unchanged for block entry) | `PrimResult`: +`Nlr` arm | `by_block` registry (+GC rehash) |
| tier links, walker, FrameView (zero changes) | `prim_value0..3`: enter-compiled hook | `stub_value_dispatch` + `rt_value_target` |
| oopmap/RootSpill discipline | `real_oop_rootspill_slots`: 2 kinds | `rt_nlr_originate` + `Ir::NlrReturn` |
| c2i adapters as the uncompiled fallback | escape.rs: B1-B4 widenings | `Ir::AllocClosure` + dead-home sentinel (A3) |
| `CtxLoc::Materialized` (S13 built, unused) | eligibility: has_ctx/escaping relax (A3) | — |
| ScopeDesc `is_block` flag, `ElidedClosure`, SenderLink | `activate_block` split (trigger vs interp-only) | — |

The column widths tell the story: this is a wiring project on top of S11/S13/S14
machinery, not a new compiler. The genuinely new invention is small (one dispatch stub,
one registry, one materializer arm, one IR op pair) — and the one genuinely open
compiler-construction problem (compiled NLR homes) is fenced OUT of scope with a static
gate, not papered over.
