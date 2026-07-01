# Sprint S9 — Vendor JASM + code cache spike

Objective: MACVM emits, patches, and executes real AArch64 machine code with the
same trust level JASM already earned (frozen LLVM-MC corpus). Delivers the
vendored `wfasm` slice, the revised structured `Assembler` trait, the code
cache with segment allocation over one `MacJit` region, literal-pool emission
with Oop reloc records, and the W^X publish protocol as a Rust RAII guard.
Implements SPEC §9 (code cache), the emit substrate of SPEC §8.3/§8.5, and
`arm64.md` §2/§4. Independent of the interpreter — can land any time after S0.

## Prerequisites

- S0: `src/oops/` with `Oop` newtype and tag constants in `src/oops/layout.rs`
  (only `Oop`'s existence is needed; S9 never dereferences one).
- The JASM repository checked out at `/Users/oberon/claudeprojects/JASM`
  (vendoring source, commit `5ac1b1b` at time of writing — record the actual
  commit you vendor from).
- No interpreter/GC dependency. S9 builds even if S1–S8 are unfinished, but the
  crate must compile as a whole.

## Deliverables

- `src/vendor/wfasm/` — vendored encoder + JIT loader + reloc patcher +
  frozen corpus test (file list below), with provenance headers and license.
- `src/compiler/assembler.rs` — **rewritten**: the structured `Assembler`
  trait (full definition below) replacing the S0-era sketch, plus operand
  helper constructors.
- `src/compiler/jasm_assembler.rs` — `JasmAssembler`, the trait's only
  implementation, over `vendor::wfasm::a64::encode::encode`.
- `src/codecache/mod.rs` — `CodeCache` (one `MacJit` region, bump + free
  list), `CodeHandle`, `JitWriteGuard`, `patch_branch26`, publish protocol.
- `tests/it_codecache.rs` — smoke tests (see `tests_s09.md`).

## Design

### D1. Vendoring procedure — exact file list

Copy these files **verbatim first, then apply the marked edits**. Source root:
`/Users/oberon/claudeprojects/JASM/rust`. Destination root:
`/Users/oberon/claudeprojects/MACVM`.

| # | Source | Destination | Lines | Edits |
|---|--------|-------------|-------|-------|
| 1 | `src/a64/parse.rs` | `src/vendor/wfasm/a64/parse.rs` | ~670 | header only |
| 2 | `src/a64/encode.rs` | `src/vendor/wfasm/a64/encode.rs` | ~2430 | header; fix `use super::parse` (already relative — no change) |
| 3 | `src/a64/mod.rs` | `src/vendor/wfasm/a64/mod.rs` | ~907 | header; `use crate::backend::…` → `use crate::vendor::wfasm::backend::…`; delete the `#[cfg(test)]` module that imports `crate::oracle::LlvmMcEncoder` (oracle is NOT vendored) |
| 4 | `src/backend.rs` | `src/vendor/wfasm/backend.rs` | 129 | header only |
| 5 | `src/native_macos.rs` | `src/vendor/wfasm/native_macos.rs` | ~353 | header; `use crate::backend/relocpatch` → `crate::vendor::wfasm::…`; **MACVM additions** below |
| 6 | `src/relocpatch.rs` | `src/vendor/wfasm/relocpatch.rs` | ~168 | header; `use crate::backend::RelocKind` → vendored path |
| 7 | `src/difftest.rs` | `src/vendor/wfasm/corpus_replay.rs` | trimmed ~250 | see D1.3 |
| 8 | `corpus/aarch64.tsv` | `src/vendor/wfasm/corpus/aarch64.tsv` | 1181 forms | byte-identical copy — never regenerate in MACVM |
| 9 | `LICENSE` (repo root) | `src/vendor/wfasm/LICENSE-JASM` | — | verbatim (MIT, © 2026 alban read) |

New files to write:

- `src/vendor/mod.rs` — `pub mod wfasm;`
- `src/vendor/wfasm/mod.rs`:
  ```rust
  #![allow(dead_code)]        // MACVM uses a subset of the vendored surface
  pub mod a64;
  pub mod backend;
  #[cfg(target_os = "macos")] pub mod native_macos;
  pub mod relocpatch;
  #[cfg(test)] mod corpus_replay;
  ```
- `src/lib.rs` gains `pub mod vendor;` and `pub mod codecache;`.
  `src/vendor/` and `src/codecache/` are `unsafe`-permitted modules
  (CONVENTIONS §1); every other module keeps `#![forbid(unsafe_code)]`.

**D1.1 Provenance header (license retention).** Prepend to every vendored
file, ABOVE the original `//!` doc comment, keeping the original text intact:

```rust
// Vendored from JASM (wfasm), https://…/JASM  commit <HASH>
// Original path: rust/src/<path>.  License: MIT (see LICENSE-JASM in this
// directory; Copyright (c) 2026 alban read).
// Local modifications are marked with `// MACVM:` comments — keep the diff
// against upstream minimal so re-vendoring stays mechanical.
```

**D1.2 MacJit additions** (in `native_macos.rs`, each marked `// MACVM:`):

```rust
impl MacJit {
    // MACVM: expose the raw region so CodeCache can manage it directly.
    pub fn region_raw(&self) -> (*mut u8, usize) { (self.region, self.cap) }
}
// MACVM: re-exported W^X primitives for CodeCache / JitWriteGuard.
pub fn jit_write_protect(exec: bool) { unsafe { pthread_jit_write_protect_np(exec as i32) } }
pub fn icache_invalidate(start: *const u8, len: usize) {
    unsafe { sys_icache_invalidate(start as *mut c_void, len) }
}
```

**D1.3 Corpus test relocation.** `corpus_replay.rs` is `difftest.rs` trimmed to
exactly: `Verdict`, `mask_relocs`, `compare`, `unhex`, `parse_corpus_line`,
`kind_str`, and the test `corpus_replay_aarch64_matches_golden` — with its
`include_str!` retargeted:

```rust
let corpus = include_str!("corpus/aarch64.tsv");
```

Delete everything else (x86 model, `IsaModel`/`Form`, `diff`/`diff_all`/
`diff_model`, `record_corpus`, the x86 replay test, anything referencing
`crate::rasm` or `crate::oracle`). Corpus regeneration stays upstream in JASM
(needs the LLVM oracle); MACVM only **replays**. The test must run under a
plain `cargo test` with **no LLVM installed** — verify by grepping the
vendored tree for `llvm`/`oracle` (zero hits outside comments).

### D2. Data structures

**D2.1 The revised `Assembler` trait** (`src/compiler/assembler.rs`, full
replacement of the existing sketch). The compiler emits *structured* operands —
no text is ever formatted and re-parsed.

```rust
pub use crate::vendor::wfasm::a64::parse::{Addr, Mem, Operand, Reg, RegClass};

/// Why a code word / pool word needs runtime attention. Recorded per site.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelocKind {
    /// 8-byte literal-pool word holding an Oop; GC updates it in place.
    Oop,
    /// As `Oop`, but the nmethod's customization-key klass: treated WEAKLY
    /// at full GC (SPEC §8.5; consumed in S12). Emitted from S11 prologues.
    KeyKlassOop,
    /// A patchable `bl` send site (compiled inline cache). S11.
    InlineCache,
    /// 8-byte pool word holding an absolute Rust/stub address (`ldr;blr`).
    RuntimeAddr,
    /// 8-byte pool word holding an address inside the code cache.
    InternalWord,
}

#[derive(Clone, Copy, Debug)]
pub struct Reloc { pub offset: u32, pub kind: RelocKind }
// For pool kinds `offset` is the byte offset of the 8-byte pool word within
// the blob; for InlineCache it is the offset of the `bl` instruction word.

#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub struct Label(pub u32);
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub struct LiteralId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cond { Eq, Ne, Hs, Lo, Mi, Pl, Vs, Vc, Hi, Ls, Ge, Lt, Gt, Le }

/// Finished, position-independent code + metadata, ready for CodeCache::publish.
pub struct CodeBlob {
    /// Instructions, then 8-byte-aligned literal pool. All label/literal
    /// fixups resolved; InlineCache sites still hold `bl .` placeholders.
    pub code: Vec<u8>,
    pub literal_off: u32,                 // start of the pool within `code`
    pub relocs: Vec<Reloc>,
    /// One human-readable line per emitted instruction (debug/test builds
    /// only; empty in release). Drives the S10 "disassembly" goldens.
    pub listing: Vec<String>,
}

pub trait Assembler {
    /// Current write position (bytes from blob start). Always 4-aligned.
    fn offset(&self) -> u32;

    /// Encode one instruction via the vendored structured encoder.
    /// Panics on encode failure — a mnemonic/operand mistake is a compiler
    /// bug, never a guest-visible condition (CONVENTIONS §4).
    fn emit(&mut self, mnemonic: &str, ops: &[Operand]);

    /// Emit a raw pre-encoded word (used for LDR-literal, see D4, and for
    /// `brk #imm` uncommon traps in S13).
    fn emit_u32(&mut self, word: u32);

    // ── labels: intra-blob control flow (branch fixups resolved in-house,
    //    NOT via encode()'s string-target Fixups) ─────────────────────────
    fn new_label(&mut self) -> Label;
    fn bind(&mut self, l: Label);                       // at current offset
    fn b(&mut self, l: Label);                          // Branch26
    fn b_cond(&mut self, c: Cond, l: Label);            // Branch19
    fn cbz(&mut self, r: Reg, l: Label);                // Branch19
    fn cbnz(&mut self, r: Reg, l: Label);               // Branch19
    fn tbz(&mut self, r: Reg, bit: u8, l: Label);       // Branch14
    fn tbnz(&mut self, r: Reg, bit: u8, l: Label);      // Branch14

    // ── literal pool ─────────────────────────────────────────────────────
    /// Intern an 8-byte pool constant; identical (value, kind) pairs with
    /// kind None/RuntimeAddr may be deduplicated, oop-kind entries may NOT
    /// (each reloc site is patched independently — dedup IS still safe for
    /// oops since all sites see the same word; dedup them too, one reloc).
    fn literal_u64(&mut self, v: u64, kind: Option<RelocKind>) -> LiteralId;
    /// `ldr <dst,x-reg>, <pc-relative literal>` (raw-encoded, D4).
    fn ldr_literal(&mut self, dst: Reg, lit: LiteralId);

    // ── patchable call sites ────────────────────────────────────────────
    /// Emit `bl .` (self-branch placeholder) + a reloc of `kind`
    /// (InlineCache). Returns the site offset. Patched after placement via
    /// `CodeCache::patch_branch26`. S11 wires the initial target.
    fn bl_patchable(&mut self, kind: RelocKind) -> u32;
    /// `ldr x16, <pool RuntimeAddr>; blr x16` — call an absolute address
    /// (Rust runtime fns live far outside ±128 MB; `arm64.md` §3, x16 = IP0).
    fn call_far(&mut self, target: LiteralId);

    /// Resolve every pending label/literal fixup, pad, append the pool,
    /// rebase pool relocs, and hand over the blob. The assembler is dead
    /// afterwards. Panics on any unbound label (compiler bug).
    fn finish(&mut self) -> CodeBlob;
}

