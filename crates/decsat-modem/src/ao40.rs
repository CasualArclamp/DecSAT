// made by CasualArclamp
//! AO-40's housekeeping: the analogue channels of its A and E blocks.
//!
//! An A block ([`crate::p3beacon`]) is four lines of text, then 128 bytes
//! of analogue telemetry (channels #100 to #17F) and 128 of digital status.
//! This reads the header line and turns each analogue byte into its value.
//!
//! Layout and equations: AMSAT-DL, "AO-40 Telemetry Specification"
//! (tlmspec.txt), §8 (block formats) and §9 (analogue list). Checked on A
//! blocks recorded by Pieter Ibelings N4IP on 2003-03-14. The digital half
//! (§10) is not decoded.

/// Where the analogue bytes start in a block, and how many there are.
const ANALOGUE_AT: usize = 256;
const ANALOGUE_LEN: usize = 128;

/// How a channel's byte becomes its value (tlmspec §9).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Cal {
    /// a·X + b
    Linear(f32, f32),
    /// a·X² + b·X + c
    Quad(f32, f32, f32),
    /// c₀ + c₁·X + c₂·X² + c₃·X³
    Cubic(f32, f32, f32, f32),
    /// The spin rate's two-part curve.
    Spin,
    /// acos(X / 255), degrees.
    ArcCos,
    /// No equation: the byte as it is.
    Raw,
}

/// The temperature sensors' equation.
const T: Cal = Cal::Linear(0.659, -69.7);
/// The solar arrays' current sensors'.
const SA: Cal = Cal::Linear(0.1014, -0.6212);
/// The battery and bus voltages'.
const VB: Cal = Cal::Linear(0.1548, -1.484);

/// Channel (its offset from #100), name, unit, equation, and whether the
/// spec marks it dead since 2000-12-26.
type Entry = (u8, &'static str, &'static str, Cal, bool);

