//! CCSDS telemetry (TM) synchronisation and channel coding, CCSDS 131.0-B-4,
//! and the transfer frames it carries (TM, CCSDS 132.0-B-3; AOS, CCSDS
//! 732.0-B-4) — what science and amateur spacecraft send, on a residual
//! carrier (PCM/PSK/PM, PCM/PM bi-phase) or a suppressed one (BPSK/QPSK).
//!
//! Pieces, each with its clause:
//! - the attached sync marker [`ASM`] (§9.3.2) that starts every codeblock;
//! - the pseudo-randomiser (§10.4): h(x) = x⁸ + x⁷ + x⁵ + x³ + 1, all ones at
//!   the start of each codeblock (after the marker), [`randomiser`];
//! - Reed–Solomon (255, 223) E = 16 and (255, 239) E = 8 (§4): field
//!   F(x) = x⁸ + x⁷ + x² + x + 1, generator roots α^(11·j) for
//!   j = 128 − E … 127 + E, symbols sent in Berlekamp's dual basis, I = 1…8
//!   codewords interleaved byte by byte, shortened by virtual fill —
//!   [`Rs`];
//! - the rate-½ K = 7 convolutional code (§3): G1 = 171, G2 = 133 (octal),
//!   G2's output inverted, G1's sent first — [`CONV_CODE`], decoded by
//!   [`crate::convblind`]'s Viterbi;
//! - the TM and AOS primary headers and the frame error control field
//!   (CRC-16, x¹⁶ + x¹² + x⁵ + 1, all ones preset: 132.0-B §4.1.6).
//!
//! The codeblock's layout on a live link is not announced, so
//! [`find_format`] works it out from a few codeblocks: the interleave
//! depth and E from the length, the randomiser and the dual basis by which
//! reading makes every codeword decode.

/// The attached sync marker, 1ACFFC1Dh (131.0-B §9.3.2), sent MSB first.
pub const ASM: u32 = 0x1ACF_FC1D;

/// The convolutional code (131.0-B §3.3): K = 7, G1 = 1111001 (171),
/// G2 = 1011011 (133), the G2 symbol inverted. As [`crate::convblind`]
/// writes generators: bit j = the tap on the input j steps ago.
pub const CONV_CODE: crate::convblind::Code = crate::convblind::Code {
    k: 7,
    // 171 octal, newest tap first → reversed: 1001111b.
    g: [0b100_1111, 0b110_1101],
    inv: [false, true],
};

/// The randomiser's sequence (131.0-B §10.4.1), `n` bytes from the start of
/// a codeblock: FF 48 0E C0 9A 0D 70 BC … (period 255 bits). XORed over the
/// codeblock after the marker, at both ends.
pub fn randomiser(n: usize) -> Vec<u8> {
    // Taps of h(x) = x⁸ + x⁷ + x⁵ + x³ + 1 on a register shifted right; its
    // low bit is the output. All ones at the start (§10.4.2).
    let mut s: u32 = 0xFF;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let mut byte = 0u8;
        for _ in 0..8 {
            byte = (byte << 1) | (s & 1) as u8;
            let fb = (s ^ (s >> 3) ^ (s >> 5) ^ (s >> 7)) & 1;
            s = (s >> 1) | (fb << 7);
        }
        out.push(byte);
    }
    out
}

/// CRC-16 of the frame error control field (132.0-B §4.1.6; 732.0-B
/// §4.1.6): x¹⁶ + x¹² + x⁵ + 1, preset all ones, MSB first, no final XOR.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
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

// ---------------------------------------------------------------------------
// Reed–Solomon over GF(256) with F(x) = x⁸ + x⁷ + x² + x + 1.

/// The field and the dual-basis conversion tables.
struct Gf {
    exp: [u8; 512],
    log: [u8; 256],
    /// Conventional → Berlekamp dual basis (131.0-B Annex F), and back.
    to_dual: [u8; 256],
    from_dual: [u8; 256],
}

