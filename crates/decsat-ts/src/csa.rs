//! The DVB Common Scrambling Algorithm (CSA, version 1): the cipher BISS
//! scrambles a transport stream with.
//!
//! How it is applied is public (ETSI ETR 289; ISO/IEC 13818-1 §2.4.3.2 for
//! the scrambling control bits): only a packet's payload is scrambled, never
//! its header or adaptation field, and the two `transport_scrambling_control`
//! bits say which of two keys was used — `10` the even, `11` the odd. The
//! cipher itself was confidential until it was reverse-engineered in 2002.
//!
//! A payload of N bytes is taken as N/8 blocks and a residue of N%8 bytes:
//!
//! - **Block cipher** (64-bit block, 56 rounds of an 8-bit S-box and bit
//!   permutation, round keys from the 64-bit control word), chained
//!   backwards from the last block, CBC-like.
//! - **Stream cipher** (two 40-bit nibble shift registers, seven 5→2-bit
//!   S-boxes and a 4-bit adder), seeded by the control word and the first
//!   scrambled block, then XORed over every later block and the residue.
//!
//! A payload under 8 bytes is not scrambled at all.
//!
//! The tables and round structure are from VLC's
//! `modules/demux/mpeg/csa.c` (Laurent Aimar, GPL-2.0-or-later, after the
//! 2002 description) and libdvbcsa (GPL-2.0-or-later); this is a byte-wise
//! version, not their bit-sliced one, with the shift registers packed into
//! integers. Checked against VLC's own scrambler: see the tests.

/// The block cipher's S-box: a permutation of the bytes.
#[rustfmt::skip]
const BLOCK_SBOX: [u8; 256] = [
    0x3A, 0xEA, 0x68, 0xFE, 0x33, 0xE9, 0x88, 0x1A, 0x83, 0xCF, 0xE1, 0x7F, 0xBA, 0xE2, 0x38, 0x12,
    0xE8, 0x27, 0x61, 0x95, 0x0C, 0x36, 0xE5, 0x70, 0xA2, 0x06, 0x82, 0x7C, 0x17, 0xA3, 0x26, 0x49,
    0xBE, 0x7A, 0x6D, 0x47, 0xC1, 0x51, 0x8F, 0xF3, 0xCC, 0x5B, 0x67, 0xBD, 0xCD, 0x18, 0x08, 0xC9,
    0xFF, 0x69, 0xEF, 0x03, 0x4E, 0x48, 0x4A, 0x84, 0x3F, 0xB4, 0x10, 0x04, 0xDC, 0xF5, 0x5C, 0xC6,
    0x16, 0xAB, 0xAC, 0x4C, 0xF1, 0x6A, 0x2F, 0x3C, 0x3B, 0xD4, 0xD5, 0x94, 0xD0, 0xC4, 0x63, 0x62,
    0x71, 0xA1, 0xF9, 0x4F, 0x2E, 0xAA, 0xC5, 0x56, 0xE3, 0x39, 0x93, 0xCE, 0x65, 0x64, 0xE4, 0x58,
    0x6C, 0x19, 0x42, 0x79, 0xDD, 0xEE, 0x96, 0xF6, 0x8A, 0xEC, 0x1E, 0x85, 0x53, 0x45, 0xDE, 0xBB,
    0x7E, 0x0A, 0x9A, 0x13, 0x2A, 0x9D, 0xC2, 0x5E, 0x5A, 0x1F, 0x32, 0x35, 0x9C, 0xA8, 0x73, 0x30,
    0x29, 0x3D, 0xE7, 0x92, 0x87, 0x1B, 0x2B, 0x4B, 0xA5, 0x57, 0x97, 0x40, 0x15, 0xE6, 0xBC, 0x0E,
    0xEB, 0xC3, 0x34, 0x2D, 0xB8, 0x44, 0x25, 0xA4, 0x1C, 0xC7, 0x23, 0xED, 0x90, 0x6E, 0x50, 0x00,
    0x99, 0x9E, 0x4D, 0xD9, 0xDA, 0x8D, 0x6F, 0x5F, 0x3E, 0xD7, 0x21, 0x74, 0x86, 0xDF, 0x6B, 0x05,
    0x8E, 0x5D, 0x37, 0x11, 0xD2, 0x28, 0x75, 0xD6, 0xA7, 0x77, 0x24, 0xBF, 0xF0, 0xB0, 0x02, 0xB7,
    0xF8, 0xFC, 0x81, 0x09, 0xB1, 0x01, 0x76, 0x91, 0x7D, 0x0F, 0xC8, 0xA0, 0xF2, 0xCB, 0x78, 0x60,
    0xD1, 0xF7, 0xE0, 0xB5, 0x98, 0x22, 0xB3, 0x20, 0x1D, 0xA6, 0xDB, 0x7B, 0x59, 0x9F, 0xAE, 0x31,
    0xFB, 0xD3, 0xB6, 0xCA, 0x43, 0x72, 0x07, 0xF4, 0xD8, 0x41, 0x14, 0x55, 0x0D, 0x54, 0x8B, 0xB9,
    0xAD, 0x46, 0x0B, 0xAF, 0x80, 0x52, 0x2C, 0xFA, 0x8C, 0x89, 0x66, 0xFD, 0xB2, 0xA9, 0x9B, 0xC0,
];

