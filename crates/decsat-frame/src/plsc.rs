//! The physical-layer signalling code: 8 bits that describe the frame.
//!
//! ETSI EN 302 307-1 §5.5.2, extended by EN 302 307-2 §5.5.2. The PLS code
//! is the whole basis of ACM: every PLFRAME announces its own MODCOD,
//! FECFRAME length and whether it carries pilots, so a receiver can follow a
//! transmitter that changes coding from one frame to the next without being
//! told anything in advance.
//!
//! Layout of the dataword `b0 … b7`:
//! - `b0 = 0`, DVB-S2: `0 | MODCOD (5 bits) | short FECFRAME | pilots`, the
//!   S2 code unchanged.
//! - `b0 = 1`, DVB-S2X: `1 | MODCOD and FECFRAME (6 bits) | pilots`, the
//!   MODCODs of EN 302 307-2 Table 17a, the VL-SNR frames (codes 129 and
//!   131) and codes reserved with a known length (Table 17b). The 64 PLS-code
//!   symbols of an S2X header are also turned by +90° against the SOF.
//!
//! Frame-geometry derivation cross-checked against `gr-dvbs2rx`'s
//! `lib/pl_signaling.cc` (GPL-3), and for S2X against `gr-dtv`'s
//! `dvbs2_physical_cc_impl.cc` (GPL-3).

use decsat_core::{FecFrame, Iq, Modcod, modcod};

use crate::defs::{PILOT_BLK_LEN, PLSC_LEN, PLSC_SCRAMBLER, SLOT_LEN, SLOTS_PER_PILOT_BLK};
use crate::pi2bpsk::{demap_bpsk_diff, derotate_bpsk_iq, map_bpsk};
use crate::rm::ReedMuller;

/// Everything the PLS code tells us about a PLFRAME.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlsInfo {
    /// The raw 8-bit PLS code; its MSB (`b0`) is set for S2X.
    pub plsc: u8,
    /// The MODCOD number ([`decsat_core::modcod`]): S2's 5-bit field
    /// (0 = a dummy frame), or the S2X PLS code with its pilot bit clear.
    pub modcod: u8,
    /// The FECFRAME length the code announces.
    pub frame: FecFrame,
    /// Short FECFRAME (16 200 bits).
    pub short_fecframe: bool,
    /// The PLFRAME carries pilot blocks.
    pub has_pilots: bool,
    /// A dummy frame: no payload, sent to fill time when there is no data.
    pub dummy_frame: bool,
    /// An S2X VL-SNR frame (codes 129, 131): set 1 or 2. Its MODCOD is in
    /// the VL-SNR header after the PLHEADER; its length is that of normal
    /// QPSK (set 1) or 16APSK (set 2) with pilots, and so is the layout of
    /// its regular pilots.
    pub vlsnr: Option<u8>,
    /// An S2X code reserved for future use (Table 17b): its length is known,
    /// so lock holds through it, but not what it carries.
    pub reserved: bool,
    /// Bits per constellation symbol (0 for a dummy frame).
    pub n_mod: u8,
    /// Payload slots of 90 symbols.
    pub n_slots: u16,
    /// Number of pilot blocks.
    pub n_pilots: u8,
    /// Whole PLFRAME length in symbols: PLHEADER + payload + pilots.
    pub plframe_len: u32,
    /// Payload length in symbols: data plus pilots, excluding the PLHEADER.
    pub payload_len: u32,
    /// XFECFRAME length in symbols: data only, no pilots.
    pub xfecframe_len: u32,
}

