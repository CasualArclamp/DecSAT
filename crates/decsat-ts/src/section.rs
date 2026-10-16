// made by CasualArclamp
//! Section reassembly from TS packets (ISO/IEC 13818-1 §2.4.4): a packet
//! starting a section (PUSI) carries a pointer to where it starts, the bytes
//! before it finishing the previous one; several sections may follow back to
//! back in one packet, and one may span many.

use std::collections::BTreeMap;

/// Sections are capped at this (private sections may reach 4096).
const MAX_SECTION: usize = 4096 + 3;

/// The 12-bit section length.
pub fn section_length(s: &[u8]) -> usize {
    u16::from_be_bytes([s[1] & 0x0F, s[2]]) as usize
}

/// Per-PID partial sections.
#[derive(Default)]
pub struct SectionAssembler {
    partial: BTreeMap<u16, Vec<u8>>,
}

impl SectionAssembler {
    /// Feed one packet's payload (after any adaptation field); complete
    /// sections are appended to `out`.
    pub fn feed(&mut self, pid: u16, pusi: bool, payload: &[u8], out: &mut Vec<Vec<u8>>) {
        if pusi {
            let Some(&ptr) = payload.first() else { return };
            let ptr = ptr as usize;
            if 1 + ptr > payload.len() {
                self.partial.remove(&pid);
                return;
            }
            if let Some(mut s) = self.partial.remove(&pid) {
                s.extend_from_slice(&payload[1..1 + ptr]);
                if let Some(done) = Self::whole(&s) {
                    out.push(done.to_vec());
                }
            }
            let mut rest = &payload[1 + ptr..];
            while !rest.is_empty() && rest[0] != 0xFF {
                if rest.len() < 3 {
                    self.partial.insert(pid, rest.to_vec());
                    return;
                }
                let len = 3 + section_length(rest);
                if rest.len() >= len {
                    out.push(rest[..len].to_vec());
                    rest = &rest[len..];
                } else {
                    self.partial.insert(pid, rest.to_vec());
                    return;
                }
            }
        } else if let Some(mut s) = self.partial.remove(&pid) {
            s.extend_from_slice(payload);
            match Self::whole(&s) {
                Some(done) => out.push(done.to_vec()),
                None if s.len() < MAX_SECTION => {
                    self.partial.insert(pid, s);
                }
                None => {}
            }
        }
    }

    /// The section in `s`, if `s` holds all of it.
    fn whole(s: &[u8]) -> Option<&[u8]> {
        if s.len() < 3 {
            return None;
        }
        let len = 3 + section_length(s);
        (len <= MAX_SECTION && s.len() >= len).then(|| &s[..len])
    }
}
