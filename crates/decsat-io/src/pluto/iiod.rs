// made by CasualArclamp
//! The IIOD text protocol (libiio 0.x), the one every ADALM-PLUTO and Pluto+
//! firmware serves, over any byte stream: a TCP connection to port 30431 or a
//! USB pipe pair ([`super::usb`]).
//!
//! Commands, replies and the context XML follow libiio v0.25/0.26
//! (`iiod-client.c`, `iiod/parser.y`, `xml.c`; LGPL-2.1, Analog Devices),
//! read from source:
//!
//! * a command is one ASCII line ending `\r\n`;
//! * the reply starts with a decimal integer on its own line: negative is
//!   `-errno`, otherwise a value or the length of what follows;
//! * `READ` replies carry the value and a trailing `\n` after the length;
//!   `WRITE` sends the length on the command line, then the bytes;
//! * `READBUF dev n` replies in chunks: a length (0 ends), the channel mask
//!   as `%08x` words once, then that many raw bytes, until `n` bytes came.
//!
//! libiio v1 also has a binary protocol (`BINARY`), but no Pluto firmware
//! ships it yet (ADI v0.39 and Tezuka both run libiio 0.26), so it is not
//! spoken here.

use std::io;

use decsat_core::{Error, Result};

/// A byte stream to an IIOD server.
///
/// Rust note: a trait so the protocol is written once for TCP and USB, and
/// tested against an in-memory fake; `Send` lets a link move to the reader
/// thread.
pub trait Link: Send {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Read at least one byte into `buf` (blocking), returning how many.
    fn read_some(&mut self, buf: &mut [u8]) -> io::Result<usize>;
}

impl Link for std::net::TcpStream {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        io::Write::write_all(self, bytes)
    }
    fn read_some(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = io::Read::read(self, buf)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the Pluto closed the connection",
            ));
        }
        Ok(n)
    }
}

fn err(what: impl std::fmt::Display) -> Error {
    Error::Other(format!("Pluto: {what}"))
}

/// The client side of one IIOD connection, with its own receive buffer (a
/// reply line and the bytes after it may arrive in one TCP segment or USB
/// packet).
pub struct Client {
    link: Box<dyn Link>,
    rx: Vec<u8>,
    pos: usize,
}

impl Client {
    pub fn new(link: Box<dyn Link>) -> Client {
        Client {
            link,
            rx: Vec::with_capacity(1 << 16),
            pos: 0,
        }
    }

    fn fill(&mut self) -> Result<()> {
        if self.pos == self.rx.len() {
            self.rx.clear();
            self.pos = 0;
        }
        let start = self.rx.len();
        self.rx.resize(start + (1 << 16), 0);
        let n = self.link.read_some(&mut self.rx[start..]).map_err(err);
        self.rx.truncate(start + *n.as_ref().unwrap_or(&0));
        n.map(|_| ())
    }

    pub fn send(&mut self, cmd: &str) -> Result<()> {
        self.link.write_all(cmd.as_bytes()).map_err(err)
    }

