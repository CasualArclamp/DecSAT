//! PlutoSDR RX via libiio 0.x C ABI. Windows dynamically loads libiio.dll.
//! No external Rust crates, import library, or C++ compiler is required.
#![cfg(windows)]
use crate::IqSource;
use decsat_core::{Error, Iq, Result};
use std::{
    ffi::{CString, c_char, c_void},
    os::windows::ffi::OsStrExt,
    path::PathBuf,
    ptr,
    sync::{Arc, Mutex},
};

type H = *mut c_void;
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(s: *const u16, file: H, flags: u32) -> H;
    fn GetProcAddress(h: H, s: *const u8) -> H;
    fn FreeLibrary(h: H) -> i32;
}
struct Module(H);
impl Drop for Module {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                FreeLibrary(self.0);
            }
        }
    }
}
unsafe fn symbol<T: Copy>(h: H, name: &'static [u8]) -> Result<T> {
    let p = unsafe { GetProcAddress(h, name.as_ptr()) };
    if p.is_null() {
        return Err(Error::Other(format!(
            "libiio missing export {} (requires libiio 0.x, not 1.x)",
            String::from_utf8_lossy(&name[..name.len() - 1])
        )));
    }
    Ok(unsafe { std::mem::transmute_copy(&p) })
}
struct Api {
    _dll: Module,
    create: unsafe extern "C" fn(*const c_char) -> H,
    destroy: unsafe extern "C" fn(H),
    find_device: unsafe extern "C" fn(H, *const c_char) -> H,
    find_channel: unsafe extern "C" fn(H, *const c_char, bool) -> H,
    write_ll: unsafe extern "C" fn(H, *const c_char, i64) -> i32,
    write_str: unsafe extern "C" fn(H, *const c_char, *const c_char) -> isize,
    read_ll: unsafe extern "C" fn(H, *const c_char, *mut i64) -> i32,
    enable: unsafe extern "C" fn(H),
    create_buffer: unsafe extern "C" fn(H, usize, bool) -> H,
    destroy_buffer: unsafe extern "C" fn(H),
    refill: unsafe extern "C" fn(H) -> isize,
    first: unsafe extern "C" fn(H, H) -> H,
    end: unsafe extern "C" fn(H) -> H,
    step: unsafe extern "C" fn(H) -> isize,
}
impl Api {
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
                "Pluto runtime missing: {}. Place libiio.dll and its dependent DLLs in the Pluto subfolder beside decsat-gui.exe.",
                path.display()
            )));
        }
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR (0x100) searches this DLL's directory for
        // dependencies; LOAD_LIBRARY_SEARCH_DEFAULT_DIRS (0x1000) retains System32 and
        // other safe standard search locations. This avoids modifying process-wide PATH.
        const LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR: u32 = 0x00000100;
        const LOAD_LIBRARY_SEARCH_DEFAULT_DIRS: u32 = 0x00001000;
        let dll = unsafe {
            LoadLibraryExW(
                wide.as_ptr(),
                ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        };
        if dll.is_null() {
            return Err(Error::Other(format!(
                "Cannot load Pluto runtime {} (or a dependent DLL): {}. Keep all matching libiio 0.x runtime DLLs together inside Pluto.",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        let module = Module(dll);
        macro_rules! sym {
            ($name:literal,$ty:ty) => {
                unsafe { symbol::<$ty>(dll, concat!($name, "\0").as_bytes())? }
            };
        }
        Ok(Self {
            create: sym!(
                "iio_create_context_from_uri",
                unsafe extern "C" fn(*const c_char) -> H
            ),
            destroy: sym!("iio_context_destroy", unsafe extern "C" fn(H)),
            find_device: sym!(
                "iio_context_find_device",
                unsafe extern "C" fn(H, *const c_char) -> H
            ),
            find_channel: sym!(
                "iio_device_find_channel",
                unsafe extern "C" fn(H, *const c_char, bool) -> H
            ),
            write_ll: sym!(
                "iio_channel_attr_write_longlong",
                unsafe extern "C" fn(H, *const c_char, i64) -> i32
            ),
            write_str: sym!(
                "iio_channel_attr_write",
                unsafe extern "C" fn(H, *const c_char, *const c_char) -> isize
            ),
            read_ll: sym!(
                "iio_channel_attr_read_longlong",
                unsafe extern "C" fn(H, *const c_char, *mut i64) -> i32
            ),
            enable: sym!("iio_channel_enable", unsafe extern "C" fn(H)),
            create_buffer: sym!(
                "iio_device_create_buffer",
                unsafe extern "C" fn(H, usize, bool) -> H
            ),
            destroy_buffer: sym!("iio_buffer_destroy", unsafe extern "C" fn(H)),
            refill: sym!("iio_buffer_refill", unsafe extern "C" fn(H) -> isize),
            first: sym!("iio_buffer_first", unsafe extern "C" fn(H, H) -> H),
            end: sym!("iio_buffer_end", unsafe extern "C" fn(H) -> H),
            step: sym!("iio_buffer_step", unsafe extern "C" fn(H) -> isize),
            _dll: module,
        })
    }
}
#[derive(Clone)]
pub struct PlutoSettings {
    pub uri: String,
    pub center_hz: f64,
    pub rate: f64,
    pub gain: f64,
    pub bandwidth: f64,
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
#[derive(Default)]
struct Pending {
    freq: Option<f64>,
    gain: Option<f64>,
}
#[derive(Clone)]
pub struct PlutoControl(Arc<Mutex<Pending>>);
impl PlutoControl {
    pub fn tune(&self, hz: f64) {
        if let Ok(mut p) = self.0.lock() {
            p.freq = Some(hz);
        }
    }
    pub fn gain(&self, db: f64) {
        if let Ok(mut p) = self.0.lock() {
            p.gain = Some(db);
        }
    }
}
pub struct PlutoSource {
    api: Api,
    ctx: H,
    buf: H,
    rx_i: H,
    rx_q: H,
    lo: H,
    gain_chan: H,
    controls: PlutoControl,
    settings: PlutoSettings,
    actual_rate: f64,
}
unsafe impl Send for PlutoSource {} // Every libiio handle is owned and used exclusively by the source thread.
fn c(s: &str) -> Result<CString> {
    CString::new(s).map_err(|e| Error::Other(e.to_string()))
}
impl PlutoSource {
    pub fn open(settings: PlutoSettings) -> Result<Self> {
        if !(70e6..=6e9).contains(&settings.center_hz)
            || !(520_834.0..=61_440_000.0).contains(&settings.rate)
        {
            return Err(Error::Other("Pluto tuning must be 70–6000 MHz and sample rate >= 520834 S/s; actual supported limits depend on firmware".into()));
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
        src.init()?;
        Ok(src)
    }
    pub fn control(&self) -> PlutoControl {
        self.controls.clone()
    }
    fn init(&mut self) -> Result<()> {
        unsafe {
            self.ctx = (self.api.create)(c(&self.settings.uri)?.as_ptr());
            if self.ctx.is_null() {
                return Err(Error::Other(format!(
                    "Pluto libiio context could not open URI {}",
                    self.settings.uri
                )));
            }
            let phy = (self.api.find_device)(self.ctx, c("ad9361-phy")?.as_ptr());
            let rx = (self.api.find_device)(self.ctx, c("cf-ad9361-lpc")?.as_ptr());
            if phy.is_null() || rx.is_null() {
                return Err(Error::Other(
                    "Pluto missing ad9361-phy or cf-ad9361-lpc device".into(),
                ));
            }
            self.lo = (self.api.find_channel)(phy, c("altvoltage0")?.as_ptr(), true);
            self.gain_chan = (self.api.find_channel)(phy, c("voltage0")?.as_ptr(), false);
            self.rx_i = (self.api.find_channel)(rx, c("voltage0")?.as_ptr(), false);
            self.rx_q = (self.api.find_channel)(rx, c("voltage1")?.as_ptr(), false);
            if [self.lo, self.gain_chan, self.rx_i, self.rx_q]
                .iter()
                .any(|v| v.is_null())
            {
                return Err(Error::Other("Pluto RX channel not found".into()));
            }
            self.write_ll(
                self.gain_chan,
                "sampling_frequency",
                self.settings.rate as i64,
            )?;
            let mut actual = 0i64;
            if (self.api.read_ll)(
                self.gain_chan,
                c("sampling_frequency")?.as_ptr(),
                &mut actual,
            ) < 0
            {
                return Err(Error::Other("Pluto sampling_frequency read failed".into()));
            }
            self.actual_rate = actual as f64;
            self.write_ll(
                self.gain_chan,
                "rf_bandwidth",
                self.settings.bandwidth as i64,
            )?;
            self.write_str(self.gain_chan, "gain_control_mode", "manual")?;
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
            (self.api.enable)(self.rx_i);
            (self.api.enable)(self.rx_q);
            self.buf = (self.api.create_buffer)(rx, 16384, false);
            if self.buf.is_null() {
                return Err(Error::Other(
                    "Pluto create RX buffer failed (is device busy?)".into(),
                ));
            }
            Ok(())
        }
    }
    fn write_ll(&self, ch: H, attr: &str, value: i64) -> Result<()> {
        let n = unsafe { (self.api.write_ll)(ch, c(attr)?.as_ptr(), value) };
        if n < 0 {
            Err(Error::Other(format!("Pluto {attr} write failed: {n}")))
        } else {
            Ok(())
        }
    }
    fn write_str(&self, ch: H, attr: &str, value: &str) -> Result<()> {
        let n = unsafe { (self.api.write_str)(ch, c(attr)?.as_ptr(), c(value)?.as_ptr()) };
        if n < 0 {
            Err(Error::Other(format!("Pluto {attr} write failed: {n}")))
        } else {
            Ok(())
        }
    }
    fn apply(&mut self) -> Result<()> {
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
        let n = unsafe { (self.api.refill)(self.buf) };
        if n < 0 {
            return Err(Error::Other(format!("Pluto RX buffer refill failed: {n}")));
        }
        let step = unsafe { (self.api.step)(self.buf) };
        if step < 4 {
            return Err(Error::Other(format!(
                "Pluto unexpected sample stride: {step}"
            )));
        }
        let mut p = unsafe { (self.api.first)(self.buf, self.rx_i) } as *const u8;
        let end = unsafe { (self.api.end)(self.buf) } as usize;
        out.clear();
        while (p as usize).saturating_add(4) <= end {
            let i = unsafe { i16::from_le(ptr::read_unaligned(p as *const i16)) };
            let q = unsafe { i16::from_le(ptr::read_unaligned(p.add(2) as *const i16)) };
            out.push(Iq::new(i as f32 / 2048.0, q as f32 / 2048.0));
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
        unsafe {
            if !self.buf.is_null() {
                (self.api.destroy_buffer)(self.buf)
            };
            if !self.ctx.is_null() {
                (self.api.destroy)(self.ctx)
            };
        }
    }
}
