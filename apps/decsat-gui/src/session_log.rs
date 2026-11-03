// Session log for DecSAT (OM5NA, 2026-10-19)
//! The session log: what the VFOs report, written as it changes to a CSV in
//! the output folder, for reading back after a session (a spreadsheet, a
//! script, a text editor).
//!
//! One line an event: UTC time, VFO, RF frequency, decoder, kind and text.
//! Kinds: `identify` (Identify's verdict), `message` (the decoder's status
//! line), `lock` / `lost` (the carrier loop), `modcod` (a MODCOD seen for
//! the first time), `text` (a readable string found in the bits), `summary`
//! (a VFO's state once a minute), `vfo` (added, removed, retuned), `note`
//! (the app's status-bar notes: source started, stopped, a snapshot saved).
//!
//! The panels redraw many times a second and their figures move all the
//! time, so a line is written only when what is said changes — text with
//! its digits left out — or, for the same thing with new figures, once a
//! minute at most. The file stays small enough to read through.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use decsat_engine::{VfoId, VfoStatus};

use crate::band_view::UiVfo;

/// The same statement with new figures is written again after this long.
const REFRESH: Duration = Duration::from_secs(60);
/// A VFO's state is summarised this often.
const SUMMARY: Duration = Duration::from_secs(60);
/// Strings remembered per VFO (so each is written once); beyond this the
/// oldest are forgotten.
const STRINGS_KEPT: usize = 4000;
/// Text lines written per VFO a minute at most; the rest are counted.
const TEXT_PER_MINUTE: u32 = 30;
/// A change of lock is written once it has held this long (an ACM carrier
/// or a weak one flickers).
const LOCK_HOLD: Duration = Duration::from_secs(2);

/// UTC as `2026-10-19 21:06:03.250`, and as `20261019-210603` for file
/// names.
///
/// Rust note: the standard library has no calendar; the date comes from the
/// Unix day number by the usual civil-from-days arithmetic (H. Hinnant's).
pub fn utc(t: SystemTime) -> (String, String) {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let (h, m, s) = (sod / 3600, sod / 60 % 60, sod % 60);
    (
        format!(
            "{year:04}-{month:02}-{day:02} {h:02}:{m:02}:{s:02}.{:03}",
            d.subsec_millis()
        ),
        format!("{year:04}{month:02}{day:02}-{h:02}{m:02}{s:02}"),
    )
}

