// made by CasualArclamp
//! Two-channel WAV files as IQ captures: I on the left channel, Q on the
//! right, as SDR#, HDSDR, SDRuno, SDR++ and others record them.
//!
//! The header gives what a raw file leaves to its name: the sample format
//! (8-bit unsigned, 16- or 24-bit signed PCM, 32-bit float), the sample rate,
//! and where the samples are (the `data` chunk — other chunks may come
//! before or after it). Files over 4 GiB come as RF64 (EBU Tech 3306), whose
//! `ds64` chunk carries the real sizes. SDR#, HDSDR and SDRuno also write an
//! `auxi` chunk whose centre frequency is read when present.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use decsat_core::{Result, SampleFormat};

/// What a WAV IQ file's header says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WavIq {
    pub format: SampleFormat,
    pub sample_rate: f64,
    /// Byte offset and length of the samples (the `data` chunk).
    pub data_offset: u64,
    pub data_len: u64,
    /// From an `auxi` chunk, when there is one.
    pub center_freq: Option<f64>,
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn invalid(msg: &str) -> decsat_core::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, format!("WAV: {msg}")).into()
}

/// Read `path`'s header if it is a WAV (RIFF or RF64) file: `Ok(None)` for
/// any other file, an error for a WAV that is not two-channel IQ in a format
/// this reader takes.
pub fn probe(path: &Path) -> Result<Option<WavIq>> {
    let mut f = File::open(path)?;
    let mut head = [0u8; 12];
    if f.read(&mut head)? < 12 || &head[8..12] != b"WAVE" {
        return Ok(None);
    }
    let rf64 = match &head[0..4] {
        b"RIFF" => false,
        b"RF64" | b"BW64" => true,
        _ => return Ok(None),
    };
    let file_len = f.metadata()?.len();
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    let mut ds64_data: Option<u64> = None;
    let mut center = None;
    let mut at = 12u64;
    // Walk the chunks: id, 32-bit size, body (padded to an even length).
    while at + 8 <= file_len {
        f.seek(SeekFrom::Start(at))?;
        let mut ch = [0u8; 8];
        f.read_exact(&mut ch)?;
        let id = &ch[0..4];
        let mut size = u64::from(u32_at(&ch, 4));
        let body = at + 8;
        match id {
            b"ds64" | b"fmt " | b"auxi" => {
                let mut b = vec![0u8; size.min(256) as usize];
                f.read_exact(&mut b)?;
                match id {
                    // ds64: RIFF size (8), data size (8), sample count (8).
                    b"ds64" if b.len() >= 16 => {
                        ds64_data = Some(u64::from_le_bytes(b[8..16].try_into().unwrap()));
                    }
                    b"fmt " if b.len() >= 16 => {
                        let mut tag = u16_at(&b, 0);
                        // WAVE_FORMAT_EXTENSIBLE: the real tag opens the
                        // sub-format GUID.
                        if tag == 0xFFFE && b.len() >= 26 {
                            tag = u16_at(&b, 24);
                        }
                        fmt = Some((tag, u16_at(&b, 2), u32_at(&b, 4), u16_at(&b, 14)));
                    }
                    // auxi (SDR#/HDSDR/SDRuno): start and stop times as two
                    // 16-byte SYSTEMTIMEs, then the centre frequency in Hz.
                    b"auxi" if b.len() >= 36 => {
                        let hz = u32_at(&b, 32);
                        if hz > 0 {
                            center = Some(f64::from(hz));
                        }
                    }
                    _ => {}
                }
            }
            b"data" => {
                if rf64 && size == 0xFFFF_FFFF {
                    size = ds64_data.ok_or_else(|| invalid("RF64 without a ds64 size"))?;
                }
                let (tag, channels, rate, bits) =
                    fmt.ok_or_else(|| invalid("no fmt chunk before the data"))?;
                if channels != 2 {
                    return Err(invalid(&format!(
                        "{channels} channel(s): IQ needs two (I left, Q right)"
                    )));
                }
                let format = match (tag, bits) {
                    (1, 8) => SampleFormat::Cu8,
                    (1, 16) => SampleFormat::Cs16,
                    (1, 24) => SampleFormat::Cs24,
                    (3, 32) => SampleFormat::Cf32,
                    _ => {
                        return Err(invalid(&format!(
                            "format {tag} with {bits}-bit samples is not supported \
                             (8/16/24-bit PCM or 32-bit float)"
                        )));
                    }
                };
                // A data chunk cut short by an interrupted recording: read
                // what is there.
                let len = size.min(file_len.saturating_sub(body));
                return Ok(Some(WavIq {
                    format,
                    sample_rate: f64::from(rate),
                    data_offset: body,
                    data_len: len,
                    center_freq: center,
                }));
            }
            _ => {}
        }
        at = body + size + (size & 1);
    }
    Err(invalid("no data chunk"))
}

/// What can be told about a capture before opening it: from a WAV header if
/// it has one, else from its extension (format) — the caller may add its
/// own guesses from the file name.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CaptureInfo {
    pub format: Option<SampleFormat>,
    pub sample_rate: Option<f64>,
    pub center_freq: Option<f64>,
    /// The values come from a WAV header (authoritative).
    pub wav: bool,
}

