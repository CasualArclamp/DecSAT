//! The few settings that outlive a session: one `key=value` per line in
//! `%APPDATA%\DecSAT\prefs.txt` (`~/.config/DecSAT/prefs.txt` elsewhere).
//! Unknown keys are kept as they are, so an older build does not drop what a
//! newer one wrote. DecSAT was DecDVB until 2026-10-10: its settings, and
//! its output folder while it holds the recordings, carry over.

use std::path::PathBuf;
use std::sync::Mutex;

/// The output folder in use: recordings, symbol files, PCAP and TS files.
static OUTPUT_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
/// The receiver's clock correction, ppm (see `VfoSettings::clock_ppm`).
static CLOCK_PPM: Mutex<f64> = Mutex::new(0.0);

/// Where per-user settings live: %APPDATA%, or ~/.config.
fn config_base() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

fn prefs_path() -> Option<PathBuf> {
    Some(settings_dir()?.join("prefs.txt"))
}

/// DecSAT's per-user folder: the settings, and caches (map tiles).
pub fn settings_dir() -> Option<PathBuf> {
    Some(config_base()?.join("DecSAT"))
}

/// The settings key for a radar's place, by its VFO's name.
fn radar_site_key(vfo: &str) -> String {
    format!("radar_site.{}", vfo.replace('=', "_"))
}

/// Where a feed's radar is, for feeds that do not say (the BSC feed), as
/// the user set it for the VFO: (latitude, longitude). Kept in the user's
/// settings only — a radar's place is theirs to know, not the project's.
pub fn radar_site(vfo: &str) -> Option<(f64, f64)> {
    let key = radar_site_key(vfo);
    let all = read_all();
    let v = &all.iter().find(|(k, _)| *k == key)?.1;
    let (lat, lon) = v.split_once(',')?;
    Some((lat.trim().parse().ok()?, lon.trim().parse().ok()?))
}

/// Set (or with `None`, forget) a radar's place for a VFO.
pub fn set_radar_site(vfo: &str, site: Option<(f64, f64)>) -> std::io::Result<()> {
    let key = radar_site_key(vfo);
    match site {
        Some((lat, lon)) => write_key(&key, &format!("{lat:.6},{lon:.6}")),
        None => {
            let mut all = read_all();
            all.retain(|(k, _)| *k != key);
            write_all(&all)
        }
    }
}

/// The scope's map: shown, its style, its brightness.
pub fn radar_map() -> (bool, String, f32) {
    let all = read_all();
    let get = |key: &str| all.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    (
        get("radar_map").is_none_or(|v| v == "1"),
        get("radar_map_style").unwrap_or_default(),
        get("radar_map_brightness")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.8),
    )
}

/// Airports and runways on the scope (on unless turned off).
pub fn radar_airports() -> bool {
    read_all()
        .iter()
        .find(|(k, _)| k == "radar_airports")
        .is_none_or(|(_, v)| v == "1")
}

pub fn set_radar_airports(on: bool) {
    let _ = write_key("radar_airports", if on { "1" } else { "0" });
}

pub fn set_radar_map(on: bool, style: &str, brightness: f32) {
    let _ = write_key("radar_map", if on { "1" } else { "0" });
    let _ = write_key("radar_map_style", style);
    let _ = write_key("radar_map_brightness", &format!("{brightness:.2}"));
}

/// The first time DecSAT runs, take DecDVB's settings over (a copy: the old
/// file stays for an old build).
fn take_over_old_prefs() {
    let (Some(new), Some(base)) = (prefs_path(), config_base()) else {
        return;
    };
    let old = base.join("DecDVB").join("prefs.txt");
    if !new.exists() && old.exists() {
        if let Some(dir) = new.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::copy(old, new);
    }
}

