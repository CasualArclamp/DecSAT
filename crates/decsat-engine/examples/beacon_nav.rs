// made by CasualArclamp
//! Reads a beacon's recorded frames (the GUI's Record button: a .bin of
//! 256-byte minor frames) for attitude and orbit words, and prints them
//! every `step` major frames.
//!
//!   cargo run --release -p decsat-engine --example beacon_nav -- FILE.bin [step]

use decsat_engine::beacon::nav::NavRx;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("a .bin of 256-byte frames");
    let step: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);
    let data = std::fs::read(&path).expect("read");
    let mut rx = NavRx::new();
    for (n, f) in data.as_chunks::<256>().0.iter().enumerate() {
        rx.push(f);
        if (n + 1) % (32 * step) == 0 {
            match rx.view() {
                Some(nav) => {
                    println!("frame {}:", n + 1);
                    for (label, text) in nav.lines() {
                        println!("  {label}: {text}");
                    }
                }
                None => println!("frame {}: no attitude and orbit words", n + 1),
            }
        }
    }
}
