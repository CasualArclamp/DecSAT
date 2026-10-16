// made by CasualArclamp
//! GSE de-encapsulation: BBFRAME data fields in, reassembled PDUs out
//! (ETSI TS 102 606-1; the wire format is summarised in `docs/DESIGN.md`
//! Appendix B).
//!
//! Every GSE packet starts with a 2-byte header — S, E, a 2-bit label type
//! and a 12-bit GSE_LENGTH — then, by S/E:
//!
//! | S E | packet | body                                                        |
//! |-----|--------|-------------------------------------------------------------|
//! | 1 1 | whole  | protocol type, label, PDU                                   |
//! | 1 0 | start  | frag id, total length, protocol type, label, PDU fragment   |
//! | 0 0 | middle | frag id, PDU fragment                                       |
//! | 0 1 | end    | frag id, PDU fragment, CRC-32                               |
//!
//! S = E = 0 with label type 0 and length 0 is padding: the rest of the data
//! field is empty. Fragments of one PDU may span BBFRAMEs, so reassembly
//! state lives across calls.
//!
//! Two deviations seen on real links are handled as variants (after
//! dontlookup's parsers, MIT): GSE_LENGTH counting the 2-byte header too,
//! and a frag-id byte that is a 6-bit id plus a 2-bit counter.
//!
//! A data field the caller has checked by other means (a vendor's CRC over
//! the whole BBFRAME, see `auto::frame_trailer`) is "verified": a PDU
//! reassembled only from verified fields, its split counters running on,
//! is passed on even when its own total length and CRC-32 follow a rule of
//! the vendor's that this decoder does not know.

use std::collections::HashMap;

use decsat_core::crc::crc32_mpeg2;

/// What GSE_LENGTH counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LengthMode {
    /// The bytes after the 2-byte header (the standard).
    Standard,
    /// The 2-byte header as well (dontlookup's "hdrlen").
    HeaderIncluded,
}

/// What the fragment-id byte holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FragMode {
    /// One 8-bit id (the standard).
    Plain,
    /// A 6-bit id and a 2-bit counter (dontlookup's "split").
    Split,
}

/// One way to read GSE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Variant {
    pub length: LengthMode,
    pub frag: FragMode,
}

impl Variant {
    pub const STANDARD: Variant = Variant {
        length: LengthMode::Standard,
        frag: FragMode::Plain,
    };

    /// All four, the standard first.
    pub const ALL: [Variant; 4] = [
        Variant::STANDARD,
        Variant {
            length: LengthMode::Standard,
            frag: FragMode::Split,
        },
        Variant {
            length: LengthMode::HeaderIncluded,
            frag: FragMode::Plain,
        },
        Variant {
            length: LengthMode::HeaderIncluded,
            frag: FragMode::Split,
        },
    ];

    pub fn label(self) -> &'static str {
        match (self.length, self.frag) {
            (LengthMode::Standard, FragMode::Plain) => "standard",
            (LengthMode::Standard, FragMode::Split) => "split frag id",
            (LengthMode::HeaderIncluded, FragMode::Plain) => "length incl. header",
            (LengthMode::HeaderIncluded, FragMode::Split) => "length incl. header, split frag id",
        }
    }

    /// Body length for a GSE_LENGTH value, if it can be one.
    fn body_len(self, gse_length: usize) -> Option<usize> {
        match self.length {
            LengthMode::Standard => Some(gse_length),
            LengthMode::HeaderIncluded => gse_length.checked_sub(2),
        }
    }

    /// The GSE_LENGTH value for a body length.
    pub fn gse_length(self, body_len: usize) -> usize {
        match self.length {
            LengthMode::Standard => body_len,
            LengthMode::HeaderIncluded => body_len + 2,
        }
    }

    /// Reassembly key of a frag-id byte.
    fn frag_key(self, byte: u8) -> u8 {
        match self.frag {
            FragMode::Plain => byte,
            FragMode::Split => byte >> 2,
        }
    }
}

/// A GSE label: the receiver address of a PDU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Label {
    /// Broadcast (label type 2), or none known.
    #[default]
    None,
    Three([u8; 3]),
    Six([u8; 6]),
}

