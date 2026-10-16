// made by CasualArclamp
//! AAC configuration and the containers multicast radio puts AAC in.
//!
//! - **AudioSpecificConfig** (ISO/IEC 14496-3 §1.6.2.1): object type,
//!   sample rate, channels, and whether SBR/PS (HE-AAC v1/v2) is signalled.
//! - **LATM** (14496-3 §1.7.3): AudioMuxElement and StreamMuxConfig, in LOAS
//!   (raw UDP, sync 0x2B7) or in RTP as MP4A-LATM (RFC 3016 / RFC 6416).
//! - **RFC 3640** mpeg4-generic: AU headers, then the access units.
//!
//! The decoder (libxaac, [`crate::xaac`]) is given each access unit in an
//! ADTS frame ([`AacConfig::adts_header`]) — the core's configuration — and
//! finds SBR and PS in the frames themselves, as players do with ADTS; a
//! recording is the same frames, so it keeps everything too.

use crate::bits::{BitReader, BitWriter};

/// Sample rates by index (14496-3 Table 1.18).
pub const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// What an AudioSpecificConfig says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AacConfig {
    /// The AAC object type under any SBR/PS signalling: 2 for AAC-LC.
    pub core_type: u8,
    /// Core sample rate index (0..=12), or 15 when given explicitly.
    pub sample_rate_index: u8,
    /// Core sample rate, Hz.
    pub sample_rate: u32,
    pub channel_config: u8,
    /// SBR is signalled (HE-AAC), with the output sample rate.
    pub sbr_rate: Option<u32>,
    /// Parametric stereo is signalled (HE-AAC v2).
    pub ps: bool,
    /// 960-sample frames (DAB+ style) rather than 1024.
    pub short_frames: bool,
}

fn object_type(r: &mut BitReader) -> Option<u8> {
    let t = r.read(5)?;
    Some(if t == 31 { 32 + r.read(6)? } else { t } as u8)
}

fn sample_rate(r: &mut BitReader) -> Option<(u8, u32)> {
    let i = r.read(4)? as u8;
    let rate = match i {
        15 => r.read(24)?,
        _ => *SAMPLE_RATES.get(i as usize)?,
    };
    Some((i, rate))
}

impl AacConfig {
    /// Parse an AudioSpecificConfig from `r`, leaving it just past it.
    /// `None` for configurations that cannot be played (a program config
    /// element, error-resilient coding, unparseable types).
    ///
    /// `standalone` means the config fills its buffer (SDP `config=`), so
    /// the backward-compatible SBR/PS extension (§1.6.5.2, sync 0x2B7) may
    /// follow it; inside LATM version 0 nothing says where a config ends,
    /// so that extension is not looked for.
    pub fn parse(r: &mut BitReader, standalone: bool) -> Option<AacConfig> {
        let mut t = object_type(r)?;
        let (mut sfi, mut rate) = sample_rate(r)?;
        let channel_config = r.read(4)? as u8;
        let mut sbr_rate = None;
        let mut ps = false;
        if t == 5 || t == 29 {
            // Explicit hierarchical signalling: the extension rate, then the
            // core's own object type.
            ps = t == 29;
            sbr_rate = Some(sample_rate(r)?.1);
            t = object_type(r)?;
            if t == 22 {
                r.skip(4)?; // extensionChannelConfiguration
            }
        }
        if !matches!(t, 1..=4 | 6 | 7 | 17 | 19..=23) {
            return None;
        }
        // GASpecificConfig (§4.4.1).
        let short_frames = r.bit()?;
        if r.bit()? {
            r.skip(14)?; // coreCoderDelay
        }
        let extension = r.bit()?;
        if channel_config == 0 {
            return None; // program_config_element: not handled
        }
        if t == 6 || t == 20 {
            r.skip(3)?; // layerNr
        }
        if extension {
            if t == 22 {
                r.skip(16)?;
            }
            if matches!(t, 17 | 19 | 20 | 23) {
                r.skip(3)?;
            }
            r.skip(1)?; // extensionFlag3
        }
        if matches!(t, 17 | 19..=27) && r.read(2)? >= 2 {
            return None; // ErrorProtectionSpecificConfig
        }
        if standalone
            && sbr_rate.is_none()
            && r.remaining() >= 16
            && r.read(11)? == 0x2B7
            && object_type(r)? == 5
            && r.bit()?
        {
            sbr_rate = Some(sample_rate(r)?.1);
            if r.remaining() >= 12 && r.read(11)? == 0x548 {
                ps = r.bit()?;
            }
        }
        if sfi == 15 {
            // Re-index an explicit rate that is a standard one.
            if let Some(i) = SAMPLE_RATES.iter().position(|&s| s == rate) {
                sfi = i as u8;
            }
        }
        rate = rate.max(1);
        Some(AacConfig {
            core_type: t,
            sample_rate_index: sfi,
            sample_rate: rate,
            channel_config,
            sbr_rate,
            ps,
            short_frames,
        })
    }

