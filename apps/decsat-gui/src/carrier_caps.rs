//! What carriers have been heard to carry — ADS-B, radar — and what modem
//! sends them (the Comtech CDM model, a Q-Flex), kept by RF, so that the
//! waterfall can say so of a carrier with or without a VFO on it (Rory:
//! "show that that carrier is ADS-B and radar capable", "show CDM type on
//! the waterfall carrier tags too"). Learnt from the VFOs' decoders as they
//! find them, and kept in the user's settings
//! (`carrier_caps.rf<kHz>=adsb,radar,model=cdm570-trunk`) for the next time
//! the band is in view.

use decsat_engine::{DecoderKind, VfoSettings, VfoStatus};
use eframe::egui::Color32;

/// What a carrier carries, as far as its decoders have found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    /// ADS-B reports (ASTERIX CAT021: the ML–PH trunk's IP).
    pub adsb: bool,
    /// A radar's track feed (ASTERIX CAT048, the PH-ML BSC feed).
    pub radar: bool,
    /// The modem it is: a CDM model, or a Q-Flex FastLink.
    pub model: Option<DecoderKind>,
}

/// The tags' colours: the radar scope's ADS-B magenta, the PPI green, and a
/// light blue for the modem.
pub const ADSB: Color32 = Color32::from_rgb(235, 130, 235);
pub const RADAR: Color32 = Color32::from_rgb(110, 220, 110);
pub const MODEM: Color32 = Color32::from_rgb(140, 185, 255);

/// A modem's tag: the CDM model, or the Q-Flex.
pub fn model_tag(k: DecoderKind) -> Option<&'static str> {
    Some(match k {
        DecoderKind::Tpc2964 => "CDM-570L TPC",
        DecoderKind::Cdm600Voice => "CDM-600L voice",
        DecoderKind::Cdm570 => "CDM-570L Viterbi",
        DecoderKind::Cdm570Trunk => "CDM-570L trunk",
        DecoderKind::FastLink => "Q-Flex",
        _ => return None,
    })
}

impl Caps {
    pub fn any(self) -> bool {
        self.adsb || self.radar || self.model.is_some()
    }

    /// `self` with what `newer` found: ADS-B and radar once heard stay; a
    /// model found again replaces the one kept (it is read off the payload,
    /// which can say more as it goes).
    pub fn union(self, newer: Caps) -> Caps {
        Caps {
            adsb: self.adsb || newer.adsb,
            radar: self.radar || newer.radar,
            model: newer.model.or(self.model),
        }
    }

    /// The tags to show on the waterfall, with their colours: the modem
    /// first.
    pub fn tags(self) -> Vec<(&'static str, Color32)> {
        let modem = self.model.and_then(model_tag).map(|t| (t, MODEM));
        modem
            .into_iter()
            .chain(
                [(self.adsb, "ADS-B", ADSB), (self.radar, "RADAR", RADAR)]
                    .into_iter()
                    .filter(|t| t.0)
                    .map(|(_, n, c)| (n, c)),
            )
            .collect()
    }