impl Label {
    /// The label type field value for this label.
    pub fn label_type(self) -> u8 {
        match self {
            Label::Six(_) => 0,
            Label::Three(_) => 1,
            Label::None => 2,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        match self {
            Label::None => &[],
            Label::Three(b) => b,
            Label::Six(b) => b,
        }
    }
}

/// One reassembled PDU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pdu {
    /// The protocol type (an EtherType: 0x0800 IPv4, 0x86DD IPv6, …).
    pub protocol: u16,
    pub label: Label,
    pub data: Vec<u8>,
    /// GSE packets it came in (1 = whole).
    pub fragments: u32,
    /// Fragmented PDUs: whether the CRC-32 matched.
    pub crc_ok: Option<bool>,
}

/// Counts of what a decoder has seen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GseStats {
    pub packets: u64,
    pub whole: u64,
    pub start: u64,
    pub middle: u64,
    pub end: u64,
    pub padding: u64,
    /// Packets that do not fit their data field, or are too short to hold
    /// their own fields; parsing of that field stops there.
    pub errors: u64,
    pub reassembled: u64,
    /// Middle/end fragments with no start.
    pub orphans: u64,
    /// A start fragment arriving for an id still being reassembled.
    pub collisions: u64,
    pub crc_ok: u64,
    pub crc_bad: u64,
    /// Reassembled PDUs passed on the strength of their fields' own CRC
    /// (their GSE CRC-32 or total length not the standard's).
    pub vouched: u64,
}

struct Partial {
    total_length: usize,
    protocol: u16,
    label: Label,
    data: Vec<u8>,
    /// Bytes the CRC covers: total length, protocol type, label, PDU.
    crc_input: Vec<u8>,
    fragments: u32,
    /// Every fragment so far came in a verified field, and (split ids) the
    /// counters ran on: none is missing.
    verified: bool,
    /// The last fragment's split counter.
    counter: u8,
}

/// A GSE decoder for one variant.
pub struct GseDecoder {
    pub variant: Variant,
    partial: HashMap<u8, Partial>,
    pub stats: GseStats,
    /// The field being parsed was verified by the caller.
    verified: bool,
}

/// Longest PDU reassembled (GSE's total length is 16 bits).
const MAX_PDU: usize = 65_535;

fn label_len(label_type: u8) -> usize {
    match label_type {
        0 => 6,
        1 => 3,
        _ => 0,
    }
}

impl GseDecoder {
    pub fn new(variant: Variant) -> Self {
        GseDecoder {
            variant,
            partial: HashMap::new(),
            stats: GseStats::default(),
            verified: false,
        }
    }

    /// Parse one BBFRAME data field, appending completed PDUs to `out`.
    pub fn data_field(&mut self, field: &[u8], out: &mut Vec<Pdu>) {
        self.data_field_verified(field, false, out);
    }

    /// [`Self::data_field`] for a field whose integrity the caller has
    /// checked (`verified`) or not.
    pub fn data_field_verified(&mut self, field: &[u8], verified: bool, out: &mut Vec<Pdu>) {
        self.verified = verified;
        let mut i = 0;
        // Label type 3 re-uses the previous packet's label in this field.
        let mut last_label = Label::None;
        while i + 2 <= field.len() {
            let h = u16::from_be_bytes([field[i], field[i + 1]]);
            let (s, e) = (h >> 15 == 1, (h >> 14) & 1 == 1);
            let lt = ((h >> 12) & 3) as u8;
            let gse_length = (h & 0x0FFF) as usize;
            if !s && !e && lt == 0 && gse_length == 0 {
                self.stats.padding += 1;
                break;
            }
            let Some(body_len) = self.variant.body_len(gse_length) else {
                self.stats.errors += 1;
                break;
            };
            let body_start = i + 2;
            if body_start + body_len > field.len() {
                self.stats.errors += 1;
                break;
            }
            let body = &field[body_start..body_start + body_len];
            i = body_start + body_len;
            self.stats.packets += 1;

            let ok = match (s, e) {
                (true, true) => self.whole(body, lt, &mut last_label, out),
                (true, false) => self.start(body, lt, &mut last_label),
                (false, false) => self.middle(body),
                (false, true) => self.end(body, out),
            };
            if !ok {
                self.stats.errors += 1;
                break;
            }
        }
    }

    /// Read a label of type `lt` at the start of `b`: (label, bytes used).
    fn label(b: &[u8], lt: u8, last: &Label) -> Option<(Label, usize)> {
        let n = label_len(lt);
        if b.len() < n {
            return None;
        }
        Some(match lt {
            0 => (Label::Six(b[..6].try_into().unwrap()), 6),
            1 => (Label::Three(b[..3].try_into().unwrap()), 3),
            2 => (Label::None, 0),
            _ => (*last, 0),
        })
    }

