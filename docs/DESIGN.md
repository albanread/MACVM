# MACVM — High-Level Design

Living document. Records the architecture and the decisions behind it. Grounded
in source-level analysis of the two reference VMs — see
[`reference-vm-analysis.md`](reference-vm-analysis.md) for the file-anchored
evidence, and [`arm64.md`](arm64.md) for the Apple-Silicon-specific machinery.

MACVM is a **research adaptive-optimizing virtual machine** in the
Self → Strongtalk → HotSpot lineage, written in **Rust** for **macOS on Apple
Silicon (arm64)**.

---

## 0. Decisions of record

| # | Decision | Status | Rationale |
|---|----------|--------|-----------|
| D1 | **Implementation language: Rust** | fixed | Safety at the meta level; `unsafe` confined to object-memory + codegen. §1 |
| D2 | **Object model: Strongtalk-style classes** (klass + direct pointers, no object table) | fixed | Both VMs share the adaptive JIT; Strongtalk showed Self's prototypes/maps + object table are its costliest choices. Classes give fixed layouts, tractable customization, one less indirection. §3 |
| D3 | **64-bit throughout** | fixed | Both originals are 32-bit; widening re-derives tags, mark word, smi range, floats — do it once, cleanly. §3.1 |
| D4 | **Modernize where flagged** | fixed | 64-bit mark word (no age/size punning), NaN-boxed/heap doubles (not Self's truncated-exponent floats), SSA-ish IR + linear-scan regalloc, immutable bytecode + side-table ICs. |
| D5 | **Adaptive compilation kept from Self/Strongtalk** | fixed | Inline caches → PICs → type feedback → optimizing recompilation → deoptimization. The crown jewels; present in both. §4 |
| D6 | **Codegen backend: vendor JASM's Rust AArch64 encoder** | leaning | Pure-Rust, LLVM-MC-verified, with a working `MAP_JIT` loader. Wrapped behind the `Assembler` trait. §5 |
| D7 | **Baseline tier: threaded-code interpreter** | fixed | Fast start, gathers IC feedback, serves as deopt target. §4.1 |
| — | Exact tag/header bit layout | **open** | §3.1 / `arm64.md` |
| — | NaN-boxing vs. heap-boxed doubles | **open** | `arm64.md` |
| — | GC: precise-through-JIT from day 1 vs. conservative bootstrap | **open** | §6 |

The language exposed by MACVM (Smalltalk-like, Self-like, or a new surface
syntax) is **not yet decided** and is deliberately downstream of the VM design.

---

## 1. Implementation language — Rust

`unsafe` is confined to two layers behind safe interfaces: the **object heap**
(raw tagged-word pointer arithmetic, moving GC) and the **codegen/JIT** (writing
and executing native memory). Everything else — the compiler front end, lookup,
IR, tooling — is safe Rust. We translate the *design* of the C++ originals, not
their code: a C++ `oop` becomes a `#[repr(transparent)]` newtype over a machine
word with `unsafe` accessors; C++ per-object vtables become a `&'static` klass
vtable or an enum-dispatched format tag (no snapshot vtable fix-up needed — a real
simplification over Strongtalk).

## 2. Lineage

- **Self** (`../self-repo`) — origin of type feedback, PICs, customization,
  splitting, dynamic deoptimization, and precise GC through JIT code.
- **Strongtalk** (`../strongtalk-repo`) — Self's optimizer on a class-based
  Smalltalk with **direct pointers and no object table**; a disciplined,
  production-grade C++ VM. MACVM takes Strongtalk's *representation* and both
  VMs' *optimization* machinery.

## 3. Object model — classes, tagged oops, direct pointers

Adopts Strongtalk's model (D2), widened to 64-bit (D3).

### 3.1 References and header
- An **oop** is a tagged 64-bit word (`#[repr(transparent)]` newtype). Low-bit
  tag scheme, provisionally: `…0` small integer (smi, tag 0 so ALU add/sub need
  no untagging), `…1` heap object. Exact bits are open (see `arm64.md`; arm64's
  **top-byte-ignore** may also carry metadata).
- Every heap object has a **2-word header**: `[mark][klass]`.
  - **mark word** — identity hash, GC/age bits, forwarding/lock state. On 64-bit
    we give these *explicit* fields and **do not** pun age with object size the
    way 32-bit Strongtalk does (D4).
  - **klass** — a *direct pointer* to the class object (itself a heap object, an
    instance of its metaclass).
- The **class** (`klass`) holds the layout descriptor, method dictionary,
  superclass link, and format tag (the VM-level type). It plays the role Self's
  *map* plays for the VM, but the language model is classes/metaclasses, not
  prototypes.

### 3.2 No object table
Confirmed the single most important representational endorsement from the
analysis: **direct tagged pointers, no GC indirection table.** Field access pays
no indirection; the cost is repaid by rewriting interior pointers during the
compacting major GC (§6). Self's classic object table is explicitly *not* copied.

### 3.3 Immediates
- **smi**: tagged small integer, 62-bit range on 64-bit.
- **floats**: **not** Self's truncated-6-bit-exponent immediate hack (D4). Choose
  between NaN-boxing (fast immediates, precision-preserving on 64-bit) and
  heap-boxed doubles; decision recorded in `arm64.md`.

## 4. Execution model — two tiers + adaptive recompilation

### 4.1 Baseline: threaded-code interpreter
Fast to start; runs everything first. Carries a **per-send-site inline cache**
(monomorphic → polymorphic → megamorphic), the same states as Strongtalk's
self-modifying send bytecodes — but MACVM keeps **bytecode immutable/shareable**
and stores the cache in a **side table** keyed by send site (D4), so we get the
IC states without self-modifying code. The interpreter is also the
**deoptimization target**.

### 4.2 Optimizing tier
Hot methods (invocation/loop counters) are recompiled by the optimizing compiler
using **type feedback** read back from the inline caches/PICs:

1. Read observed (class, method, count) tuples from the site's PIC.
2. Predict receiver types; where a type is known, **inline** the callee within a
   cost budget.
3. **Customize** per receiver class (monomorphizes `self`-sends).
4. **Split** at type-merge points so each typed path inlines instead of forcing a
   polymorphic send.
5. Emit **uncommon branches** (traps) for unlikely paths — compiled lazily on
   first real use.

IR/regalloc are **modernized** (D4): an SSA-ish IR and a linear-scan allocator,
rather than Strongtalk's non-SSA def-use graph and usage-count allocator.

### 4.3 Recompilation policy
Counters trip at a safepoint; the policy walks the frame stack and often
recompiles the **caller** (so a hot callee gets inlined). New optimized code
inherits the old site's caches to preserve type info. Effectiveness checks
disable counting on sites that don't benefit, to avoid thrash.

## 5. Codegen — abstract backend, JASM-leaning

The optimizing compiler emits through the backend-neutral `Assembler` trait
([`src/compiler/assembler.rs`](../src/compiler/assembler.rs)); the concrete
backend is feature-selected. **Leading choice (D6):** vendor JASM's pure-Rust,
LLVM-MC-verified AArch64 encoder (`wfasm`) as a `JasmAssembler` and reuse its
`MacJit` loader. Alternatives (LLVM via `inkwell`, interpreter-only) stay behind
the same trait. Details and the vendor plan: `reference-vm-analysis.md` §4.

The trait must expose what the analysis showed both VMs' assemblers expose (see
`reference-vm-analysis.md` §2.6, §3.5): a code buffer **plus a parallel relocation
stream**, labels with forward-reference resolution, and reloc kinds that
distinguish **embedded oops** (for GC), **inline-cache call sites** (for patching),
runtime calls, and internal words. On arm64 a 64-bit oop cannot be one immediate —
it is materialized via `adrp`/`add` or `movz`/`movk`, each needing its own reloc
kind (which JASM already has: `Branch26`, `AdrpPage21`, `AddPageOff12`, `Abs64`).

