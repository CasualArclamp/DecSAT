// made by CasualArclamp
//! Programme audio in a link's data, played and recorded through the
//! multicast-audio player and recorder:
//! - apt-X in an IBS carrier's descrambled data (a studio link's stereo
//!   feed), decoded here and sent on as RTP L16 — PCM at the stream's own
//!   rate, which both already take;
//! - MPEG audio frames found in a modem's data (a programme feed sent
//!   straight over a TPC carrier), handed on as they are: the player decodes
//!   them, the recorder keeps them (`.mp2`).

use std::path::PathBuf;
use std::time::SystemTime;

use decsat_audio::{AudioHandle, AudioPlayer, AudioRecorder};
use decsat_ip::mcast::{AudioStream, SdpInfo};
use decsat_modem::aptx::{AptxRx, AptxStats};
use decsat_modem::ibssync::SyncStats;
use decsat_modem::mpa::MpaStats;

use crate::fec::{AUDIO_OUTPUT, FecOutput};

/// What the decoder makes of apt-X (Standard apt-X; the upper bands' step
/// tables fitted, not known: see `decsat_modem::aptx`).
const CODEC: &str = "Standard apt-X (apt-X 100)";
/// Audio a packet carries: 20 ms.
const PACKET_MS: u32 = 20;
/// RTP payload type for the L16 stream (dynamic, named by its SDP).
const PT: u8 = 96;
/// Level readings: power over this many seconds.
const LEVEL_SECS: f64 = 0.5;

/// The sample rate for a measured one: a standard rate within 1 %, else as
/// measured (a 256 kbit/s link measures a few Hz over 32 kHz).
fn snap_rate(hz: f64) -> u32 {
    [8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000]
        .into_iter()
        .find(|&r| (hz / f64::from(r) - 1.0).abs() < 0.01)
        .unwrap_or(hz.round() as u32)
}

/// What goes to the player and recorder: decoded PCM (rate, channels), or
/// MPEG audio frames (sample rate, channels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fmt {
    Pcm(u32, u16),
    Mpa(u32, u16),
}

/// The audio as a stream the player and recorder understand.
fn stream(f: Fmt) -> AudioStream {
    let blank = AudioStream {
        group: std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        port: 0,
        src: None,
        packets: 0,
        rate_bps: 0.0,
        rtp: false,
        pt: None,
        codec: decsat_ip::Codec::Unknown,
        sdp: None,
    };
    match f {
        Fmt::Pcm(rate, channels) => AudioStream {
            rate_bps: f64::from(rate) * 16.0 * f64::from(channels),
            rtp: true,
            pt: Some(PT),
            codec: decsat_ip::Codec::Pcm,
            sdp: Some(SdpInfo {
                pt: Some(PT),
                encoding: Some(format!("L16/{rate}/{channels}")),
                ..Default::default()
            }),
            ..blank
        },
        // An elementary stream in plain UDP: the frames as they come.
        Fmt::Mpa(..) => AudioStream {
            codec: decsat_ip::Codec::MpegAudio,
            ..blank
        },
    }
}

/// Programme audio, for display.
#[derive(Debug, Clone, Default)]
pub struct ProgrammeView {
    /// The codec, as far as it is decoded.
    pub codec: String,
    /// The IBS scrambling the data came out of.
    pub ibs: Option<SyncStats>,
    pub aptx: AptxStats,
    /// MPEG audio frames, when that is what the link carries.
    pub mpeg: Option<MpaStats>,
    /// Once found: the sample rate and channels.
    pub rate: u32,
    pub channels: u16,
    /// Each channel's level over the last half second, dBFS (apt-X: the
    /// MPEG audio is decoded by the player alone).
    pub levels_db: Vec<f32>,
    pub playing: bool,
    pub audio: Option<AudioHandle>,
    pub recording: bool,
    /// The recording (current or last) and its bytes.
    pub record_file: Option<(PathBuf, u64)>,
    pub error: Option<String>,
}

/// One output (a player or a recorder) and what it waits to send.
struct Out<T> {
    sink: T,
    /// What it was started for.
    format: Fmt,
    buf: Vec<u8>,
    seq: u16,
    ts: u32,
}

impl<T> Out<T> {
    fn new(sink: T, format: Fmt) -> Self {
        Out {
            sink,
            format,
            buf: Vec::new(),
            seq: 0,
            ts: 0,
        }
    }

    /// Samples in (interleaved); whole RTP L16 packets out through `send`.
    fn samples(&mut self, pcm: &[f32], mut send: impl FnMut(&mut T, &[u8])) {
        let Fmt::Pcm(rate, ch) = self.format else {
            return;
        };
        let frames = (rate * PACKET_MS / 1000) as usize;
        let bytes = frames * 2 * usize::from(ch);
        for &x in pcm {
            let v = (x.clamp(-1.0, 1.0) * 32767.0) as i16;
            self.buf.extend_from_slice(&v.to_be_bytes());
            if self.buf.len() == bytes {
                let p = decsat_ip::mcast::rtp_packet(PT, self.seq, self.ts, 0xA97C_0000, &self.buf);
                send(&mut self.sink, &p);
                self.seq = self.seq.wrapping_add(1);
                self.ts = self.ts.wrapping_add(frames as u32);
                self.buf.clear();
            }
        }
    }
}

/// Programme audio found in a link's data, decoded, played and recorded.
pub(crate) struct ProgrammeStage {
    rx: AptxRx,
    mpeg: Option<MpaStats>,
    /// Once found: what goes out.
    format: Option<Fmt>,
    player: Option<Out<AudioPlayer>>,
    recorder: Option<Out<AudioRecorder>>,
    want_play: bool,
    want_record: bool,
    record_last: Option<(PathBuf, u64)>,
    error: Option<String>,
    /// Power per channel, and samples, toward the next level reading.
    acc: [f64; 2],
    n: usize,
    levels_db: Vec<f32>,
}

