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
    mode: TcpMode,
    seen: u32,
    /// The packets seen while looking, sent on once the mode is known.
    held: Vec<[u8; TS_LEN]>,
}

/// Packets the sink looks at for a PMT before it gives up and sends the TS
/// as it is (a stream with none, or one this analyser cannot read).
const LOOK_LIMIT: u32 = 4000;

/// What a TCP sink sends. VLC and PotPlayer cannot read AV1 in a TS (AOM's
/// mapping, crate::av1; they show a private data stream), so for an AV1
/// video the stream is the IVF they do play, made from the units: the
/// picture only, as the IVF holds no sound.
enum TcpMode {
    /// Watching for the PMT to say what the video is; nothing goes out
    /// meanwhile (a second or so), as the stream may turn out to be IVF.
    Looking(Box<TsAnalyser>),
    /// Anything else: the TS as it is.
    Ts,
    Av1 {
        pid: u16,
        ex: Av1Extractor,
        last_pts: u64,
        started: bool,
    },
}

impl TcpSink {
    /// Listen on `addr` (port 0 picks a free one; see `addr` after).
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let server = StreamServer::bind(addr, "video/mp2t")?;
        Ok(TcpSink {
            addr: server.addr,
            server,
            mode: TcpMode::Looking(Box::new(TsAnalyser::new())),
            seen: 0,
            held: Vec::new(),
        })
    }

    /// Addresses of the players connected now.
    pub fn clients(&self) -> Vec<SocketAddr> {
        self.server.clients()
    }

    pub fn write(&mut self, packets: &[[u8; TS_LEN]]) {
        if packets.is_empty() {
            return;
        }
        if let TcpMode::Looking(an) = &mut self.mode {
            for p in packets {
                an.packet(p);
            }
            self.held.extend_from_slice(packets);
            self.seen += packets.len() as u32;
            let mut next = None;
            if self.held.len() as u32 > LOOK_LIMIT {
                next = Some(TcpMode::Ts);
            } else if self.seen >= 64 {
                self.seen = 0;
                let r = an.report();
                let streams: Vec<_> = r.programmes.iter().flat_map(|q| q.streams.iter()).collect();
                if let Some(e) = streams.iter().find(|e| e.kind == Some("AV1 video")) {
                    next = Some(TcpMode::Av1 {
                        pid: e.pid,
                        ex: Av1Extractor::new(),
                        last_pts: 0,
                        started: false,
                    });
                } else if !streams.is_empty() {
                    next = Some(TcpMode::Ts);
                }
            }
            let Some(next) = next else { return };
            // What was held while looking goes out first, in the mode found.
            self.mode = next;
            let held = std::mem::take(&mut self.held);
            self.send(&held);
            return;
        }
        self.send(packets);
    }

    /// `packets` to the clients in the mode found.
    fn send(&mut self, packets: &[[u8; TS_LEN]]) {
        match &mut self.mode {
            TcpMode::Looking(_) => {}
            TcpMode::Ts => self.server.write(packets.concat()),
            TcpMode::Av1 {
                pid,
                ex,
                last_pts,
                started,
            } => {
                let mut out = Vec::new();
                for p in packets {
                    if u16::from_be_bytes([p[1] & 0x1F, p[2]]) != *pid {
                        continue;
                    }
                    let Some(u) = ex.push(p) else { continue };
                    if !*started {
                        // The header needs the size: from the first unit
                        // with a sequence header (what a decoder starts at).
                        let Some(seq) = find_sequence_header(&u.tu) else {
                            continue;
                        };
                        self.server.set_prelude(
                            "video/x-ivf",
                            ivf_header(seq.width as u16, seq.height as u16).to_vec(),
                        );
                        *started = true;
                    }
                    // A unit with no PTS keeps the last one's.
                    let pts = u.pts.unwrap_or(*last_pts);
                    *last_pts = pts;
                    ivf_frame(pts, &u.tu, &mut out);
                }
                self.server.write(out);
            }
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
        // No PMT in it: the sink looks for one a while, then sends the TS.
        sink.write(&pkts(LOOK_LIMIT as usize + 1));
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

    /// AV1 in AOM's mapping comes out of the TCP sink as an IVF, which
    /// ffmpeg reads. Skipped without an ffmpeg that has SVT-AV1.
    #[test]
    fn tcp_serves_av1_as_ivf() {
        use std::process::{Command, Stdio};
        let tmp = std::env::temp_dir();
        let ts = tmp.join(format!("decsat-sink-av1-{}.ts", std::process::id()));
        let made = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x180:rate=25",
            ])
            .args(["-t", "4", "-c:v", "libsvtav1", "-preset", "12"])
            .args([
                "-svtav1-params",
                "pred-struct=1",
                "-g",
                "25",
                "-f",
                "mpegts",
            ])
            .arg(&ts)
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("no SVT-AV1 in this ffmpeg: skipped");
            return;
        }
        let data = std::fs::read(&ts).unwrap();
        let _ = std::fs::remove_file(&ts);
        // ffmpeg's TS to AOM's mapping, as the transmitter makes it.
        let mut fx = crate::av1::Av1TsFixer::new(0x1000, 0x100, 1e6);
        let mut mapped: Vec<[u8; TS_LEN]> = Vec::new();
        let (chunks, _) = data.as_chunks::<TS_LEN>();
        for c in chunks {
            fx.push(c, &mut mapped);
        }
        let mut sink = TcpSink::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let mut http = TcpStream::connect(sink.addr).unwrap();
        http.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let t0 = std::time::Instant::now();
        while sink.clients().is_empty() && t0.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        for batch in mapped.chunks(200) {
            sink.write(batch);
        }
        drop(sink);
        http.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut all = Vec::new();
        let _ = http.read_to_end(&mut all);
        let at = all.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&all[..at]).to_string();
        assert!(head.contains("video/x-ivf"), "{head}");
        let body = &all[at..];
        assert_eq!(&body[..4], b"DKIF");
        let mut ff = Command::new("ffmpeg")
            .args(["-v", "error", "-f", "ivf", "-i", "-", "-f", "null", "-"])
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        ff.stdin.take().unwrap().write_all(body).unwrap();
        let out = ff.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
