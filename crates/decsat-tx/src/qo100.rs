// made by CasualArclamp
//! QO-100 (Es'hail-2) wideband transponder presets for DATV.
//!
//! Sources, cited at each value:
//!
//! - **[V3]** BATC / AMSAT-DL, "QO-100 Wideband Transponder – 2021 Operating
//!   Guidelines and Bandplan", Version 3, Final, 6 February 2021
//!   (<https://wiki.batc.org.uk/images/a/a2/QO-100_WB_Bandplan_V3.pdf>,
//!   linked from <https://wiki.batc.org.uk/QO-100_WB_Bandplan>).
//! - **[CONF]** BATC wiki, "Es'hail-2 conference transmissions"
//!   (<https://wiki.batc.org.uk/Es%27hail-2_conference_transmissions>):
//!   DVB-S2, QPSK, FEC 2/3, 0.2 roll-off.
//! - **[USER]** The user's choice at this app's kickoff (2026-11-03): pilots
//!   on, normal FECFRAMEs. Neither source above says.
//! - **[AIR]** Stations received on QO-100 with DecSAT, 2026-11-09, roll-off
//!   0.20 and pilots off in every case. Frame size and pilots follow from
//!   the TS rate the receiver reported, which only one combination fits:
//!   333 kS QPSK 3/5, short frames (385.1 kbit/s, two stations); 500 kS QPSK
//!   3/5, short frames (578.3 kbit/s, also about 60 PLFRAMEs a second).
//!   Also heard: 500 kS QPSK 2/3, normal frames (661.1 kbit/s).
//!
//! The bandplan names no MODCOD for users (only the beacon's FEC 4/5). The
//! 333 and 500 kS presets are what [AIR] heard; the rest start at [CONF]'s
//! QPSK 2/3 (QPSK 1/2 for the very narrow rates, where the margin matters
//! more than the rate) with [USER]'s framing. All of it stays editable in
//! the app.

use decsat_core::RollOff;
use decsat_mod::datv::DatvConfig;

/// Downlink = uplink + this, MHz: the transponder's conversion, as every
/// row of [V3]'s §4 table gives it (2402.0 → 10491.5).
pub const DOWNLINK_OFFSET_MHZ: f64 = 8089.5;
/// The wideband transponder's uplink, MHz ([V3] §5: 2401.0–2410.0 RHCP).
pub const UPLINK_MHZ: (f64, f64) = (2401.0, 2410.0);
/// The beacon's uplink, MHz ([V3] §4: 2402.0, DVB-S2 1500 kS FEC 4/5).
/// Never offered as a channel: it is the beacon's.
pub const BEACON_UPLINK_MHZ: f64 = 2402.0;
/// The maintenance uplink, downlink MHz ([V3] Bandplan note 2: 10494.0–
/// 10497.0, "used very occasionally; … give it absolute priority").
pub const MAINTENANCE_DOWNLINK_MHZ: (f64, f64) = (10494.0, 10497.0);
/// "1500 kS is the maximum symbol rate that should routinely be used"
/// ([V3] Transmission Modes).
pub const MAX_ROUTINE_SYMBOL_RATE: f64 = 1.5e6;

/// The PIDs [V3] recommends "to enable easy decoding" (Transmission
/// Modes): video 256, audio 257, PMT 32 or 4095, PCR 256 (or 258). The
/// service name "should be set to CallSign".
pub const PID_VIDEO: u16 = 256;
pub const PID_AUDIO: u16 = 257;
pub const PID_PMT: u16 = 4095;

/// DVB-S2 MODCOD numbers (EN 302 307-1 Table 12).
const QPSK_1_2: u8 = 4;
const QPSK_3_5: u8 = 5;
const QPSK_2_3: u8 = 6;

/// One preset: a group of channels sharing a symbol rate and mode.
#[derive(Debug, Clone, PartialEq)]
pub struct Preset {
    /// "Net · 333 kS".
    pub name: &'static str,
    /// [V3]'s segment: "Narrow", "Very narrow", "Wide", "Conference".
    pub segment: &'static str,
    /// The uplink centres it may use, MHz, the recommended one first.
    pub uplinks_mhz: Vec<f64>,
    pub symbol_rate: f64,
    pub modcod: u8,
    pub pilots: bool,
    pub short_fecframe: bool,
    pub roll_off: RollOff,
    /// Where the values come from.
    pub source: &'static str,
}

impl Preset {
    /// The modulator's settings (symbol rate, MODCOD, pilots, roll-off,
    /// frame size); the rest as `base` has it.
    pub fn datv(&self, base: DatvConfig) -> DatvConfig {
        DatvConfig {
            modcod: self.modcod,
            short_fecframe: self.short_fecframe,
            pilots: self.pilots,
            roll_off: self.roll_off,
            symbol_rate: self.symbol_rate,
            ..base
        }
    }

    /// The TS rate it carries, bit/s.
    pub fn ts_rate(&self) -> f64 {
        self.datv(DatvConfig::default()).ts_rate().unwrap_or(0.0)
    }
}

