//! Satellite modem formats beyond DVB-S2: DVB-S (EN 300 421) and Intelsat
//! IESS-315's turbo product code `tpc_2964`; the IESS Viterbi/Reed–Solomon
//! modes after.
//!
//! - [`conv`]: the K = 7 convolutional code, puncturing, Viterbi decoding.
//! - [`rs`]: shortened Reed–Solomon codes over GF(256).
//! - [`interleave`]: Forney convolutional interleaving.
//! - [`dvbs`]: the DVB-S transmitter and blind receiver.
//! - [`tpc`]: turbo product codes (extended Hamming products, Chase–Pyndiah).
//! - [`tpc2964`]: `tpc_2964` frames, their structure found from the signal.
//! - [`payload`]: what a modem's data carry (HDLC, MPEG-TS) and how they are
//!   scrambled, found from the data.
//! - [`text`]: live text search in any bit stream, every reading at once.
//! - [`g728`]: ITU-T G.728 LD-CELP 16 kbit/s voice, and finding it in two
//!   bits of a timeslot (the CDM-600L's Drop & Insert voice).

pub mod cid;
pub mod conv;
pub mod dandi;
pub mod dvbs;
pub mod e1;
pub mod fastlink;
pub mod g728;
pub mod ibs;
pub mod interleave;
pub mod paradise;
pub mod payload;
pub mod rs;
pub mod slotpkt;
pub mod tdm257;
pub mod tdmfast;
pub mod text;
pub mod tpc;
pub mod tpc2964;
