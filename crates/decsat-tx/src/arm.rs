// made by CasualArclamp
//! The arm gate: the radio cannot be opened without it (the approach of
//! DecDVB-T's transmitter, `decdvbt-tx/src/arm.rs`).
//!
//! 1. A new [`Arm`] is disarmed. Nothing arms it but an explicit call to
//!    [`Arm::arm`] with a [`LicenceAck`], which can only be made from the
//!    exact warning text the user was shown.
//! 2. An arm lapses after [`ARM_TIMEOUT`], and one transmission uses it up:
//!    [`Arm::take`] hands out an [`ArmedProof`] and disarms.
//! 3. `radio::open_hackrf` needs an `ArmedProof` by value. Its field is
//!    private to this module, so no other code can make one.
//! 4. Nothing here is saved, or read from arguments or the environment.
//!
//! Rust note: the "proof" is a zero-sized token. The compiler enforces the
//! gate (no code can open the radio without one), so the safety does not
//! rest on every caller remembering to check a flag.

use std::time::{Duration, Instant};

/// How long an arm lasts before it must be done again.
pub const ARM_TIMEOUT: Duration = Duration::from_secs(60);

/// The text the window must show beside the ARM control. [`LicenceAck::accept`]
/// takes it back, so the check is on what was displayed.
pub const LICENCE_WARNING: &str = "Transmitting needs a licence for the frequency and power used: \
an amateur licence for QO-100's 2.4 GHz uplink. Into a cable and attenuator to a receiver, or a \
dummy load, unless you hold one for this frequency, power and antenna. On QO-100, look before \
you transmit and keep 1 dB below the beacon. You are responsible for what this transmits.";

/// Proof the user saw and accepted [`LICENCE_WARNING`].
#[derive(Debug)]
pub struct LicenceAck(());

impl LicenceAck {
    /// `Some` only if `shown` is exactly the warning text.
    pub fn accept(shown: &str) -> Option<LicenceAck> {
        (shown == LICENCE_WARNING).then_some(LicenceAck(()))
    }
}

/// Permission for one transmission. Not `Clone`, not `Copy`, and only
/// [`Arm::take`] makes one.
#[derive(Debug)]
pub struct ArmedProof(());

/// The arm state of one session.
#[derive(Debug, Default)]
pub struct Arm {
    armed_at: Option<Instant>,
}

impl Arm {
    /// Disarmed.
    pub fn new() -> Arm {
        Arm::default()
    }

    /// Arm for [`ARM_TIMEOUT`].
    pub fn arm(&mut self, _ack: LicenceAck) {
        self.armed_at = Some(Instant::now());
    }

    pub fn disarm(&mut self) {
        self.armed_at = None;
    }

    /// Armed and not lapsed.
    pub fn is_armed(&self) -> bool {
        self.remaining().is_some()
    }

    /// Time left on the arm.
    pub fn remaining(&self) -> Option<Duration> {
        ARM_TIMEOUT.checked_sub(self.armed_at?.elapsed())
    }

    /// Use the arm up: `Some(proof)` if armed (and disarm), else `None`.
    pub fn take(&mut self) -> Option<ArmedProof> {
        let ok = self.is_armed();
        self.armed_at = None;
        ok.then_some(ArmedProof(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disarmed_by_default_and_the_proof_is_one_shot() {
        let mut a = Arm::new();
        assert!(!a.is_armed());
        assert!(a.take().is_none());
        a.arm(LicenceAck::accept(LICENCE_WARNING).unwrap());
        assert!(a.is_armed());
        assert!(a.take().is_some());
        assert!(!a.is_armed());
        assert!(a.take().is_none());
    }

    #[test]
    fn the_ack_needs_the_exact_text() {
        assert!(LicenceAck::accept("ok").is_none());
        assert!(LicenceAck::accept(LICENCE_WARNING).is_some());
    }

    #[test]
    fn an_arm_lapses_after_the_timeout() {
        let mut a = Arm::new();
        a.arm(LicenceAck::accept(LICENCE_WARNING).unwrap());
        a.armed_at = Instant::now().checked_sub(ARM_TIMEOUT + Duration::from_secs(1));
        assert!(!a.is_armed());
        assert!(a.take().is_none(), "a lapsed arm gives no proof");
    }

    #[test]
    fn disarm_cancels() {
        let mut a = Arm::new();
        a.arm(LicenceAck::accept(LICENCE_WARNING).unwrap());
        a.disarm();
        assert!(a.take().is_none());
    }

    /// The only file that may open the radio is radio.rs, and the only one
    /// that may make a proof is this one.
    #[test]
    fn only_radio_rs_opens_the_hackrf() {
        let files = [
            ("arm.rs", include_str!("arm.rs")),
            ("cpu.rs", include_str!("cpu.rs")),
            ("encode.rs", include_str!("encode.rs")),
            ("feeder.rs", include_str!("feeder.rs")),
            ("iq.rs", include_str!("iq.rs")),
            ("lib.rs", include_str!("lib.rs")),
            ("live.rs", include_str!("live.rs")),
            ("qo100.rs", include_str!("qo100.rs")),
            ("radio.rs", include_str!("radio.rs")),
            ("session.rs", include_str!("session.rs")),
        ];
        // Built from pieces so this test's own text does not match.
        let needle = ["HackRfTx", "::start"].concat();
        let proof = ["ArmedProof", "(())"].concat();
        for (name, text) in files {
            assert!(
                !text.contains(&needle) || name == "radio.rs",
                "{name} opens the radio"
            );
            if name != "arm.rs" {
                assert!(!text.contains(&proof), "{name} makes a proof");
            }
        }
    }
}
