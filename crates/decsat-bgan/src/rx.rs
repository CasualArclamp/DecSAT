// made by CasualArclamp
//! The streaming BGAN receiver: IQ in, decoded FEC-block payloads out, in real
//! time.
//!
//! ```text
//! FrontEnd (resample, RRC, Gardner)  ->  symbols
//!   Searching: buffer ~0.9 s, 4th-power line -> carrier offset, derotate,
//!              folded differential UW search -> frame offset + level
//!   Locked:    per frame, UW search in a +-320 symbol window (the offset is
//!              piecewise constant on real captures, not a linear drift),
//!              pilot-slope carrier tracking, then per block: pilot
//!              amplitude + linear phase fit, level choice, turbo decode,
//!              acceptance test
//! ```
//!
//! Ported from DecBGAN (`recv.py`, `carrier.py`, `tools/decode_wav.py`); the
//! batch estimators there became loops or fixed-size acquisitions here.
//! Level choice follows `decode_wav.py`: block 0 is the UW's level; blocks
//! 1..7 take the level from the ForwardBearerCodeRateParam AVP at byte 3 of
//! block 0's payload when it is there, else a learned trial order.

use crate::fec::{self, BlockScratch};
use crate::frame::{self, C32, DATA_PER_GROUP, GROUP, GROUPS_PER_BLOCK};
use crate::frontend::FrontEnd;
use crate::spec::{self, F80T45X8B, LEVELS};
use crate::tables;
use rayon::prelude::*;
use rustfft::FftPlanner;
use std::f64::consts::PI;
use std::sync::Arc;
use std::time::Instant;

const MOS: usize = F80T45X8B.mos;
const UWLEN: usize = F80T45X8B.uw_syms;
const RS: f64 = F80T45X8B.rs;
/// Tracking window either side of the predicted UW position, symbols
/// (DecBGAN `track_offsets`: offsets step by tens to ~150 symbols).
const WIN: i64 = 320;
/// Acquisition block lengths (symbols): FFT sizes for the 4th-power line.
const ACQ_N: usize = 1 << 17;
/// Folded-UW peak-to-sidelobe ratio needed to call a lock.
const ACQ_PSR: f64 = 7.0;
/// Frames without a decoded block (and a weak UW) before searching again.
const LOSS_FRAMES: u32 = 40;

/// One accepted FEC block.
#[derive(Debug, Clone)]
pub struct BlockOut {
    /// Block index in the frame, 0..8.
    pub block: u8,
    pub level: &'static str,
    /// Parity agreement of the re-encode check (0.70 is the acceptance line).
    pub agreement: f32,
    /// Descrambled information bytes (D/8).
    pub payload: Vec<u8>,
}

/// One 80 ms frame.
#[derive(Debug, Clone)]
pub struct FrameOut {
    /// Frames since lock.
    pub index: u64,
    /// UW level (the level of block 0).
    pub uw_level: &'static str,
    /// UW correlation against the noise floor measured half a frame away.
    pub uw_ratio: f32,
    pub esn0_db: f32,
    /// Accepted blocks, in block order.
    pub blocks: Vec<BlockOut>,
}

/// Receiver state for the UI.
#[derive(Debug, Clone, Default)]
pub struct RxStatus {
    pub locked: bool,
    pub frames: u64,
    pub blocks_ok: u64,
    pub blocks_total: u64,
    pub payload_bytes: u64,
    pub esn0_db: f32,
    pub uw_level: &'static str,
    pub uw_ratio: f32,
    pub carrier_hz: f64,
    pub clock_ppm: f64,
    /// Per-block-index decode counts.
    pub by_block: [u64; 8],
    /// Decode CPU time over frame time (1.0 = just keeping up).
    pub load: f32,
    /// Frame decoding alone, same scale.
    pub decode_load: f32,
    /// Frames dropped because the decoder fell behind.
    pub dropped_frames: u64,
    /// Set when the ETSI Annex C tables are missing.
    pub tables_missing: bool,
    /// Per level accepted-block counts, in [`spec::LEVELS`] order.
    pub by_level: [u64; 10],
}

/// Learned level order per block index (DecBGAN `decode_wav` predictor):
/// the hint, the last level seen there, then by frequency, then the rest.
#[derive(Default)]
struct Predictor {
    seen: [[u32; 10]; 8],
    last: [Option<usize>; 8],
}