pub fn probe_capture(path: &Path) -> Result<CaptureInfo> {
    Ok(match probe(path)? {
        Some(w) => CaptureInfo {
            format: Some(w.format),
            sample_rate: Some(w.sample_rate),
            center_freq: w.center_freq,
            wav: true,
        },
        None => CaptureInfo {
            format: crate::format_from_path(path),
            ..CaptureInfo::default()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IqFileReader, IqSource};
    use decsat_core::Iq;

    /// A WAV file: `tag`, two channels, `bits`, `rate`, the given sample
    /// bytes, optionally an auxi chunk first and a trailing chunk after.
    fn wav(tag: u16, bits: u16, rate: u32, data: &[u8], auxi_hz: Option<u32>) -> Vec<u8> {
        let mut body = b"WAVE".to_vec();
        if let Some(hz) = auxi_hz {
            let mut a = vec![0u8; 32];
            a.extend_from_slice(&hz.to_le_bytes());
            a.extend_from_slice(&[0u8; 16]);
            body.extend_from_slice(b"auxi");
            body.extend_from_slice(&(a.len() as u32).to_le_bytes());
            body.extend_from_slice(&a);
        }
        let block = 2 * bits / 8;
        body.extend_from_slice(b"fmt ");
        body.extend_from_slice(&16u32.to_le_bytes());
        body.extend_from_slice(&tag.to_le_bytes());
        body.extend_from_slice(&2u16.to_le_bytes());
        body.extend_from_slice(&rate.to_le_bytes());
        body.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
        body.extend_from_slice(&block.to_le_bytes());
        body.extend_from_slice(&bits.to_le_bytes());
        body.extend_from_slice(b"data");
        body.extend_from_slice(&(data.len() as u32).to_le_bytes());
        body.extend_from_slice(data);
        // A trailing chunk that must not be read as samples.
        body.extend_from_slice(b"LIST");
        body.extend_from_slice(&4u32.to_le_bytes());
        body.extend_from_slice(b"INFO");
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    fn read_all(path: &Path) -> (IqFileReader, Vec<Iq>) {
        // The arguments are wrong on purpose: the header must win.
        let mut r = IqFileReader::open(path, SampleFormat::Cs8, 1.0, 3).unwrap();
        let mut all = Vec::new();
        let mut buf = Vec::new();
        while r.read(&mut buf).unwrap() > 0 {
            all.extend_from_slice(&buf);
        }
        (r, all)
    }

    #[test]
    fn sixteen_bit_wav_reads_its_data_chunk_only() {
        let samples: Vec<u8> = [1000i16, -1000, 16384, -16384, 0, 32767]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let path = std::env::temp_dir().join("decsat-wav16.wav");
        std::fs::write(&path, wav(1, 16, 2_400_000, &samples, Some(1_635_640_000))).unwrap();
        let info = probe(&path).unwrap().unwrap();
        assert_eq!(info.format, SampleFormat::Cs16);
        assert_eq!(info.sample_rate, 2_400_000.0);
        assert_eq!(info.center_freq, Some(1_635_640_000.0));
        let (mut r, got) = read_all(&path);
        assert_eq!(r.sample_rate(), 2_400_000.0);
        assert_eq!(r.center_freq(), 1_635_640_000.0);
        assert_eq!(got.len(), 3, "the LIST chunk after the data is not samples");
        assert!((got[1].re - 0.5).abs() < 1e-6 && (got[1].im + 0.5).abs() < 1e-6);
        // Rewinding goes back to the first sample, not the header.
        r.rewind().unwrap();
        let mut buf = Vec::new();
        r.read(&mut buf).unwrap();
        assert_eq!(buf[0], got[0]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn float_eight_and_twentyfour_bit_wavs() {
        let dir = std::env::temp_dir();
        let f32s: Vec<u8> = [0.25f32, -0.75]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let p = dir.join("decsat-wavf.wav");
        std::fs::write(&p, wav(3, 32, 48_000, &f32s, None)).unwrap();
        let (_, got) = read_all(&p);
        assert_eq!(got, vec![Iq::new(0.25, -0.75)]);
        let p8 = dir.join("decsat-wav8.wav");
        std::fs::write(&p8, wav(1, 8, 48_000, &[255, 0], None)).unwrap();
        assert_eq!(probe(&p8).unwrap().unwrap().format, SampleFormat::Cu8);
        let p24 = dir.join("decsat-wav24.wav");
        std::fs::write(&p24, wav(1, 24, 96_000, &[0, 0, 0x40, 0, 0, 0xC0], None)).unwrap();
        let (_, got) = read_all(&p24);
        assert_eq!(got, vec![Iq::new(0.5, -0.5)]);
        for p in [p, p8, p24] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn mono_wav_is_refused_and_raw_files_are_not_wav() {
        let dir = std::env::temp_dir();
        let mut mono = wav(1, 16, 8000, &[0, 0, 0, 0], None);
        mono[22] = 1; // channels
        let p = dir.join("decsat-wavmono.wav");
        std::fs::write(&p, mono).unwrap();
        assert!(probe(&p).is_err());
        let raw = dir.join("decsat-raw.cs8");
        std::fs::write(&raw, [1u8, 2, 3, 4]).unwrap();
        assert_eq!(probe(&raw).unwrap(), None);
        for p in [p, raw] {
            let _ = std::fs::remove_file(p);
        }
    }
}
