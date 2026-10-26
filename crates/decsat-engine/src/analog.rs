//! Stateful analogue demodulators for the existing VFO -> PCM audio path.
//! Does not modify the DVB, IQ acquisition or GUI pipelines.
//!
//! One [`AnalogReceiver`] belongs to one VFO whose decoder is NFM, WFM (stereo
//! with RDS), AM, DSB, USB, LSB, CW or analogue TV. For the audio modes each IQ
//! block is brought down to about 384 kS/s, demodulated, resampled to 48 kHz
//! stereo and handed to the audio player; the TV modes go to
//! [`MonochromeTv`](crate::analog_tv::MonochromeTv) instead and produce a picture.
//!
//! Provenance: merged in from the "VMA" snapshot, see `docs/STATUS.md`
//! (2026-10-25).

use std::f64::consts::TAU;

use decsat_audio::player::{AudioPlayer, OutputKind};
use decsat_core::Iq;

use crate::vfo::DecoderKind;

/// Sample rate of the audio handed to the player.
const PCM_RATE: u32 = 48_000;
/// Floor under the power estimate so the AGC gain stays finite.
const EPS: f32 = 1e-9;

/// The audio chain runs at about this rate, in Hz: the input is decimated by the
/// largest whole factor that does not go below it. 384 kS/s keeps the FM
/// multiplex (to ~60 kHz, RDS at 57 kHz included) well clear of aliasing.
const IF_RATE_HZ: f64 = 384_000.0;
/// Corner of the three-pole anti-alias filter used before decimation, in Hz.
const ANTI_ALIAS_HZ: f64 = 90_000.0;

/// The pilot of an FM stereo multiplex, in Hz, and how far the pilot loop may
/// pull away from it.
const PILOT_HZ: f64 = 19_000.0;
const PILOT_MIN_HZ: f64 = 18_800.0;
const PILOT_MAX_HZ: f64 = 19_200.0;
/// Narrowest rate at which the stereo decoder and the RDS decoder run. Below
/// these the multiplex is not sampled finely enough for 38 kHz or 57 kHz.
const STEREO_MIN_RATE_HZ: f64 = 120_000.0;
const RDS_MIN_RATE_HZ: f64 = 140_000.0;

/// Peak frequency deviation, in Hz: broadcast FM and narrow-band FM voice.
const WFM_DEVIATION_HZ: f64 = 75_000.0;
const NFM_DEVIATION_HZ: f64 = 5_000.0;
/// The demodulated multiplex is clamped to this (full scale is about ±1).
const MULTIPLEX_LIMIT: f32 = 4.0;
/// De-emphasis corner frequencies, in Hz: 1 / (2π · 50 µs) and 1 / (2π · 300 µs).
/// Broadcast FM commonly uses 50 us in Europe; NFM voice generally uses 300 us
/// or 750 us depending on service. 300 us chosen here.
const DEEMPH_50US_HZ: f64 = 3183.1;
const DEEMPH_300US_HZ: f64 = 530.5;

/// Audio bandwidth (low-pass corner, Hz) per mode.
const WFM_AUDIO_HZ: f64 = 15_000.0;
const AM_AUDIO_HZ: f64 = 5_000.0;
const CW_AUDIO_HZ: f64 = 800.0;
const SSB_AUDIO_HZ: f64 = 3_500.0;
/// Corner of the low-pass on the single-sideband baseband, and of the narrower
/// one used for CW, in Hz.
const SSB_FILTER_HZ: f64 = 2_300.0;
const CW_FILTER_HZ: f64 = 500.0;
/// Corner of the filter on the stereo difference (L−R) signal, in Hz.
const DIFFERENCE_HZ: f64 = 15_000.0;
/// Frequency shift, in Hz, applied to the complex signal before the single-sideband
/// low-pass (a fixed translation, see `AnalogReceiver::process`): 1.7 kHz for
/// USB, −1.7 kHz for LSB, −800 Hz for CW (the 800 Hz pitch).
const USB_OFFSET_HZ: f64 = 1_700.0;
const LSB_OFFSET_HZ: f64 = -1_700.0;
const CW_OFFSET_HZ: f64 = -800.0;

