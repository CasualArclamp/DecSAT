//! EUROCAE ED-137 radio over IP (the VoIP of air traffic control radio):
//! the radio signalling each ED-137 RTP packet carries in its header
//! extension — push-to-talk, squelch, the PTT's id, signal quality — and
//! the SIP that sets a radio session up, whose SDP says what the session
//! is (radio type, Tx/Rx mode, keep-alive period, best-signal method).
//!
//! The specification (ED-137B Part 1) was not to hand. The header
//! extension's layout and the meaning of its values follow Wireshark's
//! ED-137 dissector (epan/dissectors/packet-rtp-ed137.c, GPL-2.0-or-later),
//! as tshark 4.6 lists its fields, bit masks and value names
//! (`tshark -G fields`, `-G values`); this decoder agrees with tshark on
//! Rory's recording of a live link (2026-10-13: a VCS's R2S keep-alives and
//! SIP to a radio, inside a Comtech EDMAC carrier, see
//! `decsat_modem::ethblock`).
//!
//! **Header extension** (RFC 3550 §5.3.1): "defined by profile" 0167h for
//! ED-137A onwards (B and C keep its layout), 0067h for the first edition.
//!
//! ```text
//! ED-137A:  PTT type (3) | SQU | PTT-id (6) | PM | PTTS | SCT | reserved (2) | X
//!           then, with X: features [type (4) | length (4) | value (length bytes)]…
//! ED-137:   PTT type (3) | SQU | PTT-id (4) | SCT | X | type (4) | length (4) | value (13) | VF
//! ```
//!
//! PTT types: 0 off, 1 normal, 2 coupling, 3 priority, 4 emergency, 5 test
//! (ED-137A). Features: 1 signal quality (an index and how it was measured;
//! "best signal selection" in the first edition), 2 CLIMAX time delay, 3
//! radio remote control (ED-137B), 4 CLIMAX dynamic delay compensation;
//! 11–15 are the vendors'. Payload type 123 is the R2S keep-alive (the
//! SDP's `a=rtpmap:123 R2S/8000`): signalling and no sound; voice goes as
//! PCMA or PCMU (8, 0). The voice of packets whose PTT or squelch is on is
//! decoded (G.711) and handed on in [`VoiceChunk`]s, for the receiver to
//! play; voice with neither on is counted, not played.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;

/// The header extension profiles: ED-137A onwards, and the first edition.
pub const PROFILE_ED137A: u16 = 0x0167;
pub const PROFILE_ED137: u16 = 0x0067;
/// The R2S keep-alive payload type.
pub const PT_R2S: u8 = 123;
/// G.711's payload types (RFC 3551 Table 4): µ-law, A-law.
pub const PT_PCMU: u8 = 0;
pub const PT_PCMA: u8 = 8;

/// G.711 A-law to linear (ITU-T G.711 Table 1a: even bits inverted, a
/// sign, a 3-bit segment, a 4-bit step), 16-bit scale.
pub fn alaw_to_linear(a: u8) -> i16 {
    let a = a ^ 0x55;
    let seg = (a >> 4) & 0x07;
    let mut v = i16::from(a & 0x0F) << 4;
    v += if seg == 0 { 8 } else { 0x108 };
    if seg > 1 {
        v <<= seg - 1;
    }
    if a & 0x80 != 0 { v } else { -v }
}

/// G.711 µ-law to linear (ITU-T G.711 Table 2a: bits inverted, a sign, a
/// 3-bit segment, a 4-bit step, bias 132), 16-bit scale.
pub fn ulaw_to_linear(u: u8) -> i16 {
    let u = !u;
    let t = ((i16::from(u & 0x0F) << 3) + 0x84) << ((u >> 4) & 0x07);
    if u & 0x80 != 0 { 0x84 - t } else { t - 0x84 }
}

/// Sound out of a stream's packet whose PTT or squelch was on: for the
/// receiver to play.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceChunk {
    /// The stream's label (as its [`StreamView::label`]).
    pub stream: String,
    /// G.711 decoded, 8000 samples a second, full scale ±1.
    pub pcm: Vec<f32>,
}

/// Voice chunks held for the taker, at most (about 10 s of 20 ms packets).
const VOICE_HELD: usize = 500;
/// SIP's port.
pub const SIP_PORT: u16 = 5060;

/// The push-to-talk state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ptt {
    #[default]
    Off,
    Normal,
    Coupling,
    Priority,
    Emergency,
    /// ED-137A's 5 (reserved in the first edition).
    Test,
    Reserved(u8),
}

impl Ptt {
    fn from_bits(v: u8, edition_a: bool) -> Self {
        match v {
            0 => Ptt::Off,
            1 => Ptt::Normal,
            2 => Ptt::Coupling,
            3 => Ptt::Priority,
            4 => Ptt::Emergency,
            5 if edition_a => Ptt::Test,
            v => Ptt::Reserved(v),
        }
    }

    pub fn on(self) -> bool {
        self != Ptt::Off
    }

