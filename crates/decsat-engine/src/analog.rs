//! Stateful analogue demodulators for the existing VFO -> PCM audio path.
//! Does not modify the DVB, IQ acquisition or GUI pipelines.
use crate::vfo::DecoderKind;
use decsat_audio::player::{AudioPlayer, OutputKind};
use decsat_core::Iq;
use std::f64::consts::TAU;

const PCM_RATE: u32 = 48_000;
const EPS: f32 = 1e-9;
#[inline]
fn coeff(hz: f64, rate: f64) -> f32 {
    (1.0 - (-TAU * hz.max(0.01) / rate.max(1.0)).exp()) as f32
}

/// Three cascaded low-pass poles. This removes substantially more out-of-band
/// energy than the original one-pole filter before PCM decimation.
#[derive(Default)]
struct LowPass3 { a: f32, b: f32, c: f32 }
impl LowPass3 {
    #[inline]
    fn tick(&mut self, value: f32, k: f32) -> f32 {
        self.a += k * (value - self.a);
        self.b += k * (self.a - self.b);
        self.c += k * (self.b - self.c);
        self.c
    }
}

pub struct AnalogReceiver {
    mode: DecoderKind,
    rate: f64,
    player: Option<AudioPlayer>,
    previous: Iq,
    power: f32,
    dc: f32,
    fm_dc: f32,
    mono: LowPass3,
    difference: LowPass3,
    ssb_i: LowPass3,
    ssb_q: LowPass3,
    deemph_l: f32,
    deemph_r: f32,
    phase: f64,
    audio_phase: f64,
    pilot_i: f32,
    pilot_q: f32,
    pilot_phase: f64,
    pilot_omega: f64,
    pilot_level: f32,
    last_error: Option<String>,
    rf_a: Iq, rf_b: Iq, rf_c: Iq,
    decim_phase: usize,
    rds: crate::analog_rds::RdsDecoder,
    tv: crate::analog_tv::MonochromeTv,
}
impl AnalogReceiver {
    pub fn new(mode: DecoderKind, rate: f64, _rf_bandwidth_hz: f64) -> Self {
        let (player, last_error) = match AudioPlayer::start_pcm(mode.label(), OutputKind::Device) {
            Ok(p) => (Some(p), None),
            Err(e) => (None, Some(e)),
        };
        let rate = rate.max(1.0);
        Self {
            mode, rate, player,
            previous: Iq::new(1.0, 0.0), power: 0.1,
            dc: 0.0, fm_dc: 0.0,
            mono: LowPass3::default(), difference: LowPass3::default(),
            ssb_i: LowPass3::default(), ssb_q: LowPass3::default(),
            deemph_l: 0.0, deemph_r: 0.0, phase: 0.0, audio_phase: 0.0,
            pilot_i: 0.0, pilot_q: 0.0, pilot_phase: 0.0,
            pilot_omega: TAU * 19_000.0 / (rate / (rate / 384_000.0).floor().max(1.0)), pilot_level: 0.0,
            last_error, rds: crate::analog_rds::RdsDecoder::default(),
            tv: crate::analog_tv::MonochromeTv::default(),
            rf_a: Iq::new(0.0, 0.0), rf_b: Iq::new(0.0, 0.0),
            rf_c: Iq::new(0.0, 0.0), decim_phase: 0,
        }
    }
    pub fn video_frame(&self) -> &[u8] { self.tv.frame() }
    pub fn status_message(&self) -> String {
        if let Some(e) = &self.last_error {
            format!("{}: audio error: {e}", self.mode.label())
        } else {
            if self.mode == DecoderKind::Wfm {
                format!("WFM stereo / RDS — {}", self.rds.summary())
            } else if matches!(self.mode, DecoderKind::AtvAm | DecoderKind::AtvFm) {
                format!("{} — PAL monochrome, pulse sync ({} H lines, {} V events)",
                    self.mode.label(), self.tv.sync_count(), self.tv.vertical_sync_count())
            } else { format!("{} — 48 kHz audio", self.mode.label()) }
        }
    }
    /// Use the existing VFO bandwidth control to set the channel width.
    /// USB/LSB use complex frequency translation and a 3-pole audio filter,
    /// with a fixed 1700 Hz translation; stopband rejection is limited.
    /// CW uses an 800 Hz pitch. Dedicated live BFO/AGC/squelch controls
    /// require GUI settings plumbing and are intentionally not represented
    /// here as if already available.
    pub fn process(&mut self, iq: &[Iq]) {
        if iq.is_empty() { return; }
        if matches!(self.mode, DecoderKind::AtvAm | DecoderKind::AtvFm) {
            self.tv.process(iq, self.rate, self.mode == DecoderKind::AtvFm);
            return;
        }
        let mut pcm = Vec::with_capacity(
            ((iq.len() as f64 * PCM_RATE as f64 / self.rate).ceil() as usize + 2) * 2
        );
        // Wideband VFOs can output tens of MS/s. Reduce that to approximately
        // 384 kS/s *before* costly trigonometric/FM/stereo processing.
        // Three cascaded complex poles attenuate aliases, retaining the FM
        // multiplex below ~60 kHz. Keep phase/history across IQ blocks.
        let decim = (self.rate / 384_000.0).floor().max(1.0) as usize;
        let rate = self.rate / decim as f64;
        let rf_k = coeff(90_000.0, self.rate);
        let is_fm = matches!(self.mode, DecoderKind::Nfm | DecoderKind::Wfm);
        let audio_hz = match self.mode {
            DecoderKind::Wfm => 15_000.0,
            DecoderKind::Am | DecoderKind::Dsb => 5_000.0,
            DecoderKind::Cw => 800.0,
            _ => 3_500.0,
        };
        let audio_k = coeff(audio_hz, rate);
        let ssb_k = coeff(2_300.0, rate);
        let diff_k = coeff(15_000.0, rate);
        // Broadcast FM commonly uses 50 us in Europe; NFM voice generally
        // uses 300 us or 750 us depending on service. 300 us chosen here.
        let de_k = coeff(if self.mode == DecoderKind::Wfm { 3183.1 } else { 530.5 }, rate);
        let dc_k = coeff(15.0, rate);
        let agc_k = coeff(12.0, rate);
        let pilot_k = coeff(500.0, rate);
        let pilot_energy_k = coeff(8.0, rate);
        let pll_limit = TAU * 300.0 / rate;
        for &raw in iq {
            let x = if decim > 1 {
                self.rf_a += (raw - self.rf_a) * rf_k;
                self.rf_b += (self.rf_a - self.rf_b) * rf_k;
                self.rf_c += (self.rf_b - self.rf_c) * rf_k;
                self.decim_phase += 1;
                if self.decim_phase < decim { continue; }
                self.decim_phase = 0;
                self.rf_c
            } else { raw };
            let amp2 = x.norm_sqr();
            self.power += agc_k * (amp2 - self.power);
            let gain = 0.6 / self.power.max(EPS).sqrt();
            let mut left: f32;
            let mut right: f32;
            if is_fm {
                let delta = x * self.previous.conj();
                let deviation = if self.mode == DecoderKind::Nfm { 5_000.0 } else { 75_000.0 };
                let multiplex = (delta.im.atan2(delta.re)
                    * (rate / (TAU * deviation)) as f32).clamp(-4.0, 4.0);
                self.fm_dc += dc_k * (multiplex - self.fm_dc);
                let multiplex = multiplex - self.fm_dc;
                if self.mode == DecoderKind::Wfm && rate >= 140_000.0 {
                    self.rds.feed(multiplex, self.pilot_phase, rate);
                }
                let mono = self.mono.tick(multiplex, audio_k);
                if self.mode == DecoderKind::Wfm && rate >= 120_000.0 {
                    // Coherent 19-kHz pilot detector / simple digital PLL.
                    // Double the recovered pilot phase for 38-kHz L-R DSB.
                    let cs = self.pilot_phase.cos() as f32;
                    let sn = self.pilot_phase.sin() as f32;
                    self.pilot_i += pilot_k * (multiplex * cs - self.pilot_i);
                    self.pilot_q += pilot_k * (multiplex * -sn - self.pilot_q);
                    let error = self.pilot_q.atan2(self.pilot_i) as f64;
                    self.pilot_omega += (0.00000012 * error).clamp(-pll_limit, pll_limit);
                    self.pilot_omega = self.pilot_omega.clamp(
                        TAU * 18_800.0 / rate, TAU * 19_200.0 / rate
                    );
                    self.pilot_phase = (self.pilot_phase + self.pilot_omega
                        + 0.0015 * error).rem_euclid(TAU);
                    let amplitude = self.pilot_i.hypot(self.pilot_q);
                    self.pilot_level += pilot_energy_k * (amplitude - self.pilot_level);
                    let difference = self.difference.tick(
                        multiplex * (2.0 * self.pilot_phase).cos() as f32 * 2.0,
                        diff_k
                    );
                    // Quiet mono fallback when no reliable 19-kHz pilot.
                    let blend = ((self.pilot_level - 0.002) / 0.01).clamp(0.0, 1.0);
                    left = mono + difference * blend;
                    right = mono - difference * blend;
                } else {
                    left = mono;
                    right = mono;
                }
                self.deemph_l += de_k * (left - self.deemph_l);
                self.deemph_r += de_k * (right - self.deemph_r);
                left = self.deemph_l;
                right = self.deemph_r;
            } else {
                let y = match self.mode {
                    DecoderKind::Am => amp2.sqrt() * gain,
                    DecoderKind::Dsb => x.re * gain,
                    DecoderKind::Usb | DecoderKind::Lsb | DecoderKind::Cw => {
                        let offset = match self.mode {
                            DecoderKind::Usb => 1_700.0,
                            DecoderKind::Lsb => -1_700.0,
                            _ => -800.0,
                        };
                        self.phase = (self.phase - TAU * offset / rate).rem_euclid(TAU);
                        let c = self.phase.cos() as f32;
                        let s = self.phase.sin() as f32;
                        let rotated_i = x.re * c - x.im * s;
                        let rotated_q = x.re * s + x.im * c;
                        let k = if self.mode == DecoderKind::Cw { coeff(500.0, rate) } else { ssb_k };
                        { self.ssb_q.tick(rotated_q, k); self.ssb_i.tick(rotated_i, k) * gain }
                    }
                    _ => 0.0,
                };
                if self.mode == DecoderKind::Am {
                    self.dc += dc_k * (y - self.dc);
                    left = self.mono.tick(y - self.dc, audio_k);
                } else if matches!(self.mode, DecoderKind::Usb | DecoderKind::Lsb | DecoderKind::Cw) {
                    left = y;
                } else {
                    left = self.mono.tick(y, audio_k);
                }
                right = left;
            }
            self.previous = x;
            self.audio_phase += PCM_RATE as f64 / rate;
            if self.audio_phase >= 1.0 {
                self.audio_phase -= 1.0;
                pcm.push((left * 0.7).clamp(-1.0, 1.0));
                pcm.push((right * 0.7).clamp(-1.0, 1.0));
            }
        }
        if !pcm.is_empty()
            && let Some(p) = &mut self.player { p.push_pcm(PCM_RATE, pcm); }
    }
}
