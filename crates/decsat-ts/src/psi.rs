//! What a transport stream carries — the analyser behind the TS viewer.
//!
//! Per PID: packets, rate, continuity, errors, scrambling, PCRs, and for PIDs
//! no table names, the PES stream id. From the tables (ISO/IEC 13818-1
//! §2.4.4, EN 300 468 §5): PAT, CAT (EMM PIDs), PMT (stream types,
//! languages, CA systems and ECM PIDs), SDT (service names and types), NIT
//! (network name, satellite transponders), EIT present/following (what is on
//! now and next) and TDT/TOT (the stream's UTC clock).
//!
//! Sections are reassembled per PID — several may start in one packet, one
//! may span many — and kept only if their CRC-32 checks.

use std::collections::BTreeMap;

use decsat_core::crc::crc32_mpeg2;

use crate::deframe::TS_LEN;
use crate::section::SectionAssembler;

pub const PID_PAT: u16 = 0x0000;
pub const PID_CAT: u16 = 0x0001;
pub const PID_NIT: u16 = 0x0010;
pub const PID_SDT: u16 = 0x0011;
pub const PID_EIT: u16 = 0x0012;
pub const PID_TDT: u16 = 0x0014;
pub const PID_NULL: u16 = 0x1FFF;

/// A stream type (PMT) as a short name.
pub fn stream_type_name(t: u8) -> &'static str {
    match t {
        0x01 => "MPEG-1 video",
        0x02 => "MPEG-2 video",
        0x03 => "MPEG-1 audio",
        0x04 => "MPEG-2 audio",
        0x05 => "private sections",
        0x06 => "private PES",
        0x0B => "DSM-CC",
        0x0D => "DSM-CC (MPE data)",
        0x0F => "AAC audio",
        0x10 => "MPEG-4 video",
        0x11 => "AAC (LATM) audio",
        0x15 => "metadata",
        0x1B => "H.264 video",
        0x24 => "HEVC video",
        0x33 => "VVC video",
        0x81 => "AC-3 audio",
        0x87 => "E-AC-3 audio",
        _ => "other",
    }
}

/// A CA system id's vendor (ETR 162 allocations).
pub fn ca_system_name(id: u16) -> &'static str {
    match id >> 8 {
        0x01 => "Seca/Mediaguard",
        0x05 => "Viaccess",
        0x06 => "Irdeto",
        0x09 => "NDS Videoguard",
        0x0B => "Conax",
        0x0D => "Cryptoworks",
        0x0E => "PowerVu",
        0x10 => "Tandberg",
        0x17 => "BetaCrypt",
        0x18 => "Nagravision",
        0x26 => "BISS",
        0x4A => "DRE/other",
        0x56 => "Verimatrix",
        _ => "CA",
    }
}

/// An SDT service type as a short name.
pub fn service_type_name(t: u8) -> &'static str {
    match t {
        0x01 | 0x11 | 0x16 | 0x19 | 0x1F | 0x20 => "TV",
        0x02 | 0x07 | 0x0A => "radio",
        0x0C => "data",
        0x03 => "teletext",
        _ => "service",
    }
}

/// One elementary stream of a programme.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EsInfo {
    pub pid: u16,
    pub stream_type: u8,
    /// ISO 639 language, from the language descriptor.
    pub language: Option<String>,
    /// What a descriptor says the stream is, where the stream type does not
    /// (private PES carrying AC-3, subtitles, teletext…).
    pub kind: Option<&'static str>,
}

impl EsInfo {
    pub fn new(pid: u16, stream_type: u8) -> Self {
        EsInfo {
            pid,
            stream_type,
            ..Default::default()
        }
    }

    /// What it is, without the language: "H.264 video", "AC-3 audio"…
    pub fn base(&self) -> &'static str {
        self.kind.unwrap_or(stream_type_name(self.stream_type))
    }

    pub fn is_video(&self) -> bool {
        self.base().ends_with("video")
    }

    pub fn is_audio(&self) -> bool {
        self.base().ends_with("audio")
    }

    /// "H.264 video", "AC-3 audio (eng)", "subtitles (fra)"…
    pub fn describe(&self) -> String {
        let base = self.base();
        match &self.language {
            Some(l) => format!("{base} ({l})"),
            None => base.to_string(),
        }
    }
}

/// A programme event from the EIT.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Event {
    pub name: String,
    pub text: String,
    /// Start, UTC, "YYYY-MM-DD HH:MM".
    pub start: Option<String>,
    pub duration_min: u32,
}

/// A programme / service: what the PAT, its PMT, the SDT and EIT say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Programme {
    pub number: u16,
    pub pmt_pid: u16,
    pub pcr_pid: Option<u16>,
    pub streams: Vec<EsInfo>,
    pub provider: Option<String>,
    pub name: Option<String>,
    pub service_type: Option<u8>,
    /// CA systems named in the PMT (scrambled services).
    pub ca_systems: Vec<u16>,
    /// The SDT's free_CA_mode: the service may be scrambled.
    pub free_ca: bool,
    pub now: Option<Event>,
    pub next: Option<Event>,
}

