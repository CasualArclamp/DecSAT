//! The radar track feed in a modem's BSC blocks (see
//! `decsat_modem::radar`) or its HDLC frames of ASTERIX (see
//! `decsat_modem::asterix`): its track table for the GUI's scope and the
//! command line, and while recording a CSV log, one line a track report.

use std::io::Write;
use std::path::PathBuf;
use std::time::SystemTime;

use decsat_modem::bsc::Block;
use decsat_modem::radar::{CSV_HEADER, Message, RadarRx, csv_line};
// For the GUI, which reaches the modem crate through the engine.
pub use decsat_modem::radar::{RadarView, Report, SECTORS, Site, Track, tod_string};
// The Mode S registers' fields and their wording, and what saw a target
// (ASTERIX), for the scope.
pub use decsat_modem::asterix::detection;
pub use decsat_modem::mode_s;

use crate::fec::FecOutput;

/// The radar feed as the GUI and CLI see it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RadarStageView {
    pub radar: RadarView,
    /// The CSV log (current or last) and its lines.
    pub log: Option<(PathBuf, u64)>,
    pub logging: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub(crate) struct RadarStage {
    rx: RadarRx,
    file: Option<std::io::BufWriter<std::fs::File>>,
    path: Option<PathBuf>,
    lines: u64,
    error: Option<String>,
}

impl RadarStage {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Log while the VFO records.
    pub(crate) fn follow(&mut self, o: &FecOutput) {
        match (o.record, self.file.is_some()) {
            (true, false) if self.error.is_none() => {
                let stamp = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let path = o.dir.join(format!(
                    "decsat-{}-{}-radar-{stamp}.csv",
                    o.name.replace(' ', "_"),
                    crate::rf::freq_tag(o.carrier_hz)
                ));
                let _ = std::fs::create_dir_all(&o.dir);
                match std::fs::File::create(&path) {
                    Ok(f) => {
                        let mut w = std::io::BufWriter::new(f);
                        if let Err(e) = writeln!(w, "{CSV_HEADER}") {
                            self.error = Some(e.to_string());
                        } else {
                            self.file = Some(w);
                        }
                    }
                    Err(e) => self.error = Some(format!("{}: {e}", path.display())),
                }
                self.path = Some(path);
                self.lines = 0;
            }
            (false, true) => {
                if let Some(mut f) = self.file.take() {
                    let _ = f.flush();
                }
            }
            (false, false) => self.error = None,
            _ => {}
        }
    }

    pub(crate) fn blocks(&mut self, blocks: &[Block]) {
        for b in blocks {
            if let Some(m) = self.rx.block(&b.heading, &b.text) {
                self.log(&m);
            }
        }
        self.flush();
    }

    /// An HDLC frame's ASTERIX messages (`decsat_modem::asterix::messages`).
    pub(crate) fn messages(&mut self, ms: &[Message]) {
        for m in ms {
            self.rx.message(m);
            self.log(m);
        }
        self.flush();
    }

    /// A message's reports to the CSV log, while recording.
    fn log(&mut self, m: &Message) {
        let Some(f) = &mut self.file else { return };
        for r in &m.reports {
            if let Err(e) = writeln!(f, "{}", csv_line(m, r)) {
                self.error = Some(e.to_string());
                self.file = None;
                return;
            }
            self.lines += 1;
        }
    }

    /// Lines reach the disk a block or frame at a time (a few a second at
    /// most).
    fn flush(&mut self) {
        if let Some(f) = &mut self.file {
            let _ = f.flush();
        }
    }

    pub(crate) fn view(&self) -> RadarStageView {
        RadarStageView {
            radar: self.rx.view(),
            log: self.path.clone().map(|p| (p, self.lines)),
            logging: self.file.is_some(),
            error: self.error.clone(),
        }
    }
}
