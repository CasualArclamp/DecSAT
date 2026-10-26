//! Windows UHD C API receiver, loaded from the user's installed uhd.dll.
//! No UHD headers, Boost, CMake or import libraries are required.
#![cfg(windows)]
use decsat_core::{Error, Iq, Result};
use std::{
    ffi::{CStr, CString, c_char, c_void},
    os::windows::ffi::OsStrExt,
    path::PathBuf,
    ptr,
    sync::{Arc, Mutex},
};
type Hmodule = *mut c_void;
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryW(name: *const u16) -> Hmodule;
    fn GetProcAddress(module: Hmodule, name: *const u8) -> *mut c_void;
    fn FreeLibrary(module: Hmodule) -> i32;
}
struct Dll(Hmodule);
impl Drop for Dll {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                FreeLibrary(self.0);
            }
        }
    }
}
unsafe fn load_sym<T: Copy>(dll: Hmodule, name: &'static [u8]) -> Result<T> {
    let p = unsafe { GetProcAddress(dll, name.as_ptr()) };
    if p.is_null() {
        return Err(Error::Other(format!(
            "UHD export missing: {}",
            String::from_utf8_lossy(&name[..name.len() - 1])
        )));
    }
    Ok(unsafe { std::mem::transmute_copy(&p) })
}

use crate::IqSource;

type Handle = *mut c_void;
type Status = i32;
#[repr(C)]
struct TuneRequest {
    target_freq: f64,
    rf_policy: i32,
    rf_freq: f64,
    dsp_policy: i32,
    dsp_freq: f64,
    args: *mut c_char,
}
#[repr(C)]
#[derive(Default)]
struct TuneResult {
    clipped_rf_freq: f64,
    target_rf_freq: f64,
    actual_rf_freq: f64,
    target_dsp_freq: f64,
    actual_dsp_freq: f64,
}
#[repr(C)]
struct StreamArgs {
    cpu_format: *mut c_char,
    otw_format: *mut c_char,
    args: *mut c_char,
    channel_list: *mut usize,
    n_channels: i32,
}
#[repr(C)]
struct StreamCmd {
    stream_mode: i32,
    num_samps: usize,
    stream_now: bool,
    time_spec_full_secs: i64,
    time_spec_frac_secs: f64,
}

