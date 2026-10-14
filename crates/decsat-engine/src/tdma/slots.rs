//! The time plan behind the bursts: the slots they keep, the frame that
//! repeats them, and the terminals sending them, told apart by what each
//! one's hardware puts on its bursts — power, the carrier offset of its
//! oscillator, and where in its slot its ranging puts it.
//!
//! The slot is measured locally, from the spacing of consecutive bursts: a
//! frame need not hold a whole number of slots (live, 125 ms frames of
//! 204¼ slots of 0.6116 ms — a silence of some slots' length closes each),
//! so one grid laid over many frames would slip. The frame is found from
//! where the silences fall: slots left empty recur at the frame's period.

use rustfft::FftPlanner;

use decsat_core::Iq;

/// A burst as the time plan sees it.
#[derive(Debug, Clone, Copy)]
pub struct BurstTime {
    /// First symbol (the layout's symbol 0 once a layout is known), in
    /// symbols since the stream began.
    pub t: f64,
    /// Length, symbols.
    pub len: f64,
    pub power_db: f32,
    /// Carrier offset, Hz.
    pub freq_hz: f64,
    pub idle: bool,
}

/// The frame the slots repeat in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    /// Length, symbols.
    pub period: f64,
    /// Where a frame's first slot starts (its silence just over), symbols,
    /// modulo the period.
    pub phase: f64,
    /// The silence that ends each frame, symbols.
    pub silence: f64,
}

impl Frame {
    /// Slots in a frame, the silence left out.
    pub fn slots(&self, slot: f64) -> f64 {
        (self.period - self.silence) / slot
    }

    /// The start of the frame `t` (symbols) falls in.
    pub fn start_of(&self, t: f64) -> f64 {
        self.phase + ((t - self.phase) / self.period).floor() * self.period
    }
}

/// The slots.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlotGrid {
    /// Slot length, symbols.
    pub period: f64,
    /// Slot length less the commonest burst length: the guard time.
    pub guard: f64,
    /// How far consecutive bursts stray from whole slots apart, RMS,
    /// symbols (each burst's share): how closely the terminals are ranged.
    pub spread: f64,
    /// Share of the slots that held a burst.
    pub occupancy: f32,
    pub frame: Option<Frame>,
}

impl SlotGrid {
    /// Where `t` falls: its slot in its frame, and how far into that slot
    /// (symbols). `None` without a frame.
    pub fn place(&self, t: f64) -> Option<(usize, f64)> {
        let f = self.frame?;
        let x = t - f.start_of(t);
        let k = (x / self.period).round();
        Some((k.max(0.0) as usize, x - k * self.period))
    }
}

