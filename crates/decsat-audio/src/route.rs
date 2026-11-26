// made by CasualArclamp
//! Sending a player's sound somewhere other than this machine's sound card.
//!
//! A decoder running on a DecSAT server (smart mode) still makes its sound
//! through an [`AudioPlayer`](crate::AudioPlayer), as it would locally. The
//! VFO's threads are given an [`AudioRoute`] ([`set_current`]), and a player
//! started on one of them takes it up. It then opens no sound device:
//! each block it decodes goes to the route's [`AudioSink`] at its own rate,
//! before any resampling, and the sink sends it on to the client.
//!
//! Over the link the sound travels as Opus ([`LinkEncoder`] at the server,
//! [`LinkDecoder`] at the client): 48 kHz stereo, 20 ms frames, about
//! 48 kbit/s a player whatever the decoder made (8 kHz voice, 48 kHz
//! programme audio).

use std::ffi::c_int;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use decsat_opus_sys as opus;

use crate::decode::{OpusDecoder, opus_error};
use crate::resample::Resampler;

/// Where a routed player's sound goes.
pub trait AudioSink: Send + Sync {
    /// Decoded sound from player `stream`: interleaved stereo at `rate`.
    /// `label` says what it is ("Inmarsat Aero voice").
    fn pcm(&self, stream: u64, label: &str, rate: u32, stereo: &[f32]);
    /// Player `stream` has stopped.
    fn end(&self, stream: u64);
}

/// A cheap handle on an [`AudioSink`], carried in a VFO's settings.
// Rust note: `Arc<dyn AudioSink>` is a shared pointer to any type that
// implements the trait; cloning it copies the pointer, not the sink.
#[derive(Clone)]
pub struct AudioRoute(pub Arc<dyn AudioSink>);

impl AudioRoute {
    pub fn new(sink: impl AudioSink + 'static) -> AudioRoute {
        AudioRoute(Arc::new(sink))
    }
}

impl std::fmt::Debug for AudioRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AudioRoute")
    }
}

/// Two routes are the same if they are the same sink.
impl PartialEq for AudioRoute {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

thread_local! {
    // Rust note: a `thread_local!` value is separate for every thread, so a
    // VFO's route never leaks into another VFO's threads.
    static CURRENT: std::cell::RefCell<Option<AudioRoute>> = const { std::cell::RefCell::new(None) };
}

/// Players started on this thread from now on send their sound along
/// `route` (`None`: to the sound card, as normal). A thread a VFO spawns
/// passes on its own with [`current`].
pub fn set_current(route: Option<AudioRoute>) {
    CURRENT.with(|c| *c.borrow_mut() = route);
}

/// This thread's route, if it has one.
pub fn current() -> Option<AudioRoute> {
    CURRENT.with(|c| c.borrow().clone())
}

/// A number for each player, unique in this process.
pub(crate) fn next_stream() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The link's audio rate: Opus's own.
pub const LINK_RATE: u32 = 48_000;
/// Stereo frames in one Opus packet: 20 ms.
const FRAME: usize = 960;
/// Bits a second for the link's Opus.
const BITRATE: i32 = 48_000;
/// Room for one packet; at 48 kbit/s a 20 ms one is about 120 bytes.
const MAX_PACKET: usize = 1500;

/// Sound to Opus packets for the link: any rate in, 48 kHz stereo out.
pub struct LinkEncoder {
    enc: NonNull<opus::OpusEncoder>,
    resampler: Option<Resampler>,
    pending: Vec<f32>,
    resampled: Vec<f32>,
}

// SAFETY: the encoder state belongs to this value alone, and libopus keeps
// no shared state for it, so it may move between threads.
unsafe impl Send for LinkEncoder {}

impl LinkEncoder {
    pub fn new() -> Result<LinkEncoder, String> {
        let mut err = 0;
        // SAFETY: a valid rate, channel count and application; `err`
        // outlives the call.
        let p = unsafe {
            opus::opus_encoder_create(
                LINK_RATE as opus::opus_int32,
                2,
                opus::OPUS_APPLICATION_AUDIO,
                &mut err,
            )
        };
        let enc = NonNull::new(p).ok_or_else(|| opus_error(err))?;
        // SAFETY: a live encoder; the bit-rate request takes one opus_int32.
        unsafe {
            opus::opus_encoder_ctl(enc.as_ptr(), opus::OPUS_SET_BITRATE_REQUEST, BITRATE);
        }
        Ok(LinkEncoder {
            enc,
            resampler: None,
            pending: Vec::new(),
            resampled: Vec::new(),
        })
    }

