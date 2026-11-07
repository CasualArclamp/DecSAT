// made by CasualArclamp
//! The Pluto front end against an in-memory IIOD server that answers the way
//! libiio 0.26's iiod does (replies, chunked READBUF with its mask line).

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Mutex};

use super::iiod::{Client, Link, parse_context};
use super::*;

/// Shaped like a real Pluto's `PRINT` (trimmed): a 2r2t RX device, the
/// transmit device, device-level attributes after the channels, escaped
/// characters.
fn xml(fw: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><!DOCTYPE context [<!ELEMENT context (device | context-attribute)*>]>
<context name="network" description="Linux pluto 5.15" >
<context-attribute name="hw_model" value="Analog Devices PlutoSDR Rev.C (Z7010-AD9363A)" />
<context-attribute name="fw_version" value="{fw}" />
<device id="iio:device0" name="ad9361-phy" >
<channel id="altvoltage1" name="TX_LO" type="output" ><attribute name="frequency" filename="out_altvoltage1_TX_LO_frequency" /><attribute name="powerdown" filename="out_altvoltage1_TX_LO_powerdown" /></channel>
<channel id="voltage0" type="input" ><attribute name="hardwaregain" filename="in_voltage0_hardwaregain" /><attribute name="gain_control_mode" filename="x" /><attribute name="rf_bandwidth" filename="x" /><attribute name="sampling_frequency" filename="x" /></channel>
<channel id="altvoltage0" name="RX_LO" type="output" ><attribute name="frequency" filename="out_altvoltage0_RX_LO_frequency" /></channel>
<attribute name="calib_mode" />
</device>
<device id="iio:device3" name="cf-ad9361-dds-core-lpc" >
<channel id="voltage0" type="output" ><scan-element index="0" format="le:S16/16&gt;&gt;0" /></channel>
</device>
<device id="iio:device4" name="cf-ad9361-lpc" >
<channel id="voltage0" type="input" ><scan-element index="0" format="le:S12/16&gt;&gt;0" /><attribute name="calibscale" filename="x" /><attribute name="sampling_frequency" filename="x" /></channel>
<channel id="voltage1" type="input" ><scan-element index="1" format="le:S12/16&gt;&gt;0" /></channel>
<channel id="voltage2" type="input" ><scan-element index="2" format="le:S12/16&gt;&gt;0" /></channel>
<channel id="voltage3" type="input" ><scan-element index="3" format="le:S12/16&gt;&gt;0" /></channel>
<attribute name="sync_start_enable" />
<buffer-attribute name="watermark" />
</device>
</context>"#
    )
}

#[derive(Default)]
struct State {
    fw: String,
    /// Every command line, in order.
    log: Vec<String>,
    attrs: HashMap<String, String>,
    /// Sample counter, so blocks carry a known ramp.
    k: u32,
    /// OPEN's mask: 1 = 8-bit (voltage0 alone), 3 = I and Q.
    mask: u32,
}

/// One connection to the fake server.
struct FakeLink {
    st: Arc<Mutex<State>>,
    inbox: Vec<u8>,
    outbox: VecDeque<u8>,
}

impl FakeLink {
    fn reply_int(&mut self, v: i64) {
        self.outbox.extend(format!("{v}\n").bytes());
    }

