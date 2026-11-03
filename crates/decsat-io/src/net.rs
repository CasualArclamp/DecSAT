// made by CasualArclamp
//! IQ over the network: headerless interleaved samples on a TCP stream or in
//! UDP datagrams, as SDR++'s IQ Exporter, GNU Radio's TCP/UDP sinks and many
//! other programs send them. Any radio those programs drive (an SDRplay
//! RSPduo under SDR++, say) can then feed DecSAT, on any OS. For satellite
//! work the sender is tuned to the LNB's IF; the GUI adds the LO.
//! (Copied from DecLAB's `declab_io::net`.)
//!
//! Three ways to meet the sender:
//! - **connect** (`tcp://HOST:PORT`): it listens, we connect. SDR++'s
//!   "TCP (Server)".
//! - **listen** (`listen://ADDR:PORT`): we listen, it connects; when it goes
//!   away the next one may connect. SDR++'s "TCP (Client)".
//! - **UDP** (`udp://ADDR:PORT`): we bind the port and take its datagrams.
//!
//! The stream says nothing about itself, so the rate, the sample type and the
//! centre frequency come from the user; [`NetIqSource::measured_rate`] shows
//! whether the rate given matches what arrives. The one exception is an
//! **rtl_tcp** server: its 12-byte `RTL0` header is recognised on connect,
//! after which the samples are unsigned bytes and [`NetIqControl::tune`]
//! tunes it (commands
//! 0x01 frequency, 0x02 rate, as `rtl_tcp.c` reads them).
//!
//! A reader thread does the socket work and hands buffers over a bounded
//! channel, as the radios' USB readers do; when the consumer falls behind,
//! whole buffers are dropped and counted. While nothing arrives (no sender yet,
//! or a UDP sender paused) a read returns a short block of silence every
//! 100 ms instead of blocking, so the engine stays responsive to its commands.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use decsat_core::{Error, Iq, Result, SampleFormat, bytes_to_iq};

use crate::IqSource;

/// Buffers the reader may queue for the consumer before dropping.
const QUEUE: usize = 64;
/// How often the reader looks at its stop flag, and the consumer waits
/// before handing back silence.
const POLL: Duration = Duration::from_millis(100);
/// Samples of silence a read returns while nothing arrives.
const IDLE_SAMPLES: usize = 256;
/// The longest a connect may take before the sender is called absent.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// How long the connect waits for an rtl_tcp header.
const HEADER_WAIT: Duration = Duration::from_millis(300);
/// A TCP read shorter than this waits [`GATHER`] before the next.
const SHORT_READ: usize = 16_384;
const GATHER: Duration = Duration::from_millis(1);
/// The largest UDP datagram.
const DATAGRAM_MAX: usize = 65_536;

fn net_err(e: impl std::fmt::Display) -> Error {
    Error::Other(format!("network IQ: {e}"))
}

/// How the samples are written on the wire (little-endian, I then Q).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetSample {
    /// Signed 8-bit; SDR++ scales by 128.
    Cs8,
    /// Unsigned 8-bit, offset 128 (rtl_tcp).
    Cu8,
    /// Signed 16-bit; SDR++ scales by 32768.
    Cs16,
    /// Signed 32-bit; SDR++ scales by 2³¹ − 1.
    Cs32,
    /// 32-bit float.
    Cf32,
}

impl NetSample {
    pub const ALL: [NetSample; 5] = [
        NetSample::Cs8,
        NetSample::Cu8,
        NetSample::Cs16,
        NetSample::Cs32,
        NetSample::Cf32,
    ];

    pub const fn bytes_per_sample(self) -> usize {
        match self {
            NetSample::Cs8 | NetSample::Cu8 => 2,
            NetSample::Cs16 => 4,
            NetSample::Cs32 | NetSample::Cf32 => 8,
        }
    }

