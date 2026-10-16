// made by CasualArclamp
//! Cutting an audio elementary stream into whole frames.
//!
//! UDP and RTP packets need not hold whole frames — RFC 2250 lets an MPEG
//! audio frame span packets, and raw-UDP senders cut wherever their buffer
//! ends — so the payloads are joined into one byte stream and the frames are
//! found by their sync words and headers, as a decoder fed from a file would.
//!
//! Three framings, each self-delimiting:
//! - **MPEG audio** (ISO/IEC 11172-3 §2.4.2.3, 13818-3): 11-bit sync, and a
//!   length from the bit rate, sample rate and padding bit.
//! - **ADTS** (ISO/IEC 13818-7 §6.2): 12-bit sync and a 13-bit frame length.
//! - **LOAS** AudioSyncStream (ISO/IEC 14496-3 §1.7.2): 11-bit sync 0x2B7
//!   and a 13-bit length of the AudioMuxElement that follows.
//!
//! A sync word can turn up by chance inside audio data, so a frame is only
//! trusted once the next frame's header is where its length says (or, while
//! locked, if the previous one checked out the same way).

/// The framing a stream uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    Mpa,
    Adts,
    Loas,
}

/// An MPEG-1/2/2.5 audio frame header (ISO/IEC 11172-3 §2.4.1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MpaHeader {
    /// 1 for MPEG-1, 2 for MPEG-2, 25 for MPEG-2.5.
    pub version: u8,
    pub layer: u8,
    pub bitrate_kbps: u32,
    pub sample_rate: u32,
    pub channels: u8,
    /// Whole frame, header included, in bytes.
    pub frame_len: usize,
}

/// Bit rates in kbit/s by [version 1 / 2 and 2.5][layer − 1][index]
/// (11172-3 Table 3-B.2... and 13818-3 Table 2.4.2.3).
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

impl MpaHeader {
    /// Parse the four header bytes; `None` unless it is a usable header
    /// (free-format and reserved values are not).
    pub fn parse(b: &[u8]) -> Option<MpaHeader> {
        if b.len() < 4 || b[0] != 0xFF || b[1] & 0xE0 != 0xE0 {
            return None;
        }
        let version = match (b[1] >> 3) & 3 {
            0 => 25,
            2 => 2,
            3 => 1,
            _ => return None,
        };
        let layer = match (b[1] >> 1) & 3 {
            1 => 3,
            2 => 2,
            3 => 1,
            _ => return None,
        };
        let bi = (b[2] >> 4) as usize;
        let si = ((b[2] >> 2) & 3) as usize;
        if bi == 0 || bi == 15 || si == 3 {
            return None;
        }
        let bitrate_kbps = BITRATES[(version != 1) as usize][layer as usize - 1][bi];
        let base = [44_100, 48_000, 32_000][si];
        let sample_rate = match version {
            1 => base,
            2 => base / 2,
            _ => base / 4,
        };
        let pad = ((b[2] >> 1) & 1) as usize;
        let br = bitrate_kbps as usize * 1000;
        let sr = sample_rate as usize;
        let frame_len = match layer {
            1 => (12 * br / sr + pad) * 4,
            3 if version != 1 => 72 * br / sr + pad,
            _ => 144 * br / sr + pad,
        };
        Some(MpaHeader {
            version,
            layer,
            bitrate_kbps,
            sample_rate,
            channels: if b[3] >> 6 == 3 { 1 } else { 2 },
            frame_len,
        })
    }

    /// The fields that stay the same from frame to frame.
    fn same_stream(&self, o: &MpaHeader) -> bool {
        (self.version, self.layer, self.sample_rate) == (o.version, o.layer, o.sample_rate)
    }
}

/// An ADTS header (ISO/IEC 13818-7 §6.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdtsHeader {
    /// Audio object type (profile + 1): 2 for AAC-LC.
    pub object_type: u8,
    pub sample_rate_index: u8,
    pub channel_config: u8,
    /// Header length: 7, or 9 with a CRC.
    pub header_len: usize,
    pub frame_len: usize,
    /// Raw data blocks in the frame (usually 1).
    pub blocks: u8,
}

impl AdtsHeader {
    pub fn parse(b: &[u8]) -> Option<AdtsHeader> {
        if b.len() < 7 || b[0] != 0xFF || b[1] & 0xF6 != 0xF0 {
            return None;
        }
        let sample_rate_index = (b[2] >> 2) & 0x0F;
        if sample_rate_index > 12 {
            return None;
        }
        let frame_len =
            (((b[3] & 3) as usize) << 11) | ((b[4] as usize) << 3) | (b[5] >> 5) as usize;
        let header_len = if b[1] & 1 == 0 { 9 } else { 7 };
        if frame_len <= header_len {
            return None;
        }
        Some(AdtsHeader {
            object_type: (b[2] >> 6) + 1,
            sample_rate_index,
            channel_config: ((b[2] & 1) << 2) | (b[3] >> 6),
            header_len,
            frame_len,
            blocks: (b[6] & 3) + 1,
        })
    }

    fn same_stream(&self, o: &AdtsHeader) -> bool {
        (
            self.object_type,
            self.sample_rate_index,
            self.channel_config,
        ) == (o.object_type, o.sample_rate_index, o.channel_config)
    }
}

/// A LOAS AudioSyncStream frame's length, header (3 bytes) included.
fn loas_len(b: &[u8]) -> Option<usize> {
    if b.len() < 3 || b[0] != 0x56 || b[1] & 0xE0 != 0xE0 {
        return None;
    }
    let len = (((b[1] & 0x1F) as usize) << 8) | b[2] as usize;
    (len > 0).then_some(len + 3)
}