    /// Parse a standalone config (hex from SDP, or bytes).
    pub fn from_bytes(b: &[u8]) -> Option<AacConfig> {
        Self::parse(&mut BitReader::new(b), true)
    }

    /// An AAC-LC AudioSpecificConfig for the core alone: what identifies
    /// the stream to the decoder (a new one is started when it changes).
    pub fn core_asc(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put(5, 2);
        if self.sample_rate_index < 13 {
            w.put(4, self.sample_rate_index as u32);
        } else {
            w.put(4, 15);
            w.put(24, self.sample_rate);
        }
        w.put(4, self.channel_config as u32);
        w.put(3, 0); // 1024-sample frames, no core coder, no extension
        w.finish()
    }

    /// The decoder can play this: an AAC-LC core (with or without SBR and
    /// PS), mono or stereo, 1024-sample frames, a standard rate.
    pub fn playable(&self) -> bool {
        self.core_type == 2
            && !self.short_frames
            && matches!(self.channel_config, 1 | 2)
            && self.sample_rate_index < 13
    }

    /// A 7-byte ADTS header for a `payload_len`-byte access unit (13818-7
    /// §6.2.1), so a recording plays anywhere. SBR and PS are implicit in
    /// ADTS: the header gives the core, and players find the extension data.
    pub fn adts_header(&self, payload_len: usize) -> [u8; 7] {
        let len = payload_len + 7;
        let profile = self.core_type.saturating_sub(1).min(3);
        let sfi = self.sample_rate_index.min(12);
        let ch = self.channel_config & 7;
        [
            0xFF,
            0xF1, // MPEG-4, layer 0, no CRC
            (profile << 6) | (sfi << 2) | (ch >> 2),
            ((ch & 3) << 6) | ((len >> 11) as u8 & 3),
            (len >> 3) as u8,
            (((len & 7) as u8) << 5) | 0x1F,
            0xFC, // buffer fullness 0x7FF (VBR), one raw data block
        ]
    }

    /// "HE-AAC v2 · 48 kHz · stereo" and the like.
    pub fn describe(&self) -> String {
        let name = match (self.sbr_rate, self.ps, self.core_type) {
            (_, true, _) => "HE-AAC v2",
            (Some(_), false, _) => "HE-AAC",
            (None, _, 2) => "AAC-LC",
            _ => "AAC",
        };
        let rate = self.sbr_rate.unwrap_or(self.sample_rate);
        let ch = match (self.channel_config, self.ps) {
            (1, true) => "stereo (PS)".to_string(),
            (1, false) => "mono".to_string(),
            (2, _) => "stereo".to_string(),
            (n, _) => format!("{n}-ch config"),
        };
        format!("{name} · {} kHz · {ch}", fmt_khz(rate))
    }
}

/// "48", "44.1", "22.05".
pub fn fmt_khz(hz: u32) -> String {
    let s = format!("{:.2}", hz as f64 / 1000.0);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// LatmGetValue (14496-3 §1.7.3.1).
fn latm_value(r: &mut BitReader) -> Option<u32> {
    let n = r.read(2)? + 1;
    let mut v = 0u32;
    for _ in 0..n {
        v = (v << 8) | r.read(8)?;
    }
    Some(v)
}

/// The parts of a StreamMuxConfig a single-program, single-layer stream
/// uses — which is every radio stream seen in practice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamMux {
    version: u8,
    asc: AacConfig,
    sub_frames: u8,
    /// Bits of "other data" after the payloads, when present.
    other_bits: Option<u32>,
}