impl Gf {
    const fn new() -> Gf {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        let mut i = 0;
        while i < 255 {
            exp[i] = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x187;
            }
            i += 1;
        }
        while i < 512 {
            exp[i] = exp[i - 255];
            i += 1;
        }
        // The dual basis: the rows of 131.0-B Annex F's matrix T, applied to
        // the conventional bits (α⁷ … α⁰ as bits 7 … 0). Same construction
        // as Karn's `gen_ccsds_tab`: to_dual[1] = 7Bh, to_dual[2] = AFh, …
        let tal: [u8; 8] = [0x8d, 0xef, 0xec, 0x86, 0xfa, 0x99, 0xaf, 0x7b];
        let mut to_dual = [0u8; 256];
        let mut from_dual = [0u8; 256];
        let mut v = 0;
        while v < 256 {
            let mut d = 0u8;
            let mut k = 0;
            while k < 8 {
                if v & (1 << k) != 0 {
                    d ^= tal[7 - k];
                }
                k += 1;
            }
            to_dual[v] = d;
            from_dual[d as usize] = v as u8;
            v += 1;
        }
        Gf {
            exp,
            log,
            to_dual,
            from_dual,
        }
    }

    #[inline]
    fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
        }
    }

    #[inline]
    fn div(&self, a: u8, b: u8) -> u8 {
        if a == 0 {
            0
        } else {
            self.exp[(self.log[a as usize] as usize + 255 - self.log[b as usize] as usize) % 255]
        }
    }

    /// α^i for any i ≥ 0.
    #[inline]
    fn pow(&self, i: usize) -> u8 {
        self.exp[i % 255]
    }
}

static GF: Gf = Gf::new();

/// β = α¹¹, the element whose powers the generator's roots are (§4.3.2).
const PRIM: usize = 11;

/// A codeword that could not be corrected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Uncorrectable;

/// One CCSDS Reed–Solomon code, shortened to `n` symbols (virtual fill: the
/// leading 255 − n symbols are zeros, never sent).
#[derive(Debug, Clone)]
pub struct Rs {
    /// Correctable symbol errors: 16 for (255, 223), 8 for (255, 239).
    pub e: usize,
    /// Symbols sent per codeword (≤ 255).
    pub n: usize,
    /// Generator coefficients, conventional basis, highest degree first
    /// (g[0] = 1).
    g: Vec<u8>,
}

impl Rs {
    /// The E-error-correcting code shortened to `n` symbols.
    ///
    /// # Panics
    /// If E is not 8 or 16, or `n` leaves no data symbols.
    pub fn new(e: usize, n: usize) -> Self {
        assert!(e == 8 || e == 16, "CCSDS RS has E = 8 or 16");
        assert!(n <= 255 && n > 2 * e);
        // g(x) = Π (x − β^j), j = 128 − E … 127 + E (§4.3.2).
        let mut g = vec![1u8];
        for j in (128 - e)..(128 + e) {
            let root = GF.pow(PRIM * j);
            let mut next = vec![0u8; g.len() + 1];
            for (i, &c) in g.iter().enumerate() {
                next[i] ^= c;
                next[i + 1] ^= GF.mul(c, root);
            }
            g = next;
        }
        Rs { e, n, g }
    }

    /// Data symbols a codeword carries.
    pub fn k(&self) -> usize {
        self.n - 2 * self.e
    }

    /// Fill in the parity after the `k` data symbols of `cw` (`n` long).
    /// `dual`: the symbols are in the dual basis, as sent.
    pub fn encode(&self, cw: &mut [u8], dual: bool) {
        assert_eq!(cw.len(), self.n);
        let p = 2 * self.e;
        let k = self.k();
        let mut rem = vec![0u8; p];
        for &d in &cw[..k] {
            let m = if dual { GF.from_dual[d as usize] } else { d };
            let fb = m ^ rem[0];
            rem.rotate_left(1);
            rem[p - 1] = 0;
            if fb != 0 {
                for (r, &g) in rem.iter_mut().zip(&self.g[1..]) {
                    *r ^= GF.mul(fb, g);
                }
            }
        }
        for (c, r) in cw[k..].iter_mut().zip(rem) {
            *c = if dual { GF.to_dual[r as usize] } else { r };
        }
    }

