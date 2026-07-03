//! `JasmAssembler` — the `Assembler` trait's only implementor
//! (`docs/sprints/sprint_s09_detail.md` D2.2), over the vendored structured
//! encoder (`crate::vendor::wfasm::a64::encode::encode`).
//!
//! Three families of instruction reach the buffer differently:
//! - Plain instructions ([`Assembler::emit`]) go through the vendored
//!   encoder directly — it never sees a label, only tagged operands.
//! - Intra-blob branches (`b`/`b_cond`/`cbz`/`cbnz`/`tbz`/`tbnz`) are
//!   hand-encoded here as displacement-0 placeholder words (P6: the
//!   compiler never passes `Operand::Sym` to the vendored encoder — that's
//!   its own text front end's business, not this one's), queued in
//!   `pending`, and resolved in one pass at [`finish`](Assembler::finish)
//!   regardless of whether the target was already bound (D3.2's bit-
//!   insertion math, cross-checked against `encode.rs`'s own base words
//!   for the forms it does support: `b`/`bl`/`b.cond`/`cbz`/`cbnz`; `tbz`/
//!   `tbnz` aren't in its scope at all, so their base words are hand-
//!   derived from the ARM ARM's `TBZ`/`TBNZ` encoding instead).
//! - The literal pool (`literal_u64`/`ldr_literal`/`call_far`) is appended
//!   at `finish` time, after the code and its branch fixups are final
//!   (D3.3) — see that method's own doc for why the pool sits at the end.

use std::collections::HashMap;

use crate::compiler::assembler::{
    x, xr, Assembler, CodeBlob, Cond, Label, LiteralId, Reloc, RelocKind,
};
use crate::vendor::wfasm::a64::encode;
use crate::vendor::wfasm::a64::parse::{Operand, Reg, RegClass};

struct PendingBranch {
    /// Byte offset of the branch instruction word itself.
    at: u32,
    label: Label,
    kind: BranchKind,
}

/// Which bit-field the branch's displacement occupies (D3.2) — the
/// TBZ/TBNZ bit-POSITION fields (`b40`/`b5`) are baked into the base word
/// at emit time (the position is known immediately); only the imm14
/// branch-offset field is deferred.
#[derive(Clone, Copy)]
enum BranchKind {
    B26,
    B19,
    B14,
}

pub struct JasmAssembler {
    buf: Vec<u8>,
    labels: Vec<Option<u32>>,
    pending: Vec<PendingBranch>,
    literals: Vec<(u64, Option<RelocKind>)>,
    lit_dedup: HashMap<(u64, Option<RelocKind>), LiteralId>,
    /// (site offset of the `ldr`-literal word, which pool entry it targets)
    lit_loads: Vec<(u32, LiteralId)>,
    relocs: Vec<Reloc>,
    listing: Vec<String>,
    finished: bool,
}

impl JasmAssembler {
    pub fn new() -> Self {
        JasmAssembler {
            buf: Vec::new(),
            labels: Vec::new(),
            pending: Vec::new(),
            literals: Vec::new(),
            lit_dedup: HashMap::new(),
            lit_loads: Vec::new(),
            relocs: Vec::new(),
            listing: Vec::new(),
            finished: false,
        }
    }

    fn push_word(&mut self, w: u32) {
        self.buf.extend_from_slice(&w.to_le_bytes());
    }

    /// Debug/test builds only (`CodeBlob::listing`'s own doc) — S10's
    /// disassembly goldens read this, release builds don't pay for it.
    fn push_listing(&mut self, offset: u32, word: u32, text: &str) {
        if cfg!(debug_assertions) || cfg!(test) {
            self.listing
                .push(format!("{offset:06x}  {word:08x}  {text}"));
        }
    }

    /// Emit a displacement-0 branch placeholder, queue its fixup, and log
    /// it. Shared by every branch-emitting `Assembler` method.
    fn queue_branch(&mut self, base_word: u32, label: Label, kind: BranchKind, text: &str) {
        let at = self.offset();
        self.pending.push(PendingBranch { at, label, kind });
        self.push_word(base_word);
        self.push_listing(at, base_word, text);
    }
}

impl Default for JasmAssembler {
    fn default() -> Self {
        Self::new()
    }
}

