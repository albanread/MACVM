# Vendoring record — JASM `wfasm`

- **Source**: https://github.com/albanread/JASM.git, `rust/` subtree.
- **Commit**: `5ac1b1b53d1cfa859e124ff962cf6180ffb72c55` (2026-06-26).
- **Vendored**: 2026-07-03 (MACVM S9).
- **Working-tree note**: at vendoring time, the JASM working tree had two
  uncommitted local changes on top of the pinned commit — `native_macos.rs`
  refactored to delegate its relocation bit-patching to a new, then-untracked
  `relocpatch.rs` (shared with an also-untracked AOT Mach-O writer,
  `macho.rs`, not vendored here — MACVM has no use for a file-output
  backend). `docs/reference-vm-analysis.md` §4.3 already documented
  `relocpatch.rs` as existing before this vendoring pass, and
  `sprint_s09_detail.md`'s own D1 file list (written against this same
  working-tree state) lists it as file #6 — this is the actual, intended
  vendoring source, not a discrepancy. Both files are vendored as they stood
  in the working tree, not reconstructed from the commit alone.

## File list

| Destination | Source | Edits |
|---|---|---|
| `a64/parse.rs` | `src/a64/parse.rs` | provenance header only |
| `a64/encode.rs` | `src/a64/encode.rs` | provenance header only |
| `a64/mod.rs` | `src/a64/mod.rs` | header; `crate::backend::*` -> `crate::vendor::wfasm::backend::*` (4 sites: 1 `use`, 2 doc links, 1 trait impl); deleted the final ~500 lines (a differential test module gated on an oracle-crate feature flag) entirely — the P1 lint is a literal grep across this tree, so even an inert reference to that module/flag name in a comment trips it (this table row deliberately doesn't spell out the exact strings either) |
| `backend.rs` | `src/backend.rs` | provenance header only |
| `native_macos.rs` | `src/native_macos.rs` | header; `crate::backend`/`crate::relocpatch`/`crate::a64` -> `crate::vendor::wfasm::*`; added `MacJit::region_raw` + free fns `jit_write_protect`/`icache_invalidate` (sprint_s09_detail.md D1.2) |
| `relocpatch.rs` | `src/relocpatch.rs` | header; `crate::backend::RelocKind` -> `crate::vendor::wfasm::backend::RelocKind` |
| `corpus_replay.rs` | `src/difftest.rs`, trimmed | see sprint_s09_detail.md D1.3 — not a verbatim copy; kept: `Verdict` (+ `Display`, `hex`), `mask_relocs`, `reloc_class`, `relocs_equiv`, `first_diff`, `compare`, `unhex`, `parse_corpus_line`, `kind_str`, `corpus_replay_aarch64_matches_golden`. Dropped: x86 support, the live-LLVM-oracle test paths, `record_corpus`/`diff_model`/generator machinery — corpus regeneration stays upstream in JASM |
| `corpus/aarch64.tsv` | `corpus/aarch64.tsv` | byte-identical, 1181 forms — never regenerate in MACVM |
| `LICENSE-JASM` | `LICENSE` (repo root) | verbatim |

## Re-vendoring

`diff` each destination file (minus its provenance header and `// MACVM:`-
marked lines) against `<JASM checkout>/rust/src/<original path>` — the only
hunks should be exactly those two categories. If upstream JASM has moved past
this commit, re-run the procedure in `docs/sprints/sprint_s09_detail.md` D1
and update the commit hash here and in every file's own header.
