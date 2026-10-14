//! apt-X: four-band sub-band ADPCM, one 16-bit codeword for every four
//! samples of a channel — 4:1 on 16-bit audio. Broadcasters' "Standard
//! apt-X" (apt-X 100) feeds studio links and programme distribution: stereo
//! at 32 kHz is 256 kbit/s, mono at 16 kHz 64 kbit/s.
//!
//! A codeword holds the four sub-bands' quantised prediction errors, first
//! sent first, 3 | 2 | 4 | 7 bits: Standard apt-X gives its bands (at 32 kHz
//! sampling) 0–4 kHz 7 bits, 4–8 kHz 4, 8–12 kHz 3 and 12–16 kHz 2 (APT's
//! apt-X technical overview); the 3-bit field drives the QMF slot that comes
//! out at 8–12 kHz. Each sub-band is backward-adaptive ADPCM (an adaptive
//! quantiser step and a pole-zero predictor), and a two-stage QMF tree joins
//! them.
//!
//! The decoder here is a port of FFmpeg's Bluetooth aptX decoder
//! (libavcodec/aptx.c, aptx.h, aptxdec.c; © 2017 Aurelien Jacobs,
//! LGPL-2.1-or-later): its tables, its ADPCM and its QMF, bit-exact against
//! it ([`Mapping::Bluetooth`]). Bluetooth aptX is Enhanced apt-X (its sync is
//! in every codeword). The broadcast stream found on a Comtech IBS carrier is
//! Standard apt-X: its AutoSync is a sync word stolen one bit a codeword,
//! from one channel's 4–8 kHz field, over 10–11 codewords of every 128. It
//! writes its fields differently ([`Mapping::Broadcast`], worked out from
//! the carrier): every bit is inverted, each field is sign and magnitude
//! (sign first), and an all-zero field (all ones as sent) is an exact zero —
//! a zero level where aptX has two half steps. With that the 0–4 kHz band
//! decodes cleanly against aptX's tables (left and right agree to 0.95
//! coherence on a near-mono programme). The upper bands' codes do not fit
//! aptX's step-size steering; Standard apt-X's own tables are not published,
//! so theirs is aptX's moved by a per-band constant fitted to the carrier
//! ([`BROADCAST_BIAS`]): an approximation, though the bands then agree
//! left to right and join one another as a programme's spectrum does.
//!
//! [`AptxRx`] takes the data bits of a link, finds where the codewords
//! start and whether they alternate left and right, and decodes.

/// Sub-bands, lowest first (FFmpeg's LF, MLF, MHF, HF).
const BANDS: usize = 4;
const FILTER_TAPS: usize = 16;

// FFmpeg aptx.c: the standard (non-HD) quantisation tables.
static QI_LF: [i32; 65] = [
    -9948, 9948, 29860, 49808, 69822, 89926, 110144, 130502, 151026, 171738, 192666, 213832,
    235264, 256982, 279014, 301384, 324118, 347244, 370790, 394782, 419250, 444226, 469742, 495832,
    522536, 549890, 577936, 606720, 636290, 666700, 698006, 730270, 763562, 797958, 833538, 870398,
    908640, 948376, 989740, 1032874, 1077948, 1125150, 1174700, 1226850, 1281900, 1340196, 1402156,
    1468282, 1539182, 1615610, 1698514, 1789098, 1888944, 2000168, 2125700, 2269750, 2438670,
    2642660, 2899462, 3243240, 3746078, 4535138, 5664098, 7102424, 8897462,
];
static IQD_LF: [i32; 65] = [
    9948, 9948, 9962, 9988, 10026, 10078, 10142, 10218, 10306, 10408, 10520, 10646, 10784, 10934,
    11098, 11274, 11462, 11664, 11880, 12112, 12358, 12618, 12898, 13194, 13510, 13844, 14202,
    14582, 14988, 15422, 15884, 16380, 16912, 17484, 18098, 18762, 19480, 20258, 21106, 22030,
    23044, 24158, 25390, 26760, 28290, 30008, 31954, 34172, 36728, 39700, 43202, 47382, 52462,
    58762, 66770, 77280, 91642, 112348, 144452, 199326, 303512, 485546, 643414, 794914, 1000124,
];
static QFS_LF: [i16; 65] = [
    0, -21, -19, -17, -15, -12, -10, -8, -6, -4, -1, 1, 3, 6, 8, 10, 13, 15, 18, 20, 23, 26, 29,
    31, 34, 37, 40, 43, 47, 50, 53, 57, 60, 64, 68, 72, 76, 80, 85, 89, 94, 99, 105, 110, 116, 123,
    129, 136, 144, 152, 161, 171, 182, 194, 207, 223, 241, 263, 291, 328, 382, 467, 522, 522, 522,
];
static QI_MLF: [i32; 9] = [
    -89806, 89806, 278502, 494338, 759442, 1113112, 1652322, 2720256, 5190186,
];
static IQD_MLF: [i32; 9] = [
    89806, 89806, 98890, 116946, 148158, 205512, 333698, 734236, 1735696,
];
static QFS_MLF: [i16; 9] = [0, -14, 6, 29, 58, 96, 154, 270, 521];
static QI_MHF: [i32; 3] = [-194080, 194080, 890562];
static IQD_MHF: [i32; 3] = [194080, 194080, 502402];
static QFS_MHF: [i16; 3] = [0, -33, 136];
static QI_HF: [i32; 5] = [-163006, 163006, 542708, 1120554, 2669238];
static IQD_HF: [i32; 5] = [163006, 163006, 216698, 361148, 1187538];
static QFS_HF: [i16; 5] = [0, -8, 33, 95, 262];

