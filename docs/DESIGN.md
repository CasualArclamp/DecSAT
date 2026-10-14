# DecSAT — Design

A Rust **DVB-S2 / DVB-S2X** receiver with **ACM** (Adaptive Coding & Modulation), **GSE** de-encapsulation to **IP (PCAP)**, **TS** extraction, and a matching **modulator** for loopback and ACM test-signal generation. HackRF One based, with IQ-file replay.

Status date: 2026-10-08 (kickoff). This document is the source of truth for scope and milestones — read it before continuing work. Decisions here come from the kickoff Q&A with Rory.

---

## 1. Goals & non-goals

**Goals**
- Receive and decode DVB-S2 and DVB-S2X signals that fit a HackRF One (symbol rate up to ~15 MS/s; practically 125 kS–10 MS/s is the sweet spot).
- Full **ACM/VCM**: demodulate each PLFRAME at whatever MODCOD its PLHEADER declares, frame by frame, with no prior knowledge.
- De-encapsulate **GSE** → reassemble **IPv4/IPv6** → write **PCAP** + show live stream stats. Handle the proprietary GSE variants real operators use (see §6).
- Extract **TS-mode** BBFRAMEs → `.ts` (most amateur DATV is still TS).
- A **modulator** (TX) that produces spec-correct S2/S2X signals (HackRF TX or IQ file) so we can generate ACM/GSE test signals and run end-to-end loopback — real ACM/GSE captures are scarce.
- egui/glow GUI + CLI, same look/feel as DecDRM. Portable exes + GitHub releases + CI.

**Non-goals (for now)**
- Channel bonding (S2X Annex D) and wideband / time-slicing (Annex M) — need multiple tuners or >20 MHz; file-only if ever.
- Full two-way ACM link with a return channel (we build the pieces — RX SNR estimate + TX per-frame MODCOD — but not the closed loop over a radio return path now).
- Conditional access / descrambling of encrypted payloads. We decode the transport, not break crypto.
- Formal DVB certification. "Compliant" here = matches the normative bit/symbol processing and verifies against reference vectors for everything a HackRF can receive.

**Targets:** QO-100 & amateur DATV; narrow Ku/C SCPC/VSAT IP carriers; wide broadcast muxes (file replay only — too wide for HackRF live); own HackRF TX later.

---

## 2. Signal chain (RX)

```
HackRF / IQ file
  → AGC + DC/IQ-balance
  → resample to N samples/symbol
  → matched filter (RRC, roll-off from PLHEADER search / config)
  → timing recovery (Gardner)
  → coarse+fine carrier frequency & phase recovery
  → PL sync: SOF correlation (25 symbols) + PLS code decode (64→7 bits)  → MODCOD, FECFRAME type, pilots?
  → pilot-aided phase tracking
  → PL descramble (gold code, index from config/search)
  → de-map to soft bits (LLRs) for the declared MODCOD
  → LDPC decode (DVB-S2/S2X H-matrices; normal 64800 / short 16200 / medium 32400)
  → BCH decode (outer)
  → BBFRAME  → BBHEADER parse (CRC-8) → MATYPE (TS/GS, SIS/MIS, CCM/ACM, ISSYI, NPD, RO), DFL, SYNC, UPL …
      ├─ GS (generic stream) → GSE de-encapsulation → reassembly → IP → PCAP + stats
      └─ TS (transport)      → TS reassembly (incl. CRC variants) → .ts
```

ACM/VCM is inherent: every PLFRAME carries its own MODCOD in the PLHEADER, so the demod re-selects constellation + LDPC rate per frame. VCM = fixed set over streams; ACM = time-varying; same code path.

## 3. Signal chain (TX / modulator)

Mirror of RX: input bytes (TS, or GSE-encapsulated IP, or BBFRAME) → BBHEADER + CRC-8 → BB scrambling → BCH encode → LDPC encode → bit interleave → map to constellation → PL framing (SOF + PLS + pilots) → PL scramble → RRC pulse shape → resample → HackRF TX / IQ file. Used for loopback tests and ACM test-signal sequences (script a MODCOD schedule).

---

## 3a. Application shape: wideband waterfall + VFOs