#[rustfmt::skip]
const CHANNELS: &[Entry] = &[
    (0x00, "SEU spin", "rpm", Cal::Spin, false),
    (0x01, "EPU motor pressure", "bar", Cal::Linear(0.0815, -1.253), false),
    (0x02, "EPU tank pressure", "bar", Cal::Linear(0.0835, -1.381), false),
    (0x03, "EPU motor current", "A", Cal::Linear(0.0503, -0.3154), false),
    (0x04, "EPU motor voltage", "V", Cal::Linear(1.221, -263.0537), false),
    (0x05, "EPU flow", "", Cal::Raw, false),
    (0x06, "Battery current", "A", Cal::Linear(0.2410, -31.28), false),
    (0x07, "EPU current (28V-U1)", "A", Cal::Linear(0.2035, -2.85), false),
    (0x08, "Main bus current (28V-U2)", "A", Cal::Linear(0.197, -0.739), false),
    (0x09, "28V-S current (28V-U3)", "A", Cal::Linear(0.0412, -0.76), false),
    (0x0A, "BCR current", "A", Cal::Linear(0.1024, -0.653), false),
    (0x0B, "Main battery", "V", VB, false),
    (0x0C, "Auxiliary battery", "V", VB, false),
    (0x0D, "28 V bus", "V", VB, false),
    (0x0E, "BCR-1 input", "V", Cal::Linear(0.1522, -1.06), false),
    (0x0F, "BCR-3 input", "V", Cal::Linear(0.1318, -0.923), false),
    (0x10, "10V C2 (BCR-3)", "V", Cal::Linear(0.0657, -0.712), false),
    (0x11, "BCR-2 input", "V", Cal::Linear(0.1318, -0.923), false),
    (0x12, "10V C1 (BCR-2)", "V", Cal::Linear(0.0657, -0.712), false),
    (0x13, "Motor valve position", "", Cal::Raw, false),
    (0x14, "400N high pressure", "bar", Cal::Linear(2.3406, -197.1), false),
    (0x15, "400N low pressure", "bar", Cal::Linear(0.1235, -1.235), false),
    (0x16, "AGC L2 receiver", "", Cal::Linear(0.154, -10.6), false),
    (0x17, "X transmitter power", "", Cal::Raw, false),
    (0x18, "X transmitter states", "", Cal::Raw, false),
    (0x19, "X transmitter helix", "mA", Cal::Linear(0.103, -0.95), false),
    (0x1A, "K transmitter power", "", Cal::Raw, false),
    (0x1B, "AGC S2/C receiver", "", Cal::Quad(-0.011, 3.66, -284.0), false),
    (0x1C, "AGC HF receiver", "", Cal::Raw, false),
    (0x1D, "AGC S1 receiver", "", Cal::Quad(-0.004, 1.25, -72.0), false),
    (0x1E, "AGC V receiver", "", Cal::Linear(0.254, -14.8), false),
    (0x1F, "AGC U receiver", "", Cal::Linear(0.457, -31.9), false),
    (0x20, "AGC L1 receiver", "", Cal::Linear(0.129, -7.9), false),
    (0x22, "S PA and mixer power", "", Cal::Raw, false),
    (0x23, "V transmitter power", "", Cal::Raw, false),
    (0x24, "V transmitter AGC", "", Cal::Raw, false),
    (0x25, "U transmitter PA power", "", Cal::Raw, false),
    (0x26, "U amplifier ALE", "", Cal::Raw, false),
    (0x29, "Antenna relay R1", "", Cal::Raw, false),
    (0x2B, "Temp X transmitter", "°C", Cal::Linear(-0.413, 103.8), false),
    (0x2C, "Temp X TWTA", "°C", Cal::Linear(-0.413, 103.8), false),
    (0x2D, "Sun sensor Z up/down", "", Cal::Raw, false),
    (0x2E, "Sun sensor Z 25", "°", Cal::Cubic(-31.501, 0.3682, -0.001539, 0.00000361), false),
    (0x2F, "Sun sensor Y 25", "°", Cal::Cubic(-31.501, 0.3682, -0.001539, 0.00000361), false),
    (0x30, "Sun sensor up X", "°", Cal::ArcCos, false),
    (0x31, "Sun sensor up Y", "°", Cal::ArcCos, false),
    (0x32, "Sun sensor down X", "°", Cal::ArcCos, false),
    (0x33, "Sun sensor down Y", "°", Cal::ArcCos, false),
    (0x34, "Sun sensor Z 45", "°", Cal::Cubic(-55.179, 0.64187, -0.002447, 0.00000581), false),
    (0x35, "Sun sensor Y 45", "°", Cal::Cubic(-55.179, 0.64187, -0.002447, 0.00000581), false),
    (0x36, "Sun sensor 25 valid", "", Cal::Raw, false),
    (0x37, "Sun sensor 45 valid", "", Cal::Raw, false),
    (0x38, "S2 transmitter power", "", Cal::Raw, false),
    (0x39, "S2 transmitter AGC", "", Cal::Raw, false),
    (0x3A, "ARU bridge A", "", Cal::Raw, false),
    (0x3B, "ARU bridge B", "", Cal::Raw, false),
    (0x40, "Temp SEU", "°C", T, false),
    (0x41, "Temp EPU", "°C", T, false),
    (0x42, "Temp BCR 1", "°C", T, false),
    (0x43, "Temp BCR 3", "°C", T, false),
    (0x44, "Temp BCR 2", "°C", T, false),
    (0x45, "Temp MMH bay 3", "°C", T, false),
    (0x46, "Temp aux battery bay 5", "°C", T, false),
    (0x47, "Temp aux battery bay 1", "°C", T, false),
    (0x48, "Temp NH3 bay 2", "°C", T, false),
    (0x49, "Temp main battery bay 2", "°C", T, false),
    (0x4A, "Temp main battery bay 4", "°C", T, true),
    (0x4B, "Temp main battery bay 6", "°C", T, true),
    (0x4C, "Temp solar panel 1", "°C", T, false),
    (0x4D, "Temp solar panel 2", "°C", T, false),
    (0x4E, "Temp solar panel 3", "°C", T, false),
    (0x4F, "Temp solar panel 4", "°C", T, false),
    (0x50, "Temp solar panel 5", "°C", T, false),
    (0x51, "Temp solar panel 6", "°C", T, false),
    (0x52, "Temp L2 receiver", "°C", T, false),
    (0x53, "Temp heat pipe 2 +X -Y", "°C", T, false),
    (0x54, "Temp heat pipe 2 -X", "°C", T, true),
    (0x56, "Temp S2/C receiver", "°C", T, false),
    (0x57, "Temp S1/HF receiver", "°C", T, false),
    (0x58, "Temp U exciter", "°C", T, false),
    (0x59, "Temp U and V receivers", "°C", T, false),
    (0x5A, "Temp L1 receiver", "°C", T, false),
    (0x5B, "Temp S1 transmitter", "°C", T, false),
    (0x5C, "Temp S2 transmitter", "°C", T, false),
    (0x5E, "Temp V transmitter", "°C", T, false),
    (0x5F, "Temp U transmitter PA", "°C", T, false),
    (0x61, "Temp IHU", "°C", T, false),
    (0x62, "Temp top", "°C", T, true),
    (0x63, "Temp bottom", "°C", T, true),
    (0x64, "Temp back", "°C", T, true),
    (0x65, "Temp side 4 panel", "°C", T, false),
    (0x66, "Temp heat pipe 4 +X +Y", "°C", T, false),
    (0x67, "Temp heat pipe 3 -X", "°C", T, false),
    (0x68, "Temp heat pipe 2 +X +Y", "°C", T, false),
    (0x69, "Temp heat pipe 1 +X -Y", "°C", T, false),
    (0x6A, "Temp heat pipe 3 +X", "°C", T, false),
    (0x6B, "Temp N2O4 -X -Y", "°C", T, true),
    (0x6C, "Temp N2O4 +X +Y", "°C", T, false),
    (0x6D, "Temp side 2 panel", "°C", T, false),
    (0x6E, "Temp S antenna", "°C", T, false),
    (0x6F, "Temp helium tank", "°C", T, true),
    (0x70, "SEU 28V current", "", Cal::Raw, true),
    (0x71, "Array 1 current (BCR-1)", "A", SA, true),
    (0x72, "Array 6 current (BCR-1)", "A", SA, true),
    (0x73, "Array 3 current (BCR-3)", "A", SA, true),
    (0x74, "Array 2 current (BCR-3)", "A", SA, true),
    (0x75, "10V C2 current (BCR-3)", "A", Cal::Linear(0.0125, -0.0875), true),
    (0x76, "Array 4 current (BCR-2)", "A", SA, true),
    (0x77, "Array 5 current (BCR-2)", "A", SA, true),
    (0x78, "10V C1 current (BCR-2)", "A", Cal::Linear(0.0125, -0.0875), true),
    (0x79, "K transmitter current", "", Cal::Raw, false),
    (0x7A, "S PA and mixer 28V current", "A", Cal::Linear(0.0429, -0.333), false),
    (0x7B, "S PA and mixer 10V current", "", Cal::Raw, false),
];

