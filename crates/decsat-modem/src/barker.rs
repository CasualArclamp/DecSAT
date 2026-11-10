// made by CasualArclamp
//! A Barker-11 direct-sequence BPSK carrier with TDM framing, as found on a
//! C-band downlink near 4004 MHz: the receiver and the framer.
//!
//! No specification is known for this signal. Every number here was
//! measured from an off-air recording (November 2026, two carriers 250 kHz
//! apart, each beside a comb of eight unmodulated pilot tones) and is
//! marked "measured" where it is used:
//!
//! - **Spreading:** each BPSK symbol is one period of the 11-chip Barker
//!   code, chips 6 µs long — 166.667 kchip/s, 15 151.5 symbols/s, about
//!   220 kHz occupied. The symbol clock is locked to the code (one code
//!   period per symbol), so the code phase gives the symbol timing.
//! - **Frames:** a 255-symbol preamble — 128 ones, then one period of a
//!   127-bit maximal-length sequence — starts every frame. Inside the
//!   frame, data blocks are separated by a 64-bit unique word (a 63-bit
//!   maximal-length sequence and a closing zero; it begins 7Eh), sent once
//!   or three times running. Two layouts were seen:
//!   254 + 512 + 2 × (64 + 512) + 1 = 1919 symbols (126.65 ms), and
//!   254 + 576 + 7 × (192 + 576) + 1 = 6207 symbols (409.66 ms).
//!   The framer does not assume either: it cuts frames at the preamble and
//!   finds the unique words in each.
//! - **Where the first block starts:** cut after all 255 preamble symbols,
//!   the first block comes out one symbol short and the last one long. The
//!   blocks' fixed bit positions line up only if the first block starts on
//!   the preamble's last symbol (a zero, as that position is in every
//!   block) and one spare symbol ends the frame — so that is how the
//!   blocks are cut here. It is a reading of the data, not a known rule.
//! - **Data:** absolute (not differential) BPSK. The blocks are not
//!   whitened — a quarter of the bit positions, on a 32- or 96-bit pattern,
//!   stayed zero through the recording — and are passed on as they are;
//!   what they carry is not known.
//!
//! The receiver is the usual spread-spectrum one: a search over the code
//! phase, then a delay-locked loop on early and late correlators, and a
//! Costas loop on the despread symbols. It runs at the rate it is given
//! (no resampler): the code is stepped by a fraction of a chip per sample.

use decsat_core::Iq;

/// Chips a second (measured: 6.000 µs chips).
pub const CHIP_RATE: f64 = 1e6 / 6.0;
/// Chips in the code, and so in a symbol.
pub const CHIPS: usize = 11;
/// Symbols a second: one code period each.
pub const SYMBOL_RATE: f64 = CHIP_RATE / CHIPS as f64;
/// The code in the order sent (measured): Barker 11, time-reversed. Its
/// overall sign is not observable — it is the BPSK polarity, which the
/// preamble settles.
pub const CODE: [f32; CHIPS] = [-1.0, 1.0, -1.0, -1.0, 1.0, -1.0, -1.0, -1.0, 1.0, 1.0, 1.0];

/// The preamble: this many ones, then the 127-bit sequence (measured).
pub const PREAMBLE_ONES: usize = 128;
pub const PREAMBLE_BITS: usize = PREAMBLE_ONES + 127;
/// The first data block starts on the preamble's last symbol, and this
/// many spare symbols end the frame (measured; see the module's notes).
pub const FIRST_BLOCK_AT: usize = PREAMBLE_BITS - 1;
pub const SPARE_SYMBOLS: usize = 1;
/// The unique word between data blocks, first bit in the top bit
/// (measured): the 63-bit sequence of x⁶ + x⁵ + 1 from 011111, and a zero.
pub const UNIQUE_WORD: u64 = 0x7E08_629E_8E4B_766A;
pub const UNIQUE_WORD_BITS: usize = 64;