/// The block cipher's bit permutation of an S-box output: bit 0 → 1,
/// 1 → 7, 2 → 5, 3 → 4, 4 → 2, 5 → 6, 6 → 0, 7 → 3.
const fn block_perm(x: u8) -> u8 {
    ((x & 0x40) >> 6)
        | ((x & 0x01) << 1)
        | ((x & 0x10) >> 2)
        | ((x & 0x80) >> 4)
        | ((x & 0x08) << 1)
        | ((x & 0x04) << 3)
        | ((x & 0x20) << 1)
        | ((x & 0x02) << 6)
}

/// [`block_perm`] for every byte, built at compile time.
///
/// Rust note: a `const` block runs in the compiler, so this is a plain
/// table in the binary; `while` because `for` is not allowed in const code.
const BLOCK_PERM: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = block_perm(i as u8);
        i += 1;
    }
    t
};

/// The key schedule's 64-bit permutation: bit n of a round key (numbered
/// from the most significant bit of its first byte) goes to bit
/// `KEY_PERM[n] - 1` of the next.
#[rustfmt::skip]
const KEY_PERM: [u8; 64] = [
    0x12, 0x24, 0x09, 0x07, 0x2A, 0x31, 0x1D, 0x15, 0x1C, 0x36, 0x3E, 0x32, 0x13, 0x21, 0x3B, 0x40,
    0x18, 0x14, 0x25, 0x27, 0x02, 0x35, 0x1B, 0x01, 0x22, 0x04, 0x0D, 0x0E, 0x39, 0x28, 0x1A, 0x29,
    0x33, 0x23, 0x34, 0x0C, 0x16, 0x30, 0x1E, 0x3A, 0x2D, 0x1F, 0x08, 0x19, 0x17, 0x2F, 0x3D, 0x11,
    0x3C, 0x05, 0x38, 0x2B, 0x0B, 0x06, 0x0A, 0x2C, 0x20, 0x3F, 0x2E, 0x0F, 0x03, 0x26, 0x10, 0x37,
];

