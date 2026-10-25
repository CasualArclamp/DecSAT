// Fobos SDR support for DecSAT (OM5NA, after the OnAir driver of 2026-10-11)
//! Live RigExpert Fobos SDR front end.
//!
//! The Fobos is a 14-bit, receive-only SDR with a USB 3.0 link: 50–6000 MHz
//! through its double-conversion front end and 8–50 MS/s of IQ (64 MS/s on a
//! rev. 4 board), so a whole 40 MHz slice of a Ku LNB's IF arrives at once —
//! twice what a HackRF can stream.
//!
//! RigExpert's own C library does the USB work; it is **loaded at run time**,
//! so DecSAT builds and runs without it and only a Fobos user needs the file.
//! Both of RigExpert's libraries are understood, tried in this order:
//!
//! - `libfobos-sdr-agile` (`fobos_sdr.dll`, `libfobos_sdr.so`, functions
//!   `fobos_sdr_*`) for the **agile** firmware;
//! - `libfobos` (`fobos.dll`, `libfobos.so`, functions `fobos_rx_*`) for the
//!   stock firmware.
//!
//! Each library sees only boards running its own firmware, so the first one
//! that finds a radio is used. `DECSAT_FOBOS_LIB` names a library file to use
//! instead of searching (beside the exe first, then the system's paths).
//!
//! **Receive only, RF input only.** The HF direct-sampling inputs are not used
//! (DecSAT works on an LNB's L-band IF), and the Fobos has no transmitter. The
//! board does not power its antenna port; feed an LNB from an inserter.
//!
//! The library delivers float IQ already DC-corrected and I/Q-balanced, with a
//! full scale of ±0.25 (14 bits scaled as 16); samples are multiplied by 4 so
//! the input meter reads dBFS as it does for the HackRF.
//!
//! A reader thread sits in the library's `read_async` (which runs libusb's
//! event loop until cancelled) and hands each buffer over a bounded channel,
//! dropping and counting whole buffers when the consumer falls behind.
//! `cancel_async` only takes effect while the stream is running, so stopping
//! repeats it until `read_async` has returned.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use decsat_core::{Error, Iq, Result};
use libloading::Library;

use crate::IqSource;

/// Complex samples per USB buffer the library hands back: its default, ~1.3 ms
/// at 50 MS/s.
const BUF_LEN: u32 = 65_536;
/// USB transfers the library keeps in flight.
const BUF_COUNT: u32 = 16;
/// Buffers queued for the consumer before dropping: ~0.35 s at 50 MS/s,
/// ~0.9 s at 20 MS/s.
const QUEUE: usize = 256;
/// The library's samples span ±0.25; this brings them to ±1.
const SCALE: f32 = 4.0;
/// The RF input's range (below it the board samples its HF inputs directly).
pub const FOBOS_MIN_HZ: u64 = 50_000_000;
pub const FOBOS_MAX_HZ: u64 = 6_000_000_000;

/// Sample rates every Fobos offers (rev. 2/3 boards; rev. 4 adds 58 and 64
/// MS/s). The board's own list decides at open time.
pub const FOBOS_RATES: [u32; 9] = [
    8_000_000, 10_000_000, 12_500_000, 16_000_000, 20_000_000, 25_000_000, 32_000_000, 40_000_000,
    50_000_000,
];

/// Gain settings for the Fobos's two receive stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FobosGains {
    /// RF LNA step, 0–3 (2 ≈ +16 dB, 3 ≈ +33 dB).
    pub lna: u8,
    /// IF VGA step, 0–31, about 2 dB each.
    pub vga: u8,
}

impl Default for FobosGains {
    fn default() -> Self {
        // An LNB already gives 50–60 dB; start low and let the input meter
        // say whether to raise it.
        FobosGains { lna: 1, vga: 8 }
    }
}

