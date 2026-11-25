<!-- made by CasualArclamp -->
# DecSAT manual: Inmarsat L band

This page describes the Inmarsat decoders. [Back to the README](../../README.md).

## Inmarsat GMR-2+ (IsatPhone, L band)

The decoder is **Inmarsat GMR-2+** (`gmr2` on the command line). Put a
60 kHz VFO on one of the 50 kHz forward carriers around 1525–1559 MHz. Use
a carrier that pulses four times a second. This is a control carrier. It is
on for half of each 235 ms multiframe.

The decoder decodes:

- The **sync channel**: the colour codes of the spot beam (NCC, BCCC) and
  the TDMA frame number (T1, T2).
- The **broadcast System Information**:
  - SI3 (network MCC-MNC, location area, paging, access).
  - SI2 (neighbour beams).
  - SI9 (timing advance, epoch).
  - The GMR-2+ messages that are not in the GMR-2 specification. These are
    a 22-part beam map and two others, which the decoder shows raw.

The card gives the network and the location area, the Eb/N0, the beam, the
frame number, the blocks decoded and the messages seen. Hover on the last
line to see the messages. The constellation shows the OQPSK carrier as four
clusters. Traffic channels are encrypted (A5-GMR-2), and DecSAT does not
touch them. DecSAT does not decode the 200 kHz full-rate carriers.

```bash
decsat decode gmr2_ch88.cf32 --decoder gmr2 --offset=980 --bandwidth 60000 --fast
```

## Inmarsat Classic Aero (L band)

The decoder is **Inmarsat Aero** (`aero` on the command line). Put a VFO on
a carrier of the aeronautical service around 1545 MHz. The decoder names
the channel and decodes it.

This is the receiver of [DecAero](https://github.com/CasualArclamp/DecAero)
(same author). It operates on the baseband of the VFO. DecAero stays the
application for these tasks:

- To monitor a full band.
- To follow calls automatically.
- To keep an aircraft list.

The decoder reads these channels:

- **P channel** (ground to aircraft; 600 and 1200 bit/s A-BPSK, 10500 bit/s
  A-QPSK). The decoder shows the signalling units in words: log-on, channel
  and voice assignments, and call announcements with the kind of call. It
  shows ACARS messages. It shows other user data in hex.
- **R and T channels** (aircraft to ground, in bursts). The decoder shows
  the same data. It also reads ADS-C position reports from the ACARS text
  of the aircraft.
- **C channel** (a telephone call). The decoder plays 8400 bit/s (AMBE)
  speech and 21000 bit/s speech through the audio output of the
  application. The 21000 bit/s speech uses the 9.6 kbit/s multipulse codec
  of ICAO Annex 10. The decoder shows the progress of the call in words.
  It names a 21000 bit/s fax or data call, but it does not decode it.

The card is the same as the DVB card. It shows:

- The channel and the bit rate.
- The link margin. This is the Es/N0 above the point where the rate ½ code
  of the channel fails. The bar has a mark at that point.
- The demodulator, the frames, Viterbi, and the signal units or the voice.
- The log, with the newest line first.

**Record** writes the log to a text file.

Make the VFO sufficiently wide:

- 4 kHz or more for the slow channels.
- Approximately 12 kHz for 10500 and 8400 bit/s.
- 25 kHz for 21000 bit/s.

A narrower VFO gives too few samples, and the card tells you. The decoder
logs voice assignments, but it does not follow them. You must put the VFO
for the call.

```
decsat decode aero_625000Sps.wav --decoder aero --offset -75600 --bandwidth 4000 --fast
```
