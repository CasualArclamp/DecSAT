// made by CasualArclamp
//! Beacon test signals: a carrier with drift and noise, phase-modulated by
//! telemetry on a subcarrier or directly, or a suppressed-carrier BPSK —
//! for the tests, and as test signals for anyone checking a receiver.

use decsat_core::Iq;
use decsat_modem::ccsds;

/// What the telemetry frames are.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Framing {
    /// No telemetry: an unmodulated (CW) carrier.
    None,
    /// IRIG-style minor frames: FAF320, a fixed byte, a counter running
    /// `0..counter_mod`, then fixed and changing words.
    Irig { frame_bytes: usize, counter_mod: u8 },
    /// CCSDS: TM transfer frames, RS(255,223) at interleave `interleave` in
    /// the dual basis, randomised, the marker; the K = 7 code on top if
    /// `conv`.
    Ccsds {
        interleave: usize,
        conv: bool,
        scid: u16,
    },
    /// CCSDS as Eutelsat 172B's beacon sends it: TM transfer frames of
    /// `frame_bytes` with their error control field, no Reed–Solomon, no
    /// randomiser, the marker, the K = 7 code on top; the frames mostly
    /// zeros, so the coded stream is mostly 0101… fill.
    CcsdsPlain { frame_bytes: usize, scid: u16 },
}

/// A beacon to make.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub rate: f64,
    pub seconds: f64,
    /// The carrier's offset at the start, Hz, and its drift, Hz/s.
    pub carrier_hz: f64,
    pub drift_hz_s: f64,
    /// C/N0 of the carrier (the residual carrier's power; a suppressed
    /// carrier's whole power), dB-Hz.
    pub cn0_dbhz: f64,
    /// Peak phase deviation, radians.
    pub index_rad: f64,
    /// A sine subcarrier the data BPSK-modulate (`None`: on the phase
    /// directly).
    pub subcarrier_hz: Option<f64>,
    /// Channel bits a second (after any convolutional code).
    pub bit_rate: f64,
    pub biphase: bool,
    /// BPSK with no residual carrier (index and subcarrier ignored).
    pub suppressed: bool,
    pub framing: Framing,
    /// The spacecraft's clock against ours, ppm (bit rate and subcarrier).
    pub clock_ppm: f64,
    /// An unmodulated tone also on the phase: (Hz, peak radians).
    pub tone: Option<(f64, f64)>,
}

/// The channel bits for `n` bits' worth of `framing`, sent at
/// `bit_rate` (IRIG frames carry a seconds clock in bytes 12–15).
pub fn bits(framing: Framing, n: usize, bit_rate: f64) -> Vec<u8> {
    let mut out = Vec::with_capacity(n + 65_536);
    let push_bytes = |out: &mut Vec<u8>, bytes: &[u8]| {
        for b in bytes {
            for k in 0..8 {
                out.push((b >> (7 - k)) & 1);
            }
        }
    };
    match framing {
        Framing::None => out.resize(n, 0),
        Framing::Irig {
            frame_bytes,
            counter_mod,
        } => {
            let mut i = 0usize;
            while out.len() < n {
                let mut f = vec![0u8; frame_bytes];
                f[..3].copy_from_slice(&[0xFA, 0xF3, 0x20]);
                f[3] = 0x10;
                f[4] = (i % usize::from(counter_mod.max(1))) as u8;
                f[5..10].copy_from_slice(&[0x00, 0x01, 0x9E, 0xA3, 0x2B]);
                for (j, b) in f.iter_mut().enumerate().skip(10) {
                    *b = match j % 7 {
                        0 => 0xFF,
                        1 => 0x00,
                        2 => (i / 3) as u8,
                        3 => (j as u8).wrapping_mul(29) ^ (i as u8 & 0x0F),
                        _ => (j as u8).wrapping_mul(13),
                    };
                }
                let t = (i * frame_bytes * 8) as f64 / bit_rate;
                let clock = 0x1B08_0000u32 + t as u32;
                f[11] = 0x21;
                f[12..16].copy_from_slice(&clock.to_be_bytes());
                push_bytes(&mut out, &f);
                i += 1;
            }
        }
        Framing::Ccsds {
            interleave,
            conv,
            scid,
        } => {
            let fmt = ccsds::Format {
                block_bytes: interleave * 255,
                rs: Some(ccsds::RsLayout {
                    e: 16,
                    interleave,
                    n: 255,
                    dual: true,
                }),
                randomised: true,
            };
            let mut plain = Vec::new();
            let mut i = 0u32;
            let target = if conv { n / 2 + 64 } else { n };
            while plain.len() < target {
                let data: Vec<u8> = (0..100).map(|k| (k as u32 * 7 + i) as u8).collect();
                let frame = ccsds::tm_frame(
                    fmt.frame_bytes(),
                    scid,
                    (i % 2) as u8,
                    i as u8,
                    (i / 2) as u8,
                    &data,
                );
                push_bytes(&mut plain, &ccsds::ASM.to_be_bytes());
                push_bytes(&mut plain, &ccsds::encode_block(&fmt, &frame));
                i += 1;
            }
            if conv {
                out = ccsds::CONV_CODE.encode(&plain);
            } else {
                out = plain;
            }
        }
        Framing::CcsdsPlain { frame_bytes, scid } => {
            let mut plain = Vec::new();
            let mut i = 0u32;
            while plain.len() < n / 2 + 64 {
                // A few changing bytes up front, zeros after.
                let mut data = vec![0u8; frame_bytes - 8];
                for (k, b) in data.iter_mut().take(24).enumerate() {
                    *b = (k as u32 * 37 + i) as u8;
                }
                let frame = ccsds::tm_frame(frame_bytes, scid, 0, i as u8, i as u8, &data);
                push_bytes(&mut plain, &ccsds::ASM.to_be_bytes());
                push_bytes(&mut plain, &frame);
                i += 1;
            }
            out = ccsds::CONV_CODE.encode(&plain);
        }
    }
    out.truncate(n.max(1));
    out
}