fn stream_mux_config(r: &mut BitReader) -> Option<StreamMux> {
    let version = r.read(1)? as u8;
    if version == 1 && r.read(1)? != 0 {
        return None; // audioMuxVersionA: reserved
    }
    if version == 1 {
        latm_value(r)?; // taraBufferFullness
    }
    let same_time_framing = r.bit()?;
    let sub_frames = r.read(6)? as u8;
    let programs = r.read(4)?;
    let layers = r.read(3)?;
    if programs != 0 || layers != 0 || !same_time_framing {
        return None; // more than one stream: not radio as we know it
    }
    let asc = if version == 0 {
        AacConfig::parse(r, false)?
    } else {
        let len = latm_value(r)? as usize;
        let start = r.position();
        let asc = AacConfig::parse(r, false)?;
        let used = r.position() - start;
        r.skip(len.checked_sub(used)?)?;
        asc
    };
    match r.read(3)? {
        0 => {
            r.skip(8)?; // latmBufferFullness
        }
        _ => return None, // fixed-length / CELP / HVXC framing
    }
    let other_bits = if r.bit()? {
        Some(if version == 1 {
            latm_value(r)?
        } else {
            let mut bits = 0u32;
            loop {
                let esc = r.bit()?;
                bits = (bits << 8) | r.read(8)?;
                if !esc {
                    break bits;
                }
            }
        })
    } else {
        None
    };
    if r.bit()? {
        r.skip(8)?; // crcCheckSum
    }
    Some(StreamMux {
        version,
        asc,
        sub_frames,
        other_bits,
    })
}

/// LATM demultiplexer: AudioMuxElements in, access units out.
#[derive(Default)]
pub struct Latm {
    mux: Option<StreamMux>,
}

impl Latm {
    pub fn new() -> Self {
        Self::default()
    }

    /// With the StreamMuxConfig given out of band (RFC 3016's `config=`).
    pub fn with_config(config: &[u8]) -> Option<Self> {
        let mux = stream_mux_config(&mut BitReader::new(config))?;
        Some(Latm { mux: Some(mux) })
    }

    pub fn config(&self) -> Option<AacConfig> {
        self.mux.map(|m| m.asc)
    }

    /// One AudioMuxElement; its access units go to `out`. False if it could
    /// not be read (no configuration yet, or one not handled).
    pub fn element(
        &mut self,
        data: &[u8],
        mux_config_present: bool,
        out: &mut Vec<(AacConfig, Vec<u8>)>,
    ) -> bool {
        self.element_inner(data, mux_config_present, out).is_some()
    }

    fn element_inner(
        &mut self,
        data: &[u8],
        mux_config_present: bool,
        out: &mut Vec<(AacConfig, Vec<u8>)>,
    ) -> Option<()> {
        let mut r = BitReader::new(data);
        if mux_config_present && !r.bit()? {
            self.mux = Some(stream_mux_config(&mut r)?);
        }
        let mux = self.mux?;
        for _ in 0..=mux.sub_frames {
            // PayloadLengthInfo: bytes in 255s, then the rest.
            let mut len = 0usize;
            loop {
                let b = r.read(8)? as usize;
                len += b;
                if b != 255 {
                    break;
                }
            }
            out.push((mux.asc, r.bytes(len)?));
        }
        if let Some(bits) = mux.other_bits {
            r.skip(bits as usize)?;
        }
        let _ = mux.version;
        Some(())
    }
}

/// `key=value` from an SDP `a=fmtp` parameter list, keys any case.
pub fn fmtp_param<'a>(fmtp: &'a str, key: &str) -> Option<&'a str> {
    fmtp.split(';').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        k.trim().eq_ignore_ascii_case(key).then(|| v.trim())
    })
}

