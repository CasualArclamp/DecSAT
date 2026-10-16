// made by CasualArclamp
//! Playing a multicast audio stream: hand it to a media player on this
//! machine.
//!
//! - **RTP described by SDP** (announced by SAP, or a static payload type
//!   like 14 for MPEG audio) is relayed packet for packet to a local UDP port,
//!   with an SDP file pointing there. The player decodes whatever the SDP
//!   names — HE-AAC in LATM, RFC 3640 AAC, MPEG audio — so nothing is
//!   depayloaded here.
//! - **A bare elementary stream** (ADTS, LOAS or MPEG audio, raw in UDP or in
//!   RTP without a description) is served over HTTP with any RTP header
//!   taken off; players find the frames by their sync words.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};

use crate::mcast::{AudioStream, Codec, rtp_payload};
use crate::serve::StreamServer;

/// What to open in the player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayTarget {
    /// An SDP file (RTP relayed to the port it names).
    Sdp(PathBuf),
    /// An HTTP URL's address.
    Http(SocketAddr),
}

enum Mode {
    Rtp {
        sock: UdpSocket,
        to: SocketAddr,
    },
    Es {
        server: StreamServer,
        rtp: bool,
        mpa_header: bool,
    },
}

pub struct AudioRelay {
    pub group: IpAddr,
    pub port: u16,
    pub target: PlayTarget,
    pub forwarded: u64,
    mode: Mode,
}

/// A free local UDP port with the one above it free too (RTP and RTCP).
fn free_port_pair() -> io::Result<u16> {
    for _ in 0..20 {
        let probe = UdpSocket::bind("127.0.0.1:0")?;
        let p = probe.local_addr()?.port() & !1;
        drop(probe);
        if let (Ok(a), Ok(b)) = (
            UdpSocket::bind(("127.0.0.1", p)),
            UdpSocket::bind(("127.0.0.1", p + 1)),
        ) {
            drop((a, b));
            return Ok(p);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        "no free RTP port pair",
    ))
}

/// `sdp` re-aimed at 127.0.0.1:`port`.
fn local_sdp(s: &AudioStream, port: u16) -> String {
    let lines: Vec<String> = match &s.sdp {
        Some(sdp) => {
            let mut done_m = false;
            // Any line ending (some encoders end lines in a bare CR); the
            // file written uses CRLF throughout.
            sdp.raw
                .split(['\r', '\n'])
                .filter(|l| !l.is_empty())
                .map(|l| {
                    if l.starts_with("c=") {
                        "c=IN IP4 127.0.0.1".to_string()
                    } else if !done_m && l.starts_with("m=audio ") {
                        done_m = true;
                        let rest: Vec<&str> = l.split_whitespace().skip(2).collect();
                        format!("m=audio {port} {}", rest.join(" "))
                    } else {
                        l.to_string()
                    }
                })
                .collect()
        }
        None => vec![
            "v=0".into(),
            "o=- 0 0 IN IP4 127.0.0.1".into(),
            format!("s={}", s.name()),
            "c=IN IP4 127.0.0.1".into(),
            "t=0 0".into(),
            format!("m=audio {port} RTP/AVP {}", s.pt.unwrap_or(14)),
        ],
    };
    let mut out = lines.join("\r\n");
    out.push_str("\r\n");
    out
}

