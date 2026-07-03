// Vendored from JASM (wfasm), https://github.com/albanread/JASM  commit 5ac1b1b53d1cfa859e124ff962cf6180ffb72c55
// Original path: rust/src/relocpatch.rs.  License: MIT (see LICENSE-JASM in
// this directory; Copyright (c) 2026 alban read).
// Local modifications are marked with `// MACVM:` comments — keep the diff
// against upstream minimal so re-vendoring stays mechanical.
// MACVM: `use crate::backend::RelocKind` -> `crate::vendor::wfasm::backend::
// RelocKind` (module moved under MACVM's own tree).

//! AArch64 relocation patching — the bit-math shared by the live JIT loader
//! (`native_macos::MacJit`) and the AOT Mach-O writer. Pure: takes a mutable
//! byte buffer (already sized to hold code + any far-call veneers) and a
//! list of already-resolved relocations, and patches the buffer in place.
//! No mmap, no W^X toggling, no OS calls — those stay in the caller, which
//! is what makes this reusable for a file-output backend (no live memory
//! involved at all) as well as the existing live-JIT path.
//!
//! `MacJit::finalize` is the original owner of this logic; it now delegates
//! here instead of duplicating it, so both callers share one implementation
//! verified by the same relocation-kind test coverage.

use std::collections::HashMap;

use anyhow::{bail, Result};

use crate::vendor::wfasm::backend::RelocKind;

/// One relocation, fully resolved: where to patch and the final target
/// address (already includes any `addend`).
pub struct ResolvedReloc {
    /// Byte offset into the buffer passed to [`patch_relocs`] — NOT relative
    /// to any individual module's own base, but to the buffer as a whole
    /// (i.e. already includes whatever offset a module was placed at).
    pub field_offset: usize,
    pub kind: RelocKind,
    pub target: u64,
}

/// `movz/movk x16, …; br x16` — load `addr` into x16 and branch. 5 words.
/// Identical to `native_macos::abs_veneer`; the single copy now lives here.
pub fn abs_veneer(addr: u64) -> [u32; 5] {
    let g = |i: u32| ((addr >> (16 * i)) & 0xFFFF) as u32;
    [
        0xD280_0000 | (g(0) << 5) | 16,             // movz x16, #g0
        0xF280_0000 | (1 << 21) | (g(1) << 5) | 16, // movk x16, #g1, lsl #16
        0xF280_0000 | (2 << 21) | (g(2) << 5) | 16, // movk x16, #g2, lsl #32
        0xF280_0000 | (3 << 21) | (g(3) << 5) | 16, // movk x16, #g3, lsl #48
        0xD61F_0000 | (16 << 5),                    // br x16
    ]
}
pub const VENEER_LEN: usize = 20;

/// OR `bits` into the 32-bit instruction word at `code[at..at+4]`
/// (read-modify-write, little-endian) — the buffer-slice analogue of
/// `native_macos::or_word`'s raw-pointer version.
fn or_word(code: &mut [u8], at: usize, bits: u32) {
    let w = u32::from_le_bytes(code[at..at + 4].try_into().unwrap()) | bits;
    code[at..at + 4].copy_from_slice(&w.to_le_bytes());
}

