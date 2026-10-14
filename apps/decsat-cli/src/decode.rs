//! `decsat decode`: run one decoder over a capture and report what it finds
//! — the same VFO the GUI runs, fed from the file, with a status line every
//! couple of seconds and a full report at the end (structure found, payload,
//! E1 timeslots, text, frame structure).
//!
//! With `--hackrf <MHz>` the source is the HackRF instead (receive only, its
//! antenna power forced off): a status line every 30 s, a report at the end
//! (`--seconds`) — for watching a carrier unattended, e.g. a FastLink link
//! with `--record-on-activity` until a voice channel speaks.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use decsat_core::{Modulation, SampleFormat};
use decsat_engine::{DecoderKind, Engine, EngineOptions, SourceState, VfoSettings, VfoStatus};
use decsat_io::{IqFileReader, IqSource};

pub struct DecodeArgs {
    pub file: Option<PathBuf>,
    /// Live: the HackRF's centre frequency, MHz, and its gains.
    pub hackrf: Option<f64>,
    pub lna: u16,
    pub vga: u16,
    pub amp: bool,
    /// Stop after this long.
    pub seconds: Option<f64>,
    /// fastlink: record while a TDM channel is active.
    pub record_on_activity: bool,
    pub format: Option<SampleFormat>,
    pub rate: Option<f64>,
    pub decoder: String,
    pub offset: f64,
    pub bandwidth: Option<f64>,
    pub symbol_rate: Option<f64>,
    pub modulation: Option<String>,
    pub out: Option<PathBuf>,
    pub fast: bool,
    pub e1_record: Option<u8>,
    /// fastlink: record the TDM multiplex's call voice.
    pub tdm_record: bool,
    /// Record a link's programme audio (apt-X in IBS).
    pub programme_record: bool,
    /// psk: how the .bin numbers the symbols.
    pub labels: String,
    /// cid: low-SNR mode.
    pub low_snr: bool,
    /// cid: the clock correction, ppm.
    pub clock_ppm: f64,
    /// psk: bits rather than symbol bytes.
    pub bits: bool,
    /// DVB-S2: append the good BBFRAMEs here.
    pub bbframes: Option<PathBuf>,
    /// Feed the IP live to a Wireshark started for it.
    pub wireshark: bool,
}

fn decoder_by_name(name: &str) -> Result<DecoderKind> {
    Ok(match name {
        "id" | "identify" => DecoderKind::Identify,
        "ip" | "gse" => DecoderKind::Dvbs2Ip,
        "ts" => DecoderKind::Dvbs2Ts,
        "dvbs" => DecoderKind::DvbsTs,
        "tpc" | "tpc2964" => DecoderKind::Tpc2964,
        "cdm600" | "cdm600-voice" | "cdm" => DecoderKind::Cdm600Voice,
        "fastlink" | "fl" => DecoderKind::FastLink,
        "viterbi" | "vit" => DecoderKind::Viterbi,
        "cdm570" | "570" => DecoderKind::Cdm570,
        "trunk" | "cdm570-trunk" => DecoderKind::Cdm570Trunk,
        "auto" | "cdm-auto" => DecoderKind::CdmAuto,
        "cid" | "carrier-id" => DecoderKind::CarrierId,
        "psk" => DecoderKind::PskSymbols,
        "tdma" => DecoderKind::Tdma,
        other => bail!(
            "unknown decoder `{other}` (id, ip, ts, dvbs, tpc, cdm600, cdm570, trunk, auto, fastlink, viterbi, cid, psk, tdma)"
        ),
    })
}

fn modulation_by_name(name: &str) -> Result<Modulation> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "bpsk" => Modulation::Bpsk,
        "qpsk" => Modulation::Qpsk,
        "8psk" => Modulation::Psk8,
        "16apsk" => Modulation::Apsk16,
        "32apsk" => Modulation::Apsk32,
        "8qam" => Modulation::Qam8,
        "16qam" => Modulation::Qam16,
        "64qam" => Modulation::Qam64,
        other => bail!("unknown modulation `{other}`"),
    })
}

/// The HackRF at `mhz`, `rate` samples a second.
#[cfg(feature = "hackrf")]
fn hackrf_source(a: &DecodeArgs, mhz: f64, rate: f64) -> Result<Box<dyn IqSource>> {
    use decsat_io::{HackRfGains, HackRfSettings, HackRfSource};
    let src = HackRfSource::open(HackRfSettings {
        center_hz: (mhz * 1e6).round() as u64,
        sample_rate: rate.round() as u32,
        gains: HackRfGains {
            amp: a.amp,
            lna_db: a.lna,
            vga_db: a.vga,
        }
        .snapped(),
    })?;
    Ok(Box::new(src))
}

#[cfg(not(feature = "hackrf"))]
fn hackrf_source(_: &DecodeArgs, _: f64, _: f64) -> Result<Box<dyn IqSource>> {
    bail!("this build has no HackRF support (the `hackrf` feature)")
}

