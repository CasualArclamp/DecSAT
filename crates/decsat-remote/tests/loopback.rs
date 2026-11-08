// made by CasualArclamp
//! A server and clients over loopback, on a fake radio: no hardware is
//! opened anywhere in these tests.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use decsat_core::{Iq, Result};
use decsat_engine::vfo::{DecoderKind, VfoSettings};
use decsat_io::IqSource;
use decsat_mod::{FrameSpec, PlFramer, Shaper};
use decsat_remote::client::{ConnectOptions, RemoteEngine, RemoteIqSource, probe};
use decsat_remote::msg::{IqParams, ServerMode, SmartParams};
use decsat_remote::radios::{
    Gain, OpenRadio, RadioControl, RadioInfo, RadioProvider, RadioRequest,
};
use decsat_remote::server::{Server, ServerConfig, SharedConfig};
use decsat_ts::TsTap;

const RATE: f64 = 500_000.0;
const CENTER: f64 = 1_000_000_000.0;

/// Plays a block of samples around and around, paced by the engine or the
/// server as a capture would be.
struct Loop {
    x: Arc<Vec<Iq>>,
    at: usize,
}

impl IqSource for Loop {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        out.clear();
        let n = out.capacity().clamp(1024, 65_536);
        for _ in 0..n {
            out.push(self.x[self.at]);
            self.at = (self.at + 1) % self.x.len();
        }
        Ok(n)
    }
    fn sample_rate(&self) -> f64 {
        RATE
    }
    fn center_freq(&self) -> f64 {
        CENTER
    }
    fn describe(&self) -> String {
        "fake radio".into()
    }
}

#[derive(Default)]
struct Log {
    tuned: Mutex<Vec<f64>>,
    gains: Mutex<Vec<(String, f64)>>,
    opens: AtomicU64,
}

struct Ctl(Arc<Log>);

impl RadioControl for Ctl {
    fn set_center(&self, hz: f64) -> Result<()> {
        self.0.tuned.lock().unwrap().push(hz);
        Ok(())
    }
    fn set_gain(&self, name: &str, value: f64) -> Result<()> {
        self.0.gains.lock().unwrap().push((name.into(), value));
        Ok(())
    }
}

struct Fake {
    x: Arc<Vec<Iq>>,
    log: Arc<Log>,
}

impl RadioProvider for Fake {
    fn list(&self) -> Vec<RadioInfo> {
        vec![RadioInfo {
            key: "fake".into(),
            name: "Fake".into(),
            min_hz: 0.0,
            max_hz: 6e9,
            rates: vec![RATE],
            gains: vec![],
        }]
    }
    fn open(&self, _req: &RadioRequest) -> Result<OpenRadio> {
        self.log.opens.fetch_add(1, Ordering::Relaxed);
        Ok(OpenRadio {
            source: Box::new(Loop {
                x: Arc::clone(&self.x),
                at: 0,
            }),
            control: Some(Arc::new(Ctl(Arc::clone(&self.log)))),
            gains: vec![Gain {
                name: "g".into(),
                min: 0.0,
                max: 10.0,
                step: 1.0,
                value: 1.0,
            }],
        })
    }
}

/// TS over DVB-S2, QPSK 1/2 with pilots at 125 kBd, 40 kHz off centre (the
/// engine's own end-to-end test signal).
fn signal() -> Arc<Vec<Iq>> {
    let syms = PlFramer::new(0, 8).build_schedule(&[FrameSpec::new(4, false, true)], 170_000);
    let mut sh = Shaper::new(4, 0.35, 16);
    let mut x = Vec::new();
    sh.process(&syms, &mut x);
    let w = std::f64::consts::TAU * 40_000.0 / RATE;
    for (n, v) in x.iter_mut().enumerate() {
        let ph = w * n as f64;
        *v *= Iq::new(ph.cos() as f32, ph.sin() as f32);
    }
    Arc::new(x)
}

fn request() -> RadioRequest {
    RadioRequest {
        key: "fake".into(),
        center_hz: CENTER,
        sample_rate: RATE,
        gains: vec![],
    }
}