/// `n` uplinks from `first` MHz, `step` apart.
fn slots(first: f64, step: f64, n: usize) -> Vec<f64> {
    (0..n).map(|i| first + step * i as f64).collect()
}

/// Uplinks in recommended order: [V3] Bandplan table, Narrow rows: "Use
/// frequencies above 10497.0 first for 333 kS and below", so the highest
/// first.
fn high_first(mut v: Vec<f64>) -> Vec<f64> {
    v.sort_by(|a, b| b.total_cmp(a));
    v
}

/// Every preset, in the order the picker lists them.
pub fn presets() -> Vec<Preset> {
    // Pilots on, normal frames: [USER]. Roll-off 0.20: [CONF].
    let p = |name, segment, uplinks_mhz, symbol_rate, modcod, source| Preset {
        name,
        segment,
        uplinks_mhz,
        symbol_rate,
        modcod,
        pilots: true,
        short_fecframe: false,
        roll_off: RollOff::R20,
        source,
    };
    // Pilots off and the frame size [AIR] heard.
    let short_no_pilots = |preset: Preset| Preset {
        pilots: false,
        short_fecframe: true,
        ..preset
    };
    // [V3] §4: Narrow, 333 kS, 2403.25 then every 500 kHz to 2409.75 (14).
    let narrow = slots(2403.25, 0.5, 14);
    // [V3] §4 notes: "The lower 9 frequencies may be used for 500 kS", and
    // Bandplan note 1: 500 kS "confined to the Wide and Narrow DATV section".
    let narrow500 = slots(2403.25, 0.5, 9);
    // [V3] §4: Very Narrow, 125 kS, 2403.25 then every 250 kHz to 2409.75 (27).
    let very = slots(2403.25, 0.25, 27);
    // [V3] §4: Wide, 1 MS at 2403.75, 2405.25, 2406.75; "1.5 MS
    // transmissions should use this part of the band".
    let wide = vec![2403.75, 2405.25, 2406.75];
    vec![
        // [V3] Bandplan note 3: "Regular nets should use 10499.25 MHz and
        // 333 kS when possible" (uplink 2409.75).
        short_no_pilots(p(
            "Net · 333 kS",
            "Narrow",
            vec![2409.75],
            333e3,
            QPSK_3_5,
            "V3 note 3; QPSK 3/5, short frames, no pilots as received on the air",
        )),
        short_no_pilots(p(
            "Narrow · 333 kS",
            "Narrow",
            high_first(narrow.clone()),
            333e3,
            QPSK_3_5,
            "V3 §4 Narrow (14 slots); QPSK 3/5, short frames, no pilots as received on the air",
        )),
        p(
            "Narrow · 250 kS",
            "Narrow",
            high_first(narrow),
            250e3,
            QPSK_2_3,
            "V3 §4 Narrow (333 kS and 250 kS share the 14 slots)",
        ),
        short_no_pilots(p(
            "Narrow · 500 kS",
            "Wide and Narrow",
            narrow500,
            500e3,
            QPSK_3_5,
            "V3 §4 Narrow (the lower 9 slots) and note 1; QPSK 3/5, short frames, no pilots as received on the air",
        )),
        p(
            "Very narrow · 125 kS",
            "Very narrow",
            high_first(very.clone()),
            125e3,
            QPSK_1_2,
            "V3 §4 Very Narrow (27 slots, 125/66/33 kS)",
        ),
        p(
            "Very narrow · 66 kS",
            "Very narrow",
            high_first(very.clone()),
            66e3,
            QPSK_1_2,
            "V3 §4 Very Narrow",
        ),
        p(
            "Very narrow · 33 kS",
            "Very narrow",
            high_first(very),
            33e3,
            QPSK_1_2,
            "V3 §4 Very Narrow",
        ),
        p(
            "Wide · 1000 kS",
            "Wide",
            wide.clone(),
            1e6,
            QPSK_2_3,
            "V3 §4 Wide (1 MS)",
        ),
        p(
            "Wide · 1500 kS",
            "Wide",
            vec![2405.25, 2406.75],
            1.5e6,
            QPSK_2_3,
            "V3 §4 Wide notes and Transmission Modes (1500 kS the routine maximum)",
        ),
        // [CONF]: "uplink 2405 MHz, downlink 10,494.5 MHz with a 0.2 roll
        // off", DVB-S2 QPSK, 1.5 MS, FEC 2/3.
        p(
            "Conference · 1500 kS",
            "Wide",
            vec![2405.0],
            1.5e6,
            QPSK_2_3,
            "BATC conference page: 2405.0 MHz, 1.5 MS, QPSK 2/3, 0.2 roll-off",
        ),
    ]
}

/// The downlink a receiver tunes to for `uplink_mhz`.
pub fn downlink_mhz(uplink_mhz: f64) -> f64 {
    uplink_mhz + DOWNLINK_OFFSET_MHZ
}

