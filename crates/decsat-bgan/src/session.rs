// made by CasualArclamp
//! A BGAN decoding session: the receiver, plus what is done with its
//! payload (recording, and the view the UI shows). One per VFO.

use crate::frame::C32;
use crate::layers::files::FileScanner;
use crate::layers::findings::{FindingsScanner, HostTally};
use crate::layers::pcap::{CarvedPacket, DLT_RAW, Ipv4Carver, PcapWriter};
use crate::layers::segments::SegmentReassembler;
use crate::layers::sip::SipScanner;
use crate::rx::{BganRx, RxStatus};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What the UI shows.
#[derive(Debug, Clone, Default)]
pub struct BganView {
    pub rx: RxStatus,
    /// One-line summary for the VFO list.
    pub message: String,
    /// Recent phase-corrected data symbols.
    pub constellation: Vec<C32>,
    /// Payload file being written and its size, when recording.
    pub recording: Option<(PathBuf, u64)>,
    /// Payload layers (bytes of accepted blocks, in order).
    pub findings: u64,
    pub packets: u64,
    pub documents: u64,
    pub documents_intact: u64,
    pub sip_messages: u64,
    /// Hostnames the payload implicates, most common first.
    pub hosts: Vec<(String, usize)>,
    /// The latest findings, newest last.
    pub recent: Vec<String>,
    /// Wireshark capturing the carved IPv4 live: packets written to it and
    /// dropped. Filled in by the engine, which owns the Wireshark process
    /// (this crate only hands over the packets, see `take_live`).
    pub wireshark: Option<(u64, u64)>,
    pub wireshark_error: Option<String>,
}

#[derive(Default)]
struct Layers {
    findings: Option<FindingsScanner>,
    files: Option<FileScanner>,
    carver: Option<Ipv4Carver>,
    /// Packets split across segments, which the carver cannot see.
    segments: Option<SegmentReassembler>,
    sip: Option<SipScanner>,
    tally: Option<HostTally>,
    n_findings: u64,
    n_packets: u64,
    n_docs: u64,
    n_intact: u64,
    n_sip: u64,
    recent: Vec<String>,
}

pub struct BganSession {
    rx: BganRx,
    in_rate: f64,
    started: std::time::Instant,
    fed_samples: u64,
    // recording
    record: bool,
    record_dir: PathBuf,
    tag: String,
    payload: Option<(PathBuf, BufWriter<File>, u64)>,
    pcap: Option<PcapWriter<BufWriter<File>>>,
    layers: Layers,
    open_failed: bool,
    /// Hand the carved packets over as they appear (`take_live`).
    live: bool,
    live_out: Vec<Vec<u8>>,
}

impl BganSession {
    pub fn new(in_rate: f64) -> Self {
        BganSession {
            rx: BganRx::new(in_rate),
            in_rate,
            started: std::time::Instant::now(),
            fed_samples: 0,
            record: false,
            record_dir: PathBuf::new(),
            tag: String::new(),
            payload: None,
            pcap: None,
            layers: Layers {
                findings: Some(FindingsScanner::new()),
                files: Some(FileScanner::new()),
                carver: Some(Ipv4Carver::default()),
                segments: Some(SegmentReassembler::default()),
                sip: Some(SipScanner::new()),
                tally: Some(HostTally::new()),
                ..Default::default()
            },
            open_failed: false,
            live: false,
            live_out: Vec::new(),
        }
    }

    /// Keep the carved IPv4 packets for `take_live` (a live Wireshark wants
    /// them as they are found) or stop keeping them.
    pub fn set_live(&mut self, on: bool) {
        self.live = on;
        if !on {
            self.live_out.clear();
        }
    }

