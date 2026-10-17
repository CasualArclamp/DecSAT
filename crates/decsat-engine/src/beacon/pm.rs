// made by CasualArclamp
//! What a beacon's phase modulation carries: a subcarrier (PCM/PSK/PM) or
//! data on the carrier's phase itself (PCM/PM), and at what bit rate, in
//! which code — all read blind from the phase the carrier loop hands on.
//!
//! **Subcarrier.** Data d(t) = ±1 BPSK-modulating a sine subcarrier and
//! phase-modulating the carrier give φ(t) = m·d(t)·sin(ω_sc t); squared,
//! φ² = m²(1 − cos 2ω_sc t)/2 whatever the data — a line at twice the
//! subcarrier (the classic squaring loop's). The strongest line in φ²'s
//! spectrum that has φ's own spectrum spread symmetrically about half its
//! frequency is the subcarrier.
//!
//! **Bit rate and code.** Data transitions fall on the bit (or chip)
//! grid, so the squared derivative of the data waveform has a line at its
//! rate. Bi-phase (Manchester: IRIG 106 Chapter 4 §4.2.1's Bi-φ-L, CCSDS
//! 401.0-B §2.4.11's "Bi-phase-L") is then told from NRZ by its chips:
//! integrated chip by chip, every bit's two chips differ.

use rustfft::{FftPlanner, num_complex::Complex32};

/// Power spectrum of a real signal, bins 0..=n/2, Welch (Hann, 50 %
/// overlap), normalised so white noise of variance σ² reads σ².
pub(crate) fn psd_real(x: &[f32], n: usize, max_segments: usize) -> (Vec<f64>, usize) {
    let mut acc = vec![0f64; n / 2 + 1];
    if x.len() < n {
        return (acc, 0);
    }
    let w: Vec<f32> = (0..n)
        .map(|k| 0.5 - 0.5 * (std::f32::consts::TAU * k as f32 / n as f32).cos())
        .collect();
    let w2: f64 = w.iter().map(|&v| f64::from(v * v)).sum();
    let fft = FftPlanner::<f32>::new().plan_fft_forward(n);
    let full = (x.len() - n) / (n / 2) + 1;
    let hop = if full > max_segments {
        (x.len() - n) / max_segments.max(1)
    } else {
        n / 2
    }
    .max(1);
    let mut buf = vec![Complex32::new(0.0, 0.0); n];
    let mut segs = 0;
    let mut start = 0;
    while start + n <= x.len() {
        let seg = &x[start..start + n];
        let mean = seg.iter().sum::<f32>() / n as f32;
        for k in 0..n {
            buf[k] = Complex32::new((seg[k] - mean) * w[k], 0.0);
        }
        fft.process(&mut buf);
        for (a, b) in acc.iter_mut().zip(&buf) {
            *a += f64::from(b.norm_sqr());
        }
        segs += 1;
        start += hop;
    }
    let norm = 1.0 / (segs.max(1) as f64 * w2);
    for a in &mut acc {
        *a *= norm;
    }
    (acc, segs)
}

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mid = v.len() / 2;
    *v.select_nth_unstable_by(mid, f64::total_cmp).1
}

/// `p` through a median over 9 bins: narrow lines (a tone, another
/// subcarrier's line, intermodulation between a subcarrier's sidebands)
/// drop out, a data spectrum's spread stays.
fn median_smoothed(p: &[f64]) -> Vec<f64> {
    (0..p.len())
        .map(|k| {
            let mut w: Vec<f64> = p[k.saturating_sub(4)..(k + 5).min(p.len())].to_vec();
            median(&mut w)
        })
        .collect()
}