    fn handle(&mut self, line: &str, payload: Option<&[u8]>) {
        let st = Arc::clone(&self.st);
        let mut st = st.lock().unwrap();
        st.log.push(line.to_string());
        let w: Vec<&str> = line.split_whitespace().collect();
        match w[0] {
            "PRINT" => {
                let x = xml(&st.fw);
                self.reply_int(x.len() as i64);
                self.outbox.extend(x.bytes());
                self.outbox.push_back(b'\n');
            }
            "READ" => {
                let key = w[1..].join(" ");
                let v = st.attrs.get(&key).cloned().unwrap_or_else(|| "0".into());
                self.reply_int(v.len() as i64);
                self.outbox.extend(v.bytes());
                self.outbox.push_back(b'\n');
            }
            "WRITE" => {
                let key = w[1..w.len() - 1].join(" ");
                let v = String::from_utf8_lossy(payload.unwrap()).into_owned();
                // The AD9363 refuses an LO above 3.8 GHz (not unlocked).
                if key.ends_with("altvoltage0 frequency")
                    && v.parse::<u64>().unwrap() > 3_800_000_000
                {
                    self.reply_int(-22);
                    return;
                }
                st.attrs.insert(key, v.clone());
                self.reply_int(v.len() as i64);
            }
            "OPEN" => {
                st.mask = u32::from_str_radix(w[3], 16).unwrap();
                self.reply_int(0);
            }
            "READBUF" => {
                let n: usize = w[2].parse().unwrap();
                // Two chunks, the mask line after the first length only.
                let mut body = Vec::with_capacity(n);
                while body.len() < n {
                    let k = st.k;
                    st.k += 1;
                    if st.mask == 1 {
                        body.push((k % 100) as u8);
                        body.push((-((k % 100) as i8)) as u8);
                    } else {
                        body.extend(((k % 2000) as i16).to_le_bytes());
                        body.extend((-((k % 2000) as i16)).to_le_bytes());
                    }
                }
                body.truncate(n);
                let half = n / 2;
                self.reply_int(half as i64);
                self.outbox.extend(format!("{:08x}\n", st.mask).bytes());
                self.outbox.extend(&body[..half]);
                self.reply_int((n - half) as i64);
                self.outbox.extend(&body[half..]);
            }
            "BADCMD" => self.reply_int(-22),
            _ => self.reply_int(0), // TIMEOUT, SET, CLOSE
        }
    }
}

impl Link for FakeLink {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inbox.extend_from_slice(bytes);
        loop {
            let Some(i) = self.inbox.iter().position(|&b| b == b'\n') else {
                return Ok(());
            };
            let line = String::from_utf8_lossy(&self.inbox[..i]).trim().to_string();
            if line.starts_with("WRITE") {
                let len: usize = line.rsplit(' ').next().unwrap().parse().unwrap();
                if self.inbox.len() < i + 1 + len {
                    return Ok(()); // the value has not come yet
                }
                let payload = self.inbox[i + 1..i + 1 + len].to_vec();
                self.inbox.drain(..i + 1 + len);
                self.handle(&line, Some(&payload));
            } else {
                self.inbox.drain(..=i);
                self.handle(&line, None);
            }
        }
    }

    fn read_some(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.outbox.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "fake: nothing to read",
            ));
        }
        // Dribble replies out in odd-sized pieces, as TCP may.
        let n = buf.len().min(self.outbox.len()).min(7919);
        for b in buf.iter_mut().take(n) {
            *b = self.outbox.pop_front().unwrap();
        }
        Ok(n)
    }
}

struct Fake(Arc<Mutex<State>>);

impl Connector for Fake {
    fn link(&self, _pipe: usize) -> Result<Box<dyn Link>> {
        Ok(Box::new(FakeLink {
            st: Arc::clone(&self.0),
            inbox: Vec::new(),
            outbox: VecDeque::new(),
        }))
    }
    fn describe(&self) -> String {
        "fake".into()
    }
}

fn fake(fw: &str) -> (Arc<Mutex<State>>, Arc<dyn Connector>) {
    let st = Arc::new(Mutex::new(State {
        fw: fw.into(),
        ..Default::default()
    }));
    (Arc::clone(&st), Arc::new(Fake(st)))
}

