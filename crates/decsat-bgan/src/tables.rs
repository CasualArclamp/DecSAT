// made by CasualArclamp
//! Loaders for the ETSI TS 102 744-2-1 Annex C.1 / C.2 tables.
//!
//! Annex C.1 (`*_TCI.TXT`): turbo code interleaver permutation.
//! Annex C.2 (`*_CIPM.TXT`): channel interleaving, puncturing, 16-QAM mapping.
//!
//! The tables are ETSI copyright and are **not** in this repository: they are
//! free to download (TS 102 744-2-1, "Annex C" archives) and are read at run
//! time from a folder holding `ts_1027440201_AnnexC1_v010101p0` and
//! `ts_1027440201_AnnexC2_v010101p0`. Ported from DecBGAN `bgan/annex.py`.
//!
//! The interleaver was checked for a closed form (QPP/ARP/dither-periodic
//! structure) and has none that fits, so it is table-driven too.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

pub const ANNEX_C1: &str = "ts_1027440201_AnnexC1_v010101p0";
pub const ANNEX_C2: &str = "ts_1027440201_AnnexC2_v010101p0";

/// Everything the receiver needs for one coding level.
#[derive(Debug)]
pub struct LevelTables {
    pub level: String,
    /// Information bits per block (D), without the 4 flush bits.
    pub d: usize,
    /// D + 4 flush bits.
    pub df: usize,
    /// Interleaver: the interleaved encoder is fed `df_bits[perm[j]]` at j.
    pub perm: Vec<u16>,
    /// Channel symbols per block (1496 for this bearer).
    pub fecsy: usize,
    /// Per channel bit slot (I1, I0, Q1, Q0 per symbol): which stream and
    /// position it carries. Packed as `kind << 16 | pos`, kind 0 = data,
    /// 1 = p parity, 2 = q parity (the global index space is the unpunctured
    /// rate-1/3 word, so `pos = idx - kind*DF`).
    pub slots: Vec<u32>,
}

impl LevelTables {
    #[inline]
    pub fn slot(&self, i: usize) -> (u8, usize) {
        let v = self.slots[i];
        ((v >> 16) as u8, (v & 0xFFFF) as usize)
    }
}

#[derive(Debug)]
pub enum TableError {
    NotFound(String),
    Parse(String),
}

impl std::fmt::Display for TableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TableError::NotFound(s) => write!(
                f,
                "ETSI Annex C tables not found ({s}); download TS 102 744-2-1's Annex C \
                 archives from etsi.org and extract them so the folders {ANNEX_C1} and \
                 {ANNEX_C2} sit in the folder you give DecSAT (or set DECSAT_BGAN_ANNEX)"
            ),
            TableError::Parse(s) => write!(f, "Annex C table: {s}"),
        }
    }
}
impl std::error::Error for TableError {}

/// Annex C file names spell the `R` level `RE`.
fn file_level(level: &str) -> &str {
    if level == "R" { "RE" } else { level }
}

/// Parse a `*_TCI.TXT`: after the dashed line, rows of `q data`.
pub fn parse_tci(text: &str) -> Result<Vec<u16>, TableError> {
    let mut started = false;
    let mut perm = Vec::new();
    for line in text.lines() {
        let s = line.trim();
        if s.starts_with("---") {
            started = true;
            continue;
        }
        if !started || s.is_empty() {
            continue;
        }
        let mut it = s.split_whitespace();
        let (Some(a), Some(b)) = (it.next(), it.next()) else {
            continue;
        };
        let q: usize = a.parse().map_err(|_| TableError::Parse(format!("bad row {s:?}")))?;
        let d: u16 = b.parse().map_err(|_| TableError::Parse(format!("bad row {s:?}")))?;
        if q != perm.len() {
            return Err(TableError::Parse("Q-bit column is not 0..N-1".into()));
        }
        perm.push(d);
    }
    let mut seen = vec![false; perm.len()];
    for &p in &perm {
        let p = p as usize;
        if p >= seen.len() || seen[p] {
            return Err(TableError::Parse("data-bit column is not a permutation".into()));
        }
        seen[p] = true;
    }
    Ok(perm)
}

