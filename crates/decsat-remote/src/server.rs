// made by CasualArclamp
//! The server: accepts connections, runs the handshake, and gives each
//! client either raw IQ or a [`Hub`].
//!
//! Threads for one client: this connection's reader, a writer that drains
//! its [`OutQueue`], and a pump (smart) or IQ loop (dumb) that fills it.

use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use decsat_core::Iq;
use decsat_dsp::Ddc;
use decsat_io::IqSource;

use crate::display::{ROW_FLOOR_DB, ROW_STEP_DB, TraceEncoder};
use crate::hub::{self, Hub, Limits};
use crate::msg::{self, C2S, Hello, IqParams, LinkStats, Mode, S2C, ServerMode, SmartParams};
use crate::outq::{Out, OutQueue};
use crate::radios::{RadioControl, RadioProvider, RadioRequest};
use crate::wire::{self, Kind, MAGIC, PROTO};
use crate::{auth, iq};

/// No frame from a client for this long and it is presumed gone.
const IDLE: Duration = Duration::from_secs(15);
const WRITE_STALL: Duration = Duration::from_secs(20);
/// Socket send buffers: short in smart mode, so the queues rather than the
/// kernel decide what goes next; long for IQ, which only needs throughput.
const SNDBUF_SMART: usize = 512 << 10;
const SNDBUF_DUMB: usize = 8 << 20;
/// Most IQ waiting in the queue, as seconds of stream.
const IQ_QUEUE_SECS: f64 = 0.25;
const PORT_POLL: Duration = Duration::from_millis(100);

/// Shared mode's configuration.
#[derive(Debug, Clone)]
pub struct SharedConfig {
    pub radio: RadioRequest,
    pub params: SmartParams,
    pub max_decoders_each: usize,
    pub max_decoders: usize,
}

/// A server's configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind: String,
    pub name: String,
    /// `None` leaves the server open to whoever can reach the port.
    pub password: Option<String>,
    /// Let a new client that asks for it replace the current one.
    pub allow_takeover: bool,
    pub shared: Option<SharedConfig>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            bind: format!("127.0.0.1:{}", crate::DEFAULT_PORT),
            name: "DecSAT server".into(),
            password: None,
            allow_takeover: false,
            shared: None,
        }
    }
}

/// Who holds an exclusive server.
struct Holder {
    conn: u64,
    name: String,
    stream: TcpStream,
}

struct Common {
    cfg: ServerConfig,
    provider: Arc<dyn RadioProvider>,
    holder: Mutex<Option<Holder>>,
    /// Shared mode's permanent hub.
    shared_hub: Option<Arc<Hub>>,
    shared_control: Option<Arc<dyn RadioControl>>,
    connected: Mutex<Vec<(u64, String)>>,
    next_conn: AtomicU64,
}

/// A running server. Dropping it stops it.
pub struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    common: Arc<Common>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    pub fn start(
        cfg: ServerConfig,
        provider: Arc<dyn RadioProvider>,
    ) -> decsat_core::Result<Server> {
        let err = |e: io::Error| decsat_core::Error::Other(format!("server: {e}"));
        let addr = cfg
            .bind
            .to_socket_addrs()
            .map_err(err)?
            .next()
            .ok_or_else(|| {
                decsat_core::Error::Other(format!("server: bad address {}", cfg.bind))
            })?;
        let listener = TcpListener::bind(addr).map_err(err)?;
        listener.set_nonblocking(true).map_err(err)?;
        let addr = listener.local_addr().map_err(err)?;
        let (shared_hub, shared_control) = match &cfg.shared {
            Some(s) => {
                let radio = provider.open(&s.radio)?;
                let control = radio.control.clone();
                let limits = Limits {
                    per_client: s.max_decoders_each,
                    total: s.max_decoders,
                };
                (
                    Some(Arc::new(Hub::start(radio, s.params, Some(limits)))),
                    control,
                )
            }
            None => (None, None),
        };
        let common = Arc::new(Common {
            cfg,
            provider,
            holder: Mutex::new(None),
            shared_hub,
            shared_control,
            connected: Mutex::new(Vec::new()),
            next_conn: AtomicU64::new(1),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (stop, common) = (Arc::clone(&stop), Arc::clone(&common));
            std::thread::Builder::new()
                .name("remote-accept".into())
                .spawn(move || accept_loop(listener, common, stop))
                .map_err(err)?
        };
        Ok(Server {
            addr,
            stop,
            common,
            thread: Some(thread),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Names of the clients connected now.
    pub fn clients(&self) -> Vec<String> {
        self.common
            .connected
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.1.clone())
            .collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(h) = self.common.holder.lock().unwrap().take() {
            let _ = h.stream.shutdown(Shutdown::Both);
        }
    }
}

fn accept_loop(listener: TcpListener, common: Arc<Common>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                let common = Arc::clone(&common);
                let _ = std::thread::Builder::new()
                    .name("remote-conn".into())
                    .spawn(move || {
                        if let Err(e) = serve(stream, &common) {
                            tracing::debug!("{peer}: {e}");
                        }
                    });
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(PORT_POLL),
            Err(e) => {
                tracing::warn!("accept: {e}");
                std::thread::sleep(PORT_POLL);
            }
        }
    }
}