/// One sub-band's tables: quantiser intervals, the dither's weight in the
/// inverse quantiser, the step-size steering, its ceiling, and the order of
/// the zero predictor.
struct Tables {
    qi: &'static [i32],
    iqd: &'static [i32],
    qfs: &'static [i16],
    factor_max: i32,
    order: usize,
}

static TABLES: [Tables; BANDS] = [
    Tables {
        qi: &QI_LF,
        iqd: &IQD_LF,
        qfs: &QFS_LF,
        factor_max: 0x11FF,
        order: 24,
    },
    Tables {
        qi: &QI_MLF,
        iqd: &IQD_MLF,
        qfs: &QFS_MLF,
        factor_max: 0x14FF,
        order: 12,
    },
    Tables {
        qi: &QI_MHF,
        iqd: &IQD_MHF,
        qfs: &QFS_MHF,
        factor_max: 0x16FF,
        order: 6,
    },
    Tables {
        qi: &QI_HF,
        iqd: &IQD_HF,
        qfs: &QFS_HF,
        factor_max: 0x15FF,
        order: 12,
    },
];

static QUANT_FACTORS: [i16; 32] = [
    2048, 2093, 2139, 2186, 2233, 2282, 2332, 2383, 2435, 2489, 2543, 2599, 2656, 2714, 2774, 2834,
    2896, 2960, 3025, 3091, 3158, 3228, 3298, 3371, 3444, 3520, 3597, 3676, 3756, 3838, 3922, 4008,
];

// FFmpeg aptx.h: the QMF tree's filters (each pair mirrors the other).
static OUTER: [[i32; FILTER_TAPS]; 2] = [
    [
        730, -413, -9611, 43626, -121026, 269973, -585547, 2801966, 697128, -160481, 27611, 8478,
        -10043, 3511, 688, -897,
    ],
    [
        -897, 688, 3511, -10043, 8478, 27611, -160481, 697128, 2801966, -585547, 269973, -121026,
        43626, -9611, -413, 730,
    ],
];
static INNER: [[i32; FILTER_TAPS]; 2] = [
    [
        1033, -584, -13592, 61697, -171156, 381799, -828088, 3962579, 985888, -226954, 39048,
        11990, -14203, 4966, 973, -1268,
    ],
    [
        -1268, 973, 4966, -14203, 11990, 39048, -226954, 985888, 3962579, -828088, 381799, -171156,
        61697, -13592, -584, 1033,
    ],
];

/// Clip to 24 bits signed (FFmpeg's `av_clip_intp2(x, 23)`).
fn clip23(x: i64) -> i32 {
    x.clamp(-(1 << 23), (1 << 23) - 1) as i32
}

/// Right shift rounding half to even (FFmpeg's `rshift32`/`rshift64`). The
/// C relies on wrapping arithmetic, hence `wrapping_add`.
fn rshift32(v: i32, s: u32) -> i32 {
    let r = 1i32 << (s - 1);
    let mask = (1i32 << (s + 1)) - 1;
    (v.wrapping_add(r) >> s) - i32::from((v & mask) == r)
}
fn rshift64(v: i64, s: u32) -> i64 {
    let r = 1i64 << (s - 1);
    let mask = (1i64 << (s + 1)) - 1;
    (v.wrapping_add(r) >> s) - i64::from((v & mask) == r)
}
fn diffsign(a: i32, b: i32) -> i32 {
    i32::from(a > b) - i32::from(a < b)
}
fn sign_extend(v: u32, bits: u32) -> i32 {
    ((v << (32 - bits)) as i32) >> (32 - bits)
}

/// A delay line for one QMF filter (stored twice over so the taps are
/// always one contiguous slice).
#[derive(Clone, Default)]
struct FilterSignal {
    pos: usize,
    buf: [i32; 2 * FILTER_TAPS],
}

