// made by CasualArclamp
//! The wideband front end: the source, the waterfall, carrier detection, and
//! the fan-out to every VFO.
//!
//! One thread reads the source in blocks of about 1/25 s. Each block becomes a
//! waterfall row, feeds a smoothed spectrum (from which carriers are detected
//! twice a second), and is shared — by `Arc`, not copied — with every VFO
//! worker. The GUI only ever takes cheap snapshots: the front status without
//! rows, and whichever waterfall rows it has not seen yet.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use decsat_core::Iq;
use decsat_io::IqSource;

use crate::carriers::{Carrier, detect_carriers, resolve_narrow};
use crate::spectrum::Spectrum;
use crate::vfo::{self, VfoHandle, VfoId, VfoSettings, VfoStatus};

/// Front-end options.
#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// Wideband FFT size: the waterfall's horizontal resolution.
    pub fft_size: usize,
    /// Waterfall rows (and blocks) per second.
    pub rows_per_sec: f64,
    /// Play files at their real sample rate rather than as fast as possible.
    pub realtime: bool,
    /// Rewind a file at its end.
    pub loop_file: bool,
    /// How far above the floor a carrier must stand to be detected, dB.
    pub carrier_snr_db: f32,
    /// Subtract the IQ mean before everything else: the spike a
    /// direct-conversion receiver (the HackRF) leaves at the centre.
    pub dc_removal: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions {
            fft_size: 4096,
            rows_per_sec: 25.0,
            realtime: true,
            loop_file: true,
            carrier_snr_db: 6.0,
            dc_removal: true,
        }
    }
}

/// What the source is doing.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum SourceState {
    #[default]
    Running,
    Paused,
    /// A file reached its end and is not looping.
    Ended,
    Failed(String),
}

/// Snapshot of the front end, minus the waterfall rows (see
/// [`Engine::new_rows`]).
#[derive(Debug, Clone, Default)]
pub struct FrontStatus {
    pub sample_rate: f64,
    pub center_freq: f64,
    pub fft_size: usize,
    /// Smoothed wideband spectrum, dB, FFT-shifted.
    pub spectrum_db: Vec<f32>,
    pub carriers: Vec<Carrier>,
    /// Bands whose power comes and goes — TDMA channels (or Bluetooth in a
    /// HackRF's IF: [`crate::bursty::bluetooth_offsets`]) — from the last
    /// couple of seconds, and how many times they have been taken.
    pub burst_bands: Vec<crate::bursty::BurstBand>,
    pub burst_takes: u64,
    pub source: String,
    pub state: SourceState,
    /// Samples read since start.
    pub samples: u64,
    /// Front-end thread load (fraction of real time).
    pub load: f32,
    /// Rows produced so far (the sequence number of the newest).
    pub row_seq: u64,
    /// The samples' RMS level, dB relative to a full-scale rail (as
    /// `decsat record` reports it), smoothed over a few blocks. Above
    /// [`STRONG_INPUT_DBFS`] the HackRF's RF chain is overloading.
    pub input_dbfs: f32,
}

/// Burst-meter takes (2 s each) shown together on the waterfall.
const BURST_TAKES: usize = 4;

/// An input level above this is too strong for the HackRF: its LNA and
/// mixer compress long before the ADC clips, and the products land on
/// carriers, which then slip and lose frames. Measured on a 14 dB TPC 2964
/// carrier in a full Ku-band slice: at LNA 40 (−15 dBFS) 19 slips in 8 s
/// and most frames lost, at LNA 32 (−23 dBFS) 8 slips and a third lost,
/// at LNA 24 (−31 dBFS) 2 slips and every frame decoded; the ADC never
/// clipped (0.0001 % of samples past 120 of 127).
pub const STRONG_INPUT_DBFS: f32 = -25.0;

