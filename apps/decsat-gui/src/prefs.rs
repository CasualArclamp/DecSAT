// made by CasualArclamp
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

/// Radar stations' radars as chosen in the scope: the station (its
/// multicast group:port) and the SAC/SIC of the feed chosen for it. Read
/// once, then kept here: the scope and the main window ask every frame.
static STATION_RADARS: Mutex<Option<Vec<StationRadar>>> = Mutex::new(None);

/// A station (`group:port`) and its radar's SAC and SIC.
type StationRadar = (String, (u8, u8));

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

/// The settings key a radar's place was kept under before places were kept
/// by radar (2026-10-12): its VFO's name.
fn radar_site_key(vfo: &str) -> String {
    format!("radar_site.{}", vfo.replace('=', "_"))
}

/// Which radar a place belongs to — the radar itself, not the VFO showing
/// it, so that going to another radar (another VFO, a retune, a VFO named
/// as another was yesterday) brings that radar's own place. Places are
/// kept in the user's settings only: a radar's place is theirs to know,
/// not the project's.
#[derive(Debug, Clone, PartialEq)]
pub enum RadarId {
    /// An ASTERIX source (a radar, an ADS-B ground station): SAC and SIC.
    Source(u8, u8),
    /// A feed that does not say who it is (the BSC feed): its carrier's
    /// RF, Hz.
    Carrier(f64),
    /// Not even the RF known (a recording named by its offset): the VFO's
    /// name, as before.
    Vfo(String),
}

/// A carrier kept within this of another is the same carrier (an LO trim,
/// a VFO set a little off, the drift it follows), Hz: the band's radar
/// carriers sit tens of kHz apart.
pub const SAME_CARRIER_HZ: f64 = 10e3;

impl RadarId {
    /// Its settings key (a carrier's: its RF to the kHz).
    fn key(&self) -> String {
        match self {
            RadarId::Source(sac, sic) => format!("radar_site.sac{sac}.sic{sic}"),
            RadarId::Carrier(hz) => format!("radar_site.rf{:.0}khz", hz / 1e3),
            RadarId::Vfo(name) => radar_site_key(name),
        }
    }

    /// The same radar: a carrier within half the tolerance (so a VFO
    /// following its carrier's drift is not a new radar every frame).
    pub fn same(&self, other: &RadarId) -> bool {
        match (self, other) {
            (RadarId::Carrier(a), RadarId::Carrier(b)) => (a - b).abs() <= SAME_CARRIER_HZ / 2.0,
            _ => self == other,
        }
    }

    /// For display: "SAC 4 SIC 22", "the 12328.083 MHz carrier", "VFO 1".
    pub fn label(&self) -> String {
        match self {
            RadarId::Source(sac, sic) => format!("SAC {sac} SIC {sic}"),
            RadarId::Carrier(hz) => format!("the {:.3} MHz carrier", hz / 1e6),
            RadarId::Vfo(name) => name.clone(),
        }
    }
}

fn parse_site(v: &str) -> Option<(f64, f64)> {
    let (lat, lon) = v.split_once(',')?;
    Some((lat.trim().parse().ok()?, lon.trim().parse().ok()?))
}

fn site_text((lat, lon): (f64, f64)) -> String {
    format!("{lat:.6},{lon:.6}")
}

/// The key radar `id`'s place is kept under, if it is: for a carrier, the
/// nearest kept within [`SAME_CARRIER_HZ`].
fn kept_key(all: &[(String, String)], id: &RadarId) -> Option<String> {
    match id {
        RadarId::Carrier(hz) => all
            .iter()
            .filter_map(|(k, _)| {
                let khz: f64 = k
                    .strip_prefix("radar_site.rf")?
                    .strip_suffix("khz")?
                    .parse()
                    .ok()?;
                let d = (khz * 1e3 - hz).abs();
                (d <= SAME_CARRIER_HZ).then_some((d, k.clone()))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, k)| k),
        _ => {
            let key = id.key();
            all.iter().any(|(k, _)| *k == key).then_some(key)
        }
    }
}

