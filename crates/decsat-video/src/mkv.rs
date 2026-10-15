//! Reading the Matroska stream ffmpeg writes to the player: raw RGBA video
//! and float PCM, each frame with its timestamp.
//!
//! Matroska is EBML (RFC 8794): each element is an ID, a size and a body,
//! IDs and sizes in a variable-length integer whose leading zero bits give
//! its length. ffmpeg writing to a pipe (`-live 1`) gives the Segment an
//! unknown size and writes each Cluster whole once done.
//!
//! The reader does not track nesting: the few container elements whose
//! children matter (Segment, Info, Tracks, TrackEntry, Video, Audio,
//! Cluster, BlockGroup) are stepped into, every other element is skipped
//! by its size. IDs and layouts: RFC 9559 (Matroska) §5.1.

use std::io::{self, Read};

const ID_EBML: u32 = 0x1A45_DFA3;
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_INFO: u32 = 0x1549_A966;
const ID_TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const ID_TRACKS: u32 = 0x1654_AE6B;
const ID_TRACK_ENTRY: u32 = 0xAE;
const ID_TRACK_NUMBER: u32 = 0xD7;
const ID_TRACK_TYPE: u32 = 0x83;
const ID_CODEC_ID: u32 = 0x86;
const ID_VIDEO: u32 = 0xE0;
const ID_PIXEL_WIDTH: u32 = 0xB0;
const ID_PIXEL_HEIGHT: u32 = 0xBA;
const ID_AUDIO: u32 = 0xE1;
const ID_SAMPLING_FREQUENCY: u32 = 0xB5;
const ID_CHANNELS: u32 = 0x9F;
const ID_CLUSTER: u32 = 0x1F43_B675;
const ID_TIMESTAMP: u32 = 0xE7;
const ID_SIMPLE_BLOCK: u32 = 0xA3;
const ID_BLOCK_GROUP: u32 = 0xA0;
const ID_BLOCK: u32 = 0xA1;

/// A body bigger than this is not believed (a 4K RGBA frame is 33 MB).
const MAX_BODY: u64 = 64 << 20;

/// One track, from the Tracks element.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Track {
    pub number: u64,
    /// 1 video, 2 audio (RFC 9559 §5.1.4.1.3).
    pub kind: u64,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub rate: f64,
    pub channels: u32,
}

/// What the stream holds next.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Every track, given once, before the first block.
    Tracks(Vec<Track>),
    /// A frame of a track, at `ms` milliseconds.
    Block { track: u64, ms: i64, data: Vec<u8> },
}

/// Reads [`Event`]s from a Matroska byte stream.
pub struct MkvReader<R: Read> {
    r: R,
    /// Nanoseconds per timestamp unit (1 ms unless the Info says).
    scale_ns: u64,
    cluster: i64,
    tracks: Vec<Track>,
    tracks_sent: bool,
}

impl<R: Read> MkvReader<R> {
    pub fn new(r: R) -> Self {
        MkvReader {
            r,
            scale_ns: 1_000_000,
            cluster: 0,
            tracks: Vec::new(),
            tracks_sent: false,
        }
    }

    /// The next event; `None` at the end of the stream.
    pub fn next_event(&mut self) -> io::Result<Option<Event>> {
        loop {
            let Some(id) = self.read_id()? else {
                return Ok(None);
            };
            let size = self.read_size()?;
            match id {
                // Containers: step in.
                ID_SEGMENT | ID_INFO | ID_TRACKS | ID_VIDEO | ID_AUDIO | ID_BLOCK_GROUP => {}
                ID_TRACK_ENTRY => self.tracks.push(Track::default()),
                ID_CLUSTER => {
                    if !self.tracks_sent {
                        self.tracks_sent = true;
                        return Ok(Some(Event::Tracks(self.tracks.clone())));
                    }
                }
                ID_EBML => self.skip(size)?,
                ID_TIMESTAMP_SCALE => self.scale_ns = self.read_uint(size)?.max(1),
                ID_TIMESTAMP => self.cluster = self.read_uint(size)? as i64,
                ID_TRACK_NUMBER | ID_TRACK_TYPE | ID_PIXEL_WIDTH | ID_PIXEL_HEIGHT
                | ID_CHANNELS => {
                    let v = self.read_uint(size)?;
                    if let Some(t) = self.tracks.last_mut() {
                        match id {
                            ID_TRACK_NUMBER => t.number = v,
                            ID_TRACK_TYPE => t.kind = v,
                            ID_PIXEL_WIDTH => t.width = v as u32,
                            ID_PIXEL_HEIGHT => t.height = v as u32,
                            _ => t.channels = v as u32,
                        }
                    }
                }
                ID_CODEC_ID => {
                    let b = self.read_body(size)?;
                    if let Some(t) = self.tracks.last_mut() {
                        t.codec = String::from_utf8_lossy(&b).trim_end_matches('\0').into();
                    }
                }
                ID_SAMPLING_FREQUENCY => {
                    let b = self.read_body(size)?;
                    let v = match b.len() {
                        4 => f32::from_be_bytes(b[..4].try_into().unwrap()) as f64,
                        8 => f64::from_be_bytes(b[..8].try_into().unwrap()),
                        _ => 0.0,
                    };
                    if let Some(t) = self.tracks.last_mut() {
                        t.rate = v;
                    }
                }
                ID_SIMPLE_BLOCK | ID_BLOCK => {
                    let b = self.read_body(size)?;
                    if let Some(e) = self.block(b) {
                        return Ok(Some(e));
                    }
                }
                _ => self.skip(size)?,
            }
        }
    }