/// A transport stream the NIT lists, with its satellite delivery
/// parameters when given.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transponder {
    pub ts_id: u16,
    pub onid: u16,
    pub frequency_ghz: Option<f64>,
    /// Orbital position, degrees, east positive.
    pub orbital: Option<f64>,
    pub polarization: Option<char>,
    pub symbol_rate_msps: Option<f64>,
    /// "DVB-S" or "DVB-S2".
    pub system: Option<&'static str>,
}

/// What the NIT says.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NetworkInfo {
    pub network_id: Option<u16>,
    pub name: Option<String>,
    pub transponders: Vec<Transponder>,
}

/// Per-PID counts.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PidStats {
    pub packets: u64,
    /// Continuity counter jumps (packets lost, or a broken stream).
    pub cc_errors: u64,
    /// Packets with the transport error indicator set.
    pub errors: u64,
    /// Packets with scrambling control set: encrypted (conditional access).
    pub scrambled: u64,
    /// Packets that came scrambled and were descrambled (a BISS key)
    /// before the analyser saw them.
    pub descrambled: u64,
    /// The latest packet came scrambled (left so: no key), or came
    /// scrambled and was descrambled. The counts above keep the history (a
    /// PID scrambled until its key was typed); these say what it is now.
    pub scrambled_now: bool,
    pub descrambled_now: bool,
    /// Packets carrying a PCR.
    pub pcr: u64,
    /// The stream id of the PES it carries, if it carries PES.
    pub pes_stream_id: Option<u8>,
    /// Bits per second over the last second or so of signal.
    pub rate_bps: f64,
}

/// One row of the viewer's PID table.
#[derive(Debug, Clone, PartialEq)]
pub struct PidRow {
    pub pid: u16,
    pub stats: PidStats,
    /// What the PID is: "PAT", "H.264 video", "ECM (Conax)", "null"…
    pub kind: String,
    /// The service it belongs to.
    pub service: Option<String>,
}

/// A snapshot of everything the analyser knows, for display.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TsReport {
    pub ts_id: Option<u16>,
    pub packets: u64,
    pub rate_bps: f64,
    pub programmes: Vec<Programme>,
    pub pids: Vec<PidRow>,
    pub network: NetworkInfo,
    /// (PID, table id, sections seen).
    pub tables: Vec<(u16, u8, u64)>,
    /// The stream's clock from the TDT/TOT, UTC.
    pub utc: Option<String>,
    pub bad_sections: u64,
}

/// Watches a transport stream: PID counts, continuity, and the PSI/SI.
#[derive(Default)]
pub struct TsAnalyser {
    pub pids: BTreeMap<u16, PidStats>,
    last_cc: BTreeMap<u16, u8>,
    sections: SectionAssembler,
    done: Vec<Vec<u8>>,
    /// Programmes by number, from the PAT (and the SDT).
    pub programmes: BTreeMap<u16, Programme>,
    /// PIDs the PAT names as PMTs, and their programme.
    pmt_pids: BTreeMap<u16, u16>,
    /// ECM PIDs → (CA system, programme); EMM PIDs → CA system.
    ecm: BTreeMap<u16, (u16, u16)>,
    emm: BTreeMap<u16, u16>,
    pub network: NetworkInfo,
    tables: BTreeMap<(u16, u8), u64>,
    pub utc: Option<String>,
    pub packets: u64,
    /// The transport stream id, from the PAT.
    pub ts_id: Option<u16>,
    pub bad_sections: u64,
    // Rate window: signal seconds, and packet counts at its start.
    win_secs: f64,
    win_start: BTreeMap<u16, u64>,
    win_total: u64,
    rate_bps: f64,
}