pub fn decode(a: DecodeArgs) -> Result<()> {
    let kind = decoder_by_name(&a.decoder)?;
    let live = a.hackrf.is_some();
    let (source, rate): (Box<dyn IqSource>, f64) = match (a.hackrf, &a.file) {
        (Some(mhz), _) => {
            let rate = a.rate.unwrap_or(2e6);
            let src = hackrf_source(&a, mhz, rate)?;
            println!(
                "live from the HackRF at {mhz:.4} MHz, {:.3} MS/s, LNA {} dB, VGA {} dB{} \
                 (receive only)",
                rate / 1e6,
                a.lna,
                a.vga,
                if a.amp { ", amp on" } else { "" }
            );
            (src, rate)
        }
        (None, Some(file)) => {
            let (fmt, rate) = crate::capture_format_rate(file, a.format, a.rate)?;
            let reader = IqFileReader::open(file, fmt, rate, 1 << 16)
                .with_context(|| format!("opening {}", file.display()))?;
            println!("decoding {} at {:.3} MS/s", reader.describe(), rate / 1e6);
            (Box::new(reader), rate)
        }
        (None, None) => bail!("give a capture, or --hackrf <MHz>"),
    };

    // Real time by default: a VFO that falls behind drops blocks, and a
    // decoder fed with holes reports them as faults of the signal.
    let mut eng = Engine::start(
        source,
        EngineOptions {
            realtime: !a.fast || live,
            loop_file: false,
            ..Default::default()
        },
    );
    let mut s = VfoSettings::new("decode", a.offset, a.bandwidth.unwrap_or(0.9 * rate), kind);
    s.symbol_rate = a.symbol_rate;
    if let Some(m) = &a.modulation {
        s.psk_modulation = Some(modulation_by_name(m)?);
    }
    {
        use decsat_engine::psk::SymbolLabels;
        s.symbol_labels = match a.labels.to_ascii_lowercase().as_str() {
            "standard" | "std" => SymbolLabels::Standard,
            "natural" | "position" => SymbolLabels::Natural,
            "gray" | "grey" => SymbolLabels::Gray,
            other => bail!("--labels: `{other}` (want standard, natural or gray)"),
        };
    }
    s.cid_low_snr = a.low_snr;
    s.clock_ppm = a.clock_ppm;
    s.psk_bits = a.bits;
    s.bbframes = a.bbframes.clone();
    s.wireshark = a.wireshark.then_some(1);
    if let Some(dir) = &a.out {
        // Record throughout, or (--record-on-activity) only while a channel
        // of a FastLink's multiplex is active.
        s.record = !a.record_on_activity;
        s.pcap = s.record;
        s.record_on_activity = a.record_on_activity;
        s.record_dir = dir.clone();
    } else if a.record_on_activity {
        bail!("--record-on-activity needs --out");
    }
    if a.e1_record.is_some() && a.out.is_none() {
        bail!("--e1-record needs --out");
    }
    s.e1_record = a.e1_record;
    // Offline the voice is recorded (--e1-record), not played: the file
    // may run faster than real time.
    s.voice_auto = false;
    if a.tdm_record && a.out.is_none() {
        bail!("--tdm-record needs --out");
    }
    s.tdm_record = a.tdm_record;
    if a.programme_record && a.out.is_none() {
        bail!("--programme-record needs --out");
    }
    s.programme_record = a.programme_record;
    let id = eng.add_vfo(s);

    let t0 = Instant::now();
    let mut last_line = Instant::now();
    let mut ended_at: Option<Instant> = None;
    // --wireshark: Wireshark has been fed, so its closing ends the run.
    let mut wireshark_fed = false;
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let front = eng.front();
        if let SourceState::Failed(e) = &front.state {
            bail!("source failed: {e}");
        }
        if matches!(front.state, SourceState::Ended) && ended_at.is_none() {
            ended_at = Some(Instant::now());
        }
        let st = eng.vfo_status(id);
        if a.wireshark {
            let ip = st
                .as_ref()
                .and_then(|s| s.fec.as_ref())
                .and_then(|f| f.gse.as_ref());
            if let Some(e) = ip.and_then(|g| g.wireshark_error.as_ref()) {
                bail!("{e}");
            }
            let feeding = ip.is_some_and(|g| g.wireshark.is_some());
            if wireshark_fed && !feeding && ended_at.is_none() {
                println!("Wireshark closed");
                ended_at = Some(Instant::now());
            }
            wireshark_fed |= feeding;
        }
        let every = Duration::from_secs(if live { 30 } else { 2 });
        if last_line.elapsed() >= every {
            if let Some(st) = &st {
                let rec = st
                    .fec
                    .as_ref()
                    .filter(|_| a.record_on_activity)
                    .map(|f| format!(" · {} recordings", f.raw_triggered))
                    .unwrap_or_default();
                // The VFO's own thread: its time over the signal's (over
                // 100 % it falls behind and drops blocks, in real time).
                println!(
                    "[{:5.1} s] {}{rec} · CPU {:.0} %{}",
                    t0.elapsed().as_secs_f64(),
                    st.message,
                    100.0 * st.load,
                    if st.dropped > 0 {
                        format!(" · {} blocks dropped", st.dropped)
                    } else {
                        String::new()
                    }
                );
            }
            last_line = Instant::now();
        }
        if a.seconds.is_some_and(|s| t0.elapsed().as_secs_f64() >= s) && ended_at.is_none() {
            ended_at = Some(Instant::now());
        }
        // A file that ended: wait until the decoder has worked through
        // everything it was given. Live (or stopped by --seconds): give the
        // FEC thread a moment.
        let file_ended = matches!(front.state, SourceState::Ended);
        if ended_at.is_some_and(|t| file_ended || t.elapsed() >= Duration::from_secs(3)) {
            if file_ended && !wait_settled(&eng, SETTLE_CAP) {
                println!(
                    "(stopped waiting after {} s: the decoder is still {} blocks or frames behind)",
                    SETTLE_CAP.as_secs(),
                    eng.pending_work()
                );
            }
            match eng.vfo_status(id) {
                Some(st) => report(&st),
                None => println!("no status from the VFO"),
            }
            // Whatever is still queued is not wanted now: leave at once.
            eng.discard_vfos();
            return Ok(());
        }
    }
}

