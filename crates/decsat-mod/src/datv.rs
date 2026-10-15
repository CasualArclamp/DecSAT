//! A DVB-S2/S2X transmitter for an MPEG-TS (DATV): packets from a file, a
//! UDP stream or the built-in test stream in; PLFRAMEs, pulse-shaped, out.
//!
//! The channel carries a fixed TS rate (the MODCOD's data field per frame
//! at the symbol rate, [`DatvConfig::ts_rate`]). Whatever the input does not
//! fill goes as null packets (PID 1FFFh, ISO/IEC 13818-1 §2.4.3.3), so a
//! slow or bursty input never stalls the carrier. A file is paced by its
//! own rate, read from its PCRs (§2.4.3.4–5), against the channel's clock
//! (the TS bits sent so far), so it plays in real time whether the samples
//! go to a radio or a file; a UDP stream (ffmpeg, OBS) paces itself.
//!
//! Mode adaptation is EN 302 307-1 §5.1 (TS, CCM, single stream, no null
//! packet deletion): user packets back to back across BBFRAMEs, each sync
//! byte replaced by the CRC-8 of the packet before, SYNCD pointing at the
//! first packet starting in the data field.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use decsat_core::{FecFrame, Iq, RollOff};
use decsat_fec::FecParams;
use decsat_frame::{BBHEADER_LEN, BbHeader, StreamFormat, bb_scramble, crc8, ro_bits};

use crate::fec::{BbFrameSource, TsBbFramer};
use crate::framer::{FrameSpec, PlFramer};
use crate::shaper::Shaper;

pub const TS_LEN: usize = 188;
const TS_SYNC: u8 = 0x47;
/// The null packet's PID (ISO/IEC 13818-1 Table 2-3).
pub const NULL_PID: u16 = 0x1FFF;

/// A null packet: PID 1FFFh, payload only, all-ones payload.
pub fn null_packet() -> [u8; TS_LEN] {
    let mut p = [0xFFu8; TS_LEN];
    p[0] = TS_SYNC;
    p[1] = (NULL_PID >> 8) as u8;
    p[2] = NULL_PID as u8;
    p[3] = 0x10;
    p
}

/// Where the transport stream comes from.
pub trait TsInput: Send {
    /// The next packet due by `now` — seconds of TS the channel has sent so
    /// far — or `None`, and a null packet goes instead.
    fn next_packet(&mut self, now: f64) -> Option<[u8; TS_LEN]>;
    /// What it is, for display.
    fn describe(&self) -> String;
    /// Its own rate, bit/s, when known.
    fn rate(&self) -> Option<f64> {
        None
    }
    /// It has nothing more to give (a file played once to its end).
    fn ended(&self) -> bool {
        false
    }
}

/// The 27 MHz PCR in a packet, if it carries one (ISO/IEC 13818-1
/// §2.4.3.4–5: base × 300 + extension).
pub fn pcr_of(p: &[u8; TS_LEN]) -> Option<u64> {
    let af = (p[3] >> 4) & 3;
    if af < 2 || p[4] < 7 || p[5] & 0x10 == 0 {
        return None;
    }
    let base = (u64::from(p[6]) << 25)
        | (u64::from(p[7]) << 17)
        | (u64::from(p[8]) << 9)
        | (u64::from(p[9]) << 1)
        | (u64::from(p[10]) >> 7);
    let ext = (u64::from(p[10] & 1) << 8) | u64::from(p[11]);
    Some(base * 300 + ext)
}

fn pid_of(p: &[u8; TS_LEN]) -> u16 {
    (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2])
}

