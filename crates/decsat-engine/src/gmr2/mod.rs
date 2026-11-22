// made by CasualArclamp
//! Inmarsat GMR-2+ (IsatPhone) 50 kHz forward control carriers on a VFO:
//! the S-SCH (spot-beam colour codes, frame number) and the broadcast
//! System Information (network, location area, neighbours, timing).
//! Receive only.
//!
//! Everything here was measured on channel 88 (1541.82 MHz) and is written
//! up in the research notes (`research_notes/gmr2/NOTES.md`); ETSI TS 101
//! 377 V1.1.1 (GMR-2) supplies the CRCs, the message layouts and the frame
//! numbering, not the GMR-2+ quarter-rate carrier itself.
//!
//! * Modulation: linear OQPSK, RRC 0.35, symbol a_n j^n with a_n = 1 − 2 d_n
//!   (no 05.004 pre-coding), Rb = 1625/24 kbit/s (rails at Rb/2).
//! * The carrier is on in frames 0–25 of each 51-frame multiframe, in runs:
//!   frame 0 FTCB, then by length 637 bits (frames 1–2, S-SCH), 3741 bits
//!   (frames 1–12 or 3–14, a broadcast block), 1259 bits (frames 15–18, an
//!   m-sequence burst x^11 + x^2 + 1 on each rail) and 2078 bits (frames
//!   19–25, a high-margin sync whose code is not known yet).
//! * The decoder gates the runs on power, takes each run's carrier offset
//!   from the squared bit-spaced samples, its timing and phase in 200-bit
//!   pieces, and decodes the run by its length ([`code`]).

pub mod code;
pub mod si;

use std::collections::{BTreeSet, HashSet, VecDeque};

use decsat_core::Iq;
use decsat_dsp::fir::Fir;
use decsat_dsp::rrc::rrc_taps;
use num_complex::Complex32;

use code::{BCCH_WORDS, Codes, HYPERFRAME, SCH_WORDS, SchInfo};
use si::SiMessage;

/// Bit rate of the quarter-rate forward carrier (1625/24 kbit/s).
pub const BIT_RATE: f64 = 1_625_000.0 / 24.0;
/// Samples per bit inside the demodulator.
const SPB: usize = 4;
/// The least baseband rate: the carrier is about 45 kHz wide.
pub const LEAST_RATE: f64 = 50_000.0;
/// One TDMA frame, seconds (60/13 ms).
const FRAME_S: f64 = 0.06 / 13.0;
/// Log lines kept.
const LINES: usize = 300;
/// Margin either side of a gated run, bits.
const EDGE: usize = 8;
/// Envelope smoothing, samples.
const ENV: usize = 40;
/// Power blocks for the noise floor, samples each, and how many are kept.
const BLOCK: usize = 1024;
const BLOCKS: usize = 96;
/// Constellation points kept.
const SCATTER: usize = 1500;
/// The longest run looked at (bits); anything longer is cut.
const MAX_RUN_BITS: usize = 6000;

/// One line of what the carrier sent.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Gmr2Line {
    /// Seconds of signal since the VFO started.
    pub seconds: f64,
    pub title: String,
    pub text: String,
}

/// What the UI shows.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Gmr2View {
    /// No carrier runs seen yet.
    pub searching: bool,
    /// A block decoded in the last few seconds.
    pub locked: bool,
    /// Carrier offset from the VFO's centre, Hz.
    pub carrier_hz: f64,
    /// Eb/N0 from the soft bits (M2M4), dB.
    pub eb_n0_db: f32,
    pub runs: u64,
    pub sch_good: u64,
    pub sch_bad: u64,
    pub bcch_good: u64,
    pub bcch_bad: u64,
    /// Frames 15–18 runs that are the m-sequence.
    pub mseq_runs: u64,
    /// Frames 19–25 runs with clean Walsh words (code unknown).
    pub hms_runs: u64,
    /// The last S-SCH, its complement settled.
    pub sch: Option<SchInfo>,
    pub frame_number: Option<u32>,
    pub si3: Option<si::Si3>,
    pub si2: Option<si::Si2>,
    pub si9: Option<si::Si9>,
    /// Beam-map (type 0x06) segments seen, of 22.
    pub beam_map_segments: u32,
    /// Message types seen, in order of first sight ("SI3", "0x06", …).
    pub seen: Vec<String>,
    pub lines: Vec<Gmr2Line>,
    /// Constellation: the soft bits in rail pairs (bit 2m on I, 2m + 1 on
    /// Q) after timing and phase recovery, so OQPSK shows as four clusters.
    #[serde(skip)]
    pub scatter: Vec<Iq>,
    pub message: String,
    /// The VFO is too narrow for the carrier.
    pub too_narrow: bool,
}