/// Where radar `id` is, as kept: (latitude, longitude). A carrier with
/// nothing kept takes the place kept for VFO `vfo` before places were kept
/// by radar, and keeps it as its own from then on (the BSC feed's place,
/// set by hand on its VFO).
pub fn radar_site(id: &RadarId, vfo: &str) -> Option<(f64, f64)> {
    let mut all = read_all();
    if let Some(key) = kept_key(&all, id) {
        return parse_site(&all.iter().find(|(k, _)| *k == key)?.1);
    }
    if !matches!(id, RadarId::Carrier(_)) {
        return None;
    }
    let old = radar_site_key(vfo);
    let site = parse_site(&all.iter().find(|(k, _)| *k == old)?.1)?;
    all.retain(|(k, _)| *k != old);
    all.push((id.key(), site_text(site)));
    let _ = write_all(&all);
    Some(site)
}

/// Keep (or with `None`, forget) radar `id`'s place: a carrier's under the
/// key it is already kept under, when it is.
pub fn set_radar_site(id: &RadarId, site: Option<(f64, f64)>) -> std::io::Result<()> {
    let mut all = read_all();
    let key = kept_key(&all, id).unwrap_or_else(|| id.key());
    match site {
        Some(site) => write_key(&key, &site_text(site)),
        None => {
            all.retain(|(k, _)| *k != key);
            write_all(&all)
        }
    }
}

/// The radar chosen for radar station `station` (its multicast
/// `group:port`) in the scope, if one was: its SAC and SIC.
pub fn station_radar(station: &str) -> Option<(u8, u8)> {
    let mut cache = STATION_RADARS.lock().unwrap();
    // Rust note: `get_or_insert_with` reads the file the first time only.
    let all = cache.get_or_insert_with(|| {
        read_all()
            .into_iter()
            .filter_map(|(k, v)| {
                let st = k.strip_prefix("radar_station.")?.to_string();
                let (sac, sic) = v.split_once(',')?;
                Some((st, (sac.trim().parse().ok()?, sic.trim().parse().ok()?)))
            })
            .collect()
    });
    all.iter().find(|(s, _)| s == station).map(|(_, r)| *r)
}

