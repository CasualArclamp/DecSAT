// made by CasualArclamp
//! The uplink identification string some encoders put in a stream's text —
//! the same facts the DVB-CID carrier (ETSI TS 103 129) carries (who made
//! the modulator or encoder, its serial number, where it is) but as plain
//! text in the SI, for example in an SDT service descriptor:
//!
//! `P02,ATEME,1915-08769__,_____,________________,+151.2469,-33.7536,______________A`
//!
//! (In the capture it sits in the NIT's transport stream loop, after a C4h
//! tag byte with no length byte of its own.)
//!
//! Comma separated, `_` pads a field out to its width and an unused field is
//! all `_`. Not a published format that I could find: the layout is read off
//! captures (an ATEME one and a DTVi one,
//! `P02,DTVi_,1230001228__,NONE_,+687299040_______,-21.05362,+164.525,Caledonia______`).
//! Field 0 is a format tag, 1 the maker, 2 the serial number, 3 something
//! ("NONE" in the one capture that filled it), 4 a telephone number, 5 and 6
//! the position in decimal degrees (east and north positive) and 7 free
//! text. The position's order is not the same in every maker's: ATEME sent
//! longitude then latitude, DTVi latitude then longitude. A value beyond
//! ±90 can only be the longitude, which settles it; when both are within
//! ±90 the order is latitude, longitude.

/// A parsed uplink identification string.
#[derive(Debug, Clone, PartialEq)]
pub struct UplinkId {
    /// The format tag: "P02".
    pub format: String,
    pub maker: String,
    pub serial: String,
    /// East positive, degrees.
    pub longitude: Option<f64>,
    /// North positive, degrees.
    pub latitude: Option<f64>,
    /// Field 3, unexplained.
    pub field3: Option<String>,
    pub telephone: Option<String>,
    /// Field 7, free text.
    pub text: Option<String>,
}

/// A field with its `_` padding removed; `None` when nothing is left.
fn field(s: &str) -> Option<String> {
    let t = s.trim_matches(|c: char| c == '_' || c.is_whitespace());
    (!t.is_empty()).then(|| t.to_string())
}

/// Read `text` as an uplink identification string, or `None` if it is not
/// one: eight comma separated fields, a tag `P` and two digits first, a
/// maker, and every position that is given a number.
pub fn parse(text: &str) -> Option<UplinkId> {
    let text = text.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    // `collect` into a `Vec` of string slices: borrowed pieces of `text`.
    let f: Vec<&str> = text.split(',').collect();
    if f.len() != 8 {
        return None;
    }
    let b = f[0].as_bytes();
    if b.len() != 3 || b[0] != b'P' || !b[1..].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let maker = field(f[1])?;
    // A position field is padding or a signed decimal number in range.
    let degrees = |s: &str, max: f64| -> Result<Option<f64>, ()> {
        match field(s) {
            None => Ok(None),
            Some(t) => match t.parse::<f64>() {
                Ok(v) if v.abs() <= max => Ok(Some(v)),
                _ => Err(()),
            },
        }
    };
    let (a, b) = (degrees(f[5], 180.0).ok()?, degrees(f[6], 180.0).ok()?);
    // Latitude first unless the first is beyond ±90 (so a longitude).
    let (latitude, longitude) = if a.is_some_and(|v| v.abs() > 90.0) {
        (b, a)
    } else {
        (a, b)
    };
    if latitude.is_some_and(|v| v.abs() > 90.0) {
        return None;
    }
    Some(UplinkId {
        format: f[0].to_string(),
        maker,
        serial: field(f[2]).unwrap_or_default(),
        longitude,
        latitude,
        field3: field(f[3]),
        telephone: field(f[4]),
        text: field(f[7]),
    })
}

impl UplinkId {
    /// The position as "33.7536°S 151.2469°E".
    pub fn position_text(&self) -> Option<String> {
        let (lat, lon) = (self.latitude?, self.longitude?);
        Some(format!(
            "{:.4}°{} {:.4}°{}",
            lat.abs(),
            if lat < 0.0 { 'S' } else { 'N' },
            lon.abs(),
            if lon < 0.0 { 'W' } else { 'E' },
        ))
    }

    /// One line for display.
    pub fn summary(&self) -> String {
        let mut s = format!("{} {}", self.maker, self.serial)
            .trim_end()
            .to_string();
        if let Some(p) = self.position_text() {
            s += &format!(", {p}");
        }
        for o in [&self.telephone, &self.text, &self.field3]
            .into_iter()
            .flatten()
        {
            s += &format!(", {o}");
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str =
        "P02,ATEME,1915-08769__,_____,________________,+151.2469,-33.7536,______________A";

    #[test]
    fn reads_the_captured_string() {
        let u = parse(SAMPLE).expect("parses");
        assert_eq!(u.format, "P02");
        assert_eq!(u.maker, "ATEME");
        assert_eq!(u.serial, "1915-08769");
        assert_eq!(u.longitude, Some(151.2469));
        assert_eq!(u.latitude, Some(-33.7536));
        assert_eq!(u.text.as_deref(), Some("A"));
        assert_eq!(u.telephone, None);
        assert_eq!(u.position_text().unwrap(), "33.7536°S 151.2469°E");
    }

    #[test]
    fn reads_the_dtvi_form() {
        let u = parse(
            "P02,DTVi_,1230001228__,NONE_,+687299040_______,-21.05362,+164.525,Caledonia______",
        )
        .expect("parses");
        assert_eq!(u.maker, "DTVi");
        assert_eq!(u.serial, "1230001228");
        assert_eq!(u.telephone.as_deref(), Some("+687299040"));
        assert_eq!(u.field3.as_deref(), Some("NONE"));
        assert_eq!(u.text.as_deref(), Some("Caledonia"));
        assert_eq!((u.latitude, u.longitude), (Some(-21.05362), Some(164.525)));
    }

    #[test]
    fn rejects_other_text() {
        assert!(parse("Nine Network Australia").is_none());
        assert!(parse("P02,ATEME,x,,,,").is_none());
        assert!(parse(&SAMPLE.replace("+151.2469", "+951.2469")).is_none());
        assert!(parse(&SAMPLE.replace("P02", "Q02")).is_none());
    }

    /// A real NIT packet (PID 10h): the string follows a C4h byte in the
    /// transport stream loop's descriptors.
    #[test]
    fn found_in_a_nit() {
        let hex = "474010120040f0833203c10000f01940174e696e65204e657477726f6b204175737472616c696120f05d00003203f057c45030322c4154454d452c313931352d30383737325f5f2c5f5f5f5f5f2c5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f2c2b3135312e323436392c2d33332e373533362c5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f4103000119cf6e54d7ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let pkt: [u8; 188] =
            std::array::from_fn(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap());
        let mut a = crate::psi::TsAnalyser::new();
        a.packet(&pkt);
        let r = a.report();
        let ids = r.uplink_ids;
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].serial, "1915-08772");
    }
}
