//! ITU-T G.728 LD-CELP at 16 kbit/s: the decoder, and the voice channel it
//! decodes on a 16 kbit/s sub-channel of a 64 kbit/s timeslot.
//!
//! G.728 sends one 10-bit codeword per five samples (0.625 ms): a 7-bit
//! shape index then a 3-bit gain index (§3.9), MSB first on a serial link
//! (§5.11). Everything else — the 50th-order synthesis filter, the
//! excitation gain — the decoder works out backwards from what it has
//! already decoded, so it only stays in step with the encoder if it sees
//! every codeword in order.
//!
//! [`Decoder`] follows the Recommendation's own description of the decoder
//! (§5.14, blocks 29–33 and the postfilter, blocks 71–85), with its update
//! timing (Annex E, Table E.1) and constants (Table 1, Annexes A–D). It is
//! floating point, as the main body is (Annex G's bit-exact fixed point is
//! not needed to listen).
//!
//! [`Lane`] finds and decodes such a channel in two bits of each octet of a
//! timeslot, as a Comtech CDM-600L's Drop & Insert timeslot 1 carries it
//! (bits 2–3). That equipment frames the channel G.728's way (§3.11): the
//! encoder searches only half the shape codebook, so the shape index's MSB
//! (codeword bit 9) is free to carry a sync bit. Here it does in every
//! codeword: inverted on the line, 1 in 31 codewords and 0 in the 32nd —
//! the last of an adaptation cycle, every 20 ms. In silence the equipment
//! sends a fill instead (the sync bit held, a 4-bit count in the codeword's
//! low bits), which a decoder must not be fed.
//!
//! [`Framer`] does that framing on any bit stream — a Q-Flex call's, put
//! together from four TDM channels (`tdm257`), comes with a bit in 64
//! not carried at all.

mod tables;

use tables::{FACGPV, FACV, SPFPCFV, SPFZCFV, WNR, WNRLG, Y};

// Table 1 (§5.1): the coder parameters the decoder uses.
const IDIM: usize = 5; // vector dimension
const LPC: usize = 50; // synthesis filter order
const LPCLG: usize = 10; // log-gain predictor order
const NFRSZ: usize = 20; // adaptation cycle in samples
const NONR: usize = 35; // non-recursive window samples, synthesis filter
const NONRLG: usize = 20; // non-recursive window samples, log-gain predictor
const NUPDATE: usize = 4; // adaptation cycle in vectors
const NPWSZ: usize = 100; // pitch analysis window
const KPMIN: usize = 20; // pitch period range, samples
const KPMAX: usize = 140;
const KPDELTA: usize = 6; // allowed deviation from the previous pitch period
const GOFF: f64 = 32.0; // log-gain offset, dB
const WNCF: f64 = 257.0 / 256.0; // white noise correction
const AGCFAC: f64 = 0.99; // postfilter AGC speed
const PPFTH: f64 = 0.6; // pitch postfilter on above this tap
const PPFZCF: f64 = 0.15; // pitch postfilter zero control
const TAPTH: f64 = 0.4; // fundamental pitch replacement threshold
const TILTF: f64 = 0.15; // spectral tilt compensation

/// §3.1.1: the coder works on ±4095 (16-bit PCM read as Q3), and §5.13's
/// limiter holds the synthesis filter memory there.
const MAX: f64 = 4095.0;

/// Annex D: the 1 kHz third-order elliptic lowpass of the pitch extractor,
/// L(z) = Σ b_i z^-i / (1 + Σ a_i z^-i). (The 2012 text prints a1 as
/// "2–2.3403658918" and loses other signs; these are the values that give
/// the filter unit gain at DC and its zero at 4 kHz.)
const AL: [f64; 3] = [-2.340_365_891_8, 2.011_900_199, -0.614_109_218];
const BL: [f64; 4] = [
    0.035_708_166_7,
    -0.006_995_624_4,
    -0.006_995_624_4,
    0.035_708_166_7,
];

/// Annex B: the gain codebook — GQ(1) = 33/64, each level 7/4 the one
/// before, indices 4–7 the same levels negated.
fn gq(ig: usize) -> f64 {
    let m = 33.0 / 64.0 * 1.75f64.powi((ig & 3) as i32);
    if ig & 4 != 0 { -m } else { m }
}

/// The hybrid windowing module, blocks 43 and 49 (§5.6, §5.7): shifts
/// `new` into the signal buffer `sb`, windows it (`w`, newest sample first)
/// and returns the autocorrelation `r[0..=order]`, keeping the window's
/// recursive part in `rexp`.
fn hybrid_window(
    sb: &mut [f64],
    rexp: &mut [f64],
    w: &[i16],
    new: &[f64],
    order: usize,
    r: &mut [f64],
) {
    let n3 = sb.len();
    let n1 = order + new.len();
    let n2 = n3 - new.len();
    sb.copy_within(new.len().., 0);
    sb[n2..].copy_from_slice(new);
    // Rust note: a fixed-size scratch array on the stack; 105 is the
    // largest window (LPC + NFRSZ + NONR).
    let mut ws = [0.0f64; LPC + NFRSZ + NONR];
    for (n, v) in sb.iter().enumerate() {
        ws[n] = v * f64::from(w[n3 - 1 - n]) / 32768.0;
    }
    for i in 0..=order {
        let tmp: f64 = (order..n1).map(|n| ws[n] * ws[n - i]).sum();
        rexp[i] = 0.75 * rexp[i] + tmp;
        r[i] = rexp[i] + (n1..n3).map(|n| ws[n] * ws[n - i]).sum::<f64>();
    }
    r[0] *= WNCF;
}

/// The Levinson–Durbin recursion, block 37 as blocks 44 and 50 use it:
/// predictor `a[0..=order]` (a[0] = 1, the Recommendation's sign) from the
/// autocorrelation `r`. False when the recursion is skipped or ill-
/// conditioned: the caller keeps its old coefficients. With `at10`, the
/// order-10 predictor and first reflection coefficient are copied out on
/// the way (for the postfilter, §4.6).
fn levinson(r: &[f64], order: usize, a: &mut [f64], at10: &mut Option<([f64; 11], f64)>) -> bool {
    if r[order] == 0.0 || r[0] <= 0.0 {
        return false;
    }
    let rc1 = -r[1] / r[0];
    a[0] = 1.0;
    a[1] = rc1;
    let mut alpha = r[0] + r[1] * rc1;
    if alpha <= 0.0 {
        return false;
    }
    for minc in 2..=order {
        let sum: f64 = (1..=minc).map(|ip| r[minc - ip + 1] * a[ip - 1]).sum();
        let rc = -sum / alpha;
        for ip in 2..=minc / 2 + 1 {
            let ib = minc - ip + 2;
            let at = a[ip - 1] + rc * a[ib - 1];
            a[ib - 1] += rc * a[ip - 1];
            a[ip - 1] = at;
        }
        a[minc] = rc;
        alpha += rc * sum;
        if alpha <= 0.0 {
            return false;
        }
        if minc == 10 && order > 10 {
            let mut apf = [0.0; 11];
            apf.copy_from_slice(&a[..11]);
            *at10 = Some((apf, rc1));
        }
    }
    true
}

