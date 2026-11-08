// made by CasualArclamp
//! The server end of smart mode: one engine and whoever is attached to it.
//! A normal session is a hub with one client; shared mode is the same hub
//! with several, limits on how many decoders each may run, and the rules
//! for who owns what.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use decsat_engine::frontend::{Engine, EngineOptions, FrontStatus};
use decsat_engine::vfo::{DecoderKind, VfoSettings, VfoStatus};
use decsat_ts::TsTap;

use crate::display::{DbTrace, Scatter};
use crate::msg::{
    self, FecWire, FrontWire, S2C, SharedVfo, SmartParams, TsWire, VfoRequest, VfoWire,
};
use crate::outq::OutQueue;
use crate::radios::OpenRadio;
use crate::ts::{self, PidFilter};

/// The decoders a server will run. Everything else stays local for now.
pub fn served(kind: DecoderKind) -> bool {
    matches!(
        kind,
        DecoderKind::Identify | DecoderKind::Dvbs2Ts | DecoderKind::DvbsTs
    )
}

/// Batches a VFO's TS tap may hold before the engine drops some.
const TAP_BATCHES: usize = 256;

/// Shared-mode limits; `None` on a normal session.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub per_client: usize,
    pub total: usize,
}

type Conn = u64;

struct Sub {
    conn: Conn,
    id: u32,
    filter: PidFilter,
    out: Arc<OutQueue>,
}

struct HubVfo {
    req: VfoRequest,
    owner: Option<Conn>,
    owner_name: Option<String>,
    tap: TsTap,
    subs: Arc<Mutex<Vec<Sub>>>,
}

struct Client {
    name: String,
    out: Arc<OutQueue>,
    /// The client's numbers for its VFOs → the engine's.
    vfos: HashMap<u32, u32>,
}

struct State {
    engine: Engine,
    vfos: HashMap<u32, HubVfo>,
    clients: HashMap<Conn, Client>,
}

/// One engine and its users.
pub struct Hub {
    state: Mutex<State>,
    limits: Option<Limits>,
}

fn settings_of(req: &VfoRequest, kind: DecoderKind, tap: &TsTap, biss: bool) -> VfoSettings {
    let mut s = VfoSettings::new(req.name.clone(), req.offset_hz, req.bandwidth_hz, kind);
    s.enabled = req.enabled;
    s.symbol_rate = req.symbol_rate;
    s.gold_code = req.gold_code;
    s.follow_carrier = req.follow_carrier;
    s.ts_isi = req.ts_isi;
    if biss {
        s.biss_key = req.biss_key.clone();
    }
    s.ts_tap = Some(tap.clone());
    s
}

fn fan_out(rx: std::sync::mpsc::Receiver<Vec<[u8; 188]>>, subs: Arc<Mutex<Vec<Sub>>>) {
    while let Ok(batch) = rx.recv() {
        for s in subs.lock().unwrap().iter() {
            let mut f = Vec::with_capacity(batch.len() * 188 + 64);
            ts::reduce(s.id, 0, &batch, &s.filter, &mut f);
            s.out.ts(f, batch.len() as u32);
        }
    }
}

impl Hub {
    pub fn start(radio: OpenRadio, p: SmartParams, limits: Option<Limits>) -> Hub {
        let engine = Engine::start(
            radio.source,
            EngineOptions {
                fft_size: p.fft_size as usize,
                rows_per_sec: f64::from(p.rows_per_sec),
                dc_removal: p.dc_removal,
                ..Default::default()
            },
        );
        Hub {
            state: Mutex::new(State {
                engine,
                vfos: HashMap::new(),
                clients: HashMap::new(),
            }),
            limits,
        }
    }

    pub fn sample_rate(&self) -> f64 {
        self.state.lock().unwrap().engine.sample_rate()
    }

    pub fn front(&self) -> FrontStatus {
        self.state.lock().unwrap().engine.front()
    }

    pub fn shared(&self) -> bool {
        self.limits.is_some()
    }

    pub fn attach(&self, conn: Conn, name: String, out: Arc<OutQueue>) {
        self.state.lock().unwrap().clients.insert(
            conn,
            Client {
                name,
                out,
                vfos: HashMap::new(),
            },
        );
    }

