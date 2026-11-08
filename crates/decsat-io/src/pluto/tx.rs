// made by CasualArclamp
//! Transmitting through an ADALM-PLUTO / Pluto+, over the same IIOD links as
//! the receiver (USB pipes or TCP 30431), in pure Rust.
//!
//! **Only when asked, and gently** (as `hackrf_tx.rs`). Nothing here runs
//! unless the caller starts a [`PlutoTx`], which the transmitter apps do only
//! behind their arm. The attenuation starts at the caller's setting (the apps
//! default to the most, 89.75 dB); the TX LO is powered up only after the
//! first block is in the DAC's buffer, and powered down again, with the
//! attenuation back at its most, when [`PlutoTx`] is dropped.
//!
//! The IIO side, from libiio v0.25 (`iiod-client.c`, `iiod/ops.c`; LGPL-2.1,
//! Analog Devices) and ADI's `ad9361-iiostream.c` example:
//!
//! * samples go to `cf-ad9361-dds-core-lpc`, `voltage0` (I) and `voltage1`
//!   (Q) of TX1, 16-bit little-endian with the 12-bit DAC word in the top
//!   bits (`<< 4`): an 8-bit cs8 sample is shifted up by 8 (full scale);
//! * opening that buffer switches the DAC from the DDS tones to the stream;
//! * each `WRITEBUF` is one kernel buffer; iiod pushes it and waits for a
//!   free one, so the radio's clock paces the writer (and the modulator);
//! * rates below 2.083 MS/s use the FPGA's ×8 interpolator, the mirror of the
//!   receiver's decimator: the phy runs at 8× and the DDS core's
//!   `sampling_frequency` asks for the rate;
//! * the AD9361 has one sample clock for RX and TX: a receiver on the same
//!   Pluto (full duplex, e.g. QO-100) must use the same rate.
//!
//! **The link.** A plain Pluto's USB 2 carries about 7 MS/s of 16-bit IQ; a
//! rate above what the link sustains starves the DAC (it then sends zeros).
//! iiod does not report that, so, as on receive, the shortfall of what was
//! written against the time gone is counted ([`PlutoTxCounters::link_short`]).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use decsat_core::Result;

use super::iiod::{self, Client, Context};
use super::{Connector, PHY, PlutoUri, block_samples, check_freq, connector, err, rate_plan};

const DDS: &str = "cf-ad9361-dds-core-lpc";
/// The AD9361's transmit attenuation range, dB (0.25 dB steps).
pub const ATTEN_MAX_DB: f64 = 89.75;
/// Kernel buffers queued ahead of the DAC: about 4 × 1/30 s.
const TX_KERNEL_BUFFERS: usize = 4;

/// What to transmit with.
#[derive(Debug, Clone, PartialEq)]
pub struct PlutoTxSettings {
    pub uri: PlutoUri,
    pub center_hz: f64,
    /// Complex samples per second (0.26–61.44 MS/s).
    pub sample_rate: f64,
    /// The TX analogue filter, Hz; `None` follows the rate.
    pub bandwidth: Option<f64>,
    /// Attenuation, 0 (full power, about +7 dBm) to 89.75 dB.
    pub attenuation_db: f64,
}

impl Default for PlutoTxSettings {
    fn default() -> Self {
        PlutoTxSettings {
            uri: PlutoUri::Auto,
            center_hz: 546e6,
            sample_rate: 64e6 / 7.0,
            bandwidth: None,
            attenuation_db: ATTEN_MAX_DB,
        }
    }
}

/// How the transmitter is doing.
#[derive(Debug, Default)]
pub struct PlutoTxCounters {
    /// cs8 bytes handed to the radio (two a sample), zeros included.
    pub bytes: AtomicU64,
    /// Blocks' worth of samples the link was late with: the DAC sent zeros
    /// for them. Zero when the link keeps up.
    pub link_short: AtomicU64,
    /// The thread has stopped (and why, in `error`).
    pub stopped: AtomicBool,
    pub error: Mutex<Option<String>>,
}

