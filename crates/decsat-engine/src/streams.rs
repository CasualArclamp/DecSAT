// made by CasualArclamp
//! The input streams of a DVB-S2/S2X carrier: what each ISI carries, at
//! which MODCODs, and how fast.
//!
//! A multistream carrier (MIS, EN 302 307-1 §5.1.6) interleaves BBFRAMEs of
//! several input streams, told apart by MATYPE-2 (the ISI). Each is its own
//! stream — a transport stream, GSE, GSE-HEM or other packets — sliced only
//! across its own BBFRAMEs, so each gets its own deframer and its own stage.
//! Seen live: a VSAT hub's S2X ACM outbound with the user IP as GSE on ISI 1
//! and the network's signalling on ISI 0, always QPSK 1/3, as 188-byte
//! transport packets with the CRC-8 in place of the sync byte, in a stream
//! flagged generic continuous (UPL 1504 bits, SYNC BBh).

use std::collections::BTreeMap;

use decsat_frame::{BbHeader, StreamFormat};

/// MPEG-TS packets in bits (188 bytes): a generic stream with packets this
/// long is read as transport packets.
pub const TS_UPL_BITS: u16 = 188 * 8;

/// What an input stream carries, from its BBHEADERs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamContent {
    /// GSE (generic mode, UPL 0): to IP.
    #[default]
    Gse,
    /// GSE in high-efficiency mode (S2X): sliced packets, to IP.
    GseHem,
    /// An MPEG transport stream (TS mode).
    Ts,
    /// 188-byte packets in a generic stream, read as transport packets
    /// (their sync byte as the BBHEADER's SYNC field gives it).
    TsPackets { sync: u8 },
    /// Other fixed-length packets in a generic stream: counted, not read.
    Packets { bytes: u16, sync: u8 },
}

impl StreamContent {
    /// What a stream whose BBHEADER is `h` carries.
    pub fn of(h: &BbHeader) -> Self {
        match h.format {
            StreamFormat::Transport => StreamContent::Ts,
            StreamFormat::GseHem => StreamContent::GseHem,
            _ if h.upl == 0 => StreamContent::Gse,
            _ if h.upl == TS_UPL_BITS => StreamContent::TsPackets { sync: h.sync },
            _ => StreamContent::Packets {
                bytes: h.upl / 8,
                sync: h.sync,
            },
        }
    }

    /// Goes to the TS stage.
    pub fn is_ts(self) -> bool {
        matches!(self, StreamContent::Ts | StreamContent::TsPackets { .. })
    }

    pub fn label(self) -> String {
        match self {
            StreamContent::Gse => "GSE".into(),
            StreamContent::GseHem => "GSE-HEM".into(),
            StreamContent::Ts => "transport stream".into(),
            StreamContent::TsPackets { sync } => {
                format!("transport packets in a generic stream (sync {sync:02X}h)")
            }
            StreamContent::Packets { bytes, sync } => {
                format!("{bytes}-byte packets (sync {sync:02X}h)")
            }
        }
    }
}

/// One input stream, for display.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamView {
    /// MATYPE-2; 0 for a single stream.
    pub isi: u8,
    /// The carrier has only this stream (SIS).
    pub single: bool,
    pub content: StreamContent,
    /// CCM (one MODCOD) rather than ACM/VCM.
    pub ccm: bool,
    /// Input stream synchronisation (ISSY) is on.
    pub issyi: bool,
    /// Null-packet deletion (TS) — or, in the generic modes, GSE-Lite.
    pub npd: bool,
    /// High-efficiency mode (CRC8_MODE = CRC-8 ⊕ 1).
    pub hem: bool,
    /// Good BBFRAMEs.
    pub frames: u64,
    /// Good frames by MODCOD (DVB-S2 number or S2X PLS code).
    pub modcods: BTreeMap<u8, u64>,
    /// Useful bit rate (DFL) over the last couple of seconds of signal.
    pub bps: f64,
    /// Transport streams: packets, and those whose CRC-8 failed; PIDs seen.
    pub ts_packets: u64,
    pub ts_crc_errors: u64,
    pub ts_pids: usize,
    /// The TS stage shown and given the outputs is this stream's.
    pub ts_primary: bool,
}

impl StreamView {
    /// The MODCOD most of its frames used.
    pub fn main_modcod(&self) -> Option<u8> {
        self.modcods.iter().max_by_key(|e| *e.1).map(|e| *e.0)
    }
}

/// A stream's running counts.
#[derive(Debug, Default)]
pub(crate) struct StreamTally {
    pub view: StreamView,
    /// Rate window: DFL bits and signal seconds.
    win: (f64, f64),
}

impl StreamTally {
    /// One good BBFRAME of this stream.
    pub fn frame(&mut self, h: &BbHeader, modcod: u8) {
        let v = &mut self.view;
        v.isi = if h.single_stream { 0 } else { h.isi };
        v.single = h.single_stream;
        v.content = StreamContent::of(h);
        v.ccm = h.ccm;
        v.issyi = h.issyi;
        v.npd = h.npd;
        v.hem = h.high_efficiency;
        v.frames += 1;
        *v.modcods.entry(modcod).or_default() += 1;
        self.win.0 += h.dfl as f64;
    }

    /// Signal time passes (every frame, of any stream).
    pub fn tick(&mut self, secs: f64) {
        self.win.1 += secs;
        if self.win.1 >= 2.0 || (self.view.bps == 0.0 && self.win.1 > 0.2) {
            self.view.bps = self.win.0 / self.win.1;
            if self.win.1 >= 2.0 {
                self.win = (0.0, 0.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(format: StreamFormat, upl: u16, sync: u8) -> BbHeader {
        BbHeader {
            format,
            single_stream: false,
            ccm: false,
            issyi: false,
            npd: false,
            ro: 2,
            isi: 0,
            upl,
            dfl: 5152,
            sync,
            syncd: 0,
            high_efficiency: false,
        }
    }

    #[test]
    fn content_follows_the_header() {
        use StreamFormat::*;
        assert_eq!(
            StreamContent::of(&header(Transport, 1504, 0x47)),
            StreamContent::Ts
        );
        assert_eq!(
            StreamContent::of(&header(GenericContinuous, 0, 0)),
            StreamContent::Gse
        );
        assert_eq!(
            StreamContent::of(&header(GseHem, 0, 0)),
            StreamContent::GseHem
        );
        // The live outbound's signalling stream.
        let sig = StreamContent::of(&header(GenericContinuous, 1504, 0xBB));
        assert_eq!(sig, StreamContent::TsPackets { sync: 0xBB });
        assert!(sig.is_ts());
        assert_eq!(
            StreamContent::of(&header(GenericPacketized, 96, 0x00)),
            StreamContent::Packets { bytes: 12, sync: 0 }
        );
    }
}