/// Wrong bits allowed in a preamble (of 255) and a unique word (of 64).
const PREAMBLE_MAX_ERRORS: u32 = 24;
const UNIQUE_WORD_MAX_ERRORS: u32 = 6;
/// A frame longer than this many symbols is given up (4 s).
const MAX_FRAME_BITS: usize = 60_000;

/// Symbols looked at to find the code phase, and the step of the search in
/// chips.
const ACQ_SYMBOLS: usize = 48;
const ACQ_STEP: f64 = 0.25;
/// The best code phase's energy over the median phase's, to take it:
/// noise alone gives 1.3 at most, a carrier at Es/N0 = 3 dB about 1.9.
const ACQ_RATIO: f32 = 1.8;
/// Searches skipped after one that found nothing (it costs 44 passes over
/// the block).
const ACQ_REST: u32 = 3;
/// Symbols of frequency-locked pull-in before the Costas loop takes over.
const FLL_SYMBOLS: u32 = 400;
const FLL_GAIN: f32 = 0.08;
/// Costas loop, per symbol: noise bandwidth 150 Hz at damping 0.707
/// (ωn = 283 rad/s, T = 66 µs).
const PLL_KP: f32 = 0.0264;
const PLL_KI: f32 = 3.49e-4;
/// Delay-locked loop: chips moved per unit of (E − L)/(E + L).
const DLL_GAIN: f64 = 0.02;
/// Smoothing of the quality figures, per symbol.
const QUALITY_ALPHA: f32 = 1.0 / 1024.0;
/// Lock is dropped when the symbols look like noise (|I| mean² under this
/// share of Q's power; noise alone gives 0.64) this many symbols running.
const LOST_RATIO: f32 = 1.2;
const LOST_SYMBOLS: u32 = 6000;
/// Points kept for the scatter, and frames kept in the view.
const SCATTER: usize = 512;
const KEEP: usize = 4;

/// One frame, from the start of its preamble to the next.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BarkerFrame {
    /// Its length in symbols, preamble included.
    pub symbols: usize,
    /// Wrong bits in its preamble.
    pub preamble_errors: u32,
    /// Unique words found, and the wrong bits in them together.
    pub unique_words: usize,
    pub unique_word_errors: u32,
    /// The carrier slipped half a cycle inside the frame (the next
    /// preamble came the other way up): the data after the slip are
    /// inverted.
    pub slipped: bool,
    /// "254 + 512 + 2×(64 + 512) + 1": the preamble before the first
    /// block, the blocks and unique words, the spare symbol.
    pub layout: String,
    /// The data blocks in order: their length in bits, and the bits packed
    /// first bit in the top bit (the last byte zero-padded).
    pub blocks: Vec<(usize, Vec<u8>)>,
}

/// What the card shows.
#[derive(Debug, Clone, Default)]
pub struct BarkerView {
    /// The code is found and tracked.
    pub locked: bool,
    /// The carrier's offset from the VFO centre (Hz), while locked.
    pub carrier_hz: Option<f64>,
    /// Es/N0 of the despread symbols (dB).
    pub esn0_db: f32,
    /// The chip clock against [`CHIP_RATE`] by this receiver's clock (ppm).
    pub chip_ppm: f32,
    pub symbols: u64,
    pub frames: u64,
    /// Frames with a half-cycle slip inside.
    pub slipped: u64,
    /// The latest frame's length (symbols) and layout.
    pub frame_symbols: Option<usize>,
    pub layout: String,
    /// The latest frames, oldest first.
    pub recent: Vec<BarkerFrame>,
    pub message: String,
}

/// The preamble as bits, first bit first: ones, then the sequence of
/// b[n] = b[n−2] ⊕ b[n−4] ⊕ b[n−6] ⊕ b[n−7] from seven ones (measured).
pub fn preamble() -> Vec<u8> {
    let mut b = vec![1u8; PREAMBLE_ONES + 7];
    while b.len() < PREAMBLE_BITS {
        let n = b.len();
        b.push(b[n - 2] ^ b[n - 4] ^ b[n - 6] ^ b[n - 7]);
    }
    b
}

