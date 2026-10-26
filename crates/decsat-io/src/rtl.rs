//! RTL-SDR (RTL2832U) receive-only source, over `librtlsdr` on Windows.
//!
//! The library is not linked: `rtlsdr.dll` is loaded at run time from an `RTL`
//! folder beside the executable, so the portable exe starts (and the build
//! needs no import library) on a machine that has no RTL-SDR runtime at all.
//! The DLL's own dependency (a matching `libusb-1.0.dll`) is found in that same
//! folder. Nothing here installs or replaces a USB driver.
//!
//! Provenance: merged in from the "VMA" snapshot, see `docs/STATUS.md`
//! (2026-10-25). Not tested against real hardware by whoever restyled it.

use std::ffi::{CString, c_int, c_void};
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::{Arc, Mutex};

use decsat_core::{Error, Iq, Result};

use crate::IqSource;

// Win32 loader calls (kernel32), declared by hand so no crate is needed.
//
// Rust note: since edition 2024 an `extern` block must be written
// `unsafe extern`, because the compiler cannot check that the declared
// signatures match the real functions. `"system"` is the platform's system
// calling convention (stdcall on 32-bit Windows, the usual one on 64-bit).
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    fn FreeLibrary(module: *mut c_void) -> i32;
}

/// `LoadLibraryExW` flag (libloaderapi.h): look for the DLL's own dependencies
/// in the directory the DLL was loaded from, so `libusb-1.0.dll` is found next
/// to `rtlsdr.dll` without touching the process-wide `PATH`.
const LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR: u32 = 0x0000_0100;
/// `LoadLibraryExW` flag (libloaderapi.h): keep the standard safe locations
/// (application directory, System32) in the search as well.
const LOAD_LIBRARY_SEARCH_DEFAULT_DIRS: u32 = 0x0000_1000;

// Tuner chip codes returned by `rtlsdr_get_tuner_type()`: `enum rtlsdr_tuner`
// in librtlsdr's `rtl-sdr.h` (0 is `RTLSDR_TUNER_UNKNOWN`).
const TUNER_E4000: c_int = 1;
const TUNER_FC0012: c_int = 2;
const TUNER_FC0013: c_int = 3;
const TUNER_FC2580: c_int = 4;
const TUNER_R820T: c_int = 5;
const TUNER_R828D: c_int = 6;

/// Upper bound accepted for the number of gain steps librtlsdr reports (real
/// tuners have fewer than 30); anything else is treated as "no gain table".
const MAX_TUNER_GAINS: c_int = 128;

/// Sample-rate range accepted here: 225 001 S/s to 3.2 MS/s. librtlsdr itself
/// additionally rejects 300 001–900 000 S/s, and that failure is reported by
/// `configure`.
const MIN_SAMPLE_RATE: u32 = 225_001;
const MAX_SAMPLE_RATE: u32 = 3_200_000;

/// Bytes asked of `rtlsdr_read_sync` per call: 16 384 IQ pairs (2 bytes each).
const READ_BYTES: usize = 32768;

/// The 8-bit ADC is offset-binary: 0 and 255 are the extremes, so the zero
/// point is 127.5. Dividing by 128 scales the result to about ±1.
const ADC_MIDPOINT: f32 = 127.5;
const ADC_SCALE: f32 = 128.0;

/// Resets of the USB buffer tried after a failed read before the stream is declared dead.
const MAX_USB_RECOVERIES: u32 = 3;

