// made by CasualArclamp
//! The password handshake. The server sends a random nonce, the client
//! answers with `HMAC-SHA-256(password, label ‖ server nonce ‖ client nonce)`,
//! so the password itself never crosses the wire and a recorded answer is no
//! use against the next nonce.
//!
//! It authenticates the client and nothing else: the link is not encrypted,
//! and someone who records a handshake can try dictionary guesses offline.
//! Over the internet, carry it inside a VPN or SSH tunnel.

use hmac::{Hmac, Mac};
use sha2::Sha256;

const LABEL: &[u8] = b"decsat-remote-auth-v1";
pub type Nonce = [u8; 32];

pub fn nonce() -> Nonce {
    let mut n = [0u8; 32];
    getrandom::fill(&mut n).expect("the system's random source");
    n
}

fn mac(password: &str, server: &Nonce, client: &Nonce) -> Hmac<Sha256> {
    // Rust note: `new_from_slice` only fails for fixed-size keys; HMAC takes
    // any length.
    let mut m = Hmac::<Sha256>::new_from_slice(password.as_bytes()).expect("HMAC takes any key");
    m.update(LABEL);
    m.update(server);
    m.update(client);
    m
}

pub fn proof(password: &str, server: &Nonce, client: &Nonce) -> Vec<u8> {
    mac(password, server, client)
        .finalize()
        .into_bytes()
        .to_vec()
}

/// Constant-time check of a client's answer.
pub fn verify(password: &str, server: &Nonce, client: &Nonce, answer: &[u8]) -> bool {
    mac(password, server, client).verify_slice(answer).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_right_password_passes_and_a_replay_does_not() {
        let (s, c) = (nonce(), nonce());
        let p = proof("hunter2", &s, &c);
        assert!(verify("hunter2", &s, &c, &p));
        assert!(!verify("hunter3", &s, &c, &p));
        assert!(!verify("hunter2", &nonce(), &c, &p), "a new server nonce");
        assert!(!verify("hunter2", &s, &c, &p[..31]));
    }
}