/// The stream cipher's seven 5-bit → 2-bit S-boxes.
#[rustfmt::skip]
const STREAM_SBOX: [[u8; 32]; 7] = [
    [2, 0, 1, 1, 2, 3, 3, 0, 3, 2, 2, 0, 1, 1, 0, 3, 0, 3, 3, 0, 2, 2, 1, 1, 2, 2, 0, 3, 1, 1, 3, 0],
    [3, 1, 0, 2, 2, 3, 3, 0, 1, 3, 2, 1, 0, 0, 1, 2, 3, 1, 0, 3, 3, 2, 0, 2, 0, 0, 1, 2, 2, 1, 3, 1],
    [2, 0, 1, 2, 2, 3, 3, 1, 1, 1, 0, 3, 3, 0, 2, 0, 1, 3, 0, 1, 3, 0, 2, 2, 2, 0, 1, 2, 0, 3, 3, 1],
    [3, 1, 2, 3, 0, 2, 1, 2, 1, 2, 0, 1, 3, 0, 0, 3, 1, 0, 3, 1, 2, 3, 0, 3, 0, 3, 2, 0, 1, 2, 2, 1],
    [2, 0, 0, 1, 3, 2, 3, 2, 0, 1, 3, 3, 1, 0, 2, 1, 2, 3, 2, 0, 0, 3, 1, 1, 1, 0, 3, 2, 3, 1, 0, 2],
    [0, 1, 2, 3, 1, 2, 2, 0, 0, 1, 3, 0, 2, 3, 1, 3, 2, 3, 0, 2, 3, 0, 1, 1, 2, 1, 1, 2, 0, 3, 3, 0],
    [0, 3, 2, 2, 3, 0, 0, 1, 3, 0, 1, 3, 1, 2, 2, 1, 1, 0, 3, 3, 0, 1, 1, 2, 2, 3, 1, 0, 2, 3, 0, 2],
];

/// A control word made ready for use: the block cipher's 56 round keys and
/// the stream cipher's key (the control word itself).
#[derive(Clone, PartialEq, Eq)]
pub struct Key {
    /// Round keys, used last to first when descrambling.
    kk: [u8; 56],
    cw: [u8; 8],
}

impl std::fmt::Debug for Key {
    // A key is not printed by accident (a log, a panic message).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(…)")
    }
}

impl Key {
    /// Schedule control word `cw`: seven 64-bit words, each the last
    /// permuted by [`KEY_PERM`], the first the control word; round key
    /// 8i + j is byte j of word i (counting from the most permuted, word
    /// 6 − i steps from the control word) XOR i.
    pub fn new(cw: [u8; 8]) -> Self {
        let mut words = [0u64; 7];
        words[6] = u64::from_be_bytes(cw);
        for i in (0..6).rev() {
            words[i] = permute_key(words[i + 1]);
        }
        let mut kk = [0u8; 56];
        for (i, w) in words.iter().enumerate() {
            for (j, b) in w.to_be_bytes().iter().enumerate() {
                kk[i * 8 + j] = b ^ i as u8;
            }
        }
        Key { kk, cw }
    }

    /// The control word.
    pub fn cw(&self) -> [u8; 8] {
        self.cw
    }

    fn block_decrypt(&self, block: [u8; 8]) -> [u8; 8] {
        let mut w = block;
        for &k in self.kk.iter().rev() {
            let s = BLOCK_SBOX[(k ^ w[6]) as usize];
            let l = w[7] ^ s;
            w[7] = w[6];
            w[6] = w[5] ^ BLOCK_PERM[s as usize];
            w[5] = w[4];
            w[4] = w[3] ^ l;
            w[3] = w[2] ^ l;
            w[2] = w[1] ^ l;
            w[1] = w[0];
            w[0] = l;
        }
        w
    }

    fn block_encrypt(&self, block: [u8; 8]) -> [u8; 8] {
        let mut w = block;
        for &k in &self.kk {
            let l = w[0];
            let s = BLOCK_SBOX[(k ^ w[7]) as usize];
            w[0] = w[1];
            w[1] = w[2] ^ l;
            w[2] = w[3] ^ l;
            w[3] = w[4] ^ l;
            w[4] = w[5];
            w[5] = w[6] ^ BLOCK_PERM[s as usize];
            w[6] = w[7];
            w[7] = l ^ s;
        }
        w
    }