/// A parabola's vertex through bins k−1, k, k+1 (dB), as a fraction of a
/// bin from k.
fn vertex(p: &[f64], k: usize) -> f64 {
    if k == 0 || k + 1 >= p.len() {
        return 0.0;
    }
    let db = |i: usize| 10.0 * p[i].max(1e-30).log10();
    let (a, b, c) = (db(k - 1), db(k), db(k + 1));
    let d = a - 2.0 * b + c;
    if d.abs() < 1e-12 {
        0.0
    } else {
        (0.5 * (a - c) / d).clamp(-0.5, 0.5)
    }
}

/// Lines in `p` (bins `lo..hi`) standing `min_ratio` over the median of
/// the bins round them: (bin, interpolated bin, ratio), strongest first.
fn lines(p: &[f64], lo: usize, hi: usize, min_ratio: f64) -> Vec<(usize, f64, f64)> {
    let hi = hi.min(p.len().saturating_sub(2));
    let mut out = Vec::new();
    for k in lo.max(2)..hi {
        if p[k] < p[k - 1] || p[k] < p[k + 1] {
            continue;
        }
        let a = k.saturating_sub(64);
        let b = (k + 64).min(p.len());
        let mut near: Vec<f64> = (a..b)
            .filter(|&i| i.abs_diff(k) > 3)
            .map(|i| p[i])
            .collect();
        let floor = median(&mut near).max(1e-30);
        let r = p[k] / floor;
        if r >= min_ratio {
            out.push((k, k as f64 + vertex(p, k), r));
        }
    }
    out.sort_by(|a, b| b.2.total_cmp(&a.2));
    out
}

/// A data subcarrier found in the phase.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Subcarrier {
    pub freq_hz: f64,
    /// How far either side of it the data reach (95 % of their power).
    pub half_width_hz: f64,
    /// The data's power over the noise in that band.
    pub snr: f64,
}

/// FFT size for `len` samples at `rate`: bins of about `bin_hz`, with at
/// least eight segments.
fn fft_size(len: usize, rate: f64, bin_hz: f64) -> usize {
    ((rate / bin_hz) as usize)
        .next_power_of_two()
        .min((len / 8).next_power_of_two())
        .clamp(256, 1 << 16)
}

/// How much wider than the data show above the noise (95 % of their power)
/// a subcarrier's band is taken.
const WIDTH_MARGIN: f64 = 1.5;

/// The data round a subcarrier at bin `ksc` of the (median-smoothed)
/// phase spectrum `smooth`, up to `reach` bins out: the noise round it
/// (from the quietest quarter of the span: the phase's noise is not flat,
/// the carrier loop and the receiver's filter shape it; `floor`, the whole
/// spectrum's, bounds it from below) and how far the data go either side
/// with nothing missing, across no gap of more than `tolerance` bins
/// (bi-phase dips at its very centre): another subcarrier's data beyond a
/// gap are no part of this one's.
fn data_extent(
    smooth: &[f64],
    floor: f64,
    ksc: usize,
    reach: usize,
    tolerance: usize,
) -> (f64, usize) {
    let mut around: Vec<f64> = smooth[ksc - reach..=ksc + reach].to_vec();
    let near = around.len() / 4;
    let local = *around.select_nth_unstable_by(near, f64::total_cmp).1;
    let local = local.max(floor.min(local * 4.0)).max(1e-30);
    let run = |dir: isize| {
        let (mut last, mut gap) = (0, 0);
        for d in 1..reach {
            let k = (ksc as isize + dir * d as isize) as usize;
            if smooth[k] - local > 0.5 * local {
                (last, gap) = (d, 0);
            } else {
                gap += 1;
                if gap > tolerance {
                    break;
                }
            }
        }
        last
    };
    (local, run(-1).max(run(1)))
}

/// Find the data subcarrier in `phi` (the carrier's phase at `rate`): the
/// best of [`find_subcarriers`].
pub fn find_subcarrier(phi: &[f32], rate: f64) -> Option<Subcarrier> {
    find_subcarriers(phi, rate).into_iter().next()
}

