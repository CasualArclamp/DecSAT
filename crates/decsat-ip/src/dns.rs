// made by CasualArclamp
//! DNS in the UDP of a link: who asks for which names, who answers, and what
//! with (RFC 1035; mDNS is RFC 6762, LLMNR RFC 4795 — the same message
//! format on other ports).
//!
//! Only plain UDP is read. TCP DNS needs stream reassembly, and DNS over
//! TLS/HTTPS is encrypted — neither shows here.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// UDP ports DNS messages travel on.
const PORT_DNS: u16 = 53;
const PORT_MDNS: u16 = 5353;
const PORT_LLMNR: u16 = 5355;

/// Names kept; past this the least recently seen tenth is dropped, so a
/// scan of random names cannot grow the table without bound.
const MAX_NAMES: usize = 5000;
const MAX_CLIENTS: usize = 4096;
const MAX_RESOLVERS: usize = 256;
/// Per name: distinct clients and answers remembered.
const NAME_CLIENTS: usize = 32;
const NAME_ANSWERS: usize = 8;
/// Records read from one message's answer section.
const MAX_RECORDS: usize = 64;

/// Which naming service a message belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Dns,
    Mdns,
    Llmnr,
}

impl Service {
    pub fn label(self) -> &'static str {
        match self {
            Service::Dns => "DNS",
            Service::Mdns => "mDNS",
            Service::Llmnr => "LLMNR",
        }
    }
}

/// A record type's name (IANA "DNS Parameters"; RFC 1035 §3.2.2 for the
/// first ones).
pub fn type_name(t: u16) -> String {
    match t {
        1 => "A",
        2 => "NS",
        5 => "CNAME",
        6 => "SOA",
        12 => "PTR",
        15 => "MX",
        16 => "TXT",
        28 => "AAAA",
        33 => "SRV",
        35 => "NAPTR",
        41 => "OPT",
        43 => "DS",
        46 => "RRSIG",
        47 => "NSEC",
        48 => "DNSKEY",
        52 => "TLSA",
        64 => "SVCB",
        65 => "HTTPS",
        99 => "SPF",
        252 => "AXFR",
        255 => "ANY",
        257 => "CAA",
        _ => return format!("TYPE{t}"),
    }
    .to_string()
}

/// A response code's name (RFC 1035 §4.1.1, RFC 2136).
pub fn rcode_name(r: u8) -> String {
    match r {
        0 => "NOERROR",
        1 => "FORMERR",
        2 => "SERVFAIL",
        3 => "NXDOMAIN",
        4 => "NOTIMP",
        5 => "REFUSED",
        _ => return format!("RCODE{r}"),
    }
    .to_string()
}

/// One resource record of an answer, its data in words.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    name: String,
    rtype: u16,
    ttl: u32,
    text: String,
}

/// A message, as far as the table needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Message {
    response: bool,
    rcode: u8,
    /// The first question: name and type.
    question: Option<(String, u16)>,
    answers: Vec<Record>,
}

/// The name at `at` in `msg`, with the offset just past it where it started
/// (a compression pointer ends it there, RFC 1035 §4.1.4).
fn read_name(msg: &[u8], at: usize) -> Option<(String, usize)> {
    let mut name = String::new();
    let mut pos = at;
    let mut end = None;
    // A pointer loop would never end; a name is at most 255 bytes, so more
    // jumps than that are a loop.
    for _ in 0..128 {
        let len = *msg.get(pos)? as usize;
        match len & 0xC0 {
            0x00 => {
                if len == 0 {
                    let end = end.unwrap_or(pos + 1);
                    return Some((if name.is_empty() { ".".into() } else { name }, end));
                }
                let label = msg.get(pos + 1..pos + 1 + len)?;
                // Hosts' names are ASCII; anything else is not DNS (or is
                // escaped on the way out).
                for &b in label {
                    if b.is_ascii_graphic() {
                        name.push(b.to_ascii_lowercase() as char);
                    } else {
                        return None;
                    }
                }
                name.push('.');
                if name.len() > 255 {
                    return None;
                }
                pos += 1 + len;
            }
            0xC0 => {
                let low = *msg.get(pos + 1)? as usize;
                end.get_or_insert(pos + 2);
                pos = (len & 0x3F) << 8 | low;
            }
            _ => return None, // the reserved label types
        }
    }
    None
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

/// A record's data in words (RFC 1035 §3.3 and later RFCs per type).
fn rdata_text(msg: &[u8], rtype: u16, at: usize, len: usize) -> String {
    let data = match msg.get(at..at + len) {
        Some(d) => d,
        None => return String::new(),
    };
    let name_at = |off: usize| read_name(msg, off).map(|(n, _)| n);
    match rtype {
        1 if len == 4 => Ipv4Addr::new(data[0], data[1], data[2], data[3]).to_string(),
        28 if len == 16 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(data);
            Ipv6Addr::from(a).to_string()
        }
        2 | 5 | 12 => name_at(at).unwrap_or_default(),
        15 => match (be16(msg, at), name_at(at + 2)) {
            (Some(p), Some(n)) => format!("{p} {n}"),
            _ => String::new(),
        },
        33 => match (
            be16(msg, at),
            be16(msg, at + 2),
            be16(msg, at + 4),
            name_at(at + 6),
        ) {
            (Some(p), Some(_w), Some(port), Some(n)) => format!("{n}:{port} (prio {p})"),
            _ => String::new(),
        },
        6 => name_at(at).unwrap_or_default(),
        16 | 99 => {
            // Length-prefixed strings.
            let mut out = Vec::new();
            let mut i = 0;
            while i < data.len() {
                let n = data[i] as usize;
                let Some(s) = data.get(i + 1..i + 1 + n) else {
                    break;
                };
                out.push(String::from_utf8_lossy(s).into_owned());
                i += 1 + n;
            }
            format!("\"{}\"", out.join(""))
        }
        _ => format!("{len} bytes"),
    }
}

