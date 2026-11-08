// made by CasualArclamp
//! The control and status messages, encoded with postcard.
//!
//! Status structures reuse the engine's own types, so client and server must
//! agree on them: [`crate::wire::PROTO`] is bumped when any of them changes,
//! and the handshake refuses a mismatch.

use std::collections::BTreeMap;

use decsat_core::RollOff;
use decsat_engine::bursty::BurstBand;
use decsat_engine::carriers::Carrier;
use decsat_engine::demod::LockState;
use decsat_engine::frontend::SourceState;
use decsat_engine::identify::Identification;
use decsat_engine::streams::StreamView;
use decsat_engine::superframe::SfView;
use decsat_engine::vfo::CarrierState;
use decsat_frame::bbframe::BbHeader;
use decsat_modem::dvbs::DvbsStats;
use serde::{Deserialize, Serialize};

use crate::display::{DbTrace, Scatter};
use crate::radios::{RadioInfo, RadioRequest};

/// A client's first message, after [`crate::wire::MAGIC`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub proto: u16,
    pub version: String,
    /// Shown to other users in shared mode.
    pub name: String,
    /// Replace a client that already holds the server, if the server allows.
    pub takeover: bool,
}

/// How a server hands out its radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMode {
    /// One client at a time, in full control.
    Exclusive,
    /// A radio fixed by the server; everyone sees the band, adds their own
    /// decoders and may camp on others'.
    Shared,
}

/// Dumb-mode options.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct IqParams {
    /// 4, 6, 8, 10, 12 or 16.
    pub bits: u8,
    /// zstd level, 0 for off.
    pub zstd: i32,
    /// Send only a slice of the band: its centre relative to the radio's
    /// and its bandwidth, Hz. `None` for the whole band.
    pub slice: Option<(f64, f64)>,
}

impl Default for IqParams {
    fn default() -> Self {
        IqParams {
            bits: 8,
            zstd: 0,
            slice: None,
        }
    }
}

/// Smart-mode options.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SmartParams {
    pub fft_size: u32,
    pub rows_per_sec: f32,
    /// zstd level for the display data.
    pub display_zstd: i32,
    /// zstd level for the TS, 0 for off (video barely compresses).
    pub ts_zstd: i32,
    /// Most constellation points in one status.
    pub scatter_points: u32,
    pub dc_removal: bool,
}

impl Default for SmartParams {
    fn default() -> Self {
        SmartParams {
            fft_size: 4096,
            rows_per_sec: 25.0,
            display_zstd: 1,
            ts_zstd: 0,
            scatter_points: 1024,
            dc_removal: true,
        }
    }
}

/// What the client wants the server to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Mode {
    Dumb(IqParams),
    Smart(SmartParams),
}

/// The parts of a VFO's settings that the server acts on. Where its output
/// goes (files, UDP, the player) stays on the client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VfoRequest {
    pub name: String,
    pub offset_hz: f64,
    pub bandwidth_hz: f64,
    /// `DecoderKind::key`.
    pub decoder: String,
    pub enabled: bool,
    pub symbol_rate: Option<f64>,
    pub gold_code: u32,
    pub follow_carrier: bool,
    pub ts_isi: Option<u8>,
    pub biss_key: String,
}

/// Client to server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum C2S {
    Hello(Hello),
    Auth {
        client_nonce: [u8; 32],
        proof: Vec<u8>,
    },
    /// Exclusive mode: open a radio and start. Ignored details in shared mode.
    Start {
        radio: Option<RadioRequest>,
        mode: Mode,
    },
    Stop,
    Tune {
        hz: f64,
    },
    Gain {
        name: String,
        value: f64,
    },
    SetPaused(bool),
    SetDcRemoval(bool),
    SetFftSize(u32),
    /// `id` is the client's own number for the VFO; the server keeps the
    /// mapping, so the client need not wait for an answer.
    AddVfo {
        id: u32,
        vfo: VfoRequest,
    },
    UpdateVfo {
        id: u32,
        vfo: VfoRequest,
        carried_by: Option<f64>,
    },
    RemoveVfo {
        id: u32,
    },
    /// Join a decoder someone else made, under the client's own number.
    Camp {
        id: u32,
        server_id: u32,
    },
    /// Empty for every PID.
    TsPids {
        id: u32,
        pids: Vec<u16>,
    },
    Ping(u64),
}

/// One decoder on a shared server, for the list everyone sees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SharedVfo {
    pub server_id: u32,
    pub name: String,
    /// `None` once its owner has left and only campers keep it alive.
    pub owner: Option<String>,
    pub offset_hz: f64,
    pub bandwidth_hz: f64,
    pub decoder: String,
    pub campers: u32,
}