/// Find the data subcarriers in `phi` (the carrier's phase at `rate`),
/// best first: the squared phase's lines, each judged by the data spread
/// round half its frequency.
///
/// A beacon may carry two subcarriers (Intelsat's: 48 and 72 kHz, each
/// 4800 bit/s bi-phase). The squared phase then holds a line at the sum
/// of their frequencies too — the data's clock lines beat into one — and
/// half of it, 60 kHz, has the data spread symmetrically about it just as a
/// subcarrier's would. What gives it away is the gap: a subcarrier has its
/// data right up to it (bi-phase dips only at the very centre), the
/// midpoint of two has none, and the data are measured only as far as they
/// run unbroken, so one subcarrier's band never takes in the other's. A
/// candidate that still reaches into a better one's band is dropped.
pub fn find_subcarriers(phi: &[f32], rate: f64) -> Vec<Subcarrier> {
    let n = fft_size(phi.len(), rate, rate / 8192.0);
    if phi.len() < 4 * n {
        return Vec::new();
    }
    let (p, _) = psd_real(phi, n, 256);
    let sq: Vec<f32> = phi.iter().map(|v| v * v).collect();
    let (q, _) = psd_real(&sq, n, 256);
    let bin = rate / n as f64;
    let half_bins = n / 2;
    let mut all = p[1..].to_vec();
    let floor = median(&mut all).max(1e-30);
    let smooth = median_smoothed(&p);
    let mut found: Vec<(f64, Subcarrier)> = Vec::new();
    // Subcarriers from ~500 Hz up.
    let lo = ((1000.0 / bin) as usize).max(4);
    // Data run on from a subcarrier through the dip a bi-phase spectrum has
    // at its very centre, not across a gap of more than this many bins.
    let tolerance = ((1500.0 / bin) as usize).max(4);
    for (_, kf, _) in lines(&q, lo, half_bins, 10.0).into_iter().take(8) {
        let f2 = kf * bin;
        // Twice the subcarrier may have aliased past the Nyquist edge.
        for fsc in [f2 / 2.0, (rate - f2) / 2.0] {
            let ksc = (fsc / bin).round() as usize;
            if ksc < 4 || ksc + 4 >= half_bins {
                continue;
            }
            let reach = ksc.min(half_bins - ksc).min((0.9 * fsc / bin) as usize);
            let (local, extent) = data_extent(&smooth, floor, ksc, reach, tolerance);
            let excess = |k: usize| (smooth[k] - local).max(0.0);
            let mut sum = 0.0;
            let mut sym = (0.0, 0.0);
            let mut cum = Vec::with_capacity(extent);
            for d in 1..=extent {
                let (a, b) = (excess(ksc - d), excess(ksc + d));
                sum += a + b;
                sym.0 += a.min(b);
                sym.1 += a.max(b);
                cum.push(sum);
            }
            if sum <= 0.0 || sym.1 <= 0.0 {
                continue;
            }
            let symmetry = sym.0 / sym.1;
            let width = cum
                .iter()
                .position(|&c| c >= 0.95 * sum)
                .unwrap_or(cum.len())
                + 1;
            let snr = sum / (local * 2.0 * width as f64);
            if symmetry < 0.5 || snr < 0.3 || width < 3 {
                continue;
            }
            found.push((
                sum * symmetry,
                Subcarrier {
                    freq_hz: fsc,
                    // Data that stand only just over the noise reach
                    // further than they show: room is left for them.
                    half_width_hz: WIDTH_MARGIN * width as f64 * bin,
                    snr,
                },
            ));
        }
    }
    found.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut out: Vec<Subcarrier> = Vec::new();
    for (_, s) in found {
        let clear = out
            .iter()
            .all(|o| (s.freq_hz - o.freq_hz).abs() > 0.8 * (s.half_width_hz + o.half_width_hz));
        if clear {
            out.push(s);
        }
    }
    out
}

