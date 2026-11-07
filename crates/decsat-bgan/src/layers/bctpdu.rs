// made by CasualArclamp
//! Bearer Control PDU framing, found by its CRC (ETSI TS 102 744-3-1
//! clause 5.1.7).
//!
//! Port of `bgan/bctpdu.py` from DecBGAN (Python, same author,
//! GPL-3.0-or-later).
//!
//! Structure, clause 5.1.2: `FwdBCtPDUHeader | BCtSDU 1..n | bct-payload | CRC`
//! where bct-payload is a BCnPDU or ALComPDU, both `OCTET STRING (SIZE(0..255))`.
//!
//! The CRC (clause 5.1.7) is generator x^16 + x^12 + x^5 + 1, register
//! initialised to all ones, data clocked in LSB first, ones complement appended
//! low octet first. Clocking a whole PDU *including* its CRC leaves 0xF0B8
//! (standard HDLC/X.25 FCS).
//!
//! A CRC hit alone is not evidence (about one per 268 bytes by chance), so
//! detection also requires one of three known first header octets
//! (clause 5.1.4: `sdu-follows | length-present | addr-type(2) | comsig/ext |
//! type(3)`): 0xc9 (broadcast, carries BulletinBoard), 0x58 (connection PDU
//! with length octet) and 0x18 (connection PDU without).

use super::stream::Rolling;

/// CRC residue of a good PDU, clause 5.1.7.
pub const GOOD_RESIDUE: u16 = 0xF0B8;
/// 255-octet payload + header + CRC, with slack.
pub const MAX_PDU: usize = 264;
pub const MIN_PDU: usize = 20;
/// Header octets that make a CRC hit credible (see module docs).
pub const KNOWN_HEADERS: [u8; 3] = [0x18, 0x58, 0xc9];