/// The G.728 decoder (§5.14). One codeword in, five samples out.
pub struct Decoder {
    /// The adaptive postfilter (§4.6) on: as G.728 sounds in service. Off
    /// gives the plain synthesis output (§4.6.1), as the test vectors want.
    pub postfilter: bool,
    /// Vector index in the 4-vector adaptation cycle, 1..=4 (ICOUNT).
    icount: usize,
    // Synthesis filter (block 32) and its backward adapter (33: 49–51).
    a: [f64; LPC + 1],
    atmp: [f64; LPC + 1],
    a_pending: bool,
    statelpc: [f64; LPC],
    sb: [f64; LPC + NFRSZ + NONR],
    rexp: [f64; LPC + 1],
    /// This cycle's decoded speech, the next cycle's window input (STTMP).
    cycle: [f64; NFRSZ],
    // Backward gain adapter (block 30: 39–48, 67).
    gp: [f64; LPCLG + 1],
    gstate: [f64; LPCLG],
    sblg: [f64; LPCLG + NUPDATE + NONRLG],
    rexplg: [f64; LPCLG + 1],
    et: [f64; IDIM],
    // Postfilter adapter (81–85) and postfilter (71–77).
    apf: [f64; 11],
    ap: [f64; 11],
    az: [f64; 11],
    tiltz: f64,
    stlpci: [f64; 10],
    /// LPC residual D(−139..100), at `d[k + 139]`.
    d: [f64; KPMAX + NPWSZ],
    ip: usize,
    /// Decimated residual DEC(−34..25), at `dec[n + 34]`.
    dec: [f64; 60],
    stlpf: [f64; 3],
    kp: usize,
    kp1: usize,
    b: f64,
    gl: f64,
    /// Decoded speech ST(−239..5), at `st[k + 239]`.
    st: [f64; NPWSZ + KPMAX + IDIM],
    stpffir: [f64; 10],
    stpfiir: [f64; 10],
    scalefil: f64,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

/// `[1, 0, 0, …]`: the initial value of every predictor (Table 2).
fn unit<const N: usize>() -> [f64; N] {
    let mut a = [0.0; N];
    a[0] = 1.0;
    a
}

impl Decoder {
    /// A decoder in the Recommendation's initial state (Table 2), postfilter
    /// on, at the first vector of an adaptation cycle.
    pub fn new() -> Self {
        let mut gp = unit::<{ LPCLG + 1 }>();
        gp[1] = -1.0;
        Self {
            postfilter: true,
            icount: 1,
            a: unit(),
            atmp: unit(),
            a_pending: false,
            statelpc: [0.0; LPC],
            sb: [0.0; LPC + NFRSZ + NONR],
            rexp: [0.0; LPC + 1],
            cycle: [0.0; NFRSZ],
            gp,
            gstate: [-GOFF; LPCLG],
            sblg: [0.0; LPCLG + NUPDATE + NONRLG],
            rexplg: [0.0; LPCLG + 1],
            et: [0.0; IDIM],
            apf: unit(),
            ap: unit(),
            az: unit(),
            tiltz: 0.0,
            stlpci: [0.0; 10],
            d: [0.0; KPMAX + NPWSZ],
            ip: NPWSZ - NFRSZ + IDIM,
            dec: [0.0; 60],
            stlpf: [0.0; 3],
            kp: 50,
            kp1: 50,
            b: 0.0,
            gl: 1.0,
            st: [0.0; NPWSZ + KPMAX + IDIM],
            stpffir: [0.0; 10],
            stpfiir: [0.0; 10],
            scalefil: 1.0,
        }
    }

    /// Make the next codeword the first of an adaptation cycle (as after a
    /// sync codeword that ends one, §3.11).
    pub fn start_cycle(&mut self) {
        self.icount = 1;
    }

    /// Decode one codeword (its low 10 bits): five samples, ±1 full scale.
    pub fn decode(&mut self, ichan: u16, out: &mut [f32; IDIM]) {
        // Block 33: a new synthesis filter from the last cycle's speech,
        // worked out at vector 1 and used from vector 3 (Table E.1); the
        // short-term postfilter's coefficients fall out of it (block 85).
        if self.icount == 1 {
            self.synthesis_adapter();
        }
        if self.icount == 3 && self.a_pending {
            self.a = self.atmp;
            self.a_pending = false;
        }
        let gain = self.gain_adapter();
        // Blocks 29 and 31: shape × gain level × excitation gain.
        let ichan = usize::from(ichan & 0x3FF);
        let g = gain * gq(ichan & 7) / 2048.0;
        for (e, &y) in self.et.iter_mut().zip(&Y[ichan >> 3]) {
            *e = g * f64::from(y);
        }
        let st = self.synthesis();
        let pf = self.postfilter_vector(&st);
        let y = if self.postfilter { pf } else { st };
        for (o, v) in out.iter_mut().zip(y) {
            // Back from Q3 (§3.1.1) to 16-bit PCM, then to ±1.
            *o = (v * 8.0 / 32768.0) as f32;
        }
        self.cycle[(self.icount - 1) * IDIM..][..IDIM].copy_from_slice(&st);
        self.icount = self.icount % NUPDATE + 1;
    }

    /// Blocks 49–51 and 85 (§5.6, §5.14), at the first vector of a cycle.
    fn synthesis_adapter(&mut self) {
        let mut r = [0.0; LPC + 1];
        hybrid_window(&mut self.sb, &mut self.rexp, &WNR, &self.cycle, LPC, &mut r);
        let mut atmp = unit::<{ LPC + 1 }>();
        let mut at10 = None;
        let ok = levinson(&r, LPC, &mut atmp, &mut at10);
        if let Some((apf, k1)) = at10 {
            // Block 85: the short-term postfilter from the order-10 predictor.
            self.apf = apf;
            for i in 1..=10 {
                self.ap[i] = f64::from(SPFPCFV[i]) / 16384.0 * apf[i];
                self.az[i] = f64::from(SPFZCFV[i]) / 16384.0 * apf[i];
            }
            self.tiltz = TILTF * k1;
        }
        if ok {
            // Block 51: bandwidth expansion; the filter changes at vector 3.
            for (a, &f) in atmp.iter_mut().zip(&FACV).skip(1) {
                *a *= f64::from(f) / 16384.0;
            }
            self.atmp = atmp;
            self.a_pending = true;
        }
    }

