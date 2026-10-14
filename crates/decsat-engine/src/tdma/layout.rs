//! A burst's layout found blind: which symbols are the same in every burst
//! (preamble, pilots, unique words, fixed fields) and which carry data.
//!
//! Bursts of the commonest length are lined up on one of them (the lag and
//! phase that correlate best), and averaged position by position: where
//! every burst sends the same symbol the average keeps its full length;
//! where they send data it shrinks towards zero. Terminals idling send the
//! same fill burst after burst, which would make *everything* look fixed, so
//! bursts that match the average's data too are set aside as idle first and
//! the layout read from the busy ones.
//!
//! Live (Intelsat 19, 12.49 GHz, a 1.42 MS/s QPSK inroute): 856-symbol
//! bursts with a 31-symbol preamble holding a 5-symbol field that varies,
//! two pilot symbols every 30, a fixed 18-symbol field near the end, and
//! ~40 % of bursts idle fill.

use decsat_core::{Iq, Modulation};

use super::demod::{nearest, points};

/// Least bursts of one length to read a layout from.
pub const MIN_BURSTS: usize = 40;
/// Most bursts averaged.
const MAX_BURSTS: usize = 400;
/// A position averaging at least this (of a unit symbol) is the same in
/// every burst. Random QPSK over 40 bursts averages ~0.15; a known symbol
/// at 7 dB MER ~0.9.
const KNOWN: f32 = 0.6;
/// A burst matching the average's data this well is idle fill.
const IDLE_MATCH: f32 = 0.5;

/// A run of symbol positions, `start..end`.
pub type Run = std::ops::Range<usize>;

/// What the bursts of one length have in common.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// The bursts' length, symbols.
    pub len: usize,
    /// The symbol every burst sends at each position (unit magnitude, in
    /// the first burst's orientation), or `None` for data.
    pub known: Vec<Option<Iq>>,
    /// Fixed symbols at the start.
    pub preamble: Option<Run>,
    /// Positions inside the preamble's span that vary from burst to burst.
    pub varying_in_preamble: Vec<Run>,
    /// Fixed symbols at the end.
    pub postamble: Option<Run>,
    /// Pilot blocks: every `period` symbols from `first`, `block` long.
    pub pilots: Option<Pilots>,
    /// Other fixed runs (unique words, constant header fields), at least
    /// two symbols long.
    pub fields: Vec<Run>,
    /// Positions carrying data, in order.
    pub data: Vec<usize>,
    /// Share of the bursts that were idle fill.
    pub idle_share: f32,
    /// The fill burst's data symbols, when there were idle bursts.
    pub fill: Option<Vec<Iq>>,
    /// Bursts the layout was read from.
    pub bursts: usize,
    pub modulation: Modulation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pilots {
    pub period: usize,
    pub first: usize,
    pub block: usize,
}

impl Layout {
    /// One line: "856 symbols: preamble 0–35 (13–17 vary), 2 pilots every
    /// 30, fixed 704–721, 752 data symbols; 39 % idle".
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(p) = &self.preamble {
            let vary = if self.varying_in_preamble.is_empty() {
                String::new()
            } else {
                format!(
                    " ({} vary)",
                    self.varying_in_preamble
                        .iter()
                        .map(run_text)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            parts.push(format!("preamble {}{vary}", run_text(p)));
        }
        if let Some(p) = &self.pilots {
            parts.push(format!(
                "{} pilot{} every {} from {}",
                p.block,
                if p.block == 1 { "" } else { "s" },
                p.period,
                p.first
            ));
        }
        for f in &self.fields {
            parts.push(format!("fixed {}", run_text(f)));
        }
        if let Some(p) = &self.postamble {
            parts.push(format!("postamble {}", run_text(p)));
        }
        parts.push(format!("{} data symbols", self.data.len()));
        let mut s = format!("{} symbols: {}", self.len, parts.join(", "));
        if self.idle_share > 0.0 {
            s += &format!("; {:.0} % idle fill", 100.0 * self.idle_share);
        }
        s
    }

    /// Known positions, in order.
    pub fn known_positions(&self) -> impl Iterator<Item = (usize, Iq)> + '_ {
        self.known
            .iter()
            .enumerate()
            .filter_map(|(i, k)| k.map(|v| (i, v)))
    }
}