/// Keep `radar` (SAC, SIC) as radar station `station`'s.
pub fn set_station_radar(station: &str, radar: (u8, u8)) {
    let _ = station_radar(station); // the cache filled
    if let Some(all) = STATION_RADARS.lock().unwrap().as_mut() {
        all.retain(|(s, _)| s != station);
        all.push((station.to_string(), radar));
    }
    let _ = write_key(
        &format!("radar_station.{station}"),
        &format!("{},{}", radar.0, radar.1),
    );
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

/// Callsigns from ADS-B on the scope (off unless turned on: it asks an
/// outside service).
/// What satellite `sat`'s carriers have been heard to carry
/// ([`crate::carrier_caps`]): (RF Hz, `adsb,radar`), from its
/// `carrier_caps.<satellite>.rf<kHz>khz` keys (`carrier_caps.rf<kHz>khz`
/// for none named).
pub fn carrier_caps(sat: &str) -> Vec<(f64, String)> {
    caps_in(&read_all(), sat)
}

fn caps_in(all: &[(String, String)], sat: &str) -> Vec<(f64, String)> {
    let prefix = caps_prefix(sat);
    all.iter()
        .filter_map(|(k, v)| Some((carrier_caps_hz(&prefix, k)?, v.clone())))
        .collect()
}

/// The RF (Hz) a carrier-caps key (`prefix` then `<kHz>khz`) keeps a
/// carrier at.
fn carrier_caps_hz(prefix: &str, key: &str) -> Option<f64> {
    let khz: f64 = key
        .strip_prefix(prefix)?
        .strip_suffix("khz")?
        .parse()
        .ok()?;
    Some(khz * 1e3)
}

/// Forget every carrier kept for satellite `sat` (see [`carrier_caps`]).
pub fn forget_carrier_caps(sat: &str) -> std::io::Result<()> {
    let mut all = read_all();
    drop_carrier_caps(&mut all, sat);
    write_all(&all)
}

/// [`forget_carrier_caps`] on the settings in hand: the other keys (and
/// other satellites' carriers) stay.
fn drop_carrier_caps(all: &mut Vec<(String, String)>, sat: &str) {
    let prefix = caps_prefix(sat);
    all.retain(|(k, _)| carrier_caps_hz(&prefix, k).is_none());
}

/// Keep what satellite `sat`'s carrier at `rf_hz` carries (see
/// [`carrier_caps`]).
pub fn set_carrier_caps(sat: &str, rf_hz: f64, caps: &str) -> std::io::Result<()> {
    write_key(&format!("{}{:.0}khz", caps_prefix(sat), rf_hz / 1e3), caps)
}

/// The scope may ask adsb.lol for callsigns (its "adsb.lol" switch, first
/// called "Internet"; off unless turned on; the map and airports are not
/// governed by it). Its own key: the old `radar_callsigns` switch, on for
/// the lookup before an ADS-B feed was decoded here, is no longer read.
pub fn radar_internet() -> bool {
    read_all()
        .iter()
        .any(|(k, v)| k == "radar_internet" && v == "1")
}

pub fn set_radar_internet(on: bool) {
    let _ = write_key("radar_internet", if on { "1" } else { "0" });
}

/// Whose Ku beacons the waterfall marks: "auto" (the toolbar's
/// satellite), "none", or a satellite's title from the beacon list.
pub fn beacon_marks() -> String {
    read_all()
        .into_iter()
        .find(|(k, _)| k == "beacon_marks")
        .map_or_else(|| "auto".to_string(), |(_, v)| v)
}

pub fn set_beacon_marks(v: &str) {
    let _ = write_key("beacon_marks", v);
}

/// The beacon card's pinned word plots, as the card writes them
/// ("200,1,*;43,0,7": byte, 16 bits or not, minor frame or every one).
pub fn beacon_pins() -> String {
    read_all()
        .into_iter()
        .find(|(k, _)| k == "beacon_pins")
        .map(|(_, v)| v)
        .unwrap_or_default()
}

pub fn set_beacon_pins(pins: &str) {
    let _ = write_key("beacon_pins", pins);
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

/// Satellite `sat`'s frequency plan file (the user's own: only its path
/// is kept).
pub fn freqplan_path(sat: &str) -> Option<PathBuf> {
    let key = plan_key(sat);
    read_all()
        .into_iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| PathBuf::from(v))
        .filter(|p| !p.as_os_str().is_empty())
}

/// Use `path` as satellite `sat`'s frequency plan from now on (`None`: no
/// plan).
pub fn set_freqplan_path(sat: &str, path: Option<&std::path::Path>) -> std::io::Result<()> {
    write_key(
        &plan_key(sat),
        &path
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    )
}

// Satellites. The frequency plan and the carrier labels belong to the
// satellite the dish is on: moved from Horizons 3e to Intelsat 19, the
// waterfall still named Intelsat 19's carriers after H3e's plan carriers at
// the same frequencies, and a VFO made on one took the wrong name. So each
// satellite keeps its own, and only the one chosen is shown. The plan and
// labels kept before satellites were named belong to none ("") until that
// one is named.

/// The satellites named, in order (`satellites`: names separated by `|`).
pub fn satellites() -> Vec<String> {
    names_in(&read_all())
}

fn names_in(all: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some((_, v)) = all.iter().find(|(k, _)| k == "satellites") {
        for n in v.split('|').map(str::trim).filter(|n| !n.is_empty()) {
            if !out.iter().any(|o| o == n) {
                out.push(n.to_string());
            }
        }
    }
    out
}

/// The satellite the dish is on, as last chosen ("" for none named).
pub fn satellite() -> String {
    read_all()
        .into_iter()
        .find(|(k, _)| k == "satellite")
        .map(|(_, v)| v)
        .unwrap_or_default()
}

/// The dish is on `name` now (added to the list if new; "" for none named).
pub fn set_satellite(name: &str) -> std::io::Result<()> {
    let mut all = read_all();
    choose_in(&mut all, name);
    write_all(&all)
}

fn choose_in(all: &mut Vec<(String, String)>, name: &str) {
    let mut names = names_in(all);
    if !name.is_empty() && !names.iter().any(|n| n == name) {
        names.push(name.to_string());
    }
    set_in(all, "satellites", names.join("|"));
    set_in(all, "satellite", name.to_string());
}

/// Rename satellite `old` ("" for the plan and labels kept before
/// satellites were named) to `new`: its plan and carrier labels go with it,
/// and it stays the one chosen.
pub fn rename_satellite(old: &str, new: &str) -> std::io::Result<()> {
    let mut all = read_all();
    rename_in(&mut all, old, new);
    write_all(&all)
}

fn rename_in(all: &mut Vec<(String, String)>, old: &str, new: &str) {
    let (old_plan, new_plan) = (plan_key(old), plan_key(new));
    let (old_caps, new_caps) = (caps_prefix(old), caps_prefix(new));
    let (old_biss, new_biss) = (biss_prefix(old), biss_prefix(new));
    for (k, _) in all.iter_mut() {
        if *k == old_plan {
            *k = new_plan.clone();
        } else if let Some(rest) = k.strip_prefix(&old_caps) {
            *k = format!("{new_caps}{rest}");
        } else if let Some(rest) = k.strip_prefix(&old_biss) {
            *k = format!("{new_biss}{rest}");
        }
    }
    let mut names = names_in(all);
    match names.iter().position(|n| n == old) {
        Some(i) => names[i] = new.to_string(),
        None => names.push(new.to_string()),
    }
    set_in(all, "satellites", names.join("|"));
    set_in(all, "satellite", new.to_string());
}

/// Whether a plan or carrier labels are kept for no satellite named (the
/// ones from before satellites were).
pub fn unnamed_kept() -> bool {
    let all = read_all();
    let plan = plan_key("");
    all.iter().any(|(k, v)| *k == plan && !v.is_empty()) || !caps_in(&all, "").is_empty()
}

/// `name` tidied, if it can name a new satellite: not empty, no `|` or `=`
/// (the settings file's separators), and not spelled in the keys as one of
/// `names` is.
pub fn satellite_name_ok(name: &str, names: &[String]) -> Option<String> {
    let n = name.trim();
    let taken = names.iter().any(|o| satellite_slug(o) == satellite_slug(n));
    (!n.is_empty() && !n.contains(['|', '=']) && !taken).then(|| n.to_string())
}

/// A satellite's name as its keys spell it: letters and digits in lower
/// case, anything else `_` ("Intelsat 19" → `intelsat_19`); "" for none.
fn satellite_slug(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Satellite `sat`'s plan key: `freqplan.<satellite>` (`freqplan` for none).
fn plan_key(sat: &str) -> String {
    match satellite_slug(sat).as_str() {
        "" => "freqplan".into(),
        s => format!("freqplan.{s}"),
    }
}

/// What satellite `sat`'s carrier-caps keys start with.
fn caps_prefix(sat: &str) -> String {
    match satellite_slug(sat).as_str() {
        "" => "carrier_caps.rf".into(),
        s => format!("carrier_caps.{s}.rf"),
    }
}

/// The ffmpeg the player was pointed at, if it was (otherwise it is
/// looked for: see `decsat_video::find_ffmpeg`).
pub fn ffmpeg_path() -> Option<PathBuf> {
    read_all()
        .into_iter()
        .find(|(k, _)| k == "ffmpeg")
        .map(|(_, v)| PathBuf::from(v))
        .filter(|p| !p.as_os_str().is_empty())
}

/// The in-app player window's own volume, 0..=1 (1 when never set).
pub fn tv_volume() -> f32 {
    read_all()
        .into_iter()
        .find(|(k, _)| k == "tv_volume")
        .and_then(|(_, v)| v.parse::<f32>().ok())
        .map_or(1.0, |v| v.clamp(0.0, 1.0))
}

pub fn set_tv_volume(v: f32) {
    let _ = write_key("tv_volume", &format!("{:.3}", v.clamp(0.0, 1.0)));
}

/// Whether the player deinterlaces bob ×2 (a picture per field; true when
/// never set) rather than one picture per frame.
pub fn tv_bob() -> bool {
    read_all()
        .into_iter()
        .find(|(k, _)| k == "tv_bob")
        .is_none_or(|(_, v)| v != "0")
}

pub fn set_tv_bob(bob: bool) {
    let _ = write_key("tv_bob", if bob { "1" } else { "0" });
}

/// Whether the player decodes video on the GPU (true when never set).
pub fn tv_gpu() -> bool {
    read_all()
        .into_iter()
        .find(|(k, _)| k == "tv_gpu")
        .is_none_or(|(_, v)| v != "0")
}

pub fn set_tv_gpu(gpu: bool) {
    let _ = write_key("tv_gpu", if gpu { "1" } else { "0" });
}

pub fn set_ffmpeg_path(path: Option<&std::path::Path>) -> std::io::Result<()> {
    write_key(
        "ffmpeg",
        &path.map(|p| p.display().to_string()).unwrap_or_default(),
    )
}

/// What satellite `sat`'s BISS-key keys start with.
fn biss_prefix(sat: &str) -> String {
    match satellite_slug(sat).as_str() {
        "" => "biss.rf".into(),
        s => format!("biss.{s}.rf"),
    }
}

/// A carrier within this of one a BISS key is kept for is that carrier, Hz:
/// a VFO set a little off, an LO trim. BISS carriers are megahertz wide,
/// so no two are this close.
pub const BISS_SAME_CARRIER_HZ: f64 = 300e3;

/// The key under which satellite `sat`'s carrier at `rf_hz` has a BISS key
/// kept, if it has: the nearest within [`BISS_SAME_CARRIER_HZ`].
fn biss_kept_key(all: &[(String, String)], sat: &str, rf_hz: f64) -> Option<String> {
    let prefix = biss_prefix(sat);
    all.iter()
        .filter_map(|(k, _)| {
            let d = (carrier_caps_hz(&prefix, k)? - rf_hz).abs();
            (d <= BISS_SAME_CARRIER_HZ).then_some((d, k.clone()))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, k)| k)
}

/// The BISS key kept for satellite `sat`'s carrier at `rf_hz`
/// (`biss.<satellite>.rf<kHz>khz`), as its 16 digits.
pub fn biss_key(sat: &str, rf_hz: f64) -> Option<String> {
    let all = read_all();
    let key = biss_kept_key(&all, sat, rf_hz)?;
    all.into_iter().find(|(k, _)| *k == key).map(|(_, v)| v)
}

/// Keep `key` for satellite `sat`'s carrier at `rf_hz` (under the key it is
/// already kept under, when it is), or with `None` forget it. Kept in the
/// user's settings only, like everything here.
pub fn set_biss_key(sat: &str, rf_hz: f64, key: Option<&str>) -> std::io::Result<()> {
    let mut all = read_all();
    let kept = biss_kept_key(&all, sat, rf_hz);
    match key {
        Some(v) => {
            let k = kept.unwrap_or_else(|| format!("{}{:.0}khz", biss_prefix(sat), rf_hz / 1e3));
            set_in(&mut all, &k, v.to_string());
        }
        None => all.retain(|(k, _)| Some(k) != kept.as_ref()),
    }
    write_all(&all)
}

/// Set `key` to `value` in the settings in hand.
fn set_in(all: &mut Vec<(String, String)>, key: &str, value: String) {
    match all.iter_mut().find(|(k, _)| k == key) {
        Some(e) => e.1 = value,
        None => all.push((key.to_string(), value)),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn carriers_are_forgotten_and_the_rest_kept() {
        let mut all = kept(&[
            ("volume", "0.800"),
            ("carrier_caps.rf12300000khz", "model=fastlink"),
            ("carrier_caps.rf12310000khz", "adsb,model=tpc2964"),
            ("carrier_caps.rf12400000khz", "radar"),
        ]);
        drop_carrier_caps(&mut all, "");
        assert_eq!(all, kept(&[("volume", "0.800")]));
    }

    #[test]
    fn each_satellite_keeps_its_own_plan_and_labels() {
        // Before satellites were named: one plan, labels by RF alone.
        let mut all = kept(&[
            ("freqplan", "C:\\plans\\h3e.json"),
            ("carrier_caps.rf12333684khz", "model=cdm570"),
            ("volume", "0.800"),
        ]);
        // A new satellite starts with nothing, and is chosen.
        choose_in(&mut all, "Intelsat 19");
        assert!(caps_in(&all, "Intelsat 19").is_empty());
        assert_eq!(names_in(&all), ["Intelsat 19"]);
        // What it learns is its own; the unnamed labels stay as they were.
        set_in(
            &mut all,
            &format!("{}12333684khz", caps_prefix("Intelsat 19")),
            "model=viterbi".into(),
        );
        assert_eq!(
            caps_in(&all, "Intelsat 19"),
            [(12_333_684_000.0, "model=viterbi".to_string())]
        );
        assert_eq!(
            caps_in(&all, ""),
            [(12_333_684_000.0, "model=cdm570".to_string())]
        );
        // Naming the old one takes its plan and labels along.
        rename_in(&mut all, "", "Horizons 3e");
        assert!(caps_in(&all, "").is_empty());
        assert_eq!(
            caps_in(&all, "Horizons 3e"),
            [(12_333_684_000.0, "model=cdm570".to_string())]
        );
        assert!(
            all.iter()
                .any(|(k, v)| k == "freqplan.horizons_3e" && v.ends_with("h3e.json"))
        );
        assert!(!all.iter().any(|(k, _)| k == "freqplan"));
        assert_eq!(names_in(&all), ["Intelsat 19", "Horizons 3e"]);
        // Forgetting one satellite's labels leaves the other's.
        drop_carrier_caps(&mut all, "Horizons 3e");
        assert!(caps_in(&all, "Horizons 3e").is_empty());
        assert_eq!(caps_in(&all, "Intelsat 19").len(), 1);
        // Names: no separators, none spelled like one taken.
        let names = names_in(&all);
        assert_eq!(
            satellite_name_ok(" Optus D3 ", &names).as_deref(),
            Some("Optus D3")
        );
        assert_eq!(satellite_name_ok("intelsat-19", &names), None);
        assert_eq!(satellite_name_ok("a|b", &names), None);
        assert_eq!(satellite_name_ok("  ", &names), None);
    }

    #[test]
    fn a_radar_is_found_by_itself_not_its_vfo() {
        // Made-up places.
        let all = kept(&[
            ("radar_site.sac4.sic99", "-10.000000,120.000000"),
            ("radar_site.rf12300050khz", "-11.000000,121.000000"),
            ("radar_site.rf12300090khz", "-12.000000,122.000000"),
            ("radar_site.VFO 1", "-13.000000,123.000000"),
        ]);
        assert_eq!(
            kept_key(&all, &RadarId::Source(4, 99)).as_deref(),
            Some("radar_site.sac4.sic99")
        );
        assert_eq!(kept_key(&all, &RadarId::Source(4, 98)), None);
        // A carrier a little off (an LO trim, drift): the nearest kept one
        // within 10 kHz.
        assert_eq!(
            kept_key(&all, &RadarId::Carrier(12_300_053_400.0)).as_deref(),
            Some("radar_site.rf12300050khz")
        );
        assert_eq!(
            kept_key(&all, &RadarId::Carrier(12_300_082_000.0)).as_deref(),
            Some("radar_site.rf12300090khz")
        );
        assert_eq!(kept_key(&all, &RadarId::Carrier(12_300_200_000.0)), None);
        assert_eq!(
            RadarId::Carrier(12_300_053_400.0).key(),
            "radar_site.rf12300053khz"
        );
        assert_eq!(
            parse_site(&all[0].1),
            Some((-10.0, 120.0)),
            "kept as lat,lon"
        );
    }

    #[test]
    fn a_carrier_following_its_drift_is_the_same_radar() {
        let a = RadarId::Carrier(12_300_050_000.0);
        assert!(a.same(&RadarId::Carrier(12_300_052_000.0)));
        assert!(!a.same(&RadarId::Carrier(12_300_070_000.0)));
        assert!(!a.same(&RadarId::Source(4, 99)));
        assert!(RadarId::Source(4, 22).same(&RadarId::Source(4, 22)));
        assert_eq!(RadarId::Source(4, 22).label(), "SAC 4 SIC 22");
    }

    #[test]
    fn a_biss_key_is_found_by_its_carrier_and_satellite() {
        // Made-up keys.
        let mut all = kept(&[
            ("biss.horizons_3e.rf11611677khz", "1234 569C 789A BCCE"),
            ("biss.horizons_3e.rf11640000khz", "0000 0000 0000 0000"),
            ("biss.rf11611677khz", "AAAA AA FE BBBB BB 77"),
        ]);
        // A VFO a little off its carrier finds it; another satellite's
        // carrier at the same RF is its own.
        assert_eq!(
            biss_kept_key(&all, "Horizons 3e", 11_611_800e3).as_deref(),
            Some("biss.horizons_3e.rf11611677khz")
        );
        assert_eq!(biss_kept_key(&all, "Horizons 3e", 11_612_100e3), None);
        assert_eq!(
            biss_kept_key(&all, "", 11_611_677e3).as_deref(),
            Some("biss.rf11611677khz")
        );
        assert_eq!(biss_kept_key(&all, "Intelsat 19", 11_611_677e3), None);
        // A rename takes the keys along.
        rename_in(&mut all, "Horizons 3e", "H3e");
        assert_eq!(
            biss_kept_key(&all, "H3e", 11_611_677e3).as_deref(),
            Some("biss.h3e.rf11611677khz")
        );
    }
}
