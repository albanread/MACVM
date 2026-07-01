# Reference-VM Analysis

Source-grounded analysis of the VMs MACVM draws on, plus the reusable Apple-
Silicon codegen assets in the sibling projects. Each section is derived by
reading the actual cloned source (`../self-repo`, `../strongtalk-repo`,
`../JASM`, `../MacModula2`). This document feeds `DESIGN.md`.

Sections:
1. Self — object model & memory ✅
2. Self — dispatch & adaptive compilation ✅
3. Strongtalk — VM & type system ✅
4. Codegen backend — JASM AArch64 encoder + macOS JIT ✅
5. MacNCL GC — reuse assessment ✅ (verdict: lessons + scaffolding, not engine)

---

## 1. Self — object model & memory

Grounded in `../self-repo/vm/src` (portable code under `any/`, arch code under
`i386/`, `sparc/`, `ppc/`). **This is the classic 32-bit Self VM** (`runtime/types.hh —
typedef int32 smi`; `memory/stringTable.cpp:35` asserts `BitsPerByte*BytesPerWord == 32`).
The single biggest up-front MACVM decision: stay 32-bit-faithful or widen to
64-bit (touches tags, mark word, smi range, float encoding). **MACVM goes 64-bit.**

### 1.1 Object header & maps
- Every heap object (`memOop`) is `[mark][map][ ...body... ]` — two header words.
  `objects/oop.hh — oopClass` declares word 0 `markOop _mark`; `objects/memOop.hh —
  memOopClass` declares word 1 `mapOop _map`. Body oops via
  `oopsOop.hh — oopsOopClass::oops(i)`.
- **Mark word** (`objects/markOop.hh`): `marked<1> age<7> hash<22> tag<2>`. Age =
  generation-scavenge tenuring age; hash = identity hash; low 2 bits = tag.
  → *arm64:* on 64-bit there are 54 spare bits; widen hash, drop packing tricks,
  keep age small (scavenger reads it hot).
- **Map** (`objects/map.hh — class Map`) = Self's shared shape+behavior descriptor
  (analogue of a hidden class). Maps are **untagged C++ objects with a vtable**,
  embedded inside a tagged `mapOop` (`mapOop.hh`, `map.hh:11`). First map word is a
  C++ vtbl pointer → per-object-kind dispatch (`slotsMap`, `floatMap`, `blockMap`,
  `byteVectorMap`…; `memory/universe.hh — FOR_ALL_MAP_TYPES`).
- `slotsMap` (`objects/slotsMap.hh`) stores `object_length`, `slots_length`,
  `annotation`, with **slot descriptors inline right after the map**
  (`Map::slots() = (slotDesc*)(this+1)`). Each `slotDesc` (`objects/slotDesc.hh`) is
  4 words `{ name, slotType, data, annotation }`.
- **Map sharing:** structurally-identical objects share one canonical `Map*` via
  `memOop.cpp — memOopClass::set_canonical_map()` interning through
  `Memory->map_table` (`memory/mapTable.*`). This is the core space win — N clones
  cost only `[mark][map][data…]` and share all methods/constants.

### 1.2 Tagged values (`objects/tag.hh`) — 2-bit low tag
| Tag | Const | Meaning |
|-----|-------|---------|
| 0 | `Int_Tag` | smi — tag 0 so ALU add/sub work on tagged ints directly |
| 1 | `Mem_Tag` | heap pointer (`memOop`) |
| 2 | `Float_Tag` | **immediate float** (not heap-boxed) |
| 3 | `Mark_Tag` | mark word / sentinel |

- **smi** (`smiOop.hh`): `(v<<2)+Int_Tag` → 30-bit signed; add/sub need no untag.
- **Floats are immediates** (`floatOop.cpp`) by *re-encoding* a 32-bit IEEE float to
  steal 2 low bits — default narrows the exponent to 6 bits (loses range).
  → *arm64 — most important float decision:* do **not** port the 6-bit-exponent
  hack. On 64-bit use NaN-boxing / a 62-bit immediate float, or heap-box doubles.

### 1.3 Slots (`objects/slotType.hh`, `slotDesc.hh`)
- `obj_slot_type` = assignable data slot, value **in the object body** (mutating it
  does not change the map); `map_slot_type` = constant/method slot, value **in the
  map** (shared by all objects with that map); `arg_slot_type` = method argument.
  Flags: `is_parent`, `is_vm_slot`.
- **Assignment slots** (`assignmentOop.hh`, `slotsMap.cpp — copy_add_assignment_slot`):
  a writable field `x` is really a data slot `x` + an assignment slot `x:`.
- **Inheritance = parent-slot delegation** — no class chain. A `is_parent()` slot
  designates an object to delegate failed lookups to; `map.cpp — Map::find_slot`
  does the per-map name search, cached by `FindSlotCache`.