    pub fn name(self) -> String {
        match self {
            Ptt::Off => "PTT off".into(),
            Ptt::Normal => "normal PTT".into(),
            Ptt::Coupling => "coupling PTT".into(),
            Ptt::Priority => "priority PTT".into(),
            Ptt::Emergency => "emergency PTT".into(),
            Ptt::Test => "test PTT".into(),
            Ptt::Reserved(v) => format!("PTT type {v} (reserved)"),
        }
    }
}

/// An additional feature of the header extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Feature {
    /// Signal quality: an index and how it was measured (0 RSSI, 1 AGC
    /// level, 2 C/N, 3 standardised PSD, 4–7 the vendor's).
    Sqi {
        index: u8,
        method: u8,
    },
    /// CLIMAX time delay: relative or absolute, and its value.
    ClimaxDelay {
        absolute: bool,
        value: u8,
    },
    /// Radio remote control (ED-137B): eight bits — main/standby Tx and
    /// Rx on frequencies 1 and 2, Tx selected on 1 and 2, Rx muted on 1
    /// and 2.
    RadioRemoteControl(u8),
    /// CLIMAX dynamic delay compensation (its measurement messages, not
    /// read).
    ClimaxDdc(Vec<u8>),
    Other {
        kind: u8,
        value: Vec<u8>,
    },
}