/// Corner of the slow DC blockers (AM carrier, FM carrier offset), in Hz.
const DC_HZ: f64 = 15.0;
/// Corner of the AGC's power tracker, in Hz.
const AGC_HZ: f64 = 12.0;
/// Corner of the low-pass on the pilot detector's I and Q, in Hz.
const PILOT_DETECT_HZ: f64 = 500.0;
/// Corner of the smoothing on the pilot amplitude, in Hz.
const PILOT_LEVEL_HZ: f64 = 8.0;
/// Largest frequency step per sample the pilot loop may take, as a bandwidth in
/// Hz, and its two loop gains (frequency and phase).
const PLL_STEP_LIMIT_HZ: f64 = 300.0;
const PLL_FREQUENCY_GAIN: f64 = 0.00000012;
const PLL_PHASE_GAIN: f64 = 0.0015;
/// Pilot amplitude at which stereo starts to fade in, and the width of the fade.
/// Below it the output is mono.
const PILOT_BLEND_START: f32 = 0.002;
const PILOT_BLEND_WIDTH: f32 = 0.01;

/// The AGC scales the envelope to about this amplitude.
const AGC_TARGET: f32 = 0.6;
/// Output level before the clamp, so peaks have headroom.
const OUTPUT_GAIN: f32 = 0.7;

/// One-pole smoothing coefficient for a corner of `hz` at sample rate `rate`:
/// `y += k * (x - y)` with this `k` is a first-order low-pass.
#[inline]
fn coeff(hz: f64, rate: f64) -> f32 {
    (1.0 - (-TAU * hz.max(0.01) / rate.max(1.0)).exp()) as f32
}

/// The factor the input rate is divided by to get near [`IF_RATE_HZ`] (1 if it
/// is already below).
fn decimation(rate: f64) -> usize {
    (rate / IF_RATE_HZ).floor().max(1.0) as usize
}

/// Three cascaded low-pass poles. This removes substantially more out-of-band
/// energy than the original one-pole filter before PCM decimation.
#[derive(Default)]
struct LowPass3 {
    a: f32,
    b: f32,
    c: f32,
}

impl LowPass3 {
    /// Run one sample through the three poles; `k` is a [`coeff`] value.
    #[inline]
    fn tick(&mut self, value: f32, k: f32) -> f32 {
        self.a += k * (value - self.a);
        self.b += k * (self.a - self.b);
        self.c += k * (self.b - self.c);
        self.c
    }
}

/// The filter coefficients for one block, all at the post-decimation rate except
/// `rf`, which acts before decimation.
struct Coeffs {
    rf: f32,
    audio: f32,
    ssb: f32,
    cw: f32,
    diff: f32,
    deemph: f32,
    dc: f32,
    agc: f32,
    pilot: f32,
    pilot_level: f32,
    /// Fastest the pilot loop may change its frequency per sample, in rad/sample.
    pll_limit: f64,
}

impl Coeffs {
    /// `input_rate` is the block's sample rate, `rate` the rate after
    /// decimation.
    fn new(mode: DecoderKind, input_rate: f64, rate: f64) -> Self {
        let audio_hz = match mode {
            DecoderKind::Wfm => WFM_AUDIO_HZ,
            DecoderKind::Am | DecoderKind::Dsb => AM_AUDIO_HZ,
            DecoderKind::Cw => CW_AUDIO_HZ,
            _ => SSB_AUDIO_HZ,
        };
        let deemph_hz = if mode == DecoderKind::Wfm {
            DEEMPH_50US_HZ
        } else {
            DEEMPH_300US_HZ
        };
        Self {
            rf: coeff(ANTI_ALIAS_HZ, input_rate),
            audio: coeff(audio_hz, rate),
            ssb: coeff(SSB_FILTER_HZ, rate),
            cw: coeff(CW_FILTER_HZ, rate),
            diff: coeff(DIFFERENCE_HZ, rate),
            deemph: coeff(deemph_hz, rate),
            dc: coeff(DC_HZ, rate),
            agc: coeff(AGC_HZ, rate),
            pilot: coeff(PILOT_DETECT_HZ, rate),
            pilot_level: coeff(PILOT_LEVEL_HZ, rate),
            pll_limit: TAU * PLL_STEP_LIMIT_HZ / rate,
        }
    }
}

