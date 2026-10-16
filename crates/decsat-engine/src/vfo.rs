// made by CasualArclamp
//! VFOs: a centre, a bandwidth and a decoder, each on its own worker thread.
//!
//! The wideband front end hands every VFO the same blocks (shared through an
//! `Arc`, so fanning out costs one pointer per VFO, not a copy). A worker that
//! falls behind has blocks dropped and counted rather than stalling the front
//! end — the waterfall and the other VFOs keep running in real time.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use decsat_core::{Iq, SampleFormat};
use decsat_dsp::Ddc;
use decsat_io::IqFileWriter;

use crate::demod::{Demod, LockState};
use crate::fec::{FecOutput, FecStats, FecWorker};
use crate::identify::{Identification, Verdict, identify_in};
use crate::psk::PskDemod;
use crate::spectrum::Spectrum;

pub type VfoId = u32;

/// What a VFO does with its signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecoderKind {
    /// "What is this?" — blind symbol rate, roll-off, constellation, standard.
    Identify,
    /// DVB-S2/S2X → GSE → IP (PCAP).
    Dvbs2Ip,
    /// DVB-S2/S2X → MPEG-TS.
    Dvbs2Ts,
    /// DVB-S (EN 300 421) → MPEG-TS: QPSK, Viterbi, Reed–Solomon.
    DvbsTs,
    /// Intelsat IESS-315 turbo product code `tpc_2964` (BPSK/QPSK): frame
    /// structure, scrambling and payload (HDLC/IP or MPEG-TS) found blind.
    Tpc2964,
    /// A Comtech CDM modem, its type found: the chain picked from Identify's
    /// first look (TPC 2964's unique word, else a K = 7 code; a FastLink if
    /// that is what it is), the next tried if that one has not locked its
    /// code in [`AUTO_LOCK_SECS`]; the model then read off what the payload
    /// turns out to be ([`VfoStatus::cdm_model`]). Voice plays when G.728
    /// speech is found, as the voice models do.
    CdmAuto,
    /// A Comtech CDM-600(L)'s voice: the TPC 2964 chain to its Drop &
    /// Insert (or E1) timeslots, with the G.728 voice found in them played
    /// as soon as it is found.
    Cdm600Voice,
    /// A Comtech CDM-570L at the rates seen (9.6 kbit/s, K = 7 rate ½): the
    /// Viterbi chain, then the payload search — on PH-ML, BSC blocks
    /// carrying a radar track feed.
    Cdm570,
    /// A Comtech CDM-570L trunk between centres (ASA176 ML–PH: TPC 2964,
    /// 1024 kbit/s): the TPC 2964 chain to its timeslots in HDLC packets —
    /// G.728 voice channels played as soon as speech is found, one talker at
    /// a time — and the IP beside them: ADS-B (ASTERIX CAT021) and radar
    /// data for the scope.
    Cdm570Trunk,
    /// Teledyne Paradise Q-Flex FastLink (QPSK, rate 0.710): sync word,
    /// LDPC decoding and descrambling as measured on a live carrier; the
    /// data go to the same payload search as TPC 2964's.
    FastLink,
    /// A convolutional code: the K = 7 one (EN 300 421's, as IESS-308/309
    /// SCPC carriers use) at any rate, or any other rate-½ code found blind
    /// from the signal's parity checks (BPSK too); rate, code and
    /// orientation found blind, then the payload search on the decoded bits.
    Viterbi,
    /// DVB-CID (ETSI TS 103 129): the carrier identification signal under a
    /// host carrier — the uplink modulator's unique ID, position, telephone
    /// and text.
    CarrierId,
    /// An outbound of the DAMA telephony network on Intelsat 19 (102.4 kBd
    /// QPSK, measured — [`decsat_modem::is19`]): frames, descrambling and
    /// the packet stream, with the call signalling (request acks, channel
    /// assignments) picked out.
    Is19Outbound,
    /// A satellite beacon: its carrier tracked and measured (frequency,
    /// drift, C/N0) and any telemetry on it decoded blind — PCM/PSK/PM on a
    /// subcarrier, PCM/PM, or suppressed-carrier BPSK/QPSK; IRIG-style
    /// minor frames on a known or blind-found sync word, or CCSDS transfer
    /// frames (Reed–Solomon, randomiser, the K = 7 code). See
    /// [`crate::beacon`].
    Beacon,
    /// Generic PSK/APSK: lock any linearly modulated carrier and write its
    /// hard-decided symbols to a `.bin` file, one byte per symbol; on BPSK
    /// and QPSK look for a convolutional code meanwhile (the Viterbi
    /// chain's search, given up after a while) and decode what it finds.
    PskSymbols,
    /// TDMA: the carriers in the VFO (one, or an MF-TDMA group), their
    /// bursts found and demodulated one by one, the bursts' layout
    /// (preamble, pilots, fixed fields) read blind, the slot grid, frame and
    /// terminals; each burst's data symbols to a `.bursts` file on Record.
    Tdma,
    /// A return pool (a VSAT network's terminals sharing a block: carriers
    /// a hub resizes and moves, Mx-DMA style, and/or MF-TDMA bursts): its
    /// layout over time and its slot and frame grid ([`crate::tdma::pool`]),
    /// matched against an outbound's signalling timing in the GUI.
    ReturnPool,
    /// Record the VFO's narrowband IQ.
    IqRecord,
    /// Zoomed spectrum and level only.
    Spectrum,
}

impl DecoderKind {
    pub const ALL: [DecoderKind; 19] = [
        DecoderKind::Identify,
        DecoderKind::Dvbs2Ip,
        DecoderKind::Dvbs2Ts,
        DecoderKind::DvbsTs,
        DecoderKind::CdmAuto,
        DecoderKind::Tpc2964,
        DecoderKind::Cdm600Voice,
        DecoderKind::Cdm570,
        DecoderKind::Cdm570Trunk,
        DecoderKind::FastLink,
        DecoderKind::Viterbi,
        DecoderKind::CarrierId,
        DecoderKind::Is19Outbound,
        DecoderKind::Beacon,
        DecoderKind::PskSymbols,
        DecoderKind::Tdma,
        DecoderKind::ReturnPool,
        DecoderKind::IqRecord,
        DecoderKind::Spectrum,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DecoderKind::Identify => "Identify (what is this?)",
            DecoderKind::Dvbs2Ip => "DVB-S2/S2X → GSE/IP (PCAP)",
            DecoderKind::Dvbs2Ts => "DVB-S2/S2X → MPEG-TS",
            DecoderKind::DvbsTs => "DVB-S → MPEG-TS",
            DecoderKind::CdmAuto => "Comtech CDM modem (type found)",
            DecoderKind::Tpc2964 => "TPC (IESS-315 2964 or Comtech 7/8) → IP / TS",
            DecoderKind::Cdm600Voice => "Comtech CDM-600 voice (TPC → D&I → G.728)",
            DecoderKind::Cdm570 => "Comtech CDM-570L (Viterbi K=7 → data)",
            DecoderKind::Cdm570Trunk => "Comtech CDM-570L trunk (TPC → voice + ADS-B/radar)",
            DecoderKind::FastLink => "Q-Flex FastLink (QPSK 0.710) → data",
            DecoderKind::Viterbi => "Viterbi (K=7 any rate, or a code found blind) → data",
            DecoderKind::CarrierId => "Carrier ID (DVB-CID)",
            DecoderKind::Is19Outbound => {
                "IS-19 DAMA outbound (102.4 kBd) → packets, call signalling"
            }
            DecoderKind::Beacon => "Satellite beacon (carrier, PCM/PM telemetry, CCSDS)",
            DecoderKind::PskSymbols => "Generic PSK → symbols (.bin) + code search",
            DecoderKind::Tdma => "TDMA bursts (layout, slots, terminals)",
            DecoderKind::ReturnPool => "Return pool (Mx-DMA / MF-TDMA: layout, frame, outbound)",
            DecoderKind::IqRecord => "IQ recorder",
            DecoderKind::Spectrum => "Spectrum only",
        }
    }

    /// A stable name for saving (settings files, the CLI): never changes
    /// once given.
    pub fn key(self) -> &'static str {
        match self {
            DecoderKind::Identify => "identify",
            DecoderKind::Dvbs2Ip => "dvbs2-ip",
            DecoderKind::Dvbs2Ts => "dvbs2-ts",
            DecoderKind::DvbsTs => "dvbs-ts",
            DecoderKind::CdmAuto => "cdm-auto",
            DecoderKind::Tpc2964 => "tpc2964",
            DecoderKind::Cdm600Voice => "cdm600-voice",
            DecoderKind::Cdm570 => "cdm570",
            DecoderKind::Cdm570Trunk => "cdm570-trunk",
            DecoderKind::FastLink => "fastlink",
            DecoderKind::Viterbi => "viterbi",
            DecoderKind::CarrierId => "carrier-id",
            DecoderKind::Is19Outbound => "is19-outbound",
            DecoderKind::Beacon => "beacon",
            DecoderKind::PskSymbols => "psk",
            DecoderKind::Tdma => "tdma",
            DecoderKind::ReturnPool => "return-pool",
            DecoderKind::IqRecord => "iq-record",
            DecoderKind::Spectrum => "spectrum",
        }
    }

    /// The kind saved as `key`.
    pub fn from_key(key: &str) -> Option<DecoderKind> {
        DecoderKind::ALL.into_iter().find(|k| k.key() == key)
    }

    /// The decoder chain a preset runs: the CDM-600 voice and CDM-570L
    /// trunk presets are TPC 2964's, the CDM-570L's the Viterbi; every other
    /// kind its own (the CDM type-found model's is picked as it runs:
    /// [`VfoStatus::chain`]).
    pub fn chain(self) -> DecoderKind {
        match self {
            DecoderKind::Cdm600Voice | DecoderKind::Cdm570Trunk => DecoderKind::Tpc2964,
            DecoderKind::Cdm570 => DecoderKind::Viterbi,
            k => k,
        }
    }

    /// The Comtech CDM modems: one entry in the decoder menu
    /// ([`DecoderKind::CDM_MENU`]), the model chosen in a second menu.
    /// The first is what choosing the entry gives.
    pub const CDM: [DecoderKind; 5] = [
        DecoderKind::CdmAuto,
        DecoderKind::Tpc2964,
        DecoderKind::Cdm600Voice,
        DecoderKind::Cdm570,
        DecoderKind::Cdm570Trunk,
    ];
    pub const CDM_MENU: &str = "Comtech CDM modem";

    pub fn is_cdm(self) -> bool {
        DecoderKind::CDM.contains(&self)
    }

    /// A preset that plays the G.728 voice it finds by itself (with its
    /// "play when speech is found" switch): the CDM-600L's, the CDM-570L
    /// trunk's, a Q-Flex FastLink's.
    pub fn plays_voice(self) -> bool {
        matches!(
            self,
            DecoderKind::Cdm600Voice
                | DecoderKind::Cdm570Trunk
                | DecoderKind::CdmAuto
                | DecoderKind::FastLink
        )
    }

    /// A CDM preset's name in the model menu (else the label).
    pub fn cdm_model(self) -> &'static str {
        match self {
            // (IP comes from the CDM-570Ls: the CDM-600s carry voice, Rory
            // says — they do no IP.)
            DecoderKind::Tpc2964 => "CDM-570L TPC (TPC 2964 or 7/8 → IP / TS)",
            DecoderKind::Cdm600Voice => "CDM-600L voice (TPC 2964 → D&I++ → G.728)",
            DecoderKind::Cdm570 => {
                "CDM-570L Viterbi (Viterbi K=7 → data, e.g. radar: BSC or ASTERIX)"
            }
            DecoderKind::Cdm570Trunk => {
                "CDM-570L trunk (TPC 2964 → G.728 voice + IP: ADS-B, radar)"
            }
            DecoderKind::CdmAuto => "Type found (TPC 2964 or Viterbi; the model from its data)",
            k => k.label(),
        }
    }

    /// The decoder ends in an MPEG-TS (with its outputs and analyser), or
    /// may (TPC 2964, when its data carry one).
    pub fn outputs_ts(self) -> bool {
        matches!(
            self.chain(),
            DecoderKind::Dvbs2Ts
                | DecoderKind::DvbsTs
                | DecoderKind::Tpc2964
                | DecoderKind::CdmAuto
                | DecoderKind::FastLink
                | DecoderKind::Viterbi
        )
    }

    /// IP can come out of the decoder: GSE, MPE in a transport stream, or
    /// a modem's link layer (HDLC, bridged Ethernet).
    pub fn carries_ip(self) -> bool {
        self.chain() == DecoderKind::Dvbs2Ip || self.outputs_ts()
    }

    /// The decoder locks a carrier and decodes it: its own state leads the
    /// side panel, and Identify's view folds away.
    pub fn demodulates(self) -> bool {
        matches!(
            self.chain(),
            DecoderKind::Dvbs2Ip
                | DecoderKind::Dvbs2Ts
                | DecoderKind::DvbsTs
                | DecoderKind::Tpc2964
                | DecoderKind::CdmAuto
                | DecoderKind::FastLink
                | DecoderKind::Viterbi
                | DecoderKind::PskSymbols
                | DecoderKind::Is19Outbound
                | DecoderKind::Beacon
        )
    }

    pub fn short(self) -> &'static str {
        match self {
            DecoderKind::Identify => "ID",
            DecoderKind::Dvbs2Ip => "S2→IP",
            DecoderKind::Dvbs2Ts => "S2→TS",
            DecoderKind::DvbsTs => "S→TS",
            DecoderKind::CdmAuto => "CDM?",
            DecoderKind::Tpc2964 => "TPC",
            DecoderKind::Cdm600Voice => "CDM",
            DecoderKind::Cdm570 => "570",
            DecoderKind::Cdm570Trunk => "TRK",
            DecoderKind::FastLink => "FL",
            DecoderKind::Viterbi => "VIT",
            DecoderKind::CarrierId => "CID",
            DecoderKind::Is19Outbound => "I19",
            DecoderKind::Beacon => "BCN",
            DecoderKind::PskSymbols => "PSK",
            DecoderKind::Tdma => "TDMA",
            DecoderKind::ReturnPool => "POOL",
            DecoderKind::IqRecord => "REC",
            DecoderKind::Spectrum => "SPEC",
        }
    }
}

/// How a VFO treats its signal: a continuous carrier (SCPC: Identify, the
/// modem and DVB decoders, the generic PSK; a burst landing on it is
/// interference, coasted through) or a channel terminals take turns on
/// (TDMA: the burst analyser). Which decoders a VFO offers follows it; the
/// waterfall's brackets say which a band looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SignalMode {
    #[default]
    Scpc,
    Tdma,
}

impl SignalMode {
    pub const ALL: [SignalMode; 2] = [SignalMode::Scpc, SignalMode::Tdma];

    /// The mode a decoder belongs to (the tools, either: SCPC).
    pub fn of(decoder: DecoderKind) -> Self {
        if matches!(decoder, DecoderKind::Tdma | DecoderKind::ReturnPool) {
            SignalMode::Tdma
        } else {
            SignalMode::Scpc
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SignalMode::Scpc => "SCPC",
            SignalMode::Tdma => "TDMA",
        }
    }

    /// A stable name for saving.
    pub fn key(self) -> &'static str {
        match self {
            SignalMode::Scpc => "scpc",
            SignalMode::Tdma => "tdma",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        SignalMode::ALL.into_iter().find(|m| m.key() == key)
    }

    /// What a VFO switched to this mode runs (when its decoder does not
    /// fit it).
    pub fn default_decoder(self) -> DecoderKind {
        match self {
            SignalMode::Scpc => DecoderKind::Identify,
            SignalMode::Tdma => DecoderKind::Tdma,
        }
    }
}

impl DecoderKind {
    /// Offered in `mode`: the burst analyser and the return pool in TDMA,
    /// the rest in SCPC, the IQ recorder and the spectrum in both.
    pub fn fits(self, mode: SignalMode) -> bool {
        match self {
            DecoderKind::IqRecord | DecoderKind::Spectrum => true,
            DecoderKind::Tdma | DecoderKind::ReturnPool => mode == SignalMode::Tdma,
            _ => mode == SignalMode::Scpc,
        }
    }
}

/// A VFO's user-facing settings.
#[derive(Debug, Clone, PartialEq)]
pub struct VfoSettings {
    pub name: String,
    /// Centre relative to the wideband centre, Hz.
    pub offset_hz: f64,
    pub bandwidth_hz: f64,
    pub decoder: DecoderKind,
    /// SCPC or TDMA: which decoders it offers (see [`SignalMode`]).
    pub signal: SignalMode,
    pub enabled: bool,
    /// Known symbol rate; `None` finds it blind.
    pub symbol_rate: Option<f64>,
    /// PL scrambling gold-code index.
    pub gold_code: u32,
    /// Generic PSK: the constellation to decide on; `None` takes Identify's.
    pub psk_modulation: Option<decsat_core::Modulation>,
    /// Generic PSK: look for text in the decided bits, every way of reading
    /// them at once.
    pub find_text: bool,
    /// Generic PSK: how the symbols written to the .bin are numbered.
    pub symbol_labels: crate::psk::SymbolLabels,
    /// Generic PSK: write each symbol's bits (log2 M of them, MSB first,
    /// packed into bytes) rather than a byte a symbol. Taken when a
    /// recording starts.
    pub psk_bits: bool,
    /// Q-Flex FastLink: record the data whenever a channel of the TDM
    /// multiplex inside goes active (the 10 s before included).
    pub record_on_activity: bool,
    /// Keep the down-converter on the carrier as its frequency drifts (an
    /// LNB warming up, a satellite's Doppler): while the demodulator is
    /// locked, the offset its carrier loop measures is moved into the
    /// VFO's tuning, so the carrier cannot wander out of the VFO's band.
    pub follow_carrier: bool,
    /// DVB-CID: low-SNR mode (searches 96–384 bits deep, a looser
    /// threshold, gentle tracking throughout) for a CID far under spec.
    pub cid_low_snr: bool,
    /// The receiver's clock correction, ppm: how much faster a transmitter's
    /// clock runs against the SDR's (an SDR clock running slow shows as
    /// positive). The CID decoder resamples to the corrected chip rate, so
    /// its long searches see no drift. It is what the CID card measures.
    pub clock_ppm: f64,
    /// Write the decoder's output to a file: symbols (generic PSK), a
    /// modem's data (.bin), the TS (.ts). Off by default — the decoder
    /// shows what it finds until recording is asked for.
    pub record: bool,
    /// Write the IP packets to a PCAP file.
    pub pcap: bool,
    /// Feed the IP packets live to Wireshark: each new number starts a
    /// new one (`FecOutput::wireshark`), `None` stops feeding.
    pub wireshark: Option<u32>,
    /// DVB-S2 → GSE/IP: read GSE this way; `None` detects it from the data.
    pub gse_variant: Option<decsat_gse::Variant>,
    /// DVB-S2 → MPEG-TS: send the TS by UDP to `ts_udp` ("host:port").
    pub ts_udp_on: bool,
    pub ts_udp: String,
    /// DVB-S2 → MPEG-TS: serve the TS over TCP/HTTP on `ts_tcp`.
    pub ts_tcp_on: bool,
    pub ts_tcp: String,
    /// DVB-S2 → MPEG-TS: the BISS key, as typed (12 or 16 hex digits;
    /// anything else, empty included, leaves scrambled packets as they
    /// are). With a key every output carries the descrambled stream.
    pub biss_key: String,
    /// DVB-S2 → MPEG-TS: a copy of the (descrambled) stream for the in-app
    /// player.
    pub ts_tap: Option<decsat_ts::TsTap>,
    /// A multistream carrier: the input stream (ISI) whose transport stream
    /// is shown and output; `None` for the first that carries one.
    pub ts_isi: Option<u8>,
    /// DVB-S2 → IP: the UDP flow (destination) whose transport packets
    /// are the VFO's MPEG-TS, for the analyser and the player; `None` for
    /// the busiest one found.
    pub udp_ts: Option<std::net::SocketAddr>,
    /// DVB-S2 → IP or TS: play this multicast audio stream (group:port), in
    /// the app or (`audio_external`) relayed to a media player.
    pub audio_play: Option<std::net::SocketAddr>,
    pub audio_external: bool,
    /// DVB-S2 → IP or TS: record this multicast audio stream to a file.
    pub audio_record: Option<std::net::SocketAddr>,
    /// A modem carrying an E1: the timeslot to play, and one to record.
    pub e1_play: Option<u8>,
    pub e1_record: Option<u8>,
    /// A Q-Flex's TDM multiplex: play, and record, its call's voice.
    pub tdm_play: bool,
    pub tdm_record: bool,
    /// Programme audio (apt-X in an IBS carrier): play it, and record it.
    pub programme_play: bool,
    pub programme_record: bool,
    /// The CDM-600 voice preset: play the G.728 voice as soon as it is
    /// found, without ▶ (so several carriers can be heard at once).
    pub voice_auto: bool,
    /// The beacon decoder: sound a tone whose pitch follows the carrier's
    /// C/N0 (the GUI plays it; the engine ignores it).
    pub pointing_tone: bool,
    /// The beacon decoder: where to look for the telemetry (subcarrier
    /// offset from the carrier, its data's reach, the bit rate), each
    /// found blind when `None`.
    pub beacon_tune: crate::beacon::Tune,
    /// Where the IQ recorder and the symbol writer write.
    pub record_dir: PathBuf,
    /// DVB-S2: append every good BBFRAME to this file, for working out
    /// what an unknown stream carries (see [`crate::fec::FecOutput`]).
    pub bbframes: Option<PathBuf>,
}

