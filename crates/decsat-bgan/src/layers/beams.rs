// made by CasualArclamp
//! Spot-beam IDs and the places they cover.
//!
//! Port of `bgan/beams.py` from DecBGAN (Python, same author,
//! GPL-3.0-or-later), minus its file I/O: reading `beams.json` /
//! `beams.local.json` and writing the observation log is the caller's job; this
//! module parses the JSON text it is given ([`BeamTable::from_json`]) and keeps
//! observations in memory ([`record`]).
//!
//! A BulletinBoard names its spot beam by number only. Two ways to a place:
//!
//! **From the broadcast.** TS 102 744-3-1 clause 5.4.10 defines a SpotBeamMap
//! AVP carrying, per beam, a polygon of SpotbeamVertex values,
//! `SpotBeamVertexValue = 360(lat + 90) + lon + 180` (clause 5.4.10.3.4).
//! The measured record is 15 octets, `<beam-id:8> 0x0c 0x06 <6 x SpotbeamVertex>`
//! (0x0c = vertex-list octets, 0x06 = vertex count), see [`spot_beam_records`].
//! Only the serving beam and its six neighbours are sent, so a long capture is
//! needed.
//!
//! **From observation.** A table of beam id -> place name, each entry labelled
//! with where it came from; an unknown beam is reported as unknown, never
//! guessed.

use std::collections::BTreeMap;

/// SpotbeamVertex, clause 5.4.10.3.4. Values at or above this are reserved.
pub const VERTEX_RESERVED: u32 = 0xFE88;

/// SpotbeamVertex -> `(lat, lon)` in degrees, or `None` if reserved.
pub fn vertex(v: u32) -> Option<(i32, i32)> {
    if v >= VERTEX_RESERVED {
        return None;
    }
    Some(((v / 360) as i32 - 90, (v % 360) as i32 - 180))
}

/// Vertices per record (a spot beam is a hexagon).
pub const MAP_NVERT: usize = 6;
/// `0x0c 0x06`: vertex-list length in octets and vertex count.
pub const MAP_HDR: [u8; 2] = [2 * MAP_NVERT as u8, MAP_NVERT as u8];
/// 1 + 2 + 12 octets.
pub const MAP_LEN: usize = 1 + MAP_HDR.len() + 2 * MAP_NVERT;

/// `(lat, lon)` in whole degrees.
pub type Point = (i32, i32);

/// A run of consecutive 16-bit values decoding to a tight cluster: the shape a
/// SpotBeamMap polygon would take (exploratory; superseded by
/// [`spot_beam_records`], which keys on the record header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VertexRun {
    pub offset: usize,
    pub points: Vec<Point>,
}

/// Exploratory search that found the record format. Validity alone filters
/// nothing (65160 of 65536 values decode); clustering is the filter, and a
/// constant run is excluded. Returns many false runs on real payload, use
/// [`spot_beam_records`].
pub fn vertex_runs(
    payload: &[u8],
    minrun: usize,
    boxdeg: f64,
    min_distinct: usize,
) -> Vec<VertexRun> {
    let mut out = Vec::new();
    for align in 0..2usize {
        if payload.len() < align {
            continue;
        }
        let b = &payload[align..];
        let n = (b.len() / 2) * 2;
        if n < 2 * minrun {
            continue;
        }
        let v: Vec<u32> = b[..n]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| ((c[0] as u32) << 8) | c[1] as u32)
            .collect();
        let lat: Vec<i32> = v.iter().map(|&x| (x / 360) as i32 - 90).collect();
        let lon: Vec<i32> = v.iter().map(|&x| (x % 360) as i32 - 180).collect();
        let ok: Vec<bool> = v.iter().map(|&x| x < VERTEX_RESERVED).collect();
        // Segment boundaries: wherever neighbours are not "near".
        let near = |k: usize| {
            ((lat[k + 1] - lat[k]).abs() as f64) <= boxdeg
                && ((lon[k + 1] - lon[k]).abs() as f64) <= boxdeg
                && ok[k]
                && ok[k + 1]
        };
        let mut starts = vec![0usize];
        let mut ends = Vec::new();
        for k in 0..v.len().saturating_sub(1) {
            if !near(k) {
                ends.push(k + 1);
                starts.push(k + 1);
            }
        }
        ends.push(v.len());
        for (&s, &e) in starts.iter().zip(&ends) {
            if e - s < minrun {
                continue;
            }
            let mut u: Vec<u32> = v[s..e].to_vec();
            u.sort_unstable();
            u.dedup();
            if u.len() < min_distinct {
                continue;
            }
            let (la, lo) = (&lat[s..e], &lon[s..e]);
            let rng = |x: &[i32]| (x.iter().max().unwrap() - x.iter().min().unwrap()) as f64;
            if rng(la) > boxdeg || rng(lo) > boxdeg {
                continue;
            }
            out.push(VertexRun {
                offset: align + 2 * s,
                points: la.iter().copied().zip(lo.iter().copied()).collect(),
            });
        }
    }
    out
}