/// An analogue-audio (or analogue-TV) demodulator for one VFO. Everything it
/// needs between blocks (filter states, loop phases, the audio resampler's
/// phase) lives in the struct, so blocks can be any length.
pub struct AnalogReceiver {
    mode: DecoderKind,
    /// Input sample rate, Hz.
    rate: f64,
    /// `None` if no audio device could be opened (see `last_error`).
    player: Option<AudioPlayer>,
    /// Previous (decimated) sample, for the FM discriminator.
    previous: Iq,
    /// Smoothed signal power, for the AGC.
    power: f32,
    /// DC trackers for the AM demodulator and the FM discriminator.
    dc: f32,
    fm_dc: f32,
    mono: LowPass3,
    difference: LowPass3,
    ssb_i: LowPass3,
    ssb_q: LowPass3,
    /// De-emphasis state, left and right.
    deemph_l: f32,
    deemph_r: f32,
    /// Phase of the SSB/CW frequency shift, radians.
    phase: f64,
    /// Fractional position of the 48 kHz output clock within an input sample.
    audio_phase: f64,
    /// Pilot detector state (in-phase and quadrature) and loop, radians.
    pilot_i: f32,
    pilot_q: f32,
    pilot_phase: f64,
    pilot_omega: f64,
    /// Smoothed pilot amplitude, to decide between stereo and mono.
    pilot_level: f32,
    /// Why audio output is unavailable, if it is.
    last_error: Option<String>,
    /// The three poles of the complex anti-alias filter used before decimation.
    rf_a: Iq,
    rf_b: Iq,
    rf_c: Iq,
    /// Input samples seen since the last one kept.
    decim_phase: usize,
    rds: crate::analog_rds::RdsDecoder,
    tv: crate::analog_tv::MonochromeTv,
}

impl AnalogReceiver {
    /// A receiver for `mode` fed at `rate` S/s. `_rf_bandwidth_hz` is accepted
    /// but not used.
    pub fn new(mode: DecoderKind, rate: f64, _rf_bandwidth_hz: f64) -> Self {
        // A missing audio device is not fatal: keep the error to show in the
        // status line and carry on without sound.
        let (player, last_error) = match AudioPlayer::start_pcm(mode.label(), OutputKind::Device) {
            Ok(p) => (Some(p), None),
            Err(e) => (None, Some(e)),
        };
        let rate = rate.max(1.0);
        Self {
            mode,
            rate,
            player,
            previous: Iq::new(1.0, 0.0),
            power: 0.1,
            dc: 0.0,
            fm_dc: 0.0,
            mono: LowPass3::default(),
            difference: LowPass3::default(),
            ssb_i: LowPass3::default(),
            ssb_q: LowPass3::default(),
            deemph_l: 0.0,
            deemph_r: 0.0,
            phase: 0.0,
            audio_phase: 0.0,
            pilot_i: 0.0,
            pilot_q: 0.0,
            pilot_phase: 0.0,
            // Start the pilot loop at exactly 19 kHz at the decimated rate.
            pilot_omega: TAU * PILOT_HZ / (rate / decimation(rate) as f64),
            pilot_level: 0.0,
            last_error,
            rds: crate::analog_rds::RdsDecoder::default(),
            tv: crate::analog_tv::MonochromeTv::default(),
            rf_a: Iq::new(0.0, 0.0),
            rf_b: Iq::new(0.0, 0.0),
            rf_c: Iq::new(0.0, 0.0),
            decim_phase: 0,
        }
    }

    /// The analogue-TV picture (empty for the audio modes).
    pub fn video_frame(&self) -> &[u8] {
        self.tv.frame()
    }

    /// One line for the VFO's status display.
    pub fn status_message(&self) -> String {
        if let Some(e) = &self.last_error {
            format!("{}: audio error: {e}", self.mode.label())
        } else if self.mode == DecoderKind::Wfm {
            format!("WFM stereo / RDS — {}", self.rds.summary())
        } else if matches!(self.mode, DecoderKind::AtvAm | DecoderKind::AtvFm) {
            format!(
                "{} — PAL monochrome, pulse sync ({} H lines, {} V events)",
                self.mode.label(),
                self.tv.sync_count(),
                self.tv.vertical_sync_count()
            )
        } else {
            format!("{} — 48 kHz audio", self.mode.label())
        }
    }