impl TsAnalyser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look at one packet.
    pub fn packet(&mut self, p: &[u8; TS_LEN]) {
        self.packets += 1;
        let tei = p[1] & 0x80 != 0;
        let pusi = p[1] & 0x40 != 0;
        let pid = u16::from_be_bytes([p[1] & 0x1F, p[2]]);
        let scrambling = p[3] >> 6;
        let afc = (p[3] >> 4) & 3;
        let cc = p[3] & 0x0F;
        let st = self.pids.entry(pid).or_default();
        st.packets += 1;
        if tei {
            st.errors += 1;
            return; // nothing in it can be trusted
        }
        if scrambling != 0 {
            st.scrambled += 1;
        }
        st.scrambled_now = scrambling != 0;
        st.descrambled_now = false; // `note_descrambled` follows if it was
        let has_af = afc & 2 != 0;
        let af_len = if has_af { p[4] as usize } else { 0 };
        if has_af && af_len > 0 && p[5] & 0x10 != 0 {
            st.pcr += 1;
        }
        let has_payload = afc & 1 == 1;
        // Continuity: payload packets count up mod 16; a repeat is allowed
        // once, the null PID is exempt, and so is a flagged discontinuity.
        let discontinuity = has_af && af_len > 0 && p[5] & 0x80 != 0;
        if pid != PID_NULL && has_payload {
            if let Some(&last) = self.last_cc.get(&pid)
                && !discontinuity
                && cc != (last + 1) & 0x0F
                && cc != last
            {
                st.cc_errors += 1;
            }
            self.last_cc.insert(pid, cc);
        }
        if !has_payload || scrambling != 0 {
            return;
        }
        let at = 4 + if has_af { 1 + af_len } else { 0 };
        if at >= TS_LEN {
            return;
        }
        let payload = &p[at..];
        if pusi && payload.len() >= 4 && payload[..3] == [0, 0, 1] {
            st.pes_stream_id = Some(payload[3]);
        }
        let is_si = matches!(
            pid,
            PID_PAT | PID_CAT | PID_NIT | PID_SDT | PID_EIT | PID_TDT
        ) || self.pmt_pids.contains_key(&pid);
        if is_si {
            self.si_payload(pid, pusi, payload);
        }
    }

    /// Packet `pid` just looked at had been descrambled.
    pub fn note_descrambled(&mut self, pid: u16) {
        if let Some(st) = self.pids.get_mut(&pid) {
            st.descrambled += 1;
            st.descrambled_now = true;
        }
    }

    /// Signal time passed: update the rates once a second's worth is in.
    pub fn tick(&mut self, secs: f64) {
        self.win_secs += secs;
        if self.win_secs < 1.0 {
            return;
        }
        let bits = (TS_LEN * 8) as f64;
        for (pid, st) in self.pids.iter_mut() {
            let start = self.win_start.get(pid).copied().unwrap_or(0);
            st.rate_bps = (st.packets - start) as f64 * bits / self.win_secs;
            self.win_start.insert(*pid, st.packets);
        }
        self.rate_bps = (self.packets - self.win_total) as f64 * bits / self.win_secs;
        self.win_total = self.packets;
        self.win_secs = 0.0;
    }

    /// Reassemble the SI sections on `pid` and read each one that completes.
    fn si_payload(&mut self, pid: u16, pusi: bool, payload: &[u8]) {
        let mut done = std::mem::take(&mut self.done);
        done.clear();
        self.sections.feed(pid, pusi, payload, &mut done);
        for s in &done {
            self.section(pid, s);
        }
        self.done = done;
    }

    /// One complete section.
    fn section(&mut self, pid: u16, s: &[u8]) {
        let tid = s[0];
        *self.tables.entry((pid, tid)).or_default() += 1;
        if tid == 0x70 {
            // TDT: just the UTC time, no CRC.
            if s.len() >= 8 {
                self.utc = Some(mjd_utc(&s[3..8], true));
            }
            return;
        }
        let len = s.len();
        if len < 12 || crc32_mpeg2(s) != 0 {
            self.bad_sections += 1;
            return;
        }
        let ext = u16::from_be_bytes([s[3], s[4]]);
        let section_number = s[6];
        let body = &s[8..len - 4];
        match (pid, tid) {
            (PID_PAT, 0x00) => self.pat(ext, body),
            (PID_CAT, 0x01) => {
                for (tag, d) in descriptors(body) {
                    if tag == 0x09 && d.len() >= 4 {
                        let sys = u16::from_be_bytes([d[0], d[1]]);
                        self.emm
                            .insert(u16::from_be_bytes([d[2] & 0x1F, d[3]]), sys);
                    }
                }
            }
            (p, 0x02) if self.pmt_pids.contains_key(&p) => self.pmt(p, ext, body),
            (PID_SDT, 0x42) => self.sdt(body),
            (PID_NIT, 0x40) => self.nit(ext, body),
            (PID_EIT, 0x4E) => self.eit(ext, section_number, body),
            // TOT: the time, then descriptors and a CRC.
            (PID_TDT, 0x73) => self.utc = Some(mjd_utc(&s[3..8], true)),
            _ => {}
        }
    }

    fn pat(&mut self, ts_id: u16, body: &[u8]) {
        self.ts_id = Some(ts_id);
        for e in body.chunks(4).filter(|e| e.len() == 4) {
            let number = u16::from_be_bytes([e[0], e[1]]);
            let pmt = u16::from_be_bytes([e[2] & 0x1F, e[3]]);
            if number == 0 {
                continue; // the network PID
            }
            self.pmt_pids.insert(pmt, number);
            let p = self.programmes.entry(number).or_default();
            p.number = number;
            p.pmt_pid = pmt;
        }
    }

    fn pmt(&mut self, pid: u16, number: u16, body: &[u8]) {
        if body.len() < 4 {
            return;
        }
        let pcr = u16::from_be_bytes([body[0] & 0x1F, body[1]]);
        let info_len = u16::from_be_bytes([body[2] & 0x0F, body[3]]) as usize;
        let mut cas = Vec::new();
        let prog_desc = &body[4..(4 + info_len).min(body.len())];
        for (tag, d) in descriptors(prog_desc) {
            if tag == 0x09 && d.len() >= 4 {
                let sys = u16::from_be_bytes([d[0], d[1]]);
                cas.push(sys);
                self.ecm
                    .insert(u16::from_be_bytes([d[2] & 0x1F, d[3]]), (sys, number));
            }
        }
        let mut at = 4 + info_len;
        let mut streams = Vec::new();
        while at + 5 <= body.len() {
            let mut es = EsInfo::new(
                u16::from_be_bytes([body[at + 1] & 0x1F, body[at + 2]]),
                body[at],
            );
            let es_len = u16::from_be_bytes([body[at + 3] & 0x0F, body[at + 4]]) as usize;
            let desc = &body[(at + 5).min(body.len())..(at + 5 + es_len).min(body.len())];
            for (tag, d) in descriptors(desc) {
                match tag {
                    0x0A if d.len() >= 3 => es.language = Some(dvb_text(&d[..3])),
                    0x59 if d.len() >= 3 => {
                        es.kind = Some("subtitles");
                        es.language = Some(dvb_text(&d[..3]));
                    }
                    0x56 if d.len() >= 3 => {
                        es.kind = Some("teletext");
                        es.language = Some(dvb_text(&d[..3]));
                    }
                    0x6A => es.kind = Some("AC-3 audio"),
                    0x7A => es.kind = Some("E-AC-3 audio"),
                    0x7C => es.kind = Some("AAC audio"),
                    0x09 if d.len() >= 4 => {
                        let sys = u16::from_be_bytes([d[0], d[1]]);
                        cas.push(sys);
                        self.ecm
                            .insert(u16::from_be_bytes([d[2] & 0x1F, d[3]]), (sys, number));
                    }
                    _ => {}
                }
            }
            streams.push(es);
            at += 5 + es_len;
        }
        cas.sort_unstable();
        cas.dedup();
        let p = self.programmes.entry(number).or_default();
        p.number = number;
        p.pmt_pid = pid;
        p.pcr_pid = Some(pcr);
        p.streams = streams;
        p.ca_systems = cas;
    }

    fn sdt(&mut self, body: &[u8]) {
        // original_network_id (2), reserved (1), then services.
        let mut at = 3;
        while at + 5 <= body.len() {
            let sid = u16::from_be_bytes([body[at], body[at + 1]]);
            let free_ca = body[at + 3] & 0x10 != 0;
            let loop_len = u16::from_be_bytes([body[at + 3] & 0x0F, body[at + 4]]) as usize;
            let desc = &body[(at + 5).min(body.len())..(at + 5 + loop_len).min(body.len())];
            let p = self.programmes.entry(sid).or_default();
            p.number = sid;
            p.free_ca = free_ca;
            for (tag, v) in descriptors(desc) {
                if tag == 0x48 && v.len() >= 2 {
                    // service_descriptor: type, provider name, service name.
                    p.service_type = Some(v[0]);
                    let pl = v[1] as usize;
                    if 2 + pl < v.len() {
                        p.provider = Some(dvb_text(&v[2..2 + pl]));
                        let nl = v[2 + pl] as usize;
                        let name = &v[(3 + pl).min(v.len())..(3 + pl + nl).min(v.len())];
                        p.name = Some(dvb_text(name));
                    }
                }
            }
            at += 5 + loop_len;
        }
    }

    fn nit(&mut self, network_id: u16, body: &[u8]) {
        self.network.network_id = Some(network_id);
        if body.len() < 2 {
            return;
        }
        let nd_len = u16::from_be_bytes([body[0] & 0x0F, body[1]]) as usize;
        let nd = &body[2..(2 + nd_len).min(body.len())];
        for (tag, d) in descriptors(nd) {
            if tag == 0x40 {
                self.network.name = Some(dvb_text(d));
            }
        }
        let mut at = 2 + nd_len + 2; // past transport_stream_loop_length
        while at + 6 <= body.len() {
            let mut t = Transponder {
                ts_id: u16::from_be_bytes([body[at], body[at + 1]]),
                onid: u16::from_be_bytes([body[at + 2], body[at + 3]]),
                ..Default::default()
            };
            let dl = u16::from_be_bytes([body[at + 4] & 0x0F, body[at + 5]]) as usize;
            let desc = &body[(at + 6).min(body.len())..(at + 6 + dl).min(body.len())];
            for (tag, d) in descriptors(desc) {
                if tag == 0x43 && d.len() >= 11 {
                    // satellite_delivery_system_descriptor (EN 300 468 §6.2.13.2).
                    t.frequency_ghz = Some(bcd(&d[0..4]) as f64 / 1e5);
                    let orb = bcd(&d[4..6]) as f64 / 10.0;
                    t.orbital = Some(if d[6] & 0x80 != 0 { orb } else { -orb });
                    t.polarization = Some(['H', 'V', 'L', 'R'][((d[6] >> 5) & 3) as usize]);
                    t.system = Some(if d[6] & 0x04 != 0 { "DVB-S2" } else { "DVB-S" });
                    // 28 bits of BCD (xxx.xxxx Msym/s), then the FEC nibble.
                    t.symbol_rate_msps = Some((bcd(&d[7..11]) / 10) as f64 / 1e4);
                }
            }
            if !self
                .network
                .transponders
                .iter()
                .any(|x| x.ts_id == t.ts_id && x.onid == t.onid)
            {
                self.network.transponders.push(t);
            }
            at += 6 + dl;
        }
    }

    fn eit(&mut self, sid: u16, section_number: u8, body: &[u8]) {
        // ts id (2), onid (2), segment last section (1), last table id (1).
        let mut at = 6;
        while at + 12 <= body.len() {
            let start = &body[at + 2..at + 7];
            let dur = &body[at + 7..at + 10];
            let dl = u16::from_be_bytes([body[at + 10] & 0x0F, body[at + 11]]) as usize;
            let desc = &body[(at + 12).min(body.len())..(at + 12 + dl).min(body.len())];
            let mut ev = Event {
                start: (start != [0xFF; 5]).then(|| mjd_utc(start, false)),
                duration_min: (bcd(&dur[0..1]) * 60 + bcd(&dur[1..2])) as u32,
                ..Default::default()
            };
            for (tag, d) in descriptors(desc) {
                if tag == 0x4D && d.len() >= 4 {
                    // short_event_descriptor: language, name, text.
                    let nl = d[3] as usize;
                    ev.name = dvb_text(&d[4..(4 + nl).min(d.len())]);
                    if 4 + nl < d.len() {
                        let tl = d[4 + nl] as usize;
                        ev.text = dvb_text(&d[(5 + nl).min(d.len())..(5 + nl + tl).min(d.len())]);
                    }
                }
            }
            let p = self.programmes.entry(sid).or_default();
            p.number = sid;
            match section_number {
                0 => p.now = Some(ev),
                1 => p.next = Some(ev),
                _ => {}
            }
            at += 12 + dl;
        }
    }

    /// What a PID is, and the service it belongs to.
    fn classify(&self, pid: u16, st: &PidStats) -> (String, Option<String>) {
        let named = |n: u16| {
            self.programmes
                .get(&n)
                .map(|p| p.name.clone().unwrap_or_else(|| format!("programme {n}")))
        };
        match pid {
            PID_PAT => return ("PAT".into(), None),
            PID_CAT => return ("CAT".into(), None),
            0x0002 => return ("TSDT".into(), None),
            PID_NIT => return ("NIT".into(), None),
            PID_SDT => return ("SDT / BAT".into(), None),
            PID_EIT => return ("EIT".into(), None),
            0x0013 => return ("RST".into(), None),
            PID_TDT => return ("TDT / TOT".into(), None),
            PID_NULL => return ("null (stuffing)".into(), None),
            _ => {}
        }
        if let Some(&n) = self.pmt_pids.get(&pid) {
            return ("PMT".into(), named(n));
        }
        if let Some(&(sys, n)) = self.ecm.get(&pid) {
            return (
                format!("ECM ({}, {sys:#06x})", ca_system_name(sys)),
                named(n),
            );
        }
        if let Some(&sys) = self.emm.get(&pid) {
            return (format!("EMM ({}, {sys:#06x})", ca_system_name(sys)), None);
        }
        for p in self.programmes.values() {
            if let Some(es) = p.streams.iter().find(|e| e.pid == pid) {
                return (es.describe(), named(p.number));
            }
        }
        for p in self.programmes.values() {
            if p.pcr_pid == Some(pid) {
                return ("PCR".into(), named(p.number));
            }
        }
        let kind = match st.pes_stream_id {
            Some(0xE0..=0xEF) => "PES video (unlisted)",
            Some(0xC0..=0xDF) => "PES audio (unlisted)",
            Some(0xBD) => "PES private (unlisted)",
            Some(_) => "PES (unlisted)",
            None if st.scrambled > 0 => "scrambled (unlisted)",
            None => "unlisted",
        };
        (kind.into(), None)
    }

    /// Everything, for the viewer.
    pub fn report(&self) -> TsReport {
        let pids = self
            .pids
            .iter()
            .map(|(&pid, &stats)| {
                let (kind, service) = self.classify(pid, &stats);
                PidRow {
                    pid,
                    stats,
                    kind,
                    service,
                }
            })
            .collect();
        TsReport {
            ts_id: self.ts_id,
            packets: self.packets,
            rate_bps: self.rate_bps,
            programmes: self.programmes.values().cloned().collect(),
            pids,
            network: self.network.clone(),
            tables: self.tables.iter().map(|(&(p, t), &n)| (p, t, n)).collect(),
            utc: self.utc.clone(),
            bad_sections: self.bad_sections,
        }
    }
}