    /// Descramble a packet's payload in place.
    pub fn descramble(&self, payload: &mut [u8]) {
        let n = payload.len() / 8;
        if n == 0 {
            return;
        }
        let mut ib = block_at(payload, 0);
        let mut stream = Stream::new(&self.cw, &ib);
        for i in 1..=n {
            let clear = self.block_decrypt(ib);
            ib = if i < n {
                xor8(block_at(payload, i), stream.next_block())
            } else {
                [0; 8]
            };
            payload[8 * (i - 1)..8 * i].copy_from_slice(&xor8(ib, clear));
        }
        xor_residue(payload, n, &mut stream);
    }

    /// Scramble a packet's payload in place (the inverse of
    /// [`Key::descramble`]): for tests, and for making BISS test streams.
    pub fn scramble(&self, payload: &mut [u8]) {
        let n = payload.len() / 8;
        if n == 0 {
            return;
        }
        // Chained from the last block back: each block's cipher input is
        // its plaintext XOR the next block's output.
        let mut ib = vec![[0u8; 8]; n + 1];
        for i in (0..n).rev() {
            ib[i] = self.block_encrypt(xor8(block_at(payload, i), ib[i + 1]));
        }
        let mut stream = Stream::new(&self.cw, &ib[0]);
        payload[..8].copy_from_slice(&ib[0]);
        for (i, b) in ib.iter().enumerate().take(n).skip(1) {
            payload[8 * i..8 * i + 8].copy_from_slice(&xor8(*b, stream.next_block()));
        }
        xor_residue(payload, n, &mut stream);
    }
}

/// [`KEY_PERM`] applied to a 64-bit word.
fn permute_key(w: u64) -> u64 {
    let mut out = 0u64;
    for (n, &to) in KEY_PERM.iter().enumerate() {
        // Bit n from the top moves to bit `to - 1` from the top.
        let bit = (w >> (63 - n)) & 1;
        out |= bit << (63 - (to as u32 - 1));
    }
    out
}

fn block_at(p: &[u8], i: usize) -> [u8; 8] {
    // Rust note: `try_into` turns the 8-byte slice into an array; the
    // slice is always 8 long here, so it cannot fail.
    p[8 * i..8 * i + 8].try_into().unwrap()
}

fn xor8(a: [u8; 8], b: [u8; 8]) -> [u8; 8] {
    std::array::from_fn(|k| a[k] ^ b[k])
}

/// The residue after `n` whole blocks: XORed with the next stream block.
fn xor_residue(payload: &mut [u8], n: usize, stream: &mut Stream) {
    let rest = &mut payload[8 * n..];
    if !rest.is_empty() {
        let s = stream.next_block();
        for (b, k) in rest.iter_mut().zip(s) {
            *b ^= k;
        }
    }
}

/// The 40 bits of ten 4-bit registers.
const MASK40: u64 = (1 << 40) - 1;

/// The stream cipher. Its shift registers A1…A10 and B1…B10 are 4 bits
/// each; here each set is one integer, register k in bits 4(k−1)..4k, so a
/// shift (A_k ← A_{k−1}, A1 ← new) is one shift left.
struct Stream {
    a: u64,
    b: u64,
    x: u8,
    y: u8,
    z: u8,
    d: u8,
    e: u8,
    f: u8,
    p: bool,
    q: bool,
    r: u8,
}

impl Stream {
    /// Load the control word (its first half into A1…A8, its second into
    /// B1…B8, high nibble first) and clock in the first scrambled block.
    fn new(cw: &[u8; 8], iv: &[u8; 8]) -> Self {
        let mut a = 0u64;
        let mut b = 0u64;
        for i in 0..4 {
            for (half, shift) in [(0usize, 4u32), (1, 0)] {
                let k = (2 * i + half) as u32; // register k + 1
                a |= (((cw[i] >> shift) & 0x0F) as u64) << (4 * k);
                b |= (((cw[4 + i] >> shift) & 0x0F) as u64) << (4 * k);
            }
        }
        let mut s = Stream {
            a,
            b,
            x: 0,
            y: 0,
            z: 0,
            d: 0,
            e: 0,
            f: 0,
            p: false,
            q: false,
            r: 0,
        };
        for &byte in iv {
            let (hi, lo) = (byte >> 4, byte & 0x0F);
            // Four clocks a byte; A takes the high nibble then the low, B
            // the other way round.
            for j in 0..4 {
                let (ia, ib) = if j % 2 == 0 { (hi, lo) } else { (lo, hi) };
                s.clock(Some((ia, ib)));
            }
        }
        s
    }

