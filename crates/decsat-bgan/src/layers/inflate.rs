// made by CasualArclamp
//! A small inflater that behaves like Python's `zlib.decompressobj` fed 512
//! bytes at a time, which is what `bgan/findings.py::_inflate` does.
//!
//! Why not a library: the Python keeps the output of every chunk that decoded
//! and *discards the whole failing chunk*, and zlib is strict about corrupt
//! deflate data (e.g. "invalid distance too far back"). The miniz backend of
//! `flate2` is not: it happily copies zeros from before the start of the
//! stream, turning a damaged body into plausible garbage. Real payloads contain
//! such bodies (a Bearer Control PDU header spliced into the middle), so the
//! strictness is part of the observable behaviour and is reproduced here.
//!
//! The decoder is a port of the structure of zlib's contributed `puff.c`
//! (Mark Adler, zlib licence) with zlib's `inflate_table` acceptance rules:
//! an over-subscribed code is always an error, an incomplete code is allowed
//! only for a literal/length or distance table whose longest code has length 1,
//! the code-length code must be complete, the block must have an end-of-block
//! code, and a match may not reach back before the start of the output.
//!
//! The whole input is decoded in one pass. Bit positions are tracked so that,
//! when something fails, the output can be cut back to what a chunked feed
//! would have returned: everything up to the 512-byte chunk in which the error
//! was detectable. The chunk is attributed from the bit position at which the
//! decoder noticed the problem; for invalid Huffman codes zlib may notice a
//! byte earlier or later, so an error within a byte or two of a 512-byte
//! boundary can land in the neighbouring chunk.

use flate2::Crc;

/// Container around the deflate data.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wrap {
    /// RFC 1952, `wbits = 16 + MAX_WBITS`.
    Gzip,
    /// RFC 1950, `wbits = MAX_WBITS`.
    Zlib,
    /// Bare deflate, `wbits = -MAX_WBITS`.
    Raw,
}

/// Python feeds the decompressor this many input bytes at a time.
const CHUNK: usize = 512;
const CHUNK_BITS: usize = CHUNK * 8;
/// Output cap (a 1 MiB body can inflate to a gigabyte).
const MAX_OUT: usize = 64 << 20;

enum Stop {
    /// Input ended mid-symbol: not an error, the output so far stands.
    NeedMore,
    /// Corrupt data, noticed with this many input bits consumed.
    Bad(usize),
}

struct Huff {
    count: [u16; 16],
    symbol: Vec<u16>,
}

impl Huff {
    /// Canonical code from code lengths, with zlib's acceptance rules.
    /// `is_codes` selects the (stricter) code-length-code rule.
    fn build(lens: &[u8], is_codes: bool) -> Option<Huff> {
        let mut count = [0u16; 16];
        for &l in lens {
            count[l as usize] += 1;
        }
        let max = (1..16).rev().find(|&l| count[l] > 0).unwrap_or(0);
        if max == 0 {
            // no symbols at all: legal, every lookup then fails
            return Some(Huff {
                count,
                symbol: Vec::new(),
            });
        }
        let mut left: i32 = 1;
        for &c in &count[1..] {
            left = (left << 1) - c as i32;
            if left < 0 {
                return None; // over-subscribed
            }
        }
        if left > 0 && (is_codes || max != 1) {
            return None; // incomplete
        }
        let mut offs = [0u16; 16];
        for l in 1..15 {
            offs[l + 1] = offs[l] + count[l];
        }
        let mut symbol = vec![0u16; lens.len()];
        for (s, &l) in lens.iter().enumerate() {
            if l != 0 {
                symbol[offs[l as usize] as usize] = s as u16;
                offs[l as usize] += 1;
            }
        }
        Some(Huff { count, symbol })
    }
}

struct Inf<'a> {
    d: &'a [u8],
    /// Bit position from the start of `d` (the start of the wrapper header).
    pos: usize,
    out: Vec<u8>,
    /// `snaps[c]` = output length after every step ending within chunks 0..=c.
    snaps: Vec<usize>,
    /// Bit position of the next unsnapped chunk boundary.
    bound: usize,
}