/// Parse a `*_CIPM.TXT` into (D, DF, FECSy, packed slots).
pub fn parse_cipm(text: &str) -> Result<(usize, usize, usize, Vec<u32>), TableError> {
    // Header: "D=2000, FECSy=1496, DF=2004, N=5984, ..." in the first lines.
    let key = |name: &str| -> Option<usize> {
        for line in text.lines().take(8) {
            let mut rest = line;
            while let Some(i) = rest.find(name) {
                let pre_ok = i == 0
                    || !rest.as_bytes()[i - 1].is_ascii_alphanumeric();
                let after = &rest[i + name.len()..];
                if pre_ok && after.trim_start().starts_with('=') {
                    let v: String = after
                        .trim_start()
                        .trim_start_matches('=')
                        .trim_start()
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    if let Ok(n) = v.parse() {
                        return Some(n);
                    }
                }
                rest = &rest[i + name.len()..];
            }
        }
        None
    };
    let d = key("D").ok_or_else(|| TableError::Parse("no D=".into()))?;
    let df = key("DF").ok_or_else(|| TableError::Parse("no DF=".into()))?;
    let fecsy = key("FECSy").ok_or_else(|| TableError::Parse("no FECSy=".into()))?;

    let mut slots = Vec::with_capacity(fecsy * 4);
    let mut started = false;
    for line in text.lines() {
        let s = line.trim();
        if s.starts_with("---") {
            started = true;
            continue;
        }
        if !started || s.is_empty() || s.starts_with("Total") {
            continue;
        }
        let f: Vec<&str> = s.split_whitespace().collect();
        if f.len() != 6 || f[5].len() != 4 {
            continue;
        }
        for (k, ch) in f[5].bytes().enumerate() {
            let idx: usize = f[1 + k]
                .parse()
                .map_err(|_| TableError::Parse(format!("bad index in {s:?}")))?;
            let kind = match ch {
                b'.' => 0usize,
                b'+' => 1,
                b'-' => 2,
                _ => return Err(TableError::Parse(format!("bad pattern in {s:?}"))),
            };
            let pos = idx
                .checked_sub(kind * df)
                .filter(|&p| p < df)
                .ok_or_else(|| TableError::Parse(format!("index outside its stream: {s:?}")))?;
            slots.push(((kind as u32) << 16) | pos as u32);
        }
    }
    if slots.len() != fecsy * 4 {
        return Err(TableError::Parse(format!(
            "{} slots, expected {}",
            slots.len(),
            fecsy * 4
        )));
    }
    Ok((d, df, fecsy, slots))
}

/// Build a level's tables from the two files' text.
pub fn build_level(level: &str, tci: &str, cipm: &str) -> Result<LevelTables, TableError> {
    let perm = parse_tci(tci)?;
    let (d, df, fecsy, slots) = parse_cipm(cipm)?;
    if perm.len() != df {
        return Err(TableError::Parse(format!(
            "interleaver length {} != DF {df}",
            perm.len()
        )));
    }
    Ok(LevelTables {
        level: level.to_string(),
        d,
        df,
        perm,
        fecsy,
        slots,
    })
}

/// Does `root` hold both Annex C folders (directly, or under `work/`, `annex/`)?
fn dirs_in(root: &Path) -> Option<(PathBuf, PathBuf)> {
    for sub in ["", "work", "annex"] {
        let base = if sub.is_empty() { root.to_path_buf() } else { root.join(sub) };
        let (a, b) = (base.join(ANNEX_C1), base.join(ANNEX_C2));
        if a.is_dir() && b.is_dir() {
            return Some((a, b));
        }
    }
    None
}

fn user_dir() -> &'static Mutex<Option<PathBuf>> {
    static D: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(None))
}

