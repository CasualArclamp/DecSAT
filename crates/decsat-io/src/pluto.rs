// made by CasualArclamp
//! Live ADALM-PLUTO and Pluto+ (AD9363/AD9361/AD9364) front end in pure
//! Rust (the transmitter is [`tx`]): the radio's own IIOD protocol ([`iiod`]) over USB
//! ([`usb`], WinUSB) or the network (TCP 30431: the Pluto's USB Ethernet at
//! 192.168.2.1, or a Pluto+'s Gigabit Ethernet). No libiio, no DLLs.
//!
//! **8-bit IQ.** F5OEO's Tezuka firmware (Pluto and Pluto+) can send 8-bit
//! I/Q instead of 16-bit, halving the link's load: a plain Pluto's USB then
//! carries ~13 MS/s instead of ~6.8. It is switched on by enabling `voltage0`
//! alone on `cf-ad9361-lpc`; each 16-bit word then holds I (byte 0) and Q
//! (byte 1), the 12-bit samples rounded to 8 bits on the FPGA (F5OEO/maia-sdr
//! `cs12_cs8.v`; detection as SoapyPlutoPAPR does it, `fw_version` starting
//! "tezuka"). [`EightBit::Auto`] uses it whenever the firmware has it.
//!
//! **Rates.** 2.084–61.44 MS/s straight from the AD9361; 260 kS/s–2.083 MS/s
//! with the FPGA's ×8 decimator (the phy runs at 8× the rate and
//! `cf-ad9361-lpc`'s `sampling_frequency` asks for the decimated rate: the
//! sequence of libad9361 / SoapyPlutoSDR).
//!
//! **The receiver does not transmit.** It never opens the transmit data
//! device (`cf-ad9361-dds-core-lpc`), and every open powers the TX LO down
//! (a [`tx::PlutoTx`] running on the same Pluto powers it back up).
//!
//! The reader thread keeps two `READBUF` requests in flight (so the round
//! trip costs no throughput), converts each block to IQ and hands it over a
//! bounded channel, dropping whole blocks if the consumer falls behind.

pub mod iiod;
pub mod tx;
pub mod usb;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use decsat_core::{Error, Iq, Result};

use self::iiod::{Client, Context, Link};
use crate::IqSource;

const PHY: &str = "ad9361-phy";
const RX: &str = "cf-ad9361-lpc";
/// IIOD's TCP port.
pub const IIOD_PORT: u16 = 30431;
/// The Pluto's own address on its USB Ethernet.
pub const DEFAULT_IP: &str = "192.168.2.1";

/// The AD9361's tuning range; an AD9363 Pluto that was not unlocked refuses
/// outside 325–3800 MHz, and that refusal is passed on.
pub const FREQ_MIN_HZ: f64 = 70e6;
pub const FREQ_MAX_HZ: f64 = 6e9;
/// Lowest rate (with the ×8 decimator) and highest.
pub const RATE_MIN: f64 = 260_417.0;
pub const RATE_MAX: f64 = 61_440_000.0;
/// Below this the AD9361 needs its FIR, so the FPGA's ×8 decimator is used
/// instead (25 MHz / 12).
const DECIM_BELOW: f64 = 25e6 / 12.0;

/// Blocks queued for the consumer before dropping.
const QUEUE: usize = 32;

fn err(what: impl std::fmt::Display) -> Error {
    Error::Other(format!("Pluto: {what}"))
}

/// How to reach the radio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlutoUri {
    /// USB if a Pluto is enumerated there, else the network at 192.168.2.1.
    Auto,
    Usb,
    /// `host` or `host:port`.
    Ip(String),
}

impl std::str::FromStr for PlutoUri {
    type Err = String;

