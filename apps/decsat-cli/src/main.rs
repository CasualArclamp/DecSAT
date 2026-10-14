//! DecSAT command line.
//!
//! M0 provides `modcods` (print the coding tables) and `analyse` (read an IQ
//! file and report level/spectrum occupancy). `decode` and `modulate` arrive
//! with the later milestones.

mod cidgen;
mod decode;
mod mcast;
mod payload;
mod pcap;
mod record;
mod scene;
mod tx;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use decsat_core::{FecFrame, RxConfig, SampleFormat, s2_modcod_table, s2x_modcod_table};
use decsat_engine::{Receiver, estimate_band};
use decsat_io::{IqFileReader, IqSource, format_from_path};

#[derive(Parser)]
#[command(
    name = "decsat",
    version,
    about = "DVB-S2/S2X receiver with GSE/IP de-encapsulation"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the modulation and coding table.
    Modcods,
    /// Read an IQ file and report basic signal statistics.
    Analyse {
        /// IQ capture to read.
        file: PathBuf,
        /// Sample format; inferred from the extension (or a WAV header)
        /// when omitted.
        #[arg(long, value_parser = parse_format)]
        format: Option<SampleFormat>,
        /// Sample rate in samples/s (e.g. 2e6 or 2000000); a WAV header's
        /// wins.
        #[arg(long, default_value_t = 2_000_000.0)]
        rate: f64,
        /// Stop after this many blocks (0 = whole file).
        #[arg(long, default_value_t = 8)]
        blocks: usize,
    },
    /// Write a synthetic RRC-shaped QPSK capture, for testing the chain.
    ///
    /// This is a signal generator, not yet a DVB-S2 modulator: it has no
    /// PLFRAMEs or FEC. It exists so the display and acquisition stages have
    /// something with the right spectral shape to chew on. The real modulator
    /// arrives in M6.
    Synth {
        /// Output file (`.cs8`, `.cs16` or `.cf32` picks the format).
        out: PathBuf,
        /// Sample rate in samples/s.
        #[arg(long, default_value_t = 2_000_000.0)]
        rate: f64,
        /// Symbol rate in symbols/s.
        #[arg(long, default_value_t = 500_000.0)]
        symbol_rate: f64,
        /// Roll-off factor.
        #[arg(long, default_value_t = 0.20)]
        alpha: f64,
        /// Carrier frequency offset in Hz.
        #[arg(long, default_value_t = 0.0)]
        offset: f64,
        /// Es/N0 in dB (very large = no noise).
        #[arg(long, default_value_t = 12.0)]
        esn0: f64,
        /// Duration in seconds.
        #[arg(long, default_value_t = 1.0)]
        seconds: f64,
        /// PRNG seed, so a capture is reproducible.
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
    /// Write a multi-carrier 8 MS/s test capture: two DVB-S2 carriers (one
    /// ACM), plain QPSK and a CW tone.
    Scene {
        /// Output file; `.cs8` is what a HackRF records.
        #[arg(default_value = "decsat-scene_8Msps.cs8")]
        out: PathBuf,
        #[arg(long, default_value_t = 4.0)]
        seconds: f64,
    },
    /// Write a test capture of a DVB-S2 carrier with a DVB-CID (carrier ID,
    /// ETSI TS 103 129) under it: identifier, position, telephone and text.
    Cid(cidgen::CidArgs),
    /// Modulate an MPEG-TS (a file, a UDP stream, or the test stream) onto a
    /// DVB-S2/S2X carrier, to an IQ file.
    Tx(tx::TxArgs),
    /// Write every test signal (the scene and the CID capture) into a
    /// folder.
    TestSignals {
        #[arg(default_value = "tests")]
        dir: PathBuf,
    },
    /// Run a modem data recording (.bin) through the payload stage again:
    /// format, Paradise framing, the TDM multiplex inside.
    Payload(payload::PayloadArgs),
    /// Read a PCAP of IP again (a VFO's, written on Record): a radar
    /// station's monitoring traffic in it, unit by unit.
    Pcap(pcap::PcapArgs),
    /// List the multicast radio in a recorded transport stream (IP from MPE):
    /// what each stream is, now-playing messages; decode or record them.
    Mcast(mcast::McastArgs),
    /// Capture raw IQ from the HackRF to .cs8 files — one frequency or a
    /// sweep — named so `scan` and `decode` read them straight back.
    Record(record::RecordArgs),
    /// Survey a capture: find every carrier and identify each one blind.
    Scan {
        file: PathBuf,
        #[arg(long, value_parser = parse_format)]
        format: Option<SampleFormat>,
        /// Sample rate; taken from the file name (…_8Msps…) when omitted.
        #[arg(long)]
        rate: Option<f64>,
        /// Give up waiting for results after this many seconds.
        #[arg(long, default_value_t = 30.0)]
        timeout: f64,
        /// Then decode: run each identified carrier's decoder (TPC 2964,
        /// FastLink, DVB-S2, DVB-S, Viterbi) over the whole capture and
        /// report what each one found.
        #[arg(long)]
        decode: bool,
    },
    /// Run one decoder over a capture — or live from the HackRF — and report
    /// what it finds.
    Decode {
        /// The capture (not needed with --hackrf).
        file: Option<PathBuf>,
        /// Live from the HackRF instead: its centre frequency, MHz (as it
        /// tunes: L-band behind an LNB). Receive only, antenna power off.
        #[arg(long)]
        hackrf: Option<f64>,
        /// HackRF IF (LNA) gain, dB: 0–40 in 8 dB steps.
        #[arg(long, default_value_t = 24)]
        lna: u16,
        /// HackRF baseband (VGA) gain, dB: 0–62 in 2 dB steps.
        #[arg(long, default_value_t = 20)]
        vga: u16,
        /// HackRF RF amplifier on (+14 dB).
        #[arg(long)]
        amp: bool,
        /// Stop after this many seconds (live runs otherwise go on until
        /// stopped).
        #[arg(long)]
        seconds: Option<f64>,
        /// fastlink: record the data while a TDM channel is active, with
        /// the 10 s before (needs --out).
        #[arg(long)]
        record_on_activity: bool,
        #[arg(long, value_parser = parse_format)]
        format: Option<SampleFormat>,
        /// Sample rate; taken from the file name (…_320000Sps…) when omitted.
        #[arg(long)]
        rate: Option<f64>,
        /// Decoder: id, ip, ts, dvbs, tpc, cdm600, cdm570, trunk, auto, fastlink, viterbi, cid, psk, tdma.
        #[arg(long, default_value = "id")]
        decoder: String,
        /// VFO centre relative to the capture's centre, Hz.
        #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
        offset: f64,
        /// VFO width, Hz (default 90 % of the sample rate).
        #[arg(long)]
        bandwidth: Option<f64>,
        /// Symbol rate, if known (found blind otherwise).
        #[arg(long)]
        symbol_rate: Option<f64>,
        /// Modulation for tpc/psk (bpsk, qpsk, 8psk, 16qam…).
        #[arg(long)]
        modulation: Option<String>,
        /// Write the decoder's outputs (PCAP, TS, data, E1 audio) here.
        #[arg(long)]
        out: Option<PathBuf>,
        /// As fast as possible rather than in real time (nothing is
        /// dropped: the file waits for slow decoders).
        #[arg(long)]
        fast: bool,
        /// Record this E1 timeslot (or D&I++ channel, or packet channel:
        /// its byte over 8, 18h as 3) to a .wav in --out.
        #[arg(long)]
        e1_record: Option<u8>,
        /// fastlink: record the TDM multiplex's call voice (G.728) to a
        /// .wav in --out.
        #[arg(long)]
        tdm_record: bool,
        /// psk: number the symbols in the .bin as the standard labels them,
        /// by position (`natural`), or Gray-coded by position (`gray`).
        #[arg(long, default_value = "standard")]
        labels: String,
        /// cid: low-SNR mode (searches 96–384 bits deep, looser threshold).
        #[arg(long)]
        low_snr: bool,
        /// cid: the receiver's clock correction, ppm (as the CID measures it).
        #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
        clock_ppm: f64,
        /// psk: write the symbols' bits (packed, MSB first) rather than a
        /// byte a symbol.
        #[arg(long)]
        bits: bool,
        /// ip/ts: append every good BBFRAME to this file (each as: length,
        /// u16 little-endian; PLS MODCOD, one byte; the BBFRAME).
        #[arg(long)]
        bbframes: Option<PathBuf>,
        /// Open Wireshark and feed it the IP packets as they are decoded
        /// (it captures from its standard input); closing it ends the run.
        #[arg(long)]
        wireshark: bool,
    },
}