Decided 2026-10-08, at Rory's request: the app works like **SDR++**, not like a
single-carrier decoder. This shapes the engine, so it lands before the FEC work.

```
IqSource (HackRF 20 MS/s, or a wideband file)
  │
  ├─► wideband FFT ──► spectrum + scrolling waterfall (the whole span)
  │
  ├─► carrier detector ──► candidate carriers marked on the waterfall
  │
  └─► ring buffer ──┬─► VFO 1: DDC (NCO mix + decimate) ─► decoder ─► output
                    ├─► VFO 2: DDC ─► decoder ─► output
                    └─► VFO n: …                    (one worker thread each)
```

- A **VFO** is a centre frequency, a bandwidth and a chosen decoder. Drop one by
  dragging on the waterfall, or by clicking a detected carrier (which sizes it
  correctly for you). Several decode at once, one worker thread each, with a CPU
  meter per VFO and an enable/disable that does not delete it.
- **Carrier detection** sweeps the band estimator (§ `decsat-engine::estimate`)
  across the span and marks each candidate with its centre and estimated symbol
  rate. The estimate is good to a few percent, which is close enough to seed a
  VFO.
- **Layout**: combined spectrum + waterfall filling the top of the window, a
  side bar listing the VFOs, and the selected VFO's decoder settings,
  constellation and statistics below it.

### Decoders a VFO can run

| Decoder | What it does | Needs |
|---|---|---|
| **Identify** ("what is this?") | Blind: measures symbol rate and roll-off, estimates constellation order, and identifies the standard. See §3b. | nothing beyond M1 |
| **DVB-S2/S2X → GSE/IP** | Full ACM demod → LDPC/BCH → GSE → IP → PCAP + stream stats | M2–M4 |
| **DVB-S2/S2X → MPEG-TS** | Same demod, TS-mode BBFRAMEs → `.ts` or UDP | M2, Appendix C |
| **Generic PSK → symbols** | Any linearly modulated carrier: blind rate, carrier lock, hard decisions to a `.bin`, one byte per symbol | done |
| **TDMA bursts** | One carrier or an MF-TDMA group: bursts found and demodulated one by one, their layout (preamble, pilots, fixed fields, idle fill) read blind, slot grid, frame and terminals; data symbols to a `.bursts` file. See §3c. | T1–T2 done |
| **IQ recorder** | That VFO's narrowband IQ to a file | M1 |
| **Spectrum only** | Zoomed spectrum, level, occupied bandwidth; no decode | done |

## 3b. "What is this?" — the Identify decoder

Drag a VFO over an unknown carrier and it reports what it can. Deliberately
split into what can be known for certain and what can only be guessed, because
over-claiming here would be worse than useless.

**Measured (no standard assumed):**
- **Symbol rate.** Seeded by the equivalent-noise-bandwidth estimate (exact for
  a root-raised-cosine spectrum at any roll-off), then refined by *spectral
  self-correlation*: a linearly modulated signal with excess bandwidth
  (α > 0) has correlated spectral components exactly `Rs` apart, so
  `C(ν) = Σ_f X(f)·conj(X(f−ν))` peaks at `ν = Rs`. Finally polished by
  maximising a timing-error metric over candidate rates.
- **Roll-off**, as occupied bandwidth / Rs − 1, snapped to the legal S2/S2X set
  {0.35, 0.25, 0.20, 0.15, 0.10, 0.05}.
- **Constellation order**, from the amplitude histogram: counting rings
  separates QPSK / 8PSK / 16APSK / 32APSK without decoding anything.

**Identified with certainty:**
- **DVB-S2 / S2X.** Run [`PlHeaderCorrelator`]: a PLHEADER correlation peak
  repeating at exactly the spacing the decoded PLS code predicts is a very
  strong signature — a 90-symbol known pattern recurring on a predicted grid.
  That also yields the MODCOD, FECFRAME length, pilots, and whether the frame
  is a dummy. S2X-only features (VL-SNR header, superframing) separate S2X
  from S2.

