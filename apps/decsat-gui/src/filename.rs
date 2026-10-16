// made by CasualArclamp
//! Guess a capture's sample rate, centre frequency and format from its name
//! — or, for a two-channel WAV file, read them from its header.
//!
//! Raw IQ files carry no metadata, but most tools encode it in the file name:
//! - gqrx: `gqrx_20261008_120000_10489750000_2000000_fc.raw` (freq, rate, cf32)
//! - SDR++ and others: `…_10489750000Hz_…`, `…_2Msps_…`, `…_500ksps_…`
//! - DecSAT's own recordings (`decdvb-` before the rename):
//!   `decsat-VFO_1-12346.0340MHz-333333Sps-….cf32` — the VFO's RF, which is
//!   the recording's centre — or, when the RF was not known (and before
//!   0.8.1), `decsat-VFO_1-+250000Hz-…`: a signed figure is an offset from
//!   the radio's tuning, not a centre, and is ignored.

use std::path::Path;

use decsat_core::SampleFormat;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Guess {
    pub rate: Option<f64>,
    pub center: Option<f64>,
    pub format: Option<SampleFormat>,
}

/// Parse `2`, `2.5`, `2m`, `500k`, `8000000` into a number.
fn number_with_prefix(s: &str) -> Option<f64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()? {
        'k' => (&s[..s.len() - 1], 1e3),
        'm' => (&s[..s.len() - 1], 1e6),
        'g' => (&s[..s.len() - 1], 1e9),
        _ => (s, 1.0),
    };
    num.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| v * mult)
}

pub fn guess(path: &Path) -> Guess {
    let named = guess_from_name(path);
    // A WAV header is authoritative for format and rate, and for the centre
    // when it has an auxi chunk; HDSDR-style names give the centre otherwise.
    match decsat_io::probe_capture(path) {
        Ok(info) if info.wav => Guess {
            rate: info.sample_rate,
            center: info.center_freq.or(named.center),
            format: info.format,
        },
        _ => named,
    }
}

fn guess_from_name(path: &Path) -> Guess {
    let mut g = Guess {
        format: decsat_io::format_from_path(path),
        ..Guess::default()
    };
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return g;
    };
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let tokens: Vec<String> = stem
        .split(['_', '-', ' '])
        .filter(|t| !t.is_empty())
        .map(|t| t.to_ascii_lowercase())
        .collect();

    // gqrx: ..._<freq>_<rate>_fc
    if let Some(i) = tokens.iter().position(|t| t == "fc")
        && i >= 2
        && let (Ok(f), Ok(r)) = (tokens[i - 2].parse::<f64>(), tokens[i - 1].parse::<f64>())
    {
        g.center = Some(f);
        g.rate = Some(r);
        if ext == "raw" {
            g.format = Some(SampleFormat::Cf32);
        }
        return g;
    }

    for t in &tokens {
        if let Some(n) = t.strip_suffix("sps").or_else(|| t.strip_suffix("s/s")) {
            if let Some(v) = number_with_prefix(n) {
                g.rate = Some(v);
            }
        } else if let Some(n) = t.strip_suffix("hz") {
            // A signed figure is an offset, not a centre frequency.
            if !n.starts_with(['+', '-'])
                && let Some(v) = number_with_prefix(n)
                && v >= 1e6
            {
                g.center = Some(v);
            }
        }
    }
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gqrx_names() {
        let g = guess(Path::new("gqrx_20261008_120000_10489750000_2000000_fc.raw"));
        assert_eq!(g.center, Some(10_489_750_000.0));
        assert_eq!(g.rate, Some(2_000_000.0));
        assert_eq!(g.format, Some(SampleFormat::Cf32));
    }

    #[test]
    fn hz_and_sps_tokens() {
        let g = guess(Path::new("qo100_10491500000Hz_2Msps.cs8"));
        assert_eq!(g.center, Some(10_491_500_000.0));
        assert_eq!(g.rate, Some(2e6));
        assert_eq!(g.format, Some(SampleFormat::Cs8));

        let g = guess(Path::new("carrier 500ksps.cf32"));
        assert_eq!(g.rate, Some(500e3));
    }

    #[test]
    fn own_recordings_ignore_the_signed_offset() {
        // Its recordings from before the rename (DecDVB) read the same.
        for name in [
            "decsat-VFO_1-+250000Hz-333333Sps-1791000000.cf32",
            "decdvb-VFO_1-+250000Hz-333333Sps-1791000000.cf32",
        ] {
            let g = guess(Path::new(name));
            assert_eq!(g.rate, Some(333_333.0));
            assert_eq!(g.center, None);
        }
    }

    #[test]
    fn own_recordings_named_by_rf_give_the_centre() {
        let g = guess(Path::new(
            "decsat-ASA245_LHI-BN-12346.0340MHz-333333Sps-1791620000.cf32",
        ));
        assert_eq!(g.rate, Some(333_333.0));
        assert_eq!(g.center, Some(12_346_034_000.0));
    }

    #[test]
    fn hdsdr_wav_names_give_the_centre() {
        // HDSDR: HDSDR_<date>_<time>Z_<freq>kHz_RF.wav (the file need not
        // exist: the name alone is read when there is no header).
        let g = guess(Path::new("HDSDR_20261010_101500Z_1635640kHz_RF.wav"));
        assert_eq!(g.center, Some(1_635_640_000.0));
    }

    #[test]
    fn nothing_to_go_on() {
        let g = guess(Path::new("capture.bin"));
        assert_eq!(g, Guess::default());
    }
}
