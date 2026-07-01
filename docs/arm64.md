# MACVM on Apple Silicon (arm64) — Machine-Level Design

The Apple-Silicon-specific half of the design. Where [`DESIGN.md`](DESIGN.md) is
architecture-neutral, this document pins down the machine: memory representation,
the JIT memory dance, calling conventions, code patching, and precise GC through
compiled frames. Evidence for the reference-VM mechanisms it modernizes is in
[`reference-vm-analysis.md`](reference-vm-analysis.md).

Target: **AArch64, macOS, Apple Silicon (M-series), little-endian, 16 KiB pages.**

---

## 1. Value representation on 64-bit AArch64

### 1.1 Tagged oops
A 64-bit word, low-bit tagged (Strongtalk scheme widened, `DESIGN.md` §3.1):

| Low bits | Kind | Notes |
|----------|------|-------|
| `xxx…0` | **smi** | tag 0 ⇒ `add`/`sub`/`cmp` work directly on tagged ints; 62-bit range if tag is 1 bit, 61-bit if 3-bit tag |
| `xxx…1` | **heap oop** | pointer, biased by the tag; recovered as `ptr = word - tag` |

Heap objects are 16-byte aligned (two 8-byte header words, and alignment aids
the allocator), so ≥3 low bits are free for tagging if we want more immediate
kinds (chars, `nil`/`true`/`false`). **Open:** 1-bit vs. 3-bit tag.

### 1.2 Top-Byte-Ignore (TBI)
AArch64 **TBI** makes the CPU ignore bits [63:56] of a pointer on load/store.
macOS enables TBI for userland data accesses. This is a genuine arm64 lever the
x86 originals never had: MACVM *may* stash a byte of metadata (e.g. a fast klass
hint or GC color) in the top byte of an oop and dereference it without masking.
**Caution:** TBI covers data accesses, **not** instruction fetch (branch targets
must be canonical), and pointer authentication (§5) also lives in the high bits —
so a TBI metadata scheme must be reconciled with PAC if we ever sign VM pointers.
**Decision deferred**; low-bit tagging is the baseline and does not depend on TBI.

### 1.3 Floats — NaN-boxing vs. heap doubles
Self's 32-bit immediate float truncates the exponent to 6 bits and loses range;
**we do not port it** (`DESIGN.md` D4). Two real options on 64-bit:

- **Heap-boxed doubles** — a `double` is a small heap object. Simple, precise,
  uniform with other oops; costs an allocation + indirection per boxed float.
  **Recommended starting point** — matches the class-based model and keeps the
  tag scheme trivial.
- **NaN-boxing** — encode all values inside the payload of IEEE-754 NaNs so a
  `double` is itself a value with no box. Fast float-heavy code, but constrains
  pointers to ~48 bits and complicates the smi/tag story. **Considered upgrade**
  if profiling shows float boxing dominates.

This is an isolated decision behind the `oops` accessors; either can be adopted
later without touching the compiler.

---

## 2. JIT memory — W^X on Apple Silicon (reuse JASM's `MacJit` verbatim)

Apple Silicon enforces that a page is not simultaneously writable and executable
*to a thread*. The working, verified sequence lives in
`../JASM/rust/src/native_macos.rs` (`reference-vm-analysis.md` §4.3) and MACVM
reuses it:

1. **Allocate once** with `mmap(PROT_READ|WRITE|EXEC, MAP_PRIVATE|MAP_ANON|
   MAP_JIT)`. `MAP_JIT = 0x0800`. Region stays RWX for its lifetime.
2. **Toggle per thread** with `pthread_jit_write_protect_np(0)` before writing
   code and `pthread_jit_write_protect_np(1)` before executing it. This is a
   fast per-thread mode switch, **not** `mprotect`.
3. **`sys_icache_invalidate(addr, len)`** after writing and before executing
   freshly emitted or patched code (D-cache→I-cache coherence; see §4.3).
4. Pages are **16 KiB** on Apple Silicon — size arenas accordingly.

**Codesigning / entitlements:** the `MAP_JIT` + toggle + icache path works in a
plain ad-hoc-signed dev binary with **no entitlements**. The
`com.apple.security.cs.allow-jit` entitlement is only needed for a *distributed
hardened-runtime* build. So day-to-day MACVM development needs **zero signing
ceremony** (a real de-risking finding from the JASM survey).

**Threading caveat:** `pthread_jit_write_protect_np` is per-thread and the region
is single. A VM with **background compilation threads** must design the
write/execute windows carefully (e.g. compile into a private RW mapping, publish
with a mode flip + icache invalidate, or use per-thread JIT regions). JASM's
`MacJit` is single-thread-finalize today; MACVM's concurrency model revisits this.

---

## 3. Calling convention & register assignment (AAPCS64)

MACVM native code follows **AAPCS64** at boundaries (VM↔native, native↔runtime,
native↔C). Apple's variations from the base AAPCS64 that matter:

- **Arguments are packed** on the stack (Apple does not round each stack arg up
  to 8 bytes the way the generic AAPCS64 does) — relevant only when we call
  variadic/C functions; internal MACVM calls define their own convention.
