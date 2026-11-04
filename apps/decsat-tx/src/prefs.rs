// made by CasualArclamp
//! What the app remembers between runs: `key=value` lines in
//! `%APPDATA%\DecSAT-TX\prefs.txt` (`~/.config/decsat-tx/prefs.txt`
//! elsewhere), its own file, never the receiver's. Nothing here is needed to
//! run: a file that cannot be read or written is the same as an empty one.
//! (Adapted from DecDVB-T's transmitter.)
//!
//! Never saved: the arm, the TX VGA, the amplifier, the output. The
//! environment variable `DECSAT_TX_PREFS` names another file (test runs
//! point it at a scratch file).

use std::path::PathBuf;

/// Where the file is.
pub fn file() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("DECSAT_TX_PREFS").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("DecSAT-TX"))
    } else {
        std::env::var_os("HOME").map(|d| PathBuf::from(d).join(".config").join("decsat-tx"))
    }?;
    Some(base.join("prefs.txt"))
}

/// The value of `key` in `text`'s `key=value` lines.
pub fn lookup(text: &str, key: &str) -> Option<String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.trim().to_string())
}

/// `old` with each of `pairs` set (an empty value removes its key).
pub fn merged(old: &str, pairs: &[(&str, String)]) -> String {
    let mut lines: Vec<String> = old
        .lines()
        .filter(|l| {
            l.split_once('=')
                .is_none_or(|(k, _)| !pairs.iter().any(|(key, _)| k == *key))
        })
        .map(String::from)
        .collect();
    for (key, value) in pairs {
        if !value.is_empty() {
            lines.push(format!("{key}={value}"));
        }
    }
    lines.join("\n") + "\n"
}

/// The whole file's text (empty if there is none).
pub fn read() -> String {
    file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default()
}

/// Several keys in one read and one write.
pub fn set_many(pairs: &[(&str, String)]) {
    let Some(path) = file() else { return };
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, merged(&old, pairs));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_replaces_and_empty_removes() {
        let t = merged("a=1\nb=2\n", &[("a", "9".into()), ("b", String::new())]);
        assert_eq!(lookup(&t, "a").as_deref(), Some("9"));
        assert_eq!(lookup(&t, "b"), None);
    }

    #[test]
    fn it_is_not_the_receivers_file() {
        let p = file().expect("a home or APPDATA");
        if std::env::var_os("DECSAT_TX_PREFS").is_none() {
            let s = p.display().to_string();
            assert!(s.contains("DecSAT-TX") || s.contains("decsat-tx"), "{s}");
        }
    }
}
