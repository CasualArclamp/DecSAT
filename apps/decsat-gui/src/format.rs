// made by CasualArclamp
//! Number formatting for frequencies and rates.

/// A frequency in Hz as `1 234.567 MHz` / `567.8 kHz` / `12 Hz`, choosing the
/// unit by magnitude.
pub fn freq(hz: f64) -> String {
    let a = hz.abs();
    if a >= 1e9 {
        format!("{:.6} GHz", hz / 1e9)
    } else if a >= 1e6 {
        format!("{:.4} MHz", hz / 1e6)
    } else if a >= 1e3 {
        format!("{:.2} kHz", hz / 1e3)
    } else {
        format!("{hz:.0} Hz")
    }
}

/// A symbol or sample rate: `1.250 MS/s`, `333.3 kS/s`.
pub fn rate(sps: f64) -> String {
    if sps.abs() >= 1e6 {
        format!("{:.3} MS/s", sps / 1e6)
    } else {
        format!("{:.1} kS/s", sps / 1e3)
    }
}

/// A bit rate: `2.10 Mbit/s`, `850.0 kbit/s`.
pub fn bitrate(bps: f64) -> String {
    if bps.abs() >= 1e6 {
        format!("{:.2} Mbit/s", bps / 1e6)
    } else {
        format!("{:.1} kbit/s", bps / 1e3)
    }
}

/// A size: `1.2 MB`, `350 kB`.
pub fn bytes(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1} MB", n as f64 / 1e6)
    } else {
        format!("{:.0} kB", n as f64 / 1e3)
    }
}

/// A short axis label for a frequency, at a precision suited to the tick step.
pub fn tick(hz: f64, step: f64) -> String {
    // Unit from the larger of the value and the step, so a tick at 0 Hz uses
    // the same unit as its neighbours.
    let mag = hz.abs().max(step);
    let (div, unit) = if mag >= 1e9 {
        (1e9, "G")
    } else if mag >= 1e6 {
        (1e6, "M")
    } else if mag >= 1e3 {
        (1e3, "k")
    } else {
        (1.0, "")
    };
    // Enough decimals that adjacent ticks differ.
    let decimals = ((div / step).log10().ceil().max(0.0) as usize).min(6);
    format!("{:.*}{unit}", decimals, hz / div)
}

/// A "nice" tick spacing (1, 2 or 5 × 10ⁿ) giving about `target` ticks over `span`.
pub fn nice_step(span: f64, target: f64) -> f64 {
    let raw = span / target.max(1.0);
    // `powi` of 10 is exact for these exponents; `powf` need not be, and an
    // axis step of 1999999.9999999998 Hz makes ragged tick labels.
    let mag = 10f64.powi(raw.log10().floor() as i32);
    let r = raw / mag;
    let nice = if r < 1.5 {
        1.0
    } else if r < 3.5 {
        2.0
    } else if r < 7.5 {
        5.0
    } else {
        10.0
    };
    nice * mag
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units() {
        assert_eq!(freq(10_489_750_000.0), "10.489750 GHz");
        assert_eq!(freq(1_234_567.0), "1.2346 MHz");
        assert_eq!(freq(-25_000.0), "-25.00 kHz");
        assert_eq!(rate(1_250_000.0), "1.250 MS/s");
        assert_eq!(rate(333_333.0), "333.3 kS/s");
    }

    #[test]
    fn nice_steps() {
        assert_eq!(nice_step(20e6, 10.0), 2e6);
        assert_eq!(nice_step(1e6, 10.0), 1e5);
        // Raw 300 kHz rounds to the nearer of 200/500 kHz in the 1-2-5 series.
        assert_eq!(nice_step(3e6, 10.0), 2e5);
        assert_eq!(nice_step(8e6, 10.0), 1e6);
    }

    #[test]
    fn ticks_resolve_their_step() {
        assert_eq!(tick(10_490_100_000.0, 100e3), "10.4901G");
        assert_eq!(tick(2.5e6, 0.5e6), "2.5M");
        assert_eq!(tick(250e3, 50e3), "250k");
        // Zero takes its neighbours' unit.
        assert_eq!(tick(0.0, 100e3), "0k");
    }
}
