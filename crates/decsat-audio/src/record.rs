// made by CasualArclamp
//! Recording a stream to a file, as broadcast — no decoding, no re-encoding.
//!
//! The file type follows what the stream carries, and is chosen on the first
//! frame (so the extension is right however the stream was sniffed):
//! - MPEG audio frames as they are: `.mp2` (`.mp1`, `.mp3` by layer);
//! - AAC of any carriage (ADTS, LATM, RFC 3640) as ADTS: `.aac`, which every
//!   player opens and which keeps HE-AAC's SBR and PS data intact;
//! - PCM as 16-bit `.wav`;
//! - MPEG-TS as `.ts`;
//! - Opus in Ogg (RFC 7845): `.opus`.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use decsat_ip::AudioStream;

use crate::depay::{Depacketizer, Unit};
use crate::es::MpaHeader;

/// Writes one stream to a file.
pub struct AudioRecorder {
    depay: Depacketizer,
    dir: PathBuf,
    stem: String,
    file: Option<BufWriter<File>>,
    path: Option<PathBuf>,
    /// WAV: (sample rate, channels, data bytes) for the header.
    wav: Option<(u32, u16, u64)>,
    /// Ogg Opus: the page writer.
    ogg: Option<OggOpus>,
    units: Vec<Unit>,
    /// Bytes written.
    pub bytes: u64,
    pub error: Option<String>,
}

impl AudioRecorder {
    /// Record `s` into `dir` as `<stem>.<ext>`; the file opens with the
    /// first frame. Fails at once for a stream that cannot be taken apart.
    pub fn start(s: &AudioStream, dir: &Path, stem: &str) -> Result<AudioRecorder, String> {
        Ok(AudioRecorder {
            depay: Depacketizer::new(s)?,
            dir: dir.to_path_buf(),
            stem: stem.to_string(),
            file: None,
            path: None,
            wav: None,
            ogg: None,
            units: Vec::new(),
            bytes: 0,
            error: None,
        })
    }

    /// The file, once opened.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// One UDP payload of the stream.
    pub fn packet(&mut self, udp_payload: &[u8]) {
        if self.error.is_some() {
            return;
        }
        let mut units = std::mem::take(&mut self.units);
        self.depay.packet(udp_payload, &mut units);
        for u in units.drain(..) {
            if let Err(e) = self.write(&u) {
                self.error = Some(e);
                self.file = None;
                break;
            }
        }
        self.units = units;
    }

    fn open(&mut self, u: &Unit) -> Result<(), String> {
        let ext = match u {
            Unit::Mpa(f) => match MpaHeader::parse(f).map(|h| h.layer) {
                Some(1) => "mp1",
                Some(3) => "mp3",
                _ => "mp2",
            },
            Unit::Aac(..) => "aac",
            Unit::Pcm { .. } => "wav",
            Unit::Ts(_) => "ts",
            Unit::Opus(_) => "opus",
        };
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let path = self.dir.join(format!("{}.{ext}", self.stem));
        let f = File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut w = BufWriter::new(f);
        if let Unit::Pcm { rate, channels, .. } = u {
            w.write_all(&wav_header(*rate, *channels, 0))
                .map_err(|e| e.to_string())?;
            self.wav = Some((*rate, *channels, 0));
        }
        if let Unit::Opus(p) = u {
            // Stereo by the first packet's TOC byte (RFC 6716 §3.1).
            let channels = if p[0] & 0x04 != 0 { 2 } else { 1 };
            let mut ogg = OggOpus {
                serial: std::process::id() ^ 0x0D_EC_D7_B5,
                seq: 0,
                granule: 0,
            };
            w.write_all(&ogg.headers(channels))
                .map_err(|e| e.to_string())?;
            self.ogg = Some(ogg);
        }
        self.file = Some(w);
        self.path = Some(path);
        Ok(())
    }

    fn write(&mut self, u: &Unit) -> Result<(), String> {
        if self.file.is_none() {
            self.open(u)?;
        }
        let w = self.file.as_mut().unwrap();
        let n = match u {
            Unit::Mpa(f) | Unit::Ts(f) => {
                w.write_all(f).map_err(|e| e.to_string())?;
                f.len()
            }
            Unit::Aac(cfg, au) => {
                w.write_all(&cfg.adts_header(au.len()))
                    .and_then(|_| w.write_all(au))
                    .map_err(|e| e.to_string())?;
                au.len() + 7
            }
            Unit::Pcm { samples, .. } => {
                let mut b = Vec::with_capacity(samples.len() * 2);
                for &s in samples {
                    let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                    b.extend_from_slice(&v.to_le_bytes());
                }
                w.write_all(&b).map_err(|e| e.to_string())?;
                if let Some(wav) = &mut self.wav {
                    wav.2 += b.len() as u64;
                }
                b.len()
            }
            Unit::Opus(p) => {
                let page = self.ogg.as_mut().map(|o| o.packet(p)).unwrap_or_default();
                w.write_all(&page).map_err(|e| e.to_string())?;
                page.len()
            }
        };
        self.bytes += n as u64;
        Ok(())
    }