/// Recent waterfall rows kept for the GUI to collect. At 25 rows/s this is a
/// few seconds' grace for a GUI that stalls.
const ROW_BACKLOG: usize = 128;
/// FFTs averaged per waterfall row, spread over the block.
const SEGMENTS_PER_ROW: usize = 48;

struct Shared {
    front: Mutex<FrontStatus>,
    rows: Mutex<VecDeque<(u64, Vec<f32>)>>,
    vfos: Mutex<BTreeMap<VfoId, VfoHandle>>,
    /// Burst plans passed between the VFOs.
    plans: Arc<crate::plan_feed::PlanFeed>,
}

enum Cmd {
    Pause(bool),
    DcRemoval(bool),
    FftSize(usize),
    Stop,
}

/// The running receiver: front end plus VFOs.
pub struct Engine {
    shared: Arc<Shared>,
    cmd: Sender<Cmd>,
    /// A file played as fast as it can be: VFOs are waited for rather than
    /// dropping blocks, so nothing is lost (`decsat decode --fast`).
    lossless: bool,
    thread: Option<JoinHandle<()>>,
    next_id: VfoId,
    sample_rate: f64,
}

impl Engine {
    /// Start reading `source`.
    pub fn start(source: Box<dyn IqSource>, opts: EngineOptions) -> Engine {
        let sample_rate = source.sample_rate();
        let lossless = !opts.realtime && !source.is_live();
        let shared = Arc::new(Shared {
            front: Mutex::new(FrontStatus {
                sample_rate,
                center_freq: source.center_freq(),
                fft_size: opts.fft_size,
                source: source.describe(),
                ..Default::default()
            }),
            rows: Mutex::new(VecDeque::with_capacity(ROW_BACKLOG)),
            vfos: Mutex::new(BTreeMap::new()),
            plans: Default::default(),
        });
        let (cmd, rx) = mpsc::channel();
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("frontend".into())
                .spawn(move || run(source, opts, shared, rx))
                .expect("spawning the front-end thread")
        };
        Engine {
            shared,
            cmd,
            lossless,
            thread: Some(thread),
            next_id: 1,
            sample_rate,
        }
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    pub fn front(&self) -> FrontStatus {
        self.shared.front.lock().unwrap().clone()
    }

