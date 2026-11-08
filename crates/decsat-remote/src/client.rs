// made by CasualArclamp
//! The client end. Dumb mode looks like any other radio to the engine
//! ([`RemoteIqSource`]); smart mode looks like the engine itself
//! ([`RemoteEngine`] has the same methods as `decsat_engine::Engine`), so
//! the GUI's waterfall and side panel need not know the difference.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use decsat_core::{Error, Iq, Result};
use decsat_engine::fec::{FecStats, TsView};
use decsat_engine::frontend::{FrontStatus, SourceState};
use decsat_engine::vfo::{DecoderKind, VfoId, VfoSettings, VfoStatus};
use decsat_io::IqSource;
use decsat_ts::{TS_LEN, TcpSink, TsAnalyser, TsFile, UdpSink};

use crate::display::TraceDecoder;
use crate::msg::{
    self, C2S, FecWire, FrontWire, Hello, IqParams, LinkStats, Mode, S2C, ServerMode, SharedVfo,
    SmartParams, VfoRequest, VfoWire,
};
use crate::radios::{Gain, RadioInfo, RadioRequest};
use crate::wire::{self, Kind, MAGIC, PROTO};
use crate::{auth, iq, ts};

const CONNECT: Duration = Duration::from_secs(5);
/// How long the server may take to open a radio.
const START: Duration = Duration::from_secs(15);
/// No frame from the server for this long and the link is presumed dead.
const IDLE: Duration = Duration::from_secs(15);
const PING: Duration = Duration::from_secs(2);
/// IQ blocks the reader may queue for the engine before dropping.
const IQ_QUEUE: usize = 256;
/// Waterfall rows kept for the GUI, as the engine keeps.
const ROW_BACKLOG: usize = 128;
const IDLE_SAMPLES: usize = 256;

fn err(e: impl std::fmt::Display) -> Error {
    Error::Other(format!("remote: {e}"))
}

/// How to reach a server.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// `HOST` or `HOST:PORT`.
    pub addr: String,
    /// Shown to other users on a shared server.
    pub name: String,
    pub password: Option<String>,
    /// Replace whoever holds the server, if it allows that.
    pub takeover: bool,
}

/// What a server said about itself.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub mode: ServerMode,
    pub radios: Vec<RadioInfo>,
}

fn resolve(addr: &str) -> Result<SocketAddr> {
    let with_port = if addr.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:{}", crate::DEFAULT_PORT)
    };
    with_port
        .to_socket_addrs()
        .map_err(err)?
        .next()
        .ok_or_else(|| err(format!("cannot resolve {addr}")))
}

fn send(s: &mut TcpStream, m: &C2S) -> io::Result<()> {
    wire::write_frame(s, Kind::Control, &msg::encode(m), 0, &mut Vec::new()).map(|_| ())
}

fn read_s2c(s: &mut TcpStream) -> Result<S2C> {
    let (mut b, mut t) = (Vec::new(), Vec::new());
    let (kind, _) = wire::read_frame(s, &mut b, &mut t).map_err(err)?;
    if kind != Kind::Control {
        return Err(err("expected a control frame"));
    }
    msg::decode(&b).ok_or_else(|| err("bad message from the server"))
}

