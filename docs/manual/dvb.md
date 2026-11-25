<!-- made by CasualArclamp -->
# DecSAT manual: DVB-S2, DVB-S2X, DVB-S, DVB-RCS and DVB-CID

This page describes the DVB decoders and their outputs. [Back to the README](../../README.md).

## DVB-S2 → GSE/IP → PCAP

A **DVB-S2/S2X → GSE/IP** VFO demodulates the carrier, decodes LDPC and
BCH, and reads the BBFRAMEs. The generic-stream frames go through GSE
(ETSI TS 102 606) to IP. The decoder assembles the fragments again and
does a check of each CRC-32.

The side bar shows these items:

- the stream
- the IP packet count and the byte count
- the IP rate
- the protocols
- the flows that have the most traffic.

Push **● Record** adjacent to *PCAP* to write the packets to a `.pcap`
file. The file contains raw IP and opens in Wireshark.

**▶ Live** adjacent to *Wireshark* opens Wireshark and sends it the packets
as DecSAT decodes them. Wireshark starts a capture from its standard input
(`-k -i -`). DecSAT writes a PCAP stream to that input. The RTP heuristic
of Wireshark is on. Thus ED-137 streams show their PTT and squelch without
the SIP that set them up. **⏹ Stop** stops the feed. The feed also stops
when you close Wireshark. Wireshark keeps the packets that it has.

Each decoder that carries IP has the two buttons. A TS decoder gets them
when IP comes into view in the TS, by MPE. `decsat decode … --wireshark`
does the same. The GUI also does the same when you start it on a carrier:

```bash
decsat-gui --hackrf 1748 --rate 4 --lo 10600 --amp --vfo-at -422000:300000 --decoder tpc --wireshark
```

VFOs that you make with `--vfo-at` give a one-off run. DecSAT does not
restore your last session and does not write over it.