pub struct Gmr2Session {
    in_rate: f64,
    fs: f64,
    codes: Codes,
    carrier: CarrierEst,
    // resampler: input samples not yet used (the first is at `pos` = 0)
    step: f64,
    pos: f64,
    hist: Vec<Iq>,
    rrc: Fir,
    filtered: Vec<Iq>,
    // demodulator buffer and its envelope, from absolute sample `base`
    y: Vec<Iq>,
    e: Vec<f32>,
    base: u64,
    env_ring: VecDeque<f32>,
    env_sum: f32,
    blocks: VecDeque<f32>,
    block_sum: f32,
    block_n: usize,
    floor: Option<f32>,
    scan: usize,
    on: bool,
    run_start: usize,
    // results
    view: Gmr2View,
    lines: VecDeque<Gmr2Line>,
    seen_blocks: HashSet<[u8; 23]>,
    seen_kinds: Vec<String>,
    segments: BTreeSet<u8>,
    prev_sch: Option<(f64, Vec<SchInfo>)>,
    /// The beam's colour codes once a pair of S-SCH blocks settled the
    /// complement.
    colour: Option<(u8, u8)>,
    last_good: Option<f64>,
    snr: Option<f32>,
    scatter: VecDeque<Iq>,
}

impl Gmr2Session {
    pub fn new(in_rate: f64) -> Self {
        let fs = BIT_RATE * SPB as f64;
        Gmr2Session {
            in_rate,
            fs,
            codes: Codes::new(),
            carrier: CarrierEst::new(),
            step: in_rate / fs,
            pos: 0.0,
            hist: Vec::new(),
            // RRC on the symbol (2 bits = 8 samples), 6 symbols a side.
            rrc: Fir::new(rrc_taps(2.0 * SPB as f64, 0.35, 6)),
            filtered: Vec::new(),
            y: Vec::new(),
            e: Vec::new(),
            base: 0,
            env_ring: VecDeque::with_capacity(ENV),
            env_sum: 0.0,
            blocks: VecDeque::with_capacity(BLOCKS),
            block_sum: 0.0,
            block_n: 0,
            floor: None,
            scan: 0,
            on: false,
            run_start: 0,
            view: Gmr2View {
                searching: true,
                ..Default::default()
            },
            lines: VecDeque::new(),
            seen_blocks: HashSet::new(),
            seen_kinds: Vec::new(),
            segments: BTreeSet::new(),
            prev_sch: None,
            colour: None,
            last_good: None,
            snr: None,
            scatter: VecDeque::new(),
        }
    }

    /// Feed the VFO's baseband.
    pub fn push(&mut self, iq: &[Iq]) {
        if self.in_rate < LEAST_RATE * 0.9 {
            return;
        }
        // Cubic interpolation onto 4 samples a bit (the VFO's rate is
        // well above the carrier's width, so no anti-alias filter needed).
        self.hist.extend_from_slice(iq);
        let mut out = Vec::with_capacity((iq.len() as f64 / self.step) as usize + 2);
        while self.pos + 3.0 < self.hist.len() as f64 {
            let i = self.pos.floor() as usize;
            let mu = (self.pos - i as f64) as f32;
            let h = &self.hist;
            out.push(decsat_dsp::timing::cubic(
                h[i],
                h[i + 1],
                h[i + 2],
                h[i + 3],
                mu,
            ));
            self.pos += self.step;
        }
        let used = (self.pos.floor() as usize).min(self.hist.len());
        self.hist.drain(..used);
        self.pos -= used as f64;
        self.filtered.clear();
        self.rrc.process(&out, &mut self.filtered);
        let filtered = std::mem::take(&mut self.filtered);
        for &s in &filtered {
            let p = s.norm_sqr();
            self.env_ring.push_back(p);
            self.env_sum += p;
            if self.env_ring.len() > ENV {
                self.env_sum -= self.env_ring.pop_front().unwrap_or(0.0);
            }
            self.y.push(s);
            self.e.push(self.env_sum / self.env_ring.len() as f32);
            self.block_sum += p;
            self.block_n += 1;
            if self.block_n == BLOCK {
                self.blocks.push_back(self.block_sum / BLOCK as f32);
                if self.blocks.len() > BLOCKS {
                    self.blocks.pop_front();
                }
                self.block_sum = 0.0;
                self.block_n = 0;
                if self.blocks.len() >= 16 {
                    let mut v: Vec<f32> = self.blocks.iter().copied().collect();
                    v.sort_by(f32::total_cmp);
                    self.floor = Some(v[v.len() * 3 / 10]);
                }
            }
        }
        self.filtered = filtered;
        self.gate();
    }

