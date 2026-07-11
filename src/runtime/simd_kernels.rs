//! SIMD level 2 — explicit hand-written NEON bulk kernels (`docs/SIMD.md`
//! Part E), the compute behind `FloatArray`'s `+@`/`sum`/`dot:` primitives.
//!
//! These are DELIBERATE `core::arch::aarch64` intrinsics — a `<primitive:>`
//! bulk op uses the hardware on purpose, NOT a scalar loop left to rustc/LLVM
//! to maybe vectorize. This is the one place in `runtime` that opts back into
//! `unsafe` (like `memory` / `codegen`), confined here to the NEON intrinsics
//! and their in-bounds slice loads; the module doc + each `// SAFETY:` note
//! carry the justification (`CONVENTIONS §1`).
//!
//! Each kernel is a 2-lane (`float64x2_t`) stream plus a scalar tail for the
//! odd final element. The reductions (`pairwise_sum`/`pairwise_dot`) fold in a
//! DEFINED pairwise order — a `float64x2_t` accumulator combined by a
//! horizontal add — which is deterministic (so the interpreter and the JIT
//! agree, since each is a single primitive call) but is NOT the scalar
//! left-to-right fold, and differs from it in the low bits (docs/SIMD.md
//! Part D — the user-chosen "fast NEON reduction"). `FloatArray>>sequentialSum`
//! is the exact scalar fold for code that needs bit-parity.
//!
//! The `#[cfg(not(target_arch = "aarch64"))]` arms are byte-exact scalar
//! mirrors (same 2-accumulator order) so the VM still builds off-target; the
//! shipping target is aarch64, where the intrinsic arms run.
#![allow(unsafe_code)]

/// Elementwise sum `c = a + b` — explicit `fadd v.2d` stream + scalar tail.
/// Per-lane bit-identical to a scalar Double add (elementwise discipline, §B4).
/// `a`, `b`, `c` must share length.
#[cfg(target_arch = "aarch64")]
pub fn neon_add(a: &[f64], b: &[f64], c: &mut [f64]) {
    use core::arch::aarch64::*;
    let n = a.len();
    debug_assert!(b.len() == n && c.len() == n, "neon_add: length mismatch");
    // SAFETY: NEON is baseline on aarch64. Every `vld1q_f64`/`vst1q_f64`
    // touches lanes [i, i+2) with i+2 <= n and a/b/c all length n, so all
    // 16-byte accesses are in-bounds of their slices.
    unsafe {
        let mut i = 0;
        while i + 2 <= n {
            let va = vld1q_f64(a.as_ptr().add(i));
            let vb = vld1q_f64(b.as_ptr().add(i));
            vst1q_f64(c.as_mut_ptr().add(i), vaddq_f64(va, vb));
            i += 2;
        }
        while i < n {
            c[i] = a[i] + b[i];
            i += 1;
        }
    }
}

/// Fast pairwise NEON reduction of `a` to its sum (docs/SIMD.md Part D).
#[cfg(target_arch = "aarch64")]
pub fn pairwise_sum(a: &[f64]) -> f64 {
    use core::arch::aarch64::*;
    let n = a.len();
    // SAFETY: NEON is baseline on aarch64; each `vld1q_f64` reads lanes
    // [i, i+2) with i+2 <= n, in-bounds of `a`.
    unsafe {
        let mut acc = vdupq_n_f64(0.0);
        let mut i = 0;
        while i + 2 <= n {
            acc = vaddq_f64(acc, vld1q_f64(a.as_ptr().add(i)));
            i += 2;
        }
        let mut s = vaddvq_f64(acc); // horizontal add: lane0 + lane1
        while i < n {
            s += a[i];
            i += 1;
        }
        s
    }
}

