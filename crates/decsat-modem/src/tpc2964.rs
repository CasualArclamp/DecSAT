// made by CasualArclamp
//! `tpc_2964`: the rate-3/4 turbo product code of Intelsat IESS-315 turbo
//! modems, as the CTCOM RCV-20x manual describes it (Table 3.2): frames of
//! 2964 bits — a 20-bit unique word F50B8h and a 2944-bit (64,57) × (46,39)
//! product codeword carrying 2223 data bits, no CRC — and a (2, 3, 9, 12)
//! descrambler with preset 475h. BPSK, QPSK and 8PSK are handled here.
//!
//! On 8PSK the frame's bits simply run on, three to a symbol in the
//! manual's Figure 3.1 labelling, so a frame of 988 symbols need not start
//! on a symbol's first bit (a live Comtech 238.96 kS/s carrier's UW starts
//! on the second): the UW is looked for at every bit.
//!
//! IESS-315 sets the carrier's performance and leaves the code to
//! "compatible turbo modems", so what the manual does not say — how the
//! block is laid out on the air, which primitive polynomial generates the
//! Hamming codes, whether and how the code bits are scrambled — is found
//! from the signal: every combination is tried on a few frames and judged by
//! the product code's own parity checks. Under the right one most rows *and*
//! most columns are codewords (all of them, less channel errors); under any
//! other about one in 128 is. Each data bit comes out in the order it was
//! sent. What was found is reported, so a guess never passes for knowledge.
//! What the data carry, and any scrambling of the data, is
//! [`payload`](crate::payload)'s business.
//!
//! The receiver, [`TpcRx`], also takes Comtech's TPC 7/8 ([`crate::tpc78`]):
//! it looks for either code's unique word and keeps to the one it finds.

use decsat_core::{Iq, Modulation};

use crate::payload::{SelfSyncScrambler, hdlc_frame};
use crate::tpc::{ExtHamming, ProductCode};
use crate::tpc78;

/// The turbo product codes [`TpcRx`] receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// IESS-315's rate 3/4: 2964-bit frames, structure found blind.
    Tpc2964,
    /// Comtech's rate 7/8: 16 320-bit frames ([`crate::tpc78`]).
    Comtech78,
}

impl Code {
    pub const ALL: [Code; 2] = [Code::Tpc2964, Code::Comtech78];

    /// `as usize` on a field-less enum gives its declaration index (0, 1).
    fn index(self) -> usize {
        self as usize
    }

    pub fn uw(self) -> u32 {
        match self {
            Code::Tpc2964 => UW,
            Code::Comtech78 => tpc78::UW,
        }
    }

    pub fn uw_len(self) -> usize {
        match self {
            Code::Tpc2964 => UW_LEN,
            Code::Comtech78 => tpc78::UW_LEN,
        }
    }

    /// Bits a frame, UW included.
    pub fn frame(self) -> usize {
        match self {
            Code::Tpc2964 => FRAME,
            Code::Comtech78 => tpc78::FRAME,
        }
    }

    /// The user's data bits a frame (what [`TpcRx`] puts out).
    pub fn data(self) -> usize {
        match self {
            Code::Tpc2964 => DATA,
            Code::Comtech78 => tpc78::DATA,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Code::Tpc2964 => "TPC 2964 (IESS-315, rate 3/4)",
            Code::Comtech78 => "Comtech TPC 7/8 (16K block)",
        }
    }
}

/// The unique word, 20 bits, sent MSB first (RCV-20x Table 3.2).
pub const UW: u32 = 0xF_50B8;
pub const UW_LEN: usize = 20;
/// The product codeword: (64,57) × (46,39).
pub const BLOCK: usize = 64 * 46;
pub const FRAME: usize = UW_LEN + BLOCK;
/// Data bits per frame.
pub const DATA: usize = 57 * 39;

/// The degree-6 primitive polynomials: the (63,57) Hamming generator is one
/// of them (bit i = x^i).
pub const POLYS: [u32; 6] = [0x43, 0x61, 0x67, 0x73, 0x5B, 0x6D];

/// The RCV-20x's scrambler polynomial: 1 + x² + x³ + x⁹ + x¹² (taps 2, 3,
/// 9, 12), preset 475h.
pub const TAPS: [u32; 4] = [2, 3, 9, 12];
pub const PRESET: u16 = 0x475;

/// The additive (2, 3, 9, 12) / 475h sequence, wired one of four ways: a
/// 12-stage register with feedback from stages 2, 3, 9 and 12 into stage 1,
/// its output either that feedback bit or stage 12 (`last`), the preset
/// loaded MSB into stage 1 or (`reversed`) into stage 12. Restarted every
/// frame; the first `n` bits.
pub fn additive_sequence(last: bool, reversed: bool, n: usize) -> Vec<u8> {
    // Stages 1..=12 in bits 0..=11.
    let mut s: u16 = if reversed {
        PRESET
    } else {
        (0..12).fold(0, |a, i| a | (((PRESET >> (11 - i)) & 1) << i))
    };
    let stage = |s: u16, k: u32| (s >> (k - 1)) & 1;
    (0..n)
        .map(|_| {
            let fb = TAPS.iter().fold(0, |a, &k| a ^ stage(s, k));
            let out = if last { stage(s, 12) } else { fb };
            s = ((s << 1) | fb) & 0x0FFF;
            out as u8
        })
        .collect()
}

/// The additive sequence's period: 1 + x² + x³ + x⁹ + x¹² is primitive.
pub const PERIOD: usize = 4095;

/// [`additive_sequence`] from `skip` bits in: `n` bits.
pub fn additive_sequence_from(last: bool, reversed: bool, skip: usize, n: usize) -> Vec<u8> {
    // `split_off(at)` cuts a Vec in two and returns the part from `at` on.
    additive_sequence(last, reversed, skip + n).split_off(skip)
}

/// How the code bits might be scrambled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodeScrambler {
    /// Not at all (the data may still be).
    None,
    /// The additive sequence, from the first bit after the UW or (`from_uw`)
    /// counting the UW's 20 bits.
    Additive {
        last: bool,
        reversed: bool,
        from_uw: bool,
    },
}

impl CodeScrambler {
    pub fn all() -> Vec<CodeScrambler> {
        let mut v = vec![CodeScrambler::None];
        for last in [false, true] {
            for reversed in [false, true] {
                for from_uw in [false, true] {
                    v.push(CodeScrambler::Additive {
                        last,
                        reversed,
                        from_uw,
                    });
                }
            }
        }
        v
    }

