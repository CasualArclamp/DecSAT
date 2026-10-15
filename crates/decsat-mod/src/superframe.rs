//! Super-framed S2X signals (EN 302 307-2 Annex E, formats 0 and 1), for
//! test signals: PLFRAMEs built without pilot blocks of their own
//! ([`crate::PlFramer::superframed`]) laid into super-frames of 612 540
//! symbols, each opened by its SOSF and SFFI and, when switched on, broken
//! every 16 CUs by a type-A SF-pilot field.

use decsat_core::Iq;
use decsat_frame::superframe::{
    PILOT_AFTER, PILOT_PERIOD, SFL, SOSF_LEN, SfPart, SfScrambler, part, pilot_sequence,
    sffi_symbols, sosf_symbols,
};

/// BPSK on the diagonal: the ±1 sequences' +1.
const DIAG: Iq = Iq::new(
    std::f32::consts::FRAC_1_SQRT_2,
    std::f32::consts::FRAC_1_SQRT_2,
);

/// Lays PLFRAME symbols into super-frames.
pub struct Superframer {
    pilots: bool,
    /// The place in the current super-frame of the next symbol out.
    pos: usize,
    reference: SfScrambler,
    sosf: Vec<Iq>,
    sffi: Vec<Iq>,
    pilot: [f32; decsat_frame::superframe::PILOT_LEN],
}

impl Superframer {
    /// Super-frames of `format` (0 or 1 carry PLFRAMEs), SF-pilots on or off,
    /// SOSF and pilot sequence 0 and scrambling codes 0 (the defaults).
    pub fn new(format: u8, pilots: bool) -> Self {
        let reference = SfScrambler::new(0);
        let payload = SfScrambler::new(0);
        Superframer {
            pilots,
            pos: 0,
            sosf: sosf_symbols(0, &reference),
            sffi: sffi_symbols(format, &payload),
            pilot: pilot_sequence(0),
            reference,
        }
    }

    /// The symbol the super-frame has at the current place, unless it is a
    /// PLFRAME's.
    fn own_symbol(&self) -> Option<Iq> {
        let k = self.pos;
        match part(k, self.pilots) {
            SfPart::Payload => None,
            SfPart::Sosf => Some(self.sosf[k]),
            SfPart::Sffi => Some(self.sffi[k - SOSF_LEN]),
            SfPart::Pilot => {
                Some(DIAG * self.pilot[k % PILOT_PERIOD - PILOT_AFTER] * self.reference.factor(k))
            }
        }
    }

    fn advance(&mut self) {
        self.pos += 1;
        if self.pos == SFL {
            self.pos = 0;
        }
    }

    /// Lay `plframes` (whole PLFRAMEs' symbols, back to back) into the
    /// super-frames, appending what goes out to `out`.
    pub fn push(&mut self, plframes: &[Iq], out: &mut Vec<Iq>) {
        for &x in plframes {
            while let Some(s) = self.own_symbol() {
                out.push(s);
                self.advance();
            }
            out.push(x);
            self.advance();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decsat_frame::superframe::{HEAD_LEN, PILOT_LEN};

    #[test]
    fn super_frames_open_with_their_head_and_keep_the_payload_in_order() {
        let mut sf = Superframer::new(0, true);
        // Payload symbols far off the unit circle, to tell them apart.
        let payload: Vec<Iq> = (0..SFL).map(|k| Iq::new(10.0 + k as f32, 0.0)).collect();
        let mut out = Vec::new();
        sf.push(&payload, &mut out);
        // The SOSF and SFFI on the diagonal, scrambled: unit magnitude.
        assert!(
            out[..HEAD_LEN]
                .iter()
                .all(|v| (v.norm() - 1.0).abs() < 1e-5)
        );
        // The payload comes through whole and in order, between the
        // super-frame's own symbols.
        let through: Vec<Iq> = out.iter().copied().filter(|v| v.re >= 10.0).collect();
        assert_eq!(through, payload);
        // Overhead: the head and 415 pilot fields a super-frame.
        let own = out.len() - payload.len();
        assert!(own >= HEAD_LEN + 400 * PILOT_LEN, "{own}");
        // A second super-frame began exactly SFL symbols on.
        assert_eq!(&out[SFL..SFL + 10], &out[..10]);
        // Its first pilot field sits after 16 CUs.
        let pilot_start = PILOT_AFTER;
        assert!(
            out[pilot_start..pilot_start + PILOT_LEN]
                .iter()
                .all(|v| (v.norm() - 1.0).abs() < 1e-5)
        );
    }
}
