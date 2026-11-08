// made by CasualArclamp
//! Raw IQ for dumb mode, packed to 4–16 bits a component with block floating
//! point: every [`BLOCK`] samples carry their own gain, so a weak signal uses
//! the whole word.
//!
//! Payload: `u64 first_sample`, `u32 samples`, `u8 bits`, one signed gain
//! code per block, then the components (I then Q, two's complement) packed
//! MSB first with no padding except at the end of the frame.
//!
//! A gain code `c` means `G = 2^(c/4)`, and the integer on the wire is
//! `x·G·2^(bits-1)`, saturated.
//!
//! - At 8 bits and more `c` is a multiple of 4, chosen from the block's peak
//!   so nothing clips. A power of two keeps integer sources exact: a HackRF's
//!   `v/128` at 8 bits comes back as itself.
//! - Below 8 bits the peak would waste most of the word (Gaussian noise peaks
//!   well past 3σ), so `c` loads full scale at [`LOAD_SIGMA`]·σ, rounded down
//!   to the 1.5 dB step, unless the block's own peak allows a higher gain.
//!   A few percent of noise peaks then saturate, deliberately.

use decsat_core::Iq;

/// Samples sharing one gain code.
pub const BLOCK: usize = 1024;
/// Full scale in standard deviations below 8 bits.
pub const LOAD_SIGMA: f32 = 3.5;
/// The sample widths offered.
pub const BITS: [u8; 6] = [4, 6, 8, 10, 12, 16];
const HEADER: usize = 13;

/// The gain code for one block.
fn gain_code(block: &[Iq], bits: u8) -> i8 {
    let mut peak = 0f32;
    let mut power = 0f32;
    for s in block {
        peak = peak.max(s.re.abs()).max(s.im.abs());
        power += s.re * s.re + s.im * s.im;
    }
    if peak <= 0.0 || !peak.is_finite() {
        return 0;
    }
    if bits >= 8 {
        return ((-peak.log2()).floor() * 4.0).clamp(-124.0, 124.0) as i8;
    }
    let sigma = (power / (2 * block.len()) as f32).sqrt();
    let by_load = -(LOAD_SIGMA * sigma).log2();
    let by_peak = -peak.log2();
    ((by_load.max(by_peak) * 4.0).floor()).clamp(-127.0, 127.0) as i8
}

#[inline]
fn gain_of(code: i8) -> f32 {
    (f32::from(code) / 4.0).exp2()
}

/// Pack `samples`, which start at `first_sample` in the stream, into `out`.
pub fn pack(samples: &[Iq], first_sample: u64, bits: u8, out: &mut Vec<u8>) {
    debug_assert!(BITS.contains(&bits));
    out.clear();
    out.reserve(HEADER + samples.len().div_ceil(BLOCK) + samples.len() * bits as usize / 4 + 8);
    out.extend_from_slice(&first_sample.to_le_bytes());
    out.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    out.push(bits);
    let codes: Vec<i8> = samples.chunks(BLOCK).map(|b| gain_code(b, bits)).collect();
    out.extend(codes.iter().map(|&c| c as u8));

    let full = (1i32 << (bits - 1)) as f32;
    let (lo, hi) = (-(1i32 << (bits - 1)), (1i32 << (bits - 1)) - 1);
    let mut acc = 0u64;
    let mut have = 0u32;
    for (block, &code) in samples.chunks(BLOCK).zip(&codes) {
        let g = gain_of(code) * full;
        for s in block {
            for x in [s.re, s.im] {
                let v = ((x * g).round() as i32).clamp(lo, hi);
                match bits {
                    8 => out.push(v as u8),
                    16 => out.extend_from_slice(&(v as i16).to_be_bytes()),
                    _ => {
                        acc = (acc << bits) | (v as u64 & ((1u64 << bits) - 1));
                        have += u32::from(bits);
                        while have >= 8 {
                            have -= 8;
                            out.push((acc >> have) as u8);
                        }
                    }
                }
            }
        }
    }
    if have > 0 {
        out.push((acc << (8 - have)) as u8);
    }
}

/// A packed frame's first sample and sample count, or `None` if it is short.
pub fn peek(payload: &[u8]) -> Option<(u64, usize)> {
    if payload.len() < HEADER {
        return None;
    }
    Some((
        u64::from_le_bytes(payload[..8].try_into().ok()?),
        u32::from_le_bytes(payload[8..12].try_into().ok()?) as usize,
    ))
}