fn run_text(r: &Run) -> String {
    if r.len() == 1 {
        format!("{}", r.start)
    } else {
        format!("{}–{}", r.start, r.end - 1)
    }
}

/// The commonest burst length among `lens` (symbols), and how many bursts
/// are within ±1 % of it.
pub fn commonest_length(lens: &[usize]) -> Option<(usize, usize)> {
    if lens.is_empty() {
        return None;
    }
    let mut sorted = lens.to_vec();
    sorted.sort_unstable();
    let mut best = (0, 0);
    let mut lo = 0;
    for hi in 0..sorted.len() {
        // A window of lengths within 2 % of each other.
        while sorted[hi] as f64 > sorted[lo] as f64 * 1.02 + 1.0 {
            lo += 1;
        }
        if hi + 1 - lo > best.1 {
            best = (sorted[(lo + hi) / 2], hi + 1 - lo);
        }
    }
    Some(best)
}

/// Correlation of `a` against `b` shifted by `lag` (b[i + lag] with a[i]),
/// over `positions` (or all), normalised by the count.
fn corr_at(a: &[Iq], b: &[Iq], lag: isize, positions: Option<&[usize]>) -> Iq {
    let mut sum = Iq::new(0.0, 0.0);
    let mut n = 0usize;
    let mut add = |i: usize| {
        let j = i as isize + lag;
        if j >= 0 && (j as usize) < b.len() && i < a.len() {
            sum += a[i] * b[j as usize].conj();
            n += 1;
        }
    };
    match positions {
        Some(p) => p.iter().for_each(|&i| add(i)),
        None => (0..a.len()).for_each(add),
    }
    if n == 0 {
        Iq::new(0.0, 0.0)
    } else {
        sum / n as f32
    }
}

/// `b` lined up on `reference`: the lag (−`w`..=`w`) and phase that
/// correlate best over `positions`, and `reference.len()` symbols of `b`
/// from there, rotated (`None` past either end).
pub fn align(
    reference: &[Iq],
    b: &[Iq],
    w: usize,
    positions: Option<&[usize]>,
) -> (isize, f32, Vec<Option<Iq>>) {
    let (lag, c) = (-(w as isize)..=w as isize)
        .map(|lag| (lag, corr_at(reference, b, lag, positions)))
        .max_by(|x, y| x.1.norm().total_cmp(&y.1.norm()))
        .unwrap_or((0, Iq::new(0.0, 0.0)));
    // c = Σ ref·conj(b): b turned by θ gives c turned by −θ, so c's own
    // direction is the turn that brings b back onto the reference.
    let rot = if c.norm() > 0.0 {
        c / c.norm()
    } else {
        Iq::new(1.0, 0.0)
    };
    let out = (0..reference.len())
        .map(|i| {
            let j = i as isize + lag;
            (j >= 0 && (j as usize) < b.len()).then(|| b[j as usize] * rot)
        })
        .collect();
    (lag, c.norm(), out)
}

/// Mean of aligned bursts at each position.
fn average(rows: &[Vec<Option<Iq>>], len: usize) -> Vec<Iq> {
    let mut sum = vec![Iq::new(0.0, 0.0); len];
    let mut n = vec![0u32; len];
    for r in rows {
        for (i, v) in r.iter().enumerate() {
            if let Some(v) = v {
                sum[i] += v;
                n[i] += 1;
            }
        }
    }
    sum.iter()
        .zip(&n)
        .map(|(s, &k)| if k > 0 { s / k as f32 } else { *s })
        .collect()
}

/// Runs of `true` in `flags`.
fn runs(flags: &[bool]) -> Vec<Run> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, &f) in flags.iter().chain(std::iter::once(&false)).enumerate() {
        match (f, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push(s..i);
                start = None;
            }
            _ => {}
        }
    }
    out
}