### 1.4 Cloning & copy-on-shape-change
- `slotsMap.cpp — slotsMap::clone()` copies header+body, **keeps the same `Map*`**,
  `init_mark()` for fresh identity. Fast fixed-size `clone0..9_prim` bump-allocate
  in eden. `can_inline_clone()` lets the compiler inline `_Clone`.
- Any add/remove-slot produces a **new object with a new/re-canonicalized map**
  (functional shape change): `copy_add_slot → copy_add_new_slot → Map::insert`, then
  `set_canonical_map` re-joins an existing shape if one matches.

### 1.5 Memory & GC (`memory/universe.hh — class universe`, singleton `Memory`)
- **Two generations + code zone.** `newGeneration` = eden + from + to
  (Ueno/Appel two-survivor); `oldGeneration` = linked `oldSpace`s + reserve;
  `zone* code` = nmethod heap, GC'd separately. Each `space` segregates **oops area
  (grows up) from bytes area (grows down)** (`space.hh:33-36`) so the scavenger scans
  only oops.
- **Direct pointers, no indirection table** in normal execution. An `oTable`
  (`memory/oTable.hh`) exists **only during full GC** for pointer forwarding.
- **Scavenge (minor GC)** — Cheney copying over the young gen
  (`universe.more.cpp — universe::scavenge`): scavenge roots (VM oops/maps/strings,
  **process stacks**, **code zone**), process dirty cards
  (`scavenge_recorded_stores`), Cheney loop, swap survivors, recompute tenuring
  threshold from `ageTable`. Copy+forward via `oopsOop::scavenge` using the mark word
  as the forwarding pointer (`memOop.hh — forward_to/forwardee`).
- **Write barrier** (`objects/oop_inline.hh:366 — universe::store`): card-marking
  remembered set (`memory/rSet.hh`), **128-byte cards** (`card_shift=7`),
  **value-conditional** (`if contents->is_new()`). Cheap: branch + byte store.
  → *arm64:* port near-verbatim, keep inlined (`mov #0` to `card_base+(addr>>7)`).
- **Full GC** — **mark + compacting** (not mark-sweep):
  `universe.more.cpp — universe::garbage_collect` marks roots via the `oTable`, then
  slides/compacts each space forwarding through `oTableEntry`s.
- **GC ↔ compiled code — precise roots (the arm64-critical part).** Self is precisely
  collected *through JIT nmethods*. `zone::scavenge_contents/gc_mark_contents` walk
  compiled frames via `runtime/frame.cpp — frame::scavenge_contents` using a
  **`FrameIterator` + `RegisterLocator`** pair that reads the nmethod's scope/location
  descriptors (`zone/nmethodScopes.*`, `scopeDescRecorder.*`, `Location`) — Self's
  equivalent of **stack maps / oop maps**. **Derived/interior pointers** are handled
  by `memOop.hh — derived_offset` + `DERIVED_*_TEMPLATE` closures.
  → *arm64 (largest porting cost in memory):* the JIT's register allocator must emit
  per-nmethod oop-location metadata at every GC-safe point (a `scopeDescRecorder`
  analogue), plus derived-pointer base+offset re-derivation, plus frame chaining
  (`zone.hh — chainFrames`) so the collector can find all frames of a moved nmethod.

### 1.6 Five biggest MACVM decisions from this analysis
1. **Word width** — go 64-bit (re-derives tags, mark word, smi range, floats).
2. **Immediate floats** — replace Self's truncated-exponent hack with NaN-boxing /
   heap doubles.
3. **Precise GC through JIT code** — emit oop-location maps from the arm64 register
   allocator + derived-pointer handling. Biggest single cost.
4. **Card-marking write barrier** — ports almost verbatim; keep inlined.
5. **Map canonicalization + copy-on-shape-change** — preserve exactly, or lose
   Self's space model.

**Key files** (relative to `../self-repo`): tags/immediates
`vm/src/any/objects/{tag.hh,smiOop.hh,floatOop.cpp}`; header/pointers
`{oop.hh,memOop.*,oopsOop.*,markOop.hh}`; maps/slots
`{map.*,mapOop.hh,slotsMap.*,slotDesc.hh,slotType.hh,assignmentOop.hh}` +
`memory/{mapTable.*,slotIterator.*}`; memory/GC
`memory/{universe.hh,universe.more.cpp,generation.hh,space.*,rSet.*,oTable.*,ageTable.*,oopClosures.hh}`;
GC↔code `runtime/frame.*` + `zone/{zone.hh,nmethodScopes.*,scopeDescRecorder.*}`.

---

## 2. Self — dispatch & adaptive compilation

Two native compilers descend from `runtime/aCompiler.hh — AbstractCompiler`:
the **NIC** (non-inlining "fast" compiler, `fast_compiler/ — FCompiler`) and the
**SIC** (inlining/optimizing compiler, `sic/ — SICompiler`). Interpreter fallback
in `interpreter/`.

### 2.1 Call sites & inline caches
- **`lookup/sendDesc.hh — sendDesc`**: per-call-site record, co-located with the
  call's return PC. Holds patchable `jump_addr()`, `lookupType()`, `selector()`,
  `mask()` (live-register `RegisterString` for GC/deopt), `dependency()`.
