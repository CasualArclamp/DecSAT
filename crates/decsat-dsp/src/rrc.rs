// made by CasualArclamp
//! Root-raised-cosine pulse shaping.
//!
//! DVB-S2 specifies square-root raised-cosine filtering at both ends of the
//! link (ETSI EN 302 307-1 §5.5.2), so the same taps serve the modulator's
//! pulse shaper and the receiver's matched filter.

use std::f64::consts::PI;

/// Root-raised-cosine impulse response.
///
/// * `sps` — samples per symbol (may be fractional).
/// * `alpha` — roll-off factor, 0.05…0.35 for S2/S2X.
/// * `span_symbols` — one-sided length in symbols; the filter is
///   `2 * span_symbols * sps + 1` taps long and symmetric.
///
/// Taps are normalised to unit energy, so a TX/RX cascade of two of these has
/// flat passband gain and the matched filter does not change the signal level.
///
/// # Panics
/// If `sps <= 0`, `alpha` is outside (0, 1], or `span_symbols` is 0.
pub fn rrc_taps(sps: f64, alpha: f64, span_symbols: usize) -> Vec<f32> {
    assert!(sps > 0.0, "samples per symbol must be positive");
    assert!(alpha > 0.0 && alpha <= 1.0, "roll-off must be in (0, 1]");
    assert!(span_symbols > 0, "span must be at least one symbol");

    let half = (span_symbols as f64 * sps).round() as isize;
    let n = (2 * half + 1) as usize;
    let mut taps = Vec::with_capacity(n);

    for k in -half..=half {
        // Time in symbol periods.
        let t = k as f64 / sps;
        taps.push(rrc_at(t, alpha) as f32);
    }

    // Unit energy.
    let energy: f64 = taps.iter().map(|&t| (t as f64) * (t as f64)).sum();
    let scale = 1.0 / energy.sqrt();
    for t in &mut taps {
        *t = (*t as f64 * scale) as f32;
    }
    taps
}

/// The RRC impulse response at time `t`, measured in symbol periods (T = 1).
///
/// The closed form has removable singularities at t = 0 and t = ±1/(4α); both
/// are handled with their limits, which is why this is a separate function.
fn rrc_at(t: f64, alpha: f64) -> f64 {
    const EPS: f64 = 1e-9;

    if t.abs() < EPS {
        // lim t->0
        return 1.0 + alpha * (4.0 / PI - 1.0);
    }

    let quarter = 1.0 / (4.0 * alpha);
    if (t.abs() - quarter).abs() < EPS {
        // lim t->±1/(4α)
        let x = PI / (4.0 * alpha);
        return (alpha / 2.0_f64.sqrt())
            * ((1.0 + 2.0 / PI) * x.sin() + (1.0 - 2.0 / PI) * x.cos());
    }

    let pit = PI * t;
    let num = (pit * (1.0 - alpha)).sin() + 4.0 * alpha * t * (pit * (1.0 + alpha)).cos();
    let den = pit * (1.0 - (4.0 * alpha * t).powi(2));
    num / den
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taps_are_symmetric_and_odd_length() {
        let taps = rrc_taps(4.0, 0.20, 8);
        assert_eq!(taps.len(), 2 * 8 * 4 + 1);
        assert_eq!(taps.len() % 2, 1);
        let n = taps.len();
        for k in 0..n / 2 {
            assert!(
                (taps[k] - taps[n - 1 - k]).abs() < 1e-6,
                "tap {k} not symmetric"
            );
        }
    }

    #[test]
    fn unit_energy() {
        for &alpha in &[0.05, 0.10, 0.20, 0.35] {
            let taps = rrc_taps(4.0, alpha, 16);
            let e: f32 = taps.iter().map(|t| t * t).sum();
            assert!((e - 1.0).abs() < 1e-5, "alpha {alpha}: energy {e}");
        }
    }

    #[test]
    fn peak_is_at_the_centre() {
        let taps = rrc_taps(8.0, 0.35, 10);
        let centre = taps.len() / 2;
        let (peak, _) = taps
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .unwrap();
        assert_eq!(peak, centre);
    }

    #[test]
    fn singularities_match_their_neighbourhood() {
        // t = 1/(4α) is a removable singularity; the limit must agree with the
        // values just either side of it.
        let alpha = 0.25;
        let t0 = 1.0 / (4.0 * alpha);
        let at = rrc_at(t0, alpha);
        let before = rrc_at(t0 - 1e-5, alpha);
        let after = rrc_at(t0 + 1e-5, alpha);
        assert!((at - before).abs() < 1e-4, "{at} vs {before}");
        assert!((at - after).abs() < 1e-4, "{at} vs {after}");
    }

    #[test]
    fn cascade_satisfies_nyquist_at_symbol_instants() {
        // Two RRCs in cascade give a raised cosine, which must be ~zero at
        // non-zero multiples of the symbol period.
        let sps = 4.0;
        let taps = rrc_taps(sps, 0.20, 24);
        let n = taps.len();
        // Autocorrelation at lag = m symbols.
        let centre = n / 2;
        let peak: f64 = taps.iter().map(|&t| (t as f64) * (t as f64)).sum();
        for m in 1..=4usize {
            let lag = m * sps as usize;
            let mut acc = 0.0f64;
            for k in 0..n {
                if k + lag < n {
                    acc += taps[k] as f64 * taps[k + lag] as f64;
                }
            }
            assert!(
                acc.abs() < 0.02 * peak,
                "lag {m} symbols: {acc} not small vs {peak}"
            );
        }
        let _ = centre;
    }
}
