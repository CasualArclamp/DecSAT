<!-- made by CasualArclamp -->
# DecSAT manual: DVB-S2, DVB-S2X, DVB-S, DVB-RCS and DVB-CID

This page describes the DVB decoders and their outputs. [Back to the README](../../README.md).

## DVB-S2 → GSE/IP → PCAP

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

A multistream carrier's streams are read apart, each its own GSE (or TS)
state, and S2X's GSE-HEM — GSE packets sliced across BBFRAMEs like TS
packets, SYNCD pointing at the first that starts — is put back together.
The roll-off is read as S2X signals it: codes 00/01/10 sent alternately
with 11 mean 0.15/0.10/0.05 (a 0.05 carrier is not shown as 0.20).

**Terminals.** On a VSAT hub's outbound every GSE packet carries its
terminal's MAC as a 6-byte label, and on an ACM outbound the hub sends each
terminal's data in the MODCOD its reported reception allows. The IP card's
*Terminals* table lists the labels by bytes, with the MODCODs each was sent
in, the Es/N0 the main one needs and (hover) its IP addresses — a map of
the network's terminals and their links as the hub sees them.

## Interactive networks (DVB-RCS) and return pools

A hub's outbound tells its terminals how to transmit. With DVB-RCS
(EN 301 790; EN 301 545-2 for RCS2) that is a programme of signalling
tables in the transport stream — on a multistream outbound often its own
input stream, transport packets in a generic stream. The TS card then gets
an **Interactive network** card: the network's name, the forward and return
links of its RCS map (satellite, beam, frequency, symbol rate, standard),
the network clock reference (the 27 MHz count every terminal times its
bursts by) and how often it comes, the terminal information's status
flags, the superframe, frame and timeslot plans, the burst time plan and
corrections when they are sent — and under *Signalling tables* every table
id with its PID, count, period and size, a vendor's own included. Tables a
PMT declares but never sends are listed too: a hub that runs its return
with tables of its own still shows its timing in their rates.

The **Return pool** decoder (TDMA mode, beside the burst analyser) over
the return shows the carriers now (a 200 ms average split at the guards
between them), how often the layout changes (a hub resizing and moving
its terminals' carriers, Mx-DMA style), and the grid —
the slot, from the repeat of the bursts' guard times, and the frame, the
multiple of it the envelope repeats over. Under the pool's own fast
waterfall (an FFT about a millisecond, the latest three frames), its
**burst lanes** — the TDMA decoder's burst map stood on end: a column a
lane under its frequencies, a box a burst down it, newest at the bottom.
The lanes are the bins that burst on and off together; bursts back to
back are cut where the FFT straddling their guard dips. Once an
outbound's burst plan is timed on the pool, the columns are the bands
its bursts use and each box is a planned burst in its terminal's colour,
as bright as its C/N — an outline where the plan gives a burst and
nothing comes. When another VFO is decoding an
outbound whose NCR or a signalling table keeps the same period, the card
says so: the pool runs on that outbound's clock.

On Eutelsat 172B a Newtec hub's 1.048 MS/s S2X ACM outbound (roll-off
0.05) carries its signalling on ISI 0 at QPSK 1/3 — an RCS map, the NCR,
and two private tables, one every 40.2 ms — and IP to its terminals on
ISI 1; the 770 kHz pool beside it holds a ~190 kHz continuous carrier
whose width the hub changes every few seconds and a block of bursts in
5.025 ms slots of a 40.200 ms frame: the private table's period.

That table, D1h, is the hub's **burst plan** — no public spec, so worked
out from a minute of outbound and return received together (the plan's
burst counts per slot follow the return's bursts eight frames later). Each
section is one return frame: a counter, the frame's start on the NCR's
clock, and a record per burst — the slot it starts in, the terminal, a
code, and its frequency in 50 Hz steps from the plan's own zero. The
code's range gives the length (one, two, four or eight slots of the
frame); its bandwidth is learned from the plan itself — the hub packs
bursts edge to edge, so the spacing between neighbours in a slot gives
each code's extent below and above its frequency (130.6 kHz for 0104h,
213.1 kHz for 02E8h, 157.1 kHz for the two-slot 2278h). The RCS card
then shows a **Burst plan** section: the frame and slots, bursts a frame,
a map of the latest frames (a row a slot, a box a burst in its terminal's
colour), the terminals with their shares of the plan, and the codes
learned. On the **Return pool** card the plan is slid across the pool's
mean spectrum: where it fits (0.97 on 172B) the card gives the plan's zero
as a frequency and draws the latest frame on the pool's band — which
terminal is where. On 172B: 24 terminals, one (413) holding a whole-frame
block at the bottom of the pool (a third of it), the rest one- and
two-slot bursts of 123–400 kHz.

```bash
decsat decode outbound_8Msps.cs8 --decoder ip --offset -881700 --bandwidth 1300000
decsat decode outbound_8Msps.cs8 --decoder pool --offset 580000 --bandwidth 900000
```