- **Monomorphic**: `jump_addr()` → target nmethod's *verified entry point*; the
  callee prologue guards the receiver map (no map-compare at the call site).
  Empty cache → trapdoor lookup stub.
- **PIC** (`lookup/cacheStub.hh — CacheStub`): generated stub of receiver-map
  compares (smi/float as inline immediate tests). Grows via `sendDesc::extend →
  CacheStub::extend` until `arity() == MaxPICSize`.
- **Megamorphic**: `makeMegamorphic()` pins arity and overwrites a case.
- **Slow path**: `sendDesc::sendMessage` probes global code table
  `Memory->code->lookup(MethodLookupKey)`, backpatches, else full lookup + compile.

### 2.2 Type feedback (`sic/`)
SIC reads *runtime* PIC contents back as recompilation scopes:
`CacheStub::get_map/get_method/countStub` → `rscope.cpp — RScope::getCallees` builds
one `RPICScope` per observed (map, method, count). `sicInline.cpp —
SCodeScope::picPredict` turns each into an `SExpr`, merges alternatives (untaken →
`UnknownSExpr`/unlikely). Runtime feedback first; `typePredict` fills gaps with
selector heuristics (booleans, `+ - * /` → smi, `at:` → vectors).

### 2.3 Tiers & (re)compilation selection
- **Hotness via count stubs** (`zone/countStub.*`) on call edges. `recompile.cpp —
  recompile_init` sets `nstages` (2 when both compilers built;
  `recompileLimits[0]=10*K`). `currentCompiler()` escalates one level (NIC→SIC).
- **NIC**: per-bytecode straight-line emit → `nmethod::new_nmethod`.
- **SIC**: node IR → `buildBBs` → optimize (def-use, copy prop, `optimizeTypeTests`)
  → regalloc (`SICAllocator`) → `computeMasks()` → emit + `ScopeDescRecorder`.
- **Recompilation policy** (`recompile.cpp — Recompilation`): walks ≤100 frames,
  often recompiles the **caller** so the hot callee inlines; new nmethods inherit
  old ICs (`forwardLinkedSends`); `checkEffectiveness()` stops thrashing.

### 2.4 Customization, inlining, splitting, uncommon branches
- **Customization**: nmethods keyed by `(receiverMap, selector[, holder])`
  (`NMethodLookupKey`) — compiled specialized to a receiver type, reused across
  clones sharing that map.
- **Inlining** (`sicInline.cpp`, budgets `sic/inlining.*`): once receiver map known,
  inline within a cost budget; declined sends marked `Uninlinable`.
- **Splitting** (`sicSplit.cpp — SplitSig`): duplicate nodes between a type-merge and
  a send along each typed path so each copy sees one type; bounded `MaxSplitDepth=7`.
- **Uncommon branches** (`runtime/uncommonBranch.*`): unlikely cases emit a trap;
  `handleUncommonTrap()` bumps a count and **deoptimizes the frame** (PC →
  `ReturnTrap2`); crossing `UncommonTrapLimit` schedules recompilation.

### 2.5 Deoptimization / OSR / scope descriptors — THE CRUX
- **Invalidation**: nmethods register **dependencies** on speculated map slots
  (`lookup/deps.* — dependencyList`); a slot add/assign/redefine invalidates
  dependents (`nmethod::invalidate/makeZombie`). On-stack frames patched to deopt.
- **Scope descriptors** map native PC → the inlined source-scope chain + variable
  locations. Recorded `zone/scopeDescRecorder.*`, stored packed `zone/nmethodScopes.*`,
  decoded `asm/scopeDesc.* — ScopeDesc`. Each carries `senderScopeOffset` +
  `senderByteCodeIndex` (an inlined scope knows its caller + inline point).
- **`asm/pcDesc.hh — PcDesc`** = `{pc, scope, byteCode}`; one native frame decodes
  into a *chain* of virtual scopes.
- **`asm/nameDesc.* — NameDesc`**: where each variable lives (register/stack
  `Location` or constant).
- **Virtual frames** (`runtime/vframe.*`): rebuild an optimized frame into one
  `compiled_vframe` per inlined scope.
- **Two conversions**: (1) deopt-to-unoptimized — redirect to `ReturnTrap2`,
  `conversion.cpp — copyVFrame`; (2) OSR into new code —
  `Recompilation::replaceOnStack()` (`getVScopes` → `replaceFrames`+`fillValues` →
  continue at new PC/SP).
  ⚠️ **SPARC is the only complete path** — i386/ppc have `fatal(...)` stubs for DI
  recompilation and `ContinueAfterReturnTrap`. MACVM writes the arm64 frame-copy +
  continuation glue from scratch.