// All function signatures below match UHD's stable C interface in usrp/usrp.h.
struct Api {
    _dll: Dll,
    usrp_make: unsafe extern "C" fn(*mut Handle, *const c_char) -> Status,
    usrp_free: unsafe extern "C" fn(*mut Handle) -> Status,
    set_rate: unsafe extern "C" fn(Handle, f64, usize) -> Status,
    get_rate: unsafe extern "C" fn(Handle, usize, *mut f64) -> Status,
    set_gain: unsafe extern "C" fn(Handle, f64, usize, *const c_char) -> Status,
    set_freq: unsafe extern "C" fn(Handle, *mut TuneRequest, usize, *mut TuneResult) -> Status,
    get_stream: unsafe extern "C" fn(Handle, *mut StreamArgs, Handle) -> Status,
    stream_make: unsafe extern "C" fn(*mut Handle) -> Status,
    stream_free: unsafe extern "C" fn(*mut Handle) -> Status,
    metadata_make: unsafe extern "C" fn(*mut Handle) -> Status,
    metadata_free: unsafe extern "C" fn(*mut Handle) -> Status,
    metadata_error: unsafe extern "C" fn(Handle, *mut i32) -> Status,
    issue_cmd: unsafe extern "C" fn(Handle, *const StreamCmd) -> Status,
    recv: unsafe extern "C" fn(
        Handle,
        *mut *mut c_void,
        usize,
        *mut Handle,
        f64,
        bool,
        *mut usize,
    ) -> Status,
    last_error: unsafe extern "C" fn(Handle, *mut c_char, usize) -> Status,
    #[allow(dead_code)]
    streamer_last_error: unsafe extern "C" fn(Handle, *mut c_char, usize) -> Status,
    max_samples: unsafe extern "C" fn(Handle, *mut usize) -> Status,
    set_antenna: unsafe extern "C" fn(Handle, *const c_char, usize) -> Status,
    set_bandwidth: unsafe extern "C" fn(Handle, f64, usize) -> Status,
    get_freq: unsafe extern "C" fn(Handle, usize, *mut f64) -> Status,
}
impl Api {
    fn load() -> Result<Self> {
        let path = std::env::var_os("UHD_DLL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files\UHD\bin\uhd.dll"));
        if !path.is_file() {
            return Err(Error::Other(format!(
                "UHD runtime not found: {}",
                path.display()
            )));
        }
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let dll = unsafe { LoadLibraryW(wide.as_ptr()) };
        if dll.is_null() {
            return Err(Error::Other(format!(
                "Could not load UHD runtime {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        let guard = Dll(dll);
        macro_rules! sym {
            ($name:literal,$ty:ty) => {{ unsafe { load_sym::<$ty>(dll, concat!($name, "\0").as_bytes())? } }};
        }
        Ok(Self {
            usrp_make: sym!(
                "uhd_usrp_make",
                unsafe extern "C" fn(*mut Handle, *const c_char) -> Status
            ),
            usrp_free: sym!("uhd_usrp_free", unsafe extern "C" fn(*mut Handle) -> Status),
            set_rate: sym!(
                "uhd_usrp_set_rx_rate",
                unsafe extern "C" fn(Handle, f64, usize) -> Status
            ),
            get_rate: sym!(
                "uhd_usrp_get_rx_rate",
                unsafe extern "C" fn(Handle, usize, *mut f64) -> Status
            ),
            set_gain: sym!(
                "uhd_usrp_set_rx_gain",
                unsafe extern "C" fn(Handle, f64, usize, *const c_char) -> Status
            ),
            set_freq: sym!(
                "uhd_usrp_set_rx_freq",
                unsafe extern "C" fn(Handle, *mut TuneRequest, usize, *mut TuneResult) -> Status
            ),
            get_stream: sym!(
                "uhd_usrp_get_rx_stream",
                unsafe extern "C" fn(Handle, *mut StreamArgs, Handle) -> Status
            ),
            stream_make: sym!(
                "uhd_rx_streamer_make",
                unsafe extern "C" fn(*mut Handle) -> Status
            ),
            stream_free: sym!(
                "uhd_rx_streamer_free",
                unsafe extern "C" fn(*mut Handle) -> Status
            ),
            metadata_make: sym!(
                "uhd_rx_metadata_make",
                unsafe extern "C" fn(*mut Handle) -> Status
            ),
            metadata_free: sym!(
                "uhd_rx_metadata_free",
                unsafe extern "C" fn(*mut Handle) -> Status
            ),
            metadata_error: sym!(
                "uhd_rx_metadata_error_code",
                unsafe extern "C" fn(Handle, *mut i32) -> Status
            ),
            issue_cmd: sym!(
                "uhd_rx_streamer_issue_stream_cmd",
                unsafe extern "C" fn(Handle, *const StreamCmd) -> Status
            ),
            recv: sym!(
                "uhd_rx_streamer_recv",
                unsafe extern "C" fn(
                    Handle,
                    *mut *mut c_void,
                    usize,
                    *mut Handle,
                    f64,
                    bool,
                    *mut usize,
                ) -> Status
            ),
            last_error: sym!(
                "uhd_usrp_last_error",
                unsafe extern "C" fn(Handle, *mut c_char, usize) -> Status
            ),
            streamer_last_error: sym!(
                "uhd_rx_streamer_last_error",
                unsafe extern "C" fn(Handle, *mut c_char, usize) -> Status
            ),
            max_samples: sym!(
                "uhd_rx_streamer_max_num_samps",
                unsafe extern "C" fn(Handle, *mut usize) -> Status
            ),
            set_antenna: sym!(
                "uhd_usrp_set_rx_antenna",
                unsafe extern "C" fn(Handle, *const c_char, usize) -> Status
            ),
            set_bandwidth: sym!(
                "uhd_usrp_set_rx_bandwidth",
                unsafe extern "C" fn(Handle, f64, usize) -> Status
            ),
            get_freq: sym!(
                "uhd_usrp_get_rx_freq",
                unsafe extern "C" fn(Handle, usize, *mut f64) -> Status
            ),
            _dll: guard,
        })
    }
}
fn check(n: i32, op: &str) -> Result<()> {
    if n == 0 {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "UHD {op}: error {n} (check UHD runtime/device log)"
        )))
    }
}
#[derive(Clone)]
pub struct UhdSettings {
    pub device_args: String,
    pub center_hz: f64,
    pub rate: f64,
    pub gain: f64,
    pub channel: usize,
    pub antenna: String,
    pub bandwidth: f64,
    pub lnb_lo_hz: f64,
}
impl Default for UhdSettings {
    fn default() -> Self {
        Self {
            device_args: "serial=3KO25DW,num_recv_frames=512,recv_frame_size=16360".into(),
            center_hz: 1_100_000_000.0,
            rate: 8_000_000.0,
            gain: 35.0,
            channel: 0,
            antenna: "RX2".into(),
            bandwidth: 0.0,
            lnb_lo_hz: 0.0,
        }
    }
}