impl Feature {
    pub fn describe(&self) -> String {
        match self {
            Feature::Sqi { index, method } => {
                let how = match method {
                    0 => "RSSI",
                    1 => "AGC level",
                    2 => "C/N",
                    3 => "standardised PSD",
                    _ => "vendor's method",
                };
                let level = match (method, index) {
                    (0, 0) => " (below -100 dBm)".to_string(),
                    (0, 1..=14) => {
                        format!(" (≤ {:.1} dBm)", -100.0 + f64::from(*index) * 30.0 / 14.0)
                    }
                    (0, 15) => " (above -70 dBm)".into(),
                    _ => String::new(),
                };
                format!("signal quality {index}{level}, {how} {method}")
            }
            Feature::ClimaxDelay { absolute, value } => format!(
                "CLIMAX delay {value} ({})",
                if *absolute { "absolute" } else { "relative" }
            ),
            Feature::RadioRemoteControl(b) => {
                const NAMES: [&str; 8] = [
                    "MS Tx F1",
                    "MS Rx F1",
                    "MS Tx F2",
                    "MS Rx F2",
                    "Tx sel F1",
                    "Tx sel F2",
                    "Rx mute F1",
                    "Rx mute F2",
                ];
                let on: Vec<&str> = (0..8)
                    .filter(|k| b >> (7 - k) & 1 == 1)
                    .map(|k| NAMES[k])
                    .collect();
                format!("radio remote control: {}", on.join(", "))
            }
            Feature::ClimaxDdc(v) => format!("CLIMAX delay compensation ({} bytes)", v.len()),
            Feature::Other { kind, value } => format!(
                "feature {kind}{}: {}",
                if *kind >= 11 { " (vendor's)" } else { "" },
                value
                    .iter()
                    .map(|b| format!("{b:02X}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        }
    }

    /// Its kind, for counting.
    pub fn kind(&self) -> u8 {
        match self {
            Feature::Sqi { .. } => 1,
            Feature::ClimaxDelay { .. } => 2,
            Feature::RadioRemoteControl(_) => 3,
            Feature::ClimaxDdc(_) => 4,
            Feature::Other { kind, .. } => *kind,
        }
    }
}

/// The radio signalling of one RTP packet.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Signalling {
    /// ED-137A's layout (profile 0167h), else the first edition's.
    pub edition_a: bool,
    pub ptt: Ptt,
    pub squelch: bool,
    pub ptt_id: u8,
    /// PTT mute, PTT summation (ED-137A).
    pub ptt_mute: bool,
    pub ptt_summation: bool,
    /// Simultaneous call transmissions.
    pub sct: bool,
    /// The first edition's VF bit.
    pub vf: Option<bool>,
    pub features: Vec<Feature>,
}

/// An RTP packet (RFC 3550 §5.1), its header extension split off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rtp<'a> {
    pub pt: u8,
    pub marker: bool,
    pub seq: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    /// The header extension: its profile word and its data.
    pub ext: Option<(u16, &'a [u8])>,
    pub payload: &'a [u8],
}

/// Read an RTP packet; `None` if it is not one (version 2, lengths that
/// fit).
pub fn rtp(p: &[u8]) -> Option<Rtp<'_>> {
    if p.len() < 12 || p[0] >> 6 != 2 {
        return None;
    }
    let cc = usize::from(p[0] & 0x0F);
    let mut at = 12 + 4 * cc;
    let ext = if p[0] & 0x10 != 0 {
        let h = p.get(at..at + 4)?;
        let profile = u16::from_be_bytes([h[0], h[1]]);
        let words = usize::from(u16::from_be_bytes([h[2], h[3]]));
        let data = p.get(at + 4..at + 4 + 4 * words)?;
        at += 4 + 4 * words;
        Some((profile, data))
    } else {
        None
    };
    let mut end = p.len();
    if p[0] & 0x20 != 0 {
        end = end.checked_sub(usize::from(*p.last()?))?;
    }
    Some(Rtp {
        pt: p[1] & 0x7F,
        marker: p[1] & 0x80 != 0,
        seq: u16::from_be_bytes([p[2], p[3]]),
        timestamp: u32::from_be_bytes([p[4], p[5], p[6], p[7]]),
        ssrc: u32::from_be_bytes([p[8], p[9], p[10], p[11]]),
        ext,
        payload: p.get(at..end)?,
    })
}

/// The radio signalling in an RTP header extension of either ED-137
/// profile; `None` for any other extension.
pub fn signalling(profile: u16, ext: &[u8]) -> Option<Signalling> {
    match profile {
        PROFILE_ED137A => {
            let h = u16::from_be_bytes([*ext.first()?, *ext.get(1)?]);
            let mut s = Signalling {
                edition_a: true,
                ptt: Ptt::from_bits((h >> 13) as u8, true),
                squelch: h >> 12 & 1 == 1,
                ptt_id: (h >> 6 & 0x3F) as u8,
                ptt_mute: h >> 5 & 1 == 1,
                ptt_summation: h >> 4 & 1 == 1,
                sct: h >> 3 & 1 == 1,
                vf: None,
                features: Vec::new(),
            };
            if h & 1 == 1 {
                let mut i = 2;
                // Features until the data end or a type 0 (padding).
                while let Some(&b) = ext.get(i) {
                    let (kind, len) = (b >> 4, usize::from(b & 0x0F));
                    if kind == 0 {
                        break;
                    }
                    let Some(v) = ext.get(i + 1..i + 1 + len) else {
                        break;
                    };
                    s.features.push(feature_a(kind, v));
                    i += 1 + len;
                }
            }
            Some(s)
        }
        PROFILE_ED137 => {
            let w = u32::from_be_bytes(ext.get(..4)?.try_into().ok()?);
            let mut s = Signalling {
                edition_a: false,
                ptt: Ptt::from_bits((w >> 29) as u8, false),
                squelch: w >> 28 & 1 == 1,
                ptt_id: (w >> 24 & 0x0F) as u8,
                sct: w >> 23 & 1 == 1,
                vf: Some(w & 1 == 1),
                ..Default::default()
            };
            if w >> 22 & 1 == 1 {
                let kind = (w >> 18 & 0x0F) as u8;
                let value = w >> 1 & 0x1FFF;
                s.features.push(match kind {
                    1 => Feature::Sqi {
                        index: (w >> 6 & 0xFF) as u8,
                        method: (w >> 3 & 0x07) as u8,
                    },
                    2 => Feature::ClimaxDelay {
                        absolute: false,
                        value: (w >> 8 & 0x3F) as u8,
                    },
                    kind => Feature::Other {
                        kind,
                        value: (value as u16).to_be_bytes().to_vec(),
                    },
                });
            }
            Some(s)
        }
        _ => None,
    }
}

/// An ED-137A feature from its type and value bytes.
fn feature_a(kind: u8, v: &[u8]) -> Feature {
    match (kind, v) {
        (1, [b]) => Feature::Sqi {
            index: b >> 3,
            method: b & 0x07,
        },
        (2, [b]) => Feature::ClimaxDelay {
            absolute: b & 0x80 != 0,
            value: b & 0x7F,
        },
        (3, [b]) => Feature::RadioRemoteControl(*b),
        (4, v) => Feature::ClimaxDdc(v.to_vec()),
        (kind, v) => Feature::Other {
            kind,
            value: v.to_vec(),
        },
    }
}

/// A SIP message (RFC 3261), as far as a radio session needs reading.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sip {
    /// The request's method, or the response's status ("200 OK").
    pub start: String,
    pub request: bool,
    pub from: String,
    pub to: String,
    pub call_id: String,
    /// The top Via header's branch (the transaction).
    pub branch: Option<String>,
    /// Headers ED-137 adds or uses: WG67-Version, Subject, Priority,
    /// User-Agent (name, value).
    pub extra: Vec<(String, String)>,
    /// The SDP's media line, connection address and attributes.
    pub media: Option<String>,
    pub connection: Option<String>,
    pub attributes: Vec<(String, String)>,
}

/// SIP headers kept beside the dialog's: what ED-137 adds, and who sent it.
const SIP_EXTRA: [&str; 5] = [
    "wg67-version",
    "subject",
    "priority",
    "user-agent",
    "server",
];

/// Read a SIP message; `None` if the datagram is not one.
pub fn sip(p: &[u8]) -> Option<Sip> {
    let text = std::str::from_utf8(p).ok()?;
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text, ""));
    let mut lines = head.split("\r\n");
    let first = lines.next()?;
    let mut m = Sip::default();
    if let Some(status) = first.strip_prefix("SIP/2.0 ") {
        m.start = status.to_string();
    } else {
        let (method, rest) = first.split_once(' ')?;
        if !rest.ends_with("SIP/2.0") || !method.bytes().all(|b| b.is_ascii_uppercase()) {
            return None;
        }
        m.start = method.to_string();
        m.request = true;
    }
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim().to_ascii_lowercase(), value.trim());
        match name.as_str() {
            "from" | "f" => m.from = party(value),
            "to" | "t" => m.to = party(value),
            "call-id" | "i" => m.call_id = value.to_string(),
            "via" | "v" if m.branch.is_none() => {
                m.branch = value
                    .split(';')
                    .find_map(|p| p.trim().strip_prefix("branch="))
                    .map(str::to_string);
            }
            n if SIP_EXTRA.contains(&n) => m.extra.push((title(n), value.to_string())),
            _ => {}
        }
    }
    for line in body.lines() {
        match line.split_once('=') {
            Some(("m", v)) => m.media = Some(v.to_string()),
            Some(("c", v)) => m.connection = Some(v.to_string()),
            Some(("a", v)) => {
                let (k, val) = v.split_once(':').unwrap_or((v, ""));
                m.attributes.push((k.to_string(), val.to_string()));
            }
            _ => {}
        }
    }
    Some(m)
}

