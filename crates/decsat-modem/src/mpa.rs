// made by CasualArclamp
//! MPEG audio (ISO/IEC 11172-3, 13818-3) in a link's data: whole frames
//! found in a bit stream at any bit offset, as a studio link or programme
//! feed sends them straight over a modem (a 256 kbit/s Comtech TPC carrier
//! carries MPEG-1 Layer II at 48 kHz this way, 6144-bit frames).
//!
//! A frame starts with a 12-bit sync 0xFFF and a header (§2.4.2.3) whose
//! bit rate, sample rate and padding bit give its length. A sync can occur
//! by chance in audio data, so a frame is only trusted when the next one's
//! header is where its length says, of the same kind; once locked the
//! frames are followed by length, and three missed syncs in a row lose it.

/// Bit rates in kbit/s by [MPEG-1 / MPEG-2][layer − 1][index]
/// (11172-3 §2.4.2.3; 13818-3 Table 2.4.2.3 for the lower sample rates).
const BITRATES: [[[u32; 15]; 3]; 2] = [
    [
        [
            0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
        ],
        [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
        ],
        [
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ],
    ],
    [
        [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
        ],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    ],
];
/// Sample rates by [MPEG-1 / MPEG-2][index] (11172-3 §2.4.2.3).
const SAMPLE_RATES: [[u32; 3]; 2] = [[44_100, 48_000, 32_000], [22_050, 24_000, 16_000]];

/// A frame header, as far as it matters here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    /// 1 for MPEG-1, 2 for MPEG-2 (lower sample rates).
    pub version: u8,
    pub layer: u8,
    pub bitrate_kbps: u32,
    pub sample_rate: u32,
    /// 0 stereo, 1 joint stereo, 2 dual channel, 3 single channel.
    pub mode: u8,
    /// A CRC follows the header (the protection bit is 0).
    pub crc: bool,
    /// The whole frame in bytes, header included.
    pub len: usize,
}

impl Header {
    /// The header in the 32 bits `h` (first sent bit as the MSB), if it is
    /// one: sync, a known layer, a bit rate (not free format), a sample rate.
    pub fn parse(h: u32) -> Option<Header> {
        if h >> 20 != 0xFFF {
            return None;
        }
        let id = (h >> 19) & 1;
        let layer = match (h >> 17) & 3 {
            3 => 1,
            2 => 2,
            1 => 3,
            _ => return None,
        };
        let v = if id == 1 { 0 } else { 1 };
        let br = BITRATES[v][layer - 1][((h >> 12) & 15) as usize % 15];
        let sri = ((h >> 10) & 3) as usize;
        if br == 0 || (h >> 12) & 15 == 15 || sri == 3 {
            return None;
        }
        let sr = SAMPLE_RATES[v][sri];
        let pad = ((h >> 9) & 1) as usize;
        let len = match (layer, v) {
            (1, _) => (12 * br as usize * 1000 / sr as usize + pad) * 4,
            (3, 1) => 72 * br as usize * 1000 / sr as usize + pad,
            _ => 144 * br as usize * 1000 / sr as usize + pad,
        };
        Some(Header {
            version: if id == 1 { 1 } else { 2 },
            layer: layer as u8,
            bitrate_kbps: br,
            sample_rate: sr,
            mode: ((h >> 6) & 3) as u8,
            crc: (h >> 16) & 1 == 0,
            len,
        })
    }

    /// Two headers of one stream: the same version, layer, sample rate and
    /// channel count (the bit rate and padding may change frame to frame,
    /// and an encoder switches between stereo and joint stereo as it likes).
    fn same_stream(&self, o: &Header) -> bool {
        self.version == o.version
            && self.layer == o.layer
            && self.sample_rate == o.sample_rate
            && (self.mode == 3) == (o.mode == 3)
    }

