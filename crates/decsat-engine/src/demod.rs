// made by CasualArclamp
//! Streaming DVB-S2 physical-layer demodulator: VFO baseband in, PLFRAMEs out.
//!
//! matched filter → AGC → Gardner timing → PLHEADER correlation → frame lock →
//! PLS decode → payload descrambling.
//!
//! **Frame lock.** A PLFRAME is only emitted once the *next* PLHEADER has been
//! found exactly where this frame's own PLS code said the frame would end. That
//! one check validates both the PLS decode and the frame length, which is what
//! makes ACM safe to follow: a mis-decoded MODCOD predicts the wrong length and
//! the next header is not there. A miss drops back to searching, which is cheap
//! because the correlation for every symbol is already computed — re-acquisition
//! happens on the very next good header.
//!
//! **Carrier recovery** needs none of the above (the correlator and the PLS
//! code are read differentially), so it runs over each frame as the frame is
//! emitted, in stream order: data-aided over the PLHEADER and the pilot blocks
//! (known symbols, so the absolute phase is resolved with no 90° ambiguity)
//! and decision-directed over the data against the frame's own MODCOD
//! constellation, so an ACM carrier stays locked as QPSK, 8PSK and APSK frames
//! alternate. At (re)acquisition the loop is seeded from the two headers that
//! confirmed the first frame: with their modulation removed they are a pure
//! tone at the carrier offset.

use std::collections::{BTreeMap, VecDeque};
use std::f64::consts::{PI, TAU};

use decsat_core::{Iq, Modulation};
use decsat_dsp::{Agc, CarrierPll, Fir, LOCK_COHERENCE, SymbolSync, cis, rrc_taps};
use decsat_fec::Constellation;
use decsat_frame::pi2bpsk::map_bpsk;
use decsat_frame::vlsnr::{self, Slot, VLSNR_HEADER_LEN};

use crate::superframe::{SfStripper, SfView};
use decsat_frame::{
    PILOT_BLK_LEN, PLHEADER_LEN, PlHeaderCorrelator, PlScrambler, PlsInfo, PlscDecoder, PlscDemap,
    PlscEncoder, SLOT_LEN, SLOTS_PER_PILOT_BLK, SOF_BIG_ENDIAN, SOF_LEN,
};

/// Correlation needed to start tracking a header from scratch.
const ACQUIRE_THRESHOLD: f32 = 0.5;
/// Correlation that confirms a header at its predicted position; lower than
/// the acquisition threshold because the position is already known.
const CONFIRM_THRESHOLD: f32 = 0.3;
/// How far a confirming header may be from prediction (timing slips).
const TOLERANCE: usize = 2;
/// Coherent header match (|Σ y·ref*| / Σ|y| over the 90 symbols, carrier
/// loop running) that confirms a header: ~0.8 for a real one even at 2 dB
/// Es/N0, ~0.1 where there is none.
const COHERENT_CONFIRM: f32 = 0.4;
/// Symbols either side a candidate must beat to count as a peak.
const PEAK_HALF_WIDTH: usize = 3;
/// Samples processed at a time (see `Demod::process`).
const CHUNK: usize = 16_384;
/// Carrier loop noise bandwidth, normalised to the symbol rate.
const CARRIER_BN: f64 = 0.01;
/// Timing loop bandwidth while searching (pull-in) and once frames are
/// locked. A Gardner loop left wide slips whole symbols near threshold —
/// and one slip misaligns the rest of a frame's descrambling, losing it.
const TIMING_BN_SEARCH: f64 = 0.005;
const TIMING_BN_LOCKED: f64 = 0.001;
/// The carrier loop steers on soft decisions (see `CarrierPll::step_soft`)
/// while the squared distance between the closest points of the frame's
/// constellation, over the noise power per symbol, is below this: where a
/// symbol lands in a neighbour's decision region once in a few thousand or
/// more (`d²/2N0` under about 15; 30 here leaves room), and the hard decision
/// and the soft one part company. Above it they agree, and the soft one costs
/// a weight for each point a symbol.
const SOFT_BELOW: f32 = 30.0;
/// Carrier-locked data symbols kept for display.
const RECENT: usize = 3000;
/// A pilot block's correction, as a fraction of the constellation's finest
/// rotational step (2π over its densest ring), beyond which the data before
/// it are taken to have slipped and are re-derotated by interpolation.
const SLIP_FRACTION: f64 = 0.25;
/// Pilots and dummy-frame payload: `(1 + j)/sqrt(2)` before scrambling
/// (§5.5.3).
const PILOT: Iq = Iq::new(
    std::f32::consts::FRAC_1_SQRT_2,
    std::f32::consts::FRAC_1_SQRT_2,
);
/// Data symbols before each pilot block, and the block-to-block period.
pub(crate) const PILOT_AFTER: usize = SLOTS_PER_PILOT_BLK * SLOT_LEN;
pub(crate) const PILOT_PERIOD: usize = PILOT_AFTER + PILOT_BLK_LEN;
/// The preferred PL scrambling sequences of EN 302 307-2 Table 19e: gold
/// sequence indexes 0 and k·10 949, k = 1..6. The demodulator tries them
/// when the one it was given does not fit the pilots.
pub const PREFERRED_GOLD_CODES: [u32; 7] = [
    0,
    10_949,
    2 * 10_949,
    3 * 10_949,
    4 * 10_949,
    5 * 10_949,
    6 * 10_949,
];
/// Pilot coherence (see `pilot_coherence`) that says a scrambling sequence
/// fits: ~1 when it does, ~0.05 when not.
const GOLD_FITS: f32 = 0.5;

/// The spread of a PLHEADER's frequency estimate, cycles per symbol, at an
/// Es/N0 of 1 (0 dB), as `K / sqrt(Es/N0)`: the Cramér–Rao bound for a tone in
/// noise over the header's 90 symbols, `sqrt(6 / (4π²·N(N²−1)))`, with a
/// little over for the estimators here, which fall short of it (checked
/// against noise in `header_frequency_estimates_scatter_as_predicted`).
const HDR_FREQ_SIGMA: f64 = 6.0e-4;
/// How many sigmas of the headers' own estimate an outside frequency hint may
/// be from it and still be used: a stale hint is caught, a good one kept at
/// low SNR (where the headers' estimate is the noisier of the two).
const SEED_SIGMAS: f64 = 4.0;
/// Headers' frequency errors kept to judge the carrier loop's frequency by:
/// none are judged before `MIN`, the oldest let go past `MAX`.
const FREQ_WATCH_MIN: usize = 4;
const FREQ_WATCH_MAX: usize = 8;
/// How many sigmas of the mean of those errors the loop's frequency may be
/// off before it is put right, and the least it is ever allowed to be off
/// (cycles per symbol) before it is: with noise this small a decision-directed
/// loop is as good as the headers, and they only need to catch the loop
/// being a long way out.
const FREQ_WATCH_SIGMAS: f64 = 3.5;
const FREQ_WATCH_FLOOR: f64 = 5.0e-5;

/// One demodulated PLFRAME.
#[derive(Debug, Clone)]
pub struct PlFrame {
    pub pls: PlsInfo,
    /// Payload after the PLHEADER, pilots included, descrambled and
    /// carrier-corrected.
    pub payload: Vec<Iq>,
    /// Header correlation, 0..1.
    pub corr: f32,
    /// Signal amplitude in `payload`: the constellations' unit-power points
    /// sit at this radius scale. Measured on the known header and pilot
    /// symbols, smoothed over frames.
    pub gain: f32,
    /// Noise power per complex symbol in `payload`, measured the same way.
    pub noise_var: f32,
    /// The first frame after (re)acquiring lock: whatever came before it was
    /// lost, so a stream reassembled across frames must start over.
    pub after_gap: bool,
    /// A VL-SNR frame: its header index (Table 18b) and how well the header
    /// matched (1 when clean). Its payload is then as sent before
    /// scrambling throughout — header, pilots, and pi/2-BPSK data too.
    pub vlsnr: Option<(u8, f32)>,
    /// The end of the receiver's block (its source clock, s) the frame came
    /// out of the demodulator in — no earlier than the frame's own end — as
    /// the VFO sets it; `None` from the demodulator itself.
    pub src_s: Option<f64>,
}