/// Reflected CRC-16 table (polynomial 0x8408 = bit-reversed 0x1021).
/// A `const` block runs at compile time, so the table costs nothing at run time.
const TAB: [u16; 256] = {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u16;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0x8408
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

#[inline]
fn step(crc: u16, b: u8) -> u16 {
    (crc >> 8) ^ TAB[((crc ^ b as u16) & 0xFF) as usize]
}

/// Clock `data` through the BCtPDU CRC register, starting from `crc`
/// (use 0xFFFF for a fresh PDU). See [`GOOD_RESIDUE`].
pub fn crc_residue(data: &[u8], crc: u16) -> u16 {
    data.iter().fold(crc, |c, &b| step(c, b))
}

/// The two CRC octets for `body`, low octet first (clause 5.1.7).
pub fn crc_append(body: &[u8]) -> [u8; 2] {
    let c = crc_residue(body, 0xFFFF) ^ 0xFFFF;
    [(c & 0xFF) as u8, (c >> 8) as u8]
}

/// True when a PDU including its two CRC octets clocks to the residue.
pub fn crc_ok(pdu: &[u8]) -> bool {
    pdu.len() >= 3 && crc_residue(pdu, 0xFFFF) == GOOD_RESIDUE
}

/// Header octets before the payload, from the length-present bit (clause 5.1.4).
pub fn header_len(first_octet: u8) -> usize {
    if first_octet & 0x40 != 0 { 4 } else { 3 }
}

/// Try a PDU starting at `blob[0]`. `Ok(Some(len))` on a CRC hit, `Ok(None)` if
/// no length in `lo..=hi` matches, `Err(())` if the data ran out before `hi`
/// and more bytes could still change the answer.
fn try_pdu(blob: &[u8], lo: usize, hi: usize, fin: bool) -> Result<Option<usize>, ()> {
    let top = hi.min(blob.len());
    let mut crc = 0xFFFFu16;
    for (k, &b) in blob[..top].iter().enumerate() {
        crc = step(crc, b);
        // Python tries lengths lo..=min(hi, end-i) in increasing order and
        // takes the first; running the CRC incrementally gives the same answer
        // in O(hi) instead of O(hi^2).
        if k + 1 >= lo && crc == GOOD_RESIDUE {
            return Ok(Some(k + 1));
        }
    }
    if top < hi && !fin { Err(()) } else { Ok(None) }
}

/// Scan for CRC-valid PDUs whose header octet is in `headers`.
///
/// Returns `(offset, length)`. Greedy and non-overlapping: on a hit the scan
/// resumes at the end of that PDU. `stop` limits the scan like the Python
/// argument of the same name.
pub fn find_pdus(
    blob: &[u8],
    start: usize,
    stop: Option<usize>,
    headers: &[u8],
    lo: usize,
    hi: usize,
) -> Vec<(usize, usize)> {
    let end = stop.map_or(blob.len(), |s| s.min(blob.len()));
    let mut out = Vec::new();
    let mut i = start;
    while i < end {
        let mut hit = 0;
        if headers.contains(&blob[i])
            && let Ok(Some(l)) = try_pdu(&blob[i..end], lo, hi, true)
        {
            hit = l;
        }
        if hit > 0 {
            out.push((i, hit));
            i += hit;
        } else {
            i += 1;
        }
    }
    out
}

/// [`find_pdus`] with the Python defaults.
pub fn find_pdus_default(blob: &[u8]) -> Vec<(usize, usize)> {
    find_pdus(blob, 0, None, &KNOWN_HEADERS, MIN_PDU, MAX_PDU)
}

/// Payload bytes of each PDU, header and CRC removed.
///
/// `extra` accounts for the BCtSDU octets between the PDU header and the
/// payload proper (fitted, not derived; Python default is 2).
pub fn payloads(blob: &[u8], pdus: &[(usize, usize)], extra: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for &(off, ln) in pdus {
        let h = header_len(blob[off]) + extra;
        if ln > h + 2 {
            out.extend_from_slice(&blob[off + h..off + ln - 2]);
        }
    }
    out
}

/// Incremental [`find_pdus_default`]: feed bytes as they arrive, get new PDUs.
/// A candidate is decided as soon as its CRC matches, or once `MAX_PDU` bytes
/// follow it. Offsets are absolute stream offsets.
#[derive(Debug, Default)]
pub struct PduScanner {
    win: Rolling,
    cur: usize,
    fin: bool,
}

impl PduScanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<(usize, usize)> {
        if self.fin {
            return Vec::new();
        }
        self.win.push(bytes);
        self.run()
    }

    /// No more data: decide the remaining candidates.
    pub fn finish(&mut self) -> Vec<(usize, usize)> {
        self.fin = true;
        self.run()
    }

    fn run(&mut self) -> Vec<(usize, usize)> {
        let base = self.win.base();
        let buf = self.win.data();
        let mut out = Vec::new();
        let mut i = self.cur - base;
        while i < buf.len() {
            if KNOWN_HEADERS.contains(&buf[i]) {
                match try_pdu(&buf[i..], MIN_PDU, MAX_PDU, self.fin) {
                    Err(()) => break, // wait for more bytes
                    Ok(Some(l)) => {
                        out.push((base + i, l));
                        i += l;
                        continue;
                    }
                    Ok(None) => {}
                }
            }
            i += 1;
        }
        self.cur = base + i;
        let keep = self.cur;
        self.win.trim_before(keep);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdu(hdr: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![hdr, 0x11, 0x22];
        v.extend_from_slice(body);
        let c = crc_append(&v);
        v.extend_from_slice(&c);
        v
    }

    #[test]
    fn crc_selftest() {
        let b = [0x0f, 0xaa, 0x55];
        let mut p = b.to_vec();
        p.extend_from_slice(&crc_append(&b));
        assert!(crc_ok(&p));
        p[1] ^= 1;
        assert!(!crc_ok(&p));
    }

    #[test]
    fn crc_known_vector() {
        // CRC-16/X-25 of "123456789" is 0x906E.
        assert_eq!(crc_residue(b"123456789", 0xFFFF) ^ 0xFFFF, 0x906E);
    }

    #[test]
    fn finds_consecutive_pdus_and_streams() {
        let mut blob = vec![0u8; 5];
        let a = pdu(0xc9, &[1; 30]);
        let b = pdu(0x58, &[2; 40]);
        blob.extend_from_slice(&a);
        blob.extend_from_slice(&b);
        blob.extend_from_slice(&[0u8; 9]);
        let want = vec![(5, a.len()), (5 + a.len(), b.len())];
        assert_eq!(find_pdus_default(&blob), want);

        let mut s = PduScanner::new();
        let mut got = Vec::new();
        for ch in blob.chunks(7) {
            got.extend(s.push(ch));
        }
        got.extend(s.finish());
        assert_eq!(got, want);
    }

    #[test]
    fn wrong_header_rejected() {
        let blob = pdu(0x77, &[3; 30]);
        assert!(find_pdus_default(&blob).is_empty());
    }

    #[test]
    fn payload_extraction() {
        let a = pdu(0x18, &[9; 30]); // header_len 3, +2 extra
        let p = payloads(&a, &[(0, a.len())], 2);
        assert_eq!(p.len(), a.len() - 5 - 2);
    }
}
