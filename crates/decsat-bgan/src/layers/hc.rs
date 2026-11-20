// made by CasualArclamp
//! IP header compression on the forward link: the octet that opens an SDU.
//!
//! Some connections start each SDU with one octet before the packet. Its
//! values match the packet identifiers (PID) of 3GPP PDCP (TS 25.323) with
//! RFC 2507 header compression, which BGAN's UMTS-derived core would use:
//!
//! ```text
//! 0  no compression: the IPv4 packet as sent
//! 1  FULL_HEADER: the packet, with the IPv4 Total Length field carrying the
//!    compression context id (CID) and generation instead of the length
//!    (RFC 2507 clause 5.3); the header checksum is the sender's, over the
//!    real length
//! 2  COMPRESSED_TCP                (seen on real recordings, not decoded yet)
//! 3  COMPRESSED_TCP_NODELTA        (not seen)
//! 4  COMPRESSED_NON_TCP            (seen, not decoded yet)
//! 5  CONTEXT_STATE                 (not seen)
//! ```
//!
//! The mapping was inferred from real payloads, not read from the BGAN spec.
//! What is checked: on every full-header packet found in three real
//! recordings, restoring Total Length from the SDU length makes the header
//! checksum hold, and the transport checksum then holds too.

use super::pcap::ipv4_checksum_ok;

/// PDCP packet identifier: no compression.
pub const PID_NONE: u8 = 0;
/// PDCP packet identifier: RFC 2507 full header.
pub const PID_FULL_HEADER: u8 = 1;

/// A full-header packet with its Total Length restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullHeader {
    /// What the Total Length field carried: the context id and generation.
    /// Compressed packets refer to the context by it.
    pub cid_field: u16,
    /// The packet as it was sent.
    pub packet: Vec<u8>,
}

/// Restore an RFC 2507 full-header IPv4 packet: `ip` is the whole packet as
/// carried (its length is the SDU's, less the PID octet). `None` unless the
/// header checksum holds once Total Length is put back, which is what tells a
/// full header from a stray byte pattern.
pub fn restore_full_header(ip: &[u8]) -> Option<FullHeader> {
    if ip.len() < 20 || ip[0] >> 4 != 4 {
        return None;
    }
    let ihl = (ip[0] & 0x0F) as usize * 4;
    if ihl < 20 || ihl > ip.len() || ip.len() > u16::MAX as usize {
        return None;
    }
    let cid_field = u16::from_be_bytes([ip[2], ip[3]]);
    let mut packet = ip.to_vec();
    packet[2..4].copy_from_slice(&(ip.len() as u16).to_be_bytes());
    ipv4_checksum_ok(&packet[..ihl]).then_some(FullHeader { cid_field, packet })
}

/// The IPv4 packet an SDU holds, from its first octet: the SDU as is (no PID
/// octet, as on some connections), after a [`PID_NONE`] octet, or restored
/// from a [`PID_FULL_HEADER`] one. `None` for compressed or unknown SDUs.
pub fn sdu_packet(sdu: &[u8]) -> Option<Vec<u8>> {
    match *sdu.first()? {
        b if b >> 4 == 4 => Some(sdu.to_vec()),
        PID_NONE => Some(sdu[1..].to_vec()),
        PID_FULL_HEADER => restore_full_header(&sdu[1..]).map(|f| f.packet),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::pcap::transport_checksum_ok;
    use crate::layers::segments::tests::ipv4;

    /// The SIP 200 OK from a real recording (channel #0): Total Length read
    /// `3a 01`, and 957 (UDP length + 20) makes the header checksum hold.
    #[test]
    fn real_full_header() {
        let hdr = [
            0x45, 0x60, 0x3a, 0x01, 0x78, 0x15, 0x40, 0x00, 0x3d, 0x11, 0x94, 0x9d, 0xa1, 0x1e,
            0x73, 0x0a, 0x0a, 0x42, 0x0e, 0xb3,
        ];
        let mut ip = hdr.to_vec();
        ip.resize(957, 0);
        let f = restore_full_header(&ip).expect("restored");
        assert_eq!(f.cid_field, 0x3a01);
        assert_eq!(&f.packet[2..4], &957u16.to_be_bytes());
        // one octet more or less and the checksum no longer holds
        assert!(restore_full_header(&ip[..956]).is_none());
    }

    #[test]
    fn sdu_kinds() {
        let p = ipv4(17, &[7; 40]);
        let mut full = p.clone();
        full[2..4].copy_from_slice(&[0x00, 0x05]); // a CID where the length was
        assert_eq!(sdu_packet(&p).as_deref(), Some(p.as_slice()));
        assert_eq!(
            sdu_packet(&[[0].as_slice(), &p].concat()).as_deref(),
            Some(p.as_slice())
        );
        let got = sdu_packet(&[[1].as_slice(), &full].concat()).unwrap();
        assert_eq!(got, p);
        assert!(transport_checksum_ok(&got));
        assert!(sdu_packet(&[[2].as_slice(), &full].concat()).is_none());
        // PID 1 in front of a packet whose length was not replaced still works
        assert_eq!(
            sdu_packet(&[[1].as_slice(), &p].concat()).as_deref(),
            Some(p.as_slice())
        );
        assert!(sdu_packet(&[]).is_none());
    }
}
