<!-- made by CasualArclamp -->
# DecSAT manual: Radios

This page describes each radio that DecSAT can use. [Back to the README](../../README.md).

## HackRF One

**▶ Start** at the top left starts the HackRF with its settings and
**⏹ Stop** stops it (green while it runs). **File ▸ 📡 HackRF settings…** (or
the source at the right of the menu bar) sets the frequency, sample rate and
gains; frequency and gains apply live. **LNB LO** only labels the axis
(RF = tuned + LO): 10600 MHz by default (a universal Ku LNB's high band),
9750 MHz for its low band or QO-100, 0 without one.

**Gain:** the HackRF's LNA and mixer compress long before its ADC clips,
and the products land on the carriers, which then slip and lose frames —
at LNA 40 dB a clean 14 dB carrier in a full Ku-band slice lost most of its
frames, at LNA 24 dB none. The **input** level beside the play button
turns orange above −25 dBFS (the tick on its bar): lower the LNA gain. It
also counts the USB buffers dropped since Start. A carrier that still misbehaves at one
tuning (slips, a raised noise floor beside it) is usually clean a few MHz
either side — the HackRF's synthesisers put noise at particular offsets —
so retune a little.

**Bluetooth:** below 2.17 GHz the HackRF mixes up to an IF of 2650 − f/7
MHz (f the tuned frequency), inside the 2.4 GHz band, and nearby Bluetooth
leaks in. Tuned to 1732.9 MHz its IF sits on Bluetooth LE advertising
channel 37 (2402 MHz): ~0.4 ms bursts about 20 times a second, ~1 MHz
wide, ≈ 248 − f/7 MHz from the centre — invisible on the averaged
spectrum, but two 10 kBd carriers under them slipped every few seconds and
read 3–6 dB worse than 2.6 MHz higher, where they were clean. Keep carriers
away from it (BLE advertising also sits at 2426 and 2480 MHz: tunings near
1568 and 1190 MHz). The generic demodulators coast through such bursts and
say so ("N interference bursts coasted" after the Es/N0).

The driver is pure Rust over USB ([`seify-hackrfone`](https://crates.io/crates/seify-hackrfone)
on `nusb`): no libhackrf, no libusb, nothing to install beyond the WinUSB driver
the HackRF already uses on Windows (Zadig, or the official tools). DecSAT only
ever **receives**.

**Bias tee.** The *Bias tee* box beside the frequency in the radio window (HackRF 3.3 V, Airspy
4.5 V, SDRplay 4.7 V — an RSPduo on Tuner B only; the RTL-SDR window has its
own, 4.5 V on an RTL-SDR Blog V3/V4 with librtlsdr 0.6+) puts DC on the
antenna port for an LNA or active antenna, e.g. an L-band patch for Inmarsat.
It is off at every start of DecSAT (never saved), can only be changed while
the radio is stopped, and is switched off again when the radio stops. On the
command line: `decsat decode --hackrf|--airspy|--sdrplay MHz --bias-tee`.
None of these can power an LNB (13/18 V): feed that from an external
inserter, and never put the bias tee into a DC short or an inserter's RF
port. The Fobos and Pluto have none; the transmitter never powers its port.

**Decimation.** Every live receiver's window (HackRF, Fobos, Airspy,
SDRplay, RTL-SDR, UHD, Pluto and network IQ) has a *Decimation* picker: none,
or the radio's sample rate divided by 2, 4, 8, 16, 32 or 64 before anything
else, for carriers much narrower than the radio's lowest rate (an Airspy R2 at
2.5 MS/s ÷ 16 is a 156 kS/s span). The span, the waterfall and the VFOs follow
the new rate, and the CPU load drops with it. It is a cascade of half-band
filters: the middle 80 % of the new span is flat and alias-free to better
than 65 dB. One setting for all receivers, remembered, changed while the
radio is stopped. On the command line: `decsat decode --airspy MHz --decimate 8`
(any live source).

**DC removal** (View menu, on by default) subtracts the IQ mean, tracked
sample by sample with a 0.2 s time constant, before the waterfall and the
VFOs, which removes the spike the HackRF leaves at the centre frequency.
Its notch is under a hertz wide, so a carrier a few tens of hertz off the
centre (a VFO recording replayed is centred on its carrier) keeps its
phase.

## Fobos SDR

The RigExpert Fobos SDR is the other live receiver: 50–6000 MHz on its RF
input, 8–50 MS/s of 14-bit IQ (58 and 64 on a rev. 4 board) — a whole
40 MHz of a Ku LNB's IF at once, which the HackRF cannot stream. Choose it
in **File ▸ 📡 Radio settings…** (or the source at the right of the menu
bar), then ▶ Start; everything else — waterfall, VFOs, panning past the
edge to retune, the beacon list, Survey — works as with the HackRF, on the
same tuning and LNB LO.

DecSAT talks to it through RigExpert's own library, loaded when Start is
pressed, so nothing extra is needed to build. Put the 64-bit library beside
`decsat-gui.exe`: **`fobos_sdr.dll`** (libfobos-sdr-agile) for a board on the
*agile* firmware, **`fobos.dll`** (libfobos) for the stock firmware — the
first one that finds a board is used; on Linux `libfobos_sdr.so` /
`libfobos.so` on the library path. `DECSAT_FOBOS_LIB` names the file to use
instead. The board needs the USB driver RigExpert's installer sets up.