/// The slot grid that `bursts` keep, or `None` when their starts keep none.
pub fn find_grid(bursts: &[BurstTime]) -> Option<SlotGrid> {
    if bursts.len() < 20 {
        return None;
    }
    let mut b: Vec<BurstTime> = bursts.to_vec();
    b.sort_by(|x, y| x.t.total_cmp(&y.t));
    let mut lens: Vec<f64> = b.iter().map(|x| x.len).collect();
    lens.sort_by(f64::total_cmp);
    let typical = lens[lens.len() / 2];
    // The commonest spacing of consecutive starts at least a burst apart:
    // one slot (bursts a slot or more apart are whole slots apart).
    let d: Vec<f64> = b.windows(2).map(|w| w[1].t - w[0].t).collect();
    let mut r: Vec<i64> = d
        .iter()
        .filter(|&&g| g >= typical * 0.9)
        .map(|&g| g.round() as i64)
        .collect();
    if r.len() < 10 {
        return None;
    }
    r.sort_unstable();
    let mut best = (0i64, 0usize);
    let mut i = 0;
    while i < r.len() {
        // Spacings within two symbols of each other count together.
        let j = r[i..].iter().take_while(|&&g| g - r[i] <= 2).count();
        if j > best.1 {
            best = (r[i] + 1, j);
        }
        i += r[i..].iter().take_while(|&&g| g == r[i]).count();
    }
    if best.1 * 4 < r.len() {
        return None;
    }
    // Refined over every spacing of 1–4 slots: least squares through the
    // origin, Σd / Σk.
    let p0 = best.0 as f64;
    let (mut sd, mut sk) = (0f64, 0f64);
    let mut resid = Vec::new();
    for &g in &d {
        let k = (g / p0).round();
        if (1.0..=4.0).contains(&k) && (g - k * p0).abs() < 3.0 {
            sd += g;
            sk += k;
        }
    }
    if sk < 10.0 {
        return None;
    }
    let period = sd / sk;
    for &g in &d {
        let k = (g / period).round();
        if (1.0..=4.0).contains(&k) {
            let e = g - k * period;
            if e.abs() < 0.25 * period {
                resid.push(e);
            }
        }
    }
    let spread = (resid.iter().map(|e| e * e).sum::<f64>() / resid.len().max(1) as f64).sqrt()
        / std::f64::consts::SQRT_2;
    // Slots in use: bins a slot wide, marked where a burst starts.
    let t0 = b[0].t;
    let span = b[b.len() - 1].t - t0;
    let bins = (span / period).ceil() as usize + 1;
    let mut used = vec![false; bins];
    for x in &b {
        let k = ((x.t - t0) / period).round() as usize;
        if k < bins {
            used[k] = true;
        }
    }
    let occupancy = used.iter().filter(|&&u| u).count() as f32 / bins as f32;
    let frame = find_frame(&b, &used, period);
    Some(SlotGrid {
        period,
        guard: period - typical,
        spread,
        occupancy,
        frame,
    })
}

