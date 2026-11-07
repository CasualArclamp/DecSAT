// made by CasualArclamp
//! Terminal addresses seen on the forward link, their public IPs, and ICMP.
//!
//! Port of `bgan/terminals.py` from DecBGAN (Python, same author,
//! GPL-3.0-or-later).
//!
//! The forward link carries traffic **to** terminals, so every destination
//! address in a carved IPv4 packet is a terminal; sources are correspondents.
//! Two independent routes to a *public* address make the answer trustworthy:
//!
//! * **Echoed back.** A terminal that queries `myip.opendns.com` (A) or
//!   `o-o.myaddr.l.google.com` (TXT) is told its own public address, and the
//!   answer travels the forward link.
//! * **Addressed directly.** A public address appearing as a *destination* is
//!   a terminal that is not behind carrier NAT. An echo naming X that was
//!   delivered *to* X ([`Terminal::self_confirmed`]) is the strongest evidence.
//!
//! Confidence is reported, not assumed: one-off "destinations" are usually
//! correspondents (below [`MIN_PACKETS`] they are "weak").
//!
//! [`TerminalTracker`] is the incremental version of [`terminals`]; it carves
//! IPv4 and finds DNS messages as bytes arrive and keeps only counters.
//!
//! Python quirk kept on purpose: DNS compression pointers are followed as
//! offsets into the *whole payload*, not into the message (they should be
//! message-relative). Only a question name ending in a pointer can be affected.
//! The tracker keeps the first [`HEAD_BYTES`] of the stream to resolve them.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::net::IpAddr;

use super::findings::DnsScanner;
use super::pcap::{CarveOpts, CarvedPacket, Ipv4Carver};
use super::stream::ascii_lossy;

/// Names whose answer is the querier's own public address.
pub const ECHO_NAMES: [&str; 6] = [
    "myip.opendns.com",
    "o-o.myaddr.l.google.com",
    "myaddr.l.google.com",
    "whoami.akamai.net",
    "whoami.ds.akahelp.net",
    "resolver.dnscrypt.info",
];

/// A single carved packet is not evidence of a terminal; a run of them is.
pub const MIN_PACKETS: usize = 3;

/// Stream prefix kept to resolve DNS pointers (see module docs).
pub const HEAD_BYTES: usize = 64 * 1024;

/// `(type, code)` -> name.
pub fn icmp_type_name(t: u8, c: u8) -> Option<&'static str> {
    Some(match (t, c) {
        (0, 0) => "echo reply",
        (8, 0) => "echo request",
        (3, 0) => "net unreachable",
        (3, 1) => "host unreachable",
        (3, 2) => "protocol unreachable",
        (3, 3) => "port unreachable",
        (3, 4) => "fragmentation needed",
        (3, 13) => "administratively filtered",
        (5, 0) => "redirect net",
        (5, 1) => "redirect host",
        (11, 0) => "TTL exceeded",
        (11, 1) => "fragment reassembly time exceeded",
        (12, 0) => "parameter problem",
        _ => return None,
    })
}

fn addr(b: &[u8], o: usize) -> String {
    format!("{}.{}.{}.{}", b[o], b[o + 1], b[o + 2], b[o + 3])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddrKind {
    Public,
    Private,
}

impl AddrKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AddrKind::Public => "public",
            AddrKind::Private => "private",
        }
    }
}

/// `public` / `private` / `None` for anything that cannot be a terminal.
///
/// Classified explicitly rather than with a library's is_private/is_global,
/// whose meaning varies by version (on some, 100.64.0.0/10 -- the carrier-NAT
/// space a satellite operator uses -- is neither private nor global). Like the
/// Python, any IPv6 text is called public (the "not a host" and private lists
/// are IPv4 only).
pub fn kind(s: &str) -> Option<AddrKind> {
    match s.parse::<IpAddr>().ok()? {
        IpAddr::V6(_) => Some(AddrKind::Public),
        IpAddr::V4(a) => {
            let o = a.octets();
            // 0/8, loopback, link-local, multicast 224/4, reserved 240/4
            if o[0] == 0 || o[0] == 127 || (o[0] == 169 && o[1] == 254) || o[0] >= 224 {
                return None;
            }
            // RFC 1918 plus RFC 6598 carrier-grade NAT
            if o[0] == 10
                || (o[0] == 172 && (16..32).contains(&o[1]))
                || (o[0] == 192 && o[1] == 168)
                || (o[0] == 100 && (64..128).contains(&o[1]))
            {
                return Some(AddrKind::Private);
            }
            Some(AddrKind::Public)
        }
    }
}