/// A CSV field: quoted, quotes doubled, line breaks flattened.
fn field(s: &str) -> String {
    let s = s.replace(['\r', '\n'], " ");
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// What a statement says, without its figures: the key that decides
/// whether it is new.
fn gist(s: &str) -> String {
    s.chars().filter(|c| !c.is_ascii_digit()).collect()
}

/// Whether a string found in the bits reads as text rather than chance:
/// six characters or more, nearly all letters, digits, spaces and plain
/// punctuation, with a word of three or more and some letters. Chance strings from
/// coded or scrambled data are full of `@ $ & " \` ^`.
fn reads_as_text(s: &str) -> bool {
    let n = s.chars().count();
    if n < 6 {
        return false;
    }
    let plain = s
        .chars()
        .filter(|c| c.is_alphanumeric() || " .,:;-_/=+'()?!".contains(*c))
        .count();
    let mut run = 0;
    let mut best = 0;
    for c in s.chars() {
        run = if c.is_alphanumeric() { run + 1 } else { 0 };
        best = best.max(run);
    }
    let letters = s.chars().filter(|c| c.is_alphabetic()).count();
    plain * 100 >= n * 90 && best >= 3 && letters >= 2
}

/// What has been said about one thing (a VFO's verdict, its status line):
/// each gist with when it was last written.
///
/// A decoder's status line can swing between two or three states many
/// times a second ("found a PLHEADER, confirming" / "locked · …"); each
/// state is written when it is first seen and then at most once a
/// [`REFRESH`], however often it comes back.
#[derive(Default)]
struct Said {
    seen: HashMap<String, Instant>,
}

impl Said {
    /// Whether `text` is worth a new line.
    fn update(&mut self, text: &str) -> bool {
        let g = gist(text);
        let due = self.seen.get(&g).is_none_or(|t| t.elapsed() >= REFRESH);
        if due {
            if self.seen.len() > 200 {
                self.seen.retain(|_, t| t.elapsed() < REFRESH);
            }
            self.seen.insert(g, Instant::now());
        }
        due
    }
}

/// What has been written about one VFO.
#[derive(Default)]
struct Memo {
    identify: Said,
    message: Said,
    locked: Option<bool>,
    /// A change of lock not yet held long enough: the state and since when.
    lock_change: Option<(bool, Instant)>,
    /// Text lines written this minute, the minute's start, and lines held
    /// back.
    text_count: u32,
    text_since: Option<Instant>,
    text_held: u32,
    modcods: BTreeSet<u8>,
    strings: HashSet<String>,
    string_order: Vec<String>,
    summary_at: Option<Instant>,
    /// Where it was and what it ran, to note a retune or a new decoder.
    place: String,
}

/// The session log; off until started.
#[derive(Default)]
pub struct SessionLog {
    out: Option<(BufWriter<File>, PathBuf)>,
    memos: HashMap<VfoId, Memo>,
    lines: u64,
    last_note: String,
    flushed: Option<Instant>,
    pub error: Option<String>,
}

impl SessionLog {
    pub fn is_on(&self) -> bool {
        self.out.is_some()
    }

    pub fn path(&self) -> Option<&Path> {
        self.out.as_ref().map(|(_, p)| p.as_path())
    }

    pub fn lines(&self) -> u64 {
        self.lines
    }

    /// Open a new log in `dir`.
    pub fn start(&mut self, dir: &Path) {
        self.stop();
        let (_, stamp) = utc(SystemTime::now());
        let path = dir.join(format!("decsat-log-{stamp}.csv"));
        let _ = std::fs::create_dir_all(dir);
        match File::create(&path) {
            Ok(f) => {
                let mut w = BufWriter::new(f);
                // A UTF-8 byte-order mark, so a spreadsheet reads the
                // accents and symbols right; then the header.
                let _ = w.write_all(b"\xEF\xBB\xBFutc,vfo,rf_mhz,decoder,kind,text\n");
                self.out = Some((w, path));
                self.memos.clear();
                self.lines = 0;
                self.error = None;
                self.write("", None, "", "note", "session log started");
            }
            Err(e) => self.error = Some(format!("{}: {e}", path.display())),
        }
    }

    pub fn stop(&mut self) {
        if self.out.is_some() {
            self.write("", None, "", "note", "session log stopped");
        }
        if let Some((mut w, _)) = self.out.take() {
            let _ = w.flush();
        }
    }

    fn write(&mut self, vfo: &str, rf_hz: Option<f64>, decoder: &str, kind: &str, text: &str) {
        let Some((w, _)) = &mut self.out else { return };
        let (t, _) = utc(SystemTime::now());
        let rf = rf_hz.map_or(String::new(), |f| format!("{:.6}", f / 1e6));
        let line = format!(
            "{t},{},{rf},{},{kind},{}\n",
            field(vfo),
            field(decoder),
            field(text)
        );
        if let Err(e) = w.write_all(line.as_bytes()) {
            self.error = Some(e.to_string());
            self.out = None;
            return;
        }
        self.lines += 1;
    }

    /// An event of the app's own (a snapshot saved, say).
    pub fn note(&mut self, text: &str) {
        self.write("", None, "", "note", text);
    }

    /// The status bar's note, written when it changes.
    pub fn follow_note(&mut self, note: &str) {
        if note != self.last_note {
            self.last_note = note.to_string();
            if !note.is_empty() {
                self.write("", None, "", "note", note);
            }
        }
    }

    /// Look at every VFO's status and write what is new. `rf_center` is the
    /// span's RF centre (0 when unknown: the offsets are written instead).
    pub fn observe(
        &mut self,
        vfos: &[UiVfo],
        statuses: &BTreeMap<VfoId, VfoStatus>,
        rf_center: f64,
    ) {
        if self.out.is_none() {
            return;
        }
        // VFOs gone since the last look.
        let gone: Vec<VfoId> = self
            .memos
            .keys()
            .copied()
            .filter(|id| !vfos.iter().any(|v| v.id == *id))
            .collect();
        for id in gone {
            self.memos.remove(&id);
            self.write(&format!("#{id}"), None, "", "vfo", "removed");
        }
        for v in vfos {
            let name = v.settings.name.clone();
            let rf = (rf_center > 0.0).then_some(rf_center + v.settings.offset_hz);
            let decoder = v.settings.decoder.label();
            let mut lines: Vec<(&'static str, String)> = Vec::new();
            let memo = self.memos.entry(v.id).or_default();

            let place = format!(
                "{:.0} Hz wide at {:+.0} Hz, {decoder}",
                v.settings.bandwidth_hz, v.settings.offset_hz
            );
            if place != memo.place {
                let what = if memo.place.is_empty() {
                    "added"
                } else {
                    "changed"
                };
                lines.push(("vfo", format!("{what}: {place}")));
                memo.place = place;
            }

            if let Some(st) = statuses.get(&v.id) {
                if let Some(id) = &st.identification {
                    let s = id.summary();
                    if memo.identify.update(&s) {
                        lines.push(("identify", s));
                    }
                }
                if !st.message.is_empty() && memo.message.update(&st.message) {
                    lines.push(("message", st.message.clone()));
                }
                if let Some(c) = &st.carrier {
                    if memo.locked == Some(c.locked) {
                        memo.lock_change = None;
                    } else {
                        let since = match memo.lock_change {
                            Some((state, t)) if state == c.locked => t,
                            _ => {
                                let t = Instant::now();
                                memo.lock_change = Some((c.locked, t));
                                t
                            }
                        };
                        if since.elapsed() >= LOCK_HOLD {
                            memo.lock_change = None;
                            let was = memo.locked.replace(c.locked);
                            if c.locked {
                                lines.push((
                                    "lock",
                                    format!(
                                        "{} locked, Es/N0 {:.1} dB",
                                        c.modulation.name(),
                                        c.mer_db
                                    ),
                                ));
                            } else if was == Some(true) {
                                lines.push(("lost", "carrier lock lost".into()));
                            }
                        }
                    }
                }
                for &m in st.modcods.keys() {
                    if memo.modcods.insert(m) {
                        lines.push(("modcod", decsat_core::modcod_name(m)));
                    }
                }
                // Text: only the strings of a reading that stands out (as
                // the panel shows them) — the longest strings of all 256
                // readings are mostly chance, and stay in the panel — and
                // at most TEXT_PER_MINUTE a minute.
                if memo
                    .text_since
                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(60))
                {
                    if memo.text_held > 0 {
                        lines.push((
                            "text",
                            format!("({} more lines of text not written)", memo.text_held),
                        ));
                    }
                    memo.text_since = Some(Instant::now());
                    memo.text_count = 0;
                    memo.text_held = 0;
                }
                if let Some(t) = st.text.as_ref().filter(|t| t.best.is_some()) {
                    for s in &t.strings {
                        let s = s.trim();
                        if !reads_as_text(s) || !memo.strings.insert(s.to_string()) {
                            continue;
                        }
                        memo.string_order.push(s.to_string());
                        if memo.text_count < TEXT_PER_MINUTE {
                            memo.text_count += 1;
                            lines.push(("text", s.to_string()));
                        } else {
                            memo.text_held += 1;
                        }
                    }
                }
                {
                    if memo.string_order.len() > STRINGS_KEPT {
                        let old: Vec<String> =
                            memo.string_order.drain(..STRINGS_KEPT / 2).collect();
                        for o in old {
                            memo.strings.remove(&o);
                        }
                    }
                }
                // The first summary a minute in, once there is something to
                // say; then a minute apart.
                let since = *memo.summary_at.get_or_insert_with(Instant::now);
                if since.elapsed() >= SUMMARY {
                    memo.summary_at = Some(Instant::now());
                    let mut s = format!("level {:.1} dB", st.level_db);
                    if let Some(c) = &st.carrier {
                        s += &format!(
                            ", {} {}, Es/N0 {:.1} dB, offset {:+.0} Hz",
                            c.modulation.name(),
                            if c.locked { "locked" } else { "not locked" },
                            c.mer_db,
                            c.offset_hz
                        );
                    }
                    if let Some(r) = st.symbol_rate {
                        s += &format!(", {:.1} kS/s", r / 1e3);
                    }
                    if st.frames > 0 {
                        s += &format!(", {} frames, {} lock losses", st.frames, st.lock_losses);
                    }
                    if let Some(f) = &st.fec {
                        s += &format!(", FEC {} of {} frames good", f.ok, f.frames);
                    }
                    if st.dropped > 0 {
                        s += &format!(", {} blocks dropped", st.dropped);
                    }
                    lines.push(("summary", s));
                }
            }
            for (kind, text) in lines {
                self.write(&name, rf, decoder, kind, &text);
            }
        }
        // Written through every few seconds, so a crash loses little.
        if self
            .flushed
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(3))
        {
            self.flushed = Some(Instant::now());
            if let Some((w, _)) = &mut self.out {
                let _ = w.flush();
            }
        }
    }
}

