//! The interning symbol table (SPEC §3.1): an open-addressed set of Symbol
//! oops. Strong in v1 (weak symbol table is S22). Bucket storage only —
//! the probe/insert/rehash algorithm lives on `Universe::intern`
//! (`universe.rs`), which is what needs `eden`/`nil_obj`/`symbol_klass`
//! alongside the table.
//!
//! Kept as a plain `Vec` of buckets (not a `HashMap<Vec<u8>, Oop>`): S7
//! scans and updates this table in place as a GC root set, and a `HashMap`
//! keyed by copied byte content would mean duplicated key storage and a
//! separate root-update path — a needless headache this shape avoids.

use crate::oops::Oop;

pub struct SymbolTable {
    pub(crate) buckets: Vec<Option<Oop>>,
    pub(crate) count: usize,
}

impl SymbolTable {
    pub fn with_capacity(cap_pow2: usize) -> SymbolTable {
        debug_assert!(
            cap_pow2.is_power_of_two(),
            "SymbolTable::with_capacity: {cap_pow2} is not a power of two"
        );
        SymbolTable {
            buckets: vec![None; cap_pow2],
            count: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.buckets.len()
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// FNV-1a 64-bit content hash.
    pub(crate) fn content_hash(bytes: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_distinct() {
        assert_ne!(
            SymbolTable::content_hash(b"foo"),
            SymbolTable::content_hash(b"bar")
        );
        assert_eq!(
            SymbolTable::content_hash(b"foo"),
            SymbolTable::content_hash(b"foo")
        );
        // Empty input is legal and stable.
        assert_eq!(
            SymbolTable::content_hash(b""),
            SymbolTable::content_hash(b"")
        );
    }
}
