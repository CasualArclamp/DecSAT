// made by CasualArclamp
//! What is remembered between runs: the HackRF panel, the file source's
//! format, rate and centre, and the VFOs — kept in `prefs.txt` as
//! `radio.*`, `source.*` and `vfo.N` keys (see `prefs`). A VFO is one line
//! of `key=value` pairs joined by `;`.

use decsat_core::{Modulation, SampleFormat};
use decsat_engine::{DecoderKind, VfoSettings};

/// Every modulation, to read one back by its name.
const MODULATIONS: [Modulation; 13] = [
    Modulation::Bpsk,
    Modulation::Pi2Bpsk,
    Modulation::Qpsk,
    Modulation::Psk8,
    Modulation::Apsk8,
    Modulation::Apsk16,
    Modulation::Apsk32,
    Modulation::Apsk64,
    Modulation::Apsk128,
    Modulation::Apsk256,
    Modulation::Qam8,
    Modulation::Qam16,
    Modulation::Qam64,
];

/// A VFO as one line.
pub fn vfo_text(s: &VfoSettings) -> String {
    let mut f = vec![
        format!("name={}", s.name.replace([';', '\n', '\r'], " ")),
        format!("offset={}", s.offset_hz),
        format!("bw={}", s.bandwidth_hz),
        format!("decoder={}", s.decoder.key()),
        format!("mode={}", s.signal.key()),
        format!("enabled={}", u8::from(s.enabled)),
        format!("follow={}", u8::from(s.follow_carrier)),
        format!("voice={}", u8::from(s.voice_auto)),
        format!("gold={}", s.gold_code),
        format!("activity={}", u8::from(s.record_on_activity)),
        format!("lowsnr={}", u8::from(s.cid_low_snr)),
    ];
    if let Some(rs) = s.symbol_rate {
        f.push(format!("rs={rs}"));
    }
    if let Some(m) = s.psk_modulation {
        f.push(format!("mod={m:?}"));
    }
    f.join(";")
}

/// A VFO back from its line (its outputs going to `record_dir`); `None`
/// if the line is not one.
pub fn vfo_from_text(t: &str, record_dir: std::path::PathBuf) -> Option<VfoSettings> {
    // Rust note: the closure borrows `t`; `split_once` keeps any `=` in
    // the value (a name may have one).
    let get = |key: &str| {
        t.split(';')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    };
    let mut s = VfoSettings::new(
        get("name")?,
        get("offset")?.parse().ok()?,
        get("bw")?.parse().ok()?,
        DecoderKind::from_key(get("decoder")?)?,
    );
    // (Sessions before the SCPC/TDMA modes: the decoder's.)
    if let Some(m) = get("mode").and_then(decsat_engine::SignalMode::from_key) {
        s.signal = m;
    }
    let flag = |key: &str, default: bool| get(key).map_or(default, |v| v == "1");
    s.enabled = flag("enabled", true);
    s.follow_carrier = flag("follow", true);
    s.voice_auto = flag("voice", true);
    s.record_on_activity = flag("activity", false);
    s.cid_low_snr = flag("lowsnr", false);
    s.gold_code = get("gold").and_then(|v| v.parse().ok()).unwrap_or(0);
    s.symbol_rate = get("rs").and_then(|v| v.parse().ok());
    s.psk_modulation =
        get("mod").and_then(|v| MODULATIONS.into_iter().find(|m| format!("{m:?}") == v));
    s.record_dir = record_dir;
    Some(s)
}

/// A sample format's name, and back.
pub fn format_name(f: SampleFormat) -> &'static str {
    match f {
        SampleFormat::Cs8 => "cs8",
        SampleFormat::Cu8 => "cu8",
        SampleFormat::Cs16 => "cs16",
        SampleFormat::Cs24 => "cs24",
        SampleFormat::Cf32 => "cf32",
    }
}

pub fn format_by_name(name: &str) -> Option<SampleFormat> {
    [
        SampleFormat::Cs8,
        SampleFormat::Cu8,
        SampleFormat::Cs16,
        SampleFormat::Cs24,
        SampleFormat::Cf32,
    ]
    .into_iter()
    .find(|&f| format_name(f) == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vfo_survives_its_line() {
        let mut s = VfoSettings::new(
            "CDM; site 2",
            5_844_351.0,
            150_000.0,
            DecoderKind::Cdm600Voice,
        );
        s.symbol_rate = Some(43_615.6);
        s.psk_modulation = Some(Modulation::Qpsk);
        s.follow_carrier = false;
        s.gold_code = 3;
        let dir = std::path::PathBuf::from("out");
        let back = vfo_from_text(&vfo_text(&s), dir.clone()).unwrap();
        let mut want = s.clone();
        want.name = "CDM  site 2".into();
        want.record_dir = dir;
        assert_eq!(back, want);
        assert!(vfo_from_text("nonsense", std::path::PathBuf::new()).is_none());
    }

    #[test]
    fn the_signal_mode_is_kept_and_old_lines_take_the_decoders() {
        // A TDMA VFO recording its IQ: the mode is its own, not the tool's.
        let mut s = VfoSettings::new("group", 1e5, 2e5, DecoderKind::IqRecord);
        s.signal = decsat_engine::SignalMode::Tdma;
        let back = vfo_from_text(&vfo_text(&s), std::path::PathBuf::new()).unwrap();
        assert_eq!(back.signal, decsat_engine::SignalMode::Tdma);
        // A line from before the modes: TDMA if its decoder is the burst
        // analyser, else SCPC.
        let old = "name=a;offset=0;bw=1000;decoder=tdma;enabled=1";
        let back = vfo_from_text(old, std::path::PathBuf::new()).unwrap();
        assert_eq!(back.signal, decsat_engine::SignalMode::Tdma);
        let old = "name=b;offset=0;bw=1000;decoder=viterbi;enabled=1";
        let back = vfo_from_text(old, std::path::PathBuf::new()).unwrap();
        assert_eq!(back.signal, decsat_engine::SignalMode::Scpc);
    }
}