/// Parse a DNS message, strictly enough that a stray datagram on port 53
/// is turned away: the header's counts must fit, the first question and
/// every answer read must be well formed.
fn parse(msg: &[u8]) -> Option<Message> {
    if msg.len() < 12 {
        return None;
    }
    let flags = be16(msg, 2)?;
    let (qd, an) = (be16(msg, 4)? as usize, be16(msg, 6)? as usize);
    let opcode = (flags >> 11) & 0xF;
    if opcode > 5 || qd > 8 || (qd == 0 && an == 0) {
        return None;
    }
    let response = flags & 0x8000 != 0;
    let mut at = 12;
    let mut question = None;
    for _ in 0..qd {
        let (name, next) = read_name(msg, at)?;
        let qtype = be16(msg, next)?;
        be16(msg, next + 2)?; // the class
        question.get_or_insert((name, qtype));
        at = next + 4;
    }
    let mut answers = Vec::new();
    for _ in 0..an.min(MAX_RECORDS) {
        // A truncated message (the TC bit) or a count too big for what
        // came: keep the records that did parse.
        let Some((name, next)) = read_name(msg, at) else {
            break;
        };
        let (Some(rtype), Some(ttl_hi), Some(ttl_lo), Some(rdlen)) = (
            be16(msg, next),
            be16(msg, next + 4),
            be16(msg, next + 6),
            be16(msg, next + 8),
        ) else {
            break;
        };
        let rdlen = rdlen as usize;
        let data_at = next + 10;
        if data_at + rdlen > msg.len() {
            break;
        }
        answers.push(Record {
            name,
            rtype,
            ttl: (ttl_hi as u32) << 16 | ttl_lo as u32,
            text: rdata_text(msg, rtype, data_at, rdlen),
        });
        at = data_at + rdlen;
    }
    Some(Message {
        response,
        rcode: (flags & 0xF) as u8,
        question,
        answers,
    })
}

/// What is known of one (name, type).
#[derive(Debug, Clone)]
struct Entry {
    service: Service,
    queries: u64,
    responses: u64,
    rcode: Option<u8>,
    answers: Vec<String>,
    ttl: Option<u32>,
    clients: Vec<IpAddr>,
    resolver: Option<IpAddr>,
    /// Signal time it was last seen at (s).
    last: f64,
}

/// Who sent a message and when, for [`DnsRx::touch`].
struct Who {
    service: Service,
    response: bool,
    client: IpAddr,
    resolver: IpAddr,
    now: f64,
}

impl Entry {
    /// Note an answer's data, once.
    fn add_answer(&mut self, r: &Record) {
        let text = if r.text.is_empty() {
            type_name(r.rtype)
        } else if matches!(r.rtype, 1 | 28) {
            r.text.clone()
        } else {
            format!("{} {}", type_name(r.rtype), r.text)
        };
        if self.answers.len() < NAME_ANSWERS && !self.answers.contains(&text) {
            self.answers.push(text);
        }
        self.ttl.get_or_insert(r.ttl);
    }
}