/// A subcarrier placed by hand at about `near_hz`: snapped to the squared
/// phase's line within 3 % (or 500 Hz) of twice it, when one stands 7 dB
/// clear (else left where it was put), and its data's reach either side
/// measured there (95 % of the excess power; `width_hz` when given).
pub fn subcarrier_at(
    phi: &[f32],
    rate: f64,
    near_hz: f64,
    width_hz: Option<f64>,
) -> Option<Subcarrier> {
    let n = fft_size(phi.len(), rate, rate / 8192.0);
    if phi.len() < 4 * n || near_hz <= 0.0 || near_hz >= rate / 2.0 {
        return None;
    }
    let (p, _) = psd_real(phi, n, 256);
    let sq: Vec<f32> = phi.iter().map(|v| v * v).collect();
    let (q, _) = psd_real(&sq, n, 256);
    let bin = rate / n as f64;
    let half_bins = n / 2;
    let span = (0.03 * 2.0 * near_hz).max(500.0);
    let lo = (((2.0 * near_hz - span) / bin) as usize).max(2);
    let hi = (((2.0 * near_hz + span) / bin) as usize + 1).min(half_bins);
    let fsc = lines(&q, lo, hi, 5.0)
        .first()
        .map_or(near_hz, |&(_, kf, _)| kf * bin / 2.0);
    let ksc = (fsc / bin).round() as usize;
    if ksc < 2 || ksc + 2 >= half_bins {
        return None;
    }
    let mut all = p[1..].to_vec();
    let floor = median(&mut all).max(1e-30);
    let reach = ksc.min(half_bins - ksc);
    // The data's spread: narrow lines dropped first, then only what both
    // sides share, as data on a subcarrier are mirrored about it, and only
    // as far as they run unbroken (a second subcarrier is no part of it).
    let smooth = median_smoothed(&p);
    let (local, extent) = data_extent(&smooth, floor, ksc, reach, ((1500.0 / bin) as usize).max(4));
    let excess = |k: usize| (smooth[k] - local).max(0.0);
    let mut sum = 0.0;
    let mut cum = Vec::with_capacity(extent);
    for d in 1..=extent {
        sum += 2.0 * excess(ksc - d).min(excess(ksc + d));
        cum.push(sum);
    }
    let width_bins = cum
        .iter()
        .position(|&c| c >= 0.95 * sum)
        .unwrap_or(cum.len())
        + 1;
    let measured = (WIDTH_MARGIN * width_bins as f64 * bin).max(4.0 * bin);
    Some(Subcarrier {
        freq_hz: fsc,
        half_width_hz: width_hz.unwrap_or(measured).min(fsc).max(bin),
        snr: sum / (local * 2.0 * width_bins as f64),
    })
}

/// The code and chip rate of data known to run at `bit_rate`: bi-phase if
/// chips at twice it pair up opposite nine times in ten, else NRZ.
pub fn coding_at(s: &[f32], rate: f64, bit_rate: f64) -> Option<(Coding, f64, f64)> {
    if rate / (2.0 * bit_rate) >= 2.5 {
        let (chips, phase) = chips_at(s, rate, 2.0 * bit_rate);
        if chips.len() >= 400 {
            let opp = pair_opposite(&chips);
            if opp[0].max(opp[1]) >= 0.9 {
                return Some((Coding::Biphase, 2.0 * bit_rate, phase));
            }
        }
    }
    if rate / bit_rate < 2.0 {
        return None;
    }
    let (chips, phase) = chips_at(s, rate, bit_rate);
    (chips.len() >= 400).then_some((Coding::Nrz, bit_rate, phase))
}

