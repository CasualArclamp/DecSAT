//! Experimental FM RDS: coherent 57-kHz extraction from 19-kHz pilot phase,
//! biphase chip integration, differential decoding, RDS CRC and basic groups.
//!
//! Fed one multiplex sample at a time by the WFM demodulator
//! (`analog::AnalogReceiver`), together with that demodulator's recovered pilot
//! phase. Decodes only the programme-service name (PS, groups 0A/0B) and
//! Radiotext (RT, groups 2A/2B), plus the PI code and programme type, and
//! reports them through [`RdsDecoder::summary`].
//!
//! Framing is from the RDS standard (IEC 62106 / CENELEC EN 50067): a group is
//! four 26-bit blocks, each 16 data bits and a 10-bit checkword, sent at
//! 1187.5 bit/s differentially encoded and biphase (Manchester) coded onto a
//! 57 kHz subcarrier, which is the third harmonic of the 19 kHz stereo pilot.
//!
//! Provenance: merged in from the "VMA" snapshot, see `docs/STATUS.md`
//! (2026-10-25).

const TAU: f64 = std::f64::consts::TAU;

/// The RDS subcarrier is this multiple of the 19 kHz pilot (57 kHz).
const SUBCARRIER_HARMONIC: f64 = 3.0;
/// Biphase chips a second: two per data bit of 1187.5 bit/s.
const CHIP_RATE_HZ: f64 = 2375.0;
/// Corner of the one-pole low-pass that follows the 57 kHz mixer, in Hz.
const BASEBAND_LOWPASS_HZ: f64 = 2_400.0;
/// A sign change of the filtered signal is used as a clock reference only if it
/// is at least this large (rejects noise around zero).
const CLOCK_EDGE_MIN_LEVEL: f32 = 0.002;
/// Fraction of the measured clock error corrected per accepted edge, and the
/// largest error (in chips) accepted as a real transition.
const CLOCK_CORRECTION_GAIN: f64 = 0.05;
const CLOCK_ERROR_LIMIT: f64 = 0.30;

/// Bits in one block: 16 information bits and a 10-bit checkword.
const BLOCK_BITS: usize = 26;
const CHECK_BITS: u32 = 10;
const BLOCK_MASK: u32 = (1 << BLOCK_BITS) - 1;
/// Blocks in a group (A, B, C or C', D).
const BLOCKS_PER_GROUP: usize = 4;

/// Generator polynomial of the RDS block code, x^10 + x^8 + x^7 + x^5 + x^4 +
/// x^3 + 1, without the leading x^10 term.
const POLY: u16 = 0x5B9;
/// Offset words added to the checkword of each block type so a receiver can
/// tell which block it has found: A, B, C, D, then C' (used in place of C in
/// version-B groups).
const OFFSETS: [u16; 5] = [0x0FC, 0x198, 0x168, 0x1B4, 0x350];
/// Index of C' in [`OFFSETS`].
const OFFSET_C_PRIME: usize = 4;

/// Characters in the programme-service name and in Radiotext.
const PS_LEN: usize = 8;
const TEXT_LEN: usize = 64;

/// Remainder of `v` (a 26-bit block, checkword included) after dividing by the
/// generator polynomial. For a valid block this equals that block's offset word.
fn syndrome(mut v: u32) -> u16 {
    for bit in (CHECK_BITS..BLOCK_BITS as u32).rev() {
        if v & (1 << bit) != 0 {
            v ^= (POLY as u32) << (bit - CHECK_BITS);
        }
    }
    (v & 0x3ff) as u16
}

