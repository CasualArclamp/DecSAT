//! `decsat mcast`: the multicast radio in a recorded transport stream.
//!
//! IP comes out of MPE on whatever PIDs carry it, IPv4 fragments are put
//! back together, and every multicast audio stream is listed with what it
//! is — announced by SAP, or described from its own packets — along with any
//! now-playing messages. `--decode` runs each stream through the in-app
//! decoder to show that it plays; `--record` writes each one as broadcast.
//!
//! Signal time comes from `--ts-rate`, else from the stream's PCRs; with
//! neither, RTP clock rates come from RTCP sender reports alone.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use decsat_audio::AudioRecorder;
use decsat_audio::decode::{Block, Decoder};
use decsat_audio::depay::{Depacketizer, Unit};
use decsat_ip::mcast::udp_payload;
use decsat_ip::{AudioStream, IpInfo, McastScanner, Reassembler};
use decsat_ts::{MpeExtractor, TS_LEN};

#[derive(Args)]
pub struct McastArgs {
    /// A recorded MPEG transport stream (.ts).
    pub file: PathBuf,
    /// The transport stream's rate, bit/s (else from its PCRs, if any): the
    /// signal's time, for streams that send no RTCP.
    #[arg(long)]
    pub ts_rate: Option<f64>,
    /// Decode every stream found and say how it went.
    #[arg(long)]
    pub decode: bool,
    /// Record every stream found, as broadcast, into this folder.
    #[arg(long)]
    pub record: Option<PathBuf>,
    /// Decode every stream found to a 16-bit stereo WAV in this folder.
    #[arg(long)]
    pub wav: Option<PathBuf>,
}

/// Every TS packet in `path`, found by its sync byte (a file cut mid-packet
/// is picked up again at the next 0x47 that repeats 188 bytes on).
fn each_packet(path: &Path, mut f: impl FnMut(&[u8; TS_LEN])) -> Result<u64> {
    let mut r = BufReader::with_capacity(
        1 << 20,
        File::open(path).with_context(|| format!("cannot open {}", path.display()))?,
    );
    let mut buf = Vec::with_capacity(1 << 20);
    let mut chunk = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        let got = r.read(&mut chunk)?;
        if got == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..got]);
        let mut at = 0;
        while at + 2 * TS_LEN <= buf.len() {
            if buf[at] == 0x47 && buf[at + TS_LEN] == 0x47 {
                // `try_into` turns the slice into a fixed-size array ref.
                f(buf[at..at + TS_LEN].try_into().unwrap());
                n += 1;
                at += TS_LEN;
            } else {
                at += 1;
            }
        }
        buf.drain(..at);
    }
    if buf.len() >= TS_LEN && buf[0] == 0x47 {
        f(buf[..TS_LEN].try_into().unwrap());
        n += 1;
    }
    Ok(n)
}

/// The rate from the first and last PCR on the first PID that carries one
/// (ISO/IEC 13818-1 §2.4.3.5: 27 MHz, a 33-bit base × 300 plus extension).
fn pcr_rate(path: &Path) -> Result<Option<f64>> {
    let mut pid_seen: Option<u16> = None;
    let mut first: Option<(u64, f64)> = None;
    let mut last: Option<(u64, f64)> = None;
    let mut index = 0u64;
    each_packet(path, |p| {
        let pid = u16::from_be_bytes([p[1] & 0x1F, p[2]]);
        if (p[3] >> 4) & 2 != 0
            && p[4] >= 7
            && p[5] & 0x10 != 0
            && pid_seen.is_none_or(|s| s == pid)
        {
            pid_seen = Some(pid);
            let base = (u64::from(p[6]) << 25)
                | (u64::from(p[7]) << 17)
                | (u64::from(p[8]) << 9)
                | (u64::from(p[9]) << 1)
                | u64::from(p[10] >> 7);
            let ext = (u64::from(p[10] & 1) << 8) | u64::from(p[11]);
            let t = (base * 300 + ext) as f64 / 27e6;
            if first.is_none() {
                first = Some((index, t));
            }
            last = Some((index, t));
        }
        index += 1;
    })?;
    Ok(match (first, last) {
        (Some((i0, t0)), Some((i1, t1))) if t1 > t0 + 0.5 => {
            Some((i1 - i0) as f64 * TS_LEN as f64 * 8.0 / (t1 - t0))
        }
        _ => None,
    })
}