    /// Block 30 (§5.7): the excitation gain, predicted from the log-gains
    /// of the excitation already decoded.
    fn gain_adapter(&mut self) -> f64 {
        // Blocks 67, 39, 40 and 42: the previous excitation's dB level
        // (clipped at 0 dB), less the offset.
        let e = self.et.iter().map(|v| v * v).sum::<f64>() / IDIM as f64;
        self.gstate[0] = 10.0 * e.max(1.0).log10() - GOFF;
        if self.icount == 2 {
            // Blocks 43–45: a new predictor from the log-gains of the last
            // four vectors (oldest first), used from this vector on.
            let g = &self.gstate;
            let gtmp = [g[3], g[2], g[1], g[0]];
            let mut r = [0.0; LPCLG + 1];
            hybrid_window(
                &mut self.sblg,
                &mut self.rexplg,
                &WNRLG,
                &gtmp,
                LPCLG,
                &mut r,
            );
            let mut gptmp = unit::<{ LPCLG + 1 }>();
            if levinson(&r, LPCLG, &mut gptmp, &mut None) {
                for i in 1..=LPCLG {
                    self.gp[i] = f64::from(FACGPV[i]) / 16384.0 * gptmp[i];
                }
            }
        }
        // Block 46, shifting the predictor memory as it goes.
        let mut gain = 0.0;
        for i in (1..LPCLG).rev() {
            gain -= self.gp[i + 1] * self.gstate[i];
            self.gstate[i] = self.gstate[i - 1];
        }
        gain -= self.gp[1] * self.gstate[0];
        // The offset back, block 47's limits (gain 1 to 1000), block 48.
        10f64.powf((gain + GOFF).clamp(0.0, 60.0) / 20.0)
    }

    /// Block 32: the synthesis filter as the sum of its zero-input and
    /// zero-state responses, with §5.13's limiter. STATELPC holds the
    /// output newest first.
    fn synthesis(&mut self) -> [f64; IDIM] {
        let a = &self.a;
        let s = &mut self.statelpc;
        for _ in 0..IDIM {
            let mut t = 0.0;
            for j in (1..LPC).rev() {
                t -= s[j] * a[j + 1];
                s[j] = s[j - 1];
            }
            t -= s[0] * a[1];
            s[0] = t;
        }
        let mut zsr = [0.0; IDIM];
        zsr[0] = self.et[0];
        for k in 1..IDIM {
            let mut a0 = self.et[k];
            for i in (1..=k).rev() {
                zsr[i] = zsr[i - 1];
                a0 -= a[i] * zsr[i];
            }
            zsr[0] = a0;
        }
        let mut st = [0.0; IDIM];
        for k in 0..IDIM {
            s[k] = (s[k] + zsr[k]).clamp(-MAX, MAX);
        }
        for (k, v) in st.iter_mut().enumerate() {
            *v = s[IDIM - 1 - k];
        }
        st
    }

    /// The postfilter adapter (blocks 81–84) and postfilter (71–77).
    fn postfilter_vector(&mut self, st: &[f64; IDIM]) -> [f64; IDIM] {
        const OFF: usize = NPWSZ + KPMAX - 1; // ST(k) is st[k + OFF]
        self.st[OFF + 1..].copy_from_slice(st);
        // Block 81: the 10th-order LPC residual into D(81..100).
        if self.ip == NPWSZ {
            self.ip = NPWSZ - NFRSZ;
        }
        for (k, &x) in st.iter().enumerate() {
            let mut v = x;
            for j in (1..10).rev() {
                v += self.stlpci[j] * self.apf[j + 1];
                self.stlpci[j] = self.stlpci[j - 1];
            }
            v += self.stlpci[0] * self.apf[1];
            self.stlpci[0] = x;
            self.d[self.ip + k + 1 + KPMAX - 1] = v;
        }
        self.ip += IDIM;
        if self.icount == 3 {
            self.pitch();
            // Blocks 83 and 84: the pitch tap, and the comb filter from it.
            let (mut sum, mut tmp) = (0.0, 0.0);
            for k in OFF + 1 - NPWSZ..=OFF {
                let p = self.st[k - self.kp];
                sum += p * p;
                tmp += self.st[k] * p;
            }
            let mut ptap = if sum == 0.0 { 0.0 } else { tmp / sum };
            ptap = ptap.min(1.0);
            if ptap < PPFTH {
                ptap = 0.0;
            }
            self.b = PPFZCF * ptap;
            self.gl = 1.0 / (1.0 + self.b);
        }
        // Block 71: the long-term (pitch) postfilter, then the buffer shift.
        let mut temp = [0.0; IDIM];
        for (k, t) in temp.iter_mut().enumerate() {
            *t = self.gl * (self.st[OFF + 1 + k] + self.b * self.st[OFF + 1 + k - self.kp]);
        }
        self.st.copy_within(IDIM.., 0);
        // Block 72: the short-term pole-zero postfilter and tilt compensation.
        for t in temp.iter_mut() {
            let x = *t;
            let mut v = x;
            for j in (1..10).rev() {
                v += self.stpffir[j] * self.az[j + 1];
                self.stpffir[j] = self.stpffir[j - 1];
            }
            v += self.stpffir[0] * self.az[1];
            self.stpffir[0] = x;
            for j in (1..10).rev() {
                v -= self.stpfiir[j] * self.ap[j + 1];
                self.stpfiir[j] = self.stpfiir[j - 1];
            }
            v -= self.stpfiir[0] * self.ap[1];
            self.stpfiir[0] = v;
            *t = v + self.stpfiir[1] * self.tiltz;
        }
        // Blocks 73–77: AGC to the unfiltered vector's level.
        let unfil: f64 = st.iter().map(|v| v.abs()).sum();
        let fil: f64 = temp.iter().map(|v| v.abs()).sum();
        let scale = if fil > 1.0 { unfil / fil } else { 1.0 };
        for t in temp.iter_mut() {
            self.scalefil = AGCFAC * self.scalefil + (1.0 - AGCFAC) * scale;
            *t *= self.scalefil;
        }
        temp
    }