// --- DNS, only enough of it to read an answer ---------------------------------

/// Byte access in "whole payload" coordinates: the message itself plus the
/// retained head of the stream.
struct Ctx<'a> {
    msg: &'a [u8],
    off: usize,
    head: &'a [u8],
}

impl Ctx<'_> {
    fn at(&self, abs: usize) -> Option<u8> {
        if abs >= self.off && abs - self.off < self.msg.len() {
            Some(self.msg[abs - self.off])
        } else {
            self.head.get(abs).copied()
        }
    }
}

/// Python `_name`: a possibly pointer-terminated name. The returned string may
/// be empty (callers treat that as "no name", like Python's falsy `""`).
fn dns_name(c: &Ctx, mut i: usize, end: usize, hops: usize) -> (Option<String>, usize) {
    let mut out: Vec<String> = Vec::new();
    while i < end {
        let Some(n) = c.at(i) else { return (None, i) };
        if n == 0 {
            return (Some(out.join(".")), i + 1);
        }
        if n & 0xC0 == 0xC0 {
            if hops > 8 || i + 1 >= end {
                return (None, i);
            }
            let Some(b1) = c.at(i + 1) else {
                return (None, i);
            };
            let (nm, _) = dns_name(c, (((n & 0x3F) as usize) << 8) | b1 as usize, end, hops + 1);
            if let Some(nm) = nm.filter(|s| !s.is_empty()) {
                out.push(nm);
            }
            return (Some(out.join(".")), i + 2);
        }
        i += 1;
        let n = n as usize;
        if i + n > end {
            return (None, i);
        }
        let mut lab = Vec::with_capacity(n);
        for k in 0..n {
            match c.at(i + k) {
                Some(x) => lab.push(x),
                None => return (None, i),
            }
        }
        out.push(ascii_lossy(&lab));
        i += n;
    }
    (None, i)
}

/// `(qname, value)` for the A and TXT answers of the DNS *response* that starts
/// at `msg[0]` (absolute offset `off`; `msg` runs to `min(end, off + 1500)`).
/// `head` is the stream prefix used to follow compression pointers.
pub fn dns_answers(msg: &[u8], off: usize, head: &[u8]) -> Vec<(String, String)> {
    let c = Ctx { msg, off, head };
    let mut out = Vec::new();
    let end = off + msg.len();
    let g16 = |rel: usize| -> Option<usize> {
        Some(((*msg.get(rel)? as usize) << 8) | *msg.get(rel + 1)? as usize)
    };
    let Some(flags) = g16(2) else { return out };
    if flags & 0x8000 == 0 || g16(4) != Some(1) {
        return out; // a query has no answers
    }
    let Some(an) = g16(6) else { return out };
    if !(1..=16).contains(&an) {
        return out;
    }
    let (q, mut j) = dns_name(&c, off + 12, end, 0);
    let q = match q {
        Some(q) if !q.is_empty() => q,
        _ => return out,
    };
    j += 4; // qtype, qclass
    for _ in 0..an {
        let (_, nj) = dns_name(&c, j, end, 0);
        j = nj;
        if j + 10 > end {
            return out;
        }
        let (Some(rtype), Some(rdlen)) = (g16(j - off), g16(j - off + 8)) else {
            return out;
        };
        j += 10;
        if j + rdlen > end || rdlen > 512 {
            return out;
        }
        let rd = &msg[j - off..j - off + rdlen];
        if rtype == 1 && rdlen == 4 {
            out.push((q.clone(), addr(rd, 0)));
        } else if rtype == 16 && rdlen > 1 && (rd[0] as usize) < rdlen {
            out.push((q.clone(), ascii_lossy(&rd[1..1 + rd[0] as usize])));
        }
        j += rdlen;
    }
    out
}