    /// A (Simple)Block: track number, timestamp relative to the cluster's,
    /// flags, then the frame (RFC 9559 §10.1). Laced blocks (several frames
    /// in one) are not written by ffmpeg for these codecs, and are skipped.
    fn block(&self, mut b: Vec<u8>) -> Option<Event> {
        let len = vint_len(*b.first()?)?;
        if b.len() < len + 3 {
            return None;
        }
        let mut track = (b[0] as u64) & (0xFF >> len);
        for &x in &b[1..len] {
            track = (track << 8) | x as u64;
        }
        let rel = i16::from_be_bytes([b[len], b[len + 1]]) as i64;
        let flags = b[len + 2];
        if flags & 0x06 != 0 {
            return None;
        }
        let units = self.cluster + rel;
        let ms = (units as i128 * self.scale_ns as i128 / 1_000_000) as i64;
        b.drain(..len + 3);
        Some(Event::Block { track, ms, data: b })
    }

    /// An element ID (its length marker kept, as IDs are written); `None`
    /// at a clean end of stream.
    fn read_id(&mut self) -> io::Result<Option<u32>> {
        let mut first = [0u8];
        if self.r.read(&mut first)? == 0 {
            return Ok(None);
        }
        let len = vint_len(first[0])
            .filter(|&l| l <= 4)
            .ok_or_else(|| bad("element ID"))?;
        let mut id = first[0] as u32;
        for _ in 1..len {
            id = (id << 8) | self.byte()? as u32;
        }
        Ok(Some(id))
    }

    /// An element size; `u64::MAX` for "unknown" (all value bits set).
    fn read_size(&mut self) -> io::Result<u64> {
        let first = self.byte()?;
        let len = vint_len(first).ok_or_else(|| bad("element size"))?;
        let mut v = (first as u64) & (0xFF >> len);
        let mut all_ones = v == (0xFF >> len);
        for _ in 1..len {
            let b = self.byte()?;
            all_ones &= b == 0xFF;
            v = (v << 8) | b as u64;
        }
        Ok(if all_ones { u64::MAX } else { v })
    }

    fn byte(&mut self) -> io::Result<u8> {
        let mut b = [0u8];
        self.r.read_exact(&mut b)?;
        Ok(b[0])
    }

    fn read_body(&mut self, size: u64) -> io::Result<Vec<u8>> {
        if size > MAX_BODY {
            return Err(bad("element too large"));
        }
        let mut b = vec![0u8; size as usize];
        self.r.read_exact(&mut b)?;
        Ok(b)
    }

    fn read_uint(&mut self, size: u64) -> io::Result<u64> {
        if size > 8 {
            return Err(bad("integer too long"));
        }
        Ok(self
            .read_body(size)?
            .iter()
            .fold(0u64, |v, &b| (v << 8) | b as u64))
    }

