// made by CasualArclamp
//! Auto: a carrier claimed from its bracket on the waterfall gets the
//! survey's treatment (Rory: "make it the auto mode like the survey doing a
//! scan then choosing the right mode") — Identify first, then the decoder
//! the survey would pick for what it saw ([`survey::pick`]), on the carrier
//! as Identify measured it. Unlike the survey's, the VFO stays: a DVB-S2
//! carrier whose good frames turn out to be TS-mode moves on to the MPEG-TS
//! decoder, and one nothing here decodes stays on Identify, showing what it
//! is.

use std::collections::BTreeMap;
use std::time::Instant;

use decsat_engine::{DecoderKind, VfoId, VfoSettings, VfoStatus};

use crate::band_view::UiVfo;
use crate::survey::{self, Outcome};

/// Identify's answer is waited for this long, s (longer than the survey's:
/// nothing waits behind a claimed carrier).
const IDENTIFY_SECS: f64 = 30.0;
/// A DVB-S2 carrier's frames are watched this long for TS against GS
/// mode, s.
const MODE_SECS: f64 = 60.0;
/// A narrow carrier goes to the beacon decoder with a VFO this wide, Hz:
/// room for a telemetry subcarrier's sidebands either side (the IS-19
/// beacon's reach ±60 kHz).
const BEACON_LOOK_HZ: f64 = 200e3;

/// The VFO a beacon's telemetry needs: wide enough for its sidebands (a
/// seventh more for the filter's skirts), never narrower than it was.
fn beacon_width(id: &decsat_engine::Identification, now: f64) -> f64 {
    let want = id
        .beacon
        .filter(|b| b.telemetry.is_some())
        .map_or(BEACON_LOOK_HZ, |b| 2.3 * b.reach_hz);
    now.max(want)
}

enum Stage {
    Identify,
    /// On the GSE/IP decoder: TS-mode frames move it to the MPEG-TS one.
    Dvbs2,
}

struct Watch {
    id: VfoId,
    stage: Stage,
    since: Instant,
}

#[derive(Default)]
pub struct AutoPick {
    watching: Vec<Watch>,
}

impl AutoPick {
    /// Watch a VFO just made on Identify.
    pub fn watch(&mut self, id: VfoId) {
        self.watching.push(Watch {
            id,
            stage: Stage::Identify,
            since: Instant::now(),
        });
    }