fn is_echo_name(q: &str) -> bool {
    let l = q.to_lowercase();
    ECHO_NAMES.iter().any(|e| l.contains(e))
}

// --- terminals -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    Confirmed,
    Probable,
    Weak,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Confirmed => "confirmed",
            Confidence::Probable => "probable",
            Confidence::Weak => "weak",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    pub addr: String,
    pub kind: AddrKind,
    pub packets: usize,
    /// Times its own address came back by DNS.
    pub echoed: usize,
    pub echo_names: Vec<String>,
    /// An echo naming it was delivered TO it.
    pub self_confirmed: bool,
}

impl Terminal {
    /// Two independent signals -> confirmed; one strong -> probable.
    pub fn confidence(&self) -> Confidence {
        if self.self_confirmed || (self.echoed > 0 && self.packets >= MIN_PACKETS) {
            Confidence::Confirmed
        } else if self.packets >= MIN_PACKETS {
            Confidence::Probable
        } else {
            Confidence::Weak
        }
    }
}

/// A DNS echo waiting for the carver to say which packet carried it.
#[derive(Debug)]
struct PendingEcho {
    offset: usize,
    /// Every value of an echo-named answer.
    all: Vec<String>,
    /// The public-address ones.
    public: Vec<String>,
}

/// Incremental terminal / NAT-pair tracker. See the module docs.
#[derive(Debug)]
pub struct TerminalTracker {
    carver: Ipv4Carver,
    dns: DnsScanner,
    head: Vec<u8>,
    /// Carved packet spans `(start, end, destination)`, offset-ordered.
    spans: VecDeque<(usize, usize, String)>,
    packets: HashMap<String, usize>,
    /// Packets seen in order of first appearance, for stable iteration.
    packet_order: Vec<String>,
    echoes: HashMap<String, (usize, BTreeSet<String>)>,
    echo_order: Vec<String>,
    pending: VecDeque<PendingEcho>,
    delivered: HashMap<String, HashMap<String, usize>>,
    nat: BTreeMap<String, BTreeMap<String, usize>>,
}

impl Default for TerminalTracker {
    fn default() -> Self {
        TerminalTracker {
            carver: Ipv4Carver::new(CarveOpts::default()),
            dns: DnsScanner::new(),
            head: Vec::new(),
            spans: VecDeque::new(),
            packets: HashMap::new(),
            packet_order: Vec::new(),
            echoes: HashMap::new(),
            echo_order: Vec::new(),
            pending: VecDeque::new(),
            delivered: HashMap::new(),
            nat: BTreeMap::new(),
        }
    }
}

