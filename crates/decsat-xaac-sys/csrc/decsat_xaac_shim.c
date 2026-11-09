/* made by CasualArclamp
 * DecSAT helper shim compiled together with the libxaac encoder (after
 * DecDRM's decdrm_xaac_shim.c).
 *
 * 1. Exports sizeof/offsetof of every public struct that decsat-xaac-sys
 *    transcribes by hand, so Rust unit tests can check the hand-written
 *    #[repr(C)] layouts against the real compiler's view (there is no bindgen
 *    on the build machines).
 * 2. Provides the aligned allocator pair that libxaac expects in
 *    ixheaace_output_config (malloc_xheaace / free_xheaace), equivalent to the
 *    one in libxaac's test bench.
 * 3. Reports sizeof(ia_drc_input_config): ixheaace_create() writes a whole
 *    ia_drc_input_config through ixheaace_input_config.pv_drc_cfg, so the
 *    caller must provide a buffer of at least this size.
 *
 * This file is part of DecSAT (GPL-3.0-or-later); it only includes libxaac's
 * public headers.
 */

#include <stddef.h>
#include <stdlib.h>
#if defined(_WIN32)
#include <malloc.h>
#endif

#include "ixheaac_type_def.h"
#include "impd_drc_common_enc.h"
#include "impd_drc_uni_drc.h"
#include "impd_drc_tables.h"
#include "impd_drc_api.h"
#include "ixheaace_api.h"

typedef struct {
  const char *name;
  size_t value;
} DecsatLayoutEntry;

#define DECSAT_SIZE(T) {"sizeof " #T, sizeof(T)}
#define DECSAT_OFF(T, f) {#T "." #f, offsetof(T, f)}

static const DecsatLayoutEntry kLayout[] = {
    /* scalar typedefs */
    DECSAT_SIZE(WORD32),
    DECSAT_SIZE(UWORD32),
    DECSAT_SIZE(FLAG),
    DECSAT_SIZE(FLOAT32),
    DECSAT_SIZE(FLOAT64),
    DECSAT_SIZE(UWORD16),
    DECSAT_SIZE(SIZE_T),
    DECSAT_SIZE(pVOID),

    /* ixheaace_mem_info_table */
    DECSAT_SIZE(ixheaace_mem_info_table),
    DECSAT_OFF(ixheaace_mem_info_table, ui_size),
    DECSAT_OFF(ixheaace_mem_info_table, ui_alignment),
    DECSAT_OFF(ixheaace_mem_info_table, ui_type),
    DECSAT_OFF(ixheaace_mem_info_table, mem_ptr),

    /* ixheaace_aac_enc_config */
    DECSAT_SIZE(ixheaace_aac_enc_config),
    DECSAT_OFF(ixheaace_aac_enc_config, sample_rate),
    DECSAT_OFF(ixheaace_aac_enc_config, bitrate),
    DECSAT_OFF(ixheaace_aac_enc_config, num_channels_in),
    DECSAT_OFF(ixheaace_aac_enc_config, num_channels_out),
    DECSAT_OFF(ixheaace_aac_enc_config, bandwidth),
    DECSAT_OFF(ixheaace_aac_enc_config, dual_mono),
    DECSAT_OFF(ixheaace_aac_enc_config, use_tns),
    DECSAT_OFF(ixheaace_aac_enc_config, noise_filling),
    DECSAT_OFF(ixheaace_aac_enc_config, use_adts),
    DECSAT_OFF(ixheaace_aac_enc_config, private_bit),
    DECSAT_OFF(ixheaace_aac_enc_config, copyright_bit),
    DECSAT_OFF(ixheaace_aac_enc_config, original_copy_bit),
    DECSAT_OFF(ixheaace_aac_enc_config, f_no_stereo_preprocessing),
    DECSAT_OFF(ixheaace_aac_enc_config, inv_quant),
    DECSAT_OFF(ixheaace_aac_enc_config, full_bandwidth),
    DECSAT_OFF(ixheaace_aac_enc_config, bitreservoir_size),
    DECSAT_OFF(ixheaace_aac_enc_config, length),

    /* ixheaace_input_config */
    DECSAT_SIZE(ixheaace_input_config),
    DECSAT_OFF(ixheaace_input_config, ui_pcm_wd_sz),
    DECSAT_OFF(ixheaace_input_config, i_bitrate),
    DECSAT_OFF(ixheaace_input_config, frame_length),
    DECSAT_OFF(ixheaace_input_config, frame_cmd_flag),
    DECSAT_OFF(ixheaace_input_config, out_bytes_flag),
    DECSAT_OFF(ixheaace_input_config, user_tns_flag),
    DECSAT_OFF(ixheaace_input_config, user_esbr_flag),
    DECSAT_OFF(ixheaace_input_config, aot),
    DECSAT_OFF(ixheaace_input_config, i_mps_tree_config),
    DECSAT_OFF(ixheaace_input_config, esbr_flag),
    DECSAT_OFF(ixheaace_input_config, i_channels),
    DECSAT_OFF(ixheaace_input_config, i_samp_freq),
    DECSAT_OFF(ixheaace_input_config, i_native_samp_freq),
    DECSAT_OFF(ixheaace_input_config, i_channels_mask),
    DECSAT_OFF(ixheaace_input_config, i_num_coupling_chan),
    DECSAT_OFF(ixheaace_input_config, i_use_mps),
    DECSAT_OFF(ixheaace_input_config, i_use_adts),
    DECSAT_OFF(ixheaace_input_config, i_use_es),
    DECSAT_OFF(ixheaace_input_config, usac_en),
    DECSAT_OFF(ixheaace_input_config, codec_mode),
    DECSAT_OFF(ixheaace_input_config, cplx_pred),
    DECSAT_OFF(ixheaace_input_config, ccfl_idx),
    DECSAT_OFF(ixheaace_input_config, pvc_active),
    DECSAT_OFF(ixheaace_input_config, harmonic_sbr),
    DECSAT_OFF(ixheaace_input_config, inter_tes_active),
    DECSAT_OFF(ixheaace_input_config, pv_drc_cfg),
    DECSAT_OFF(ixheaace_input_config, use_drc_element),
    DECSAT_OFF(ixheaace_input_config, drc_frame_size),
    DECSAT_OFF(ixheaace_input_config, hq_esbr),
    DECSAT_OFF(ixheaace_input_config, write_program_config_element),
    DECSAT_OFF(ixheaace_input_config, aac_config),
    DECSAT_OFF(ixheaace_input_config, random_access_interval),
    DECSAT_OFF(ixheaace_input_config, method_def),
    DECSAT_OFF(ixheaace_input_config, measured_loudness),
    DECSAT_OFF(ixheaace_input_config, measurement_system),
    DECSAT_OFF(ixheaace_input_config, sample_peak_level),
    DECSAT_OFF(ixheaace_input_config, stream_id),
    DECSAT_OFF(ixheaace_input_config, use_delay_adjustment),

    /* ixheaace_version */
    DECSAT_SIZE(ixheaace_version),
    DECSAT_OFF(ixheaace_version, p_lib_name),
    DECSAT_OFF(ixheaace_version, p_version_num),

    /* ixheaace_output_config */
    DECSAT_SIZE(ixheaace_output_config),
    DECSAT_OFF(ixheaace_output_config, i_out_bytes),
    DECSAT_OFF(ixheaace_output_config, i_bytes_consumed),
    DECSAT_OFF(ixheaace_output_config, ui_inp_buf_size),
    DECSAT_OFF(ixheaace_output_config, malloc_count),
    DECSAT_OFF(ixheaace_output_config, ui_rem),
    DECSAT_OFF(ixheaace_output_config, ui_proc_mem_tabs_size),
    DECSAT_OFF(ixheaace_output_config, pv_ia_process_api_obj),
    DECSAT_OFF(ixheaace_output_config, arr_alloc_memory),
    DECSAT_OFF(ixheaace_output_config, malloc_xheaace),
    DECSAT_OFF(ixheaace_output_config, free_xheaace),
    DECSAT_OFF(ixheaace_output_config, version),
    DECSAT_OFF(ixheaace_output_config, mem_info_table),
    DECSAT_OFF(ixheaace_output_config, input_size),
    DECSAT_OFF(ixheaace_output_config, samp_freq),
    DECSAT_OFF(ixheaace_output_config, header_samp_freq),
    DECSAT_OFF(ixheaace_output_config, audio_profile),
    DECSAT_OFF(ixheaace_output_config, down_sampling_ratio),
    DECSAT_OFF(ixheaace_output_config, expected_frame_count),
    DECSAT_OFF(ixheaace_output_config, is_loudness_configured),
};