impl PlsInfo {
    /// Derive the frame geometry from an 8-bit PLS code.
    pub fn parse(plsc: u8) -> Self {
        if plsc & 0x80 != 0 {
            return Self::parse_s2x(plsc);
        }
        let modcod = plsc >> 2;
        let short_fecframe = plsc & 0x2 != 0;
        let dummy_frame = modcod == 0;
        // A dummy frame never carries pilots, whatever the bit says.
        let has_pilots = (plsc & 0x1 != 0) && !dummy_frame;

        // Slots per frame follow from the bits per symbol: a normal FECFRAME is
        // 64 800 bits, and a slot is 90 symbols, so S = 64800 / (90 * n_mod).
        let (n_mod, mut n_slots): (u8, u16) = match modcod {
            1..=11 => (2, 360),  // QPSK
            12..=17 => (3, 240), // 8PSK
            18..=23 => (4, 180), // 16APSK
            24..=28 => (5, 144), // 32APSK
            _ => (0, 36),        // dummy frame (and the reserved indexes)
        };

        // A short FECFRAME is a quarter the length, so a quarter the slots.
        if short_fecframe && !dummy_frame {
            n_slots >>= 2;
        }
        let frame = if short_fecframe {
            FecFrame::Short
        } else {
            FecFrame::Normal
        };
        Self::geometry(plsc, modcod, frame, has_pilots, dummy_frame, n_mod, n_slots)
            .with(None, false)
    }

    /// An S2X code (EN 302 307-2 §5.5.2.2): a Table 17a MODCOD, a VL-SNR
    /// frame, or a reserved code of known length (Table 17b).
    fn parse_s2x(plsc: u8) -> Self {
        let pilots = plsc & 1 != 0;
        // (bits per symbol, pilots, VL-SNR set, reserved).
        let (n_mod, frame, has_pilots, vlsnr, reserved) = match plsc {
            // VL-SNR: pilots always on; the geometry of normal QPSK and
            // 16APSK with pilots (§5.5.2.0, Figures 17 and 18).
            129 => (2, FecFrame::Normal, true, Some(1), false),
            131 => (4, FecFrame::Normal, true, Some(2), false),
            // Table 17b, n-ary normal frames, pilots off then on.
            128 => (3, FecFrame::Normal, false, None, true),
            130 => (4, FecFrame::Normal, false, None, true),
            176 => (5, FecFrame::Normal, false, None, true),
            177 => (5, FecFrame::Normal, true, None, true),
            188 | 192 | 196 => (6, FecFrame::Normal, false, None, true),
            189 | 193 | 197 => (6, FecFrame::Normal, true, None, true),
            250 => (3, FecFrame::Normal, true, None, true),
            251 => (4, FecFrame::Normal, true, None, true),
            252 => (5, FecFrame::Normal, true, None, true),
            253 => (6, FecFrame::Normal, true, None, true),
            254 => (8, FecFrame::Normal, true, None, true),
            255 => (10, FecFrame::Normal, true, None, true),
            _ => match modcod(plsc, FecFrame::Normal) {
                Some(m) => (m.modulation.bits_per_symbol(), m.frame, pilots, None, false),
                // Every S2X code is one of the above; this is unreachable
                // short of a table error, and the frame is skipped.
                None => (2, FecFrame::Normal, pilots, None, true),
            },
        };
        // S = ceil(N / (90 · n_mod)): 128APSK pads its 64 800 bits to 103
        // slots (§5.3.2.2, Table 16).
        let per_slot = SLOT_LEN * n_mod as usize;
        let n_slots = frame.n_ldpc().div_ceil(per_slot) as u16;
        Self::geometry(plsc, plsc & 0xFE, frame, has_pilots, false, n_mod, n_slots)
            .with(vlsnr, reserved)
    }

    fn with(mut self, vlsnr: Option<u8>, reserved: bool) -> Self {
        self.vlsnr = vlsnr;
        self.reserved = reserved;
        self
    }

    /// The lengths that follow from the slot count and the pilots.
    fn geometry(
        plsc: u8,
        modcod: u8,
        frame: FecFrame,
        has_pilots: bool,
        dummy_frame: bool,
        n_mod: u8,
        n_slots: u16,
    ) -> Self {
        // One pilot block after every 16 slots, but not a trailing one.
        let n_pilots = if has_pilots {
            ((n_slots as usize - 1) / SLOTS_PER_PILOT_BLK) as u8
        } else {
            0
        };

        let xfecframe_len = n_slots as u32 * SLOT_LEN as u32;
        // The PLHEADER is itself exactly one slot long.
        let plframe_len =
            (n_slots as u32 + 1) * SLOT_LEN as u32 + n_pilots as u32 * PILOT_BLK_LEN as u32;

        PlsInfo {
            plsc,
            modcod,
            frame,
            short_fecframe: frame == FecFrame::Short,
            has_pilots,
            dummy_frame,
            vlsnr: None,
            reserved: false,
            n_mod,
            n_slots,
            n_pilots,
            plframe_len,
            payload_len: plframe_len - SLOT_LEN as u32,
            xfecframe_len,
        }
    }

