// made by CasualArclamp
//! Physical-layer framing for DVB-S2/S2X.
//!
//! M1 covers the PLHEADER: the 26-symbol SOF, the 64-symbol PLS code and the
//! pi/2-BPSK mapping both are carried in, plus the PLFRAME geometry (slots,
//! pilot blocks) that the PLS code implies; BBFRAMEs and their header
//! (`bbframe`), VL-SNR (`vlsnr`) and S2X super-frames (`superframe`).

pub mod bbframe;
pub mod defs;
pub mod pi2bpsk;
pub mod plsc;
pub mod rm;
pub mod scramble;
pub mod superframe;
pub mod sync;
pub mod vlsnr;
mod vlsnr_tables;

pub use bbframe::{
    BBHEADER_LEN, BbHeader, BbHeaderError, RollOffReader, StreamFormat, bb_scramble, crc8, ro_bits,
};
pub use defs::*;
pub use plsc::{PlsInfo, PlscDecoder, PlscDemap, PlscEncoder};
pub use rm::ReedMuller;
pub use scramble::PlScrambler;
pub use sync::{PlHeaderCorrelator, sof_differential};
