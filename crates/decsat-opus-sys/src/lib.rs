// made by CasualArclamp
//! Build and raw FFI bindings for the vendored libopus.
//!
//! `build.rs` compiles `third_party/opus` (tag v1.6.1) with Opus' CMake project into a
//! static library (float API; no programs, tests or DNN extensions). This module is a
//! hand-written subset of `opus.h` / `opus_defines.h`: encoder and decoder life cycle,
//! float encode/decode, the variadic `*_ctl` functions, packet inspection helpers and
//! constants. `decsat-audio` decodes radio with it (`decode::OpusDecoder`); the encoder
//! makes test streams.
//!
//! Taken from DecDRM's `decdrm-opus-sys` (same author, GPL-3.0-or-later); libopus itself
//! is BSD-licensed (`third_party/opus/COPYING`).
//!
//! # Calling the variadic `*_ctl` functions
//!
//! In C the requests are wrapped in macros such as `OPUS_SET_BITRATE(x)`, which expand to
//! `OPUS_SET_BITRATE_REQUEST, (opus_int32)(x)`. From Rust pass the `*_REQUEST` constant
//! followed by an [`opus_int32`] (setters) or a `*mut opus_int32` (getters; `*mut
//! opus_uint32` for [`OPUS_GET_FINAL_RANGE_REQUEST`]). [`OPUS_RESET_STATE`] takes no
//! argument.

#![allow(non_camel_case_types)]

use core::ffi::{c_char, c_int, c_uchar};
use core::marker::{PhantomData, PhantomPinned};

/// `opus_int16`.
pub type opus_int16 = i16;
/// `opus_int32`.
pub type opus_int32 = i32;
/// `opus_uint32`.
pub type opus_uint32 = u32;

