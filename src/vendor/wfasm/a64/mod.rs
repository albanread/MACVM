// Vendored from JASM (wfasm), https://github.com/albanread/JASM  commit 5ac1b1b53d1cfa859e124ff962cf6180ffb72c55
// Original path: rust/src/a64/mod.rs.  License: MIT (see LICENSE-JASM in this
// directory; Copyright (c) 2026 alban read).
// Local modifications are marked with `// MACVM:` comments — keep the diff
// against upstream minimal so re-vendoring stays mechanical.
// MACVM: every `crate::backend::...` reference (the `use` plus two doc-link
// targets plus the trait impl) repointed to `crate::vendor::wfasm::backend::
// ...` (module moved under MACVM's own tree). MACVM: the original file's final
// ~500 lines — a differential test module gated on a feature flag naming an
// LLVM-MC oracle module MACVM does not vendor — are deleted entirely, not
// just left feature-gated: the S9 P1 lint is a literal, comment-blind
// grep for that module/flag name across this whole tree, so even an inert
// reference to it (in code OR in a comment quoting it) trips a hard fail.
// See this file's own trimmed test module below for what's kept: the
// oracle-free unit tests only.

//! `a64` — the native, LLVM-free AArch64 encoder for Apple Silicon.
//!
//! The AArch64 sibling of [`rasm`](crate::rasm): it takes the assembled
//! (post-macro-expansion) AArch64 assembly text the `asm/` front-end produces and
//! emits an [`EncodedModule`](crate::vendor::wfasm::backend::EncodedModule) the native loader
//! places. Tables/logic are derived from — and gated byte-for-byte against — an
//! external x86/AArch64 disassembler oracle (see MACVM's own S9 sprint doc for
//! why that verification path isn't vendored here). See
//! docs/design/aarch64-apple-silicon.md.
//!
//! Layering: [`parse`] (text → [`Line`](parse::Line)) → [`encode`] (one
//! instruction → one 32-bit word + fixups) → this module's driver (assign
//! offsets, resolve internal labels, emit relocs for externs).
//!
//! Unlike x86, AArch64 instructions are fixed-width, so there is no
//! rel8→rel32 relaxation: offsets are known after one layout pass. Out-of-range
//! branches will be handled by veneer insertion in a later phase (§3.4.3); the
//! first slice range-checks and errors instead.

pub mod encode;
pub mod parse;

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};

use crate::vendor::wfasm::backend::{EncodedModule, Reloc, RelocKind};

use encode::{encode, FixupKind};
use parse::{Directive, Line};

/// The native AArch64 [`Encoder`](crate::vendor::wfasm::backend::Encoder).
#[derive(Debug, Default, Clone, Copy)]
pub struct A64Encoder;

impl crate::vendor::wfasm::backend::Encoder for A64Encoder {
    fn encode(&self, asm_text: &str) -> Result<EncodedModule> {
        assemble(asm_text)
    }
}

/// A laid-out item in the text stream.
enum Item {
    /// Fixed bytes with optional branch fixups (offset-within-bytes, kind, target).
    Code {
        bytes: Vec<u8>,
        fixups: Vec<(usize, FixupKind, String)>,
    },
    Label(String),
    Globl(String),
    /// Pad to a 2^n boundary.
    AlignP2(u32),
    /// A relaxable conditional/compare branch (`b.cond`/`cbz`/`cbnz`/`tbz`/`tbnz`).
    /// `word` is the instruction with a zero immediate. When the target is out of
    /// the imm19/imm14 range, or is an extern, it relaxes to the **long** form —
    /// the condition inverted to skip an unconditional `b` to the target (which
    /// has ±128 MB reach, and may itself become a `Branch26` relocation/veneer).
    /// This is a deliberate extension beyond LLVM-MC, which errors on out-of-range
    /// conditional branches. See docs/design/aarch64-apple-silicon.md §3.4.3.
    CondBr {
        word: u32,
        target: String,
        is_long: bool,
    },
}

impl Item {
    fn size_at(&self, off: usize) -> usize {
        match self {
            Item::Code { bytes, .. } => bytes.len(),
            Item::Label(_) | Item::Globl(_) => 0,
            Item::CondBr { is_long, .. } => {
                if *is_long {
                    8
                } else {
                    4
                }
            }
            Item::AlignP2(n) => {
                let align = 1usize << *n;
                (align - (off % align)) % align
            }
        }
    }
}

