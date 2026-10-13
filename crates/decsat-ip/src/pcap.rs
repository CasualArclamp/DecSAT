//! A classic libpcap file writer for raw IP packets (link type 101,
//! LINKTYPE_RAW: each record starts at the IP header, v4 or v6), which
//! Wireshark and tcpdump read as is; and a reader for such files (and
//! Ethernet ones), for looking at a recording again.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// LINKTYPE_RAW.
const LINKTYPE_RAW: u32 = 101;
/// Longest packet stored whole.
const SNAPLEN: u32 = 65_535;

/// Writes to a file by default; `W` can be any byte sink — Wireshark's
/// standard input, for a live capture (a type parameter with a default:
/// plain `PcapWriter` is the file writer).
pub struct PcapWriter<W: Write = BufWriter<File>> {
    out: W,
    packets: u64,
    bytes: u64,
}

impl PcapWriter {
    /// Create `path` and write the global header.
    pub fn create(path: &Path) -> io::Result<Self> {
        PcapWriter::new(BufWriter::new(File::create(path)?))
    }
}

impl<W: Write> PcapWriter<W> {
    /// Write the global header to `out`; packets follow.
    pub fn new(mut out: W) -> io::Result<Self> {
        // Magic (microsecond timestamps, little-endian), version 2.4,
        // GMT offset 0, accuracy 0, snaplen, link type.
        out.write_all(&0xA1B2_C3D4u32.to_le_bytes())?;
        out.write_all(&2u16.to_le_bytes())?;
        out.write_all(&4u16.to_le_bytes())?;
        out.write_all(&0i32.to_le_bytes())?;
        out.write_all(&0u32.to_le_bytes())?;
        out.write_all(&SNAPLEN.to_le_bytes())?;
        out.write_all(&LINKTYPE_RAW.to_le_bytes())?;
        Ok(PcapWriter {
            out,
            packets: 0,
            bytes: 0,
        })
    }

    /// Append one packet stamped `at`.
    pub fn write(&mut self, at: SystemTime, packet: &[u8]) -> io::Result<()> {
        let t = at.duration_since(UNIX_EPOCH).unwrap_or_default();
        let stored = packet.len().min(SNAPLEN as usize);
        self.out.write_all(&(t.as_secs() as u32).to_le_bytes())?;
        self.out.write_all(&t.subsec_micros().to_le_bytes())?;
        self.out.write_all(&(stored as u32).to_le_bytes())?;
        self.out.write_all(&(packet.len() as u32).to_le_bytes())?;
        self.out.write_all(&packet[..stored])?;
        self.packets += 1;
        self.bytes += packet.len() as u64;
        Ok(())
    }

    pub fn packets(&self) -> u64 {
        self.packets
    }

    /// Bytes of packet data written (headers excluded).
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// LINKTYPE_ETHERNET and LINKTYPE_IPV4.
const LINKTYPE_ETHERNET: u32 = 1;
const LINKTYPE_IPV4: u32 = 228;

/// Read a classic PCAP file's IP packets with their times (s since 1970):
/// raw IP (DecSAT's own) or Ethernet (802.1Q tags skipped, IP frames
/// only). Either byte order, micro- or nanosecond stamps. Not pcapng.
pub fn read_pcap(path: &Path) -> io::Result<Vec<(f64, Vec<u8>)>> {
    let d = std::fs::read(path)?;
    let bad = |why: &str| io::Error::new(io::ErrorKind::InvalidData, why.to_string());
    if d.len() < 24 {
        return Err(bad("too short for a PCAP header"));
    }
    let magic = [d[0], d[1], d[2], d[3]];
    // The magic says the byte order (as written) and the stamp unit.
    let (le, nanos) = match magic {
        [0xD4, 0xC3, 0xB2, 0xA1] => (true, false),
        [0xA1, 0xB2, 0xC3, 0xD4] => (false, false),
        [0x4D, 0x3C, 0xB2, 0xA1] => (true, true),
        [0xA1, 0xB2, 0x3C, 0x4D] => (false, true),
        _ => return Err(bad("not a classic PCAP file (pcapng is not read)")),
    };
    let u32_at = |at: usize| {
        let b = [d[at], d[at + 1], d[at + 2], d[at + 3]];
        if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        }
    };
    let link = u32_at(20) & 0xFFFF;
    if ![LINKTYPE_RAW, LINKTYPE_ETHERNET, LINKTYPE_IPV4].contains(&link) {
        return Err(bad(&format!("link type {link} is not read")));
    }
    let mut out = Vec::new();
    let mut at = 24;
    while at + 16 <= d.len() {
        let (secs, frac, stored) = (u32_at(at), u32_at(at + 4), u32_at(at + 8) as usize);
        at += 16;
        let Some(rec) = d.get(at..at + stored) else {
            break; // cut short: keep what came before
        };
        at += stored;
        let t = secs as f64 + frac as f64 * if nanos { 1e-9 } else { 1e-6 };
        let ip = if link == LINKTYPE_ETHERNET {
            // Destination, source, then EtherTypes past any VLAN tags.
            let mut et_at = 12;
            while rec.get(et_at..et_at + 2) == Some(&[0x81, 0x00]) {
                et_at += 4;
            }
            match rec.get(et_at..et_at + 2) {
                Some([0x08, 0x00]) | Some([0x86, 0xDD]) => &rec[et_at + 2..],
                _ => continue,
            }
        } else {
            rec
        };
        out.push((t, ip.to_vec()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::udp_v4;

    #[test]
    fn writes_a_readable_file() {
        let path = std::env::temp_dir().join(format!("decsat-pcap-{}.pcap", std::process::id()));
        let p = udp_v4([10, 0, 0, 1], [10, 0, 0, 2], 5000, 6000, b"payload");
        {
            let mut w = PcapWriter::create(&path).unwrap();
            w.write(UNIX_EPOCH + std::time::Duration::from_micros(1_500_000), &p)
                .unwrap();
            w.write(SystemTime::now(), &p).unwrap();
            w.flush().unwrap();
            assert_eq!(w.packets(), 2);
        }
        let b = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(&b[..4], &[0xD4, 0xC3, 0xB2, 0xA1]);
        assert_eq!(u32::from_le_bytes(b[20..24].try_into().unwrap()), 101);
        // First record: ts 1 s 500 000 µs, lengths, then the packet.
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(b[28..32].try_into().unwrap()), 500_000);
        assert_eq!(
            u32::from_le_bytes(b[32..36].try_into().unwrap()) as usize,
            p.len()
        );
        assert_eq!(&b[40..40 + p.len()], &p[..]);
        assert_eq!(b.len(), 24 + 2 * (16 + p.len()));
    }

    #[test]
    fn reads_back_what_it_wrote() {
        let path = std::env::temp_dir().join(format!("decsat-pcap-rd-{}.pcap", std::process::id()));
        let p = udp_v4([10, 0, 0, 1], [233, 1, 2, 3], 5000, 6000, b"radar");
        {
            let mut w = PcapWriter::create(&path).unwrap();
            w.write(UNIX_EPOCH + std::time::Duration::from_micros(2_250_000), &p)
                .unwrap();
            w.flush().unwrap();
        }
        let r = read_pcap(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(r.len(), 1);
        assert!((r[0].0 - 2.25).abs() < 1e-9);
        assert_eq!(r[0].1, p);
    }
}