    /// Find the carrier's runs in the envelope and decode each.
    fn gate(&mut self) {
        let Some(floor) = self.floor else {
            self.trim();
            return;
        };
        let thr = 3.0 * floor;
        // The envelope is a trailing average: half its length late.
        let lag = ENV / 2;
        let tail = (EDGE + 8) * SPB + lag;
        while self.scan + tail < self.y.len() {
            let i = self.scan;
            let is_on = self.e[i] > thr;
            if !self.on && is_on {
                self.on = true;
                self.run_start = i;
            } else if self.on && (!is_on || i - self.run_start > MAX_RUN_BITS * SPB) {
                self.on = false;
                let s0 = self.run_start.saturating_sub(lag);
                let e0 = i.saturating_sub(lag);
                if (e0 - s0) / SPB >= 500 {
                    let a = s0.saturating_sub(EDGE * SPB);
                    let b = (e0 + EDGE * SPB).min(self.y.len());
                    let t = (self.base + a as u64) as f64 / self.fs;
                    let seg = self.y[a..b].to_vec();
                    self.run(&seg, t);
                }
            }
            self.scan += 1;
        }
        self.trim();
    }

    fn trim(&mut self) {
        let keep_from = if self.on { self.run_start } else { self.scan };
        let drop = keep_from.saturating_sub(64 * SPB);
        if drop > 0 {
            self.y.drain(..drop);
            self.e.drain(..drop);
            self.base += drop as u64;
            self.scan -= drop;
            self.run_start = self.run_start.saturating_sub(drop);
        }
    }

    /// Demodulate one run to soft bits and decode it by its length.
    fn run(&mut self, seg: &[Iq], t: f64) {
        self.view.searching = false;
        self.view.runs += 1;
        let f = self.carrier.update(seg, self.fs);
        self.view.carrier_hz = f;
        let Some(r) = soft_bits(seg, self.fs, f) else {
            return;
        };
        // Constellation points from the run's middle (its edges are ramp).
        let mid = r
            .get(2 * EDGE..r.len().saturating_sub(2 * EDGE))
            .unwrap_or(&[]);
        for p in mid.as_chunks::<2>().0.iter().step_by((mid.len() / 600).max(1)) {
            self.scatter.push_back(Iq::new(p[0], p[1]));
        }
        while self.scatter.len() > SCATTER {
            self.scatter.pop_front();
        }
        if let Some(s) = m2m4(&r) {
            self.snr = Some(self.snr.map_or(s, |o| 0.9 * o + 0.1 * s));
        }
        match r.len() {
            600..=700 => self.sch(&r, t),
            3690..=3800 => self.bcch(&r, t),
            1230..=1300 if mseq_check(&r) => self.view.mseq_runs += 1,
            2050..=2110 if hms_check(&r) => self.view.hms_runs += 1,
            _ => {}
        }
    }