    /// The frame as a super-frame of format 0 or 1 carries it (EN 302 307-2
    /// §E.3.2.1): no pilot blocks of its own. Its pilot bit (kept in `plsc`)
    /// says instead whether the super-frame's own pilots are on. VL-SNR
    /// frames, whose pilots Annex E redefines, are left as they are.
    pub fn without_pilot_blocks(self) -> Self {
        if !self.has_pilots || self.vlsnr.is_some() {
            return self;
        }
        Self::geometry(
            self.plsc,
            self.modcod,
            self.frame,
            false,
            self.dummy_frame,
            self.n_mod,
            self.n_slots,
        )
        .with(self.vlsnr, self.reserved)
    }

    /// An S2 frame from its fields.
    pub fn from_fields(modcod: u8, short_fecframe: bool, has_pilots: bool) -> Self {
        Self::parse(((modcod & 0x1F) << 2) | ((short_fecframe as u8) << 1) | has_pilots as u8)
    }

    /// An S2X frame from its PLS code (the pilot bit is set from `pilots`).
    pub fn s2x(pls: u8, has_pilots: bool) -> Self {
        Self::parse(0x80 | (pls & 0xFE) | has_pilots as u8)
    }

    /// Any frame from a MODCOD number ([`decsat_core::modcod`]).
    pub fn for_modcod(modcod: u8, short_fecframe: bool, has_pilots: bool) -> Self {
        if modcod >= 128 {
            Self::s2x(modcod, has_pilots)
        } else {
            Self::from_fields(modcod, short_fecframe, has_pilots)
        }
    }

    /// An S2X code.
    pub fn is_s2x(&self) -> bool {
        self.plsc & 0x80 != 0
    }

    /// The frame's MODCOD, when it is a data frame of a known MODCOD (not a
    /// dummy, VL-SNR or reserved frame).
    pub fn modcod(&self) -> Option<Modcod> {
        if self.dummy_frame || self.vlsnr.is_some() || self.reserved {
            return None;
        }
        modcod(self.modcod, self.frame)
    }
}

/// Encodes a PLS code into its 64 pi/2-BPSK symbols.
pub struct PlscEncoder {
    rm: ReedMuller,
}

impl Default for PlscEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl PlscEncoder {
    pub fn new() -> Self {
        PlscEncoder {
            rm: ReedMuller::new(PLSC_SCRAMBLER),
        }
    }

    /// Write the 64 PLS symbols for an 8-bit code: pi/2-BPSK continuing the
    /// SOF's, turned by +90° for an S2X code (EN 302 307-2 §5.5.2.0).
    ///
    /// # Panics
    /// If `out` is shorter than 64.
    pub fn encode(&self, plsc: u8, out: &mut [Iq]) {
        map_bpsk(self.rm.encode(plsc), out, PLSC_LEN);
        if plsc & 0x80 != 0 {
            for s in &mut out[..PLSC_LEN] {
                *s = Iq::new(-s.im, s.re);
            }
        }
    }

    /// Write the 64 PLS symbols for the given fields.
    pub fn encode_fields(
        &self,
        modcod: u8,
        short_fecframe: bool,
        has_pilots: bool,
        out: &mut [Iq],
    ) {
        let plsc = ((modcod & 0x1F) << 2) | ((short_fecframe as u8) << 1) | has_pilots as u8;
        self.encode(plsc, out);
    }
}

/// How to demap the PLS symbols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlscDemap {
    /// Coherent soft decisions — the best option once the carrier is locked,
    /// worth roughly 2 dB over hard decisions.
    CoherentSoft,
    /// Coherent hard decisions.
    CoherentHard,
    /// Differential hard decisions — works with an unknown or drifting carrier
    /// phase, at a cost in sensitivity. Used during acquisition.
    Differential,
}