- `x18` is **reserved by the platform** — never use it.
- `x16`/`x17` are intra-procedure-call scratch (IP0/IP1) and are what veneers use
  (JASM's far-call veneer is `movz/movk x16…; br x16`).
- `x29` = FP, `x30` = LR, `sp` 16-byte aligned at public boundaries.

### Proposed VM register roles (internal convention, revisable)
Modeled on the reference interpreters' fixed roles (Strongtalk used
`eax`=TOS/`esi`=bcp/`ebp`=frame), mapped to callee-saved AArch64 registers so
they survive runtime calls:

| Reg | Role |
|-----|------|
| `x28` | current thread/VM state (`Memory`, allocation top, safepoint flag) |
| `x27` | receiver / TOS cache (interpreter) |
| `x26` | bytecode pointer (interpreter) |
| `x25` | method / frame descriptor |
| `x24` | heap allocation top (or fold into thread state) |
| `x29`/`x30`/`sp` | FP / LR / stack per AAPCS64 |

Compiled code uses the general allocatable set (`x0`–`x17` minus IP scratch) via
the linear-scan allocator; the callee-saved VM registers above are preserved
across compiled↔runtime transitions so the runtime (GC, lookup, deopt) can always
find VM state. Final assignment is fixed once the interpreter is written.

---

## 4. Compiled-code layout, relocations, and patching

Both reference VMs pair a code buffer with a **parallel relocation stream** so GC
and inline-cache patching can find the right words without decoding instructions
(`reference-vm-analysis.md` §2.6, §3.5). MACVM does the same; JASM's encoder
already emits exactly the AArch64 reloc kinds needed.

### 4.1 Embedding a 64-bit oop
x86 dropped an oop as a single 32-bit immediate; **AArch64 cannot**. An embedded
oop is materialized by either:
- **`adrp x, PAGE` + `add x, x, #OFF`** — PC-relative, ±4 GB page + 12-bit
  offset. Reloc kinds `AdrpPage21` + `AddPageOff12` (JASM has both).
- **`movz`/`movk` ×3–4** — build the constant in a register. Reloc `Abs64`.
- **literal pool** — `ldr x, =oop` loading from a nearby constant word; the pool
  word is the reloc site (simplest for the GC to update).

Each embedded-oop site gets an **`oop`-kind reloc record** so the scavenger and
compactor can find and rewrite it in place (§6). For the mutable-constant case a
literal pool is attractive: GC updates one aligned data word, no instruction
re-encoding.

### 4.2 Inline-cache call sites
A monomorphic send is a **`bl` to the callee's verified entry**, guarded by a
class check in the callee prologue. The call site carries an **`inline-cache`
reloc** so the runtime can:
- repoint it on a miss (`bl` displacement is `Branch26`, ±128 MB; beyond that,
  route via an `x16` veneer as JASM does), and
- promote it to a PIC stub (a generated sequence of class-compares).

Patching a `bl` displacement is a single aligned 32-bit store, then an icache
invalidate of that word (§4.3).

### 4.3 Instruction-cache coherence when patching
AArch64 has **separate I- and D-caches** and does not snoop them for
self-modifying code. After any code write or patch, MACVM must, for the affected
range: `dc cvau` (clean D-cache to point of unification) → `dsb ish` → `ic ivau`
(invalidate I-cache) → `dsb ish` → `isb`. On macOS this whole dance is wrapped by
**`sys_icache_invalidate(addr, len)`** — call it; do not hand-roll the barriers.
Every IC patch and every deopt patch ends with it.

### 4.4 nmethod structure (MACVM)
Per compiled method, mirroring the analysis:
`[ header | native code | reloc stream | scope descriptors | Pc→scope map | oop maps ]`
— header with entry offsets (verified vs. class-checked) and flags
(state/level/version); the reloc stream (§4.1–4.2); and the deopt/GC metadata
(§6, §7).

---

## 5. Pointer Authentication (PAC)

Apple Silicon supports **PAC** (signing pointers in the unused high bits, checked
on use). Implications for a JIT VM:

- MACVM-generated code and hand-managed return addresses are **not** automatically
  PAC-protected; we choose whether to sign. Baseline: **do not sign VM-internal
  pointers** — simpler, and our object pointers use low-bit/TBI tagging, not high
  bits.
- If MACVM later signs return addresses in synthesized frames (deopt/OSR build
  frames by hand), use `pacia`/`autia`/`retab` consistently, or the CPU faults on
  return. Deopt frame synthesis (§7) is exactly where this bites.
- Interop: when calling into system libraries built with PAC (arm64e), respect
  their signing at the boundary. macOS user code is generally arm64 (not arm64e)
  for third-party binaries today, so this is mostly a forward-looking concern.

**Baseline decision:** PAC off for VM-internal control flow; revisit if we target
arm64e or want the hardening.

---

## 6. Precise GC through compiled frames (the hardest arm64 piece)

The moving collector must find and relocate every oop, including those held in
**registers and stack slots of optimized frames** — MACVM is precisely collected
through JIT code, like Self (`reference-vm-analysis.md` §1.5).