    fn read_line(&mut self) -> Result<String> {
        loop {
            if let Some(i) = self.rx[self.pos..].iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(&self.rx[self.pos..self.pos + i]).into_owned();
                self.pos += i + 1;
                return Ok(line);
            }
            if self.rx.len() - self.pos > 4096 {
                return Err(err("a reply line that never ends"));
            }
            self.fill()?;
        }
    }

    /// Exactly `out.len()` bytes.
    fn read_exact(&mut self, out: &mut [u8]) -> Result<()> {
        let mut done = 0;
        while done < out.len() {
            if self.pos == self.rx.len() {
                // A large read goes straight into the caller's buffer.
                if out.len() - done >= 1 << 16 {
                    done += self.link.read_some(&mut out[done..]).map_err(err)?;
                    continue;
                }
                self.fill()?;
            }
            let n = (self.rx.len() - self.pos).min(out.len() - done);
            out[done..done + n].copy_from_slice(&self.rx[self.pos..self.pos + n]);
            self.pos += n;
            done += n;
        }
        Ok(())
    }

    /// The integer that starts every reply (libiio `iiod_client_read_integer`:
    /// blank lines are skipped).
    pub fn read_integer(&mut self) -> Result<i64> {
        loop {
            let line = self.read_line()?;
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            return t
                .parse()
                .map_err(|_| err(format!("expected a number, got {t:?}")));
        }
    }

    /// Send a command and return its integer reply; a negative one is an error.
    pub fn exec(&mut self, cmd: &str) -> Result<i64> {
        self.send(cmd)?;
        let v = self.read_integer()?;
        if v < 0 {
            return Err(err(format!(
                "{} refused: {}",
                cmd.trim_end(),
                errno_text(v)
            )));
        }
        Ok(v)
    }

    /// The context XML (`PRINT`).
    pub fn print(&mut self) -> Result<String> {
        let n = self.exec("PRINT\r\n")? as usize;
        let mut xml = vec![0u8; n + 1]; // and its trailing \n
        self.read_exact(&mut xml)?;
        xml.truncate(n);
        Ok(String::from_utf8_lossy(&xml).into_owned())
    }

    /// Read an attribute: `chan` is (channel id, output?) or none for a
    /// device attribute.
    pub fn read_attr(
        &mut self,
        dev: &str,
        chan: Option<(&str, bool)>,
        attr: &str,
    ) -> Result<String> {
        let cmd = match chan {
            Some((c, out)) => format!("READ {dev} {} {c} {attr}\r\n", dir(out)),
            None => format!("READ {dev} {attr}\r\n"),
        };
        let n = self.exec(&cmd)? as usize;
        let mut v = vec![0u8; n + 1];
        self.read_exact(&mut v)?;
        v.truncate(n);
        Ok(String::from_utf8_lossy(&v)
            .trim_end_matches('\0')
            .trim()
            .to_string())
    }

    pub fn write_attr(
        &mut self,
        dev: &str,
        chan: Option<(&str, bool)>,
        attr: &str,
        value: &str,
    ) -> Result<()> {
        let cmd = match chan {
            Some((c, out)) => format!("WRITE {dev} {} {c} {attr} {}\r\n", dir(out), value.len()),
            None => format!("WRITE {dev} {attr} {}\r\n", value.len()),
        };
        self.send(&cmd)?;
        self.link.write_all(value.as_bytes()).map_err(err)?;
        let v = self.read_integer()?;
        if v < 0 {
            return Err(err(format!("{attr} = {value} refused: {}", errno_text(v))));
        }
        Ok(())
    }

    /// `OPEN dev samples mask`: the mask's 32-bit words most significant
    /// first, `%08x` each (libiio `iiod_client_open_unlocked`).
    pub fn open(&mut self, dev: &str, samples: usize, mask: &[u32]) -> Result<()> {
        let words: String = mask.iter().rev().map(|w| format!("{w:08x}")).collect();
        self.exec(&format!("OPEN {dev} {samples} {words}\r\n"))
            .map(|_| ())
    }

    pub fn close(&mut self, dev: &str) -> Result<()> {
        self.exec(&format!("CLOSE {dev}\r\n")).map(|_| ())
    }

    /// Ask for `bytes` of samples; the reply is read by [`Client::readbuf_reply`].
    /// Sending the next request before reading this one's reply keeps the
    /// link busy across the round trip.
    pub fn readbuf_request(&mut self, dev: &str, bytes: usize) -> Result<()> {
        self.send(&format!("READBUF {dev} {bytes}\r\n"))
    }

    /// One `READBUF` reply into `out` (exactly `bytes`, unless the server
    /// ends early). `words` is the mask length the server echoes once.
    pub fn readbuf_reply(&mut self, out: &mut Vec<u8>, bytes: usize, words: usize) -> Result<()> {
        out.clear();
        out.resize(bytes, 0);
        let mut got = 0;
        let mut first = true;
        while got < bytes {
            let n = self.read_integer()?;
            if n < 0 {
                return Err(err(format!("reading samples: {}", errno_text(n))));
            }
            if n == 0 {
                break;
            }
            if first {
                // The mask, one line of `words` x 8 hex digits.
                let line = self.read_line()?;
                if line.trim().len() != words * 8 {
                    return Err(err(format!("unexpected mask line {line:?}")));
                }
                first = false;
            }
            let n = (n as usize).min(bytes - got);
            self.read_exact(&mut out[got..got + n])?;
            got += n;
        }
        out.truncate(got);
        Ok(())
    }
}

fn dir(output: bool) -> &'static str {
    if output { "OUTPUT" } else { "INPUT" }
}

