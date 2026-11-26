<!-- made by CasualArclamp -->
# DecSAT manual: Radios

This page describes each radio that DecSAT can use. [Back to the README](../../README.md).

## HackRF One

**▶ Start** at the top left starts the HackRF with its settings.
**⏹ Stop** stops it. The button is green while the HackRF operates.
**File ▸ 📡 HackRF settings…** sets the frequency, the sample rate and the
gains. You can also click the source at the right of the menu bar. The
frequency and the gains apply live.

**LNB LO** only gives the labels of the axis (RF = tuned + LO):

- 10600 MHz is the default (the high band of a universal Ku LNB).
- Use 9750 MHz for the low band of the LNB or for QO-100.
- Use 0 if there is no LNB.

**Gain:** The LNA and the mixer of the HackRF compress long before its ADC
clips. The products fall on the carriers. The carriers then slip and lose
frames. At LNA 40 dB, a clean 14 dB carrier in a full Ku-band slice lost
most of its frames. At LNA 24 dB, it lost none.

The **input** level adjacent to the play button becomes orange above
−25 dBFS (the tick on its bar). When it is orange, decrease the LNA gain.
The input display also counts the USB buffers dropped since Start.

A carrier can continue to operate incorrectly at one tuning (slips, or a
raised noise floor adjacent to it). Such a carrier is usually clean a few
MHz to each side, because the synthesisers of the HackRF put noise at
particular offsets. Thus, retune by a small quantity.

**Bluetooth:** Below 2.17 GHz, the HackRF mixes up to an IF of 2650 − f/7
MHz (f is the tuned frequency). This IF is in the 2.4 GHz band, and
Bluetooth signals that are near leak in.

- When the tuning is 1732.9 MHz, the IF is on Bluetooth LE advertising
  channel 37 (2402 MHz).
- The result is ~0.4 ms bursts approximately 20 times a second, ~1 MHz
  wide, ≈ 248 − f/7 MHz from the centre.
- You cannot see the bursts on the averaged spectrum.
- But two 10 kBd carriers below the bursts slipped every few seconds. They
  read 3–6 dB worse than 2.6 MHz higher, where they were clean.