/// One name of the view.
#[derive(Debug, Clone)]
pub struct DnsName {
    pub name: String,
    pub qtype: u16,
    pub service: Service,
    pub queries: u64,
    pub responses: u64,
    pub rcode: Option<u8>,
    /// The data the answers carried, distinct, as text.
    pub answers: Vec<String>,
    pub ttl: Option<u32>,
    pub clients: usize,
    pub resolver: Option<IpAddr>,
    /// Seconds since it was last seen.
    pub age: f64,
}

/// All of it, for a display.
#[derive(Debug, Clone, Default)]
pub struct DnsView {
    pub queries: u64,
    pub responses: u64,
    /// Port-53 datagrams that did not read as DNS.
    pub unparsed: u64,
    /// Distinct names (with types) seen, and the most recent of them.
    pub names_total: usize,
    pub names: Vec<DnsName>,
    pub clients: usize,
    /// Servers that answered (or were asked), busiest first.
    pub resolvers: Vec<(IpAddr, u64)>,
}

#[derive(Debug, Default)]
pub struct DnsRx {
    names: HashMap<(String, u16), Entry>,
    resolvers: HashMap<IpAddr, u64>,
    clients: HashSet<IpAddr>,
    queries: u64,
    responses: u64,
    /// Datagrams on port 53 that were not DNS (a garbled or encrypted
    /// payload, or another protocol).
    unparsed: u64,
}

impl DnsRx {
    pub fn new() -> Self {
        Self::default()
    }

    /// Any DNS has been seen.
    pub fn active(&self) -> bool {
        self.queries + self.responses + self.unparsed > 0
    }

    /// A UDP datagram: `src` and `dst` are (address, port), `now` the signal
    /// time (s). Datagrams that are not DNS are ignored.
    pub fn udp(&mut self, src: (IpAddr, u16), dst: (IpAddr, u16), payload: &[u8], now: f64) {
        let service = match (src.1, dst.1) {
            (PORT_DNS, _) | (_, PORT_DNS) => Service::Dns,
            (PORT_MDNS, _) | (_, PORT_MDNS) => Service::Mdns,
            (PORT_LLMNR, _) | (_, PORT_LLMNR) => Service::Llmnr,
            _ => return,
        };
        let Some(m) = parse(payload) else {
            if service == Service::Dns {
                self.unparsed += 1;
            }
            return;
        };
        // A unicast DNS query goes to port 53 and its response comes from
        // it; the multicast services use their port both ways. A message
        // whose direction contradicts its QR bit is some other protocol.
        if service == Service::Dns && m.response != (src.1 == PORT_DNS) {
            self.unparsed += 1;
            return;
        }
        let (client, resolver) = if m.response {
            (dst.0, src.0)
        } else {
            (src.0, dst.0)
        };
        if m.response {
            self.responses += 1;
        } else {
            self.queries += 1;
        }
        if service == Service::Dns {
            if self.resolvers.len() < MAX_RESOLVERS || self.resolvers.contains_key(&resolver) {
                *self.resolvers.entry(resolver).or_insert(0) += 1;
            }
            if self.clients.len() < MAX_CLIENTS {
                self.clients.insert(client);
            }
        }
        let who = Who {
            service,
            response: m.response,
            client,
            resolver,
            now,
        };
        // File the message's answers under its question, else (the
        // multicast announcements carry none) under each answer's own name.
        match &m.question {
            Some((name, qtype)) => {
                let e = self.touch((name.clone(), *qtype), &who);
                if m.response {
                    e.rcode = Some(m.rcode);
                    for r in &m.answers {
                        e.add_answer(r);
                    }
                }
            }
            None if m.response => {
                for r in &m.answers {
                    self.touch((r.name.clone(), r.rtype), &who).add_answer(r);
                }
            }
            None => {}
        }
    }

    /// The entry for `key`, made if new, with this message counted in it.
    fn touch(&mut self, key: (String, u16), who: &Who) -> &mut Entry {
        if self.names.len() >= MAX_NAMES && !self.names.contains_key(&key) {
            self.evict();
        }
        let e = self.names.entry(key).or_insert_with(|| Entry {
            service: who.service,
            queries: 0,
            responses: 0,
            rcode: None,
            answers: Vec::new(),
            ttl: None,
            clients: Vec::new(),
            resolver: None,
            last: who.now,
        });
        e.last = who.now;
        if who.response {
            e.responses += 1;
            e.resolver = Some(who.resolver);
        } else {
            e.queries += 1;
        }
        if who.service == Service::Dns
            && e.clients.len() < NAME_CLIENTS
            && !e.clients.contains(&who.client)
        {
            e.clients.push(who.client);
        }
        e
    }

