//! The FEC stage of a DVB-S2 VFO: demodulated PLFRAMEs in, BBFRAMEs out.
//!
//! pilots out → scale by the measured gain → max-log LLRs, de-interleaved →
//! LDPC (layered min-sum) → BCH → BB descrambling → BBHEADER (CRC-8).
//!
//! It runs on its own thread per VFO (`FecWorker`), fed through a bounded
//! queue: the demodulator must keep real time, and when decoding cannot,
//! frames are dropped and counted rather than stalling it. Good GS-mode
//! BBFRAMEs then go through GSE to IP (every GSE variant tried, the one
//! yielding valid IP kept — `decsat_gse::GseIp`), into live statistics and,
//! while recording, a PCAP file. Good TS-mode BBFRAMEs become MPEG-TS
//! (`decsat_ts`): analysed (programmes, PIDs, continuity) and sent to a
//! `.ts` file, UDP, and/or a TCP/HTTP server for VLC or PotPlayer.
//!
//! The thread also serves the other coded modems: DVB-S symbols (Viterbi,
//! Reed–Solomon, then the TS stage) and TPC 2964 symbols (the turbo product
//! code, then the payload — HDLC frames to IP, or MPEG-TS — and, while
//! recording, the raw data to a `.bin` file).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime};

use decsat_core::Iq;
use decsat_fec::demap::{Mapper, quantize};
use decsat_fec::vlsnr::VlsnrCode;
use decsat_fec::{Bch, BchError, DecodeOutcome, FecParams, LdpcCode, LdpcDecoder};
use decsat_frame::pi2bpsk::pi2_soft;
use decsat_frame::vlsnr::{self as vl, Slot};
use decsat_frame::{BBHEADER_LEN, BbHeader, BbHeaderError, PlsInfo, StreamFormat, bb_scramble};
use decsat_gse::{GseIp, IpPacket, Source, Variant, VariantReport};
use decsat_ip::mcast::{NowPlaying, udp_payload};
use decsat_ip::{
    AudioRelay, AudioStream, Codec, Flow, IpInfo, IpStats, McastScanner, PcapWriter, PlayTarget,
    Reassembler,
};
use decsat_ts::{
    MpeExtractor, MpeStats, TS_LEN, TcpSink, TsAnalyser, TsDeframer, TsFile, TsReport, UdpSink,
};

use crate::demod::{PILOT_AFTER, PILOT_PERIOD, PlFrame};
use decsat_audio::{AudioHandle, AudioPlayer, AudioRecorder, OutputKind};

/// What the FEC thread works on: DVB-S2 frames, or DVB-S symbols.
pub(crate) enum FecInput {
    Frame(PlFrame),
    /// Carrier-locked QPSK symbols of a DVB-S carrier.
    Dvbs(Vec<Iq>),
    /// Carrier-locked symbols of a TPC 2964 carrier: QPSK, else BPSK.
    Tpc {
        symbols: Vec<Iq>,
        qpsk: bool,
    },
    /// Carrier-locked QPSK symbols of a Q-Flex FastLink carrier.
    FastLink(Vec<Iq>),
    /// Carrier-locked QPSK symbols of a K = 7 convolutional-code carrier
    /// (IESS-308/309 and the like).
    Viterbi(Vec<Iq>),
}

/// LLR quantization: steps per LLR unit (the decoder is happy from 2 to 8).
const LLR_SCALE: f32 = 4.0;
/// Where in-app audio plays: the sound card, except under test.
const AUDIO_OUTPUT: OutputKind = if cfg!(test) {
    OutputKind::Null
} else {
    OutputKind::Device
};
/// LDPC iteration budget per frame.
const MAX_ITERATIONS: usize = 50;
/// Frames queued for the FEC thread before new ones are dropped.
const QUEUE: usize = 64;

/// One decoded BBFRAME.
#[derive(Debug, Clone)]
pub struct BbFrame {
    pub pls: PlsInfo,
    /// The descrambled BBFRAME: header, data field, padding (K_bch bits).
    pub bytes: Vec<u8>,
    pub header: Result<BbHeader, BbHeaderError>,
    pub ldpc: DecodeOutcome,
    /// Bits BCH corrected, or why it could not.
    pub bch: Result<usize, BchError>,
    pub es_n0_db: f32,
}

impl BbFrame {
    /// BCH accepted the codeword and the BBHEADER's CRC-8 checks.
    pub fn ok(&self) -> bool {
        self.bch.is_ok() && self.header.is_ok()
    }
}

struct Code {
    params: FecParams,
    mapper: Mapper,
    ldpc: LdpcDecoder,
    bch: Bch,
}

/// A VL-SNR code's decoders.
struct VlCode {
    code: &'static VlsnrCode,
    ldpc: LdpcDecoder,
    bch: Bch,
    /// QPSK demapping for the 2/9 code.
    qpsk: Option<Mapper>,
}

/// Decodes PLFRAMEs; holds a decoder per code met so far.
#[derive(Default)]
pub struct FecDecoder {
    codes: HashMap<(bool, u8), Option<Code>>,
    vl_codes: HashMap<u8, Option<VlCode>>,
    sent: Vec<f32>,
    data: Vec<Iq>,
    llr: Vec<f32>,
    quantized: Vec<i8>,
    info: Vec<u8>,
}

impl FecDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one frame; `None` for a dummy frame, a frame with no code to
    /// decode (S2 short 9/10, S2X VL-SNR and reserved codes).
    pub fn decode(&mut self, f: &PlFrame) -> Option<BbFrame> {
        if f.pls.dummy_frame {
            return None;
        }
        if let (Some(set), Some((k, _))) = (f.pls.vlsnr, f.vlsnr) {
            return self.decode_vlsnr(f, set, k);
        }
        let FecDecoder {
            codes,
            data,
            llr,
            quantized,
            info,
            ..
        } = self;
        let code = codes
            .entry((f.pls.short_fecframe, f.pls.modcod))
            .or_insert_with(|| build_code(f.pls))
            .as_mut()?;
        let p = code.params;

        // Data symbols only, on the constellation's unit-power scale.
        let inv = 1.0 / f.gain.max(1e-6);
        data.clear();
        data.extend(
            f.payload
                .iter()
                .enumerate()
                .filter(|(i, _)| !f.pls.has_pilots || i % PILOT_PERIOD < PILOT_AFTER)
                .map(|(_, &y)| y * inv),
        );
        let n = p.n_ldpc;
        if data.len() != code.mapper.symbols() {
            return None;
        }
        llr.resize(n, 0.0);
        quantized.resize(n, 0);
        info.resize(p.n_bch / 8, 0);

        code.mapper.demap_llr(data, f.noise_var * inv * inv, llr);
        quantize(llr, LLR_SCALE, quantized);
        let ldpc = code.ldpc.decode(quantized, info, MAX_ITERATIONS);

        let mut bytes = info.clone();
        let bch = code.bch.decode(&mut bytes);
        bytes.truncate(p.bbframe_bytes());
        bb_scramble(&mut bytes);
        let header = BbHeader::parse(&bytes);
        Some(BbFrame {
            pls: f.pls,
            bytes,
            header,
            ldpc,
            bch,
            es_n0_db: f.es_n0_db(),
        })
    }
}

impl FecDecoder {
    /// A VL-SNR frame (EN 302 307-2 §5.5.2.6): data symbols from the
    /// layout, LLRs (pi/2-BPSK, spreading combined; or QPSK), the shortened
    /// and punctured bits put back, then LDPC and BCH as usual.
    fn decode_vlsnr(&mut self, f: &PlFrame, set: u8, k: u8) -> Option<BbFrame> {
        let vc = self
            .vl_codes
            .entry(k)
            .or_insert_with(|| {
                let code = VlsnrCode::for_header(k)?;
                Some(VlCode {
                    code,
                    ldpc: LdpcDecoder::new(LdpcCode::new(code.table)),
                    bch: Bch::new(code.frame, 12, code.n_bch),
                    qpsk: code.qpsk.then(|| {
                        Mapper::s2(
                            decsat_fec::Constellation::qpsk(),
                            decsat_core::CodeRate::new(2, 9),
                            code.sent_bits(),
                        )
                    }),
                })
            })
            .as_mut()?;
        let code = vc.code;
        let inv = 1.0 / f.gain.max(1e-6);
        let noise = (f.noise_var * inv * inv).max(1e-6);
        let layout = vl::layout(set);
        self.data.clear();
        self.data.extend(
            f.payload
                .iter()
                .zip(layout)
                .filter(|(_, s)| **s == Slot::Data)
                .map(|(&y, _)| y * inv),
        );
        if self.data.len() != code.symbols() {
            return None;
        }
        self.sent.clear();
        match &vc.qpsk {
            Some(m) => {
                self.sent.resize(code.sent_bits(), 0.0);
                m.demap_llr(&self.data, noise, &mut self.sent);
            }
            None => {
                // Unit-amplitude 2-PAM: LLR = 4·y/N0; spread bits add.
                let reps = if code.spread { 2 } else { 1 };
                for pair in self.data.chunks(reps).enumerate() {
                    let (j, ys) = pair;
                    let llr: f32 = ys
                        .iter()
                        .enumerate()
                        .map(|(r, &y)| 4.0 * pi2_soft(j * reps + r, y) / noise)
                        .sum();
                    self.sent.push(llr);
                }
            }
        }
        let n = code.n_ldpc();
        self.llr.resize(n, 0.0);
        code.expand_llr(&self.sent, &mut self.llr);
        self.quantized.resize(n, 0);
        quantize(&self.llr, LLR_SCALE, &mut self.quantized);
        self.info.resize(code.k_ldpc() / 8, 0);
        let ldpc = vc
            .ldpc
            .decode(&self.quantized, &mut self.info, MAX_ITERATIONS);

        let at = code.xs / 8;
        let mut bytes = self.info[at..at + code.n_bch / 8].to_vec();
        let bch = vc.bch.decode(&mut bytes);
        bytes.truncate(code.bbframe_bytes());
        bb_scramble(&mut bytes);
        // A ragged K (the medium codes): the last byte's spare bits are
        // BCH parity, not BBFRAME.
        if !code.k_bch.is_multiple_of(8)
            && let Some(last) = bytes.last_mut()
        {
            *last &= 0xFF << (8 - code.k_bch % 8);
        }
        let header = BbHeader::parse(&bytes);
        Some(BbFrame {
            pls: f.pls,
            bytes,
            header,
            ldpc,
            bch,
            es_n0_db: f.es_n0_db(),
        })
    }
}

fn build_code(pls: PlsInfo) -> Option<Code> {
    let mc = pls.modcod()?;
    let params = FecParams::new(mc.frame, mc.rate)?;
    Some(Code {
        params,
        mapper: Mapper::for_modcod(&mc)?,
        ldpc: LdpcDecoder::new(LdpcCode::new(params.ldpc_table())),
        bch: Bch::new(mc.frame, params.t, params.n_bch),
    })
}

/// What a VFO's FEC has done, for display.
#[derive(Debug, Clone, Default)]
pub struct FecStats {
    /// Data frames decoded (dummy frames excluded).
    pub frames: u64,
    /// BCH and the BBHEADER CRC both passed.
    pub ok: u64,
    /// BCH could not correct (LDPC left too many errors).
    pub bch_failed: u64,
    /// BCH passed but the BBHEADER CRC did not.
    pub crc_failed: u64,
    /// LDPC ended with checks unsatisfied (BCH may still have rescued it).
    pub ldpc_unconverged: u64,
    /// Frames dropped because decoding fell behind.
    pub dropped: u64,
    /// LDPC iterations, summed over `frames`.
    pub iterations: u64,
    /// Bits BCH corrected, summed.
    pub bch_corrected: u64,
    /// Es/N0 of the latest frame, from its known symbols.
    pub es_n0_db: Option<f32>,
    /// The latest valid BBHEADER.
    pub last_header: Option<BbHeader>,
    /// Good frames per input stream (ISI; 0 for a single stream).
    pub streams: BTreeMap<u8, u64>,
    /// (good, total) per MODCOD index.
    pub per_modcod: BTreeMap<u8, (u64, u64)>,
    /// Useful bit rate (BBHEADER DFL) over the last couple of seconds of
    /// signal, bits per second.
    pub payload_bps: f64,
    /// Fraction of real time the FEC thread is busy.
    pub load: f32,
    /// Good TS-mode frames.
    pub ts_frames: u64,
    /// What GSE and IP have made of the GS-mode frames.
    pub gse: Option<GseView>,
    /// The MPEG-TS from the TS-mode frames (MPEG-TS decoders).
    pub ts: Option<TsView>,
    /// DVB-S: the code rate found, Viterbi and Reed–Solomon.
    pub dvbs: Option<decsat_modem::dvbs::DvbsStats>,
    /// TPC 2964: frame sync, the structure found, decoding.
    pub tpc: Option<decsat_modem::tpc2964::TpcStats>,
    /// Q-Flex FastLink: frame sync and LDPC decoding.
    pub fastlink: Option<decsat_modem::fastlink::FastLinkStats>,
    /// The 257-bit TDM multiplex inside a Paradise-framed payload, once
    /// its alignment word was found.
    pub tdm: Option<decsat_modem::tdm257::TdmStats>,
    /// A K = 7 convolutional code: the rate and orientation found.
    pub viterbi: Option<decsat_modem::dvbs::ViterbiStats>,
    /// What a modem's data carry (TPC 2964).
    pub payload: Option<decsat_modem::payload::PayloadStats>,
    /// The raw data file (TPC 2964, while recording) and its bytes.
    pub raw_file: Option<(PathBuf, u64)>,
    pub raw_active: bool,
    /// Recordings started by activity in the TDM multiplex.
    pub raw_triggered: u64,
    /// Text in a modem's data while what they carry is not known (TPC
    /// 2964), every reading of the bits at once.
    pub text: Option<decsat_modem::text::TextView>,
    /// An E1 in a modem's data: its framing and voice channels.
    pub e1: Option<E1View>,
    /// A TDM multiplex's call voice being played or recorded.
    pub call_voice: Option<CallVoiceView>,
}