    /// Use the existing VFO bandwidth control to set the channel width.
    /// USB/LSB use complex frequency translation and a 3-pole audio filter,
    /// with a fixed 1700 Hz translation; stopband rejection is limited.
    /// CW uses an 800 Hz pitch. Dedicated live BFO/AGC/squelch controls
    /// require GUI settings plumbing and are intentionally not represented
    /// here as if already available.
    pub fn process(&mut self, iq: &[Iq]) {
        if iq.is_empty() {
            return;
        }
        if matches!(self.mode, DecoderKind::AtvAm | DecoderKind::AtvFm) {
            self.tv
                .process(iq, self.rate, self.mode == DecoderKind::AtvFm);
            return;
        }
        let mut pcm = Vec::with_capacity(
            ((iq.len() as f64 * PCM_RATE as f64 / self.rate).ceil() as usize + 2) * 2,
        );
        // Wideband VFOs can output tens of MS/s. Reduce that to approximately
        // 384 kS/s *before* costly trigonometric/FM/stereo processing.
        // Three cascaded complex poles attenuate aliases, retaining the FM
        // multiplex below ~60 kHz. Keep phase/history across IQ blocks.
        let decim = decimation(self.rate);
        let rate = self.rate / decim as f64;
        let coeffs = Coeffs::new(self.mode, self.rate, rate);
        let is_fm = matches!(self.mode, DecoderKind::Nfm | DecoderKind::Wfm);
        for &raw in iq {
            let Some(x) = self.decimate(raw, decim, coeffs.rf) else {
                continue;
            };
            let amp2 = x.norm_sqr();
            self.power += coeffs.agc * (amp2 - self.power);
            let gain = AGC_TARGET / self.power.max(EPS).sqrt();
            let (left, right) = if is_fm {
                self.fm_audio(x, rate, &coeffs)
            } else {
                self.am_ssb_audio(x, amp2, gain, rate, &coeffs)
            };
            self.previous = x;
            // Resample to 48 kHz by taking the input sample each time the
            // output clock passes one (nearest-sample, no interpolation).
            self.audio_phase += PCM_RATE as f64 / rate;
            if self.audio_phase >= 1.0 {
                self.audio_phase -= 1.0;
                pcm.push((left * OUTPUT_GAIN).clamp(-1.0, 1.0));
                pcm.push((right * OUTPUT_GAIN).clamp(-1.0, 1.0));
            }
        }
        // Rust note: `if cond && let Some(p) = ..` is a let-chain; the body
        // runs only when both hold, with `p` bound inside.
        if !pcm.is_empty()
            && let Some(p) = &mut self.player
        {
            p.push_pcm(PCM_RATE, pcm);
        }
    }

    /// Low-pass the input with three complex poles and keep every `decim`-th
    /// sample. Returns the sample to process, or `None` for one that is dropped.
    /// With `decim == 1` the input passes straight through.
    fn decimate(&mut self, raw: Iq, decim: usize, rf_k: f32) -> Option<Iq> {
        if decim <= 1 {
            return Some(raw);
        }
        self.rf_a += (raw - self.rf_a) * rf_k;
        self.rf_b += (self.rf_a - self.rf_b) * rf_k;
        self.rf_c += (self.rf_b - self.rf_c) * rf_k;
        self.decim_phase += 1;
        if self.decim_phase < decim {
            return None;
        }
        self.decim_phase = 0;
        Some(self.rf_c)
    }

    /// NFM or WFM: discriminator, DC removal, optional stereo and RDS, then
    /// de-emphasis. Returns `(left, right)`.
    fn fm_audio(&mut self, x: Iq, rate: f64, c: &Coeffs) -> (f32, f32) {
        // Phase step between this sample and the last is the instantaneous
        // frequency; scale it so the peak deviation is about ±1.
        let delta = x * self.previous.conj();
        let deviation = if self.mode == DecoderKind::Nfm {
            NFM_DEVIATION_HZ
        } else {
            WFM_DEVIATION_HZ
        };
        let multiplex = (delta.im.atan2(delta.re) * (rate / (TAU * deviation)) as f32)
            .clamp(-MULTIPLEX_LIMIT, MULTIPLEX_LIMIT);
        // Remove the slowly varying offset left by a tuning error.
        self.fm_dc += c.dc * (multiplex - self.fm_dc);
        let multiplex = multiplex - self.fm_dc;
        if self.mode == DecoderKind::Wfm && rate >= RDS_MIN_RATE_HZ {
            self.rds.feed(multiplex, self.pilot_phase, rate);
        }
        let mono = self.mono.tick(multiplex, c.audio);
        let (left, right) = if self.mode == DecoderKind::Wfm && rate >= STEREO_MIN_RATE_HZ {
            self.stereo_split(multiplex, mono, rate, c)
        } else {
            (mono, mono)
        };
        self.deemph_l += c.deemph * (left - self.deemph_l);
        self.deemph_r += c.deemph * (right - self.deemph_r);
        (self.deemph_l, self.deemph_r)
    }

