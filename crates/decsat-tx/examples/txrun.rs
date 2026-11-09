// made by CasualArclamp
//! The transmitter with no window: a real session into an IQ file or
//! nowhere (never the radio), a status line a second, the CPU at the end.
//!
//! ```text
//! cargo run --release -p decsat-tx --example txrun -- [options]
//!   --source camera[:NAME] | test | file:PATH      (default test)
//!   --mic NAME | --no-audio
//!   --codec h264|h265|h266|av1|mpeg2               (default h265)
//!   --engine auto|cpu|nvenc|amf|qsv                (default auto)
//!   --preset N                                     (a QO-100 preset's index, default 0: the net)
//!   --out PATH.cs8 | --null                        (default --null)
//!   --seconds S                                    (default 10)
//!   --retune-at S --retune-modcod N [--retune-short] [--retune-pilots]
//!                                                  (change the MODCOD on the air)
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use decsat_tx::encode::{self, Engine};
use decsat_tx::{
    AudioCodec, EncodeSettings, Input, Source, TxConfig, TxSession, TxSink, TxState, VideoCodec,
    qo100,
};

fn main() {
    // SAFETY (Rust 2024: changing the environment is `unsafe` because other
    // threads may read it): no other thread exists yet.
    unsafe { std::env::set_var(decsat_tx::NO_RADIO_ENV, "1") };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let has = |k: &str| args.iter().any(|a| a == k);
    let exe = encode::find_ffmpeg(None).expect("ffmpeg not found");
    let codec = get("--codec")
        .map(|c| VideoCodec::from_key(&c).expect("codec"))
        .unwrap_or(VideoCodec::H265);
    let devices = encode::list_devices(&exe);
    let source = get("--source").unwrap_or_else(|| "test".into());
    let audio = if has("--no-audio") {
        AudioCodec::None
    } else {
        AudioCodec::Aac
    };
    let input = if let Some(rest) = source.strip_prefix("camera") {
        let name = rest
            .strip_prefix(':')
            .map(String::from)
            .or_else(|| devices.cameras.first().cloned())
            .expect("no camera");
        let mic = get("--mic").or_else(|| devices.microphones.first().cloned());
        let modes = encode::camera_modes(&exe, &name);
        println!("camera {name}: {} modes; mic {mic:?}", modes.len());
        Input::Camera {
            video: name,
            audio: mic,
            mode: None,
        }
    } else if let Some(p) = source.strip_prefix("file:") {
        Input::File {
            path: PathBuf::from(p),
            looped: false,
        }
    } else {
        Input::TestPattern
    };
    let preset = &qo100::presets()[get("--preset").map_or(0, |p| p.parse().unwrap())];
    let datv = preset.datv(Default::default());
    let ts_rate = datv.ts_rate().unwrap();
    let working = encode::working_encoders(&exe);
    let wanted = match get("--engine").as_deref() {
        Some("cpu") => Some(Engine::Cpu),
        Some("nvenc") => Some(Engine::Nvenc),
        Some("amf") => Some(Engine::Amf),
        Some("qsv") => Some(Engine::Qsv),
        _ => None,
    };
    let engine = encode::pick_engine(codec, wanted, &working).expect("no working encoder");
    let mut enc = EncodeSettings {
        input,
        codec,
        engine,
        audio,
        service_name: "TEST".into(),
        ..EncodeSettings::default()
    };
    // The camera's capture mode for the picture the rate gives.
    let vb = encode::video_rate(ts_rate, enc.audio, enc.audio_bps);
    let pic = encode::picture_for(codec, vb);
    if let Input::Camera { video, mode, .. } = &mut enc.input {
        *mode = encode::best_mode(&encode::camera_modes(&exe, video), pic);
    }
    println!(
        "{} at {:.3} MHz: {} {}, TS {:.0} kbit/s, video {} kbit/s {}, {} {}",
        preset.name,
        preset.uplinks_mhz[0],
        decsat_core::modcod_name(datv.modcod),
        datv.symbol_rate,
        ts_rate / 1e3,
        vb / 1000,
        pic.describe(),
        codec.label(),
        engine.label()
    );
    let seconds: f64 = get("--seconds").map_or(10.0, |s| s.parse().unwrap());
    let sink = match get("--out") {
        Some(p) => TxSink::IqFile {
            path: PathBuf::from(p),
            seconds: Some(seconds),
        },
        None => TxSink::Null,
    };
    let mut s = TxSession::new(TxConfig {
        datv,
        source: Source::Encode(enc),
        sink,
        preview: has("--preview"),
        ..TxConfig::default()
    });
    let t0 = Instant::now();
    s.prepare().expect("prepare");
    while s.status().state == Some(TxState::Preparing) {
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("ready after {:.2} s", t0.elapsed().as_secs_f64());
    if let Err(e) = s.transmit() {
        eprintln!("transmit: {e}");
        std::process::exit(1);
    }
    let t1 = Instant::now();
    let mut retune_at: Option<f64> = get("--retune-at").map(|v| v.parse().unwrap());
    let (mut ff_sum, mut app_sum, mut mod_sum, mut n) = (0.0, 0.0, 0.0, 0);
    loop {
        std::thread::sleep(Duration::from_millis(1000));
        if retune_at.is_some_and(|at| t1.elapsed().as_secs_f64() >= at) {
            retune_at = None;
            let m: u8 = get("--retune-modcod")
                .expect("--retune-modcod")
                .parse()
                .unwrap();
            let r = s.set_mode(m, has("--retune-short"), has("--retune-pilots"));
            println!("retune to {}: {r:?}", decsat_core::modcod_name(m));
        }
        let st = s.status();
        let ff = st.ffmpeg.clone().unwrap_or_default();
        println!(
            "{:5.1} s {:?} ts {:.2} s nulls {} starved {} | ffmpeg {:5.1} % speed {:?} fps {:?} | modulator {:4.1} % app {:5.1} % | peak {:.1} dBFS clipped {}{}",
            t1.elapsed().as_secs_f64(),
            st.state.unwrap(),
            st.ring_secs,
            st.nulls,
            st.starved,
            ff.cpu_pct,
            ff.speed,
            ff.fps,
            st.modulator_pct,
            st.app_cpu_pct,
            st.peak_dbfs,
            st.clipped,
            ff.av1
                .map(|a| format!(" | AV1 level {}", a.level_idx_0))
                .unwrap_or_default(),
        );
        if st.state != Some(TxState::Transmitting) {
            if let Some(e) = st.error {
                eprintln!("error: {e}");
            }
            for l in ff.log {
                eprintln!("ffmpeg: {l}");
            }
            break;
        }
        // Skip the first two seconds (start-up) in the averages.
        if t1.elapsed().as_secs_f64() > 2.5 {
            ff_sum += ff.cpu_pct;
            app_sum += st.app_cpu_pct;
            mod_sum += st.modulator_pct;
            n += 1;
        }
        if t1.elapsed().as_secs_f64() > seconds + 3.0 {
            break;
        }
    }
    if n > 0 {
        let n = n as f32;
        println!(
            "mean CPU (% of one core): ffmpeg {:.1}, this program {:.1} (modulator {:.1})",
            ff_sum / n,
            app_sum / n,
            mod_sum / n
        );
    }
}