/// Hex ("1190") to bytes.
pub fn hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// RFC 3640 (mpeg4-generic) depayloader for AAC-hbr / AAC-lbr.
#[derive(Debug, Clone, Copy)]
pub struct Rfc3640 {
    size_len: u32,
    index_len: u32,
    delta_len: u32,
    pub config: AacConfig,
}

impl Rfc3640 {
    /// From the SDP format parameters; `None` without a usable `config=`.
    pub fn from_fmtp(fmtp: &str) -> Option<Rfc3640> {
        let num = |k: &str| fmtp_param(fmtp, k).and_then(|v| v.parse::<u32>().ok());
        let config = AacConfig::from_bytes(&hex(fmtp_param(fmtp, "config")?)?)?;
        Some(Rfc3640 {
            size_len: num("sizelength").unwrap_or(13),
            index_len: num("indexlength").unwrap_or(3),
            delta_len: num("indexdeltalength").unwrap_or(3),
            config,
        })
    }

    /// The access units in one RTP payload.
    pub fn split(&self, p: &[u8], out: &mut Vec<(AacConfig, Vec<u8>)>) -> Option<()> {
        let mut r = BitReader::new(p);
        let header_bits = r.read(16)? as usize;
        let mut sizes = Vec::new();
        let start = r.position();
        while r.position() - start < header_bits {
            let size = r.read(self.size_len)? as usize;
            let idx = if sizes.is_empty() {
                self.index_len
            } else {
                self.delta_len
            };
            r.skip(idx as usize)?;
            sizes.push(size);
        }
        let mut at = 2 + header_bits.div_ceil(8);
        for size in sizes {
            let au = p.get(at..at + size)?;
            out.push((self.config, au.to_vec()));
            at += size;
        }
        Some(())
    }
}

/// A silent AAC-LC access unit for `channels` (1 or 2): one SCE or CPE with
/// no scale-factor bands coded, then END — every spectral value is zero.
/// For tests and test signals.
pub fn silent_aac_au(channels: u8) -> Vec<u8> {
    let mut w = BitWriter::new();
    let ics = |w: &mut BitWriter| {
        w.put(8, 100); // global_gain
        // ics_info: reserved, ONLY_LONG_SEQUENCE, sine window, max_sfb 0,
        // no prediction.
        w.put(1, 0);
        w.put(2, 0);
        w.put(1, 0);
        w.put(6, 0);
        w.put(1, 0);
        // No sections, scale factors or spectral data with max_sfb 0; no
        // pulse, TNS or gain control.
        w.put(3, 0);
    };
    if channels == 2 {
        w.put(3, 1); // ID_CPE
        w.put(4, 0);
        w.put(1, 0); // common_window = 0
        ics(&mut w);
        ics(&mut w);
    } else {
        w.put(3, 0); // ID_SCE
        w.put(4, 0);
        ics(&mut w);
    }
    w.put(3, 7); // ID_END
    w.finish()
}