/// A call's voice out of the TDM multiplex, for display.
#[derive(Debug, Clone, Default)]
pub struct CallVoiceView {
    pub playing: bool,
    /// Playing by itself (speech was found, `voice_auto`), not by ▶.
    pub auto: bool,
    /// Playing by itself and holding the voice floor: heard now.
    pub on_air: bool,
    pub audio: Option<AudioHandle>,
    pub recording: bool,
    /// The recording (current or last) and its bytes.
    pub record_file: Option<(PathBuf, u64)>,
    pub error: Option<String>,
}

/// A Q-Flex call's G.728 voice (decoded by the TDM receiver) through the
/// multicast-audio player and recorder as RTP A-law, as an E1 timeslot
/// goes: 16 samples a 2 ms TDM frame, silence when there is no speech.
struct CallVoice {
    play: bool,
    /// Playing by itself, as the voice floor allows, and whether the
    /// floor lets it be heard now.
    auto: bool,
    heard: bool,
    /// Its number on the voice floor.
    source: u64,
    player: Option<AudioPlayer>,
    play_q: VecDeque<u8>,
    play_buf: Vec<u8>,
    play_rtp: (u16, u32),
    record: bool,
    recorder: Option<AudioRecorder>,
    record_q: VecDeque<u8>,
    record_buf: Vec<u8>,
    record_rtp: (u16, u32),
    record_last: Option<(PathBuf, u64)>,
    error: Option<String>,
    /// TDM frames already accounted for.
    frames: u64,
}

impl CallVoice {
    fn new() -> Self {
        Self {
            play: false,
            auto: false,
            heard: false,
            source: crate::floor::new_source(),
            player: None,
            play_q: VecDeque::new(),
            play_buf: Vec::new(),
            play_rtp: (0, 0),
            record: false,
            recorder: None,
            record_q: VecDeque::new(),
            record_buf: Vec::new(),
            record_rtp: (0, 0),
            record_last: None,
            error: None,
            frames: 0,
        }
    }

    /// `voice`: the call's stream, once found. With `voice_auto` it plays
    /// by itself once it has spoken (▶ plays it regardless).
    fn follow(&mut self, o: &FecOutput, voice: Option<decsat_modem::tdm257::VoiceView>) {
        let spoken = voice.is_some_and(|v| v.talk_blocks > 0);
        self.auto = !o.tdm_play && o.voice_auto && spoken;
        let want = o.tdm_play || self.auto;
        if want != self.play {
            self.play = want;
            self.player = None;
            self.play_q.clear();
            self.play_buf.clear();
            if self.play {
                match AudioPlayer::start(&e1_stream(0), AUDIO_OUTPUT) {
                    Ok(p) => self.player = Some(p),
                    Err(e) => self.error = Some(e),
                }
            }
        }
        if o.tdm_record != self.record {
            self.record = o.tdm_record;
            if let Some(r) = self.recorder.take() {
                self.record_last = r.path().map(|p| (p.to_path_buf(), r.bytes));
            }
            self.record_q.clear();
            self.record_buf.clear();
            if self.record {
                let stamp = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let stem = format!(
                    "decsat-{}-{}-call-{stamp}",
                    o.name.replace(' ', "_"),
                    crate::rf::freq_tag(o.carrier_hz)
                );
                let _ = std::fs::create_dir_all(&o.dir);
                match AudioRecorder::start(&e1_stream(0), &o.dir, &stem) {
                    Ok(r) => self.recorder = Some(r),
                    Err(e) => self.error = Some(e),
                }
            }
        }
    }

    /// The receiver's decoded speech in; for each TDM frame since last
    /// time, 2 ms of A-law out to the player and recorder.
    fn feed(&mut self, rx: &mut decsat_modem::tdm257::TdmRx) {
        use decsat_ip::mcast::rtp_packet;
        rx.decode_voice = self.play || self.record;
        // Playing by itself, it waits its turn on the voice floor; ▶
        // plays it whatever the floor says.
        let talking = rx.stats.voice.is_some_and(|v| v.talking);
        self.heard = !self.auto || crate::floor::may_play((self.source, 0), talking);
        for &x in &rx.voice_pcm {
            let b = decsat_modem::e1::alaw_encode(x);
            if self.play && self.heard {
                self.play_q.push_back(b);
            }
            if self.record {
                self.record_q.push_back(b);
            }
        }
        rx.voice_pcm.clear();
        for q in [&mut self.play_q, &mut self.record_q] {
            if q.len() > VOICE_BACKLOG {
                q.drain(..q.len() - VOICE_BACKLOG / 2);
            }
        }
        let new = rx.stats.frames.saturating_sub(self.frames);
        self.frames = rx.stats.frames;
        for _ in 0..new * decsat_modem::tdm257::WORDS as u64 {
            if let Some(p) = &mut self.player {
                self.play_buf.push(voice_byte(&mut self.play_q));
                if self.play_buf.len() == E1_CHUNK {
                    let (seq, ts) = &mut self.play_rtp;
                    p.packet(&rtp_packet(8, *seq, *ts, 0xCA11_0000, &self.play_buf));
                    *seq = seq.wrapping_add(1);
                    *ts = ts.wrapping_add(E1_CHUNK as u32);
                    self.play_buf.clear();
                }
            }
            if let Some(r) = &mut self.recorder {
                self.record_buf.push(voice_byte(&mut self.record_q));
                if self.record_buf.len() == E1_CHUNK {
                    let (seq, ts) = &mut self.record_rtp;
                    r.packet(&rtp_packet(8, *seq, *ts, 0xCA11_0000, &self.record_buf));
                    *seq = seq.wrapping_add(1);
                    *ts = ts.wrapping_add(E1_CHUNK as u32);
                    self.record_buf.clear();
                }
            }
        }
    }

    fn view(&self) -> CallVoiceView {
        CallVoiceView {
            playing: self.player.is_some(),
            auto: self.auto,
            on_air: self.auto && self.heard && self.player.is_some(),
            audio: self.player.as_ref().map(|p| p.handle()),
            recording: self.recorder.is_some(),
            record_file: match &self.recorder {
                Some(r) => r.path().map(|p| (p.to_path_buf(), r.bytes)),
                None => self.record_last.clone(),
            },
            error: self
                .error
                .clone()
                .or_else(|| self.recorder.as_ref().and_then(|r| r.error.clone())),
        }
    }
}

/// An E1's state, for display.
#[derive(Debug, Clone, Default)]
pub struct E1View {
    /// How the channels are carried: "E1 (G.704)", "D&I++" or HDLC
    /// packets.
    pub source: String,
    /// D&I++ or packets: the 64 kbit/s timeslots carried are channels
    /// 1..=n; `None` for a whole E1 (timeslots 1–31).
    pub channels: Option<u8>,
    /// Timeslots in HDLC packets: packets taken, and packets lost.
    pub packets: Option<(u64, u64)>,
    pub stats: decsat_modem::e1::E1Stats,
    /// Level of each timeslot over the last half second, dBFS (A-law
    /// decoded; idle channels sit near −70, data near −5).
    pub levels_db: Vec<f32>,
    /// What each timeslot carries, judged from which bits change: G.711, or
    /// something that only sounds like noise played as A-law.
    pub coding: Vec<decsat_modem::e1::Coding>,
    /// G.728 voice found in a sub-rate timeslot: the first of its two bits
    /// (G.704 numbering) and whether it is speaking now. Such a timeslot
    /// plays and records decoded.
    pub voice: Vec<Option<(u8, bool)>>,
    /// The timeslot playing ([`MIX`]: every G.728 channel, by itself),
    /// and its player.
    pub playing: Option<u8>,
    pub audio: Option<AudioHandle>,
    /// The channel of this carrier holding the voice floor (heard now,
    /// others waiting; see [`crate::floor`]).
    pub on_air: Option<u8>,
    /// The timeslot recording, and the file (current or last) with bytes.
    pub recording: Option<u8>,
    pub record_file: Option<(PathBuf, u64)>,
    pub error: Option<String>,
}

/// An E1's voice channels: their levels, and one played and one recorded
/// through the multicast-audio player and recorder as RTP A-law (PCMA,
/// 8 kHz) — G.711 is what both already take.
struct E1Stage {
    /// Power per timeslot over the current 10 ms chunk, and the chunks'
    /// powers over the current half second.
    acc: [f64; decsat_modem::e1::TIMESLOTS],
    n: u32,
    chunks: Vec<[f64; decsat_modem::e1::TIMESLOTS]>,
    levels_db: Vec<f32>,
    activity: decsat_modem::e1::BitActivity,
    play_want: Option<u8>,
    player: Option<AudioPlayer>,
    play_buf: Vec<u8>,
    play_rtp: (u16, u32),
    record_want: Option<u8>,
    recorder: Option<AudioRecorder>,
    record_buf: Vec<u8>,
    record_rtp: (u16, u32),
    record_last: Option<(PathBuf, u64)>,
    error: Option<String>,
    /// D&I++: timeslots carried, and the frame being filled from its bytes.
    dandi: Option<u8>,
    dandi_frame: [u8; decsat_modem::e1::TIMESLOTS],
    dandi_next: usize,
    /// A G.728 finder on each sub-rate timeslot (the CDM-600L's D&I voice
    /// is G.728 in bits 2–3); only the played or recorded one decodes.
    voice: Vec<Option<decsat_modem::g728::Voice>>,
    voice_pcm: Vec<f32>,
    /// Its speech as A-law, waiting to go out a byte a frame in place of
    /// the timeslot's own bytes.
    play_voice: VecDeque<u8>,
    record_voice: VecDeque<u8>,
    /// Playing [`MIX`]: each channel's decoded speech, mixed as it plays,
    /// and which channels the voice floor lets be heard.
    mix: Vec<VecDeque<f32>>,
    heard: [bool; decsat_modem::e1::TIMESLOTS],
    /// This stage's number on the voice floor.
    source: u64,
    /// Timeslots from HDLC packets: the channels seen (a mask), each
    /// channel's octets not yet made into frames, and when (in packets)
    /// each was last heard from.
    slots: Option<u16>,
    slot_fifo: Vec<VecDeque<u8>>,
    slot_seen: [Option<u64>; decsat_modem::e1::TIMESLOTS],
    slot_count: u64,
}

/// [`E1View::playing`] when the voice preset plays every channel with
/// G.728 on it (as the floor allows), mixed. Timeslot 0 is an E1's
/// alignment, never a voice channel.
pub const MIX: u8 = 0;

/// A packet channel not heard from for this many packets is taken to have
/// stopped, and no longer holds the others' frames back.
const SLOT_STALE: u64 = 16;

/// Decoded G.728 waiting to play or record, at most (in samples): it comes
/// 40 ms at a time and leaves a sample a frame, so it never needs more.
const VOICE_BACKLOG: usize = 1600;

/// The next byte of a timeslot's decoded voice, A-law silence when there is
/// none (between talk spurts the finder sends nothing).
fn voice_byte(q: &mut VecDeque<u8>) -> u8 {
    q.pop_front().unwrap_or(0xD5)
}

/// Frames in a level chunk (10 ms), and chunks in a reading (half a
/// second). A timeslot's level is the 80th percentile of its chunks' power:
/// a few garbled frames while the alignment is lost and found again (an
/// average would read them as speech on an idle channel) are left out, yet a
/// channel speaking for a fifth of the time shows.
const E1_CHUNK_FRAMES: u32 = 80;
const E1_LEVEL_CHUNKS: usize = 50;
/// A-law bytes per RTP packet: 20 ms.
const E1_CHUNK: usize = 160;

/// A timeslot as an audio stream the player and recorder understand.
fn e1_stream(ts: u8) -> AudioStream {
    AudioStream {
        group: std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        port: ts as u16,
        src: None,
        packets: 0,
        rate_bps: 64e3,
        rtp: true,
        pt: Some(8), // PCMA (RFC 3551)
        codec: decsat_ip::Codec::Pcm,
        sdp: None,
    }
}

impl E1Stage {
    fn new() -> Self {
        E1Stage {
            acc: [0.0; decsat_modem::e1::TIMESLOTS],
            n: 0,
            chunks: Vec::new(),
            levels_db: Vec::new(),
            activity: decsat_modem::e1::BitActivity::new(),
            play_want: None,
            player: None,
            play_buf: Vec::new(),
            play_rtp: (0, 0),
            record_want: None,
            recorder: None,
            record_buf: Vec::new(),
            record_rtp: (0, 0),
            record_last: None,
            error: None,
            dandi: None,
            dandi_frame: [0xD5; decsat_modem::e1::TIMESLOTS],
            dandi_next: 0,
            voice: (0..decsat_modem::e1::TIMESLOTS).map(|_| None).collect(),
            voice_pcm: Vec::new(),
            play_voice: VecDeque::new(),
            record_voice: VecDeque::new(),
            mix: (0..decsat_modem::e1::TIMESLOTS)
                .map(|_| VecDeque::new())
                .collect(),
            heard: [false; decsat_modem::e1::TIMESLOTS],
            source: crate::floor::new_source(),
            slots: None,
            slot_fifo: (0..decsat_modem::e1::TIMESLOTS)
                .map(|_| VecDeque::new())
                .collect(),
            slot_seen: [None; decsat_modem::e1::TIMESLOTS],
            slot_count: 0,
        }
    }

