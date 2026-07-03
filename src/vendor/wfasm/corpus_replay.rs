// Derived from JASM (wfasm), https://github.com/albanread/JASM  commit 5ac1b1b53d1cfa859e124ff962cf6180ffb72c55
// Original path: rust/src/difftest.rs — NOT a verbatim copy (sprint_s09_
// detail.md D1.3): trimmed to exactly the AArch64 corpus-replay path plus
// the pieces it depends on (`Verdict`+`Display`+`hex`, `mask_relocs`,
// `reloc_class`, `relocs_equiv`, `first_diff`, `compare`, `unhex`,
// `parse_corpus_line`, `kind_str`, and the test itself). Dropped: x86
// support, the test paths that talk to a live external disassembler oracle
// (see this file's own module doc below for why that path isn't vendored —
// deliberately not spelling out the module path/feature-flag name here
// too, since the S9 P1 lint is a literal, comment-blind grep for exactly
// that text across this whole tree), `record_corpus`/`diff_model`/
// `diff_all`/the generator-driven coverage machinery. Corpus regeneration
// stays upstream in JASM; this file only replays the frozen result.
// License: MIT (see LICENSE-JASM in this directory; Copyright (c) 2026
// alban read).

//! The no-LLVM regression gate (S9 P1): the vendored native [`A64Encoder`]
//! must reproduce every committed golden in `corpus/aarch64.tsv` byte-for-
//! byte, checked without ever encoding anything live against an LLVM-MC
//! oracle — the frozen corpus *is* the oracle here. This is what lets
//! `cargo test` pass on a machine with no LLVM installed at all: JASM's own
//! `difftest.rs` records `corpus/aarch64.tsv` by running the encoder
//! against a live oracle upstream, once; MACVM only ever replays that
//! committed file.
//!
//! Normalization (kept from the original driver): the frozen golden and a
//! fresh encode agree on the *encoding* but not on relocated displacement
//! values (the encoder leaves a zero placeholder at a reloc site; the
//! corpus line's code bytes have those same fields zeroed when recorded —
//! see `corpus_line` in the upstream file). So [`compare`] masks every byte
//! a relocation covers to `0x00` on both sides before comparing, then
//! compares the relocation lists structurally.

use crate::vendor::wfasm::backend::{EncodedModule, Encoder, Reloc, RelocKind};

/// The outcome of comparing one assembled form against its golden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Byte-identical (after masking reloc fields) and reloc-equivalent.
    Match,
    /// Code bytes diverge after masking — a real encoder regression.
    ByteMismatch {
        asm: String,
        rasm: Vec<u8>,
        oracle: Vec<u8>,
        /// First differing index in the masked, compared view.
        first_diff: usize,
    },
    /// Code matched but the relocation lists are not equivalent.
    RelocMismatch {
        asm: String,
        rasm: Vec<Reloc>,
        oracle: Vec<Reloc>,
    },
    /// The encoder errored on a form the frozen corpus says it used to
    /// handle — a real regression, not a coverage gap (the corpus was only
    /// ever populated with forms the encoder already matched).
    RasmError { asm: String, err: String },
}

impl Verdict {
    pub fn is_match(&self) -> bool {
        matches!(self, Verdict::Match)
    }
}

/// Compare a fresh encode against a recorded golden.
pub fn compare(asm: &str, rasm_mod: &EncodedModule, oracle_mod: &EncodedModule) -> Verdict {
    let masked_rasm = mask_relocs(&rasm_mod.code, &rasm_mod.relocs);
    let masked_oracle = mask_relocs(&oracle_mod.code, &oracle_mod.relocs);
    if masked_rasm != masked_oracle {
        return Verdict::ByteMismatch {
            asm: asm.to_string(),
            rasm: rasm_mod.code.clone(),
            oracle: oracle_mod.code.clone(),
            first_diff: first_diff(&masked_rasm, &masked_oracle),
        };
    }
    if !relocs_equiv(&rasm_mod.relocs, &oracle_mod.relocs) {
        return Verdict::RelocMismatch {
            asm: asm.to_string(),
            rasm: rasm_mod.relocs.clone(),
            oracle: oracle_mod.relocs.clone(),
        };
    }
    Verdict::Match
}

/// Return a copy of `code` with every byte covered by a relocation zeroed.
fn mask_relocs(code: &[u8], relocs: &[Reloc]) -> Vec<u8> {
    let mut out = code.to_vec();
    for r in relocs {
        let start = r.at.min(out.len());
        let end = (r.at + r.size as usize).min(out.len());
        for b in &mut out[start..end] {
            *b = 0;
        }
    }
    out
}

