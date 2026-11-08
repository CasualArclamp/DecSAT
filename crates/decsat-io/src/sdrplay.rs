// made by CasualArclamp
//! Live SDRplay front end: RSPduo (the target), and any other RSP the API
//! lists (RSP1/1A/1B, RSP2, RSPdx) on a best-effort basis.
//!
//! SDRplay's API (v3.x) does the USB work in a background service; this
//! module **loads its library at run time**, so DecSAT builds and runs without
//! the SDK and only an SDRplay user needs it installed (from sdrplay.com, which
//! also installs the "SDRplay API Service" the library talks to). The struct
//! layouts and enums below are transcribed from the v3.15 headers
//! (`sdrplay_api*.h`); only the leading fields of the big parameter structs
//! are declared, because the tail (per-model extras: notch filters, bias-T,
//! antenna select) is reached through pointers the API owns and DecSAT leaves
//! at their defaults. That keeps the layout independent of the RSP models the
//! header describes after those fields. API 3.07 to 3.x share this prefix.
//!
//! **RSPduo mode:** single tuner (the only mode a plain IQ source needs): the
//! RSPduo is opened as `Single_Tuner` on Tuner A (the 50 ohm SMA, "Tuner 1") or
//! Tuner B. Dual-tuner and master/slave modes are not used.
//!
//! **Receive only; bias-T stays off** (its default): DecSAT feeds an LNB from
//! an external inserter.
//!
//! **Signal path:** zero-IF at the chosen rate (2–10.66 MS/s, the analogue
//! bandwidth picked to match), the API's AGC off so the two gain stages below
//! are what you set, DC and IQ-imbalance correction left on. The API is
//! callback-driven: it calls us with 16-bit I and Q arrays; each call is turned
//! into a `Vec<Iq>` and handed over a bounded channel, dropping and counting
//! whole buffers when the consumer falls behind.
//!
//! **Gain is *reduction*:** `lna` is the LNA state (0 = most gain, each step
//! about 7 dB less; the number of states depends on the model and band) and
//! `gr` the IF gain reduction in dB (20 = most gain, 59 = least).
//!
//! `DECSAT_SDRPLAY_LIB` names a library file to use instead of searching.

use std::ffi::{CStr, c_char, c_int, c_uint, c_ushort, c_void};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use decsat_core::{Error, Iq, Result};
use libloading::Library;

use crate::IqSource;

/// Complex samples queued for the consumer before dropping (the API's packets
/// are small, ~2 k samples).
const QUEUE: usize = 512;
/// Integer samples to unit scale. The API delivers 16-bit values; DecSAT
/// treats them as full scale at ±32768 so the input meter reads dBFS as it
/// does for the other radios. (The ADC is 12 bit, so a strong signal reads
/// lower than 0 dBFS; judge the gain by the overload warning, not the meter.)
const SCALE: f32 = 1.0 / 32_768.0;

/// Tuning range of the RSPs (the RSPduo: 1 kHz to 2 GHz).
pub const SDRPLAY_MIN_HZ: u64 = 1_000;
pub const SDRPLAY_MAX_HZ: u64 = 2_000_000_000;
/// Zero-IF sample rates the picker offers; the API takes anything from 2 to
/// 10.66667 MS/s.
pub const SDRPLAY_RATES: [u32; 5] = [2_000_000, 4_000_000, 6_000_000, 8_000_000, 10_000_000];
const FS_MIN: f64 = 2_000_000.0;
const FS_MAX: f64 = 10_666_667.0;

// ---- the C API (sdrplay_api*.h, v3.15) ------------------------------------

type Handle = *mut c_void;
type ErrT = c_int;

const TUNER_A: c_int = 1;
const TUNER_B: c_int = 2;
const MODE_SINGLE_TUNER: c_int = 1;
const RSPDUO_ID: u8 = 3;
const IF_ZERO: c_int = 0;
const LO_AUTO: c_int = 1;
const AGC_DISABLE: c_int = 0;
const NORMAL_MIN_GR: c_int = 20;
const MAX_BB_GR: u8 = 59;
// sdrplay_api_ReasonForUpdateT
const UPDATE_TUNER_GR: c_uint = 0x0000_8000;
const UPDATE_TUNER_FRF: c_uint = 0x0002_0000;
// sdrplay_api_EventT
const EVENT_POWER_OVERLOAD: c_int = 1;
const EVENT_DEVICE_REMOVED: c_int = 2;
const EVENT_DEVICE_FAILURE: c_int = 4;
// sdrplay_api_PowerOverloadCbEventIdT
const OVERLOAD_DETECTED: c_int = 0;

