// made by CasualArclamp
//! One FEC block: soft demap, de-interleave, turbo decode, and the two-part
//! acceptance test. Ported from DecBGAN `bgan/decoder.py`, `bgan/pipeline.py`
//! (`verify_block`) and `tools/decode_wav.py` (`decode_block`).

use crate::frame::{self, C32};
use crate::tables::LevelTables;
use crate::turbo::{TurboDecoder, turbo_encode};

/// Parity-agreement threshold. Correct decodes never fell below 0.7585, wrong
/// ones never exceeded 0.6023 over 37281 impossible tries (DecBGAN
/// `decode_wav.ACCEPT_AGREEMENT`, calibrated against a labelled negative
/// set; do not move it without redoing that calibration).
pub const ACCEPT_AGREEMENT: f32 = 0.70;
/// Likelihood-ratio margin in nats (DecBGAN `verify_block`): 1200 kept every
/// correct block and rejected every wrong one of 4960 labelled blocks.
pub const LR_MARGIN_NATS: f64 = 1200.0;
/// DecBGAN decodes with a fixed noise scale of 0.5 on unit-power symbols; the
/// acceptance thresholds were calibrated with LLRs at that scale.
pub const N0: f32 = 0.5;

/// Max-log LLRs for the two bits of one 4-PAM axis. Level index is
/// `hi*2 + mag` with levels (-0.5, -1.5, +0.5, +1.5)/sqrt(2.5), Table 5.3.
#[inline]
fn pam_llr(y: f32, inv_n0: f32) -> [f32; 2] {
    const S: f32 = 0.632_455_5; // 1/sqrt(2.5)
    const LV: [f32; 4] = [-0.5 * S, -1.5 * S, 0.5 * S, 1.5 * S];
    let mut d = [0.0f32; 4];
    for (i, l) in LV.iter().enumerate() {
        d[i] = (y - l) * (y - l);
    }
    // bit "hi" is the index MSB, bit "mag" the LSB
    let hi = d[0].min(d[1]); // hi = 0
    let hi1 = d[2].min(d[3]); // hi = 1
    let mag0 = d[0].min(d[2]); // mag = 0
    let mag1 = d[1].min(d[3]); // mag = 1
    [(hi1 - hi) * inv_n0, (mag1 - mag0) * inv_n0]
}

/// 16-QAM soft demap: 4 LLRs per symbol in CIPM slot order I1, I0, Q1, Q0.
pub fn demap(sym: &[C32], n0: f32, out: &mut Vec<f32>) {
    out.clear();
    out.reserve(sym.len() * 4);
    let inv = 1.0 / n0;
    for s in sym {
        let i = pam_llr(s.re, inv);
        let q = pam_llr(s.im, inv);
        out.extend_from_slice(&[i[0], i[1], q[0], q[1]]);
    }
}

/// Scatter slot LLRs into (Lsys, Lp, Lq), each length DF; punctured positions
/// stay 0, the neutral LLR.
pub fn deinterleave(slot_llr: &[f32], t: &LevelTables, ls: &mut Vec<f32>, lp: &mut Vec<f32>, lq: &mut Vec<f32>) {
    for v in [&mut *ls, &mut *lp, &mut *lq] {
        v.clear();
        v.resize(t.df, 0.0);
    }
    for (i, &l) in slot_llr.iter().enumerate() {
        let (k, pos) = t.slot(i);
        match k {
            0 => ls[pos] = l,
            1 => lp[pos] = l,
            _ => lq[pos] = l,
        }
    }
}

/// Result of decoding one block at one candidate level.
#[derive(Debug, Clone)]
pub struct BlockDecode {
    /// D hard information bits, still scrambled.
    pub bits: Vec<u8>,
    /// Fraction of parity slots whose re-encoded value matches the hard decision.
    pub agreement: f32,
    /// Likelihood-ratio check passed.
    pub lr_ok: bool,
    pub iterations: usize,
}

impl BlockDecode {
    pub fn accepted(&self) -> bool {
        self.agreement > ACCEPT_AGREEMENT && self.lr_ok
    }
}