/// One SpotBeamMap record found in a payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeamRecord {
    pub offset: usize,
    pub beam_id: u8,
    pub polygon: Vec<Point>,
}

/// Every SpotBeamMap record. Three independent conditions hold at once: the
/// two header octets, six legal SpotbeamVertex values, and a cluster no wider
/// than `boxdeg` degrees. Measured on a 25.6 MB payload: 14 records, 7 beams
/// twice each with identical geometry, against 0 on random bytes.
/// (Python defaults: `boxdeg` 25.0, `min_distinct` 4.)
pub fn spot_beam_records_with(payload: &[u8], boxdeg: f64, min_distinct: usize) -> Vec<BeamRecord> {
    if payload.len() < MAP_LEN {
        return Vec::new();
    }
    (0..=payload.len() - MAP_LEN)
        .filter_map(|i| record_at(payload, i, boxdeg, min_distinct))
        .collect()
}

/// The record starting at `payload[i]` (needs `MAP_LEN` bytes from there).
fn record_at(payload: &[u8], i: usize, boxdeg: f64, min_distinct: usize) -> Option<BeamRecord> {
    if payload[i + 1] != MAP_HDR[0] || payload[i + 2] != MAP_HDR[1] {
        return None;
    }
    let v: Vec<u32> = payload[i + 3..i + MAP_LEN]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| ((c[0] as u32) << 8) | c[1] as u32)
        .collect();
    let pts: Vec<Point> = v.iter().map(|&x| vertex(x)).collect::<Option<_>>()?;
    let mut u = v;
    u.sort_unstable();
    u.dedup();
    if u.len() < min_distinct {
        return None;
    }
    let rng = |f: fn(&Point) -> i32| {
        (pts.iter().map(f).max().unwrap() - pts.iter().map(f).min().unwrap()) as f64
    };
    if rng(|p| p.0) > boxdeg || rng(|p| p.1) > boxdeg {
        return None;
    }
    Some(BeamRecord {
        offset: i,
        beam_id: payload[i],
        polygon: pts,
    })
}

/// [`spot_beam_records_with`] using the Python defaults.
pub fn spot_beam_records(payload: &[u8]) -> Vec<BeamRecord> {
    spot_beam_records_with(payload, 25.0, 4)
}

/// `{beam_id: polygon}` from the broadcast map, the majority shape per beam
/// (first seen wins a tie).
pub fn spot_beam_map(payload: &[u8]) -> BTreeMap<u8, Vec<Point>> {
    beam_map_from(&spot_beam_records(payload))
}

/// [`spot_beam_map`] over records already found (e.g. incrementally).
pub fn beam_map_from(records: &[BeamRecord]) -> BTreeMap<u8, Vec<Point>> {
    let mut seen: BTreeMap<u8, Vec<(Vec<Point>, usize)>> = BTreeMap::new();
    for r in records {
        let shapes = seen.entry(r.beam_id).or_default();
        match shapes.iter_mut().find(|(p, _)| *p == r.polygon) {
            Some(s) => s.1 += 1,
            None => shapes.push((r.polygon.clone(), 1)),
        }
    }
    seen.into_iter()
        .map(|(b, shapes)| {
            let mut best = &shapes[0];
            for s in &shapes {
                if s.1 > best.1 {
                    best = s;
                }
            }
            (b, best.0.clone())
        })
        .collect()
}