/// Connect and run the handshake, as far as the server's `Ready`.
fn handshake(o: &ConnectOptions) -> Result<(TcpStream, ServerInfo)> {
    let addr = resolve(&o.addr)?;
    let mut s =
        TcpStream::connect_timeout(&addr, CONNECT).map_err(|e| err(format!("{addr}: {e}")))?;
    let _ = s.set_nodelay(true);
    s.set_read_timeout(Some(START)).map_err(err)?;
    s.set_write_timeout(Some(START)).map_err(err)?;
    s.write_all(&MAGIC).map_err(err)?;
    send(
        &mut s,
        &C2S::Hello(Hello {
            proto: PROTO,
            version: env!("CARGO_PKG_VERSION").into(),
            name: o.name.clone(),
            takeover: o.takeover,
        }),
    )
    .map_err(err)?;
    let (name, version, mode, nonce) = match read_s2c(&mut s)? {
        S2C::ServerHello {
            proto,
            version,
            name,
            mode,
            nonce,
        } => {
            if proto != PROTO {
                return Err(err(format!(
                    "server speaks protocol {proto}, this client {PROTO}"
                )));
            }
            (name, version, mode, nonce)
        }
        S2C::Refused(why) => return Err(err(why)),
        _ => return Err(err("unexpected reply")),
    };
    if let Some(n) = nonce {
        let pw = o
            .password
            .as_deref()
            .ok_or_else(|| err("this server needs a password"))?;
        let client_nonce = auth::nonce();
        let proof = auth::proof(pw, &n, &client_nonce);
        send(
            &mut s,
            &C2S::Auth {
                client_nonce,
                proof,
            },
        )
        .map_err(err)?;
    }
    match read_s2c(&mut s)? {
        S2C::Ready { radios } => Ok((
            s,
            ServerInfo {
                name,
                version,
                mode,
                radios,
            },
        )),
        S2C::Refused(why) => Err(err(why)),
        _ => Err(err("unexpected reply")),
    }
}

/// Ask a server what it is and which radios it has, then hang up.
pub fn probe(o: &ConnectOptions) -> Result<ServerInfo> {
    let (s, info) = handshake(o)?;
    let _ = s.shutdown(Shutdown::Both);
    Ok(info)
}

/// State both modes share.
#[derive(Default)]
struct LinkState {
    stats: LinkStats,
    note: Option<String>,
    center_hz: f64,
    gains: Vec<Gain>,
    /// Why the link ended.
    ended: Option<String>,
}

/// The handle both modes use to send commands.
struct Link {
    tx: Mutex<TcpStream>,
    state: Mutex<LinkState>,
    closed: AtomicBool,
    bytes: AtomicU64,
}

impl Link {
    fn send(&self, m: &C2S) {
        let mut s = self.tx.lock().unwrap();
        if send(&mut s, m).is_err() {
            self.closed.store(true, Ordering::Relaxed);
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let _ = self.tx.lock().unwrap().shutdown(Shutdown::Both);
    }

    fn end(&self, why: String) {
        self.state.lock().unwrap().ended.get_or_insert(why);
        self.closed.store(true, Ordering::Relaxed);
    }

    fn ping_loop(self: Arc<Self>) {
        let mut t = 0;
        while !self.closed.load(Ordering::Relaxed) {
            std::thread::sleep(PING);
            t += 1;
            self.send(&C2S::Ping(t));
        }
    }
}

/// Open the session and wait for the server's `Started`.
fn start_session(
    o: &ConnectOptions,
    radio: Option<RadioRequest>,
    mode: Mode,
) -> Result<(TcpStream, Arc<Link>, ServerInfo, f64, String)> {
    let (mut s, info) = handshake(o)?;
    send(&mut s, &C2S::Start { radio, mode }).map_err(err)?;
    let (rate, center, description, gains) = loop {
        match read_s2c(&mut s)? {
            S2C::Started {
                sample_rate,
                center_hz,
                description,
                gains,
            } => {
                break (sample_rate, center_hz, description, gains);
            }
            S2C::Stopped(why) | S2C::Refused(why) => return Err(err(why)),
            _ => {}
        }
    };
    s.set_read_timeout(Some(IDLE)).map_err(err)?;
    let link = Arc::new(Link {
        tx: Mutex::new(s.try_clone().map_err(err)?),
        state: Mutex::new(LinkState {
            center_hz: center,
            gains,
            ..Default::default()
        }),
        closed: AtomicBool::new(false),
        bytes: AtomicU64::new(0),
    });
    {
        let l = Arc::clone(&link);
        std::thread::Builder::new()
            .name("remote-ping".into())
            .spawn(move || l.ping_loop())
            .map_err(err)?;
    }
    Ok((s, link, info, rate, description))
}

/// Live controls of a remote radio, for either mode. Cloning shares them.
#[derive(Clone)]
pub struct RemoteControl {
    link: Arc<Link>,
}

impl RemoteControl {
    pub fn tune(&self, hz: f64) {
        self.link.send(&C2S::Tune { hz });
    }