impl PlFrame {
    /// Es/N0 from the known symbols, dB.
    pub fn es_n0_db(&self) -> f32 {
        10.0 * (self.gain * self.gain / self.noise_var.max(1e-12)).log10()
    }
}

/// Running sums over received symbols whose sent values are known.
#[derive(Default)]
struct Known {
    corr: Iq,
    power: f32,
    n: usize,
}

impl Known {
    fn add(&mut self, y: Iq, sent: Iq) {
        self.corr += y * sent.conj();
        self.power += y.norm_sqr();
        self.n += 1;
    }

    /// (amplitude, noise variance): with unit-magnitude references,
    /// Σ|y − a·ref·e^{jθ}|² = Σ|y|² − a²·n at the best a and θ.
    fn estimate(&self) -> Option<(f32, f32)> {
        (self.n > 0).then(|| {
            let a = self.corr.norm() / self.n as f32;
            let var = (self.power / self.n as f32 - a * a).max(a * a * 1e-6);
            (a, var)
        })
    }
}

/// Lock state, for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    /// Looking for a PLHEADER.
    Searching,
    /// Found one; waiting to confirm the next on the predicted grid.
    Found,
    /// Consecutive headers confirmed on the grid.
    Locked,
}

enum State {
    Searching {
        scan_from: usize,
    },
    Tracking {
        /// Index (in `sym`) of the current header's last symbol.
        hdr_end: usize,
        pls: PlsInfo,
        corr: f32,
        confirmed: u32,
    },
}

/// Streaming PL demodulator.
pub struct Demod {
    rate: f64,
    mf: Fir,
    agc: Agc,
    sync: SymbolSync,
    corr: PlHeaderCorrelator,
    dec: PlscDecoder,
    scrambler: PlScrambler,
    /// Recovered symbols and the header correlation ending at each.
    sym: Vec<Iq>,
    metric: Vec<f32>,
    state: State,
    // Scratch.
    filtered: Vec<Iq>,
    new_sym: Vec<Iq>,
    // Counters.
    frames: u64,
    lost: u64,
    // Carrier recovery.
    plsc_enc: PlscEncoder,
    pll: CarrierPll,
    /// The loop has been seeded since the last acquisition.
    pll_live: bool,
    /// Frequency hint, cycles per symbol: from outside, then the last value
    /// the loop held while locked.
    seed: Option<f64>,
    /// Constellation per MODCOD and frame size (the PLS code less its pilot
    /// bit).
    csts: BTreeMap<u8, Constellation>,
    hdr_ref: [Iq; PLHEADER_LEN],
    /// The SOF as sent (the same in every header).
    sof_ref: [Iq; SOF_LEN],
    data: Vec<Iq>,
    recent: VecDeque<Iq>,
    mer: f32,
    coherence: f32,
    fresh: bool,
    modulation: Option<Modulation>,
    /// The scrambling sequence has been seen to fit a frame's pilots.
    gold_verified: bool,
    /// The frame's payload before carrier recovery (slip repair needs it).
    raw: Vec<Iq>,
    /// Stretches of data re-derotated after a slip.
    slips: u64,
    /// The carrier loop's frequency error at recent headers (cycles per
    /// symbol, the headers' known symbols against the loop), since the loop
    /// was last seeded or corrected, and the corrections made so far.
    freq_watch: Vec<f64>,
    freq_fixes: u64,
    /// Smoothed signal amplitude and noise variance (see `PlFrame`).
    amp: f32,
    noise: f32,
    level_fresh: bool,
    /// S2X super-frames (Annex E): found, and their own symbols taken out
    /// between timing and framing.
    superframes: SfStripper,
    stripped: Vec<Iq>,
}

impl Demod {
    /// Demodulate baseband at `rate` carrying a carrier of symbol rate `rs` and
    /// roll-off `alpha`, PL-scrambled with `gold_code`.
    ///
    /// # Panics
    /// If `rate / rs < 2`.
    pub fn new(rate: f64, rs: f64, alpha: f64, gold_code: u32) -> Self {
        let sps = rate / rs;
        Demod {
            rate,
            mf: Fir::new(rrc_taps(sps, alpha, 12)),
            agc: Agc::new(1.0, 0.2),
            sync: SymbolSync::new(sps, TIMING_BN_SEARCH, 0.02),
            corr: PlHeaderCorrelator::new(),
            dec: PlscDecoder::new(),
            scrambler: PlScrambler::new(gold_code),
            sym: Vec::new(),
            metric: Vec::new(),
            state: State::Searching { scan_from: 0 },
            filtered: Vec::new(),
            new_sym: Vec::new(),
            frames: 0,
            lost: 0,
            plsc_enc: PlscEncoder::new(),
            pll: CarrierPll::new(CARRIER_BN, 0.0),
            pll_live: false,
            seed: None,
            csts: BTreeMap::new(),
            hdr_ref: [Iq::new(0.0, 0.0); PLHEADER_LEN],
            sof_ref: {
                let mut sof = [Iq::new(0.0, 0.0); SOF_LEN];
                map_bpsk(SOF_BIG_ENDIAN, &mut sof, SOF_LEN);
                sof
            },
            data: Vec::new(),
            recent: VecDeque::with_capacity(RECENT),
            mer: 0.0,
            coherence: 0.0,
            fresh: true,
            modulation: None,
            gold_verified: false,
            raw: Vec::new(),
            slips: 0,
            freq_watch: Vec::new(),
            freq_fixes: 0,
            amp: 1.0,
            noise: 1.0,
            level_fresh: true,
            superframes: SfStripper::new(),
            stripped: Vec::new(),
        }
    }

    /// What the super-frame stage has found.
    pub fn superframes(&self) -> SfView {
        self.superframes.view()
    }

    /// A PLS code's frame as it comes: inside super-frames of format 0 or 1
    /// without pilot blocks of its own (the pilot bit then means SF-pilots).
    fn frame_pls(&self, pls: PlsInfo) -> PlsInfo {
        if self.superframes.view().carries_plframes() {
            pls.without_pilot_blocks()
        } else {
            pls
        }
    }

    /// Hint the carrier offset, cycles per symbol (e.g. Identify's, averaged
    /// over many headers). Used at acquisition if the headers agree with it.
    pub fn with_carrier_offset(mut self, cycles: f64) -> Self {
        self.seed = Some(cycles);
        self
    }

    /// The carrier loop is running (frames are flowing since acquisition).
    pub fn carrier_running(&self) -> bool {
        self.pll_live
    }

    /// The carrier loop is running and the data symbols sit on their points.
    pub fn carrier_locked(&self) -> bool {
        self.pll_live && self.coherence > LOCK_COHERENCE
    }

    /// MER of the data symbols, dB, smoothed over frames.
    pub fn mer_db(&self) -> f32 {
        self.mer
    }

    /// Es/N0 for display, dB: from the pilots' known symbols once there
    /// have been any, else the data MER. Decision-directed MER overstates a
    /// dense constellation near threshold — a noisy point snaps to the wrong
    /// neighbour and its error looks small: a live 1 MS/s ACM carrier read
    /// 12.2 dB on its 32APSK frames while its spectrum and pilots said 6–9.
    pub fn es_n0_db(&self) -> f32 {
        if self.level_fresh {
            self.mer
        } else {
            10.0 * (self.amp * self.amp / self.noise.max(1e-12)).log10()
        }
    }