// Operand helpers (free fns; keep call sites short):
pub fn x(n: u8) -> Operand;            // Operand::Reg(Reg{class:X, num:n, is_sp:false})
pub fn w(n: u8) -> Operand;
pub fn sp() -> Operand;                // Reg{class:X, num:31, is_sp:true}
pub fn xr(n: u8) -> Reg;               // bare Reg for cbz/tbz/ldr_literal
pub fn imm(v: i64) -> Operand;         // Operand::Imm
pub fn mem(base: u8, off: i64) -> Operand;      // [xN, #off]
pub fn mem_pre(base: u8, off: i64) -> Operand;  // [xN, #off]!
pub fn mem_post(base: u8, off: i64) -> Operand; // [xN], #off
```

**D2.2 `JasmAssembler`** (`src/compiler/jasm_assembler.rs`):

```rust
pub struct JasmAssembler {
    buf: Vec<u8>,
    labels: Vec<Option<u32>>,                  // Label -> bound offset
    pending: Vec<PendingBranch>,               // fixups awaiting bind/finish
    literals: Vec<(u64, Option<RelocKind>)>,   // pool, in insertion order
    lit_dedup: HashMap<(u64, u8), LiteralId>,
    lit_loads: Vec<(u32, LiteralId)>,          // ldr-literal sites to resolve
    relocs: Vec<Reloc>,
    listing: Vec<String>,                      // cfg!(debug_assertions) or test
}
struct PendingBranch { at: u32, label: Label, kind: BranchKind }
enum BranchKind { B26, B19, B14 }              // bit-field widths, see D3.2
```

`emit` calls `encode::encode(mnemonic, ops)`; the returned
`Encoded { bytes, fixups }` must have `fixups.is_empty()` (the compiler never
passes `Operand::Sym` — string-target fixups are the text pipeline's business;
`debug_assert!` this). Listing line format (pinned, goldens depend on it):
`"{offset:06x}  {word:08x}  {mnemonic} {ops:?}"`.

**D2.3 CodeCache** (`src/codecache/mod.rs`):

```rust
pub struct CodeCache {
    _jit: MacJit,            // owns the mmap; kept alive, otherwise unused
    base: *mut u8,
    cap: usize,
    top: usize,                              // bump pointer (16-aligned)
    free: BTreeMap<usize, usize>,            // offset -> len, address-ordered
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CodeHandle { pub base: *const u8, pub len: usize }

impl CodeCache {
    /// One MAP_JIT region for the VM's lifetime. Default 64 MiB (SPEC §9,
    /// tunable). Leaves the thread in EXEC mode (MacJit::with_capacity
    /// returns in write mode — flip it back before returning).
    pub fn new(cap: usize) -> anyhow::Result<CodeCache>;

