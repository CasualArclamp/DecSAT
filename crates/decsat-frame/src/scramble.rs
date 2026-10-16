// made by CasualArclamp
//! Physical-layer scrambling (ETSI EN 302 307-1 §5.5.4).
//!
//! The PLFRAME **payload** is randomised by rotating each symbol by a multiple
//! of 90°, drawn from a Gold-code sequence. This spreads the spectrum and stops
//! a repetitive payload producing discrete spectral lines.
//!
//! Two details matter and are easy to get wrong:
//!
//! * The **PLHEADER is not scrambled** — it has to be readable before anything
//!   else is known, and it has its own scrambler on the PLS code.
//! * **Pilot blocks *are* scrambled**, so the payload here means everything
//!   after the PLHEADER, pilots included.
//!
//! The sequence depends only on the gold-code index, so it is computed once up
//! front. Because every factor is exactly ±1 or ±j, scrambling is exact in
//! floating point and a scramble/descramble round trip is bit-identical.
//!
//! Sequence generator ported from `gr-dvbs2rx`'s `lib/pl_descrambler.cc`, which
//! in turn takes it from `gr-dtv`'s `dvbs2_physical_cc_impl.cc` (both GPL-3).

use decsat_core::Iq;

use crate::defs::MAX_PLFRAME_PAYLOAD;

/// Highest gold-code index the standard allows (§5.5.4: n = 0..=262141).
pub const MAX_GOLD_CODE: u32 = 262_141;

/// Parity of the bits of `a & b`, over the 18-bit LFSR width.
#[inline]
fn parity_chk(a: u32, b: u32) -> u32 {
    (a & b & 0x3_FFFF).count_ones() & 1
}

/// Pre-computed PL scrambling sequence for one gold code.
pub struct PlScrambler {
    /// Descrambling factors: the conjugate of the transmit-side rotation.
    /// Always one of `1, -j, -1, +j`.
    descramble_seq: Vec<Iq>,
    gold_code: u32,
}

impl PlScrambler {
    /// Build the sequence for `gold_code` (0 is the default sequence).
    ///
    /// # Panics
    /// If `gold_code > MAX_GOLD_CODE`.
    pub fn new(gold_code: u32) -> Self {
        assert!(
            gold_code <= MAX_GOLD_CODE,
            "gold code must be at most {MAX_GOLD_CODE}"
        );

        // Descrambling multiplies by exp(-j·Rn·π/2).
        const LUT: [Iq; 4] = [
            Iq::new(1.0, 0.0),
            Iq::new(0.0, -1.0),
            Iq::new(-1.0, 0.0),
            Iq::new(0.0, 1.0),
        ];

        let mut x: u32 = 0x0_0001;
        let mut y: u32 = 0x3_FFFF;

        // The gold-code index selects a starting point by advancing the x
        // register that many steps; y always starts from all-ones.
        for _ in 0..gold_code {
            let xb = parity_chk(x, 0x0081);
            x >>= 1;
            if xb != 0 {
                x |= 0x2_0000;
            }
        }

        let mut descramble_seq = Vec::with_capacity(MAX_PLFRAME_PAYLOAD);
        for _ in 0..MAX_PLFRAME_PAYLOAD {
            let xa = parity_chk(x, 0x8050);
            let xb = parity_chk(x, 0x0081);
            let xc = x & 1;
            x >>= 1;
            if xb != 0 {
                x |= 0x2_0000;
            }

            let ya = parity_chk(y, 0x04A1);
            let yb = parity_chk(y, 0xFF60);
            let yc = y & 1;
            y >>= 1;
            if ya != 0 {
                y |= 0x2_0000;
            }

            // Rn in 0..=3 selects the quadrant rotation.
            let rn = (((xa ^ yb) << 1) + (xc ^ yc)) as usize;
            descramble_seq.push(LUT[rn]);
        }

        PlScrambler {
            descramble_seq,
            gold_code,
        }
    }

    pub fn gold_code(&self) -> u32 {
        self.gold_code
    }

    /// The rotation index `Rn` for payload symbol `i`, in 0..=3. Mostly useful
    /// for tests and for cross-checking against the standard's tables.
    pub fn rn(&self, i: usize) -> u8 {
        let d = self.descramble_seq[i];
        // Invert the LUT: 1 -> 0, -j -> 1, -1 -> 2, +j -> 3.
        match (d.re as i32, d.im as i32) {
            (1, 0) => 0,
            (0, -1) => 1,
            (-1, 0) => 2,
            _ => 3,
        }
    }

    /// The receive factor for payload symbol `i` (multiply by it to
    /// descramble; the transmit factor is its conjugate).
    pub fn factor(&self, i: usize) -> Iq {
        self.descramble_seq[i]
    }

    /// Undo scrambling in place. `payload` starts at the first symbol after the
    /// PLHEADER and includes pilot blocks.
    ///
    /// # Panics
    /// If `payload` is longer than [`MAX_PLFRAME_PAYLOAD`].
    pub fn descramble(&self, payload: &mut [Iq]) {
        assert!(
            payload.len() <= MAX_PLFRAME_PAYLOAD,
            "payload longer than any legal PLFRAME"
        );
        for (s, &d) in payload.iter_mut().zip(&self.descramble_seq) {
            *s *= d;
        }
    }

