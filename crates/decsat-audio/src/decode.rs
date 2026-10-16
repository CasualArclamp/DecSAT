// made by CasualArclamp
//! Decoding [`Unit`]s to stereo PCM.
//!
//! MPEG audio layers I–III are decoded by Symphonia (pure Rust, MPL-2.0); AAC
//! — LC, and HE-AAC v1/v2 with SBR and PS — by libxaac (C, Apache-2.0,
//! vendored: `decsat-xaac-sys`, see [`crate::xaac`]); Opus by libopus (C,
//! BSD, vendored: `decsat-opus-sys`); PCM arrives already decoded. Everything comes out as
//! interleaved stereo f32 — mono is copied to both sides, and of more than
//! two channels the first two are kept — so the rest of the player has one
//! shape to deal with.

use std::ffi::{CStr, c_int};
use std::ptr::NonNull;

use decsat_opus_sys as opus;
use symphonia_bundle_mp3::MpaDecoder;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

use crate::aac::{AacConfig, fmt_khz};
use crate::depay::Unit;
use crate::es::MpaHeader;
use crate::xaac::XaacDecoder;

/// Stereo audio at one sample rate.
#[derive(Debug, Default, Clone)]
pub struct Block {
    pub rate: u32,
    /// Interleaved L, R.
    pub samples: Vec<f32>,
}

/// Decodes whatever a stream's units are.
#[derive(Default)]
pub struct Decoder {
    mpa: Option<(MpaHeader, MpaDecoder)>,
    aac: Option<(AacConfig, XaacDecoder)>,
    opus: Option<OpusDecoder>,
    scratch: Vec<f32>,
    pcm: Vec<i16>,
    frame: Vec<u8>,
    /// Units decoded, and those that failed.
    pub decoded: u64,
    pub errors: u64,
    /// What is being decoded ("MPEG-1 Layer II · 192 kbit/s · 48 kHz ·
    /// stereo"), once something has been.
    pub description: Option<String>,
    /// Why the last unit could not be decoded.
    pub last_error: Option<String>,
}