#[test]
fn parses_the_context() {
    let ctx = parse_context(&xml("v0.39")).unwrap();
    assert_eq!(ctx.attr("fw_version"), Some("v0.39"));
    assert!(ctx.attr("hw_model").unwrap().contains("AD9363"));
    let rx = ctx.device("cf-ad9361-lpc").unwrap();
    assert_eq!(rx.id, "iio:device4");
    assert_eq!(rx.channels.len(), 4);
    assert_eq!(rx.mask_words(), 1);
    let v0 = rx.channel("voltage0", false).unwrap();
    assert_eq!(v0.scan_index, Some(0));
    assert!(v0.attrs.iter().any(|a| a == "sampling_frequency"));
    // The device's own attribute after </channel> is not the last channel's.
    assert!(rx.channel("voltage3", false).unwrap().attrs.is_empty());
    let phy = ctx.device("ad9361-phy").unwrap();
    assert!(phy.channel("altvoltage1", true).is_some());
    assert!(phy.channel("altvoltage1", false).is_none());
    assert!(phy.channel("calib_mode", false).is_none());
    assert!(parse_context("<context></context>").is_err());
}

#[test]
fn uri_forms() {
    assert_eq!("".parse::<PlutoUri>().unwrap(), PlutoUri::Auto);
    assert_eq!("Auto".parse::<PlutoUri>().unwrap(), PlutoUri::Auto);
    assert_eq!("usb:".parse::<PlutoUri>().unwrap(), PlutoUri::Usb);
    assert_eq!("usb:1.5.5".parse::<PlutoUri>().unwrap(), PlutoUri::Usb);
    assert_eq!(
        "ip:192.168.2.1".parse::<PlutoUri>().unwrap(),
        PlutoUri::Ip("192.168.2.1".into())
    );
    assert_eq!(
        "pluto.local:30431".parse::<PlutoUri>().unwrap(),
        PlutoUri::Ip("pluto.local:30431".into())
    );
    assert!("ip:".parse::<PlutoUri>().is_err());
    assert_eq!(PlutoUri::Ip("10.0.0.5".into()).to_string(), "ip:10.0.0.5");
}

#[test]
fn rate_plan_uses_the_decimator_below_2_083_msps() {
    assert_eq!(rate_plan(10e6).unwrap(), (10_000_000, None));
    assert_eq!(rate_plan(64e6 / 7.0).unwrap(), (9_142_857, None));
    assert_eq!(rate_plan(2.5e6).unwrap(), (2_500_000, None));
    assert_eq!(rate_plan(1e6).unwrap(), (8_000_000, Some(1_000_000)));
    assert_eq!(rate_plan(300e3).unwrap(), (2_400_000, Some(300_000)));
    assert!(rate_plan(100e3).is_err());
    assert!(rate_plan(70e6).is_err());
}

#[test]
fn converts_both_sample_formats() {
    let mut out = Vec::new();
    let raw16: Vec<u8> = [2047i16, -2048, 0, 1024]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    convert(&raw16, false, &mut out);
    assert_eq!(out.len(), 2);
    assert!((out[0].re - 2047.0 / 2048.0).abs() < 1e-6);
    assert_eq!(out[0].im, -1.0);
    assert_eq!(out[1], Iq::new(0.0, 0.5));
    // Tezuka: I in byte 0, Q in byte 1, int8.
    convert(&[127, 0x80, 0xff, 64], true, &mut out);
    assert_eq!(out.len(), 2);
    assert!((out[0].re - 127.0 / 128.0).abs() < 1e-6);
    assert_eq!(out[0].im, -1.0);
    assert_eq!(out[1], Iq::new(-1.0 / 128.0, 0.5));
}

#[test]
fn block_and_kernel_buffer_sizes() {
    for rate in [300e3, 2.5e6, 64e6 / 7.0, 20e6, 61.44e6] {
        let b = block_samples(rate);
        assert_eq!(b % 4096, 0);
        assert!((16_384..=1 << 20).contains(&b));
        assert!((4..=16).contains(&kernel_buffers(rate)));
    }
}

#[test]
fn errors_come_back_with_their_errno() {
    let (_st, conn) = fake("v0.39");
    let mut c = Client::new(conn.link(0).unwrap());
    let e = c.exec("BADCMD\r\n").unwrap_err().to_string();
    assert!(e.contains("-22") && e.contains("EINVAL"), "{e}");
}

