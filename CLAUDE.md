<!-- made by CasualArclamp -->
# DecSAT — working notes

Read [`docs/DESIGN.md`](docs/DESIGN.md) first: it holds the agreed scope,
the crate layout and the milestone plan. Update it when a decision changes.

## Commands

```bash
cargo check --workspace --all-targets   # fast feedback
cargo test --workspace                  # unit tests
cargo clippy --workspace --all-targets -- -D warnings
cargo run --release -p decsat-gui       # the GUI
cargo run --release -p decsat-cli -- modcods
cargo run --release -p decsat-io --example net_send -- FILE.cf32 8M tcp://127.0.0.1:7777 int16   # stand-in for SDR++'s IQ Exporter: plays a capture out over TCP (listens), connect:// or udp://
cargo run --release -p decsat-cli -- decode --net tcp://127.0.0.1:7777 --iq int16 --rate 8000000 --decoder dvbs   # network IQ (SDR++, GNU Radio, rtl_tcp); also listen://, udp://
# the transmitter app (decsat-tx): its own window and prefs (%APPDATA%\DecSAT-TX\prefs.txt). Tests and these hooks never reach the radio (DECSAT_TX_NO_RADIO is set for them)
cargo run --release -p decsat-tx-app                                     # the TX window
cargo run --release -p decsat-tx-app -- --source camera --codec h265 --sink iq:out/tx_2331000Sps.cs8 --start --run-secs 10   # a real session into a file: prints the CPU
cargo run --release -p decsat-tx-app -- --source test --sink null --start --screenshot out/tx.png --after 9   # screenshots from the test pattern only (public repo)
cargo run --release -p decsat-tx --example txrun -- --source camera --codec av1 --out out/tx.cs8 --seconds 8   # the library alone, a status line a second
cargo run --release -p decsat-cli -- decode out/tx.cs8 --rate 2331000 --decoder ts --symbol-rate 333000 --fast --out out/rx.ts   # read a transmission back
cargo run --release -p decsat-ts --example av1_ivf -- out/rx.ts/x.ts out/av1.ivf   # AV1 (AOM's TS mapping) out of a TS for ffmpeg
cargo run --release -p decsat-modem --example aptx_wav -- out/rx.ts/x.ts out/wavs   # Enhanced apt-X services (PMT indicator APTx) of a TS to .wav
cargo run --release -p decsat-server -- --bind 0.0.0.0:5262 --password-file pw.txt   # a radio for a client elsewhere (docs/REMOTE.md); receive only
cargo run --release -p decsat-server -- --file capture.cs8 --file-rate 8e6 --file-center 1100e6 --only file --bind 127.0.0.1:5262   # a capture as a remote radio
cargo run --release -p decsat-cli -- remote 127.0.0.1:5262 --radio file --mhz 1100 --rate 8e6 --decoder dvbs2-ts --offset=1200000 --bandwidth=700000 --out out/   # smart mode: decodes on the server
cargo run --release -p decsat-cli -- decode --remote 127.0.0.1:5262 --remote-radio file --remote-mhz 1100 --rate 8e6 --remote-bits 8 --decoder ts --offset=1200000 --bandwidth=700000   # dumb mode: IQ over the link
cargo test --release -p decsat-remote                                     # codecs, and server + clients over loopback on a fake radio
cargo run --release -p decsat-cli -- decode --pluto 1450 --rate 4e6 --pluto-gain 40 --decoder id   # live ADALM-PLUTO / Pluto+ (--pluto-uri auto|usb|ip:ADDR, --pluto-gain agc, --pluto-16bit); receive only
cargo test -p decsat-io --features pluto pluto                                     # the Pluto driver against a fake IIOD server
PLUTO_URI=ip:192.168.2.1 cargo test -p decsat-io --features pluto live_pluto -- --ignored --nocapture   # a real Pluto (PLUTO_URI defaults to auto); receive only
cargo run --release -p decsat-cli -- decode recording.wav --decoder p3 --bandwidth 8000 --fast   # QO-100's PSK beacon from a WebSDR's mono WAV
cargo run --release -p qo100-beacon -- --file recording.wav --speed 20 --screenshot out/b.png --after 9   # the standalone sound-card app (--device NAME for a sound input)
cargo run --release -p decsat-gui -- --net tcp://127.0.0.1:7777 --iq int8 --rate 8 --net-mhz 1100 --lo 9750 --claim-carriers --screenshot out/x.png --after 10
```

