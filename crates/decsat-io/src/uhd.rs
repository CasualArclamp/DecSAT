//! Ettus/NI USRP (B210 and kin) receive-only source, over UHD's C API.
//!
//! `uhd.dll` is loaded at run time from the user's own UHD install (default
//! `C:\Program Files\UHD\bin\uhd.dll`, or the absolute path in the `UHD_DLL`
//! environment variable), so no UHD headers, Boost, CMake or import library are
//! needed to build, and the portable exe starts on a machine without UHD.
//!
//! Unlike the RTL-SDR and Pluto backends (`LoadLibraryExW` with explicit search
//! flags) this uses plain `LoadLibraryW` on the DLL's absolute path, so UHD's own
//! dependencies (libusb and friends) are found by the default Windows search
//! order. That order includes the executable's directory, which is why the other
//! two backends keep their runtime DLLs in sub-folders rather than beside the exe.
//!
//! Samples are requested as `fc32` (host complex `f32`, the same layout as
//! [`Iq`]) over the wire as `sc16`.
//!
//! Provenance: merged in from the "VMA" snapshot, see `docs/STATUS.md`
//! (2026-10-25). Not tested against real hardware by whoever restyled it.
#![cfg(windows)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use decsat_core::{Error, Iq, Result};

use crate::IqSource;

/// A Win32 `HMODULE`.
type Hmodule = *mut c_void;

// Win32 loader calls (kernel32), declared by hand so no crate is needed.
//
// Rust note: since edition 2024 an `extern` block must be written
// `unsafe extern`, because the compiler cannot check that the declared
// signatures match the real functions. `"system"` is the platform's system
// calling convention (stdcall on 32-bit Windows, the usual one on 64-bit).
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryW(name: *const u16) -> Hmodule;
    fn GetProcAddress(module: Hmodule, name: *const u8) -> *mut c_void;
    fn FreeLibrary(module: Hmodule) -> i32;
}

/// A loaded module, freed when dropped.
struct Dll(Hmodule);

impl Drop for Dll {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle came from a successful `LoadLibraryW` and is
            // freed exactly once, here.
            unsafe {
                FreeLibrary(self.0);
            }
        }
    }
}

/// Look up the exported function `name` and reinterpret it as the function
/// pointer type `T`. `name` must include its terminating NUL.
///
/// # Safety
///
/// `dll` must be a live module handle, and `T` must be the real signature of
/// the export (the compiler cannot check that).
unsafe fn load_sym<T: Copy>(dll: Hmodule, name: &'static [u8]) -> Result<T> {
    // SAFETY: `dll` is valid (caller) and `name` is NUL-terminated.
    let address = unsafe { GetProcAddress(dll, name.as_ptr()) };
    if address.is_null() {
        return Err(Error::Other(format!(
            "UHD export missing: {}",
            String::from_utf8_lossy(&name[..name.len() - 1])
        )));
    }
    // Rust note: `transmute_copy` reads the pointer's bytes as a `T`. It is the
    // usual way to turn a raw `GetProcAddress` result into a typed `extern "C"`
    // function pointer, which `as` casts cannot do generically.
    //
    // SAFETY: `T` is a function-pointer type (pointer-sized, caller's contract).
    Ok(unsafe { std::mem::transmute_copy(&address) })
}

/// A UHD C-API handle (`uhd_usrp_handle`, `uhd_rx_streamer_handle`,
/// `uhd_rx_metadata_handle`): an opaque pointer. Null means "none".
type Handle = *mut c_void;
/// `uhd_error`: 0 is success, anything else is a failure code.
type Status = i32;

// UHD enum values used below. The C enums are `int`-sized, so plain integers
// mirror them; the numbers are the ASCII codes UHD gave the enumerators.