/// Fast pairwise NEON dot product of `a`·`b` (docs/SIMD.md Part D). `fmla`-free
/// (`vmulq` then `vaddq`, so no FMA rounding delta): products are per-lane
/// exact; only the SUM is the reordered reduction. `a`, `b` share length.
#[cfg(target_arch = "aarch64")]
pub fn pairwise_dot(a: &[f64], b: &[f64]) -> f64 {
    use core::arch::aarch64::*;
    let n = a.len();
    debug_assert_eq!(b.len(), n, "pairwise_dot: length mismatch");
    // SAFETY: NEON is baseline on aarch64; each `vld1q_f64` reads lanes
    // [i, i+2) with i+2 <= n, in-bounds of both `a` and `b` (equal length).
    unsafe {
        let mut acc = vdupq_n_f64(0.0);
        let mut i = 0;
        while i + 2 <= n {
            let prod = vmulq_f64(vld1q_f64(a.as_ptr().add(i)), vld1q_f64(b.as_ptr().add(i)));
            acc = vaddq_f64(acc, prod);
            i += 2;
        }
        let mut s = vaddvq_f64(acc);
        while i < n {
            s += a[i] * b[i];
            i += 1;
        }
        s
    }
}

/// Scale `c = a * k` by a broadcast scalar — explicit `fmul v.2d` stream +
/// scalar tail. Per-lane bit-identical to a scalar Double multiply. `a`, `c`
/// share length.
#[cfg(target_arch = "aarch64")]
pub fn neon_scale(a: &[f64], k: f64, c: &mut [f64]) {
    use core::arch::aarch64::*;
    let n = a.len();
    debug_assert_eq!(c.len(), n, "neon_scale: length mismatch");
    // SAFETY: NEON is baseline on aarch64; every access is in-bounds (i+2<=n,
    // a/c length n).
    unsafe {
        let vk = vdupq_n_f64(k);
        let mut i = 0;
        while i + 2 <= n {
            vst1q_f64(c.as_mut_ptr().add(i), vmulq_f64(vld1q_f64(a.as_ptr().add(i)), vk));
            i += 2;
        }
        while i < n {
            c[i] = a[i] * k;
            i += 1;
        }
    }
}

/// Reduce `a` to its maximum lane — explicit `fmax v.2d` accumulator + a
/// horizontal max. `fmax`/`min` are associative and commutative for non-NaN,
/// so the result is order-independent (bit-exact regardless of lane grouping),
/// unlike the FP SUM. The caller guarantees `a` is non-empty.
#[cfg(target_arch = "aarch64")]
pub fn neon_max(a: &[f64]) -> f64 {
    use core::arch::aarch64::*;
    let n = a.len();
    debug_assert!(n >= 1, "neon_max: empty");
    // SAFETY: NEON is baseline on aarch64; n>=1 so a[0] is valid, and each
    // `vld1q_f64` reads lanes [i, i+2) with i+2 <= n.
    unsafe {
        let mut acc = vdupq_n_f64(a[0]); // seed both lanes (max is idempotent)
        let mut i = 0;
        while i + 2 <= n {
            acc = vmaxq_f64(acc, vld1q_f64(a.as_ptr().add(i)));
            i += 2;
        }
        let mut m = vgetq_lane_f64(acc, 0).max(vgetq_lane_f64(acc, 1));
        while i < n {
            m = m.max(a[i]);
            i += 1;
        }
        m
    }
}

/// Reduce `a` to its minimum lane — `fmin v.2d` accumulator + horizontal min.
/// Order-independent, like [`neon_max`]. The caller guarantees `a` non-empty.
#[cfg(target_arch = "aarch64")]
pub fn neon_min(a: &[f64]) -> f64 {
    use core::arch::aarch64::*;
    let n = a.len();
    debug_assert!(n >= 1, "neon_min: empty");
    // SAFETY: as neon_max.
    unsafe {
        let mut acc = vdupq_n_f64(a[0]);
        let mut i = 0;
        while i + 2 <= n {
            acc = vminq_f64(acc, vld1q_f64(a.as_ptr().add(i)));
            i += 2;
        }
        let mut m = vgetq_lane_f64(acc, 0).min(vgetq_lane_f64(acc, 1));
        while i < n {
            m = m.min(a[i]);
            i += 1;
        }
        m
    }
}

// ── Off-target scalar mirrors (byte-exact: same 2-accumulator order) ─────────