    /// Interleaved stereo at `rate` in; whole Opus packets out (none until
    /// 20 ms has gathered).
    pub fn push(&mut self, rate: u32, stereo: &[f32]) -> Vec<Vec<u8>> {
        if rate == LINK_RATE {
            self.pending.extend_from_slice(stereo);
        } else {
            if self.resampler.as_ref().is_none_or(|r| r.rates().0 != rate) {
                self.resampler = Some(Resampler::new(rate, LINK_RATE));
            }
            self.resampled.clear();
            let r = self.resampler.as_mut().unwrap();
            r.process(stereo, &mut self.resampled);
            self.pending.extend_from_slice(&self.resampled);
        }
        let mut out = Vec::new();
        let mut buf = [0u8; MAX_PACKET];
        let mut used = 0;
        while self.pending.len() - used >= 2 * FRAME {
            let frame = &self.pending[used..used + 2 * FRAME];
            used += 2 * FRAME;
            // SAFETY: a live encoder; `frame` holds FRAME stereo samples
            // and `buf` MAX_PACKET bytes, the sizes passed.
            let n = unsafe {
                opus::opus_encode_float(
                    self.enc.as_ptr(),
                    frame.as_ptr(),
                    FRAME as c_int,
                    buf.as_mut_ptr(),
                    MAX_PACKET as opus::opus_int32,
                )
            };
            if n > 0 {
                out.push(buf[..n as usize].to_vec());
            }
        }
        self.pending.drain(..used);
        out
    }
}

impl Drop for LinkEncoder {
    fn drop(&mut self) {
        // SAFETY: created by `opus_encoder_create` and destroyed only here.
        unsafe { opus::opus_encoder_destroy(self.enc.as_ptr()) }
    }
}

/// Opus packets from the link back to 48 kHz stereo.
pub struct LinkDecoder(OpusDecoder);

impl LinkDecoder {
    pub fn new() -> Result<LinkDecoder, String> {
        OpusDecoder::new().map(LinkDecoder)
    }

    /// One packet to interleaved stereo at [`LINK_RATE`].
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<f32>, String> {
        let mut out = Vec::new();
        self.0.decode(packet, &mut out)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tone_crosses_the_link() {
        let mut e = LinkEncoder::new().unwrap();
        let mut d = LinkDecoder::new().unwrap();
        // One second of 440 Hz at 8 kHz, as a voice decoder would make it.
        let tone: Vec<f32> = (0..8000)
            .flat_map(|i| {
                let v = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 8000.0).sin();
                [v, v]
            })
            .collect();
        let packets = e.push(8000, &tone);
        assert!(packets.len() >= 45, "{} packets", packets.len());
        let bytes: usize = packets.iter().map(Vec::len).sum();
        assert!(bytes < 10_000, "about 48 kbit/s: {bytes} bytes");
        let mut out = Vec::new();
        for p in &packets {
            out.extend(d.decode(p).unwrap());
        }
        // Past the codec's start-up, the level is the tone's.
        let tail = &out[out.len() / 2..];
        let rms = (tail.iter().map(|v| v * v).sum::<f32>() / tail.len() as f32).sqrt();
        assert!((rms - 0.354).abs() < 0.08, "rms {rms}");
    }
}