/// Read the layout of bursts of about `len` symbols from `bursts` (symbols
/// as demodulated, unit power, each in its own orientation).
pub fn find_layout(bursts: &[&[Iq]], len: usize, modulation: Modulation) -> Option<Layout> {
    let same: Vec<&[Iq]> = bursts
        .iter()
        .copied()
        .filter(|b| (b.len() as f64 - len as f64).abs() <= len as f64 * 0.02 + 1.0)
        .take(MAX_BURSTS)
        .collect();
    if same.len() < MIN_BURSTS {
        return None;
    }
    // Lags allowed: a few symbols of edge jitter, a little more on long
    // bursts.
    let w = 6 + len / 100;
    // The reference: the burst whose length is the median's.
    let mut by_len: Vec<&[Iq]> = same.clone();
    by_len.sort_by_key(|b| b.len());
    let reference: Vec<Iq> =
        by_len[by_len.len() / 2][..len.min(by_len[by_len.len() / 2].len())].to_vec();
    let len = reference.len();

    // Pass 1: line every burst up on the reference over all its symbols.
    let rows: Vec<Vec<Option<Iq>>> = same
        .iter()
        .map(|b| align(&reference, b, w, None).2)
        .collect();
    let mean = average(&rows, len);
    // Pass 2: again, on the positions that look fixed (the reference's own
    // data no longer pulls the others), against the average.
    let fixed: Vec<usize> = (0..len).filter(|&i| mean[i].norm() > KNOWN).collect();
    if fixed.len() < 4 {
        return None;
    }
    let template: Vec<Iq> = mean.iter().map(|m| m / m.norm().max(1e-9)).collect();
    let rows: Vec<Vec<Option<Iq>>> = same
        .iter()
        .map(|b| align(&template, b, w, Some(&fixed)).2)
        .collect();
    let mean = average(&rows, len);

    // Idle fill: bursts matching the average all through. A busy burst
    // matches it only at the fixed symbols (live, an eighth of them); fill
    // matches everywhere, however much of the traffic it is — even when
    // so much is fill that its symbols average past `KNOWN` too. With no
    // idle bursts the average's data are noise and nothing matches.
    let cons: Vec<Iq> = mean.iter().map(|m| m / m.norm().max(1e-9)).collect();
    let similarity = |r: &Vec<Option<Iq>>| -> f32 {
        let (mut s, mut n) = (Iq::new(0.0, 0.0), 0);
        for (v, c) in r.iter().zip(&cons) {
            if let Some(v) = v {
                s += v * c.conj();
                n += 1;
            }
        }
        if n == 0 { 0.0 } else { s.norm() / n as f32 }
    };
    let idle: Vec<bool> = rows.iter().map(|r| similarity(r) > IDLE_MATCH).collect();
    let n_idle = idle.iter().filter(|&&i| i).count();
    let busy: Vec<Vec<Option<Iq>>> = rows
        .iter()
        .zip(&idle)
        .filter(|(_, i)| !**i)
        .map(|(r, _)| r.clone())
        .collect();
    let (mean, fill) = if busy.len() >= MIN_BURSTS / 2 && n_idle > 0 {
        let idle_rows: Vec<Vec<Option<Iq>>> = rows
            .iter()
            .zip(&idle)
            .filter(|(_, i)| **i)
            .map(|(r, _)| r.clone())
            .collect();
        (average(&busy, len), Some(average(&idle_rows, len)))
    } else {
        (mean, None)
    };

    let mut is_known: Vec<bool> = mean.iter().map(|m| m.norm() > KNOWN).collect();
    let mut rs = runs(&is_known);
    // The preamble: the run starting near the front (the first symbol or
    // two may be lost to the ramp-up), extended over the runs that follow
    // within a few symbols — the varying symbols between are a field inside
    // it (with now and then a stray fixed one among them). Pilots, tens of
    // symbols apart, end it.
    let mut preamble: Option<Run> = None;
    let mut varying = Vec::new();
    if rs.first().is_some_and(|r| r.start <= 3) {
        let mut p = rs.remove(0);
        while let Some(next) = rs.first() {
            let gap = next.start - p.end;
            // (The first run may be cut short by the ramp-up: two fixed
            // symbols and a run of more after a short gap still make one.)
            if gap <= 8 && p.len() + next.len() >= 6 {
                varying.push(p.end..next.start);
                p.end = next.end;
                rs.remove(0);
            } else {
                break;
            }
        }
        // Varying stretches split by a stray fixed symbol are one field.
        let mut merged: Vec<Run> = Vec::new();
        for v in varying {
            match merged.last_mut() {
                Some(m) if v.start - m.end <= 2 => m.end = v.end,
                _ => merged.push(v),
            }
        }
        varying = merged;
        // A terminal sending most of the bursts can lift its own field
        // values past `KNOWN` (live: half a field read as fixed): inside
        // the preamble, a position well short of the preamble's own level
        // is part of the field too.
        let mut lv: Vec<f32> = p
            .clone()
            .filter(|&i| is_known[i])
            .map(|i| mean[i].norm())
            .collect();
        if !lv.is_empty() {
            lv.sort_by(f32::total_cmp);
            let level = lv[lv.len() / 2];
            for i in p.clone() {
                if is_known[i] && mean[i].norm() < 0.75 * level {
                    is_known[i] = false;
                }
            }
            // Ends that went with it shorten the preamble.
            let start = (p.start..p.end).find(|&i| is_known[i]).unwrap_or(p.start);
            let end = (p.start..p.end)
                .rev()
                .find(|&i| is_known[i])
                .map_or(p.end, |i| i + 1);
            p = start..end;
            varying = runs(&is_known[p.clone()])
                .into_iter()
                .map(|r| r.start + p.start..r.end + p.start)
                .collect::<Vec<Run>>();
            // The gaps between fixed runs, as above.
            let fixed_runs = varying;
            varying = Vec::new();
            for w in fixed_runs.windows(2) {
                varying.push(w[0].end..w[1].start);
            }
            let mut merged: Vec<Run> = Vec::new();
            for v in varying {
                match merged.last_mut() {
                    Some(m) if v.start - m.end <= 2 => m.end = v.end,
                    _ => merged.push(v),
                }
            }
            varying = merged;
        }
        preamble = Some(p);
    }
    let known: Vec<Option<Iq>> = mean
        .iter()
        .zip(&is_known)
        .map(|(m, &k)| k.then(|| m / m.norm()))
        .collect();
    let postamble = if rs.last().is_some_and(|r| r.end + 3 >= len && r.len() >= 2) {
        rs.pop()
    } else {
        None
    };
    // Pilots: the commonest spacing between the remaining runs' starts,
    // the commonest block length, and the phase from the first.
    let pilots = find_pilots(&rs, preamble.as_ref(), len);
    let mut fields = Vec::new();
    for r in rs {
        let explained = pilots.is_some_and(|p| {
            r.start >= p.first && (r.start - p.first) % p.period == 0 && r.len() <= p.block
        });
        if !explained && r.len() >= 2 {
            fields.push(r);
        }
    }
    // Pilot positions inside the preamble or a field are part of those.
    let data: Vec<usize> = (0..len).filter(|&i| !is_known[i]).collect();
    let fill = fill.map(|f| {
        data.iter()
            .map(|&i| f[i] / f[i].norm().max(1e-9))
            .collect::<Vec<Iq>>()
    });
    Some(Layout {
        len,
        known,
        preamble,
        varying_in_preamble: varying,
        postamble,
        pilots,
        fields,
        data,
        idle_share: n_idle as f32 / rows.len() as f32,
        fill,
        bursts: rows.len(),
        modulation,
    })
}