/// Every IP datagram the file's MPE carries, fragments put back together,
/// with the signal time (seconds per TS packet: `dt`, 0 for none).
fn each_datagram(path: &Path, dt: f64, mut f: impl FnMut(&[u8], &IpInfo, f64)) -> Result<u64> {
    let mut mpe = MpeExtractor::new();
    let mut frags = Reassembler::new();
    let mut out = Vec::new();
    each_packet(path, |p| {
        mpe.packet(p, &mut out);
        for (d, info) in out.drain(..) {
            if let Some(whole) = frags.push(&d, &info) {
                let info = match &whole {
                    std::borrow::Cow::Borrowed(_) => Some(info),
                    std::borrow::Cow::Owned(w) => decsat_ip::parse(w),
                };
                if let Some(i) = info {
                    f(&whole, &i, 0.0);
                }
            }
        }
        if dt > 0.0 {
            f(&[], &NO_PACKET, dt);
        }
    })?;
    Ok(mpe.stats.datagrams)
}

/// A 16-bit WAV of `channels` interleaved in `pcm`.
pub(crate) fn write_wav(path: &Path, rate: u32, channels: u16, pcm: &[i16]) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let data = (pcm.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + data as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2 * channels as u32).to_le_bytes());
    b.extend_from_slice(&(2 * channels).to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for v in pcm {
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, b)?;
    Ok(())
}

/// Stands in for "no packet, just time passing" in `each_datagram`.
const NO_PACKET: IpInfo = IpInfo {
    src: std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
    dst: std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
    protocol: 0,
    len: 0,
    ports: None,
};

/// One stream's decode or recording in the second pass.
struct Run {
    stream: AudioStream,
    depay: Option<Result<Depacketizer, String>>,
    decoder: Decoder,
    units: Vec<Unit>,
    block: Block,
    seconds: f64,
    recorder: Option<Result<AudioRecorder, String>>,
    /// Decoded audio for `--wav`: rate and interleaved stereo samples.
    pcm: Option<(u32, Vec<i16>)>,
}

