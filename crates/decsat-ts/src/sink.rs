// made by CasualArclamp
//! Where a transport stream goes: a `.ts` file, UDP datagrams, or a TCP
//! server that media players connect to.
//!
//! - **UDP**: 7 packets (1316 bytes) per datagram, the usual size for TS over
//!   UDP. VLC: `udp://@:1234`. PotPlayer: `udp://127.0.0.1:1234`.
//! - **TCP**: a server; each client gets the live stream. A client that
//!   speaks HTTP (`GET …`) gets an HTTP response first, so VLC and PotPlayer
//!   can open `http://127.0.0.1:8001/`; one that says nothing (VLC's
//!   `tcp://127.0.0.1:8001`) gets the raw stream. Each client has its own
//!   thread and a bounded queue, so a slow or stalled player loses data
//!   rather than holding up the receiver.
//!
//! - **A tap** ([`TsTap`]): the stream handed to something in this process
//!   (the in-app player) over a bounded channel; when the taker falls
//!   behind, packets are dropped and counted, never waited for.
//!
//! Both default to 127.0.0.1 in the GUI: reachable from this machine only,
//! unless pointed elsewhere on purpose.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use decsat_ip::serve::StreamServer;

use crate::TsAnalyser;
use crate::av1::{Av1Extractor, Av1Unit, find_sequence_header, ivf_frame, ivf_header};
use crate::deframe::TS_LEN;

/// Packets per UDP datagram.
const PER_DATAGRAM: usize = 7;

/// A `.ts` file. When the stream's video turns out to be AV1 in AOM's TS
/// mapping (crate::av1), which ffmpeg and VLC cannot read from a TS (they
/// show it as a private data stream), the AV1 is also written beside it as
/// `NAME.ivf`, which both play.
pub struct TsFile {
    out: BufWriter<File>,
    pub path: PathBuf,
    pub packets: u64,
    av1: Av1Side,
    /// Packets seen by the PMT watcher.
    seen: u32,
}

/// The `.ivf` that goes with an AV1 recording.
enum Av1Side {
    /// Watching for the PMT (analyser) to say what the video is.
    Looking(Box<TsAnalyser>),
    /// Not AV1: nothing more to do.
    No,
    /// AV1 on `pid`: units out of the PES stream into the IVF.
    Yes {
        pid: u16,
        ex: Av1Extractor,
        ivf: Option<BufWriter<File>>,
        ivf_path: PathBuf,
        last_pts: u64,
        buf: Vec<u8>,
    },
}

impl TsFile {
    pub fn create(path: &Path) -> io::Result<Self> {
        Ok(TsFile {
            out: BufWriter::new(File::create(path)?),
            path: path.to_path_buf(),
            packets: 0,
            av1: Av1Side::Looking(Box::new(TsAnalyser::new())),
            seen: 0,
        })
    }

    pub fn write(&mut self, packets: &[[u8; TS_LEN]]) -> io::Result<()> {
        for p in packets {
            self.out.write_all(p)?;
            self.av1_packet(p);
        }
        self.packets += packets.len() as u64;
        Ok(())
    }