    /// The sequence over the block's bits as sent.
    pub fn sequence(self) -> Vec<u8> {
        match self {
            CodeScrambler::None => vec![0; BLOCK],
            CodeScrambler::Additive {
                last,
                reversed,
                from_uw,
            } => {
                let skip = if from_uw { UW_LEN } else { 0 };
                additive_sequence(last, reversed, BLOCK + skip)[skip..].to_vec()
            }
        }
    }

    pub fn describe(self) -> String {
        match self {
            CodeScrambler::None => "code bits not scrambled".into(),
            CodeScrambler::Additive {
                last,
                reversed,
                from_uw,
            } => format!(
                "code bits scrambled: (2,3,9,12)/475h additive, output {}, preset {}, from {}",
                if last { "stage 12" } else { "feedback" },
                if reversed { "reversed" } else { "as written" },
                if from_uw { "the UW" } else { "the block" }
            ),
        }
    }
}

/// What the manual leaves open about a frame. The block is 46 rows of
/// (64,57) codewords whose columns are (46,39) codewords; which way round it
/// goes on the air is open (a block sent column by column is the same thing
/// as its transpose — 64 rows of (46,39) words — sent row by row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Structure {
    /// Sent row by row (a (64,57) word at a time), else column by column (a
    /// (46,39) word at a time).
    pub row_wise: bool,
    /// Rows sent last first (the parity rows first).
    pub flip_rows: bool,
    /// Columns sent last first: each row back to front (its parity first).
    pub flip_cols: bool,
    /// The Hamming generator of both component codes.
    pub poly: u32,
    pub scrambler: CodeScrambler,
}

/// The block's shape: rows, and columns (bits per row).
const ROWS: usize = 46;
const COLS: usize = 64;

impl Structure {
    /// The structure test signals use: row by row, data first, x⁶ + x + 1,
    /// code bits not scrambled (the data are, see [`TpcHdlcTx`]).
    pub const TEST: Structure = Structure {
        row_wise: true,
        flip_rows: false,
        flip_cols: false,
        poly: 0x43,
        scrambler: CodeScrambler::None,
    };

    pub fn all() -> Vec<Structure> {
        let mut v = Vec::new();
        for row_wise in [true, false] {
            for flip_rows in [false, true] {
                for flip_cols in [false, true] {
                    for poly in POLYS {
                        for scrambler in CodeScrambler::all() {
                            v.push(Structure {
                                row_wise,
                                flip_rows,
                                flip_cols,
                                poly,
                                scrambler,
                            });
                        }
                    }
                }
            }
        }
        v
    }

    pub fn code(&self) -> ProductCode {
        ProductCode::new(
            ExtHamming::new(6, self.poly, 0),  // (64,57)
            ExtHamming::new(6, self.poly, 18), // (46,39)
        )
    }

    /// For each block bit in the product code's order (row by row), where
    /// on the air it goes.
    fn air_positions(&self) -> Vec<usize> {
        let mut v = Vec::with_capacity(BLOCK);
        for r in 0..ROWS {
            for c in 0..COLS {
                let rr = if self.flip_rows { ROWS - 1 - r } else { r };
                let cc = if self.flip_cols { COLS - 1 - c } else { c };
                v.push(if self.row_wise {
                    rr * COLS + cc
                } else {
                    cc * ROWS + rr
                });
            }
        }
        v
    }

    /// The data bits (indices into the product code's data, row by row) in
    /// the order they go on the air.
    fn data_order(&self, air: &[usize]) -> Vec<usize> {
        const RK: usize = 57;
        let mut v: Vec<usize> = (0..DATA).collect();
        // `sort_by_key` sorts by what the closure returns for each element.
        v.sort_by_key(|&i| air[(i / RK) * COLS + i % RK]);
        v
    }

    pub fn describe(&self) -> String {
        format!(
            "sent {}{}{}, generator {:#04x}, {}",
            if self.row_wise {
                "row by row ((64,57) words)"
            } else {
                "column by column ((46,39) words)"
            },
            if self.flip_rows {
                ", last row first"
            } else {
                ""
            },
            if self.flip_cols {
                ", last column first"
            } else {
                ""
            },
            self.poly,
            self.scrambler.describe()
        )
    }
}

/// A [`Structure`] made ready to use (or the TPC 7/8's fixed one).
struct Layout {
    code: ProductCode,
    /// For each product code bit, where in the block it was sent
    /// ([`NOT_SENT`] if shortened away).
    air: Vec<usize>,
    seq: Vec<u8>,
    /// The user's data bits (indices into the product code's data, row by
    /// row) in the order they go on the air.
    data_order: Vec<usize>,
}

/// A shortened bit's place in [`Layout::air`]: not sent, known to be 0.
const NOT_SENT: usize = usize::MAX;

/// A shortened bit's soft value, over the block's mean magnitude: sure.
const SURE: f32 = 8.0;

impl Layout {
    fn new(st: Structure) -> Self {
        let air = st.air_positions();
        Layout {
            code: st.code(),
            seq: st.scrambler.sequence(),
            data_order: st.data_order(&air),
            air,
        }
    }

    /// Comtech TPC 7/8: row by row from the first row's 97th bit, code
    /// bits not scrambled, the user's data all but the last 24 data bits.
    fn comtech78() -> Self {
        Layout {
            code: tpc78::code(),
            // `checked_sub` is None below 0: the shortened bits.
            air: (0..tpc78::SIDE * tpc78::SIDE)
                .map(|i| i.checked_sub(tpc78::SHORTENED).unwrap_or(NOT_SENT))
                .collect(),
            seq: vec![0; tpc78::BLOCK],
            data_order: (tpc78::SHORTENED..tpc78::SHORTENED + tpc78::DATA).collect(),
        }
    }

    /// One block's soft bits as sent, descrambled, in the product code's
    /// order.
    fn arrange(&self, block: &[f32]) -> Vec<f32> {
        let sure = SURE * block.iter().map(|v| v.abs()).sum::<f32>() / block.len().max(1) as f32;
        self.air
            .iter()
            .map(|&p| {
                if p == NOT_SENT {
                    sure
                } else if self.seq[p] == 0 {
                    block[p]
                } else {
                    -block[p]
                }
            })
            .collect()
    }
}