    /// "MPEG-1 Layer II, 256 kbit/s, 48 kHz, joint stereo".
    pub fn describe(&self) -> String {
        format!(
            "MPEG-{} Layer {}, {} kbit/s, {} kHz, {}",
            self.version,
            ["I", "II", "III"][usize::from(self.layer.clamp(1, 3)) - 1],
            self.bitrate_kbps,
            f64::from(self.sample_rate) / 1000.0,
            ["stereo", "joint stereo", "dual channel", "mono"][usize::from(self.mode & 3)]
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MpaStats {
    pub locked: bool,
    pub frames: u64,
    /// Frames whose bits the frames cover (for the payload search's score).
    pub bits: u64,
    pub losses: u64,
    /// The latest frame's header.
    pub header: Option<Header>,
}

/// Missed syncs in a row that lose the frame alignment.
const LOSE: u32 = 3;

/// Finds MPEG audio frames in a bit stream; see the module notes.
#[derive(Default)]
pub struct MpaRx {
    bits: Vec<u8>,
    /// Locked: the bit in `bits` where the next frame starts.
    at: Option<usize>,
    misses: u32,
    pub stats: MpaStats,
}

fn word32(bits: &[u8], at: usize) -> u32 {
    bits[at..at + 32]
        .iter()
        .fold(0u32, |a, &b| (a << 1) | u32::from(b))
}

fn bytes(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b))
        .collect()
}

impl MpaRx {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bits (0/1) in; whole frames out (bytes, header first).
    pub fn push(&mut self, bits: &[u8], out: &mut Vec<Vec<u8>>) {
        self.bits.extend_from_slice(bits);
        loop {
            let Some(at) = self.at.or_else(|| self.find()) else {
                return;
            };
            if at + 32 > self.bits.len() {
                return;
            }
            let h = Header::parse(word32(&self.bits, at));
            let ok = h
                .zip(self.stats.header)
                .is_some_and(|(h, p)| h.same_stream(&p));
            let len = match (ok, h) {
                (true, Some(h)) => {
                    self.misses = 0;
                    self.stats.header = Some(h);
                    h.len
                }
                // A miss: step a frame of the last length and look again.
                _ => {
                    self.misses += 1;
                    if self.misses >= LOSE {
                        self.at = None;
                        self.stats.locked = false;
                        self.stats.losses += 1;
                        self.bits.drain(..at + 1);
                        continue;
                    }
                    self.stats.header.map_or(0, |p| p.len)
                }
            };
            let end = at + 8 * len;
            if end > self.bits.len() {
                self.at = Some(at);
                return;
            }
            if ok {
                out.push(bytes(&self.bits[at..end]));
                self.stats.frames += 1;
                self.stats.bits += 8 * len as u64;
            }
            self.bits.drain(..end);
            self.at = Some(0);
        }
    }

    /// Search for a header whose next header is where its length says.
    fn find(&mut self) -> Option<usize> {
        let n = self.bits.len();
        let mut p = 0;
        while p + 32 <= n {
            if let Some(h) = Header::parse(word32(&self.bits, p)) {
                let q = p + 8 * h.len;
                if q + 32 > n {
                    // Wait for the next header (keep from here).
                    self.bits.drain(..p);
                    return None;
                }
                if Header::parse(word32(&self.bits, q)).is_some_and(|h2| h2.same_stream(&h)) {
                    self.bits.drain(..p);
                    self.at = Some(0);
                    self.misses = 0;
                    self.stats.locked = true;
                    self.stats.header = Some(h);
                    return Some(0);
                }
            }
            p += 1;
        }
        // Keep the tail: a header may straddle the next bits.
        let keep = n.saturating_sub(31);
        self.bits.drain(..keep);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_carriers_header() {
        // Seen on the carrier: MPEG-1 Layer II, no CRC, 256 kbit/s, 48 kHz,
        // joint stereo — 768 bytes (6144 bits) a frame.
        let h = Header::parse(0xFFFD_C470).expect("header");
        assert_eq!(
            (h.version, h.layer, h.bitrate_kbps, h.sample_rate),
            (1, 2, 256, 48_000)
        );
        assert_eq!((h.mode, h.crc, h.len), (1, false, 768));
        assert_eq!(
            h.describe(),
            "MPEG-1 Layer II, 256 kbit/s, 48 kHz, joint stereo"
        );
        assert!(Header::parse(0x1234_5678).is_none());
    }

    #[test]
    fn finds_frames_at_any_bit_offset() {
        let mut s = 0x9E37_79B9u32;
        let mut rnd = || {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s >> 24) as u8
        };
        let mut bits: Vec<u8> = (0..1003).map(|_| rnd() & 1).collect();
        let mut sent = Vec::new();
        for _ in 0..20 {
            let mut f = vec![0xFF, 0xFD, 0xC4, 0x70];
            f.extend((0..764).map(|_| rnd()));
            bits.extend(
                f.iter()
                    .flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1)),
            );
            sent.push(f);
        }
        let mut rx = MpaRx::new();
        let mut out = Vec::new();
        for c in bits.chunks(777) {
            rx.push(c, &mut out);
        }
        assert!(rx.stats.locked);
        // All but the last (its successor never came, so it waits).
        assert!(out.len() >= 19, "{}", out.len());
        assert_eq!(out[0], sent[0]);
        assert_eq!(out[18], sent[18]);
    }
}
