// made by CasualArclamp
//! HE-AAC v1 (AAC-LC core at half the rate, with SBR) from libxaac's encoder,
//! in ADTS frames: what the stations on QO-100 send as stream type 0Fh.
//! ffmpeg's own AAC encoder has no SBR and libfdk_aac does not mix with the
//! GPL, so ffmpeg only copies these frames into the TS (see crate::feeder).
//! The wrapper's shape is DecDRM's `XheAacEncoder`.

use decsat_xaac_sys::enc::{
    AOT_SBR, IA_MEMTYPE_INPUT, IA_MEMTYPE_OUTPUT, MEASUREMENT_SYSTEM_BS_1770_3,
    METHOD_DEFINITION_PROGRAM_LOUDNESS, decsat_xaac_drc_config_size, decsat_xaac_free,
    decsat_xaac_malloc, ia_error_name, ia_is_fatal, ixheaace_create, ixheaace_delete,
    ixheaace_input_config, ixheaace_output_config, ixheaace_process,
};

/// One encoder. 16-bit interleaved PCM in, one ADTS frame out at a time.
pub struct HeAacEncoder {
    // Rust: boxed so the addresses libxaac keeps stay put when this moves.
    inp: Box<ixheaace_input_config>,
    out: Box<ixheaace_output_config>,
    // Backs `inp.pv_drc_cfg`: libxaac writes through it even with DRC off.
    _drc: Vec<u64>,
}

// SAFETY: the encoder's memory is only touched through `&mut self`, by one
// thread at a time; libxaac keeps no thread-local state.
unsafe impl Send for HeAacEncoder {}

impl HeAacEncoder {
    /// `sample_rate` of the PCM (the core runs at half of it).
    pub fn new(sample_rate: u32, channels: u32, bitrate: u32) -> Result<Self, String> {
        // SAFETY: a pure function of the library's struct size.
        let drc_size = unsafe { decsat_xaac_drc_config_size() };
        let mut drc = vec![0u64; drc_size.div_ceil(8)];
        let mut inp = Box::new(ixheaace_input_config::zeroed());
        inp.ui_pcm_wd_sz = 16;
        inp.aot = AOT_SBR;
        inp.i_channels = channels as i32;
        inp.i_samp_freq = sample_rate;
        inp.i_bitrate = bitrate as i32;
        inp.frame_length = 1024;
        inp.i_use_adts = 1;
        inp.i_mps_tree_config = -1;
        inp.pv_drc_cfg = drc.as_mut_ptr().cast();
        // As libxaac's test bench sets them for AAC.
        inp.aac_config.inv_quant = 2;
        inp.aac_config.use_tns = 1;
        inp.aac_config.bitreservoir_size = 768;
        inp.random_access_interval = -1;
        inp.method_def = METHOD_DEFINITION_PROGRAM_LOUDNESS;
        inp.measurement_system = MEASUREMENT_SYSTEM_BS_1770_3;
        let mut out = Box::new(ixheaace_output_config::zeroed());
        out.malloc_xheaace = Some(decsat_xaac_malloc);
        out.free_xheaace = Some(decsat_xaac_free);
        // SAFETY: both configs and the DRC buffer outlive the encoder (they
        // are owned by the value returned, and freed after ixheaace_delete).
        let err = unsafe { ixheaace_create((&raw mut *inp).cast(), (&raw mut *out).cast()) };
        if ia_is_fatal(err) {
            return Err(format!("HE-AAC encoder: {}", ia_error_name(err)));
        }
        Ok(HeAacEncoder {
            inp,
            out,
            _drc: drc,
        })
    }

    /// Bytes of PCM each [`Self::encode`] takes.
    pub fn frame_bytes(&self) -> usize {
        self.out.input_size as usize
    }

    /// The bit rate the library settled on (it corrects what it cannot do).
    pub fn bitrate(&self) -> u32 {
        self.inp.i_bitrate as u32
    }