    /// Residual carrier offset the loop is tracking, Hz.
    pub fn carrier_offset_hz(&self) -> f64 {
        self.pll.freq_cycles() * self.symbol_rate()
    }

    /// The PL scrambling sequence (gold code) in use: the one given, or a
    /// preferred one found to fit the pilots.
    pub fn gold_code(&self) -> u32 {
        self.scrambler.gold_code()
    }

    /// Modulation of the last frame that carried data.
    pub fn modulation(&self) -> Option<Modulation> {
        self.modulation
    }

    pub fn lock_state(&self) -> LockState {
        match self.state {
            State::Searching { .. } => LockState::Searching,
            State::Tracking { confirmed: 0, .. } => LockState::Found,
            State::Tracking { .. } => LockState::Locked,
        }
    }

    /// Symbol rate as the timing loop currently tracks it.
    pub fn symbol_rate(&self) -> f64 {
        self.rate / self.sync.sps()
    }

    /// The input is about to arrive `delta_hz` lower (the down-converter
    /// moved onto the carrier by that much): take it out of the loop's
    /// frequency and of the hint kept for re-acquisition.
    pub fn retune_hz(&mut self, delta_hz: f64) {
        let d = delta_hz / self.symbol_rate();
        self.pll.set_freq_cycles(self.pll.freq_cycles() - d);
        if let Some(s) = &mut self.seed {
            *s -= d;
        }
    }

    /// Frames emitted so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Stretches of data repaired after a carrier slip.
    pub fn slips_repaired(&self) -> u64 {
        self.slips
    }

    /// Times the carrier loop's frequency was found off against the headers'
    /// known symbols and put right.
    pub fn frequency_corrections(&self) -> u64 {
        self.freq_fixes
    }

    /// Times lock has been lost.
    pub fn losses(&self) -> u64 {
        self.lost
    }

    /// The most recent symbols for a constellation display: carrier-locked
    /// data symbols while frames flow, else the raw recovered symbols.
    pub fn recent_symbols(&self, n: usize) -> Vec<Iq> {
        if self.pll_live && !self.recent.is_empty() {
            let skip = self.recent.len().saturating_sub(n);
            self.recent.iter().skip(skip).copied().collect()
        } else {
            self.sym[self.sym.len().saturating_sub(n)..].to_vec()
        }
    }

    /// Feed baseband; completed, grid-confirmed frames are appended to `out`.
    pub fn process(&mut self, baseband: &[Iq], out: &mut Vec<PlFrame>) {
        // In chunks, so what the frame logic decides — a lock lost or gained,
        // and with it the timing loop's bandwidth — reaches the loops within
        // a few thousand samples whatever block size the caller uses. (One
        // call with everything after a dropout once ran the timing loop
        // across all of it still narrowed for lock, and never re-acquired.)
        for chunk in baseband.chunks(CHUNK) {
            self.filtered.clear();
            self.mf.process(chunk, &mut self.filtered);
            self.agc.process(&mut self.filtered);
            self.new_sym.clear();
            self.sync.process(&self.filtered, &mut self.new_sym);
            // Super-frames' own symbols out (none until some are found).
            self.stripped.clear();
            self.superframes.push(&self.new_sym, &mut self.stripped);

            self.sym.extend_from_slice(&self.stripped);
            self.corr.process(&self.stripped, &mut self.metric);
            self.advance(out);
            self.trim();
        }
    }

    fn decode_at(&mut self, hdr_end: usize) -> PlsInfo {
        // The last SOF symbol plus the 64 PLS symbols: 65 ending at `hdr_end`.
        let pls = self
            .dec
            .decode(&self.sym[hdr_end - 64..=hdr_end], PlscDemap::Differential);
        self.frame_pls(pls)
    }

    fn advance(&mut self, out: &mut Vec<PlFrame>) {
        loop {
            let len = self.sym.len();
            match self.state {
                State::Searching { scan_from } => {
                    // A peak needs a full PLHEADER behind it and a few symbols
                    // after it to prove it is the maximum.
                    let start = scan_from.max(PLHEADER_LEN);
                    let end = len.saturating_sub(PEAK_HALF_WIDTH);
                    let found = (start..end).find(|&j| {
                        let m = self.metric[j];
                        m > ACQUIRE_THRESHOLD
                            && (j - PEAK_HALF_WIDTH..=j + PEAK_HALF_WIDTH)
                                .all(|k| k == j || self.metric[k] < m)
                    });
                    match found {
                        Some(j) => {
                            let pls = self.decode_at(j);
                            self.state = State::Tracking {
                                hdr_end: j,
                                pls,
                                corr: self.metric[j],
                                confirmed: 0,
                            };
                        }
                        None => {
                            self.state = State::Searching { scan_from: end };
                            return;
                        }
                    }
                }
                State::Tracking {
                    hdr_end,
                    pls,
                    corr,
                    confirmed,
                } => {
                    let next = hdr_end + pls.plframe_len as usize;
                    if next + TOLERANCE >= len {
                        return; // wait for the rest of the frame
                    }
                    // Best differential correlation around where the next
                    // header should end.
                    let (best_d, best_m) = (next - TOLERANCE..=next + TOLERANCE)
                        .map(|k| (k, self.metric[k]))
                        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
                        .unwrap();

                    // Seed the carrier loop from this header and that one if
                    // it is not running. Acquisition reads both PLS codes
                    // coherently; the differential read that found this
                    // header is the weakest link near threshold (a QPSK 1/2
                    // header read as 3/5 at 6 dB: same length, so the grid
                    // held, and FEC ran the wrong code). The grid has proved
                    // the length, so only a same-length correction is taken.
                    let mut pls = pls;
                    if !self.pll_live {
                        let coherent =
                            self.acquire(hdr_end + 1 - PLHEADER_LEN, best_d + 1 - PLHEADER_LEN);
                        if coherent.plframe_len == pls.plframe_len {
                            pls = coherent;
                        }
                    }

                    // Carrier first: the loop then stands where the next
                    // header begins, and can read it coherently.
                    let p0 = hdr_end + 1;
                    if pls.has_pilots && !self.gold_verified {
                        self.check_gold(p0, pls);
                    }
                    let mut payload = self.sym[p0..p0 + pls.payload_len as usize].to_vec();
                    self.scrambler.descramble(&mut payload);
                    let (gain, noise_var) = self.recover_carrier(hdr_end, pls, &mut payload);
                    let vlsnr = pls.vlsnr.map(|set| self.vlsnr_finish(set, &mut payload));

                    let (k, coherent_pls, c) = self.best_coherent_header(next);
                    let confirmed_at = if c > COHERENT_CONFIRM {
                        Some((k, coherent_pls, c))
                    } else if best_m > CONFIRM_THRESHOLD {
                        Some((best_d, self.decode_at(best_d), best_m))
                    } else {
                        None
                    };

                    if let Some((best, next_pls, metric)) = confirmed_at {
                        // The grid held: this frame is real. Emit it.
                        out.push(PlFrame {
                            pls,
                            payload,
                            corr,
                            gain,
                            noise_var,
                            after_gap: confirmed == 0,
                            vlsnr,
                            src_s: None,
                        });
                        self.frames += 1;
                        // Frames on the grid: the timing loop can go quiet.
                        self.sync.set_bandwidth(TIMING_BN_LOCKED);

                        self.state = State::Tracking {
                            hdr_end: best,
                            pls: next_pls,
                            corr: metric,
                            confirmed: confirmed + 1,
                        };
                    } else {
                        if confirmed > 0 {
                            self.lost += 1;
                        }
                        // Remember where the carrier was for re-acquisition.
                        if self.carrier_locked() {
                            self.seed = Some(self.pll.freq_cycles());
                        }
                        self.pll_live = false;
                        self.sync.set_bandwidth(TIMING_BN_SEARCH);
                        self.state = State::Searching {
                            scan_from: hdr_end + 1,
                        };
                    }
                }
            }
        }
    }

