// made by CasualArclamp
//! IP out of DVB-S2: packet validation, IPv4 fragment reassembly (`frag`),
//! a PCAP writer, live statistics, the blind IPv4 search (see `docs/DESIGN.md` §6 and Appendix B), multicast
//! audio detection (`mcast`) and playback (`relay`), a streaming server
//! for media players (`serve`), a radar station's monitoring traffic
//! (`rcms`), and ED-137 radio over IP's signalling and SIP (`ed137`).

pub mod ed137;
pub mod frag;
pub mod mcast;
pub mod packet;
pub mod pcap;
pub mod rcms;
pub mod relay;
pub mod serve;
pub mod stats;

pub use frag::Reassembler;
pub use mcast::{AudioStream, Codec, McastScanner, SdpInfo};
pub use packet::{IpInfo, blind_ipv4_search, parse};
pub use pcap::{PcapWriter, read_pcap};
pub use relay::{AudioRelay, PlayTarget};
pub use serve::StreamServer;
pub use stats::{Flow, IpStats};