/// Is `(lat, lon)` inside `polygon`? Ray casting. Vertices are whole degrees,
/// so this is coarse by construction: a point within a degree of an edge is
/// undecided rather than answered.
pub fn contains(polygon: &[Point], lat: f64, lon: f64) -> bool {
    let mut inside = false;
    let n = polygon.len();
    for i in 0..n {
        let (y0, x0) = (polygon[i].0 as f64, polygon[i].1 as f64);
        let (y1, x1) = (polygon[(i + 1) % n].0 as f64, polygon[(i + 1) % n].1 as f64);
        if (y0 > lat) != (y1 > lat) && lon < x0 + (lat - y0) * (x1 - x0) / (y1 - y0) {
            inside = !inside;
        }
    }
    inside
}

/// Beam ids whose broadcast polygon contains `(lat, lon)`.
pub fn locate(lat: f64, lon: f64, map: &BTreeMap<u8, Vec<Point>>) -> Vec<u8> {
    map.iter()
        .filter(|(_, p)| contains(p, lat, lon))
        .map(|(&b, _)| b)
        .collect()
}

/// Incremental [`spot_beam_records`]. A 15-octet record is decided as soon as
/// its last octet arrives; only the last 14 octets are ever retained.
#[derive(Debug, Default)]
pub struct BeamScanner {
    buf: Vec<u8>,
    /// Absolute offset of `buf[0]`.
    base: usize,
    records: Vec<BeamRecord>,
}

impl BeamScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns new records (absolute offsets).
    pub fn push(&mut self, bytes: &[u8]) -> Vec<BeamRecord> {
        self.buf.extend_from_slice(bytes);
        let mut new = Vec::new();
        if self.buf.len() >= MAP_LEN {
            let last = self.buf.len() - MAP_LEN;
            for i in 0..=last {
                if let Some(mut r) = record_at(&self.buf, i, 25.0, 4) {
                    r.offset += self.base;
                    new.push(r);
                }
            }
            // positions 0..=last are decided; keep the undecided tail
            self.buf.drain(..=last);
            self.base += last + 1;
        }
        self.records.extend(new.iter().cloned());
        new
    }

    /// The broadcast map from the records so far.
    pub fn map(&self) -> BTreeMap<u8, Vec<Point>> {
        beam_map_from(&self.records)
    }
}

// --- the table --------------------------------------------------------------

/// One beam table entry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BeamEntry {
    pub name: Option<String>,
    pub source: Option<String>,
    pub note: Option<String>,
}

/// `{beam_id: entry}`, from `beams.json` and optionally `beams.local.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BeamTable {
    pub entries: BTreeMap<u32, BeamEntry>,
}

impl BeamTable {
    /// Parse one table file's text. Empty if malformed (like Python's `_read`).
    pub fn from_json(text: &str) -> BeamTable {
        let mut t = BeamTable::default();
        let Some(Json::Obj(top)) = Json::parse(text) else {
            return t;
        };
        let Some(Json::Obj(beams)) = top.into_iter().find(|(k, _)| k == "beams").map(|(_, v)| v)
        else {
            return t;
        };
        for (k, v) in beams {
            if k.is_empty() || !k.bytes().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let Ok(id) = k.parse::<u32>() else { continue };
            let field = |name: &str| match &v {
                Json::Obj(o) => o
                    .iter()
                    .find(|(n, _)| n == name)
                    .and_then(|(_, x)| match x {
                        Json::Str(s) => Some(s.clone()),
                        _ => None,
                    }),
                _ => None,
            };
            t.entries.insert(
                id,
                BeamEntry {
                    name: field("name"),
                    source: field("source"),
                    note: field("note"),
                },
            );
        }
        t
    }