#[cfg(not(target_arch = "aarch64"))]
pub fn neon_add(a: &[f64], b: &[f64], c: &mut [f64]) {
    for i in 0..a.len() {
        c[i] = a[i] + b[i];
    }
}

#[cfg(not(target_arch = "aarch64"))]
pub fn pairwise_sum(a: &[f64]) -> f64 {
    let mut acc = [0.0f64; 2];
    let n = a.len();
    let mut i = 0;
    while i + 2 <= n {
        acc[0] += a[i];
        acc[1] += a[i + 1];
        i += 2;
    }
    let mut s = acc[0] + acc[1];
    while i < n {
        s += a[i];
        i += 1;
    }
    s
}

#[cfg(not(target_arch = "aarch64"))]
pub fn pairwise_dot(a: &[f64], b: &[f64]) -> f64 {
    let mut acc = [0.0f64; 2];
    let n = a.len();
    let mut i = 0;
    while i + 2 <= n {
        acc[0] += a[i] * b[i];
        acc[1] += a[i + 1] * b[i + 1];
        i += 2;
    }
    let mut s = acc[0] + acc[1];
    while i < n {
        s += a[i] * b[i];
        i += 1;
    }
    s
}

#[cfg(not(target_arch = "aarch64"))]
pub fn neon_scale(a: &[f64], k: f64, c: &mut [f64]) {
    for i in 0..a.len() {
        c[i] = a[i] * k;
    }
}

#[cfg(not(target_arch = "aarch64"))]
pub fn neon_max(a: &[f64]) -> f64 {
    a.iter().copied().fold(a[0], f64::max)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn neon_min(a: &[f64]) -> f64 {
    a.iter().copied().fold(a[0], f64::min)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pairwise (2-lane) order is what the primitives promise, and it is
    /// deterministic — the same inputs always fold the same way. A tiny case
    /// pins the DEFINED order against a hand-computed reference so a future
    /// refactor can't silently change it.
    #[test]
    fn pairwise_sum_defined_order() {
        // lanes: acc0 = a0+a2, acc1 = a1+a3, then acc0+acc1, then +tail(a4).
        let a = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(pairwise_sum(&a), (1.0 + 3.0) + (2.0 + 4.0) + 5.0);
        assert_eq!(pairwise_sum(&[]), 0.0);
        assert_eq!(pairwise_sum(&[7.0]), 7.0);
    }

    #[test]
    fn neon_add_is_elementwise() {
        let a = [1.5, 2.5, 3.5];
        let b = [0.5, 0.5, 0.5];
        let mut c = [0.0; 3];
        neon_add(&a, &b, &mut c);
        assert_eq!(c, [2.0, 3.0, 4.0]);
    }

    #[test]
    fn pairwise_dot_defined_order() {
        let a = [1.0, 2.0, 3.0, 4.0];
        let b = [10.0, 100.0, 1000.0, 10000.0];
        // acc0 = 1*10 + 3*1000, acc1 = 2*100 + 4*10000, then acc0+acc1.
        assert_eq!(
            pairwise_dot(&a, &b),
            (1.0 * 10.0 + 3.0 * 1000.0) + (2.0 * 100.0 + 4.0 * 10000.0)
        );
    }

    #[test]
    fn scale_is_elementwise() {
        let a = [1.0, 2.0, 3.0, 4.0, 5.0];
        let mut c = [0.0; 5];
        neon_scale(&a, 2.5, &mut c);
        assert_eq!(c, [2.5, 5.0, 7.5, 10.0, 12.5]);
    }

    #[test]
    fn max_min_reductions() {
        let a = [3.0, -1.0, 7.5, 2.0, -4.25, 7.5, 0.0];
        assert_eq!(neon_max(&a), 7.5);
        assert_eq!(neon_min(&a), -4.25);
        // odd length exercises the scalar tail; single element is its own max/min.
        assert_eq!(neon_max(&[42.0]), 42.0);
        assert_eq!(neon_min(&[42.0]), 42.0);
    }
}