/// Opaque encoder state (`OpusEncoder`).
#[repr(C)]
pub struct OpusEncoder {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// Opaque decoder state (`OpusDecoder`).
#[repr(C)]
pub struct OpusDecoder {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

// Error codes.
pub const OPUS_OK: c_int = 0;
pub const OPUS_BAD_ARG: c_int = -1;
pub const OPUS_BUFFER_TOO_SMALL: c_int = -2;
pub const OPUS_INTERNAL_ERROR: c_int = -3;
pub const OPUS_INVALID_PACKET: c_int = -4;
pub const OPUS_UNIMPLEMENTED: c_int = -5;
pub const OPUS_INVALID_STATE: c_int = -6;
pub const OPUS_ALLOC_FAIL: c_int = -7;

// Generic values.
pub const OPUS_AUTO: opus_int32 = -1000;
pub const OPUS_BITRATE_MAX: opus_int32 = -1;

// Applications.
pub const OPUS_APPLICATION_VOIP: c_int = 2048;
pub const OPUS_APPLICATION_AUDIO: c_int = 2049;
pub const OPUS_APPLICATION_RESTRICTED_LOWDELAY: c_int = 2051;

// Signal types.
pub const OPUS_SIGNAL_VOICE: opus_int32 = 3001;
pub const OPUS_SIGNAL_MUSIC: opus_int32 = 3002;

// Bandwidths.
pub const OPUS_BANDWIDTH_NARROWBAND: opus_int32 = 1101;
pub const OPUS_BANDWIDTH_MEDIUMBAND: opus_int32 = 1102;
pub const OPUS_BANDWIDTH_WIDEBAND: opus_int32 = 1103;
pub const OPUS_BANDWIDTH_SUPERWIDEBAND: opus_int32 = 1104;
pub const OPUS_BANDWIDTH_FULLBAND: opus_int32 = 1105;

// Expert frame durations.
pub const OPUS_FRAMESIZE_ARG: opus_int32 = 5000;
pub const OPUS_FRAMESIZE_2_5_MS: opus_int32 = 5001;
pub const OPUS_FRAMESIZE_5_MS: opus_int32 = 5002;
pub const OPUS_FRAMESIZE_10_MS: opus_int32 = 5003;
pub const OPUS_FRAMESIZE_20_MS: opus_int32 = 5004;
pub const OPUS_FRAMESIZE_40_MS: opus_int32 = 5005;
pub const OPUS_FRAMESIZE_60_MS: opus_int32 = 5006;

// CTL requests (see the module docs for the argument conventions).
pub const OPUS_SET_APPLICATION_REQUEST: c_int = 4000;
pub const OPUS_GET_APPLICATION_REQUEST: c_int = 4001;
pub const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
pub const OPUS_GET_BITRATE_REQUEST: c_int = 4003;
pub const OPUS_SET_MAX_BANDWIDTH_REQUEST: c_int = 4004;
pub const OPUS_GET_MAX_BANDWIDTH_REQUEST: c_int = 4005;
pub const OPUS_SET_VBR_REQUEST: c_int = 4006;
pub const OPUS_GET_VBR_REQUEST: c_int = 4007;
pub const OPUS_SET_BANDWIDTH_REQUEST: c_int = 4008;
pub const OPUS_GET_BANDWIDTH_REQUEST: c_int = 4009;
pub const OPUS_SET_COMPLEXITY_REQUEST: c_int = 4010;
pub const OPUS_GET_COMPLEXITY_REQUEST: c_int = 4011;
pub const OPUS_SET_INBAND_FEC_REQUEST: c_int = 4012;
pub const OPUS_GET_INBAND_FEC_REQUEST: c_int = 4013;
pub const OPUS_SET_PACKET_LOSS_PERC_REQUEST: c_int = 4014;
pub const OPUS_GET_PACKET_LOSS_PERC_REQUEST: c_int = 4015;
pub const OPUS_SET_DTX_REQUEST: c_int = 4016;
pub const OPUS_GET_DTX_REQUEST: c_int = 4017;
pub const OPUS_SET_VBR_CONSTRAINT_REQUEST: c_int = 4020;
pub const OPUS_GET_VBR_CONSTRAINT_REQUEST: c_int = 4021;
pub const OPUS_SET_FORCE_CHANNELS_REQUEST: c_int = 4022;
pub const OPUS_GET_FORCE_CHANNELS_REQUEST: c_int = 4023;
pub const OPUS_SET_SIGNAL_REQUEST: c_int = 4024;
pub const OPUS_GET_SIGNAL_REQUEST: c_int = 4025;
pub const OPUS_GET_LOOKAHEAD_REQUEST: c_int = 4027;
pub const OPUS_RESET_STATE: c_int = 4028;
pub const OPUS_GET_SAMPLE_RATE_REQUEST: c_int = 4029;
pub const OPUS_GET_FINAL_RANGE_REQUEST: c_int = 4031;
pub const OPUS_SET_GAIN_REQUEST: c_int = 4034;
pub const OPUS_SET_LSB_DEPTH_REQUEST: c_int = 4036;
pub const OPUS_GET_LSB_DEPTH_REQUEST: c_int = 4037;
pub const OPUS_GET_LAST_PACKET_DURATION_REQUEST: c_int = 4039;
pub const OPUS_SET_EXPERT_FRAME_DURATION_REQUEST: c_int = 4040;
pub const OPUS_GET_EXPERT_FRAME_DURATION_REQUEST: c_int = 4041;
pub const OPUS_SET_PREDICTION_DISABLED_REQUEST: c_int = 4042;
pub const OPUS_GET_PREDICTION_DISABLED_REQUEST: c_int = 4043;
pub const OPUS_SET_PHASE_INVERSION_DISABLED_REQUEST: c_int = 4046;

unsafe extern "C" {
    // --- encoder ---
    pub fn opus_encoder_get_size(channels: c_int) -> c_int;
    /// Returns NULL on failure (`error` receives the reason).
    pub fn opus_encoder_create(
        Fs: opus_int32,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut OpusEncoder;
    /// Encodes one frame of interleaved float PCM (nominal range ±1); returns the packet
    /// length in bytes or a negative error code.
    pub fn opus_encode_float(
        st: *mut OpusEncoder,
        pcm: *const f32,
        frame_size: c_int,
        data: *mut c_uchar,
        max_data_bytes: opus_int32,
    ) -> opus_int32;
    pub fn opus_encode(
        st: *mut OpusEncoder,
        pcm: *const opus_int16,
        frame_size: c_int,
        data: *mut c_uchar,
        max_data_bytes: opus_int32,
    ) -> opus_int32;
    pub fn opus_encoder_destroy(st: *mut OpusEncoder);
    /// Variadic CTL (see module docs).
    pub fn opus_encoder_ctl(st: *mut OpusEncoder, request: c_int, ...) -> c_int;

    // --- decoder ---
    pub fn opus_decoder_get_size(channels: c_int) -> c_int;
    /// Returns NULL on failure (`error` receives the reason).
    pub fn opus_decoder_create(
        Fs: opus_int32,
        channels: c_int,
        error: *mut c_int,
    ) -> *mut OpusDecoder;
    /// Decodes a packet (or runs PLC when `data` is NULL / `len` is 0) into interleaved
    /// float PCM; returns samples per channel or a negative error code. `frame_size` is
    /// the capacity of `pcm` per channel; for PLC/FEC it must be the exact duration (a
    /// multiple of 2.5 ms).
    pub fn opus_decode_float(
        st: *mut OpusDecoder,
        data: *const c_uchar,
        len: opus_int32,
        pcm: *mut f32,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    pub fn opus_decode(
        st: *mut OpusDecoder,
        data: *const c_uchar,
        len: opus_int32,
        pcm: *mut opus_int16,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    pub fn opus_decoder_destroy(st: *mut OpusDecoder);
    /// Variadic CTL (see module docs).
    pub fn opus_decoder_ctl(st: *mut OpusDecoder, request: c_int, ...) -> c_int;
    pub fn opus_decoder_get_nb_samples(
        dec: *const OpusDecoder,
        packet: *const c_uchar,
        len: opus_int32,
    ) -> c_int;

    // --- packet helpers ---
    pub fn opus_packet_get_bandwidth(data: *const c_uchar) -> c_int;
    pub fn opus_packet_get_samples_per_frame(data: *const c_uchar, Fs: opus_int32) -> c_int;
    pub fn opus_packet_get_nb_channels(data: *const c_uchar) -> c_int;
    pub fn opus_packet_get_nb_frames(packet: *const c_uchar, len: opus_int32) -> c_int;
    pub fn opus_packet_get_nb_samples(
        packet: *const c_uchar,
        len: opus_int32,
        Fs: opus_int32,
    ) -> c_int;

    // --- misc ---
    /// Soft-clips interleaved float PCM into [-1, 1] in place (the int16 decode API does
    /// this internally). `softclip_mem` holds one float of state per channel, zero-initialised.
    pub fn opus_pcm_soft_clip(
        pcm: *mut f32,
        frame_size: c_int,
        channels: c_int,
        softclip_mem: *mut f32,
    );
    pub fn opus_strerror(error: c_int) -> *const c_char;
    pub fn opus_get_version_string() -> *const c_char;
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ffi::CStr;

    #[test]
    fn version_string_is_1_6() {
        // SAFETY: returns a static NUL-terminated string.
        let v = unsafe { CStr::from_ptr(opus_get_version_string()) };
        let v = v.to_str().unwrap();
        // libopus takes its version from `git describe --tags`; a shallow submodule
        // clone (as `.gitmodules` requests) has no tags and reports "unknown". Any
        // other version means a different libopus was linked.
        assert!(
            v.starts_with("libopus 1.6") || v == "libopus unknown",
            "unexpected version {v}"
        );
    }

    #[test]
    fn encode_decode_smoke() {
        const FS: i32 = 48_000;
        const N: usize = 960; // 20 ms
        let mut err = 0;
        // SAFETY: valid arguments; the states are destroyed at the end of the test and all
        // buffers outlive the calls that use them.
        unsafe {
            let enc = opus_encoder_create(FS, 1, OPUS_APPLICATION_AUDIO, &mut err);
            assert!(!enc.is_null(), "{err}");
            assert_eq!(
                opus_encoder_ctl(enc, OPUS_SET_BITRATE_REQUEST, 32_000i32),
                OPUS_OK
            );
            assert_eq!(opus_encoder_ctl(enc, OPUS_SET_VBR_REQUEST, 0i32), OPUS_OK);
            let mut br: opus_int32 = 0;
            assert_eq!(
                opus_encoder_ctl(enc, OPUS_GET_BITRATE_REQUEST, &mut br as *mut opus_int32),
                OPUS_OK
            );
            assert_eq!(br, 32_000);

            let dec = opus_decoder_create(FS, 2, &mut err);
            assert!(!dec.is_null(), "{err}");

            let pcm: Vec<f32> = (0..N)
                .map(|i| 0.5 * (2.0 * core::f32::consts::PI * 1000.0 * i as f32 / FS as f32).sin())
                .collect();
            let mut packet = [0u8; 1275];
            let n = opus_encode_float(enc, pcm.as_ptr(), N as c_int, packet.as_mut_ptr(), 1275);
            assert!(n > 0, "encode failed: {n}");
            // CBR at 32 kbit/s and 20 ms: exactly 80 bytes.
            assert_eq!(n, 80);
            assert_eq!(
                opus_packet_get_nb_samples(packet.as_ptr(), n, FS),
                N as c_int
            );
            assert_eq!(opus_packet_get_nb_channels(packet.as_ptr()), 1);

            let mut out = vec![0f32; 2 * 5760];
            let got = opus_decode_float(dec, packet.as_ptr(), n, out.as_mut_ptr(), 5760, 0);
            assert_eq!(got, N as c_int);
            // Packet loss concealment for one more 20 ms frame.
            let plc = opus_decode_float(dec, core::ptr::null(), 0, out.as_mut_ptr(), N as c_int, 0);
            assert_eq!(plc, N as c_int);
            // Garbage must be rejected, not crash.
            let junk = [0xFFu8; 3];
            let bad = opus_decode_float(dec, junk.as_ptr(), 3, out.as_mut_ptr(), 5760, 0);
            assert!(bad < 0);
            let msg = CStr::from_ptr(opus_strerror(OPUS_INVALID_PACKET));
            assert!(!msg.to_bytes().is_empty());

            opus_decoder_destroy(dec);
            opus_encoder_destroy(enc);
        }
    }
}