    /// One frame of PCM (little-endian 16-bit, [`Self::frame_bytes`] long)
    /// to its ADTS frame, empty while the encoder is still filling up.
    pub fn encode(&mut self, pcm: &[u8]) -> Result<&[u8], String> {
        assert_eq!(pcm.len(), self.frame_bytes());
        let input = self.out.mem_info_table[IA_MEMTYPE_INPUT]
            .mem_ptr
            .cast::<u8>();
        // SAFETY: the library allocated `input_size` bytes of input buffer;
        // the configs stay valid during the call; the output is `i_out_bytes`
        // long in its own buffer, and the borrow of `self` keeps it alive.
        unsafe {
            std::ptr::copy_nonoverlapping(pcm.as_ptr(), input, pcm.len());
            let err = ixheaace_process(
                self.out.pv_ia_process_api_obj,
                (&raw mut *self.inp).cast(),
                (&raw mut *self.out).cast(),
            );
            if ia_is_fatal(err) {
                return Err(format!("HE-AAC encoder: {}", ia_error_name(err)));
            }
            let output = self.out.mem_info_table[IA_MEMTYPE_OUTPUT]
                .mem_ptr
                .cast::<u8>()
                .cast_const();
            Ok(std::slice::from_raw_parts(
                output,
                self.out.i_out_bytes.max(0) as usize,
            ))
        }
    }
}

impl Drop for HeAacEncoder {
    fn drop(&mut self) {
        // SAFETY: created by ixheaace_create, freed once.
        unsafe {
            ixheaace_delete((&raw mut *self.out).cast());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a tone; the ADTS frames and the bytes they add up to.
    fn run(channels: u32, bitrate: u32) -> (Vec<Vec<u8>>, f64) {
        let mut e = HeAacEncoder::new(48_000, channels, bitrate).unwrap();
        let n = e.frame_bytes();
        let per_ch = n / 2 / channels as usize;
        assert_eq!(per_ch, 2048, "two core frames' worth of input");
        let mut frames = Vec::new();
        for f in 0..200usize {
            let mut pcm = Vec::with_capacity(n);
            for i in 0..per_ch {
                let t = (f * per_ch + i) as f64 / 48_000.0;
                let v = (8000.0 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()) as i16;
                for _ in 0..channels {
                    pcm.extend_from_slice(&v.to_le_bytes());
                }
            }
            let out = e.encode(&pcm).unwrap();
            if !out.is_empty() {
                frames.push(out.to_vec());
            }
        }
        let bytes: usize = frames.iter().map(Vec::len).sum();
        let secs = frames.len() as f64 * 2048.0 / 48_000.0;
        (frames, bytes as f64 * 8.0 / secs)
    }

    #[test]
    fn it_makes_adts_frames_of_a_24_khz_core() {
        for (channels, bitrate) in [(1, 32_000), (2, 48_000)] {
            let (frames, rate) = run(channels, bitrate);
            assert!(frames.len() > 190, "{}", frames.len());
            for f in &frames {
                // ISO/IEC 13818-7 §6.2 / 14496-3 §1.A.2: the ADTS fixed header.
                assert_eq!((f[0], f[1] & 0xF6), (0xFF, 0xF0), "syncword, layer 0");
                assert_eq!(f[2] >> 6, 1, "AAC-LC core (SBR is implicit in ADTS)");
                assert_eq!((f[2] >> 2) & 0x0F, 6, "24 kHz core");
                let channel_config = ((f[2] & 1) << 2) | (f[3] >> 6);
                assert_eq!(u32::from(channel_config), channels);
                let len = (usize::from(f[3] & 3) << 11)
                    | (usize::from(f[4]) << 3)
                    | usize::from(f[5] >> 5);
                assert_eq!(len, f.len(), "frame length");
            }
            let want = f64::from(bitrate);
            assert!((rate - want).abs() < want * 0.25, "{rate} for {want}");
        }
    }
}
