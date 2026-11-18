// made by CasualArclamp
//! Turn a recorded BGAN payload (`bgan-…_payload.bin`) into a raw-IP pcap:
//! the carved IPv4 packets plus the ones rebuilt from link-layer segments.
//!   bgan_pcap PAYLOAD.bin OUT.pcap
//! The payload holds no arrival times, so packets are 1 ms apart in stream
//! order (where each packet starts in the payload).
use decsat_bgan::layers::pcap::{DLT_RAW, PcapWriter, carve_ipv4};
use decsat_bgan::layers::segments::reassemble;
use std::io::BufWriter;

fn main() -> std::io::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 3 {
        eprintln!("usage: bgan_pcap PAYLOAD.bin OUT.pcap");
        std::process::exit(2);
    }
    let blob = std::fs::read(&a[1])?;
    // (offset, packet bytes) from both sources, merged by offset
    let mut all: Vec<(usize, Vec<u8>)> = carve_ipv4(&blob)
        .into_iter()
        .map(|(o, p)| (o, p.to_vec()))
        .collect();
    let carved = all.len();
    all.extend(reassemble(&blob).into_iter().map(|p| (p.offset, p.data)));
    let rebuilt = all.len() - carved;
    all.sort_by_key(|x| x.0);

    let mut w = PcapWriter::new(BufWriter::new(std::fs::File::create(&a[2])?), DLT_RAW)?;
    for (k, (_, p)) in all.iter().enumerate() {
        w.write_record(k as f64 * 0.001, p)?;
    }
    println!(
        "{} packets ({carved} carved, {rebuilt} rebuilt from segments) -> {}",
        all.len(),
        a[2]
    );
    Ok(())
}