/// The frame: roughly, the period with which empty slots recur (their
/// indicator's autocorrelation peak); then exactly, from the silences —
/// the longest gaps, one a frame — whose spacings are whole frames.
fn find_frame(b: &[BurstTime], used: &[bool], slot: f64) -> Option<Frame> {
    let n = used.len();
    let empty = used.iter().filter(|&&u| !u).count();
    // Needs empty slots, and enough frames' worth of slots.
    if n < 256 || empty < 8 || empty * 10 > n * 9 {
        return None;
    }
    let size = (2 * n).next_power_of_two();
    let mean = empty as f32 / n as f32;
    let mut buf: Vec<Iq> = vec![Iq::new(0.0, 0.0); size];
    for (x, &u) in buf.iter_mut().zip(used) {
        *x = Iq::new(if u { 0.0 } else { 1.0 } - mean, 0.0);
    }
    let mut planner = FftPlanner::new();
    planner.plan_fft_forward(size).process(&mut buf);
    for x in &mut buf {
        *x = Iq::new(x.norm_sqr(), 0.0);
    }
    planner.plan_fft_inverse(size).process(&mut buf);
    let r0 = buf[0].re.max(1e-12);
    // Lags up to a third of the span, so three frames at least are seen. A
    // frame of a whole-and-a-bit slots spreads its peak over two
    // neighbouring lags, so they are summed; and a frame's multiples
    // correlate as well as the frame (or better, when a few frames come
    // nearer a whole number of slots), so the best lag's fractions are
    // tried first.
    let r: Vec<f32> = (0..n / 3)
        .map(|l| buf[l].re / r0 * n as f32 / (n - l) as f32)
        .collect();
    let pair = |l: usize| r[l] + r.get(l + 1).copied().unwrap_or(0.0).max(0.0);
    let (best, peak) = (2..r.len())
        .map(|l| (l, pair(l)))
        .max_by(|x, y| x.1.total_cmp(&y.1))?;
    if peak <= 0.4 {
        return None;
    }
    let lag = (2..=8)
        .rev()
        .map(|d| best / d)
        .find(|&l| {
            l >= 2 && (l.saturating_sub(1)..=l + 1).any(|k| k < r.len() && pair(k) >= 0.5 * peak)
        })
        .unwrap_or(best);
    let rough = lag as f64 * slot;
    // The silences: of the gaps (from a burst's end to the next start)
    // over half a slot, the longest — as many as there are frames, and a
    // few more for frames where a slot either side was empty too (a frame
    // may close with one slot kept empty, or with a silence of several;
    // a run of missed bursts must not set the bar).
    let mut gaps: Vec<(f64, f64)> = b
        .windows(2)
        .map(|w| (w[1].t, w[1].t - (w[0].t + w[0].len)))
        .filter(|g| g.1 > 0.5 * slot)
        .collect();
    let frames = ((b[b.len() - 1].t - b[0].t) / rough).ceil() as usize;
    gaps.sort_by(|x, y| y.1.total_cmp(&x.1));
    gaps.truncate(frames + frames / 4 + 1);
    gaps.sort_by(|x, y| x.0.total_cmp(&y.0));
    let silences = gaps;
    if silences.len() < 3 {
        return None;
    }
    // The silences' spacings are whole frames. Every pair's spacing over
    // its frames estimates the period; the median of those that fit the
    // rough period (a slot out a frame at most) holds even with other long
    // gaps among the silences. Then least squares, Σ spacing / Σ frames,
    // over the pairs that fit it to within half a slot.
    let mut est = Vec::new();
    for (i, a) in silences.iter().enumerate() {
        for c in &silences[i + 1..] {
            let d = c.0 - a.0;
            let k = (d / rough).round();
            if k >= 1.0 && (d - k * rough).abs() <= (k + 2.0) * slot {
                est.push(d / k);
            }
        }
    }
    if est.len() < 3 {
        return None;
    }
    est.sort_by(f64::total_cmp);
    let median = est[est.len() / 2];
    let (mut sd, mut sk) = (0f64, 0f64);
    for (i, a) in silences.iter().enumerate() {
        for c in &silences[i + 1..] {
            let d = c.0 - a.0;
            let k = (d / median).round();
            if k >= 1.0 && (d - k * median).abs() <= 0.5 * slot {
                sd += d;
                sk += k;
            }
        }
    }
    if sk < 3.0 {
        return None;
    }
    let period = sd / sk;
    // The phase: where the silences end, folded on the period (about the
    // first, so folding does not wrap).
    let e0 = silences[0].0;
    let mut off: Vec<f64> = silences
        .iter()
        .map(|g| {
            let x = g.0 - e0;
            x - (x / period).round() * period
        })
        .collect();
    off.sort_by(f64::total_cmp);
    let phase = (e0 + off[off.len() / 2]).rem_euclid(period);
    // The silences on the frame grid, and how many frames have one.
    let on_grid: Vec<f64> = silences
        .iter()
        .filter(|g| {
            let x = (g.0 - phase).rem_euclid(period);
            x.min(period - x) < slot / 2.0
        })
        .map(|g| g.1)
        .collect();
    let frames = (b[b.len() - 1].t - b[0].t) / period;
    if (on_grid.len() as f64) < 0.5 * frames.floor() || on_grid.len() < 3 {
        return None;
    }
    let mut lens = on_grid;
    lens.sort_by(f64::total_cmp);
    Some(Frame {
        period,
        phase,
        silence: lens[lens.len() / 2],
    })
}

/// A terminal, as its bursts reveal it.
#[derive(Debug, Clone, PartialEq)]
pub struct Terminal {
    /// The value of the field inside its bursts' preamble (its points'
    /// numbers), when the layout has one: live, one value a terminal.
    pub id: Option<String>,
    pub bursts: u64,
    pub idle: u64,
    pub power_db: f32,
    pub freq_hz: f64,
    /// Where in its slot it sends, symbols after the slot's start (0
    /// without a frame to count slots from).
    pub offset: f64,
}

/// Bursts grouped into terminals, greedily: a burst joins the terminal
/// within these of its power and slot offset (and within the caller's
/// tolerance of its carrier), else starts one.
const SAME_POWER_DB: f32 = 1.5;
const SAME_OFFSET: f64 = 0.35;
const MAX_TERMINALS: usize = 96;

#[derive(Default)]
pub struct Terminals {
    list: Vec<Terminal>,
}

