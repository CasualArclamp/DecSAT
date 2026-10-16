// made by CasualArclamp
//! Build and raw FFI bindings for the vendored libxaac AAC decoder.
//!
//! `build.rs` compiles the decoder of `third_party/libxaac` (tag v0.1.13, Apache-2.0):
//! AAC-LC, HE-AAC v1 (SBR) and v2 (PS), xHE-AAC (USAC), in ADTS, LOAS or raw access units.
//! Its whole API is one function, [`ixheaacd_dec_api`], driven by command numbers; this
//! module transcribes the commands and parameters used (from `ixheaacd_apicmd_standards.h`,
//! `ixheaacd_aac_config.h`, `ixheaacd_memory_standards.h` and
//! `ixheaac_error_standards.h`). `decsat-audio` wraps it safely.
//!
//! # The call sequence (libxaac's `README_dec.md` and its fuzzer)
//!
//! 1. [`IA_API_CMD_GET_API_SIZE`], allocate the API object, then [`IA_API_CMD_INIT`] /
//!    [`IA_CMD_TYPE_INIT_API_PRE_CONFIG_PARAMS`].
//! 2. [`IA_API_CMD_SET_CONFIG_PARAM`] as wanted ([`IA_XHEAAC_DEC_CONFIG_PARAM_MP4FLAG`] 0
//!    for ADTS).
//! 3. [`IA_API_CMD_GET_MEMTABS_SIZE`], allocate, [`IA_API_CMD_SET_MEMTABS_PTR`], then
//!    [`IA_CMD_TYPE_INIT_API_POST_CONFIG_PARAMS`].
//! 4. For each of the [`IA_API_CMD_GET_N_MEMTABS`] memories: size, alignment and type,
//!    allocate, [`IA_API_CMD_SET_MEM_PTR`]. The input and output memories are the
//!    buffers the caller fills and reads.
//! 5. Copy the first bytes in, [`IA_API_CMD_SET_INPUT_BYTES`], [`IA_CMD_TYPE_INIT_PROCESS`]
//!    and [`IA_CMD_TYPE_INIT_DONE_QUERY`]; [`IA_API_CMD_GET_CURIDX_INPUT_BUF`] says how
//!    much was used.
//! 6. Per frame: copy in, set the input bytes, [`IA_API_CMD_EXECUTE`] /
//!    [`IA_CMD_TYPE_DO_EXECUTE`], then the bytes used and [`IA_API_CMD_GET_OUTPUT_BYTES`]
//!    of 16-bit interleaved PCM in the output memory.
//!
//! Every value is passed by pointer (`*mut c_void` to a 32-bit integer, or the memory
//! itself). An error code with bit 31 set ([`IA_FATAL_ERROR`]) is fatal; other non-zero
//! codes are warnings (a damaged frame, concealed).

use core::ffi::c_void;

/// `IA_ERRORCODE`.
pub type IaErrorCode = i32;

pub const IA_NO_ERROR: IaErrorCode = 0;
/// The bit that marks an error fatal.
pub const IA_FATAL_ERROR: IaErrorCode = 0x8000_0000_u32 as i32;

// API commands (`ixheaacd_apicmd_standards.h`).
pub const IA_API_CMD_GET_LIB_ID_STRINGS: i32 = 0x0001;
pub const IA_API_CMD_GET_API_SIZE: i32 = 0x0002;
pub const IA_API_CMD_INIT: i32 = 0x0003;
pub const IA_API_CMD_SET_CONFIG_PARAM: i32 = 0x0004;
pub const IA_API_CMD_GET_CONFIG_PARAM: i32 = 0x0005;
pub const IA_API_CMD_GET_MEMTABS_SIZE: i32 = 0x0006;
pub const IA_API_CMD_SET_MEMTABS_PTR: i32 = 0x0007;
pub const IA_API_CMD_GET_N_MEMTABS: i32 = 0x0008;
pub const IA_API_CMD_EXECUTE: i32 = 0x0009;
pub const IA_API_CMD_GET_CURIDX_INPUT_BUF: i32 = 0x000B;
pub const IA_API_CMD_SET_INPUT_BYTES: i32 = 0x000C;
pub const IA_API_CMD_GET_OUTPUT_BYTES: i32 = 0x000D;
pub const IA_API_CMD_INPUT_OVER: i32 = 0x000E;
pub const IA_API_CMD_GET_MEM_INFO_SIZE: i32 = 0x0011;
pub const IA_API_CMD_GET_MEM_INFO_ALIGNMENT: i32 = 0x0012;
pub const IA_API_CMD_GET_MEM_INFO_TYPE: i32 = 0x0013;
pub const IA_API_CMD_SET_MEM_PTR: i32 = 0x0016;

