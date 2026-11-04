// made by CasualArclamp
//! AV1 in an MPEG-TS, as AOM's "Carriage of AV1 in MPEG-2 TS" (v1.0.1,
//! <https://AOMediaCodec.github.io/av1-mpeg2-ts>) has it. Neither ffmpeg's
//! muxer nor its demuxer (9.0) knows the mapping: the muxer writes AV1 as
//! bare private data, and the demuxer cannot read it back. So both ends are
//! here:
//!
//! - [`Av1TsFixer`] (transmit) takes ffmpeg's transport stream and makes the
//!   AV1 stream conform: the registration descriptor `AV01` and the AV1 video
//!   descriptor in the PMT (§ "Identifying AV1 streams"), every OBU behind a
//!   start code with emulation prevention (§ "Start-code based format"), one
//!   access unit per PES with `data_alignment_indicator` set (§ "Use of PES
//!   packets").
//! - [`Av1Extractor`] (receive) undoes it: PES packets in, temporal units in
//!   the low-overhead OBU format out with their PTS, which [`ivf_header`] and
//!   [`ivf_frame`] wrap for ffmpeg's IVF reader.
//!
//! One access unit per PES is taken to be one temporal unit, which holds
//! when the encoder makes one shown frame per temporal unit (a low-delay
//! prediction structure, as the transmitter asks SVT-AV1 for).
//!
//! OBU layout, sequence header fields: AV1 Bitstream & Decoding Process
//! Specification (1.0.0 with errata) §5.3 (OBU header), §5.5 (sequence
//! header), §4.10.5 (leb128).

use crate::TS_LEN;

/// The registration descriptor's format identifier (§ "AV1 registration
/// descriptor").
pub const FORMAT_ID: [u8; 4] = *b"AV01";
/// The AV1 video descriptor's tag (§ "AV1 video descriptor").
pub const AV1_DESCRIPTOR_TAG: u8 = 0x80;
/// Private PES (ISO/IEC 13818-1 Table 2-34), the stream type AV1 travels as.
pub const STREAM_TYPE_PRIVATE_PES: u8 = 0x06;

/// OBU types (AV1 §6.2.2, Table "obu_type").
const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;