fn server(cfg: ServerConfig) -> (Server, Arc<Log>, String) {
    let log = Arc::new(Log::default());
    let p = Fake {
        x: signal(),
        log: Arc::clone(&log),
    };
    let s = Server::start(
        ServerConfig {
            bind: "127.0.0.1:0".into(),
            ..cfg
        },
        Arc::new(p),
    )
    .unwrap();
    let addr = s.local_addr().to_string();
    (s, log, addr)
}

fn opts(addr: &str, name: &str) -> ConnectOptions {
    ConnectOptions {
        addr: addr.into(),
        name: name.into(),
        password: None,
        takeover: false,
    }
}

fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed().as_secs() < secs, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn ts_vfo(name: &str) -> VfoSettings {
    VfoSettings::new(name, 40_000.0, 190_000.0, DecoderKind::Dvbs2Ts)
}

#[test]
fn dumb_mode_delivers_the_samples() {
    let (_s, log, addr) = server(ServerConfig::default());
    let info = probe(&opts(&addr, "t")).unwrap();
    assert_eq!(info.mode, ServerMode::Exclusive);
    assert_eq!(info.radios[0].key, "fake");

    let params = IqParams {
        bits: 8,
        zstd: 1,
        slice: None,
    };
    // The probe's slot takes a moment to free.
    let mut held = None;
    wait_for("the probe to let go", 5, || {
        held = RemoteIqSource::connect(&opts(&addr, "t"), request(), params).ok();
        held.is_some()
    });
    let mut src = held.unwrap();
    assert_eq!(src.sample_rate(), RATE);
    assert_eq!(src.center_freq(), CENTER);

    let reference = signal();
    let mut got = Vec::new();
    let mut buf: Vec<Iq> = Vec::with_capacity(16_384);
    wait_for("100k samples", 15, || {
        src.read(&mut buf).unwrap();
        got.extend_from_slice(&buf);
        got.len() >= 100_000
    });
    // Any idle-time silence comes first, and the signal's own ramp-up packs
    // to zeros, so find where the reference starts by trying each lag.
    let score = |lag: usize, n: usize| {
        let (mut err, mut pow) = (0f32, 0f32);
        for (a, b) in got[lag..].iter().zip(reference.iter()).take(n) {
            err += (a - b).norm_sqr();
            pow += b.norm_sqr();
        }
        (err, pow)
    };
    let start = (0..got.len() - 60_000)
        .min_by(|a, b| score(*a, 4000).0.total_cmp(&score(*b, 4000).0))
        .unwrap();
    let (err, pow) = score(start, 50_000);
    let snr = 10.0 * (pow / err).log10();
    assert!(snr > 30.0, "{snr:.1} dB");

    let c = src.control();
    c.tune(2e9);
    c.set_gain("g", 7.0);
    wait_for("the retune", 5, || c.center_hz() == 2e9);
    wait_for("the gain", 5, || !log.gains.lock().unwrap().is_empty());
    assert_eq!(log.gains.lock().unwrap()[0], ("g".to_string(), 7.0));
    assert_eq!(c.gains()[0].value, 7.0);
    assert!(c.bytes_received() > 100_000);
}

#[test]
fn dumb_mode_can_send_a_slice_of_the_band() {
    let (_s, _log, addr) = server(ServerConfig::default());
    let params = IqParams {
        bits: 12,
        zstd: 0,
        slice: Some((40_000.0, 100_000.0)),
    };
    let mut src = RemoteIqSource::connect(&opts(&addr, "t"), request(), params).unwrap();
    assert!(src.sample_rate() < RATE, "decimated: {}", src.sample_rate());
    assert_eq!(src.center_freq(), CENTER + 40_000.0);
    let mut buf: Vec<Iq> = Vec::with_capacity(4096);
    let mut n = 0;
    wait_for("samples", 15, || {
        src.read(&mut buf).unwrap();
        n += buf.iter().filter(|s| s.norm() > 0.0).count();
        n > 20_000
    });
}