    fn sch(&mut self, r: &[f32], t: f64) {
        let o = pilot_offset(r);
        let Some(b) = r.get(o..o + SCH_WORDS * 32).and_then(|c| self.codes.sch(c)) else {
            self.view.sch_bad += 1;
            return;
        };
        self.view.sch_good += 1;
        self.last_good = Some(t);
        let cands: Vec<SchInfo> = b.candidates.iter().map(SchInfo::from_bits).collect();
        // Settle the complement: a pair whose frame numbers are as far apart
        // as the time between them, or the beam's colour codes once known.
        let mut pick = None;
        if let Some((tp, prev)) = &self.prev_sch {
            let want = (t - tp) / FRAME_S;
            if t - tp < 60.0 {
                'outer: for c in &cands {
                    for p in prev {
                        let d = (c.frame_number() + HYPERFRAME - p.frame_number()) % HYPERFRAME;
                        if (f64::from(d) - want).abs() <= 6.0 {
                            pick = Some(*c);
                            break 'outer;
                        }
                    }
                }
            }
        }
        if let (None, Some(cb)) = (pick, self.colour) {
            pick = cands.iter().copied().find(|c| (c.ncc, c.bccc) == cb);
        }
        self.prev_sch = Some((t, cands));
        if let Some(c) = pick {
            self.colour = Some((c.ncc, c.bccc));
            let fnum = c.frame_number();
            self.view.sch = Some(c);
            self.view.frame_number = Some(fnum);
            self.line(
                t,
                "SYNC",
                format!(
                    "FN {fnum} · T1 {} T2 {} · NCC {} BCCC {}",
                    c.t1, c.t2, c.ncc, c.bccc
                ),
            );
        }
    }

    fn bcch(&mut self, r: &[f32], t: f64) {
        let o = pilot_offset(r);
        let Some(b) = r
            .get(o..o + BCCH_WORDS * 32)
            .and_then(|c| self.codes.bcch(c))
        else {
            self.view.bcch_bad += 1;
            return;
        };
        self.view.bcch_good += 1;
        self.last_good = Some(t);
        let m = si::parse(&b.octets);
        let name = si::name(&m);
        if !self.seen_kinds.contains(&name) {
            self.seen_kinds.push(name.clone());
        }
        let fresh = self.seen_blocks.insert(b.octets);
        match &m {
            SiMessage::Si3(s) => self.view.si3 = Some(s.clone()),
            SiMessage::Si2(s) => self.view.si2 = Some(s.clone()),
            SiMessage::Si9(s) => self.view.si9 = Some(*s),
            SiMessage::BeamMap { segment, .. } => {
                self.segments.insert(*segment);
            }
            SiMessage::Other { .. } => {}
        }
        if fresh {
            self.line(t, &name, si::describe(&m));
        }
    }

    fn line(&mut self, t: f64, title: &str, text: String) {
        self.lines.push_back(Gmr2Line {
            seconds: t,
            title: title.into(),
            text,
        });
        if self.lines.len() > LINES {
            self.lines.pop_front();
        }
    }

    /// The carrier offset from the VFO's centre, once runs are seen.
    pub fn carrier_offset_hz(&self) -> Option<f64> {
        (!self.view.searching).then_some(self.view.carrier_hz)
    }

    pub fn view(&self) -> Gmr2View {
        let mut v = self.view.clone();
        let now = (self.base + self.y.len() as u64) as f64 / self.fs;
        v.locked = self.last_good.is_some_and(|t| now - t < 5.0);
        v.eb_n0_db = self.snr.map_or(0.0, |s| 10.0 * s.max(1e-3).log10());
        v.beam_map_segments = self.segments.len() as u32;
        v.seen = self.seen_kinds.clone();
        v.lines = self.lines.iter().cloned().collect();
        v.scatter = self.scatter.iter().copied().collect();
        v.message = if self.in_rate < LEAST_RATE * 0.9 {
            v.too_narrow = true;
            format!(
                "VFO too narrow ({:.0} kHz of IQ): the GMR-2+ carrier is 45 kHz wide, widen it to 50 kHz or more",
                self.in_rate / 1e3
            )
        } else if v.searching {
            "looking for a GMR-2+ carrier".into()
        } else if let (Some(s3), Some(fnum)) = (&v.si3, v.frame_number) {
            format!(
                "GMR-2+ {}-{} LAC {:04X} · FN {fnum} · sync {}/{} · BCCH {}/{}",
                s3.mcc,
                s3.mnc,
                s3.lac,
                v.sch_good,
                v.sch_good + v.sch_bad,
                v.bcch_good,
                v.bcch_good + v.bcch_bad
            )
        } else {
            format!(
                "GMR-2+ carrier, {} runs · sync {}/{} · BCCH {}/{}",
                v.runs,
                v.sch_good,
                v.sch_good + v.sch_bad,
                v.bcch_good,
                v.bcch_good + v.bcch_bad
            )
        };
        v
    }
}

