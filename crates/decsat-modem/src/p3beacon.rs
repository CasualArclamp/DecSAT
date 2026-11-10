// made by CasualArclamp
//! AMSAT's 400 bit/s PSK beacon frames: AO-40's uncoded format, which
//! QO-100's (Es'hail 2) P4-A narrow-band beacons send too.
//!
//! On the air: BPSK, the data differentially encoded and then Manchester
//! encoded. A frame is a 32-bit sync word and 514 bytes, 10.36 s at
//! 400 bit/s: 512 bytes of block, the last 2 a CRC, unscrambled and with no
//! FEC. The first byte names the block. QO-100's blocks are plain ASCII,
//! 64 characters to the line (eight lines); its housekeeping values, when
//! it sends them, are text in those lines, parsed by [`parse_telemetry`].
//!
//! Format from Daniel Estévez EA4GPZ, "Decoding AO-40 uncoded telemetry"
//! (destevez.net, 2017) and "Decoding the QO-100 beacon with gr-satellites"
//! (2019); the sync word and the CRC variant are as gr-satellites (GPL-3.0)
//! has them, and were checked against a frame received from QO-100 (see the
//! tests). AO-40's binary A block carries calibrated channels whose table
//! is not applied here: such a block shows as hex.

/// The sync word, sent most significant bit first.
pub const SYNC_WORD: u32 = 0x3915_ED30;
/// Bytes after the sync word: the block and its CRC.
pub const FRAME_BYTES: usize = 514;
/// Bytes of block, before the CRC.
pub const BLOCK_BYTES: usize = 512;
/// A block's text lines.
pub const LINE_CHARS: usize = 64;
/// Bits QO-100 sends between one uncoded frame and the next sync word (as
/// received in 2026): by its shape AO-40's FEC block with the same text.
/// It is not decoded here.
pub const FEC_BLOCK_BITS: u64 = 6016;

/// Sync bits allowed wrong when hunting for a frame.
const HUNT_ERRORS: u32 = 2;
/// Sync bits allowed wrong right where the last frame says one must be.
const FLYWHEEL_ERRORS: u32 = 8;
/// A sync word this good inside a frame ends it (ASCII text cannot hold one).
const RESYNC_ERRORS: u32 = 1;
/// A cut-off frame shorter than this (bytes) is not worth showing.
const MIN_PARTIAL: usize = 8;

/// CRC-16/CCITT-FALSE: polynomial 0x1021, initial value 0xFFFF, no
/// reflection, no final XOR. Over a whole frame (block and CRC) it is 0.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// One frame off the air, whole or cut short.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct P3Frame {
    /// The block and CRC (fewer than [`FRAME_BYTES`] when cut short).
    pub bytes: Vec<u8>,
    /// All [`FRAME_BYTES`] arrived.
    pub complete: bool,
    /// Whole, and the CRC checks.
    pub crc_ok: bool,
    /// Sync bits that were wrong.
    pub sync_errors: u32,
    /// Bits received before this frame's first (after its sync word).
    pub bit_index: u64,
}

impl P3Frame {
    /// The block's name, its first byte, when that is a letter.
    pub fn block(&self) -> Option<char> {
        self.bytes
            .first()
            .map(|&b| b as char)
            .filter(char::is_ascii_alphabetic)
    }

    /// True when the block reads as text (so a line view means something).
    pub fn is_text(&self) -> bool {
        let n = self.bytes.len().min(BLOCK_BYTES);
        if n == 0 {
            return false;
        }
        let good = self.bytes[..n]
            .iter()
            .filter(|&&b| (0x20..0x7F).contains(&b))
            .count();
        good * 10 >= n * 8
    }