impl TerminalTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.head.len() < HEAD_BYTES {
            let take = (HEAD_BYTES - self.head.len()).min(bytes.len());
            self.head.extend_from_slice(&bytes[..take]);
        }
        let carved = self.carver.push(bytes);
        self.absorb_packets(carved);
        let mut hits = Vec::new();
        let head = &self.head;
        self.dns.push(bytes, |f, msg| {
            hits.push((f.offset, dns_answers(msg, f.offset, head)))
        });
        self.absorb_dns(hits);
        self.resolve();
    }

    /// No more bytes: decide everything left.
    pub fn finish(&mut self) {
        let carved = self.carver.finish();
        self.absorb_packets(carved);
        let mut hits = Vec::new();
        let head = &self.head;
        self.dns
            .finish(|f, msg| hits.push((f.offset, dns_answers(msg, f.offset, head))));
        self.absorb_dns(hits);
        self.resolve();
    }

    fn absorb_packets(&mut self, carved: Vec<CarvedPacket>) {
        for p in carved {
            let dst = addr(&p.data, 16);
            if kind(&dst).is_some() {
                if !self.packets.contains_key(&dst) {
                    self.packet_order.push(dst.clone());
                }
                *self.packets.entry(dst.clone()).or_default() += 1;
            }
            self.spans
                .push_back((p.offset, p.offset + p.data.len(), dst));
        }
    }

    fn absorb_dns(&mut self, hits: Vec<(usize, Vec<(String, String)>)>) {
        for (offset, ans) in hits {
            let echo: Vec<&(String, String)> =
                ans.iter().filter(|(q, _)| is_echo_name(q)).collect();
            for (q, v) in &echo {
                if kind(v) == Some(AddrKind::Public) {
                    if !self.echoes.contains_key(v) {
                        self.echo_order.push(v.clone());
                    }
                    let e = self.echoes.entry(v.clone()).or_default();
                    e.0 += 1;
                    e.1.insert(q.clone());
                }
            }
            if echo.is_empty() {
                continue;
            }
            self.pending.push_back(PendingEcho {
                offset,
                all: echo.iter().map(|(_, v)| v.clone()).collect(),
                public: echo
                    .iter()
                    .filter(|(_, v)| kind(v) == Some(AddrKind::Public))
                    .map(|(_, v)| v.clone())
                    .collect(),
            });
        }
    }

    /// Settle echoes whose carrying packet is now known: every packet starting
    /// at or before the echo has been decided once the carver cursor passes it.
    fn resolve(&mut self) {
        let cur = self.carver.cursor();
        while self.pending.front().is_some_and(|p| p.offset < cur) {
            let p = self.pending.pop_front().unwrap();
            let span = self
                .spans
                .iter()
                .find(|(a, b, _)| *a <= p.offset && p.offset < *b);
            if let Some((_, _, to)) = span {
                let to = to.clone();
                for v in &p.all {
                    *self
                        .delivered
                        .entry(to.clone())
                        .or_default()
                        .entry(v.clone())
                        .or_default() += 1;
                }
                if kind(&to) == Some(AddrKind::Private) {
                    for v in &p.public {
                        *self
                            .nat
                            .entry(to.clone())
                            .or_default()
                            .entry(v.clone())
                            .or_default() += 1;
                    }
                }
            }
        }
        let low = self
            .pending
            .front()
            .map_or(self.dns.cursor(), |p| p.offset)
            .min(self.dns.cursor());
        while self.spans.front().is_some_and(|(_, e, _)| *e <= low) {
            self.spans.pop_front();
        }
    }

    /// Terminal addresses so far, strongest evidence first.
    pub fn terminals(&self) -> Vec<Terminal> {
        let mut out = Vec::new();
        for addr in &self.packet_order {
            let (hits, names) = self
                .echoes
                .get(addr)
                .map_or((0, Vec::new()), |(h, n)| (*h, n.iter().cloned().collect()));
            out.push(Terminal {
                addr: addr.clone(),
                kind: kind(addr).unwrap_or(AddrKind::Public),
                packets: self.packets[addr],
                echoed: hits,
                echo_names: names,
                self_confirmed: self
                    .delivered
                    .get(addr)
                    .and_then(|d| d.get(addr))
                    .is_some_and(|&n| n > 0),
            });
        }
        // An echoed address that never appeared as a destination is still a
        // terminal's public IP -- the terminal itself may be NATed.
        for ip in &self.echo_order {
            if !self.packets.contains_key(ip) {
                let (hits, names) = &self.echoes[ip];
                out.push(Terminal {
                    addr: ip.clone(),
                    kind: AddrKind::Public,
                    packets: 0,
                    echoed: *hits,
                    echo_names: names.iter().cloned().collect(),
                    self_confirmed: false,
                });
            }
        }
        out.sort_by(|a, b| {
            (a.confidence(), std::cmp::Reverse(a.packets), &a.addr).cmp(&(
                b.confidence(),
                std::cmp::Reverse(b.packets),
                &b.addr,
            ))
        });
        out
    }

    /// `{private_terminal: {public_ip: hits}}` where an echo reached a private
    /// host: only populated when a NATed terminal asks for its own address.
    pub fn nat_pairs(&self) -> BTreeMap<String, BTreeMap<String, usize>> {
        self.nat.clone()
    }

    /// `{public_ip: (hits, names)}` from "what is my IP" answers.
    pub fn echoed_ips(&self) -> BTreeMap<String, (usize, Vec<String>)> {
        self.echoes
            .iter()
            .map(|(k, (h, n))| (k.clone(), (*h, n.iter().cloned().collect())))
            .collect()
    }
}