impl FobosGains {
    /// Clamp to the ranges the library accepts.
    pub fn snapped(self) -> Self {
        FobosGains {
            lna: self.lna.min(3),
            vga: self.vga.min(31),
        }
    }

    /// The VGA step as dB, for display.
    pub fn vga_db(self) -> u32 {
        u32::from(self.vga) * 2
    }
}

/// What a live session is set to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FobosSettings {
    pub center_hz: u64,
    /// 8–50 MS/s, one of the board's own rates (the nearest is taken).
    pub sample_rate: u32,
    pub gains: FobosGains,
    /// Lock to a 10 MHz reference on the CLK IN socket.
    pub external_clock: bool,
}

impl Default for FobosSettings {
    fn default() -> Self {
        FobosSettings {
            center_hz: 1_200_000_000,
            sample_rate: 20_000_000,
            gains: FobosGains::default(),
            external_clock: false,
        }
    }
}

/// Which of RigExpert's libraries is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FobosFlavour {
    /// `libfobos-sdr-agile`, `fobos_sdr_*`.
    Agile,
    /// `libfobos`, `fobos_rx_*`.
    Stock,
}

impl FobosFlavour {
    fn prefix(self) -> &'static str {
        match self {
            FobosFlavour::Agile => "fobos_sdr_",
            FobosFlavour::Stock => "fobos_rx_",
        }
    }

    fn file_names(self) -> &'static [&'static str] {
        match self {
            FobosFlavour::Agile => {
                if cfg!(windows) {
                    &["fobos_sdr.dll", "libfobos_sdr.dll"]
                } else if cfg!(target_os = "macos") {
                    &["libfobos_sdr.dylib"]
                } else {
                    &["libfobos_sdr.so", "libfobos_sdr.so.1"]
                }
            }
            FobosFlavour::Stock => {
                if cfg!(windows) {
                    &["fobos.dll", "libfobos.dll"]
                } else if cfg!(target_os = "macos") {
                    &["libfobos.dylib"]
                } else {
                    &["libfobos.so", "libfobos.so.1"]
                }
            }
        }
    }
}

type Dev = *mut c_void;
type CbAgile = unsafe extern "C" fn(*mut f32, u32, Dev, *mut c_void);
type CbStock = unsafe extern "C" fn(*mut f32, u32, *mut c_void);

/// The library's entry points, resolved once.
///
/// Rust note: the function pointers stay valid only while `_lib` is loaded,
/// which is why they live in the same struct and the struct is only ever
/// shared behind an `Arc`.
struct FobosLib {
    flavour: FobosFlavour,
    path: String,
    get_api_info: unsafe extern "C" fn(*mut c_char, *mut c_char) -> c_int,
    get_device_count: unsafe extern "C" fn() -> c_int,
    open: unsafe extern "C" fn(*mut Dev, u32) -> c_int,
    close: unsafe extern "C" fn(Dev) -> c_int,
    get_board_info: unsafe extern "C" fn(
        Dev,
        *mut c_char,
        *mut c_char,
        *mut c_char,
        *mut c_char,
        *mut c_char,
    ) -> c_int,
    /// Agile: (dev, value); stock: (dev, value, *actual).
    set_frequency: *const c_void,
    set_samplerate: *const c_void,
    set_direct_sampling: unsafe extern "C" fn(Dev, c_uint) -> c_int,
    set_lna_gain: unsafe extern "C" fn(Dev, c_uint) -> c_int,
    set_vga_gain: unsafe extern "C" fn(Dev, c_uint) -> c_int,
    get_samplerates: unsafe extern "C" fn(Dev, *mut f64, *mut c_uint) -> c_int,
    set_clk_source: unsafe extern "C" fn(Dev, c_int) -> c_int,
    /// Agile and stock callbacks differ; see `CbAgile` / `CbStock`.
    read_async: *const c_void,
    cancel_async: unsafe extern "C" fn(Dev) -> c_int,
    error_name: unsafe extern "C" fn(c_int) -> *const c_char,
    _lib: Library,
}