// Sub-commands.
pub const IA_CMD_TYPE_LIB_NAME: i32 = 0x0100;
pub const IA_CMD_TYPE_LIB_VERSION: i32 = 0x0200;
pub const IA_CMD_TYPE_INIT_API_PRE_CONFIG_PARAMS: i32 = 0x0100;
pub const IA_CMD_TYPE_INIT_API_POST_CONFIG_PARAMS: i32 = 0x0200;
pub const IA_CMD_TYPE_INIT_PROCESS: i32 = 0x0300;
pub const IA_CMD_TYPE_INIT_DONE_QUERY: i32 = 0x0400;
pub const IA_CMD_TYPE_DO_EXECUTE: i32 = 0x0100;
pub const IA_CMD_TYPE_DONE_QUERY: i32 = 0x0200;

// Memory types (`ixheaacd_memory_standards.h`).
pub const IA_MEMTYPE_PERSIST: i32 = 0x00;
pub const IA_MEMTYPE_SCRATCH: i32 = 0x01;
pub const IA_MEMTYPE_INPUT: i32 = 0x02;
pub const IA_MEMTYPE_OUTPUT: i32 = 0x03;

// Configuration parameters (`ixheaacd_aac_config.h`).
pub const IA_XHEAAC_DEC_CONFIG_PARAM_PCM_WDSZ: i32 = 0x0000;
pub const IA_XHEAAC_DEC_CONFIG_PARAM_SAMP_FREQ: i32 = 0x0001;
pub const IA_XHEAAC_DEC_CONFIG_PARAM_NUM_CHANNELS: i32 = 0x0002;
pub const IA_XHEAAC_DEC_CONFIG_PARAM_CHANNEL_MODE: i32 = 0x0004;
/// SBR: 0 not present, 1 present (HE-AAC), 2 present with PS (HE-AAC v2).
pub const IA_XHEAAC_DEC_CONFIG_PARAM_SBR_MODE: i32 = 0x0005;
pub const IA_XHEAAC_DEC_CONFIG_PARAM_TOSTEREO: i32 = 0x000A;
/// 1: the input is raw access units after an AudioSpecificConfig; 0: ADTS/LOAS.
pub const IA_XHEAAC_DEC_CONFIG_PARAM_MP4FLAG: i32 = 0x000C;
pub const IA_XHEAAC_DEC_CONFIG_PARAM_MAX_CHANNEL: i32 = 0x000D;
pub const IA_XHEAAC_DEC_CONFIG_PARAM_PS_ENABLE: i32 = 0x0019;
pub const IA_XHEAAC_DEC_CONFIG_PARAM_AOT: i32 = 0x001A;
/// 1: 960-sample frames rather than 1024.
pub const IA_XHEAAC_DEC_CONFIG_PARAM_FRAMELENGTH_FLAG: i32 = 0x001C;
pub const IA_XHEAAC_DEC_CONFIG_ERROR_CONCEALMENT: i32 = 0x001D;

unsafe extern "C" {
    /// The decoder's one entry point: `cmd` and `idx` select what is done to or read from
    /// `obj` (null for [`IA_API_CMD_GET_API_SIZE`]); `value` points at the argument or
    /// result.
    pub fn ixheaacd_dec_api(
        obj: *mut c_void,
        cmd: i32,
        idx: i32,
        value: *mut c_void,
    ) -> IaErrorCode;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_api_answers() {
        let mut size: u32 = 0;
        // SAFETY: GET_API_SIZE needs no object and writes one 32-bit value.
        let e = unsafe {
            ixheaacd_dec_api(
                core::ptr::null_mut(),
                IA_API_CMD_GET_API_SIZE,
                0,
                (&mut size as *mut u32).cast(),
            )
        };
        assert_eq!(e, IA_NO_ERROR);
        assert!(size > 0 && size < 1 << 24, "{size}");
    }
}