fn read_all() -> Vec<(String, String)> {
    let Some(p) = prefs_path() else {
        return Vec::new();
    };
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            l.split_once('=')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

fn write_all(all: &[(String, String)]) -> std::io::Result<()> {
    let Some(p) = prefs_path() else {
        return Ok(());
    };
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text: String = all.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    std::fs::write(p, text)
}

fn write_key(key: &str, value: &str) -> std::io::Result<()> {
    let mut all = read_all();
    match all.iter_mut().find(|(k, _)| k == key) {
        Some(e) => e.1 = value.to_string(),
        None => all.push((key.to_string(), value.to_string())),
    }
    write_all(&all)
}

/// The last session's keys: the radio, the file source, the VFOs.
fn is_session(key: &str) -> bool {
    key.starts_with("radio.") || key.starts_with("source.") || key.starts_with("vfo.")
}

/// The session saved last time, as key/value pairs.
pub fn session() -> Vec<(String, String)> {
    read_all()
        .into_iter()
        .filter(|(k, _)| is_session(k))
        .collect()
}

/// Replace the saved session with `pairs` (the other settings stay).
pub fn save_session(pairs: &[(String, String)]) -> std::io::Result<()> {
    let mut all = read_all();
    all.retain(|(k, _)| !is_session(k));
    all.extend(pairs.iter().cloned());
    write_all(&all)
}

/// The transmit panel's settings (`tx.*`; never its power or armed state).
pub fn tx_settings() -> Vec<(String, String)> {
    read_all()
        .into_iter()
        .filter(|(k, _)| k.starts_with("tx."))
        .collect()
}

/// Replace the saved transmit settings with `pairs`.
pub fn save_tx(pairs: &[(String, String)]) -> std::io::Result<()> {
    let mut all = read_all();
    all.retain(|(k, _)| !k.starts_with("tx."));
    all.extend(pairs.iter().cloned());
    write_all(&all)
}

/// Documents\DecSAT, or the temp dir if there is no home.
fn fallback_dir() -> PathBuf {
    let Some(docs) = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|b| PathBuf::from(b).join("Documents"))
    else {
        return std::env::temp_dir();
    };
    // Keep writing beside DecDVB's recordings rather than start a second
    // folder.
    let (new, old) = (docs.join("DecSAT"), docs.join("DecDVB"));
    if !new.exists() && old.is_dir() {
        old
    } else {
        new
    }
}

/// Load the saved output folder (call once at start).
pub fn load() {
    take_over_old_prefs();
    let saved = read_all()
        .into_iter()
        .find(|(k, _)| k == "output_dir")
        .map(|(_, v)| PathBuf::from(v))
        .filter(|p| !p.as_os_str().is_empty());
    *OUTPUT_DIR.lock().unwrap() = saved;
    let all = read_all();
    let get = |key: &str| all.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
    if let Some(v) = get("volume").and_then(|v| v.parse::<f32>().ok()) {
        decsat_audio::set_volume(v);
    }
    decsat_audio::set_muted(get("muted") == Some("1"));
    // The sound output by name; empty (or absent) for the system's default.
    decsat_audio::set_output_device(
        get("audio_device")
            .filter(|d| !d.is_empty())
            .map(str::to_string),
    );
    // One voice at a time (the hold, s), or "off": everyone at once.
    match get("voice_hold") {
        Some("off") => decsat_engine::floor::set_hold(None),
        Some(v) => {
            if let Ok(s) = v.parse::<f32>() {
                decsat_engine::floor::set_hold(Some(s));
            }
        }
        None => {}
    }
    if let Some(v) = get("clock_ppm").and_then(|v| v.parse::<f64>().ok()) {
        *CLOCK_PPM.lock().unwrap() = v;
    }
}

/// The clock correction, ppm.
pub fn clock_ppm() -> f64 {
    *CLOCK_PPM.lock().unwrap()
}

/// Use (and remember) a clock correction of `ppm`.
pub fn set_clock_ppm(ppm: f64) {
    *CLOCK_PPM.lock().unwrap() = ppm;
    let _ = write_key("clock_ppm", &format!("{ppm:.2}"));
}

/// Remember the audio volume and mute as they are now.
pub fn save_audio() {
    let _ = write_key("volume", &format!("{:.3}", decsat_audio::volume()));
    let _ = write_key("muted", if decsat_audio::muted() { "1" } else { "0" });
    let _ = write_key(
        "audio_device",
        &decsat_audio::output_device().unwrap_or_default(),
    );
    let hold = decsat_engine::floor::hold().map_or("off".into(), |s| format!("{s:.1}"));
    let _ = write_key("voice_hold", &hold);
}

/// The frequency plan file in use (the user's own: only its path is kept).
pub fn freqplan_path() -> Option<PathBuf> {
    read_all()
        .into_iter()
        .find(|(k, _)| k == "freqplan")
        .map(|(_, v)| PathBuf::from(v))
        .filter(|p| !p.as_os_str().is_empty())
}

/// Use `path` as the frequency plan from now on (`None`: no plan).
pub fn set_freqplan_path(path: Option<&std::path::Path>) -> std::io::Result<()> {
    write_key(
        "freqplan",
        &path
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    )
}

/// Where new output goes.
pub fn output_dir() -> PathBuf {
    OUTPUT_DIR
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(fallback_dir)
}

/// Use `dir` from now on, and remember it.
pub fn set_output_dir(dir: PathBuf) -> std::io::Result<()> {
    write_key("output_dir", &dir.to_string_lossy())?;
    *OUTPUT_DIR.lock().unwrap() = Some(dir);
    Ok(())
}
