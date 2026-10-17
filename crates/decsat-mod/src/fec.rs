// made by CasualArclamp
//! The transmit FEC chain: BBFRAME → BCH → LDPC → FECFRAME
//! (EN 302 307-1 §5.3), and BBFRAME sources for test signals: TS packets
//! (TS mode) or IP over GSE (generic continuous mode).

use std::collections::HashMap;

use decsat_core::{FecFrame, RollOff};
use decsat_fec::vlsnr::VlsnrCode;
use decsat_fec::{Bch, FecParams, LdpcCode};
use decsat_frame::{BBHEADER_LEN, BbHeader, StreamFormat, bb_scramble, crc8, ro_bits};
use decsat_gse::{GseEncapsulator, Label, Variant};

/// Where a framer's BBFRAMEs come from.
pub trait BbFrameSource: Send {
    /// The next BBFRAME of `bytes` bytes, header included and BB-scrambled.
    fn next_frame(&mut self, bytes: usize) -> Vec<u8>;
    /// The roll-off the BBHEADER announces.
    fn set_roll_off(&mut self, roll_off: RollOff);
    /// CCM (one MODCOD) or ACM/VCM, for MATYPE.
    fn set_ccm(&mut self, ccm: bool);
}

/// BCH and LDPC encoders, built on first use per code.
#[derive(Default)]
pub struct FecEncoder {
    codes: HashMap<(FecFrame, u16, u16), (Bch, LdpcCode)>,
    /// VL-SNR codes by header index.
    vlsnr: HashMap<u8, (Bch, LdpcCode)>,
}

impl FecEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Encode one (scrambled) BBFRAME of `p.k_bch / 8` bytes into a FECFRAME
    /// of `p.n_ldpc / 8` bytes: the frame, its BCH parity, its LDPC parity.
    pub fn encode(&mut self, p: FecParams, bbframe: &[u8]) -> Vec<u8> {
        assert_eq!(bbframe.len() * 8, p.k_bch);
        let (bch, ldpc) = self
            .codes
            .entry((p.frame, p.rate.num, p.rate.den))
            .or_insert_with(|| {
                (
                    Bch::new(p.frame, p.t, p.n_bch),
                    LdpcCode::new(p.ldpc_table()),
                )
            });
        let mut out = vec![0u8; p.n_ldpc / 8];
        out[..bbframe.len()].copy_from_slice(bbframe);
        bch.encode(&mut out[..p.n_bch / 8]);
        let (info, parity) = out.split_at_mut(p.n_bch / 8);
        ldpc.encode(info, parity);
        out
    }

    /// Encode a VL-SNR BBFRAME (`code.k_bch` bits, the last byte's spare
    /// bits ignored) and return the bits sent, one per byte: the BCH
    /// codeword after `xs` zeros, LDPC, then shortened and punctured
    /// (EN 302 307-2 §5.5.2.6).
    pub fn encode_vlsnr(&mut self, code: &VlsnrCode, bbframe: &[u8]) -> Vec<u8> {
        assert_eq!(bbframe.len(), code.bbframe_bytes());
        let (bch, ldpc) = self.vlsnr.entry(code.header).or_insert_with(|| {
            (
                Bch::new(code.frame, 12, code.n_bch),
                LdpcCode::new(code.table),
            )
        });
        let mut cw = vec![0u8; code.n_ldpc() / 8];
        let at = code.xs / 8;
        cw[at..at + bbframe.len()].copy_from_slice(bbframe);
        bch.encode(&mut cw[at..at + code.n_bch / 8]);
        let (info, parity) = cw.split_at_mut(code.k_ldpc() / 8);
        ldpc.encode(info, parity);
        code.sent_bits_of(&cw)
    }
}

/// MPEG-TS packet length and sync byte.
const TS_LEN: usize = 188;
const TS_SYNC: u8 = 0x47;