#[test]
fn passwords_and_a_busy_server() {
    let (_s, _log, addr) = server(ServerConfig {
        password: Some("sesame".into()),
        allow_takeover: true,
        ..Default::default()
    });
    assert!(
        probe(&opts(&addr, "a"))
            .unwrap_err()
            .to_string()
            .contains("password")
    );
    let mut wrong = opts(&addr, "a");
    wrong.password = Some("nope".into());
    assert!(
        probe(&wrong)
            .unwrap_err()
            .to_string()
            .contains("wrong password")
    );

    let mut a = opts(&addr, "alice");
    a.password = Some("sesame".into());
    let first = RemoteIqSource::connect(&a, request(), IqParams::default()).unwrap();

    let mut b = opts(&addr, "bob");
    b.password = Some("sesame".into());
    let busy = RemoteIqSource::connect(&b, request(), IqParams::default());
    assert!(busy.err().unwrap().to_string().contains("in use by alice"));

    b.takeover = true;
    let second = RemoteIqSource::connect(&b, request(), IqParams::default()).unwrap();
    wait_for("alice to be cut off", 5, || {
        first.control().ended().is_some()
    });
    drop(second);
}

#[test]
fn smart_mode_sends_the_display_and_the_ts() {
    let (_s, log, addr) = server(ServerConfig::default());
    let p = SmartParams {
        fft_size: 1024,
        ..Default::default()
    };
    let mut e = RemoteEngine::connect(&opts(&addr, "t"), Some(request()), p).unwrap();
    assert_eq!(e.sample_rate(), RATE);

    let (tap, ts_rx) = TsTap::new(1024);
    let mut s = ts_vfo("TS");
    s.ts_tap = Some(tap);
    let id = e.add_vfo(s);

    let mut last = 0;
    let mut rows = 0;
    wait_for("waterfall rows", 15, || {
        for (seq, row) in e.new_rows(last) {
            assert_eq!(row.len(), 1024);
            last = seq;
            rows += 1;
        }
        rows > 10
    });
    wait_for("the spectrum and a carrier", 15, || {
        let f = e.front();
        f.spectrum_db.len() == 1024 && !f.carriers.is_empty()
    });
    wait_for("a constellation", 30, || {
        e.vfo_status(id).is_some_and(|s| !s.scatter.is_empty())
    });
    wait_for("decoded frames", 30, || {
        e.vfo_status(id)
            .and_then(|s| s.fec)
            .is_some_and(|f| f.ok > 2)
    });

    let mut packets = 0;
    wait_for("TS packets at the tap", 30, || {
        while let Ok(batch) = ts_rx.try_recv() {
            assert!(batch.iter().all(|p| p[0] == 0x47));
            packets += batch.len();
        }
        packets > 200
    });
    wait_for("the programme, read on the client", 30, || {
        e.vfo_status(id)
            .and_then(|s| s.fec)
            .and_then(|f| f.ts)
            .is_some_and(|t| {
                t.report
                    .programmes
                    .iter()
                    .any(|p| p.name.as_deref() == Some("DecSAT test signal"))
            })
    });
    // A decoder the server does not run is refused, and said to be.
    let refused = e.add_vfo(VfoSettings::new("BGAN", 0.0, 50_000.0, DecoderKind::Bgan));
    wait_for("the refusal", 5, || e.vfo_status(refused).is_none());
    assert!(
        e.take_gone()
            .iter()
            .any(|(i, why)| *i == refused && why.contains("does not run"))
    );

    e.control().tune(1.5e9);
    wait_for("the retune", 5, || {
        log.tuned.lock().unwrap().as_slice() == [1.5e9]
    });
    assert!(e.control().stats().bytes_per_sec > 0.0);

    // The radio is released with the session.
    let opens = log.opens.load(Ordering::Relaxed);
    drop(e);
    let mut again = None;
    wait_for("the server to free up", 5, || {
        again = RemoteEngine::connect(&opts(&addr, "u"), Some(request()), p).ok();
        again.is_some()
    });
    assert_eq!(log.opens.load(Ordering::Relaxed), opens + 1);
}

#[test]
fn smart_mode_filters_pids_and_restores_the_rate() {
    let (_s, _log, addr) = server(ServerConfig::default());
    let p = SmartParams {
        fft_size: 1024,
        ..Default::default()
    };
    let mut e = RemoteEngine::connect(&opts(&addr, "t"), Some(request()), p).unwrap();
    let (tap, ts_rx) = TsTap::new(1024);
    let mut s = ts_vfo("TS");
    s.ts_tap = Some(tap);
    let id = e.add_vfo(s);
    // Keep only PSI (a PID the signal does not use): its video 0x100 must
    // arrive as nulls, the packet count holding.
    e.set_ts_pids(id, vec![0x1abc]);
    wait_for("the filter to bite", 30, || {
        let mut video_gone = false;
        while let Ok(batch) = ts_rx.try_recv() {
            let pids = batch
                .iter()
                .map(|p| u16::from(p[1] & 0x1f) << 8 | u16::from(p[2]));
            if pids.clone().any(|p| p == 0x1fff) {
                video_gone = pids.clone().all(|p| p != 0x100);
            }
        }
        video_gone
    });
}

