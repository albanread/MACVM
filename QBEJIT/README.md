# QBEJIT — extracted QBE-based codegen, for evaluation

Two separate extractions live here, from two sibling repos by the same author:

- **[`jit/`](jit/)** — the real thing: an in-process, W^X-compliant ARM64 JIT
  built on top of QBE's optimizer, from
  [github.com/albanread/FasterBASIC](https://github.com/albanread/FasterBASIC)
  (`zig_compiler/`). QBE IL goes in; executable machine code in `mmap`'d
  memory comes out — no assembler, no linker, no temp files, no subprocess.
- **[`aot/`](aot/)** — an earlier, ahead-of-time QBE integration from
  [github.com/albanread/FBCQBE](https://github.com/albanread/FBCQBE): QBE IL
  → assembly *text* → external `as`/`cc`. Kept for contrast; **this was my
  first extraction and it answered the wrong question** — it's a batch
  text-to-text backend, not a JIT. See `aot/` for that review; see
  [REVIEW.md](REVIEW.md) for why `jit/` is the one that actually matters.

Neither is wired into the MACVM build. Both exist to be read against
`docs/DESIGN.md` §5 (open decision D6, currently JASM-leaning) and
`docs/arm64.md` (W^X / `MAP_JIT` design). See [REVIEW.md](REVIEW.md) for the
discussion.

**[`jit/rust/`](jit/rust/)** is a full Rust port of `jit/{c,zig}` above,
driving the same vendored C QBE-fork over FFI instead of Zig — same
architecture, one language, one `cargo build`. `cargo test` there: 225
passing (215 unit, 10 end-to-end: both a QBE-IL-text path and a **text-free**
path — construct a function directly, no `.ssa` text or `parse()` involved —
through compile → encode → `mmap`/`MAP_JIT` allocate → link → execute,
checking the actual returned value of real generated code). See
`jit/rust/README.md`.

## `jit/rust/` — the Rust port

The evaluation crate MACVM would actually prototype against: `qbejit`, an
independent `cargo` package (own `Cargo.toml`, not a workspace member of
MACVM's) under `jit/rust/`. Drives `jit/c/`'s vendored QBE-fork sources
directly over FFI — no Zig toolchain involved at all. See
[`jit/rust/README.md`](jit/rust/README.md) for the module-by-module
breakdown; the short version: `csrc/rust_bridge.c` supplies the
setjmp/longjmp protected-call wrapper QBE's `basic_exit()` needs (so a
malformed IL string can't `exit()` the host process), `src/ffi.rs` mirrors
the C structs, `src/compile.rs` is the safe (text) entry point, `src/encoder.rs` /
`src/encode.rs` / `src/memory.rs` / `src/linker.rs` are straight ports of
the four Zig modules above them, `src/patch.rs` adds moving-GC-safe oop
updates and inline-cache re-patching (not in the Zig original — built for
MACVM's needs, see `REVIEW.md`'s "Addendum 2"), and `jit/c/ir_builder.{c,h}`
+ `src/ir{,_ffi}.rs` are the **text-free** entry point — construct a
function directly via Rust builder calls, no QBE IL text anywhere, through
the identical optimizer pipeline (see `REVIEW.md`'s "text-IL question").

## `jit/` layout

- **`jit/c/`** — QBE's SSA pipeline (parse → optimize → regalloc → isel),
  forked further than the `aot/` copy (adds NEON SIMD opcodes, more arm64
  peepholes — see `design/QBE_extensions.md`), plus two files that make it an
  embeddable JIT rather than a `cc1`-alike:
  - `qbe_bridge.c/h` — replaces QBE's `main()` with a C API
    (`qbe_compile_il_jit(il_text, len, JitCollector*, target)`). No global
    `main()`, callable from another process's own entry point.
  - `jit_collect.c/h` — walks QBE's *post-register-allocation* `Fn*` (the
    same structures `emit.c` would otherwise turn into assembly text) and
    flattens it into a `JitInst[]`: fixed-size, pointer-free, C-ABI structs
    (register ids, opcode, immediates) readable directly from Zig via
    `@cImport`, with zero text in the loop.

- **`jit/zig/`** — the ARM64 encoder and JIT runtime that consume `JitInst[]`:
  - `arm64_encoder.zig` — standalone, `std`-only ARM64 instruction encoder,
    ~300 `emit*` functions returning raw `u32` words. **Verified in this
    session**: `zig test arm64_encoder.zig` → 344/344 pass, standalone, on
    this machine (Darwin arm64, Zig 0.15.2).
  - `jit_encode.zig` — `JitInst[]` → `JitModule` (code/data buffers +
    unresolved fixups). **Verified**: 371/371 tests pass standalone.
  - `jit_memory.zig` — `mmap`/`MAP_JIT` allocation, `pthread_jit_write_protect_np`
    W^X toggling, icache invalidation. **Verified**: 19/19 tests pass
    standalone, including the W^X state-machine and trampoline-stub tests.
  - `jit_linker.zig` — resolves `ADRP`/`ADD` data relocations and builds the
    trampoline island for external calls (see below). **Verified**: 391/391
    tests pass standalone (chain-includes the three modules above).
  - `jit_capstone.zig`, `qbe.zig` — Capstone disassembly and the top-level
    `compileILJit()` entry point. Not independently re-tested here: both
    need the vendored `capstone/` C library and/or the `jit/c/` objects
    linked in, which is a full-build exercise, not a vendoring one.
  - `jit_runtime.zig` — ties the above into `compileAndRun(il_text)`.

- **`jit/reference/jit_stubs.zig`** — *not reusable, reference only*: a
  200+-entry `extern fn` jump table mapping QBE external-call names to
  FasterBASIC's own linked-in runtime. Shows the pattern (a symbol table for
  the trampoline island's `dlsym`/jump-table resolution), tightly coupled to
  their runtime.

- **`jit/known_issue_regression_test.bas`** — `tests/test_while_array_bug.bas`
  from the FasterBASIC repo, a live regression test with the same shape as
  the arm64 register-allocation bug documented in `aot/known_issues/`. I did
  **not** verify whether it currently passes (would require vendoring
  Capstone and doing a full build) — flagging it as unresolved risk, not a
  fixed bug. See REVIEW.md.

## `aot/` layout

See [`aot/README.md`](aot/README.md) — unchanged from the first pass, still
useful as: a smaller, easier-to-read reference for QBE's pipeline (`main.c`
was restored to QBE's plain upstream CLI driver, `qbe -t <target> file.ssa`),
and the documented arm64 register-allocator bug in `aot/known_issues/`.

## License

Both source repos are MIT, same copyright holder (`FasterBASIC QBE Project
Contributors`, 2025) for their own code, plus upstream QBE's MIT license
(Quentin Carbonneaux, 2015–2026) for the vendored backend. `LICENSE-QBE` and
`LICENSE-FBCQBE` at this folder's root cover both trees, `jit/` and `aot/`.