### 2.6 Assembler / codegen — what MACVM's `Assembler` must expose
- **`asm/asm_abstract.hh — BaseAssembler`**: two growable buffers — instructions
  (`offset/addr/setOffset`, raw emit, `saveExcursion/endExcursion`) and **locations**
  (`addrDesc` reloc array via `genLoc`). `doAddOffset(OperandType, isEmbedded, mask)`
  tags a reloc; `OperandType` = register / number / **oop** / VM-address / **prim
  call** / **send-stub** / **code-address** / **DI**.
- **Labels** (`asm/label.hh`): `define`/`unify` forward-ref chains + branch backpatch.
- **nmethod** (`zone/nmethod.hh`): C++ object, then `insts()`, then `locs()`
  (`addrDesc[]`), plus `scopes` + `deps`. Entry offsets
  `verifiedOffset/diCheckOffset/frameCreationOffset`. Lives in the movable **zone**.
- **`zone/addrDesc.hh — addrDesc`**: packed 32-bit reloc descriptor
  (isSendDesc/isDIDesc/isPrimitive/isEmbedded/isUncommonTrap/isRelative + offset).
  `referent/set_referent` read/patch embedded oops or call targets (+ i-cache flush);
  `relocateTarget(delta)` for code motion. *GC*: visit `isOop()` locs. *IC patch*:
  `isSendDesc()` → `jump_addr()/set_jump_addr()` + `forwardLinkedSends`.
- **Minimum arm64 backend surface**: code + `addrDesc` buffers with excursion; raw
  32-bit word + oop emit; labels with define/unify + AArch64 branch/ADRP+ADD; reloc-
  tag distinguishing oop/VM-addr/prim/send-stub/code-addr/DI/uncommon + rel/abs;
  per-arch `referent/set_referent/relocateTarget` for ADRP/ADD + BL/B; scope/PC/name
  metadata; entry offsets + map-guard prologue.

---

## 3. Strongtalk — VM & type system

Strongtalk is **Self technology (type feedback, PICs, deopt) on a class-based
Smalltalk**. Recurring theme: **it keeps Self's dynamic-optimization machinery but
drops Self's two most expensive representational choices — prototypes/maps and the
object table — for classes and direct pointers.** For a modern Rust arm64 VM, that
trade is exactly right. Repo HEAD `39b336f` ("merge disassembler relocation info").

### 3.1 Object model — classes + tagged oops + klassOop/Klass
- **Two disjoint C++ hierarchies** (`oops/oopsHierarchy.hpp`): an **oop tree**
  (`oopDesc → memOopDesc → …`) that are pure **representation descriptors with no
  vtable**, and a **Klass tree** (`Klass → memOopKlass → …`) that **has the vtable
  and implements all behavior**. Objects forward "virtual" calls to their klass;
  those methods are prefixed `oop_` (`klass.hpp:31-37` — deliberately *no C++ vtbl
  per object*).
- **Tagging** (`topIncludes/tag.hpp`, 2-bit): `Int_Tag=0` (smi), `Mem_Tag=1`
  (heap), `Mark_Tag=3`. `Mem_Tag=1` (not 0) is deliberate so old/new boundary
  compares work on the tagged value. smi via arithmetic shift.
- **2-word header** (`memOop.hpp`): `markOop _mark` + `klassOop _klass_field`
  (direct class pointer). Offsets pre-biased by tag for codegen.
- **Mark word** (32-bit): `sentinel:1 near_death:1 tagged_contents:1 age:7 hash:20
  tag:2`. `age` is **overloaded to hold object size during GC** (a 32-bit-cramped
  hack — do NOT replicate on 64-bit).
- **klassOop/Klass**: a class is a `memOop` header **immediately followed inline by
  an embedded `Klass`** (`vtbl, non_indexable_size, has_untagged_contents, classVars,
  methods, superKlass, mixin`). `blueprint()` reaches the `Klass*` by pointer
  arithmetic. **Mixins** are the unit of behavior (Bracha model). `Klass::Format`
  enum is the VM-level type tag.
- **vs Self**: Strongtalk's `Klass` plays the VM role Self's map plays (layout +
  dispatch metadata), but the *language* model is Smalltalk classes/metaclasses, not
  prototypes — fixed layouts, no map-splitting on slot add.

### 3.2 Memory & GC — gen scavenge + mark-sweep, card marking, **NO object table**
- **Two generations** (`memory/universe.hpp`): new = **eden + two survivor spaces**
  (bump-pointer eden); old = linked `oldSpace` segments each with an **offset array**
  for card scanning.
- **Scavenger** (copying): Cheney copy to survivor; forwarding **in the mark word**
  (`is_forwarded/forwardee`). **Adaptive tenuring** via `ageTable` recomputes the
  threshold each scavenge (Self/HotSpot lineage).
- **Mark-sweep** (major, compacting, `markSweep.cpp`): **pointer-reversal marking**
  (no mark stack), forwarding-address computation, slide-compact. Object size stashed
  in the mark `age` field during marking.
