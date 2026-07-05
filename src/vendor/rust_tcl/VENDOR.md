# Vendoring record — `rust-tcl`

- **Source**: https://github.com/albanread/locus.git, `rust-tcl/` subtree.
- **Commit**: `bce3847` (last commit touching `rust-tcl/`), tree at
  `24a92a0a23233b28b911b0693e6b387cf418e5ef`. Working tree clean at
  vendoring time (`git status --porcelain -- rust-tcl` empty).
- **Vendored**: 2026-07-05 (MACVM RUSTTCL diagnostic shell).
- **Why**: a small, dependency-free, already-working Tcl implementation
  (lexer → parser → sema → CFG → bytecode → VM, with `set`/`if`/`while`/
  `foreach`/`proc`/`upvar`/`uplevel`/`list`/`dict`/`expr` and an
  extensible `Registry::register` verb table) — reused as-is rather than
  hand-rolling a second, weaker command shell for MACVM's own VM
  introspection tools (`docs/RUSTTCL.md`... no such doc yet; see
  `src/rusttcl/` for the MACVM-side integration this tree feeds).

## Why source-vendored, not a Cargo dependency

Sister repos (MF66/MF66_baseline/MF67) depend on `rust-tcl` via a normal
Cargo path dependency (`vendor/rust-tcl` + a `[dependencies]` line). MACVM
vendors the SOURCE instead, compiled as part of the `macvm` crate itself
(no `Cargo.toml`/`Cargo.lock` edits at all) — the same choice S9 made for
JASM's `wfasm` (see `../wfasm/VENDOR.md`), and specifically necessary here
since `Cargo.toml`/`Cargo.lock` were off-limits to the session that did
this vendoring (owned by concurrent, unrelated work).

## File list

Every file below is a byte-for-byte copy of upstream EXCEPT a single
mechanical rewrite: `crate::` → `crate::vendor::rust_tcl::` (upstream's
`crate::` meant "this crate's own root"; nested inside MACVM's crate, that
root is `crate::vendor::rust_tcl`). No logic, types, or tests were changed.

| Destination | Source | Edits |
|---|---|---|
| `mod.rs` | `src/lib.rs` | no `crate::` rewrite needed (upstream's `lib.rs` has zero `crate::` references — only relative `mod`/`pub use` items); reformatted with `rustfmt --edition 2021` (upstream is edition 2024; one macro-call-argument wrapping difference, in the `#[cfg(test)]` block) plus the module-doc/`#![allow(...)]` header added above |
| `ast.rs` | `src/ast.rs` | `crate::` rewrite only |
| `bytecode.rs` | `src/bytecode.rs` | `crate::` rewrite only |
| `cfg.rs` | `src/cfg.rs` | `crate::` rewrite only |
| `codegen.rs` | `src/codegen.rs` | `crate::` rewrite only |
| `error.rs` | `src/error.rs` | `crate::` rewrite only |
| `expr.rs` | `src/expr.rs` | `crate::` rewrite only |
| `lexer.rs` | `src/lexer.rs` | `crate::` rewrite only |
| `list_value.rs` | `src/list_value.rs` | `crate::` rewrite only |
| `parser.rs` | `src/parser.rs` | `crate::` rewrite only |
| `registry.rs` | `src/registry.rs` | `crate::` rewrite; reformatted with `rustfmt --edition 2021` (upstream is edition 2024; one long `type` alias line wraps differently) |
| `sema.rs` | `src/sema.rs` | `crate::` rewrite only |
| `span.rs` | `src/span.rs` | none needed — zero `crate::` references upstream |
| `value.rs` | `src/value.rs` | none needed — zero `crate::` references upstream |
| `verbs.rs` | `src/verbs.rs` | `crate::` rewrite only |
| `vm.rs` | `src/vm.rs` | `crate::` rewrite only |

Every file passes `rustfmt --edition 2021 --check` (MACVM's own house style/edition,
CONVENTIONS' "always rustfmt before commit" rule) — most needed no change at
all; `mod.rs`/`registry.rs` had the two edition-2024-vs-2021 formatting
differences noted above. None of this touches logic, types, or tests.

**Not vendored**: `src/main.rs` — upstream's own standalone `rust-tcl` CLI
binary (parses argv, runs a `.tcl` file). MACVM has its own entry point
(`macvm rusttcl`, `src/main.rs::cmd_rusttcl` + `src/rusttcl/`); vendoring a
second, unused `fn main` would just be dead code.

Upstream has zero `unsafe` code anywhere in this tree — no module-level
`#![allow(unsafe_code)]` opt-out was needed here (contrast `../wfasm/`,
which does its own `mmap`/JIT-protection FFI and needs one).

## Re-vendoring

`diff` each destination file against `<locus checkout>/rust-tcl/src/<same
name, lib.rs -> mod.rs>` after undoing the `crate::` rewrite (`sed 's/crate::vendor::rust_tcl::/crate::/g'`)
— the diff should be empty for every file. If upstream has moved on,
re-run this same copy + rewrite procedure and update the commit hash
above.