Not all real links obey the standard. As
[dontlookup](https://github.com/ucsdsysnet/dontlookup) does, DecSAT reads
each data field in four ways at the same time:

- GSE_LENGTH includes the 2-byte header, or does not include it.
- The fragment id is 8 bits, or it is 6 bits plus a 2-bit counter.

DecSAT keeps the way that gives valid IP (checksums, lengths). The *GSE*
setting can force one way. If no way gives IP, a blind IPv4 search through
the data fields starts.

DecSAT reads each stream of a multistream carrier separately. Each stream
has its own GSE (or TS) state. S2X has GSE-HEM: the modulator cuts GSE
packets across BBFRAMEs as it does TS packets, and SYNCD points at the
first packet that starts. DecSAT assembles these packets again.

DecSAT reads the roll-off as S2X signals it. The codes 00, 01 and 10, sent
alternately with 11, mean 0.15, 0.10 and 0.05. DecSAT does not show a 0.05
carrier as 0.20.

**Terminals.** On the outbound of a VSAT hub, each GSE packet carries the
MAC of its terminal as a 6-byte label. On an ACM outbound, the hub sends
the data of each terminal in the MODCOD that the reported reception of the
terminal permits. The *Terminals* table of the IP card lists the labels by
bytes. For each label, the table shows:

- the MODCODs that the hub used for it
- the Es/N0 that the primary MODCOD must have
- its IP addresses, when you hover.

The table is a map of the terminals of the network and their links, as
the hub sees them.

## Interactive networks (DVB-RCS) and return pools

The outbound of a hub tells the terminals how to transmit. With DVB-RCS
(EN 301 790; EN 301 545-2 for RCS2), a programme of signalling tables in
the transport stream does this. On a multistream outbound, this programme
is frequently a separate input stream: transport packets in a generic
stream.

The TS card then gets an **Interactive network** card. The card shows:

- the name of the network
- the forward and return links of its RCS map (satellite, beam, frequency,
  symbol rate, standard)
- the network clock reference and its interval (the reference is the
  27 MHz count that each terminal uses to time its bursts)
- the status flags of the terminal information
- the superframe, frame and timeslot plans
- the burst time plan and the corrections, when the hub sends them.

Below *Signalling tables*, the card lists each table id with its PID,
count, period and size. The list includes the private tables of a vendor.
The list also includes tables that a PMT declares but the hub does not
send. A hub that operates its return with private tables thus continues to
show its timing in the rates of those tables.

The **Return pool** decoder is in TDMA mode, adjacent to the burst
analyser. Put it on the return. It shows:

- the carriers at this time (a 200 ms average, divided at the guard bands
  between the carriers)
- how frequently the layout changes (a hub changes the size and the
  position of the carriers of its terminals, Mx-DMA style)
- the grid.

The grid has two parts. The slot comes from the repeat of the guard times
of the bursts. The frame is the multiple of the slot at which the envelope
repeats.

The pool has its own fast waterfall. One FFT is approximately one
millisecond, and the waterfall shows the last three frames. Below it are
the **burst lanes**. The burst lanes are the burst map of the TDMA
decoder, turned 90°:

- A column is a lane, below its frequencies.
- A box is a burst in the lane.
- The newest burst is at the bottom.

The lanes are the bins that burst on and off together. Where two bursts
have no interval between them, DecSAT cuts them where the FFT across their
guard time has a dip.

When DecSAT has timed the burst plan of an outbound on the pool, the
display changes:

- The columns are the bands that the bursts of the plan use.
- Each box is a planned burst in the colour of its terminal.
- The brightness of a box shows its C/N.
- An outline shows a burst that the plan gives but that does not come.

Sometimes a different VFO decodes an outbound whose NCR or signalling
table has the same period. The card then tells you that the pool operates
on the clock of that outbound.

On Eutelsat 172B, a Newtec hub has a 1.048 MS/s S2X ACM outbound (roll-off
0.05). The outbound carries its signalling on ISI 0 at QPSK 1/3: an RCS
map, the NCR and two private tables, one of them each 40.2 ms. It carries
IP to its terminals on ISI 1. The 770 kHz pool adjacent to it contains:

- a continuous carrier of ~190 kHz, whose width the hub changes at
  intervals of a few seconds
- a block of bursts in 5.025 ms slots of a 40.200 ms frame. This is the
  period of the private table.

That table, D1h, is the **burst plan** of the hub. There is no public
specification. The format was worked out from one minute of the outbound
and the return, received together. The burst counts of the plan for each
slot agree with the bursts of the return eight frames later.

Each section is one return frame. It contains a counter, the start of the
frame on the clock of the NCR, and one record for each burst. A record
contains:

- the slot in which the burst starts
- the terminal
- a code
- the frequency, in 50 Hz steps from the zero of the plan.

The range of the code gives the length: one, two, four or eight slots of
the frame. DecSAT learns the bandwidth of the code from the plan. The hub
puts the bursts edge to edge. Thus the distance between adjacent bursts in
a slot gives the extent of each code below and above its frequency. The
extents are 130.6 kHz for 0104h, 213.1 kHz for 02E8h, and 157.1 kHz for
the two-slot 2278h.

The RCS card then shows a **Burst plan** section:

- the frame and the slots
- the bursts in one frame
- a map of the latest frames (a row is a slot, a box is a burst in the
  colour of its terminal)
- the terminals, with their shares of the plan
- the codes that DecSAT learned.

On the **Return pool** card, DecSAT moves the plan across the mean
spectrum of the pool. Where the plan fits (0.97 on 172B), the card gives
the zero of the plan as a frequency. The card also draws the latest frame
on the band of the pool. This shows which terminal is where.

On 172B there are 24 terminals. One terminal (413) has a full-frame block
at the bottom of the pool (one third of the pool). The other terminals
have one-slot and two-slot bursts of 123–400 kHz.

```bash
decsat decode outbound_8Msps.cs8 --decoder ip --offset -881700 --bandwidth 1300000
decsat decode outbound_8Msps.cs8 --decoder pool --offset 580000 --bandwidth 900000
```

A different hub on 172B sends its signalling in GSE. It has a 2 MS/s
DVB-S2 ACM outbound. Stream 0 carries the IP of its terminals and also
private PDUs of the hub:

- a 46.08 MHz clock
- an **inroute table**: for each return carrier, the uplink frequency, the
  symbol rate, the slot length in 27 MHz ticks and the modulation
- a **burst time plan** for each inroute, for each 125 ms frame: a
  terminal and a kind for each slot.

The IP card shows this as **Hub signalling**: the inroutes, the number of
plans in one second, the terminals and their slots.

A **Return pool** VFO across the inroutes uses the plans automatically. It
does these steps:

1. It moves the frequencies of the inroutes along its spectrum.
2. It finds the frames from the guard silences of the inroutes. Each
   carrier is silent for ~3 ms at the end of each frame.
3. It finds how many frames after its arrival a plan applies. The result
   is five. With this count, the owners in the plans best agree with the
   powers of the slots.
4. It cuts out and demodulates the burst of each slot. A burst is 8PSK
   with a 34-symbol preamble and four known symbols in each 30.

For each terminal, the card shows its slots, MER, carrier offset, timing
offset, tag (three preamble symbols of its own) and C/N. The card also
shows a constellation.

On a 90 s capture, DecSAT found four 665.8 kBd inroutes and ~3300 bursts
in one second. It demodulated 97 % of the bursts at MER 8–11 dB. ● Record
writes the symbols and the soft bits of the bursts to an `.ibursts` file.

```bash
decsat decode cap_10Msps.cs8 --decoder pool --offset 1415000 --bandwidth 3194000 --plan-from=-1290000:2500000:ip
```

## DVB-S2 → MPEG-TS → VLC / PotPlayer

A **DVB-S2/S2X → MPEG-TS** VFO builds the transport stream again from
TS-mode BBFRAMEs. It restores the sync bytes from the CRC-8 chain and puts
back the deleted null packets. It shows the services (from the PAT, PMT
and SDT), the PIDs, the CRC errors and the continuity errors. Use one of
these outputs:

| | |
|---|---|
| **TS file → ● Record** | a `.ts` in the output folder |
| **UDP** `127.0.0.1:1234` | VLC: `udp://@:1234` · PotPlayer: `udp://127.0.0.1:1234` |
| **TCP / HTTP** `127.0.0.1:8001` | VLC or PotPlayer: `http://127.0.0.1:8001/` (VLC also `tcp://…`) |
| **▶ VLC / ▶ PotPlayer** | starts the TCP server and opens the stream in the player |
| **▶ In app** | plays a service in a window of DecSAT (see below) |

**🔍 TS analyser** opens a window in the style of the EBSPro
analyser. It shows:

- each PID with its type (PAT, PMT, H.264 video, AC-3 audio, ECM/EMM with
  the CA system, unlisted PES…), its service, its bitrate and share, its
  continuity and error counts, its scrambling and its PCR
- the service tree, with the programme that is on now and the subsequent
  programme (EIT)
- the transponders of the network (NIT)
- each table that DecSAT received.

The two network outputs start on 127.0.0.1, which is this computer only.
To get access from the network, use 0.0.0.0 (TCP) or the address of a
different host (UDP). More than one player can connect to the TCP server
at the same time. A player that stops reading loses data. It does not
delay the receiver.

**BISS.** Contribution feeds use BISS scrambling. On such a stream, the
PIDs show *scrambled* and the services show 🔒. Type the key in one of
these places:

- the analyser, above the tabs
- the **BISS key** row of the VFO panel.

Type the 12 hex digits (DecSAT adds the checksum bytes) or all 16 digits.
DecSAT does a check of the key against the stream. Each PES packet must
start with `00 00 01` after it is descrambled. The row then shows *the key
fits* or *not this stream's key*.

When a key is in, these outputs all get the clear stream: the analyser,
the `.ts` recording, UDP, HTTP and the players. DecSAT keeps a correct key
for its carrier (for each satellite, by RF, in your settings only). When a
VFO comes on that carrier again and the stream is scrambled, DecSAT puts
the key back in.

DecSAT does BISS mode 1 only. A BISS-E key that is already decrypted, in
its 16-digit form, is also satisfactory. The descrambler is DVB-CSA. It
is bit-exact against that of VLC in the two directions. Its speed is
sufficient for a full multiplex (~140 Mbit/s on one core).
`decsat biss KEY in.ts out.ts` descrambles a recording. `--scramble` makes
a scrambled test stream.

**▶ In app** plays a service in a window. It is in the Play row of the VFO
panel. It is also **▶ Play in the app** on a service in the Services tab
of the analyser. The player does these things:

- It deinterlaces the pictures. The default is bob ×2, one picture for
  each field. A checkbox gives one picture for each frame.
- It fits the pictures to 16:9 and shows anamorphic SD correctly.
- It plays the sound through the audio of the app. The volume and the mute
  are at the top right. The output device is in the Audio menu. The window
  also has its own volume slider.
- It times the pictures by the sound.

In the window, you select the service and the sound track (language). The
window lists what ffmpeg reports for the streams.

The player must have [ffmpeg](https://ffmpeg.org)
(`winget install Gyan.FFmpeg`). DecSAT finds ffmpeg on the PATH, adjacent
to DecSAT, or where you tell it. ffmpeg runs as a child process, below
normal priority. It decodes all that it can: H.264 (interlaced and 4:2:2
also), MPEG-2, HEVC, MPEG audio, AAC, AC-3, SMPTE 302M. If a stream is
scrambled, the player also shows the key field.

**Television in IP.** A DVB-S2 → GSE/IP VFO looks for transport streams
that UDP carries. These are datagrams that end in complete 188-byte
packets. The packets can be:

- bare, as VLC or ffmpeg send `udp://`
- after RTP
- after a private header of the sender. The BBC News multicast of Eutelsat
  172B puts a sequence number before its seven packets.

The IP card lists them below **MPEG-TS in UDP**. The stream with the
highest rate (or the one that you **Take**) becomes the MPEG-TS of the
VFO. It then has services, the analyser, **▶ In app** and a BISS key.
While the PCAP records, DecSAT also writes this stream to a `.ts`
adjacent to the PCAP.

The player starts immediately, also on a low-rate feed whose keyframes are
ten seconds or more apart. Each transport stream keeps the packets since
the last keyframe of each picture. The random access indicator of the
encoder identifies the keyframe. A player that opens gets those packets
first, with the latest PAT and PMT. The player decodes them quickly and
shows the newest picture.

## Multicast audio

Satellite links carry radio as IP multicast. The multicast is in GSE, or
in MPE in a transport stream. DecSAT reads the two types. A DVB-S2 IP VFO
or TS VFO lists the audio streams that it finds below **Multicast audio**.
The list gives:

- the name from SAP/SDP announcements, where there are announcements
- the codec (AAC in ADTS, LATM/LOAS or RFC 3640, MPEG audio, PCM, Opus)
- the bitrate and the RTP payload type.

The list is in the sequence of the addresses.

DecSAT describes streams without an announcement from their own packets:

- RFC 3640 AAC: by its RTP clock rate (from RTCP sender reports, or timed
  against the signal), by the clock ticks of one access unit (1024:
  AAC-LC; 2048: HE-AAC), and by its first element (mono or stereo).
- Opus: by packets that parse as Opus.

DecSAT first assembles IPv4 fragments again. A multiplex that puts some
AUs into a 2.3 kB RTP packet sends each packet as two fragments.
`<nowplaying>` messages (title, artist, station) are in the list below
**Now playing**.

- **▶ Play** decodes the stream in DecSAT. The codecs are MPEG audio
  layers I–III, AAC-LC and HE-AAC v1/v2 with SBR and PS (libxaac; from
  ADTS, LATM/LOAS or RFC 3640), Opus (libopus) and PCM/G.711. There are
  **⏸ Pause** (the sound comes back live), **⏹ Stop** and a level meter.
  The volume slider and the 🔊 mute are at the top of the list. They apply
  to all of the app, and DecSAT keeps their settings.
- **⏺ Record** saves the stream to the output folder as broadcast.
  DecSAT does not encode it again. The files are `.mp2`/`.mp3`, `.aac`
  (ADTS, for each AAC carriage, HE-AAC not changed), `.wav` for PCM, `.ts`
  for TS in UDP, and `.opus` (Ogg) for Opus.
- **… → Open in VLC / PotPlayer**: DecSAT sends an RTP stream that has a
  description to a local port and gives the player an SDP file. The player
  then decodes HE-AAC, LATM and the other codecs itself. DecSAT supplies a
  bare elementary stream at a local HTTP address.

This function follows the
[VK2SWL DVB-S/S2 Multicast Audio Receiver](https://github.com/VK2SWL/DVB-S-S2-Multicast-Audio-Receiver).

## DVB-S → MPEG-TS

DVB-S is the older standard. It is still the usual standard for amateur
DATV (QO-100) and some feeds. The VFO finds the symbol rate as usual and
locks the QPSK. Then it finds these parameters itself:

- the code rate (1/2 … 7/8)
- the puncturing phase
- the 90° rotation of the carrier
- the spectral inversion.

To do this, it decodes a block for each hypothesis. It keeps the
hypothesis whose encoded result agrees with the received block.

The subsequent stages are:

1. Viterbi (K = 7, soft)
2. the sync bytes
3. the convolutional deinterleaver
4. Reed–Solomon (204,188)
5. energy dispersal.

The stream then goes to the same TS outputs and analyser as that of
DVB-S2.

## Carrier ID (DVB-CID)

Modern uplink modulators put a carrier identification signal below their
carrier (ETSI TS 103 129). The signal is BPSK, spread with 4096 chips for
each bit. It is 27.5 dB below the spectral density of the carrier and
220 Hz above the centre of the carrier. It identifies the transmitter:

- the 64-bit identifier of the modulator (frequently its MAC address)
- the position, a telephone number and a short text, if the operator
  entered them.

Put a **Carrier ID (DVB-CID)** VFO on a carrier. The VFO must be a minimum
of 1.35 × the chip rate wide: 151 kHz below 512 kBd, 302 kHz above. The
VFO measures the carrier and searches for the spreading code (less than
one second). Then it reads the frames. There is one frame each 36 s (18 s
on carriers of 512 kBd and more).

The card shows the tracking live:

- the despread bits and their differential products as constellations
  (with Es/N0)
- the early, prompt and late correlators on the correlation peak of the
  code
- the SNR and the frequency during the last few seconds
- the soft bits
- the frame sync, which fills copy by copy.

Below **Code search**, the card shows the acquisition: the correlation
through the 4096 code phases, and code phase × frequency around the peak.
Offline:

```bash
decsat decode capture_1000000Sps.cf32 --decoder cid --fast
```

If you do not have a CID, `decsat cid` writes one below a DVB-S2 carrier.
You select the identifier, the position, the telephone number and the
text (see [`tests/`](../../tests/README.md)).