impl Predictor {
    fn order(&self, b: usize, hint: Option<usize>) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::with_capacity(10);
        if let Some(h) = hint {
            out.push(h);
        }
        if let Some(l) = self.last[b]
            && !out.contains(&l)
        {
            out.push(l);
        }
        let mut by_freq: Vec<usize> = (0..10).collect();
        by_freq.sort_by_key(|&i| std::cmp::Reverse(self.seen[b][i]));
        for i in by_freq {
            if self.seen[b][i] > 0 && !out.contains(&i) {
                out.push(i);
            }
        }
        for i in 0..10 {
            if !out.contains(&i) {
                out.push(i);
            }
        }
        out
    }
    fn note(&mut self, b: usize, lv: usize) {
        self.seen[b][lv] += 1;
        self.last[b] = Some(lv);
    }
}

fn level_index(name: &str) -> Option<usize> {
    LEVELS.iter().position(|&l| l == name)
}

/// Per-block levels from a ForwardBearerCodeRateParam AVP at byte 3 of block
/// 0's payload (TS 102 744-3-1 clause 5.7.16, Figure 5.109): `10010 | n-1`
/// then n octets `mii(1) | block(3) | rate(4, signed)`. The first BCtSDU sits
/// after a 3-octet FwdBCtPDUHeader. Predicts 99.35% of levels on real
/// captures (DecBGAN `levels_from_block0`). Entries naming levels this
/// bearer lacks are left `None` so the block is searched instead.
pub fn levels_from_block0(payload: &[u8], uw_level: usize) -> Option<[Option<usize>; 8]> {
    let tag = *payload.get(3)?;
    if !(0x90..=0x97).contains(&tag) {
        return None;
    }
    let n = (tag & 7) as usize + 1;
    if payload.len() < 4 + n {
        return None;
    }
    let mut levels = [Some(uw_level); 8];
    let mut entries: Vec<(usize, Option<usize>)> = Vec::new();
    for k in 0..n {
        let v = payload[4 + k];
        let blk = ((v >> 4) & 7) as usize;
        let mut cr = (v & 0x0F) as i32;
        if cr >= 8 {
            cr -= 16;
        }
        // Table 5.20 spans L8..H6; unknown or unsupported -> None
        let lv = match cr {
            -8..=-4 => None,
            c => spec::code_rate_level(c).and_then(level_index),
        };
        entries.push((blk, lv));
    }
    entries.sort_by_key(|e| e.0);
    // "for the rest of the frame or until another change is signalled"
    for (blk, lv) in entries {
        for l in levels.iter_mut().skip(blk) {
            *l = lv;
        }
    }
    Some(levels)
}

/// One decode pool for every receiver in the process: two threads fewer than
/// the machine has (the rest is for the UI and the front ends), at least two.
pub fn shared_pool() -> Arc<rayon::ThreadPool> {
    static P: std::sync::OnceLock<Arc<rayon::ThreadPool>> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let n = std::thread::available_parallelism().map_or(4, |n| n.get());
        Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(n.saturating_sub(2).max(2))
                .thread_name(|i| format!("bgan-fec-{i}"))
                .build()
                .expect("thread pool"),
        )
    })
    .clone()
}

/// Differential UW templates: +-1 per transition, 39 long.
struct Templates {
    w: Vec<[f32; UWLEN - 1]>,
}

impl Templates {
    fn new() -> Self {
        let w = LEVELS
            .iter()
            .map(|l| {
                let b = frame::uw_bits(l);
                let mut t = [0.0f32; UWLEN - 1];
                for k in 0..UWLEN - 1 {
                    t[k] = if b[k + 1] ^ b[k] == 1 { -1.0 } else { 1.0 };
                }
                t
            })
            .collect();
        Templates { w }
    }

    /// |sum_k d[pos+k] * w[k]| for products `d[n] = s[n+1] conj(s[n])`
    /// starting at the UW's first transition (`pos` = UW start in `s`).
    #[inline]
    fn corr_d(&self, level: usize, d: &[C32], pos: usize) -> f32 {
        let w = &self.w[level];
        let mut re = 0.0f32;
        let mut im = 0.0f32;
        for k in 0..UWLEN - 1 {
            re += d[pos + k].re * w[k];
            im += d[pos + k].im * w[k];
        }
        (re * re + im * im).sqrt()
    }

