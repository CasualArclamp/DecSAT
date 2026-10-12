//! Multicast audio: the radio feeds that satellite IP links carry as UDP
//! multicast — RTP or raw — found, named and classified.
//!
//! - **Flows**: every UDP datagram to a multicast group (224.0.0.0/4,
//!   ff00::/8) counts toward its (group, port) flow.
//! - **RTP** (RFC 3550) is recognised by version 2 headers whose SSRC holds
//!   and whose sequence numbers count up.
//! - **Codecs** from the payload's own sync words — AAC in ADTS, AAC in
//!   LOAS/LATM, MPEG audio (layer 1/2/3) — or from the RTP payload format:
//!   RFC 3640 AU headers, static payload type 14 (MPA).
//! - **SAP** (RFC 2974, port 9875) announcements carry SDP (RFC 8866) that
//!   name the streams and say exactly what they are; a stream so described
//!   is taken at its word.
//! - **Unannounced RTP** in a dynamic payload type is described from its
//!   own packets when it can be: RFC 3640 AAC by its RTP clock rate (from
//!   RTCP sender reports, else timed against the signal), the clock ticks
//!   an access unit spans (1024: AAC-LC; 2048: HE-AAC, SBR doubling the
//!   rate) and its first syntax element (SCE: mono; CPE: stereo); Opus
//!   (RFC 7587) by packets that parse as Opus under a steady TOC byte. The
//!   SDP so made plays in the app and in external players alike.
//! - **Now playing**: `<nowplaying>` XML messages (title, artist, station)
//!   sent alongside the audio are read out.
//!
//! After the VK2SWL DVB-S/S2 multicast audio receiver, which does this from
//! MPE in a transport stream; here IP may come from GSE or MPE alike.

use std::collections::BTreeMap;
use std::net::IpAddr;

use crate::packet::IpInfo;

/// The SAP port.
pub const SAP_PORT: u16 = 9875;
/// RTP packets in a row that must look right before a flow counts as RTP.
const RTP_VOTES: u32 = 3;

/// What a stream carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Codec {
    /// AAC in ADTS frames.
    AacAdts,
    /// AAC (often HE-AAC) in LOAS/LATM.
    AacLatm,
    /// AAC in RFC 3640 (mpeg4-generic) AU headers.
    AacRfc3640,
    /// MPEG-1/2 audio, layer 1, 2 or 3.
    MpegAudio,
    /// Linear or G.711 PCM.
    Pcm,
    Opus,
    /// MPEG-TS in UDP.
    Ts,
    #[default]
    Unknown,
}

impl Codec {
    pub fn label(self) -> &'static str {
        match self {
            Codec::AacAdts => "AAC (ADTS)",
            Codec::AacLatm => "AAC (LATM/LOAS)",
            Codec::AacRfc3640 => "AAC (RFC 3640)",
            Codec::MpegAudio => "MPEG audio",
            Codec::Pcm => "PCM",
            Codec::Opus => "Opus",
            Codec::Ts => "MPEG-TS",
            Codec::Unknown => "unknown",
        }
    }

    pub fn is_audio(self) -> bool {
        !matches!(self, Codec::Ts | Codec::Unknown)
    }

    /// From an SDP encoding name ("MP4A-LATM", "mpeg4-generic", …).
    fn from_encoding(e: &str) -> Codec {
        let e = e.to_ascii_uppercase();
        match e.as_str() {
            "MP4A-LATM" => Codec::AacLatm,
            "MPEG4-GENERIC" => Codec::AacRfc3640,
            "MPA" | "MP3" => Codec::MpegAudio,
            "L16" | "L24" | "L8" | "PCMU" | "PCMA" => Codec::Pcm,
            "OPUS" => Codec::Opus,
            "MP2T" => Codec::Ts,
            _ => Codec::Unknown,
        }
    }

    /// From a static RTP payload type (RFC 3551).
    fn from_static_pt(pt: u8) -> Codec {
        match pt {
            14 => Codec::MpegAudio,
            0 | 8 | 10 | 11 => Codec::Pcm,
            33 => Codec::Ts,
            _ => Codec::Unknown,
        }
    }

    /// From the bytes a payload starts with.
    fn sniff(b: &[u8]) -> Codec {
        if b.len() < 4 {
            return Codec::Unknown;
        }
        if b[0] == 0xFF && b[1] & 0xF6 == 0xF0 {
            Codec::AacAdts // 12-bit sync, layer 00
        } else if b[0] == 0x56 && b[1] & 0xE0 == 0xE0 {
            Codec::AacLatm // 11-bit LOAS sync 0x2B7
        } else if b[0] == 0xFF && b[1] & 0xE0 == 0xE0 && (b[1] >> 1) & 3 != 0 && b[2] >> 4 != 0xF {
            Codec::MpegAudio // 11-bit sync, a layer, a valid bitrate
        } else if b[0] == 0x47 && b.len().is_multiple_of(188) {
            Codec::Ts
        } else if rfc3640_aus(b).is_some() {
            Codec::AacRfc3640
        } else {
            Codec::Unknown
        }
    }
}

/// A session as SDP describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SdpInfo {
    pub name: Option<String>,
    pub info: Option<String>,
    pub group: Option<IpAddr>,
    pub port: Option<u16>,
    pub media: Option<String>,
    pub pt: Option<u8>,
    /// "MP4A-LATM/48000/2" and the like.
    pub encoding: Option<String>,
    /// The format parameters for that payload type ("profile-level-id=…;
    /// config=…"): where AAC carries its decoder configuration.
    pub fmtp: Option<String>,
    /// The SDP itself.
    pub raw: String,
    /// Made here from the stream's packets, not announced.
    pub inferred: bool,
}

impl SdpInfo {
    /// Parse the parts of an SDP that matter here (first media only).
    pub fn parse(text: &str) -> SdpInfo {
        SdpInfo::parse_for(text, None)
    }

    /// The same announcement read for payload type `pt`, the one the packets
    /// actually carry: its rtpmap and fmtp if the media line lists it, and no
    /// payload type or encoding at all if it does not (the SDP then names the
    /// stream but does not describe it).
    pub fn for_pt(&self, pt: u8) -> SdpInfo {
        SdpInfo::parse_for(&self.raw, Some(pt))
    }