    fn skip(&mut self, size: u64) -> io::Result<()> {
        if size == u64::MAX {
            return Ok(()); // unknown size: its children follow; read on
        }
        let n = io::copy(&mut (&mut self.r).take(size), &mut io::sink())?;
        if n < size {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(())
    }
}

/// The length of a variable-length integer from its first byte: one more
/// than its leading zero bits (1..=8); `None` for a zero byte.
fn vint_len(first: u8) -> Option<usize> {
    (first != 0).then(|| first.leading_zeros() as usize + 1)
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("Matroska: bad {what}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An element: ID bytes, an 8-byte size (as ffmpeg writes some), body.
    pub(crate) fn el(id: u32, body: &[u8]) -> Vec<u8> {
        let mut v: Vec<u8> = id
            .to_be_bytes()
            .into_iter()
            .skip_while(|&b| b == 0)
            .collect();
        v.push(0x01);
        v.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
        v.extend_from_slice(body);
        v
    }

    fn uint(id: u32, v: u64) -> Vec<u8> {
        el(id, &v.to_be_bytes())
    }

    /// A stream as ffmpeg writes it to a pipe: a video and an audio track,
    /// a cluster at 1000 ms with a frame at +40 and audio at -5.
    pub(crate) fn sample_stream() -> Vec<u8> {
        let mut s = el(ID_EBML, &uint(0x4282, 1)); // DocType stand-in
        // Segment of unknown size.
        s.extend_from_slice(&[
            0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        ]);
        s.extend(el(0x114D_9B74, &[0xEC, 0x81, 0x00])); // SeekHead, skipped
        s.extend(el(ID_INFO, &uint(ID_TIMESTAMP_SCALE, 1_000_000)));
        let video = [
            uint(ID_TRACK_NUMBER, 1),
            uint(ID_TRACK_TYPE, 1),
            el(ID_CODEC_ID, b"V_UNCOMPRESSED"),
            el(
                ID_VIDEO,
                &[uint(ID_PIXEL_WIDTH, 2), uint(ID_PIXEL_HEIGHT, 1)].concat(),
            ),
        ]
        .concat();
        let audio = [
            uint(ID_TRACK_NUMBER, 2),
            uint(ID_TRACK_TYPE, 2),
            el(ID_CODEC_ID, b"A_PCM/FLOAT/IEEE"),
            el(
                ID_AUDIO,
                &[
                    el(ID_SAMPLING_FREQUENCY, &48_000f64.to_be_bytes()),
                    uint(ID_CHANNELS, 2),
                ]
                .concat(),
            ),
        ]
        .concat();
        s.extend(el(
            ID_TRACKS,
            &[el(ID_TRACK_ENTRY, &video), el(ID_TRACK_ENTRY, &audio)].concat(),
        ));
        let mut frame = vec![0x81, 0x00, 40, 0x80]; // track 1, +40 ms, key
        frame.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let mut pcm = vec![0x82, 0xFF, 0xFB, 0x80]; // track 2, -5 ms
        pcm.extend_from_slice(&0.5f32.to_le_bytes());
        pcm.extend_from_slice(&(-0.5f32).to_le_bytes());
        let cluster = [
            el(0xBF, &[0, 0, 0, 0]), // CRC-32, skipped
            uint(ID_TIMESTAMP, 1000),
            el(ID_SIMPLE_BLOCK, &frame),
            el(ID_BLOCK_GROUP, &el(ID_BLOCK, &pcm)),
        ]
        .concat();
        s.extend(el(ID_CLUSTER, &cluster));
        s
    }

    #[test]
    fn reads_tracks_then_timed_blocks() {
        let s = sample_stream();
        let mut r = MkvReader::new(&s[..]);
        let Some(Event::Tracks(t)) = r.next_event().unwrap() else {
            panic!("tracks first");
        };
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].kind, t[0].width, t[0].height), (1, 2, 1));
        assert_eq!(t[0].codec, "V_UNCOMPRESSED");
        assert_eq!((t[1].kind, t[1].rate, t[1].channels), (2, 48_000.0, 2));
        assert_eq!(
            r.next_event().unwrap(),
            Some(Event::Block {
                track: 1,
                ms: 1040,
                data: vec![1, 2, 3, 4, 5, 6, 7, 8]
            })
        );
        let Some(Event::Block { track, ms, data }) = r.next_event().unwrap() else {
            panic!("the audio block");
        };
        assert_eq!((track, ms, data.len()), (2, 995, 8));
        assert_eq!(r.next_event().unwrap(), None);
    }

    #[test]
    fn a_cut_stream_is_an_error_not_a_hang() {
        let s = sample_stream();
        let mut r = MkvReader::new(&s[..s.len() - 3]);
        let mut n = 0;
        while let Ok(Some(_)) = r.next_event() {
            n += 1;
        }
        assert!(n >= 2); // the tracks and the frame
    }
}