/// Why a carrier cannot go at `uplink_mhz` at all: off the transponder or on
/// the beacon. A retune while transmitting is refused for these.
pub fn refused(uplink_mhz: f64, symbol_rate: f64, roll_off: RollOff) -> Option<String> {
    let half = symbol_rate * (1.0 + roll_off.as_f64()) / 2e6;
    if uplink_mhz - half < UPLINK_MHZ.0 || uplink_mhz + half > UPLINK_MHZ.1 {
        return Some(format!(
            "outside the wideband transponder ({}–{} MHz uplink)",
            UPLINK_MHZ.0, UPLINK_MHZ.1
        ));
    }
    // The beacon occupies 1.5 MS × 1.2 around 2402.0 (its roll-off unknown;
    // 0.2 taken).
    let beacon_half = 1.5 * 1.2 / 2.0;
    if (uplink_mhz - BEACON_UPLINK_MHZ).abs() < half + beacon_half {
        return Some("on the beacon".into());
    }
    None
}

/// What is wrong, if anything, with sending `symbol_rate` at `uplink_mhz`
/// on QO-100 by [V3]; `None` when it fits the bandplan.
pub fn check(uplink_mhz: f64, symbol_rate: f64, roll_off: RollOff) -> Option<String> {
    if let Some(why) = refused(uplink_mhz, symbol_rate, roll_off) {
        return Some(why);
    }
    let half = symbol_rate * (1.0 + roll_off.as_f64()) / 2e6;
    if symbol_rate > MAX_ROUTINE_SYMBOL_RATE {
        return Some("above 1500 kS, the bandplan's routine maximum".into());
    }
    // [V3] Bandplan note 1: above 333 kS only in the Wide and Narrow
    // section, which ends at 2407.5 MHz uplink ([V3] graphic).
    if symbol_rate > 333e3 + 1.0 && uplink_mhz + half > 2407.5 {
        return Some("above 333 kS belongs in the Wide and Narrow DATV section".into());
    }
    let down = downlink_mhz(uplink_mhz);
    let (m0, m1) = MAINTENANCE_DOWNLINK_MHZ;
    if down + half > m0 && down - half < m1 {
        return Some("inside the maintenance uplink's band (give way when it is used)".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_net_is_where_the_bandplan_puts_it() {
        let net = &presets()[0];
        assert_eq!(net.uplinks_mhz, vec![2409.75]);
        assert!((downlink_mhz(2409.75) - 10499.25).abs() < 1e-9);
        assert_eq!(net.symbol_rate, 333e3);
        assert!(!net.pilots && net.short_fecframe);
        // QPSK 3/5, short, no pilots: 9472 bits a 8190-symbol PLFRAME, the
        // 385.1 kbit/s received on the air.
        assert!(
            (net.ts_rate() - 385_128.0).abs() < 100.0,
            "{}",
            net.ts_rate()
        );
    }

    #[test]
    fn five_hundred_ks_is_what_was_received() {
        let ps = presets();
        let n500 = ps.iter().find(|p| p.name == "Narrow · 500 kS").unwrap();
        assert!(
            (n500.ts_rate() - 578_266.0).abs() < 100.0,
            "{}",
            n500.ts_rate()
        );
    }

    #[test]
    fn every_preset_fits_the_bandplan_on_its_first_slot() {
        for p in presets() {
            assert!(
                p.uplinks_mhz
                    .iter()
                    .all(|&f| (2401.0..=2410.0).contains(&f)),
                "{}",
                p.name
            );
            let f = p.uplinks_mhz[0];
            let why = check(f, p.symbol_rate, p.roll_off);
            // The wide channels sit in the maintenance band by design.
            if p.segment != "Wide" {
                assert_eq!(why, None, "{} at {f}", p.name);
            }
            assert!(p.ts_rate() > 0.0, "{}", p.name);
        }
    }

    #[test]
    fn the_slots_are_the_bandplans() {
        let ps = presets();
        let narrow = ps.iter().find(|p| p.name == "Narrow · 333 kS").unwrap();
        assert_eq!(narrow.uplinks_mhz.len(), 14);
        assert_eq!(narrow.uplinks_mhz[0], 2409.75);
        assert_eq!(*narrow.uplinks_mhz.last().unwrap(), 2403.25);
        let very = ps
            .iter()
            .find(|p| p.name == "Very narrow · 125 kS")
            .unwrap();
        assert_eq!(very.uplinks_mhz.len(), 27);
        let n500 = ps.iter().find(|p| p.name == "Narrow · 500 kS").unwrap();
        assert_eq!(n500.uplinks_mhz.len(), 9);
        assert_eq!(*n500.uplinks_mhz.last().unwrap(), 2407.25);
    }

    #[test]
    fn checks_catch_the_beacon_and_the_edges() {
        assert!(check(2402.0, 333e3, RollOff::R20).is_some());
        assert!(check(2409.95, 333e3, RollOff::R20).is_some());
        assert!(check(2409.25, 500e3, RollOff::R20).is_some());
        assert!(check(2405.25, 2e6, RollOff::R20).is_some());
        assert_eq!(check(2409.75, 333e3, RollOff::R20), None);
    }
}