    /// A G.728 finder on every sub-rate timeslot (replaced when the
    /// changing bits do, kept once it has found G.728), and the played or
    /// recorded one's speech queued as A-law.
    fn voice(&mut self, f: &[u8; decsat_modem::e1::TIMESLOTS]) {
        use decsat_modem::e1::Coding;
        use decsat_modem::g728::Voice;
        for ts in 1..decsat_modem::e1::TIMESLOTS {
            // Once G.728 is found it stays: a few garbled frames can make
            // the sign bit seem to move (a half second judged G.711), and
            // silence fill can still other bits.
            let found = self.voice[ts]
                .as_ref()
                .and_then(|v| v.lane())
                .map(|l| l.bits[0] | l.bits[1]);
            match self.activity.coding()[ts] {
                Coding::SubRate(m)
                    if self.voice[ts].as_ref().map(|v| v.mask) != Some(m)
                        && found.is_none_or(|b| m & b != b) =>
                {
                    self.voice[ts] = Some(Voice::new(m));
                }
                Coding::G711 if found.is_none() => self.voice[ts] = None,
                _ => {}
            }
            let Some(v) = &mut self.voice[ts] else {
                continue;
            };
            let mixing = self.play_want == Some(MIX);
            let (play, rec) = (
                self.play_want == Some(ts as u8),
                self.record_want == Some(ts as u8),
            );
            self.voice_pcm.clear();
            v.push(&f[ts..=ts], play || rec || mixing, &mut self.voice_pcm);
            for &x in &self.voice_pcm {
                if mixing {
                    self.mix[ts].push_back(x);
                }
                let b = decsat_modem::e1::alaw_encode(x);
                if play {
                    self.play_voice.push_back(b);
                }
                if rec {
                    self.record_voice.push_back(b);
                }
            }
            let q = &mut self.mix[ts];
            if q.len() > VOICE_BACKLOG {
                q.drain(..q.len() - VOICE_BACKLOG / 2);
            }
        }
        for q in [&mut self.play_voice, &mut self.record_voice] {
            if q.len() > VOICE_BACKLOG {
                q.drain(..q.len() - VOICE_BACKLOG / 2);
            }
        }
    }

    /// Timeslot `t`'s G.728 lane, once found.
    fn lane(&self, t: usize) -> Option<&decsat_modem::g728::Lane> {
        self.voice.get(t)?.as_ref()?.lane()
    }

    /// The next sample of the mix: every channel's speech the voice floor
    /// lets be heard (A-law silence when there is none).
    fn mix_byte(&mut self) -> u8 {
        let mut sum = 0.0f32;
        let mut any = false;
        for (t, q) in self.mix.iter_mut().enumerate() {
            if let Some(x) = q.pop_front()
                && self.heard[t]
            {
                sum += x;
                any = true;
            }
        }
        if any {
            decsat_modem::e1::alaw_encode(sum.clamp(-1.0, 1.0))
        } else {
            0xD5
        }
    }

    /// Ask the voice floor which channels may be heard in the mix.
    fn ask_floor(&mut self) {
        for t in 1..decsat_modem::e1::TIMESLOTS {
            let talking = self.lane(t).map(|l| l.talking());
            self.heard[t] = match talking {
                Some(talking) => crate::floor::may_play((self.source, t as u8), talking),
                None => false,
            };
        }
    }

    /// A timeslot's own byte when it is G.711 audio (or a steady pattern:
    /// idle or a tone); A-law silence while it is still being judged or is
    /// sub-rate with no codec found — played as A-law that is loud noise.
    fn plain_byte(&self, f: &[u8; decsat_modem::e1::TIMESLOTS], t: u8) -> u8 {
        use decsat_modem::e1::Coding;
        match self.activity.coding()[t as usize] {
            Coding::G711 | Coding::Steady => f[t as usize],
            Coding::Unknown | Coding::SubRate(_) => 0xD5,
        }
    }

    /// Whether timeslot `t` plays and records as decoded G.728.
    fn is_voice(&self, t: u8) -> bool {
        self.voice
            .get(t as usize)
            .is_some_and(|v| v.as_ref().is_some_and(|v| v.lane().is_some()))
    }

    /// D&I++ bytes, `n` timeslots taking turns: made into E1-shaped frames
    /// (channel k in timeslot k, the rest idle A-law) for the meters,
    /// player and recorder.
    fn dandi_bytes(&mut self, n: u8, bytes: &[u8]) {
        let n = n.clamp(1, 31) as usize;
        if self.dandi != Some(n as u8) {
            self.dandi = Some(n as u8);
            self.dandi_next = 0;
        }
        let mut frames = Vec::with_capacity(bytes.len() / n + 1);
        for &b in bytes {
            self.dandi_frame[1 + self.dandi_next] = b;
            self.dandi_next += 1;
            if self.dandi_next == n {
                frames.push(self.dandi_frame);
                self.dandi_next = 0;
            }
        }
        self.frames(&frames);
    }

    /// Timeslots from HDLC packets, `(channel, octets)`: made into
    /// E1-shaped frames (channel k in timeslot k, the rest idle A-law) once
    /// every channel still sending has an octet for them.
    fn slot_packets(&mut self, packets: &[(u8, Vec<u8>)], channels: u16) {
        use decsat_modem::e1::TIMESLOTS;
        self.slots = Some(channels);
        for (t, octets) in packets {
            let t = *t as usize;
            if (1..TIMESLOTS).contains(&t) {
                self.slot_fifo[t].extend(octets.iter().copied());
                self.slot_seen[t] = Some(self.slot_count);
                self.slot_count += 1;
            }
        }
        let now = self.slot_count;
        let mut live = Vec::new();
        for t in 1..TIMESLOTS {
            match self.slot_seen[t] {
                Some(at) if now - at <= SLOT_STALE => live.push(t),
                _ => self.slot_fifo[t].clear(),
            }
        }
        let n = live
            .iter()
            .map(|&t| self.slot_fifo[t].len())
            .min()
            .unwrap_or(0);
        let frames: Vec<[u8; TIMESLOTS]> = (0..n)
            .map(|_| {
                let mut f = [0xD5; TIMESLOTS];
                for &t in &live {
                    f[t] = self.slot_fifo[t].pop_front().unwrap_or(0xD5);
                }
                f
            })
            .collect();
        self.frames(&frames);
    }

    fn follow(&mut self, o: &FecOutput) {
        // The timeslot chosen, else (asked to) every channel with G.728,
        // mixed, once one has spoken.
        let spoken = (1..decsat_modem::e1::TIMESLOTS)
            .any(|t| self.lane(t).is_some_and(|l| l.framer.talk_blocks > 0));
        let auto = (o.voice_auto && spoken).then_some(MIX);
        let want = o.e1_play.or(auto);
        if want != self.play_want {
            self.play_want = want;
            self.player = None;
            self.play_buf.clear();
            self.play_voice.clear();
            for q in &mut self.mix {
                q.clear();
            }
            if let Some(t) = want {
                match AudioPlayer::start(&e1_stream(t), AUDIO_OUTPUT) {
                    Ok(p) => self.player = Some(p),
                    Err(e) => self.error = Some(e),
                }
            }
        }
        if o.e1_record != self.record_want {
            self.record_want = o.e1_record;
            if let Some(r) = self.recorder.take() {
                self.record_last = r.path().map(|p| (p.to_path_buf(), r.bytes));
            }
            self.record_buf.clear();
            self.record_voice.clear();
            if let Some(t) = o.e1_record {
                let stamp = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let stem = format!(
                    "decsat-{}-{}-e1-ts{t:02}-{stamp}",
                    o.name.replace(' ', "_"),
                    crate::rf::freq_tag(o.carrier_hz)
                );
                let _ = std::fs::create_dir_all(&o.dir);
                match AudioRecorder::start(&e1_stream(t), &o.dir, &stem) {
                    Ok(r) => self.recorder = Some(r),
                    Err(e) => self.error = Some(e),
                }
            }
        }
    }

    fn frames(&mut self, frames: &[[u8; decsat_modem::e1::TIMESLOTS]]) {
        use decsat_ip::mcast::rtp_packet;
        if self.play_want == Some(MIX) {
            self.ask_floor();
        }
        for f in frames {
            self.activity.push(f);
            self.voice(f);
            for (a, &b) in self.acc.iter_mut().zip(f) {
                let v = decsat_modem::e1::alaw(b) as f64;
                *a += v * v;
            }
            self.n += 1;
            if self.n >= E1_CHUNK_FRAMES {
                let n = self.n as f64;
                let mut c = self.acc;
                for v in &mut c {
                    *v /= n;
                }
                self.chunks.push(c);
                self.acc = [0.0; decsat_modem::e1::TIMESLOTS];
                self.n = 0;
            }
            if self.chunks.len() >= E1_LEVEL_CHUNKS {
                self.levels_db = (0..decsat_modem::e1::TIMESLOTS)
                    .map(|k| {
                        let mut p: Vec<f64> = self.chunks.iter().map(|c| c[k]).collect();
                        p.sort_unstable_by(f64::total_cmp);
                        let v = p[p.len() * 4 / 5];
                        (10.0 * v.max(1e-12).log10()) as f32
                    })
                    .collect();
                self.chunks.clear();
            }
            // One RTP packet per 20 ms of the chosen timeslot.
            // A timeslot carrying G.728 goes out decoded.
            let play_byte = match self.play_want {
                Some(MIX) => Some(self.mix_byte()),
                Some(t) if self.is_voice(t) => Some(voice_byte(&mut self.play_voice)),
                Some(t) => Some(self.plain_byte(f, t)),
                None => None,
            };
            let record_byte = match self.record_want {
                Some(t) if self.is_voice(t) => Some(voice_byte(&mut self.record_voice)),
                Some(t) => Some(self.plain_byte(f, t)),
                None => None,
            };
            if let (Some(t), Some(p), Some(b)) = (self.play_want, &mut self.player, play_byte) {
                self.play_buf.push(b);
                if self.play_buf.len() == E1_CHUNK {
                    let (seq, ts) = &mut self.play_rtp;
                    p.packet(&rtp_packet(
                        8,
                        *seq,
                        *ts,
                        0xE1E1_0000 | t as u32,
                        &self.play_buf,
                    ));
                    *seq = seq.wrapping_add(1);
                    *ts = ts.wrapping_add(E1_CHUNK as u32);
                    self.play_buf.clear();
                }
            }
            if let (Some(t), Some(r), Some(b)) = (self.record_want, &mut self.recorder, record_byte)
            {
                self.record_buf.push(b);
                if self.record_buf.len() == E1_CHUNK {
                    let (seq, ts) = &mut self.record_rtp;
                    r.packet(&rtp_packet(
                        8,
                        *seq,
                        *ts,
                        0xE1E1_0000 | t as u32,
                        &self.record_buf,
                    ));
                    *seq = seq.wrapping_add(1);
                    *ts = ts.wrapping_add(E1_CHUNK as u32);
                    self.record_buf.clear();
                }
            }
        }
    }

    fn view(&self, stats: decsat_modem::e1::E1Stats) -> E1View {
        // The highest channel a packet has carried: rows 1..=it.
        let slot_last = self.slots.map(|m| (15 - m.leading_zeros().min(15)) as u8);
        let holder = crate::floor::holder();
        E1View {
            source: match (self.dandi, slot_last) {
                (Some(n), _) => format!("Comtech D&I++, {n} × 64 kbit/s"),
                (None, Some(_)) => {
                    let n = self.slots.unwrap_or(0).count_ones();
                    format!("timeslots in HDLC packets, {n} × 64 kbit/s")
                }
                (None, None) => "E1 (G.704)".into(),
            },
            channels: self.dandi.or(slot_last),
            packets: None,
            on_air: holder
                .filter(|h| h.0 == self.source && self.play_want == Some(MIX))
                .map(|h| h.1),
            stats,
            levels_db: self.levels_db.clone(),
            coding: self.activity.coding().to_vec(),
            voice: self
                .voice
                .iter()
                .map(|v| {
                    let l = v.as_ref()?.lane()?;
                    Some((l.bits[0].leading_zeros() as u8 + 1, l.talking()))
                })
                .collect(),
            playing: self.play_want.filter(|_| self.player.is_some()),
            audio: self.player.as_ref().map(|p| p.handle()),
            recording: self.record_want.filter(|_| self.recorder.is_some()),
            record_file: match &self.recorder {
                Some(r) => r.path().map(|p| (p.to_path_buf(), r.bytes)),
                None => self.record_last.clone(),
            },
            error: self
                .error
                .clone()
                .or_else(|| self.recorder.as_ref().and_then(|r| r.error.clone())),
        }
    }
}

/// The transport stream and its outputs, for display.
#[derive(Debug, Clone, Default)]
pub struct TsView {
    pub packets: u64,
    /// Packets whose CRC-8 failed (flagged with the transport error bit).
    pub crc_errors: u64,
    /// Continuity counter jumps, all PIDs.
    pub cc_errors: u64,
    pub nulls_reinserted: u64,
    pub resyncs: u64,
    /// The stream uses ISSY, which is not read yet.
    pub issy: bool,
    /// TS rate over the last couple of seconds of signal, bits per second.
    pub ts_bps: f64,
    /// Everything the analyser knows: services, PIDs, network, tables.
    pub report: TsReport,
    pub file: Option<(PathBuf, u64)>,
    pub file_active: bool,
    /// UDP target and datagrams sent.
    pub udp: Option<(SocketAddr, u64)>,
    /// TCP server address and the players connected.
    pub tcp: Option<(SocketAddr, Vec<SocketAddr>)>,
    /// The last output error (a file that will not open, a port in use…).
    pub error: Option<String>,
    /// Text in the packets' payloads.
    pub text: decsat_modem::text::ByteTextView,
}