    /// Overlay your own entries (`beams.local.json`): they win on a clash, and
    /// a missing `source` becomes "local".
    pub fn merge_local(&mut self, local: BeamTable) {
        for (id, mut e) in local.entries {
            e.source.get_or_insert_with(|| "local".to_string());
            self.entries.insert(id, e);
        }
    }

    /// Place name for a beam, or `None`. Never guesses.
    pub fn name(&self, beam_id: u32) -> Option<&str> {
        self.entries.get(&beam_id).and_then(|e| e.name.as_deref())
    }

    /// `134 (Brisbane)` or `122 (unmapped)`; "none" for no beam at all.
    pub fn describe(&self, beam_id: Option<u32>) -> String {
        match beam_id {
            None => "none".into(),
            Some(b) => match self.name(b) {
                Some(n) => format!("{b} ({n})"),
                None => format!("{b} (unmapped)"),
            },
        }
    }
}

// --- observations -----------------------------------------------------------

/// One sighting of a beam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub beam: u32,
    pub freq_hz: Option<u64>,
    /// Capture file base name.
    pub capture: Option<String>,
    pub rnc_id: Option<i64>,
    pub bct_id: Option<i64>,
    /// Timestamp text supplied by the caller (Python used UTC ISO seconds).
    pub seen: String,
}

fn basename(p: &str) -> &str {
    p.rsplit(['/', '\\']).next().unwrap_or(p)
}

/// Append one sighting, keyed by (beam, freq, capture base name) so re-decoding
/// the same file updates rather than duplicates. A zero frequency counts as
/// none, as in Python.
pub fn record(
    obs: &mut Vec<Observation>,
    beam_id: Option<u32>,
    freq_hz: Option<u64>,
    capture: Option<&str>,
    rnc_id: Option<i64>,
    bct_id: Option<i64>,
    seen: &str,
) {
    let Some(beam) = beam_id else { return };
    let freq = freq_hz.filter(|&f| f != 0);
    let cap = capture
        .map(basename)
        .filter(|c| !c.is_empty())
        .map(str::to_string);
    obs.retain(|o| {
        !(o.beam == beam
            && o.freq_hz == freq_hz
            && o.capture.as_deref().unwrap_or("") == cap.as_deref().unwrap_or(""))
    });
    obs.push(Observation {
        beam,
        freq_hz: freq,
        capture: cap,
        rnc_id,
        bct_id,
        seen: seen.to_string(),
    });
}

/// Summary of all sightings of one beam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeamSummary {
    pub name: Option<String>,
    pub freqs: Vec<u64>,
    pub captures: usize,
}

/// `{beam: {name, freqs, captures}}` over all sightings.
pub fn by_beam(obs: &[Observation], table: &BeamTable) -> BTreeMap<u32, BeamSummary> {
    let mut out: BTreeMap<u32, BeamSummary> = BTreeMap::new();
    for o in obs {
        let e = out.entry(o.beam).or_insert_with(|| BeamSummary {
            name: table.name(o.beam).map(str::to_string),
            freqs: Vec::new(),
            captures: 0,
        });
        if let Some(f) = o.freq_hz.filter(|&f| f != 0)
            && !e.freqs.contains(&f)
        {
            e.freqs.push(f);
        }
        e.captures += 1;
    }
    for e in out.values_mut() {
        e.freqs.sort_unstable();
    }
    out
}