impl FilterSignal {
    fn push(&mut self, s: i32) {
        self.buf[self.pos] = s;
        self.buf[self.pos + FILTER_TAPS] = s;
        self.pos = (self.pos + 1) & (FILTER_TAPS - 1);
    }

    fn conv(&self, c: &[i32; FILTER_TAPS], shift: u32) -> i32 {
        let e: i64 = self.buf[self.pos..self.pos + FILTER_TAPS]
            .iter()
            .zip(c)
            .map(|(&s, &k)| i64::from(s) * i64::from(k))
            .sum();
        clip23(rshift64(e, shift))
    }
}

/// Two sub-bands into two samples (FFmpeg's `aptx_qmf_polyphase_synthesis`).
fn polyphase(
    sig: &mut [FilterSignal; 2],
    c: &[[i32; FILTER_TAPS]; 2],
    shift: u32,
    low: i32,
    high: i32,
) -> [i32; 2] {
    let sb = [low.wrapping_add(high), low.wrapping_sub(high)];
    let mut out = [0; 2];
    for i in 0..2 {
        sig[i].push(sb[1 - i]);
        out[i] = sig[i].conv(&c[i], shift);
    }
    out
}

/// A sub-band's inverse quantiser and predictor state.
#[derive(Clone)]
struct Band {
    // Inverse quantiser.
    quant_factor: i32,
    factor_select: i32,
    rdiff: i32,
    // Predictor.
    prev_sign: [i32; 2],
    s_weight: [i32; 2],
    d_weight: [i32; 24],
    pos: usize,
    rd: [i32; 48],
    prev_rs: i32,
    pred_diff: i32,
    pred_sample: i32,
}

impl Default for Band {
    fn default() -> Self {
        Band {
            quant_factor: 0,
            factor_select: 0,
            rdiff: 0,
            prev_sign: [1, 1],
            s_weight: [0; 2],
            d_weight: [0; 24],
            pos: 0,
            rd: [0; 48],
            prev_rs: 0,
            pred_diff: 0,
            pred_sample: 0,
        }
    }
}

/// A quantised value: FFmpeg's `quantized_sample` (aptX), or a magnitude
/// index and sign with an exact zero (the broadcast stream).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Code {
    Aptx(i32),
    /// Table index (1 for the smallest step) and −1, 0 or +1.
    Index(usize, i32),
}

impl Band {
    /// FFmpeg's `aptx_invert_quantization`: the reconstructed difference,
    /// and the step size steered by the index (its steering moved by
    /// `bias`: 0 for aptX, see [`BROADCAST_BIAS`]).
    fn invert_quantization(&mut self, code: Code, dither: i32, t: &Tables, bias: i32) {
        let (idx, sign) = match code {
            // Rust note: `q ^ -(q < 0)` is the C's one's complement of a
            // negative value: −1 → 0, −2 → 1 …; plus one, the table index.
            Code::Aptx(q) => (
                ((q ^ -i32::from(q < 0)) + 1) as usize,
                if q < 0 { -1 } else { 1 },
            ),
            Code::Index(i, s) => (i, s),
        };
        let mut qr = t.qi[idx] / 2;
        if sign < 0 {
            qr = -qr;
        }
        let qr = if sign == 0 {
            0
        } else {
            clip23(rshift64(
                (i64::from(qr) << 32) + i64::from(dither) * i64::from(t.iqd[idx]),
                32,
            ))
        };
        self.rdiff = ((i64::from(self.quant_factor) * i64::from(qr)) >> 19) as i32;
        let fs = 32620i32.wrapping_mul(self.factor_select);
        let fs = rshift32(
            fs.wrapping_add((i32::from(t.qfs[idx]) + bias) * (1 << 15)),
            15,
        );
        self.factor_select = fs.clamp(0, t.factor_max);
        let i = ((self.factor_select & 0xFF) >> 3) as usize;
        let shift = (t.factor_max - self.factor_select) >> 8;
        self.quant_factor = (i32::from(QUANT_FACTORS[i]) << 11) >> shift;
    }

    /// FFmpeg's `aptx_prediction_filtering`.
    fn prediction_filtering(&mut self, order: usize) {
        let rdiff = self.rdiff;
        let rs = clip23(i64::from(rdiff) + i64::from(self.pred_sample));
        let predictor = clip23(
            (i64::from(self.s_weight[0]) * i64::from(self.prev_rs)
                + i64::from(self.s_weight[1]) * i64::from(rs))
                >> 22,
        );
        self.prev_rs = rs;
        // The reconstructed differences: a ring of `order`, stored twice.
        let p = self.pos;
        self.rd[p] = self.rd[order + p];
        let p = (p + 1) % order;
        self.pos = p;
        self.rd[order + p] = rdiff;
        let b = order + p;
        let srd0 = diffsign(rdiff, 0) * (1 << 23);
        let mut pd = 0i64;
        for i in 0..order {
            let srd = (self.rd[b - i - 1] >> 31) | 1;
            self.d_weight[i] -= rshift32(self.d_weight[i].wrapping_sub(srd.wrapping_mul(srd0)), 8);
            pd += i64::from(self.rd[b - i]) * i64::from(self.d_weight[i]);
        }
        self.pred_diff = clip23(pd >> 22);
        self.pred_sample = clip23(i64::from(predictor) + i64::from(self.pred_diff));
    }