/// Iterate (tag, contents) over a descriptor loop, stopping at a truncation.
fn descriptors(mut d: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    std::iter::from_fn(move || {
        if d.len() < 2 || d.len() < 2 + d[1] as usize {
            return None;
        }
        let (tag, len) = (d[0], d[1] as usize);
        let v = &d[2..2 + len];
        d = &d[2 + len..];
        Some((tag, v))
    })
}

/// Packed BCD digits as a number.
fn bcd(b: &[u8]) -> u64 {
    b.iter().fold(0u64, |n, &x| {
        n * 100 + ((x >> 4) as u64) * 10 + (x & 0x0F) as u64
    })
}

/// 16-bit MJD + 24-bit BCD UTC (EN 300 468 Annex C) as "YYYY-MM-DD HH:MM[:SS]".
fn mjd_utc(b: &[u8], seconds: bool) -> String {
    let mjd = u16::from_be_bytes([b[0], b[1]]) as f64;
    let y1 = ((mjd - 15078.2) / 365.25).floor();
    let m1 = ((mjd - 14956.1 - (y1 * 365.25).floor()) / 30.6001).floor();
    let day = mjd - 14956.0 - (y1 * 365.25).floor() - (m1 * 30.6001).floor();
    let k = if m1 == 14.0 || m1 == 15.0 { 1.0 } else { 0.0 };
    let year = 1900.0 + y1 + k;
    let month = m1 - 1.0 - k * 12.0;
    let (h, m, s) = (bcd(&b[2..3]), bcd(&b[3..4]), bcd(&b[4..5]));
    if seconds {
        format!("{year:04}-{month:02}-{day:02} {h:02}:{m:02}:{s:02}")
    } else {
        format!("{year:04}-{month:02}-{day:02} {h:02}:{m:02}")
    }
}