    /// Drop the tenth of the names seen longest ago.
    fn evict(&mut self) {
        let mut by_age: Vec<_> = self
            .names
            .iter()
            .map(|(k, e)| (e.last, k.clone()))
            .collect();
        by_age.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (_, k) in by_age.into_iter().take(MAX_NAMES / 10) {
            self.names.remove(&k);
        }
    }

    /// The `limit` most recent names, and the totals; `now` is the signal
    /// time (s).
    pub fn view(&self, now: f64, limit: usize) -> DnsView {
        let mut recent: Vec<(&(String, u16), &Entry)> = self.names.iter().collect();
        // Newest first; only the first `limit` need to be in order.
        let newest = |a: &(&(String, u16), &Entry), b: &(&(String, u16), &Entry)| {
            b.1.last.total_cmp(&a.1.last)
        };
        if recent.len() > limit {
            recent.select_nth_unstable_by(limit, newest);
            recent.truncate(limit);
        }
        recent.sort_by(newest);
        let mut resolvers: Vec<(IpAddr, u64)> =
            self.resolvers.iter().map(|(a, n)| (*a, *n)).collect();
        resolvers.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        DnsView {
            queries: self.queries,
            responses: self.responses,
            unparsed: self.unparsed,
            names_total: self.names.len(),
            names: recent
                .into_iter()
                .map(|((name, qtype), e)| DnsName {
                    name: name.clone(),
                    qtype: *qtype,
                    service: e.service,
                    queries: e.queries,
                    responses: e.responses,
                    rcode: e.rcode,
                    answers: e.answers.clone(),
                    ttl: e.ttl,
                    clients: e.clients.len(),
                    resolver: e.resolver,
                    age: (now - e.last).max(0.0),
                })
                .collect(),
            clients: self.clients.len(),
            resolvers,
        }
    }
}

