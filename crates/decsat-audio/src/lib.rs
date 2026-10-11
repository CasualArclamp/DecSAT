//! In-app playback and recording of multicast audio (radio over IP over
//! DVB-S/S2: GSE or MPE).
//!
//! - [`depay`]: a stream's UDP payloads to whole frames (RTP, RFC 2250,
//!   RFC 3016 LATM, RFC 3640, raw ADTS/LOAS/MPEG audio, PCM).
//! - [`decode`]: frames to stereo PCM (Symphonia: MPEG audio I–III;
//!   libxaac: AAC with SBR and PS; libopus: Opus).
//! - [`resample`]: to the sound card's rate, drift-trimmed.
//! - [`player`]: the thread and sound output, with volume and pause.
//! - [`record`]: the stream to a file as broadcast.
//!
//! - [`xaac`]: AAC, LC and HE-AAC v1/v2 with SBR and PS, through libxaac.
//!
//! Three decoders: Symphonia (MPEG audio), libxaac (AAC) and libopus (Opus).

pub mod aac;
pub mod bits;
pub mod decode;
pub mod depay;
pub mod es;
pub mod player;
pub mod record;
pub mod resample;
pub mod xaac;

pub use player::{
    AudioHandle, AudioPlayer, OutputKind, PlayState, Status, muted, output_device, output_devices,
    set_muted, set_output_device, set_volume, volume,
};
pub use record::AudioRecorder;
