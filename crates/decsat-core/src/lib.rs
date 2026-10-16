// made by CasualArclamp
//! Shared types for DecSAT: modulation/coding tables, frame parameters,
//! configuration, metrics and error types. No DSP or I/O lives here — this crate
//! is the vocabulary every other crate speaks.
//!
//! References: ETSI EN 302 307-1 (DVB-S2) and EN 302 307-2 (DVB-S2X). See
//! `docs/DESIGN.md` for scope.

pub mod crc;
pub mod modcod;

pub use modcod::{
    CodeRate, FecFrame, Modcod, Modulation, modcod, modcod_name, qef_es_n0_db, s2_modcod,
    s2_modcod_table, s2x_modcod_table,
};

use num_complex::Complex32;

/// Baseband complex sample. `f32` is plenty for 8-bit HackRF input and keeps the
/// FFTs cache-friendly on the target CPU.
pub type Iq = Complex32;

/// Pulse-shaping roll-off factors allowed by DVB-S2 (0.35/0.25/0.20) and the
/// tighter ones added by S2X (0.15/0.10/0.05).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RollOff {
    R35,
    R25,
    R20,
    R15,
    R10,
    R05,
}

impl RollOff {
    /// The roll-off as a fraction (e.g. [`RollOff::R20`] -> 0.20).
    pub const fn as_f64(self) -> f64 {
        match self {
            RollOff::R35 => 0.35,
            RollOff::R25 => 0.25,
            RollOff::R20 => 0.20,
            RollOff::R15 => 0.15,
            RollOff::R10 => 0.10,
            RollOff::R05 => 0.05,
        }
    }

    /// All roll-offs, widest first. Handy for a blind acquisition sweep.
    pub const ALL: [RollOff; 6] = [
        RollOff::R35,
        RollOff::R25,
        RollOff::R20,
        RollOff::R15,
        RollOff::R10,
        RollOff::R05,
    ];
}

/// Sample format of an interleaved-IQ source or file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// Signed 8-bit I, 8-bit Q — the HackRF's native format.
    Cs8,
    /// Unsigned 8-bit I/Q, offset 128 — the RTL-SDR's native format, and
    /// 8-bit WAV.
    Cu8,
    /// Signed 16-bit I/Q (little-endian).
    Cs16,
    /// Signed 24-bit I/Q (little-endian, packed) — 24-bit WAV.
    Cs24,
    /// 32-bit float I/Q.
    Cf32,
}

impl SampleFormat {
    /// Bytes per complex sample.
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            SampleFormat::Cs8 | SampleFormat::Cu8 => 2,
            SampleFormat::Cs16 => 4,
            SampleFormat::Cs24 => 6,
            SampleFormat::Cf32 => 8,
        }
    }
}

/// Front-end / demodulator configuration shared by the CLI and GUI.
#[derive(Debug, Clone)]
pub struct RxConfig {
    /// Sample rate at the ADC / in the file, in Hz.
    pub sample_rate: f64,
    /// Nominal symbol rate, in symbols/s. `None` = unknown (acquire blindly).
    pub symbol_rate: Option<f64>,
    /// RF centre frequency in Hz (for display / HackRF tuning).
    pub center_freq: f64,
    /// Expected roll-off, or `None` to sweep [`RollOff::ALL`].
    pub roll_off: Option<RollOff>,
    /// Physical-layer scrambler gold-code index (0 for the default sequence).
    pub gold_code: u32,
    /// Keep only this input-stream identifier (ISI) in multistream mode.
    pub stream_filter: Option<u8>,
}

impl Default for RxConfig {
    fn default() -> Self {
        RxConfig {
            sample_rate: 2_000_000.0,
            symbol_rate: None,
            center_freq: 0.0,
            roll_off: None,
            gold_code: 0,
            stream_filter: None,
        }
    }
}

/// Live receiver metrics for the GUI/CLI. Filled in progressively as milestones land.
#[derive(Debug, Clone, Copy, Default)]
pub struct Metrics {
    /// Estimated carrier-to-noise ratio in dB (frame SNR).
    pub cnr_db: f32,
    /// Fractional frequency offset estimate (of the sample rate).
    pub freq_offset: f32,
    /// MODCOD of the most recently decoded PLFRAME, if locked.
    pub modcod: Option<Modcod>,
    /// Whether PL synchronisation is currently held.
    pub locked: bool,
    /// LDPC-corrected / total frames seen since start.
    pub frames_ok: u64,
    pub frames_total: u64,
}

