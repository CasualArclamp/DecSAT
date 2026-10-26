//! RTL2832U tuner-aware receive-only source (librtlsdr, Windows).
//! Uses a dynamically loaded rtlsdr.dll. No driver changes to existing receivers.
use crate::IqSource;
use decsat_core::{Error, Iq, Result};
use std::os::windows::ffi::OsStrExt;
use std::{
    ffi::{CString, c_int, c_void},
    ptr,
    sync::{Arc, Mutex},
};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    fn FreeLibrary(module: *mut c_void) -> i32;
}
type Open = unsafe extern "C" fn(*mut *mut c_void, u32) -> c_int;
type Close = unsafe extern "C" fn(*mut c_void) -> c_int;
type SetU32 = unsafe extern "C" fn(*mut c_void, u32) -> c_int;
type SetGainMode = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type SetGain = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type Reset = unsafe extern "C" fn(*mut c_void) -> c_int;
type GetTunerType = unsafe extern "C" fn(*mut c_void) -> c_int;
type GetTunerGains = unsafe extern "C" fn(*mut c_void, *mut c_int) -> c_int;
type ReadSync = unsafe extern "C" fn(*mut c_void, *mut c_void, c_int, *mut c_int) -> c_int;
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
unsafe impl Send for Api {}
impl Drop for Api {
    fn drop(&mut self) {
        unsafe {
            FreeLibrary(self.module);
        }
    }
}
unsafe fn symbol<T: Copy>(m: *mut c_void, n: &str) -> Result<T> {
    let name = CString::new(n).unwrap();
    let p = unsafe { GetProcAddress(m, name.as_ptr().cast::<u8>()) };
    if p.is_null() {
        return Err(Error::Other(format!("RTL-SDR: missing {n} in rtlsdr.dll")));
    }
    Ok(unsafe { std::mem::transmute_copy(&p) })
}
impl Api {
    fn load() -> Result<Self> {
        let path = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("RTL").join("rtlsdr.dll")));
        let Some(path) = path else {
            return Err(Error::Other(
                "RTL-SDR: cannot locate executable directory".into(),
            ));
        };
        let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let m = unsafe { LoadLibraryExW(name.as_ptr(), ptr::null_mut(), 0x00000100 | 0x00001000) };
        if m.is_null() {
            return Err(Error::Other(r"RTL-SDR: RTL\rtlsdr.dll not found or dependencies unavailable. Put rtlsdr.dll and its matching libusb-1.0.dll into bin\x64\Release\RTL\.".into()));
        }
        let build = (|| unsafe {
            Ok(Self {
                module: m,
                open: symbol(m, "rtlsdr_open")?,
                close: symbol(m, "rtlsdr_close")?,
                center: symbol(m, "rtlsdr_set_center_freq")?,
                rate: symbol(m, "rtlsdr_set_sample_rate")?,
                reset: symbol(m, "rtlsdr_reset_buffer")?,
                gain_mode: symbol(m, "rtlsdr_set_tuner_gain_mode")?,
                gain: symbol(m, "rtlsdr_set_tuner_gain")?,
                get_tuner_type: symbol(m, "rtlsdr_get_tuner_type")?,
                get_tuner_gains: symbol(m, "rtlsdr_get_tuner_gains")?,
                read: symbol(m, "rtlsdr_read_sync")?,
            })
        })();
        if build.is_err() {
            unsafe {
                FreeLibrary(m);
            }
        }
        build
    }
}
#[derive(Clone, Debug)]
pub struct RtlTunerInfo {
    pub name: &'static str,
    pub min_hz: u32,
    pub max_hz: u32,
    /// Values returned by librtlsdr, in tenths of a dB.
    pub gains_tenths: Vec<i32>,
}
impl RtlTunerInfo {
    fn from_device(api: &Api, dev: *mut c_void) -> Self {
        let (name, min_hz, max_hz) = match unsafe { (api.get_tuner_type)(dev) } {
            1 => ("E4000", 52_000_000, 2_200_000_000),
            2 => ("FC0012", 22_000_000, 948_600_000),
            3 => ("FC0013", 22_000_000, 1_100_000_000),
            4 => ("FC2580", 146_000_000, 924_000_000),
            5 => ("R820T", 24_000_000, 1_766_000_000),
            6 => ("R828D", 24_000_000, 1_766_000_000),
            _ => ("Unknown", 24_000_000, 2_200_000_000),
        };
        let count = unsafe { (api.get_tuner_gains)(dev, ptr::null_mut()) };
        let mut gains_tenths = Vec::new();
        if (1..=128).contains(&count) {
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
    pub fn supports(&self, hz: f64) -> bool {
        hz >= self.min_hz as f64 && hz <= self.max_hz as f64
    }
    pub fn nearest_gain(&self, gain: i32) -> i32 {
        self.gains_tenths
            .iter()
            .copied()
            .min_by_key(|g| (i64::from(*g) - i64::from(gain)).abs())
            .unwrap_or(gain)
    }
}

#[derive(Clone, Debug)]
pub struct RtlSettings {
    pub device_index: u32,
    pub center_hz: u32,
    pub lnb_lo_hz: f64,
    pub rate: u32,
    pub gain_tenths: i32,
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
#[derive(Default)]
struct PendingControl {
    frequency: Option<u32>,
    gain: Option<(bool, i32)>,
    tuner: Option<RtlTunerInfo>,
}
#[derive(Clone)]
pub struct RtlControl(Arc<Mutex<PendingControl>>);
impl RtlControl {
    pub fn tuner_info(&self) -> Option<RtlTunerInfo> {
        self.0.lock().ok().and_then(|p| p.tuner.clone())
    }
    pub fn tune(&self, hz: u32) {
        if let Ok(mut p) = self.0.lock() {
            p.frequency = Some(hz);
        }
    }
    pub fn gain(&self, manual: bool, tenths: i32) {
        if let Ok(mut p) = self.0.lock() {
            p.gain = Some((manual, tenths));
        }
    }
}
/// Resets of the USB buffer tried after a failed read before the stream is declared dead.
const MAX_USB_RECOVERIES: u32 = 3;
pub struct RtlSource {
    api: Api,
    dev: *mut c_void,
    settings: RtlSettings,
    bytes: Vec<u8>,
    control: RtlControl,
    tuner: RtlTunerInfo,
    usb_errors: u32,
}
unsafe impl Send for RtlSource {}
impl RtlSource {
    pub fn control(&self) -> RtlControl {
        self.control.clone()
    }
    fn apply_controls(&mut self) -> Result<()> {
        let (frequency, gain) = {
            let mut p = self
                .control
                .0
                .lock()
                .map_err(|_| Error::Other("RTL-SDR control lock poisoned".into()))?;
            (p.frequency.take(), p.gain.take())
        };
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
            let rc = unsafe { (self.api.gain_mode)(self.dev, if manual { 1 } else { 0 }) };
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
    pub fn open(s: RtlSettings) -> Result<Self> {
        if !(225_001..=3_200_000).contains(&s.rate) {
            return Err(Error::Other("RTL-SDR: sample rate must be 0.225–3.2 MS/s; use <=2.4 MS/s for reliable operation".into()));
        }
        let api = Api::load()?;
        let mut d = ptr::null_mut();
        let status = unsafe { (api.open)(&mut d, s.device_index) };
        if status != 0 || d.is_null() {
            return Err(Error::Other(format!(
                "RTL-SDR: cannot open device {} (code {status}). Check WinUSB driver and that no other SDR app uses it.",
                s.device_index
            )));
        }
        let tuner = RtlTunerInfo::from_device(&api, d);
        if !tuner.supports(s.center_hz as f64) {
            unsafe { (api.close)(d) };
            return Err(Error::Other(format!(
                "RTL-SDR {} supports {:.0}–{:.0} MHz (requested {:.3} MHz)",
                tuner.name,
                tuner.min_hz as f64 / 1e6,
                tuner.max_hz as f64 / 1e6,
                s.center_hz as f64 / 1e6
            )));
        }
        let setup = (|| {
            let checks = [
                ("sample rate", unsafe { (api.rate)(d, s.rate) }),
                ("frequency", unsafe { (api.center)(d, s.center_hz) }),
                ("gain mode", unsafe {
                    (api.gain_mode)(d, if s.manual_gain { 1 } else { 0 })
                }),
            ];
            for (what, code) in checks {
                if code != 0 {
                    return Err(Error::Other(format!("RTL-SDR: {what} failed ({code})")));
                }
            }
            if s.manual_gain {
                let code = unsafe { (api.gain)(d, tuner.nearest_gain(s.gain_tenths)) };
                if code != 0 {
                    return Err(Error::Other(format!("RTL-SDR: tuner gain failed ({code})")));
                }
            }
            let code = unsafe { (api.reset)(d) };
            if code != 0 {
                return Err(Error::Other(format!("RTL-SDR: reset failed ({code})")));
            }
            Ok(())
        })();
        if let Err(e) = setup {
            unsafe { (api.close)(d) };
            return Err(e);
        }
        Ok(Self {
            api,
            dev: d,
            settings: s,
            bytes: vec![0; 32768],
            control: RtlControl(Arc::new(Mutex::new(PendingControl {
                tuner: Some(tuner.clone()),
                ..PendingControl::default()
            }))),
            tuner,
            usb_errors: 0,
        })
    }
}
impl Drop for RtlSource {
    fn drop(&mut self) {
        unsafe {
            (self.api.close)(self.dev);
        }
    }
}
impl IqSource for RtlSource {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        self.apply_controls()?;
        let mut n = 0;
        let mut code = unsafe {
            (self.api.read)(
                self.dev,
                self.bytes.as_mut_ptr().cast(),
                self.bytes.len() as c_int,
                &mut n,
            )
        };
        // A stalled bulk endpoint (libusb -9, PIPE) is what rtlsdr_reset_buffer clears, so
        // try that a few times before giving up; a dead device fails the same way every time.
        for _ in 0..MAX_USB_RECOVERIES {
            if code == 0 {
                break;
            }
            self.usb_errors += 1;
            if unsafe { (self.api.reset)(self.dev) } != 0 {
                break;
            }
            n = 0;
            code = unsafe {
                (self.api.read)(
                    self.dev,
                    self.bytes.as_mut_ptr().cast(),
                    self.bytes.len() as c_int,
                    &mut n,
                )
            };
        }
        if code != 0 {
            return Err(Error::Other(format!(
                "RTL-SDR: USB read failed ({code}) after {} recoveries. Streaming stopped; check USB/WinUSB driver, power and compatible RTL DLLs. This is not a frequency-retune requirement.",
                self.usb_errors
            )));
        }
        if n < 0 || n as usize > self.bytes.len() || n % 2 != 0 {
            return Err(Error::Other(format!("RTL-SDR: invalid USB read size {n}")));
        }
        out.clear();
        let count = (n.max(0) as usize).min(self.bytes.len()) / 2;
        out.reserve(count);
        for iq in self.bytes[..count * 2].as_chunks::<2>().0 {
            out.push(Iq::new(
                (iq[0] as f32 - 127.5) / 128.0,
                (iq[1] as f32 - 127.5) / 128.0,
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
