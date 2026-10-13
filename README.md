# DecSAT

A **satellite receiver** in Rust, for a **HackRF One** or recorded IQ, built
like **SDR++**: a big waterfall over the whole span, and VFOs you drop on it,
each running the decoder of your choice — including a blind **"what is
this?"** mode that finds a carrier's symbol rate and tells you what it is.

It decodes DVB-S2 and DVB-S2X with adaptive coding and modulation (**ACM**),
frame by frame: **GSE** to **IP** written as a PCAP, **MPEG-TS** to a file or
a media player, multicast radio played in the app. Beyond DVB it takes the
SCPC modems behind thin-route links apart — Intelsat TPC 2964 (Comtech),
Paradise Q-Flex FastLink, K = 7 Viterbi (IESS-308/309) — down to their E1,
Drop & Insert and TDM channels, and plays the voice in them (G.711, and
G.728 LD-CELP found by itself); it reads the radar feeds on such links —
track reports in IBM bisync or ASTERIX, ADS-B in IP — onto a radar scope
with a map; DVB-CID carrier IDs too. It was called **DecDVB** until
October 2026.

![DecSAT: carriers in an 8 MS/s span, each claimed by an Identify VFO; the selected one is identified as a DVB-S2X ACM carrier at 500 kS/s with its four MODCODs listed](docs/images/waterfall.png)

*A synthetic 8 MS/s test scene (`decsat scene`): a DVB-S2 CCM carrier, an
ACM carrier mixing S2 and S2X MODCODs (QPSK 1/2 → 8PSK 25/36 → 16APSK 26/45 →
32APSK 32/45), a DVB-S carrier and a CW tone (the scene has since gained a
TPC 2964 carrier). Every carrier was found and identified blind.*

> Early development. [`docs/DESIGN.md`](docs/DESIGN.md) holds the scope and the
> milestone plan, [`docs/STATUS.md`](docs/STATUS.md) what is done.

## What works now