/// Finds preambles in the bit stream, cuts frames at them and finds the
/// unique words in each. Polarity is taken from the preamble.
struct Framer {
    /// The last 256 bits, newest in bit 0 of `reg[0]`.
    reg: [u64; 4],
    pattern: [u64; 4],
    mask: [u64; 4],
    /// Bits since the start of the latest preamble, as received.
    bits: Vec<u8>,
    /// That preamble: inverted, and its wrong bits.
    start: Option<(bool, u32)>,
}

impl Framer {
    fn new() -> Self {
        let mut pattern = [0u64; 4];
        let mut mask = [0u64; 4];
        // The newest bit is the preamble's last: bit i of the register
        // holds preamble bit 254 − i.
        for (i, &b) in preamble().iter().rev().enumerate() {
            pattern[i / 64] |= (b as u64) << (i % 64);
            mask[i / 64] |= 1 << (i % 64);
        }
        Framer {
            reg: [0; 4],
            pattern,
            mask,
            bits: Vec::new(),
            start: None,
        }
    }

    fn reset(&mut self) {
        self.reg = [0; 4];
        self.bits.clear();
        self.start = None;
    }

    fn push(&mut self, bit: u8, out: &mut Vec<BarkerFrame>) {
        // Shift the 256-bit register left by one.
        for k in (1..4).rev() {
            self.reg[k] = (self.reg[k] << 1) | (self.reg[k - 1] >> 63);
        }
        self.reg[0] = (self.reg[0] << 1) | bit as u64;
        if self.start.is_some() {
            self.bits.push(bit);
        }
        let wrong: u32 = (0..4)
            .map(|k| ((self.reg[k] ^ self.pattern[k]) & self.mask[k]).count_ones())
            .sum();
        let total = PREAMBLE_BITS as u32;
        let hit = if wrong <= PREAMBLE_MAX_ERRORS {
            Some((false, wrong))
        } else if wrong >= total - PREAMBLE_MAX_ERRORS {
            Some((true, total - wrong))
        } else {
            None
        };
        if let Some((inverted, errors)) = hit {
            if let Some((was_inverted, was_errors)) = self.start {
                let n = self.bits.len() - PREAMBLE_BITS;
                let flip = was_inverted as u8;
                let frame: Vec<u8> = self.bits[..n].iter().map(|&b| b ^ flip).collect();
                let mut f = parse(&frame);
                f.preamble_errors = was_errors;
                f.slipped = was_inverted != inverted;
                out.push(f);
                self.bits.drain(..n);
            } else {
                // The first preamble: put it at the head of the frame.
                self.bits = (0..PREAMBLE_BITS)
                    .map(|i| {
                        let j = PREAMBLE_BITS - 1 - i;
                        ((self.reg[j / 64] >> (j % 64)) & 1) as u8
                    })
                    .collect();
            }
            self.start = Some((inverted, errors));
        } else if self.bits.len() > MAX_FRAME_BITS {
            self.bits.clear();
            self.start = None;
        }
    }
}