impl ProgrammeStage {
    pub(crate) fn new() -> Self {
        ProgrammeStage {
            rx: AptxRx::new(),
            mpeg: None,
            format: None,
            player: None,
            recorder: None,
            want_play: false,
            want_record: false,
            record_last: None,
            error: None,
            acc: [0.0; 2],
            n: 0,
            levels_db: Vec::new(),
        }
    }

    /// Play and record as asked (once the format is known).
    pub(crate) fn follow(&mut self, o: &FecOutput) {
        self.want_play = o.programme_play;
        self.want_record = o.programme_record;
        // A change of format (found again differently) starts both afresh.
        let fmt = self.format;
        if self.player.as_ref().is_some_and(|p| Some(p.format) != fmt) || !self.want_play {
            self.player = None;
        }
        // Rust note: `&& let` chains a pattern match onto the condition.
        if (self
            .recorder
            .as_ref()
            .is_some_and(|r| Some(r.format) != fmt)
            || !self.want_record)
            && let Some(r) = self.recorder.take()
        {
            self.record_last = r.sink.path().map(|p| (p.to_path_buf(), r.sink.bytes));
        }
        let Some(f) = fmt else {
            return;
        };
        if self.want_play && self.player.is_none() {
            match AudioPlayer::start(&stream(f), AUDIO_OUTPUT) {
                Ok(p) => self.player = Some(Out::new(p, f)),
                Err(e) => self.error = Some(e),
            }
        }
        if self.want_record && self.recorder.is_none() {
            let stamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let stem = format!(
                "decsat-{}-{}-programme-{stamp}",
                o.name.replace(' ', "_"),
                crate::rf::freq_tag(o.carrier_hz)
            );
            let _ = std::fs::create_dir_all(&o.dir);
            match AudioRecorder::start(&stream(f), &o.dir, &stem) {
                Ok(r) => self.recorder = Some(Out::new(r, f)),
                Err(e) => self.error = Some(e),
            }
        }
    }

    /// apt-X: the link's data octets in, and its data rate (for the sample
    /// rate).
    pub(crate) fn data(&mut self, data: &[u8], data_bps: f64, o: &FecOutput) {
        self.rx.push(data);
        if self.rx.stats.locked {
            let f = Fmt::Pcm(snap_rate(self.rx.sample_rate(data_bps)), self.rx.channels());
            if self.format != Some(f) {
                self.format = Some(f);
                self.follow(o);
            }
        }
        let pcm = std::mem::take(&mut self.rx.pcm);
        if pcm.is_empty() {
            return;
        }
        let Some(Fmt::Pcm(rate, ch)) = self.format else {
            return;
        };
        let chans = usize::from(ch);
        for frame in pcm.chunks(chans) {
            for (a, &x) in self.acc.iter_mut().zip(frame) {
                *a += f64::from(x) * f64::from(x);
            }
            self.n += 1;
        }
        if self.n as f64 >= LEVEL_SECS * f64::from(rate) {
            self.levels_db = self.acc[..chans]
                .iter()
                .map(|&a| (10.0 * (a / self.n as f64).max(1e-12).log10()) as f32)
                .collect();
            self.acc = [0.0; 2];
            self.n = 0;
        }
        if let Some(p) = &mut self.player {
            p.samples(&pcm, |s, pkt| s.packet(pkt));
        }
        if let Some(r) = &mut self.recorder {
            r.samples(&pcm, |s, pkt| s.packet(pkt));
        }
    }

    /// MPEG audio: whole frames in, as found in the link's data.
    pub(crate) fn mpeg(&mut self, frames: &[Vec<u8>], stats: &MpaStats, o: &FecOutput) {
        self.mpeg = Some(stats.clone());
        if let Some(h) = stats.header {
            let f = Fmt::Mpa(h.sample_rate, if h.mode == 3 { 1 } else { 2 });
            if self.format != Some(f) {
                self.format = Some(f);
                self.follow(o);
            }
        }
        for f in frames {
            if let Some(p) = &mut self.player {
                p.sink.packet(f);
            }
            if let Some(r) = &mut self.recorder {
                r.sink.packet(f);
            }
        }
    }

    pub(crate) fn view(&self, ibs: Option<SyncStats>) -> ProgrammeView {
        let (rate, channels) = match self.format {
            Some(Fmt::Pcm(r, c)) | Some(Fmt::Mpa(r, c)) => (r, c),
            None => (0, 0),
        };
        let codec = match self.mpeg.as_ref().and_then(|m| m.header) {
            Some(h) => h.describe(),
            None => CODEC.into(),
        };
        ProgrammeView {
            codec,
            ibs,
            aptx: self.rx.stats.clone(),
            mpeg: self.mpeg.clone(),
            rate,
            channels,
            levels_db: self.levels_db.clone(),
            playing: self.player.is_some(),
            audio: self.player.as_ref().map(|p| p.sink.handle()),
            recording: self.recorder.is_some(),
            record_file: match &self.recorder {
                Some(r) => r.sink.path().map(|p| (p.to_path_buf(), r.sink.bytes)),
                None => self.record_last.clone(),
            },
            error: self
                .error
                .clone()
                .or_else(|| self.recorder.as_ref().and_then(|r| r.sink.error.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snaps_to_standard_rates() {
        // 256 kbit/s measured a little fast, stereo: 32 kHz.
        assert_eq!(snap_rate(273_081.2 * 30.0 / 32.0 / 8.0), 32_000);
        assert_eq!(snap_rate(15_990.0), 16_000);
        assert_eq!(snap_rate(30_000.0), 30_000);
    }
}