/// Decodes 64 noisy pi/2-BPSK symbols back into a PLS code.
///
/// An S2 header's PLS code continues the SOF's pi/2-BPSK; an S2X one is
/// turned by +90°. The coherent modes read the symbols both ways — the plain
/// way against the 128 S2 codewords, turned back against the 128 S2X ones —
/// and keep the better match, so the turn itself counts towards telling the
/// two apart. The differential mode only sees the turn at the first symbol,
/// and reads that one bit both ways.
pub struct PlscDecoder {
    rm: ReedMuller,
    soft: Vec<f32>,
    rot: Vec<Iq>,
}

impl Default for PlscDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl PlscDecoder {
    pub fn new() -> Self {
        PlscDecoder {
            rm: ReedMuller::new(PLSC_SCRAMBLER),
            soft: vec![0.0; PLSC_LEN],
            rot: vec![Iq::new(0.0, 0.0); PLSC_LEN],
        }
    }

    /// Restrict decoding to a known set of PLS codes.
    ///
    /// Worth doing on a carrier whose MODCOD set is known: it cannot then
    /// mis-decode into a MODCOD that cannot occur.
    pub fn with_expected(expected: Vec<u8>) -> Self {
        PlscDecoder {
            rm: ReedMuller::with_enabled(PLSC_SCRAMBLER, expected),
            soft: vec![0.0; PLSC_LEN],
            rot: vec![Iq::new(0.0, 0.0); PLSC_LEN],
        }
    }

    /// Decode a PLHEADER.
    ///
    /// `symbols` must start at **the last SOF symbol**, followed by the 64 PLS
    /// symbols, so `symbols.len() >= 65`. The coherent modes skip that first
    /// symbol; the differential mode needs it to seed the chain. Requiring it
    /// in every mode keeps the caller's slicing identical whichever is used.
    ///
    /// # Panics
    /// If `symbols` holds fewer than 65 symbols.
    pub fn decode(&mut self, symbols: &[Iq], how: PlscDemap) -> PlsInfo {
        assert!(
            symbols.len() > PLSC_LEN,
            "need the last SOF symbol plus 64 PLS symbols"
        );
        let s2 = |d: u8| d < 128;
        let s2x = |d: u8| d >= 128;
        let plsc = match how {
            PlscDemap::CoherentSoft | PlscDemap::CoherentHard => {
                derotate_bpsk_iq(&symbols[1..], &mut self.rot, PLSC_LEN);
                let hard = |f: fn(&Iq) -> f32, rot: &[Iq]| {
                    rot.iter()
                        .enumerate()
                        .fold(0u64, |c, (j, y)| c | ((f(y) < 0.0) as u64) << (63 - j))
                };
                let (a, b) = if how == PlscDemap::CoherentSoft {
                    // Plain: the real parts; turned: the imaginary parts.
                    for (s, y) in self.soft.iter_mut().zip(&self.rot) {
                        *s = y.re;
                    }
                    let a = self.rm.best_soft(&self.soft, s2);
                    for (s, y) in self.soft.iter_mut().zip(&self.rot) {
                        *s = y.im;
                    }
                    let b = self.rm.best_soft(&self.soft, s2x);
                    (a.map(|(d, m)| (d, -m)), b.map(|(d, m)| (d, -m)))
                } else {
                    let a = self.rm.best_hard(hard(|y| y.re, &self.rot), s2);
                    let b = self.rm.best_hard(hard(|y| y.im, &self.rot), s2x);
                    (a.map(|(d, m)| (d, m as f32)), b.map(|(d, m)| (d, m as f32)))
                };
                pick(a, b)
            }
            PlscDemap::Differential => {
                let code = demap_bpsk_diff(symbols, PLSC_LEN);
                // The first bit is read across the SOF/PLS boundary, where
                // S2X turns by 90°: read it the other way for S2X. Every
                // later bit is relative to it, so a different first bit
                // flips them all.
                let d = symbols[1].conj() * symbols[0];
                let first_s2 = d.im < 0.0;
                let first_s2x = d.re < 0.0;
                let code_x = if first_s2 == first_s2x { code } else { !code };
                let a = self.rm.best_hard(code, s2);
                let b = self.rm.best_hard(code_x, s2x);
                pick(a.map(|(d, m)| (d, m as f32)), b.map(|(d, m)| (d, m as f32)))
            }
        };
        PlsInfo::parse(plsc)
    }
}

