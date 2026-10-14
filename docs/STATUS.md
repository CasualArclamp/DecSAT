# Status

> **Renamed DecSAT on 2026-10-10** (it does far more than DVB). Entries
> before that use the old name — crates `decdvb-*`, binaries `decdvb` /
> `decdvb-gui`, recordings `decdvb-…` — as they were then.

Running log of what is done and what is next. `docs/DESIGN.md` holds the plan.

## M0 — skeleton (done, 2026-10-08)

Workspace builds clean, tests and clippy green.

- `decdvb-core` — `Modulation`, `CodeRate`, `FecFrame`, `Modcod` + the 28
  standard DVB-S2 MODCODs; `RollOff` (0.35…0.05); `SampleFormat` (cs8/cs16/cf32)
  and IQ byte conversion; `RxConfig`, `Metrics`, `Error`.
- `decdvb-io` — `IqSource` trait, `IqFileReader` / `IqFileWriter`, extension
  sniffing. HackRF front end scaffolded behind the `hackrf` feature (loads
  `libhackrf` at run time in M1, so no SDK is needed to build).
- `decdvb-dsp` — root-raised-cosine taps (unit energy, singularities handled)
  and a streaming complex FIR.
- `decdvb-engine` — `Spectrum` (Hann, 50 % overlap, averaged, FFT-shifted) and
  `estimate_band`: carrier centre, 99 % occupied bandwidth, noise floor, and a
  **symbol-rate estimate from the equivalent noise bandwidth** — exact for a
  root-raised-cosine signal regardless of roll-off (see the doc comment).
- `decdvb-cli` — `modcods`, `analyse`, and `synth` (a shaped QPSK test-signal
  generator with frequency offset and Es/N0; not a DVB-S2 modulator, that is M6).
- `decdvb-gui` — eframe/glow window: open a capture, spectrum and constellation.
- CI on GitHub Actions: Linux + Windows build/test, clippy, rustfmt, and a build
  with the `hackrf` feature to prove it compiles with no SDR attached.
- Stub crates in place for `fec`, `frame`, `gse`, `ts`, `ip`, `mod`.

Verified by hand: `synth` then `analyse` recovers the carrier offset and symbol
rate of signals at 125 kS/s–1 MS/s with roll-offs 0.05–0.35.

## M1 — acquisition and PL sync (done, 2026-10-08)

### Done: the PLHEADER (`decdvb-frame`)

The 90-symbol PLHEADER is fully implemented and tested — this is the part that
makes ACM possible at all, since every PLFRAME announces its own MODCOD.

- `defs` — PLFRAME geometry (SOF 26, PLSC 64, slot 90, pilot block 36 every
  16 slots), the SOF pattern and the PLS scrambler.
- `pi2bpsk` — pi/2-BPSK map, coherent hard/soft demap, and **differential**
  demap for use before carrier lock.
- `rm` — the interleaved **(64, 7, 32) Reed–Muller** code the PLS code is
  protected by, with hard (Hamming) and soft (max-inner-product) decoding. The
  soft path folds descrambling into the codeword table. Tests confirm minimum
  distance 32 and correction of any 15-bit error pattern.
- `plsc` — `PlsInfo`: MODCOD, FECFRAME length, pilots, and the frame geometry
  they imply (slots, pilot blocks, XFECFRAME and PLFRAME lengths). A test
  checks `slots × 90 × bits_per_symbol` equals the FECFRAME length for all 28
  MODCODs in both frame lengths, so the geometry cannot silently drift.
- `sync` — `PlHeaderCorrelator`, differential correlation against SOF (25 taps)
  plus PLSC (32 taps). The PLSC taps come from the **scrambler alone**: the
  interleaved Reed–Muller construction makes each consecutive codeword bit pair
  either equal or opposite, so the pairwise differential is fixed up to a 180°
  flip that `max(|SOF+PLSC|, |SOF-PLSC|)` resolves. So the correlator finds a
  PLHEADER without knowing its PLS code, and — being differential — works at
  frequency offsets up to at least 1 % of the symbol rate. Verified: correct
  peak position for all 32 MODCODs, under noise, and under offset.

### Done: PL scrambling, timing, streaming demodulator

- `scramble` — Gold-code PL scrambling (§5.5.4); exact round trip, x LFSR
  verified maximal-length.
- `decdvb-dsp::timing` — Gardner TED + cubic Farrow interpolator, at
  fractional samples per symbol; verified from 2.7 up to 84 sps.
- `decdvb-engine::demod` — streaming PL demodulator: matched filter → AGC →
  timing → PLHEADER correlation → **frame lock** → PLS decode → descrambling.
  A frame is emitted only once the *next* header is found exactly where this
  frame's PLS code predicted, which validates the PLS decode and the frame
  length together — what makes following ACM safe. Follows a five-MODCOD ACM
  sequence with zero losses, re-acquires after a dropout, never invents frames
  from noise.

### Done: live HackRF (2026-10-08)

`decdvb-io::hackrf` — pure Rust over USB via `seify-hackrfone` (MIT, nusb):
no libhackrf, no libusb, no DLLs, so the portable exe stays one file, and the
feature is now on by default. Receive only; antenna-port power forced off on
every configuration. A reader thread keeps three 256 KB transfers in flight and
hands buffers over a bounded channel, dropping (and counting) whole buffers if
the consumer falls behind. The driver's gain setters `panic!` on an unexpected
reply, so gains are snapped to valid steps and calls run under `catch_unwind`.
Verified on Rory's HackRF: 7.99 MS/s delivered at 8 MS/s, and the GUI running
live at 20 MS/s with the front end at 1–2 % CPU. GUI: a HackRF panel with
frequency, rate, LNA/VGA/amp and an LNB LO for the axis; frequency and gains
apply live.

**Carrier detection, hardened on real signal.** The live FM band first
produced 168 "carriers": a regular ~100 kHz comb (likely switching-PSU or USB
interference near the PC, or overload; not investigated) whose every tooth
passed the threshold. Now: regions are split only at valleys that are deep
relative to both sides' robust (95th-percentile) tops *and* where both sides
have carrier-like flat tops; narrow lines are recognised by a short top run
that holds most of the power; a region with a non-flat top is flagged `rough`
(orange "lump"); spurs are ticks, not brackets; clean carriers outrank spurs
under the 48-carrier cap. The same band now reads 6 carriers + 42 narrow lines,
the comb as one lump. Six regression tests cover it.

### Done: carrier recovery, generic PSK, narrow carriers (2026-10-08)

From live Ku-band use:

- **Carrier recovery** (`decdvb-dsp::carrier`): a second-order PLL,
  decision-directed or data-aided. Lock is judged by *coherence*,
  |⟨e^{jS·err}⟩| folded by the constellation's symmetry — MER alone cannot
  tell (a rotating QPSK ring still reads ~7 dB, 8PSK ~13 dB).
- **DVB-S2 VFOs** (`demod`): the loop runs over each frame as it is emitted.
  Data-aided over the PLHEADER and pilots, so the absolute phase is resolved;
  decision-directed over the data against that frame's MODCOD constellation,
  so ACM stays locked across QPSK/8PSK/APSK. Seeded at acquisition from the
  two confirming headers (lag-1/8/32 cascade on the de-modulated header, near
  the CRB) or from Identify's offset when the headers agree. `PlFrame.payload`
  is now carrier-corrected — ready for M2.
- **Identify** shows a locked constellation and MER: SOF-summed coarse offset
  for DVB-S2 (else the power line), then the same PLL.
- **Generic PSK → symbols** (`psk`): blind acquisition through Identify, then
  MF → AGC → Gardner → PLL; hard decisions to a `.bin`, one byte per symbol.
- **Narrow carriers**: VFOs down to 500 Hz; demodulator acquisition now takes
  Identify's first look (≤ 2 s) instead of a fixed 300 000 samples, which on a
  10 kBd VFO had meant 6–12 s before anything happened. Identify's in-band
  floor moved to the 3rd percentile — a carrier filling its VFO had read as
  "no signal".
- **GUI**: SDR++-style frequency readout across the top; ✕ on VFO labels and
  list rows. egui's proportional font lacks →, ✕ and ●, which rendered as
  boxes: the X is now drawn, and the bundled monospace font is a fallback.

Tests: a 10 kBd QPSK carrier with 150 Hz LNB error and phase noise through a
narrow VFO; 8PSK through a VFO to a `.bin`; an ACM sequence locked frame by
frame with pilots on their true phase; a wrong seed overruled by the headers.

### M1 complete

Every M1 item is in: RRC, timing, carrier recovery, PLHEADER sync and decode,
descrambling, pilot tracking, a locked constellation.

### Still to do

1. DC/IQ-imbalance correction for the HackRF's DC spur.
2. NCO mixing per VFO in f32 / lane-parallel (it is f64 per sample now).
3. The carrier loop's nearest-point search is linear in the constellation
   size; a sector lookup would cut 32APSK's cost.

## M1b — wideband waterfall + VFOs + Identify (done, 2026-10-08)

Rory asked for an SDR++-style app: a big waterfall, VFOs dropped on it each
running a decoder of choice, and a "what is this?" mode. Design in DESIGN.md
§3a/§3b.

- **Front end** (`frontend`): one thread reads the source in ~1/25 s blocks,
  makes a waterfall row per block, keeps a smoothed spectrum, detects carriers
  twice a second, and shares each block with every VFO by `Arc` (no copies).
  A slow VFO has blocks dropped and counted rather than stalling the rest.
- **VFOs** (`vfo`): one worker thread each; settings travel through a mailbox
  slot so the GUI never blocks on a busy worker (a bounded channel would have).
  Decoders: Identify, DVB-S2 → IP, DVB-S2 → TS (both run the PL demodulator
  until FEC exists), IQ recorder, spectrum only.
- **DDC**: NCO + one or two decimating FIR stages, the split chosen to minimise
  multiply-adds; stopband just past the VFO's edge so a VFO delivers what its
  edges show.
- **Carrier detection** (`carriers`): runs over a 20th-percentile floor (a
  busy span still finds the true noise), ignores the DC spur, separates
  adjacent carriers, estimates each one's symbol rate from its noise bandwidth.
- **Identify** (`identify`): see DESIGN.md §3b. The symbol rate from the cyclic
  line of |x|² is accurate to ~1e-8 on test carriers. Adaptive listening: a
  quick first look, then — if no headers were seen and the carrier is too slow
  to have shown three frames — a longer listen sized from the measured rate,
  the interim verdict marked provisional. Rests 3 s between checks once sure.
- **GUI**: spectrum + ring-texture waterfall, zoom/pan, VFO boxes with live
  badges, green brackets on detected carriers (click to claim), side bar with
  VFO list (CPU per VFO), settings, Identify card, MODCOD table, constellation,
  zoomed spectrum. Unattended mode for screenshots/smoke tests:
  `--claim-carriers --select N --after S --screenshot out.png`.
- **CLI**: `scene` (the multi-carrier test capture), `scan` (find and identify
  every carrier).

Verified on the `scene` capture: all four carriers found (centres within
0.2 kHz, symbol rates within 1–2 %) and all four identified correctly — DVB-S2
CCM QPSK 1/2; DVB-S2 ACM QPSK 1/2 → 8PSK 3/5 → 16APSK 2/3; plain QPSK as
"possibly DVB-S (not verified)"; a CW as a narrow carrier.

Bugs found by that real run and fixed, each with a regression test:
- the DDC stopband sat at the aliasing limit, passing a neighbour ~1.7 MHz away
  into a 1.5 MHz VFO and collapsing the symbol-rate estimate to 47.5 kS/s;
- the timing loop's buffer trim overran at > 8 samples/symbol (panic);
- the timing loop's gains were in samples, not symbols, so it ran 1/sps too
  slow — 84× at 84 sps;
- an Identify VFO dragged to another carrier kept analysing the old signal.

CPU per VFO on the scene went from 45/3/30/38 % to 11/3/8/6 % (two-stage DDC,
vectorisable FIR inner loop, Identify resting once sure). Measured and
rejected: AVX2 run-time dispatch with a flat tap layout — slower on these
window lengths (notes in `decdvb-dsp::dot`).

## M2 — FEC core (done, 2026-10-08)

PLFRAMEs now decode to BBFRAMEs, for all 21 DVB-S2 codes (normal and short).

- **LDPC** (`decdvb-fec::ldpc`): the Annex B/C address tables (generated from
  gr-dvbs2rx's copy of the standard's), an encoder, and a layered normalized
  min-sum decoder over the codes' quasi-cyclic structure — q layers of 360
  checks, each a block of lane-parallel arithmetic that vectorises. 8-bit
  messages, 16-bit posteriors: with 8-bit posteriors a saturated value stops
  being the sum of its messages and decoding can diverge outright (measured:
  36 000 bit errors where 16-bit converges). Normalization 7/8 (3/4 left
  rates 1/4 and 1/3 stuck a few bits short). A decode stuck on a couple of
  parity checks — two adjacent accumulator bits both wrong, which min-sum
  holds forever — stops early and leaves the verdict to BCH. ~0.5 ms per
  iteration per normal frame (release); QPSK 1/2 at 1.5 dB Eb/N0 converges in
  ~12 iterations.
- **BCH** (`bch`): GF(2^16) / GF(2^14), generator built from the minimal
  polynomials of α, α³, … (checked against Tables 6a/6b); a byte-wise
  remainder says "clean" in one pass, else syndromes, Berlekamp–Massey and a
  Chien search over the shortened positions.
- **Demapping** (`demap`): the Table 8 interleaver (8PSK 3/5 reads columns
  2,1,0), max-log LLRs, scaled by the gain and noise measured on each
  frame's header and pilots.
- **BBFRAMEs** (`decdvb-frame::bbframe`): BB descrambling (sequence checked
  against Figure 5: `03 F6 08 34 …`), BBHEADER parse with CRC-8 (check value
  0xBC), normal vs high-efficiency mode read from CRC vs CRC ⊕ 1.
- **Transmit side** (`decdvb-mod`): TS-mode BBFRAMEs (CRC-8 replacing sync
  bytes, SYNCD), BCH and LDPC encoders; `PlFramer` builds real FECFRAMEs, so
  test signals and the `scene` capture decode end to end.
- **VFOs**: each DVB-S2 VFO runs FEC on its own thread behind a bounded queue
  (frames dropped and counted if it falls behind); the side bar shows good
  BBFRAMEs, LDPC iterations, BCH corrections, Es/N0, the stream (TS/GS,
  CCM/ACM, ISI, roll-off) and the payload rate.

**What FEC exposed.** The first end-to-end run failed every QPSK frame below
6 dB Es/N0, though LDPC alone works at 1 dB. Causes, in order of impact:

1. The PLS code was read differentially with hard decisions — a QPSK 1/2
   header read as 3/5 (same frame length, so the grid still held and FEC ran
   the wrong code). Headers are now read coherently (phase anchored on the
   SOF, soft decisions) once the carrier loop runs, and confirmed by coherent
   correlation; acquisition reads both headers coherently from an SOF-only
   frequency estimate.
2. Cycle slips in the decision-directed carrier loop: the phase is now
   re-anchored on every header and pilot block, and the loop bandwidth follows
   the measured Es/N0 (0.01 → 0.002), seeded from the headers at acquisition.
3. The timing loop narrows once frames are locked.

Result: QPSK 1/2 with pilots decodes every frame at 2 dB Es/N0 (threshold
~1 dB), Es/N0 read within 0.2 dB. Without pilots: clean at 5 dB, about one
frame in seven lost to a slip at 4 dB — what pilots are for. An ACM sequence
(QPSK 1/2, 8PSK 3/5, short 16APSK 2/3, short 32APSK 3/4) decodes bit-exact
against the BBFRAMEs that were sent.

Also in this round: the generic PSK decoder writes only while Record is on;
Identify keeps a live constellation between identifications; middle/right
drag moves the spectrum and, past the span's edge, tunes a live HackRF.

### Next