    /// Flush, and complete a WAV header.
    fn finish(&mut self) -> std::io::Result<()> {
        let Some(mut w) = self.file.take() else {
            return Ok(());
        };
        w.flush()?;
        if let Some((rate, ch, data)) = self.wav {
            let mut f = w.into_inner().map_err(|e| e.into_error())?;
            f.seek(SeekFrom::Start(0))?;
            f.write_all(&wav_header(rate, ch, data))?;
        }
        Ok(())
    }
}

impl Drop for AudioRecorder {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// Samples (at 48 kHz) in an Opus packet: its frames times their length
/// (RFC 6716 §3.1–3.2).
fn opus_samples(p: &[u8]) -> u64 {
    let Some(&toc) = p.first() else {
        return 0;
    };
    let config = toc >> 3;
    // Frame length in 48 kHz samples (§3.1, Table 2): SILK, hybrid, CELT.
    let frame = match config {
        0..=11 => [480, 960, 1920, 2880][(config % 4) as usize],
        12..=15 => [480, 960][(config % 2) as usize],
        _ => [120, 240, 480, 960][(config % 4) as usize],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => p.get(1).map_or(0, |c| c & 0x3F) as u64,
    };
    frame * frames
}

/// Ogg Opus (RFC 7845): the ID and comment headers, then each packet in a
/// page of its own, its granule position the samples so far at 48 kHz.
struct OggOpus {
    serial: u32,
    seq: u32,
    granule: u64,
}

impl OggOpus {
    /// The ID header (§5.1) and comment header (§5.2), a page each.
    fn headers(&mut self, channels: u8) -> Vec<u8> {
        let mut head = b"OpusHead".to_vec();
        head.push(1); // version
        head.push(channels);
        head.extend_from_slice(&0u16.to_le_bytes()); // pre-skip: not known
        head.extend_from_slice(&48_000u32.to_le_bytes()); // input rate
        head.extend_from_slice(&0i16.to_le_bytes()); // output gain
        head.push(0); // channel mapping family 0: mono or stereo
        let mut tags = b"OpusTags".to_vec();
        let vendor = b"DecSAT";
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor);
        tags.extend_from_slice(&0u32.to_le_bytes()); // no comments
        let mut out = self.page(&head, 0x02, 0); // beginning of stream
        out.extend(self.page(&tags, 0, 0));
        out
    }

    /// One audio packet's page.
    fn packet(&mut self, p: &[u8]) -> Vec<u8> {
        self.granule += opus_samples(p);
        self.page(p, 0, self.granule)
    }

    /// A page holding one packet (Ogg, RFC 3533 §6): lacing values of 255
    /// then the remainder, so the packet must be under 255 × 255 bytes.
    fn page(&mut self, packet: &[u8], flags: u8, granule: u64) -> Vec<u8> {
        if packet.len() >= 255 * 255 {
            return Vec::new();
        }
        let mut lacing = vec![255u8; packet.len() / 255];
        lacing.push((packet.len() % 255) as u8);
        let mut h = Vec::with_capacity(27 + lacing.len() + packet.len());
        h.extend_from_slice(b"OggS");
        h.push(0); // version
        h.push(flags);
        h.extend_from_slice(&granule.to_le_bytes());
        h.extend_from_slice(&self.serial.to_le_bytes());
        h.extend_from_slice(&self.seq.to_le_bytes());
        h.extend_from_slice(&[0; 4]); // CRC, filled in below
        h.push(lacing.len() as u8);
        h.extend_from_slice(&lacing);
        h.extend_from_slice(packet);
        let crc = ogg_crc(&h);
        h[22..26].copy_from_slice(&crc.to_le_bytes());
        self.seq += 1;
        h
    }
}

/// Ogg's page CRC (RFC 3533 §6): CRC-32, polynomial 0x04C11DB7, MSB first,
/// starting from zero with no final inversion.
fn ogg_crc(b: &[u8]) -> u32 {
    let mut c = 0u32;
    for &x in b {
        c ^= u32::from(x) << 24;
        for _ in 0..8 {
            c = if c & 0x8000_0000 != 0 {
                (c << 1) ^ 0x04C1_1DB7
            } else {
                c << 1
            };
        }
    }
    c
}