// Rust note: raw pointers are neither `Send` nor `Sync` by default. These
// point at the library's code, which any thread may call.
unsafe impl Send for FobosLib {}
unsafe impl Sync for FobosLib {}

impl FobosLib {
    fn load_file(flavour: FobosFlavour, path: &str) -> std::result::Result<Self, String> {
        // SAFETY: loading a library runs its initialisers; RigExpert's have
        // none beyond the C runtime's.
        let lib = unsafe { Library::new(path) }.map_err(|e| format!("{path}: {e}"))?;
        let p = flavour.prefix();
        macro_rules! sym {
            ($name:literal) => {{
                let full = format!("{p}{}\0", $name);
                // SAFETY: the type is the header's; a missing symbol is an
                // error, not undefined behaviour.
                let s: libloading::Symbol<*const c_void> = unsafe { lib.get(full.as_bytes()) }
                    .map_err(|_| format!("{path} has no {p}{}", $name))?;
                *s
            }};
        }
        macro_rules! func {
            ($name:literal) => {{
                let raw = sym!($name);
                // SAFETY: a function pointer of the header's signature.
                unsafe { as_fn(raw) }
            }};
        }
        Ok(FobosLib {
            flavour,
            path: path.to_string(),
            get_api_info: func!("get_api_info"),
            get_device_count: func!("get_device_count"),
            open: func!("open"),
            close: func!("close"),
            get_board_info: func!("get_board_info"),
            set_frequency: sym!("set_frequency"),
            set_samplerate: sym!("set_samplerate"),
            set_direct_sampling: func!("set_direct_sampling"),
            set_lna_gain: func!("set_lna_gain"),
            set_vga_gain: func!("set_vga_gain"),
            get_samplerates: func!("get_samplerates"),
            set_clk_source: func!("set_clk_source"),
            read_async: sym!("read_async"),
            cancel_async: func!("cancel_async"),
            error_name: func!("error_name"),
            _lib: lib,
        })
    }

    /// Paths to try for one flavour: the override, beside the exe, then the
    /// bare name (the system's search path).
    fn candidates(flavour: FobosFlavour) -> Vec<String> {
        let mut out = Vec::new();
        let dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(PathBuf::from));
        for name in flavour.file_names() {
            if let Some(d) = &dir {
                let p = d.join(name);
                if p.exists() {
                    out.push(p.to_string_lossy().into_owned());
                }
            }
            out.push((*name).to_string());
        }
        out
    }

    /// Every library that loads, agile first. With `DECSAT_FOBOS_LIB` set, only
    /// that file (as whichever flavour it exports).
    fn load_all() -> (Vec<FobosLib>, Vec<String>) {
        let mut libs = Vec::new();
        let mut why = Vec::new();
        if let Some(path) = std::env::var_os("DECSAT_FOBOS_LIB") {
            let path = path.to_string_lossy().into_owned();
            for f in [FobosFlavour::Agile, FobosFlavour::Stock] {
                match FobosLib::load_file(f, &path) {
                    Ok(l) => {
                        libs.push(l);
                        break;
                    }
                    Err(e) => why.push(e),
                }
            }
            return (libs, why);
        }
        for f in [FobosFlavour::Agile, FobosFlavour::Stock] {
            for c in FobosLib::candidates(f) {
                match FobosLib::load_file(f, &c) {
                    Ok(l) => {
                        libs.push(l);
                        break;
                    }
                    Err(e) => why.push(e),
                }
            }
        }
        (libs, why)
    }

    fn err_name(&self, code: c_int) -> String {
        // SAFETY: returns a static string (or null for an unknown code).
        let p = unsafe { (self.error_name)(code) };
        if p.is_null() {
            format!("error {code}")
        } else {
            // SAFETY: a NUL-terminated static string.
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        }
    }

    fn check(&self, what: &str, code: c_int) -> Result<()> {
        if code == 0 {
            Ok(())
        } else {
            Err(Error::Other(format!(
                "Fobos: {what} failed: {} ({code})",
                self.err_name(code)
            )))
        }
    }

    fn set_frequency(&self, dev: Dev, hz: f64) -> c_int {
        // SAFETY: the signature the flavour's header gives.
        unsafe {
            match self.flavour {
                FobosFlavour::Agile => {
                    let f: unsafe extern "C" fn(Dev, f64) -> c_int =
                        std::mem::transmute(self.set_frequency);
                    f(dev, hz)
                }
                FobosFlavour::Stock => {
                    let f: unsafe extern "C" fn(Dev, f64, *mut f64) -> c_int =
                        std::mem::transmute(self.set_frequency);
                    let mut actual = 0.0;
                    f(dev, hz, &mut actual)
                }
            }
        }
    }

    fn set_samplerate(&self, dev: Dev, sps: f64) -> c_int {
        // SAFETY: as above.
        unsafe {
            match self.flavour {
                FobosFlavour::Agile => {
                    let f: unsafe extern "C" fn(Dev, f64) -> c_int =
                        std::mem::transmute(self.set_samplerate);
                    f(dev, sps)
                }
                FobosFlavour::Stock => {
                    let f: unsafe extern "C" fn(Dev, f64, *mut f64) -> c_int =
                        std::mem::transmute(self.set_samplerate);
                    let mut actual = 0.0;
                    f(dev, sps, &mut actual)
                }
            }
        }
    }

    fn version(&self) -> String {
        let mut lib = [0 as c_char; 64];
        let mut drv = [0 as c_char; 64];
        // SAFETY: two 64-byte buffers, as the header asks.
        unsafe { (self.get_api_info)(lib.as_mut_ptr(), drv.as_mut_ptr()) };
        cstr(&lib)
    }
}