/// Patch every relocation in `relocs` into `code`. `code_base` is the address
/// `code[0]` will have once loaded/mapped (a live `mmap` base for JIT, or the
/// chosen `__TEXT` load address for a Mach-O file) — needed for the
/// PC-relative math (`Branch26`/`AdrpPage21`). `next_free` is the first
/// unused byte in `code` available for far-call veneers (`Branch26` targets
/// out of ±128 MB range get an absolute `movz/movk/br` veneer appended
/// there, deduplicated by target); `code.len()` must already leave room for
/// one [`VENEER_LEN`] per potentially-far target, the same way
/// `MacJit::build_if_needed` sizes its region today
/// (`code.len() + externs.len() * VENEER_LEN`). Returns the new `next_free`
/// (how many veneer bytes were actually used) so the caller can truncate a
/// growable buffer or know how much of a fixed one is now live.
pub fn patch_relocs(
    code: &mut [u8],
    code_base: u64,
    mut next_free: usize,
    relocs: &[ResolvedReloc],
) -> Result<usize> {
    let mut veneers: HashMap<u64, u64> = HashMap::new();
    for r in relocs {
        let field = code_base + r.field_offset as u64;
        match r.kind {
            RelocKind::Branch26 => {
                let mut disp = r.target as i64 - field as i64;
                if !(-(1 << 27)..(1 << 27)).contains(&disp) {
                    // Out of ±128 MB → route through an absolute veneer.
                    let v = match veneers.get(&r.target) {
                        Some(&v) => v,
                        None => {
                            if next_free + VENEER_LEN > code.len() {
                                bail!("veneer space exhausted");
                            }
                            let v_addr = code_base + next_free as u64;
                            let words = abs_veneer(r.target);
                            for (i, w) in words.iter().enumerate() {
                                code[next_free + i * 4..next_free + i * 4 + 4]
                                    .copy_from_slice(&w.to_le_bytes());
                            }
                            next_free += VENEER_LEN;
                            veneers.insert(r.target, v_addr);
                            v_addr
                        }
                    };
                    disp = v as i64 - field as i64;
                }
                or_word(code, r.field_offset, ((disp >> 2) as u32) & 0x03FF_FFFF);
            }
            RelocKind::AdrpPage21 => {
                let delta = ((r.target & !0xFFF) as i64 - (field & !0xFFF) as i64) >> 12;
                if !(-(1 << 20)..(1 << 20)).contains(&delta) {
                    bail!("adrp page offset out of ±4 GB range");
                }
                let d = (delta as u32) & 0x1F_FFFF;
                let immlo = d & 0x3;
                let immhi = (d >> 2) & 0x7_FFFF;
                or_word(code, r.field_offset, (immlo << 29) | (immhi << 5));
            }
            RelocKind::AddPageOff12 => {
                or_word(code, r.field_offset, ((r.target & 0xFFF) as u32) << 10);
            }
            RelocKind::Abs64 => {
                code[r.field_offset..r.field_offset + 8].copy_from_slice(&r.target.to_le_bytes());
            }
            RelocKind::BranchRel32 | RelocKind::RipRel32 => {
                bail!("x86 relocation kind {:?} in an AArch64 module", r.kind);
            }
        }
    }
    Ok(next_free)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abs_veneer_encoding() {
        let v = abs_veneer(0x1122_3344_5566_7788);
        assert_eq!(v[0], 0xD2800000 | (0x7788 << 5) | 16);
        assert_eq!(v[4], 0xD61F0000 | (16 << 5));
    }

    /// A near Branch26 (in ±128MB range) patches the imm26 field directly,
    /// no veneer consumed.
    #[test]
    fn branch26_near_no_veneer() {
        let mut code = vec![0u32.to_le_bytes(); 4].concat(); // 16 bytes, room for 4 words
        let code_base = 0x1_0000_0000u64;
        let target = code_base + 8; // 2 instructions ahead
        let relocs = [ResolvedReloc {
            field_offset: 0,
            kind: RelocKind::Branch26,
            target,
        }];
        let len = code.len();
        let next_free = patch_relocs(&mut code, code_base, len, &relocs).unwrap();
        assert_eq!(next_free, len, "no veneer should have been used");
        let w = u32::from_le_bytes(code[0..4].try_into().unwrap());
        assert_eq!(w & 0x03FF_FFFF, (8i64 >> 2) as u32 & 0x03FF_FFFF);
    }

    /// A far Branch26 (beyond ±128MB) gets routed through a deduplicated
    /// absolute veneer appended after `next_free`.
    #[test]
    fn branch26_far_uses_veneer() {
        let mut code = vec![0u8; 4 + VENEER_LEN * 2]; // one instr + room for 2 veneers
        let code_base = 0x1_0000_0000u64;
        let target = code_base + (1 << 30); // 1 GB away, out of ±128MB range
        let relocs = [ResolvedReloc {
            field_offset: 0,
            kind: RelocKind::Branch26,
            target,
        }];
        let next_free = patch_relocs(&mut code, code_base, 4, &relocs).unwrap();
        assert_eq!(
            next_free,
            4 + VENEER_LEN,
            "exactly one veneer should have been added"
        );
        // The veneer bytes at offset 4 should decode back to `target` via abs_veneer.
        let expected = abs_veneer(target);
        for (i, w) in expected.iter().enumerate() {
            let got = u32::from_le_bytes(code[4 + i * 4..4 + i * 4 + 4].try_into().unwrap());
            assert_eq!(got, *w, "veneer word {i} mismatch");
        }
    }
}
