// made by CasualArclamp
//! A DVB-RCS2 linear-modulation burst (EN 301 545-2 §7.3, TC-LM): built
//! from a frame PDU, and read back from the burst's symbols.
//!
//! The chain (figure 7-8): the PDU is scrambled (§7.3.3, the PRBS
//! 1 + x^14 + x^15), a CRC put after it (§7.3.4: CRC16 for logon and
//! control bursts, CRC32 for traffic, over the scrambled bits), turbo coded
//! ([`decsat_fec::duobinary`], §7.3.5.1), mapped to symbols (§7.3.7.1.4:
//! π/2-BPSK, QPSK, 8PSK or 16QAM, each with its own order of systematic
//! and parity bits), and the known symbols put round and among them
//! (§7.3.6.1: a preamble, pilot blocks every pilot period, a postamble).
//! What a burst type is — sizes, permutation, puncturing, known symbols —
//! comes from the BCT ([`decsat_ts::rcs2::LinearBurst`]) or is one of
//! annex A's reference waveforms ([`reference`]).
//!
//! The receiver here takes the burst at one sample a symbol, timed (the
//! matched filter and timing are the caller's): the carrier's frequency,
//! phase and gain from the known symbols, the phase followed from pilot
//! block to pilot block, max-log LLRs, the turbo decoder, the CRC.
//!
//! Built from the standard alone: no RCS2 burst has been read off the air
//! yet, so where the standard's figures had to be read (the constellations'
//! labels, the turbo encoder's taps) the reading is untested on a real one.

use decsat_core::Iq;
use decsat_core::crc::crc32_mpeg2;
use decsat_fec::duobinary::DuoBinaryTurbo;
use decsat_ts::rcs2::LinearBurst;

/// The payload modulations (Table 6-19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modulation {
    /// 0x00: plain BPSK (Table 6-19's reserved code; annex A's waveforms
    /// 40–43 name it).
    Bpsk,
    Pi2Bpsk,
    Qpsk,
    Psk8,
    Qam16,
}

impl Modulation {
    pub fn from_code(m: u8) -> Option<Self> {
        Some(match m {
            0x00 => Self::Bpsk,
            0x01 => Self::Qpsk,
            0x02 => Self::Psk8,
            0x03 => Self::Qam16,
            0x05 => Self::Pi2Bpsk,
            _ => return None,
        })
    }

    /// Bits a symbol.
    pub fn bits(self) -> usize {
        match self {
            Self::Bpsk | Self::Pi2Bpsk => 1,
            Self::Qpsk => 2,
            Self::Psk8 => 3,
            Self::Qam16 => 4,
        }
    }

    /// The point of a symbol whose bits are `v` (u0 the most significant;
    /// 16QAM's in Table 7-20's order uQ1 uQ0 uI1 uI0), `n` symbols into
    /// the burst (π/2-BPSK turns by a quarter a symbol).
    pub fn point(self, v: u32, n: usize) -> Iq {
        let s = std::f32::consts::FRAC_1_SQRT_2;
        match self {
            Self::Bpsk => Iq::new(if v & 1 == 0 { 1.0 } else { -1.0 }, 0.0),
            // §7.3.7.1.4.1: +1 for a 0, turned by π/2 · n + π/4.
            Self::Pi2Bpsk => {
                let a = std::f32::consts::FRAC_PI_2 * (n % 4) as f32 + std::f32::consts::FRAC_PI_4;
                Iq::from_polar(if v & 1 == 0 { 1.0 } else { -1.0 }, a)
            }
            // Figure 7-20: 00 at π/4, 01 at −π/4, 10 at 3π/4, 11 at −3π/4.
            Self::Qpsk => Iq::new(
                if v & 2 == 0 { s } else { -s },
                if v & 1 == 0 { s } else { -s },
            ),
            // Figure 7-21, u0u1u2 → odd multiples of π/8.
            Self::Psk8 => {
                const EIGHTHS: [f32; 8] = [1.0, 3.0, 15.0, 13.0, 7.0, 5.0, 9.0, 11.0];
                Iq::from_polar(1.0, EIGHTHS[(v & 7) as usize] * std::f32::consts::PI / 8.0)
            }
            // Table 7-21: of a PAM's two bits the second (u1) the sign, the
            // first (u0) the size: 00 −1, 01 +1, 10 −3, 11 +3, over √10.
            Self::Qam16 => {
                let pam = |u1: u32, u0: u32| {
                    (if u1 == 1 { 1.0 } else { -1.0 }) * (if u0 == 1 { 3.0 } else { 1.0 })
                        / 10f32.sqrt()
                };
                Iq::new(pam((v >> 1) & 1, v & 1), pam((v >> 3) & 1, (v >> 2) & 1))
            }
        }
    }
}