impl VfoSettings {
    pub fn new(
        name: impl Into<String>,
        offset_hz: f64,
        bandwidth_hz: f64,
        decoder: DecoderKind,
    ) -> Self {
        VfoSettings {
            name: name.into(),
            offset_hz,
            bandwidth_hz,
            decoder,
            signal: SignalMode::of(decoder),
            enabled: true,
            symbol_rate: None,
            gold_code: 0,
            psk_modulation: None,
            find_text: true,
            symbol_labels: crate::psk::SymbolLabels::Standard,
            psk_bits: false,
            record_on_activity: false,
            follow_carrier: true,
            cid_low_snr: false,
            clock_ppm: 0.0,
            record: false,
            pcap: false,
            wireshark: None,
            gse_variant: None,
            // Local only: a player on this machine. Point them elsewhere on
            // purpose to stream to the network.
            ts_udp_on: false,
            ts_udp: "127.0.0.1:1234".into(),
            ts_tcp_on: false,
            ts_tcp: "127.0.0.1:8001".into(),
            biss_key: String::new(),
            ts_tap: None,
            ts_isi: None,
            udp_ts: None,
            audio_play: None,
            audio_external: false,
            audio_record: None,
            e1_play: None,
            e1_record: None,
            tdm_play: false,
            tdm_record: false,
            programme_play: false,
            programme_record: false,
            voice_auto: true,
            pointing_tone: false,
            beacon_tune: crate::beacon::Tune::default(),
            record_dir: std::env::temp_dir(),
            bbframes: None,
        }
    }
}

/// What a VFO worker publishes for the GUI. Cloned on read; kept small.
#[derive(Debug, Clone, Default)]
pub struct VfoStatus {
    pub out_rate: f64,
    pub decimation: usize,
    /// The VFO offset the worker is running on, and the carrier drift it has
    /// followed beyond it (Hz) — for the GUI to move the VFO by.
    pub vfo_offset_hz: f64,
    pub follow_hz: f64,
    /// The VFO's centre is outside what the receiver takes in (the radio
    /// was retuned away from its carrier): it is given no signal, and idles
    /// until the radio comes back.
    pub out_of_span: bool,
    /// Fraction of real time the worker spends processing (1.0 = can't keep up).
    pub load: f32,
    pub dropped: u64,
    pub level_db: f32,
    /// The VFO's own spectrum, dB, FFT-shifted.
    pub spectrum_db: Vec<f32>,
    /// Points for the constellation view.
    pub scatter: Vec<Iq>,
    pub identification: Option<Identification>,
    /// A coded modem's chain running (TPC 2964, the Viterbi, FastLink):
    /// for the CDM type-found model, the one it picked.
    pub chain: Option<DecoderKind>,
    /// The CDM type-found model: the model its carrier turned out to be
    /// (from the chain, then what the payload carries).
    pub cdm_model: Option<DecoderKind>,
    /// 0..1 while gathering signal for Identify or DVB-S2 acquisition.
    pub progress: f32,
    /// Identify's last answer came from too short a listen to rule DVB-S2
    /// out at the symbol rate it found; a longer listen is under way.
    pub provisional: bool,
    pub lock: Option<LockState>,
    pub frames: u64,
    pub lock_losses: u64,
    /// MODCOD index → frames, DVB-S2 decoders.
    pub modcods: BTreeMap<u8, u64>,
    /// The latest data frame's MODCOD (dummy frames, which an ACM carrier
    /// sends between data, left out so the reading holds still).
    pub last_modcod: Option<u8>,
    pub symbol_rate: Option<f64>,
    pub message: String,
    /// File being written and its size so far: bytes of IQ for the recorder,
    /// symbols (one byte each) for the generic PSK decoder.
    pub recording: Option<(PathBuf, u64)>,
    /// `recording` is being written now (not just the last file).
    pub recording_active: bool,
    /// Carrier loop of a running demodulator (for Identify: its live view).
    pub carrier: Option<CarrierState>,
    /// DVB-S2: the PL scrambling sequence in use (it may have been found,
    /// not set).
    pub gold_code: Option<u32>,
    /// DVB-S2/S2X: super-frames (Annex E), once looked for.
    pub superframes: Option<crate::superframe::SfView>,
    /// DVB-S2 decoders: what FEC has made of the frames.
    pub fec: Option<FecStats>,
    /// Generic PSK: text found in the decided bits.
    pub text: Option<decsat_modem::text::TextView>,
    /// DVB-CID: the despreader and what the identifier said.
    pub cid: Option<crate::cid::CidView>,
    /// TDMA: the carriers, their bursts, layouts, slots and terminals.
    pub tdma: Option<crate::tdma::TdmaView>,
    /// A return pool: its layout over time and its grid.
    pub pool: Option<crate::tdma::pool::PoolView>,
    /// The Intelsat 19 DAMA outbound: frames, packets, call signalling.
    pub is19: Option<decsat_modem::is19::OutboundView>,
    /// A satellite beacon: its carrier, telemetry and frames.
    pub beacon: Option<crate::beacon::BeaconView>,
}

/// A running demodulator's carrier loop, for display.
#[derive(Debug, Clone, Copy)]
pub struct CarrierState {
    pub locked: bool,
    /// The symbols' modulation error ratio, dB: their Es/N0 as measured
    /// (signal power over the error from the nearest ideal point).
    pub mer_db: f32,
    /// Residual offset the loop is tracking, Hz.
    pub offset_hz: f64,
    pub modulation: decsat_core::Modulation,
    /// Information bits a symbol carries, once the code is known (its rate
    /// times the bits per symbol, less framing overhead): Eb/N0 is then per
    /// information bit. `None`: per channel bit (uncoded).
    pub info_bits: Option<f32>,
    /// Bursts of interference (Bluetooth leaking into the HackRF's IF, say)
    /// the loop coasted through: the generic PSK demodulator's.
    pub bursts: u64,
}

impl CarrierState {
    /// Es/N0, dB: the MER.
    pub fn esn0_db(&self) -> f32 {
        self.mer_db
    }

    /// Eb/N0, dB: Es/N0 less 10·log10 of the bits a symbol carries
    /// (information bits when the code is known, else channel bits).
    pub fn ebn0_db(&self) -> f32 {
        let bits = self
            .info_bits
            .unwrap_or(f32::from(self.modulation.bits_per_symbol()));
        self.mer_db - 10.0 * bits.max(1e-3).log10()
    }

    /// "Es/N0 8.1 dB · Eb/N0 4.6 dB", the Eb/N0 marked uncoded when the
    /// code's rate is not known, and the interference bursts if any.
    pub fn snr_text(&self) -> String {
        format!(
            "Es/N0 {:.1} dB · Eb/N0 {:.1} dB{}{}",
            self.esn0_db(),
            self.ebn0_db(),
            if self.info_bits.is_some() {
                ""
            } else {
                " (uncoded)"
            },
            match self.bursts {
                0 => String::new(),
                n => format!(" · {n} interference bursts coasted"),
            }
        )
    }
}

/// Messages on a worker's queue. Settings do not travel here: the queue is
/// bounded (that is how a slow worker sheds blocks), so a blocking send from
/// the GUI could freeze it behind a busy worker. Settings go in a mailbox slot
/// instead, and a non-blocking `Wake` tells the worker to look.
pub(crate) enum VfoMsg {
    /// A block of the receiver's samples and when its first one came on the
    /// source clock (s): what lets two VFOs line up their views of one
    /// signal.
    Block(Arc<Vec<Iq>>, f64),
    Wake,
}

/// The mailbox slot: the latest settings not yet taken, and how much of
/// their offset's move since the worker last looked was the radio
/// retuning under the VFO rather than the VFO moving (see
/// [`VfoHandle::post_carried`]). The two travel together so the worker
/// can tell them apart however many posts it missed.
#[derive(Default)]
pub(crate) struct Mail {
    settings: Option<VfoSettings>,
    carried_hz: f64,
}

/// The engine's handle on a running VFO.
pub(crate) struct VfoHandle {
    tx: SyncSender<VfoMsg>,
    lossless: bool,
    /// The receiver's sample rate: a VFO centred beyond ±half of it is out
    /// of the span and given nothing.
    in_rate: f64,
    pub status: Arc<Mutex<VfoStatus>>,
    pub settings: VfoSettings,
    dropped: Arc<AtomicU64>,
    /// Input samples in the blocks dropped (decoders that keep time, the
    /// CID's, stand zeros in for them).
    lost: Arc<AtomicU64>,
    /// Latest settings not yet taken by the worker.
    mailbox: Arc<Mutex<Mail>>,
    stop: Arc<AtomicBool>,
    /// Stop without working through what is queued (here and on the FEC
    /// and CID threads).
    quit: Arc<AtomicBool>,
    /// Blocks queued to the worker, and frames or blocks to its FEC or CID
    /// thread, not yet done.
    pub(crate) pending: crate::fec::Pending,
    join: Option<JoinHandle<()>>,
}

impl VfoHandle {
    /// Queue a block, its first sample at `start_s` on the source clock;
    /// drop it (and count) if the worker is behind — or, lossless, wait for
    /// it.
    pub fn offer(&self, block: &Arc<Vec<Iq>>, start_s: f64) {
        if !self.settings.enabled || out_of_span(&self.settings, self.in_rate) {
            return;
        }
        self.pending.add();
        if self.lossless {
            if self
                .tx
                .send(VfoMsg::Block(Arc::clone(block), start_s))
                .is_err()
            {
                self.pending.done();
            }
        } else if let Err(e) = self.tx.try_send(VfoMsg::Block(Arc::clone(block), start_s)) {
            self.pending.done();
            if let TrySendError::Full(_) = e {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                self.lost.fetch_add(block.len() as u64, Ordering::Relaxed);
            }
        }
    }

    /// Hand the worker new settings without ever blocking the caller.
    pub fn post_settings(&mut self, settings: VfoSettings) {
        self.post_carried(settings, 0.0);
    }

    /// The radio was retuned and the VFO's offset moved by `by` Hz (the
    /// retune's opposite) to stay on its carrier: the same signal at
    /// another offset, so the worker moves its down-converter and its
    /// decoder carries on. Anything else changed in `settings` applies as
    /// with [`post_settings`](Self::post_settings).
    pub fn post_carried(&mut self, settings: VfoSettings, by: f64) {
        self.settings = settings.clone();
        {
            let mut m = self.mailbox.lock().unwrap();
            m.settings = Some(settings);
            m.carried_hz += by;
        }
        // If the queue is full the worker is busy and will see the mailbox
        // with its next block anyway.
        let _ = self.tx.try_send(VfoMsg::Wake);
    }

    /// Stop the VFO and wait for its threads. A file played losslessly
    /// has its queues worked through first; anything else (live, real
    /// time) stops after the block in hand.
    pub fn stop(mut self) {
        if !self.lossless {
            self.quit.store(true, Ordering::Relaxed);
        }
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.tx.try_send(VfoMsg::Wake);
        // Dropping the sender also wakes a worker blocked on an empty queue.
        drop(self.tx);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }

    /// Stop the VFO (a deletion) without waiting: its threads finish the
    /// block in hand, drop the rest, and are joined on a thread of their
    /// own, so a decoder deep in a search never holds up the caller.
    pub fn discard(mut self) {
        self.quit.store(true, Ordering::Relaxed);
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.tx.try_send(VfoMsg::Wake);
        drop(self.tx);
        if let Some(j) = self.join.take() {
            let _ = std::thread::Builder::new()
                .name("vfo-reaper".into())
                .spawn(move || {
                    let _ = j.join();
                });
        }
    }
}

/// Blocks a worker may have queued before new ones are dropped.
const QUEUE_DEPTH: usize = 8;

/// A VFO centred outside what a receiver at `in_rate` takes in (±half the
/// rate about its centre).
fn out_of_span(s: &VfoSettings, in_rate: f64) -> bool {
    s.offset_hz.abs() > in_rate / 2.0
}