    /// The next 8 bytes of keystream.
    fn next_block(&mut self) -> [u8; 8] {
        std::array::from_fn(|_| {
            let mut op = 0u8;
            for _ in 0..4 {
                op = (op << 2) ^ self.clock(None);
            }
            op
        })
    }

    /// One clock: two bits of keystream. `init` carries the IV nibbles for
    /// A and B while the block is being clocked in.
    #[inline(always)]
    fn clock(&mut self, init: Option<(u8, u8)>) -> u8 {
        let (a, b) = (self.a, self.b);
        // Bit j of register A_k.
        let ab = |k: u32, j: u32| ((a >> (4 * (k - 1) + j)) & 1) as usize;
        // From A1…A9, 35 bits feed the seven S-boxes, 5 bits each.
        let s1 = STREAM_SBOX[0]
            [ab(4, 0) << 4 | ab(1, 2) << 3 | ab(6, 1) << 2 | ab(7, 3) << 1 | ab(9, 0)];
        let s2 = STREAM_SBOX[1]
            [ab(2, 1) << 4 | ab(3, 2) << 3 | ab(6, 3) << 2 | ab(7, 0) << 1 | ab(9, 1)];
        let s3 = STREAM_SBOX[2]
            [ab(1, 3) << 4 | ab(2, 0) << 3 | ab(5, 1) << 2 | ab(5, 3) << 1 | ab(6, 2)];
        let s4 = STREAM_SBOX[3]
            [ab(3, 3) << 4 | ab(1, 1) << 3 | ab(2, 3) << 2 | ab(4, 2) << 1 | ab(8, 0)];
        let s5 = STREAM_SBOX[4]
            [ab(5, 2) << 4 | ab(4, 3) << 3 | ab(6, 0) << 2 | ab(8, 1) << 1 | ab(9, 2)];
        let s6 = STREAM_SBOX[5]
            [ab(3, 1) << 4 | ab(4, 1) << 3 | ab(5, 0) << 2 | ab(7, 2) << 1 | ab(9, 3)];
        let s7 = STREAM_SBOX[6]
            [ab(2, 2) << 4 | ab(3, 0) << 3 | ab(7, 1) << 2 | ab(8, 2) << 1 | ab(8, 3)];
        let bn = |k: u32| ((b >> (4 * (k - 1))) & 0x0F) as u8;
        let (b3, b4, b5, b6, b7, b8, b9) = (bn(3), bn(4), bn(5), bn(6), bn(7), bn(8), bn(9));
        // Sixteen bits of B XORed four by four into a nibble.
        let extra_b = (((b3 & 1) << 3) ^ ((b6 & 2) << 2) ^ ((b7 & 4) << 1) ^ (b9 & 8))
            | (((b6 & 1) << 2) ^ ((b8 & 2) << 1) ^ ((b3 & 8) >> 1) ^ (b4 & 4))
            | (((b5 & 8) >> 2) ^ ((b8 & 4) >> 1) ^ ((b4 & 1) << 1) ^ (b5 & 2))
            | (((b9 & 4) >> 2) ^ ((b6 & 8) >> 3) ^ ((b3 & 2) >> 1) ^ (b8 & 1));
        let mut next_a1 = bn_of(a, 10) ^ self.x;
        let mut next_b1 = b7 ^ bn(10) ^ self.y;
        if let Some((ia, ib)) = init {
            next_a1 ^= self.d ^ ia;
            next_b1 ^= ib;
        }
        if self.p {
            next_b1 = ((next_b1 << 1) | (next_b1 >> 3)) & 0x0F;
        }
        self.d = self.e ^ self.z ^ extra_b;
        // E ← F; F ← Z + E + carry when q, else E.
        let next_e = self.f;
        if self.q {
            let sum = self.z + self.e + self.r;
            self.r = (sum >> 4) & 1;
            self.f = sum & 0x0F;
        } else {
            self.f = self.e;
        }
        self.e = next_e;
        self.a = ((a << 4) | next_a1 as u64) & MASK40;
        self.b = ((b << 4) | next_b1 as u64) & MASK40;
        self.x = ((s4 & 1) << 3) | ((s3 & 1) << 2) | (s2 & 2) | ((s1 & 2) >> 1);
        self.y = ((s6 & 1) << 3) | ((s5 & 1) << 2) | (s4 & 2) | ((s3 & 2) >> 1);
        self.z = ((s2 & 1) << 3) | ((s1 & 1) << 2) | (s6 & 2) | ((s5 & 2) >> 1);
        self.p = s7 & 2 != 0;
        self.q = s7 & 1 != 0;
        // Two keystream bits from D: D3⊕D2, D1⊕D0.
        let t = self.d ^ (self.d >> 1);
        ((t >> 1) & 2) | (t & 1)
    }
}