fn cond_bits(c: Cond) -> u32 {
    match c {
        Cond::Eq => 0,
        Cond::Ne => 1,
        Cond::Hs => 2,
        Cond::Lo => 3,
        Cond::Mi => 4,
        Cond::Pl => 5,
        Cond::Vs => 6,
        Cond::Vc => 7,
        Cond::Hi => 8,
        Cond::Ls => 9,
        Cond::Ge => 10,
        Cond::Lt => 11,
        Cond::Gt => 12,
        Cond::Le => 13,
    }
}

/// CBZ/CBNZ base word (displacement 0). Cross-checked against the vendored
/// encoder's own `compare_branch` (`a64/encode.rs`): `sf<<31 |
/// 0b011010<<25 | op<<24 | Rt` — not re-derived from the ARM ARM
/// independently, read directly off the corpus-verified source.
fn cbz_base(r: Reg, nz: bool) -> u32 {
    let sf = match r.class {
        RegClass::X => 1u32,
        RegClass::W => 0u32,
        other => panic!("cbz/cbnz: expected an X or W register, got {other:?}"),
    };
    let op = nz as u32;
    (sf << 31) | (0b011010 << 25) | (op << 24) | (r.num as u32)
}

/// TBZ/TBNZ base word (displacement 0): `b5<<31 | 0b011011<<25 | op<<24 |
/// b40<<19 | Rt`. Not in the vendored encoder's supported mnemonic set (its
/// own "Scope" doc comment stops at cbz/cbnz) — hand-derived from the ARM
/// ARM's TBZ/TBNZ encoding (D3.2), so there is no corpus form to check this
/// against; `bcond_cbz_tbz_fields`'s B14 cases (`tests` module below) are
/// the direct evidence for it instead.
fn tbz_base(r: Reg, bit: u8, nz: bool) -> u32 {
    debug_assert!(bit < 64, "tbz/tbnz: bit index {bit} out of range (0..64)");
    let b5 = (bit >> 5) as u32;
    let b40 = (bit & 0x1F) as u32;
    let op = nz as u32;
    (b5 << 31) | (0b011011 << 25) | (op << 24) | (b40 << 19) | (r.num as u32)
}

impl Assembler for JasmAssembler {
    fn offset(&self) -> u32 {
        self.buf.len() as u32
    }

    fn emit(&mut self, mnemonic: &str, ops: &[Operand]) {
        let offset = self.offset();
        let encoded = encode::encode(mnemonic, ops)
            .unwrap_or_else(|e| panic!("JasmAssembler::emit(\"{mnemonic}\", {ops:?}): {e:#}"));
        debug_assert!(
            encoded.fixups.is_empty(),
            "JasmAssembler::emit(\"{mnemonic}\", {ops:?}) produced a Sym fixup (P6): the \
             compiler must never pass Operand::Sym to the vendored encoder — use this \
             trait's own label methods for intra-blob control flow instead"
        );
        debug_assert_eq!(
            encoded.bytes.len(),
            4,
            "AArch64 instructions are always exactly one word"
        );
        let word = u32::from_le_bytes(encoded.bytes[..4].try_into().unwrap());
        self.buf.extend_from_slice(&encoded.bytes);
        self.push_listing(offset, word, &format!("{mnemonic} {ops:?}"));
    }

    fn emit_u32(&mut self, word: u32) {
        let offset = self.offset();
        self.push_word(word);
        self.push_listing(offset, word, "<raw>");
    }

    fn new_label(&mut self) -> Label {
        let id = self.labels.len() as u32;
        self.labels.push(None);
        Label(id)
    }

    fn bind(&mut self, l: Label) {
        let off = self.offset();
        self.labels[l.0 as usize] = Some(off);
    }

    fn b(&mut self, l: Label) {
        self.queue_branch(0x1400_0000, l, BranchKind::B26, &format!("b L{}", l.0));
    }

    fn b_cond(&mut self, c: Cond, l: Label) {
        let word = 0x5400_0000 | cond_bits(c);
        self.queue_branch(word, l, BranchKind::B19, &format!("b.{c:?} L{}", l.0));
    }

    fn cbz(&mut self, r: Reg, l: Label) {
        let word = cbz_base(r, false);
        self.queue_branch(word, l, BranchKind::B19, &format!("cbz {r:?} L{}", l.0));
    }

    fn cbnz(&mut self, r: Reg, l: Label) {
        let word = cbz_base(r, true);
        self.queue_branch(word, l, BranchKind::B19, &format!("cbnz {r:?} L{}", l.0));
    }