    /// Correct `cw` in place (`n` symbols); returns the symbols corrected.
    pub fn decode(&self, cw: &mut [u8], dual: bool) -> Result<usize, Uncorrectable> {
        assert_eq!(cw.len(), self.n);
        let p = 2 * self.e;
        let fcr = 128 - self.e;
        let conv: Vec<u8> = if dual {
            cw.iter().map(|&b| GF.from_dual[b as usize]).collect()
        } else {
            cw.to_vec()
        };
        // Syndromes S_i = r(β^(fcr + i)); symbol 0 of the shortened word is
        // the coefficient of x^(n − 1).
        let mut s = vec![0u8; p];
        let mut clean = true;
        for (i, si) in s.iter_mut().enumerate() {
            let x = GF.pow(PRIM * (fcr + i));
            let mut acc = 0u8;
            for &c in &conv {
                acc = GF.mul(acc, x) ^ c;
            }
            *si = acc;
            clean &= acc == 0;
        }
        if clean {
            return Ok(0);
        }
        // Berlekamp–Massey: Λ(x) = Π (1 − X_l x), coefficients lowest first.
        let mut lambda = vec![0u8; p + 1];
        lambda[0] = 1;
        let mut b = lambda.clone();
        let mut l = 0usize;
        let mut m = 1usize;
        let mut bb = 1u8;
        for r in 0..p {
            let mut d = s[r];
            for i in 1..=l {
                d ^= GF.mul(lambda[i], s[r - i]);
            }
            if d == 0 {
                m += 1;
            } else if 2 * l <= r {
                let t = lambda.clone();
                let coef = GF.div(d, bb);
                for i in m..=p {
                    lambda[i] ^= GF.mul(coef, b[i - m]);
                }
                l = r + 1 - l;
                b = t;
                bb = d;
                m = 1;
            } else {
                let coef = GF.div(d, bb);
                for i in m..=p {
                    lambda[i] ^= GF.mul(coef, b[i - m]);
                }
                m += 1;
            }
        }
        if l > self.e {
            return Err(Uncorrectable);
        }
        // Chien search over the degrees actually sent: an error on x^e has
        // X = β^e, a root of Λ at β^(−e).
        let mut locs = Vec::with_capacity(l);
        for e in 0..self.n {
            let xinv = GF.pow(PRIM * (255 - e % 255));
            let mut acc = 0u8;
            let mut xp = 1u8;
            for &c in lambda.iter().take(l + 1) {
                acc ^= GF.mul(c, xp);
                xp = GF.mul(xp, xinv);
            }
            if acc == 0 {
                locs.push(e);
            }
        }
        if locs.len() != l {
            return Err(Uncorrectable);
        }
        // Ω(x) = S(x)·Λ(x) mod x^p.
        let mut omega = vec![0u8; p];
        for (i, o) in omega.iter_mut().enumerate() {
            let mut acc = 0u8;
            for j in 0..=i.min(l) {
                acc ^= GF.mul(lambda[j], s[i - j]);
            }
            *o = acc;
        }
        // Forney, for first consecutive root fcr:
        // Y = X^(1 − fcr) · Ω(X⁻¹) / Λ'(X⁻¹).
        let mut fixed = conv;
        for &e in &locs {
            let x = GF.pow(PRIM * e);
            let xinv = GF.pow(PRIM * (255 - e % 255));
            let mut num = 0u8;
            let mut xp = 1u8;
            for &o in &omega {
                num ^= GF.mul(o, xp);
                xp = GF.mul(xp, xinv);
            }
            // Λ'(x): the odd terms, i·λ_i x^(i−1) — in characteristic 2,
            // λ_i x^(i−1) for odd i.
            let mut den = 0u8;
            let mut i = 1;
            while i <= l {
                let mut t = lambda[i];
                for _ in 0..i - 1 {
                    t = GF.mul(t, xinv);
                }
                den ^= t;
                i += 2;
            }
            if den == 0 {
                return Err(Uncorrectable);
            }
            // X^(1 − fcr) = X · (X^fcr)⁻¹.
            let xf = GF.pow((PRIM * e * fcr) % 255);
            let y = GF.mul(GF.div(GF.mul(num, x), den), GF.div(1, xf));
            // Degree e is symbol n − 1 − e of the shortened word.
            fixed[self.n - 1 - e] ^= y;
        }
        for (c, f) in cw.iter_mut().zip(fixed) {
            *c = if dual { GF.to_dual[f as usize] } else { f };
        }
        Ok(locs.len())
    }
}