    /// FFmpeg's `aptx_process_subband`: one quantised value in.
    fn process(&mut self, code: Code, dither: i32, t: &Tables, bias: i32) {
        self.invert_quantization(code, dither, t, bias);
        let sign = diffsign(self.rdiff, -self.pred_diff);
        let ss0 = sign * self.prev_sign[0];
        let ss1 = sign * self.prev_sign[1];
        self.prev_sign[0] = self.prev_sign[1];
        self.prev_sign[1] = sign | 1;
        let range = 0x100000;
        let sw1 = rshift32((-ss1).wrapping_mul(self.s_weight[1]), 1);
        let sw1 = (sw1.clamp(-range, range) & !0xF) * 16;
        let range = 0x300000;
        let w0 = 254i32
            .wrapping_mul(self.s_weight[0])
            .wrapping_add(0x800000 * ss0)
            .wrapping_add(sw1);
        self.s_weight[0] = rshift32(w0, 8).clamp(-range, range);
        let range = 0x3C0000 - self.s_weight[0];
        let w1 = 255i32
            .wrapping_mul(self.s_weight[1])
            .wrapping_add(0xC00000 * ss1);
        self.s_weight[1] = rshift32(w1, 8).clamp(-range, range);
        self.prediction_filtering(t.order);
    }
}

/// How a codeword's fields are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mapping {
    /// FFmpeg's aptX: two's complement, the HH field's last bit a parity
    /// (sync) bit, dither added.
    Bluetooth,
    /// The broadcast stream (Standard apt-X): inverted, sign and magnitude,
    /// an exact zero, no dither; the upper bands' step sizes fitted (see the
    /// module notes).
    Broadcast,
}

/// One channel's decoder.
#[derive(Clone, Default)]
pub struct Channel {
    bands: [Band; BANDS],
    history: i32,
    dither_parity: i32,
    dither: [i32; BANDS],
    q: [i32; BANDS],
    outer: [FilterSignal; 2],
    inner: [[FilterSignal; 2]; 2],
}

impl Channel {
    pub fn new() -> Self {
        Self::default()
    }

    /// FFmpeg's `ff_aptx_generate_dither` (aptX only).
    fn generate_dither(&mut self) {
        let cw = (self.q[0] & 3) + ((self.q[1] & 2) << 1) + ((self.q[2] & 1) << 3);
        self.history = (cw << 8).wrapping_add(((self.history as u32) << 4) as i32);
        let m = 5_184_443i64 * i64::from(self.history >> 7);
        let d = ((m * 4) + (m >> 22)) as i32;
        for (sb, v) in self.dither.iter_mut().enumerate() {
            *v = ((d as u32) << (23 - 5 * sb)) as i32;
        }
        self.dither_parity = (d >> 25) & 1;
    }

    /// One codeword in (first-sent bit as the MSB), four samples out
    /// (24-bit).
    pub fn decode(&mut self, word: u16, mapping: Mapping) -> [i32; 4] {
        let w = u32::from(word);
        match mapping {
            Mapping::Bluetooth => {
                self.generate_dither();
                self.q = [
                    sign_extend(w, 7),
                    sign_extend(w >> 7, 4),
                    sign_extend(w >> 11, 2),
                    sign_extend(w >> 13, 3),
                ];
                // The HH field's last bit is the sync bit: the decoder puts
                // the parity it implies in its place (FFmpeg's
                // `aptx_unpack_codeword`).
                let parity = (self.dither_parity ^ self.q.iter().fold(0, |a, &q| a ^ q)) & 1;
                self.q[3] = (self.q[3] & !1) | parity;
                for (sb, band) in self.bands.iter_mut().enumerate() {
                    band.process(Code::Aptx(self.q[sb]), self.dither[sb], &TABLES[sb], 0);
                }
            }
            Mapping::Broadcast => {
                // Every bit is inverted on the line.
                let v = !w & 0xFFFF;
                for (sb, band) in self.bands.iter_mut().enumerate() {
                    let (shift, bits) = BROADCAST_FIELDS[sb];
                    let f = (v >> shift) & ((1 << bits) - 1);
                    // Sign first, then the magnitude.
                    let (neg, m) = (f >> (bits - 1) == 1, (f & ((1 << (bits - 1)) - 1)) as usize);
                    let code = if m == 0 && !neg {
                        // The exact zero, steering as the smallest step.
                        Code::Index(1, 0)
                    } else {
                        // LL: magnitude m is aptX's index m (m ≥ 1); the
                        // upper bands: index m + 1 (see the module notes).
                        let idx = if sb == 0 { m.max(1) } else { m + 1 };
                        Code::Index(idx, if neg { -1 } else { 1 })
                    };
                    band.process(code, 0, &TABLES[sb], BROADCAST_BIAS[sb]);
                }
            }
        }
        self.synth()
    }