/// A 44-byte PCM WAV header for `data` bytes of 16-bit audio.
fn wav_header(rate: u32, channels: u16, data: u64) -> [u8; 44] {
    let data = data.min(u32::MAX as u64 - 36) as u32;
    let block = channels * 2;
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * block as u32).to_le_bytes());
    h[32..34].copy_from_slice(&block.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aac::{loas_frame, silent_aac_au};
    use decsat_ip::Codec;
    use decsat_ip::mcast::{rtp_packet, silent_mp2_frame};

    fn stream(rtp: bool, pt: Option<u8>, codec: Codec) -> AudioStream {
        AudioStream {
            group: "239.1.1.1".parse().unwrap(),
            port: 5004,
            src: None,
            packets: 100,
            rate_bps: 1e5,
            rtp,
            pt,
            codec,
            sdp: None,
        }
    }

    fn dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("decsat-rec-{tag}-{}", std::process::id()))
    }

    #[test]
    fn mp2_is_written_as_is() {
        let d = dir("mp2");
        let mut r =
            AudioRecorder::start(&stream(true, Some(14), Codec::MpegAudio), &d, "x").unwrap();
        for k in 0..10u16 {
            let mut p = vec![0, 0, 0, 0];
            p.extend_from_slice(&silent_mp2_frame());
            r.packet(&rtp_packet(14, k, 0, 1, &p));
        }
        let path = r.path().unwrap().to_path_buf();
        drop(r);
        let b = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(path.extension().unwrap(), "mp2");
        assert!(b.len() >= 9 * 384 && b.len().is_multiple_of(384));
        assert_eq!(&b[..384], &silent_mp2_frame()[..]);
    }

    #[test]
    fn latm_becomes_adts() {
        let d = dir("aac");
        let mut r = AudioRecorder::start(&stream(false, None, Codec::AacLatm), &d, "y").unwrap();
        let au = silent_aac_au(2);
        for _ in 0..5 {
            r.packet(&loas_frame(3, 2, &au));
        }
        let path = r.path().unwrap().to_path_buf();
        drop(r);
        let b = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(path.extension().unwrap(), "aac");
        let h = crate::es::AdtsHeader::parse(&b).unwrap();
        assert_eq!(h.frame_len, 7 + au.len());
        assert_eq!(&b[7..7 + au.len()], &au[..]);
    }

    #[test]
    fn opus_goes_into_ogg() {
        let d = dir("opus");
        let mut r = AudioRecorder::start(&stream(true, Some(96), Codec::Opus), &d, "o").unwrap();
        // Five 20 ms CELT frames (TOC 0xFF: stereo, code 3), CBR.
        let mut p = vec![0xFF, 0x05];
        p.extend_from_slice(&[0x11; 5 * 100]);
        for k in 0..3u16 {
            r.packet(&rtp_packet(96, k, k as u32 * 4800, 1, &p));
        }
        let path = r.path().unwrap().to_path_buf();
        drop(r);
        let b = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(path.extension().unwrap(), "opus");
        // Pages: OpusHead, OpusTags, then three of audio.
        let mut at = 0;
        let mut pages = Vec::new();
        while at < b.len() {
            assert_eq!(&b[at..at + 4], b"OggS");
            let mut page = b[at..].to_vec();
            let n = page[26] as usize;
            let body: usize = page[27..27 + n].iter().map(|&l| l as usize).sum();
            page.truncate(27 + n + body);
            let crc = u32::from_le_bytes(page[22..26].try_into().unwrap());
            page[22..26].copy_from_slice(&[0; 4]);
            assert_eq!(ogg_crc(&page), crc);
            let granule = u64::from_le_bytes(page[6..14].try_into().unwrap());
            pages.push((page[27 + n..].to_vec(), granule));
            at += page.len();
        }
        assert_eq!(pages.len(), 5);
        assert_eq!(&pages[0].0[..8], b"OpusHead");
        assert_eq!(pages[0].0[9], 2, "stereo");
        assert_eq!(&pages[1].0[..8], b"OpusTags");
        assert_eq!(pages[2].0, p);
        assert_eq!(pages[4].1, 3 * 4800);
    }

    #[test]
    fn ogg_crc_matches_a_known_page() {
        // CRC-32/POSIX (cksum) without its final inversion: the check
        // value 0x765E7680 inverted.
        assert_eq!(ogg_crc(b"123456789"), 0x89A1_897F);
    }

    #[test]
    fn pcm_makes_a_complete_wav() {
        let d = dir("wav");
        let mut r = AudioRecorder::start(&stream(true, Some(0), Codec::Pcm), &d, "z").unwrap();
        for k in 0..3u16 {
            r.packet(&rtp_packet(0, k, 0, 1, &[0xFF; 160]));
        }
        let path = r.path().unwrap().to_path_buf();
        drop(r);
        let b = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(&b[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 8000);
        assert_eq!(
            u32::from_le_bytes(b[40..44].try_into().unwrap()),
            3 * 160 * 2
        );
        assert_eq!(b.len(), 44 + 3 * 160 * 2);
    }
}
