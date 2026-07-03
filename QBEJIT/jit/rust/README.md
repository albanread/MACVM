# qbejit — Rust port of the FasterBASIC QBE JIT

Rust port of `QBEJIT/jit/{c,zig}` (see `../README.md` for provenance and the
original Zig). Same architecture, same vendored C QBE-fork backend, driven
from Rust instead of Zig. Not wired into MACVM's own `Cargo.toml` — this is
an evaluation crate for `docs/DESIGN.md` §5 (D6), built and tested standalone.

This file covers provenance and module layout. For a usage-oriented guide to
what the crate can actually do — the text-free IR builder, oop-slot/IC-site
patching — see **[FEATURES.md](FEATURES.md)**.

## Why port it

MACVM is a Rust project; embedding a Zig dependency means a second toolchain,
a second build system to keep in sync, and no direct path to sharing types
(oop representations, calling-convention structs) between the VM and the
codegen backend without a second FFI hop. This port collapses that to one
language and one `cargo build`, at the cost of re-verifying everything the
Zig side had already proven — see "What's verified" below for exactly how
much of that re-verification has actually happened versus is still owed.

## Layout

- `csrc/rust_bridge.c` — the *only* C this crate adds *outside* `../c/`
  itself. QBE's parser calls `basic_exit()` on a fatal error, which in the
  original FasterBASIC runtime longjmps back to a setjmp() scope the caller
  establishes first. Without that, a malformed QBE IL string would `exit()`
  the entire host process (MACVM itself). `rust_qbe_protected_call()`
  establishes that scope in C (setjmp/longjmp never crosses the Rust frame
  that calls it — the same pattern `lua_pcall` uses) — see the file's module
  doc for the full reasoning. Every path into the QBE pipeline goes through
  it, both entry points (text and text-free, see `ir.rs` below) — nothing
  calls `qbe_compile_il_jit`/`qbe_ir_compile_jit` directly.
- `../c/ir_builder.{c,h}` — **new, lives alongside the vendored fork, not
  upstream QBE or FasterBASIC's own `qbe_bridge.c`/`jit_collect.c`**: the
  text-free IR construction API this crate's `ir.rs`/`ir_ffi.rs` drive. See
  `ir_builder.h`'s own module doc for the design (short version: QBE already
  exports `newtmp`/`newcon`/`intern`/`newblk` — the hard parts a from-scratch
  IR builder would need — this just replicates `parsefn()`'s setup protocol
  around them) and `../../REVIEW.md`'s "text-free interface" addendum for
  why it exists. Comes with exactly **one** deliberate, linkage-only patch to
  the vendored source: `typecheck()` in `parse.c` was `static`, now isn't (a
  declaration moved to `all.h`) — no behavior change to the text path, just
  reused validation. Documented at both edit sites.
- `build.rs` — two `cc::Build`s: one for `../c/*.c` (the vendored fork,
  warnings off — not ours to lint) plus `../c/ir_builder.c` and
  `csrc/rust_bridge.c` (our own C, warnings on) — generating `config.h` the
  way `../c`'s own `build_qbe.sh` does.
- `src/ffi.rs` — raw `extern "C"` surface, mirroring `jit_collect.h` /
  `qbe_bridge.h` byte-for-byte (`JitInst`, `JitCollector`, the instruction-kind
  constants).
- `src/compile.rs` — the only safe entry point: `compile_il_jit(il, target)`.
  Takes a process-wide lock (QBE's C globals aren't thread-safe — this
  enforces that instead of pushing the obligation onto callers), routes the
  call through the setjmp guard, and returns an RAII-wrapped instruction
  stream.
- `src/encoder.rs` — port of `arm64_encoder.zig`: the ~157 `emit*` functions
  QBE's arm64 backend actually produces (of ~300 defined upstream — the rest
  is unused breadth, deliberately not ported; see file for the exact list),
  pure `u32`-returning functions, no `unsafe`.