#[repr(C)]
#[derive(Clone, Copy)]
struct DeviceT {
    ser_no: [c_char; 64],
    hw_ver: u8,
    tuner: c_int,
    rsp_duo_mode: c_int,
    valid: u8,
    rsp_duo_sample_freq: f64,
    dev: Handle,
}

#[repr(C)]
struct GainValuesT {
    curr: f32,
    max: f32,
    min: f32,
}

#[repr(C)]
struct GainT {
    g_rdb: c_int,
    lna_state: u8,
    sync_update: u8,
    min_gr: c_int,
    gain_vals: GainValuesT,
}

#[repr(C)]
struct RfFreqT {
    rf_hz: f64,
    sync_update: u8,
}

#[repr(C)]
struct DcOffsetTunerT {
    dc_cal: u8,
    speed_up: u8,
    track_time: c_int,
    refresh_rate_time: c_int,
}

#[repr(C)]
struct TunerParamsT {
    bw_type: c_int,
    if_type: c_int,
    lo_mode: c_int,
    gain: GainT,
    rf_freq: RfFreqT,
    dc_offset_tuner: DcOffsetTunerT,
}

#[repr(C)]
struct DcOffsetT {
    dc_enable: u8,
    iq_enable: u8,
}

#[repr(C)]
struct DecimationT {
    enable: u8,
    decimation_factor: u8,
    wide_band_signal: u8,
}

#[repr(C)]
struct AgcT {
    enable: c_int,
    set_point_dbfs: c_int,
    attack_ms: c_ushort,
    decay_ms: c_ushort,
    decay_delay_ms: c_ushort,
    decay_threshold_db: c_ushort,
    sync_update: c_int,
}

#[repr(C)]
struct ControlParamsT {
    dc_offset: DcOffsetT,
    decimation: DecimationT,
    agc: AgcT,
    adsb_mode: c_int,
}

/// The first two fields of `sdrplay_api_RxChannelParamsT`; the per-model tuner
/// extras follow and are never touched.
#[repr(C)]
struct RxChannelParamsT {
    tuner: TunerParamsT,
    ctrl: ControlParamsT,
}

#[repr(C)]
struct FsFreqT {
    fs_hz: f64,
    sync_update: u8,
    re_cal: u8,
}

#[repr(C)]
struct SyncUpdateT {
    sample_num: c_uint,
    period: c_uint,
}

#[repr(C)]
struct ResetFlagsT {
    reset_gain_update: u8,
    reset_rf_update: u8,
    reset_fs_update: u8,
}

/// The first fields of `sdrplay_api_DevParamsT`.
#[repr(C)]
struct DevParamsT {
    ppm: f64,
    fs_freq: FsFreqT,
    sync_update: SyncUpdateT,
    reset_flags: ResetFlagsT,
    mode: c_int,
    samples_per_pkt: c_uint,
}

#[repr(C)]
struct DeviceParamsT {
    dev_params: *mut DevParamsT,
    rx_channel_a: *mut RxChannelParamsT,
    rx_channel_b: *mut RxChannelParamsT,
}

#[repr(C)]
struct StreamCbParamsT {
    first_sample_num: c_uint,
    gr_changed: c_int,
    rf_changed: c_int,
    fs_changed: c_int,
    num_samples: c_uint,
}

type StreamCb =
    unsafe extern "C" fn(*mut i16, *mut i16, *mut StreamCbParamsT, c_uint, c_uint, *mut c_void);
/// `params` is a union whose first member (for the power-overload event) is
/// the event type as an int; that is all that is read.
type EventCb = unsafe extern "C" fn(c_int, c_int, *mut c_int, *mut c_void);

#[repr(C)]
struct CallbackFnsT {
    stream_a: Option<StreamCb>,
    stream_b: Option<StreamCb>,
    event: Option<EventCb>,
}

/// The library's entry points, resolved once.
///
/// Rust note: the function pointers are only valid while `_lib` is loaded,
/// which is why they live in the same struct, shared behind an `Arc`.
struct SdrplayLib {
    path: String,
    open: unsafe extern "C" fn() -> ErrT,
    close: unsafe extern "C" fn() -> ErrT,
    api_version: unsafe extern "C" fn(*mut f32) -> ErrT,
    lock_device_api: unsafe extern "C" fn() -> ErrT,
    unlock_device_api: unsafe extern "C" fn() -> ErrT,
    get_devices: unsafe extern "C" fn(*mut DeviceT, *mut c_uint, c_uint) -> ErrT,
    select_device: unsafe extern "C" fn(*mut DeviceT) -> ErrT,
    release_device: unsafe extern "C" fn(*mut DeviceT) -> ErrT,
    get_error_string: unsafe extern "C" fn(ErrT) -> *const c_char,
    get_device_params: unsafe extern "C" fn(Handle, *mut *mut DeviceParamsT) -> ErrT,
    init: unsafe extern "C" fn(Handle, *mut CallbackFnsT, *mut c_void) -> ErrT,
    uninit: unsafe extern "C" fn(Handle) -> ErrT,
    update: unsafe extern "C" fn(Handle, c_int, c_uint, c_uint) -> ErrT,
    _lib: Library,
}