    /// `auto`, `usb`, `usb:`, `ip:192.168.2.1`, `192.168.2.1`, `pluto.local:30431`.
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        let s = s.trim();
        match s.to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(PlutoUri::Auto),
            u if u == "usb" || u.starts_with("usb:") => Ok(PlutoUri::Usb),
            _ => {
                let host = s.strip_prefix("ip:").unwrap_or(s).trim();
                if host.is_empty() || host.contains(char::is_whitespace) {
                    return Err(format!("not a Pluto address: {s:?}"));
                }
                Ok(PlutoUri::Ip(host.to_string()))
            }
        }
    }
}

impl std::fmt::Display for PlutoUri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlutoUri::Auto => write!(f, "auto"),
            PlutoUri::Usb => write!(f, "usb"),
            PlutoUri::Ip(h) => write!(f, "ip:{h}"),
        }
    }
}

/// Whether to stream 8-bit IQ (see the module comment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EightBit {
    /// When the firmware offers it (Tezuka).
    #[default]
    Auto,
    Off,
}

/// The receiver's gain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlutoGain {
    /// Manual, dB (the AD9361 takes about −3 to 71 dB, depending on the band).
    Manual(f64),
    /// The AD9361's slow-attack AGC.
    Agc,
}

impl Default for PlutoGain {
    fn default() -> Self {
        PlutoGain::Manual(50.0)
    }
}

/// What a live session is set to.
#[derive(Debug, Clone, PartialEq)]
pub struct PlutoSettings {
    pub uri: PlutoUri,
    pub center_hz: f64,
    /// Complex samples per second.
    pub sample_rate: f64,
    /// RF filter bandwidth, Hz; `None` follows the rate.
    pub bandwidth: Option<f64>,
    pub gain: PlutoGain,
    pub eight_bit: EightBit,
}

impl Default for PlutoSettings {
    fn default() -> Self {
        PlutoSettings {
            uri: PlutoUri::Auto,
            center_hz: 546e6,
            sample_rate: 64e6 / 7.0,
            bandwidth: None,
            gain: PlutoGain::default(),
            eight_bit: EightBit::Auto,
        }
    }
}

/// What the radio said about itself at open.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlutoInfo {
    /// "usb" or "ip:…".
    pub link: String,
    /// `fw_version`, e.g. "v0.39" or "tezuka-…".
    pub firmware: String,
    /// `hw_model`, e.g. "Analog Devices PlutoSDR Rev.C (Z7010-AD9363A)".
    pub model: String,
    /// Whether samples arrive as 8-bit IQ.
    pub eight_bit: bool,
}

/// The IIO names a session needs, resolved once from the context.
#[derive(Debug, Clone)]
struct Ids {
    phy: String,
    rx: String,
    /// Channels of the RX data device (mask length).
    rx_words: usize,
    /// Mask bits of I and Q (`voltage0`, `voltage1`).
    i_bit: u32,
    q_bit: u32,
    /// `cf-ad9361-lpc` voltage0 has `sampling_frequency` (the ×8 decimator).
    has_decimator: bool,
    /// The TX LO channel exists (powered down at open).
    has_tx_lo: bool,
}

impl Ids {
    fn from(ctx: &Context) -> Result<Ids> {
        let phy = ctx
            .device(PHY)
            .ok_or_else(|| err("no ad9361-phy: is this a Pluto?"))?;
        let rx = ctx
            .device(RX)
            .ok_or_else(|| err("no cf-ad9361-lpc: is this a Pluto?"))?;
        let bit = |id: &str| {
            rx.channel(id, false)
                .and_then(|c| c.scan_index)
                .ok_or_else(|| err(format!("cf-ad9361-lpc has no {id} scan element")))
        };
        Ok(Ids {
            phy: phy.id.clone(),
            rx: rx.id.clone(),
            rx_words: rx.mask_words(),
            i_bit: bit("voltage0")?,
            q_bit: bit("voltage1")?,
            has_decimator: rx
                .channel("voltage0", false)
                .is_some_and(|c| c.attrs.iter().any(|a| a == "sampling_frequency")),
            has_tx_lo: phy.channel("altvoltage1", true).is_some(),
        })
    }