Keep carriers away from this offset. BLE advertising is also at 2426 and
2480 MHz, which agree with tunings near 1568 and 1190 MHz. The generic
demodulators coast through such bursts and tell you ("N interference bursts
coasted" after the Es/N0).

The driver is pure Rust over USB
([`seify-hackrfone`](https://crates.io/crates/seify-hackrfone) on `nusb`).
It uses no libhackrf and no libusb. You install nothing more than the
WinUSB driver that the HackRF already uses on Windows (Zadig, or the
official tools). DecSAT only **receives**.

**Bias tee.** The *Bias tee* box is adjacent to the frequency in the radio
window. It puts DC on the antenna port for an LNA or an active antenna, for
example an L-band patch for Inmarsat. The voltages are:

- HackRF: 3.3 V.
- Airspy: 4.5 V.
- SDRplay: 4.7 V (on an RSPduo, Tuner B only).
- RTL-SDR: 4.5 V on an RTL-SDR Blog V3/V4 with librtlsdr 0.6+. The RTL-SDR
  window has its own box.

These rules apply:

- The bias tee is off at each start of DecSAT. DecSAT never saves it.
- You can change it only while the radio is stopped.
- DecSAT sets it to off again when the radio stops.
- On the command line:
  `decsat decode --hackrf|--airspy|--sdrplay MHz --bias-tee`.
- None of these can supply power to an LNB (13/18 V). Supply the LNB from
  an external inserter.
- Never connect the bias tee to a DC short or to the RF port of an
  inserter.
- The Fobos and the Pluto have no bias tee. The transmitter never supplies
  power to its port.

**Decimation.** The window of each live receiver (HackRF, Fobos, Airspy,
SDRplay, RTL-SDR, UHD, Pluto and network IQ) has a *Decimation* selector.
The options are none, or the sample rate of the radio divided by 2, 4, 8,
16, 32 or 64. DecSAT divides the rate before all other steps.

- Use it for carriers much narrower than the lowest rate of the radio. For
  example, an Airspy R2 at 2.5 MS/s ÷ 16 is a 156 kS/s span.
- The span, the waterfall and the VFOs follow the new rate. The CPU load
  decreases with the rate.
- It is a cascade of half-band filters. The middle 80 % of the new span is
  flat and alias-free to better than 65 dB.
- One setting applies to all receivers. DecSAT keeps it. Change it while
  the radio is stopped.
- On the command line: `decsat decode --airspy MHz --decimate 8` (any live
  source).

**DC removal** (View menu, on by default) subtracts the IQ mean before the
waterfall and the VFOs. It tracks the mean sample by sample with a 0.2 s
time constant. This removes the spike that the HackRF leaves at the centre
frequency. The notch is less than one hertz wide. Thus a carrier a few tens
of hertz from the centre keeps its phase. (A VFO recording that you replay
is centred on its carrier.)

## Fobos SDR

The RigExpert Fobos SDR is the other live receiver. It has 50–6000 MHz on
its RF input and 8–50 MS/s of 14-bit IQ (58 and 64 on a rev. 4 board).
This is a full 40 MHz of the IF of a Ku LNB at one time, which the HackRF
cannot stream. Select it in **File ▸ 📡 Radio settings…** (or click the
source at the right of the menu bar). Then push ▶ Start.

All other functions operate as with the HackRF, on the same tuning and LNB
LO: the waterfall, the VFOs, the pan past the edge to retune, the beacon
list and Survey.

DecSAT uses the library of RigExpert, which it loads when you push Start.
Thus the build needs nothing more. Put the 64-bit library adjacent to
`decsat-gui.exe`:

- **`fobos_sdr.dll`** (libfobos-sdr-agile) for a board on the *agile*
  firmware.
- **`fobos.dll`** (libfobos) for the stock firmware.
- DecSAT uses the first library that finds a board.
- On Linux, put `libfobos_sdr.so` / `libfobos.so` on the library path.
- `DECSAT_FOBOS_LIB` names the file to use as an alternative.
- The board must have the USB driver that the RigExpert installer sets up.

Notes:

- **Gains:** the RF LNA has four steps (0–3; 2 ≈ +16 dB, 3 ≈ +33 dB). The
  IF VGA has 0–62 dB in 2 dB steps. The gains apply live. Behind an LNB,
  start with LNA 0–1. Then increase the VGA until the input meter is at
  approximately −30 to −20 dBFS.
- **Rate:** DecSAT uses the rate of the board that is nearest to the
  selected rate. The rate is fixed while the radio operates (DecSAT does
  not ask the firmware to change it mid-stream). Above ~20 MS/s the PC sets
  the limit, because the DDC of each VFO operates on the full rate.
- **Clock:** an external 10 MHz on CLK IN, if you tick the box.
- The library removes DC and balances I/Q. DecSAT scales its samples
  (±0.25 full scale) to ±1. Thus the input meter reads dBFS as for the
  HackRF.
- Receive only, RF input only (DecSAT does not use the HF direct-sampling
  inputs). The Fobos does not supply power to its antenna port. Supply an
  LNB from an inserter.
- DecSAT does not mark the artefacts of the HackRF (Bluetooth in its IF,
  its overload advice) while the Fobos operates. A transmission with a
  HackRF does not stop a Fobos receive session.

```
decsat-gui --fobos 1748 --rate 20 --lo 10600 --vga 30
decsat decode --fobos 1748 --rate 20e6 --fobos-lna 1 --fobos-vga 30 --decoder id --offset -915e3 --bandwidth 640e3
```

## Airspy Mini / R2

A third live receiver is an Airspy Mini (3 or 6 MS/s) or an Airspy R2 (2.5
or 10 MS/s), 12-bit, 24–1750 MHz. Behind a Ku LNB, it receives the IF up to
approximately 1.7 GHz, not the top of the 950–2150 MHz band. It is narrower
than the HackRF or the Fobos. Thus use it for a few SCPC carriers, beacons
or L-band, not for a transponder.

- As the HackRF, it needs no DLL.
  [`libairspy-rs`](https://crates.io/crates/libairspy-rs) connects to it
  over USB through `nusb`. This crate (BSD-3-Clause and MIT) is a port of
  the libairspy of Airspy. Its IQ converter is bit-exact against the C
  library.
- On Windows, the board must have the WinUSB driver that the Airspy
  installer or Zadig sets up.
- There is one gain step 0–21. It uses the *linearity* table of libairspy
  (for the output of an LNB) or its *sensitivity* table.
- The bias tee is off unless you tick *Bias tee* (see the HackRF section).
  It is off again on stop. 4.5 V cannot supply power to an LNB.

A **HydraSDR RFOne** (2.5, 5 or 10 MS/s, R828D tuner) operates the same
way, because it uses the USB protocol of the R2. Select it as **Airspy
Mini / R2 / HydraSDR**, or use `--airspy` (alias `--hydrasdr`) on the
command line. The radio panel names it from its firmware. It was tested
live on an RFOne at 5 and 10 MS/s.

```
decsat-gui --airspy 1450 --rate 6 --lo 10600 --vga 10    # --vga is the gain step here
decsat decode --airspy 1450 --airspy-gain 10 --decoder id --offset 200e3 --bandwidth 300e3
```

## ADALM-PLUTO, Pluto+ and LibreSDR

One more live receiver is the ADALM-PLUTO of ADI, the Pluto+ and clones
such as the LibreSDR (same firmware). The range is 70 MHz–6 GHz (an AD9363
Pluto that was not unlocked: 325–3800 MHz) and 0.26–61.44 MS/s.

As the HackRF and the Airspy, it needs no DLL. DecSAT uses the IIOD
protocol of the radio in pure Rust, over USB or the network. This is the
protocol that libiio 0.26 uses, and all Pluto firmware supplies it.

- **USB** (`usb`): on Windows, the IIO interface of the Pluto must have
  the Pluto USB driver of ADI (WinUSB; the PlutoSDR-M2k driver package, or
  Zadig).
- **Network** (`ip:ADDRESS`): the USB Ethernet of the Pluto at
  `ip:192.168.2.1`, or a **Pluto+ on Gigabit Ethernet** at its address.
  The network is the way to the high rates. The USB 2.0 of a plain Pluto
  has a limit near 6.8 MS/s of 16-bit IQ.
- `auto` (the default) uses USB when a Pluto is connected. If not, it uses
  192.168.2.1.

On the **Tezuka** firmware of F5OEO, the Pluto sends **8-bit IQ** on
request. This halves the load of the link (approximately 13 MS/s over the
USB of a plain Pluto). DecSAT uses 8-bit IQ when the firmware has it,
unless you untick the 8-bit box (`--pluto-16bit` on the command line).

The gain is manual (dB) or the slow-attack AGC of the AD9361.

**Receive only:** each open powers the transmit LO down, and DecSAT never
opens the transmit data path. The Pluto does not supply power to its
antenna port. Supply an LNB from an inserter, and enter the LO of the LNB
in the window (RF = tuner IF + LO).

Select **PlutoSDR / Pluto+ / LibreSDR** in the source menu. Its settings
window shows the link, the firmware, 8- or 16-bit IQ and dropped blocks.
It was tested against a simulated Pluto (a fake IIOD server, also over
TCP). It is not yet tried on hardware.

```
decsat decode --pluto 1450 --rate 4e6 --pluto-gain 40 --decoder id --offset 200e3 --bandwidth 300e3
decsat decode --plutoplus 1450 --pluto-uri ip:192.168.1.50 --rate 20e6 --pluto-gain agc --decoder ts --offset -1.2e6
```