// SAFETY: plain function pointers into a loaded library, callable from any
// thread (the API serialises per device itself).
unsafe impl Send for SdrplayLib {}
unsafe impl Sync for SdrplayLib {}

impl SdrplayLib {
    fn load_file(path: &str) -> std::result::Result<Self, String> {
        // SAFETY: loading runs the library's initialisers, which SDRplay's
        // keep to the C runtime's.
        let lib = unsafe { Library::new(path) }.map_err(|e| format!("{path}: {e}"))?;
        macro_rules! func {
            ($name:literal) => {{
                // SAFETY: the field's type is the header's signature; a
                // missing symbol is an error, not undefined behaviour.
                *unsafe { lib.get(concat!($name, "\0").as_bytes()) }
                    .map_err(|_| format!("{path} has no {}", $name))?
            }};
        }
        Ok(SdrplayLib {
            path: path.to_string(),
            open: func!("sdrplay_api_Open"),
            close: func!("sdrplay_api_Close"),
            api_version: func!("sdrplay_api_ApiVersion"),
            lock_device_api: func!("sdrplay_api_LockDeviceApi"),
            unlock_device_api: func!("sdrplay_api_UnlockDeviceApi"),
            get_devices: func!("sdrplay_api_GetDevices"),
            select_device: func!("sdrplay_api_SelectDevice"),
            release_device: func!("sdrplay_api_ReleaseDevice"),
            get_error_string: func!("sdrplay_api_GetErrorString"),
            get_device_params: func!("sdrplay_api_GetDeviceParams"),
            init: func!("sdrplay_api_Init"),
            uninit: func!("sdrplay_api_Uninit"),
            update: func!("sdrplay_api_Update"),
            _lib: lib,
        })
    }

    /// Paths to try: the override only, or beside the exe, the installer's
    /// folder, then the bare name (the system's search path).
    fn candidates() -> Vec<String> {
        if let Some(p) = std::env::var_os("DECSAT_SDRPLAY_LIB") {
            return vec![p.to_string_lossy().into_owned()];
        }
        let names: &[&str] = if cfg!(windows) {
            &["sdrplay_api.dll"]
        } else if cfg!(target_os = "macos") {
            &["libsdrplay_api.dylib", "libsdrplay_api.so.3"]
        } else {
            &["libsdrplay_api.so.3", "libsdrplay_api.so"]
        };
        let mut out = Vec::new();
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(PathBuf::from));
        for name in names {
            if let Some(d) = &exe_dir {
                let p = d.join(name);
                if p.exists() {
                    out.push(p.to_string_lossy().into_owned());
                }
            }
        }
        if cfg!(windows) {
            // The installer's own folder (v3 puts the 64-bit DLL in x64).
            for var in ["ProgramFiles", "ProgramW6432"] {
                if let Some(pf) = std::env::var_os(var) {
                    let p = PathBuf::from(pf).join(r"SDRplay\API\x64\sdrplay_api.dll");
                    if p.exists() {
                        out.push(p.to_string_lossy().into_owned());
                    }
                }
            }
        }
        out.extend(names.iter().map(|n| (*n).to_string()));
        out
    }

    fn load() -> std::result::Result<Self, Vec<String>> {
        let mut why = Vec::new();
        for c in Self::candidates() {
            match Self::load_file(&c) {
                Ok(l) => return Ok(l),
                Err(e) => why.push(e),
            }
        }
        Err(why)
    }

    fn err_name(&self, code: ErrT) -> String {
        // SAFETY: returns a static string (or null for an unknown code).
        let p = unsafe { (self.get_error_string)(code) };
        if p.is_null() {
            format!("error {code}")
        } else {
            // SAFETY: a NUL-terminated static string.
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        }
    }

    fn check(&self, what: &str, code: ErrT) -> Result<()> {
        if code == 0 {
            Ok(())
        } else {
            Err(Error::Other(format!(
                "SDRplay: {what} failed: {} ({code})",
                self.err_name(code)
            )))
        }
    }
}

/// Gains: both are gain *reductions*, as in the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdrplayGains {
    /// LNA state, 0 = most gain. How many states exist depends on the model
    /// and band (RSPduo: 0–9, or 0–8 at 1–2 GHz); a value the radio does not
    /// have is brought down to its last state.
    pub lna: u8,
    /// IF gain reduction, dB: 20 (most gain) to 59.
    pub gr: u8,
}