/// Reusable per-thread scratch for [`decode_block`].
#[derive(Default)]
pub struct BlockScratch {
    turbo: TurboDecoder,
    llr: Vec<f32>,
    ls: Vec<f32>,
    lp: Vec<f32>,
    lq: Vec<f32>,
    bits: Vec<u8>,
    post: Vec<f32>,
}

impl BlockScratch {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Turbo-decode `sym` (one block's `teo` symbols, phase-corrected, unit power)
/// as `t`'s level, then check it by re-encoding.
///
/// The re-encode check: a turbo decoder always outputs *something* and the
/// layer has no CRC, so a decode is trustworthy only if re-encoding it
/// reproduces what the channel saw. Two hypotheses for the pattern of
/// disagreements between re-encoded and hard-decided bits: H_ok (bit i
/// disagrees with probability p_i = 1/(1+e^|LLR_i|)) and H_rand (each with
/// probability 1/2). Accept when log P(obs|H_ok) - log P(obs|H_rand) exceeds
/// [`LR_MARGIN_NATS`]. Necessary but not sufficient, so it is paired with the
/// parity-agreement threshold ([`ACCEPT_AGREEMENT`]); together they gave zero
/// false accepts over 37281 impossible tries in DecBGAN.
pub fn decode_block(sc: &mut BlockScratch, sym: &[C32], t: &LevelTables, niter: usize) -> BlockDecode {
    demap(sym, N0, &mut sc.llr);
    deinterleave(&sc.llr, t, &mut sc.ls, &mut sc.lp, &mut sc.lq);
    let iterations = sc.turbo.decode(&sc.ls, &sc.lp, &sc.lq, &t.perm, niter, 0.75, &mut sc.bits, &mut sc.post);
    let bits = sc.bits[..t.d].to_vec();

    let (df2, p2, q2) = turbo_encode(&bits, &t.perm);
    let streams = [&df2, &p2, &q2];
    let flush_ok = df2 == sc.bits;

    let mut par_total = 0usize;
    let mut par_match = 0usize;
    let mut ll_ok = 0.0f64;
    for (i, &l) in sc.llr.iter().enumerate() {
        let (k, pos) = t.slot(i);
        let reenc = streams[k as usize][pos];
        let hard = u8::from(l < 0.0);
        let dis = reenc != hard;
        if k != 0 {
            par_total += 1;
            par_match += usize::from(!dis);
        }
        // p = implied per-bit error probability, clipped as in the reference
        let a = (l.abs() as f64).min(60.0);
        let p = (1.0 / (1.0 + a.exp())).clamp(1e-12, 1.0 - 1e-12);
        ll_ok += if dis { p.ln() } else { (-p).ln_1p() };
    }
    let ll_rand = sc.llr.len() as f64 * 0.5f64.ln();
    let lr_ok = flush_ok && (ll_ok - ll_rand) > LR_MARGIN_NATS;
    let agreement = if par_total > 0 { par_match as f32 / par_total as f32 } else { 0.0 };
    BlockDecode { bits, agreement, lr_ok, iterations }
}

/// Descramble (clause 5.3.7) and pack information bits MSB first into bytes.
pub fn payload_bytes(bits: &[u8]) -> Vec<u8> {
    let d = frame::scramble(bits);
    d.chunks(8)
        .map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | (b & 1)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demap_signs() {
        // symbol at the 1111 point: all four LLRs negative (bits are 1).
        let s = frame::pilot_symbol();
        let mut out = Vec::new();
        demap(&[s], 0.5, &mut out);
        assert!(out.iter().all(|&l| l < 0.0), "{out:?}");
        // and 0101: hi bits 0 (positive LLR), mag bits 1 (negative)
        let s = frame::qam16()[0b0101];
        demap(&[s], 0.5, &mut out);
        assert!(out[0] > 0.0 && out[1] < 0.0 && out[2] > 0.0 && out[3] < 0.0, "{out:?}");
    }
}
