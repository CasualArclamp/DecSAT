// made by CasualArclamp
//! Carrier detection across the whole wideband span.
//!
//! Finds every signal standing above the noise floor in an averaged spectrum
//! and estimates its centre, width and symbol rate, so the GUI can mark it on
//! the waterfall and let you claim it with one click — a VFO sized to fit.
//!
//! The symbol-rate figure is the equivalent-noise-bandwidth estimate (a few
//! percent); dropping a VFO with Identify on the carrier then pins it down to
//! well under a percent.
//!
//! Real spectra are not tidy. A HackRF overloaded by strong broadcast signals
//! (first seen live on the FM band, where a naive detector reported 168
//! "carriers") shows a dense comb of ripple; spurs do similar things at L-band.
//! So neighbouring peaks are merged when the trough between them stays within
//! a few dB of the weaker peak — ripple and combs merge, genuinely separate
//! carriers have deep troughs — and anything whose top is not flat is flagged
//! [`Carrier::rough`]: a linearly modulated carrier has a flat top, a lump of
//! other signals does not.

/// One detected carrier.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Carrier {
    /// Centre relative to the wideband centre, Hz.
    pub center_hz: f64,
    /// 99 % occupied bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Symbol-rate estimate from the equivalent noise bandwidth, Hz. Only
    /// meaningful when the carrier is neither narrow nor rough.
    pub symbol_rate_hz: f64,
    /// Peak above the noise floor, dB.
    pub snr_db: f32,
    /// Only a few bins wide: a CW tone, a spur, or a carrier too narrow to
    /// resolve at this FFT size.
    pub narrow: bool,
    /// The top is not flat: a lump of several signals, overload products, or
    /// something not linearly modulated. Not a clean single carrier.
    pub rough: bool,
}

impl Carrier {
    /// A VFO bandwidth that fits the carrier: its occupied width plus a
    /// little room for the skirts and frequency error (the VFO's filter
    /// passes everything inside it, and the demodulator's matched filter
    /// does the rest, so more only lets neighbours in).
    pub fn suggested_vfo_bandwidth(&self) -> f64 {
        if self.rough {
            return self.bandwidth_hz * 1.1;
        }
        if self.narrow {
            // Unresolved at this FFT size: its symbol-rate estimate means
            // little, so leave generous room (callers add a floor in bins).
            return self.bandwidth_hz * 2.0;
        }
        (self.bandwidth_hz * 1.12).max(self.symbol_rate_hz * 1.3)
    }

    /// `want` narrowed so a VFO centred on this carrier stops short of every
    /// other carrier in `all` (which may include this one), but never
    /// narrower than this carrier's own occupied width.
    pub fn fit_among(&self, want: f64, all: &[Carrier]) -> f64 {
        let room = all
            .iter()
            .filter(|o| (o.center_hz - self.center_hz).abs() > 1.0)
            .map(|o| (o.center_hz - self.center_hz).abs() - o.bandwidth_hz / 2.0)
            .fold(f64::INFINITY, f64::min);
        want.min(2.0 * room).max(self.bandwidth_hz)
    }

    /// [`Self::suggested_vfo_bandwidth`], kept clear of the neighbours.
    pub fn vfo_bandwidth_among(&self, all: &[Carrier]) -> f64 {
        self.fit_among(self.suggested_vfo_bandwidth(), all)
    }

    /// Looks like one clean carrier worth identifying.
    pub fn is_clean(&self) -> bool {
        !self.narrow && !self.rough
    }
}

/// Bins either side of DC treated as the receiver's own DC spur (the HackRF has
/// a strong one) when nothing wider surrounds them.
const DC_GUARD_BINS: usize = 3;
/// Gaps up to this many bins inside a carrier do not split it (a deep notch in
/// a noisy average, or a pilot tone's neighbours).
const MERGE_GAP_BINS: usize = 2;
/// A run narrower than this is flagged `narrow`.
const NARROW_BINS: usize = 6;
/// Neighbouring peaks merge when the trough between them is within this of the
/// weaker peak.
const MERGE_TROUGH_DB: f32 = 6.0;
/// A plateau whose bins scatter more than this about a straight line
/// (standard deviation, dB) is rough. A smoothed linearly modulated carrier
/// sits around 0.2 dB, ±0.5 dB transponder ripple ~0.35; a comb of ±2 dB
/// ripple ~1.4.
const ROUGH_DB: f32 = 1.0;
/// Most carriers reported; beyond this the weakest are dropped. (48 was
/// too few: a live 20 MHz slice of Ku-band SCPC held more — 43.6 kS/s
/// carriers 58.8 kHz apart — and lost real ones to the cap.)
const MAX_CARRIERS: usize = 128;

/// `q`-quantile of `xs` (0..1), by sorting a copy.
fn quantile(xs: &[f32], q: f64) -> f32 {
    let mut v = xs.to_vec();
    v.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[((v.len() - 1) as f64 * q).round() as usize]
}