fn send(w: &mut TcpStream, m: &S2C) -> io::Result<()> {
    wire::write_frame(w, Kind::Control, &msg::encode(m), 0, &mut Vec::new()).map(|_| ())
}

fn read_c2s(r: &mut TcpStream, buf: &mut Vec<u8>, scratch: &mut Vec<u8>) -> io::Result<C2S> {
    let (kind, _) = wire::read_frame(r, buf, scratch)?;
    if kind != Kind::Control {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "expected a control frame",
        ));
    }
    msg::decode(buf).ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "bad message"))
}

fn refuse(w: &mut TcpStream, why: impl Into<String>) -> io::Result<()> {
    let _ = send(w, &S2C::Refused(why.into()));
    // Half-close and wait for the client to hang up: closing outright with
    // its bytes unread would reset the connection and lose the message.
    let _ = w.shutdown(Shutdown::Write);
    let _ = w.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = io::copy(w, &mut io::sink());
    Ok(())
}

/// One client, start to finish.
fn serve(mut stream: TcpStream, common: &Arc<Common>) -> io::Result<()> {
    // A socket accepted from a non-blocking listener may inherit that mode
    // (it does on Windows), and every read would then fail at once.
    stream.set_nonblocking(false)?;
    let _ = stream.set_nodelay(true);
    stream.set_read_timeout(Some(IDLE))?;
    stream.set_write_timeout(Some(WRITE_STALL))?;
    let mut magic = [0u8; 4];
    stream.read_exact(&mut magic)?;
    if magic != MAGIC {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "not a DecSAT client",
        ));
    }
    let (mut buf, mut scratch) = (Vec::new(), Vec::new());
    let C2S::Hello(hello) = read_c2s(&mut stream, &mut buf, &mut scratch)? else {
        return Err(io::Error::new(ErrorKind::InvalidData, "expected hello"));
    };
    let mode = if common.shared_hub.is_some() {
        ServerMode::Shared
    } else {
        ServerMode::Exclusive
    };
    let nonce = common.cfg.password.as_ref().map(|_| auth::nonce());
    send(
        &mut stream,
        &S2C::ServerHello {
            proto: PROTO,
            version: env!("CARGO_PKG_VERSION").into(),
            name: common.cfg.name.clone(),
            mode,
            nonce,
        },
    )?;
    if hello.proto != PROTO {
        return refuse(
            &mut stream,
            format!(
                "protocol {} here, {} there: update one end",
                PROTO, hello.proto
            ),
        );
    }
    if let (Some(pw), Some(server_nonce)) = (&common.cfg.password, &nonce) {
        let good = matches!(
            read_c2s(&mut stream, &mut buf, &mut scratch)?,
            C2S::Auth { client_nonce, proof } if auth::verify(pw, server_nonce, &client_nonce, &proof)
        );
        if !good {
            // Slows a guesser down, and costs an honest typo a second.
            std::thread::sleep(Duration::from_secs(1));
            return refuse(&mut stream, "wrong password");
        }
    }
    let conn = common.next_conn.fetch_add(1, Ordering::Relaxed);
    if mode == ServerMode::Exclusive && !claim(common, conn, &hello, &stream) {
        let who = common
            .holder
            .lock()
            .unwrap()
            .as_ref()
            .map_or(String::new(), |h| h.name.clone());
        return refuse(&mut stream, format!("in use by {who}"));
    }
    common
        .connected
        .lock()
        .unwrap()
        .push((conn, hello.name.clone()));
    let result = run_client(stream, common, conn, hello);
    common.connected.lock().unwrap().retain(|c| c.0 != conn);
    let mut holder = common.holder.lock().unwrap();
    if holder.as_ref().is_some_and(|h| h.conn == conn) {
        *holder = None;
    }
    result
}

