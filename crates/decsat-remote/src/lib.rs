// made by CasualArclamp
//! DecSAT's remote protocol: a radio on one machine, the app on another,
//! over one TCP connection. See `docs/REMOTE.md`.
//!
//! - **Dumb mode** sends the IQ, packed and optionally compressed; the
//!   client's own engine does the work ([`client::RemoteIqSource`]).
//! - **Smart mode** runs the engine on the server and sends only what is
//!   looked at: waterfall, spectrum, constellations, decoder status and the
//!   transport stream ([`client::RemoteEngine`]).
//! - **Shared mode** is smart mode with the radio fixed by the server and
//!   several users, each adding decoders and camping on others'.

pub mod auth;
pub mod client;
pub mod display;
pub mod iq;
pub mod msg;
pub mod radios;
pub mod server;
pub mod ts;
pub mod wire;

mod hub;
mod outq;

pub use hub::served;

/// The port a server listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 5262;