/// Gaussian pairs (Box–Muller) from a xorshift generator.
struct Noise(u64);

impl Noise {
    fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// A complex sample of variance `var` (`var`/2 each part).
    fn complex(&mut self, var: f64) -> (f64, f64) {
        let r = (-var * self.uniform().ln()).sqrt();
        let a = std::f64::consts::TAU * self.uniform();
        (r * a.cos(), r * a.sin())
    }
}

/// The beacon's baseband.
pub fn generate(s: &Spec) -> Vec<Iq> {
    let n = (s.seconds * s.rate) as usize;
    let clock = 1.0 + s.clock_ppm * 1e-6;
    let nbits = (s.seconds * s.bit_rate * clock) as usize + 16;
    let data = bits(s.framing, nbits, s.bit_rate);
    let amp = 0.5;
    let modulated = !matches!(s.framing, Framing::None);
    let carrier_power = if s.suppressed || !modulated {
        amp * amp
    } else if s.subcarrier_hz.is_some() {
        (amp * crate::beacon::carrier::bessel_j0(s.index_rad)).powi(2)
    } else {
        (amp * s.index_rad.cos()).powi(2)
    };
    let noise_var = carrier_power * s.rate / 10f64.powf(s.cn0_dbhz / 10.0);
    let mut noise = Noise(0x9E37_79B9_7F4A_7C15);
    (0..n)
        .map(|i| {
            let t = i as f64 / s.rate;
            let tb = t * s.bit_rate * clock;
            let k = (tb as usize).min(data.len() - 1);
            let mut level = if data[k] == 1 { 1.0 } else { -1.0 };
            if s.biphase && tb.fract() >= 0.5 {
                level = -level;
            }
            let theta = std::f64::consts::TAU * (s.carrier_hz * t + 0.5 * s.drift_hz_s * t * t);
            let (re, im) = if s.suppressed {
                (amp * level * theta.cos(), amp * level * theta.sin())
            } else {
                let m = if modulated {
                    match s.subcarrier_hz {
                        Some(f) => {
                            s.index_rad * level * (std::f64::consts::TAU * f * clock * t).sin()
                        }
                        None => s.index_rad * level,
                    }
                } else {
                    0.0
                };
                let tone = s
                    .tone
                    .map_or(0.0, |(f, a)| a * (std::f64::consts::TAU * f * t).sin());
                let ph = theta + m + tone;
                (amp * ph.cos(), amp * ph.sin())
            };
            let (nr, ni) = noise.complex(noise_var);
            Iq::new((re + nr) as f32, (im + ni) as f32)
        })
        .collect()
}
