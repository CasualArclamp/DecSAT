// made by CasualArclamp
//! The inner loop of every FIR here: a complex-by-real dot product.
//!
//! Written with eight independent partial sums because the compiler will not
//! vectorise a single floating-point accumulator — it may not reorder float
//! additions, and one running sum is a serial dependency chain. Eight lanes of
//! re/im sums break the chain; the result differs from the serial sum only by
//! float rounding.
//!
//! Measured on the 9800X3D (116 taps, release): serial 4.0, this 5.0 G MAC/s.
//! Two alternatives were tried and measured slower, so they are not here:
//! duplicated taps over a flat `f32` view of the samples, with AVX2/FMA picked
//! by run-time dispatch (3.5 G MAC/s — the per-call dispatch cost more than the
//! wider vectors saved on windows this short). The serial loop is faster than
//! its dependency chain suggests because successive outputs are independent and
//! the CPU overlaps them out of order.
//!
//! A filter that does not decimate has a faster way: [`fir_cr`] works out
//! [`BLOCK`] neighbouring outputs together, so each tap is loaded once for all
//! of them and the sums never leave registers — 16.5 G MAC/s at 55–73 taps,
//! 3.4× the dot product per output (same machine, portable SSE2).
//!
//! [`fir_cr`] also has AVX2 and AVX-512 copies picked at run time. Measured on
//! the same machine (rustc 1.98, 55–116 taps, 1 k and 16 k outputs a call):
//! SSE2 16.5, AVX2 28, AVX-512 41 G MAC/s. Hand-written FMA over the same
//! block sizes gained under 6 %, so these are the plain loop recompiled: no
//! FMA is emitted, and every output is bit-identical to the SSE2 build.

use decsat_core::Iq;

/// Outputs [`fir_cr`] works out together on SSE2. 16 complex sums are 8 SSE
/// registers; 8 measured 10.6 and 32 13.3 G MAC/s against 16's 16.5.
const BLOCK: usize = 16;
/// The same for AVX2: 28 G MAC/s at 32, 25.5 at 16.
#[cfg(target_arch = "x86_64")]
const BLOCK_AVX2: usize = 32;
/// The same for AVX-512: 41 G MAC/s at 64, 35 at 16.
#[cfg(target_arch = "x86_64")]
const BLOCK_AVX512: usize = 64;

/// Every output of a filter with taps `h_rev` (reversed) that fits in `x`,
/// appended to `out`: `out[j] = Σ_k x[j + k] · h_rev[k]` for
/// `j = 0 ..= x.len() − h_rev.len()`. The same sums as [`dot_cr`] over each
/// window, added tap by tap in order, so it differs from it only by rounding.
///
/// Rust note: a release build targets plain x86-64 (SSE2) so that the one exe
/// runs anywhere. `#[target_feature]` compiles further copies of the same
/// loop, and `is_x86_feature_detected!` (a cached flag test) picks one here.
/// Under [`BLOCK`] outputs there is nothing for a wider copy to do, so a
/// sample-at-a-time caller such as `Fir::push` stays on the portable path.
pub fn fir_cr(x: &[Iq], h_rev: &[f32], out: &mut Vec<Iq>) {
    let l = h_rev.len();
    if l == 0 || x.len() < l {
        return;
    }
    let n_out = x.len() + 1 - l;
    #[cfg(target_arch = "x86_64")]
    if n_out >= BLOCK {
        if has_avx512() {
            // SAFETY: the CPU has the features the function was compiled for.
            unsafe { fir_avx512(x, h_rev, n_out, out) };
            return;
        }
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: as above.
            unsafe { fir_avx2(x, h_rev, n_out, out) };
            return;
        }
    }
    fir_body::<BLOCK>(x, h_rev, n_out, out);
}

/// Name of the widest vector unit [`fir_cr`] uses on this CPU, for display.
pub fn vector_level() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx512() {
            "AVX-512"
        } else if std::arch::is_x86_feature_detected!("avx2") {
            "AVX2"
        } else {
            "SSE2"
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        "portable"
    }
}