/// Terminals by the field in their bursts' preamble: a burst's value is
/// taken as the commonest value seen within one symbol of it (a symbol
/// misread now and then must not start a terminal of its own).
#[derive(Default)]
pub struct TerminalsById {
    list: Vec<(Vec<u8>, Terminal)>,
    adds: u64,
}

impl TerminalsById {
    pub fn add(&mut self, field: &[u8], b: &BurstTime, grid: Option<&SlotGrid>) {
        self.adds += 1;
        if self.adds.is_multiple_of(500) {
            self.consolidate();
        }
        let offset = grid.and_then(|g| g.place(b.t)).map_or(0.0, |p| p.1);
        let near = |a: &[u8]| {
            a.len() == field.len() && a.iter().zip(field).filter(|(x, y)| x != y).count() <= 1
        };
        let k = match self.list.iter().position(|(v, _)| v.as_slice() == field) {
            Some(k) => Some(k),
            None => (0..self.list.len())
                .filter(|&k| near(&self.list[k].0) && self.list[k].1.bursts >= 10)
                .max_by_key(|&k| self.list[k].1.bursts),
        };
        match k {
            Some(k) => {
                let t = &mut self.list[k].1;
                let w = 1.0 / (t.bursts as f64 + 1.0).min(50.0);
                t.power_db += (b.power_db - t.power_db) * w as f32;
                t.freq_hz += (b.freq_hz - t.freq_hz) * w;
                t.offset += (offset - t.offset) * w;
                t.bursts += 1;
                t.idle += b.idle as u64;
            }
            None => {
                if self.list.len() >= 4 * MAX_TERMINALS
                    && let Some(k) = (0..self.list.len()).min_by_key(|&k| self.list[k].1.bursts)
                {
                    self.list.swap_remove(k);
                }
                self.list.push((
                    field.to_vec(),
                    Terminal {
                        id: Some(field.iter().map(|d| char::from(b'0' + d)).collect()),
                        bursts: 1,
                        idle: b.idle as u64,
                        power_db: b.power_db,
                        freq_hz: b.freq_hz,
                        offset,
                    },
                ));
            }
        }
    }

    /// Values a symbol away from a far commoner one are that one misread
    /// (taken before it had bursts enough to be matched): folded into it.
    fn consolidate(&mut self) {
        self.list.sort_by_key(|(_, t)| std::cmp::Reverse(t.bursts));
        let mut k = 1;
        while k < self.list.len() {
            let (v, small) = (&self.list[k].0, self.list[k].1.bursts);
            let into = (0..k).find(|&j| {
                let w = &self.list[j].0;
                w.len() == v.len()
                    && w.iter().zip(v).filter(|(x, y)| x != y).count() <= 1
                    && small * 10 < self.list[j].1.bursts * 3
            });
            match into {
                Some(j) => {
                    let (_, t) = self.list.remove(k);
                    let big = &mut self.list[j].1;
                    big.bursts += t.bursts;
                    big.idle += t.idle;
                }
                None => k += 1,
            }
        }
    }

    pub fn list(&self, min: u64) -> Vec<Terminal> {
        let mut v: Vec<Terminal> = self
            .list
            .iter()
            .map(|(_, t)| t)
            .filter(|t| t.bursts >= min)
            .cloned()
            .collect();
        v.sort_by_key(|t| std::cmp::Reverse(t.bursts));
        v
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
}

impl Terminals {
    pub fn add(&mut self, b: &BurstTime, grid: &SlotGrid, freq_tol: f64) {
        let offset = grid.place(b.t).map_or(0.0, |p| p.1);
        let hit = self.list.iter_mut().find(|t| {
            (t.power_db - b.power_db).abs() <= SAME_POWER_DB
                && (t.freq_hz - b.freq_hz).abs() <= freq_tol
                && (t.offset - offset).abs() <= SAME_OFFSET
        });
        match hit {
            Some(t) => {
                // Running means, slow to move once a terminal has history.
                let w = 1.0 / (t.bursts as f64 + 1.0).min(50.0);
                t.power_db += (b.power_db - t.power_db) * w as f32;
                t.freq_hz += (b.freq_hz - t.freq_hz) * w;
                t.offset += (offset - t.offset) * w;
                t.bursts += 1;
                t.idle += b.idle as u64;
            }
            None => {
                if self.list.len() >= MAX_TERMINALS {
                    // Forget the one seen least.
                    if let Some(k) = (0..self.list.len()).min_by_key(|&k| self.list[k].bursts) {
                        self.list.swap_remove(k);
                    }
                }
                self.list.push(Terminal {
                    id: None,
                    bursts: 1,
                    idle: b.idle as u64,
                    power_db: b.power_db,
                    freq_hz: b.freq_hz,
                    offset,
                });
            }
        }
    }