/// FFT length for the carrier estimate (16.5 Hz bins at 4 samples a bit).
const NFFT: usize = 16384;

/// The carrier offset from the squared signal's two lines at 2 fc ± Rb/2
/// (an OQPSK carrier squared, as the research notes' `oqpsk.py`), on a
/// power spectrum averaged over runs.
struct CarrierEst {
    fft: std::sync::Arc<dyn rustfft::Fft<f32>>,
    spec: Vec<f32>,
    runs: u32,
}

impl CarrierEst {
    fn new() -> Self {
        CarrierEst {
            fft: rustfft::FftPlanner::new().plan_fft_forward(NFFT),
            spec: vec![0.0; NFFT],
            runs: 0,
        }
    }

    /// Add a run and return the carrier offset in Hz.
    fn update(&mut self, seg: &[Iq], fs: f64) -> f64 {
        let mut s = vec![0.0f32; NFFT];
        for chunk in seg.chunks(NFFT) {
            let mut buf: Vec<Complex32> = chunk.iter().map(|x| x * x).collect();
            buf.resize(NFFT, Complex32::new(0.0, 0.0));
            self.fft.process(&mut buf);
            for (a, b) in s.iter_mut().zip(&buf) {
                *a += b.norm_sqr();
            }
        }
        let tot: f32 = s.iter().sum::<f32>().max(1e-30);
        let w = if self.runs < 8 {
            1.0 / (self.runs + 1) as f32
        } else {
            0.125
        };
        for (a, b) in self.spec.iter_mut().zip(&s) {
            *a = (1.0 - w) * *a + w * b / tot;
        }
        self.runs += 1;
        // x = 2 fc within ±(fs/2 − Rb/2); the lines at x ± Rb/2.
        let bin = fs / NFFT as f64;
        let half = (BIT_RATE / 2.0 / bin).round() as isize;
        let lim = (fs / 2.0 / bin) as isize - half - 1;
        let idx = |b: isize| self.spec[b.rem_euclid(NFFT as isize) as usize];
        let mut best = (0isize, f32::MIN);
        for x in -lim..=lim {
            let p = idx(x - half) + idx(x + half);
            if p > best.1 {
                best = (x, p);
            }
        }
        // parabolic refinement on the summed lines
        let p = |x: isize| idx(x - half) + idx(x + half);
        let (a, b, c) = (p(best.0 - 1), p(best.0), p(best.0 + 1));
        let d = if (a - 2.0 * b + c).abs() > 0.0 {
            0.5 * (a - c) / (a - 2.0 * b + c)
        } else {
            0.0
        };
        (best.0 as f64 + f64::from(d)) * bin / 2.0
    }
}