    /// Direct form on symbols (used for the noise-floor probe).
    fn corr(&self, level: usize, s: &[C32], pos: usize) -> f32 {
        let d: Vec<C32> = (0..UWLEN - 1)
            .map(|k| s[pos + k + 1] * s[pos + k].conj())
            .collect();
        self.corr_d(level, &d, 0)
    }

    /// Best (metric, position, level) over UW starts in `lo..hi` of `s`.
    fn best(&self, s: &[C32], lo: usize, hi: usize) -> Option<(f32, usize, usize)> {
        let hi = hi.min(s.len().saturating_sub(UWLEN));
        if lo >= hi {
            return None;
        }
        let d = diffs(&s[lo..hi + UWLEN]);
        let mut best: Option<(f32, usize, usize)> = None;
        for pos in 0..hi - lo {
            for l in 0..self.w.len() {
                let m = self.corr_d(l, &d, pos);
                if best.is_none_or(|b| m > b.0) {
                    best = Some((m, lo + pos, l));
                }
            }
        }
        best
    }
}

/// `d[n] = s[n+1] * conj(s[n])`: insensitive to the carrier phase and to
/// small residual offsets, which is why the UW search works before carrier
/// recovery is exact.
fn diffs(s: &[C32]) -> Vec<C32> {
    s.windows(2).map(|w| w[1] * w[0].conj()).collect()
}

enum State {
    Searching,
    Locked {
        /// Absolute symbol index of the next frame's UW start.
        next: u64,
        frame_no: u64,
        bad: u32,
    },
}

/// The receiver. Feed baseband with [`BganRx::push`]; collect results with
/// [`BganRx::take_frames`].
pub struct BganRx {
    fe: FrontEnd,
    tpl: Templates,
    state: State,
    /// Recovered symbols; `buf[0]` is absolute symbol `base`.
    buf: Vec<C32>,
    base: u64,
    sym_scratch: Vec<C32>,
    /// Residual carrier (Hz) applied in the symbol domain on top of the
    /// front end's NCO, adapted from the pilots each frame.
    resid_hz: f64,
    predictor: Predictor,
    status: RxStatus,
    frames: Vec<FrameOut>,
    constellation: Vec<C32>,
    pool: Option<Arc<rayon::ThreadPool>>,
    scratch0: BlockScratch,
    /// Decode time spent / frame time elapsed (smoothed).
    load: f32,
    /// Frames' worth of wall-clock debt: positive when decoding lags.
    debt_s: f64,
    tables_ok: Option<bool>,
    /// Drop the first symbols of the stream (filter/AGC transient).
    warm: bool,
}

impl BganRx {
    /// `fs_in`: sample rate of the baseband handed to `push` (the VFO's DDC
    /// output); it should be at least ~215 kHz (the signal is 189 kHz wide).
    pub fn new(fs_in: f64) -> Self {
        BganRx {
            fe: FrontEnd::new(fs_in),
            tpl: Templates::new(),
            state: State::Searching,
            buf: Vec::new(),
            base: 0,
            sym_scratch: Vec::new(),
            resid_hz: 0.0,
            predictor: Predictor::default(),
            status: RxStatus::default(),
            frames: Vec::new(),
            constellation: Vec::new(),
            pool: Some(shared_pool()),
            scratch0: BlockScratch::new(),
            load: 0.0,
            debt_s: 0.0,
            tables_ok: None,
            warm: true,
        }
    }

    /// Run block decodes on this pool (the app's shared FEC pool) instead of
    /// rayon's global one.
    pub fn set_pool(&mut self, pool: Option<Arc<rayon::ThreadPool>>) {
        self.pool = pool;
    }

    pub fn status(&self) -> &RxStatus {
        &self.status
    }

    pub fn status_mut(&mut self) -> &mut RxStatus {
        &mut self.status
    }

    /// Offset of the carrier from the VFO centre, Hz (what the front end and
    /// the symbol-domain loop have removed).
    pub fn carrier_offset_hz(&self) -> f64 {
        self.fe.carrier_hz() + self.resid_hz
    }

    /// The VFO was retuned by `hz` (the DDC moved): the offset we have
    /// removed shrinks by as much.
    pub fn retune_hz(&mut self, hz: f64) {
        self.fe.set_carrier_hz(self.fe.carrier_hz() - hz);
    }

