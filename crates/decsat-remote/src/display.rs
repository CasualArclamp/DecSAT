// made by CasualArclamp
//! Display data for smart mode: dB traces quantised to bytes and delta coded
//! against the last one sent, and constellation points as signed bytes. The
//! frame layer's zstd then takes out the redundancy the delta exposes.

use decsat_core::Iq;
use serde::{Deserialize, Serialize};

/// The waterfall's fixed scale, dB: −160 to +31.25 in 0.75 dB steps. A row
/// is 48 averaged FFTs, whose spread (about 0.6 dB) is below a step, so most
/// bytes repeat from row to row.
pub const ROW_FLOOR_DB: f32 = -160.0;
pub const ROW_STEP_DB: f32 = 0.75;
/// The spectrum trace's scale: the same range, finer steps and a floor that
/// follows the trace, since it is drawn as a line.
pub const SPECTRUM_STEP_DB: f32 = 0.25;

fn quantise(trace: &[f32], floor: f32, step: f32, out: &mut Vec<u8>) {
    out.clear();
    out.extend(
        trace
            .iter()
            .map(|&d| ((d - floor) / step).round().clamp(0.0, 255.0) as u8),
    );
}

fn expand(q: &[u8], floor: f32, step: f32) -> Vec<f32> {
    q.iter().map(|&v| floor + f32::from(v) * step).collect()
}

/// Encodes a run of dB traces, each against the last one encoded.
///
/// Encoding happens as a frame is handed to the socket, so a row thinned out
/// under congestion is never referred to.
#[derive(Default)]
pub struct TraceEncoder {
    last: Vec<u8>,
    q: Vec<u8>,
    /// The spectrum's current floor (unused for rows, whose floor is fixed).
    floor: f32,
}

impl TraceEncoder {
    /// Payload: `u64 seq`, `f32 floor`, `f32 step`, `u8 key`, the bytes.
    pub fn encode(&mut self, seq: u64, trace: &[f32], floor: f32, step: f32, out: &mut Vec<u8>) {
        quantise(trace, floor, step, &mut self.q);
        out.clear();
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&floor.to_le_bytes());
        out.extend_from_slice(&step.to_le_bytes());
        // A delta only means something on the same grid.
        let key = self.last.len() != self.q.len();
        out.push(u8::from(key));
        if key {
            out.extend_from_slice(&self.q);
        } else {
            out.extend(
                self.q
                    .iter()
                    .zip(&self.last)
                    .map(|(a, b)| a.wrapping_sub(*b)),
            );
        }
        std::mem::swap(&mut self.last, &mut self.q);
    }

    /// A spectrum trace, on a floor taken from the trace itself. The floor
    /// moves in 16 dB jumps so deltas usually stay valid.
    pub fn encode_spectrum(&mut self, seq: u64, trace: &[f32], out: &mut Vec<u8>) {
        let low = trace.iter().copied().fold(f32::INFINITY, f32::min);
        let floor = if low.is_finite() {
            (low / 16.0).floor() * 16.0
        } else {
            ROW_FLOOR_DB
        };
        if floor != self.floor {
            self.last.clear();
            self.floor = floor;
        }
        self.encode(seq, trace, floor, SPECTRUM_STEP_DB, out);
    }
}

/// The matching decoder.
#[derive(Default)]
pub struct TraceDecoder {
    last: Vec<u8>,
}

impl TraceDecoder {
    /// A trace and its sequence number; `None` if malformed or if a delta
    /// has nothing to apply to.
    pub fn decode(&mut self, p: &[u8]) -> Option<(u64, Vec<f32>)> {
        if p.len() < 17 {
            return None;
        }
        let seq = u64::from_le_bytes(p[..8].try_into().ok()?);
        let floor = f32::from_le_bytes(p[8..12].try_into().ok()?);
        let step = f32::from_le_bytes(p[12..16].try_into().ok()?);
        let body = &p[17..];
        if p[16] != 0 {
            self.last.clear();
            self.last.extend_from_slice(body);
        } else {
            if self.last.len() != body.len() {
                return None;
            }
            for (l, d) in self.last.iter_mut().zip(body) {
                *l = l.wrapping_add(*d);
            }
        }
        Some((seq, expand(&self.last, floor, step)))
    }
}

/// A one-off dB trace (a VFO's own spectrum) inside a status message.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DbTrace {
    pub floor: f32,
    pub step: f32,
    #[serde(with = "serde_bytes_like")]
    pub bytes: Vec<u8>,
}

impl DbTrace {
    pub fn of(trace: &[f32]) -> DbTrace {
        let low = trace.iter().copied().fold(f32::INFINITY, f32::min);
        let high = trace.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        if !low.is_finite() || !high.is_finite() {
            return DbTrace::default();
        }
        let step = ((high - low) / 255.0).max(0.1);
        let mut bytes = Vec::new();
        quantise(trace, low, step, &mut bytes);
        DbTrace {
            floor: low,
            step,
            bytes,
        }
    }

    pub fn expand(&self) -> Vec<f32> {
        expand(&self.bytes, self.floor, self.step)
    }
}

