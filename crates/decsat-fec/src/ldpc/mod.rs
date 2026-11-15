// made by CasualArclamp
//! The LDPC inner code (EN 302 307-1 §5.3.2): encoder and layered decoder.
//!
//! **Structure.** The address tables (Annex B/C) give, for each group of 360
//! information bits, the parity accumulators its first bit feeds; bit `k` of
//! the group feeds `(x + k·q) mod (N−K)` for each listed address `x`, with
//! q = (N−K)/360. Writing a check index as `j = c·q + r` (r < q, c < 360)
//! shows the quasi-cyclic structure the decoder exploits: address `x = s·q + r`
//! connects the group to the 360 checks of *layer* r, bit k going to lane
//! c = (s + k) mod 360 — a cyclic shift by s. The parity part is the
//! accumulator chain `p_j = p_(j−1) ⊕ acc_j`; with parity bits re-indexed the
//! same way it too is a set of identity (and one shift-by-one) blocks.
//!
//! The decoder walks the q layers in turn, each a batch of 360 independent
//! check nodes, normalized min-sum on 8-bit LLRs — the layout of the
//! SIMD decoder in gr-dvbs2rx (Ahmet Inan's, GPL-3), re-expressed so plain
//! Rust loops over 360-lane arrays vectorise.

pub mod tables;
pub mod tables_ns4;
pub mod tables_s2x;

mod decoder;

pub use decoder::{DecodeOutcome, LdpcDecoder};

use tables::Table;

/// Lanes per layer: the code's circulant size (§5.3.2, "M = 360").
pub const LANES: usize = 360;

/// One edge block of a layer: 360 checks joined to a 360-bit group by a
/// cyclic shift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Edge {
    /// Variable group: 0..K/360 information groups, then q parity groups.
    pub group: u32,
    /// Check lane c sees bit (c − shift) mod 360 of the group.
    pub shift: u16,
}

/// How a code's parity bits are tied together and sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParityStyle {
    /// DVB-S2/S2X (§5.3.2): the accumulator chain `p_j = p_(j−1) ⊕ acc_j`,
    /// parity sent in chain order.
    S2,
    /// NovelSat NS4, as measured from a modem's test pattern (there is no
    /// published description): the q parity groups are sent whole, one
    /// after the other (lane order, as DVB-T2's parity interleaver leaves
    /// them), and the chain closes on itself. Check lane j of layer r ties
    /// parity group r to group r−1 in the same lane; layer 1 takes group 0
    /// one lane on, layer 0 takes group q−1 one lane back, and the middle
    /// layer ⌈q/2⌉ takes group 0 as a third parity bit (a weight-3 column,
    /// as in the IEEE 802.16e codes).
    Ns4,
}

/// An LDPC code, expanded from its address table.
pub struct LdpcCode {
    pub n: usize,
    pub k: usize,
    /// (N−K)/360: the number of layers.
    pub q: usize,
    pub style: ParityStyle,
    table: &'static Table,
    /// Edges of every layer, back to back; layer r is
    /// `edges[layer_start[r]..layer_start[r + 1]]`. Information edges first,
    /// then the parity edges (see `decoder` for the one special block).
    pub(crate) edges: Vec<Edge>,
    pub(crate) layer_start: Vec<usize>,
}

impl LdpcCode {
    /// A DVB-S2/S2X code.
    pub fn new(table: &'static Table) -> Self {
        Self::with_style(table, ParityStyle::S2)
    }

    /// A NovelSat NS4 code ([`tables_ns4`]).
    pub fn ns4(table: &'static Table) -> Self {
        Self::with_style(table, ParityStyle::Ns4)
    }

    /// The NS4 layer whose checks take parity group 0 as a third parity bit.
    fn ns4_middle(q: usize) -> usize {
        q.div_ceil(2)
    }