/// A LOAS frame (AudioSyncStream + AudioMuxElement with its config) around
/// one AAC-LC access unit. For tests and test signals.
pub fn loas_frame(sample_rate_index: u8, channels: u8, au: &[u8]) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.put(1, 0); // useSameStreamMux = 0: config follows
    // StreamMuxConfig, version 0.
    w.put(1, 0);
    w.put(1, 1); // allStreamsSameTimeFraming
    w.put(6, 0); // numSubFrames
    w.put(4, 0); // numProgram
    w.put(3, 0); // numLayer
    w.put(5, 2); // AAC-LC
    w.put(4, sample_rate_index as u32);
    w.put(4, channels as u32);
    w.put(3, 0); // GASpecificConfig
    w.put(3, 0); // frameLengthType
    w.put(8, 0xFF); // latmBufferFullness
    w.put(1, 0); // otherDataPresent
    w.put(1, 0); // crcCheckPresent
    // PayloadLengthInfo + PayloadMux.
    let mut n = au.len();
    while n >= 255 {
        w.put(8, 255);
        n -= 255;
    }
    w.put(8, n as u32);
    for &b in au {
        w.put(8, b as u32);
    }
    let body = w.finish();
    let mut f = vec![0x56, 0xE0 | (body.len() >> 8) as u8, body.len() as u8];
    f.extend_from_slice(&body);
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asc_lc_and_he_aac() {
        // 0x1190: AAC-LC, 48 kHz, stereo.
        let c = AacConfig::from_bytes(&[0x11, 0x90]).unwrap();
        assert_eq!(
            (c.core_type, c.sample_rate, c.channel_config),
            (2, 48_000, 2)
        );
        assert!(c.playable() && c.sbr_rate.is_none());
        assert_eq!(c.core_asc(), vec![0x11, 0x90]);

        // Explicit HE-AAC: type 5, 24 kHz core, stereo, 48 kHz output, core
        // AAC-LC: 00101 0110 0010 0011 00010 000 → 0x2B 0x11 0x88 0x00.
        let c = AacConfig::from_bytes(&[0x2B, 0x11, 0x88, 0x00]).unwrap();
        assert_eq!(c.sbr_rate, Some(48_000));
        assert_eq!((c.core_type, c.sample_rate), (2, 24_000));
        assert!(c.playable());
        assert_eq!(c.describe(), "HE-AAC · 48 kHz · stereo");
        // The core config the decoder gets: AAC-LC at 24 kHz.
        assert_eq!(
            AacConfig::from_bytes(&c.core_asc()).unwrap().sample_rate,
            24_000
        );

        // Implicit signalling with the sync extension: LC 24 kHz stereo,
        // then 0x2B7, type 5, SBR present, 48 kHz.
        let mut w = BitWriter::new();
        for (n, v) in [
            (5, 2),
            (4, 6),
            (4, 2),
            (3, 0),
            (11, 0x2B7),
            (5, 5),
            (1, 1),
            (4, 3),
        ] {
            w.put(n, v);
        }
        let c = AacConfig::from_bytes(&w.finish()).unwrap();
        assert_eq!(c.sbr_rate, Some(48_000));
    }

    #[test]
    fn adts_header_round_trips() {
        let c = AacConfig::from_bytes(&[0x11, 0x90]).unwrap();
        let h = c.adts_header(100);
        let p = crate::es::AdtsHeader::parse(&h).unwrap();
        assert_eq!(
            (p.object_type, p.sample_rate_index, p.channel_config),
            (2, 3, 2)
        );
        assert_eq!(p.frame_len, 107);
    }

    #[test]
    fn loas_round_trip() {
        let au = silent_aac_au(2);
        let f = loas_frame(3, 2, &au);
        let mut latm = Latm::new();
        let mut out = Vec::new();
        assert!(latm.element(&f[3..], true, &mut out));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, au);
        assert_eq!(out[0].0.sample_rate, 48_000);
        // Without a config the next element (useSameStreamMux) still reads.
        let mut w = BitWriter::new();
        w.put(1, 1);
        w.put(8, au.len() as u32);
        for &b in &au {
            w.put(8, b as u32);
        }
        out.clear();
        assert!(latm.element(&w.finish(), true, &mut out));
        assert_eq!(out[0].1, au);
    }

    #[test]
    fn rfc3640_splits_aus() {
        let fmtp = "streamtype=5; profile-level-id=15; mode=AAC-hbr; config=1190; \
                    SizeLength=13; IndexLength=3; IndexDeltaLength=3";
        let d = Rfc3640::from_fmtp(fmtp).unwrap();
        let (a, b) = (vec![1u8; 10], vec![2u8; 300]);
        let mut w = BitWriter::new();
        w.put(16, 32); // two 16-bit AU headers
        w.put(13, a.len() as u32);
        w.put(3, 0);
        w.put(13, b.len() as u32);
        w.put(3, 0);
        let mut p = w.finish();
        p.extend_from_slice(&a);
        p.extend_from_slice(&b);
        let mut out = Vec::new();
        d.split(&p, &mut out).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].1.clone(), out[1].1.clone()), (a, b));
    }
}