    /// Parse, describing payload type `want` (`None`: the first listed).
    fn parse_for(text: &str, want: Option<u8>) -> SdpInfo {
        let mut s = SdpInfo {
            raw: text.to_string(),
            ..Default::default()
        };
        // Lines end in CRLF or LF (RFC 4566 §5) — or a bare CR, as the
        // encoders on a live DVB-S2 radio multiplex send.
        for line in text.split(['\r', '\n']) {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k {
                "s" => s.name = Some(v.trim().to_string()).filter(|n| !n.is_empty() && n != "-"),
                "i" => s.info = Some(v.trim().to_string()),
                "c" => {
                    // c=IN IP4 239.1.2.3/32 — the media-level line wins.
                    if let Some(addr) = v.split_whitespace().nth(2) {
                        let addr = addr.split('/').next().unwrap_or(addr);
                        if let Ok(a) = addr.parse() {
                            s.group = Some(a);
                        }
                    }
                }
                "m" if s.media.is_none() => {
                    // m=audio 5004 RTP/AVP 96 97 — media, port, protocol,
                    // then the payload types offered (RFC 4566 §5.14).
                    let mut f = v.split_whitespace();
                    s.media = f.next().map(str::to_string);
                    s.port = f.next().and_then(|p| p.split('/').next()?.parse().ok());
                    // `skip(1)` steps over the protocol; `filter_map` keeps
                    // the fields that parse as numbers.
                    let listed: Vec<u8> = f.skip(1).filter_map(|p| p.parse().ok()).collect();
                    s.pt = match want {
                        Some(pt) => listed.contains(&pt).then_some(pt),
                        None => listed.first().copied(),
                    };
                }
                "a" => {
                    if let Some(r) = v.strip_prefix("rtpmap:")
                        && let Some((pt, enc)) = r.split_once(' ')
                        && pt.parse::<u8>().ok() == s.pt
                    {
                        s.encoding = Some(enc.trim().to_string());
                    } else if let Some(r) = v.strip_prefix("fmtp:")
                        && let Some((pt, params)) = r.split_once(' ')
                        && pt.parse::<u8>().ok() == s.pt
                    {
                        s.fmtp = Some(params.trim().to_string());
                    }
                }
                _ => {}
            }
        }
        s
    }

    /// Every media section's group and port (RFC 4566 §5.7, §5.14: a
    /// connection line in a media section overrides the session's).
    pub fn endpoints(&self) -> Vec<(IpAddr, u16)> {
        let mut out = Vec::new();
        let mut session_c: Option<IpAddr> = None;
        let mut media: Option<(Option<IpAddr>, u16)> = None;
        let addr = |v: &str| -> Option<IpAddr> {
            v.split_whitespace().nth(2)?.split('/').next()?.parse().ok()
        };
        let mut flush = |m: &mut Option<(Option<IpAddr>, u16)>, sc: Option<IpAddr>| {
            if let Some((c, port)) = m.take()
                && let Some(g) = c.or(sc)
            {
                out.push((g, port));
            }
        };
        for line in self.raw.split(['\r', '\n']) {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k {
                "m" => {
                    flush(&mut media, session_c);
                    media = v
                        .split_whitespace()
                        .nth(1)
                        .and_then(|p| p.split('/').next()?.parse().ok())
                        .map(|port| (None, port));
                }
                "c" => match &mut media {
                    Some((c, _)) => *c = addr(v),
                    None => session_c = addr(v),
                },
                _ => {}
            }
        }
        flush(&mut media, session_c);
        out
    }

    pub fn codec(&self) -> Codec {
        match &self.encoding {
            Some(e) => Codec::from_encoding(e.split('/').next().unwrap_or("")),
            None => self.pt.map_or(Codec::Unknown, Codec::from_static_pt),
        }
    }
}

/// An SDP announcement in a UDP payload: SAP-framed (RFC 2974), or bare SDP
/// text as some encoders send to a port of their own.
pub fn announcement(p: &[u8]) -> Option<SdpInfo> {
    if p.starts_with(b"v=0") {
        let text = std::str::from_utf8(p).ok()?;
        let s = SdpInfo::parse(text);
        return (s.media.is_some() || s.name.is_some()).then_some(s);
    }
    sap_sdp(p)
}

/// The SDP in a SAP packet, if it carries one (announcements, not deletes;
/// encrypted and compressed ones are skipped).
pub fn sap_sdp(p: &[u8]) -> Option<SdpInfo> {
    if p.len() < 8 || p[0] >> 5 != 1 {
        return None; // SAP version 1
    }
    let ipv6 = p[0] & 0x10 != 0;
    let delete = p[0] & 0x04 != 0;
    let encrypted = p[0] & 0x02 != 0;
    let compressed = p[0] & 0x01 != 0;
    if delete || encrypted || compressed {
        return None;
    }
    let at = 4 + if ipv6 { 16 } else { 4 } + p[1] as usize * 4;
    let mut body = p.get(at..)?;
    // An optional payload type ("application/sdp"), NUL-terminated.
    if !body.starts_with(b"v=0")
        && let Some(nul) = body.iter().position(|&b| b == 0)
    {
        body = &body[nul + 1..];
    }
    let text = std::str::from_utf8(body).ok()?;
    text.starts_with("v=0").then(|| SdpInfo::parse(text))
}

/// The UDP payload of an IP packet, with its ports.
pub fn udp_payload<'a>(ip: &'a [u8], info: &IpInfo) -> Option<(&'a [u8], u16, u16)> {
    if info.protocol != crate::packet::PROTO_UDP {
        return None;
    }
    let ihl = if info.src.is_ipv4() {
        (ip[0] & 0x0F) as usize * 4
    } else {
        40
    };
    let udp = ip.get(ihl..info.len)?;
    if udp.len() < 8 {
        return None;
    }
    let (sport, dport) = (
        u16::from_be_bytes([udp[0], udp[1]]),
        u16::from_be_bytes([udp[2], udp[3]]),
    );
    Some((&udp[8..], sport, dport))
}

/// The RTP payload of a UDP payload already taken for RTP: past the CSRCs
/// and any header extension, padding removed.
pub fn rtp_payload(p: &[u8]) -> Option<&[u8]> {
    if p.len() < 12 || p[0] >> 6 != 2 {
        return None;
    }
    let mut at = 12 + 4 * (p[0] & 0x0F) as usize;
    if p[0] & 0x10 != 0 {
        let ext = p.get(at + 2..at + 4)?;
        at += 4 + 4 * u16::from_be_bytes([ext[0], ext[1]]) as usize;
    }
    let mut end = p.len();
    if p[0] & 0x20 != 0 {
        end = end.checked_sub(*p.last()? as usize)?;
    }
    p.get(at..end)
}

fn is_multicast(a: &IpAddr) -> bool {
    a.is_multicast()
}