/// A stream's rate from its PCRs: the bytes between the first and last
/// PCR on the first PID carrying one, over the time between them.
pub fn pcr_rate(packets: impl Iterator<Item = [u8; TS_LEN]>) -> Option<f64> {
    let mut pid = None;
    let (mut first, mut last) = (None, None);
    for (k, p) in packets.enumerate() {
        let Some(pcr) = pcr_of(&p) else { continue };
        if *pid.get_or_insert(pid_of(&p)) != pid_of(&p) {
            continue;
        }
        if first.is_none() {
            first = Some((k, pcr));
        }
        last = Some((k, pcr));
    }
    let ((k0, c0), (k1, c1)) = (first?, last?);
    // The PCR wraps at 2^33 × 300 ticks (26.5 hours): a capture never
    // spans two wraps.
    let ticks = (c1 + (1u64 << 33) * 300 - c0) % ((1u64 << 33) * 300);
    (k1 > k0 && ticks > 0).then(|| ((k1 - k0) * TS_LEN * 8) as f64 / (ticks as f64 / 27e6))
}

/// Packets from a `.ts` file, paced by its own PCR rate (or as fast as the
/// channel takes them when it has no PCR), once or looped.
pub struct TsFile {
    path: PathBuf,
    file: std::io::BufReader<std::fs::File>,
    rate: Option<f64>,
    looped: bool,
    /// Packets sent, over all loops (their due times follow from the rate).
    sent: u64,
    ended: bool,
}

impl TsFile {
    /// Open `path`; its rate is read from the PCRs in its first 32 MB.
    pub fn open(path: &Path, looped: bool) -> std::io::Result<Self> {
        let mut f = std::fs::File::open(path)?;
        let mut head = vec![0u8; 32 << 20];
        let n = read_up_to(&mut f, &mut head)?;
        head.truncate(n);
        let start = sync_start(&head).unwrap_or(0);
        let rate = pcr_rate(
            // Rust note: `as_chunks` views the bytes as whole 188-byte
            // arrays (and a remainder, unused).
            head[start..]
                .as_chunks::<TS_LEN>()
                .0
                .iter()
                .filter(|c| c[0] == TS_SYNC)
                .copied(),
        );
        f.seek(SeekFrom::Start(start as u64))?;
        Ok(TsFile {
            path: path.to_path_buf(),
            file: std::io::BufReader::with_capacity(1 << 20, f),
            rate,
            looped,
            sent: 0,
            ended: false,
        })
    }

    fn read_packet(&mut self) -> Option<[u8; TS_LEN]> {
        let mut p = [0u8; TS_LEN];
        loop {
            match self.file.read_exact(&mut p) {
                Ok(()) if p[0] == TS_SYNC => return Some(p),
                Ok(()) => {
                    // Lost sync: step a byte at a time until it returns.
                    let _ = self.file.seek_relative(1 - TS_LEN as i64);
                }
                Err(_) if self.looped && self.sent > 0 => {
                    self.file.seek(SeekFrom::Start(0)).ok()?;
                }
                Err(_) => {
                    self.ended = true;
                    return None;
                }
            }
        }
    }
}

