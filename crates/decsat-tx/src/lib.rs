// made by CasualArclamp
//! The DecSAT transmitter: a camera, a video file or a transport stream in,
//! a DVB-S2/S2X carrier out through a HackRF One (or into an IQ file, or
//! nowhere, for checks). The engine behind the `decsat-tx` window, laid out
//! like DecDVB-T's transmitter (`decdvbt-tx`).
//!
//! ```text
//! camera, mic ─ffmpeg (child)─> TS at the channel's rate ─[tx-ts-read]─> TsRing
//!   (H.264/5/6, AV1, MPEG-2)     (AV1 made conformant:        (nulls when empty)
//!                                 decsat_ts::av1)                 │
//!                                                              Pump: DVB-S2/S2X
//!                                                     modulator (decsat_mod::datv)
//!                                                                 │
//!                                 HackRF (its sample clock paces all) / IQ file / nothing
//! ```
//!
//! **Nothing here reaches the radio without [`arm::Arm`].** The one call
//! that opens the HackRF is in [`radio`], takes an [`arm::ArmedProof`] only
//! [`arm`] can make, and a test checks that no other file has it.
//!
//! - [`qo100`]: QO-100 wideband transponder presets, with their sources.
//! - [`encode`]: codecs and engines, ffmpeg's command line, cameras.
//! - [`feeder`]: ffmpeg running, its stream, log and preview read.
//! - [`live`]: the TS ring and the modulator's input from it.
//! - [`iq`]: the modulator as a sample pump, with its spectrum.
//! - [`radio`]: the sinks; [`arm`]: the gate; [`session`]: [`TxSession`].
//! - [`cpu`]: CPU meters.

pub mod arm;
pub mod cpu;
pub mod encode;
pub mod feeder;
pub mod iq;
pub mod live;
pub mod qo100;
pub mod radio;
pub mod session;

pub use arm::{LICENCE_WARNING, LicenceAck};
pub use encode::{AudioCodec, EncodeSettings, Engine, Input, Picture, VideoCodec};
pub use radio::{NO_RADIO_ENV, PLUTO_ATTEN_MAX_DB, TxSink};
pub use session::{Source, TxConfig, TxSession, TxState, TxStatus};