/// Every feature [`fir_avx512`] is compiled for.
#[cfg(target_arch = "x86_64")]
fn has_avx512() -> bool {
    std::arch::is_x86_feature_detected!("avx512f")
        && std::arch::is_x86_feature_detected!("avx512bw")
        && std::arch::is_x86_feature_detected!("avx512vl")
        && std::arch::is_x86_feature_detected!("avx512dq")
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn fir_avx2(x: &[Iq], h_rev: &[f32], n_out: usize, out: &mut Vec<Iq>) {
    fir_body::<BLOCK_AVX2>(x, h_rev, n_out, out);
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx512vl,avx512dq")]
unsafe fn fir_avx512(x: &[Iq], h_rev: &[f32], n_out: usize, out: &mut Vec<Iq>) {
    fir_body::<BLOCK_AVX512>(x, h_rev, n_out, out);
}

/// `WIDE` outputs at a time, then [`BLOCK`], then singly. `WIDE` is a multiple
/// of [`BLOCK`], so the same last few outputs reach [`dot_cr`] on every CPU
/// and the results do not depend on which copy ran.
#[inline(always)]
fn fir_body<const WIDE: usize>(x: &[Iq], h_rev: &[f32], n_out: usize, out: &mut Vec<Iq>) {
    out.reserve(n_out);
    let mut j = fir_blocks::<WIDE>(x, h_rev, n_out, 0, out);
    if WIDE != BLOCK {
        j = fir_blocks::<BLOCK>(x, h_rev, n_out, j, out);
    }
    // The last few outputs, one at a time.
    let l = h_rev.len();
    for j in j..n_out {
        out.push(dot_cr(&x[j..j + l], h_rev));
    }
}

/// As many blocks of `B` outputs as fit, from output `j`; returns the next
/// output due.
#[inline(always)]
fn fir_blocks<const B: usize>(
    x: &[Iq],
    h_rev: &[f32],
    n_out: usize,
    mut j: usize,
    out: &mut Vec<Iq>,
) -> usize {
    while j + B <= n_out {
        let mut acc = [Iq::new(0.0, 0.0); B];
        for (k, &h) in h_rev.iter().enumerate() {
            // Rust note: `try_into` turns the slice into a `&[Iq; B]`, so
            // the loop below has a constant trip count and no bounds
            // checks — it unrolls into vector multiply-adds.
            let xs: &[Iq; B] = x[j + k..j + k + B].try_into().unwrap();
            for (a, &s) in acc.iter_mut().zip(xs) {
                *a += s * h;
            }
        }
        out.extend_from_slice(&acc);
        j += B;
    }
    j
}

/// `Σ x[k] · h[k]` for complex `x` and real `h` of equal length.
#[inline]
pub fn dot_cr(x: &[Iq], h: &[f32]) -> Iq {
    debug_assert_eq!(x.len(), h.len());
    let n = x.len().min(h.len());
    let (x, h) = (&x[..n], &h[..n]);

    let mut re = [0.0f32; 8];
    let mut im = [0.0f32; 8];
    // Rust note: `as_chunks::<8>()` yields `&[T; 8]` arrays, so the inner loop
    // has a constant trip count with no bounds checks — what SIMD wants.
    let (xc, xr) = x.as_chunks::<8>();
    let (hc, hr) = h.as_chunks::<8>();
    for (xs, hs) in xc.iter().zip(hc) {
        for k in 0..8 {
            re[k] += xs[k].re * hs[k];
            im[k] += xs[k].im * hs[k];
        }
    }
    let mut acc = Iq::new(re.iter().sum(), im.iter().sum());
    for (s, &t) in xr.iter().zip(hr) {
        acc += s * t;
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_serial_sum() {
        for n in [0usize, 1, 7, 8, 9, 63, 64, 65, 1000] {
            let x: Vec<Iq> = (0..n)
                .map(|k| Iq::new((k as f32 * 0.37).sin(), (k as f32 * 0.11).cos()))
                .collect();
            let h: Vec<f32> = (0..n)
                .map(|k| (k as f32 * 0.23).cos() / (1.0 + k as f32))
                .collect();
            let serial = x
                .iter()
                .zip(&h)
                .fold(Iq::new(0.0, 0.0), |a, (s, &t)| a + s * t);
            let fast = dot_cr(&x, &h);
            assert!(
                (serial - fast).norm() < 1e-4 * (1.0 + serial.norm()),
                "n {n}: {serial} vs {fast}"
            );
        }
    }

    /// Every copy the CPU can run gives the same bits as the portable one,
    /// around each block boundary.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn every_vector_width_gives_identical_bits() {
        let bits = |v: &[Iq]| -> Vec<(u32, u32)> {
            v.iter().map(|s| (s.re.to_bits(), s.im.to_bits())).collect()
        };
        for l in [1usize, 7, 55, 116] {
            for n_out in [
                1usize, 15, 16, 17, 31, 32, 33, 63, 64, 65, 79, 80, 127, 1000,
            ] {
                let x: Vec<Iq> = (0..n_out + l - 1)
                    .map(|k| Iq::new((k as f32 * 0.37).sin(), (k as f32 * 0.11).cos()))
                    .collect();
                let h: Vec<f32> = (0..l).map(|k| (k as f32 * 0.23).cos() * 0.1).collect();
                let mut want = Vec::new();
                fir_body::<BLOCK>(&x, &h, n_out, &mut want);
                assert_eq!(want.len(), n_out);

                let mut got = Vec::new();
                fir_cr(&x, &h, &mut got);
                assert_eq!(bits(&got), bits(&want), "dispatch, l {l} n {n_out}");
                if std::arch::is_x86_feature_detected!("avx2") {
                    got.clear();
                    // SAFETY: the CPU has the feature, just checked.
                    unsafe { fir_avx2(&x, &h, n_out, &mut got) };
                    assert_eq!(bits(&got), bits(&want), "AVX2, l {l} n {n_out}");
                }
                if has_avx512() {
                    got.clear();
                    // SAFETY: as above.
                    unsafe { fir_avx512(&x, &h, n_out, &mut got) };
                    assert_eq!(bits(&got), bits(&want), "AVX-512, l {l} n {n_out}");
                }
            }
        }
    }

    #[test]
    fn fir_matches_a_dot_product_per_window() {
        for (n, l) in [
            (0usize, 5usize),
            (4, 5),
            (5, 5),
            (20, 5),
            (100, 55),
            (1000, 73),
            (37, 1),
        ] {
            let x: Vec<Iq> = (0..n)
                .map(|k| Iq::new((k as f32 * 0.37).sin(), (k as f32 * 0.11).cos()))
                .collect();
            let h: Vec<f32> = (0..l).map(|k| (k as f32 * 0.23).cos()).collect();
            let mut out = Vec::new();
            fir_cr(&x, &h, &mut out);
            assert_eq!(out.len(), (n + 1).saturating_sub(l), "n {n} l {l}");
            for (j, y) in out.iter().enumerate() {
                let want = dot_cr(&x[j..j + l], &h);
                assert!((y - want).norm() < 1e-4, "n {n} l {l} j {j}: {y} vs {want}");
            }
        }
    }
}
