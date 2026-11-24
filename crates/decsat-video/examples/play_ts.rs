// made by CasualArclamp
//! Play a recorded `.ts` through the in-app player headless and say what
//! came out: `cargo run --release -p decsat-video --example play_ts -- FILE.ts`.

use std::time::{Duration, Instant};

use decsat_audio::OutputKind;
use decsat_video::file::{FileFeed, programmes};
use decsat_video::{Deinterlace, Selection, VideoPlayer, find_ffmpeg};

fn main() {
    let path = std::path::PathBuf::from(std::env::args().nth(1).expect("FILE.ts"));
    let progs = programmes(&path).unwrap();
    for p in &progs {
        println!("service {} {:?} pcr {:?}", p.number, p.name, p.pcr_pid);
        for e in &p.streams {
            println!("  {:#06x} {}", e.pid, e.describe());
        }
    }
    let p = progs
        .iter()
        .find(|p| p.streams.iter().any(|e| e.is_video()))
        .unwrap_or(&progs[0]);
    let video = p.streams.iter().find(|e| e.is_video());
    let mut pids = vec![0, p.pmt_pid];
    pids.extend(p.pcr_pid);
    pids.extend(p.streams.iter().map(|e| e.pid));
    pids.sort_unstable();
    pids.dedup();
    let sel = Selection {
        pids,
        video: video.map(|e| e.pid),
        audio: p.streams.iter().find(|e| e.is_audio()).map(|e| e.pid),
        deint: Deinterlace::default(),
        gpu: false,
        av1: video.is_some_and(|e| e.kind == Some("AV1 video")),
        aptx: false,
    };
    println!("{sel:?}");
    let exe = find_ffmpeg(None).expect("ffmpeg");
    let (tap, rx) = decsat_ts::TsTap::new(2048);
    let mut player = VideoPlayer::start(&exe, rx, sel, OutputKind::Null).unwrap();
    let feed = FileFeed::start(&path, p.pcr_pid, tap).unwrap();
    let t0 = Instant::now();
    let (mut shown, mut size) = (0, (0, 0));
    while !(feed.finished() && t0.elapsed() > Duration::from_secs(3))
        && t0.elapsed() < Duration::from_secs(120)
    {
        match player.poll() {
            Some(f) => {
                shown += 1;
                size = (f.width, f.height);
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    let st = player.status();
    println!(
        "{:.1} s: {shown} pictures {size:?}, sound decoded {:?}, progress {:.0} %",
        t0.elapsed().as_secs_f64(),
        st.audio.as_ref().map(|a| a.decoded),
        feed.progress() * 100.0
    );
    println!("streams {:?}", st.streams);
}