// Function signatures from `rtl-sdr.h`. `rtlsdr_dev_t *` is an opaque handle,
// carried here as `*mut c_void`. All return 0 on success unless noted.
type Open = unsafe extern "C" fn(*mut *mut c_void, u32) -> c_int;
type Close = unsafe extern "C" fn(*mut c_void) -> c_int;
/// `rtlsdr_set_center_freq` and `rtlsdr_set_sample_rate`: a `uint32_t` in Hz.
type SetU32 = unsafe extern "C" fn(*mut c_void, u32) -> c_int;
/// `rtlsdr_set_tuner_gain_mode(dev, manual)`: 1 = manual, 0 = automatic.
type SetGainMode = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
/// `rtlsdr_set_tuner_gain(dev, gain)`: gain in tenths of a dB.
type SetGain = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type Reset = unsafe extern "C" fn(*mut c_void) -> c_int;
/// Returns one of the `TUNER_*` codes above.
type GetTunerType = unsafe extern "C" fn(*mut c_void) -> c_int;
/// `rtlsdr_get_tuner_gains(dev, gains)`: with a null `gains` returns only the
/// number of steps, otherwise fills the array (tenths of a dB) and returns the
/// number written; negative on error.
type GetTunerGains = unsafe extern "C" fn(*mut c_void, *mut c_int) -> c_int;
/// `rtlsdr_read_sync(dev, buf, len, &n_read)`.
type ReadSync = unsafe extern "C" fn(*mut c_void, *mut c_void, c_int, *mut c_int) -> c_int;

/// The librtlsdr entry points DecSAT uses, resolved once from the loaded DLL.
struct Api {
    module: *mut c_void,
    open: Open,
    close: Close,
    center: SetU32,
    rate: SetU32,
    reset: Reset,
    gain_mode: SetGainMode,
    gain: SetGain,
    get_tuner_type: GetTunerType,
    get_tuner_gains: GetTunerGains,
    read: ReadSync,
}

// SAFETY: `Api` is a module handle plus plain function pointers. They carry no
// thread affinity (a loaded module is process-wide), so moving them to the
// engine's source thread is fine.
unsafe impl Send for Api {}

impl Drop for Api {
    fn drop(&mut self) {
        // SAFETY: `module` came from a successful `LoadLibraryExW` and is freed
        // exactly once, here.
        unsafe {
            FreeLibrary(self.module);
        }
    }
}

/// Look up the exported function `name` and reinterpret it as the function
/// pointer type `T`.
///
/// # Safety
///
/// `module` must be a live module handle, and `T` must be the real signature of
/// the export (the compiler cannot check that).
unsafe fn symbol<T: Copy>(module: *mut c_void, name: &str) -> Result<T> {
    let c_name = CString::new(name).unwrap();
    // SAFETY: `module` is valid (caller), `c_name` is NUL-terminated.
    let address = unsafe { GetProcAddress(module, c_name.as_ptr().cast::<u8>()) };
    if address.is_null() {
        return Err(Error::Other(format!(
            "RTL-SDR: missing {name} in rtlsdr.dll"
        )));
    }
    // Rust note: `transmute_copy` reads the pointer's bytes as a `T`. It is the
    // usual way to turn a raw `GetProcAddress` result into a typed `extern "C"`
    // function pointer, which `as` casts cannot do generically.
    //
    // SAFETY: `T` is a function-pointer type (pointer-sized, caller's contract).
    Ok(unsafe { std::mem::transmute_copy(&address) })
}

