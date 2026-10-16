// made by CasualArclamp
//! AAC through libxaac (`decsat-xaac-sys`): AAC-LC, and HE-AAC v1 and v2 with
//! their SBR and parametric stereo decoded — the full bandwidth, not just the
//! core. Access units go in one ADTS frame at a time, so SBR and PS are found
//! the way every player finds them in ADTS: implicitly, from the extension
//! data in the frames.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ffi::c_void;
use std::ptr::NonNull;

use decsat_xaac_sys as x;

/// One block of memory handed to libxaac, freed with the decoder.
struct Mem {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl Mem {
    fn new(size: usize, align: usize) -> Result<Mem, String> {
        let align = align.max(16).next_power_of_two();
        let layout = Layout::from_size_align(size.max(1), align).map_err(|e| e.to_string())?;
        // SAFETY: the layout has a non-zero size.
        let p = unsafe { alloc_zeroed(layout) };
        NonNull::new(p)
            .map(|ptr| Mem { ptr, layout })
            .ok_or_else(|| "libxaac: out of memory".to_string())
    }
}

impl Drop for Mem {
    fn drop(&mut self) {
        // SAFETY: allocated in `Mem::new` with this layout, freed only here.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

/// What the decoder reports about the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct XaacInfo {
    /// The output rate (twice the core's with SBR).
    pub rate: u32,
    pub channels: u32,
    /// 0 no SBR, 1 SBR (HE-AAC), 2 SBR and PS (HE-AAC v2).
    pub sbr: u32,
}

/// A libxaac decoder fed ADTS frames.
pub struct XaacDecoder {
    api: Mem,
    /// The memory tables and memories (kept alive; libxaac holds pointers).
    _mems: Vec<Mem>,
    input: (NonNull<u8>, usize),
    output: NonNull<u8>,
    /// The stream's header has been read (INIT_PROCESS done).
    ready: bool,
    /// Bytes given but not yet used.
    pending: Vec<u8>,
    pub info: XaacInfo,
}

// SAFETY: the decoder's memory belongs to this value alone and libxaac keeps
// no per-thread or shared state for it, so it may move between threads.
unsafe impl Send for XaacDecoder {}

/// The error for a failed call, if it failed fatally.
fn check(what: &str, code: x::IaErrorCode) -> Result<x::IaErrorCode, String> {
    if code & x::IA_FATAL_ERROR != 0 {
        Err(format!("libxaac: {what} failed (0x{code:08X})"))
    } else {
        Ok(code)
    }
}

impl XaacDecoder {
    /// A decoder for ADTS; `short_frames` for 960-sample frames.
    pub fn new(short_frames: bool) -> Result<XaacDecoder, String> {
        let mut size: i32 = 0;
        // SAFETY: GET_API_SIZE takes no object and writes one 32-bit value.
        check("API size", unsafe {
            x::ixheaacd_dec_api(
                std::ptr::null_mut(),
                x::IA_API_CMD_GET_API_SIZE,
                0,
                (&mut size as *mut i32).cast(),
            )
        })?;
        let api = Mem::new(size as usize, 16)?;
        let mut d = XaacDecoder {
            input: (api.ptr, 0),
            output: api.ptr,
            api,
            _mems: Vec::new(),
            ready: false,
            pending: Vec::new(),
            info: XaacInfo::default(),
        };
        d.call(
            "pre-config",
            x::IA_API_CMD_INIT,
            x::IA_CMD_TYPE_INIT_API_PRE_CONFIG_PARAMS,
            std::ptr::null_mut(),
        )?;
        d.set(x::IA_XHEAAC_DEC_CONFIG_PARAM_MP4FLAG, 0)?; // ADTS
        if short_frames {
            d.set(x::IA_XHEAAC_DEC_CONFIG_PARAM_FRAMELENGTH_FLAG, 1)?;
        }
        let tabs = d.get_cmd(x::IA_API_CMD_GET_MEMTABS_SIZE, 0)?;
        let tabs = Mem::new(tabs as usize, 16)?;
        d.call(
            "memory tables",
            x::IA_API_CMD_SET_MEMTABS_PTR,
            0,
            tabs.ptr.as_ptr().cast(),
        )?;
        d._mems.push(tabs);
        d.call(
            "post-config",
            x::IA_API_CMD_INIT,
            x::IA_CMD_TYPE_INIT_API_POST_CONFIG_PARAMS,
            std::ptr::null_mut(),
        )?;
        let n = d.get_cmd(x::IA_API_CMD_GET_N_MEMTABS, 0)?;
        for i in 0..n {
            let size = d.get_cmd(x::IA_API_CMD_GET_MEM_INFO_SIZE, i)?;
            let align = d.get_cmd(x::IA_API_CMD_GET_MEM_INFO_ALIGNMENT, i)?;
            let kind = d.get_cmd(x::IA_API_CMD_GET_MEM_INFO_TYPE, i)?;
            let m = Mem::new(size as usize, align as usize)?;
            d.call(
                "memory",
                x::IA_API_CMD_SET_MEM_PTR,
                i,
                m.ptr.as_ptr().cast(),
            )?;
            match kind {
                x::IA_MEMTYPE_INPUT => d.input = (m.ptr, size as usize),
                x::IA_MEMTYPE_OUTPUT => d.output = m.ptr,
                _ => {}
            }
            d._mems.push(m);
        }
        if d.input.1 == 0 {
            return Err("libxaac: no input buffer".into());
        }
        Ok(d)
    }