// Use the measured B210 USB 3 transport defaults unless the user overrides them.
// The original device selector (serial, type, addr, etc.) is preserved verbatim.
fn uhd_device_args_with_rx_defaults(user_args: &str) -> String {
    let mut result = user_args.trim().trim_end_matches(',').to_owned();
    for (key, value) in [("num_recv_frames", "512"), ("recv_frame_size", "16360")] {
        let present = result.split(',').any(|entry| {
            entry
                .split_once('=')
                .map(|(name, _)| name.trim().eq_ignore_ascii_case(key))
                .unwrap_or(false)
        });
        if !present {
            if !result.is_empty() {
                result.push(',');
            }
            result.push_str(key);
            result.push('=');
            result.push_str(value);
        }
    }
    result
}

#[derive(Default)]
struct PendingControl {
    freq: Option<f64>,
    gain: Option<f64>,
}
#[derive(Clone)]
pub struct UhdControl(Arc<Mutex<PendingControl>>);
impl UhdControl {
    pub fn tune(&self, hz: f64) {
        if let Ok(mut p) = self.0.lock() {
            p.freq = Some(hz);
        }
    }
    pub fn gain(&self, gain: f64) {
        if let Ok(mut p) = self.0.lock() {
            p.gain = Some(gain);
        }
    }
}

