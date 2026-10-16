// made by CasualArclamp
//! BISS keys remembered by carrier: a key seen to fit is kept for its
//! carrier's RF (per satellite, in the user's settings), and a scrambled
//! carrier with no key typed is given the one kept for it — once, so a key
//! cleared by hand stays cleared.

use std::collections::BTreeMap;

use decsat_engine::{TsView, VfoId, VfoSettings};
use decsat_ts::{BissKey, Verdict};

use crate::prefs;

#[derive(Default)]
pub struct BissMemory {
    /// VFOs already offered a kept key, and the carrier (Hz) they were on.
    offered: BTreeMap<VfoId, f64>,
    /// The control word last kept for each VFO, so the settings file is
    /// written once per key, not every frame.
    kept: BTreeMap<VfoId, [u8; 8]>,
}

impl BissMemory {
    /// Look at VFO `id` (on the carrier at `rf_hz`, when the RF is known):
    /// keep a key that fits; return the kept key to give it, if it should
    /// be given one.
    pub fn check(
        &mut self,
        sat: &str,
        id: VfoId,
        rf_hz: Option<f64>,
        s: &VfoSettings,
        ts: &TsView,
    ) -> Option<String> {
        let rf = rf_hz?;
        let typed = BissKey::parse(&s.biss_key).ok();
        if let Some(k) = typed
            && ts.biss.verdict == Verdict::Right
            && self.kept.get(&id) != Some(&k.cw)
        {
            self.kept.insert(id, k.cw);
            let _ = prefs::set_biss_key(sat, rf, Some(&k.text()));
        }
        if !s.biss_key.trim().is_empty() || ts.biss.verdict != Verdict::NoKey {
            return None;
        }
        let near = |a: f64| (a - rf).abs() <= prefs::BISS_SAME_CARRIER_HZ;
        if self.offered.get(&id).is_some_and(|&a| near(a)) {
            return None;
        }
        self.offered.insert(id, rf);
        prefs::biss_key(sat, rf)
    }
}