/// Take the server for this client, displacing another if allowed.
fn claim(common: &Common, conn: u64, hello: &Hello, stream: &TcpStream) -> bool {
    let mut holder = common.holder.lock().unwrap();
    if let Some(h) = holder.as_ref() {
        if !(hello.takeover && common.cfg.allow_takeover) {
            return false;
        }
        let _ = h.stream.shutdown(Shutdown::Both);
    }
    match stream.try_clone() {
        Ok(s) => {
            *holder = Some(Holder {
                conn,
                name: hello.name.clone(),
                stream: s,
            });
            true
        }
        Err(_) => false,
    }
}

/// What a client's session is running.
enum Session {
    Idle,
    Dumb {
        stop: Arc<AtomicBool>,
        thread: JoinHandle<()>,
    },
    Smart {
        hub: Arc<Hub>,
        stop: Arc<AtomicBool>,
        pump: JoinHandle<()>,
    },
}

impl Session {
    fn end(&mut self, conn: u64) {
        match std::mem::replace(self, Session::Idle) {
            Session::Idle => {}
            Session::Dumb { stop, thread } => {
                stop.store(true, Ordering::Relaxed);
                let _ = thread.join();
            }
            Session::Smart { hub, stop, pump } => {
                stop.store(true, Ordering::Relaxed);
                let _ = pump.join();
                hub.detach(conn);
            }
        }
    }
}

/// Queue a [`LinkStats`] report; `written` is the writer's byte count,
/// which this empties.
pub(crate) fn send_stats(out: &OutQueue, written: &AtomicU64, divider: u32) {
    let c = out.counts();
    let written = written.swap(0, Ordering::Relaxed);
    out.control(msg::encode(&S2C::Stats(LinkStats {
        bytes_per_sec: written as f64,
        iq_dropped: c.iq_dropped,
        ts_dropped: c.ts_dropped,
        rows_thinned: c.rows_thinned,
        row_divider: divider,
    })));
}