    /// The IPv4 packets found since the last call: carved ones (see
    /// `layers::pcap`) and ones rebuilt from link-layer segments (see
    /// `layers::segments`). Packets with a segment lost in a failed block are
    /// missing.
    pub fn take_live(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.live_out)
    }

    pub fn set_pool(&mut self, pool: Option<Arc<rayon::ThreadPool>>) {
        self.rx.set_pool(pool);
    }

    /// Where the carrier is relative to the VFO centre (Hz).
    pub fn carrier_offset_hz(&self) -> f64 {
        self.rx.carrier_offset_hz()
    }
    pub fn retune_hz(&mut self, hz: f64) {
        self.rx.retune_hz(hz);
    }
    pub fn locked(&self) -> bool {
        self.rx.status().locked
    }

    /// Start/stop recording; `tag` names the files (`bgan-<tag>-<time>_payload.bin`).
    pub fn set_recording(&mut self, on: bool, dir: &Path, tag: &str) {
        if on && !self.record {
            self.open_failed = false;
        }
        if !on && self.record {
            // the last packets are still waiting for look-ahead that this
            // recording will never get: decide them and write them first
            self.flush_packets();
        }
        self.record = on;
        self.record_dir = dir.to_path_buf();
        self.tag = tag.to_string();
        if !on {
            if let Some((_, mut w, _)) = self.payload.take() {
                let _ = w.flush();
            }
            self.pcap = None;
        }
    }

    pub fn push(&mut self, iq: &[C32]) {
        let t0 = std::time::Instant::now();
        self.rx.push(iq);
        self.fed_samples += iq.len() as u64;
        let frames = self.rx.take_frames();
        for fr in &frames {
            for b in &fr.blocks {
                self.write_payload(&b.payload);
                self.feed_layers(&b.payload);
            }
        }
        // total CPU against the signal time it covered (1.0 = just real time)
        let sig_s = iq.len() as f64 / self.in_rate;
        if sig_s > 0.0 {
            let r = (t0.elapsed().as_secs_f64() / sig_s) as f32;
            let st = &mut self.rx.status_mut().load;
            *st += 0.05 * (r - *st);
        }
    }

    /// Run one accepted block's bytes through the payload layers. The blocks
    /// are the concatenated stream DecBGAN's `payload.bin` holds.
    fn feed_layers(&mut self, bytes: &[u8]) {
        let l = &mut self.layers;
        if let (Some(f), Some(t)) = (l.findings.as_mut(), l.tally.as_mut()) {
            for fi in f.push(bytes) {
                t.add(&fi);
                l.n_findings += 1;
                l.recent.push(format!("{:?}: {}", fi.kind, fi.summary));
            }
            let n = l.recent.len();
            if n > 40 {
                l.recent.drain(..n - 40);
            }
        }
        if let Some(f) = l.files.as_mut() {
            for d in f.push(bytes) {
                l.n_docs += 1;
                l.n_intact += u64::from(d.intact());
            }
        }
        if let Some(s) = l.sip.as_mut() {
            l.n_sip += s.push(bytes).len() as u64;
        }
        let mut carved = Vec::new();
        if let Some(c) = l.carver.as_mut() {
            carved = c.push(bytes);
        }
        // never the same packet twice: the reassembler only reports packets
        // of two or more segments, whose headers the carver cannot find
        if let Some(s) = l.segments.as_mut() {
            carved.extend(s.push(bytes));
        }
        self.emit_packets(carved);
    }

    /// Decide what the packet scanners are still holding back (positions
    /// waiting for look-ahead, as if the stream ended here), hand it on, and
    /// start fresh scanners: decoding goes on after a recording stops. A
    /// packet that straddles this point is lost; it would be cut short in the
    /// recording's payload file too.
    fn flush_packets(&mut self) {
        let l = &mut self.layers;
        let mut carved = Vec::new();
        // `take` swaps a fresh (default) scanner in and hands back the old
        // one, which `finish` then empties
        if let Some(c) = l.carver.as_mut() {
            carved = std::mem::take(c).finish();
        }
        if let Some(s) = l.segments.as_mut() {
            carved.extend(std::mem::take(s).finish());
        }
        self.emit_packets(carved);
    }

    /// Count packets, give them to a live Wireshark and write them to the
    /// recording's pcap.
    fn emit_packets(&mut self, carved: Vec<CarvedPacket>) {
        self.layers.n_packets += carved.len() as u64;
        if self.live {
            self.live_out.extend(carved.iter().map(|p| p.data.clone()));
        }
        if self.record && !carved.is_empty() {
            if self.pcap.is_none() && !self.open_failed {
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let p = self
                    .record_dir
                    .join(format!("bgan-{}-{secs}_ipv4.pcap", self.tag));
                let _ = std::fs::create_dir_all(&self.record_dir);
                if let Ok(f) = File::create(&p) {
                    self.pcap = PcapWriter::new(BufWriter::new(f), DLT_RAW).ok();
                }
            }
            if let Some(w) = self.pcap.as_mut() {
                let t = self.started.elapsed().as_secs_f64();
                for p in &carved {
                    let _ = w.write_record(t, &p.data);
                }
            }
        }
    }

    fn write_payload(&mut self, bytes: &[u8]) {
        if !self.record || self.open_failed {
            return;
        }
        if self.payload.is_none() {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let p = self
                .record_dir
                .join(format!("bgan-{}-{secs}_payload.bin", self.tag));
            let _ = std::fs::create_dir_all(&self.record_dir);
            match File::create(&p) {
                Ok(f) => self.payload = Some((p, BufWriter::new(f), 0)),
                Err(_) => {
                    self.open_failed = true;
                    return;
                }
            }
        }
        if let Some((_, w, n)) = &mut self.payload
            && w.write_all(bytes).is_ok()
        {
            *n += bytes.len() as u64;
        }
    }

    pub fn view(&self) -> BganView {
        let st = self.rx.status().clone();
        let message = if st.tables_missing {
            "ETSI Annex C tables not found — see the README (set the Annex folder)".to_string()
        } else if !st.locked {
            let t = self.started.elapsed().as_secs_f32();
            format!("searching for an F80T4.5X-8B carrier… {t:.0} s")
        } else {
            format!(
                "locked — Es/N0 {:.1} dB, {}/{} blocks, UW {}",
                st.esn0_db, st.blocks_ok, st.blocks_total, st.uw_level
            )
        };
        BganView {
            rx: st,
            message,
            constellation: self.rx.constellation().to_vec(),
            recording: self.payload.as_ref().map(|(p, _, n)| (p.clone(), *n)),
            findings: self.layers.n_findings,
            packets: self.layers.n_packets,
            documents: self.layers.n_docs,
            documents_intact: self.layers.n_intact,
            sip_messages: self.layers.n_sip,
            hosts: self
                .layers
                .tally
                .as_ref()
                .map(|t| t.most_common().into_iter().take(10).collect())
                .unwrap_or_default(),
            recent: self.layers.recent.iter().rev().take(12).cloned().collect(),
            wireshark: None,
            wireshark_error: None,
        }
    }
}