/// What one bit of a payload symbol carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bit {
    A(u32),
    B(u32),
    Z1(u32),
    Z2(u32),
    Zero,
}

/// One symbol of a burst.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Symbol {
    Known(Iq),
    /// The i-th payload symbol.
    Payload(u32),
}

/// Which CRC a burst ends in (Table 7-12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crc {
    /// Logon and control bursts.
    Crc16,
    /// Traffic.
    Crc32,
}

impl Crc {
    pub fn bytes(self) -> usize {
        match self {
            Self::Crc16 => 2,
            Self::Crc32 => 4,
        }
    }

    /// The CRC of `b`, big-endian.
    fn of(self, b: &[u8]) -> Vec<u8> {
        match self {
            Self::Crc16 => crc16(b).to_be_bytes().to_vec(),
            Self::Crc32 => crc32_mpeg2(b).to_be_bytes().to_vec(),
        }
    }
}

/// §7.3.4's CRC16: x^16 + x^15 + x^2 + 1, the register from 0, MSB first,
/// the register sent as it stands.
fn crc16(b: &[u8]) -> u16 {
    let mut r = 0u16;
    for &x in b {
        r ^= u16::from(x) << 8;
        for _ in 0..8 {
            r = if r & 0x8000 != 0 {
                (r << 1) ^ 0x8005
            } else {
                r << 1
            };
        }
    }
    r
}

/// §7.3.3's energy dispersal (the same PRBS as DVB-S): registers SR1–SR15
/// from Table 7-11's 100101010000000, SR14 ⊕ SR15 added to each bit and fed
/// back into SR1. Its own inverse.
pub fn scramble(b: &[u8]) -> Vec<u8> {
    let mut r: u16 = 0b000_0000_1010_1001; // bit i is SR(i+1)
    b.iter()
        .map(|&x| {
            let mut out = 0u8;
            for i in (0..8).rev() {
                let p = ((r >> 13) ^ (r >> 14)) & 1;
                r = ((r << 1) | p) & 0x7FFF;
                out |= (((x >> i) & 1) ^ p as u8) << i;
            }
            out
        })
        .collect()
}

/// A burst type laid out: which symbols are known, and which code bits
/// each payload symbol carries.
#[derive(Debug, Clone)]
pub struct BurstShape {
    pub modulation: Modulation,
    pub payload_bytes: usize,
    turbo: DuoBinaryTurbo,
    symbols: Vec<Symbol>,
    bits: Vec<Bit>,
}