fn run_client(
    mut stream: TcpStream,
    common: &Arc<Common>,
    conn: u64,
    hello: Hello,
) -> io::Result<()> {
    let out = Arc::new(OutQueue::default());
    let written = Arc::new(AtomicU64::new(0));
    let zstd = Arc::new(Levels::default());
    let writer = {
        let (out, written, zstd) = (Arc::clone(&out), Arc::clone(&written), Arc::clone(&zstd));
        let s = stream.try_clone()?;
        std::thread::Builder::new()
            .name("remote-write".into())
            .spawn(move || writer_loop(s, &out, &written, &zstd))?
    };
    out.control(msg::encode(&S2C::Ready {
        radios: common.provider.list(),
    }));

    let mut session = Session::Idle;
    let mut control: Option<Arc<dyn RadioControl>> = common.shared_control.clone();
    let (mut buf, mut scratch) = (Vec::new(), Vec::new());
    let result = loop {
        let m = match read_c2s(&mut stream, &mut buf, &mut scratch) {
            Ok(m) => m,
            Err(e) => break Err(e),
        };
        let hub = match &session {
            Session::Smart { hub, .. } => Some(hub),
            _ => None,
        };
        let shared = common.shared_hub.is_some();
        match m {
            C2S::Ping(t) => out.control(msg::encode(&S2C::Pong(t))),
            C2S::Start { radio, mode } => {
                session.end(conn);
                match start(
                    common, conn, &hello, radio, mode, &out, &written, &zstd, &stream,
                ) {
                    Ok((s, c)) => {
                        session = s;
                        if !shared {
                            control = c;
                        }
                    }
                    Err(e) => out.control(msg::encode(&S2C::Stopped(e))),
                }
            }
            C2S::Stop => {
                session.end(conn);
                if !shared {
                    control = None;
                }
                out.control(msg::encode(&S2C::Stopped("stopped".into())));
            }
            C2S::Tune { hz } => {
                if shared {
                    note(&out, "this server's radio is fixed");
                } else if let Some(c) = &control {
                    match c.set_center(hz) {
                        Ok(()) => out.control(msg::encode(&S2C::Tuned { center_hz: hz })),
                        Err(e) => note(&out, &format!("tune: {e}")),
                    }
                }
            }
            C2S::Gain { name, value } => {
                if shared {
                    note(&out, "this server's radio is fixed");
                } else if let Some(c) = &control
                    && let Err(e) = c.set_gain(&name, value)
                {
                    note(&out, &format!("gain: {e}"));
                }
            }
            C2S::SetPaused(p) => {
                if let Some(h) = hub {
                    h.set_paused(p);
                }
            }
            C2S::SetDcRemoval(on) => {
                if let Some(h) = hub {
                    h.set_dc_removal(on);
                }
            }
            C2S::SetFftSize(n) => {
                if let Some(h) = hub {
                    h.set_fft_size(n as usize);
                }
            }
            C2S::AddVfo { id, vfo } => {
                if let Some(h) = hub
                    && let Err(why) = h.add_vfo(conn, id, vfo)
                {
                    out.control(msg::encode(&S2C::VfoGone { id, why }));
                }
            }
            C2S::UpdateVfo {
                id,
                vfo,
                carried_by,
            } => {
                if let Some(h) = hub
                    && let Err(why) = h.update_vfo(conn, id, vfo, carried_by)
                {
                    note(&out, &why);
                }
            }
            C2S::RemoveVfo { id } => {
                if let Some(h) = hub {
                    h.remove_vfo(conn, id);
                    out.forget_vfo(id);
                }
            }
            C2S::Camp { id, server_id } => {
                if let Some(h) = hub {
                    match h.camp(conn, id, server_id) {
                        Ok(vfo) => out.control(msg::encode(&S2C::Camped { id, vfo })),
                        Err(why) => out.control(msg::encode(&S2C::VfoGone { id, why })),
                    }
                }
            }
            C2S::TsPids { id, pids } => {
                if let Some(h) = hub {
                    h.ts_pids(conn, id, &pids);
                }
            }
            C2S::Hello(_) | C2S::Auth { .. } => {
                break Err(io::Error::new(ErrorKind::InvalidData, "handshake repeated"));
            }
        }
    };
    session.end(conn);
    out.close();
    let _ = writer.join();
    let _ = stream.shutdown(Shutdown::Both);
    result
}

fn note(out: &OutQueue, text: &str) {
    out.control(msg::encode(&S2C::Note(text.into())));
}

/// zstd levels the writer uses, set when a session starts.
#[derive(Default)]
struct Levels {
    display: std::sync::atomic::AtomicI32,
    ts: std::sync::atomic::AtomicI32,
    iq: std::sync::atomic::AtomicI32,
}