- **Write barrier** (`memory/universe.store.hpp`, `rSet.hpp`): **card marking**,
  `card_shift=9` → **512-byte cards**, **dirty = byte 0** (a single `strb wzr` on
  arm64). `Universe::store` dirties unconditionally when checked; klass-field stores
  skip it (classes always tenured). Scavenge scans only dirty old-space cards.
- **NO GC object table — direct tagged pointers.** (The only `objectIDTable` is for
  debug printing.) `documentation/tour/virtualmachine.html` lists "direct pointers,
  eliminating object-table indirections" as a headline feature. **This is the single
  most important endorsement for MACVM: do NOT build an object table** — direct
  pointers + card-marked remembered set + compacting old gen.

### 3.3 Interpreter — generated threaded dispatch, self-modifying send opcodes
- **256 bytecodes** (`interpreter/bytecodes.*`), grouped, with predicted inlined
  arithmetic (`smi_add/sub/…`), inlined control flow, NLR returns.
- **Generated single block of machine code**, **direct-threaded** via two parallel
  256-entry tables (`dispatchTable.cpp`): `original_table` (pristine) + `dispatch_table`
  (patchable). Handlers end with `jmp [dispatch_table + bytecode*4]`. Single-stepping
  = patch `dispatch_table` entries.
- **Send bytecode = interpreter-level inline cache** (the key trick): each send is
  followed by an 8-byte embedded IC `{selector/method/jumptable, klass/PIC/0}`
  (`interpretedIC.hpp`) with 4 states (empty / mono-interpreted / mono-compiled /
  polymorphic-PIC-objArray). On miss, `inline_cache_miss()` updates the cache **and
  rewrites the send opcode itself** (`interpreted_→polymorphic_→megamorphic_`, or
  `compiled_/access_/primitive_`) — the opcode *is* the cache state, so the hot path
  needs no check.
- **methodOop** (`oops/methodOop.hpp`): `_counters` (invocation|sharing — the
  recompilation trigger), bytecodes inline after the header. Primitives via
  `primitive_desc` flags (`can_scavenge/can_perform_NLR/can_fail/can_be_constant_folded`).
- → *Rust*: keep the IC *states*, but move the cache to a side table (keep bytecode
  immutable/shareable) and use `match`/tail-calls, not self-modifying bytecode.

### 3.4 Optimizing compiler — type-feedback adaptive inlining
- **Pipeline** (`compiler/compiler.cpp — Compiler::compile`): `NodeBuilder` bytecode→IR
  → `buildBBs` → escaping-block analysis → `makeUses` (def-use) → copy prop + dead-node
  elim + integer-loop opt → block info → float-temp alloc → **regalloc** → codegen →
  `new_nmethod`. Recompilation rebuilds an inlining plan from prior scope descriptors
  and/or the persistent **inlining database** (`code/inliningdb.cpp`).
- **IR** (`compiler/node.hpp`): Trivial vs NonTrivial nodes (Prologue, Load/Store,
  `TArithRRNode` tagged-smi arithmetic, `SendNode/PrimNode`, `TypeTestNode`,
  `UncommonNode`…). Values are `PReg` pseudo-registers; typed annotations in `Expr`
  (`KlassExpr/ConstantExpr/MergeExpr` + "Unlikely" flag). **Not strict SSA.**
- **Regalloc** (`regAlloc.hpp`): per-block local alloc + global usage-count heuristic
  with spilling. Simple — a **linear-scan/graph-coloring upgrade is natural in Rust**.
- **nmethod** (`code/nmethod.hpp`): `nmFlags` (version/level/age/state/…). Two entry
  points (`entryPoint` with klass check, `verifiedEntryPoint`). Layout: code → oop/reloc
  info → `nmethodScopes` (compressed scope descs) → `PcDesc[]` (pc→scope+bci).
- **Compiled ICs/PICs** (`code/compiledIC.hpp`, `compiledPIC.hpp`, `max_nof_entries=4`):
  `[klass, nmethod|methodOop]` pairs; >4 → megamorphic.
- **Deopt** (`recompiler/`): counter overflow at a safepoint (`VM_Operation`);
  `findRecompilee` walks an `RFrame` stack (often picks the caller). Uncommon traps
  force recompilation. Stack deopt converts compiled→interpreter frames via scope
  descs + `PcDesc`, re-executing the current bytecode. `MaxRecompilationLevels=4`.
- → *Rust*: **the compiler is the crown jewel to keep from Strongtalk over Self** —
  a complete type-feedback optimizer with real deopt; the class model makes
  customization tractable. Preserve phase structure, PIC/CompiledIC design,
  scope-descriptor deopt; modernize IR→SSA and allocator→linear-scan.

### 3.5 Assembler / codegen / relocation — dual-stream CodeBuffer + packed relocInfo
- **Assembler/MacroAssembler** (`asm/assembler.hpp`): two-tier x86 emitter; `Label`
  encodes bound/unbound in sign of `_pos`; `inline_oop(oop)` emits the oop **and an
  `oop_type` reloc record**.
