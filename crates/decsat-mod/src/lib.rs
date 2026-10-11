//! DVB-S2 modulator.
//!
//! Builds real DVB-S2 signals for tests and test captures: TS-mode BBFRAMEs
//! (`fec`), BCH + LDPC encoding, interleaving and mapping, PLFRAMEs
//! (`framer`: header, pilots, scrambling) and pulse shaping (`shaper`). The
//! HackRF transmit path and a user-facing modulator arrive in M6.

pub mod datv;
pub mod fec;
pub mod framer;
pub mod shaper;

pub use datv::{DatvConfig, DatvModulator, TsFile, TsInput, TsTest, TsUdp};
pub use fec::{BbFrameSource, FecEncoder, GseBbFramer, TsBbFramer};
pub use framer::{FrameSpec, PlFramer};
pub use shaper::Shaper;