    pub fn set_gain(&self, name: &str, value: f64) {
        {
            let mut st = self.link.state.lock().unwrap();
            if let Some(g) = st.gains.iter_mut().find(|g| g.name == name) {
                g.value = value;
            }
        }
        self.link.send(&C2S::Gain {
            name: name.into(),
            value,
        });
    }

    pub fn gains(&self) -> Vec<Gain> {
        self.link.state.lock().unwrap().gains.clone()
    }

    pub fn center_hz(&self) -> f64 {
        self.link.state.lock().unwrap().center_hz
    }

    pub fn stats(&self) -> LinkStats {
        self.link.state.lock().unwrap().stats
    }

    /// Bytes received since the connection opened.
    pub fn bytes_received(&self) -> u64 {
        self.link.bytes.load(Ordering::Relaxed)
    }

    /// The server's latest message for the user, once.
    pub fn take_note(&self) -> Option<String> {
        self.link.state.lock().unwrap().note.take()
    }

    pub fn ended(&self) -> Option<String> {
        self.link.state.lock().unwrap().ended.clone()
    }
}

/// Handle the control messages both modes share. Returns those it did not.
fn common_control(link: &Link, m: S2C) -> Option<S2C> {
    let mut st = link.state.lock().unwrap();
    match m {
        S2C::Stats(s) => st.stats = s,
        S2C::Note(n) => st.note = Some(n),
        S2C::Tuned { center_hz } => st.center_hz = center_hz,
        S2C::Pong(_) => {}
        S2C::Stopped(why) | S2C::Refused(why) => {
            st.ended.get_or_insert(why);
        }
        other => return Some(other),
    }
    None
}

// ---------------------------------------------------------------- dumb mode

/// Raw IQ from a remote radio, as an [`IqSource`].
pub struct RemoteIqSource {
    rx: Receiver<Vec<Iq>>,
    held: Vec<Iq>,
    held_at: usize,
    rate: f64,
    description: String,
    dropped: Arc<AtomicU64>,
    control: RemoteControl,
}

impl RemoteIqSource {
    pub fn connect(o: &ConnectOptions, radio: RadioRequest, p: IqParams) -> Result<RemoteIqSource> {
        let (mut s, link, info, rate, description) = start_session(o, Some(radio), Mode::Dumb(p))?;
        if info.mode == ServerMode::Shared {
            return Err(err("a shared server only does smart mode"));
        }
        let (tx, rx) = mpsc::sync_channel(IQ_QUEUE);
        let dropped = Arc::new(AtomicU64::new(0));
        {
            let (link, dropped) = (Arc::clone(&link), Arc::clone(&dropped));
            std::thread::Builder::new()
                .name("remote-iq-rx".into())
                .spawn(move || iq_reader(&mut s, &link, &tx, &dropped))
                .map_err(err)?;
        }
        Ok(RemoteIqSource {
            rx,
            held: Vec::new(),
            held_at: 0,
            rate,
            description: format!("{description} on {} ({})", o.addr, info.name),
            dropped,
            control: RemoteControl { link },
        })
    }

    pub fn control(&self) -> RemoteControl {
        self.control.clone()
    }