    /// One packet to the AV1 side file (or to the PMT watcher before it
    /// knows). Errors on the side file are not the recording's.
    fn av1_packet(&mut self, p: &[u8; TS_LEN]) {
        // Rust note: `match &mut self.av1` borrows the enum's contents
        // mutably; the new state is set after the match, once that ends.
        let mut found = None;
        match &mut self.av1 {
            Av1Side::No => {}
            Av1Side::Looking(an) => {
                an.packet(p);
                // The report is not free: look every so often, not per packet.
                self.seen += 1;
                if self.seen.is_multiple_of(256) {
                    let r = an.report();
                    let streams: Vec<_> =
                        r.programmes.iter().flat_map(|q| q.streams.iter()).collect();
                    if let Some(e) = streams.iter().find(|e| e.kind == Some("AV1 video")) {
                        found = Some(Some(e.pid));
                    } else if streams
                        .iter()
                        .any(|e| e.kind.is_some_and(|k| k.contains("video")))
                    {
                        found = Some(None);
                    }
                }
            }
            Av1Side::Yes {
                pid,
                ex,
                ivf,
                ivf_path,
                last_pts,
                buf,
            } => {
                if u16::from_be_bytes([p[1] & 0x1F, p[2]]) == *pid
                    && let Some(u) = ex.push(p)
                {
                    write_unit(u, ivf, ivf_path, last_pts, buf);
                }
            }
        }
        match found {
            Some(Some(pid)) => {
                self.av1 = Av1Side::Yes {
                    pid,
                    ex: Av1Extractor::new(),
                    ivf: None,
                    ivf_path: self.path.with_extension("ivf"),
                    last_pts: 0,
                    buf: Vec::new(),
                }
            }
            Some(None) => self.av1 = Av1Side::No,
            None => {}
        }
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if let Av1Side::Yes {
            ex,
            ivf,
            ivf_path,
            last_pts,
            buf,
            ..
        } = &mut self.av1
        {
            if let Some(u) = ex.finish() {
                write_unit(u, ivf, ivf_path, last_pts, buf);
            }
            if let Some(f) = ivf {
                let _ = f.flush();
            }
        }
        self.out.flush()
    }
}

/// One AV1 unit to the IVF. The file starts at the first unit with a
/// sequence header (what a decoder needs; the width and height go in the
/// IVF header), so a recording that began mid-stream still plays.
fn write_unit(
    u: Av1Unit,
    ivf: &mut Option<BufWriter<File>>,
    path: &Path,
    last_pts: &mut u64,
    buf: &mut Vec<u8>,
) {
    if ivf.is_none() {
        let Some(seq) = find_sequence_header(&u.tu) else {
            return;
        };
        let Ok(f) = File::create(path) else { return };
        let mut f = BufWriter::new(f);
        if f.write_all(&ivf_header(seq.width as u16, seq.height as u16))
            .is_err()
        {
            return;
        }
        *ivf = Some(f);
    }
    // Units without a PTS keep the last one's.
    let pts = u.pts.unwrap_or(*last_pts);
    *last_pts = pts;
    buf.clear();
    ivf_frame(pts, &u.tu, buf);
    if let Some(f) = ivf {
        let _ = f.write_all(buf);
    }
}

/// TS over UDP to one address.
pub struct UdpSink {
    socket: UdpSocket,
    pub target: SocketAddr,
    pending: Vec<u8>,
    pub datagrams: u64,
    pub errors: u64,
}

impl UdpSink {
    pub fn new(target: SocketAddr) -> io::Result<Self> {
        let bind: SocketAddr = if target.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        };
        let socket = UdpSocket::bind(bind)?;
        if target.ip().is_multicast() {
            let _ = socket.set_multicast_ttl_v4(1);
        }
        Ok(UdpSink {
            socket,
            target,
            pending: Vec::with_capacity(PER_DATAGRAM * TS_LEN),
            datagrams: 0,
            errors: 0,
        })
    }

    pub fn write(&mut self, packets: &[[u8; TS_LEN]]) {
        for p in packets {
            self.pending.extend_from_slice(p);
            if self.pending.len() == PER_DATAGRAM * TS_LEN {
                match self.socket.send_to(&self.pending, self.target) {
                    Ok(_) => self.datagrams += 1,
                    Err(_) => self.errors += 1,
                }
                self.pending.clear();
            }
        }
    }
}

/// A TCP server streaming TS to every connected client (HTTP-aware, see
/// `decsat_ip::serve`).
pub struct TcpSink {
    server: StreamServer,
    pub addr: SocketAddr,
}

