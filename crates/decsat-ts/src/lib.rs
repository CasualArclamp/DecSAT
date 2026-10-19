// made by CasualArclamp
//! TS-mode BBFRAMEs to MPEG-TS, what the stream carries, and where it goes.
//!
//! - [`deframe`]: data fields back to 188-byte packets (CRC-8 chain lock,
//!   null-packet re-insertion).
//! - [`psi`]: per-PID counts and continuity; PAT, PMT and SDT for the
//!   programmes and service names.
//! - [`sink`]: a `.ts` file, UDP, or a TCP/HTTP server for VLC and PotPlayer.
//! - [`backlog`]: the stream since its last keyframe, for a player that
//!   starts now.
//! - [`csa`] and [`biss`]: DVB-CSA descrambling with a BISS key.
//! - [`rcs`]: a VSAT hub's DVB-RCS signalling — the return link it runs.
//! - [`newtec`]: a Newtec hub's private burst plan (table D1h).
//! - [`uplink_id`]: the encoder's uplink identification text (maker, serial,
//!   position) some streams carry in their SI.
//! - [`udp`]: transport packets carried in UDP (bare, RTP, a sender's own
//!   header), found in an IP stream.

pub mod backlog;
pub mod biss;
pub mod csa;
pub mod deframe;
pub mod mpe;
pub mod newtec;
pub mod psi;
pub mod rcs;
pub mod section;
pub mod sink;
pub mod udp;
pub mod uplink_id;

pub use biss::{BissKey, DescrambleStats, Descrambler, KeyError, Outcome, Verdict};
pub use deframe::{DeframeStats, TS_LEN, TsDeframer, null_packet};
pub use mpe::{MpeExtractor, MpeStats};
pub use psi::{
    EsInfo, Event, NetworkInfo, PidRow, PidStats, Programme, Transponder, TsAnalyser, TsReport,
    ca_system_name, service_type_name, stream_type_name,
};
pub use sink::{TcpSink, TsFile, TsTap, UdpSink};
pub use udp::{UdpTs, UdpTsFinder};