/// One multicast flow.
#[derive(Debug, Clone, Default)]
struct Flow {
    src: Option<IpAddr>,
    packets: u64,
    bytes: u64,
    /// UDP checksums checked and failed: a flow whose checksums all fail is
    /// encrypted in transit, and its random payloads would "sniff" as audio
    /// now and then (the Q-Flex sites' multicast read as MPEG audio and
    /// AAC).
    udp_checked: u64,
    udp_bad: u64,
    rtp_votes: u32,
    rtp: bool,
    ssrc: Option<u32>,
    seq: Option<u16>,
    pt: Option<u8>,
    sniffed: BTreeMap<u8, u32>,
    win_bytes: u64,
    rate_bps: f64,
    /// The last payload's first byte (Opus's TOC byte holds steady).
    first: Option<u8>,
    /// The last RTP timestamp, and the access units that packet held
    /// (RFC 3640).
    ts: Option<u32>,
    aus: Option<u32>,
    /// RTP clock ticks per access unit, as seen between packets, tallied.
    au_ticks: BTreeMap<u32, u32>,
    /// Channels: from the first AU's first syntax element, or Opus's TOC.
    channels: Option<u8>,
    /// The RTP clock timed against the signal: when the measurement began,
    /// the last packet's time, and the timestamps' advance (unwrapped)
    /// since, for streams without RTCP.
    t0: Option<f64>,
    t_last: f64,
    ts_adv: u64,
}

/// One sender's RTCP sender reports (RFC 3550 §6.4.1): the first and the
/// latest (NTP time in seconds, RTP timestamp) pair — two clocks read at
/// one instant, so between them they give the RTP clock's rate.
#[derive(Debug, Clone, Copy)]
struct SenderClock {
    first: (f64, u32),
    last: (f64, u32),
}

/// What one now-playing message says (`<nowplaying><title>…</title>
/// <artist>…</artist><station>…</station><media_type>…</media_type>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NowPlaying {
    pub station: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    /// SONG, UNSPECIFIED, … as sent.
    pub kind: Option<String>,
    /// Where the messages come from.
    pub group: IpAddr,
    pub port: u16,
}

/// RTP clock rates a measurement snaps to (RFC 3551 and the AAC rates).
const CLOCK_RATES: [u32; 13] = [
    8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 64000, 88200, 90000, 96000,
];

/// `measured` snapped to the nearest standard rate, if within `tol` (a
/// fraction) of it.
fn snap_rate(measured: f64, tol: f64) -> Option<u32> {
    CLOCK_RATES
        .iter()
        .copied()
        .min_by(|a, b| {
            (measured - *a as f64)
                .abs()
                .total_cmp(&(measured - *b as f64).abs())
        })
        .filter(|r| (measured / *r as f64 - 1.0).abs() <= tol)
}

/// The access-unit sizes of an RFC 3640 payload in AAC-hbr mode — 13-bit
/// sizes and 3-bit indices (RFC 3640 §3.3.6) — if they account for every
/// byte of it.
fn rfc3640_aus(b: &[u8]) -> Option<Vec<usize>> {
    let bits = u16::from_be_bytes([*b.first()?, *b.get(1)?]) as usize;
    if bits == 0 || !bits.is_multiple_of(16) {
        return None;
    }
    let n = bits / 16;
    // `collect` into `Option<Vec<_>>` gives `None` if any size is missing.
    let sizes: Vec<usize> = (0..n)
        .map(|i| {
            let h = b.get(2 + 2 * i..4 + 2 * i)?;
            Some((u16::from_be_bytes([h[0], h[1]]) >> 3) as usize)
        })
        .collect::<Option<_>>()?;
    (2 + 2 * n + sizes.iter().sum::<usize>() == b.len()).then_some(sizes)
}

/// An Opus frame length (RFC 6716 §3.2.1): one byte, or two from 252 up.
/// The length and the bytes it took.
fn opus_len(b: &[u8]) -> Option<(usize, usize)> {
    let l0 = *b.first()? as usize;
    if l0 < 252 {
        Some((l0, 1))
    } else {
        Some((l0 + 4 * *b.get(1)? as usize, 2))
    }
}

/// Whether `b` parses as one Opus packet (RFC 6716 §3.2): its frame-count
/// code, frame lengths and padding add up to its length, and it holds at
/// most 120 ms of audio.
fn opus_packet_ok(b: &[u8]) -> bool {
    let Some(&toc) = b.first() else {
        return false;
    };
    let config = toc >> 3;
    // Frame duration in 2.5 ms units (§3.1, Table 2): SILK, hybrid, CELT.
    let d25 = match config {
        0..=11 => [4, 8, 16, 24][(config % 4) as usize],
        12..=15 => [4, 8][(config % 2) as usize],
        _ => [1, 2, 4, 8][(config % 4) as usize],
    };
    let rest = &b[1..];
    match toc & 3 {
        // One frame; two equal frames.
        0 => rest.len() <= 1275,
        1 => rest.len().is_multiple_of(2) && rest.len() / 2 <= 1275,
        // Two frames, the first's length coded.
        2 => opus_len(rest).is_some_and(|(l, h)| l <= rest.len() - h),
        // A frame count byte: VBR, padding, count; then padding lengths,
        // frame lengths (VBR), frames and padding.
        _ => {
            let Some(&fc) = rest.first() else {
                return false;
            };
            let m = (fc & 0x3F) as usize;
            if m == 0 || m * d25 > 48 {
                return false;
            }
            let mut at = 1;
            let mut pad = 0;
            if fc & 0x40 != 0 {
                loop {
                    let Some(&p) = rest.get(at) else {
                        return false;
                    };
                    at += 1;
                    if p == 255 {
                        pad += 254;
                    } else {
                        pad += p as usize;
                        break;
                    }
                }
            }
            let mut used = 0;
            if fc & 0x80 != 0 {
                for _ in 1..m {
                    let Some((l, h)) = rest.get(at..).and_then(opus_len) else {
                        return false;
                    };
                    at += h;
                    used += l;
                }
            }
            let Some(frames) = rest.len().checked_sub(at + pad) else {
                return false;
            };
            if fc & 0x80 != 0 {
                used <= frames && frames - used <= 1275
            } else {
                frames.is_multiple_of(m) && frames / m <= 1275
            }
        }
    }
}