- **CodeBuffer** (`asm/codeBuffer.hpp`): **two parallel growable streams** —
  instructions + relocation. `relocate(at, type)` appends a **byte-delta-encoded**
  reloc record; `copyTo(nmethod)` pads/copies both streams then
  `fix_relocation_at_move(delta)`.
- **relocInfo** (`code/relocInfo.hpp`): **16 bits = 3-bit type + 13-bit offset delta**
  (HotSpot-style compressed stream). Types: `oop/ic/prim/runtime_call/external_word/
  internal_word/uncommon/dll`. `call_end = addr+4` is **INTEL-SPECIFIC** (a porting
  point). (Source bug noted: `isExternalWord()` compares against `internal_word_type`
  — don't replicate.)
- **Embedded oops & GC**: each baked-in oop gets an `oop_type` reloc; `nmethod::oops_do`
  walks the `relocIterator` and updates embedded oops in place — **how compiled code
  stays GC-correct with direct pointers and no object table.**
- → *Rust/arm64*: reimplement the dual-stream CodeBuffer + delta-packed reloc almost
  verbatim, but you **can't drop a 64-bit oop as one immediate** — use a literal pool
  or `adrp/movz/movk` sequences, each with its own reloc kind; 4-byte fixed branches.
  Keep `oop_type` reloc as the GC's handle. **This aligns with the JASM encoder (§4),
  whose reloc kinds (`Branch26/AdrpPage21/AddPageOff12/Abs64`) are exactly these.**

### 3.6 Type system — pluggable, optional, **entirely image-side; VM never sees it**
- **The VM has zero typechecker logic** (grep of `vm/**` finds only runtime `Format`
  tags). Per `documentation/type-system/nwst.html`: *"No changes are needed to the
  underlying compiler or runtime because the type system is optional"* and *"the
  dynamic behavior … must not be influenced by any type annotations."* The optimizer
  uses **concrete** type feedback (PICs), never these interface types.
- **Design intent** (Bracha/Griswold, OOPSLA '96): a pluggable, optional interface
  type system — **types are `protocol`s (selector-signature sets), separating the
  subtype lattice from the subclass lattice**; parameterized types (`Collection[T]`),
  polymorphic messages with inference, subtyping **and** matching, `Self` types, union
  types, `BottomType`, declared variance. Evolved from structural typing + "brands" to
  **declaration-based subtyping** (structural didn't express programmer intent).
- → *Rust*: **out of VM scope entirely.** If MACVM ever wants the optional type layer
  it belongs in the IDE/toolchain over the image, not the runtime. Lasting lesson:
  keep the runtime dynamically typed with concrete type feedback for optimization.

### 3.7 Self vs Strongtalk — the decision table for MACVM
| Concern | Self | Strongtalk | MACVM choice |
|---|---|---|---|
| Instance model | prototypes + **maps** | **classes + klassOop/Klass** | **Strongtalk** — fixed layouts, tractable customization, no map-splitting |
| Object access | **object-table** indirection | **direct tagged pointers** | **Strongtalk** — one less indirection; pay back via compacting old gen |
| Header | map pointer | **2 words: mark + klass** | **Strongtalk**, widened on 64-bit (stop punning age/size/hash) |
| GC | copying + object-table compaction | **gen scavenge + mark-sweep + 512B cards** | **Strongtalk** spaces/barrier; modernize mark-sweep (explicit worklist), 64-bit mark word |
| Dispatch opt | type feedback, PICs, deopt (origin) | **same, on classes** | **Strongtalk** — same power, class model simpler |
| Interpreter | generated threaded | generated threaded + self-modifying send opcodes | Keep IC *states*; Rust: side-table cache + `match`/tail-call dispatch |
| Codegen/reloc | `addrDesc` stream | **dual-stream CodeBuffer + delta-packed relocInfo + oop_type** | **Strongtalk**, re-encoded for arm64 (literal pools/`adrp`, new reloc kinds) — aligns with JASM §4 |
| Type system | (none) | optional, pluggable, image-only | Out of runtime scope; toolchain only |

**Porting hotspots are all 32-bit/x86 encodings, not architecture:** 2-bit tags at
±2^29 smi, 32-bit mark-word packing (esp. age/size overloading), `_jump_ebx`
threaded dispatch, `call_end=addr+4`, single-immediate embedded oops, x87 floats.
Each has a clean 64-bit/arm64 replacement noted above.

---

## 4. Codegen backend — JASM AArch64 encoder + macOS JIT

**Bottom line:** the JASM AArch64 encoder is **pure Rust** (crate `wfasm`, at
`../JASM/rust`), not C/C++. The "FFI vs. reimplement" question in `DESIGN.md` §4
therefore collapses into a third, better option: **vendor the relevant modules
into MACVM as Rust source** (or depend on `wfasm` as a path crate). No ABI
boundary, no bindgen, no clean-room re-derivation of the highest-risk code.

### 4.1 The encoder (`../JASM/rust/src/a64/`)

- `parse.rs` (~670 lines) — text → `Line`/`Operand`; `parse_line(&str)`. Rich
  operand model (X/W/B/H/S/D/Q register views, NEON `Arr`/`ElemSize`, addressing
  modes). MACVM can **skip this** and go structured (see 4.4).
- `encode.rs` (~2,430 lines) — the instruction encoder. Structured entry point:
  **`encode(mnemonic: &str, ops: &[Operand]) -> Result<Encoded>`** (`encode.rs:99`),
  returning `Encoded { bytes: Vec<u8>, fixups: Vec<Fixup> }`. Every base opcode
  derived from `llvm-mc -triple=aarch64-apple-darwin --show-encoding`.
- `mod.rs` (~907 lines) — driver: `assemble(text) -> EncodedModule`, `A64Encoder`.

**Coverage** — effectively the full practical Apple-Silicon integer/FP/NEON ISA,
all byte-verified: integer ALU (add/sub/logical/mul/madd/div/shift/bitfield/
extend/bitmask-imm/csel), full load/store (all sizes, pre/post/reg-offset,
ldp/stp, ldur/stur), branches, adrp/add PC-relative, scalar FP, comprehensive
NEON, system (nop/dmb/msr/mrs), atomics (ldxr/stxr + LSE), CRC32, crypto,
fixed-point fcvt. Documented gaps are low-value for a VM JIT: single-structure
lane ld/st, SVE/SME, `adr` (±1 MB), `ldr =imm` literal pool.

**Verification** — gated byte-identical against LLVM-MC (`src/oracle.rs`, behind
the `llvm` feature) AND frozen into a replayable corpus
(`src/difftest/aarch64.rs` + `corpus/aarch64.tsv`, ~1,181 forms) that runs with
**no LLVM installed**. MACVM inherits this gate for free.

### 4.2 Relocations (`../JASM/rust/src/backend.rs`)

- Reloc kinds: `Branch26` (b/bl ±128 MB), `AdrpPage21`, `AddPageOff12`, `Abs64`
  (`backend.rs:38-64`). **Bit-packed within the 32-bit word**, not contiguous LE
  fields — the patcher masks bits in.
- Encoder-internal `FixupKind::{Branch26, Branch19, AdrpPage21, AddPageOff12}`.
  `Branch19` is **relaxable**: out-of-range conditional branches invert-and-`b`,
  a deliberate extension beyond LLVM-MC (which errors).

### 4.3 macOS arm64 JIT loader — reuse verbatim (`../JASM/rust/src/native_macos.rs`)

Struct **`MacJit`** implements the complete Apple-Silicon JIT dance:

- **`mmap(MAP_JIT)` RWX once** — `PROT_READ|WRITE|EXEC`,
  `MAP_PRIVATE|MAP_ANON|MAP_JIT` (`MAP_JIT = 0x0800`). Page size 16 KiB.
- **`pthread_jit_write_protect_np`** per-thread toggle — writable (`0`) during
  emission, exec (`1`) at finalize. The region stays RWX; the toggle is
  per-thread (NOT `mprotect`).
- **`sys_icache_invalidate`** after writes, before execute.
- **Reloc patching + far-call veneers** in `src/relocpatch.rs`:
  `patch_relocs(...)` does the bit-insertion and appends a `movz/movk x16…; br x16`
  absolute veneer (`VENEER_LEN = 20`) for targets beyond ±128 MB (e.g. host
  callbacks via dlsym).

**Critical de-risking finding:** `MAP_JIT` + toggle + icache work in a plain
`cargo test` binary with **no codesign and no entitlements** (ad-hoc,
non-hardened). `com.apple.security.cs.allow-jit` is only needed for *distributed
hardened-runtime* binaries. MACVM can develop the JIT with zero signing ceremony.
End-to-end tests in the file JIT-run arithmetic, an internal `bl`, and a host
`extern "C"` callback on this machine.

There is also a full AOT Mach-O writer (`src/macho.rs`) that emits a runnable,
in-process ad-hoc-signed `MH_EXECUTE` with no `ld`/`codesign` shell-out —
relevant later if MACVM wants to snapshot compiled images.

### 4.4 How MACVM should consume it

- **Vendor the self-contained slice** (~5,500 lines, dependency-light — hot path
  is essentially `anyhow` + std):
  - `src/a64/{parse.rs, encode.rs, mod.rs}` — the encoder (~4,000 lines)
  - `src/backend.rs` — the `Encoder`/`Loader` traits + `EncodedModule`/`Reloc`
    data contract (~129 lines)
  - `src/native_macos.rs` + `src/relocpatch.rs` — JIT loader + patcher (~520)
  - optionally `src/oracle.rs` + `src/difftest/aarch64.rs` + `corpus/aarch64.tsv`
    to carry the verification gate along
- **Call `encode::encode(mnemonic, &[Operand])` directly** from the JIT — skip
  the text/macro front-end; the compiler emits structured operands from IR, not
  a formatted string that gets re-parsed. `backend.rs` itself anticipates a
  structured instruction stream.
- **Threading caveat:** `MacJit` is single-region, single-thread-finalize (the
  write-protect toggle is per-thread). A VM with background compilation threads
  must revisit the region/threading model, but the primitives are exactly right.

**Impact on `DESIGN.md` §4:** the JASM-reuse option is now strongly favored —
fast startup, full control, an already-verified encoder, and a proven macOS JIT
loader, all in the implementation language. This maps cleanly onto the
`Assembler` trait: a `JasmAssembler` backend wraps `encode` + `MacJit`.

### 4.5 Cocoa / objc bridge (for later macOS integration)

MacModula2's runtime bridge is `../MacModula2/src/newm2-runtime/src/objc.rs`
(~1,701 lines): resolves `objc_msgSend`/`objc_getClass`/`sel_registerName` via a
`dlsym_default` resolver, with autorelease-pool management and `extern "C-unwind"`
entry points. This is the pattern MACVM mirrors when it wants Cocoa integration;
MACVM would reuse the resolver/msgSend approach and supply its own high-level
binding layer.

---

## 5. MacNCL GC — reuse assessment

MacNCL (`../MacNCL`, sibling Rust Lisp system) delegates its GC to the external
crate **`newgc-core`** (repo `github.com/albanread/GC-.git`, pinned in
`MacNCL/src/ncl-runtime/Cargo.toml`). Assessed against SPEC §2/§7.

### 5.1 What it is
A **page-based, conservatively-pinning, cohort-promoting mark-evacuate**
generational GC (SBCL-gencgc lineage, not Strongtalk lineage): one 2 GiB
reservation carved into 64 KiB pages whose generation is a *per-page property*;
Cheney-style evacuation to fresh pages (no slide compaction anywhere);
promotion is whole-cohort every N cycles (no per-object age); roots are a
precise explicit root stack **plus load-bearing conservative native-stack
scanning with object pinning** (per-page pin bytes, start-bit bitmaps,
interior-pointer gates, pinned-page generation flips); 1-word self-describing
headers (size in header, no class chase); 512-byte lock-free card table
(dirty=1, rebuilt from heap walk each cycle).

### 5.2 Verdict: engine does NOT transfer — lessons + scaffolding do
Five structural mismatches, each disqualifying for direct reuse:
1. **Conservative pinning woven through the engine** — MACVM is precise-only;
   the pin plumbing is a large correctness-critical subsystem we'd maintain
   but never use (and it's where MacNCL's bugs lived).
2. **Wrong generational geometry** — page/cohort vs. MACVM's contiguous
   eden+survivors with 7-bit per-object age and adaptive tenuring.
3. **No slide compactor** — MACVM full GC is mark + slide compact.
4. **Header contract mismatch** — newgc sizes objects from the header alone;
   MACVM `Slots` sizing chases a (possibly forwarded) klass oop.
5. **No single is_old boundary** possible in a page heap.
(newgc-core's own README: "Experimental — do not depend on this.")

**What transfers:** the `Backing` mmap reservation abstraction
(reserve PROT_NONE / commit / decommit, idempotent commit path — exactly
SPEC §7.1's mechanism, proven on this platform); the lock-free `CardTable`
(512-byte cards — flip dirty polarity to 0); `ncl:stack_map.rs`'s
PC→live-slot table shapes (the oop-map skeleton for tier 1); the
`GcStallError` structured-failure pattern; the categorized+stochastic test
corpus *shape* (categories and oracles, not code).

### 5.3 Lessons imported into S7/S8 (from GC_LESSONS.md + bughunt docs)
Full 15-lesson list lives in `sprints/sprint_s07_detail.md`; headlines:
- Unit tests test GC *mechanics*, not *semantics* — MacNCL had 312 passing
  tests "and it didn't work on `(+ 1 2)`". The smallest meaningful test is a
  guest-language workload through the production allocator with an **exact**
  result oracle (the canonical list-walk reproducer: sum == 5000000 exactly
  under a shrunken eden).
- Every bug was *disagreement between separately-correct data structures* —
  ship a debug cross-check pass (card table vs offset table vs marks vs
  handles) run at every GC entry, plus a single-oop trace hook
  (`MACVM_DBG_OOP`) and a frozen invariants list *before* bring-up.
- Cascaded collection phases must not wipe each other's per-cycle state (the
  nastiest MacNCL bug: per-sub-evac pin clearing during a cascading cycle).
- Rust containers holding raw oops are invisible roots — MacNCL's history is
  the evidence for enforcing Handle discipline + GC-stress from day one.
- Never assume recycled memory is zeroed; define the fill discipline (nil).
- Scavenger OOM must be a designed cascade (survivor overflow → direct
  promotion → old growth → full GC), not a reserve constant.
- When survivors look too big, measure marked bytes vs expected working set
  before touching policy — the problem is almost always in the roots.