/// `UHD_TUNE_REQUEST_POLICY_AUTO` (`uhd/types/tune_request.h`): UHD chooses how
/// the target frequency is split between the RF synthesiser and the DSP.
const TUNE_POLICY_AUTO: i32 = 65; // 'A'
/// `UHD_STREAM_MODE_START_CONTINUOUS` (`uhd/types/stream_cmd.h`): stream until told to stop.
const STREAM_MODE_START_CONTINUOUS: i32 = 97; // 'a'
/// `UHD_STREAM_MODE_STOP_CONTINUOUS` (`uhd/types/stream_cmd.h`).
const STREAM_MODE_STOP_CONTINUOUS: i32 = 111; // 'o'
/// `UHD_RX_METADATA_ERROR_CODE_TIMEOUT` (`uhd/types/metadata.h`): no packet arrived in time.
const MD_ERROR_TIMEOUT: i32 = 1;
/// `UHD_RX_METADATA_ERROR_CODE_OVERFLOW` (`uhd/types/metadata.h`): the host fell behind and
/// samples were dropped.
const MD_ERROR_OVERFLOW: i32 = 8;

/// `uhd_tune_request_t` (`uhd/types/tune_request.h`).
#[repr(C)]
struct TuneRequest {
    target_freq: f64,
    rf_policy: i32,
    rf_freq: f64,
    dsp_policy: i32,
    dsp_freq: f64,
    args: *mut c_char,
}

impl TuneRequest {
    /// Tune to `target_freq` Hz and let UHD split it between RF and DSP.
    fn auto(target_freq: f64) -> Self {
        Self {
            target_freq,
            rf_policy: TUNE_POLICY_AUTO,
            rf_freq: 0.0,
            dsp_policy: TUNE_POLICY_AUTO,
            dsp_freq: 0.0,
            args: ptr::null_mut(),
        }
    }
}

/// `uhd_tune_result_t` (`uhd/types/tune_result.h`), filled in by UHD.
#[repr(C)]
#[derive(Default)]
struct TuneResult {
    clipped_rf_freq: f64,
    target_rf_freq: f64,
    actual_rf_freq: f64,
    target_dsp_freq: f64,
    actual_dsp_freq: f64,
}

/// `uhd_stream_args_t` (`uhd/types/stream_args.h`). The strings are NUL-terminated.
#[repr(C)]
struct StreamArgs {
    /// Sample format in host memory.
    cpu_format: *mut c_char,
    /// Sample format on the wire.
    otw_format: *mut c_char,
    /// Extra `key=value` stream arguments.
    args: *mut c_char,
    channel_list: *mut usize,
    n_channels: i32,
}

/// `uhd_stream_cmd_t` (`uhd/types/stream_cmd.h`).
#[repr(C)]
struct StreamCmd {
    stream_mode: i32,
    num_samps: usize,
    stream_now: bool,
    time_spec_full_secs: i64,
    time_spec_frac_secs: f64,
}

