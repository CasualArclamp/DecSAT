// made by CasualArclamp
//! `decsat scene`: a wideband test capture with a mix of carriers, the kind
//! of span the waterfall, carrier detection and Identify are built for.
//!
//! At 8 MS/s (a typical HackRF setting), cs8 like a HackRF capture:
//!
//! | offset   | what                                             |
//! |----------|--------------------------------------------------|
//! | −2.5 MHz | DVB-S2 CCM, QPSK 1/2 + pilots, 1 MS/s, α 0.20 — IP over GSE |
//! | −0.8 MHz | an unmodulated CW tone                           |
//! | +1.2 MHz | DVB-S2/**S2X ACM**: QPSK 1/2 → 8PSK 25/36 → short 16APSK 26/45 → 32APSK 32/45 (4+8+4+16), 500 kS/s, α 0.25 — MPEG-TS |
//! | +0.25 MHz | TPC 2964 (IESS-315 turbo product code), QPSK, 125 kS/s, α 0.35 — IP over HDLC |
//! | +2.8 MHz | DVB-S (EN 300 421), QPSK 3/4, 250 kS/s, α 0.35 — MPEG-TS |
//!
//! The carriers are real, fully coded signals — TS packets in BBFRAMEs, BCH
//! and LDPC, PLFRAMEs with pilots and scrambling for DVB-S2; Reed–Solomon,
//! interleaving and the punctured convolutional code for DVB-S; HDLC,
//! scrambling and the (64,57) × (46,39) product code for TPC 2964 — and
//! decode end to end.

use std::path::Path;

use anyhow::Result;
use decsat_core::{Iq, RollOff, SampleFormat};
use decsat_gse::Variant;
use decsat_io::IqFileWriter;
use decsat_mod::{FrameSpec, GseBbFramer, PlFramer, Shaper, TsBbFramer};
use decsat_modem::conv::Rate;
use decsat_modem::dvbs::DvbsTx;
use decsat_modem::tpc2964::{Structure, TpcHdlcTx, modulate};

pub const RATE: f64 = 8e6;

/// xorshift64* (test signals only). `pub(crate)`: visible to the other
/// generators in this binary, not outside it.
pub(crate) struct Rng(pub(crate) u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn uniform(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub(crate) fn gauss(&mut self) -> Iq {
        let u1 = self.uniform().max(1e-300);
        let u2 = self.uniform();
        let r = (-2.0 * u1.ln()).sqrt();
        let t = std::f64::consts::TAU * u2;
        Iq::new((r * t.cos()) as f32, (r * t.sin()) as f32) * std::f32::consts::FRAC_1_SQRT_2
    }
}

/// A shaped carrier at `sps` samples/symbol, from `symbols`.
fn shaped(symbols: &[Iq], sps: usize, alpha: f64) -> Vec<Iq> {
    let mut sh = Shaper::new(sps, alpha, 16);
    let mut x = Vec::with_capacity(symbols.len() * sps);
    sh.process(symbols, &mut x);
    x
}

/// Write the scene, `seconds` long.
pub fn write(out: &Path, seconds: f64) -> Result<()> {
    let n = (RATE * seconds) as usize;
    let mut rng = Rng(0x00DE_CD7B);

    // The DVB-S2 carriers are fully coded (TS packets in BBFRAMEs, BCH,
    // LDPC), so the scene decodes end to end; their BBHEADERs announce the
    // roll-off each is shaped with.
    let a = {
        let mut f = PlFramer::new(0, 1)
            .with_source(Box::new(GseBbFramer::new(1, Variant::STANDARD).with_dns()));
        f.set_roll_off(RollOff::R20);
        let syms = f.build_schedule(&[FrameSpec::new(4, false, true)], n / 8 + 64);
        shaped(&syms, 8, 0.20)
    };
    let b = {
        // S2 and S2X MODCODs in one ACM carrier, as S2X allows.
        let sched = [
            FrameSpec::new(4, false, true),
            FrameSpec::s2x(144, true),
            FrameSpec::s2x(240, true),
            FrameSpec::s2x(178, true),
        ];
        let mut f = PlFramer::new(0, 2);
        f.set_roll_off(RollOff::R25);
        let syms = f.build_schedule(&sched, n / 16 + 64);
        shaped(&syms, 16, 0.25)
    };
    // DVB-S: the same test transport stream (tables, MPE radio), rate 3/4.
    let c = {
        let mut ts = TsBbFramer::new(3);
        let mut tx = DvbsTx::new(Rate::R3_4);
        let mut syms = Vec::new();
        while syms.len() < n / 32 + 20_064 {
            tx.packet(&ts.packet(), &mut syms);
        }
        // Mid-stream: skip the interleaver's start-up zeros.
        shaped(&syms[20_000..], 32, 0.35)
    };

    // TPC 2964: a multicast radio and some unicast traffic over Cisco HDLC,
    // the radio paced to real time (a 24 ms MPEG audio frame per packet),
    // idle flags between.
    let d = {
        use decsat_ip::packet::udp_v4;
        use decsat_mod::fec::TestRadio;
        const BAUD: f64 = 125e3;
        let mut tx = TpcHdlcTx::new(Structure::TEST);
        let mut radio = TestRadio::new([239, 10, 20, 30], "DecSAT TPC radio");
        let mut bits = Vec::new();
        let need = 2 * (n / 64 + 64);
        let (mut t, mut next_radio, mut next_data) = (0.0, 0.0, 0.0);
        let mut seq = 0u32;
        while bits.len() < need {
            let cisco = |p: Vec<u8>| {
                let mut f = vec![0x0F, 0x00, 0x08, 0x00];
                f.extend(p);
                f
            };
            if t >= next_radio {
                tx.send(&cisco(radio.next_packet()));
                next_radio += 0.024;
            }
            if t >= next_data {
                seq += 1;
                let payload = format!("DecSAT TPC 2964 test traffic {seq:08}");
                tx.send(&cisco(udp_v4(
                    [10, 1, 1, 2],
                    [10, 1, 1, 1],
                    40000,
                    5001,
                    payload.as_bytes(),
                )));
                next_data += 0.05;
            }
            tx.frame(&mut bits);
            t += decsat_modem::tpc2964::FRAME as f64 / (2.0 * BAUD);
        }
        let mut syms = Vec::new();
        modulate(&bits, decsat_core::Modulation::Qpsk, &mut syms);
        shaped(&syms, 64, 0.35)
    };

    // Mix: each carrier rotated to its offset and scaled to its level.
    let carriers: [(&[Iq], f64, f32); 4] = [
        (&a, -2.5e6, 0.20),
        (&b, 1.2e6, 0.16),
        (&c, 2.8e6, 0.12),
        (&d, 0.25e6, 0.10),
    ];
    let cw_hz = -0.8e6;
    let noise = 0.035f32;

    let mut w = IqFileWriter::create(out, SampleFormat::Cs8)?;
    let mut block = Vec::with_capacity(1 << 16);
    let mut k = 0usize;
    while k < n {
        block.clear();
        let end = (k + (1 << 16)).min(n);
        for i in k..end {
            let t = i as f64 / RATE;
            let mut s = rng.gauss() * noise;
            for &(x, f, g) in &carriers {
                let ph = std::f64::consts::TAU * f * t;
                s += x[i] * Iq::new(ph.cos() as f32, ph.sin() as f32) * g;
            }
            let ph = std::f64::consts::TAU * cw_hz * t;
            s += Iq::new(ph.cos() as f32, ph.sin() as f32) * 0.05;
            block.push(s);
        }
        w.write(&block)?;
        k = end;
    }
    w.finish()?;
    Ok(())
}