    /// FFmpeg's `aptx_qmf_tree_synthesis`.
    fn synth(&mut self) -> [i32; 4] {
        let sbs = [
            self.bands[0].prev_rs,
            self.bands[1].prev_rs,
            self.bands[2].prev_rs,
            self.bands[3].prev_rs,
        ];
        let mut inter = [0i32; 4];
        for i in 0..2 {
            let o = polyphase(&mut self.inner[i], &INNER, 22, sbs[2 * i], sbs[2 * i + 1]);
            inter[2 * i..2 * i + 2].copy_from_slice(&o);
        }
        let mut out = [0i32; 4];
        for i in 0..2 {
            let o = polyphase(&mut self.outer, &OUTER, 21, inter[i], inter[2 + i]);
            out[2 * i..2 * i + 2].copy_from_slice(&o);
        }
        out
    }

    /// The LL band's reconstruction so far (the search judges by it).
    fn ll(&self) -> i32 {
        self.bands[0].prev_rs
    }
}

/// The broadcast stream's fields, per QMF slot (the order of [`TABLES`]):
/// (lowest bit, bits). Standard apt-X gives its bands 7, 4, 3 and 2 bits
/// (0–4, 4–8, 8–12, 12–16 kHz at 32 kHz); slot 3 is 8–12 kHz, the high
/// half's QMF output being frequency-inverted, so the 3-bit field drives it
/// with aptX's 5-entry tables and the 2-bit field slot 2 with the 3-entry.
/// The same layout as aptX's codeword, MSB first: 3 | 2 | 4 | 7.
const BROADCAST_FIELDS: [(u32, u32); BANDS] = [(0, 7), (7, 4), (11, 2), (13, 3)];

/// The broadcast stream's step-size steering, moved from aptX's by this per
/// band. Fitted on the carrier (2026-10-15): aptX's offsets with these added
/// make each upper band's level continuous with the band below across 4, 8
/// and 12 kHz (frame by frame, at the spectral slope below the edge), and
/// at those levels left and right agree as a near-mono programme should
/// (coherence 0.86, 0.80 and 0.50 against 0.2–0.4 off them). Standard
/// apt-X's own tables are not published; these stand in for them.
const BROADCAST_BIAS: [i32; BANDS] = [0, -35, 12, -16];

/// The broadcast stream's samples are this much quieter than its programme
/// (the reconstruction's scale for the stream's codes is not known).
const GAIN: f32 = 2.0;

/// AutoSync: codewords in its frame; frames gathered before judging, and
/// given up after; fixed places in a row that make the word (10 bits; on
/// the carrier 11 places read fixed).
const SYNC_FRAME: usize = 128;
const SYNC_LOOK: u16 = 16;
const SYNC_GIVE_UP: u16 = 64;
const SYNC_MIN_RUN: usize = 8;

/// Codewords looked at per channel when searching, and the data needed.
const SEARCH_WORDS: usize = 4000;
/// LL reconstructions this close to full scale count as clipped.
const CLIP: i32 = (1 << 23) - 2;
/// A channel decodes when its LL band clips this rarely and is this smooth
/// (sample to sample correlation; wrong alignments sit near 0 or clip).
const MAX_CLIP: f64 = 0.005;
const MIN_LAG1: f64 = 0.4;
/// Codewords a channel between checks while decoding, and the share
/// clipped that sends it searching again.
const CHECK_WORDS: u32 = 16_000;
const LOSE_CLIP: f64 = 0.05;