impl Api {
    /// Load `RTL\rtlsdr.dll` from beside the executable and resolve every export.
    fn load() -> Result<Self> {
        // Resolve relative to the executable, never the current directory, so a
        // stray DLL in the working directory cannot be picked up instead.
        let path = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("RTL").join("rtlsdr.dll")));
        let Some(path) = path else {
            return Err(Error::Other(
                "RTL-SDR: cannot locate executable directory".into(),
            ));
        };
        // Win32 wants a NUL-terminated UTF-16 string.
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let module = unsafe {
            LoadLibraryExW(
                wide.as_ptr(),
                ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        };
        if module.is_null() {
            return Err(Error::Other(
                "RTL-SDR: RTL\\rtlsdr.dll not found or dependencies unavailable. \
                 Put rtlsdr.dll and its matching libusb-1.0.dll into bin\\x64\\Release\\RTL\\."
                    .into(),
            ));
        }
        // SAFETY: `module` is a live handle, and `resolve` pairs each name with
        // its header signature.
        let api = unsafe { Self::resolve(module) };
        if api.is_err() {
            // A missing export: nothing owns the module yet, so release it here.
            // SAFETY: `module` is live and not freed elsewhere on this path.
            unsafe {
                FreeLibrary(module);
            }
        }
        api
    }

    /// Resolve every export. On success the returned `Api` owns `module`.
    ///
    /// # Safety
    ///
    /// `module` must be a live handle to a librtlsdr DLL.
    unsafe fn resolve(module: *mut c_void) -> Result<Self> {
        // SAFETY: the type of each field is the signature of the export it is
        // filled from (see the `type` aliases above).
        unsafe {
            Ok(Self {
                module,
                open: symbol(module, "rtlsdr_open")?,
                close: symbol(module, "rtlsdr_close")?,
                center: symbol(module, "rtlsdr_set_center_freq")?,
                rate: symbol(module, "rtlsdr_set_sample_rate")?,
                reset: symbol(module, "rtlsdr_reset_buffer")?,
                gain_mode: symbol(module, "rtlsdr_set_tuner_gain_mode")?,
                gain: symbol(module, "rtlsdr_set_tuner_gain")?,
                get_tuner_type: symbol(module, "rtlsdr_get_tuner_type")?,
                get_tuner_gains: symbol(module, "rtlsdr_get_tuner_gains")?,
                read: symbol(module, "rtlsdr_read_sync")?,
            })
        }
    }
}

/// What the tuner chip is, the range it can tune and the gain steps it offers.
#[derive(Clone, Debug)]
pub struct RtlTunerInfo {
    pub name: &'static str,
    pub min_hz: u32,
    pub max_hz: u32,
    /// Values returned by librtlsdr, in tenths of a dB.
    pub gains_tenths: Vec<i32>,
}

impl RtlTunerInfo {
    /// Ask an open device which tuner it has.
    fn from_device(api: &Api, dev: *mut c_void) -> Self {
        // SAFETY (every call in this function): `dev` is an open device handle
        // from `rtlsdr_open`, and the arrays passed are sized as librtlsdr expects.
        //
        // The ranges are the tuner chips' tuning limits as librtlsdr documents them.
        let (name, min_hz, max_hz) = match unsafe { (api.get_tuner_type)(dev) } {
            TUNER_E4000 => ("E4000", 52_000_000, 2_200_000_000),
            TUNER_FC0012 => ("FC0012", 22_000_000, 948_600_000),
            TUNER_FC0013 => ("FC0013", 22_000_000, 1_100_000_000),
            TUNER_FC2580 => ("FC2580", 146_000_000, 924_000_000),
            TUNER_R820T => ("R820T", 24_000_000, 1_766_000_000),
            TUNER_R828D => ("R828D", 24_000_000, 1_766_000_000),
            _ => ("Unknown", 24_000_000, 2_200_000_000),
        };
        // Two-step call: a null array asks for the count, then the array is
        // sized to fit and filled.
        let count = unsafe { (api.get_tuner_gains)(dev, ptr::null_mut()) };
        let mut gains_tenths = Vec::new();
        if (1..=MAX_TUNER_GAINS).contains(&count) {
            gains_tenths.resize(count as usize, 0);
            let actual = unsafe { (api.get_tuner_gains)(dev, gains_tenths.as_mut_ptr()) };
            if actual < 0 {
                gains_tenths.clear()
            } else {
                gains_tenths.truncate((actual as usize).min(count as usize));
            }
            gains_tenths.sort_unstable();
            gains_tenths.dedup();
        }
        Self {
            name,
            min_hz,
            max_hz,
            gains_tenths,
        }
    }