**Reported as a guess, labelled as one:**
- Anything with no PLHEADER. QPSK with no PLHEADER peak is *consistent with*
  DVB-S, but confirming it needs Viterbi plus the 204-byte RS frame sync, which
  DecSAT does not implement; the UI will say "QPSK, no DVB-S2 PLHEADER —
  possibly DVB-S" and not pretend otherwise.
- Unrecognised signals get their measured parameters and an explicit "does not
  match DVB-S2/S2X", which is honest and still useful.
- A CW tone or an empty band is called out from the band estimate alone (very
  narrow with a high peak-to-floor, or nothing above the floor).

## 3c. TDMA bursts

Asked for 2026-10-14 ("can you make support for TDMA stuff"). Rory's
answers: a **burst analyser**, a **burst demodulator** and **payload
decoding**, in that order; targets **anything bursty** (generic blind tools
first, vendors as captures come — the kit on the live IS-19 group is not
known); a **VFO decoder with a burst panel**, one VFO able to span a whole
group.

`decsat-engine::tdma`, per VFO:

1. **Channels**: the first look's carriers (spectrum), each with its rate
   (cyclic line of the envelope), roll-off (fit to the masked spectrum) and
   centre; then a mixer, a light decimating FIR (the matched filter after it
   removes the neighbours, so it only stops aliases) and the RRC matched
   filter, at ≥ 3.2 samples a symbol.
2. **Bursts** (`burst.rs`): envelope over 6 symbols against a forgetting
   histogram — floor = its 0.3 % point (a loaded carrier shows noise only in
   its guard times), bursts' level = the median clear of it; hysteresis at
   60/40 % of the dB span, a state change held 3 symbols; edges then set per
   burst at half its own level.
3. **Demodulation** (`demod.rs`), each burst on its own: Oerder & Meyr
   timing, carrier from the M-th power (FFT, then block phases fitted with
   a line); once every burst's carrier agrees (terminals locked to the hub)
   the FFT is skipped.
4. **Layout** (`layout.rs`): bursts of the commonest length lined up on one
   (lag and phase), averaged per position; idle fill (bursts matching the
   average everywhere) set aside; |mean| > 0.6 = fixed. Runs read as
   preamble (with a varying field inside), pilots (commonest spacing and
   residue), fixed fields, postamble. Each burst is then fitted: lag and
   phase from the fixed symbols, phase tracked through them, data symbols
   out, idle flagged.
5. **Time plan** (`slots.rs`): slot from consecutive spacings (least
   squares over 1–4-slot spacings); frame from the empty-slot
   autocorrelation, made exact on the silences (longest gaps, one a frame):
   pairwise spacing / frames, median, then least squares. Slots are counted
   from each frame's start — frames need not hold whole slots.
   **Terminals**: bursts grouped by power, carrier and offset in the slot
   (the carrier tolerance scaled to the measured scatter).

6. **Terminals by field**: when the preamble holds a short varying field
   (≤ 12 symbols), each burst's value (snapped to the commonest within a
   symbol; small values a symbol from a far commoner one folded in) names
   its terminal — live, one value a terminal, each at its own power.