    /// The block's text, one string per 64-character line, without trailing
    /// spaces. Bytes that are not printable ASCII show as `·`.
    pub fn lines(&self) -> Vec<String> {
        let n = self.bytes.len().min(BLOCK_BYTES);
        self.bytes[..n]
            .chunks(LINE_CHARS)
            .map(|l| {
                l.iter()
                    .map(|&b| {
                        if (0x20..0x7F).contains(&b) {
                            b as char
                        } else {
                            '·'
                        }
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// The block's text as one string (no line breaks).
    pub fn text(&self) -> String {
        let n = self.bytes.len().min(BLOCK_BYTES);
        self.bytes[..n]
            .iter()
            .map(|&b| {
                if (0x20..0x7F).contains(&b) {
                    b as char
                } else {
                    ' '
                }
            })
            .collect()
    }
}

/// Finds frames in the differentially decoded bits.
#[derive(Debug, Clone, Default)]
pub struct Deframer {
    shift: u32,
    /// Bits seen so far.
    bits: u64,
    /// The frame being gathered, and the bits of its current byte.
    frame: Option<P3Frame>,
    byte: u8,
    byte_bits: u8,
    /// Where the next sync word must end, after a whole frame.
    expect: Option<u64>,
    /// The bit count when the last whole frame ended.
    ended: Option<u64>,
}

impl Deframer {
    pub fn new() -> Self {
        Self::default()
    }

    /// True while inside a frame.
    pub fn in_frame(&self) -> bool {
        self.frame.is_some()
    }

    /// Bytes of the frame being gathered.
    pub fn frame_bytes(&self) -> usize {
        self.frame.as_ref().map_or(0, |f| f.bytes.len())
    }

    /// The bytes of the frame being gathered, so far.
    pub fn partial(&self) -> &[u8] {
        self.frame.as_ref().map_or(&[], |f| &f.bytes)
    }

    /// Bits since the last whole frame ended, while what follows it (the
    /// FEC block) is going by; `None` in a frame, or once that is over.
    pub fn bits_after_frame(&self) -> Option<u64> {
        if self.frame.is_some() {
            return None;
        }
        self.ended
            .map(|e| self.bits - e)
            .filter(|&n| n <= FEC_BLOCK_BITS)
    }

    /// Take one bit (0 or 1); any frame it ends goes on `out`.
    pub fn push(&mut self, bit: u8, out: &mut Vec<P3Frame>) {
        self.shift = (self.shift << 1) | u32::from(bit & 1);
        self.bits += 1;
        let errors = (self.shift ^ SYNC_WORD).count_ones();
        let wanted = self.bits >= 32;

        if self.frame.is_some() {
            if wanted && errors <= RESYNC_ERRORS {
                // A new frame began inside this one: the sync was false, or
                // bits were lost. Keep what there was.
                self.flush(out);
                self.start(errors);
                return;
            }
            self.gather(bit & 1, out);
            return;
        }

        let hit = match self.expect {
            Some(at) if self.bits == at => errors <= FLYWHEEL_ERRORS,
            _ => errors <= HUNT_ERRORS,
        };
        if wanted && hit {
            self.start(errors);
        } else if self.expect.is_some_and(|at| self.bits >= at) {
            self.expect = None;
        }
    }

    /// The signal has gone: emit a frame cut short, and hunt afresh.
    pub fn flush(&mut self, out: &mut Vec<P3Frame>) {
        if let Some(f) = self.frame.take()
            && f.bytes.len() >= MIN_PARTIAL
        {
            out.push(f);
        }
        self.byte = 0;
        self.byte_bits = 0;
        self.expect = None;
        self.ended = None;
    }

    fn start(&mut self, sync_errors: u32) {
        self.frame = Some(P3Frame {
            bytes: Vec::with_capacity(FRAME_BYTES),
            complete: false,
            crc_ok: false,
            sync_errors,
            bit_index: self.bits,
        });
        self.byte = 0;
        self.byte_bits = 0;
        self.expect = None;
    }

    fn gather(&mut self, bit: u8, out: &mut Vec<P3Frame>) {
        self.byte = (self.byte << 1) | bit;
        self.byte_bits += 1;
        if self.byte_bits < 8 {
            return;
        }
        let byte = self.byte;
        self.byte = 0;
        self.byte_bits = 0;
        let Some(f) = self.frame.as_mut() else {
            return;
        };
        f.bytes.push(byte);
        if f.bytes.len() == FRAME_BYTES {
            f.complete = true;
            f.crc_ok = crc16(&f.bytes) == 0;
            let done = self.frame.take().expect("just used");
            out.push(done);
            // The next sync word, if the frames follow without a gap.
            self.expect = Some(self.bits + 32);
            self.ended = Some(self.bits);
        }
    }
}

/// What a block's housekeeping lines say. QO-100 has sent these as text:
/// `UPT: 3d 0h 29m CMD: 91 LEI_REQ: 0 LEI_ACT: 0`,
/// `TEMP: 56 C VOLTAGES: 1.0 1.8 ... Volts`, `TFL: 0 TFE: 0 ...`. Which rail
/// each voltage is, and what the counters mean, AMSAT has not published.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Telemetry {
    /// `UPT`, in seconds.
    pub uptime_s: Option<u64>,
    /// `CMD`.
    pub commands: Option<u32>,
    /// `LEI_REQ`, `LEI_ACT`: LEILA's (the uplink-power watchdog's) requests
    /// and activations.
    pub leila_requests: Option<u32>,
    pub leila_active: Option<u32>,
    /// `TEMP`, °C.
    pub temperature_c: Option<f32>,
    /// `VOLTAGES`, volts, in the order sent.
    pub voltages: Vec<f32>,
    /// Every other `NAME: number` field (`TFL`, `TFE`, `TFH`, `HFF`, `HTH`,
    /// `HR`), in the order sent.
    pub counters: Vec<(String, i64)>,
}

impl Telemetry {
    /// Uptime as the beacon writes it.
    pub fn uptime_text(&self) -> Option<String> {
        self.uptime_s.map(|s| {
            format!(
                "{}d {}h {:02}m",
                s / 86_400,
                s % 86_400 / 3600,
                s % 3600 / 60
            )
        })
    }

    fn is_empty(&self) -> bool {
        self.uptime_s.is_none()
            && self.commands.is_none()
            && self.temperature_c.is_none()
            && self.voltages.is_empty()
            && self.counters.is_empty()
    }
}

/// The word's value if it is `<number><suffix>` (as `3d`).
fn with_suffix(word: &str, suffix: char) -> Option<u64> {
    word.strip_suffix(suffix)?.parse().ok()
}

/// A word that ends in `:` names a field.
fn is_field(word: &str) -> bool {
    word.len() >= 2
        && word.ends_with(':')
        && word[..word.len() - 1]
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Read the housekeeping fields out of a block's text; `None` when it has
/// none.
pub fn parse_telemetry(text: &str) -> Option<Telemetry> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut t = Telemetry::default();
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        if !is_field(w) {
            i += 1;
            continue;
        }
        let name = &w[..w.len() - 1];
        let mut j = i + 1;
        match name {
            "UPT" => {
                let mut seconds = 0u64;
                let mut any = false;
                while let Some(v) = words.get(j) {
                    let part = with_suffix(v, 'd')
                        .map(|n| n * 86_400)
                        .or_else(|| with_suffix(v, 'h').map(|n| n * 3600))
                        .or_else(|| with_suffix(v, 'm').map(|n| n * 60))
                        .or_else(|| with_suffix(v, 's'));
                    let Some(part) = part else { break };
                    seconds += part;
                    any = true;
                    j += 1;
                }
                if any {
                    t.uptime_s = Some(seconds);
                }
            }
            "VOLTAGES" => {
                while let Some(v) = words.get(j).and_then(|v| v.parse::<f32>().ok()) {
                    t.voltages.push(v);
                    j += 1;
                }
            }
            "TEMP" => {
                t.temperature_c = words.get(j).and_then(|v| v.parse().ok());
            }
            _ => {
                let number = words.get(j).and_then(|v| v.parse::<i64>().ok());
                let count = number.and_then(|n| u32::try_from(n).ok());
                match name {
                    "CMD" => t.commands = count,
                    "LEI_REQ" => t.leila_requests = count,
                    "LEI_ACT" => t.leila_active = count,
                    _ => {
                        if let Some(n) = number {
                            t.counters.push((name.to_string(), n));
                        }
                    }
                }
            }
        }
        i = j.max(i + 1);
    }
    (!t.is_empty()).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The block QO-100 sent at 22:26:45 UTC on 2026-11-09, its damaged
    /// first lines put right by reading them.
    fn received_block() -> Vec<u8> {
        [
            "M HI de Qatar-OSCAR 100 (DK0SB)",
            "2020-02-14: Celebrating 1 year of QO-100",
            "New bandplan with extended transponder range active now. New",
            "beacons (and new LEILA ground station) currently experimental.",
            "",
            "New beacon frequencies:",
            "CW now F1A on 10489500        Middle Beacon AMSAT PSK 10489750",
            "Experimental Beacon 10490000  currently also F1A CW",
        ]
        .iter()
        .flat_map(|l| format!("{l:<64}").into_bytes())
        .collect()
    }

    fn feed(d: &mut Deframer, bytes: &[u8], out: &mut Vec<P3Frame>) {
        for &b in bytes {
            for i in (0..8).rev() {
                d.push((b >> i) & 1, out);
            }
        }
    }

    #[test]
    fn crc_check_value() {
        // The catalogue's check value for CRC-16/CCITT-FALSE.
        assert_eq!(crc16(b"123456789"), 0x29B1);
    }

    #[test]
    fn crc_matches_what_the_satellite_sent() {
        let block = received_block();
        assert_eq!(block.len(), BLOCK_BYTES);
        // 0x5BB4 is the CRC received on the air after this block.
        assert_eq!(crc16(&block), 0x5BB4);
    }

    #[test]
    fn frames_found_and_checked() {
        let mut body = received_block();
        body.extend_from_slice(&[0x5B, 0xB4]);
        let mut stream = vec![0x55; 7];
        for _ in 0..2 {
            stream.extend_from_slice(&SYNC_WORD.to_be_bytes());
            stream.extend_from_slice(&body);
        }
        let mut d = Deframer::new();
        let mut out = Vec::new();
        feed(&mut d, &stream, &mut out);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|f| f.crc_ok && f.complete));
        assert_eq!(out[0].block(), Some('M'));
        assert!(out[0].is_text());
        assert_eq!(out[0].lines()[0], "M HI de Qatar-OSCAR 100 (DK0SB)");
        assert_eq!(out[0].lines().len(), 8);
    }

    #[test]
    fn a_damaged_frame_fails_its_crc() {
        let mut body = received_block();
        body.extend_from_slice(&[0x5B, 0xB4]);
        body[100] ^= 0x10;
        let mut stream = SYNC_WORD.to_be_bytes().to_vec();
        stream.extend_from_slice(&body);
        let mut d = Deframer::new();
        let mut out = Vec::new();
        feed(&mut d, &stream, &mut out);
        assert_eq!(out.len(), 1);
        assert!(out[0].complete && !out[0].crc_ok);
    }

    #[test]
    fn a_bad_sync_is_held_on_the_flywheel_only() {
        let mut body = received_block();
        body.extend_from_slice(&[0x5B, 0xB4]);
        // Four sync bits wrong: too many to hunt on, fine where one is due.
        let bad = (SYNC_WORD ^ 0x0101_0101).to_be_bytes();
        let mut good_then_bad = SYNC_WORD.to_be_bytes().to_vec();
        good_then_bad.extend_from_slice(&body);
        good_then_bad.extend_from_slice(&bad);
        good_then_bad.extend_from_slice(&body);
        let mut d = Deframer::new();
        let mut out = Vec::new();
        feed(&mut d, &good_then_bad, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].sync_errors, 4);

        let mut only_bad = bad.to_vec();
        only_bad.extend_from_slice(&body);
        let mut d = Deframer::new();
        let mut out = Vec::new();
        feed(&mut d, &only_bad, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn progress_is_reported_in_and_after_a_frame() {
        let mut body = received_block();
        body.extend_from_slice(&[0x5B, 0xB4]);
        let mut d = Deframer::new();
        let mut out = Vec::new();
        feed(&mut d, &SYNC_WORD.to_be_bytes(), &mut out);
        feed(&mut d, &body[..100], &mut out);
        assert_eq!(d.partial(), &body[..100]);
        assert_eq!(d.bits_after_frame(), None);
        feed(&mut d, &body[100..], &mut out);
        assert_eq!(out.len(), 1);
        assert!(d.partial().is_empty());
        feed(&mut d, &[0x50; 10], &mut out);
        assert_eq!(d.bits_after_frame(), Some(80));
        feed(&mut d, &[0x50; 800], &mut out);
        assert_eq!(d.bits_after_frame(), None);
    }

    #[test]
    fn a_cut_off_frame_is_kept() {
        let mut stream = SYNC_WORD.to_be_bytes().to_vec();
        stream.extend_from_slice(&received_block()[..200]);
        let mut d = Deframer::new();
        let mut out = Vec::new();
        feed(&mut d, &stream, &mut out);
        assert!(out.is_empty());
        d.flush(&mut out);
        assert_eq!(out.len(), 1);
        assert!(!out[0].complete && !out[0].crc_ok);
        assert_eq!(out[0].bytes.len(), 200);
    }

    #[test]
    fn telemetry_lines_parse() {
        // As EA4GPZ received them in 2019.
        let text = "K HI de QO-100 (DL50AMSAT BOCHUM \
                    UPT: 3d 0h 29m CMD: 91 LEI_REQ: 0 LEI_ACT: 0 \
                    TEMP: 56 C VOLTAGES: 1.0 1.8 1.0 1.0 1.8 1.5 1.3 0.0 0.5 Volts \
                    TFL: 0 TFE: 0 TFH: 0 HFF: 0 HTH: 0 HR: 0";
        let t = parse_telemetry(text).expect("telemetry");
        assert_eq!(t.uptime_s, Some(3 * 86_400 + 29 * 60));
        assert_eq!(t.uptime_text().as_deref(), Some("3d 0h 29m"));
        assert_eq!(t.commands, Some(91));
        assert_eq!(t.leila_requests, Some(0));
        assert_eq!(t.leila_active, Some(0));
        assert_eq!(t.temperature_c, Some(56.0));
        assert_eq!(t.voltages.len(), 9);
        assert_eq!(t.voltages[1], 1.8);
        assert_eq!(t.counters.len(), 6);
        assert_eq!(t.counters[0], ("TFL".to_string(), 0));
        assert_eq!(t.counters[5], ("HR".to_string(), 0));
    }

    #[test]
    fn text_without_telemetry_has_none() {
        assert_eq!(parse_telemetry("K HI de Qatar-OSCAR 100 (DK0SB)    "), None);
        assert_eq!(
            parse_telemetry("Experimental Beacon 10490000  currently also F1A CW"),
            None
        );
    }

    #[test]
    fn binary_blocks_are_not_text() {
        let f = P3Frame {
            bytes: (0..=255u8).cycle().take(512).collect(),
            ..Default::default()
        };
        assert!(!f.is_text());
    }
}