/// Cut a frame (bits the right way up, preamble first) into its blocks.
fn parse(bits: &[u8]) -> BarkerFrame {
    let mut f = BarkerFrame {
        symbols: bits.len(),
        ..Default::default()
    };
    // (is a unique word, length in bits) after the preamble.
    let mut items: Vec<(bool, usize)> = Vec::new();
    let mut block_start = FIRST_BLOCK_AT.min(bits.len());
    // The spare symbol at the end belongs to no block.
    let bits = &bits[..bits.len().saturating_sub(SPARE_SYMBOLS).max(block_start)];
    let mut reg = 0u64;
    let mut filled = 0usize;
    let mut i = block_start;
    while i < bits.len() {
        reg = (reg << 1) | bits[i] as u64;
        filled += 1;
        i += 1;
        if filled < UNIQUE_WORD_BITS {
            continue;
        }
        let wrong = (reg ^ UNIQUE_WORD).count_ones();
        if wrong <= UNIQUE_WORD_MAX_ERRORS {
            let at = i - UNIQUE_WORD_BITS;
            if at > block_start {
                f.blocks.push(pack(&bits[block_start..at]));
                items.push((false, at - block_start));
            }
            // Unique words sent back to back count as one run.
            match items.last_mut() {
                Some((true, n)) => *n += UNIQUE_WORD_BITS,
                _ => items.push((true, UNIQUE_WORD_BITS)),
            }
            f.unique_words += 1;
            f.unique_word_errors += wrong;
            block_start = i;
            filled = 0;
        }
    }
    if bits.len() > block_start {
        f.blocks.push(pack(&bits[block_start..]));
        items.push((false, bits.len() - block_start));
    }
    f.layout = layout(&items);
    f
}

/// Bits to bytes, first bit in the top bit.
fn pack(bits: &[u8]) -> (usize, Vec<u8>) {
    let mut out = vec![0u8; bits.len().div_ceil(8)];
    for (i, &b) in bits.iter().enumerate() {
        out[i / 8] |= b << (7 - i % 8);
    }
    (bits.len(), out)
}

/// "254 + 512 + 2×(64 + 512) + 1": the preamble, the first block, then the
/// (unique words, block) pairs — folded when they are all alike — and the
/// spare symbol.
fn layout(items: &[(bool, usize)]) -> String {
    let mut s = FIRST_BLOCK_AT.to_string();
    let mut rest = items;
    if let Some(&(false, n)) = rest.first() {
        s += &format!(" + {n}");
        rest = &rest[1..];
    }
    // Rust note: `as_chunks::<2>()` gives the slice as pairs (and whatever
    // is left over, not used here).
    let pairs: Vec<(usize, usize)> = rest
        .as_chunks::<2>()
        .0
        .iter()
        .filter(|c| c[0].0 && !c[1].0)
        .map(|c| (c[0].1, c[1].1))
        .collect();
    let regular = rest.len() == 2 * pairs.len() && pairs.windows(2).all(|w| w[0] == w[1]);
    if regular && pairs.len() > 1 {
        s += &format!(" + {}×({} + {})", pairs.len(), pairs[0].0, pairs[0].1);
    } else {
        for &(_, n) in rest {
            s += &format!(" + {n}");
        }
    }
    s + &format!(" + {SPARE_SYMBOLS}")
}

/// The chip under code phase `ph` (chips; may be a little negative or past
/// the end, as the early and late taps are).
#[inline]
fn chip(ph: f64) -> f32 {
    CODE[(ph + CHIPS as f64) as usize % CHIPS]
}

pub struct BarkerRx {
    rate: f64,
    /// Chips per input sample, nominal.
    chips_per_sample: f64,
    samples_per_symbol: f64,
    /// Samples gathered for the code search.
    acq: Vec<Iq>,
    acq_len: usize,
    rest: u32,
    locked: bool,
    /// Code phase of the next sample, chips.
    code_phase: f64,
    /// Chips the delay-locked loop has moved the code since lock, and the
    /// symbols it took: the chip clock's error.
    dll_moved: f64,
    dll_symbols: u64,
    /// The carrier NCO: its phasor and the phase step per sample.
    rot: Iq,
    step: Iq,
    /// Carrier frequency, radians per symbol.
    freq: f32,
    /// Correlators: prompt, early, late.
    p: Iq,
    e: Iq,
    l: Iq,
    last: Iq,
    since_lock: u32,
    /// Smoothed |I| and Q² of the symbols.
    mean_i: f32,
    mean_q2: f32,
    poor: u32,
    framer: Framer,
    done: Vec<BarkerFrame>,
    scatter: Vec<Iq>,
    view: BarkerView,
}

