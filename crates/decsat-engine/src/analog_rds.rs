//! Experimental FM RDS: coherent 57-kHz extraction from 19-kHz pilot phase,
//! biphase chip integration, differential decoding, RDS CRC and basic groups.
const TAU: f64 = std::f64::consts::TAU;
const POLY: u16 = 0x5B9;
const OFFSETS: [u16; 5] = [0x0FC, 0x198, 0x168, 0x1B4, 0x350];
fn syndrome(mut v: u32) -> u16 {
    for bit in (10..=25).rev() {
        if v & (1 << bit) != 0 {
            v ^= (POLY as u32) << (bit - 10);
        }
    }
    (v & 0x3ff) as u16
}
pub struct RdsDecoder {
    lp: f32,
    chip_sum: f32,
    chip_clock: f64,
    chip_prev: Option<bool>,
    last_sign: bool,
    chip_phase_locked: bool,
    last_symbol: Option<bool>,
    bits: u32,
    bits_seen: usize,
    block: usize,
    words: [u16; 4],
    pi: Option<u16>,
    pty: Option<u8>,
    ps: [u8; 8],
    text: [u8; 64],
    groups: u64,
}
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
            words: [0; 4],
            pi: None,
            pty: None,
            ps: [0; 8],
            text: [0; 64],
            groups: 0,
        }
    }
}
impl RdsDecoder {
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
    pub fn feed(&mut self, multiplex: f32, pilot_phase: f64, rate: f64) {
        let carrier = (3.0 * pilot_phase).cos() as f32;
        let mixed = multiplex * carrier;
        let k = (TAU * 2_400.0 / rate).min(1.0) as f32;
        self.lp += k * (mixed - self.lp);
        // Manchester transitions provide a clock reference. Apply a small,
        // bounded phase correction instead of letting a free-running 2375-Hz
        // chip clock drift indefinitely relative to the transmitter.
        let sign = self.lp >= 0.0;
        if sign != self.last_sign && self.lp.abs() > 0.002 {
            let phase = self.chip_clock;
            // A Manchester mid-chip transition normally occurs at the
            // sampling boundary. Other transitions are accepted with lower
            // confidence; avoid large jumps which erase decoded groups.
            let error = if phase < 0.5 { -phase } else { 1.0 - phase };
            if error.abs() < 0.30 {
                self.chip_clock = (self.chip_clock + 0.05 * error).rem_euclid(1.0);
                self.chip_phase_locked = true;
            }
        }
        self.last_sign = sign;
        self.chip_sum += self.lp;
        self.chip_clock += 2375.0 / rate;
        if self.chip_clock >= 1.0 {
            self.chip_clock -= 1.0;
            let chip = self.chip_sum >= 0.0;
            self.chip_sum = 0.0;
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
    fn bit(&mut self, b: bool) {
        self.bits = (self.bits << 1) | u32::from(b);
        self.bits_seen += 1;
        if self.bits_seen < 26 {
            return;
        }
        let word = self.bits & 0x03ff_ffff;
        let wanted = if self.block == 2 {
            OFFSETS[2]
        } else {
            OFFSETS[self.block]
        };
        let crc = syndrome(word);
        if self.block == 0 {
            if crc != OFFSETS[0] {
                return;
            }
            self.words[0] = (word >> 10) as u16;
            self.block = 1;
            self.bits_seen = 0;
        } else if crc == wanted || (self.block == 2 && crc == OFFSETS[4]) {
            self.words[self.block] = (word >> 10) as u16;
            self.block += 1;
            self.bits_seen = 0;
            if self.block == 4 {
                self.group();
                self.block = 0;
            }
        } else {
            self.block = 0;
            self.bits_seen = 0;
        }
    }
    fn group(&mut self) {
        self.groups += 1;
        self.pi = Some(self.words[0]);
        let b = self.words[1];
        self.pty = Some(((b >> 5) & 0x1f) as u8);
        let typ = (b >> 12) & 0xf;
        let ver = (b >> 11) & 1;
        let addr = (b & 0xf) as usize;
        if typ == 0 && addr < 4 {
            self.ps[addr * 2] = (self.words[3] >> 8) as u8;
            self.ps[addr * 2 + 1] = self.words[3] as u8;
        } else if typ == 2 {
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
