// made by CasualArclamp
//! A small manufacturer lookup for MAC addresses (IEEE OUIs, the first
//! three octets), for naming the maker of a modulator from the MAC its
//! DVB-CID identifier encapsulates.
//!
//! Hand-curated, not the IEEE registry: the satellite modem makers seen on
//! the air and a few common network and compute vendors. An address not
//! listed simply has no name. Add entries in OUI order (a test checks it).

/// `(OUI, manufacturer)`, sorted by OUI so
/// `vendor` can binary-search it.
const TABLE: &[([u8; 3], &str)] = &[
    ([0x00, 0x00, 0x0C], "Cisco Systems"),
    ([0x00, 0x03, 0x93], "Apple"),
    ([0x00, 0x05, 0x85], "Juniper Networks"),
    // Newtec: seen in the IP traffic of a Newtec hub (see `decsat-ts::newtec`).
    ([0x00, 0x06, 0x39], "Newtec"),
    // The example identifier in ETSI TS 103 129 (00:06:B0:FF:FF:01:AC:07).
    ([0x00, 0x06, 0xB0], "Comtech EF Data"),
    ([0x00, 0x07, 0xED], "Altera"),
    ([0x00, 0x09, 0x5B], "Netgear"),
    ([0x00, 0x0A, 0x35], "Xilinx"),
    ([0x00, 0x0C, 0x29], "VMware"),
    ([0x00, 0x14, 0x22], "Dell"),
    ([0x00, 0x15, 0x6D], "Ubiquiti Networks"),
    ([0x00, 0x1B, 0x21], "Intel"),
    ([0x00, 0x1B, 0x78], "Hewlett Packard"),
    ([0x00, 0x1C, 0x73], "Arista Networks"),
    ([0x00, 0x25, 0x90], "Super Micro Computer"),
    ([0x00, 0x50, 0x56], "VMware"),
    ([0x00, 0x80, 0xA3], "Lantronix"),
    ([0x00, 0x90, 0xE8], "Moxa"),
    ([0x00, 0xA0, 0xC9], "Intel"),
    ([0x08, 0x00, 0x20], "Sun Microsystems"),
    ([0x4C, 0x5E, 0x0C], "MikroTik"),
    ([0xB8, 0x27, 0xEB], "Raspberry Pi Foundation"),
    ([0xDC, 0xA6, 0x32], "Raspberry Pi Trading"),
];

/// The manufacturer an OUI (a MAC address's first three octets) is
/// assigned to, if it is in the table.
pub fn vendor(oui: [u8; 3]) -> Option<&'static str> {
    // `binary_search_by_key` returns `Ok(index)` on a hit and `Err(..)`
    // otherwise; `.ok()` turns that into an `Option`. Byte arrays compare
    // lexicographically, which is numeric order for big-endian octets.
    TABLE
        .binary_search_by_key(&oui, |&(o, _)| o)
        .ok()
        .map(|i| TABLE[i].1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_and_unique() {
        assert!(TABLE.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn looks_up_known_and_unknown() {
        assert_eq!(vendor([0x00, 0x06, 0xB0]), Some("Comtech EF Data"));
        assert_eq!(vendor([0x00, 0x06, 0x39]), Some("Newtec"));
        assert_eq!(vendor([0x12, 0x34, 0x56]), None);
    }
}