    /// Waterfall rows newer than `after`, oldest first, with their sequence
    /// numbers.
    pub fn new_rows(&self, after: u64) -> Vec<(u64, Vec<f32>)> {
        self.shared
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|(seq, _)| *seq > after)
            .cloned()
            .collect()
    }

    pub fn set_paused(&self, paused: bool) {
        let _ = self.cmd.send(Cmd::Pause(paused));
    }

    /// Turn DC-spike removal on or off without restarting the source.
    pub fn set_dc_removal(&self, on: bool) {
        let _ = self.cmd.send(Cmd::DcRemoval(on));
    }

    /// Change the waterfall's FFT size without restarting the source — a
    /// radio stays open and the VFOs keep running. Sizes that are not a power
    /// of two of at least 64 are ignored.
    pub fn set_fft_size(&self, n: usize) {
        if n.is_power_of_two() && n >= 64 {
            let _ = self.cmd.send(Cmd::FftSize(n));
        }
    }

    /// Add a VFO; it starts at once.
    pub fn add_vfo(&mut self, settings: VfoSettings) -> VfoId {
        let id = self.next_id;
        self.next_id += 1;
        let link = crate::plan_feed::PlanLink {
            id,
            feed: Arc::clone(&self.shared.plans),
        };
        let handle = vfo::spawn(self.sample_rate, settings, self.lossless, Some(link));
        self.shared.vfos.lock().unwrap().insert(id, handle);
        id
    }

    /// Change a VFO's settings. Never blocks, however busy the VFO is, so it is
    /// safe to call every frame while a VFO is being dragged.
    pub fn update_vfo(&self, id: VfoId, settings: VfoSettings) {
        if let Some(h) = self.shared.vfos.lock().unwrap().get_mut(&id)
            && h.settings != settings
        {
            h.post_settings(settings);
        }
    }

    /// The radio was retuned and a VFO's offset moved by `by` Hz (the
    /// retune's opposite) so that it stays on its carrier: its decoder
    /// carries on rather than starting afresh, as it would for a VFO moved
    /// to another carrier. Never blocks, as [`update_vfo`](Self::update_vfo).
    pub fn carry_vfo(&self, id: VfoId, settings: VfoSettings, by: f64) {
        if let Some(h) = self.shared.vfos.lock().unwrap().get_mut(&id)
            && h.settings != settings
        {
            h.post_carried(settings, by);
        }
    }

    /// Delete a VFO. Returns at once: its threads stop after the block in
    /// hand and are joined in the background.
    pub fn remove_vfo(&self, id: VfoId) {
        self.shared.plans.forget(id);
        let h = self.shared.vfos.lock().unwrap().remove(&id);
        if let Some(h) = h {
            h.discard();
        }
    }

    pub fn vfo_ids(&self) -> Vec<VfoId> {
        self.shared.vfos.lock().unwrap().keys().copied().collect()
    }

    pub fn vfo_settings(&self, id: VfoId) -> Option<VfoSettings> {
        self.shared
            .vfos
            .lock()
            .unwrap()
            .get(&id)
            .map(|h| h.settings.clone())
    }

    /// A file played losslessly (not in real time) has ended and every VFO
    /// has worked through what it was given — its own blocks and its FEC
    /// or CID thread's queue. Wait for this before reading final results;
    /// give the VFOs one more status tick (~0.3 s) after it turns true.
    pub fn settled(&self) -> bool {
        matches!(self.front().state, SourceState::Ended) && self.pending_work() == 0
    }

    /// Blocks and frames handed to the VFOs and their threads and not yet
    /// worked through, all VFOs together.
    pub fn pending_work(&self) -> u64 {
        self.shared
            .vfos
            .lock()
            .unwrap()
            .values()
            .map(|h| h.pending.get())
            .sum()
    }

    /// Stop every VFO without waiting for its queues: what is left is
    /// dropped (for leaving after a report, when a decoder is still far
    /// behind).
    pub fn discard_vfos(&self) {
        let vfos = std::mem::take(&mut *self.shared.vfos.lock().unwrap());
        for (_, h) in vfos {
            h.discard();
        }
    }

    pub fn vfo_status(&self, id: VfoId) -> Option<VfoStatus> {
        let st = self
            .shared
            .vfos
            .lock()
            .unwrap()
            .get(&id)
            .map(|h| Arc::clone(&h.status))?;
        let s = st.lock().unwrap().clone();
        Some(s)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let vfos = std::mem::take(&mut *self.shared.vfos.lock().unwrap());
        for (_, h) in vfos {
            h.stop();
        }
    }
}