impl BarkerRx {
    pub fn new(rate: f64) -> Self {
        let samples_per_symbol = rate / SYMBOL_RATE;
        let message = if rate < 1.5 * CHIP_RATE {
            format!(
                "The VFO is too narrow: {:.0} kHz, the code needs 250 kHz",
                rate / 1e3
            )
        } else {
            "Looking for the code".to_string()
        };
        BarkerRx {
            rate,
            chips_per_sample: CHIP_RATE / rate,
            samples_per_symbol,
            acq: Vec::new(),
            acq_len: (ACQ_SYMBOLS as f64 * samples_per_symbol).ceil() as usize,
            rest: 0,
            locked: false,
            code_phase: 0.0,
            dll_moved: 0.0,
            dll_symbols: 0,
            rot: Iq::new(1.0, 0.0),
            step: Iq::new(1.0, 0.0),
            freq: 0.0,
            p: Iq::new(0.0, 0.0),
            e: Iq::new(0.0, 0.0),
            l: Iq::new(0.0, 0.0),
            last: Iq::new(0.0, 0.0),
            since_lock: 0,
            mean_i: 0.0,
            mean_q2: 0.0,
            poor: 0,
            framer: Framer::new(),
            done: Vec::new(),
            scatter: Vec::with_capacity(SCATTER),
            view: BarkerView {
                message,
                ..Default::default()
            },
        }
    }

    /// Take samples at the rate given to [`BarkerRx::new`].
    pub fn push(&mut self, input: &[Iq]) {
        if self.rate < 1.5 * CHIP_RATE {
            return;
        }
        let mut input = input;
        while !input.is_empty() {
            if self.locked {
                let used = self.track(input);
                input = &input[used..];
            } else {
                let want = self.acq_len - self.acq.len();
                let take = want.min(input.len());
                self.acq.extend_from_slice(&input[..take]);
                input = &input[take..];
                if self.acq.len() == self.acq_len {
                    if self.rest > 0 {
                        self.rest -= 1;
                    } else if !self.acquire() {
                        self.rest = ACQ_REST;
                    }
                    self.acq.clear();
                }
            }
        }
        self.collect();
    }

    pub fn view(&self) -> BarkerView {
        self.view.clone()
    }

    pub fn scatter(&self) -> Vec<Iq> {
        self.scatter.clone()
    }

    /// Search the gathered block for the code phase. Each symbol is
    /// correlated in two halves whose powers are added, so a carrier a few
    /// kHz off still shows; the halves' phase step gives that offset.
    fn acquire(&mut self) -> bool {
        let steps = (CHIPS as f64 / ACQ_STEP) as usize;
        let half = CHIPS as f64 / 2.0;
        let mut energy = vec![0f32; steps];
        let mut turn = vec![Iq::new(0.0, 0.0); steps];
        for (k, (en, tu)) in energy.iter_mut().zip(turn.iter_mut()).enumerate() {
            let mut ph = k as f64 * ACQ_STEP;
            let (mut a, mut b) = (Iq::new(0.0, 0.0), Iq::new(0.0, 0.0));
            for &x in &self.acq {
                let c = chip(ph);
                if ph < half {
                    a += x * c;
                } else {
                    b += x * c;
                }
                ph += self.chips_per_sample;
                if ph >= CHIPS as f64 {
                    ph -= CHIPS as f64;
                    *en += a.norm_sqr() + b.norm_sqr();
                    *tu += b * a.conj();
                    a = Iq::new(0.0, 0.0);
                    b = Iq::new(0.0, 0.0);
                }
            }
        }
        let best = (0..steps)
            .max_by(|&a, &b| energy[a].total_cmp(&energy[b]))
            .unwrap_or(0);
        let mut sorted = energy.clone();
        sorted.sort_by(f32::total_cmp);
        let median = sorted[steps / 2];
        if median <= 0.0 || energy[best] < ACQ_RATIO * median {
            return false;
        }
        // The halves' centres are half a symbol apart.
        self.freq = 2.0 * turn[best].arg();
        self.code_phase = (best as f64 * ACQ_STEP + self.acq.len() as f64 * self.chips_per_sample)
            .rem_euclid(CHIPS as f64);
        self.set_step();
        self.rot = Iq::new(1.0, 0.0);
        self.p = Iq::new(0.0, 0.0);
        self.e = Iq::new(0.0, 0.0);
        self.l = Iq::new(0.0, 0.0);
        self.last = Iq::new(0.0, 0.0);
        self.since_lock = 0;
        self.dll_moved = 0.0;
        self.dll_symbols = 0;
        self.mean_i = 0.0;
        self.mean_q2 = 0.0;
        self.poor = 0;
        self.locked = true;
        self.framer.reset();
        self.view.message = "Code found, waiting for a frame".into();
        true
    }