impl BurstShape {
    /// `None` for a modulation, size or permutation the standard does not
    /// allow, or a unique word too short for its preamble, pilots and
    /// postamble.
    pub fn new(b: &LinearBurst) -> Option<Self> {
        let modulation = Modulation::from_code(b.modulation)?;
        let m = modulation.bits();
        let n = usize::from(b.payload_bytes) * 4;
        let turbo = DuoBinaryTurbo::new(
            n,
            u32::from(b.p),
            b.q.map(u32::from),
            &b.y_pattern,
            &b.w_pattern,
        )?;
        let parity = turbo.parity_bits() as u32;
        let mut sys = (0..n as u32).flat_map(|k| [Bit::A(k), Bit::B(k)]);
        let mut bits = Vec::new();
        match modulation {
            // Tables 7-16, 7-17: the systematic couples, then the parity
            // pairs (Z1,n, Z2,n).
            Modulation::Bpsk | Modulation::Pi2Bpsk | Modulation::Qpsk => {
                bits.extend(sys);
                for i in 0..parity {
                    bits.extend([Bit::Z1(i), Bit::Z2(i)]);
                }
            }
            // Tables 7-18, 7-19: a parity bit (Z1 and Z2 by turns) and two
            // systematic bits a symbol; once the parity is out, three
            // systematic bits.
            Modulation::Psk8 => {
                for i in 0..2 * parity {
                    let z = if i % 2 == 0 {
                        Bit::Z1(i / 2)
                    } else {
                        Bit::Z2(i / 2)
                    };
                    bits.push(z);
                    bits.extend((0..2).map(|_| sys.next().unwrap_or(Bit::Zero)));
                }
                bits.extend(sys);
            }
            // Table 7-20: a parity bit on uQ1 and three systematic bits;
            // then four.
            Modulation::Qam16 => {
                for i in 0..2 * parity {
                    let z = if i % 2 == 0 {
                        Bit::Z1(i / 2)
                    } else {
                        Bit::Z2(i / 2)
                    };
                    bits.push(z);
                    bits.extend((0..3).map(|_| sys.next().unwrap_or(Bit::Zero)));
                }
                bits.extend(sys);
            }
        }
        while bits.len() % m != 0 {
            bits.push(Bit::Zero);
        }
        let payload = bits.len() / m;

        // The known symbols (Table 6-18's UW): preamble, one pilot block,
        // postamble.
        let pilots = b.pilot_period > 0 && b.pilot_block > 0;
        let blk = if pilots {
            usize::from(b.pilot_block)
        } else {
            0
        };
        let (pre, post) = (usize::from(b.preamble), usize::from(b.postamble));
        if b.uw.len() < pre + blk + post {
            return None;
        }
        let known = |i: usize, n: usize| {
            let mut v = u32::from(b.uw[i]);
            // A known 16QAM symbol is given as (uI1, uI0, uQ1, uQ0)
            // (§7.3.7.1.4.4), the payload's order turned round.
            if modulation == Modulation::Qam16 {
                v = ((v & 3) << 2) | (v >> 2);
            }
            Symbol::Known(modulation.point(v, n))
        };
        let mut symbols = Vec::with_capacity(pre + post + payload + usize::from(b.pilot_sum));
        for i in 0..pre {
            symbols.push(known(i, symbols.len()));
        }
        // §7.3.6.1: a pilot block's last symbol a pilot period after the
        // preamble's last, blocks a period apart, as many as the pilot sum
        // allows while the payload left fills the gap before each.
        let mut next = 0u32;
        if pilots {
            let gap = usize::from(b.pilot_period).saturating_sub(blk);
            for _ in 0..usize::from(b.pilot_sum) / blk {
                if payload - (next as usize) < gap {
                    break;
                }
                for _ in 0..gap {
                    symbols.push(Symbol::Payload(next));
                    next += 1;
                }
                for i in 0..blk {
                    symbols.push(known(pre + i, symbols.len()));
                }
            }
        }
        while (next as usize) < payload {
            symbols.push(Symbol::Payload(next));
            next += 1;
        }
        for i in 0..post {
            symbols.push(known(pre + blk + i, symbols.len()));
        }
        Some(Self {
            modulation,
            payload_bytes: usize::from(b.payload_bytes),
            turbo,
            symbols,
            bits,
        })
    }

    /// Symbols in the burst, known ones too.
    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    /// Payload symbols.
    pub fn payload_symbols(&self) -> usize {
        self.bits.len() / self.modulation.bits()
    }

    /// The burst's symbols for a frame PDU of `payload_bytes − CRC` bytes.
    pub fn transmit(&self, pdu: &[u8], crc: Crc) -> Vec<Iq> {
        assert_eq!(pdu.len() + crc.bytes(), self.payload_bytes);
        let mut block = scramble(pdu);
        block.extend(crc.of(&block));
        let couples: Vec<u8> = block
            .iter()
            .flat_map(|&x| [x >> 6, (x >> 4) & 3, (x >> 2) & 3, x & 3])
            .collect();
        let (z1, z2) = self.turbo.encode(&couples);
        let bit = |b: Bit| -> u32 {
            u32::from(match b {
                Bit::A(k) => couples[k as usize] >> 1,
                Bit::B(k) => couples[k as usize] & 1,
                Bit::Z1(i) => z1[i as usize],
                Bit::Z2(i) => z2[i as usize],
                Bit::Zero => 0,
            })
        };
        let m = self.modulation.bits();
        self.symbols
            .iter()
            .enumerate()
            .map(|(n, s)| match *s {
                Symbol::Known(p) => p,
                Symbol::Payload(i) => {
                    let v = self.bits[i as usize * m..(i as usize + 1) * m]
                        .iter()
                        .fold(0, |v, &b| (v << 1) | bit(b));
                    self.modulation.point(v, n)
                }
            })
            .collect()
    }