    /// Apply scrambling in place — the transmit side, for the modulator.
    ///
    /// # Panics
    /// If `payload` is longer than [`MAX_PLFRAME_PAYLOAD`].
    pub fn scramble(&self, payload: &mut [Iq]) {
        assert!(
            payload.len() <= MAX_PLFRAME_PAYLOAD,
            "payload longer than any legal PLFRAME"
        );
        for (s, &d) in payload.iter_mut().zip(&self.descramble_seq) {
            // The transmit factor is the conjugate of the receive factor.
            *s *= d.conj();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_is_exact() {
        // Every factor is +-1 or +-j, so this must be bit-identical, not merely
        // close -- which is worth asserting exactly.
        let sc = PlScrambler::new(0);
        let original: Vec<Iq> = (0..5000)
            .map(|k| Iq::new(k as f32 * 0.001 - 2.0, 1.5 - k as f32 * 0.0007))
            .collect();

        let mut buf = original.clone();
        sc.scramble(&mut buf);
        assert_ne!(buf, original, "scrambling changed nothing");
        sc.descramble(&mut buf);
        assert_eq!(buf, original, "round trip was not exact");
    }

    #[test]
    fn factors_are_unit_quadrant_rotations() {
        let sc = PlScrambler::new(1234);
        for i in [0usize, 1, 2, 91, 1000, MAX_PLFRAME_PAYLOAD - 1] {
            assert!(sc.rn(i) < 4);
            let d = sc.descramble_seq[i];
            assert!((d.norm() - 1.0).abs() < 1e-9);
            // Exactly on an axis: one component zero, the other +-1.
            assert!(
                (d.re == 0.0 && d.im.abs() == 1.0) || (d.im == 0.0 && d.re.abs() == 1.0),
                "factor {d} is not a quadrant rotation"
            );
        }
    }

    #[test]
    fn sequence_covers_the_full_payload_length() {
        let sc = PlScrambler::new(0);
        assert_eq!(sc.descramble_seq.len(), MAX_PLFRAME_PAYLOAD);
        // The longest legal PLFRAME is normal QPSK with pilots.
        assert_eq!(MAX_PLFRAME_PAYLOAD, 360 * 90 + 22 * 36);
    }

    #[test]
    fn different_gold_codes_give_different_sequences() {
        let a = PlScrambler::new(0);
        let b = PlScrambler::new(1);
        let c = PlScrambler::new(16_384);
        let first = |s: &PlScrambler| (0..64).map(|i| s.rn(i)).collect::<Vec<_>>();
        assert_ne!(first(&a), first(&b));
        assert_ne!(first(&a), first(&c));
        assert_ne!(first(&b), first(&c));
    }

    #[test]
    fn descrambling_another_codes_scrambling_fails() {
        // A receiver on the wrong gold code must not recover the payload --
        // this is how multiple carriers share a transponder.
        let tx = PlScrambler::new(0);
        let rx = PlScrambler::new(7);
        let original: Vec<Iq> = (0..1000).map(|k| Iq::new(1.0, k as f32 * 0.01)).collect();
        let mut buf = original.clone();
        tx.scramble(&mut buf);
        rx.descramble(&mut buf);
        assert_ne!(buf, original);
    }

    #[test]
    fn rotation_indexes_are_roughly_balanced() {
        // A Gold-code sequence should visit the four quadrants about equally;
        // a badly wired LFSR usually shows up as a gross imbalance.
        let sc = PlScrambler::new(0);
        let mut counts = [0usize; 4];
        for i in 0..MAX_PLFRAME_PAYLOAD {
            counts[sc.rn(i) as usize] += 1;
        }
        let expected = MAX_PLFRAME_PAYLOAD as f64 / 4.0;
        for (r, &c) in counts.iter().enumerate() {
            let ratio = c as f64 / expected;
            assert!(
                (0.9..1.1).contains(&ratio),
                "Rn={r} appeared {c} times, expected about {expected:.0}"
            );
        }
    }

    #[test]
    fn x_register_has_the_full_maximal_period() {
        // The x LFSR is an 18-bit maximal-length register, so it must return to
        // its seed after exactly 2^18 - 1 steps and not before.
        let step = |x: &mut u32| {
            let xb = parity_chk(*x, 0x0081);
            *x >>= 1;
            if xb != 0 {
                *x |= 0x2_0000;
            }
        };
        const PERIOD: u32 = (1 << 18) - 1;
        let mut x = 0x0_0001u32;
        let mut first_return = None;
        for n in 1..=PERIOD {
            step(&mut x);
            if x == 0x0_0001 {
                first_return = Some(n);
                break;
            }
        }
        assert_eq!(first_return, Some(PERIOD), "x LFSR is not maximal length");
    }

    #[test]
    fn scrambling_preserves_symbol_magnitudes() {
        // Scrambling only rotates, so it must not change any amplitude -- if it
        // did, it would interfere with AGC and the demapper's ring decisions.
        let sc = PlScrambler::new(99);
        let original: Vec<Iq> = (0..2000)
            .map(|k| Iq::new((k % 7) as f32 * 0.3, (k % 5) as f32 * 0.4))
            .collect();
        let mut buf = original.clone();
        sc.scramble(&mut buf);
        for (a, b) in original.iter().zip(&buf) {
            assert!((a.norm() - b.norm()).abs() < 1e-6);
        }
    }
}
