// made by CasualArclamp
//! Blind frame-structure finder, for waveforms DecSAT has no decoder for
//! (proprietary modems among them). A waveform that sends the same known
//! symbols every frame — a header, a unique word, pilot blocks under a
//! scrambling sequence that restarts each frame — shows them as a peak in the
//! autocorrelation of its symbols at the frame length, while scrambled data
//! averages out to ~1/√N. Once the period is known, averaging the frames on
//! top of each other ("folding") shows *where* in the frame the known
//! symbols are: the header's length and the pilot blocks' size and spacing,
//! a fingerprint of the framing even when no specification is public.
//!
//! The symbols should be carrier-locked (a fixed phase ambiguity is fine:
//! it is the same in every frame).

use decsat_core::Iq;
use rustfft::FftPlanner;

/// A repeating structure in a symbol stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Periodicity {
    /// The frame length, symbols.
    pub period: usize,
    /// |autocorrelation| at the period (unit-magnitude symbols): about the
    /// share of each frame that repeats, less carrier phase wander.
    pub share: f32,
    /// How far the peak stands above the autocorrelation's noise floor.
    pub prominence: f32,
    /// Frames the listen held.
    pub frames: usize,
    /// Runs of repeating symbols as (start, length) within the frame, when
    /// enough frames were heard to tell (see [`FOLD_FRAMES`]).
    pub known: Vec<(usize, usize)>,
}

impl Periodicity {
    pub fn describe(&self) -> String {
        let mut s = format!(
            "repeats every {} symbols ({:.1} % of each frame)",
            self.period,
            self.share * 100.0
        );
        if let Some(&(start, len)) = self.known.first() {
            let total: usize = self.known.iter().map(|k| k.1).sum();
            s += &format!(
                "; {total} known symbols: {len} from {start}{}",
                match self.known.len() {
                    1 => String::new(),
                    n => {
                        // Pilot-like blocks: report their size and spacing
                        // when regular.
                        let rest = &self.known[1..];
                        let lens: Vec<usize> = rest.iter().map(|k| k.1).collect();
                        let gaps: Vec<usize> = rest.windows(2).map(|w| w[1].0 - w[0].0).collect();
                        let same = |v: &[usize]| v.iter().all(|&x| x == v[0]);
                        if same(&lens) && (gaps.is_empty() || same(&gaps)) {
                            match gaps.first() {
                                Some(g) => {
                                    format!(", then {} blocks of {} every {g}", n - 1, lens[0])
                                }
                                None => format!(", then a block of {} at {}", lens[0], rest[0].0),
                            }
                        } else {
                            format!(", then {} irregular blocks", n - 1)
                        }
                    }
                }
            );
        }
        s
    }
}

/// The autocorrelation peak must stand this far above the floor (the
/// median; for noise alone a peak this high comes up ~once in 10⁸ lags).
const PROMINENCE: f32 = 6.0;
/// Frames needed to say which symbols repeat.
pub const FOLD_FRAMES: usize = 8;
/// A folded position counts as known above this coherence.
const KNOWN: f32 = 0.7;

/// Look for a period between `min_lag` and `max_lag` symbols (and at most a
/// third of the listen, so three frames are seen).
pub fn find_period(sym: &[Iq], min_lag: usize, max_lag: usize) -> Option<Periodicity> {
    let n = sym.len();
    let max_lag = max_lag.min(n / 3);
    if n < 64 || max_lag <= min_lag + 8 {
        return None;
    }
    // Unit-magnitude symbols: amplitude rings and gain changes do not
    // matter, only phase.
    let u: Vec<Iq> = sym
        .iter()
        .map(|s| {
            let m = s.norm();
            if m > 1e-12 { s / m } else { Iq::new(0.0, 0.0) }
        })
        .collect();
    // Autocorrelation by FFT: r[L] = Σ u[k + L] u*[k].
    let size = (2 * n).next_power_of_two();
    let mut buf: Vec<rustfft::num_complex::Complex<f32>> = u
        .iter()
        .copied()
        .chain(std::iter::repeat(Iq::new(0.0, 0.0)))
        .take(size)
        .collect();
    let mut planner = FftPlanner::new();
    planner.plan_fft_forward(size).process(&mut buf);
    for v in buf.iter_mut() {
        *v = Iq::new(v.norm_sqr(), 0.0);
    }
    planner.plan_fft_inverse(size).process(&mut buf);
    let c: Vec<f32> = (0..=max_lag)
        .map(|l| {
            if l < min_lag {
                0.0
            } else {
                buf[l].norm() / size as f32 / (n - l) as f32
            }
        })
        .collect();
    let mut sorted: Vec<f32> = c[min_lag..].to_vec();
    sorted.sort_unstable_by(f32::total_cmp);
    let floor = sorted[sorted.len() / 2].max(1e-9);
    let best = sorted[sorted.len() - 1];
    if best < PROMINENCE * floor {
        return None;
    }
    // The fundamental: the first local peak at least half as strong as the
    // strongest (its multiples are as strong; anything weaker below it is a
    // sub-structure such as a pilot spacing, kept out of "period").
    let period = (min_lag..=max_lag).find(|&l| {
        c[l] >= 0.5 * best
            && c[l] >= c[l.saturating_sub(1)]
            && c.get(l + 1).is_none_or(|&v| c[l] >= v)
    })?;
    let frames = n / period;
    let mut p = Periodicity {
        period,
        share: c[period],
        prominence: c[period] / floor,
        frames,
        known: Vec::new(),
    };
    if frames >= FOLD_FRAMES {
        p.known = fold(&u, period, frames);
        // Correlation spread thinly over every position is the data's own
        // regularity (unscrambled text, for one), not a frame: a frame has
        // somewhere its symbols repeat.
        if p.known.is_empty() {
            return None;
        }
    }
    Some(p)
}