/// Reinterpret a symbol's address as the function pointer type the field
/// wants.
///
/// Rust note: `transmute_copy` lets one generic helper serve every field (the
/// target type is inferred from where the result goes); the assertion keeps
/// it to pointer-sized targets, i.e. function pointers.
///
/// # Safety
/// `raw` must be a function whose signature is `F`.
unsafe fn as_fn<F: Copy>(raw: *const c_void) -> F {
    assert_eq!(
        std::mem::size_of::<F>(),
        std::mem::size_of::<*const c_void>()
    );
    // SAFETY: same size, and the caller vouches for the signature.
    unsafe { std::mem::transmute_copy::<*const c_void, F>(&raw) }
}

fn cstr(b: &[c_char]) -> String {
    let bytes: Vec<u8> = b
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// The board the session opened.
#[derive(Debug, Clone, Default)]
pub struct FobosInfo {
    pub serial: String,
    pub hw_revision: String,
    pub fw_version: String,
    pub library: String,
    pub library_version: String,
    pub agile: bool,
    /// The sample rates the board offers, Hz, ascending.
    pub rates: Vec<u32>,
}

/// An open board; closed when the last handle goes.
struct Device {
    lib: Arc<FobosLib>,
    dev: Dev,
    /// Serialises control calls from the GUI thread.
    ctl: Mutex<()>,
}

// SAFETY: the library guards its own device state; control calls are
// serialised by `ctl`, and the streaming thread only runs `read_async`.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: the stream has stopped (the reader thread holds a handle
        // until `read_async` returns), so nothing else uses `dev`.
        unsafe { (self.lib.close)(self.dev) };
    }
}

/// Live controls on a running Fobos, usable from the GUI thread while the
/// engine reads samples.
#[derive(Clone)]
pub struct FobosControl {
    dev: Arc<Device>,
    settings: Arc<Mutex<FobosSettings>>,
    overflows: Arc<AtomicU64>,
    info: Arc<FobosInfo>,
}

impl FobosControl {
    pub fn settings(&self) -> FobosSettings {
        *self.settings.lock().unwrap()
    }

