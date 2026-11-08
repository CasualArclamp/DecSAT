// made by CasualArclamp
//! The window's settings, one `key=value` a line in
//! `%APPDATA%\DecSAT-Server\prefs.txt` (`~/.config/DecSAT-Server` elsewhere).

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub name: String,
    /// Listen beyond this machine.
    pub network: bool,
    pub port: u16,
    pub use_password: bool,
    pub password: String,
    pub remember_password: bool,
    pub shared: bool,
    pub takeover: bool,
    /// Radios not offered to clients.
    pub hidden: Vec<String>,
    pub shared_radio: String,
    pub center_mhz: f64,
    pub rate_msps: f64,
    /// (radio, gain, value).
    pub gains: Vec<(String, String, f64)>,
    pub max_each: usize,
    pub max_total: usize,
    pub auto_start: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "DecSAT server".into()),
            // As the rest of DecSAT: reachable from this machine until asked.
            network: false,
            port: decsat_remote::DEFAULT_PORT,
            use_password: false,
            password: String::new(),
            remember_password: false,
            shared: false,
            takeover: false,
            hidden: Vec::new(),
            shared_radio: "hackrf".into(),
            center_mhz: 739.5,
            rate_msps: 8.0,
            gains: Vec::new(),
            max_each: 2,
            max_total: 8,
            auto_start: false,
        }
    }
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("DecSAT-Server").join("prefs.txt"))
}

pub fn load() -> Settings {
    let mut s = Settings::default();
    let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else {
        return s;
    };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let on = v == "1";
        match k {
            "name" => s.name = v.into(),
            "network" => s.network = on,
            "port" => s.port = v.parse().unwrap_or(s.port),
            "use_password" => s.use_password = on,
            "password" => s.password = v.into(),
            "remember_password" => s.remember_password = on,
            "shared" => s.shared = on,
            "takeover" => s.takeover = on,
            "hidden" => {
                s.hidden = v
                    .split(',')
                    .filter(|x| !x.is_empty())
                    .map(String::from)
                    .collect()
            }
            "shared_radio" => s.shared_radio = v.into(),
            "center_mhz" => s.center_mhz = v.parse().unwrap_or(s.center_mhz),
            "rate_msps" => s.rate_msps = v.parse().unwrap_or(s.rate_msps),
            "max_each" => s.max_each = v.parse().unwrap_or(s.max_each),
            "max_total" => s.max_total = v.parse().unwrap_or(s.max_total),
            "auto_start" => s.auto_start = on,
            "gain" => {
                let mut f = v.splitn(3, ',');
                if let (Some(r), Some(n), Some(x)) = (f.next(), f.next(), f.next())
                    && let Ok(x) = x.parse()
                {
                    s.gains.push((r.into(), n.into(), x));
                }
            }
            _ => {}
        }
    }
    s
}

pub fn save(s: &Settings) -> std::io::Result<()> {
    let Some(p) = path() else { return Ok(()) };
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let b = |x: bool| if x { "1" } else { "0" };
    let mut t = format!(
        "name={}\nnetwork={}\nport={}\nuse_password={}\nremember_password={}\nshared={}\ntakeover={}\n\
         hidden={}\nshared_radio={}\ncenter_mhz={}\nrate_msps={}\nmax_each={}\nmax_total={}\nauto_start={}\n",
        s.name,
        b(s.network),
        s.port,
        b(s.use_password),
        b(s.remember_password),
        b(s.shared),
        b(s.takeover),
        s.hidden.join(","),
        s.shared_radio,
        s.center_mhz,
        s.rate_msps,
        s.max_each,
        s.max_total,
        b(s.auto_start),
    );
    // Only when asked: the file is plain text.
    if s.remember_password {
        t.push_str(&format!("password={}\n", s.password));
    }
    for (r, n, v) in &s.gains {
        t.push_str(&format!("gain={r},{n},{v}\n"));
    }
    std::fs::write(p, t)
}