    fn mask(&self, eight_bit: bool) -> Vec<u32> {
        let mut m = vec![0u32; self.rx_words];
        let mut set = |b: u32| m[(b / 32) as usize] |= 1 << (b % 32);
        set(self.i_bit);
        if !eight_bit {
            set(self.q_bit);
        }
        m
    }
}

/// The attribute writes for a rate: (phy rate, decimated rate if the ×8
/// decimator is used).
fn rate_plan(rate: f64) -> Result<(u64, Option<u64>)> {
    if !(RATE_MIN..=RATE_MAX).contains(&rate) {
        return Err(err(format!(
            "{:.3} MS/s is outside 0.26–61.44 MS/s",
            rate / 1e6
        )));
    }
    let r = rate.round() as u64;
    Ok(if rate < DECIM_BELOW {
        (r * 8, Some(r))
    } else {
        (r, None)
    })
}

fn check_freq(hz: f64) -> Result<()> {
    if (FREQ_MIN_HZ..=FREQ_MAX_HZ).contains(&hz) {
        Ok(())
    } else {
        Err(err(format!(
            "{:.3} MHz is outside the AD9361's 70–6000 MHz",
            hz / 1e6
        )))
    }
}

/// Program the receiver (and power the TX LO down) on the control link.
fn configure(c: &mut Client, ids: &Ids, s: &PlutoSettings) -> Result<()> {
    let (phy_rate, decimated) = rate_plan(s.sample_rate)?;
    let rx_path = Some(("voltage0", false));
    if ids.has_tx_lo {
        // Receive only: the transmit LO stays off.
        c.write_attr(&ids.phy, Some(("altvoltage1", true)), "powerdown", "1")?;
    }
    c.write_attr(
        &ids.phy,
        rx_path,
        "sampling_frequency",
        &phy_rate.to_string(),
    )?;
    if ids.has_decimator {
        // The FPGA decimates ×8 when asked for an eighth of the phy's rate,
        // and passes the phy's rate straight through otherwise.
        let r = decimated.unwrap_or(phy_rate);
        c.write_attr(
            &ids.rx,
            Some(("voltage0", false)),
            "sampling_frequency",
            &r.to_string(),
        )?;
    } else if decimated.is_some() {
        return Err(err(
            "this firmware has no ×8 decimator: use 2.084 MS/s or more",
        ));
    }
    let bw = s
        .bandwidth
        .unwrap_or(s.sample_rate)
        .clamp(200e3, 56e6)
        .round() as u64;
    c.write_attr(&ids.phy, rx_path, "rf_bandwidth", &bw.to_string())?;
    set_gain(c, ids, s.gain)?;
    c.write_attr(
        &ids.phy,
        Some(("altvoltage0", true)),
        "frequency",
        &(s.center_hz.round() as u64).to_string(),
    )?;
    Ok(())
}

fn set_gain(c: &mut Client, ids: &Ids, g: PlutoGain) -> Result<()> {
    let rx_path = Some(("voltage0", false));
    match g {
        PlutoGain::Agc => c.write_attr(&ids.phy, rx_path, "gain_control_mode", "slow_attack"),
        PlutoGain::Manual(db) => {
            c.write_attr(&ids.phy, rx_path, "gain_control_mode", "manual")?;
            c.write_attr(
                &ids.phy,
                rx_path,
                "hardwaregain",
                &format!("{:.0}", db.clamp(-3.0, 73.0)),
            )
        }
    }
}

/// Complex samples per `READBUF` block: about 1/30 s, a multiple of 4096,
/// 16 k–1 M.
fn block_samples(rate: f64) -> usize {
    ((rate / 30.0) as usize)
        .next_multiple_of(4096)
        .clamp(16_384, 1 << 20)
}

