// made by CasualArclamp
//! A VFO's DVB-CID decoder (ETSI TS 103 129): its own thread, fed the VFO's
//! baseband. The host carrier's centre (from Identify) is mixed to zero and
//! the CID's band kept, then the signal is resampled to eight samples per
//! chip for `decsat_modem::cid::CidRx`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use decsat_core::Iq;
use decsat_dsp::Ddc;
use decsat_modem::cid::{CidRx, SPS, chip_rate};
// What a CID VFO's view holds, and how to show it, for the apps.
pub use decsat_modem::cid::{
    CHIPS, CidLive, CidReport, CidSearch, CidStats, FRAME_BITS, LIVE_BITS, REPEAT, ScramblerOrder,
    guid_mac, guid_text,
};

/// A CID VFO's state, for display.
#[derive(Debug, Clone, Default)]
pub struct CidView {
    /// The host carrier's symbol rate (as Identify measured it) and the
    /// chip rate it implies.
    pub host_symbol_rate: f64,
    pub chip_rate: f64,
    /// Where the host carrier's centre was taken to be, from the VFO's
    /// centre (Identify's estimate, good to some tens of Hz); the CID's
    /// offset is from there.
    pub center_hz: f64,
    /// The VFO is wide enough for the CID's whole band (1.35 × chip rate).
    pub wide_enough: bool,
    pub stats: CidStats,
    /// Low-SNR mode is on (deep searches to 384 bits, looser threshold).
    pub low_snr: bool,
    /// The clock correction in use, ppm.
    pub clock_ppm: f64,
    /// Blocks dropped because the thread was behind.
    pub dropped: u64,
}

/// Blocks queued before dropping.
const QUEUE: usize = 64;

/// What the CID thread is sent: baseband, or how many samples went missing
/// before the next block (dropped here or upstream).
enum Msg {
    Block(Vec<Iq>),
    Gap(u64),
}

pub(crate) struct CidWorker {
    tx: Option<SyncSender<Msg>>,
    view: Arc<Mutex<CidView>>,
    join: Option<JoinHandle<()>>,
    lossless: bool,
    /// Samples lost and not yet reported to the thread. `AtomicU64`: a
    /// counter changed through a shared reference (`offer` takes `&self`).
    gap: AtomicU64,
    /// Low-SNR mode, read by the thread before each block (`Arc`: owned by
    /// both this handle and the thread).
    low_snr: Arc<AtomicBool>,
    /// The clock correction, ppm, as an f64's bits (atomics hold integers).
    clock_ppm: Arc<AtomicU64>,
    /// Set to stop without working through the queue.
    quit: Arc<AtomicBool>,
    /// Blocks queued and not yet despread (the VFO's count).
    pending: crate::fec::Pending,
}

impl CidWorker {
    /// A decoder for a VFO at `in_rate`, `vfo_bandwidth` wide, on a host
    /// carrier of `symbol_rate` centred `center_hz` from the VFO's centre,
    /// searching ±(220 Hz + `span_hz`) about it.
    pub fn spawn(
        in_rate: f64,
        vfo_bandwidth: f64,
        center_hz: f64,
        span_hz: f64,
        symbol_rate: f64,
        lossless: bool,
        pending: crate::fec::Pending,
    ) -> Self {
        let rc = chip_rate(symbol_rate);
        let view = Arc::new(Mutex::new(CidView {
            host_symbol_rate: symbol_rate,
            chip_rate: rc,
            center_hz,
            wide_enough: vfo_bandwidth >= 1.35 * rc,
            ..Default::default()
        }));
        let (tx, rx) = mpsc::sync_channel::<Msg>(QUEUE);
        let low_snr = Arc::new(AtomicBool::new(false));
        let low = Arc::clone(&low_snr);
        let clock_ppm = Arc::new(AtomicU64::new(0f64.to_bits()));
        let clock = Arc::clone(&clock_ppm);
        let quit = Arc::new(AtomicBool::new(false));
        let q = Arc::clone(&quit);
        let v = view.clone();
        let p = pending.clone();
        let join = std::thread::Builder::new()
            .name("decsat-cid".into())
            .spawn(move || {
                // The CID's band (1.35 × chip rate), a little more, no more
                // than the VFO passes.
                let bw = (1.5 * rc).min(0.95 * in_rate);
                let mut ddc = Ddc::new(in_rate, center_hz, bw);
                // Chips at SPS a sample — at the chip rate as our clock sees
                // it, the correction applied.
                let rate_for = |ppm: f64| SPS as f64 * rc * (1.0 + ppm * 1e-6);
                let mut ppm_now = 0.0;
                let mut rs = Resampler::new(ddc.out_rate(), rate_for(ppm_now));
                let mut cid = CidRx::new(rc).with_span(span_hz);
                let (mut mixed, mut at4, mut frames) = (Vec::new(), Vec::new(), Vec::new());
                let mut to_chips = rate_for(ppm_now) / in_rate;
                while let Ok(msg) = rx.recv() {
                    let _done = p.guard();
                    if q.load(Ordering::Relaxed) {
                        break;
                    }
                    let block = match msg {
                        Msg::Block(b) => b,
                        Msg::Gap(n) => {
                            frames.clear();
                            cid.gap((n as f64 * to_chips).round() as usize, &mut frames);
                            continue;
                        }
                    };
                    let ppm = f64::from_bits(clock.load(Ordering::Relaxed));
                    if ppm != ppm_now {
                        ppm_now = ppm;
                        rs.retune(ddc.out_rate(), rate_for(ppm));
                        to_chips = rate_for(ppm) / in_rate;
                    }
                    mixed.clear();
                    ddc.process(&block, &mut mixed);
                    at4.clear();
                    rs.process(&mixed, &mut at4);
                    frames.clear();
                    cid.set_low_snr(low.load(Ordering::Relaxed));
                    cid.push(&at4, &mut frames);
                    let mut view = v.lock().unwrap();
                    view.stats = cid.stats.clone();
                    view.low_snr = cid.low_snr();
                    view.clock_ppm = ppm_now;
                }
            })
            .expect("spawn CID thread");
        CidWorker {
            tx: Some(tx),
            view,
            join: Some(join),
            lossless,
            gap: AtomicU64::new(0),
            low_snr,
            clock_ppm,
            quit,
            pending,
        }
    }

