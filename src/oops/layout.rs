//! Bit/offset constants for the tagged-oop and mark-word representation.
//!
//! Single source of truth (CONVENTIONS §2): nothing outside `oops::layout`
//! may name a tag value, shift, or mask directly. SPEC §2.1 (tags), §2.2
//! (mark word).

// --- oop tag scheme (SPEC §2.1) --------------------------------------------

pub const TAG_BITS: u32 = 2;
pub const TAG_MASK: u64 = 0b11;
pub const INT_TAG: u64 = 0b00; // smi
pub const MEM_TAG: u64 = 0b01; // heap oop; address = word - MEM_TAG
pub const RESERVED_TAG: u64 = 0b10; // future immediate Character; illegal in v1
pub const MARK_TAG: u64 = 0b11; // only as header word 0

pub const SMI_SHIFT: u32 = 2;
pub const SMI_BITS: u32 = 62;
pub const SMI_MAX: i64 = (1i64 << 61) - 1; //  2_305_843_009_213_693_951
pub const SMI_MIN: i64 = -(1i64 << 61); // -2_305_843_009_213_693_952

// --- heap layout (SPEC §2.2) ------------------------------------------------

pub const WORD_SIZE: usize = 8;
pub const ALLOC_ALIGN: usize = 8; // SPEC §2.1: objects 8-byte aligned (NOT 16)
pub const MARK_OFFSET: usize = 0; // header word 0
pub const KLASS_OFFSET: usize = 8; // header word 1
pub const BODY_OFFSET: usize = 16; // first body word
pub const HEADER_WORDS: usize = 2;

// --- mark word fields (SPEC §2.2) — bit positions, low to high -------------

pub const MARK_SENTINEL_SHIFT: u32 = 2; // always 1 in a real mark
pub const MARK_NEAR_DEATH_SHIFT: u32 = 3;
pub const MARK_TAGGED_CONTENTS_SHIFT: u32 = 4;
pub const MARK_AGE_SHIFT: u32 = 5; // bits 11:5
pub const MARK_AGE_BITS: u32 = 7;
pub const MARK_AGE_MAX: u8 = 127;
pub const MARK_HASH_SHIFT: u32 = 12; // bits 43:12
pub const MARK_HASH_BITS: u32 = 32;
// bits 63:44 reserved, must stay zero in v1.

pub const MARK_SENTINEL_MASK: u64 = 1 << MARK_SENTINEL_SHIFT;
pub const MARK_NEAR_DEATH_MASK: u64 = 1 << MARK_NEAR_DEATH_SHIFT;
pub const MARK_TAGGED_CONTENTS_MASK: u64 = 1 << MARK_TAGGED_CONTENTS_SHIFT;
pub const MARK_AGE_MASK: u64 = ((1u64 << MARK_AGE_BITS) - 1) << MARK_AGE_SHIFT;
pub const MARK_HASH_MASK: u64 = ((1u64 << MARK_HASH_BITS) - 1) << MARK_HASH_SHIFT;

/// Bit 44: the full-GC mark bit (SPEC §2.2, amendment A: added at S8 review).
/// Always clear outside a collection.
pub const MARK_GC_MARK_SHIFT: u32 = 44;
pub const MARK_GC_MARK_MASK: u64 = 1 << MARK_GC_MARK_SHIFT;

/// The mark word of a freshly allocated object: tag=MARK_TAG, sentinel=1,
/// every other field zero. `0b111` = 7.
pub const MARK_PRISTINE: u64 = MARK_TAG | MARK_SENTINEL_MASK;

// --- klass body layout (SPEC §2.4), in declaration order -------------------
// `*_INDEX` constants are body-word indexes (word 0 = byte offset
// BODY_OFFSET); byte offset = BODY_OFFSET + 8*index.

pub const KLASS_FORMAT_INDEX: usize = 0; // smi: format enum + flags
pub const KLASS_NON_INDEXABLE_SIZE_INDEX: usize = 1; // smi: words incl. header
pub const KLASS_SUPERCLASS_INDEX: usize = 2; // klassOop | nil
pub const KLASS_METHODS_INDEX: usize = 3; // MethodDictionary | nil (nil until S3)
pub const KLASS_NAME_INDEX: usize = 4; // Symbol | nil (during genesis)
pub const KLASS_INST_VAR_NAMES_INDEX: usize = 5; // Array | nil
pub const KLASS_CLASS_VARS_INDEX: usize = 6; // Array | nil
pub const KLASS_MIXIN_INDEX: usize = 7; // always nil in v1 (SPEC §2.4 Δ)
pub const KLASS_BODY_WORDS: usize = 8;
pub const KLASS_SIZE_WORDS: usize = HEADER_WORDS + KLASS_BODY_WORDS; // 10

// Format smi encoding: bits 7:0 = Format discriminant,
// bit 8 = has_untagged_contents (SPEC §2.4 "format enum + flags").
pub const FORMAT_KIND_MASK: i64 = 0xFF;
pub const FORMAT_UNTAGGED_BIT: i64 = 1 << 8;