    /// The name SDR++ and DecSAT's session use.
    pub fn label(self) -> &'static str {
        match self {
            NetSample::Cs8 => "int8",
            NetSample::Cu8 => "uint8",
            NetSample::Cs16 => "int16",
            NetSample::Cs32 => "int32",
            NetSample::Cf32 => "float32",
        }
    }

    /// SDR++'s names (`int8`), the file extensions (`cs8`), or GNU Radio's
    /// (`char`, `short`, `complex`).
    pub fn parse(s: &str) -> Option<NetSample> {
        Some(match s.to_ascii_lowercase().as_str() {
            "int8" | "cs8" | "s8" | "i8" | "char" => NetSample::Cs8,
            "uint8" | "cu8" | "u8" => NetSample::Cu8,
            "int16" | "cs16" | "s16" | "i16" | "short" => NetSample::Cs16,
            "int32" | "cs32" | "s32" | "i32" | "int" => NetSample::Cs32,
            "float32" | "cf32" | "fc32" | "f32" | "float" | "complex" => NetSample::Cf32,
            _ => return None,
        })
    }

    /// Append the samples in `bytes` (whole samples only) to `out`.
    fn convert(self, bytes: &[u8], out: &mut Vec<Iq>, scratch: &mut Vec<Iq>) {
        let fmt = match self {
            NetSample::Cs8 => SampleFormat::Cs8,
            NetSample::Cu8 => SampleFormat::Cu8,
            NetSample::Cs16 => SampleFormat::Cs16,
            NetSample::Cf32 => SampleFormat::Cf32,
            NetSample::Cs32 => {
                const SCALE: f32 = 1.0 / 2_147_483_648.0;
                let (chunks, _rest) = bytes.as_chunks::<8>();
                out.extend(chunks.iter().map(|c| {
                    let i = i32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                    let q = i32::from_le_bytes([c[4], c[5], c[6], c[7]]);
                    Iq::new(i as f32 * SCALE, q as f32 * SCALE)
                }));
                return;
            }
        };
        // Rust note: `bytes_to_iq` clears what it is given, so it fills a
        // scratch vector that is then appended.
        bytes_to_iq(bytes, fmt, scratch);
        out.extend_from_slice(scratch);
    }
}

/// Which side opens the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetMode {
    /// We connect to a TCP server.
    Connect,
    /// We listen; the sender connects.
    Listen,
    /// We bind a UDP port.
    Udp,
}

impl NetMode {
    pub const ALL: [NetMode; 3] = [NetMode::Connect, NetMode::Listen, NetMode::Udp];