/// How well each structure fits some frames (hard bits as sent): the
/// fraction of rows that are codewords and the fraction of columns, the
/// lesser of the two, so a structure right about the rows but wrong about
/// the columns does not pass.
fn fits(all: &[Structure], frames: &[Vec<u8>]) -> Vec<f32> {
    // `iter().map(..).collect()` builds a Vec from an iterator: here one
    // scrambling sequence per wiring, and the codes once per generator.
    let seqs: Vec<(CodeScrambler, Vec<u8>)> = CodeScrambler::all()
        .into_iter()
        .map(|s| (s, s.sequence()))
        .collect();
    let codes: Vec<(u32, ProductCode)> = POLYS
        .iter()
        .map(|&poly| {
            let st = Structure {
                row_wise: true,
                flip_rows: false,
                flip_cols: false,
                poly,
                scrambler: CodeScrambler::None,
            };
            (poly, st.code())
        })
        .collect();
    let mut block = vec![0u8; BLOCK];
    let mut tmp = vec![0u8; COLS];
    let mut line = vec![0u8; ROWS];
    all.iter()
        .map(|st| {
            let seq = &seqs.iter().find(|s| s.0 == st.scrambler).unwrap().1;
            let code = &codes.iter().find(|c| c.0 == st.poly).unwrap().1;
            let air = st.air_positions();
            let (mut good_r, mut good_c) = (0usize, 0usize);
            for f in frames {
                for (i, &p) in air.iter().enumerate() {
                    block[i] = f[p] ^ seq[p];
                }
                for r in 0..ROWS {
                    let w = &block[r * COLS..(r + 1) * COLS];
                    good_r += (code.row.decode_hard(w, &mut tmp) && tmp == *w) as usize;
                }
                for c in 0..COLS {
                    for r in 0..ROWS {
                        line[r] = block[r * COLS + c];
                    }
                    good_c += (code.col.decode_hard(&line, &mut tmp[..ROWS])
                        && tmp[..ROWS] == line[..]) as usize;
                }
            }
            let n = frames.len().max(1);
            (good_r as f32 / (ROWS * n) as f32).min(good_c as f32 / (COLS * n) as f32)
        })
        .collect()
}

/// tpc_2964 transmitter, for tests and test signals: 2223 data bits in (in
/// the order they go on the air), 2964 frame bits out (0/1).
pub struct TpcTx {
    lay: Layout,
}

impl TpcTx {
    pub fn new(st: Structure) -> Self {
        TpcTx {
            lay: Layout::new(st),
        }
    }

    pub fn frame(&self, data: &[u8], out: &mut Vec<u8>) {
        assert_eq!(data.len(), DATA);
        out.extend((0..UW_LEN).map(|i| ((UW >> (UW_LEN - 1 - i)) & 1) as u8));
        let mut ordered = vec![0u8; DATA];
        for (i, &d) in self.lay.data_order.iter().enumerate() {
            ordered[d] = data[i];
        }
        let block = self.lay.code.encode(&ordered);
        let mut sent = vec![0u8; BLOCK];
        for (i, &p) in self.lay.air.iter().enumerate() {
            sent[p] = block[i] ^ self.lay.seq[p];
        }
        out.extend_from_slice(&sent);
    }
}

/// A TPC 2964 carrier's data side, for tests and test signals: HDLC frames
/// (FCS-16, flags between, flags when idle) through the RCV-20x polynomial
/// as a self-synchronising scrambler, into frames of a [`Structure`].
pub struct TpcHdlcTx {
    tx: TpcTx,
    scrambler: SelfSyncScrambler,
    /// HDLC bits waiting to go.
    pending: std::collections::VecDeque<u8>,
}

impl TpcHdlcTx {
    pub fn new(st: Structure) -> Self {
        TpcHdlcTx {
            tx: TpcTx::new(st),
            scrambler: SelfSyncScrambler::new(&[2, 3, 9, 12]),
            pending: Default::default(),
        }
    }

    /// Queue an HDLC frame's contents (address, control, protocol and
    /// information: the FCS is added).
    pub fn send(&mut self, frame: &[u8]) {
        let mut bits = Vec::new();
        hdlc_frame(frame, &mut bits);
        self.pending.extend(bits);
    }

    /// Data bits queued and not yet sent.
    pub fn backlog(&self) -> usize {
        self.pending.len()
    }

    /// The next frame's 2964 bits (0/1).
    pub fn frame(&mut self, out: &mut Vec<u8>) {
        let mut data: Vec<u8> = Vec::with_capacity(DATA);
        while data.len() < DATA {
            match self.pending.pop_front() {
                Some(b) => data.push(b),
                // Idle: flags.
                None => self.pending.extend([0, 1, 1, 1, 1, 1, 1, 0]),
            }
        }
        self.scrambler.scramble(&mut data);
        self.tx.frame(&data, out);
    }
}

/// Which code's unique word carrier-locked `symbols` carry, frame after
/// frame (under any orientation): tpc_2964's every 2964 bits, or the
/// Comtech TPC 7/8's every 16 320. For Identify.
pub fn detect(symbols: &[Iq], modulation: Modulation) -> Option<Code> {
    let mut rx = TpcRx::new(modulation);
    // Look all through the listen, a few short frames at a time: the loop
    // may slip somewhere in it.
    let per = rx.per();
    symbols.chunks(FRAME * 4 / per).find_map(|c| {
        rx.add(c);
        if rx.find_uw() { rx.code } else { None }
    })
}

/// How the frame's bits ride the symbols, as the RCV-20x manual's Figure
/// 3.1 labels them: BPSK 0 → +1; QPSK Gray, I then Q, 0 → +; 8PSK three
/// bits a symbol, most significant first, Gray round the circle — label
/// k ⊕ (k >> 1) on point k counter-clockwise (000, 001, 011, 010, 110, …).
/// The points sit at k × 45° here, where the carrier loop leaves them; the
/// figure's 22.5° start is one of the orientations the UW resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mapper {
    Bpsk,
    Qpsk,
    Psk8,
}

impl Mapper {
    /// Anything but BPSK and 8PSK is taken as QPSK (16QAM, which the
    /// manual lists too, is not handled).
    fn of(m: Modulation) -> Self {
        match m {
            Modulation::Bpsk | Modulation::Pi2Bpsk => Mapper::Bpsk,
            Modulation::Psk8 => Mapper::Psk8,
            _ => Mapper::Qpsk,
        }
    }

    fn bits(self) -> usize {
        match self {
            Mapper::Bpsk => 1,
            Mapper::Qpsk => 2,
            Mapper::Psk8 => 3,
        }
    }
}