/// IP out of GSE, for display.
#[derive(Debug, Clone, Default)]
pub struct GseView {
    /// Where packets come from: the GSE variant in use, or the blind search.
    pub source: Option<Source>,
    /// Every variant's counts, for comparison.
    pub variants: Vec<VariantReport>,
    /// Non-IP PDUs by protocol type.
    pub other_protocols: BTreeMap<u16, u64>,
    pub packets: u64,
    pub bytes: u64,
    pub ipv4: u64,
    pub ipv6: u64,
    /// Packets by IP protocol number.
    pub protocols: BTreeMap<u8, u64>,
    /// The busiest flows by bytes.
    pub top: Vec<Flow>,
    pub flows: usize,
    /// IP bit rate over the last couple of seconds of signal.
    pub ip_bps: f64,
    /// The PCAP being written, or the last one, and its packet count.
    pub pcap: Option<(PathBuf, u64)>,
    pub pcap_active: bool,
    pub pcap_error: Option<String>,
    /// IP that came by MPE from a transport stream rather than GSE.
    pub mpe: Option<MpeStats>,
    /// IP that came in link-layer frames (HDLC), and how: for display.
    pub link: Option<String>,
    /// Multicast audio streams found, by address.
    pub audio: Vec<AudioStream>,
    /// Now-playing messages, the latest from each station.
    pub now_playing: Vec<NowPlaying>,
    /// IPv4 fragments seen, datagrams rebuilt from them, and datagrams
    /// given up (a piece lost).
    pub fragments: (u64, u64, u64),
    pub sap_packets: u64,
    /// Stations announced (SAP or bare SDP): group, port and what the SDP
    /// says — their names, whether or not their streams are seen.
    pub stations: Vec<(std::net::IpAddr, u16, decsat_ip::SdpInfo)>,
    /// The stream being played, and what to open in the player.
    pub audio_playing: Option<SocketAddr>,
    pub audio_target: Option<PlayTarget>,
    pub audio_error: Option<String>,
    /// The in-app player, while one is playing: status, level, pause.
    pub audio_app: Option<AudioHandle>,
    /// The stream being recorded, and the file (current or last) with
    /// its bytes so far.
    pub audio_recording: Option<SocketAddr>,
    pub audio_record_file: Option<(PathBuf, u64)>,
    pub audio_record_error: Option<String>,
    /// Packets passed to the player so far.
    pub audio_forwarded: u64,
    /// Text in the packets' payloads (UDP/TCP data).
    pub text: decsat_modem::text::ByteTextView,
}

/// Where and whether a VFO's FEC writes its output.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FecOutput {
    /// Write IP packets to a PCAP file.
    pub record: bool,
    /// A modem's data: record them, the last 10 s included, while a channel
    /// of the TDM multiplex inside is active (and 10 s after).
    pub record_on_activity: bool,
    pub dir: PathBuf,
    /// For file names.
    pub name: String,
    pub carrier_hz: f64,
    /// Read GSE this way instead of detecting it.
    pub gse_variant: Option<Variant>,
    /// The VFO's decoder is the MPEG-TS one: run the TS stage.
    pub ts: bool,
    /// Write the MPEG-TS to a `.ts` file.
    pub ts_record: bool,
    /// Send the MPEG-TS by UDP here.
    pub ts_udp: Option<SocketAddr>,
    /// Serve the MPEG-TS over TCP/HTTP here.
    pub ts_tcp: Option<SocketAddr>,
    /// Play this multicast audio stream (group:port): in the app, or
    /// relayed to an external player when `audio_external`.
    pub audio_play: Option<SocketAddr>,
    pub audio_external: bool,
    /// Record this multicast audio stream to a file in `dir`.
    pub audio_record: Option<SocketAddr>,
    /// An E1's timeslot to play, and one to record (to a `.wav` in `dir`).
    pub e1_play: Option<u8>,
    pub e1_record: Option<u8>,
    /// A TDM multiplex's call voice: play it, record it (to a `.wav`).
    pub tdm_play: bool,
    pub tdm_record: bool,
    /// Play an E1's G.728 voice as soon as it is found, when no timeslot
    /// is chosen (the CDM-600 voice preset).
    pub voice_auto: bool,
}

/// Runs a [`FecDecoder`] on its own thread.
pub(crate) struct FecWorker {
    tx: Option<SyncSender<FecInput>>,
    stats: Arc<Mutex<FecStats>>,
    output: Arc<Mutex<FecOutput>>,
    /// A frame was dropped since the thread last looked: streams that span
    /// frames (TS) must start over.
    gap: Arc<AtomicBool>,
    /// Set to stop without working through the queue.
    quit: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    lossless: bool,
}

impl FecWorker {
    /// `symbol_rate` turns frame lengths into signal time for the rates.
    pub fn spawn(symbol_rate: f64, output: FecOutput) -> Self {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let stats = Arc::new(Mutex::new(FecStats::default()));
        let output = Arc::new(Mutex::new(output));
        let gap = Arc::new(AtomicBool::new(false));
        let quit = Arc::new(AtomicBool::new(false));
        let (s, o, g, q) = (stats.clone(), output.clone(), gap.clone(), quit.clone());
        let join = std::thread::Builder::new()
            .name("decsat-fec".into())
            .spawn(move || run(rx, s, o, g, q, symbol_rate))
            .expect("spawn FEC thread");
        FecWorker {
            tx: Some(tx),
            stats,
            output,
            gap,
            quit,
            join: Some(join),
            lossless: false,
        }
    }

    /// Have the thread stop after what it is doing now, leaving the rest
    /// of its queue (a VFO deleted: nobody wants the rest).
    pub fn quit(&self) {
        self.quit.store(true, Ordering::Relaxed);
    }

    /// Wait for the thread instead of dropping input when it is behind (a
    /// file played as fast as possible).
    pub fn lossless(mut self, on: bool) -> Self {
        self.lossless = on;
        self
    }

    /// Queue a frame; drop it (counted) if the thread is behind.
    pub fn offer(&self, f: PlFrame) {
        self.send(FecInput::Frame(f));
    }

    /// Queue a block of DVB-S symbols.
    pub fn offer_dvbs(&self, symbols: Vec<Iq>) {
        self.send(FecInput::Dvbs(symbols));
    }

    /// Queue a block of TPC 2964 symbols.
    pub fn offer_tpc(&self, symbols: Vec<Iq>, qpsk: bool) {
        self.send(FecInput::Tpc { symbols, qpsk });
    }

    /// Queue a block of Q-Flex FastLink symbols.
    pub fn offer_fastlink(&self, symbols: Vec<Iq>) {
        self.send(FecInput::FastLink(symbols));
    }

    /// Queue a block of symbols of a K = 7 convolutional-code carrier.
    pub fn offer_viterbi(&self, symbols: Vec<Iq>) {
        self.send(FecInput::Viterbi(symbols));
    }

    fn send(&self, input: FecInput) {
        if self.lossless {
            if let Some(tx) = &self.tx {
                let _ = tx.send(input);
            }
            return;
        }
        if let Some(tx) = &self.tx
            && let Err(TrySendError::Full(_)) = tx.try_send(input)
        {
            self.stats.lock().unwrap().dropped += 1;
            self.gap.store(true, Ordering::Relaxed);
        }
    }

    /// Change what is written; takes effect from the next frame.
    pub fn set_output(&self, o: FecOutput) {
        *self.output.lock().unwrap() = o;
    }

    pub fn stats(&self) -> FecStats {
        self.stats.lock().unwrap().clone()
    }
}

