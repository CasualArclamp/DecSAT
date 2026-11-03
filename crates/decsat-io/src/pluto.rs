//! ADALM-PLUTO (PlutoSDR, AD9361) receive-only source, over the libiio 0.x C ABI.
//!
//! `libiio.dll` is loaded at run time rather than linked, so no import library,
//! C++ compiler or extra crate is needed to build, and the portable exe starts
//! on a machine without the Pluto runtime. The DLL is looked for in a `Pluto`
//! folder beside the executable (with its dependent DLLs), or at the absolute
//! path in the `LIBIIO_DLL` environment variable. It has to be libiio 0.x:
//! 1.x renamed most of the calls used here.
//!
//! libiio model, as used below: a *context* (the connection, opened from a URI
//! such as `ip:192.168.2.1` or `usb:`) holds *devices*, which hold *channels*
//! with named *attributes*. The AD9361 shows up as two devices: `ad9361-phy`
//! (the radio's control: sample rate, bandwidth, gain, LO) and
//! `cf-ad9361-lpc` (the ADC data path). Samples come through a *buffer* on the
//! data device that is refilled one block at a time.
//!
//! Provenance: merged in from the "VMA" snapshot, see `docs/STATUS.md`
//! (2026-10-25). Not tested against real hardware by whoever restyled it.
#![cfg(windows)]

use std::ffi::{CString, c_char, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::{Arc, Mutex};

use decsat_core::{Error, Iq, Result};

use crate::IqSource;

/// An opaque libiio object (`struct iio_context *`, `iio_device *`,
/// `iio_channel *`, `iio_buffer *`) or a Win32 `HMODULE`. Null means "none".
type Handle = *mut c_void;

// Win32 loader calls (kernel32), declared by hand so no crate is needed.
//
// Rust note: since edition 2024 an `extern` block must be written
// `unsafe extern`, because the compiler cannot check that the declared
// signatures match the real functions. `"system"` is the platform's system
// calling convention (stdcall on 32-bit Windows, the usual one on 64-bit).
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(name: *const u16, file: Handle, flags: u32) -> Handle;
    fn GetProcAddress(module: Handle, name: *const u8) -> Handle;
    fn FreeLibrary(module: Handle) -> i32;
}

/// `LoadLibraryExW` flag (libloaderapi.h): search the DLL's own directory for
/// its dependencies. Avoids modifying the process-wide `PATH`.
const LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR: u32 = 0x0000_0100;
/// `LoadLibraryExW` flag (libloaderapi.h): keep System32 and the other safe
/// standard search locations as well.
const LOAD_LIBRARY_SEARCH_DEFAULT_DIRS: u32 = 0x0000_1000;

// Names fixed by the Pluto's Linux IIO drivers (ad9361 and its AXI ADC core).
/// Control device: sample rate, RF bandwidth, gain, LO.
const DEVICE_PHY: &str = "ad9361-phy";
/// Data device the RX buffer is created on.
const DEVICE_RX_DATA: &str = "cf-ad9361-lpc";
/// RX local oscillator: an *output* channel of the phy whose `frequency`
/// attribute is the tuning frequency in Hz.
const CHANNEL_RX_LO: &str = "altvoltage0";
/// RX path settings (`sampling_frequency`, `rf_bandwidth`, `hardwaregain`,
/// `gain_control_mode`): *input* channel 0 of the phy.
const CHANNEL_RX_PATH: &str = "voltage0";
/// The I and Q data channels on the data device.
const CHANNEL_RX_I: &str = "voltage0";
const CHANNEL_RX_Q: &str = "voltage1";

/// Complex samples per `iio_buffer_refill`.
const BUFFER_SAMPLES: usize = 16384;

/// The AD9361's ADC is 12 bits, delivered sign-extended in an `i16`, so full
/// scale is ±2048.
const ADC_FULL_SCALE: f32 = 2048.0;

/// Bytes in one complex sample of the buffer: an `i16` I followed by an `i16` Q.
const IQ_BYTES: usize = 4;

/// Tuning and sample-rate limits accepted here (Hz and S/s): the AD9361's
/// 70 MHz–6 GHz tuning range and 61.44 MS/s top rate. The firmware has the final
/// say, and may refuse values near the edges.
const MIN_CENTER_HZ: f64 = 70e6;
const MAX_CENTER_HZ: f64 = 6e9;
const MIN_SAMPLE_RATE: f64 = 520_834.0;
const MAX_SAMPLE_RATE: f64 = 61_440_000.0;

/// A loaded module, freed when dropped.
struct Module(Handle);