impl TcpSink {
    /// Listen on `addr` (port 0 picks a free one; see `addr` after).
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let server = StreamServer::bind(addr, "video/mp2t")?;
        Ok(TcpSink {
            addr: server.addr,
            server,
        })
    }

    /// Addresses of the players connected now.
    pub fn clients(&self) -> Vec<SocketAddr> {
        self.server.clients()
    }

    pub fn write(&mut self, packets: &[[u8; TS_LEN]]) {
        if !packets.is_empty() {
            self.server.write(packets.concat());
        }
    }
}

/// A copy of a stream for a taker in this process: each batch of packets
/// is sent as it comes; a full channel drops the batch (counted).
///
/// Rust note: it lives in a VFO's settings, which are compared and cloned,
/// so `PartialEq` is by identity (two clones of one tap are equal) and
/// `Clone` shares the one channel.
#[derive(Clone)]
pub struct TsTap {
    tx: SyncSender<Vec<[u8; TS_LEN]>>,
    dropped: Arc<AtomicU64>,
    /// The video PID the taker decodes: it is handed the stream from that
    /// PID's last keyframe first (`crate::backlog`).
    video: Option<u16>,
}

impl TsTap {
    /// A tap and its receiving end, holding up to `batches` batches.
    pub fn new(batches: usize) -> (TsTap, Receiver<Vec<[u8; TS_LEN]>>) {
        let (tx, rx) = sync_channel(batches);
        (
            TsTap {
                tx,
                dropped: Arc::new(AtomicU64::new(0)),
                video: None,
            },
            rx,
        )
    }

    /// Hand on `packets`; false once the taker has gone.
    pub fn send(&self, packets: &[[u8; TS_LEN]]) -> bool {
        if packets.is_empty() {
            return true;
        }
        match self.tx.try_send(packets.to_vec()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.dropped
                    .fetch_add(packets.len() as u64, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// The taker decodes this video PID (its keyframes start the backlog
    /// it is handed).
    pub fn for_video(mut self, pid: Option<u16>) -> Self {
        self.video = pid;
        self
    }

    pub fn video(&self) -> Option<u16> {
        self.video
    }

    /// Packets dropped because the taker was behind.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl PartialEq for TsTap {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.dropped, &other.dropped)
    }
}

impl std::fmt::Debug for TsTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TsTap({} dropped)", self.dropped())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;
    use std::time::Duration;

    fn pkts(n: usize) -> Vec<[u8; TS_LEN]> {
        (0..n)
            .map(|k| {
                let mut p = [k as u8; TS_LEN];
                p[0] = 0x47;
                p
            })
            .collect()
    }

    #[test]
    fn udp_sends_seven_packets_per_datagram() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut tx = UdpSink::new(rx.local_addr().unwrap()).unwrap();
        tx.write(&pkts(15));
        let mut buf = [0u8; 2048];
        let (n, _) = rx.recv_from(&mut buf).unwrap();
        assert_eq!(n, 7 * TS_LEN);
        assert_eq!(buf[0], 0x47);
        assert_eq!(buf[TS_LEN + 1], 1);
        assert_eq!(tx.datagrams, 2);
    }

    #[test]
    fn tcp_serves_raw_and_http_clients() {
        let mut sink = TcpSink::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = sink.addr;
        let mut raw = TcpStream::connect(addr).unwrap();
        let mut http = TcpStream::connect(addr).unwrap();
        http.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        // Let both be accepted and classified.
        let t0 = std::time::Instant::now();
        while sink.clients().len() < 2 && t0.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(600));
        sink.write(&pkts(10));

        raw.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut got = vec![0u8; 10 * TS_LEN];
        raw.read_exact(&mut got).unwrap();
        assert_eq!(got[0], 0x47);
        assert_eq!(got[9 * TS_LEN + 1], 9);

        http.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            http.read_exact(&mut b).unwrap();
            head.push(b[0]);
        }
        let head = String::from_utf8(head).unwrap();
        assert!(head.starts_with("HTTP/1.0 200 OK"));
        assert!(head.contains("video/mp2t"));
        let mut got = vec![0u8; TS_LEN];
        http.read_exact(&mut got).unwrap();
        assert_eq!(got[0], 0x47);
    }
}