    pub fn info(&self) -> &FobosInfo {
        &self.info
    }

    /// Retune (RF input, 50–6000 MHz).
    pub fn set_center(&self, hz: u64) -> Result<()> {
        if !(FOBOS_MIN_HZ..=FOBOS_MAX_HZ).contains(&hz) {
            return Err(Error::Other(format!(
                "Fobos: {:.3} MHz is outside its RF input's 50–6000 MHz",
                hz as f64 / 1e6
            )));
        }
        let _g = self.dev.ctl.lock().unwrap();
        let code = self.dev.lib.set_frequency(self.dev.dev, hz as f64);
        self.dev.lib.check("set frequency", code)?;
        self.settings.lock().unwrap().center_hz = hz;
        Ok(())
    }

    pub fn set_gains(&self, gains: FobosGains) -> Result<()> {
        let g = gains.snapped();
        let _l = self.dev.ctl.lock().unwrap();
        let lib = &self.dev.lib;
        // SAFETY: an open device; values in the library's ranges.
        lib.check("set LNA gain", unsafe {
            (lib.set_lna_gain)(self.dev.dev, g.lna.into())
        })?;
        lib.check("set VGA gain", unsafe {
            (lib.set_vga_gain)(self.dev.dev, g.vga.into())
        })?;
        self.settings.lock().unwrap().gains = g;
        Ok(())
    }

    /// Buffers dropped because the consumer fell behind.
    pub fn overflows(&self) -> u64 {
        self.overflows.load(Ordering::Relaxed)
    }
}

/// What the library's callback needs: where to send, and what to count.
struct CbCtx {
    tx: SyncSender<Vec<Iq>>,
    overflows: Arc<AtomicU64>,
    /// Emptied buffers coming back from the consumer, to avoid allocating.
    spare: Receiver<Vec<Iq>>,
}

fn deliver(ctx: &CbCtx, buf: *const f32, len: u32) {
    if buf.is_null() || len == 0 {
        return;
    }
    // SAFETY: the library hands `len` complex samples, `2·len` floats, valid
    // for the duration of the call.
    let s = unsafe { std::slice::from_raw_parts(buf, len as usize * 2) };
    let mut v = ctx.spare.try_recv().unwrap_or_default();
    v.clear();
    v.extend(
        s.chunks_exact(2)
            .map(|p| Iq::new(p[0] * SCALE, p[1] * SCALE)),
    );
    match ctx.tx.try_send(v) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            ctx.overflows.fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
}

unsafe extern "C" fn cb_agile(buf: *mut f32, len: u32, _dev: Dev, user: *mut c_void) {
    // A panic must not unwind into C.
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: `user` is the `CbCtx` the reader thread owns for the whole
        // of `read_async`.
        let ctx = unsafe { &*(user as *const CbCtx) };
        deliver(ctx, buf, len);
    });
}

unsafe extern "C" fn cb_stock(buf: *mut f32, len: u32, user: *mut c_void) {
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: as in `cb_agile`.
        let ctx = unsafe { &*(user as *const CbCtx) };
        deliver(ctx, buf, len);
    });
}

/// A live Fobos SDR receive stream.
pub struct FobosSource {
    rx: Receiver<Vec<Iq>>,
    spare: SyncSender<Vec<Iq>>,
    control: FobosControl,
    thread: Option<JoinHandle<()>>,
    sample_rate: f64,
}

/// Pick the board's rate nearest the one asked for.
fn nearest_rate(rates: &[u32], want: u32) -> u32 {
    rates
        .iter()
        .copied()
        .min_by_key(|&r| (i64::from(r) - i64::from(want)).unsigned_abs())
        .unwrap_or(want)
}

impl FobosSource {
    /// Whether a Fobos library can be loaded at all (for greying out the
    /// choice in a menu), and which.
    pub fn library_available() -> Option<String> {
        let (libs, _) = FobosLib::load_all();
        libs.first().map(|l| l.path.clone())
    }

