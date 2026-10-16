// made by CasualArclamp
//! Launch a media player on a VFO's TS stream.
//!
//! The player gets the TCP server's HTTP URL, which both VLC and PotPlayer
//! open. Players are looked for where their installers put them; a player
//! that is not found is reported with the URL to open by hand.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Player {
    Vlc,
    PotPlayer,
}

impl Player {
    pub fn name(self) -> &'static str {
        match self {
            Player::Vlc => "VLC",
            Player::PotPlayer => "PotPlayer",
        }
    }

    fn candidates(self) -> Vec<PathBuf> {
        let dirs: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
            .iter()
            .filter_map(std::env::var_os)
            .map(PathBuf::from)
            .collect();
        let mut out = Vec::new();
        for d in &dirs {
            match self {
                Player::Vlc => out.push(d.join("VideoLAN").join("VLC").join("vlc.exe")),
                Player::PotPlayer => {
                    for sub in [d.join("DAUM").join("PotPlayer"), d.join("PotPlayer")] {
                        out.push(sub.join("PotPlayerMini64.exe"));
                        out.push(sub.join("PotPlayerMini.exe"));
                    }
                }
            }
        }
        out
    }
}

/// The URL a local player should open for a TCP server on `addr`: a server
/// listening on every interface is reached here through the loopback.
pub fn http_url(addr: SocketAddr) -> String {
    let ip = if addr.ip().is_unspecified() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        addr.ip()
    };
    format!("http://{}/", SocketAddr::new(ip, addr.port()))
}

/// Start `player` on `url`.
pub fn launch(player: Player, url: &str) -> Result<(), String> {
    for exe in player.candidates() {
        if exe.exists() {
            return std::process::Command::new(&exe)
                .arg(url)
                .spawn()
                .map(|_| ())
                .map_err(|e| format!("{}: {e}", exe.display()));
        }
    }
    // Elsewhere it may simply be on the PATH.
    let cmd = match player {
        Player::Vlc => "vlc",
        Player::PotPlayer => "PotPlayerMini64",
    };
    std::process::Command::new(cmd)
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|_| format!("{} not found: open {url} in it", player.name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_reach_the_server_locally() {
        assert_eq!(
            http_url("0.0.0.0:8001".parse().unwrap()),
            "http://127.0.0.1:8001/"
        );
        assert_eq!(
            http_url("127.0.0.1:9000".parse().unwrap()),
            "http://127.0.0.1:9000/"
        );
        assert_eq!(
            http_url("192.168.1.5:8001".parse().unwrap()),
            "http://192.168.1.5:8001/"
        );
    }
}