/// Kernel buffers behind the stream: enough to ride out ~0.25 s on the host.
fn kernel_buffers(rate: f64) -> usize {
    let block_s = block_samples(rate) as f64 / rate;
    ((0.25 / block_s).ceil() as usize).clamp(4, 16)
}

/// Convert one block of raw samples to IQ (12-bit samples in i16, or
/// Tezuka's I8/Q8 pairs), full scale ±1.
fn convert(raw: &[u8], eight_bit: bool, out: &mut Vec<Iq>) {
    out.clear();
    if eight_bit {
        const S: f32 = 1.0 / 128.0;
        out.extend(
            raw.as_chunks::<2>()
                .0
                .iter()
                .map(|&[i, q]| Iq::new(i as i8 as f32 * S, q as i8 as f32 * S)),
        );
    } else {
        // 12 bits sign-extended in little-endian i16 (`le:S12/16>>0`).
        const S: f32 = 1.0 / 2048.0;
        out.extend(raw.as_chunks::<4>().0.iter().map(|&[a, b, c, d]| {
            Iq::new(
                i16::from_le_bytes([a, b]) as f32 * S,
                i16::from_le_bytes([c, d]) as f32 * S,
            )
        }));
    }
}

/// Opens links: the real ones, or a test's fakes.
pub(crate) trait Connector: Send + Sync {
    /// `pipe` 0 is the control link; 1 carries samples.
    fn link(&self, pipe: usize) -> Result<Box<dyn Link>>;
    fn describe(&self) -> String;
}

struct Tcp(String);

impl Connector for Tcp {
    fn link(&self, _pipe: usize) -> Result<Box<dyn Link>> {
        use std::net::ToSocketAddrs;
        let addr = if self.0.contains(':') {
            self.0.clone()
        } else {
            format!("{}:{IIOD_PORT}", self.0)
        };
        let sock = addr
            .to_socket_addrs()
            .map_err(|e| err(format!("cannot resolve {addr}: {e}")))?
            .next()
            .ok_or_else(|| err(format!("cannot resolve {addr}")))?;
        let s = std::net::TcpStream::connect_timeout(&sock, Duration::from_secs(3))
            .map_err(|e| err(format!("no Pluto at {addr} ({e})")))?;
        let _ = s.set_nodelay(true);
        s.set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(err)?;
        Ok(Box::new(s))
    }
    fn describe(&self) -> String {
        format!("ip:{}", self.0)
    }
}

struct Usb(usb::UsbIio);

impl Connector for Usb {
    fn link(&self, pipe: usize) -> Result<Box<dyn Link>> {
        let l = if pipe == 0 {
            // One packet per request: see `UsbIio::pipe`.
            self.0.pipe(0, 512, 4)?
        } else {
            self.0.pipe(pipe, 256 << 10, 8)?
        };
        Ok(Box::new(l))
    }
    fn describe(&self) -> String {
        "usb".into()
    }
}

fn connector(uri: &PlutoUri) -> Result<Arc<dyn Connector>> {
    Ok(match uri {
        PlutoUri::Usb => Arc::new(Usb(usb::UsbIio::open()?)),
        PlutoUri::Ip(h) => Arc::new(Tcp(h.clone())),
        PlutoUri::Auto => {
            if usb::present() {
                match usb::UsbIio::open() {
                    Ok(u) => Arc::new(Usb(u)),
                    // No WinUSB driver on the IIO interface: its network
                    // link still works.
                    Err(e) => {
                        tracing::info!("{e}; trying the network link");
                        Arc::new(Tcp(DEFAULT_IP.into()))
                    }
                }
            } else {
                Arc::new(Tcp(DEFAULT_IP.into()))
            }
        }
    })
}

/// What the reader thread counts.
#[derive(Default)]
struct Counters {
    /// Blocks dropped because the consumer fell behind.
    overflows: AtomicU64,
    /// Blocks' worth of samples the radio never sent: IIOD gives no sign of
    /// an overrun on the Pluto, so this is the shortfall of what arrived
    /// against the time gone at the set rate (a link that cannot carry the
    /// rate shows here).
    link_lost: AtomicU64,
    /// Samples per second delivered since the start (f64 bits).
    delivered: AtomicU64,
}