impl Drop for Module {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle came from a successful `LoadLibraryExW` and is
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
/// `module` must be a live module handle, and `T` must be the real signature of
/// the export (the compiler cannot check that).
unsafe fn symbol<T: Copy>(module: Handle, name: &'static [u8]) -> Result<T> {
    // SAFETY: `module` is valid (caller) and `name` is NUL-terminated.
    let address = unsafe { GetProcAddress(module, name.as_ptr()) };
    if address.is_null() {
        return Err(Error::Other(format!(
            "libiio missing export {} (requires libiio 0.x, not 1.x)",
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

/// The libiio 0.x entry points DecSAT uses (`iio.h`), resolved once from the
/// loaded DLL. Return codes: negative is an errno-style failure unless noted.
struct Api {
    /// Keeps the DLL loaded for as long as the function pointers below are used.
    _dll: Module,
    /// `iio_create_context_from_uri(uri)`: null on failure.
    create: unsafe extern "C" fn(*const c_char) -> Handle,
    /// `iio_context_destroy(ctx)`.
    destroy: unsafe extern "C" fn(Handle),
    /// `iio_context_find_device(ctx, name)`: null if absent.
    find_device: unsafe extern "C" fn(Handle, *const c_char) -> Handle,
    /// `iio_device_find_channel(dev, name, is_output)`: null if absent.
    find_channel: unsafe extern "C" fn(Handle, *const c_char, bool) -> Handle,
    /// `iio_channel_attr_write_longlong(chn, attr, value)`.
    write_ll: unsafe extern "C" fn(Handle, *const c_char, i64) -> i32,
    /// `iio_channel_attr_write(chn, attr, text)`: returns bytes written.
    write_str: unsafe extern "C" fn(Handle, *const c_char, *const c_char) -> isize,
    /// `iio_channel_attr_read_longlong(chn, attr, &value)`.
    read_ll: unsafe extern "C" fn(Handle, *const c_char, *mut i64) -> i32,
    /// `iio_channel_enable(chn)`: include the channel in buffer data.
    enable: unsafe extern "C" fn(Handle),
    /// `iio_device_create_buffer(dev, samples, cyclic)`: null on failure.
    create_buffer: unsafe extern "C" fn(Handle, usize, bool) -> Handle,
    /// `iio_buffer_destroy(buf)`.
    destroy_buffer: unsafe extern "C" fn(Handle),
    /// `iio_buffer_refill(buf)`: blocks for a full buffer, returns bytes read.
    refill: unsafe extern "C" fn(Handle) -> isize,
    /// `iio_buffer_first(buf, chn)`: address of the channel's first sample.
    first: unsafe extern "C" fn(Handle, Handle) -> Handle,
    /// `iio_buffer_end(buf)`: address one past the last sample.
    end: unsafe extern "C" fn(Handle) -> Handle,
    /// `iio_buffer_step(buf)`: bytes between one sample and the next.
    step: unsafe extern "C" fn(Handle) -> isize,
}

impl Api {
    /// Load `libiio.dll` and resolve every export.
    fn load() -> Result<Self> {
        // Always resolve Pluto's DLL relative to DecSAT's executable, not the current working
        // directory. Keeping Pluto's runtime files outside the executable directory avoids
        // shadowing the libusb DLL expected by UHD.
        let exe_dir = std::env::current_exe()
            .map_err(|e| Error::Other(format!("Cannot determine DecSAT executable path: {e}")))?
            .parent()
            .ok_or_else(|| Error::Other("DecSAT executable has no parent directory".into()))?
            .to_path_buf();
        let path = std::env::var_os("LIBIIO_DLL")
            .map(PathBuf::from)
            .unwrap_or_else(|| exe_dir.join("Pluto").join("libiio.dll"));
        if !path.is_absolute() {
            return Err(Error::Other(format!(
                "LIBIIO_DLL must be an absolute path (received {})",
                path.display()
            )));
        }
        if !path.is_file() {
            return Err(Error::Other(format!(
                "Pluto runtime missing: {}. Place libiio.dll and its dependent DLLs in the Pluto \
                 subfolder beside decsat-gui.exe.",
                path.display()
            )));
        }
        // Win32 wants a NUL-terminated UTF-16 string.
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let dll = unsafe {
            LoadLibraryExW(
                wide.as_ptr(),
                ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        };
        if dll.is_null() {
            return Err(Error::Other(format!(
                "Cannot load Pluto runtime {} (or a dependent DLL): {}. \
                 Keep all matching libiio 0.x runtime DLLs together inside Pluto.",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        // From here `module` owns the DLL, so an early `?` frees it.
        let module = Module(dll);
        // Rust note: the target type of `symbol` is inferred from the struct
        // field each result is assigned to, so each signature is written once,
        // in `Api` above.
        macro_rules! sym {
            ($name:literal) => {
                // SAFETY: `dll` is live (held by `module`) and every field type
                // is the C signature of the export it is filled from.
                unsafe { symbol(dll, concat!($name, "\0").as_bytes())? }
            };
        }
        Ok(Self {
            create: sym!("iio_create_context_from_uri"),
            destroy: sym!("iio_context_destroy"),
            find_device: sym!("iio_context_find_device"),
            find_channel: sym!("iio_device_find_channel"),
            write_ll: sym!("iio_channel_attr_write_longlong"),
            write_str: sym!("iio_channel_attr_write"),
            read_ll: sym!("iio_channel_attr_read_longlong"),
            enable: sym!("iio_channel_enable"),
            create_buffer: sym!("iio_device_create_buffer"),
            destroy_buffer: sym!("iio_buffer_destroy"),
            refill: sym!("iio_buffer_refill"),
            first: sym!("iio_buffer_first"),
            end: sym!("iio_buffer_end"),
            step: sym!("iio_buffer_step"),
            _dll: module,
        })
    }
}

/// What a live session is set to.
#[derive(Clone)]
pub struct PlutoSettings {
    /// libiio URI, e.g. `ip:192.168.2.1` or `usb:`.
    pub uri: String,
    pub center_hz: f64,
    /// Sample rate in S/s; the radio may pick a nearby value (see
    /// [`PlutoSource::sample_rate`]).
    pub rate: f64,
    /// Hardware gain in dB (manual gain control).
    pub gain: f64,
    /// RF filter bandwidth in Hz.
    pub bandwidth: f64,
    /// LNB local-oscillator frequency in Hz. The GUI adds it to `center_hz` to
    /// show the satellite-band frequency; the driver never sees it.
    pub lnb_lo_hz: f64,
}

impl Default for PlutoSettings {
    fn default() -> Self {
        Self {
            uri: "ip:192.168.2.1".into(),
            center_hz: 1_100_000_000.0,
            rate: 4_000_000.0,
            gain: 35.0,
            bandwidth: 3_000_000.0,
            lnb_lo_hz: 0.0,
        }
    }
}

/// Changes requested from another thread, applied by the source thread at the
/// start of its next read.
#[derive(Default)]
struct Pending {
    freq: Option<f64>,
    gain: Option<f64>,
}

/// Live controls on a running Pluto, usable from the GUI thread while the
/// engine reads samples.
///
/// Rust note: the `Arc<Mutex<..>>` is shared between this handle and the
/// source. A poisoned lock (a thread panicked holding it) is simply ignored
/// here, since losing one tune request is harmless.
#[derive(Clone)]
pub struct PlutoControl(Arc<Mutex<Pending>>);

impl PlutoControl {
    /// Retune; applied before the next block is read.
    pub fn tune(&self, hz: f64) {
        if let Ok(mut p) = self.0.lock() {
            p.freq = Some(hz);
        }
    }

    /// Set the hardware gain in dB.
    pub fn gain(&self, db: f64) {
        if let Ok(mut p) = self.0.lock() {
            p.gain = Some(db);
        }
    }
}

/// A live PlutoSDR receive stream.
///
/// The libiio handles start null and are filled in by `init`; `Drop` releases
/// whichever were created, so a failed `init` does not leak.
pub struct PlutoSource {
    api: Api,
    ctx: Handle,
    buf: Handle,
    /// I and Q data channels, and the LO and gain/rate channels, found by name.
    rx_i: Handle,
    rx_q: Handle,
    lo: Handle,
    gain_chan: Handle,
    controls: PlutoControl,
    settings: PlutoSettings,
    /// The sample rate the radio reports after being asked for `settings.rate`.
    actual_rate: f64,
}

// SAFETY: every libiio handle is owned and used exclusively by the source
// thread; the engine moves the source there once and never shares it.
unsafe impl Send for PlutoSource {}

/// `s` as a C string, for libiio's `const char *` arguments. Fails only if `s`
/// has an interior NUL.
fn cstring(s: &str) -> Result<CString> {
    CString::new(s).map_err(|e| Error::Other(e.to_string()))
}

impl PlutoSource {
    /// Connect to the radio and start filling its RX buffer.
    pub fn open(settings: PlutoSettings) -> Result<Self> {
        if !(MIN_CENTER_HZ..=MAX_CENTER_HZ).contains(&settings.center_hz)
            || !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&settings.rate)
        {
            return Err(Error::Other(
                "Pluto tuning must be 70–6000 MHz and sample rate >= 520834 S/s; \
                 actual supported limits depend on firmware"
                    .into(),
            ));
        }
        let api = Api::load()?;
        let mut src = Self {
            api,
            ctx: ptr::null_mut(),
            buf: ptr::null_mut(),
            rx_i: ptr::null_mut(),
            rx_q: ptr::null_mut(),
            lo: ptr::null_mut(),
            gain_chan: ptr::null_mut(),
            controls: PlutoControl(Arc::new(Mutex::new(Pending::default()))),
            actual_rate: 0.0,
            settings,
        };
        // On failure `src` drops here and `Drop` releases the partial setup.
        src.init()?;
        Ok(src)
    }

    pub fn control(&self) -> PlutoControl {
        self.controls.clone()
    }

    /// Open the context, find the channels, configure the RX path and create the
    /// buffer, in that order.
    fn init(&mut self) -> Result<()> {
        // SAFETY (every libiio call in this function): the handle arguments are
        // either the non-null results of earlier calls (checked) or `self.ctx`,
        // and each `cstring(..).as_ptr()` lives to the end of its statement.
        self.ctx = unsafe { (self.api.create)(cstring(&self.settings.uri)?.as_ptr()) };
        if self.ctx.is_null() {
            return Err(Error::Other(format!(
                "Pluto libiio context could not open URI {}",
                self.settings.uri
            )));
        }
        let phy = unsafe { (self.api.find_device)(self.ctx, cstring(DEVICE_PHY)?.as_ptr()) };
        let rx = unsafe { (self.api.find_device)(self.ctx, cstring(DEVICE_RX_DATA)?.as_ptr()) };
        if phy.is_null() || rx.is_null() {
            return Err(Error::Other(
                "Pluto missing ad9361-phy or cf-ad9361-lpc device".into(),
            ));
        }
        self.lo = unsafe { (self.api.find_channel)(phy, cstring(CHANNEL_RX_LO)?.as_ptr(), true) };
        self.gain_chan =
            unsafe { (self.api.find_channel)(phy, cstring(CHANNEL_RX_PATH)?.as_ptr(), false) };
        self.rx_i = unsafe { (self.api.find_channel)(rx, cstring(CHANNEL_RX_I)?.as_ptr(), false) };
        self.rx_q = unsafe { (self.api.find_channel)(rx, cstring(CHANNEL_RX_Q)?.as_ptr(), false) };
        if [self.lo, self.gain_chan, self.rx_i, self.rx_q]
            .iter()
            .any(|v| v.is_null())
        {
            return Err(Error::Other("Pluto RX channel not found".into()));
        }

        // Ask for the sample rate, then read back what the radio settled on.
        self.write_ll(
            self.gain_chan,
            "sampling_frequency",
            self.settings.rate as i64,
        )?;
        let mut actual = 0i64;
        let rc = unsafe {
            (self.api.read_ll)(
                self.gain_chan,
                cstring("sampling_frequency")?.as_ptr(),
                &mut actual,
            )
        };
        if rc < 0 {
            return Err(Error::Other("Pluto sampling_frequency read failed".into()));
        }
        self.actual_rate = actual as f64;

        self.write_ll(
            self.gain_chan,
            "rf_bandwidth",
            self.settings.bandwidth as i64,
        )?;
        self.write_str(self.gain_chan, "gain_control_mode", "manual")?;
        // `hardwaregain` takes a whole number of dB; some firmware only accepts
        // the text form, so fall back to that if the integer write is refused.
        self.write_ll(
            self.gain_chan,
            "hardwaregain",
            self.settings.gain.round() as i64,
        )
        .or_else(|_| {
            self.write_str(
                self.gain_chan,
                "hardwaregain",
                &format!("{:.0}", self.settings.gain),
            )
        })?;
        self.write_ll(self.lo, "frequency", self.settings.center_hz as i64)?;

        unsafe {
            (self.api.enable)(self.rx_i);
            (self.api.enable)(self.rx_q);
        }
        self.buf = unsafe { (self.api.create_buffer)(rx, BUFFER_SAMPLES, false) };
        if self.buf.is_null() {
            return Err(Error::Other(
                "Pluto create RX buffer failed (is device busy?)".into(),
            ));
        }
        Ok(())
    }

    /// Write an integer channel attribute.
    fn write_ll(&self, ch: Handle, attr: &str, value: i64) -> Result<()> {
        // SAFETY: `ch` is a non-null channel of the open context (checked in
        // `init`); the C string lives to the end of the statement.
        let n = unsafe { (self.api.write_ll)(ch, cstring(attr)?.as_ptr(), value) };
        if n < 0 {
            Err(Error::Other(format!("Pluto {attr} write failed: {n}")))
        } else {
            Ok(())
        }
    }

    /// Write a text channel attribute.
    fn write_str(&self, ch: Handle, attr: &str, value: &str) -> Result<()> {
        // SAFETY: as `write_ll`; both C strings live to the end of the statement.
        let n =
            unsafe { (self.api.write_str)(ch, cstring(attr)?.as_ptr(), cstring(value)?.as_ptr()) };
        if n < 0 {
            Err(Error::Other(format!("Pluto {attr} write failed: {n}")))
        } else {
            Ok(())
        }
    }

    /// Apply any retune or gain change queued by [`PlutoControl`].
    fn apply(&mut self) -> Result<()> {
        // Take the requests and drop the lock before touching the hardware.
        let (freq, gain) = {
            let mut p = self
                .controls
                .0
                .lock()
                .map_err(|_| Error::Other("Pluto control lock poisoned".into()))?;
            (p.freq.take(), p.gain.take())
        };
        if let Some(f) = freq {
            self.write_ll(self.lo, "frequency", f as i64)?;
            self.settings.center_hz = f;
        }
        if let Some(g) = gain {
            self.write_str(self.gain_chan, "hardwaregain", &format!("{g:.0}"))?;
            self.settings.gain = g;
        }
        Ok(())
    }
}

impl IqSource for PlutoSource {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        self.apply()?;
        // SAFETY (every libiio call here): `self.buf` is the buffer made in
        // `init`, and `rx_i` is a channel enabled on it.
        let n = unsafe { (self.api.refill)(self.buf) };
        if n < 0 {
            return Err(Error::Other(format!("Pluto RX buffer refill failed: {n}")));
        }
        // The step is the bytes per sample across all enabled channels: 4 for I
        // and Q as two `i16`.
        let step = unsafe { (self.api.step)(self.buf) };
        if step < IQ_BYTES as isize {
            return Err(Error::Other(format!(
                "Pluto unexpected sample stride: {step}"
            )));
        }
        let mut p = unsafe { (self.api.first)(self.buf, self.rx_i) } as *const u8;
        let end = unsafe { (self.api.end)(self.buf) } as usize;
        out.clear();
        // Walk from the first sample to the end of the buffer, one stride at a
        // time. The comparison is done on addresses (as `usize`) so no pointer
        // is ever formed past the end.
        while (p as usize).saturating_add(IQ_BYTES) <= end {
            // The data is little-endian `i16`s and not necessarily aligned, so
            // `read_unaligned` rather than a plain dereference.
            //
            // SAFETY: the loop condition keeps the four bytes at `p` inside the
            // buffer libiio just filled.
            let i = unsafe { i16::from_le(ptr::read_unaligned(p as *const i16)) };
            let q = unsafe { i16::from_le(ptr::read_unaligned(p.add(2) as *const i16)) };
            out.push(Iq::new(
                i as f32 / ADC_FULL_SCALE,
                q as f32 / ADC_FULL_SCALE,
            ));
            // SAFETY: `step` is the buffer's own stride; the loop condition
            // re-checks the bounds before the next read.
            p = unsafe { p.add(step as usize) };
        }
        Ok(out.len())
    }

    fn sample_rate(&self) -> f64 {
        self.actual_rate
    }

    fn center_freq(&self) -> f64 {
        self.settings.center_hz
    }

    fn describe(&self) -> String {
        format!(
            "PlutoSDR {} @ {:.3} MHz, {:.3} MS/s",
            self.settings.uri,
            self.settings.center_hz / 1e6,
            self.actual_rate / 1e6
        )
    }

    fn is_live(&self) -> bool {
        true
    }
}

impl Drop for PlutoSource {
    fn drop(&mut self) {
        // Buffer first, then the context it belongs to. The DLL is unloaded
        // afterwards, when the `api` field drops.
        //
        // SAFETY: each handle is released at most once, here, and only if `init`
        // got far enough to create it.
        unsafe {
            if !self.buf.is_null() {
                (self.api.destroy_buffer)(self.buf);
            }
            if !self.ctx.is_null() {
                (self.api.destroy)(self.ctx);
            }
        }
    }
}