/// A whole 16-bit session: configure, stream, retune, gain, close.
#[test]
fn sixteen_bit_session() {
    let (st, conn) = fake("v0.39");
    let mut src = PlutoSource::start(
        conn,
        PlutoSettings {
            center_hz: 546e6,
            sample_rate: 64e6 / 7.0,
            ..Default::default()
        },
    )
    .unwrap();
    let c = src.control();
    assert!(!c.info().eight_bit);
    assert_eq!(c.info().firmware, "v0.39");

    let mut buf = Vec::new();
    let n = src.read(&mut buf).unwrap();
    assert_eq!(n, block_samples(64e6 / 7.0));
    // The fake's ramp: I = k mod 2000, Q = -I (full scale 2048).
    assert_eq!(buf[0], Iq::new(0.0, 0.0));
    assert!((buf[5].re - 5.0 / 2048.0).abs() < 1e-7);
    assert_eq!(buf[5].im, -buf[5].re);
    for w in buf.windows(2).take(1990) {
        assert!(((w[1].re - w[0].re) * 2048.0 - 1.0).abs() < 1e-3);
    }

    assert!(src.retune(600e6).unwrap());
    c.set_gain(PlutoGain::Agc).unwrap();
    src.read(&mut buf).unwrap();
    drop(src);

    let st = st.lock().unwrap();
    let log = st.log.join("\n");
    // Receive only: the TX LO powered down, the transmit device never touched.
    assert!(
        log.contains("WRITE iio:device0 OUTPUT altvoltage1 powerdown 1"),
        "{log}"
    );
    assert!(!log.contains("iio:device3"), "{log}");
    assert!(!log.contains("dds"), "{log}");
    assert_eq!(st.attrs["iio:device0 OUTPUT altvoltage1 powerdown"], "1");
    // I and Q enabled; the phy at the rate, the FPGA passing it through.
    assert!(
        log.contains("OPEN iio:device4 ") && log.contains(" 00000003"),
        "{log}"
    );
    assert_eq!(
        st.attrs["iio:device0 INPUT voltage0 sampling_frequency"],
        "9142857"
    );
    assert_eq!(
        st.attrs["iio:device4 INPUT voltage0 sampling_frequency"],
        "9142857"
    );
    assert_eq!(
        st.attrs["iio:device0 INPUT voltage0 rf_bandwidth"],
        "9142857"
    );
    assert_eq!(st.attrs["iio:device0 INPUT voltage0 hardwaregain"], "50");
    assert_eq!(
        st.attrs["iio:device0 OUTPUT altvoltage0 frequency"],
        "600000000"
    );
    assert_eq!(
        st.attrs["iio:device0 INPUT voltage0 gain_control_mode"],
        "slow_attack"
    );
    assert!(log.contains("CLOSE iio:device4"), "{log}");
}