impl Inf<'_> {
    fn bits(&mut self, n: usize) -> Result<u32, Stop> {
        if self.pos + n > self.d.len() * 8 {
            return Err(Stop::NeedMore);
        }
        let mut v = 0u32;
        for k in 0..n {
            let p = self.pos + k;
            v |= (((self.d[p >> 3] >> (p & 7)) & 1) as u32) << k;
        }
        self.pos += n;
        Ok(v)
    }

    /// Record that a step ending at bit `end` started with `before` output bytes.
    fn mark(&mut self, end: usize, before: usize) {
        while end > self.bound {
            self.snaps.push(before);
            self.bound += CHUNK_BITS;
        }
    }

    /// Output length that a chunked feed would have returned if chunk `c`
    /// (0-based) is the one that failed.
    fn keep(&self, c: usize) -> usize {
        if c == 0 {
            0
        } else {
            self.snaps.get(c - 1).copied().unwrap_or(self.out.len())
        }
    }

    fn decode(&mut self, h: &Huff) -> Result<u16, Stop> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= self.bits(1)? as i32;
            let count = h.count[len] as i32;
            if code - count < first {
                return Ok(h.symbol[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(Stop::Bad(self.pos)) // ran out of codes: invalid code
    }

    fn codes(&mut self, lit: &Huff, dist: &Huff) -> Result<(), Stop> {
        const LBASE: [u16; 29] = [
            3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99,
            115, 131, 163, 195, 227, 258,
        ];
        const LEXT: [u8; 29] = [
            0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
        ];
        const DBASE: [u16; 30] = [
            1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025,
            1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
        ];
        const DEXT: [u8; 30] = [
            0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12,
            12, 13, 13,
        ];
        loop {
            let before = self.out.len();
            let start = self.pos;
            let step = (|| -> Result<bool, Stop> {
                let sym = self.decode(lit)? as usize;
                if sym < 256 {
                    self.out.push(sym as u8);
                } else if sym == 256 {
                    return Ok(true);
                } else {
                    let s = sym - 257;
                    if s >= 29 {
                        return Err(Stop::Bad(self.pos));
                    }
                    let len = LBASE[s] as usize + self.bits(LEXT[s] as usize)? as usize;
                    let ds = self.decode(dist)? as usize;
                    if ds >= 30 {
                        return Err(Stop::Bad(self.pos));
                    }
                    let d = DBASE[ds] as usize + self.bits(DEXT[ds] as usize)? as usize;
                    if d > self.out.len() {
                        return Err(Stop::Bad(self.pos)); // invalid distance too far back
                    }
                    let from = self.out.len() - d;
                    for k in 0..len {
                        let b = self.out[from + k];
                        self.out.push(b);
                    }
                }
                Ok(false)
            })();
            match step {
                Err(e) => {
                    self.pos = if matches!(e, Stop::NeedMore) {
                        start
                    } else {
                        self.pos
                    };
                    return Err(e);
                }
                Ok(true) => {
                    let end = self.pos;
                    self.mark(end, before);
                    return Ok(());
                }
                Ok(false) => {
                    let end = self.pos;
                    self.mark(end, before);
                    if self.out.len() > MAX_OUT {
                        return Err(Stop::NeedMore);
                    }
                }
            }
        }
    }

    fn stored(&mut self) -> Result<(), Stop> {
        self.pos = self.pos.div_ceil(8) * 8;
        let len = self.bits(16)? as usize;
        let nlen = self.bits(16)? as usize;
        if len != (!nlen & 0xFFFF) {
            return Err(Stop::Bad(self.pos));
        }
        let mut left = len;
        while left > 0 {
            let at = self.pos / 8;
            if at >= self.d.len() {
                return Err(Stop::NeedMore);
            }
            let before = self.out.len();
            self.out.push(self.d[at]);
            self.pos += 8;
            let end = self.pos;
            self.mark(end, before);
            left -= 1;
        }
        Ok(())
    }

    fn dynamic(&mut self) -> Result<(), Stop> {
        const ORDER: [usize; 19] = [
            16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
        ];
        let nlen = self.bits(5)? as usize + 257;
        let ndist = self.bits(5)? as usize + 1;
        let ncode = self.bits(4)? as usize + 4;
        if nlen > 286 || ndist > 30 {
            return Err(Stop::Bad(self.pos));
        }
        let mut lens = [0u8; 320];
        for &o in ORDER.iter().take(ncode) {
            lens[o] = self.bits(3)? as u8;
        }
        let Some(lencode) = Huff::build(&lens[..19], true) else {
            return Err(Stop::Bad(self.pos));
        };
        let mut lens = [0u8; 320];
        let mut i = 0;
        while i < nlen + ndist {
            let sym = self.decode(&lencode)?;
            if sym < 16 {
                lens[i] = sym as u8;
                i += 1;
            } else {
                let (prev, rep) = match sym {
                    16 => {
                        if i == 0 {
                            return Err(Stop::Bad(self.pos));
                        }
                        (lens[i - 1], 3 + self.bits(2)? as usize)
                    }
                    17 => (0, 3 + self.bits(3)? as usize),
                    _ => (0, 11 + self.bits(7)? as usize),
                };
                if i + rep > nlen + ndist {
                    return Err(Stop::Bad(self.pos));
                }
                for _ in 0..rep {
                    lens[i] = prev;
                    i += 1;
                }
            }
        }
        if lens[256] == 0 {
            return Err(Stop::Bad(self.pos)); // missing end-of-block
        }
        let Some(lit) = Huff::build(&lens[..nlen], false) else {
            return Err(Stop::Bad(self.pos));
        };
        let Some(dist) = Huff::build(&lens[nlen..nlen + ndist], false) else {
            return Err(Stop::Bad(self.pos));
        };
        self.codes(&lit, &dist)
    }

    fn fixed(&mut self) -> Result<(), Stop> {
        let mut l = [0u8; 288];
        for (s, v) in l.iter_mut().enumerate() {
            *v = match s {
                0..=143 => 8,
                144..=255 => 9,
                256..=279 => 7,
                _ => 8,
            };
        }
        let lit = Huff::build(&l, false).unwrap();
        let dist = Huff::build(&[5u8; 32], false).unwrap();
        self.codes(&lit, &dist)
    }

    /// Decode deflate blocks to the final block. `Ok(())` means the stream ended.
    fn blocks(&mut self) -> Result<(), Stop> {
        loop {
            let last = self.bits(1)?;
            match self.bits(2)? {
                0 => self.stored()?,
                1 => self.fixed()?,
                2 => self.dynamic()?,
                _ => return Err(Stop::Bad(self.pos)),
            }
            if last == 1 {
                return Ok(());
            }
        }
    }
}

/// gzip member header length, as zlib reads it. `Ok(None)`: need more bytes.
fn gzip_header_len(h: &[u8]) -> Result<Option<usize>, ()> {
    if h.first().is_some_and(|&c| c != 0x1f)
        || h.get(1).is_some_and(|&c| c != 0x8b)
        || h.get(2).is_some_and(|&c| c != 8)
    {
        return Err(());
    }
    if h.len() < 10 {
        return Ok(None);
    }
    let flg = h[3];
    if flg & 0xe0 != 0 {
        return Err(());
    }
    let mut p = 10;
    if flg & 4 != 0 {
        if h.len() < p + 2 {
            return Ok(None);
        }
        p += 2 + (h[p] as usize | (h[p + 1] as usize) << 8);
    }
    for bit in [8u8, 16] {
        if flg & bit != 0 {
            match h.get(p..).and_then(|r| r.iter().position(|&c| c == 0)) {
                Some(z) => p += z + 1,
                None => return Ok(None),
            }
        }
    }
    if flg & 2 != 0 {
        p += 2; // header CRC, not verified
    }
    if h.len() < p { Ok(None) } else { Ok(Some(p)) }
}

fn adler32(d: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in d {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// Decompress as much as possible, tolerating a truncated tail, with the
/// failure semantics described in the module docs. Empty if nothing decodes.
pub fn inflate(data: &[u8], wrap: Wrap) -> Vec<u8> {
    let start = match wrap {
        Wrap::Raw => 0,
        Wrap::Zlib => {
            if data.len() < 2 {
                return Vec::new();
            }
            let (cmf, flg) = (data[0] as u32, data[1] as u32);
            if (cmf << 8 | flg) % 31 != 0 || cmf & 0xf != 8 || cmf >> 4 > 7 || flg & 0x20 != 0 {
                return Vec::new();
            }
            2
        }
        Wrap::Gzip => match gzip_header_len(data) {
            Ok(Some(n)) => n,
            _ => return Vec::new(),
        },
    };
    let mut s = Inf {
        d: data,
        pos: start * 8,
        out: Vec::new(),
        snaps: Vec::new(),
        bound: CHUNK_BITS,
    };
    match s.blocks() {
        Err(Stop::NeedMore) => s.out,
        Err(Stop::Bad(e)) => {
            let c = (e.max(1) - 1) / CHUNK_BITS;
            let k = s.keep(c);
            s.out.truncate(k);
            s.out
        }
        Ok(()) => {
            let end = s.pos.div_ceil(8);
            let fail_at = |nbytes: usize| (end + nbytes - 1) / CHUNK;
            let bad = match wrap {
                Wrap::Raw => None,
                Wrap::Gzip => {
                    let t = data.get(end..).unwrap_or(&[]);
                    let mut crc = Crc::new();
                    crc.update(&s.out);
                    if t.len() >= 4 && u32::from_le_bytes(t[0..4].try_into().unwrap()) != crc.sum()
                    {
                        Some(fail_at(4))
                    } else if t.len() >= 8
                        && u32::from_le_bytes(t[4..8].try_into().unwrap()) != s.out.len() as u32
                    {
                        Some(fail_at(8))
                    } else {
                        None
                    }
                }
                Wrap::Zlib => {
                    let t = data.get(end..).unwrap_or(&[]);
                    if t.len() >= 4
                        && u32::from_be_bytes(t[0..4].try_into().unwrap()) != adler32(&s.out)
                    {
                        Some(fail_at(4))
                    } else {
                        None
                    }
                }
            };
            if let Some(c) = bad {
                let k = s.keep(c);
                s.out.truncate(k);
            }
            s.out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::DeflateEncoder, write::GzEncoder, write::ZlibEncoder};
    use std::io::Write;

    fn text(n: usize) -> Vec<u8> {
        let mut x = 7u32;
        (0..n)
            .map(|i| {
                x = x.wrapping_mul(1103515245).wrapping_add(12345);
                if i % 3 == 0 {
                    b"the quick brown fox "[(x >> 16) as usize % 20]
                } else {
                    b'a' + ((x >> 20) % 5) as u8
                }
            })
            .collect()
    }

    #[test]
    fn roundtrips_all_wrappers_and_levels() {
        let t = text(20_000);
        for level in [0, 1, 6, 9] {
            let c = Compression::new(level);
            let mut g = GzEncoder::new(Vec::new(), c);
            g.write_all(&t).unwrap();
            assert_eq!(inflate(&g.finish().unwrap(), Wrap::Gzip), t, "gzip {level}");
            let mut z = ZlibEncoder::new(Vec::new(), c);
            z.write_all(&t).unwrap();
            assert_eq!(inflate(&z.finish().unwrap(), Wrap::Zlib), t, "zlib {level}");
            let mut r = DeflateEncoder::new(Vec::new(), c);
            r.write_all(&t).unwrap();
            assert_eq!(inflate(&r.finish().unwrap(), Wrap::Raw), t, "raw {level}");
        }
    }

    #[test]
    fn truncated_input_gives_prefix() {
        let t = text(30_000);
        let mut g = GzEncoder::new(Vec::new(), Compression::default());
        g.write_all(&t).unwrap();
        let c = g.finish().unwrap();
        let out = inflate(&c[..c.len() / 2], Wrap::Gzip);
        assert!(!out.is_empty() && out.len() < t.len());
        assert_eq!(out, t[..out.len()]);
    }

    #[test]
    fn bad_trailer_loses_last_chunk() {
        let t = text(5000);
        let mut g = GzEncoder::new(Vec::new(), Compression::default());
        g.write_all(&t).unwrap();
        let mut c = g.finish().unwrap();
        let n = c.len();
        // short body (< 512 bytes) with a bad CRC: the single chunk is lost
        let small = text(300);
        let mut gs = GzEncoder::new(Vec::new(), Compression::default());
        gs.write_all(&small).unwrap();
        let mut cs = gs.finish().unwrap();
        let m = cs.len();
        assert!(m < 512);
        cs[m - 8] ^= 1;
        assert!(inflate(&cs, Wrap::Gzip).is_empty());
        // long body: only the final chunk goes, earlier ones stay
        c[n - 8] ^= 1;
        let out = inflate(&c, Wrap::Gzip);
        assert!(out.len() < t.len());
        assert_eq!(out, t[..out.len()]);
    }

    #[test]
    fn rejects_distance_before_start() {
        // fixed block: literal 'a', then match length 3 distance 5 (> 1 byte of output)
        // bits: BFINAL=1, BTYPE=01, 'a'=0x61 (8-bit code 0x91), len3 (code 257=0000001 7-bit), dist code 4 (00100) + 1 extra bit
        let mut bits: Vec<u8> = vec![1, 1, 0];
        let push_msb = |v: &mut Vec<u8>, code: u32, n: u32| {
            for i in (0..n).rev() {
                v.push(((code >> i) & 1) as u8);
            }
        };
        push_msb(&mut bits, 0x30 + 0x61, 8);
        push_msb(&mut bits, 1, 7); // symbol 257
        push_msb(&mut bits, 4, 5); // dist symbol 4 -> base 5, 1 extra bit
        bits.push(0);
        push_msb(&mut bits, 0, 7); // end of block
        let mut bytes = vec![0u8; bits.len().div_ceil(8)];
        for (i, b) in bits.iter().enumerate() {
            bytes[i / 8] |= b << (i % 8);
        }
        assert!(inflate(&bytes, Wrap::Raw).is_empty());
        // and a legal match (distance 1) decodes
        let mut ok = vec![1u8, 1, 0];
        push_msb(&mut ok, 0x30 + 0x61, 8);
        push_msb(&mut ok, 1, 7);
        push_msb(&mut ok, 0, 5); // dist symbol 0 -> distance 1
        push_msb(&mut ok, 0, 7);
        let mut bytes = vec![0u8; ok.len().div_ceil(8)];
        for (i, b) in ok.iter().enumerate() {
            bytes[i / 8] |= b << (i % 8);
        }
        assert_eq!(inflate(&bytes, Wrap::Raw), b"aaaa");
    }
}