    pub fn label(self) -> &'static str {
        match self {
            NetMode::Connect => "TCP connect",
            NetMode::Listen => "TCP listen",
            NetMode::Udp => "UDP",
        }
    }

    fn scheme(self) -> &'static str {
        match self {
            NetMode::Connect => "tcp",
            NetMode::Listen => "listen",
            NetMode::Udp => "udp",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NetIqSettings {
    pub mode: NetMode,
    /// `HOST:PORT` to connect to, or `ADDR:PORT` to bind.
    pub addr: String,
    pub sample: NetSample,
    /// What the sender says it sends, in Hz (the stream cannot say).
    pub sample_rate: f64,
    /// The RF frequency at the middle of the stream, in Hz; 0 if unknown.
    pub center_hz: f64,
}

impl Default for NetIqSettings {
    /// SDR++'s IQ Exporter as it starts: a TCP server on port 7777 (with
    /// "Int8" picked, the cheapest on the wire).
    fn default() -> Self {
        NetIqSettings {
            mode: NetMode::Connect,
            addr: "127.0.0.1:7777".into(),
            sample: NetSample::Cs8,
            sample_rate: 2e6,
            center_hz: 0.0,
        }
    }
}

impl NetIqSettings {
    /// `tcp://HOST:PORT`, `listen://ADDR:PORT` or `udp://ADDR:PORT` (a bare
    /// `HOST:PORT` connects) into the mode and address.
    pub fn parse_url(url: &str) -> Result<(NetMode, String)> {
        let (mode, rest) = match url.split_once("://") {
            None => (NetMode::Connect, url),
            Some((scheme, rest)) => {
                let mode = match scheme.to_ascii_lowercase().as_str() {
                    "tcp" => NetMode::Connect,
                    "listen" | "tcp-listen" | "tcpl" => NetMode::Listen,
                    "udp" => NetMode::Udp,
                    other => {
                        return Err(net_err(format!(
                            "unknown scheme {other:?} (tcp://, listen:// or udp://)"
                        )));
                    }
                };
                (mode, rest)
            }
        };
        let rest = rest.trim_end_matches('/');
        if rest
            .rsplit_once(':')
            .is_none_or(|(_, p)| p.parse::<u16>().is_err())
        {
            return Err(net_err(format!("{rest:?} needs a :PORT")));
        }
        Ok((mode, rest.to_string()))
    }

    /// The settings as a URL, for a status line.
    pub fn url(&self) -> String {
        format!("{}://{}", self.mode.scheme(), self.addr)
    }
}

/// Check what can be wrong before any socket is opened.
pub fn check_settings(s: &NetIqSettings) -> Result<()> {
    if !(1e3..=100e6).contains(&s.sample_rate) {
        return Err(net_err(format!(
            "give the sender's sample rate (1 kS/s to 100 MS/s), not {:.0}",
            s.sample_rate
        )));
    }
    NetIqSettings::parse_url(&s.addr).map(|_| ())
}

fn resolve(addr: &str) -> Result<SocketAddr> {
    addr.to_socket_addrs()
        .map_err(|e| net_err(format!("{addr}: {e}")))?
        .next()
        .ok_or_else(|| net_err(format!("{addr}: no address")))
}

/// What the reader thread tells the consumer, besides samples.
#[derive(Default)]
struct Status {
    /// "listening", "connected to …", "ended"…
    state: String,
    /// Set when the reader gave up; the next read returns it.
    error: Option<String>,
}

struct Shared {
    stop: AtomicBool,
    bytes: AtomicU64,
    overflows: AtomicU64,
    status: Mutex<Status>,
    /// An rtl_tcp server's command socket and its tuner's name.
    rtl_tcp: Mutex<Option<(TcpStream, &'static str)>>,
    /// The centre, Hz, as `f64` bits (it moves when rtl_tcp is tuned).
    center_bits: AtomicU64,
    /// Set by a tune; the source then drops what came before it.
    retuned: AtomicBool,
}

impl Shared {
    fn set_state(&self, s: impl Into<String>) {
        self.status.lock().unwrap().state = s.into();
    }
}

/// The open socket, as the reader thread holds it.
enum Socket {
    Tcp(TcpStream),
    Listen(TcpListener),
    Udp(UdpSocket),
}

struct Reader {
    socket: Socket,
    /// Bytes already read with the rtl_tcp check, sent first.
    first: Vec<u8>,
    /// Bytes per buffer handed over: about 10 ms of signal.
    chunk: usize,
    tx: SyncSender<Vec<u8>>,
    recycled: Receiver<Vec<u8>>,
    shared: Arc<Shared>,
}

impl Reader {
    fn buffer(&self) -> Vec<u8> {
        let mut b = self.recycled.try_recv().unwrap_or_default();
        b.clear();
        b.reserve(self.chunk);
        b
    }

    /// Hand a buffer over, or drop it if the consumer is behind. `false`
    /// once the consumer has gone.
    fn send(&self, buf: Vec<u8>) -> bool {
        self.shared
            .bytes
            .fetch_add(buf.len() as u64, Ordering::Relaxed);
        match self.tx.try_send(buf) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.shared.overflows.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    fn stopping(&self) -> bool {
        self.shared.stop.load(Ordering::Relaxed)
    }

    fn run(mut self) {
        let first = std::mem::take(&mut self.first);
        if !first.is_empty() && !self.send(first) {
            return;
        }
        let socket = std::mem::replace(&mut self.socket, Socket::Udp(dummy_udp()));
        let result = match socket {
            Socket::Tcp(s) => self.stream(s).map(|_| ()),
            Socket::Listen(l) => self.listen(l),
            Socket::Udp(u) => self.udp(u),
        };
        let mut st = self.shared.status.lock().unwrap();
        match result {
            Ok(()) => st.state = "ended".into(),
            Err(e) => {
                st.state = "failed".into();
                st.error = Some(e.to_string());
            }
        }
        // Dropping `tx` here tells the consumer the stream is over.
    }

    /// Read one TCP connection to its end. `Ok(true)` if it was stopped.
    ///
    /// Senders write small packets (SDR++: 1024 bytes by default) and a read
    /// returns about one, so each buffer is filled to `chunk` before it goes:
    /// a block per packet swamped the decoders' queues. A pause sends what
    /// there is.
    fn stream(&self, mut s: TcpStream) -> Result<bool> {
        s.set_read_timeout(Some(POLL)).map_err(net_err)?;
        let mut buf = self.buffer();
        loop {
            if self.stopping() {
                return Ok(true);
            }
            let have = buf.len();
            buf.resize(self.chunk, 0);
            let r = s.read(&mut buf[have..]);
            buf.truncate(have + *r.as_ref().unwrap_or(&0));
            let flush = match r {
                Ok(0) => {
                    if !buf.is_empty() {
                        self.send(buf);
                    }
                    return Ok(false);
                }
                Ok(n) => {
                    // A short read means the socket was all but empty: wait
                    // for more to gather rather than make a call per packet
                    // (at 10 MS/s that halved the reader's CPU).
                    if n < SHORT_READ {
                        std::thread::sleep(GATHER);
                    }
                    buf.len() >= self.chunk
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    !buf.is_empty()
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => false,
                Err(e) => return Err(net_err(format!("the connection failed: {e}"))),
            };
            if flush {
                if !self.send(std::mem::take(&mut buf)) {
                    return Ok(true);
                }
                buf = self.buffer();
            }
        }
    }

    /// Accept senders one after another until stopped.
    fn listen(&self, l: TcpListener) -> Result<()> {
        l.set_nonblocking(true).map_err(net_err)?;
        loop {
            if self.stopping() {
                return Ok(());
            }
            match l.accept() {
                Ok((s, peer)) => {
                    // Accepted sockets may inherit the listener's
                    // non-blocking mode (they do on Windows).
                    s.set_nonblocking(false).map_err(net_err)?;
                    self.shared.set_state(format!("connected from {peer}"));
                    if self.stream(s)? {
                        return Ok(());
                    }
                    self.shared
                        .set_state(format!("{peer} went away; listening"));
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(POLL / 4),
                Err(e) => return Err(net_err(format!("accept failed: {e}"))),
            }
        }
    }

    /// Datagrams, gathered into buffers of about `chunk` bytes.
    fn udp(&self, u: UdpSocket) -> Result<()> {
        u.set_read_timeout(Some(POLL)).map_err(net_err)?;
        let mut dgram = vec![0u8; DATAGRAM_MAX];
        let mut buf = self.buffer();
        let mut heard = false;
        loop {
            if self.stopping() {
                return Ok(());
            }
            match u.recv_from(&mut dgram) {
                Ok((n, peer)) => {
                    if !heard {
                        heard = true;
                        self.shared.set_state(format!("receiving from {peer}"));
                    }
                    buf.extend_from_slice(&dgram[..n]);
                    if buf.len() >= self.chunk {
                        if !self.send(std::mem::take(&mut buf)) {
                            return Ok(());
                        }
                        buf = self.buffer();
                    }
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    // A pause: hand over what there is.
                    if !buf.is_empty() {
                        if !self.send(std::mem::take(&mut buf)) {
                            return Ok(());
                        }
                        buf = self.buffer();
                    }
                }
                // Windows reports an ICMP "port unreachable" from an earlier
                // send as a reset on the next receive; it means nothing here.
                Err(e) if e.kind() == ErrorKind::ConnectionReset => {}
                Err(e) => return Err(net_err(format!("UDP receive failed: {e}"))),
            }
        }
    }
}

/// A placeholder for `mem::replace`: a socket that is never used.
fn dummy_udp() -> UdpSocket {
    UdpSocket::bind("127.0.0.1:0").expect("a loopback UDP socket")
}

/// An rtl_tcp server's command: one byte, then a big-endian `u32`.
fn rtl_tcp_command(cmd: u8, value: u32) -> [u8; 5] {
    let v = value.to_be_bytes();
    [cmd, v[0], v[1], v[2], v[3]]
}

/// What rtl_tcp's header says the dongle's tuner is (`rtlsdr_tuner` order).
fn rtl_tuner_name(t: u32) -> &'static str {
    match t {
        1 => "E4000",
        2 => "FC0012",
        3 => "FC0013",
        4 => "FC2580",
        5 => "R820T",
        6 => "R828D",
        _ => "unknown tuner",
    }
}

/// Statistics a status line or the GUI can show.
#[derive(Clone)]
pub struct NetIqControl {
    shared: Arc<Shared>,
    started: Instant,
    bytes_per_sample: usize,
}

impl NetIqControl {
    /// Bytes received since the start.
    pub fn bytes(&self) -> u64 {
        self.shared.bytes.load(Ordering::Relaxed)
    }

    /// Buffers dropped because the consumer fell behind.
    pub fn overflows(&self) -> u64 {
        self.shared.overflows.load(Ordering::Relaxed)
    }

    /// "connected to …", "listening on …", "ended".
    pub fn state(&self) -> String {
        self.shared.status.lock().unwrap().state.clone()
    }

    /// Whether the sender is an rtl_tcp server, so [`NetIqControl::tune`]
    /// moves it.
    pub fn is_rtl_tcp(&self) -> bool {
        self.shared.rtl_tcp.lock().unwrap().is_some()
    }

    /// Tune an rtl_tcp server to `hz`. `Ok(false)` for any other sender: it
    /// is tuned at its own end. What arrives before the tune took effect is
    /// thrown away by the source.
    pub fn tune(&self, hz: f64) -> Result<bool> {
        let mut cmd = self.shared.rtl_tcp.lock().unwrap();
        let Some((s, _)) = cmd.as_mut() else {
            return Ok(false);
        };
        s.write_all(&rtl_tcp_command(0x01, hz.round() as u32))
            .map_err(net_err)?;
        self.shared
            .center_bits
            .store(hz.round().to_bits(), Ordering::Relaxed);
        self.shared.retuned.store(true, Ordering::Release);
        Ok(true)
    }

    /// The rate the samples arrive at, averaged since the start; `None` in
    /// the first two seconds (a TCP stream starts with a burst of whatever
    /// was buffered).
    pub fn measured_rate(&self) -> Option<f64> {
        let t = self.started.elapsed().as_secs_f64();
        (t >= 2.0).then(|| self.bytes() as f64 / self.bytes_per_sample as f64 / t)
    }
}

/// Network IQ, from the moment the socket is open.
pub struct NetIqSource {
    settings: NetIqSettings,
    rx: Receiver<Vec<u8>>,
    recycle: mpsc::Sender<Vec<u8>>,
    control: NetIqControl,
    thread: Option<JoinHandle<()>>,
    /// Bytes of a sample split across two buffers.
    carry: Vec<u8>,
    scratch: Vec<Iq>,
    /// Buffers still to be thrown away after a retune.
    skip: u32,
    ended: bool,
}

impl NetIqSource {
    /// Open the socket (connect, listen or bind) and start the reader. A
    /// connect fails at once if nothing is listening; listen and UDP wait for
    /// a sender, handing out silence until one comes.
    pub fn open(settings: NetIqSettings) -> Result<Self> {
        check_settings(&settings)?;
        let mut settings = settings;
        let mut first = Vec::new();
        let mut rtl_tcp = None;
        let (socket, state) = match settings.mode {
            NetMode::Connect => {
                let addr = resolve(&settings.addr)?;
                let mut s = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| {
                    net_err(format!(
                        "nothing to connect to at {addr} ({e}). Is the sender running, \
                         as a TCP server on that port?"
                    ))
                })?;
                // An rtl_tcp server greets with its header at once; a plain
                // sender starts with samples (kept, not lost).
                s.set_read_timeout(Some(HEADER_WAIT)).map_err(net_err)?;
                let deadline = Instant::now() + HEADER_WAIT;
                let mut head = [0u8; 12];
                let mut got = 0;
                while got < head.len() && Instant::now() < deadline {
                    match s.read(&mut head[got..]) {
                        Ok(0) => break,
                        Ok(n) => got += n,
                        Err(e)
                            if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                        {
                            break;
                        }
                        Err(e) => return Err(net_err(e)),
                    }
                    if got >= 4 && &head[..4] != b"RTL0" {
                        break;
                    }
                }
                if got == head.len() && &head[..4] == b"RTL0" {
                    let tuner = u32::from_be_bytes([head[4], head[5], head[6], head[7]]);
                    settings.sample = NetSample::Cu8;
                    let mut cmd = s.try_clone().map_err(net_err)?;
                    cmd.write_all(&rtl_tcp_command(0x02, settings.sample_rate as u32))
                        .and_then(|()| {
                            if settings.center_hz > 0.0 {
                                cmd.write_all(&rtl_tcp_command(0x01, settings.center_hz as u32))
                            } else {
                                Ok(())
                            }
                        })
                        .map_err(net_err)?;
                    rtl_tcp = Some((cmd, rtl_tuner_name(tuner)));
                } else {
                    first.extend_from_slice(&head[..got]);
                }
                (Socket::Tcp(s), format!("connected to {addr}"))
            }
            NetMode::Listen => {
                let l = TcpListener::bind(&settings.addr)
                    .map_err(|e| net_err(format!("cannot listen on {}: {e}", settings.addr)))?;
                let local = l.local_addr().map_err(net_err)?;
                settings.addr = local.to_string();
                (Socket::Listen(l), format!("listening on {local}"))
            }
            NetMode::Udp => {
                let u = UdpSocket::bind(&settings.addr)
                    .map_err(|e| net_err(format!("cannot bind UDP {}: {e}", settings.addr)))?;
                let local = u.local_addr().map_err(net_err)?;
                settings.addr = local.to_string();
                (Socket::Udp(u), format!("waiting on UDP {local}"))
            }
        };

        let bps = settings.sample.bytes_per_sample();
        // About 10 ms a buffer, whole samples, 4 KiB to 256 KiB.
        let chunk =
            ((settings.sample_rate / 100.0) as usize * bps).clamp(4096, 262_144) / bps * bps;
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            bytes: AtomicU64::new(0),
            overflows: AtomicU64::new(0),
            status: Mutex::new(Status { state, error: None }),
            center_bits: AtomicU64::new(settings.center_hz.to_bits()),
            rtl_tcp: Mutex::new(rtl_tcp),
            retuned: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let (recycle, recycled) = mpsc::channel();
        let reader = Reader {
            socket,
            first,
            chunk,
            tx,
            recycled,
            shared: Arc::clone(&shared),
        };
        let thread = std::thread::Builder::new()
            .name("net-iq-rx".into())
            .spawn(move || reader.run())
            .map_err(|e| net_err(format!("cannot start the reader: {e}")))?;
        Ok(NetIqSource {
            control: NetIqControl {
                shared,
                started: Instant::now(),
                bytes_per_sample: bps,
            },
            settings,
            rx,
            recycle,
            thread: Some(thread),
            carry: Vec::new(),
            scratch: Vec::new(),
            skip: 0,
            ended: false,
        })
    }

    pub fn control(&self) -> NetIqControl {
        self.control.clone()
    }

    pub fn settings(&self) -> &NetIqSettings {
        &self.settings
    }

    /// Whether the sender turned out to be an rtl_tcp server.
    pub fn is_rtl_tcp(&self) -> bool {
        self.control.is_rtl_tcp()
    }

    /// Turn a buffer into samples after what was carried over.
    fn take(&mut self, bytes: &[u8], out: &mut Vec<Iq>) {
        let bps = self.settings.sample.bytes_per_sample();
        let mut bytes = bytes;
        if !self.carry.is_empty() {
            let need = (bps - self.carry.len()).min(bytes.len());
            self.carry.extend_from_slice(&bytes[..need]);
            bytes = &bytes[need..];
            if self.carry.len() == bps {
                let carry = std::mem::take(&mut self.carry);
                self.settings.sample.convert(&carry, out, &mut self.scratch);
                self.carry = carry;
                self.carry.clear();
            }
        }
        let whole = bytes.len() / bps * bps;
        self.settings
            .sample
            .convert(&bytes[..whole], out, &mut self.scratch);
        self.carry.extend_from_slice(&bytes[whole..]);
    }
}

impl IqSource for NetIqSource {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        out.clear();
        if self.ended {
            return Ok(0);
        }
        // Tuned since the last read: what is queued is the old frequency's,
        // and a few buffers more (the dongle's PLL settles, and what was in
        // flight arrives).
        if self.control.shared.retuned.swap(false, Ordering::Acquire) {
            while let Ok(b) = self.rx.try_recv() {
                let _ = self.recycle.send(b);
            }
            self.carry.clear();
            self.skip = 4;
        }
        loop {
            match self.rx.recv_timeout(POLL) {
                Ok(bytes) => {
                    if self.skip > 0 {
                        self.skip -= 1;
                    } else {
                        self.take(&bytes, out);
                    }
                    let _ = self.recycle.send(bytes);
                    if !out.is_empty() {
                        return Ok(out.len());
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    // Nothing yet: silence, so the engine gets its turn.
                    out.resize(IDLE_SAMPLES, Iq::new(0.0, 0.0));
                    return Ok(out.len());
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.ended = true;
                    let error = self.control.shared.status.lock().unwrap().error.take();
                    return match error {
                        Some(e) => Err(Error::Other(e)),
                        None => Ok(0),
                    };
                }
            }
        }
    }

    fn sample_rate(&self) -> f64 {
        self.settings.sample_rate
    }

    fn center_freq(&self) -> f64 {
        f64::from_bits(self.control.shared.center_bits.load(Ordering::Relaxed))
    }

    fn describe(&self) -> String {
        let tuner = self
            .control
            .shared
            .rtl_tcp
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, t)| *t);
        let what = match tuner {
            Some(tuner) => format!("rtl_tcp ({tuner})"),
            None => format!("network IQ {}", self.settings.sample.label()),
        };
        let mut s = format!(
            "{what} {} — {}, {:.3} MS/s",
            self.settings.url(),
            self.control.state(),
            self.settings.sample_rate / 1e6
        );
        if let Some(m) = self.control.measured_rate() {
            s += &format!(" (arriving at {:.3})", m / 1e6);
        }
        let o = self.control.overflows();
        if o > 0 {
            s += &format!(", {o} buffers dropped");
        }
        s
    }

    fn is_live(&self) -> bool {
        true
    }
}

impl Drop for NetIqSource {
    fn drop(&mut self) {
        self.control.shared.stop.store(true, Ordering::Relaxed);
        // The reader looks at the flag every 100 ms; it may be waiting to
        // hand over a buffer, so the queue is emptied first.
        while self.rx.try_recv().is_ok() {}
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(mode: NetMode, addr: &str, sample: NetSample) -> NetIqSettings {
        NetIqSettings {
            mode,
            addr: addr.into(),
            sample,
            sample_rate: 1e6,
            center_hz: 100e6,
        }
    }

    /// Reads until `n` real samples have come (silence skipped) or 3 s pass.
    fn read_n(src: &mut NetIqSource, n: usize) -> Vec<Iq> {
        let mut got = Vec::new();
        let mut buf = Vec::new();
        let t0 = Instant::now();
        while got.len() < n && t0.elapsed() < Duration::from_secs(3) {
            let k = src.read(&mut buf).unwrap();
            if k == 0 {
                break;
            }
            if !(k == IDLE_SAMPLES && buf.iter().all(|s| s.norm_sqr() == 0.0)) {
                got.extend_from_slice(&buf);
            }
        }
        got
    }

    /// cs16 samples 0..n as (i, -i) / 32768.
    fn cs16_ramp(n: i16) -> Vec<u8> {
        (0..n)
            .flat_map(|i| [i.to_le_bytes(), (-i).to_le_bytes()].concat())
            .collect()
    }

    #[test]
    fn urls() {
        assert_eq!(
            NetIqSettings::parse_url("127.0.0.1:7777").unwrap(),
            (NetMode::Connect, "127.0.0.1:7777".into())
        );
        assert_eq!(
            NetIqSettings::parse_url("udp://0.0.0.0:7777").unwrap().0,
            NetMode::Udp
        );
        assert_eq!(
            NetIqSettings::parse_url("listen://0.0.0.0:1234/")
                .unwrap()
                .0,
            NetMode::Listen
        );
        assert!(NetIqSettings::parse_url("localhost").is_err());
        assert!(NetIqSettings::parse_url("http://a:1").is_err());
        assert_eq!(NetSample::parse("Int16"), Some(NetSample::Cs16));
        assert_eq!(NetSample::parse("complex"), Some(NetSample::Cf32));
    }

    #[test]
    fn cs32_scales_like_sdrpp() {
        let mut out = Vec::new();
        let mut scratch = Vec::new();
        let bytes = [i32::MIN.to_le_bytes(), (1i32 << 30).to_le_bytes()].concat();
        NetSample::Cs32.convert(&bytes, &mut out, &mut scratch);
        assert_eq!(out, vec![Iq::new(-1.0, 0.5)]);
    }

    #[test]
    fn connect_reads_a_tcp_stream_split_mid_sample() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let bytes = cs16_ramp(1000);
        let send = bytes.clone();
        let server = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            // Odd-sized writes, so samples straddle the reads.
            for c in send.chunks(333) {
                s.write_all(c).unwrap();
                s.flush().unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let mut src =
            NetIqSource::open(settings(NetMode::Connect, &addr, NetSample::Cs16)).unwrap();
        assert!(!src.is_rtl_tcp());
        assert!(!src.control().tune(1e9).unwrap());
        let got = read_n(&mut src, 1000);
        server.join().unwrap();
        assert_eq!(got.len(), 1000);
        for (i, s) in got.iter().enumerate() {
            assert_eq!(*s, Iq::new(i as f32 / 32768.0, -(i as f32) / 32768.0));
        }
        // The sender closed: the stream ends.
        let mut buf = Vec::new();
        assert_eq!(src.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn connect_fails_at_once_with_nothing_listening() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let e = NetIqSource::open(settings(
            NetMode::Connect,
            &format!("127.0.0.1:{port}"),
            NetSample::Cs8,
        ))
        .err()
        .expect("nothing listens");
        assert!(e.to_string().contains("nothing to connect to"), "{e}");
    }

    #[test]
    fn listen_takes_one_sender_after_another() {
        let mut src =
            NetIqSource::open(settings(NetMode::Listen, "127.0.0.1:0", NetSample::Cs8)).unwrap();
        let addr = src.settings().addr.clone();
        // Silence while nobody has connected.
        let mut buf = Vec::new();
        assert_eq!(src.read(&mut buf).unwrap(), IDLE_SAMPLES);
        for round in 0..2i8 {
            let mut c = TcpStream::connect(&addr).unwrap();
            c.write_all(&[round as u8, 64, (-64i8) as u8, 127]).unwrap();
            drop(c);
            let got = read_n(&mut src, 2);
            assert_eq!(
                got,
                vec![
                    Iq::new(round as f32 / 128.0, 0.5),
                    Iq::new(-0.5, 127.0 / 128.0)
                ]
            );
        }
    }

    #[test]
    fn udp_datagrams() {
        let mut src =
            NetIqSource::open(settings(NetMode::Udp, "127.0.0.1:0", NetSample::Cf32)).unwrap();
        let addr = src.settings().addr.clone();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut sent = Vec::new();
        for k in 0..20 {
            let pkt: Vec<u8> = (0..128)
                .flat_map(|j| {
                    let v = (k * 128 + j) as f32;
                    [v.to_le_bytes(), (-v).to_le_bytes()].concat()
                })
                .collect();
            tx.send_to(&pkt, &addr).unwrap();
            sent.extend((0..128).map(|j| {
                let v = (k * 128 + j) as f32;
                Iq::new(v, -v)
            }));
        }
        let got = read_n(&mut src, sent.len());
        assert_eq!(got, sent);
        assert!(
            src.describe().contains("receiving from"),
            "{}",
            src.describe()
        );
    }

    #[test]
    fn rtl_tcp_is_recognised_and_tuned() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut head = b"RTL0".to_vec();
            head.extend_from_slice(&5u32.to_be_bytes());
            head.extend_from_slice(&29u32.to_be_bytes());
            s.write_all(&head).unwrap();
            let mut cmds = [0u8; 15];
            s.read_exact(&mut cmds).unwrap();
            s.write_all(&[255, 0, 128, 128]).unwrap();
            cmds
        });
        // Asked for cs16, but rtl_tcp sends unsigned bytes.
        let src = NetIqSource::open(settings(NetMode::Connect, &addr, NetSample::Cs16)).unwrap();
        assert!(src.is_rtl_tcp());
        assert!(src.describe().contains("R820T"), "{}", src.describe());
        assert!(src.control().tune(153_350_000.0).unwrap());
        let cmds = server.join().unwrap();
        assert_eq!(&cmds[..5], &rtl_tcp_command(0x02, 1_000_000));
        assert_eq!(&cmds[5..10], &rtl_tcp_command(0x01, 100_000_000));
        assert_eq!(&cmds[10..], &rtl_tcp_command(0x01, 153_350_000));
        assert_eq!(src.center_freq(), 153_350_000.0);
    }

    #[test]
    fn dropping_a_waiting_source_is_quick() {
        let src = NetIqSource::open(settings(NetMode::Udp, "127.0.0.1:0", NetSample::Cs8)).unwrap();
        let t0 = Instant::now();
        drop(src);
        assert!(t0.elapsed() < Duration::from_millis(500));
    }
}