    /// The terminals seen in at least `min` bursts, busiest first.
    pub fn list(&self, min: u64) -> Vec<Terminal> {
        let mut v: Vec<Terminal> = self
            .list
            .iter()
            .filter(|t| t.bursts >= min)
            .cloned()
            .collect();
        v.sort_by_key(|t| std::cmp::Reverse(t.bursts));
        v
    }

    pub fn clear(&mut self) {
        self.list.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames of `slots` slots of `slot` symbols and a silence, with a few
    /// slots empty at random, starts jittered ±0.2 symbol.
    fn frames(n_frames: usize, slots: usize, slot: f64, silence: f64) -> Vec<BurstTime> {
        let mut bursts = Vec::new();
        let mut x = 12345u64;
        let frame = slots as f64 * slot + silence;
        for f in 0..n_frames {
            for s in 0..slots {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                if (x >> 33).is_multiple_of(50) {
                    continue;
                }
                let jitter = ((x >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 0.4;
                bursts.push(BurstTime {
                    t: 100.0 + f as f64 * frame + s as f64 * slot + jitter,
                    len: slot - 13.5,
                    power_db: 0.0,
                    freq_hz: 0.0,
                    idle: false,
                });
            }
        }
        bursts
    }

    #[test]
    fn finds_the_slots_and_a_frame_of_a_slot_and_a_bit() {
        // The live plan: 200 slots of 869.5 symbols, then a 3700-symbol
        // silence (a frame of 204.26 slots).
        let g = find_grid(&frames(30, 200, 869.5, 3700.0)).unwrap();
        assert!((g.period - 869.5).abs() < 0.05, "{g:?}");
        assert!((g.guard - 13.5).abs() < 0.1, "{g:?}");
        assert!(g.spread < 0.2, "{g:?}");
        let f = g.frame.unwrap();
        assert!((f.period - (200.0 * 869.5 + 3700.0)).abs() < 2.0, "{f:?}");
        assert!((f.silence - 3700.0 - 13.5).abs() < 5.0, "{f:?}");
        assert!((f.slots(g.period) - 200.0).abs() < 0.1, "{f:?}");
        // A burst in slot 7 of frame 3, 0.1 symbol late.
        let t = 100.0 + 3.0 * f.period + 7.0 * 869.5 + 0.1;
        let (k, off) = g.place(t).unwrap();
        assert_eq!(k, 7);
        assert!((off - 0.1).abs() < 0.3, "{off}");
    }

    #[test]
    fn tells_terminals_apart() {
        let grid = find_grid(&frames(10, 100, 870.0, 2000.0)).unwrap();
        let f = grid.frame.unwrap();
        let mut t = Terminals::default();
        for s in 0..300 {
            let k = s % 3;
            let slot = (s % 90) as f64;
            t.add(
                &BurstTime {
                    t: f.phase + 2.0 * f.period + slot * grid.period + [0.0, 0.6, -0.7][k],
                    len: 856.0,
                    power_db: [0.0, -4.0, 2.5][k],
                    freq_hz: [-6050.0, -5900.0, -6100.0][k],
                    idle: false,
                },
                &grid,
                60.0,
            );
        }
        let l = t.list(10);
        assert_eq!(l.len(), 3, "{l:?}");
        assert!(l.iter().all(|x| x.bursts == 100));
    }
}
