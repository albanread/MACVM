//! `HomeRef` — a closure's packed reference to its home method activation
//! (SPEC §5.4, S4): `(process id, frame-stack index, frame serial)`, packed
//! into one smi. Lives in its own small module rather than `oops::layout`
//! (which stays pure bit/offset constants, CONVENTIONS §2) — the same split
//! `oops::mark` uses for the mark word's own pack/unpack.

use super::layout::{
    HOME_REF_ALL_MASK, HOME_REF_FP_MAX, HOME_REF_FP_SHIFT, HOME_REF_PROC_SHIFT,
    HOME_REF_SERIAL_SHIFT,
};
use super::smi::SmallInt;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct HomeRef {
    pub proc: u8,
    pub serial: u32,
    pub fp: usize,
}

/// Packs `h` into a smi. The 62-bit field (`proc:8 | serial:32 | fp:22`)
/// exactly spans a smi's two's-complement value range — a field
/// combination whose top bit (proc's high bit) is set legitimately packs
/// to a *negative* smi; sign-extending the 62-bit pattern (rather than
/// treating it as an unsigned value that must fit under `SMI_MAX`) is what
/// makes every field combination representable.
pub fn pack_home_ref(h: HomeRef) -> SmallInt {
    debug_assert!(
        h.fp <= HOME_REF_FP_MAX,
        "pack_home_ref: fp {} exceeds 22 bits",
        h.fp
    );
    let raw: u64 = ((h.proc as u64) << HOME_REF_PROC_SHIFT)
        | ((h.serial as u64) << HOME_REF_SERIAL_SHIFT)
        | ((h.fp as u64) << HOME_REF_FP_SHIFT);
    // Sign-extend the low 62 bits into a full i64: shift the field to the
    // top of the word, then arithmetic-shift back down.
    let signed = ((raw << 2) as i64) >> 2;
    SmallInt::new(signed)
}

pub fn unpack_home_ref(s: SmallInt) -> HomeRef {
    let raw = (s.value() as u64) & HOME_REF_ALL_MASK;
    HomeRef {
        proc: (raw >> HOME_REF_PROC_SHIFT) as u8,
        serial: (raw >> HOME_REF_SERIAL_SHIFT) as u32,
        fp: (raw >> HOME_REF_FP_SHIFT) as usize & HOME_REF_FP_MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_ref_roundtrip() {
        let cases = [
            HomeRef {
                proc: 0,
                serial: 0,
                fp: 0,
            },
            HomeRef {
                proc: 255,
                serial: u32::MAX,
                fp: HOME_REF_FP_MAX,
            },
            HomeRef {
                proc: 255,
                serial: 0,
                fp: 0,
            },
            HomeRef {
                proc: 0,
                serial: u32::MAX,
                fp: 0,
            },
            HomeRef {
                proc: 0,
                serial: 0,
                fp: HOME_REF_FP_MAX,
            },
            HomeRef {
                proc: 1,
                serial: 12345,
                fp: 6789,
            },
            HomeRef {
                proc: 128,
                serial: 1,
                fp: 1,
            },
        ];
        for h in cases {
            let packed = pack_home_ref(h);
            let unpacked = unpack_home_ref(packed);
            assert_eq!(unpacked, h, "roundtrip failed for {h:?}");
        }
    }
}