/// What a frame header says, framing-independently.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Head {
    Mpa(MpaHeader),
    Adts(AdtsHeader),
    Loas,
}

impl Head {
    fn len(&self, b: &[u8]) -> usize {
        match self {
            Head::Mpa(h) => h.frame_len,
            Head::Adts(h) => h.frame_len,
            Head::Loas => loas_len(b).unwrap_or(0),
        }
    }

    fn same_stream(&self, o: &Head) -> bool {
        match (self, o) {
            (Head::Mpa(a), Head::Mpa(b)) => a.same_stream(b),
            (Head::Adts(a), Head::Adts(b)) => a.same_stream(b),
            (Head::Loas, Head::Loas) => true,
            _ => false,
        }
    }
}

/// Most a stream may buffer without a frame being found before old bytes go.
const MAX_BUFFER: usize = 64 * 1024;

/// Joins payloads and hands back whole frames.
pub struct Framer {
    framing: Framing,
    buf: Vec<u8>,
    /// The last frame's header, while frames keep following each other.
    locked: Option<Head>,
    /// Bytes skipped looking for sync.
    pub skipped: u64,
}

impl Framer {
    pub fn new(framing: Framing) -> Self {
        Framer {
            framing,
            buf: Vec::new(),
            locked: None,
            skipped: 0,
        }
    }

    pub fn framing(&self) -> Framing {
        self.framing
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > MAX_BUFFER {
            let drop = self.buf.len() - MAX_BUFFER / 2;
            self.buf.drain(..drop);
            self.skipped += drop as u64;
            self.locked = None;
        }
    }

    /// Lost data: whatever is buffered no longer joins up with what comes
    /// next.
    pub fn reset(&mut self) {
        self.skipped += self.buf.len() as u64;
        self.buf.clear();
        self.locked = None;
    }

    fn head(&self, at: usize) -> Option<Head> {
        let b = &self.buf[at..];
        match self.framing {
            Framing::Mpa => MpaHeader::parse(b).map(Head::Mpa),
            Framing::Adts => AdtsHeader::parse(b).map(Head::Adts),
            Framing::Loas => loas_len(b).map(|_| Head::Loas),
        }
    }

    /// The next whole frame, if one is buffered.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        let mut at = 0;
        while at + 7 <= self.buf.len() {
            let Some(h) = self.head(at) else {
                at += 1;
                self.locked = None;
                continue;
            };
            let len = h.len(&self.buf[at..]);
            let end = at + len;
            // Trust it if the previous frame led straight here, or if the
            // next header is where this one says it ends.
            let trusted = self.locked.is_some_and(|l| at == 0 && l.same_stream(&h));
            if !trusted {
                if end + 7 > self.buf.len() {
                    break; // wait for the next header to arrive
                }
                match self.head(end) {
                    Some(n) if n.same_stream(&h) => {}
                    _ => {
                        at += 1;
                        continue;
                    }
                }
            }
            if end > self.buf.len() {
                break; // the rest of the frame is still to come
            }
            self.skipped += at as u64;
            let frame = self.buf[at..end].to_vec();
            self.buf.drain(..end);
            self.locked = Some(h);
            return Some(frame);
        }
        // Keep the tail that could still be the start of a frame.
        if at > 0 {
            self.skipped += at as u64;
            self.buf.drain(..at);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decsat_ip::mcast::silent_mp2_frame;

    #[test]
    fn mp2_header() {
        let h = MpaHeader::parse(&silent_mp2_frame()).unwrap();
        assert_eq!((h.version, h.layer), (1, 2));
        assert_eq!(
            (h.bitrate_kbps, h.sample_rate, h.channels),
            (128, 48_000, 2)
        );
        assert_eq!(h.frame_len, 384);
    }

    #[test]
    fn frames_come_out_whole_however_the_bytes_are_cut() {
        let f = silent_mp2_frame();
        let stream: Vec<u8> = std::iter::repeat_n(&f, 6).flatten().copied().collect();
        // Junk first, then the frames in awkward pieces.
        let mut fr = Framer::new(Framing::Mpa);
        fr.push(&[0xFF, 0x12, 0x34, 0xFF, 0xFB]);
        let mut got = Vec::new();
        for piece in stream.chunks(137) {
            fr.push(piece);
            while let Some(frame) = fr.next_frame() {
                got.push(frame);
            }
        }
        // The last frame waits for proof (the next header) or its successor.
        assert!(got.len() >= 5, "{} frames", got.len());
        assert!(got.iter().all(|g| *g == f));
        assert_eq!(fr.skipped, 5);
    }

    #[test]
    fn adts_and_loas_lengths() {
        // ADTS: AAC-LC, 48 kHz (index 3), stereo, 20-byte frame, no CRC.
        let len = 20usize;
        let mut a = vec![
            0xFF,
            0xF1,
            (1 << 6) | (3 << 2),
            (2 << 6) | ((len >> 11) as u8 & 3),
            (len >> 3) as u8,
            ((len & 7) as u8) << 5 | 0x1F,
            0xFC,
        ];
        a.resize(len, 0);
        let h = AdtsHeader::parse(&a).unwrap();
        assert_eq!(
            (h.object_type, h.sample_rate_index, h.channel_config),
            (2, 3, 2)
        );
        assert_eq!((h.header_len, h.frame_len, h.blocks), (7, 20, 1));
        let mut fr = Framer::new(Framing::Adts);
        for _ in 0..3 {
            fr.push(&a);
        }
        assert_eq!(fr.next_frame(), Some(a.clone()));

        let l = [0x56, 0xE0, 0x04, 1, 2, 3, 4];
        assert_eq!(loas_len(&l), Some(7));
    }
}