/// How far the data on the carrier's own phase reach (PCM/PM, no
/// subcarrier): 95 % of the phase's power above the noise, from `lo_hz`
/// (below which the carrier loop tracks it out). `None` if there is next
/// to none.
pub fn direct_width(phi: &[f32], rate: f64, lo_hz: f64) -> Option<f64> {
    let n = fft_size(phi.len(), rate, rate / 8192.0);
    if phi.len() < 4 * n {
        return None;
    }
    let (p, _) = psd_real(phi, n, 256);
    let bin = rate / n as f64;
    let mut all = p[1..].to_vec();
    // The noise from the top quarter of the band, where data rarely reach.
    let top = &mut all[3 * n / 8..];
    let floor = median(top).max(1e-30);
    let k0 = ((lo_hz / bin) as usize).max(1);
    let excess: Vec<f64> = (k0..=n / 2).map(|k| (p[k] - floor).max(0.0)).collect();
    let sum: f64 = excess.iter().sum();
    if sum < 0.5 * floor * excess.len() as f64 * 0.05 {
        return None;
    }
    let mut c = 0.0;
    let k95 = excess
        .iter()
        .position(|&e| {
            c += e;
            c >= 0.95 * sum
        })
        .unwrap_or(excess.len());
    Some((k0 + k95) as f64 * bin)
}

/// Harmonics of the transition rate looked through for its fundamental.
const MAX_HARMONIC: usize = 16;

/// The data's transition rate in `s` (a data waveform at `rate`): from
/// the lines of its squared derivative between `lo` and `hi` Hz — the
/// strongest, or the lowest of its family that stands nearly as tall, the
/// strongest being its 2nd…16th harmonic with most of the harmonics
/// between standing too. Sharp transitions put the rate's harmonics up
/// with it, and fill that alternates (1010…, or a convolutional code's
/// output for a run of zeros: Eutelsat 172B's 9600 symbol/s beacon) sets
/// the whole comb level, the 7th harmonic as likely to top it as the
/// first. With how far the line stands over the bins round it. `None`
/// below 9 dB.
pub fn find_clock(s: &[f32], rate: f64, lo: f64, hi: f64) -> Option<(f64, f64)> {
    if s.len() < 4096 {
        return None;
    }
    let d: Vec<f32> = s
        .windows(2)
        .map(|w| (w[1] - w[0]) * (w[1] - w[0]))
        .collect();
    let n = (d.len() / 4).next_power_of_two().clamp(1024, 1 << 18);
    let n = if n > d.len() / 2 { n / 2 } else { n };
    let (p, _) = psd_real(&d, n, 64);
    let bin = rate / n as f64;
    let lo_k = ((lo / bin) as usize).max(3);
    let hi_k = ((hi / bin) as usize).min(n / 2 - 2);
    let found = lines(&p, lo_k, hi_k, 8.0);
    let (kf, rr) = fundamental(&found)?;
    Some((kf * bin, rr))
}

/// Of `found` (lines as [`lines`] gives them, strongest first: bin,
/// interpolated bin, ratio), the fundamental of the strongest's family:
/// the lowest line (the 16th subharmonic first) standing at least 0.3 of
/// the strongest, with at least half the harmonics between it and the
/// strongest there too — a lone line that happens to divide the top is
/// not the clock. Its interpolated bin and ratio.
fn fundamental(found: &[(usize, f64, f64)]) -> Option<(f64, f64)> {
    let &(_, top, r) = found.first()?;
    // A line within a bin and a half (or 0.1 %) of `want`, standing `min`.
    let line_at = |want: f64, min: f64| {
        found
            .iter()
            .find(|&&(_, kf, rr)| (kf - want).abs() <= 1.5 + 1e-3 * want && rr >= min)
            .copied()
    };
    let sub = (2..=MAX_HARMONIC).rev().find_map(|m| {
        let f0 = top / m as f64;
        let base = line_at(f0, 0.3 * r)?;
        let between = (2..m).filter(|&j| line_at(f0 * j as f64, 8.0).is_some());
        (2 * between.count() >= m - 2).then_some(base)
    });
    let (_, kf, rr) = sub.unwrap_or((0, top, r));
    Some((kf, rr))
}

