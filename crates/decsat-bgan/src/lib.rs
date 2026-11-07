// made by CasualArclamp
//! BGAN forward-link receiver: a Rust port of DecBGAN (Python, same author),
//! written against ETSI TS 102 744. See `docs/BGAN.md`.
pub mod fec;
pub mod frame;
pub mod frontend;
pub mod layers;
pub mod rx;
pub mod session;
pub mod spec;
pub mod tables;
pub mod turbo;
pub mod tx;
