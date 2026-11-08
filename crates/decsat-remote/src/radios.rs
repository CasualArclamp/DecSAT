// made by CasualArclamp
//! The radios a server offers, described generically so a client can draw
//! the controls for one it has never heard of. Receive only: nothing here
//! opens a transmit path.

use std::path::PathBuf;
use std::sync::Arc;

use decsat_core::{Result, SampleFormat};
use decsat_io::IqSource;
use serde::{Deserialize, Serialize};

/// One gain control. A switch is a stage with `max` 1 and `step` 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gain {
    pub name: String,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub value: f64,
}

/// A radio on offer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RadioInfo {
    pub key: String,
    pub name: String,
    pub min_hz: f64,
    pub max_hz: f64,
    /// Sample rates offered, Hz.
    pub rates: Vec<f64>,
    pub gains: Vec<Gain>,
}

/// What a client (or the shared-mode configuration) opens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RadioRequest {
    pub key: String,
    pub center_hz: f64,
    pub sample_rate: f64,
    /// Stages left out keep the radio's defaults.
    pub gains: Vec<(String, f64)>,
}

/// Live controls of an open radio.
pub trait RadioControl: Send + Sync {
    fn set_center(&self, hz: f64) -> Result<()>;
    fn set_gain(&self, name: &str, value: f64) -> Result<()>;
}

/// An open radio.
pub struct OpenRadio {
    pub source: Box<dyn IqSource>,
    pub control: Option<Arc<dyn RadioControl>>,
    pub gains: Vec<Gain>,
}

/// Where a server's radios come from. Tests use a fake one, so they never
/// touch hardware.
pub trait RadioProvider: Send + Sync {
    fn list(&self) -> Vec<RadioInfo>;
    fn open(&self, req: &RadioRequest) -> Result<OpenRadio>;
}

fn bad(msg: impl Into<String>) -> decsat_core::Error {
    decsat_core::Error::Other(msg.into())
}

/// Apply a request's gains to a list of stages, refusing unknown names.
fn merged(mut gains: Vec<Gain>, req: &RadioRequest) -> Result<Vec<Gain>> {
    for (name, v) in &req.gains {
        let g = gains
            .iter_mut()
            .find(|g| g.name == *name)
            .ok_or_else(|| bad(format!("{}: no gain called {name}", req.key)))?;
        g.value = v.clamp(g.min, g.max);
    }
    Ok(gains)
}

#[allow(dead_code)] // used by whichever radios are compiled in
fn value(gains: &[Gain], name: &str) -> f64 {
    gains
        .iter()
        .find(|g| g.name == name)
        .map_or(0.0, |g| g.value)
}

#[allow(dead_code)]
fn stage(name: &str, min: f64, max: f64, step: f64, value: f64) -> Gain {
    Gain {
        name: name.into(),
        min,
        max,
        step,
        value,
    }
}

/// A capture played on the server: for demonstrations and tests.
#[derive(Debug, Clone)]
pub struct FileRadio {
    pub path: PathBuf,
    pub format: SampleFormat,
    pub sample_rate: f64,
    pub center_hz: f64,
}

/// The radios this build can drive, plus any capture given to the server.
#[derive(Default)]
pub struct Devices {
    pub file: Option<FileRadio>,
    /// When set, only these keys are offered.
    pub only: Option<Vec<String>>,
}