/// How a bit is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coding {
    /// NRZ (-L, or -M/S: told apart by the framer).
    Nrz,
    /// Bi-phase (Manchester): two chips a bit, always different.
    Biphase,
}

impl Coding {
    pub fn name(self) -> &'static str {
        match self {
            Coding::Nrz => "NRZ",
            Coding::Biphase => "bi-phase (Manchester)",
        }
    }
}

/// `s` integrated chip by chip at `chip_rate`: in segments of 400 chips,
/// each at the phase (of 8 tried) that makes the chips largest. Returns the
/// chips and the first segment's phase, samples.
pub(crate) fn chips_at(s: &[f32], rate: f64, chip_rate: f64) -> (Vec<f32>, f64) {
    let period = rate / chip_rate;
    let mut prefix = Vec::with_capacity(s.len() + 1);
    let mut acc = 0f64;
    prefix.push(0.0);
    for &v in s {
        acc += f64::from(v);
        prefix.push(acc);
    }
    let integral = |a: f64, b: f64| -> f64 {
        let f = |t: f64| {
            let i = (t.floor() as usize).min(s.len() - 1);
            prefix[i] + (t - i as f64) * f64::from(s[i])
        };
        f(b) - f(a)
    };
    const SEG: usize = 400;
    let mut out = Vec::new();
    let mut first_phase = 0.0;
    let mut start = 0.0;
    let mut seg = 0;
    while start + (SEG as f64 + 1.0) * period < s.len() as f64 - 1.0 {
        let (best, _) = (0..8)
            .map(|k| {
                let ph = k as f64 * period / 8.0;
                let e: f64 = (0..SEG)
                    .map(|c| {
                        let a = start + ph + c as f64 * period;
                        integral(a, a + period).abs()
                    })
                    .sum();
                (ph, e)
            })
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap_or((0.0, 0.0));
        if seg == 0 {
            first_phase = best;
        }
        for c in 0..SEG {
            let a = start + best + c as f64 * period;
            out.push((integral(a, a + period) / period) as f32);
        }
        start += SEG as f64 * period;
        seg += 1;
    }
    (out, first_phase)
}

/// For each way of pairing chips (from chip 0 or chip 1), the share of
/// pairs whose two chips differ in sign — counted where the chips round a
/// pair are not periodic fill. Chips repeating every 2 or 4 (bi-phase of a
/// run of one bit or of 0101…; NRZ fill — a convolutional code's output
/// for a run of zeros is 0101… — read at its rate or twice it) pair up
/// opposite one way or both, so an NRZ stream mostly fill passed for
/// bi-phase; where the chips do more, only bi-phase's own pairing is
/// always opposite. Every pair counts when too few are left.
pub(crate) fn pair_opposite(chips: &[f32]) -> [f64; 2] {
    let sign: Vec<bool> = chips.iter().map(|&c| c > 0.0).collect();
    let mut out = [0.0; 2];
    for (a, o) in out.iter_mut().enumerate() {
        let (mut busy, mut busy_opp, mut all, mut all_opp) = (0usize, 0usize, 0usize, 0usize);
        for i in (a..chips.len().saturating_sub(1)).step_by(2) {
            let opp = sign[i] != sign[i + 1];
            all += 1;
            all_opp += usize::from(opp);
            // Ten chips round the pair, the same four after four?
            let still =
                i >= 4 && i + 5 < chips.len() && (i - 4..=i + 1).all(|j| sign[j] == sign[j + 4]);
            if !still {
                busy += 1;
                busy_opp += usize::from(opp);
            }
        }
        *o = if busy >= MIN_BUSY_PAIRS.max(all / 20) {
            busy_opp as f64 / busy as f64
        } else if all > 0 {
            all_opp as f64 / all as f64
        } else {
            0.0
        };
    }
    out
}

