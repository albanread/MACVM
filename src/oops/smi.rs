//! `SmallInt` — the tagged small-integer immediate (SPEC §2.1) plus
//! overflow-checked arithmetic. `//` and `\\` are Smalltalk's **floored**
//! division/modulo, not Rust's truncating `/`/`%`.

use super::layout::{SMI_MAX, SMI_MIN, SMI_SHIFT};
use super::Oop;

#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct SmallInt(Oop);

impl SmallInt {
    pub const MAX: i64 = SMI_MAX;
    pub const MIN: i64 = SMI_MIN;

    /// `None` if `v` is outside `[SMI_MIN, SMI_MAX]`.
    #[inline]
    pub fn try_new(v: i64) -> Option<SmallInt> {
        if !(SMI_MIN..=SMI_MAX).contains(&v) {
            return None;
        }
        // INT_TAG == 0, so no OR is needed: the tagged word is exactly v * 4.
        Some(SmallInt(Oop::from_raw_unchecked((v as u64) << SMI_SHIFT)))
    }

    /// Panics (a VM bug, not a guest error) if `v` is out of range — for
    /// VM-internal constants only.
    #[inline]
    pub fn new(v: i64) -> SmallInt {
        Self::try_new(v).unwrap_or_else(|| panic!("SmallInt::new: {v} out of smi range"))
    }

    #[inline]
    pub fn value(self) -> i64 {
        // Arithmetic (sign-preserving) shift, NOT logical — negative smis
        // must sign-extend correctly.
        (self.0.raw() as i64) >> SMI_SHIFT
    }

    #[inline]
    pub fn oop(self) -> Oop {
        self.0
    }

    #[inline]
    pub fn try_from(o: Oop) -> Option<SmallInt> {
        if o.is_smi() {
            Some(SmallInt(o))
        } else {
            None
        }
    }

    #[inline]
    pub fn checked_add(self, rhs: SmallInt) -> Option<SmallInt> {
        self.value()
            .checked_add(rhs.value())
            .and_then(Self::try_new)
    }

    #[inline]
    pub fn checked_sub(self, rhs: SmallInt) -> Option<SmallInt> {
        self.value()
            .checked_sub(rhs.value())
            .and_then(Self::try_new)
    }

    #[inline]
    pub fn checked_mul(self, rhs: SmallInt) -> Option<SmallInt> {
        // Untag-first: tagged*tagged would be 16*v_l*v_r (wrong scale), so
        // multiply the untagged values and re-tag/range-check the result.
        self.value()
            .checked_mul(rhs.value())
            .and_then(Self::try_new)
    }

    /// Smalltalk `//` — floored division (rounds toward negative infinity).
    #[inline]
    pub fn checked_div(self, rhs: SmallInt) -> Option<SmallInt> {
        let (a, b) = (self.value(), rhs.value());
        if b == 0 {
            return None;
        }
        Self::try_new(floor_div(a, b))
    }

    /// Smalltalk `\\` — floored modulo (result has the sign of the divisor).
    #[inline]
    pub fn checked_rem(self, rhs: SmallInt) -> Option<SmallInt> {
        let (a, b) = (self.value(), rhs.value());
        if b == 0 {
            return None;
        }
        Self::try_new(floor_mod(a, b))
    }

    /// Left shift only; a negative `by` (mapped to right-shift) is an S3
    /// primitive-layer decision, not this type's concern.
    #[inline]
    pub fn checked_shl(self, by: u32) -> Option<SmallInt> {
        if by >= 64 {
            return None;
        }
        let shifted = (self.value() as i128) << by;
        if shifted > SMI_MAX as i128 || shifted < SMI_MIN as i128 {
            return None;
        }
        Self::try_new(shifted as i64)
    }
}

/// `a` and `b` truncating-divided/remaindered, then adjusted toward negative
/// infinity when the truncating remainder's sign disagrees with `b`'s sign.
/// `b != 0` is the caller's responsibility (checked above).
#[inline]
fn floor_div(a: i64, b: i64) -> i64 {
    let q = a / b;
    let r = a % b;
    if r != 0 && (r < 0) != (b < 0) {
        q - 1
    } else {
        q
    }
}