/// The IIO names a transmit session needs.
#[derive(Debug, Clone)]
struct TxIds {
    phy: String,
    dds: String,
    words: usize,
    i_bit: u32,
    q_bit: u32,
    has_interpolator: bool,
}

impl TxIds {
    fn from(ctx: &Context) -> Result<TxIds> {
        let phy = ctx
            .device(PHY)
            .ok_or_else(|| err("no ad9361-phy: is this a Pluto?"))?;
        let dds = ctx
            .device(DDS)
            .ok_or_else(|| err("no cf-ad9361-dds-core-lpc: this firmware cannot transmit"))?;
        if phy.channel("altvoltage1", true).is_none() {
            return Err(err("the ad9361-phy has no TX LO"));
        }
        let bit = |id: &str| {
            dds.channel(id, true)
                .and_then(|c| c.scan_index)
                .ok_or_else(|| err(format!("{DDS} has no {id} scan element")))
        };
        Ok(TxIds {
            phy: phy.id.clone(),
            dds: dds.id.clone(),
            words: dds.mask_words(),
            i_bit: bit("voltage0")?,
            q_bit: bit("voltage1")?,
            has_interpolator: dds
                .channel("voltage0", true)
                .is_some_and(|c| c.attrs.iter().any(|a| a == "sampling_frequency")),
        })
    }

    fn mask(&self) -> Vec<u32> {
        let mut m = vec![0u32; self.words];
        for b in [self.i_bit, self.q_bit] {
            m[(b / 32) as usize] |= 1 << (b % 32);
        }
        m
    }
}

const TX: Option<(&str, bool)> = Some(("voltage0", true));
const TX_LO: Option<(&str, bool)> = Some(("altvoltage1", true));

fn atten_value(db: f64) -> String {
    // `hardwaregain` is the negative attenuation, in 0.25 dB steps.
    let q = (db.clamp(0.0, ATTEN_MAX_DB) * 4.0).round() / 4.0;
    format!("{:.2}", -q)
}

/// Program the transmitter with its LO still off.
fn configure(c: &mut Client, ids: &TxIds, s: &PlutoTxSettings) -> Result<()> {
    let (phy_rate, interpolated) = rate_plan(s.sample_rate)?;
    c.write_attr(&ids.phy, TX_LO, "powerdown", "1")?;
    c.write_attr(&ids.phy, TX, "hardwaregain", &atten_value(ATTEN_MAX_DB))?;
    c.write_attr(&ids.phy, TX, "sampling_frequency", &phy_rate.to_string())?;
    if ids.has_interpolator {
        let r = interpolated.unwrap_or(phy_rate);
        c.write_attr(&ids.dds, TX, "sampling_frequency", &r.to_string())?;
    } else if interpolated.is_some() {
        return Err(err(
            "this firmware has no ×8 interpolator: use 2.084 MS/s or more",
        ));
    }
    let bw = s
        .bandwidth
        .unwrap_or(s.sample_rate)
        .clamp(200e3, 40e6)
        .round() as u64;
    c.write_attr(&ids.phy, TX, "rf_bandwidth", &bw.to_string())?;
    c.write_attr(
        &ids.phy,
        TX_LO,
        "frequency",
        &(s.center_hz.round() as u64).to_string(),
    )?;
    c.write_attr(&ids.phy, TX, "hardwaregain", &atten_value(s.attenuation_db))
}

/// cs8 (I8, Q8) to the DAC's 16-bit words, 12 bits in the top.
fn to_dac(cs8: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.extend(
        cs8.iter()
            .flat_map(|&b| (((b as i8) as i16) << 8).to_le_bytes()),
    );
}