pub struct UhdSource {
    control: UhdControl,
    api: Api,
    usrp: Handle,
    stream: Handle,
    md: Handle,
    settings: UhdSettings,
    real_rate: f64,
    buffer: Vec<Iq>,
    overflows: u64,
    consecutive_timeouts: u32,
}
// UHD C handles are accessed only on the engine's single source thread.
unsafe impl Send for UhdSource {}
impl UhdSource {
    pub fn open(settings: UhdSettings) -> Result<Self> {
        if !settings.center_hz.is_finite()
            || settings.center_hz <= 0.0
            || !settings.rate.is_finite()
            || settings.rate <= 0.0
            || !settings.gain.is_finite()
        {
            return Err(Error::Other("Invalid UHD radio settings".into()));
        }
        if settings.rate > 50_000_000.0 {
            return Err(Error::Other(
                "UHD sample rates above 50 MS/s are not enabled in this build.".into(),
            ));
        }
        let api = Api::load()?;
        let mut src = Self {
            control: UhdControl(Arc::new(Mutex::new(PendingControl::default()))),
            api,
            usrp: ptr::null_mut(),
            stream: ptr::null_mut(),
            md: ptr::null_mut(),
            settings,
            real_rate: 0.0,
            buffer: vec![Iq::new(0.0, 0.0); 16384],
            overflows: 0,
            consecutive_timeouts: 0,
        };
        src.configure()?;
        Ok(src)
    }
    pub fn control(&self) -> UhdControl {
        self.control.clone()
    }
    fn configure(&mut self) -> Result<()> {
        let args = CString::new(uhd_device_args_with_rx_defaults(&self.settings.device_args))
            .map_err(|e| Error::Other(e.to_string()))?;
        let c = self.settings.channel;
        let detail = |api: &Api, handle: Handle, op: &str, n: Status| -> Error {
            let mut buf = [0 as c_char; 2048];
            let message = if !handle.is_null()
                && unsafe { (api.last_error)(handle, buf.as_mut_ptr(), buf.len()) } == 0
            {
                unsafe { CStr::from_ptr(buf.as_ptr()) }
                    .to_string_lossy()
                    .into_owned()
            } else {
                "No UHD exception detail".into()
            };
            Error::Other(format!("UHD {op}: {n}, {message}"))
        };
        // SAFETY: all pointer arguments are valid, and UHD returns owned opaque handles.
        unsafe {
            let rc = (self.api.usrp_make)(&mut self.usrp, args.as_ptr());
            if rc != 0 {
                return Err(detail(&self.api, self.usrp, "open", rc));
            }
            check(
                (self.api.set_rate)(self.usrp, self.settings.rate, c),
                "set sample rate",
            )?;
            check(
                (self.api.get_rate)(self.usrp, c, &mut self.real_rate),
                "get actual sample rate",
            )?;
            check(
                (self.api.set_gain)(self.usrp, self.settings.gain, c, c"".as_ptr()),
                "set gain",
            )?;
            let ant = CString::new(self.settings.antenna.as_str())
                .map_err(|e| Error::Other(e.to_string()))?;
            check(
                (self.api.set_antenna)(self.usrp, ant.as_ptr(), c),
                "set antenna",
            )?;
            if self.settings.bandwidth > 0.0 {
                check(
                    (self.api.set_bandwidth)(self.usrp, self.settings.bandwidth, c),
                    "set bandwidth",
                )?;
            }
            let mut tune = TuneRequest {
                target_freq: self.settings.center_hz,
                rf_policy: 65,
                rf_freq: 0.0,
                dsp_policy: 65,
                dsp_freq: 0.0,
                args: ptr::null_mut(),
            };
            let mut result = TuneResult::default();
            check(
                (self.api.set_freq)(self.usrp, &mut tune, c, &mut result),
                "tune",
            )?;
            let mut actual_freq = 0.0;
            check(
                (self.api.get_freq)(self.usrp, c, &mut actual_freq),
                "get actual frequency",
            )?;
            self.settings.center_hz = actual_freq;
            check((self.api.stream_make)(&mut self.stream), "create stream")?;
            let mut channel = c;
            let mut cpu = b"fc32\0".to_vec();
            let mut otw = b"sc16\0".to_vec();
            let mut extra = b"\0".to_vec();
            let mut sa = StreamArgs {
                cpu_format: cpu.as_mut_ptr().cast(),
                otw_format: otw.as_mut_ptr().cast(),
                args: extra.as_mut_ptr().cast(),
                channel_list: &mut channel,
                n_channels: 1,
            };
            check(
                (self.api.get_stream)(self.usrp, &mut sa, self.stream),
                "configure stream",
            )?;
            let mut max_samples = 0usize;
            check(
                (self.api.max_samples)(self.stream, &mut max_samples),
                "max buffer size",
            )?;
            if max_samples > 0 {
                self.buffer
                    .resize(max_samples.min(65536), Iq::new(0.0, 0.0));
            }
            check((self.api.metadata_make)(&mut self.md), "create metadata")?;
            self.command(97)?;
        }
        Ok(())
    }
    /// One frequency step of the B210 overview. The device and streamer are
    /// kept open across the entire sweep; only RX streaming is restarted.
    /// Unlike the generic sweep, no arbitrary fixed number of old RX blocks
    /// is assumed to delimit two tuner positions.
    pub fn sweep_capture(&mut self, hz: f64, out: &mut Vec<Iq>) -> Result<f64> {
        if !hz.is_finite() || !(50e6..=6e9).contains(&hz) {
            return Err(Error::Other(
                "UHD sweep frequency outside 50–6000 MHz".into(),
            ));
        }
        // RX must be stopped before changing the RF synthesizer.
        unsafe {
            self.command(111)?;
        }
        // Drain the packets already in the host transport queue. A short UHD
        // timeout bounds this loop even when the queue is empty.
        for _ in 0..128 {
            let mut addr = self.buffer.as_mut_ptr().cast::<c_void>();
            let mut n = 0usize;
            let rc = unsafe {
                (self.api.recv)(
                    self.stream,
                    &mut addr,
                    self.buffer.len(),
                    &mut self.md,
                    0.002,
                    false,
                    &mut n,
                )
            };
            if rc != 0 || n == 0 {
                break;
            }
        }
        let mut tune = TuneRequest {
            target_freq: hz,
            rf_policy: 65,
            rf_freq: 0.0,
            dsp_policy: 65,
            dsp_freq: 0.0,
            args: ptr::null_mut(),
        };
        let mut result = TuneResult::default();
        check(
            unsafe {
                (self.api.set_freq)(self.usrp, &mut tune, self.settings.channel, &mut result)
            },
            "sweep tune",
        )?;
        let mut actual = 0f64;
        check(
            unsafe { (self.api.get_freq)(self.usrp, self.settings.channel, &mut actual) },
            "sweep actual frequency",
        )?;
        if !actual.is_finite() || (actual - hz).abs() > self.real_rate * 0.05 {
            return Err(Error::Other(format!(
                "UHD sweep tune mismatch: requested {hz:.0} Hz, actual {actual:.0} Hz"
            )));
        }
        self.settings.center_hz = actual;
        std::thread::sleep(std::time::Duration::from_millis(12));
        unsafe {
            self.command(97)?;
        }
        // Discard a time-based acquisition guard after starting the new stream.
        // Also discard at least two whole internal receive buffers.
        let discard = ((self.real_rate * 0.012).ceil() as usize)
            .max(self.buffer.len().saturating_mul(2))
            .min(524288);
        let mut skipped = 0usize;
        let mut tmp = Vec::new();
        let mut attempts = 0;
        while skipped < discard && attempts < 128 {
            attempts += 1;
            match self.read(&mut tmp) {
                Ok(n) if n > 0 => skipped += n,
                Ok(_) => {}
                Err(e) => return Err(e),
            }
        }
        if skipped < discard {
            return Err(Error::Other(
                "UHD sweep failed to acquire fresh samples".into(),
            ));
        }
        out.clear();
        attempts = 0;
        while out.len() < 4096 && attempts < 32 {
            attempts += 1;
            let n = self.read(&mut tmp)?;
            if n > 0 {
                out.extend_from_slice(&tmp[..n.min(4096 - out.len())]);
            }
        }
        if out.len() < 4096 {
            return Err(Error::Other("UHD sweep: insufficient IQ samples".into()));
        }
        Ok(actual)
    }
    fn apply_controls(&mut self) -> Result<()> {
        let (freq, gain) = {
            let mut p = self
                .control
                .0
                .lock()
                .map_err(|_| Error::Other("UHD control lock poisoned".into()))?;
            (p.freq.take(), p.gain.take())
        };
        if let Some(freq) = freq {
            let mut tune = TuneRequest {
                target_freq: freq,
                rf_policy: 65,
                rf_freq: 0.0,
                dsp_policy: 65,
                dsp_freq: 0.0,
                args: ptr::null_mut(),
            };
            let mut result = TuneResult::default();
            check(
                unsafe {
                    (self.api.set_freq)(self.usrp, &mut tune, self.settings.channel, &mut result)
                },
                "live tune",
            )?;
            self.settings.center_hz = freq;
        }
        if let Some(gain) = gain {
            check(
                unsafe {
                    (self.api.set_gain)(self.usrp, gain, self.settings.channel, c"".as_ptr())
                },
                "live gain",
            )?;
            self.settings.gain = gain;
        }
        Ok(())
    }
    unsafe fn command(&self, mode: i32) -> Result<()> {
        let cmd = StreamCmd {
            stream_mode: mode,
            num_samps: 0,
            stream_now: true,
            time_spec_full_secs: 0,
            time_spec_frac_secs: 0.0,
        };
        check(
            unsafe { (self.api.issue_cmd)(self.stream, &cmd) },
            "stream command",
        )
    }
}
impl IqSource for UhdSource {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        self.apply_controls()?;
        // IqSource::read returning Ok(0) signals EOF, so retry transient overflows.
        for attempt in 0..12 {
            let mut addr = self.buffer.as_mut_ptr().cast::<c_void>();
            let mut n = 0usize;
            let code = unsafe {
                (self.api.recv)(
                    self.stream,
                    &mut addr,
                    self.buffer.len(),
                    &mut self.md,
                    0.4,
                    false,
                    &mut n,
                )
            };
            check(code, "receive")?;
            let mut md_error = 0i32;
            check(
                unsafe { (self.api.metadata_error)(self.md, &mut md_error) },
                "receive metadata",
            )?;
            if md_error != 0 {
                if md_error == 8 {
                    // UHD RX overflow: skip this block, keep the receiver alive.
                    self.overflows += 1;
                    if attempt < 11 {
                        continue;
                    }
                    return Err(Error::Other(format!(
                        "UHD RX cannot recover from overflows ({} events). Lower the sample rate.",
                        self.overflows
                    )));
                } else if md_error == 1 {
                    // UHD timeout: transient is not a fatal device failure.
                    self.consecutive_timeouts += 1;
                    if self.consecutive_timeouts < 10 && attempt < 11 {
                        continue;
                    }
                    return Err(Error::Other("UHD: ten consecutive RX timeouts".into()));
                } else {
                    return Err(Error::Other(format!("UHD RX metadata error {md_error}")));
                }
            }
            self.consecutive_timeouts = 0;
            if n == 0 {
                continue;
            }
            out.clear();
            out.extend_from_slice(&self.buffer[..n.min(self.buffer.len())]);
            return Ok(out.len());
        }
        Err(Error::Other(
            "UHD RX did not deliver IQ data after retries".into(),
        ))
    }
    fn sample_rate(&self) -> f64 {
        self.real_rate
    }
    fn center_freq(&self) -> f64 {
        self.settings.center_hz
    }
    fn describe(&self) -> String {
        format!(
            "UHD B210 @ {:.3} MHz, {:.2} MS/s, overflow events {}",
            self.settings.center_hz / 1e6,
            self.real_rate / 1e6,
            self.overflows
        )
    }
    fn is_live(&self) -> bool {
        true
    }
}
impl Drop for UhdSource {
    fn drop(&mut self) {
        unsafe {
            if !self.stream.is_null() {
                let _ = self.command(111);
            }
            if !self.md.is_null() {
                let _ = (self.api.metadata_free)(&mut self.md);
            }
            if !self.stream.is_null() {
                let _ = (self.api.stream_free)(&mut self.stream);
            }
            if !self.usrp.is_null() {
                let _ = (self.api.usrp_free)(&mut self.usrp);
            }
        }
    }
}
