<!-- made by CasualArclamp -->
# DecSAT manual: TDMA bursts and the Intelsat 19 DAMA network

This page describes the burst decoders. [Back to the README](../../README.md).

## TDMA bursts

**SCPC or TDMA.** Every VFO says which kind of signal it is on — *Signal:
SCPC* (a continuous carrier: Identify, the modem and DVB decoders, the
generic PSK; a burst landing on it is interference) or *TDMA* (terminals
taking turns: the burst analyser) — and its decoder list follows; *New VFOs
run* has the same switch. The waterfall finds the two kinds apart: green
brackets for continuous carriers, from the averaged spectrum; **violet
dashed brackets** for bands whose power comes and goes, from how much each
bin varies frame to frame (2 ms frames: a stationary bin — noise, a
carrier, its skirts — varies by a fixed share of its mean, an on-and-off
one far more), so a TDMA group a few dB over the noise is not missed or
merged into its neighbour. Click one for a TDMA VFO over it; Identify's
claim sends a bursty carrier there too. A grey dashed bracket is Bluetooth
leaking into the HackRF's IF (see *HackRF One*), not the satellite.
`decsat scan` lists the bands as well.

Drop a VFO over one TDMA carrier or a whole group (an MF-TDMA return) and
choose **TDMA bursts**. Each carrier in it is found — from a peak hold of
the spectrum, so a slow return that bursts once every few seconds is waited
for (up to 20 s) rather than missed, and looked for again every 5 s, so a
carrier that starts later joins, its earlier bursts decoded from what was
kept — and given its own down-converter and matched filter. Its centre and
symbol rate come from its bursts alone: the rate is the one its bursts
demodulate cleanly at, checked burst by burst, so a slow carrier is
usually read from its first burst or two; the work is spread over a few
blocks, so a whole group never holds the VFO up. Then, for each carrier:

- **bursts** are found by their envelope (against the noise's own spread,
  so silence between a slow carrier's bursts never passes for bursts) and
  demodulated one by one — timing, symbol clock and carrier phase from each
  burst alone, as no two come from the same terminal (once the bursts show
  the terminals keep the hub's clock, every burst is read at their median
  clock instead);
- once a hundred or so are in (forty of one length on a slow carrier:
  ~2 minutes at one burst every 2 s), the **layout** every burst shares is read
  blind: the preamble (and a varying field inside it), pilot blocks, fixed
  fields, the data symbols — and the share of bursts that are idle fill
  (terminals with nothing to send repeat one burst);
- the **slots** (length, guard time, how closely terminals are ranged), the
  **frame** they repeat in (from the silence that closes each), and the
  **terminals** — by the short field in the preamble when the bursts have
  one (live, one value a terminal), else by power, carrier offset and where
  in its slot each sends;
- the **header fields**: the short fields ahead of the payload that vary
  from burst to burst (inside the preamble, and short runs of data before
  the payload), each with its latest values, how many it has taken and how
  often it changes — a count, a time, a terminal's name — read as numbered
  constellation points, since what they mean is the system's own.

The **burst map** shows the last quarter second, a row a carrier (brighter
is stronger, idle fill dimmed). **● Record** writes each burst's data
symbols (the ones between the layout's fixed symbols) to
`decsat-<VFO>-<RF>MHz-tdma-<time>.bursts`, and a line per burst to a `.csv`
(with its header fields' values); the format is at the top of
`crates/decsat-engine/src/tdma/mod.rs`.

On Intelsat 19 at 12.49 GHz a group of three 1.42 MS/s QPSK inroutes reads
as 852- and 1135-symbol bursts with a 35-symbol preamble and two pilot
symbols every 30, slots of 0.6089–0.8102 ms, 125 ms frames each closed by a
2–3 ms silence, about half the bursts idle fill.

```bash
decsat decode capture_20Msps.cs8 --decoder tdma --offset -4.27e6 --bandwidth 5.3e6 --out dir
```

## Intelsat 19 DAMA outbound

Around 12 335 MHz on Intelsat 19 sits a DAMA telephony network: two
outbounds of 102.4 kBd, the slow 16-carrier TDMA return beside them, and,
above, the calls — pairs of 5.1 kBd channels that come up for a call and
go again. Drop a VFO on an outbound (about 125 kHz wide) and choose
**IS-19 DAMA outbound**:

- the carrier is centred and locked as QPSK at 102.4 kBd (a blind look
  reads it BPSK: its pilots, one every 16 symbols, put lines in its squared
  spectrum);
- **frames** (40 112 symbols, 0.39 s) are found by their 176-symbol sync
  word and read the right way up; once a clean frame has taught the
  pilots' signs, a 90° carrier slip part-way through a frame is put right;
- each frame's 52 × 960 information bits are **descrambled**
  (1 + x² + x⁸ + x¹³ + x¹⁶, restarted every frame) into one packet stream,
  idle fill between packets; the card shows the load (about three
  quarters), the 275-byte traffic packets and the rest by type;
- **call signalling**: a *request ack* ~0.37 s after a terminal's request
  burst on the call channels, naming the terminal; a *channel assignment*
  ~0.42 s before a call's carriers key up, one to each end — the
  **calling terminal** and the **called terminal**, each told the **call
  channel** to send on and the one to receive on, with the call's
  reference. The two go out on the outbounds that serve the two terminals,
  so a VFO on each outbound sees both ends. An ack from a terminal in a
  call says which end it is (a called terminal is acked as it answers).
  The **Calls** table puts each call's ends and channels together. Newest
  first, the packet's hex on hover; times are seconds since the decoder
  started.

**Terminals** are named by the 32-bit field the network uses (`92a80d69`),
the same in their acks and assignments. **Call channels** are numbered up
from the lowest of the call carriers, 6.16 kHz apart, a call taking an
even one (the calling end's) and the next: on the recording channel 0 was
at 12 335.436 GHz (LNB 10 599.955 MHz) — the pair the first calls took,
the next pair up channels 2 and 3, and so on.

**● Record** writes every packet, one a line (time, kind, type, length,
terminal, flags, call end, call reference, send and receive channel, hex),
to `decsat-<VFO>-<RF>MHz-is19-<time>.csv`. The rest of the packets is not
read (their fields look coded further), nor is the rate-⅔ code (the
information bits are clean enough as sent), nor the calls' voice, which is
encrypted. The decoder was worked out from one recording; how it was found
is in `docs/STATUS.md` (2026-10-15).

```bash
decsat decode capture_4Msps.cs8 --decoder is19 --offset -803.8e3 --bandwidth 125e3 --out dir
```