    /// 16-byte-aligned first-fit from the free list, else bump. None = full
    /// (caller policy: S10 stops compiling and logs under MACVM_TRACE=jit).
    pub fn alloc(&mut self, len: usize) -> Option<CodeHandle>;

    /// Return a segment to the free list, coalescing with adjacent entries.
    /// Caller (CodeTable, S12) guarantees no frame references it.
    pub fn free(&mut self, h: CodeHandle);

    /// Copy a finished blob into `h` under a write guard, rebase nothing
    /// (blob is position-independent; pool loads are pc-relative), flush
    /// icache via the guard's drop. Returns the entry address.
    pub fn publish(&mut self, h: CodeHandle, blob: &CodeBlob) -> *const u8;

    pub fn contains(&self, addr: u64) -> bool;

    /// Patch the Branch26 field of the `bl` at `site` to reach `target`.
    /// If out of ±128 MB range: allocate a 20-byte movz/movk-x16-br veneer
    /// (relocpatch::abs_veneer) via self.alloc and aim the bl at it.
    /// The single u32 store happens under a JitWriteGuard covering the word.
    pub fn patch_branch26(&mut self, site: *const u8, target: u64);

    /// Rewrite the 8-byte pool word at `addr` (GC path, S12; unit-tested
    /// here). Data word: needs the write guard, NOT an icache concern —
    /// pool words are read by `ldr` through the D-side.
    pub fn patch_pool_word(&mut self, addr: *mut u64, value: u64);
}
```

**D2.4 `JitWriteGuard`** — the publish/patch protocol as RAII
(`src/codecache/guard.rs`):

```rust
/// While alive: this THREAD may write the MAP_JIT region and must not
/// execute from it. Drop: back to exec mode + sys_icache_invalidate over
/// the recorded ranges. SPEC §9's per-publish protocol, enforced by type.
pub struct JitWriteGuard { ranges: SmallVec<[(usize, usize); 2]> }

impl JitWriteGuard {
    pub fn new() -> JitWriteGuard;                 // jit_write_protect(false)
    pub fn note(&mut self, start: *const u8, len: usize);  // range to flush
}
impl Drop for JitWriteGuard {
    fn drop(&mut self) {
        jit_write_protect(true);                   // exec mode FIRST
        for (s, l) in &self.ranges { icache_invalidate(*s as *const u8, *l); }
    }
}
```

Nesting: forbidden in v1 — a thread-local `GUARD_DEPTH: Cell<u32>` asserts
depth ≤ 1 in debug builds (a nested guard's drop would flip to exec while the
outer guard still writes). Single VM thread (SPEC §0) makes this sufficient.

### D3. Algorithms

**D3.1 `CodeCache::alloc`.** Round `len` up to 16. Scan `free` in address
order for the first block with `blk_len >= len`; split (remainder ≥ 32 stays
on the list, else absorbed into the allocation — no crumbs). Otherwise bump
`top`; fail if `top + len > cap`. `free(h)`: insert `(off, len)`; if the
previous entry ends at `off`, merge; if an entry starts at `off + len`, merge;
if the merged block abuts `top`, retract `top` instead of listing it.

**D3.2 Branch fixup bit-insertion** (assembler-internal; same math as
`relocpatch.rs` — reuse its formulas, cite it in comments):

| kind | field | insertion |
|------|-------|-----------|
| B26 (`b`,`bl`) | bits [25:0] | `word |= (((target−site) >> 2) as u32) & 0x03FF_FFFF` |
| B19 (`b.cond`,`cbz/cbnz`,LDR-lit) | bits [23:5] | `word |= ((((target−site) >> 2) as u32) & 0x7FFFF) << 5` |
| B14 (`tbz/tbnz`) | bits [18:5] | `word |= ((((target−site) >> 2) as u32) & 0x3FFF) << 5` |

Placeholder emission: encode the instruction with displacement 0 via
`encode()` where possible (`b`/`b.cond` take `Operand::Sym` — do NOT use it;
instead hand-build placeholders, see pitfall P6): emit the base opcode word
directly (`b` = `0x14000000`, `bl` = `0x94000000`, `b.cond` =
`0x54000000 | cond`, `cbz x` = `0xB4000000 | rt`, `cbnz x` = `0xB5000000 | rt`,
`tbz` = `0x36000000 | b40<<19 | (bit>>5)<<31? — use B6/T encodings from the
ARM ARM; keep these six constants in one `mod raw` with a unit test comparing
each against `encode()` output for a resolvable form where one exists).
On `bind`/`finish`, patch pending sites in `buf` (plain Vec — no W^X involved
yet). Range asserts: B19 ±1 MiB, B14 ±32 KiB — a violation is a compiler bug
(blobs are ≪ 1 MiB); `panic!` with the offsets.