    fn tbz(&mut self, r: Reg, bit: u8, l: Label) {
        let word = tbz_base(r, bit, false);
        self.queue_branch(
            word,
            l,
            BranchKind::B14,
            &format!("tbz {r:?} #{bit} L{}", l.0),
        );
    }

    fn tbnz(&mut self, r: Reg, bit: u8, l: Label) {
        let word = tbz_base(r, bit, true);
        self.queue_branch(
            word,
            l,
            BranchKind::B14,
            &format!("tbnz {r:?} #{bit} L{}", l.0),
        );
    }

    fn literal_u64(&mut self, v: u64, kind: Option<RelocKind>) -> LiteralId {
        let key = (v, kind);
        if let Some(&id) = self.lit_dedup.get(&key) {
            return id;
        }
        let id = LiteralId(self.literals.len() as u32);
        self.literals.push((v, kind));
        self.lit_dedup.insert(key, id);
        id
    }

    fn ldr_literal(&mut self, dst: Reg, lit: LiteralId) {
        debug_assert_eq!(
            dst.class,
            RegClass::X,
            "ldr_literal: pool words are 64-bit, dst must be X-class"
        );
        let offset = self.offset();
        // 64-bit LDR-literal, imm19 = 0 (D3.4) — patched once the pool's
        // layout is known, at `finish`.
        let word = 0x5800_0000 | (dst.num as u32);
        self.push_word(word);
        self.lit_loads.push((offset, lit));
        self.push_listing(offset, word, &format!("ldr {dst:?}, =lit{}", lit.0));
    }

    fn bl_patchable(&mut self, kind: RelocKind) -> u32 {
        let offset = self.offset();
        // `bl .` — imm26 = 0 is a self-branch, exactly the placeholder
        // CodeCache::patch_branch26 (S9 step 7) expects to overwrite.
        let word = 0x9400_0000;
        self.push_word(word);
        self.relocs.push(Reloc { offset, kind });
        self.push_listing(offset, word, &format!("bl . ; {kind:?} reloc"));
        offset
    }

    fn call_far(&mut self, target: LiteralId) {
        self.ldr_literal(xr(16), target);
        self.emit("blr", &[x(16)]);
    }