    /// Whether `hz` is inside the tuner's range.
    pub fn supports(&self, hz: f64) -> bool {
        hz >= self.min_hz as f64 && hz <= self.max_hz as f64
    }

    /// The gain step closest to `gain` (tenths of a dB); `gain` itself if the
    /// tuner reported no table.
    pub fn nearest_gain(&self, gain: i32) -> i32 {
        self.gains_tenths
            .iter()
            .copied()
            .min_by_key(|g| (i64::from(*g) - i64::from(gain)).abs())
            .unwrap_or(gain)
    }
}

/// What a live session is set to.
#[derive(Clone, Debug)]
pub struct RtlSettings {
    /// librtlsdr device index (0 = the first dongle).
    pub device_index: u32,
    pub center_hz: u32,
    /// LNB local-oscillator frequency in Hz. The GUI adds it to `center_hz` to
    /// show the satellite-band frequency; the driver never sees it.
    pub lnb_lo_hz: f64,
    pub rate: u32,
    /// Manual gain in tenths of a dB (snapped to the tuner's nearest step).
    pub gain_tenths: i32,
    /// Manual gain on, or the tuner's automatic gain.
    pub manual_gain: bool,
}

impl Default for RtlSettings {
    fn default() -> Self {
        Self {
            device_index: 0,
            center_hz: 1_100_000_000,
            lnb_lo_hz: 0.0,
            rate: 2_400_000,
            gain_tenths: 200,
            manual_gain: false,
        }
    }
}

/// Changes requested from another thread, applied by the source thread at the
/// start of its next read.
#[derive(Default)]
struct PendingControl {
    frequency: Option<u32>,
    /// `(manual, tenths of a dB)`.
    gain: Option<(bool, i32)>,
    /// Published once at open, so the GUI can offer the right gain steps.
    tuner: Option<RtlTunerInfo>,
}

/// Live controls on a running RTL-SDR, usable from the GUI thread while the
/// engine reads samples.
///
/// Rust note: the `Arc<Mutex<..>>` is shared between this handle and the
/// source. A poisoned lock (a thread panicked holding it) is simply ignored
/// here, since losing one tune request is harmless.
#[derive(Clone)]
pub struct RtlControl(Arc<Mutex<PendingControl>>);

impl RtlControl {
    pub fn tuner_info(&self) -> Option<RtlTunerInfo> {
        self.0.lock().ok().and_then(|p| p.tuner.clone())
    }

    /// Retune; applied before the next block is read.
    pub fn tune(&self, hz: u32) {
        if let Ok(mut p) = self.0.lock() {
            p.frequency = Some(hz);
        }
    }

    /// Set the gain mode and, if `manual`, the gain in tenths of a dB.
    pub fn gain(&self, manual: bool, tenths: i32) {
        if let Ok(mut p) = self.0.lock() {
            p.gain = Some((manual, tenths));
        }
    }
}

/// A live RTL-SDR receive stream.
pub struct RtlSource {
    api: Api,
    /// Open `rtlsdr_dev_t *`; closed in `Drop`.
    dev: *mut c_void,
    settings: RtlSettings,
    /// Raw 8-bit I/Q bytes from the last read.
    bytes: Vec<u8>,
    control: RtlControl,
    tuner: RtlTunerInfo,
    /// Failed reads recovered by a buffer reset, over the whole session.
    usb_errors: u32,
}

// SAFETY: `dev` is created, used and closed only through this struct, which the
// engine moves to its single source thread; librtlsdr is never called from two
// threads at once.
unsafe impl Send for RtlSource {}

impl RtlSource {
    pub fn control(&self) -> RtlControl {
        self.control.clone()
    }