/// Split `s[a..=b]` at every valley deeper than `depth` dB below the peaks on
/// both its sides, recursively; push the pieces to `out`.
///
/// Valley depth at `k` is `min(max(s[a..k]), max(s[k..=b])) - s[k]`, computed
/// from running maxima in one pass. A split is only made where **both** sides
/// have a carrier-like top (see [`top_run`]): two carriers whose skirts overlap
/// — so the spectrum never returns to the floor between them — split at their
/// trough, but a comb of single-bin teeth (seen live on the FM band, ~10 dB
/// deep) is never chopped into a row of fake carriers.
fn split_at_valleys(s: &[f32], a: usize, b: usize, depth: f32, out: &mut Vec<(usize, usize)>) {
    if b <= a + 2 * NARROW_BINS {
        out.push((a, b));
        return;
    }
    let len = b - a + 1;
    let mut right_max = vec![f32::MIN; len];
    let mut m = f32::MIN;
    for i in (0..len).rev() {
        m = m.max(s[a + i]);
        right_max[i] = m;
    }
    let mut left_max = f32::MIN;
    let mut valleys: Vec<(usize, f32)> = Vec::new();
    for i in 0..len {
        if i > 0 && i + 1 < len {
            let d = left_max.min(right_max[i + 1]) - s[a + i];
            if d > depth {
                valleys.push((a + i, d));
            }
        }
        left_max = left_max.max(s[a + i]);
    }
    // The running maxima above only find candidates. Confirm each against the
    // sides' robust top levels (95th percentile), the same reference
    // `top_run` uses: measured from the maxima, a stretch of hump between two
    // comb teeth looks like a deep valley; measured from the hump it is none.
    valleys.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal));
    valleys.truncate(64);
    let p95 = |a: usize, b: usize| {
        let mut v = s[a..=b].to_vec();
        v.sort_unstable_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
        v[(v.len() - 1) * 95 / 100]
    };
    let carrier_like = |a: usize, b: usize| top_run(s, a, b).1 >= NARROW_BINS;
    match valleys.iter().find(|&&(k, _)| {
        p95(a, k).min(p95(k + 1, b)) - s[k] > depth && carrier_like(a, k) && carrier_like(k + 1, b)
    }) {
        Some(&(k, _)) => {
            split_at_valleys(s, a, k, depth, out);
            split_at_valleys(s, k + 1, b, depth, out);
        }
        None => out.push((a, b)),
    }
}

/// The longest contiguous run of bins in `s[a..=b]` within 6 dB of the
/// region's 95th-percentile level, as (start, length).
///
/// The 95th percentile rather than the maximum, so one spur riding on a
/// carrier does not set the reference. A carrier's top run is its flat top; a
/// CW's is its main lobe; a comb's is a single tooth.
fn top_run(s: &[f32], a: usize, b: usize) -> (usize, usize) {
    let mut v = s[a..=b].to_vec();
    v.sort_unstable_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    let level = v[(v.len() - 1) * 95 / 100] - 6.0;
    let (mut best, mut cur_start, mut cur) = ((a, 0usize), a, 0usize);
    for (k, &x) in s.iter().enumerate().take(b + 1).skip(a) {
        if x >= level {
            if cur == 0 {
                cur_start = k;
            }
            cur += 1;
            if cur > best.1 {
                best = (cur_start, cur);
            }
        } else {
            cur = 0;
        }
    }
    best
}

/// Standard deviation of `v` after removing its least-squares straight line,
/// so a tilted but smooth top (a transponder's slope) does not count as rough.
fn detrended_std(v: &[f32]) -> f32 {
    let n = v.len() as f32;
    let mx = (n - 1.0) / 2.0;
    let my = v.iter().sum::<f32>() / n;
    let (mut sxy, mut sxx) = (0.0f32, 0.0f32);
    for (i, &y) in v.iter().enumerate() {
        let dx = i as f32 - mx;
        sxy += dx * (y - my);
        sxx += dx * dx;
    }
    let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    let var = v
        .iter()
        .enumerate()
        .map(|(i, &y)| {
            let r = y - (my + slope * (i as f32 - mx));
            r * r
        })
        .sum::<f32>()
        / n;
    var.sqrt()
}

/// Edges of the span this far below its strong part (90th percentile), and
/// this rough (detrended standard deviation, dB), are outside the
/// recording's passband: a filter's stopband. An averaged noise floor is
/// smooth (under 2 dB); a digital filter's stopband is a comb of sidelobes
/// and nulls (seen: about 10 dB).
const STOPBAND_DB: f32 = 30.0;
const STOPBAND_ROUGH_DB: f32 = 4.0;

