// made by CasualArclamp
//! The RF frequency of the band's centre — the radio's tuning plus the LNB's
//! LO and trim, or a recording's own centre — app-wide, so that recordings
//! are named by the frequency they hold (`12346.0340MHz`) rather than by an
//! offset from wherever the radio happened to be tuned.

use std::sync::atomic::{AtomicU64, Ordering};

/// The band centre's RF, Hz, as an f64's bits (atomics hold integers); 0
/// while not known.
static RF_CENTER: AtomicU64 = AtomicU64::new(0);

/// The band centre's RF, Hz (0 or less: not known).
pub fn set_rf_center(hz: f64) {
    let v = if hz.is_finite() && hz > 0.0 { hz } else { 0.0 };
    RF_CENTER.store(v.to_bits(), Ordering::Relaxed);
}

pub fn rf_center() -> Option<f64> {
    let v = f64::from_bits(RF_CENTER.load(Ordering::Relaxed));
    (v > 0.0).then_some(v)
}

/// The frequency part of a recording's name for something `offset_hz` off
/// the band's centre: its RF in MHz to 100 Hz when the centre's RF is known
/// (an unsigned `…MHz` that opening the file reads back as its centre),
/// else the signed offset as before.
pub fn freq_tag(offset_hz: f64) -> String {
    tag(rf_center(), offset_hz)
}

fn tag(rf: Option<f64>, offset_hz: f64) -> String {
    match rf {
        Some(c) => format!("{:.4}MHz", (c + offset_hz) / 1e6),
        None => format!("{offset_hz:+.0}Hz"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_carry_the_rf_when_known() {
        assert_eq!(tag(Some(12_350_732_344.0), -4_698_344.0), "12346.0340MHz");
        assert_eq!(tag(None, -317_000.0), "-317000Hz");
        assert_eq!(tag(None, 5_236_688.0), "+5236688Hz");
    }
}