## 6. Memory & GC

Strongtalk's scheme, modernized:
- **Generations**: eden + two survivor spaces (bump-pointer, Cheney scavenge with
  **adaptive tenuring**), a compacting old generation, and a separate code cache
  (movable ⇒ relocations).
- **Write barrier**: card marking — a single `strb wzr` to `card_base + (addr >>
  shift)` (dirty = 0). Cheap; ports near-verbatim.
- **Major GC**: mark + compact with a *modern explicit worklist* (not
  Strongtalk's pointer-reversal marking), 64-bit mark word (no size/age punning).
- **Precise GC through JIT code (the hard part)**: compiled frames must expose
  **oop maps / stack maps** at every GC-safe point, emitted by the register
  allocator — plus derived/interior-pointer handling and frame chaining so the
  collector can find and relocate oops in optimized frames. This is the single
  largest porting cost in the memory system (Self analysis §1.5). Open decision:
  build this from day one, or bootstrap with a conservative stack scan and add
  precision before the moving collector ships.

## 7. macOS integration (later)

Reuse the `objc_msgSend`/Cocoa bridge pattern proven in MacModula2
(`../MacModula2/src/newm2-runtime/src/objc.rs`) so MACVM objects can eventually
send messages to real Cocoa objects. Out of scope until the core VM runs.

## 8. Type system — explicitly out of VM scope

Strongtalk's celebrated optional/pluggable type system lives **entirely in the
image/toolchain and never touches the VM** (`reference-vm-analysis.md` §3.6). MACVM
keeps the runtime **dynamically typed** and optimizes purely on **concrete** type
feedback. Any optional static type layer would belong in the IDE, added later.

## 9. Provenance tracking

We translate designs, not code; there is no line-by-line C++ port. Where a file
*does* adapt a specific reference algorithm, note the source (`self-repo` /
`strongtalk-repo` path) and what changed at the top of the file. Vendored JASM
Rust modules retain their original headers. New code carries the MACVM `LICENSE`.

## 10. Rough roadmap (VM-first)

1. Object model + heap allocation (bump-pointer eden), no GC yet.
2. Bytecode + threaded interpreter; classes, method dictionaries, sends via
   side-table inline caches.
3. Generational scavenge + card-marking write barrier.
4. `Assembler` trait + `JasmAssembler` (vendor `wfasm`); emit a trivial compiled
   method; `MacJit` execution.
5. Compiled inline caches + PICs; type feedback plumbing.
6. Optimizing compiler: IR, inlining, customization; oop maps for precise GC.
7. Deoptimization + OSR (the arm64 frame-copy/continuation glue Self never had
   outside SPARC).
8. Compacting major GC.

## Open questions log

- [ ] Exact tag scheme / 64-bit mark-word field layout (§3.1, `arm64.md`).
- [ ] NaN-boxing vs. heap-boxed doubles (§3.3, `arm64.md`).
- [ ] Precise-GC-through-JIT from day 1 vs. conservative bootstrap (§6).
- [ ] Bytecode set definition (baseline tier).
- [ ] Bootstrap: hand-written world vs. imported from a reference image.
- [ ] Confirm D6 (JASM vendor) after a spike wrapping `encode` behind `Assembler`.
