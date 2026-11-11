// made by CasualArclamp
//! The LNB's mixing: which side of the band its oscillator is on.
//!
//! Low side (Ku: 9750, 10600 MHz): the LO is below the band, RF = LO + IF.
//! High side (C band: 5150 MHz): the LO is above it, RF = LO - IF, and the
//! band arrives mirrored (the source conjugates it back, `decsat_io::Mirror`).
//! An LO of 0 means no LNB: the tuner is at RF either way.

/// The RF a tuner frequency `tuned` corresponds to, Hz.
pub fn rf(lo: f64, tuned: f64, high_side: bool) -> f64 {
    if high_side && lo > 0.0 {
        lo - tuned
    } else {
        tuned + lo
    }
}

/// The tuner frequency for an RF, Hz: the inverse of [`rf`].
pub fn tuned(lo: f64, rf: f64, high_side: bool) -> f64 {
    if high_side && lo > 0.0 {
        lo - rf
    } else {
        rf - lo
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_and_back() {
        // Ku low band: 10489.75 MHz through 9750 lands at 739.75.
        assert_eq!(rf(9750e6, 739.75e6, false), 10489.75e6);
        assert_eq!(tuned(9750e6, 10489.75e6, false), 739.75e6);
        // C band: 4126.883 MHz through 5150 lands at 1023.117.
        assert!((tuned(5150e6, 4126.883e6, true) - 1023.117e6).abs() < 1.0);
        assert!((rf(5150e6, 1023.117e6, true) - 4126.883e6).abs() < 1.0);
        // No LNB: the side does not matter.
        assert_eq!(rf(0.0, 100e6, true), 100e6);
        assert_eq!(tuned(0.0, 100e6, true), 100e6);
    }
}