impl Drop for SessionLog {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_dates_are_right() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_millis(1_792_443_963_250);
        let (long, short) = utc(t);
        assert_eq!(long, "2026-10-19 21:06:03.250");
        assert_eq!(short, "20261019-210603");
        let (long, _) = utc(SystemTime::UNIX_EPOCH + Duration::from_secs(951_782_400));
        assert_eq!(long, "2000-02-29 00:00:00.000");
    }

    #[test]
    fn fields_are_quoted() {
        assert_eq!(field("a \"b\"\nc"), "\"a \"\"b\"\" c\"");
    }

    #[test]
    fn chance_strings_are_left_out() {
        assert!(reads_as_text("DecSAT test radio"));
        assert!(reads_as_text("o=- 1 1 IN IP4 192.0.2.99"));
        assert!(!reads_as_text("`F\"dBBfF$F@b"));
        assert!(!reads_as_text("/&O.CTrcT`h"));
        assert!(!reads_as_text("12345678"));
    }

    #[test]
    fn new_figures_alone_wait_a_minute() {
        let mut s = Said::default();
        assert!(s.update("Es/N0 12.1 dB"));
        assert!(!s.update("Es/N0 12.3 dB"));
        assert!(s.update("lock lost"));
        // Swinging back and forth writes each state once.
        assert!(!s.update("Es/N0 12.4 dB"));
        assert!(!s.update("lock lost"));
    }
}