/// One alignment's trial: the LL band of each channel decoded from scratch.
fn trial(bits: &[u8], offset: usize, stereo: bool) -> (f64, f64, f64) {
    let chans = if stereo { 2 } else { 1 };
    let words = ((bits.len() - offset) / 16).min(SEARCH_WORDS * chans);
    let mut dec = [Channel::new(), Channel::new()];
    let mut ll: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
    let mut clipped = 0usize;
    for k in 0..words {
        let w = word_at(bits, offset + 16 * k);
        let c = k % chans;
        dec[c].decode(w, Mapping::Broadcast);
        let v = dec[c].ll();
        clipped += usize::from(v.abs() >= CLIP);
        ll[c].push(f64::from(v));
    }
    let lag1 = |x: &[f64]| {
        let n = x.len().max(1) as f64;
        let m = x.iter().sum::<f64>() / n;
        let (mut s0, mut s1) = (0.0, 0.0);
        for i in 0..x.len() {
            s0 += (x[i] - m) * (x[i] - m);
            if i > 0 {
                s1 += (x[i] - m) * (x[i - 1] - m);
            }
        }
        if s0 > 0.0 { s1 / s0 } else { 0.0 }
    };
    let l = (0..chans)
        .map(|c| lag1(&ll[c]))
        .fold(f64::INFINITY, f64::min);
    let lr = if stereo {
        let n = ll[0].len().min(ll[1].len());
        let (a, b) = (&ll[0][..n], &ll[1][..n]);
        let ma = a.iter().sum::<f64>() / n.max(1) as f64;
        let mb = b.iter().sum::<f64>() / n.max(1) as f64;
        let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
        for i in 0..n {
            sab += (a[i] - ma) * (b[i] - mb);
            saa += (a[i] - ma) * (a[i] - ma);
            sbb += (b[i] - mb) * (b[i] - mb);
        }
        if saa > 0.0 && sbb > 0.0 {
            sab / (saa * sbb).sqrt()
        } else {
            0.0
        }
    } else {
        0.0
    };
    (clipped as f64 / words.max(1) as f64, l, lr)
}