/// DVB text (EN 300 468 Annex A) as a Rust string: a leading byte below 0x20
/// selects a character table — read UTF-8 (0x15) as such, anything else as
/// Latin-1 — and control codes are dropped.
pub fn dvb_text(b: &[u8]) -> String {
    let (utf8, body) = match b.first() {
        Some(0x15) => (true, &b[1..]),
        Some(0x10) if b.len() >= 3 => (false, &b[3..]),
        Some(&c) if c < 0x20 => (false, &b[1..]),
        _ => (false, b),
    };
    let s: String = if utf8 {
        String::from_utf8_lossy(body).into_owned()
    } else {
        body.iter().map(|&c| c as char).collect()
    };
    s.chars()
        .filter(|c| !c.is_control() && !('\u{80}'..='\u{9F}').contains(c))
        .collect::<String>()
        .trim()
        .to_string()
}

/// Build a PSI section with its CRC (for tests and test signals).
pub fn section(table_id: u8, ext: u16, section_number: u8, body: &[u8]) -> Vec<u8> {
    let len = 5 + body.len() + 4;
    let mut s = vec![
        table_id,
        0xB0 | ((len >> 8) as u8 & 0x0F),
        len as u8,
        (ext >> 8) as u8,
        ext as u8,
        0xC1, // version 0, current
        section_number,
        section_number.max(1),
    ];
    s.extend_from_slice(body);
    let crc = crc32_mpeg2(&s);
    s.extend_from_slice(&crc.to_be_bytes());
    s
}

