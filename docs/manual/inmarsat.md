<!-- made by CasualArclamp -->
# DecSAT manual: Inmarsat L band

This page describes the Inmarsat decoders. [Back to the README](../../README.md).

## Inmarsat GMR-2+ (IsatPhone, L band)

Decoder **Inmarsat GMR-2+** (`gmr2` on the command line): put a 60 kHz VFO
on one of the 50 kHz forward carriers around 1525–1559 MHz that pulse four
times a second (a control carrier: on for half of each 235 ms multiframe).
It decodes:

- the **sync channel**: the spot beam's colour codes (NCC, BCCC) and the
  TDMA frame number (T1, T2);
- the **broadcast System Information**: SI3 (network MCC-MNC, location
  area, paging, access), SI2 (neighbour beams), SI9 (timing advance, epoch)
  and the GMR-2+ messages not in the GMR-2 specification (a 22-part beam map
  and two others, shown raw).

The card gives the network and location area, the Eb/N0, the beam, the
frame number, the blocks decoded and the messages seen; hover the last line
for the messages themselves. The constellation shows the OQPSK carrier as
four clusters. Traffic channels are encrypted (A5-GMR-2) and not touched;
the 200 kHz full-rate carriers are not decoded.

```bash
decsat decode gmr2_ch88.cf32 --decoder gmr2 --offset=980 --bandwidth 60000 --fast
```

## Inmarsat Classic Aero (L band)

Decoder **Inmarsat Aero** (`aero` on the command line): put a VFO on a
carrier of the aeronautical service around 1545 MHz and it names the channel
itself and decodes it. This is [DecAero](https://github.com/CasualArclamp/DecAero)'s
receiver (same author) run on the VFO's baseband; DecAero remains the app
for watching a whole band, following calls by itself and keeping an
aircraft list.

- **P channel** (ground to aircraft; 600 and 1200 bit/s A-BPSK, 10500 bit/s
  A-QPSK): signalling units in words (log-on, channel and voice assignments,
  call announcements with the kind of call), ACARS messages, other user data
  in hex.
- **R and T channels** (aircraft to ground, in bursts): the same, with ADS-C
  position reports read from the aircraft's ACARS text.
- **C channel** (a telephone call): 8400 bit/s (AMBE) and 21000 bit/s (the
  9.6 kbit/s multipulse codec of ICAO Annex 10) speech, played through the
  app's audio output; the call's progress in words. A 21000 bit/s fax or
  data call is named as such, not decoded.

The card is the DVB card's: the channel and bit rate, the link margin (the
Es/N0 over the point where the channel's rate ½ code gives up, marked on the
bar), then demodulator, frames, Viterbi, signal units or voice, and the log,
newest first. **Record** writes the log to a text file.

Make the VFO 4 kHz wide or more for the slow channels, about 12 kHz for
10500 and 8400 bit/s, 25 kHz for 21000 bit/s: a narrower one leaves too few
samples and the card says so. Voice assignments are logged, not followed:
the VFO for the call is yours to place.

```
decsat decode aero_625000Sps.wav --decoder aero --offset -75600 --bandwidth 4000 --fast
```