// ---------------------------------------------------------------------------
// Codeblock layout.

/// How a codeblock (the bytes between two markers) is coded.
#[derive(Debug, Clone, PartialEq)]
pub struct Format {
    /// Bytes from one marker's end to the next marker.
    pub block_bytes: usize,
    /// Reed–Solomon: E, the interleave depth, the dual basis; `None`
    /// uncoded (the frame error control field, if any, guards it).
    pub rs: Option<RsLayout>,
    /// The randomiser is on.
    pub randomised: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RsLayout {
    pub e: usize,
    pub interleave: usize,
    /// Symbols per codeword as sent (255 less any virtual fill).
    pub n: usize,
    pub dual: bool,
}

impl Format {
    /// The transfer frame's length, bytes: the codeblock less the parity.
    pub fn frame_bytes(&self) -> usize {
        match self.rs {
            Some(r) => r.interleave * (r.n - 2 * r.e),
            None => self.block_bytes,
        }
    }

    /// "RS(255,223) I = 4, dual basis, randomised".
    pub fn describe(&self) -> String {
        let mut s = match self.rs {
            Some(r) => format!(
                "RS({},{}) I = {}{}",
                r.n,
                r.n - 2 * r.e,
                r.interleave,
                if r.dual { "" } else { ", conventional basis" }
            ),
            None => "no Reed–Solomon".into(),
        };
        if self.randomised {
            s.push_str(", randomised");
        }
        s
    }