/// Builds TS-mode BBFRAMEs (§5.1) from a deterministic stream of 188-byte
/// test packets on one PID with a running continuity counter.
///
/// Mode adaptation as §5.1.4 has it: user packets back to back across frames,
/// each packet's sync byte replaced by the CRC-8 of the previous packet's 187
/// bytes, and SYNCD giving the bit offset of the first packet that starts in
/// the data field.
pub struct TsBbFramer {
    rng: u64,
    cc: u8,
    /// Packets made so far, and the PSI/SI tables sent every 40 of them
    /// (PID, section), with each table PID's continuity counter.
    seq: u64,
    tables: Vec<(u16, Vec<u8>)>,
    table_cc: std::collections::BTreeMap<u16, u8>,
    /// The rest of a packet that did not fit the previous frame.
    carry: Vec<u8>,
    /// CRC-8 of the last packet's 187 useful bytes.
    prev_crc: u8,
    pub roll_off: RollOff,
    /// CCM (one MODCOD) or ACM/VCM, for MATYPE.
    pub ccm: bool,
    /// BBFRAMEs made: the low roll-offs' RO alternates frame by frame.
    frames: u64,
    /// A multicast radio in MPE on TEST_MPE_PID, and its packets queued.
    radio: TestRadio,
    mpe_queue: std::collections::VecDeque<[u8; TS_LEN]>,
    mpe_cc: u8,
}

/// The PID carrying IP (the test radio) in MPE.
pub const TEST_MPE_PID: u16 = 0x0FA0;

/// The test stream's data PID and its programme's PMT PID.
pub const TEST_PID: u16 = 0x0100;
pub const TEST_PMT_PID: u16 = 0x1000;

impl TsBbFramer {
    pub fn new(seed: u64) -> Self {
        TsBbFramer {
            rng: seed | 1,
            cc: 0,
            seq: 0,
            // One programme, its data on TEST_PID as private data (it is
            // noise, not video), named in the SDT.
            tables: decsat_ts::psi::test_tables(
                1,
                TEST_PMT_PID,
                decsat_ts::EsInfo::new(TEST_PID, 0x06),
                "DecSAT",
                "DecSAT test signal",
            ),
            table_cc: Default::default(),
            carry: Vec::new(),
            prev_crc: 0,
            roll_off: RollOff::R35,
            ccm: true,
            frames: 0,
            radio: TestRadio::new([239, 255, 1, 2], "DecSAT test radio (MPE)"),
            mpe_queue: Default::default(),
            mpe_cc: 0,
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// The next test packet, sync byte included: the tables (PAT, PMT,
    /// SDT, EIT, NIT, TDT) every 40 packets, else data.
    pub fn packet(&mut self) -> [u8; TS_LEN] {
        let n = self.seq;
        self.seq += 1;
        let k = (n % 40) as usize;
        if k < self.tables.len() {
            let (pid, ref sec) = self.tables[k];
            let cc = self.table_cc.entry(pid).or_default();
            let p = decsat_ts::psi::section_packet(pid, *cc, sec);
            *cc = cc.wrapping_add(1) & 0x0F;
            return p;
        }
        // Every fourth packet: the radio, in MPE.
        if n.is_multiple_of(4) {
            if self.mpe_queue.is_empty() {
                let ip = self.radio.next_packet();
                let sec = decsat_ts::mpe::mpe_section([0x01, 0x00, 0x5E, 0x7F, 0x01, 0x02], &ip);
                self.mpe_queue.extend(decsat_ts::mpe::packetize(
                    TEST_MPE_PID,
                    &mut self.mpe_cc,
                    &sec,
                ));
            }
            return self.mpe_queue.pop_front().unwrap();
        }
        let mut p = [0u8; TS_LEN];
        p[0] = TS_SYNC;
        p[1] = (TEST_PID >> 8) as u8 & 0x1F;
        p[2] = TEST_PID as u8;
        p[3] = 0x10 | (self.cc & 0x0F); // payload only
        self.cc = self.cc.wrapping_add(1);
        for chunk in p[4..].chunks_mut(8) {
            let r = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&r[..chunk.len()]);
        }
        p
    }

    /// The next BBFRAME of `bytes` bytes, header included and BB-scrambled.
    pub fn next_frame(&mut self, bytes: usize) -> Vec<u8> {
        BbFrameSource::next_frame(self, bytes)
    }
}

impl BbFrameSource for TsBbFramer {
    fn set_roll_off(&mut self, roll_off: RollOff) {
        self.roll_off = roll_off;
    }

    fn set_ccm(&mut self, ccm: bool) {
        self.ccm = ccm;
    }

