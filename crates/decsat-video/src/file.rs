// made by CasualArclamp
//! A recorded transport stream (a VFO's `.ts`) played as if it were on air:
//! its services read from the file's own tables, and its packets handed to
//! a [`TsTap`] at the pace its PCRs give, so the player sees what a live
//! carrier would have given it.

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use decsat_ts::{Programme, TS_LEN, TsAnalyser, TsTap};

/// Bytes read for the services (PAT, PMT, SDT come round at least once a
/// second, so a few MB is many repeats on any carrier).
const SCAN_BYTES: u64 = 8 << 20;
/// Packets handed over at once.
const BATCH: usize = 32;
/// A PCR step beyond this (or backwards) is a new clock: start timing again.
const PCR_JUMP_S: f64 = 5.0;
/// Pace without any PCR: packets a second (about 1.5 Mbit/s).
const NO_PCR_RATE: f64 = 1000.0;

/// The services in `path`, as its PAT, PMTs and SDT give them.
pub fn programmes(path: &Path) -> Result<Vec<Programme>, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut data = Vec::new();
    f.take(SCAN_BYTES)
        .read_to_end(&mut data)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut a = TsAnalyser::new();
    for p in packets(&data) {
        a.packet(p);
    }
    a.tick(1.0);
    let progs = a.report().programmes;
    if progs.is_empty() {
        return Err(format!(
            "{}: no services found (no PAT/PMT in its first {} MB)",
            path.display(),
            SCAN_BYTES >> 20
        ));
    }
    Ok(progs)
}

/// The whole 188-byte packets in `data`, from the first sync byte that
/// three packets in a row agree on.
fn packets(data: &[u8]) -> impl Iterator<Item = &[u8; TS_LEN]> {
    let start = (0..TS_LEN.min(data.len()))
        .find(|&i| (0..3).all(|k| data.get(i + k * TS_LEN).is_none_or(|&b| b == 0x47)))
        .unwrap_or(0);
    data[start..]
        .as_chunks::<TS_LEN>()
        .0
        .iter()
        .filter(|p| p[0] == 0x47)
}

/// A packet's PCR, in 27 MHz ticks (ISO/IEC 13818-1 §2.4.3.5: 33-bit base
/// at 90 kHz, 9-bit extension).
fn pcr(p: &[u8; TS_LEN]) -> Option<u64> {
    let has_af = p[3] & 0x20 != 0;
    if !has_af || p[4] < 7 || p[5] & 0x10 == 0 {
        return None;
    }
    let b = &p[6..12];
    let base = (u64::from(b[0]) << 25)
        | (u64::from(b[1]) << 17)
        | (u64::from(b[2]) << 9)
        | (u64::from(b[3]) << 1)
        | u64::from(b[4] >> 7);
    let ext = (u64::from(b[4] & 1) << 8) | u64::from(b[5]);
    Some(base * 300 + ext)
}

/// The file's packets going to a tap; dropping it stops them.
pub struct FileFeed {
    quit: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    sent: Arc<AtomicU64>,
    total: u64,
    thread: Option<JoinHandle<()>>,
}

impl FileFeed {
    /// Hand `path`'s packets to `tap` in real time, from the start, timed
    /// by the PCRs on `pcr_pid` (any PID's when `None`).
    pub fn start(path: &Path, pcr_pid: Option<u16>, tap: TsTap) -> Result<FileFeed, String> {
        let total = std::fs::metadata(path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .len()
            / TS_LEN as u64;
        let quit = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicU64::new(0));
        // Clones of the `Arc`s move into the thread; these stay here.
        let (q, d, s, p) = (
            quit.clone(),
            done.clone(),
            sent.clone(),
            PathBuf::from(path),
        );
        let thread = std::thread::Builder::new()
            .name("decsat-ts-file".into())
            .spawn(move || {
                let _ = run(&p, pcr_pid, &tap, &q, &s);
                d.store(true, Ordering::Relaxed);
            })
            .map_err(|e| e.to_string())?;
        Ok(FileFeed {
            quit,
            done,
            sent,
            total,
            thread: Some(thread),
        })
    }

    /// How far through the file, 0..=1.
    pub fn progress(&self) -> f32 {
        if self.total == 0 {
            return 1.0;
        }
        (self.sent.load(Ordering::Relaxed) as f32 / self.total as f32).min(1.0)
    }