fn run(mut source: Box<dyn IqSource>, opts: EngineOptions, shared: Arc<Shared>, rx: Receiver<Cmd>) {
    let rate = source.sample_rate();
    // A block is one waterfall row: 1/rows_per_sec of signal, but never less
    // than two FFTs. Both change with the FFT size, so they are `mut`.
    let block_for = |fft: usize| ((rate / opts.rows_per_sec).round() as usize).max(fft * 2);
    let mut block_len = block_for(opts.fft_size);
    // Spectrum is optional, unlike the sample stream consumed by the VFOs.
    // A bounded 1-slot queue ensures that a slow FFT never holds the receiver.
    let fft_size = Arc::new(AtomicUsize::new(opts.fft_size));
    let (spectrum_tx, spectrum_rx) = mpsc::sync_channel::<Arc<Vec<Iq>>>(1);
    let spectrum_thread = {
        let shared = Arc::clone(&shared);
        let fft_size = Arc::clone(&fft_size);
        let snr = opts.carrier_snr_db;
        std::thread::Builder::new()
            .name("decsat-spectrum".into())
            .spawn(move || {
                crate::thread_priority::set_display_priority();
                run_spectrum(spectrum_rx, shared, rate, fft_size, snr)
            })
            .expect("spawning spectrum thread")
    };
    let mut acc: Vec<Iq> = Vec::with_capacity(block_len * 2);
    let mut tmp: Vec<Iq> = Vec::new();
    let mut samples = 0u64;
    // The source clock the VFOs share: samples so far, plus a second's
    // jump at each rewind of a looping file, so a decoder that lines up
    // two VFOs sees the signal break there rather than run on.
    let mut source_samples = 0u64;
    let mut load = 0.0f32;
    let mut input_dbfs: Option<f32> = None;
    let mut paused = false;
    // Real-time pacing: signal time played since `clock`.
    let mut clock = Instant::now();
    let mut played = 0.0f64;
    let mut dc_removal = opts.dc_removal;
    let mut dc = decsat_dsp::DcBlocker::new(rate);

    let set_state = |s: SourceState| shared.front.lock().unwrap().state = s;

    'outer: loop {
        // Commands, blocking only while paused.
        loop {
            let msg = if paused {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(m) => Some(m),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break 'outer,
                }
            } else {
                match rx.try_recv() {
                    Ok(m) => Some(m),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => break 'outer,
                }
            };
            match msg {
                Some(Cmd::Stop) => break 'outer,
                Some(Cmd::Pause(p)) => {
                    paused = p;
                    set_state(if p {
                        SourceState::Paused
                    } else {
                        SourceState::Running
                    });
                    clock = Instant::now();
                    played = 0.0;
                }
                Some(Cmd::DcRemoval(on)) => {
                    dc_removal = on;
                    dc.reset();
                }
                Some(Cmd::FftSize(n)) if n != fft_size.load(Ordering::Relaxed) => {
                    fft_size.store(n, Ordering::Relaxed);
                    block_len = block_for(n);
                    // The smoothed spectrum starts again at the new
                    // resolution (its length no longer matches), and rows of
                    // the old width are dropped so the GUI never mixes them.
                    shared.rows.lock().unwrap().clear();
                    shared.front.lock().unwrap().fft_size = n;
                }
                Some(Cmd::FftSize(_)) => {}
                None if paused => continue,
                None => break,
            }
        }

        // Gather a block.
        while acc.len() < block_len {
            match source.read(&mut tmp) {
                Ok(0) => {
                    if opts.loop_file && matches!(source.rewind(), Ok(true)) {
                        source_samples += rate as u64;
                        continue;
                    }
                    set_state(SourceState::Ended);
                    // Nothing more will come; wait to be stopped.
                    while let Ok(m) = rx.recv() {
                        if matches!(m, Cmd::Stop) {
                            break;
                        }
                    }
                    break 'outer;
                }
                Ok(_) => acc.extend_from_slice(&tmp),
                Err(e) => {
                    set_state(SourceState::Failed(e.to_string()));
                    while let Ok(m) = rx.recv() {
                        if matches!(m, Cmd::Stop) {
                            break;
                        }
                    }
                    break 'outer;
                }
            }
        }
        let mut block: Vec<Iq> = acc.drain(..block_len).collect();
        if dc_removal {
            dc.process(&mut block);
        }
        let block = Arc::new(block);
        samples += block_len as u64;
        let block_start_s = source_samples as f64 / rate;
        source_samples += block_len as u64;
        // The block's level, smoothed over about five blocks.
        let power = block.iter().map(|z| z.norm_sqr() as f64).sum::<f64>() / block_len as f64;
        let level = 10.0 * power.max(1e-20).log10() as f32;
        input_dbfs = Some(input_dbfs.map_or(level, |v| v + 0.2 * (level - v)));

        // Pace files to real time so the waterfall scrolls at the true rate.
        if opts.realtime && !source.is_live() {
            played += block_len as f64 / rate;
            let ahead = played - clock.elapsed().as_secs_f64();
            if ahead > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(ahead));
            }
        }
        let t0 = Instant::now();

        // VFO fan-out first. The FFT branch may discard a display frame,
        // but must never delay the source or demodulator.
        for h in shared.vfos.lock().unwrap().values() {
            h.offer(&block, block_start_s);
        }
        let _ = spectrum_tx.try_send(Arc::clone(&block));

        let used = t0.elapsed().as_secs_f64() / (block_len as f64 / rate);
        load = 0.9 * load + 0.1 * used as f32;

        let mut f = shared.front.lock().unwrap();
        f.samples = samples;
        f.load = load;
        f.input_dbfs = input_dbfs.unwrap_or(-200.0);
        if f.state == SourceState::Paused {
            f.state = SourceState::Running;
        }
    }
    // Flush the optional display queue and join after acquisition ends.
    drop(spectrum_tx);
    let _ = spectrum_thread.join();
}