    fn with_style(table: &'static Table, style: ParityStyle) -> Self {
        let (n, k) = (table.n, table.k);
        let q = (n - k) / LANES;
        assert_eq!(q * LANES, n - k);

        let mut per_layer: Vec<Vec<Edge>> = vec![Vec::new(); q];
        for (g, row) in rows(table).enumerate() {
            for &x in row {
                let x = x as usize;
                per_layer[x % q].push(Edge {
                    group: g as u32,
                    shift: (x / q) as u16,
                });
            }
        }
        let info_groups = (k / LANES) as u32;
        let mut edges = Vec::new();
        let mut layer_start = Vec::with_capacity(q + 1);
        for (r, mut layer) in per_layer.into_iter().enumerate() {
            layer_start.push(edges.len());
            if style == ParityStyle::Ns4 {
                let group = |g: usize| info_groups + g as u32;
                layer.push(Edge {
                    group: group(r),
                    shift: 0,
                });
                // `shift` s means lane c sees bit c − s: one lane on is
                // LANES − 1, one lane back is 1.
                layer.push(match r {
                    0 => Edge {
                        group: group(q - 1),
                        shift: 1,
                    },
                    1 => Edge {
                        group: group(0),
                        shift: LANES as u16 - 1,
                    },
                    _ => Edge {
                        group: group(r - 1),
                        shift: 0,
                    },
                });
                if r == Self::ns4_middle(q) {
                    layer.push(Edge {
                        group: group(0),
                        shift: 0,
                    });
                }
                edges.extend(layer);
                continue;
            }
            // p_j: parity group r, same lane.
            layer.push(Edge {
                group: info_groups + r as u32,
                shift: 0,
            });
            // p_(j−1): group r−1, same lane; for layer 0 it is group q−1
            // shifted by one, with no edge at lane 0 (p_(−1) does not exist).
            layer.push(if r > 0 {
                Edge {
                    group: info_groups + r as u32 - 1,
                    shift: 0,
                }
            } else {
                Edge {
                    group: info_groups + q as u32 - 1,
                    shift: 1,
                }
            });
            edges.extend(layer);
        }
        layer_start.push(edges.len());
        LdpcCode {
            n,
            k,
            q,
            style,
            table,
            edges,
            layer_start,
        }
    }

    /// Fill the parity bits of a codeword from its information bits. Both are
    /// bytes, MSB first: `info` is K/8 bytes, `parity` (N−K)/8.
    pub fn encode(&self, info: &[u8], parity: &mut [u8]) {
        assert_eq!(info.len() * 8, self.k);
        assert_eq!(parity.len() * 8, self.n - self.k);
        let nk = self.n - self.k;
        let mut acc = vec![0u8; nk];
        for (g, row) in rows(self.table).enumerate() {
            for b in 0..LANES {
                let m = g * LANES + b;
                if info[m / 8] & (0x80 >> (m % 8)) == 0 {
                    continue;
                }
                for &x in row {
                    acc[(x as usize + b * self.q) % nk] ^= 1;
                }
            }
        }
        if self.style == ParityStyle::Ns4 {
            self.ns4_parity(&mut acc);
        } else {
            // The accumulator chain: p_i = p_i ⊕ p_(i−1).
            for i in 1..nk {
                acc[i] ^= acc[i - 1];
            }
        }
        parity.fill(0);
        for (i, &bit) in acc.iter().enumerate() {
            parity[i / 8] |= bit << (7 - i % 8);
        }
    }

    /// Check a hard-decision codeword (bytes, MSB first, N/8 of them) against
    /// every parity equation, straight from the table — independent of the
    /// decoder's layered layout, so it can check it.
    pub fn is_codeword(&self, cw: &[u8]) -> bool {
        assert_eq!(cw.len() * 8, self.n);
        let nk = self.n - self.k;
        let bit = |i: usize| (cw[i / 8] >> (7 - i % 8)) & 1;
        let mut acc = vec![0u8; nk];
        for (g, row) in rows(self.table).enumerate() {
            for b in 0..LANES {
                let m = g * LANES + b;
                if bit(m) == 1 {
                    for &x in row {
                        acc[(x as usize + b * self.q) % nk] ^= 1;
                    }
                }
            }
        }
        if self.style == ParityStyle::Ns4 {
            let (q, m) = (self.q, Self::ns4_middle(self.q));
            // Parity group g, lane c, as sent.
            let p = |g: usize, c: usize| bit(self.k + g * LANES + c % LANES);
            return (0..nk).all(|j| {
                let (c, r) = (j / q, j % q);
                let other = match r {
                    0 => p(q - 1, c + LANES - 1),
                    1 => p(0, c + 1),
                    _ => p(r - 1, c),
                };
                let third = if r == m { p(0, c) } else { 0 };
                acc[j] ^ p(r, c) ^ other ^ third == 0
            });
        }
        (0..nk).all(|j| {
            let prev = if j > 0 { bit(self.k + j - 1) } else { 0 };
            acc[j] ^ bit(self.k + j) ^ prev == 0
        })
    }