fn word_at(bits: &[u8], at: usize) -> u16 {
    bits[at..at + 16]
        .iter()
        .fold(0u16, |a, &b| (a << 1) | u16::from(b))
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AptxStats {
    pub locked: bool,
    /// Left and right codewords alternate (else one channel).
    pub stereo: bool,
    /// Where the codewords start, in bits from the first data bit (stereo:
    /// the left one's), and how well the LL band decoded when found: its
    /// sample-to-sample correlation and (stereo) left/right correlation.
    pub offset: usize,
    pub lag1: f64,
    pub lr: f64,
    pub codewords: u64,
    /// Searches made, and alignments lost.
    pub searches: u64,
    pub losses: u64,
    /// Share of LL reconstructions clipped over the last check.
    pub clipped: f64,
    /// AutoSync: Standard apt-X's sync word, one bit in each of 10–11
    /// codewords of every 128, stolen from one channel's LMF bit: where in
    /// its frame it starts, once seen. That channel is played as the left.
    pub autosync: Option<usize>,
    /// The stream's second channel carried the AutoSync, so the two are
    /// played swapped.
    pub swapped: bool,
}

/// Finds apt-X in a link's data and decodes it; see the module notes.
#[derive(Default)]
pub struct AptxRx {
    bits: Vec<u8>,
    dec: [Channel; 2],
    /// The channel the next codeword is for, and the left channel's four
    /// samples waiting for the right's.
    next: usize,
    left: [i32; 4],
    check_words: u32,
    check_clipped: u32,
    /// AutoSync search: per channel, ones seen in the LMF bit at each place
    /// of a 128-codeword frame, frames seen, and the next codeword's place.
    sync_ones: Vec<[u16; SYNC_FRAME]>,
    sync_frames: u16,
    sync_at: usize,
    /// Decoded samples, interleaved when stereo, ±1 full scale.
    pub pcm: Vec<f32>,
    pub stats: AptxStats,
}

impl AptxRx {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn channels(&self) -> u16 {
        if self.stats.stereo { 2 } else { 1 }
    }

    /// The sample rate for a data rate: four samples a codeword a channel.
    pub fn sample_rate(&self, data_bps: f64) -> f64 {
        data_bps / 16.0 * 4.0 / f64::from(self.channels())
    }

    /// Data octets in (MSB first sent first); samples to `pcm`.
    pub fn push(&mut self, data: &[u8]) {
        self.bits.extend(
            data.iter()
                .flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1)),
        );
        if !self.stats.locked {
            if self.bits.len() < 2 * 16 * SEARCH_WORDS + 64 {
                return;
            }
            self.search();
            if !self.stats.locked {
                return;
            }
        }
        let chans = usize::from(self.channels());
        let words = self.bits.len() / 16;
        for k in 0..words {
            let w = word_at(&self.bits, 16 * k);
            let c = self.next;
            self.watch_autosync(c, chans, w);
            let s = self.dec[c].decode(w, Mapping::Broadcast);
            self.check_clipped += u32::from(self.dec[c].ll().abs() >= CLIP);
            self.check_words += 1;
            self.next = (self.next + 1) % chans;
            if chans == 1 {
                self.pcm.extend(s.iter().map(|&v| sample(v)));
            } else if c == 1 {
                // The second channel's four samples go with the first's
                // (swapped when the second carries the AutoSync).
                let (l, r) = if self.stats.swapped {
                    (s, self.left)
                } else {
                    (self.left, s)
                };
                for i in 0..4 {
                    self.pcm.push(sample(l[i]));
                    self.pcm.push(sample(r[i]));
                }
            } else {
                self.left = s;
            }
        }
        self.stats.codewords += words as u64;
        self.bits.drain(..16 * words);
        if self.check_words >= CHECK_WORDS {
            self.stats.clipped = f64::from(self.check_clipped) / f64::from(self.check_words);
            self.check_words = 0;
            self.check_clipped = 0;
            if self.stats.clipped > LOSE_CLIP {
                self.stats.locked = false;
                self.stats.losses += 1;
            }
        }
    }

    /// Gather the LMF bit of each channel's codewords by their place in a
    /// 128-codeword frame; after enough frames, the channel with a run of
    /// places that never change carries the AutoSync word.
    fn watch_autosync(&mut self, c: usize, chans: usize, w: u16) {
        if self.stats.autosync.is_some() || self.sync_frames >= SYNC_GIVE_UP {
            return;
        }
        if self.sync_ones.len() != chans {
            self.sync_ones = vec![[0; SYNC_FRAME]; chans];
            self.sync_frames = 0;
            self.sync_at = 0;
        }
        // The LMF field's last bit (bit 7 as sent).
        self.sync_ones[c][self.sync_at] += (w >> 7) & 1;
        if c + 1 < chans {
            return;
        }
        self.sync_at += 1;
        if self.sync_at < SYNC_FRAME {
            return;
        }
        self.sync_at = 0;
        self.sync_frames += 1;
        if self.sync_frames < SYNC_LOOK {
            return;
        }
        let n = self.sync_frames;
        for (ch, ones) in self.sync_ones.iter().enumerate() {
            // The longest run of places (round the frame) always 0 or 1.
            let fixed = |p: usize| ones[p % SYNC_FRAME] == 0 || ones[p % SYNC_FRAME] == n;
            let mut best = (0, 0);
            for start in 0..SYNC_FRAME {
                if fixed(start + SYNC_FRAME - 1) || !fixed(start) {
                    continue;
                }
                let len = (start..start + SYNC_FRAME)
                    .take_while(|&p| fixed(p))
                    .count();
                if len > best.0 {
                    best = (len, start);
                }
            }
            if best.0 >= SYNC_MIN_RUN {
                self.stats.autosync = Some(best.1);
                self.stats.swapped = ch == 1;
                return;
            }
        }
    }

    /// Try every start (and left/right pairing) on the bits held.
    fn search(&mut self) {
        self.stats.searches += 1;
        let mut best: Option<(f64, usize, bool, f64, f64)> = None;
        for stereo in [true, false] {
            for offset in 0..if stereo { 32 } else { 16 } {
                let (clip, lag1, lr) = trial(&self.bits, offset, stereo);
                if clip > MAX_CLIP || lag1 < MIN_LAG1 {
                    continue;
                }
                // Stereo pairs both decoding is the stronger evidence.
                let score = lag1 + lr.max(0.0) + if stereo { 0.5 } else { 0.0 };
                if best.is_none_or(|b| score > b.0) {
                    best = Some((score, offset, stereo, lag1, lr));
                }
            }
        }
        match best {
            Some((_, offset, stereo, lag1, lr)) => {
                self.stats.locked = true;
                self.stats.stereo = stereo;
                self.stats.offset = offset;
                self.stats.lag1 = lag1;
                self.stats.lr = lr;
                self.dec = [Channel::new(), Channel::new()];
                self.next = 0;
                self.sync_ones.clear();
                self.stats.autosync = None;
                self.stats.swapped = false;
                self.bits.drain(..offset);
            }
            None => {
                // Keep the latest half, to try again with more.
                let keep = self.bits.len() / 2;
                self.bits.drain(..self.bits.len() - keep);
            }
        }
    }
}