    /// Read a header (first symbol `h0`) on a known carrier `phase` (radians,
    /// at `h0`) and frequency `w` (radians per symbol): decode its PLS code
    /// from coherent soft decisions, then measure how well the whole header
    /// matches what that code sends.
    fn read_header(&mut self, h0: usize, phase: f64, w: f64) -> (PlsInfo, f32) {
        let derotate = |n: usize, y: Iq| y * cis(-(phase + w * n as f64));
        // The decoder takes the last SOF symbol and the 64 PLS symbols.
        let mut buf = [Iq::new(0.0, 0.0); PLHEADER_LEN - SOF_LEN + 1];
        for (k, b) in buf.iter_mut().enumerate() {
            let n = SOF_LEN - 1 + k;
            *b = derotate(n, self.sym[h0 + n]);
        }
        // (Rust note: decoded first, then adjusted — the decoder's `&mut`
        // borrow must end before `frame_pls` borrows `self` again.)
        let pls = self.dec.decode(&buf, PlscDemap::CoherentSoft);
        let pls = self.frame_pls(pls);
        self.header_reference(pls.plsc);
        let (mut acc, mut mag) = (Iq::new(0.0, 0.0), 0f32);
        for n in 0..PLHEADER_LEN {
            let y = derotate(n, self.sym[h0 + n]);
            acc += y * self.hdr_ref[n].conj();
            mag += y.norm();
        }
        (pls, acc.norm() / mag.max(1e-12))
    }

    /// The header ending at `hdr_end`, read coherently from the running
    /// carrier loop (which must stand at that header): its phase anchored on
    /// the SOF, then [`Self::read_header`]. The loop itself is not moved.
    fn header_coherent(&mut self, hdr_end: usize) -> (PlsInfo, f32) {
        let h0 = hdr_end + 1 - PLHEADER_LEN;
        let mut probe = self.pll.clone();
        let sof = self.sof_ref;
        probe.anchor(&self.sym[h0..h0 + SOF_LEN], |n| sof[n]);
        self.read_header(h0, probe.phase(), TAU * probe.freq_cycles())
    }

    /// The best coherent header match within the tolerance around `next`:
    /// (header end, its PLS, match).
    fn best_coherent_header(&mut self, next: usize) -> (usize, PlsInfo, f32) {
        let mut best = (next, PlsInfo::parse(0), f32::MIN);
        for k in next - TOLERANCE..=next + TOLERANCE {
            let (pls, c) = self.header_coherent(k);
            if c > best.2 {
                best = (k, pls, c);
            }
        }
        best
    }

    /// The 90 PLHEADER symbols as sent, for a PLS code, into `hdr_ref`.
    fn header_reference(&mut self, plsc: u8) {
        map_bpsk(SOF_BIG_ENDIAN, &mut self.hdr_ref[..SOF_LEN], SOF_LEN);
        self.plsc_enc.encode(plsc, &mut self.hdr_ref[SOF_LEN..]);
    }

    /// Seed the loop from two headers (first symbols at `h0`, `h1`): with
    /// their modulation removed they are a pure tone at the offset. The SOFs
    /// (always the same) give a first frequency, on which both PLS codes are
    /// read coherently; the whole headers then refine it. Returns the first
    /// header's PLS as read coherently.
    fn acquire(&mut self, h0: usize, h1: usize) -> PlsInfo {
        let sof = self.sof_ref;
        let mut zs = [[Iq::new(0.0, 0.0); SOF_LEN]; 2];
        for (z, h) in zs.iter_mut().zip([h0, h1]) {
            for (n, v) in z.iter_mut().enumerate() {
                *v = self.sym[h + n] * sof[n].conj();
            }
        }
        let f0 = tone_freq(&[&zs[0], &zs[1]], &[1, 8]);
        let phase_of = |z: &[Iq], f: f64| -> f64 {
            let acc: Iq = z
                .iter()
                .enumerate()
                .map(|(n, &v)| {
                    let ph = -TAU * f * n as f64;
                    v * Iq::new(ph.cos() as f32, ph.sin() as f32)
                })
                .sum();
            acc.arg() as f64
        };
        let mut pls = [PlsInfo::parse(0); 2];
        let mut z = [[Iq::new(0.0, 0.0); PLHEADER_LEN]; 2];
        for k in 0..2 {
            let h = [h0, h1][k];
            pls[k] = self.read_header(h, phase_of(&zs[k], f0), TAU * f0).0;
            self.header_reference(pls[k].plsc);
            for (n, v) in z[k].iter_mut().enumerate() {
                *v = self.sym[h + n] * self.hdr_ref[n].conj();
            }
        }
        let f_hdr = tone_freq(&[&z[0], &z[1]], &[1, 8, 32]);
        // The headers' own SNR (the signal in them is the tone at `f_hdr`).
        let (mut a, mut p) = (0f32, 0f32);
        for zk in &z {
            let acc: Iq = zk
                .iter()
                .enumerate()
                .map(|(n, &v)| {
                    let ph = -TAU * f_hdr * n as f64;
                    v * Iq::new(ph.cos() as f32, ph.sin() as f32)
                })
                .sum();
            a += acc.norm() / (2 * PLHEADER_LEN) as f32;
            p += zk.iter().map(|v| v.norm_sqr()).sum::<f32>() / (2 * PLHEADER_LEN) as f32;
        }
        let snr = a * a / (p - a * a).max(a * a * 1e-6);
        // An outside hint (Identify's, averaged over many headers, or where
        // the carrier was before) beats the two headers' estimate where they
        // agree to within the latter's noise, which scales with the SNR: a
        // fixed window, wide enough for the noisy case, once let a hint a
        // few hundred hertz off start the loop outside its pull-in, and
        // frames failed for good while the headers still read a clean carrier.
        let window = SEED_SIGMAS * HDR_FREQ_SIGMA / f64::from(snr).sqrt();
        let f = match self.seed {
            Some(s) if (s - f_hdr).abs() < window => s,
            _ => f_hdr,
        };
        self.pll.set_freq_cycles(f);
        self.pll.set_phase(phase_of(&z[0], f));
        self.freq_watch.clear();
        // The loop's bandwidth from the headers' own SNR, so the first frame
        // is tracked as narrowly as the later ones (it starts wide otherwise,
        // and near threshold the first frame was lost to a slip).
        self.pll
            .set_bandwidth(carrier_bandwidth(10.0 * snr.log10() as f64));
        self.pll_live = true;
        self.fresh = true;
        self.level_fresh = true;
        pls[0]
    }