impl Drop for FecWorker {
    fn drop(&mut self) {
        // Closing the queue ends the thread after its current frame.
        self.tx = None;
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// The GSE → IP → PCAP end of the FEC thread.
struct IpStage {
    gse: GseIp,
    stats: IpStats,
    packets: Vec<IpPacket>,
    pcap: Option<PcapWriter>,
    path: Option<PathBuf>,
    error: Option<String>,
    /// IP rate window: bytes and signal seconds.
    win: (f64, f64),
    ip_bps: f64,
    last_top: Instant,
    top: Vec<Flow>,
    mcast: McastScanner,
    audio: Vec<AudioStream>,
    relay: Option<AudioRelay>,
    player: Option<AudioPlayer>,
    /// Why the asked-for stream is not playing.
    relay_error: Option<String>,
    audio_external: bool,
    recorder: Option<AudioRecorder>,
    record_want: Option<SocketAddr>,
    record_error: Option<String>,
    /// The last recording's file and size, once it is closed.
    record_last: Option<(PathBuf, u64)>,
    /// The stream asked for (it may not have been heard yet).
    audio_want: Option<SocketAddr>,
    mpe: Option<MpeStats>,
    /// IP from HDLC frames: the frames and their FCS, for display.
    hdlc: Option<(u64, decsat_modem::payload::Fcs)>,
    /// Text in the payloads.
    text: decsat_modem::text::ByteText,
    /// IPv4 fragments put back together.
    frags: Reassembler,
}

impl IpStage {
    fn new() -> Self {
        IpStage {
            mcast: McastScanner::new(),
            audio: Vec::new(),
            relay: None,
            player: None,
            relay_error: None,
            audio_external: false,
            recorder: None,
            record_want: None,
            record_error: None,
            record_last: None,
            audio_want: None,
            mpe: None,
            hdlc: None,
            text: decsat_modem::text::ByteText::new(),
            frags: Reassembler::new(),
            gse: GseIp::new(),
            stats: IpStats::default(),
            packets: Vec::new(),
            pcap: None,
            path: None,
            error: None,
            win: (0.0, 0.0),
            ip_bps: 0.0,
            last_top: Instant::now(),
            top: Vec::new(),
        }
    }

    /// Open or close the PCAP to match `o.record` (a new file per
    /// recording), and start or stop playing audio to match `o.audio_play`.
    fn follow(&mut self, o: &FecOutput) {
        self.gse.forced = o.gse_variant;
        if o.audio_play != self.audio_want || o.audio_external != self.audio_external {
            self.audio_want = o.audio_play;
            self.audio_external = o.audio_external;
            self.relay = None;
            self.player = None;
            self.relay_error = None;
        }
        // Start playing once the stream has been seen enough to tell RTP
        // from raw and to know its codec: started early, it would be taken
        // apart the wrong way for good.
        if let Some(want) = self.audio_want
            && self.relay.is_none()
            && self.player.is_none()
            && self.relay_error.is_none()
            && let Some(st) = self.heard(want)
        {
            if self.audio_external {
                let dir = std::env::temp_dir().join("DecSAT");
                match AudioRelay::start(&st, &dir) {
                    Ok(r) => self.relay = Some(r),
                    Err(e) => self.relay_error = Some(format!("{}: {e}", st.name())),
                }
            } else {
                match AudioPlayer::start(&st, AUDIO_OUTPUT) {
                    Ok(p) => self.player = Some(p),
                    Err(e) => self.relay_error = Some(format!("{}: {e}", st.name())),
                }
            }
        }
        if o.audio_record != self.record_want {
            self.record_want = o.audio_record;
            self.stop_recording();
            self.record_error = None;
        }
        if let Some(want) = self.record_want
            && self.recorder.is_none()
            && self.record_error.is_none()
            && let Some(st) = self.heard(want)
        {
            let stamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let name: String = st
                .name()
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            let stem =
                format!("decsat-{name}-{}_{}-{stamp}", st.group, st.port).replace([':', '.'], "_");
            match AudioRecorder::start(&st, &o.dir, &stem) {
                Ok(r) => self.recorder = Some(r),
                Err(e) => self.record_error = Some(format!("{}: {e}", st.name())),
            }
        }
        match (o.record, self.pcap.is_some()) {
            (true, false) if self.error.is_none() => {
                let stamp = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let path = o.dir.join(format!(
                    "decsat-{}-{}-{stamp}.pcap",
                    o.name.replace(' ', "_"),
                    crate::rf::freq_tag(o.carrier_hz)
                ));
                let _ = std::fs::create_dir_all(&o.dir);
                match PcapWriter::create(&path) {
                    Ok(w) => self.pcap = Some(w),
                    Err(e) => self.error = Some(format!("{}: {e}", path.display())),
                }
                self.path = Some(path);
            }
            (false, true) => {
                if let Some(mut w) = self.pcap.take() {
                    let _ = w.flush();
                }
            }
            (false, false) => self.error = None,
            _ => {}
        }
    }

    /// One good GS-mode frame's data field.
    fn data_field(&mut self, field: &[u8]) {
        let mut packets = std::mem::take(&mut self.packets);
        packets.clear();
        self.gse.data_field(field, &mut packets);
        for p in &packets {
            self.ip(&p.data, &p.info);
        }
        self.packets = packets;
    }

    /// IP datagrams from MPE (a transport stream).
    fn mpe(&mut self, datagrams: &[(Vec<u8>, IpInfo)], stats: &MpeStats) {
        for (d, info) in datagrams {
            self.ip(d, info);
        }
        self.mpe = Some(stats.clone());
    }

    /// One IP packet, wherever it came from. The statistics and the PCAP
    /// take packets as sent (Wireshark reassembles for itself); what reads
    /// the payloads takes whole datagrams, fragments put back together.
    fn ip(&mut self, data: &[u8], info: &IpInfo) {
        self.stats.add(info);
        self.win.0 += data.len() as f64;
        if let Some(w) = &mut self.pcap
            && let Err(e) = w.write(SystemTime::now(), data)
        {
            self.error = Some(e.to_string());
            self.pcap = None;
        }
        // `Cow`: borrowed when the packet was whole, owned when rebuilt.
        let Some(whole) = self.frags.push(data, info) else {
            return;
        };
        let rebuilt;
        let (data, info) = match &whole {
            std::borrow::Cow::Borrowed(d) => (*d, info),
            std::borrow::Cow::Owned(d) => match decsat_ip::parse(d) {
                Some(i) => {
                    rebuilt = i;
                    (d.as_slice(), &rebuilt)
                }
                None => return,
            },
        };
        self.text.push_unit(ip_payload(data, info));
        self.mcast.packet(data, info);
        if (self.audio_want.is_some() || self.record_want.is_some())
            && let Some((payload, _, dport)) = udp_payload(data, info)
        {
            let key = Some(SocketAddr::new(info.dst, dport));
            if key == self.audio_want {
                if let Some(r) = &mut self.relay {
                    r.packet(payload);
                }
                if let Some(p) = &mut self.player {
                    p.packet(payload);
                }
            }
            if key == self.record_want
                && let Some(r) = &mut self.recorder
            {
                r.packet(payload);
            }
        }
    }

    /// Signal time passes (every frame, good or not).
    fn tick(&mut self, secs: f64) {
        self.mcast.tick(secs);
        self.win.1 += secs;
        if self.win.1 >= 2.0 || (self.ip_bps == 0.0 && self.win.1 > 0.2) {
            self.ip_bps = self.win.0 * 8.0 / self.win.1;
            if self.win.1 >= 2.0 {
                self.win = (0.0, 0.0);
            }
        }
    }

    fn view(&mut self) -> GseView {
        // Ranking flows sorts the table: twice a second is plenty.
        // Re-ranking sorts the flow table: twice a second, or at once while
        // new flows are still appearing in a short list.
        let new_flows = self.top.len() < 8 && self.stats.flow_count() != self.top.len();
        if self.last_top.elapsed().as_millis() >= 500 || new_flows {
            self.top = self.stats.top_flows(8);
            self.last_top = Instant::now();
        }
        // Few flows, cheap to list: always current.
        self.audio = self.mcast.streams();

        GseView {
            source: self.gse.source(),
            variants: self.gse.reports(),
            other_protocols: self.gse.other_protocols.clone(),
            packets: self.stats.packets,
            bytes: self.stats.bytes,
            ipv4: self.stats.ipv4,
            ipv6: self.stats.ipv6,
            protocols: self.stats.protocols.clone(),
            top: self.top.clone(),
            flows: self.stats.flow_count(),
            ip_bps: self.ip_bps,
            pcap: self
                .path
                .clone()
                .map(|p| (p, self.pcap.as_ref().map_or(0, |w| w.packets()))),
            pcap_active: self.pcap.is_some(),
            pcap_error: self.error.clone(),
            mpe: self.mpe.clone(),
            link: self.hdlc.map(|(n, fcs)| {
                let fcs = match fcs {
                    decsat_modem::payload::Fcs::Crc16 => "FCS-16",
                    decsat_modem::payload::Fcs::Crc32 => "FCS-32",
                };
                format!("HDLC ({fcs}) · {n} frames with IP")
            }),
            audio: self.audio.clone(),
            now_playing: self.mcast.now_playing(),
            fragments: (
                self.frags.fragments,
                self.frags.reassembled,
                self.frags.dropped,
            ),
            sap_packets: self.mcast.sap_packets,
            stations: self.mcast.stations(),
            audio_playing: self.audio_want,
            audio_target: self.relay.as_ref().map(|r| r.target.clone()),
            audio_error: self.relay_error.clone(),
            audio_forwarded: self
                .relay
                .as_ref()
                .map(|r| r.forwarded)
                .or(self.player.as_ref().map(|p| p.forwarded))
                .unwrap_or(0),
            audio_app: self.player.as_ref().map(|p| p.handle()),
            audio_recording: self.record_want,
            audio_record_file: match &self.recorder {
                Some(r) => r.path().map(|p| (p.to_path_buf(), r.bytes)),
                None => self.record_last.clone(),
            },
            audio_record_error: self
                .record_error
                .clone()
                .or_else(|| self.recorder.as_ref().and_then(|r| r.error.clone())),
            text: self.text.view(),
        }
    }

    /// The stream at `want`, once it has been heard enough to know how it
    /// is carried — for RFC 3640 AAC and Opus, once there is an SDP for it
    /// (announced, or made from its packets after a few seconds).
    fn heard(&self, want: SocketAddr) -> Option<AudioStream> {
        self.mcast.streams().into_iter().find(|a| {
            a.group == want.ip()
                && a.port == want.port()
                && a.packets >= 8
                && (a.sdp.is_some() || !matches!(a.codec, Codec::AacRfc3640 | Codec::Opus))
        })
    }

    /// Close the recording, keeping its name and size for display.
    fn stop_recording(&mut self) {
        if let Some(r) = self.recorder.take() {
            self.record_last = r.path().map(|p| (p.to_path_buf(), r.bytes));
        }
    }
}

impl Drop for IpStage {
    fn drop(&mut self) {
        if let Some(w) = &mut self.pcap {
            let _ = w.flush();
        }
    }
}

/// The MPEG-TS end of the FEC thread.
struct TsStage {
    deframer: TsDeframer,
    analyser: TsAnalyser,
    packets: Vec<[u8; TS_LEN]>,
    file: Option<TsFile>,
    file_path: Option<PathBuf>,
    udp: Option<UdpSink>,
    tcp: Option<TcpSink>,
    /// What the outputs were last set to, so they are rebuilt only on change.
    udp_want: Option<SocketAddr>,
    tcp_want: Option<SocketAddr>,
    error: Option<String>,
    win: (f64, f64),
    ts_bps: f64,
    issy: bool,
    /// The analyser's last report, rebuilt at most every 250 ms.
    report: TsReport,
    report_at: Option<Instant>,
    /// IP in MPE sections, and the datagrams from the last field.
    mpe: MpeExtractor,
    datagrams: Vec<(Vec<u8>, IpInfo)>,
    /// Packets that came whole (DVB-S) rather than through the deframer.
    direct: u64,
    /// Text in the payloads.
    text: decsat_modem::text::ByteText,
}

impl TsStage {
    fn new() -> Self {
        TsStage {
            deframer: TsDeframer::new(),
            analyser: TsAnalyser::new(),
            packets: Vec::new(),
            file: None,
            file_path: None,
            udp: None,
            tcp: None,
            udp_want: None,
            tcp_want: None,
            error: None,
            win: (0.0, 0.0),
            ts_bps: 0.0,
            issy: false,
            report: TsReport::default(),
            report_at: None,
            mpe: MpeExtractor::new(),
            datagrams: Vec::new(),
            direct: 0,
            text: decsat_modem::text::ByteText::new(),
        }
    }

    /// Open, close or move the outputs to match `o`.
    fn follow(&mut self, o: &FecOutput) {
        match (o.ts_record, self.file.is_some()) {
            (true, false) if self.file_path.is_none() || self.error.is_none() => {
                let stamp = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let path = o.dir.join(format!(
                    "decsat-{}-{}-{stamp}.ts",
                    o.name.replace(' ', "_"),
                    crate::rf::freq_tag(o.carrier_hz)
                ));
                let _ = std::fs::create_dir_all(&o.dir);
                match TsFile::create(&path) {
                    Ok(f) => self.file = Some(f),
                    Err(e) => self.error = Some(format!("{}: {e}", path.display())),
                }
                self.file_path = Some(path);
            }
            (false, true) => {
                if let Some(mut f) = self.file.take() {
                    let _ = f.flush();
                }
            }
            _ => {}
        }
        if o.ts_udp != self.udp_want {
            self.udp_want = o.ts_udp;
            self.udp = None;
            if let Some(a) = o.ts_udp {
                match UdpSink::new(a) {
                    Ok(u) => self.udp = Some(u),
                    Err(e) => self.error = Some(format!("UDP {a}: {e}")),
                }
            }
        }
        if o.ts_tcp != self.tcp_want {
            self.tcp_want = o.ts_tcp;
            self.tcp = None; // closes the old server and its clients
            if let Some(a) = o.ts_tcp {
                match TcpSink::bind(a) {
                    Ok(t) => self.tcp = Some(t),
                    Err(e) => self.error = Some(format!("TCP {a}: {e}")),
                }
            }
        }
    }

    fn data_field(&mut self, field: &[u8], h: &BbHeader) {
        self.issy |= h.issyi;
        self.packets.clear();
        self.deframer
            .data_field(field, h.syncd, h.npd, h.issyi, &mut self.packets);
        self.forward();
    }

    /// Whole packets (DVB-S).
    fn packets_in(&mut self, pkts: &[[u8; TS_LEN]]) {
        self.packets.clear();
        self.packets.extend_from_slice(pkts);
        self.direct += pkts.len() as u64;
        self.forward();
    }

    /// The packets just taken to the analyser, MPE and the outputs.
    fn forward(&mut self) {
        self.datagrams.clear();
        for p in &self.packets {
            self.analyser.packet(p);
            self.mpe.packet(p, &mut self.datagrams);
            // Text per PID, so a table's strings carry across packets.
            let pid = u16::from_be_bytes([p[1] & 0x1F, p[2]]);
            if pid != 0x1FFF
                && let Some(payload) = ts_payload(p)
            {
                self.text.push_keyed(pid as u32, payload);
            }
        }
        self.win.0 += (self.packets.len() * TS_LEN) as f64;
        if let Some(f) = &mut self.file
            && let Err(e) = f.write(&self.packets)
        {
            self.error = Some(e.to_string());
            self.file = None;
        }
        if let Some(u) = &mut self.udp {
            u.write(&self.packets);
        }
        if let Some(t) = &mut self.tcp {
            t.write(&self.packets);
        }
    }

    fn tick(&mut self, secs: f64) {
        self.analyser.tick(secs);
        self.win.1 += secs;
        if self.win.1 >= 2.0 || (self.ts_bps == 0.0 && self.win.1 > 0.2) {
            self.ts_bps = self.win.0 * 8.0 / self.win.1;
            if self.win.1 >= 2.0 {
                self.win = (0.0, 0.0);
            }
        }
    }

    fn view(&mut self) -> TsView {
        // Classifying every PID costs more than a frame is worth on a busy
        // multiplex: refresh the report four times a second.
        if self
            .report_at
            .is_none_or(|t| t.elapsed().as_millis() >= 250)
        {
            self.report = self.analyser.report();
            self.report_at = Some(Instant::now());
        }
        let d = &self.deframer.stats;
        let cc_errors = self.analyser.pids.values().map(|s| s.cc_errors).sum();
        TsView {
            packets: d.packets + self.direct,
            crc_errors: d.crc_errors,
            cc_errors,
            nulls_reinserted: d.nulls_reinserted,
            resyncs: d.resyncs,
            issy: self.issy,
            ts_bps: self.ts_bps,
            report: self.report.clone(),
            file: self
                .file_path
                .clone()
                .map(|p| (p, self.file.as_ref().map_or(0, |f| f.packets))),
            file_active: self.file.is_some(),
            udp: self.udp.as_ref().map(|u| (u.target, u.datagrams)),
            tcp: self.tcp.as_ref().map(|t| (t.addr, t.clients())),
            error: self.error.clone(),
            text: self.text.view(),
        }
    }
}

/// A TS packet's payload: after the 4-byte header and any adaptation field
/// (ISO/IEC 13818-1 §2.4.3.2–3); none if the packet carries none.
fn ts_payload(p: &[u8; TS_LEN]) -> Option<&[u8]> {
    if p[3] & 0x10 == 0 {
        return None;
    }
    let start = if p[3] & 0x20 != 0 {
        5 + p[4] as usize
    } else {
        4
    };
    p.get(start..)
}

/// An IP packet's payload: the UDP or TCP data, else what follows the IP
/// header.
fn ip_payload<'a>(data: &'a [u8], info: &IpInfo) -> &'a [u8] {
    let ip = match data.first().map(|b| b >> 4) {
        Some(4) => (data[0] & 0x0F) as usize * 4,
        Some(6) => 40,
        _ => 0,
    };
    let l4 = match info.protocol {
        17 => 8,
        6 => data.get(ip + 12).map_or(0, |b| (b >> 4) as usize * 4),
        _ => 0,
    };
    let end = info.len.min(data.len());
    data.get((ip + l4).min(end)..end).unwrap_or(&[])
}

impl Drop for TsStage {
    fn drop(&mut self) {
        if let Some(f) = &mut self.file {
            let _ = f.flush();
        }
    }
}

/// Data kept from before a recording started on activity: 10 s at the
/// Q-Flex's 136.5 kbit/s.
const PRE_TRIGGER: usize = 171_000;

/// A modem's data as they come, to a `.bin` file while recording.
#[derive(Default)]
struct RawFile {
    file: Option<std::io::BufWriter<std::fs::File>>,
    path: Option<PathBuf>,
    bytes: u64,
    error: Option<String>,
    /// The latest data while not recording, for a recording started by
    /// activity to begin before it.
    pre: std::collections::VecDeque<u8>,
    /// Recordings started by activity.
    triggered: u64,
}