**D3.3 Literal pool layout at `finish`.** Pad `buf` to 8 bytes;
`literal_off = buf.len()`; append pool words in insertion order; for each
`lit_loads` entry patch the LDR-literal imm19 = `(pool_word_off − site) >> 2`
(B19 field position); for each pool entry with `Some(kind)` push
`Reloc { offset: literal_off + 8*i, kind }`. Pool is at the END so hot code
prefetches cleanly and the GC's writes never touch instruction bytes
(`arm64.md` §4.1 — the reason literal pools were chosen over adrp/add or
movz/movk for mutable constants).

**D3.4 LDR-literal raw encoding** (the vendored encoder does not support it —
its documented gap): 64-bit form `0x5800_0000 | (imm19 << 5) | rt`, imm19 =
word-offset/4… precisely `((target − pc) >> 2) & 0x7FFFF` at bits [23:5], rt =
destination Xn. Emit with displacement 0 at `ldr_literal`, resolve at
`finish`. Unit-test the constant against a known vector (D-test T9-U7).

**D3.5 Publish protocol** (`CodeCache::publish`):
1. `let mut g = JitWriteGuard::new(); g.note(h.base, h.len);`
2. `ptr::copy_nonoverlapping(blob.code.as_ptr(), h.base as *mut u8, blob.code.len())`
   (assert `blob.code.len() <= h.len`).