fn options() -> AudioDecoderOptions {
    let mut o = AudioDecoderOptions::default();
    // Nothing here has encoder delay or padding to trim.
    o.gapless = false;
    o
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one unit into `out` (replacing what it held). False if
    /// nothing came out.
    pub fn decode(&mut self, unit: &Unit, out: &mut Block) -> bool {
        out.samples.clear();
        let r = match unit {
            Unit::Mpa(f) => self.mpa(f, out),
            Unit::Aac(cfg, au) => self.aac(cfg, au, out),
            Unit::Pcm {
                rate,
                channels,
                samples,
            } => {
                out.rate = *rate;
                to_stereo(samples, *channels as usize, &mut out.samples);
                self.description = Some(format!(
                    "PCM · {} kHz · {}",
                    fmt_khz(*rate),
                    if *channels == 1 { "mono" } else { "stereo" }
                ));
                Ok(())
            }
            Unit::Ts(_) => Err("MPEG-TS is recorded, not played".into()),
            Unit::Opus(p) => self.opus(p, out),
        };
        match r {
            Ok(()) if !out.samples.is_empty() => {
                self.decoded += 1;
                true
            }
            Ok(()) => false,
            Err(e) => {
                self.errors += 1;
                self.last_error = Some(e);
                false
            }
        }
    }

    fn mpa(&mut self, f: &[u8], out: &mut Block) -> Result<(), String> {
        let h = MpaHeader::parse(f).ok_or("not an MPEG audio frame")?;
        let same = self.mpa.as_ref().is_some_and(|(o, _)| {
            (o.layer, o.sample_rate, o.channels) == (h.layer, h.sample_rate, h.channels)
        });
        if !same {
            let mut p = AudioCodecParameters::new();
            p.for_codec(match h.layer {
                1 => CODEC_ID_MP1,
                2 => CODEC_ID_MP2,
                _ => CODEC_ID_MP3,
            });
            let d = MpaDecoder::try_new(&p, &options()).map_err(|e| e.to_string())?;
            self.mpa = Some((h, d));
        }
        let (_, d) = self.mpa.as_mut().unwrap();
        let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), f);
        let buf = d.decode_ref(&pkt).map_err(|e| e.to_string())?;
        let ch = buf.spec().channels().count();
        out.rate = buf.spec().rate();
        buf.copy_to_vec_interleaved(&mut self.scratch);
        to_stereo(&self.scratch, ch, &mut out.samples);
        self.description = Some(format!(
            "MPEG-{} Layer {} · {} kbit/s · {} kHz · {}",
            if h.version == 25 {
                "2.5".to_string()
            } else {
                h.version.to_string()
            },
            ["I", "II", "III"][h.layer as usize - 1],
            h.bitrate_kbps,
            fmt_khz(h.sample_rate),
            if h.channels == 1 { "mono" } else { "stereo" }
        ));
        Ok(())
    }

    fn aac(&mut self, cfg: &AacConfig, au: &[u8], out: &mut Block) -> Result<(), String> {
        if !cfg.playable() {
            return Err(format!("{} cannot be decoded here", cfg.describe()));
        }
        // A new decoder when the core changes (SBR and PS are found in the
        // frames, whatever the configuration says of them).
        if self
            .aac
            .as_ref()
            .is_none_or(|(c, _)| c.core_asc() != cfg.core_asc())
        {
            self.aac = Some((*cfg, XaacDecoder::new(cfg.short_frames)?));
        }
        let (_, d) = self.aac.as_mut().unwrap();
        self.frame.clear();
        self.frame.extend_from_slice(&cfg.adts_header(au.len()));
        self.frame.extend_from_slice(au);
        if let Err(e) = d.decode(&self.frame, &mut self.pcm) {
            // A fatal error leaves the decoder unusable: start afresh.
            self.aac = None;
            return Err(e);
        }
        let info = d.info;
        if self.pcm.is_empty() || info.channels == 0 {
            return Ok(()); // the header, read before the first frame decodes
        }
        out.rate = info.rate;
        self.scratch.clear();
        self.scratch
            .extend(self.pcm.iter().map(|&v| f32::from(v) / 32768.0));
        to_stereo(&self.scratch, info.channels as usize, &mut out.samples);
        let name = match info.sbr {
            0 => "AAC-LC",
            1 => "HE-AAC (SBR)",
            _ => "HE-AAC v2 (SBR + PS)",
        };
        let ch = match (info.channels, cfg.channel_config) {
            (2, 1) => "stereo (PS)".to_string(),
            (1, _) => "mono".to_string(),
            (2, _) => "stereo".to_string(),
            (n, _) => format!("{n} channels"),
        };
        self.description = Some(format!("{name} · {} kHz · {ch}", fmt_khz(info.rate)));
        Ok(())
    }
}

impl Decoder {
    fn opus(&mut self, p: &[u8], out: &mut Block) -> Result<(), String> {
        if self.opus.is_none() {
            self.opus = Some(OpusDecoder::new()?);
        }
        let d = self.opus.as_mut().unwrap();
        d.decode(p, &mut out.samples)?;
        out.rate = 48_000;
        // SAFETY: `p` is non-empty (the depacketiser drops empty
        // payloads); these read only its TOC byte.
        let (bw, ch) = unsafe {
            (
                opus::opus_packet_get_bandwidth(p.as_ptr()),
                opus::opus_packet_get_nb_channels(p.as_ptr()),
            )
        };
        // Audio bandwidth by mode (RFC 6716 §2, Table 1).
        let band = match bw {
            opus::OPUS_BANDWIDTH_NARROWBAND => "narrowband (4 kHz)",
            opus::OPUS_BANDWIDTH_MEDIUMBAND => "mediumband (6 kHz)",
            opus::OPUS_BANDWIDTH_WIDEBAND => "wideband (8 kHz)",
            opus::OPUS_BANDWIDTH_SUPERWIDEBAND => "super-wideband (12 kHz)",
            _ => "fullband (20 kHz)",
        };
        self.description = Some(format!(
            "Opus · 48 kHz · {} · {band}",
            if ch == 1 { "mono" } else { "stereo" }
        ));
        Ok(())
    }
}

/// The largest Opus packet's audio: 120 ms at 48 kHz (RFC 6716 §3.2.5).
const OPUS_MAX_FRAME: usize = 5760;

/// A libopus decoder, always 48 kHz stereo out: libopus duplicates a mono
/// stream to both sides itself.
struct OpusDecoder(NonNull<opus::OpusDecoder>);

