// made by CasualArclamp
//! Cross-check against real decoded payloads and the Python reference.
//!
//! These tests are `#[ignore]`d: they need data that is never committed.
//!
//! ```text
//! DECBGAN_PAYLOAD=F:\codebases\DecBGAN\out\X_payload.bin   payload to scan
//! DECBGAN_REF=path\to\X.py.txt   optional: dump written by the Python reference
//!                                (a script printing the lines built in `dump` below)
//! DECBGAN_DUMP=path\to\out.txt   optional: where to write this crate's dump
//! cargo test --release -p decsat-bgan -- --ignored realdata --nocapture
//! ```
//!
//! `payload_matches_python` compares the dump line by line;
//! `streaming_equals_one_shot` feeds the payload in odd-sized chunks to every
//! incremental scanner and checks the results equal the one-shot functions.

use std::fmt::Write as _;

use super::*;

fn payload() -> Option<Vec<u8>> {
    let p = std::env::var("DECBGAN_PAYLOAD").ok()?;
    Some(std::fs::read(p).expect("read DECBGAN_PAYLOAD"))
}

/// Python `json.dumps(s)` (ensure_ascii).
fn q(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '\x08' => o.push_str("\\b"),
            '\x0c' => o.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c if (0x20..0x7f).contains(&(c as u32)) => o.push(c),
            c => {
                let mut b = [0u16; 2];
                for u in c.encode_utf16(&mut b) {
                    let _ = write!(o, "\\u{u:04x}");
                }
            }
        }
    }
    o.push('"');
    o
}