/// Tezuka firmware: voltage0 alone, I8/Q8 pairs.
#[test]
fn tezuka_streams_eight_bit() {
    let (st, conn) = fake("tezuka-0.8.2");
    let mut src = PlutoSource::start(conn, PlutoSettings::default()).unwrap();
    assert!(src.control().info().eight_bit);
    assert!(src.describe().contains("8-bit"));
    let mut buf = Vec::new();
    let n = src.read(&mut buf).unwrap();
    assert_eq!(n, block_samples(64e6 / 7.0));
    assert!((buf[7].re - 7.0 / 128.0).abs() < 1e-7);
    assert_eq!(buf[7].im, -buf[7].re);
    drop(src);
    let log = st.lock().unwrap().log.join("\n");
    assert!(log.contains(" 00000001"), "{log}");

    // And 16-bit when asked, on the same firmware.
    let (st, conn) = fake("tezuka-0.8.2");
    let src = PlutoSource::start(
        conn,
        PlutoSettings {
            eight_bit: EightBit::Off,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!src.control().info().eight_bit);
    drop(src);
    assert!(st.lock().unwrap().log.join("\n").contains(" 00000003"));
}

#[test]
fn low_rates_use_the_fpga_decimator() {
    let (st, conn) = fake("v0.39");
    let src = PlutoSource::start(
        conn,
        PlutoSettings {
            center_hz: 145e6,
            sample_rate: 1e6,
            ..Default::default()
        },
    )
    .unwrap();
    drop(src);
    let st = st.lock().unwrap();
    assert_eq!(
        st.attrs["iio:device0 INPUT voltage0 sampling_frequency"],
        "8000000"
    );
    assert_eq!(
        st.attrs["iio:device4 INPUT voltage0 sampling_frequency"],
        "1000000"
    );
    // The RF filter never goes below the AD9361's 200 kHz.
    assert_eq!(
        st.attrs["iio:device0 INPUT voltage0 rf_bandwidth"],
        "1000000"
    );
}

#[test]
fn a_refused_frequency_is_an_error() {
    let (_st, conn) = fake("v0.39");
    let src = PlutoSource::start(conn, PlutoSettings::default()).unwrap();
    let e = src.control().set_center(5.8e9).unwrap_err().to_string();
    assert!(e.contains("refused"), "{e}");
    assert!(src.control().set_center(10e6).is_err());
    assert_eq!(src.control().settings().center_hz, 546e6);
}

/// The real TCP path (`ip:` URI, two connections) against the fake served on
/// loopback.
#[test]
fn over_tcp() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let st = Arc::new(Mutex::new(State {
        fw: "v0.39".into(),
        ..Default::default()
    }));
    {
        let st = Arc::clone(&st);
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { return };
                let st = Arc::clone(&st);
                std::thread::spawn(move || {
                    let mut link = FakeLink {
                        st,
                        inbox: Vec::new(),
                        outbox: VecDeque::new(),
                    };
                    let mut b = [0u8; 4096];
                    while let Ok(n) = sock.read(&mut b) {
                        if n == 0 {
                            return;
                        }
                        link.write_all(&b[..n]).unwrap();
                        let out: Vec<u8> = link.outbox.drain(..).collect();
                        // Rust note: TcpStream is both an io::Write and our
                        // Link; name the trait to pick io::Write's.
                        if Write::write_all(&mut sock, &out).is_err() {
                            return;
                        }
                    }
                });
            }
        });
    }
    let mut src = PlutoSource::open(PlutoSettings {
        uri: format!("ip:127.0.0.1:{port}").parse().unwrap(),
        sample_rate: 2.5e6,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(src.control().info().link, format!("ip:127.0.0.1:{port}"));
    let mut buf = Vec::new();
    for _ in 0..3 {
        assert_eq!(src.read(&mut buf).unwrap(), block_samples(2.5e6));
    }
    src.retune(433.92e6).unwrap();
    drop(src);
    let st = st.lock().unwrap();
    assert_eq!(
        st.attrs["iio:device0 OUTPUT altvoltage0 frequency"],
        "433920000"
    );
}

/// Live: `cargo test -p decsat-io --features pluto live_pluto -- --ignored
/// --nocapture` (env PLUTO_URI, default auto). Receive only.
#[test]
#[ignore = "needs a Pluto attached"]
fn live_pluto() {
    let uri = std::env::var("PLUTO_URI")
        .unwrap_or_default()
        .parse()
        .unwrap();
    let mut src = PlutoSource::open(PlutoSettings {
        uri,
        center_hz: 100e6,
        ..Default::default()
    })
    .expect("open");
    eprintln!("{:?}", src.control().info());
    let mut buf = Vec::new();
    let mut n = 0usize;
    let t0 = std::time::Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        n += src.read(&mut buf).expect("read");
    }
    let rate = n as f64 / t0.elapsed().as_secs_f64();
    eprintln!(
        "{} — {:.2} MS/s delivered, overflows {}",
        src.describe(),
        rate / 1e6,
        src.control().overflows()
    );
}
