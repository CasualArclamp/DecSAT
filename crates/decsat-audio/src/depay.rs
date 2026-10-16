// made by CasualArclamp
//! From a stream's UDP payloads to whole audio frames.
//!
//! How a stream is carried decides how it is taken apart:
//! - **A self-framing elementary stream** (MPEG audio, ADTS, LOAS) — raw in
//!   UDP, or in RTP (MPEG audio with RFC 2250's 4-byte header when it is
//!   payload type 14) — goes through a [`Framer`].
//! - **MP4A-LATM in RTP** (RFC 3016/6416): AudioMuxElements, possibly split
//!   over packets (the marker bit ends one), with the StreamMuxConfig in the
//!   SDP (`cpresent=0`) or in band.
//! - **mpeg4-generic in RTP** (RFC 3640): AU headers and access units, the
//!   AudioSpecificConfig in the SDP.
//! - **PCM in RTP** (RFC 3551): L16/L24/L8, and G.711 µ-law and A-law.
//! - **Opus in RTP** (RFC 7587): one Opus packet a payload.

use decsat_ip::mcast::rtp_payload;
use decsat_ip::{AudioStream, Codec};

use crate::aac::{AacConfig, Latm, Rfc3640, SAMPLE_RATES, fmtp_param, hex};
use crate::es::{AdtsHeader, Framer, Framing};