T3 so far (2026-10-14, the live group): a burst is preamble + a 5-symbol
field (the terminal; 10 bits with strong short parity checks — a small
block code) + a ~624-symbol main codeword + an 18-symbol fixed word + a
~123-symbol trailer that differs on every burst, idle ones too. The main
codeword shows no parity check within any 14-bit window (Walsh–Hadamard of
every window's patterns over 1900 bursts, busy XOR the fill): a long code
— turbo, LDPC or an interleaved product code. At ~5 dB Es/N0 (symbol error
rate 6.6 % against the fill) blind rank methods fail; a cleaner capture, or
the network's outbound to name the vendor, comes first. Weak carriers
(≤ 7 dB) still fragment; detecting bursts on their preamble once the layout
is known is the fix.

## 4. Workspace layout

Cargo workspace `decsat`, modelled on DecDRM.

```
crates/
  decsat-core     shared types: MODCOD tables, FECFRAME params, roll-offs,
                  BBHEADER/MATYPE, config, error types, SNR/metrics structs
  decsat-dsp      AGC, DC/IQ correction, resampler, RRC filter, Gardner TED,
                  carrier recovery (coarse FFT + fine PLL), interpolators
  decsat-fec      LDPC decoder+encoder (all rates, 3 frame lengths), BCH,
                  bit (de)interleaver, (de)mapper + LLR for QPSK..256APSK, pi/2-BPSK
  decsat-frame    PLHEADER (SOF + PLS 64-bit), MODCOD/type decode, dummy frames,
                  pilot insertion/removal, PL (de)scrambler, VL-SNR header,
                  superframe (Annex E), BBFRAME assembly, BBHEADER + CRC-8
  decsat-gse      GSE de-encapsulation + reassembly; standard + proprietary
                  variants (header-len, split frag-id); label/protocol handling
  decsat-ts       TS-mode BBFRAME → MPEG-TS; null-packet reinsertion, sync,
                  Generic/Newtec CRC variants
  decsat-ip       IPv4/IPv6 parse, PCAP writer and reader, stream
                  classification + stats, blind IP-header-checksum fallback
                  search; a radar station's monitoring multicast (rcms)
  decsat-audio    multicast radio in the app: RTP/RFC 2250/3016/3640/7587
                  and raw ES depacketising, MPEG audio (Symphonia), AAC
                  with SBR/PS (libxaac), Opus (libopus), resampling, sound output
                  (cpal), recording as broadcast
  decsat-opus-sys libopus (BSD), the third_party/opus submodule (v1.6.1),
                  built with CMake; FFI bindings (from DecDRM)
  decsat-xaac-sys libxaac's AAC decoder (Apache-2.0), the third_party/libxaac
                  submodule (v0.1.13), built with cc; FFI bindings
  decsat-modem    other satellite modem formats: DVB-S (Viterbi K = 7 with
                  puncturing, RS(204,188), Forney interleaving, energy dispersal),
                  generic RS over GF(256); turbo product codes and IESS-315
                  tpc_2964 (structure found blind); modem payloads (HDLC,
                  MPEG-TS, descramblers found blind); E1/D&I++ timeslots,
                  G.711, and G.728 LD-CELP (decoder from the Recommendation,
                  plus the robbed-sync-bit framing that finds it in a
                  timeslot's sub-rate bits)
  decsat-io       HackRF source (libhackrf FFI / soapy), IQ file reader/writer
                  (cs8/cs16/cf32), HackRF TX sink, ring buffers
  decsat-engine   orchestrates RX (and TX) chain; ACM state; multistream/ISI
                  filter; gold-code handling; metrics aggregation
apps/
  decsat-cli      headless decode / modulate
  decsat-gui      egui/glow: constellation, spectrum, ACM timeline, stream table
docs/ DESIGN.md, reference/ (git-ignored), samples/ (git-ignored), exe/, scripts/, .github/
```

## 5. FEC — the heavy part

- **LDPC**: DVB-S2 (EN 302 307-1) + S2X (EN 302 307-2) parity-check matrices. Frame lengths 64800 (normal), 16200 (short), 32400 (S2X medium). Rates from 1/4 … 9/10 (S2) plus the many S2X rates (e.g. 13/45, 9/20, 11/20, 26/45, 28/45, 23/36, 25/36, 13/18, 90/180 …). Layered/min-sum decoder with normalized min-sum, a few iterations, early-stop on BCH/parity. Port structure + the rate tables from **gr-dvbs2rx** / **leansdr** (GPL-3), re-expressed in Rust.
- **BCH**: outer code, t = 8/10/12 correctable, GF(2^16)/(2^14)/(2^15) per frame length.
- **Interleaver**: column-twist bit interleaver for 8PSK+ (S2) and the S2X variants.
- **Demapper**: Gray-mapped QPSK/8PSK, and the APSK ring constellations (16/32/64/128/256APSK) with the ring ratios γ per MODCOD from the spec tables; soft LLRs (approx-LLR with per-ring scaling). pi/2-BPSK for VL-SNR.

Verification: compare our encode→decode and demap/LLR against vectors generated by gr-dtv (TX) and gr-dvbs2rx (RX) for each MODCOD. Reference clones are git-ignored; vectors are generated locally (CI can regenerate small ones or carry committed golden vectors that contain no third-party code).

## 6. GSE / IP (the headline feature, per dontlookup)

After BBFRAMEs exist, GS-mode payload is GSE. Implement:
- Standard GSE (EN 301 545 / TS 102 606): Start/End fragment flags, 1-byte vs no fragment-id, length field, LT (label type) 6B/3B/broadcast/reuse, PROTOCOL-TYPE / extension headers, total-length + CRC-32 on reassembly.
- **Proprietary variants** (from dontlookup, MIT): (a) non-standard **2-byte header-length** field (`hdrlen-2`); (b) **split fragment-id** = 6-bit frag-id + 2-bit counter instead of 8-bit. → run the 4 combinations and let the user/validator pick the one that yields valid IP (IP-header checksum + sane lengths).
- **Blind IP search** fallback: scan BBFRAME payload byte offsets for an IPv4 header whose checksum validates (dontlookup's approach) when framing is unknown; also a byte-pair-swap pass.
- Reassemble fragments per frag-id → IP datagram → **PCAP** (DLT_RAW or DLT_EN10MB with a synthetic MAC) + live stats (per src/dst, protocol, pps, bps). TS-mode handled in `decsat-ts`.

## 7. Input / output

- **Input:** HackRF live via `libhackrf` FFI (preferred; a `libhackrf.dll` is already present on this PC) — optionally SoapySDR later. IQ files: HackRF `cs8` (int8 I/Q), `cs16`, `cf32`. Center-freq / sample-rate / gain in config; LNB LO offset handling for Ku.
- **Output:** `.pcap` for IP; `.ts` for transport; GUI panels; CSV/JSON metrics.

## 8. Platform, build, release

- Windows 11 primary, Linux x86_64 too. egui/eframe with the **glow** (OpenGL) renderer (as DecDRM — keeps the Rust-version floor sane). `rustfft` for DSP.
- Rust throughout, with two C libraries (2026-10-10), there being no mature Rust decoders for them: **libopus** (BSD; `third_party/opus`, xiph/opus v1.6.1, built by CMake in `decsat-opus-sys`, as DecDRM) and **libxaac**'s decoder (Apache-2.0, GPL-3-compatible; `third_party/libxaac`, ittiam-systems v0.1.13, built by `cc` from its own CMake source lists in `decsat-xaac-sys`) for AAC with SBR and PS. FDK-AAC, which DecDRM decodes with, is not used: its licence is not GPL-compatible. A build needs CMake and a C compiler; CI checks out submodules.
- Portable static-CRT exes in `exe/` + attached to GitHub releases; CI on GitHub Actions (Linux+Windows tests, clippy, smoke); release.yml drafts portable exes from a pushed tag. See memory [[feedback-release-and-ci]].
- Repo: public `CasualArclamp/DecSAT` (DecDVB until 2026-10-10; GitHub forwards the old name), **GPL-3.0-or-later**. Commit identity `Arclamp <45412977+CasualArclamp@users.noreply.github.com>`.

## 9. Milestones

- **M0 — skeleton**: workspace builds; core types + MODCOD/FECFRAME tables; config; IQ-file + HackRF source; CLI prints samples; GUI shows raw IQ constellation + spectrum. CI green. *(local git; create public repo at end of M0/M1.)*
- **M1 — acquisition & PL**: RRC + timing (Gardner) + carrier recovery; SOF/PLS correlation; PLHEADER decode (MODCOD, FECFRAME, pilots); PL descramble; pilot phase tracking. Output: locks on a signal, prints MODCOD per frame, clean constellation. *(Done 2026-10-08.)*
- **M1b — wideband + VFOs + Identify** *(added 2026-10-08, see §3a/§3b)*: DDC (NCO mix + decimating filter) and AGC; engine restructured into a wideband front end feeding N VFO worker threads behind a `Decoder` trait; carrier detection across the span; the **Identify** decoder (symbol rate, roll-off, constellation order, DVB-S2/S2X identification); an S2 PLFRAME generator in `synth` so all of it can be tested on realistic signals; GUI rebuilt around a big spectrum + waterfall with draggable VFOs and a side bar; live HackRF. Needs no FEC, so it lands first and is usable on its own as a carrier survey tool.
- **M2 — FEC core**: demap→LLR + LDPC + BCH for the common MODCODs (QPSK/8PSK, normal+short); BBHEADER + CRC-8; emit valid BBFRAMEs. Verified vs reference vectors. *(Done 2026-10-08 for all 21 S2 codes; verified end to end against our own encoder, with the conventions — interleaver, BCH, BB scrambler, CRC-8 — cross-checked against gr-dvbs2rx and the standard. Live-signal confirmation pending.)*
- **M3 — full MODCOD coverage**: 16/32/64/128/256APSK, all S2 + S2X rates, medium frame, VL-SNR + pi/2-BPSK. *(2026-10-08: the S2X FEC layer and the S2X physical layer for normal and short frames are done — every Table 17a MODCOD decodes end to end; VL-SNR with medium FECFRAMEs too, from about 0 dB Es/N0 — the PL sync, not the codes, sets that floor.)*
- **M4 — GSE → IP → PCAP + TS**: standard + proprietary GSE variants, reassembly, PCAP + live stats; TS-mode extraction. (Headline.)
- **M5 — S2X framing & ACM polish**: superframe (Annex E), multistream/ISI filtering, gold-code index, ACM/VCM across changing MODCODs, SNR estimate per frame.
- **M6 — modulator (TX)**: full encode chain → HackRF TX / IQ file; scripted ACM MODCOD schedules; end-to-end loopback test.
  *Started 2026-10-11 with Rory's choices:* DVB-S2/S2X **DATV from an MPEG-TS** first (a file paced by its PCRs, a UDP stream from ffmpeg/OBS, or the test stream; null packets fill the channel); output to the **HackRF live** and to **IQ files**; used **into a cable/dummy load only**; controlled from a **GUI transmit panel** (`decsat tx` for files). Safety: the HackRF transmits only when the panel is armed and Transmit pressed; TX VGA starts at 0 dB and the amplifier off every time; antenna power never; the radio is put back to off on Stop, closing or quitting; a HackRF receive session is stopped first (half duplex). Claude never keys the radio itself: checks are by IQ file and loopback through DecSAT's own receiver. ACM schedules and GSE/IP payloads come later.
- **T — TDMA** *(2026-10-14, §3c)*: T1 analyser (bursts, layout, slots, frame, terminals) and T2 per-burst demodulation through the layout — done, on the live IS-19 group; T3 payload decoding next.
- **M7 — GUI polish + release**: stream table, ACM timeline, per-MODCOD constellation, throughput; portable exe + first public release.

## 10. Testing

- Unit tests per crate (FEC encode↔decode round trips, GSE reassembly, CRC, mapper/demapper symmetry).
- Golden vectors (committed, self-generated — no third-party source in the repo) for LDPC/BCH/mapping.
- Loopback: modulator → demodulator in-process and over a HackRF TX→RX cable, across MODCODs and roll-offs.
- Live: Rory tests against QO-100 / real carriers himself. Keep heavy sweeps on CI.

## Appendix A — where to look in the reference code

Clones live in `reference/` (git-ignored). `gr-dvbs2rx`'s `lib/` maps almost
one-to-one onto our crates, so each milestone knows where to read first:

| Our crate / milestone | gr-dvbs2rx files |
|---|---|
| `decsat-dsp` timing (M1) | `symbol_sync_cc_impl.cc`, `delay_line.h` |
| `decsat-dsp` carrier (M1) | `pl_freq_sync.cc/.h`, `rotator_cc_impl.cc` |
| `decsat-frame` PL sync (M1) | `pl_frame_sync.cc/.h`, `plsync_cc_impl.cc` |
| `decsat-frame` PLS decode (M1) | `pl_signaling.cc`, `reed_muller.cc` — the 64-bit PLS code is a **Reed–Muller** code |
| `decsat-frame` descramble (M1) | `pl_descrambler.cc`, `pl_defs.h` |
| `decsat-fec` demap/LLR (M2–M3) | `xfecframe_demapper_cb_impl.cc`, `qpsk.h`, `psk.hh`, `qam.hh`, `pi2_bpsk.cc` |
| `decsat-fec` LDPC (M2–M3) | `ldpc_decoder_bb_impl.cc`, `dvb_s2_tables.hh` (55 KB), `dvb_s2x_tables.hh` (131 KB) |
| `decsat-fec` BCH (M2) | `bch.cc`, `gf.cc`, `gf_util.h`, `reed_solomon_error_correction.hh` |
| `decsat-fec` params (M2) | `fec_params.cc` — K/N per MODCOD |
| `decsat-frame` BBHEADER (M2) | `bbdeheader_bb_impl.cc`, `bbdescrambler_bb_impl.cc`, `crc.h` |

`leansdr`'s `dvbs2.h` is the compact alternative worth comparing for
acquisition behaviour at low SNR. `gr-dtv` is the modulator reference for M6.

Those directories are untrusted downloaded code: read them, never build or run
them as part of DecSAT's own test loop.

## Appendix B — GSE wire format

Recorded here so M4 does not have to re-derive it. Cross-checked against the
Kaitai definitions in `dontlookup/parser/parsers/gse/*.ksy` (MIT) and ETSI
TS 102 606.

Every GSE packet starts with a 2-byte header, big-endian bit order:

```
 bit  15 14 | 13 12 | 11 ........ 0
      S  E  |  LT   |  GSE_LENGTH (12 bits)
```

`S`/`E` taken together as a 2-bit value:

| value | name   | S | E | meaning                   |
|-------|--------|---|---|---------------------------|
| 0     | middle | 0 | 0 | a middle fragment         |
| 1     | end    | 0 | 1 | the last fragment         |
| 2     | start  | 1 | 0 | the first fragment        |
| 3     | whole  | 1 | 1 | an unfragmented PDU       |

`LT` (label type): `0` = 6-byte label, `1` = 3-byte label, `2` = broadcast (no
label), `3` = re-use the previous label.

**Padding:** `S/E == middle (0b00)` *and* `LT == 0b00` *and* `GSE_LENGTH == 0`
means the rest of the BBFRAME data field is padding — stop parsing the frame.

Body by type (all after the 2-byte header):

| type   | fields                                                                   |
|--------|--------------------------------------------------------------------------|
| start  | `frag_id` u8, `total_length` u16, `protocol_type` u16, label (6/3/0 B), data |
| middle | `frag_id` u8, data                                                       |
| end    | `frag_id` u8, data, `crc32` u32 (last 4 bytes)                           |
| whole  | `protocol_type` u16, label (6/3/0 B), data                                |

Note that **whole** carries no `frag_id` and no CRC, and **middle**/**end**
carry no protocol type or label — those come from the matching *start*
fragment, keyed by `frag_id`.

### The four variants to try

Two independent deviations, so four parsers, exactly as dontlookup does. Run
them all and accept whichever yields a valid IP header (checksum + sane
lengths):

1. **Length meaning.** *Standard*: `GSE_LENGTH` counts every byte after the
   2-byte header. *`hdrlen`*: `GSE_LENGTH` counts the 2-byte header **as
   well**, so the body is `GSE_LENGTH − 2` bytes (`gse_hdrlen.ksy`:
   `size: gse_length-2`). *(Corrected in M4: this note first said "the PDU
   only", which the Kaitai source does not support.)*
2. **Fragment id.** *Standard*: `frag_id` is a plain u8. *`split`*: that byte is
   a 6-bit `frag_id` plus a 2-bit continuity counter.

A fifth, `hdrlen_unsafe`, is `hdrlen` that also ignores `LT` — more permissive,
accepting packets a label-type check would discard. Worth having as a last
resort.

### Fallbacks when framing is unknown

- **Blind IPv4 search**: scan every byte offset of the BBFRAME data field for a
  header whose IHL, total length and checksum all agree.
- **Byte-pair swap**: some captures arrive 16-bit byte-swapped; re-run the
  parsers over a pair-swapped copy.

## Appendix C — TS mode

TS-mode BBFRAMEs carry 188-byte MPEG-TS packets with the sync byte replaced per
the BBHEADER's `SYNC`/`SYNCD` fields, and optional null-packet deletion (NPD).
dontlookup also implements two CRC variants seen in the wild — "Generic" and
"Newtec" — which `decsat-ts` should offer alongside the standard handling.