| | |
|---|---|
| Waterfall + spectrum over the whole span, zoom and pan | ✅ |
| Carrier detection: every carrier marked with its symbol rate; slow ones (a 9.6 kS/s link in a 20 MS/s span) told from CW tones and spurs by a finer look | ✅ |
| VFOs: draw, drag, resize, click a carrier to claim it; one thread each | ✅ |
| **Identify** — blind symbol rate, roll-off, constellation, DVB-S2/S2X detection | ✅ |
| **Survey**: every clean carrier in the span tried in turn, a few at a time — Identify, then its decoder with a time to lock — with the outcome, the model, what it carries and its payload listed, and saved as CSV | ✅ |
| DVB-S2/S2X PL demodulation: frame lock, MODCOD per frame (ACM) | ✅ |
| Carrier recovery: a locked constellation, Es/N0 (the MER) and Eb/N0, for Identify and DVB-S2 VFOs | ✅ |
| **DVB-S → MPEG-TS** (EN 300 421): code rate, rotation and inversion found blind; Viterbi, RS, the same TS outputs | ✅ |
| **TPC 2964 → IP / TS / voice** (Intelsat IESS-315 turbo product code, BPSK/QPSK/8PSK): frame structure, scrambling and payload found from the signal; HDLC → IP (Cisco HDLC, PPP, Frame Relay), Comtech EDMAC → bridged Ethernet → IP, MPEG-TS, E1, Comtech D&I++ voice (one timeslot or several) and 64 kbit/s timeslots in HDLC packets — a trunk's voice channels, its IP, and the ADS-B and radar in it | ✅ confirmed on live Comtech CDM-600L and CDM-570L carriers |
| **Q-Flex FastLink → data and voice** (Paradise, QPSK 0.710): sync word, the (2880, 2048) LDPC code and the frame scrambler, all measured from a live carrier; inside, Paradise's closed-network + ESC framing and a 257-bit TDM multiplex (sixteen 8 kbit/s channels, metered live so speech shows); a call's G.728 voice put together from its four channels — listen, record `.wav`; the 2–4 × faster links' multiplex, its HDLC timeslot (bridged Ethernet → IP) and its G.728 calls | ✅ decodes live Q-Flex links down to the calls' speech, the faster ones' IP too |
| **Viterbi K=7 → data** (IESS-308/309 SCPC and the like): rate 1/2–7/8, puncturing and orientation found blind and followed when the carrier turns; then the payload search — with or without differential decoding — for HDLC, TS, E1, D&I++, Paradise or IBS/SMS (IESS-309, 16/15) framing, or just the scrambler from the idle fill | ✅ decodes a live 10.24 kBd IESS-308 carrier to its IBS frames (rate 1/2, differential, V.35) |
| **Carrier ID (DVB-CID, ETSI TS 103 129)**: the spread-spectrum identifier under a carrier — the uplink modulator's unique ID (and MAC), position, telephone and text | ✅ to the specification, on synthetic carriers (no CID among the recordings yet) |
| **Comtech CDM modems** (one decoder entry): the type found by itself — TPC 2964, K = 7 Viterbi or FastLink from the first look, the model from the data — or chosen: CDM-600L voice (TPC → D&I++ → G.728), CDM-570L TPC (→ IP / TS), CDM-570L Viterbi, CDM-570L trunk (voice + IP with ADS-B and radar); the G.728 voice in every channel played as soon as speech is found — no ▶, so several carriers can be heard at once, one voice at a time if you like | ✅ on live CDM-600L and CDM-570L carriers |
| **E1 voice**: G.704 E1 or Comtech Drop & Insert++ timeslots, G.711 A-law — levels per channel, listen, record `.wav`; G.728 LD-CELP 16 kbit/s voice in two bits of a timeslot — up to three such channels in one timeslot — found and decoded | ✅ (G.728 confirmed on live CDM-600L and CDM-570L voice) |
| **Radar feeds** on CDM-570/570L links: track reports in IBM bisync blocks, **ASTERIX** categories 034 and 048 in HDLC (the Mode S registers in them read too) and ADS-B (category 021) in IP — a track table, a CSV log, and the **radar scope**: a PPI with trails and leaders, a map, airports and runways, ADS-B aircraft over a radar's picture, callsigns from a decoded ADS-B feed or (switched on) adsb.lol | ✅ on live carriers |
| **Radar station monitoring** (an RCMS multicast in Frame Relay on a CDM-570L trunk): the station's units and the points they report, beside the scope | ✅ on a live trunk (what the points mean is open) |
| **Generic PSK/APSK/QAM → symbols** (`.bin`, one byte per symbol), BPSK…32APSK and 8/16/64QAM, for non-DVB carriers | ✅ |
| Narrow carriers: VFOs down to 500 Hz, ~10 kBd SCPC carriers lock | ✅ |
| IQ recorder and spectrum-only VFOs | ✅ |
| IQ file replay (`cs8`, `cu8`, `cs16`, `cs24`, `cf32`; two-channel `.wav` from SDR#, HDSDR, SDRuno, SDR++ with format, rate and centre from the header), rate/centre from file names | ✅ |
| **Live HackRF One**, 2–20 MS/s, pure Rust over USB (no DLLs), LNB LO, an input-level warning when the gain is too high | ✅ |
| **LDPC + BCH → BBFRAMEs**: all 21 S2 and 31 S2X codes, BBHEADER, stream info, payload rate | ✅ |
| **S2X**: 8-bit PLS code, all 55 normal/short MODCODs — 2+4+2 8APSK to 256APSK, the new interleavers | ✅ |
| **S2X VL-SNR**: the VL-SNR header, pi/2-BPSK (and spread), medium FECFRAMEs, all 9 MODCODs — down to about 0 dB Es/N0 so far | ✅ |
| PL scrambling: the preferred sequences of S2X Table 19e found from the pilots | ✅ |
| S2X superframing (Annex E) | to do |
| **GSE → IP → PCAP** + live IP stats, flows encrypted in transit marked as such; GSE variant detected from the data | ✅ |
| **MPEG-TS**: services, PIDs, errors; `.ts` file, UDP, TCP/HTTP to VLC or PotPlayer | ✅ |
| **TS analyser** (EBSPro-style): PIDs, services, now/next, network, tables | ✅ |
| **Multicast audio** from GSE or MPE: SAP/SDP names, codecs; unannounced RTP AAC and Opus described from their packets; IPv4 fragments reassembled; now-playing messages; play in the app (volume, pause) or VLC/PotPlayer; record to file | ✅ checked on a recording of a live MPE radio multiplex |
| Modulator (HackRF TX / IQ file) | M6 |

## Using it

```bash
cargo run --release -p decsat-gui -- capture_8Msps.cs8
```

Or open a capture from the toolbar, or drop it on the window. The sample rate,
centre frequency and format are read from the file name when it carries them
(gqrx, SDR++ and DecSAT's own recordings do); otherwise set them in the toolbar.

DecSAT names its recordings — IQ, symbols, modem data, PCAP, TS, voice — by
the RF frequency they hold, e.g. `decsat-ASA245_LHI-BN-12346.0340MHz-…`: the
radio's tuning plus the LNB LO and LO trim. Reopened, an IQ recording takes
its centre from that. When the RF is not known (no LO set, a file with no
centre) the name holds the VFO's offset from the band's centre instead
(`+5236688Hz`), as recordings from before 0.8.1 do.

On the waterfall:

| | |
|---|---|
| **wheel** | zoom about the cursor |
| **right- or middle-drag** | pan; past the span's edge (or at full span) with the HackRF live, it tunes the radio — the spectrum follows the mouse |
| **drag on empty space** | draw a new VFO |
| **double-click** | drop a VFO |
| **click a green bracket** | claim a detected carrier with a VFO sized to fit, on the decoder it is known to need, else found as the Survey finds it (Identify, then DVB-S2 or the Comtech *type found*) |
| **shift-click a green bracket** | claim it with the decoder *New VFOs run* instead |
| **drag a VFO** / **its edge** | move it / resize it |
| **click** (VFO selected) | tune it there |
| **✕** on a VFO's label, or **Delete** | remove it |

The big readout at the top is the centre frequency, as in SDR++: wheel over a
digit or click its upper/lower half to step it, right-click to zero that digit
and every one below it. With the HackRF running it retunes the radio.

**📁 Output folder…** in the toolbar sets where IQ recordings, symbol files,
PCAP and TS files go (default `Documents\DecSAT`); it is remembered between
runs and applies to existing VFOs too.

The side bar lists the VFOs with their CPU load (✕ removes one), and shows the
selected one's settings, its Identify result or demodulator state (lock, Es/N0 and Eb/N0,
residual offset), a carrier-locked constellation and a zoomed spectrum. The
status bar counts the carriers and narrow lines, and shows the front end's
load and the input level — orange when the HackRF's gain is too high (see
*HackRF One* below).