    /// The known symbols: (place in the burst, point).
    fn known(&self) -> Vec<(usize, Iq)> {
        self.symbols
            .iter()
            .enumerate()
            .filter_map(|(n, s)| match *s {
                Symbol::Known(p) => Some((n, p)),
                Symbol::Payload(_) => None,
            })
            .collect()
    }

    /// A burst read from its symbols, one a symbol, `r[0]` the first.
    pub fn receive(&self, r: &[Iq]) -> Option<Received> {
        if r.len() < self.symbols.len() {
            return None;
        }
        let known = self.known();
        if known.len() < 2 {
            return None;
        }
        let (freq, phase, gain) = carrier(r, &known)?;
        let turn = |n: usize| {
            Iq::from_polar(
                1.0 / gain,
                -(std::f64::consts::TAU * freq * n as f64 + phase) as f32,
            )
        };
        let mut y: Vec<Iq> = (0..self.symbols.len()).map(|n| r[n] * turn(n)).collect();
        follow_phase(&mut y, &known);
        let noise = known
            .iter()
            .map(|&(n, p)| (y[n] - p).norm_sqr())
            .sum::<f32>()
            / known.len() as f32;
        let power = known.iter().map(|&(_, p)| p.norm_sqr()).sum::<f32>() / known.len() as f32;
        let noise = noise.max(1e-6);

        // Max-log LLRs on each code bit's place.
        let m = self.modulation.bits();
        let n = self.turbo.couples();
        let parity = self.turbo.parity_bits();
        let (mut la, mut lb) = (vec![0f32; n], vec![0f32; n]);
        let (mut z1, mut z2) = (vec![0f32; parity], vec![0f32; parity]);
        for (pos, s) in self.symbols.iter().enumerate() {
            let Symbol::Payload(i) = *s else { continue };
            let points: Vec<Iq> = (0..1u32 << m)
                .map(|v| self.modulation.point(v, pos))
                .collect();
            for (j, &b) in self.bits[i as usize * m..(i as usize + 1) * m]
                .iter()
                .enumerate()
            {
                let mask = 1u32 << (m - 1 - j);
                let (mut d0, mut d1) = (f32::MAX, f32::MAX);
                for (v, p) in points.iter().enumerate() {
                    let d = (y[pos] - p).norm_sqr();
                    if v as u32 & mask == 0 {
                        d0 = d0.min(d);
                    } else {
                        d1 = d1.min(d);
                    }
                }
                let l = (d1 - d0) / noise;
                match b {
                    Bit::A(k) => la[k as usize] = l,
                    Bit::B(k) => lb[k as usize] = l,
                    Bit::Z1(k) => z1[k as usize] = l,
                    Bit::Z2(k) => z2[k as usize] = l,
                    Bit::Zero => {}
                }
            }
        }
        let (couples, agreed) = self.turbo.decode(&la, &lb, &z1, &z2, 8);
        let block: Vec<u8> = couples
            .chunks(4)
            .map(|c| c.iter().fold(0u8, |v, &d| (v << 2) | d))
            .collect();
        let crc = [Crc::Crc32, Crc::Crc16].into_iter().find(|&c| {
            block.len() > c.bytes() && {
                let (data, tail) = block.split_at(block.len() - c.bytes());
                c.of(data) == tail
            }
        });
        let pdu = scramble(&block[..block.len() - crc.map_or(0, Crc::bytes)]);
        Some(Received {
            pdu,
            crc,
            agreed,
            freq,
            mer_db: 10.0 * (power / noise).log10(),
        })
    }
}