/// Server to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum S2C {
    ServerHello {
        proto: u16,
        version: String,
        name: String,
        mode: ServerMode,
        /// Present when a password is needed.
        nonce: Option<[u8; 32]>,
    },
    /// The client may proceed.
    Ready {
        radios: Vec<RadioInfo>,
    },
    /// The connection is over, and why.
    Refused(String),
    Started {
        sample_rate: f64,
        center_hz: f64,
        description: String,
        gains: Vec<crate::radios::Gain>,
    },
    Stopped(String),
    /// Something the user should see; the session carries on.
    Note(String),
    /// A VFO the client held has gone (its id is free again), or a request
    /// for one was refused.
    VfoGone {
        id: u32,
        why: String,
    },
    /// A camp succeeded: the decoder's settings, under the client's number.
    Camped {
        id: u32,
        vfo: VfoRequest,
    },
    Shared(Vec<SharedVfo>),
    /// A radio retune took effect.
    Tuned {
        center_hz: f64,
    },
    Stats(LinkStats),
    Pong(u64),
}

/// What the server knows about the link.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkStats {
    /// Bytes per second written to the socket, last second.
    pub bytes_per_sec: f64,
    /// IQ samples thrown away because the link fell behind, since start.
    pub iq_dropped: u64,
    /// TS packets thrown away.
    pub ts_dropped: u64,
    /// Waterfall rows thinned out.
    pub rows_thinned: u64,
    /// One row in this many is being sent (1 when the link keeps up).
    pub row_divider: u32,
}

/// The front end's status, minus its spectrum (sent on its own).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontWire {
    pub sample_rate: f64,
    pub center_freq: f64,
    pub fft_size: u32,
    pub carriers: Vec<Carrier>,
    pub burst_bands: Vec<BurstBand>,
    pub burst_takes: u64,
    pub source: String,
    pub state: SourceState,
    pub samples: u64,
    pub load: f32,
    pub row_seq: u64,
    pub input_dbfs: f32,
}

/// The MPEG-TS counters. The service and PID report is rebuilt by the
/// client from the packets it receives, so it is not sent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TsWire {
    pub packets: u64,
    pub crc_errors: u64,
    pub cc_errors: u64,
    pub nulls_reinserted: u64,
    pub resyncs: u64,
    pub issy: bool,
    pub ts_bps: f64,
}

/// What FEC has made of the frames, for the decoders served remotely.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FecWire {
    pub frames: u64,
    pub ok: u64,
    pub bch_failed: u64,
    pub crc_failed: u64,
    pub ldpc_unconverged: u64,
    pub dropped: u64,
    pub iterations: u64,
    pub bch_corrected: u64,
    pub es_n0_db: Option<f32>,
    pub last_header: Option<BbHeader>,
    pub roll_off: Option<RollOff>,
    pub streams: Vec<StreamView>,
    pub ts_isi: Option<u8>,
    pub per_modcod: BTreeMap<u8, (u64, u64)>,
    pub payload_bps: f64,
    pub load: f32,
    pub ts_frames: u64,
    pub ts: Option<TsWire>,
    pub dvbs: Option<DvbsStats>,
}

/// A VFO's status.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VfoWire {
    /// The client's number for it.
    pub id: u32,
    pub out_rate: f64,
    pub decimation: u32,
    pub vfo_offset_hz: f64,
    pub follow_hz: f64,
    pub out_of_span: bool,
    pub load: f32,
    pub dropped: u64,
    pub level_db: f32,
    pub spectrum: DbTrace,
    pub scatter: Scatter,
    pub identification: Option<Identification>,
    pub id_symbols: Scatter,
    pub progress: f32,
    pub provisional: bool,
    pub lock: Option<LockState>,
    pub frames: u64,
    pub lock_losses: u64,
    pub modcods: BTreeMap<u8, u64>,
    pub last_modcod: Option<u8>,
    pub symbol_rate: Option<f64>,
    pub message: String,
    pub carrier: Option<CarrierState>,
    pub gold_code: Option<u32>,
    pub superframes: Option<SfView>,
    pub fec: Option<FecWire>,
}

/// Encode a message for the wire.
pub fn encode<T: Serialize>(m: &T) -> Vec<u8> {
    // Rust note: serialising these types to a Vec cannot fail: only a sink
    // that runs out of room would.
    postcard::to_stdvec(m).expect("encoding to a Vec")
}

/// Decode a message from the wire.
pub fn decode<'a, T: Deserialize<'a>>(b: &'a [u8]) -> Option<T> {
    postcard::from_bytes(b).ok()
}