/// Collapse `RelocKind` to its machine class (AArch64: each field shape is
/// its own class — no x86-style rel32/disp32 aliasing).
fn reloc_class(k: RelocKind) -> u8 {
    match k {
        RelocKind::BranchRel32 | RelocKind::RipRel32 => 0,
        RelocKind::Abs64 => 1,
        RelocKind::Branch26 => 2,
        RelocKind::AdrpPage21 => 3,
        RelocKind::AddPageOff12 => 4,
    }
}

/// Structural reloc equivalence on `(offset, width, class, target)` as an
/// order-independent multiset. `addend` is intentionally excluded — see the
/// upstream file's own note; no AArch64 corpus form carries one.
fn relocs_equiv(a: &[Reloc], b: &[Reloc]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let key = |r: &Reloc| (r.at, r.size, reloc_class(r.kind), r.target.clone());
    let mut ka: Vec<_> = a.iter().map(key).collect();
    let mut kb: Vec<_> = b.iter().map(key).collect();
    ka.sort();
    kb.sort();
    ka == kb
}

fn first_diff(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()))
}

fn kind_str(k: RelocKind) -> &'static str {
    match k {
        RelocKind::Abs64 => "abs64",
        RelocKind::BranchRel32 | RelocKind::RipRel32 => "rel32",
        RelocKind::Branch26 => "branch26",
        RelocKind::AdrpPage21 => "adrp_page21",
        RelocKind::AddPageOff12 => "add_pageoff12",
    }
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Parse one corpus line — `<asm> \t <hex code, reloc fields zeroed> \t
/// <reloc;reloc;...>`, each reloc `at,size,kind,target` — back into
/// `(asm, golden EncodedModule)`.
fn parse_corpus_line(line: &str) -> Option<(String, EncodedModule)> {
    let mut fields = line.splitn(3, '\t');
    let asm = fields.next()?.to_string();
    let code = unhex(fields.next()?)?;
    let reloc_field = fields.next().unwrap_or("");
    let relocs = if reloc_field.is_empty() {
        Vec::new()
    } else {
        reloc_field
            .split(';')
            .map(|r| {
                let mut p = r.split(',');
                let at = p.next()?.parse().ok()?;
                let size = p.next()?.parse().ok()?;
                let kind = match p.next()? {
                    "rel32" => RelocKind::BranchRel32,
                    "abs64" => RelocKind::Abs64,
                    "branch26" => RelocKind::Branch26,
                    "adrp_page21" => RelocKind::AdrpPage21,
                    "add_pageoff12" => RelocKind::AddPageOff12,
                    _ => return None,
                };
                let target = p.next()?.to_string();
                Some(Reloc {
                    at,
                    size,
                    kind,
                    target,
                    addend: 0,
                })
            })
            .collect::<Option<Vec<_>>>()?
    };
    Some((
        asm,
        EncodedModule {
            code,
            relocs,
            ..Default::default()
        },
    ))
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Verdict::Match => write!(f, "match"),
            Verdict::ByteMismatch {
                asm,
                rasm,
                oracle,
                first_diff,
            } => write!(
                f,
                "BYTE MISMATCH `{asm}` @{first_diff}\n  rasm:   {}\n  oracle: {}",
                hex(rasm),
                hex(oracle)
            ),
            Verdict::RelocMismatch { asm, rasm, oracle } => write!(
                f,
                "RELOC MISMATCH `{asm}`\n  rasm:   {rasm:?}\n  oracle: {oracle:?}"
            ),
            Verdict::RasmError { asm, err } => write!(f, "RASM GAP `{asm}`: {err}"),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The no-LLVM AArch64 regression gate: the native `A64Encoder` must
    /// reproduce every committed golden in `corpus/aarch64.tsv` byte-for-
    /// byte. `kind_str` is unused by this replay direction (it's the
    /// inverse of the string match in `parse_corpus_line`, kept only
    /// because `#![allow(dead_code)]` on this tree makes carrying it
    /// harmless and re-vendoring simpler to eyeball against upstream).
    #[test]
    fn corpus_replay_aarch64_matches_golden() {
        use crate::vendor::wfasm::a64::A64Encoder;
        let corpus = include_str!("corpus/aarch64.tsv");
        let mut n = 0usize;
        for (i, line) in corpus.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let (asm, golden) = parse_corpus_line(line)
                .unwrap_or_else(|| panic!("aarch64 corpus line {} malformed: {line:?}", i + 1));
            let rm = A64Encoder
                .encode(&asm)
                .unwrap_or_else(|e| panic!("a64 failed on corpus form `{asm}`: {e:#}"));
            let v = compare(&asm, &rm, &golden);
            assert!(v.is_match(), "aarch64 corpus regression on `{asm}`: {v}");
            n += 1;
        }
        assert!(
            n > 1000,
            "aarch64 corpus suspiciously small ({n} forms) — regenerate upstream with `a64-corpus`"
        );
    }
}