/// RDS decoder state.
pub struct RdsDecoder {
    /// Output of the low-pass after the 57 kHz mixer.
    lp: f32,
    /// Sum of the filtered signal over the current chip.
    chip_sum: f32,
    /// Position within the current chip, 0 to 1.
    chip_clock: f64,
    /// The first chip of a pair, waiting for its partner.
    chip_prev: Option<bool>,
    last_sign: bool,
    chip_phase_locked: bool,
    /// The previous biphase symbol, for differential decoding.
    last_symbol: Option<bool>,
    /// The most recent bits, newest in bit 0.
    bits: u32,
    bits_seen: usize,
    /// Which block of the group is expected next (0 = A).
    block: usize,
    /// Information words of the blocks received so far in this group.
    words: [u16; BLOCKS_PER_GROUP],
    pi: Option<u16>,
    pty: Option<u8>,
    ps: [u8; PS_LEN],
    text: [u8; TEXT_LEN],
    groups: u64,
}

// Rust note: `#[derive(Default)]` is not available here because `[u8; 64]`
// does not implement `Default` (the standard library stops at 32 elements).
impl Default for RdsDecoder {
    fn default() -> Self {
        Self {
            lp: 0.0,
            chip_sum: 0.0,
            chip_clock: 0.0,
            chip_prev: None,
            last_sign: false,
            chip_phase_locked: false,
            last_symbol: None,
            bits: 0,
            bits_seen: 0,
            block: 0,
            words: [0; BLOCKS_PER_GROUP],
            pi: None,
            pty: None,
            ps: [0; PS_LEN],
            text: [0; TEXT_LEN],
            groups: 0,
        }
    }
}

impl RdsDecoder {
    /// One line for the status bar: the station identity once a group has been
    /// received, otherwise a search message.
    pub fn summary(&self) -> String {
        if let Some(pi) = self.pi {
            let ps = String::from_utf8_lossy(&self.ps)
                .trim_matches('\0')
                .trim()
                .to_string();
            let text = String::from_utf8_lossy(&self.text)
                .trim_matches('\0')
                .trim()
                .to_string();
            format!(
                "PI={pi:04X} PTY={} PS={} RT={} ({} groups)",
                self.pty.unwrap_or(0),
                ps,
                text,
                self.groups
            )
        } else {
            "searching for RDS groups".to_string()
        }
    }

    /// Feed one FM multiplex sample at `rate` S/s, with the stereo decoder's
    /// current 19 kHz pilot phase (radians).
    pub fn feed(&mut self, multiplex: f32, pilot_phase: f64, rate: f64) {
        // Mix the 57 kHz subcarrier down to baseband using the pilot's phase,
        // then low-pass it.
        let carrier = (SUBCARRIER_HARMONIC * pilot_phase).cos() as f32;
        let mixed = multiplex * carrier;
        let k = (TAU * BASEBAND_LOWPASS_HZ / rate).min(1.0) as f32;
        self.lp += k * (mixed - self.lp);
        // Manchester transitions provide a clock reference. Apply a small,
        // bounded phase correction instead of letting a free-running 2375-Hz
        // chip clock drift indefinitely relative to the transmitter.
        let sign = self.lp >= 0.0;
        if sign != self.last_sign && self.lp.abs() > CLOCK_EDGE_MIN_LEVEL {
            let phase = self.chip_clock;
            // A Manchester mid-chip transition normally occurs at the
            // sampling boundary. Other transitions are accepted with lower
            // confidence; avoid large jumps which erase decoded groups.
            let error = if phase < 0.5 { -phase } else { 1.0 - phase };
            if error.abs() < CLOCK_ERROR_LIMIT {
                self.chip_clock = (self.chip_clock + CLOCK_CORRECTION_GAIN * error).rem_euclid(1.0);
                self.chip_phase_locked = true;
            }
        }
        self.last_sign = sign;
        self.chip_sum += self.lp;
        self.chip_clock += CHIP_RATE_HZ / rate;
        if self.chip_clock >= 1.0 {
            // A chip is complete: decide its sign.
            self.chip_clock -= 1.0;
            let chip = self.chip_sum >= 0.0;
            self.chip_sum = 0.0;
            // Rust note: `Option::take` empties `chip_prev` and hands back what
            // was in it, so it has to be put back explicitly where it is kept.
            if let Some(prev) = self.chip_prev.take() {
                // The two biphase chips should have opposite sign.
                if chip == prev {
                    // Invalid Manchester pair: re-use the last chip as a
                    // possible first chip of the opposite phase alignment.
                    self.chip_prev = Some(chip);
                    self.last_symbol = None;
                    self.bits_seen = 0;
                    self.block = 0;
                    return;
                }
                // The pair is valid; the second chip is the biphase symbol, and
                // the data bit is the change from the previous symbol (the
                // stream is differentially encoded).
                let symbol = chip;
                if let Some(last) = self.last_symbol {
                    self.bit(symbol ^ last);
                }
                self.last_symbol = Some(symbol);
            } else {
                self.chip_prev = Some(chip);
            }
        }
    }