### DVB-S2 → GSE/IP → PCAP

A **DVB-S2/S2X → GSE/IP** VFO demodulates, decodes LDPC and BCH, and reads
the BBFRAMEs. Generic-stream frames go through GSE (ETSI TS 102 606) to IP,
fragments reassembled and their CRC-32s checked. The side bar shows the
stream, IP packet and byte counts, the IP rate, protocols and the busiest
flows; press **● Record** next to *PCAP* to write the packets to a `.pcap`
(raw IP, opens in Wireshark).

**▶ Live** next to *Wireshark* opens Wireshark and feeds it the packets as
they are decoded: it is started capturing from its standard input
(`-k -i -`), which DecSAT writes as a PCAP stream, with its RTP heuristic on
so ED-137 streams show their PTT and squelch without the SIP that set them
up. **⏹ Stop**, or closing Wireshark, ends it; Wireshark keeps what it has.
Every decoder that carries IP has both buttons (a TS decoder once IP turns
up in it, by MPE); `decsat decode … --wireshark` does the same, and so does
the GUI started on a carrier:

```bash
decsat-gui --hackrf 1748 --rate 4 --lo 10600 --amp --vfo-at -422000:300000 --decoder tpc --wireshark
```

(VFOs placed with `--vfo-at` make a one-off run: the session you had is
neither restored nor overwritten.)