#[test]
fn shared_mode_owners_and_campers() {
    let (_s, log, addr) = server(ServerConfig {
        shared: Some(SharedConfig {
            radio: request(),
            params: SmartParams {
                fft_size: 1024,
                ..Default::default()
            },
            max_decoders_each: 1,
            max_decoders: 4,
        }),
        ..Default::default()
    });
    assert_eq!(probe(&opts(&addr, "x")).unwrap().mode, ServerMode::Shared);
    let p = SmartParams::default();
    let mut alice = RemoteEngine::connect(&opts(&addr, "alice"), None, p).unwrap();
    let bob = RemoteEngine::connect(&opts(&addr, "bob"), None, p).unwrap();
    let watcher = RemoteEngine::connect(&opts(&addr, "carol"), None, p).unwrap();
    assert_eq!(bob.mode(), ServerMode::Shared);

    // Both see the band, without opening the radio again.
    wait_for("alice's waterfall", 15, || !alice.new_rows(0).is_empty());
    wait_for("bob's waterfall", 15, || !bob.new_rows(0).is_empty());
    assert_eq!(log.opens.load(Ordering::Relaxed), 1);

    let mine = alice.add_vfo(ts_vfo("Alice TS"));
    wait_for("the decoder list", 5, || bob.shared_list().len() == 1);
    let listed = bob.shared_list()[0].clone();
    assert_eq!(listed.owner.as_deref(), Some("alice"));
    assert_eq!(listed.campers, 0);

    // The limit counts only what a user owns.
    let extra = alice.add_vfo(ts_vfo("One too many"));
    wait_for("the limit", 5, || alice.vfo_status(extra).is_none());
    assert!(
        alice
            .take_gone()
            .iter()
            .any(|(_, why)| why.contains("decoders each"))
    );

    // Bob camps, and gets the decodes and the name of the decoder.
    let camped = bob.camp(listed.server_id);
    wait_for("the camp", 5, || {
        bob.vfo_settings(camped)
            .is_some_and(|s| s.name == "Alice TS")
    });
    assert!(bob.is_camped(camped));
    wait_for("bob's decodes", 30, || {
        bob.vfo_status(camped)
            .and_then(|s| s.fec)
            .is_some_and(|f| f.ok > 2)
    });

    // A camper cannot retune it; the owner can, and the camper hears.
    let mut moved = bob.vfo_settings(camped).unwrap();
    moved.offset_hz = 1234.0;
    bob.update_vfo(camped, moved);
    std::thread::sleep(Duration::from_millis(300));
    wait_for("the list to settle", 5, || {
        watcher.shared_list()[0].campers == 1
    });
    assert_eq!(watcher.shared_list()[0].offset_hz, 40_000.0);

    let mut retuned = alice.vfo_settings(mine).unwrap();
    retuned.offset_hz = 39_000.0;
    alice.update_vfo(mine, retuned);
    wait_for("the owner's retune to reach bob", 5, || {
        bob.vfo_settings(camped)
            .is_some_and(|s| s.offset_hz == 39_000.0)
    });

    // Tuning is refused on a shared radio.
    alice.control().tune(5e9);
    wait_for("the note", 5, || alice.control().take_note().is_some());
    assert!(log.tuned.lock().unwrap().is_empty());

    // The owner leaves; the camper keeps it alive, without an owner.
    drop(alice);
    wait_for("the owner to go", 20, || {
        watcher
            .shared_list()
            .first()
            .is_some_and(|v| v.owner.is_none())
    });
    assert!(bob.vfo_status(camped).is_some());

    // The last one out closes it.
    bob.remove_vfo(camped);
    wait_for("the decoder to close", 5, || {
        watcher.shared_list().is_empty()
    });
}