fn errno_text(v: i64) -> String {
    let name = match -v {
        1 => "EPERM",
        2 => "ENOENT",
        5 => "EIO",
        9 => "EBADF",
        12 => "ENOMEM",
        16 => "EBUSY",
        19 => "ENODEV",
        22 => "EINVAL",
        110 => "ETIMEDOUT",
        _ => "",
    };
    format!("error {v} {name}").trim_end().to_string()
}

/// What the context XML says, as far as a receiver needs it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Context {
    /// `<context-attribute name value>`: `fw_version`, `hw_model`, …
    pub attrs: Vec<(String, String)>,
    pub devices: Vec<Device>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Device {
    /// `iio:deviceN`: what commands address.
    pub id: String,
    /// `ad9361-phy`, `cf-ad9361-lpc`, …
    pub name: String,
    pub channels: Vec<Channel>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Channel {
    pub id: String,
    pub output: bool,
    /// `<scan-element index>`: the channel's bit in a buffer mask.
    pub scan_index: Option<u32>,
    pub attrs: Vec<String>,
}

impl Context {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn device(&self, name: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.name == name || d.id == name)
    }
}

impl Device {
    pub fn channel(&self, id: &str, output: bool) -> Option<&Channel> {
        self.channels
            .iter()
            .find(|c| c.id == id && c.output == output)
    }

    /// Mask words for a buffer on this device (libiio: one bit per channel,
    /// `(channels + 31) / 32` words).
    pub fn mask_words(&self) -> usize {
        self.channels.len().div_ceil(32).max(1)
    }
}

/// Parse the context XML: a tag scanner for the few elements needed (the
/// format is libiio's own `iio_context_get_xml` output, not arbitrary XML).
pub fn parse_context(xml: &str) -> Result<Context> {
    let mut ctx = Context::default();
    // Inside a <channel> element: its <attribute>s are the channel's; after
    // </channel> they are the device's (not needed here).
    let mut in_channel = false;
    let mut rest = xml;
    while let Some(start) = rest.find('<') {
        let Some(end) = rest[start..].find('>') else {
            break;
        };
        let raw = &rest[start + 1..start + end];
        rest = &rest[start + end + 1..];
        let self_closing = raw.ends_with('/');
        let tag = raw.trim_end_matches('/').trim();
        let (name, body) = tag.split_once(char::is_whitespace).unwrap_or((tag, ""));
        let attr = |key: &str| xml_attr(body, key);
        match name {
            "context-attribute" => {
                if let (Some(n), Some(v)) = (attr("name"), attr("value")) {
                    ctx.attrs.push((n, v));
                }
            }
            "device" => ctx.devices.push(Device {
                id: attr("id").unwrap_or_default(),
                name: attr("name").unwrap_or_default(),
                channels: Vec::new(),
            }),
            "channel" => {
                let Some(dev) = ctx.devices.last_mut() else {
                    continue;
                };
                dev.channels.push(Channel {
                    id: attr("id").unwrap_or_default(),
                    output: attr("type").as_deref() == Some("output"),
                    scan_index: None,
                    attrs: Vec::new(),
                });
                in_channel = !self_closing;
            }
            "/channel" => in_channel = false,
            "scan-element" if in_channel => {
                if let Some(c) = ctx.devices.last_mut().and_then(|d| d.channels.last_mut()) {
                    c.scan_index = attr("index").and_then(|i| i.parse().ok());
                }
            }
            "attribute" if in_channel => {
                if let Some(c) = ctx.devices.last_mut().and_then(|d| d.channels.last_mut())
                    && let Some(n) = attr("name")
                {
                    c.attrs.push(n);
                }
            }
            _ => {}
        }
    }
    if ctx.devices.is_empty() {
        return Err(err("the context description lists no devices"));
    }
    Ok(ctx)
}

/// `key="value"` from a tag body, with XML's five entities undone.
fn xml_attr(body: &str, key: &str) -> Option<String> {
    let mut s = body;
    loop {
        let i = s.find(key)?;
        let before_ok = i == 0 || s.as_bytes()[i - 1].is_ascii_whitespace();
        let after = &s[i + key.len()..];
        if before_ok && let Some(v) = after.strip_prefix("=\"") {
            let end = v.find('"')?;
            return Some(unescape(&v[..end]));
        }
        s = &s[i + key.len()..];
    }
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}