Real links do not all follow the standard. As in
[dontlookup](https://github.com/ucsdsysnet/dontlookup), DecSAT reads every
data field four ways at once — GSE_LENGTH counting the 2-byte header or not,
the fragment id as 8 bits or as 6 bits plus a 2-bit counter — and keeps
whichever yields valid IP (checksums, lengths). The *GSE* setting can force
one. If none yields IP, a blind IPv4 search over the data fields takes over.

### DVB-S2 → MPEG-TS → VLC / PotPlayer

A **DVB-S2/S2X → MPEG-TS** VFO rebuilds the transport stream from TS-mode
BBFRAMEs (sync bytes restored from the CRC-8 chain, deleted null packets put
back) and shows its services (from the PAT, PMT and SDT), PIDs, CRC and
continuity errors. Send it on with any of:

| | |
|---|---|
| **TS file → ● Record** | a `.ts` in the output folder |
| **UDP** `127.0.0.1:1234` | VLC: `udp://@:1234` · PotPlayer: `udp://127.0.0.1:1234` |
| **TCP / HTTP** `127.0.0.1:8001` | VLC or PotPlayer: `http://127.0.0.1:8001/` (VLC also `tcp://…`) |
| **▶ VLC / ▶ PotPlayer** | starts the TCP server and opens the stream in the player |

**🔍 TS analyser** opens a window in the spirit of EBSPro's: every PID with
what it is (PAT, PMT, H.264 video, AC-3 audio, ECM/EMM with the CA system,
unlisted PES…), the service it belongs to, its bitrate and share, continuity
and error counts, scrambling and PCR; the service tree with what is on now and
next (EIT); the network's transponders (NIT); and every table seen.

Both network outputs start on 127.0.0.1, this machine only; use 0.0.0.0 (TCP)
or another host's address (UDP) to reach the network. Several players can
connect to the TCP server at once; one that stalls loses data rather than
holding up the receiver.

### Multicast audio

Satellite links carry radio as IP multicast — in GSE, or in MPE inside a
transport stream (both are read). Any DVB-S2 IP or TS VFO lists the audio
streams it finds under **Multicast audio**: named from SAP/SDP announcements
where there are any, with codec (AAC in ADTS, LATM/LOAS or RFC 3640, MPEG
audio, PCM, Opus), bitrate and RTP payload type, listed by address.

Streams with no announcement are described from their own packets: RFC 3640
AAC by its RTP clock rate (from RTCP sender reports, else timed against the
signal), the clock ticks an access unit spans (1024: AAC-LC; 2048: HE-AAC)
and its first element (mono or stereo); Opus by packets that parse as Opus.
IPv4 fragments are put back together first — a multiplex packing several
AUs into a 2.3 kB RTP packet sends each as two. `<nowplaying>` messages
(title, artist, station) are listed under **Now playing**.

- **▶ Play** decodes it in DecSAT: MPEG audio layers I–III, AAC-LC and
  HE-AAC v1/v2 with SBR and PS (libxaac; from ADTS, LATM/LOAS or RFC 3640),
  Opus (libopus) and PCM/G.711, with **⏸ Pause** (resumes live), **⏹ Stop**,
  a level meter, and the volume slider and 🔊 mute at the top of the list
  (app-wide, remembered).
- **⏺ Record** saves the stream to the output folder as broadcast, with no
  re-encoding: `.mp2`/`.mp3`, `.aac` (ADTS, any AAC carriage, HE-AAC intact),
  `.wav` for PCM, `.ts` for TS in UDP, `.opus` (Ogg) for Opus.
- **… → Open in VLC / PotPlayer**: an RTP stream with a description is
  relayed to a local port and the player is given an SDP file (so it decodes
  HE-AAC, LATM and the rest itself); a bare elementary stream is served at a
  local HTTP address.

After the
[VK2SWL DVB-S/S2 Multicast Audio Receiver](https://github.com/VK2SWL/DVB-S-S2-Multicast-Audio-Receiver).

### DVB-S → MPEG-TS

The older standard, still the norm for amateur DATV (QO-100) and some feeds.
The VFO finds the symbol rate as usual, locks the QPSK, then works out the
rest itself: the code rate (1/2 … 7/8), the puncturing phase, and the
carrier's 90° rotation and spectral inversion, by decoding a block under
each hypothesis and keeping the one whose re-encoding matches what was
received. Then Viterbi (K = 7, soft), the sync bytes, the convolutional
deinterleaver, Reed–Solomon (204,188) and energy dispersal — and the stream
goes to the same TS outputs and analyser as DVB-S2's.

### TPC 2964 → IP / MPEG-TS

The rate-3/4 turbo product code of Intelsat IESS-315 VSAT carriers, as the
CTCOM RCV-20x manual describes it: frames of 2964 bits, a 20-bit unique word
F50B8h and a (64,57) × (46,39) extended-Hamming product codeword holding 2223
data bits, with a (2, 3, 9, 12) / 475h scrambler — on BPSK, QPSK or 8PSK
(Gray-labelled as the manual's Figure 3.1; on 8PSK a frame is 988 symbols
and its UW need not start on a symbol's first bit). IESS-315 itself sets
only performance and leaves the code to "compatible turbo modems", so the
VFO finds the rest from the signal:

1. **Unique word** every 2964 bits, under each carrier phase ambiguity; a
   carrier slip or a lost symbol is repaired at the next frame.
2. **Frame structure**: row or column order, either way round, the Hamming
   generator (all six degree-6 primitive polynomials), whether and how the
   code bits are scrambled — 432 combinations, judged by how many rows *and*
   columns are codewords as received (near all under the right one, 1 in
   128 under any other); close calls are settled by decoding.
3. **Decoding**: iterative Chase–Pyndiah soft decoding.
4. **Payload**: HDLC (FCS-16/32) or MPEG-TS, under each candidate
   descrambler — the (2, 3, 9, 12) polynomial self-synchronising or additive,
   ITU-T V.35's and V.29's — whichever yields frames that check.

IP found in the HDLC frames (Cisco HDLC, PPP, Frame Relay as RFC 2427
carries it, or anything with the packet after a short header) gets the same
statistics, PCAP and multicast audio as GSE — a flow whose UDP checksums all
fail while its headers check is marked **encrypted** in transit, and the
audio sniffer leaves it alone; a transport stream gets the TS outputs.
**● Record** writes the decoded data to a `.bin` (the IP has its own
*PCAP* and *Wireshark* buttons), and `decsat pcap` reads a recorded PCAP
back. Identify recognises the carrier by its unique
word. What each search found is shown, so you can tell a finding from a
default. It is confirmed on live Comtech CDM-600L and CDM-570L carriers.

**Comtech EDMAC with bridged Ethernet** (worked out from a live 8PSK
carrier; no specification was to hand): the data rate grown by 21/20 in
1008-bit frames — a fixed header, five 192-bit data blocks and 9-bit
overhead groups between them carrying the modems' M&C channel. Inside, the
traffic is bridged Ethernet in 258-byte packet blocks: a 4-byte word with a
pointer to where a packet ends, then packets, each with a length byte (or 0
for "to the pointer") and a 6-byte header. A packet with whole headers sets
up a header-compression context; the compressed packets that follow
(SNMP, the RTP of a radio-over-IP link, the middle fragments of its SIP)
are rebuilt from it — UDP checksums checked, fragments put back together by
the IP stage. The Ethernet frames go to the IP stage — statistics, PCAP —
like any other; the side panel's **Framing** and **Packets** rows say how it
is going.

**ED-137 radio over IP** (air traffic control radio as VoIP, wherever it
turns up in the IP): each RTP packet's header extension carries the radio's
signalling — PTT and its type (normal, coupling, priority, emergency,
test), squelch, the PTT's id, PTT mute and summation, simultaneous calls,
and features such as signal quality and CLIMAX delay — in R2S keep-alives
and voice packets alike. The IP card's **Radio over IP** section shows each
stream's state live (PTT red, squelch green), how often and how long each
was on, packets lost, and a log of the changes; the SIP dialogs that set the
sessions up are read too — who calls whom, the SDP (radio type, Tx/Rx mode,
keep-alive period, best-signal method) and whether the call was refused or
accepted (one direction tells it by the ACK's transaction). The field
layout follows Wireshark's ED-137 dissector, and agrees with tshark packet
for packet on a live recording. `decsat pcap … --radio-csv` writes every
packet's signalling. **The voice is heard**: the sound (G.711, A- or
µ-law) of packets whose PTT or squelch is on plays in the **Radio voice**
card as a call's does — by itself under the voice preset, one stream and
one talker at a time across the app, ▶ to listen regardless, ● to record a
`.wav` — with silence between transmissions.

### Voice over E1 (G.704, Comtech D&I++)

Modems carrying telephony or radio links (airband voice, for one) send
64 kbit/s G.711 channels: a whole E1, or — Comtech's **Drop & Insert++** —
the chosen timeslots in blocks of 2944 bits (64 overhead, 2880 data; the
layout is not published and was worked out from CDM-600L carriers: a 21-bit
sync and three bits that say whether the block starts a frame, then five
blocks of 72 bytes each after 10 bits of overhead — a block a timeslot
every 45 ms, the timeslots' bytes taking turns). Behind the TPC 2964
decoder the payload search finds either,
under the V.35 descrambler or any other it tries, and the **Voice** card
lists the channels with their levels: **▶** listens (the app's player and
volume), **●** records a `.wav`. A timeslot whose sign bit never changes is
not G.711 audio — sub-rate channels or compressed voice — and is marked
**not G.711** with the bits that do change; it plays as silence (as A-law
it would be loud digital noise), and so does any timeslot not yet judged.

When those bits carry **G.728** (LD-CELP, 16 kbit/s: a 10-bit codeword every
five samples), the card says **G.728 speech** and ▶/● play and record it
decoded; while someone speaks, the VFO lights up green with 🔊 in the
waterfall and the VFO list. The voice equipment behind a CDM-600L seen on Horizons 3e sends it
in bits 2–3 of its timeslot, framed G.728's own way (§3.11): the shape
index's top bit carries a sync bit, once every 20 ms different, which the
decoder locks to; its silence fill is muted, and recognised: a channel
sending it reads **G.728, silent** before anyone has spoken. The decoder is
written from the Recommendation and matches its Appendix I test vectors
(112 dB SNR or better).

**Multi-radio sites** send their timeslots differently: as HDLC packets on an
inverted line (idle flags read `00000011` repeated), each packet a sequence
count, a channel byte (`08h`–`F8h`), a type byte `13h` and 44 octets — 5.5 ms
— of one 64 kbit/s timeslot. Each channel's packets join into a continuous
timeslot, and the **Voice** card lists them by that byte (ch 10h, 18h, …)
and finds, plays and records the G.728 in each as above — a trunk's
timeslot can hold up to three G.728 channels in different bit pairs, each
decoded, and *one voice at a time* holds within the timeslot too. Packets
of another type on such a trunk carry the site's IP, which goes to the IP
stage (with the ADS-B in it: see below). A site with nothing to say sends
only a status packet every two seconds.

The **Comtech CDM modem** entry's voice models — CDM-600L voice, CDM-570L
trunk, or *type found*, which picks the chain from the first look and the
model from the data — play every channel with G.728 by itself.
**One voice at a time** (the toolbar, on by default) works like a scanner's
hang time across all carriers and channels: whoever speaks first is heard,
and the others wait until they have been quiet for the **hold** (3 s, set
beside it); off, everyone is mixed. A channel played with ▶ is always heard.
The toolbar's **Volume** and 🔊 mute are the master for everything played,
and the list beside them picks the sound output (the system's default, or
any device by name — a headset, a virtual cable into other software);
players already running move to it at once, and the choice is remembered.
Offline:

```bash
decsat decode capture_148148Sps.cf32 --decoder tpc --out dir --e1-record 1
```

### Radar feeds and the radar scope

The same thin-route links carry air-traffic surveillance. Any VFO whose
chain yields the frames reads it (the **Comtech CDM modem** entry with *type
found* picks the chain itself; CDM-570L Viterbi and CDM-570L trunk are the
usual ones), in three forms:

- **Track reports in IBM bisync blocks** (SYN, an SOH heading, DLE STX … DLE
  ETX, a BCC) on a 9.6 kbit/s link: one message a sector, 32 a scan, each
  report a track's position, velocity, Mode A code, Mode C level and
  ground speed. The format is nobody's standard; it was worked out from
  the signal.
- **ASTERIX** in HDLC UI frames on 64 kbit/s links: category 034 service
  messages (sector crossings, the north marker with the radar's place and
  scan time) and category 048 target reports (position, Mode 3/A, flight
  level, Mode S address, track number, velocity, status), with the Mode S
  registers the radar reads from each aircraft (BDS 4,0, 5,0 and 6,0:
  selected altitude, altimeter setting, true track, heading, IAS, Mach,
  vertical rates). A block counts only if its records parse to its exact
  length under the standard UAPs.
- **ADS-B** as ASTERIX category 021 (edition 0.23) in a trunk's IP: each
  aircraft's own position, altitude, callsign, squawk and emitter class.

The VFO's **Radar feed** card lists the tracks; **📡 Radar scope** opens a
window with a PPI — the beam turning at the measured scan rate, range
rings, trails, one-minute leaders, Mode A / level / speed labels, hover
details, a track table, wheel zoom and drag pan. Click a track, or its row,
for its details: Mode S/A/C, climb or descent, speed and heading, range
and bearing from the radar and from the nearest airport, latitude and
longitude when the radar's place is known, the status word said field by
field, the raw octets. Coasted tracks are yellow; fast or high ones purple
(650 kt or more, FL450 or above, or over 350 kt below FL100); and with an
ADS-B feed in step with the radar's clock, the ADS-B aircraft are drawn over
the radar's picture as magenta diamonds at the places they report, and the
radar tracks no ADS-B aircraft matches turn red.

Under the PPI, **Map** draws OpenStreetMap tiles (fetched with the system's
`curl`, cached for good under `%APPDATA%\DecSAT`; **Light** or dark) and
**Airports** the airports and runways of OurAirports' open data (fetched
once, refreshed monthly). The radar's place comes from the feed when it
gives it (ASTERIX's north marker), else **Site…** sets it; either is kept in
your own settings, keyed by the radar, never in the project. Tracks the
feed leaves unnamed take callsigns from an ADS-B feed decoded on another
VFO (nothing asked of anyone) or, only with the **adsb.lol** switch on (off
by default), from that site's live aircraft near the radar. The waterfall
tags a carrier **ADS-B** or **RADAR** once a decoder has found either in it,
and remembers the carrier. While a VFO records, its track reports go to a
`.csv` beside the PCAP.

A CDM-570L trunk that carries a radar station's **monitoring and control**
multicast (Frame Relay; a word-oriented format of typed points, the units
named in the data) gets a **Radar station** card above the IP card, and
its scope shows the station's units and the readings that move beside the
PPI, with the targets of the radar feed another VFO decodes. What the
points mean is not known yet.

Offline, and unattended:

```bash
decsat decode radar_64000Sps.cf32 --decoder auto --fast      # a radar link: its reports
decsat payload trunk.bin --frame-bits 2223 --bit-rate 1024000 --radar-out tracks.csv --voice-out voice.wav   # a trunk's data recording
decsat pcap recording.pcap --csv points.csv                   # a recorded PCAP: the IP, a station's points
decsat-gui capture.cs8 --vfo-at 1461800:140000 --decoder trunk --radar-scope --radar-range 120 --after 20 --screenshot scope.png
```

### Carrier ID (DVB-CID)

Modern uplink modulators hide a carrier identification signal under their
carrier (ETSI TS 103 129): BPSK spread 4096 chips a bit, 27.5 dB below the
carrier's spectral density, 220 Hz above its centre, saying who transmits —
the modulator's 64-bit identifier (often its MAC address) and, if the
operator entered them, its position, a telephone number and a short text.
Put a **Carrier ID (DVB-CID)** VFO on a carrier (at least 1.35 × the chip
rate wide: 151 kHz below 512 kBd, 302 kHz above): it measures the carrier,
searches for the spreading code (a fraction of a second), then reads frames —
one every 36 s (18 s on carriers of 512 kBd and up). Its card shows the
tracking live — the despread bits and their differential products as
constellations (with Es/N0), the early/prompt/late correlators on the code's
correlation peak, SNR and frequency over the last few seconds, the soft bits
and the frame sync filling copy by copy — and, under **Code search**, the
acquisition itself: correlation over the 4096 code phases, and code phase ×
frequency around the peak. Offline:

```bash
decsat decode capture_1000000Sps.cf32 --decoder cid --fast
```

No CID to hand? `decsat cid` writes one under a DVB-S2 carrier, with your
choice of identifier, position, telephone and text (see
[`tests/`](tests/README.md)).

### Generic PSK → symbols

For carriers that are not DVB-S2 — SCPC data, telemetry, DVB-S — a VFO locks
the carrier and shows its constellation; press **● Record** and it writes the
hard-decided symbols to
`decsat-<VFO>-<RF>MHz-<rate>Bd-<modulation>-<time>.bin`, one byte per symbol:
the symbol's bit label under the DVB-S2 mapping (BPSK: 0 = +1). The symbol rate
and constellation come from Identify, or set them by hand. Without a preamble
the carrier phase is ambiguous by the constellation's symmetry (90° for QPSK),
so the labels may be a fixed rotation of the sent ones — a sync word found
offline resolves it.

**Text (live)**: the decided bits are also searched for readable strings,
under every phase rotation and mirror image, both bit orders, all eight byte
alignments, plain and differentially decoded — 256 readings at once for
QPSK. The reading with far more text than the rest is shown, scrolling as
the strings arrive (telemetry, beacons, NMEA, idle messages), with the
longest strings from any reading as candidates. Untick *look for text* to
save the CPU (about a quarter of a core at 1 MBd).

### Text in every decoder

Every decoder shows the text in what it decodes:

- **DVB-S2/S2X and DVB-S → MPEG-TS**: the transport stream's payloads, read
  per PID so a table's strings come out whole — service and provider names,
  EPG text, URLs, anything carried in the clear — and, when the stream
  carries IP (MPE), the IP payloads too.
- **DVB-S2/S2X → GSE/IP** and **TPC 2964 over HDLC**: the UDP/TCP payloads —
  SAP/SDP announcements, HTTP headers, plain-text protocols.
- **TPC 2964** whose payload is not recognised yet, **Identify** while it
  demodulates live, and the **generic PSK** decoder: every reading of the
  bits, as above.

Decoded data may be compressed, encrypted or video, which throws up short
printable runs by chance, so the byte decoders rank strings by how often
they recur (chance runs do not recur) and list long strings as they come.

### What Identify reports

It splits what it *knows* from what it *guesses*. **DVB-S2** is reported as a
fact — PLHEADERs found repeating exactly where their own PLS codes predict the
next frame — with the MODCODs in use, pilots, frame lengths, and CCM vs ACM
(a MODCOD read once among many frames is a misread header, said so, not
taken for ACM or S2X).
Anything else gets measurements (symbol rate, roll-off, an estimated
constellation) and a labelled guess: plain QPSK reads *"possibly DVB-S (not
verified)"* (the DVB-S decoder confirms it), and a BPSK, QPSK or 8PSK carrier
with the TPC 2964 unique word every 2964 bits is named as such. For the rest it
looks for fingerprints of proprietary waveforms:

- **DVB-S2-style headers off their own grid**: SOF and PLS headers that recur
  regularly but not where their PLS codes say the next frame starts — S2
  framing whose codes mean something else, as NovelSat NS3/NS4 appear to use
  (their manuals describe S2-style headers, S2 frame sizes and Gold-code
  scrambling, with extra code rates) or a vendor's short frames.
- **A 2 % roll-off**, sharper than DVB-S2X's 5 % — NovelSat NS4's.
- **Frame structure**: any symbols that repeat frame after frame (a header,
  unique word or pilots) show up in the autocorrelation of the locked
  symbols; Identify reports the period and, given enough frames, where the
  repeating symbols sit (*"repeats every 3330 symbols; 162 known symbols: 90
  from 0, then 2 blocks of 36 every 1476"*) — a fingerprint of a framing even
  when no specification is public.

Comtech VersaFEC and the TPC/LDPC modes of SCPC modems publish no sync
word or frame layout, so they can only be recognised by such measurements;
captures of them would let DecSAT name them. Paradise FastLink was worked
out that way from a capture of a Q-Flex (QPSK 0.710): Identify names it, and
the **Q-Flex FastLink** decoder decodes it — down to the calls in its TDM
multiplex: G.728 voice spread over four channels, two bits a TDM word.
Where each channel's bits go is learnt from the voice equipment's silence
fill (a known pattern) while the channels idle; the **Call voice** card
then says when someone speaks, and ▶/● play and record it. With **Voice:
play when G.728 speech is found** (on by default) a call plays by itself
once anyone has spoken, taking turns with the Comtech channels under **one
voice at a time**; ⏹ turns that off for the VFO.

The faster Q-Flex links (FastLink at 2, 3 or 4 × the 95 kBd links' rate)
carry 2–4 × 128.5 kbit/s in the same Paradise framing, and the same 257-bit
multiplex at that many times the frame rate, laid out differently (at 3 ×
in superframes of three frames, placed by the alignment column): DecSAT
frames it, meters its activity, and finds in it a 64 kbit/s HDLC timeslot —
bridged Ethernet on an inverted line, the site's IP network — whose
packets go to the IP view and the PCAP like any other IP; then, in another
64 kbit/s timeslot beside it, the radios' calls: G.728 on two bits of each
octet, found by their silence fill. The call speaking is heard, through
the same ▶, ● and *play when G.728 speech is found* as the slower links'
(`decsat payload … --voice-out call.wav` from a data recording).

A carrier too slow to show three frames in the first look is marked
*provisional* while it listens longer. Between identifications it keeps
demodulating with what it found, so the constellation and Es/N0 stay live.

### Survey

**Survey…** in the toolbar tries every clean carrier the receiver sees, in
frequency order, a few at a time (*At once*, 1–4): each gets a VFO of its
own for Identify, then the decoder for what it is — DVB-S2 to GSE/IP, the
modem carriers through the Comtech entry's *type found* (TPC 2964, the
K = 7 code, a FastLink); bursts, 8PSK and the rest have no decoder yet — and
a time to lock (*give up after*, 40 s by default). One that locks is
watched a few seconds more so what it carries shows, then its VFO goes and
the next carrier takes its turn; nothing plays or records meanwhile. The
window lists each carrier's frequency, S/N and rate, the outcome (decoded,
no lock, no decoder, no signal), what it decoded as, what it carries
(ADS-B, radar, voice) and its payload or the decoder's last word; **VFO**
puts a kept VFO on a decoded one, **Save CSV** writes the table to the
output folder. The waterfall's ADS-B and RADAR tags learn as it goes, and a
retune stops it. `--survey` runs one unattended (with `--screenshot` to
keep the picture). **Reset labels** (in the toolbar beside *Survey…*, and
in its window) removes every carrier label on the waterfall — the modem,
ADS-B and RADAR tags, kept between runs — and the survey's results, in one
click: after false decodes, or once the satellite's carriers have moved. A
carrier with a VFO still decoding on it is labelled again.

A click on one carrier's green bracket does the same for that carrier
alone, and keeps the VFO: it starts on Identify and switches to the
decoder for what Identify saw, on the centre and rate it measured (a
DVB-S2 carrier whose frames turn out to be TS-mode moves on to the
MPEG-TS decoder; one with no decoder here stays on Identify). The status
bar says what it picked. A carrier already decoded once (here or by a
survey) gets that decoder straight away, and a tool chosen in *New VFOs
run* (IQ recorder, spectrum, symbols, carrier ID) is kept. Shift-click gives the carrier the decoder *New
VFOs run* instead; `--claim-auto` claims every carrier this way
unattended.

### Frequency plan

**📋 Plan…** loads a frequency plan — an SDR++ frequency-manager bookmark
list (`.json`: name, RF frequency, bandwidth per carrier). Its carriers are
marked along the foot of the spectrum and named where there is room (zoom in
for more); hovering names the one under the pointer, clicking a name makes a
VFO on that carrier with its name and width, and a VFO drawn or claimed on a
plan carrier takes its name. A carrier given `"color": "#rrggbb"` in the
file is marked in that colour (to tell modem types apart, say); SDR++ ignores
the field. The plan stays your own file: only its path is remembered
(right-click the button to stop using it).

An LNB's oscillator is rarely exactly on 10600 MHz and drifts as it warms,
so the carriers sit a few kHz off their plan frequencies. **LO trim** in the
toolbar corrects the RF scale for that on the fly (drag or type kHz;
right-click for zero) without retuning anything; **⟲ Align** sets it from
the carriers in view — the trim at which most of them land on plan carriers
of like width, searching ±3 MHz — and **keep** repeats that every 5 s in
small steps as the LNB drifts. The frequency bar shows the trim; it is saved
with the session.

### Transmitting (DVB-S2/S2X DATV)

**📡 Transmit…** in the toolbar opens the transmit panel: an MPEG-TS on a
DVB-S2 or S2X carrier, to the HackRF or to an IQ file.

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

### Command line

```bash
decsat scan capture_8Msps.cs8     # find every carrier and identify each
decsat scan capture_8Msps.cs8 --decode   # …then run each one's decoder over the capture and report
decsat record --hackrf 1720:1780:14 --rate 20 --seconds 2 --out caps   # raw IQ from the HackRF: one slice or a sweep (receive only)
decsat scene                      # write the 8 MS/s test scene above
decsat cid                        # a DVB-S2 carrier with a DVB-CID under it
decsat test-signals tests         # both, into tests/ (see tests/README.md)
decsat mcast recording.ts --decode --record out   # the radio in a .ts (MPE)
decsat payload fastlink.bin --tdm-out frames.bin  # a modem .bin through the payload stage
decsat decode --hackrf 1635.0 --decoder fastlink --offset 640e3 --bandwidth 200e3   --out dir --record-on-activity     # watch a FastLink live, record when a channel speaks
decsat decode qflex_333333Sps.cf32 --decoder fastlink --out dir --tdm-record   # a Q-Flex's call voice to a .wav
decsat decode capture_8Msps.cs8 --decoder ip --offset -1.2e6 --bbframes s2.bbf   # a DVB-S2 carrier, its good BBFRAMEs kept
decsat decode capture_8Msps.cs8 --decoder auto --offset 3.3e6 --bandwidth 60e3   # a Comtech CDM carrier: the type found, the model from its data
decsat payload trunk.bin --frame-bits 2223 --bit-rate 1024000 --voice-out voice.wav --radar-out tracks.csv --hdlc-out frames.txt
decsat decode --hackrf 1748 --rate 4000000 --decoder tpc --offset -915e3 --bandwidth 640e3 --wireshark   # a modem's IP live in Wireshark
decsat pcap recording.pcap --csv points.csv   # a recorded PCAP read back
decsat pcap recording.pcap --radio-csv radio.csv   # ED-137 radio signalling, packet by packet
decsat payload link.bin --frame-bits 2223 --bit-rate 537665 --pcap-out ip.pcap   # a data recording's IP, decoded again
decsat modcods                    # the DVB-S2 MODCOD table
```

## Build

Rust 1.95 or newer, a C compiler and CMake: libopus (Opus radio) and
libxaac (AAC with SBR and PS) are built from the `third_party/` submodules.

```bash
git clone --recurse-submodules https://github.com/CasualArclamp/DecSAT
cargo build --release
```

In an existing clone, `git submodule update --init` fetches them.

### HackRF One

Click **📡 HackRF** in the toolbar, set the frequency, sample rate and gains,
and **Start**. Frequency and gains apply live. **LNB LO** only labels the axis
(RF = tuned + LO): 10600 MHz by default (a universal Ku LNB's high band),
9750 MHz for its low band or QO-100, 0 without one.

**Gain:** the HackRF's LNA and mixer compress long before its ADC clips,
and the products land on the carriers, which then slip and lose frames —
at LNA 40 dB a clean 14 dB carrier in a full Ku-band slice lost most of its
frames, at LNA 24 dB none. The status bar's **input** level turns orange
above −25 dBFS: lower the LNA gain. A carrier that still misbehaves at one
tuning (slips, a raised noise floor beside it) is usually clean a few MHz
either side — the HackRF's synthesisers put noise at particular offsets —
so retune a little.

The driver is pure Rust over USB ([`seify-hackrfone`](https://crates.io/crates/seify-hackrfone)
on `nusb`): no libhackrf, no libusb, nothing to install beyond the WinUSB driver
the HackRF already uses on Windows (Zadig, or the official tools). DecSAT only
ever **receives**, and keeps the antenna-port power **off** — feed an LNB from
an external inserter.

**DC removal** (toolbar, on by default) subtracts the IQ mean before the
waterfall and the VFOs, which removes the spike the HackRF leaves at the
centre frequency. Its notch is a few hertz wide.

## Scope

Everything one HackRF One can carry: symbol rates to roughly 15 MS/s, normal /
short / medium FECFRAMEs, all DVB-S2 and S2X MODCODs up to 256APSK, VL-SNR with
pi/2-BPSK, roll-offs 0.35 down to 0.05, superframing (Annex E), multistream
(ISI) and the gold-code index. Channel bonding (Annex D) and wideband
time-slicing (Annex M) are out of scope — they need more spectrum than one
HackRF has. Wide broadcast transponders (27–45 MS/s) exceed the HackRF's USB
throughput, so those are handled from recorded IQ only.

## Credits

Written from ETSI **EN 302 307-1** (DVB-S2) and **EN 302 307-2** (DVB-S2X), with
algorithms studied from and credited to:

- [gr-dvbs2rx](https://github.com/igorauad/gr-dvbs2rx) (GPL-3) — PL framing,
  frame sync, LDPC/BCH
- [leansdr / leandvb](https://github.com/pabr/leansdr) (GPL-3) — constellations
- [gr-dtv](https://github.com/gnuradio/gnuradio) (GPL-3) — modulator reference
- [dontlookup](https://github.com/ucsdsysnet/dontlookup) (MIT) — GSE/IP
  de-encapsulation, including the proprietary header-length and split
  fragment-ID variants found on real carriers
- DecDRM — the waterfall's GPU ring texture

The radar scope draws OpenStreetMap's tiles (© OpenStreetMap contributors,
ODbL; credited on the map), OurAirports' airport and runway data (public
domain) and, when you switch it on, aircraft from adsb.lol (ODbL). ASTERIX
is read from EUROCONTROL's category specifications, the Mode S registers
from ICAO Doc 9871.

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).