    /// Samples lost so far on this end, plus gaps the server reported.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

fn iq_reader(s: &mut TcpStream, link: &Link, tx: &SyncSender<Vec<Iq>>, dropped: &AtomicU64) {
    let (mut b, mut t) = (Vec::new(), Vec::new());
    let mut next = 0u64;
    loop {
        let (kind, n) = match wire::read_frame(s, &mut b, &mut t) {
            Ok(f) => f,
            Err(e) => {
                link.end(e.to_string());
                return;
            }
        };
        link.bytes.fetch_add(n as u64, Ordering::Relaxed);
        match kind {
            Kind::Iq => {
                let Some((first, count)) = iq::peek(&b) else {
                    continue;
                };
                if first > next {
                    dropped.fetch_add(first - next, Ordering::Relaxed);
                }
                next = first + count as u64;
                let mut out = Vec::with_capacity(count);
                if iq::unpack(&b, &mut out).is_none() {
                    continue;
                }
                match tx.try_send(out) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        dropped.fetch_add(count as u64, Ordering::Relaxed);
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        link.close();
                        return;
                    }
                }
            }
            Kind::Control => {
                if let Some(m) = msg::decode::<S2C>(&b) {
                    common_control(link, m);
                }
            }
            _ => {}
        }
    }
}

impl Drop for RemoteIqSource {
    fn drop(&mut self) {
        self.control.link.close();
    }
}

impl IqSource for RemoteIqSource {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        out.clear();
        let cap = out.capacity().max(4096);
        while out.len() < cap {
            if self.held_at >= self.held.len() {
                let wait = if out.is_empty() {
                    Duration::from_millis(100)
                } else {
                    Duration::ZERO
                };
                match self.rx.recv_timeout(wait) {
                    Ok(b) => {
                        self.held = b;
                        self.held_at = 0;
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => {
                        let why = self.control.ended().unwrap_or_else(|| "link closed".into());
                        return Err(err(why));
                    }
                }
            }
            let n = (cap - out.len()).min(self.held.len() - self.held_at);
            out.extend_from_slice(&self.held[self.held_at..self.held_at + n]);
            self.held_at += n;
        }
        if out.is_empty() {
            // Keeps the engine answering its commands through a stall.
            out.resize(IDLE_SAMPLES, Iq::new(0.0, 0.0));
        }
        Ok(out.len())
    }

    fn sample_rate(&self) -> f64 {
        self.rate
    }

    fn center_freq(&self) -> f64 {
        self.control.center_hz()
    }

    fn describe(&self) -> String {
        self.description.clone()
    }

    fn is_live(&self) -> bool {
        true
    }
}

// --------------------------------------------------------------- smart mode

/// A VFO as this client sees it.
struct LocalVfo {
    settings: VfoSettings,
    status: VfoStatus,
    outputs: TsOutputs,
    camped: bool,
}

/// Where the received TS goes on this machine.
#[derive(Default)]
struct TsOutputs {
    analyser: TsAnalyser,
    report_at: Option<Instant>,
    report: decsat_ts::TsReport,
    ticked: Option<Instant>,
    file: Option<TsFile>,
    file_path: Option<PathBuf>,
    udp: Option<UdpSink>,
    udp_want: Option<SocketAddr>,
    tcp: Option<TcpSink>,
    tcp_want: Option<SocketAddr>,
    error: Option<String>,
    lost: u64,
}

impl TsOutputs {
    fn follow(&mut self, s: &VfoSettings) {
        let record = s.record && s.decoder.outputs_ts();
        match (record, self.file.is_some()) {
            (true, false) if self.file_path.is_none() || self.error.is_none() => {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let path = s.record_dir.join(format!(
                    "decsat-{}-{}-{stamp}.ts",
                    s.name.replace(' ', "_"),
                    decsat_engine::rf::freq_tag(s.offset_hz)
                ));
                let _ = std::fs::create_dir_all(&s.record_dir);
                match TsFile::create(&path) {
                    Ok(f) => self.file = Some(f),
                    Err(e) => self.error = Some(format!("{}: {e}", path.display())),
                }
                self.file_path = Some(path);
            }
            (false, true) => {
                if let Some(mut f) = self.file.take() {
                    let _ = f.flush();
                }
            }
            _ => {}
        }
        let udp = s.ts_udp_on.then(|| s.ts_udp.trim().parse().ok()).flatten();
        if udp != self.udp_want {
            self.udp_want = udp;
            self.udp = None;
            if let Some(a) = udp {
                match UdpSink::new(a) {
                    Ok(u) => self.udp = Some(u),
                    Err(e) => self.error = Some(format!("UDP {a}: {e}")),
                }
            }
        }
        let tcp = s.ts_tcp_on.then(|| s.ts_tcp.trim().parse().ok()).flatten();
        if tcp != self.tcp_want {
            self.tcp_want = tcp;
            self.tcp = None;
            if let Some(a) = tcp {
                match TcpSink::bind(a) {
                    Ok(t) => self.tcp = Some(t),
                    Err(e) => self.error = Some(format!("TCP {a}: {e}")),
                }
            }
        }
    }