/* Number of entries in the layout table. */
size_t decsat_xaac_layout_count(void) {
  return sizeof(kLayout) / sizeof(kLayout[0]);
}

/* Name ("sizeof X" or "Struct.field") and value of entry `i`.
 * Returns NULL (and leaves *value untouched) when `i` is out of range. */
const char *decsat_xaac_layout_entry(size_t i, size_t *value) {
  if (i >= decsat_xaac_layout_count()) return NULL;
  if (value != NULL) *value = kLayout[i].value;
  return kLayout[i].name;
}

/* Size in bytes of the ia_drc_input_config that pv_drc_cfg must point to. */
size_t decsat_xaac_drc_config_size(void) { return sizeof(ia_drc_input_config); }

/* Aligned allocation for ixheaace_output_config.malloc_xheaace (the library asks
 * for 8-byte alignment). Returns NULL on failure. */
pVOID decsat_xaac_malloc(UWORD32 size, UWORD32 alignment) {
  size_t align = alignment;
  if (align < sizeof(void *)) align = sizeof(void *);
  /* posix_memalign/_aligned_malloc need a power of two. */
  if ((align & (align - 1)) != 0) return NULL;
  if (size == 0) size = 1;
#if defined(_WIN32)
  return _aligned_malloc(size, align);
#else
  {
    void *ptr = NULL;
    if (posix_memalign(&ptr, align, size) != 0) return NULL;
    return ptr;
  }
#endif
}

/* Matching release function for ixheaace_output_config.free_xheaace. */
VOID decsat_xaac_free(pVOID ptr) {
  if (ptr == NULL) return;
#if defined(_WIN32)
  _aligned_free(ptr);
#else
  free(ptr);
#endif
}