/// Soft bits of one run, its carrier offset `f` already known: timing as
/// the sample phase whose j^-n de-rotated bit-spaced samples lie most on
/// one axis, phase in 200-bit pieces by the same measure, unwrapped (mod π)
/// and interpolated — the research notes' `extract.py`.
fn soft_bits(seg: &[Iq], fs: f64, f: f64) -> Option<Vec<f32>> {
    let jn = |n: usize| match n % 4 {
        0 => Complex32::new(1.0, 0.0),
        1 => Complex32::new(0.0, -1.0),
        2 => Complex32::new(-1.0, 0.0),
        _ => Complex32::new(0.0, 1.0),
    };
    let w = -2.0 * std::f64::consts::PI * f / fs;
    let seg: Vec<Iq> = seg
        .iter()
        .enumerate()
        .map(|(i, x)| x * Complex32::from_polar(1.0, (w * i as f64) as f32))
        .collect();
    const ANGLES: usize = 48;
    let rots: Vec<Complex32> = (0..ANGLES)
        .map(|a| Complex32::from_polar(1.0, -(a as f32) * std::f32::consts::PI / ANGLES as f32))
        .collect();
    // The angle (mod π) putting a piece most on the real axis, and how well.
    let best_angle = |v: &[Complex32]| -> (f32, f32) {
        let mut best = (0usize, f32::MIN);
        for (a, r) in rots.iter().enumerate() {
            let m: f32 = v.iter().map(|x| (x * r).re.abs()).sum();
            if m > best.1 {
                best = (a, m);
            }
        }
        (best.0 as f32 * std::f32::consts::PI / ANGLES as f32, best.1)
    };
    let mut best: Option<(f32, usize)> = None;
    for k in 0..2 * SPB {
        let n = (seg.len().saturating_sub(k)) / SPB;
        if n < 100 {
            return None;
        }
        let v: Vec<Complex32> = (0..n.min(400)).map(|i| seg[k + SPB * i] * jn(i)).collect();
        let (_, m) = best_angle(&v);
        if best.is_none_or(|b| m > b.0) {
            best = Some((m, k));
        }
    }
    let (_, k) = best?;
    let n = (seg.len() - k) / SPB;
    let v: Vec<Complex32> = (0..n).map(|i| seg[k + SPB * i] * jn(i)).collect();
    let piece = 200;
    let mut cen = Vec::new();
    let mut ths: Vec<f32> = Vec::new();
    let mut p0 = 0;
    while p0 + 100 <= n {
        let (mut th, _) = best_angle(&v[p0..(p0 + piece).min(n)]);
        if let Some(&last) = ths.last() {
            let pi = std::f32::consts::PI;
            while th - last > pi / 2.0 {
                th -= pi;
            }
            while th - last < -pi / 2.0 {
                th += pi;
            }
        }
        ths.push(th);
        cen.push((p0 + piece / 2) as f32);
        p0 += piece;
    }
    if ths.is_empty() {
        return None;
    }
    let r = v
        .iter()
        .enumerate()
        .map(|(i, x)| {
            let fi = i as f32;
            let j = cen.partition_point(|&c| c <= fi);
            let th = if j == 0 {
                ths[0]
            } else if j >= cen.len() {
                ths[cen.len() - 1]
            } else {
                let a = (fi - cen[j - 1]) / (cen[j] - cen[j - 1]);
                ths[j - 1] + a * (ths[j] - ths[j - 1])
            };
            (x * Complex32::from_polar(1.0, -th)).re
        })
        .collect();
    Some(r)
}

/// Eb/N0 (linear) of BPSK-like soft bits by the M2M4 moments.
fn m2m4(r: &[f32]) -> Option<f32> {
    let n = r.len() as f32;
    let m2 = r.iter().map(|x| x * x).sum::<f32>() / n;
    let m4 = r.iter().map(|x| x.powi(4)).sum::<f32>() / n;
    // real BPSK: M2 = S + N, M4 = S² + 6SN + 3N²
    let s = ((3.0 * m2 * m2 - m4) / 2.0).max(0.0).sqrt();
    let noise = m2 - s;
    (noise > 0.0 && s > 0.0).then(|| s / (2.0 * noise))
}

/// The Walsh word grid: chip 0 of every word is the same (the scrambler is
/// 0 at chips 0/32/64/96), so the bits 32 apart that agree most.
fn pilot_offset(r: &[f32]) -> usize {
    (0..32)
        .max_by_key(|&o| {
            let s: i32 = r[o..]
                .iter()
                .step_by(32)
                .map(|&x| if x < 0.0 { -1 } else { 1 })
                .sum();
            s.abs()
        })
        .unwrap_or(0)
}

/// Frames 15–18: each rail is the m-sequence s[n] = s[n−2] ^ s[n−11].
fn mseq_check(r: &[f32]) -> bool {
    let b: Vec<u8> = r.iter().map(|&x| u8::from(x < 0.0)).collect();
    (0..2).any(|rail| {
        let s: Vec<u8> = b.iter().skip(rail).step_by(2).copied().collect();
        let lo = 20;
        let hi = s.len().saturating_sub(10);
        if hi <= lo + 100 {
            return false;
        }
        let bad = (lo..hi).filter(|&n| s[n] != s[n - 2] ^ s[n - 11]).count();
        bad * 20 < hi - lo
    })
}

/// Frames 19–25: 64 Walsh words whose scrambler repeats every 32 words.
fn hms_check(r: &[f32]) -> bool {
    (0..32).any(|o| {
        let Some(c) = r.get(o..o + 64 * 32) else {
            return false;
        };
        let mut sum = 0.0;
        for w in 0..32 {
            sum += code::walsh_pair(
                &c[32 * w..32 * w + 32],
                &c[32 * (w + 32)..32 * (w + 32) + 32],
            )
            .1;
        }
        sum / 32.0 > 0.8
    })
}