/// Non-critical display worker. Slow rendering skips whole blocks, never
/// creates a gap in the independent VFO IQ stream.
fn run_spectrum(
    rx: Receiver<Arc<Vec<Iq>>>,
    shared: Arc<Shared>,
    rate: f64,
    fft_size: Arc<AtomicUsize>,
    carrier_snr_db: f32,
) {
    let mut spec = Spectrum::new(fft_size.load(Ordering::Relaxed));
    let mut fine: Option<Spectrum> = None;
    let mut avg_lin: Vec<f32> = Vec::new();
    let mut row_seq = 0u64;
    let mut last_carriers = Instant::now() - Duration::from_secs(1);
    let mut bursts = crate::bursty::BurstMeter::default();
    let mut last_hop_secs = 0.0;
    let mut last_bursts = Instant::now();
    let mut takes: VecDeque<Vec<crate::bursty::BurstBand>> = VecDeque::new();
    while let Ok(block) = rx.recv() {
        let want = fft_size.load(Ordering::Relaxed);
        if want != spec.size() {
            spec = Spectrum::new(want);
            fine = None;
            avg_lin.clear();
            takes.clear();
        }
        let spacing = last_hop_secs;
        let (row, hop) =
            spec.compute_max_with(&block, SEGMENTS_PER_ROW, |p| bursts.push(p, rate, spacing));
        last_hop_secs = hop as f64 / rate;
        row_seq += 1;
        if avg_lin.len() != row.len() {
            avg_lin = row.iter().map(|&d| 10f32.powf(d / 10.0)).collect();
        } else {
            for (a, &d) in avg_lin.iter_mut().zip(&row) {
                *a += 0.15 * (10f32.powf(d / 10.0) - *a);
            }
        }
        let avg_db: Vec<f32> = avg_lin
            .iter()
            .map(|&p| 10.0 * p.max(1e-20).log10())
            .collect();
        let carriers = (last_carriers.elapsed() >= Duration::from_millis(500)).then(|| {
            last_carriers = Instant::now();
            let mut found = detect_carriers(&avg_db, rate, carrier_snr_db);
            if found.iter().any(|c| c.narrow) {
                let cap = 1usize << (block.len() / 4).max(1).ilog2();
                let want = (spec.size() * 16).min(cap);
                if want > spec.size() {
                    let f = fine.get_or_insert_with(|| Spectrum::new(want));
                    if f.size() != want {
                        *f = Spectrum::new(want);
                    }
                    resolve_narrow(&mut found, &f.compute_max(&block, 64), rate);
                }
            }
            found
        });
        let burst_bands = (last_bursts.elapsed() >= Duration::from_secs(2)).then(|| {
            last_bursts = Instant::now();
            takes.push_back(bursts.take());
            if takes.len() > BURST_TAKES {
                takes.pop_front();
            }
            crate::bursty::union(takes.make_contiguous())
        });
        {
            let mut rows = shared.rows.lock().unwrap();
            // This worker is the only producer: a row of another width than
            // those kept (the FFT size changed while it computed) starts the
            // backlog again, so the GUI never mixes widths.
            if rows.back().is_some_and(|(_, r)| r.len() != row.len()) {
                rows.clear();
            }
            if rows.len() == ROW_BACKLOG {
                rows.pop_front();
            }
            rows.push_back((row_seq, row));
        }
        let mut f = shared.front.lock().unwrap();
        f.spectrum_db = avg_db;
        if let Some(c) = carriers {
            f.carriers = c;
        }
        if let Some(b) = burst_bands {
            f.burst_bands = b;
            f.burst_takes += 1;
        }
        f.row_seq = row_seq;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identify::Verdict;
    use crate::vfo::DecoderKind;
    use decsat_core::SampleFormat;
    use decsat_io::{IqFileReader, IqFileWriter};
    use decsat_mod::{FrameSpec, PlFramer, Shaper};

    /// Two DVB-S2 carriers in an 8 MS/s span: 1 MS/s at −1.5 MHz and
    /// 500 kS/s at +2 MHz.
    fn two_carrier_file(path: &std::path::Path) {
        let rate = 8e6;
        let make = |modcod: u8, sps: usize, seed: u64, n_sym: usize| {
            let s = PlFramer::new(0, seed)
                .build_schedule(&[FrameSpec::new(modcod, false, true)], n_sym);
            let mut sh = Shaper::new(sps, 0.25, 16);
            let mut x = Vec::new();
            sh.process(&s, &mut x);
            x
        };
        let a = make(4, 8, 1, 200_000);
        let b = make(12, 16, 2, 100_000);
        let n = a.len().min(b.len());
        let mut noise = 0x9E37_79B9u64;
        let x: Vec<Iq> = (0..n)
            .map(|k| {
                let t = k as f64 / rate;
                let pa = std::f64::consts::TAU * -1.5e6 * t;
                let pb = std::f64::consts::TAU * 2.0e6 * t;
                noise ^= noise >> 12;
                noise ^= noise << 25;
                noise ^= noise >> 27;
                let v = noise.wrapping_mul(0x2545_F491_4F6C_DD1D);
                let nz = Iq::new(
                    ((v >> 40) as f32 / (1u32 << 24) as f32) - 0.5,
                    (((v >> 16) & 0xFF_FFFF) as f32 / (1u32 << 24) as f32) - 0.5,
                ) * 0.05;
                a[k] * Iq::new(pa.cos() as f32, pa.sin() as f32) * 0.3
                    + b[k] * Iq::new(pb.cos() as f32, pb.sin() as f32) * 0.3
                    + nz
            })
            .collect();
        let mut w = IqFileWriter::create(path, SampleFormat::Cf32).unwrap();
        w.write(&x).unwrap();
        w.finish().unwrap();
    }

    fn wait_for<T>(timeout_s: f64, mut f: impl FnMut() -> Option<T>) -> Option<T> {
        let t0 = Instant::now();
        while t0.elapsed().as_secs_f64() < timeout_s {
            if let Some(v) = f() {
                return Some(v);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    #[test]
    fn finds_carriers_and_identifies_one_through_a_vfo() {
        let path = std::env::temp_dir().join("decsat-engine-two-carriers.cf32");
        two_carrier_file(&path);
        let src = IqFileReader::open(&path, SampleFormat::Cf32, 8e6, 1 << 16).unwrap();
        let mut eng = Engine::start(
            Box::new(src),
            EngineOptions {
                realtime: false,
                loop_file: true,
                ..Default::default()
            },
        );

        // Both carriers detected.
        let carriers = wait_for(30.0, || {
            let c = eng.front().carriers;
            (c.len() >= 2).then_some(c)
        })
        .expect("carriers were not detected");
        let b = *carriers
            .iter()
            .find(|c| (c.center_hz - 2.0e6).abs() < 50e3)
            .unwrap_or_else(|| panic!("no carrier near +2 MHz: {carriers:#?}"));
        assert!((b.symbol_rate_hz - 500e3).abs() / 500e3 < 0.06, "{b:?}");

        // Claim it with Identify.
        let id = eng.add_vfo(VfoSettings::new(
            "B",
            b.center_hz,
            b.suggested_vfo_bandwidth(),
            DecoderKind::Identify,
        ));
        let ident = wait_for(60.0, || eng.vfo_status(id).and_then(|s| s.identification))
            .expect("no identification");
        match &ident.verdict {
            Verdict::DvbS2(d) => assert!(d.modcods.contains_key(&12), "{d:?}"),
            v => panic!("expected DVB-S2, got {v:?} — {}", ident.summary()),
        }
        let rs = ident.symbol_rate.unwrap();
        assert!((rs - 500e3).abs() / 500e3 < 0.01, "Rs {rs}");

        drop(eng);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fft_size_changes_without_a_restart() {
        // A tone at +250 kHz in a 1 MS/s file, looping.
        let path = std::env::temp_dir().join("decsat-engine-fft-size.cf32");
        let x: Vec<Iq> = (0..200_000)
            .map(|k| {
                let p = std::f64::consts::TAU * 0.25 * k as f64;
                Iq::new(p.cos() as f32, p.sin() as f32)
            })
            .collect();
        let mut w = IqFileWriter::create(&path, SampleFormat::Cf32).unwrap();
        w.write(&x).unwrap();
        w.finish().unwrap();
        let src = IqFileReader::open(&path, SampleFormat::Cf32, 1e6, 1 << 16).unwrap();
        let eng = Engine::start(
            Box::new(src),
            EngineOptions {
                realtime: false,
                ..Default::default()
            },
        );
        let width = |eng: &Engine| eng.new_rows(0).last().map(|(_, r)| r.len());
        assert_eq!(
            wait_for(10.0, || width(&eng).filter(|&w| w == 4096)),
            Some(4096)
        );
        let samples = eng.front().samples;

        eng.set_fft_size(1024);
        eng.set_fft_size(1000); // not a power of two: ignored
        assert_eq!(
            wait_for(10.0, || width(&eng).filter(|&w| w == 1024)),
            Some(1024)
        );
        let f = eng.front();
        assert_eq!(f.fft_size, 1024);
        // The same engine kept counting: a restart would begin again at zero.
        assert!(f.samples > samples);
        // Every row still kept is of the new width.
        assert!(eng.new_rows(0).iter().all(|(_, r)| r.len() == 1024));
        // The tone sits at +250 kHz: bin 3/4 of the way across.
        // `new_rows` returns an owned Vec; take the last row out of it.
        let row = eng.new_rows(0).pop().unwrap().1;
        let peak = (0..row.len())
            .max_by(|&a, &b| row[a].total_cmp(&row[b]))
            .unwrap();
        assert!((peak as i64 - 768).abs() <= 1, "peak at bin {peak}");

        drop(eng);
        let _ = std::fs::remove_file(&path);
    }

    /// A hub's outbound and its return pool in one capture: the burst
    /// plan lined up in frequency and time, followed every five seconds of
    /// signal, and each terminal's level at the end. Run by hand with the
    /// 172B capture (8 MS/s cs8; outbound −881.7 kHz, pool +580 kHz):
    /// `DECSAT_172B=<file> cargo test -p decsat-engine --release
    /// plan_on_a_capture -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn plan_on_a_capture() {
        use crate::tdma::pool::{align_plan, lead_scores, match_plan, terminal_levels};
        let Ok(path) = std::env::var("DECSAT_172B") else {
            return;
        };
        let src = IqFileReader::open(path.as_ref(), SampleFormat::Cs8, 8e6, 1 << 16).unwrap();
        let mut eng = Engine::start(
            Box::new(src),
            EngineOptions {
                realtime: false,
                loop_file: false,
                ..Default::default()
            },
        );
        let out = eng.add_vfo(VfoSettings::new(
            "VFO 1",
            -881_700.0,
            1_300_000.0,
            DecoderKind::Dvbs2Ts,
        ));
        let pool = eng.add_vfo(VfoSettings::new(
            "VFO 2",
            580_000.0,
            900_000.0,
            DecoderKind::ReturnPool,
        ));
        let mut prior = None;
        let mut shown = 0.0;
        let look = |eng: &Engine, prior: &mut Option<_>, last: bool| {
            let (Some(o), Some(p)) = (eng.vfo_status(out), eng.vfo_status(pool)) else {
                return;
            };
            let Some(p) = p.pool else { return };
            if let Some(b) = &p.bursts {
                println!(
                    "{:.1} s: bursts from VFO {:?}: zero {:+.0} Hz, lead {:?} ms; planned {} cut {} \
                     demodulated {} failed {} late {} dropped {} unknown {}; {:.0}/s, load {:.2} \
                     ({} workers)",
                    p.now_s,
                    b.from,
                    b.offset_hz,
                    b.timing.map(|t| t.lead_s * 1e3),
                    b.planned,
                    b.cut,
                    b.demodulated,
                    b.failed,
                    b.late,
                    b.dropped,
                    b.unknown,
                    b.per_s,
                    b.load,
                    b.workers
                );
                if last {
                    for t in &b.terminals {
                        println!(
                            "  terminal {:3}: {} of {} cut, MER {:.1} dB, {:+.0} Hz, {:+.1} µs, \
                             fit {:.2}, code {:04X}h {:?} {:.0} Bd",
                            t.id,
                            t.demodulated,
                            t.cut,
                            t.mer_db,
                            t.freq_hz,
                            t.timing_us,
                            t.fit,
                            t.code,
                            t.waveform,
                            t.symbol_rate
                        );
                    }
                }
            }
            let Some(plan) = o
                .fec
                .and_then(|f| f.ts)
                .and_then(|t| t.rcs)
                .and_then(|r| r.plan)
            else {
                println!("{:.1} s: no plan yet", p.now_s);
                return;
            };
            let m = match_plan(&p, &plan);
            println!(
                "{:.1} s: plan {} frames, clock {:?}; pool slot {:?} frame {:?} grid {:?}, \
                 {} slot rows; frequency {:?}",
                p.now_s,
                plan.frames,
                plan.clock,
                p.slot_s,
                p.frame_s,
                p.grid,
                p.slot_rows.len(),
                m
            );
            let Some(m) = m else { return };
            let t = align_plan(&p, &plan, m.offset_hz, prior.as_ref());
            println!(
                "  timing {t:?} ({:.2} frames)",
                t.map_or(0.0, |t| t.lead_frames())
            );
            if let Some(t) = &t {
                println!(
                    "  zero refined: {:?}",
                    crate::tdma::pool::refine_offset(&p, &plan, t, m.offset_hz)
                );
            }
            if last && let Some(s) = lead_scores(&p, &plan, m.offset_hz) {
                for (lead, r, n) in &s.tried {
                    println!(
                        "    lead {:8.2} ms ({:6.3} frames): {r:+.3} over {n}",
                        lead * 1e3,
                        lead / s.frame_s
                    );
                }
            }
            if let Some(t) = t {
                *prior = Some(t);
                if last {
                    for l in terminal_levels(&p, &plan, &t, m.offset_hz) {
                        println!("  {l:?}");
                    }
                }
            }
        };
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let ended = eng.front().state == SourceState::Ended;
            let now = eng
                .vfo_status(pool)
                .and_then(|s| s.pool)
                .map_or(0.0, |p| p.now_s);
            if now - shown >= 5.0 {
                shown = now;
                look(&eng, &mut prior, false);
            }
            if ended && eng.settled() {
                break;
            }
        }
        look(&eng, &mut prior, true);
    }
}