    fn next_frame(&mut self, bytes: usize) -> Vec<u8> {
        let field = bytes - BBHEADER_LEN;
        let mut data = Vec::with_capacity(field);
        data.extend_from_slice(&self.carry[..self.carry.len().min(field)]);
        let carried = data.len();
        self.carry.drain(..carried);
        // SYNCD: where the first packet starting in this field begins.
        let syncd = (data.len() * 8) as u16;
        while data.len() < field {
            let mut p = self.packet();
            // Sync byte out, CRC-8 of the previous packet's useful part in.
            p[0] = self.prev_crc;
            self.prev_crc = crc8(&p[1..]);
            let room = field - data.len();
            if room >= TS_LEN {
                data.extend_from_slice(&p);
            } else {
                data.extend_from_slice(&p[..room]);
                self.carry.extend_from_slice(&p[room..]);
            }
        }
        let header = BbHeader {
            format: StreamFormat::Transport,
            single_stream: true,
            ccm: self.ccm,
            issyi: false,
            npd: false,
            ro: ro_bits(self.roll_off, self.frames),
            isi: 0,
            upl: (TS_LEN * 8) as u16,
            dfl: (field * 8) as u16,
            sync: TS_SYNC,
            syncd,
            high_efficiency: false,
        };
        self.frames += 1;
        let mut frame = Vec::with_capacity(bytes);
        frame.extend_from_slice(&header.to_bytes());
        frame.extend_from_slice(&data);
        bb_scramble(&mut frame);
        frame
    }
}

/// Builds GS-mode BBFRAMEs carrying IP over GSE: a deterministic mix of
/// UDP flows between documentation addresses (RFC 5737), packets of 40 to
/// 9000 bytes so that some fragment across frames, in any GSE [`Variant`].
pub struct GseBbFramer {
    enc: GseEncapsulator,
    radio: TestRadio,
    rng: u64,
    seq: u32,
    pub roll_off: RollOff,
    pub ccm: bool,
    /// Mix a few DNS queries and answers into the traffic.
    dns: bool,
    /// BBFRAMEs made (see `TsBbFramer::frames`).
    frames: u64,
    /// GSE-HEM (S2X): packets sliced across frames rather than each frame
    /// holding whole ones — the packets waiting to go out, and where in
    /// them each starts.
    hem: bool,
    pending: Vec<u8>,
    starts: std::collections::VecDeque<usize>,
    /// The labels the PDUs go to, in turn (terminals of a test network).
    labels: Vec<Label>,
}

impl GseBbFramer {
    pub fn new(seed: u64, variant: Variant) -> Self {
        GseBbFramer {
            enc: GseEncapsulator::new(variant),
            radio: TestRadio::new([239, 255, 1, 1], "DecSAT test radio"),
            rng: seed | 1,
            seq: 0,
            roll_off: RollOff::R35,
            ccm: true,
            dns: false,
            frames: 0,
            hem: false,
            pending: Vec::new(),
            starts: Default::default(),
            labels: vec![Label::Six([0x02, 0, 0, 0, 0, 0x01])],
        }
    }

    /// Mix DNS in: a client asking a resolver for a few names, and the
    /// answers (RFC 5737 addresses), about one packet in eleven.
    pub fn with_dns(mut self) -> Self {
        self.dns = true;
        self
    }

    /// GSE-HEM framing (EN 302 307-2 §5.1.7): TS/GS `10`, CRC8_MODE =
    /// CRC-8 ⊕ 1, GSE packets (standard lengths) sliced across BBFRAMEs with
    /// SYNCD at the first that starts in each.
    pub fn hem(mut self) -> Self {
        self.hem = true;
        self
    }

    /// Send the PDUs to these labels in turn.
    pub fn with_labels(mut self, labels: Vec<Label>) -> Self {
        assert!(!labels.is_empty());
        self.labels = labels;
        self
    }

    /// Queue PDUs until at least `n` are waiting.
    fn top_up(&mut self, n: usize) {
        while self.enc.queued() < n {
            let p = self.packet();
            let label = self.labels[self.seq as usize % self.labels.len()];
            self.enc.push(0x0800, label, p);
        }
    }

