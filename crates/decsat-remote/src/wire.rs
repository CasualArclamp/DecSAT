// made by CasualArclamp
//! Frames on the TCP stream: `[u32 length][u8 kind][u8 flags][payload]`, all
//! little-endian, the length counting the kind, the flags and the payload.
//! The layout is set out in `docs/REMOTE.md`.

use std::io::{self, Read, Write};

/// Sent first by the client: the protocol's name and generation.
pub const MAGIC: [u8; 4] = *b"DSR1";
/// Bumped whenever a message or a status structure changes shape.
pub const PROTO: u16 = 1;
/// The largest frame either end accepts.
pub const MAX_FRAME: usize = 8 << 20;
/// Flag bit: the payload is one zstd frame.
pub const FLAG_ZSTD: u8 = 1;

/// What a frame's payload is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// A postcard-encoded `C2S` or `S2C` message.
    Control = 0,
    /// Packed IQ ([`crate::iq`]).
    Iq = 1,
    /// A waterfall row ([`crate::display`]).
    Row = 2,
    /// The smoothed wideband spectrum ([`crate::display`]).
    Spectrum = 3,
    /// Postcard-encoded `FrontWire`.
    Front = 4,
    /// Postcard-encoded `VfoWire`.
    Vfo = 5,
    /// Transport stream ([`crate::ts`]).
    Ts = 6,
}

impl Kind {
    pub fn from_u8(v: u8) -> Option<Kind> {
        Some(match v {
            0 => Kind::Control,
            1 => Kind::Iq,
            2 => Kind::Row,
            3 => Kind::Spectrum,
            4 => Kind::Front,
            5 => Kind::Vfo,
            6 => Kind::Ts,
            _ => return None,
        })
    }
}

/// Write one frame. Compresses with zstd at `level` (0 = never) when that
/// makes the payload smaller. Returns the bytes put on the wire.
pub fn write_frame(
    w: &mut impl Write,
    kind: Kind,
    payload: &[u8],
    level: i32,
    scratch: &mut Vec<u8>,
) -> io::Result<usize> {
    let mut flags = 0;
    let mut body = payload;
    if level != 0 && payload.len() > 64 {
        scratch.clear();
        // Rust note: `zstd::bulk` compresses one buffer in one call; the
        // `compress_to_buffer` form reuses `scratch` rather than allocating.
        scratch.reserve(zstd::zstd_safe::compress_bound(payload.len()));
        let cap = scratch.capacity();
        scratch.resize(cap, 0);
        if let Ok(n) = zstd::bulk::compress_to_buffer(payload, &mut scratch[..], level)
            && n < payload.len()
        {
            flags |= FLAG_ZSTD;
            body = &scratch[..n];
        }
    }
    let len = body.len() + 2;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too long",
        ));
    }
    w.write_all(&(len as u32).to_le_bytes())?;
    w.write_all(&[kind as u8, flags])?;
    w.write_all(body)?;
    Ok(len + 4)
}

/// Read one frame into `out`, decompressed. Returns its kind and the bytes
/// it took on the wire.
pub fn read_frame(
    r: &mut impl Read,
    out: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
) -> io::Result<(Kind, usize)> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    let mut head = [0u8; 6];
    r.read_exact(&mut head)?;
    let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
    if !(2..=MAX_FRAME).contains(&len) {
        return Err(bad("frame length out of range"));
    }
    let kind = Kind::from_u8(head[4]).ok_or_else(|| bad("unknown frame kind"))?;
    let flags = head[5];
    let body = len - 2;
    if flags & FLAG_ZSTD != 0 {
        scratch.resize(body, 0);
        r.read_exact(scratch)?;
        out.clear();
        // A frame's content size is always written by `compress_to_buffer`;
        // refusing anything larger than MAX_FRAME keeps a hostile peer from
        // asking for a huge allocation.
        let size = zstd::zstd_safe::get_frame_content_size(scratch)
            .ok()
            .flatten()
            .ok_or_else(|| bad("zstd frame without a size"))? as usize;
        if size > MAX_FRAME {
            return Err(bad("frame expands too far"));
        }
        out.resize(size, 0);
        let n = zstd::bulk::decompress_to_buffer(scratch, &mut out[..])?;
        out.truncate(n);
    } else {
        out.resize(body, 0);
        r.read_exact(out)?;
    }
    Ok((kind, len + 4))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_compressed_or_not() {
        let mut wire = Vec::new();
        let mut s = Vec::new();
        let compressible = vec![7u8; 10_000];
        let noise: Vec<u8> = (0..1000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
            .collect();
        write_frame(&mut wire, Kind::Row, &compressible, 1, &mut s).unwrap();
        write_frame(&mut wire, Kind::Ts, &noise, 1, &mut s).unwrap();
        write_frame(&mut wire, Kind::Control, b"hi", 0, &mut s).unwrap();
        assert!(wire.len() < 2000, "the run of sevens compresses");

        let mut r = &wire[..];
        let (mut o, mut t) = (Vec::new(), Vec::new());
        assert_eq!(read_frame(&mut r, &mut o, &mut t).unwrap().0, Kind::Row);
        assert_eq!(o, compressible);
        assert_eq!(read_frame(&mut r, &mut o, &mut t).unwrap().0, Kind::Ts);
        assert_eq!(o, noise);
        assert_eq!(read_frame(&mut r, &mut o, &mut t).unwrap().0, Kind::Control);
        assert_eq!(o, b"hi");
    }

    #[test]
    fn a_silly_length_is_refused() {
        let wire = [0xffu8, 0xff, 0xff, 0x7f, 0, 0];
        let (mut o, mut t) = (Vec::new(), Vec::new());
        assert!(read_frame(&mut &wire[..], &mut o, &mut t).is_err());
    }
}