/// An AudioSpecificConfig (ISO/IEC 14496-3 §1.6.2.1): AAC-LC at `rate`, or
/// with `sbr` HE-AAC signalled explicitly (object type 5, §1.6.5: the core
/// at half `rate`, the output at `rate`); 960-sample frames if `short`.
fn aac_config(rate: u32, channels: u8, sbr: bool, short: bool) -> Option<Vec<u8>> {
    // Sampling frequency index (§1.6.3.4, Table 1.18).
    const RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    let index = |r: u32| RATES.iter().position(|&x| x == r).map(|i| i as u64);
    let mut fields: Vec<(u64, u32)> = Vec::new();
    if sbr {
        if !rate.is_multiple_of(2) {
            return None;
        }
        fields.extend([
            (5, 5),                // audioObjectType: SBR
            (index(rate / 2)?, 4), // the core's sampling frequency index
            (channels as u64, 4),  // channelConfiguration
            (index(rate)?, 4),     // extensionSamplingFrequencyIndex
            (2, 5),                // the core's audioObjectType: AAC-LC
        ]);
    } else {
        fields.extend([(2, 5), (index(rate)?, 4), (channels as u64, 4)]);
    }
    // GASpecificConfig (§4.4.1): frameLengthFlag, dependsOnCoreCoder,
    // extensionFlag.
    fields.extend([(short as u64, 1), (0, 1), (0, 1)]);
    let n: u32 = fields.iter().map(|f| f.1).sum();
    let bits = fields.iter().fold(0u64, |acc, &(v, w)| acc << w | v);
    let bytes = n.div_ceil(8);
    let bits = bits << (bytes * 8 - n);
    Some((0..bytes).rev().map(|k| (bits >> (8 * k)) as u8).collect())
}

/// The text of `<name>…</name>` in `x`, entities undone.
fn xml_tag(x: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let a = x.find(&open)? + open.len();
    let b = a + x[a..].find(&format!("</{name}>"))?;
    let v = x[a..b]
        .trim()
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    Some(v).filter(|v| !v.is_empty())
}

/// A stream for display and playback.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioStream {
    pub group: IpAddr,
    pub port: u16,
    pub src: Option<IpAddr>,
    pub packets: u64,
    pub rate_bps: f64,
    pub rtp: bool,
    pub pt: Option<u8>,
    pub codec: Codec,
    /// What a SAP announcement says about it.
    pub sdp: Option<SdpInfo>,
}

impl AudioStream {
    /// The announced name, else the address.
    pub fn name(&self) -> String {
        self.sdp
            .as_ref()
            .and_then(|s| s.name.clone())
            .unwrap_or_else(|| format!("{}:{}", self.group, self.port))
    }
}

/// Watches IP packets for multicast audio.
#[derive(Default)]
pub struct McastScanner {
    flows: BTreeMap<(IpAddr, u16), Flow>,
    /// Sessions announced by SAP, keyed by their media group and port.
    pub announced: BTreeMap<(IpAddr, u16), SdpInfo>,
    pub sap_packets: u64,
    win_secs: f64,
    /// Signal time, seconds (from `tick`).
    now: f64,
    /// RTCP sender reports, by SSRC.
    rtcp: BTreeMap<u32, SenderClock>,
    /// The latest now-playing message, by station.
    playing: BTreeMap<String, NowPlaying>,
}

/// Flows kept at most (a link carrying thousands is not radio).
const MAX_FLOWS: usize = 512;

/// Index of a codec in the sniff tally.
fn codec_index(c: Codec) -> u8 {
    c as u8
}

const CODECS: [Codec; 8] = [
    Codec::AacAdts,
    Codec::AacLatm,
    Codec::AacRfc3640,
    Codec::MpegAudio,
    Codec::Pcm,
    Codec::Opus,
    Codec::Ts,
    Codec::Unknown,
];