/// `lossless`: wait for the worker rather than dropping blocks (a file
/// played as fast as possible). `plans`: the engine's plan feed and this
/// VFO's id on it.
pub(crate) fn spawn(
    in_rate: f64,
    settings: VfoSettings,
    lossless: bool,
    plans: Option<crate::plan_feed::PlanLink>,
) -> VfoHandle {
    let (tx, rx) = mpsc::sync_channel(QUEUE_DEPTH);
    let status = Arc::new(Mutex::new(VfoStatus::default()));
    let dropped = Arc::new(AtomicU64::new(0));
    let lost = Arc::new(AtomicU64::new(0));
    let mailbox = Arc::new(Mutex::new(Mail::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let quit = Arc::new(AtomicBool::new(false));
    let pending = crate::fec::Pending::default();
    let join = {
        let status = Arc::clone(&status);
        let dropped = Arc::clone(&dropped);
        let lost = Arc::clone(&lost);
        let mailbox = Arc::clone(&mailbox);
        let stop = Arc::clone(&stop);
        let quit = Arc::clone(&quit);
        let pending = pending.clone();
        let settings = settings.clone();
        std::thread::Builder::new()
            .name(format!("vfo-{}", settings.name))
            .spawn(move || {
                let mut w = Worker::new(in_rate, settings, status, dropped);
                w.lossless = lossless;
                w.plans = plans;
                w.lost = lost;
                w.pending = pending;
                // `run` takes the worker by value: it is dropped when the
                // loop ends, which joins its FEC or CID thread.
                w.run(rx, mailbox, stop, quit)
            })
            .expect("spawning a VFO thread")
    };
    VfoHandle {
        tx,
        lossless,
        in_rate,
        status,
        settings,
        dropped,
        lost,
        mailbox,
        stop,
        quit,
        pending,
        join: Some(join),
    }
}

/// A first look: ~0.6 s, but never more than 2 s of signal, so a narrow VFO
/// answers quickly (a 10 kBd carrier gives 20 000 symbols, plenty for the
/// symbol rate). For Identify, whether that was long enough to judge DVB-S2 is
/// decided afterwards, from the symbol rate it measured; the demodulators
/// only need the symbol rate from it, and find frames themselves.
fn first_look(out_rate: f64) -> usize {
    ((out_rate * 0.6) as usize).clamp((out_rate * 2.0).min(300_000.0) as usize, 2_000_000)
}

/// Samples holding two of the longest PLFRAMEs at `rs`, with margin — what it
/// takes to see one header confirmed on its frame grid wherever in the frame
/// the listen happens to start.
fn enough_frames(out_rate: f64, rs: f64) -> usize {
    let symbols = 2.2 * decsat_frame::MAX_PLFRAME_LEN as f64;
    (symbols * out_rate / rs) as usize
}

/// Pause between Identify checks once it has a confident answer.
const IDENTIFY_REST: std::time::Duration = std::time::Duration::from_secs(3);

enum Decoder {
    Identify {
        buf: Vec<Iq>,
        rest_until: Option<Instant>,
        /// Samples to gather for the current check.
        target: usize,
        /// The last result came from too short a listen to rule DVB-S2 out.
        provisional: bool,
        /// Demodulating live between identifications.
        live: Option<LiveView>,
    },
    Spectrum,
    Record {
        writer: Option<IqFileWriter>,
        path: PathBuf,
        bytes: u64,
    },
    Dvbs2 {
        buf: Vec<Iq>,
        demod: Option<Box<Demod>>,
        /// LDPC/BCH on its own thread, started with the demodulator.
        fec: Option<FecWorker>,
    },
    /// A coded single-carrier modem: DVB-S or TPC 2964 (`kind`).
    Modem {
        kind: DecoderKind,
        buf: Vec<Iq>,
        demod: Option<Box<PskDemod>>,
        /// The FEC (Viterbi and Reed–Solomon, or the turbo product code),
        /// the payload and its outputs on the FEC thread.
        fec: Option<FecWorker>,
        /// The CDM type-found model: how its chain is being picked.
        auto: Option<AutoPick>,
    },
    /// DVB-CID: Identify's first look gives the host carrier's centre and
    /// rate, then the despreader runs on its own thread.
    Cid {
        buf: Vec<Iq>,
        worker: Option<crate::cid::CidWorker>,
    },
    /// TDMA: the analyser runs on the VFO's baseband from the start.
    Tdma(Box<crate::tdma::TdmaAnalyser>),
    /// A return pool: the tracker on the VFO's baseband from the start, and
    /// its bursts demodulated by an outbound's plan once one is followed.
    Pool(
        Box<crate::tdma::pool::PoolTracker>,
        Box<crate::tdma::pool_bursts::PoolBursts>,
        Box<crate::tdma::inroute_bursts::InrouteBursts>,
    ),
    /// The Intelsat 19 DAMA outbound: Identify's first look centres the
    /// carrier, then QPSK at the measured rate into the framer.
    Is19 {
        buf: Vec<Iq>,
        demod: Option<Box<PskDemod>>,
        rx: Option<Box<decsat_modem::is19::OutboundRx>>,
        file: Is19File,
    },
    /// A satellite beacon: the receiver, and its files while recording.
    Beacon {
        rx: Box<crate::beacon::BeaconRx>,
        files: crate::beacon::BeaconFiles,
    },
    Psk {
        buf: Vec<Iq>,
        demod: Option<Box<PskDemod>>,
        /// Open while recording.
        writer: Option<BufWriter<File>>,
        /// The current or last file, and the symbols in it.
        path: Option<PathBuf>,
        written: u64,
        /// Opening the file failed; shown until recording is turned off.
        open_failed: bool,
        /// The carrier's offset in the band, for file names.
        carrier_hz: f64,
        /// The live text search, while asked for.
        text: Option<Box<crate::psk::TextSearch>>,
        /// How the open file is written (bits or bytes) and bits not yet
        /// making a byte.
        pack: SymbolPacker,
        /// The look for a convolutional code (BPSK and QPSK), and what it
        /// decodes, on the FEC thread.
        fec: Option<FecWorker>,
    },
}

/// The CDM type-found model's search: the chains tried that did not lock
/// their code, and the signal (samples) the one running has had.
#[derive(Default)]
struct AutoPick {
    failed: Vec<DecoderKind>,
    samples: usize,
    /// Its code locked: the chain stays.
    settled: bool,
}

/// Signal a chain picked by the CDM type-found model has to lock its code
/// before the next is tried, s: TPC 2964's blind structure search takes a
/// few seconds at the slow rates; the Viterbi's rate search less.
pub const AUTO_LOCK_SECS: f64 = 10.0;

/// The chains the CDM type-found model may run.
const AUTO_CHAINS: [DecoderKind; 3] = [
    DecoderKind::Tpc2964,
    DecoderKind::Viterbi,
    DecoderKind::FastLink,
];

/// The chain to try on a carrier Identify saw so: TPC 2964 when it found its
/// unique word, a FastLink when it named one, else the K = 7 code (what the
/// CDM-570L radar carriers run); one that failed before last, unless all
/// have.
fn auto_chain(id: &Identification, failed: &[DecoderKind]) -> DecoderKind {
    let hint = match &id.verdict {
        Verdict::NotDvbS2 { hint } => hint.as_str(),
        _ => "",
    };
    use DecoderKind::{FastLink, Tpc2964, Viterbi};
    let order: [DecoderKind; 3] = if crate::identify::names_tpc(hint) {
        [Tpc2964, Viterbi, FastLink]
    } else if hint.contains("FastLink") {
        [FastLink, Tpc2964, Viterbi]
    } else {
        [Viterbi, Tpc2964, FastLink]
    };
    order
        .into_iter()
        .find(|k| !failed.contains(k))
        .unwrap_or(order[0])
}

/// A chain has locked its code: TPC 2964 frames decoded, the K = 7 code's
/// rate found, FastLink codewords decoded.
fn code_locked(kind: DecoderKind, f: &crate::fec::FecStats) -> bool {
    match kind {
        DecoderKind::Tpc2964 => f.tpc.as_ref().is_some_and(|t| t.decoded > 0),
        DecoderKind::Viterbi => {
            f.viterbi.as_ref().is_some_and(|v| v.rate.is_some())
                || f.conv.as_ref().is_some_and(|c| c.code.is_some())
        }
        DecoderKind::FastLink => f.fastlink.as_ref().is_some_and(|x| x.decoded > 0),
        _ => true,
    }
}

/// The Comtech model a carrier is, from the chain it runs and what its data
/// carry: Drop & Insert++ (or a whole E1's) timeslots → the CDM-600L's
/// voice; HDLC packets carrying timeslots → a CDM-570L trunk (voice beside
/// its IP); anything else on TPC 2964 → a CDM-570L's data — IBS's
/// G.704-shaped frames too, which say nothing of voice; the K = 7 code → a
/// CDM-570L's (the radar carriers); a FastLink is a Q-Flex.
pub fn cdm_model(chain: DecoderKind, f: Option<&crate::fec::FecStats>) -> DecoderKind {
    let p = f.and_then(|f| f.payload.as_ref());
    match chain {
        DecoderKind::Tpc2964 => {
            let e1 = |p: &decsat_modem::payload::PayloadStats| {
                p.e1.is_some() && p.ibs_channels.is_none() && p.ibs_sync.is_none()
            };
            if p.is_some_and(|p| p.dandi.is_some() || e1(p)) {
                DecoderKind::Cdm600Voice
            } else if p
                .and_then(|p| p.slots.as_ref())
                .is_some_and(|s| s.slot_packets > 0)
            {
                DecoderKind::Cdm570Trunk
            } else {
                DecoderKind::Tpc2964
            }
        }
        // A code found blind other than the K = 7 is no Comtech's.
        DecoderKind::Viterbi
            if f.and_then(|f| f.conv.as_ref())
                .and_then(|c| c.code)
                .is_some_and(|c| !c.is_k7_standard()) =>
        {
            DecoderKind::Viterbi
        }
        DecoderKind::Viterbi => DecoderKind::Cdm570,
        k => k,
    }
}

/// Symbols into the .bin: a byte each, or their bits packed MSB first.
#[derive(Default)]
struct SymbolPacker {
    bits: bool,
    acc: u32,
    n: u32,
}

impl SymbolPacker {
    /// The bytes for `values` (labels, `per` bits each) into `out`.
    fn pack(&mut self, values: impl Iterator<Item = u8>, per: u32, out: &mut Vec<u8>) {
        if !self.bits {
            out.extend(values);
            return;
        }
        for v in values {
            self.acc = (self.acc << per) | u32::from(v) & ((1 << per) - 1);
            self.n += per;
            while self.n >= 8 {
                self.n -= 8;
                out.push((self.acc >> self.n) as u8);
            }
            self.acc &= (1 << self.n) - 1;
        }
    }
}

struct Worker {
    in_rate: f64,
    settings: VfoSettings,
    ddc: Ddc,
    decoder: Decoder,
    spec: Spectrum,
    status: Arc<Mutex<VfoStatus>>,
    dropped: Arc<AtomicU64>,
    load: f32,
    bb: Vec<Iq>,
    frames: Vec<crate::demod::PlFrame>,
    syms: Vec<(u8, Iq)>,
    /// The soft symbols alone, for decoders that take no labels.
    soft: Vec<Iq>,
    sym_bytes: Vec<u8>,
    modcods: BTreeMap<u8, u64>,
    last_modcod: Option<u8>,
    identification: Option<Identification>,
    /// Wait for the FEC thread rather than dropping its input.
    lossless: bool,
    /// Input samples dropped before reaching this thread, and how many of
    /// them have been passed on.
    lost: Arc<AtomicU64>,
    lost_seen: u64,
    /// The handle's count of work not yet done (see [`crate::fec::Pending`]).
    pending: crate::fec::Pending,
    /// Carrier drift followed (Hz) not yet taken into the settings' offset,
    /// and baseband samples since the last look.
    followed: f64,
    follow_samples: usize,
    /// Baseband gathered to check a blind symbol rate still holds, and
    /// checks in a row that found it moved.
    rate_watch: Vec<Iq>,
    rate_moves: u8,
    /// The block being worked on: its first sample and its end on the
    /// source clock, s.
    block_s: (f64, f64),
    /// The engine's plan feed and this VFO's id on it: a TS VFO posts a
    /// hub's burst plan there, a return pool reads it.
    plans: Option<crate::plan_feed::PlanLink>,
}

/// How much signal (s) each check of a blind symbol rate looks at.
const RATE_WATCH_SECS: f64 = 2.0;

/// How often a locked carrier's drift is followed (s), and the least
/// offset (Hz, and share of the symbol rate) worth a move.
const FOLLOW_EVERY: f64 = 0.25;
const FOLLOW_MIN_HZ: f64 = 5.0;
const FOLLOW_MIN_RS: f64 = 1e-3;

impl Worker {
    /// Tell the FEC or CID thread to stop after its current input.
    fn quit_threads(&self) {
        match &self.decoder {
            Decoder::Dvbs2 { fec: Some(f), .. }
            | Decoder::Modem { fec: Some(f), .. }
            | Decoder::Psk { fec: Some(f), .. } => f.quit(),
            Decoder::Cid {
                worker: Some(w), ..
            } => w.quit(),
            _ => {}
        }
    }

    fn new(
        in_rate: f64,
        settings: VfoSettings,
        status: Arc<Mutex<VfoStatus>>,
        dropped: Arc<AtomicU64>,
    ) -> Self {
        let ddc = Ddc::new(
            in_rate,
            settings.offset_hz,
            settings.bandwidth_hz.min(in_rate),
        );
        let decoder = Self::make_decoder(&settings, &ddc);
        Worker {
            in_rate,
            ddc,
            decoder,
            spec: Spectrum::new(1024),
            settings,
            status,
            dropped,
            lost: Arc::new(AtomicU64::new(0)),
            lost_seen: 0,
            pending: Default::default(),
            load: 0.0,
            bb: Vec::new(),
            frames: Vec::new(),
            syms: Vec::new(),
            soft: Vec::new(),
            sym_bytes: Vec::new(),
            modcods: BTreeMap::new(),
            last_modcod: None,
            identification: None,
            lossless: false,
            followed: 0.0,
            follow_samples: 0,
            rate_watch: Vec::new(),
            rate_moves: 0,
            block_s: (0.0, 0.0),
            plans: None,
        }
    }

    fn make_decoder(s: &VfoSettings, ddc: &Ddc) -> Decoder {
        match s.decoder.chain() {
            DecoderKind::Identify => Decoder::Identify {
                buf: Vec::new(),
                rest_until: None,
                target: first_look(ddc.out_rate()),
                provisional: false,
                live: None,
            },
            DecoderKind::Spectrum => Decoder::Spectrum,
            DecoderKind::IqRecord => {
                // Named by the VFO's RF centre (when known), which is the
                // recording's centre: opening it labels the axis.
                let path = s.record_dir.join(format!(
                    "decsat-{}-{}-{:.0}Sps-{}.cf32",
                    s.name.replace(' ', "_"),
                    crate::rf::freq_tag(s.offset_hz),
                    ddc.out_rate(),
                    unix_stamp()
                ));
                let _ = std::fs::create_dir_all(&s.record_dir);
                let writer = IqFileWriter::create(&path, SampleFormat::Cf32).ok();
                Decoder::Record {
                    writer,
                    path,
                    bytes: 0,
                }
            }
            DecoderKind::Dvbs2Ip | DecoderKind::Dvbs2Ts => Decoder::Dvbs2 {
                buf: Vec::new(),
                demod: None,
                fec: None,
            },
            // (The CDM presets arrive here as TPC 2964 or the Viterbi.)
            DecoderKind::DvbsTs
            | DecoderKind::Tpc2964
            | DecoderKind::Cdm600Voice
            | DecoderKind::Cdm570
            | DecoderKind::Cdm570Trunk
            | DecoderKind::CdmAuto
            | DecoderKind::FastLink
            | DecoderKind::Viterbi => Decoder::Modem {
                kind: s.decoder.chain(),
                buf: Vec::new(),
                demod: None,
                fec: None,
                auto: (s.decoder == DecoderKind::CdmAuto).then(AutoPick::default),
            },
            DecoderKind::CarrierId => Decoder::Cid {
                buf: Vec::new(),
                worker: None,
            },
            DecoderKind::Is19Outbound => Decoder::Is19 {
                buf: Vec::new(),
                demod: None,
                rx: None,
                file: Is19File::default(),
            },
            DecoderKind::Beacon => Decoder::Beacon {
                rx: {
                    let mut rx = Box::new(crate::beacon::BeaconRx::new(
                        ddc.out_rate(),
                        s.bandwidth_hz.min(ddc.out_rate()),
                    ));
                    rx.set_tune(s.beacon_tune);
                    rx
                },
                files: crate::beacon::BeaconFiles::default(),
            },
            DecoderKind::Tdma => Decoder::Tdma(Box::new(crate::tdma::TdmaAnalyser::new(
                ddc.out_rate(),
                s.bandwidth_hz.min(ddc.out_rate()),
            ))),
            DecoderKind::ReturnPool => Decoder::Pool(
                Box::new(crate::tdma::pool::PoolTracker::new(
                    ddc.out_rate(),
                    s.bandwidth_hz.min(ddc.out_rate()),
                )),
                Box::new(crate::tdma::pool_bursts::PoolBursts::new(
                    ddc.out_rate(),
                    crate::tdma::pool_bursts::PoolBursts::default_workers(),
                )),
                Box::new(crate::tdma::inroute_bursts::InrouteBursts::new(
                    ddc.out_rate(),
                    crate::tdma::pool_bursts::PoolBursts::default_workers(),
                )),
            ),
            DecoderKind::PskSymbols => Decoder::Psk {
                buf: Vec::new(),
                demod: None,
                writer: None,
                path: None,
                written: 0,
                open_failed: false,
                carrier_hz: s.offset_hz,
                text: None,
                pack: SymbolPacker::default(),
                fec: None,
            },
        }
    }

    /// Take new settings. `carried`: how much of the offset's move was the
    /// radio retuning under the VFO (see [`VfoHandle::post_carried`]).
    fn apply(&mut self, new: VfoSettings, carried: f64) {
        let rebuild_ddc = new.bandwidth_hz != self.settings.bandwidth_hz;
        // What the VFO itself moved: its offset's move less what carried it
        // along with a retune (a millihertz allows for the rounding).
        let moved = new.offset_hz - self.settings.offset_hz - carried;
        let moved = if moved.abs() < 1e-3 { 0.0 } else { moved };
        // The GUI moving the VFO by drift this worker followed (see
        // `follow`): the down-converter is there already, so nothing restarts.
        let absorbed = moved != 0.0
            && self.followed != 0.0
            && moved.signum() == self.followed.signum()
            && moved.abs() <= self.followed.abs() + 0.5;
        if absorbed {
            self.followed -= moved;
        }
        let retuned = moved != 0.0 && !absorbed;
        let new_decoder = new.decoder != self.settings.decoder
            || new.symbol_rate != self.settings.symbol_rate
            || new.gold_code != self.settings.gold_code
            || new.psk_modulation != self.settings.psk_modulation;
        if rebuild_ddc {
            self.ddc = Ddc::new(
                self.in_rate,
                new.offset_hz,
                new.bandwidth_hz.min(self.in_rate),
            );
        } else if retuned {
            // Retune in place: the filter keeps its history, so dragging is smooth.
            self.ddc.set_offset(new.offset_hz);
        } else if carried != 0.0 {
            // Carried with the radio: the carrier sits where it did in the
            // down-converter's output (the drift followed included), so
            // the demodulator sees no step — bar the moment the radio's
            // samples take to arrive at the new tuning.
            self.ddc.set_offset(self.ddc.offset_hz() + carried);
            // Where a symbols file's name says the carrier is (its offset,
            // made RF with the radio's new tuning).
            if let Decoder::Psk { carrier_hz, .. } = &mut self.decoder {
                *carrier_hz += carried;
            }
            // A beacon's log keeps its frequencies as first tuned.
            if let Decoder::Beacon { rx, .. } = &mut self.decoder {
                rx.carried(carried);
            }
        }
        // An enable/disable or a rename changes nothing about the signal; a
        // retune does. Without the reset an Identify VFO dragged onto another
        // carrier kept its gathered signal and analysed a mixture of the two.
        let signal_changed = rebuild_ddc || retuned || new_decoder;
        self.settings = new;
        // Where a beacon's telemetry is looked for: its receiver starts its
        // search again, the carrier loop running on.
        if let Decoder::Beacon { rx, .. } = &mut self.decoder {
            rx.set_tune(self.settings.beacon_tune);
        }
        // Record / GSE variant changes reach a running FEC thread directly.
        if let Decoder::Dvbs2 { fec: Some(w), .. }
        | Decoder::Modem { fec: Some(w), .. }
        | Decoder::Psk { fec: Some(w), .. } = &self.decoder
        {
            w.set_output(fec_output(&self.settings, &self.ddc, &self.plans));
        }
        // A recorder keeps its file across a retune (a new file per drag frame
        // would be worse); everything that analyses the signal starts afresh.
        let analyses = !matches!(self.decoder, Decoder::Record { .. } | Decoder::Spectrum);
        if (signal_changed && analyses) || new_decoder {
            self.decoder = Self::make_decoder(&self.settings, &self.ddc);
            self.modcods.clear();
            self.last_modcod = None;
            self.identification = None;
        }
        if rebuild_ddc || retuned {
            // The down-converter is back on the VFO: nothing followed.
            self.followed = 0.0;
        }
    }

    /// Follow the carrier's drift: a few times a second, a locked
    /// demodulator's residual offset moves into the down-converter's
    /// tuning (and out of the carrier loop, so it sees no step). The VFO's
    /// band then stays on the carrier however far it wanders.
    fn follow(&mut self, n: usize) {
        if !self.settings.follow_carrier {
            return;
        }
        self.follow_samples += n;
        if (self.follow_samples as f64) < FOLLOW_EVERY * self.ddc.out_rate() {
            return;
        }
        self.follow_samples = 0;
        let (f, rs) = match &self.decoder {
            Decoder::Dvbs2 { demod: Some(d), .. } if d.carrier_locked() => {
                (d.carrier_offset_hz(), d.symbol_rate())
            }
            Decoder::Modem { demod: Some(d), .. }
            | Decoder::Psk { demod: Some(d), .. }
            | Decoder::Is19 { demod: Some(d), .. }
                if d.locked() =>
            {
                (d.carrier_offset_hz(), d.symbol_rate())
            }
            // A beacon's carrier: followed once well off the VFO's centre
            // (the loop tracks the rest, and each move is a step it must
            // take).
            Decoder::Beacon { rx, .. } => match rx.carrier_offset_hz() {
                Some(f) if f.abs() > 0.02 * self.ddc.bandwidth() => (f, 0.0),
                _ => return,
            },
            _ => return,
        };
        if f.abs() < FOLLOW_MIN_HZ.max(FOLLOW_MIN_RS * rs) {
            return;
        }
        // Not past the edge of what the receiver takes in.
        let to = self.ddc.offset_hz() + f;
        if to.abs() > 0.5 * (self.in_rate - self.ddc.bandwidth()) {
            return;
        }
        self.ddc.set_offset(to);
        match &mut self.decoder {
            Decoder::Dvbs2 { demod: Some(d), .. } => d.retune_hz(f),
            Decoder::Modem { demod: Some(d), .. }
            | Decoder::Psk { demod: Some(d), .. }
            | Decoder::Is19 { demod: Some(d), .. } => d.retune_hz(f),
            Decoder::Beacon { rx, .. } => rx.retune_hz(f),
            _ => {}
        }
        self.followed += f;
    }

    fn run(
        mut self,
        rx: Receiver<VfoMsg>,
        mailbox: Arc<Mutex<Mail>>,
        stop: Arc<AtomicBool>,
        quit: Arc<AtomicBool>,
    ) {
        self.run_loop(&rx, &mailbox, &stop);
        // A deletion: the FEC or CID thread is told to leave its queue, so
        // the join when `self` is dropped (at the end of this function) is
        // quick.
        if quit.load(Ordering::Relaxed) {
            self.quit_threads();
        }
    }

    fn run_loop(
        &mut self,
        rx: &Receiver<VfoMsg>,
        mailbox: &Arc<Mutex<Mail>>,
        stop: &Arc<AtomicBool>,
    ) {
        loop {
            // With no input for a while (a file that ended, a paused
            // source) publish anyway: the FEC and CID threads may still be
            // finishing, and their state should show.
            let msg = match rx.recv_timeout(std::time::Duration::from_millis(250)) {
                Ok(m) => m,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    self.publish(0);
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // `Option::map` on the settings taken; the carry is taken (and
            // zeroed) with them.
            let pending = {
                let mut m = mailbox.lock().unwrap();
                m.settings
                    .take()
                    .map(|s| (s, std::mem::take(&mut m.carried_hz)))
            };
            if let Some((s, carried)) = pending {
                self.apply(s, carried);
                self.publish(0);
            }
            match msg {
                VfoMsg::Wake => {}
                VfoMsg::Block(block, start_s) => {
                    let pending = self.pending.clone();
                    let _done = pending.guard();
                    // Blocks dropped since the last one: tell the CID
                    // decoder how much baseband never came.
                    let lost = self.lost.load(Ordering::Relaxed);
                    if lost > self.lost_seen {
                        let n = lost - self.lost_seen;
                        self.lost_seen = lost;
                        if let Decoder::Cid {
                            worker: Some(w), ..
                        } = &self.decoder
                        {
                            w.lost((n as f64 * self.ddc.out_rate() / self.in_rate) as u64);
                        }
                    }
                    let t0 = Instant::now();
                    self.block_s = (start_s, start_s + block.len() as f64 / self.in_rate);
                    self.bb.clear();
                    self.ddc.process(&block, &mut self.bb);
                    let n = self.bb.len();
                    self.decode();
                    self.follow(n);
                    self.watch_rate();
                    // Load: processing time over the block's real duration.
                    let real = block.len() as f64 / self.in_rate;
                    let used = t0.elapsed().as_secs_f64() / real.max(1e-9);
                    self.load = 0.9 * self.load + 0.1 * used as f32;
                    self.publish(n);
                }
            }
        }
        match std::mem::replace(&mut self.decoder, Decoder::Spectrum) {
            Decoder::Record {
                writer: Some(w), ..
            } => {
                let _ = w.finish();
            }
            Decoder::Psk {
                writer: Some(mut w),
                ..
            } => {
                let _ = w.flush();
            }
            // Closes (and flushes) the burst files.
            Decoder::Tdma(mut a) => a.set_recording(false, &self.settings.record_dir, ""),
            Decoder::Beacon { mut files, .. } => files.finish(),
            Decoder::Is19 {
                file:
                    Is19File {
                        writer: Some(mut w),
                        ..
                    },
                ..
            } => {
                let _ = w.flush();
            }
            _ => {}
        }
    }

    /// A modem or PSK carrier whose symbol rate was found blind: every two
    /// seconds, check the carrier still runs at it; found moved twice in a
    /// row (a modem reconfigured on the fly), acquire it afresh.
    fn watch_rate(&mut self) {
        let rs = match &self.decoder {
            Decoder::Modem { demod: Some(d), .. } | Decoder::Psk { demod: Some(d), .. }
                if self.settings.symbol_rate.is_none() =>
            {
                d.symbol_rate()
            }
            _ => {
                self.rate_watch.clear();
                self.rate_moves = 0;
                return;
            }
        };
        let out_rate = self.ddc.out_rate();
        self.rate_watch.extend_from_slice(&self.bb);
        if (self.rate_watch.len() as f64) < RATE_WATCH_SECS * out_rate {
            return;
        }
        let moved = crate::identify::symbol_rate_moved(&self.rate_watch, out_rate, rs);
        self.rate_watch.clear();
        self.rate_moves = if moved.is_some() {
            self.rate_moves + 1
        } else {
            0
        };
        if self.rate_moves >= 2 {
            self.rate_moves = 0;
            // The FEC thread's queue is at the old rate: leave it.
            self.quit_threads();
            self.decoder = Self::make_decoder(&self.settings, &self.ddc);
            self.identification = None;
        }
    }

    fn decode(&mut self) {
        let out_rate = self.ddc.out_rate();
        match &mut self.decoder {
            Decoder::Spectrum => {}
            Decoder::Record { writer, bytes, .. } => {
                if let Some(w) = writer {
                    if w.write(&self.bb).is_ok() {
                        *bytes += self.bb.len() as u64 * 8;
                    } else {
                        *writer = None;
                    }
                }
            }
            Decoder::Identify {
                buf,
                rest_until,
                target,
                provisional,
                live,
            } => {
                // Between identifications the last result drives a live
                // demodulator, so the constellation keeps moving.
                if let Some(l) = live {
                    l.process(&self.bb, &mut self.syms);
                    // Text in the decided bits, as the generic decoder looks.
                    match (self.settings.find_text, l.text.as_mut()) {
                        (true, Some(t)) => t.push(&self.syms),
                        (true, None) => {
                            let mut t = Box::new(crate::psk::TextSearch::new(l.modulation));
                            t.push(&self.syms);
                            l.text = Some(t);
                        }
                        (false, _) => l.text = None,
                    }
                }
                // After a confident result, rest before checking again: a
                // carrier rarely changes, and re-analysing millions of samples
                // back to back was most of a wide Identify VFO's CPU. A retune
                // resets this (see `apply`), so moving the VFO answers at once.
                if rest_until.is_some_and(|t| Instant::now() < t) {
                    return;
                }
                buf.extend_from_slice(&self.bb);
                if buf.len() >= *target {
                    let id = identify_in(buf, out_rate, Some(self.settings.bandwidth_hz));
                    let is_s2 = matches!(id.verdict, Verdict::DvbS2(_));
                    let needed = id.symbol_rate.map(|rs| enough_frames(out_rate, rs));
                    match needed {
                        // Not DVB-S2, but the listen was too short to see two
                        // frames at this symbol rate: say so, and listen longer
                        // straight away.
                        Some(n) if !is_s2 && buf.len() < n => {
                            *provisional = true;
                            *target = n.min(8_000_000);
                            *rest_until = None;
                        }
                        _ => {
                            *provisional = false;
                            *target = needed
                                .unwrap_or(*target)
                                .clamp(first_look(out_rate), 8_000_000);
                            *rest_until = Some(Instant::now() + IDENTIFY_REST);
                        }
                    }
                    // Keep the live view running if this is the same carrier;
                    // start a new one if not.
                    if !live.as_ref().is_some_and(|l| l.same_carrier(&id)) {
                        *live = LiveView::new(&id, out_rate);
                    }
                    self.identification = Some(id);
                    buf.clear();
                }
            }
            Decoder::Dvbs2 { buf, demod, fec } => match demod {
                Some(d) => {
                    self.frames.clear();
                    d.process(&self.bb, &mut self.frames);
                    for f in &mut self.frames {
                        f.src_s = Some(self.block_s.1);
                        *self.modcods.entry(f.pls.modcod).or_default() += 1;
                        if f.pls.modcod != 0 {
                            self.last_modcod = Some(f.pls.modcod);
                        }
                    }
                    if let Some(w) = fec {
                        for f in self.frames.drain(..) {
                            w.offer(f);
                        }
                    }
                }
                None => {
                    buf.extend_from_slice(&self.bb);
                    if buf.len() >= first_look(out_rate) {
                        let id = identify_in(buf, out_rate, Some(self.settings.bandwidth_hz));
                        let rs = self.settings.symbol_rate.or(id.symbol_rate);
                        let alpha = id.roll_off.map_or(0.35, |r| r.as_f64());
                        if let Some(rs) = rs.filter(|&r| out_rate / r >= 2.0) {
                            // Start demodulating, from the gathered signal on.
                            let mut d = Demod::new(out_rate, rs, alpha, self.settings.gold_code);
                            // Identify's offset is averaged over every header
                            // it saw: the best seed for the carrier loop.
                            if let Some(f) = id.carrier_offset_hz {
                                d = d.with_carrier_offset(f / rs);
                            }
                            let mut d = Box::new(d);
                            // The carrier may sit off the VFO centre; the
                            // demodulator expects it at DC. Shift what was
                            // gathered, and retune the DDC onto the carrier so
                            // every later block arrives centred too.
                            let mut shifted = std::mem::take(buf);
                            shift(&mut shifted, out_rate, id.center_offset_hz);
                            self.ddc
                                .set_offset(self.settings.offset_hz + id.center_offset_hz);
                            self.frames.clear();
                            d.process(&shifted, &mut self.frames);
                            let w = FecWorker::spawn(
                                rs,
                                fec_output(&self.settings, &self.ddc, &self.plans),
                                self.pending.clone(),
                            )
                            .lossless(self.lossless);
                            for mut f in self.frames.drain(..) {
                                f.src_s = Some(self.block_s.1);
                                w.offer(f);
                            }
                            *fec = Some(w);
                            *demod = Some(d);
                        } else {
                            buf.clear();
                        }
                        self.identification = Some(id);
                    }
                }
            },
            Decoder::Modem {
                kind,
                buf,
                demod,
                fec,
                auto,
            } => match demod {
                Some(d) => {
                    self.syms.clear();
                    d.process(&self.bb, &mut self.syms);
                    if let Some(w) = fec {
                        offer_symbols(w, *kind, d.modulation(), &self.syms);
                    }
                    // The CDM type-found model: a chain that has not locked
                    // its code in time gives way to the next (a fresh first
                    // look picks it).
                    if let Some(a) = auto.as_mut().filter(|a| !a.settled) {
                        a.samples += self.bb.len();
                        if a.samples as f64 >= AUTO_LOCK_SECS * out_rate {
                            let locked =
                                fec.as_ref().is_some_and(|w| code_locked(*kind, &w.stats()));
                            if locked {
                                a.settled = true;
                            } else {
                                a.failed.push(*kind);
                                if AUTO_CHAINS.iter().all(|k| a.failed.contains(k)) {
                                    a.failed.clear();
                                }
                                a.samples = 0;
                                if let Some(w) = fec.take() {
                                    w.quit();
                                }
                                *demod = None;
                                buf.clear();
                                *kind = DecoderKind::CdmAuto;
                            }
                        }
                    }
                }
                None => {
                    buf.extend_from_slice(&self.bb);
                    if buf.len() >= first_look(out_rate) {
                        let id = identify_in(buf, out_rate, Some(self.settings.bandwidth_hz));
                        // The CDM type-found model: its chain, from what
                        // Identify saw.
                        if let Some(a) = auto.as_mut() {
                            *kind = auto_chain(&id, &a.failed);
                            a.samples = 0;
                        }
                        let rs = self.settings.symbol_rate.or(id.symbol_rate);
                        let alpha = id.roll_off.map_or(0.35, |r| r.as_f64());
                        let usable = !matches!(id.verdict, Verdict::NoSignal);
                        if let Some(rs) = rs.filter(|&r| usable && out_rate / r >= 2.0) {
                            let seed = id.carrier_offset_hz.unwrap_or(0.0) / rs;
                            // DVB-S is always QPSK; TPC 2964 is BPSK, QPSK
                            // or 8PSK (as set, or as Identify saw it).
                            let modulation = match *kind {
                                DecoderKind::Tpc2964 => match self
                                    .settings
                                    .psk_modulation
                                    .or(id.constellation.map(|c| c.modulation()))
                                {
                                    Some(
                                        decsat_core::Modulation::Bpsk
                                        | decsat_core::Modulation::Pi2Bpsk,
                                    ) => decsat_core::Modulation::Bpsk,
                                    Some(decsat_core::Modulation::Psk8) => {
                                        decsat_core::Modulation::Psk8
                                    }
                                    _ => decsat_core::Modulation::Qpsk,
                                },
                                // A convolutional code rides on BPSK or QPSK
                                // (the K = 7 search on QPSK alone).
                                DecoderKind::Viterbi => match self
                                    .settings
                                    .psk_modulation
                                    .or(id.constellation.map(|c| c.modulation()))
                                {
                                    Some(
                                        decsat_core::Modulation::Bpsk
                                        | decsat_core::Modulation::Pi2Bpsk,
                                    ) => decsat_core::Modulation::Bpsk,
                                    _ => decsat_core::Modulation::Qpsk,
                                },
                                _ => decsat_core::Modulation::Qpsk,
                            };
                            let mut d =
                                Box::new(PskDemod::new(out_rate, rs, alpha, modulation, seed));
                            let mut shifted = std::mem::take(buf);
                            shift(&mut shifted, out_rate, id.center_offset_hz);
                            self.ddc
                                .set_offset(self.settings.offset_hz + id.center_offset_hz);
                            let w = FecWorker::spawn(
                                rs,
                                fec_output(&self.settings, &self.ddc, &self.plans),
                                self.pending.clone(),
                            )
                            .lossless(self.lossless);
                            self.syms.clear();
                            d.process(&shifted, &mut self.syms);
                            offer_symbols(&w, *kind, modulation, &self.syms);
                            *fec = Some(w);
                            *demod = Some(d);
                        } else {
                            buf.clear();
                        }
                        self.identification = Some(id);
                    }
                }
            },
            Decoder::Cid { buf, worker } => match worker {
                Some(w) => {
                    w.set_low_snr(self.settings.cid_low_snr);
                    w.set_clock_ppm(self.settings.clock_ppm);
                    w.offer(self.bb.clone());
                }
                None => {
                    buf.extend_from_slice(&self.bb);
                    if buf.len() >= first_look(out_rate) {
                        let id = identify_in(buf, out_rate, Some(self.settings.bandwidth_hz));
                        let rs = self.settings.symbol_rate.or(id.symbol_rate);
                        let usable = !matches!(id.verdict, Verdict::NoSignal);
                        match rs.filter(|_| usable) {
                            Some(rs) => {
                                // The host's centre: Identify's, plus the
                                // residual its carrier lock measured (on a
                                // live 1 MBd carrier that was 8.6 kHz — far
                                // outside the CID search). Without a lock,
                                // search wider.
                                let (centre, span) = match id.carrier_offset_hz {
                                    Some(f) if id.carrier_locked => {
                                        (id.center_offset_hz + f, decsat_modem::cid::ACQ_SPAN_HZ)
                                    }
                                    // Not locked: Identify's centre, the span
                                    // wide enough to take in its residual
                                    // estimate too (a power-line guess, often
                                    // right: on a live 16APSK carrier the CID
                                    // sat 203 Hz from it).
                                    _ => (
                                        id.center_offset_hz,
                                        id.carrier_offset_hz
                                            .map_or(4000.0, |f| (f.abs() + 1500.0).max(4000.0)),
                                    ),
                                };
                                let w = crate::cid::CidWorker::spawn(
                                    out_rate,
                                    self.settings.bandwidth_hz.min(out_rate),
                                    centre,
                                    span,
                                    rs,
                                    self.lossless,
                                    self.pending.clone(),
                                );
                                w.offer(std::mem::take(buf));
                                *worker = Some(w);
                            }
                            None => buf.clear(),
                        }
                        self.identification = Some(id);
                    }
                }
            },
            Decoder::Is19 {
                buf,
                demod,
                rx,
                file,
            } => {
                let mut packets = Vec::new();
                match (demod.as_mut(), rx.as_mut()) {
                    (Some(d), Some(r)) => {
                        self.syms.clear();
                        d.process(&self.bb, &mut self.syms);
                        self.soft.clear();
                        self.soft.extend(self.syms.iter().map(|&(_, s)| s));
                        r.push(&self.soft, &mut packets);
                    }
                    _ => {
                        buf.extend_from_slice(&self.bb);
                        if buf.len() >= first_look(out_rate) {
                            let id = identify_in(buf, out_rate, Some(self.settings.bandwidth_hz));
                            let rs = self
                                .settings
                                .symbol_rate
                                .unwrap_or(decsat_modem::is19::SYMBOL_RATE);
                            if !matches!(id.verdict, Verdict::NoSignal) && out_rate / rs >= 2.0 {
                                let mut shifted = std::mem::take(buf);
                                shift(&mut shifted, out_rate, id.center_offset_hz);
                                self.ddc
                                    .set_offset(self.settings.offset_hz + id.center_offset_hz);
                                // The carrier's residual offset from the
                                // fourth-power line: Identify, seeing the
                                // pilots' squared lines, can take it for BPSK
                                // and measure it wrong.
                                let seed = qpsk_offset_hz(&shifted, out_rate, rs) / rs;
                                let mut d = Box::new(PskDemod::new(
                                    out_rate,
                                    rs,
                                    decsat_modem::is19::ROLL_OFF,
                                    decsat_core::Modulation::Qpsk,
                                    seed,
                                ));
                                let mut r = Box::new(decsat_modem::is19::OutboundRx::new(0.0, rs));
                                self.syms.clear();
                                d.process(&shifted, &mut self.syms);
                                self.soft.clear();
                                self.soft.extend(self.syms.iter().map(|&(_, s)| s));
                                r.push(&self.soft, &mut packets);
                                *demod = Some(d);
                                *rx = Some(r);
                            } else {
                                buf.clear();
                            }
                            self.identification = Some(id);
                        }
                    }
                }
                file.follow(&self.settings, self.ddc.offset_hz());
                file.write(&packets);
            }
            Decoder::Beacon { rx, files } => {
                rx.push(&self.bb, self.ddc.offset_hz());
                let tag = format!(
                    "{}-{}",
                    self.settings.name.replace(' ', "_"),
                    crate::rf::freq_tag(self.settings.offset_hz)
                );
                files.follow(self.settings.record, &self.settings.record_dir, &tag);
                let (frames, points) = rx.take_new();
                if files.active() {
                    files.write(&frames, &points, rx.carried_total());
                }
            }
            Decoder::Pool(p, b, i) => {
                p.push_at(&self.bb, self.block_s.0);
                let tag = format!(
                    "{}-{}",
                    self.settings.name.replace(' ', "_"),
                    crate::rf::freq_tag(self.settings.offset_hz)
                );
                let record = self
                    .settings
                    .record
                    .then_some((self.settings.record_dir.as_path(), tag.as_str()));
                b.set_lossless(self.lossless);
                b.push(
                    &self.bb,
                    self.block_s.0,
                    p.view(),
                    self.plans.as_ref(),
                    record,
                );
                p.set_plan_hint(b.plan_hint());
                // A GSE-signalling hub's plans (decsat_gse::inroute), from
                // an IP VFO on its outbound.
                i.set_lossless(self.lossless);
                i.push(
                    &self.bb,
                    self.block_s.0,
                    p.view(),
                    self.plans.as_ref(),
                    record,
                );
            }
            Decoder::Tdma(a) => {
                a.push(&self.bb, self.settings.psk_modulation);
                let tag = format!(
                    "{}-{}",
                    self.settings.name.replace(' ', "_"),
                    crate::rf::freq_tag(self.settings.offset_hz)
                );
                a.set_recording(self.settings.record, &self.settings.record_dir, &tag);
            }
            Decoder::Psk {
                buf,
                demod,
                writer,
                path,
                written,
                open_failed,
                carrier_hz,
                text,
                pack,
                fec,
            } => match demod {
                Some(d) => {
                    self.syms.clear();
                    d.process(&self.bb, &mut self.syms);
                    if let Some(w) = fec {
                        offer_code_search(w, d.modulation(), &self.syms);
                    }
                    let file = PskFile {
                        writer,
                        path,
                        written,
                        open_failed,
                        pack,
                    };
                    file.follow(&self.settings, d, *carrier_hz);
                    let table = d.label_table(self.settings.symbol_labels);
                    write_symbols(
                        writer,
                        written,
                        &self.syms,
                        &table,
                        pack,
                        &mut self.sym_bytes,
                    );
                    match (self.settings.find_text, text.as_mut()) {
                        (true, Some(t)) => t.push(&self.syms),
                        (true, None) => {
                            let mut t = Box::new(crate::psk::TextSearch::new(d.modulation()));
                            t.push(&self.syms);
                            *text = Some(t);
                        }
                        (false, _) => *text = None,
                    }
                }
                None => {
                    buf.extend_from_slice(&self.bb);
                    if buf.len() >= first_look(out_rate) {
                        let id = identify_in(buf, out_rate, Some(self.settings.bandwidth_hz));
                        let rs = self.settings.symbol_rate.or(id.symbol_rate);
                        let alpha = id.roll_off.map_or(0.35, |r| r.as_f64());
                        let usable = !matches!(id.verdict, Verdict::NoSignal);
                        if let Some(rs) = rs.filter(|&r| usable && out_rate / r >= 2.0) {
                            let modulation = self
                                .settings
                                .psk_modulation
                                .or(id.constellation.map(|c| c.modulation()))
                                .unwrap_or(decsat_core::Modulation::Qpsk);
                            // Identify's residual offset seeds the carrier loop.
                            let seed = id.carrier_offset_hz.unwrap_or(0.0) / rs;
                            let mut d =
                                Box::new(PskDemod::new(out_rate, rs, alpha, modulation, seed));
                            // Centre the carrier, as for DVB-S2 above.
                            let mut shifted = std::mem::take(buf);
                            shift(&mut shifted, out_rate, id.center_offset_hz);
                            self.ddc
                                .set_offset(self.settings.offset_hz + id.center_offset_hz);

                            *carrier_hz = self.settings.offset_hz + id.center_offset_hz;

                            self.syms.clear();
                            d.process(&shifted, &mut self.syms);
                            // BPSK and QPSK: look for a convolutional code.
                            if matches!(
                                modulation,
                                decsat_core::Modulation::Bpsk | decsat_core::Modulation::Qpsk
                            ) {
                                let w = FecWorker::spawn(
                                    rs,
                                    fec_output(&self.settings, &self.ddc, &self.plans),
                                    self.pending.clone(),
                                )
                                .lossless(self.lossless);
                                offer_code_search(&w, modulation, &self.syms);
                                *fec = Some(w);
                            }
                            // Recording armed before lock starts at once.
                            let file = PskFile {
                                writer,
                                path,
                                written,
                                open_failed,
                                pack,
                            };
                            file.follow(&self.settings, &d, *carrier_hz);
                            let table = d.label_table(self.settings.symbol_labels);
                            write_symbols(
                                writer,
                                written,
                                &self.syms,
                                &table,
                                pack,
                                &mut self.sym_bytes,
                            );
                            *demod = Some(d);
                        } else {
                            buf.clear();
                        }
                        self.identification = Some(id);
                    }
                }
            },
        }
    }

    fn publish(&mut self, new_samples: usize) {
        let out_rate = self.ddc.out_rate();
        let spectrum_db = if self.bb.len() >= 1024 {
            Some(self.spec.compute_max(&self.bb, 16))
        } else {
            None
        };
        let level_db = if self.bb.is_empty() {
            None
        } else {
            let p = self.bb.iter().map(|s| s.norm_sqr()).sum::<f32>() / self.bb.len() as f32;
            Some(10.0 * p.max(1e-20).log10())
        };

        let mut st = self.status.lock().unwrap();
        st.out_rate = out_rate;
        st.decimation = self.ddc.decimation();
        st.vfo_offset_hz = self.settings.offset_hz;
        st.follow_hz = self.followed;
        st.out_of_span = out_of_span(&self.settings, self.in_rate);
        st.load = self.load;
        st.dropped = self.dropped.load(Ordering::Relaxed);
        if let Some(s) = spectrum_db {
            st.spectrum_db = s;
        }
        if let Some(l) = level_db {
            st.level_db = l;
        }
        st.identification = self.identification.clone();
        st.chain = None;
        st.cdm_model = None;
        st.modcods = self.modcods.clone();
        st.last_modcod = self.last_modcod;
        st.progress = 0.0;
        st.lock = None;
        st.recording = None;
        st.recording_active = false;
        st.carrier = None;
        st.fec = None;
        st.tdma = None;
        st.is19 = None;
        st.beacon = None;

        match &self.decoder {
            Decoder::Identify {
                buf,
                target,
                provisional,
                live,
                ..
            } => {
                st.progress = buf.len() as f32 / (*target).max(1) as f32;
                st.provisional = *provisional;
                st.message = match &self.identification {
                    Some(id) if *provisional => format!(
                        "{} — provisional: listening longer for frame headers",
                        id.summary()
                    ),
                    Some(id) => id.summary(),
                    None => "listening…".into(),
                };
                st.symbol_rate = self.identification.as_ref().and_then(|i| i.symbol_rate);
                // Live carrier-locked symbols once the live view has some;
                // before that the last identification's; raw baseband only
                // until there is one.
                st.text = live
                    .as_ref()
                    .and_then(|l| l.text.as_ref())
                    .map(|t| t.view());
                let live = live.as_ref().filter(|l| l.demod.symbols() >= 500);
                if let Some(l) = live {
                    st.scatter = l.demod.recent();
                    st.carrier = Some(CarrierState {
                        locked: l.demod.locked(),
                        mer_db: l.demod.mer_db(),
                        offset_hz: l.demod.carrier_offset_hz(),
                        modulation: l.demod.modulation(),
                        info_bits: None,
                        bursts: l.demod.blanked().0,
                    });
                } else {
                    match self
                        .identification
                        .as_ref()
                        .filter(|i| !i.symbols.is_empty())
                    {
                        Some(id) => st.scatter = id.symbols.clone(),
                        None if new_samples > 0 => {
                            let stride = (self.bb.len() / 2000).max(1);
                            st.scatter = self.bb.iter().step_by(stride).copied().collect();
                        }
                        None => {}
                    }
                }
            }
            Decoder::Spectrum => {
                st.message = format!("{:.1} dB", st.level_db);
                if new_samples > 0 {
                    let stride = (self.bb.len() / 2000).max(1);
                    st.scatter = self.bb.iter().step_by(stride).copied().collect();
                }
            }
            Decoder::Record {
                writer,
                path,
                bytes,
            } => {
                st.recording = Some((path.clone(), *bytes));
                st.recording_active = writer.is_some();
                st.message = if writer.is_some() {
                    format!("recording {:.1} MB", *bytes as f64 / 1e6)
                } else {
                    format!("cannot write {}", path.display())
                };
            }
            Decoder::Dvbs2 { buf, demod, fec } => match demod {
                None => {
                    st.progress = buf.len() as f32 / first_look(out_rate) as f32;
                    st.message = match &self.identification {
                        Some(id) if matches!(id.verdict, Verdict::NoSignal) => {
                            "no signal in this VFO".into()
                        }
                        Some(id) => format!("no symbol rate yet — {}", id.summary()),
                        None => "acquiring: finding the symbol rate…".into(),
                    };
                }
                Some(d) => {
                    st.lock = Some(d.lock_state());
                    st.frames = d.frames();
                    st.lock_losses = d.losses();
                    st.symbol_rate = Some(d.symbol_rate());
                    st.scatter = d.recent_symbols(2000);
                    if let (true, Some(m)) = (d.carrier_running(), d.modulation()) {
                        // The last MODCOD's LDPC rate (BCH, header and pilots
                        // left out: a few tenths of a dB).
                        let rate = self
                            .last_modcod
                            .and_then(|i| decsat_core::modcod(i, decsat_core::FecFrame::Normal))
                            .map(|mc| mc.rate.as_f64() as f32);
                        st.carrier = Some(CarrierState {
                            locked: d.carrier_locked(),
                            mer_db: d.es_n0_db(),
                            offset_hz: d.carrier_offset_hz(),
                            modulation: m,
                            info_bits: rate.map(|r| r * f32::from(m.bits_per_symbol())),
                            bursts: 0,
                        });
                    }
                    st.gold_code = Some(d.gold_code());
                    st.superframes = Some(d.superframes());
                    let fec = fec.as_ref().map(|w| w.stats());
                    st.message = match (d.lock_state(), &fec) {
                        (LockState::Searching, _) => "searching for PLHEADERs".into(),
                        (LockState::Found, _) => "found a PLHEADER, confirming".into(),
                        (LockState::Locked, Some(f)) if f.frames > 0 => {
                            let what = match (&f.gse, &f.ts, self.settings.decoder) {
                                (Some(g), _, DecoderKind::Dvbs2Ip) => {
                                    // No IP: name what the GSE carries.
                                    match g.other_protocols.iter().max_by_key(|e| *e.1) {
                                        Some((t, n)) if g.packets == 0 => format!(
                                            "0 IP packets · GSE protocol type {t:04x}h ×{n}{}",
                                            if g.other_encrypted { ", encrypted" } else { "" }
                                        ),
                                        _ => format!("{} IP packets", g.packets),
                                    }
                                }
                                (_, Some(t), DecoderKind::Dvbs2Ts) => {
                                    match t.report.programmes.iter().find_map(|p| p.name.clone()) {
                                        Some(n) => format!("{} TS packets · {n}", t.packets),
                                        None => format!("{} TS packets", t.packets),
                                    }
                                }
                                _ if f.ts_frames > 0 => {
                                    format!("{} TS frames — use the MPEG-TS decoder", f.ts_frames)
                                }
                                _ => "no stream data yet".into(),
                            };
                            format!("locked · {} of {} BBFRAMEs good · {what}", f.ok, f.frames)
                        }
                        (LockState::Locked, _) => match &st.carrier {
                            Some(c) => format!("locked, {} frames, {}", d.frames(), c.snr_text()),
                            None => format!("locked, {} frames", d.frames()),
                        },
                    };
                    st.fec = fec;
                }
            },
            Decoder::Modem {
                kind,
                buf,
                demod,
                fec,
                auto,
            } => match demod {
                None => {
                    st.progress = buf.len() as f32 / first_look(out_rate) as f32;
                    st.message = match &self.identification {
                        Some(id) if matches!(id.verdict, Verdict::NoSignal) => {
                            "no signal in this VFO".into()
                        }
                        Some(id) => format!("no symbol rate yet — {}", id.summary()),
                        None => "acquiring: finding the symbol rate…".into(),
                    };
                }
                Some(d) => {
                    st.symbol_rate = Some(d.symbol_rate());
                    st.scatter = d.recent();
                    let f = fec.as_ref().map(|w| w.stats());
                    st.chain = Some(*kind);
                    st.cdm_model = auto.as_ref().map(|_| cdm_model(*kind, f.as_ref()));
                    let carrier = CarrierState {
                        locked: d.locked(),
                        mer_db: d.mer_db(),
                        offset_hz: d.carrier_offset_hz(),
                        modulation: d.modulation(),
                        info_bits: modem_info_bits(kind.chain(), d.modulation(), f.as_ref()),
                        bursts: d.blanked().0,
                    };
                    st.carrier = Some(carrier);
                    let lock = format!(
                        "{} {}, {}",
                        d.modulation().name(),
                        if d.locked() { "locked" } else { "not locked" },
                        carrier.snr_text()
                    );
                    st.message = if *kind == DecoderKind::Tpc2964 {
                        tpc_message(&lock, f.as_ref())
                    } else if *kind == DecoderKind::FastLink {
                        fastlink_message(&lock, f.as_ref())
                    } else if *kind == DecoderKind::Viterbi {
                        viterbi_message(&lock, f.as_ref())
                    } else {
                        match f.as_ref().and_then(|f| f.dvbs.as_ref().map(|v| (v, &f.ts))) {
                            Some((v, ts)) if v.rate.is_some() => {
                                let rate = v.rate.map_or("?", |r| r.name());
                                match ts.as_ref().and_then(|t| {
                                    t.report.programmes.iter().find_map(|p| p.name.clone())
                                }) {
                                    Some(n) => {
                                        format!(
                                            "{lock} · rate {rate} · {} TS packets · {n}",
                                            v.packets
                                        )
                                    }
                                    None => {
                                        format!("{lock} · rate {rate} · {} TS packets", v.packets)
                                    }
                                }
                            }
                            _ => format!("{lock} · finding the code rate"),
                        }
                    };
                    st.fec = f;
                }
            },
            Decoder::Cid { buf, worker } => match worker {
                None => {
                    st.progress = buf.len() as f32 / first_look(out_rate) as f32;
                    st.message = match &self.identification {
                        Some(id) if matches!(id.verdict, Verdict::NoSignal) => {
                            "no signal in this VFO".into()
                        }
                        Some(id) => format!("no symbol rate yet — {}", id.summary()),
                        None => "measuring the host carrier…".into(),
                    };
                }
                Some(w) => {
                    let v = w.view();
                    st.symbol_rate = Some(v.host_symbol_rate);
                    st.message = cid_message(&v);
                    st.cid = Some(v);
                }
            },
            Decoder::Beacon { rx, files } => {
                let v = rx.view();
                st.progress = if rx.progress() < 1.0 {
                    rx.progress()
                } else {
                    0.0
                };
                st.scatter = rx.scatter();
                st.symbol_rate = v.telemetry.as_ref().map(|t| t.bit_rate);
                if let Some(p) = &files.path {
                    st.recording = Some((p.clone(), files.frames));
                }
                st.recording_active = files.active();
                st.message = match (&files.path, files.active(), files.open_failed) {
                    (Some(p), _, true) => format!("{} — cannot write {}", v.message, p.display()),
                    (_, true, _) => format!("{} — recording, {} frames", v.message, files.frames),
                    _ => v.message.clone(),
                };
                st.beacon = Some(v);
            }
            Decoder::Pool(p, b, i) => {
                let mut v = p.view().clone();
                let bursts = b.view();
                let inroutes = i.has_plans().then(|| i.view());
                st.message = match (v.slot_s, v.frame_s) {
                    _ if v.seconds < 1.0 => "listening…".into(),
                    _ if let Some(ib) = inroutes.as_ref().filter(|ib| ib.fit.is_some()) => format!(
                        "{} inroutes · {:.0} bursts/s demodulated ({} of {} planned) · {} terminals",
                        ib.carriers.len(),
                        ib.per_s,
                        ib.demodulated,
                        ib.planned,
                        ib.terminals.len()
                    ),
                    _ if bursts.timing.is_some() => format!(
                        "{} carriers · {:.0} bursts/s demodulated ({} of {} planned)",
                        v.carriers.len(),
                        bursts.per_s,
                        bursts.demodulated,
                        bursts.planned
                    ),
                    (Some(_), Some(f)) => match v.grid {
                        Some(g) => format!(
                            "{} carriers · {} slots of {:.3} ms in a {:.3} ms frame",
                            v.carriers.len(),
                            g.slots,
                            g.pitch_s * 1e3,
                            f * 1e3
                        ),
                        None => format!("{} carriers · {:.3} ms frame", v.carriers.len(), f * 1e3),
                    },
                    (Some(s), None) => format!(
                        "{} carriers · repeats every {:.3} ms",
                        v.carriers.len(),
                        s * 1e3
                    ),
                    _ => format!("{} carriers · finding the grid…", v.carriers.len()),
                };
                if let Some((p, n)) = &bursts.recording {
                    st.recording = Some((p.clone(), *n));
                }
                if let Some((p, n)) = inroutes.as_ref().and_then(|ib| ib.recording.as_ref()) {
                    st.recording = Some((p.clone(), *n));
                }
                st.recording_active = bursts.recording.is_some()
                    || inroutes.as_ref().is_some_and(|ib| ib.recording.is_some());
                v.bursts = Some(Arc::new(bursts));
                v.inroutes = inroutes.map(Arc::new);
                st.pool = Some(v);
            }
            Decoder::Tdma(a) => {
                let v = a.view();
                st.progress = if v.progress < 1.0 { v.progress } else { 0.0 };
                st.message = match a.recording() {
                    Some((p, _, _)) if a.open_failed() => {
                        format!("{} — cannot write {}", v.message, p.display())
                    }
                    Some((_, n, true)) => format!("{} — recording, {n} bursts", v.message),
                    _ => v.message.clone(),
                };
                if let Some((p, n, active)) = a.recording() {
                    st.recording = Some((p, n));
                    st.recording_active = active;
                }
                st.scatter = a.scatter();
                st.symbol_rate = v.channels.first().map(|c| c.symbol_rate);
                st.tdma = Some(v);
            }
            Decoder::Is19 {
                buf,
                demod,
                rx,
                file,
            } => match (demod, rx) {
                (Some(d), Some(r)) => {
                    st.symbol_rate = Some(d.symbol_rate());
                    st.scatter = d.recent();
                    let carrier = CarrierState {
                        locked: d.locked(),
                        mer_db: d.mer_db(),
                        offset_hz: d.carrier_offset_hz(),
                        modulation: d.modulation(),
                        // 52 × 960 information bits a frame of 40 112
                        // symbols (the code, sync word and pilots on top).
                        info_bits: Some(
                            decsat_modem::is19::FRAME_INFO_BITS as f32
                                / decsat_modem::is19::FRAME_SYMBOLS as f32,
                        ),
                        bursts: d.blanked().0,
                    };
                    st.carrier = Some(carrier);
                    let v = r.view();
                    st.frames = v.stats.framer.frames;
                    st.lock_losses = v.stats.framer.acquisitions.saturating_sub(1);
                    if let Some(p) = &file.path {
                        st.recording = Some((p.clone(), file.written));
                    }
                    st.recording_active = file.writer.is_some();
                    let lock = format!(
                        "QPSK {}, {}",
                        if d.locked() { "locked" } else { "not locked" },
                        carrier.snr_text()
                    );
                    st.message = is19_message(&lock, &v, file);
                    st.is19 = Some(v);
                }
                _ => {
                    st.progress = buf.len() as f32 / first_look(out_rate) as f32;
                    st.message = match &self.identification {
                        Some(id) if matches!(id.verdict, Verdict::NoSignal) => {
                            "no signal in this VFO".into()
                        }
                        _ => "acquiring: centring the carrier…".into(),
                    };
                }
            },
            Decoder::Psk {
                buf,
                demod,
                writer,
                path,
                written,
                open_failed,
                text,
                fec,
                ..
            } => match demod {
                None => {
                    st.progress = buf.len() as f32 / first_look(out_rate) as f32;
                    st.message = match &self.identification {
                        Some(id) if matches!(id.verdict, Verdict::NoSignal) => {
                            "no signal in this VFO".into()
                        }
                        Some(id) => format!("no symbol rate yet — {}", id.summary()),
                        None => "acquiring: finding the symbol rate…".into(),
                    };
                }
                Some(d) => {
                    st.symbol_rate = Some(d.symbol_rate());
                    st.scatter = d.recent();
                    let carrier = CarrierState {
                        locked: d.locked(),
                        mer_db: d.mer_db(),
                        offset_hz: d.carrier_offset_hz(),
                        modulation: d.modulation(),
                        info_bits: None,
                        bursts: d.blanked().0,
                    };
                    st.carrier = Some(carrier);
                    if let Some(p) = path {
                        st.recording = Some((p.clone(), *written));
                    }
                    st.recording_active = writer.is_some();
                    st.text = text.as_ref().map(|t| t.view());
                    st.fec = fec.as_ref().map(|w| w.stats());
                    let lock = format!(
                        "{} {}, {}{}",
                        d.modulation().name(),
                        if d.locked() { "locked" } else { "not locked" },
                        carrier.snr_text(),
                        match &st.fec {
                            Some(f) => format!(" · {}", code_text(Some(f))),
                            None => String::new(),
                        }
                    );
                    st.message = match (writer.is_some(), *open_failed, path) {
                        (_, true, Some(p)) => format!("{lock} — cannot write {}", p.display()),
                        (true, _, _) => format!("{lock} — recording, {written} symbols"),
                        _ => format!("{lock} — press Record to write symbols"),
                    };
                }
            },
        }
    }
}

/// Information bits a symbol of a coded modem carries, when its code is
/// known: TPC 2964's 2223 data bits a 2964-bit frame (UW included), or
/// Comtech TPC 7/8's 14 280 a 16 320-bit one once its UW is found;
/// FastLink's data bits a frame of symbols; a convolutional code's rate (and
/// DVB-S's Reed–Solomon 188/204, EN 300 421 §4.4.2) once found.
fn modem_info_bits(
    kind: DecoderKind,
    m: decsat_core::Modulation,
    f: Option<&FecStats>,
) -> Option<f32> {
    use decsat_modem::{fastlink, tpc2964};
    let bits = f32::from(m.bits_per_symbol());
    match kind {
        DecoderKind::Tpc2964 => {
            let code = f
                .and_then(|f| f.tpc.as_ref())
                .and_then(|t| t.code)
                .unwrap_or(tpc2964::Code::Tpc2964);
            Some(bits * code.data() as f32 / code.frame() as f32)
        }
        DecoderKind::FastLink => Some(fastlink::FRAME_DATA as f32 / fastlink::FRAME_SYMBOLS as f32),
        DecoderKind::Viterbi => f?.viterbi.as_ref()?.rate.map(|r| bits * r.value() as f32),
        DecoderKind::DvbsTs => f?
            .dvbs
            .as_ref()?
            .rate
            .map(|r| bits * r.value() as f32 * 188.0 / 204.0),
        _ => None,
    }
}

/// What a DVB-S2 VFO's FEC thread should write. The DDC is tuned onto the
/// carrier by then, so its offset names the file.
/// Hand a coded modem's symbols to its FEC thread.
fn offer_symbols(
    w: &FecWorker,
    kind: DecoderKind,
    modulation: decsat_core::Modulation,
    syms: &[(u8, Iq)],
) {
    let s = syms.iter().map(|s| s.1).collect();
    match kind {
        DecoderKind::Tpc2964 => w.offer_tpc(s, modulation),
        DecoderKind::FastLink => w.offer_fastlink(s),
        DecoderKind::Viterbi => {
            w.offer_viterbi(s, modulation == decsat_core::Modulation::Bpsk, false)
        }
        _ => w.offer_dvbs(s),
    }
}

/// The generic PSK decoder's look for a convolutional code: its symbols to
/// the Viterbi chain's search, bounded.
fn offer_code_search(w: &FecWorker, modulation: decsat_core::Modulation, syms: &[(u8, Iq)]) {
    let s = syms.iter().map(|s| s.1).collect();
    w.offer_viterbi(s, modulation == decsat_core::Modulation::Bpsk, true);
}

/// A Viterbi VFO's status line.
fn viterbi_message(lock: &str, f: Option<&crate::fec::FecStats>) -> String {
    format!("{lock} · {}", code_text(f))
}

/// A payload label's format: up to its first comma outside brackets (the
/// scrambling follows it; "IBS 3 × 64 kbit/s (G.704-shaped framing,
/// IESS-309)" keeps its bracket whole).
fn payload_format(found: &str) -> &str {
    let mut depth = 0i32;
    for (i, c) in found.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => return &found[..i],
            _ => {}
        }
    }
    found
}

/// What the convolutional-code search has found (the Viterbi chain's, or
/// the generic PSK decoder's): the code and what its data carry, or how
/// the search is going, or that there is no such code.
fn code_text(f: Option<&crate::fec::FecStats>) -> String {
    let payload = || {
        f.and_then(|f| f.payload.as_ref())
            .and_then(|p| p.found.as_deref())
            .map_or("payload not recognised yet".to_string(), |p| {
                payload_format(p).to_string()
            })
    };
    let k7 = f.and_then(|f| f.viterbi.as_ref());
    let blind = f.and_then(|f| f.conv.as_ref());
    if let Some(rate) = k7.and_then(|v| v.rate) {
        let ber = k7.map_or(0.0, |v| v.channel_ber);
        return format!("K = 7 rate {} · BER {ber:.1e} · {}", rate.name(), payload());
    }
    if let Some((code, c)) = blind.and_then(|c| c.code.map(|code| (code, c))) {
        return format!(
            "{} · BER {:.1e} · {}",
            code.name(),
            c.channel_ber,
            payload()
        );
    }
    let k7_done = f.is_some_and(|f| f.k7_stopped) || k7.is_none();
    match (blind, k7) {
        (Some(c), _) if c.gave_up && k7_done => "no convolutional code: not K = 7 at any rate, \
             nor a rate-½ code up to K = 10 — a block code (TPC, LDPC, turbo) or uncoded"
            .into(),
        (Some(c), Some(v)) if c.gave_up => format!(
            "finding the K = 7 code rate ({} tries) · no other rate-½ code",
            v.searches
        ),
        (Some(c), Some(v)) => format!(
            "finding the code: K = 7 at every rate ({} tries), rate-½ codes ({} looks)",
            v.searches, c.searches
        ),
        (Some(c), None) => format!("finding a rate-½ code ({} looks)", c.searches),
        _ => "finding the code".into(),
    }
}

/// A FastLink VFO's status line.
fn fastlink_message(lock: &str, f: Option<&crate::fec::FecStats>) -> String {
    let Some(t) = f.and_then(|f| f.fastlink.as_ref()).filter(|t| t.locked) else {
        return format!("{lock} · looking for the FastLink sync word");
    };
    let payload = f
        .and_then(|f| f.payload.as_ref())
        .and_then(|p| p.found.as_deref())
        .map_or("payload not recognised yet".to_string(), |p| {
            payload_format(p).to_string()
        });
    format!(
        "{lock} · {} frames · {} codewords ({} failed) · BER {:.1e} · {payload}",
        t.frames,
        t.codewords,
        t.failed,
        t.channel_ber()
    )
}

/// The Intelsat 19 outbound's packet file, written while Record is on: one
/// line a packet (time, kind, type, length, terminal field, hex).
#[derive(Default)]
struct Is19File {
    writer: Option<BufWriter<File>>,
    path: Option<PathBuf>,
    /// Packets written.
    written: u64,
    open_failed: bool,
}

impl Is19File {
    /// Open or close the file as the Record switch says.
    fn follow(&mut self, settings: &VfoSettings, carrier_hz: f64) {
        match (settings.record, self.writer.is_some()) {
            (true, false) if !self.open_failed => {
                let p = settings.record_dir.join(format!(
                    "decsat-{}-{}-is19-{}.csv",
                    settings.name.replace(' ', "_"),
                    crate::rf::freq_tag(carrier_hz),
                    unix_stamp()
                ));
                match File::create(&p) {
                    Ok(f) => {
                        let mut w = BufWriter::new(f);
                        let _ = writeln!(
                            w,
                            "t_s,kind,type,bytes,terminal,flags,end,call,tx_channel,rx_channel,hex"
                        );
                        self.writer = Some(w);
                        self.written = 0;
                    }
                    Err(_) => self.open_failed = true,
                }
                self.path = Some(p);
            }
            (false, true) => {
                if let Some(mut w) = self.writer.take() {
                    let _ = w.flush();
                }
            }
            (false, false) => self.open_failed = false,
            _ => {}
        }
    }

    fn write(&mut self, packets: &[decsat_modem::is19::Packet]) {
        let Some(w) = self.writer.as_mut() else {
            return;
        };
        for p in packets {
            let ok = writeln!(
                w,
                "{:.4},{},{},{},{},{},{},{},{},{},{}",
                p.t_s,
                p.kind.name(),
                p.type_code.map_or(String::new(), |t| format!("{t:02x}")),
                p.bytes(),
                p.terminal.map_or(String::new(), |t| format!("{t:08x}")),
                p.flags.map_or(String::new(), |f| format!("{f:02x}")),
                p.assignment.map_or("", |a| a.end.name()),
                p.assignment
                    .map_or(String::new(), |a| a.call_ref.to_string()),
                p.assignment
                    .and_then(|a| a.tx)
                    .map_or(String::new(), |c| c.to_string()),
                p.assignment
                    .and_then(|a| a.rx)
                    .map_or(String::new(), |c| c.to_string()),
                p.hex()
            );
            if ok.is_err() {
                self.writer = None;
                self.open_failed = true;
                return;
            }
            self.written += 1;
        }
    }
}

/// A QPSK carrier's offset from DC, Hz, from the line its fourth power puts
/// at four times it (searched over a tenth of the symbol rate either side).
fn qpsk_offset_hz(x: &[Iq], rate: f64, rs: f64) -> f64 {
    let p4: Vec<Iq> = x
        .iter()
        .map(|s| {
            let s2 = s * s;
            s2 * s2
        })
        .collect();
    let span = 4.0 * 0.1 * rs;
    crate::identify::strongest_line(&p4, rate, -span, span).map_or(0.0, |(f, _)| f / 4.0)
}

/// An Intelsat 19 outbound VFO's status line.
fn is19_message(lock: &str, v: &decsat_modem::is19::OutboundView, file: &Is19File) -> String {
    use decsat_modem::is19::PacketKind;
    let s = &v.stats;
    if s.framer.frames == 0 {
        return format!("{lock} · looking for the frame sync word");
    }
    let calls = s.count(PacketKind::Assignment);
    let rec = match (&file.path, file.writer.is_some(), file.open_failed) {
        (Some(p), _, true) => format!(" — cannot write {}", p.display()),
        (_, true, _) => format!(" — recording, {} packets", file.written),
        _ => String::new(),
    };
    format!(
        "{lock} · {} frames · load {:.0} % · {} traffic packets · {} request acks · {} assignments{}{rec}",
        s.framer.frames,
        100.0 * s.load(),
        s.count(PacketKind::Traffic),
        s.count(PacketKind::RequestAck),
        calls,
        if s.framer.locked { "" } else { " · sync lost" },
    )
}

/// A DVB-CID VFO's status line.
fn cid_message(v: &crate::cid::CidView) -> String {
    let s = &v.stats;
    let rate = format!("{:.0} kchip/s", v.chip_rate / 1e3);
    if !s.acquired {
        return format!("looking for a DVB-CID at {rate} ({} searches)", s.searches);
    }
    match s.report.guid {
        Some(g) => format!(
            "DVB-CID {} · {} frames · {:+.0} Hz · {:.1} dB",
            decsat_modem::cid::guid_text(g),
            s.frames,
            s.offset_hz,
            s.snr_db
        ),
        None => format!(
            "DVB-CID code found at {:+.0} Hz, {:.1} dB · reading the first frame (~{:.0} s)",
            s.offset_hz,
            s.snr_db,
            976.0 * 4096.0 / v.chip_rate
        ),
    }
}

/// A TPC VFO's status line (TPC 2964 or Comtech TPC 7/8).
fn tpc_message(lock: &str, f: Option<&crate::fec::FecStats>) -> String {
    let Some(t) = f.and_then(|f| f.tpc.as_ref()) else {
        return format!("{lock} · looking for the unique word");
    };
    if !t.uw_locked {
        return format!("{lock} · looking for the unique word");
    }
    if t.structure.is_none() {
        return format!(
            "{lock} · UW found · identifying the frame structure (best fit {:.0}%)",
            t.fit * 100.0
        );
    }
    let payload = f
        .and_then(|f| f.payload.as_ref())
        .and_then(|p| p.found.as_deref())
        .map_or("payload not recognised yet".to_string(), |p| {
            payload_format(p).to_string()
        });
    // Comtech's code is named; TPC 2964 is the one the decoder is known by.
    let code = match t.code {
        Some(decsat_modem::tpc2964::Code::Comtech78) => "TPC 7/8 · ",
        _ => "",
    };
    format!(
        "{lock} · {code}{} frames ({} failed) · BER {:.1e} · {payload}",
        t.frames,
        t.failed,
        t.channel_ber()
    )
}

fn fec_output(s: &VfoSettings, ddc: &Ddc, plans: &Option<crate::plan_feed::PlanLink>) -> FecOutput {
    FecOutput {
        plan_link: plans.clone().filter(|_| s.decoder.carries_ip()),
        record: s.record
            && matches!(
                s.decoder.chain(),
                DecoderKind::Dvbs2Ip
                    | DecoderKind::Tpc2964
                    | DecoderKind::CdmAuto
                    | DecoderKind::FastLink
                    | DecoderKind::Viterbi
                    | DecoderKind::PskSymbols
            ),
        record_on_activity: s.record_on_activity && s.decoder == DecoderKind::FastLink,
        pcap: s.pcap && s.decoder.carries_ip(),
        wireshark: s.wireshark.filter(|_| s.decoder.carries_ip()),
        dir: s.record_dir.clone(),
        name: s.name.clone(),
        carrier_hz: ddc.offset_hz(),
        gse_variant: s.gse_variant,
        ts: s.decoder.outputs_ts(),
        ts_record: s.record && s.decoder.outputs_ts(),
        ts_udp: s.ts_udp_on.then(|| s.ts_udp.trim().parse().ok()).flatten(),
        ts_tcp: s.ts_tcp_on.then(|| s.ts_tcp.trim().parse().ok()).flatten(),
        // An IP decoder's transport stream is one carried in UDP.
        biss: decsat_ts::BissKey::parse(&s.biss_key)
            .ok()
            .map(|k| k.cw)
            .filter(|_| s.decoder.carries_ip()),
        ts_tap: s.ts_tap.clone().filter(|_| s.decoder.carries_ip()),
        ts_isi: s.ts_isi,
        udp_ts: s
            .udp_ts
            .filter(|_| s.decoder.chain() == DecoderKind::Dvbs2Ip),
        audio_play: s.audio_play,
        audio_external: s.audio_external,
        audio_record: s.audio_record,
        e1_play: s.e1_play,
        e1_record: s.e1_record,
        tdm_play: s.tdm_play,
        tdm_record: s.tdm_record,
        programme_play: s.programme_play,
        programme_record: s.programme_record,
        voice_auto: s.voice_auto && s.decoder.plays_voice(),
        bbframes: s.bbframes.clone(),
    }
}

/// Identify's live view: the last identification's symbol rate,
/// constellation and carrier offset driving a generic demodulator between
/// identifications, so the constellation stays live instead of freezing on
/// the last snapshot. Identify does not retune the VFO, so the view mixes
/// the carrier to DC itself.
struct LiveView {
    demod: Box<PskDemod>,
    rs: f64,
    modulation: decsat_core::Modulation,
    center_hz: f64,
    /// Mixer phase and step, radians (per sample).
    phase: f64,
    step: f64,
    mixed: Vec<Iq>,
    /// Text in the decided bits, while asked for.
    text: Option<Box<crate::psk::TextSearch>>,
}

impl LiveView {
    /// A view for `id`, if it found a carrier with a usable symbol rate.
    fn new(id: &Identification, rate: f64) -> Option<Self> {
        if matches!(id.verdict, Verdict::NoSignal) {
            return None;
        }
        let rs = id.symbol_rate.filter(|&r| rate / r >= 2.0)?;
        let alpha = id.roll_off.map_or(0.35, |r| r.as_f64());
        let modulation = live_modulation(id);
        let seed = id.carrier_offset_hz.unwrap_or(0.0) / rs;
        Some(LiveView {
            demod: Box::new(PskDemod::new(rate, rs, alpha, modulation, seed)),
            rs,
            modulation,
            center_hz: id.center_offset_hz,
            phase: 0.0,
            step: -std::f64::consts::TAU * id.center_offset_hz / rate,
            mixed: Vec::new(),
            text: None,
        })
    }

    /// The new identification describes the carrier this view already
    /// follows (so it keeps running rather than re-acquiring).
    fn same_carrier(&self, id: &Identification) -> bool {
        id.symbol_rate
            .is_some_and(|r| (r - self.rs).abs() < self.rs * 0.002)
            && live_modulation(id) == self.modulation
            && (id.center_offset_hz - self.center_hz).abs() < self.rs * 0.05
    }

    fn process(&mut self, bb: &[Iq], scratch: &mut Vec<(u8, Iq)>) {
        self.mixed.clear();
        self.mixed.extend(bb.iter().map(|&x| {
            let v = x * Iq::new(self.phase.cos() as f32, self.phase.sin() as f32);
            self.phase = (self.phase + self.step) % std::f64::consts::TAU;
            v
        }));
        scratch.clear();
        self.demod.process(&self.mixed, scratch);
    }
}

/// The constellation to steer Identify's live view with: for DVB-S2 the most
/// common MODCOD's (every S2 constellation shares QPSK's 90° symmetry, so
/// even ACM stays locked), else Identify's estimate.
fn live_modulation(id: &Identification) -> decsat_core::Modulation {
    match &id.verdict {
        Verdict::DvbS2(d) => d
            .modcods
            .iter()
            .max_by_key(|(_, n)| **n)
            .and_then(|(&m, _)| decsat_core::modcod(m, decsat_core::FecFrame::Normal))
            .map_or(decsat_core::Modulation::Qpsk, |mc| mc.modulation),
        _ => id
            .constellation
            .map_or(decsat_core::Modulation::Qpsk, |c| c.modulation()),
    }
}

/// The generic PSK decoder's output file, borrowed from its decoder state.
struct PskFile<'a> {
    writer: &'a mut Option<BufWriter<File>>,
    path: &'a mut Option<PathBuf>,
    written: &'a mut u64,
    open_failed: &'a mut bool,
    pack: &'a mut SymbolPacker,
}