/// A burst read back.
#[derive(Debug, Clone, PartialEq)]
pub struct Received {
    /// The frame PDU (descrambled; the CRC's bytes off when one checked,
    /// else the whole block).
    pub pdu: Vec<u8>,
    /// The CRC that checked, if one did.
    pub crc: Option<Crc>,
    /// The turbo decoder's two halves agreed at the end.
    pub agreed: bool,
    /// The carrier's offset, cycles a symbol.
    pub freq: f64,
    /// MER on the known symbols, dB.
    pub mer_db: f32,
}

/// The carrier from the known symbols: frequency (cycles a symbol), phase
/// and gain. Coarse from neighbouring known symbols (the preamble's), then
/// the peak of their coherent sum over all of them.
fn carrier(r: &[Iq], known: &[(usize, Iq)]) -> Option<(f64, f64, f32)> {
    let z: Vec<(usize, num_complex::Complex64)> = known
        .iter()
        .map(|&(n, p)| {
            let v = r[n] * p.conj() / p.norm_sqr().max(1e-9);
            (
                n,
                num_complex::Complex64::new(f64::from(v.re), f64::from(v.im)),
            )
        })
        .collect();
    let diff: num_complex::Complex64 = z
        .windows(2)
        .filter(|w| w[1].0 == w[0].0 + 1)
        .map(|w| w[1].1 * w[0].1.conj())
        .sum();
    let mut f = if diff.norm() > 0.0 {
        diff.arg() / std::f64::consts::TAU
    } else {
        0.0
    };
    let sum = |f: f64| -> num_complex::Complex64 {
        z.iter()
            .map(|&(n, v)| {
                v * num_complex::Complex64::from_polar(1.0, -std::f64::consts::TAU * f * n as f64)
            })
            .sum()
    };
    // Refine: a grid round the coarse value, narrowing.
    let span = (z.last()?.0 - z.first()?.0).max(1) as f64;
    let mut step = 0.5 / span;
    for _ in 0..4 {
        let best = (-8..=8)
            .map(|k| f + f64::from(k) * step / 4.0)
            .max_by(|a, b| sum(*a).norm().total_cmp(&sum(*b).norm()))?;
        f = best;
        step /= 4.0;
    }
    let c = sum(f) / z.len() as f64;
    Some((f, c.arg(), c.norm().max(1e-9) as f32))
}

/// The phase that remains, measured on each run of known symbols and
/// drawn straight between them, taken off.
fn follow_phase(y: &mut [Iq], known: &[(usize, Iq)]) {
    // Runs of neighbouring known symbols: (centre, phase).
    let mut runs: Vec<(f32, f32)> = Vec::new();
    let mut i = 0;
    while i < known.len() {
        let mut j = i;
        while j + 1 < known.len() && known[j + 1].0 == known[j].0 + 1 {
            j += 1;
        }
        let acc: Iq = known[i..=j].iter().map(|&(n, p)| y[n] * p.conj()).sum();
        let centre = (known[i].0 + known[j].0) as f32 / 2.0;
        // Unwrapped against the run before.
        let mut a = acc.arg();
        if let Some(&(_, prev)) = runs.last() {
            a = prev + (a - prev + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU)
                - std::f32::consts::PI;
        }
        runs.push((centre, a));
        i = j + 1;
    }
    if runs.len() < 2 {
        return;
    }
    let mut k = 0;
    for (n, v) in y.iter_mut().enumerate() {
        let t = n as f32;
        while k + 2 < runs.len() && t > runs[k + 1].0 {
            k += 1;
        }
        let ((t0, a0), (t1, a1)) = (runs[k], runs[k + 1]);
        let a = if t <= t0 {
            a0
        } else if t >= t1 {
            a1
        } else {
            a0 + (a1 - a0) * (t - t0) / (t1 - t0)
        };
        *v *= Iq::from_polar(1.0, -a);
    }
}

/// Hex digits to symbols of `m` bits, MSB first.
fn uw_symbols(hex: &str, m: usize, count: usize) -> Vec<u8> {
    let bits: Vec<u8> = hex
        .chars()
        .filter_map(|c| c.to_digit(16))
        .flat_map(|d| (0..4).rev().map(move |i| ((d >> i) & 1) as u8))
        .collect();
    bits.chunks(m)
        .take(count)
        .map(|c| c.iter().fold(0, |v, &b| (v << 1) | b))
        .collect()
}

