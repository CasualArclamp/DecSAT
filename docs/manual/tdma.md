<!-- made by CasualArclamp -->
# DecSAT manual: TDMA bursts and the Intelsat 19 DAMA network

This page describes the burst decoders. [Back to the README](../../README.md).

## TDMA bursts

**SCPC or TDMA.** Each VFO shows the type of signal that it is on:

- *Signal: SCPC* is a continuous carrier. It uses Identify, the modem
  decoders, the DVB decoders and the generic PSK decoder. A burst on such
  a carrier is interference.
- *Signal: TDMA* is a carrier on which terminals transmit in turns. It
  uses the burst analyser.

The decoder list of the VFO agrees with this setting. *New VFOs run* has
the same switch.

The waterfall finds the two types separately:

- Green brackets show continuous carriers. DecSAT finds them from the
  averaged spectrum.
- **Violet dashed brackets** show bands in which the power comes and goes.
  DecSAT finds them from the variation of each bin from frame to frame.
- A grey dashed bracket shows Bluetooth that leaks into the IF of the
  HackRF (see *HackRF One*). It is not from the satellite.

The frames are 2 ms long. A stationary bin (noise, a carrier, or the
skirts of a carrier) changes by a fixed part of its mean. A bin that goes
on and off changes much more. Thus DecSAT does not miss a TDMA group that
is some dB above the noise. It also does not merge the group into the
adjacent carrier.

Click a violet dashed bracket to put a TDMA VFO on the band. When you
claim a carrier with Identify, a carrier with bursts also gets a TDMA VFO.
`decsat scan` also lists the bands.

**To analyse the bursts**

1. Put a VFO on one TDMA carrier or on a full group (an MF-TDMA return).
2. Select **TDMA bursts**.

The decoder finds each carrier in the VFO as follows:

- It finds the carriers from a peak hold of the spectrum. Thus it waits
  (20 s maximum) for a slow return that sends one burst each few seconds.
- It looks again each 5 s. Thus a carrier that starts later is added. The
  decoder decodes the earlier bursts of that carrier from the samples that
  it kept.
- Each carrier gets its own down-converter and matched filter.
- The centre and the symbol rate of a carrier come only from its bursts.
  The rate is the rate at which the bursts demodulate cleanly. The decoder
  examines it burst by burst. Thus the decoder usually reads a slow
  carrier from its first one or two bursts.
- The decoder divides the work between some blocks. Thus a full group
  does not stop the VFO.

Then, for each carrier, the decoder does these steps:

- **Bursts.** The decoder finds the bursts by their envelope. It compares
  the envelope with the variation of the noise. Thus the silence between
  the bursts of a slow carrier does not look like bursts.
- The decoder demodulates the bursts one at a time. It gets the timing,
  the symbol clock and the carrier phase from each burst alone, because
  each burst comes from a different terminal.
- When the bursts show that the terminals use the clock of the hub, the
  decoder reads each burst at their median clock.
- **Layout.** The decoder reads the layout that all the bursts have,
  without help. It does this after approximately one hundred bursts. On a
  slow carrier, forty bursts of one length are sufficient. This is
  approximately 2 minutes at one burst each 2 s.
- The layout has the preamble (and a field in it that changes), the pilot
  blocks, the fixed fields and the data symbols.
- The layout also gives the part of the bursts that are idle fill. A
  terminal that has no data sends the same burst again and again.
- **Slots.** The decoder gives the length, the guard time and the accuracy
  of the range adjustment of the terminals.
- **Frame.** The decoder finds the frame in which the slots repeat. It
  finds the frame from the silence at the end of each frame.
- **Terminals.** If the bursts have a short field in the preamble, the
  decoder identifies the terminals by this field. On a live signal, each
  terminal had one value. If not, the decoder uses the power, the carrier
  offset and the position of each burst in its slot.
- **Header fields.** These are the short fields before the payload that
  change from burst to burst. They are in the preamble, or they are short
  runs of data before the payload.
- For each header field, the decoder shows the latest values, the number
  of different values and the rate of change. A field can be a count, a
  time or the name of a terminal.
- The decoder shows the fields as numbered constellation points, because
  only the system knows their function.

The **burst map** shows the last quarter second. Each row is a carrier. A
brighter burst is stronger. Idle fill is dim.

**● Record** writes two files:

- `decsat-<VFO>-<RF>MHz-tdma-<time>.bursts` contains the data symbols of
  each burst. These are the symbols between the fixed symbols of the
  layout.
- A `.csv` file contains one line for each burst, with the values of its
  header fields.

The description of the format is at the top of
`crates/decsat-engine/src/tdma/mod.rs`.

Results on Intelsat 19 at 12.49 GHz, for a group of three 1.42 MS/s QPSK
inroutes:

- Bursts of 852 and 1135 symbols.
- A 35-symbol preamble and two pilot symbols each 30 symbols.
- Slots of 0.6089–0.8102 ms.
- 125 ms frames. A silence of 2–3 ms is at the end of each frame.
- Approximately half of the bursts are idle fill.

```bash
decsat decode capture_20Msps.cs8 --decoder tdma --offset -4.27e6 --bandwidth 5.3e6 --out dir
```

## Intelsat 19 DAMA outbound

A DAMA telephony network is near 12 335 MHz on Intelsat 19. It has these
carriers:

- Two outbounds of 102.4 kBd.
- The slow 16-carrier TDMA return, adjacent to the outbounds.
- The calls, above them. The calls are pairs of 5.1 kBd channels. A pair
  starts for a call and stops at the end of the call.

To decode an outbound:

1. Put a VFO on an outbound. Make the VFO approximately 125 kHz wide.
2. Select **IS-19 DAMA outbound**.

The decoder does these steps:

- **Carrier.** The decoder centres the carrier and locks it as QPSK at
  102.4 kBd. A blind look reads the carrier as BPSK. The cause is the
  pilots, one each 16 symbols, which put lines in the squared spectrum.
- **Frames.** A frame has 40 112 symbols (0.39 s). The decoder finds the
  frames by their 176-symbol sync word and reads them in the correct
  orientation.
- A clean frame gives the signs of the pilots. After that, the decoder
  corrects a 90° carrier slip that occurs in a frame.
- **Descrambling.** The decoder descrambles the 52 × 960 information bits
  of each frame into one packet stream. The polynomial is
  1 + x² + x⁸ + x¹³ + x¹⁶, and it starts again at each frame. Idle fill is
  between the packets.
- The card shows the load (approximately three quarters), the 275-byte
  traffic packets and the other packets by type.

**Call signalling.** The decoder finds two types of packet:

- A *request ack* comes ~0.37 s after the request burst of a terminal on
  the call channels. It gives the name of the terminal.
- A *channel assignment* comes ~0.42 s before the carriers of a call
  start. One assignment goes to each end: the **calling terminal** and the
  **called terminal**.
- An assignment tells the terminal the **call channel** on which to send
  and the call channel on which to receive. It also gives the reference of
  the call.

The two assignments go out on the outbounds that serve the two terminals.
Thus a VFO on each outbound sees the two ends. An ack from a terminal in a
call tells you which end the terminal is. The network acks a called
terminal when it answers.

The **Calls** table shows the ends and the channels of each call together.
The newest call is first. Hold the pointer on a line to see the hex of the
packet. The times are in seconds from the start of the decoder.

**Terminals.** The decoder identifies each terminal by the 32-bit field
that the network uses (`92a80d69`). The field is the same in the acks and
the assignments of the terminal.

**Call channels.** The decoder gives numbers to the call channels. The
numbers start at the lowest call carrier, and the channels are 6.16 kHz
apart. A call uses an even channel (for the calling end) and the next
channel. On the recording, channel 0 was at 12 335.436 GHz (LNB
10 599.955 MHz). The first calls used this pair. The next pair up was
channels 2 and 3, and so on.

**● Record** writes every packet to
`decsat-<VFO>-<RF>MHz-is19-<time>.csv`, one packet on each line. A line
has the time, kind, type, length, terminal, flags, call end, call
reference, send channel, receive channel and hex.

The decoder does not read these items:

- The other content of the packets. Their fields look as if they have more
  coding.
- The rate-⅔ code. The information bits are sufficiently clean as sent.
- The voice of the calls. It is encrypted.

The decoder was worked out from one recording. `docs/STATUS.md`
(2026-10-15) tells you how it was found.

```bash
decsat decode capture_4Msps.cs8 --decoder is19 --offset -803.8e3 --bandwidth 125e3 --out dir
```