impl PskFile<'_> {
    /// Open or close the file to match `settings.record`. A new recording is
    /// a new file; stopping keeps the last path and count for display.
    fn follow(self, settings: &VfoSettings, d: &PskDemod, carrier_hz: f64) {
        match (settings.record, self.writer.is_some()) {
            (true, false) if !*self.open_failed => {
                let p = settings.record_dir.join(format!(
                    "decsat-{}-{}-{:.0}Bd-{}{}-{}.bin",
                    settings.name.replace(' ', "_"),
                    crate::rf::freq_tag(carrier_hz),
                    d.symbol_rate(),
                    d.modulation().name().replace('/', ""),
                    if settings.psk_bits { "-bits" } else { "" },
                    unix_stamp()
                ));
                *self.pack = SymbolPacker {
                    bits: settings.psk_bits,
                    ..SymbolPacker::default()
                };
                let _ = std::fs::create_dir_all(&settings.record_dir);
                *self.writer = File::create(&p).ok().map(BufWriter::new);
                *self.open_failed = self.writer.is_none();
                *self.written = 0;
                *self.path = Some(p);
            }
            (false, true) => {
                if let Some(mut w) = self.writer.take() {
                    let _ = w.flush();
                }
            }
            (false, false) => *self.open_failed = false,
            _ => {}
        }
    }
}