/// Register k (1-based) of a packed set.
fn bn_of(reg: u64, k: u32) -> u8 {
    ((reg >> (4 * (k - 1))) & 0x0F) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_are_what_they_should_be() {
        // The block S-box and the key permutation are permutations; each
        // stream S-box is balanced (every 2-bit output 8 times). A slip in
        // copying a table breaks one of these.
        let mut seen = [false; 256];
        for &v in &BLOCK_SBOX {
            assert!(!seen[v as usize], "{v:#04x} twice in the block S-box");
            seen[v as usize] = true;
        }
        let mut seen = [false; 64];
        for &v in &KEY_PERM {
            assert!((1..=64).contains(&v));
            assert!(!seen[v as usize - 1], "{v} twice in the key permutation");
            seen[v as usize - 1] = true;
        }
        for (i, sbox) in STREAM_SBOX.iter().enumerate() {
            for out in 0..4 {
                let n = sbox.iter().filter(|&&v| v == out).count();
                assert_eq!(n, 8, "stream S-box {} gives {out} {n} times", i + 1);
            }
        }
        // Spot values of VLC's block_perm table.
        assert_eq!(BLOCK_PERM[0x03], 0x82);
        assert_eq!(BLOCK_PERM[0x0C], 0x30);
        assert_eq!(BLOCK_PERM[0x30], 0x44);
        assert_eq!(BLOCK_PERM[0xC0], 0x09);
        assert_eq!(BLOCK_PERM[0xFF], 0xFF);
    }

    #[test]
    fn the_block_cipher_inverts() {
        let key = Key::new([0x12, 0x34, 0x56, 0xAC, 0x78, 0x9A, 0xBC, 0xAE]);
        for seed in 0..64u8 {
            let b: [u8; 8] =
                std::array::from_fn(|k| seed.wrapping_mul(37).wrapping_add(k as u8 * 11));
            assert_eq!(key.block_decrypt(key.block_encrypt(b)), b);
        }
    }

    #[test]
    fn payloads_of_every_length_round_trip() {
        let key = Key::new([0xA1, 0xB2, 0xC3, 0x36, 0xD4, 0xE5, 0xF6, 0xAF]);
        for len in 0..=184 {
            let clear: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            let mut p = clear.clone();
            key.scramble(&mut p);
            if len >= 8 {
                assert_ne!(p, clear, "{len} bytes left clear");
            } else {
                assert_eq!(p, clear, "under 8 bytes is not scrambled");
            }
            key.descramble(&mut p);
            assert_eq!(p, clear, "{len} bytes");
        }
    }

    #[test]
    fn another_key_does_not_descramble() {
        let a = Key::new([1, 2, 3, 6, 4, 5, 6, 15]);
        let b = Key::new([1, 2, 3, 6, 4, 5, 7, 16]);
        let clear = [0u8; 184];
        let mut p = clear;
        a.scramble(&mut p);
        b.descramble(&mut p);
        assert_ne!(p, clear);
    }
}