    /// Block 82: the pitch period, from the LPC residual decimated 4:1 then
    /// refined, kept near the last period unless a multiple is far better.
    fn pitch(&mut self) {
        // D(k) is d[k + 139]; DEC(n) is dec[n + 34].
        const DO: usize = KPMAX - 1;
        const EO: usize = 34;
        for k in NPWSZ - NFRSZ + 1..=NPWSZ {
            let l = &mut self.stlpf;
            let tmp = self.d[k + DO] - l[0] * AL[0] - l[1] * AL[1] - l[2] * AL[2];
            if k % 4 == 0 {
                self.dec[k / 4 + EO] = tmp * BL[0] + l[0] * BL[1] + l[1] * BL[2] + l[2] * BL[3];
            }
            l[2] = l[1];
            l[1] = l[0];
            l[0] = tmp;
        }
        let mut cormax = f64::MIN;
        let mut kmax = KPMIN / 4;
        for j in KPMIN / 4..=KPMAX / 4 {
            let tmp: f64 = (1..=NPWSZ / 4)
                .map(|n| self.dec[n + EO] * self.dec[n + EO - j])
                .sum();
            if tmp > cormax {
                cormax = tmp;
                kmax = j;
            }
        }
        self.dec.copy_within(IDIM.., 0);
        let d = &self.d;
        let corr = |j: usize| -> f64 { (1..=NPWSZ).map(|k| d[k + DO] * d[k + DO - j]).sum() };
        let energy = |j: usize| -> f64 { (1..=NPWSZ).map(|k| d[k + DO - j] * d[k + DO - j]).sum() };
        let mut cormax = f64::MIN;
        let mut kp = KPMIN;
        for j in (4 * kmax - 3).max(KPMIN)..=(4 * kmax + 3).min(KPMAX) {
            let tmp = corr(j);
            if tmp > cormax {
                cormax = tmp;
                kp = j;
            }
        }
        // A lag well above the last period may be a multiple of it: look
        // near the last one and take it if its tap holds up (eq. 4-11).
        let m2 = self.kp1 + KPDELTA;
        if kp > m2 {
            let mut cmax = f64::MIN;
            let mut kptmp = KPMIN;
            for j in self.kp1.saturating_sub(KPDELTA).max(KPMIN)..=m2 {
                let tmp = corr(j);
                if tmp > cmax {
                    cmax = tmp;
                    kptmp = j;
                }
            }
            let (sum, tmp) = (energy(kp), energy(kptmp));
            let tap = if sum == 0.0 {
                0.0
            } else {
                (cormax / sum).clamp(0.0, 1.0)
            };
            let tap1 = if tmp == 0.0 {
                0.0
            } else {
                (cmax / tmp).clamp(0.0, 1.0)
            };
            if tap1 > TAPTH * tap {
                kp = kptmp;
            }
        }
        self.kp = kp;
        self.kp1 = kp;
        self.d.copy_within(NFRSZ.., 0);
    }
}

/// Codewords judged at a time (40 ms): two sync periods.
const BLOCK: usize = 64;
/// Codewords from one sync bit to the next (20 ms).
const SYNC_PERIOD: usize = 32;

/// A bit the carrier does not carry (the Q-Flex multiplex loses one of
/// every 64): [`Framer`] takes it as unknown.
pub const UNCARRIED: u8 = 2;

/// Sync bits a block of speech may have unknown: one bit in 64 lost puts
/// about one in a block's 64 sync places.
const MAX_UNKNOWN_SYNC: usize = 3;

/// Codewords of a block of silence fill that may break its pattern (see
/// `Framer::fill_at`).
const FILL_BROKEN: usize = 8;

/// Share of a block's bits equal to those 20 ms (32 codewords) on beyond
/// which it is not speech (see `Framer::repeats`).
const REPEATS: f32 = 0.9;

/// G.728 codewords out of a bit stream framed by a sync bit in place of
/// every codeword's bit 9 (see the module notes), decoded. Bits in (0, 1
/// or [`UNCARRIED`]); speech out 40 ms at a time, five samples a codeword
/// while there is speech, nothing in silence.
pub struct Framer {
    raw: Vec<u8>,
    /// Where codewords start in `raw` (at their sync bits), once found.
    phase: Option<usize>,
    decoder: Decoder,
    /// The last 40 ms held speech (codewords with their sync bits).
    pub talking: bool,
    /// 40 ms blocks of speech so far.
    pub talk_blocks: u64,
    /// 40 ms blocks of the equipment's silence fill so far: G.728 is here
    /// even before anyone speaks.
    pub fill_blocks: u64,
    /// 40 ms blocks looked at, either or neither.
    pub blocks: u64,
    /// While decoding, give a block that is not speech as silence (zeros)
    /// rather than nothing: one sample per two bits in, always, so that
    /// lanes decoded side by side stay in step and can be mixed.
    pub timeline: bool,
}

impl Default for Framer {
    fn default() -> Self {
        Self::new()
    }
}

impl Framer {
    pub fn new() -> Self {
        Self {
            raw: Vec::with_capacity(BLOCK * 10 + 32),
            phase: None,
            decoder: Decoder::new(),
            talking: false,
            talk_blocks: 0,
            fill_blocks: 0,
            blocks: 0,
            timeline: false,
        }
    }

    /// One bit in; with `decode`, speech out (±1, 8 kHz).
    pub fn push(&mut self, bit: u8, decode: bool, out: &mut Vec<f32>) {
        self.raw.push(bit);
        if self.raw.len() >= BLOCK * 10 + 10 {
            self.block(decode, out);
        }
    }

    /// Is this a block of speech with codewords starting at bit `p`: the
    /// sync bits all alike but at two codewords 32 apart (or one, the
    /// other not carried)? Then: whether the line is inverted (the sync
    /// bit then reads 1 but once in 32 — shape indices 64–127, as §3.11
    /// would have it) and which codeword (0..32) carries the odd sync bit.
    fn sync_at(&self, p: usize) -> Option<(u8, usize)> {
        let s = |i: usize| self.raw[p + 10 * i];
        let known = (0..BLOCK).filter(|&i| s(i) != UNCARRIED).count();
        // A carrier that loses one bit in 64 (the Q-Flex multiplex) leaves
        // a sync bit or two unknown in a block; more, and the block is a
        // wrong reading of the channels, whose unknown bits would pass the
        // test below as easily as a sync bit's.
        if known + MAX_UNKNOWN_SYNC < BLOCK {
            return None;
        }
        let ones = (0..BLOCK).filter(|&i| s(i) == 1).count();
        let majority = u8::from(ones * 2 > known);
        let odd: Vec<usize> = (0..BLOCK)
            .filter(|&i| s(i) != UNCARRIED && s(i) != majority)
            .collect();
        let o = *odd.first()? % SYNC_PERIOD;
        // Both sync slots odd, or one of them not carried; nothing else.
        let slot_ok = |i: usize| s(i) != majority;
        let fits = odd.len() <= 2
            && odd.iter().all(|&i| i % SYNC_PERIOD == o)
            && slot_ok(o)
            && slot_ok(o + SYNC_PERIOD);
        fits.then_some((1 - majority, o))
    }