// --- a tiny JSON reader (enough for beams.json) -------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    /// Insertion-ordered object (duplicate keys keep every pair; lookups take the first).
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn parse(s: &str) -> Option<Json> {
        let b = s.as_bytes();
        let mut i = 0;
        let v = Self::value(b, &mut i, 0)?;
        Self::ws(b, &mut i);
        (i == b.len()).then_some(v)
    }

    fn ws(b: &[u8], i: &mut usize) {
        while *i < b.len() && matches!(b[*i], b' ' | b'\t' | b'\n' | b'\r') {
            *i += 1;
        }
    }

    fn value(b: &[u8], i: &mut usize, depth: usize) -> Option<Json> {
        if depth > 64 {
            return None;
        }
        Self::ws(b, i);
        match *b.get(*i)? {
            b'{' => {
                *i += 1;
                let mut o = Vec::new();
                Self::ws(b, i);
                if b.get(*i) == Some(&b'}') {
                    *i += 1;
                    return Some(Json::Obj(o));
                }
                loop {
                    Self::ws(b, i);
                    let k = Self::string(b, i)?;
                    Self::ws(b, i);
                    if b.get(*i) != Some(&b':') {
                        return None;
                    }
                    *i += 1;
                    let v = Self::value(b, i, depth + 1)?;
                    // JSON objects keep the last duplicate (Python dict semantics)
                    o.retain(|(n, _): &(String, Json)| *n != k);
                    o.push((k, v));
                    Self::ws(b, i);
                    match b.get(*i)? {
                        b',' => *i += 1,
                        b'}' => {
                            *i += 1;
                            return Some(Json::Obj(o));
                        }
                        _ => return None,
                    }
                }
            }
            b'[' => {
                *i += 1;
                let mut a = Vec::new();
                Self::ws(b, i);
                if b.get(*i) == Some(&b']') {
                    *i += 1;
                    return Some(Json::Arr(a));
                }
                loop {
                    a.push(Self::value(b, i, depth + 1)?);
                    Self::ws(b, i);
                    match b.get(*i)? {
                        b',' => *i += 1,
                        b']' => {
                            *i += 1;
                            return Some(Json::Arr(a));
                        }
                        _ => return None,
                    }
                }
            }
            b'"' => Some(Json::Str(Self::string(b, i)?)),
            b't' if b[*i..].starts_with(b"true") => {
                *i += 4;
                Some(Json::Bool(true))
            }
            b'f' if b[*i..].starts_with(b"false") => {
                *i += 5;
                Some(Json::Bool(false))
            }
            b'n' if b[*i..].starts_with(b"null") => {
                *i += 4;
                Some(Json::Null)
            }
            _ => {
                let st = *i;
                while *i < b.len()
                    && matches!(b[*i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                {
                    *i += 1;
                }
                std::str::from_utf8(&b[st..*i])
                    .ok()?
                    .parse()
                    .ok()
                    .map(Json::Num)
            }
        }
    }

    fn string(b: &[u8], i: &mut usize) -> Option<String> {
        if b.get(*i) != Some(&b'"') {
            return None;
        }
        *i += 1;
        let mut out: Vec<u16> = Vec::new();
        let mut run_start = *i;
        let flush = |out: &mut Vec<u16>, from: usize, to: usize| -> Option<()> {
            out.extend(std::str::from_utf8(&b[from..to]).ok()?.encode_utf16());
            Some(())
        };
        loop {
            match *b.get(*i)? {
                b'"' => {
                    flush(&mut out, run_start, *i)?;
                    *i += 1;
                    return String::from_utf16(&out).ok();
                }
                b'\\' => {
                    flush(&mut out, run_start, *i)?;
                    *i += 1;
                    let c = *b.get(*i)?;
                    *i += 1;
                    match c {
                        b'"' => out.push(b'"' as u16),
                        b'\\' => out.push(b'\\' as u16),
                        b'/' => out.push(b'/' as u16),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(10),
                        b'r' => out.push(13),
                        b't' => out.push(9),
                        b'u' => {
                            let h = std::str::from_utf8(b.get(*i..*i + 4)?).ok()?;
                            out.push(u16::from_str_radix(h, 16).ok()?);
                            *i += 4;
                        }
                        _ => return None,
                    }
                    run_start = *i;
                }
                c if c < 0x20 => return None,
                _ => *i += 1,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(beam: u8, pts: [(i32, i32); 6]) -> Vec<u8> {
        let mut v = vec![beam, 0x0c, 0x06];
        for (lat, lon) in pts {
            let x = (360 * (lat + 90) + lon + 180) as u16;
            v.extend(x.to_be_bytes());
        }
        v
    }

    const HEX: [(i32, i32); 6] = [
        (-27, 152),
        (-26, 153),
        (-26, 154),
        (-27, 155),
        (-28, 154),
        (-28, 153),
    ];

    #[test]
    fn vertex_decode() {
        assert_eq!(vertex(0), Some((-90, -180)));
        assert_eq!(vertex(360 * 63 + 147 + 180), Some((-27, 147)));
        assert_eq!(vertex(VERTEX_RESERVED), None);
    }

    #[test]
    fn records_map_and_point_in_polygon() {
        let mut blob = vec![0u8; 40];
        blob.extend(rec(134, HEX));
        blob.extend([0xAAu8; 10]);
        blob.extend(rec(134, HEX));
        let mut other = HEX;
        other[0].1 = 100; // too wide a cluster for beam 7: rejected
        blob.extend(rec(7, other));
        blob.extend(rec(8, [(-27, 152); 6])); // constant run: rejected
        let r = spot_beam_records(&blob);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].offset, 40);
        let m = spot_beam_map(&blob);
        assert_eq!(m.len(), 1);
        assert_eq!(m[&134], HEX.to_vec());
        assert_eq!(locate(-27.0, 153.5, &m), vec![134]);
        assert!(locate(-10.0, 153.5, &m).is_empty());

        let mut sc = BeamScanner::new();
        let mut got = Vec::new();
        for ch in blob.chunks(7) {
            got.extend(sc.push(ch));
        }
        assert_eq!(got, r);
        assert_eq!(sc.map(), m);
    }

    #[test]
    fn table_json_and_describe() {
        let shared = r#"{"_comment": ["x"], "beams": {"134": {"name": "Brisbane", "source": "broadcast", "note": "n\u00e9"}, "bad": {}, "12": {"name": "A"}}}"#;
        let local =
            r#"{"beams": {"12": {"name": "Mine"}, "99": {"name": "Z", "source": "observed"}}}"#;
        let mut t = BeamTable::from_json(shared);
        assert_eq!(t.entries.len(), 2);
        assert_eq!(t.entries[&134].note.as_deref(), Some("né"));
        t.merge_local(BeamTable::from_json(local));
        assert_eq!(t.name(12), Some("Mine"));
        assert_eq!(t.entries[&12].source.as_deref(), Some("local"));
        assert_eq!(t.entries[&99].source.as_deref(), Some("observed"));
        assert_eq!(t.describe(Some(134)), "134 (Brisbane)");
        assert_eq!(t.describe(Some(122)), "122 (unmapped)");
        assert_eq!(t.describe(None), "none");
        assert!(BeamTable::from_json("not json").entries.is_empty());
    }

    #[test]
    fn observations_replace_by_key() {
        let mut obs = Vec::new();
        record(
            &mut obs,
            Some(134),
            Some(1_534_500_000),
            Some("C:/x/cap.wav"),
            Some(29),
            Some(1),
            "t1",
        );
        record(
            &mut obs,
            Some(134),
            Some(1_534_500_000),
            Some("D:\\y\\cap.wav"),
            Some(30),
            Some(1),
            "t2",
        );
        record(
            &mut obs,
            Some(134),
            Some(1_535_900_000),
            Some("other.wav"),
            None,
            None,
            "t3",
        );
        record(&mut obs, None, None, None, None, None, "t4");
        assert_eq!(obs.len(), 2);
        let mut t = BeamTable::default();
        t.entries.insert(
            134,
            BeamEntry {
                name: Some("Brisbane".into()),
                ..Default::default()
            },
        );
        let s = by_beam(&obs, &t);
        assert_eq!(s[&134].captures, 2);
        assert_eq!(s[&134].freqs, vec![1_534_500_000, 1_535_900_000]);
        assert_eq!(s[&134].name.as_deref(), Some("Brisbane"));
    }

    #[test]
    fn runs_exploratory() {
        let mut blob = Vec::new();
        for (lat, lon) in HEX.iter().cycle().take(8) {
            blob.extend(((360 * (lat + 90) + lon + 180) as u16).to_be_bytes());
        }
        let r = vertex_runs(&blob, 5, 25.0, 4);
        assert!(!r.is_empty());
        assert_eq!(r[0].offset, 0);
    }
}