impl McastScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look at one IP packet.
    pub fn packet(&mut self, ip: &[u8], info: &IpInfo) {
        if !is_multicast(&info.dst) {
            return;
        }
        let Some((payload, _, dport)) = udp_payload(ip, info) else {
            return;
        };
        // Announcements: SAP's port, or SDP sent anywhere (bare, or in a SAP
        // header on another port).
        let sdp = if dport == SAP_PORT {
            self.sap_packets += 1;
            sap_sdp(payload)
        } else if payload.starts_with(b"v=0") || (payload.len() > 8 && payload[0] >> 5 == 1) {
            announcement(payload)
        } else {
            None
        };
        if let Some(sdp) = sdp {
            if dport != SAP_PORT {
                self.sap_packets += 1;
            }
            for key in sdp.endpoints() {
                self.announced.insert(key, sdp.clone());
            }
            return;
        }
        if dport == SAP_PORT {
            return;
        }
        // RTCP sender reports (RFC 3550 §6.4.1): version 2, packet type 200.
        if payload.len() >= 28 && payload[0] >> 6 == 2 && payload[1] == 200 {
            let word = |i: usize| u32::from_be_bytes(payload[i..i + 4].try_into().unwrap());
            let sr = (word(8) as f64 + word(12) as f64 / 4_294_967_296.0, word(16));
            let ssrc = word(4);
            if let Some(c) = self.rtcp.get_mut(&ssrc) {
                c.last = sr;
            } else if self.rtcp.len() < MAX_FLOWS {
                self.rtcp.insert(
                    ssrc,
                    SenderClock {
                        first: sr,
                        last: sr,
                    },
                );
            }
            return;
        }
        if let Some(at) = payload
            .get(..payload.len().min(16))
            .and_then(|h| h.windows(12).position(|w| w == b"<nowplaying>"))
        {
            let x = String::from_utf8_lossy(&payload[at..]);
            if let Some(station) = xml_tag(&x, "station")
                && (self.playing.len() < 64 || self.playing.contains_key(&station))
            {
                let np = NowPlaying {
                    station: station.clone(),
                    title: xml_tag(&x, "title"),
                    artist: xml_tag(&x, "artist"),
                    kind: xml_tag(&x, "media_type"),
                    group: info.dst,
                    port: dport,
                };
                self.playing.insert(station, np);
            }
            return;
        }
        let key = (info.dst, dport);
        if !self.flows.contains_key(&key) && self.flows.len() >= MAX_FLOWS {
            return;
        }
        let f = self.flows.entry(key).or_default();
        f.src = Some(info.src);
        f.packets += 1;
        if let Some(ok) = crate::packet::udp_checksum_ok(ip, info) {
            f.udp_checked += 1;
            f.udp_bad += u64::from(!ok);
        }
        f.bytes += payload.len() as u64;
        f.win_bytes += payload.len() as u64;

        // RTP: version 2, a steady SSRC, sequence numbers counting up.
        let prev_seq = f.seq;
        let looks_rtp = payload.len() >= 12
            && payload[0] >> 6 == 2
            && !(72..=76).contains(&(payload[1] & 0x7F)); // not RTCP
        if looks_rtp {
            let ssrc = u32::from_be_bytes(payload[8..12].try_into().unwrap());
            let seq = u16::from_be_bytes([payload[2], payload[3]]);
            if f.ssrc == Some(ssrc) && f.seq.is_some_and(|s| s.wrapping_add(1) == seq) {
                f.rtp_votes += 1;
            }
            f.ssrc = Some(ssrc);
            f.seq = Some(seq);
            if f.rtp_votes >= RTP_VOTES {
                f.rtp = true;
                f.pt = Some(payload[1] & 0x7F);
            }
        } else {
            f.rtp_votes = f.rtp_votes.saturating_sub(1);
        }
        let body = if f.rtp {
            rtp_payload(payload).unwrap_or(&[])
        } else {
            payload
        };
        let mut c = Codec::sniff(body);
        if c == Codec::Unknown && f.rtp && f.pt == Some(14) && body.len() > 4 {
            c = Codec::sniff(&body[4..]); // RFC 2250's 4-byte MPA header
        }
        if c == Codec::Unknown
            && f.rtp
            && f.pt.is_some_and(|p| p >= 96)
            && f.first == body.first().copied()
            && opus_packet_ok(body)
        {
            c = Codec::Opus;
            f.channels = Some(if body[0] & 0x04 != 0 { 2 } else { 1 });
        }
        f.first = body.first().copied();
        if f.rtp && payload.len() >= 12 {
            // The RTP clock: timestamps' advance against the signal's time,
            // and the ticks an access unit spans.
            let ts = u32::from_be_bytes(payload[4..8].try_into().unwrap());
            let seq = u16::from_be_bytes([payload[2], payload[3]]);
            let d = f.ts.map(|last| ts.wrapping_sub(last));
            match d {
                // Forward, by under three minutes at 96 kHz.
                Some(d) if d < 1 << 24 => {
                    f.ts_adv += u64::from(d);
                    f.t_last = self.now;
                    if let Some(n) = f.aus.filter(|&n| n > 0)
                        && prev_seq.is_some_and(|s| s.wrapping_add(1) == seq)
                        && d > 0
                        && d.is_multiple_of(n)
                    {
                        *f.au_ticks.entry(d / n).or_default() += 1;
                    }
                }
                // The first, or a jump: measure afresh.
                _ => {
                    f.t0 = Some(self.now);
                    f.t_last = self.now;
                    f.ts_adv = 0;
                }
            }
            f.ts = Some(ts);
            f.aus = None;
            if c == Codec::AacRfc3640
                && let Some(sizes) = rfc3640_aus(body)
            {
                f.aus = Some(sizes.len() as u32);
                // The first raw_data_block's first element (ISO/IEC
                // 14496-3 §4.4.2.1): 0 a single channel, 1 a channel pair.
                match body.get(2 + 2 * sizes.len()).map(|b| b >> 5) {
                    Some(0) => f.channels = Some(1),
                    Some(1) => f.channels = Some(2),
                    _ => {}
                }
            }
        }
        *f.sniffed.entry(codec_index(c)).or_default() += 1;
    }

    /// A flow's RTP clock rate: from its sender's RTCP reports over 1.5 s
    /// or more, else its timestamps against the signal over 4 s or more.
    fn clock_rate(&self, f: &Flow) -> Option<u32> {
        let rtcp = f.ssrc.and_then(|s| self.rtcp.get(&s)).and_then(|c| {
            let dt = c.last.0 - c.first.0;
            (dt >= 1.5)
                .then(|| snap_rate(c.last.1.wrapping_sub(c.first.1) as f64 / dt, 0.01))
                .flatten()
        });
        rtcp.or_else(|| {
            let dt = f.t_last - f.t0?;
            (dt >= 4.0)
                .then(|| snap_rate(f.ts_adv as f64 / dt, 0.03))
                .flatten()
        })
    }

    /// An SDP for an unannounced RTP stream in a dynamic payload type,
    /// made from what its packets show, once they show enough.
    fn described(&self, group: IpAddr, port: u16, f: &Flow, codec: Codec) -> Option<SdpInfo> {
        let pt = f.pt.filter(|&p| f.rtp && p >= 96)?;
        let stereo = |ch: u8| if ch == 1 { "mono" } else { "stereo" };
        let (encoding, fmtp, what) = match codec {
            // RFC 7587 §7: always opus/48000/2, whatever is sent.
            Codec::Opus => {
                let ch = f.channels.unwrap_or(2);
                (
                    "opus/48000/2".to_string(),
                    format!("stereo={0}; sprop-stereo={0}", u8::from(ch == 2)),
                    format!("Opus · {}", stereo(ch)),
                )
            }
            Codec::AacRfc3640 => {
                let rate = self.clock_rate(f)?;
                let ch = f.channels?;
                let ticks = f
                    .au_ticks
                    .iter()
                    .max_by_key(|(_, n)| **n)
                    .map(|(t, _)| *t)?;
                let (sbr, short) = match ticks {
                    1024 => (false, false),
                    960 => (false, true),
                    2048 => (true, false),
                    1920 => (true, true),
                    _ => return None,
                };
                let config = aac_config(rate, ch, sbr, short)?;
                let hex: String = config.iter().map(|b| format!("{b:02x}")).collect();
                (
                    format!("mpeg4-generic/{rate}/{ch}"),
                    format!(
                        "streamtype=5; profile-level-id={}; mode=AAC-hbr; config={hex}; \
                         sizelength=13; indexlength=3; indexdeltalength=3",
                        if sbr { 44 } else { 41 }
                    ),
                    format!(
                        "{} · {:.1} kHz · {}",
                        if sbr { "HE-AAC" } else { "AAC-LC" },
                        rate as f64 / 1000.0,
                        stereo(ch)
                    ),
                )
            }
            _ => return None,
        };
        let ip = if group.is_ipv4() { "IP4" } else { "IP6" };
        let raw = format!(
            "v=0\r\no=- 0 0 IN {ip} {src}\r\ns=-\r\n\
             i=Not announced: described from its packets ({what})\r\n\
             c=IN {ip} {group}\r\nt=0 0\r\nm=audio {port} RTP/AVP {pt}\r\n\
             a=rtpmap:{pt} {encoding}\r\na=fmtp:{pt} {fmtp}\r\n",
            src = f.src.unwrap_or(group),
        );
        let mut s = SdpInfo::parse(&raw);
        s.inferred = true;
        Some(s)
    }

    /// The latest now-playing message from each station, by name.
    pub fn now_playing(&self) -> Vec<NowPlaying> {
        self.playing.values().cloned().collect()
    }

    /// Signal time passed: update the flow rates once a second's worth is in.
    pub fn tick(&mut self, secs: f64) {
        self.now += secs;
        self.win_secs += secs;
        if self.win_secs < 1.0 {
            return;
        }
        for f in self.flows.values_mut() {
            f.rate_bps = f.win_bytes as f64 * 8.0 / self.win_secs;
            f.win_bytes = 0;
        }
        self.win_secs = 0.0;
    }

    /// Whether `key` is (still) a known flow.
    pub fn has_flow(&self, group: IpAddr, port: u16) -> bool {
        self.flows.contains_key(&(group, port))
    }

    /// The announcement for a stream: its group and port, else the only one
    /// for its group, else the only one for its port (senders behind NAT or
    /// with a stale SDP get one of the two wrong).
    fn announcement_for(&self, group: IpAddr, port: u16) -> Option<SdpInfo> {
        if let Some(s) = self.announced.get(&(group, port)) {
            return Some(s.clone());
        }
        let only = |f: &dyn Fn(&(IpAddr, u16)) -> bool| {
            let mut it = self.announced.iter().filter(|(k, _)| f(k));
            match (it.next(), it.next()) {
                (Some((_, s)), None) => Some(s.clone()),
                _ => None,
            }
        };
        only(&|k| k.0 == group).or_else(|| only(&|k| k.1 == port))
    }

    /// Every station announced so far, by name (one per media address).
    pub fn stations(&self) -> Vec<(IpAddr, u16, SdpInfo)> {
        self.announced
            .iter()
            .map(|(&(g, p), s)| (g, p, s.clone()))
            .collect()
    }

    /// The multicast streams that carry audio (or are announced as audio),
    /// by group address and port.
    pub fn streams(&self) -> Vec<AudioStream> {
        let mut v: Vec<AudioStream> = self
            .flows
            .iter()
            // Not an encrypted flow (as `stats::Flow::encrypted`).
            .filter(|(_, f)| !(f.udp_checked >= 8 && f.udp_bad * 20 >= f.udp_checked * 19))
            .map(|(&(group, port), f)| {
                // An encoder may announce one payload type and send another
                // (a radio multiplex announced MPEG audio as type 14 and sent
                // ADTS as type 99): then the SDP names the stream, but its
                // codec is taken from the packets.
                let sdp = self.announcement_for(group, port).map(|s| match f.pt {
                    Some(pt) if f.rtp && s.pt != Some(pt) => s.for_pt(pt),
                    _ => s,
                });
                // Opus has no sync word, so it must hold for half the
                // packets, not just win the tally.
                let sniffed = f
                    .sniffed
                    .iter()
                    .filter(|(c, n)| match CODECS[**c as usize] {
                        Codec::Unknown => false,
                        Codec::Opus => **n as u64 * 2 >= f.packets,
                        _ => true,
                    })
                    .max_by_key(|(_, n)| **n)
                    .map(|(&c, _)| CODECS[c as usize])
                    .unwrap_or(Codec::Unknown);
                let sdp = sdp.or_else(|| self.described(group, port, f, sniffed));
                let codec = match (&sdp, f.rtp) {
                    (Some(s), _) if s.codec() != Codec::Unknown => s.codec(),
                    (_, true) if sniffed == Codec::Unknown => {
                        f.pt.map_or(Codec::Unknown, Codec::from_static_pt)
                    }
                    _ => sniffed,
                };
                AudioStream {
                    group,
                    port,
                    src: f.src,
                    packets: f.packets,
                    rate_bps: f.rate_bps,
                    rtp: f.rtp,
                    pt: f.pt,
                    codec,
                    sdp,
                }
            })
            .filter(|s| {
                s.codec.is_audio()
                    || s.sdp
                        .as_ref()
                        .is_some_and(|d| d.media.as_deref() == Some("audio"))
            })
            .collect();
        // By address, so the list stays put while rates move about.
        v.sort_by_key(|a| (a.group, a.port));
        v
    }
}