3. Drop `g` → exec mode + icache flush. Only after this may the address be
   published (stored in a CodeTable / IC — S10).

### D4. Layer boundaries

- `src/vendor/wfasm/**` is **read-only** except `// MACVM:`-marked lines.
  Nothing outside `src/compiler/assembler*.rs`, `src/codecache/` and the
  vendored tree itself may `use crate::vendor::…`.
- `src/codecache/` may not know about `Ir`, bytecode, or the interpreter. It
  deals in `CodeBlob`, `CodeHandle`, raw addresses.
- `src/compiler/` may not call `mmap`/`pthread_jit_write_protect_np` — all
  W^X goes through `CodeCache`/`JitWriteGuard`.

## Implementation order

1. `src/vendor/` tree: copy files 1–6, 8, 9; write the two `mod.rs` files;
   apply path edits + provenance headers. `cargo build` green.
2. File 7 (`corpus_replay.rs`) trimmed; `cargo test corpus_replay` green —
   **1181 forms, no LLVM present** (this is the trust gate; do it before any
   original code).
3. Delete the old `src/compiler/assembler.rs` body; write the new trait +
   operand helpers (compiles with no implementor).
4. `JasmAssembler`: emit + labels + fixups (Vec-only, unit-testable without
   any JIT memory).
5. Literal pool + `ldr_literal` + `finish` + relocs + listing.
6. `CodeCache::new/alloc/free` + `JitWriteGuard` + `publish`.
7. `patch_branch26` + veneer fallback + `patch_pool_word`.
8. Integration smoke tests (`tests_s09.md`).