/// The UHD entry points DecSAT uses, resolved once from the loaded DLL. The
/// signatures match UHD's stable C interface (`uhd/usrp/usrp.h`,
/// `uhd/usrp/rx_streamer.h`, `uhd/types/metadata.h`). Each field is named after
/// the `uhd_*` export it comes from; `usrp` is the radio handle, `channel` the
/// RX channel index.
struct Api {
    /// Keeps the DLL loaded for as long as the function pointers below are used.
    _dll: Dll,
    /// `uhd_usrp_make(&handle, device_args)` / `uhd_usrp_free(&handle)`.
    usrp_make: unsafe extern "C" fn(*mut Handle, *const c_char) -> Status,
    usrp_free: unsafe extern "C" fn(*mut Handle) -> Status,
    /// `uhd_usrp_set_rx_rate(usrp, rate, channel)` / `uhd_usrp_get_rx_rate(usrp, channel, &rate)`.
    set_rate: unsafe extern "C" fn(Handle, f64, usize) -> Status,
    get_rate: unsafe extern "C" fn(Handle, usize, *mut f64) -> Status,
    /// `uhd_usrp_set_rx_gain(usrp, gain, channel, gain_name)`; an empty name is the overall gain.
    set_gain: unsafe extern "C" fn(Handle, f64, usize, *const c_char) -> Status,
    /// `uhd_usrp_set_rx_freq(usrp, &request, channel, &result)`.
    set_freq: unsafe extern "C" fn(Handle, *mut TuneRequest, usize, *mut TuneResult) -> Status,
    /// `uhd_usrp_get_rx_stream(usrp, &args, streamer)`.
    get_stream: unsafe extern "C" fn(Handle, *mut StreamArgs, Handle) -> Status,
    /// `uhd_rx_streamer_make(&streamer)` / `uhd_rx_streamer_free(&streamer)`.
    stream_make: unsafe extern "C" fn(*mut Handle) -> Status,
    stream_free: unsafe extern "C" fn(*mut Handle) -> Status,
    /// `uhd_rx_metadata_make(&md)` / `uhd_rx_metadata_free(&md)`.
    metadata_make: unsafe extern "C" fn(*mut Handle) -> Status,
    metadata_free: unsafe extern "C" fn(*mut Handle) -> Status,
    /// `uhd_rx_metadata_error_code(md, &code)`: one of the `MD_ERROR_*` values, or 0.
    metadata_error: unsafe extern "C" fn(Handle, *mut i32) -> Status,
    /// `uhd_rx_streamer_issue_stream_cmd(streamer, &cmd)`.
    issue_cmd: unsafe extern "C" fn(Handle, *const StreamCmd) -> Status,
    /// `uhd_rx_streamer_recv(streamer, buffs, samps_per_buff, &md, timeout_s, one_packet, &received)`.
    /// `buffs` is an array of one buffer pointer per channel.
    recv: unsafe extern "C" fn(
        Handle,
        *mut *mut c_void,
        usize,
        *mut Handle,
        f64,
        bool,
        *mut usize,
    ) -> Status,
    /// `uhd_usrp_last_error(usrp, buf, len)`: text of the last exception on that handle.
    last_error: unsafe extern "C" fn(Handle, *mut c_char, usize) -> Status,
    /// `uhd_rx_streamer_last_error`. Resolved (so a UHD without it is refused up
    /// front) but not called.
    #[allow(dead_code)]
    streamer_last_error: unsafe extern "C" fn(Handle, *mut c_char, usize) -> Status,
    /// `uhd_rx_streamer_max_num_samps(streamer, &n)`.
    max_samples: unsafe extern "C" fn(Handle, *mut usize) -> Status,
    /// `uhd_usrp_set_rx_antenna(usrp, name, channel)`.
    set_antenna: unsafe extern "C" fn(Handle, *const c_char, usize) -> Status,
    /// `uhd_usrp_set_rx_bandwidth(usrp, bandwidth, channel)`.
    set_bandwidth: unsafe extern "C" fn(Handle, f64, usize) -> Status,
    /// `uhd_usrp_get_rx_freq(usrp, channel, &freq)`.
    get_freq: unsafe extern "C" fn(Handle, usize, *mut f64) -> Status,
}