impl Devices {
    fn all(&self) -> Vec<RadioInfo> {
        let mut v = Vec::new();
        if let Some(f) = &self.file {
            v.push(RadioInfo {
                key: "file".into(),
                name: format!("Capture: {}", f.path.display()),
                min_hz: f.center_hz,
                max_hz: f.center_hz,
                rates: vec![f.sample_rate],
                gains: vec![],
            });
        }
        #[cfg(feature = "hackrf")]
        v.push(RadioInfo {
            key: "hackrf".into(),
            name: "HackRF One".into(),
            min_hz: 1e6,
            max_hz: 6e9,
            rates: vec![2e6, 4e6, 8e6, 10e6, 12.5e6, 16e6, 20e6],
            gains: hackrf_gains(),
        });
        #[cfg(feature = "airspy")]
        v.push(RadioInfo {
            key: "airspy".into(),
            name: "Airspy Mini / R2 / HydraSDR".into(),
            min_hz: decsat_io::airspy::AIRSPY_MIN_HZ as f64,
            max_hz: decsat_io::airspy::AIRSPY_MAX_HZ as f64,
            rates: decsat_io::AIRSPY_RATES
                .iter()
                .map(|r| f64::from(*r))
                .collect(),
            gains: airspy_gains(),
        });
        #[cfg(feature = "fobos")]
        v.push(RadioInfo {
            key: "fobos".into(),
            name: "Fobos SDR".into(),
            min_hz: decsat_io::fobos::FOBOS_MIN_HZ as f64,
            max_hz: 6e9,
            rates: decsat_io::FOBOS_RATES
                .iter()
                .map(|r| f64::from(*r))
                .collect(),
            gains: fobos_gains(),
        });
        #[cfg(feature = "pluto")]
        v.push(RadioInfo {
            key: "pluto".into(),
            name: "ADALM-PLUTO / Pluto+".into(),
            min_hz: 70e6,
            max_hz: 6e9,
            rates: vec![2e6, 4e6, 64e6 / 7.0, 16e6, 20e6],
            gains: pluto_gains(),
        });
        if let Some(only) = &self.only {
            v.retain(|r| only.contains(&r.key));
        }
        v
    }
}

#[cfg(feature = "hackrf")]
fn hackrf_gains() -> Vec<Gain> {
    let d = decsat_io::HackRfGains::default();
    vec![
        stage("amp", 0.0, 1.0, 1.0, f64::from(u8::from(d.amp))),
        stage("lna", 0.0, 40.0, 8.0, f64::from(d.lna_db)),
        stage("vga", 0.0, 62.0, 2.0, f64::from(d.vga_db)),
    ]
}

#[cfg(feature = "hackrf")]
fn hackrf_of(g: &[Gain]) -> decsat_io::HackRfGains {
    decsat_io::HackRfGains {
        amp: value(g, "amp") >= 0.5,
        lna_db: value(g, "lna") as u16,
        vga_db: value(g, "vga") as u16,
    }
}

#[cfg(feature = "airspy")]
fn airspy_gains() -> Vec<Gain> {
    let d = decsat_io::AirspyGains::default();
    vec![
        stage("sensitivity", 0.0, 1.0, 1.0, 0.0),
        stage("gain", 0.0, 21.0, 1.0, f64::from(d.step)),
    ]
}

#[cfg(feature = "airspy")]
fn airspy_of(g: &[Gain]) -> decsat_io::AirspyGains {
    decsat_io::AirspyGains {
        mode: if value(g, "sensitivity") >= 0.5 {
            decsat_io::AirspyGainMode::Sensitivity
        } else {
            decsat_io::AirspyGainMode::Linearity
        },
        step: value(g, "gain") as u8,
    }
}

#[cfg(feature = "fobos")]
fn fobos_gains() -> Vec<Gain> {
    let d = decsat_io::FobosGains::default();
    vec![
        stage("lna", 0.0, 3.0, 1.0, f64::from(d.lna)),
        stage("vga", 0.0, 31.0, 1.0, f64::from(d.vga)),
    ]
}

#[cfg(feature = "fobos")]
fn fobos_of(g: &[Gain]) -> decsat_io::FobosGains {
    decsat_io::FobosGains {
        lna: value(g, "lna") as u8,
        vga: value(g, "vga") as u8,
    }
}

#[cfg(feature = "pluto")]
fn pluto_gains() -> Vec<Gain> {
    vec![
        stage("agc", 0.0, 1.0, 1.0, 0.0),
        stage("gain", -1.0, 73.0, 1.0, 50.0),
    ]
}

#[cfg(feature = "pluto")]
fn pluto_of(g: &[Gain]) -> decsat_io::PlutoGain {
    if value(g, "agc") >= 0.5 {
        decsat_io::PlutoGain::Agc
    } else {
        decsat_io::PlutoGain::Manual(value(g, "gain"))
    }
}

/// Keeps a radio's gain stages so one can change at a time: the drivers set
/// them all together.
#[cfg(any(
    feature = "hackrf",
    feature = "airspy",
    feature = "fobos",
    feature = "pluto"
))]
struct Live<C> {
    ctl: C,
    gains: std::sync::Mutex<Vec<Gain>>,
    center: fn(&C, f64) -> Result<()>,
    apply: fn(&C, &[Gain]) -> Result<()>,
}

#[cfg(any(
    feature = "hackrf",
    feature = "airspy",
    feature = "fobos",
    feature = "pluto"
))]
impl<C: Send + Sync> RadioControl for Live<C> {
    fn set_center(&self, hz: f64) -> Result<()> {
        (self.center)(&self.ctl, hz)
    }