/// Constellation points as signed bytes against a common scale, thinned to
/// at most `max` points spread evenly over the block.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Scatter {
    pub scale: f32,
    #[serde(with = "serde_bytes_like")]
    pub points: Vec<u8>,
}

impl Scatter {
    pub fn of(points: &[Iq], max: usize) -> Scatter {
        if points.is_empty() || max == 0 {
            return Scatter::default();
        }
        let step = points.len().div_ceil(max).max(1);
        let peak = points
            .iter()
            .step_by(step)
            .map(|p| p.re.abs().max(p.im.abs()))
            .fold(0f32, f32::max);
        if peak <= 0.0 || !peak.is_finite() {
            return Scatter::default();
        }
        let g = 127.0 / peak;
        let mut out = Vec::with_capacity(points.len() / step * 2 + 2);
        for p in points.iter().step_by(step) {
            out.push((p.re * g).round() as i8 as u8);
            out.push((p.im * g).round() as i8 as u8);
        }
        Scatter {
            scale: peak,
            points: out,
        }
    }

    pub fn expand(&self) -> Vec<Iq> {
        let g = self.scale / 127.0;
        self.points
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| Iq::new(f32::from(c[0] as i8) * g, f32::from(c[1] as i8) * g))
            .collect()
    }
}

/// Postcard writes `Vec<u8>` as a length and raw bytes, but serde's default
/// visits a sequence one element at a time; `serialize_bytes` is the same
/// encoding without the per-element work.
mod serde_bytes_like {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        // Rust note: `&[u8]` borrows from the input; postcard can hand it
        // back without copying, and `to_vec` makes the one copy we need.
        Ok(<&'de [u8]>::deserialize(d)?.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: &[f32], b: &[f32], tol: f32) {
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b) {
            assert!((x - y).abs() <= tol, "{x} vs {y}");
        }
    }

    fn row(i: u32, n: usize) -> Vec<f32> {
        // Noise of about ±0.5 dB on a flat floor, a carrier in the middle.
        (0..n as u32)
            .map(|k| {
                let h = k
                    .wrapping_mul(2654435761)
                    .wrapping_add(i.wrapping_mul(40503))
                    >> 24;
                let c = if (n as u32 / 3..n as u32 / 2).contains(&k) {
                    30.0
                } else {
                    0.0
                };
                -100.0 + c + (h as f32 / 255.0 - 0.5)
            })
            .collect()
    }

    #[test]
    fn rows_round_trip_within_half_a_step() {
        let (mut e, mut d, mut p) = (TraceEncoder::default(), TraceDecoder::default(), Vec::new());
        for i in 0..20 {
            let r = row(i, 4096);
            e.encode(i as u64, &r, ROW_FLOOR_DB, ROW_STEP_DB, &mut p);
            let (seq, got) = d.decode(&p).unwrap();
            assert_eq!(seq, i as u64);
            near(&r, &got, ROW_STEP_DB / 2.0 + 1e-3);
        }
    }

    /// The saving the whole scheme is for: measured on the synthetic rows
    /// above, against 16 kB of f32.
    #[test]
    fn delta_rows_compress_well() {
        let (mut e, mut p) = (TraceEncoder::default(), Vec::new());
        e.encode(0, &row(0, 4096), ROW_FLOOR_DB, ROW_STEP_DB, &mut p);
        e.encode(1, &row(1, 4096), ROW_FLOOR_DB, ROW_STEP_DB, &mut p);
        let z = zstd::bulk::compress(&p, 1).unwrap();
        assert!(z.len() < 1600, "{} bytes", z.len());
    }

    #[test]
    fn spectrum_floor_moves_force_a_key_trace() {
        let (mut e, mut d, mut p) = (TraceEncoder::default(), TraceDecoder::default(), Vec::new());
        let a = vec![-70.0f32; 64];
        let b = vec![-20.0f32; 64];
        for (i, t) in [&a, &a, &b, &b].iter().enumerate() {
            e.encode_spectrum(i as u64, t, &mut p);
            near(t, &d.decode(&p).unwrap().1, SPECTRUM_STEP_DB);
        }
    }

    #[test]
    fn a_delta_without_a_base_is_refused() {
        let (mut e, mut p) = (TraceEncoder::default(), Vec::new());
        let r = row(0, 16);
        e.encode(0, &r, ROW_FLOOR_DB, ROW_STEP_DB, &mut p);
        e.encode(1, &r, ROW_FLOOR_DB, ROW_STEP_DB, &mut p);
        assert!(TraceDecoder::default().decode(&p).is_none());
    }

    #[test]
    fn scatter_thins_and_scales() {
        let pts: Vec<Iq> = (0..4000)
            .map(|i| Iq::from_polar(0.7, i as f32 * 0.1))
            .collect();
        let s = Scatter::of(&pts, 500);
        let back = s.expand();
        assert!(back.len() <= 500 && back.len() >= 400);
        for p in &back {
            assert!((p.norm() - 0.7).abs() < 0.02);
        }
    }
}