/// Point the loader at a folder (from prefs or `--annex`). Clears the cache
/// so tables are read again from the new place.
pub fn set_annex_dir(p: Option<PathBuf>) {
    *user_dir().lock().unwrap() = p;
    cache().lock().unwrap().clear();
}

/// Where the tables are found, in order: [`set_annex_dir`], `DECSAT_BGAN_ANNEX`,
/// the working directory and its parents (three levels), the exe's folder.
pub fn locate() -> Result<(PathBuf, PathBuf), TableError> {
    let mut tried = Vec::new();
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(p) = user_dir().lock().unwrap().clone() {
        cands.push(p);
    }
    if let Some(p) = std::env::var_os("DECSAT_BGAN_ANNEX") {
        cands.push(p.into());
    }
    if let Ok(mut cwd) = std::env::current_dir() {
        for _ in 0..4 {
            cands.push(cwd.clone());
            if !cwd.pop() {
                break;
            }
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(d) = exe.parent()
    {
        cands.push(d.to_path_buf());
    }
    for c in cands {
        if let Some(found) = dirs_in(&c) {
            return Ok(found);
        }
        tried.push(c.display().to_string());
    }
    Err(TableError::NotFound(tried.join(", ")))
}

fn cache() -> &'static Mutex<HashMap<String, Arc<LevelTables>>> {
    static C: OnceLock<Mutex<HashMap<String, Arc<LevelTables>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The tables for `level` (one of [`crate::spec::LEVELS`]), cached.
pub fn tables(level: &str) -> Result<Arc<LevelTables>, TableError> {
    if let Some(t) = cache().lock().unwrap().get(level) {
        return Ok(t.clone());
    }
    let (c1, c2) = locate()?;
    let fl = file_level(level);
    let rd = |p: PathBuf| {
        std::fs::read_to_string(&p).map_err(|e| TableError::NotFound(format!("{}: {e}", p.display())))
    };
    let tci = rd(c1.join(format!("F80T45X{fl}_TCI.TXT")))?;
    let cipm = rd(c2.join(format!("F80T45X{fl}_CIPM.TXT")))?;
    let t = Arc::new(build_level(level, &tci, &cipm)?);
    cache().lock().unwrap().insert(level.to_string(), t.clone());
    Ok(t)
}

/// True when every level of the bearer can be loaded.
pub fn available() -> bool {
    crate::spec::LEVELS.iter().all(|l| tables(l).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TCI: &str = "F80T45XL3\nTurbo Code Interleaving\nUser data with 4 flush bits N=6\n\nQ-bit Data-bit\n--------------\n    0    2\n    1    0\n    2    5\n    3    1\n    4    4\n    5    3\n";

    #[test]
    fn tci_must_be_a_permutation() {
        assert_eq!(parse_tci(TCI).unwrap(), vec![2, 0, 5, 1, 4, 3]);
        let bad = TCI.replace("    5    3", "    5    2");
        assert!(parse_tci(&bad).is_err());
    }

    #[test]
    fn cipm_header_and_rows() {
        // D=2, DF=6: one symbol, slots data / p / data / q
        let t = "X\nUser data=2\nD=2, FECSy=1, DF=6, N=4\n\nSymbol I1 I0 Q1 Q0 Pattern\n-----\n    0  1  7  0  15 .+.-\nTotal data plus flush bits=6\n";
        let (d, df, fecsy, slots) = parse_cipm(t).unwrap();
        assert_eq!((d, df, fecsy), (2, 6, 1));
        // p index 7 -> pos 1 of kind 1; q index 15 -> pos 3 of kind 2
        assert_eq!(slots, vec![1, (1 << 16) | 1, 0, (2 << 16) | 3]);
    }

    #[test]
    fn missing_tables_say_where_to_get_them() {
        let e = TableError::NotFound("x".into()).to_string();
        assert!(e.contains("etsi.org") && e.contains(ANNEX_C1));
    }
}