    fn whole(&mut self, b: &[u8], lt: u8, last: &mut Label, out: &mut Vec<Pdu>) -> bool {
        if b.len() < 2 {
            return false;
        }
        let protocol = u16::from_be_bytes([b[0], b[1]]);
        let Some((label, n)) = Self::label(&b[2..], lt, last) else {
            return false;
        };
        *last = label;
        self.stats.whole += 1;
        out.push(Pdu {
            protocol,
            label,
            data: b[2 + n..].to_vec(),
            fragments: 1,
            crc_ok: None,
        });
        true
    }

    fn start(&mut self, b: &[u8], lt: u8, last: &mut Label) -> bool {
        if b.len() < 5 {
            return false;
        }
        let key = self.variant.frag_key(b[0]);
        let total_length = u16::from_be_bytes([b[1], b[2]]) as usize;
        let protocol = u16::from_be_bytes([b[3], b[4]]);
        let Some((label, n)) = Self::label(&b[5..], lt, last) else {
            return false;
        };
        *last = label;
        self.stats.start += 1;
        let p = Partial {
            total_length,
            protocol,
            label,
            data: b[5 + n..].to_vec(),
            crc_input: b[1..].to_vec(),
            fragments: 1,
            verified: self.verified,
            counter: b[0] & 3,
        };
        if self.partial.insert(key, p).is_some() {
            self.stats.collisions += 1;
        }
        true
    }

    fn middle(&mut self, b: &[u8]) -> bool {
        if b.is_empty() {
            return false;
        }
        self.stats.middle += 1;
        let key = self.variant.frag_key(b[0]);
        let (verified, split) = (self.verified, self.variant.frag == FragMode::Split);
        match self.partial.get_mut(&key) {
            Some(p) if p.data.len() + b.len() <= MAX_PDU => {
                p.data.extend_from_slice(&b[1..]);
                p.crc_input.extend_from_slice(&b[1..]);
                p.fragments += 1;
                p.follow(b[0], verified, split);
            }
            Some(_) => {
                self.partial.remove(&key);
            }
            None => self.stats.orphans += 1,
        }
        true
    }

    fn end(&mut self, b: &[u8], out: &mut Vec<Pdu>) -> bool {
        if b.len() < 5 {
            return false;
        }
        self.stats.end += 1;
        let key = self.variant.frag_key(b[0]);
        let Some(mut p) = self.partial.remove(&key) else {
            self.stats.orphans += 1;
            return true;
        };
        p.follow(b[0], self.verified, self.variant.frag == FragMode::Split);
        let data = &b[1..b.len() - 4];
        p.data.extend_from_slice(data);
        p.crc_input.extend_from_slice(data);
        let sent = u32::from_be_bytes(b[b.len() - 4..].try_into().unwrap());
        let crc_ok = crc32_mpeg2(&p.crc_input) == sent;
        // The total length covers everything after its own field.
        let length_ok = p.total_length + 2 == p.crc_input.len();
        if crc_ok {
            self.stats.crc_ok += 1;
        } else if p.verified {
            // Seen live: a vendor whose total length reads 9 short and whose
            // CRC-32 is none of the usual, every BBFRAME carrying a CRC of
            // its own (`auto::frame_trailer`) — the PDUs are whole and right.
            self.stats.vouched += 1;
        } else {
            self.stats.crc_bad += 1;
        }
        if !crc_ok && !length_ok && !p.verified {
            return true;
        }
        self.stats.reassembled += 1;
        out.push(Pdu {
            protocol: p.protocol,
            label: p.label,
            data: p.data,
            fragments: p.fragments + 1,
            // Vouched for by the fields, not by its own CRC.
            crc_ok: (crc_ok || !p.verified).then_some(crc_ok),
        });
        true
    }
}

impl Partial {
    /// A further fragment (frag-id byte `id`) arrived, in a `verified`
    /// field or not: the PDU stays verified only if it did, and, with
    /// split ids, its counter is the next.
    fn follow(&mut self, id: u8, verified: bool, split: bool) {
        let counter = id & 3;
        let next = !split || counter == (self.counter + 1) & 3;
        self.verified &= verified && next;
        self.counter = counter;
    }
}