- **Gains:** the RF LNA in four steps (0–3; 2 ≈ +16 dB, 3 ≈ +33 dB) and the
  IF VGA, 0–62 dB in 2 dB steps, applied live. Behind an LNB, start with LNA
  0–1 and raise the VGA until the input meter sits around −30 to −20 dBFS.
- **Rate:** the board's nearest rate to the one chosen; fixed while running
  (the firmware is not asked to change it mid-stream). Above ~20 MS/s the
  PC sets the limit: every VFO's DDC runs on the full rate.
- **Clock:** an external 10 MHz on CLK IN, if ticked.
- The library removes DC and balances I/Q itself; its samples (±0.25 full
  scale) are scaled to ±1, so the input meter reads dBFS as for the HackRF.
- Receive only, RF input only (the HF direct-sampling inputs are not used).
  The Fobos does not power its antenna port: feed an LNB from an inserter.
- The HackRF's own artefacts (Bluetooth in its IF, its overload advice) are
  not marked while the Fobos runs, and transmitting with a HackRF no longer
  stops a Fobos receive session.

```
decsat-gui --fobos 1748 --rate 20 --lo 10600 --vga 30
decsat decode --fobos 1748 --rate 20e6 --fobos-lna 1 --fobos-vga 30 --decoder id --offset -915e3 --bandwidth 640e3
```

## Airspy Mini / R2

A third live receiver: an Airspy Mini (3 or 6 MS/s) or R2 (2.5 or 10 MS/s),
12-bit, 24–1750 MHz — behind a Ku LNB the IF up to about 1.7 GHz, not the
top of the 950–2150 MHz band. Narrower than the HackRF or the Fobos, so for
a few SCPC carriers, beacons or L-band rather than a transponder. Like the
HackRF it needs no DLL: [`libairspy-rs`](https://crates.io/crates/libairspy-rs)
(BSD-3-Clause and MIT, a port of Airspy's libairspy whose IQ converter is
bit-exact against the C library) speaks to it over USB through `nusb`; on
Windows the board needs the WinUSB driver Airspy's installer or Zadig sets
up. One gain step 0–21 on libairspy's *linearity* table (for an LNB's
output) or its *sensitivity* table. The bias tee is off unless *Bias tee* is
ticked (see the HackRF section), and off again on stop: 4.5 V cannot power an LNB.

A **HydraSDR RFOne** (2.5, 5 or 10 MS/s, R828D tuner) works the same way:
it speaks the R2's USB protocol and is picked as **Airspy Mini / R2 /
HydraSDR**, or `--airspy` (alias `--hydrasdr`) on the command line; the
radio panel names it from its firmware. Tested live on an RFOne at 5 and
10 MS/s.

```
decsat-gui --airspy 1450 --rate 6 --lo 10600 --vga 10    # --vga is the gain step here
decsat decode --airspy 1450 --airspy-gain 10 --decoder id --offset 200e3 --bandwidth 300e3
```

## ADALM-PLUTO, Pluto+ and LibreSDR

Another live receiver: ADI's ADALM-PLUTO, the Pluto+ and clones such as
the LibreSDR (same firmware), 70 MHz–6 GHz (an AD9363 Pluto that was not
unlocked: 325–3800 MHz), 0.26–61.44 MS/s. Like the HackRF and the Airspy
it needs no DLL: DecSAT speaks the radio's own IIOD protocol (the one
libiio 0.26 uses, served by every Pluto firmware) in pure Rust, over USB or
the network:

- **USB** (`usb`): on Windows the Pluto's IIO interface needs ADI's Pluto
  USB driver (WinUSB; the PlutoSDR-M2k driver package, or Zadig).
- **Network** (`ip:ADDRESS`): the Pluto's own USB Ethernet at
  `ip:192.168.2.1`, or a **Pluto+ on Gigabit Ethernet** at its address —
  the way to the high rates, where a plain Pluto's USB 2.0 tops out near
  6.8 MS/s of 16-bit IQ.
- `auto` (the default) takes USB when a Pluto is plugged in, else
  192.168.2.1.

On F5OEO's **Tezuka** firmware the Pluto sends **8-bit IQ** when asked,
halving the link's load (about 13 MS/s over a plain Pluto's USB); DecSAT
uses it whenever the firmware offers it, unless the 8-bit box is unticked
(`--pluto-16bit` on the command line). Gain is manual (dB) or the AD9361's
slow-attack AGC. **Receive only:** every open powers the transmit LO down,
and the transmit data path is never opened. The Pluto does not power its
antenna port: feed an LNB from an inserter, and enter its LO in the window
(RF = tuner IF + LO).

Pick **PlutoSDR / Pluto+ / LibreSDR** in the source menu; its settings
window shows the link, the firmware, 8- or 16-bit IQ and dropped blocks.
Tested against a simulated Pluto (a fake IIOD server, over TCP too); not
yet tried on hardware.

```
decsat decode --pluto 1450 --rate 4e6 --pluto-gain 40 --decoder id --offset 200e3 --bandwidth 300e3
decsat decode --plutoplus 1450 --pluto-uri ip:192.168.1.50 --rate 20e6 --pluto-gain agc --decoder ts --offset -1.2e6
```