    /// Open the device and apply `s`.
    pub fn open(s: RtlSettings) -> Result<Self> {
        if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&s.rate) {
            return Err(Error::Other(
                "RTL-SDR: sample rate must be 0.225–3.2 MS/s; use <=2.4 MS/s for reliable operation"
                    .into(),
            ));
        }
        let api = Api::load()?;
        let mut dev = ptr::null_mut();
        // SAFETY: `dev` is a valid out-pointer for the handle.
        let status = unsafe { (api.open)(&mut dev, s.device_index) };
        if status != 0 || dev.is_null() {
            return Err(Error::Other(format!(
                "RTL-SDR: cannot open device {} (code {status}). \
                 Check WinUSB driver and that no other SDR app uses it.",
                s.device_index
            )));
        }
        let tuner = RtlTunerInfo::from_device(&api, dev);
        if !tuner.supports(s.center_hz as f64) {
            // SAFETY: `dev` is open and closed once, on this error path.
            unsafe { (api.close)(dev) };
            return Err(Error::Other(format!(
                "RTL-SDR {} supports {:.0}–{:.0} MHz (requested {:.3} MHz)",
                tuner.name,
                tuner.min_hz as f64 / 1e6,
                tuner.max_hz as f64 / 1e6,
                s.center_hz as f64 / 1e6
            )));
        }
        if let Err(e) = Self::configure(&api, dev, &s, &tuner) {
            // SAFETY: as above.
            unsafe { (api.close)(dev) };
            return Err(e);
        }
        Ok(Self {
            api,
            dev,
            settings: s,
            bytes: vec![0; READ_BYTES],
            control: RtlControl(Arc::new(Mutex::new(PendingControl {
                tuner: Some(tuner.clone()),
                ..PendingControl::default()
            }))),
            tuner,
            usb_errors: 0,
        })
    }

    /// Sample rate, frequency, gain, then clear the USB buffer so streaming
    /// starts clean.
    fn configure(api: &Api, dev: *mut c_void, s: &RtlSettings, tuner: &RtlTunerInfo) -> Result<()> {
        // SAFETY (every call in this function): `dev` is an open device handle.
        //
        // All three setters run before any result is looked at, as before.
        let checks = [
            ("sample rate", unsafe { (api.rate)(dev, s.rate) }),
            ("frequency", unsafe { (api.center)(dev, s.center_hz) }),
            ("gain mode", unsafe {
                (api.gain_mode)(dev, c_int::from(s.manual_gain))
            }),
        ];
        for (what, code) in checks {
            if code != 0 {
                return Err(Error::Other(format!("RTL-SDR: {what} failed ({code})")));
            }
        }
        if s.manual_gain {
            let code = unsafe { (api.gain)(dev, tuner.nearest_gain(s.gain_tenths)) };
            if code != 0 {
                return Err(Error::Other(format!("RTL-SDR: tuner gain failed ({code})")));
            }
        }
        let code = unsafe { (api.reset)(dev) };
        if code != 0 {
            return Err(Error::Other(format!("RTL-SDR: reset failed ({code})")));
        }
        Ok(())
    }

    /// Apply any retune or gain change queued by [`RtlControl`].
    fn apply_controls(&mut self) -> Result<()> {
        // Take the requests and drop the lock before touching the hardware.
        let (frequency, gain) = {
            let mut p = self
                .control
                .0
                .lock()
                .map_err(|_| Error::Other("RTL-SDR control lock poisoned".into()))?;
            (p.frequency.take(), p.gain.take())
        };
        // SAFETY (every call below): `self.dev` stays open until `Drop`.
        if let Some(hz) = frequency {
            if !self.tuner.supports(hz as f64) {
                return Err(Error::Other(format!(
                    "RTL-SDR {} frequency outside {:.0}–{:.0} MHz",
                    self.tuner.name,
                    self.tuner.min_hz as f64 / 1e6,
                    self.tuner.max_hz as f64 / 1e6
                )));
            }
            let rc = unsafe { (self.api.center)(self.dev, hz) };
            if rc != 0 {
                return Err(Error::Other(format!(
                    "RTL-SDR live tune to {hz} Hz failed ({rc})"
                )));
            }
            self.settings.center_hz = hz;
        }
        if let Some((manual, tenths)) = gain {
            let rc = unsafe { (self.api.gain_mode)(self.dev, c_int::from(manual)) };
            if rc != 0 {
                return Err(Error::Other(format!(
                    "RTL-SDR live gain mode failed ({rc})"
                )));
            }
            if manual {
                let rc = unsafe { (self.api.gain)(self.dev, self.tuner.nearest_gain(tenths)) };
                if rc != 0 {
                    return Err(Error::Other(format!(
                        "RTL-SDR live tuner gain failed ({rc})"
                    )));
                }
            }
            self.settings.manual_gain = manual;
            self.settings.gain_tenths = tenths;
        }
        Ok(())
    }

    /// One `rtlsdr_read_sync` into `self.bytes`; returns `(status, bytes read)`.
    fn read_block(&mut self) -> (c_int, c_int) {
        let mut n_read = 0;
        // SAFETY: `self.dev` is open; the buffer pointer and the length passed
        // with it describe the same live allocation.
        let code = unsafe {
            (self.api.read)(
                self.dev,
                self.bytes.as_mut_ptr().cast(),
                self.bytes.len() as c_int,
                &mut n_read,
            )
        };
        (code, n_read)
    }
}