impl AudioRelay {
    /// Start playing `s`; an SDP file, if one is needed, goes in `dir`.
    pub fn start(s: &AudioStream, dir: &Path) -> io::Result<AudioRelay> {
        // Described: an SDP for the payload type the packets carry (the
        // scanner drops the SDP's payload type when it names another), or a
        // static payload type.
        let described = s
            .sdp
            .as_ref()
            .is_some_and(|d| d.pt.is_some() && d.pt == s.pt)
            || matches!(s.pt, Some(0..=34));
        let mode_target = if s.rtp && described {
            let port = free_port_pair()?;
            let sock = UdpSocket::bind("127.0.0.1:0")?;
            let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
            std::fs::create_dir_all(dir)?;
            let path =
                dir.join(format!("decsat-audio-{}-{}.sdp", s.group, s.port).replace(':', "_"));
            std::fs::write(&path, local_sdp(s, port))?;
            (Mode::Rtp { sock, to }, PlayTarget::Sdp(path))
        } else {
            let content_type = match s.codec {
                Codec::AacAdts | Codec::AacLatm => "audio/aac",
                Codec::MpegAudio => "audio/mpeg",
                Codec::Ts => "video/mp2t",
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "this stream needs an SDP description (none was announced)",
                    ));
                }
            };
            let server = StreamServer::bind("127.0.0.1:0".parse().unwrap(), content_type)?;
            let addr = server.addr;
            (
                Mode::Es {
                    server,
                    rtp: s.rtp,
                    mpa_header: s.rtp && s.pt == Some(14),
                },
                PlayTarget::Http(addr),
            )
        };
        Ok(AudioRelay {
            group: s.group,
            port: s.port,
            target: mode_target.1,
            forwarded: 0,
            mode: mode_target.0,
        })
    }

    /// One UDP payload of the stream.
    pub fn packet(&mut self, udp_payload: &[u8]) {
        match &mut self.mode {
            Mode::Rtp { sock, to } => {
                if sock.send_to(udp_payload, *to).is_ok() {
                    self.forwarded += 1;
                }
            }
            Mode::Es {
                server,
                rtp,
                mpa_header,
            } => {
                let body = if *rtp {
                    match rtp_payload(udp_payload) {
                        Some(b) if *mpa_header && b.len() > 4 => &b[4..],
                        Some(b) => b,
                        None => return,
                    }
                } else {
                    udp_payload
                };
                server.write(body.to_vec());
                self.forwarded += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcast::{SdpInfo, rtp_packet, silent_mp2_frame};
    use std::io::{Read, Write};
    use std::time::Duration;

    fn stream(rtp: bool, pt: Option<u8>, codec: Codec, sdp: Option<&str>) -> AudioStream {
        AudioStream {
            group: "239.255.1.1".parse().unwrap(),
            port: 5004,
            src: None,
            packets: 10,
            rate_bps: 128_000.0,
            rtp,
            pt,
            codec,
            sdp: sdp.map(SdpInfo::parse),
        }
    }

    #[test]
    fn relays_described_rtp_with_a_local_sdp() {
        let dir = std::env::temp_dir().join(format!("decsat-relay-{}", std::process::id()));
        let s = stream(
            true,
            Some(96),
            Codec::AacLatm,
            Some(
                "v=0\r\ns=Radio\r\nc=IN IP4 239.255.1.1/32\r\nm=audio 5004 RTP/AVP 96\r\na=rtpmap:96 MP4A-LATM/48000/2\r\n",
            ),
        );
        let mut r = AudioRelay::start(&s, &dir).unwrap();
        let PlayTarget::Sdp(path) = r.target.clone() else {
            panic!("{:?}", r.target)
        };
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(text.contains("c=IN IP4 127.0.0.1"));
        assert!(text.contains("a=rtpmap:96 MP4A-LATM/48000/2"));
        let port: u16 = text
            .lines()
            .find(|l| l.starts_with("m=audio"))
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap()
            .parse()
            .unwrap();
        // The player's end: what arrives is the RTP packet, unchanged.
        let rx = UdpSocket::bind(("127.0.0.1", port)).unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let pkt = rtp_packet(96, 1, 0, 9, b"latm");
        r.packet(&pkt);
        let mut buf = [0u8; 64];
        let (n, _) = rx.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], &pkt[..]);
    }

    #[test]
    fn serves_an_undescribed_stream_as_its_elementary_stream() {
        let s = stream(true, Some(14), Codec::MpegAudio, None);
        // Payload type 14 is static: that alone describes it.
        let r = AudioRelay::start(&s, &std::env::temp_dir()).unwrap();
        assert!(matches!(r.target, PlayTarget::Sdp(_)));
        if let PlayTarget::Sdp(p) = &r.target {
            let _ = std::fs::remove_file(p);
        }

        // Raw MPEG audio in UDP: served over HTTP as is.
        let s = stream(false, None, Codec::MpegAudio, None);
        let mut r = AudioRelay::start(&s, &std::env::temp_dir()).unwrap();
        let PlayTarget::Http(addr) = r.target.clone() else {
            panic!()
        };
        let mut c = std::net::TcpStream::connect(addr).unwrap();
        c.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
        std::thread::sleep(Duration::from_millis(600));
        let frame = silent_mp2_frame();
        r.packet(&frame);
        c.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut got = Vec::new();
        let mut b = [0u8; 512];
        while got.len() < 60 + frame.len() {
            let n = c.read(&mut b).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&b[..n]);
        }
        let text = String::from_utf8_lossy(&got);
        assert!(text.starts_with("HTTP/1.0 200 OK"));
        assert!(text.contains("audio/mpeg"));
        let body = got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        assert_eq!(&got[body..body + 4], &frame[..4]);
    }

    #[test]
    fn an_sdp_for_another_payload_type_does_not_describe_the_stream() {
        // Announced as type 14, sent as type 99 ADTS: served over HTTP as
        // the ADTS it is, not relayed with an SDP the player would follow.
        let sdp = SdpInfo::parse("v=0\rs=Newstalk_ZB\rm=audio 10001 RTP/AVP 14\r").for_pt(99);
        let mut s = stream(true, Some(99), Codec::AacAdts, None);
        s.sdp = Some(sdp);
        let r = AudioRelay::start(&s, &std::env::temp_dir()).unwrap();
        assert!(matches!(r.target, PlayTarget::Http(_)), "{:?}", r.target);
    }

    #[test]
    fn an_undescribed_dynamic_payload_cannot_be_guessed() {
        let s = stream(true, Some(96), Codec::AacRfc3640, None);
        assert!(AudioRelay::start(&s, &std::env::temp_dir()).is_err());
    }
}