/// A running Pluto transmission: dropping it powers the TX LO down.
pub struct PlutoTx {
    ctl: Arc<Mutex<Client>>,
    ids: Arc<TxIds>,
    settings: Mutex<PlutoTxSettings>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub counters: Arc<PlutoTxCounters>,
    link: String,
}

impl PlutoTx {
    /// Connect, configure and transmit what `source` gives: it appends 8-bit
    /// I/Q pairs (cs8) to the buffer each time it is called; when it gives
    /// nothing, a block of zeros goes out instead.
    pub fn start(
        settings: PlutoTxSettings,
        source: impl FnMut(&mut Vec<u8>) + Send + 'static,
    ) -> Result<Self> {
        check_freq(settings.center_hz)?;
        rate_plan(settings.sample_rate)?;
        let conn = connector(&settings.uri)?;
        Self::start_on(conn, settings, source)
    }

    pub(crate) fn start_on(
        conn: Arc<dyn Connector>,
        settings: PlutoTxSettings,
        mut source: impl FnMut(&mut Vec<u8>) + Send + 'static,
    ) -> Result<Self> {
        let mut ctl = Client::new(conn.link(0)?);
        let ctx = iiod::parse_context(&ctl.print()?)?;
        let ids = TxIds::from(&ctx)?;
        configure(&mut ctl, &ids, &settings)?;
        let link = conn.describe();
        tracing::info!(
            "Pluto TX on {link}: {} {}",
            ctx.attr("hw_model").unwrap_or_default(),
            ctx.attr("fw_version").unwrap_or_default()
        );

        let mut data = Client::new(conn.link(1)?);
        let samples = block_samples(settings.sample_rate);
        let _ = data.exec("TIMEOUT 5000\r\n");
        if let Err(e) = data.exec(&format!(
            "SET {} BUFFERS_COUNT {TX_KERNEL_BUFFERS}\r\n",
            ids.dds
        )) {
            tracing::debug!("{e}");
        }
        data.open(&ids.dds, samples, &ids.mask())?;

        let ctl = Arc::new(Mutex::new(ctl));
        let ids = Arc::new(ids);
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(PlutoTxCounters::default());
        let rate = settings.sample_rate;
        let thread = {
            let (stop, counters) = (stop.clone(), counters.clone());
            let (ctl, ids) = (ctl.clone(), ids.clone());
            std::thread::Builder::new()
                .name("pluto-tx".into())
                .spawn(move || {
                    let r = writer(
                        &mut data,
                        &ctl,
                        &ids,
                        samples,
                        rate,
                        &mut source,
                        &stop,
                        &counters,
                    );
                    if let Err(e) = r {
                        let _ = ctl
                            .lock()
                            .unwrap()
                            .write_attr(&ids.phy, TX_LO, "powerdown", "1");
                        *counters.error.lock().unwrap() = Some(e.to_string());
                    } else {
                        // All its replies are in: the buffer can be closed.
                        let _ = data.close(&ids.dds);
                    }
                    counters.stopped.store(true, Ordering::Release);
                })
                .map_err(|e| err(format!("cannot start the transmit thread: {e}")))?
        };
        Ok(PlutoTx {
            ctl,
            ids,
            settings: Mutex::new(settings),
            stop,
            thread: Some(thread),
            counters,
            link,
        })
    }

    pub fn settings(&self) -> PlutoTxSettings {
        self.settings.lock().unwrap().clone()
    }

    /// "usb" or "ip:…".
    pub fn link(&self) -> &str {
        &self.link
    }

    /// Change the attenuation while transmitting (0–89.75 dB).
    pub fn set_attenuation(&self, db: f64) -> Result<()> {
        self.ctl
            .lock()
            .unwrap()
            .write_attr(&self.ids.phy, TX, "hardwaregain", &atten_value(db))?;
        self.settings.lock().unwrap().attenuation_db = db.clamp(0.0, ATTEN_MAX_DB);
        Ok(())
    }