impl RawFile {
    /// Record if asked, or (`active`) while the multiplex inside is busy.
    fn follow(&mut self, o: &FecOutput, what: &str, active: bool) {
        use std::io::Write;
        let on_activity = !o.record && o.record_on_activity && active;
        match (o.record || on_activity, self.file.is_some()) {
            (true, false) if self.error.is_none() => {
                let what = if on_activity {
                    format!("{what}-active")
                } else {
                    what.to_string()
                };
                let stamp = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let path = o.dir.join(format!(
                    "decsat-{}-{}-{what}-{stamp}.bin",
                    o.name.replace(' ', "_"),
                    crate::rf::freq_tag(o.carrier_hz)
                ));
                let _ = std::fs::create_dir_all(&o.dir);
                match std::fs::File::create(&path) {
                    Ok(f) => self.file = Some(std::io::BufWriter::new(f)),
                    Err(e) => self.error = Some(format!("{}: {e}", path.display())),
                }
                self.path = Some(path);
                self.bytes = 0;
                if on_activity {
                    self.triggered += 1;
                    // The moments before the trigger: speech starts before
                    // the meter calls it.
                    let pre: Vec<u8> = self.pre.drain(..).collect();
                    self.write(&pre);
                }
                self.pre.clear();
            }
            (false, true) => {
                if let Some(mut f) = self.file.take() {
                    let _ = f.flush();
                }
            }
            (false, false) => self.error = None,
            _ => {}
        }
    }

    fn write(&mut self, data: &[u8]) {
        use std::io::Write;
        if self.file.is_none() {
            self.pre.extend(data);
            let over = self.pre.len().saturating_sub(PRE_TRIGGER);
            self.pre.drain(..over);
            return;
        }
        if let Some(f) = &mut self.file {
            match f.write_all(data) {
                Ok(()) => self.bytes += data.len() as u64,
                Err(e) => {
                    self.error = Some(e.to_string());
                    self.file = None;
                }
            }
        }
    }
}

impl Drop for RawFile {
    fn drop(&mut self) {
        use std::io::Write;
        if let Some(f) = &mut self.file {
            let _ = f.flush();
        }
    }
}

/// The IP packet in an HDLC frame: after whatever header the link puts
/// first (Cisco HDLC's 4 bytes, PPP's 4 or fewer, an Ethernet header…),
/// found by trying each offset for a packet that validates and fills the
/// rest of the frame exactly — or, IPv4 only (its header has a checksum),
/// that fits in it.
fn hdlc_ip(frame: &[u8]) -> Option<(&[u8], IpInfo)> {
    const MAX_HEADER: usize = 24;
    let offsets = 0..=MAX_HEADER.min(frame.len());
    for off in offsets.clone() {
        if let Some(info) = decsat_ip::parse(&frame[off..])
            && info.len == frame.len() - off
        {
            return Some((&frame[off..], info));
        }
    }
    for off in offsets {
        if frame[off..].first().is_some_and(|b| b >> 4 == 4)
            && let Some(info) = decsat_ip::parse(&frame[off..])
        {
            return Some((&frame[off..off + info.len], info));
        }
    }
    None
}