/// A From/To header's party: its display name, or its URI's user part,
/// percent-escapes undone (`Radio%20(IP)`), and the host.
fn party(v: &str) -> String {
    let uri = v
        .split_once('<')
        .and_then(|(_, r)| r.split_once('>'))
        .map_or(v, |(u, _)| u);
    let name = v.split_once('<').map(|(n, _)| n.trim().trim_matches('"'));
    let uri = uri.trim_start_matches("sip:").trim_start_matches("sips:");
    let (user, host) = uri.split_once('@').unwrap_or(("", uri));
    let user = match name {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => unescape(user),
    };
    if user.is_empty() {
        host.to_string()
    } else {
        format!("{user} @ {host}")
    }
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A header's name as written: each word capitalised (`User-Agent`).
fn title(n: &str) -> String {
    if n == "wg67-version" {
        return "WG67-Version".into();
    }
    n.split('-')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// An RTP stream with ED-137 signalling: one way, one SSRC.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamKey {
    pub src: IpAddr,
    pub sport: u16,
    pub dst: IpAddr,
    pub dport: u16,
    pub ssrc: u32,
}

/// What a stream has said so far.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamView {
    pub key: StreamKey,
    pub packets: u64,
    /// R2S keep-alives, and packets carrying sound (any other payload
    /// type), with the payload types seen.
    pub keepalives: u64,
    pub voice: u64,
    pub payload_types: Vec<u8>,
    /// Packets missing by the sequence numbers.
    pub lost: u64,
    /// The latest signalling, and when it came (s).
    pub now: Signalling,
    pub last_time: f64,
    pub first_time: f64,
    /// Times PTT and squelch came on, and how long they were on (s).
    pub ptt_on: u64,
    pub ptt_secs: f64,
    pub squelch_on: u64,
    pub squelch_secs: f64,
    /// Feature kinds seen.
    pub feature_kinds: Vec<u8>,
    /// Packets of sound while PTT or squelch was on (handed on to play),
    /// and their seconds.
    pub talk_packets: u64,
    pub talk_secs: f64,
}

impl StreamView {
    fn new(key: StreamKey, t: f64) -> Self {
        StreamView {
            key,
            packets: 0,
            keepalives: 0,
            voice: 0,
            payload_types: Vec::new(),
            lost: 0,
            now: Signalling::default(),
            last_time: t,
            first_time: t,
            ptt_on: 0,
            ptt_secs: 0.0,
            squelch_on: 0,
            squelch_secs: 0.0,
            feature_kinds: Vec::new(),
            talk_packets: 0,
            talk_secs: 0.0,
        }
    }

    /// "172.16.82.28:8602 → 10.26.185.112:8580 (SSRC A5944A87)".
    pub fn label(&self) -> String {
        let k = &self.key;
        format!(
            "{}:{} → {}:{} (SSRC {:08X})",
            k.src, k.sport, k.dst, k.dport, k.ssrc
        )
    }
}

/// How an INVITE was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Refused,
    Accepted,
}

impl Outcome {
    pub fn name(self) -> &'static str {
        match self {
            Outcome::Refused => "refused",
            Outcome::Accepted => "accepted",
        }
    }
}

/// A change of state, for the log.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub time: f64,
    /// The stream's label.
    pub stream: String,
    pub what: String,
}