Another hub on 172B signals in GSE instead: a 2 MS/s DVB-S2 ACM outbound
whose stream 0 carries, besides its terminals' IP, PDUs of the hub's own —
a 46.08 MHz clock, an **inroute table** (each return carrier's uplink
frequency, symbol rate, slot length in 27 MHz ticks and modulation) and a
**burst time plan** for every inroute every 125 ms frame: a terminal and a
kind for each slot. The IP card shows it as **Hub signalling**: the
inroutes, plans a second, the terminals and their slots. A **Return pool**
VFO over the inroutes takes the plans up by itself: it slides the
inroutes' frequencies along its spectrum, finds the frames on the
inroutes' guard silences (every carrier quiet ~3 ms at each frame's end)
and how many frames after it arrives a plan applies (five — the count
under which the plans' owners best explain the slots' powers), then cuts
out and demodulates every slot's burst (8PSK, a 34-symbol preamble, four
known symbols every 30): per terminal its slots, MER, carrier and timing
offsets, its tag (three preamble symbols of its own) and C/N, and a
constellation. On a 90 s capture: four 665.8 kBd inroutes, ~3300 bursts a
second, 97 % demodulated at MER 8–11 dB. ● Record writes the bursts'
symbols and soft bits to an `.ibursts` file.

```bash
decsat decode cap_10Msps.cs8 --decoder pool --offset 1415000 --bandwidth 3194000 --plan-from=-1290000:2500000:ip
```

## DVB-S2 → MPEG-TS → VLC / PotPlayer

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
| **▶ In app** | plays a service in a window here (below) |

**🔍 TS analyser** opens a window in the spirit of EBSPro's: every PID with
what it is (PAT, PMT, H.264 video, AC-3 audio, ECM/EMM with the CA system,
unlisted PES…), the service it belongs to, its bitrate and share, continuity
and error counts, scrambling and PCR; the service tree with what is on now and
next (EIT); the network's transponders (NIT); and every table seen.

Both network outputs start on 127.0.0.1, this machine only; use 0.0.0.0 (TCP)
or another host's address (UDP) to reach the network. Several players can
connect to the TCP server at once; one that stalls loses data rather than
holding up the receiver.

**BISS.** A stream scrambled with BISS (contribution feeds: the PIDs show
*scrambled*, the services 🔒) takes its key in the analyser, above the
tabs, or in the VFO panel's **BISS key** row: the 12 hex digits (the
checksum bytes are added) or all 16. The key is checked against the
stream — each PES packet must start `00 00 01` once descrambled — and the
row says *the key fits*, or *not this stream's key*. With a key in, the
analyser, the `.ts` recording, UDP, HTTP and the players all get the clear
stream. A key that fits is remembered for its carrier (per satellite, by
RF, in your settings only) and put back in when a VFO lands on that
carrier again still scrambled. BISS mode 1 only (an already-decrypted
BISS-E key in its 16-digit form works too); the descrambler is DVB-CSA,
checked bit-exact against VLC's in both directions, and fast enough for a
whole multiplex (~140 Mbit/s on one core). `decsat biss KEY in.ts out.ts`
descrambles a recording (`--scramble` makes a scrambled test stream).

**▶ In app** (the VFO panel's Play row, or **▶ Play in the app** on a
service in the analyser's Services tab) plays a service in a window:
pictures deinterlaced (bob ×2 by default, a picture per field; a checkbox gives one per frame) and fitted 16:9 (anamorphic SD shown right), its
sound through the app's audio (the volume and mute at the top right, the
output device in the Audio menu) with a volume slider of its own, the pictures timed by the sound.
The window picks the service
and the sound track (language), and lists what ffmpeg says the streams are.
It needs [ffmpeg](https://ffmpeg.org) (`winget install Gyan.FFmpeg`):
found on the PATH, beside DecSAT, or where you point it; it runs as a
child process, below normal priority, decoding whatever it can — H.264
(interlaced and 4:2:2 too), MPEG-2, HEVC, MPEG audio, AAC, AC-3, SMPTE 302M.
A stream still scrambled shows the key field in the player too.

**Television in IP.** A DVB-S2 → GSE/IP VFO looks for transport streams
carried in UDP: datagrams that end in whole 188-byte packets, bare (as
VLC or ffmpeg send `udp://`), behind RTP, or behind a header of the
sender's own (Eutelsat 172B's BBC News multicast puts a sequence number
ahead of its seven packets). The IP card lists them under **MPEG-TS in
UDP**; the busiest (or the one you **Take**) becomes the VFO's MPEG-TS —
services, the analyser, **▶ In app**, a BISS key — and is written to a
`.ts` beside the PCAP while that records.

The player starts at once even on a low-rate feed whose keyframes are ten
seconds or more apart: every transport stream keeps the packets since each
picture's last keyframe (marked by the encoder's random access indicator),
and a player that opens is handed those first, with the latest PAT and
PMT; it decodes them in a burst and shows the newest picture.

## Multicast audio

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

## DVB-S → MPEG-TS

The older standard, still the norm for amateur DATV (QO-100) and some feeds.
The VFO finds the symbol rate as usual, locks the QPSK, then works out the
rest itself: the code rate (1/2 … 7/8), the puncturing phase, and the
carrier's 90° rotation and spectral inversion, by decoding a block under
each hypothesis and keeping the one whose re-encoding matches what was
received. Then Viterbi (K = 7, soft), the sync bytes, the convolutional
deinterleaver, Reed–Solomon (204,188) and energy dispersal — and the stream
goes to the same TS outputs and analyser as DVB-S2's.

## Carrier ID (DVB-CID)

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
[`tests/`](../../tests/README.md)).