pub fn run(a: &McastArgs) -> Result<()> {
    let rate = match a.ts_rate {
        Some(r) => Some(r),
        None => pcr_rate(&a.file)?,
    };
    let dt = rate.map_or(0.0, |r| TS_LEN as f64 * 8.0 / r);
    let mut scan = McastScanner::new();
    let mut secs = 0.0;
    let datagrams = each_datagram(&a.file, dt, |d, info, t| {
        if d.is_empty() {
            scan.tick(t);
            secs += t;
        } else {
            scan.packet(d, info);
        }
    })?;
    if datagrams == 0 {
        bail!("no MPE datagrams in {}", a.file.display());
    }
    let streams = scan.streams();
    match rate {
        Some(r) => println!(
            "{datagrams} MPE datagrams, {secs:.1} s at {:.1} kbit/s; {} audio stream(s)",
            r / 1e3,
            streams.len()
        ),
        None => println!(
            "{datagrams} MPE datagrams (no PCR: pass --ts-rate to time streams without RTCP); \
             {} audio stream(s)",
            streams.len()
        ),
    }

    let mut runs: Vec<Run> = streams
        .iter()
        .map(|s| Run {
            stream: s.clone(),
            depay: (a.decode || a.wav.is_some()).then(|| Depacketizer::new(s)),
            decoder: Decoder::new(),
            units: Vec::new(),
            block: Block::default(),
            seconds: 0.0,
            recorder: a.record.as_ref().map(|dir| {
                let stem = format!("decsat-{}_{}", s.group, s.port).replace([':', '.'], "_");
                AudioRecorder::start(s, dir, &stem)
            }),
            pcm: a.wav.as_ref().map(|_| (0, Vec::new())),
        })
        .collect();
    if a.decode || a.record.is_some() || a.wav.is_some() {
        each_datagram(&a.file, 0.0, |d, info, _| {
            let Some((payload, _, dport)) = udp_payload(d, info) else {
                return;
            };
            for r in runs
                .iter_mut()
                .filter(|r| r.stream.group == info.dst && r.stream.port == dport)
            {
                if let Some(Ok(rec)) = &mut r.recorder {
                    rec.packet(payload);
                }
                if let Some(Ok(dp)) = &mut r.depay {
                    dp.packet(payload, &mut r.units);
                    for u in r.units.drain(..) {
                        if r.decoder.decode(&u, &mut r.block) && r.block.rate > 0 {
                            r.seconds += r.block.samples.len() as f64 / 2.0 / r.block.rate as f64;
                            if let Some((rate, pcm)) = &mut r.pcm {
                                *rate = r.block.rate;
                                pcm.extend(
                                    r.block
                                        .samples
                                        .iter()
                                        .map(|&v| (v.clamp(-1.0, 1.0) * 32767.0).round() as i16),
                                );
                            }
                        }
                    }
                }
            }
        })?;
    }

    for r in &runs {
        let s = &r.stream;
        println!(
            "\n{}:{}  {}  {}{}{}",
            s.group,
            s.port,
            s.codec.label(),
            if s.rate_bps > 0.0 {
                format!("{:.1} kbit/s  ", s.rate_bps / 1e3)
            } else {
                String::new()
            },
            match (s.rtp, s.pt) {
                (true, Some(pt)) => format!("RTP PT {pt}"),
                _ => "raw UDP".into(),
            },
            s.src.map(|a| format!("  from {a}")).unwrap_or_default()
        );
        match &s.sdp {
            Some(d) if d.inferred => {
                println!(
                    "    {}",
                    d.info.as_deref().unwrap_or("described from its packets")
                )
            }
            Some(d) => println!(
                "    announced{}",
                d.name
                    .as_deref()
                    .map(|n| format!(": {n}"))
                    .unwrap_or_default()
            ),
            None => println!("    not described (no announcement, too little to go on)"),
        }
        match &r.depay {
            Some(Err(e)) => println!("    decode: {e}"),
            Some(Ok(dp)) => println!(
                "    decoded {:.1} s in {} units, {} errors{}{}{}",
                r.seconds,
                r.decoder.decoded,
                r.decoder.errors,
                r.decoder
                    .description
                    .as_deref()
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default(),
                if dp.lost > 0 {
                    format!(" · {} RTP packets lost", dp.lost)
                } else {
                    String::new()
                },
                r.decoder
                    .last_error
                    .as_deref()
                    .map(|e| format!(" · last error: {e}"))
                    .unwrap_or_default()
            ),
            None => {}
        }
        if let (Some(dir), Some((rate, pcm))) = (&a.wav, &r.pcm)
            && *rate > 0
        {
            let stem = format!("decsat-{}_{}", s.group, s.port).replace([':', '.'], "_");
            let path = dir.join(format!("{stem}.wav"));
            match write_wav(&path, *rate, 2, pcm) {
                Ok(()) => println!("    wrote {} ({:.1} s)", path.display(), r.seconds),
                Err(e) => println!("    wav: {e}"),
            }
        }
        match &r.recorder {
            Some(Ok(rec)) => println!(
                "    recorded {} ({} bytes){}",
                rec.path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                rec.bytes,
                rec.error
                    .as_deref()
                    .map(|e| format!(" · {e}"))
                    .unwrap_or_default()
            ),
            Some(Err(e)) => println!("    record: {e}"),
            None => {}
        }
    }
    let playing = scan.now_playing();
    if !playing.is_empty() {
        println!("\nNow playing ({}:{}):", playing[0].group, playing[0].port);
        for n in &playing {
            let what = match (&n.artist, &n.title) {
                (Some(a), Some(t)) => format!("{a} – {t}"),
                (None, Some(t)) => t.clone(),
                (Some(a), None) => a.clone(),
                (None, None) => "—".into(),
            };
            println!(
                "    {:<10} {what}{}",
                n.station,
                n.kind
                    .as_deref()
                    .map(|k| format!("  ({})", k.to_lowercase()))
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}