    /// Judge the carrier loop's frequency by the header at `h0` (`hdr_ref`
    /// holds it as sent; the loop must stand there, its phase anchored on
    /// it), and put it right if the last few headers all say it is out.
    ///
    /// Nothing else watches the loop's *frequency*: the headers and pilots
    /// re-anchor its phase, and the Es/N0 and the margin shown are measured
    /// on those same known symbols, so a loop whose frequency is a few
    /// hundred hertz out — started so by a stale hint, or walked off by
    /// decision errors near threshold — still reads a clean carrier while the
    /// data between the known blocks rotate away and every frame fails to
    /// decode. (With pilots it is worse: slip repair joins the blocks'
    /// phases across the data by the nearest turn, and a loop a pilot-
    /// spacing's reciprocal out settles on that alias for good.) The header
    /// is 90 known symbols: split in halves, the phase between them is the
    /// frequency error, unambiguous at any plausible error, and its
    /// noise is known from the SNR. One header says little; a few in a row
    /// that agree say the loop is out, and by how much.
    fn check_frequency(&mut self, h0: usize) {
        const HALF: usize = PLHEADER_LEN / 2;
        let (phase, w) = (self.pll.phase(), self.pll.freq_rad());
        let (mut first, mut second) = (Iq::new(0.0, 0.0), Iq::new(0.0, 0.0));
        let (mut acc, mut power) = (Iq::new(0.0, 0.0), 0f32);
        for n in 0..PLHEADER_LEN {
            let y = self.sym[h0 + n];
            let z = y * cis(-(phase + w * n as f64)) * self.hdr_ref[n].conj();
            if n < HALF {
                first += z;
            } else {
                second += z;
            }
            acc += z;
            power += y.norm_sqr();
        }
        let err = header_split_frequency(first, second, HALF);
        // The SNR the estimate's noise follows: the smoothed one once there
        // is one, else this header's own.
        let snr = if self.level_fresh {
            let a = acc.norm() / PLHEADER_LEN as f32;
            a * a / (power / PLHEADER_LEN as f32 - a * a).max(a * a * 1e-6)
        } else {
            self.amp * self.amp / self.noise.max(1e-12)
        };

        self.freq_watch.push(err);
        if self.freq_watch.len() > FREQ_WATCH_MAX {
            self.freq_watch.remove(0);
        }
        let n = self.freq_watch.len();
        if n < FREQ_WATCH_MIN {
            return;
        }
        let mean = self.freq_watch.iter().sum::<f64>() / n as f64;
        let sigma = HDR_FREQ_SIGMA / f64::from(snr).sqrt() / (n as f64).sqrt();
        if mean.abs() > (FREQ_WATCH_SIGMAS * sigma).max(FREQ_WATCH_FLOOR) {
            self.pll.set_freq_cycles(self.pll.freq_cycles() + mean);
            self.freq_watch.clear();
            self.freq_fixes += 1;
        }
    }

    /// Take the carrier off one frame (the loop must be running): its header
    /// (ending at `hdr_end`) then its descrambled `payload`, in place.
    /// Returns the smoothed (amplitude, noise variance) of the known symbols.
    fn recover_carrier(&mut self, hdr_end: usize, pls: PlsInfo, payload: &mut [Iq]) -> (f32, f32) {
        let h0 = hdr_end + 1 - PLHEADER_LEN;
        let mut known = Known::default();
        self.header_reference(pls.plsc);
        // Known blocks re-anchor the phase outright, so a cycle slip in the
        // data never outlives the next header or pilot block.
        let hdr = self.hdr_ref;
        self.pll
            .anchor(&self.sym[h0..h0 + PLHEADER_LEN], |n| hdr[n]);
        self.check_frequency(h0);
        for n in 0..PLHEADER_LEN {
            let y = self.pll.step_known(self.sym[h0 + n], self.hdr_ref[n]);
            known.add(y, self.hdr_ref[n]);
        }

        let cst = if pls.dummy_frame {
            None
        } else {
            Some(
                &*self
                    .csts
                    .entry(pls.plsc >> 1)
                    .or_insert_with(|| frame_constellation(pls)),
            )
        };
        self.data.clear();
        // A VL-SNR frame's extra pilot blocks are known symbols too.
        let layout = pls.vlsnr.map(vlsnr::layout);
        // Slip repair: the data between two known blocks are re-derotated
        // by interpolating between the phases the blocks measure when the
        // later block finds the loop drifted by more than this. A decision-
        // directed loop on APSK can settle a whole step off — 30° on 4+12's
        // outer ring, with only the faint inner ring disagreeing — and stay
        // there until the next pilots, which cost 16APSK 2–3 dB.
        let dense = cst.map_or(4, |c| decsat_dsp::carrier::densest_ring(&c.points));
        let slip = if cst.is_some() {
            SLIP_FRACTION * TAU / dense as f64
        } else {
            f64::INFINITY
        };
        // Through the data, a bandwidth for the decisions' own SNR: a dense
        // ring's points are closer together than QPSK's, by its point count
        // over 4, and the loop must be as much quieter.
        if let Some(c) = cst {
            let dense = decsat_dsp::carrier::densest_ring(&c.points).max(4) as f64;
            let snr = 10.0 * (self.amp * self.amp / self.noise).log10() as f64;
            self.pll
                .set_bandwidth(carrier_bandwidth(snr - 20.0 * (dense / 4.0).log10()));
        }
        // The levels the soft decisions below are made against: the smoothed
        // ones, or on a first frame this header's own.
        let (soft_amp, soft_noise) = match (self.level_fresh, known.estimate()) {
            (true, Some(level)) => level,
            _ => (self.amp, self.noise),
        };
        let inv_amp = 1.0 / soft_amp.max(1e-6);
        // (Capped: with next to no noise the weights are hard decisions
        // anyway, and an infinite scale would make 0 × ∞.)
        let inv_n0 = (soft_amp * soft_amp / soft_noise.max(1e-12)).min(1e4);
        // Soft decisions only where hard ones are unreliable, which is what
        // they cost (a weight per point a symbol) for: the squared distance
        // between the closest points over the noise per symbol is the
        // constellation's own measure of it (8PSK: 15 dB or less).
        let soft = cst.is_some_and(|c| {
            c.points.len() <= decsat_dsp::carrier::SOFT_MAX_POINTS
                && inv_n0 * min_distance_sq(&c.points) < SOFT_BELOW
        });
        self.raw.clear();
        self.raw.extend_from_slice(payload);
        // Where the current stretch of data began, and the loop's phase and
        // frequency there.
        let mut seg = (0usize, self.pll.phase(), self.pll.freq_rad());
        let mut in_block = false;
        for i in 0..payload.len() {
            let pilot = match layout {
                Some(l) => l[i] == Slot::Pilot,
                None => pls.has_pilots && i % PILOT_PERIOD >= PILOT_AFTER,
            };
            let block_start = match layout {
                Some(l) => pilot && (i == 0 || l[i - 1] != Slot::Pilot),
                None => pilot && i % PILOT_PERIOD == PILOT_AFTER,
            };
            if block_start {
                let end = match layout {
                    Some(l) => (i..l.len())
                        .find(|&j| l[j] != Slot::Pilot)
                        .unwrap_or(l.len()),
                    None => (i + PILOT_BLK_LEN).min(payload.len()),
                };
                let corr = self.pll.anchor(&payload[i..end], |_| PILOT);
                if corr.abs() > slip && i > seg.0 {
                    let (i0, ph0, f0) = seg;
                    let span = (i - i0) as f64;
                    // Unwrap the block's phase against where the frequency
                    // at the stretch's start says it should be, then
                    // interpolate linearly. Not the loop's frequency now:
                    // the slip being repaired can have kicked that by more
                    // than half a turn over the stretch (> 2.3 kHz at
                    // 6.75 MBd), and the wrong turn then lays a whole turn
                    // of ramp across data that were fine — on 172B's ACM
                    // outbound, 3 % of QPSK 2/3 frames at 13 dB (0.3 % so).
                    let want = ph0 + f0 * span;
                    let d = self.pll.phase() - want;
                    let ph1 = want + (d + PI).rem_euclid(TAU) - PI;
                    for (t, (y, &x)) in payload[i0..i].iter_mut().zip(&self.raw[i0..i]).enumerate()
                    {
                        *y = x * cis(-(ph0 + (ph1 - ph0) * t as f64 / span));
                    }
                    // A slip also kicked the loop's frequency: take the one
                    // the two blocks measure instead — on constellations
                    // denser than QPSK, which are the ones that slip and
                    // run where the blocks' phases are precise. (At QPSK's
                    // −1 dB the measured frequency is noisier than the
                    // loop's own: resetting it lost every 13/45 frame.)
                    if dense > 4 {
                        self.pll.set_freq_cycles((ph1 - ph0) / span / TAU);
                    }
                    self.slips += 1;
                }
                in_block = true;
            } else if in_block && !pilot {
                // The first symbol after a known block starts a stretch.
                seg = (i, self.pll.phase(), self.pll.freq_rad());
                in_block = false;
            }
            let x = payload[i];
            payload[i] = match cst {
                Some(c) if !pilot => {
                    let y = if soft {
                        self.pll.step_soft(x, &c.points, inv_amp, inv_n0)
                    } else {
                        self.pll.step(x, &c.points)
                    };
                    self.data.push(y);
                    y
                }
                // Pilots, and a dummy frame's whole payload.
                _ => {
                    let y = self.pll.step_known(x, PILOT);
                    known.add(y, PILOT);
                    y
                }
            };
        }

        if let Some((a, var)) = known.estimate() {
            if self.level_fresh {
                (self.amp, self.noise) = (a, var);
                self.level_fresh = false;
            } else {
                self.amp += 0.2 * (a - self.amp);
                self.noise += 0.2 * (var - self.noise);
            }
            self.pll.set_bandwidth(carrier_bandwidth(
                10.0 * (self.amp * self.amp / self.noise).log10() as f64,
            ));
        }
        let level = (self.amp, self.noise);

        let Some(c) = cst else { return level };
        let (mer, coh) = decsat_dsp::quality(&self.data, |_| &c.points);
        if self.fresh {
            (self.mer, self.coherence) = (mer, coh);
            self.fresh = false;
        } else {
            self.mer += 0.3 * (mer - self.mer);
            self.coherence += 0.3 * (coh - self.coherence);
        }
        self.modulation = Some(c.modulation);
        // The newest RECENT data symbols, older ones let go in one step.
        let take = self.data.len().min(RECENT);
        let excess = (self.recent.len() + take).saturating_sub(RECENT);
        self.recent.drain(..excess);
        self.recent.extend(&self.data[self.data.len() - take..]);
        level
    }