/// Append a packed frame's samples to `out`. `None` if it is malformed.
pub fn unpack(payload: &[u8], out: &mut Vec<Iq>) -> Option<()> {
    let (_, n) = peek(payload)?;
    let bits = payload[12];
    if !BITS.contains(&bits) {
        return None;
    }
    let blocks = n.div_ceil(BLOCK);
    let codes = payload.get(HEADER..HEADER + blocks)?;
    let data = &payload[HEADER + blocks..];
    if data.len() < (n * 2 * bits as usize).div_ceil(8) {
        return None;
    }
    out.reserve(n);
    let full = (1i32 << (bits - 1)) as f32;
    let shift = 32 - u32::from(bits);
    let mut at = 0usize;
    let mut acc = 0u64;
    let mut have = 0u32;
    // Rust note: a closure capturing the bit reader's state by mutable
    // reference, so the loop below can call it for I and for Q.
    let mut next = || -> i32 {
        match bits {
            8 => {
                let v = data[at] as i8 as i32;
                at += 1;
                v
            }
            16 => {
                let v = i16::from_be_bytes([data[at], data[at + 1]]) as i32;
                at += 2;
                v
            }
            _ => {
                while have < u32::from(bits) {
                    acc = (acc << 8) | u64::from(data[at]);
                    at += 1;
                    have += 8;
                }
                have -= u32::from(bits);
                // Sign-extend by moving the field to the top of an i32.
                (((acc >> have) as u32) << shift) as i32 >> shift
            }
        }
    };
    let mut left = n;
    for &c in codes {
        let g = 1.0 / (gain_of(c as i8) * full);
        let m = left.min(BLOCK);
        left -= m;
        for _ in 0..m {
            let re = next() as f32 * g;
            let im = next() as f32 * g;
            out.push(Iq::new(re, im));
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic Gaussian samples (Box–Muller over an LCG).
    fn noise(n: usize, sigma: f32) -> Vec<Iq> {
        let mut x = 12345u32;
        let mut u = move || {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            (x >> 8) as f32 / (1u32 << 24) as f32
        };
        (0..n)
            .map(|_| {
                let (a, b) = (u().max(1e-7), u());
                let r = sigma * (-2.0 * a.ln()).sqrt();
                let t = std::f32::consts::TAU * b;
                Iq::new(r * t.cos(), r * t.sin())
            })
            .collect()
    }

    fn snr_db(a: &[Iq], b: &[Iq]) -> f32 {
        let s: f32 = a.iter().map(|x| x.norm_sqr()).sum();
        let e: f32 = a.iter().zip(b).map(|(x, y)| (x - y).norm_sqr()).sum();
        10.0 * (s / e).log10()
    }

    fn trip(a: &[Iq], bits: u8) -> Vec<Iq> {
        let (mut p, mut o) = (Vec::new(), Vec::new());
        pack(a, 42, bits, &mut p);
        assert_eq!(peek(&p), Some((42, a.len())));
        assert_eq!(
            p.len(),
            13 + a.len().div_ceil(BLOCK) + (a.len() * 2 * bits as usize).div_ceil(8)
        );
        unpack(&p, &mut o).unwrap();
        assert_eq!(o.len(), a.len());
        o
    }

    #[test]
    fn eight_bit_integers_come_back_exactly() {
        // A HackRF's samples, v/128, at a level well under full scale.
        let a: Vec<Iq> = (0..5000i32)
            .map(|i| {
                let v = ((i * 7919) % 41 - 20) as f32;
                Iq::new(v / 128.0, -v / 128.0)
            })
            .collect();
        assert_eq!(trip(&a, 8), a);
    }

    /// Measured signal-to-quantisation-noise on Gaussian input; the floors
    /// follow from the step: 6.02 dB a bit, less the loading.
    #[test]
    fn quantisation_noise_on_gaussian_input() {
        let a = noise(8192, 0.013);
        for (bits, floor) in [
            (4, 13.0),
            (6, 26.0),
            (8, 26.0),
            (10, 38.0),
            (12, 50.0),
            (16, 74.0),
        ] {
            let snr = snr_db(&a, &trip(&a, bits));
            assert!(snr >= floor, "{bits} bits: {snr:.1} dB < {floor}");
        }
    }

    #[test]
    fn blocks_scale_independently() {
        // A loud block after a quiet one: the quiet one keeps its precision.
        let mut a = noise(BLOCK, 0.001);
        a.extend(noise(BLOCK, 0.5));
        let b = trip(&a, 8);
        assert!(snr_db(&a[..BLOCK], &b[..BLOCK]) > 26.0);
    }

    #[test]
    fn partial_blocks_silence_and_nonsense() {
        assert_eq!(
            trip(&vec![Iq::new(0.0, 0.0); 100], 6),
            vec![Iq::new(0.0, 0.0); 100]
        );
        assert!(trip(&[], 12).is_empty());
        let (mut p, mut o) = (Vec::new(), Vec::new());
        pack(&noise(100, 0.1), 0, 12, &mut p);
        p.truncate(40);
        assert!(unpack(&p, &mut o).is_none());
    }
}