    /// Recover the 19 kHz pilot, use it to demodulate the 38 kHz L−R signal and
    /// combine that with `mono` (L+R) into `(left, right)`.
    fn stereo_split(&mut self, multiplex: f32, mono: f32, rate: f64, c: &Coeffs) -> (f32, f32) {
        // Coherent 19-kHz pilot detector / simple digital PLL.
        // Double the recovered pilot phase for 38-kHz L-R DSB.
        let cs = self.pilot_phase.cos() as f32;
        let sn = self.pilot_phase.sin() as f32;
        self.pilot_i += c.pilot * (multiplex * cs - self.pilot_i);
        self.pilot_q += c.pilot * (multiplex * -sn - self.pilot_q);
        let error = self.pilot_q.atan2(self.pilot_i) as f64;
        self.pilot_omega += (PLL_FREQUENCY_GAIN * error).clamp(-c.pll_limit, c.pll_limit);
        self.pilot_omega = self
            .pilot_omega
            .clamp(TAU * PILOT_MIN_HZ / rate, TAU * PILOT_MAX_HZ / rate);
        self.pilot_phase =
            (self.pilot_phase + self.pilot_omega + PLL_PHASE_GAIN * error).rem_euclid(TAU);
        let amplitude = self.pilot_i.hypot(self.pilot_q);
        self.pilot_level += c.pilot_level * (amplitude - self.pilot_level);
        // L−R is double-sideband around 38 kHz; mixing with cos(2·pilot) and
        // doubling restores its amplitude.
        let difference = self.difference.tick(
            multiplex * (2.0 * self.pilot_phase).cos() as f32 * 2.0,
            c.diff,
        );
        // Quiet mono fallback when no reliable 19-kHz pilot.
        let blend = ((self.pilot_level - PILOT_BLEND_START) / PILOT_BLEND_WIDTH).clamp(0.0, 1.0);
        (mono + difference * blend, mono - difference * blend)
    }

    /// AM, DSB, USB, LSB or CW: returns the same sample for `(left, right)`.
    /// `amp2` is the sample's power and `gain` the AGC gain.
    fn am_ssb_audio(&mut self, x: Iq, amp2: f32, gain: f32, rate: f64, c: &Coeffs) -> (f32, f32) {
        let y = match self.mode {
            DecoderKind::Am => amp2.sqrt() * gain,
            DecoderKind::Dsb => x.re * gain,
            DecoderKind::Usb | DecoderKind::Lsb | DecoderKind::Cw => {
                let offset = match self.mode {
                    DecoderKind::Usb => USB_OFFSET_HZ,
                    DecoderKind::Lsb => LSB_OFFSET_HZ,
                    _ => CW_OFFSET_HZ,
                };
                // Rotate the complex signal by the offset (frequency shift),
                // then low-pass; the real part is the audio.
                self.phase = (self.phase - TAU * offset / rate).rem_euclid(TAU);
                let cs = self.phase.cos() as f32;
                let sn = self.phase.sin() as f32;
                let rotated_i = x.re * cs - x.im * sn;
                let rotated_q = x.re * sn + x.im * cs;
                let k = if self.mode == DecoderKind::Cw {
                    c.cw
                } else {
                    c.ssb
                };
                // The Q arm is filtered too, but only its state is kept: its
                // output is not used.
                self.ssb_q.tick(rotated_q, k);
                self.ssb_i.tick(rotated_i, k) * gain
            }
            _ => 0.0,
        };
        let left = if self.mode == DecoderKind::Am {
            // Remove the carrier (DC), then limit the audio bandwidth.
            self.dc += c.dc * (y - self.dc);
            self.mono.tick(y - self.dc, c.audio)
        } else if matches!(
            self.mode,
            DecoderKind::Usb | DecoderKind::Lsb | DecoderKind::Cw
        ) {
            y
        } else {
            self.mono.tick(y, c.audio)
        };
        (left, left)
    }
}