#[inline]
fn floor_mod(a: i64, b: i64) -> i64 {
    let r = a % b;
    if r != 0 && (r < 0) != (b < 0) {
        r + b
    } else {
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oops::layout::{INT_TAG, MEM_TAG};

    #[test]
    fn smi_zero_and_small() {
        assert_eq!(SmallInt::new(0).oop().raw(), 0);
        assert_eq!(SmallInt::new(1).oop().raw(), 4);
        assert_eq!(SmallInt::new(-1).oop().raw(), 0xFFFF_FFFF_FFFF_FFFC);
    }

    #[test]
    fn smi_roundtrip_edges() {
        for v in [
            0,
            1,
            -1,
            42,
            -42,
            SMI_MAX,
            SMI_MIN,
            SMI_MAX - 1,
            SMI_MIN + 1,
        ] {
            assert_eq!(SmallInt::new(v).value(), v, "roundtrip failed for {v}");
        }
    }

    #[test]
    fn smi_try_new_range() {
        assert!(SmallInt::try_new(SMI_MAX).is_some());
        assert!(SmallInt::try_new(SMI_MIN).is_some());
        assert!(SmallInt::try_new(SMI_MAX + 1).is_none());
        assert!(SmallInt::try_new(SMI_MIN - 1).is_none());
        assert!(SmallInt::try_new(i64::MAX).is_none());
    }

    #[test]
    fn smi_try_from_oop() {
        let o = Oop::from_raw_unchecked((1u64) << SMI_SHIFT);
        assert_eq!(o.tag(), INT_TAG);
        let si = SmallInt::try_from(o).expect("smi tag");
        assert_eq!(si.value(), 1);

        let mem = Oop::from_raw(0x1000 + MEM_TAG);
        assert!(SmallInt::try_from(mem).is_none());
    }

    #[test]
    fn smi_checked_add_overflow() {
        assert!(SmallInt::new(SMI_MAX)
            .checked_add(SmallInt::new(1))
            .is_none());
        assert!(SmallInt::new(SMI_MIN)
            .checked_add(SmallInt::new(-1))
            .is_none());
        assert_eq!(
            SmallInt::new(SMI_MAX)
                .checked_add(SmallInt::new(SMI_MIN))
                .map(SmallInt::value),
            Some(-1)
        );
    }

    #[test]
    fn smi_checked_sub_overflow() {
        assert!(SmallInt::new(SMI_MIN)
            .checked_sub(SmallInt::new(1))
            .is_none());
        // -SMI_MIN > SMI_MAX: the range is asymmetric.
        assert!(SmallInt::new(0)
            .checked_sub(SmallInt::new(SMI_MIN))
            .is_none());
    }

    #[test]
    fn smi_checked_mul() {
        assert_eq!(
            SmallInt::new(3)
                .checked_mul(SmallInt::new(-7))
                .map(SmallInt::value),
            Some(-21)
        );
        assert!(SmallInt::new(SMI_MAX)
            .checked_mul(SmallInt::new(2))
            .is_none());
        assert!(SmallInt::new(SMI_MIN)
            .checked_mul(SmallInt::new(-1))
            .is_none());
        // 2^31 * 2^31 = 2^62, one bit past SMI_MAX's 2^61-1.
        assert!(SmallInt::new(1 << 31)
            .checked_mul(SmallInt::new(1 << 31))
            .is_none());
    }

    #[test]
    fn smi_div_floored() {
        assert_eq!(
            SmallInt::new(-7)
                .checked_div(SmallInt::new(2))
                .map(SmallInt::value),
            Some(-4)
        );
        assert_eq!(
            SmallInt::new(7)
                .checked_div(SmallInt::new(-2))
                .map(SmallInt::value),
            Some(-4)
        );
        assert_eq!(
            SmallInt::new(-7)
                .checked_rem(SmallInt::new(2))
                .map(SmallInt::value),
            Some(1)
        );
        assert_eq!(
            SmallInt::new(7)
                .checked_rem(SmallInt::new(-2))
                .map(SmallInt::value),
            Some(-1)
        );
    }

    #[test]
    fn smi_div_edge() {
        assert!(SmallInt::new(1).checked_div(SmallInt::new(0)).is_none());
        // SMI_MIN / -1 == 2^61, which is one past SMI_MAX — overflows the
        // smi range even though it fits i64.
        assert!(SmallInt::new(SMI_MIN)
            .checked_div(SmallInt::new(-1))
            .is_none());
        assert_eq!(
            SmallInt::new(SMI_MIN)
                .checked_rem(SmallInt::new(-1))
                .map(SmallInt::value),
            Some(0)
        );
    }

    #[test]
    fn smi_checked_shl() {
        assert_eq!(
            SmallInt::new(1).checked_shl(60).map(SmallInt::value),
            Some(1i64 << 60)
        );
        assert!(SmallInt::new(1).checked_shl(61).is_none());
        assert_eq!(
            SmallInt::new(-1).checked_shl(61).map(SmallInt::value),
            Some(SMI_MIN)
        );
    }

    #[test]
    fn smi_eq_is_value_eq() {
        assert!(SmallInt::new(5) == SmallInt::new(5));
        assert!(SmallInt::new(5) != SmallInt::new(6));
        assert!(SmallInt::new(5).oop() == SmallInt::new(5).oop());
    }

    #[test]
    fn smi_boundary_sweep() {
        let mut values: Vec<i64> = Vec::new();
        values.extend(SMI_MIN..=SMI_MIN + 1000);
        values.extend(-1000..=1000);
        values.extend(SMI_MAX - 1000..=SMI_MAX);

        for v in values {
            assert_eq!(SmallInt::try_new(v).unwrap().value(), v);

            let add_oracle = v.checked_add(1).filter(|r| *r >= SMI_MIN && *r <= SMI_MAX);
            let add_actual = SmallInt::new(v)
                .checked_add(SmallInt::new(1))
                .map(SmallInt::value);
            assert_eq!(add_actual, add_oracle, "add mismatch at {v}");

            let sub_oracle = v.checked_sub(1).filter(|r| *r >= SMI_MIN && *r <= SMI_MAX);
            let sub_actual = SmallInt::new(v)
                .checked_sub(SmallInt::new(1))
                .map(SmallInt::value);
            assert_eq!(sub_actual, sub_oracle, "sub mismatch at {v}");

            let mul_oracle = v.checked_mul(3).filter(|r| *r >= SMI_MIN && *r <= SMI_MAX);
            let mul_actual = SmallInt::new(v)
                .checked_mul(SmallInt::new(3))
                .map(SmallInt::value);
            assert_eq!(mul_actual, mul_oracle, "mul mismatch at {v}");

            // div_euclid IS Smalltalk's // for a positive divisor (7).
            let div_oracle = v.div_euclid(7);
            let div_actual = SmallInt::new(v)
                .checked_div(SmallInt::new(7))
                .map(SmallInt::value);
            assert_eq!(div_actual, Some(div_oracle), "div mismatch at {v}");
        }
    }
}