/// Live controls on a running Pluto, usable from the GUI thread while the
/// engine reads samples.
#[derive(Clone)]
pub struct PlutoControl {
    ctl: Arc<Mutex<Client>>,
    ids: Arc<Ids>,
    settings: Arc<Mutex<PlutoSettings>>,
    info: Arc<PlutoInfo>,
    counters: Arc<Counters>,
}

impl PlutoControl {
    pub fn settings(&self) -> PlutoSettings {
        self.settings.lock().unwrap().clone()
    }

    pub fn info(&self) -> &PlutoInfo {
        &self.info
    }

    pub fn set_center(&self, hz: f64) -> Result<()> {
        check_freq(hz)?;
        self.ctl.lock().unwrap().write_attr(
            &self.ids.phy,
            Some(("altvoltage0", true)),
            "frequency",
            &(hz.round() as u64).to_string(),
        )?;
        self.settings.lock().unwrap().center_hz = hz;
        Ok(())
    }

    pub fn set_gain(&self, g: PlutoGain) -> Result<()> {
        set_gain(&mut self.ctl.lock().unwrap(), &self.ids, g)?;
        self.settings.lock().unwrap().gain = g;
        Ok(())
    }

    /// The gain the AD9361 is at now (the AGC's choice when it runs), dB.
    pub fn read_gain(&self) -> Result<f64> {
        let v = self.ctl.lock().unwrap().read_attr(
            &self.ids.phy,
            Some(("voltage0", false)),
            "hardwaregain",
        )?;
        v.split_whitespace()
            .next()
            .and_then(|x| x.parse().ok())
            .ok_or_else(|| err(format!("unreadable gain {v:?}")))
    }

    /// Blocks dropped because the consumer fell behind.
    pub fn overflows(&self) -> u64 {
        self.counters.overflows.load(Ordering::Relaxed)
    }

    /// Blocks' worth of samples that never came over the link (see
    /// [`Counters::link_lost`]).
    pub fn link_lost(&self) -> u64 {
        self.counters.link_lost.load(Ordering::Relaxed)
    }

    /// Everything lost: the figure the apps show as drops.
    pub fn lost(&self) -> u64 {
        self.overflows() + self.link_lost()
    }

    /// Samples per second the link has delivered since the start.
    pub fn delivered_rate(&self) -> f64 {
        f64::from_bits(self.counters.delivered.load(Ordering::Relaxed))
    }
}

/// A live Pluto receive stream.
pub struct PlutoSource {
    rx: Receiver<Vec<Iq>>,
    recycle: SyncSender<Vec<Iq>>,
    control: PlutoControl,
    /// Blocks still to throw away after a retune.
    skip: u32,
    flush_blocks: u32,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    done: Receiver<()>,
}

impl PlutoSource {
    /// Connect, configure and start receiving.
    pub fn open(settings: PlutoSettings) -> Result<Self> {
        check_freq(settings.center_hz)?;
        rate_plan(settings.sample_rate)?;
        let conn = connector(&settings.uri)?;
        Self::start(conn, settings)
    }

