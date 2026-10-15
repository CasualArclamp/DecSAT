//! TS-mode BBFRAMEs to MPEG-TS, what the stream carries, and where it goes.
//!
//! - [`deframe`]: data fields back to 188-byte packets (CRC-8 chain lock,
//!   null-packet re-insertion).
//! - [`psi`]: per-PID counts and continuity; PAT, PMT and SDT for the
//!   programmes and service names.
//! - [`sink`]: a `.ts` file, UDP, or a TCP/HTTP server for VLC and PotPlayer.
//! - [`csa`] and [`biss`]: DVB-CSA descrambling with a BISS key.

pub mod biss;
pub mod csa;
pub mod deframe;
pub mod mpe;
pub mod psi;
pub mod section;
pub mod sink;

pub use biss::{BissKey, DescrambleStats, Descrambler, KeyError, Outcome, Verdict};
pub use deframe::{DeframeStats, TS_LEN, TsDeframer, null_packet};
pub use mpe::{MpeExtractor, MpeStats};
pub use psi::{
    EsInfo, Event, NetworkInfo, PidRow, PidStats, Programme, Transponder, TsAnalyser, TsReport,
    ca_system_name, service_type_name, stream_type_name,
};
pub use sink::{TcpSink, TsFile, TsTap, UdpSink};