/// One unit of audio, ready to decode or record.
#[derive(Debug, Clone, PartialEq)]
pub enum Unit {
    /// An MPEG audio frame, header included.
    Mpa(Vec<u8>),
    /// An AAC access unit (raw_data_block) and its configuration.
    Aac(AacConfig, Vec<u8>),
    /// PCM samples, interleaved, ±1.0.
    Pcm {
        rate: u32,
        channels: u16,
        samples: Vec<f32>,
    },
    /// MPEG-TS bytes (recorded, not played).
    Ts(Vec<u8>),
    /// One Opus packet (RFC 7587 §4.2).
    Opus(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PcmCoding {
    L16,
    L24,
    L8,
    Ulaw,
    Alaw,
}

enum Kind {
    Es {
        framer: Framer,
        latm: Latm,
    },
    LatmRtp {
        latm: Latm,
        mux_config_present: bool,
        pending: Vec<u8>,
    },
    Rfc3640(Rfc3640),
    Pcm {
        coding: PcmCoding,
        rate: u32,
        channels: u16,
    },
    Ts,
    Opus,
}

/// Turns one stream's UDP payloads into [`Unit`]s.
pub struct Depacketizer {
    kind: Kind,
    rtp: bool,
    /// Strip RFC 2250's 4-byte MPEG audio header from each RTP payload.
    mpa_header: bool,
    last_seq: Option<u16>,
    /// RTP packets missing from the sequence.
    pub lost: u64,
    /// Payloads that could not be taken apart.
    pub bad: u64,
    /// How the stream is carried, for display ("MP4A-LATM in RTP").
    pub carriage: String,
}

/// The SDP's encoding, as (name, clock rate, channels).
fn encoding(s: &AudioStream) -> Option<(String, Option<u32>, Option<u16>)> {
    let e = s.sdp.as_ref()?.encoding.as_ref()?;
    let mut f = e.split('/');
    let name = f.next()?.to_ascii_uppercase();
    let rate = f.next().and_then(|r| r.parse().ok());
    let ch = f.next().and_then(|c| c.parse().ok());
    Some((name, rate, ch))
}

impl Depacketizer {
    /// Set up for `s`, or say why it cannot be taken apart.
    pub fn new(s: &AudioStream) -> Result<Depacketizer, String> {
        let enc = encoding(s);
        let enc_name = enc.as_ref().map(|e| e.0.as_str()).unwrap_or("");
        let fmtp = s.sdp.as_ref().and_then(|d| d.fmtp.clone());
        let transport = if s.rtp { "RTP" } else { "UDP" };
        let es = |f: Framing| Kind::Es {
            framer: Framer::new(f),
            latm: Latm::new(),
        };
        let (kind, carriage) = match s.codec {
            Codec::MpegAudio => (es(Framing::Mpa), format!("MPEG audio in {transport}")),
            Codec::AacAdts => (es(Framing::Adts), format!("ADTS in {transport}")),
            Codec::AacLatm if s.rtp && enc_name == "MP4A-LATM" => {
                let f = fmtp.as_deref().unwrap_or("");
                let cpresent = fmtp_param(f, "cpresent").is_none_or(|v| v != "0");
                let latm = if cpresent {
                    Latm::new()
                } else {
                    fmtp_param(f, "config")
                        .and_then(hex)
                        .and_then(|c| Latm::with_config(&c))
                        .ok_or("MP4A-LATM: the SDP's config= is missing or not understood")?
                };
                (
                    Kind::LatmRtp {
                        latm,
                        mux_config_present: cpresent,
                        pending: Vec::new(),
                    },
                    "MP4A-LATM in RTP".to_string(),
                )
            }
            Codec::AacLatm => (es(Framing::Loas), format!("LOAS/LATM in {transport}")),
            Codec::AacRfc3640 => {
                let d = fmtp
                    .as_deref()
                    .and_then(Rfc3640::from_fmtp)
                    .ok_or("RFC 3640 AAC needs its SDP announcement (none heard yet)")?;
                (Kind::Rfc3640(d), "mpeg4-generic in RTP".to_string())
            }
            Codec::Pcm => {
                let (coding, rate, channels) = match (enc_name, s.pt) {
                    ("L16", _) => (PcmCoding::L16, 44_100, 2),
                    ("L24", _) => (PcmCoding::L24, 48_000, 2),
                    ("L8", _) => (PcmCoding::L8, 8_000, 1),
                    ("PCMU", _) | ("", Some(0)) => (PcmCoding::Ulaw, 8_000, 1),
                    ("PCMA", _) | ("", Some(8)) => (PcmCoding::Alaw, 8_000, 1),
                    ("", Some(10)) => (PcmCoding::L16, 44_100, 2),
                    ("", Some(11)) => (PcmCoding::L16, 44_100, 1),
                    _ => return Err("PCM of an unknown kind".into()),
                };
                let rate = enc.as_ref().and_then(|e| e.1).unwrap_or(rate);
                let channels = enc.as_ref().and_then(|e| e.2).unwrap_or(channels);
                (
                    Kind::Pcm {
                        coding,
                        rate,
                        channels,
                    },
                    format!("{coding:?} PCM in RTP"),
                )
            }
            Codec::Ts => (Kind::Ts, format!("MPEG-TS in {transport}")),
            Codec::Opus if s.rtp => (Kind::Opus, "Opus in RTP".to_string()),
            Codec::Opus => return Err("Opus outside RTP is not understood".into()),
            Codec::Unknown => return Err("the codec is not known".into()),
        };
        let mpa_header =
            s.codec == Codec::MpegAudio && s.rtp && (s.pt == Some(14) || enc_name == "MPA");
        Ok(Depacketizer {
            kind,
            rtp: s.rtp,
            mpa_header,
            last_seq: None,
            lost: 0,
            bad: 0,
            carriage,
        })
    }

    /// MPEG-TS streams are recorded but not played.
    pub fn is_ts(&self) -> bool {
        matches!(self.kind, Kind::Ts)
    }

    /// One UDP payload; finished units go to `out`.
    pub fn packet(&mut self, udp: &[u8], out: &mut Vec<Unit>) {
        let (payload, marker, gap) = if self.rtp {
            if udp.len() < 12 {
                self.bad += 1;
                return;
            }
            let seq = u16::from_be_bytes([udp[2], udp[3]]);
            let gap = match self.last_seq {
                Some(l) => {
                    let d = seq.wrapping_sub(l);
                    if d != 1 && d < 0x8000 {
                        self.lost += d.wrapping_sub(1) as u64;
                    }
                    d != 1
                }
                None => false,
            };
            self.last_seq = Some(seq);
            let Some(p) = rtp_payload(udp) else {
                self.bad += 1;
                return;
            };
            (p, udp[1] & 0x80 != 0, gap)
        } else {
            (udp, true, false)
        };
        match &mut self.kind {
            Kind::Es { framer, latm } => {
                if gap {
                    framer.reset();
                }
                let mut p = payload;
                if self.mpa_header && p.len() > 4 {
                    p = &p[4..];
                }
                framer.push(p);
                while let Some(f) = framer.next_frame() {
                    es_unit(&f, framer.framing(), latm, out, &mut self.bad);
                }
            }
            Kind::LatmRtp {
                latm,
                mux_config_present,
                pending,
            } => {
                if gap {
                    pending.clear();
                }
                pending.extend_from_slice(payload);
                if marker || pending.len() > 16 * 1024 {
                    let mut aus = Vec::new();
                    if latm.element(pending, *mux_config_present, &mut aus) {
                        out.extend(aus.into_iter().map(|(c, a)| Unit::Aac(c, a)));
                    } else {
                        self.bad += 1;
                    }
                    pending.clear();
                }
            }
            Kind::Rfc3640(d) => {
                let mut aus = Vec::new();
                if d.split(payload, &mut aus).is_some() {
                    out.extend(aus.into_iter().map(|(c, a)| Unit::Aac(c, a)));
                } else {
                    self.bad += 1;
                }
            }
            Kind::Pcm {
                coding,
                rate,
                channels,
            } => {
                let samples = pcm(*coding, payload);
                if !samples.is_empty() {
                    out.push(Unit::Pcm {
                        rate: *rate,
                        channels: (*channels).max(1),
                        samples,
                    });
                }
            }
            Kind::Ts => out.push(Unit::Ts(payload.to_vec())),
            Kind::Opus => {
                if !payload.is_empty() {
                    out.push(Unit::Opus(payload.to_vec()));
                }
            }
        }
    }
}

fn es_unit(f: &[u8], kind: Framing, latm: &mut Latm, out: &mut Vec<Unit>, bad: &mut u64) {
    match kind {
        Framing::Mpa => out.push(Unit::Mpa(f.to_vec())),
        Framing::Adts => {
            let Some(h) = AdtsHeader::parse(f) else {
                *bad += 1;
                return;
            };
            if h.blocks != 1 {
                *bad += 1; // several raw data blocks: no lengths to split by
                return;
            }
            let cfg = AacConfig {
                core_type: h.object_type,
                sample_rate_index: h.sample_rate_index,
                sample_rate: SAMPLE_RATES[h.sample_rate_index as usize],
                channel_config: h.channel_config,
                sbr_rate: None,
                ps: false,
                short_frames: false,
            };
            out.push(Unit::Aac(cfg, f[h.header_len..].to_vec()));
        }
        Framing::Loas => {
            let mut aus = Vec::new();
            if latm.element(&f[3..], true, &mut aus) {
                out.extend(aus.into_iter().map(|(c, a)| Unit::Aac(c, a)));
            } else {
                *bad += 1;
            }
        }
    }
}

/// G.711 µ-law to linear (ITU-T G.711).
fn ulaw(u: u8) -> i16 {
    let u = !u;
    let exp = (u >> 4) & 7;
    let mant = (u & 0x0F) as i32;
    let s = (((mant << 3) + 0x84) << exp) - 0x84;
    (if u & 0x80 != 0 { -s } else { s }) as i16
}

/// G.711 A-law to linear.
fn alaw(a: u8) -> i16 {
    let a = a ^ 0x55;
    let exp = (a >> 4) & 7;
    let mant = (a & 0x0F) as i32;
    let s = if exp == 0 {
        (mant << 4) + 8
    } else {
        ((mant << 4) + 0x108) << (exp - 1)
    };
    (if a & 0x80 != 0 { s } else { -s }) as i16
}

fn pcm(coding: PcmCoding, p: &[u8]) -> Vec<f32> {
    const K: f32 = 1.0 / 32768.0;
    match coding {
        PcmCoding::L16 => p
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| i16::from_be_bytes(c) as f32 * K)
            .collect(),
        PcmCoding::L24 => p
            .as_chunks::<3>()
            .0
            .iter()
            .map(|c| (i32::from_be_bytes([c[0], c[1], c[2], 0]) >> 8) as f32 / 8_388_608.0)
            .collect(),
        PcmCoding::L8 => p.iter().map(|&b| (b as f32 - 128.0) / 128.0).collect(),
        PcmCoding::Ulaw => p.iter().map(|&b| ulaw(b) as f32 * K).collect(),
        PcmCoding::Alaw => p.iter().map(|&b| alaw(b) as f32 * K).collect(),
    }
}

/// G.711 µ-law encoding of a linear sample (for test signals).
pub fn ulaw_encode(x: i16) -> u8 {
    const BIAS: i32 = 0x84;
    let mut s = x as i32;
    let sign = if s < 0 {
        s = -s;
        0x80
    } else {
        0
    };
    s = (s + BIAS).min(32_635 + BIAS);
    let exp = (0..8i32)
        .rev()
        .find(|&e| s & (0x4000 >> (7 - e)) != 0)
        .unwrap_or(0);
    let mant = (s >> (exp + 3)) & 0x0F;
    !(sign | (exp << 4) as u8 | mant as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aac::{loas_frame, silent_aac_au};
    use decsat_ip::mcast::{SdpInfo, rtp_packet, silent_mp2_frame};

    fn stream(rtp: bool, pt: Option<u8>, codec: Codec, sdp: Option<&str>) -> AudioStream {
        AudioStream {
            group: "239.1.1.1".parse().unwrap(),
            port: 5004,
            src: None,
            packets: 100,
            rate_bps: 1e5,
            rtp,
            pt,
            codec,
            sdp: sdp.map(SdpInfo::parse),
        }
    }

    #[test]
    fn mp2_in_rtp_pt14() {
        let mut d = Depacketizer::new(&stream(true, Some(14), Codec::MpegAudio, None)).unwrap();
        let mut out = Vec::new();
        for k in 0..5u16 {
            let mut p = vec![0, 0, 0, 0];
            p.extend_from_slice(&silent_mp2_frame());
            d.packet(&rtp_packet(14, k, 0, 1, &p), &mut out);
        }
        assert!(out.len() >= 4);
        assert!(out.iter().all(|u| *u == Unit::Mpa(silent_mp2_frame())));
    }

    #[test]
    fn loas_in_raw_udp_and_latm_in_rtp() {
        let au = silent_aac_au(2);
        let f = loas_frame(3, 2, &au);
        let mut d = Depacketizer::new(&stream(false, None, Codec::AacLatm, None)).unwrap();
        let mut out = Vec::new();
        for _ in 0..4 {
            d.packet(&f, &mut out);
        }
        assert!(out.len() >= 3);
        assert!(matches!(&out[0], Unit::Aac(c, a) if c.sample_rate == 48_000 && *a == au));

        // RFC 3016 with cpresent=1: the AudioMuxElement in band, split in two
        // packets, the marker on the second.
        let sdp = "v=0\r\ns=R\r\nc=IN IP4 239.1.1.1\r\nm=audio 5004 RTP/AVP 96\r\n\
                   a=rtpmap:96 MP4A-LATM/48000/2\r\na=fmtp:96 profile-level-id=15;object=2;cpresent=1\r\n";
        let mut d = Depacketizer::new(&stream(true, Some(96), Codec::AacLatm, Some(sdp))).unwrap();
        let el = &f[3..];
        let (a, b) = el.split_at(el.len() / 2);
        let mut out = Vec::new();
        d.packet(&rtp_packet(96, 1, 0, 1, a), &mut out);
        assert!(out.is_empty());
        let mut p2 = rtp_packet(96, 2, 0, 1, b);
        p2[1] |= 0x80;
        d.packet(&p2, &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn g711_round_trips_closely() {
        for x in [-30_000i16, -1000, -10, 0, 10, 1000, 30_000] {
            let y = ulaw(ulaw_encode(x));
            assert!(
                (y as i32 - x as i32).abs() <= (x as i32).abs() / 16 + 8,
                "{x} -> {y}"
            );
        }
        // A-law: 0xD5 is the smallest positive value, 0x55 the negative.
        assert_eq!(alaw(0xD5), 8);
        assert_eq!(alaw(0x55), -8);
    }

    #[test]
    fn rtp_loss_is_counted() {
        let mut d = Depacketizer::new(&stream(true, Some(0), Codec::Pcm, None)).unwrap();
        let mut out = Vec::new();
        for seq in [1u16, 2, 5, 6] {
            d.packet(&rtp_packet(0, seq, 0, 1, &[0xFF; 160]), &mut out);
        }
        assert_eq!(d.lost, 2);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn unplayable_streams_say_why() {
        assert!(Depacketizer::new(&stream(false, None, Codec::Opus, None)).is_err());
        assert!(Depacketizer::new(&stream(true, Some(96), Codec::AacRfc3640, None)).is_err());
    }

    #[test]
    fn opus_in_rtp_plays() {
        // libopus's packets in RTP, through the player to the null output.
        let s = stream(true, Some(96), Codec::Opus, None);
        let mut p = crate::AudioPlayer::start(&s, crate::OutputKind::Null).unwrap();
        for (k, pkt) in crate::decode::tests::opus_tone(50, 2).iter().enumerate() {
            p.packet(&rtp_packet(96, k as u16, k as u32 * 960, 5, pkt));
        }
        let h = p.handle();
        for _ in 0..200 {
            if h.status().decoded >= 50 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let st = h.status();
        assert_eq!(st.decoded, 50, "{st:?}");
        assert_eq!(st.carriage, "Opus in RTP");
    }
}