/// Bits a symbol carries when tpc_2964 is sent on `m`.
pub fn bits_per_symbol(m: Modulation) -> usize {
    Mapper::of(m).bits()
}

/// 8PSK point k at k × 45°: (cos, sin).
const PSK8: [(f32, f32); 8] = {
    const R: f32 = std::f32::consts::FRAC_1_SQRT_2;
    [
        (1.0, 0.0),
        (R, R),
        (0.0, 1.0),
        (-R, R),
        (-1.0, 0.0),
        (-R, -R),
        (0.0, -1.0),
        (R, -R),
    ]
};

/// Map bits to symbols on `m` (see [`Mapper`]), unit power.
pub fn modulate(bits: &[u8], m: Modulation, out: &mut Vec<Iq>) {
    let pm = |b: u8| if b == 0 { 1.0f32 } else { -1.0 };
    match Mapper::of(m) {
        Mapper::Bpsk => out.extend(bits.iter().map(|&b| Iq::new(pm(b), 0.0))),
        Mapper::Qpsk => {
            let k = std::f32::consts::FRAC_1_SQRT_2;
            for p in bits.as_chunks::<2>().0 {
                out.push(Iq::new(pm(p[0]) * k, pm(p[1]) * k));
            }
        }
        Mapper::Psk8 => {
            for t in bits.as_chunks::<3>().0 {
                let label = usize::from(t[0] << 2 | t[1] << 1 | t[2]);
                // The point whose Gray label this is.
                let k = (0..8).find(|&k| k ^ (k >> 1) == label).unwrap();
                out.push(Iq::new(PSK8[k].0, PSK8[k].1));
            }
        }
    }
}

/// What the receiver has found and done.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TpcStats {
    /// The code whose unique word was found.
    pub code: Option<Code>,
    /// The unique word is being found every frame.
    pub uw_locked: bool,
    /// The carrier's phase ambiguity, once the UW resolved it.
    pub orientation: Option<String>,
    /// The frame structure identified.
    pub structure: Option<String>,
    /// How well it fitted (rows and columns that were codewords as
    /// received; channel errors make it less than 1), or the best fit so far
    /// while searching.
    pub fit: f32,
    pub frames: u64,
    /// Frames whose decoding converged: every row and column a codeword.
    pub decoded: u64,
    pub failed: u64,
    /// Data bits the decoder changed, and data bits, in frames that
    /// decoded: a channel bit error estimate.
    pub corrected_bits: u64,
    pub data_bits: u64,
    pub uw_misses: u64,
    /// Carrier phase or symbol slips repaired.
    pub slips: u64,
    /// TPC 7/8: the last decoded frame's 24 overhead bits.
    pub overhead: Option<u32>,
}

impl TpcStats {
    pub fn channel_ber(&self) -> f64 {
        self.corrected_bits as f64 / self.data_bits.max(1) as f64
    }
}

/// Frames gathered before the structure search, and the most kept.
const IDENTIFY_MIN: usize = 4;
const IDENTIFY_MAX: usize = 16;
// The structure search (every structure over every frame held) runs each
// IDENTIFY_MIN frames while the window fills, then once a full window:
// run on every frame, a weak 685 kS/s carrier that never fitted (~460
// frames a second) left its FEC thread minutes behind.
/// The best structure must fit at least this well (wrong ones fit ~1/128);
/// those within this factor of it are told apart by decoding.
const FIT_MIN: f32 = 0.03;
const FIT_MARGIN: f32 = 6.0;
/// Frames running the UW must be found in (a chance match of three is
/// ~10⁻⁹ per place and orientation).
const UW_FRAMES: usize = 3;
/// UW bit errors allowed.
const UW_ERRORS: u32 = 3;
/// UW misses in a row before searching again.
const UW_LOST: u32 = 4;
/// Decoder iterations (Chase–Pyndiah).
const ITERATIONS: usize = 8;

/// (rotation in steps of the constellation's symmetry — quarter turns for
/// QPSK, eighths for 8PSK —, conjugated, inverted)
type Orientation = (u8, bool, bool);

/// Symbols either way a slip is looked for.
const SLIP: usize = 2;

/// tpc_2964 receiver — or Comtech TPC 7/8, whichever unique word it finds:
/// carrier-locked BPSK, QPSK or 8PSK symbols in, data bits out, a frame
/// ([`Code::data`] bits) at a time.
///
/// The carrier loop leaves a phase ambiguity (2 ways for BPSK, 8 for QPSK
/// and 16 for 8PSK, counting mirror images), which the UW resolves. When
/// the loop slips a step, or symbol timing gains or loses a symbol, the UW
/// goes missing where it was due; the other orientations and offsets of up
/// to [`SLIP`] symbols are then tried there, and one under which this UW
/// and the next both appear is taken at once, rather than losing frames to
/// a full search.
pub struct TpcRx {
    mapper: Mapper,
    /// The codes looked for: both until one's UW is found, then that one.
    search: Vec<Code>,
    /// The code found.
    code: Option<Code>,
    /// While searching: symbols come since each code was last looked for
    /// (by [`Code::index`]), so each is looked for once a frame.
    since: [usize; 2],
    /// Symbols not yet used up.
    held: Vec<Iq>,
    /// Locked: the orientation, and the bit in `held` where the next frame
    /// (its UW) starts.
    lock: Option<(Orientation, usize)>,
    layout: Option<Layout>,
    /// Hard and soft frames gathered for the structure search.
    ident: Vec<(Vec<u8>, Vec<f32>)>,
    /// Frames taken for identification so far (paces the search).
    ident_seen: usize,
    misses: u32,
    pub stats: TpcStats,
}

/// One symbol's soft bits (positive for 0) under orientation `o`.
fn soft_bits(m: Mapper, s: Iq, (rot, conj, inv): Orientation, out: &mut Vec<f32>) {
    let mut y = if conj { s.conj() } else { s };
    if m == Mapper::Psk8 {
        // Max-log: |y − x|² = |y|² + 1 − 2 Re(y x*), so each bit's value
        // is the best correlation with a point labelled 0 less the best
        // with one labelled 1. Turning y back `rot` eighths is correlating
        // it with the point `rot` further on.
        let mut best = [[f32::NEG_INFINITY; 2]; 3];
        for k in 0..8 {
            let (c, s) = PSK8[(k + rot as usize) % 8];
            let corr = y.re * c + y.im * s;
            let label = k ^ (k >> 1);
            for (b, bit) in best.iter_mut().enumerate() {
                let v = &mut bit[(label >> (2 - b)) & 1];
                *v = v.max(corr);
            }
        }
        out.extend(best.iter().map(|b| b[0] - b[1]));
        return;
    }
    for _ in 0..rot {
        y *= Iq::new(0.0, -1.0);
    }
    let sign = if inv { -1.0 } else { 1.0 };
    out.push(sign * y.re);
    if m == Mapper::Qpsk {
        out.push(sign * y.im);
    }
}