// SAFETY: the decoder state belongs to this value alone, and libopus keeps
// no per-thread or shared state for it, so it may move between threads.
unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    fn new() -> Result<OpusDecoder, String> {
        let mut err = 0;
        // SAFETY: a valid rate and channel count; `err` outlives the call.
        let p = unsafe { opus::opus_decoder_create(48_000, 2, &mut err) };
        NonNull::new(p)
            .map(OpusDecoder)
            .ok_or_else(|| opus_error(err))
    }

    /// One packet into interleaved stereo `out` (replacing what it held).
    fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<(), String> {
        out.clear();
        out.resize(2 * OPUS_MAX_FRAME, 0.0);
        // SAFETY: the state is live; `packet` is valid for its length and
        // `out` holds OPUS_MAX_FRAME stereo samples, the capacity passed.
        let n = unsafe {
            opus::opus_decode_float(
                self.0.as_ptr(),
                packet.as_ptr(),
                packet.len() as opus::opus_int32,
                out.as_mut_ptr(),
                OPUS_MAX_FRAME as c_int,
                0,
            )
        };
        if n < 0 {
            out.clear();
            return Err(opus_error(n));
        }
        out.truncate(2 * n as usize);
        Ok(())
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        // SAFETY: created by `opus_decoder_create` and destroyed only here.
        unsafe { opus::opus_decoder_destroy(self.0.as_ptr()) }
    }
}

/// libopus's text for an error code.
fn opus_error(code: c_int) -> String {
    // SAFETY: `opus_strerror` returns a static NUL-terminated string for
    // any code.
    let s = unsafe { CStr::from_ptr(opus::opus_strerror(code)) };
    format!("Opus: {}", s.to_string_lossy())
}