/// Seconds since the Unix epoch, for file names.
pub(crate) fn unix_stamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Append hard decisions to the symbol file: each hard index through
/// `table` (see [`crate::psk::SymbolLabels`]), a byte each or its bits
/// packed (see [`SymbolPacker`]). A write error closes the file rather than
/// retrying every block.
fn write_symbols(
    writer: &mut Option<BufWriter<File>>,
    written: &mut u64,
    syms: &[(u8, Iq)],
    table: &[u8],
    pack: &mut SymbolPacker,
    scratch: &mut Vec<u8>,
) {
    let Some(w) = writer else { return };
    scratch.clear();
    // Bits a symbol: log2 of the points (BPSK 1, QPSK 2, 8PSK 3, …).
    let per = (table.len().max(2) as u32)
        .next_power_of_two()
        .trailing_zeros();
    pack.pack(
        syms.iter()
            .map(|&(i, _)| table.get(i as usize).copied().unwrap_or(i)),
        per,
        scratch,
    );
    if w.write_all(scratch).is_ok() {
        *written += syms.len() as u64;
    } else {
        *writer = None;
    }
}

/// Rotate baseband by `-freq_hz` in place.
fn shift(x: &mut [Iq], rate: f64, freq_hz: f64) {
    if freq_hz == 0.0 {
        return;
    }
    let w = -std::f64::consts::TAU * freq_hz / rate;
    for (n, s) in x.iter_mut().enumerate() {
        let ph = w * n as f64;
        *s *= Iq::new(ph.cos() as f32, ph.sin() as f32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decsat_fec::Constellation;
    use decsat_mod::Shaper;

    /// The CDM type-found model on a TPC 2964 carrier: Identify's unique
    /// word picks TPC 2964, the code locks, and HDLC carrying IP (no
    /// timeslot packets, no D&I) names a CDM-570L's data.
    #[test]
    fn cdm_type_found_runs_tpc_2964_and_names_the_model() {
        let x = tpc_test_carrier();
        let settings = VfoSettings::new("A", 40_000.0, 190_000.0, DecoderKind::CdmAuto);
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(
            500_000.0,
            settings,
            status.clone(),
            Arc::new(AtomicU64::new(0)),
        );
        feed(&mut wk, &x, 65_536);
        let t0 = Instant::now();
        loop {
            wk.publish(0);
            {
                let st = status.lock().unwrap();
                let decoded = st
                    .fec
                    .as_ref()
                    .and_then(|f| f.tpc.as_ref())
                    .is_some_and(|t| t.decoded > 0);
                let found = st
                    .fec
                    .as_ref()
                    .and_then(|f| f.payload.as_ref())
                    .is_some_and(|p| p.found.is_some());
                if decoded && found {
                    assert_eq!(st.chain, Some(DecoderKind::Tpc2964));
                    assert_eq!(st.cdm_model, Some(DecoderKind::Tpc2964));
                    break;
                }
                assert!(
                    t0.elapsed().as_secs() < 30,
                    "not decoded: {} / {:?}",
                    st.message,
                    st.chain
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn cdm_models_from_what_the_data_carry() {
        use crate::fec::FecStats;
        use decsat_modem::payload::PayloadStats;
        let with = |p: PayloadStats| FecStats {
            payload: Some(p),
            ..Default::default()
        };
        assert_eq!(cdm_model(DecoderKind::Viterbi, None), DecoderKind::Cdm570);
        assert_eq!(
            cdm_model(DecoderKind::FastLink, None),
            DecoderKind::FastLink
        );
        assert_eq!(cdm_model(DecoderKind::Tpc2964, None), DecoderKind::Tpc2964);
        let di = with(PayloadStats {
            dandi: Some(Default::default()),
            ..Default::default()
        });
        assert_eq!(
            cdm_model(DecoderKind::Tpc2964, Some(&di)),
            DecoderKind::Cdm600Voice
        );
        // A whole E1: voice. G.704-shaped frames slower than an E1, or under
        // the IBS scrambler: IBS, a CDM-570L's data as far as can be told
        // (an idle 3 × 64k link on Intelsat 19 was badged CDM-600L voice).
        let e1 = |ibs_channels, ibs_sync| {
            with(PayloadStats {
                e1: Some(Default::default()),
                ibs_channels,
                ibs_sync,
                ..Default::default()
            })
        };
        assert_eq!(
            cdm_model(DecoderKind::Tpc2964, Some(&e1(None, None))),
            DecoderKind::Cdm600Voice
        );
        assert_eq!(
            cdm_model(DecoderKind::Tpc2964, Some(&e1(Some(3), None))),
            DecoderKind::Tpc2964
        );
        assert_eq!(
            cdm_model(
                DecoderKind::Tpc2964,
                Some(&e1(None, Some(Default::default())))
            ),
            DecoderKind::Tpc2964
        );
        let slots = |n| {
            with(PayloadStats {
                slots: Some(decsat_modem::slotpkt::SlotStats {
                    slot_packets: n,
                    ..Default::default()
                }),
                ..Default::default()
            })
        };
        assert_eq!(
            cdm_model(DecoderKind::Tpc2964, Some(&slots(40))),
            DecoderKind::Cdm570Trunk
        );
        // Plain HDLC on the inverted line (the 12335.13 MHz trunk's Frame
        // Relay): a CDM-570L's data.
        assert_eq!(
            cdm_model(DecoderKind::Tpc2964, Some(&slots(0))),
            DecoderKind::Tpc2964
        );
    }

    #[test]
    fn a_payload_label_cut_to_its_format() {
        assert_eq!(
            payload_format("IBS 3 × 64 kbit/s (G.704-shaped framing, IESS-309), additive"),
            "IBS 3 × 64 kbit/s (G.704-shaped framing, IESS-309)"
        );
        assert_eq!(
            payload_format("HDLC (FCS-16), not scrambled"),
            "HDLC (FCS-16)"
        );
        assert_eq!(payload_format("MPEG-TS"), "MPEG-TS");
    }

    #[test]
    fn the_cdm_570l_trunk_preset() {
        let k = DecoderKind::Cdm570Trunk;
        // Saved and read back by its own key; run as TPC 2964; a CDM model
        // that plays its voice by itself.
        assert_eq!(DecoderKind::from_key(k.key()), Some(k));
        assert_eq!(k.chain(), DecoderKind::Tpc2964);
        assert!(k.is_cdm() && k.plays_voice() && k.demodulates());
        assert!(!DecoderKind::Tpc2964.plays_voice());
        // Every kind's key is its own.
        let keys: std::collections::HashSet<_> = DecoderKind::ALL.map(|k| k.key()).into();
        assert_eq!(keys.len(), DecoderKind::ALL.len());
    }

    #[test]
    fn symbols_pack_into_bits_msb_first() {
        // QPSK 0, 1, 2, 3 → 00 01 10 11.
        let mut p = SymbolPacker {
            bits: true,
            ..SymbolPacker::default()
        };
        let mut out = Vec::new();
        p.pack([0u8, 1, 2, 3].into_iter(), 2, &mut out);
        assert_eq!(out, [0x1B]);
        // 8PSK 7, 0, 5 → 111 000 10|1: a byte, one bit kept for the next.
        out.clear();
        p.pack([7u8, 0, 5].into_iter(), 3, &mut out);
        assert_eq!(out, [0xE2]);
        p.pack([0u8, 0, 0].into_iter(), 3, &mut out);
        assert_eq!(out, [0xE2, 0x80]);
        // Bytes mode: the labels as they are.
        let mut b = SymbolPacker::default();
        out.clear();
        b.pack([3u8, 1].into_iter(), 2, &mut out);
        assert_eq!(out, [3, 1]);
    }

    /// Drive a worker the way `run` does, without the thread.
    fn feed(w: &mut Worker, x: &[Iq], block: usize) {
        for c in x.chunks(block) {
            w.bb.clear();
            w.ddc.process(c, &mut w.bb);
            let n = w.bb.len();
            w.decode();
            w.follow(n);
            w.publish(n);
        }
    }

    /// QPSK steady for `flat` seconds, then drifting `slope` Hz/s (an LNB
    /// warming up, but faster), `sps` samples a symbol, a little noise.
    fn drifting(n_sym: usize, sps: usize, rs: f64, flat: f64, slope: f64) -> Vec<Iq> {
        let cst = Constellation::qpsk();
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut uniform = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
        };
        let syms: Vec<Iq> = (0..n_sym)
            .map(|_| cst.map((uniform() * 4.0) as usize % 4))
            .collect();
        let mut sh = Shaper::new(sps, 0.25, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let rate = rs * sps as f64;
        let mut ph = 0.0f64;
        for (n, v) in x.iter_mut().enumerate() {
            let t = n as f64 / rate;
            ph += std::f64::consts::TAU * slope * (t - flat).max(0.0) / rate;
            let noise = Iq::new(uniform() as f32 - 0.5, uniform() as f32 - 0.5) * 0.02;
            *v = *v * Iq::new(ph.cos() as f32, ph.sin() as f32) + noise;
        }
        x
    }

    #[test]
    fn the_vfo_follows_a_drifting_carrier() {
        // 20 kBd QPSK in a 40 kHz VFO, steady for 2 s (to lock), then
        // drifting 2.5 kHz/s for 10 s: 25 kHz, past the VFO's edge (20 kHz
        // from its centre).
        let (rs, sps) = (20_000.0, 4);
        let x = drifting(240_000, sps, rs, 2.0, 2_500.0);
        let run = |follow: bool| {
            let mut s = VfoSettings::new("drift", 0.0, 40_000.0, DecoderKind::PskSymbols);
            s.symbol_rate = Some(rs);
            s.psk_modulation = Some(decsat_core::Modulation::Qpsk);
            s.follow_carrier = follow;
            let status = Arc::new(Mutex::new(VfoStatus::default()));
            let mut wk = Worker::new(
                rs * sps as f64,
                s,
                status.clone(),
                Arc::new(AtomicU64::new(0)),
            );
            feed(&mut wk, &x, 16_384);
            let st = status.lock().unwrap().clone();
            (wk, st)
        };
        let (mut wk, st) = run(true);
        let c = st.carrier.expect("demodulator running");
        assert!(c.locked, "lost the drifting carrier: {}", st.message);
        // Followed to where it went (25 kHz), less what the loop holds.
        let at = st.follow_hz + c.offset_hz;
        assert!((at - 25_000.0).abs() < 1_000.0, "followed to {at} Hz");
        // The GUI moving the VFO by what was followed changes nothing that
        // runs: the demodulator carries on, the drift is taken in.
        let mut moved = wk.settings.clone();
        moved.offset_hz += st.follow_hz;
        wk.apply(moved, 0.0);
        assert!(matches!(wk.decoder, Decoder::Psk { demod: Some(_), .. }));
        assert!(wk.followed.abs() < 1.0, "{}", wk.followed);
        // Without following it leaves the VFO.
        let (_, st) = run(false);
        assert!(st.carrier.is_none_or(|c| !c.locked), "{}", st.message);
    }

    /// A carrier of `cst` symbols at `rs`, `sps` samples per symbol (the band
    /// is `rs * sps` wide), `f_hz` off the band centre, with a random-walk
    /// phase noise of `walk` rad per symbol (rms) and a little white noise.
    fn carrier(
        cst: &Constellation,
        n_sym: usize,
        sps: usize,
        rs: f64,
        f_hz: f64,
        walk: f64,
        seed: u64,
    ) -> Vec<Iq> {
        let mut s = seed | 1;
        let mut uniform = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
        };
        let m = cst.points.len();
        let syms: Vec<Iq> = (0..n_sym)
            .map(|_| cst.map((uniform() * m as f64) as usize % m))
            .collect();
        let mut sh = Shaper::new(sps, 0.25, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let w = std::f64::consts::TAU * f_hz / (rs * sps as f64);
        let mut drift = 0.0;
        for (n, v) in x.iter_mut().enumerate() {
            if n % sps == 0 {
                drift += walk * 12f64.sqrt() * (uniform() - 0.5);
            }
            let ph = w * n as f64 + drift;
            let noise = Iq::new(uniform() as f32 - 0.5, uniform() as f32 - 0.5) * 0.02;
            *v = *v * Iq::new(ph.cos() as f32, ph.sin() as f32) + noise;
        }
        x
    }

    /// A modem reconfigured on the fly (a Q-Flex went from 64 to 48 kS/s
    /// mid-recording): the VFO notices and acquires the new rate.
    #[test]
    fn a_carrier_changing_symbol_rate_is_followed() {
        // 80 kS/s: 20 kBd (4 a symbol) for 4 s, then 16 kBd (5 a symbol).
        let cst = Constellation::qpsk();
        let mut x = carrier(&cst, 80_000, 4, 20_000.0, 0.0, 0.0, 21);
        x.extend(carrier(&cst, 112_000, 5, 16_000.0, 0.0, 0.0, 22));
        let mut s = VfoSettings::new("rate", 0.0, 40_000.0, DecoderKind::PskSymbols);
        s.psk_modulation = Some(decsat_core::Modulation::Qpsk);
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(80_000.0, s, status.clone(), Arc::new(AtomicU64::new(0)));
        for c in x.chunks(8_192) {
            wk.bb.clear();
            wk.ddc.process(c, &mut wk.bb);
            let n = wk.bb.len();
            wk.decode();
            wk.follow(n);
            wk.watch_rate();
            wk.publish(n);
        }
        let st = status.lock().unwrap().clone();
        let rs = st.symbol_rate.expect("a symbol rate");
        assert!(
            (rs - 16_000.0).abs() < 100.0,
            "still at {rs}: {}",
            st.message
        );
        assert!(st.carrier.is_some_and(|c| c.locked), "{}", st.message);
    }

    /// Run a generic PSK VFO over `x`; return its status and output folder.
    fn psk_vfo(
        rate: f64,
        x: &[Iq],
        offset_hz: f64,
        bandwidth_hz: f64,
        tag: &str,
    ) -> (VfoStatus, PathBuf) {
        let dir = std::env::temp_dir().join(format!("decsat-{tag}-{}", std::process::id()));
        let mut settings =
            VfoSettings::new("PSK test", offset_hz, bandwidth_hz, DecoderKind::PskSymbols);
        settings.record_dir = dir.clone();
        settings.record = true;
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(rate, settings, status.clone(), Arc::new(AtomicU64::new(0)));
        feed(&mut wk, x, 65_536);
        drop(wk); // flushes the file
        let st = status.lock().unwrap().clone();
        (st, dir)
    }

    #[test]
    fn psk_vfo_locks_16qam_when_told() {
        // Identify reads rings, not grids: a QAM carrier is named by the
        // user, as the RCV-20x's demodulator modes are.
        let rs = 62_500.0;
        let x = carrier(&Constellation::qam16(), 200_000, 8, rs, 40_310.0, 0.0, 9);
        let mut settings = VfoSettings::new("QAM", 38_000.0, 110_000.0, DecoderKind::PskSymbols);
        settings.psk_modulation = Some(decsat_core::Modulation::Qam16);
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(
            500_000.0,
            settings,
            status.clone(),
            Arc::new(AtomicU64::new(0)),
        );
        feed(&mut wk, &x, 65_536);
        drop(wk);
        let st = status.lock().unwrap().clone();
        let c = st.carrier.expect("demodulator running");
        assert_eq!(c.modulation, decsat_core::Modulation::Qam16);
        assert!(c.locked && c.mer_db > 20.0, "{}", st.message);
    }

    #[test]
    fn psk_vfo_locks_and_writes_one_byte_per_symbol() {
        // 8PSK at 62.5 kBd, 40 kHz off the band centre plus 310 Hz the VFO is
        // not told about, in a 500 kS/s band.
        let rs = 62_500.0;
        let x = carrier(&Constellation::psk8(), 200_000, 8, rs, 40_310.0, 0.0, 7);
        let (st, dir) = psk_vfo(500_000.0, &x, 38_000.0, 110_000.0, "psk-vfo");

        let c = st.carrier.expect("demodulator running");
        assert_eq!(
            c.modulation,
            decsat_core::Modulation::Psk8,
            "{}",
            st.message
        );
        assert!(c.locked, "{}", st.message);
        assert!(c.mer_db > 20.0, "{}", st.message);
        let tracked = st.symbol_rate.unwrap();
        assert!((tracked - rs).abs() < rs * 1e-3, "symbol rate {tracked}");
        let (path, written) = st.recording.expect("symbol file");
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(written > 100_000, "only {written} symbols");
        assert_eq!(bytes.len() as u64, written);
        assert!(bytes.iter().all(|&b| b < 8));
        assert!(path.to_string_lossy().ends_with(".bin"));
    }

    #[test]
    fn psk_vfo_finds_text_in_the_bits() {
        // QPSK at 62.5 kBd carrying random bytes with a message every so
        // often, LSB first, mapped through the DVB-S2 QPSK labels.
        let cst = Constellation::qpsk();
        let mut s = 0x7E57u64;
        let mut next = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let mut bytes = Vec::new();
        let mut k = 0;
        while bytes.len() < 40_000 {
            for _ in 0..(40 + next() % 80) {
                bytes.push(next() as u8);
            }
            bytes.extend_from_slice(format!("VK2DEC BEACON {k:05} ").as_bytes());
            k += 1;
        }
        let bits: Vec<usize> = bytes
            .iter()
            .flat_map(|&b| (0..8).map(move |i| ((b >> i) & 1) as usize))
            .collect();
        let syms: Vec<Iq> = bits
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| cst.map(p[0] << 1 | p[1]))
            .collect();
        let mut sh = Shaper::new(8, 0.25, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let w = std::f64::consts::TAU * 40_000.0 / 500_000.0;
        for (n, v) in x.iter_mut().enumerate() {
            let ph = w * n as f64 + 1.0;
            *v *= Iq::new(ph.cos() as f32, ph.sin() as f32);
        }
        // Unscrambled text puts lines in the spectrum that throw the blind
        // symbol rate off: set, as one would on such a carrier.
        let mut settings = VfoSettings::new("TXT", 40_000.0, 110_000.0, DecoderKind::PskSymbols);
        settings.symbol_rate = Some(62_500.0);
        settings.psk_modulation = Some(decsat_core::Modulation::Qpsk);
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(
            500_000.0,
            settings,
            status.clone(),
            Arc::new(AtomicU64::new(0)),
        );
        feed(&mut wk, &x, 65_536);
        let st = status.lock().unwrap().clone();
        let t = st.text.expect("no text search");
        assert!(t.best.is_some(), "{t:?} / {}", st.message);
        assert!(
            t.strings.iter().any(|s| s.contains("VK2DEC BEACON")),
            "{t:?}"
        );
    }

    #[test]
    fn a_vfo_carried_with_a_retune_keeps_decoding() {
        // QPSK at 40 kHz; the radio then retunes 100 kHz up, so the
        // carrier comes in at -60 kHz, and the GUI moves the VFO with it.
        let rs = 62_500.0;
        let before = carrier(&Constellation::qpsk(), 60_000, 8, rs, 40_000.0, 0.0, 11);
        let after = carrier(&Constellation::qpsk(), 60_000, 8, rs, -60_000.0, 0.0, 12);
        let mut s = VfoSettings::new("carried", 40_000.0, 110_000.0, DecoderKind::PskSymbols);
        s.symbol_rate = Some(rs);
        s.psk_modulation = Some(decsat_core::Modulation::Qpsk);
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(
            500_000.0,
            s.clone(),
            status.clone(),
            Arc::new(AtomicU64::new(0)),
        );
        feed(&mut wk, &before, 65_536);
        assert!(status.lock().unwrap().carrier.is_some_and(|c| c.locked));

        let mut moved = s.clone();
        moved.offset_hz -= 100_000.0;
        // The same move as the VFO's own (dragged) restarts its decoder...
        let mut dragged = Worker::new(500_000.0, s, status.clone(), Arc::new(AtomicU64::new(0)));
        feed(&mut dragged, &before[..65_536], 65_536);
        dragged.apply(moved.clone(), 0.0);
        assert!(matches!(dragged.decoder, Decoder::Psk { demod: None, .. }));
        // ...carried with the radio it does not, and stays locked.
        let was = wk.ddc.offset_hz();
        wk.apply(moved, -100_000.0);
        assert!(matches!(wk.decoder, Decoder::Psk { demod: Some(_), .. }));
        // Moved with the radio, keeping whatever fine tuning it had found.
        assert!((wk.ddc.offset_hz() - (was - 100_000.0)).abs() < 1e-6);
        feed(&mut wk, &after, 65_536);
        let st = status.lock().unwrap().clone();
        assert!(st.carrier.is_some_and(|c| c.locked), "{}", st.message);
        assert!(!st.out_of_span);

        // Carried past the span's edge: out of it.
        let mut gone = wk.settings.clone();
        gone.offset_hz -= 300_000.0;
        wk.apply(gone, -300_000.0);
        wk.publish(0);
        assert!(status.lock().unwrap().out_of_span);
    }

    #[test]
    fn psk_vfo_writes_nothing_until_record_is_pressed() {
        let rs = 62_500.0;
        let x = carrier(&Constellation::qpsk(), 150_000, 8, rs, 40_000.0, 0.0, 11);
        let dir = std::env::temp_dir().join(format!("decsat-psk-arm-{}", std::process::id()));
        let mut settings =
            VfoSettings::new("PSK arm", 40_000.0, 110_000.0, DecoderKind::PskSymbols);
        settings.record_dir = dir.clone();
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(
            500_000.0,
            settings.clone(),
            status.clone(),
            Arc::new(AtomicU64::new(0)),
        );
        let third = x.len() / 3;

        // Locked and showing symbols, but no file.
        feed(&mut wk, &x[..third], 65_536);
        {
            let st = status.lock().unwrap();
            assert!(st.carrier.is_some_and(|c| c.locked), "{}", st.message);
            assert!(!st.scatter.is_empty());
            assert!(st.recording.is_none() && !st.recording_active);
        }
        assert!(!dir.exists(), "a file was written before Record");

        // Record: a file grows, without restarting the demodulator.
        settings.record = true;
        wk.apply(settings.clone(), 0.0);
        feed(&mut wk, &x[third..2 * third], 65_536);
        let (path, n) = {
            let st = status.lock().unwrap();
            assert!(st.recording_active, "{}", st.message);
            st.recording.clone().unwrap()
        };
        assert!(n > 10_000, "only {n} symbols");

        // Stop: the file is closed; the last one stays on show.
        settings.record = false;
        wk.apply(settings, 0.0);
        feed(&mut wk, &x[2 * third..], 65_536);
        let st = status.lock().unwrap().clone();
        assert!(!st.recording_active);
        assert_eq!(st.recording, Some((path.clone(), n)));
        let len = std::fs::metadata(&path).unwrap().len();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(len, n);
    }

    #[test]
    fn identify_keeps_a_live_constellation_between_identifications() {
        let rs = 62_500.0;
        let x = carrier(&Constellation::qpsk(), 150_000, 8, rs, 40_000.0, 0.0, 12);
        let settings = VfoSettings::new("ID", 38_000.0, 110_000.0, DecoderKind::Identify);
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(
            500_000.0,
            settings,
            status.clone(),
            Arc::new(AtomicU64::new(0)),
        );
        let half = x.len() / 2;
        feed(&mut wk, &x[..half], 65_536);
        let first = status.lock().unwrap().clone();
        assert!(first.identification.is_some(), "{}", first.message);
        // Identify now rests for seconds; the live view must keep going.
        feed(&mut wk, &x[half..], 65_536);
        let later = status.lock().unwrap().clone();
        let c = later.carrier.expect("no live view");
        assert!(c.locked && c.mer_db > 15.0, "live view: {c:?}");
        assert_ne!(first.scatter, later.scatter, "the constellation froze");
    }

    #[test]
    fn dvbs2_vfo_decodes_bbframes() {
        use decsat_mod::{FrameSpec, PlFramer};
        // QPSK 1/2 with pilots at 125 kBd, 40 kHz into a 500 kS/s band.
        let syms = PlFramer::new(0, 5).build_schedule(&[FrameSpec::new(4, false, true)], 170_000);
        let mut sh = Shaper::new(4, 0.35, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let w = std::f64::consts::TAU * 40_000.0 / 500_000.0;
        for (n, v) in x.iter_mut().enumerate() {
            let ph = w * n as f64;
            *v *= Iq::new(ph.cos() as f32, ph.sin() as f32);
        }
        let settings = VfoSettings::new("S2", 40_000.0, 190_000.0, DecoderKind::Dvbs2Ip);
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(
            500_000.0,
            settings,
            status.clone(),
            Arc::new(AtomicU64::new(0)),
        );
        feed(&mut wk, &x, 65_536);
        // FEC runs on its own thread: give it time to drain its queue.
        let t0 = Instant::now();
        let fec = loop {
            wk.publish(0);
            let f = status.lock().unwrap().fec.clone().expect("no FEC");
            if f.frames >= 3 || t0.elapsed().as_secs() > 20 {
                break f;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert!(fec.frames >= 3, "{fec:?}");
        assert_eq!(fec.ok, fec.frames, "{fec:?}");
        let h = fec.last_header.expect("no BBHEADER");
        assert_eq!(h.format, decsat_frame::StreamFormat::Transport);
        assert!(fec.es_n0_db.is_some_and(|e| e > 15.0), "{fec:?}");
    }

    #[test]
    fn dvbs2_ip_vfo_finds_the_gse_variant_and_writes_pcap() {
        use decsat_gse::{LengthMode, Source, Variant};
        use decsat_mod::{FrameSpec, GseBbFramer, PlFramer};
        // IP over GSE written the non-standard way (length including the
        // header, split frag ids), QPSK 3/4 short frames at 125 kBd.
        let variant = Variant::ALL[3];
        let syms = PlFramer::new(0, 6)
            .with_source(Box::new(GseBbFramer::new(9, variant)))
            .build_schedule(&[FrameSpec::new(7, true, true)], 200_000);
        let mut sh = Shaper::new(4, 0.35, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let w = std::f64::consts::TAU * 40_000.0 / 500_000.0;
        for (n, v) in x.iter_mut().enumerate() {
            let ph = w * n as f64;
            *v *= Iq::new(ph.cos() as f32, ph.sin() as f32);
        }
        let dir = std::env::temp_dir().join(format!("decsat-pcap-vfo-{}", std::process::id()));
        let mut settings = VfoSettings::new("IP", 40_000.0, 190_000.0, DecoderKind::Dvbs2Ip);
        settings.pcap = true;
        settings.record_dir = dir.clone();
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(
            500_000.0,
            settings,
            status.clone(),
            Arc::new(AtomicU64::new(0)),
        );
        feed(&mut wk, &x, 65_536);
        let t0 = Instant::now();
        let gse = loop {
            wk.publish(0);
            let g = status.lock().unwrap().fec.clone().and_then(|f| f.gse);
            if g.as_ref().is_some_and(|g| g.packets >= 10) || t0.elapsed().as_secs() > 20 {
                break g.expect("no GSE");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        drop(wk); // closes the PCAP

        let Some(Source::Gse(found)) = gse.source else {
            panic!("source {:?}", gse.source)
        };
        assert_eq!(found.length, LengthMode::HeaderIncluded);
        assert!(gse.packets >= 10, "{gse:?}");
        assert_eq!(gse.ipv4, gse.packets);
        // The test flows (the test radio shares the link with them).
        assert!(
            gse.top
                .iter()
                .any(|f| f.dst.to_string().starts_with("198.51.100."))
        );
        let (path, written) = gse.pcap.clone().expect("no PCAP");
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gse.pcap_active && written > 0);
        // Header, then records of 16 bytes plus a packet each.
        let (mut at, mut n) = (24usize, 0u64);
        while at + 16 <= bytes.len() {
            let len = u32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap()) as usize;
            assert!(decsat_ip::parse(&bytes[at + 16..at + 16 + len]).is_some());
            at += 16 + len;
            n += 1;
        }
        assert_eq!(at, bytes.len());
        assert!(n >= written, "{n} records, {written} counted");
    }

    #[test]
    fn dvbs2_ts_vfo_streams_mpeg_ts_by_udp() {
        use decsat_mod::{FrameSpec, PlFramer};
        // TS over DVB-S2, QPSK 1/2 with pilots at 125 kBd; the VFO sends the
        // transport stream to a UDP socket this test listens on.
        let syms = PlFramer::new(0, 8).build_schedule(&[FrameSpec::new(4, false, true)], 170_000);
        let mut sh = Shaper::new(4, 0.35, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let w = std::f64::consts::TAU * 40_000.0 / 500_000.0;
        for (n, v) in x.iter_mut().enumerate() {
            let ph = w * n as f64;
            *v *= Iq::new(ph.cos() as f32, ph.sin() as f32);
        }
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_secs(20)))
            .unwrap();
        let mut settings = VfoSettings::new("TS", 40_000.0, 190_000.0, DecoderKind::Dvbs2Ts);
        settings.ts_udp_on = true;
        settings.ts_udp = rx.local_addr().unwrap().to_string();
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(
            500_000.0,
            settings,
            status.clone(),
            Arc::new(AtomicU64::new(0)),
        );
        feed(&mut wk, &x, 65_536);

        // Datagrams of 7 packets, each starting with the sync byte.
        let mut buf = [0u8; 2048];
        let mut packets = 0;
        while packets < 70 {
            let (n, _) = rx.recv_from(&mut buf).expect("no TS by UDP");
            assert_eq!(n, 7 * 188);
            assert!(buf[..n].chunks(188).all(|p| p[0] == 0x47));
            packets += 7;
        }
        let t0 = Instant::now();
        let ts = loop {
            wk.publish(0);
            let t = status.lock().unwrap().fec.clone().and_then(|f| f.ts);
            if t.as_ref()
                .is_some_and(|t| t.report.programmes.iter().any(|p| p.name.is_some()))
                || t0.elapsed().as_secs() > 20
            {
                break t.expect("no TS");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert_eq!(ts.crc_errors, 0, "{ts:?}");
        assert_eq!(ts.cc_errors, 0, "{ts:?}");
        let p = ts
            .report
            .programmes
            .iter()
            .find(|p| p.number == 1)
            .expect("programme 1");
        assert_eq!(p.name.as_deref(), Some("DecSAT test signal"));
        assert_eq!(p.streams[0].pid, 0x100);
        assert!(ts.udp.is_some_and(|(_, n)| n >= 10));
    }

    /// A DVB-S2 VFO on a test carrier from `framer`, run until `done` holds
    /// for its FEC stats (or 20 s pass).
    fn s2_vfo(
        framer: decsat_mod::PlFramer,
        settings: VfoSettings,
        done: impl Fn(&FecStats) -> bool,
    ) -> FecStats {
        use decsat_mod::FrameSpec;
        let mut framer = framer;
        let syms = framer.build_schedule(&[FrameSpec::new(4, false, true)], 1_000_000);
        let mut sh = Shaper::new(4, 0.35, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let w = std::f64::consts::TAU * 40_000.0 / 500_000.0;
        for (n, v) in x.iter_mut().enumerate() {
            let ph = w * n as f64;
            *v *= Iq::new(ph.cos() as f32, ph.sin() as f32);
        }
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(
            500_000.0,
            settings,
            status.clone(),
            Arc::new(AtomicU64::new(0)),
        );
        feed(&mut wk, &x, 65_536);
        let t0 = Instant::now();
        loop {
            wk.publish(0);
            let f = status.lock().unwrap().fec.clone();
            if let Some(f) = f
                && (done(&f) || t0.elapsed().as_secs() > 20)
            {
                return f;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn plays_and_records_multicast_radio_in_the_app() {
        use decsat_gse::Variant;
        use decsat_mod::{GseBbFramer, PlFramer};
        let group: std::net::SocketAddr = "239.255.1.1:5004".parse().unwrap();
        let dir = std::env::temp_dir().join(format!("decsat-radio-{}", std::process::id()));
        let mut settings = VfoSettings::new("IP", 40_000.0, 190_000.0, DecoderKind::Dvbs2Ip);
        settings.audio_play = Some(group);
        settings.audio_record = Some(group);
        settings.record_dir = dir.clone();
        let framer =
            PlFramer::new(0, 6).with_source(Box::new(GseBbFramer::new(9, Variant::STANDARD)));
        let f = s2_vfo(framer, settings, |f| {
            f.gse.as_ref().is_some_and(|g| {
                g.audio_app
                    .as_ref()
                    .is_some_and(|h| h.status().decoded >= 5)
                    && g.audio_record_file.as_ref().is_some_and(|(_, n)| *n > 0)
            })
        });
        let g = f.gse.expect("no IP");
        let h = g.audio_app.as_ref().expect("not playing");
        let st = h.status();
        assert!(st.decoded >= 5, "{st:?} ({:?})", g.audio_error);
        assert!(
            st.codec.as_deref().unwrap_or("").contains("Layer II"),
            "{st:?}"
        );
        let (path, bytes) = g.audio_record_file.clone().expect("not recording");
        assert_eq!(path.extension().unwrap(), "mp2");
        assert!(bytes >= 384);
        assert_eq!(g.audio_recording, Some(group));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_and_relays_multicast_radio_over_gse() {
        use decsat_gse::Variant;
        use decsat_mod::{GseBbFramer, PlFramer};
        let group: std::net::SocketAddr = "239.255.1.1:5004".parse().unwrap();
        let mut settings = VfoSettings::new("IP", 40_000.0, 190_000.0, DecoderKind::Dvbs2Ip);
        settings.audio_play = Some(group);
        settings.audio_external = true;
        let framer =
            PlFramer::new(0, 6).with_source(Box::new(GseBbFramer::new(9, Variant::STANDARD)));
        let f = s2_vfo(framer, settings, |f| {
            f.gse.as_ref().is_some_and(|g| g.audio_forwarded >= 5)
        });
        let g = f.gse.expect("no IP");
        let radio = g
            .audio
            .iter()
            .find(|a| a.group == group.ip())
            .expect("no radio found");
        assert_eq!(radio.name(), "DecSAT test radio");
        assert!(radio.rtp);
        assert_eq!(radio.codec, decsat_ip::Codec::MpegAudio);
        assert!(g.sap_packets > 0);
        let Some(decsat_ip::PlayTarget::Sdp(path)) = &g.audio_target else {
            panic!("target {:?} ({:?})", g.audio_target, g.audio_error)
        };
        let sdp = std::fs::read_to_string(path).unwrap();
        assert!(
            sdp.contains("c=IN IP4 127.0.0.1") && sdp.contains("RTP/AVP 14"),
            "{sdp}"
        );
        assert!(g.audio_forwarded >= 5);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn dvbs_vfo_decodes_the_transport_stream() {
        use decsat_mod::{Shaper, TsBbFramer};
        use decsat_modem::conv::Rate;
        use decsat_modem::dvbs::DvbsTx;
        // A DVB-S carrier, rate 5/6, 125 kBd at 40 kHz in a 500 kS/s band.
        let mut ts = TsBbFramer::new(11);
        let mut tx = DvbsTx::new(Rate::R5_6);
        let mut syms = Vec::new();
        while syms.len() < 320_000 {
            tx.packet(&ts.packet(), &mut syms);
        }
        // Tune in mid-stream: the transmitter's first symbols carry its
        // interleaver's zero fill, long runs no real carrier shows.
        syms.drain(..20_000);
        let mut sh = Shaper::new(4, 0.35, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let w = std::f64::consts::TAU * 40_000.0 / 500_000.0;
        for (n, v) in x.iter_mut().enumerate() {
            let ph = w * n as f64;
            *v *= Iq::new(ph.cos() as f32, ph.sin() as f32);
        }
        let settings = VfoSettings::new("S", 40_000.0, 190_000.0, DecoderKind::DvbsTs);
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(
            500_000.0,
            settings,
            status.clone(),
            Arc::new(AtomicU64::new(0)),
        );
        feed(&mut wk, &x, 65_536);
        let t0 = Instant::now();
        let f = loop {
            wk.publish(0);
            let f = status.lock().unwrap().fec.clone();
            if let Some(f) = f.filter(|f| {
                f.ts.as_ref().is_some_and(|t| {
                    t.packets > 200
                        && t.report.programmes.iter().any(|p| p.name.is_some())
                        && t.text
                            .repeated
                            .iter()
                            .any(|r| r.0.contains("DecSAT test signal"))
                })
            }) {
                break f;
            }
            if t0.elapsed().as_secs() >= 20 {
                let st = status.lock().unwrap();
                panic!(
                    "no programme: {:?} / {:?} / rs {:?}",
                    st.message,
                    st.identification.as_ref().map(|i| i.summary()),
                    st.symbol_rate
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let d = f.dvbs.expect("no DVB-S stats");
        assert_eq!(d.rate, Some(Rate::R5_6));
        assert_eq!(d.rs_failed, 0);
        let t = f.ts.unwrap();
        assert!(t.packets > 100);
        assert_eq!(t.cc_errors, 0);
        let p = &t.report.programmes[0];
        assert_eq!(p.name.as_deref(), Some("DecSAT test signal"));
    }

    #[test]
    fn carrier_id_vfo_finds_a_cid_under_its_host() {
        use decsat_mod::Shaper;
        use decsat_modem::cid::{Field, build_frame, spread};
        // A 224 kBd QPSK host at 15 dB Es/N0 in an 896 kS/s VFO, and under
        // it a DVB-CID at 112 kchip/s, 220 Hz up, 27.5 dB below the host's
        // spectral density (TS 103 129 §5.8–5.9).
        let fs = 896_000.0;
        let mut r = 0x5EED_1234_u64;
        let mut rnd = move || {
            r ^= r << 13;
            r ^= r >> 7;
            r ^= r << 17;
            r
        };
        let n_sym = 600_000; // 2.7 s
        let host: Vec<Iq> = (0..n_sym)
            .map(|_| {
                let v = rnd();
                let a = std::f32::consts::FRAC_1_SQRT_2;
                Iq::new(
                    if v & 1 == 0 { a } else { -a },
                    if v & 2 == 0 { a } else { -a },
                )
            })
            .collect();
        let mut x = Vec::new();
        Shaper::new(4, 0.35, 16).process(&host, &mut x);
        let frame = build_frame(
            0x0006_B0FF_FF01_AC07,
            [
                Field {
                    content_id: 0,
                    info: 1,
                },
                Field {
                    content_id: 0,
                    info: 1,
                },
            ],
            false,
        );
        let (mut chips, mut diff) = (Vec::new(), 0);
        spread(&[frame], &mut diff, &mut chips);
        let chip_syms: Vec<Iq> = chips[..x.len() / 8 + 64]
            .iter()
            .map(|&c| Iq::new(if c == 1 { -1.0 } else { 1.0 }, 0.0))
            .collect();
        let mut cid = Vec::new();
        Shaper::new(8, 0.35, 16).process(&chip_syms, &mut cid);
        // Power: the host's samples are about unit power over its band;
        // the CID's density 27.5 dB lower over half the bandwidth.
        let p_host = x.iter().map(|z| z.norm_sqr()).sum::<f32>() / x.len() as f32;
        let p_cid = cid.iter().map(|z| z.norm_sqr()).sum::<f32>() / cid.len() as f32;
        let g = (p_host / p_cid * 0.5 * 10f32.powf(-2.75)).sqrt();
        let noise = (p_host / 10f32.powf(1.5) * fs as f32 / 224_000.0 / 2.0).sqrt();
        for (n, v) in x.iter_mut().enumerate() {
            let ph = std::f64::consts::TAU * 220.0 * n as f64 / fs;
            let mut u = || ((rnd() >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 3.46;
            *v +=
                cid[n] * Iq::new(ph.cos() as f32, ph.sin() as f32) * g + Iq::new(u(), u()) * noise;
        }
        let mut settings = VfoSettings::new("C", 0.0, 400_000.0, DecoderKind::CarrierId);
        settings.symbol_rate = Some(224_000.0);
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(fs, settings, status.clone(), Arc::new(AtomicU64::new(0)));
        wk.lossless = true;
        feed(&mut wk, &x, 65_536);
        let t0 = Instant::now();
        let c = loop {
            wk.publish(0);
            let c = status.lock().unwrap().cid.clone();
            if let Some(c) = c.filter(|c| c.stats.acquired && c.stats.bits > 20) {
                break c;
            }
            if t0.elapsed().as_secs() >= 60 {
                let st = status.lock().unwrap();
                panic!("no CID: {} / {:?}", st.message, st.cid);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert_eq!(c.chip_rate, 112e3);
        assert!(c.wide_enough);
        // 220 Hz from the carrier, wherever Identify put its centre.
        let at = c.center_hz + c.stats.offset_hz;
        assert!((at - 220.0).abs() < 10.0, "found at {at:+.1} Hz");
        assert!(c.stats.snr_db > 3.0, "{:?}", c.stats);
    }

    #[test]
    fn fastlink_vfo_decodes_the_ldpc_and_the_ip_in_it() {
        use decsat_mod::Shaper;
        use decsat_mod::fec::TestRadio;
        use decsat_modem::fastlink::{FRAME_DATA, modulate};
        use decsat_modem::payload::{SelfSyncScrambler, hdlc_frame};
        // A FastLink carrier, 125 kBd at 40 kHz in a 500 kS/s band, its
        // data a multicast radio over Cisco HDLC, self-synchronising
        // scrambled.
        let mut radio = TestRadio::new([239, 1, 2, 4], "FastLink radio");
        let mut scrambler = SelfSyncScrambler::new(&[2, 3, 9, 12]);
        let mut bits = Vec::new();
        while bits.len() < 40 * FRAME_DATA {
            let mut f = vec![0x0F, 0x00, 0x08, 0x00];
            f.extend(radio.next_packet());
            hdlc_frame(&f, &mut bits);
        }
        scrambler.scramble(&mut bits);
        let mut syms = Vec::new();
        // `as_chunks`: whole frames as fixed-size arrays (the tail is dropped).
        for frame in bits.as_chunks::<FRAME_DATA>().0 {
            modulate(frame, &mut syms);
        }
        // Tune in mid-frame.
        let mut sh = Shaper::new(4, 0.35, 16);
        let mut x = Vec::new();
        sh.process(&syms[5000..], &mut x);
        let w = std::f64::consts::TAU * 40_000.0 / 500_000.0;
        for (n, v) in x.iter_mut().enumerate() {
            let ph = w * n as f64 + 0.7;
            *v *= Iq::new(ph.cos() as f32, ph.sin() as f32);
        }
        let settings = VfoSettings::new("F", 40_000.0, 190_000.0, DecoderKind::FastLink);
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(
            500_000.0,
            settings,
            status.clone(),
            Arc::new(AtomicU64::new(0)),
        );
        feed(&mut wk, &x, 65_536);
        let t0 = Instant::now();
        let f = loop {
            wk.publish(0);
            let f = status.lock().unwrap().fec.clone();
            if let Some(f) = f.filter(|f| f.gse.as_ref().is_some_and(|g| g.packets > 50)) {
                break f;
            }
            if t0.elapsed().as_secs() >= 30 {
                let st = status.lock().unwrap();
                panic!(
                    "no IP: {:?} / {:?} / {:?}",
                    st.message,
                    st.fec.as_ref().and_then(|f| f.fastlink.clone()),
                    st.fec.as_ref().and_then(|f| f.payload.clone())
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let t = f.fastlink.expect("no FastLink stats");
        assert!(t.locked, "{t:?}");
        // A frame the demodulator's acquisition slips in is lost; none other.
        assert!(t.failed <= 8 * t.slips, "{t:?}");
        assert_eq!(t.uw_misses, 0, "{t:?}");
        assert!(t.decoded >= 8, "{t:?}");
        let found = f.payload.and_then(|p| p.found).expect("payload not found");
        assert!(found.starts_with("HDLC (FCS-16)"), "{found}");
        let g = f.gse.unwrap();
        assert!(
            g.audio
                .iter()
                .any(|a| a.group == "239.1.2.4".parse::<std::net::IpAddr>().unwrap())
        );
    }

    /// An Intelsat 19 outbound, 102.4 kBd at 30 kHz in a 409.6 kS/s band,
    /// tuned in mid-frame: traffic, then a request ack and the channel
    /// assignment after it (packets as received on the air) — the VFO
    /// frames it and names the call signalling.
    #[test]
    fn reads_an_is19_outbound_and_its_call_signalling() {
        use decsat_mod::Shaper;
        use decsat_modem::is19::{self, PacketKind};
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut bit = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 40) as u8 & 1
        };
        let traffic = |bit: &mut dyn FnMut() -> u8| {
            let mut b = is19::hex_bits("aaa3d15555555555");
            while b.len() < 275 * 8 {
                b.push(bit());
            }
            b
        };
        let mut pk = Vec::new();
        for i in 0..30 {
            pk.push((200 + 31 * i, 3, traffic(&mut bit)));
            if i == 20 {
                pk.push((300, 5, is19::hex_bits("aa9955495406b4a3d551c5faaa")));
                pk.push((
                    40,
                    2,
                    is19::hex_bits(
                        "5550baa92a82d697c9fc5b81e0cc5ac6f51f568ca842ab8982aaa8eaaaaaaaaaaaa9ab1aaba9576755",
                    ),
                ));
            }
        }
        let frames = 5;
        let stream = is19::stream_with(&pk, frames * is19::FRAME_INFO_BITS);
        let pilots: Vec<bool> = (0..2496).map(|_| bit() == 1).collect();
        let mut syms = Vec::new();
        for f in 0..frames {
            let info = &stream[f * is19::FRAME_INFO_BITS..(f + 1) * is19::FRAME_INFO_BITS];
            syms.extend(is19::build_frame(info, &mut bit, &pilots, f % 2 == 1));
        }
        let mut sh = Shaper::new(4, is19::ROLL_OFF, 16);
        let mut sig = Vec::new();
        sh.process(&syms[7_000..], &mut sig);
        let w = std::f64::consts::TAU * 30_000.0 / 409_600.0;
        let mut u = 0x9E37_79B9_7F4A_7C15u64;
        let mut uniform = move || {
            u ^= u << 13;
            u ^= u >> 7;
            u ^= u << 17;
            (u >> 11) as f32 / (1u64 << 53) as f32 - 0.5
        };
        for (n, v) in sig.iter_mut().enumerate() {
            let ph = w * n as f64 + 1.1;
            *v = *v * Iq::new(ph.cos() as f32, ph.sin() as f32)
                + Iq::new(uniform(), uniform()) * 0.06;
        }
        let settings = VfoSettings::new("O", 30_000.0, 125_000.0, DecoderKind::Is19Outbound);
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(
            409_600.0,
            settings,
            status.clone(),
            Arc::new(AtomicU64::new(0)),
        );
        feed(&mut wk, &sig, 32_768);
        let st = status.lock().unwrap();
        let v = st
            .is19
            .clone()
            .unwrap_or_else(|| panic!("no outbound view: {}", st.message));
        let f = &v.stats.framer;
        // Joined mid-frame: the frames after the first sync word found.
        assert!(f.frames >= 3, "{f:?} / {}", st.message);
        assert_eq!(f.uw_misses, 0, "{f:?}");
        assert!(v.stats.count(PacketKind::Traffic) >= 8, "{:?}", v.stats);
        let kinds: Vec<PacketKind> = v.events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            [PacketKind::RequestAck, PacketKind::Assignment],
            "{}",
            st.message
        );
        assert_eq!(v.events[0].terminal, Some(0x92a8_0d69));
        // The assignment: the calling end of call 56, on channels 0 and 1.
        let a = v.events[1].assignment.expect("no assignment fields");
        assert_eq!((a.call_ref, a.tx, a.rx), (56, Some(0), Some(1)));
        assert_eq!(v.calls.len(), 1);
        assert_eq!(v.events[0].assigned, Some(true));
        assert!(st.carrier.is_some_and(|c| c.locked && c.mer_db > 12.0));
    }

    /// A TPC 2964 QPSK carrier, 125 kBd at 40 kHz in a 500 kS/s band,
    /// carrying a multicast radio over Cisco HDLC, tuned in mid-frame.
    fn tpc_test_carrier() -> Vec<Iq> {
        use decsat_mod::Shaper;
        use decsat_mod::fec::TestRadio;
        use decsat_modem::tpc2964::{Structure, TpcHdlcTx, modulate};
        let mut tx = TpcHdlcTx::new(Structure::TEST);
        let mut radio = TestRadio::new([239, 1, 2, 3], "TPC radio");
        let mut bits = Vec::new();
        while bits.len() < 600_000 {
            if tx.backlog() < 4000 {
                let mut f = vec![0x0F, 0x00, 0x08, 0x00];
                f.extend(radio.next_packet());
                tx.send(&f);
            }
            tx.frame(&mut bits);
        }
        // Tune in mid-frame.
        let mut syms = Vec::new();
        modulate(&bits[1000..], decsat_core::Modulation::Qpsk, &mut syms);
        let mut sh = Shaper::new(4, 0.35, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let w = std::f64::consts::TAU * 40_000.0 / 500_000.0;
        for (n, v) in x.iter_mut().enumerate() {
            let ph = w * n as f64 + 0.7;
            *v *= Iq::new(ph.cos() as f32, ph.sin() as f32);
        }
        x
    }

    #[test]
    fn tpc2964_vfo_finds_the_code_and_the_ip_in_it() {
        use decsat_modem::tpc2964::Structure;
        let x = tpc_test_carrier();
        let settings = VfoSettings::new("T", 40_000.0, 190_000.0, DecoderKind::Tpc2964);
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(
            500_000.0,
            settings,
            status.clone(),
            Arc::new(AtomicU64::new(0)),
        );
        feed(&mut wk, &x, 65_536);
        let t0 = Instant::now();
        let f = loop {
            wk.publish(0);
            let f = status.lock().unwrap().fec.clone();
            if let Some(f) = f.filter(|f| {
                f.gse
                    .as_ref()
                    .is_some_and(|g| g.packets > 50 && g.audio.iter().any(|a| a.sdp.is_some()))
            }) {
                break f;
            }
            if t0.elapsed().as_secs() >= 30 {
                let st = status.lock().unwrap();
                panic!(
                    "no IP: {:?} / {:?} / {:?}",
                    st.message,
                    st.fec.as_ref().and_then(|f| f.tpc.clone()),
                    st.fec.as_ref().and_then(|f| f.payload.clone())
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let t = f.tpc.expect("no TPC stats");
        assert_eq!(t.structure, Some(Structure::TEST.describe()));
        // A frame a slip falls in is lost (the receiver re-finds the UW at
        // once); none other.
        assert!(t.failed <= t.slips, "{t:?}");
        assert_eq!(t.uw_misses, 0, "{t:?}");
        let p = f.payload.expect("no payload stats");
        let found = p.found.expect("payload not found");
        assert!(found.starts_with("HDLC (FCS-16)"), "{found}");
        assert!(found.contains("taps 2, 3, 9, 12"), "{found}");
        let g = f.gse.unwrap();
        assert!(
            g.audio
                .iter()
                .any(|a| a.group == "239.1.2.3".parse::<std::net::IpAddr>().unwrap())
        );
    }

    #[test]
    fn tpc2964_vfo_plays_and_records_a_timeslot_of_an_e1() {
        use decsat_mod::Shaper;
        use decsat_modem::e1::{E1Tx, TIMESLOTS, alaw_encode};
        use decsat_modem::payload::SelfSyncScrambler;
        use decsat_modem::tpc2964::{DATA, Structure, TpcTx, modulate};
        // 0.6 s of E1: a 1 kHz tone in timeslot 7, the rest idle, V.35-style
        // scrambled, in TPC 2964 frames on QPSK at 125 kBd.
        let mut e1 = E1Tx::new();
        let mut data = Vec::new();
        for n in 0..4800 {
            let mut ts = [0xD5u8; TIMESLOTS];
            let t = n as f32 / 8000.0;
            ts[7] = alaw_encode(0.4 * (std::f32::consts::TAU * 1000.0 * t).sin());
            e1.frame(&ts, &mut data);
        }
        SelfSyncScrambler::new(&[3, 20]).scramble(&mut data);
        let tx = TpcTx::new(Structure::TEST);
        let mut bits = Vec::new();
        for d in data.as_chunks::<DATA>().0 {
            tx.frame(d, &mut bits);
        }
        let mut syms = Vec::new();
        modulate(&bits[500..], decsat_core::Modulation::Qpsk, &mut syms);
        let mut sh = Shaper::new(4, 0.35, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let w = std::f64::consts::TAU * 40_000.0 / 500_000.0;
        for (n, v) in x.iter_mut().enumerate() {
            let ph = w * n as f64;
            *v *= Iq::new(ph.cos() as f32, ph.sin() as f32);
        }
        let dir = std::env::temp_dir().join(format!("decsat-e1-{}", std::process::id()));
        let mut settings = VfoSettings::new("E1", 40_000.0, 190_000.0, DecoderKind::Tpc2964);
        settings.record_dir = dir.clone();
        settings.e1_play = Some(7);
        settings.e1_record = Some(7);
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(
            500_000.0,
            settings,
            status.clone(),
            Arc::new(AtomicU64::new(0)),
        );
        feed(&mut wk, &x, 65_536);
        let t0 = Instant::now();
        let e = loop {
            wk.publish(0);
            let f = status.lock().unwrap().fec.clone();
            if let Some(e) = f.as_ref().and_then(|f| f.e1.clone()).filter(|e| {
                e.levels_db.len() == 32 && e.record_file.as_ref().is_some_and(|r| r.1 > 8000)
            }) {
                break e;
            }
            if t0.elapsed().as_secs() >= 30 {
                let st = status.lock().unwrap();
                panic!(
                    "no E1: {} / {:?}",
                    st.message,
                    st.fec.as_ref().map(|f| (&f.payload, &f.e1))
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        drop(wk);
        let _ = std::fs::remove_dir_all(&dir);
        // The synthetic carrier's one carrier slip costs a TPC frame, and
        // the E1 its alignment once.
        assert!(e.stats.locked && e.stats.losses <= 1, "{:?}", e.stats);
        assert!(e.levels_db[7] > -15.0, "TS 7 at {} dB", e.levels_db[7]);
        assert!(e.levels_db[1] < -60.0, "TS 1 at {} dB", e.levels_db[1]);
        assert_eq!(e.playing, Some(7));
        let a = e.audio.expect("no player").status();
        assert!(a.decoded > 10, "{a:?}");
        let (path, _) = e.record_file.unwrap();
        assert!(path.to_string_lossy().ends_with(".wav"), "{path:?}");
    }

    #[test]
    fn tpc2964_vfo_plays_a_comtech_dandi_plus_channel() {
        use decsat_mod::Shaper;
        use decsat_modem::dandi::{DATA_BYTES, frame};
        use decsat_modem::e1::alaw_encode;
        use decsat_modem::payload::SelfSyncScrambler;
        use decsat_modem::tpc2964::{DATA, Structure, TpcTx, modulate};
        // A CDM-600-style carrier: one 64 kbit/s timeslot (a 440 Hz tone)
        // in D&I++ frames, V.35-scrambled, TPC 2964 on QPSK. Its symbol rate
        // follows from the data rate: 64000 × 46/45 / (2 × 0.75).
        let mut data = Vec::new();
        let mut n = 0usize;
        for _ in 0..120 {
            let mut d = [0u8; DATA_BYTES];
            for v in d.iter_mut() {
                let t = n as f32 / 8000.0;
                *v = alaw_encode(0.3 * (std::f32::consts::TAU * 440.0 * t).sin());
                n += 1;
            }
            frame(&d, &mut data);
        }
        SelfSyncScrambler::new(&[3, 20]).scramble(&mut data);
        let tx = TpcTx::new(Structure::TEST);
        let mut bits = Vec::new();
        for d in data.as_chunks::<DATA>().0 {
            tx.frame(d, &mut bits);
        }
        let mut syms = Vec::new();
        modulate(&bits[300..], decsat_core::Modulation::Qpsk, &mut syms);
        // 4 samples a symbol: the band is 4 × 43 616 S/s wide.
        let rs = 64_000.0 * 46.0 / 45.0 / 1.5;
        let mut sh = Shaper::new(4, 0.35, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let rate = 4.0 * rs;
        let settings = VfoSettings::new("DI", 0.0, 0.8 * rate, DecoderKind::Tpc2964);
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(rate, settings, status.clone(), Arc::new(AtomicU64::new(0)));
        feed(&mut wk, &x, 65_536);
        let t0 = Instant::now();
        let e = loop {
            wk.publish(0);
            let f = status.lock().unwrap().fec.clone();
            if let Some(e) = f
                .as_ref()
                .and_then(|f| f.e1.clone())
                .filter(|e| e.levels_db.len() == 32)
            {
                break e;
            }
            if t0.elapsed().as_secs() >= 30 {
                let st = status.lock().unwrap();
                panic!(
                    "no D&I++: {} / {:?}",
                    st.message,
                    st.fec.as_ref().map(|f| (&f.tpc, &f.payload))
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert_eq!(e.channels, Some(1), "{}", e.source);
        assert!(e.source.contains("D&I++"), "{}", e.source);
        assert!(e.levels_db[1] > -20.0, "channel 1 at {} dB", e.levels_db[1]);
    }

    #[test]
    fn finds_multicast_radio_in_mpe_on_a_ts_carrier() {
        use decsat_mod::PlFramer;
        let settings = VfoSettings::new("TS", 40_000.0, 190_000.0, DecoderKind::Dvbs2Ts);
        let f = s2_vfo(PlFramer::new(0, 8), settings, |f| {
            f.gse.as_ref().is_some_and(|g| {
                g.audio.iter().any(|a| a.sdp.is_some())
                    && g.text
                        .recent
                        .iter()
                        .any(|r| r.contains("DecSAT test radio"))
            })
        });
        let g = f.gse.expect("no IP from MPE");
        // The announcement's SDP, line by line, in the IP payloads.
        assert!(
            g.text
                .recent
                .iter()
                .any(|r| r == "s=DecSAT test radio (MPE)"),
            "{:?}",
            g.text
        );
        let mpe = g.mpe.as_ref().expect("no MPE");
        assert!(
            mpe.pids.contains_key(&decsat_mod::fec::TEST_MPE_PID),
            "{mpe:?}"
        );
        let radio = g.audio.first().expect("no radio");
        assert_eq!(radio.name(), "DecSAT test radio (MPE)");
        assert_eq!(radio.codec, decsat_ip::Codec::MpegAudio);
    }

    #[test]
    fn psk_vfo_locks_a_10_kbaud_carrier_with_lnb_drift() {
        // A narrow Ku-band SCPC carrier: QPSK at 10 kBd, 150 Hz from where the
        // VFO was dropped (LNB error), with phase noise, in a 250 kS/s band.
        let rs = 10_000.0;
        let x = carrier(&Constellation::qpsk(), 50_000, 25, rs, 31_150.0, 0.01, 9);
        let (st, dir) = psk_vfo(250_000.0, &x, 31_000.0, 20_000.0, "psk-narrow");
        let _ = std::fs::remove_dir_all(&dir);

        let Some(c) = st.carrier else {
            panic!(
                "not demodulating: {} (rate {:.0}, progress {}, {} samples in)",
                st.message,
                st.out_rate,
                st.progress,
                x.len()
            )
        };
        assert_eq!(
            c.modulation,
            decsat_core::Modulation::Qpsk,
            "{}",
            st.message
        );
        assert!(c.locked, "{}", st.message);
        assert!(c.mer_db > 15.0, "{}", st.message);
        let tracked = st.symbol_rate.unwrap();
        assert!((tracked - rs).abs() < rs * 2e-3, "symbol rate {tracked}");
        assert!(
            st.recording.is_some_and(|(_, n)| n > 10_000),
            "{}",
            st.message
        );
    }
}