impl Api {
    /// Load `uhd.dll` and resolve every export.
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
        // Win32 wants a NUL-terminated UTF-16 string.
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let dll = unsafe { LoadLibraryW(wide.as_ptr()) };
        if dll.is_null() {
            return Err(Error::Other(format!(
                "Could not load UHD runtime {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        // From here `guard` owns the DLL, so an early `?` frees it.
        let guard = Dll(dll);
        // Rust note: the target type of `load_sym` is inferred from the struct
        // field each result is assigned to, so each signature is written once,
        // in `Api` above.
        macro_rules! sym {
            ($name:literal) => {
                // SAFETY: `dll` is live (held by `guard`) and every field type
                // is the C signature of the export it is filled from.
                unsafe { load_sym(dll, concat!($name, "\0").as_bytes())? }
            };
        }
        Ok(Self {
            usrp_make: sym!("uhd_usrp_make"),
            usrp_free: sym!("uhd_usrp_free"),
            set_rate: sym!("uhd_usrp_set_rx_rate"),
            get_rate: sym!("uhd_usrp_get_rx_rate"),
            set_gain: sym!("uhd_usrp_set_rx_gain"),
            set_freq: sym!("uhd_usrp_set_rx_freq"),
            get_stream: sym!("uhd_usrp_get_rx_stream"),
            stream_make: sym!("uhd_rx_streamer_make"),
            stream_free: sym!("uhd_rx_streamer_free"),
            metadata_make: sym!("uhd_rx_metadata_make"),
            metadata_free: sym!("uhd_rx_metadata_free"),
            metadata_error: sym!("uhd_rx_metadata_error_code"),
            issue_cmd: sym!("uhd_rx_streamer_issue_stream_cmd"),
            recv: sym!("uhd_rx_streamer_recv"),
            last_error: sym!("uhd_usrp_last_error"),
            streamer_last_error: sym!("uhd_rx_streamer_last_error"),
            max_samples: sym!("uhd_rx_streamer_max_num_samps"),
            set_antenna: sym!("uhd_usrp_set_rx_antenna"),
            set_bandwidth: sym!("uhd_usrp_set_rx_bandwidth"),
            get_freq: sym!("uhd_usrp_get_rx_freq"),
            _dll: guard,
        })
    }

    /// An error for a failed call `op` that returned `status`, with the text of
    /// the exception UHD recorded on `handle` when there is one.
    fn error_with_detail(&self, handle: Handle, op: &str, status: Status) -> Error {
        let mut text = [0 as c_char; 2048];
        // SAFETY: `handle` is non-null (checked first) and `text` is a writable
        // buffer of the length passed with it.
        let have_detail = !handle.is_null()
            && unsafe { (self.last_error)(handle, text.as_mut_ptr(), text.len()) } == 0;
        let message = if have_detail {
            // SAFETY: UHD NUL-terminates what it wrote into `text`.
            unsafe { CStr::from_ptr(text.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        } else {
            "No UHD exception detail".into()
        };
        Error::Other(format!("UHD {op}: {status}, {message}"))
    }
}

/// Turn a UHD status code into a `Result`, naming the operation that failed.
fn check(status: Status, op: &str) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "UHD {op}: error {status} (check UHD runtime/device log)"
        )))
    }
}

/// What a live session is set to.
#[derive(Clone)]
pub struct UhdSettings {
    /// UHD device-selection string (`serial=..`, `type=b200`, `addr=..`), plus
    /// any transport options; the receive transport defaults are added by
    /// `uhd_device_args_with_rx_defaults`.
    pub device_args: String,
    pub center_hz: f64,
    pub rate: f64,
    /// Overall RX gain in dB.
    pub gain: f64,
    /// RX channel index on the device.
    pub channel: usize,
    /// Antenna port name, e.g. `RX2` or `TX/RX`.
    pub antenna: String,
    /// Analogue filter bandwidth in Hz; 0 leaves the device's default.
    pub bandwidth: f64,
    /// LNB local-oscillator frequency in Hz. The GUI adds it to `center_hz` to
    /// show the satellite-band frequency; the driver never sees it.
    pub lnb_lo_hz: f64,
}