    fn call(&mut self, what: &str, cmd: i32, idx: i32, value: *mut c_void) -> Result<i32, String> {
        // SAFETY: the object is live and `value` points where `cmd` expects
        // (a 32-bit value, or memory of the size libxaac asked for).
        check(what, unsafe {
            x::ixheaacd_dec_api(self.api.ptr.as_ptr().cast(), cmd, idx, value)
        })
    }

    /// A 32-bit value read with `cmd`.
    fn get_cmd(&mut self, cmd: i32, idx: i32) -> Result<i32, String> {
        let mut v: i32 = 0;
        self.call("query", cmd, idx, (&mut v as *mut i32).cast())?;
        Ok(v)
    }

    fn set(&mut self, param: i32, mut v: i32) -> Result<(), String> {
        self.call(
            "configuration",
            x::IA_API_CMD_SET_CONFIG_PARAM,
            param,
            (&mut v as *mut i32).cast(),
        )
        .map(|_| ())
    }

    /// Hand libxaac what is pending (as much as its buffer holds).
    fn load(&mut self) -> Result<(), String> {
        let n = self.pending.len().min(self.input.1);
        // SAFETY: the input buffer holds `input.1` bytes; `n` is no more.
        unsafe {
            std::ptr::copy_nonoverlapping(self.pending.as_ptr(), self.input.0.as_ptr(), n);
        }
        let mut n = n as i32;
        self.call(
            "input",
            x::IA_API_CMD_SET_INPUT_BYTES,
            0,
            (&mut n as *mut i32).cast(),
        )?;
        Ok(())
    }

    /// Drop what libxaac used of the pending bytes; nothing used drops it
    /// all (it cannot be read, and must not pile up).
    fn used(&mut self) -> Result<(), String> {
        let used = self.get_cmd(x::IA_API_CMD_GET_CURIDX_INPUT_BUF, 0)?.max(0) as usize;
        if used == 0 {
            self.pending.clear();
        } else {
            self.pending.drain(..used.min(self.pending.len()));
        }
        Ok(())
    }

    /// One ADTS frame; its audio, interleaved with `info.channels`, into
    /// `out` (replacing what it held; empty while the header is read).
    pub fn decode(&mut self, frame: &[u8], out: &mut Vec<i16>) -> Result<(), String> {
        out.clear();
        self.pending.extend_from_slice(frame);
        if !self.ready {
            self.load()?;
            self.call(
                "header",
                x::IA_API_CMD_INIT,
                x::IA_CMD_TYPE_INIT_PROCESS,
                std::ptr::null_mut(),
            )?;
            let done = self.get_cmd(x::IA_API_CMD_INIT, x::IA_CMD_TYPE_INIT_DONE_QUERY)?;
            self.used()?;
            if done == 0 {
                return Ok(());
            }
            self.ready = true;
            self.query()?;
        }
        while !self.pending.is_empty() {
            self.load()?;
            self.call(
                "decode",
                x::IA_API_CMD_EXECUTE,
                x::IA_CMD_TYPE_DO_EXECUTE,
                std::ptr::null_mut(),
            )?;
            self.used()?;
            let bytes = self.get_cmd(x::IA_API_CMD_GET_OUTPUT_BYTES, 0)?.max(0) as usize;
            self.query()?;
            if self.info.channels == 0 || bytes == 0 {
                continue;
            }
            // SAFETY: libxaac wrote `bytes` bytes of 16-bit PCM to its
            // output buffer, which is aligned for i16.
            let pcm = unsafe {
                std::slice::from_raw_parts(self.output.as_ptr().cast::<i16>(), bytes / 2)
            };
            out.extend_from_slice(pcm);
        }
        Ok(())
    }

    fn query(&mut self) -> Result<(), String> {
        let get = |d: &mut Self, p| d.get_cmd(x::IA_API_CMD_GET_CONFIG_PARAM, p);
        self.info = XaacInfo {
            rate: get(self, x::IA_XHEAAC_DEC_CONFIG_PARAM_SAMP_FREQ)?.max(0) as u32,
            channels: get(self, x::IA_XHEAAC_DEC_CONFIG_PARAM_NUM_CHANNELS)?.max(0) as u32,
            sbr: get(self, x::IA_XHEAAC_DEC_CONFIG_PARAM_SBR_MODE)?.max(0) as u32,
        };
        Ok(())
    }
}

impl Drop for XaacDecoder {
    fn drop(&mut self) {
        // Tell libxaac the input is over before its memory goes (as its
        // fuzzer does); the memory is freed whatever it says.
        let _ = self.call("end", x::IA_API_CMD_INPUT_OVER, 0, std::ptr::null_mut());
    }
}