    fn write(&mut self, packets: &[[u8; TS_LEN]], s: &VfoSettings) {
        for p in packets {
            self.analyser.packet(p);
        }
        if let Some(f) = &mut self.file
            && let Err(e) = f.write(packets)
        {
            self.error = Some(e.to_string());
            self.file = None;
        }
        if let Some(u) = &mut self.udp {
            u.write(packets);
        }
        if let Some(t) = &mut self.tcp {
            t.write(packets);
        }
        if let Some(t) = &s.ts_tap {
            t.send(packets);
        }
    }

    /// What the server counted, with what only this end knows.
    fn view(&mut self, w: &crate::msg::TsWire, s: &VfoSettings) -> TsView {
        if let Some(t) = self.ticked {
            self.analyser.tick(t.elapsed().as_secs_f64());
        }
        self.ticked = Some(Instant::now());
        if self
            .report_at
            .is_none_or(|t| t.elapsed().as_millis() >= 250)
        {
            self.report = self.analyser.report();
            self.report_at = Some(Instant::now());
        }
        TsView {
            packets: w.packets,
            crc_errors: w.crc_errors,
            cc_errors: w.cc_errors,
            nulls_reinserted: w.nulls_reinserted,
            resyncs: w.resyncs,
            issy: w.issy,
            ts_bps: w.ts_bps,
            report: self.report.clone(),
            file: self
                .file_path
                .clone()
                .map(|p| (p, self.file.as_ref().map_or(0, |f| f.packets))),
            file_active: self.file.is_some(),
            udp: self.udp.as_ref().map(|u| (u.target, u.datagrams)),
            tcp: self.tcp.as_ref().map(|t| (t.addr, t.clients())),
            error: self.error.clone(),
            biss_key: !s.biss_key.is_empty(),
            tap_dropped: s.ts_tap.as_ref().map_or(0, decsat_ts::TsTap::dropped) + self.lost,
            ..Default::default()
        }
    }
}

fn fec_stats(f: &FecWire, ts: Option<TsView>) -> FecStats {
    FecStats {
        frames: f.frames,
        ok: f.ok,
        bch_failed: f.bch_failed,
        crc_failed: f.crc_failed,
        ldpc_unconverged: f.ldpc_unconverged,
        dropped: f.dropped,
        iterations: f.iterations,
        bch_corrected: f.bch_corrected,
        es_n0_db: f.es_n0_db,
        last_header: f.last_header,
        roll_off: f.roll_off,
        streams: f.streams.clone(),
        ts_isi: f.ts_isi,
        per_modcod: f.per_modcod.clone(),
        payload_bps: f.payload_bps,
        load: f.load,
        ts_frames: f.ts_frames,
        ts,
        dvbs: f.dvbs.clone(),
        ..Default::default()
    }
}

fn vfo_status(w: VfoWire, ts: Option<TsView>) -> VfoStatus {
    let mut identification = w.identification;
    if let Some(i) = &mut identification {
        i.symbols = w.id_symbols.expand();
    }
    VfoStatus {
        out_rate: w.out_rate,
        decimation: w.decimation as usize,
        vfo_offset_hz: w.vfo_offset_hz,
        follow_hz: w.follow_hz,
        out_of_span: w.out_of_span,
        load: w.load,
        dropped: w.dropped,
        level_db: w.level_db,
        spectrum_db: w.spectrum.expand(),
        scatter: w.scatter.expand(),
        identification,
        progress: w.progress,
        provisional: w.provisional,
        lock: w.lock,
        frames: w.frames,
        lock_losses: w.lock_losses,
        modcods: w.modcods,
        last_modcod: w.last_modcod,
        symbol_rate: w.symbol_rate,
        message: w.message,
        carrier: w.carrier,
        gold_code: w.gold_code,
        superframes: w.superframes,
        fec: w.fec.as_ref().map(|f| fec_stats(f, ts)),
        ..Default::default()
    }
}