    /// Is this a block of silence fill with codewords starting at bit `p`:
    /// the sync bit and the five below it alike in every codeword, the low
    /// four bits counting one up a codeword (down on an inverted line)?
    /// A few codewords may break the pattern, and a few steps the count:
    /// the Q-Flex's equipment keeps its sync marker (once in 32 codewords)
    /// going through the fill, and its stream, put together from four TDM
    /// channels, has the odd bit wrong. Speech is nowhere near: about 60
    /// codewords of 64 break the pattern, and 1 step in 16 fits by chance.
    fn fill_at(&self, p: usize) -> bool {
        let unit = |i: usize| &self.raw[p + 10 * i..p + 10 * i + 10];
        let ones = (0..BLOCK)
            .flat_map(|i| unit(i)[..6].iter().copied())
            .filter(|&b| b == 1)
            .count();
        let zeros = (0..BLOCK)
            .flat_map(|i| unit(i)[..6].iter().copied())
            .filter(|&b| b == 0)
            .count();
        let v = u8::from(ones > zeros);
        let broken = (0..BLOCK)
            .filter(|&i| unit(i)[..6].iter().any(|&b| b != v && b != UNCARRIED))
            .count();
        if broken > FILL_BROKEN {
            return false;
        }
        // The count, where all four of its bits are carried.
        let count = |i: usize| {
            let c = &unit(i)[6..];
            (!c.contains(&UNCARRIED)).then(|| c.iter().fold(0u8, |a, &b| a << 1 | b))
        };
        let step = if v == 0 { 1 } else { 15 };
        let (mut steps, mut ok) = (0, 0);
        for i in 1..BLOCK {
            if let (Some(a), Some(b)) = (count(i - 1), count(i)) {
                steps += 1;
                ok += usize::from(b.wrapping_sub(a) & 15 == step);
            }
        }
        steps >= BLOCK / 2 && ok * 4 >= steps * 3
    }

    /// Does this block repeat itself 20 ms on? Then it is the silence fill
    /// however it reads — its 16-codeword count cycles twice in 20 ms — and
    /// never speech, whose codewords do not repeat. A Q-Flex stream put
    /// together a bit askew (the ASA204 ML-NBK site: two stray bits in every
    /// 64, from the channels' other halves) breaks the fill's pattern, and
    /// its stray bits, recurring every 320, land on one sync place in 32 —
    /// just as speech's sync marker does: it passed for speech, and was
    /// played as full-scale noise.
    fn repeats(&self) -> bool {
        let half = BLOCK * 10 / 2;
        let (mut known, mut same) = (0u32, 0u32);
        for i in 0..half {
            let (a, b) = (self.raw[i], self.raw[i + half]);
            if a != UNCARRIED && b != UNCARRIED {
                known += 1;
                same += u32::from(a == b);
            }
        }
        known > 0 && same as f32 >= REPEATS * known as f32
    }

    fn block(&mut self, decode: bool, out: &mut Vec<f32>) {
        // The silence fill first: read a bit off its phase, the Q-Flex's
        // fill (which keeps its sync marker) passes for speech's sync, and
        // would be decoded as loud noise.
        let p0 = self.phase.unwrap_or(0);
        let fill = (0..10).map(|k| (p0 + k) % 10).find(|&p| self.fill_at(p));
        // Rust note: `or_else` only searches the other phases when the
        // locked one fails.
        let found = match fill {
            Some(_) => None,
            None if self.repeats() => None,
            None => self
                .phase
                .and_then(|p| self.sync_at(p).map(|s| (p, s)))
                .or_else(|| (0..10).find_map(|p| self.sync_at(p).map(|s| (p, s)))),
        };
        self.talking = found.is_some();
        self.blocks += 1;
        if let Some(p) = fill {
            self.phase = Some(p);
            self.fill_blocks += 1;
        }
        if let Some((p, (invert, odd))) = found {
            self.phase = Some(p);
            self.talk_blocks += 1;
            if decode {
                let mut v = [0.0f32; IDIM];
                for i in 0..BLOCK {
                    let sync_end = i % SYNC_PERIOD == odd;
                    let cw =
                        self.raw[p + 10 * i..][..10]
                            .iter()
                            .enumerate()
                            .fold(0u16, |c, (k, &b)| {
                                // A bit not carried: the sync bit's known value,
                                // else 0 (the commoner value of most fields).
                                let v = match (b, k) {
                                    (UNCARRIED, 0) => u8::from(!sync_end),
                                    (UNCARRIED, _) => 0,
                                    _ => b ^ invert,
                                };
                                c << 1 | u16::from(v)
                            });
                    self.decoder.decode(cw, &mut v);
                    out.extend_from_slice(&v);
                    // The odd sync bit ends an adaptation cycle (§3.11).
                    if sync_end {
                        self.decoder.start_cycle();
                    }
                }
            }
        } else if decode && self.timeline {
            out.resize(out.len() + BLOCK * IDIM, 0.0);
        }
        self.raw.drain(..BLOCK * 10);
    }
}

/// A G.728 channel on two bits of each octet of a 64 kbit/s timeslot (the
/// CDM-600L's D&I voice): octets in, one sample per octet out while there
/// is speech.
pub struct Lane {
    /// The two bits carrying it, as masks, first-sent first (G.704 bit 2
    /// is `0x40`).
    pub bits: [u8; 2],
    pub framer: Framer,
}

impl Lane {
    /// The channel on G.704 bits `first` and `first + 1` (1..=7).
    pub fn new(first: u8) -> Self {
        let m = 0x80u8 >> (first.clamp(1, 7) - 1);
        Self {
            bits: [m, m >> 1],
            framer: Framer::new(),
        }
    }

    /// The timeslot's octets in; with `decode`, speech out (±1, 8 kHz).
    pub fn push(&mut self, octets: &[u8], decode: bool, out: &mut Vec<f32>) {
        for &o in octets {
            for m in self.bits {
                self.framer.push(u8::from(o & m != 0), decode, out);
            }
        }
    }

    /// The last 40 ms held speech.
    pub fn talking(&self) -> bool {
        self.framer.talking
    }

    /// The first of its two bits (G.704 numbering, 1..=7).
    pub fn first_bit(&self) -> u8 {
        self.bits[0].leading_zeros() as u8 + 1
    }