impl Cal {
    fn value(self, x: u8) -> f32 {
        let x = f32::from(x);
        match self {
            Cal::Linear(a, b) => a * x + b,
            Cal::Quad(a, b, c) => a * x * x + b * x + c,
            Cal::Cubic(c0, c1, c2, c3) => c0 + c1 * x + c2 * x * x + c3 * x * x * x,
            Cal::Spin if x > 101.0 => (x / 150.303_39).powf(-5.032_524),
            Cal::Spin => 46.4720 - 0.38452 * x,
            Cal::ArcCos => (x / 255.0).acos().to_degrees(),
            Cal::Raw => x,
        }
    }
}

/// One analogue channel's reading.
#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    /// Its telemetry address, #100 to #17F.
    pub address: u16,
    pub name: &'static str,
    /// The unit; empty for a channel with no equation (the raw byte).
    pub unit: &'static str,
    pub raw: u8,
    pub value: f32,
    /// The spec lists the sensor as not working since 2000-12-26.
    pub dead: bool,
}

/// An A or E block's housekeeping.
#[derive(Debug, Clone, PartialEq)]
pub struct Housekeeping {
    /// `A` (now) or `E` (a past event).
    pub block: char,
    /// The header's date and time, UTC, and command number, as sent.
    pub date: String,
    pub time: String,
    pub command: String,
    pub channels: Vec<Channel>,
}

impl Housekeeping {
    /// The channel at a telemetry address (#100 to #17F), if it is wired.
    pub fn channel(&self, address: u16) -> Option<&Channel> {
        self.channels.iter().find(|c| c.address == address)
    }
}