- `src/module.rs` — `JitModule`: code/data buffers plus every unresolved
  fixup kind (`BranchFixup`, `ExtCallEntry`, `LoadAddrReloc`, `DataSymRef`).
  Deliberately drops the Zig original's `source_map`/`comment_map`/
  `diagnostics` fields — debug/reporting conveniences, not part of the
  codegen-to-executable contract. Add them back if MACVM's own diagnostics
  need them.
- `src/encode.rs` — port of `jit_encode.zig`'s dispatch: `JitInst` →
  `encoder::emit_*` calls → `JitModule`. Also drops the Zig original's
  `dump*`/disassembly-report functions for the same reason.
- `src/memory.rs` — port of `jit_memory.zig`: `mmap`/`MAP_JIT` allocation,
  `pthread_jit_write_protect_np` W^X toggling, icache invalidation, and the
  in-place patch functions (`patch_bl_direct`, `patch_adrp_add`, ...) an
  inline-cache patcher would reuse directly.
- `src/linker.rs` — port of `jit_linker.zig`: ADRP+ADD data relocations,
  the external-call trampoline island, `dlsym`/jump-table symbol resolution.
- `src/patch.rs` — **new, not in the Zig original**: closes the reloc-kind
  gap `../REVIEW.md` flagged against `docs/DESIGN.md` §5. Oops route through
  a `data $__macvm_oop$name = { l 0 }` slot (a naming convention recognized
  purely on the Rust side — no `jit/c` changes) instead of a code immediate,
  so a moving-GC update (`update_oop_slot`) is one bounds-checked memory
  write to the always-RW data region, no W^X toggle, no code patch at all.
  `repatch_ic_site` is the actual code-patching case DESIGN.md asked for (a
  PIC redirecting a `BL`), built on `patch_bl_direct`/`patch_bl_to_trampoline`,
  which already existed. See the module's own doc comment for the full
  design rationale, and `../REVIEW.md`'s "Addendum 2" for how this was
  arrived at.
- `src/ir_ffi.rs` — raw `extern "C"` surface for `../c/ir_builder.h`, same
  role `ffi.rs` has for `jit_collect.h`: opaque `QbeFn*`/`QbeBlk*` handles, a
  `#[repr(C)]` `QbeRef`, and the curated `op::*`/`cls::*` opcode-index
  constants (kept in exact 1:1 order with `ir_builder.h`'s own enum — see
  that module's doc comment for why a plain `c_int` alias was chosen over an
  ~80-arm Rust enum).
- `src/ir.rs` — the safe wrapper: [`build_function_jit`] constructs and
  compiles exactly one function per call, via an `IrFunction` builder handed
  to a caller-supplied closure — no text, no `parse()`. Takes the *same*
  `COMPILE_LOCK` `compile.rs` does (both touch QBE's identical C globals —
  sharing one lock is load-bearing) and runs the whole closure plus the
  optimizer pipeline inside one `rust_qbe_protected_call` scope, so a
  malformed function aborts cleanly (proven in
  `tests/ir_builder.rs::malformed_function_aborts_cleanly_instead_of_crashing`
  — the process survives and can immediately compile something else) instead
  of taking the process down.

## Scope decisions (all inherited from `../REVIEW.md`'s "what to skip" call)

Dropped versus the Zig original, everywhere: `jit_capstone.zig` (disassembly
— needs vendoring Capstone as a C dependency, pure debug convenience),
source maps, comment maps, diagnostics collections, and the `dump*`/report
functions built around them. `jit_stubs.zig` (FasterBASIC's own 200+-entry
runtime jump table) isn't ported at all — it's application-specific; MACVM
would build its own `RuntimeContext` jump table mapping its own runtime
function names to addresses, using `linker::RuntimeContext` as the API to
build it against.

Only `arm64_apple` is tested. `build.rs` compiles all three QBE backends
(amd64/arm64/rv64) because `qbe_bridge.c`'s fixed 5-entry target table
requires all three `Target` structs to link — but only arm64_apple is what
MACVM needs or what this port's tests exercise.