#[allow(clippy::too_many_arguments)]
fn start(
    common: &Arc<Common>,
    conn: u64,
    hello: &Hello,
    radio: Option<RadioRequest>,
    mode: Mode,
    out: &Arc<OutQueue>,
    written: &Arc<AtomicU64>,
    zstd: &Levels,
    stream: &TcpStream,
) -> Result<(Session, Option<Arc<dyn RadioControl>>), String> {
    let sndbuf = |n: usize| {
        let _ = socket2::SockRef::from(stream).set_send_buffer_size(n);
    };
    let stop = Arc::new(AtomicBool::new(false));
    if let Some(hub) = &common.shared_hub {
        let Mode::Smart(p) = mode else {
            return Err("a shared server only does smart mode".into());
        };
        sndbuf(SNDBUF_SMART);
        zstd.display.store(p.display_zstd, Ordering::Relaxed);
        zstd.ts.store(p.ts_zstd, Ordering::Relaxed);
        hub.attach(conn, hello.name.clone(), Arc::clone(out));
        let f = hub.front();
        out.control(msg::encode(&S2C::Started {
            sample_rate: hub.sample_rate(),
            center_hz: f.center_freq,
            description: f.source,
            gains: Vec::new(),
        }));
        let pump = spawn_pump(
            Arc::clone(hub),
            conn,
            Arc::clone(out),
            p,
            Arc::clone(&stop),
            Arc::clone(written),
        )?;
        return Ok((
            Session::Smart {
                hub: Arc::clone(hub),
                stop,
                pump,
            },
            None,
        ));
    }
    let req = radio.ok_or("no radio asked for")?;
    let opened = common.provider.open(&req).map_err(|e| e.to_string())?;
    let (gains, control) = (opened.gains.clone(), opened.control.clone());
    match mode {
        Mode::Dumb(p) => {
            if !iq::BITS.contains(&p.bits) {
                return Err(format!("{} bits is not offered", p.bits));
            }
            sndbuf(SNDBUF_DUMB);
            zstd.iq.store(p.zstd, Ordering::Relaxed);
            let source = opened.source;
            let in_rate = source.sample_rate();
            let center = source.center_freq();
            let description = source.describe();
            if let Some((off, bw)) = p.slice
                && (bw <= 0.0 || bw > in_rate || off.abs() > in_rate / 2.0)
            {
                return Err("that slice is outside the band".into());
            }
            let ddc = p.slice.map(|(off, bw)| Ddc::new(in_rate, off, bw));
            let (rate, center_out) = match (&ddc, p.slice) {
                (Some(d), Some((off, _))) => (d.out_rate(), center + off),
                _ => (in_rate, center),
            };
            out.control(msg::encode(&S2C::Started {
                sample_rate: rate,
                center_hz: center_out,
                description,
                gains,
            }));
            let thread = {
                let (out, stop, written) =
                    (Arc::clone(out), Arc::clone(&stop), Arc::clone(written));
                std::thread::Builder::new()
                    .name("remote-iq".into())
                    .spawn(move || iq_loop(source, ddc, rate, p, &out, &stop, &written))
                    .map_err(|e| e.to_string())?
            };
            Ok((Session::Dumb { stop, thread }, control))
        }
        Mode::Smart(p) => {
            sndbuf(SNDBUF_SMART);
            zstd.display.store(p.display_zstd, Ordering::Relaxed);
            zstd.ts.store(p.ts_zstd, Ordering::Relaxed);
            let description = opened.source.describe();
            let center = opened.source.center_freq();
            let rate = opened.source.sample_rate();
            let hub = Arc::new(Hub::start(opened, p, None));
            hub.attach(conn, hello.name.clone(), Arc::clone(out));
            out.control(msg::encode(&S2C::Started {
                sample_rate: rate,
                center_hz: center,
                description,
                gains,
            }));
            let pump = spawn_pump(
                Arc::clone(&hub),
                conn,
                Arc::clone(out),
                p,
                Arc::clone(&stop),
                Arc::clone(written),
            )?;
            Ok((Session::Smart { hub, stop, pump }, control))
        }
    }
}

fn spawn_pump(
    hub: Arc<Hub>,
    conn: u64,
    out: Arc<OutQueue>,
    p: SmartParams,
    stop: Arc<AtomicBool>,
    written: Arc<AtomicU64>,
) -> Result<JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name("remote-pump".into())
        .spawn(move || hub::pump(&hub, conn, &out, p, &stop, &written))
        .map_err(|e| e.to_string())
}