/// Packetize one section on `pid` (it must fit one packet here).
pub fn section_packet(pid: u16, cc: u8, sec: &[u8]) -> [u8; TS_LEN] {
    let mut p = [0xFFu8; TS_LEN];
    p[0] = 0x47;
    p[1] = 0x40 | (pid >> 8) as u8 & 0x1F;
    p[2] = pid as u8;
    p[3] = 0x10 | (cc & 0x0F);
    p[4] = 0; // pointer field
    p[5..5 + sec.len()].copy_from_slice(sec);
    p
}

/// A descriptor: tag, length, contents.
fn desc(tag: u8, v: &[u8]) -> Vec<u8> {
    let mut d = vec![tag, v.len() as u8];
    d.extend_from_slice(v);
    d
}

/// The tables of a test stream (for test signals): one programme `number`
/// on PMT PID `pmt_pid` with one elementary stream, named in the SDT, an EIT
/// present/following, a NIT naming the network and one DVB-S2 transponder,
/// and a TDT. Returns (PID, section) pairs.
pub fn test_tables(
    number: u16,
    pmt_pid: u16,
    es: EsInfo,
    provider: &str,
    name: &str,
) -> Vec<(u16, Vec<u8>)> {
    let mut pat = Vec::new();
    pat.extend_from_slice(&number.to_be_bytes());
    pat.extend_from_slice(&(0xE000 | pmt_pid).to_be_bytes());

    let mut pmt = Vec::new();
    pmt.extend_from_slice(&(0xE000 | es.pid).to_be_bytes()); // PCR PID
    pmt.extend_from_slice(&0xF000u16.to_be_bytes()); // no programme info
    pmt.push(es.stream_type);
    pmt.extend_from_slice(&(0xE000 | es.pid).to_be_bytes());
    let lang = desc(0x0A, b"eng\x00");
    pmt.extend_from_slice(&((0xF000 | lang.len()) as u16).to_be_bytes());
    pmt.extend_from_slice(&lang);

    let mut sd = vec![0x01, provider.len() as u8];
    sd.extend_from_slice(provider.as_bytes());
    sd.push(name.len() as u8);
    sd.extend_from_slice(name.as_bytes());
    let sd = desc(0x48, &sd);
    let mut sdt = vec![0, 1, 0xFF]; // original network id, reserved
    sdt.extend_from_slice(&number.to_be_bytes());
    sdt.push(0xFC);
    sdt.extend_from_slice(&((0x8000 | sd.len()) as u16).to_be_bytes()); // running
    sdt.extend_from_slice(&sd);

    // EIT p/f: events from MJD 60000 (2023-02-25) 12:00, 1h30 each.
    let eit = |now: bool| {
        let mut ev_d = b"eng".to_vec();
        let ev_name: &[u8] = if now {
            b"Test pattern"
        } else {
            b"More test pattern"
        };
        ev_d.push(ev_name.len() as u8);
        ev_d.extend_from_slice(ev_name);
        ev_d.push(0);
        let ev_d = desc(0x4D, &ev_d);
        let mut b = vec![0, 1, 0, 1, 1, 0x4E]; // ts id, onid, last section, last table
        b.extend_from_slice(&[0, if now { 1 } else { 2 }]); // event id
        b.extend_from_slice(&60000u16.to_be_bytes());
        b.extend_from_slice(if now {
            &[0x12, 0x00, 0x00]
        } else {
            &[0x13, 0x30, 0x00]
        });
        b.extend_from_slice(&[0x01, 0x30, 0x00]); // duration 1:30
        b.extend_from_slice(&((0x8000 | ev_d.len()) as u16).to_be_bytes());
        b.extend_from_slice(&ev_d);
        section(0x4E, number, if now { 0 } else { 1 }, &b)
    };

    // NIT: network name, one DVB-S2 transponder at 11.7 GHz, 28.2°E, V,
    // 27.5 Msym/s.
    let nn = desc(0x40, b"DecSAT net");
    let sat = desc(
        0x43,
        &[
            0x01,
            0x17,
            0x00,
            0x00,
            0x02,
            0x82,
            0x80 | 0x20 | 0x04 | 0x01,
            0x02,
            0x75,
            0x00,
            0x03,
        ],
    );
    let mut nit = Vec::new();
    nit.extend_from_slice(&((0xF000 | nn.len()) as u16).to_be_bytes());
    nit.extend_from_slice(&nn);
    let ts_loop_len = 6 + sat.len();
    nit.extend_from_slice(&((0xF000 | ts_loop_len) as u16).to_be_bytes());
    nit.extend_from_slice(&[0, 1, 0, 1]);
    nit.extend_from_slice(&((0xF000 | sat.len()) as u16).to_be_bytes());
    nit.extend_from_slice(&sat);

    // TDT: no CRC, 5 bytes of time.
    let tdt = vec![0x70, 0x70, 0x05, 0xEA, 0x60, 0x12, 0x34, 0x56];

    vec![
        (PID_PAT, section(0x00, 1, 0, &pat)),
        (pmt_pid, section(0x02, number, 0, &pmt)),
        (PID_SDT, section(0x42, 1, 0, &sdt)),
        (PID_EIT, eit(true)),
        (PID_EIT, eit(false)),
        (PID_NIT, section(0x40, 0x0042, 0, &nit)),
        (PID_TDT, tdt),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables() -> (TsAnalyser, Vec<(u16, Vec<u8>)>) {
        let t = test_tables(7, 0x1000, EsInfo::new(0x100, 0x1B), "DecSAT", "Test card");
        let mut a = TsAnalyser::new();
        for (k, (pid, s)) in t.iter().enumerate() {
            a.packet(&section_packet(*pid, k as u8, s));
        }
        (a, t)
    }

    #[test]
    fn reads_the_programme_tables() {
        let (a, _) = tables();
        let p = &a.programmes[&7];
        assert_eq!(p.pmt_pid, 0x1000);
        assert_eq!(p.streams[0].pid, 0x100);
        assert_eq!(p.streams[0].describe(), "H.264 video (eng)");
        assert_eq!(p.name.as_deref(), Some("Test card"));
        assert_eq!(p.provider.as_deref(), Some("DecSAT"));
        assert_eq!(p.service_type.map(service_type_name), Some("TV"));
        assert_eq!(a.bad_sections, 0);
    }

    #[test]
    fn reads_eit_nit_and_tdt() {
        let (a, _) = tables();
        let p = &a.programmes[&7];
        let now = p.now.as_ref().unwrap();
        assert_eq!(now.name, "Test pattern");
        assert_eq!(now.start.as_deref(), Some("2023-02-25 12:00"));
        assert_eq!(now.duration_min, 90);
        assert_eq!(p.next.as_ref().unwrap().name, "More test pattern");
        assert_eq!(a.network.name.as_deref(), Some("DecSAT net"));
        let t = &a.network.transponders[0];
        assert_eq!(t.frequency_ghz, Some(11.7));
        assert_eq!(t.orbital, Some(28.2));
        assert_eq!(t.polarization, Some('V'));
        assert_eq!(t.system, Some("DVB-S2"));
        assert_eq!(t.symbol_rate_msps, Some(27.5));
        // MJD 0xEA60 = 60000: 2023-02-25.
        assert_eq!(a.utc.as_deref(), Some("2023-02-25 12:34:56"));
    }

    #[test]
    fn several_sections_in_one_packet_and_one_across_packets() {
        let t = test_tables(1, 0x1000, EsInfo::new(0x100, 0x1B), "P", "S");
        let (eit_now, eit_next) = (&t[3].1, &t[4].1);
        // Both EIT sections back to back in one packet.
        let mut p = [0xFFu8; TS_LEN];
        p[..5].copy_from_slice(&[0x47, 0x40, 0x12, 0x10, 0]);
        p[5..5 + eit_now.len()].copy_from_slice(eit_now);
        p[5 + eit_now.len()..5 + eit_now.len() + eit_next.len()].copy_from_slice(eit_next);
        let mut a = TsAnalyser::new();
        a.packet(&p);
        assert!(a.programmes[&1].now.is_some() && a.programmes[&1].next.is_some());

        // An SDT split over two packets: the first ends with an adaptation
        // field so the section really continues in the next.
        let sdt = &t[2].1;
        let split = 10;
        let mut q1 = [0xFFu8; TS_LEN];
        let af = TS_LEN - 4 - (1 + split); // adaptation field incl. its length byte
        q1[..4].copy_from_slice(&[0x47, 0x40, 0x11, 0x30]);
        q1[4] = (af - 1) as u8;
        q1[5] = 0;
        q1[4 + af] = 0; // pointer
        q1[5 + af..].copy_from_slice(&sdt[..split]);
        let mut q2 = [0xFFu8; TS_LEN];
        q2[..4].copy_from_slice(&[0x47, 0x00, 0x11, 0x11]);
        q2[4..4 + sdt.len() - split].copy_from_slice(&sdt[split..]);
        let mut b = TsAnalyser::new();
        b.packet(&q1);
        b.packet(&q2);
        assert_eq!(b.programmes[&1].name.as_deref(), Some("S"));
        assert_eq!(b.bad_sections, 0);
    }

    #[test]
    fn counts_continuity_errors_and_classifies_pids() {
        let (mut a, _) = tables();
        let mut p = [0u8; TS_LEN];
        p[0] = 0x47;
        p[1] = 0x01;
        for cc in [0u8, 1, 2, 2, 3, 5, 6] {
            p[3] = 0x10 | cc;
            a.packet(&p);
        }
        // 2 → 2 is a permitted repeat; 3 → 5 skips one.
        assert_eq!(a.pids[&0x100].cc_errors, 1);
        a.tick(1.0);
        let r = a.report();
        let row = |pid| r.pids.iter().find(|x| x.pid == pid).unwrap();
        assert_eq!(row(0x100).kind, "H.264 video (eng)");
        assert_eq!(row(0x100).service.as_deref(), Some("Test card"));
        assert_eq!(row(0x1000).kind, "PMT");
        assert_eq!(row(0x0000).kind, "PAT");
        assert!(row(0x100).stats.rate_bps > 0.0);
        assert!(
            r.tables
                .iter()
                .any(|&(pid, tid, _)| pid == 0x12 && tid == 0x4E)
        );
    }

    #[test]
    fn dvb_text_reads_utf8_and_latin1() {
        assert_eq!(dvb_text(b"\x15Caf\xc3\xa9"), "Café");
        assert_eq!(dvb_text(b"\x05Caf\xe9"), "Café");
        assert_eq!(dvb_text(b"Plain \x86name\x87"), "Plain name");
    }
}
