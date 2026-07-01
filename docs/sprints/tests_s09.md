# Sprint S9 — Test Plan

## Acceptance gate

Restated from SPRINTS.md S9, made checkable:

1. `cargo test corpus_replay_aarch64_matches_golden` passes in-tree, on a
   machine with **no LLVM installed** (verify: `which llvm-mc` fails), and the
   test asserts > 1000 forms replayed.
2. JIT smoke: emit and execute (a) `(x+1)*2`, (b) a function with an internal
   conditional branch, (c) a call to a Rust `extern "C"` function — all three
   return correct values in-process.
3. Write-protect toggle + icache-flush round trip: publish code, run it, patch
   it under a guard, run the NEW behavior.
4. Literal-pool word patch-and-rerun: a published function returns a pool
   constant; `patch_pool_word` changes it; re-execution returns the new value
   with **no re-publish**.
5. All prior sprints' tests still green (standing rule 1).

`just gate-s09` runs, in order:

```sh
! command -v llvm-mc                      # document the no-LLVM claim (warn only —
                                          # CI images without llvm make it hard)
! grep -rn 'crate::oracle\|feature = "llvm"' src/vendor/   # P1 lint, hard fail
cargo test -p macvm                       # includes corpus replay + all units
cargo test -p macvm --release -- it_codecache   # smoke tests under optimizations
                                          # (W^X/icache bugs can hide in debug)
cargo clippy -- -D warnings
```

Provenance check (manual, once per vendoring): `diff` each vendored file
against `../JASM/rust/src/<orig>` — the only hunks allowed are the provenance
header and `// MACVM:`-marked lines. Record the JASM commit hash in the
header AND in `src/vendor/wfasm/VENDOR.md` (one line: commit, date, file
list) so re-vendoring is mechanical.

## Unit tests

| Test name | Module | Assertion | Rationale |
|---|---|---|---|
| `corpus_replay_aarch64_matches_golden` | `vendor::wfasm::corpus_replay` | every corpus line re-encodes byte-identically; count > 1000 | frozen-oracle trust gate (SPEC §12.7) |
| `vendor_has_no_oracle_refs` | `tests/it_codecache.rs` (build-time grep via `include_str!` is overkill — make it a `just` lint: `! grep -rn "crate::oracle\|feature = \"llvm\"" src/vendor/`) | zero hits | P1: corpus must run without LLVM |
| `emit_matches_corpus_forms` | `compiler::jasm_assembler` | `emit("add",[x(0),x(1),x(2)])` produces `8b020020`-LE word = corpus line 1; repeat for `sub`, `ldr` scaled, `ldur` unscaled, `movz` | trait→encoder plumbing is faithful |
| `emit_rejects_sym_fixups` | `compiler::jasm_assembler` | debug: `emit` with an operand mix yielding fixups panics | P6 guard |
| `label_backward_branch_bits` | `compiler::jasm_assembler` | `bind(l)` then `b(l)` after 3 insns → imm26 == −4 (0x3FFFFFC masked) | D3.2 math, negative displacement |
| `label_forward_branch_patched` | `compiler::jasm_assembler` | `b(l)` … `bind(l)`: placeholder is `0x14000000`, after finish the field equals forward distance | forward-ref chain works |
| `bcond_cbz_tbz_fields` | `compiler::jasm_assembler` | B19 at [23:5], B14 at [18:5] round-trip for ±  distances | field-position mistakes are silent otherwise |
| `ldr_literal_encoding` | `compiler::jasm_assembler` | `ldr_literal(x0, lit)` with pool 16 bytes ahead → word == `0x58000080` | D3.4 hand encoding (not corpus-covered) |
| `finish_pool_layout` | `compiler::jasm_assembler` | pool 8-aligned at `literal_off`; oop-kind relocs point at correct word offsets; dedup by (value,kind) | D3.3 + P10 |
| `listing_format_stable` | `compiler::jasm_assembler` | listing line for one instruction matches pinned format exactly | S10 goldens depend on it |
| `alloc_first_fit_and_split` | `codecache` | alloc 64/128/64; free middle; alloc 96 → reuses hole, 32-byte remainder retained | free-list math |
| `free_coalesces_neighbors` | `codecache` | free A, free B adjacent → one entry; free block abutting `top` retracts `top` | fragmentation control |
| `alloc_alignment` | `codecache` | every returned base % 16 == 0 | AAPCS/pool alignment |
| `alloc_exhaustion_returns_none` | `codecache` | tiny cache, over-alloc → `None`, no panic | SPEC §9 full-cache policy is caller's |
| `guard_depth_asserted` | `codecache::guard` | debug: nested `JitWriteGuard::new` panics | P3 |
| `guard_restores_exec_on_panic` | `codecache::guard` | `catch_unwind` around a panicking closure holding a guard → thread back in exec mode (Drop ran) | RAII is the point |
| `patch_branch26_in_range_no_veneer` | `codecache` | near target → no veneer allocated (`top` unchanged), field == displacement | veneer must be the exception |
| `reloc_offsets_blob_relative` | `compiler::jasm_assembler` | `CodeBlob.relocs` offsets index into `code` correctly for a 2-literal blob | S10 consumes these blindly |
| `pool_word_alignment` | `compiler::jasm_assembler` | `literal_off % 8 == 0` even when code length % 8 == 4 | GC writes u64s — must be aligned |