/// Dumb mode: read, optionally slice, pack, queue.
fn iq_loop(
    mut source: Box<dyn IqSource>,
    mut ddc: Option<Ddc>,
    rate: f64,
    p: IqParams,
    out: &OutQueue,
    stop: &AtomicBool,
    written: &AtomicU64,
) {
    let frame = ((rate / 50.0) as usize).clamp(1024, 32_768);
    let bytes_per_sec = rate * f64::from(p.bits) / 4.0;
    let cap = ((bytes_per_sec * IQ_QUEUE_SECS) as usize).max(4 << 20);
    let live = source.is_live();
    let mut block: Vec<Iq> = Vec::with_capacity(1 << 18);
    let mut sliced: Vec<Iq> = Vec::new();
    let mut pending: VecDeque<Iq> = VecDeque::new();
    let mut chunk: Vec<Iq> = Vec::with_capacity(frame);
    let mut first = 0u64;
    let mut stats_at = Instant::now();
    let started = Instant::now();
    while !stop.load(Ordering::Relaxed) && !out.is_closed() {
        match source.read(&mut block) {
            Ok(0) => match source.rewind() {
                Ok(true) => continue,
                _ => {
                    out.control(msg::encode(&S2C::Stopped("end of stream".into())));
                    return;
                }
            },
            Ok(_) => {}
            Err(e) => {
                out.control(msg::encode(&S2C::Stopped(e.to_string())));
                return;
            }
        }
        match &mut ddc {
            Some(d) => {
                d.process(&block, &mut sliced);
                pending.extend(sliced.iter().copied());
            }
            None => pending.extend(block.iter().copied()),
        }
        while pending.len() >= frame {
            chunk.clear();
            chunk.extend(pending.drain(..frame));
            let mut f = Vec::new();
            iq::pack(&chunk, first, p.bits, &mut f);
            first += frame as u64;
            out.iq(f, frame as u64, cap);
        }
        if !live {
            // A capture has no clock of its own: keep to real time.
            let ahead = first as f64 / rate - started.elapsed().as_secs_f64();
            if ahead > 0.005 {
                std::thread::sleep(Duration::from_secs_f64(ahead));
            }
        }
        if stats_at.elapsed() >= Duration::from_secs(1) {
            stats_at = Instant::now();
            send_stats(out, written, 1);
        }
    }
}

fn writer_loop(mut stream: TcpStream, out: &OutQueue, written: &AtomicU64, zstd: &Levels) {
    let mut row_enc = TraceEncoder::default();
    let mut spec_enc = TraceEncoder::default();
    let (mut payload, mut scratch) = (Vec::new(), Vec::new());
    while let Some(item) = out.pop() {
        let display = zstd.display.load(Ordering::Relaxed);
        let result = match &item {
            Out::Frame(kind, b) => {
                let level = match kind {
                    Kind::Ts => zstd.ts.load(Ordering::Relaxed),
                    Kind::Iq => zstd.iq.load(Ordering::Relaxed),
                    Kind::Control => 0,
                    _ => display,
                };
                wire::write_frame(&mut stream, *kind, b, level, &mut scratch)
            }
            Out::Row(seq, r) => {
                row_enc.encode(*seq, r, ROW_FLOOR_DB, ROW_STEP_DB, &mut payload);
                wire::write_frame(&mut stream, Kind::Row, &payload, display, &mut scratch)
            }
            Out::Spectrum(seq, t) => {
                spec_enc.encode_spectrum(*seq, t, &mut payload);
                wire::write_frame(&mut stream, Kind::Spectrum, &payload, display, &mut scratch)
            }
        };
        match result {
            Ok(n) => {
                written.fetch_add(n as u64, Ordering::Relaxed);
            }
            Err(_) => break,
        }
    }
    let _ = stream.flush();
    // Wakes the reader, whose read then fails, and with it everything else.
    let _ = stream.shutdown(Shutdown::Both);
    out.close();
}