impl Default for SdrplayGains {
    fn default() -> Self {
        // Behind an LNB there is signal to spare; start with some reduction
        // and let the overload warning say which way to go.
        SdrplayGains { lna: 4, gr: 40 }
    }
}

impl SdrplayGains {
    /// Clamp to the API's ranges.
    pub fn snapped(self) -> Self {
        SdrplayGains {
            lna: self.lna.min(27),
            gr: self.gr.clamp(NORMAL_MIN_GR as u8, MAX_BB_GR),
        }
    }
}

/// What a live session is set to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SdrplaySettings {
    pub center_hz: u64,
    /// 2–10.66 MS/s; anything in the range is taken.
    pub sample_rate: u32,
    pub gains: SdrplayGains,
    /// Use an RSPduo's Tuner B (Tuner 2) instead of Tuner A.
    pub tuner_b: bool,
}

impl Default for SdrplaySettings {
    fn default() -> Self {
        SdrplaySettings {
            center_hz: 1_200_000_000,
            sample_rate: 8_000_000,
            gains: SdrplayGains::default(),
            tuner_b: false,
        }
    }
}

/// The radio the session opened.
#[derive(Debug, Clone, Default)]
pub struct SdrplayInfo {
    pub serial: String,
    pub model: String,
    pub hw_ver: u8,
    pub api_version: f32,
    pub library: String,
    /// 'A' or 'B' (RSPduo tuner).
    pub tuner: char,
    pub is_rspduo: bool,
}

fn model_name(hw: u8) -> &'static str {
    match hw {
        1 => "RSP1",
        255 => "RSP1A",
        2 => "RSP2",
        RSPDUO_ID => "RSPduo",
        4 => "RSPdx",
        6 => "RSP1B",
        7 => "RSPdx-R2",
        _ => "RSP",
    }
}

/// The analogue bandwidth that fits a zero-IF rate (`sdrplay_api_Bw_MHzT`
/// values are the bandwidth in kHz).
fn bandwidth_for(rate: f64) -> c_int {
    match rate {
        r if r >= 8.0e6 => 8000,
        r if r >= 7.0e6 => 7000,
        r if r >= 6.0e6 => 6000,
        r if r >= 5.0e6 => 5000,
        _ => 1536,
    }
}

/// Highest LNA state worth sending. The RSPduo's counts are the header's
/// (`RSPDUO_NUM_LNA_STATES` 10, `_LBAND` 9); other models are left to the API,
/// which reports a state it does not have.
fn max_lna(info: &SdrplayInfo, hz: u64) -> u8 {
    if info.is_rspduo {
        if hz >= 1_000_000_000 { 8 } else { 9 }
    } else {
        27
    }
}

fn cstr(b: &[c_char]) -> String {
    let bytes: Vec<u8> = b
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

fn check_hz(hz: u64) -> Result<()> {
    if (SDRPLAY_MIN_HZ..=SDRPLAY_MAX_HZ).contains(&hz) {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "SDRplay: {:.3} MHz is outside the 1 kHz–2 GHz tuning range",
            hz as f64 / 1e6
        )))
    }
}

/// What the API's callbacks need: where to send, and what to count.
struct CbCtx {
    tx: SyncSender<Vec<Iq>>,
    overflows: Arc<AtomicU64>,
    overloads: Arc<AtomicU64>,
    /// Set when the radio was unplugged or failed, so `read` can say so.
    lost: Arc<AtomicBool>,
    /// Emptied buffers coming back from the consumer, to avoid allocating.
    spare: Receiver<Vec<Iq>>,
}

unsafe extern "C" fn on_stream(
    xi: *mut i16,
    xq: *mut i16,
    _params: *mut StreamCbParamsT,
    num: c_uint,
    _reset: c_uint,
    user: *mut c_void,
) {
    // A panic must not unwind into C.
    let _ = std::panic::catch_unwind(|| {
        if xi.is_null() || xq.is_null() || num == 0 || user.is_null() {
            return;
        }
        // SAFETY: `user` is the `CbCtx` the device keeps alive until after
        // `Uninit` has returned; the API hands `num` samples in each array,
        // valid for the duration of the call.
        let ctx = unsafe { &*(user as *const CbCtx) };
        let (i, q) = unsafe {
            (
                std::slice::from_raw_parts(xi, num as usize),
                std::slice::from_raw_parts(xq, num as usize),
            )
        };
        let mut v = ctx.spare.try_recv().unwrap_or_default();
        v.clear();
        v.extend(
            i.iter()
                .zip(q)
                .map(|(&i, &q)| Iq::new(f32::from(i) * SCALE, f32::from(q) * SCALE)),
        );
        match ctx.tx.try_send(v) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                ctx.overflows.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    });
}