fn read_up_to(f: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// The first offset with sync bytes 188 and 376 bytes on as well.
fn sync_start(b: &[u8]) -> Option<usize> {
    (0..TS_LEN.min(b.len())).find(|&i| (0..3).all(|k| b.get(i + k * TS_LEN) == Some(&TS_SYNC)))
}

impl TsInput for TsFile {
    fn next_packet(&mut self, now: f64) -> Option<[u8; TS_LEN]> {
        if self.ended {
            return None;
        }
        // Not yet due at the file's own rate: a null goes instead.
        if let Some(r) = self.rate
            && (self.sent * (TS_LEN * 8) as u64) as f64 / r > now
        {
            return None;
        }
        let p = self.read_packet()?;
        self.sent += 1;
        Some(p)
    }

    fn describe(&self) -> String {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match self.rate {
            Some(r) => format!("{name}, {:.3} Mbit/s (from its PCRs)", r / 1e6),
            None => format!("{name}, no PCR: sent as fast as the channel takes it"),
        }
    }

    fn rate(&self) -> Option<f64> {
        self.rate
    }

    fn ended(&self) -> bool {
        self.ended
    }
}

/// Packets from a UDP stream (raw TS, or RTP carrying TS — RFC 2250 —
/// whose 12-byte header is skipped), as ffmpeg or OBS send them; a thread
/// of its own receives, and the newest packets are kept if the channel
/// falls behind.
pub struct TsUdp {
    addr: SocketAddr,
    queue: Arc<Mutex<VecDeque<[u8; TS_LEN]>>>,
    stop: Arc<AtomicBool>,
    pub dropped: Arc<AtomicU64>,
    received: Arc<AtomicU64>,
}

/// Packets the UDP input holds before dropping the oldest: ~2 s at 10 Mbit/s.
const UDP_QUEUE: usize = 16_000;

impl TsUdp {
    /// Listen on `addr` (e.g. 127.0.0.1:1234, or 0.0.0.0:1234 for any host).
    pub fn listen(addr: SocketAddr) -> std::io::Result<Self> {
        let sock = UdpSocket::bind(addr)?;
        sock.set_read_timeout(Some(Duration::from_millis(200)))?;
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicU64::new(0));
        let received = Arc::new(AtomicU64::new(0));
        let (q, s, d, r) = (
            queue.clone(),
            stop.clone(),
            dropped.clone(),
            received.clone(),
        );
        std::thread::Builder::new()
            .name("decsat-tx-udp".into())
            .spawn(move || {
                let mut buf = vec![0u8; 65_536];
                while !s.load(Ordering::Relaxed) {
                    let Ok(n) = sock.recv(&mut buf) else { continue };
                    let mut d_ = &buf[..n];
                    // RTP (version 2) in front of whole packets.
                    if n % TS_LEN == 12 && d_[0] >> 6 == 2 {
                        d_ = &d_[12..];
                    }
                    let mut q = q.lock().unwrap_or_else(|e| e.into_inner());
                    for c in d_
                        .as_chunks::<TS_LEN>()
                        .0
                        .iter()
                        .filter(|c| c[0] == TS_SYNC)
                    {
                        if q.len() >= UDP_QUEUE {
                            q.pop_front();
                            d.fetch_add(1, Ordering::Relaxed);
                        }
                        q.push_back(*c);
                        r.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })?;
        Ok(TsUdp {
            addr,
            queue,
            stop,
            dropped,
            received,
        })
    }
}

impl Drop for TsUdp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl TsInput for TsUdp {
    fn next_packet(&mut self, _now: f64) -> Option<[u8; TS_LEN]> {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    fn describe(&self) -> String {
        format!(
            "UDP {} · {} packets in, {} dropped (the channel too slow)",
            self.addr,
            self.received.load(Ordering::Relaxed),
            self.dropped.load(Ordering::Relaxed)
        )
    }
}

/// DecSAT's own test stream (PSI/SI, a data PID, a radio in MPE), filling
/// the channel.
pub struct TsTest(TsBbFramer);

impl TsTest {
    pub fn new() -> Self {
        TsTest(TsBbFramer::new(0x7E57))
    }
}

impl Default for TsTest {
    fn default() -> Self {
        Self::new()
    }
}

impl TsInput for TsTest {
    fn next_packet(&mut self, _now: f64) -> Option<[u8; TS_LEN]> {
        Some(self.0.packet())
    }

    fn describe(&self) -> String {
        "DecSAT test stream".into()
    }
}

/// What the feed has sent.
#[derive(Debug, Default)]
pub struct FeedCounters {
    pub packets: AtomicU64,
    pub nulls: AtomicU64,
}

/// TS-mode BBFRAMEs from a [`TsInput`], nulls filling the gaps.
pub struct TsFeed {
    input: Box<dyn TsInput>,
    /// The channel's TS rate, bit/s: its clock for pacing the input.
    ts_rate: f64,
    carry: Vec<u8>,
    prev_crc: u8,
    roll_off: RollOff,
    ccm: bool,
    /// BBFRAMEs made: the low roll-offs' RO alternates frame by frame.
    frames: u64,
    pub counters: Arc<FeedCounters>,
    /// The input's description, refreshed as frames go.
    pub described: Arc<Mutex<String>>,
}

impl TsFeed {
    pub fn new(input: Box<dyn TsInput>, ts_rate: f64) -> Self {
        let described = Arc::new(Mutex::new(input.describe()));
        TsFeed {
            input,
            ts_rate,
            carry: Vec::new(),
            prev_crc: 0,
            roll_off: RollOff::R35,
            ccm: true,
            frames: 0,
            counters: Arc::new(FeedCounters::default()),
            described,
        }
    }

    fn packet(&mut self) -> [u8; TS_LEN] {
        let c = &self.counters;
        let sent = c.packets.load(Ordering::Relaxed) + c.nulls.load(Ordering::Relaxed);
        let now = (sent * (TS_LEN * 8) as u64) as f64 / self.ts_rate.max(1.0);
        match self.input.next_packet(now) {
            Some(p) => {
                c.packets.fetch_add(1, Ordering::Relaxed);
                p
            }
            None => {
                c.nulls.fetch_add(1, Ordering::Relaxed);
                null_packet()
            }
        }
    }
}

impl BbFrameSource for TsFeed {
    fn set_roll_off(&mut self, roll_off: RollOff) {
        self.roll_off = roll_off;
    }

    fn set_ccm(&mut self, ccm: bool) {
        self.ccm = ccm;
    }

    fn next_frame(&mut self, bytes: usize) -> Vec<u8> {
        let field = bytes - BBHEADER_LEN;
        let mut data = Vec::with_capacity(field);
        let carried = self.carry.len().min(field);
        data.extend_from_slice(&self.carry[..carried]);
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
        if let Ok(mut d) = self.described.try_lock() {
            *d = self.input.describe();
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

/// What the transmitter sends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DatvConfig {
    /// A DVB-S2 MODCOD (1..=28) or an S2X PLS code (128..=255).
    pub modcod: u8,
    /// S2 only: short FECFRAMEs (16 200 bits) rather than normal (64 800).
    pub short_fecframe: bool,
    pub pilots: bool,
    pub roll_off: RollOff,
    /// Symbols per second.
    pub symbol_rate: f64,
    /// The PL scrambling sequence (EN 302 307-1 §5.5.4); 0 is the default.
    pub gold_code: u32,
    /// The lowest and highest sample rates the output may use (a HackRF:
    /// 2 and 20 MS/s). Within them it is the symbol rate times a whole
    /// number of samples a symbol: 4 where that fits — at 2 the carrier
    /// fills the band, hard on a receiver and on the DAC's images — more to
    /// reach the lowest, down to 2 to stay under the highest.
    pub min_sample_rate: f64,
    pub max_sample_rate: f64,
}

impl Default for DatvConfig {
    fn default() -> Self {
        DatvConfig {
            modcod: 4, // QPSK 1/2
            short_fecframe: false,
            pilots: true,
            roll_off: RollOff::R20,
            symbol_rate: 1e6,
            gold_code: 0,
            min_sample_rate: 2e6,
            max_sample_rate: 20e6,
        }
    }
}

impl DatvConfig {
    pub fn spec(&self) -> FrameSpec {
        FrameSpec::new(self.modcod, self.short_fecframe, self.pilots)
    }

    /// The code's parameters, if the MODCOD has data frames to send.
    pub fn fec(&self) -> Option<FecParams> {
        let m = self.spec().info().modcod()?;
        FecParams::new(m.frame, m.rate)
    }

    /// The TS rate the channel carries, bit/s: the data field (Kbch less the
    /// 80-bit BBHEADER) per PLFRAME, at the symbol rate.
    pub fn ts_rate(&self) -> Option<f64> {
        let p = self.fec()?;
        let frame = f64::from(self.spec().info().plframe_len);
        Some((p.k_bch - BBHEADER_LEN * 8) as f64 * self.symbol_rate / frame)
    }

    /// Samples a symbol.
    pub fn sps(&self) -> usize {
        let want = ((self.min_sample_rate / self.symbol_rate).ceil() as usize).max(4);
        let most = (self.max_sample_rate / self.symbol_rate).floor() as usize;
        want.min(most).max(2)
    }

    pub fn sample_rate(&self) -> f64 {
        self.symbol_rate * self.sps() as f64
    }

    /// The occupied bandwidth, Hz: the symbol rate times (1 + roll-off).
    pub fn bandwidth(&self) -> f64 {
        self.symbol_rate * (1.0 + self.roll_off.as_f64())
    }

    /// Whether the MODCOD's frames carry data (not a dummy, reserved or
    /// VL-SNR code — those have no TS to carry here).
    pub fn carries_data(&self) -> bool {
        self.ts_rate().is_some()
            && self
                .spec()
                .info()
                .modcod()
                .is_some_and(|m| m.frame != FecFrame::Medium)
    }
}

/// The MODCODs that carry a TS here, by number and name: DVB-S2's 1–28,
/// then the S2X codes with data frames (EN 302 307-2 Table 17a), in PLS
/// order.
pub fn data_modcods() -> Vec<(u8, String)> {
    (1..=255u8)
        .filter(|&m| m <= 28 || m >= 128)
        .filter(|&m| m < 128 || m % 2 == 0)
        .filter(|&m| {
            DatvConfig {
                modcod: m,
                ..DatvConfig::default()
            }
            .carries_data()
        })
        .map(|m| (m, decsat_core::modcod_name(m)))
        .collect()
}

/// A MODCOD by number ("4", "138") or name ("QPSK 1/2", "qpsk-1/2",
/// "8psk 23/36"): DVB-S2's first where S2X has the same name.
pub fn modcod_by_name(s: &str) -> Option<u8> {
    if let Ok(n) = s.trim().parse::<u8>() {
        return Some(n);
    }
    let norm = |x: &str| {
        x.chars()
            .filter(|c| !matches!(c, ' ' | '-' | '_'))
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let want = norm(s);
    data_modcods()
        .into_iter()
        .find(|(_, name)| norm(name) == want)
        .map(|(m, _)| m)
}

/// The modulator: TS in, baseband samples at [`DatvConfig::sample_rate`] out,
/// a PLFRAME at a time.
pub struct DatvModulator {
    pub config: DatvConfig,
    framer: PlFramer,
    shaper: Shaper,
    symbols: Vec<Iq>,
    pub counters: Arc<FeedCounters>,
    pub input_described: Arc<Mutex<String>>,
    frames: u64,
}

impl DatvModulator {
    /// # Panics
    /// When the MODCOD carries no data ([`DatvConfig::carries_data`]).
    pub fn new(config: DatvConfig, input: Box<dyn TsInput>) -> Self {
        let ts_rate = config.ts_rate().expect("a MODCOD that carries data");
        let feed = TsFeed::new(input, ts_rate);
        let counters = feed.counters.clone();
        let input_described = feed.described.clone();
        let mut framer = PlFramer::new(config.gold_code, 1).with_source(Box::new(feed));
        framer.set_roll_off(config.roll_off);
        DatvModulator {
            shaper: Shaper::new(config.sps(), config.roll_off.as_f64(), 16),
            config,
            framer,
            symbols: Vec::new(),
            counters,
            input_described,
            frames: 0,
        }
    }

    /// Append one PLFRAME's samples to `out` (unit average power).
    pub fn next_frame(&mut self, out: &mut Vec<Iq>) {
        self.symbols.clear();
        self.framer.build(self.config.spec(), &mut self.symbols);
        self.shaper.process(&self.symbols, out);
        self.frames += 1;
    }

    /// The symbols alone, one PLFRAME's (for loopback tests).
    pub fn next_frame_symbols(&mut self, out: &mut Vec<Iq>) {
        self.framer.build(self.config.spec(), out);
        self.frames += 1;
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }
}

/// Samples to signed 8-bit IQ (a HackRF's, `cs8`), scaled by `gain` and
/// clipped: unit-power samples at `gain` 40 peak near 127 for QPSK.
pub fn to_cs8(samples: &[Iq], gain: f32, out: &mut Vec<u8>) {
    out.reserve(samples.len() * 2);
    for s in samples {
        out.push((s.re * gain).round().clamp(-127.0, 127.0) as i8 as u8);
        out.push((s.im * gain).round().clamp(-127.0, 127.0) as i8 as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decsat_ts::TsDeframer;

    /// Packets from a list, then nothing.
    struct TsList(VecDeque<[u8; TS_LEN]>);

    impl TsInput for TsList {
        fn next_packet(&mut self, _now: f64) -> Option<[u8; TS_LEN]> {
            self.0.pop_front()
        }
        fn describe(&self) -> String {
            "list".into()
        }
    }

    fn numbered(n: usize) -> Vec<[u8; TS_LEN]> {
        (0..n)
            .map(|k| {
                let mut p = [0u8; TS_LEN];
                p[0] = TS_SYNC;
                p[1] = 0x01;
                p[2] = 0x00;
                p[3] = 0x10 | (k as u8 & 0x0F);
                p[4..8].copy_from_slice(&(k as u32).to_be_bytes());
                p
            })
            .collect()
    }

    /// The feed's BBFRAMEs, descrambled and deframed as a receiver does.
    fn deframe(frames: &[Vec<u8>]) -> Vec<[u8; TS_LEN]> {
        let mut d = TsDeframer::new();
        let mut out = Vec::new();
        for f in frames {
            let mut f = f.clone();
            bb_scramble(&mut f);
            let h = BbHeader::parse(&f).expect("BBHEADER");
            let field = &f[BBHEADER_LEN..BBHEADER_LEN + h.dfl as usize / 8];
            d.data_field(field, h.syncd, h.npd, h.issyi, &mut out);
        }
        out
    }

    #[test]
    fn the_ts_goes_through_whole_and_nulls_fill_the_rest() {
        let sent = numbered(500);
        let mut feed = TsFeed::new(Box::new(TsList(sent.iter().copied().collect())), 1e6);
        // QPSK 1/2 normal: 4027 data bytes a frame.
        let p = DatvConfig::default().fec().unwrap();
        let frames: Vec<Vec<u8>> = (0..30)
            .map(|_| feed.next_frame(p.bbframe_bytes()))
            .collect();
        let got = deframe(&frames);
        // Every packet sent, in order and intact, then only nulls.
        assert_eq!(&got[..sent.len()], &sent[..]);
        assert!(got[sent.len()..].iter().all(|p| pid_of(p) == NULL_PID));
        assert!(got.len() > sent.len() + 100);
        assert_eq!(feed.counters.packets.load(Ordering::Relaxed), 500);
    }

    #[test]
    fn the_channel_rate_follows_the_modcod() {
        // QPSK 1/2, normal frames, no pilots, 1 MS/s: (32 208 − 80) bits a
        // frame of 90 + 32 400 symbols.
        let c = DatvConfig {
            pilots: false,
            ..DatvConfig::default()
        };
        let r = c.ts_rate().unwrap();
        assert!((r - 32_128.0 / 32_490.0 * 1e6).abs() < 1.0, "{r}");
        // 8PSK 3/4 with pilots at 2 MS/s: 4 a symbol; (48 408 − 80) bits a
        // frame of 90 + 21 600 + 14 × 36.
        let c = DatvConfig {
            modcod: 14,
            symbol_rate: 2e6,
            ..DatvConfig::default()
        };
        assert_eq!(c.sps(), 4);
        let r = c.ts_rate().unwrap();
        assert!((r - 48_328.0 / 22_194.0 * 2e6).abs() < 1.0, "{r}");
        // A slow carrier gets more samples a symbol to reach 2 MS/s, a fast
        // one fewer to stay under 20.
        let c = DatvConfig {
            symbol_rate: 333_000.0,
            ..DatvConfig::default()
        };
        assert_eq!(c.sps(), 7);
        let c = DatvConfig {
            symbol_rate: 8e6,
            ..DatvConfig::default()
        };
        assert_eq!(c.sps(), 2);
    }

    /// A TS with a PCR every 40 packets at 2 Mbit/s.
    fn paced_stream(packets: usize, rate: f64) -> Vec<[u8; TS_LEN]> {
        let mut v = numbered(packets);
        for (k, p) in v.iter_mut().enumerate().filter(|(k, _)| k % 40 == 0) {
            let t = (k * TS_LEN * 8) as f64 / rate;
            let pcr = (t * 27e6) as u64;
            let (base, ext) = (pcr / 300, pcr % 300);
            p[3] = 0x30; // adaptation field and payload
            p[4] = 7;
            p[5] = 0x10; // PCR flag
            p[6] = (base >> 25) as u8;
            p[7] = (base >> 17) as u8;
            p[8] = (base >> 9) as u8;
            p[9] = (base >> 1) as u8;
            p[10] = ((base & 1) << 7) as u8 | 0x7E | (ext >> 8) as u8;
            p[11] = ext as u8;
        }
        v
    }

    #[test]
    fn a_file_plays_at_its_own_rate_with_nulls_between() {
        let v = paced_stream(4000, 2e6);
        assert!((pcr_rate(v.iter().copied()).unwrap() - 2e6).abs() < 1e3);
        let dir = std::env::temp_dir().join(format!("decsat-datv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("paced.ts");
        std::fs::write(&path, v.concat()).unwrap();
        let file = TsFile::open(&path, false).unwrap();
        assert!(
            file.describe().contains("2.000 Mbit/s"),
            "{}",
            file.describe()
        );
        // A channel twice as fast: about one packet in two is the file's.
        let mut feed = TsFeed::new(Box::new(file), 4e6);
        let bytes = DatvConfig::default().fec().unwrap().bbframe_bytes();
        for _ in 0..40 {
            feed.next_frame(bytes);
        }
        let (p, n) = (
            feed.counters.packets.load(Ordering::Relaxed) as f64,
            feed.counters.nulls.load(Ordering::Relaxed) as f64,
        );
        assert!((p / (p + n) - 0.5).abs() < 0.02, "{p} packets, {n} nulls");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn modcods_by_name_and_number() {
        assert_eq!(modcod_by_name("QPSK 1/2"), Some(4));
        assert_eq!(modcod_by_name("qpsk-1/2"), Some(4));
        assert_eq!(modcod_by_name("8psk 3/4"), Some(14));
        assert_eq!(modcod_by_name("17"), Some(17));
        assert_eq!(modcod_by_name("QPSK 13/45"), Some(132));
        assert_eq!(modcod_by_name("nonsense"), None);
        let all = data_modcods();
        assert_eq!(all.iter().filter(|(m, _)| *m <= 28).count(), 28);
        assert!(all.len() > 60, "{}", all.len());
    }

    #[test]
    fn samples_come_out_at_the_configured_rate() {
        let c = DatvConfig::default();
        let mut m = DatvModulator::new(c, Box::new(TsTest::new()));
        let mut out = Vec::new();
        m.next_frame(&mut out);
        // One frame: its symbols times the samples a symbol.
        let frame = c.spec().info().plframe_len as usize;
        assert_eq!(out.len(), frame * c.sps());
        let p = out.iter().map(|s| s.norm_sqr()).sum::<f32>() / out.len() as f32;
        assert!((p - 1.0).abs() < 0.15, "power {p}");
        let mut b = Vec::new();
        to_cs8(&out, 40.0, &mut b);
        assert_eq!(b.len(), out.len() * 2);
    }
}