/// A SIP dialog (one Call-ID).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionView {
    pub call_id: String,
    pub from: String,
    pub to: String,
    /// Messages seen, in order ("INVITE", "200 OK", "ACK", …), the last
    /// few.
    pub messages: Vec<String>,
    pub first_time: f64,
    pub last_time: f64,
    /// How the INVITE went, as far as one direction tells: its ACK reuses
    /// the INVITE's branch when the answer was an error (RFC 3261
    /// §17.1.1.3), and starts a transaction of its own after a 200 OK.
    pub outcome: Option<Outcome>,
    pub invite_branch: Option<String>,
    pub extra: Vec<(String, String)>,
    pub media: Option<String>,
    pub connection: Option<String>,
    pub attributes: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RadioView {
    /// Streams of `MIN_PACKETS` packets or more; packets in the rest (an
    /// SSRC or address hit by errors makes a stream of its own).
    pub streams: Vec<StreamView>,
    pub stray: u64,
    /// The newest dialogs first (at most `SESSIONS`), and how many there
    /// have been.
    pub sessions: Vec<SessionView>,
    pub session_count: u64,
    /// The newest last (at most `EVENTS`).
    pub events: Vec<Event>,
}

/// Packets a stream needs to be listed.
const MIN_PACKETS: u64 = 3;
/// Dialogs and events kept.
const SESSIONS: usize = 12;
const EVENTS: usize = 200;
/// Messages kept a dialog.
const MESSAGES: usize = 12;

/// Follows the ED-137 radio over IP in UDP: the streams' signalling, the
/// SIP dialogs, and a log of what changed.
#[derive(Default)]
pub struct RadioRx {
    streams: Vec<StreamView>,
    index: HashMap<StreamKey, usize>,
    last_seq: Vec<Option<u16>>,
    sessions: VecDeque<SessionView>,
    session_count: u64,
    events: VecDeque<Event>,
    /// Voice for the taker, while it wants it.
    want_voice: bool,
    voice: Vec<VoiceChunk>,
}

impl RadioRx {
    /// Hold the voice of talking packets for [`Self::take_voice`] (else it
    /// is only counted).
    pub fn want_voice(&mut self, on: bool) {
        self.want_voice = on;
        if !on {
            self.voice.clear();
        }
    }

    /// The voice held since last time, oldest first.
    pub fn take_voice(&mut self) -> Vec<VoiceChunk> {
        std::mem::take(&mut self.voice)
    }