    /// A frame's worth: the VFOs whose decoder has been picked, with their
    /// new settings, and a line for the status bar.
    pub fn tick(
        &mut self,
        statuses: &BTreeMap<VfoId, VfoStatus>,
        vfos: &[UiVfo],
    ) -> (Vec<(VfoId, VfoSettings)>, Option<String>) {
        let mut updates = Vec::new();
        let mut note = None;
        // Rust note: `retain_mut` keeps the watches the closure returns
        // `true` for, and lets it change them as it goes.
        self.watching.retain_mut(|w| {
            // Deleted, or its decoder changed by hand: no longer ours.
            let Some(v) = vfos.iter().find(|v| v.id == w.id) else {
                return false;
            };
            let want = match w.stage {
                Stage::Identify => DecoderKind::Identify,
                Stage::Dvbs2 => DecoderKind::Dvbs2Ip,
            };
            if v.settings.decoder != want {
                return false;
            }
            let Some(st) = statuses.get(&w.id) else {
                return true;
            };
            let name = &v.settings.name;
            let t = w.since.elapsed().as_secs_f64();
            match w.stage {
                Stage::Identify => {
                    // Only an answer for where the VFO is now (it may have
                    // been dragged meanwhile), and not a provisional one
                    // until the survey would take it.
                    let answer = st
                        .identification
                        .as_ref()
                        .filter(|_| st.vfo_offset_hz == v.settings.offset_hz)
                        .filter(|_| !st.provisional || t > survey::PROVISIONAL_SECS);
                    let Some(id) = answer else {
                        if t > IDENTIFY_SECS {
                            note = Some(format!("{name}: Identify found nothing; left on it"));
                            return false;
                        }
                        return true;
                    };
                    // A narrow carrier: the beacon decoder measures it, and
                    // looks for telemetry in a wider VFO round it.
                    let pick = match survey::pick(id) {
                        Err(Outcome::NoDecoder)
                            if id.verdict == decsat_engine::Verdict::Carrier =>
                        {
                            Ok(DecoderKind::Beacon)
                        }
                        p => p,
                    };
                    match pick {
                        Ok(kind) => {
                            let mut s = v.settings.clone();
                            s.decoder = kind;
                            s.signal = decsat_engine::SignalMode::of(kind);
                            s.offset_hz += id.center_offset_hz;
                            // (The burst analyser finds its carriers' rates
                            // itself.)
                            s.symbol_rate = id.symbol_rate.filter(|_| kind != DecoderKind::Tdma);
                            if kind == DecoderKind::Beacon {
                                s.bandwidth_hz = beacon_width(id, s.bandwidth_hz);
                                s.symbol_rate = None;
                            }
                            updates.push((w.id, s));
                            note = Some(format!("{name}: {} → {}", id.summary(), kind.label()));
                            if kind == DecoderKind::Dvbs2Ip {
                                w.stage = Stage::Dvbs2;
                                w.since = Instant::now();
                                return true;
                            }
                            false
                        }
                        Err(o) => {
                            note = Some(match o {
                                Outcome::NoSignal => format!("{name}: no signal found"),
                                _ => format!(
                                    "{name}: {} — no decoder for it here; left on Identify",
                                    id.summary()
                                ),
                            });
                            false
                        }
                    }
                }
                Stage::Dvbs2 => {
                    let Some(f) = st.fec.as_ref().filter(|f| f.ok > 0) else {
                        return t < MODE_SECS;
                    };
                    // Most good frames TS-mode: the MPEG-TS decoder reads
                    // them (GSE/IP only says "use the MPEG-TS decoder").
                    if 2 * f.ts_frames > f.ok {
                        let mut s = v.settings.clone();
                        s.decoder = DecoderKind::Dvbs2Ts;
                        updates.push((w.id, s));
                        note = Some(format!(
                            "{name}: TS-mode frames → {}",
                            DecoderKind::Dvbs2Ts.label()
                        ));
                    }
                    false
                }
            }
        });
        (updates, note)
    }
}

/// VFOs on the CDM type-found model whose chain turned out DVB-S (tried
/// first on QPSK that may be DVB-S): moved to the DVB-S decoder, whose
/// TS cards and outputs are keyed on it, with a line for the status bar.
/// `skip`: the survey's VFOs, which it moves itself.
pub fn dvbs_found(
    statuses: &BTreeMap<VfoId, VfoStatus>,
    vfos: &[UiVfo],
    skip: &[VfoId],
) -> (Vec<(VfoId, VfoSettings)>, Option<String>) {
    let mut updates = Vec::new();
    let mut note = None;
    for v in vfos {
        if v.settings.decoder != DecoderKind::CdmAuto || skip.contains(&v.id) {
            continue;
        }
        let Some(st) = statuses.get(&v.id) else {
            continue;
        };
        let locked = st.chain == Some(DecoderKind::DvbsTs)
            && st
                .fec
                .as_ref()
                .and_then(|f| f.dvbs.as_ref())
                .is_some_and(|d| d.rate.is_some() && d.packets > 0);
        if locked {
            let mut s = v.settings.clone();
            s.decoder = DecoderKind::DvbsTs;
            s.signal = decsat_engine::SignalMode::of(DecoderKind::DvbsTs);
            updates.push((v.id, s));
            note = Some(format!(
                "{}: DVB-S found → {}",
                v.settings.name,
                DecoderKind::DvbsTs.label()
            ));
        }
    }
    (updates, note)
}