CMake for libopus: if `cmake` is not on PATH, Visual Studio's own is at
`C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin`.

libopus (Opus radio) is the `third_party/opus` submodule (tag v1.6.1), built
by `decsat-opus-sys` with CMake, as in DecDRM; libxaac (AAC with SBR/PS,
Apache-2.0) is `third_party/libxaac` (v0.1.13), its decoder built by
`decsat-xaac-sys` with `cc` (and its encoder, for the transmitter's HE-AAC, with the
`encoder` feature). Not FDK-AAC: its licence does not mix with the
GPL. `git submodule update --init` fetches both.

The live HackRF front end is behind `--features hackrf` (on `decsat-io`, re-exported
by both apps), so a plain build never needs the HackRF SDK and CI stays green.
The Fobos SDR is behind `--features fobos` (on by default in both apps): it
loads RigExpert's `fobos_sdr`/`fobos` library at run time, so building needs
nothing extra. `DECSAT_FOBOS_LIB=/path/to/lib cargo test -p decsat-io
--features fobos live_fobos -- --ignored` runs the live test against a board
(or a stand-in library).
The Airspy Mini / R2 is behind `--features airspy` (on by default in both
apps): pure Rust over USB through `libairspy-rs`, no DLL.
The SDRplay RSPduo / RSP is behind `--features sdrplay` (on by default in the apps):
it loads SDRplay's `sdrplay_api.dll` (API v3, installed from sdrplay.com with its
service) at run time, like the Fobos; `DECSAT_SDRPLAY_LIB` overrides the path. Single
tuner mode only (RSPduo Tuner A or B), zero-IF 2-10.67 MS/s, AGC off, bias-T only on request (`bias_tee`).
Gains are reductions (LNA state, IF GR 20-59 dB). Structs are transcribed from the
v3.15 headers; the live test (`cargo test -p decsat-io --features sdrplay live_sdrplay
-- --ignored --nocapture`) needs a radio. CLI `--sdrplay MHz`, GUI `--sdrplay MHz`, server key `sdrplay`.
The ADALM-PLUTO / Pluto+ / LibreSDR is behind `--features pluto` (on by
default in both apps): its IIOD protocol in pure Rust over USB (nusb, ADI's
WinUSB driver) or TCP 30431, no libiio, no DLL. The receiver never opens the
DDS device and powers the TX LO down; the transmitter (`decsat-io/src/pluto/tx.rs`,
`PlutoTx`) is the TX app's "ADALM-PLUTO / Pluto+ (radio)" output, behind the
same arm as the HackRF, starting at 89.75 dB attenuation, its LO up only after
the first block, and putting the LO back up if a receiver on the same Pluto
powers it down (full duplex: both on ip:192.168.2.1 at the same rate). Tested
against the fake IIOD server only (`cargo test -p decsat-io --features pluto tx_`).
Ported from DecDVB-T's driver.

## House rules

- **Licence:** GPL-3.0-or-later. The project borrows algorithms from GPL-3 code
  (gr-dvbs2rx, leansdr, gr-dtv) and MIT code (dontlookup). Credit the source in a
  comment at the function or module that borrows from it.
- **Never commit** IQ captures, reference clones (`reference/`), or built exes.
- **Specs are the authority:** ETSI EN 302 307-1 / -2. Cite the clause or table
  number in comments next to constants, tables and bit layouts.
- The user has a strong DSP background but is newer to Rust: explain Rust idioms
  in comments, not DSP basics.
- DSP is unusably slow unoptimised, so `[profile.dev]` runs at `opt-level = 1`
  with dependencies at 3. Prefer `--release` for anything timing-sensitive.
- Heavy test sweeps belong on CI, not on the user's machine.

## Release

See the project memory: bump `[workspace.package].version` by matching the
`version = "…"` line, commit, wait for CI, annotated tag, push the tag; the
release workflow builds the portable static-CRT exes and drafts the release.