/// Interleaved `channels`-channel audio to interleaved stereo.
fn to_stereo(x: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        0 => {}
        1 => out.extend(x.iter().flat_map(|&v| [v, v])),
        2 => out.extend_from_slice(x),
        n => out.extend(x.chunks_exact(n).flat_map(|c| [c[0], c[1]])),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::aac::silent_aac_au;
    use decsat_ip::mcast::silent_mp2_frame;

    #[test]
    fn decodes_silent_mp2() {
        let mut d = Decoder::new();
        let mut b = Block::default();
        assert!(
            d.decode(&Unit::Mpa(silent_mp2_frame()), &mut b),
            "{:?}",
            d.last_error
        );
        assert_eq!(b.rate, 48_000);
        assert_eq!(b.samples.len(), 2 * 1152);
        assert!(b.samples.iter().all(|v| v.abs() < 1e-6));
        assert!(
            d.description
                .as_deref()
                .unwrap()
                .starts_with("MPEG-1 Layer II")
        );
    }

    /// Decode `n` copies of `unit`; the blocks that came out.
    fn run(d: &mut Decoder, unit: &Unit, n: usize) -> Vec<Block> {
        let mut out = Vec::new();
        for _ in 0..n {
            let mut b = Block::default();
            if d.decode(unit, &mut b) {
                out.push(b);
            }
            assert!(d.last_error.is_none(), "{:?}", d.last_error);
        }
        out
    }

    #[test]
    fn decodes_silent_aac_mono_and_stereo() {
        for (asc, ch) in [([0x11u8, 0x88], 1u8), ([0x11, 0x90], 2)] {
            let cfg = AacConfig::from_bytes(&asc).unwrap();
            assert_eq!(cfg.channel_config, ch);
            let mut d = Decoder::new();
            // libxaac reads the stream's header from the first frame, and
            // trims its start-up delay from the first block out.
            let blocks = run(&mut d, &Unit::Aac(cfg, silent_aac_au(ch)), 8);
            assert!(blocks.len() >= 6, "{ch} ch: {} blocks", blocks.len());
            for b in &blocks {
                assert_eq!(b.rate, 48_000);
                assert!(b.samples.len() <= 2 * 1024);
            }
            for b in &blocks[1..] {
                assert_eq!(b.samples.len(), 2 * 1024);
                assert!(b.samples.iter().all(|v| v.abs() < 1e-3));
            }
            let desc = d.description.unwrap();
            assert!(desc.starts_with("AAC-LC · 48 kHz"), "{desc}");
            assert!(desc.ends_with(if ch == 1 { "mono" } else { "stereo" }));
        }
    }

    #[test]
    fn an_he_aac_configuration_without_sbr_data_still_plays() {
        // HE-AAC signalled (24 kHz core, 48 kHz with SBR), but the frames
        // carry no SBR data: libxaac plays what is there.
        let cfg = AacConfig::from_bytes(&[0x2B, 0x11, 0x88, 0x00]).unwrap();
        let mut d = Decoder::new();
        let blocks = run(&mut d, &Unit::Aac(cfg, silent_aac_au(2)), 4);
        assert!(!blocks.is_empty());
        let b = &blocks[blocks.len() - 1];
        assert!(matches!(b.rate, 24_000 | 48_000), "{}", b.rate);
        assert_eq!(b.samples.len() as u32 * 24_000, 2 * 1024 * b.rate);
    }

    #[test]
    fn damaged_aac_is_an_error_and_recovers() {
        let cfg = AacConfig::from_bytes(&[0x11, 0x90]).unwrap();
        let mut d = Decoder::new();
        run(&mut d, &Unit::Aac(cfg, silent_aac_au(2)), 3);
        let mut b = Block::default();
        for k in 0..20u8 {
            let junk: Vec<u8> = (0..200u32)
                .map(|i| (i as u8).wrapping_mul(37) ^ k)
                .collect();
            d.decode(&Unit::Aac(cfg, junk), &mut b);
        }
        // Good frames play again afterwards.
        d.last_error = None;
        let blocks = run(&mut d, &Unit::Aac(cfg, silent_aac_au(2)), 4);
        assert!(!blocks.is_empty());
    }

    /// `n` 20 ms Opus packets of a 1 kHz tone at −6 dBFS, `channels` 1 or 2,
    /// from libopus's encoder.
    pub(crate) fn opus_tone(n: usize, channels: usize) -> Vec<Vec<u8>> {
        const N: usize = 960;
        let mut err = 0;
        let mut out = Vec::new();
        // SAFETY: valid arguments; the encoder is destroyed at the end and
        // every buffer outlives the calls that use it.
        unsafe {
            let enc = opus::opus_encoder_create(
                48_000,
                channels as c_int,
                opus::OPUS_APPLICATION_AUDIO,
                &mut err,
            );
            assert!(!enc.is_null(), "{err}");
            for k in 0..n {
                let pcm: Vec<f32> = (0..N * channels)
                    .map(|i| {
                        let t = (k * N + i / channels) as f32 / 48_000.0;
                        0.5 * (std::f32::consts::TAU * 1000.0 * t).sin()
                    })
                    .collect();
                let mut p = vec![0u8; 1275];
                let len =
                    opus::opus_encode_float(enc, pcm.as_ptr(), N as c_int, p.as_mut_ptr(), 1275);
                assert!(len > 0, "{len}");
                p.truncate(len as usize);
                out.push(p);
            }
            opus::opus_encoder_destroy(enc);
        }
        out
    }

    #[test]
    fn decodes_opus_mono_and_stereo_to_stereo() {
        for ch in [1, 2] {
            let mut d = Decoder::new();
            let mut b = Block::default();
            let mut energy = 0.0;
            let mut samples = 0;
            for (k, p) in opus_tone(25, ch).into_iter().enumerate() {
                assert!(d.decode(&Unit::Opus(p), &mut b), "{:?}", d.last_error);
                assert_eq!((b.rate, b.samples.len()), (48_000, 2 * 960));
                if k >= 5 {
                    energy += b.samples.iter().map(|v| v * v).sum::<f32>();
                    samples += b.samples.len();
                }
            }
            // A 0.5-amplitude sine: RMS 0.354 (−9 dBFS).
            let rms = (energy / samples as f32).sqrt();
            assert!((rms - 0.354).abs() < 0.05, "{ch} ch: rms {rms}");
            let desc = d.description.clone().unwrap();
            assert!(desc.starts_with("Opus · 48 kHz"), "{desc}");
            assert!(desc.contains(if ch == 1 { "mono" } else { "stereo" }));
        }
        // Garbage is an error, not a crash.
        let mut d = Decoder::new();
        assert!(!d.decode(&Unit::Opus(vec![0xFF; 3]), &mut Block::default()));
        assert!(d.last_error.unwrap().starts_with("Opus:"));
    }

    #[test]
    fn pcm_mono_becomes_stereo() {
        let mut d = Decoder::new();
        let mut b = Block::default();
        let u = Unit::Pcm {
            rate: 8000,
            channels: 1,
            samples: vec![0.5, -0.25],
        };
        assert!(d.decode(&u, &mut b));
        assert_eq!(b.samples, vec![0.5, 0.5, -0.25, -0.25]);
    }
}