fn qlist<S: AsRef<str>>(v: &[S]) -> String {
    format!(
        "[{}]",
        v.iter()
            .map(|s| q(s.as_ref()))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn crc(b: &[u8]) -> u32 {
    let mut c = flate2::Crc::new();
    c.update(b);
    c.sum()
}

fn pyq(x: &Option<terminals::Quoted>) -> String {
    match x {
        None => "()".into(),
        Some(q) => format!(
            "('{}', '{}', {}, {}, {})",
            q.src, q.dst, q.proto, q.sport, q.dport
        ),
    }
}

fn pts(p: &[(i32, i32)]) -> String {
    format!(
        "[{}]",
        p.iter()
            .map(|(a, b)| format!("({a}, {b})"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn dump(blob: &[u8]) -> Vec<String> {
    let mut l: Vec<String> = Vec::new();
    for (o, n) in bctpdu::find_pdus_default(blob) {
        l.push(format!("pdu {o} {n}"));
    }
    for (o, p) in pcap::carve_ipv4(blob) {
        l.push(format!("ipv4 {o} {}", p.len()));
    }
    let fs = findings::scan(blob);
    for f in &fs {
        l.push(format!(
            "find {} {} {} {}",
            f.kind.name(),
            f.offset,
            q(&f.summary),
            qlist(&f.detail)
        ));
    }
    let mut hosts = findings::hosts(&fs);
    hosts.sort();
    for (h, c) in hosts {
        l.push(format!("host {} {c}", q(&h)));
    }
    for d in files::documents(blob) {
        l.push(format!(
            "doc {} {} {} {} {} {} {} {} {:08x}",
            d.offset,
            q(&d.status),
            q(&d.ctype),
            d.declared,
            d.data.len(),
            d.check().as_str(),
            q(&d.note),
            d.name(),
            crc(&d.data)
        ));
    }
    let msgs = sip::messages(blob);
    for m in &msgs {
        let hd = m
            .headers
            .iter()
            .map(|(k, v)| format!("[{}, {}]", q(k), q(v)))
            .collect::<Vec<_>>()
            .join(", ");
        l.push(format!(
            "sip {} {} {} {} {} {} {} {} {} [{hd}]",
            m.offset,
            if m.kind == sip::SipKind::Request {
                "request"
            } else {
                "response"
            },
            q(&m.start),
            q(&m.method),
            m.status,
            q(&m.reason),
            m.check(),
            u8::from(m.damaged),
            m.body.len()
        ));
    }
    for d in sip::dialogs(&msgs) {
        let media = d
            .media()
            .iter()
            .map(|m| {
                format!(
                    "[{}, {}, {}, {}, {}]",
                    q(&m.kind),
                    m.port,
                    q(&m.proto),
                    qlist(&m.formats),
                    q(&m.address)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        l.push(format!(
            "dialog {} {} {} {} {} {} {} [{media}]",
            q(&d.call_id),
            d.offset(),
            d.messages.len(),
            q(&d.outcome()),
            q(&d.from_uri()),
            q(&d.to_uri()),
            qlist(&d.methods())
        ));
    }
    for s in rtp::streams(blob) {
        let pcm = s.pcm();
        let bytes: Vec<u8> = pcm.iter().flat_map(|x| x.to_le_bytes()).collect();
        l.push(format!(
            "rtp {} {} {} {} {} {} {} {} {} {:.6} {} {:08x}",
            s.ssrc,
            s.src,
            s.dst,
            s.sport,
            s.dport,
            s.pt,
            s.packets.len(),
            s.offset(),
            s.lost(),
            s.density(),
            pcm.len(),
            crc(&bytes)
        ));
    }
    l.push(format!("rtppackets {}", rtp::packets(blob).len()));
    for t in terminals::terminals(blob) {
        l.push(format!(
            "term {} {} {} {} {} {} {}",
            t.addr,
            t.kind.as_str(),
            t.packets,
            t.echoed,
            qlist(&t.echo_names),
            u8::from(t.self_confirmed),
            t.confidence().as_str()
        ));
    }
    for (k, v) in terminals::nat_pairs(blob) {
        let items = v
            .iter()
            .map(|(a, n)| format!("[{}, {n}]", q(a)))
            .collect::<Vec<_>>()
            .join(", ");
        l.push(format!("nat {k} [{items}]"));
    }
    let (ms, tally, flows) = terminals::icmp_summary(blob);
    for m in &ms {
        l.push(format!(
            "icmp {} {} {} {} {} {}",
            m.offset,
            m.src,
            m.dst,
            m.itype,
            m.icode,
            pyq(&m.quoted)
        ));
    }
    for (k, v) in tally {
        l.push(format!("icmptally {} {v}", q(&k)));
    }
    for f in flows {
        l.push(format!("flow {}", pyq(&Some(f))));
    }
    for r in beams::spot_beam_records(blob) {
        l.push(format!(
            "beamrec {} {} {}",
            r.offset,
            r.beam_id,
            pts(&r.polygon)
        ));
    }
    for (b, p) in beams::spot_beam_map(blob) {
        l.push(format!("beammap {b} {}", pts(&p)));
    }
    l
}

#[test]
#[ignore = "needs DECBGAN_PAYLOAD"]
fn payload_matches_python() {
    let Some(blob) = payload() else { return };
    let t = std::time::Instant::now();
    let mine = dump(&blob);
    println!(
        "{} bytes scanned in {:.2}s, {} lines",
        blob.len(),
        t.elapsed().as_secs_f64(),
        mine.len()
    );
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for x in &mine {
        *counts.entry(x.split(' ').next().unwrap()).or_default() += 1;
    }
    println!("{counts:?}");
    if let Ok(p) = std::env::var("DECBGAN_DUMP") {
        std::fs::write(p, mine.join("\n") + "\n").unwrap();
    }
    if let Ok(p) = std::env::var("DECBGAN_PCAP") {
        // Python's `write_ipv4` output (t0 = 0, dt = 1 ms) must be reproduced byte for byte.
        let mut out = Vec::new();
        pcap::write_ipv4(&mut out, &blob, 0.0, 0.001).unwrap();
        assert!(out == std::fs::read(p).unwrap(), "pcap bytes differ");
        println!("pcap byte-identical ({} bytes)", out.len());
    }
    let Ok(r) = std::env::var("DECBGAN_REF") else {
        return;
    };
    let want: Vec<String> = std::fs::read_to_string(r)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let mut bad = 0;
    for i in 0..mine.len().max(want.len()) {
        let (a, b) = (mine.get(i), want.get(i));
        if a != b {
            bad += 1;
            if bad <= 12 {
                println!("line {i}:\n  rust   {a:?}\n  python {b:?}");
            }
        }
    }
    assert_eq!(bad, 0, "{bad} differing lines");
}

#[test]
#[ignore = "needs DECBGAN_PAYLOAD"]
fn streaming_equals_one_shot() {
    let Some(blob) = payload() else { return };
    // an awkward chunk size so boundaries land everywhere
    let chunk = 49_999;

    let mut s = bctpdu::PduScanner::new();
    let mut pdus = Vec::new();
    for c in blob.chunks(chunk) {
        pdus.extend(s.push(c));
    }
    pdus.extend(s.finish());
    assert_eq!(pdus, bctpdu::find_pdus_default(&blob), "pdus");

    let mut s = pcap::Ipv4Carver::default();
    let mut pk = Vec::new();
    for c in blob.chunks(chunk) {
        pk.extend(s.push(c));
    }
    pk.extend(s.finish());
    let one = pcap::carve_ipv4(&blob);
    assert_eq!(pk.len(), one.len(), "ipv4 count");
    assert!(
        pk.iter()
            .zip(&one)
            .all(|(a, (o, p))| a.offset == *o && a.data == *p),
        "ipv4"
    );

    let mut s = findings::FindingsScanner::new();
    let mut f = Vec::new();
    for c in blob.chunks(chunk) {
        f.extend(s.push(c));
    }
    f.extend(s.finish());
    f.sort_by_key(|x| (x.offset, x.kind));
    assert_eq!(f, findings::scan(&blob), "findings");

    let mut s = files::FileScanner::new();
    let mut d = Vec::new();
    for c in blob.chunks(chunk) {
        d.extend(s.push(c));
    }
    d.extend(s.finish());
    d.sort_by_key(|x| x.offset);
    assert_eq!(d, files::documents(&blob), "documents");

    let mut s = sip::SipScanner::new();
    let mut m = Vec::new();
    for c in blob.chunks(chunk) {
        m.extend(s.push(c));
    }
    m.extend(s.finish());
    m.sort_by_key(|x| x.offset);
    assert_eq!(m, sip::messages(&blob), "sip");

    let mut s = rtp::RtpScanner::new();
    for c in blob.chunks(chunk) {
        s.push(c);
    }
    s.finish();
    assert_eq!(s.streams(), rtp::streams(&blob), "rtp");

    let mut t = terminals::TerminalTracker::new();
    for c in blob.chunks(chunk) {
        t.push(c);
    }
    t.finish();
    assert_eq!(t.terminals(), terminals::terminals(&blob), "terminals");
    assert_eq!(t.nat_pairs(), terminals::nat_pairs(&blob), "nat");

    let mut s = beams::BeamScanner::new();
    let mut r = Vec::new();
    for c in blob.chunks(chunk) {
        r.extend(s.push(c));
    }
    assert_eq!(r, beams::spot_beam_records(&blob), "beam records");
}