    fn set_step(&mut self) {
        let w = self.freq as f64 / self.samples_per_symbol;
        self.step = Iq::new(w.cos() as f32, w.sin() as f32);
    }

    /// Despread `input` until it ends or lock is lost; returns the samples
    /// used.
    fn track(&mut self, input: &[Iq]) -> usize {
        let period = CHIPS as f64;
        for (n, &x) in input.iter().enumerate() {
            let y = x * self.rot.conj();
            self.rot *= self.step;
            let ph = self.code_phase;
            self.p += y * chip(ph);
            self.e += y * chip(ph + 0.5);
            self.l += y * chip(ph - 0.5);
            self.code_phase = ph + self.chips_per_sample;
            if self.code_phase >= period {
                self.code_phase -= period;
                self.symbol();
                if !self.locked {
                    return n + 1;
                }
            }
        }
        input.len()
    }

    /// A symbol's correlators are full: run the loops, read the bit.
    fn symbol(&mut self) {
        let (p, e, l) = (self.p, self.e, self.l);
        self.p = Iq::new(0.0, 0.0);
        self.e = Iq::new(0.0, 0.0);
        self.l = Iq::new(0.0, 0.0);
        self.view.symbols += 1;

        // Code: early minus late. A replica half a chip early correlates
        // better when the signal's code is ahead of ours.
        let (em, lm) = (e.norm(), l.norm());
        if em + lm > 0.0 {
            let d = DLL_GAIN * ((em - lm) / (em + lm)) as f64;
            self.code_phase += d;
            self.dll_moved += d;
            self.dll_symbols += 1;
        }

        // Carrier: frequency from the turn between symbols (squared, to
        // lose the data) while pulling in, then a Costas loop.
        let mag = p.norm().max(1e-20);
        if self.since_lock < FLL_SYMBOLS {
            let d = p * self.last.conj();
            let turn = 0.5 * (2.0 * d.re * d.im).atan2(d.re * d.re - d.im * d.im);
            if self.since_lock > 0 {
                self.freq += FLL_GAIN * turn;
            }
            self.since_lock += 1;
        } else {
            let err = p.im * p.re.signum() / mag;
            self.freq += PLL_KI * err;
            let bump = PLL_KP * err;
            self.rot *= Iq::new(bump.cos(), bump.sin());
        }
        self.last = p;
        self.set_step();
        // Rust note: an f32 phasor multiplied a million times a second
        // drifts off the unit circle; pull it back once a symbol.
        self.rot /= self.rot.norm();

        if self.since_lock < FLL_SYMBOLS {
            return;
        }
        self.mean_i += QUALITY_ALPHA * (p.re.abs() - self.mean_i);
        self.mean_q2 += QUALITY_ALPHA * (p.im * p.im - self.mean_q2);
        if self.scatter.len() == SCATTER {
            self.scatter.remove(0);
        }
        let scale = if self.mean_i > 0.0 {
            1.0 / self.mean_i
        } else {
            0.0
        };
        self.scatter.push(p * scale);

        // Noise alone: |I|'s mean squared is 2/π of Q's power.
        if self.mean_i * self.mean_i < LOST_RATIO * self.mean_q2 {
            self.poor += 1;
            if self.poor > LOST_SYMBOLS {
                self.locked = false;
                self.view.locked = false;
                self.view.carrier_hz = None;
                self.view.message = "Lost the code; looking again".into();
                self.framer.reset();
                return;
            }
        } else {
            self.poor = 0;
        }
        // A one is the preamble's lead; which way up that is, the framer
        // finds.
        self.framer.push((p.re < 0.0) as u8, &mut self.done);
    }

