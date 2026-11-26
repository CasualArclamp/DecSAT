<!-- made by CasualArclamp -->
# DecSAT manual: SCPC modems, voice, radar and other carriers

This page describes the modem decoders and the data that they find. [Back to the README](../../README.md).

## TPC 2964 → IP / MPEG-TS

TPC 2964 is the rate-3/4 turbo product code of Intelsat IESS-315 VSAT
carriers. The CTCOM RCV-20x manual describes it:

- A frame has 2964 bits.
- The unique word has 20 bits: F50B8h.
- The codeword is a (64,57) × (46,39) extended-Hamming product codeword.
  It contains 2223 data bits.
- The scrambler is (2, 3, 9, 12) / 475h.
- The modulation is BPSK, QPSK or 8PSK. The labels are Gray labels, as in
  Figure 3.1 of the manual.
- On 8PSK a frame is 988 symbols. Its unique word does not always start on
  the first bit of a symbol.

IESS-315 sets only the performance. It leaves the code to "compatible turbo
modems". Thus the VFO finds the other parameters from the signal:

1. **Unique word**: the VFO looks for it every 2964 bits, for each carrier
   phase ambiguity. If the carrier slips or a symbol is lost, the VFO
   repairs the error at the next frame.
2. **Frame structure**: the VFO tries 432 combinations of these items:
   - row order or column order, in each direction
   - the Hamming generator (all six primitive polynomials of degree 6)
   - if the code bits are scrambled, and how

   The VFO counts how many rows *and* columns are codewords as received.
   With the correct combination, almost all of them are codewords. With
   each other combination, 1 in 128 is a codeword. If two results are
   almost equal, the decoder makes the decision.
3. **Decoder**: an iterative Chase–Pyndiah soft decoder.
4. **Payload**: HDLC (FCS-16/32) or MPEG-TS. The VFO tries each candidate
   descrambler. The candidates are the (2, 3, 9, 12) polynomial
   (self-synchronising or additive), ITU-T V.35 and ITU-T V.29. The VFO
   keeps the descrambler that gives frames with a correct check.

**Comtech TPC 7/8** uses the same decoder. The decoder accepts each unique
word that it finds. There is no document for this format. It was worked out
from a live 171.43 kS/s QPSK carrier:

- A frame has 16 320 bits (8160 QPSK symbols).
- The unique word has 32 bits: F8DD4258h.
- The codeword is a (128,120) × (128,120) extended-Hamming product
  codeword. The generator is x⁷ + x³ + 1. The modem sends the codeword row
  by row.
- The code is shortened. The modem does not send the first 96 bits of the
  first row.
- A frame has 14 304 data bits. The last 24 bits are overhead. The other
  bits are the data of the user. They are exactly 7/8 of the frame
  (300 kbit/s at 171.43 kS/s).
- The same (2, 3, 9, 12) / 475h sequence scrambles the data. The sequence
  starts again each frame, one bit earlier than on TPC 2964.

**Comtech 8QAM, rate 0.8** also uses the TPC decoder. Identify gives its
name and sets the demodulator to 8QAM. There is no document for this
format. It was worked out from a live, idle 427.28 kS/s carrier:

- A frame has 2092 symbols.
- The unique word is 52 BPSK symbols. They are on two opposite corners of
  the 8QAM constellation.
- A systematic codeword follows. It has 1632 data symbols and 408 parity
  symbols.
- The data symbols contain 4896 bits. Each symbol has three bits, least
  significant bit first.
- The data rate is 1.000 Mbit/s at that symbol rate.
- The modem scrambles the data with V.35 before the encoder. The carrier
  showed HDLC idle flags.

**The parity is not decoded.** On an idle carrier, the data do not show
which code the parity uses. Thus the data bits go to the output with the
errors of the channel. A capture with traffic would let the code be found.

![A Comtech 8QAM rate 0.8 carrier under a VFO: 8QAM locked, the unique word found every 2092 symbols, and the frame structure listed in the card](../images/releases/v1.1.0/comtech-8qam.jpg)

*Comtech 8QAM, rate 0.8, on a simulated carrier. The carrier is locked.
The card shows the unique word and the structure of the frame.*

DecSAT finds IP in the HDLC frames in these forms:

- Cisco HDLC
- PPP
- Frame Relay, as RFC 2427 carries it
- each frame that has the packet after a short header

This IP gets the same statistics, PCAP and multicast audio as GSE. If all
the UDP checksums of a flow fail but its headers are correct, the flow is
marked **encrypted** in transit. The audio sniffer ignores such a flow. A
transport stream gets the TS outputs.

**● Record** writes the decoded data to a `.bin` file. The IP has its own
*PCAP* and *Wireshark* buttons. `decsat pcap` reads a recorded PCAP again.
Identify knows the carrier by its unique word. The card shows what each
search found. Thus you can tell a result from a default. The decoder is
confirmed on live Comtech CDM-600L and CDM-570L carriers.

**Comtech EDMAC with bridged Ethernet** was worked out from a live 8PSK
carrier. No specification was available.

- EDMAC increases the data rate by 21/20.
- A frame has 1008 bits. It has a fixed header, five 192-bit data blocks
  and 9-bit overhead groups between the blocks.
- The overhead groups carry the M&C channel of the modems.
- The traffic is bridged Ethernet in 258-byte packet blocks.
- A packet block starts with a 4-byte word. The word has a pointer to the
  end of a packet.
- The packets follow. Each packet has a length byte (or 0 for "to the
  pointer") and a 6-byte header.

A packet with full headers sets up a header-compression context. The
decoder uses this context to build the compressed packets that follow.
Examples are SNMP, the RTP of a radio-over-IP link and the middle fragments
of its SIP. The decoder does a check of the UDP checksums. The IP stage
puts the fragments together.

The Ethernet frames go to the IP stage (statistics, PCAP), as all other
frames do. The **Framing** and **Packets** rows in the side panel show the
status.

**ED-137 radio over IP** is air traffic control radio as VoIP. DecSAT reads
it at each place where it finds it in the IP. The header extension of each
RTP packet carries the signalling of the radio:

- PTT and its type (normal, coupling, priority, emergency, test)
- squelch
- the id of the PTT
- PTT mute and summation
- simultaneous calls
- features such as signal quality and CLIMAX delay

R2S keep-alive packets and voice packets carry this signalling. The
**Radio over IP** section of the IP card shows these items for each stream:

- the state, live (PTT red, squelch green)
- how often and for how long PTT and squelch were on
- the packets lost
- a log of the changes

DecSAT also reads the SIP dialogs that set up the sessions:

- who calls whom
- the SDP (radio type, Tx/Rx mode, keep-alive period, best-signal method)
- if the call was refused or accepted (with one direction only, the
  transaction of the ACK shows this)

The field layout follows the ED-137 dissector of Wireshark. On a live
recording, the result agrees with tshark for each packet.
`decsat pcap … --radio-csv` writes the signalling of each packet.

**You can hear the voice.** The sound is G.711, A-law or µ-law. The **Radio
voice** card plays the sound of the packets whose PTT or squelch is on. It
plays this sound as it plays the sound of a call:

- With the voice preset, the sound plays automatically.
- One stream and one talker play at a time in the full app.
- ▶ plays the stream without these conditions.
- ● records a `.wav` file.
- There is silence between transmissions.

## Voice over E1 (G.704, Comtech D&I++)

Some modems carry telephony or radio links (airband voice is an example).
These modems send 64 kbit/s G.711 channels. They send a full E1, or they
use Comtech **Drop & Insert++**. D&I++ sends the selected timeslots in
blocks of 2944 bits (64 overhead bits, 2880 data bits). The layout is not
published. It was worked out from CDM-600L carriers:

- A block starts with a 21-bit sync and three bits. The three bits show if
  the block starts a frame.
- Five blocks of 72 bytes follow. There are 10 bits of overhead before
  each of them.
- This gives one block for each timeslot every 45 ms.
- The bytes of the timeslots alternate.

After the TPC 2964 decoder, the payload search finds E1 or D&I++. It finds
them with the V.35 descrambler or with each other descrambler that it
tries. The **Voice** card lists the channels with their levels. **▶** plays
a channel with the player and the volume of the app. **●** records a `.wav`
file.

If the sign bit of a timeslot does not change, the timeslot is not G.711
audio. It contains sub-rate channels or compressed voice. The card marks
it **not G.711** and shows the bits that change. Such a timeslot plays as
silence, because as A-law it would be loud digital noise. A timeslot that
has no decision yet also plays as silence.

Those bits can carry **G.728** (LD-CELP, 16 kbit/s: a 10-bit codeword every
five samples). Then the card shows **G.728 speech**, and ▶ and ● play and
record the decoded voice. While a person speaks, the VFO becomes green with
🔊 in the waterfall and in the VFO list.

The voice equipment behind a CDM-600L on Horizons 3e sends G.728 in bits
2–3 of its timeslot. It uses the frame format of G.728 (§3.11):

- The top bit of the shape index carries a sync bit.
- The sync bit is different one time every 20 ms. The decoder locks to it.
- The decoder knows the silence fill of the equipment and mutes it.
- A channel that sends silence fill shows **G.728, silent** before a
  person has spoken.

The decoder is written from the Recommendation. It agrees with the test
vectors of Appendix I (112 dB SNR or better).

**Multi-radio sites** send their timeslots differently. They send HDLC
packets on an inverted line (the idle flags read `00000011` again and
again). Each packet has these parts:

- a sequence count
- a channel byte (`08h`–`F8h`)
- a type byte `13h`
- 44 octets (5.5 ms) of one 64 kbit/s timeslot

DecSAT joins the packets of each channel into a continuous timeslot. The
**Voice** card lists the channels by the channel byte (ch 10h, 18h, …). It
finds, plays and records the G.728 in each channel as given above.

A timeslot of a trunk can contain a maximum of three G.728 channels in
different bit pairs. DecSAT decodes each of them. *One voice at a time*
also applies in the timeslot. Packets of a different type on such a trunk
carry the IP of the site. This IP goes to the IP stage, with the ADS-B in
it (see below). A site with no traffic sends only a status packet every
two seconds.

The **Comtech CDM modem** entry has these voice models:

- CDM-600L voice
- CDM-570L trunk
- *type found*, which selects the chain from the first look and the model
  from the data

These models play each channel that has G.728 automatically.

**One voice at a time** (in the Audio menu, on by default) operates as the
hang time of a scanner does, for all carriers and channels. You hear the
voice that speaks first. The other voices wait until that voice is quiet
for the **hold** time (3 s, set adjacent to the switch). When the switch
is off, DecSAT mixes all voices. You always hear a channel that you play
with ▶.

The volume slider and the 🔊 mute at the top right are the master controls
for all sound. **Audio ▸ Output** selects the sound output. You can select
the default of the system or a device by name (a headset, or a virtual
cable into other software). Players that already run move to the new
output immediately. DecSAT remembers the selection.

Offline:

```bash
decsat decode capture_148148Sps.cf32 --decoder tpc --out dir --e1-record 1
```

## Radar feeds and the radar scope

The same thin-route links carry air-traffic surveillance data. Each VFO
whose chain gives the frames reads the data. The **Comtech CDM modem**
entry with *type found* selects the chain automatically. CDM-570L Viterbi
and CDM-570L trunk are the usual chains. The data has three forms:

- **Track reports in IBM bisync blocks** (SYN, an SOH heading, DLE STX …
  DLE ETX, a BCC) on a 9.6 kbit/s link. There is one message for each
  sector and there are 32 messages for each scan. Each report gives the
  position, velocity, Mode A code, Mode C level and ground speed of a
  track. The format is not a standard. It was worked out from the signal.
- **ASTERIX** in HDLC UI frames on 64 kbit/s links:
  - category 034 service messages (sector crossings, the north marker with
    the position of the radar and the scan time)
  - category 048 target reports (position, Mode 3/A, flight level, Mode S
    address, track number, velocity, status)
  - the Mode S registers that the radar reads from each aircraft (BDS 4,0,
    5,0 and 6,0: selected altitude, altimeter setting, true track, heading,
    IAS, Mach, vertical rates)

  DecSAT accepts a block only if its records parse to its exact length
  with the standard UAPs.
- **ADS-B** as ASTERIX category 021 (edition 0.23) in the IP of a trunk.
  Each aircraft reports its own position, altitude, callsign, squawk and
  emitter class.

The **Radar feed** card of the VFO lists the tracks. **📡 Radar scope**
opens a window with a PPI. The PPI has these items:

- the beam, which turns at the measured scan rate
- range rings
- trails
- one-minute leaders
- Mode A, level and speed labels
- details when you hover
- a track table
- zoom with the wheel and pan with a drag

Click a track, or its row, to see its details:

- Mode S/A/C
- climb or descent
- speed and heading
- range and bearing from the radar and from the nearest airport
- latitude and longitude, when the position of the radar is known
- the status word, field by field
- the raw octets

The colours have these meanings:

- Coasted tracks are yellow.
- Fast or high tracks are purple (650 kt or more, FL450 or above, or more
  than 350 kt below FL100).
- When an ADS-B feed is in step with the clock of the radar, the ADS-B
  aircraft are magenta diamonds. The scope draws them on the radar picture
  at the positions that they report.
- Then the radar tracks that agree with no ADS-B aircraft become red.

Below the PPI are two switches:

- **Map** draws OpenStreetMap tiles. DecSAT gets them with the `curl` of
  the system and keeps them permanently in a cache in `%APPDATA%\DecSAT`.
  You can select **Light** or dark.
- **Airports** draws the airports and runways from the open data of
  OurAirports. DecSAT gets the data one time and gets it again each month.

The position of the radar comes from the feed when the feed gives it (the
north marker of ASTERIX). If the feed does not give it, set it with
**Site…**. DecSAT keeps the position in your own settings, with the radar
as the key. It never keeps the position in the project.

Some tracks have no name in the feed. They get callsigns from an ADS-B
feed that a different VFO decodes. DecSAT sends no request to a third
party for this. Only when the **adsb.lol** switch is on (it is off by
default), they can also get callsigns from that site. The site gives its
live aircraft near the radar.

The waterfall tags a carrier **ADS-B** or **RADAR** after a decoder finds
one of them in the carrier. DecSAT remembers the carrier. While a VFO
records, its track reports go to a `.csv` file adjacent to the PCAP.

A CDM-570L trunk can carry the **monitoring and control** multicast of a
radar station. The multicast is in Frame Relay. Its format is
word-oriented and has typed points. The data give the names of the units.
Such a trunk gets a **Radar station** card above the IP card. Its scope
shows these items adjacent to the PPI:

- the units of the station
- the readings that change
- the targets of the radar feed that a different VFO decodes

The meaning of the points is not known at this time.

Offline, and with no operator:

```bash
decsat decode radar_64000Sps.cf32 --decoder auto --fast      # a radar link: its reports
decsat payload trunk.bin --frame-bits 2223 --bit-rate 1024000 --radar-out tracks.csv --voice-out voice.wav   # a trunk's data recording
decsat pcap recording.pcap --csv points.csv                   # a recorded PCAP: the IP, a station's points
decsat-gui capture.cs8 --vfo-at 1461800:140000 --decoder trunk --radar-scope --radar-range 120 --after 20 --screenshot scope.png
```

## NovelSat NS4

A NovelSat modem in NS4 mode sends the frame of DVB-S2. These items are
the same as in DVB-S2:

- the SOF and the PLS code
- the slots and the pilots
- the Gold-code scrambler
- the BBHEADER and the baseband scrambler
- the BCH code

Two items are different. The PLS code gives different numbers to the
modes, and the LDPC codes are NovelSat codes. If you read such a carrier
as DVB-S2, one of two things occurs. The carrier locks with the incorrect
MODCOD name (QPSK 1/2 shows as *QPSK 2/3*), or it loses lock again and
again. In each case, the FEC of all frames fails. NovelSat publishes no
data about NS4. The data on this page was measured from a modem.

Select **NovelSat NS4 → GSE/IP (PCAP)** as the decoder of the VFO
(`--decoder ns4` on `decsat decode`). This is the DVB-S2 decoder with two
changes. It reads the header as NS4 defines it, and it uses the LDPC codes
of NS4. After the BBFRAME, all steps are the same as for DVB-S2.

```
decsat decode ns4_645161Sps.cf32 --decoder ns4 --symbol-rate 33000 --bandwidth 60000
```

| | PLS code (pilots off / on) | |
|---|---|---|
| QPSK 1/4 | 4 / 8 | ✅ |
| QPSK 1/2 | 21 / 25 | ✅ |
| QPSK 3/5 | 28 / 32 | ✅ |
| QPSK 2/3 | 33 / 37 | ✅ |
| QPSK 3/4 | 40 / 44 | ✅ |
| QPSK 5/6 | 52 / 56 | ✅ |
| QPSK 1/5, short frame | 6 / 10 | ✅ |
| QPSK 4/9, short frame | 23 / 27 | ✅ |

The code of a short frame is the code of the normal frame plus 2. The
modem gives short frames the name of their true rate. 1/5 is the short
"1/4", and 4/9 is the short "1/2".

These modes are not decoded at this time, and DecSAT skips their frames:

- the other QPSK rates of a modem with normal frames (1/3, 2/5, 13/30,
  7/15, 8/15, 17/30, 19/30, 32/45, 4/5, 8/9, 9/10)
- fourteen more QPSK rates with short frames
- the higher constellations

The payload of the Ethernet modes of the modem is not GSE. It has a
NovelSat wrapper, and DecSAT does not unpack it. DecSAT counts the BBFRAMEs
and shows their headers.

The LDPC codes are quasi-cyclic, as the DVB-S2 codes are. They have their
own address tables and a different parity part:

- The modem sends each parity group as one unit. This is the order that
  the parity interleaver of DVB-T2 gives.
- The chain of parity bits closes on itself.
- One parity group is in three checks.

Each table was worked out from the PRBS test pattern of the modem. Each
table has a test against a frame as recorded.

## Generic PSK → symbols

This decoder is for carriers that are not DVB-S2: SCPC data, telemetry and
DVB-S. The VFO locks the carrier and shows its constellation. Push
**● Record** to write the hard-decided symbols to this file:

`decsat-<VFO>-<RF>MHz-<rate>Bd-<modulation>-<time>.bin`

The file has one byte for each symbol. The byte is the bit label of the
symbol in the DVB-S2 mapping (BPSK: 0 = +1). The symbol rate and the
constellation come from Identify, or you can set them manually.

Without a preamble, the carrier phase is ambiguous by the symmetry of the
constellation (90° for QPSK). Thus the labels can be a fixed rotation of
the labels that were sent. A sync word that you find offline removes the
ambiguity.

**Text (live)**: the decoder also searches the decided bits for strings
that you can read. It tries these alternatives:

- each phase rotation and the mirror image
- the two bit orders
- all eight byte alignments
- plain bits and differentially decoded bits

For QPSK this gives 256 alternatives at the same time. The decoder shows
the alternative that has much more text than the others. The text scrolls
as the strings arrive (telemetry, beacons, NMEA, idle messages). The
decoder also shows the longest strings from all alternatives as
candidates. Clear *look for text* to decrease the CPU load (approximately
a quarter of a core at 1 MBd).

## A Barker-11 spread carrier (C band, 4004 MHz)

The decoder is **Barker-11 spread BPSK** (`barker` on the command line).
Put a VFO of 250 kHz or wider on the carrier. Each BPSK symbol of the
carrier is one period of the 11-chip Barker code (166.667 kchip/s,
15 151.5 symbols/s). Two such carriers are 250 kHz apart near 4004 MHz.
Each carrier is adjacent to a comb of eight pilot tones.

The decoder does these steps:

- It despreads the carrier.
- It follows the code and the carrier.
- It cuts the frames at the 255-symbol preamble.
- It finds the 64-bit unique words.
- It shows the data blocks in hex.

The card draws the frames as a bitmap. A row is a frame and a column is a
symbol. The preamble and the unique words are blue. The data that does not
change is grey.

The format was measured from a recording. No specification is known. The
contents of the blocks are also not known. No interpretation that was
tried gave text.

![The Barker-11 decoder on a carrier near 4004 MHz: the despread spectrum, the frame fields in hex, the frames drawn as a bitmap and the BPSK constellation](../images/releases/v1.8.0/barker-decoder.jpg)

*The Barker-11 decoder. It shows the frames in hex and as a bitmap. A row
is a frame.*

```bash
decsat decode capture_8000000Sps.cs8 --decoder barker --offset -813000 --bandwidth 300000 --fast
```