    /// How well `scr` descrambles the pilot blocks of the frame whose
    /// payload starts at `p0`: the differentials across each block of what
    /// should be one repeated symbol, coherently summed — near 1 when the
    /// sequence is right, near 0 when not, and blind to the carrier.
    fn pilot_coherence(&self, scr: &PlScrambler, p0: usize, pls: PlsInfo) -> f32 {
        let (mut acc, mut mag) = (Iq::new(0.0, 0.0), 0f32);
        for b in 0..pls.n_pilots as usize {
            let start = b * PILOT_PERIOD + PILOT_AFTER;
            for i in start..start + PILOT_BLK_LEN - 1 {
                let z0 = self.sym[p0 + i] * scr.factor(i);
                let z1 = self.sym[p0 + i + 1] * scr.factor(i + 1);
                acc += z1 * z0.conj();
                mag += z0.norm() * z1.norm();
            }
        }
        acc.norm() / mag.max(1e-12)
    }

    /// Check the scrambling sequence against a frame's pilots, and if it
    /// does not fit, look for one of Table 19e's that does.
    fn check_gold(&mut self, p0: usize, pls: PlsInfo) {
        if self.pilot_coherence(&self.scrambler, p0, pls) > GOLD_FITS {
            self.gold_verified = true;
            return;
        }
        let current = self.scrambler.gold_code();
        let best = PREFERRED_GOLD_CODES
            .iter()
            .filter(|&&n| n != current)
            .map(|&n| {
                let scr = PlScrambler::new(n);
                let c = self.pilot_coherence(&scr, p0, pls);
                (scr, c)
            })
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        if let Some((scr, c)) = best
            && c > GOLD_FITS
        {
            self.scrambler = scr;
            self.gold_verified = true;
        }
    }

    /// A VL-SNR frame's payload, carrier-corrected and descrambled as if
    /// all of it had been scrambled by quarter turns: put back the VL-SNR
    /// header (sent unscrambled), read it, and for pi/2-BPSK data take off
    /// the rest of their ±1 scrambling (EN 302 307-2 §5.5.4.1). Returns
    /// the header index and match.
    fn vlsnr_finish(&self, set: u8, payload: &mut [Iq]) -> (u8, f32) {
        for (i, y) in payload[..VLSNR_HEADER_LEN].iter_mut().enumerate() {
            *y *= self.scrambler.factor(i).conj();
        }
        let (k, c) = vlsnr::decode_header(&payload[..VLSNR_HEADER_LEN]);
        let bpsk = decsat_fec::vlsnr::VlsnrCode::for_header(k).is_some_and(|c| !c.qpsk);
        if bpsk {
            for (i, slot) in vlsnr::layout(set).iter().enumerate() {
                if *slot == Slot::Data {
                    payload[i] *= self.scrambler.factor(i);
                }
            }
        }
        (k, c)
    }

    /// Drop symbols nothing will look at again.
    fn trim(&mut self) {
        let keep_from = match self.state {
            State::Searching { scan_from } => scan_from.saturating_sub(PLHEADER_LEN + 8),
            State::Tracking { hdr_end, .. } => hdr_end.saturating_sub(PLHEADER_LEN + 8),
        };
        // Trim in large steps; shifting a big Vec every block would cost more
        // than it saves.
        if keep_from > 200_000 {
            self.sym.drain(..keep_from);
            self.metric.drain(..keep_from);
            match &mut self.state {
                State::Searching { scan_from } => *scan_from -= keep_from,
                State::Tracking { hdr_end, .. } => *hdr_end -= keep_from,
            }
        }
    }
}

/// Carrier loop bandwidth for an Es/N0: wide (0.01) where decisions are
/// clean, so phase noise is tracked; narrow (0.002) near threshold, where a
/// wide decision-directed loop slips cycles every few thousand symbols
/// (measured: QPSK frames failing from 6 dB down). Log-linear between 3 and
/// 15 dB.
fn carrier_bandwidth(es_n0_db: f64) -> f64 {
    let t = ((es_n0_db - 3.0) / 12.0).clamp(0.0, 1.0);
    0.002 * 5f64.powf(t)
}

/// The squared distance between the two closest of `points`.
fn min_distance_sq(points: &[Iq]) -> f32 {
    let mut least = f32::INFINITY;
    for (i, a) in points.iter().enumerate() {
        for b in &points[i + 1..] {
            least = least.min((a - b).norm_sqr());
        }
    }
    least
}

/// The constellation a frame's data is decided against; QPSK where the PLS
/// code names none — VL-SNR (pi/2-BPSK and QPSK, all on QPSK's points),
/// reserved codes, S2's unused MODCODs — whose decisions still track the
/// 4-fold symmetry every constellation has.
fn frame_constellation(pls: PlsInfo) -> Constellation {
    pls.modcod()
        .and_then(|mc| Constellation::for_modcod(&mc))
        .unwrap_or_else(Constellation::qpsk)
}

/// A tone's frequency, cycles per symbol, from the sums of its two halves of
/// `half` symbols each (the tone read through whatever phase the first
/// half's sum has): the phase the second half has gained over the first, over
/// the `half` symbols between their centres. Near the Cramér–Rao bound, and
/// unambiguous to ±1/(2·half) — far beyond a carrier loop's error.
fn header_split_frequency(first: Iq, second: Iq, half: usize) -> f64 {
    (second * first.conj()).arg() as f64 / (TAU * half as f64)
}

