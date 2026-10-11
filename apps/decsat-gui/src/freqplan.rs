//! A frequency plan: named carriers at RF frequencies, read from an SDR++
//! frequency-manager bookmark list —
//! `{"bookmarks": {"name": {"frequency": Hz, "bandwidth": Hz, "mode": n}}}`.
//! A carrier may also carry `"color": "#rrggbb"`, DecSAT's own field for the
//! colour of its label (SDR++ ignores it, and drops it if it rewrites the
//! list). The waterfall labels the carriers it shows, and a VFO made on one
//! takes its name (and, made from the label, its width). The plan file is the
//! user's own: its path is remembered in the prefs, nothing of it is kept
//! anywhere else.

use std::path::Path;

/// One carrier of the plan.
#[derive(Debug, Clone, PartialEq)]
pub struct Bookmark {
    pub name: String,
    /// RF centre, Hz.
    pub freq_hz: f64,
    pub bandwidth_hz: f64,
    /// Label colour (RGB); `None` draws it in the plan's own colour.
    pub color: Option<[u8; 3]>,
}

/// The plan in `path`, sorted by frequency.
pub fn load(path: &Path) -> Result<Vec<Bookmark>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse(&text)
}

/// An SDR++ bookmark list's carriers, sorted by frequency.
pub fn parse(text: &str) -> Result<Vec<Bookmark>, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not a JSON bookmark list: {e}"))?;
    let marks = v
        .get("bookmarks")
        .and_then(|b| b.as_object())
        .ok_or("no \"bookmarks\" in the file (an SDR++ frequency-manager list?)")?;
    let mut out: Vec<Bookmark> = marks
        .iter()
        .filter_map(|(name, m)| {
            Some(Bookmark {
                name: name.trim().to_string(),
                freq_hz: m.get("frequency")?.as_f64()?,
                bandwidth_hz: m.get("bandwidth").and_then(|b| b.as_f64()).unwrap_or(0.0),
                // A colour that is missing or malformed is no colour, not an
                // error: the carrier still belongs in the plan.
                color: m.get("color").and_then(|c| c.as_str()).and_then(rgb),
            })
        })
        .collect();
    out.sort_by(|a, b| a.freq_hz.total_cmp(&b.freq_hz));
    Ok(out)
}

/// `"#rrggbb"` (the `#` optional) as RGB.
fn rgb(s: &str) -> Option<[u8; 3]> {
    let h = s.trim().trim_start_matches('#');
    // `from_str_radix` alone would also take a sign, so check the digits.
    if h.len() != 6 || !h.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

/// The plan's carrier a VFO centred at `rf_hz`, `bw_hz` wide, sits on: the
/// nearest whose centre lies inside the VFO (or the VFO's centre inside it).
pub fn at(plan: &[Bookmark], rf_hz: f64, bw_hz: f64) -> Option<&Bookmark> {
    plan.iter()
        .filter(|b| (b.freq_hz - rf_hz).abs() <= (bw_hz / 2.0).max(b.bandwidth_hz / 2.0))
        .min_by(|a, b| {
            (a.freq_hz - rf_hz)
                .abs()
                .total_cmp(&(b.freq_hz - rf_hz).abs())
        })
}

/// The LO trim (Hz, added to the RF scale) that lines detected carriers up
/// with the plan: `carriers` are their (untrimmed) RF centres and widths.
/// Each pairing of a carrier with a plan carrier of like width (0.6–1.7×)
/// within `search` of `around` proposes a trim; the trim proposed by the
/// most carriers within `tol` of each other wins (their median). With the
/// number of carriers that agree; `None` when fewer than two do (one, if
/// only one carrier is seen).
pub fn align(
    plan: &[Bookmark],
    carriers: &[(f64, f64)],
    around: f64,
    search: f64,
    tol: f64,
) -> Option<(f64, usize)> {
    // (trim, carrier index)
    let mut cand: Vec<(f64, usize)> = Vec::new();
    for (i, &(rf, bw)) in carriers.iter().enumerate() {
        for b in plan {
            let t = b.freq_hz - rf;
            let ratio = bw / b.bandwidth_hz.max(1.0);
            if (t - around).abs() <= search && (0.6..=1.7).contains(&ratio) {
                cand.push((t, i));
            }
        }
    }
    cand.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut best: Option<(usize, f64)> = None;
    for (k, &(t0, _)) in cand.iter().enumerate() {
        let window: Vec<&(f64, usize)> = cand[k..].iter().take_while(|c| c.0 - t0 <= tol).collect();
        let mut who: Vec<usize> = window.iter().map(|c| c.1).collect();
        who.sort_unstable();
        who.dedup();
        let n = who.len();
        if best.is_none_or(|(m, _)| n > m) {
            best = Some((n, window[window.len() / 2].0));
        }
    }
    let need = if carriers.len() == 1 { 1 } else { 2 };
    best.filter(|&(n, _)| n >= need).map(|(n, t)| (t, n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lo_trim_that_lines_carriers_up_with_the_plan() {
        let b = |f: f64, w: f64| Bookmark {
            name: String::new(),
            freq_hz: f,
            bandwidth_hz: w,
            color: None,
        };
        // A plan of CDM-600-like carriers 59 kHz apart and a wider Q-Flex.
        let plan = vec![
            b(12_352_038_770.0, 58_837.0),
            b(12_352_097_820.0, 58_837.0),
            b(12_352_156_870.0, 58_837.0),
            b(12_352_215_920.0, 58_837.0),
            b(12_359_096_300.0, 114_019.0),
        ];
        // Three of them seen with an LNB 7.3 kHz low (they read 7.3 kHz
        // low), give or take a bin, and a stray carrier the plan lacks.
        let seen = [
            (12_352_038_770.0 - 7_300.0 + 400.0, 57_000.0),
            (12_352_156_870.0 - 7_300.0 - 300.0, 60_000.0),
            (12_359_096_300.0 - 7_300.0 + 100.0, 110_000.0),
            (12_355_000_000.0, 50_000.0),
        ];
        let (t, n) = align(&plan, &seen, 0.0, 3e6, 2_000.0).unwrap();
        assert_eq!(n, 3);
        assert!((t - 7_300.0).abs() < 500.0, "{t}");
        // Nothing to line up with: no answer.
        assert!(
            align(
                &plan,
                &[(13e9, 58_000.0), (13.1e9, 58_000.0)],
                0.0,
                3e6,
                2e3
            )
            .is_none()
        );
    }

    #[test]
    fn reads_an_sdrpp_bookmark_list() {
        let plan = parse(
            r##"{"bookmarks":{"Site B":{"bandwidth":58837.25,"frequency":12352038770.0,"mode":1,"color":"#5aa0ff"},
                "Site A ":{"bandwidth":15000.0,"frequency":12351836670.0,"mode":2,"color":"+12345"},
                "no frequency":{"bandwidth":1.0}}}"##,
        )
        .unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].name, "Site A");
        assert_eq!(plan[1].freq_hz, 12_352_038_770.0);
        // A colour where given; a malformed one is none.
        assert_eq!(plan[1].color, Some([0x5a, 0xa0, 0xff]));
        assert_eq!(plan[0].color, None);
        assert_eq!(rgb("6ED76E"), Some([0x6e, 0xd7, 0x6e]));
        // A VFO near Site B's centre is on it; one between them on neither.
        assert_eq!(
            at(&plan, 12_352_040_000.0, 40_000.0).unwrap().name,
            "Site B"
        );
        assert!(at(&plan, 12_351_940_000.0, 20_000.0).is_none());
        assert!(parse("{}").is_err() && parse("nonsense").is_err());
    }
}