- **Oop maps / stack maps.** At every **GC-safe point** (call sites, allocation
  points, backward branches), the register allocator emits a map: which
  registers and which stack slots currently hold oops. Stored in the nmethod
  (§4.4). The collector, walking a frame, consults the map for the current PC to
  enumerate live oops.
- **Embedded oops** in the code stream are found via the `oop` reloc records
  (§4.1) and updated in place (with an icache invalidate if an instruction, not a
  pool word, changed).
- **Derived / interior pointers.** If compiled code holds a pointer into the
  middle of an object (or into the code cache), the map must record it as
  *derived from* a base oop at a byte offset, so the collector relocates the base
  and re-derives the interior pointer. Budget for this — Self needed it.
- **Frame walking.** AArch64 frames chain through `x29` (FP); each saved FP points
  to the caller's frame record `[FP, LR]`. The collector walks this chain, and for
  each compiled frame maps PC→safepoint→oop map. Frame chaining metadata also lets
  the collector find all frames of a code object it wants to move.
- **Write barrier.** Card marking: `lsr xtmp, xobj, #card_shift` then `strb wzr,
  [xcards, xtmp]` (dirty = 0). One shift + one byte store on the store fast path.
  Value-conditional (only when storing a young pointer) as in Self, or
  unconditional-when-checked as in Strongtalk — MACVM starts with the cheaper
  unconditional-checked form and refines.

**Bootstrap option (open):** ship first with a **conservative** stack scan (treat
any word that looks like a heap pointer as a root, no moving) to get the
interpreter + allocator running, then add precise oop maps *before* enabling the
moving collector. This decouples "get it running" from "get GC moving," at the
cost of temporary non-moving allocation.

---

## 7. Deoptimization & OSR — the glue Self never shipped off-SPARC

Self's dynamic deoptimization and on-stack replacement were only fully
implemented on SPARC; i386/ppc had `fatal(...)` stubs (`reference-vm-analysis.md`
§2.5). MACVM must write the **AArch64** version from scratch. The pieces:

- **Scope descriptors + PC map + name descriptors** (emitted at compile time,
  §4.4): for any native PC, recover the chain of (possibly inlined) virtual
  scopes and, for each source variable, its **location** — an AArch64 register or
  a stack slot or a constant.
- **Deopt (optimized → interpreter).** On dependency invalidation or an uncommon
  trap: patch the frame's return so control reaches a **deopt trampoline**; the
  trampoline reads the scope/name descriptors for the trap PC, **materializes each
  virtual frame** as an interpreter activation (writing locals/expression-stack
  from the mapped registers/slots), and resumes the interpreter at the current
  bytecode. Must restore callee-saved VM registers (§3) and honor PAC (§5) if
  return addresses are signed.
- **OSR (interpreter → optimized, and optimized → newly-optimized).** Capture the
  source-level state from the running frame, build the target frame(s) by hand,
  fill in values from the descriptors, and continue at the mapped native PC/SP.
- **Uncommon traps.** Unlikely paths compile to a trap instruction (a `brk` /
  `udf` with an index, or a call to a runtime stub) that deoptimizes on first hit
  and, past a threshold, triggers recompilation-with-that-path.
- **Safepoints.** Deopt and GC both need threads at known-good points. Baseline: a
  polled safepoint — compiled code and interpreter check `x28`'s safepoint flag at
  method entry and loop back-edges and call into the runtime when set. (A
  page-protection safepoint — fault on a guard page — is a later optimization.)

This subsystem is the largest *original* engineering in MACVM (as opposed to
translated-from-reference), precisely because the originals left it unfinished
outside SPARC.

---

## 8. Summary — what is reuse vs. what is new

| Area | Source | MACVM work |
|------|--------|-----------|
| AArch64 instruction encoding | **reuse** JASM `wfasm` (verified) | wrap `encode()` behind `Assembler` |
| JIT memory / W^X / icache | **reuse** JASM `MacJit` | revisit for background-compile threads |
| Reloc kinds (adrp/add, movk, abs64, branch26) | **reuse** JASM | map to `Assembler::RelocKind` |
| Tagging, mark word, header | translate Strongtalk, widen to 64-bit | pick tag scheme, explicit 64-bit mark fields |
| Floats | modernize (drop Self hack) | NaN-box or heap-box decision |
| Card-marking write barrier | translate (both VMs) | 2-instruction arm64 fast path |
| Generational scavenge + compaction | translate Strongtalk | modern worklist marking |
| Inline caches / PICs / type feedback | translate (both VMs) | side-table ICs, arm64 PIC stubs |
| Optimizing compiler (inline/customize/split) | translate + modernize | SSA-ish IR, linear-scan regalloc |
| Oop maps for precise GC | translate Self concept | **new** arm64 map emission from regalloc |
| Deopt / OSR / safepoints | Self concept, **unfinished off-SPARC** | **new** arm64 frame-copy + continuation glue |
| Cocoa/objc bridge | pattern from MacModula2 | later |