    /// The clock correction, ppm (taken up with the next block).
    /// Have the thread stop after the block in hand, leaving the rest.
    pub fn quit(&self) {
        self.quit.store(true, Ordering::Relaxed);
    }

    pub fn set_clock_ppm(&self, ppm: f64) {
        self.clock_ppm.store(ppm.to_bits(), Ordering::Relaxed);
    }

    /// Low-SNR mode on or off (taken up with the next block).
    pub fn set_low_snr(&self, on: bool) {
        self.low_snr.store(on, Ordering::Relaxed);
    }

    /// Queue a block of the VFO's baseband; drop it (counted, and its
    /// length reported as a gap) if the thread is behind, unless lossless.
    pub fn offer(&self, block: Vec<Iq>) {
        let Some(tx) = &self.tx else { return };
        if self.lossless {
            self.pending.add();
            if tx.send(Msg::Block(block)).is_err() {
                self.pending.done();
            }
            return;
        }
        let missed = self.gap.swap(0, Ordering::Relaxed);
        if missed > 0 {
            self.pending.add();
            if tx.try_send(Msg::Gap(missed)).is_err() {
                self.pending.done();
                self.gap
                    .fetch_add(missed + block.len() as u64, Ordering::Relaxed);
                self.view.lock().unwrap().dropped += 1;
                return;
            }
        }
        let n = block.len() as u64;
        self.pending.add();
        if let Err(e) = tx.try_send(Msg::Block(block)) {
            self.pending.done();
            if let TrySendError::Full(_) = e {
                self.gap.fetch_add(n, Ordering::Relaxed);
                self.view.lock().unwrap().dropped += 1;
            }
        }
    }

    /// Samples of the VFO's baseband lost before they got here.
    pub fn lost(&self, samples: u64) {
        self.gap.fetch_add(samples, Ordering::Relaxed);
    }

    pub fn view(&self) -> CidView {
        self.view.lock().unwrap().clone()
    }
}

impl Drop for CidWorker {
    fn drop(&mut self) {
        // Closing the channel ends the thread's loop.
        self.tx = None;
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Cubic-interpolating resampler (the input is already band-limited).
struct Resampler {
    step: f64,
    pos: f64,
    buf: Vec<Iq>,
}

impl Resampler {
    fn new(in_rate: f64, out_rate: f64) -> Self {
        Resampler {
            step: in_rate / out_rate,
            pos: 1.0,
            buf: Vec::new(),
        }
    }

    /// A new output rate, carrying on from where it is.
    fn retune(&mut self, in_rate: f64, out_rate: f64) {
        self.step = in_rate / out_rate;
    }

    fn process(&mut self, input: &[Iq], out: &mut Vec<Iq>) {
        self.buf.extend_from_slice(input);
        while (self.pos as usize) + 2 < self.buf.len() {
            let i = self.pos as usize;
            let mu = (self.pos - i as f64) as f32;
            let b = &self.buf;
            out.push(decsat_dsp::timing::cubic(
                b[i - 1],
                b[i],
                b[i + 1],
                b[i + 2],
                mu,
            ));
            self.pos += self.step;
        }
        // Keep one sample before the next position.
        let keep_from = (self.pos as usize).saturating_sub(1);
        self.buf.drain(..keep_from);
        self.pos -= keep_from as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resamples_a_tone() {
        let (fin, fout, f) = (300_000.0, 448_000.0, 10_000.0);
        let x: Vec<Iq> = (0..30_000)
            .map(|n| {
                let p = std::f64::consts::TAU * f * n as f64 / fin;
                Iq::new(p.cos() as f32, p.sin() as f32)
            })
            .collect();
        let mut r = Resampler::new(fin, fout);
        let mut y = Vec::new();
        for c in x.chunks(777) {
            r.process(c, &mut y);
        }
        assert!(
            (y.len() as f64 - 30_000.0 * fout / fin).abs() < 5.0,
            "{}",
            y.len()
        );
        // The tone at the new rate: consecutive samples step by 2π f / fout.
        let w = std::f64::consts::TAU * f / fout;
        let err = y
            .windows(2)
            .skip(10)
            .map(|p| ((p[1] * p[0].conj()).arg() as f64 - w).abs())
            .fold(0.0, f64::max);
        assert!(err < 0.01, "{err}");
    }
}