    fn text(self) -> String {
        let model = self.model.map(|k| format!("model={}", k.key()));
        [
            self.adsb.then(|| "adsb".to_string()),
            self.radar.then(|| "radar".to_string()),
            model,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(",")
    }

    fn parse(s: &str) -> Caps {
        let words: Vec<&str> = s.split(',').map(str::trim).collect();
        Caps {
            adsb: words.contains(&"adsb"),
            radar: words.contains(&"radar"),
            model: words
                .iter()
                .find_map(|w| DecoderKind::from_key(w.strip_prefix("model=")?)),
        }
    }

    /// What a VFO's decoders have found on its carrier now: ADS-B reports;
    /// a radar's feed — sector messages or a scan (ASTERIX), or track
    /// reports that are not ADS-B (the BSC feed); and the modem, once its
    /// code has locked — for a CDM VFO the model its data say (as the type
    /// found works it out, whichever model was chosen), for a FastLink VFO
    /// a Q-Flex.
    pub fn of(s: &VfoSettings, st: &VfoStatus) -> Caps {
        let f = st.fec.as_ref();
        let v = f.and_then(|f| f.radar.as_ref()).map(|r| &r.radar);
        Caps {
            adsb: v.is_some_and(|v| v.adsb),
            radar: v.is_some_and(|v| {
                v.sector.is_some() || v.scan_secs.is_some() || (!v.adsb && v.reports > 0)
            }),
            model: modem(s, st),
        }
    }
}

/// The modem a VFO's carrier is, once its code has locked (and, for TPC
/// 2964, once the payload's format is known: the model is read off it).
fn modem(s: &VfoSettings, st: &VfoStatus) -> Option<DecoderKind> {
    let (chain, f) = (st.chain?, st.fec.as_ref()?);
    let locked = match chain {
        DecoderKind::Tpc2964 => {
            f.tpc.as_ref().is_some_and(|t| t.decoded > 0)
                && f.payload.as_ref().is_some_and(|p| p.found.is_some())
        }
        DecoderKind::Viterbi => f.viterbi.as_ref().is_some_and(|v| v.rate.is_some()),
        DecoderKind::FastLink => f.fastlink.as_ref().is_some_and(|x| x.decoded > 0),
        _ => false,
    };
    if !locked {
        return None;
    }
    if s.decoder.is_cdm() {
        Some(
            st.cdm_model
                .unwrap_or(decsat_engine::vfo::cdm_model(chain, Some(f))),
        )
    } else {
        (s.decoder == DecoderKind::FastLink).then_some(DecoderKind::FastLink)
    }
}

/// The modem a VFO's carrier is: as its decoders find it now, else as kept
/// for the carrier at `rf` (Hz; 0 or less: not known).
pub fn vfo_model(
    s: &VfoSettings,
    st: Option<&VfoStatus>,
    known: &CarrierCaps,
    rf: f64,
) -> Option<DecoderKind> {
    st.and_then(|st| modem(s, st)).or_else(|| {
        (rf > 0.0)
            .then(|| known.at(rf))
            .flatten()
            .and_then(|(_, c)| c.model)
    })
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
            ..Default::default()
        };
        assert_eq!(Caps::parse(&c.text()), c);
        let both = c.union(Caps {
            radar: true,
            ..Default::default()
        });
        assert_eq!(both.text(), "adsb,radar");
        assert_eq!(Caps::parse("radar, adsb"), both);
        assert_eq!(
            both.tags().iter().map(|t| t.0).collect::<Vec<_>>(),
            ["ADS-B", "RADAR"]
        );
        assert!(!Caps::parse("").any());
        // The modem: kept by its key, the newer reading winning, its tag
        // first.
        let tpc = both.union(Caps {
            model: Some(DecoderKind::Tpc2964),
            ..Default::default()
        });
        let trunk = tpc.union(Caps {
            model: Some(DecoderKind::Cdm570Trunk),
            ..Default::default()
        });
        assert_eq!(trunk.text(), "adsb,radar,model=cdm570-trunk");
        assert_eq!(Caps::parse(&trunk.text()), trunk);
        assert_eq!(
            trunk.tags().iter().map(|t| t.0).collect::<Vec<_>>(),
            ["CDM-570L trunk", "ADS-B", "RADAR"]
        );
        // Only the modem known is a carrier worth keeping too.
        assert!(Caps::parse("model=cdm570").any());
    }

    #[test]
    fn a_carrier_is_found_near_its_rf() {
        let k = CarrierCaps {
            known: vec![
                (
                    12_300_000_000.0,
                    Caps {
                        adsb: true,
                        ..Default::default()
                    },
                ),
                (
                    12_300_050_000.0,
                    Caps {
                        radar: true,
                        ..Default::default()
                    },
                ),
            ],
        };
        assert!(k.at(12_300_004_000.0).is_some_and(|(_, c)| c.adsb));
        assert!(k.at(12_300_046_000.0).is_some_and(|(_, c)| c.radar));
        assert!(k.at(12_300_025_000.0).is_none());
    }
}