- M4: GSE → IP → PCAP (dontlookup's quirks), TS output — the FEC thread is
  where they hook in.
- M3: S2X codes and constellations.
- FEC throughput for wide carriers: explicit SIMD, or a decoder pool.
- Cycle-slip resistance without pilots (non-causal phase smoothing between
  headers).

## M4, part 1 — GSE → IP → PCAP (done, 2026-10-08)

- **`decdvb-ip`**: IPv4/IPv6 validation (IPv4 header checksum, lengths),
  a classic PCAP writer (LINKTYPE_RAW), live statistics (protocols, a
  bounded flow table, top talkers) and the blind IPv4 search.
- **`decdvb-gse`**: a GSE decoder per variant — GSE_LENGTH standard or
  counting the header (dontlookup's "hdrlen"), frag id 8-bit or 6+2 split —
  with reassembly across BBFRAMEs, label types incl. re-use, padding, and the
  CRC-32/MPEG-2 over total length, protocol type, label and PDU. `GseIp` runs
  all four on every data field, scores each by valid IP and passes on the
  best one's packets; a blind IPv4 search takes over if none yields IP for
  50 fields. Bridged-Ethernet PDUs (type 0x0001) give up their IP. An
  encapsulator produces any variant, for tests and the modulator.
- **Engine**: good generic-stream BBFRAMEs (UPL 0) go from the FEC thread
  through `GseIp`; **● Record** writes PCAP (a new file per recording, with
  no restart). The GSE variant can be forced per VFO.
- **Test signals**: `GseBbFramer` puts UDP flows between RFC 5737
  documentation addresses into GSE in any variant; the `scene` capture's
  1 MS/s carrier now carries IP over GSE.

Verified: a QPSK 3/4 short-frame carrier carrying GSE written the
non-standard way (header-inclusive length, split frag ids) through a VFO —
the variant found from the data, fragments reassembled with CRCs matching,
and a PCAP whose every record parses as IP. In the GUI on the scene: 389 of
389 BBFRAMEs, 782 IPv4/UDP packets at 847 kbit/s.

Note: the CRC-32 coverage and total-length meaning follow TS 102 606-1 as
read here, and match this encoder; dontlookup computes the CRC but ignores
mismatches, so real links will show whether they agree (the GSE variants
table in the side bar counts CRC ok / bad).

### Next

- MPEG-TS output from TS-mode BBFRAMEs (`decdvb-ts`): sync-byte restore
  from the CRC-8s, null-packet re-insertion, `.ts` file / UDP.
- Live confirmation on a real GSE carrier.

## M4, part 2 — MPEG-TS out (done, 2026-10-08)

- **`decdvb-ts`**: TS-mode data fields back to 188-byte packets. The
  deframer treats the data fields as one stream and locks where SYNCD says,
  verified by the CRC-8 chain (each packet's sync position holds the CRC of
  the one before); if the chain fails there it searches every offset —
  which also reads links whose encoders get SYNCD wrong (dontlookup's
  "generic" and "Newtec" parsers). Sync bytes restored, failed CRCs flagged
  in the transport error indicator, NPD null packets re-inserted, ISSY
  counted (not read yet). A frame the demodulator, queue or FEC lost breaks
  the stream: `PlFrame::after_gap`, queue drops and failed frames all
  trigger a resync.
- **Analysis**: per-PID packets, continuity errors and scrambling; PAT, PMT
  and SDT (with CRC-32) for programme numbers, stream types and service
  names (DVB text, UTF-8 or Latin-1).
- **Outputs** from the FEC thread, switchable live: `.ts` file; UDP (7
  packets per datagram); a TCP server that answers HTTP so VLC and PotPlayer
  open `http://127.0.0.1:8001/`, each client on its own thread with a bounded
  queue. ▶ VLC / ▶ PotPlayer start the server and launch the player once it
  is up.
- **Test signals**: the TS carrier carries a PAT, PMT and SDT ("DecDVB test
  signal") every 40 packets.

Verified: a QPSK 1/2 TS carrier through a VFO to a UDP socket — every
datagram 7 packets with sync bytes, no CRC or continuity errors, the
service name read from the SDT; the TCP server tested with a raw and an
HTTP client on loopback. GUI on the scene: 5896 TS packets at 680 kbit/s.

Also: the toolbar's 📁 Output folder (remembered in
`%APPDATA%\DecDVB\prefs.txt`), LNB LO default 10700 MHz.

## M4, part 3 — TS analyser, MPE, multicast audio (done, 2026-10-08)

- **TS analyser** (`decdvb-ts::psi`, GUI `ts_viewer`): per-PID rate, PCR,
  PES stream id, CC/TEI errors, scrambling; PAT, CAT (EMM), PMT (languages,
  AC-3/E-AC-3/AAC, subtitles, teletext, CA systems → ECM PIDs), SDT, NIT
  (network name, satellite transponders), EIT present/following, TDT/TOT.
  An EBSPro-style window: sortable PID table with share bars, service tree
  with now/next, NIT transponders, tables seen.
- **MPE** (`decdvb-ts::mpe`, EN 301 192 §7): DSM-CC private sections found on
  any unscrambled non-PES PID by their table id; IP datagrams (LLC/SNAP or
  not) go to the same IP stage as GSE — statistics and multicast audio work
  for TS carriers too. Section reassembly is shared (`section.rs`).
- **Multicast audio** (`decdvb-ip::mcast`, `relay`): UDP multicast flows;
  RTP recognised by a steady SSRC and counting sequence numbers; codecs from
  sync words (ADTS, LOAS, MPEG audio) or RTP formats (RFC 3640, PT 14); SAP
  announcements' SDP names and describes streams. Playback: described RTP is
  relayed to a local port with an SDP file for VLC/PotPlayer; a bare
  elementary stream is served over local HTTP. `decdvb-ip::serve` is the
  shared HTTP/TCP streaming server (TS uses it too).
- **Test signals**: a multicast "test radio" (RTP MPEG audio of silent layer
  II frames, announced by SAP) on the GSE carrier, and in MPE on the TS one.

Bugs found on the way, each with a test: a relay started before a stream was
classified kept the wrong mode for good (now waits for 8 packets); stale
audio and top-flow lists after a burst; and the demodulator ran its timing
loop across a whole input block before frame logic could re-widen it after a
lost lock — one call with everything after a dropout never re-acquired (it
now works in 16 k-sample chunks).

## M3, part 1 — S2X FEC layer (done, 2026-10-08)

- The 31 S2X LDPC codes for normal and short FECFRAMEs (EN 302 307-2
  Annex B/C, generated from gr-dvbs2rx's tables: rates 2/9 … 154/180, the
  "-L" rates 90/180 … 22/30 kept unreduced), plus the three medium-frame
  tables for VL-SNR. K_bch/N_bch/t for each (all t = 12).
- Encoder, layered decoder and BCH tested over every S2X code exactly as
  over the S2 ones (codewords check, decoding near threshold, t errors
  corrected).

**What S2X still needs — and why it is not done yet.** The reference code
on hand stops at the FEC layer: none of it has the S2X physical layer — the
8-bit PLS code and the MODCOD 128–255 table, the 8/16/32/64/128/256APSK
constellations (radii and bit labels), the S2X bit-interleaver permutations,
VL-SNR headers and pi/2-BPSK spreading, or superframing. Those come from
EN 302 307-2 itself (or gr-dtv's S2X modulator); they are tables where a
guess would silently decode nothing, so they wait for the source.

## M4, part 4 — multicast audio in the app (done, 2026-10-08)

Rory asked for the multicast radio to decode in DecDVB itself, with a volume
slider and playback controls, the list sorted by address, and a file output.

- **`decdvb-audio`** (new crate). `depay`: UDP payloads to frames — MPEG audio,
  ADTS and LOAS byte streams re-framed by sync word and length (trusted only
  when the next header follows), RFC 2250's header dropped for payload type
  14, MP4A-LATM in RTP (RFC 3016, config in band or from `config=`, fragments
  joined on the marker bit), RFC 3640 AU headers, L16/L24/L8 and G.711.
  `aac`: AudioSpecificConfig (explicit and backward-compatible SBR/PS
  signalling), StreamMuxConfig, ADTS headers. `decode`: Symphonia 0.6
  (MPL-2.0) for MPEG audio I–III and AAC-LC, everything to stereo. `resample`:
  32-tap windowed sinc, ratio trimmable ±1 %. `player`: its own thread, a
  ring buffer to cpal 0.18 (WASAPI/ALSA), 400 ms start level held by trimming
  the ratio against clock drift, app-wide volume (cubed) and mute, per-player
  pause that resumes live, peak meter. `record`: as broadcast — `.mp2/.mp3`,
  ADTS `.aac` for any AAC carriage, `.wav`, `.ts`.
- **HE-AAC** plays its AAC-LC core (Symphonia has no SBR): the decoder is
  given an LC config for the core and ignores the SBR fill elements. Full
  sound: "… → Open in VLC", or the recording.
- **Engine**: `audio_play` + `audio_external` choose the in-app player or the
  external relay; `audio_record` records to the output folder. Under test the
  player uses a null output, so `cargo test` never makes a sound.
- **GUI**: per stream ▶ Play / ⏸ Pause / ⏹ Stop / ⏺ Record, level meter, a
  "…" menu for VLC/PotPlayer; volume slider and mute on the list header,
  saved in prefs. `--play-audio` plays the selected VFO's first stream in
  unattended runs. Streams are listed by group address and port.
- The synthetic scene's test radio is silent and slower than real time, so
  there it reads "buffering · dropouts"; a real stream does not.
- CI installs `libasound2-dev` for cpal on Linux.

**Also (2026-10-08):** a VFO dropped on a detected carrier is now sized to its
occupied width plus about 12 % (was 25 %, or 1.5 Rs), and stops short of the
nearest neighbouring carrier (`Carrier::vfo_bandwidth_among`), never narrower
than the carrier itself: close carriers no longer end up inside each other's VFOs.

## M3, part 2 — the S2X physical layer for normal and short frames (done, 2026-10-08)

With Rory's OK, EN 302 307-2 V1.3.1 (the ETSI PDF) and GNU Radio's `gr-dtv`
(sparse checkout) are in `reference/`. Tables were read out of the PDF with
PyMuPDF's table finder and turned into Rust by scripts, not retyped; gr-dtv
was the cross-check.

- **PLS code, 8 bits** (§5.5.2): the S2X generator row 0x90AC2DDD (Figure 20)
  on top of S2's (64,7) code — distance 32 among S2 codes, 24 across all 256.
  An S2X header's 64 PLS symbols are turned +90° against the SOF; the
  coherent reads try both ways (S2 codewords plainly, S2X ones turned back),
  the differential read takes the first bit both ways. The frame-sync
  correlator needed nothing: it only uses differentials within symbol pairs.
- **`PlsInfo`** holds the 8-bit code, the FECFRAME and `modcod()`; S2X codes
  map to Table 17a MODCODs, VL-SNR frames (129, 131: the lengths of normal
  QPSK/16APSK with pilots, so lock holds), and the Table 17b reserved codes
  with their stated lengths. 128APSK takes 103 slots.
- **`decdvb_core::modcod`**: one numbering for S2 (1–28) and S2X (PLS code,
  132–248); `Display` gives the canonical names ("16APSK 1/2-L").
- **Constellations**: 2+4+2 8APSK, 8+8 16APSK (by γ, and the 18/30, 20/30
  point tables), 4+12 16APSK with S2X ratios, 4+12+16rb and 4+8+4+16 32APSK,
  16+16+16+16, 8+16+20+20 and 4+12+20+28 64APSK, 128APSK, 256APSK on rings
  and the 20/30, 22/30 point tables. Tests: unit power, ring populations
  match each name, every table row's angles are φ, −φ, π−φ, π+φ, labels
  agree with gr-dtv where checked. Table 15d's 256APSK has pairs of points
  0.0001 apart — in the standard and in gr-dtv; not an error.
- **Interleavers** (Tables 9a/9b) per MODCOD; `demap::Mapper` bundles
  constellation, interleaver and 128APSK's padding (6 zero bits, 12 all-ones
  symbols).
- **Modulator**: `FrameSpec::s2x(pls, pilots)`; reserved codes get a random
  payload of the right length. **Test scene**: carrier B is S2/S2X ACM.
- **Tests**: every S2X MODCOD maps and demaps noiselessly; an ACM carrier
  through ten S2X constellation families plus S2 QPSK and a reserved code
  decodes, at 30 dB, to exactly the BBFRAMEs sent. In the GUI the scene's S2X
  carrier decodes 100 %, and Identify calls it DVB-S2X with the MODCOD table.

**Still to do for S2X:** VL-SNR (header with Walsh–Hadamard codes, extra
pilots, pi/2-BPSK with spreading, shortening and puncturing, medium
FECFRAMEs with GF(2^15) BCH), the other PL scrambling sequences of Table 19e
in acquisition, and superframing (Annex E; DESIGN puts it in M5).

## M3, part 3 — S2X VL-SNR (done, 2026-10-08)

- **Medium FECFRAMEs**: BCH over GF(2^15) (g1 = 0x802D; the built generator
  equals the product of Table 7's g1…g12), with messages that are not whole
  bytes (K = 5660, 7740, 10 620: 180 parity bits); the medium LDPC tables
  were already in from part 1.
- **VL-SNR header** (`decdvb-frame::vlsnr`, §5.5.2.5): 16 base rows of 56
  bits signed by Walsh–Hadamard rows — generated from the standard's text,
  and all 16 resulting sequences equal gr-dtv's `ph_vlsnr_seq`. Decoded by
  correlating row by row (896 bits: decodes at −3 dB in a test). The frame
  layout of Figures 17/18 (extra pilot blocks of 34/36 and 32/36 symbols
  mid-group), cross-checked against gr-dtv's pilot insertion.
- **VL-SNR codes** (`decdvb-fec::vlsnr`, Tables 18a, 19a–19d): QPSK 2/9 and
  pi/2-BPSK 1/5, 11/45, 1/3 (medium), 1/5 and 11/45 with spreading factor 2,
  1/5, 4/15, 1/3 (short; "1/5" is S2's short 1/4 code). Shortening (Xs
  zeros, certain at the decoder) and puncturing (every P-th parity bit until
  Xp, erased), as gr-dtv's encoder does.
- **Demod**: anchors the carrier on the extra pilots too; after the frame,
  puts the VL-SNR header back as sent (it is not scrambled), reads it, and
  for pi/2-BPSK takes off the rest of the ±1 scrambling (§5.5.4.1).
- **FEC**: data symbols by layout, LLRs (2-PAM, spread pairs summed; or
  QPSK), shortened and punctured bits restored, LDPC, BCH, BBFRAME (a ragged
  K's spare bits cleared).
- **Modulator**: `FrameSpec::vlsnr(header)` builds them, dummy included.
- **Tests**: every VL-SNR MODCOD and the dummy, between S2 frames, decode at
  8 dB to the BBFRAMEs sent. A sweep (`vlsnr_snr_sweep`, ignored) decodes
  them down to 0 dB; below about −2 dB the PLHEADER is not acquired at all —
  VL-SNR's own territory (−10 dB) needs acquisition built for it (longer
  correlation, header-aided). One seed lost three of six frames at +2 dB
  while 0 dB was clean: worth a look when a real VL-SNR signal turns up.

**S2X left**: superframing (Annex E, M5 in DESIGN), and searching the
preferred PL scrambling sequences of Table 19e when the default does not
lock.

## Demodulator: APSK within 1 dB of the ideal, scrambling found (2026-10-08)

Measuring S2X against EN 302 307-2 Table 20a's ideal Es/N0 showed the FEC
chain itself within 0.5–1 dB on plain AWGN (`awgn_modcod_sweep`, and every
LDPC code's waterfall sits in rate order — `ldpc_waterfalls`), but the full
receiver losing 3 dB and more on 4+12 16APSK and 4+12+20+28 64APSK — and S2's
own 16APSK 2/3 losing two frames in eleven at 12 dB. Whole frames failed, in
runs: carrier slips. Three changes to the carrier loop:

- **Decision error weighted by amplitude** (`Im(y·d*)`, not `arg`): an inner
  ring's angle is several times noisier than the outer's.
- **Slip repair**: each pilot block's anchor reports how far the loop had
  drifted; beyond a quarter of the constellation's finest rotational step,
  the data since the previous known block are re-derotated by interpolating
  between the two blocks' phases, from the raw symbols. On constellations
  denser than QPSK the loop's frequency is reset to the blocks' too (not on
  QPSK: at −1 dB that lost every 13/45 frame).
- **Bandwidth by density**: the decision-directed loop's bandwidth is set for
  the SNR less 20·log10(densest ring / 4) — a 12-point ring decides angles
  three times closer than QPSK.

Now (`s2x_threshold_sweep`, ignored): 8PSK 25/36, 16APSK 26/45, 32APSK 32/45,
64APSK 11/15 and 256APSK 3/4 decode every frame at their ideal + 1 dB; QPSK
13/45 at + 2 dB (below that, PL sync). The GUI scene is unchanged (100 %).

**PL scrambling (Table 19e)**: a frame's pilots descrambled with the right
sequence are one repeated symbol, so differentials across each pilot block
add up coherently (~1) and with a wrong one do not (~0.05) — no carrier lock
needed. When the given gold code does not fit, the demodulator tries the
seven preferred ones (0 and k·10 949) and switches; the side panel shows
"using … (found)".

## RCV-20x modes, part 1 — DVB-S (done, 2026-10-09)

Rory sent the CTCOM RCV-20x manual ("add this as a mod"): its decoder modes,
after S2X, with TPC 2964 (IESS-315) the most important. IESS-315 has to be
fetched first (asked for, not downloaded); DVB-S, also on the RCV-20x's
list, needs nothing new and came first.

- **`decdvb-modem`** (new crate). `conv`: K = 7, G1 = 171₈, G2 = 133₈,
  puncturing for 1/2 … 7/8 (EN 300 421 Table 2), streaming soft Viterbi with
  block traceback — conventions checked against gr-dtv's DVB-T inner coder.
  `rs`: RS over GF(256) (0x11D, roots α⁰…), any shortened (n, k): DVB-S's
  204/188 and the Intelsat sizes the RCV-20x lists. `interleave`: Forney
  I = 12, M = 17. `dvbs`: transmitter, and a blind receiver — every
  (rate, puncturing phase, 90° turn, inversion) is decoded and re-encoded
  over 3000 symbols, each scored against the median of its own rate (a 7/8
  code re-encodes noise almost as well as a right 1/2 does a noisy signal),
  then packet sync on 0x47/0xB8, deinterleaving, RS, energy dispersal (PRBS
  starts 03 F6 08, as it must).
- **Engine**: the FEC thread takes DVB-S symbol blocks as well as DVB-S2
  frames; the TS stage takes whole packets, so DVB-S gets the same outputs,
  analyser, MPE/IP and multicast audio.
- **GUI**: decoder "DVB-S → MPEG-TS" (`--decoder dvbs`), a DVB-S card (code
  rate found, channel BER, RS), the TS card.
- **Scene**: carrier C is now DVB-S, QPSK 3/4. A transmitter's first ~1100
  bytes are its interleaver's zero fill, which made Identify read 6 kS/s:
  the scene and test start mid-stream, as a real carrier is.
- Tests: RS to t errors for five sizes, every rate round trip, every
  rotation/inversion/start, rate 1/2 at 3.5 dB, and a DVB-S VFO end to end.

## RCV-20x modes, part 2 — turbo product code machinery (2026-10-09)

`decdvb-modem::tpc`: extended Hamming codes (any m, shortened), their
two-dimensional products, and the iterative Chase–Pyndiah soft-in/soft-out
decoder (16 test patterns, Pyndiah's α/β schedule). On the shape of IESS-315's
`tpc_2964` — (64,57) × (46,39), 2223 data bits in 2944, rate 0.755 — a
frame with ~100 raw errors (Eb/N0 3.5 dB) decodes in 9 or more of 10.

**Not done, and why:** what makes it *tpc_2964* rather than a product code
of that shape — where the 20-bit unique word F50B8h sits, the bit order of
the 46 × 64 block, the Hamming polynomial (x⁶ + x + 1 is assumed only for
the tests), the (2,3,9,12) descrambler with preset 475h and where it runs
— is in IESS-315 (or could be found from a real carrier with the parity
checks as the judge). Waiting on Rory for the spec or a capture.

## RCV-20x modes, part 3 — QAM in the generic decoder (2026-10-09)

8QAM, 16QAM and 64QAM join BPSK…32APSK in "Generic PSK → symbols", labelled
as the RCV-20x manual's Figures 3.2/3.3 label its hard decisions (16QAM is
natural binary per axis there; 64QAM Gray). Identify reads rings, not
grids, so a QAM carrier is named in the VFO's modulation setting.

Decision-directed carrier recovery false-locked 16QAM 27° off (MER 11 dB):
the generic demodulator now normalises symbol power itself (the AGC levels
samples, not symbols), acquires multi-ring constellations on the outer ring
alone (the reduced-constellation algorithm), and takes full decisions only
above 0.7 coherence (back below 0.4); a false lock still scores ~0.45, so
multi-ring "locked" needs 0.6. 16QAM: MER 34 dB on a clean carrier.

Left out: OS8QAM, the RCV's 8APSK and 32QAM (ring ratios / geometry not in
the figures), OQPSK and pi/4-DQPSK (they need demodulator changes).

## RCV-20x modes, part 4 — TPC 2964 (2026-10-09)

Rory: "download IESS-315 and do TPC 2964". IESS-315 (rev. E, 2005) turned
out to be a performance document — EIRP, link budgets, a spectrum mask, a
data scrambler given only as figures — that leaves the turbo code to
"compatible turbo modems", and it carries an Intelsat confidentiality notice:
nothing from it is in the repo, and nothing was needed from it. What is
used comes from the RCV-20x manual (Table 3.2) and the signal.

- **`decdvb-modem::tpc2964`**: UW F50B8h every 2964 bits found under each
  phase ambiguity (2 for BPSK, 8 for QPSK), three frames running. The frame
  structure is searched: send order (rows or columns), either direction in
  each, the Hamming generator (six primitive polynomials), the code bits
  unscrambled or scrambled by the (2,3,9,12)/475h sequence wired four ways
  and started at the block or the UW — 432 hypotheses. Each is scored by
  min(rows, columns) that are codewords as received; a column-major block is
  the transpose of a row-major one, so only the 46 × (64,57) orientation is
  kept (the first version counted each layout twice and could not choose).
  Near-equivalents (a reversed Hamming word is a word of the reciprocal
  polynomial's code, so some fit a quarter as well) are settled by soft
  decoding. Slips: when the UW is missing where due, the other orientations
  and ±2 symbols are tried there and taken if this UW and the next agree.
  Frames that missed their UW and do not decode are dropped.
- **`decdvb-modem::payload`**: HDLC deframing (flags, zero-bit stuffing,
  FCS-16/32), TS alignment at any bit offset (three syncs to lock), and the
  descrambler search: none, self-synchronising (2,3,9,12), its reciprocal,
  V.35 (3,20), V.29 (18,23), and additive (2,3,9,12) per frame four ways.
  Random data never passes (tested).
- **Engine/GUI**: decoder "TPC 2964 (IESS-315) → IP / TS" (`--decoder tpc`),
  BPSK/QPSK (or Identify's), a TPC card (UW, orientation, structure and fit,
  decoding, payload, data file); IP from HDLC goes to the IP stage (PCAP,
  flows, multicast audio), TS to the TS stage; Record writes the data `.bin`
  as well. Identify names the carrier from its UW.
- **Scene**: carrier D, TPC 2964 QPSK at 125 kBd, +250 kHz, a multicast radio
  and unicast traffic over Cisco HDLC, self-synchronising (2,3,9,12)
  scrambling. Found, decoded and the radio listed in the GUI.
- **Not covered**: 8PSK and 16QAM tpc_2964 (labels and soft demapping still to
  do), OQPSK (IESS-315 allows it; needs the demodulator), a scrambler running
  across frames, the V.35 run counter. A real TPC 2964 recording would
  confirm which hypotheses are real; until then it is checked on synthetic
  signals only.
- Also this session: **v0.1.0 released** — `release.yml` (DecDRM's pattern)
  builds the static-CRT exes on a tag, checks versions and the C runtime,
  smoke-tests the CLI and drafts the release; `decdvb-gui --version`.

## Proprietary-waveform fingerprints, live text (2026-10-09)

Rory: "look for modes like NovelSat NS4 and Q-Flex FastLink", and "in the
generic, something for looking for live strings". Also **v0.2.0 released**
(TPC 2964).

What is public (vendor sheets, manuals, a NovelSat patent; summarised from
reading, nothing kept in the repo): NS3/NS4 use BCH + LDPC with 64800/16200
frames, extra code rates (13/30, 7/15, 8/15, 17/30, 19/30), roll-offs down
to 2 % (NS4), a "Golden-Sequence" setting 0–262141 (the S2 Gold-code range)
and scrambling "reset at the start of a frame" sparing the header — S2-style
framing is likely but the PLS meaning is not published. Paradise FastLink
(Q-Flex) publishes rates and roll-offs only; its "DVB-S2X low-latency"
extension adds 5400- and 3240-bit frames. Comtech VersaFEC-2: 9600/1600-symbol
blocks. None publishes a sync word.

- **Identify**: `Dvbs2Info::unconfirmed_spacing` — S2 headers recurring on a
  grid their PLS codes do not explain → "DVB-S2-style PLHEADERs every N
  symbols … a proprietary DVB-S2 extension (NovelSat NS3/NS4, or a vendor's
  short frames)". `roll_off_2pct` — fits 2 % better than 5 % → "sharper than
  DVB-S2X allows". The roll-off fit held its floor at −300 dB when the fitted
  floor went negative, which made a 2 % carrier read 35 %; now it is held at
  the measured floor.
- **`period.rs`**: frame-structure finder — autocorrelation of unit-magnitude
  locked symbols by FFT; the first peak at least half the strongest, ≥ 6×
  the median; with ≥ 8 frames, frames folded (pass 1 straight, pass 2 each
  frame turned onto the candidate positions) to find the runs of repeating
  symbols. A period with nothing repeating in the fold is the data's own
  regularity (unscrambled text) and is not reported. On the scene's DVB-S
  carrier it finds the synthetic TS's 40-packet repeat.
- **Live text** (`decdvb-modem::text`, `psk::TextSearch`): generic PSK VFOs
  read the decided bits 32 ways per carrier orientation (plain/differential
  × 8 alignments × 2 bit orders), keep runs of ≥ 6 printable characters, and
  call the reading with ≥ 4× the median's text the one; strings ≥ 10 shown
  live, the longest from any reading as candidates. 3.6 Msym/s for QPSK.
- **Spectral lines**: unscrambled data puts lines on its carrier, which made
  the ENBW (and so the symbol rate) collapse and the carrier read as a lump.
  `estimate::flatten_lines` takes up to 8 narrow lines standing on a plateau
  out of the level and flatness measures (not a CW on the floor, not a comb,
  not lines holding most of the power), and the ENBW reference is now also
  bounded by the median over the middle half of the occupied band.
- Not possible without captures: naming FastLink/VersaFEC; confirming the NS3/NS4
  header layout. HackRF tops out at 20 MS/s, below most NS4 hypermuxes.

## Text in every decoder (2026-10-09)

Rory: "add the strings decoding to all decoders". **v0.3.0 released** before
it (live text in the generic decoder, the proprietary-waveform cues).

- `decdvb-modem::text::ByteText`: runs of ≥ 6 printable bytes, counted; the
  strings seen more than once are kept in their own small table, so ranking
  costs nothing and the view is built on every frame (a cached view went
  stale as soon as data stopped — the test saw 140 of 17 000 bytes); up to
  65 536 distinct strings remembered, single sightings dropped oldest first;
  strings ≥ 16 kept as they come. `push_keyed` carries a run on per stream.
- TS stage: payloads after the header and adaptation field, per PID (a
  table's strings span packets). IP stage: UDP/TCP payload (IP header and
  L4 header skipped). TPC 2964 with an unknown payload: `TextFinder` over the
  data bits (32 readings). Identify's live view: `TextSearch` as the generic
  decoder (its "look for text" setting now shows for Identify too).
- GUI: "Text in the transport stream" / "Text in the IP packets" (recurring
  with counts, latest long strings); the bit-level card for TPC-unknown and
  Identify.
- Tests: the DVB-S VFO finds "DecDVB test signal" recurring in its TS; the
  MPE carrier shows the SAP announcement's SDP lines in its IP text.

## E1 voice in a modem's data (2026-10-09)

Rory carries an E1 with G.711 airband voice over a Comtech CDM-600 link.
`decdvb-modem::e1`: G.704 frame alignment at any bit offset (FAS x0011011
every 512 bits with the NFAS bit between, 8 in a row to align, 3 bad to lose
— G.706), timeslots out, CAS seen in TS16, G.711 A-law both ways. The payload
search tries E1 beside HDLC and TS under each descrambler. The FEC thread's
E1 stage meters every timeslot (dBFS over half a second: idle ≈ −70, voice
in between, data ≈ −5) and plays one / records one to `.wav` by handing
20 ms RTP PCMA packets to the multicast-audio player and recorder. GUI: an
E1 card with a level bar, ▶ and ● per timeslot. Test: a scrambled E1 in TPC
2964 with a tone in TS 7 — found, metered, played, recorded. Drop-and-insert
(IBS-framed n×64k) is not handled yet: waiting on what the CDM-600 sends.

## First live Comtech carriers: CDM-600L TPC 2964 and D&I++ voice (2026-10-09)

Rory recorded his own carriers on Horizons 3e (169°E): a CDM-600L and a
Q-Flex.

- **CDM-600L** (43.6 kS/s QPSK): Identify named it from the TPC 2964 UW, and
  the TPC decoder took it: UW every 2964 bits, structure row by row,
  x⁶ + x + 1, code bits unscrambled (the search's own finding, fit 10 % at a
  raw BER of 0.6 %), 426 of 443 frames decoded.
- The payload: data rate 65 423 bit/s = 64 000 × 46/45 — Comtech **D&I++**
  (CDM-625 manual: 2944-bit frames, 64 overhead + 2880 data). Under the
  V.35 descrambler (taps 3, 20) the data fold at 2944 bits into a 24-bit
  header `000001010111101000111000` and four 10-bit overheads
  (`0111111111` ×3, the last with a varying bit) between five 576-bit blocks;
  the data are 360 bytes (45 ms) of the timeslot a frame. (First read as
  open-squelch hiss and hum; wrong — see the next section.)
  `decdvb-modem::dandi` deframes it; the payload search tries it; the FEC
  thread works out the timeslots from the symbol rate (n × 64k × 46/45) and
  feeds them to the E1 voice stage; the GUI's Voice card shows "ch 1..n".
- **False MPEG-TS**: three sync bytes 188 apart turn up by chance in a long
  probe, so the CDM-600L read "MPEG-TS" (5–10 packets, random PIDs). TS, E1
  and D&I++ must now account for at least half of the probed bits.
- `decdvb decode <capture> --decoder … [--out d --e1-record n]`.
- **Q-Flex** capture (95 kS/s QPSK): a structure repeating every 23 076
  symbols (~10 %), no header DecDVB knows — FastLink or Paradise TPC; open.
- With several D&I++ timeslots the byte order (alternating) is assumed, not
  confirmed.

## The CDM-600L's timeslot is not G.711 (2026-10-09)

Rory: the Comtech voice sounds like digital noise. Right — the D&I++
deframing is sound (header and overheads in place, frames every 2944 bits),
but the timeslot does not hold G.711:

- Each octet's bits 1 and 4–8 (G.704 numbering, bit 1 first) repeat a fixed
  pattern every millisecond — bit 1 reads `01100000` over eight octets, and
  bits 4–8 read serially are `0x60` repeated — through the whole capture.
- Only bits 2 and 3 change: two 8 kbit/s streams (I.460-style sub-rate).
  During quiet stretches each repeats an identical 80-bit (10 ms) pattern —
  what a speech codec makes of digital silence; otherwise they look random.
  The bit-2 stream also carries a framing bit every 5th bit (0, with a 1
  every 160 bits = 20 ms); the bit-3 stream has no fixed bits at all.
- Not G.729 in its ITU bit order: the pitch-parity bit (P0 over P1's six
  MSBs) fits at no offset, nor does any 6-bit parity elsewhere in the frame.
  The codec, and the equipment feeding the E1, are unknown.
- So the A-law levels (−9 dBFS, "loud") and the noise heard were the
  idle-pattern bits. The E1 stage now judges each timeslot by which bits
  change (compared with the same bit a millisecond earlier): sign bit
  moving → G.711; nothing moving → steady (idle or a tone); sign bit frozen
  while others move → **not G.711, bits …** (Voice card, `decdvb decode`).

## Q-Flex FastLink, blind (2026-10-09)

Rory sent his Q-Flex's settings (FastLink, QPSK, 0.710, 128.5 kbit/s,
closed network + ESC, Drop-Insert TS1, 95 017 sym/s) and a 6.9-minute
capture. Demodulated losslessly (file playback now waits for slow VFOs and
FEC threads instead of dropping when not real time — `decdvb decode --fast`
keeps every symbol, 18× real time).

- Frame: 18-symbol sync word every 11 538 symbols (3527 found, slip-free),
  then 23 040 bits = four 5760-bit slots.
- GF(2) rank of thousands of slots (and their differences): 4110 / 4109 once
  the ~8 % with bit errors are weeded out by checks from the other half —
  a **(5760, 4096)-ish LDPC code** with a fixed scrambling offset on the code
  bits; 13 positions per slot lie in no check (outside the code), 2 are
  always 0. 4 × 4096 / 23 076 = 0.7100, the menu's rate.
- Sparse checks by random-permutation RREF of the dual: 1251 of weight 16,
  ~180 heavier (24–28): an irregular LDPC; bit degrees spread evenly along
  the slot, so the coded bits are interleaved.
- **Identify** names the framing (`decdvb-modem::fastlink`: sync word under
  any QPSK orientation, three frames running). Reading the data still needs
  the data positions, their order and the data scrambler; the recovered
  checks are kept out of the repo for now.
- Refined: the checks of one 2880-bit half of a 5760-bit slot map onto the
  other's (shift by 2880: 269 of 300) — the code is **(2880, 2048)**, eight
  codewords a frame (2048/2880 = 0.711). Its order sent shows no
  block-cyclic structure, nor after undoing any row–column interleaver of
  2880; bit degrees vary with position mod 16. Next: the interleaver, the
  data positions and the data scrambler — or a capture with the Q-Flex
  sending a test pattern, which would give them directly.

## FastLink decoded: the code, the layout and the scrambler (2026-10-09)

Blind, from Rory's 6.9-minute Q-Flex capture (FastLink QPSK 0.710):

- One codeword at a time this time (2880 bits, slot-wise differences with
  bad words weeded out): rank exactly 2048, a dual of 832, every position
  covered — the "13 positions in no check" were bit errors.
- Random information-set reductions of that dual, a single codeword's,
  converge in seconds: 640 checks of weight 10, all independent, pairwise
  overlaps ≤ 2, in 10 classes of exactly 64; then 192 more of weight 18
  outside their span. Bit degrees: 832 of 2, 2048 of 4 — an **irregular
  repeat–accumulate LDPC**. The degree-2 bits chain all 832 checks into one
  ring (an accumulator closing on a bit that is always 0); each data bit's
  four checks sit at `x + 13·t` on the ring (13 = 832 / 64): 32 circulants
  of 64, a DVB-S2-style address table (EN 302 307-1 §5.3.2).
- Layout as sent: 416 units of (2 parity, 4 data) bits, then 384 data bits;
  data rows of 32 (one bit per circulant), row `r` holding circulant row
  `37·r mod 64`. A 32 × 4 table plus these rules rebuilds the measured
  832 × 2880 matrix exactly. Every check has odd parity as received.
- The data's fixed offset per frame position, read in the order sent, has
  linear complexity 32 across all eight codewords: a frame-synchronous
  scrambler `s[i] = s[i−2] ⊕ s[i−16] ⊕ s[i−18] ⊕ s[i−30] ⊕ s[i−32]` from
  `0xAAA2A2A6` (two interleaved `x¹⁵ + x⁸ + 1` sequences, the even one
  inverted). The data bits are therefore sent in their own order.
- `decdvb-modem::fastlink` decodes it (layered normalised min-sum, odd
  checks, slips of the timing or the carrier phase repaired), and the
  **Q-Flex FastLink** VFO decoder / `decdvb decode --decoder fastlink` hands
  the descrambled data to the payload search, text finder and data file.
  Rory's capture: 783 of 784 codewords decode, channel BER 1.6 × 10⁻⁵.
- What the data carry is next. They hold idle patterns much like the
  CDM-600L timeslot's (1/3 ones, period-8 runs drifting slowly — the
  128.5 kbit/s data rate is 257/256 of 128 kbit/s, so an overhead bit every
  257 is likely); no HDLC.

## Inside FastLink: Paradise closed network + ESC (2026-10-09)

The FastLink data rate, 134 925 bit/s, is exactly 21/20 of Rory's
128.5 kbit/s: an overhead octet after every 20 data octets.

- Folded at 672 bits (four overhead octets) the overhead shows: a frame
  alignment word `1x011000` (98h, one bit ESC), an ESC octet, a control
  octet `0xm1xxxx` (`m` a 64-group multiframe pattern, `x` ESC) and another
  ESC octet. 22 ESC bits a group, 0 when idle, in bursts of 38–59 groups
  (short packets; ~4.4 kbit/s against the menu's 4800 baud). No HDLC in
  them under any common descrambler or bit order — unread for now.
- Aligned 99.97 % of 7 minutes (the misses all where the LDPC slipped), and
  found unaided on Rory's second Q-Flex capture: `decdvb-modem::paradise`
  is a payload-search format ("Paradise closed network + ESC framing").
- The 128.5 kbit/s inside: 2 ms frames of 257 bits — one bit of a
  ~500 bit/s side channel (random-looking), then sixteen 16-bit words at
  8 kHz: two 64 kbit/s timeslots. In silence the words repeat every 4 ms
  with a few bits moving; in traffic they fill up. Which bits are which
  timeslot's octet is not settled: neither a byte nor a bit split gives the
  CDM-600L timeslot's 1 ms idle cycle (this mux idles on a 4 ms cycle).

## The Q-Flex's 128 kbit/s: sixteen 8 kbit/s sub-channels (2026-10-09)

Inside the Paradise framing the 128.5 kbit/s are 2 ms frames of 257 bits
(one side-channel bit, then 256), and the 256 are sixteen 16-bit words at
8 kHz. Read bit by bit position, each word is sixteen 8 kbit/s sub-channels
(I.460-style), not two octets:

- Bits 0, 1, 2 and 15 repeat a 160-bit (20 ms) frame in silence — codec
  channels, as on the CDM-600L timeslot (whose 8 kbit/s codec channels idled
  on 80 bits, one with a 0 every 5th bit and a 1 every 160).
- Bits 3–11 and 14 cycle on 4 ms idle patterns; 12 and 13 are always 0.
- Over the 7 minutes no codec channel ever goes active (no speech: squelch
  shut). The only traffic is a burst every 5 s (activity autocorrelation
  peaks at 10 × 0.5 s on bits 3–14), mostly in bits 8–13: no HDLC in it,
  in any grouping or polarity — the voice multiplexer's own status or
  keep-alive, presumably.
- So nothing to hear in this capture, and the codec (8 kbit/s channels,
  20 ms frames) is still unknown: not G.729 in its ITU bit order (tested on
  the CDM-600L's active channels). A capture during a transmission, and the
  multiplexer's make, would be the way on.

## Narrow SCPC carriers: PLL bandwidth, Viterbi decoder (2026-10-09)

Rory's screenshots of five ~10 kBd carriers near 12.3337 GHz showed a ring
for a constellation while the demodulator said "locked, MER 8.8 dB". A ring
scores ~7 dB against QPSK decisions, so it never locked: at Bn·T 0.008 the
carrier loop is ~80 Hz wide at 10 kBd, too narrow for the LNB's phase noise.
A decision-directed loop at 0.02–0.04 gives 15 dB on the same capture. The
PSK demodulator now sets Bn·T = clamp(250 Hz / Rs, 0.008, 0.04) — unchanged
from ~31 kBd up — and the capture demodulates at MER 12.4 dB.

- The carrier: QPSK 10 240 Bd. Not plain QPSK-with-noise but proper QPSK
  (symbol-to-symbol phase steps ¼ 0, ¼ ±π/2, ⅛ each ±π). A window-rank test
  (rows of 40 symbol pairs: rank = 40 + code memory) found memory 6, and the
  null space gave the taps of 133/171 written backwards — the K = 7 code of
  DVB-S and IESS-308/309 (an earlier syndrome check of mine had the window
  reversed). The DVB-S receiver did find rate 1/2 but restarted for want of
  TS sync bytes: hence `ViterbiRx` (dvbs.rs) — the same blind search, no
  transport layer — and the **Viterbi K=7** VFO decoder / `--decoder viterbi`.
- On the capture: rate 1/2, mirrored, channel BER 7 × 10⁻⁴. Under V.35
  (taps 3, 20) the data are 0x55 fill in 16-byte blocks (one varying byte,
  cycling ff e0 00 5c / 00 1f ff a3, then 15 × 55h — 10 240 = 9600 × 16/15,
  EDMAC-like) between half-second bursts of random-looking data. The payload
  search now names a scrambler from idle fill alone (`Format::Idle`: bits
  repeating 1, 2 or 8 back ≥ 75 %, once the framing formats had 8 probes).
- A VFO publishes its status every 250 ms with no input as well: on a short
  file played fast the last publish came before the FEC thread caught up.
- The 128 kBd QPSK carrier next to them (Rory's second capture, MER 9.3 dB):
  no K = 7 rate, no TPC 2964 UW, no repeating unique word in 8 s, not an
  uncoded scrambled stream. Open; a long code (IESS-308 sequential K = 36)
  is not ruled out at that error rate.

## DVB-CID (2026-10-09)

Rory asked for "DVB-CI": the carrier ID (ETSI TS 103 129 V1.1.1, fetched
with his go-ahead, cache deleted), not the common interface.

- `decdvb-modem::cid` follows §4–5: the 244-bit frame (UW 147147h,
  complemented every other frame; two halves of identifier, content ID,
  24 information bits, CRC-8, BCH(111, 69)), the x⁹ + x⁵ + 1 scrambler, four
  copies, differential encoding, the 4096-chip x¹⁵ + x¹⁴ + 1 sequence,
  112/224 kchip/s, +220 Hz. Checked against the document's own examples:
  the first 32 chips (5091E364h), the check digits of 00:06:B0:FF:FF:01:AC:07
  (75h), the position and telephone codings. The BCH generator is the
  product of table 4's six polynomials; with GF(2⁷) from x⁷ + x³ + 1 its
  roots include β¹…β¹² for β = α²³, so Berlekamp–Massey corrects 6.
- The scrambler figure can be read two ways (register x⁹…x¹ written left to
  right or right to left); the receiver tries both, keeps the one that
  passes the CRC and says so.
- Receiver: VFO baseband → DDC onto the host's centre (Identify) → cubic
  resampling to 4 samples a chip → FFT search over code phase × frequency
  (±1.7 kHz, half-bin steps, 24 bits non-coherently) → per-bit prompt,
  early and late correlators with an FLL on the squared differential →
  DBPSK → UW four times 244 apart → the copies summed → descramble, BCH,
  CRC → identifier, MAC, position, telephone, text (§4.2 table 1).
- Tested on synthetic signals: a full frame decoded from mid-stream at a
  chip SNR of −26 dB; through a VFO, a CID 27.5 dB under a 224 kBd QPSK
  host (Es/N0 15 dB) found within 10 Hz of +220 Hz. None of Rory's
  recordings (CDM-600L, Q-Flex ×2, three others) carries a CID: no code
  correlation peak in any of them.

## FFT size without a restart (2026-10-09)

The waterfall's FFT size used to need **⟳ Apply** on a file and a restart
of the radio. It is now an engine command (`Engine::set_fft_size`, like DC
removal): the front-end thread swaps its spectrum and block length between
blocks, drops rows of the old width and starts the smoothed spectrum again;
the source, the HackRF and the VFOs carry on untouched.

## Station names from SAP, and an SDP that names the wrong payload (2026-10-09)

A live DVB-S2 radio multiplex announces each station by SAP (224.2.127.254)
with SDP lines ended by a bare CR; the parser now splits on CR or LF, so the
`s=` names show (and bare SDP sent to any port counts as an announcement;
a stream takes the announcement for its group and port, else the only one
for its group, else the only one for its port). The same SDPs say
`m=audio … RTP/AVP 14` (MPEG audio) while the packets are payload type 99:
ADTS AAC behind an RTP header extension. Trusting the SDP made the player
strip RFC 2250's 4 bytes from every packet and look for MPEG audio, so the
named stations went silent. An announcement now describes a stream only for
the payload type the packets carry (`SdpInfo::for_pt`): otherwise it names
the stream and the codec comes from the bytes, and the external-player relay
serves the ADTS over HTTP rather than handing over the misleading SDP.

## DVB-CID: the code search on screen (2026-10-09)

`CidRx` keeps a map of every acquisition search (`CidStats::search`, an
`Arc<CidSearch>` so the stats copy cheaply): the correlation over all 4096
code phases at the strongest frequency (512 points, the largest of each 8
chips), and code phase ± 24 chips × the whole ±1.7 kHz searched (pooled to
at most 64 rows), all in dB over the mean of every cell, with the peak and
the lock threshold (4.8 dB). The Carrier ID card draws both under "Code
search": while searching it shows how close the best cell came; once locked,
the search that found the code (a single spike, and one bright cell near
+220 Hz). Checked on a simulated 1.008 MBd QPSK host with a CID 27.5 dB
under it: peak 6.6 dB, found at code phase 1299. The frequency reads from
Identify's estimate of the host's centre, here 310 Hz high, so the CID shows
at −80 Hz rather than +220.

## Test signals in tests/; a DVB-CID generator (2026-10-09)

`decdvb cid` writes a DVB-S2 QPSK 1/2 carrier (1.12 MBd, α 0.20, the test
TS) with a DVB-CID under it at 224 kchip/s, −27.5 dB, +220 Hz; 3.36 MS/s so
both are whole samples (3 a symbol, 15 a chip). The CID frames cycle the
fields set by the new `CidReport::set_position / set_telephone /
set_user_text` (inverses of the decoders; §4.2's example bits come out
exactly) two to a frame (`frame_fields`), the UW complemented on odd frames;
the capture opens a second before a frame so the receiver is locked when it
starts. 30 s holds one whole frame (identifier, position); all six take
108 s. `decdvb test-signals [dir]` writes the scene and the CID capture into
`tests/` (git keeps only its README). Checked with the CLI: the CID decodes
(identifier, MAC, position), Identify calls the host DVB-S2 QPSK 1/2 with
pilots, and its TS decodes. The CID's offset reads +523 Hz: Identify's
centre for the host is ~300 Hz low, as on the GUI test (−80 Hz there).

## DVB-CID live view; three receiver fixes (2026-10-09)

`CidStats::live` (`CidLive`) records the tracking a bit at a time: the
prompt correlations turned onto the real axis (the bits' squares averaged
give the phase) at unit power, the differential products, SNR and
frequency over 256 bits, early/prompt/late magnitudes averaged over ~10 bits,
the last 244 soft bits, and the frame sync (copies of the UW in a row, time
to the frame being whole). The Carrier ID card draws them as instrument
panels (painted, not egui_plot): two constellations with MER, the E·P·L
stems on the ideal correlation triangle with (E−L)/(E+L), two strip charts,
a soft-bit bar code and four "copy" boxes filling.

Building it showed the GUI's CID at 0.9 dB a bit where the CLI had 8.4 on
the same file. Fixed:
- Lost lock was "64 bits in a row with |P|² < 2·noise"; off the code one
  bit's power exceeds that one time in seven, so a slipped tracker followed
  noise for good. Now the smoothed SNR under 3 dB for 64 bits.
- Dropped samples (the VFO's queue, or the CID thread's own) are reported as
  gaps (`VfoHandle` counts the samples, `CidWorker::lost`, a `Gap` message)
  and `CidRx::gap` stands zeros in, so timing and phase carry on and only
  the bits in the gap are hurt.
- The frame search waited for a frame + 22 bits, so a frame ending near the
  end of a recording never decoded.
Tests: a reported 60 000-sample gap rides through (one search, the frame
decodes); unreported, the receiver searches again and relocks. In the GUI
the 30 s test capture now locks at 9.3 dB a bit and decodes its frame.

## The Q-Flex's 128.5 kbit/s is a 257-bit TDM multiplex (2026-10-09)

The alignment bit of each 257-bit frame (2 ms; found by its column making
the rest 16-periodic) is now read: a 70-frame pattern, 99.9 % stable — on
alternate frames the Barker-7 sequence reversed, 0100111, again and again
(a 14-frame, 28 ms cycle), and between them "01101" repeating (one bit every
4 ms, so a 20 ms cycle: the codec frames' marker). So the 128.5 kbit/s on the
modem's data port is a multiplexer's own TDM stream: 500 bit/s of framing
and 128 kbit/s of payload as sixteen 16-bit words at 8 kHz (two 64 kbit/s
timeslots' worth). Its make is still unknown.

What 7 minutes show, channel by channel (bit n of each word, 8 kbit/s):
- 0, 1, 2: idle codec — one 160-bit frame repeated (20 ms); laid out in
  octets each frame is five 4 ms sub-frames whose first octet is a
  per-channel constant (11000000, 10000100, 10010000), every octet starting
  with a 1.
- 3, 4, 5: a fixed 4 ms pattern.
- 6–15: varying, more for about 20 s in every 60 s. Bits 8–14 hold one
  7-bit value per 2 ms, changing at word 10 of every frame (P = 0.999), in
  one of eight states (66/0/1, 64/32/33, 16/96/97, 3/4/5, 48/72/73, 9/6/7,
  12/18/19, 36/24/25) that steps every 0.5–1.5 s: signalling, not audio.
  (Corrects the earlier notes: bits 12 and 13 are not always 0, and the
  activity is a 60 s cycle, not a 5 s burst.)
- No channel carries speech in this capture.

ESC: the bursts (0.2–0.3 s, on multiframe boundaries) are neither async
characters (stop bits fail as often as random) nor HDLC, plain or under
the usual descramblers: scrambled or encrypted binary, presumably the modems'
M&C.

`decdvb-modem::tdm257::TdmRx` finds the frame by the alignment word (every
phase × parity × rotation, ≥ 95 % of 56 word bits), checks it every other
frame (5 wrong of the last 14 → search again), and meters each channel over
0.5 s: bits changed since 20 ms and since 4 ms, and ones → fixed, 4 ms
pattern, idle codec, varying, or active (> 25 % changed at 20 ms). It runs
on Paradise data in the FEC thread (`FecStats::tdm`); the CLI prints the
channels, the FastLink card draws them as bars (speech should stand up near
half). On the 7-minute capture: aligned, 214 030 frames, 5 word-bit errors,
1 loss (at the FastLink slip). Next: a capture while someone talks, and the
multiplexer's make — then the codec.

## IESS-308/309 narrow carrier: differential decoding, IBS framing, 90° turns (2026-10-09)

Rory's chain for the 10.24 kBd carrier: PSK ← K=7 rate 1/2 ← differential
encoding ← IESS-308 scrambling (taps 3, 20) ← data + IBS/SMS framing. The
"0x55 fill in 16-byte blocks" found before was the missing differential
decoding: with it the data are an idle line's all ones, and the 0x55/0xAA
blocks (whole frames inverted four at a time) go away. Differential decoding
and a self-synchronising descrambler commute.
- IBS (`decdvb-modem::ibs`): 128-bit frames, one overhead octet then 120
  data bits (16/15), the overhead in a four-frame cycle — on this carrier
  00 20 00 E4, with 10h of the second toggling (a service bit). IESS-309
  was not to hand, so the overhead is learned, not decoded: the receiver
  takes the octet window whose bits each keep a four-frame cycle (all but
  two), changing most across the cycle, then most zeros (idle all-ones data
  repeat too); loses it at 10 of 16 frames misaligned. Scored on data bits
  only, so an E1 (whose alignment octets also make a 4-frame cycle at
  128-bit spacing) still wins as E1.
- The payload search tries every descrambler with and without differential
  decoding (plain wins ties); `PayloadStats::ibs`; CLI and GUI show the
  overhead cycle and service bits.
- The "data bursts" were not data: the carrier turns 90° for ~0.4 s, three
  times in 6 s; decoded under the turned orientation the bursts are the same
  idle IBS frames. `ViterbiRx` now checks the fit every 256 symbols (over
  the last 384) and, when it fails, follows the orientation that fits at
  least twice as well — rotation and mirroring only, the trellis not reset,
  so no bit is lost or added (`ViterbiStats::turns`). Test: a 90° turn for
  4000 symbols, followed twice, the bits still aligned at the end.
- On the 6 s capture: "IBS/SMS framing (IESS-309, 16/15), differential
  decoding, self-synchronising descrambler, taps 3, 20"; 3 turns followed,
  channel BER 0; IBS aligned, 381 frames, 1 loss (before the first turn was
  followed). The data are idle throughout.

## DVB-CID: the right centre, deeper searches, a frequency-loop false lock (2026-10-09)

Rory asked to lower the lock threshold: a live 1.048 MBd DVB-S2X carrier's
search peaked at 3.9 dB against 4.8. That is the noise: ~1 M cells of a
24-bit sum (Gamma(24) over its mean) top out near 3.8 dB, so a lower fixed
threshold would lock on noise. What was wrong:
- The search was centred on Identify's centre; that carrier's residual
  offset (from the carrier lock) was −8.6 kHz, so a CID 220 Hz from the true
  centre lay outside the ±1.7 kHz searched. The CID VFO now centres on
  Identify's centre plus the residual when the carrier locked, and searches
  ±4 kHz otherwise (`CidRx::with_span`).
- Searches deepen: 24 bits, then after three empty searches 48, then 96.
  The threshold is computed for the bits and cells (Chernoff bound on the
  largest of the cells, one false lock in 10⁴ searches): 4.9, 3.7, 2.7 dB
  (`CidSearch::bits` shows the depth).
- Moving the centre exposed a tracking bug: the frequency loop squared the
  bit-to-bit phase step, which also locks half a cycle a bit away (±27 Hz
  at 224 kchip/s): 3.9 dB lost and every bit inverted, so no frame (8.4 →
  4.7 dB on the test capture). It now resolves that ambiguity with the phase
  between the two halves of a bit (same data bit, unambiguous within ±1 bit
  rate) and tracks with the quiet squared detector. Test: locks true at
  +20, +97, +300, −150 Hz (+20 false-locked before).

## Generic PSK: symbol numbering in the .bin (2026-10-09)

`VfoSettings::symbol_labels` (`psk::SymbolLabels`): the byte written per
symbol is the standard label (as before: DVB-S2's mappings, the RCV-20x
manual's for its QAMs), the point's position (PSK: by angle from the first
point counter-clockwise of 0°; square QAM: x index then y index from the
lowest), or the Gray code of the position (per axis for QAM; neighbours
differ in one bit). APSK and 8QAM keep their labels. GUI: "Numbering" under
the PSK constellation; CLI: `decode --decoder psk --labels natural|gray`.
Checked on the 10 kBd QPSK capture: Gray is a fixed relabelling of the
standard labels (0→0, 1→2, 2→1, 3→3).

## Q-Flex: record on activity; DVB-CID: weak ones (2026-10-09)

Q-Flex: the second capture (QFLEX_2, 14 s) is the same — codec channels 0–2
idle, no speech — but its alignment channel's 20 ms word reads 11110 where
qflex_3's read 01101: it carries state, not a fixed marker. With no speech
on record, the way on is to catch some: the FastLink VFO has "Auto-record:
when a TDM channel goes active" (`VfoSettings::record_on_activity`). The FEC
thread keeps the last 10 s of data while not recording, starts a
`…-fastlink-active-….bin` with that pre-trigger when any channel goes
Active, and stops 10 s after the last activity (`FecStats::raw_triggered`
counts them).

DVB-CID: on Rory's live 16APSK host the strongest cell of a 96-bit search
(2.2 dB, under the 2.7 dB threshold) sat at −3418 Hz while Identify's
(unlocked, power-line) residual put the carrier at −3621 Hz — 203 Hz apart,
where a CID belongs; under a 1 % chance for noise in an 8.4 kHz span. So a
real CID ~10 dB under the specified level. For those:
- a 192-bit search level (threshold 1.9 dB);
- unlocked hosts searched wide enough to take in Identify's residual too;
- found at 96 bits or deeper, tracking is gentler (FLL gain 0.01, DLL 0.05)
  and keeps lock down to a 1 dB power/noise ratio (a leaky counter: below
  counts up, above counts down twice as fast);
- the acquisition's frequency is interpolated between half-bin cells;
- the unique word is looked for on the four copies summed (6 dB better; a
  chance match fails the CRC).
Test (ignored, `--release`): a CID 41 dB under the noise (~1.4 dB a bit)
is found by a deeper search, tracked, and its frame decoded.

## DVB-CID low-SNR mode (2026-10-09)

A "Low-SNR mode" button on the Carrier ID VFO (`VfoSettings::cid_low_snr`,
CLI `--low-snr`), taken up by the CID thread live: searches start at 96
bits and go to 384 (7 s at 224 kchip/s), with a false lock allowed once in
100 searches (thresholds 2.6, 1.7, 1.3 dB); tracking is the deep kind
throughout. Searches now run on up to 8 cores (each bit's spectrum once,
the cells split across threads). Deep tracking gained:
- a Costas loop after a 200-bit frequency pull-in (wide ~4 Hz for 300 bits,
  then ~1.5 Hz), so bits are detected coherently;
- the acquisition's code phase interpolated to a quarter chip, and the
  timing loop quick for 200 bits then slow;
- in low-SNR mode the frame's four copies combined coherently (each copy's
  sign from correlating whole copies) before the differential decoding,
  the summed-differential way as fallback.
Measured (simulation, CID at x dB under unit noise a sample, 4 per chip):
normal mode locks to −42..−43 dB; low-SNR mode to −46 dB (−45: normal
cannot, low can — test); frames decode to about −42 dB (~1 dB a bit) in
either. Tracking shows ~0.6–1.3 dB less than ideal: the 4-per-chip grid
(chip edges between samples) and timing dither — 8 per chip would halve it.

## DVB-CID at eight samples a chip (2026-10-09)

`cid::SPS` 4 → 8 (the CID thread resamples to 8 × the chip rate): a chip's
edge now falls within 1/16 chip of a sample. Early/late stay a quarter chip
out (±2 samples), timing steps an eighth, the timing loop's gain scaled to
move as fast in chips; the search decimates by SPS/2 to two a chip (its
frequency bins follow: a first try took them from fs/2 and doubled every
acquisition frequency). Weak-signal tests now set the level as SNR a bit.
Measured: tracking ~0.6 dB short of ideal (was 1.3–1.6); frames decode to
−0.5 dB a bit in low-SNR mode, 0 dB in normal (was about +0.1).

## DVB-CID on a live carrier: phase noise and clock error (2026-10-09)

Rory's 3.58 MBd 8PSK host: the CID found at 9.2 dB, tracked at 8.4 dB a
bit, but the despread bits a ring and no unique word, in low-SNR mode.
Simulated with real-world impairments (8 dB a bit):
- Phase noise (random walk, 0.6 rad a bit): low-SNR mode looked for the
  unique word on the coherent combination only, which phase noise defeats.
  Now either the coherent or the summed-differential combination may find
  it, and either decoding is tried; the Costas loop has a lock detector
  (cos 2φ averaged) and is given up below 0.3, leaving the frequency loop.
- Clock error (the receiver's chip clock a few ppm out — a HackRF is
  ±20 ppm): the timing loop was first order and lagged; at 5 ppm neither
  mode decoded. It is now second order (an integrator for the drift, the
  discriminator in samples: 4x/(3·SPS)), the deep gains scaled with the
  SNR. The integrator gives the clock error (`CidStats::clock_ppm`, shown
  in the card).
Now: both modes read frames with 0.3–1 rad of phase noise, and with 5 ppm
plus 0.6 rad together (test); normal mode copes with ±12 ppm, low-SNR mode
±8 (its 96–384-bit searches smear the code peak beyond that; a clock
correction would extend it).

## Clock correction (2026-10-09)

One value per receiver (`VfoSettings::clock_ppm`, the GUI keeps every VFO
on the one in prefs.txt, `clock_ppm=`; CLI `--clock-ppm`): how much faster
transmitters' clocks run against the SDR's, in the same terms the CID card
measures. The CID thread resamples to the chip rate as our clock sees it
(SPS × rate × (1 + ppm·10⁻⁶), retuned live), so even 384-bit searches see
no drift. The Carrier ID VFO's settings show it with "Use measured", which
adds the timing loop's measurement. Checked: the CID test capture decoded
as if 15 ppm out (rate given 15 ppm high) — uncorrected, neither mode reads
a frame (8–16 searches); with --clock-ppm 15 both lock at once, 9.9 dB a
bit, identifier read.

## DVB-CID on the spectrum (2026-10-09)

Once a Carrier ID VFO has the code, the band view marks the spread signal
under its host: a faint column over its occupied band (1.35 × the chip
rate, RRC α 0.35) at host centre + the CID's offset, and the raised-cosine
shape at its level — the despread SNR a bit less the 36.1 dB processing
gain, against the spectrum's power there (host and noise). Below the
display's floor it is drawn as a small dashed hump on the floor instead,
the label still giving the level ("DSSS · 302 kHz · 29 dB under"). On the
test capture (CID at −27.5 dB re the host's density, host at Es/N0 10 dB)
it reads 29 dB under.

## Generic PSK: bits instead of symbols (2026-10-09)

`VfoSettings::psk_bits` (GUI "Output: bits instead of symbols", CLI
`--bits`): the .bin gets each symbol's log2 M bits from its label under the
chosen numbering, MSB first, packed into bytes (`SymbolPacker`, bits left
over carried to the next block; the mode is fixed when a recording starts;
the name ends -bits). Checked on the 10 kBd QPSK capture: 60 209 symbols →
15 052 bytes, equal to the Gray labels unpacked. Also fixed: four tooltips
whose line continuations had been lost (long runs of spaces in the text).

## MPE radio: fragments, unannounced AAC and Opus, now playing (2026-10-10)

A live DVB-S2 TS carrier (867 kbit/s, MPE on PID 0x0020) carries eight
radio streams to 225.0.0.x with no SAP: none was listed. Its RTP packets
hold four or eight AAC access units, 2.3 kB, so each crosses the 1500-byte
MTU as two IPv4 fragments — and only the first, cut short, was being read.

- `decdvb-ip::frag::Reassembler` (RFC 791 §3.2): keyed by source,
  destination, protocol and id; pieces in any order; a datagram whose pieces
  stop is dropped after 4096 packets, 64 held at most. The IP stage counts
  and records packets as sent (Wireshark reassembles itself) and reads
  payloads — text, multicast, audio — from whole datagrams.
- The scanner describes unannounced RTP in a dynamic payload type: RFC 3640
  by any number of 13/3-bit AU headers summing to the payload; the RTP clock
  from RTCP sender reports (NTP against RTP over ≥ 1.5 s, snapped within
  1 %) or its timestamps against signal time (≥ 4 s, within 3 %); AU ticks
  1024/960 → AAC-LC, 2048/1920 → HE-AAC (explicit SBR signalling, object
  type 5, ISO/IEC 14496-3 §1.6.5); the first element SCE/CPE → mono/stereo.
  Opus (RFC 7587) by packets that parse under RFC 6716 §3.2 with a steady
  TOC byte, in at least half the packets. The SDP made (`SdpInfo::inferred`)
  feeds the in-app decoder and the external player alike.
- Opus records to Ogg Opus (RFC 7845, a page a packet); it is not decoded in
  the app.
- `<nowplaying>` XML (title, artist, station, media_type) is read into a
  **Now playing** list; its station codes are not tied to the streams.
- `decdvb mcast file.ts [--decode] [--record dir] [--ts-rate bps]`.

On the recording (24.8 s): two AAC-LC and five HE-AAC streams at 44.1 kHz
stereo and one Opus at 48 kHz, all from RTCP; the seven AAC decode with 0
errors; ffmpeg reads the .aac as HE-AAC/LC and the .opus as 24.7 s of
stereo Opus; the HE-AAC SDP, replayed over RTP into ffmpeg, decodes.

## Opus in the app: libopus vendored (2026-10-10)

`third_party/opus` (xiph/opus v1.6.1, shallow submodule) built by
`decdvb-opus-sys` — DecDRM's `decdrm-opus-sys` build script and bindings:
CMake, static, float API, no DNN extensions, the static C runtime when
`crt-static` is on. `decdvb-audio::decode` wraps the decoder (48 kHz stereo
out; mono duplicated by libopus) and names the mode's bandwidth; the player
no longer refuses Opus. CI and the release workflow check out submodules;
Linux CI installs CMake.

Tests: a libopus-encoded 1 kHz tone, mono and stereo, decodes at its level
(RMS within 0.05 of 0.354); garbage is an error; 50 RTP packets play through
the player to the null output. On the recording, 225.0.0.20:6012 decodes
24.7 s with 0 errors (stereo, fullband).

## HE-AAC with SBR and PS: libxaac (2026-10-10)

HE-AAC played its AAC-LC core (Symphonia has no SBR): 11 kHz of audio at
half the rate. AAC now goes through libxaac's decoder (Ittiam, Apache-2.0 —
GPL-3-compatible, unlike FDK-AAC), the `third_party/libxaac` submodule
(v0.1.13, as in DecDRM) built by `decdvb-xaac-sys` with `cc` from libxaac's
own CMake source lists (common, decoder, DRC, generic kernels). Each access
unit goes in as an ADTS frame of the core's configuration; libxaac finds
SBR and PS in the frames (implicit signalling), so ADTS, LOAS, LATM and
RFC 3640 streams all get them. It reads the header from the first frame and
trims its start-up delay from the first block out. A fatal error starts a
fresh decoder. Symphonia keeps MPEG audio; `symphonia-codec-aac` is gone.

`decdvb mcast … --wav dir` decodes every stream to WAV. On the recording,
against ffmpeg decoding the same frames: AAC-LC 225.0.0.2:6002 and HE-AAC
225.0.0.20:6020 both agree to about 78 dB SNR after the first second, the
HE-AAC over the full band (11–16 kHz at −30.6 dB of the total against
ffmpeg's −30.2 dB). All five HE-AAC streams report SBR at 44.1 kHz.

## CLI: live HackRF decoding, data recordings re-read (2026-10-10)

`decdvb decode --hackrf <MHz> [--rate] [--lna] [--vga] [--amp] [--seconds]`
runs any decoder live (receive only, antenna power off; a status line every
30 s), and `--record-on-activity` (with `--out`) leaves a FastLink VFO
recording only while its multiplex is busy — an unattended watch for
speech. `decdvb payload file.bin [--frame-bits] [--tdm-out]` runs a modem
data recording through the payload stage again and writes the TDM
multiplex's aligned frames (`TdmRx::keep_frames`). The 19:54 FastLink
recording (39 s) is like the others: Paradise aligned, TDM aligned with no
errors, codec channels 0–2 idle; its 20 ms state word reads 11110 (as
QFLEX_2's did; qflex_3's 01101).

## Q-Flex channels: 4 ms subframes, and two calls on record (2026-10-10)

Re-reading the multiplex channel by channel (`decdvb payload … --tdm-out`,
scripts in the scratchpad):

- Every channel (one bit position of the 16-bit words, 8 kbit/s) runs in
  4 ms subframes of four octets, `W1 W2 S S`: 16 data bits, then a status
  octet sent twice. The subframes slip a bit against the TDM frame now and
  then (channel 0: about every 10 s, ~12 ppm): each source has its own
  clock, so a channel decoder must follow its own subframe phase (the
  doubled S finds it).
- S: on channels 6–15 it is 00 or FF, changing every 0.1–0.8 s — the
  signalling seen before, now per channel; on 0–5 it is a byte with two
  bits set that rotates every ~10 s.
- In silence the codec channels' data cycle five octets (a 40-bit pattern
  over 20 ms, read in 16-bit steps): channels 0 and 2 the same (FF FE F3 CE
  BB), 1 and 15 others; their rhythm, a 0 every fifth bit, is that of the
  CDM-600L's E1 TS1 sub-rate streams too (same voice equipment behind both
  modems, it seems).
- **Calls**: in qflex_3 at ~380–420 s and in the 19:54 FastLink recording
  at ~18.5–32 s, channels 0, 1, 2 and 15 leave their silence frames at the
  same instant and carry changing data in talk spurts with pauses. Four
  distinct streams (no two agree beyond chance at any lag), 16 data bits
  each per 4 ms — 4 kbit/s a channel, 16 kbit/s in all. Over talk spurts
  the data have a 20 ms (5-subframe) frame: channels 1 and 15 hold a fixed
  0 about every fifth bit (and 15 a per-call flag at the last bit of each
  subframe: 1 in one call, 0 in the other); 0 and 2 have no fixed bits.
  Not G.729 (pitch parity) and not G.726-16 (no interleave decodes to
  anything speech-like through ffmpeg). The codec is still unknown.
- The old activity meter (> 25 % of bits changed at 20 ms) missed these
  calls: speech changes only the data half of a subframe (~25 %, often
  less). `TdmRx` now remembers a channel proven an idle codec (10 s idle
  in all) and calls it active once it departs from its silence frame by
  5 %: on qflex_3 channels 0–2 now read active for the call only (~10 s;
  15, whose status octet toggles like the signalling channels', more). The
  auto-record trigger uses it. `decdvb payload` prints each channel's
  active time.

ESC (the Paradise overhead's 22 bits a group, ~4.4 kbit/s): idle 0;
packets start on an ESC octet (group bit 1 or 14), their FAW/CTRL bits
included; lengths in families ~825, ~1118, ~1270, ~1930 bits (±7), sent in
a repeating three-message cycle with gaps of 1.1–2.2 s — the modems' M&C
polling; single bits at multiframe (64-group) boundaries between. Not HDLC
in any bit order, polarity or NRZI reading, not sampled async, not text
under any shift; near-identical repeats rule out encryption. Its coding is
still unknown. `decdvb payload --esc-out` writes the bits.

None of Rory's 102–128 kBd QPSK symbol recordings carries the FastLink
unique word under any label assignment.

Call log: `TdmStats::calls` — a call opens when two or more proven codec
channels are active at once (one alone may be its status octet), grows
while any of them is, and closes after 2 s of quiet. The FastLink card
lists the last eight (time from alignment, length, channels), `decdvb
payload` and `decode` print them. On the recordings: qflex_3 one call at
395.3 s for 13.7 s on channels 0, 1, 2, 15; the 19:54 one at 18.9 s for
12.4 s on 0, 1, 2 (15 not yet proven in 37 s). So a long unattended
session shows when anyone spoke, and auto-record keeps the data.

Codec lead: G.728 (LD-CELP, 16 kbit/s, 10-bit codewords) is what ATC voice
equipment of this kind uses for 16 kbit/s sub-channels (e.g. a SolaCom
patent: a 64 kbit/s ATS-QSIG channel split into four 16 kbit/s ones, three
of G.728 voice). The CDM-600L's E1 TS1 codec is exactly such a 16 kbit/s
sub-channel (bits 2–3 of each octet), and its combined stream agrees with
itself 10 bits on (0.63, against ~0.53 at the lags either side), its
silence made of near-all-ones 10-bit words — the "0 every fifth bit" of
each 8 kbit/s half. The Q-Flex calls are 16 kbit/s across four channels
too. Testing it needs a G.728 decoder, i.e. the codebooks of ITU-T G.728
(not fetched: ask first).

G.728 tried (2026-10-10, Rory's go-ahead): the ITU-T reference coder (G.191
STL, `openitu/STL` src/g728, AT&T, "ITU-T General Public License" — used
locally as a test tool only, nothing of it in this repo) built and checked
bit-exact against all its test vectors. Then the CDM-600L TS1 sub-channel
and both Q-Flex calls were decoded under every codeword alignment, bit and
field order, lane order and polarity (the Q-Flex stream taken as two bits a
TDM word: channels 0, 1 for 2 ms, then 2, 15). Result: not confirmed.
- Inverted, the idle frames decode near-silent (5th-percentile frame level
  −50 dBFS, against −10 for random codewords): the idle code is close to
  all-zero codewords once inverted. That fits G.728 but would fit others.
- No reading decodes the talk spurts as speech: every one comes out loud
  and clipping (2–4 % of samples), like random codewords (8 %).
- The talk data carry fixed bits plain G.728 cannot have: in the second
  lane of each pair (channels 1 and 15) a 0 after a mostly-1 bit, about
  every fifth lane bit — 32 per 20 ms — and channel 15's per-call flag.
  Their drift says each 4 ms holds ~60 bits of a coded stream, but no
  choice of lane order and left-out bit puts them on a 10-bit grid. So the
  multiplexer's mapping is not solved; with it, G.728 (or its 12.8/9.6
  kbit/s Annex H, if the fixed bits are padding) could be tried again. The
  make of the voice multiplexer would short-cut all of this.

The calls' bit layout, found (2026-10-10, continued): one 16 kbit/s stream,
two bits a TDM word — channels 0, 1 for 2 ms, then 2, 15 for 2 ms — each
pair's data window at its own offset in the subframe (in the 19:54 call
pair 2's starts a bit earlier than its doubled status octet suggests;
15/16 bits), shifting a bit now and then as the channels slip. So taken,
the talk spurts sit on a strict 10-bit grid: one bit 96 % constant
(raw 0), the bit two places before it 81 % ones, the other eight free, no
structure at any longer period (20–320 bits). 10 bits every 0.625 ms is
G.728's codeword; its Annex H (12.8 kbit/s, which drops shape-index bit 9)
explains a constant bit. But nothing decodes as speech: the ITU reference
decoder, 16 kbit/s and a local 12.8 kbit/s variant (Annex H Table H.1 gain
table; checked against the Annex H test vectors to 44.6 dB), over every
codeword phase, bit order, polarity and lane arrangement (interleaved or
split, each lane either way), gives a voicing score of at most 0.04 where
real speech scores 0.57. The units show no sample-to-sample correlation
either, so they are codec indices or data, not PCM. Still open: the codec,
or a scrambling of the units.

## WAV IQ import (2026-10-10)

Two-channel WAV files (I left, Q right) open like raw captures, in the GUI
(Open IQ…, the file dialog lists .wav) and every CLI command: `decdvb-io::wav`
walks the RIFF (or RF64/BW64, with `ds64` sizes) chunks for `fmt ` — PCM
8-bit (unsigned, new `SampleFormat::Cu8`, also RTL-SDR's `.cu8`), 16- and
24-bit (new `Cs24`), IEEE float 32-bit, `WAVE_FORMAT_EXTENSIBLE` included —
and `data`; the reader starts at the data chunk, stops at its end (trailing
chunks are not samples) and rewinds to it. SDR#/HDSDR/SDRuno `auxi` chunks
give the centre frequency (HDSDR-style `…_1635640kHz_…` names otherwise).
The header wins over flags and name guesses (`probe_capture`,
`capture_format_rate`). Checked: the 8 MS/s test scene as 16-bit WAV with
an auxi centre scans to the same carriers and identifications without
flags, and opens in the GUI at Cs16, 8 MS/s, 1.635640 GHz.

## The CDM-600L's TS1 voice is the same codec as the Q-Flex calls (2026-10-10)

Rory hears digital noise on the CDM-600L's D&I timeslot 1: rightly — it is
not G.711. Its codec rides in bits 2 and 3 of each octet, a 16 kbit/s
sub-channel at two bits a 125 µs, the same shape as the Q-Flex calls' stream,
and folded at 10 bits it shows the same thing: in talk (0–6 s and 12–14 s
of the recording) one column at 0.03 — the talk flag — and the rest near
0.5; in silence (8–11 s) frames repeating every 20 ms with mostly-1 columns.
So the same voice equipment feeds both modems, and the Comtech
demodulation, TPC and D&I++ framing are right; only the codec is unknown.
On this clean stream (no pair alternation or slips) G.728 again fails at
16 kbit/s and 12.8 kbit/s, both lane orders, all phases, bit orders and
polarities: what scores as voiced is a pitch at multiples of 5 samples —
the codeword period showing through — not speech. (Voicing now measured on
the first difference with the peak kept off the range edge: speech 0.45,
random codewords 0.14.)

## The CDM-600L's TS1 voice decoded: G.728 with a robbed sync bit (2026-10-10)

The codec is ITU-T G.728 LD-CELP at 16 kbit/s. What looked like a talk
flag is a frame sync: in talk the bit is (raw) 0 in 31 codewords and 1 in
the 32nd — a 20 ms frame of 32 codewords; in silence it stays 1. It is
G.728 §3.11's synchronization by bit robbing with N = 1: the encoder
searches only half the shape codebook and the shape index's MSB (codeword
bit 9) carries the sync bit. So on the TS1 sub-channel (bits 2–3 of each
octet, 16 kbit/s):

- a codeword is 10 contiguous bits starting at the sync bit, MSB first,
  all bits inverted: bit 9 sync (inverted 1 → shapes 64–127, once per
  20 ms 0 → 0–63), bits 8–3 the rest of the shape index, bit 2 the gain
  sign, bits 1–0 the gain magnitude (G.728 §3.9 and §5.14: 7-bit shape index
  then 3-bit gain index; codebooks in Annex B);
- the silence fill is not codec output: raw units 1111 cccc 11 with a
  4-bit counter stepping down one a codeword (inverted, codewords 0…15
  over and over), which decodes near-silent.

Evidence, before any decode: the gain-magnitude field's distribution
(0.25/0.38/0.28/0.09) is G.728's on noisy speech (0.26/0.38/0.29/0.07 at
5 dB SNR, from the reference encoder restricted to half the shape codebook
on synthetic speech); the joint histogram of the other nine bits matches
that encoder's with the plain bit order (r 0.80, the next-best of all 5040
orders 0.61); the gain field's autocorrelation peaks at 12–14 codewords, a
~120 Hz pitch. Decoded with the ITU reference decoder (local test tool),
the talk spurts are telephone-band speech (300 Hz highpass), pitch
harmonics ~110–130 Hz, voicing 0.47 on 11.8–14.4 s — as real G.728 speech
scores. The first spurt (0–3.4 s) is noisier (voicing 0.03).

Why the earlier sweep missed it: this reading was in it, but scored 0.16
over its two windows — the noisy first spurt and silence-fill codewords
inside the window drag it down — below readings whose "pitch" was the
codeword period. The decoder is robust to the things that were worried
about (cycle phase, a dropped or zeroed codeword in 32: all within 0.04 of
the true voicing on the reference).

Q-Flex: the calls carry the same framing (sync bit 1 in 32, 81 % ones two
places before it — a cleaner talker), but their four-channel reassembly is
not yet right: each channel slips on its own clock (channel 0's data
window moved a bit between 397 s and 407 s), so a shared offset per pair
scrambles alternate bits; with per-channel offsets the sync spacing holds
94 % but the gain-bit pair is still weak (17 mbit against 84 on the
CDM-600L). Next: the silence fill is a known plaintext to pin the mapping.

## G.728 in the app (2026-10-10)

`decdvb-modem::g728`: a G.728 decoder written from the Recommendation —
§5.14's blocks 29–33 and the postfilter (71–85), Annex E's update timing,
Table 1's constants, Annexes A–C's tables as printed (pulled from the text
by a script, checked against the text's own floating-point tables and
Table 1's factors) and Annex D's filter. The 2012 text has misprints: five
shape-codebook entries without their minus signs (indices 36 and 82) and
Annex D's coefficients' signs garbled ("2–2.3403658918"); the codebook signs
are the ones under which the Appendix I test vectors decode, the filter's
the ones giving it unit DC gain and its 4 kHz zero. Against Appendix I
(CW1–6 → OUTA1–6 postfilter off, CW4 → OUTB4 on): 112.6–209 dB SNR, i.e.
equal but for rounding. The vectors are ITU's and not in the repo: the
`#[ignore]` test `decodes_the_itu_test_vectors` reads them from
`DECDVB_G728_VECTORS`.

`g728::Lane` finds the channel in two bits of each octet: 40 ms at a time,
the 10-bit grid whose first bit is alike in all codewords but two, 32
apart; polarity from that bit (1 in 31 of 32 once right); the odd one ends
an adaptation cycle; blocks without the pattern (silence fill, onsets) are
not decoded. `g728::Voice` tries every pair of neighbouring bits a sub-rate
timeslot changes. The engine's E1 stage keeps one on each sub-rate timeslot
(kept once found: a few garbled frames can make a half second judge G.711)
and plays and records the found one decoded, as A-law through the existing
player and recorder. GUI: "G.728 speech" / "G.728, silent"; CLI: "G.728
voice in bits 2–3". Checked: the CDM-600L recording decodes end to end
(`decdvb decode … --decoder tpc --e1-record 1`) to the same speech as the
reference decoder, the silence fill muted; only the first half second,
before the timeslot is judged, plays raw.

## The Q-Flex calls are G.728 too: their channel mapping (2026-10-10)

The silence fill is a known plaintext (raw units 1111 cccc 11, the count
stepping down: a 160-bit cycle), and each call channel's data bits in
silence match it exactly (96–100 %) as every other bit of the 16 kbit/s
stream, 64 stream bits a 4 ms subframe. So the stream is two 8 kbit/s
halves interleaved: even bits from channel 15 (15 bits, from subframe
bit 0) then channel 1 (16, from bit 16), odd bits from channel 2 (16,
from bit 0) then channel 0 (16, from bit 16). Two things make it hard:
- the halves drift against each other (0 half-bits apart at 401 s, 8 at
  407 s; in the talk spurts the framing says 32), and the channels slip
  a bit now and then, each on its own;
- one even bit each subframe is in none of the 16 channels (it sits
  between channel 1's block and channel 15's next): 63 of 64 bits carried,
  so about one G.728 codeword in six has a wrong bit.

Taken as halves with a delay searched per 0.5 s, the windows where the
G.728 framing holds (sync bit 1 in 32, gain-bit pair dependent) decode with
the reference decoder as speech (pitch harmonics, formants) in both calls
— about 2.5 s of each so far; the rest is lost to slips inside windows.
Next: follow each channel's slips subframe by subframe (by its overhead
pattern) and the halves' delay continuously, then put it in the TDM path
as the CDM-600L's is.

## Q-Flex call voice in the app (2026-10-10)

The "drift" between the call's two half-streams was the analysis's own
doing, mostly the 0.5 s windows guessing channel 15's place a bit either
way: in both calls the channels hold their places throughout. Per 4 ms the
stream is, in time order, (ch1, ch0) bit pairs in their data half and
(ch15 one bit late, ch2) pairs in theirs, the first even bit of that half
lost (the 64th bit, in no channel). The 19:54 call has the halves the other
way round in the subframe; the rule is the same.

`tdm257` now learns this itself. While a channel idles as a codec its last
160 ms are matched against the silence fill (`calibrate`: for each offset
into the fill's 160-bit cycle, the longest run of subframe positions that
follow it; the offset is known modulo 160, or 80 for some channels). Four
placed channels whose bits tile a subframe's 64 stream bits (63 here, none
twice, offsets within 40 of each other, relative so that the fill
restarting after speech changes nothing) make a stream (`group`). A run can
come out a bit long — a status bit next to it matching the fill by chance —
so every 15- or 16-bit window of each run is tried: all such tilings are
assembled and framed side by side, and the first whose G.728 framing shows
speech is kept. The stream goes through `g728::Framer` (the Lane's framing
split out, with `UNCARRIED` bits: a sync slot not carried may stand for an
odd one; decoded as the sync value or 0).

Engine: `tdm_play` / `tdm_record` in the VFO settings; a CallVoice stage
plays and records the decoded speech as A-law, 2 ms a TDM frame, silence
between talk spurts. GUI: a "Call voice (G.728)" card (channels, speech or
silent, seconds so far, ▶ ●). CLI: `--tdm-record` and a "G.728 call voice"
line. Checked: the qflex_3 capture end to end — call at 395.3 s on channels
0, 1, 2, 15, 8.7 s of speech decoded and recorded where it falls in time;
the 19:54 call's frames, 9.9 s (the `#[ignore]` test
`decodes_a_captured_call` reads saved frames). Weak steady lines at 400,
1200 and 2000 Hz remain in both calls' decodes; they do not change with the
guess for the missing bit.

E1 timeslots not judged yet, or sub-rate with no G.728 found, now play
silence instead of their bytes as A-law (loud noise).

## Carrier drift followed; CDM-600 voice preset; settings remembered (2026-10-10)

- **Drift.** A carrier drifting out of its VFO (an LNB warming up) is now
  followed: every 0.25 s, while the demodulator is locked, the residual
  offset its carrier loop holds (beyond 5 Hz or 0.1 % of the symbol rate)
  moves into the down-converter's tuning and out of the loop
  (`retune_hz` on `PskDemod` and the DVB-S2 `Demod`, its re-acquisition
  hint too), so the loop sees no step and the VFO's band stays on the
  carrier. The worker reports the drift it followed; the GUI moves the VFO
  by it once the worker runs on the GUI's offset, and the worker takes a
  move of up to what it followed as its own (nothing restarts). Setting
  `follow_carrier` (on; "Drift: follow the carrier"). Test: 20 kBd QPSK in a
  40 kHz VFO drifting 25 kHz (2.5 kHz/s after a steady start) stays locked
  and followed; without following it is lost. Acquisition while already
  ramping at 3 kHz/s fails (the carrier loop's pull-in), which a real LNB's
  drift is far below.
- **CDM-600 voice preset** (`DecoderKind::Cdm600Voice`, CLI `cdm600`): the
  TPC 2964 chain (`chain()`), with `voice_auto` — the E1/D&I stage plays the
  first timeslot whose G.728 has been found when none is chosen, so several
  carriers play at once (each player its own output stream). ⏹ on the
  auto-playing timeslot turns it off for that VFO. Badge: "LOCK · G.728
  speech / silent / voice?". The CLI keeps it off (it records instead).
- **E1 silence** as before: timeslots not yet judged, or sub-rate with no
  codec found, play silence.
- **Settings remembered** in prefs.txt: the HackRF panel (frequency, rate,
  gains, LNB LO), the file source's rate, format and centre, and the VFOs
  (`vfo.N`: name, offset, width, decoder, symbol rate, modulation, follow,
  voice, gold code, …), written within 2 s of a change and restored at
  start (VFOs join the first source started). Unattended runs (screenshot,
  claimed carriers) neither read nor write them. The LNB LO default is
  10600 MHz again.

Three more CDM-600 sites (Rory's VFO_3/4/5 TPC recordings, "more than one
VHF radio" each) are not the first carrier's format: no D&I++ header
(scrambled or not), no E1 FAS, no HDLC; unscrambled, idling on a repeating
8-bit pattern (00000110 or 00000101) with bursts (VFO_4 busy, VFO_3 a third
of the time, VFO_5 idle throughout). Their framing is still to be found.

## DecSAT; speaking carriers light up (2026-10-10)

Renamed DecSAT (crates, binaries, prefs with DecDVB's carried over; the
GitHub repository is CasualArclamp/DecSAT, the old name forwarding). A VFO
whose carrier has voice active — G.728 speech in an E1/D&I timeslot, or a
TDM call talking (`band_view::voice_active`) — is drawn green with 🔊 in
the waterfall and the VFO list. Checked on the CDM-600L recording (an
unattended screenshot, TPC decoder so nothing plays).

## Frequency plan (2026-10-10)

`decsat-gui::freqplan` reads SDR++ frequency-manager bookmark lists
(`{"bookmarks": {name: {frequency, bandwidth, mode}}}`, `serde_json`):
📋 Plan… in the toolbar, the path kept in prefs (`freqplan`; right-click
clears it), the plan itself never copied. The band view marks each plan
carrier in view along the spectrum's foot (names decluttered left to right,
more as it zooms), names the one under the pointer with its frequency and
width, makes a VFO on a clicked name (its width × 1.12), and names VFOs
drawn, claimed or double-clicked onto a plan carrier (`freqplan::at`). GUI
automation takes `--plan FILE` (unattended runs do not read the saved one).
Rory's Horizons 3e plan (Comtech and Q-Flex sites; 58 837 Hz is
1.35 × 43.6 kS/s, the CDM-600s) stays local.

2026-10-11: an optional per-carrier `"color": "#rrggbb"` (DecSAT's own
field; a malformed one is ignored) colours its tick, label and hover card.
The H3e plan was rebuilt from the Satellite SOC v9 (98 → 233 carriers:
inbound RF = hub Rx IF + 12 210 MHz, outbound = remote Rx IF + 11 300 MHz)
and coloured by modem from the SOC hostnames: Q-Flex `#5aa0ff`, Comtech
CDM-570/600 `#6ed76e`.

## LO trim; multi-radio trunks; one voice at a time (2026-10-10)

- **LO trim** (`main.rs`): the LNB's error added to the RF scale — axis,
  readout, plan labels, VFO names — with the radio's tuning unchanged
  (`rf_center` = untrimmed + trim; `set_rf_center` takes it off again).
  ⟲ Align: `freqplan::align`, each carrier (wide ones) paired with plan
  carriers of 0.6–1.7× its width within ±3 MHz, the trim most carriers agree
  on within two FFT bins (their median; two needed). keep: every 5 s, ±20
  kHz, half the step. Saved as `radio.lo_trim_hz` / `radio.plan_keep`.
- **Multi-radio Comtech trunks** (`decsat-modem::slotpkt`): Rory's VFO_4
  recording (320 kbit/s — the same as his live VFO 20, 213.3 kS/s) is HDLC
  on an inverted line: idle flags `00000011`, packets of 398 bits whose
  FCS-16 (bitwise, CCITT, remainder F0B8h) checks on every one. Content: bit
  0 = 1, bits 1–4 a link sequence count, channel byte `10h`/`20h` (two in
  turn, two packets each), type `13h`, five bits (`0xxx0`), 352 timeslot
  bits, a 1, the FCS. Found by joining each channel's packets and asking
  which cut keeps the timeslot's 1 ms pattern unbroken across the joins
  (bits 29–380; bit 381 breaks it). Each channel is then 23.9 s of 64 kbit/s
  over the 24 s recording, with G.728 in bits 2–3 exactly as on the first
  CDM-600L — all silence fill in this recording (`decsat payload … --frame-bits
  2223` now says so per channel). VFO_5 (187 kbit/s) is the same link with
  only a status packet every 2 s. Other packet types share the 30 + 8n bit
  shape. VFO_3 (≈1 Mbit/s, idle `00000101`) is something else again: not
  yet found.
- `payload::Format::Slots`: chosen on packets (three) or flags covering most
  bits (an idle site); inverted line only, so plain HDLC links are untouched.
  The E1 stage makes E1-shaped frames from the channels (a channel unheard
  for 16 packets stops holding the others back) and runs its G.728 finders
  on them.
- **G.728 silence fill recognised** (`Framer::fill_blocks`): the sync bit
  and the five below it fixed, the low four counting one per codeword (up;
  down on an inverted line). A lane sending it counts as found, so an idle
  trunk shows "G.728, silent" instead of "voice?". Steady and random bits
  are never taken for it (test).
- **Mixing and the voice floor** (`decsat-engine::floor`): the preset now
  plays `MIX` — every G.728 channel of the carrier, mixed — once any has
  spoken. With one voice at a time (default, hold 3 s), every auto-playing
  channel across all VFOs asks an app-wide floor: the first to speak holds
  it until quiet for the hold; the rest are muted meanwhile. The card says
  "on air" / "waiting". ▶ on a channel bypasses it. Toolbar: master Volume
  and mute (the app-wide player gain, as before), the floor switch and hold
  (prefs `voice_hold`, "off" for none).

## Q-Flex calls auto-play; their fill never decoded (2026-10-10)

- **Auto-play** (`CallVoice::follow`): under the FastLink decoder's Voice
  setting a call plays once its stream has spoken (`talk_blocks > 0`), as
  the voice floor allows (`may_play((source, 0), talking)`; ▶ bypasses it);
  the card says on air / waiting, ⏹ turns `voice_auto` off.
- **False speech in the fill, found decoding qflex_3 to a .wav:** 34 bursts
  of near full-scale noise between calls (every 10–20 s, 0.5–1.2 s each),
  never counted as a call. Logged blocks showed why: the Q-Flex's silence
  fill (inverted units `111111cccc` counting down) keeps the sync marker
  going, once in 32 codewords, and read one bit off its codeword phase the
  marker column looks like speech's sync — with the opposite polarity to the
  real call (sync mostly 1, not 0). The fill test needed all six fixed bits
  in every codeword, so the marker and the stream's odd wrong bit (five
  codewords of 64 off, 89 % of count steps right) defeated it. Now
  `fill_at` allows `FILL_BROKEN` = 8 codewords off and 3 steps in 4 (speech:
  ~60 and 1 in 16) and runs before the speech test. All 34 bursts gone, the
  call unchanged (8.7 s); the CDM-600L's decode identical sample for sample.
  Also: a block with more than three unknown sync bits is not speech.

**The Q-Flex calls' tones are the radio's own (2026-10-10).** Rory, having
listened: the weak lines at ~400, 1200 and 2000 Hz in the decoded calls come
from the radio itself, not the decoding. Nothing to chase there.

## A carrier that changes symbol rate is followed (2026-10-10)

Rory's ASA253 MAL-BN (a Q-Flex site, `decsat-ASA253_MAL-BN-+1668302Hz-
217391Sps-….cf32`) "doesn't decode as Q-Flex". It isn't FastLink: QPSK with
K=7 rate-1/2 Viterbi, differential coding and V.35 (taps 3, 20) — and the
modem changed from 64.000 kS/s (64 kbit/s) to 48.000 kS/s (48 kbit/s) four
seconds into the recording, at the same frequency and power. Its data were
idle (zeros after descrambling) throughout. DecSAT acquired 64 kS/s from its
first look and demodulated on at that rate after the change, its timing loop
wandering to 63 kS/s while still reporting a lock, so nothing decoded.

`identify::symbol_rate_moved`: the carrier's own cyclic line (±1 %) gone and
a clear one (>15 dB) elsewhere in 0.3–2.5× the rate. A modem or PSK VFO whose
rate was found blind checks every 2 s of baseband (`Worker::watch_rate`) and,
moved twice in a row, acquires afresh (its FEC thread told to quit first).
The whole MAL-BN recording now decodes: re-acquired at 47 999 S/s, rate 1/2,
channel BER 8e-4. Test: QPSK switching 20 → 16 kBd is followed (and without
the check stays at 19.8 kS/s, "locked").

## 8PSK read as BPSK; false DVB-S2X on unlocked carriers (2026-10-10)

Rory's `baseband_11662547900Hz_…_comtechs_and_idk.wav` (SDR++, cs16 10 MS/s):

- **8PSK as BPSK** (`3819dcb`): two 711.111 kS/s 8PSK carriers (Rory: Comtech,
  maybe "tpc_1630") read as BPSK, a 171 kS/s QPSK one too. Their frames
  (988 symbols = 2964 bits, a 7-symbol word at frame position 151 that
  flips 180° every frame; ~31 % of the frame repeats sign-flipped) put lines
  in s² and s⁴ that are not the carrier's, and any s² line meant BPSK. A
  line now counts only when the next power has one at twice its frequency.
  The 8PSK frames are not TPC 2964's rows under any Gray mapping, alignment
  or generator (searched) — layout still unknown.
- **False DVB-S2X ACM**: two carriers (973 kS/s and 590 kS/s) demodulated
  unlocked (residual offsets of −261 and −191 kHz) gave header candidates
  thick and fast — 1 385 in 583 k symbols, 10 489 in 347 k — and one
  "confirmation" was enough: 23 and 3 285 of them, near the 16 and 1 585
  chance predicts, over every MODCOD including reserved ones. Real carriers:
  15 of 16 and 19 of 21 confirm, chance ~0.004. `Dvbs2Info::confirmed` now
  wants confirmations above 4× the chance expectation (candidates seen² ×
  5 / symbols); the recurring-spacing hint (proprietary S2) also needs
  sparse candidates. The test scene's CCM and ACM carriers still read as
  DVB-S2; the two now read "2-ring amplitude/phase, no DVB-S2 PLHEADER".

## Centred QPSK read as 8PSK (0.8.2 regression); the 8PSK TPC still open (2026-10-11)

- **QPSK as 8PSK** (`daaa9a6`): Rory's live ASA253 MAL-BN (QPSK, clear four
  clusters, −23 Hz off at 64 kS/s) read "8PSK (estimate)". `strongest_line`
  takes the mean out first; with the carrier on frequency the s⁴ line is
  at DC and went with it, the s⁸ line survived, and 0.8.2's agreement rule
  fell through to 8PSK. The constellation lines are now found with the mean
  left in (`line_in(…, keep_dc)`), and a QPSK/BPSK line stands when the next
  power has no line at all (a weak QPSK's s⁸ drowns in noise⁸); a line
  elsewhere still vetoes it. Test: centred and nearly centred QPSK at 7–10
  dB stay QPSK.
- **8PSK TPC carriers** (711.111 kS/s, frames of 988 symbols = 2964 bits,
  7-symbol word flipping 180° each frame): searched further, with nothing
  found — no (64,57)/(46,39) row or column checks, nor any rank deficit in
  64- or 46-bit windows at any alignment, under every Gray mapping and
  natural binary, plain, frame-XORed (2 and 8 apart: removes a
  frame-synchronous scrambler) or after the V.35, IESS-308, differential
  and RCV-20x descramblers. Either the bit layout on 8PSK is not row or
  column order, or the mapping is not Gray, or the code is not TPC 2964's.
  Needs the modem's settings (Rory: "might be tpc_1630").

## Transmitter (M6), DVB-S2/S2X DATV (2026-10-11)

Rory's choices: DATV from an MPEG-TS, HackRF live + IQ file, cable/dummy
load only, a GUI panel.

- **Stage 1** (`6b0447f`, `decsat-mod::datv`): `TsInput` — `TsFile` (paced
  against the channel's clock by its PCR rate, ISO/IEC 13818-1 §2.4.3.4–5;
  no PCR: as fast as the channel), `TsUdp` (raw TS or RTP from ffmpeg/OBS,
  newest kept), `TsTest`; `TsFeed` makes TS-mode BBFRAMEs (EN 302 307-1
  §5.1, CCM, no NPD) with null packets for the gaps; `DatvConfig` gives the
  channel's TS rate ((Kbch − 80) × Rs / PLFRAME length) and the sample rate;
  `DatvModulator` = PL framer + FEC + RRC shaper.
- **Stage 2** (`d902d1e`): `decsat tx` to `.cs8/.cs16/.cf32`. A recorded TS
  sent as QPSK 3/4 at 500 kS/s decoded back through DecSAT's receiver: 60/60
  BBFRAMEs good, every packet byte-identical and in order, its multicast
  radio playing. 4 samples a symbol by default: at 2 the carrier fills the
  band and Identify (rightly) could not analyse it.
- **Stage 3** (`75e37bc`): `decsat-io::hackrf_tx::HackRfTx` (TX VGA 0–47,
  amp off by default, antenna power never; the TX thread pulls cs8 from the
  modulator as USB transfers drain, so the radio paces it; drop = off);
  `decsat-engine::tx` (`TxPump`, `TxFileWriter` in real time, `TxStatus`).
- **Stage 4**: the GUI's 📡 Transmit… panel (`transmit.rs`): source,
  MODCOD (S2 and S2X data codes), frames, roll-off, symbol rate, gold;
  what it carries; HackRF (frequency, TX gain, amp — live while
  transmitting) or IQ file; level; Armed + Transmit; status (input, frames,
  packets/nulls, keeping up, bytes sent, errors). Starting HackRF transmit
  stops a HackRF receive session first. Settings except power and arming
  are remembered (`tx.*`). Automation `--tx-panel` opens it for screenshots.
  Not yet keyed on a real HackRF (Rory does the first transmit).


## A Q-Flex with skewed data rails; Identify through a slip and a wander (2026-10-11)

Rory's ASA192 MSO-BN (`decsat-ASA192_MSO-BN-12342.5165MHz-303030Sps-….cf32`,
12 s, FastLink QPSK 95.0 kS/s) "not decoding as the Q-Flex":

- **Payload not recognised**, though the FastLink FEC was fine (805 of 816
  codewords, channel BER 1.3e-5, turned 90°). The Paradise 672-bit group was
  there (nine fixed bits a group, FAW and CTRL 336 bits apart), but each
  overhead octet came out spread over ten bits, fixed bits at offsets
  0, 2–7, 9 and data at 1 and 8: every pair's odd bit (the Q bit of a QPSK
  data symbol) is sent two pairs after its even bit, across codeword and
  frame boundaries alike. Only that skew fits (FAW 100 %; every other skew
  from −4 to 4 pairs under 26 %). Not the decoder: qflex_3 turned 90° by hand
  decodes as before, unskewed. A setting or firmware difference in that
  modem, presumably.
- `PayloadRx::with_rail_skews`: the search also tries the unscrambled data
  with either rail held back 1–3 pairs; the FastLink decoder and `decsat
  payload` (16 384-bit frames) opt in. A skewed reading still half-frames
  (four of the nine fixed bits come from the wrong place, the deframer allows
  one wrong and coasts through misses), so Paradise now scores the groups
  whose fixed bits checked. Result: "Paradise closed network + ESC framing,
  not scrambled, rails skewed (odd bits 2 pairs late)", 2454 groups, TDM
  aligned, G.728 call voice found on channels 0, 1, 2 and 15 (silent in these
  12 s). Channels 8–13 change for 4.4 s of it — not known what.
- **Identify said "possibly DVB-S"**: a half-turn carrier slip after the
  second frame of its listen, and `fastlink::locate` only took runs of sync
  words starting in the first frame. It now takes the earliest run anywhere,
  under any orientation.
- **qflex_3 identified as 8PSK** (since 0.8.2's line-agreement rule): LNB
  wander smears the s⁴ line into a 220 Hz hump and the s⁸ line into one twice
  as wide; their tallest bins were 11 Hz off one frequency and twice it,
  against a 9 Hz tolerance, and the s⁸ line vetoed QPSK. Now (a) the
  tolerance grows to twice the line's width (`identify::Line`: the contiguous
  −6 dB span of the 9-bin-smoothed spectrum, so a comb of separate lines does
  not count), and (b) a weaker s⁸ line never vetoes QPSK — QPSK's s⁸ line is
  its s⁴ line squared (11–16 dB weaker on every live QPSK capture), 8PSK's is
  its own (19–23 dB *stronger* than the data-made s⁴ lines of the live
  Comtech 8PSK). BPSK keeps the strict test: that 8PSK's data lines in s² and
  s⁴ are only 3–7 dB apart. The Comtech recording's scan is unchanged.
  Tests: QPSK under sine and smooth ±35 Hz wander (fails on the old rules),
  FastLink framing after a phase slip, Paradise on skewed rails both ways.

## A live survey: every decoder on the satellite's carriers (2026-10-11)

Rory: "go around decoding carriers and check they work". Receive only, his
HackRF (LNA 40, VGA 20) behind the LNB (LO 10 600 MHz): the whole IF,
950–2150 MHz, in 86 slices of 20 MS/s × 2 s; the busy 12.32–12.37 GHz block
again in 9 slices of 8 MS/s × 4 s; the DVB-S2 carriers and some others
recorded on their own. Every slice through `decsat scan --decode`, then the
failures one by one. Captures stayed in the scratchpad.

**New in the CLI**: `decsat record` (raw IQ from the HackRF to `.cs8`, one
frequency or `from:to:step`, named for `scan`/`decode` to read back);
`decsat scan --decode` (after Identify, a lossless second pass runs each
carrier's decoder over the capture — TPC 2964, FastLink, DVB-S2 → IP; DVB-S
and the K = 7 Viterbi for "possibly DVB-S"; the Viterbi as a probe on any
other plain BPSK/QPSK; TDMA bursts skipped — and prints every report);
`decode --bbframes file` (good BBFRAMEs kept: u16 LE length, PLS MODCOD,
the frame); the report's DVB-S2 lines (FEC counts, per MODCOD, the BBHEADER,
the GSE variants, non-IP protocol types); `--offset -1.2e6` now parses.

**What is there, and what decoded** (215 carriers in the wide sweep; 145 in
the 12.32–12.37 GHz block, whose 8 MS/s × 4 s pass the counts below are from):

- **TPC 2964**, QPSK, 43.6–682.7 kS/s, 66: 60 decode cleanly. Of the other
  six, the two ~700 kS/s MEL trunks are below threshold at this dish (raw
  BER 5 %, ≈4.3 dB; the decoder clears every frame at 5 dB in AWGN), one at
  6.2 dB loses 30 % (marginal), one at 16 dB loses frames in bursts
  (something overlaps it), one loses frames to slips, and a 785 kHz-wide
  signal Identify named TPC never shows the decoder its UW (earlier passes
  called it plain QPSK at 160–200 kS/s: unresolved). Payloads: Comtech D&I++ with G.728 voice (the
  43.6 kS/s family), 64 kbit/s timeslots in HDLC packets (85–682 kS/s),
  idle fill. A 298.7 kS/s one, focused at 2 MS/s: 3019 of 3021 frames in 15 s.
- **Q-Flex FastLink**, 95 kS/s, 38: all lock and decode (one lost 72 of 256
  codewords after a slip), Paradise framing and the TDM multiplex, several
  with skewed rails. The faster FastLinks (about 121, 190 and 285 kS/s — the
  plan's "CAP" links) decode at the LDPC (e.g. 336 codewords, BER 1.9e-5)
  but their payload is not recognised yet.
- **Q-Flex Viterbi K = 7, rate 1/2**: 7 links, 64 kS/s and 9.6 kS/s, decode
  (timeslot HDLC or idle fill). The DVB-S decoder on them finds rate 1/2 but
  no TS — right: they are not DVB-S.
- **DVB-S2**, four ACM carriers, all GSE, multiple-stream flagged (ISI 0):
  - 2721.7 and 1200.6 kS/s: every BBFRAME good (3251/3251 in 10 s), QPSK 1/4
    and 1/3 short frames; each holds one GSE PDU of type 0005h — a mandatory
    extension header with no registered meaning (TS 102 606-1 §4.2, RFC 4326
    §5) — and an encrypted-looking body. A proprietary VSAT outbound: "0 IP"
    is the right answer, and the report now says what the GSE carries.
  - 1000 kS/s (×2): the QPSK 1/4 frames all good (DFL 128 bits: a 12-byte
    type-0001h PDU with a running counter — hub timing), the 32APSK 3/4
    frames none: Es/N0 is 6–9 dB here (spectral plateau; the FEC's pilot
    estimate agrees), below that MODCOD's ~10.2 dB. The dish, not the
    decoder.
  - No DVB-CID under the two strong ones (10 s, both search modes).
- **Not decodable here**: the plan's STDMA groups (~100 kS/s TDMA bursts,
  gaps of ~20 ms), the Comtech 8PSK at 477.9/238.9 kS/s (still open),
  BPSK at 0.13–1.5 MS/s with no K = 7 code, transponders wider than the
  HackRF's 20 MHz (no noise floor in a capture: 11.6, 12.39, 12.53,
  12.61 GHz…), and receiver artefacts: spurs at whole-MHz and 1.875 MHz
  steps of IF, and 400–650 kS/s "carriers" around 12.13–12.32 GHz whose IF
  offset steps with the tuning.

**Fixed** (each from a live carrier):

- **PSK demodulator readouts from zero** (`psk.rs`): MER and lock coherence
  were smoothed 0.3 a block starting at 0, so a first look arriving as one
  block (any short capture) read 0.3 × the truth: 43.6 kS/s TPC carriers at
  9.3 dB showed 2.8 dB and "not locked" — and the same carriers 6–9 dB lower
  than their BER implied all through the first survey. Now seeded from the
  first block and smoothed over ~2000 symbols whatever the block size.
  Full multi-ring decisions wait for the second block (from the first, a
  16QAM test false-locked).
- **DVB-S2 Es/N0** (`demod.rs`): the VFO showed decision-directed MER, which
  read 12.2 dB on 32APSK frames at 6–9 dB; it now shows the pilots' known-
  symbol estimate when there are pilots.
- **Identify**:
  - a HackRF spur in a narrow VFO was "VFO too narrow … 596 kS/s" (its 99 %
    band is mostly noise, its noise-bandwidth rate divides by a plateau a
    tone has not got): half the above-floor power within ±8 bins of the
    peak is now a tone, centred on the peak;
  - "2-ring" on QPSK at 6–9 dB: two clusters cut a single noisy ring's
    spread 2.75–4×, past the 3× bar; a ring structure now also needs a dip
    between the rings (midpoint density under half the sparser ring's);
  - narrow carriers whose s⁴ line LNB phase noise smears under the bar
    ("modulated"): a QPSK lock seeded from the s⁴ peak that holds
    (coherence ≥ 0.3; 8PSK scores ~0 against QPSK points) names QPSK, and
    TPC/FastLink detection then runs — a 16 dB TPC carrier and a FastLink
    called "2-ring" are named again;
  - **TDMA bursts**: in-band power in 64-symbol blocks, ≥ 2 runs of blocks
    3 dB under the median: "QPSK in bursts — a TDMA carrier (on 91 % of the
    time, gaps of ~18 ms, 9 dB deep)" instead of "possibly DVB-S";
  - DVB-S2 needs a quarter of its header candidates confirmed: a TPC's UW
    every 1482 symbols gave 2–3 chance confirmations among dozens, read as
    ACM with a different MODCOD each;
  - proprietary-S2 header spacing at least 1000 symbols (an STDMA group's
    burst preambles recurred at 59–72, "PLHEADERs every 72 symbols").
- **Carrier list cap** 48 → 128: two 20 MHz Ku slices held more SCPC
  carriers than that and lost real ones.
- **GSE**: with no IP from any variant, the standard reading's protocol
  types are counted (they were only once IP had been seen), and the S2
  status names the commonest ("0 IP packets · GSE protocol type 0005h").
- **Reports before the decoders had finished, and a 3½-minute exit**:
  `decode` and `scan --decode` printed 3 s after a file ended, while a slow
  FEC thread still had a queue; the exit then waited for it (lossless VFOs
  work their queues through). Each VFO now counts the blocks and frames
  handed to it and its FEC/CID thread and not yet done
  (`fec::Pending`); `Engine::settled()` is "file ended, nothing pending",
  the CLI waits for it (capped at 2 min, with progress notes) and then
  discards the VFOs. The slow one: the **TPC 2964 structure search** ran
  over all 432 structures × its 16-frame window on every frame until
  something fitted — on a weak 685 kS/s trunk that never fits, ~460
  searches a second. It now searches every 4 frames while the window
  fills, then once a window.
- **Payload search**: MPEG-TS needs six packets whose sync byte checked —
  three chance 0x47s 188 bytes apart (~4 % of 4-frame looks over all the
  descramblings) plus coasted misses had out-scored D&I++ on a voice carrier.

**Slips come with the HackRF's tuning.** Some carriers slipped in the
8 MS/s block captures (identical before and after these changes, and again
in clean captures with the machine idle). The same carrier, retuned:

| carrier | HackRF centre (MHz) | result over 4 s |
|---|---|---|
| FastLink 95 kS/s, 12326.49 GHz, 10.9 dB | 1720 (20 MS/s) | 248/256 codewords, 1 slip |
| | **1725** (20 and 8 MS/s) | **170/256, 4–5 slips** |
| | 1726, 1727.5 | 255/256, 254/256, 1 slip |
| TPC 298.7 kS/s, 12341.28 GHz, 8.4 dB | 1740, 1744 | 803/805 frames, 2 slips |
| | **1743** | **731/805, 18 slips** |
| | **1742** | **no UW at all** |

Not the decoders, nor the carrier's offset in the band: something in the
HackRF's synthesisers at particular centre frequencies (its LO and IF are
chosen per tuning). If a carrier slips, retune a few MHz. Worth a closer
look: measure the HackRF's close-in phase noise against centre frequency.

Open: the faster FastLinks' payload; the Comtech 8PSK; what the BPSK
carriers are; a live DVB-CID (none found yet); TDMA burst demodulation;
the HackRF tunings that slip.

## Two Q-Flex calls sharing channels: the "monster" voice (2026-10-11)

Rory, live on ASA211 ESP-ML (12327.868 MHz, rails skewed) and ASA197
TMR-ML: the call voice "sounded like a monster". The GUI had built the
stream from channels 0, 1, 2, 3 and was decoding noise with speech's sync.

- **Two calls, not one.** At both sites channels 1 and 2 carry fill (and
  speech) in *both* halves of their 4 ms subframes, each half at its own
  offset; 0, 3, 4 and 15 in one half, as on qflex_3. Read from the TMR-ML
  fill: call A = 1 (positions 0–15, offset 54) and 3 (16–31, 54) for the even
  bits, 4 (0–15, 55) and 2 (16–31, 55) for the odd; call B = 15 (0–14, 128)
  and 1 (16–31, 126) even, 2 (0–15, 127) and 0 (16–31, 127) odd — two 16 kbit/s
  G.728 streams in the same layout as qflex_3's (odd members one bit after
  the even, a spread of 2). On Rory's 2 s ESP-ML recording both carry speech
  at once (voicing 0.56 and 0.57 against 0.38 for the confirmed qflex_3 call;
  same talker on both, cross-correlation 0.68).
- **Why the growl.** `calibrate` kept one run per channel, so call B's
  halves of 1 and 2 were never placed and call B could not be built; and an
  odd-bit channel sees only the fill count's bits 2 and 0, which repeat every
  8 codewords, so its offset is known only modulo 80 — channel 2's call-B
  half (127) read 47 and channel 0 (127) read 47, within `group`'s ±40 of call
  A's 54. The GUI's {0, 1, 2, 3} was call A's even bits with call B's odd
  bits: sync bits right (so "speech"), codewords wrong (voicing 0.35).
- **Fix** (`tdm257`): `calibrate` returns every run of 14–20 fill bits per
  channel (runs sharing most bits are one half at two offsets); `group`
  wants a stream's offsets within `SPREAD` = 4 (every call seen spans 2), so
  an 80-ambiguous member cannot join the other call; a stream that frames
  speech is chosen and only the ways sharing a channel's half with it go,
  so the second call is chosen too; the voice output is one call at a time
  (`FLOOR_HOLD`, 1 s), `stats.voice` the chosen calls together. TMR-ML's
  fill now groups into exactly the two calls (with their 15/16-bit window
  variants) and no mix; Rory's ESP-ML call through the receiver with those
  placements gives both calls, speech out (voicing 0.57); the qflex_3 call
  decodes as before (248 blocks). Test: two calls on the ASA211 layout,
  found from silence, each chosen from its own halves.
- Decoded: `tests\qflex-voice\esp-ml-call-A-ch1234.wav`,
  `esp-ml-call-B-ch0-1-2-15.wav`, and the 0.9.0 mix
  (`esp-ml-monster-as-0.9.0-mixed.wav`) for comparison.

## The faster FastLinks: framing, their IP and their calls (2026-10-11)

Rory: "crack the faster fastlink payloads". Three Q-Flex FastLinks on the
CAP sites, recorded together (HackRF at 1760.6 MHz, 4 MS/s, 60 s — a
tuning that does not slip): 12359.750 GHz at 185.5 kBd (strong, BER
6e-5), 12358.886 at 276.0 kBd and 12361.004 at 366.5 kBd (both weak,
channel BER 3–4 %, yet 99.9 % of codewords decode).

- **Rates.** FastLink carries 16 384 data bits a frame of 11 538 symbols:
  263.4, 391.9 and 520.4 kbit/s — exactly 41, 61 and 81 × 6.425 kbit/s,
  as the 95 kBd links' 134.925 kbit/s is 21 ×. So: k × 128.5 kbit/s of
  data (k = 2, 3, 4) plus the same 6.425 kbit/s of overhead.
- **Paradise framing at every rate** (`paradise.rs`): the same four
  overhead octets (FAW 98h/D8h, ESC, CTRL 10h/30h with the multiframe bit,
  ESC), but 40, 60 and 80 data octets to an overhead octet — groups of
  1312, 1952 and 2592 bits (the fold of the 185.5 kBd link's data showed
  exactly nine fixed bits a 1312-bit period). `EscRx::with_octets`, the
  stats say which; the payload search tries 20/40/60/80 and reports
  "40 data octets an overhead octet (2 × 128.5 kbit/s)".
- **Re-probing** (`payload.rs`): the 276 kBd link's rails line up
  differently from one stretch to the next (straight, then the odd bit a
  pair early, after its slips — the FAW is perfect within each). The
  search ran once and kept the first answer: 474 groups, 21 losses. Now
  the chosen Paradise framing lost for two modem frames sends the payload
  back to the search: 11 654 groups, 8 losses.
- **The multiplex** (`tdmfast.rs`): the same 257-bit frames as the 128.5
  kbit/s links, the alignment word on alternate frames, at k times the
  frame rate — a frame every 1.00, 0.67 and 0.50 ms. All three align
  (59 910, 87 954 and 119 881 frames). `FastRx` finds the frames and meters
  activity (bits changed from 20 ms before); CLI and GUI show both.
- **Inside, at first: not cracked** (kept as found; the next part
  undoes it). The frames are not the 128.5 kbit/s sites'
  sixteen 8 kbit/s channels. Idle, they are mostly constant bits with a few
  changing fields, repeating every 10 ms (k = 2), 4 ms (k = 3) or 2 ms
  (k = 4); on the 2 × link, 64-bit blocks of two 4-bit fields, a marker
  that steps at mid-frame (1000 → 0100) and ones. The G.728 silence fill
  of the other sites is nowhere (searched: every channel split and stride,
  inverted and bit-reversed). On the 2 × link the even and the odd bits
  each look like a 128.5 kbit/s multiplex half a frame apart (identical
  per-position statistics after a 129-bit shift), but bit-splitting the
  3 × link gives nothing, so that is not taken as the rule. The 60 s held
  no traffic (0.2–0.4 % of bits changing on the two clean links), so which
  fields carry voice, and in what code, cannot be told yet. A recording
  while a CAP site talks would — and the make of its voice equipment.

`decsat payload … --inner-out f` writes the data inside the Paradise
framing (0/1 bytes) for this kind of work.

**Cracked** (`tdmfast.rs`), on a 15-minute recording of the three links
(1760.6 MHz, 4 MS/s, 900 s), with traffic in it:

- **The QPSK pairs come swapped.** Every pair of bits of the data stream
  is in the other order from the one the multiplex was built in (which
  pairing is not known, so both are tried, by absolute bit parity).
  Swapped back, every 257-bit frame has one layout: octets from bit 1,
  each kind at fixed places. All that was "a few 4-bit fields and a
  marker" above was this, read across the wrong pairs.
- **A 64 kbit/s HDLC timeslot.** 8-bit runs at fixed places in the frame
  (2 × link: bits 17, 41, 81, 105, 145, 169, 209, 233 — eight a frame;
  4 ×: 25, 57, 153, 185 — four), HDLC on an inverted line. Its frames are
  bridged Ethernet with 802.1Q VLANs: the site's IP network — SNMP to
  four managers, traps, OSPF, NTP, ICMP, PPPoE discovery, ARP, a UDP
  multicast to 233.71.15.x:50000. The UDP payloads are encrypted: their
  checksums fail while the HDLC FCS and the IP header checksums hold. B:
  2633 frames, 2503 IP packets in 16 flows; C: 2782 frames. Found blind:
  idle HDLC is flags, 10000001 inverted, so a run of it reads as one of
  that's eight rotations most of the time, and the rotation steps
  whenever a frame has gone by — runs showing all eight rotations, in 60 %
  of frames, changing 25 times in 10 s; confirmed by three good FCS. The
  frames go to the IP stage (IP view, PCAP).
- **The calls: G.728 in a second 64 kbit/s timeslot**, the HDLC one's
  runs 16 bits earlier (2 ×: bits 1, 25, 65, …, 217) or 24 (4 ×: 1, 33,
  129, 161) — octets laid out [s v v v v x x x]. Each pair of `v` bits is
  a 16 kbit/s G.728 stream, octet after octet, as on the CDM-600L's D&I
  timeslot: on the 2 × link bits 2–3 and 4–5 are two calls with the same
  talker in them (the decoded audio correlates 0.48, 2.6 ms apart: the
  same speech coded twice, as at the 128.5 kbit/s sites' "two calls");
  on the 4 × link bits 2–3 are an open radio channel, speech over a
  noise floor most of the time (216 s of speech in 15 min). Idle, the
  pairs send the equipment's silence fill (10-bit units 1111 cccc 11, the
  160-bit cycle that made the fields "repeat every 10 ms"). Speech shows
  clear harmonics on both links. The `s` and `x` bits change rarely
  (signalling, likely). G.726-32 on the whole nibble had given white
  noise; the four other ways of splitting the nibble (by bit, by nibble,
  by half frame) give nothing.
- **Found blind**: once the HDLC timeslot is known, its runs are moved
  along the frame by whole octets (each place clear of it), a G.728
  framer on every pair of neighbouring bits of each — 4 places at 2 ×, 10
  at 4 × — and a pair is a call when it has sent 3 blocks of the
  silence fill: the count stepping a codeword, which a wrong pairing
  cannot fake. Speech's sync bits alone are not enough: a pair one bit
  off sees them too (on the 4 × link bits 1–2 read as "talking" as long
  as 2–3 did). The search stops 30 s after its last find. B: both calls,
  40 s of speech each; C: its one call. The call speaking is the one
  heard; it goes through the same player and recorder as the 128.5
  kbit/s calls (`CallVoice` now takes any 8 kHz stream with its clock),
  and `decsat payload --voice-out` writes it to a `.wav`.
- **Still open** (then): the 3 × link (276 kBd, weak) aligns but shows no
  HDLC timeslot — at 1500 frames a second a 64 kbit/s timeslot is 5⅓
  octets a frame, so its layout must span frames; and with no HDLC to
  start from, its voice is not looked for. Cracked the same night — below.

## The 3 × FastLink: superframes of three frames (2026-10-11)

Rory: "crack the 3x link too". The 276 kBd link (12358.886 GHz, weak:
channel BER 4 %, 148 TDM losses in 15 min) from the same 900 s recording.

- **A 2 ms superframe.** Pairs swapped back and aligned, almost every bit
  repeats three frames later and changes from one frame to the next: the
  layout spans three frames — the fewest holding whole octets of a
  64 kbit/s timeslot (16/k a frame; 16 a superframe). In it, 24 groups of
  four octets, three kinds in turn: [V h H V], [h H h h], [h h h h] — the
  2 × link's [V C H V], [C H C C] and a third. H: the HDLC timeslot (16
  octets: 2668 good frames in 15 min, the same kind of site IP — SNMP to
  the same managers, the 233.71.15.x multicast, OSPF). V: the voice
  timeslot, two octets before each H as at 2 ×: two G.728 calls on bits
  2–3 and 4–5, the same talker on both again (38 s of speech each, fill
  the rest). h: idle timeslots, flags; one of them (X.b with the third
  kind's c octets) gave 7 frames in 120 s — a second, near-silent HDLC
  timeslot, not decoded. (Wrong — see the next entry: an artefact of the
  analysis.)
- **Placing superframes** (`tdmfast::Phaser`): the alignment column — the
  word on even frames, and on the odd ones a 21-bit pattern with two
  zeros — repeats every 42 frames (14 × 3). Learnt over 24 cycles, then
  followed; after a loss of lock the frames are placed again once a cycle
  matches it to a bit (a cycle 14 or 28 frames off differs in only 4 of
  42 bits, so any other placement must miss 3 more).
- **The HDLC search, superframe by superframe** (octet-aligned now): idle
  octets here are flags too, and change rotation at every slip (all at
  once, ~25 times in 10 s of the weak link), as much as the timeslot does
  with traffic (23–45). Changes in more octets at once than one timeslot
  has (and half the flag-like ones) are slips and not counted; the
  threshold drops from 25 changes to 12. What is left goes on trial
  together, beside groups of octets that change together, all deframed at
  once until one gives three good FCS. (Grouping alone fails: at 2 × and
  4 × the timeslot idles part of the time with something other than flags
  — inverted, eleven ones and three zeros — where bit errors make most of
  an octet's "changes", so its octets do not change together.)
- **All three** run unattended on the 15-minute recording: 3 × 2668 HDLC
  frames and two calls (38 s each), 2 × 2633 and two calls (40 s each),
  4 × 2782 and its open channel (216 s) — those two as before. A
  synthetic 3 × link with a slip of two frames and two bits at 16 s keeps
  decoding after it (`a_3x_link_in_superframes_…`).

## The 3 × link's "second HDLC timeslot": none (2026-10-11)

Rory: "decode the second HDLC timeslot on the 3x link too". It is the
first one's octets moved four earlier (as the voice timeslot is two
earlier) — X.b with the third kind's c — and in the analysis superframes
it gave 24 frames with a good FCS in 234 s: the same bridged Ethernet,
VLAN 3155 SNMP and the 233.71.15.1 multicast, and NTP on VLAN 1118.

All 24 sat at the very ends of the analysis' 2 s segments (superframes
965–999 of each thousand; times all ending .2–.3 s), where the segments,
re-aligned one by one, were stitched: most likely a slip late in a kept
segment moved the real timeslot's octets onto the second's places for a
frame or two (none of the 24 is a frame the first timeslot gave). In the
receiver's own continuous, placed superframes — dumped and deframed in
Python too — the second timeslot gives no frame in 15 minutes, nor does
any other move of the HDLC timeslot (nine sets): inverted flags,
changing phase now and then, and nothing between them. A second data
channel configured and unused while recorded.

So it is decoded the only way it can be — watched: `tdmfast` now deframes
every move of the HDLC timeslot round the superframe (which also lets the
voice search find a timeslot past the superframe's end) and takes one as
"HDLC timeslot 2" (3, …) once it gives three frames of at least an
Ethernet header's length with a good FCS within two minutes, holding them
until then so none is lost; its frames join the first's in the IP view and
the PCAP. CLI and GUI list it. On the recordings none is found at 2 ×, 3 ×
or 4 ×; a synthetic 3 × link with a frame every 0.75 s in that timeslot
has it found and every frame given out, through the slip.

## Calls kept through the fill restarting (2026-10-11)

Rory's auto-record of an ASA211 ESP-ML call (`…-fastlink-active-1791722375.bin`)
decoded with 0.9.1 from the fill alone: call A {1, 2, 3, 4} and call B
{0, 1, 2, 15} chosen together, clean speech (the same audio on both), and
1.6 s of noise-like bursts after it on both circuits alike — real sound,
decoded right. But each talk spurt restarts the equipment's fill at a new
phase, the channels' places are measured again one channel at a time (13.10,
13.15, 13.75 s), and a regroup with a call's old and new places mixed left it
out of the ways: it was dropped and found again on its next words, a 40 ms
block late (three times in 21 s).

Now (`TdmRx::regroup`) a chosen stream stays through every regroup — speech
proved it, and a fill restart moves all of a call's channels alike — and only
ways sharing none of its channels' halves are tried beside it. A chosen
stream goes when it frames neither speech nor fill for `LOST_BLOCKS` (2 s;
`g728::Framer::blocks` counts every block), and the ways are tried afresh.
On the recording both calls now stay chosen throughout; the qflex_3 call
decodes as before. Tests: two calls through a fill restart with channel 15
measured late (fails on the old rule), and a stream whose channels turn to
noise let go.

## The PH-ML radar feed: BSC blocks of track reports (2026-10-11)

Rory: "PH-ML 570 radar feed decoding", 9.6k (a CDM-570L, 12328.083 MHz;
his 37 s VFO recording `decsat-PH-ML-…-118343Sps-…cf32`, 12:40:54 UTC).

- **Modem:** QPSK 9.6 kS/s, K = 7 rate ½ (channel BER 5e-4 at Es/N0
  11 dB), V.35 (taps 3, 20) — the existing Viterbi chain. The payload
  search had called it "idle fill between bursts": bursts every ~110 ms on
  a line that sat at 1 or 0 between them, flipping level after each burst.
- **NRZI**, then **IBM bisync (BSC)**: NRZI-decoded (a 0 changes the
  level) the bursts are `SYN SYN SYN` (32h, EBCDIC), `SOH 0C 40`, `DLE
  STX`, text, `DLE ETX`, a 16-bit BCC, `PAD`, sent LSB first. The search's
  differential decoding is NRZI read inverted, so `bsc::BscRx` takes either
  polarity from the SYNs. **BCC** = CCITT CRC preset 0 (CRC-16/KERMIT) over
  heading + `DLE STX` + text (stuffed `DLE`s once) + `ETX`, found by
  brute force over polynomials, presets and spans: 295/296 blocks, the
  other cut off by the file's end; 25 texts had `DLE DLE` and checked.
  Classic BSC's CRC-16 and its span are tried too. New `Format::Bsc`.
- **The messages** (`modem::radar`, fields documented there with the
  evidence): one a sector, 32 a scan, header `FD`, sector, time of day in
  1/128 s (12:40:57.1 = the file's time + 3 s); then 24-byte track
  reports `FF 18`: status, track number, x/y signed 1/64 NM (bearing falls
  in the message's sector; displacement against ground speed fits 64.1
  units a NM), velocity ≈ 11.25 kt a unit, Mode C as BCD with a flag digit
  (`3380` = FL380, counting down through a descent), Mode A octal in 12
  bits, ground speed BCD kt, a field falling with range (amplitude?), and
  the plot's time after north in 1/128 s (472 a scan: 3.6875 s, 16.3 rpm).
  Once a scan a `19 40` message to another address. Ranges to 247 NM; an
  arrival on a 3° final at 7.7 NM north is at FL023, so the radar is near
  a threshold. Not ASTERIX; the format's name is not known.
- **Result:** 321 blocks, 145 track reports, 15 tracks (FL008–FL390,
  98–514 kt), 1 bad block (a real error burst: sector 24 missing, the one
  "sector jump"). `decode --decoder viterbi|cdm570` prints the tracks;
  with `--out`/Record a CSV, one line a report (`…-radar-….csv`);
  `decsat payload x.bin --frame-bits 4096 --radar-out f.csv`.
- **GUI:** a Radar feed card and **📡 Radar scope** — a PPI (north up,
  rings, the last sector, trails, 1-minute leaders, Mode A/FL/GS labels,
  hover details, wheel zoom, drag pan) and a track table.
  `--radar-scope` opens it in unattended runs.

**CDM menu** (Rory): the decoder menu has one "Comtech CDM modem" entry
and a "CDM model" menu under it: CDM-600/600L data (TPC 2964 → IP/TS),
CDM-600L voice (the D&I++ G.728 preset), and the new **CDM-570L** preset
(`DecoderKind::Cdm570`, key `cdm570`: the Viterbi chain, as the voice
preset runs TPC 2964's). Generic Viterbi and FastLink stay top-level.

Open: the PH-ML carrier was not found by carrier detection live (Rory
added the VFO by hand); some Q-Flex sites may carry radar, audio and
ADS-B too (Rory).

**Doubled tracks** (Rory, live at 13:26 UTC, HackRF 20 MS/s): the scope
showed pairs 008A/048A, 011D/051D, 0071/0471 — each pair one aircraft
(same squawk, level within 200 ft, speed, place; their reports summing to
one a scan). Bit 10 of the track number comes and goes scan to scan: a
flag, not part of the number (also set on the recording's 247 NM track in
decline, 05CE). Tracks are now keyed by the low 10 bits; the flag bits are
kept (`Report::number_flags`, CSV `track_flags`, the tooltip counts the
flagged reports). Its meaning is open.

**ASA251 ML-BN "won't decode"** (Rory, 21 s VFO recording, 12346.375 MHz):
9.6 kS/s QPSK, K = 7 ½ (channel BER 1.5e-3), V.35, NRZI — and HDLC
flags only, the whole 21 s (one 32-bit error burst at 2 s): the link was
up and idle, nothing to decode. The payload stage had said "64 kbit/s
timeslots in HDLC packets" (the multi-radio format takes any inverted
line with 512 flags). Now: plain HDLC on the inverted line is a search
candidate too, and the label says "HDLC on an inverted line: idle flags
only so far" until timeslot packets come; frames with a good FCS on such
a line go to the HDLC stats and the IP stage. Test: HDLC frames on an
NRZI line under V.35.

## The fill read askew passed for speech: "blowing out my ears" (2026-10-11)

Rory's ASA204 ML-NBK (12344.3695 MHz, `…-fastlink-1791725778.bin`, 12 s): the
two-call layout again (call A on 1, 2, 3, 4 at offsets 16/17, call B on 0, 1,
2, 15 at 64–66), both calls idle throughout — yet 3.9 s of "speech" on call A,
played as full-scale noise. The way chosen was call A read one subframe
position askew (windows 31+16, 15+16 from runs of 17: a neighbouring bit
matching the fill by chance): two stray bits in every 64 stream bits. They
break the fill's pattern, so `fill_at` fails; and recurring every 320 bits
they land on one sync place in 32 codewords — exactly speech's sync marker —
so `sync_at` passed.

- `g728::Framer::repeats`: a block whose second 20 ms repeats its first
  (≥ 90 % of its bits) is never speech. The fill, however it is put
  together, repeats (≥ 98 % on ML-NBK, askew or not); speech never does
  (≤ 62 %, mean 55 %, on the ESP-ML call).
- `TdmRx`: of ways that start talking together (readings of one call whose
  windows differ by a bit), the one that framed the most fill is chosen,
  then the one carrying the most bits. The ESP-ML and qflex_3 calls now take
  the full 16-bit windows where 15-bit ones had won by list order (one bit a
  subframe left unknown): voicing 0.44 → 0.46 and 0.54 → 0.58 (voiced frames
  20 → 23 % and 37 → 47 %).
- ML-NBK now decodes no speech, nothing played. Test: the fill with two
  stray bits in 64 is not speech (fails without the veto). A synthetic
  two-call idle site did not reproduce the askew runs (the chance matches
  depend on the real status octets and fill phases), so no TDM-level test.

## The other radar feeds: ASTERIX in HDLC (2026-10-12)

Rory's older (2025) carrier plan names the band's radar carriers; he
asked for them recorded, warning the plan is old (its rates are: ASA251
and PH-ML were 64k there, 9.6k now). Surveyed at 20 MS/s, then 60 s at
8 MS/s on each group. Of the plan's radar slots, five carry a 64 kS/s
CDM-570 carrier now: CB-BN (12335.22), MTBB-BN (12346.63), the slot the
current plan calls ASA119 AD-BN (12346.72), and two the current plan
lacks, 12355.08 and 12359.44. SY-ML (12346.46) is empty; the others are
TPC carriers or wide trunks now.

- **Modem**: the `cdm570` chain as it was — K = 7 ½, V.35, differential —
  and HDLC on an inverted line (FCS-16), 7–9 frames a second.
- **Frames**: UI frames (address `02h`/`03h`, control `03h`) of
  **ASTERIX** data blocks: category 034 service messages (a sector
  crossing every 11.25°, sent in pairs every 0.25 s; a north marker once a
  scan, sent ~0.5 s before the beam gets there with the time it will)
  and category 048 target reports (polar and Cartesian position, Mode
  3/A, flight level, Mode S address and MB data, track number, velocity,
  track status). Over 1496 frames every block parses to its exact length
  under the standard UAPs. Times of day agree with the clock (13:56 UTC).
- **Radars** (SAC 4; I034/120 in the north marker): SIC 20 Adelaide
  Airport (its place in the north marker, 30 m up), SIC 19 near Canberra (903 m), SIC
  18 Cairns Airport (on the 12355.08 carrier), SIC 63 Darwin (12359.44),
  SIC 48 (MTBB, no position sent). Scan 3.95–3.97 s (I034/041). Each
  shows a few fixed Mode S transponders (squawks 77xx, FL −10, ≤ 10 kt,
  addresses 7CF7xx) — test or monitoring transponders — and, past
  midnight local, little traffic: FL351/FL380 jets at 475 kt.
- `decsat-modem::asterix`: a UAP-driven reader (fixed, extended,
  repetitive, compound, explicit items) for categories 034 and 048; a
  block is taken only if its records end exactly at its end. Its messages
  are `radar::Message`s, so the BSC feed's track table, CSV log and scope
  serve both: `Report` gains Mode S address and callsign (I048/240, none
  seen yet), `coasting` (I048/020 TYP 0; BSC from its status), signed
  flight levels and a variable `raw`; `Message` gains the radar's SAC/SIC,
  site and rotation period and the tracks ended (I048/170 TRE, which drop
  from the table); `RadarRx::message`/`asterix` take them. CAT 034 sector
  crossings map onto the scope's 32 sectors (the beam at azimuth A at the
  message's time); the north marker sets no sector (its early time would
  count as a jump). Track ages are taken round midnight the short way
  (a slightly older message no longer drops a track as a day stale).
- Engine: HDLC frames that are not IP go to the radar stage when they are
  ASTERIX; the radar card and scope show the radar's SAC/SIC and place,
  the scope's labels a callsign when there is one. CSV gains `mode_s` and
  `callsign` columns at the end. `decsat payload` reads ASTERIX too and
  writes the frames that check as hex (`--hdlc-out`, `--bit-rate`).
- PH-ML (BSC) decodes as before (321 blocks, 145 reports, 15 tracks).
- Open: the `payload` re-run of a Viterbi `.bin` picked the wrong
  descrambler hypothesis on two of these recordings (their first 0.25 s
  idle flags only) — 2 frames where the decoder had found 500. The live
  decoder is right; the re-run's search is not.
## Slow carriers no longer dismissed as narrow lines (2026-10-12)

Rory's live GUI never offered the PH-ML radar carrier (9.6 kS/s) as a
carrier — he added its VFO by hand. At 20 MS/s with the 4096-point FFT
(4.9 kHz bins) its 11 kHz of width is two or three bins, the same as a CW
tone with its Hann leakage, so `detect_carriers` flagged it `narrow`:
neither the band view's carrier marks, "claim carriers" nor Align-to-plan
took it. The CLI's `scan` identified it only because it gives every narrow
line a 20 kHz VFO regardless, and an 8 MS/s capture (2 kHz bins) found it
outright.

Now the front end, at each carrier-detection tick that reports narrow
lines, takes one finer spectrum of the current block (16× the bins:
65536 points at FFT 4096, 305 Hz bins; capped so the 40 ms block still
holds seven segments) and `carriers::resolve_narrow` looks at each line
again: its region above the local floor, and the longest run within 6 dB
of its top. A tone's run is its main lobe, two or three bins at any
resolution; a carrier's is its flat top, Rs (1 − α) wide — 30 bins for
9.6 kS/s. A line that resolves is reported as a carrier, with its centre,
99 % width, ENBW symbol rate and S/N measured at the fine resolution
(`occupied_window` and `enbw_bins` now shared with `detect_carriers`).
Tests: a 9.6 kS/s carrier at 20 MS/s resolves (rate within 10 %), a CW
stays a line, neighbours are untouched. On a survey capture the PH-ML
carrier reads "9.5 kS/s, 11.3 kHz wide, 13.8 dB" where it read "10.3 kS/s
(narrow)", and the band's two spurs stay narrow lines. Cost: a few ms
every 500 ms while narrow lines are present.

## D&I++ with two timeslots: the 87.2 kS/s TPC carriers (2026-10-12)

Two TPC 2964 carriers at 87.2 kS/s (12345.46 and 12345.82 GHz, not in
Rory's plan by name) decoded (470 frames in 8 s, BER 2.5e-3 and 3.5e-3)
but "payload not recognised". Their data rate, 87 228 × 2 × 2223/2964 =
130 850 bit/s, is 2 × 64 kbit/s × 46/45: D&I++ with two timeslots. Under
the V.35 descrambler (taps 3, 20) the D&I++ header turns up every
**5888** bits — two 2944-bit blocks — not every 2944, so `DiPlusRx`,
which wanted three consecutive 2944-bit frames, never aligned. Folding
the descrambled bits at 5888 on the header (177 frames, both carriers):

| bits        | first block                  | second block (2944 on)       |
|-------------|------------------------------|------------------------------|
| 0–23        | `000001010111101000111 000`  | `000001010111101000111 111`  |
| 600, 1186, 1772 | `0111111111` (constancy ≥ 0.99) | the same               |
| 2358        | one or two bits varying      | the same                     |

So the header is a 21-bit sync and three bits that are `000` in a
frame's first block and `111` in its second (what they say with more
timeslots is not known: a counter, flags…), and a frame of n timeslots
is n blocks in the same 45 ms. The deframer now matches the 21 sync bits
only, counts the blocks between first blocks (`DiPlusStats::timeslots`,
with `frame_n` to make such frames for tests), and the engine takes that
count over the one it derives from the data rate. The timeslots' bytes
are taken to alternate (the engine's `dandi_bytes` already split them
so): read that way, channel 1 of both carriers carries the G.728 silence
fill in bits 2–3 like the one-timeslot CDM-600L carriers (−15 dBFS,
silent), which a wrong interleave would not give; channel 2 (−14.6 dBFS)
shows no coding the E1 stage knows. Both carriers: "E1 timeslots, Comtech
D&I++ framing … Comtech D&I++, 2 × 64 kbit/s" now; the second has 19
slips and 45 UW misses in 8 s (HackRF tuning, as before).

## Overnight survey, 2026-10-12 (Rory: "use the HackRF and look around")

Receive only, LO 10 600 MHz, Rory's LNA 40 / VGA 20 at first. The busy
block in two 20 MS/s × 8 s slices (HackRF at 1729.146 — his saved tuning —
and 1744 MHz), the whole IF 960–2144 MHz in 75 slices of 20 MS/s × 2 s
(16 MHz steps), every slice through `scan --decode`. Captures stayed in
the scratchpad.

- The busy block: 61 + 58 carriers found; everything the decoders know
  decodes — 50 TPC 2964 (D&I++ voice, HDLC timeslot packets, HDLC lines,
  idle), 31 FastLink 95 kS/s (Paradise framing and the TDM multiplex),
  the Viterbi K=7 links (HDLC on an inverted line, the PH-ML radar feed's
  BSC blocks). Two 87.2 kS/s TPC carriers were the only "payload not
  recognised": D&I++ with two timeslots (above). Nothing regressed.
- Elsewhere: the four DVB-S2 GSE outbounds (634/635, 650/650 and 285/285
  BBFRAMEs good; the 1 MS/s ones' 32APSK frames below threshold here, as
  before), six 43.6 kS/s TPC voice carriers and three FastLinks around
  12.49 GHz, a 170.7 and an 85.3 kS/s TPC link. Not decodable: the STDMA
  bursts, the two Comtech 8PSK carriers, two 937 kS/s BPSK carriers at
  12.50–12.52 GHz (locked at 8–9 dB, no K = 7 rate and no TPC unique word),
  BPSK/QPSK at 0.8–5.6 MS/s with no K = 7 code, and the wide transponders.
- Identify named the 2721.7 kS/s DVB-S2 outbound "DVB-S2X ACM/VCM" in the
  slice where it sits at the edge: of twenty confirmed frames, nine QPSK
  codes and a 256APSK one were read once to four times each beside the
  QPSK 1/4 and 1/3 it sends. One-off MODCODs among 16 or more frames are
  now misread headers (`Dvbs2Info::modcods_sent`): left out of the S2X and
  ACM verdicts and the MODCOD list, counted as "n headers read as other
  MODCODs".
- Two 54 kS/s QPSK carriers in the busy block (12 333.58 and 12 334.02 GHz,
  Es/N0 7 dB) lock but fit no K = 7 rate and carry no TPC unique word.
  64 000 × 46/45 / (2 × 3/5) = 54 518 S/s: a rate-3/5 code (LDPC?) over a
  64 kbit/s D&I++ stream would fit the rate. Open.
- The live GUI with Rory's plan (unattended, 20 s): 51 carriers and
  2 narrow lines where the same tuning gave 3 narrow lines before — the
  PH-ML carrier is a carrier now.

**LNA 40 overloads the HackRF.** The 43.6 kS/s TPC voice carrier at
12 332.47 GHz (Rory's VFO 16, +3.324 MHz from his centre) slipped 19 times
in 8 s with 47 unique-word misses and 85 of 235 frames decoded, at
14.3 dB. The same carrier, same centre, 8 s each:

| LNA (dB) | input RMS (dBFS) | slips | UW misses | frames decoded |
|---|---|---|---|---|
| 40 | −15 | 19 (then 1 and lock lost) | 47 | 85 / 235 |
| 32 | −23 | 8 | 19 | 168 / 234 |
| 24 | −31 | 2 | 0 | 233 / 234 |
| 16 | −37 | 2 | 0 | 234 / 235 (BER 1.5e-4: the noise now) |

The ADC never clips (0.0001 % of samples past 120 of 127 at LNA 40): the
LNA and mixer compress first and their products land on carriers. So:
**LNA 24** here, not 40. The GUI's status bar now shows the input level
(`FrontStatus::input_dbfs`, smoothed) and turns it orange above
`STRONG_INPUT_DBFS` = −25 dBFS with the advice on hover.

**And some centres are still bad at any gain.** The MSO-BN FastLink
(12 342.54 GHz) at centre 1744 MHz (−1.462 MHz offset) fails 121 of 520
codewords at LNA 40 and at LNA 24 alike, evenly through the 8 s, with no
slip or sync miss (BER 8e-4 where Rory's 2026-10-11 recording of it gave
1.2e-5): at centres 1740 and 1748 (LNA 24) 5 and 4 fail. A 65536-point
look: the carrier's top is the same in both captures, the floor beside it
4–5 dB higher at 1744 (−32 dB against −37). Receiver noise that moves with
the tuning, not the signal — fractional-N spurs or noise sidebands of the
HackRF's LOs at particular IF offsets, as the 1725/1742/1743 cases before.
The remedy is still to retune a few MHz; a map of the bad (centre, offset)
pairs would let the GUI pick its tuning. Open.

## A VFO recording's filter sidelobes were "carriers"; an idle line named so (2026-10-12)

Rory's VFO recording of ASA253 MAL-BN (12346.5443 MHz, cf32 at 227 kS/s,
15 s): `decsat scan` listed eight carriers — the link, 104 dB up, and
seven 2–3 kS/s "carriers" at ±73–86 kHz. The link is QPSK 64 kS/s,
Viterbi K = 7 rate ½, V.35 scrambling, Es/N0 13.4 dB, no channel errors;
its data, descrambled, are one constant for all 15 s (the descrambler's
first 45 bits and a 16-bit glitch aside): an idle line.

- **The seven were the VFO's filter** (`carriers.rs`): beyond its edge at
  ±68 kHz, a stopband 85–120 dB down with sidelobes every 4 kHz. The
  floor is the spectrum's 20th percentile — here inside the stopband, so
  each sidelobe stood "above the floor" and the link read 104 dB. Now the
  span's edges that sit 30 dB below its 90th percentile *and* are rough
  (detrended spread over 4 dB: a filter's comb of sidelobes and nulls; an
  averaged noise floor stays under 2 dB) are left out, of the floor and
  of the search. Six VFO recordings now give one carrier each at a
  believable level (11–20 dB) and better rates (the 95 kBd FastLinks 95.3
  kS/s, was 99–100; the CDM-600 42.7, was 49.2); the 4 MS/s HackRF
  capture's 17 carriers are unchanged. A carrier 60 dB over a smooth
  floor keeps its floor (test).
- **"idle fill between bursts" said of a line with no bursts**
  (`payload.rs`): above 99 % fill the payload now reads "idle: nothing but
  its fill so far (no traffic to tell the format by)".

## A radar station's monitoring in IP: the 12335.13 MHz CDM-570L trunk (2026-10-12)

Rory's TPC 2964 VFO at 12 335.1341 MHz (a CDM-570L: the CDM-600s do no
IP, Rory says), 354 s, data recording and PCAP: "multicast radar IP data",
"just do the radar part".

- **The link** (`decsat payload … --frame-bits 2223 --bit-rate 62500`):
  62.5 kbit/s, HDLC on an inverted line, not scrambled, 15 461 frames, no
  bad FCS. **Frame Relay**: 15 425 on DLCI 100 (Q.922 address `18 41`, UI,
  RFC 2427 NLPID CCh = IPv4), 31 LMI status enquiries on DLCI 0, 5 Cisco
  CDP frames (SNAP). `hdlc_ip` already found the IP past the 4 octets.
- **The IP**: one multicast is 99 % of it, 10.230.73.244:30002 →
  233.71.15.145:4021, 43 datagrams/s (18 kbit/s); the rest is SNMP, PIM,
  ICMP and NTP of the site (not looked into).
- **The multicast is the radar's monitoring and control (RCMS), not target
  data.** A word-oriented format (lengths in 16-bit words): a 12-octet
  header (length, sending unit, kind 100/102/103/105, unit addressed,
  flags), sections of typed points (32-bit point numbers; 32-bit integers,
  IEEE floats, texts), `00 00`. A unit table every 10 s names nine units
  in Spanish — `PEDESTAL`, `UCS_SSR`, `UCS_SCR`, `UCS_RELOJ` (clock),
  `UCS_EQUIPO`, `UCS_CNR`, `UCS_SWITCH`, `UCS_UTS20S`, `PPI` — so likely
  an Indra radar (not confirmed). The pedestal sends two sets of four
  floats (~3, ~15, ~700, ~4.0: two drives? the last perhaps the scan
  period); the SSR two channels of five modules' firmware dates and
  versions; the rest states and counters. Point meanings are unknown;
  layout and evidence in `decsat-ip/src/rcms.rs`. All 15 263 datagrams
  parse to their last octet but one 22-octet configuration message (18
  copies) whose body is no chain of sections. No plots or tracks are on
  this trunk at all; the radar's ASTERIX goes elsewhere (the CDM-570
  Viterbi radar carriers).
- **Built**: `decsat_ip::rcms` (parser; `RcmsRx` takes a flow after 3
  well-shaped messages, counts repeats — every message goes 2–4 times —
  apart from updates); the IP stage tries it on every UDP datagram
  (`GseView::radar_stations`); GUI "Radar station (monitoring)" card above
  the IP card (units from the table, each with its points: value, range,
  updates; the moving floats open); `decsat pcap file.pcap [--csv] [--all]`
  reads a PCAP back (new `read_pcap`: raw IP or Ethernet); `decsat payload`
  and `decode` print the station. Checked on the PCAP, on the data
  recording, and on a synthetic TPC 2964 carrier carrying the PCAP's
  packets in Frame Relay (scratch generator, not committed): Identify,
  1349/1349 frames, the station in the GUI.
- Also: the CDM model menu says **CDM-570L TPC** (was "CDM-600 / 600L
  data") and **CDM-570L Viterbi**; an HDLC line chosen while idle no
  longer says "idle flags only so far" once frames come (this trunk began
  idle); `decsat payload` prints the given bit rate, not 128 kbit/s.
- Open: what the points mean (a manual, or watching them during a known
  event); the 22-octet message; categories 1–4 named only by guess.

**The station on the radar scope** (Rory: "add the PPI scope for that
radar stuff"). The radar station card has the 📡 Radar scope button too.
For a VFO with only a station's monitoring traffic, the scope shows the
station beside the PPI: its units (green heard within 30 s, amber quiet,
grey never heard) and the floats that move. The traffic has no targets,
no antenna bearing and no radar identity, so the PPI draws the targets
of a radar feed another VFO decodes: "Targets from", taken at once when
there is only one feed, else chosen (none: the PPI says why it is empty).
The feed's scan time is shown under the pedestal's readings: a pedestal
point near it would say the feed is this station's radar. Checked on a
synthetic mix: Rory's PH-ML recording plus a TPC 2964 carrier carrying the
monitoring PCAP at +33 kHz (both found and named by Identify). The PH-ML
targets were drawn on the TPC VFO's scope, beside the station; PH-ML scans
in 3.69 s while the pedestal reads ~4.0, consistent with their being
different radars. The unattended GUI's `--decoder` takes a list, one per
claimed carrier in frequency order (`--decoder cdm570,tpc`).

## A map under the radar scope (2026-10-12)

Rory asked for a map under a radar's PPI, the radar's place for him only.
`radar_map.rs`: OpenStreetMap raster tiles in the scope's own frame (x/y
taken as range and bearing from the radar: azimuthal equidistant, R =
3440.065 NM; each tile bent through a 4 × 4 grid), fetched one at a time
with the system's `curl` (no TLS stack in DecSAT: ring's licence and the
GPL), cached for good under `%APPDATA%\DecSAT\tiles\osm`, credited on the
map, named User-Agent. "Dark" = the same tiles with HSL lightness turned
over (every channel + 255 − max − min); CARTO's dark basemap now answers
"API key required". The radar's place: from the feed (ASTERIX I034/120),
else **Site…** in the scope, saved as `radar_site.<VFO name>` in the
user's prefs — never in the project (tests and hints use made-up places).
Map on/off, style and brightness are prefs too.

**Airports and runways on the scope** (Rory, same day): OurAirports' open
data (public domain), `airports.csv` and `runways.csv` fetched once with
curl into `%APPDATA%\DecSAT\airports\` and refreshed after 30 days
(`radar_airports.rs`, its own RFC 4180 reader). Runways with surveyed
ends solid; the rest (in Australia 1505 of 2049 have no ends) laid along
their heading (else number × 10°, magnetic) and length on the airport's
reference point, dimmer. Large and medium airports always with their
ICAO code; small ones inside 120 NM (codes inside 40); runway numbers
inside 25 NM; heliports, seaplane bases and closed ones left out; an
airport's name on hover. "Airports" switch beside the map's. Unattended
runs: `--radar-range NM`.

**Click a track** (Rory): a click on a track on the PPI, or on its number
in the table, picks it — ringed, its row lit, its details in a box in the
PPI's corner across from it (Mode S/A/C, climb or descent rate from the
levels kept with the trail, speed and heading, range/bearing from the
radar and from the nearest airport, latitude/longitude when the radar's
place is known, reports, status, raw octets). Again, ×, or a click on
nothing lets it go; a pick waits while its track is away.
`--radar-track HEX` for unattended runs. `Track::levels`,
`Track::vertical_rate` in the modem.

**Callsigns from ADS-B** (Rory chose the online lookup; the PH-ML feed
has none and no carrier with flight-plan data is known): `adsb_names.rs`
asks adsb.lol (free, no key, ODbL) every 15 s for the aircraft within
250 NM of the radar's place rounded to 0.1°, with curl, and matches each
track without a Mode S callsign to the nearest aircraft within 2 NM plus
the distance flown since, the same squawk when both have one, the level
within 1500 ft — never by squawk alone, so a recording (another sky) gets
no names. Names last 3 min after the last match, and only while the
squawk stays. Labels, the table (Callsign column), the hover tip and the
details box (type and registration) use them. "Callsigns" switch, off by
default (it asks an outside service).

## ASA176 PH-ML is the ML–PH voice trunk: four G.728 channels (2026-10-12)

Rory's TPC 2964 data recording of "ASA176 PH-ML" (12327.2241 MHz, plan
label CDM-570L, "may contain voice"): 23.17 Mbit in 23 s (start stamp
to last write), so ~1 Mbit/s — a 1024 kbit/s TPC ¾ trunk (682.67 kS/s),
not a 64k link. HDLC on an inverted line, unscrambled:
the timeslot packets of `slotpkt`, two channels, **18h and 20h**, each a
continuous 64 kbit/s timeslot (4100 packets of 44 octets in 22.6 s).

- **18h carries three G.728 channels**: bits 1–2 a fixed 1 ms pattern,
  bits 3–8 moving — G.728 sub-rate lanes in bits 3–4 and 5–6 (silence
  fill throughout) and 7–8 (2.6 s of speech at the start: pitch
  108–127 Hz, voicing to 0.56, then fill). **20h one**, bits 7–8 (fill);
  bits 1–6 fixed. Same equipment framing as the CDM-600L (sync bit 1 in
  32, inverted, counting fill).
- Missed before on three counts: `slotpkt::timeslot` took only channel
  bytes that are multiples of 10h (18h was dropped); `g728::Voice` kept
  one lane a timeslot; and `decsat payload` searched mask `FEh`, never
  bits 7–8.
- Fixes: channels are numbered byte / 8 (08h–F8h → 1–31; 10h/20h, the
  older sites', are now 2 and 4) and shown by their byte ("ch 18h") in
  the GUI and CLI (`E1View::rows`). `Voice` keeps every lane that shows
  speech or the fill, decodes them side by side and mixes them (a sample
  an octet once found: `Framer::timeline` gives silence as zeros), and
  widens when more bits start changing. Lanes are tried in pairs from the
  start of each run of changing bits (`lane_starts`): the pair straddling
  two lanes (bits 6–7 here) passed for speech whenever 7–8 spoke, its
  "pitch" the codeword period. `E1View::voice` lists each timeslot's
  lanes. `decsat payload` judges each channel's bits as the E1 stage does
  and `--voice-out` writes the timeslots' voice mixed.
- Checked: the recording (`payload … --frame-bits 2223 --bit-rate
  1024000`) lists all four channels; a synthetic 682.7 kS/s TPC carrier
  built from its bits (scratch, not committed) decodes live in the CLI
  and GUI to "ch 18h · 3 × G.728 speech in 7–8", and the 18h recording
  is the speech. Tests: lane starts, three lanes in one timeslot, channel
  18h packets, the E1 stage with the trunk's shape.
- **Also on the trunk**: packets of type 40h on channel 58h (5 % of
  them) carry IPv4 from bit 125, LSB first — 225 of 226 headers check.
  ASTERIX **CAT021** (ADS-B target reports) multicast 10.230.171.1:1028 →
  233.71.15.30:50000 (86 datagrams, ~4/s), SNMP from site devices to an
  NMS, PIM, ICMP and an ESP tunnel. Neither the packets' IP nor CAT021
  is read by DecSAT yet. CAT021 carries callsigns (I021/170): a local
  source for the radar scope's names.

## PH-BN: a sixth ASTERIX radar, and the Mode S registers in its reports (2026-10-12)

Rory's Viterbi data recording of the PH-BN carrier (12 325.82 MHz, 12.6 s
at 64 kbit/s): "large carrier with radar maybe more stuff in it", then
"ADS-B might be on the carrier".

- **The line carries one radar and nothing else.** HDLC on an inverted
  line, 105 frames, no bad FCS; the frames and the flags between them are
  99.9 % of the bits (the rest the recording's ragged ends). Every frame is
  address 03h UI, ASTERIX from SAC 4 SIC 22: category 034 (102 sector
  crossings, 3 north markers with the radar's place and a 3.961 s scan)
  and 048 (145 target reports, 39 Mode S aircraft). **No ADS-B**: no
  category 021 (nor any other category or address), and ADS-B reports come
  about once a second an aircraft, so 12 s would have shown them.
- **The "more" is Mode S, inside the radar's reports**: I048/250 (Mode S
  MB data) in 78 of the 145, the registers the radar reads from each
  transponder — the enhanced-surveillance set BDS 4,0 (selected altitude,
  altimeter setting, autopilot modes), 5,0 (roll, true track, ground
  speed, track rate, TAS) and 6,0 (magnetic heading, IAS, Mach, baro and
  inertial vertical rates) from 23 aircraft; 1,0 (sent as 0,0) and E,1
  from the site's test transponders. Layouts from ICAO Doc 9871, checked
  against the radar's own tracks: true track within 2° of the radar's,
  ground speed within 4 kt, aircraft holding at FL380 with 38 000 ft
  selected, 1013.2 hPa set above the transition and the local 1021 below,
  baro rates agreeing with the climbs and descents. I048/230 (flight
  status) said "airborne" for all. No BDS 2,0 or I048/240 in 12 s, so still
  no callsigns from the feed (both are read when they come). The AD-BN and
  MTBB-BN feeds recorded on 2026-10-11 carry 4,0/5,0/6,0 too, but at that
  hour only from their test transponders.
- **Built**: `decsat_modem::mode_s` (registers 2,0, 4,0, 5,0, 6,0; a 4,0
  with reserved bits set is refused; `selected_level`, `to_go_ft` reading
  Mode C on the altimeter setting the aircraft reports, 27 ft an hPa);
  ASTERIX reads I048/250 and I048/230 into `Report::mode_s`, and a track
  keeps each register's latest (`Track::mode_s`, as a radar need not read
  every register every scan); `asterix::detection` names I048/020's TYP.
  Scope: details box rows Selected (MCP, FMS when it differs, VNAV / ALT
  HOLD / APP), Altimeter, Air data, Heading (magnetic, bank, turn rate),
  flight status when not plain airborne, "Seen by" (Mode S roll-call +
  PSR …); the vertical rate the aircraft's own when it gives one; the PPI
  label shows `060↓030` while the selected altitude is 300 ft or more away;
  a Sel column in the table; the hover tip's Selected row. CSV: 12 columns
  appended (selected_alt_ft … flight_status); `decsat payload`/`decode`
  track lines end with the selected altitude, QNH, IAS, Mach and rate.
- Checked on unit tests with the recording's registers and one whole
  record, and in the GUI on a synthetic carrier made from the recording's
  bits (V.35 taps 3/20 + differential + K = 7 ½, QPSK 64 kS/s, scratch
  generator not committed): 103 of 105 frames, all 143 reports, the rows
  and labels as above.

## ADS-B on the ML–PH trunk: ASTERIX CAT021 edition 0.23 (2026-10-12)

Rory: "add the CAT021 decoding". The ASA176 trunk's packets of type 40h
(channel 58h, 5 %) carry IPv4 from bit 125 to the tail bit, LSB first;
the 96 bits before it are not read (a count among them steps one a
packet). Most of that IP is an ADS-B multicast (SAC 4 SIC 171, ~4
datagrams a second) of ASTERIX Cat 021: the Perth area's traffic (PH is
Perth: the positions sit around 32°S 116°E), 36 aircraft in 23 s —
QFA66 descending to 4000 ft set, VOZ1859, NWK and JST jets, VH- GA
squawking 1200 and 3000, RSAF "EGLE" PC-21s out of Pearce, the ground
station's field monitor (KMDB, RAB and GBS set).

- **Edition 0.23** (2003, the 6-octet I021/130 at 180/2²³°), with FRNs
  27 and 28, spare in that edition, in use: FRN 27 one octet (50–150 on
  every record; edition 0.26 has I021/131 signal amplitude there), FRN 28
  two octets — the Mode 3/A code in I021/070's layout (the VFR squawks
  say so; 0.26 has /070 at 27, the other way round). All 709 records
  parse to their blocks' ends.
- `slotpkt`: type 40h → `SlotRx::ip`, handed on as frames, so the IP
  stage takes them as it takes IP from HDLC (`ip_packet_bits` builds one
  for tests). `asterix`: the Cat 021 UAP (a `Spare` item kind refuses a
  record that sets a spare FRN), `cat021` → reports with an `Adsb` block
  (WGS-84, geometric altitude, emitter category, emergency, field
  monitor, on ground, FRN 27), the selected altitude (I021/146 by its
  source, /148), air data, heading, rates and roll in the `ModeS` fields
  the Mode S registers fill, so the scope shows them alike; `datagram()`
  for UDP payloads; `emitter_text`, `emergency_text`.
- `RadarRx` numbers ADS-B aircraft by address (1, 2, … as first heard)
  and places them east and north of `RadarView::origin`, the first
  aircraft's place rounded to 0.1° (`east_north_nm`, the map's
  projection); `RadarView::adsb`; `source_label` "SAC 4 SIC 171 · ADS-B".
  `RadarRx::message` takes `&mut Message` (it numbers and places them);
  the CSV gains `lat,lon,geo_alt_ft,emitter`.
- Engine: the IP stage tries ASTERIX on every UDP datagram (block
  lengths must tile it) and the modem chain's radar stage takes what it
  finds, so the trunk's VFO gets a radar feed and its scope. `decsat
  payload` lists the aircraft (lat/lon, category, emergencies) and
  `--radar-out` writes them.
- Callsigns: `adsb_names::seen_from` turns an ADS-B feed's aircraft into
  the lookup's, and `AdsbNames::set_local` (fed by the scope from every
  other VFO's ADS-B feed) names radar tracks from them instead of asking
  adsb.lol — offline, and on recordings made alongside.
- Checked: the recording (`payload … --bit-rate 1024000 --radar-out`),
  and a synthetic 682.7 kS/s TPC carrier from its bits through the live
  chain (CLI and GUI): the same aircraft; the scope draws them with their
  callsigns, squawks, levels and selected altitudes. Tests: records of
  the recording (an airliner, one without FRN 28, the field monitor),
  numbering and placing, IP out of a type 40h packet, CAT021 in UDP to
  the radar stage.
- Open (the scope, in the PH-BN session's hands): ADS-B targets placed
  round a site keyed by SAC/SIC, the map under an ADS-B feed (its origin
  is no radar), the details box's "seen by" and "from radar" for ADS-B.

## One voice at a time inside a timeslot; radar places kept by radar (2026-10-12)

**Two channels of one timeslot talking at once** (Rory, live on the ML–PH
trunk: ch 18h "3 × G.728 speech in 3–4, 7–8 · on air", both heard). The
voice floor gave one talker at a time across carriers and timeslots, but
`g728::Voice` mixed every channel of a timeslot. Now each G.728 channel is
a talker of its own: `floor::lane_channel(timeslot, first bit)` on the
app-wide floor in the voice preset's mix, and a `floor::LocalFloor` (same
rule and hold) among the channels of a timeslot played with ▶ or recorded
by itself. `Voice::set_heard(bits)` mixes only the channels heard, the
others decoded and dropped so all stay in step; with the floor's hold off
all are mixed, as before. `E1View::lane_heard` names the one heard; the
row reads "… speech in 3–4, 7–8 · on air 7–8" (or "· hearing 7–8" when
played by itself). `decsat payload --voice-out` still mixes (a file keeps
everyone). Tests: one of two talkers heard is that one alone; lane
channels both ways.

**Radar places by radar, not by VFO** (Rory: "so you don't have to change
it when you go to a different radar"). The scope kept a hand-set place as
`radar_site.<VFO name>`: another radar on a VFO of the same name took it,
and a radar on a new VFO had none. Now `prefs::RadarId`: an ASTERIX
source by SAC/SIC (`radar_site.sac4.sic22`), a feed that does not say who
it is (the BSC feed) by its carrier's RF (`radar_site.rf12328083khz`,
matched within 10 kHz, so an LO trim or the drift a VFO follows is the
same radar), else the VFO's name as before. The place an ASTERIX feed says
(I034/120) is kept under its SAC/SIC as it comes, so the map is there at
once next time, before the north marker; Site… keeps a hand-set place under
the radar's key, and says which. A place kept for a VFO's name moves to the
carrier's key the first time a BSC feed asks for it. The scope gets each
VFO's RF (band centre + offset) from `ScopeInput::rf_hz` and the feeds'.
Checked unattended with a scratch settings folder: the synthetic PH-BN
carrier kept `radar_site.sac4.sic22`; the PH-ML recording with a made-up
place under `radar_site.VFO 1` moved it to `radar_site.rf12328083khz` and
drew its map switch enabled.

## The scope with ADS-B; an "Internet" switch for the online lookup (2026-10-12)

With CAT021 decoded off the ML–PH trunk, Rory: "make a tick box for
'internet' … as we don't need to use an ADS-B site". Callsigns from an
ADS-B feed decoded on another VFO now name radar tracks always (nothing
asked of anyone); adsb.lol is asked only with the scope's **Internet**
switch on — its own setting, `radar_internet`, off by default (the old
`radar_callsigns` switch is no longer read, so the lookup stays off until
ticked). Names say where they came from: "(ADS-B decoded here)" or
"(adsb.lol)". The ADS-B feed's own scope: the header says it has no scans
("each aircraft reports itself") instead of waiting for north crossings;
the map and airports are drawn round the feed's origin (`site_of` takes
`RadarView::origin` after a radar's own place); Site… is not offered; the
details box gives "Seen by ADS-B" (or a ground station's field monitor),
the aircraft's own latitude and longitude, GNSS altitude, emitter class,
on the ground and any emergency, in place of "From radar"; the hover tip
its position. Checked unattended on the synthetic trunk carrier (scratch
settings, the old switch set): 36 tracks, QFA66 descending to 4000 ft
with its place and GNSS altitude, Internet off and nothing asked.

The switch is named **adsb.lol** now (Rory: "we still need internet for
the maps"): it governs the callsign lookup alone, and its first name read
as if it cut the scope off the internet. The map's tiles and the airport
data download whatever it says, as before; its setting keeps its key
(`radar_internet`), so a tick made under the old name stays.

## ADS-B over a radar's scope (2026-10-12)

Rory: "show ADS-B targets on the radar scope too". A radar's PPI now draws
the aircraft an ADS-B feed on another VFO hears (the ML–PH trunk's
CAT021): each at the place it reports, projected round the radar's site
(the feed's trails, measured from its own origin, placed again); magenta
diamonds — small for an aircraft the radar tracks too (the same Mode S
address, else the same callsign), labelled with callsign, level (and the
level it is going to) and speed, with trail and one-minute leader, for the
rest; the hover tip gives its details. An "ADS-B" switch beside Trails,
shown when a feed is there; a corner note counts them ("19 aircraft · 11
tracked by the radar too"). A feed whose clock is more than a minute from
the radar's is not drawn, and the note says so: a recording of another
hour's sky would place aircraft where there are none. The same rule now
holds for the callsigns local ADS-B gives radar tracks, which matched
across clocks before (the PH-BN recording, nine minutes from the trunk's,
took names from it). Checked on a mixed capture (the synthetic trunk
carrier and the PH-BN carrier made from its recording, at −800 kHz,
scratch tools not committed): the note "its feed's clock is 540 s from
the radar's — not drawn" and no names; with the limit lifted in a scratch
build, the diamonds as above (the same aircraft by address in both).

## A CDM model for the trunks: "CDM-570L trunk" (2026-10-12)

Rory asked for a Comtech model for the ML–PH trunk's mix (voice, ADS-B,
radar). `DecoderKind::Cdm570Trunk` ("CDM-570L trunk (TPC 2964 → G.728
voice + IP: ADS-B, radar)", key `cdm570-trunk`, CLI and unattended
`--decoder trunk`) runs the TPC 2964 chain like "CDM-570L TPC", and plays
the G.728 voice it finds by itself under the voice floor (one channel at a
time, now within a timeslot too) as the CDM-600L voice model does
(`DecoderKind::plays_voice`); its IP goes to the IP stage as before, the
ADS-B (CAT021) and any radar data to the radar view and scope. The VFO
reads "TRK", lit while speech is on, and its card "Comtech CDM-570L trunk
· TPC 2964". Checked unattended on the synthetic ASA176 carrier: ch 18h
"3 × G.728 speech in 7–8 · on air 7–8", the ADS-B feed below. Also: four
hover texts (the plan loader, follow the carrier, the radar station card
and its scope button) had runs of spaces left in them by an earlier edit.

**The station is the SIC 20 radar** (Rory: "the CB-BN radar is ASTERIX SIC
20, use that one"). `decsat_ip::rcms::known_radar` gives the
233.71.15.145:4021 station SAC 4 SIC 20 (`StationView::radar`); the
station's scope takes the feed sending SAC 4 SIC 20 by itself (none while
no VFO decodes it, with a note saying so), and that feed's own scope shows
the station beside its PPI. Choosing another feed that gives its SAC/SIC
in "Targets from" keeps it as the station's radar (prefs
`radar_station.<group:port>`). The side card and CLI name the radar.
Note: the recordings of 2026-10-11 (radar survey, 1734.5 and 1745 MHz at
8 MS/s) do not agree with the name: the 12335.24 MHz carrier (the plan's
CB-BN) sent SIC 19, whose north marker places it at Canberra, and SIC 20
(Adelaide, by its north marker) came on 12346.74 MHz
(the plan's AD-BN slot). SIC 20 is used as asked. Checked on real signals:
the trunk (same station, same multicast, in the 1734.5 MHz capture at
+657.8 kHz), the SIC 19 carrier and the SIC 20 carrier, cut out and mixed
at 500 kS/s (scratch script): the trunk's scope drew SIC 20's targets,
SIC 20's scope showed the station. SIC 20 scans in 3.961 s; pedestal
points 2303/2307 read 3.96–4.06 there. The unattended GUI's `--vfo-at`
may be given once a VFO, each taking its place in the `--decoder` list.

## Radar tracks without ADS-B, in red (2026-10-12)

Rory (through the radar-station session): radar tracks in red when no
ADS-B aircraft matches them, only while both a radar feed and an ADS-B
feed are decoded. `RadarScope::without_adsb`: with an ADS-B feed in step
with the radar's clock (within a minute, as for the overlay), the radar's
tracks with neither the Mode S address of an ADS-B aircraft nor its
callsign (the track's own, or the one the local ADS-B gave it by place,
squawk and level) are red on the PPI and in the table, and the details box
says "ADS-B: no aircraft matches"; the corner note counts them ("24 radar
tracks without ADS-B (red)"), with or without the ADS-B diamonds shown.
Primary-only targets, aircraft without ADS-B, and ADS-B out of reach all
read red. Checked on the mixed capture with the clock limit lifted in a
scratch build (the feeds nine minutes apart): the primary-only tracks red,
those matched by address in their colours.

## Q-Flex: ADS-B and radar looked for; the sites' IP is encrypted (2026-10-12)

Rory: "the Q-Flex should have ADS-B and some radar". Looked for on what is
recorded: the 15-minute capture of the three faster links (the plan's
remote "- CAP"/"- MEL" sites: 2 ×, 3 ×, 4 × 128.5 kbit/s) and three
128.5 kbit/s links (ML-NBK, ESP-ML, the 2026-10-09 VFO 3).

- **The faster links' site IP is encrypted in transit.** Every UDP
  datagram's checksum fails (2375 on the 2 × link, 2498 on the 4 ×; the
  plain radar station multicast on the TPC trunk: 15 345 good, 0 bad)
  while the HDLC FCS and the IP header checksums hold; the payloads have
  7.999 bits of entropy an octet, come in whole 16-octet blocks, never
  repeat a first block, and run 23–35 octets past what their UDP length
  field says — an inline encryptor that keeps the UDP header and pads and
  tags the payload. That covers the SNMP and the site multicast to
  233.71.15.x:50000 (2 pkt/s on the 2 × link, 1.4 on the 4 ×; 64–384
  octets; a 64-octet packet about once a second): the right size and rhythm
  for a surveillance feed (a status message a second, reports growing with
  traffic), but not readable here.
- **No plain ASTERIX elsewhere.** The faster links' other timeslots held
  idle flags for all 15 minutes. On the 128.5 kbit/s links, channels 6–15
  (in pairs 6–7, 8–9, 10–11, 12–13 by their statistics) gave no HDLC frame
  with a good FCS and no BSC SYNs, alone, paired or all together, plain or
  inverted, whole or with the subframes' status octets dropped: the
  signalling seen before.
- Plain ASTERIX on any Q-Flex timeslot would reach the scope already: the
  HDLC timeslots' frames go to the IP stage, whose UDP payloads are read
  for ASTERIX (`asterix::datagram`), and non-IP frames of ASTERIX to the
  radar stage.
- **Built**: `decsat_ip::packet::udp_checksum_ok` (over the UDP length's
  span); `IpStats` counts checksums per flow and `Flow::encrypted()` (≥ 8
  checked, ≥ 95 % failing); the IP card marks such flows "encrypted" and
  counts them; the CLI says so; the multicast audio sniffer leaves them out
  (the random payloads had "sniffed" as MPEG audio and AAC, offered to
  play).
- Open: a Q-Flex recording while a site sends its ADS-B or radar in clear,
  if any does.

**The station is SIC 19 after all** (Rory: "use SIC 19 then, CB-BN is
Canberra"). `known_radar` now gives the 233.71.15.145:4021 station SAC 4
SIC 19, the CB-BN carrier's radar (Canberra by its north marker), as the
2026-10-11 recordings said. On the real-signal mix the
trunk's scope takes the SIC 19 feed by itself and draws its targets over
Canberra, the station beside them.

## The waterfall says which carriers carry ADS-B and radar (2026-10-12)

Rory: "show that that carrier is ADS-B and radar capable". `carrier_caps`
(GUI): each VFO's decoders say what its carrier carries — ADS-B (CAT021
reports) and/or a radar feed (sector messages or a scan, or track reports
that are not ADS-B) — and that is kept by the carrier's RF in the user's
settings (`carrier_caps.rf<kHz>=adsb,radar`, the same carrier within 10
kHz). On the waterfall a VFO's label gets ADS-B (magenta) and RADAR
(green) tags under it, from what it finds now or what its carrier is
known for; a known carrier with no VFO on it gets its tags at the foot of
the spectrum, over the plan's names, so the band shows where the ADS-B
and the radars are before anything is decoded. Checked unattended (scratch
settings) on the mixed capture: the radar VFO tagged RADAR, the trunk
ADS-B; both kept; the same capture opened with no VFOs showed both tags on
their carriers.

**Every Q-Flex in the 15-minute capture, searched for ADS-B** (Rory: "look
for ADS-B on the Q-Flex"). The 1760.6 MHz, 4 MS/s, 900 s capture holds 14
Q-Flex FastLinks (by the plan, with this LNB's ~+24 kHz: Broome - CAP 3 ×,
Tindal - CAP 2 ×, KAR - MEL 4 ×, and eleven at 95 kS/s). All decoded in
full but one:

- **The ten 128.5 kbit/s links decoded carry G.728 voice only.** Speech
  on most (24–415 s of 900). Searched their TDM multiplex for HDLC frames
  with a good FCS-16 or FCS-32 (three or more), in each of the 16 channels
  and every adjacent pair, each half of the word as a 64 kbit/s octet
  timeslot in both bit orders, the whole word both ways, and the ESC
  channel, each plain and inverted (a search that finds planted
  CAT021-in-HDLC frames in a channel, all 300): nothing. Two links show
  channels 1–4 active for ~4 minutes with no speech decoded; their
  activity splits into two groups sharing channel 1 and comes in talk
  spurts — the two-calls-sharing-channels layout again, not data.
- **The three faster links' site IP is encrypted** — the 3 × link's too
  (15 of its 29 flows flagged: the 233.71.15.1:50000 multicast, SNMP; its
  TCP port 5928 flows random in 16-octet blocks as well; the rest OSPF,
  ICMP, and NTP without checksums). The multicast is the only candidate
  for an ADS-B or radar feed, and it cannot be read.
- **One strong 95 kS/s carrier does not demodulate** (−1236.7 kHz: the
  plan's ASA187 LEO - MEL; 20 dB over the floor): FastLink rate and
  roll-off exactly, but no lock as QPSK, 8PSK, 8QAM, 16QAM or 16APSK, and
  no FastLink sync, Viterbi or TPC 2964. Two carriers on one frequency
  (Paradise's paired-carrier mode, each modem cancelling its own) would
  look like this; not pursued.
- ASA148 LT-ML (TPC 2964, 128 kS/s, a CDM-570L pair): idle flags.

So no ADS-B or radar in clear on any Q-Flex recorded so far.

## Radar status words said field by field; fast or high tracks purple (2026-10-12)

Rory asked what a track's "Status A846h" meant, then for every field's
meaning beside it. `asterix::status_fields` re-reads the report's record
and says each field of its status items, with every value it can take:
I048/020 (TYP, SIM, RDP, SPI, RAB; the first extension's TST, ERR, XPP,
ME, MI, FOE/FRI; the second's ADSB, SCN and PAI bits) and I048/170 (CNF,
RAD, DOU, MAH, CDM; TRE, GHO, SUP, TCC), tables from Cat 048 edition 1.32,
and I021/040 from Cat 021 edition 0.23 for ADS-B. An item the record
leaves out gives no rows, rather than its fields read as zeros from the
packed word. The scope's details box lists, under the word, only the
fields set now (not zero: Rory's ask), a row each; hovering a row names
its item and lists every value it can take. The BSC feed's
words are said by what each was seen to mean (`radar::bsc_status_text`).
A846h is Mode S roll-call, RDP chain 2, a confirmed SSR/Mode S track,
climb mode unknown. PH-BN's track 1A9 (E800h, roll-call + PSR on a
combined track) checked unattended.

Rory: purple for "something that is going very fast or very high, likely
military". The scope colours a track purple, ahead of its height colour
but behind coasted yellow, at a ground speed of 650 kt or more, FL450 or
above, or over 350 kt below FL100 (civil traffic keeps to 250 kt IAS
there); the legend says so. A business jet above FL450 turns purple too.
Rory then asked for military addresses as well: a Mode S address in
`MILITARY_ADDRESSES` turns a track purple however it flies — for now the
ADF's 7CF830–7CFBFF (his range; the field monitors sit just below, at
7CF7xx). The legend's entry is "mil / fast". Then the US block too:
ADF7C8–AFFFFF, the rest of the US A00000–AFFFFF above the N-number
addresses (N1–N99999 fill A00001–ADF7C7), held for the US government and
nearly all military.

## The CDM type found by itself (2026-10-12)

Rory: "is it possible to auto detect CDM type?" — "yes build it".
`DecoderKind::CdmAuto`, now the Comtech entry's default ("Type found (TPC
2964 or Viterbi; the model from its data)", key `cdm-auto`, `--decoder
auto`): the Modem path's first-look Identify picks the chain — TPC 2964
when it found the unique word, a FastLink when it named one, else the K = 7
code (`auto_chain`); a chain that has not locked its code after
`AUTO_LOCK_SECS` (10 s of signal: TPC frames decoded, the Viterbi's rate
found, FastLink codewords decoded) is marked failed and a fresh first look
takes the next. The model is read off what the data carry
(`vfo::cdm_model`): D&I++ or E1 timeslots → CDM-600L voice; HDLC packets
carrying timeslots → CDM-570L trunk; other TPC data (HDLC IP, Frame Relay,
TS) → CDM-570L TPC; the K = 7 code → CDM-570L Viterbi; a FastLink stays
one. `VfoStatus::chain` and `cdm_model` carry them to the GUI: cards by the
chain running, titled by the model; the label "auto TRK", "auto 570" (CDM?
until found); "found: …" beside the model menu. Voice plays by itself when
G.728 speech is found, as for the voice models. Tests: the type-found
model on the synthetic TPC carrier (Identify picks TPC 2964, the model is
a CDM-570L's data), the model from each payload. Checked unattended (muted)
on the mixed capture: PH-BN found as CDM-570L Viterbi, the ASA176 trunk as
a CDM-570L trunk; and in the CLI on the PH-BN carrier.

**Live: every Q-Flex on the band searched for ADS-B (2026-10-12, 20:24
local).** Rory: "survey now". HackRF at 1732, 1746, 1760.6, 1774 and
1788 MHz, 20 MS/s, 60 s each, LNA 24 / VGA 20 (input −34 to −25 dBFS),
LNB about +26 kHz (by the CAP links against the plan). Identify named 44
FastLinks in the first three (none above 12366 MHz): 38 once deduplicated
by RF, and two pairs Identify had merged (two 95 kS/s FastLinks 119 kHz
apart read as one ~190 kS/s carrier: the plan's Doongan + TEN and Kilgor +
Limbunya, decoded apart at their plan frequencies) — 42 in all.

- **39 links at 128.5 kbit/s: G.728 voice, nothing else.** Every one
  decoded (BER after LDPC clean but for a few failed codewords), its
  multiplex searched as before (all channels, pairs, octet timeslots, both
  words, the ESC; plain and inverted; three good FCS needed): no HDLC
  frame, so no ASTERIX. Speech decoded on 25 of them in the minute.
- **The three faster links (Broome 3 ×, Tindal 2 ×, KAR 4 ×): site IP
  encrypted**, as in the 15-minute capture — the 233.71.15.x:50000
  multicast and SNMP flagged; nothing in clear.
- **LEO - MEL** still does not demodulate (no FastLink sync), as the day
  before.

No ADS-B or radar in clear on any Q-Flex live either. If the Q-Flex sites
send ADS-B, it is in their encrypted multicast. Scripts (survey.py) and
the 12 GB of captures stayed in the session's scratchpad.

## The CDM type on the waterfall's carrier tags (2026-10-12)

Rory: "show cdm type on the waterfall carrier tags too". `carrier_caps`
now keeps the modem beside ADS-B and radar (`model=<decoder key>` in the
setting): for a VFO on a Comtech CDM model, once its code has locked (and
for TPC 2964 once the payload's format is known), the model its data say
(the type found's, or `vfo::cdm_model` of the chain and payload whichever
model was chosen); for a FastLink VFO, Q-Flex. A light-blue tag leads the
row — "CDM-570L trunk", "CDM-570L Viterbi", "CDM-570L TPC", "CDM-600L
voice", "Q-Flex" — under a VFO's label and on known carriers with no VFO
on them; a newer reading replaces the kept one (TPC → trunk as the
timeslot packets show). Rows of tags from carriers close together no
longer cover each other: a row moves a line down (under the labels) or up
(at the spectrum's foot). Checked unattended on the mixed capture
(muted): kept `radar,model=cdm570` and `adsb,model=cdm570-trunk`, both
shown with and without VFOs.

## The CDM type in the VFO list (2026-10-12)

Rory: "show the cdm type in the vfo list too". Each row of the side
panel's VFO list now ends its text with the same light-blue tag as the
waterfall: the modem its decoder found once the code locked (the CDM model
its data say, whichever model was chosen — a VFO set to "CDM-570L TPC" on
the ASA176 trunk reads "CDM-570L trunk"), else the one kept for its
carrier (`carrier_caps::vfo_model`); shown where it fits before the CPU
reading, explained on hover. Checked unattended on the mixed capture: VFO
1 (type found) "CDM-570L Viterbi", VFO 2 (TPC chosen) "CDM-570L trunk".

## Survey: every carrier in the span tried in turn (2026-10-12)

Rory: "a 'survey' button that tests all carriers sequentially on a
satellite that it sees … a time out for bad decode and don't do too many
at once". Toolbar **Survey…** opens `survey.rs`'s window: ▶ snapshots the
front end's clean carriers in the whole span (by frequency) and tries them
`parallel` at a time (1–4, default 2), each on a VFO of its own named
"Survey <MHz>" (voice not played, nothing recorded, kept out of the saved
session, made without taking the selection). Identify first (12 s; a
provisional answer taken after 6 s), then the decoder for what it is
(`pick`: DVB-S2 → GSE/IP; TPC 2964, FastLink, DVB-S-like or plain
BPSK/QPSK → the Comtech type-found model; bursts, 8PSK and the rest → "no
decoder"), given `give_up_secs` (default 40) to lock — DVB-S2 frames
decoded, a modem's code locked (and TPC's payload known) — then watched
`DWELL_SECS` (6 s) for what it carries before its VFO goes and the next
carrier starts. Results stay in the window: MHz, SNR, rate, outcome
(decoded / no lock / no decoder / no signal / VFO gone), what it decoded
as (the CDM model, Q-Flex, DVB-S2), what it carries (ADS-B, radar, voice)
and the payload or the decoder's last word; "VFO" makes a kept VFO on a
decoded one; Save CSV writes `decsat-survey-<stamp>.csv` to the output
folder. The waterfall's carrier tags learn as it goes. A retune stops it.
Unattended: `--survey`. Checked on the mixed capture (2 of 2 decoded:
CDM-570L Viterbi with radar, CDM-570L trunk with ADS-B and voice) and on a
live 8 MS/s capture (Oct 11, 1745 MHz centre): 26 of 29 decoded in under
three minutes, two at a time — Q-Flex FastLinks, CDM-570L TPC carriers,
one with radar.

## The BSC feed's ping in the details box (2026-10-12)

Rory, on the PH-ML scope: "the radar ping status info is not showing on
here". ASTERIX tracks list their detection fields (I048/020's TYP: PSR,
SSR, Mode S); the BSC feed's track showed only its status word. Its
reports carry the ping all the same, read off the 37 s PH-ML recording
(234 reports): octets 22–23's low 9 bits, the plot's time after north,
give the beam's azimuth then (ticks / 128 / scan × 360°, within 1° of the
track's bearing every time); octet 20 the plot's flags — 0Ch on all 40
reports with neither Mode A nor C (primary-only plots), 0Dh on primary-only
tracks in decline, 00h/01h with Mode A and C (97/67; 01h on most climbing
or descending tracks), 08h/0Ah with them too (8: primary and secondary?);
octet 21 a value falling with range (plot strength?). `radar::bsc_seen_by`,
`bsc_plot_azimuth`, `BSC_PLOT_FLAGS`; the details box's BSC rows: Ping
("beam at 148.0° · 1.516 s after north"), Seen by ("primary only (no
reply) (0Ch)", every flag value seen on hover), Strength?. All marked as
read off the recording, not known. Checked unattended on the recording:
track 103, primary only, beam 148.0° against its bearing 148.1°.

## TPC 2964 on 8PSK; an EDMAC carrier with bridged Ethernet (2026-10-12)

Rory's VFO_103 capture (12347.55 MHz, not in his plan; 800 kS/s cf32,
15.8 s) — "find out what this 8PSK is": 8PSK, 238 962 S/s, α 0.25, Es/N0
10.6 dB, one of the survey's open "Comtech 8PSK" carriers.

- **The code is tpc_2964, laid out as on QPSK.** Frames of 988 symbols =
  2964 bits; the UW F50B8h reads plainly under the RCV-20x manual's Figure
  3.1 8PSK labels (reflected Gray round the circle, 000, 001, 011, 010,
  110, 111, 101, 100, most significant bit first), starting on the
  *second* bit of a symbol; then 46 rows of (64,57), x⁶ + x + 1, parity
  last, code bits unscrambled (`Structure::TEST`). At the raw BER of ~4 %
  a 64-bit parity test is only +0.002 above chance — lost in the noise —
  but whole-row Hamming syndromes show it at once: 7.2 % of rows and 15 %
  of columns clean, against 1/128 at random. Whether the 711 kS/s pair
  (whose UW flipped 180° every frame) is the same layout is still open.
- **Receiver**: `tpc2964` takes a `Modulation` (BPSK, QPSK, 8PSK; anything
  else stays QPSK): 8PSK max-log soft bits on its own Figure 3.1 labels
  (DVB-S2's 8PSK labels are these bit-reversed), 16 orientations, the UW
  looked for at every bit as before. Identify looks for the UW on 8PSK
  too, a TPC VFO hands 8PSK on, the FEC thread's data rate counts 3 bits a
  symbol. On the capture: Identify names it, `auto` picks TPC, 3557 of
  3800 frames decode (channel BER 4 %; the failures sit at the 30 carrier
  slips). Tests: the structure under four orientations at 12 dB; every
  frame clean at 9.5 dB Es/N0.
- **Payload, found by hand (no decoder yet)**: V.35 (3, 20) descrambled,
  then Comtech **EDMAC** framing — 1008-bit frames: a 12-bit header
  `000101110011`, five 192-bit data blocks with 9-bit overhead groups
  between them (the first constant `111111100`): 48 overhead bits per 960
  data = 21/20, so 537.7 kbit/s carries **512 kbit/s**. The same layout as
  the RCV-20x manual's EDMAC-2 data-mask example (`13-204,214-405,415-606,
  616-807,817-2928`, a 2928-bit frame ≈ 1.6 % overhead). The overhead
  groups (27 bits a frame, the M&C channel) ran one 216-bit cycle (8
  frames) throughout: no M&C traffic in the 15 s.
- **The 512 kbit/s carry bridged Ethernet**, MSB first, the line idling at
  1 (true polarity = the descrambled data inverted), no HDLC flags: a
  6-byte header (`08 A0/A2 xx xx 8n 31/32`, the 8n counting per flow),
  the Ethernet header (one MAC pair throughout — one direction of a
  routed link — EtherType 0800), the IP packet, and a 4-byte trailer that
  is not an Ethernet FCS. 88 checksum-good IPv4 headers in 15 s: SIP and
  RTP-like UDP, SNMP polls and syslog to managed devices, BGP, SMB/TCP —
  an enterprise WAN. Plus an 18-bit word every 2064 data bits. How frames
  are delimited (length?) is not known, so no PCAP yet.

Next for this carrier: EDMAC as a payload format, the bridging
encapsulation into the IP stage, and a recording of the 711 kS/s pair.

## A VFO on a known carrier takes its decoder (2026-10-12)

Rory: "after a scan, if you press on the green above or drag a decoder
over it, it chooses the right decoder". `CarrierCaps::decoder_for`: a VFO
made over a carrier whose modem is known (a survey, any earlier decode:
the CDM model it turned out to be, a Q-Flex FastLink, and now DVB-S2 IP/TS
and DVB-S too, which the carrier tags learn and show) takes that decoder —
from a click on its green bracket, a drag across it, a double-click or a
plan name — whatever "New VFOs run" says, unless that is a tool (IQ
recorder, spectrum, symbols, carrier ID). The bracket's hover says which
("click: CDM-570L trunk (known) on …"). Unattended `--claim-carriers`
without `--decoder` does the same. Checked on the mixed capture with "New
VFOs run: Identify": the radar carrier came up CDM-570L Viterbi, the trunk
CDM-570L trunk.

## Two radars on one scope (2026-10-12)

Rory: "if you are decoding 2 radars nearby can you show both radars on the
map along with the ADS-B if available". A radar's scope now draws the
other radar feeds decoded on other VFOs as well (`OtherRadar`): each where
its place is known — its own (ASTERIX I034/120), else the place kept for
it (by SAC/SIC or carrier RF, looked up once a VFO) — its site marked (⊕
and its name), its tracks moved from its frame into this one's (through
latitude and longitude): hollow circles in the height colours with their
trail, leader and label, small and dim for an aircraft this radar tracks
too (same Mode S address, or same squawk within 3 NM); hover names the
radar. "Other radars" switch beside ADS-B; a feed whose clock is more than
a minute from this one's is left out and named in the corner. With the
ADS-B diamonds, the PPI is the radars' and the ADS-B picture together. A
radar that does not say where it is needs its place set once with Site… on
its own scope. Checked on the Oct 11 8 MS/s capture: AD-BN's scope with
MTBB-BN on another VFO (its place made up in scratch settings): its site
and its two test transponders drawn.

## EDMAC and the bridged Ethernet inside it (2026-10-12)

Rory: "build the EDMAC and Ethernet decoder" for the 238.96 kS/s 8PSK
carrier (12347.55 MHz). Now TPC 2964 → V.35 → EDMAC → packet blocks →
Ethernet → the IP stage (statistics, PCAP), found blind like any payload.

- **`decsat-modem::edmac`**: the 1008-bit frames (header `000101110011`,
  five 192-bit data blocks, 9-bit groups between: the first fixed
  `111111100`, three carrying the M&C channel), found by the 21 fixed bits
  three frames running, either way up; frames with them wrong are kept
  (up to 4 bits) until three in a row, then searched for again. A payload
  format (`Format::Edmac`), scored by frames found; its M&C bits go out as
  the ESC bits, its data as the inner bits (text search). Re-running a
  recorded `.bin` through `decsat payload`: V.35 on data already
  descrambled shows every EDMAC frame a second time 20 bits on (the far
  tap reaches past the header into idle data), so a later reading must now
  beat EDMAC by 1/16 — though such a recording's first frames are written
  before the descrambler is chosen, so a re-run of one is never clean.
- **`decsat-modem::ethblock`** (the encapsulation, Comtech's name not
  known): 258-byte blocks of a 4-byte word — 2 bytes not decoded (no
  CRC-16 polynomial fits the block before or after, with or without the
  words' flag and pointer bytes: all 32 768 tried), flags C7h without a pointer, 47h or B8h with one, and a
  pointer (0–253) to the first byte after a packet ending in the block —
  then 254 packet bytes, idle FFh. Idle the word is EB 49 C7 FF. Packets:
  a length byte (0 = to the pointer; the long ones, and any the sender
  chose), then `08 A0|A2, context (2), s, …`. `s` with 80h set + a type
  byte (31h/32h/35h) = the whole Ethernet frame, no FCS, setting up the
  context; without, a compressed packet: no flags → `[s][UDP checksum if
  the context's was not 0][UDP payload]`, rebuilt from the context's
  headers (IP identification kept, lengths and IP checksum recomputed) and
  passed only if its UDP checksum checks; flags 40h/20h add bytes not
  understood (SIP and a few RTP ones) and are counted. A length byte must
  be followed by 08h (else it was an idle byte hit by an error — the data
  of modem frames that did not decode reach this far); one-byte packets
  `01 02` (a few a second) are counted. Locks on four words that look
  right, one without a pointer (read upside down, pointer words can look
  right; idle ones cannot) — the bytes come inverted here.
- **On the 15 s capture** (`decode --decoder tpc --modulation 8psk --out`):
  EDMAC 8290 frames; 3823 blocks; 105 whole frames, 96 rebuilt, 15 left
  compressed, 24 before their context; **174 IP packets to the PCAP**, every
  IP header checksum good, UDP 66 checked good + 54 without a checksum
  (RTP) + 2 bad (whole frames hit by errors). The traffic: SNMP polling of
  managed devices, BGP, SMB/TCP, and **SIP plus RTP from a "Radio (IP)"
  user** — the RTP's header extension profile 0167h is ED-137's (EUROCAE
  radio over IP); its 24-byte packets look like ED-137 keep-alives (no
  audio in the 15 s). The GUI's side panel: Framing (EDMAC) and Packets
  rows; the IP card's source "Bridged Ethernet (packet blocks)". Tests:
  EDMAC at any phase, either way up, through errors and a gap; blocks with
  length- and pointer-delimited packets, rebuilt UDP (with and without a
  checksum), a failed checksum, idle errors, one-byte packets, random bits;
  the payload stage finding all of it under V.35.

Open: the word's first two bytes; the compressed flags 40h/20h (and the
A0h/A2h bit); what the one-byte packets are; ED-137's signalling in the RTP
extension. The 8PSK carrier loop reads "not locked" while the TPC decodes
— not looked into yet.

## Auto on a carrier's bracket (2026-10-13)

Rory: "when pressing the green estimated symbol rate above the signal can
you make it the 'auto mode' like the survey doing a scan then choosing the
right mode". A click on a detected carrier's bracket (the green rate
label above it) now sends `Action::Claim`: the VFO, sized as before, starts
on Identify and `auto_pick.rs` watches it — once Identify answers (for the
offset the VFO is at; a provisional answer after the survey's 6 s; 30 s
before it gives up and leaves it on Identify) the survey's `pick` chooses
the decoder (DVB-S2 → GSE/IP; TPC 2964, FastLink, DVB-S-like, plain
BPSK/QPSK → the Comtech type-found model), set on Identify's centre and
symbol rate. A DVB-S2 VFO is watched on: when most good frames are
TS-mode it moves to DVB-S2 → MPEG-TS. No decoder (bursts, an 8PSK without
TPC, a CW): it stays on Identify. Unlike the survey's, the VFO is kept;
the status bar names what was picked. A decoder changed by hand, or the
VFO deleted, ends the watch. A carrier already known (`decoder_for`, from
the "known carrier takes its decoder" change) still gets its known decoder
at once, and a tool chosen for new VFOs (IQ recorder, spectrum, symbols,
CID) is kept: auto is for the carriers not known yet
(`band_view::bracket_decoder`). Shift-click gives exactly the decoder *New
VFOs run*. Unattended: `--claim-auto` (claims as `--claim-carriers` does,
each the way a bracket click would). Checked on the 8 MS/s test scene: VFO 1 DVB-S2 → GSE/IP
(locked, IP and the test radio), VFO 2 TPC carrier → "auto TPC", VFO 3
DVB-S2 32APSK → GSE/IP → TS-mode frames → MPEG-TS, VFO 4 → "auto 570
VIT 3/4"; all locked.

## ED-137 radio signalling; compressed fragments (2026-10-13)

Rory: "decode the ED-137 radio signalling in the RTP", with a 159 s
recording of the 8PSK carrier from the GUI (VFO_32, 12347.5531 MHz: its
`.bin` and `.pcap`). tshark 4.6 (Rory's Wireshark install) served as the
oracle: it dissects ED-137, ED-137A, B and C.

- **`decsat-ip::ed137`**: RTP (RFC 3550, CSRCs, extension, padding); the
  header extension of profile 0167h (ED-137A onwards: PTT type 3 bits, SQU,
  PTT-id 6, PM, PTTS, SCT, 2 reserved, X, then type/length/value features)
  and 0067h (the first edition's 32-bit word, VF); features 1 signal
  quality (index, method; the RSSI index's dBm bands), 2 CLIMAX delay, 3
  radio remote control, 4 CLIMAX delay compensation, the rest raw. Layout,
  masks and value names from tshark's `-G fields` / `-G values` (Wireshark's
  packet-rtp-ed137.c, GPL-2.0-or-later — credited in the module). A
  `RadioRx` follows each stream (src, dst, ports, SSRC): keep-alives (PT
  123, the SDP's `R2S/8000`) and sound, loss by sequence, PTT and squelch
  now, times on and seconds on, a log of changes; streams under 3 packets
  are stray (an SSRC hit by errors — this flow has no UDP checksum).
  SIP: start line, From/To (display name or user, %-escapes undone), Call-ID,
  top Via branch, WG67-Version / Subject / Priority / User-Agent, the SDP's
  m=, c=, a=. **Refused or accepted**: one direction only shows the caller's
  requests, but an ACK reusing the INVITE's branch acknowledges an error
  answer (RFC 3261 §17.1.1.3), a new branch a 200 OK.
- **Checked against tshark packet by packet** (`decsat pcap --radio-csv`,
  matched by time): 488 of 488 agree on PTT, SQU, PTT-id, PM, PTTS, SCT,
  SSRC, PT, sequence and the feature; tshark also shows one packet whose
  extension length is broken (DecSAT drops it).
- **What the recording holds**: one stream, 172.16.82.28:8602 →
  10.26.185.112:8580 — R2S keep-alives only (PTT off, squelch closed, PTT-id
  0, signal quality 0 by a vendor's method 4), ~5 a second as the SDP's
  `R2S-KeepAlivePeriod:200` asks, none with sound in 159 s: the VCS → radio
  direction of an idle radio. Every ~10 s the VCS ("Radio (IP)" at
  172.16.82.15, user agent SipLib2-ganymed-evo4_1_113_0) INVITEs radio
  109061002 @ 10.26.185.115 (Subject: radio, WG67-Version: radio.01, SDP
  PCMA/PCMU + R2S, ptime 20, `type:Radio-TxRx`, `txrxmode:TxRx`, `bss:FRQ1
  RSSI`, `sigtime:1`, `ptt_rep:0`, `rtphe:1`, no frequency id) with new
  ports each time, and every one is **refused** (the ACKs reuse the
  INVITEs' branches); the answers travel on the other direction's carrier.
- **`ethblock` compressed flags understood**: 40h = one byte follows (00h
  every time; taken for an IP identification change, the context's kept);
  20h = the IP flags and fragment offset follow and the payload is that
  fragment's bytes (no UDP header): the SIP INVITEs go in 300-byte IP
  fragments — the first whole (context), the middle ones compressed
  (offsets 280 and 528), the last whole under a context of its own. A
  context now keeps Ethernet + IPv4 (+ UDP when its frame began a
  datagram); a compressed first fragment cannot be rebuilt (its datagram's
  length is not sent), not seen. On the recording: 937 rebuilt (was 805),
  65 left (was 197), 537 keep-alives (was 487), the INVITEs whole.
- `decsat pcap` puts fragments back together first; `decsat payload` reads
  ED-137 and writes `--pcap-out` (the `.bin` decoded again: 1614 IP packets
  where the GUI's PCAP, from the build before, had 1497). GUI: the IP
  card's **Radio over IP (ED-137)** section; unattended `--panel-scroll PX`
  holds the side panel scrolled for screenshots of the lower cards.

Open: the voice (PCMA/PCMU when PTT or squelch is on) is counted, not
played; the 40h byte's meaning; the other direction (radio → VCS, its
squelch and receive audio) is on the paired carrier.

## ED-137 voice played (2026-10-13)

Rory: "play the ED-137 voice when PTT or squelch is on".

- `decsat-ip::ed137`: G.711 decoding (`alaw_to_linear`, `ulaw_to_linear`,
  ITU-T G.711 Tables 1a/2a; A-law checked against the E1 stage's decoder on
  all 256 codes); a packet of PT 8 or 0 whose signalling has PTT or squelch
  on is a `VoiceChunk` (decoded, 8 kHz) for the taker, while one wants it
  (`want_voice`, `take_voice`); `talk_packets`/`talk_secs` per stream. Sound
  with neither on is counted, not played; keep-alives (PT 123) never are.
- Engine: the IP stage keeps a `CallVoice` (the Q-Flex calls' player,
  recorder and voice-floor gate) for the radio, made once ED-137 is seen;
  ▶/● are the call-voice settings (`tdm_play`, `tdm_record`), the voice
  preset (`voice_auto`) plays it once a stream has talked. One stream is
  heard at a time: the one talking keeps it until 0.3 s without a talking
  packet; its PCM goes to the `CallVoice`, which `tick` feeds on the signal
  clock — silence between talk spurts, the floor deciding whether it is
  heard (one talker across the app). `GseView.radio_voice` for the GUI.
- GUI: the call-voice card takes a title (and seconds of speech) and shows
  **Radio voice (ED-137)** on a VFO with ED-137 streams: speech · on air /
  waiting / silent, what is on (e.g. "normal PTT on 172.16.82.28:8602 →
  …"), ▶ ●, the recording.
- Test: a second of PTT with a 400 Hz PCMA tone between seconds of
  keep-alives, through `IpStage` to the call recorder: the `.wav` is
  silence (RMS < 0.01), the tone (0.35), silence. Rory's recordings hold no
  voice (an idle radio, VCS → radio side), so not yet heard live.

## Reset labels (2026-10-13)

Rory, on a live 20 MS/s view crowded with modem and ADS-B tags (several
Q-Flex tags over what the detector now brackets as one 483 kS/s carrier):
"can you make a button to reset all the found carrier in survey it looks
like there was a bunch of false decode or something". A first cut (da9101d)
asked span-or-all; Rory: "just put a button the removes/ resets all the
survey lables". So: **Reset labels** in the toolbar beside Survey… (off
when none are kept) and the same in the Survey window, one click, no
question: `CarrierCaps::forget_all` empties what is known and
`prefs::forget_carrier_caps` drops every `carrier_caps.rf…` key (the
other settings stay), and the survey's results table is cleared. A VFO
still decoding a carrier labels it again. The labels are kept by RF for
good, so carriers that have since gone (SCPC traffic changes) leave theirs
behind; this is the way to clear them. Test:
`carriers_are_forgotten_and_the_rest_kept`. Button checked on screen in
an unattended run with three labels seeded (enabled); not clicked
unattended.

## The ED-137 link's other direction: 477.9 kS/s 8PSK (2026-10-13)

Rory: "decode the paired carrier for the aircraft side". The pair, found
in survey captures: **12347.08 MHz, 477.9 kS/s 8PSK, TPC 2964** (right below
the 238.96 kS/s one at 12347.58: 1075 kbit/s = 1024 k × 21/20, EDMAC), the
other direction of the same routed link: 10.26.x → 172.16.x, the MAC pair
reversed.

- 8 s at 1744 MHz (Oct 12 00:11, probably LNA 40 — that night's overload):
  Identify reads 8PSK + the TPC word; 878 of 3196 frames decode (179 slips,
  BER 4.7 %), EDMAC and the packet blocks found. In it: the radio side's
  SIP answer to the VCS's INVITEs — **603 Decline, `Reason: WG-67
  ;cause=2008 ;text="limit exceeded"`, body "Max UAs (1) in any mode is
  reached"**: the radio takes one session and has one, so the VCS's
  attempt every 10 s is declined (as the ACKs on the other carrier said) —
  and an R2S keep-alive from the radio (10.26.185.112 → 172.16.82.20).
- 60 s at 1746 MHz (Oct 12 20:26, LNA 24): Identify reads it **BPSK** — on
  the timing-recovered symbols a 28 dB line in s² at ~97 kHz, none in s⁴,
  none in s⁸ (the 1744 capture: s⁸ only) — its loop is seeded there and
  never locks; as 8PSK by hand it reaches 8.7 dB with no frame lock. Not
  explained: not the in-band HackRF spur (1746.875 MHz, −32 to −44 dB), not
  IQ imbalance (0.02 dB, 0.09°: image rejection 58 dB); an 8PSK lock tried
  whenever the UW is missing found no word either (not kept).
- So no aircraft voice yet: the HackRF was busy (Rory's GUI). For a good
  look: LNA 24, a centre a few MHz away (1744 MHz put the pair at +3.1/+3.6
  MHz and read it right), a VFO on 12347.08 with the Comtech decoder.

## The radio side decoded live; the carrier loop no wider than 1 kHz (2026-10-13)

Rory closed the GUI: "record it". The 477.9 kS/s 8PSK (12347.08 MHz), live:

- **Why it failed before**: (1) my VFO widths — 560 kHz clipped this carrier
  (it occupies ±300 kHz) and took in its neighbour when put on the 238.96k
  one (the two are ~50 kHz apart edge to edge): 300 and 640 kHz fit; (2)
  the carrier loop: Bn·T = 0.008 at 477.9 kS/s is 3.8 kHz, and the carrier
  slipped 135 times in 6 s (31 at 0.005, none at 0.002); (3) SNR: 9.6–9.8
  dB is at the code's threshold for this decoder (an AWGN waterfall: 57 %
  of frames at 4.85 % raw BER) — the HackRF's **RF amp** (+14 dB, LNA 24,
  VGA 20; input −21 dBFS) gave 10.5 dB and 97 % of frames in 6 s, with
  neighbours (8PSK and QPSK TPC) unharmed. Tuning (1744/1747.4/1748/1750
  MHz) made no difference once these were right.
- **`pll_bandwidth`**: 0.008 of the symbol rate, kept between 250 Hz and
  1 kHz (was 250/Rs clamped to 0.008–0.04, so ≥ 0.008 for every fast
  carrier). Six fast QPSK TPC carriers (299–640 kS/s) decoded the same
  either way; slow carriers unchanged.
- **Chase–Pyndiah**: 6 least reliable positions instead of 4 decode 91 %
  rather than 57 % at 4.85 % raw BER (iterations barely matter), but cost
  4×: the decoder already runs 1.45× real time on this carrier when most
  frames fail. Left at 4; `waterfall_8psk` (ignored) measures it.
- **10 minutes live** (1748 MHz, 4 MS/s — `decode --hackrf` defaults to
  2 MS/s, too narrow for a carrier at −915 kHz): 183 048 of 248 383 frames
  (74 %), 5 slips, sync lost for a few seconds now and then (bursts of
  errors); 7390 IP packets, 85 flows. The radio's stream: keep-alives only,
  squelch never open; one controller transmission through it (normal PTT,
  PTT-id 50, 8.7 s) — its sound travels VCS → radio, on the other carrier.
  597 SIP dialogs: four VCS addresses calling ~20 radios (109051001–022 at
  10.26.189.111/.121, 109061002), every one declined (603, max UAs 1).

## Both directions for 15 minutes: PTT summation, no voice (2026-10-13)

15 min of IQ at 1748 MHz, 4 MS/s, LNA 24, VGA 20, RF amp (7.2 GB, deleted
once decoded), both carriers decoded offline:

- **VCS → radio (238.96k)**: 217 499 of 217 650 frames; its stream
  (172.16.82.28:8602 → 10.26.185.112:8580) keep-alives only, 4.9/s, PTT
  never on: this VCS never transmitted. 84 SIP dialogs.
- **Radio → VCS (477.9k)**: 240 715 of 435 013 frames (55 %; 74 % in the
  10 min live run before); keep-alives only (two thirds lost with the
  failed frames). The radio reports other positions' transmissions in them
  (ED-137's PTT summation): 16 in 15 min, 110 s — PTT-id 50 normal PTT
  most of it, PTT-id 4 priority PTT at the end of several, 32 and 36
  coupling once each — and squelch open once, 0.4 s, inside one of them.
- No sound on either stream (the call recordings: silence throughout).
  This satellite pair carries a session between one VCS and radio
  109061002 that stays signalling only; the controllers' audio, and any
  aircraft's, take other paths. The VCS's attempts at more sessions are
  declined (603, max UAs 1).

## PCAP on its own button; IP live into Wireshark (2026-10-13)

Rory: a separate record-PCAP button, and IP live in Wireshark.

- **PCAP** (`VfoSettings::pcap`, `FecOutput::pcap`): the IP stage's file
  follows its own switch, no longer the modem's ● Record (which keeps the
  `.bin` and `.ts`). Shown for every decoder that carries IP
  (`DecoderKind::carries_ip`: GSE, the modems, and a TS decoder once MPE
  brings IP). `decsat decode --out` writes both, as before.
- **Wireshark ▶ Live** (`decsat_engine::wireshark`): Wireshark is started
  with `-k -i -` and captures from its standard input (through dumpcap —
  tested on Windows with tshark first), which a thread of its own writes
  as a PCAP stream (`PcapWriter` is now generic over its sink) behind a
  4096-packet queue: a Wireshark that falls behind loses packets, counted,
  never the decoder's time. `--enable-heuristic rtp_udp`: ED-137's RTP on
  8580/8602 otherwise shows as data when the capture starts mid-session
  (with it, the ED-137A extension — PTT, squelch, SQI — is dissected).
  Each ▶ is a new ask (`Option<u32>`, a counter), so a Wireshark that was
  closed can be opened again; ⏹ drops the feed and Wireshark keeps what it
  has. `decsat decode … --wireshark` for the CLI.
- Checked: the VFO_103 capture (8PSK TPC → EDMAC → bridged Ethernet)
  decoded with `--wireshark --out`: 201 packets in Wireshark's own capture
  file, 201 in the PCAP, the same protocols.
- **Live on the carrier** (Rory, signal back after an outage — no satellite
  traffic at all 1700–1800 MHz for a while, input −40 dBFS against −29):
  the VCS side is at −422 kHz from 1748 MHz (12347.578 with the LO), not
  −447. `decode --wireshark` and then the GUI
  (`--hackrf 1748 --rate 4 --lo 10600 --amp --vfo-at -422000:300000
  --decoder tpc --wireshark`; new `--amp` and `--wireshark`) fed Wireshark
  ED-137 RTP, SIP, SNMP and BGP within seconds. A run with `--vfo-at` no
  longer restores or saves the session, and its VFOs record to the output
  folder unless it is a screenshot run. The CLI run ends when its
  Wireshark is closed.

## The 12667.565 MHz DVB-S2 outbound: encrypted (2026-10-13)

Rory's VFO 24 (not in his plan): 1200.6 kS/s DVB-S2, α 0.20, pilots, short
frames, every BBFRAME good. Looked at from the 2026-10-11 survey dump
(1433 frames, 10 s; same format as the 2721.7 kS/s outbound, 3251 frames):

- BBHEADER 42 00 0000 DFL SYNC 0000: generic continuous, multiple-stream
  flag with only ISI 0, ACM. **SYNC is a frame counter** (+1 every frame,
  mod 256) — continuous GS SYNC is "user private" from B9h up (EN 302 307-1
  §5.1.6), so a vendor's own use of the field. No frame missing in 10 s.
- QPSK 1/4 all the time, except a run of 3–4 QPSK 1/3 frames every ~4 s
  (half the frames on the 2721.7k outbound). DFL fixed per MODCOD: 2888
  bits (1/4, 13 bytes of the BBFRAME unused), 5064 (1/3, 11 unused).
- Data field: E1 65 | 00 05 | 81 | 356 random bytes. Read as GSE that is
  S = E = 1, no label, type 0005h, but its length (DFL − 4) leaves the
  last 2 bytes outside the packet (each GSE variant counts a length error
  a frame): GSE-like vendor framing, the length not counting the type.
- After the constant 81h: 8.00 bits/byte, no field that counts, no CRC-16
  in the last 2 bytes (15 polynomials × init/reflect/xorout, chance hits
  only), no repeated PDU, no idle frame — every frame is full. Link
  encryption at a constant rate (TRANSEC-style: hides even the traffic
  volume). Nothing to decode; the vendor is not identifiable from this.

**Now shown**: `GseIp` keeps a byte histogram of non-IP PDUs; past 32 KiB
their entropy is judged (> 7.95 bits/byte = encrypted). The VFO status
reads "GSE protocol type 0005h ×N, encrypted", the IP card's source "none:
the GSE PDUs are encrypted" and a Contents row (bits/byte, with a tip),
the CLI a "non-IP PDU bytes" line. The survey capture says so after 2 s;
the 1 MS/s outbound (type 0001h timing PDUs) is not flagged. Test
`random_pdus_are_called_encrypted`.

## The 1 MS/s outbounds: one vendor's framing, hub clock, tables (2026-10-13)

12381.26 and 12506.25 MHz (LO 10600), 1000 kS/s, pilots, short frames;
from the 2026-10-11 survey captures (10 s each).

- **Not the demodulator**: Es/N0 from the spectrum (signal over the floor
  beside it) 6.3 and 8.7 dB — the demod's pilot estimate says 6.5 — and
  32APSK 3/4 needs ~10.2: its frames (≈ 44 % of the air time) are 1.5–4 dB
  short at this dish. (The 1.2 MS/s carrier: 19.1 dB by spectrum, 17.0 dB
  by pilots.)
- **One framing on all four outbounds** (these and the encrypted 1.2/2.7
  MS/s ones; 9268 frames, every one fits): GSE_LENGTH counts its own 2-byte
  header; split frag ids (6-bit id, 2-bit counter running 0,1,2,3,0…); the
  last 4 bytes of each data field are a CRC-32 of the BBHEADER and field
  (04C11DB7h, MSB first, register from 0, no final XOR, big-endian). This
  corrects the entry above: the "2 bytes past the packet" were half this
  trailer, the length counting the header. Fragmented PDUs' total length
  reads 9 under the bytes after it and their end CRC-32 fits nothing tried
  (7 polynomials × init × reflection × final XOR × ranges, rebuilt headers):
  a rule of the vendor's; the frame CRC vouches for them.
- **Two streams**: SYNC counts only the QPSK 1/4 frames, which say ISI 0;
  the 32APSK 3/4 and 4/5 frames are another stream (MATYPE's multiple-
  stream flag; its ISI unknown until one decodes).
- **ISI 0 is the hub's signalling**, no IP and no clear text:
  - every frame starts with a whole type-0001h PDU, no label, 8 bytes: a
    big-endian counter running **46.08 counts a symbol** — the deltas
    1157090, 1326194, 1482920, 1542716 are 3×8370, 8370 + 6×3402, 8370 +
    7×3402 and 4×8370 symbols (QPSK 1/4 and 32APSK 3/4 short frames with
    pilots): a network clock reference for TDMA returns;
  - type-0002h PDUs to label 00:ff:ff:10:0c:10, then tag-length-value
    records (1-byte tag, 2-byte length) from the label's last byte, 10h:
    tag 10h (276 bytes: 0038h, a 32-bit value, then **45 × [16-bit id,
    32-bit 0]**, a different id moved to the front each time — the
    terminals, likely), then one of tags 11h (6–18 bytes), 15h (168–880
    bytes of packed 16-bit pairs), 16h, 25h, and tag 17h (60 nn 7c, nn a
    counter). 0001h/0002h are registered ULE next-headers (bridged frame,
    TS-Concat) but used for neither here.
- **Decoder** (`decsat-gse`): `GseIp::bbframe` takes the BBHEADER too,
  checks `frame_trailer` and strips it (a chance fit: 1 in 2³²); fields
  that passed are *verified*, and a reassembled PDU whose fields all were,
  with split counters running on, is passed on whatever its own total
  length and CRC (`GseStats::vouched`). With no IP, the non-IP report uses
  the variant that fits the fields best (fewest overruns, orphans,
  collisions) instead of the standard's. Results: 297 of 299 and 292 of
  293 fragmented PDUs reassembled; the report reads 0001h ×314, 0002h ×324
  (5.5 bits/byte, structured); the encrypted carrier now parses with no
  errors. GUI: a "Framing" row, "reassembled (n by frame CRC)" in GSE
  variants; CLI: a trailer line. Tests build the vendor's frames by hand.

## TDMA bursts: analyser and demodulator (2026-10-14)

Rory, on Intelsat 19: "can you make support for TDMA stuff" — then chose
the burst analyser, burst demodulator and payload decoding, for anything
bursty, as a VFO decoder with a burst panel (DESIGN §3c).

**The live group** (12489–12497 MHz, LO 10600; 4 s at 20 MS/s, LNA 24):
three QPSK inroutes at 1420.7 kS/s beside a 1.27 MS/s DVB-S2 TS carrier —
which turned out to be NZ radio distribution (Newstalk ZB, ZM, Hauraki… as
multicast AAC), not their outbound. Prototyped in Python first, then
measured by the new decoder:

| carrier | burst | slot | frame | idle |
|---|---|---|---|---|
| 12491.29 | 852 symbols | 869.00 symbols, 0.6117 ms, guard 17 | 125.000 ms = 200 slots + 2.678 ms silence | 47 % |
| 12492.96 | 852 symbols | 865.00 symbols, 0.6089 ms, guard 13 | 125.000 ms = 201 slots + 2.615 ms silence | ~50 % |
| 12489.62 | 1135 symbols | 1151.0 symbols, 0.8101 ms, guard 18 | 125.0 ms = 152 slots + ~1.9 ms silence | ? |

Layout of the 852-symbol bursts: preamble 0–35 with symbols 13–17 varying
(an ID or type field?), two pilot symbols every 30 (0/1, 30/31 … 840/841),
an 18-symbol fixed field at 704–721 (three of its symbols vary on one
carrier), ~750 data symbols = ~1500 coded bits. The 1135-symbol bursts
share the preamble and pilots, ~980 data symbols. Every terminal's carrier
is within ±80 Hz (locked to the hub); they separate by power (6 dB spread)
and by slot offset (±0.3–4 symbols). 125 ms frames are what iDirect
inroutes use — a hint, not an identification.

**Built** (`decsat-engine::tdma`, `DecoderKind::Tdma`, CLI `--decoder tdma`,
GUI panel with a burst map, `.bursts` + `.csv` on Record): see DESIGN §3c.
Lessons on the way, each from the live capture:

- the noise floor must be a deep percentile (0.3 %): the carrier is on 98 %
  of the time and noise shows only in the 13–18-symbol guard times;
- thresholds placed in dB fire 3 symbols early at high SNR, in linear power
  miss terminals 3 dB under the rest: detect in dB, then set each burst's
  edges at half its own level;
- a state change must hold 3 symbols, or a 7 dB carrier's bursts break up;
- a frame need not hold whole slots (204¼ here): one grid over many frames
  smeared to "±104 symbols"; slots are now counted from each frame's start,
  the frame found on its silences;
- bursts merging over a 13-symbol guard (8 %) are split again a slot apart;
- per-burst carrier estimates scatter ±300 Hz at 6 dB: terminal grouping
  scales its tolerance to the measured scatter;
- speed: a VFO-style down-converter per channel (2.9 s of every 4 s) →
  mixer + ~30-tap anti-alias FIR; a planner per FFT and a log per sample
  were the rest. Three carriers now take 2.4 s per 4 s; carrier discovery
  0.3 s (Identify in full took 1.7 s).

**Open**: the weakest carrier (6.9 dB over the floor) still breaks a third
of its bursts — burst detection on the preamble is the fix; the coding in
the data symbols (T3).

## TDMA: inside the bursts (2026-10-14)

From the `.bursts` file of the 4 s capture (Python in that session's
scratchpad: `code1/2.py`, `ser.py`, `wht.py`, `hdr.py`):

- **Idle fill is one codeword** on each carrier (1204 and 1240 bursts);
  every busy burst is different from every other (1899 of 1903 unique), and
  agrees with the fill only by chance (25 %): scrambled or coded all
  through.
- **The demodulator is near the channel's limit**: against the fill, the
  symbol error rate is 6.6 % (best tenth 0.8 %), flat along the burst; that
  is Es/N0 ≈ 5.3 dB, which the burst-over-floor reading agrees with once
  the floor percentile's bias is taken off. The SNR, not the demod.
- **Layout of a burst** (852 symbols): preamble with a 5-symbol field
  (13–17), ~624 data symbols, the 18-symbol fixed word (704–721), ~123 data
  symbols. On carrier 1 idle bursts keep the first part constant but the
  trailer after the fixed word differs on every burst (3092 distinct of
  3107) — per-burst information (a counter, an acknowledgement…) even when
  idle.
- **The 5-symbol field names the terminal**: ~17 values on carrier 1, idle
  bursts using 7 and busy ones 10, each value always at one power
  (`23112` at −32 dB: the strong idle-only terminal). Its 10 bits keep
  strong parity checks (bits 0+1+4+5, 4+7, 6+9 with biases 0.85, 0.78,
  0.52): a small block code. The decoder now lists terminals by it.
- **No local structure in the main codeword**: no parity check inside any
  14-bit window (biases ≤ 0.14 against 0.10 of noise, 1900 bursts, busy
  XOR fill so a fixed scrambler cancels). Not a convolutional code in plain
  order, not a short product code row by row: turbo, LDPC or an interleaved
  product code. Weak structure at bits 1275–1285 (bias 0.30), where the
  trailer begins.

Next for the coding: a cleaner capture (more LNA/amp, the dish peaked) so
rank-based code recognition works, and the network's outbound — its
signalling names the system and may carry the burst plan.
## Intelsat 19's narrow carriers at 12333.60–12333.68 MHz; Bluetooth in the HackRF's IF (2026-10-14)

Rory's screenshot: a row of narrow carriers near 12333.6 MHz, and "the
radio is yours". The dish is on **Intelsat 19** (166° E): the GUI's names
for them (ML-LEC, ASA084 LEC-ML) came from the H3e plan, still loaded, and
do not apply. Recorded 30 s and 5 min at 4 MS/s, centre 1732.9 MHz (LNA
24, VGA 20).

- **Five carriers**, ~16.5 kHz apart: +735.8, +751.4, +768.2, +784.4 and
  +816.5 kHz from 1732.9 MHz, ≈ 12333.603, .618, .635, .651 and .684 MHz
  RF. All the same chain as H3e's 10.24 kBd carrier of 2026-10-09: QPSK
  10 240 Bd, K = 7 rate ½, differential, V.35 (taps 3, 20), IBS/SMS
  framing (IESS-309, 16/15) → 9.6 kbit/s. The CDM-570L Viterbi decoder
  takes each as it is (the modems' make is not known: the chain is
  IESS-308/309's). IBS overhead cycle E4 00 20 00 on all five; bit 10h
  of the 20h octet carries one fixed 16-bit pattern every 64 cycles (3.2 s),
  the same on every carrier — framing housekeeping, not a service channel.
- **Idle**, all five, for the whole 5 minutes: four at constant 0, the
  +816.5 kHz one at constant 1. The 300–700-bit stretches of random-looking
  data on three of them were carrier slips, not traffic: their count
  matched the Viterbi decoder's "turns followed" exactly (68 / 5 / 0 / 73 /
  1 in 5 min), the output garbage until it follows the new orientation.
- **The slips were Bluetooth**, not the satellite (my first reading, an
  overlapping 1 MBd TDMA network, was wrong; Rory: "bluetooth"). Below
  2.17 GHz the HackRF mixes up to a 2.34–2.65 GHz IF — 2650 − f/7 MHz,
  firmware `tuning.c`, `select_graduated_if` — so tuned to 1732.9 MHz its
  IF is 2402.4 MHz, on Bluetooth LE advertising channel 37 (2402 MHz).
  Nearby devices' adverts showed as ~0.47 ms bursts, ~1 MHz wide, centred
  +0.48 MHz, ~19 a second in two ~105 ms trains (two devices), 1 Msym/s,
  all starting with the same ~57 symbols (one access address), invisible on
  the averaged spectrum. They lifted the carriers' in-band power 3–20× for
  ~5 symbols; the two at Es/N0 ≈ 8 dB slipped every ~4 s (29 and 32 times
  in 120 s). With the bursts zeroed in the IQ (1.6 % of samples): once
  and not at all, and their MER read ~2 dB higher.
- **Tuned off centre** (Rory's suggestion), 1735.5 MHz at 8 MS/s: the leak
  moved to 0…+0.5 MHz (it sits ≈ 248 − f/7 MHz from the centre, inverted),
  the carriers to −1.86…−1.78 MHz, and the release build decodes them with
  0 / 0 / 1 / 1 / 1 turns in 60 s at Es/N0 14.7 / 16.5 / 19.5 / 11.6 /
  19.6 dB (8.6 / 10.0 / 16.5 / 8.2 / 16.6 at 1732.9 MHz). BLE advertising
  also uses 2426 and 2480 MHz: the same leak near tunings of 1568 and
  1190 MHz.
- **Burst blanking** in `PskDemod` (`psk.rs`, `Blanker`; every generic-PSK
  decoder: Viterbi, TPC, FastLink, DVB-S, CID, PSK, Identify's live view),
  for when the leak and a carrier cannot be kept apart. Each symbol's power
  before the AGC, averaged over 5 symbols, against the carrier's own mean
  and spread (5σ of the window mean, at least 2 dB); found bursts (±2
  symbols) coast the carrier loop and are passed on weighed by N / (N + I),
  I read from the quietest 3-symbol window holding the symbol less 3σ of
  its spread, N from the power's mean and variance (N = m − √(m² − v)).
  The weighting mattered as much as the coasting — error events in 100 s
  of the five idle streams (+735 / +751 / +768 / +784 / +816 kHz):

  | | events |
  |---|---|
  | release build | 1455 / 1214 / 43 / 1409 / 17 |
  | coasting, symbols at full weight | 1442 / 1217 / 46 / 1379 / 16 |
  | coasting, symbols erased | 1943 / 1860 / 1698 / 1858 / 1436 |
  | N / (N + I), I over the 5-symbol window | 451 / 702 / 483 / 698 / 168 |
  | as built | 258 / 127 / 53 / 109 / 6 |

  Erasing ~9 symbols a burst is more than a K = 7 code bridges (~6), and on
  a strong carrier a clean symbol only 1σ loud weighed 0.08 under the wide
  window. Slips in 120 s: 29 → 1 and 32 → 2. Nine earlier captures (TPC
  QPSK/8PSK, FastLink ×2, CDM-600 D&I++, Viterbi 64k/9.6k) decode the same
  as before (one TPC frame fewer failed). Live, 60 s of the +784 kHz carrier
  at the old tuning: 1 turn, IBS misaligned 0.2 % (3.3 % before). The
  status reads "N interference bursts coasted" after the Es/N0 (CLI, GUI).
  Tests `coasts_through_bursts_without_slipping` (14 slips without the
  finder, 0 with), `finds_no_bursts_on_a_clean_carrier`.

## Plan and carrier labels per satellite (2026-10-14)

Rory, on Intelsat 19: "its not LEC-ML … can you set the labels to only show
when on that satellite". The H3e plan and the labels learned on H3e named
Intelsat 19's carriers at the same frequencies (a VFO took the name ASA084
LEC-ML), because neither knew its satellite.

- **Toolbar satellite list** (beside 📋 Plan…): the satellites named, *Add*
  a new one (no plan or labels yet), *Rename this one*. Choosing one loads
  its own plan and carrier labels and clears the survey's results; the
  plan button and *Reset labels* act on the chosen one's.
- **Prefs**: `satellites=` (names, `|`-separated), `satellite=` (chosen),
  `freqplan.<slug>` and `carrier_caps.<slug>.rf<kHz>khz` per satellite
  (slug: letters/digits lower case, else `_` — "Intelsat 19" →
  `intelsat_19`). The old `freqplan` / `carrier_caps.rf…` keys are the
  unnamed satellite's, shown as *(unnamed)* until renamed, so nothing
  changes until a satellite is added. `--plan FILE` still loads a plan for
  the run whatever is chosen. Radar places stay keyed by radar.
- Test `each_satellite_keeps_its_own_plan_and_labels`.

## Convolutional codes found blind; the generic PSK decoder looks for one (2026-10-14)

Rory: "add support in the generic psk for 1/2 rate and other stuff that
could be used and wire that into the auto ident system". The Intelsat 19
group (12333.2–12336.2 MHz, 90 s at 8 MS/s, centre 1733.8 MHz so the
Bluetooth leak sits clear) had prompted it: none of its PSK carriers is
K = 7, TPC 2964 or FastLink.

- **`decsat-modem::convblind`**: any rate-½ convolutional code, K ≤ 10. A
  code with generators g₁, g₂ satisfies g₂·c₁ + g₁·c₂ = 0 — a parity check
  of span K whose taps are the generators reversed — so windows of K steps
  have one null vector, and none for a shorter K. Null spaces of random
  handfuls of windows (an all-ones column added, so a check holding as 1 —
  an inverted output — comes out too), each candidate judged on 6000
  windows (≥ 0.62 to count; a true check of weight w holds in
  (1 + (1 − 2p)^w)/2), shortest K first, after the windows prove varied
  enough (an idle pattern fits many checks and proves none). Generic soft
  Viterbi (2^(K−1) states), the outputs' inversion picked by decode and
  re-encode; `ConvRx` searches 6000 symbols at a time (8 looks, then gives
  up), decodes, checks the code's own parity check every 512 symbols and
  follows 90° turns (keeping the mirror; a ±90° slip cannot be told apart
  when the check has even weight, so the data after it may come out
  inverted — as with the K = 7 code). On QPSK the receiver reads the
  carrier the way up that needs no inverted output. One orientation is
  enough to find the code (turned or mirrored it is the same code, outputs
  swapped or inverted). Tests: K = 3, 5, 7, 9 found exactly through 0.5 %
  errors at either pair alignment; K = 7 identical to `conv::Encoder`;
  random and idle bits give nothing; a 90° turn followed; BPSK with an
  inverted output (CCSDS-style); every QPSK orientation × alignment comes
  out uninverted and unturned; an uncoded carrier given up on.
- **The Viterbi chain** (`FecInput::Viterbi { symbols, bpsk, bounded }`):
  the K = 7 `ViterbiRx` (QPSK) and `ConvRx` side by side, the first to lock
  decodes; BPSK now allowed (rate-½ search only). Status: "K = 7 rate 1/2",
  "K = 9, rate 1/2 (561, 753)", the search's progress, or "no
  convolutional code: not K = 7 at any rate, nor a rate-½ code up to K = 10
  — a block code (TPC, LDPC, turbo) or uncoded". `cdm_model` keeps a
  non-K = 7 code off the CDM-570L name. GUI: one "Code" row (`code_row`).
- **Generic PSK** (BPSK/QPSK): its own FEC worker runs the same search,
  bounded (K = 7 stops after 40 tries) — Code/Payload/Framing rows and the
  decoded data's text in its panel; Record writes the decoded data beside
  the symbols.
- **Identify**: `conv_code` from the locked symbols; the hint names the
  code ("QPSK, convolutional code K = 7, rate 1/2 (171, 133) — the code of
  DVB-S (not verified) and IESS-308/309, found from its parity checks").
  The type-found modem, the survey and `scan --decode` already send BPSK
  and QPSK carriers to the Viterbi chain, which now decodes any such code.
- **Checked**: the H3e 10.24 kBd IBS carriers — Identify names K = 7 (171,
  133), Generic PSK and Viterbi decode to IBS framing; synthetic K = 9 (561,
  753) BPSK and K = 5 (23, 35) QPSK through `--decoder auto`: Identify names
  them and they decode (BER 0 at 8.8 dB); Intelsat 19's 102.4 kS/s and
  5.133 kBd QPSK carriers: "no convolutional code" after the bounded search
  — the 102.4 kS/s one repeats a 40 112-symbol frame (idle, ~98 % identical
  frame to frame after ~5 s of traffic): a block code with a frame-
  synchronous scrambler, coding unknown.

## SCPC or TDMA: a mode per VFO, bursty bands on the waterfall (2026-10-14)

Rory: "maybe it would be a good idea for the VFO to have 2 modes SCPC or
TDMA to separate signal detection" — then, asked: per VFO with a default
for new ones; two detectors on the waterfall; TDMA running the analyser and
the code search.

- **`bursty.rs`**: `BurstMeter` takes the waterfall's own FFT segments
  (`Spectrum::compute_max_with` hands each to a callback: no extra FFTs),
  averages them into ~2 ms frames smoothed over ~4 kHz, and every 2 s finds
  bins whose coefficient of variation is ≥ 1.6× the noise's (measured on
  the bins at the floor, the 20th percentile of the passband) and ≥ 0.6 dB
  up; runs with gaps ≤ 20 kHz make a band (an MF-TDMA group's carriers one
  band), ≥ 3 kHz wide. On the Intelsat 19 capture (8 MS/s, 1733.8 MHz)
  every continuous carrier, 1–17 dB up, sat at the noise's CV (0.2–0.3);
  the MF-TDMA group whose bursts are ~5 dB up read 0.7–1.9 and makes one
  band (+1.48…+1.62 MHz, 11 carriers for the analyser), more bursty bands at
  12331.0–12331.4 and 12332.0–12332.25 MHz. An averaged-spectrum
  threshold, tried first, missed the group and called carrier skirts
  bursty. `bluetooth_offsets(tuning)` (HackRF IF 2650 − f/7 MHz; BLE
  advertising 2402/2426/2480 MHz, spectrally inverted) marks the
  Bluetooth leak (there: −0.07…+1.02 MHz, varying 4–5× the noise).
  `FrontStatus.burst_bands`, `burst_takes`.
- **`SignalMode` { Scpc, Tdma }** on `VfoSettings.signal` (from the decoder
  by default; `DecoderKind::fits`: TDMA bursts in TDMA, the recorder and
  spectrum in both, the rest SCPC); the session keeps it (`mode=`; older
  lines take the decoder's). GUI: a *Signal* row above *Decoder*, the
  decoder list filtered; *New VFOs run* has the switch.
- **Waterfall**: violet dashed brackets under the green ones (grey,
  "Bluetooth", where the leak would be for the HackRF's tuning or the
  file's), hover text, click → a TDMA VFO over the band (+15 % + 4 kHz).
  The survey's/claim's `pick` sends a bursty carrier to TDMA (it was "no
  decoder"); `scan --decode` likewise, and `scan` lists the bands.
- **TDMA code search**: per channel, fitted non-idle bursts' data (phase
  fixed by the layout's known symbols) → hard bits, one piece a burst;
  every 12 000 bits `convblind::find_pieces` (no window across bursts) up
  to 6 looks; `ChannelView.code` → CLI "code: …", GUI "Code" row. The
  Intelsat 19 group's bursts (MER 4.5 dB) never yield a layout, so it waits.
- Tests: `finds_the_bursty_channel_not_the_steady_carrier`,
  `bluetooth_lands_where_it_was_seen`, `finds_a_code_in_separate_bursts`,
  `the_signal_mode_is_kept_and_old_lines_take_the_decoders`.

## Slow TDMA: the analyser waits for bursts and keeps looking (2026-10-14)

Rory, on a live Intelsat 19 return: "the TDMA vfo does not wait long enough
for slow TDMA that bursts every couple of seconds". The analyser took the
carriers from a 0.4 s first look at an averaged spectrum: a carrier silent
then, or averaged into the noise, was never found.

- **Peak hold** (`PeakHold`: 4096-point segments, 3-bin smoothed, max over
  the stretch) instead of the average; the first look goes on, "listening
  for bursts… n s", until a carrier shows (or 20 s: then the old whole-VFO
  fallback, and it keeps listening).
- **Looks again every 5 s** over the peak hold of the last two stretches
  (~10 s); a carrier no channel covers is added at the end of the list
  (numbers kept for the file and the map) and decodes what was kept of its
  past — the last 12 s of baseband (at most 4 M samples), the channel's
  `t0_s` putting its bursts on the VFO's clock.
- **A new carrier's rate from its bursts alone** (`bursts_only`: ~16-symbol
  blocks 3 dB over the quietest tenth, a block either side): the silence no
  longer buries its spectrum and cyclic line.
- **Waterfall**: the bursty bands shown are the union of the last four 2 s
  takes (`bursty::union`), so a slow carrier's bracket stays.
- On the 90 s Intelsat 19 capture the 175 kHz group now reads as a regular
  MF-TDMA grid: 17 channels 7.7 kHz apart (−80.6 … +73.8 kHz), mostly
  6.4 kS/s (6.4 × 1.2 = 7.7), where the averaged look found 11; the last
  one added by a re-look. Test
  `waits_for_a_slow_carrier_and_finds_one_that_starts_later` (a burst every
  2 s from 0.9 s, found by ~1 s; a second carrier from 7 s, added with its
  earlier bursts), `takes_merge_into_one_list`.