/// Errors surfaced across the DecSAT crates.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported or unknown MODCOD index {0}")]
    UnknownModcod(u8),
    #[error("not yet implemented: {0}")]
    Unimplemented(&'static str),
    #[error("{0}")]
    Other(String),
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Convert one interleaved-IQ byte buffer into complex samples, normalised to
/// roughly unit scale. Used by the file reader and (later) the HackRF source.
pub fn bytes_to_iq(bytes: &[u8], fmt: SampleFormat, out: &mut Vec<Iq>) {
    out.clear();
    // Rust note: `as_chunks::<N>()` gives fixed-size `&[u8; N]` chunks plus a
    // remainder, which lets `from_le_bytes` take the array without a runtime
    // length check — and keeps clippy happy about constant chunk sizes.
    match fmt {
        SampleFormat::Cs8 => {
            let (chunks, _rest) = bytes.as_chunks::<2>();
            for ch in chunks {
                let i = (ch[0] as i8) as f32 / 128.0;
                let q = (ch[1] as i8) as f32 / 128.0;
                out.push(Iq::new(i, q));
            }
        }
        SampleFormat::Cu8 => {
            let (chunks, _rest) = bytes.as_chunks::<2>();
            for ch in chunks {
                let i = (ch[0] as f32 - 127.5) / 128.0;
                let q = (ch[1] as f32 - 127.5) / 128.0;
                out.push(Iq::new(i, q));
            }
        }
        SampleFormat::Cs16 => {
            let (chunks, _rest) = bytes.as_chunks::<4>();
            for ch in chunks {
                let i = i16::from_le_bytes([ch[0], ch[1]]) as f32 / 32768.0;
                let q = i16::from_le_bytes([ch[2], ch[3]]) as f32 / 32768.0;
                out.push(Iq::new(i, q));
            }
        }
        SampleFormat::Cs24 => {
            // Each 24-bit value placed in the top of an i32, then scaled: the
            // shift keeps the sign.
            let s24 =
                |b: &[u8]| (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0;
            let (chunks, _rest) = bytes.as_chunks::<6>();
            for ch in chunks {
                out.push(Iq::new(s24(&ch[0..3]), s24(&ch[3..6])));
            }
        }
        SampleFormat::Cf32 => {
            let (chunks, _rest) = bytes.as_chunks::<8>();
            for ch in chunks {
                let i = f32::from_le_bytes([ch[0], ch[1], ch[2], ch[3]]);
                let q = f32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]);
                out.push(Iq::new(i, q));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cs8_round_trips_scale() {
        let bytes = [0u8, 0, 127, 0, 0x80, 0x80];
        let mut out = Vec::new();
        bytes_to_iq(&bytes, SampleFormat::Cs8, &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], Iq::new(0.0, 0.0));
        assert!((out[1].re - 127.0 / 128.0).abs() < 1e-6);
        // 0x80 = -128
        assert!((out[2].re + 1.0).abs() < 1e-6);
    }

    #[test]
    fn unsigned_8_and_24_bit_samples() {
        let mut out = Vec::new();
        bytes_to_iq(&[128, 127, 255, 0], SampleFormat::Cu8, &mut out);
        assert!(out[0].re.abs() < 0.01 && out[0].im.abs() < 0.01);
        assert!((out[1].re - 1.0).abs() < 0.01 && (out[1].im + 1.0).abs() < 0.01);
        // 24-bit: 0x400000 = +0.5, 0xC00000 = -0.5
        bytes_to_iq(&[0, 0, 0x40, 0, 0, 0xC0], SampleFormat::Cs24, &mut out);
        assert_eq!(out, vec![Iq::new(0.5, -0.5)]);
    }

    #[test]
    fn roll_offs_cover_s2_and_s2x() {
        assert_eq!(RollOff::ALL.len(), 6);
        assert!((RollOff::R05.as_f64() - 0.05).abs() < 1e-12);
    }
}
