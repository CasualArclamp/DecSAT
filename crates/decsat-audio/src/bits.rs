// made by CasualArclamp
//! MSB-first bit reading and writing, for the AAC configuration structures
//! (AudioSpecificConfig, LATM's StreamMuxConfig, RFC 3640 AU headers), none
//! of which keep to byte boundaries.

/// Reads bits MSB first. Every read returns `None` past the end, so parsers
/// can use `?` and treat a short buffer as "not decodable".
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Position in bits.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    /// The next `n` bits (at most 32) as an unsigned number.
    pub fn read(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n <= 32);
        if self.pos + n as usize > self.data.len() * 8 {
            return None;
        }
        let mut v = 0u64;
        for _ in 0..n {
            let byte = self.data[self.pos / 8];
            v = (v << 1) | ((byte >> (7 - self.pos % 8)) & 1) as u64;
            self.pos += 1;
        }
        Some(v as u32)
    }

    pub fn bit(&mut self) -> Option<bool> {
        self.read(1).map(|b| b == 1)
    }

    pub fn skip(&mut self, n: usize) -> Option<()> {
        if self.pos + n > self.data.len() * 8 {
            return None;
        }
        self.pos += n;
        Some(())
    }

    /// `n` whole bytes from the current (possibly unaligned) position.
    pub fn bytes(&mut self, n: usize) -> Option<Vec<u8>> {
        if self.pos.is_multiple_of(8) {
            let at = self.pos / 8;
            let b = self.data.get(at..at + n)?.to_vec();
            self.pos += n * 8;
            return Some(b);
        }
        (0..n).map(|_| self.read(8).map(|b| b as u8)).collect()
    }

    pub fn align(&mut self) {
        self.pos = self.pos.div_ceil(8) * 8;
    }

    /// Bits read so far.
    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }
}

/// Writes bits MSB first (to build configurations and test frames).
#[derive(Default)]
pub struct BitWriter {
    pub bytes: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The low `n` bits of `v`, MSB first.
    pub fn put(&mut self, n: u32, v: u32) {
        for i in (0..n).rev() {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let bit = ((v >> i) & 1) as u8;
            *self.bytes.last_mut().unwrap() |= bit << (7 - self.bits % 8);
            self.bits += 1;
        }
    }

    pub fn len_bits(&self) -> usize {
        self.bits
    }

    /// Pad to a byte boundary and hand the bytes over.
    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_across_byte_boundaries() {
        let mut w = BitWriter::new();
        w.put(3, 0b101);
        w.put(13, 0x1ABC);
        w.put(1, 1);
        w.put(32, 0xDEAD_BEEF);
        let n = w.len_bits();
        let b = w.finish();
        let mut r = BitReader::new(&b);
        assert_eq!(r.read(3), Some(0b101));
        assert_eq!(r.read(13), Some(0x1ABC));
        assert_eq!(r.bit(), Some(true));
        assert_eq!(r.read(32), Some(0xDEAD_BEEF));
        assert_eq!(r.position(), n);
        assert!(r.read(8).is_none());
    }

    #[test]
    fn unaligned_bytes() {
        let mut r = BitReader::new(&[0x0F, 0xF0, 0xAB]);
        r.skip(4).unwrap();
        assert_eq!(r.bytes(1), Some(vec![0xFF]));
        r.align();
        assert_eq!(r.bytes(1), Some(vec![0xAB]));
    }
}