/// The span's passband, as inclusive bins: all of it, but for edges that
/// are a filter's stopband. Seen: a VFO recording at 227 kS/s, its filter
/// edge at ±68 kHz and a stopband 85–120 dB down with sidelobes every
/// 4 kHz — each reported as a carrier against a floor taken from the
/// stopband, and the real carrier as 104 dB above it rather than 13.
pub(crate) fn passband(s: &[f32]) -> (usize, usize) {
    let n = s.len();
    let cut = quantile(s, 0.9) - STOPBAND_DB;
    let lo = s.iter().position(|&x| x >= cut).unwrap_or(0);
    let hi = s.iter().rposition(|&x| x >= cut).unwrap_or(n - 1);
    let stopband = |r: &[f32]| r.len() >= NARROW_BINS && detrended_std(r) > STOPBAND_ROUGH_DB;
    (
        if stopband(&s[..lo]) { lo } else { 0 },
        if stopband(&s[hi + 1..]) { hi } else { n - 1 },
    )
}

/// Detect carriers in an FFT-shifted dB spectrum (bin 0 = −rate/2).
///
/// `min_snr_db` is how far above the floor a carrier's peak must stand. The
/// floor is the 20th percentile rather than the median, so a busy span — a
/// transponder mostly full of carriers — still finds the true noise; and it
/// is taken over the passband only (see [`passband`]), where carriers are
/// looked for too.
pub fn detect_carriers(spectrum_db: &[f32], rate: f64, min_snr_db: f32) -> Vec<Carrier> {
    let full = spectrum_db.len();
    if full < 16 {
        return Vec::new();
    }
    let bin_hz = rate / full as f64;
    let (lo, hi) = passband(spectrum_db);
    let s = &spectrum_db[lo..=hi];
    let n = s.len();
    if n < 16 {
        return Vec::new();
    }
    let floor = quantile(s, 0.2);
    let above = |k: usize, margin: f32| s[k] > floor + margin;

    // 1. Runs of bins above the detection threshold, merging small gaps.
    let mut runs: Vec<(usize, usize)> = Vec::new(); // inclusive
    let mut k = 0;
    while k < n {
        if above(k, min_snr_db) {
            let start = k;
            let mut end = k;
            let mut j = k + 1;
            while j < n {
                if above(j, min_snr_db) {
                    end = j;
                    j += 1;
                } else if j + MERGE_GAP_BINS < n
                    && (j..=j + MERGE_GAP_BINS).any(|g| above(g, min_snr_db))
                {
                    j += 1;
                } else {
                    break;
                }
            }
            runs.push((start, end));
            k = end + 1;
        } else {
            k += 1;
        }
    }

    // 2. Grow each run outward over its skirts (3 dB above the floor), up to
    //    its neighbours.
    let mut grown: Vec<(usize, usize)> = Vec::with_capacity(runs.len());
    for (idx, &(a0, b0)) in runs.iter().enumerate() {
        let left_limit = if idx == 0 { 0 } else { runs[idx - 1].1 + 1 };
        let right_limit = runs.get(idx + 1).map_or(n - 1, |r| r.0 - 1);
        let mut a = a0;
        while a > left_limit && above(a - 1, 3.0) {
            a -= 1;
        }
        let mut b = b0;
        while b < right_limit && above(b + 1, 3.0) {
            b += 1;
        }
        grown.push((a, b));
    }

    // 3. Join regions that touch (the spectrum never fell to the floor between
    //    them), then split every region at its deep valleys. Ripple, combs and
    //    lumps have shallow valleys and become one region instead of dozens of
    //    "carriers"; carriers whose skirts overlap still split at the trough.
    let mut joined: Vec<(usize, usize)> = Vec::with_capacity(grown.len());
    for (a, b) in grown {
        match joined.last_mut() {
            Some(last) if a <= last.1 + 1 => last.1 = last.1.max(b),
            _ => joined.push((a, b)),
        }
    }
    let mut pieces: Vec<(usize, usize)> = Vec::new();
    for (a, b) in joined {
        split_at_valleys(s, a, b, MERGE_TROUGH_DB, &mut pieces);
    }
    // A piece that never reaches the detection threshold is a shoulder.
    let merged: Vec<(usize, usize)> = pieces
        .into_iter()
        .filter(|&(a, b)| (a..=b).any(|i| above(i, min_snr_db)))
        .collect();

    // 4. Describe each region.
    let floor_lin = 10f64.powf(floor as f64 / 10.0);
    // DC, and frequencies, in the whole span's terms (bin `k` of the
    // passband is bin `lo + k` of the span).
    let dc = (full / 2).saturating_sub(lo);
    let freq = |k: usize| ((lo + k) as f64 / full as f64 - 0.5) * rate;
    let mut out = Vec::new();
    for (a, b) in merged {
        let width = b - a + 1;
        // The DC spur on its own is not a carrier.
        if width <= 2 * DC_GUARD_BINS + 1 && a + DC_GUARD_BINS >= dc && b <= dc + DC_GUARD_BINS {
            continue;
        }

        // Lines on the carrier (unscrambled data, a pilot tone) left out of
        // its level and flatness.
        let flat = crate::estimate::flatten_lines(&s[a..=b], floor);
        let excess: Vec<f64> = flat
            .iter()
            .map(|&v| (10f64.powf(v as f64 / 10.0) - floor_lin).max(0.0))
            .collect();
        let total: f64 = excess.iter().sum();
        if total <= 0.0 {
            continue;
        }
        let center = excess
            .iter()
            .enumerate()
            .map(|(i, &p)| p * freq(a + i))
            .sum::<f64>()
            / total;

        // Narrowest window holding 99 % of the excess power, and the
        // equivalent noise bandwidth over the plateau.
        let (best_lo, best) = occupied_window(&excess);
        let rs = enbw_bins(&excess, best_lo, best) * bin_hz;

        // A narrow line has a short top run that also holds most of the power:
        // a CW's main lobe, even with window leakage widening its region. A
        // comb on a hump also has a short top run (one tooth), but the tooth
        // holds little of the region's power, so the region is a rough lump
        // rather than a line.
        let (ts, tl) = top_run(s, a, b);
        let top_power: f64 = excess[ts - a..ts - a + tl].iter().sum();
        let narrow = width < NARROW_BINS || (tl < NARROW_BINS && top_power >= 0.5 * total);
        // Flatness over the central 40 % of the region, in dB. That stays on
        // the flat top of any legal roll-off (α = 0.35 is flat over 48 % of its
        // width) yet spans many periods of any comb. Not over the half-power
        // plateau: that clips a ripple's troughs out and hides it.
        let core: Vec<f32> = if width >= 20 {
            let lo = width * 3 / 10;
            let hi = width - 1 - width * 3 / 10;
            flat[lo..=hi].to_vec()
        } else {
            Vec::new()
        };
        let rough = core.len() >= 6 && detrended_std(&core) > ROUGH_DB;

        let peak = s[a..=b].iter().copied().fold(f32::MIN, f32::max);
        out.push(Carrier {
            center_hz: center,
            bandwidth_hz: best as f64 * bin_hz,
            symbol_rate_hz: rs,
            snr_db: peak - floor,
            narrow,
            rough,
        });
    }

    // 5. If there are too many, keep clean carriers first, then rough lumps,
    //    then narrow lines, strongest first within each — so a comb of spurs
    //    (seen live: a regular ~100 kHz comb across the whole FM band) cannot
    //    crowd a weaker real carrier out. Then back into frequency order.
    if out.len() > MAX_CARRIERS {
        let rank = |c: &Carrier| {
            if c.is_clean() {
                0
            } else if c.rough {
                1
            } else {
                2
            }
        };
        out.sort_by(|x, y| {
            rank(x).cmp(&rank(y)).then(
                y.snr_db
                    .partial_cmp(&x.snr_db)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        });
        out.truncate(MAX_CARRIERS);
        out.sort_by(|x, y| {
            x.center_hz
                .partial_cmp(&y.center_hz)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    out
}

/// The narrowest window of `excess` (linear power above the floor, a bin
/// each) holding 99 % of its total, as (first bin, bins).
fn occupied_window(excess: &[f64]) -> (usize, usize) {
    let total: f64 = excess.iter().sum();
    let target = 0.99 * total;
    let (mut lo, mut acc, mut best, mut best_lo) = (0usize, 0.0f64, excess.len(), 0usize);
    for hi in 0..excess.len() {
        acc += excess[hi];
        while lo < hi && acc - excess[lo] >= target {
            acc -= excess[lo];
            lo += 1;
        }
        if acc >= target && hi - lo + 1 < best {
            best = hi - lo + 1;
            best_lo = lo;
        }
    }
    (best_lo, best)
}

/// The equivalent noise bandwidth of `excess` in bins: its total over its
/// plateau level — the median of the bins within 3 dB of the peak, or the
/// core level of its occupied window `(lo, len)` where that is lower.
fn enbw_bins(excess: &[f64], lo: usize, len: usize) -> f64 {
    let total: f64 = excess.iter().sum();
    let max_e = excess.iter().copied().fold(0.0, f64::max);
    let mut plateau: Vec<f64> = excess
        .iter()
        .copied()
        .filter(|&e| e >= 0.5 * max_e)
        .collect();
    if plateau.is_empty() {
        return 0.0;
    }
    plateau.sort_unstable_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    let p0 = plateau[plateau.len() / 2].min(crate::estimate::core_level(excess, lo, len));
    if p0 > 0.0 { total / p0 } else { 0.0 }
}

/// A second look at the narrow lines in a finer spectrum of the same span
/// (`fine_db`: FFT-shifted dB like [`detect_carriers`]'s input, many more
/// bins). A slow carrier only a few coarse bins wide — a 9.6 kS/s link is
/// three bins at 20 MS/s and 4096 points, the same as a CW with its window
/// leakage — resolves into a flat-topped carrier here and is reported as
/// one, its centre, width and symbol rate measured at this resolution; a
/// tone or a spur is a line at any resolution and stays `narrow`.
///
/// Seen live: the PH-ML radar link (9.6 kS/s) was a "narrow line" in the
/// GUI at 20 MS/s, never offered as a carrier, while an 8 MS/s capture
/// found it.
pub fn resolve_narrow(carriers: &mut [Carrier], fine_db: &[f32], rate: f64) {
    let n = fine_db.len();
    if n < 64 {
        return;
    }
    let bin_hz = rate / n as f64;
    let freq = |k: usize| (k as f64 / n as f64 - 0.5) * rate;
    for c in carriers.iter_mut().filter(|c| c.narrow) {
        let k0 = ((c.center_hz / rate + 0.5) * n as f64).round();
        if !(0.0..n as f64).contains(&k0) {
            continue;
        }
        let k0 = k0 as usize;
        // The line's neighbourhood: twice its coarse width either side; the
        // floor from twice that again.
        let half = ((2.0 * c.bandwidth_hz / bin_hz).ceil() as usize).max(8);
        let lo = k0.saturating_sub(2 * half);
        let hi = (k0 + 2 * half).min(n - 1);
        let floor = quantile(&fine_db[lo..=hi], 0.2);
        let peak = (k0.saturating_sub(half)..=(k0 + half).min(n - 1))
            .max_by(|&x, &y| {
                fine_db[x]
                    .partial_cmp(&fine_db[y])
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(k0);
        if fine_db[peak] < floor + 6.0 {
            continue;
        }
        // Its region: outward from the peak over everything above the
        // floor's skirts, across gaps of up to MERGE_GAP_BINS.
        let above = |k: usize| fine_db[k] > floor + 3.0;
        let mut a = peak;
        while a > lo && (a.saturating_sub(MERGE_GAP_BINS + 1).max(lo)..a).any(above) {
            a -= 1;
        }
        let mut b = peak;
        while b < hi && (b + 1..=(b + MERGE_GAP_BINS + 1).min(hi)).any(above) {
            b += 1;
        }
        // A line's top run is its main lobe (three bins at most with the
        // Hann window); a carrier's is its flat top, Rs (1 − α) wide.
        let (_, tl) = top_run(fine_db, a, b);
        if tl < NARROW_BINS {
            continue;
        }
        let floor_lin = 10f64.powf(floor as f64 / 10.0);
        let excess: Vec<f64> = fine_db[a..=b]
            .iter()
            .map(|&v| (10f64.powf(v as f64 / 10.0) - floor_lin).max(0.0))
            .collect();
        let total: f64 = excess.iter().sum();
        if total <= 0.0 {
            continue;
        }
        let (best_lo, best) = occupied_window(&excess);
        c.center_hz = excess
            .iter()
            .enumerate()
            .map(|(i, &p)| p * freq(a + i))
            .sum::<f64>()
            / total;
        c.bandwidth_hz = best as f64 * bin_hz;
        c.symbol_rate_hz = enbw_bins(&excess, best_lo, best) * bin_hz;
        c.snr_db = fine_db[peak] - floor;
        c.narrow = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A spectrum with raised-cosine carriers `(centre Hz, Rs, alpha, level dB)`
    /// over a flat floor, plus a DC spur.
    fn spectrum(n: usize, rate: f64, carriers: &[(f64, f64, f64, f32)], dc_spur: bool) -> Vec<f32> {
        let floor = -80.0f32;
        let mut lin: Vec<f64> = vec![10f64.powf(floor as f64 / 10.0); n];
        for &(c, rs, alpha, level) in carriers {
            let p0 = 10f64.powf(level as f64 / 10.0);
            for (k, v) in lin.iter_mut().enumerate() {
                let f = (k as f64 / n as f64 - 0.5) * rate - c;
                let a = f.abs();
                let flat = (1.0 - alpha) * rs / 2.0;
                let edge = (1.0 + alpha) * rs / 2.0;
                let shape = if a <= flat {
                    1.0
                } else if a <= edge {
                    0.5 * (1.0 + (std::f64::consts::PI / (alpha * rs) * (a - flat)).cos())
                } else {
                    0.0
                };
                *v += p0 * shape;
            }
        }
        if dc_spur {
            lin[n / 2] += 10f64.powf(-40.0 / 10.0);
        }
        lin.iter().map(|&p| (10.0 * p.log10()) as f32).collect()
    }

    /// Deterministic ±`amp` dB jitter, like a smoothed spectrum's noise.
    fn jitter(s: &mut [f32], amp: f32, seed: u64) {
        let mut x = seed | 1;
        for v in s.iter_mut() {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            let u = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u32 << 24) as f32;
            *v += (u - 0.5) * 2.0 * amp;
        }
    }

    #[test]
    fn finds_two_carriers_and_ignores_the_dc_spur() {
        let rate = 20e6;
        let n = 8192;
        let s = spectrum(
            n,
            rate,
            &[(-4.0e6, 2.0e6, 0.20, -50.0), (3.5e6, 500e3, 0.35, -55.0)],
            true,
        );
        let found = detect_carriers(&s, rate, 6.0);
        assert_eq!(found.len(), 2, "{found:#?}");

        let bin = rate / n as f64;
        let a = found[0];
        assert!((a.center_hz + 4.0e6).abs() < 3.0 * bin, "{a:?}");
        assert!((a.symbol_rate_hz - 2.0e6).abs() / 2.0e6 < 0.03, "{a:?}");
        assert!(a.is_clean(), "{a:?}");

        let b = found[1];
        assert!((b.center_hz - 3.5e6).abs() < 3.0 * bin, "{b:?}");
        assert!((b.symbol_rate_hz - 500e3).abs() / 500e3 < 0.05, "{b:?}");
        assert!(b.snr_db > 20.0);
    }

    /// A VFO recording: 64 kS/s QPSK 13 dB above the receiver's noise,
    /// the VFO's filter edge at ±68 kHz of 227 kS/s, and beyond it a
    /// stopband 85 dB and more below the carrier — a comb of sidelobes
    /// every 4 kHz with nulls between. One carrier, 13 dB up; no sidelobe
    /// a carrier.
    #[test]
    fn a_vfo_filters_stopband_is_not_carriers() {
        let rate = 227_273.0;
        let n = 4096;
        let mut s = spectrum(n, rate, &[(0.0, 64e3, 0.35, -67.0)], false);
        for (k, v) in s.iter_mut().enumerate() {
            let f = (k as f64 / n as f64 - 0.5) * rate;
            if f.abs() > 68e3 {
                let lobe = (std::f64::consts::PI * f / 4.1e3).sin().abs().max(1e-2);
                *v = (-152.0 - 0.6 * (f.abs() - 68e3) / 1e3 + 20.0 * lobe.log10()) as f32;
            }
        }
        jitter(&mut s, 0.3, 11);
        let found = detect_carriers(&s, rate, 6.0);
        assert_eq!(found.len(), 1, "{found:#?}");
        let c = found[0];
        assert!(c.center_hz.abs() < 1e3, "{c:?}");
        assert!((c.symbol_rate_hz - 64e3).abs() / 64e3 < 0.05, "{c:?}");
        assert!((c.snr_db - 13.0).abs() < 1.5, "{c:?}");
    }

    /// A carrier 60 dB over a smooth floor is not a filter's passband: the
    /// floor beside it stays the floor.
    #[test]
    fn a_very_strong_carrier_keeps_its_floor() {
        let rate = 2e6;
        let n = 4096;
        let mut s = spectrum(n, rate, &[(0.2e6, 500e3, 0.35, -20.0)], false);
        jitter(&mut s, 0.5, 3);
        let found = detect_carriers(&s, rate, 6.0);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].snr_db > 55.0, "{:?}", found[0]);
    }

    #[test]
    fn adjacent_carriers_stay_separate() {
        let rate = 10e6;
        let n = 8192;
        let s = spectrum(
            n,
            rate,
            &[(-0.7e6, 1.0e6, 0.20, -50.0), (0.7e6, 1.0e6, 0.20, -50.0)],
            false,
        );
        let found = detect_carriers(&s, rate, 6.0);
        assert_eq!(found.len(), 2, "{found:#?}");
    }

    #[test]
    fn tightly_packed_carriers_with_deep_troughs_stay_separate() {
        // Skirts touching, so the spectrum never returns to the floor between
        // them -- but the trough is far below both peaks, so they are two.
        let rate = 10e6;
        let n = 8192;
        let s = spectrum(
            n,
            rate,
            &[(-0.62e6, 1.0e6, 0.35, -50.0), (0.62e6, 1.0e6, 0.35, -50.0)],
            false,
        );
        let found = detect_carriers(&s, rate, 6.0);
        assert_eq!(found.len(), 2, "{found:#?}");
    }

    #[test]
    fn a_comb_of_ripple_is_one_rough_lump_not_dozens_of_carriers() {
        // Regression: the live FM band through an overloaded HackRF showed a
        // dense comb, and every ripple peak was reported as a carrier (168).
        let rate = 20e6;
        let n = 4096;
        let mut s = spectrum(n, rate, &[], false);
        let bin = rate / n as f64;
        for (k, v) in s.iter_mut().enumerate() {
            let f = (k as f64 / n as f64 - 0.5) * rate;
            if (-9.5e6..-2.5e6).contains(&f) {
                // A hump 20 dB up with 4 dB ripple every ~130 kHz.
                let ripple = 2.0 * (std::f64::consts::TAU * f / 130e3).cos();
                *v = -60.0 + ripple as f32;
            }
        }
        jitter(&mut s, 0.3, 7);
        let found = detect_carriers(&s, rate, 6.0);
        assert!(found.len() <= 3, "{} regions: {found:#?}", found.len());
        let lump = found
            .iter()
            .max_by(|a, b| a.bandwidth_hz.partial_cmp(&b.bandwidth_hz).unwrap())
            .unwrap();
        assert!(lump.bandwidth_hz > 6.0e6, "{lump:?} (bin {bin})");
        assert!(lump.rough, "a comb must be flagged rough: {lump:?}");
    }

    #[test]
    fn a_noisy_but_clean_carrier_is_not_rough() {
        let rate = 8e6;
        let n = 4096;
        let mut s = spectrum(n, rate, &[(1.0e6, 1.0e6, 0.2, -55.0)], false);
        jitter(&mut s, 0.6, 3);
        let found = detect_carriers(&s, rate, 6.0);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].is_clean(), "{:?}", found[0]);
    }

    #[test]
    fn cw_tone_is_flagged_narrow() {
        let rate = 2e6;
        let n = 4096;
        let mut s = spectrum(n, rate, &[], false);
        s[3000] = -30.0;
        let found = detect_carriers(&s, rate, 6.0);
        assert_eq!(found.len(), 1);
        assert!(found[0].narrow);
    }

    #[test]
    fn empty_span_finds_nothing() {
        let s = spectrum(4096, 2e6, &[], true);
        assert!(detect_carriers(&s, 2e6, 6.0).is_empty());
    }

    #[test]
    fn too_many_keeps_the_strongest() {
        // 160 well-separated tones of rising strength: only MAX_CARRIERS
        // come back, the strongest ones, in frequency order.
        let rate = 20e6;
        let n = 8192;
        let mut s = spectrum(n, rate, &[], false);
        for i in 0..160usize {
            s[200 + i * 48] = -60.0 + i as f32 * 0.1;
        }
        let found = detect_carriers(&s, rate, 6.0);
        assert_eq!(found.len(), MAX_CARRIERS);
        assert!(found.windows(2).all(|w| w[0].center_hz < w[1].center_hz));
        let weakest = found.iter().map(|c| c.snr_db).fold(f32::MAX, f32::min);
        assert!(
            weakest > 20.0 + (160 - MAX_CARRIERS) as f32 * 0.1 - 0.3,
            "{weakest}"
        );
    }

    #[test]
    fn a_deep_comb_on_a_hump_is_one_rough_lump_not_carriers() {
        // Regression, from the live FM band: one-bin teeth 10 dB above a hump
        // that is itself well above the floor. Splitting at those valleys
        // would chop it into dozens of fake carriers; it must stay one rough
        // region, and nothing in it may be offered as a clean carrier.
        let rate = 20e6;
        let n = 4096;
        let mut s = spectrum(n, rate, &[], false);
        let bin = rate / n as f64;
        let period = (110e3 / bin).round() as usize; // ~22 bins
        for (k, v) in s.iter_mut().enumerate() {
            let f = (k as f64 / n as f64 - 0.5) * rate;
            if (-9.0e6..-4.0e6).contains(&f) {
                // Hump at -62 dB, with one-bin teeth 10 dB up.
                *v = if k % period == 0 { -52.0 } else { -62.0 };
            }
        }
        jitter(&mut s, 0.3, 11);
        let found = detect_carriers(&s, rate, 6.0);
        let clean: Vec<_> = found.iter().filter(|c| c.is_clean()).collect();
        assert!(
            clean.is_empty(),
            "{} clean carriers in a comb: {clean:#?}",
            clean.len()
        );
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].rough && found[0].bandwidth_hz > 4.0e6,
            "{:?}",
            found[0]
        );
    }

    #[test]
    fn a_cw_tone_with_window_leakage_is_narrow() {
        // The scene's CW: Hann leakage widens its region past NARROW_BINS, but
        // above its pedestal it is a line.
        let rate = 8e6;
        let n = 4096;
        let mut s = spectrum(n, rate, &[], false);
        for (d, v) in [
            (0usize, -20.0f32),
            (1, -26.0),
            (2, -40.0),
            (3, -55.0),
            (4, -66.0),
            (5, -74.0),
        ] {
            s[1000 + d] = v;
            s[1000 - d] = v;
        }
        let found = detect_carriers(&s, rate, 6.0);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].narrow, "{:?}", found[0]);
    }

    #[test]
    fn a_comb_of_spurs_cannot_crowd_out_a_real_carrier() {
        // 170 strong narrow spurs plus one weaker clean carrier: the
        // carrier must survive the cap.
        let rate = 20e6;
        let n = 8192;
        let mut s = spectrum(n, rate, &[(6.0e6, 500e3, 0.2, -66.0)], false);
        for i in 0..170usize {
            s[100 + i * 37] = -40.0;
        }
        let found = detect_carriers(&s, rate, 6.0);
        assert_eq!(found.len(), MAX_CARRIERS);
        assert!(
            found
                .iter()
                .any(|c| c.is_clean() && (c.center_hz - 6.0e6).abs() < 20e3),
            "the real carrier was dropped"
        );
    }

    #[test]
    fn suggested_vfo_fits_the_carrier() {
        let c = Carrier {
            center_hz: 0.0,
            bandwidth_hz: 1.15e6,
            symbol_rate_hz: 1.0e6,
            snr_db: 20.0,
            narrow: false,
            rough: false,
        };
        // Room for the skirts, not much more.
        let bw = c.suggested_vfo_bandwidth();
        assert!((1.25e6..=1.4e6).contains(&bw), "{bw}");
    }

    #[test]
    fn a_vfo_stops_short_of_a_close_neighbour() {
        let c = |center_hz: f64, bandwidth_hz: f64| Carrier {
            center_hz,
            bandwidth_hz,
            symbol_rate_hz: bandwidth_hz / 1.2,
            snr_db: 20.0,
            narrow: false,
            rough: false,
        };
        // 1 MHz wide (1.12 MHz suggested), a 500 kHz one whose edge is
        // 540 kHz from its centre.
        let all = [c(0.0, 1.0e6), c(790e3, 500e3), c(-5e6, 1e6)];
        let bw = all[0].vfo_bandwidth_among(&all);
        assert!((bw - 1.08e6).abs() < 1.0, "{bw}");
        // Alone, it gets its full suggestion.
        assert_eq!(
            all[0].vfo_bandwidth_among(&all[..1]),
            all[0].suggested_vfo_bandwidth()
        );
        // Touching neighbours never squeeze it below its own width.
        let tight = [c(0.0, 1.0e6), c(600e3, 400e3)];
        assert_eq!(tight[0].vfo_bandwidth_among(&tight), 1.0e6);
    }
    #[test]
    fn a_slow_carrier_resolves_at_finer_resolution() {
        // A 9.6 kS/s link at 20 MS/s: two or three bins of 4096, a narrow
        // line like a CW — until a 65536-point look shows its flat top.
        let rate = 20e6;
        let carriers = [(-1.06e6, 9.6e3, 0.35, -50.0)];
        let coarse = spectrum(4096, rate, &carriers, false);
        let mut found = detect_carriers(&coarse, rate, 6.0);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].narrow, "{:?}", found[0]);

        let mut fine = spectrum(65536, rate, &carriers, false);
        jitter(&mut fine, 0.8, 5);
        resolve_narrow(&mut found, &fine, rate);
        let c = found[0];
        assert!(c.is_clean(), "{c:?}");
        assert!((c.center_hz + 1.06e6).abs() < 500.0, "{c:?}");
        assert!((c.symbol_rate_hz - 9.6e3).abs() / 9.6e3 < 0.1, "{c:?}");
        assert!((10e3..20e3).contains(&c.bandwidth_hz), "{c:?}");
        assert!(c.snr_db > 25.0, "{c:?}");
    }

    #[test]
    fn a_cw_stays_a_line_at_finer_resolution() {
        let rate = 20e6;
        let mut coarse = spectrum(4096, rate, &[], false);
        coarse[1500] = -30.0;
        let mut found = detect_carriers(&coarse, rate, 6.0);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].narrow, "{:?}", found[0]);
        // The same tone at 16× the resolution: a Hann main lobe and its
        // leakage, nothing wider.
        let mut fine = spectrum(65536, rate, &[], false);
        let k = 1500 * 16;
        for (d, v) in [
            (0usize, -30.0f32),
            (1, -36.0),
            (2, -50.0),
            (3, -65.0),
            (4, -74.0),
        ] {
            fine[k + d] = v;
            fine[k - d] = v;
        }
        jitter(&mut fine, 0.8, 9);
        resolve_narrow(&mut found, &fine, rate);
        assert!(found[0].narrow, "{:?}", found[0]);
    }

    #[test]
    fn a_resolved_carrier_keeps_its_place_among_wide_neighbours() {
        // The slow carrier between two SCPC carriers 60 kHz away: only the
        // line itself is re-measured, the neighbours are untouched.
        let rate = 20e6;
        let carriers = [
            (-1.12e6, 43.6e3, 0.25, -50.0),
            (-1.06e6, 9.6e3, 0.35, -52.0),
            (-1.0e6, 43.6e3, 0.25, -50.0),
        ];
        let coarse = spectrum(4096, rate, &carriers, false);
        let mut found = detect_carriers(&coarse, rate, 6.0);
        let before = found.clone();
        let fine = spectrum(65536, rate, &carriers, false);
        resolve_narrow(&mut found, &fine, rate);
        assert_eq!(found.len(), before.len());
        for (x, y) in found.iter().zip(&before) {
            if y.narrow {
                assert!(
                    !x.narrow && (x.symbol_rate_hz - 9.6e3).abs() < 1.5e3,
                    "{x:?}"
                );
            } else {
                assert_eq!(x, y);
            }
        }
        assert!(
            found
                .iter()
                .any(|c| !c.narrow && (c.center_hz + 1.06e6).abs() < 1e3),
            "{found:#?}"
        );
    }
}