    /// GSE-HEM: the next `field` bytes of the packet stream, and SYNCD.
    fn hem_field(&mut self, field: usize) -> (Vec<u8>, u16) {
        while self.pending.len() < field {
            self.top_up(4);
            let run = self.enc.packets(4096);
            let mut q = 0;
            while q + 2 <= run.len() {
                self.starts.push_back(self.pending.len() + q);
                q += 2 + (u16::from_be_bytes([run[q], run[q + 1]]) & 0x0FFF) as usize;
            }
            self.pending.extend_from_slice(&run);
        }
        let data: Vec<u8> = self.pending.drain(..field).collect();
        let syncd = self
            .starts
            .front()
            .filter(|&&s| s < field)
            .map_or(0xFFFF, |&s| (s * 8) as u16);
        for s in self.starts.iter_mut() {
            *s = s.wrapping_sub(field);
        }
        while self.starts.front().is_some_and(|&s| s > usize::MAX / 2) {
            self.starts.pop_front();
        }
        (data, syncd)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// The `n`-th test packet: every third one the test radio, else flow
    /// `n % 5` with a size from a fixed cycle.
    fn packet(&mut self) -> Vec<u8> {
        let n = self.seq;
        self.seq += 1;
        if n.is_multiple_of(3) {
            return self.radio.next_packet();
        }
        if self.dns && n % 11 == 5 {
            const NAMES: [&str; 6] = [
                "www.example.com",
                "mail.example.org",
                "update.vendor.example.net",
                "time.example.com",
                "cdn.example.net",
                "no-such-host.example.invalid",
            ];
            let k = (n / 11) as usize;
            let name = NAMES[k / 2 % NAMES.len()];
            let (client, resolver) = ([192, 0, 2, 10], [198, 51, 100, 53]);
            return if k.is_multiple_of(2) {
                let q = decsat_ip::dns::test_message(k as u16, name, None);
                decsat_ip::packet::udp_v4(client, resolver, 41_000 + (k % 50) as u16, 53, &q)
            } else if name.starts_with("no-such") {
                // NXDOMAIN: the response flags' RCODE 3, no answer.
                let mut a = decsat_ip::dns::test_message(k as u16 - 1, name, None);
                a[2..4].copy_from_slice(&[0x81, 0x83]);
                decsat_ip::packet::udp_v4(resolver, client, 53, 41_000 + (k % 50) as u16, &a)
            } else {
                let ip = [203, 0, 113, 10 + (k / 2 % NAMES.len()) as u8];
                let a = decsat_ip::dns::test_message(k as u16 - 1, name, Some(ip));
                decsat_ip::packet::udp_v4(resolver, client, 53, 41_000 + (k % 50) as u16 - 1, &a)
            };
        }
        let flow = (n % 5) as u8;
        let len = [64usize, 1400, 512, 9000, 40, 1200, 200][n as usize % 7];
        let mut payload = vec![0u8; len];
        for chunk in payload.chunks_mut(8) {
            let r = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&r[..chunk.len()]);
        }
        decsat_ip::packet::udp_v4(
            [192, 0, 2, 10 + flow],
            [198, 51, 100, 20 + flow],
            40_000 + flow as u16,
            5004,
            &payload,
        )
    }
}

impl BbFrameSource for GseBbFramer {
    fn set_roll_off(&mut self, roll_off: RollOff) {
        self.roll_off = roll_off;
    }

    fn set_ccm(&mut self, ccm: bool) {
        self.ccm = ccm;
    }

    fn next_frame(&mut self, bytes: usize) -> Vec<u8> {
        let field = bytes - BBHEADER_LEN;
        let (data, syncd) = if self.hem {
            self.hem_field(field)
        } else {
            // Keep the queue topped up so every frame is full.
            self.top_up(4);
            (self.enc.fill(field), 0)
        };
        let header = BbHeader {
            format: if self.hem {
                StreamFormat::GseHem
            } else {
                StreamFormat::GenericContinuous
            },
            single_stream: true,
            ccm: self.ccm,
            issyi: false,
            npd: false,
            ro: ro_bits(self.roll_off, self.frames),
            isi: 0,
            upl: 0,
            dfl: (field * 8) as u16,
            sync: 0,
            syncd,
            high_efficiency: self.hem,
        };
        self.frames += 1;
        let mut frame = Vec::with_capacity(bytes);
        frame.extend_from_slice(&header.to_bytes());
        frame.extend_from_slice(&data);
        bb_scramble(&mut frame);
        frame
    }
}

/// One input stream of a [`MultiStream`].
struct Input {
    isi: u8,
    source: Box<dyn BbFrameSource>,
    /// Re-flag TS-mode frames as a generic stream of 188-byte packets with
    /// this SYNC byte.
    generic_sync: Option<u8>,
}

/// Several BBFRAME sources as one multistream signal (MIS, EN 302 307-1
/// §5.1.6): frames taken from each in turn, their MATYPE rewritten — the
/// stream's ISI, MIS, ACM, and the RO bits run over all the streams together
/// (EN 302 307-2 §5.1.6 wants the low roll-offs' alternation evident across
/// every ISI). A stream added with [`Self::with_generic_ts`] keeps its
/// transport packets (CRC-8 in place of each sync byte) but is flagged a
/// generic continuous stream with UPL 188 bytes and its own SYNC byte, as a
/// live VSAT hub sends its signalling.
pub struct MultiStream {
    inputs: Vec<Input>,
    next: usize,
    frames: u64,
    roll_off: RollOff,
}

impl Default for MultiStream {
    fn default() -> Self {
        Self::new()
    }
}

impl MultiStream {
    pub fn new() -> Self {
        MultiStream {
            inputs: Vec::new(),
            next: 0,
            frames: 0,
            roll_off: RollOff::R35,
        }
    }

