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
//! 3.4× the dot product per output (same machine, portable SSE2; AVX2 measured
//! no faster).

use decsat_core::Iq;

/// Outputs [`fir_cr`] works out together. 16 complex sums are 8 SSE
/// registers; 8 measured 10.6 and 32 13.3 G MAC/s against 16's 16.5.
const BLOCK: usize = 16;

/// Every output of a filter with taps `h_rev` (reversed) that fits in `x`,
/// appended to `out`: `out[j] = Σ_k x[j + k] · h_rev[k]` for
/// `j = 0 ..= x.len() − h_rev.len()`. The same sums as [`dot_cr`] over each
/// window, added tap by tap in order, so it differs from it only by rounding.
pub fn fir_cr(x: &[Iq], h_rev: &[f32], out: &mut Vec<Iq>) {
    let l = h_rev.len();
    if l == 0 || x.len() < l {
        return;
    }
    let n_out = x.len() + 1 - l;
    out.reserve(n_out);
    let mut j = 0;
    while j + BLOCK <= n_out {
        let mut acc = [Iq::new(0.0, 0.0); BLOCK];
        for (k, &h) in h_rev.iter().enumerate() {
            // Rust note: `try_into` turns the slice into a `&[Iq; BLOCK]`,
            // so the loop below has a constant trip count and no bounds
            // checks — it unrolls into vector multiply-adds.
            let xs: &[Iq; BLOCK] = x[j + k..j + k + BLOCK].try_into().unwrap();
            for (a, &s) in acc.iter_mut().zip(xs) {
                *a += s * h;
            }
        }
        out.extend_from_slice(&acc);
        j += BLOCK;
    }
    // The last few outputs, one at a time.
    for j in j..n_out {
        out.push(dot_cr(&x[j..j + l], h_rev));
    }
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