/// Invert a conditional/compare branch word: flip the `b.cond` condition, or the
/// `cbz`/`cbnz`/`tbz`/`tbnz` op bit (which swaps zero ↔ non-zero).
fn invert_cond(word: u32) -> u32 {
    if (word >> 24) & 0xFF == 0x54 {
        word ^ 1 // b.cond: invert the 4-bit condition (low bit)
    } else {
        word ^ (1 << 24) // cbz↔cbnz / tbz↔tbnz: flip op bit24
    }
}

/// Assemble a module's AArch64 text into an [`EncodedModule`].
pub fn assemble(text: &str) -> Result<EncodedModule> {
    // ── Pass 1: parse into items ────────────────────────────────────────────
    let mut items: Vec<Item> = Vec::new();
    for (lineno, raw) in text.lines().enumerate() {
        let clean = parse::strip_comment(raw);
        let (label, rest) = parse::split_leading_label(clean);
        if let Some(name) = label {
            items.push(Item::Label(name.to_string()));
            if rest.is_empty() {
                continue;
            }
        }
        let body = if label.is_some() { rest } else { clean };
        let line =
            parse::parse_line(body).with_context(|| format!("line {}: `{raw}`", lineno + 1))?;
        match line {
            Line::Empty => {}
            Line::Label(name) => items.push(Item::Label(name)),
            Line::Directive(d) => push_directive(&mut items, d)?,
            Line::Insn { mnemonic, ops } => {
                let enc = encode(&mnemonic, &ops)
                    .with_context(|| format!("line {}: encode `{raw}`", lineno + 1))?;
                // A conditional/compare branch (single Branch19 fixup) becomes a
                // relaxable CondBr item; everything else is fixed Code.
                if enc.fixups.len() == 1 && enc.fixups[0].kind == FixupKind::Branch19 {
                    let word = u32::from_le_bytes(enc.bytes[..4].try_into().unwrap());
                    items.push(Item::CondBr {
                        word,
                        target: enc.fixups[0].target.clone(),
                        is_long: false,
                    });
                } else {
                    let fixups = enc
                        .fixups
                        .into_iter()
                        .map(|f| (f.at, f.kind, f.target))
                        .collect();
                    items.push(Item::Code {
                        bytes: enc.bytes,
                        fixups,
                    });
                }
            }
        }
    }

    // ── Relaxation: grow conditional branches to the long form when their
    // target is out of imm19 range or is an extern. Grow-only ⇒ converges. ────
    loop {
        let (offsets, labels) = layout(&items);
        let mut changed = false;
        for (idx, it) in items.iter_mut().enumerate() {
            if let Item::CondBr {
                target, is_long, ..
            } = it
            {
                if *is_long {
                    continue;
                }
                let must_long = match labels.get(target) {
                    None => true, // extern → must use the long (b reloc) form
                    Some(&tgt) => {
                        let disp = tgt as i64 - offsets[idx] as i64;
                        !(-(1 << 20)..(1 << 20)).contains(&disp)
                    }
                };
                if must_long {
                    *is_long = true;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    // ── Layout: byte offset of each item + every label ──────────────────────
    let (offsets, labels) = layout(&items);

    // ── Emit: place bytes, patch internal branches, emit relocs for externs ─
    let mut code: Vec<u8> = Vec::new();
    let mut symbols: BTreeMap<String, usize> = BTreeMap::new();
    let mut relocs: Vec<Reloc> = Vec::new();
    let mut externs: Vec<String> = Vec::new();
    let mut globls: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (idx, it) in items.iter().enumerate() {
        let off = offsets[idx];
        debug_assert_eq!(off, code.len());
        match it {
            Item::Label(n) => {
                symbols.insert(n.clone(), code.len());
            }
            Item::Globl(n) => {
                globls.insert(n.clone());
            }
            Item::AlignP2(n) => {
                let align = 1usize << *n;
                let pad = (align - (code.len() % align)) % align;
                write_nop_padding(&mut code, pad);
            }
            Item::Code { bytes, fixups } => {
                let base = code.len();
                code.extend_from_slice(bytes);
                for (at, kind, target) in fixups {
                    let field = base + at;
                    match kind {
                        // PC-relative page/page-offset are *always* relocated —
                        // the page address is only known at load time, so even a
                        // local symbol carries the reloc (matches LLVM-MC).
                        FixupKind::AdrpPage21 | FixupKind::AddPageOff12 => {
                            let rk = if *kind == FixupKind::AdrpPage21 {
                                RelocKind::AdrpPage21
                            } else {
                                RelocKind::AddPageOff12
                            };
                            relocs.push(Reloc {
                                at: field,
                                size: 4,
                                kind: rk,
                                target: target.clone(),
                                addend: 0,
                            });
                            if !labels.contains_key(target) {
                                externs.push(target.clone());
                            }
                        }
                        // Branches: patch internal targets, relocate externs.
                        FixupKind::Branch26 | FixupKind::Branch19 => {
                            if let Some(&tgt) = labels.get(target) {
                                patch_branch(&mut code, field, *kind, tgt)?;
                            } else if *kind == FixupKind::Branch26 {
                                relocs.push(Reloc {
                                    at: field,
                                    size: 4,
                                    kind: RelocKind::Branch26,
                                    target: target.clone(),
                                    addend: 0,
                                });
                                externs.push(target.clone());
                            } else {
                                bail!("conditional/compare branch to extern `{target}` needs a veneer (not in the first slice)");
                            }
                        }
                    }
                }
            }
            Item::CondBr {
                word,
                target,
                is_long,
            } => {
                let base = code.len();
                if !*is_long {
                    // Short form: patch imm19 in place.
                    code.extend_from_slice(&word.to_le_bytes());
                    let tgt = *labels
                        .get(target)
                        .expect("short cond-branch to extern is impossible");
                    let disp = tgt as i64 - base as i64;
                    let mut w = *word;
                    w |= (((disp >> 2) as u32) & 0x7_FFFF) << 5;
                    code[base..base + 4].copy_from_slice(&w.to_le_bytes());
                } else {
                    // Long form: `<inverted cond> +8` then `b target`.
                    let inv = invert_cond(*word) | (2u32 << 5); // imm19 = 2 → skip the b
                    code.extend_from_slice(&inv.to_le_bytes());
                    let b_at = code.len();
                    code.extend_from_slice(&0x1400_0000u32.to_le_bytes()); // b, imm26=0
                    if let Some(&tgt) = labels.get(target) {
                        let disp = tgt as i64 - b_at as i64;
                        if !(-(1 << 27)..(1 << 27)).contains(&disp) {
                            bail!("relaxed conditional branch target out of ±128MB range");
                        }
                        let w = 0x1400_0000u32 | (((disp >> 2) as u32) & 0x03FF_FFFF);
                        code[b_at..b_at + 4].copy_from_slice(&w.to_le_bytes());
                    } else {
                        // Conditional branch to an extern: the `b` is a Branch26
                        // relocation (the loader veneers it if it ends up far).
                        relocs.push(Reloc {
                            at: b_at,
                            size: 4,
                            kind: RelocKind::Branch26,
                            target: target.clone(),
                            addend: 0,
                        });
                        externs.push(target.clone());
                    }
                }
            }
        }
    }

    symbols.retain(|name, _| globls.contains(name));
    externs.sort();
    externs.dedup();

    Ok(EncodedModule {
        code,
        symbols,
        relocs,
        externs,
    })
}

/// Patch a PC-relative branch immediate into the 4-byte word at `field`.
fn patch_branch(code: &mut [u8], field: usize, kind: FixupKind, target: usize) -> Result<()> {
    let site = field as i64;
    let disp = target as i64 - site;
    if disp % 4 != 0 {
        bail!("branch target not 4-byte aligned (disp {disp})");
    }
    let imm = disp >> 2;
    let mut w = u32::from_le_bytes(code[field..field + 4].try_into().unwrap());
    match kind {
        FixupKind::Branch26 => {
            if !(-(1 << 25)..(1 << 25)).contains(&imm) {
                bail!("b/bl target out of ±128MB range (needs a veneer)");
            }
            w |= (imm as u32) & 0x03FF_FFFF;
        }
        FixupKind::Branch19 => {
            if !(-(1 << 18)..(1 << 18)).contains(&imm) {
                bail!("conditional/compare branch out of ±1MB range (needs inversion+veneer)");
            }
            w |= ((imm as u32) & 0x7_FFFF) << 5;
        }
        FixupKind::AdrpPage21 | FixupKind::AddPageOff12 => {
            bail!("pc-relative fixups are resolved as relocations, not patched in-section");
        }
    }
    code[field..field + 4].copy_from_slice(&w.to_le_bytes());
    Ok(())
}

/// Pad with AArch64 `NOP` words (`0xD503201F`) for whole words, zero bytes for
/// any sub-word remainder (a pure instruction stream is always word-aligned).
fn write_nop_padding(code: &mut Vec<u8>, mut pad: usize) {
    const NOP: [u8; 4] = [0x1f, 0x20, 0x03, 0xd5];
    while pad >= 4 {
        code.extend_from_slice(&NOP);
        pad -= 4;
    }
    for _ in 0..pad {
        code.push(0);
    }
}

fn layout(items: &[Item]) -> (Vec<usize>, BTreeMap<String, usize>) {
    let mut offsets = Vec::with_capacity(items.len());
    let mut labels = BTreeMap::new();
    let mut off = 0usize;
    for it in items {
        offsets.push(off);
        if let Item::Label(n) = it {
            labels.insert(n.clone(), off);
        }
        off += it.size_at(off);
    }
    (offsets, labels)
}

fn push_directive(items: &mut Vec<Item>, d: Directive) -> Result<()> {
    match d {
        Directive::Text | Directive::Other(_) => {}
        Directive::Globl(n) => items.push(Item::Globl(n)),
        Directive::P2align(n) => items.push(Item::AlignP2(n)),
        Directive::Quad(vs) => items.push(Item::Code {
            bytes: vs.iter().flat_map(|v| v.to_le_bytes()).collect(),
            fixups: vec![],
        }),
        Directive::Byte(b) => items.push(Item::Code {
            bytes: vec![b],
            fixups: vec![],
        }),
        Directive::Zero(n) => items.push(Item::Code {
            bytes: vec![0u8; n],
            fixups: vec![],
        }),
        Directive::Ascii(bytes, nul) => {
            let mut v = bytes;
            if nul {
                v.push(0);
            }
            items.push(Item::Code {
                bytes: v,
                fixups: vec![],
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_branch_resolves_no_reloc() {
        // A countdown loop: cbz exits, b loops. Both internal → patched, no reloc.
        let src = "\
.globl countdown
countdown:
loop:
cbz x0, done
sub x0, x0, #1
b loop
done:
ret
";
        let m = assemble(src).unwrap();
        assert!(m.symbols.contains_key("countdown"));
        assert!(!m.symbols.contains_key("loop"), "local label not exported");
        assert!(
            m.relocs.is_empty(),
            "internal branches must not relocate: {:?}",
            m.relocs
        );
        assert!(m.externs.is_empty());
        // `b loop` is the 3rd word (offset 8): branch back to offset 0 → disp -8.
        // 0x17FFFFFE = b #-8.
        let b = u32::from_le_bytes(m.code[8..12].try_into().unwrap());
        assert_eq!(b, 0x17FF_FFFE, "b loop encodes as {b:#010x}");
    }

    #[test]
    fn cond_branch_to_extern_relaxes() {
        // `cbz x0, ext` → `cbnz x0, #8 ; b ext(reloc) ; ret`.
        let m = assemble(".globl w\nw:\ncbz x0, ext\nret\n").unwrap();
        assert_eq!(m.code.len(), 12, "long cond branch = 2 words + ret");
        assert_eq!(&m.code[0..4], &0xB500_0040u32.to_le_bytes(), "cbnz x0, #8");
        assert_eq!(
            &m.code[4..8],
            &0x1400_0000u32.to_le_bytes(),
            "b ext (imm26=0, reloc)"
        );
        assert_eq!(m.relocs.len(), 1);
        assert_eq!(m.relocs[0].kind, RelocKind::Branch26);
        assert_eq!(m.relocs[0].target, "ext");
        assert_eq!(m.relocs[0].at, 4);
        assert_eq!(m.externs, vec!["ext".to_string()]);
    }

    #[test]
    fn far_conditional_branch_relaxes_to_inverted_plus_b() {
        // `cbz x0, far` with `far` > 1MB away → inverted cond + unconditional b.
        let src = ".globl w\nw:\ncbz x0, far\n.space 0x100000\nfar:\nret\n";
        let m = assemble(src).unwrap();
        assert!(m.relocs.is_empty(), "internal target → no relocs");
        // cbz relaxed to 8 bytes; `far` now sits at 8 + 0x100000.
        assert_eq!(m.code.len(), 0x10000C);
        assert_eq!(&m.code[0..4], &0xB500_0040u32.to_le_bytes(), "cbnz x0, #8");
        // b at offset 4 → far (0x100008): disp 0x100004, imm26 0x40001.
        assert_eq!(&m.code[4..8], &0x1404_0001u32.to_le_bytes(), "b far");
    }

    #[test]
    fn extern_bl_becomes_branch26_reloc() {
        let src = ".globl w\nw:\nbl rt_emit\nret\n";
        let m = assemble(src).unwrap();
        assert_eq!(m.externs, vec!["rt_emit".to_string()]);
        assert_eq!(m.relocs.len(), 1);
        assert_eq!(m.relocs[0].kind, RelocKind::Branch26);
        assert_eq!(m.relocs[0].target, "rt_emit");
        assert_eq!(m.relocs[0].at, 0);
        // bl placeholder = 0x94000000 (imm26 = 0).
        assert_eq!(&m.code[0..4], &[0x00, 0x00, 0x00, 0x94]);
    }
}