/// Read an AO-40 A or E block's 512 bytes; `None` for any other block.
pub fn housekeeping(block: &[u8]) -> Option<Housekeeping> {
    if block.len() < ANALOGUE_AT + ANALOGUE_LEN {
        return None;
    }
    let name = *block.first()? as char;
    let line = std::str::from_utf8(&block[..64]).ok()?;
    if !matches!(name, 'A' | 'E') || !line.contains("AMSAT OSCAR-40") {
        return None;
    }
    // "A  HI, THIS IS AMSAT OSCAR-40       2003-03-14  23:38:21  #08D7"
    let mut words = line.split_whitespace().rev();
    let command = words.next()?.to_string();
    let time = words.next()?.to_string();
    let date = words.next()?.to_string();
    let analogue = &block[ANALOGUE_AT..ANALOGUE_AT + ANALOGUE_LEN];
    let channels = CHANNELS
        .iter()
        .map(|&(offset, name, unit, cal, dead)| {
            let raw = analogue[usize::from(offset)];
            Channel {
                address: 0x100 + u16::from(offset),
                name,
                unit,
                raw,
                value: cal.value(raw),
                dead,
            }
        })
        .collect();
    Some(Housekeeping {
        block: name,
        date,
        time,
        command,
        channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The analogue bytes of the A block AO-40 sent at 23:38:21 UTC on
    /// 2003-03-14 (N4IP's recording).
    const RECEIVED: &str = "7d08080b15087f0f113121af09aeac8bab2eab08080845082525460808080845\
                            444508324e08085321fd90292608080808080808080808086a4d08082b4a7285\
                            7f8f868b908d95a09d98fcf96b7f7f7f7f6c988ffc428f8d8e8788878c418e91\
                            4280fcfcfc84868a8e918afc898582fcb3b2b2b1b1b2b1b1b223225a22222222";

    fn received_block() -> Vec<u8> {
        let mut block = format!(
            "{:<256}",
            "A  HI, THIS IS AMSAT OSCAR-40       2003-03-14  23:38:21  #08D7"
        )
        .into_bytes();
        block
            .extend((0..128).map(|i| u8::from_str_radix(&RECEIVED[2 * i..2 * i + 2], 16).unwrap()));
        block.resize(512, 0);
        block
    }

    #[test]
    fn the_received_block_reads_sensibly() {
        let h = housekeeping(&received_block()).expect("an A block");
        assert_eq!(
            (h.block, h.date.as_str(), h.time.as_str()),
            ('A', "2003-03-14", "23:38:21")
        );
        assert_eq!(h.command, "#08D7");
        let value = |a: u16| h.channel(a).unwrap().value;
        // Main battery and the 28 V bus near 25.5 V; the aux battery off.
        assert!((value(0x10B) - 25.6).abs() < 0.1, "{}", value(0x10B));
        assert!((value(0x10D) - 25.45).abs() < 0.1, "{}", value(0x10D));
        assert!(value(0x10C).abs() < 0.2);
        // A slow spin, and electronics at room temperature.
        assert!((value(0x100) - 2.53).abs() < 0.05, "{}", value(0x100));
        assert!((value(0x140) - 14.0).abs() < 0.1, "{}", value(0x140));
        assert!((value(0x161) - 24.5).abs() < 20.0, "{}", value(0x161));
        // The sensors the spec calls dead read off the scale.
        let dead = h.channel(0x14A).unwrap();
        assert!(dead.dead && dead.raw == 0xFC);
    }

    #[test]
    fn every_channel_has_its_own_address_and_unwired_ones_are_left_out() {
        let h = housekeeping(&received_block()).unwrap();
        let mut seen = std::collections::BTreeSet::new();
        assert!(h.channels.iter().all(|c| seen.insert(c.address)));
        for unwired in [0x121, 0x127, 0x13C, 0x155, 0x15D, 0x160, 0x17F] {
            assert!(h.channel(unwired).is_none(), "{unwired:#x}");
        }
        assert_eq!(h.channels.len(), 113);
    }

    #[test]
    fn other_blocks_are_not_housekeeping() {
        let mut block = received_block();
        block[0] = b'L';
        assert_eq!(housekeeping(&block), None);
        let qo100 = format!("{:<512}", "K HI de Qatar-OSCAR 100 (DK0SB)").into_bytes();
        assert_eq!(housekeeping(&qo100), None);
        assert_eq!(housekeeping(&received_block()[..300]), None);
    }
}