/// Pilot blocks among the fixed runs past the preamble: a common spacing
/// that most runs keep.
fn find_pilots(rs: &[Run], preamble: Option<&Run>, len: usize) -> Option<Pilots> {
    if rs.len() < 3 {
        return None;
    }
    let starts: Vec<usize> = rs.iter().map(|r| r.start).collect();
    let mut gaps: Vec<usize> = starts.windows(2).map(|w| w[1] - w[0]).collect();
    gaps.sort_unstable();
    // The commonest gap.
    let mut best = (0, 0);
    let mut i = 0;
    while i < gaps.len() {
        let j = gaps[i..].iter().take_while(|&&g| g == gaps[i]).count();
        if j > best.1 {
            best = (gaps[i], j);
        }
        i += j;
    }
    let period = best.0;
    if period < 4 || best.1 < 2 || best.1 * 2 < gaps.len() {
        return None;
    }
    // The commonest residue of the runs' starts: a stray fixed symbol
    // beside the preamble must not set the pilots' phase.
    let first = {
        let mut count = vec![0usize; period];
        for &s in &starts {
            count[s % period] += 1;
        }
        (0..period).max_by_key(|&k| count[k]).unwrap_or(0)
    };
    let block = {
        let mut lens: Vec<usize> = rs
            .iter()
            .filter(|r| r.start % period == first)
            .map(|r| r.len())
            .collect();
        lens.sort_unstable();
        lens[lens.len() / 2]
    };
    // Back to the first pilot position in the burst (some may sit inside
    // the preamble).
    let _ = (preamble, len);
    Some(Pilots {
        period,
        first,
        block,
    })
}