/// The better of the S2 and S2X candidates (lower cost; S2 on a tie).
fn pick(a: Option<(u8, f32)>, b: Option<(u8, f32)>) -> u8 {
    match (a, b) {
        (Some(a), Some(b)) if b.1 < a.1 => b.0,
        (Some(a), _) => a.0,
        (None, Some(b)) => b.0,
        (None, None) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::defs::{MAX_PLFRAME_LEN, PLHEADER_LEN, SOF_BIG_ENDIAN, SOF_LEN};

    /// Build a full 90-symbol PLHEADER for the given fields.
    fn plheader(modcod: u8, short_fecframe: bool, has_pilots: bool) -> Vec<Iq> {
        let mut out = vec![Iq::new(0.0, 0.0); PLHEADER_LEN];
        map_bpsk(SOF_BIG_ENDIAN, &mut out[..SOF_LEN], SOF_LEN);
        PlscEncoder::new().encode_fields(modcod, short_fecframe, has_pilots, &mut out[SOF_LEN..]);
        out
    }

    #[test]
    fn qpsk_normal_frame_geometry() {
        // MODCOD 4 = QPSK 1/2, normal FECFRAME, no pilots.
        let p = PlsInfo::from_fields(4, false, false);
        assert_eq!(p.modcod, 4);
        assert!(!p.short_fecframe && !p.has_pilots && !p.dummy_frame);
        assert_eq!(p.n_mod, 2);
        assert_eq!(p.n_slots, 360);
        assert_eq!(p.n_pilots, 0);
        assert_eq!(p.xfecframe_len, 32_400); // 64800 bits / 2 bits per symbol
        assert_eq!(p.plframe_len, 32_400 + 90);
        assert_eq!(p.payload_len, 32_400);
    }

    #[test]
    fn slots_match_the_fecframe_length_for_every_modcod() {
        // S * 90 * n_mod must equal the FECFRAME length, which is the identity
        // the slot counts come from.
        for modcod in 1..=28u8 {
            for short in [false, true] {
                let p = PlsInfo::from_fields(modcod, short, false);
                let bits = p.xfecframe_len * p.n_mod as u32;
                let expected = if short { 16_200 } else { 64_800 };
                assert_eq!(bits, expected, "MODCOD {modcod}, short={short}");
            }
        }
    }

    #[test]
    fn pilot_blocks_follow_the_16_slot_rule() {
        // Normal QPSK: 360 slots -> (360-1)/16 = 22 pilot blocks.
        let p = PlsInfo::from_fields(4, false, true);
        assert_eq!(p.n_pilots, 22);
        assert_eq!(p.plframe_len, 90 + 360 * 90 + 22 * 36);
        assert_eq!(p.plframe_len as usize, MAX_PLFRAME_LEN);

        // Short 32APSK: 144/4 = 36 slots -> (36-1)/16 = 2 pilot blocks.
        let p = PlsInfo::from_fields(24, true, true);
        assert_eq!(p.n_slots, 36);
        assert_eq!(p.n_pilots, 2);
    }

    #[test]
    fn longest_frame_is_normal_qpsk_with_pilots() {
        // Nothing may exceed the buffer size the acquisition code allocates.
        let worst = (0..=255u8)
            .map(|c| PlsInfo::parse(c).plframe_len)
            .max()
            .unwrap();
        assert_eq!(worst as usize, MAX_PLFRAME_LEN);
    }

    #[test]
    fn dummy_frame_has_no_payload_and_no_pilots() {
        // MODCOD 0 is a dummy frame; the pilots bit must be ignored.
        let p = PlsInfo::from_fields(0, false, true);
        assert!(p.dummy_frame);
        assert!(!p.has_pilots);
        assert_eq!(p.n_pilots, 0);
        assert_eq!(p.n_mod, 0);
        assert_eq!(p.n_slots, 36);
        assert_eq!(p.plframe_len, 37 * 90);
    }

    #[test]
    fn modcod_ranges_give_the_right_constellation() {
        let bits = |mc: u8| PlsInfo::from_fields(mc, false, false).n_mod;
        assert_eq!(bits(1), 2); // QPSK 1/4
        assert_eq!(bits(11), 2); // QPSK 9/10
        assert_eq!(bits(12), 3); // 8PSK 3/5
        assert_eq!(bits(17), 3); // 8PSK 9/10
        assert_eq!(bits(18), 4); // 16APSK 2/3
        assert_eq!(bits(23), 4); // 16APSK 9/10
        assert_eq!(bits(24), 5); // 32APSK 3/4
        assert_eq!(bits(28), 5); // 32APSK 9/10
    }

    /// A PLHEADER for any 8-bit code.
    fn plheader_code(plsc: u8) -> Vec<Iq> {
        let mut out = vec![Iq::new(0.0, 0.0); PLHEADER_LEN];
        map_bpsk(SOF_BIG_ENDIAN, &mut out[..SOF_LEN], SOF_LEN);
        PlscEncoder::new().encode(plsc, &mut out[SOF_LEN..]);
        out
    }

    #[test]
    fn every_8_bit_code_round_trips_in_all_three_modes() {
        let mut dec = PlscDecoder::new();
        for plsc in 0..=255u8 {
            let header = plheader_code(plsc);
            for how in [
                PlscDemap::CoherentSoft,
                PlscDemap::CoherentHard,
                PlscDemap::Differential,
            ] {
                let got = dec.decode(&header[SOF_LEN - 1..], how);
                assert_eq!(got.plsc, plsc, "{how:?}");
            }
        }
    }

    #[test]
    fn s2x_pls_symbols_are_turned_90_degrees() {
        // Same 7 low bits, b0 set: the symbols are the S2 ones times j,
        // except for the extra generator row's bits.
        let a = plheader_code(0x84);
        let b = plheader_code(0x04);
        assert_eq!(a[..SOF_LEN], b[..SOF_LEN]);
        let turned = PlscEncoder::new();
        let mut c = vec![Iq::new(0.0, 0.0); PLSC_LEN];
        turned.encode(0x84, &mut c);
        let rm = ReedMuller::new(PLSC_SCRAMBLER);
        let mut plain = vec![Iq::new(0.0, 0.0); PLSC_LEN];
        map_bpsk(rm.encode(0x84), &mut plain, PLSC_LEN);
        for (x, y) in c.iter().zip(&plain) {
            assert!((x - y * Iq::new(0.0, 1.0)).norm() < 1e-6);
        }
    }

    #[test]
    fn s2x_geometry() {
        // QPSK 13/45 normal: like S2 QPSK.
        let p = PlsInfo::s2x(132, true);
        assert_eq!((p.n_mod, p.n_slots, p.n_pilots), (2, 360, 22));
        assert_eq!(p.plframe_len as usize, MAX_PLFRAME_LEN);
        assert_eq!(p.modcod().unwrap().to_string(), "QPSK 13/45");
        // 128APSK: 103 slots (Table 16), the last 12 symbols padding.
        let p = PlsInfo::s2x(200, false);
        assert_eq!((p.n_mod, p.n_slots), (7, 103));
        // 256APSK: 90 slots.
        assert_eq!(PlsInfo::s2x(214, false).n_slots, 90);
        // Short 32APSK 2/3 (4+12+16rb): 36 slots.
        let p = PlsInfo::s2x(246, true);
        assert_eq!((p.frame, p.n_slots, p.n_pilots), (FecFrame::Short, 36, 2));
        // VL-SNR: the lengths of Figures 17 and 18, no MODCOD from the PLS.
        let p = PlsInfo::parse(129);
        assert_eq!((p.vlsnr, p.plframe_len), (Some(1), 33_282));
        assert!(p.modcod().is_none());
        assert_eq!(PlsInfo::parse(131).plframe_len, 16_686);
        // Table 17b: every reserved code's length.
        for (code, len) in [
            (128u8, 21_690u32),
            (130, 16_290),
            (176, 13_050),
            (177, 13_338),
            (188, 10_890),
            (189, 11_142),
            (192, 10_890),
            (193, 11_142),
            (196, 10_890),
            (197, 11_142),
            (250, 22_194),
            (251, 16_686),
            (252, 13_338),
            (253, 11_142),
            (254, 8_370),
            (255, 6_714),
        ] {
            let p = PlsInfo::parse(code);
            assert!(p.reserved && p.modcod().is_none(), "{code}");
            assert_eq!(p.plframe_len, len, "code {code}");
        }
        // Every other S2X code is a Table 17a MODCOD whose bits fill the
        // slots it is given.
        for code in 128..=255u8 {
            let p = PlsInfo::parse(code);
            if let Some(m) = p.modcod() {
                let bits = p.n_slots as usize * SLOT_LEN * p.n_mod as usize;
                assert!(
                    bits >= m.frame.n_ldpc() && bits - m.frame.n_ldpc() < SLOT_LEN * 8,
                    "{m}: {bits} bits for {}",
                    m.frame.n_ldpc()
                );
                assert_eq!(p.has_pilots, code & 1 == 1);
            } else {
                assert!(p.reserved || p.vlsnr.is_some(), "code {code}");
            }
        }
    }

    #[test]
    fn round_trip_every_plsc_in_all_three_demap_modes() {
        let mut dec = PlscDecoder::new();
        for modcod in 0..32u8 {
            for short in [false, true] {
                for pilots in [false, true] {
                    let header = plheader(modcod, short, pilots);
                    // The decoder wants the last SOF symbol first.
                    let from_last_sof = &header[SOF_LEN - 1..];
                    let want = PlsInfo::from_fields(modcod, short, pilots);

                    for how in [
                        PlscDemap::CoherentSoft,
                        PlscDemap::CoherentHard,
                        PlscDemap::Differential,
                    ] {
                        let got = dec.decode(from_last_sof, how);
                        assert_eq!(
                            got, want,
                            "MODCOD {modcod}, short={short}, pilots={pilots}, {how:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn differential_decode_survives_a_phase_offset() {
        let mut dec = PlscDecoder::new();
        let header = plheader(14, false, true); // 8PSK 3/4 with pilots
        let want = PlsInfo::from_fields(14, false, true);

        for turns in [0.13f32, 0.25, 0.5, 0.77] {
            let th = std::f32::consts::TAU * turns;
            let rot = Iq::new(th.cos(), th.sin());
            let rotated: Vec<Iq> = header[SOF_LEN - 1..].iter().map(|s| s * rot).collect();
            assert_eq!(
                dec.decode(&rotated, PlscDemap::Differential),
                want,
                "{turns} turns"
            );
        }
    }

    #[test]
    fn soft_decode_survives_heavy_noise() {
        // Minimum distance 32 should shrug off noise that visibly wrecks the
        // individual symbols. Deterministic pseudo-noise keeps the test stable.
        let mut dec = PlscDecoder::new();
        let header = plheader(4, false, false);
        let want = PlsInfo::from_fields(4, false, false);

        let mut state = 0x1234_5678_9ABC_DEF0u64;
        let mut next = || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let x = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
            // Roughly uniform in [-1, 1).
            ((x >> 11) as f32 / (1u64 << 52) as f32) - 1.0
        };

        let noisy: Vec<Iq> = header[SOF_LEN - 1..]
            .iter()
            .map(|s| s + Iq::new(next() * 0.8, next() * 0.8))
            .collect();
        assert_eq!(dec.decode(&noisy, PlscDemap::CoherentSoft), want);
    }

    #[test]
    fn restricting_expected_codes_avoids_impossible_modcods() {
        // A receiver told to expect only QPSK 1/2 and 3/4 (no pilots) must
        // never report anything else, even from pure noise.
        let a = PlsInfo::from_fields(4, false, false).plsc;
        let b = PlsInfo::from_fields(7, false, false).plsc;
        let mut dec = PlscDecoder::with_expected(vec![a, b]);
        let noise = vec![Iq::new(0.01, -0.02); PLSC_LEN + 1];
        let got = dec.decode(&noise, PlscDemap::CoherentSoft);
        assert!(got.plsc == a || got.plsc == b, "got {:#x}", got.plsc);
    }
}