// --- CompiledMethod body layout (SPEC §4.4) --------------------------------

pub const METHOD_SELECTOR_INDEX: usize = 0; // Symbol
pub const METHOD_HOLDER_INDEX: usize = 1; // klassOop | nil (nil until S3 install)
pub const METHOD_FLAGS_INDEX: usize = 2; // smi, packing below
pub const METHOD_PRIMITIVE_INDEX: usize = 3; // smi, 0 = none
pub const METHOD_COUNTERS_INDEX: usize = 4; // smi (invocation:16 | reserved)
pub const METHOD_LITERALS_INDEX: usize = 5; // Array
pub const METHOD_ICS_INDEX: usize = 6; // Array (stride 4, SPEC §4.3)
pub const METHOD_NAMED_WORDS: usize = 7; // => klass nis = 9
pub const METHOD_SIZE_INDEX: usize = 7; // smi: bytecode byte count
/// Absolute byte offset of the first bytecode byte.
pub const METHOD_BYTECODE_BYTE_OFFSET: usize = BODY_OFFSET + 8 * (METHOD_SIZE_INDEX + 1); // 80

// flags packing — SPEC §4.4 "argc:4 | ntemps:8 | has_ctx:1 | is_block:1 | prim_fails:1"
pub const METHOD_FLAGS_ARGC_SHIFT: u32 = 0;
pub const METHOD_FLAGS_ARGC_BITS: u32 = 4;
pub const METHOD_FLAGS_NTEMPS_SHIFT: u32 = 4;
pub const METHOD_FLAGS_NTEMPS_BITS: u32 = 8;
pub const METHOD_FLAGS_HAS_CTX_SHIFT: u32 = 12;
pub const METHOD_FLAGS_IS_BLOCK_SHIFT: u32 = 13;
pub const METHOD_FLAGS_PRIM_FAILS_SHIFT: u32 = 14;

pub const METHOD_FLAGS_ARGC_MASK: i64 =
    ((1i64 << METHOD_FLAGS_ARGC_BITS) - 1) << METHOD_FLAGS_ARGC_SHIFT;
pub const METHOD_FLAGS_NTEMPS_MASK: i64 =
    ((1i64 << METHOD_FLAGS_NTEMPS_BITS) - 1) << METHOD_FLAGS_NTEMPS_SHIFT;
pub const METHOD_FLAGS_HAS_CTX_MASK: i64 = 1 << METHOD_FLAGS_HAS_CTX_SHIFT;
pub const METHOD_FLAGS_IS_BLOCK_MASK: i64 = 1 << METHOD_FLAGS_IS_BLOCK_SHIFT;
pub const METHOD_FLAGS_PRIM_FAILS_MASK: i64 = 1 << METHOD_FLAGS_PRIM_FAILS_SHIFT;

pub const METHOD_ARGC_MAX: usize = (1 << METHOD_FLAGS_ARGC_BITS) - 1; // 15
pub const METHOD_NTEMPS_MAX: usize = (1 << METHOD_FLAGS_NTEMPS_BITS) - 1; // 255

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_relations() {
        assert_eq!(MARK_HASH_SHIFT, MARK_AGE_SHIFT + MARK_AGE_BITS);
        assert_eq!(MARK_AGE_SHIFT, MARK_TAGGED_CONTENTS_SHIFT + 1);
        assert_eq!(SMI_MAX, -(SMI_MIN + 1));
        assert_eq!(SMI_BITS, 64 - TAG_BITS);
    }

    #[test]
    fn layout_values_pinned() {
        assert_eq!(INT_TAG, 0);
        assert_eq!(MEM_TAG, 1);
        assert_eq!(RESERVED_TAG, 2);
        assert_eq!(MARK_TAG, 3);
        assert_eq!(SMI_MAX, 2_305_843_009_213_693_951);
        assert_eq!(ALLOC_ALIGN, 8);
        assert_eq!(BODY_OFFSET, 16);
    }

    #[test]
    fn mask_disjointness() {
        assert_eq!(MARK_AGE_MASK & MARK_HASH_MASK, 0);
        let flag_bits = MARK_SENTINEL_MASK | MARK_NEAR_DEATH_MASK | MARK_TAGGED_CONTENTS_MASK;
        assert_eq!(MARK_AGE_MASK & flag_bits, 0);
        assert_eq!(MARK_HASH_MASK & flag_bits, 0);
        assert_eq!(MARK_AGE_MASK & TAG_MASK, 0);
        assert_eq!(MARK_HASH_MASK & TAG_MASK, 0);
        assert_eq!(flag_bits & TAG_MASK, 0);
        // fields partition bits 43:0 with no overlap and leave 63:44 reserved
        // (the gc_mark bit at 44 is the one exception, checked separately).
        let used = MARK_AGE_MASK | MARK_HASH_MASK | TAG_MASK | flag_bits;
        assert_eq!(used >> 44, 0);
        assert_eq!(MARK_GC_MARK_MASK & used, 0);
    }
}