unsafe extern "C" fn on_event(event: c_int, _tuner: c_int, params: *mut c_int, user: *mut c_void) {
    let _ = std::panic::catch_unwind(|| {
        if user.is_null() {
            return;
        }
        // SAFETY: as in `on_stream`.
        let ctx = unsafe { &*(user as *const CbCtx) };
        match event {
            EVENT_POWER_OVERLOAD => {
                // SAFETY: for this event the union's first member is the
                // overload type (an int).
                if !params.is_null() && unsafe { *params } == OVERLOAD_DETECTED {
                    ctx.overloads.fetch_add(1, Ordering::Relaxed);
                }
            }
            EVENT_DEVICE_REMOVED | EVENT_DEVICE_FAILURE => {
                ctx.lost.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    });
}

/// An opened, selected and streaming radio; released when the last handle goes.
struct Device {
    lib: Arc<SdrplayLib>,
    /// The API's record for the device (its `dev` handle is what calls take).
    dev: Mutex<DeviceT>,
    /// Points into API-owned memory that lives until `ReleaseDevice`.
    params: *mut DeviceParamsT,
    tuner_b: bool,
    /// Stream has stopped (`Uninit` done); control calls then fail quietly.
    stopped: AtomicBool,
    /// The callback context. Boxed so its address is stable; dropped only
    /// after `Uninit` guarantees no callback is running.
    ctx: Box<CbCtx>,
}

// SAFETY: the API guards its own device state; our control calls take the
// `dev` lock, and the raw pointers are the API's, valid while selected.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    fn handle(&self) -> Handle {
        self.dev.lock().unwrap().dev
    }

    fn tuner_sel(&self) -> c_int {
        if self.tuner_b { TUNER_B } else { TUNER_A }
    }

    fn rx(&self) -> *mut RxChannelParamsT {
        // SAFETY: `params` was returned by GetDeviceParams and stays valid
        // while the device is selected.
        unsafe {
            if self.tuner_b {
                (*self.params).rx_channel_b
            } else {
                (*self.params).rx_channel_a
            }
        }
    }

    /// Stop streaming (idempotent).
    fn stop(&self) {
        if !self.stopped.swap(true, Ordering::SeqCst) {
            let h = self.handle();
            // SAFETY: a selected, initialised device.
            let code = unsafe { (self.lib.uninit)(h) };
            if code != 0 {
                tracing::warn!("SDRplay: Uninit: {}", self.lib.err_name(code));
            }
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.stop();
        let lib = &self.lib;
        // SAFETY: streaming has stopped, so nothing else uses the device. The
        // lock keeps another program's select/release from interleaving.
        unsafe {
            (lib.lock_device_api)();
            let mut d = *self.dev.lock().unwrap();
            (lib.release_device)(&mut d);
            (lib.unlock_device_api)();
            (lib.close)();
        }
    }
}

/// Live controls on a running radio, usable from the GUI thread while the
/// engine reads samples.
#[derive(Clone)]
pub struct SdrplayControl {
    dev: Arc<Device>,
    settings: Arc<Mutex<SdrplaySettings>>,
    overflows: Arc<AtomicU64>,
    overloads: Arc<AtomicU64>,
    info: Arc<SdrplayInfo>,
}

impl SdrplayControl {
    pub fn settings(&self) -> SdrplaySettings {
        *self.settings.lock().unwrap()
    }

    pub fn info(&self) -> &SdrplayInfo {
        &self.info
    }

    fn update(&self, what: &str, reason: c_uint) -> Result<()> {
        if self.dev.stopped.load(Ordering::SeqCst) {
            return Err(Error::Other("SDRplay: the stream has stopped".into()));
        }
        let lib = &self.dev.lib;
        // SAFETY: a selected, streaming device.
        let code = unsafe { (lib.update)(self.dev.handle(), self.dev.tuner_sel(), reason, 0) };
        lib.check(what, code)
    }

    /// Retune (1 kHz–2 GHz).
    pub fn set_center(&self, hz: u64) -> Result<()> {
        check_hz(hz)?;
        let mut s = self.settings.lock().unwrap();
        // SAFETY: the tuner's parameter block, owned by the API.
        unsafe { (*self.dev.rx()).tuner.rf_freq.rf_hz = hz as f64 };
        self.update("set frequency", UPDATE_TUNER_FRF)?;
        s.center_hz = hz;
        // The LNA state count depends on the band: reapply the gains.
        drop(s);
        let g = self.settings().gains;
        self.set_gains(g)
    }

    pub fn set_gains(&self, gains: SdrplayGains) -> Result<()> {
        let g = gains.snapped();
        let lna = g.lna.min(max_lna(&self.info, self.settings().center_hz));
        // SAFETY: as above.
        unsafe {
            let t = &mut (*self.dev.rx()).tuner.gain;
            t.g_rdb = c_int::from(g.gr);
            t.lna_state = lna;
        }
        self.update("set gain", UPDATE_TUNER_GR)?;
        self.settings.lock().unwrap().gains = SdrplayGains { lna, gr: g.gr };
        Ok(())
    }

    /// Buffers dropped because the consumer fell behind.
    pub fn overflows(&self) -> u64 {
        self.overflows.load(Ordering::Relaxed)
    }

    /// ADC overload events the radio has reported (raise the gain reduction).
    pub fn overloads(&self) -> u64 {
        self.overloads.load(Ordering::Relaxed)
    }
}

/// A live SDRplay receive stream.
pub struct SdrplaySource {
    rx: Receiver<Vec<Iq>>,
    spare: SyncSender<Vec<Iq>>,
    control: SdrplayControl,
    lost: Arc<AtomicBool>,
    sample_rate: f64,
}

impl SdrplaySource {
    /// Whether the SDRplay library can be loaded at all (for greying out the
    /// choice in a menu), and from where.
    pub fn library_available() -> Option<String> {
        SdrplayLib::load().ok().map(|l| l.path.clone())
    }

    /// Open the first RSP (an RSPduo if there is one), configure it and start
    /// receiving.
    pub fn open(settings: SdrplaySettings) -> Result<Self> {
        check_hz(settings.center_hz)?;
        let rate = f64::from(settings.sample_rate);
        if !(FS_MIN..=FS_MAX).contains(&rate) {
            return Err(Error::Other(format!(
                "SDRplay: {:.3} MS/s is outside 2–10.667 MS/s",
                rate / 1e6
            )));
        }
        let lib = SdrplayLib::load().map_err(|why| {
            Error::Other(format!(
                "SDRplay: the API library was not found. Install the SDRplay API (v3, from \
                 sdrplay.com), or put sdrplay_api.dll beside DecSAT, or set DECSAT_SDRPLAY_LIB \
                 to its path. ({})",
                why.join("; ")
            ))
        })?;
        let lib = Arc::new(lib);

        // SAFETY: Open connects to the API service; Close undoes it, and is
        // called on every path out of here (directly, or by `Device::drop`).
        lib.check(
            "open the API (is the SDRplay API Service running?)",
            unsafe { (lib.open)() },
        )?;
        let fail = |e: Error| {
            // SAFETY: balances the Open above.
            unsafe { (lib.close)() };
            e
        };

        let mut ver = 0.0f32;
        // SAFETY: one float out.
        lib.check("API version", unsafe { (lib.api_version)(&mut ver) })
            .map_err(fail)?;
        if !(3.07..4.0).contains(&ver) {
            return Err(fail(Error::Other(format!(
                "SDRplay: API version {ver} is not a v3 API (DecSAT is written for 3.15)"
            ))));
        }
        if (ver - 3.15).abs() > 0.001 {
            tracing::warn!("SDRplay API {ver}: DecSAT is written against 3.15");
        }

        // Pick a device. The lock keeps another program's selection out.
        // SAFETY: the array has room for `max`.
        unsafe { (lib.lock_device_api)() };
        let mut devs: [DeviceT; 16] = unsafe { std::mem::zeroed() };
        let mut n: c_uint = 0;
        let code = unsafe { (lib.get_devices)(devs.as_mut_ptr(), &mut n, 16) };
        if let Err(e) = lib.check("list devices", code) {
            unsafe { (lib.unlock_device_api)() };
            return Err(fail(e));
        }
        let devs = &devs[..(n as usize).min(16)];
        let Some(pick) = devs
            .iter()
            .find(|d| d.hw_ver == RSPDUO_ID)
            .or_else(|| devs.first())
        else {
            unsafe { (lib.unlock_device_api)() };
            return Err(fail(Error::Other(
                "no SDRplay RSP found. Is it plugged in, and is the SDRplay API Service \
                 running (and not in use by SDRuno or another program)?"
                    .into(),
            )));
        };
        let mut chosen = *pick;
        let is_duo = chosen.hw_ver == RSPDUO_ID;
        chosen.tuner = if settings.tuner_b && is_duo {
            TUNER_B
        } else {
            TUNER_A
        };
        chosen.rsp_duo_mode = MODE_SINGLE_TUNER;
        chosen.rsp_duo_sample_freq = 0.0;
        // SAFETY: a record from GetDevices with the tuner and mode set.
        let code = unsafe { (lib.select_device)(&mut chosen) };
        unsafe { (lib.unlock_device_api)() };
        lib.check("select the device (in use by another program?)", code)
            .map_err(fail)?;

        let tuner_b = chosen.tuner == TUNER_B;
        let info = SdrplayInfo {
            serial: cstr(&chosen.ser_no),
            model: model_name(chosen.hw_ver).to_string(),
            hw_ver: chosen.hw_ver,
            api_version: ver,
            library: lib.path.clone(),
            tuner: if tuner_b { 'B' } else { 'A' },
            is_rspduo: is_duo,
        };

        let mut params: *mut DeviceParamsT = std::ptr::null_mut();
        // SAFETY: the selected device's handle; the API returns its own block.
        let code = unsafe { (lib.get_device_params)(chosen.dev, &mut params) };
        let release = |lib: &SdrplayLib, chosen: &mut DeviceT| {
            // SAFETY: undoes the selection and the Open.
            unsafe {
                (lib.lock_device_api)();
                (lib.release_device)(chosen);
                (lib.unlock_device_api)();
                (lib.close)();
            }
        };
        if code != 0 || params.is_null() {
            let e = lib.check("device parameters", if code != 0 { code } else { 1 });
            release(&lib, &mut chosen);
            return Err(e.unwrap_err());
        }

        // Configure before Init: zero-IF at the chosen rate, AGC off.
        // SAFETY: the API's parameter blocks, valid while selected; for a
        // single-tuner session only the chosen channel is used.
        let gains = settings.gains.snapped();
        let lna = gains.lna.min(max_lna(&info, settings.center_hz));
        unsafe {
            let dp = &mut *(*params).dev_params;
            dp.fs_freq.fs_hz = rate;
            let rx = &mut *(if tuner_b {
                (*params).rx_channel_b
            } else {
                (*params).rx_channel_a
            });
            rx.tuner.bw_type = bandwidth_for(rate);
            rx.tuner.if_type = IF_ZERO;
            rx.tuner.lo_mode = LO_AUTO;
            rx.tuner.rf_freq.rf_hz = settings.center_hz as f64;
            rx.tuner.gain.g_rdb = c_int::from(gains.gr);
            rx.tuner.gain.lna_state = lna;
            rx.tuner.gain.min_gr = NORMAL_MIN_GR;
            rx.ctrl.agc.enable = AGC_DISABLE;
        }

        let (tx, rx) = mpsc::sync_channel::<Vec<Iq>>(QUEUE);
        let (spare_tx, spare_rx) = mpsc::sync_channel::<Vec<Iq>>(8);
        let overflows = Arc::new(AtomicU64::new(0));
        let overloads = Arc::new(AtomicU64::new(0));
        let lost = Arc::new(AtomicBool::new(false));
        let ctx = Box::new(CbCtx {
            tx,
            overflows: Arc::clone(&overflows),
            overloads: Arc::clone(&overloads),
            lost: Arc::clone(&lost),
            spare: spare_rx,
        });
        let user = &*ctx as *const CbCtx as *mut c_void;
        let mut cbs = CallbackFnsT {
            stream_a: Some(on_stream),
            stream_b: Some(on_stream),
            event: Some(on_event),
        };
        // Init starts the stream; the callbacks run on the API's thread. The
        // context outlives them: it is dropped only after Uninit.
        // SAFETY: valid handle, callbacks and context.
        let code = unsafe { (lib.init)(chosen.dev, &mut cbs, user) };
        if let Err(e) = lib.check("start streaming", code) {
            release(&lib, &mut chosen);
            return Err(e);
        }
        // The API copies the callback table; the context stays boxed.
        let device = Arc::new(Device {
            lib: Arc::clone(&lib),
            dev: Mutex::new(chosen),
            params,
            tuner_b,
            stopped: AtomicBool::new(false),
            ctx,
        });
        tracing::info!(
            "SDRplay {} {} (hw {}) tuner {} via {} API {}",
            info.model,
            info.serial,
            info.hw_ver,
            info.tuner,
            info.library,
            info.api_version
        );
        let _ = &device.ctx; // held for the stream's lifetime

        Ok(SdrplaySource {
            rx,
            spare: spare_tx,
            lost,
            control: SdrplayControl {
                dev: device,
                settings: Arc::new(Mutex::new(SdrplaySettings {
                    gains: SdrplayGains { lna, gr: gains.gr },
                    ..settings
                })),
                overflows,
                overloads,
                info: Arc::new(info),
            },
            sample_rate: rate,
        })
    }

    /// A handle for retuning and gain changes while the engine reads.
    pub fn control(&self) -> SdrplayControl {
        self.control.clone()
    }
}

impl IqSource for SdrplaySource {
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
                if self.lost.load(Ordering::Relaxed) {
                    Err(Error::Other("SDRplay was removed or failed".into()))
                } else {
                    Err(Error::Other("SDRplay stopped delivering samples".into()))
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(Error::Other("SDRplay stream stopped (unplugged?)".into()))
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
        let i = &self.control.info;
        format!(
            "SDRplay {} {} tuner {} @ {:.4} MHz, {:.2} MS/s, LNA state {} IF GR {} dB",
            i.model,
            i.serial,
            i.tuner,
            s.center_hz as f64 / 1e6,
            s.sample_rate as f64 / 1e6,
            s.gains.lna,
            s.gains.gr
        )
    }

    fn is_live(&self) -> bool {
        true
    }
}

impl Drop for SdrplaySource {
    fn drop(&mut self) {
        // Stop the stream now, even if a control handle keeps the device
        // alive; the radio is released when the last handle goes.
        self.control.dev.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gains_clamp_to_api_ranges() {
        let g = SdrplayGains { lna: 99, gr: 5 }.snapped();
        assert_eq!((g.lna, g.gr), (27, 20));
        let g = SdrplayGains { lna: 0, gr: 200 }.snapped();
        assert_eq!(g.gr, 59);
    }

    #[test]
    fn bandwidth_fits_the_rate() {
        assert_eq!(bandwidth_for(2.0e6), 1536);
        assert_eq!(bandwidth_for(4.0e6), 1536);
        assert_eq!(bandwidth_for(6.0e6), 6000);
        assert_eq!(bandwidth_for(8.0e6), 8000);
        assert_eq!(bandwidth_for(10.0e6), 8000);
    }

    #[test]
    fn rspduo_lna_states_follow_the_band() {
        let info = SdrplayInfo {
            is_rspduo: true,
            ..Default::default()
        };
        assert_eq!(max_lna(&info, 1_450_000_000), 8);
        assert_eq!(max_lna(&info, 500_000_000), 9);
    }

    #[test]
    fn rejects_bad_tuning_before_loading_anything() {
        for hz in [0u64, 3_000_000_000] {
            let s = SdrplaySettings {
                center_hz: hz,
                ..Default::default()
            };
            assert!(SdrplaySource::open(s).is_err());
        }
        let s = SdrplaySettings {
            sample_rate: 20_000_000,
            ..Default::default()
        };
        assert!(SdrplaySource::open(s).is_err());
    }

    /// The structs must match the C header's layout (MSVC x64): spot-check
    /// the sizes and the offsets DecSAT writes through.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn struct_layouts_match_the_header() {
        use std::mem::{offset_of, size_of};
        assert_eq!(size_of::<DeviceT>(), 96);
        assert_eq!(offset_of!(DeviceT, tuner), 68);
        assert_eq!(offset_of!(DeviceT, rsp_duo_sample_freq), 80);
        assert_eq!(offset_of!(DeviceT, dev), 88);
        assert_eq!(size_of::<GainT>(), 24);
        assert_eq!(size_of::<TunerParamsT>(), 72);
        assert_eq!(offset_of!(DevParamsT, fs_freq), 8);
        assert_eq!(offset_of!(DevParamsT, mode), 36);
        assert_eq!(size_of::<CallbackFnsT>(), 24);
    }

    /// Against a real RSPduo: `cargo test -p decsat-io --features sdrplay
    /// live_sdrplay -- --ignored --nocapture`. Receive only.
    #[test]
    #[ignore = "needs an SDRplay RSP and the SDRplay API installed"]
    fn live_sdrplay() {
        let mut src = SdrplaySource::open(SdrplaySettings::default()).expect("open");
        let c = src.control();
        eprintln!("{:?}", c.info());
        let mut buf = Vec::new();
        let t0 = std::time::Instant::now();
        let mut n = 0usize;
        while t0.elapsed() < Duration::from_secs(1) {
            n += src.read(&mut buf).expect("read");
        }
        c.set_center(1_300_000_000).expect("retune");
        c.set_gains(SdrplayGains { lna: 2, gr: 35 }).expect("gains");
        for _ in 0..10 {
            n += src.read(&mut buf).expect("read after retune");
        }
        let rate = n as f64 / t0.elapsed().as_secs_f64();
        let rms = (buf.iter().map(|s| s.norm_sqr()).sum::<f32>() / buf.len() as f32).sqrt();
        eprintln!(
            "{} — {:.2} MS/s delivered, rms {rms:.4}, dropped {}, overloads {}",
            src.describe(),
            rate / 1e6,
            c.overflows(),
            c.overloads()
        );
        assert!(rate > 6.0e6, "only {:.2} MS/s", rate / 1e6);
        drop(src);
        drop(c);
        drop(SdrplaySource::open(SdrplaySettings::default()).expect("reopen"));
    }
}
