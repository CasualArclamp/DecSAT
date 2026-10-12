//! What carriers have been heard to carry — ADS-B, radar — kept by RF, so
//! that the waterfall can say so of a carrier with or without a VFO on it
//! (Rory: "show that that carrier is ADS-B and radar capable"). Learnt from
//! the VFOs' decoders as they find them, and kept in the user's settings
//! (`carrier_caps.rf<kHz>=adsb,radar`) for the next time the band is in
//! view.

use decsat_engine::VfoStatus;
use eframe::egui::Color32;

/// What a carrier carries, as far as its decoders have found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    /// ADS-B reports (ASTERIX CAT021: the ML–PH trunk's IP).
    pub adsb: bool,
    /// A radar's track feed (ASTERIX CAT048, the PH-ML BSC feed).
    pub radar: bool,
}

/// The tags' colours: the radar scope's ADS-B magenta, and the PPI green.
pub const ADSB: Color32 = Color32::from_rgb(235, 130, 235);
pub const RADAR: Color32 = Color32::from_rgb(110, 220, 110);

impl Caps {
    pub fn any(self) -> bool {
        self.adsb || self.radar
    }

    pub fn union(self, o: Caps) -> Caps {
        Caps {
            adsb: self.adsb || o.adsb,
            radar: self.radar || o.radar,
        }
    }

    /// The tags to show on the waterfall, with their colours.
    pub fn tags(self) -> Vec<(&'static str, Color32)> {
        [(self.adsb, "ADS-B", ADSB), (self.radar, "RADAR", RADAR)]
            .into_iter()
            .filter(|t| t.0)
            .map(|(_, n, c)| (n, c))
            .collect()
    }

    fn text(self) -> String {
        [self.adsb.then_some("adsb"), self.radar.then_some("radar")]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(",")
    }

    fn parse(s: &str) -> Caps {
        let has = |w: &str| s.split(',').any(|x| x.trim() == w);
        Caps {
            adsb: has("adsb"),
            radar: has("radar"),
        }
    }

    /// What a VFO's decoders have found on its carrier now: ADS-B reports;
    /// a radar's feed — sector messages or a scan (ASTERIX), or track
    /// reports that are not ADS-B (the BSC feed).
    pub fn of(st: &VfoStatus) -> Caps {
        let Some(v) = st
            .fec
            .as_ref()
            .and_then(|f| f.radar.as_ref())
            .map(|r| &r.radar)
        else {
            return Caps::default();
        };
        Caps {
            adsb: v.adsb,
            radar: v.sector.is_some() || v.scan_secs.is_some() || (!v.adsb && v.reports > 0),
        }
    }
}

/// The carriers known, by RF (Hz).
#[derive(Debug, Default)]
pub struct CarrierCaps {
    known: Vec<(f64, Caps)>,
}

impl CarrierCaps {
    /// As kept in the settings.
    pub fn load() -> Self {
        CarrierCaps {
            known: crate::prefs::carrier_caps()
                .into_iter()
                .map(|(hz, t)| (hz, Caps::parse(&t)))
                .filter(|(_, c)| c.any())
                .collect(),
        }
    }

    /// The carrier at `rf` (within [`crate::prefs::SAME_CARRIER_HZ`]), if
    /// known: its RF as kept, and what it carries.
    pub fn at(&self, rf: f64) -> Option<(f64, Caps)> {
        self.known
            .iter()
            .filter(|(hz, _)| (hz - rf).abs() <= crate::prefs::SAME_CARRIER_HZ)
            .min_by(|a, b| (a.0 - rf).abs().total_cmp(&(b.0 - rf).abs()))
            .copied()
    }

    /// A VFO at `rf` found `caps`: added to what is known of its carrier,
    /// and kept when that grows.
    pub fn learn(&mut self, rf: f64, caps: Caps) {
        if !caps.any() || !rf.is_finite() || rf <= 0.0 {
            return;
        }
        let (hz, old) = self.at(rf).unwrap_or((rf, Caps::default()));
        let new = old.union(caps);
        if new == old {
            return;
        }
        match self.known.iter_mut().find(|k| k.0 == hz) {
            Some(k) => k.1 = new,
            None => self.known.push((hz, new)),
        }
        let _ = crate::prefs::set_carrier_caps(hz, &new.text());
    }

    pub fn all(&self) -> &[(f64, Caps)] {
        &self.known
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_read_back_and_merge() {
        let c = Caps {
            adsb: true,
            radar: false,
        };
        assert_eq!(Caps::parse(&c.text()), c);
        let both = c.union(Caps {
            adsb: false,
            radar: true,
        });
        assert_eq!(both.text(), "adsb,radar");
        assert_eq!(Caps::parse("radar, adsb"), both);
        assert_eq!(
            both.tags().iter().map(|t| t.0).collect::<Vec<_>>(),
            ["ADS-B", "RADAR"]
        );
        assert!(!Caps::parse("").any());
    }

    #[test]
    fn a_carrier_is_found_near_its_rf() {
        let k = CarrierCaps {
            known: vec![
                (
                    12_300_000_000.0,
                    Caps {
                        adsb: true,
                        radar: false,
                    },
                ),
                (
                    12_300_050_000.0,
                    Caps {
                        adsb: false,
                        radar: true,
                    },
                ),
            ],
        };
        assert!(k.at(12_300_004_000.0).is_some_and(|(_, c)| c.adsb));
        assert!(k.at(12_300_046_000.0).is_some_and(|(_, c)| c.radar));
        assert!(k.at(12_300_025_000.0).is_none());
    }
}