    /// Retune while transmitting.
    pub fn set_center(&self, hz: f64) -> Result<()> {
        check_freq(hz)?;
        self.ctl.lock().unwrap().write_attr(
            &self.ids.phy,
            TX_LO,
            "frequency",
            &(hz.round() as u64).to_string(),
        )?;
        self.settings.lock().unwrap().center_hz = hz;
        Ok(())
    }
}

/// The transmit thread: fill a block from the source (zeros if it has
/// nothing), send it, and once a second check the LO is still up.
#[allow(clippy::too_many_arguments)]
fn writer(
    data: &mut Client,
    ctl: &Mutex<Client>,
    ids: &TxIds,
    samples: usize,
    rate: f64,
    source: &mut dyn FnMut(&mut Vec<u8>),
    stop: &AtomicBool,
    counters: &PlutoTxCounters,
) -> Result<()> {
    let block = 2 * samples; // cs8 bytes
    let mut pending: Vec<u8> = Vec::with_capacity(2 * block);
    let mut raw = Vec::with_capacity(2 * block);
    let mut sent: u64 = 0;
    let mut t0: Option<Instant> = None;
    let mut last_check = Instant::now();
    let mut warned = false;
    while !stop.load(Ordering::Relaxed) {
        while pending.len() < block {
            let before = pending.len();
            source(&mut pending);
            if pending.len() == before {
                pending.resize(before + block, 0);
            }
        }
        to_dac(&pending[..block], &mut raw);
        data.writebuf(&ids.dds, &raw)?;
        pending.drain(..block);
        sent += 1;
        counters.bytes.fetch_add(block as u64, Ordering::Relaxed);
        match t0 {
            None => {
                // The first block is in the DAC's buffer: only now the LO.
                // Under the lock, after a last look at `stop`, so a drop
                // that has already powered it down is never undone.
                let mut c = ctl.lock().unwrap();
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                c.write_attr(&ids.phy, TX_LO, "powerdown", "0")?;
                t0 = Some(Instant::now());
            }
            Some(t) => {
                // The kernel's buffers filled first, so `sent` runs ahead of
                // the clock by that many blocks while the link keeps up.
                let due = t.elapsed().as_secs_f64() * rate / samples as f64;
                let ahead = (sent - 1) as f64 + TX_KERNEL_BUFFERS as f64;
                let short = (due - ahead - 1.0).max(0.0) as u64;
                counters.link_short.fetch_max(short, Ordering::Relaxed);
                if short > 0 && !warned {
                    warned = true;
                    tracing::warn!(
                        "Pluto TX: the link cannot carry {:.2} MS/s of 16-bit IQ; the DAC is \
                         sending gaps. Use a lower rate or a Pluto+ on Ethernet",
                        rate / 1e6
                    );
                }
            }
        }
        if last_check.elapsed() > Duration::from_secs(1) {
            last_check = Instant::now();
            // Another program opening the receiver on this Pluto powers the
            // TX LO down (its receive-only open does that): this session is
            // still armed and transmitting, so it comes back up.
            let mut c = ctl.lock().unwrap();
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if c.read_attr(&ids.phy, TX_LO, "powerdown")?.trim() == "1" {
                tracing::warn!("Pluto TX: the TX LO was powered down by another client; back up");
                c.write_attr(&ids.phy, TX_LO, "powerdown", "0")?;
            }
        }
    }
    Ok(())
}

impl Drop for PlutoTx {
    fn drop(&mut self) {
        // The LO first, under the lock the writer's checks take: no RF from
        // here on, whatever the thread is doing.
        {
            let mut c = self.ctl.lock().unwrap();
            self.stop.store(true, Ordering::Relaxed);
            let _ = c.write_attr(&self.ids.phy, TX_LO, "powerdown", "1");
            let _ = c.write_attr(
                &self.ids.phy,
                TX,
                "hardwaregain",
                &atten_value(ATTEN_MAX_DB),
            );
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