/// Longest wait, after a file ends, for the decoders to catch up.
pub(crate) const SETTLE_CAP: Duration = Duration::from_secs(120);

/// After a file's end, wait until every VFO has worked through what it was
/// given, then a status tick more so the reports show the last of it.
/// `false` when `cap` ran out first. Notes progress every 10 s.
pub(crate) fn wait_settled(eng: &Engine, cap: Duration) -> bool {
    let t0 = Instant::now();
    let mut note = Instant::now();
    let mut since: Option<Instant> = None;
    loop {
        if eng.settled() {
            // Rust note: `get_or_insert_with` fills the Option the first
            // time and returns what is there after.
            if since.get_or_insert_with(Instant::now).elapsed() >= Duration::from_millis(600) {
                return true;
            }
        } else {
            since = None;
        }
        if t0.elapsed() >= cap {
            return false;
        }
        if note.elapsed() >= Duration::from_secs(10) {
            println!(
                "… still decoding ({} blocks or frames queued)",
                eng.pending_work()
            );
            note = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A TDMA channel's code search, in words.
pub(crate) fn tdma_code_text(v: &decsat_engine::tdma::CodeView, layout: bool) -> String {
    match v.code {
        Some(code) => format!(
            "{} (its parity check holds in {:.0} % of the bursts' data)",
            code.name(),
            100.0 * v.fit
        ),
        None if v.given_up => format!(
            "no rate-1/2 convolutional code up to K = 10 in the bursts' data ({} looks)",
            v.looks
        ),
        None if !layout => "waiting for the layout (the bursts' data, their phase known)".into(),
        None => format!("looking ({} coded bits, {} looks)", v.bits, v.looks),
    }
}

pub(crate) fn report(st: &VfoStatus) {
    println!("\n== {}", st.message);
    if let Some(c) = &st.carrier {
        println!(
            "carrier: {} {}, {}, offset {:+.0} Hz",
            c.modulation.name(),
            if c.locked { "locked" } else { "not locked" },
            c.snr_text(),
            c.offset_hz
        );
    }
    if let Some(rs) = st.symbol_rate {
        println!("symbol rate: {:.1} S/s", rs);
    }
    if let Some(id) = &st.identification {
        println!("identify: {}", id.summary());
        if let Some(p) = &id.frame_structure {
            println!(
                "frames: {} (over {} frames, {:.0}× the noise)",
                p.describe(),
                p.frames,
                p.prominence
            );
        }
    }
    if let Some(t) = &st.text {
        print_bit_text("text", t);
    }
    if let Some(t) = &st.tdma {
        print_tdma(t);
    }
    if let Some(c) = &st.cid {
        use decsat_engine::cid::{guid_mac, guid_text};
        let s = &c.stats;
        println!(
            "dvb-cid: {:.0} kchip/s, code {}, {} searches, {:+.1} Hz, {:.1} dB a bit, {} bits, {} frames ({} failed){}",
            c.chip_rate / 1e3,
            if s.acquired { "found" } else { "not found" },
            s.searches,
            s.offset_hz,
            s.snr_db,
            s.bits,
            s.frames,
            s.bad_frames,
            if c.wide_enough {
                ""
            } else {
                " — VFO narrower than the CID's band"
            }
        );
        let r = &s.report;
        if let Some(g) = r.guid {
            println!("  identifier {}", guid_text(g));
            if let Some(m) = guid_mac(g) {
                println!("  MAC {m}");
            }
        }
        if let (Some(lat), Some(lon)) = (r.latitude(), r.longitude()) {
            println!("  position {lat:.5}, {lon:.5}");
        }
        if let Some(t) = r.telephone() {
            println!("  telephone {t}");
        }
        if let Some(t) = r.user_text() {
            println!("  text \"{t}\"");
        }
    }
    let Some(f) = &st.fec else { return };
    // DVB-S2: what the FEC made of the frames, and the latest BBHEADER.
    if f.frames > 0 || f.last_header.is_some() {
        println!(
            "s2 fec: {} frames, {} good, {} BCH failed, {} header CRC failed, {} dropped, \
             LDPC {:.1} iterations a frame{}",
            f.frames,
            f.ok,
            f.bch_failed,
            f.crc_failed,
            f.dropped,
            f.iterations as f64 / f.frames.max(1) as f64,
            f.es_n0_db
                .map(|e| format!(", Es/N0 {e:.1} dB from the known symbols"))
                .unwrap_or_default()
        );
        let mc: Vec<String> = f
            .per_modcod
            .iter()
            .map(|(&m, &(good, total))| format!("{} {good}/{total}", decsat_core::modcod_name(m)))
            .collect();
        if !mc.is_empty() {
            println!("  by MODCOD: {}", mc.join(" · "));
        }
        if let Some(h) = &f.last_header {
            println!(
                "  BBHEADER: {}, {}, {}{}{}, ISI {}, UPL {} bits, DFL {} bits, SYNC {:02x}h, \
                 SYNCD {}{}",
                h.format.label(),
                if h.single_stream {
                    "single stream"
                } else {
                    "multiple streams"
                },
                if h.ccm { "CCM" } else { "ACM/VCM" },
                if h.issyi { ", ISSY" } else { "" },
                if h.npd { ", NPD" } else { "" },
                h.isi,
                h.upl,
                h.dfl,
                h.sync,
                h.syncd,
                if h.high_efficiency {
                    ", high-efficiency mode"
                } else {
                    ""
                }
            );
        }
        if f.streams.len() > 1 {
            let s: Vec<String> = f.streams.iter().map(|(i, n)| format!("{i}: {n}")).collect();
            println!("  streams (ISI: good frames): {}", s.join(", "));
        }
        if f.ts_frames > 0 {
            println!("  {} TS-mode frames", f.ts_frames);
        }
        if f.payload_bps > 0.0 {
            println!("  payload {:.1} kbit/s (DFL)", f.payload_bps / 1e3);
        }
        if let Some(g) = &f.gse {
            if g.trailers > 0 {
                println!(
                    "  vendor CRC-32 at the end of the data field (over BBHEADER and field, \
                     from 0): {} frames",
                    g.trailers
                );
            }
            for v in &g.variants {
                let s = &v.stats;
                if s.packets + s.errors > 0 {
                    println!(
                        "  gse {:?}: {} packets ({} whole, {} start, {} middle, {} end, {} padding), \
                         {} errors, {} reassembled, CRC {} ok / {} bad{}, {} IP",
                        v.variant,
                        s.packets,
                        s.whole,
                        s.start,
                        s.middle,
                        s.end,
                        s.padding,
                        s.errors,
                        s.reassembled,
                        s.crc_ok,
                        s.crc_bad,
                        if s.vouched > 0 {
                            format!(" / {} by the frame CRC", s.vouched)
                        } else {
                            String::new()
                        },
                        v.ip_packets
                    );
                }
            }
            if !g.other_protocols.is_empty() {
                let p: Vec<String> = g
                    .other_protocols
                    .iter()
                    .map(|(t, n)| format!("{t:04x}h ×{n}"))
                    .collect();
                println!("  non-IP PDUs by protocol type: {}", p.join(", "));
                if let Some(h) = g.other_entropy {
                    println!(
                        "  non-IP PDU bytes: {h:.2} bits/byte{}",
                        if g.other_encrypted {
                            " (random: encrypted)"
                        } else {
                            ""
                        }
                    );
                }
            }
        }
    }
    if let Some(t) = &f.tpc {
        println!(
            "tpc: {}UW {}, {}, structure {}, fit {:.0} %, {} frames ({} decoded, {} failed), BER {:.2e}, {} slips, {} UW misses{}",
            t.code.map_or(String::new(), |c| format!("{}, ", c.name())),
            if t.uw_locked { "found" } else { "not found" },
            t.orientation.as_deref().unwrap_or("-"),
            t.structure.as_deref().unwrap_or("not identified"),
            t.fit * 100.0,
            t.frames,
            t.decoded,
            t.failed,
            t.channel_ber(),
            t.slips,
            t.uw_misses,
            t.overhead
                .map_or(String::new(), |o| format!(", overhead {o:06X}h"))
        );
    }
    if let Some(v) = &f.viterbi {
        println!(
            "viterbi: rate {}, {}, channel BER {:.2e}, {} bits, {} searches, {} relocks, {} turns followed",
            v.rate.map_or("not found", |r| r.name()),
            v.orientation.as_deref().unwrap_or("-"),
            v.channel_ber,
            v.bits,
            v.searches,
            v.losses,
            v.turns
        );
    }
    if f.k7_stopped {
        println!("viterbi: the K = 7 search stopped (no rate fitted)");
    }
    // (The rate-½ search's line only when the K = 7 code has not locked.)
    if let Some(c) = f
        .conv
        .as_ref()
        .filter(|_| f.viterbi.as_ref().is_none_or(|v| v.rate.is_none()))
    {
        match c.code {
            Some(code) => println!(
                "conv: {}, {}, channel BER {:.2e}, {} bits, {} looks, {} relocks, {} turns followed",
                code.name(),
                c.orientation.as_deref().unwrap_or("-"),
                c.channel_ber,
                c.bits,
                c.searches,
                c.losses,
                c.turns
            ),
            None if c.gave_up => println!(
                "conv: no rate-1/2 convolutional code up to K = 10 in {} looks",
                c.searches
            ),
            None => println!("conv: looking for a rate-1/2 code ({} looks)", c.searches),
        }
    }
    if let Some(t) = &f.fastlink {
        println!(
            "fastlink: sync {}, {}, {} frames, {} codewords ({} decoded, {} failed), BER {:.2e}, {} slips, {} sync misses",
            if t.locked { "found" } else { "not found" },
            t.orientation.as_deref().unwrap_or("-"),
            t.frames,
            t.codewords,
            t.decoded,
            t.failed,
            t.channel_ber(),
            t.slips,
            t.uw_misses
        );
    }
    if let Some(p) = &f.payload {
        print_payload(p);
    }
    if let Some(r) = &f.radar {
        print_radar(&r.radar);
        if let Some((p, n)) = &r.log {
            println!("  logged {n} track reports to {}", p.display());
        }
        if let Some(e) = &r.error {
            println!("  log: {e}");
        }
    }
    if let Some(t) = &f.fast_tdm {
        print_fast_tdm(t);
    }
    if let Some(t) = &f.tdm {
        print_tdm(t);
    }
    if let Some(p) = &f.programme {
        print_programme(p);
    }
    if let Some(e) = &f.e1 {
        if let Some((n, lost)) = e.packets {
            println!("e1: {n} timeslot packets · {lost} lost");
        } else {
            println!(
                "e1: {} · {} frames · {} FAS errors · {} losses{}",
                if e.stats.locked {
                    "aligned"
                } else {
                    "not aligned"
                },
                e.stats.frames,
                e.stats.fas_errors,
                e.stats.losses,
                if e.stats.cas { " · CAS" } else { "" }
            );
        }
        println!("  {}", e.source);
        if let Some((p, n)) = &e.record_file {
            println!("  recorded to {} ({n} bytes)", p.display());
        }
        for (ts, name) in &e.rows {
            let ts = *ts as usize;
            let db = e.levels_db.get(ts).copied().unwrap_or(f32::NEG_INFINITY);
            if db > -60.0 {
                use decsat_modem::e1::Coding;
                let voice = e.voice.get(ts).map_or(&[][..], Vec::as_slice);
                let note = match e.coding.get(ts).copied().unwrap_or_default() {
                    _ if !voice.is_empty() => {
                        let lanes: Vec<String> = voice
                            .iter()
                            .map(|(b, talking)| {
                                format!(
                                    "bits {b}–{} {}",
                                    b + 1,
                                    if *talking { "speaking" } else { "silent now" }
                                )
                            })
                            .collect();
                        format!(" · G.728 voice in {} (recorded decoded)", lanes.join(", "))
                    }
                    Coding::SubRate(m) => format!(
                        " · not G.711: only bits {} change (sub-rate channels or compressed voice)",
                        Coding::bits_text(m)
                    ),
                    Coding::Steady => " · steady (idle pattern or tone)".into(),
                    _ => String::new(),
                };
                println!("  {name}: {db:6.1} dBFS{note}");
            }
        }
    }
    if let Some(t) = &f.text {
        print_bit_text("data text", t);
    }
    if let Some(g) = &f.gse {
        println!(
            "ip: {} packets, {} flows, {:.1} kbit/s",
            g.packets,
            g.flows,
            g.ip_bps / 1e3
        );
        if let Some(l) = &g.link {
            println!("  via {l}");
        }
        for fl in g.top.iter().take(8) {
            println!(
                "  {} -> {}: {} packets, {} bytes{}",
                fl.src,
                fl.dst,
                fl.packets,
                fl.bytes,
                if fl.encrypted() {
                    " (encrypted: UDP checksums fail)"
                } else {
                    ""
                }
            );
        }
        if g.encrypted_flows > 0 {
            println!(
                "  {} flows encrypted in transit (UDP checksums fail, IP headers and FCS hold)",
                g.encrypted_flows
            );
        }
        for (group, port, sdp) in &g.stations {
            println!(
                "  station \"{}\" at {group}:{port} ({})",
                sdp.name.as_deref().unwrap_or("no name"),
                sdp.codec().label()
            );
        }
        for a in &g.audio {
            println!("  audio: {} ({:?})", a.name(), a.codec);
        }
        for st in &g.radar_stations {
            crate::pcap::print_station(st, false);
        }
        if let Some(r) = &g.radio {
            crate::pcap::print_radio(r);
        }
        print_byte_text("ip text", &g.text);
    }
    if let Some(t) = &f.ts {
        println!("ts: {} packets, {} CC errors", t.packets, t.cc_errors);
        for p in &t.report.programmes {
            println!("  programme {:?}", p.name);
        }
        print_byte_text("ts text", &t.text);
    }
}

fn print_bit_text(what: &str, t: &decsat_engine::TextView) {
    match &t.best {
        Some(how) => {
            println!("{what}: read as {how}");
            for s in t.strings.iter().rev().take(12) {
                println!("  {s}");
            }
        }
        None => println!(
            "{what}: nothing stands out ({} bytes, {} readings)",
            t.bytes, t.readings
        ),
    }
    for (how, s) in t.candidates.iter().take(5) {
        println!("  candidate {s:?}  [{how}]");
    }
}

fn print_byte_text(what: &str, t: &decsat_engine::ByteTextView) {
    if t.repeated.is_empty() && t.recent.is_empty() {
        return;
    }
    println!("{what}:");
    for (s, n) in t.repeated.iter().take(10) {
        println!("  ×{n:<4} {s}");
    }
    for s in t.recent.iter().take(10) {
        println!("  … {s}");
    }
}

/// The payload stage's report.
pub(crate) fn print_payload(p: &decsat_modem::payload::PayloadStats) {
    println!(
        "payload: {} ({} frames looked at; HDLC {} good / {} bad; TS {})",
        p.found.as_deref().unwrap_or("not recognised"),
        p.probed,
        p.hdlc_good,
        p.hdlc_bad,
        p.ts_packets
    );
    if let Some(e) = &p.paradise {
        println!(
            "  paradise framing: {}, {} groups, {} FAW errors, {} losses, ESC busy {:.1} %",
            if e.locked { "aligned" } else { "searching" },
            e.groups,
            e.faw_errors,
            e.losses,
            100.0 * e.esc_busy as f64 / e.groups.max(1) as f64
        );
    }
    if let Some(e) = &p.ibs {
        let hex = |v: &[u8; 4]| v.map(|b| format!("{b:02x}")).join(" ");
        println!(
            "  ibs framing: {}, {} frames, {} misaligned, {} losses, overhead cycle {}, service bits {}",
            if e.locked { "aligned" } else { "searching" },
            e.frames,
            e.misaligned,
            e.losses,
            hex(&e.cycle),
            hex(&e.varying)
        );
    }
    if let Some(e) = &p.edmac {
        println!(
            "  edmac framing: {}{}, {} frames, {} with their fixed bits wrong, {} losses",
            if e.locked { "aligned" } else { "searching" },
            if e.inverted { " (inverted)" } else { "" },
            e.frames,
            e.misses,
            e.losses
        );
    }
    if let Some(k) = &p.packets {
        println!(
            "  packet blocks: {}{}, {} blocks ({} bad words, {} losses) · {} whole Ethernet frames, \
             {} compressed rebuilt, {} compressed not rebuilt, {} before their context, \
             {} UDP checksums failed, {} bad ({} pointer, {} start, {} too long), \
             {} one-byte packets, {} bytes of errors between packets · {} contexts",
            if k.locked { "aligned" } else { "searching" },
            if k.inverted { " (inverted)" } else { "" },
            k.blocks,
            k.bad_words,
            k.losses,
            k.frames,
            k.rebuilt,
            k.not_rebuilt,
            k.no_context,
            k.checksum_failed,
            k.bad,
            k.bad_pointer,
            k.bad_start,
            k.too_long,
            k.short,
            k.idle_errors,
            k.contexts
        );
    }
    if let Some(b) = &p.bsc {
        println!(
            "  bsc: {} blocks with a good BCC ({}), {} bad, line {}",
            b.blocks,
            b.bcc.map_or("-", |c| c.describe()),
            b.bad,
            if b.inverted { "inverted" } else { "upright" }
        );
    }
    if let Some(s) = &p.slots {
        let ch: Vec<String> = (1..32u8)
            .filter(|c| s.channels >> c & 1 == 1)
            .map(|c| format!("{:02x}h", decsat_modem::slotpkt::channel_byte(c)))
            .collect();
        println!(
            "  timeslot packets: {} packets ({} carrying timeslots), {} lost, channels {}",
            s.packets,
            s.slot_packets,
            s.lost,
            if ch.is_empty() {
                "none yet".into()
            } else {
                ch.join(", ")
            }
        );
    }
}

/// A radar track feed: its counts, then the tracks, nearest first.
pub(crate) fn print_radar(v: &decsat_modem::radar::RadarView) {
    use decsat_modem::radar::tod_string;
    println!(
        "radar: {} messages · {} track reports · {} scans{} · {} sector jumps · {} other texts{}",
        v.messages,
        v.reports,
        v.scans,
        v.scan_secs.map_or(String::new(), |s| format!(
            " of {s:.3} s ({:.1} rpm)",
            60.0 / s
        )),
        v.sector_jumps,
        v.unknown,
        v.tod.map_or(String::new(), |t| format!(
            " · last at {} UTC",
            tod_string(t)
        ))
    );
    if let Some(s) = v.source_label() {
        println!("  radar: {s} (ASTERIX)");
    }
    let mut tracks: Vec<_> = v.tracks.iter().collect();
    tracks.sort_by(|a, b| a.last.range().total_cmp(&b.last.range()));
    for t in tracks {
        let r = &t.last;
        // Mode S, when the feed gives it: the callsign and the address.
        let mode_s = match (&r.callsign, r.address) {
            (Some(c), Some(a)) => format!(" · {c} ({a:06X})"),
            (None, Some(a)) => format!(" · Mode S {a:06X}"),
            (Some(c), None) => format!(" · {c}"),
            (None, None) => String::new(),
        };
        // What the aircraft says of itself (Mode S registers): where it is
        // going and its air data.
        let ms = &t.mode_s;
        let own: String = [
            ms.intent
                .and_then(|i| i.mcp_ft.filter(|&f| f > 0))
                .map(|f| format!("sel {}", decsat_modem::mode_s::altitude_text(f))),
            ms.intent
                .and_then(|i| i.qnh_hpa)
                .map(|q| format!("{q:.0} hPa")),
            ms.heading_speed
                .and_then(|h| h.ias_kt)
                .map(|v| format!("IAS {v}")),
            ms.heading_speed
                .and_then(|h| h.mach)
                .map(|m| format!("M{m:.3}")),
            ms.vertical_rate()
                .filter(|v| v.abs() >= 150)
                .map(|v| format!("{v:+} ft/min")),
        ]
        .into_iter()
        .flatten()
        .map(|s| format!(" · {s}"))
        .collect();
        // Where: from the radar; ADS-B's own place, and what it is.
        let (at, adsb) = match r.adsb {
            Some(a) => {
                use decsat_modem::asterix::{emergency_text, emitter_text};
                let what: String = [
                    a.emitter.map(|e| emitter_text(e).to_string()),
                    a.field_monitor.then(|| "field monitor".into()),
                    a.on_ground.then(|| "on the ground".into()),
                    a.emergency
                        .filter(|&e| e != 0)
                        .map(|e| format!("EMERGENCY: {}", emergency_text(e))),
                ]
                .into_iter()
                .flatten()
                .map(|s| format!(" · {s}"))
                .collect();
                (format!("{:8.4}, {:8.4}", a.lat, a.lon), what)
            }
            None => (
                format!("{:5.1} NM at {:5.1}°", r.range(), r.bearing()),
                String::new(),
            ),
        };
        println!(
            "  track {:03X}{mode_s} · squawk {} · {} · {} kt · {at} · heading {:3.0}° · {} reports{}{own}{adsb}",
            r.number,
            r.squawk.map_or("----".into(), |s| format!("{s:04}")),
            r.flight_level
                .map_or("no Mode C".into(), |f| format!("FL{f:03}")),
            r.ground_speed.map_or("-".into(), |g| g.to_string()),
            r.track(),
            t.reports,
            if r.updated() { "" } else { " (in decline)" }
        );
    }
}

/// A k × 128.5 kbit/s link's multiplex.
pub(crate) fn print_fast_tdm(s: &decsat_modem::tdmfast::FastStats) {
    // Where octets are: in the frame, or (when a timeslot spans frames) in
    // the superframe, frame after frame.
    let (sf, at) = match s.superframe {
        0 | 1 => (String::new(), "frame bits"),
        n => (
            format!(
                " · {n}-frame superframes{}",
                if s.phased { "" } else { " (placing)" }
            ),
            "superframe bits",
        ),
    };
    println!(
        "tdm 257 at {} × 128.5 kbit/s: {} · {} frames ({:.2} ms){sf} · {} alignment-bit errors · \
         {} losses · bits changed from 20 ms before {:.2} %",
        s.k,
        if s.aligned { "aligned" } else { "searching" },
        s.frames,
        1e3 * s.frame_seconds(),
        s.faw_errors,
        s.losses,
        100.0 * s.change
    );
    if !s.hdlc_runs.is_empty() {
        println!(
            "  HDLC timeslot: {:.0} kbit/s in octets at {at} {:?}, inverted line \
             · {} frames with a good FCS ({} fragments between flags or bad FCS)",
            s.hdlc_bps() / 1e3,
            s.hdlc_runs,
            s.hdlc_good,
            s.hdlc_bad
        );
    }
    for (i, m) in s.more_hdlc.iter().enumerate() {
        println!(
            "  HDLC timeslot {}: 64 kbit/s in octets at {at} {:?}, inverted line · {} frames \
             with a good FCS",
            i + 2,
            m.runs,
            m.good
        );
    }
    for c in &s.calls {
        println!(
            "  G.728 call on bits {}–{} of the 64 kbit/s timeslot at {at} {:?} · \
             {:.1} s of speech, {:.1} s of silence fill",
            c.bit,
            c.bit + 1,
            c.runs,
            c.talk_blocks as f64 * 0.04,
            c.fill_blocks as f64 * 0.04
        );
    }
}

/// The 257-bit TDM multiplex's report.
pub(crate) fn print_tdm(t: &decsat_modem::tdm257::TdmStats) {
    println!(
        "tdm 257: {} · {} frames (2 ms) · {} alignment-bit errors · {} losses · side data {}",
        if t.locked { "aligned" } else { "searching" },
        t.frames,
        t.faw_errors,
        t.losses,
        t.side_data
            .iter()
            .map(|b| char::from(b'0' + b))
            .collect::<String>()
    );
    for c in &t.calls {
        println!(
            "  call at {:.1} s for {:.1} s{} on channels {}",
            c.start as f64 * 0.002,
            c.seconds(),
            if c.open { " (still going)" } else { "" },
            c.channel_list()
        );
    }
    if let Some(v) = &t.voice {
        let ch: Vec<String> = (0..16)
            .filter(|c| v.channels >> c & 1 == 1)
            .map(|c| c.to_string())
            .collect();
        println!(
            "  G.728 call voice on channels {} · {:.1} s of speech decoded{}",
            ch.join(", "),
            v.talk_blocks as f64 * 0.04,
            if v.talking { " · speaking" } else { "" }
        );
    }
    for (c, ch) in t.channels.iter().enumerate() {
        println!(
            "  channel {c:2} (bit {c:2} of each word): {:12} changes 20 ms {:5.1} % · 4 ms {:5.1} % · ones {:4.1} %",
            ch.state.label(),
            100.0 * ch.change_20ms,
            100.0 * ch.change_4ms,
            100.0 * ch.ones
        );
    }
}

/// A TDMA VFO's carriers: their bursts, layout, slot grid and terminals.
pub(crate) fn print_tdma(v: &decsat_engine::TdmaView) {
    for (i, c) in v.channels.iter().enumerate() {
        println!(
            "tdma #{i}: {:+.1} kHz, {:.1} kS/s α {:.2}, {} · {} bursts ({:.0}/s, on {:.0} %) · {} · MER {:.1} dB, {:.1} dB over the floor",
            c.center_hz / 1e3,
            c.symbol_rate / 1e3,
            c.alpha,
            c.modulation.map_or("modulation ?", |m| m.name()),
            c.bursts,
            c.per_s,
            100.0 * c.duty,
            c.len_symbols
                .map_or("length ?".to_string(), |l| format!("{l} symbols")),
            c.mer_db,
            c.snr_db
        );
        match &c.layout {
            Some(l) => println!("  layout: {} (from {} bursts)", l.describe(), l.bursts),
            None => println!("  layout: not read yet"),
        }
        println!("  code: {}", tdma_code_text(&c.code, c.layout.is_some()));
        if !c.header.is_empty() {
            println!("  header fields (symbols: latest ← before · values · changes):");
            for f in &c.header {
                let at = if f.from == f.to {
                    format!("{}", f.from)
                } else {
                    format!("{}–{}", f.from, f.to)
                };
                println!(
                    "    {at:>9}: {} · {} values ({:.0} % one) · changes in {:.0} % of bursts",
                    f.history.join(" ← "),
                    f.distinct,
                    100.0 * f.commonest_share,
                    100.0 * f.change_share
                );
            }
        }
        if let Some(g) = &c.grid {
            let ms = |sym: f64| sym / c.symbol_rate * 1e3;
            println!(
                "  slots: {:.2} symbols ({:.4} ms), guard {:.1} symbols, ranged within ±{:.2} symbols, {:.0} % used",
                g.period,
                ms(g.period),
                g.guard,
                g.spread,
                100.0 * g.occupancy,
            );
            if let Some(f) = &g.frame {
                println!(
                    "  frame: {:.3} ms = {:.2} slots + a {:.0}-symbol ({:.3} ms) silence",
                    ms(f.period),
                    f.slots(g.period),
                    f.silence,
                    ms(f.silence)
                );
            }
        }
        if !c.terminals.is_empty() {
            println!("  terminals ({}):", c.terminals.len());
            for t in c.terminals.iter().take(12) {
                println!(
                    "    {:>8} {:6} bursts ({:3.0} % idle)  {:+6.1} dB  {:+8.0} Hz  {:+.2} symbols into the slot",
                    t.id.as_deref().unwrap_or("-"),
                    t.bursts,
                    100.0 * t.idle as f64 / t.bursts.max(1) as f64,
                    t.power_db,
                    t.freq_hz,
                    t.offset
                );
            }
        }
    }
}

/// Programme audio: the IBS scrambling it came out of and the apt-X in it.
fn print_programme(p: &decsat_engine::ProgrammeView) {
    if let Some(i) = &p.ibs {
        println!(
            "ibs: synchronous scrambler (x^15+x^14+1, 128-frame multiframe) · {} frames · \
             found {}× · lost {}× · TS16 {}",
            i.frames,
            i.found,
            i.losses,
            i.ts16.map_or("varies".into(), |v| format!("{v:02X}h"))
        );
    }
    let a = &p.aptx;
    if a.locked {
        println!(
            "programme: {} · {} · {} Hz · codewords from bit {} · LL lag-1 {:.2}{} · \
             {} codewords · {:.1} % clipped",
            p.codec,
            if a.stereo { "stereo" } else { "mono" },
            p.rate,
            a.offset,
            a.lag1,
            if a.stereo {
                format!(" · L/R {:.2}", a.lr)
            } else {
                String::new()
            },
            a.codewords,
            100.0 * a.clipped
        );
    } else {
        println!(
            "programme: {} not found yet ({} searches)",
            p.codec, a.searches
        );
    }
    if !p.levels_db.is_empty() {
        let l: Vec<String> = p.levels_db.iter().map(|d| format!("{d:.1}")).collect();
        println!("  levels {} dBFS", l.join(" / "));
    }
    if let Some((path, n)) = &p.record_file {
        println!("  recorded to {} ({n} bytes)", path.display());
    }
    if let Some(at) = a.autosync {
        println!(
            "  AutoSync word {at} codewords into each 128, in the {} channel (played as the left)",
            if a.swapped { "second" } else { "first" }
        );
    }
    if let Some(e) = &p.error {
        println!("  error: {e}");
    }
}