impl Drop for BganSession {
    fn drop(&mut self) {
        if self.record {
            self.flush_packets();
        }
        if let Some((_, w, _)) = &mut self.payload {
            let _ = w.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 20-byte-header UDP packet with valid header and UDP checksums.
    fn packet(n: usize) -> Vec<u8> {
        let mut h = vec![0u8; 20];
        h[0] = 0x45;
        h[2..4].copy_from_slice(&((20 + n) as u16).to_be_bytes());
        h[8] = 64;
        h[9] = 17;
        h[12..16].copy_from_slice(&[10, 0, 0, 1]);
        h[16..20].copy_from_slice(&[10, 0, 0, 2]);
        let mut s: u32 = h
            .chunks(2)
            .map(|c| u32::from(c[0]) << 8 | u32::from(c[1]))
            .sum();
        while s >> 16 != 0 {
            s = (s & 0xFFFF) + (s >> 16);
        }
        h[10..12].copy_from_slice(&(!(s as u16)).to_be_bytes());
        h.extend((0..n).map(|i| i as u8));
        crate::layers::pcap::tests::seal(&mut h);
        h
    }

    /// With live on, the carved packets come out of `take_live` once each,
    /// even when a packet straddles two pushes; with it off nothing is kept.
    #[test]
    fn live_hands_over_each_carved_packet_once() {
        let mut stream = vec![0u8; 37];
        stream.extend(packet(100));
        stream.extend([0xFF; 5]);
        stream.extend(packet(200));
        stream.extend([0u8; 9]);

        let mut s = BganSession::new(192_000.0);
        s.set_live(true);
        let mut got = Vec::new();
        for c in stream.chunks(64) {
            s.feed_layers(c);
            got.extend(s.take_live());
        }
        assert_eq!(got, vec![packet(100), packet(200)]);
        assert!(s.take_live().is_empty());

        let mut off = BganSession::new(192_000.0);
        off.feed_layers(&stream);
        assert!(off.take_live().is_empty());
    }

    /// A packet the link layer split into segments comes out of the session
    /// alongside a carved one, each once, in any chunking.
    #[test]
    fn split_packets_reach_the_live_feed() {
        use crate::layers::segments::tests::{seg, split};
        let plain = packet(50);
        let cut = packet(120);
        let parts = split([0x57, 0xc7], 3, 1, &cut, &[10, 80]);
        let mut stream = vec![0u8; 11];
        stream.extend(&parts[0]);
        stream.extend(seg([0x55, 0xbc], 0, &[])); // an acknowledgement between
        stream.extend(&plain);
        stream.extend(&parts[1]);
        stream.extend([0u8; 30]);
        stream.extend(&parts[2]);
        // like the carver, the reassembler decides a position only once the
        // longest segment that could start there has arrived (260 octets), and
        // the session never calls `finish`: in a live stream the next blocks
        // supply that look-ahead
        stream.extend([0u8; 260]);

        for chunk in [1usize, 13, 64, stream.len()] {
            let mut s = BganSession::new(192_000.0);
            s.set_live(true);
            let mut got = Vec::new();
            for c in stream.chunks(chunk) {
                s.feed_layers(c);
                got.extend(s.take_live());
            }
            got.sort();
            let mut want = vec![plain.clone(), cut.clone()];
            want.sort();
            assert_eq!(got, want, "chunk {chunk}");
            assert_eq!(s.layers.n_packets, 2, "chunk {chunk}");
        }
    }

    /// The records of a raw pcap file: 24-octet file header, then a 16-octet
    /// header per record with its length at octets 8..12.
    fn pcap_records(path: &std::path::Path) -> Vec<Vec<u8>> {
        let b = std::fs::read(path).unwrap();
        let mut out = Vec::new();
        let mut i = 24;
        while i + 16 <= b.len() {
            let n = u32::from_le_bytes(b[i + 8..i + 12].try_into().unwrap()) as usize;
            out.push(b[i + 16..i + 16 + n].to_vec());
            i += 16 + n;
        }
        out
    }

    /// Stopping a recording writes the packets still waiting for look-ahead,
    /// and the session keeps decoding afterwards.
    #[test]
    fn stopping_a_recording_flushes_held_packets() {
        use crate::layers::segments::tests::split;
        let plain = packet(50);
        let cut = packet(120);
        let parts = split([0x57, 0xc7], 3, 1, &cut, &[10, 80]);
        let mut stream = vec![0u8; 11];
        stream.extend(&plain);
        // what could open a 260-octet segment (length-present bit, length
        // 255): the reassembler waits for it, so `cut`, after it, is held back
        stream.extend([0x41, 0x00, 0xff]);
        stream.extend(parts.concat());
        stream.extend([0u8; 9]);

        let dir = std::env::temp_dir().join(format!("decsat-bgan-flush-{}", std::process::id()));
        let mut s = BganSession::new(192_000.0);
        s.set_live(true);
        s.set_recording(true, &dir, "t");
        s.feed_layers(&stream);
        assert_eq!(
            s.take_live(),
            vec![plain.clone()],
            "held back before the stop"
        );
        s.set_recording(false, &dir, "t");
        assert_eq!(s.take_live(), vec![cut.clone()], "released by the stop");

        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(pcap_records(&files[0]), vec![plain.clone(), cut]);
        std::fs::remove_dir_all(&dir).unwrap();

        // fresh scanners: later packets still come through
        let mut more = vec![0u8; 5];
        more.extend(packet(30));
        more.extend([0u8; 9]);
        s.feed_layers(&more);
        assert_eq!(s.take_live(), vec![packet(30)]);
    }
}