    /// A cloud of recent phase-corrected data symbols for a constellation plot.
    pub fn constellation(&self) -> &[C32] {
        &self.constellation
    }

    pub fn take_frames(&mut self) -> Vec<FrameOut> {
        std::mem::take(&mut self.frames)
    }

    /// Drop lock and search again (the carrier moved, or the user asked).
    pub fn reset(&mut self) {
        self.state = State::Searching;
        self.buf.clear();
        self.status.locked = false;
    }

    pub fn push(&mut self, iq: &[C32]) {
        self.sym_scratch.clear();
        self.fe.push(iq, &mut self.sym_scratch);
        self.buf.extend_from_slice(&self.sym_scratch);
        self.status.clock_ppm = self.fe.clock_ppm;
        if self.tables_ok.is_none() {
            self.tables_ok = Some(tables::tables("L3").is_ok());
            self.status.tables_missing = !self.tables_ok.unwrap_or(false);
        }
        loop {
            let progressed = match self.state {
                State::Searching => self.try_acquire(),
                State::Locked { .. } => self.try_frame(),
            };
            if !progressed {
                break;
            }
        }
        self.status.carrier_hz = self.carrier_offset_hz();
    }

    // --- acquisition -----------------------------------------------------

    /// Coarse carrier offset from the 4th-power line (DecBGAN
    /// `blind_carrier_recovery` stage 1). Returns cycles per symbol.
    fn fourth_power_cfo(s: &[C32]) -> f64 {
        let n = s.len(); // a power of two (ACQ_N)
        let mut z: Vec<rustfft::num_complex::Complex<f32>> = s[..n]
            .iter()
            .map(|v| {
                let p = *v * *v;
                let q = p * p;
                rustfft::num_complex::Complex::new(q.re, q.im)
            })
            .collect();
        // remove DC of the 4th power? No: the line itself is what we want; the
        // mean of s^4 is the wanted component at the carrier offset only.
        FftPlanner::<f32>::new().plan_fft_forward(n).process(&mut z);
        let mag: Vec<f32> = z.iter().map(|c| c.norm_sqr()).collect();
        let (mut k, mut best) = (0usize, 0.0f32);
        for (i, &m) in mag.iter().enumerate() {
            if m > best {
                best = m;
                k = i;
            }
        }
        // parabolic interpolation on the log magnitude
        let (a, b, c) = (
            (mag[(k + n - 1) % n].max(1e-30) as f64).ln(),
            (mag[k].max(1e-30) as f64).ln(),
            (mag[(k + 1) % n].max(1e-30) as f64).ln(),
        );
        let den = a - 2.0 * b + c;
        let d = if den.abs() > 1e-12 {
            (0.5 * (a - c) / den).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        let mut kk = k as f64 + d;
        if kk > n as f64 / 2.0 {
            kk -= n as f64;
        }
        kk / n as f64 / 4.0
    }

    fn try_acquire(&mut self) -> bool {
        if self.buf.len() < ACQ_N {
            return false;
        }
        if self.warm {
            // front-end start-up transient (filters, AGC): not signal
            self.buf.drain(..256);
            self.base += 256;
            self.warm = false;
            return true;
        }
        let n = ACQ_N;
        let f = Self::fourth_power_cfo(&self.buf[..n]);
        // derotate the buffer by f (cycles/symbol)
        let mut s: Vec<C32> = self.buf[..n].to_vec();
        for (i, v) in s.iter_mut().enumerate() {
            let (sn, cs) = (-2.0 * PI * f * i as f64).sin_cos();
            *v *= C32::new(cs as f32, sn as f32);
        }
        // Fold |corr|^2 over whole frames (valid while the offset is fixed,
        // true for a few seconds), per level.
        let nfr = n / MOS;
        let d = diffs(&s);
        let tpl = &self.tpl;
        // Level-agnostic fold: per frame and position take the best of the ten
        // UWs (the level may change from frame to frame), then sum over frames.
        let per_frame: Vec<Vec<f32>> = (0..nfr)
            .into_par_iter()
            .map(|fr| {
                (0..MOS)
                    .map(|p| {
                        let pos = fr * MOS + p;
                        if pos + UWLEN >= d.len() {
                            return 0.0;
                        }
                        (0..LEVELS.len())
                            .map(|l| {
                                let c = tpl.corr_d(l, &d, pos);
                                c * c
                            })
                            .fold(0.0f32, f32::max)
                    })
                    .collect()
            })
            .collect();
        let mut folded = vec![0.0f64; MOS];
        for pf in &per_frame {
            for (f, v) in folded.iter_mut().zip(pf) {
                *f += *v as f64;
            }
        }
        let (j, &pk) = folded
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        let (mut sum, mut cnt) = (0.0, 0usize);
        for (p, v) in folded.iter().enumerate() {
            let dd = (p + MOS - j) % MOS;
            if dd > 3 && dd < MOS - 3 {
                sum += v;
                cnt += 1;
            }
        }
        let mean = (sum / cnt as f64).max(1e-30);
        // The peak must come from most frames, not one: a start-up transient
        // or a burst of interference in a single frame can dominate a folded
        // sum on its own (pure noise once scored a PSR of 42 that way). A
        // real carrier puts its UW in every frame, far above one frame's
        // noise mean (mean / nfr).
        let per_frame_mean = mean / nfr as f64;
        let frames_hit = per_frame
            .iter()
            .filter(|pf| pf[j] as f64 > 4.0 * per_frame_mean)
            .count();
        let psr = if frames_hit * 10 >= nfr * 6 {
            pk / mean
        } else {
            0.0
        };
        let pos = j;
        if psr < ACQ_PSR {
            // nothing here: slide half a buffer and look again
            let drop = n / 2;
            self.buf.drain(..drop);
            self.base += drop as u64;
            return true;
        }
        // Locked on frame offset `pos`, carrier f. Hand f to the front end
        // (so later symbols arrive derotated), derotate what we hold, and
        // start tracking at the first complete frame.
        let total_hz = self.fe.carrier_hz() + f * RS;
        self.fe.set_carrier_hz(total_hz);
        let nbuf = self.buf.len();
        for (i, v) in self.buf.iter_mut().enumerate() {
            let (sn, cs) = (-2.0 * PI * f * i as f64).sin_cos();
            *v *= C32::new(cs as f32, sn as f32);
        }
        let _ = nbuf;
        self.resid_hz = 0.0;
        self.state = State::Locked {
            next: self.base + pos as u64,
            frame_no: 0,
            bad: 0,
        };
        self.status.locked = true;
        true
    }

    // --- tracking and decoding -------------------------------------------

    fn try_frame(&mut self) -> bool {
        let State::Locked {
            next,
            frame_no,
            bad,
        } = self.state
        else {
            return false;
        };
        // symbols needed: window before, the frame, window after
        let rel = next as i64 - self.base as i64;
        let need = rel + WIN + MOS as i64 + UWLEN as i64 + 8;
        if rel < 0 {
            // behind the buffer start (we were starved): skip a frame
            self.state = State::Locked {
                next: next + MOS as u64,
                frame_no,
                bad,
            };
            return true;
        }
        if (self.buf.len() as i64) < need {
            return false;
        }
        let t0 = Instant::now();
        let lo = (rel - WIN).max(0) as usize;
        let hi = (rel + WIN) as usize;
        let Some((metric, pos, level)) = self.tpl.best(&self.buf, lo, hi + 1) else {
            return false;
        };
        // noise floor half a frame away (DecBGAN `floor_corr`)
        let floor = {
            let mut sum = 0.0f32;
            let mut cnt = 0;
            let start = pos + MOS / 2;
            for k in (0..MOS / 8).step_by(31) {
                if start + k + UWLEN < self.buf.len() {
                    sum += self.tpl.corr(level, &self.buf, start + k);
                    cnt += 1;
                }
            }
            if cnt > 0 { sum / cnt as f32 } else { f32::NAN }
        };
        let ratio = if floor.is_finite() && floor > 0.0 {
            metric / floor
        } else {
            0.0
        };

        let abs_pos = self.base + pos as u64;
        let mut fr: Vec<C32> = self.buf[pos..pos + MOS].to_vec();
        // residual carrier
        let w = -2.0 * PI * self.resid_hz / RS;
        if self.resid_hz != 0.0 {
            for (i, v) in fr.iter_mut().enumerate() {
                let (sn, cs) = (w * i as f64).sin_cos();
                *v *= C32::new(cs as f32, sn as f32);
            }
        }
        self.track_carrier(&fr);

        let out = self.decode_frame(&fr, level, ratio, frame_no);
        let ok = out.blocks.len();
        let dt = t0.elapsed().as_secs_f64();
        // real-time governor bookkeeping: debt grows by decode time, shrinks
        // by the 80 ms of signal each frame represents
        self.debt_s = (self.debt_s + dt - 0.080).max(0.0);
        self.load += 0.1 * ((dt / 0.080) as f32 - self.load);
        self.status.decode_load = self.load;

        let good = ok > 0 || ratio > 3.0;
        let bad = if good { 0 } else { bad + 1 };
        self.frames.push(out);
        // advance
        let next = abs_pos + MOS as u64;
        // trim the buffer: keep window margin before the next frame
        let keep_from = (next as i64 - self.base as i64 - WIN - 8).max(0) as usize;
        if keep_from > 0 {
            self.buf.drain(..keep_from.min(self.buf.len()));
            self.base += keep_from as u64;
        }
        if bad >= LOSS_FRAMES {
            self.state = State::Searching;
            self.status.locked = false;
            self.buf.clear();
        } else {
            self.state = State::Locked {
                next,
                frame_no: frame_no + 1,
                bad,
            };
        }
        true
    }

    /// Pilot-slope frequency error (successive pilots 137 symbols apart; the
    /// unwrap limit is 1/(2*906 us) = 552 Hz, DecBGAN carrier.py).
    fn track_carrier(&mut self, fr: &[C32]) {
        let re = frame::pilot_symbol();
        let mut acc = C32::new(0.0, 0.0);
        let mut prev: Option<C32> = None;
        for g in 0..F80T45X8B.pilot_syms {
            let z = fr[UWLEN + g * GROUP + DATA_PER_GROUP] * re.conj();
            if let Some(p) = prev {
                acc += z * p.conj();
            }
            prev = Some(z);
        }
        if acc.norm() > 0.0 {
            let err_hz = (acc.im as f64).atan2(acc.re as f64) / (2.0 * PI * GROUP as f64) * RS;
            if err_hz.abs() < 500.0 {
                self.resid_hz += 0.3 * err_hz;
                // fold a large residual into the front end's NCO
                if self.resid_hz.abs() > 150.0 {
                    self.fe.set_carrier_hz(self.fe.carrier_hz() + self.resid_hz);
                    self.resid_hz = 0.0;
                }
            }
        }
    }

    /// Per-block amplitude and linear-phase correction from the block's 11
    /// pilots (DecBGAN `prepare`). Returns the block's 1496 symbols at unit
    /// mean power and canonical rotation.
    fn prepare(fr: &[C32], b: usize) -> Vec<C32> {
        let g0 = b * GROUPS_PER_BLOCK;
        let pil_ref = frame::pilot_symbol();
        let mut pl = [C32::new(0.0, 0.0); GROUPS_PER_BLOCK];
        let mut mag = 0.0f32;
        for (gi, p) in pl.iter_mut().enumerate() {
            *p = fr[UWLEN + (g0 + gi) * GROUP + DATA_PER_GROUP];
            mag += p.norm();
        }
        mag /= GROUPS_PER_BLOCK as f32;
        // pilots (1111) sit at the outer-corner magnitude 1.5*sqrt(2)/sqrt(2.5)
        let sc = 1.341_640_8 / (mag + 1e-9);
        // unwrapped phase of pilot * conj(reference) against position
        let mut ph = [0.0f64; GROUPS_PER_BLOCK];
        let mut prev = 0.0f64;
        for gi in 0..GROUPS_PER_BLOCK {
            let z = pl[gi] * pil_ref.conj();
            let mut a = (z.im as f64).atan2(z.re as f64);
            if gi > 0 {
                while a - prev > PI {
                    a -= 2.0 * PI;
                }
                while a - prev < -PI {
                    a += 2.0 * PI;
                }
            }
            ph[gi] = a;
            prev = a;
        }
        // least-squares line through (gi*137 + 136, ph)
        let n = GROUPS_PER_BLOCK as f64;
        let xs: Vec<f64> = (0..GROUPS_PER_BLOCK)
            .map(|g| (g * GROUP + DATA_PER_GROUP) as f64)
            .collect();
        let (sx, sy) = (xs.iter().sum::<f64>(), ph.iter().sum::<f64>());
        let sxx: f64 = xs.iter().map(|x| x * x).sum();
        let sxy: f64 = xs.iter().zip(&ph).map(|(x, y)| x * y).sum();
        let c1 = (n * sxy - sx * sy) / (n * sxx - sx * sx);
        let c0 = (sy - c1 * sx) / n;

        let mut out = Vec::with_capacity(F80T45X8B.teo);
        for gi in 0..GROUPS_PER_BLOCK {
            let base = UWLEN + (g0 + gi) * GROUP;
            for k in 0..DATA_PER_GROUP {
                let x = (gi * GROUP + k) as f64;
                let (s, c) = (-(c0 + c1 * x)).sin_cos();
                out.push(fr[base + k] * sc * C32::new(c as f32, s as f32));
            }
        }
        out
    }

    fn decode_frame(&mut self, fr: &[C32], uw_level: usize, uw_ratio: f32, index: u64) -> FrameOut {
        let blocks_sym: Vec<Vec<C32>> = (0..F80T45X8B.nblocks)
            .map(|b| Self::prepare(fr, b))
            .collect();

        // Es/N0 from the data symbols' kurtosis (M4/M2^2 inversion, DecBGAN
        // `esn0_from_m4`, calibrated +-0.25 dB over 8..20 dB).
        let (mut m2, mut m4, mut cnt) = (0.0f64, 0.0f64, 0usize);
        for blk in &blocks_sym {
            for v in blk {
                let p = v.norm_sqr() as f64;
                m2 += p;
                m4 += p * p;
                cnt += 1;
            }
        }
        let esn0 = esn0_from_m4((m4 / cnt as f64) / (m2 / cnt as f64).powi(2));
        // Rolling cloud of the last ~4000 data symbols (every 5th of all
        // eight blocks), so the 16 clusters fill in at a glance.
        for blk in &blocks_sym {
            self.constellation.extend(blk.iter().step_by(5).copied());
        }
        let n = self.constellation.len();
        if n > 4096 {
            self.constellation.drain(..n - 4096);
        }

        let mut out = FrameOut {
            index,
            uw_level: LEVELS[uw_level],
            uw_ratio,
            esn0_db: esn0 as f32,
            blocks: Vec::new(),
        };
        self.status.frames += 1;
        self.status.esn0_db = esn0 as f32;
        self.status.uw_level = LEVELS[uw_level];
        self.status.uw_ratio = uw_ratio;
        self.status.blocks_total += 8;

        if self.tables_ok != Some(true) {
            return out;
        }
        // Behind real time by more than ~1 s: shed work rather than queue it.
        let behind = self.debt_s > 1.0;
        if behind {
            self.status.dropped_frames += 1;
            self.status.blocks_total -= 8;
            return out;
        }
        let tight = self.debt_s > 0.3;

        // Block 0: the UW's level.
        let mut hints: Option<[Option<usize>; 8]> = None;
        let mut block0_level = uw_level;
        let first = try_levels(&mut self.scratch0, &blocks_sym[0], &[uw_level], tight);
        let first = match first {
            Some(b) => Some(b),
            // UW mis-picked? try the rest, in learned order
            None if !tight => {
                let order: Vec<usize> = self
                    .predictor
                    .order(0, None)
                    .into_iter()
                    .filter(|&l| l != uw_level)
                    .collect();
                try_levels(&mut self.scratch0, &blocks_sym[0], &order, tight)
            }
            None => None,
        };
        if let Some(b) = first {
            self.predictor.note(0, b.0);
            block0_level = b.0;
            hints = levels_from_block0(&fec::payload_bytes(&b.1.bits), b.0);
            self.record(&mut out, 0, b);
        }

        // Blocks 1..7 in parallel.
        let orders: Vec<Vec<usize>> = (1..8)
            .map(|b| {
                // Clause 5.7.16: with no AVP every block uses the UW's level.
                let hint = Some(hints.and_then(|h| h[b]).unwrap_or(block0_level));
                let mut o = self.predictor.order(b, hint);
                if tight {
                    o.truncate(2);
                }
                o
            })
            .collect();
        let work = || {
            blocks_sym[1..]
                .par_iter()
                .zip(orders.par_iter())
                .map_init(BlockScratch::new, |sc, (sym, order)| {
                    try_levels(sc, sym, order, tight)
                })
                .collect::<Vec<_>>()
        };
        let results = match &self.pool {
            Some(p) => p.install(work),
            None => work(),
        };
        for (i, r) in results.into_iter().enumerate() {
            if let Some(b) = r {
                self.predictor.note(i + 1, b.0);
                self.record(&mut out, i + 1, b);
            }
        }
        out.blocks.sort_by_key(|b| b.block);
        out
    }

    fn record(&mut self, out: &mut FrameOut, block: usize, b: (usize, fec::BlockDecode)) {
        let payload = fec::payload_bytes(&b.1.bits);
        self.status.blocks_ok += 1;
        self.status.by_block[block] += 1;
        self.status.by_level[b.0] += 1;
        self.status.payload_bytes += payload.len() as u64;
        out.blocks.push(BlockOut {
            block: block as u8,
            level: LEVELS[b.0],
            agreement: b.1.agreement,
            payload,
        });
    }
}

/// Trial-decode `sym` at each level in `order` until one is accepted. The
/// first attempt at a level uses a short run of iterations (a wrong level
/// never converges); a near miss is rerun at full length. Early stopping is
/// safe: over 22400 labelled tries no block passed at two levels (DecBGAN).
fn try_levels(
    sc: &mut BlockScratch,
    sym: &[C32],
    order: &[usize],
    tight: bool,
) -> Option<(usize, fec::BlockDecode)> {
    for (rank, &lv) in order.iter().enumerate() {
        let Ok(t) = tables::tables(LEVELS[lv]) else {
            continue;
        };
        if rank == 0 {
            // the expected level: decode it properly straight away
            let d = fec::decode_block(sc, sym, &t, 8);
            if d.accepted() {
                return Some((lv, d));
            }
            continue;
        }
        let first = fec::decode_block(sc, sym, &t, if tight { 3 } else { 4 });
        if first.accepted() {
            let full = fec::decode_block(sc, sym, &t, 8);
            return Some((lv, if full.accepted() { full } else { first }));
        }
        if first.agreement > 0.58 && !tight {
            let full = fec::decode_block(sc, sym, &t, 8);
            if full.accepted() {
                return Some((lv, full));
            }
        }
    }
    None
}

/// Invert (1.32 rho^2 + 4 rho + 2)/(rho+1)^2 = r (DecBGAN `esn0_from_m4`).
pub fn esn0_from_m4(r: f64) -> f64 {
    const CAP: f64 = 30.0;
    const FLOOR: f64 = -10.0;
    if !r.is_finite() || r <= 1.3205 {
        return CAP;
    }
    if r >= 1.9995 {
        return FLOOR;
    }
    let (a, b, c) = (1.32 - r, 4.0 - 2.0 * r, 2.0 - r);
    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 || a.abs() < 1e-12 {
        return FLOOR;
    }
    let rho = (-b - disc.sqrt()) / (2.0 * a);
    if !rho.is_finite() || rho <= 0.0 {
        return FLOOR;
    }
    (10.0 * rho.log10()).clamp(FLOOR, CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avp_levels() {
        // payload: 3-octet header, then tag 10010|n-1 with n=2, then entries
        // block 3 -> H1 (rate 1), block 5 -> L2 (rate -2 = 0b1110)
        let p = [0xc9, 0, 0, 0x91, (3 << 4) | 1, (5 << 4) | 0x0E];
        let l = levels_from_block0(&p, level_index("L3").unwrap()).unwrap();
        let name = |i: Option<usize>| i.map(|i| LEVELS[i]);
        assert_eq!(name(l[0]), Some("L3"));
        assert_eq!(name(l[2]), Some("L3"));
        assert_eq!(name(l[3]), Some("H1"));
        assert_eq!(name(l[4]), Some("H1"));
        assert_eq!(name(l[5]), Some("L2"));
        assert_eq!(name(l[7]), Some("L2"));
        // a BulletinBoard header byte is not a code-rate tag
        assert!(levels_from_block0(&[0xc9, 0, 0, 0x10], 0).is_none());
    }

    #[test]
    fn esn0_inversion() {
        // r for rho = 12 dB: (1.32 rho^2 + 4 rho + 2)/(rho+1)^2
        let rho = 10f64.powf(1.2);
        let r = (1.32 * rho * rho + 4.0 * rho + 2.0) / ((rho + 1.0) * (rho + 1.0));
        assert!((esn0_from_m4(r) - 12.0).abs() < 0.01);
    }
}