/// Frequency of a pure tone in noise, cycles per symbol, from blocks of it:
/// a lag-1 differential for an unambiguous coarse value, refined at each
/// longer lag in turn (each lag's phase is read only after the coarser
/// estimate is removed, so it never wraps while the error stays under
/// 1/(2·lag)).
pub(crate) fn tone_freq(blocks: &[&[Iq]], lags: &[usize]) -> f64 {
    let mut f = 0.0;
    for &lag in lags {
        let mut acc = Iq::new(0.0, 0.0);
        for b in blocks {
            for n in 0..b.len().saturating_sub(lag) {
                acc += b[n + lag] * b[n].conj();
            }
        }
        let w = -TAU * f * lag as f64;
        let resid = (acc * Iq::new(w.cos() as f32, w.sin() as f32)).arg() as f64;
        f += resid / (TAU * lag as f64);
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use decsat_mod::{FrameSpec, PlFramer, Shaper};

    fn signal(schedule: &[FrameSpec], n_sym: usize, sps: usize, alpha: f64, seed: u64) -> Vec<Iq> {
        let syms = PlFramer::new(0, seed).build_schedule(schedule, n_sym);
        let mut sh = Shaper::new(sps, alpha, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        x
    }

    #[test]
    fn follows_an_acm_sequence_frame_by_frame() {
        let schedule = [
            FrameSpec::new(4, false, true),
            FrameSpec::new(14, false, true),
            FrameSpec::new(20, true, false),
            FrameSpec::new(0, false, false),
            FrameSpec::new(9, true, true),
        ];
        let x = signal(&schedule, 250_000, 4, 0.25, 4);

        let mut d = Demod::new(4.0, 1.0, 0.25, 0);
        let mut frames = Vec::new();
        // Feed in uneven blocks, as a VFO would.
        for chunk in x.chunks(12_345) {
            d.process(chunk, &mut frames);
        }

        assert!(frames.len() >= 8, "only {} frames", frames.len());
        assert_eq!(d.lock_state(), LockState::Locked);
        assert_eq!(d.losses(), 0);

        // Frames must follow the schedule in order, from wherever lock began.
        let first = schedule
            .iter()
            .position(|s| s.modcod == frames[0].pls.modcod)
            .unwrap();
        for (k, f) in frames.iter().enumerate() {
            let want = schedule[(first + k) % schedule.len()];
            assert_eq!(f.pls.modcod, want.modcod, "frame {k}");
            assert_eq!(f.pls.short_fecframe, want.short_fecframe, "frame {k}");
            assert_eq!(f.payload.len(), f.pls.payload_len as usize);
        }
    }

    #[test]
    fn descrambled_pilots_are_constant() {
        // With pilots, the descrambled pilot blocks must all be the same symbol
        // (up to the residual carrier phase) — a check that descrambling lines
        // up with the frame start exactly.
        let x = signal(&[FrameSpec::new(4, false, true)], 120_000, 4, 0.2, 7);
        let mut d = Demod::new(4.0, 1.0, 0.2, 0);
        let mut frames = Vec::new();
        d.process(&x, &mut frames);
        let f = frames.last().expect("no frames");

        // First pilot block: after 16 slots of 90 data symbols.
        let p = &f.payload[16 * 90..16 * 90 + 36];
        let mean = p.iter().sum::<Iq>() / 36.0;
        let spread = p.iter().map(|s| (s - mean).norm()).fold(0.0f32, f32::max);
        assert!(mean.norm() > 0.8, "pilot mean {mean}");
        assert!(spread < 0.15, "pilots not constant: spread {spread}");
    }

    #[test]
    fn recovers_after_a_dropout() {
        let x = signal(&[FrameSpec::new(4, true, false)], 200_000, 4, 0.35, 8);
        let mut d = Demod::new(4.0, 1.0, 0.35, 0);
        let mut frames = Vec::new();

        let cut = x.len() / 2;
        d.process(&x[..cut], &mut frames);
        let before = frames.len();
        // 30 000 samples of silence where the signal should be.
        d.process(&vec![Iq::new(0.0, 0.0); 30_000], &mut frames);
        d.process(&x[cut + 30_000..], &mut frames);

        assert!(before >= 3);
        assert!(d.losses() >= 1, "the dropout should have broken lock");
        assert!(frames.len() > before + 3, "did not re-acquire");
        assert_eq!(d.lock_state(), LockState::Locked);
    }

    #[test]
    fn noise_alone_never_produces_frames() {
        let mut s = 0x1234_5678u64;
        let x: Vec<Iq> = (0..400_000)
            .map(|_| {
                s ^= s >> 12;
                s ^= s << 25;
                s ^= s >> 27;
                let v = s.wrapping_mul(0x2545_F491_4F6C_DD1D);
                Iq::new(
                    ((v >> 40) as f32 / (1u32 << 24) as f32) - 0.5,
                    (((v >> 16) & 0xFF_FFFF) as f32 / (1u32 << 24) as f32) - 0.5,
                )
            })
            .collect();
        let mut d = Demod::new(4.0, 1.0, 0.35, 0);
        let mut frames = Vec::new();
        d.process(&x, &mut frames);
        assert!(
            frames.is_empty(),
            "{} false frames from noise",
            frames.len()
        );
    }

    /// `x` offset by `cycles` per sample, rotated by `phase`, plus complex
    /// Gaussian noise of power `10^(-snr_db/10)` per sample (unit signal).
    fn impair(x: &[Iq], cycles: f64, phase: f64, snr_db: f64, seed: u64) -> Vec<Iq> {
        let mut s = seed | 1;
        let mut uniform = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
        };
        let sigma = 10f64.powf(-snr_db / 20.0) * std::f64::consts::FRAC_1_SQRT_2;
        x.iter()
            .enumerate()
            .map(|(n, &v)| {
                let ph = TAU * cycles * n as f64 + phase;
                let r = (-2.0 * uniform().max(1e-300).ln()).sqrt() * sigma;
                let t = TAU * uniform();
                v * Iq::new(ph.cos() as f32, ph.sin() as f32)
                    + Iq::new((r * t.cos()) as f32, (r * t.sin()) as f32)
            })
            .collect()
    }

    #[test]
    fn locks_the_carrier_of_an_acm_sequence() {
        // QPSK, 8PSK, short 16APSK, dummy and short QPSK frames, 0.3 % of the
        // symbol rate off and rotated: every data frame must come out on its
        // own constellation, and the pilots on their true phase (the header
        // resolves the 90° ambiguity a blind loop would be left with).
        let schedule = [
            FrameSpec::new(4, false, true),
            FrameSpec::new(14, false, true),
            FrameSpec::new(20, true, false),
            FrameSpec::new(0, false, false),
            FrameSpec::new(9, true, true),
        ];
        let x = signal(&schedule, 300_000, 4, 0.25, 11);
        let x = impair(&x, 0.003 / 4.0, 2.2, 20.0, 12);

        let mut d = Demod::new(4.0, 1.0, 0.25, 0);
        let mut frames = Vec::new();
        for chunk in x.chunks(20_000) {
            d.process(chunk, &mut frames);
        }
        assert!(frames.len() >= 8, "only {} frames", frames.len());
        assert_eq!(d.losses(), 0);
        assert!(d.carrier_locked(), "MER {:.1} dB", d.mer_db());
        assert!(d.mer_db() > 14.0, "MER {:.1} dB", d.mer_db());
        let f = d.carrier_offset_hz();
        assert!((f - 0.003).abs() < 2e-4, "offset {f}");

        // After the first frame, every pilot block sits on (1 + j)/sqrt(2).
        for fr in frames.iter().skip(1).filter(|f| f.pls.has_pilots) {
            let p = &fr.payload[PILOT_AFTER..PILOT_PERIOD];
            let mean = p.iter().sum::<Iq>() / PILOT_BLK_LEN as f32;
            assert!(
                (mean - PILOT).norm() < 0.15,
                "MODCOD {} pilots at {mean}",
                fr.pls.modcod
            );
        }
        // And the data of each frame on its own constellation.
        for fr in frames.iter().skip(1).filter(|f| !f.pls.dummy_frame) {
            let c = frame_constellation(fr.pls);
            let data: Vec<Iq> = fr
                .payload
                .iter()
                .enumerate()
                .filter(|(i, _)| !fr.pls.has_pilots || i % PILOT_PERIOD < PILOT_AFTER)
                .map(|(_, &v)| v)
                .collect();
            let mer = decsat_dsp::mer_db(&data, &c.points);
            assert!(mer > 14.0, "MODCOD {}: MER {mer:.1} dB", fr.pls.modcod);
        }
    }

    #[test]
    fn a_wrong_seed_is_overruled_by_the_headers() {
        let x = signal(&[FrameSpec::new(4, false, true)], 150_000, 4, 0.2, 13);
        let x = impair(&x, -0.01 / 4.0, 0.3, 20.0, 14);
        let mut d = Demod::new(4.0, 1.0, 0.2, 0).with_carrier_offset(0.04);
        let mut frames = Vec::new();
        d.process(&x, &mut frames);
        assert!(d.carrier_locked(), "MER {:.1} dB", d.mer_db());
        assert!((d.carrier_offset_hz() + 0.01).abs() < 2e-4);
    }

    #[test]
    fn a_stale_hint_does_not_leave_the_loop_off_the_carrier() {
        // 8PSK 3/5 (MODCOD 12) near its threshold, the carrier 0.35 % of the
        // symbol rate off, and a hint 0.04 % (400 Hz at 1 MBd) from it: well
        // inside the old trust window, outside what a decision-directed loop
        // pulls in at this SNR. Without pilots the loop sat out of tune for
        // good; with them slip repair settled on a frequency a pilot-spacing's
        // reciprocal off. Either way the headers and pilots kept reading a
        // clean carrier, so the Es/N0 and margin looked fine while every
        // frame failed to decode. The loop must end on the carrier.
        //
        // Over 8 noise realisations, not one: near threshold an unpiloted
        // decision-directed loop pulls in slowly, and in a few realisations
        // in a hundred (9 of 300 tried) it is still 1e-4 to 3e-4 out after 32
        // frames, the data demodulating as cleanly meanwhile. Which
        // realisation that is changes with the platform's maths library, so
        // a single seed that settled on Windows failed on Linux. With pilots
        // the old code ended every time at +6.8e-4, a pilot spacing's
        // reciprocal (1/1476) out, and the new one never in 300: all must
        // settle. Without, most must, and none may end farther out than the
        // hint began.
        const SEEDS: u64 = 8;
        for pilots in [true, false] {
            let carrier = 0.0035;
            let mut errs = Vec::new();
            for k in 0..SEEDS {
                let x = signal(
                    &[FrameSpec::new(12, false, pilots)],
                    700_000,
                    4,
                    0.2,
                    31 + 100 * k,
                );
                // (`impair`'s SNR is per sample: 7 dB Es/N0 at 4 samples a
                // symbol is 6 dB less.)
                let x = impair(
                    &x,
                    carrier / 4.0,
                    0.9,
                    7.0 - 10.0 * 4f64.log10(),
                    32 + 100 * k,
                );
                let mut d = Demod::new(4.0, 1.0, 0.2, 0).with_carrier_offset(carrier + 4.0e-4);
                let mut frames = Vec::new();
                for chunk in x.chunks(50_000) {
                    d.process(chunk, &mut frames);
                }
                assert!(
                    frames.len() >= 25,
                    "pilots {pilots}, seed {k}: {} frames",
                    frames.len()
                );
                errs.push(d.carrier_offset_hz() - carrier);
            }
            let settled = errs.iter().filter(|e| e.abs() < 4.0e-5).count();
            let worst = errs.iter().fold(0.0f64, |w, e| w.max(e.abs()));
            let need = if pilots { SEEDS as usize } else { 6 };
            assert!(
                settled >= need && worst < 4.0e-4,
                "pilots {pilots}: {settled} of {SEEDS} loops settled on the carrier (need \
                 {need}), the worst {worst:.1e} cycles/symbol off: {}",
                errs.iter()
                    .map(|e| format!("{e:+.1e}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
    }

    /// Complex Gaussian noise generator for the tests below.
    fn noise_source(seed: u64) -> impl FnMut(f64) -> Iq {
        let mut s = seed | 1;
        let mut uniform = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
        };
        move |sigma| {
            let r = (-2.0 * uniform().max(1e-300).ln()).sqrt()
                * sigma
                * std::f64::consts::FRAC_1_SQRT_2;
            let t = TAU * uniform();
            Iq::new((r * t.cos()) as f32, (r * t.sin()) as f32)
        }
    }

    #[test]
    fn header_frequency_estimates_scatter_as_predicted() {
        // A header's known symbols with modulation removed are a tone in
        // noise. Both estimators (the split halves the carrier watch uses,
        // and `tone_freq` over two headers, which seeds the loop) must scatter
        // no more than HDR_FREQ_SIGMA says at the SNRs where they matter, or
        // the watch cries wolf and the hint window rejects good hints.
        let mut noise = noise_source(21);
        let f_true = 3.1e-3;
        for snr_db in [3.0f64, 6.0, 10.0, 20.0] {
            let sigma = 10f64.powf(-snr_db / 20.0);
            let predicted = HDR_FREQ_SIGMA / 10f64.powf(snr_db / 20.0);
            let (mut split, mut pair) = (Vec::new(), Vec::new());
            for k in 0..400 {
                let block = |start: usize, phase: f64, noise: &mut dyn FnMut(f64) -> Iq| {
                    (0..PLHEADER_LEN)
                        .map(|n| {
                            let ph = TAU * f_true * (start + n) as f64 + phase;
                            cis(ph) + noise(sigma)
                        })
                        .collect::<Vec<Iq>>()
                };
                let a = block(0, 0.3 * k as f64, &mut noise);
                let b = block(21_690, 1.1 * k as f64, &mut noise);
                let half = PLHEADER_LEN / 2;
                let sum = |z: &[Iq]| z.iter().sum::<Iq>();
                split.push(header_split_frequency(sum(&a[..half]), sum(&a[half..]), half) - f_true);
                pair.push(tone_freq(&[&a, &b], &[1, 8, 32]) - f_true);
            }
            let rms = |v: &[f64]| (v.iter().map(|x| x * x).sum::<f64>() / v.len() as f64).sqrt();
            assert!(
                rms(&split) < predicted,
                "{snr_db} dB: split halves rms {:.2e} against {predicted:.2e}",
                rms(&split)
            );
            // Two blocks should do better than one; at least as well as the
            // single-header figure.
            assert!(
                rms(&pair) < predicted,
                "{snr_db} dB: two headers rms {:.2e} against {predicted:.2e}",
                rms(&pair)
            );
        }
    }

    #[test]
    fn tone_freq_is_unambiguous_and_fine() {
        for f in [-0.2, -0.013, 0.0, 0.0047, 0.11] {
            let mut b = [[Iq::new(0.0, 0.0); PLHEADER_LEN]; 2];
            for (k, blk) in b.iter_mut().enumerate() {
                for (n, v) in blk.iter_mut().enumerate() {
                    let ph = TAU * f * (n + 1000 * k) as f64 + 0.7 * k as f64;
                    *v = Iq::new(ph.cos() as f32, ph.sin() as f32);
                }
            }
            assert!(
                (tone_freq(&[&b[0], &b[1]], &[1, 8, 32]) - f).abs() < 1e-6,
                "{f}"
            );
        }
    }
}