/// Annex A's reference waveforms 1–7 (Table A-1, the QPSK ones) as
/// [`LinearBurst`]s.
pub fn reference(id: u8) -> Option<LinearBurst> {
    // (payload bytes, preamble, postamble, pilot period, pilot block,
    // pilot sum, P, Q0–Q3, Y pattern, W pattern, UW)
    let (bytes, pre, post, period, blk, sum, p, q, y, w, uw): (
        u16,
        u8,
        u8,
        u16,
        u8,
        u8,
        u8,
        [u8; 4],
        &str,
        &str,
        &str,
    ) = match id {
        1 => (
            38,
            155,
            27,
            18,
            1,
            26,
            17,
            [9, 5, 14, 1],
            "1",
            "1",
            "3300FC0FF3C33CCFFF0300C0FCF300CCCCCF0CFFC3C3F00CFCC0F33FF0CC0F00F030F330CFFF00F030F330CFFF00",
        ),
        2 => (
            14,
            41,
            41,
            15,
            1,
            12,
            9,
            [2, 2, 8, 0],
            "1",
            "1",
            "3CF0003F0F30CCCFCFFC03CF0003F0F30CCCFCFFC0",
        ),
        3 => (
            38,
            27,
            27,
            18,
            1,
            26,
            17,
            [9, 5, 14, 1],
            "1",
            "1",
            "F030F330CFFF00F030F330CFFF00",
        ),
        4 => (
            59,
            22,
            22,
            24,
            1,
            20,
            23,
            [10, 2, 11, 1],
            "1",
            "0",
            "C300FCC33FC30C03F30CFF0",
        ),
        5 => (
            85,
            13,
            13,
            0,
            1,
            0,
            23,
            [6, 13, 10, 0],
            "10",
            "0",
            "F30303FCC0C0F",
        ),
        6 => (
            96,
            12,
            12,
            0,
            1,
            0,
            25,
            [1, 2, 0, 1],
            "101000",
            "0",
            "3FCC303FCC30",
        ),
        7 => (
            108,
            9,
            8,
            0,
            1,
            0,
            29,
            [1, 4, 1, 1],
            "10001000100010000000",
            "0",
            "C033CCFC0",
        ),
        _ => return None,
    };
    let pattern = |s: &str| s.chars().map(|c| c == '1').collect::<Vec<bool>>();
    let known =
        usize::from(pre) + usize::from(post) + if period > 0 { usize::from(blk) } else { 0 };
    Some(LinearBurst {
        payload_bytes: bytes,
        modulation: 0x01,
        p,
        q,
        y_pattern: pattern(y),
        w_pattern: pattern(w),
        preamble: pre,
        postamble: post,
        pilot_period: period,
        pilot_block: blk,
        pilot_sum: sum,
        uw: uw_symbols(uw, 2, known),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: &mut u64) -> f64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        (*seed >> 11) as f64 / (1u64 << 53) as f64
    }

    fn gauss(seed: &mut u64) -> f32 {
        let (u, v) = (rng(seed).max(1e-12), rng(seed));
        ((-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()) as f32
    }

    /// The burst through a channel: frequency (cycles a symbol), phase,
    /// gain, and noise at Es/N0 `snr_db`.
    fn channel(x: &[Iq], freq: f64, phase: f64, snr_db: f32, seed: &mut u64) -> Vec<Iq> {
        let sigma = (10f32.powf(-snr_db / 10.0) / 2.0).sqrt();
        x.iter()
            .enumerate()
            .map(|(n, &s)| {
                let rot = Iq::from_polar(
                    0.7,
                    (std::f64::consts::TAU * freq * n as f64 + phase) as f32,
                );
                s * rot + Iq::new(gauss(seed), gauss(seed)) * sigma
            })
            .collect()
    }

    fn pdu(len: usize, seed: &mut u64) -> Vec<u8> {
        (0..len).map(|_| (rng(seed) * 256.0) as u8).collect()
    }

    #[test]
    fn the_scrambler_is_dvb_s_s() {
        // DVB-S's PRBS from the same start begins 0000 0011 (EN 300 421
        // §4.4.1: the first bits of the sequence after initialisation).
        assert_eq!(scramble(&[0, 0]), [0x03, 0xF6]);
        assert_eq!(scramble(&scramble(b"return link")), b"return link");
    }

    #[test]
    fn crc16_checks_itself() {
        let mut b = b"123456789".to_vec();
        // CRC-16/UMTS (poly 8005h, from 0, unreflected) of "123456789".
        assert_eq!(crc16(&b), 0xFEE8);
        b.extend(crc16(&b).to_be_bytes());
        assert_eq!(crc16(&b), 0);
    }

    #[test]
    fn annex_a_bursts_are_as_long_as_table_a_1_says() {
        // (waveform, burst symbols, payload symbols)
        for (id, len, payload) in [
            (1, 664, 456),
            (2, 262, 168),
            (3, 536, 456),
            (4, 536, 472),
            (5, 536, 510),
            (6, 536, 512),
            (7, 536, 519),
        ] {
            let s = BurstShape::new(&reference(id).unwrap()).unwrap();
            assert_eq!(
                (s.len(), s.payload_symbols()),
                (len, payload),
                "waveform {id}"
            );
        }
    }

    #[test]
    fn annex_a_bursts_read_back_through_a_channel() {
        let mut seed = 0xDEC5A7;
        // Es/N0 a little over each rate's need.
        for (id, snr) in [
            (1, 2.0),
            (2, 2.0),
            (3, 2.0),
            (4, 4.0),
            (5, 6.5),
            (6, 7.5),
            (7, 9.0),
        ] {
            let s = BurstShape::new(&reference(id).unwrap()).unwrap();
            for crc in [Crc::Crc16, Crc::Crc32] {
                let msg = pdu(s.payload_bytes - crc.bytes(), &mut seed);
                let x = s.transmit(&msg, crc);
                let r = channel(&x, 0.004, 1.0, snr, &mut seed);
                let got = s.receive(&r).unwrap();
                assert_eq!(got.crc, Some(crc), "waveform {id}: {got:?}");
                assert_eq!(got.pdu, msg, "waveform {id}");
                assert!(
                    (got.freq - 0.004).abs() < 2e-4,
                    "waveform {id}: {}",
                    got.freq
                );
            }
        }
    }

    /// A burst type the hub defines, in `modulation`, rate 3/4.
    fn own(modulation: u8, seed: &mut u64) -> LinearBurst {
        let m = Modulation::from_code(modulation).unwrap().bits();
        LinearBurst {
            payload_bytes: 130,
            modulation,
            p: 31,
            q: [1, 4, 2, 3],
            y_pattern: vec![true, false, true, false, false, false],
            w_pattern: vec![false],
            preamble: 16,
            postamble: 8,
            pilot_period: 40,
            pilot_block: 4,
            pilot_sum: 40,
            uw: (0..28)
                .map(|_| (rng(seed) * (1 << m) as f64) as u8)
                .collect(),
        }
    }

    #[test]
    fn eight_psk_and_16qam_bursts_read_back() {
        let mut seed = 31337;
        for (modulation, snr) in [(0x02, 12.0), (0x03, 15.0), (0x05, 6.0)] {
            let s = BurstShape::new(&own(modulation, &mut seed)).unwrap();
            let msg = pdu(s.payload_bytes - 4, &mut seed);
            let x = s.transmit(&msg, Crc::Crc32);
            let r = channel(&x, -0.002, -2.0, snr, &mut seed);
            let got = s.receive(&r).unwrap();
            assert_eq!(got.crc, Some(Crc::Crc32), "{modulation:#x}: {got:?}");
            assert_eq!(got.pdu, msg);
        }
    }

    #[test]
    fn noise_alone_checks_no_crc() {
        let mut seed = 4;
        let s = BurstShape::new(&reference(3).unwrap()).unwrap();
        let r: Vec<Iq> = (0..s.len())
            .map(|_| Iq::new(gauss(&mut seed), gauss(&mut seed)))
            .collect();
        assert_eq!(s.receive(&r).unwrap().crc, None);
    }
}