#[derive(Default)]
struct Smart {
    front: FrontStatus,
    rows: VecDeque<(u64, Vec<f32>)>,
    vfos: BTreeMap<VfoId, LocalVfo>,
    shared: Vec<SharedVfo>,
    /// VFOs the server took away or refused, for the GUI to collect.
    gone: Vec<(VfoId, String)>,
    /// Decoders whose settings the owner changed, for campers' GUIs.
    camp_updates: Vec<VfoId>,
}

fn request_of(s: &VfoSettings) -> VfoRequest {
    VfoRequest {
        name: s.name.clone(),
        offset_hz: s.offset_hz,
        bandwidth_hz: s.bandwidth_hz,
        decoder: s.decoder.key().into(),
        enabled: s.enabled,
        symbol_rate: s.symbol_rate,
        gold_code: s.gold_code,
        follow_carrier: s.follow_carrier,
        ts_isi: s.ts_isi,
        biss_key: s.biss_key.clone(),
    }
}

/// The remote engine. Every method returns at once.
pub struct RemoteEngine {
    link: Arc<Link>,
    state: Arc<Mutex<Smart>>,
    next_id: AtomicU32,
    sample_rate: f64,
    mode: ServerMode,
}

impl RemoteEngine {
    /// `radio` is ignored by a shared server, whose radio is its own.
    pub fn connect(
        o: &ConnectOptions,
        radio: Option<RadioRequest>,
        p: SmartParams,
    ) -> Result<RemoteEngine> {
        let (mut s, link, info, rate, description) = start_session(o, radio, Mode::Smart(p))?;
        let state = Arc::new(Mutex::new(Smart {
            front: FrontStatus {
                sample_rate: rate,
                center_freq: link.state.lock().unwrap().center_hz,
                fft_size: p.fft_size as usize,
                source: description,
                ..Default::default()
            },
            ..Default::default()
        }));
        {
            let (link, state) = (Arc::clone(&link), Arc::clone(&state));
            std::thread::Builder::new()
                .name("remote-smart-rx".into())
                .spawn(move || smart_reader(&mut s, &link, &state))
                .map_err(err)?;
        }
        Ok(RemoteEngine {
            link,
            state,
            next_id: AtomicU32::new(1),
            sample_rate: rate,
            mode: info.mode,
        })
    }

    pub fn control(&self) -> RemoteControl {
        RemoteControl {
            link: Arc::clone(&self.link),
        }
    }