fn run(
    rx: Receiver<FecInput>,
    stats: Arc<Mutex<FecStats>>,
    output: Arc<Mutex<FecOutput>>,
    gap: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
    symbol_rate: f64,
) {
    let mut dec = FecDecoder::new();
    let mut ip: Option<IpStage> = None;
    let mut ts: Option<TsStage> = None;
    // Payload rate window: DFL bits and signal seconds.
    let (mut win_bits, mut win_secs) = (0f64, 0f64);
    let mut busy = 0f64;
    let mut dvbs: Option<decsat_modem::dvbs::DvbsRx> = None;
    let mut dvbs_out = Vec::new();
    let mut tpc: Option<decsat_modem::tpc2964::TpcRx> = None;
    let mut fastlink: Option<decsat_modem::fastlink::FastLinkRx> = None;
    let mut tdm: Option<decsat_modem::tdm257::TdmRx> = None;
    let mut call_voice: Option<CallVoice> = None;
    let mut tdm_active_until = 0u64;
    let mut viterbi: Option<decsat_modem::dvbs::ViterbiRx> = None;
    let mut tpc_frames = Vec::new();
    let mut payload: Option<decsat_modem::payload::PayloadRx> = None;
    let mut payload_out = decsat_modem::payload::PayloadOut::default();
    let mut raw = RawFile::default();
    let mut tpc_text: Option<decsat_modem::text::TextFinder> = None;
    let mut e1: Option<E1Stage> = None;
    while let Ok(input) = rx.recv() {
        if quit.load(Ordering::Relaxed) {
            break;
        }
        let t0 = Instant::now();
        let f = match input {
            FecInput::Frame(f) => f,
            // `name @ (A | B)` binds the whole input whichever it is.
            modem @ (FecInput::Tpc { .. } | FecInput::FastLink(_) | FecInput::Viterbi(_)) => {
                // A coded modem's data blocks: TPC 2964 (frame sync and the
                // code's structure found blind, turbo decoding) or Q-Flex
                // FastLink (measured LDPC code); then, for either, the
                // payload's format and scrambling.
                use decsat_modem::payload::PayloadRx;
                tpc_frames.clear();
                // Data bits a block, the data rate, and the raw file's tag.
                let (secs, block, data_bps, tag) = match modem {
                    FecInput::Tpc { symbols, qpsk } => {
                        use decsat_modem::tpc2964::{DATA, FRAME, TpcRx};
                        let rx = tpc.get_or_insert_with(|| TpcRx::new(qpsk));
                        rx.push(&symbols, &mut tpc_frames);
                        let bps =
                            symbol_rate * if qpsk { 2.0 } else { 1.0 } * DATA as f64 / FRAME as f64;
                        (symbols.len() as f64 / symbol_rate, DATA, bps, "tpc2964")
                    }
                    FecInput::FastLink(symbols) => {
                        use decsat_modem::fastlink::{FRAME_DATA, FRAME_SYMBOLS, FastLinkRx};
                        let rx = fastlink.get_or_insert_with(FastLinkRx::new);
                        rx.push(&symbols, &mut tpc_frames);
                        let bps = symbol_rate * FRAME_DATA as f64 / FRAME_SYMBOLS as f64;
                        (
                            symbols.len() as f64 / symbol_rate,
                            FRAME_DATA,
                            bps,
                            "fastlink",
                        )
                    }
                    FecInput::Viterbi(symbols) => {
                        use decsat_modem::dvbs::{VITERBI_BLOCK, ViterbiRx};
                        let rx = viterbi.get_or_insert_with(ViterbiRx::new);
                        rx.push(&symbols, &mut tpc_frames);
                        let bps = 2.0 * symbol_rate * rx.stats.rate.map_or(0.5, |r| r.value());
                        (
                            symbols.len() as f64 / symbol_rate,
                            VITERBI_BLOCK,
                            bps,
                            "viterbi",
                        )
                    }
                    _ => unreachable!("matched above"),
                };
                // FastLink's data pairs are QPSK symbols, and one Q-Flex
                // sends its Q rail late: look for that too.
                let pay = payload.get_or_insert_with(|| match tag {
                    "fastlink" => PayloadRx::new(block).with_rail_skews(),
                    _ => PayloadRx::new(block),
                });
                let o = output.lock().unwrap().clone();
                // Activity in the TDM multiplex (any channel changing from
                // one 20 ms frame to the next), held 10 s (5000 frames).
                let tdm_busy = tdm.as_ref().is_some_and(|t| {
                    t.stats.locked
                        && t.stats
                            .channels
                            .iter()
                            .any(|c| c.state == decsat_modem::tdm257::ChannelState::Active)
                });
                let frames = tdm.as_ref().map_or(0, |t| t.stats.frames);
                if tdm_busy {
                    tdm_active_until = frames + 5000;
                }
                raw.follow(&o, tag, frames < tdm_active_until);
                for data in &tpc_frames {
                    payload_out.clear();
                    pay.push(data, &mut payload_out);
                    raw.write(&payload_out.raw);
                    // Unknown format: look for text in every reading of
                    // the bits (once known, the TS or IP stage looks).
                    // Unknown format, or framing whose contents are not
                    // known (Paradise's): look for text in the data.
                    let unread = if pay.format().is_none() {
                        Some(data.as_slice())
                    } else {
                        (!payload_out.inner.is_empty()).then_some(payload_out.inner.as_slice())
                    };
                    // Paradise's data: look for the 257-bit multiplex in it.
                    if matches!(
                        pay.format(),
                        Some(decsat_modem::payload::Format::ParadiseEsc)
                    ) && !payload_out.inner.is_empty()
                    {
                        let rx = tdm.get_or_insert_with(decsat_modem::tdm257::TdmRx::new);
                        rx.push(&payload_out.inner);
                        let cv = call_voice.get_or_insert_with(CallVoice::new);
                        cv.follow(&o, rx.stats.voice);
                        cv.feed(rx);
                    }
                    if let Some(bits) = unread {
                        tpc_text
                            .get_or_insert_with(|| {
                                decsat_modem::text::TextFinder::new(vec!["data".into()])
                            })
                            .push(0, bits);
                    }
                    if !payload_out.ts.is_empty() {
                        let stage = ts.get_or_insert_with(TsStage::new);
                        stage.follow(&o);
                        stage.packets_in(&payload_out.ts);
                        if !stage.datagrams.is_empty() || ip.is_some() {
                            let ipst = ip.get_or_insert_with(IpStage::new);
                            ipst.follow(&o);
                            ipst.mpe(&stage.datagrams, &stage.mpe.stats);
                        }
                    }
                    if !payload_out.e1.is_empty() {
                        let stage = e1.get_or_insert_with(E1Stage::new);
                        stage.follow(&o);
                        stage.frames(&payload_out.e1);
                    }
                    if !payload_out.slots.is_empty() {
                        let channels = pay.stats.slots.as_ref().map_or(0, |s| s.channels);
                        let stage = e1.get_or_insert_with(E1Stage::new);
                        stage.follow(&o);
                        stage.slot_packets(&payload_out.slots, channels);
                    }
                    if !payload_out.dandi.is_empty() {
                        // Timeslots carried: the data rate — the symbol rate
                        // through the code — is n × 64 kbit/s × 46/45 (the
                        // D&I++ overhead).
                        let n = (data_bps * 45.0 / 46.0 / 64e3).round().clamp(1.0, 31.0) as u8;
                        let stage = e1.get_or_insert_with(E1Stage::new);
                        stage.follow(&o);
                        stage.dandi_bytes(n, &payload_out.dandi);
                    }
                    for (frame, fcs) in &payload_out.frames {
                        if let Some((pkt, info)) = hdlc_ip(frame) {
                            let ipst = ip.get_or_insert_with(IpStage::new);
                            ipst.follow(&o);
                            let n = ipst.hdlc.map_or(0, |h| h.0);
                            ipst.hdlc = Some((n + 1, *fcs));
                            ipst.ip(pkt, &info);
                        }
                    }
                }
                if let Some(stage) = &mut ts {
                    stage.follow(&o);
                    stage.tick(secs);
                }
                if let Some(stage) = &mut ip {
                    stage.follow(&o);
                    stage.tick(secs);
                }
                let used = t0.elapsed().as_secs_f64();
                busy = 0.95 * busy + 0.05 * (used / secs.max(1e-9));
                win_secs += secs;
                win_bits += (tpc_frames.len() * block) as f64;
                let mut s = stats.lock().unwrap();
                s.load = busy as f32;
                if win_secs >= 2.0 {
                    s.payload_bps = win_bits / win_secs;
                    (win_bits, win_secs) = (0.0, 0.0);
                } else if s.payload_bps == 0.0 && win_secs > 0.2 {
                    s.payload_bps = win_bits / win_secs;
                }
                s.tpc = tpc.as_ref().map(|r| r.stats.clone());
                s.fastlink = fastlink.as_ref().map(|r| r.stats.clone());
                // Shown once it has locked at least once.
                s.tdm = tdm
                    .as_ref()
                    .filter(|t| t.stats.frames > 0)
                    .map(|t| t.stats.clone());
                s.call_voice = call_voice.as_ref().map(|c| c.view());
                s.viterbi = viterbi.as_ref().map(|r| r.stats.clone());
                s.payload = Some(pay.stats.clone());
                s.text = tpc_text.as_ref().map(|t| t.view());
                if let Some(stage) = &mut e1 {
                    stage.follow(&o);
                    let st = match (&pay.stats.dandi, &pay.stats.slots) {
                        (Some(d), _) => decsat_modem::e1::E1Stats {
                            locked: d.locked,
                            frames: d.frames,
                            fas_errors: d.bad_frames,
                            losses: d.losses,
                            cas: false,
                        },
                        (None, Some(p)) => decsat_modem::e1::E1Stats {
                            locked: p.slot_packets > 0,
                            frames: p.slot_packets,
                            ..Default::default()
                        },
                        _ => pay.stats.e1.clone().unwrap_or_default(),
                    };
                    let mut view = stage.view(st);
                    view.packets = pay.stats.slots.as_ref().map(|p| (p.slot_packets, p.lost));
                    s.e1 = Some(view);
                }
                s.raw_file = raw.path.clone().map(|p| (p, raw.bytes));
                s.raw_triggered = raw.triggered;
                s.raw_active = raw.file.is_some();
                if let Some(stage) = &mut ts {
                    s.ts = Some(stage.view());
                }
                if let Some(stage) = &mut ip {
                    s.gse = Some(stage.view());
                }
                continue;
            }
            FecInput::Dvbs(sym) => {
                // DVB-S: blind Viterbi/RS decoding, then the TS stage as for
                // a DVB-S2 TS carrier (and IP from MPE).
                let secs = sym.len() as f64 / symbol_rate;
                let rx = dvbs.get_or_insert_with(decsat_modem::dvbs::DvbsRx::new);
                dvbs_out.clear();
                rx.push(&sym, &mut dvbs_out);
                let stage = ts.get_or_insert_with(TsStage::new);
                stage.follow(&output.lock().unwrap());
                stage.packets_in(&dvbs_out);
                stage.tick(secs);
                if !stage.datagrams.is_empty() || ip.is_some() {
                    let ipst = ip.get_or_insert_with(IpStage::new);
                    ipst.follow(&output.lock().unwrap());
                    ipst.mpe(&stage.datagrams, &stage.mpe.stats);
                    ipst.tick(secs);
                }
                let used = t0.elapsed().as_secs_f64();
                busy = 0.95 * busy + 0.05 * (used / secs.max(1e-9));
                win_secs += secs;
                win_bits += (dvbs_out.len() * TS_LEN * 8) as f64;
                let mut s = stats.lock().unwrap();
                s.load = busy as f32;
                if win_secs >= 2.0 {
                    s.payload_bps = win_bits / win_secs;
                    (win_bits, win_secs) = (0.0, 0.0);
                } else if s.payload_bps == 0.0 && win_secs > 0.2 {
                    s.payload_bps = win_bits / win_secs;
                }
                s.dvbs = Some(rx.stats.clone());
                s.ts = Some(stage.view());
                if let Some(stage) = &mut ip {
                    s.gse = Some(stage.view());
                }
                continue;
            }
        };
        let secs = f.pls.plframe_len as f64 / symbol_rate;
        let out = dec.decode(&f);

        // A stream reassembled across frames breaks at any frame missing in
        // between: one the demodulator never saw (lock regained), one dropped
        // from the queue, or one that would not decode.
        let broken = f.after_gap
            || gap.swap(false, Ordering::Relaxed)
            || out.as_ref().is_some_and(|b| !b.ok());
        if broken && let Some(t) = &mut ts {
            t.deframer.discontinuity();
        }
        let wants_ts = output.lock().unwrap().ts;
        if let Some(b) = &out
            && let Ok(h) = &b.header
            && b.ok()
            && h.format == StreamFormat::Transport
            && wants_ts
        {
            let end = (BBHEADER_LEN + h.dfl as usize / 8).min(b.bytes.len());
            let stage = ts.get_or_insert_with(TsStage::new);
            stage.follow(&output.lock().unwrap());
            stage.data_field(&b.bytes[BBHEADER_LEN..end], h);
            // IP by MPE joins the IP stage (statistics, multicast audio).
            if !stage.datagrams.is_empty() || ip.is_some() {
                let ipst = ip.get_or_insert_with(IpStage::new);
                ipst.follow(&output.lock().unwrap());
                ipst.mpe(&stage.datagrams, &stage.mpe.stats);
            }
        } else if let Some(t) = &mut ts {
            // Keep Record/UDP/TCP responsive between TS frames.
            t.follow(&output.lock().unwrap());
        }
        if let Some(t) = &mut ts {
            t.tick(secs);
        }

        // GSE/IP on good generic-stream frames (UPL 0: continuous; GSE also
        // turns up flagged as packetized with UPL 0).
        let gs = out.as_ref().and_then(|b| match &b.header {
            Ok(h) if b.ok() && h.format != StreamFormat::Transport && h.upl == 0 => {
                let end = (BBHEADER_LEN + h.dfl as usize / 8).min(b.bytes.len());
                Some(&b.bytes[BBHEADER_LEN..end])
            }
            _ => None,
        });
        if let Some(field) = gs {
            let stage = ip.get_or_insert_with(IpStage::new);
            stage.follow(&output.lock().unwrap());
            stage.data_field(field);
        }
        if let Some(stage) = &mut ip {
            stage.tick(secs);
            if gs.is_none() {
                // Keep Record/Stop responsive between GS frames.
                stage.follow(&output.lock().unwrap());
            }
        }

        let used = t0.elapsed().as_secs_f64();
        busy = 0.95 * busy + 0.05 * (used / secs.max(1e-9));

        let mut s = stats.lock().unwrap();
        s.load = busy as f32;
        win_secs += secs;
        if let Some(b) = &out {
            s.frames += 1;
            s.iterations += b.ldpc.iterations as u64;
            s.ldpc_unconverged += !b.ldpc.converged as u64;
            s.es_n0_db = Some(b.es_n0_db);
            let entry = s.per_modcod.entry(b.pls.modcod).or_default();
            entry.1 += 1;
            match (&b.bch, &b.header) {
                (Err(_), _) => s.bch_failed += 1,
                (Ok(_), Err(_)) => s.crc_failed += 1,
                (Ok(c), Ok(h)) => {
                    s.ok += 1;
                    s.bch_corrected += *c as u64;
                    s.last_header = Some(*h);
                    *s.streams
                        .entry(if h.single_stream { 0 } else { h.isi })
                        .or_default() += 1;
                    s.per_modcod.entry(b.pls.modcod).or_default().0 += 1;
                    win_bits += h.dfl as f64;
                    if h.format == StreamFormat::Transport {
                        s.ts_frames += 1;
                    }
                }
            }
        }
        if win_secs >= 2.0 {
            s.payload_bps = win_bits / win_secs;
            (win_bits, win_secs) = (0.0, 0.0);
        } else if s.payload_bps == 0.0 && win_secs > 0.2 {
            // A first figure quickly, refined once the window fills.
            s.payload_bps = win_bits / win_secs;
        }
        if let Some(stage) = &mut ip {
            s.gse = Some(stage.view());
        }
        if let Some(t) = &mut ts {
            s.ts = Some(t.view());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demod::Demod;
    use decsat_frame::StreamFormat;
    use decsat_mod::{FrameSpec, PlFramer, Shaper, TsBbFramer};
    use std::f64::consts::TAU;

    /// A D&I timeslot carrying G.728 (as the CDM-600L's: bits 2–3 of each
    /// octet, codewords inverted, a sync bit 0 once in 32) plays by itself
    /// under the CDM-600 voice preset, and not otherwise.
    /// A Q-Flex call plays by itself once it has spoken, under the voice
    /// preset; ▶ plays it regardless; with neither it stays quiet.
    #[test]
    fn a_qflex_call_plays_by_itself_once_spoken() {
        use decsat_modem::tdm257::VoiceView;
        let quiet = VoiceView {
            channels: 0b1000_0000_0000_0111,
            talking: false,
            talk_blocks: 0,
        };
        let spoken = VoiceView {
            talk_blocks: 12,
            ..quiet
        };
        let out = |auto: bool, play: bool| FecOutput {
            voice_auto: auto,
            tdm_play: play,
            ..FecOutput::default()
        };
        for (o, voice, playing, auto) in [
            (out(true, false), None, false, false),
            (out(true, false), Some(quiet), false, false),
            (out(true, false), Some(spoken), true, true),
            (out(false, false), Some(spoken), false, false),
            (out(false, true), Some(quiet), true, false),
            (out(true, true), Some(spoken), true, false),
        ] {
            let mut cv = CallVoice::new();
            cv.follow(&o, voice);
            let v = cv.view();
            assert_eq!((v.playing, v.auto), (playing, auto), "{voice:?}");
        }
    }

    /// A deleted VFO's FEC thread leaves its queue: told to quit, it
    /// handles nothing more; otherwise it works through what was queued.
    #[test]
    fn a_quit_fec_thread_leaves_its_queue() {
        let mut x = 7u32;
        let block: Vec<Iq> = (0..4_000)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                Iq::new(
                    (x >> 16) as f32 / 65_536.0 - 0.5,
                    (x >> 8 & 0xFF) as f32 / 256.0 - 0.5,
                )
            })
            .collect();
        for quit in [false, true] {
            let w = FecWorker::spawn(100e3, FecOutput::default());
            if quit {
                w.quit();
            }
            for _ in 0..8 {
                w.offer_tpc(block.clone(), true);
            }
            let stats = Arc::clone(&w.stats);
            drop(w);
            let s = stats.lock().unwrap();
            assert_eq!(s.tpc.is_some(), !quit, "quit {quit}");
        }
    }

    /// A multi-radio site's timeslots in HDLC packets: speech on one
    /// channel, the G.728 silence fill on the other. Both are found as
    /// G.728, and the preset plays them mixed.
    #[test]
    fn packet_timeslots_carry_g728_on_each_channel() {
        let octets = |words: &[u16], invert: bool| -> Vec<u8> {
            let bits: Vec<u8> = words
                .iter()
                .flat_map(|&w| {
                    (0..10)
                        .rev()
                        .map(move |k| u8::from(w >> k & 1 == 1) ^ u8::from(invert))
                })
                .collect();
            bits.chunks(2)
                .map(|b| 0x81 | b[0] << 6 | b[1] << 5)
                .collect()
        };
        let mut x = 0x1357_9BDFu32;
        let speech: Vec<u16> = (0..2400)
            .map(|i| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (x >> 16) as u16 & 0x1FF | u16::from(i % 32 != 31) << 9
            })
            .collect();
        let fill: Vec<u16> = (0..2400).map(|i| (i % 16) as u16).collect();
        let (one, two) = (octets(&speech, true), octets(&fill, false));
        let o = FecOutput {
            voice_auto: true,
            ..FecOutput::default()
        };
        let mut st = E1Stage::new();
        // Two packets of one channel, then two of the other, 44 octets each.
        let mut k = 0;
        while (k + 2) * 44 <= one.len() {
            let pk: Vec<(u8, Vec<u8>)> = (0..2)
                .map(|j| (1, one[(k + j) * 44..(k + j + 1) * 44].to_vec()))
                .chain((0..2).map(|j| (2, two[(k + j) * 44..(k + j + 1) * 44].to_vec())))
                .collect();
            st.follow(&o);
            st.slot_packets(&pk, 0b110);
            k += 2;
        }
        st.follow(&o);
        assert!(
            st.lane(1).is_some_and(|l| l.framer.talk_blocks > 0),
            "speech on 1"
        );
        assert!(
            st.lane(2).is_some_and(|l| l.framer.fill_blocks > 0),
            "fill on 2"
        );
        let v = st.view(Default::default());
        assert_eq!(v.channels, Some(2));
        assert_eq!(v.voice[2], Some((2, false)));
        assert_eq!(v.playing, Some(MIX));
    }

    #[test]
    fn found_g728_speech_plays_by_itself() {
        let mut x = 0x2468_ACE1u32;
        let mut bits = Vec::new();
        for i in 0..2400 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let cw = (x >> 16) as u16 & 0x1FF | u16::from(i % 32 != 31) << 9;
            bits.extend((0..10).rev().map(|k| u8::from(cw >> k & 1 == 0)));
        }
        let frames: Vec<[u8; decsat_modem::e1::TIMESLOTS]> = bits
            .chunks(2)
            .map(|b| {
                let mut f = [0xD5; decsat_modem::e1::TIMESLOTS];
                f[1] = 0x81 | b[0] << 6 | b[1] << 5;
                f
            })
            .collect();
        for auto in [true, false] {
            let o = FecOutput {
                voice_auto: auto,
                ..FecOutput::default()
            };
            let mut st = E1Stage::new();
            for chunk in frames.chunks(400) {
                st.follow(&o);
                st.frames(chunk);
            }
            st.follow(&o);
            assert!(st.is_voice(1), "G.728 found in timeslot 1");
            assert_eq!(st.view(Default::default()).playing, auto.then_some(MIX));
        }
    }

    #[test]
    fn ip_stage_reassembles_fragmented_rtp_aac() {
        use decsat_ip::frag::fragment;
        use decsat_ip::mcast::rtp_packet;
        use decsat_ip::packet::udp_v4;
        // As a radio multiplex sends it over MPE: RFC 3640 AAC, four 2048-
        // tick AUs a packet (HE-AAC at 44.1 kHz), each 2.2 kB RTP packet in
        // two IPv4 fragments, RTCP sender reports alongside, no SAP.
        let mut st = IpStage::new();
        let au = |n: usize| {
            let mut p = ((n * 16) as u16).to_be_bytes().to_vec();
            for _ in 0..n {
                p.extend_from_slice(&(540u16 << 3).to_be_bytes());
            }
            for _ in 0..n {
                p.push(0x21); // a channel pair element
                p.extend_from_slice(&[0x5A; 539]);
            }
            p
        };
        let src = [192, 168, 1, 89];
        let group = [225, 0, 0, 20];
        for k in 0..60u32 {
            let rtp = rtp_packet(96, k as u16, k * 8192, 0x48AB, &au(4));
            let mut d = udp_v4(src, group, 6020, 6020, &rtp);
            d[4..6].copy_from_slice(&(k as u16).to_be_bytes()); // IP id
            let sum = decsat_ip::packet::checksum(&{
                let mut h = d[..20].to_vec();
                h[10..12].copy_from_slice(&[0, 0]);
                h
            });
            d[10..12].copy_from_slice(&sum.to_be_bytes());
            let f = fragment(&d, 1500);
            assert_eq!(f.len(), 2);
            for p in &f {
                st.ip(p, &decsat_ip::parse(p).unwrap());
            }
            if k % 25 == 0 {
                let t = k as f64 * 8192.0 / 44100.0;
                let mut sr = vec![0x80, 200, 0, 6, 0, 0, 0x48, 0xAB];
                sr.extend_from_slice(&((3e9 + t) as u32).to_be_bytes());
                sr.extend_from_slice(&((t.fract() * 4_294_967_296.0) as u32).to_be_bytes());
                sr.extend_from_slice(&(k * 8192).to_be_bytes());
                sr.extend_from_slice(&[0; 8]);
                let p = udp_v4(src, group, 6021, 6021, &sr);
                st.ip(&p, &decsat_ip::parse(&p).unwrap());
            }
            st.tick(8192.0 / 44100.0);
        }
        let v = st.view();
        assert_eq!(v.fragments, (120, 60, 0));
        assert_eq!(
            v.packets,
            120 + 3,
            "link statistics count fragments as sent"
        );
        assert_eq!(v.audio.len(), 1);
        let a = &v.audio[0];
        assert_eq!(a.codec, Codec::AacRfc3640);
        let sdp = a.sdp.as_ref().expect("described from the packets");
        assert!(sdp.fmtp.as_deref().unwrap().contains("config=2b920800"));
        // Heard enough to play.
        assert!(st.heard(SocketAddr::new(a.group, a.port)).is_some());
    }

    /// A real DVB-S2 signal: `schedule` frames, shaped at 4 samples per
    /// symbol, offset by `cycles` per symbol, at `esn0_db`.
    fn signal(
        schedule: &[FrameSpec],
        n_sym: usize,
        cycles: f64,
        esn0_db: f64,
        seed: u64,
    ) -> Vec<Iq> {
        signal_gold(schedule, n_sym, cycles, esn0_db, seed, 0)
    }

    fn signal_gold(
        schedule: &[FrameSpec],
        n_sym: usize,
        cycles: f64,
        esn0_db: f64,
        seed: u64,
        gold: u32,
    ) -> Vec<Iq> {
        let mut framer = PlFramer::new(gold, seed);
        let syms = framer.build_schedule(schedule, n_sym);
        let mut sh = Shaper::new(4, 0.25, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let mut s = seed | 1;
        let mut uniform = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
        };
        // The shaper's output has unit power per *sample*, so a symbol's
        // energy after the matched filter is 4× that: N0 per sample is
        // 4 · 10^(−Es/N0 / 10). (The receiver's estimate caught this when the
        // test first had it per symbol: it read 8.6 dB for "2.5".)
        let sigma = (4.0 * 10f64.powf(-esn0_db / 10.0)).sqrt() * std::f64::consts::FRAC_1_SQRT_2;
        x.iter()
            .enumerate()
            .map(|(n, &v)| {
                let ph = TAU * cycles * n as f64 / 4.0 + 0.9;
                let r = (-2.0 * uniform().max(1e-300).ln()).sqrt() * sigma;
                let t = TAU * uniform();
                v * Iq::new(ph.cos() as f32, ph.sin() as f32)
                    + Iq::new((r * t.cos()) as f32, (r * t.sin()) as f32)
            })
            .collect()
    }

    /// The BBFRAMEs a `PlFramer` with `seed` sends for `schedule`, in order,
    /// descrambled.
    fn expected(schedule: &[FrameSpec], frames: usize, seed: u64) -> Vec<Vec<u8>> {
        let mut ts = TsBbFramer::new(seed ^ 0x7E57);
        ts.ccm = schedule.iter().filter(|s| s.modcod != 0).all(|s| {
            (s.modcod, s.short_fecframe) == (schedule[0].modcod, schedule[0].short_fecframe)
        });
        schedule
            .iter()
            .cycle()
            .take(frames)
            // Frames with nothing to encode (dummy, reserved) take no BBFRAME.
            .filter_map(|s| {
                if let Some(k) = s.vlsnr {
                    // Whole bytes from the source; the receiver's ragged
                    // last byte is compared only that far.
                    let code = VlsnrCode::for_header(k)?;
                    let mut f = ts.next_frame(code.k_bch / 8);
                    bb_scramble(&mut f);
                    return Some(f);
                }
                let mc = s.info().modcod()?;
                let p = FecParams::new(mc.frame, mc.rate)?;
                let mut f = ts.next_frame(p.bbframe_bytes());
                bb_scramble(&mut f);
                Some(f)
            })
            .collect()
    }

    fn decode_all(x: &[Iq]) -> Vec<BbFrame> {
        let mut d = Demod::new(4.0, 1.0, 0.25, 0);
        let mut fec = FecDecoder::new();
        let mut frames = Vec::new();
        let mut out = Vec::new();
        for chunk in x.chunks(50_000) {
            frames.clear();
            d.process(chunk, &mut frames);
            out.extend(frames.iter().filter_map(|f| fec.decode(f)));
        }
        out
    }

    #[test]
    fn decodes_an_acm_carrier_to_the_bbframes_that_were_sent() {
        // QPSK 1/2, 8PSK 3/5 (the odd interleaver), short 16APSK 2/3, a dummy
        // and short 32APSK 3/4, 0.2 % off frequency, at 18 dB.
        let schedule = [
            FrameSpec::new(4, false, true),
            FrameSpec::new(12, false, true),
            FrameSpec::new(18, true, false),
            FrameSpec::new(0, false, false),
            FrameSpec::new(24, true, true),
        ];
        let x = signal(&schedule, 400_000, 0.002, 18.0, 21);
        let got = decode_all(&x);
        let want = expected(&schedule, 60, 21);
        assert!(got.len() >= 6, "only {} frames", got.len());
        // The first decoded frame is some frame of the sequence; every one
        // after must follow it exactly.
        let first = want
            .iter()
            .position(|w| *w == got[0].bytes)
            .expect("first frame not in the sent sequence");
        for (k, b) in got.iter().enumerate() {
            assert!(b.ok(), "frame {k}: {:?} {:?} {:?}", b.ldpc, b.bch, b.header);
            assert_eq!(
                b.bytes,
                want[first + k],
                "frame {k} (MODCOD {})",
                b.pls.modcod
            );
            let h = b.header.unwrap();
            assert_eq!(h.format, StreamFormat::Transport);
            assert!(!h.ccm, "an ACM schedule must say so");
        }
    }

    #[test]
    fn decodes_an_s2x_acm_carrier() {
        // One frame of each S2X constellation family, in ACM with S2 QPSK
        // 1/2 and a reserved code (kept lock through, nothing decoded), at
        // 30 dB for 256APSK's sake.
        let schedule = [
            FrameSpec::s2x(132, true), // QPSK 13/45
            FrameSpec::s2x(144, true), // 8PSK 25/36, interleaver "102"
            FrameSpec::s2x(138, true), // 2+4+2APSK 5/9-L
            FrameSpec::s2x(154, true), // 4+12APSK 26/45, "3201"
            FrameSpec::s2x(158, true), // 8+8APSK 3/5-L, points by table
            FrameSpec::s2x(178, true), // 4+8+4+16APSK 32/45
            FrameSpec::s2x(250, true), // reserved, 8-ary
            FrameSpec::s2x(186, true), // 4+12+20+28APSK 11/15
            FrameSpec::s2x(200, true), // 128APSK 3/4: 103 slots
            FrameSpec::s2x(214, true), // 256APSK 3/4
            FrameSpec::s2x(240, true), // short 4+12APSK 26/45, "2130"
            FrameSpec::new(4, false, true),
        ];
        let x = signal(&schedule, 600_000, 0.001, 30.0, 24);
        let got = decode_all(&x);
        let want = expected(&schedule, 80, 24);
        assert!(got.len() >= 12, "only {} frames", got.len());
        let first = want
            .iter()
            .position(|w| *w == got[0].bytes)
            .expect("first frame not in the sent sequence");
        let mut seen = std::collections::BTreeSet::new();
        for (k, b) in got.iter().enumerate() {
            let name = b.pls.modcod().map(|m| m.to_string()).unwrap_or_default();
            assert!(b.ok(), "frame {k} ({name}): {:?} {:?}", b.ldpc, b.bch);
            assert_eq!(b.bytes, want[first + k], "frame {k} ({name})");
            seen.insert(b.pls.modcod);
        }
        assert!(seen.len() >= 10, "MODCODs decoded: {seen:?}");
    }

    #[test]
    fn decodes_every_vlsnr_modcod() {
        // Set 1 (QPSK 2/9, pi/2-BPSK 1/5, 11/45, 1/3 medium, 1/5 and 11/45
        // SF2 short) and set 2 (pi/2-BPSK 1/5, 4/15, 1/3 short, and a dummy),
        // between S2 QPSK 1/2 frames, at 8 dB.
        let mut schedule = vec![FrameSpec::new(4, false, true)];
        for k in [0u8, 1, 2, 3, 4, 5, 9, 10, 11, 12] {
            schedule.push(FrameSpec::vlsnr(k));
        }
        let x = signal(&schedule, 600_000, 0.001, 8.0, 25);
        let got = decode_all(&x);
        let want = expected(&schedule, 40, 25);
        assert!(got.len() >= 10, "only {} frames", got.len());
        let first = want
            .iter()
            .position(|w| got[0].bytes.starts_with(w))
            .expect("first frame not in the sent sequence");
        let mut kinds = std::collections::BTreeSet::new();
        for (k, b) in got.iter().enumerate() {
            assert!(
                b.ok(),
                "frame {k} (PLS {}): {:?} {:?}",
                b.pls.plsc,
                b.ldpc,
                b.bch
            );
            let w = &want[first + k];
            assert_eq!(
                &b.bytes[..w.len()],
                &w[..],
                "frame {k} (PLS {})",
                b.pls.plsc
            );
            kinds.insert(b.bytes.len());
        }
        // Nine codes, nine BBFRAME sizes.
        assert!(kinds.len() >= 8, "sizes {kinds:?}");
    }

    /// How low VL-SNR goes with this receiver (run by hand:
    /// `cargo test -p decsat-engine --release vlsnr_snr_sweep -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn vlsnr_snr_sweep() {
        for snr in [4.0, 2.0, 0.0, -2.0, -4.0] {
            let schedule = [
                FrameSpec::vlsnr(1),
                FrameSpec::vlsnr(4),
                FrameSpec::vlsnr(9),
            ];
            let x = signal(&schedule, 400_000, 0.001, snr, 26);
            let got = decode_all(&x);
            let ok = got.iter().filter(|b| b.ok()).count();
            println!("{snr:+.0} dB: {ok} of {} frames good", got.len());
        }
    }

    /// Frames good against Table 20a's ideal Es/N0 for a few S2X MODCODs —
    /// the implementation loss (run by hand: `cargo test -p decsat-engine
    /// --release s2x_threshold_sweep -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn s2x_threshold_sweep() {
        for (spec, ideal) in [
            (FrameSpec::s2x(132, true), -2.03),
            (FrameSpec::s2x(144, true), 7.02),
            (FrameSpec::s2x(154, true), 7.51),
            (FrameSpec::s2x(178, true), 11.75),
            (FrameSpec::s2x(186, true), 14.81),
            (FrameSpec::s2x(214, true), 19.57),
        ] {
            let name = spec.info().modcod().unwrap().to_string();
            for over in [1.0, 2.0, 3.0] {
                let x = signal(&[spec], 300_000, 0.001, ideal + over, 29);
                let got = decode_all(&x);
                let ok = got.iter().filter(|b| b.ok()).count();
                println!(
                    "{name:16} ideal {ideal:+.2} dB, +{over}: {ok}/{} good",
                    got.len()
                );
            }
        }
    }

    #[test]
    fn finds_a_preferred_scrambling_sequence() {
        // Scrambled with Table 19e's sequence 3 (gold code 3·10 949); the
        // demodulator is told 0 and must find it from the pilots.
        let schedule = [FrameSpec::s2x(154, true), FrameSpec::new(4, false, true)];
        let x = signal_gold(&schedule, 300_000, 0.001, 12.0, 27, 32_847);
        let mut d = Demod::new(4.0, 1.0, 0.25, 0);
        let mut fec = FecDecoder::new();
        let mut frames = Vec::new();
        let mut got = Vec::new();
        for chunk in x.chunks(50_000) {
            frames.clear();
            d.process(chunk, &mut frames);
            got.extend(frames.iter().filter_map(|f| fec.decode(f)));
        }
        assert_eq!(d.gold_code(), 32_847);
        assert!(got.len() >= 5, "only {} frames", got.len());
        for (k, b) in got.iter().enumerate() {
            assert!(
                b.ok(),
                "frame {k} of {} (PLS {}): {:?} {:?} {:?}",
                got.len(),
                b.pls.plsc,
                b.ldpc,
                b.bch,
                b.header
            );
        }
    }

    #[test]
    fn decodes_qpsk_near_its_threshold() {
        // QPSK 1/2 needs ~1 dB Es/N0 in theory. At 2 dB every frame must
        // come through — carrier and timing recovery, coherent header reads,
        // LLRs, LDPC, BCH — and the known-symbol Es/N0 must read true.
        // (Before the coherent header reads, frames failed from 6 dB down:
        // a QPSK 1/2 header read differentially as 3/5.)
        let schedule = [FrameSpec::new(4, false, true)];
        let x = signal(&schedule, 230_000, 0.001, 2.0, 22);
        let got = decode_all(&x);
        assert!(got.len() >= 5, "only {} frames", got.len());
        for b in &got {
            assert!(
                b.ok(),
                "{:?} {:?} {:?} at {:.1} dB",
                b.ldpc,
                b.bch,
                b.header,
                b.es_n0_db
            );
            assert!(
                (b.es_n0_db - 2.0).abs() < 0.5,
                "Es/N0 estimate {:.2}",
                b.es_n0_db
            );
        }
    }

    #[test]
    fn decodes_without_pilots_at_moderate_snr() {
        // No pilots: only the headers, 32 490 symbols apart, anchor the
        // phase; the decision-directed loop must hold in between. At 5 dB it
        // does; at 4 dB about one frame in seven is lost to a cycle slip —
        // which is what DVB-S2's pilots are for.
        let schedule = [FrameSpec::new(4, false, false)];
        let x = signal(&schedule, 230_000, 0.001, 5.0, 23);
        let got = decode_all(&x);
        assert!(got.len() >= 5, "only {} frames", got.len());
        let ok = got.iter().filter(|b| b.ok()).count();
        assert_eq!(ok, got.len(), "{ok} of {} frames", got.len());
    }
}