    /// The whole file has been handed over.
    pub fn finished(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }
}

impl Drop for FileFeed {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(
    path: &Path,
    pcr_pid: Option<u16>,
    tap: &TsTap,
    quit: &AtomicBool,
    sent: &AtomicU64,
) -> Result<(), String> {
    let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut r = BufReader::with_capacity(1 << 20, f);
    let mut buf = vec![0u8; TS_LEN * 1024];
    let mut carry: Vec<u8> = Vec::new();
    // (wall time, PCR) the timing is reckoned from.
    let mut origin: Option<(Instant, u64)> = None;
    let mut last_pcr: Option<u64> = None;
    let t0 = Instant::now();
    let mut count: u64 = 0;
    let mut batch: Vec<[u8; TS_LEN]> = Vec::with_capacity(BATCH);
    loop {
        let n = r.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        carry.extend_from_slice(&buf[..n]);
        let mut at = 0;
        while at + TS_LEN <= carry.len() {
            // Out of step: look for the sync byte again.
            if carry[at] != 0x47 {
                at += 1;
                continue;
            }
            let p: [u8; TS_LEN] = carry[at..at + TS_LEN].try_into().unwrap();
            at += TS_LEN;
            count += 1;
            let pid = u16::from_be_bytes([p[1] & 0x1F, p[2]]);
            if pcr_pid.is_none_or(|w| w == pid)
                && let Some(v) = pcr(&p)
            {
                // The first, a jump, or the clock going back: time from here.
                let jumped = last_pcr.is_none_or(|l| v < l || (v - l) as f64 / 27e6 > PCR_JUMP_S);
                if jumped {
                    origin = Some((Instant::now(), v));
                }
                last_pcr = Some(v);
                if let Some((w0, p0)) = origin {
                    let due = w0 + Duration::from_secs_f64((v - p0) as f64 / 27e6);
                    flush(tap, &mut batch, sent)?;
                    sleep_until(due, quit);
                }
            } else if last_pcr.is_none() {
                // No PCR yet: a steady trickle.
                sleep_until(
                    t0 + Duration::from_secs_f64(count as f64 / NO_PCR_RATE),
                    quit,
                );
            }
            if quit.load(Ordering::Relaxed) {
                return Ok(());
            }
            batch.push(p);
            if batch.len() == BATCH {
                flush(tap, &mut batch, sent)?;
            }
        }
        carry.drain(..at);
    }
    flush(tap, &mut batch, sent)
}

/// Hand `batch` on, waiting while the taker is full (a file can wait, a
/// carrier cannot).
fn flush(tap: &TsTap, batch: &mut Vec<[u8; TS_LEN]>, sent: &AtomicU64) -> Result<(), String> {
    if batch.is_empty() {
        return Ok(());
    }
    if !tap.send_blocking(batch) {
        return Err("the player has gone".into());
    }
    sent.fetch_add(batch.len() as u64, Ordering::Relaxed);
    batch.clear();
    Ok(())
}

/// Sleep until `due`, in short steps so a stop is seen.
fn sleep_until(due: Instant, quit: &AtomicBool) {
    loop {
        let now = Instant::now();
        if now >= due || quit.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep((due - now).min(Duration::from_millis(50)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcr_reads_base_and_extension() {
        let mut p = [0xFFu8; TS_LEN];
        p[..4].copy_from_slice(&[0x47, 0x01, 0x00, 0x20]);
        p[4] = 7;
        p[5] = 0x10;
        // base 90 000 (1 s), extension 5.
        let base: u64 = 90_000;
        p[6] = (base >> 25) as u8;
        p[7] = (base >> 17) as u8;
        p[8] = (base >> 9) as u8;
        p[9] = (base >> 1) as u8;
        p[10] = (((base & 1) << 7) as u8) | 0x7E;
        p[11] = 5;
        assert_eq!(pcr(&p), Some(27_000_005));
        p[5] = 0;
        assert_eq!(pcr(&p), None);
    }

    #[test]
    fn packets_are_found_past_junk() {
        let mut data = vec![0u8; 5];
        for i in 0..4u8 {
            let mut p = [i; TS_LEN];
            p[0] = 0x47;
            data.extend_from_slice(&p);
        }
        let got: Vec<u8> = packets(&data).map(|p| p[1]).collect();
        assert_eq!(got, [0, 1, 2, 3]);
    }
}