impl Drop for RtlSource {
    fn drop(&mut self) {
        // SAFETY: `dev` was opened by `RtlSource::open` and is closed only here.
        // The DLL is unloaded afterwards, when the `api` field drops.
        unsafe {
            (self.api.close)(self.dev);
        }
    }
}

impl IqSource for RtlSource {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        self.apply_controls()?;
        let (mut code, mut n) = self.read_block();
        // A stalled bulk endpoint (libusb -9, PIPE) is what rtlsdr_reset_buffer clears, so
        // try that a few times before giving up; a dead device fails the same way every time.
        for _ in 0..MAX_USB_RECOVERIES {
            if code == 0 {
                break;
            }
            self.usb_errors += 1;
            // SAFETY: `self.dev` is open.
            if unsafe { (self.api.reset)(self.dev) } != 0 {
                break;
            }
            (code, n) = self.read_block();
        }
        if code != 0 {
            return Err(Error::Other(format!(
                "RTL-SDR: USB read failed ({code}) after {} recoveries. Streaming stopped; \
                 check USB/WinUSB driver, power and compatible RTL DLLs. \
                 This is not a frequency-retune requirement.",
                self.usb_errors
            )));
        }
        // The driver reports how much it wrote; trust it only if it is a whole
        // number of IQ pairs that fits the buffer.
        if n < 0 || n as usize > self.bytes.len() || n % 2 != 0 {
            return Err(Error::Other(format!("RTL-SDR: invalid USB read size {n}")));
        }
        out.clear();
        let count = (n.max(0) as usize).min(self.bytes.len()) / 2;
        out.reserve(count);
        // Rust note: `as_chunks::<2>()` views the bytes as `[u8; 2]` pairs
        // (I then Q) with no copying; `.0` is the pairs, `.1` any odd remainder.
        for iq in self.bytes[..count * 2].as_chunks::<2>().0 {
            out.push(Iq::new(
                (iq[0] as f32 - ADC_MIDPOINT) / ADC_SCALE,
                (iq[1] as f32 - ADC_MIDPOINT) / ADC_SCALE,
            ));
        }
        Ok(count)
    }

    fn sample_rate(&self) -> f64 {
        self.settings.rate as f64
    }

    fn center_freq(&self) -> f64 {
        self.settings.center_hz as f64
    }

    fn describe(&self) -> String {
        format!(
            "RTL-SDR #{} @ {:.3} MS/s, USB recoveries {}",
            self.settings.device_index,
            self.settings.rate as f64 / 1e6,
            self.usb_errors
        )
    }

    fn is_live(&self) -> bool {
        true
    }
}