    /// Its two bits as one mask.
    pub fn mask(&self) -> u8 {
        self.bits[0] | self.bits[1]
    }
}

/// Where G.728 channels may sit in a timeslot whose changing bits are
/// `mask`: the first bit of each pair to try. Each run of changing bits is
/// read in pairs from its first bit — the CDM-600L's timeslot is one run,
/// bits 2–3; the PH trunk's channel 18h one of six, bits 3–8, three
/// channels. Read from a run's second bit, a pair straddles two channels,
/// and one of them speaking makes it look like speech too (that channel's
/// sync bit lands on the straddling pair's 10-bit grid all the same): seen
/// on bits 6–7 there. So an even run is only read from its start; an odd
/// one has no telling which end is whole, and both readings are tried.
fn lane_starts(mask: u8) -> Vec<u8> {
    let on = |b: u8| (1..=8).contains(&b) && mask & (0x80 >> (b - 1)) != 0;
    let mut starts = Vec::new();
    let mut b = 1u8;
    while b <= 8 {
        if on(b) {
            let first = b;
            while on(b + 1) {
                b += 1;
            }
            let step = if (b - first) % 2 == 1 { 2 } else { 1 };
            // Rust note: `step_by` takes every `step`-th of the range.
            starts.extend((first..b).step_by(step));
        }
        b += 1;
    }
    starts
}

/// A [`Lane`] being tried, or found, in a [`Voice`].
struct Candidate {
    lane: Lane,
    /// It has carried G.728: speech, or the silence fill.
    found: bool,
    /// Its decoded samples not yet mixed (found lanes only).
    queue: std::collections::VecDeque<f32>,
}

/// G.728 voice somewhere in a timeslot's sub-rate bits: a [`Lane`] on each
/// pair of bits that may carry a channel (see [`lane_starts`]). Every lane
/// that speaks or sends the silence fill is kept — one timeslot can carry
/// several channels, each a radio — and they are decoded side by side and
/// mixed: those the caller lets be heard ([`Voice::set_heard`]), so that a
/// voice floor can give one talker at a time within a timeslot as it does
/// across carriers (the ML–PH trunk's channel 18h had bits 3–4 and 7–8
/// talking at once).
pub struct Voice {
    /// The changing bits ([`crate::e1::Coding::SubRate`]'s mask).
    pub mask: u8,
    lanes: Vec<Candidate>,
    scratch: Vec<f32>,
    /// The bits whose lanes are mixed (all, unless set).
    heard: u8,
}

impl Voice {
    pub fn new(mask: u8) -> Self {
        let mut v = Self {
            mask: 0,
            lanes: Vec::new(),
            scratch: Vec::new(),
            heard: 0xFF,
        };
        v.widen(mask);
        v
    }

    /// More bits seen changing: lanes to try in them too. The lanes found
    /// stay; those still being tried are replaced by `lane_starts` of the
    /// wider mask (a run grown by a channel starting up reads differently).
    pub fn widen(&mut self, mask: u8) {
        let mask = self.mask | mask;
        if mask == self.mask && !self.lanes.is_empty() {
            return;
        }
        self.mask = mask;
        let taken = self.found_bits();
        // Rust note: `retain` drops in place the elements the test fails.
        self.lanes.retain(|c| c.found);
        for first in lane_starts(mask) {
            let lane = Lane::new(first);
            if lane.mask() & taken == 0 {
                self.lanes.push(Candidate {
                    lane,
                    found: false,
                    queue: Default::default(),
                });
            }
        }
        self.lanes.sort_by_key(|c| c.lane.first_bit());
    }

    /// The bits of the lanes found.
    fn found_bits(&self) -> u8 {
        self.lanes
            .iter()
            .filter(|c| c.found)
            .fold(0, |m, c| m | c.lane.mask())
    }

    /// The timeslot's octets in; with `decode`, the found lanes' speech out,
    /// mixed: a sample an octet once a lane is found (silence as zeros),
    /// nothing before.
    pub fn push(&mut self, mut octets: &[u8], decode: bool, out: &mut Vec<f32>) {
        // A block's worth of octets at a time: a lane finishes at most one
        // block in it, so one found in it decodes from the next on, in step
        // with the others.
        while !octets.is_empty() {
            let n = octets.len().min(BLOCK * IDIM);
            for c in &mut self.lanes {
                self.scratch.clear();
                c.lane
                    .push(&octets[..n], decode && c.found, &mut self.scratch);
                c.queue.extend(self.scratch.drain(..));
            }
            octets = &octets[n..];
            // Speech, or the silence fill: either way the lane is G.728 —
            // unless it shares a bit with one found already.
            for i in 0..self.lanes.len() {
                let f = &self.lanes[i].lane.framer;
                if !self.lanes[i].found
                    && f.talk_blocks + f.fill_blocks > 0
                    && self.lanes[i].lane.mask() & self.found_bits() == 0
                {
                    self.lanes[i].found = true;
                    self.lanes[i].lane.framer.timeline = true;
                }
            }
            if decode {
                self.mix(out);
            } else {
                for c in &mut self.lanes {
                    c.queue.clear();
                }
            }
        }
    }

    /// Mix only the lanes on these bits (masks as [`Lane::bits`]; a lane is
    /// heard when either of its bits is): the voice floor's choice. From
    /// the next block decoded on.
    pub fn set_heard(&mut self, bits: u8) {
        self.heard = bits;
    }

    /// As much of the found lanes' speech as all of them have, the heard
    /// ones' summed (the others' dropped, keeping all in step).
    fn mix(&mut self, out: &mut Vec<f32>) {
        let n = self
            .lanes
            .iter()
            .filter(|c| c.found)
            .map(|c| c.queue.len())
            .min()
            .unwrap_or(0);
        let start = out.len();
        out.resize(start + n, 0.0);
        let heard = self.heard;
        for c in self.lanes.iter_mut().filter(|c| c.found) {
            let on = c.lane.mask() & heard != 0;
            for (o, x) in out[start..].iter_mut().zip(c.queue.drain(..n)) {
                if on {
                    *o += x;
                }
            }
        }
    }

    /// The lanes carrying G.728, by bit position.
    pub fn lanes(&self) -> impl Iterator<Item = &Lane> {
        self.lanes.iter().filter(|c| c.found).map(|c| &c.lane)
    }

    /// G.728 has been found here.
    pub fn found(&self) -> bool {
        self.lanes.iter().any(|c| c.found)
    }

    /// A lane spoke in the last 40 ms.
    pub fn talking(&self) -> bool {
        self.lanes().any(Lane::talking)
    }