fn parse_format(s: &str) -> std::result::Result<SampleFormat, String> {
    match s.to_ascii_lowercase().as_str() {
        "cs8" | "s8" | "i8" => Ok(SampleFormat::Cs8),
        "cu8" | "u8" => Ok(SampleFormat::Cu8),
        "cs16" | "s16" => Ok(SampleFormat::Cs16),
        "cs24" | "s24" => Ok(SampleFormat::Cs24),
        "cf32" | "fc32" | "f32" => Ok(SampleFormat::Cf32),
        other => Err(format!(
            "unknown format `{other}` (want cs8, cu8, cs16, cs24 or cf32)"
        )),
    }
}

/// A capture's sample format and rate: from its WAV header if it is a WAV
/// file (which then wins over the flags), else from the flags, the
/// extension and the file name.
pub(crate) fn capture_format_rate(
    file: &std::path::Path,
    format: Option<SampleFormat>,
    rate: Option<f64>,
) -> Result<(SampleFormat, f64)> {
    let info =
        decsat_io::probe_capture(file).with_context(|| format!("reading {}", file.display()))?;
    if info.wav {
        // `probe_capture` fills both from the header.
        return Ok((info.format.unwrap(), info.sample_rate.unwrap()));
    }
    let fmt = format
        .or(info.format)
        .context("cannot tell the sample format from the extension — pass --format")?;
    let rate = rate
        .or_else(|| rate_from_name(file))
        .context("cannot tell the sample rate from the file name — pass --rate")?;
    Ok((fmt, rate))
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Modcods => modcods(),
        Command::Analyse {
            file,
            format,
            rate,
            blocks,
        } => analyse(file, format, rate, blocks),
        Command::Synth {
            out,
            rate,
            symbol_rate,
            alpha,
            offset,
            esn0,
            seconds,
            seed,
        } => synth(SynthArgs {
            out,
            rate,
            symbol_rate,
            alpha,
            offset,
            esn0,
            seconds,
            seed,
        }),
        Command::Scene { out, seconds } => {
            scene::write(&out, seconds)?;
            println!(
                "wrote {} — {seconds:.1} s at {} MS/s, cs8",
                out.display(),
                scene::RATE / 1e6
            );
            Ok(())
        }
        Command::Tx(a) => tx::run(&a),
        Command::Cid(a) => {
            let cycle = cidgen::write(&a)?;
            println!("{}", cidgen::summary(&a, cycle));
            Ok(())
        }
        Command::TestSignals { dir } => {
            std::fs::create_dir_all(&dir)?;
            let out = dir.join("decsat-scene_8Msps.cs8");
            scene::write(&out, 4.0)?;
            println!(
                "wrote {} — 4.0 s at {} MS/s, cs8",
                out.display(),
                scene::RATE / 1e6
            );
            let a = cidgen::CidArgs::defaults(dir.join(cidgen::DEFAULT_NAME));
            let cycle = cidgen::write(&a)?;
            println!("{}", cidgen::summary(&a, cycle));
            Ok(())
        }
        Command::Mcast(a) => mcast::run(&a),
        Command::Payload(a) => payload::run(&a),
        Command::Pcap(a) => pcap::run(&a),
        Command::Record(a) => record::run(&a),
        Command::Scan {
            file,
            format,
            rate,
            timeout,
            decode,
        } => scan(file, format, rate, timeout, decode),
        Command::Decode {
            file,
            hackrf,
            lna,
            vga,
            amp,
            seconds,
            record_on_activity,
            format,
            rate,
            decoder,
            offset,
            bandwidth,
            symbol_rate,
            modulation,
            out,
            fast,
            e1_record,
            tdm_record,
            labels,
            low_snr,
            clock_ppm,
            bits,
            bbframes,
            wireshark,
        } => decode::decode(decode::DecodeArgs {
            file,
            hackrf,
            lna,
            vga,
            amp,
            seconds,
            record_on_activity,
            format,
            rate,
            decoder,
            offset,
            bandwidth,
            symbol_rate,
            modulation,
            out,
            fast,
            e1_record,
            tdm_record,
            labels,
            low_snr,
            clock_ppm,
            bits,
            bbframes,
            wireshark,
        }),
    }
}