    fn finish(&mut self) -> CodeBlob {
        assert!(!self.finished, "JasmAssembler::finish called twice");
        self.finished = true;

        // Resolve every pending intra-blob branch (D3.2) — uniformly at
        // finish, whether the target was bound before or after the branch
        // that references it; backward and forward references go through
        // the identical path.
        for pb in &self.pending {
            let target = self.labels[pb.label.0 as usize]
                .unwrap_or_else(|| panic!("JasmAssembler::finish: unbound label L{}", pb.label.0));
            let disp = target as i64 - pb.at as i64;
            let at = pb.at as usize;
            let word = u32::from_le_bytes(self.buf[at..at + 4].try_into().unwrap());
            let patched = match pb.kind {
                BranchKind::B26 => word | (((disp >> 2) as u32) & 0x03FF_FFFF),
                BranchKind::B19 => word | ((((disp >> 2) as u32) & 0x7FFFF) << 5),
                BranchKind::B14 => word | ((((disp >> 2) as u32) & 0x3FFF) << 5),
            };
            self.buf[at..at + 4].copy_from_slice(&patched.to_le_bytes());
        }

        // Literal pool, 8-byte aligned, at the end (D3.3 — arm64.md §4.1:
        // GC rewrites pool words in place, never instruction bytes).
        while !self.buf.len().is_multiple_of(8) {
            self.buf.push(0);
        }
        let literal_off = self.buf.len() as u32;
        let mut relocs = std::mem::take(&mut self.relocs);
        for (i, &(v, kind)) in self.literals.iter().enumerate() {
            self.buf.extend_from_slice(&v.to_le_bytes());
            if let Some(kind) = kind {
                relocs.push(Reloc {
                    offset: literal_off + 8 * i as u32,
                    kind,
                });
            }
        }

        // Now that pool word offsets are known, resolve ldr-literal sites.
        for &(site, lit) in &self.lit_loads {
            let word_off = literal_off + 8 * lit.0;
            let disp = word_off as i64 - site as i64;
            let imm19 = (((disp >> 2) as u32) & 0x7FFFF) << 5;
            let at = site as usize;
            let word = u32::from_le_bytes(self.buf[at..at + 4].try_into().unwrap());
            self.buf[at..at + 4].copy_from_slice(&(word | imm19).to_le_bytes());
        }

        CodeBlob {
            code: std::mem::take(&mut self.buf),
            literal_off,
            relocs,
            listing: std::mem::take(&mut self.listing),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::assembler::{imm, mem};

    const NOP: u32 = 0xD503_201F;

    /// `emit` reproduces exactly what the vendored encoder itself produces —
    /// checked against known corpus lines for `add`/`sub`/`ldr` (scaled)/
    /// `movz` (tests_s09.md table row 3's own worked example, `add x0, x1,
    /// x2` = corpus line 1, plus three more forms actually present in
    /// `corpus/aarch64.tsv`). `ldur` (unscaled) has no corpus line with this
    /// exact register/offset combination, and the non-goals section is
    /// explicit that MACVM re-verifying encoder correctness beyond corpus
    /// replay is out of scope — so that form is checked against a direct
    /// call to the same `encode::encode` plumbing instead, which still
    /// proves this method wires up the *other* load/store code path
    /// (`ldst_ur`, not `ldst`) faithfully.
    #[test]
    fn emit_matches_corpus_forms() {
        let mut a = JasmAssembler::new();
        a.emit("add", &[x(0), x(1), x(2)]);
        assert_eq!(
            u32::from_le_bytes(a.finish().code[0..4].try_into().unwrap()),
            0x8b02_0020,
            "add x0, x1, x2 — corpus line 1"
        );

        let mut a = JasmAssembler::new();
        a.emit("sub", &[x(0), x(1), x(2)]);
        assert_eq!(
            u32::from_le_bytes(a.finish().code[0..4].try_into().unwrap()),
            0xcb02_0020,
            "sub x0, x1, x2"
        );

        let mut a = JasmAssembler::new();
        a.emit("ldr", &[x(0), mem(1, 8)]);
        assert_eq!(
            u32::from_le_bytes(a.finish().code[0..4].try_into().unwrap()),
            0xf940_0420,
            "ldr x0, [x1, #8] — scaled"
        );

        let mut a = JasmAssembler::new();
        a.emit("movz", &[x(0), imm(0x1234)]);
        assert_eq!(
            u32::from_le_bytes(a.finish().code[0..4].try_into().unwrap()),
            0xd282_4680,
            "movz x0, #0x1234"
        );

        let ops = [x(0), mem(1, 1)];
        let direct = encode::encode("ldur", &ops).expect("vendored encoder supports ldur");
        let mut a = JasmAssembler::new();
        a.emit("ldur", &ops);
        assert_eq!(
            &a.finish().code[0..4],
            &direct.bytes[..],
            "ldur x0, [x1, #1] — unscaled"
        );
    }

    /// P6: the compiler must never hand the vendored encoder an operand mix
    /// that yields a `Sym` fixup — that path is for `corpus_replay`'s text
    /// front end only. `emit` guards this in debug builds.
    #[test]
    #[should_panic(expected = "P6")]
    fn emit_rejects_sym_fixups() {
        let mut a = JasmAssembler::new();
        a.emit("b", &[Operand::Sym("somewhere".to_string())]);
    }

    /// D3.2: a backward branch (`bind` before `b`) patches imm26 to a
    /// negative word count. `bind(l)`, 4 filler words, then `b(l)` puts the
    /// branch 4 words after its own target — imm26 must equal −4, i.e.
    /// `0x3FFFFFC` once masked to 26 bits.
    #[test]
    fn label_backward_branch_bits() {
        let mut a = JasmAssembler::new();
        let l = a.new_label();
        a.bind(l);
        for _ in 0..4 {
            a.emit_u32(NOP);
        }
        a.b(l);
        let blob = a.finish();
        let word = u32::from_le_bytes(blob.code[16..20].try_into().unwrap());
        assert_eq!(word & 0x03FF_FFFF, 0x03FF_FFFC, "imm26 == -4, masked");
        assert_eq!(
            word & !0x03FF_FFFF,
            0x1400_0000,
            "base opcode bits untouched"
        );
    }

    /// Forward reference: the placeholder word is the raw, unpatched base
    /// opcode until `finish` resolves it against the now-bound label.
    #[test]
    fn label_forward_branch_patched() {
        let mut a = JasmAssembler::new();
        let l = a.new_label();
        a.b(l);
        assert_eq!(
            u32::from_le_bytes(a.buf[0..4].try_into().unwrap()),
            0x1400_0000,
            "unpatched placeholder"
        );
        a.bind(l); // target offset 4 — one word after the branch itself
        let blob = a.finish();
        assert_eq!(
            u32::from_le_bytes(blob.code[0..4].try_into().unwrap()),
            0x1400_0001,
            "forward distance of 1 word"
        );
    }

    /// D3.2's bit-insertion table: B19 lands at [23:5] (`b.cond`/`cbz`), B14
    /// at [18:5] (`tbz`/`tbnz`) — for both forward and backward references.
    /// Field-position mistakes are silent otherwise (two adjacent fields
    /// that are the wrong width still "work" for small test displacements).
    #[test]
    fn bcond_cbz_tbz_fields() {
        // B19, forward: b.eq 2 words ahead.
        let mut a = JasmAssembler::new();
        let l = a.new_label();
        a.b_cond(Cond::Eq, l);
        a.emit_u32(NOP);
        a.bind(l);
        let blob = a.finish();
        let word = u32::from_le_bytes(blob.code[0..4].try_into().unwrap());
        assert_eq!((word >> 5) & 0x7FFFF, 2, "imm19 at [23:5], forward 2 words");
        assert_eq!(word & !(0x7FFFF << 5), 0x5400_0000, "b.eq base bits");

        // B19, backward: cbz 3 words back.
        let mut a = JasmAssembler::new();
        let l = a.new_label();
        a.bind(l);
        for _ in 0..3 {
            a.emit_u32(NOP);
        }
        a.cbz(xr(0), l);
        let blob = a.finish();
        let word = u32::from_le_bytes(blob.code[12..16].try_into().unwrap());
        assert_eq!((word >> 5) & 0x7FFFF, 0x7_FFFD, "imm19 == -3, masked");

        // B14, forward: tbz bit 7 of x3, 5 words ahead.
        let mut a = JasmAssembler::new();
        let l = a.new_label();
        a.tbz(xr(3), 7, l);
        for _ in 0..4 {
            a.emit_u32(NOP);
        }
        a.bind(l);
        let blob = a.finish();
        let word = u32::from_le_bytes(blob.code[0..4].try_into().unwrap());
        assert_eq!((word >> 5) & 0x3FFF, 5, "imm14 at [18:5], forward 5 words");

        // B14, backward: tbnz bit 40 of x3, 2 words back — also checks the
        // bit-position fields (b40 at [23:19], b5 at [31]) survive the imm14
        // patch untouched.
        let mut a = JasmAssembler::new();
        let l = a.new_label();
        a.bind(l);
        for _ in 0..2 {
            a.emit_u32(NOP);
        }
        a.tbnz(xr(3), 40, l);
        let blob = a.finish();
        let word = u32::from_le_bytes(blob.code[8..12].try_into().unwrap());
        assert_eq!((word >> 5) & 0x3FFF, 0x3FFE, "imm14 == -2, masked");
        assert_eq!((word >> 19) & 0x1F, 40 & 0x1F, "b40 field");
        assert_eq!((word >> 31) & 1, 1, "b5 field set for bit >= 32");
    }

    /// D3.4's hand encoding, not corpus-covered: a pool word 16 bytes ahead
    /// of an `ldr x0, =lit` site encodes to exactly `0x58000080`.
    #[test]
    fn ldr_literal_encoding() {
        let mut a = JasmAssembler::new();
        let lit = a.literal_u64(0xDEAD_BEEF, None);
        a.ldr_literal(xr(0), lit);
        for _ in 0..3 {
            a.emit_u32(NOP); // pad code to 16 bytes so the pool starts at +16
        }
        let blob = a.finish();
        assert_eq!(blob.literal_off, 16);
        assert_eq!(
            u32::from_le_bytes(blob.code[0..4].try_into().unwrap()),
            0x5800_0080
        );
    }

    /// D3.3 + P10: the pool is 8-aligned at `literal_off`, entries dedup by
    /// `(value, kind)` — never by value alone.
    #[test]
    fn finish_pool_layout() {
        let mut a = JasmAssembler::new();
        a.emit_u32(NOP); // 4 bytes of code before the pool
        let l1 = a.literal_u64(100, Some(RelocKind::Oop));
        let l2 = a.literal_u64(200, None);
        let l3 = a.literal_u64(100, Some(RelocKind::Oop)); // dedup hit: same (value, kind)
        let l4 = a.literal_u64(100, None); // NOT a dedup hit: same value, different kind
        assert_eq!(l1, l3, "dedup by (value, kind)");
        assert_ne!(l1, l4, "same value, different kind must not dedup (P10)");

        let blob = a.finish();
        assert_eq!(blob.literal_off % 8, 0, "pool is 8-aligned");
        assert_eq!(blob.literal_off, 8, "code padded from 4 to 8 bytes");
        assert_eq!(
            blob.code.len(),
            blob.literal_off as usize + 3 * 8,
            "3 distinct pool entries: l1(=l3), l2, l4"
        );

        let oop_relocs: Vec<_> = blob
            .relocs
            .iter()
            .filter(|r| r.kind == RelocKind::Oop)
            .collect();
        assert_eq!(oop_relocs.len(), 1, "exactly one Oop reloc");
        assert_eq!(oop_relocs[0].offset, blob.literal_off + 8 * l1.0);

        let word_at = |id: LiteralId| -> u64 {
            let off = (blob.literal_off + 8 * id.0) as usize;
            u64::from_le_bytes(blob.code[off..off + 8].try_into().unwrap())
        };
        assert_eq!(word_at(l1), 100);
        assert_eq!(word_at(l2), 200);
        assert_eq!(word_at(l4), 100);
    }

    /// The listing line format is pinned — S10's disassembly goldens depend
    /// on it byte-for-byte.
    #[test]
    fn listing_format_stable() {
        let mut a = JasmAssembler::new();
        a.emit("add", &[x(0), x(1), x(2)]);
        let blob = a.finish();
        let expected = format!(
            "{:06x}  {:08x}  add {:?}",
            0u32,
            0x8b02_0020u32,
            [x(0), x(1), x(2)].as_slice()
        );
        assert_eq!(blob.listing[0], expected);
    }

    /// `CodeBlob.relocs` offsets index directly into `code` — S10 consumes
    /// these blindly, with no rebasing of its own.
    #[test]
    fn reloc_offsets_blob_relative() {
        let mut a = JasmAssembler::new();
        let l1 = a.literal_u64(0x1111_1111_1111_1111, Some(RelocKind::Oop));
        let l2 = a.literal_u64(0x2222_2222_2222_2222, Some(RelocKind::RuntimeAddr));
        a.ldr_literal(xr(0), l1);
        a.ldr_literal(xr(1), l2);
        let blob = a.finish();

        assert_eq!(blob.relocs.len(), 2);
        let by_kind = |k: RelocKind| blob.relocs.iter().find(|r| r.kind == k).unwrap();
        let oop = by_kind(RelocKind::Oop);
        let addr = by_kind(RelocKind::RuntimeAddr);
        assert_eq!(oop.offset, blob.literal_off);
        assert_eq!(addr.offset, blob.literal_off + 8);
        assert_eq!(
            u64::from_le_bytes(
                blob.code[oop.offset as usize..oop.offset as usize + 8]
                    .try_into()
                    .unwrap()
            ),
            0x1111_1111_1111_1111
        );
        assert_eq!(
            u64::from_le_bytes(
                blob.code[addr.offset as usize..addr.offset as usize + 8]
                    .try_into()
                    .unwrap()
            ),
            0x2222_2222_2222_2222
        );
    }

    /// GC writes pool words as `u64`s — `literal_off` must land 8-aligned
    /// even when the code preceding it is only word- (4-byte-) aligned.
    #[test]
    fn pool_word_alignment() {
        let mut a = JasmAssembler::new();
        a.emit_u32(NOP); // 4 bytes of code -> len % 8 == 4
        a.literal_u64(42, None);
        let blob = a.finish();
        assert_eq!(blob.literal_off % 8, 0);
        assert_eq!(blob.literal_off, 8);
    }

    /// Compiler-bug loudness (CONVENTIONS §4): an unbound label is a bug in
    /// the caller, not a silently-wrong branch.
    #[test]
    #[should_panic(expected = "unbound label")]
    fn unbound_label_panics() {
        let mut a = JasmAssembler::new();
        let l = a.new_label();
        a.b(l);
        a.finish();
    }

    /// Compiler-bug loudness: an unknown mnemonic is never a guest-visible
    /// condition.
    #[test]
    #[should_panic(expected = "frobnicate")]
    fn emit_bad_mnemonic_panics() {
        let mut a = JasmAssembler::new();
        a.emit("frobnicate", &[]);
    }

    /// Calling `finish` on an already-consumed assembler is a loud bug, not
    /// a silent empty blob.
    #[test]
    #[should_panic(expected = "finish called twice")]
    fn finish_twice_panics() {
        let mut a = JasmAssembler::new();
        a.finish();
        a.finish();
    }
}
