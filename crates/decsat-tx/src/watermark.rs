// made by CasualArclamp
//! A plain-text watermark in the transmitted TS: "DecSAT vX.Y.Z" in clear
//! ASCII, so a receiver's text scan ("Text in the transport stream") shows
//! which program and version made a stream.
//!
//! The text goes in its own packets on a private PID that the PMT does not
//! list, so a set-top box ignores them (ISO/IEC 13818-1 §2.4.3.2: a decoder
//! drops packets of a PID it was not told about). Each one takes the place
//! of a null packet (PID 0x1FFF), so the multiplex rate does not change.

use decsat_mod::datv::TS_LEN;

/// The watermark's PID: a user PID (0x0010–0x1FFE, ISO/IEC 13818-1
/// Table 2-3) well away from ffmpeg's 0x100 upwards and the PMT.
pub const PID_WATERMARK: u16 = 0x1FF0;

/// A null packet becomes the watermark when this many packets have gone by
/// since the last one: at 430 kbit/s of TS (286 packets a second) about one
/// a second.
pub const EVERY: u64 = 256;

/// With no null packet for this many packets (an encoder that fills the
/// rate), the watermark goes in as an extra packet: 0.1 % more, inside the
/// 0.2 % that [`crate::encode::mux_rate`] keeps free under the channel
/// rate. About one every 3.6 s at 430 kbit/s.
pub const FORCE: u64 = 1024;

/// The watermark text.
pub fn text() -> String {
    format!("DecSAT v{} transmitter", env!("CARGO_PKG_VERSION"))
}

/// Puts the watermark in place of a null packet once every [`EVERY`]
/// packets, or adds it when no null comes for [`FORCE`] packets.
pub struct Watermark {
    packet: [u8; TS_LEN],
    /// Packets since the last watermark.
    since: u64,
    /// The continuity counter (4 bits), ISO/IEC 13818-1 §2.4.3.3.
    cc: u8,
}

impl Default for Watermark {
    fn default() -> Self {
        Self::new()
    }
}

impl Watermark {
    pub fn new() -> Self {
        let mut p = [0xFFu8; TS_LEN];
        // Header (§2.4.3.2): sync, no error, no unit start, PID, not
        // scrambled, payload only (adaptation_field_control 01).
        p[0] = 0x47;
        p[1] = (PID_WATERMARK >> 8) as u8 & 0x1F;
        p[2] = PID_WATERMARK as u8;
        p[3] = 0x10;
        // The text twice, with a space between, so a scan that starts
        // reading in the first copy still gets a whole one. 0xFF pads the
        // rest (not printable, so the text ends there).
        let t = text();
        let body = format!("{t} {t}");
        let n = body.len().min(TS_LEN - 4);
        p[4..4 + n].copy_from_slice(&body.as_bytes()[..n]);
        Self {
            packet: p,
            since: EVERY, // the first null takes one at once
            cc: 0,
        }
    }

    /// Changes at most one null packet in `batch` into the watermark when
    /// [`EVERY`] packets have gone by, or puts the watermark in after the
    /// packet that makes [`FORCE`]. Returns whether it did either.
    pub fn apply(&mut self, batch: &mut Vec<[u8; TS_LEN]>) -> bool {
        let mut add_at = None;
        let mut done = false;
        for (i, p) in batch.iter_mut().enumerate() {
            self.since += 1;
            if done {
                continue;
            }
            let is_null = p[1] & 0x1F == 0x1F && p[2] == 0xFF;
            if is_null && self.since >= EVERY {
                *p = self.next();
                done = true;
            } else if self.since >= FORCE {
                add_at = Some(i + 1);
                done = true;
            }
        }
        if let Some(i) = add_at {
            let m = self.next();
            batch.insert(i, m);
        }
        done
    }

    /// The next watermark packet, its continuity counter set.
    fn next(&mut self) -> [u8; TS_LEN] {
        let mut p = self.packet;
        p[3] = 0x10 | self.cc;
        self.cc = (self.cc + 1) & 0x0F;
        self.since = 0;
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn null() -> [u8; TS_LEN] {
        let mut p = [0xFFu8; TS_LEN];
        p[0] = 0x47;
        p[1] = 0x1F;
        p[2] = 0xFF;
        p[3] = 0x10;
        p
    }

    #[test]
    fn a_null_becomes_the_text_once_in_a_while() {
        let mut w = Watermark::new();
        let mut marks = Vec::new();
        for _ in 0..4 {
            let mut batch = vec![null(); 300];
            assert!(w.apply(&mut batch));
            let m: Vec<_> = batch
                .iter()
                .filter(|p| u16::from_be_bytes([p[1] & 0x1F, p[2]]) == PID_WATERMARK)
                .copied()
                .collect();
            assert_eq!(m.len(), 1);
            marks.push(m[0]);
        }
        // The continuity counter counts up.
        let cc: Vec<u8> = marks.iter().map(|p| p[3] & 0x0F).collect();
        assert_eq!(cc, [0, 1, 2, 3]);
        let payload = String::from_utf8_lossy(&marks[0][4..]);
        assert!(payload.starts_with(&text()));
        assert!(text().contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn packets_that_are_not_null_stay() {
        let mut w = Watermark::new();
        let mut p = null();
        p[1] = 0x01; // PID 0x100
        p[2] = 0x00;
        let mut batch = vec![p; 300];
        assert!(!w.apply(&mut batch));
        assert!(batch.iter().all(|q| *q == p));
    }

    #[test]
    fn with_no_nulls_the_text_is_added() {
        let mut w = Watermark::new();
        let mut p = null();
        p[1] = 0x01;
        p[2] = 0x00;
        let mut added = 0;
        for _ in 0..10 {
            let mut batch = vec![p; 512];
            if w.apply(&mut batch) {
                added += 1;
            }
            let marks = batch
                .iter()
                .filter(|q| u16::from_be_bytes([q[1] & 0x1F, q[2]]) == PID_WATERMARK)
                .count();
            assert_eq!(batch.len(), 512 + marks);
        }
        // 5120 packets: the first at 768 (the counter starts at EVERY),
        // then one every 1024.
        assert_eq!(added, 5);
    }
}
