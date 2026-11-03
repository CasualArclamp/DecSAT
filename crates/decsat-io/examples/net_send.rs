// made by CasualArclamp
//! A stand-in for SDR++'s IQ Exporter: plays an IQ file out over the network
//! in real time, looped, so the network source can be tried without SDR++.
//!
//! ```text
//! cargo run --release -p decsat-io --example net_send -- FILE RATE URL [TYPE]
//!   URL  tcp://127.0.0.1:7777     listen there; the receiver connects (SDR++'s "TCP (Server)")
//!        connect://127.0.0.1:7777 connect to a receiver that listens ("TCP (Client)")
//!        udp://127.0.0.1:7777     send datagrams there ("UDP")
//!   TYPE int8 (default), uint8, int16, int32, float32 — scaled as SDR++ scales them
//! ```
//!
//! The file's format comes from its extension, as everywhere in DecSAT.
//! (Copied from DecLAB.)

use std::io::Write;
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use decsat_core::Iq;
use decsat_io::{IqFileReader, IqSource, NetSample, format_from_path};

/// Samples a packet, as SDR++'s default 1024-byte packets hold at int16.
const PACKET: usize = 256;

fn encode(t: NetSample, s: &[Iq], out: &mut Vec<u8>) {
    out.clear();
    for v in s {
        for x in [v.re, v.im] {
            match t {
                NetSample::Cs8 => out.push((x * 128.0).round().clamp(-128.0, 127.0) as i8 as u8),
                NetSample::Cu8 => out.push((x * 128.0 + 127.5).round().clamp(0.0, 255.0) as u8),
                NetSample::Cs16 => out.extend_from_slice(
                    &((x * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes(),
                ),
                NetSample::Cs32 => {
                    out.extend_from_slice(&((x as f64 * 2_147_483_647.0) as i32).to_le_bytes())
                }
                NetSample::Cf32 => out.extend_from_slice(&x.to_le_bytes()),
            }
        }
    }
}

enum Out {
    Tcp(TcpStream),
    Udp(UdpSocket, String),
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 3 {
        eprintln!(
            "net_send FILE RATE tcp://ADDR:PORT|connect://HOST:PORT|udp://HOST:PORT [int8|uint8|int16|int32|float32]"
        );
        std::process::exit(2);
    }
    let path = PathBuf::from(&a[0]);
    let rate: f64 = a[1]
        .trim_end_matches(['M', 'm'])
        .parse::<f64>()
        .expect("RATE")
        * if a[1].ends_with(['M', 'm']) { 1e6 } else { 1.0 };
    let (scheme, addr) = a[2].split_once("://").expect("URL");
    let t = a
        .get(3)
        .map(|s| NetSample::parse(s).expect("TYPE"))
        .unwrap_or(NetSample::Cs8);
    let fmt = format_from_path(&path).expect("format from extension");

    let mut out = match scheme {
        "tcp" => {
            let l = TcpListener::bind(addr).expect("bind");
            eprintln!("listening on {addr}; waiting for the receiver");
            let (s, peer) = l.accept().expect("accept");
            eprintln!("{peer} connected");
            Out::Tcp(s)
        }
        "connect" => Out::Tcp(TcpStream::connect(addr).expect("connect")),
        "udp" => Out::Udp(
            UdpSocket::bind("0.0.0.0:0").expect("bind"),
            addr.to_string(),
        ),
        s => panic!("unknown scheme {s}"),
    };

    let mut src = IqFileReader::open(&path, fmt, rate, PACKET).expect("open");
    let (mut buf, mut bytes) = (Vec::new(), Vec::new());
    let t0 = Instant::now();
    let mut sent = 0u64;
    loop {
        if src.read(&mut buf).expect("read") == 0 {
            src.rewind().expect("rewind");
            continue;
        }
        encode(t, &buf, &mut bytes);
        let ok = match &mut out {
            Out::Tcp(s) => s.write_all(&bytes).is_ok(),
            Out::Udp(u, to) => u.send_to(&bytes, to.as_str()).is_ok(),
        };
        if !ok {
            eprintln!(
                "the receiver went away after {:.1} s",
                t0.elapsed().as_secs_f64()
            );
            return;
        }
        sent += buf.len() as u64;
        // Real time: sleep off any lead over the wall clock.
        let ahead = sent as f64 / rate - t0.elapsed().as_secs_f64();
        if ahead > 0.002 {
            std::thread::sleep(Duration::from_secs_f64(ahead));
        }
    }
}