fn run_tracker(blob: &[u8]) -> TerminalTracker {
    let mut t = TerminalTracker::new();
    t.push(blob);
    t.finish();
    t
}

/// Terminal addresses on the forward link, strongest evidence first.
pub fn terminals(blob: &[u8]) -> Vec<Terminal> {
    run_tracker(blob).terminals()
}

/// See [`TerminalTracker::nat_pairs`].
pub fn nat_pairs(blob: &[u8]) -> BTreeMap<String, BTreeMap<String, usize>> {
    run_tracker(blob).nat_pairs()
}

/// See [`TerminalTracker::echoed_ips`].
pub fn echoed_ips(blob: &[u8]) -> BTreeMap<String, (usize, Vec<String>)> {
    run_tracker(blob).echoed_ips()
}

// --- ICMP ------------------------------------------------------------------------

/// The IPv4 header quoted inside an ICMP error.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Quoted {
    pub src: String,
    pub dst: String,
    pub proto: u8,
    pub sport: u16,
    pub dport: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Icmp {
    pub offset: usize,
    pub src: String,
    pub dst: String,
    pub itype: u8,
    pub icode: u8,
    pub quoted: Option<Quoted>,
}

impl Icmp {
    pub fn name(&self) -> String {
        icmp_type_name(self.itype, self.icode).map_or_else(
            || format!("type {} code {}", self.itype, self.icode),
            str::to_string,
        )
    }
}

/// The quote inside an ICMP error, validated rather than trusted: it sits at a
/// fixed offset, so a mis-carved ICMP produces one anyway. Requires a v4
/// header with a legal IHL, a plausible length, a real protocol and routable
/// endpoints.
fn quoted(b: &[u8]) -> Option<Quoted> {
    if b.len() < 8 + 20 {
        return None;
    }
    let inner = &b[8..];
    if inner[0] >> 4 != 4 {
        return None;
    }
    let iihl = ((inner[0] & 0x0F) as usize) * 4;
    if iihl < 20 || inner.len() < iihl {
        return None;
    }
    let total = ((inner[2] as usize) << 8) | inner[3] as usize;
    if !(iihl..=65535).contains(&total) {
        return None;
    }
    let proto = inner[9];
    if ![1, 6, 17, 47, 50, 58].contains(&proto) {
        return None;
    }
    let (src, dst) = (addr(inner, 12), addr(inner, 16));
    if kind(&src).is_none() || kind(&dst).is_none() {
        return None;
    }
    let (mut sp, mut dp) = (0, 0);
    if (proto == 6 || proto == 17) && inner.len() >= iihl + 4 {
        sp = ((inner[iihl] as u16) << 8) | inner[iihl + 1] as u16;
        dp = ((inner[iihl + 2] as u16) << 8) | inner[iihl + 3] as u16;
    }
    Some(Quoted {
        src,
        dst,
        proto,
        sport: sp,
        dport: dp,
    })
}

fn icmp_from(c: &CarvedPacket) -> Option<Icmp> {
    let pkt = &c.data;
    let ihl = ((pkt[0] & 0x0F) as usize) * 4;
    let b = &pkt[ihl..];
    if b.len() < 8 {
        return None;
    }
    let (t, code) = (b[0], b[1]);
    icmp_type_name(t, code)?; // unknown type: almost always a bad carve
    let q = if matches!(t, 3 | 5 | 11 | 12) {
        quoted(b)
    } else {
        None
    };
    Some(Icmp {
        offset: c.offset,
        src: addr(pkt, 12),
        dst: addr(pkt, 16),
        itype: t,
        icode: code,
        quoted: q,
    })
}

/// Incremental ICMP finder (a protocol-1 IPv4 carve).
#[derive(Debug)]
pub struct IcmpScanner {
    carver: Ipv4Carver,
}