/// Pairs the bi-phase test needs where the chips are not fill.
const MIN_BUSY_PAIRS: usize = 64;

/// The code and chip rate behind a transition line at `line_hz` in `s`:
/// bi-phase if, at twice the line or at the line, the chips pair up
/// opposite nine times in ten; else NRZ at the line. With the first
/// segment's chip phase (samples), to start a bit synchroniser on.
pub fn choose_coding(s: &[f32], rate: f64, line_hz: f64) -> Option<(Coding, f64, f64)> {
    for rc in [2.0 * line_hz, line_hz] {
        if rate / rc < 2.5 {
            continue;
        }
        let (chips, phase) = chips_at(s, rate, rc);
        if chips.len() < 400 {
            continue;
        }
        let opp = pair_opposite(&chips);
        if opp[0].max(opp[1]) >= 0.9 {
            return Some((Coding::Biphase, rc, phase));
        }
    }
    if rate / line_hz < 2.0 {
        return None;
    }
    let (chips, phase) = chips_at(s, rate, line_hz);
    (chips.len() >= 400).then_some((Coding::Nrz, line_hz, phase))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prbs(n: usize, seed: u32) -> Vec<u8> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s & 1) as u8
            })
            .collect()
    }

    /// A rectangular data waveform at `rate`: bits at `rb`, bi-phase or not.
    fn waveform(bits: &[u8], rate: f64, rb: f64, biphase: bool, phase: f64) -> Vec<f32> {
        let n = (bits.len() as f64 * rate / rb) as usize;
        (0..n)
            .map(|i| {
                let t = (i as f64 + phase) * rb / rate;
                let k = (t as usize).min(bits.len() - 1);
                let v = if bits[k] == 1 { 1.0 } else { -1.0 };
                if biphase && t.fract() >= 0.5 { -v } else { v }
            })
            .collect()
    }

    #[test]
    fn tells_biphase_from_nrz_whichever_line_is_found() {
        let rate = 60_000.0;
        let bits = prbs(6000, 5);
        let bp = waveform(&bits, rate, 4800.0, true, 0.3);
        let (f, _) = find_clock(&bp, rate, 100.0, 20_000.0).expect("clock");
        assert!((f - 4800.0).abs() < 2.0 || (f - 9600.0).abs() < 2.0, "{f}");
        let (c, rc, _) = choose_coding(&bp, rate, f).unwrap();
        assert_eq!(c, Coding::Biphase);
        assert!((rc - 9600.0).abs() < 2.0);
        let nrz = waveform(&bits, rate, 4800.0, false, 0.3);
        let (f, _) = find_clock(&nrz, rate, 100.0, 20_000.0).expect("clock");
        assert!((f - 4800.0).abs() < 2.0, "{f}");
        let (c, rc, _) = choose_coding(&nrz, rate, f).unwrap();
        assert_eq!(c, Coding::Nrz);
        assert!((rc - 4800.0).abs() < 2.0);
    }

    #[test]
    fn finds_a_bpsk_subcarrier_in_the_phase() {
        let rate = 400_000.0;
        let fsc = 48_000.0;
        let bits = prbs(4000, 9);
        let d = waveform(&bits, rate, 2400.0, true, 0.0);
        let noise = prbs(d.len(), 77);
        let phi: Vec<f32> = d
            .iter()
            .zip(&noise)
            .enumerate()
            .map(|(i, (&v, &w))| {
                1.2 * v * (std::f64::consts::TAU * fsc * i as f64 / rate).sin() as f32
                    + if w == 1 { 0.1 } else { -0.1 }
            })
            .collect();
        let sc = find_subcarrier(&phi, rate).expect("subcarrier");
        assert!((sc.freq_hz - fsc).abs() < 60.0, "{}", sc.freq_hz);
        // Bi-phase at 2.4 kbit/s: its main lobe reaches 4.8 kHz either
        // side; unfiltered, the sidelobes carry 95 % out past 10 kHz.
        assert!(
            sc.half_width_hz > 3000.0 && sc.half_width_hz < 20_000.0,
            "{}",
            sc.half_width_hz
        );
    }

    /// Two bi-phase subcarriers (48 and 72 kHz, 4800 bit/s each, on the
    /// beacon this was fixed on): both found, neither the 60 kHz midpoint
    /// their clock lines beat into.
    #[test]
    fn two_subcarriers_are_two_not_their_midpoint() {
        let rate = 400_000.0;
        // Filtered, as a transmitter's data are: the unfiltered waveform's
        // sidelobes would fill the band.
        let smooth = |mut v: Vec<f32>| {
            for _ in 0..4 {
                let w: Vec<f32> = v
                    .iter()
                    .scan(0.0, |z, &x| (*z += 0.2 * (x - *z), Some(*z)).1)
                    .collect();
                v = w;
            }
            v
        };
        let a = smooth(waveform(&prbs(4000, 9), rate, 4800.0, true, 0.0));
        let b = smooth(waveform(&prbs(4000, 31), rate, 4800.0, true, 0.0));
        let noise = prbs(a.len(), 77);
        let tone = |f: f64, i: usize| (std::f64::consts::TAU * f * i as f64 / rate).sin() as f32;
        let phi: Vec<f32> = (0..a.len().min(b.len()))
            .map(|i| {
                0.9 * a[i] * tone(48_000.0, i)
                    + 0.9 * b[i] * tone(72_000.0, i)
                    + if noise[i] == 1 { 0.5 } else { -0.5 }
            })
            .collect();
        let subs = find_subcarriers(&phi, rate);
        let at = |f: f64| subs.iter().any(|s| (s.freq_hz - f).abs() < 150.0);
        assert!(at(48_000.0) && at(72_000.0), "{subs:?}");
        assert!(!at(60_000.0), "{subs:?}");
        assert_eq!(subs.len(), 2, "{subs:?}");
        // Each one's data reach is its own (8–10 kHz), not both.
        assert!(subs.iter().all(|s| s.half_width_hz < 14_000.0), "{subs:?}");
        // Set by hand near one of them it snaps there and measures its own.
        let one = subcarrier_at(&phi, rate, 48_500.0, None).expect("at 48 kHz");
        assert!((one.freq_hz - 48_000.0).abs() < 150.0, "{}", one.freq_hz);
        assert!(one.half_width_hz < 14_000.0, "{}", one.half_width_hz);
    }

    #[test]
    fn plain_noise_has_no_subcarrier() {
        let phi: Vec<f32> = prbs(400_000, 3)
            .iter()
            .map(|&b| if b == 1 { 0.3 } else { -0.3 })
            .collect();
        assert!(find_subcarrier(&phi, 400_000.0).is_none());
    }

    #[test]
    fn the_clock_is_the_fundamental_under_a_level_comb() {
        // Eutelsat 172B's beacon: 9600 symbol/s, mostly 0101… fill — its
        // transition line's harmonics level with it, the 7th on top.
        let line = |h: usize, db: f64| {
            let k = 6144.0 * h as f64;
            (k as usize, k + 0.2, 10f64.powf(db / 10.0))
        };
        let found = vec![
            line(7, 33.0),
            line(6, 33.0),
            line(5, 32.0),
            line(8, 31.0),
            line(4, 31.0),
            line(2, 30.0),
            line(1, 30.0),
            line(3, 30.0),
        ];
        let (kf, _) = fundamental(&found).expect("line");
        assert!((kf - 6144.2).abs() < 1.0, "{kf}");
        // A lone line dividing the top, no comb between: not the clock.
        let found = vec![line(7, 33.0), line(1, 31.0)];
        let (kf, _) = fundamental(&found).expect("line");
        assert!((kf - 7.0 * 6144.0).abs() < 1.0, "{kf}");
    }
}