/// A leb128 number (AV1 §4.10.5): seven bits a byte, low first, the top bit
/// set while more follow. Returns the value and the bytes it took.
pub fn read_leb128(b: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for (i, &x) in b.iter().take(8).enumerate() {
        v |= u64::from(x & 0x7F) << (7 * i);
        if x & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

fn write_leb128(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// One OBU of a buffer in the low-overhead format: its type, and the bytes
/// of the whole OBU (header, size field and payload).
fn next_obu(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let h = *b.first()?;
    let ty = (h >> 3) & 0x0F;
    let ext = h & 0x04 != 0;
    let has_size = h & 0x02 != 0;
    let hdr = 1 + usize::from(ext);
    if b.len() < hdr {
        return None;
    }
    if !has_size {
        // The last OBU of the buffer may leave its size out (§5.2): it runs
        // to the end.
        return Some((ty, b, &[]));
    }
    let (size, n) = read_leb128(&b[hdr..])?;
    let end = hdr + n + usize::try_from(size).ok()?;
    (end <= b.len()).then(|| (ty, &b[..end], &b[end..]))
}

/// Split a temporal unit (low-overhead format) into its OBUs.
pub fn obus(mut tu: &[u8]) -> Vec<(u8, &[u8])> {
    let mut v = Vec::new();
    while let Some((ty, obu, rest)) = next_obu(tu) {
        v.push((ty, obu));
        tu = rest;
    }
    v
}

/// Append `obu` in the start-code format: `00 00 01`, then its bytes with an
/// `03` put in wherever two zeros are followed by a byte of 3 or less, so no
/// start code (nor 00 00 00/02) appears inside (§ "Start-code based
/// format"; the same rule as H.264's emulation prevention).
pub fn push_start_coded(obu: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&[0, 0, 1]);
    let mut zeros = 0;
    for &b in obu {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
}

/// A temporal unit to the start-code format.
pub fn to_start_codes(tu: &[u8], out: &mut Vec<u8>) {
    for (_, obu) in obus(tu) {
        push_start_coded(obu, out);
    }
}

/// The start-code format back to a temporal unit in the low-overhead format,
/// every OBU with its size field (what ffmpeg's IVF and OBU readers want).
pub fn from_start_codes(es: &[u8], out: &mut Vec<u8>) {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= es.len() {
        if es[i] == 0 && es[i + 1] == 0 && es[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut raw = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map_or(es.len(), |&n| n - 3);
        raw.clear();
        let mut zeros = 0;
        for &b in &es[s..end] {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            raw.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        let Some(&h) = raw.first() else { continue };
        let hdr = 1 + usize::from(h & 0x04 != 0);
        if raw.len() < hdr {
            continue;
        }
        if h & 0x02 != 0 {
            // Has its size: take exactly that (zeros before the next start
            // code are not the OBU's).
            if let Some((_, obu, _)) = next_obu(&raw) {
                out.extend_from_slice(obu);
            }
        } else {
            // No size field: give it one. Zero bytes at the end belong to the
            // next start code's run, not to the OBU (its trailing bits end
            // with a one, §5.3.4), unless the payload is empty.
            let mut len = raw.len();
            while len > hdr && raw[len - 1] == 0 {
                len -= 1;
            }
            out.push(h | 0x02);
            out.extend_from_slice(&raw[1..hdr]);
            write_leb128((len - hdr) as u64, out);
            out.extend_from_slice(&raw[hdr..len]);
        }
    }
}

/// MSB-first bit reader for the sequence header.
struct Bits<'a> {
    b: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn f(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.b.get(self.at / 8)?;
            v = (v << 1) | u32::from((byte >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Some(v)
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.f(1)? == 1)
    }

    /// uvlc() (AV1 §4.10.3).
    fn uvlc(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros >= 32 {
                return Some(u32::MAX);
            }
        }
        Some(self.f(zeros)? + ((1u32 << zeros) - 1))
    }
}

/// What the AV1 video descriptor carries, from the sequence header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeqInfo {
    pub profile: u8,
    pub level_idx_0: u8,
    pub tier_0: bool,
    pub high_bitdepth: bool,
    pub twelve_bit: bool,
    pub monochrome: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub chroma_sample_position: u8,
    pub width: u32,
    pub height: u32,
}

impl Default for SeqInfo {
    /// Main profile, 4:2:0, 8 bits, level 5.1 (index 13): what the
    /// transmitter's encoders make, for a PMT sent before the first
    /// sequence header has been seen.
    fn default() -> Self {
        SeqInfo {
            profile: 0,
            level_idx_0: 13,
            tier_0: false,
            high_bitdepth: false,
            twelve_bit: false,
            monochrome: false,
            subsampling_x: true,
            subsampling_y: true,
            chroma_sample_position: 0,
            width: 0,
            height: 0,
        }
    }
}

/// Read a sequence header OBU's payload (AV1 §5.5.1–§5.5.2), as far as the
/// colour config.
pub fn parse_sequence_header(p: &[u8]) -> Option<SeqInfo> {
    let mut r = Bits { b: p, at: 0 };
    let profile = r.f(3)? as u8;
    let _still_picture = r.flag()?;
    let reduced = r.flag()?;
    let (level_idx_0, tier_0);
    if reduced {
        level_idx_0 = r.f(5)? as u8;
        tier_0 = false;
    } else {
        let timing_info = r.flag()?;
        let mut decoder_model_info = false;
        let mut buffer_delay_len = 0;
        if timing_info {
            r.f(32)?; // num_units_in_display_tick
            r.f(32)?; // time_scale
            if r.flag()? {
                r.uvlc()?; // num_ticks_per_picture_minus_1
            }
            decoder_model_info = r.flag()?;
            if decoder_model_info {
                buffer_delay_len = r.f(5)? + 1;
                r.f(32)?; // num_units_in_decoding_tick
                r.f(5)?; // buffer_removal_time_length_minus_1
                r.f(5)?; // frame_presentation_time_length_minus_1
            }
        }
        let initial_display_delay = r.flag()?;
        let ops = r.f(5)? + 1;
        let (mut l0, mut t0) = (0, false);
        for i in 0..ops {
            r.f(12)?; // operating_point_idc
            let level = r.f(5)? as u8;
            let tier = level > 7 && r.flag()?;
            if decoder_model_info && r.flag()? {
                r.f(buffer_delay_len)?; // decoder_buffer_delay
                r.f(buffer_delay_len)?; // encoder_buffer_delay
                r.flag()?; // low_delay_mode_flag
            }
            if initial_display_delay && r.flag()? {
                r.f(4)?;
            }
            if i == 0 {
                (l0, t0) = (level, tier);
            }
        }
        level_idx_0 = l0;
        tier_0 = t0;
    }
    let wbits = r.f(4)? + 1;
    let hbits = r.f(4)? + 1;
    let width = r.f(wbits)? + 1;
    let height = r.f(hbits)? + 1;
    if !reduced && r.flag()? {
        // frame_id_numbers_present_flag
        r.f(4)?;
        r.f(3)?;
    }
    r.f(3)?; // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
    if !reduced {
        r.f(4)?; // interintra, masked compound, warped motion, dual filter
        let order_hint = r.flag()?;
        if order_hint {
            r.f(2)?; // jnt_comp, ref_frame_mvs
        }
        let force_sct = if r.flag()? { 2 } else { r.f(1)? };
        if force_sct > 0 && !r.flag()? {
            r.f(1)?; // seq_force_integer_mv
        }
        if order_hint {
            r.f(3)?;
        }
    }
    r.f(3)?; // superres, cdef, restoration
    // color_config() (§5.5.2)
    let high_bitdepth = r.flag()?;
    let twelve_bit = profile == 2 && high_bitdepth && r.flag()?;
    let monochrome = profile != 1 && r.flag()?;
    let (mut cp, mut tc, mut mc) = (2, 2, 2);
    if r.flag()? {
        cp = r.f(8)?;
        tc = r.f(8)?;
        mc = r.f(8)?;
    }
    let (ss_x, ss_y, mut csp) = if monochrome {
        (true, true, 0)
    } else if cp == 1 && tc == 13 && mc == 0 {
        (false, false, 0)
    } else {
        r.flag()?; // color_range
        let (x, y) = match profile {
            0 => (true, true),
            1 => (false, false),
            _ if twelve_bit => {
                let x = r.flag()?;
                (x, x && r.flag()?)
            }
            _ => (true, false),
        };
        (x, y, 0)
    };
    if !monochrome && ss_x && ss_y && !(cp == 1 && tc == 13 && mc == 0) {
        csp = r.f(2)? as u8;
    }
    Some(SeqInfo {
        profile,
        level_idx_0,
        tier_0,
        high_bitdepth,
        twelve_bit,
        monochrome,
        subsampling_x: ss_x,
        subsampling_y: ss_y,
        chroma_sample_position: csp,
        width,
        height,
    })
}

/// The sequence header in a temporal unit, if it has one.
pub fn find_sequence_header(tu: &[u8]) -> Option<SeqInfo> {
    obus(tu)
        .into_iter()
        .find(|(ty, _)| *ty == OBU_SEQUENCE_HEADER)
        .and_then(|(_, obu)| {
            let hdr = 1 + usize::from(obu[0] & 0x04 != 0);
            let body = if obu[0] & 0x02 != 0 {
                let (_, n) = read_leb128(&obu[hdr..])?;
                &obu[hdr + n..]
            } else {
                &obu[hdr..]
            };
            parse_sequence_header(body)
        })
}

/// The ES loop's two descriptors for an AV1 stream: the registration
/// descriptor (tag 05h, `AV01`), then the AV1 video descriptor (tag 80h,
/// four bytes; § "AV1 video descriptor", hdr_wcg_idc 0 = SDR).
pub fn es_descriptors(s: &SeqInfo) -> [u8; 12] {
    let b = |v: bool| u8::from(v);
    [
        0x05,
        4,
        FORMAT_ID[0],
        FORMAT_ID[1],
        FORMAT_ID[2],
        FORMAT_ID[3],
        AV1_DESCRIPTOR_TAG,
        4,
        0x80 | 1, // marker, version 1
        (s.profile << 5) | (s.level_idx_0 & 0x1F),
        (b(s.tier_0) << 7)
            | (b(s.high_bitdepth) << 6)
            | (b(s.twelve_bit) << 5)
            | (b(s.monochrome) << 4)
            | (b(s.subsampling_x) << 3)
            | (b(s.subsampling_y) << 2)
            | (s.chroma_sample_position & 3),
        // hdr_wcg_idc 0, reserved_zeros, no initial presentation delay.
        0x00,
    ]
}

/// Whether an ES descriptor loop marks the stream as AV1 (a registration
/// descriptor with `AV01`).
pub fn is_av1(descriptors: &[u8]) -> bool {
    let mut d = descriptors;
    while d.len() >= 2 {
        let (tag, len) = (d[0], d[1] as usize);
        if d.len() < 2 + len {
            break;
        }
        if tag == 0x05 && len >= 4 && d[2..6] == FORMAT_ID {
            return true;
        }
        d = &d[2 + len..];
    }
    false
}

/// MPEG-2 CRC-32 (ISO/IEC 13818-1 Annex A): polynomial 04C11DB7h, no
/// reflection, initial all ones.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn pid_of(p: &[u8; TS_LEN]) -> u16 {
    u16::from_be_bytes([p[1] & 0x1F, p[2]])
}

/// Where a packet's payload starts, if it has one.
fn payload_start(p: &[u8; TS_LEN]) -> Option<usize> {
    let afc = (p[3] >> 4) & 3;
    match afc {
        1 => Some(4),
        3 => {
            let at = 5 + p[4] as usize;
            (at <= TS_LEN).then_some(at)
        }
        _ => None,
    }
}

/// The 27 MHz PCR of a packet that carries one.
fn pcr(p: &[u8; TS_LEN]) -> Option<u64> {
    let afc = (p[3] >> 4) & 3;
    if afc < 2 || p[4] < 7 || p[5] & 0x10 == 0 {
        return None;
    }
    let base = (u64::from(p[6]) << 25)
        | (u64::from(p[7]) << 17)
        | (u64::from(p[8]) << 9)
        | (u64::from(p[9]) << 1)
        | (u64::from(p[10]) >> 7);
    let ext = (u64::from(p[10] & 1) << 8) | u64::from(p[11]);
    Some(base * 300 + ext)
}

/// A packet with only an adaptation field holding `pcr` (27 MHz): no
/// payload, so the continuity counter does not move (ISO/IEC 13818-1
/// §2.4.3.3).
fn pcr_packet(pid: u16, cc: u8, pcr: u64) -> [u8; TS_LEN] {
    const WRAP: u64 = (1 << 33) * 300;
    let pcr = pcr % WRAP;
    let (base, ext) = (pcr / 300, pcr % 300);
    let mut p = [0xFFu8; TS_LEN];
    p[0] = 0x47;
    p[1] = (pid >> 8) as u8 & 0x1F;
    p[2] = pid as u8;
    p[3] = 0x20 | (cc & 0x0F);
    p[4] = 183;
    p[5] = 0x10; // PCR_flag
    p[6] = (base >> 25) as u8;
    p[7] = (base >> 17) as u8;
    p[8] = (base >> 9) as u8;
    p[9] = (base >> 1) as u8;
    p[10] = ((base & 1) as u8) << 7 | 0x7E | ((ext >> 8) as u8 & 1);
    p[11] = ext as u8;
    p
}

/// The transmit side: ffmpeg's transport stream in, AV1 made conformant.
///
/// The AV1 stream's packets are held until its PES is whole (the next
/// payload start, or the PES length reached), converted and sent again as
/// new packets. That moves them later in the multiplex and makes them a
/// little longer, so:
///
/// - a PCR the stream carried goes out at once, where it was, in a packet
///   of its own (adaptation field only), its value corrected by how far the
///   output is ahead of or behind the input at that point (a packet is
///   `ticks_per_packet` of the 27 MHz clock at the channel's rate);
/// - the null packets ffmpeg pads its constant rate with are dropped while
///   the output is ahead, so the stream keeps its rate.
pub struct Av1TsFixer {
    pmt_pid: u16,
    av1_pid: u16,
    ticks_per_packet: f64,
    seq: SeqInfo,
    seq_seen: bool,
    pes: Vec<u8>,
    /// The PES being gathered: its first packet's adaptation field flags
    /// (random access, priority), whether it started at all.
    pes_af_flags: u8,
    pes_open: bool,
    pes_len: Option<usize>,
    cc: u8,
    in_count: u64,
    out_count: u64,
    es: Vec<u8>,
    /// Packets dropped because they could not be made sense of.
    pub dropped_pes: u64,
}

impl Av1TsFixer {
    /// `rate_bps`: the channel's TS rate, which the output fills packet for
    /// packet.
    pub fn new(pmt_pid: u16, av1_pid: u16, rate_bps: f64) -> Self {
        Av1TsFixer {
            pmt_pid,
            av1_pid,
            ticks_per_packet: (TS_LEN * 8) as f64 / rate_bps * 27e6,
            seq: SeqInfo::default(),
            seq_seen: false,
            pes: Vec::new(),
            pes_af_flags: 0,
            pes_open: false,
            pes_len: None,
            cc: 0,
            in_count: 0,
            out_count: 0,
            es: Vec::new(),
            dropped_pes: 0,
        }
    }

    /// The sequence header's fields, once one has been seen.
    pub fn sequence(&self) -> Option<SeqInfo> {
        self.seq_seen.then_some(self.seq)
    }

    /// Take one input packet; append what goes out now to `out`.
    pub fn push(&mut self, p: &[u8; TS_LEN], out: &mut Vec<[u8; TS_LEN]>) {
        self.in_count += 1;
        let pid = pid_of(p);
        if pid == 0x1FFF && self.out_count >= self.in_count {
            // The output is ahead: a null packet's slot is taken.
            return;
        }
        if pid == self.pmt_pid {
            let mut q = *p;
            self.fix_pmt(&mut q);
            self.emit(q, out);
        } else if pid == self.av1_pid {
            self.av1_packet(p, out);
        } else {
            self.emit(*p, out);
        }
    }

    fn emit(&mut self, p: [u8; TS_LEN], out: &mut Vec<[u8; TS_LEN]>) {
        self.out_count += 1;
        out.push(p);
    }

    fn av1_packet(&mut self, p: &[u8; TS_LEN], out: &mut Vec<[u8; TS_LEN]>) {
        if let Some(v) = pcr(p) {
            // Out where it came, corrected for the output's lead or lag. The
            // emitted packet itself takes the slot that this one had.
            let lead = self.out_count as f64 - (self.in_count - 1) as f64;
            let v = (v as f64 + lead * self.ticks_per_packet).round().max(0.0) as u64;
            // No payload, so it repeats the last counter value.
            let q = pcr_packet(self.av1_pid, self.cc.wrapping_sub(1) & 0x0F, v);
            self.emit(q, out);
        }
        let Some(at) = payload_start(p) else {
            return;
        };
        let pusi = p[1] & 0x40 != 0;
        if pusi {
            self.flush(out);
            self.pes_open = true;
            let afc = (p[3] >> 4) & 3;
            self.pes_af_flags = if afc >= 2 && p[4] > 0 { p[5] & 0x60 } else { 0 };
        }
        if !self.pes_open {
            return;
        }
        self.pes.extend_from_slice(&p[at..]);
        if pusi && self.pes.len() >= 6 {
            let l = u16::from_be_bytes([self.pes[4], self.pes[5]]) as usize;
            self.pes_len = (l > 0).then_some(6 + l);
        }
        if self.pes_len.is_some_and(|l| self.pes.len() >= l) {
            self.flush(out);
        }
    }

    /// Convert and send the PES gathered so far.
    fn flush(&mut self, out: &mut Vec<[u8; TS_LEN]>) {
        if !self.pes_open {
            return;
        }
        self.pes_open = false;
        let mut pes = std::mem::take(&mut self.pes);
        if let Some(l) = self.pes_len.take() {
            pes.truncate(l);
        }
        let ok = pes.len() >= 9 && pes[..3] == [0, 0, 1];
        let hdr_end = if ok { 9 + pes[8] as usize } else { 0 };
        if !ok || hdr_end > pes.len() {
            self.dropped_pes += 1;
            self.pes = pes;
            self.pes.clear();
            return;
        }
        let tu = &pes[hdr_end..];
        if let Some(s) = find_sequence_header(tu) {
            self.seq = s;
            self.seq_seen = true;
        }
        self.es.clear();
        // Temporal delimiters "may be removed" (§ "General constraints"); the
        // PES boundary marks the unit.
        for (ty, obu) in obus(tu) {
            if ty != OBU_TEMPORAL_DELIMITER {
                push_start_coded(obu, &mut self.es);
            }
        }
        let mut np = Vec::with_capacity(hdr_end + self.es.len());
        np.extend_from_slice(&pes[..hdr_end]);
        np[3] = 0xBD; // private_stream_1 (§ "Use of PES packets")
        np[6] |= 0x04; // data_alignment_indicator
        np.extend_from_slice(&self.es);
        let len = np.len() - 6;
        let len = if len <= 0xFFFF { len as u16 } else { 0 };
        np[4..6].copy_from_slice(&len.to_be_bytes());
        // Into packets: the first starts the unit and carries the random
        // access flags ffmpeg gave; the last is stuffed by its adaptation
        // field.
        let mut rest = &np[..];
        let mut first = true;
        while !rest.is_empty() {
            let mut q = [0xFFu8; TS_LEN];
            q[0] = 0x47;
            q[1] = (self.av1_pid >> 8) as u8 & 0x1F | if first { 0x40 } else { 0 };
            q[2] = self.av1_pid as u8;
            let flags = if first { self.pes_af_flags } else { 0 };
            let room_plain = TS_LEN - 4;
            let need_af = flags != 0 || rest.len() < room_plain;
            let take;
            if need_af {
                // Adaptation field: length byte, flags byte, stuffing.
                let min_af = if flags != 0 { 2 } else { 1 };
                take = rest.len().min(TS_LEN - 4 - min_af);
                let af_len = TS_LEN - 4 - 1 - take; // bytes after the length byte
                q[3] = 0x30 | self.cc;
                q[4] = af_len as u8;
                if af_len > 0 {
                    q[5] = flags;
                }
                let at = 5 + af_len;
                q[at..at + take].copy_from_slice(&rest[..take]);
            } else {
                take = room_plain;
                q[3] = 0x10 | self.cc;
                q[4..].copy_from_slice(&rest[..take]);
            }
            self.cc = (self.cc + 1) & 0x0F;
            rest = &rest[take..];
            first = false;
            self.emit(q, out);
        }
        self.pes = pes;
        self.pes.clear();
    }

    /// Put the two descriptors into the AV1 stream's entry of a one-packet
    /// PMT, and correct its CRC. Left alone if it is not such a PMT.
    fn fix_pmt(&mut self, p: &mut [u8; TS_LEN]) {
        if p[1] & 0x40 == 0 {
            return;
        }
        let Some(at) = payload_start(p) else { return };
        let s = at + 1 + p[at] as usize; // after the pointer field
        if s + 12 > TS_LEN || p[s] != 0x02 {
            return;
        }
        let sec_len = (usize::from(p[s + 1] & 0x0F) << 8) | p[s + 2] as usize;
        let end = s + 3 + sec_len;
        if end > TS_LEN || sec_len < 13 {
            return;
        }
        let sec = p[s..end].to_vec();
        let prog_info = (usize::from(sec[10] & 0x0F) << 8) | sec[11] as usize;
        let mut body = sec[..12 + prog_info].to_vec();
        let mut j = 12 + prog_info;
        let desc = es_descriptors(&self.seq);
        while j + 5 <= sec.len() - 4 {
            let es_pid = u16::from_be_bytes([sec[j + 1] & 0x1F, sec[j + 2]]);
            let info = (usize::from(sec[j + 3] & 0x0F) << 8) | sec[j + 4] as usize;
            let loop_end = (j + 5 + info).min(sec.len() - 4);
            let mut entry = sec[j..loop_end].to_vec();
            if es_pid == self.av1_pid && !is_av1(&entry[5..]) {
                entry[0] = STREAM_TYPE_PRIVATE_PES;
                // The registration descriptor first, then the AV1 one, then
                // whatever ffmpeg had.
                let mut d = desc.to_vec();
                d.extend_from_slice(&entry[5..]);
                entry.truncate(5);
                entry.extend_from_slice(&d);
                let n = entry.len() - 5;
                entry[3] = 0xF0 | (n >> 8) as u8;
                entry[4] = n as u8;
            }
            body.extend_from_slice(&entry);
            j = loop_end;
        }
        let n = body.len() - 3 + 4;
        if s + body.len() + 4 > TS_LEN {
            return;
        }
        body[1] = (body[1] & 0xF0) | (n >> 8) as u8;
        body[2] = n as u8;
        let crc = crc32(&body);
        body.extend_from_slice(&crc.to_be_bytes());
        p[s..s + body.len()].copy_from_slice(&body);
        for b in &mut p[s + body.len()..] {
            *b = 0xFF;
        }
    }
}

/// One temporal unit taken out of the transport stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Av1Unit {
    /// 90 kHz, from the PES header.
    pub pts: Option<u64>,
    /// The temporal unit, low-overhead format, a temporal delimiter first.
    pub tu: Vec<u8>,
}

/// The receive side: one PID's PES packets in, temporal units out.
#[derive(Default)]
pub struct Av1Extractor {
    pes: Vec<u8>,
    open: bool,
    cc: Option<u8>,
    pub units: u64,
    pub broken: u64,
}

impl Av1Extractor {
    pub fn new() -> Self {
        Av1Extractor::default()
    }

    /// Take a packet of the AV1 PID; a unit that has ended comes back.
    pub fn push(&mut self, p: &[u8; TS_LEN]) -> Option<Av1Unit> {
        let at = payload_start(p)?;
        let cc = p[3] & 0x0F;
        let mut done = None;
        if p[1] & 0x40 != 0 {
            done = self.finish();
            self.open = true;
        } else if let Some(prev) = self.cc
            && cc != (prev + 1) & 0x0F
        {
            // A lost packet: this unit cannot be decoded.
            self.open = false;
            self.pes.clear();
            self.broken += 1;
        }
        self.cc = Some(cc);
        if self.open {
            self.pes.extend_from_slice(&p[at..]);
        }
        done
    }

    /// The unit being gathered, ended now (the stream has stopped).
    pub fn finish(&mut self) -> Option<Av1Unit> {
        if !self.open {
            self.pes.clear();
            return None;
        }
        self.open = false;
        let pes = std::mem::take(&mut self.pes);
        if pes.len() < 9 || pes[..3] != [0, 0, 1] {
            self.broken += 1;
            return None;
        }
        let hdr_end = 9 + pes[8] as usize;
        if hdr_end > pes.len() {
            self.broken += 1;
            return None;
        }
        let pts = (pes[7] & 0x80 != 0 && pes.len() >= 14).then(|| {
            (u64::from(pes[9] >> 1 & 7) << 30)
                | (u64::from(pes[10]) << 22)
                | (u64::from(pes[11] >> 1) << 15)
                | (u64::from(pes[12]) << 7)
                | u64::from(pes[13] >> 1)
        });
        let l = u16::from_be_bytes([pes[4], pes[5]]) as usize;
        let end = if l > 0 {
            (6 + l).min(pes.len())
        } else {
            pes.len()
        };
        // A temporal delimiter first: the IVF and OBU readers expect one.
        let mut tu = vec![0x12, 0x00];
        from_start_codes(&pes[hdr_end..end], &mut tu);
        self.units += 1;
        self.pes = pes;
        self.pes.clear();
        Some(Av1Unit { pts, tu })
    }
}

/// An IVF file header (32 bytes) for AV1 with a 90 kHz time base, as
/// libvpx/libaom write it.
pub fn ivf_header(width: u16, height: u16) -> [u8; 32] {
    let mut h = [0u8; 32];
    h[..4].copy_from_slice(b"DKIF");
    h[6..8].copy_from_slice(&32u16.to_le_bytes());
    h[8..12].copy_from_slice(b"AV01");
    h[12..14].copy_from_slice(&width.to_le_bytes());
    h[14..16].copy_from_slice(&height.to_le_bytes());
    h[16..20].copy_from_slice(&90_000u32.to_le_bytes()); // rate
    h[20..24].copy_from_slice(&1u32.to_le_bytes()); // scale
    h
}

/// One IVF frame: its size, its timestamp (time base units), its bytes.
pub fn ivf_frame(pts: u64, data: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&pts.to_le_bytes());
    out.extend_from_slice(data);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An OBU with a size field.
    fn obu(ty: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![(ty << 3) | 0x02];
        write_leb128(payload.len() as u64, &mut v);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn leb128_round_trip() {
        for v in [0u64, 1, 127, 128, 300, 1 << 20, 123_456_789] {
            let mut b = Vec::new();
            write_leb128(v, &mut b);
            assert_eq!(read_leb128(&b), Some((v, b.len())));
        }
    }

    #[test]
    fn start_codes_round_trip_with_emulation_prevention() {
        // Payloads full of the patterns that must be escaped.
        let a = obu(6, &[0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 7, 0, 0, 0]);
        let b = obu(4, &[5, 0, 0, 1, 0, 0]);
        let tu: Vec<u8> = [a.clone(), b.clone()].concat();
        let mut sc = Vec::new();
        to_start_codes(&tu, &mut sc);
        // No start code but the two we put in, and no 00 00 00 or 00 00 02
        // inside either OBU's payload (a payload's last zero may meet the
        // next start code's zeros, which a reader takes as the start code).
        let n = sc.windows(3).filter(|w| *w == [0, 0, 1]).count();
        assert_eq!(n, 2, "{sc:02x?}");
        let second = 3 + sc[3..].windows(3).position(|w| w == [0, 0, 1]).unwrap();
        for part in [&sc[3..second], &sc[second + 3..]] {
            let body = &part[..part.len() - 1];
            assert!(!body.windows(3).any(|w| w == [0, 0, 0] || w == [0, 0, 2]));
        }
        let mut back = Vec::new();
        from_start_codes(&sc, &mut back);
        assert_eq!(back, tu);
    }

    #[test]
    fn an_obu_without_its_size_gets_one() {
        let raw = [6u8 << 3, 9, 8, 7];
        let mut sc = Vec::new();
        push_start_coded(&raw, &mut sc);
        let mut back = Vec::new();
        from_start_codes(&sc, &mut back);
        assert_eq!(back, obu(6, &[9, 8, 7]));
    }

    #[test]
    fn reads_a_sequence_header() {
        // SVT-AV1's for 640x360, main profile, level 3.0 (idx 4), 4:2:0.
        // Built by hand: profile 0, not still, not reduced, no timing info,
        // no initial display delay, one operating point (idc 0, level 4),
        // 10+9 bit sizes, 639 x 359, flags off, 8-bit, no colour description.
        let mut bits = String::new();
        bits += "000"; // profile
        bits += "0"; // still
        bits += "0"; // reduced
        bits += "0"; // timing_info_present
        bits += "0"; // initial_display_delay_present
        bits += "00000"; // operating_points_cnt_minus_1
        bits += "000000000000"; // idc
        bits += "00100"; // level 4
        bits += "1001"; // frame_width_bits_minus_1 = 9
        bits += "1000"; // frame_height_bits_minus_1 = 8
        bits += &format!("{:010b}", 639);
        bits += &format!("{:09b}", 359);
        bits += "0"; // frame_id_numbers_present
        bits += "000"; // 128x128, filter intra, intra edge
        bits += "0000"; // interintra, masked, warped, dual filter
        bits += "1"; // enable_order_hint
        bits += "00"; // jnt_comp, ref_frame_mvs
        bits += "1"; // seq_choose_screen_content_tools -> 2
        bits += "1"; // seq_choose_integer_mv
        bits += "110"; // order_hint_bits_minus_1
        bits += "000"; // superres, cdef, restoration
        bits += "0"; // high_bitdepth
        bits += "0"; // mono
        bits += "0"; // colour description
        bits += "0"; // colour range
        bits += "10"; // chroma_sample_position
        while !bits.len().is_multiple_of(8) {
            bits += "0";
        }
        let bytes: Vec<u8> = bits
            .as_bytes()
            .chunks(8)
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 2).unwrap())
            .collect();
        let s = parse_sequence_header(&bytes).unwrap();
        assert_eq!((s.width, s.height), (640, 360));
        assert_eq!((s.profile, s.level_idx_0, s.tier_0), (0, 4, false));
        assert!(s.subsampling_x && s.subsampling_y);
        assert_eq!(s.chroma_sample_position, 2);
        let tu = [obu(2, &[]), obu(1, &bytes)].concat();
        assert_eq!(find_sequence_header(&tu), Some(s));
        let d = es_descriptors(&s);
        assert_eq!(&d[..6], &[0x05, 4, b'A', b'V', b'0', b'1']);
        assert_eq!(&d[6..9], &[0x80, 4, 0x81]);
        assert_eq!(d[9], 4); // profile 0, level 4
        assert_eq!(d[10], 0b0000_1110);
        assert!(is_av1(&d));
    }

    /// A PES of `tu` on `pid`, as ffmpeg writes private data: one or more
    /// packets, a PTS.
    fn ffmpeg_like(pid: u16, cc: &mut u8, pts: u64, tu: &[u8], rai: bool) -> Vec<[u8; TS_LEN]> {
        let mut pes = vec![0, 0, 1, 0xBD, 0, 0, 0x80, 0x80, 5];
        pes.push(0x21 | ((pts >> 29) as u8 & 0x0E));
        pes.push((pts >> 22) as u8);
        pes.push(((pts >> 14) as u8 & 0xFE) | 1);
        pes.push((pts >> 7) as u8);
        pes.push(((pts << 1) as u8) | 1);
        pes.extend_from_slice(tu);
        let l = (pes.len() - 6) as u16;
        pes[4..6].copy_from_slice(&l.to_be_bytes());
        let mut v = Vec::new();
        let mut rest = &pes[..];
        let mut first = true;
        while !rest.is_empty() {
            let mut q = [0xFFu8; TS_LEN];
            q[0] = 0x47;
            q[1] = (pid >> 8) as u8 | if first { 0x40 } else { 0 };
            q[2] = pid as u8;
            let take = rest.len().min(if first && rai { 182 } else { 184 });
            if take < 184 || (first && rai) {
                let af = 183 - take;
                q[3] = 0x30 | *cc;
                q[4] = af as u8;
                if af > 0 {
                    q[5] = if first && rai { 0x40 } else { 0 };
                }
                q[5 + af..].copy_from_slice(&rest[..take]);
            } else {
                q[3] = 0x10 | *cc;
                q[4..].copy_from_slice(&rest[..take]);
            }
            *cc = (*cc + 1) & 15;
            rest = &rest[take..];
            first = false;
            v.push(q);
        }
        v
    }

    #[test]
    fn the_fixer_and_the_extractor_agree() {
        let pid = 256;
        let td = obu(2, &[]);
        let frames: Vec<Vec<u8>> = (0..6u8)
            .map(|k| {
                let body: Vec<u8> = (0..400u16)
                    .map(|i| if i % 37 < 3 { 0 } else { (i as u8) ^ k })
                    .collect();
                [td.clone(), obu(6, &body)].concat()
            })
            .collect();
        let mut input = Vec::new();
        let mut cc = 0;
        for (k, f) in frames.iter().enumerate() {
            input.extend(ffmpeg_like(
                pid,
                &mut cc,
                90_000 + 3600 * k as u64,
                f,
                k == 0,
            ));
            for _ in 0..3 {
                input.push(crate::null_packet());
            }
        }
        let mut fx = Av1TsFixer::new(4095, pid, 500_000.0);
        let mut out = Vec::new();
        for p in &input {
            fx.push(p, &mut out);
        }
        // Rate kept: nulls gave way to the longer packets.
        assert!(
            out.len() <= input.len() + 1,
            "{} vs {}",
            out.len(),
            input.len()
        );
        // The first output packet keeps the random access flag.
        let first = out.iter().find(|p| pid_of(p) == pid).unwrap();
        assert_eq!(first[5] & 0x40, 0x40);
        let mut ex = Av1Extractor::new();
        let mut got = Vec::new();
        for p in out.iter().filter(|p| pid_of(p) == pid) {
            got.extend(ex.push(p));
        }
        got.extend(ex.finish());
        assert_eq!(got.len(), frames.len());
        for (k, u) in got.iter().enumerate() {
            assert_eq!(u.pts, Some(90_000 + 3600 * k as u64));
            assert_eq!(u.tu, frames[k], "unit {k}");
        }
        assert_eq!(ex.broken, 0);
    }

    #[test]
    fn the_pmt_gets_the_descriptors_and_a_good_crc() {
        // PMT: program 1, PCR PID 256, one stream: type 06h on 256.
        let mut sec = vec![0x02, 0xB0, 0, 0, 1, 0xC1, 0, 0, 0xE1, 0x00, 0xF0, 0];
        sec.extend_from_slice(&[0x06, 0xE1, 0x00, 0xF0, 0x00]);
        let n = sec.len() - 3 + 4;
        sec[2] = n as u8;
        let crc = crc32(&sec);
        sec.extend_from_slice(&crc.to_be_bytes());
        let mut p = [0xFFu8; TS_LEN];
        p[..5].copy_from_slice(&[0x47, 0x4F, 0xFF, 0x10, 0x00]);
        p[5..5 + sec.len()].copy_from_slice(&sec);
        let mut fx = Av1TsFixer::new(0x0FFF, 256, 1e6);
        let mut out = Vec::new();
        fx.push(&p, &mut out);
        let q = out[0];
        let len = ((q[6] as usize & 0x0F) << 8) | q[7] as usize;
        let s = &q[5..5 + 3 + len];
        assert_eq!(crc32(s), 0, "a section with its CRC checks to zero");
        assert!(is_av1(&s[17..17 + 12]));
    }
}