    /// A client has gone: its subscriptions end, and any decoder left with
    /// neither owner nor campers closes.
    pub fn detach(&self, conn: Conn) {
        let mut st = self.state.lock().unwrap();
        st.clients.remove(&conn);
        let ids: Vec<u32> = st.vfos.keys().copied().collect();
        for sid in ids {
            let v = st.vfos.get_mut(&sid).unwrap();
            v.subs.lock().unwrap().retain(|s| s.conn != conn);
            if v.owner == Some(conn) {
                v.owner = None;
                v.owner_name = None;
            }
            Self::close_if_orphan(&mut st, sid);
        }
    }

    fn close_if_orphan(st: &mut State, sid: u32) {
        if let Some(v) = st.vfos.get(&sid)
            && v.owner.is_none()
            && v.subs.lock().unwrap().is_empty()
        {
            st.vfos.remove(&sid);
            st.engine.remove_vfo(sid);
        }
    }

    pub fn add_vfo(&self, conn: Conn, id: u32, req: VfoRequest) -> Result<(), String> {
        let kind = DecoderKind::from_key(&req.decoder)
            .filter(|k| served(*k))
            .ok_or_else(|| format!("{} does not run on the server yet", req.decoder))?;
        if req.bandwidth_hz <= 0.0 {
            return Err("a VFO needs a bandwidth".into());
        }
        let mut st = self.state.lock().unwrap();
        if let Some(l) = self.limits {
            let mine = st.vfos.values().filter(|v| v.owner == Some(conn)).count();
            if mine >= l.per_client {
                return Err(format!("this server allows {} decoders each", l.per_client));
            }
            if st.vfos.len() >= l.total {
                return Err(format!(
                    "this server is running its limit of {} decoders",
                    l.total
                ));
            }
        }
        let client = st.clients.get(&conn).ok_or("not attached")?;
        let (name, out) = (client.name.clone(), Arc::clone(&client.out));
        let (tap, rx) = TsTap::new(TAP_BATCHES);
        let biss = self.limits.is_none();
        let settings = settings_of(&req, kind, &tap, biss);
        let sid = st.engine.add_vfo(settings);
        let subs = Arc::new(Mutex::new(vec![Sub {
            conn,
            id,
            filter: None,
            out,
        }]));
        {
            let subs = Arc::clone(&subs);
            // The thread ends when the engine drops the tap with the VFO.
            std::thread::Builder::new()
                .name("remote-ts".into())
                .spawn(move || fan_out(rx, subs))
                .map_err(|e| e.to_string())?;
        }
        st.vfos.insert(
            sid,
            HubVfo {
                req,
                owner: Some(conn),
                owner_name: Some(name),
                tap,
                subs,
            },
        );
        st.clients.get_mut(&conn).unwrap().vfos.insert(id, sid);
        Ok(())
    }

    /// Only a decoder's owner may change it; campers' attempts are ignored
    /// quietly, since a GUI posts its own view of a VFO as a matter of course.
    pub fn update_vfo(
        &self,
        conn: Conn,
        id: u32,
        req: VfoRequest,
        carried: Option<f64>,
    ) -> Result<(), String> {
        let st = &mut *self.state.lock().unwrap();
        let Some(&sid) = st.clients.get(&conn).and_then(|c| c.vfos.get(&id)) else {
            return Ok(());
        };
        let v = st.vfos.get_mut(&sid).ok_or("no such decoder")?;
        if v.owner != Some(conn) {
            return Ok(());
        }
        let kind = DecoderKind::from_key(&req.decoder)
            .filter(|k| served(*k))
            .ok_or_else(|| format!("{} does not run on the server yet", req.decoder))?;
        let settings = settings_of(&req, kind, &v.tap, self.limits.is_none());
        v.req = req;
        match carried {
            Some(by) => st.engine.carry_vfo(sid, settings, by),
            None => st.engine.update_vfo(sid, settings),
        }
        Ok(())
    }

    /// The client lets go of a VFO: an owner ends it for everyone unless
    /// someone is camped on it; a camper just leaves.
    pub fn remove_vfo(&self, conn: Conn, id: u32) {
        let st = &mut *self.state.lock().unwrap();
        let Some(sid) = st.clients.get_mut(&conn).and_then(|c| c.vfos.remove(&id)) else {
            return;
        };
        if let Some(v) = st.vfos.get_mut(&sid) {
            v.subs.lock().unwrap().retain(|s| s.conn != conn);
            if v.owner == Some(conn) {
                v.owner = None;
                v.owner_name = None;
            }
        }
        Self::close_if_orphan(st, sid);
    }