    /// A lane has spoken at all.
    pub fn spoken(&self) -> bool {
        self.lanes().any(|l| l.framer.talk_blocks > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Codewords (random, deterministic) with §3.11 sync bits in bit 9:
    /// 1, but 0 in every 32nd.
    fn codewords(n: usize, seed: u32) -> Vec<u16> {
        let mut x = seed;
        (0..n)
            .map(|i| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let sync = u16::from(i % SYNC_PERIOD != SYNC_PERIOD - 1);
                (x >> 16) as u16 & 0x1FF | sync << 9
            })
            .collect()
    }

    /// Codewords (inverted, MSB first) and silence fill on bits 2–3 of each
    /// octet, the other bits busy, as on the CDM-600L's timeslot 1.
    fn timeslot(units: &[u16]) -> Vec<u8> {
        let bits: Vec<u8> = units
            .iter()
            .flat_map(|&w| (0..10).rev().map(move |k| u8::from(w >> k & 1 == 0)))
            .collect();
        bits.chunks(2)
            .enumerate()
            .map(|(i, b)| (b[0] << 6 | b[1] << 5) | (i as u8 & 0x9F))
            .collect()
    }

    /// The silence fill read a bit askew, as from a Q-Flex stream put
    /// together with a window one position off (the ASA204 ML-NBK site):
    /// two stray bits in every 64, recurring every 320 — so on one sync
    /// place in 32, as speech's sync marker is. It broke the fill's pattern
    /// and passed for speech; it repeats every 20 ms, so it is not.
    #[test]
    fn fill_read_askew_is_not_speech() {
        // Fill as the line carries it (inverted): 111111, then a count
        // stepping down a codeword.
        let mut bits: Vec<u8> = (0..BLOCK * 30)
            .flat_map(|u| {
                let c = 15 - (u % 16) as u8;
                [1, 1, 1, 1, 1, 1, c >> 3 & 1, c >> 2 & 1, c >> 1 & 1, c & 1]
            })
            .collect();
        let mut askew = bits.clone();
        for (i, b) in askew.iter_mut().enumerate() {
            if i % 64 == 0 || i % 64 == 37 {
                *b ^= 1;
            }
        }
        let run = |bits: &[u8]| {
            let mut f = Framer::new();
            for &b in bits {
                f.push(b, false, &mut Vec::new());
            }
            (f.talk_blocks, f.fill_blocks)
        };
        assert_eq!(run(&askew).0, 0, "askew fill taken for speech");
        // The fill read right is fill; speech is still speech.
        assert!(run(&bits).1 > 20);
        bits = codewords(BLOCK * 30, 3)
            .iter()
            .flat_map(|&w| (0..10).rev().map(move |k| u8::from(w >> k & 1 == 0)))
            .collect();
        assert!(run(&bits).0 > 25);
    }

    #[test]
    fn a_lane_decodes_what_the_decoder_does() {
        let cw = codewords(BLOCK * 6, 7);
        let mut want = Vec::new();
        let mut dec = Decoder::new();
        let mut v = [0.0f32; IDIM];
        for (i, &c) in cw.iter().enumerate() {
            dec.decode(c, &mut v);
            want.extend_from_slice(&v);
            if i % SYNC_PERIOD == SYNC_PERIOD - 1 {
                dec.start_cycle();
            }
        }
        let mut voice = Voice::new(0x60);
        let mut got = Vec::new();
        voice.push(&timeslot(&cw), true, &mut got);
        let lane = voice.lanes().next().expect("found");
        assert_eq!(lane.bits, [0x40, 0x20]);
        assert!(lane.talking());
        // The first block finds the lane; the next ones decode from a fresh
        // decoder, so line up against one started there.
        let mut dec = Decoder::new();
        let mut want = Vec::new();
        for (i, &c) in cw.iter().enumerate().skip(BLOCK) {
            dec.decode(c, &mut v);
            want.extend_from_slice(&v);
            if i % SYNC_PERIOD == SYNC_PERIOD - 1 {
                dec.start_cycle();
            }
        }
        assert!(!got.is_empty());
        assert_eq!(&got[..], &want[..got.len()]);
    }

    #[test]
    fn the_silence_fill_is_not_speech() {
        // CDM-600L silence: raw units 1111 cccc 11, the count stepping down;
        // as codewords (inverted, from the sync bit) 0..15 counting up.
        let fill: Vec<u16> = (0..BLOCK * 4).map(|i| (i % 16) as u16).collect();
        let mut voice = Voice::new(0x60);
        let mut out = Vec::new();
        voice.push(&timeslot(&fill), true, &mut out);
        // The fill says G.728 is there, but nothing is decoded from it:
        // silence.
        assert!(voice.found() && !voice.talking());
        assert!(!out.is_empty() && out.iter().all(|&x| x == 0.0));
        // Then speech: found, and decoded.
        voice.push(&timeslot(&codewords(BLOCK * 3, 3)), true, &mut out);
        assert!(voice.talking());
        // Then silence again: silence out, not talking.
        let n = out.len();
        voice.push(&timeslot(&fill), true, &mut out);
        assert!(!voice.talking());
        assert!(
            out[n + BLOCK * IDIM..].iter().all(|&x| x == 0.0),
            "speech at most in the block in flight"
        );
    }

    /// Two channels of one timeslot talking at once (bits 3–4 and 7–8, as
    /// on the ML–PH trunk's channel 18h): both mixed by default; with one
    /// heard, the mix is that one alone.
    #[test]
    fn one_of_two_talkers_heard() {
        let line = |units: &[u16]| -> Vec<u8> {
            units
                .iter()
                .flat_map(|&w| (0..10).rev().map(move |k| u8::from(w >> k & 1 == 0)))
                .collect()
        };
        let (a, c) = (
            line(&codewords(BLOCK * 6, 5)),
            line(&codewords(BLOCK * 6, 11)),
        );
        let octets: Vec<u8> = (0..a.len() / 2)
            .map(|i| {
                let pair = |s: &[u8], first: u8| (s[2 * i] << 1 | s[2 * i + 1]) << (7 - first);
                pair(&a, 3) | pair(&c, 7)
            })
            .collect();
        let run = |heard: Option<u8>, mask: u8| {
            let mut v = Voice::new(mask);
            if let Some(h) = heard {
                v.set_heard(h);
            }
            let mut out = Vec::new();
            v.push(&octets, true, &mut out);
            (v, out)
        };
        let (both, mixed) = run(None, 0x33);
        let talking: Vec<(u8, bool)> = both.lanes().map(|l| (l.first_bit(), l.talking())).collect();
        assert_eq!(talking, [(3, true), (7, true)]);
        // Bits 7–8 heard: as if it were alone.
        let (_, only_78) = run(Some(0x03), 0x33);
        let (_, alone) = run(None, 0x03);
        assert!(alone.iter().any(|&x| x != 0.0));
        assert_eq!(only_78, alone);
        assert_ne!(mixed, alone);
        // Neither heard: silence, still in step.
        let (_, none) = run(Some(0), 0x33);
        assert_eq!(none.len(), mixed.len());
        assert!(none.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn lanes_are_read_from_the_start_of_each_run() {
        assert_eq!(lane_starts(0x60), [2]); // the CDM-600L: bits 2–3
        assert_eq!(lane_starts(0x3F), [3, 5, 7]); // PH trunk 18h: 3–8
        assert_eq!(lane_starts(0x03), [7]);
        assert_eq!(lane_starts(0x63), [2, 7]);
        // An odd run: either end may be the whole pair.
        assert_eq!(lane_starts(0x70), [2, 3]);
        assert!(lane_starts(0x10).is_empty());
    }

    /// The PH trunk's channel 18h: three G.728 channels in one timeslot,
    /// bits 3–4 and 5–6 sending the silence fill, 7–8 speech; bits 1–2 a
    /// fixed pattern repeating each millisecond.
    #[test]
    fn three_channels_in_one_timeslot() {
        let fill = |from: usize| -> Vec<u16> {
            (0..BLOCK * 6).map(|i| ((i + from) % 16) as u16).collect()
        };
        let speech = codewords(BLOCK * 6, 11);
        let line = |units: &[u16]| -> Vec<u8> {
            units
                .iter()
                .flat_map(|&w| (0..10).rev().map(move |k| u8::from(w >> k & 1 == 0)))
                .collect()
        };
        let (a, b, c) = (line(&fill(0)), line(&fill(5)), line(&speech));
        let octets: Vec<u8> = (0..a.len() / 2)
            .map(|i| {
                let pair = |s: &[u8], first: u8| (s[2 * i] << 1 | s[2 * i + 1]) << (7 - first);
                [0xC0, 0x80, 0x40, 0xC0, 0x00, 0xC0, 0x80, 0xC0][i % 8]
                    | pair(&a, 3)
                    | pair(&b, 5)
                    | pair(&c, 7)
            })
            .collect();
        let mut voice = Voice::new(0x3F);
        let mut got = Vec::new();
        voice.push(&octets, true, &mut got);
        let lanes: Vec<(u8, bool)> = voice
            .lanes()
            .map(|l| (l.first_bit(), l.talking()))
            .collect();
        assert_eq!(lanes, [(3, false), (5, false), (7, true)]);
        assert!(voice.talking() && voice.spoken());
        // The mix is the speaking channel's speech, as if it were alone,
        // a sample an octet once found.
        let mut alone = Voice::new(0x03);
        let mut want = Vec::new();
        alone.push(&octets, true, &mut want);
        assert!(want.iter().any(|&x| x != 0.0));
        assert_eq!(got, want);
        assert!(
            got.len() + 2 * BLOCK * IDIM >= octets.len(),
            "{}",
            got.len()
        );
    }

    /// The Q-Flex's silence fill as it comes out of its four TDM channels:
    /// inverted units 111111cccc counting down, a sync marker in one
    /// codeword of 32, and the odd bit wrong. Read from any bit, the marker
    /// looks like speech's sync; it must still be taken for fill (it once
    /// played as bursts of loud noise between calls).
    #[test]
    fn the_qflex_fill_with_its_marker_is_not_speech() {
        let mut x = 5u32;
        let mut bits = Vec::new();
        for i in 0..BLOCK * 12 {
            let mut unit = [1u8; 10];
            let c = 15 - (i % 16) as u8;
            for k in 0..4 {
                unit[6 + k] = c >> (3 - k) & 1;
            }
            if i % SYNC_PERIOD == 7 {
                unit[1] = 0;
            }
            bits.extend_from_slice(&unit);
        }
        for b in bits.iter_mut() {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            if x >> 24 == 0 {
                *b ^= 1; // about one bit in 256 wrong
            }
        }
        for start in 0..10 {
            let mut f = Framer::new();
            let mut out = Vec::new();
            for &b in &bits[start..] {
                f.push(b, true, &mut out);
            }
            assert_eq!(f.talk_blocks, 0, "read from bit {start}");
            assert!(out.is_empty());
            assert!(
                f.fill_blocks >= 8,
                "read from bit {start}: {}",
                f.fill_blocks
            );
        }
    }

    #[test]
    fn steady_or_random_bits_are_not_fill() {
        let mut out = Vec::new();
        // A lane stuck at one value: no count.
        let mut f = Framer::new();
        for _ in 0..BLOCK * 10 * 3 {
            f.push(1, false, &mut out);
        }
        assert_eq!(f.fill_blocks, 0);
        // Random bits.
        let mut x = 99u32;
        let mut f = Framer::new();
        for _ in 0..BLOCK * 10 * 20 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            f.push((x >> 20) as u8 & 1, false, &mut out);
        }
        assert_eq!(f.fill_blocks + f.talk_blocks, 0);
    }

    /// The Recommendation's decoder test vectors (Appendix I: CWn.BIN in,
    /// OUTAn.BIN out with the postfilter off, OUTB4.BIN with it on; 16-bit
    /// little-endian). They are ITU's and stay out of the repo: point
    /// `DECSAT_G728_VECTORS` at the folder and run with `--ignored`.
    #[test]
    #[ignore]
    fn decodes_the_itu_test_vectors() {
        let Ok(dir) = std::env::var("DECSAT_G728_VECTORS") else {
            panic!("set DECSAT_G728_VECTORS to the folder of CW1.BIN …");
        };
        let read = |name: &str| -> Vec<i16> {
            let b = std::fs::read(std::path::Path::new(&dir).join(name)).unwrap();
            b.as_chunks::<2>()
                .0
                .iter()
                .map(|c| i16::from_le_bytes(*c))
                .collect()
        };
        for (cw, out, post) in [
            ("CW1.BIN", "OUTA1.BIN", false),
            ("CW2.BIN", "OUTA2.BIN", false),
            ("CW3.BIN", "OUTA3.BIN", false),
            ("CW4.BIN", "OUTA4.BIN", false),
            ("CW4.BIN", "OUTB4.BIN", true),
            ("CW5.BIN", "OUTA5.BIN", false),
            ("CW6.BIN", "OUTA6.BIN", false),
        ] {
            let mut dec = Decoder::new();
            dec.postfilter = post;
            let want = read(out);
            let mut got = Vec::new();
            let mut v = [0.0f32; IDIM];
            for &c in &read(cw) {
                dec.decode(c as u16, &mut v);
                got.extend(
                    v.iter()
                        .map(|x| (f64::from(*x) * 32768.0).round().clamp(-32768.0, 32767.0)),
                );
            }
            let (mut s, mut e) = (0.0, 0.0);
            for (g, w) in got.iter().zip(&want) {
                s += f64::from(*w) * f64::from(*w);
                e += (g - f64::from(*w)).powi(2);
            }
            let snr = 10.0 * (s / e.max(1e-9)).log10();
            println!("{cw} -> {out}: {} samples, SNR {snr:.1} dB", got.len());
            assert_eq!(got.len(), want.len());
            assert!(snr > 60.0, "{cw} -> {out}: SNR {snr:.1} dB");
        }
    }

    #[test]
    fn gain_codebook_is_annex_b() {
        // Annex B's printed levels.
        for (ig, v) in [0.515625, 0.90234375, 1.579101563, 2.763427734]
            .iter()
            .enumerate()
        {
            assert!((gq(ig) - v).abs() < 1e-8 && (gq(ig + 4) + v).abs() < 1e-8);
        }
    }
}