impl Default for IcmpScanner {
    fn default() -> Self {
        IcmpScanner {
            carver: Ipv4Carver::new(CarveOpts::with_protocols(&[1])),
        }
    }
}

impl IcmpScanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<Icmp> {
        self.carver
            .push(bytes)
            .iter()
            .filter_map(icmp_from)
            .collect()
    }

    pub fn finish(&mut self) -> Vec<Icmp> {
        self.carver.finish().iter().filter_map(icmp_from).collect()
    }
}

/// ICMP messages, with the header quoted inside an error decoded. The quote
/// names a flow that may never have been captured directly.
pub fn icmp(blob: &[u8]) -> Vec<Icmp> {
    let mut s = IcmpScanner::new();
    let mut v = s.push(blob);
    v.extend(s.finish());
    v
}

/// `(messages, name tally most-common first, flows revealed by quotes)`.
#[allow(clippy::type_complexity)]
pub fn icmp_summary(blob: &[u8]) -> (Vec<Icmp>, Vec<(String, usize)>, BTreeSet<Quoted>) {
    let msgs = icmp(blob);
    let mut tally: Vec<(String, usize)> = Vec::new();
    for m in &msgs {
        let n = m.name();
        match tally.iter_mut().find(|(k, _)| *k == n) {
            Some(t) => t.1 += 1,
            None => tally.push((n, 1)),
        }
    }
    tally.sort_by_key(|t| std::cmp::Reverse(t.1)); // stable: ties in first-seen order
    let flows = msgs.iter().filter_map(|m| m.quoted.clone()).collect();
    (msgs, tally, flows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ipv4(proto: u8, src: [u8; 4], dst: [u8; 4], body: &[u8]) -> Vec<u8> {
        let total = 20 + body.len();
        let mut h = vec![
            0x45,
            0,
            (total >> 8) as u8,
            total as u8,
            0,
            1,
            0,
            0,
            64,
            proto,
            0,
            0,
        ];
        h.extend(src);
        h.extend(dst);
        let mut s: u32 = h
            .chunks(2)
            .map(|c| ((c[0] as u32) << 8) | c[1] as u32)
            .sum();
        while s >> 16 != 0 {
            s = (s & 0xFFFF) + (s >> 16);
        }
        let ck = !(s as u16);
        h[10] = (ck >> 8) as u8;
        h[11] = ck as u8;
        h.extend(body);
        h
    }

    fn udp(sport: u16, dport: u16, body: &[u8]) -> Vec<u8> {
        let l = 8 + body.len();
        let mut u = vec![
            (sport >> 8) as u8,
            sport as u8,
            (dport >> 8) as u8,
            dport as u8,
            (l >> 8) as u8,
            l as u8,
            0,
            0,
        ];
        u.extend(body);
        u
    }

    /// DNS response to `myip.opendns.com` A with a compressed answer.
    fn echo_response(ip: [u8; 4]) -> Vec<u8> {
        let mut d = vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0];
        for l in ["myip", "opendns", "com"] {
            d.push(l.len() as u8);
            d.extend(l.bytes());
        }
        d.extend([0, 0, 1, 0, 1]);
        d.extend([0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
        d.extend(ip);
        d
    }

    fn sample() -> Vec<u8> {
        let mut b = vec![0u8; 9];
        // echo delivered to the very address it names -> self confirmed
        b.extend(ipv4(
            17,
            [208, 67, 222, 222],
            [159, 100, 53, 55],
            &udp(53, 40000, &echo_response([159, 100, 53, 55])),
        ));
        for _ in 0..3 {
            b.extend(ipv4(6, [1, 1, 1, 1], [159, 100, 53, 55], &[0; 30]));
        }
        // NATed terminal asking for its public address
        b.extend(ipv4(
            17,
            [208, 67, 222, 222],
            [10, 1, 1, 7],
            &udp(53, 40001, &echo_response([203, 0, 114, 9])),
        ));
        // one-off correspondent
        b.extend(ipv4(6, [10, 1, 1, 7], [17, 1, 1, 1], &[0; 30]));
        // ICMP port unreachable quoting a UDP flow
        let quote = ipv4(
            17,
            [10, 1, 1, 7],
            [8, 8, 8, 8],
            &[0x9c, 0x40, 0, 53, 0, 8, 0, 0],
        );
        let mut icmp_msg = vec![3, 3, 0, 0, 0, 0, 0, 0];
        icmp_msg.extend(&quote);
        b.extend(ipv4(1, [8, 8, 8, 8], [10, 1, 1, 7], &icmp_msg));
        b.extend([0u8; 20]);
        b
    }

    #[test]
    fn address_kinds() {
        assert_eq!(kind("10.1.2.3"), Some(AddrKind::Private));
        assert_eq!(kind("100.64.0.1"), Some(AddrKind::Private));
        assert_eq!(kind("172.31.0.1"), Some(AddrKind::Private));
        assert_eq!(kind("172.32.0.1"), Some(AddrKind::Public));
        assert_eq!(kind("159.100.53.55"), Some(AddrKind::Public));
        assert_eq!(kind("255.255.255.255"), None);
        assert_eq!(kind("224.0.0.1"), None);
        assert_eq!(kind("not an ip"), None);
    }

    #[test]
    fn terminals_echo_and_nat() {
        let b = sample();
        let t = terminals(&b);
        let me = t.iter().find(|x| x.addr == "159.100.53.55").unwrap();
        assert_eq!(me.packets, 4);
        assert_eq!(me.echoed, 1);
        assert!(me.self_confirmed);
        assert_eq!(me.confidence(), Confidence::Confirmed);
        assert_eq!(me.echo_names, ["myip.opendns.com"]);
        let nat = t.iter().find(|x| x.addr == "203.0.114.9").unwrap();
        assert_eq!(nat.packets, 0);
        assert_eq!(nat.echoed, 1);
        assert_eq!(nat.confidence(), Confidence::Weak);
        let one_off = t.iter().find(|x| x.addr == "17.1.1.1").unwrap();
        assert_eq!(one_off.confidence(), Confidence::Weak);
        assert_eq!(t[0].addr, "159.100.53.55"); // strongest first
        let pairs = nat_pairs(&b);
        assert_eq!(pairs["10.1.1.7"]["203.0.114.9"], 1);
        assert_eq!(echoed_ips(&b).len(), 2);
    }

    #[test]
    fn tracker_streaming_matches_one_shot() {
        let mut b = Vec::new();
        for _ in 0..4 {
            b.extend(vec![0x77u8; 5000]);
            b.extend(sample());
        }
        let want = (terminals(&b), nat_pairs(&b), echoed_ips(&b));
        for chunk in [61usize, 2000] {
            let mut t = TerminalTracker::new();
            for ch in b.chunks(chunk) {
                t.push(ch);
            }
            t.finish();
            assert_eq!(
                (t.terminals(), t.nat_pairs(), t.echoed_ips()),
                want,
                "chunk {chunk}"
            );
        }
    }

    #[test]
    fn icmp_quote_decoded() {
        let b = sample();
        let (msgs, tally, flows) = icmp_summary(&b);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].name(), "port unreachable");
        assert_eq!(tally, [("port unreachable".to_string(), 1)]);
        let q = flows.iter().next().unwrap();
        assert_eq!(
            (q.src.as_str(), q.dst.as_str(), q.proto, q.sport, q.dport),
            ("10.1.1.7", "8.8.8.8", 17, 0x9c40, 53)
        );
        // streaming agrees
        let mut s = IcmpScanner::new();
        let mut got = Vec::new();
        for ch in b.chunks(50) {
            got.extend(s.push(ch));
        }
        got.extend(s.finish());
        assert_eq!(got, msgs);
    }

    #[test]
    fn random_bytes_find_nothing_strong() {
        let mut x = 0xdead_beefu32;
        let b: Vec<u8> = (0..2_000_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 8) as u8
            })
            .collect();
        assert!(
            terminals(&b)
                .iter()
                .all(|t| t.confidence() == Confidence::Weak)
        );
        assert!(icmp(&b).is_empty());
    }
}
