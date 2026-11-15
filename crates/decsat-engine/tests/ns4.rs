// made by CasualArclamp
//! A NovelSat NS4 frame through the FEC stage, from the modem's own bits.
//!
//! The frames in `decsat-fec/tests/ns4` were recorded from a NovelSat modem
//! in its PRBS test mode (TS, CCM). Mapped back onto QPSK with a PLHEADER's
//! worth of meaning attached, each has to come out as a BBFRAME whose BCH
//! and BBHEADER CRC check — which also pins NS4's bit-to-symbol mapping,
//! BCH code and baseband scrambling as DVB-S2's.

use decsat_core::Iq;
use decsat_engine::PlFrame;
use decsat_engine::fec::FecDecoder;
use decsat_frame::PlsInfo;

/// A pilot block of 36 symbols follows every 16 slots of 90 (§5.5.3).
const PILOT_AFTER: usize = 16 * 90;
const PILOT_PERIOD: usize = PILOT_AFTER + 36;

/// (NS4 PLS code, K_bch − 80: the data field of a full TS frame, the frame).
fn frames() -> Vec<(u8, u16, &'static str)> {
    include!("../../decsat-fec/tests/ns4/engine_frames.rs").to_vec()
}

fn bits(hex: &str) -> Vec<u8> {
    hex.trim()
        .as_bytes()
        .chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .flat_map(|b| (0..8).map(move |i| (b >> (7 - i)) & 1))
        .collect()
}

#[test]
fn recorded_ns4_frames_decode_to_bbframes() {
    for (plsc, dfl, hex) in frames() {
        let pls = PlsInfo::parse_ns4(plsc);
        assert!(pls.ns4 && pls.has_pilots && !pls.reserved);
        let bits = bits(hex);
        // QPSK as §5.4.1 maps it: first bit on I, second on Q, 0 → positive.
        let a = std::f32::consts::FRAC_1_SQRT_2;
        let mut data = bits
            .chunks(2)
            .map(|b| Iq::new(a * (1.0 - 2.0 * b[0] as f32), a * (1.0 - 2.0 * b[1] as f32)));
        let mut payload = Vec::with_capacity(pls.payload_len as usize);
        for i in 0..pls.payload_len as usize {
            payload.push(if i % PILOT_PERIOD < PILOT_AFTER {
                data.next().unwrap()
            } else {
                Iq::new(a, a)
            });
        }
        assert!(data.next().is_none());
        // A few symbols knocked into the next quadrant, for the LDPC to mend.
        for i in (0..payload.len()).step_by(211) {
            payload[i] = Iq::new(-payload[i].im, payload[i].re);
        }
        let frame = PlFrame {
            pls,
            payload,
            corr: 1.0,
            gain: 1.0,
            noise_var: 0.1,
            after_gap: false,
            vlsnr: None,
            src_s: None,
        };
        let bb = FecDecoder::new().decode(&frame).expect("a known NS4 code");
        assert!(bb.ldpc.converged, "{plsc}: {:?}", bb.ldpc);
        assert_eq!(bb.bch, Ok(0), "{plsc}");
        let h = bb.header.expect("BBHEADER CRC");
        assert_eq!((h.upl, h.dfl, h.sync), (1504, dfl, 0x47), "{plsc}");
    }
}