    pub(crate) fn start(conn: Arc<dyn Connector>, settings: PlutoSettings) -> Result<Self> {
        let mut ctl = Client::new(conn.link(0)?);
        let ctx = iiod::parse_context(&ctl.print()?)?;
        let ids = Ids::from(&ctx)?;
        let firmware = ctx.attr("fw_version").unwrap_or_default().to_string();
        let eight_bit = settings.eight_bit == EightBit::Auto
            && firmware.to_ascii_lowercase().starts_with("tezuka");
        configure(&mut ctl, &ids, &settings)?;
        let info = PlutoInfo {
            link: conn.describe(),
            firmware,
            model: ctx.attr("hw_model").unwrap_or_default().to_string(),
            eight_bit,
        };
        tracing::info!(
            "Pluto on {}: {} {}, {}-bit IQ",
            info.link,
            info.model,
            info.firmware,
            if eight_bit { 8 } else { 16 }
        );

        // The sample link: its own connection (TCP) or pipe (USB).
        let mut data = Client::new(conn.link(1)?);
        let samples = block_samples(settings.sample_rate);
        let bytes = samples * if eight_bit { 2 } else { 4 };
        let kbufs = kernel_buffers(settings.sample_rate);
        let _ = data.exec("TIMEOUT 5000\r\n");
        // Older iiods do not know SET; the default count (4) then stands.
        if let Err(e) = data.exec(&format!("SET {} BUFFERS_COUNT {kbufs}\r\n", ids.rx)) {
            tracing::debug!("{e}");
        }
        let mask = ids.mask(eight_bit);
        data.open(&ids.rx, samples, &mask)?;

        let (tx, rx) = mpsc::sync_channel::<Vec<Iq>>(QUEUE);
        let (recycle_tx, recycle_rx) = mpsc::sync_channel::<Vec<Iq>>(QUEUE + 4);
        let (done_tx, done_rx) = mpsc::sync_channel::<()>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters::default());
        let rate = settings.sample_rate;
        let thread = {
            let stop = Arc::clone(&stop);
            let counters = Arc::clone(&counters);
            let dev = ids.rx.clone();
            let words = ids.rx_words;
            std::thread::Builder::new()
                .name("pluto-rx".into())
                .spawn(move || {
                    let r = reader(
                        &mut data,
                        &dev,
                        bytes,
                        words,
                        eight_bit,
                        &tx,
                        &recycle_rx,
                        &stop,
                        &counters,
                        rate,
                    );
                    if let Err(e) = r
                        && !stop.load(Ordering::Relaxed)
                    {
                        tracing::warn!("Pluto read failed: {e}");
                    }
                    // No CLOSE: a READBUF is still being answered on this
                    // link. Dropping it ends the session on the radio (the
                    // socket closes; a USB pipe is closed by a request).
                    let _ = done_tx.send(());
                })
                .map_err(|e| err(format!("cannot start the reader: {e}")))?
        };
        Ok(PlutoSource {
            rx,
            recycle: recycle_tx,
            control: PlutoControl {
                ctl: Arc::new(Mutex::new(ctl)),
                ids: Arc::new(ids),
                settings: Arc::new(Mutex::new(settings)),
                info: Arc::new(info),
                counters,
            },
            skip: 0,
            // What may already sit in the kernel's buffers and our pipeline.
            flush_blocks: kbufs as u32 + 2,
            stop,
            thread: Some(thread),
            done: done_rx,
        })
    }

    /// A handle for retuning and gain changes while the engine reads.
    pub fn control(&self) -> PlutoControl {
        self.control.clone()
    }

    /// Moves the centre and throws away what was captured before it took
    /// effect. (DecSAT's [`IqSource`] has no retune, so this is an inherent
    /// method for whoever holds the source itself; the GUI retunes through
    /// [`PlutoControl::set_center`], and the decoder rides out the few stale
    /// blocks.)
    pub fn retune(&mut self, hz: f64) -> Result<bool> {
        self.control.set_center(hz)?;
        while let Ok(b) = self.rx.try_recv() {
            let _ = self.recycle.try_send(b);
        }
        self.skip = self.flush_blocks;
        Ok(true)
    }
}

