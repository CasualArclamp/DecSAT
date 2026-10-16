// made by CasualArclamp
//! Forward error correction and constellation mapping for DVB-S2/S2X.
//!
//! - [`params`]: code sizes per frame length and rate (Tables 5a/5b).
//! - [`bch`]: the outer BCH code.
//! - [`ldpc`]: the inner LDPC code, encoder and layered min-sum decoder.
//! - [`constellation`]: the S2 constellations and their bit labels.
//! - [`demap`]: bit interleaving, mapping, and max-log soft demapping.
//!
//! - [`apsk_tables`]: the S2X constellation and interleaver tables.
//! - [`vlsnr`]: the shortened and punctured codes of S2X VL-SNR frames.

pub mod apsk_tables;
pub mod bch;
pub mod constellation;
pub mod demap;
pub mod ldpc;
pub mod params;
pub mod vlsnr;

pub use bch::{Bch, BchError};
pub use constellation::Constellation;
pub use ldpc::{DecodeOutcome, LdpcCode, LdpcDecoder};
pub use params::FecParams;