    /// Whether any stream has talked yet (sound with PTT or squelch on).
    pub fn talked(&self) -> bool {
        self.streams.iter().any(|s| s.talk_packets > 0)
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Whether anything ED-137 has been seen.
    pub fn active(&self) -> bool {
        !self.streams.is_empty() || !self.sessions.is_empty()
    }

    /// One UDP datagram (whole: fragments put back together), at signal
    /// time `t` (s). Returns whether it was ED-137 RTP or SIP.
    pub fn udp(
        &mut self,
        (src, sport): (IpAddr, u16),
        (dst, dport): (IpAddr, u16),
        payload: &[u8],
        t: f64,
    ) -> bool {
        if (sport == SIP_PORT || dport == SIP_PORT)
            && let Some(m) = sip(payload)
        {
            self.sip(m, t);
            return true;
        }
        let Some(r) = rtp(payload) else {
            return false;
        };
        let Some(s) = r.ext.and_then(|(p, d)| signalling(p, d)) else {
            return false;
        };
        let key = StreamKey {
            src,
            sport,
            dst,
            dport,
            ssrc: r.ssrc,
        };
        let i = *self.index.entry(key.clone()).or_insert_with(|| {
            self.streams.push(StreamView::new(key, t));
            self.last_seq.push(None);
            self.streams.len() - 1
        });
        let mut events = Vec::new();
        let st = &mut self.streams[i];
        st.packets += 1;
        if let Some(prev) = self.last_seq[i] {
            let gap = r.seq.wrapping_sub(prev);
            // A jump this size is a restart, not loss.
            if (2..1000).contains(&gap) {
                st.lost += u64::from(gap - 1);
            }
        }
        self.last_seq[i] = Some(r.seq);
        if r.pt == PT_R2S {
            st.keepalives += 1;
        } else {
            st.voice += 1;
        }
        if !st.payload_types.contains(&r.pt) {
            st.payload_types.push(r.pt);
        }
        for f in &s.features {
            if !st.feature_kinds.contains(&f.kind()) {
                st.feature_kinds.push(f.kind());
            }
        }
        let dt = (t - st.last_time).max(0.0);
        if st.now.ptt.on() {
            st.ptt_secs += dt;
        }
        if st.now.squelch {
            st.squelch_secs += dt;
        }
        let first = st.packets == 1;
        if s.ptt != st.now.ptt || (first && s.ptt.on()) {
            if s.ptt.on() {
                st.ptt_on += 1;
                events.push(format!("{} (PTT-id {})", s.ptt.name(), s.ptt_id));
            } else {
                events.push("PTT off".to_string());
            }
        }
        if s.squelch != st.now.squelch || (first && s.squelch) {
            if s.squelch {
                st.squelch_on += 1;
            }
            events.push(
                if s.squelch {
                    "squelch open"
                } else {
                    "squelch closed"
                }
                .into(),
            );
        }
        // Sound, when PTT or squelch is on: G.711 decoded for the taker.
        let talking = s.ptt.on() || s.squelch;
        let law = match r.pt {
            PT_PCMA => Some(alaw_to_linear as fn(u8) -> i16),
            PT_PCMU => Some(ulaw_to_linear as fn(u8) -> i16),
            _ => None,
        };
        let mut chunk = None;
        if talking && let Some(law) = law {
            st.talk_packets += 1;
            st.talk_secs += r.payload.len() as f64 / 8000.0;
            if self.want_voice {
                chunk = Some(VoiceChunk {
                    stream: st.label(),
                    pcm: r
                        .payload
                        .iter()
                        .map(|&b| f32::from(law(b)) / 32768.0)
                        .collect(),
                });
            }
        }
        st.now = s;
        st.last_time = t;
        let label = st.label();
        if let Some(c) = chunk {
            if self.voice.len() == VOICE_HELD {
                self.voice.remove(0);
            }
            self.voice.push(c);
        }
        for what in events {
            self.event(t, label.clone(), what);
        }
        true
    }

    fn event(&mut self, time: f64, stream: String, what: String) {
        if self.events.len() == EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(Event { time, stream, what });
    }

    fn sip(&mut self, m: Sip, t: f64) {
        if m.call_id.is_empty() {
            return;
        }
        let s = match self
            .sessions
            .iter_mut()
            .position(|s| s.call_id == m.call_id)
        {
            Some(k) => &mut self.sessions[k],
            None => {
                if self.sessions.len() == SESSIONS {
                    self.sessions.pop_back();
                }
                self.session_count += 1;
                self.sessions.push_front(SessionView {
                    call_id: m.call_id.clone(),
                    first_time: t,
                    ..Default::default()
                });
                &mut self.sessions[0]
            }
        };
        s.last_time = t;
        if s.messages.len() == MESSAGES {
            s.messages.remove(0);
        }
        s.messages.push(m.start.clone());
        match m.start.as_str() {
            "INVITE" => s.invite_branch = m.branch.clone(),
            "ACK" if s.invite_branch.is_some() && m.branch.is_some() => {
                s.outcome = Some(if m.branch == s.invite_branch {
                    Outcome::Refused
                } else {
                    Outcome::Accepted
                });
            }
            _ => {}
        }
        // The caller's view of who is who: the request's From and To.
        if m.request || s.from.is_empty() {
            if !m.from.is_empty() {
                s.from = m.from;
            }
            if !m.to.is_empty() {
                s.to = m.to;
            }
        }
        for (k, v) in m.extra {
            match s.extra.iter_mut().find(|e| e.0 == k) {
                Some(e) => e.1 = v,
                None => s.extra.push((k, v)),
            }
        }
        if m.media.is_some() {
            s.media = m.media;
            s.connection = m.connection;
            s.attributes = m.attributes;
        }
    }

    pub fn view(&self) -> RadioView {
        let (streams, stray): (Vec<_>, Vec<_>) = self
            .streams
            .iter()
            .cloned()
            .partition(|s| s.packets >= MIN_PACKETS);
        RadioView {
            streams,
            stray: stray.iter().map(|s| s.packets).sum(),
            sessions: self.sessions.iter().cloned().collect(),
            session_count: self.session_count,
            events: self.events.iter().cloned().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An RTP packet with an ED-137A extension (words after the profile).
    fn packet(pt: u8, seq: u16, ext: &[u8], payload: &[u8]) -> Vec<u8> {
        assert!(ext.len().is_multiple_of(4));
        let mut p = vec![0x90, pt];
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&[0x9E, 0x52, 0x33, 0xDC, 0xA5, 0x94, 0x4A, 0x87]);
        p.extend_from_slice(&PROFILE_ED137A.to_be_bytes());
        p.extend_from_slice(&((ext.len() / 4) as u16).to_be_bytes());
        p.extend_from_slice(ext);
        p.extend_from_slice(payload);
        p
    }

    /// The keep-alive Rory's recording carries, byte for byte: tshark
    /// reads PTT off, SQ off, PTT-id 0, X 1, feature type 1 length 1.
    #[test]
    fn reads_a_live_keepalive() {
        let p = [
            0x90, 0x7B, 0x6A, 0x8D, 0x9E, 0x52, 0x33, 0xDC, 0xA5, 0x94, 0x4A, 0x87, 0x01, 0x67,
            0x00, 0x01, 0x00, 0x01, 0x11, 0x04, 0x00, 0x00, 0x00, 0x00,
        ];
        let r = rtp(&p).unwrap();
        assert_eq!((r.pt, r.seq, r.ssrc), (PT_R2S, 0x6A8D, 0xA594_4A87));
        assert_eq!(r.payload, [0, 0, 0, 0]);
        let (profile, ext) = r.ext.unwrap();
        let s = signalling(profile, ext).unwrap();
        assert_eq!(
            s,
            Signalling {
                edition_a: true,
                features: vec![Feature::Sqi {
                    index: 0,
                    method: 4
                }],
                ..Default::default()
            }
        );
    }

    #[test]
    fn reads_every_field() {
        // Priority PTT, squelch, PTT-id 37, PM, PTTS, SCT; then signal
        // quality 9 by RSSI, a CLIMAX delay, radio remote control.
        let h: u16 = 3 << 13 | 1 << 12 | 37 << 6 | 1 << 5 | 1 << 4 | 1 << 3 | 1;
        let mut ext = h.to_be_bytes().to_vec();
        ext.extend_from_slice(&[0x11, 9 << 3, 0x21, 0x85, 0x31, 0b1010_0001, 0, 0]);
        let s = signalling(PROFILE_ED137A, &ext).unwrap();
        assert_eq!(
            (
                s.ptt,
                s.squelch,
                s.ptt_id,
                s.ptt_mute,
                s.ptt_summation,
                s.sct
            ),
            (Ptt::Priority, true, 37, true, true, true)
        );
        assert_eq!(
            s.features,
            vec![
                Feature::Sqi {
                    index: 9,
                    method: 0
                },
                Feature::ClimaxDelay {
                    absolute: true,
                    value: 5
                },
                Feature::RadioRemoteControl(0b1010_0001),
            ]
        );
        assert_eq!(
            s.features[0].describe(),
            "signal quality 9 (≤ -80.7 dBm), RSSI 0"
        );
        assert_eq!(
            s.features[2].describe(),
            "radio remote control: MS Tx F1, MS Tx F2, Rx mute F2"
        );
        // The first edition: normal PTT, id 5, BSS index 200, VF.
        let w: u32 = 1 << 29 | 5 << 24 | 1 << 22 | 1 << 18 | 1 << 14 | 200 << 6 | 1;
        let s = signalling(PROFILE_ED137, &w.to_be_bytes()).unwrap();
        assert_eq!((s.ptt, s.ptt_id, s.vf), (Ptt::Normal, 5, Some(true)));
        assert_eq!(
            s.features,
            vec![Feature::Sqi {
                index: 200,
                method: 0
            }]
        );
        assert!(signalling(0x1234, &ext).is_none());
    }

    #[test]
    fn follows_ptt_and_squelch() {
        let a: IpAddr = [172, 16, 82, 28].into();
        let b: IpAddr = [10, 26, 185, 112].into();
        let mut rx = RadioRx::new();
        let off = [0x00, 0x00, 0x00, 0x00];
        let ptt = (1u16 << 13 | 4 << 6).to_be_bytes();
        let ptt = [ptt[0], ptt[1], 0, 0];
        let sq = (1u16 << 12).to_be_bytes();
        let sq = [sq[0], sq[1], 0, 0];
        let mut t = 0.0;
        let mut seq = 100u16;
        let mut send = |rx: &mut RadioRx, pt: u8, ext: &[u8], skip: u16| {
            seq = seq.wrapping_add(1 + skip);
            t += 0.02;
            assert!(rx.udp((a, 8602), (b, 8580), &packet(pt, seq, ext, &[0xD5; 160]), t));
        };
        for _ in 0..5 {
            send(&mut rx, PT_R2S, &off, 0);
        }
        for _ in 0..50 {
            send(&mut rx, 8, &ptt, 0);
        }
        send(&mut rx, PT_R2S, &off, 2);
        for _ in 0..10 {
            send(&mut rx, 8, &sq, 0);
        }
        send(&mut rx, PT_R2S, &off, 0);
        let v = rx.view();
        assert_eq!(v.streams.len(), 1);
        let s = &v.streams[0];
        assert_eq!((s.packets, s.keepalives, s.voice, s.lost), (67, 7, 60, 2));
        assert_eq!((s.ptt_on, s.squelch_on), (1, 1));
        assert!((s.ptt_secs - 1.0).abs() < 1e-6, "{}", s.ptt_secs);
        let log: Vec<&str> = v.events.iter().map(|e| e.what.as_str()).collect();
        assert_eq!(
            log,
            [
                "normal PTT (PTT-id 4)",
                "PTT off",
                "squelch open",
                "squelch closed"
            ]
        );
        // Not ED-137: plain RTP, SNMP.
        let mut plain = packet(8, 1, &[0, 0, 0, 0], &[]);
        plain[12] = 0xBE;
        assert!(!rx.udp((a, 1), (b, 2), &plain, 1.0));
        assert!(!rx.udp((a, 161), (b, 162), &[0x30, 0x35, 2, 1, 1], 1.0));
    }

    /// G.711 against values from the standard's tables.
    #[test]
    fn g711_decodes() {
        assert_eq!((alaw_to_linear(0xD5), alaw_to_linear(0x55)), (8, -8));
        assert_eq!(
            (alaw_to_linear(0xAA), alaw_to_linear(0x2A)),
            (32256, -32256)
        );
        assert_eq!((ulaw_to_linear(0xFF), ulaw_to_linear(0x7F)), (0, 0));
        assert_eq!(
            (ulaw_to_linear(0x80), ulaw_to_linear(0x00)),
            (32124, -32124)
        );
        // The A-law decoder the E1 stage has always used agrees.
        for b in 0..=255u8 {
            let e1 = {
                let a = b ^ 0x55;
                let seg = (a >> 4) & 7;
                let mant = i32::from(a & 0x0F);
                let mag = if seg == 0 {
                    (mant << 4) + 8
                } else {
                    ((mant << 4) + 0x108) << (seg - 1)
                };
                if a & 0x80 != 0 { mag } else { -mag }
            };
            assert_eq!(i32::from(alaw_to_linear(b)), e1, "{b:02X}");
        }
    }

    /// Voice is handed on only while PTT or squelch is on, G.711 decoded.
    #[test]
    fn hands_on_the_voice_of_talking_packets() {
        let a: IpAddr = [172, 16, 82, 28].into();
        let b: IpAddr = [10, 26, 185, 112].into();
        let mut rx = RadioRx::new();
        rx.want_voice(true);
        let sq = (1u16 << 12).to_be_bytes();
        let sq = [sq[0], sq[1], 0, 0];
        let off = [0u8; 4];
        let mut seq = 0u16;
        let mut send = |rx: &mut RadioRx, pt: u8, ext: &[u8], payload: &[u8]| {
            seq += 1;
            rx.udp(
                (a, 8602),
                (b, 8580),
                &packet(pt, seq, ext, payload),
                f64::from(seq) * 0.02,
            );
        };
        send(&mut rx, PT_PCMA, &off, &[0xD5; 160]); // no PTT, no squelch
        send(&mut rx, PT_PCMA, &sq, &[0xAA; 160]);
        send(&mut rx, PT_PCMU, &sq, &[0x80; 160]);
        send(&mut rx, PT_R2S, &sq, &[0; 4]); // a keep-alive: no sound
        let v = rx.take_voice();
        assert_eq!(v.len(), 2);
        assert!(
            v[0].pcm
                .iter()
                .all(|&x| (x - 32256.0 / 32768.0).abs() < 1e-6)
        );
        assert!(
            v[1].pcm
                .iter()
                .all(|&x| (x - 32124.0 / 32768.0).abs() < 1e-6)
        );
        let s = &rx.view().streams[0];
        assert_eq!((s.voice, s.talk_packets), (3, 2));
        assert!((s.talk_secs - 0.04).abs() < 1e-9);
        assert!(rx.take_voice().is_empty());
    }

    #[test]
    fn reads_the_sip_that_sets_a_session_up() {
        let invite = "INVITE sip:109061002@10.26.185.115 SIP/2.0\r\n\
            Via: SIP/2.0/UDP 172.16.82.18:5060;rport;branch=z9hG4bK135744739\r\n\
            From: <sip:Radio%20(IP)@172.16.82.15>;tag=1\r\n\
            To: <sip:109061002@10.26.185.115>\r\n\
            Call-ID: 1477173987\r\n\
            Subject: radio\r\n\
            Priority: normal\r\n\
            WG67-Version: radio.01\r\n\
            Content-Type: application/sdp\r\n\r\n\
            v=0\r\nm=audio 8574 RTP/AVP 8 0 123\r\nc=IN IP4 172.16.82.16\r\n\
            a=rtpmap:123 R2S/8000\r\na=R2S-KeepAlivePeriod:200\r\n\
            a=type:Radio-TxRx\r\na=txrxmode:TxRx\r\n";
        let a: IpAddr = [172, 16, 82, 18].into();
        let b: IpAddr = [10, 26, 185, 115].into();
        let mut rx = RadioRx::new();
        assert!(rx.udp((a, 5060), (b, 5060), invite.as_bytes(), 3.0));
        let ack = "ACK sip:109061002@10.26.185.115 SIP/2.0\r\n\
            Via: SIP/2.0/UDP 172.16.82.18:5060;rport;branch=z9hG4bK135744739\r\n\
            Call-ID: 1477173987\r\n\r\n";
        assert!(rx.udp((a, 5060), (b, 5060), ack.as_bytes(), 3.5));
        let v = rx.view();
        let s = &v.sessions[0];
        assert_eq!(s.from, "Radio (IP) @ 172.16.82.15");
        assert_eq!(s.to, "109061002 @ 10.26.185.115");
        assert_eq!(s.messages, ["INVITE", "ACK"]);
        // The ACK reuses the INVITE's branch: the answer was an error.
        assert_eq!(s.outcome, Some(Outcome::Refused));
        assert_eq!(s.media.as_deref(), Some("audio 8574 RTP/AVP 8 0 123"));
        assert!(
            s.attributes
                .contains(&("type".to_string(), "Radio-TxRx".to_string()))
        );
        assert!(
            s.extra
                .contains(&("WG67-Version".to_string(), "radio.01".to_string()))
        );
        assert!(sip(b"\x30\x35\x02\x01").is_none());
    }
}