/// Bits of `code`'s UW wrong at bit `at` of `bits`.
fn uw_errors(code: Code, bits: &[f32], at: usize) -> u32 {
    let (uw, len) = (code.uw(), code.uw_len());
    (0..len)
        .filter(|&i| {
            let want = (uw >> (len - 1 - i)) & 1;
            ((bits[at + i] < 0.0) as u32) != want
        })
        .count() as u32
}

impl TpcRx {
    /// A receiver for tpc_2964 or Comtech TPC 7/8 sent on `modulation`
    /// (BPSK, QPSK or 8PSK; anything else is taken as QPSK).
    pub fn new(modulation: Modulation) -> Self {
        Self::with_codes(modulation, &Code::ALL)
    }

    /// A receiver that looks only for `codes`.
    pub fn with_codes(modulation: Modulation, codes: &[Code]) -> Self {
        TpcRx {
            mapper: Mapper::of(modulation),
            search: codes.to_vec(),
            code: None,
            // Look as soon as enough has come.
            since: [usize::MAX; 2],
            held: Vec::new(),
            lock: None,
            layout: None,
            ident: Vec::new(),
            ident_seen: 0,
            misses: 0,
            stats: TpcStats::default(),
        }
    }

    /// The code found, once its UW has been.
    pub fn code(&self) -> Option<Code> {
        self.code
    }

    /// The user's data bits a frame: the code found's, or (until then)
    /// tpc_2964's.
    pub fn data_bits(&self) -> usize {
        self.code.unwrap_or(Code::Tpc2964).data()
    }

    /// Bits a frame (UW included), as [`TpcRx::data_bits`].
    pub fn frame_bits(&self) -> usize {
        self.code.unwrap_or(Code::Tpc2964).frame()
    }

    /// Hold more symbols.
    fn add(&mut self, symbols: &[Iq]) {
        self.held.extend_from_slice(symbols);
        if self.lock.is_none() {
            for s in &mut self.since {
                *s = s.saturating_add(symbols.len());
            }
        }
    }

    /// Bits per symbol.
    fn per(&self) -> usize {
        self.mapper.bits()
    }

    /// Steps of the constellation's rotational symmetry in a turn.
    fn steps(&self) -> u8 {
        match self.mapper {
            Mapper::Psk8 => 8,
            _ => 4,
        }
    }

    fn orientations(&self) -> Vec<Orientation> {
        if self.mapper == Mapper::Bpsk {
            vec![(0, false, false), (0, false, true)]
        } else {
            (0..self.steps())
                .flat_map(|r| [(r, false, false), (r, true, false)])
                .collect()
        }
    }

    fn set_orientation(&mut self, o: Orientation) {
        self.stats.orientation = Some(match (self.mapper, o) {
            (Mapper::Bpsk, (_, _, inv)) => if inv { "inverted" } else { "upright" }.into(),
            (_, (r, c, _)) => format!(
                "turned {}°{}",
                360 / u32::from(self.steps()) * u32::from(r),
                if c { ", mirrored" } else { "" }
            ),
        });
    }

    /// `n` soft bits from bit `at` of the held symbols, under `o`.
    fn soft_at(&self, o: Orientation, at: usize, n: usize) -> Vec<f32> {
        let per = self.per();
        let (s0, s1) = (at / per, (at + n).div_ceil(per));
        let mut v = Vec::with_capacity((s1 - s0) * per);
        for &s in &self.held[s0..s1] {
            soft_bits(self.mapper, s, o, &mut v);
        }
        let skip = at - s0 * per;
        v.drain(..skip);
        v.truncate(n);
        v
    }

    /// Feed symbols; each decoded frame's data bits ([`Code::data`] of
    /// them, 0/1) go to `out`.
    pub fn push(&mut self, symbols: &[Iq], out: &mut Vec<Vec<u8>>) {
        self.add(symbols);
        if self.lock.is_none() && !self.find_uw() {
            return;
        }
        self.frames(out);
    }

    /// Symbols a search for `code`'s UW looks through: every start in a
    /// frame, and the frames from there.
    fn need(&self, code: Code) -> usize {
        (code.frame() + UW_FRAMES * code.frame() + code.uw_len()).div_ceil(self.per())
    }