/// A DNS A query for `name` (id `id`), or with `answer` its response with
/// that address — for the test signals.
pub fn test_message(id: u16, name: &str, answer: Option<[u8; 4]>) -> Vec<u8> {
    let mut m = id.to_be_bytes().to_vec();
    let (flags, an) = if answer.is_some() {
        (0x8180u16, 1u16)
    } else {
        (0x0100, 0)
    };
    for w in [flags, 1, an, 0, 0] {
        m.extend_from_slice(&w.to_be_bytes());
    }
    for label in name.split('.') {
        m.push(label.len() as u8);
        m.extend_from_slice(label.as_bytes());
    }
    m.extend_from_slice(&[0, 0, 1, 0, 1]); // root, type A, class IN
    if let Some(a) = answer {
        // Owner name: a pointer to the question's (0xC00C), TTL 300 s.
        m.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0x01, 0x2C, 0, 4]);
        m.extend_from_slice(&a);
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(n: &str) -> Vec<u8> {
        let mut v = Vec::new();
        for l in n.trim_end_matches('.').split('.') {
            v.push(l.len() as u8);
            v.extend_from_slice(l.as_bytes());
        }
        v.push(0);
        v
    }

    fn header(id: u16, flags: u16, qd: u16, an: u16) -> Vec<u8> {
        let mut v = id.to_be_bytes().to_vec();
        for w in [flags, qd, an, 0, 0] {
            v.extend_from_slice(&w.to_be_bytes());
        }
        v
    }

    fn query(n: &str, qtype: u16) -> Vec<u8> {
        let mut v = header(7, 0x0100, 1, 0);
        v.extend(name(n));
        v.extend_from_slice(&qtype.to_be_bytes());
        v.extend_from_slice(&1u16.to_be_bytes());
        v
    }

    /// A response to `query(n, qtype)` with a CNAME and then an A record,
    /// the owner names compressed to the question's (0xC00C).
    fn response(n: &str, rcode: u16) -> Vec<u8> {
        let mut v = header(7, 0x8180 | rcode, 1, 2);
        v.extend(name(n));
        v.extend_from_slice(&1u16.to_be_bytes());
        v.extend_from_slice(&1u16.to_be_bytes());
        let cname = name("edge.example.net");
        v.extend_from_slice(&[0xC0, 0x0C, 0, 5, 0, 1, 0, 0, 0x01, 0x2C]);
        v.extend_from_slice(&(cname.len() as u16).to_be_bytes());
        v.extend(cname);
        v.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 192, 0, 2, 7]);
        v
    }

    fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn a_query_and_its_response_make_one_entry() {
        let mut rx = DnsRx::new();
        let (client, server) = (ip(10, 0, 0, 5), ip(8, 8, 8, 8));
        rx.udp(
            (client, 40000),
            (server, 53),
            &query("Www.Example.com", 1),
            1.0,
        );
        rx.udp(
            (server, 53),
            (client, 40000),
            &response("www.example.com", 0),
            1.2,
        );
        let v = rx.view(2.0, 10);
        assert_eq!((v.queries, v.responses, v.names_total), (1, 1, 1));
        let n = &v.names[0];
        assert_eq!(n.name, "www.example.com.");
        assert_eq!(n.qtype, 1);
        assert_eq!((n.queries, n.responses, n.clients), (1, 1, 1));
        assert_eq!(n.rcode, Some(0));
        assert_eq!(n.answers, vec!["CNAME edge.example.net.", "192.0.2.7"]);
        assert_eq!(n.resolver, Some(server));
        assert!((n.age - 0.8).abs() < 1e-9);
        assert_eq!(v.resolvers, vec![(server, 2)]);
    }

    #[test]
    fn nxdomain_is_kept() {
        let mut rx = DnsRx::new();
        let mut m = header(1, 0x8183, 1, 0);
        m.extend(name("nope.invalid"));
        m.extend_from_slice(&[0, 28, 0, 1]);
        rx.udp((ip(1, 1, 1, 1), 53), (ip(10, 0, 0, 5), 999), &m, 0.0);
        let n = &rx.view(0.0, 10).names[0];
        assert_eq!(n.rcode, Some(3));
        assert_eq!(rcode_name(3), "NXDOMAIN");
        assert_eq!(type_name(n.qtype), "AAAA");
    }

    #[test]
    fn other_traffic_on_port_53_is_not_dns() {
        let mut rx = DnsRx::new();
        // Noise, and a query-looking message heading the wrong way.
        rx.udp((ip(1, 1, 1, 1), 53), (ip(2, 2, 2, 2), 9), &[0xFF; 40], 0.0);
        rx.udp(
            (ip(1, 1, 1, 1), 53),
            (ip(2, 2, 2, 2), 9),
            &query("a.b", 1),
            0.0,
        );
        rx.udp(
            (ip(1, 1, 1, 1), 80),
            (ip(2, 2, 2, 2), 9),
            &query("a.b", 1),
            0.0,
        );
        let v = rx.view(0.0, 10);
        assert_eq!((v.queries, v.responses, v.unparsed), (0, 0, 2));
    }

    #[test]
    fn a_pointer_loop_does_not_hang() {
        let mut m = header(1, 0x0100, 1, 0);
        m.extend_from_slice(&[0xC0, 12]); // points at itself
        m.extend_from_slice(&[0, 1, 0, 1]);
        assert!(parse(&m).is_none());
    }

    #[test]
    fn mdns_announcements_without_a_question_are_listed_by_their_own_names() {
        let mut m = header(0, 0x8400, 0, 1);
        m.extend(name("printer.local"));
        m.extend_from_slice(&[0, 1, 0x80, 1, 0, 0, 0, 120, 0, 4, 192, 168, 1, 9]);
        let mut rx = DnsRx::new();
        rx.udp(
            (ip(192, 168, 1, 9), 5353),
            (ip(224, 0, 0, 251), 5353),
            &m,
            3.0,
        );
        let n = &rx.view(3.0, 10).names[0];
        assert_eq!(n.name, "printer.local.");
        assert_eq!(n.service, Service::Mdns);
        assert_eq!(n.answers, vec!["192.168.1.9"]);
        assert_eq!(n.ttl, Some(120));
    }

    #[test]
    fn the_table_is_bounded_and_the_view_is_newest_first() {
        let mut rx = DnsRx::new();
        let (c, s) = (ip(10, 0, 0, 5), ip(8, 8, 8, 8));
        for i in 0..(MAX_NAMES + 100) {
            rx.udp(
                (c, 1000),
                (s, 53),
                &query(&format!("h{i}.example.com"), 1),
                i as f64,
            );
        }
        let v = rx.view(1e6, 3);
        assert!(v.names_total <= MAX_NAMES);
        assert_eq!(v.names.len(), 3);
        assert_eq!(v.names[0].name, format!("h{}.example.com.", MAX_NAMES + 99));
        assert!(v.names[0].age <= v.names[1].age && v.names[1].age <= v.names[2].age);
    }
}