/// A burst read through a layout: lined up on its known symbols, the phase
/// tracked through them, and its data symbols taken out.
#[derive(Debug, Clone)]
pub struct Fitted {
    /// The lag (symbols) at which the burst's known symbols fell.
    pub lag: isize,
    /// How well the known symbols matched, 0..1.
    pub match_known: f32,
    /// The data symbols, phase-corrected, in the layout's orientation.
    pub data: Vec<Iq>,
    /// MER over every symbol, decided against the known symbols where
    /// known, dB.
    pub mer_db: f32,
    /// Idle fill (its data match the fill).
    pub idle: bool,
    /// The symbols of the field inside the preamble (their constellation
    /// points' numbers): live, a few values, one a terminal.
    pub field: Vec<u8>,
}

/// Fit `sym` (a demodulated burst) to `layout`.
pub fn fit(layout: &Layout, sym: &[Iq]) -> Option<Fitted> {
    let known: Vec<(usize, Iq)> = layout.known_positions().collect();
    if known.len() < 4 {
        return None;
    }
    let positions: Vec<usize> = known.iter().map(|k| k.0).collect();
    let template: Vec<Iq> = (0..layout.len)
        .map(|i| layout.known[i].unwrap_or(Iq::new(0.0, 0.0)))
        .collect();
    let w = 6 + layout.len / 100;
    let (lag, m, row) = align(&template, sym, w, Some(&positions));
    // Phase through the burst: the error at each known run (averaged over
    // it), joined by straight lines.
    let mut anchors: Vec<(f64, f64)> = Vec::new();
    let mut prev: Option<f64> = None;
    let is_known: Vec<bool> = layout.known.iter().map(|k| k.is_some()).collect();
    for r in runs(&is_known) {
        let mut s = Iq::new(0.0, 0.0);
        for i in r.clone() {
            if let (Some(v), Some(k)) = (row[i], layout.known[i]) {
                s += v * k.conj();
            }
        }
        if s.norm() == 0.0 {
            continue;
        }
        let mut ph = (s.im as f64).atan2(s.re as f64);
        if let Some(p) = prev {
            ph -= std::f64::consts::TAU * ((ph - p) / std::f64::consts::TAU).round();
        }
        prev = Some(ph);
        anchors.push(((r.start + r.end - 1) as f64 / 2.0, ph));
    }
    let phase_at = |i: usize| -> f64 {
        let x = i as f64;
        match anchors.len() {
            0 => 0.0,
            1 => anchors[0].1,
            _ => {
                let k = anchors
                    .partition_point(|a| a.0 < x)
                    .clamp(1, anchors.len() - 1);
                let (a, b) = (anchors[k - 1], anchors[k]);
                a.1 + (b.1 - a.1) * (x - a.0) / (b.0 - a.0)
            }
        }
    };
    let pts = points(layout.modulation);
    let (mut sig, mut err) = (0f64, 0f64);
    let mut data = Vec::with_capacity(layout.data.len());
    let mut data_set = vec![false; layout.len];
    for &i in &layout.data {
        data_set[i] = true;
    }
    let mut in_field = vec![false; layout.len];
    for r in &layout.varying_in_preamble {
        for i in r.clone() {
            in_field[i] = true;
        }
    }
    let mut field = Vec::new();
    for (i, v) in row.iter().enumerate() {
        let Some(v) = v else { continue };
        let ph = phase_at(i);
        let c = v * Iq::new((-ph).cos() as f32, (-ph).sin() as f32);
        let ideal = match layout.known[i] {
            Some(k) => k,
            None => pts[nearest(c, pts)],
        };
        sig += ideal.norm_sqr() as f64;
        err += (c - ideal).norm_sqr() as f64;
        if data_set[i] {
            data.push(c);
        }
        if in_field[i] {
            field.push(nearest(c, pts) as u8);
        }
    }
    let idle = layout.fill.as_ref().is_some_and(|f| {
        let n = f.len().min(data.len());
        let s: Iq = data.iter().zip(f).take(n).map(|(d, f)| d * f.conj()).sum();
        n > 0 && s.norm() / n as f32 > IDLE_MATCH
    });
    Some(Fitted {
        lag,
        match_known: m,
        data,
        mer_db: (10.0 * (sig / err.max(1e-12)).log10()) as f32,
        idle,
        field,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::demod::tests::Lcg;
    use super::*;

    /// A burst of the live layout: preamble 0..36 with 13..18 varying, two
    /// pilots every 30, a fixed field at 704..722, data elsewhere.
    pub fn live_like(r: &mut Lcg, idle: bool, fill: &[Iq], fixed: &[Iq]) -> Vec<Iq> {
        let p = points(Modulation::Qpsk);
        let mut out = Vec::with_capacity(856);
        let mut d = 0;
        for (i, &f) in fixed.iter().enumerate().take(856) {
            let known =
                (i < 36 && !(13..18).contains(&i)) || (i % 30 < 2) || (704..722).contains(&i);
            if known {
                out.push(f);
            } else if idle {
                out.push(fill[d % fill.len()]);
                d += 1;
            } else {
                out.push(p[(r.next() % 4) as usize]);
            }
        }
        out
    }

    pub fn noisy(s: &[Iq], sigma: f32, rot: f32, r: &mut Lcg) -> Vec<Iq> {
        let c = Iq::from_polar(1.0, rot);
        s.iter()
            .map(|&v| v * c + Iq::new(r.gauss() * sigma, r.gauss() * sigma))
            .collect()
    }

    #[test]
    fn reads_the_live_layout() {
        let mut r = Lcg(5);
        let p = points(Modulation::Qpsk);
        let fixed: Vec<Iq> = (0..856).map(|_| p[(r.next() % 4) as usize]).collect();
        let fill: Vec<Iq> = (0..900).map(|_| p[(r.next() % 4) as usize]).collect();
        let mut bursts = Vec::new();
        for k in 0..200 {
            let b = live_like(&mut r, k % 5 < 2, &fill, &fixed);
            // Each burst in its own quarter-turn, some a symbol early or late.
            let mut b = noisy(
                &b,
                0.25,
                (k % 4) as f32 * std::f32::consts::FRAC_PI_2,
                &mut r,
            );
            match k % 3 {
                0 => {
                    b.remove(0);
                }
                1 => b.insert(0, Iq::new(0.1, 0.0)),
                _ => {}
            }
            bursts.push(b);
        }
        let refs: Vec<&[Iq]> = bursts.iter().map(|b| b.as_slice()).collect();
        let lens: Vec<usize> = refs.iter().map(|b| b.len()).collect();
        let (len, n) = commonest_length(&lens).unwrap();
        assert!(n >= 190);
        let l = find_layout(&refs, len, Modulation::Qpsk).unwrap();
        let d = l.describe();
        assert!((0.35..0.45).contains(&l.idle_share), "{d}");
        let pre = l.preamble.clone().unwrap();
        assert!(pre.start <= 1 && pre.end >= 35, "{d}");
        assert_eq!(l.varying_in_preamble.len(), 1, "{d}");
        let pil = l.pilots.unwrap();
        assert_eq!((pil.period, pil.block), (30, 2), "{d}");
        assert!(l.fields.iter().any(|f| f.len() >= 16), "{d}");
        assert!((740..770).contains(&l.data.len()), "{d}");
        // A new burst fits, its data come out in the layout's orientation.
        let b = live_like(&mut r, false, &fill, &fixed);
        let nb = noisy(&b, 0.2, 2.0, &mut r);
        let f = fit(&l, &nb).unwrap();
        assert!(f.match_known > 0.8 && !f.idle && f.mer_db > 9.0, "{f:?}");
        let idle = noisy(&live_like(&mut r, true, &fill, &fixed), 0.2, 1.0, &mut r);
        assert!(fit(&l, &idle).unwrap().idle);
    }
}