/// The reader thread: two `READBUF`s in flight, convert, hand over.
#[allow(clippy::too_many_arguments)]
fn reader(
    data: &mut Client,
    dev: &str,
    bytes: usize,
    words: usize,
    eight_bit: bool,
    tx: &SyncSender<Vec<Iq>>,
    recycle: &Receiver<Vec<Iq>>,
    stop: &AtomicBool,
    counters: &Counters,
    rate: f64,
) -> Result<()> {
    let mut raw = Vec::with_capacity(bytes);
    let block = bytes / if eight_bit { 2 } else { 4 };
    data.readbuf_request(dev, bytes)?;
    // The clock starts at the first block (the kernel's buffers were
    // filling before it, so the count runs ahead at first: never a loss).
    let mut t0: Option<std::time::Instant> = None;
    let mut got = 0f64;
    let mut warned = false;
    while !stop.load(Ordering::Relaxed) {
        data.readbuf_request(dev, bytes)?;
        data.readbuf_reply(&mut raw, bytes, words)?;
        let mut v = recycle.try_recv().unwrap_or_default();
        convert(&raw, eight_bit, &mut v);
        match t0 {
            None => t0 = Some(std::time::Instant::now()),
            Some(t) => {
                got += v.len() as f64;
                let secs = t.elapsed().as_secs_f64();
                let short = (secs * rate - got) / block as f64;
                // Two blocks of slack for the timing of one reply.
                let lost = (short - 2.0).max(0.0) as u64;
                counters.link_lost.fetch_max(lost, Ordering::Relaxed);
                if secs > 0.0 {
                    let delivered = got / secs;
                    counters
                        .delivered
                        .store(delivered.to_bits(), Ordering::Relaxed);
                    if !warned && secs > 2.0 && delivered < 0.97 * rate {
                        warned = true;
                        tracing::warn!(
                            "Pluto: the link carries only {:.2} of {:.2} MS/s ({}); \
                             samples are being lost on the radio. Use a lower rate, or 8-bit \
                             IQ (Tezuka firmware), or a Pluto+ on Ethernet",
                            delivered / 1e6,
                            rate / 1e6,
                            if eight_bit { "8-bit" } else { "16-bit" }
                        );
                    }
                }
            }
        }
        match tx.try_send(v) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                counters.overflows.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => break,
        }
    }
    // The request still in flight is not waited for: its reply may never
    // finish arriving over USB (see `usb::UsbIio::pipe`), and the link is
    // dropped next anyway.
    Ok(())
}

impl IqSource for PlutoSource {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        loop {
            match self.rx.recv_timeout(Duration::from_secs(3)) {
                Ok(spent) if self.skip > 0 => {
                    self.skip -= 1;
                    let _ = self.recycle.try_send(spent);
                }
                Ok(mut buf) => {
                    std::mem::swap(out, &mut buf);
                    let _ = self.recycle.try_send(buf);
                    return Ok(out.len());
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(err("stopped delivering samples"));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(err("the stream stopped (unplugged?)"));
                }
            }
        }
    }

    fn sample_rate(&self) -> f64 {
        self.control.settings.lock().unwrap().sample_rate
    }

    fn center_freq(&self) -> f64 {
        self.control.settings.lock().unwrap().center_hz
    }

    fn describe(&self) -> String {
        let s = self.control.settings();
        let i = &self.control.info;
        format!(
            "Pluto ({}) @ {:.4} MHz, {:.3} MS/s, {}, {}-bit",
            i.link,
            s.center_hz / 1e6,
            s.sample_rate / 1e6,
            match s.gain {
                PlutoGain::Agc => "AGC".to_string(),
                PlutoGain::Manual(db) => format!("{db:.0} dB"),
            },
            if i.eight_bit { 8 } else { 16 }
        )
    }

    fn is_live(&self) -> bool {
        true
    }
}

impl Drop for PlutoSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Keep the reader from blocking on a full queue while it stops.
        while self.rx.try_recv().is_ok() {}
        if self.done.recv_timeout(Duration::from_secs(3)).is_ok() {
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        } else {
            tracing::warn!("Pluto reader did not stop; detaching it");
        }
    }
}

#[cfg(test)]
mod tests;