/// Sample rate from a name like `…_8Msps…`, `…_500ksps…` or `…_2000000sps…`.
pub(crate) fn rate_from_name(path: &std::path::Path) -> Option<f64> {
    let stem = path.file_stem()?.to_str()?.to_ascii_lowercase();
    stem.split(['_', '-', ' ']).find_map(|t| {
        let n = t.strip_suffix("sps")?;
        let (num, mult) = match n.chars().last()? {
            'k' => (&n[..n.len() - 1], 1e3),
            'm' => (&n[..n.len() - 1], 1e6),
            _ => (n, 1.0),
        };
        num.parse::<f64>().ok().map(|v| v * mult)
    })
}

fn scan(
    file: PathBuf,
    format: Option<SampleFormat>,
    rate: Option<f64>,
    timeout: f64,
    decode: bool,
) -> Result<()> {
    use decsat_engine::{DecoderKind, Engine, EngineOptions, VfoSettings};
    use std::time::{Duration, Instant};

    let (fmt, rate) = capture_format_rate(&file, format, rate)?;
    let reader = IqFileReader::open(&file, fmt, rate, 1 << 16)
        .with_context(|| format!("opening {}", file.display()))?;
    println!("scanning {} at {:.3} MS/s", reader.describe(), rate / 1e6);

    let mut eng = Engine::start(
        Box::new(reader),
        EngineOptions {
            realtime: false,
            loop_file: true,
            ..Default::default()
        },
    );

    // Let the averaged spectrum settle, then take the carrier list.
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_millis(1500));
    let carriers = eng.front().carriers;
    if carriers.is_empty() {
        println!("no carriers found");
        return Ok(());
    }
    println!("\n{} carriers:", carriers.len());

    let mut ids = Vec::new();
    for (i, c) in carriers.iter().enumerate() {
        println!(
            "  #{i}  {:+9.1} kHz  ~{:>8.1} kS/s  {:6.1} kHz wide  {:5.1} dB{}",
            c.center_hz / 1e3,
            c.symbol_rate_hz / 1e3,
            c.bandwidth_hz / 1e3,
            c.snr_db,
            if c.narrow { "  (narrow)" } else { "" }
        );
        let bw = if c.narrow {
            c.fit_among(20e3, &carriers)
        } else {
            c.vfo_bandwidth_among(&carriers)
        };
        ids.push(eng.add_vfo(VfoSettings::new(
            format!("#{i}"),
            c.center_hz,
            bw,
            DecoderKind::Identify,
        )));
    }

    println!("\nidentifying…");
    let mut found = vec![None; ids.len()];
    while found.iter().any(Option::is_none) && t0.elapsed().as_secs_f64() < timeout {
        for (i, &id) in ids.iter().enumerate() {
            if found[i].is_none()
                && let Some(ident) = eng.vfo_status(id).and_then(|s| s.identification)
            {
                println!("  #{i}  {}", ident.summary());
                found[i] = Some(ident);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    for (i, f) in found.iter().enumerate() {
        if f.is_none() {
            println!("  #{i}  (no result within {timeout:.0} s)");
        }
    }
    drop(eng);
    if !decode {
        return Ok(());
    }

    // The decode pass: the capture once more from the start, not looped
    // (a decoder would take the loop's seam for a fault) and lossless (the
    // file waits for slow decoders), a VFO on each carrier Identify named.
    let reader = IqFileReader::open(&file, fmt, rate, 1 << 16)?;
    let mut eng = Engine::start(
        Box::new(reader),
        EngineOptions {
            realtime: false,
            loop_file: false,
            ..Default::default()
        },
    );
    let mut runs = Vec::new();
    for (i, (c, ident)) in carriers.iter().zip(&found).enumerate() {
        let Some(ident) = ident else { continue };
        let bw = if c.narrow {
            c.fit_among(20e3, &carriers)
        } else {
            c.vfo_bandwidth_among(&carriers)
        };
        for kind in decoders_for(ident) {
            // Centred on the carrier as Identify measured it.
            let mut s = VfoSettings::new(
                format!("#{i}"),
                c.center_hz + ident.center_offset_hz,
                bw,
                kind,
            );
            s.symbol_rate = ident.symbol_rate;
            s.voice_auto = false;
            runs.push((i, kind, eng.add_vfo(s)));
        }
    }
    if runs.is_empty() {
        println!("\nnothing to decode: no carrier has a decoder");
        return Ok(());
    }
    println!("\ndecoding {} carriers over the whole capture…", runs.len());
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let front = eng.front();
        if let decsat_engine::SourceState::Failed(e) = &front.state {
            bail!("source failed: {e}");
        }
        if matches!(front.state, decsat_engine::SourceState::Ended) {
            break;
        }
    }
    // Every decoder through all it was given, before the reports.
    if !decode::wait_settled(&eng, decode::SETTLE_CAP) {
        println!(
            "(stopped waiting after {} s: the decoders are still {} blocks or frames behind)",
            decode::SETTLE_CAP.as_secs(),
            eng.pending_work()
        );
    }
    for (i, kind, id) in runs {
        let c = &carriers[i];
        println!(
            "\n#### #{i}  {:+.1} kHz  {} ({})",
            c.center_hz / 1e3,
            kind.label(),
            found[i]
                .as_ref()
                .and_then(|f| f.symbol_rate)
                .map_or("?".into(), |r| format!("{:.1} kS/s", r / 1e3))
        );
        match eng.vfo_status(id) {
            Some(st) => decode::report(&st),
            None => println!("no status from the VFO"),
        }
    }
    // Whatever is still queued is not wanted now: leave at once.
    eng.discard_vfos();
    Ok(())
}

/// The decoders to try on what Identify found: DVB-S2 and the modems it
/// names by their framing; for a plain BPSK/QPSK carrier with no framing
/// found, the K = 7 Viterbi decoder (it finds a convolutional code's rate
/// blind, or says there is none); nothing for the rest.
fn decoders_for(ident: &decsat_engine::Identification) -> Vec<decsat_engine::DecoderKind> {
    use decsat_engine::identify::ConstellationGuess;
    use decsat_engine::{DecoderKind, Verdict};
    match &ident.verdict {
        Verdict::DvbS2(d) if d.unconfirmed_spacing.is_none() => vec![DecoderKind::Dvbs2Ip],
        // A TDMA carrier's bursts: no continuous decoder reads them.
        _ if ident.bursts.is_some() => Vec::new(),
        Verdict::NotDvbS2 { hint } => {
            // Rust note: the hint is a sentence for people; these are the
            // names Identify puts in it when a decoder can read the carrier.
            if hint.contains("TPC 2964") {
                vec![DecoderKind::Tpc2964]
            } else if hint.contains("FastLink") {
                vec![DecoderKind::FastLink]
            } else if hint.contains("DVB-S (") {
                // "Possibly": DVB-S's inner code is the same K = 7 code as
                // many SCPC modems', so try both.
                vec![DecoderKind::DvbsTs, DecoderKind::Viterbi]
            } else if matches!(
                ident.constellation,
                Some(ConstellationGuess::Bpsk | ConstellationGuess::Qpsk)
            ) {
                vec![DecoderKind::Viterbi]
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

struct SynthArgs {
    out: PathBuf,
    rate: f64,
    symbol_rate: f64,
    alpha: f64,
    offset: f64,
    esn0: f64,
    seconds: f64,
    seed: u64,
}

/// xorshift64*: a few lines, reproducible, and good enough for test signals.
/// (Not for anything that needs cryptographic or statistical rigour.)
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    fn next_f64(&mut self) -> f64 {
        // Top 53 bits give a double with full mantissa precision.
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// One standard normal pair, Box–Muller.
    fn next_gaussian_pair(&mut self) -> (f64, f64) {
        let u1 = self.next_f64().max(f64::MIN_POSITIVE);
        let u2 = self.next_f64();
        let r = (-2.0 * u1.ln()).sqrt();
        let th = std::f64::consts::TAU * u2;
        (r * th.cos(), r * th.sin())
    }
}

fn synth(a: SynthArgs) -> Result<()> {
    use decsat_core::Iq;
    use decsat_dsp::{Fir, rrc_taps};
    use decsat_io::IqFileWriter;

    let fmt = format_from_path(&a.out).unwrap_or(SampleFormat::Cs8);
    let sps_exact = a.rate / a.symbol_rate;
    let sps = sps_exact.round();
    if (sps_exact - sps).abs() > 1e-6 {
        bail!(
            "rate / symbol_rate must be a whole number for now (got {sps_exact:.4}); \
             fractional resampling arrives in M1"
        );
    }
    if sps < 2.0 {
        bail!("need at least 2 samples per symbol (got {sps})");
    }
    let sps_i = sps as usize;

    let mut rng = Rng(a.seed | 1);
    let mut shaper = Fir::new(rrc_taps(sps, a.alpha, 16));
    let mut writer = IqFileWriter::create(&a.out, fmt)?;

    let total_symbols = (a.seconds * a.symbol_rate).round() as usize;
    // Es/N0 -> per-sample noise sigma. Signal power is normalised to 1 below.
    let n0 = 10f64.powf(-a.esn0 / 10.0);
    let sigma = (n0 / 2.0).sqrt();
    // Keep well clear of full scale so cs8 does not clip on peaks.
    let amplitude = 0.35;
    let dphi = std::f64::consts::TAU * a.offset / a.rate;

    let mut phase = 0.0f64;
    let mut block: Vec<Iq> = Vec::with_capacity(sps_i * 1024);
    let mut shaped: Vec<Iq> = Vec::with_capacity(sps_i * 1024);

    for chunk_start in (0..total_symbols).step_by(1024) {
        let n = 1024.min(total_symbols - chunk_start);
        block.clear();
        for _ in 0..n {
            // Gray-mapped QPSK on the diagonals, unit average power.
            let bits = self_next_two_bits(&mut rng);
            let s = match bits {
                0 => Iq::new(
                    std::f32::consts::FRAC_1_SQRT_2,
                    std::f32::consts::FRAC_1_SQRT_2,
                ),
                1 => Iq::new(
                    -std::f32::consts::FRAC_1_SQRT_2,
                    std::f32::consts::FRAC_1_SQRT_2,
                ),
                2 => Iq::new(
                    -std::f32::consts::FRAC_1_SQRT_2,
                    -std::f32::consts::FRAC_1_SQRT_2,
                ),
                _ => Iq::new(
                    std::f32::consts::FRAC_1_SQRT_2,
                    -std::f32::consts::FRAC_1_SQRT_2,
                ),
            };
            // Upsample: the symbol, then sps-1 zeros for the shaper to fill in.
            block.push(s);
            for _ in 1..sps_i {
                block.push(Iq::new(0.0, 0.0));
            }
        }

        shaped.clear();
        shaper.process(&block, &mut shaped);

        // Zero-stuffing divides the power by sps; sqrt(sps) puts it back.
        let gain = (amplitude * sps.sqrt()) as f32;
        for s in shaped.iter_mut() {
            let (nr, ni) = rng.next_gaussian_pair();
            let noise = Iq::new((sigma * nr) as f32, (sigma * ni) as f32) * amplitude as f32;
            let rot = Iq::new(phase.cos() as f32, phase.sin() as f32);
            *s = *s * gain * rot + noise;
            phase += dphi;
            if phase > std::f64::consts::TAU {
                phase -= std::f64::consts::TAU;
            }
        }
        writer.write(&shaped)?;
    }

    writer.finish()?;
    println!(
        "wrote {} — {:.3} s, {:.0} S/s, {:.0} sym/s ({} sps), alpha {:.2}, \
         offset {:+.0} Hz, Es/N0 {:.1} dB, {:?}",
        a.out.display(),
        a.seconds,
        a.rate,
        a.symbol_rate,
        sps_i,
        a.alpha,
        a.offset,
        a.esn0,
        fmt
    );
    Ok(())
}

/// Two fresh bits from the generator (one QPSK symbol).
fn self_next_two_bits(rng: &mut Rng) -> u8 {
    (rng.next_u64() & 0b11) as u8
}

fn modcods() -> Result<()> {
    println!(
        "{:>3}  {:<10} {:>6}  {:>9}  {:>9}",
        "idx", "mod", "rate", "K(64800)", "K(16200)"
    );
    for m in s2_modcod_table() {
        let short = decsat_core::modcod::s2_modcod(m.index, FecFrame::Short)
            .map(|s| s.k_approx())
            .unwrap_or(0);
        println!(
            "{:>3}  {:<10} {:>6}  {:>9}  {:>9}",
            m.index,
            m.modulation.name(),
            m.rate.to_string(),
            m.k_approx(),
            short
        );
    }
    println!("\n28 DVB-S2 MODCODs (EN 302 307-1 Table 12).\n");
    println!(
        "{:>3}  {:<16} {:>8}  {:<7} {:>6}",
        "PLS", "MODCOD", "LDPC", "FECFRAME", "K"
    );
    for m in s2x_modcod_table() {
        println!(
            "{:>3}  {:<16} {:>8}  {:<7} {:>7}",
            m.index,
            m.to_string(),
            m.rate.to_string(),
            if m.frame == FecFrame::Short {
                "short"
            } else {
                "normal"
            },
            m.k_approx()
        );
    }
    println!(
        "\n{} DVB-S2X MODCODs (EN 302 307-2 Table 17a), by PLS code with pilots off.",
        s2x_modcod_table().len()
    );
    Ok(())
}

fn analyse(file: PathBuf, format: Option<SampleFormat>, rate: f64, blocks: usize) -> Result<()> {
    // A WAV file's header gives both; otherwise the flags or the extension.
    let (fmt, rate) = capture_format_rate(&file, format, Some(rate))?;

    const FFT: usize = 4096;
    let reader = IqFileReader::open(&file, fmt, rate, FFT * 16)
        .with_context(|| format!("opening {}", file.display()))?;
    println!("source: {}", reader.describe());

    let mut rx = Receiver::new(
        Box::new(reader),
        RxConfig {
            sample_rate: rate,
            ..Default::default()
        },
        FFT,
    );

    let mut total = 0usize;
    let mut block = 0usize;
    let mut last: Option<decsat_engine::BandEstimate> = None;
    while let Some(snap) = rx.step()? {
        total += snap.samples;
        block += 1;

        let rms = (snap.scatter.iter().map(|s| s.norm_sqr()).sum::<f32>()
            / snap.scatter.len().max(1) as f32)
            .sqrt();

        match estimate_band(&snap.spectrum_db, rate, 0.99) {
            Some(b) => {
                println!(
                    "block {block:>4}: rms {rms:>7.4}  centre {:>+8.1} kHz  \
                     bw(99%) {:>7.1} kHz  Rs ~{:>7.1} kS/s  S/N {:>5.1} dB",
                    b.center_hz / 1e3,
                    b.bandwidth_hz / 1e3,
                    b.symbol_rate_hz / 1e3,
                    b.snr_db,
                );
                last = Some(b);
            }
            None => println!("block {block:>4}: rms {rms:>7.4}  (no spectrum)"),
        }

        if blocks != 0 && block >= blocks {
            break;
        }
    }

    println!(
        "read {total} samples ({:.3} s at {:.3} MS/s)",
        total as f64 / rate,
        rate / 1e6
    );
    if let Some(b) = last {
        // Deliberately no roll-off estimate here: a 99 % occupied bandwidth is
        // narrower than the true (1 + alpha) * Rs width, because the RRC skirts
        // carry little power, so alpha derived from it reads systematically low
        // (measured ~0.14 for a true 0.20). M1 identifies the roll-off properly
        // by sweeping matched filters and taking the best timing-error metric.
        println!(
            "suggest: --symbol-rate {:.0} --offset {:.0}; \
             blind acquisition in M1 will refine this",
            b.symbol_rate_hz, b.center_hz,
        );
    }
    Ok(())
}