    pub fn with(mut self, isi: u8, source: Box<dyn BbFrameSource>) -> Self {
        self.inputs.push(Input {
            isi,
            source,
            generic_sync: None,
        });
        self
    }

    /// A TS-mode source sent as transport packets in a generic stream.
    pub fn with_generic_ts(mut self, isi: u8, source: Box<dyn BbFrameSource>, sync: u8) -> Self {
        self.inputs.push(Input {
            isi,
            source,
            generic_sync: Some(sync),
        });
        self
    }
}

impl BbFrameSource for MultiStream {
    fn set_roll_off(&mut self, roll_off: RollOff) {
        self.roll_off = roll_off;
        for i in &mut self.inputs {
            i.source.set_roll_off(roll_off);
        }
    }

    fn set_ccm(&mut self, _ccm: bool) {
        // A multistream signal is flagged ACM whatever its MODCODs.
        for i in &mut self.inputs {
            i.source.set_ccm(false);
        }
    }

    fn next_frame(&mut self, bytes: usize) -> Vec<u8> {
        let k = self.next % self.inputs.len();
        self.next += 1;
        let input = &mut self.inputs[k];
        let mut f = input.source.next_frame(bytes);
        bb_scramble(&mut f);
        let mut h = BbHeader::parse(&f).expect("a source's own header");
        h.single_stream = false;
        h.ccm = false;
        h.isi = input.isi;
        h.ro = ro_bits(self.roll_off, self.frames);
        if let Some(sync) = input.generic_sync {
            h.format = StreamFormat::GenericContinuous;
            h.sync = sync;
        }
        f[..BBHEADER_LEN].copy_from_slice(&h.to_bytes());
        self.frames += 1;
        bb_scramble(&mut f);
        f
    }
}

/// A multicast "radio" for test signals: RTP MPEG audio (payload type 14)
/// of silent layer II frames, announced by SAP every 50 packets.
pub struct TestRadio {
    group: [u8; 4],
    name: &'static str,
    n: u64,
    seq: u16,
    ts: u32,
}

impl TestRadio {
    pub fn new(group: [u8; 4], name: &'static str) -> Self {
        TestRadio {
            group,
            name,
            n: 0,
            seq: 0,
            ts: 0,
        }
    }