    fn set_gain(&self, name: &str, v: f64) -> Result<()> {
        let mut g = self.gains.lock().unwrap();
        let s = g
            .iter_mut()
            .find(|s| s.name == name)
            .ok_or_else(|| bad(format!("no gain called {name}")))?;
        s.value = v.clamp(s.min, s.max);
        (self.apply)(&self.ctl, &g)
    }
}

impl RadioProvider for Devices {
    fn list(&self) -> Vec<RadioInfo> {
        self.all()
    }

    fn open(&self, req: &RadioRequest) -> Result<OpenRadio> {
        let info = self
            .all()
            .into_iter()
            .find(|r| r.key == req.key)
            .ok_or_else(|| bad(format!("this server offers no radio called {}", req.key)))?;
        let gains = merged(info.gains, req)?;
        match req.key.as_str() {
            "file" => {
                let f = self.file.as_ref().ok_or_else(|| bad("no capture"))?;
                let block = (f.sample_rate / 25.0).round() as usize;
                let source =
                    decsat_io::IqFileReader::open(&f.path, f.format, f.sample_rate, block)?
                        .with_center_freq(f.center_hz);
                Ok(OpenRadio {
                    source: Box::new(source),
                    control: None,
                    gains,
                })
            }
            #[cfg(feature = "hackrf")]
            "hackrf" => {
                let s = decsat_io::HackRfSource::open(decsat_io::HackRfSettings {
                    center_hz: req.center_hz as u64,
                    sample_rate: req.sample_rate as u32,
                    gains: hackrf_of(&gains),
                })?;
                let live = Live {
                    ctl: s.control(),
                    gains: gains.clone().into(),
                    center: |c: &decsat_io::HackRfControl, hz| c.set_center(hz as u64),
                    apply: |c: &decsat_io::HackRfControl, g| c.set_gains(hackrf_of(g)),
                };
                Ok(OpenRadio {
                    source: Box::new(s),
                    control: Some(Arc::new(live)),
                    gains,
                })
            }
            #[cfg(feature = "airspy")]
            "airspy" => {
                let s = decsat_io::AirspySource::open(decsat_io::AirspySettings {
                    center_hz: req.center_hz as u64,
                    sample_rate: req.sample_rate as u32,
                    gains: airspy_of(&gains),
                })?;
                let live = Live {
                    ctl: s.control(),
                    gains: gains.clone().into(),
                    center: |c: &decsat_io::AirspyControl, hz| c.set_center(hz as u64),
                    apply: |c: &decsat_io::AirspyControl, g| c.set_gains(airspy_of(g)),
                };
                Ok(OpenRadio {
                    source: Box::new(s),
                    control: Some(Arc::new(live)),
                    gains,
                })
            }
            #[cfg(feature = "fobos")]
            "fobos" => {
                let s = decsat_io::FobosSource::open(decsat_io::FobosSettings {
                    center_hz: req.center_hz as u64,
                    sample_rate: req.sample_rate as u32,
                    gains: fobos_of(&gains),
                    external_clock: false,
                })?;
                let live = Live {
                    ctl: s.control(),
                    gains: gains.clone().into(),
                    center: |c: &decsat_io::FobosControl, hz| c.set_center(hz as u64),
                    apply: |c: &decsat_io::FobosControl, g| c.set_gains(fobos_of(g)),
                };
                Ok(OpenRadio {
                    source: Box::new(s),
                    control: Some(Arc::new(live)),
                    gains,
                })
            }
            #[cfg(feature = "pluto")]
            "pluto" => {
                let s = decsat_io::PlutoSource::open(decsat_io::PlutoSettings {
                    center_hz: req.center_hz,
                    sample_rate: req.sample_rate,
                    gain: pluto_of(&gains),
                    ..Default::default()
                })?;
                let live = Live {
                    ctl: s.control(),
                    gains: gains.clone().into(),
                    center: |c: &decsat_io::PlutoControl, hz| c.set_center(hz),
                    apply: |c: &decsat_io::PlutoControl, g| c.set_gain(pluto_of(g)),
                };
                Ok(OpenRadio {
                    source: Box::new(s),
                    control: Some(Arc::new(live)),
                    gains,
                })
            }
            other => Err(bad(format!("{other} is not built into this server"))),
        }
    }
}