## Integration/golden tests

All in `tests/it_codecache.rs`; each builds a `CodeBlob` with `JasmAssembler`,
publishes into a shared `CodeCache::new(1 << 20)`, and calls the code via
`std::mem::transmute` to an `extern "C"` fn type. (This file is allowed
`unsafe` — it lives in `tests/`, exempt from the module lint; keep the
transmutes wrapped in one helper.)

1. **`smoke_arith`** — setup: emit `add x0, x0, #1; lsl x0, x0, #1; ret`.
   Input: `f(20)`. Expected: `42`.
2. **`smoke_internal_branch`** — emit: `cmp x0, #10; b.lt L; mov x0, #1;
   ret; L: mov x0, #0; ret` (labels both directions). Expected: `f(3) == 0`,
   `f(30) == 1`.
3. **`smoke_call_rust_extern`** — a `pub extern "C" fn add3(a: u64) -> u64`
   in the test; emit prologue `stp x29,x30,[sp,#-16]!; mov x29,sp`, then
   `call_far(literal_u64(add3 as u64, Some(RuntimeAddr)))`, epilogue, ret.
   Expected: result == input+3. Exercises the `ldr x16; blr x16` far-call
   path (Rust code is > ±128 MB from the MAP_JIT region in practice; assert
   the distance in the test and skip-with-message if ASLR ever lands it
   close — do NOT let the test silently stop covering the far path).
4. **`patch_and_rerun_branch26`** — publish two leaf functions `ret_1`,
   `ret_2` and a caller whose `bl_patchable` site is patched to `ret_1`;
   run (==1); `patch_branch26` to `ret_2`; run (==2). Covers gate item 3:
   write-protect round trip on LIVE code.
5. **`veneer_fallback_forced`** — call `patch_branch26` with a synthetic
   target address `cache_base + 512 MiB` (never executed): assert a 20-byte
   veneer was allocated (cache `top` advanced by ≥ VENEER_LEN) and the `bl`
   field targets the veneer, and the veneer's movz/movk words reconstruct the
   target (`relocpatch::abs_veneer` comparison). Execution of the far target
   is NOT attempted.
6. **`literal_pool_patch_rerun`** — emit `ldr_literal(x0, lit(0x2A))`; ret.
   Run → 42. `patch_pool_word` → 0x54. Run → 84. Gate item 4; proves pool
   words are D-side data (no re-publish, no explicit flush needed beyond the
   guard's).
7. **`publish_is_position_independent`** — publish the SAME blob at two
   different cache offsets; both execute identically (catches any accidental
   absolute intra-blob reference).
8. **`freelist_reuse_executes`** — publish A, publish B, free A, publish C
   (same size as A) into A's hole; run B and C. Catches stale-icache bugs on
   REUSED cache memory — the case the S12 flush protocol will hit constantly
   (C's publish guard must flush the reused range; if the guard's `note` is
   forgotten, C executes A's stale instructions here, deterministically).
9. **`two_blobs_one_guard`** — patch two `bl` sites in different segments
   under ONE `JitWriteGuard` with two `note`d ranges; both patches visible
   after drop. Validates the multi-range flush path GC will use (S12 P5).

Execution-helper pattern (pinned for all of the above):

```rust
unsafe fn call1(entry: *const u8, a: u64) -> u64 {
    let f: extern "C" fn(u64) -> u64 = std::mem::transmute(entry);
    f(a)
}
```

All smoke tests run on the main test thread — never inside
`std::thread::spawn` (the write-protect toggle is per-thread, S9 P3; a
helper-thread version would pass or fail for the wrong reasons).

## In-language tests

None. S9 has no interpreter/world coupling; the in-language suite is
unaffected and must simply remain green (`tests/it_world.rs` unchanged).

## Stress/negative tests

- `exec_without_publish_would_fault` — documented as a **manual** test only
  (a `#[ignore]` test that writes to the region without a guard and expects
  SIGBUS): validates W^X is actually enforced on this machine. Not in CI (it
  crashes the process by design); run once per toolchain bump.
- `alloc_exhaustion_returns_none` (also listed above) — negative path.
- `unbound_label_panics` — `finish` with an unbound label panics with the
  label id in the message (compiler-bug loudness, CONVENTIONS §4).
- Fuzz-lite: `alloc_free_random_churn` — 10k random alloc/free ops against a
  model allocator (Vec of ranges); assert no overlap, coalescing matches
  model, peak `top` bounded. Seeded, deterministic.
- `emit_bad_mnemonic_panics` — `emit("frobnicate", &[])` panics with the
  mnemonic in the message (compiler-bug loudness; never a guest error).
- `finish_twice_panics` — calling `finish` on a consumed assembler is a
  loud bug, not a silent empty blob.

## Non-goals

- Correctness of the ~2,400-line vendored encoder beyond corpus replay — the
  corpus IS the coverage; MACVM adds none (regeneration lives upstream).
- nmethod/CodeTable/dispatch tests → `tests_s10.md`.
- IC patching semantics on real send sites, PIC layout → `tests_s11.md`.
- GC reading Oop relocs / moving objects referenced from pools →
  `tests_s12.md` (S9 only proves the patch mechanics).
- Performance — no perf assertions in S9 (standing rule 3).