    /// The next IP packet: a SAP announcement or an RTP packet.
    pub fn next_packet(&mut self) -> Vec<u8> {
        use decsat_ip::mcast::{rtp_packet, sap_packet, silent_mp2_frame};
        use decsat_ip::packet::udp_v4;
        let src = [192, 0, 2, 99];
        let n = self.n;
        self.n += 1;
        if n.is_multiple_of(50) {
            let g = self.group;
            let sdp = format!(
                "v=0\r\no=- 1 1 IN IP4 192.0.2.99\r\ns={}\r\nc=IN IP4 {}.{}.{}.{}/32\r\nt=0 0\r\n\
                 m=audio 5004 RTP/AVP 14\r\n",
                self.name, g[0], g[1], g[2], g[3]
            );
            return udp_v4(
                src,
                [224, 2, 127, 254],
                9875,
                9875,
                &sap_packet(src, 1, &sdp),
            );
        }
        let mut payload = vec![0u8; 4]; // RFC 2250: MBZ and fragment offset
        payload.extend_from_slice(&silent_mp2_frame());
        let rtp = rtp_packet(
            14,
            self.seq,
            self.ts,
            0x0DEC_DB00 | self.group[3] as u32,
            &payload,
        );
        self.seq = self.seq.wrapping_add(1);
        self.ts = self.ts.wrapping_add(2160); // 1152 samples at 48 kHz, 90 kHz clock
        udp_v4(src, self.group, 4000, 5004, &rtp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decsat_core::CodeRate;
    use decsat_fec::LdpcCode;

    #[test]
    fn frames_carry_a_valid_header_and_whole_packets() {
        let p = FecParams::new(FecFrame::Short, CodeRate::new(1, 2)).unwrap();
        let mut f = TsBbFramer::new(3);
        let mut stream = Vec::new();
        for _ in 0..5 {
            let mut fr = f.next_frame(p.bbframe_bytes());
            bb_scramble(&mut fr);
            let h = BbHeader::parse(&fr).unwrap();
            assert_eq!(h.format, StreamFormat::Transport);
            assert_eq!(h.dfl as usize, p.k_bch - 80);
            // SYNCD lands on a packet boundary of the concatenated stream.
            assert_eq!((stream.len() + h.syncd as usize / 8) % TS_LEN, 0);
            stream.extend_from_slice(&fr[BBHEADER_LEN..]);
        }
        // Every packet after the first starts with the CRC of the one before.
        for w in stream.as_chunks::<TS_LEN>().0.windows(2) {
            assert_eq!(w[1][0], crc8(&w[0][1..]));
        }
    }

    #[test]
    fn hem_frames_slice_packets_and_point_at_the_first() {
        let p = FecParams::new(FecFrame::Short, CodeRate::new(1, 2)).unwrap();
        let mut f = GseBbFramer::new(5, Variant::STANDARD).hem();
        let mut stream = Vec::new();
        let mut sliced = 0;
        for _ in 0..30 {
            let mut fr = f.next_frame(p.bbframe_bytes());
            bb_scramble(&mut fr);
            let h = BbHeader::parse(&fr).unwrap();
            assert_eq!(h.format, StreamFormat::GseHem);
            assert!(h.high_efficiency);
            let field = &fr[BBHEADER_LEN..BBHEADER_LEN + h.dfl as usize / 8];
            if h.syncd != 0 {
                sliced += 1;
            }
            if h.syncd != 0xFFFF {
                // SYNCD lands on a packet start of the whole stream.
                let at = stream.len() + h.syncd as usize / 8;
                let mut q = 0;
                while q < at {
                    q += 2
                        + (u16::from_be_bytes([
                            stream_or(&stream, field, q),
                            stream_or(&stream, field, q + 1),
                        ]) & 0x0FFF) as usize;
                }
                assert_eq!(q, at);
            }
            stream.extend_from_slice(field);
        }
        assert!(sliced > 10, "{sliced}");

        fn stream_or(stream: &[u8], field: &[u8], i: usize) -> u8 {
            if i < stream.len() {
                stream[i]
            } else {
                field[i - stream.len()]
            }
        }
    }

    #[test]
    fn a_multistream_signal_flags_each_stream() {
        let p = FecParams::new(FecFrame::Short, CodeRate::new(1, 2)).unwrap();
        let mut m = MultiStream::new()
            .with_generic_ts(0, Box::new(TsBbFramer::new(1)), 0xBB)
            .with(1, Box::new(GseBbFramer::new(2, Variant::STANDARD)));
        m.set_roll_off(RollOff::R05);
        let mut rd = decsat_frame::RollOffReader::new();
        for n in 0..6 {
            let mut fr = m.next_frame(p.bbframe_bytes());
            bb_scramble(&mut fr);
            let h = BbHeader::parse(&fr).unwrap();
            rd.push(h.ro);
            assert!(!h.single_stream && !h.ccm);
            assert_eq!(h.isi, n % 2);
            assert_eq!(h.format, StreamFormat::GenericContinuous);
            if n % 2 == 0 {
                assert_eq!((h.upl, h.sync), (1504, 0xBB));
            } else {
                assert_eq!(h.upl, 0);
            }
        }
        assert_eq!(rd.roll_off(), Some(RollOff::R05));
    }

    #[test]
    fn encoded_frames_are_ldpc_codewords() {
        let mut enc = FecEncoder::new();
        let mut f = TsBbFramer::new(4);
        for p in decsat_fec::params::all_s2().step_by(4) {
            let bb = f.next_frame(p.bbframe_bytes());
            let fec = enc.encode(p, &bb);
            assert_eq!(&fec[..bb.len()], &bb[..]);
            assert!(LdpcCode::new(p.ldpc_table()).is_codeword(&fec), "{p:?}");
            let bch = Bch::new(p.frame, p.t, p.n_bch);
            let mut cw = fec[..p.n_bch / 8].to_vec();
            assert_eq!(bch.decode(&mut cw), Ok(0));
        }
    }
}