    pub fn camp(&self, conn: Conn, id: u32, sid: u32) -> Result<VfoRequest, String> {
        if self.limits.is_none() {
            return Err("camping needs a shared server".into());
        }
        let st = &mut *self.state.lock().unwrap();
        let out = st.clients.get(&conn).ok_or("not attached")?.out.clone();
        let v = st.vfos.get(&sid).ok_or("that decoder has gone")?;
        {
            let mut subs = v.subs.lock().unwrap();
            if subs.iter().any(|s| s.conn == conn) {
                return Err("already on that decoder".into());
            }
            subs.push(Sub {
                conn,
                id,
                filter: None,
                out,
            });
        }
        let req = v.req.clone();
        st.clients.get_mut(&conn).unwrap().vfos.insert(id, sid);
        Ok(req)
    }

    pub fn ts_pids(&self, conn: Conn, id: u32, pids: &[u16]) {
        let st = self.state.lock().unwrap();
        if let Some(sid) = st.clients.get(&conn).and_then(|c| c.vfos.get(&id))
            && let Some(v) = st.vfos.get(sid)
        {
            for s in v.subs.lock().unwrap().iter_mut().filter(|s| s.conn == conn) {
                s.filter = ts::filter_of(pids);
            }
        }
    }

    pub fn set_paused(&self, p: bool) {
        if !self.shared() {
            self.state.lock().unwrap().engine.set_paused(p);
        }
    }

    pub fn set_dc_removal(&self, on: bool) {
        if !self.shared() {
            self.state.lock().unwrap().engine.set_dc_removal(on);
        }
    }

    pub fn set_fft_size(&self, n: usize) {
        if !self.shared() {
            self.state.lock().unwrap().engine.set_fft_size(n);
        }
    }

    fn new_rows(&self, after: u64) -> Vec<(u64, Vec<f32>)> {
        self.state.lock().unwrap().engine.new_rows(after)
    }

    /// The client's VFOs and the statuses to send them.
    fn statuses(&self, conn: Conn) -> Vec<(u32, VfoStatus)> {
        let st = self.state.lock().unwrap();
        let Some(c) = st.clients.get(&conn) else {
            return Vec::new();
        };
        c.vfos
            .iter()
            .filter_map(|(id, sid)| Some((*id, st.engine.vfo_status(*sid)?)))
            .collect()
    }

    /// Current settings of the decoders the client is camped on.
    fn camped_settings(&self, conn: Conn) -> Vec<(u32, VfoRequest)> {
        let st = self.state.lock().unwrap();
        let Some(c) = st.clients.get(&conn) else {
            return Vec::new();
        };
        c.vfos
            .iter()
            .filter_map(|(id, sid)| {
                let v = st.vfos.get(sid)?;
                (v.owner != Some(conn)).then(|| (*id, v.req.clone()))
            })
            .collect()
    }

    fn shared_list(&self) -> Vec<SharedVfo> {
        let st = self.state.lock().unwrap();
        let mut list: Vec<SharedVfo> = st
            .vfos
            .iter()
            .map(|(sid, v)| {
                let subs = v.subs.lock().unwrap().len() as u32;
                SharedVfo {
                    server_id: *sid,
                    name: v.req.name.clone(),
                    owner: v.owner_name.clone(),
                    offset_hz: v.req.offset_hz,
                    bandwidth_hz: v.req.bandwidth_hz,
                    decoder: v.req.decoder.clone(),
                    campers: subs - u32::from(v.owner.is_some()),
                }
            })
            .collect();
        list.sort_by_key(|v| v.server_id);
        list
    }
}

fn front_wire(f: &FrontStatus) -> FrontWire {
    FrontWire {
        sample_rate: f.sample_rate,
        center_freq: f.center_freq,
        fft_size: f.fft_size as u32,
        carriers: f.carriers.clone(),
        burst_bands: f.burst_bands.clone(),
        burst_takes: f.burst_takes,
        source: f.source.clone(),
        state: f.state.clone(),
        samples: f.samples,
        load: f.load,
        row_seq: f.row_seq,
        input_dbfs: f.input_dbfs,
    }
}