`ir_builder.h`'s opcode table is a curated ~84-opcode subset (integer/float
arithmetic, all four class comparisons, memory access, type conversions,
stack allocation) — not QBE's full opcode space. Explicitly out of scope for
this first cut: aggregate (struct-by-value) parameters/arguments/returns
(`Oparc`/`Oargc`, the `Kc` class), vararg calls, NEON ops, and explicit Phi
node construction (mutable locals go through `alloc4`/`8` + load/store
instead — `qbe_ir_compile_jit` runs the same `promote()` pass the text path
does, which lifts SSA-promotable stack slots automatically; proven in
`tests/ir_builder.rs::loop_sum_via_ir_builder_no_text`). See
`ir_builder.h`'s own top comment for the exact reasoning on each.

## What's verified, and what's still owed

`cargo test` (225 tests: 215 unit across `encoder`/`memory`, 10 integration
across `tests/{integration,patching,register_bug_investigation,ir_builder}.rs`)
passes clean, zero warnings, on this machine (Darwin arm64). Beyond the
pipeline plumbing (`tests/integration.rs`: `add`, and a `sum_to(n)` loop
exercising `resolve_fixups`'s backward-branch and `B_COND` paths):

- **`tests/ir_builder.rs`** proves the text-free path
  ([`ir::build_function_jit`]) end to end: the same `add`/`sum_to(n)`
  programs as `tests/integration.rs`, built with zero QBE IL text, executing
  identically — plus a dedicated test that a malformed builder-constructed
  function aborts cleanly through the setjmp guard instead of crashing the
  process, and that the *next* build afterward still succeeds (QBE's global
  state recovers correctly).

- **`tests/patching.rs`** proves both `patch.rs` mechanisms against real,
  executing JIT'd code: an oop slot updated in place with the calling
  function observing the change on its next call, no re-link; an
  inline-cache site re-patched from one target to another the same way.
- **`tests/register_bug_investigation.rs`** attempted to reproduce
  `../aot/known_issues/BUG_REPORT.md`'s arm64 register-allocation bug
  (transliterated directly from that report's own BASIC reproducer, plus
  two narrower variants) — **did not reproduce**, on this `jit/c` fork. See
  `../REVIEW.md`'s "Addendum 2" for the full account, including a real,
  unrelated bug this same investigation *did* find and fix (the unresolved-
  symbol trap stub was encoding an unallocated instruction instead of
  `BRK`, due to a wrong opcode-base constant — present in the original
  `jit_memory.zig` too).

This is real evidence the pipeline is wired together correctly end to
end — FFI boundary, encoder, memory manager, linker, the patching layer, and
now the text-free builder too, all agreeing on the same contract — not a
general bug-free guarantee. Specifically still owed before trusting this for
anything touching MACVM's GC'd heap:

1. **Breadth beyond these shapes**: no float/NEON path, no multi-function
   module (`build_function_jit` builds one function per call — see this
   file's scope note above), no large-immediate/large-frame path, no
   `ir_builder`-side call/trampoline test (`tests/ir_builder.rs` doesn't
   exercise `qbe_ir_call` at all — `tests/patching.rs` covers `CALL_EXT`
   only via text) has been exercised end-to-end yet — only unit-tested per
   instruction in isolation. The `tests/` directory is a template to extend,
   not a ceiling.
2. **The arm64 register-allocation bug**: three targeted reproduction
   attempts didn't trigger it (see above) — that's real negative evidence,
   not a proof it can't occur; the original report's full array-descriptor/
   bounds-check IL shape wasn't fully replicated.
3. **Everything Capstone/diagnostics would have caught** — this port has no
   disassembly-based sanity check layer at all; encoding bugs that don't
   crash (wrong-but-plausible instruction) have no safety net beyond the
   ported unit tests' fixed expected values and whatever the `tests/`
   directory happens to cover. (The trap-stub bug above is a concrete
   example of exactly this: a wrong-but-plausible-looking constant, caught
   only by actually executing the path, not by reading the code.)