fn sample(v: i32) -> f32 {
    (v as f32 / (1 << 23) as f32 * GAIN).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FFmpeg's aptX decoder on a stream its encoder made (a 1 kHz tone plus
    /// noise, stereo, 32 kHz): the first eight codeword pairs and the
    /// samples it decoded from them (as 16-bit).
    #[test]
    fn bluetooth_mapping_is_ffmpegs() {
        let (words, want) = ffmpeg_vector();
        let mut ch = [Channel::new(), Channel::new()];
        let mut got = Vec::new();
        for pair in words.chunks(2) {
            let l = ch[0].decode(pair[0], Mapping::Bluetooth);
            let r = ch[1].decode(pair[1], Mapping::Bluetooth);
            for i in 0..4 {
                got.push((l[i] >> 8) as i16);
                got.push((r[i] >> 8) as i16);
            }
        }
        assert_eq!(got, want);
    }

    fn ffmpeg_vector() -> (Vec<u16>, Vec<i16>) {
        (FFMPEG_WORDS.to_vec(), FFMPEG_PCM.to_vec())
    }

    // Made with `ffmpeg -f lavfi -i "sine=1000:sample_rate=32000"
    // … -c:a aptx -f aptx` and decoded with `ffmpeg -f aptx`.
    include!("aptx_vector.in");

    /// A broadcast-mapped encoder of the LL band alone, for tests: each
    /// codeword's LL code is the one whose reconstruction lands nearest the
    /// target (analysis by synthesis through the decoder's own state), the
    /// other bands' fields their zero code.
    fn encode_ll(target: &[f64], ch: &mut Channel) -> Vec<u16> {
        let mut out = Vec::new();
        for &x in target {
            let mut best = (f64::INFINITY, 0u16);
            for f in 0..128u16 {
                // f is the LL field as meant (sign first), the other fields
                // zero; every bit is inverted on the line.
                let word = !f;
                let mut c = ch.clone();
                c.decode(word, Mapping::Broadcast);
                let e = (f64::from(c.ll()) - x).abs();
                if e < best.0 {
                    best = (e, word);
                }
            }
            ch.decode(best.1, Mapping::Broadcast);
            out.push(best.1);
        }
        out
    }

    #[test]
    fn finds_broadcast_stereo_at_any_offset() {
        // 440 Hz (left) and 300 Hz + 440 Hz (right) in the LL band, at its
        // 8 kHz rate, a third of full scale.
        let n = 2 * SEARCH_WORDS + 500;
        let a = 0.3 * f64::from(1 << 23);
        let tone = |f: f64, k: usize| (2.0 * std::f64::consts::PI * f * k as f64 / 8000.0).sin();
        let left: Vec<f64> = (0..n).map(|k| a * tone(440.0, k)).collect();
        let right: Vec<f64> = (0..n)
            .map(|k| a * (0.6 * tone(440.0, k) + 0.4 * tone(300.0, k)))
            .collect();
        let (mut el, mut er) = (Channel::new(), Channel::new());
        let wl = encode_ll(&left, &mut el);
        let wr = encode_ll(&right, &mut er);
        // Interleaved, then 11 stray bits ahead of the first codeword. The
        // second channel carries AutoSync in its LMF bit (bit 7 as sent),
        // as the carrier's first channel does, 16 codewords into each 128.
        // Elsewhere that bit changes (on the carrier it is the LMF band's
        // least significant): here at random.
        const SYNC: [u16; 11] = [0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1];
        let mut seed = 0x1234_5678u32;
        let mut coin = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 31) as u16
        };
        let mut bits: Vec<u8> = vec![1, 0, 1, 1, 0, 0, 1, 0, 1, 1, 1];
        for (k, (l, r)) in wl.iter().zip(&wr).enumerate() {
            let p = k % SYNC_FRAME;
            let l = (l & !0x80) | (coin() << 7);
            let r = if (16..27).contains(&p) {
                (r & !0x80) | (SYNC[p - 16] << 7)
            } else {
                (r & !0x80) | (coin() << 7)
            };
            for w in [l, r] {
                bits.extend((0..16).rev().map(|i| ((w >> i) & 1) as u8));
            }
        }
        while !bits.len().is_multiple_of(8) {
            bits.push(1);
        }
        let octets: Vec<u8> = bits
            .chunks(8)
            .map(|c| c.iter().fold(0, |a, &b| (a << 1) | b))
            .collect();
        let mut rx = AptxRx::new();
        for c in octets.chunks(1000) {
            rx.push(c);
        }
        assert!(rx.stats.locked, "{:?}", rx.stats);
        assert!(rx.stats.stereo);
        assert_eq!(rx.stats.offset, 11);
        assert_eq!(rx.sample_rate(256_000.0), 32_000.0);
        // The AutoSync puts the second channel first: the left played is
        // the one with 300 Hz in it.
        assert_eq!(rx.stats.autosync, Some(16), "{:?}", rx.stats);
        assert!(rx.stats.swapped);
        // 440 Hz dominates the left channel at 32 kHz, and 300 Hz is there.
        let l: Vec<f32> = rx.pcm.iter().step_by(2).copied().collect();
        let power = |f: f64| {
            let (mut re, mut im) = (0.0, 0.0);
            for (k, &x) in l.iter().enumerate().skip(4000) {
                let ph = 2.0 * std::f64::consts::PI * f * k as f64 / 32_000.0;
                re += f64::from(x) * ph.cos();
                im += f64::from(x) * ph.sin();
            }
            re * re + im * im
        };
        assert!(
            power(440.0) > 100.0 * power(1000.0),
            "{} {}",
            power(440.0),
            power(1000.0)
        );
        assert!(
            power(300.0) > 10.0 * power(1000.0),
            "{} {}",
            power(300.0),
            power(1000.0)
        );
    }
}