fn vfo_wire(id: u32, s: &VfoStatus, points: usize) -> VfoWire {
    let mut identification = s.identification.clone();
    let id_symbols = identification
        .as_mut()
        .map(|i| Scatter::of(&std::mem::take(&mut i.symbols), points))
        .unwrap_or_default();
    VfoWire {
        id,
        out_rate: s.out_rate,
        decimation: s.decimation as u32,
        vfo_offset_hz: s.vfo_offset_hz,
        follow_hz: s.follow_hz,
        out_of_span: s.out_of_span,
        load: s.load,
        dropped: s.dropped,
        level_db: s.level_db,
        spectrum: DbTrace::of(&s.spectrum_db),
        scatter: Scatter::of(&s.scatter, points),
        identification,
        id_symbols,
        progress: s.progress,
        provisional: s.provisional,
        lock: s.lock,
        frames: s.frames,
        lock_losses: s.lock_losses,
        modcods: s.modcods.clone(),
        last_modcod: s.last_modcod,
        symbol_rate: s.symbol_rate,
        message: s.message.clone(),
        carrier: s.carrier,
        gold_code: s.gold_code,
        superframes: s.superframes,
        fec: s.fec.as_ref().map(|f| FecWire {
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
            ts: f.ts.as_ref().map(|t| TsWire {
                packets: t.packets,
                crc_errors: t.crc_errors,
                cc_errors: t.cc_errors,
                nulls_reinserted: t.nulls_reinserted,
                resyncs: t.resyncs,
                issy: t.issy,
                ts_bps: t.ts_bps,
            }),
            dvbs: f.dvbs.clone(),
        }),
    }
}

/// Rows held back before the row rate is cut.
const ROW_BACKLOG: usize = 6;
/// How long the link must stay clear before the row rate is raised.
const RECOVER: Duration = Duration::from_secs(3);

/// Feed one client until told to stop.
pub fn pump(
    hub: &Hub,
    conn: Conn,
    out: &OutQueue,
    p: SmartParams,
    stop: &AtomicBool,
    written: &AtomicU64,
) {
    let tick = Duration::from_millis(20);
    let due = |at: &mut Instant, every: Duration| {
        if at.elapsed() >= every {
            *at = Instant::now();
            true
        } else {
            false
        }
    };
    let long_ago = Instant::now() - Duration::from_secs(10);
    let (mut spec_at, mut front_at, mut vfo_at, mut list_at, mut stats_at) =
        (long_ago, long_ago, long_ago, long_ago, long_ago);
    let (mut last_row, mut spec_seq, mut count) = (0u64, 0u64, 0u64);
    let mut divider = 1u32;
    let mut changed = Instant::now();
    let mut list: Vec<SharedVfo> = Vec::new();
    let mut camped: HashMap<u32, VfoRequest> = HashMap::new();
    let points = p.scatter_points as usize;

    while !stop.load(Ordering::Relaxed) && !out.is_closed() {
        for (seq, row) in hub.new_rows(last_row) {
            last_row = seq;
            count += 1;
            if count.is_multiple_of(u64::from(divider)) {
                out.row(seq, row);
            } else {
                out.thinned(1);
            }
        }
        let waiting = out.rows_waiting();
        if waiting >= ROW_BACKLOG {
            divider = (divider * 2).min(16);
            out.trim_rows(2);
            changed = Instant::now();
        } else if waiting == 0 && divider > 1 && changed.elapsed() >= RECOVER {
            divider /= 2;
            changed = Instant::now();
        }
        if due(&mut front_at, Duration::from_millis(250)) {
            out.front(msg::encode(&front_wire(&hub.front())));
        }
        if due(&mut spec_at, Duration::from_millis(100)) {
            spec_seq += 1;
            out.spectrum(spec_seq, hub.front().spectrum_db);
        }
        if due(&mut vfo_at, Duration::from_millis(200)) {
            for (id, s) in hub.statuses(conn) {
                out.vfo(id, msg::encode(&vfo_wire(id, &s, points)));
            }
        }
        if hub.shared() && due(&mut list_at, Duration::from_secs(1)) {
            let now = hub.shared_list();
            if now != list {
                out.control(msg::encode(&S2C::Shared(now.clone())));
                list = now;
            }
            // An owner's retuning reaches those camped on the decoder.
            for (id, req) in hub.camped_settings(conn) {
                if camped.get(&id) != Some(&req) {
                    camped.insert(id, req.clone());
                    out.control(msg::encode(&S2C::Camped { id, vfo: req }));
                }
            }
        }
        if due(&mut stats_at, Duration::from_secs(1)) {
            crate::server::send_stats(out, written, divider);
        }
        std::thread::sleep(tick);
    }
}