Each step compiles and its tests pass before the next begins.

## Pitfalls

- **P1 — corpus test must run WITHOUT llvm.** The vendored slice must not
  reference `crate::oracle`, the `llvm` cargo feature, or `llvm-mc`. The
  a64/mod.rs test module and difftest's oracle plumbing are the two places
  this leaks in — delete both. CI runs on machines with no LLVM toolchain.
- **P2 — `MacJit::with_capacity` leaves the thread WRITABLE.** It calls
  `pthread_jit_write_protect_np(0)` before returning. `CodeCache::new` must
  flip to exec (`jit_write_protect(true)`) so the steady state is
  "executable, not writable" and every write is guard-mediated.
- **P3 — the write toggle is PER-THREAD** (`arm64.md` §2). The guard flips
  only the calling thread. v1 is single-threaded (SPEC §0) so this is safe;
  the thread-local depth assert documents the assumption. Never publish from
  a `std::thread::spawn`'d test helper.
- **P4 — patching is a single aligned u32 store + icache flush.** A `bl`
  displacement patch must be one 32-bit store to a 4-aligned address (it is —
  all A64 instructions are), then `sys_icache_invalidate` over that word.
  Never memcpy a multi-instruction patch over live code (S11+ depends on this
  discipline).
- **P5 — adrp/add pairs must never be split by GC-time patching.** A moved
  oop rewritten into an `adrp`+`add` pair is two dependent instruction writes
  — un-atomic and needing an icache flush per GC. That is WHY embedded oops
  live in the literal pool (data words, `ldr`-loaded, D-side coherent, no
  flush). Do not "optimize" oop materialization into adrp/add or movz/movk.
- **P6 — do not use `encode()`'s `Operand::Sym` fixups.** They carry String
  targets resolved by JASM's two-pass text driver, which MACVM skips. All
  intra-blob branches go through the Label mechanism; `debug_assert!` that
  `Encoded::fixups` is empty on every `emit`.
- **P7 — biased field offsets encode as `ldur`.** SPEC §2.1 pre-biases heap
  offsets by −1 (`[xoop, #7]`), which is not a scaled-`ldr` multiple of 8; the
  vendored encoder silently selects the unscaled `ldur` form (range ±255).
  Fine here; S10 handles the >255 case (see S10 P4). Just don't be surprised
  when goldens show `ldur`.
- **P8 — 16 KiB pages.** `MacJit` rounds capacities to 0x4000. Don't assume
  4 KiB anywhere (`PAGE` is already correct in the vendored file).
- **P9 — icache flush ordering.** Exec-mode flip FIRST, then invalidate
  (matches `MacJit::finalize`). Both orders work on current hardware, but
  keep the vendored order so behavior matches the proven code path.
- **P10 — dedup pool words by (value, kind), not value alone.** An address
  that is both a plain `RuntimeAddr` and (later) a patched target must not
  share a word with an `Oop` entry — GC would rewrite the runtime address.

## Interfaces for later sprints

- S10: `Assembler` (all of it), `CodeBlob`, `CodeCache::{alloc, publish,
  contains}`, `Reloc`/`RelocKind`, listing format for goldens.
- S11: `bl_patchable`, `call_far`, `CodeCache::{patch_branch26, alloc, free}`,
  veneer fallback, `JitWriteGuard` for IC patching.
- S12: `Reloc` streams (`Oop`/`KeyKlassOop`) + `patch_pool_word` for GC
  rewriting; the "code cache never moves blocks" invariant (bump+free-list,
  no compaction — SPEC §9).
- S13: `emit_u32` for `brk #imm` uncommon traps.

## Out of scope

- nmethod struct, CodeTable, any dispatch (S10).
- Stub generation, IC state machine, PICs, veneer-exercising sends (S11 —
  S9 only ships the `patch_branch26` mechanism + its unit test).
- GC consuming Oop relocs (S12; S9 only records them and unit-tests
  `patch_pool_word`).
- Code-cache compaction (stretch, SPEC §9), background compilation
  (`arm64.md` §2 threading caveat — v1 is synchronous, SPEC §8.1).
