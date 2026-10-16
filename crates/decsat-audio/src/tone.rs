// made by CasualArclamp
//! A tone whose pitch is set live: the beacon decoder's pointing aid — its
//! pitch follows the beacon's level, so a dish can be peaked by ear.
//!
//! The sound card's callback makes the sine itself (no buffering, so a new
//! pitch is heard at once), gliding to each new pitch over ~30 ms and
//! fading in and out, at the app's volume and mute, through the output
//! device chosen for the players ([`crate::set_output_device`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::player::gain_now;

struct Shared {
    /// Wanted pitch, Hz (f32 bits); on or silent.
    freq: AtomicU32,
    on: AtomicBool,
}

/// A running tone. Dropping it closes the sound device.
pub struct Tone {
    shared: Arc<Shared>,
    /// Held so the device keeps playing (not `Send`: kept on the thread
    /// that made it).
    _stream: cpal::Stream,
    /// The output device changes count it was opened at.
    pub opened_at: u64,
}

impl Tone {
    /// Open the sound output and start silent.
    pub fn start() -> Result<Tone, String> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        let host = cpal::default_host();
        let chosen = crate::output_device().and_then(|want| {
            host.output_devices().ok()?.find(|d| {
                d.description()
                    .map(|n| n.name() == want.as_str())
                    .unwrap_or(false)
            })
        });
        let dev = match chosen {
            Some(d) => d,
            None => host
                .default_output_device()
                .ok_or("no sound output device")?,
        };
        let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
        let (rate, ch, fmt) = (
            cfg.sample_rate() as f32,
            cfg.channels() as usize,
            cfg.sample_format(),
        );
        let shared = Arc::new(Shared {
            freq: AtomicU32::new(600f32.to_bits()),
            on: AtomicBool::new(false),
        });
        let config: cpal::StreamConfig = cfg.into();
        let err = |e| eprintln!("pointing tone: {e}");
        let sh = shared.clone();
        let stream = match fmt {
            cpal::SampleFormat::F32 => {
                dev.build_output_stream(config, callback::<f32>(sh, ch, rate), err, None)
            }
            cpal::SampleFormat::I16 => {
                dev.build_output_stream(config, callback::<i16>(sh, ch, rate), err, None)
            }
            cpal::SampleFormat::U16 => {
                dev.build_output_stream(config, callback::<u16>(sh, ch, rate), err, None)
            }
            cpal::SampleFormat::I32 => {
                dev.build_output_stream(config, callback::<i32>(sh, ch, rate), err, None)
            }
            f => return Err(format!("sound device format {f} is not handled")),
        }
        .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(Tone {
            shared,
            _stream: stream,
            opened_at: crate::player::output_changes(),
        })
    }

    /// Sound at `freq_hz`, or (`None`) fall silent.
    pub fn set(&self, freq_hz: Option<f32>) {
        match freq_hz {
            Some(f) => {
                self.shared
                    .freq
                    .store(f.clamp(50.0, 8000.0).to_bits(), Ordering::Relaxed);
                self.shared.on.store(true, Ordering::Relaxed);
            }
            None => self.shared.on.store(false, Ordering::Relaxed),
        }
    }
}

fn callback<T>(
    sh: Arc<Shared>,
    ch: usize,
    rate: f32,
) -> impl FnMut(&mut [T], &cpal::OutputCallbackInfo) + Send + 'static
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let mut phase = 0f32;
    let mut freq = 600f32;
    let mut env = 0f32;
    // One-pole glides: ~30 ms for the pitch, ~20 ms for the fade.
    let glide = 1.0 - (-1.0 / (0.03 * rate)).exp();
    let fade = 1.0 - (-1.0 / (0.02 * rate)).exp();
    move |data: &mut [T], _| {
        let want = f32::from_bits(sh.freq.load(Ordering::Relaxed));
        let target = if sh.on.load(Ordering::Relaxed) {
            1.0
        } else {
            0.0
        };
        // The players' volume, a little under (a pure tone is loud).
        let g = 0.3 * gain_now();
        for frame in data.chunks_mut(ch.max(1)) {
            freq += glide * (want - freq);
            env += fade * (target - env);
            phase += std::f32::consts::TAU * freq / rate;
            if phase > std::f32::consts::TAU {
                phase -= std::f32::consts::TAU;
            }
            let v = g * env * phase.sin();
            for s in frame.iter_mut() {
                *s = T::from_sample(v);
            }
        }
    }
}