    pub fn mode(&self) -> ServerMode {
        self.mode
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    pub fn front(&self) -> FrontStatus {
        let mut f = self.state.lock().unwrap().front.clone();
        if let Some(why) = self.control().ended() {
            f.state = SourceState::Failed(why);
        }
        f
    }

    pub fn new_rows(&self, after: u64) -> Vec<(u64, Vec<f32>)> {
        self.state
            .lock()
            .unwrap()
            .rows
            .iter()
            .filter(|(s, _)| *s > after)
            .cloned()
            .collect()
    }

    pub fn set_paused(&self, p: bool) {
        self.link.send(&C2S::SetPaused(p));
    }

    pub fn set_dc_removal(&self, on: bool) {
        self.link.send(&C2S::SetDcRemoval(on));
    }

    pub fn set_fft_size(&self, n: usize) {
        self.link.send(&C2S::SetFftSize(n as u32));
    }

    fn new_id(&self) -> VfoId {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn add_vfo(&mut self, settings: VfoSettings) -> VfoId {
        let id = self.new_id();
        self.insert(id, settings, false);
        let vfo = {
            let st = self.state.lock().unwrap();
            request_of(&st.vfos[&id].settings)
        };
        self.link.send(&C2S::AddVfo { id, vfo });
        id
    }

    fn insert(&self, id: VfoId, settings: VfoSettings, camped: bool) {
        let mut outputs = TsOutputs::default();
        outputs.follow(&settings);
        self.state.lock().unwrap().vfos.insert(
            id,
            LocalVfo {
                settings,
                status: VfoStatus::default(),
                outputs,
                camped,
            },
        );
    }

    pub fn update_vfo(&self, id: VfoId, settings: VfoSettings) {
        self.post(id, settings, None);
    }

    pub fn carry_vfo(&self, id: VfoId, settings: VfoSettings, by: f64) {
        self.post(id, settings, Some(by));
    }

    fn post(&self, id: VfoId, settings: VfoSettings, carried_by: Option<f64>) {
        let send = {
            let mut st = self.state.lock().unwrap();
            let Some(v) = st.vfos.get_mut(&id) else {
                return;
            };
            let changed = request_of(&v.settings) != request_of(&settings);
            v.outputs.follow(&settings);
            v.settings = settings.clone();
            changed && !v.camped
        };
        if send {
            self.link.send(&C2S::UpdateVfo {
                id,
                vfo: request_of(&settings),
                carried_by,
            });
        }
    }

    pub fn remove_vfo(&self, id: VfoId) {
        if self.state.lock().unwrap().vfos.remove(&id).is_some() {
            self.link.send(&C2S::RemoveVfo { id });
        }
    }

    pub fn vfo_ids(&self) -> Vec<VfoId> {
        self.state.lock().unwrap().vfos.keys().copied().collect()
    }

    pub fn vfo_settings(&self, id: VfoId) -> Option<VfoSettings> {
        self.state
            .lock()
            .unwrap()
            .vfos
            .get(&id)
            .map(|v| v.settings.clone())
    }

    pub fn vfo_status(&self, id: VfoId) -> Option<VfoStatus> {
        self.state
            .lock()
            .unwrap()
            .vfos
            .get(&id)
            .map(|v| v.status.clone())
    }

    /// Restrict the TS the server sends for a VFO; empty for everything.
    pub fn set_ts_pids(&self, id: VfoId, pids: Vec<u16>) {
        self.link.send(&C2S::TsPids { id, pids });
    }

    /// The decoders on a shared server.
    pub fn shared_list(&self) -> Vec<SharedVfo> {
        self.state.lock().unwrap().shared.clone()
    }

    /// Join another user's decoder. The returned id appears at once; its
    /// settings fill in when the server answers, and it goes away (see
    /// [`take_gone`](Self::take_gone)) if refused.
    pub fn camp(&self, server_id: u32) -> VfoId {
        let id = self.new_id();
        let mut placeholder = VfoSettings::new("…", 0.0, 1.0, DecoderKind::Identify);
        placeholder.enabled = false;
        self.insert(id, placeholder, true);
        self.link.send(&C2S::Camp { id, server_id });
        id
    }

    pub fn is_camped(&self, id: VfoId) -> bool {
        self.state
            .lock()
            .unwrap()
            .vfos
            .get(&id)
            .is_some_and(|v| v.camped)
    }

    /// VFOs that have gone since the last call, and why.
    pub fn take_gone(&self) -> Vec<(VfoId, String)> {
        std::mem::take(&mut self.state.lock().unwrap().gone)
    }

    /// Camped VFOs whose settings changed since the last call.
    pub fn take_camp_updates(&self) -> Vec<VfoId> {
        std::mem::take(&mut self.state.lock().unwrap().camp_updates)
    }
}

impl Drop for RemoteEngine {
    fn drop(&mut self) {
        self.link.close();
    }
}

fn smart_reader(s: &mut TcpStream, link: &Link, state: &Mutex<Smart>) {
    let (mut b, mut t) = (Vec::new(), Vec::new());
    let (mut rows, mut spectrum) = (TraceDecoder::default(), TraceDecoder::default());
    loop {
        let (kind, n) = match wire::read_frame(s, &mut b, &mut t) {
            Ok(f) => f,
            Err(e) => {
                link.end(e.to_string());
                return;
            }
        };
        link.bytes.fetch_add(n as u64, Ordering::Relaxed);
        match kind {
            Kind::Row => {
                if let Some((seq, row)) = rows.decode(&b) {
                    let mut st = state.lock().unwrap();
                    if st.rows.len() >= ROW_BACKLOG {
                        st.rows.pop_front();
                    }
                    st.rows.push_back((seq, row));
                }
            }
            Kind::Spectrum => {
                if let Some((_, trace)) = spectrum.decode(&b) {
                    state.lock().unwrap().front.spectrum_db = trace;
                }
            }
            Kind::Front => {
                if let Some(w) = msg::decode::<FrontWire>(&b) {
                    let mut st = state.lock().unwrap();
                    let keep = std::mem::take(&mut st.front.spectrum_db);
                    st.front = FrontStatus {
                        sample_rate: w.sample_rate,
                        center_freq: w.center_freq,
                        fft_size: w.fft_size as usize,
                        spectrum_db: keep,
                        carriers: w.carriers,
                        burst_bands: w.burst_bands,
                        burst_takes: w.burst_takes,
                        source: w.source,
                        state: w.state,
                        samples: w.samples,
                        load: w.load,
                        row_seq: w.row_seq,
                        input_dbfs: w.input_dbfs,
                    };
                }
            }
            Kind::Vfo => {
                if let Some(w) = msg::decode::<VfoWire>(&b) {
                    let mut st = state.lock().unwrap();
                    if let Some(v) = st.vfos.get_mut(&w.id) {
                        let ts = w
                            .fec
                            .as_ref()
                            .and_then(|f| f.ts.as_ref())
                            .map(|t| v.outputs.view(t, &v.settings));
                        v.status = vfo_status(w, ts);
                    }
                }
            }
            Kind::Ts => {
                if let Some((id, lost, packets)) = ts::rebuild(&b) {
                    let mut st = state.lock().unwrap();
                    if let Some(v) = st.vfos.get_mut(&id) {
                        v.outputs.lost += u64::from(lost);
                        let settings = v.settings.clone();
                        v.outputs.write(&packets, &settings);
                    }
                }
            }
            Kind::Control => {
                let Some(m) = msg::decode::<S2C>(&b) else {
                    continue;
                };
                let Some(m) = common_control(link, m) else {
                    continue;
                };
                let mut st = state.lock().unwrap();
                match m {
                    S2C::Shared(list) => st.shared = list,
                    S2C::VfoGone { id, why } => {
                        st.vfos.remove(&id);
                        st.gone.push((id, why));
                    }
                    S2C::Camped { id, vfo } => {
                        if let Some(v) = st.vfos.get_mut(&id) {
                            let kind = DecoderKind::from_key(&vfo.decoder)
                                .unwrap_or(DecoderKind::Identify);
                            let mut s =
                                VfoSettings::new(vfo.name, vfo.offset_hz, vfo.bandwidth_hz, kind);
                            s.enabled = vfo.enabled;
                            s.symbol_rate = vfo.symbol_rate;
                            s.gold_code = vfo.gold_code;
                            s.follow_carrier = vfo.follow_carrier;
                            s.ts_isi = vfo.ts_isi;
                            // Local outputs are this user's own.
                            s.record = v.settings.record;
                            s.record_dir = v.settings.record_dir.clone();
                            s.ts_tap = v.settings.ts_tap.clone();
                            s.ts_udp_on = v.settings.ts_udp_on;
                            s.ts_udp = v.settings.ts_udp.clone();
                            s.ts_tcp_on = v.settings.ts_tcp_on;
                            s.ts_tcp = v.settings.ts_tcp.clone();
                            v.settings = s;
                            st.camp_updates.push(id);
                        }
                    }
                    _ => {}
                }
            }
            Kind::Iq => {}
        }
    }
}