/// Runs of positions whose symbols agree across frames.
fn fold(u: &[Iq], period: usize, frames: usize) -> Vec<(usize, usize)> {
    let frame = |j: usize| &u[j * period..(j + 1) * period];
    // Pass 1: frames summed as they are (a locked carrier barely drifts
    // over a listen); positions that stand out are candidates.
    let mut first = vec![Iq::new(0.0, 0.0); period];
    for j in 0..frames {
        for (a, &v) in first.iter_mut().zip(frame(j)) {
            *a += v;
        }
    }
    let cand: Vec<usize> = (0..period)
        .filter(|&k| first[k].norm() / frames as f32 > 0.5 * KNOWN)
        .collect();
    // Pass 2: each frame turned onto the candidates' sum, which follows a
    // drifting phase from frame to frame (estimating it from a whole frame
    // would mostly measure the data).
    let mut acc = vec![Iq::new(0.0, 0.0); period];
    for j in 0..frames {
        let f = frame(j);
        let c: Iq = cand.iter().map(|&k| f[k] * first[k].conj()).sum();
        let rot = if c.norm() > 1e-9 {
            c.conj() / c.norm()
        } else {
            Iq::new(1.0, 0.0)
        };
        for (a, &v) in acc.iter_mut().zip(f) {
            *a += v * rot;
        }
    }
    let known: Vec<bool> = acc
        .iter()
        .map(|a| a.norm() / frames as f32 > KNOWN)
        .collect();
    // Runs, with gaps of one symbol (a noise dip) closed.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut k = 0;
    while k < period {
        if known[k] {
            let start = k;
            while k < period && (known[k] || (k + 1 < period && known[k + 1])) {
                k += 1;
            }
            runs.push((start, k - start));
        } else {
            k += 1;
        }
    }
    // Lone positions are chance.
    runs.retain(|r| r.1 >= 2);
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: u64) -> impl FnMut() -> u64 {
        let mut s = seed | 1;
        move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    fn qpsk(r: u64) -> Iq {
        let k = std::f32::consts::FRAC_1_SQRT_2;
        Iq::new(
            if r & 1 == 0 { k } else { -k },
            if r & 2 == 0 { k } else { -k },
        )
    }

    /// Frames of `period` QPSK symbols: a fixed header of `header`, fixed
    /// pilot blocks of `pilot` every `spacing` after it, random data.
    fn frames(
        period: usize,
        header: usize,
        pilot: usize,
        spacing: usize,
        n: usize,
        snr_db: f32,
    ) -> Vec<Iq> {
        let mut known = rng(99);
        let fixed: Vec<Iq> = (0..period).map(|_| qpsk(known())).collect();
        let is_known = |k: usize| {
            k < header || (pilot > 0 && k >= header && (k - header) % spacing >= spacing - pilot)
        };
        let mut data = rng(7);
        let mut noise = rng(3);
        let sigma = (10f32.powf(-snr_db / 10.0) / 2.0).sqrt();
        let mut g = move || {
            let a = ((noise() >> 11) as f64 / (1u64 << 53) as f64).max(1e-300);
            let b = (noise() >> 11) as f64 / (1u64 << 53) as f64;
            ((-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()) as f32
        };
        (0..n)
            .map(|i| {
                let k = i % period;
                let s = if is_known(k) { fixed[k] } else { qpsk(data()) };
                // A residual carrier offset, as a locked loop leaves none
                // but a drifting phase might.
                let ph = 2e-6 * i as f32;
                s * Iq::new(ph.cos(), ph.sin()) + Iq::new(sigma * g(), sigma * g())
            })
            .collect()
    }

    #[test]
    fn finds_a_header_and_pilot_blocks() {
        // 3330-symbol frames, a 90-symbol header, 36-symbol pilot blocks
        // closing every 1476 (a DVB-S2-like layout).
        let x = frames(3330, 90, 36, 1476, 3330 * 12, 6.0);
        let p = find_period(&x, 16, 100_000).expect("no period");
        assert_eq!(p.period, 3330);
        assert!(p.share > 0.03, "{p:?}");
        assert_eq!(p.known.first(), Some(&(0, 90)), "{p:?}");
        assert_eq!(p.known[1..], [(1530, 36), (3006, 36)], "{p:?}");
        assert!(
            p.describe().contains("2 blocks of 36 every 1476"),
            "{}",
            p.describe()
        );
    }

    #[test]
    fn finds_a_short_unique_word_given_enough_frames() {
        // A 32-symbol UW every 1000: 3.2 %.
        let x = frames(1000, 32, 0, 1, 1000 * 40, 8.0);
        let p = find_period(&x, 16, 100_000).expect("no period");
        assert_eq!(p.period, 1000);
        assert_eq!(p.known, [(0, 32)]);
    }

    #[test]
    fn random_symbols_have_no_period() {
        let mut r = rng(5);
        let x: Vec<Iq> = (0..60_000).map(|_| qpsk(r())).collect();
        assert_eq!(find_period(&x, 16, 100_000), None);
    }
}