    /// NS4 parity from the check sums `acc` (indexed c·q + r: lane c of
    /// layer r), written back in sending order (group r at r·360).
    ///
    /// Adding layers 1..q of one lane leaves p_(q−1)(c) ⊕ p_0(c+1) ⊕ p_0(c):
    /// every other group meets twice, and the middle layer adds p_0(c).
    /// Layer 0 of the next lane is p_0(c+1) ⊕ p_(q−1)(c). Their sum is
    /// p_0(c) alone, and the rest follow down the chain.
    fn ns4_parity(&self, acc: &mut [u8]) {
        let (q, m) = (self.q, Self::ns4_middle(self.q));
        let s = |r: usize, c: usize| acc[(c % LANES) * q + r];
        let mut p = vec![0u8; q * LANES];
        let first: Vec<u8> = (0..LANES)
            .map(|c| (1..q).fold(s(0, c + 1), |x, r| x ^ s(r, c)))
            .collect();
        p[..LANES].copy_from_slice(&first);
        for c in 0..LANES {
            p[LANES + c] = s(1, c) ^ p[(c + 1) % LANES];
            for r in 2..q {
                let third = if r == m { p[c] } else { 0 };
                p[r * LANES + c] = s(r, c) ^ p[(r - 1) * LANES + c] ^ third;
            }
        }
        acc.copy_from_slice(&p);
    }
}

/// The table's rows: one slice of addresses per group of 360 bits.
fn rows(t: &'static Table) -> impl Iterator<Item = &'static [u16]> {
    let mut at = 0;
    t.runs
        .iter()
        .flat_map(move |&(deg, count)| (0..count).map(move |_| deg).collect::<Vec<_>>())
        .map(move |deg| {
            let row = &t.addr[at..at + deg];
            at += deg;
            row
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{all_s2, all_s2x};

    pub(crate) fn rng(seed: u64) -> impl FnMut() -> u64 {
        let mut s = seed | 1;
        move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    #[test]
    fn encoded_words_satisfy_every_check() {
        let mut next = rng(3);
        for p in all_s2().chain(all_s2x()) {
            let code = LdpcCode::new(p.ldpc_table());
            let info: Vec<u8> = (0..code.k / 8).map(|_| next() as u8).collect();
            let mut cw = info.clone();
            cw.resize(code.n / 8, 0);
            code.encode(&info, &mut cw[code.k / 8..]);
            assert!(code.is_codeword(&cw), "{p:?}");
            // One flipped bit anywhere breaks it.
            let b = (next() % code.n as u64) as usize;
            cw[b / 8] ^= 0x80 >> (b % 8);
            assert!(!code.is_codeword(&cw), "{p:?}");
        }
    }

    #[test]
    fn every_table_address_becomes_one_edge_block() {
        for p in all_s2().chain(all_s2x()) {
            let code = LdpcCode::new(p.ldpc_table());
            let t = p.ldpc_table();
            // Info edges: every address of every row is one 360-edge block;
            // two more per layer for the accumulator chain.
            let info_blocks: usize = t.runs.iter().map(|&(d, c)| d * c).sum();
            assert_eq!(code.edges.len(), info_blocks + 2 * code.q, "{p:?}");
            // (Check degrees are not uniform: short 1/2 spans 4..7.)
            let max = code
                .layer_start
                .windows(2)
                .map(|w| w[1] - w[0])
                .max()
                .unwrap();
            assert!(
                max < 256,
                "{p:?}: layer degree {max} overflows the u8 argmin"
            );
        }
    }
}