    /// Look for each code's UW, frame after frame, in the newest held
    /// symbols under each orientation — each code once a frame of its own.
    fn find_uw(&mut self) -> bool {
        let per = self.per();
        let due: Vec<Code> = self
            .search
            .iter()
            .copied()
            .filter(|&c| {
                self.held.len() >= self.need(c) && self.since[c.index()] >= c.frame() / per
            })
            .collect();
        let mut found = None;
        // `if let Some(x) = …` runs the block only when there is a value.
        if let Some(span) = due.iter().map(|&c| self.need(c)).max() {
            let s0 = self.held.len() - span;
            let mut bits = Vec::with_capacity(span * per);
            // A labelled loop: `break 'search` leaves both loops at once.
            'search: for o in self.orientations() {
                bits.clear();
                for &s in &self.held[s0..] {
                    soft_bits(self.mapper, s, o, &mut bits);
                }
                for &c in &due {
                    // This code's window: its newest `need` symbols.
                    let base = (span - self.need(c)) * per;
                    let f = c.frame();
                    let hit = (0..f).find(|&off| {
                        (0..UW_FRAMES).all(|m| uw_errors(c, &bits, base + off + m * f) <= UW_ERRORS)
                    });
                    if let Some(off) = hit {
                        found = Some((c, o, s0 * per + base + off));
                        break 'search;
                    }
                }
            }
            for &c in &due {
                self.since[c.index()] = 0;
            }
        }
        if let Some((c, o, at)) = found {
            self.found_code(c);
            self.lock = Some((o, at));
            self.misses = 0;
            self.stats.uw_locked = true;
            self.set_orientation(o);
            return true;
        }
        // Keep what the longest search needs, and try again with more.
        let keep = self.search.iter().map(|&c| self.need(c)).max().unwrap_or(0);
        let drop = self.held.len().saturating_sub(keep);
        self.held.drain(..drop);
        false
    }

    /// `code`'s UW has been found: keep to it. TPC 7/8's layout is known
    /// already; tpc_2964's is found from its frames.
    fn found_code(&mut self, code: Code) {
        if self.code == Some(code) {
            return;
        }
        self.code = Some(code);
        self.search = vec![code];
        self.stats.code = Some(code);
        if code == Code::Comtech78 {
            self.layout = Some(Layout::comtech78());
            self.stats.structure = Some(format!(
                "sent row by row ((128,120) words), generator {:#04x}, first {} bits shortened, \
                 code bits not scrambled",
                tpc78::POLY,
                tpc78::SHORTENED
            ));
            self.stats.fit = 1.0;
        }
    }

    /// Where the UW due at bit `at` went, if the carrier or the timing
    /// slipped: an orientation and offset under which it and the next UW
    /// both appear. `None` when there is not yet enough signal to tell.
    fn repair(&self, code: Code, at: usize) -> Option<Option<(Orientation, usize)>> {
        let per = self.per();
        let (frame, uw_len) = (code.frame(), code.uw_len());
        if (at + SLIP * per + frame + uw_len).div_ceil(per) > self.held.len() {
            return None;
        }
        for d in [0isize, -1, 1, -2, 2] {
            let a = at as isize + d * per as isize;
            if a < 0 {
                continue;
            }
            let a = a as usize;
            for o in self.orientations() {
                if (0..2).all(|m| {
                    uw_errors(code, &self.soft_at(o, a + m * frame, uw_len), 0) <= UW_ERRORS
                }) {
                    return Some(Some((o, a)));
                }
            }
        }
        Some(None)
    }

    fn frames(&mut self, out: &mut Vec<Vec<u8>>) {
        let per = self.per();
        let Some(code) = self.code else {
            return;
        };
        let (frame, uw_len) = (code.frame(), code.uw_len());
        while let Some((mut o, mut at)) = self.lock {
            if (at + frame).div_ceil(per) > self.held.len() {
                break;
            }
            let mut uw_ok = true;
            if uw_errors(code, &self.soft_at(o, at, uw_len), 0) > UW_ERRORS {
                match self.repair(code, at) {
                    None => break, // wait for the next UW to tell
                    Some(Some((o2, a2))) => {
                        if (a2 + frame).div_ceil(per) > self.held.len() {
                            break;
                        }
                        (o, at) = (o2, a2);
                        self.set_orientation(o);
                        self.stats.slips += 1;
                        self.misses = 0;
                    }
                    Some(None) => {
                        uw_ok = false;
                        self.misses += 1;
                        self.stats.uw_misses += 1;
                        if self.misses >= UW_LOST {
                            self.lost();
                            return;
                        }
                    }
                }
            } else {
                self.misses = 0;
            }
            let block = self.soft_at(o, at + uw_len, frame - uw_len);
            // Keep a few symbols before the next frame for the slip search.
            let next = at + frame;
            let drop = (next / per).saturating_sub(SLIP);
            self.held.drain(..drop);
            self.lock = Some((o, next - drop * per));
            self.stats.frames += 1;
            self.frame(block, uw_ok, out);
        }
    }

    /// The UW has gone: search again from the symbols held (for the code
    /// found, at once).
    fn lost(&mut self) {
        self.lock = None;
        self.misses = 0;
        self.since = [usize::MAX; 2];
        self.stats.uw_locked = false;
        self.stats.orientation = None;
    }

    /// One frame's block; `uw_ok`: its UW was where it was due.
    fn frame(&mut self, block: Vec<f32>, uw_ok: bool, out: &mut Vec<Vec<u8>>) {
        if self.layout.is_some() {
            // Without its UW a frame that will not decode is most likely
            // not a frame at all (the signal jumped): nothing to pass on.
            if let Some(d) = self.decode(&block, uw_ok) {
                out.push(d);
            }
            return;
        }
        if !uw_ok {
            return;
        }
        let hard = block.iter().map(|&v| (v < 0.0) as u8).collect();
        self.ident.push((hard, block));
        self.ident_seen += 1;
        if self.ident.len() < IDENTIFY_MIN {
            return;
        }
        if self.ident.len() > IDENTIFY_MAX {
            self.ident.remove(0);
        }
        let every = if self.ident_seen < IDENTIFY_MAX {
            IDENTIFY_MIN
        } else {
            IDENTIFY_MAX
        };
        if !self.ident_seen.is_multiple_of(every) {
            return;
        }
        let all = Structure::all();
        let hard: Vec<Vec<u8>> = self.ident.iter().map(|f| f.0.clone()).collect();
        let fit = fits(&all, &hard);
        let mut order: Vec<usize> = (0..fit.len()).collect();
        order.sort_by(|&a, &b| fit[b].total_cmp(&fit[a]));
        let best = fit[order[0]];
        self.stats.fit = best;
        if best < FIT_MIN {
            return; // too noisy yet, or not this code
        }
        // Structures near the best can share part of its parity (a reversed
        // Hamming word is a word of the reciprocal polynomial's code, which
        // makes some relatives fit a quarter as well): soft-decode the frames
        // under each and keep the one that converges most often.
        let near: Vec<usize> = order
            .iter()
            .copied()
            .take_while(|&i| fit[i] * FIT_MARGIN >= best)
            .take(6)
            .collect();
        let chosen = if near.len() == 1 {
            near[0]
        } else {
            let scores: Vec<usize> = near
                .iter()
                .map(|&i| {
                    let lay = Layout::new(all[i]);
                    let mut data = vec![0u8; DATA];
                    self.ident
                        .iter()
                        .filter(|f| {
                            lay.code
                                .decode(&lay.arrange(&f.1), &mut data, ITERATIONS)
                                .converged
                        })
                        .count()
                })
                .collect();
            let top = *scores.iter().max().unwrap();
            if top == 0 || scores.iter().filter(|&&s| s == top).count() > 1 {
                return; // not told apart yet: more frames
            }
            near[scores.iter().position(|&s| s == top).unwrap()]
        };
        let st = all[chosen];
        self.stats.structure = Some(st.describe());
        self.stats.fit = fit[chosen];
        self.layout = Some(Layout::new(st));
        for (_, b) in std::mem::take(&mut self.ident) {
            out.extend(self.decode(&b, true));
        }
    }

    /// Soft-decode one block; its data bits in the order sent.
    /// `None` for a frame that neither had its UW nor decoded.
    fn decode(&mut self, block: &[f32], uw_ok: bool) -> Option<Vec<u8>> {
        let lay = self.layout.as_ref().expect("structure known");
        let arranged = lay.arrange(block);
        let mut data = vec![0u8; lay.code.k()];
        let o = lay.code.decode(&arranged, &mut data, ITERATIONS);
        if !o.converged {
            self.stats.failed += 1;
            if !uw_ok {
                return None;
            }
        } else {
            self.stats.decoded += 1;
            // Data bits the decoder changed against the received ones: the
            // channel's errors, counted where decoding says what was sent.
            let (rk, rn) = (lay.code.row.k, lay.code.row.n);
            let changed = lay
                .data_order
                .iter()
                .filter(|&&i| (arranged[(i / rk) * rn + i % rk] < 0.0) as u8 != data[i])
                .count();
            self.stats.corrected_bits += changed as u64;
            self.stats.data_bits += lay.data_order.len() as u64;
            if self.code == Some(Code::Comtech78) {
                // The data bits after the user's.
                let end = tpc78::SHORTENED + tpc78::DATA;
                self.stats.overhead = Some(
                    data[end..end + tpc78::OVERHEAD]
                        .iter()
                        .fold(0, |a, &b| (a << 1) | u32::from(b)),
                );
            }
        }
        Some(lay.data_order.iter().map(|&d| data[d]).collect())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn rng(seed: u64) -> impl FnMut() -> u64 {
        let mut s = seed | 1;
        move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    /// Unit-variance Gaussian noise (Box–Muller).
    pub(crate) fn gauss(seed: u64) -> impl FnMut() -> f32 {
        let mut g = rng(seed);
        move || {
            let a = ((g() >> 11) as f64 / (1u64 << 53) as f64).max(1e-300);
            let b = (g() >> 11) as f64 / (1u64 << 53) as f64;
            ((-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()) as f32
        }
    }

    /// Frame bits on `m` at `esn0_db`, conjugated if `conj` and then
    /// turned by `turn`, starting mid-frame (on 8PSK, 778 bits in puts
    /// each frame's UW on a symbol's third bit).
    pub(crate) fn on_air(
        bits: &[u8],
        m: Modulation,
        esn0_db: f32,
        turn: Iq,
        conj: bool,
    ) -> Vec<Iq> {
        let mut sym = Vec::new();
        modulate(&bits[778..], m, &mut sym);
        let sigma = (10f32.powf(-esn0_db / 10.0) / 2.0).sqrt();
        let mut n = gauss(0xABC);
        sym.into_iter()
            .map(|s| {
                let s = if conj { s.conj() } else { s };
                s * turn + Iq::new(sigma * n(), sigma * n())
            })
            .collect()
    }

    fn check(st: Structure, m: Modulation, esn0: f32, turn: Iq, conj: bool) -> TpcStats {
        let tx = TpcTx::new(st);
        let mut next = rng(5);
        let mut bits = Vec::new();
        let mut sent = Vec::new();
        for _ in 0..20 {
            let d: Vec<u8> = (0..DATA).map(|_| (next() & 1) as u8).collect();
            tx.frame(&d, &mut bits);
            sent.push(d);
        }
        let sym = on_air(&bits, m, esn0, turn, conj);
        let mut rx = TpcRx::new(m);
        let mut out = Vec::new();
        for c in sym.chunks(1000) {
            rx.push(c, &mut out);
        }
        assert!(out.len() >= 15, "{} frames: {:?}", out.len(), rx.stats);
        // The first whole frame received is the second sent.
        for (k, f) in out.iter().enumerate() {
            assert_eq!(*f, sent[1 + k], "frame {k}: {:?}", rx.stats);
        }
        assert_eq!(rx.stats.structure.as_deref(), Some(st.describe().as_str()));
        rx.stats
    }

    #[test]
    fn scrambler_wirings_differ() {
        let seqs: Vec<Vec<u8>> = CodeScrambler::all().iter().map(|s| s.sequence()).collect();
        for i in 0..seqs.len() {
            for j in i + 1..seqs.len() {
                assert_ne!(seqs[i], seqs[j]);
            }
        }
        // 1 + x² + x³ + x⁹ + x¹² is primitive: period 4095, longer than a
        // frame.
        let s = additive_sequence(true, false, 2 * 4095);
        assert_eq!(s[..4095], s[4095..]);
        assert!((1..4095).all(|p| s[..100] != s[p..p + 100]));
    }

    #[test]
    fn finds_the_structure_under_any_qpsk_orientation() {
        let st = Structure {
            row_wise: true,
            flip_rows: false,
            flip_cols: false,
            poly: 0x43,
            scrambler: CodeScrambler::Additive {
                last: false,
                reversed: false,
                from_uw: false,
            },
        };
        for (turn, conj) in [
            (Iq::new(1.0, 0.0), false),
            (Iq::new(0.0, 1.0), true),
            (Iq::new(-1.0, 0.0), false),
            (Iq::new(0.0, -1.0), true),
        ] {
            check(st, Modulation::Qpsk, 8.0, turn, conj);
        }
    }

    #[test]
    fn finds_other_structures_on_bpsk() {
        for st in [
            Structure {
                row_wise: false,
                flip_rows: true,
                flip_cols: false,
                poly: 0x6D,
                scrambler: CodeScrambler::None,
            },
            Structure {
                row_wise: false,
                flip_rows: false,
                flip_cols: true,
                poly: 0x61,
                scrambler: CodeScrambler::Additive {
                    last: true,
                    reversed: true,
                    from_uw: true,
                },
            },
        ] {
            check(st, Modulation::Bpsk, 7.0, Iq::new(-1.0, 0.0), false);
        }
    }

    #[test]
    fn rides_through_carrier_and_timing_slips() {
        let tx = TpcTx::new(Structure::TEST);
        let mut next = rng(8);
        let mut bits = Vec::new();
        let mut sent = Vec::new();
        for _ in 0..24 {
            let d: Vec<u8> = (0..DATA).map(|_| (next() & 1) as u8).collect();
            tx.frame(&d, &mut bits);
            sent.push(d);
        }
        let mut sym = on_air(&bits, Modulation::Qpsk, 9.0, Iq::new(1.0, 0.0), false);
        // A quarter turn from a third of the way in, and a symbol lost at
        // two thirds.
        let n = sym.len();
        for s in &mut sym[n / 3..] {
            *s *= Iq::new(0.0, 1.0);
        }
        sym.remove(2 * n / 3);
        let mut rx = TpcRx::new(Modulation::Qpsk);
        let mut out = Vec::new();
        for c in sym.chunks(700) {
            rx.push(c, &mut out);
        }
        let s = &rx.stats;
        assert_eq!(s.slips, 2, "{s:?}");
        assert_eq!(s.uw_misses, 0, "{s:?}");
        // Only the two frames a slip falls in come out wrong.
        let wrong = out
            .iter()
            .enumerate()
            .filter(|(k, f)| **f != sent[1 + k])
            .count();
        assert!(
            out.len() >= 20 && wrong <= 2,
            "{} frames, {wrong} wrong",
            out.len()
        );
    }

    #[test]
    fn corrects_a_noisy_channel() {
        let st = Structure {
            row_wise: true,
            flip_rows: false,
            flip_cols: true,
            poly: 0x5B,
            scrambler: CodeScrambler::Additive {
                last: true,
                reversed: false,
                from_uw: false,
            },
        };
        // QPSK at 5 dB Es/N0 (Eb/N0 3.2 dB at rate 3/4): raw BER ~2.3 %,
        // and every frame comes out clean.
        let s = check(st, Modulation::Qpsk, 5.0, Iq::new(1.0, 0.0), false);
        assert!(s.channel_ber() > 0.01, "{s:?}");
        assert_eq!(s.failed, 0, "{s:?}");
    }

    /// 8PSK, as a live Comtech carrier sends it (the test structure, the
    /// UW off the symbol grid), under eighth turns and mirror images.
    #[test]
    fn finds_the_structure_on_8psk_under_any_orientation() {
        for (k, conj) in [(0, false), (3, true), (5, false), (6, true)] {
            let a = std::f32::consts::FRAC_PI_4 * k as f32;
            let s = check(
                Structure::TEST,
                Modulation::Psk8,
                12.0,
                Iq::new(a.cos(), a.sin()),
                conj,
            );
            assert_eq!(s.failed, 0, "{s:?}");
        }
    }

    /// Frames decoded on 8PSK against Es/N0, for tuning the decoder (run by
    /// hand: `cargo test --release -p decsat-modem waterfall_8psk -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn waterfall_8psk() {
        let tx = TpcTx::new(Structure::TEST);
        let lay = Layout::new(Structure::TEST);
        let mut next = rng(77);
        for esn0 in [8.4f32, 8.6, 8.8, 9.0, 9.2, 9.4] {
            let sigma = (10f32.powf(-esn0 / 10.0) / 2.0).sqrt();
            let mut n = gauss(esn0.to_bits() as u64);
            let (mut ok, mut raw_err, mut raw_n, mut frames) = (0, 0usize, 0usize, 0);
            for _ in 0..200 {
                let d: Vec<u8> = (0..DATA).map(|_| (next() & 1) as u8).collect();
                let mut bits = Vec::new();
                tx.frame(&d, &mut bits);
                let mut sym = Vec::new();
                modulate(&bits, Modulation::Psk8, &mut sym);
                let mut soft = Vec::new();
                for s in sym {
                    let y = s + Iq::new(sigma * n(), sigma * n());
                    soft_bits(Mapper::Psk8, y, (0, false, false), &mut soft);
                }
                raw_err += soft
                    .iter()
                    .zip(&bits)
                    .filter(|(v, b)| ((**v < 0.0) as u8) != **b)
                    .count();
                raw_n += bits.len();
                let block = &soft[UW_LEN..FRAME];
                let mut out = vec![0u8; DATA];
                lay.code.decode(&lay.arrange(block), &mut out, ITERATIONS);
                let sent: Vec<u8> = lay.data_order.iter().map(|&i| d[i]).collect();
                let got: Vec<u8> = lay.data_order.iter().map(|&i| out[i]).collect();
                ok += usize::from(got == sent);
                frames += 1;
            }
            println!(
                "Es/N0 {esn0:.1} dB: raw BER {:.2} %, {ok} of {frames} frames right",
                100.0 * raw_err as f64 / raw_n as f64
            );
        }
    }

    /// Comtech TPC 7/8: the receiver tells it by its UW, under any QPSK
    /// orientation, and decodes it through noise — QPSK at 7.5 dB Es/N0
    /// (raw BER ~0.6 %), the live carrier was at 12.3 dB.
    #[test]
    fn receives_comtech_tpc_7_8() {
        let code = tpc78::code();
        let mut next = rng(78);
        let mut bits = Vec::new();
        let mut sent = Vec::new();
        for _ in 0..7 {
            let d: Vec<u8> = (0..tpc78::DATA).map(|_| (next() & 1) as u8).collect();
            tpc78::frame(&code, &d, 0x8C_EA42, &mut bits);
            sent.push(d);
        }
        for (turn, conj) in [(Iq::new(0.0, 1.0), true), (Iq::new(-1.0, 0.0), false)] {
            let sym = on_air(&bits, Modulation::Qpsk, 7.5, turn, conj);
            let mut rx = TpcRx::new(Modulation::Qpsk);
            let mut out = Vec::new();
            for c in sym.chunks(1000) {
                rx.push(c, &mut out);
            }
            let s = &rx.stats;
            assert_eq!(s.code, Some(Code::Comtech78), "{s:?}");
            assert_eq!(out.len(), 6, "{s:?}");
            // The first whole frame received is the second sent.
            for (k, f) in out.iter().enumerate() {
                assert_eq!(*f, sent[1 + k], "frame {k}: {s:?}");
            }
            assert_eq!(s.overhead, Some(0x8C_EA42));
            assert_eq!(s.failed, 0, "{s:?}");
            assert!(s.channel_ber() > 0.002, "{s:?}");
        }
    }

    /// 8PSK at 9.5 dB Es/N0 (Eb/N0 6 dB at rate 3/4), a little under the
    /// live carrier's 10.6 dB: raw BER ~3 %, and every frame comes out clean.
    #[test]
    fn corrects_a_noisy_8psk_channel() {
        let s = check(
            Structure::TEST,
            Modulation::Psk8,
            9.5,
            Iq::new(1.0, 0.0),
            false,
        );
        assert!(s.channel_ber() > 0.02, "{s:?}");
        assert_eq!(s.failed, 0, "{s:?}");
    }
}