    /// Open the first Fobos, configure it and start receiving.
    pub fn open(settings: FobosSettings) -> Result<Self> {
        if !(FOBOS_MIN_HZ..=FOBOS_MAX_HZ).contains(&settings.center_hz) {
            return Err(Error::Other(format!(
                "Fobos: {:.3} MHz is outside its RF input's 50–6000 MHz",
                settings.center_hz as f64 / 1e6
            )));
        }
        let (libs, why) = FobosLib::load_all();
        if libs.is_empty() {
            return Err(Error::Other(format!(
                "Fobos: RigExpert's library was not found. Put fobos_sdr.dll (agile firmware) \
                 or fobos.dll (stock firmware) beside DecSAT, or set DECSAT_FOBOS_LIB to its \
                 path. ({})",
                why.join("; ")
            )));
        }
        let mut tried = Vec::new();
        let lib = libs
            .into_iter()
            .find(|l| {
                // SAFETY: no arguments; enumerates the USB bus.
                let n = unsafe { (l.get_device_count)() };
                tried.push(format!("{} sees {}", l.path, n.max(0)));
                n > 0
            })
            .map(Arc::new)
            .ok_or_else(|| {
                Error::Other(format!(
                    "no Fobos SDR found ({}). Is it plugged into USB 3, not in use by another \
                     program, and on the driver RigExpert's installer sets up? A board with the \
                     agile firmware needs fobos_sdr.dll, the stock firmware fobos.dll.",
                    tried.join(", ")
                ))
            })?;

        let mut dev: Dev = std::ptr::null_mut();
        // SAFETY: `dev` receives the handle.
        lib.check("open", unsafe { (lib.open)(&mut dev, 0) })?;
        let device = Arc::new(Device {
            lib: Arc::clone(&lib),
            dev,
            ctl: Mutex::new(()),
        });

        let mut hw = [0 as c_char; 64];
        let mut fw = [0 as c_char; 64];
        let mut manuf = [0 as c_char; 64];
        let mut product = [0 as c_char; 64];
        let mut serial = [0 as c_char; 64];
        // SAFETY: five 64-byte buffers (FOBOS_INFO_LEN).
        unsafe {
            (lib.get_board_info)(
                dev,
                hw.as_mut_ptr(),
                fw.as_mut_ptr(),
                manuf.as_mut_ptr(),
                product.as_mut_ptr(),
                serial.as_mut_ptr(),
            )
        };

        let mut rates_buf = [0.0f64; 64];
        let mut count: c_uint = 0;
        // SAFETY: a buffer larger than any board's list (at most 11 today).
        let code = unsafe { (lib.get_samplerates)(dev, rates_buf.as_mut_ptr(), &mut count) };
        let mut rates: Vec<u32> = if code == 0 {
            rates_buf[..(count as usize).min(rates_buf.len())]
                .iter()
                .map(|&r| r.round() as u32)
                .filter(|&r| r > 0)
                .collect()
        } else {
            FOBOS_RATES.to_vec()
        };
        rates.sort_unstable();
        rates.dedup();
        let rate = nearest_rate(&rates, settings.sample_rate);
        let gains = settings.gains.snapped();

        // SAFETY: an open device, idle; values within the library's ranges.
        unsafe {
            lib.check("RF input", (lib.set_direct_sampling)(dev, 0))?;
            lib.check("set sample rate", lib.set_samplerate(dev, rate as f64))?;
            lib.check(
                "clock source",
                (lib.set_clk_source)(dev, c_int::from(settings.external_clock)),
            )?;
            lib.check(
                "set frequency",
                lib.set_frequency(dev, settings.center_hz as f64),
            )?;
            lib.check("set LNA gain", (lib.set_lna_gain)(dev, gains.lna.into()))?;
            lib.check("set VGA gain", (lib.set_vga_gain)(dev, gains.vga.into()))?;
        }

        let info = FobosInfo {
            serial: cstr(&serial),
            hw_revision: cstr(&hw),
            fw_version: cstr(&fw),
            library: lib.path.clone(),
            library_version: lib.version(),
            agile: lib.flavour == FobosFlavour::Agile,
            rates,
        };
        tracing::info!(
            "Fobos {} (hw {}, fw {}) via {} {}",
            info.serial,
            info.hw_revision,
            info.fw_version,
            info.library,
            info.library_version
        );

        let (tx, rx) = mpsc::sync_channel::<Vec<Iq>>(QUEUE);
        let (spare_tx, spare_rx) = mpsc::sync_channel::<Vec<Iq>>(8);
        let overflows = Arc::new(AtomicU64::new(0));
        let thread = {
            let device = Arc::clone(&device);
            let overflows = Arc::clone(&overflows);
            std::thread::Builder::new()
                .name("fobos-rx".into())
                .spawn(move || {
                    let ctx = Box::new(CbCtx {
                        tx,
                        overflows,
                        spare: spare_rx,
                    });
                    let user = &*ctx as *const CbCtx as *mut c_void;
                    let lib = &device.lib;
                    // SAFETY: `ctx` outlives the call, which returns only once
                    // the stream is cancelled (or the board is lost).
                    let code = unsafe {
                        match lib.flavour {
                            FobosFlavour::Agile => {
                                let f: unsafe extern "C" fn(
                                    Dev,
                                    CbAgile,
                                    *mut c_void,
                                    u32,
                                    u32,
                                )
                                    -> c_int = std::mem::transmute(lib.read_async);
                                f(device.dev, cb_agile, user, BUF_COUNT, BUF_LEN)
                            }
                            FobosFlavour::Stock => {
                                let f: unsafe extern "C" fn(
                                    Dev,
                                    CbStock,
                                    *mut c_void,
                                    u32,
                                    u32,
                                )
                                    -> c_int = std::mem::transmute(lib.read_async);
                                f(device.dev, cb_stock, user, BUF_COUNT, BUF_LEN)
                            }
                        }
                    };
                    if code != 0 {
                        tracing::warn!("Fobos stream ended: {}", lib.err_name(code));
                    }
                    // Dropping `ctx` closes the channel: the consumer sees
                    // the stream end.
                    drop(ctx);
                })
                .map_err(|e| Error::Other(format!("cannot start the Fobos reader: {e}")))?
        };

        Ok(FobosSource {
            rx,
            spare: spare_tx,
            control: FobosControl {
                dev: device,
                settings: Arc::new(Mutex::new(FobosSettings {
                    sample_rate: rate,
                    gains,
                    ..settings
                })),
                overflows,
                info: Arc::new(info),
            },
            thread: Some(thread),
            sample_rate: rate as f64,
        })
    }

