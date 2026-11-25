<!-- made by CasualArclamp -->
# DecSAT manual: Transmit

This page describes the transmit panel and the `decsat-tx` application. [Back to the README](../../README.md).

## Transmitting (DVB-S2/S2X DATV)

**Tools ▸ 📡 Transmit…** opens the transmit panel: an MPEG-TS on a
DVB-S2 or S2X carrier, to the HackRF or to an IQ file. While it transmits, a
red **📡 Transmitting** button sits at the right of the menu bar.

- **Source:** a `.ts` file (played at its own rate, read from its PCRs;
  looped if you like), a UDP stream (raw TS or RTP, as ffmpeg or OBS send
  it: `ffmpeg … -f mpegts udp://127.0.0.1:1234`), or DecSAT's test stream.
  Whatever the input leaves of the channel goes as null packets.
- **Carrier:** any DVB-S2 MODCOD (normal or short frames) or S2X data
  MODCOD, pilots, roll-off 0.35–0.05, symbol rate, PL gold code. The panel
  shows the TS rate the channel carries, the bandwidth and the sample rate
  (4 samples a symbol where it fits, 2–20 MS/s).
- **Output:** the HackRF at a frequency, or an IQ file (`.cs8`, `.cs16`,
  `.cf32`) written in real time.

**The HackRF transmits only when you tick Armed and press Transmit.** TX
gain starts at 0 dB and the +14 dB amplifier off every time (neither is
remembered); antenna power is never switched on. Stop, closing the panel or
quitting puts the radio back to off. The HackRF cannot receive and
transmit at once, so Transmit stops a HackRF receive session; start it
again afterwards. Meant for a cable and attenuator into a receiver, or a
dummy load: radiating needs a licence for the frequency and the power.

From the command line, to a file:

```bash
decsat tx --ts video.ts --loop --modcod "8PSK 3/4" --symbol-rate 1000000 --out tx_4000000Sps.cs8 --seconds 30
```

## The transmitter app (`decsat-tx`): a webcam on QO-100

![DecSAT TX sending ffmpeg's test picture as H.265 on QO-100's net preset (333 kS QPSK 2/3) into the test output: the picture, the meters and the 400 kHz carrier's spectrum](../images/tx.jpg)

`decsat-tx` is a program of its own, beside the receiver (as DecDVB-T has
its own transmitter): a **camera and microphone** (or a video file, a `.ts`
file, a UDP stream or the test pattern), **encoded live by ffmpeg** into a
transport stream at exactly the channel's rate, on a **DVB-S2/S2X carrier**,
to the HackRF One or an IQ file. It needs `ffmpeg` (on the PATH, beside the
exe, or `winget install Gyan.FFmpeg`); its own settings are in
`%APPDATA%\DecSAT-TX\prefs.txt`.

- **Video:** H.264, **H.265/HEVC**, **H.266/VVC**, **AV1** or MPEG-2, on the
  graphics card's encoder where there is one (NVENC, AMF, Quick Sync: auto
  picks the first that works) or the CPU (x264, x265, vvenc, SVT-AV1). The
  picture size follows the rate (640×360 for H.265 at QO-100's 333 kS);
  VVC is held to 480×270, as vvenc takes about two cores even there.
  **AV1** travels as AOM's "Carriage of AV1 in MPEG-2 TS" (v1.0.1) has it:
  ffmpeg cannot do that mapping, so DecSAT writes it (`decsat_ts::av1`) and
  its player reads it back (most DATV receivers will not play AV1).
- **Audio:** AAC or MP2, mono; the microphone's level is shown.
- **QO-100 presets** from BATC / AMSAT-DL's 2021 bandplan (V3): the net
  (10499.25 MHz, 333 kS), the 14 narrow slots at 333/250 kS (the lower 9 at
  500 kS), the very narrow 125/66/33 kS slots, the wide 1000/1500 kS
  channels and the conference frequency; QPSK 2/3 (1/2 very narrow), roll-off
  0.20, pilots on, normal frames. The uplink is entered (2401–2410 MHz) and
  the downlink shown; a frequency or rate the bandplan does not allow is
  flagged. PIDs as BATC asks: PMT 4095, video 256, audio 257, the service
  named by your call sign (which can go on the picture). Every preset value
  cites its source in `crates/decsat-tx/src/qo100.rs`.
- **What it costs** (Ryzen 7 4800H, RTX 3050, the webcam at 640×360 or so,
  the net preset; % of one core): ffmpeg with H.265 on NVENC 15, H.264 on
  NVENC 10, x264 21, x265 31, SVT-AV1 40, MPEG-2 3, vvenc 180; the app
  itself about 10 (the modulator 8) while drawing.

**The HackRF transmits only when you have ticked "I have read this",
pressed ARM (good for 60 s, one transmission, lapses when the window loses
focus) and pressed TRANSMIT.** Every start is on the "Test only" output, TX
VGA 0 dB, amplifier off; none of those is remembered. Antenna power is never
switched on. Stop (or Escape, or Space), closing the window or quitting puts
the radio back to off. A receive session holding the HackRF (DecSAT's own,
or another program's) must be stopped first: the app says so and touches no
other program. Meant for a cable and attenuator, or a dummy load, unless you
hold a licence for QO-100's uplink; on the satellite, look before you
transmit and keep 1 dB below the beacon.

Checks without a radio (the radio cannot be reached from these):

```bash
decsat-tx --source camera --codec h265 --sink iq:tx_2331000Sps.cs8 --start --run-secs 10   # prints the CPU
decsat decode tx_2331000Sps.cs8 --decoder ts --symbol-rate 333000 --fast --out rx.ts
cargo run --release -p decsat-ts --example av1_ivf -- rx.ts/….ts av1.ivf   # an AV1 stream out of a TS, for ffmpeg
cargo run --release -p decsat-modem --example aptx_wav -- rx.ts/….ts wavs   # Enhanced apt-X services of a TS, a .wav each
```