    /// Shift in one decoded bit; every 26 bits, test whether they form the next
    /// expected block.
    fn bit(&mut self, b: bool) {
        self.bits = (self.bits << 1) | u32::from(b);
        self.bits_seen += 1;
        if self.bits_seen < BLOCK_BITS {
            return;
        }
        let word = self.bits & BLOCK_MASK;
        let wanted = OFFSETS[self.block];
        let crc = syndrome(word);
        if self.block == 0 {
            // Looking for the start of a group: keep sliding one bit at a time
            // until a block A checks.
            if crc != OFFSETS[0] {
                return;
            }
            self.words[0] = (word >> CHECK_BITS) as u16;
            self.block = 1;
            self.bits_seen = 0;
        } else if crc == wanted || (self.block == 2 && crc == OFFSETS[OFFSET_C_PRIME]) {
            // (The third block may be C, or C' in a version-B group.)
            self.words[self.block] = (word >> CHECK_BITS) as u16;
            self.block += 1;
            self.bits_seen = 0;
            if self.block == BLOCKS_PER_GROUP {
                self.group();
                self.block = 0;
            }
        } else {
            // A block failed its check: drop the group and hunt for block A again.
            self.block = 0;
            self.bits_seen = 0;
        }
    }

    /// A whole group has been received: pick out the programme identity, the
    /// programme type and the PS / Radiotext characters it carries.
    fn group(&mut self) {
        self.groups += 1;
        self.pi = Some(self.words[0]);
        // Block B: group type (4 bits), version A/B (1 bit), traffic programme
        // (1 bit), programme type (5 bits), then 5 bits whose meaning depends on
        // the group type.
        //
        // NOTE: `addr` takes the low four bits as a segment address. That is
        // right for group 2, but in group 0 only the lowest two are the address
        // (bit 3 is the music/speech flag, bit 2 the decoder-identification
        // bit), so the `addr < 4` test below rejects 0A/0B groups whenever those
        // are set. Left as it was: this restyle does not change behaviour.
        let b = self.words[1];
        self.pty = Some(((b >> 5) & 0x1f) as u8);
        let typ = (b >> 12) & 0xf;
        let ver = (b >> 11) & 1;
        let addr = (b & 0xf) as usize;
        if typ == 0 && addr < 4 {
            // Group 0: two PS characters per segment, in block D.
            self.ps[addr * 2] = (self.words[3] >> 8) as u8;
            self.ps[addr * 2 + 1] = self.words[3] as u8;
        } else if typ == 2 {
            // Group 2: Radiotext. Version A carries four characters per segment
            // (blocks C and D), version B two (block D only).
            if ver == 0 && addr < 16 {
                let i = addr * 4;
                self.text[i] = (self.words[2] >> 8) as u8;
                self.text[i + 1] = self.words[2] as u8;
                self.text[i + 2] = (self.words[3] >> 8) as u8;
                self.text[i + 3] = self.words[3] as u8;
            } else if ver == 1 && addr < 16 {
                let i = addr * 2;
                self.text[i] = (self.words[3] >> 8) as u8;
                self.text[i + 1] = self.words[3] as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parity_syndrome() {
        assert_eq!(syndrome(0), 0);
        // Multiplying generator by a polynomial gives zero syndrome.
        assert_eq!(syndrome((POLY as u32) << 10), 0);
    }
}