    /// Decode one codeblock into its transfer frame: derandomise, then
    /// correct each interleaved codeword. Returns the frame and the
    /// symbols corrected, or `Err` with the frame as read when a codeword
    /// failed.
    pub fn decode(&self, block: &[u8]) -> Result<(Vec<u8>, usize), Vec<u8>> {
        let mut b = block[..self.block_bytes.min(block.len())].to_vec();
        if self.randomised {
            for (x, r) in b.iter_mut().zip(randomiser(self.block_bytes)) {
                *x ^= r;
            }
        }
        let Some(rl) = self.rs else {
            return Ok((b, 0));
        };
        let code = Rs::new(rl.e, rl.n);
        let k = code.k();
        let mut frame = vec![0u8; rl.interleave * k];
        let mut corrected = 0;
        let mut failed = false;
        let mut cw = vec![0u8; rl.n];
        for i in 0..rl.interleave {
            for (j, c) in cw.iter_mut().enumerate() {
                *c = b[j * rl.interleave + i];
            }
            match code.decode(&mut cw, rl.dual) {
                Ok(n) => corrected += n,
                Err(_) => failed = true,
            }
            for j in 0..k {
                frame[j * rl.interleave + i] = cw[j];
            }
        }
        if failed {
            Err(frame)
        } else {
            Ok((frame, corrected))
        }
    }
}

/// Work out how codeblocks of `blocks` (each the bytes after a marker, all
/// the same length) are coded: an uncoded block whose frame error control
/// field checks (randomised or not) first — a check passes by chance once
/// in 65 536, where frames that are mostly zeros "decode" under a wrong
/// Reed–Solomon layout as near enough the all-zero codeword (Eutelsat
/// 172B's beacon) — then every Reed–Solomon layout their length allows
/// (E = 16 then 8, interleave 1–8, virtual fill), randomised or not, dual
/// basis or not, until one decodes every codeword of every block. `None`:
/// nothing fits.
pub fn find_format(blocks: &[&[u8]]) -> Option<Format> {
    let len = blocks.first()?.len();
    if len < 8 || blocks.iter().any(|b| b.len() < len) {
        return None;
    }
    for randomised in [true, false] {
        let f = Format {
            block_bytes: len,
            rs: None,
            randomised,
        };
        let crc_ok = blocks.iter().all(|b| {
            let (fr, _) = f.decode(b).unwrap_or_default();
            fr.len() > 8
                && crc16(&fr[..fr.len() - 2])
                    == u16::from_be_bytes([fr[fr.len() - 2], fr[fr.len() - 1]])
        });
        if crc_ok {
            return Some(f);
        }
    }
    for e in [16usize, 8] {
        for interleave in 1..=8usize {
            if !len.is_multiple_of(interleave) {
                continue;
            }
            let n = len / interleave;
            if n > 255 || n <= 2 * e + 4 {
                continue;
            }
            for randomised in [true, false] {
                for dual in [true, false] {
                    let f = Format {
                        block_bytes: len,
                        rs: Some(RsLayout {
                            e,
                            interleave,
                            n,
                            dual,
                        }),
                        randomised,
                    };
                    if blocks.iter().all(|b| f.decode(b).is_ok()) {
                        return Some(f);
                    }
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Transfer frames.

/// A transfer frame's primary header: TM (version 1, field value 00b) or
/// AOS (version 2, 01b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Transfer frame version number field: 0 TM, 1 AOS.
    pub version: u8,
    /// Spacecraft identifier: 10 bits (TM), 8 bits (AOS).
    pub scid: u16,
    /// Virtual channel: 3 bits (TM), 6 bits (AOS).
    pub vcid: u8,
    /// Master channel frame count (TM, 8 bits); `None` on AOS.
    pub mc_count: Option<u8>,
    /// Virtual channel frame count: 8 bits (TM), 24 bits (AOS).
    pub vc_count: u32,
    /// TM: operational control field present; first header pointer
    /// (11 bits; 7FFh = no packet starts here, 7FEh = idle data).
    pub ocf: bool,
    pub first_header: Option<u16>,
}

impl Header {
    /// Read the header at the start of `f` (132.0-B §4.1.2; 732.0-B §4.1.2).
    pub fn parse(f: &[u8]) -> Option<Header> {
        if f.len() < 6 {
            return None;
        }
        let w = u16::from_be_bytes([f[0], f[1]]);
        match w >> 14 {
            0 => Some(Header {
                version: 0,
                scid: (w >> 4) & 0x3FF,
                vcid: ((w >> 1) & 7) as u8,
                ocf: w & 1 == 1,
                mc_count: Some(f[2]),
                vc_count: u32::from(f[3]),
                first_header: Some(u16::from_be_bytes([f[4], f[5]]) & 0x7FF),
            }),
            1 => Some(Header {
                version: 1,
                scid: (w >> 6) & 0xFF,
                vcid: (w & 0x3F) as u8,
                ocf: false,
                mc_count: None,
                vc_count: u32::from_be_bytes([0, f[2], f[3], f[4]]),
                first_header: None,
            }),
            _ => None,
        }
    }

    /// The virtual channel carries only idle data: TM VC 7 by convention
    /// with first header pointer 7FEh, AOS VC 63 (732.0-B §4.1.2.3).
    pub fn idle(&self) -> bool {
        match self.version {
            0 => self.first_header == Some(0x7FE),
            _ => self.vcid == 63,
        }
    }

    pub fn kind(&self) -> &'static str {
        if self.version == 0 { "TM" } else { "AOS" }
    }
}

/// What a stream of transfer frames has shown.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    /// Codeblocks seen, decoded (every codeword), failed.
    pub blocks: u64,
    pub decoded: u64,
    pub failed: u64,
    /// Symbols the Reed–Solomon decoder corrected.
    pub corrected: u64,
    /// Frames whose error control field checked, and failed.
    pub crc_ok: u64,
    pub crc_bad: u64,
    /// Frames per (spacecraft, virtual channel), with the last count.
    pub channels: std::collections::BTreeMap<(u16, u8), VcStats>,
    /// Master-channel frames missing by the count (TM).
    pub lost: u64,
    /// The header of the last good frame.
    pub last: Option<Header>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VcStats {
    pub frames: u64,
    pub idle: u64,
    pub last_count: u32,
}

/// The receiver after the marker: codeblocks in, frames and statistics out.
#[derive(Debug, Clone, Default)]
pub struct TmRx {
    format: Option<Format>,
    /// Codeblocks kept while the format is worked out.
    pending: Vec<Vec<u8>>,
    /// Blocks in a row the format failed on (a wrong guess re-searched).
    misses: u32,
    /// The frame error control field is present (seen checking).
    fecf: Option<bool>,
    last_mc: Option<u8>,
    pub stats: Stats,
}

/// A decoded transfer frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub bytes: Vec<u8>,
    pub header: Option<Header>,
    /// Reed–Solomon (if any) corrected it, and its CRC (if present) checks.
    pub good: bool,
}

impl TmRx {
    pub fn new() -> Self {
        Self::default()
    }

    /// The layout found, once found.
    pub fn format(&self) -> Option<&Format> {
        self.format.as_ref()
    }

    /// Codeblocks seen so far and no layout fits them.
    pub fn unknown(&self) -> bool {
        self.format.is_none() && self.pending.len() >= 4
    }

    /// One codeblock (the bytes after a marker); any frames it gives (the
    /// first few come together once the layout is found).
    pub fn push(&mut self, block: &[u8], out: &mut Vec<Frame>) {
        if self.format.is_none() {
            self.pending.push(block.to_vec());
            if self.pending.len() > 8 {
                self.pending.remove(0);
            }
            let take = self.pending.len().min(3);
            if take >= 2 {
                let refs: Vec<&[u8]> = self.pending[self.pending.len() - take..]
                    .iter()
                    .map(|b| b.as_slice())
                    .collect();
                if let Some(f) = find_format(&refs) {
                    self.format = Some(f);
                    let blocks = std::mem::take(&mut self.pending);
                    for b in blocks {
                        self.decode(&b, out);
                    }
                }
            }
            return;
        }
        self.decode(block, out);
    }

    fn decode(&mut self, block: &[u8], out: &mut Vec<Frame>) {
        let Some(f) = self.format.clone() else {
            return;
        };
        self.stats.blocks += 1;
        let (bytes, ok) = match f.decode(block) {
            Ok((b, n)) => {
                self.stats.decoded += 1;
                self.stats.corrected += n as u64;
                self.misses = 0;
                (b, true)
            }
            Err(b) => {
                self.stats.failed += 1;
                self.misses += 1;
                (b, false)
            }
        };
        // A layout that stops decoding altogether (a wrong first guess):
        // look again.
        if self.misses >= 16 && self.stats.decoded == 0 {
            self.format = None;
            self.misses = 0;
        }
        let crc = (bytes.len() > 8).then(|| {
            let n = bytes.len();
            crc16(&bytes[..n - 2]) == u16::from_be_bytes([bytes[n - 2], bytes[n - 1]])
        });
        // The field is optional: taken as present once it has checked on a
        // frame (a random one checks once in 65 536).
        if crc == Some(true) {
            self.fecf = Some(true);
        }
        let mut good = ok;
        if self.fecf == Some(true) {
            match crc {
                Some(true) => self.stats.crc_ok += 1,
                _ => {
                    self.stats.crc_bad += 1;
                    good = false;
                }
            }
        }
        let header = Header::parse(&bytes).filter(|_| good);
        if let Some(h) = header {
            let vc = self.stats.channels.entry((h.scid, h.vcid)).or_default();
            vc.frames += 1;
            if h.idle() {
                vc.idle += 1;
            }
            vc.last_count = h.vc_count;
            if let Some(mc) = h.mc_count {
                if let Some(prev) = self.last_mc {
                    self.stats.lost += u64::from(mc.wrapping_sub(prev).wrapping_sub(1));
                }
                self.last_mc = Some(mc);
            }
            self.stats.last = Some(h);
        }
        out.push(Frame {
            bytes,
            header,
            good,
        });
    }
}

/// Build a codeblock (no marker) from a transfer frame under `f`: encode,
/// interleave, randomise — for tests and test signals.
pub fn encode_block(f: &Format, frame: &[u8]) -> Vec<u8> {
    assert_eq!(frame.len(), f.frame_bytes());
    let mut b = match f.rs {
        None => frame.to_vec(),
        Some(rl) => {
            let code = Rs::new(rl.e, rl.n);
            let k = code.k();
            let mut b = vec![0u8; rl.interleave * rl.n];
            let mut cw = vec![0u8; rl.n];
            for i in 0..rl.interleave {
                for j in 0..k {
                    cw[j] = frame[j * rl.interleave + i];
                }
                code.encode(&mut cw, rl.dual);
                for (j, &c) in cw.iter().enumerate() {
                    b[j * rl.interleave + i] = c;
                }
            }
            b
        }
    };
    if f.randomised {
        for (x, r) in b.iter_mut().zip(randomiser(f.block_bytes)) {
            *x ^= r;
        }
    }
    b
}

/// A TM transfer frame of `len` bytes (header, data, CRC-16), for tests and
/// test signals: spacecraft `scid`, virtual channel `vcid`, the counts.
pub fn tm_frame(len: usize, scid: u16, vcid: u8, mc: u8, vc: u8, data: &[u8]) -> Vec<u8> {
    let mut f = vec![0u8; len];
    let w: u16 = ((scid & 0x3FF) << 4) | (u16::from(vcid & 7) << 1);
    f[..2].copy_from_slice(&w.to_be_bytes());
    f[2] = mc;
    f[3] = vc;
    // No secondary header, packets in order, first header pointer 0.
    f[4] = 0x18;
    f[5] = 0x00;
    let body = len - 8;
    for (i, x) in f[6..6 + body].iter_mut().enumerate() {
        *x = data.get(i).copied().unwrap_or(0x55);
    }
    let crc = crc16(&f[..len - 2]);
    f[len - 2..].copy_from_slice(&crc.to_be_bytes());
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn randomiser_starts_as_the_book_says() {
        // 131.0-B §10.4.1: the sequence begins FF 48 0E C0 9A 0D 70 BC.
        assert_eq!(
            randomiser(8),
            [0xFF, 0x48, 0x0E, 0xC0, 0x9A, 0x0D, 0x70, 0xBC]
        );
        // Period 255 bits: byte 255 restarts the pattern shifted by bits;
        // the bit sequence repeats exactly after 255.
        let r = randomiser(64);
        let bit = |i: usize| (r[i / 8] >> (7 - i % 8)) & 1;
        for i in 0..200 {
            assert_eq!(bit(i), bit(i + 255));
        }
    }

    #[test]
    fn dual_basis_tables_match_annex_f() {
        // Karn's Taltab begins 00 7B AF D4 99 E2 36 4D FA 81, Tal1tab
        // 00 CC AC 60 79 B5 D5 19.
        assert_eq!(
            &GF.to_dual[..10],
            &[0x00, 0x7b, 0xaf, 0xd4, 0x99, 0xe2, 0x36, 0x4d, 0xfa, 0x81]
        );
        assert_eq!(
            &GF.from_dual[..8],
            &[0x00, 0xcc, 0xac, 0x60, 0x79, 0xb5, 0xd5, 0x19]
        );
    }

    #[test]
    fn crc16_check_value() {
        // CRC-16/CCITT-FALSE of "123456789" is 29B1h.
        assert_eq!(crc16(b"123456789"), 0x29B1);
    }

    #[test]
    fn rs_corrects_up_to_e_errors_in_both_bases() {
        for (e, n) in [(16usize, 255usize), (8, 255), (16, 200)] {
            for dual in [true, false] {
                let rs = Rs::new(e, n);
                let mut cw: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
                rs.encode(&mut cw, dual);
                let mut clean = cw.clone();
                assert_eq!(rs.decode(&mut clean, dual), Ok(0));
                let mut bad = cw.clone();
                for k in 0..e {
                    bad[(k * 13 + 5) % n] ^= (k as u8).wrapping_mul(29) | 1;
                }
                assert_eq!(rs.decode(&mut bad, dual), Ok(e), "e {e} n {n} dual {dual}");
                assert_eq!(bad, cw);
                // One more is beyond it.
                let mut worse = cw.clone();
                for k in 0..=e {
                    worse[(k * 7 + 1) % n] ^= 0x5A;
                }
                assert!(rs.decode(&mut worse, dual).is_err() || worse != cw);
            }
        }
    }

    #[test]
    fn a_codeword_in_the_dual_basis_is_not_one_in_the_conventional() {
        // What lets find_format tell the two apart.
        let rs = Rs::new(16, 255);
        let mut cw: Vec<u8> = (0..255).map(|i| (i * 3 + 1) as u8).collect();
        rs.encode(&mut cw, true);
        let mut as_conv = cw.clone();
        assert!(rs.decode(&mut as_conv, false).is_err() || as_conv != cw);
    }

    #[test]
    fn finds_an_interleaved_randomised_layout_and_decodes_frames() {
        let f = Format {
            block_bytes: 4 * 255,
            rs: Some(RsLayout {
                e: 16,
                interleave: 4,
                n: 255,
                dual: true,
            }),
            randomised: true,
        };
        let frames: Vec<Vec<u8>> = (0..5u8)
            .map(|i| tm_frame(f.frame_bytes(), 0x2A3, i % 3, i, i, &[i; 40]))
            .collect();
        let mut blocks: Vec<Vec<u8>> = frames.iter().map(|fr| encode_block(&f, fr)).collect();
        // Some errors in each.
        for (k, b) in blocks.iter_mut().enumerate() {
            let n = b.len();
            for j in 0..20 {
                b[(j * 41 + k * 7) % n] ^= 0xFF;
            }
        }
        let refs: Vec<&[u8]> = blocks.iter().map(|b| b.as_slice()).collect();
        assert_eq!(find_format(&refs[..3]), Some(f.clone()));
        let mut rx = TmRx::new();
        let mut out = Vec::new();
        for b in &blocks {
            rx.push(b, &mut out);
        }
        assert_eq!(out.len(), 5);
        for (o, fr) in out.iter().zip(&frames) {
            assert!(o.good);
            assert_eq!(&o.bytes, fr);
        }
        let h = out[1].header.unwrap();
        assert_eq!(
            (h.version, h.scid, h.vcid, h.mc_count),
            (0, 0x2A3, 1, Some(1))
        );
        assert_eq!(rx.stats.lost, 0);
        assert_eq!(rx.stats.crc_ok, 5);
        assert_eq!(rx.stats.channels.len(), 3);
    }

    #[test]
    fn an_uncoded_frame_is_found_by_its_crc() {
        let f = Format {
            block_bytes: 223,
            rs: None,
            randomised: true,
        };
        let blocks: Vec<Vec<u8>> = (0..3u8)
            .map(|i| encode_block(&f, &tm_frame(223, 77, 2, i, i, &[0x10; 9])))
            .collect();
        let refs: Vec<&[u8]> = blocks.iter().map(|b| b.as_slice()).collect();
        assert_eq!(find_format(&refs), Some(f));
    }

    #[test]
    fn the_convolutional_code_is_the_books() {
        // G1 = 171, G2 = 133 octal, the second inverted (131.0-B §3.3).
        assert_eq!(CONV_CODE.octal(), [0o171, 0o133]);
        // All-zero input: G1 zeros, G2 ones (inverted).
        let c = CONV_CODE.encode(&[0, 0, 0]);
        assert_eq!(c, [0, 1, 0, 1, 0, 1]);
    }

    #[test]
    fn aos_header() {
        // Version 01, SCID 0xAB, VCID 5, VC count 0x010203.
        let w: u16 = (1 << 14) | (0xAB << 6) | 5;
        let mut f = w.to_be_bytes().to_vec();
        f.extend([1, 2, 3, 0, 0, 0]);
        let h = Header::parse(&f).unwrap();
        assert_eq!(
            (h.version, h.scid, h.vcid, h.vc_count),
            (1, 0xAB, 5, 0x010203)
        );
        assert_eq!(h.kind(), "AOS");
    }
}