    fn collect(&mut self) {
        let v = &mut self.view;
        v.locked = self.locked;
        if self.locked {
            v.carrier_hz = Some(self.freq as f64 * SYMBOL_RATE / std::f64::consts::TAU);
            // Es/N0 = A² / 2σ², σ² the noise in one rail (Q's power).
            if self.mean_q2 > 0.0 {
                v.esn0_db = 10.0 * (self.mean_i * self.mean_i / (2.0 * self.mean_q2)).log10();
            }
            if self.dll_symbols > 0 {
                v.chip_ppm =
                    (self.dll_moved / (self.dll_symbols as f64 * CHIPS as f64) * 1e6) as f32;
            }
        }
        for f in self.done.drain(..) {
            v.frames += 1;
            v.slipped += f.slipped as u64;
            v.frame_symbols = Some(f.symbols);
            v.layout = f.layout.clone();
            v.message = format!(
                "Frames of {} symbols ({:.2} ms): {}",
                f.symbols,
                f.symbols as f64 / SYMBOL_RATE * 1e3,
                f.layout
            );
            if v.recent.len() == KEEP {
                v.recent.remove(0);
            }
            v.recent.push(f);
        }
    }
}

/// A test signal: `frames` frames of the 1919-symbol layout at `rate`,
/// the carrier `offset_hz` off, with noise for the given Es/N0. Returns
/// the samples and the data blocks sent (three of 512 bits a frame).
pub fn synth(rate: f64, frames: usize, offset_hz: f64, esn0_db: f64) -> (Vec<Iq>, Vec<Vec<u8>>) {
    // A small xorshift generator: the tests need no crate for this.
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut bits: Vec<u8> = Vec::new();
    let mut blocks = Vec::new();
    // A lead-in of random symbols, as the receiver joins mid-frame.
    bits.extend((0..700).map(|_| (next() & 1) as u8));
    for _ in 0..frames + 1 {
        bits.extend(&preamble()[..FIRST_BLOCK_AT]);
        for k in 0..3 {
            if k > 0 {
                bits.extend((0..64).map(|i| ((UNIQUE_WORD >> (63 - i)) & 1) as u8));
            }
            let mut block: Vec<u8> = (0..512).map(|_| (next() & 1) as u8).collect();
            // A block's first bit is zero: in the first block it is the
            // preamble's last symbol.
            block[0] = 0;
            bits.extend(&block);
            blocks.push(block);
        }
        bits.push((next() & 1) as u8);
    }
    let chips_per_sample = CHIP_RATE / rate;
    let n = (bits.len() as f64 * CHIPS as f64 / chips_per_sample) as usize;
    // Symbol energy is rate / SYMBOL_RATE samples of unit power.
    let sigma = ((rate / SYMBOL_RATE) / (2.0 * 10f64.powf(esn0_db / 10.0))).sqrt();
    let mut gauss = move || {
        // Twelve uniforms summed: near enough normal for a test.
        let s: f64 = (0..12)
            .map(|_| (next() >> 11) as f64 / (1u64 << 53) as f64)
            .sum();
        s - 6.0
    };
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let ph = 0.37 + i as f64 * chips_per_sample;
        let sym = (ph / CHIPS as f64) as usize;
        if sym >= bits.len() {
            break;
        }
        let a = CODE[ph as usize % CHIPS] as f64 * if bits[sym] == 1 { -1.0 } else { 1.0 };
        let w = std::f64::consts::TAU * offset_hz * i as f64 / rate + 1.0;
        out.push(Iq::new(
            (a * w.cos() + sigma * gauss()) as f32,
            (a * w.sin() + sigma * gauss()) as f32,
        ));
    }
    (out, blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The preamble as it came off the air (255 symbols, the same in
    /// every frame of both carriers).
    const MEASURED_PREAMBLE: &str = "111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111001111101101110000001010000110110010000011110001011010110000100010011101111010001100110100110001110010101011101010010010";
    /// The unique word as it came off the air.
    const MEASURED_UNIQUE_WORD: &str =
        "0111111000001000011000101001111010001110010010110111011001101010";

    #[test]
    fn preamble_is_the_measured_one() {
        let p: String = preamble().iter().map(|b| char::from(b'0' + b)).collect();
        assert_eq!(p, MEASURED_PREAMBLE);
    }

    #[test]
    fn unique_word_is_the_measured_one() {
        assert_eq!(format!("{UNIQUE_WORD:064b}"), MEASURED_UNIQUE_WORD);
        // 63 bits of b[n] = b[n−5] ⊕ b[n−6], and a zero.
        let b: Vec<u8> = MEASURED_UNIQUE_WORD.bytes().map(|c| c - b'0').collect();
        for n in 6..63 {
            assert_eq!(b[n], b[n - 5] ^ b[n - 6], "bit {n}");
        }
        assert_eq!(b[63], 0);
    }

    fn unpack(block: &(usize, Vec<u8>)) -> Vec<u8> {
        (0..block.0)
            .map(|i| (block.1[i / 8] >> (7 - i % 8)) & 1)
            .collect()
    }

    fn decode(rate: f64, offset_hz: f64, esn0_db: f64) {
        let (x, sent) = synth(rate, 4, offset_hz, esn0_db);
        let mut rx = BarkerRx::new(rate);
        let mut frames = Vec::new();
        // Small uneven pushes, as a live source gives.
        for c in x.chunks(4099) {
            rx.push(c);
            for f in rx.view().recent {
                if !frames.contains(&f) {
                    frames.push(f);
                }
            }
        }
        let v = rx.view();
        assert!(v.locked, "{}", v.message);
        assert_eq!(v.frames, 4, "{}", v.message);
        assert_eq!(v.layout, "254 + 512 + 2×(64 + 512) + 1");
        assert_eq!(v.frame_symbols, Some(1919));
        let got = v.carrier_hz.unwrap();
        assert!((got - offset_hz).abs() < 20.0, "carrier {got} Hz");
        for (k, f) in frames.iter().enumerate() {
            assert_eq!(f.blocks.len(), 3);
            assert!(!f.slipped);
            for (j, b) in f.blocks.iter().enumerate() {
                assert_eq!(unpack(b), sent[3 * k + j], "frame {k} block {j}");
            }
        }
    }

    #[test]
    fn decodes_at_six_samples_a_chip() {
        decode(1e6, 1234.0, 12.0);
    }

    #[test]
    fn decodes_at_an_odd_rate_with_the_carrier_off() {
        decode(437_500.0, -2900.0, 10.0);
    }

    #[test]
    fn noise_does_not_lock() {
        let (mut x, _) = synth(500e3, 1, 0.0, -40.0);
        x.truncate(400_000);
        let mut rx = BarkerRx::new(500e3);
        rx.push(&x);
        assert_eq!(rx.view().frames, 0);
    }

    #[test]
    fn layout_folds_the_longer_frame() {
        let mut items = vec![(false, 576)];
        for _ in 0..7 {
            items.push((true, 192));
            items.push((false, 576));
        }
        assert_eq!(layout(&items), "254 + 576 + 7×(192 + 576) + 1");
    }
}