/// Build a SAP announcement of `sdp` (for tests and test signals).
pub fn sap_packet(origin: [u8; 4], msg_id: u16, sdp: &str) -> Vec<u8> {
    let mut p = vec![0x20, 0, (msg_id >> 8) as u8, msg_id as u8];
    p.extend_from_slice(&origin);
    p.extend_from_slice(b"application/sdp\0");
    p.extend_from_slice(sdp.as_bytes());
    p
}

/// Build an RTP packet (for tests and test signals).
pub fn rtp_packet(pt: u8, seq: u16, ts: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0x80, pt & 0x7F];
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&ts.to_be_bytes());
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

/// A silent MPEG-1 layer II frame: 48 kHz, 128 kbit/s, stereo — the header,
/// then every allocation zero (no samples coded), so it decodes to silence.
pub fn silent_mp2_frame() -> Vec<u8> {
    let mut f = vec![0u8; 384];
    f[..4].copy_from_slice(&[0xFF, 0xFD, 0x84, 0x04]);
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{parse, udp_v4};

    fn feed(s: &mut McastScanner, p: &[u8]) {
        let info = parse(p).unwrap();
        s.packet(p, &info);
    }

    #[test]
    fn finds_an_announced_rtp_mp2_stream() {
        let mut s = McastScanner::new();
        let sdp = "v=0\r\no=- 1 1 IN IP4 10.0.0.1\r\ns=Test Radio\r\nc=IN IP4 239.255.1.1/32\r\n\
                   t=0 0\r\nm=audio 5004 RTP/AVP 14\r\n";
        feed(
            &mut s,
            &udp_v4(
                [10, 0, 0, 1],
                [224, 2, 127, 254],
                9875,
                9875,
                &sap_packet([10, 0, 0, 1], 7, sdp),
            ),
        );
        for k in 0..10u16 {
            let mut payload = vec![0u8; 4]; // RFC 2250 header
            payload.extend_from_slice(&silent_mp2_frame());
            let rtp = rtp_packet(14, k, k as u32 * 2160, 0xABCD, &payload);
            feed(
                &mut s,
                &udp_v4([10, 0, 0, 1], [239, 255, 1, 1], 4000, 5004, &rtp),
            );
        }
        s.tick(1.0);
        let v = s.streams();
        assert_eq!(v.len(), 1);
        let a = &v[0];
        assert_eq!(a.name(), "Test Radio");
        assert!(a.rtp);
        assert_eq!(a.pt, Some(14));
        assert_eq!(a.codec, Codec::MpegAudio);
        assert!(a.rate_bps > 0.0);
    }

    #[test]
    fn sniffs_raw_adts_and_latm_without_announcements() {
        let mut s = McastScanner::new();
        let adts = [0xFF, 0xF1, 0x50, 0x80, 0x02, 0x1F, 0xFC, 0, 0, 0];
        let loas = [0x56, 0xE0, 0x20, 0, 0, 0, 0, 0];
        for _ in 0..5 {
            feed(
                &mut s,
                &udp_v4([1, 1, 1, 1], [239, 1, 1, 1], 1, 1234, &adts),
            );
            feed(
                &mut s,
                &udp_v4([1, 1, 1, 1], [239, 1, 1, 2], 1, 1234, &loas),
            );
            // Unicast and non-audio multicast are ignored.
            feed(&mut s, &udp_v4([1, 1, 1, 1], [10, 1, 1, 2], 1, 1234, &adts));
            feed(
                &mut s,
                &udp_v4([1, 1, 1, 1], [239, 1, 1, 3], 1, 1234, b"hello world!"),
            );
        }
        let v = s.streams();
        assert_eq!(v.len(), 2);
        assert!(v.iter().any(|a| a.codec == Codec::AacAdts && !a.rtp));
        assert!(v.iter().any(|a| a.codec == Codec::AacLatm));
    }

    #[test]
    fn bare_sdp_on_any_port_names_its_streams() {
        // Bare SDP (no SAP header) to a port of its own, two media sections,
        // the second with its own group; the audio flows then carry names.
        let mut s = McastScanner::new();
        let sdp = "v=0\r\no=- 4 13 IN IP4 192.168.1.14\r\ns=Classic_Hits\r\ni=Classic_Hits\r\n\
                   c=IN IP4 230.0.0.3/16\r\nt=0 0\r\nm=audio 10001 RTP/AVP 14\r\n\
                   m=audio 10005 RTP/AVP 14\r\nc=IN IP4 230.0.0.5/16\r\n";
        feed(
            &mut s,
            &udp_v4([10, 1, 1, 1], [230, 0, 0, 1], 5000, 5555, sdp.as_bytes()),
        );
        for (g, port) in [([230, 0, 0, 3], 10001u16), ([230, 0, 0, 5], 10005)] {
            for k in 0..10u16 {
                let mut payload = vec![0u8; 4];
                payload.extend_from_slice(&silent_mp2_frame());
                let rtp = rtp_packet(14, k, k as u32 * 2160, 0x1234, &payload);
                feed(&mut s, &udp_v4([10, 1, 1, 1], g, 4000, port, &rtp));
            }
        }
        let v = s.streams();
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(v.iter().all(|a| a.name() == "Classic_Hits"), "{v:?}");
        assert_eq!(s.stations().len(), 2);
    }

    #[test]
    fn sap_with_bare_cr_lines_names_the_station() {
        // As sent on a live DVB-S2 radio multiplex: SAP to 224.2.127.254,
        // SDP lines ended by a bare CR, announcing payload type 14 (MPEG
        // audio) — while the packets are type 99, ADTS behind an RTP header
        // extension.
        let mut s = McastScanner::new();
        let sdp = "v=0\ro=- 1 47 IN IP4 192.168.1.11\rs=Newstalk_ZB\ri=Newstalk_ZB\r\
                   a=X-PID:1001\rm=audio 10001 RTP/AVP 14\rc=IN IP4 230.0.0.1\ra=bitrate:0\r";
        feed(
            &mut s,
            &udp_v4(
                [192, 168, 1, 11],
                [224, 2, 127, 254],
                9875,
                9875,
                &sap_packet([192, 168, 1, 11], 47, sdp),
            ),
        );
        let adts = [0xFF, 0xF1, 0x50, 0x80, 0x02, 0x1F, 0xFC, 0, 0, 0];
        for k in 0..10u16 {
            let mut rtp = rtp_packet(99, k, k as u32 * 1024, 0x99, &[]);
            rtp[0] |= 0x10; // a header extension: profile, one word
            rtp.extend_from_slice(&[0x56, 0x85, 0, 1, 0, 0, 0, 3]);
            rtp.extend_from_slice(&adts);
            feed(
                &mut s,
                &udp_v4([10, 152, 26, 11], [230, 0, 0, 1], 4000, 10001, &rtp),
            );
        }
        let v = s.streams();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name(), "Newstalk_ZB");
        assert_eq!(v[0].pt, Some(99));
        assert_eq!(v[0].codec, Codec::AacAdts);
        // The announcement no longer claims to describe the packets.
        assert_eq!(v[0].sdp.as_ref().unwrap().pt, None);
        assert_eq!(s.stations()[0].2.pt, Some(14));
    }

    #[test]
    fn sdp_for_a_listed_payload_type_takes_its_rtpmap() {
        let s = SdpInfo::parse("v=0\nm=audio 6000 RTP/AVP 14 96\na=rtpmap:96 MP4A-LATM/48000/2\n");
        assert_eq!(s.pt, Some(14));
        assert_eq!(s.codec(), Codec::MpegAudio);
        let t = s.for_pt(96);
        assert_eq!(t.pt, Some(96));
        assert_eq!(t.codec(), Codec::AacLatm);
        assert_eq!(s.for_pt(99).codec(), Codec::Unknown);
    }

    #[test]
    fn sdp_parses_the_media_line_and_rtpmap() {
        let s = SdpInfo::parse(
            "v=0\ns=Radio\nc=IN IP4 239.0.0.9/16\nm=audio 6000 RTP/AVP 96\na=rtpmap:96 MP4A-LATM/48000/2\n",
        );
        assert_eq!(s.port, Some(6000));
        assert_eq!(s.pt, Some(96));
        assert_eq!(s.group, Some("239.0.0.9".parse().unwrap()));
        assert_eq!(s.codec(), Codec::AacLatm);
    }

    /// An RFC 3640 payload of `aus` access units of `size` bytes, each
    /// starting with a channel-pair element (as a stereo raw_data_block).
    fn rfc3640(aus: usize, size: usize) -> Vec<u8> {
        let mut p = ((aus * 16) as u16).to_be_bytes().to_vec();
        for _ in 0..aus {
            p.extend_from_slice(&((size as u16) << 3).to_be_bytes());
        }
        for _ in 0..aus {
            let mut au = vec![0x5Au8; size];
            au[0] = 0x21; // ID_CPE, tag 0, common window
            p.extend_from_slice(&au);
        }
        p
    }

    /// An RTCP sender report: `ssrc`, NTP time `t` s, RTP timestamp `ts`.
    fn sender_report(ssrc: u32, t: f64, ts: u32) -> Vec<u8> {
        let mut p = vec![0x80, 200, 0, 6];
        p.extend_from_slice(&ssrc.to_be_bytes());
        p.extend_from_slice(&((3_000_000_000.0 + t) as u32).to_be_bytes());
        p.extend_from_slice(&((t.fract() * 4_294_967_296.0) as u32).to_be_bytes());
        p.extend_from_slice(&ts.to_be_bytes());
        p.extend_from_slice(&[0; 8]);
        p
    }

    const SRC: [u8; 4] = [192, 168, 1, 89];

    #[test]
    fn describes_unannounced_he_aac_from_its_rtcp() {
        // As a radio multiplex sends it: four 2048-tick AUs a packet at a
        // 44.1 kHz RTP clock (HE-AAC), RTCP every five seconds, no SAP.
        let mut s = McastScanner::new();
        let ssrc = 0x48AB_581F;
        for k in 0..120u32 {
            let ts = 0x17BB_51B4u32.wrapping_add(k * 8192);
            let rtp = rtp_packet(96, k as u16, ts, ssrc, &rfc3640(4, 540));
            feed(&mut s, &udp_v4(SRC, [225, 0, 0, 20], 6020, 6020, &rtp));
            if k % 27 == 0 {
                let t = k as f64 * 8192.0 / 44100.0;
                let sr = sender_report(ssrc, t, ts);
                feed(&mut s, &udp_v4(SRC, [225, 0, 0, 20], 6021, 6021, &sr));
            }
            s.tick(8192.0 / 44100.0 * 0.97); // our clock a little off
        }
        let v = s.streams();
        assert_eq!(v.len(), 1, "RTCP makes no stream of its own");
        let a = &v[0];
        assert_eq!(a.codec, Codec::AacRfc3640);
        let d = a.sdp.as_ref().expect("described from the packets");
        assert!(d.inferred);
        assert_eq!(d.encoding.as_deref(), Some("mpeg4-generic/44100/2"));
        let fmtp = d.fmtp.as_deref().unwrap();
        assert!(fmtp.contains("config=2b920800"), "{fmtp}");
        assert!(
            d.info
                .as_deref()
                .unwrap()
                .contains("HE-AAC · 44.1 kHz · stereo")
        );
        assert_eq!(d.group, Some("225.0.0.20".parse().unwrap()));
        assert_eq!(d.port, Some(6020));
    }

    #[test]
    fn times_an_rtp_clock_without_rtcp() {
        // Eight 1024-tick AUs a packet (AAC-LC) at 44.1 kHz, no RTCP.
        let mut s = McastScanner::new();
        for k in 0..40u32 {
            let rtp = rtp_packet(96, k as u16, k * 8192, 7, &rfc3640(8, 280));
            feed(&mut s, &udp_v4(SRC, [225, 0, 0, 2], 6002, 6002, &rtp));
            s.tick(8192.0 / 44100.0);
            let early = s.streams().first().is_none_or(|a| a.sdp.is_none());
            assert!(k > 20 || early, "{k}: too soon to say");
        }
        let a = &s.streams()[0];
        let d = a.sdp.as_ref().expect("timed after four seconds");
        assert_eq!(d.encoding.as_deref(), Some("mpeg4-generic/44100/2"));
        assert!(d.fmtp.as_deref().unwrap().contains("config=1210"));
    }

    #[test]
    fn finds_unannounced_opus() {
        // Five 20 ms CELT frames (TOC 0xFF), CBR, 7 bytes of padding.
        let mut opus = vec![0xFF, 0x45, 0x07];
        opus.extend((0..5 * 248 + 7).map(|i| (i * 31) as u8));
        assert!(opus_packet_ok(&opus));
        let mut s = McastScanner::new();
        for k in 0..20u32 {
            let rtp = rtp_packet(96, k as u16, k * 4800, 9, &opus);
            feed(&mut s, &udp_v4(SRC, [225, 0, 0, 20], 6012, 6012, &rtp));
        }
        let a = &s.streams()[0];
        assert_eq!(a.codec, Codec::Opus);
        let d = a.sdp.as_ref().unwrap();
        assert_eq!(d.encoding.as_deref(), Some("opus/48000/2"));
        assert!(d.fmtp.as_deref().unwrap().contains("stereo=1"));

        // Random bytes in a dynamic payload type are not Opus.
        let mut s = McastScanner::new();
        let mut x = 0x1234_5678u32;
        for k in 0..200u32 {
            let p: Vec<u8> = (0..300)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    x as u8
                })
                .collect();
            let rtp = rtp_packet(96, k as u16, k * 960, 9, &p);
            feed(&mut s, &udp_v4(SRC, [225, 0, 0, 9], 6000, 6000, &rtp));
        }
        assert!(s.streams().is_empty());
    }

    #[test]
    fn opus_framing() {
        assert!(opus_packet_ok(&[0x78, 1, 2, 3])); // code 0
        assert!(opus_packet_ok(&[0x79, 1, 2])); // code 1: two equal frames
        assert!(!opus_packet_ok(&[0x79, 1, 2, 3]));
        assert!(opus_packet_ok(&[0x7A, 2, 9, 9, 8])); // code 2: 2 then 1
        assert!(!opus_packet_ok(&[0x7A, 9, 9, 9]));
        // Code 3, VBR, three frames of 1, 2 and 3 bytes.
        assert!(opus_packet_ok(&[0x7B, 0x83, 1, 2, 7, 8, 8, 9, 9, 9]));
        // Seven 20 ms frames: 140 ms, more than a packet may hold.
        assert!(!opus_packet_ok(&[0xFB, 0x07, 0, 0, 0, 0, 0, 0, 0]));
    }

    #[test]
    fn audio_specific_configs() {
        assert_eq!(aac_config(44100, 2, false, false).unwrap(), [0x12, 0x10]);
        assert_eq!(aac_config(48000, 2, false, false).unwrap(), [0x11, 0x90]);
        assert_eq!(aac_config(48000, 1, false, true).unwrap(), [0x11, 0x8C]);
        assert_eq!(
            aac_config(44100, 2, true, false).unwrap(),
            [0x2B, 0x92, 0x08, 0x00]
        );
        assert_eq!(aac_config(44100, 2, true, false).map(|c| c.len()), Some(4));
        assert!(aac_config(44000, 2, false, false).is_none());
    }

    #[test]
    fn reads_now_playing_messages() {
        let mut s = McastScanner::new();
        let x = "<nowplaying><title>Livin&apos; In The City</title><artist>John Butler \
                 Trio</artist><station>RblCNQ</station><media_type>SONG</media_type>\
                 <metadata></metadata></nowplaying>";
        feed(
            &mut s,
            &udp_v4(SRC, [225, 0, 0, 98], 1915, 1915, x.as_bytes()),
        );
        let y = "<nowplaying><station>BrzMNC</station><media_type>UNSPECIFIED</media_type>\
                 </nowplaying>";
        feed(
            &mut s,
            &udp_v4(SRC, [225, 0, 0, 98], 1915, 1915, y.as_bytes()),
        );
        let n = s.now_playing();
        assert_eq!(n.len(), 2);
        assert_eq!(n[0].station, "BrzMNC");
        assert_eq!(n[0].title, None);
        assert_eq!(n[1].title.as_deref(), Some("Livin' In The City"));
        assert_eq!(n[1].artist.as_deref(), Some("John Butler Trio"));
        assert_eq!(n[1].kind.as_deref(), Some("SONG"));
        assert!(s.streams().is_empty());
    }
}