impl Default for UhdSettings {
    fn default() -> Self {
        Self {
            // This selects one particular B210 by serial number; it has to be
            // changed to match any other unit.
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

/// Whether the comma-separated `key=value` list `args` already sets `key`
/// (UHD keys are case-insensitive).
fn has_device_arg(args: &str, key: &str) -> bool {
    args.split(',').any(|entry| {
        entry
            .split_once('=')
            .map(|(name, _)| name.trim().eq_ignore_ascii_case(key))
            .unwrap_or(false)
    })
}

// Use the measured B210 USB 3 transport defaults unless the user overrides them.
// The original device selector (serial, type, addr, etc.) is preserved verbatim.
fn uhd_device_args_with_rx_defaults(user_args: &str) -> String {
    let mut result = user_args.trim().trim_end_matches(',').to_owned();
    for (key, value) in [("num_recv_frames", "512"), ("recv_frame_size", "16360")] {
        if !has_device_arg(&result, key) {
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

/// Changes requested from another thread, applied by the source thread at the
/// start of its next read.
#[derive(Default)]
struct PendingControl {
    freq: Option<f64>,
    gain: Option<f64>,
}

/// Live controls on a running USRP, usable from the GUI thread while the
/// engine reads samples.
///
/// Rust note: the `Arc<Mutex<..>>` is shared between this handle and the
/// source. A poisoned lock (a thread panicked holding it) is simply ignored
/// here, since losing one tune request is harmless.
#[derive(Clone)]
pub struct UhdControl(Arc<Mutex<PendingControl>>);

impl UhdControl {
    /// Retune; applied before the next block is read.
    pub fn tune(&self, hz: f64) {
        if let Ok(mut p) = self.0.lock() {
            p.freq = Some(hz);
        }
    }

    /// Set the RX gain in dB.
    pub fn gain(&self, gain: f64) {
        if let Ok(mut p) = self.0.lock() {
            p.gain = Some(gain);
        }
    }
}

/// Sample rates above this are refused by `open`. The limit is this build's
/// choice, not UHD's.
const MAX_SAMPLE_RATE: f64 = 50_000_000.0;
/// Samples held in the receive buffer until the streamer reports its own
/// maximum packet size.
const DEFAULT_BLOCK_SAMPLES: usize = 16384;
/// Upper bound on the receive block, whatever the streamer reports.
const MAX_BLOCK_SAMPLES: usize = 65536;
/// `uhd_rx_streamer_recv` timeout for ordinary reads, in seconds.
const RECV_TIMEOUT_S: f64 = 0.4;
/// Tries per `read` before it gives up: transient overflows and empty receives
/// are skipped, because `Ok(0)` from [`IqSource::read`] would mean end of stream.
const READ_ATTEMPTS: u32 = 12;
/// Consecutive receive timeouts tolerated before the stream is declared dead.
const MAX_CONSECUTIVE_TIMEOUTS: u32 = 10;

/// Sweep frequencies accepted by [`UhdSource::sweep_capture`], in Hz.
const SWEEP_MIN_HZ: f64 = 50e6;
const SWEEP_MAX_HZ: f64 = 6e9;
/// How far the radio may land from a sweep step, as a fraction of the sample
/// rate, before the step is rejected.
const SWEEP_TUNE_TOLERANCE: f64 = 0.05;
/// Short `recv` timeout used while draining the old packets, in seconds. It
/// bounds the loop even when the queue is already empty.
const SWEEP_DRAIN_TIMEOUT_S: f64 = 0.002;
/// Most `recv` calls spent draining.
const SWEEP_DRAIN_CALLS: u32 = 128;
/// Let the synthesiser settle after a retune.
const SWEEP_SETTLE: Duration = Duration::from_millis(12);
/// Seconds of samples to throw away after restarting the stream.
const SWEEP_GUARD_S: f64 = 0.012;
/// Cap on the number of samples thrown away.
const SWEEP_MAX_DISCARD: usize = 524_288;
/// Reads allowed while discarding, and while collecting.
const SWEEP_DISCARD_READS: u32 = 128;
const SWEEP_COLLECT_READS: u32 = 32;
/// Samples returned for one sweep step.
const SWEEP_CAPTURE_SAMPLES: usize = 4096;

/// A live USRP receive stream.
///
/// The UHD handles start null and are filled in by `configure`; `Drop` releases
/// whichever were created, so a failed `configure` does not leak.
pub struct UhdSource {
    control: UhdControl,
    api: Api,
    usrp: Handle,
    stream: Handle,
    /// Metadata of the last `recv` (error code, timestamps).
    md: Handle,
    settings: UhdSettings,
    /// The sample rate the radio reports after being asked for `settings.rate`.
    real_rate: f64,
    /// Receive buffer. `Iq` is two `f32`s, the `fc32` layout UHD fills.
    buffer: Vec<Iq>,
    overflows: u64,
    consecutive_timeouts: u32,
}

// SAFETY: the UHD handles are used only on the engine's single source thread;
// the engine moves the source there once and never shares it.
unsafe impl Send for UhdSource {}

impl UhdSource {
    /// Open the radio, configure it and start streaming.
    pub fn open(settings: UhdSettings) -> Result<Self> {
        if !settings.center_hz.is_finite()
            || settings.center_hz <= 0.0
            || !settings.rate.is_finite()
            || settings.rate <= 0.0
            || !settings.gain.is_finite()
        {
            return Err(Error::Other("Invalid UHD radio settings".into()));
        }
        if settings.rate > MAX_SAMPLE_RATE {
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
            buffer: vec![Iq::new(0.0, 0.0); DEFAULT_BLOCK_SAMPLES],
            overflows: 0,
            consecutive_timeouts: 0,
        };
        // On failure `src` drops here and `Drop` releases the partial setup.
        src.configure()?;
        Ok(src)
    }

    pub fn control(&self) -> UhdControl {
        self.control.clone()
    }

    /// Make the device, set rate, gain, antenna, bandwidth and frequency, set up
    /// the RX streamer and start it.
    fn configure(&mut self) -> Result<()> {
        let args = CString::new(uhd_device_args_with_rx_defaults(&self.settings.device_args))
            .map_err(|e| Error::Other(e.to_string()))?;
        let c = self.settings.channel;
        // SAFETY (every UHD call in this function): all pointer arguments are
        // valid for the call, and UHD returns owned opaque handles that `Drop`
        // frees.
        let rc = unsafe { (self.api.usrp_make)(&mut self.usrp, args.as_ptr()) };
        if rc != 0 {
            return Err(self.api.error_with_detail(self.usrp, "open", rc));
        }
        check(
            unsafe { (self.api.set_rate)(self.usrp, self.settings.rate, c) },
            "set sample rate",
        )?;
        check(
            unsafe { (self.api.get_rate)(self.usrp, c, &mut self.real_rate) },
            "get actual sample rate",
        )?;
        // Rust note: `c""` is a C-string literal (NUL-terminated `&CStr`); the
        // empty name means the overall gain rather than one gain stage.
        check(
            unsafe { (self.api.set_gain)(self.usrp, self.settings.gain, c, c"".as_ptr()) },
            "set gain",
        )?;
        let antenna = CString::new(self.settings.antenna.as_str())
            .map_err(|e| Error::Other(e.to_string()))?;
        check(
            unsafe { (self.api.set_antenna)(self.usrp, antenna.as_ptr(), c) },
            "set antenna",
        )?;
        if self.settings.bandwidth > 0.0 {
            check(
                unsafe { (self.api.set_bandwidth)(self.usrp, self.settings.bandwidth, c) },
                "set bandwidth",
            )?;
        }
        self.tune_to(self.settings.center_hz, "tune")?;
        // Record where the radio actually landed, not what was asked for.
        let mut actual_freq = 0.0;
        check(
            unsafe { (self.api.get_freq)(self.usrp, c, &mut actual_freq) },
            "get actual frequency",
        )?;
        self.settings.center_hz = actual_freq;

        check(
            unsafe { (self.api.stream_make)(&mut self.stream) },
            "create stream",
        )?;
        // UHD takes the format names as mutable C strings, so give it its own
        // NUL-terminated byte buffers that outlive the call.
        let mut channel = c;
        let mut cpu = b"fc32\0".to_vec();
        let mut otw = b"sc16\0".to_vec();
        let mut extra = b"\0".to_vec();
        let mut stream_args = StreamArgs {
            cpu_format: cpu.as_mut_ptr().cast(),
            otw_format: otw.as_mut_ptr().cast(),
            args: extra.as_mut_ptr().cast(),
            channel_list: &mut channel,
            n_channels: 1,
        };
        check(
            unsafe { (self.api.get_stream)(self.usrp, &mut stream_args, self.stream) },
            "configure stream",
        )?;
        let mut max_samples = 0usize;
        check(
            unsafe { (self.api.max_samples)(self.stream, &mut max_samples) },
            "max buffer size",
        )?;
        if max_samples > 0 {
            self.buffer
                .resize(max_samples.min(MAX_BLOCK_SAMPLES), Iq::new(0.0, 0.0));
        }
        check(
            unsafe { (self.api.metadata_make)(&mut self.md) },
            "create metadata",
        )?;
        // SAFETY: the streamer was created above.
        unsafe { self.command(STREAM_MODE_START_CONTINUOUS) }
    }

    /// Tune the RX channel to `hz`; `op` names the step in any error.
    fn tune_to(&self, hz: f64, op: &str) -> Result<()> {
        let mut request = TuneRequest::auto(hz);
        let mut result = TuneResult::default();
        // SAFETY: `self.usrp` is the open radio; both structs live for the call.
        check(
            unsafe {
                (self.api.set_freq)(self.usrp, &mut request, self.settings.channel, &mut result)
            },
            op,
        )
    }

    /// Receive one block into `self.buffer`; returns `(status, samples)`.
    fn recv_block(&mut self, timeout_s: f64) -> (Status, usize) {
        // UHD wants an array of one buffer pointer per channel; there is one.
        let mut buffer_ptr = self.buffer.as_mut_ptr().cast::<c_void>();
        let mut received = 0usize;
        // SAFETY: the streamer and metadata handles are live; `buffer_ptr`
        // addresses `self.buffer.len()` samples of `fc32`, the layout of `Iq`.
        let status = unsafe {
            (self.api.recv)(
                self.stream,
                &mut buffer_ptr,
                self.buffer.len(),
                &mut self.md,
                timeout_s,
                false,
                &mut received,
            )
        };
        (status, received)
    }

    /// One frequency step of the B210 overview. The device and streamer are
    /// kept open across the entire sweep; only RX streaming is restarted.
    /// Unlike the generic sweep, no arbitrary fixed number of old RX blocks
    /// is assumed to delimit two tuner positions.
    ///
    /// Returns the frequency the radio actually tuned to; `out` receives
    /// `SWEEP_CAPTURE_SAMPLES` (4096) fresh samples taken there.
    pub fn sweep_capture(&mut self, hz: f64, out: &mut Vec<Iq>) -> Result<f64> {
        if !hz.is_finite() || !(SWEEP_MIN_HZ..=SWEEP_MAX_HZ).contains(&hz) {
            return Err(Error::Other(
                "UHD sweep frequency outside 50–6000 MHz".into(),
            ));
        }
        // RX must be stopped before changing the RF synthesizer.
        // SAFETY: the streamer exists once `open` has succeeded.
        unsafe {
            self.command(STREAM_MODE_STOP_CONTINUOUS)?;
        }
        // Drain the packets already in the host transport queue. A short UHD
        // timeout bounds this loop even when the queue is empty.
        for _ in 0..SWEEP_DRAIN_CALLS {
            let (status, received) = self.recv_block(SWEEP_DRAIN_TIMEOUT_S);
            if status != 0 || received == 0 {
                break;
            }
        }
        self.tune_to(hz, "sweep tune")?;
        let mut actual = 0f64;
        check(
            // SAFETY: `self.usrp` is the open radio.
            unsafe { (self.api.get_freq)(self.usrp, self.settings.channel, &mut actual) },
            "sweep actual frequency",
        )?;
        if !actual.is_finite() || (actual - hz).abs() > self.real_rate * SWEEP_TUNE_TOLERANCE {
            return Err(Error::Other(format!(
                "UHD sweep tune mismatch: requested {hz:.0} Hz, actual {actual:.0} Hz"
            )));
        }
        self.settings.center_hz = actual;
        std::thread::sleep(SWEEP_SETTLE);
        // SAFETY: as above.
        unsafe {
            self.command(STREAM_MODE_START_CONTINUOUS)?;
        }
        // Discard a time-based acquisition guard after starting the new stream.
        // Also discard at least two whole internal receive buffers.
        let discard = ((self.real_rate * SWEEP_GUARD_S).ceil() as usize)
            .max(self.buffer.len().saturating_mul(2))
            .min(SWEEP_MAX_DISCARD);
        let mut skipped = 0usize;
        let mut scratch = Vec::new();
        let mut reads = 0;
        while skipped < discard && reads < SWEEP_DISCARD_READS {
            reads += 1;
            skipped += self.read(&mut scratch)?;
        }
        if skipped < discard {
            return Err(Error::Other(
                "UHD sweep failed to acquire fresh samples".into(),
            ));
        }
        out.clear();
        reads = 0;
        while out.len() < SWEEP_CAPTURE_SAMPLES && reads < SWEEP_COLLECT_READS {
            reads += 1;
            let n = self.read(&mut scratch)?;
            if n > 0 {
                out.extend_from_slice(&scratch[..n.min(SWEEP_CAPTURE_SAMPLES - out.len())]);
            }
        }
        if out.len() < SWEEP_CAPTURE_SAMPLES {
            return Err(Error::Other("UHD sweep: insufficient IQ samples".into()));
        }
        Ok(actual)
    }

    /// Apply any retune or gain change queued by [`UhdControl`].
    fn apply_controls(&mut self) -> Result<()> {
        // Take the requests and drop the lock before touching the hardware.
        let (freq, gain) = {
            let mut p = self
                .control
                .0
                .lock()
                .map_err(|_| Error::Other("UHD control lock poisoned".into()))?;
            (p.freq.take(), p.gain.take())
        };
        if let Some(freq) = freq {
            self.tune_to(freq, "live tune")?;
            self.settings.center_hz = freq;
        }
        if let Some(gain) = gain {
            check(
                // SAFETY: `self.usrp` is the open radio.
                unsafe {
                    (self.api.set_gain)(self.usrp, gain, self.settings.channel, c"".as_ptr())
                },
                "live gain",
            )?;
            self.settings.gain = gain;
        }
        Ok(())
    }

    /// Issue a stream command (`STREAM_MODE_*`) to start or stop continuous
    /// streaming, effective immediately.
    ///
    /// # Safety
    ///
    /// `self.stream` must be a streamer made by `uhd_rx_streamer_make`, not null.
    unsafe fn command(&self, mode: i32) -> Result<()> {
        let cmd = StreamCmd {
            stream_mode: mode,
            num_samps: 0,
            stream_now: true,
            time_spec_full_secs: 0,
            time_spec_frac_secs: 0.0,
        };
        check(
            // SAFETY: the streamer is live (caller) and `cmd` lives for the call.
            unsafe { (self.api.issue_cmd)(self.stream, &cmd) },
            "stream command",
        )
    }
}

impl IqSource for UhdSource {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        self.apply_controls()?;
        // IqSource::read returning Ok(0) signals EOF, so retry transient overflows.
        for attempt in 0..READ_ATTEMPTS {
            let last_attempt = attempt + 1 == READ_ATTEMPTS;
            let (status, received) = self.recv_block(RECV_TIMEOUT_S);
            check(status, "receive")?;
            let mut md_error = 0i32;
            check(
                // SAFETY: `self.md` is the live metadata handle just filled by `recv`.
                unsafe { (self.api.metadata_error)(self.md, &mut md_error) },
                "receive metadata",
            )?;
            match md_error {
                0 => {}
                MD_ERROR_OVERFLOW => {
                    // UHD RX overflow: skip this block, keep the receiver alive.
                    self.overflows += 1;
                    if !last_attempt {
                        continue;
                    }
                    return Err(Error::Other(format!(
                        "UHD RX cannot recover from overflows ({} events). Lower the sample rate.",
                        self.overflows
                    )));
                }
                MD_ERROR_TIMEOUT => {
                    // UHD timeout: transient is not a fatal device failure.
                    self.consecutive_timeouts += 1;
                    if self.consecutive_timeouts < MAX_CONSECUTIVE_TIMEOUTS && !last_attempt {
                        continue;
                    }
                    return Err(Error::Other("UHD: ten consecutive RX timeouts".into()));
                }
                other => {
                    return Err(Error::Other(format!("UHD RX metadata error {other}")));
                }
            }
            self.consecutive_timeouts = 0;
            if received == 0 {
                continue;
            }
            out.clear();
            out.extend_from_slice(&self.buffer[..received.min(self.buffer.len())]);
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
        // Stop streaming, then free metadata, streamer and radio, in that order.
        // The DLL is unloaded afterwards, when the `api` field drops.
        //
        // SAFETY: each handle is released at most once, here, and only if
        // `configure` got far enough to create it.
        unsafe {
            if !self.stream.is_null() {
                let _ = self.command(STREAM_MODE_STOP_CONTINUOUS);
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
