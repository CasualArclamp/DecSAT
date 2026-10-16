// made by CasualArclamp
//! GSE de-encapsulation and reassembly to IP, standard and the variants seen
//! on real links (`docs/DESIGN.md` §6, Appendix B).
//!
//! - [`decode`]: one GSE reader per variant, with fragment reassembly.
//! - [`auto`]: all variants at once, the one yielding valid IP chosen.
//! - [`encap`]: the transmit side, for test signals and the modulator.
//! - [`hem`]: GSE-HEM (S2X), packets sliced across BBFRAMEs put back whole.

pub mod auto;
pub mod decode;
pub mod encap;
pub mod hem;

pub use auto::{GseIp, IpPacket, LabelStats, Source, VariantReport, ip_of};
pub use decode::{FragMode, GseDecoder, GseStats, Label, LengthMode, Pdu, Variant};
pub use encap::GseEncapsulator;
pub use hem::{HemDeframer, HemStats};