    /// A handle for retuning and gain changes while the engine reads.
    pub fn control(&self) -> FobosControl {
        self.control.clone()
    }
}

impl IqSource for FobosSource {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        match self.rx.recv_timeout(Duration::from_secs(2)) {
            Ok(mut v) => {
                // Hand the caller's old buffer back to the callback for reuse.
                std::mem::swap(out, &mut v);
                v.clear();
                let _ = self.spare.try_send(v);
                Ok(out.len())
            }
            Err(RecvTimeoutError::Timeout) => {
                Err(Error::Other("Fobos stopped delivering samples".into()))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(Error::Other("Fobos stream stopped (unplugged?)".into()))
            }
        }
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn center_freq(&self) -> f64 {
        self.control.settings().center_hz as f64
    }

    fn describe(&self) -> String {
        let s = self.control.settings();
        format!(
            "Fobos SDR {} @ {:.4} MHz, {:.2} MS/s, LNA {} VGA {} dB",
            self.control.info.serial,
            s.center_hz as f64 / 1e6,
            s.sample_rate as f64 / 1e6,
            s.gains.lna,
            s.gains.vga_db()
        )
    }

    fn is_live(&self) -> bool {
        true
    }
}

impl Drop for FobosSource {
    fn drop(&mut self) {
        let lib = Arc::clone(&self.control.dev.lib);
        let dev = self.control.dev.dev;
        let t0 = Instant::now();
        if let Some(t) = self.thread.take() {
            // `cancel_async` is ignored until the stream is running, so keep
            // asking until `read_async` has returned (bounded, in case the
            // board has gone).
            while !t.is_finished() && t0.elapsed() < Duration::from_secs(5) {
                // SAFETY: only sets the library's cancel flags.
                unsafe { (lib.cancel_async)(dev) };
                while self.rx.try_recv().is_ok() {}
                std::thread::sleep(Duration::from_millis(20));
            }
            if t.is_finished() {
                let _ = t.join();
            } else {
                tracing::warn!("Fobos stream did not stop; leaving its thread");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gains_clamp_to_library_ranges() {
        let g = FobosGains { lna: 9, vga: 40 }.snapped();
        assert_eq!((g.lna, g.vga), (3, 31));
        assert_eq!(g.vga_db(), 62);
    }

    #[test]
    fn nearest_rate_picks_the_closest() {
        let r = FOBOS_RATES;
        assert_eq!(nearest_rate(&r, 20_000_000), 20_000_000);
        assert_eq!(nearest_rate(&r, 2_000_000), 8_000_000);
        assert_eq!(nearest_rate(&r, 60_000_000), 50_000_000);
        assert_eq!(nearest_rate(&r, 13_000_000), 12_500_000);
    }

    #[test]
    fn rejects_frequencies_outside_the_rf_input() {
        for hz in [10_000_000u64, 7_000_000_000] {
            let s = FobosSettings {
                center_hz: hz,
                ..Default::default()
            };
            // Checked before loading anything, so this needs no radio.
            assert!(FobosSource::open(s).is_err());
        }
    }

    /// Against a library (real or the test fake): set `DECSAT_FOBOS_LIB` and
    /// run `cargo test -p decsat-io --features fobos live_fobos -- --ignored
    /// --nocapture`. Receive only.
    #[test]
    #[ignore = "needs a Fobos (or DECSAT_FOBOS_LIB pointing at a fake)"]
    fn live_fobos() {
        let mut src = FobosSource::open(FobosSettings {
            center_hz: 1_200_000_000,
            sample_rate: 20_000_000,
            ..Default::default()
        })
        .expect("open");
        let c = src.control();
        eprintln!("{:?}", c.info());
        let mut buf = Vec::new();
        let mut n = 0usize;
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(1) {
            n += src.read(&mut buf).expect("read");
        }
        c.set_center(1_300_000_000).expect("retune");
        c.set_gains(FobosGains { lna: 2, vga: 10 }).expect("gains");
        for _ in 0..10 {
            n += src.read(&mut buf).expect("read after retune");
        }
        let rate = n as f64 / t0.elapsed().as_secs_f64();
        let rms = (buf.iter().map(|s| s.norm_sqr()).sum::<f32>() / buf.len() as f32).sqrt();
        eprintln!(
            "{} — {:.2} MS/s delivered, rms {rms:.3}, overflows {}",
            src.describe(),
            rate / 1e6,
            c.overflows()
        );
        assert!(rate > 15.0e6, "only {:.2} MS/s", rate / 1e6);
        drop(src);
        drop(c);
        // Stopped at once, before the stream runs: `cancel_async` is ignored
        // until then, and the board must still close so it can open again.
        let t0 = Instant::now();
        drop(FobosSource::open(FobosSettings::default()).expect("reopen"));
        eprintln!("open and stop at once: {:?}", t0.elapsed());
        drop(FobosSource::open(FobosSettings::default()).expect("open a third time"));
    }
}
